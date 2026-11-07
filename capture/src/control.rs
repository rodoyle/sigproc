//! Bounded UDP polling; hardware dispatch stays on the capture owner thread.
use sigproc_common::{
    config::ControlConfig,
    vita49::{ControlBackend, Controllee, ParseError},
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

    /// Backend failures are fatal to the owner loop: no applied ACK is emitted
    /// and capture must not continue after a possibly partial hardware change.
    pub fn dispatch<B: ControlBackend>(
        &self,
        datagram: &[u8],
        controllee: &mut Controllee,
        backend: &mut B,
    ) -> anyhow::Result<()> {
        match controllee.handle(datagram, backend) {
            Ok(ack) => {
                // A lost ACK must not roll back successfully applied hardware.
                if let Err(e) = self.socket.send_to(&ack, self.peer) {
                    log::warn!("control acknowledgement send failed: {e}");
                }
                Ok(())
            }
            Err(e) if e.downcast_ref::<ParseError>().is_some() => {
                log::warn!("malformed control datagram dropped: {e}");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
