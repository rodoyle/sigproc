//! sigproc — SDR RF front-end for LibreSDR B210.
//!
//! Discovers a USRP B210 via UHD, configures RF parameters from a
//! config.toml, acquires sc16 IQ samples, and forwards them as
//! VITA 49.0 IF Data Packets over UDP to a downstream waterfall consumer.
//!
//! VITA49 framing and config parsing now live in `sigproc-common`, because the
//! channelizer and any other consumer share that wire format. This binary owns
//! only the UHD capture path.
//!
//! The CLI is parsed in **every** build configuration so `--version` works even
//! where libuhd is absent (the crate is not UHD-dependent for the CLI). Only the
//! capture path requires the `uhd-support` feature, which is opt-in because
//! libuhd is not present on every build host or in every service image.

use clap::Parser;
use std::path::PathBuf;

#[cfg(feature = "uhd-support")]
mod capture;

#[cfg(any(feature = "uhd-support", test))]
mod control;
#[cfg(feature = "uhd-support")]
mod uhd_control;
#[cfg(test)]
mod profile_tests;

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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(cli)
}

// ── UHD-enabled capture path (default feature) ───────────────────────────────
#[cfg(feature = "uhd-support")]
fn run(cli: Cli) -> anyhow::Result<()> {
    use anyhow::Context;
    use sigproc_common::{config, vita49};

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

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
    // Bind before opening hardware so endpoint errors fail at startup.
    let control_socket = control::ControlSocket::bind(&cfg.control)?;
    log::info!("opening USRP and configuring RF...");
    let (mut usrp, _) = capture::open_usrp(&cfg.device.device_args, &cfg.rf)?;
    let stream_id = cfg.control.stream_id;
    let mut controllee = vita49::Controllee::new(uhd_control::readback(&usrp)?, stream_id);
    let mut forwarder = vita49::Vita49Forwarder::new(
        &cfg.forwarding.vita49_dest_host,
        cfg.forwarding.vita49_dest_port,
        stream_id,
    )?;
    forwarder.set_context(controllee.applied);
    let mut multicast_forwarder = if let Some(multicast) = &cfg.forwarding.multicast {
        let copy = vita49::Vita49Forwarder::new(&multicast.group.to_string(), multicast.port, stream_id)?;
        copy.configure_multicast(multicast.interface, multicast.ttl)?;
        log::info!("LAN multicast {}:{} via {} TTL {}; primary unicast retained",
            multicast.group, multicast.port, multicast.interface, multicast.ttl);
        Some(copy)
    } else {
        None
    };
    if let Some(copy) = &mut multicast_forwarder {
        copy.set_context(controllee.applied);
    }
    log::info!(
        "capture loop started: stream_id=0x{:08x}, {} samples/packet",
        stream_id,
        forwarder.samples_per_packet()
    );

    loop {
        // UHD borrows Usrp for the streamer's lifetime. Stop/drop before
        // applying controls on this same owner thread; never open a second radio.
        let datagram = {
            let mut streamer = capture::create_rx_streamer(&mut usrp)?;
            loop {
                if let Some(endpoint) = &control_socket {
                    if let Some(datagram) = endpoint.poll()? {
                        streamer.send_command(&uhd::StreamCommand {
                            time: uhd::StreamTime::Now,
                            command_type: uhd::StreamCommandType::StopContinuous,
                        })?;
                        break datagram;
                    }
                }
                match capture::read_batch(&mut streamer) {
                    Ok(Some((ts, samples))) => {
                        forwarder.send(&samples, ts, controllee.applied.sample_rate_hz);
                        if let Some(copy) = &mut multicast_forwarder {
                            copy.send(&samples, ts, controllee.applied.sample_rate_hz);
                        }
                    }
                    Ok(None) => {
                        std::thread::sleep(std::time::Duration::from_micros(100));
                    }
                    Err(e) => {
                        log::error!("capture error: {}", e);
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
        };
        if let Some(endpoint) = &control_socket {
            endpoint.dispatch(&datagram, &mut controllee, &mut uhd_control::UhdControl(&mut usrp))?;
            forwarder.set_context(controllee.applied);
            if let Some(copy) = &mut multicast_forwarder {
                copy.set_context(controllee.applied);
            }
        }
    }
}

// ── Stub path (no uhd-support feature — build/test without libuhd) ───────────
#[cfg(not(feature = "uhd-support"))]
fn run(_cli: Cli) -> anyhow::Result<()> {
    anyhow::bail!(
        "sigproc v{} built without the uhd-support feature: capture needs libuhd. \
         Rebuild with --features uhd-support to stream from a USRP (the capture \
         image does exactly that). The VITA49 framer is testable without it: \
         cargo test -p sigproc-common",
        env!("CARGO_PKG_VERSION")
    );
}
