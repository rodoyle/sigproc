//! Optional LAN copy; the existing unicast consumer remains unchanged.
use serde::Deserialize;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, Deserialize)]
pub struct MulticastConfig {
    pub group: Ipv4Addr,
    pub port: u16,
    /// Local IPv4 address on the chosen LAN interface, never a device name.
    pub interface: Ipv4Addr,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
}
fn default_ttl() -> u32 { 1 }
impl MulticastConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.group.is_multicast(), "forwarding.multicast.group must be multicast");
        anyhow::ensure!(self.port != 0, "forwarding.multicast.port must be nonzero");
        anyhow::ensure!(!self.interface.is_unspecified() && !self.interface.is_multicast(), "forwarding.multicast.interface must be a local unicast IPv4 address");
        anyhow::ensure!((1..=255).contains(&self.ttl), "forwarding.multicast.ttl must be 1..255");
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validate_group_interface_port_ttl() {
        let mut cfg = MulticastConfig { group: "239.255.49.2".parse().unwrap(), port: 4820, interface: "192.0.2.10".parse().unwrap(), ttl: 1 };
        cfg.validate().unwrap();
        cfg.group = Ipv4Addr::LOCALHOST;
        assert!(cfg.validate().is_err());
        cfg.group = "239.255.49.2".parse().unwrap();
        cfg.ttl = 256;
        assert!(cfg.validate().is_err());
        cfg.ttl = 1;
        cfg.interface = Ipv4Addr::UNSPECIFIED;
        assert!(cfg.validate().is_err());
    }
}
