//! Configuration struct and config.toml parsing.
//!
//! Mounted at `/etc/sigproc/config.toml` via ConfigMap in k8s.
//!
//! One parser serves every service in the chain. The capture service uses
//! `device`, `rf` and `forwarding`; the channelizer reads `rf` as a description
//! of its wideband *input* stream, `forwarding` as its narrowband output
//! destination, and the `[channel]` section. A misconfiguration is therefore
//! reported identically by every service, and the live capture ConfigMap — which
//! has no `[channel]` section — keeps parsing unchanged.

use serde::Deserialize;
use std::path::Path;

/// Top-level configuration read from config.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// UHD device selection. Optional because the services downstream of capture
    /// (channelizer, sink) have no UHD involvement at all and should not be
    /// forced to carry an irrelevant section; absent means default discovery,
    /// exactly as an empty `device_args` did.
    #[serde(default)]
    pub device: DeviceConfig,
    pub rf: RfConfig,
    pub forwarding: ForwardingConfig,
    /// Where this service's input comes from. Optional: capture reads a USRP, not
    /// a UDP stream, so it has no use for the section.
    #[serde(default)]
    pub input: InputConfig,
    /// Channel plan — required by the channelizer, absent for capture.
    #[serde(default)]
    pub channel: Option<ChannelConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
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

fn default_stopband_db() -> f64 {
    80.0
}

fn default_listen() -> String {
    "0.0.0.0:4810".to_string()
}

/// Where a service's samples come from (`[input]` in config.toml).
///
/// The same two shapes serve production and verification: `udp` for the real
/// stream, `replay` for a fixture file. The code path after the VITA49 receive
/// boundary is identical, so the DSP under test is never mocked — only the packet
/// source changes.
#[derive(Debug, Clone, Deserialize)]
pub struct InputConfig {
    /// UDP address to bind for incoming VITA49 packets (`udp` mode).
    #[serde(default = "default_listen")]
    pub listen: String,
    /// `udp` or `replay`.
    #[serde(default = "default_input_mode")]
    pub mode: String,
    /// Fixture file to replay (`replay` mode).
    #[serde(default)]
    pub file: Option<String>,
    /// In `replay` mode, pace packets at the nominal sample rate instead of
    /// draining the file as fast as possible.
    #[serde(default)]
    pub realtime: bool,
    /// HTTP address for `/metrics` and `/healthz`.
    #[serde(default = "default_metrics_listen")]
    pub metrics_listen: String,
}

impl Default for InputConfig {
    fn default() -> Self {
        InputConfig {
            listen: default_listen(),
            mode: default_input_mode(),
            file: None,
            realtime: false,
            metrics_listen: default_metrics_listen(),
        }
    }
}

fn default_input_mode() -> String {
    "udp".to_string()
}

fn default_metrics_listen() -> String {
    "0.0.0.0:8080".to_string()
}

impl InputConfig {
    /// Validate the section, without pulling in the CLI's enum type.
    pub fn validate(&self) -> anyhow::Result<()> {
        match self.mode.as_str() {
            "udp" => {}
            "replay" => {
                if self.file.as_deref().unwrap_or("").is_empty() {
                    anyhow::bail!("[input] mode = \"replay\" needs a file = \"...\"");
                }
            }
            other => anyhow::bail!("[input] mode must be \"udp\" or \"replay\", got {other:?}"),
        }
        self.listen.parse::<std::net::SocketAddr>().map_err(|e| {
            anyhow::anyhow!("[input] listen {:?} is not host:port: {e}", self.listen)
        })?;
        self.metrics_listen
            .parse::<std::net::SocketAddr>()
            .map_err(|e| {
                anyhow::anyhow!(
                    "[input] metrics_listen {:?} is not host:port: {e}",
                    self.metrics_listen
                )
            })?;
        Ok(())
    }
}

fn default_max_taps() -> usize {
    512
}

/// Channel plan for the channelizer (`[channel]` in config.toml).
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    /// Channel centre offset from the wideband centre, in Hz. May be negative
    /// for a channel below the centre frequency.
    pub offset_hz: f64,
    /// Narrowband output sample rate in Hz. `rf.sample_rate / output_rate_hz`
    /// must be a whole number, because this channelizer decimates by integers.
    pub output_rate_hz: f64,
    /// Channel passband half-width in Hz. Defaults to 40% of `output_rate_hz`,
    /// which leaves a guard band below the output Nyquist.
    #[serde(default)]
    pub passband_hz: Option<f64>,
    /// FIR stopband attenuation target in dB.
    #[serde(default = "default_stopband_db")]
    pub stopband_db: f64,
    /// Largest FIR tap count any single decimation stage may use. A plan that
    /// exceeds it is rejected at startup rather than silently burning a core
    /// this service does not have.
    #[serde(default = "default_max_taps")]
    pub max_taps: usize,
}

impl ChannelConfig {
    /// Channel passband half-width in Hz.
    pub fn passband(&self) -> f64 {
        self.passband_hz.unwrap_or(0.4 * self.output_rate_hz)
    }

