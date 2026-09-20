//! Configuration struct and config.toml parsing.
//!
//! Mounted at `/etc/sigproc/config.toml` via ConfigMap in k8s.

use serde::Deserialize;
use std::path::Path;

/// Top-level configuration read from config.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub device: DeviceConfig,
    pub rf: RfConfig,
    pub forwarding: ForwardingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    /// Optional UHD device args string (e.g. "type=b200,serial=ABC123").
    /// Leave empty for default device discovery.
    #[serde(default)]
    pub device_args: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RfConfig {
    /// Center frequency in Hz.
    pub center_freq: f64,
    /// Sample rate in Hz (samples per second).
    pub sample_rate: f64,
    /// RX gain in dB.
    pub rx_gain: f64,
    /// Antenna port (e.g. "RX2" on B210, "TX/RX" on B200).
    #[serde(default = "default_antenna")]
    pub antenna: String,
    /// Bandwidth in Hz.
    pub bandwidth: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForwardingConfig {
    /// Destination host for VITA49 UDP packets.
    pub vita49_dest_host: String,
    /// Destination port for VITA49 UDP packets.
    pub vita49_dest_port: u16,
}

fn default_antenna() -> String {
    "RX2".to_string()
}

impl Config {
    /// Load and parse config from a TOML file path.
    pub fn from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate configuration values.
    fn validate(&self) -> anyhow::Result<()> {
        if self.rf.center_freq <= 0.0 {
            anyhow::bail!(
                "center_freq must be positive (Hz), got {}",
                self.rf.center_freq
            );
        }
        if self.rf.sample_rate <= 0.0 {
            anyhow::bail!(
                "sample_rate must be positive (Hz), got {}",
                self.rf.sample_rate
            );
        }
        if self.rf.bandwidth <= 0.0 {
            anyhow::bail!("bandwidth must be positive (Hz), got {}", self.rf.bandwidth);
        }
        if self.forwarding.vita49_dest_port == 0 {
            anyhow::bail!("vita49_dest_port must be non-zero");
        }
        Ok(())
    }
}
