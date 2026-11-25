//! sigproc — SDR RF front-end for LibreSDR B210.
//!
//! Discovers a USRP B210 via UHD, configures RF parameters from a
//! config.toml, acquires sc16 IQ samples, and forwards them as
//! VITA 49.2 IF Data Packets over UDP to a downstream waterfall consumer.
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
#[cfg(any(feature = "uhd-support", test))]
mod http_c2;
#[cfg(test)]
mod profile_tests;
#[cfg(any(feature = "uhd-support", test))]
mod telemetry;
#[cfg(feature = "uhd-support")]
mod uhd_control;

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
    let metrics = std::sync::Arc::new(telemetry::CaptureMetrics::default());
    metrics.set_rate(controllee.applied.sample_rate_hz);
    let metrics_addr = cfg.input.metrics_listen.parse()?;
    let _metrics_server = sigproc_common::metrics::spawn_http(
        metrics_addr,
        "sigproc_capture".into(),
        metrics.clone(),
    )?;
    // HTTP C2 server: optional restart-free control plane sharing the sole-owner handler.
    let c2_addr = cfg
        .control
        .http_c2_listen
        .as_ref()
        .and_then(|s| s.parse::<SocketAddr>().ok());
    let (c2_handle, c2_rx) = if let Some(addr) = c2_addr {
        match http_c2::spawn_c2_http(addr) {
            Ok((handle, rx)) => {
                log::info!("capture HTTP C2 enabled on http://{}/state", addr);
                (Some(handle), Some(rx))
            }
            Err(e) => {
                log::warn!("failed to bind capture HTTP C2 on {addr}: {e}");
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    let c2_auth = cfg.control.http_auth_token.clone();
    let mut forwarder = vita49::Vita49Forwarder::new(
        &cfg.forwarding.vita49_dest_host,
        cfg.forwarding.vita49_dest_port,
        stream_id,
    )?;
    forwarder.set_context(controllee.applied);
    let mut multicast_forwarder = if let Some(multicast) = &cfg.forwarding.multicast {
        let copy =
            vita49::Vita49Forwarder::new(&multicast.group.to_string(), multicast.port, stream_id)?;
        copy.configure_multicast(multicast.interface, multicast.ttl)?;
        log::info!(
            "LAN multicast {}:{} via {} TTL {}; primary unicast retained",
            multicast.group,
            multicast.port,
            multicast.interface,
            multicast.ttl
        );
        Some(copy)
    } else {
        None
    };
    if let Some(copy) = &mut multicast_forwarder {
        copy.set_context(controllee.applied);
        metrics.multicast(copy.stats());
    }
    log::info!(
        "capture loop started: stream_id=0x{:08x}, {} samples/packet",
        stream_id,
        forwarder.samples_per_packet()
    );

    loop {
        // UHD borrows Usrp for the streamer's lifetime. Stop/drop before
        // applying controls on this same owner thread; never open a second radio.
        let pending = {
            let mut streamer = capture::create_rx_streamer(&mut usrp)?;
            loop {
                if let Some(endpoint) = &control_socket {
                    if let Some(datagram) = endpoint.poll()? {
                        metrics.control();
                        if let Some(pending) =
                            endpoint.prepare_streaming(&datagram, &mut controllee, || {
                                streamer.send_command(&uhd::StreamCommand {
                                    time: uhd::StreamTime::Now,
                                    command_type: uhd::StreamCommandType::StopContinuous,
                                })?;
                                Ok(())
                            })?
                        {
                            break pending;
                        }
                    }
                }
                match capture::read_batch(&mut streamer) {
                    Ok((batch, metadata_error)) => {
                        if metadata_error {
                            metrics.receive_error();
                        }
                        if let Some((ts, samples)) = batch {
                            metrics.samples(samples.len() / 2);
                            forwarder.send(&samples, ts, controllee.applied.sample_rate_hz);
                            metrics.primary(forwarder.stats());
                            if let Some(copy) = &mut multicast_forwarder {
                                copy.send(&samples, ts, controllee.applied.sample_rate_hz);
                                metrics.multicast(copy.stats());
                            }
                        } else {
                            metrics.empty_read();
                            std::thread::sleep(std::time::Duration::from_micros(100));
                        }
                    }
                    Err(e) => {
                        metrics.receive_error();
                        log::error!("capture error: {}", e);
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
        };
        if let Some(endpoint) = &control_socket {
            endpoint.execute(
                pending,
                &mut controllee,
                &mut uhd_control::UhdControl(&mut usrp),
            )?;
            metrics.set_rate(controllee.applied.sample_rate_hz);
            forwarder.set_context(controllee.applied);
            if let Some(copy) = &mut multicast_forwarder {
                copy.set_context(controllee.applied);
            }
        }
        // Drain HTTP C2 requests: bounded try_recv, mutations require bearer token.
        // Collect pending mutations to apply after releasing the streamer.
        let mut pending_http = Vec::new();
        if let (Some(rx), Some(auth)) = (&c2_rx, &c2_auth) {
            for _ in 0..8 {
                match rx.try_recv() {
                    Ok(http_c2::LoopRequest::State(mut stream)) => {
                        let resp = http_c2::StateResponse {
                            rf_freq_hz: controllee.applied.rf_freq_hz,
                            sample_rate_hz: controllee.applied.sample_rate_hz,
                            bandwidth_hz: controllee.applied.bandwidth_hz,
                            gain_db: controllee.applied.gain_db,
                            stream_id,
                            packets_forwarded: forwarder.stats().packets_sent,
                            samples_forwarded: metrics
                                .samples
                                .load(std::sync::atomic::Ordering::Relaxed)
                                / 2,
                            receive_errors: metrics
                                .receive_errors
                                .load(std::sync::atomic::Ordering::Relaxed),
                            controls_processed: metrics
                                .controls
                                .load(std::sync::atomic::Ordering::Relaxed),
                        };
                        let body = serde_json::to_string(&resp)
                            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                        let _ = http_c2::write_response(
                            &mut stream,
                            "200 OK",
                            "application/json",
                            &body,
                        );
                    }
                    Ok(http_c2::LoopRequest::Config {
                        update,
                        auth_token,
                        stream,
                    }) => {
                        metrics.control();
                        let resp = if auth_token.as_deref() != Some(auth.as_str()) {
                            http_c2::ConfigResponse::error("unauthorized")
                        } else {
                            // Defer hardware application until after the streamer is released.
                            pending_http.push((update, stream));
                            continue;
                        };
                        let body = serde_json::to_string(&resp)
                            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                        let _ = http_c2::write_response(
                            &mut stream,
                            if resp.ok { "200 OK" } else { "400 Bad Request" },
                            "application/json",
                            &body,
                        );
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }
        // Apply each pending HTTP mutation through the sole-owner backend,
        // replying to each client so no queued mutation is dropped.
        for (update, stream) in pending_http {
            let resp =
                match controllee.apply_update(update, &mut uhd_control::UhdControl(&mut usrp)) {
                    Ok(ctx) => {
                        metrics.set_rate(ctx.sample_rate_hz);
                        forwarder.set_context(ctx);
                        if let Some(copy) = &mut multicast_forwarder {
                            copy.set_context(ctx);
                        }
                        http_c2::ConfigResponse::from_context(ctx)
                    }
                    Err(e) => http_c2::ConfigResponse::error(e.to_string()),
                };
            let body =
                serde_json::to_string(&resp).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
            let _ = http_c2::write_response(
                &mut stream,
                if resp.ok { "200 OK" } else { "400 Bad Request" },
                "application/json",
                &body,
            );
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
