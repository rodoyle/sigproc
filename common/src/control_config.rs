//! Capture control endpoint configuration; disabled unless explicitly enabled.
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ControlConfig {
    pub enabled: bool,
    pub ctrl_addr: IpAddr,
    pub ctrl_port: u16,
    /// Exact UDP source address and port. This is an allowlist, not authentication.
    pub authorized_controller: Option<SocketAddr>,
    /// Stable across retunes and restarts with the same configuration.
    pub stream_id: u32,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ctrl_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            ctrl_port: 4991,
            authorized_controller: None,
            stream_id: 1,
        }
    }
}

impl ControlConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        anyhow::ensure!(self.ctrl_port != 0, "[control] ctrl_port must be nonzero");
        anyhow::ensure!(
            !self.ctrl_addr.is_multicast(),
            "[control] ctrl_addr must be unicast"
        );
        let peer = self
            .authorized_controller
            .ok_or_else(|| anyhow::anyhow!("[control] authorized_controller is required"))?;
        anyhow::ensure!(
            peer.port() != 0 && !peer.ip().is_unspecified() && !peer.ip().is_multicast(),
            "[control] authorized_controller must name one unicast peer and nonzero port"
        );
        anyhow::ensure!(
            peer.is_ipv4() == self.ctrl_addr.is_ipv4(),
            "[control] bind and controller address families must match"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_loopback_default_and_enabled_peer_requirement() {
        let mut cfg = ControlConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.ctrl_addr.is_loopback());
        cfg.validate().unwrap();
        cfg.enabled = true;
        assert!(cfg.validate().is_err());
        cfg.authorized_controller = Some("127.0.0.1:50000".parse().unwrap());
        cfg.validate().unwrap();
        cfg.ctrl_port = 0;
        assert!(cfg.validate().is_err());
    }
}
