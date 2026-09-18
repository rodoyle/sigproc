//! sigproc — SDR RF front-end for LibreSDR B210
//!
//! Discovers a USRP B210 via UHD, configures RF parameters from a
//! config.toml, acquires sc16 IQ samples, and forwards them as
//! VITA 49.0 IF Data Packets over UDP to a downstream waterfall consumer.
//!
//! Requires the `uhd-support` feature (default). Without it, only
//! the FMA kernel library (`lib.rs`) is compiled — tests pass anywhere
//! without UHD installed.

mod config;

#[cfg(feature = "uhd-support")]
mod capture;
#[cfg(feature = "uhd-support")]
mod vita49;

// ── UHD-enabled binary (default feature) ─────────────────────────────────────
#[cfg(feature = "uhd-support")]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use clap::Parser;
    use std::path::PathBuf;

    /// SDR RF front-end: UHD capture + VITA49 UDP forward.
    #[derive(Parser, Debug)]
    #[command(name = "sigproc", version, about)]
    struct Cli {
        /// Path to config.toml (default: $SIGPROC_CONFIG or /etc/sigproc/config.toml)
        #[arg(
            short,
            long,
            env = "SIGPROC_CONFIG",
            default_value = "/etc/sigproc/config.toml"
        )]
        config: PathBuf,

        /// Dry-run: load config, probe device, print info, then exit.
        #[arg(long)]
        dry_run: bool,
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let cli = Cli::parse();

    log::info!("sigproc v{} starting", env!("CARGO_PKG_VERSION"));
    log::info!("config path: {}", cli.config.display());

    // ── Load configuration ────────────────────────────────────────────────
    let cfg = config::Config::from_file(&cli.config)
        .with_context(|| format!("failed to load config from {}", cli.config.display()))?;

    log::info!("config loaded:");
    log::info!(
        "  device_args:  {}",
        if cfg.device.device_args.is_empty() {
            "(auto-detect)"
        } else {
            &cfg.device.device_args
        }
    );
    log::info!("  center_freq:  {} Hz", cfg.rf.center_freq);
    log::info!("  sample_rate:  {} Hz", cfg.rf.sample_rate);
    log::info!("  rx_gain:      {} dB", cfg.rf.rx_gain);
    log::info!("  antenna:      {}", cfg.rf.antenna);
    log::info!("  bandwidth:    {} Hz", cfg.rf.bandwidth);
    log::info!(
        "  vita49_dest:  {}:{}",
        cfg.forwarding.vita49_dest_host,
        cfg.forwarding.vita49_dest_port
    );

    // ── Dry-run: probe only ──────────────────────────────────────────────
    if cli.dry_run {
        log::info!("dry-run: probing UHD device...");
        match capture::probe_device(&cfg.device.device_args) {
            Ok(info) => {
                log::info!("device found: mboard={}", info.mboard_name);
                log::info!("  rx channels:    {}", info.num_rx_channels);
                log::info!("dry-run complete — device is reachable");
            }
            Err(e) => {
                log::error!("no UHD device found: {}", e);
                anyhow::bail!(
                    "no UHD device found: {}. Is the B210 connected and powered?",
                    e
                );
            }
        }
        return Ok(());
    }

    // ── Open USRP and start streaming ────────────────────────────────────
    log::info!("opening USRP and configuring RF...");
    let (mut usrp, sample_rate) = capture::open_usrp(&cfg.device.device_args, &cfg.rf)?;

    let mut streamer = capture::create_rx_streamer(&mut usrp)?;

    // ── VITA49 UDP forwarder ─────────────────────────────────────────────
    let stream_id = (cfg.rf.center_freq as u32).wrapping_mul(1000);
    let mut forwarder = vita49::Vita49Forwarder::new(
        &cfg.forwarding.vita49_dest_host,
        cfg.forwarding.vita49_dest_port,
        stream_id,
    )?;

    log::info!("capture loop started: stream_id=0x{:08x}", stream_id);

    // ── Main capture loop ────────────────────────────────────────────────
    loop {
        match capture::read_batch(&mut streamer) {
            Ok(Some((ts, samples))) => {
                forwarder.send(&samples, ts, sample_rate);
            }
            Ok(None) => {
                // No samples available yet — brief yield to avoid busy-wait
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
            Err(e) => {
                log::error!("capture error: {}", e);
                // Transient errors (USB hiccup, buffer overflow) — continue
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

// ── Stub binary (no uhd-support feature — for cargo test --lib only) ──────────
#[cfg(not(feature = "uhd-support"))]
fn main() {
    eprintln!(
        "sigproc: built without uhd-support feature. \
         Rebuild with --features uhd-support (default) for the capture binary."
    );
    std::process::exit(1);
}