    /// Frequency above which the last stage must reject: the output Nyquist.
    pub fn stopband(&self) -> f64 {
        0.5 * self.output_rate_hz
    }

    /// Whole-number decimation factor, or a diagnostic saying what to change.
    pub fn decimation(&self, input_rate: f64) -> anyhow::Result<usize> {
        if !(self.output_rate_hz > 0.0) {
            anyhow::bail!(
                "[channel] output_rate_hz must be positive, got {}",
                self.output_rate_hz
            );
        }
        let ratio = input_rate / self.output_rate_hz;
        let rounded = ratio.round();
        if rounded < 1.0 || (ratio - rounded).abs() > 1e-9 {
            anyhow::bail!(
                "[channel] rf.sample_rate / output_rate_hz must be a whole number, got \
                 {ratio} ({input_rate} / {}). A fractional ratio needs a rational \
                 resampler, which this channelizer deliberately does not have: choose an \
                 output rate that divides the input rate (50 kS/s divides 2 MS/s exactly; \
                 48 kS/s does not).",
                self.output_rate_hz
            );
        }
        Ok(rounded as usize)
    }

    /// Apply every channel-plan check against the wideband input rate.
    ///
    /// Builds the stage plan as part of validation, so an unaffordable filter is
    /// rejected when the config is loaded rather than when the first packet
    /// arrives — the difference between a CrashLoopBackOff that names the problem
    /// and a pod that starts, falls behind, and drops samples.
    pub fn validate(&self, input_rate: f64) -> anyhow::Result<()> {
        self.decimation(input_rate)?;
        // A channel lying outside the captured band cannot be isolated; failing
        // here beats emitting a channel of pure noise.
        let span = self.offset_hz.abs() + self.passband();
        if span > input_rate / 2.0 {
            anyhow::bail!(
                "[channel] offset_hz {} with passband {} spans {span} Hz, past the \
                 captured band edge ({} Hz) of a {input_rate} Hz input",
                self.offset_hz,
                self.passband(),
                input_rate / 2.0
            );
        }
        self.plan(input_rate)?;
        Ok(())
    }

