//! Bounded UDP polling; hardware dispatch stays on the capture owner thread.
use sigproc_common::{
    config::ControlConfig,
    vita49::{ControlBackend, Controllee, ParseError, PendingExecute, PreparedControl},
};
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};

pub struct ControlSocket {
    socket: UdpSocket,
    peer: SocketAddr,
}

impl ControlSocket {
    pub fn bind(cfg: &ControlConfig) -> anyhow::Result<Option<Self>> {
        cfg.validate()?;
        if !cfg.enabled {
            return Ok(None);
        }
        let peer = cfg
            .authorized_controller
            .ok_or_else(|| anyhow::anyhow!("missing authorized controller"))?;
        let socket = UdpSocket::bind(SocketAddr::new(cfg.ctrl_addr, cfg.ctrl_port))?;
        socket.set_nonblocking(true)?;
        log::info!(
            "control listening on {}, authorized peer {}",
            socket.local_addr()?,
            peer
        );
        Ok(Some(Self { socket, peer }))
    }

    /// At most eight datagrams per capture batch, so observer traffic cannot
    /// monopolize the owner loop. Unauthorized peers receive no response.
    pub fn poll(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let mut buf = [0u8; 65536];
        for _ in 0..8 {
            match self.socket.recv_from(&mut buf) {
                Ok((len, peer)) if peer == self.peer => return Ok(Some(buf[..len].to_vec())),
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    /// Answer queries/rejections while RX is alive. A pending execute is the
    /// only reason for the owner loop to stop/drop the streamer.
    pub fn prepare(
        &self,
        datagram: &[u8],
        controllee: &mut Controllee,
    ) -> anyhow::Result<Option<PendingExecute>> {
        match controllee.prepare(datagram) {
            Ok(PreparedControl::Reply(ack)) => {
                self.send_ack(&ack);
                Ok(None)
            }
            Ok(PreparedControl::Execute(pending)) => Ok(Some(pending)),
            Err(e) if e.downcast_ref::<ParseError>().is_some() => {
                log::warn!("malformed control datagram dropped: {e}");
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// The streaming-phase transition, shared with hardware-free lifecycle
    /// tests. No query/rejection may call `stop`; stop failure cannot produce
    /// a pending execute or an applied acknowledgement.
    pub fn prepare_streaming(
        &self,
        datagram: &[u8],
        controllee: &mut Controllee,
        stop: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<Option<PendingExecute>> {
        let pending = self.prepare(datagram, controllee)?;
        if pending.is_some() {
            stop()?;
        }
        Ok(pending)
    }

    /// Hardware failures are fatal; never send an applied ACK for a failed
    /// or partially applied update. Call only after releasing the streamer.
    pub fn execute<B: ControlBackend>(
        &self,
        pending: PendingExecute,
        controllee: &mut Controllee,
        backend: &mut B,
    ) -> anyhow::Result<()> {
        let ack = controllee.execute(pending, backend)?;
        self.send_ack(&ack);
        Ok(())
    }

    #[cfg(test)]
    fn dispatch<B: ControlBackend>(
        &self,
        datagram: &[u8],
        controllee: &mut Controllee,
        backend: &mut B,
    ) -> anyhow::Result<()> {
        if let Some(pending) = self.prepare(datagram, controllee)? {
            self.execute(pending, controllee, backend)?;
        }
        Ok(())
    }

    fn send_ack(&self, ack: &[u8]) {
        // A lost ACK must not roll back successfully applied hardware.
        if let Err(e) = self.socket.send_to(ack, self.peer) {
            log::warn!("control acknowledgement send failed: {e}");
        }
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