    /// Build the decimation stage plan for this channel.
    pub fn plan(&self, input_rate: f64) -> anyhow::Result<Vec<crate::dsp::StagePlan>> {
        crate::dsp::plan_stages(
            input_rate,
            self.output_rate_hz,
            self.passband(),
            self.stopband(),
            self.stopband_db,
            self.max_taps,
        )
    }
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
        self.input.validate()?;
        if let Some(channel) = &self.channel {
            channel.validate(self.rf.sample_rate)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURE_TOML: &str = r#"
        [device]
        device_args = ""
        [rf]
        center_freq = 915_000_000.0
        sample_rate = 2_000_000.0
        rx_gain = 30.0
        antenna = "RX2"
        bandwidth = 2_000_000.0
        [forwarding]
        vita49_dest_host = "waterfall-service.default.svc.cluster.local"
        vita49_dest_port = 4820
    "#;

    const CHANNELIZER_TOML: &str = r#"
        [rf]
        center_freq = 915_000_000.0
        sample_rate = 2_000_000.0
        rx_gain = 30.0
        bandwidth = 2_000_000.0
        [forwarding]
        vita49_dest_host = "vita49-sink.default.svc.cluster.local"
        vita49_dest_port = 4920
        [channel]
        offset_hz = 200_000.0
        output_rate_hz = 50_000.0
    "#;

    /// The live capture ConfigMap has no `[channel]` section and must keep parsing.
    #[test]
    fn capture_config_without_a_channel_section_still_parses() {
        let cfg: Config = toml::from_str(CAPTURE_TOML).expect("parses");
        assert!(cfg.channel.is_none());
        cfg.validate().expect("validates");
        assert_eq!(cfg.rf.sample_rate, 2_000_000.0);
        assert_eq!(cfg.rf.antenna, "RX2");
    }

    /// The channelizer has no UHD involvement, so its config must not have to
    /// carry a `[device]` section — and an absent one must mean default
    /// discovery, not a parse failure.
    #[test]
    fn a_config_without_a_device_section_parses() {
        let cfg: Config = toml::from_str(CHANNELIZER_TOML).expect("parses");
        assert_eq!(cfg.device.device_args, "");
        cfg.validate().expect("validates");
    }

    #[test]
    fn channel_plan_defaults_are_derived_from_the_output_rate() {
        let cfg: Config = toml::from_str(CHANNELIZER_TOML).expect("parses");
        let ch = cfg.channel.as_ref().expect("channel section");
        assert_eq!(ch.passband(), 20_000.0, "40% of a 50 kS/s output rate");
        assert_eq!(ch.stopband(), 25_000.0, "the output Nyquist");
        assert_eq!(ch.stopband_db, 80.0);
        assert_eq!(ch.max_taps, 512);
        assert_eq!(ch.decimation(2_000_000.0).expect("integer ratio"), 40);
        cfg.validate().expect("validates");
    }

    /// The failure mode this prevents: 48 kS/s is the rate someone reaches for
    /// from audio habits, and 2 MS/s / 48 kS/s is not a whole number.
    #[test]
    fn config_non_integer_decimation_rejected() {
        let toml_text = CHANNELIZER_TOML.replace("50_000.0", "48_000.0");
        let cfg: Config = toml::from_str(&toml_text).expect("TOML itself parses");
        let err = cfg
            .validate()
            .expect_err("48 kS/s must be rejected, not silently rounded")
            .to_string();
        assert!(err.contains("whole number"), "{err}");
        assert!(
            err.contains("resampler"),
            "must name the missing capability: {err}"
        );
        assert!(
            err.contains("50 kS/s"),
            "must offer a rate that works: {err}"
        );
    }

    /// A narrow passband alone is cheap (the intermediate stopbands sit far away),
    /// so the real budget failure is a narrow **transition** — a channel that
    /// nearly fills its output Nyquist. That needs a very long filter at a rate
    /// the node cannot afford, and it must be refused at load.
    #[test]
    fn a_channel_whose_transition_band_is_too_narrow_is_rejected_at_load() {
        let toml_text = CHANNELIZER_TOML
            .replace("output_rate_hz = 50_000.0", "output_rate_hz = 80_000.0")
            .replace(
                "offset_hz = 200_000.0",
                "offset_hz = 200_000.0\n        passband_hz = 39_000.0",
            );
        let cfg: Config = toml::from_str(&toml_text).expect("parses");
        let err = cfg
            .validate()
            .expect_err("a 1 kHz transition out of 2 MS/s cannot be cheap")
            .to_string();
        assert!(err.contains("taps"), "the budget must be named: {err}");
    }

    #[test]
    fn a_channel_outside_the_captured_band_is_rejected() {
        let toml_text =
            CHANNELIZER_TOML.replace("offset_hz = 200_000.0", "offset_hz = 1_500_000.0");
        let cfg: Config = toml::from_str(&toml_text).expect("parses");
        let err = cfg.validate().expect_err("past the band edge").to_string();
        assert!(err.contains("band edge"), "{err}");
    }

    #[test]
    fn a_negative_offset_is_legal() {
        let toml_text = CHANNELIZER_TOML.replace("offset_hz = 200_000.0", "offset_hz = -200_000.0");
        let cfg: Config = toml::from_str(&toml_text).expect("parses");
        cfg.validate().expect("a channel below the centre is fine");
    }

    #[test]
    fn the_existing_capture_validation_still_fires() {
        let bad = CAPTURE_TOML.replace("center_freq = 915_000_000.0", "center_freq = 0.0");
        let cfg: Config = toml::from_str(&bad).expect("parses");
        let err = cfg
            .validate()
            .expect_err("zero centre frequency")
            .to_string();
        assert!(err.contains("center_freq must be positive"), "{err}");
    }

    /// Capture reads a USRP, so a config with no `[input]` section must default
    /// to something usable rather than fail.
    #[test]
    fn input_defaults_are_applied_when_the_section_is_absent() {
        let cfg: Config = toml::from_str(CAPTURE_TOML).expect("parses");
        assert_eq!(cfg.input.mode, "udp");
        assert_eq!(cfg.input.listen, "0.0.0.0:4810");
        assert_eq!(cfg.input.metrics_listen, "0.0.0.0:8080");
        assert!(cfg.input.file.is_none());
        cfg.input.validate().expect("defaults are valid");
    }

    /// `replay` without a file is a config that cannot work; catching it at load
    /// beats a pod that starts and then blocks on nothing.
    #[test]
    fn replay_mode_without_a_file_is_rejected() {
        let toml_text =
            format!("{CHANNELIZER_TOML}\n        [input]\n        mode = \"replay\"\n    ");
        let cfg: Config = toml::from_str(&toml_text).expect("parses");
        let err = cfg.validate().expect_err("replay needs a file").to_string();
        assert!(err.contains("replay"), "{err}");
        assert!(err.contains("file"), "must name the missing key: {err}");
    }

    #[test]
    fn an_unknown_input_mode_is_rejected_with_both_valid_values_named() {
        let toml_text =
            format!("{CHANNELIZER_TOML}\n        [input]\n        mode = \"carrier-pigeon\"\n    ");
        let cfg: Config = toml::from_str(&toml_text).expect("parses");
        let err = cfg.validate().expect_err("unknown mode").to_string();
        assert!(err.contains("udp"), "{err}");
        assert!(err.contains("replay"), "{err}");
    }

    #[test]
    fn a_non_host_port_listen_address_is_rejected_with_the_key_named() {
        let toml_text = format!(
            "{CHANNELIZER_TOML}\n        [input]\n        listen = \"not-an-address\"\n    "
        );
        let cfg: Config = toml::from_str(&toml_text).expect("parses");
        let err = cfg.validate().expect_err("bad address").to_string();
        assert!(err.contains("listen"), "{err}");
        assert!(err.contains("host:port"), "{err}");
    }
}
