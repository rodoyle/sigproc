//! sigproc-channelizer — wideband VITA49 in, one narrowband channel out.
//!
//! Second service boundary in the chain:
//!
//! ```text
//!   sigproc (B210, 2 MS/s @ 915 MHz)
//!        │  VITA49 UDP, wideband sc16
//!        ▼
//!   sigproc-channelizer            ← this binary
//!        │  VITA49 UDP, one channel, decimated (50 kS/s by default)
//!        ▼
//!   vita49-sink / demod / waterfall
//! ```
//!
//! Input modes:
//! * `--input=udp`    — subscribe to the wideband VITA49 stream (production).
//! * `--input=replay` — read VITA49 packets from a file written by
//!   `sigproc-fixture`. The code path after the VITA49 receive boundary is
//!   identical, so the DSP is exercised for real rather than mocked. Replay
//!   drains the file and exits, which is what makes it usable as a gate.
//!
//! Conventions the gates rely on: stdout carries one machine-readable JSON
//! report line; logs go to stderr; `--report` writes the same report to a file.

use clap::{Parser, ValueEnum};
use sigproc_channelizer::{ChannelizerService, Report, ServiceConfig};
use sigproc_common::config::Config;
use sigproc_common::metrics::{spawn_http, Metrics};
use sigproc_common::vita49::{PacketCursor, Vita49Forwarder};
use std::path::PathBuf;
use std::sync::Arc;

/// Where wideband VITA49 packets come from.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum InputMode {
    /// Receive on the configured UDP port (production).
    Udp,
    /// Replay a VITA49 packet file produced by `sigproc-fixture` (verification).
    Replay,
}

/// Channelizer: isolate one narrowband channel from the wideband VITA49 stream.
#[derive(Parser, Debug)]
#[command(name = "sigproc-channelizer", version, about)]
struct Cli {
    /// Path to config.toml (default: $SIGPROC_CONFIG or /etc/sigproc/config.toml)
    #[arg(
        short,
        long,
        env = "SIGPROC_CONFIG",
        default_value = "/etc/sigproc/config.toml"
    )]
    config: PathBuf,

    /// VITA49 packet source (overrides `[input] mode`).
    #[arg(long, value_enum)]
    input: Option<InputMode>,

    /// Packet file for replay mode (overrides `[input] file`).
    #[arg(long)]
    file: Option<PathBuf>,

    /// In replay mode, pace packets at the input sample rate instead of draining.
    #[arg(long)]
    realtime: bool,

    /// Write the JSON report to this path when the stream ends.
    #[arg(long)]
    report: Option<PathBuf>,

    /// Validate configuration and log the resolved channel plan, then exit.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(cli)
}

fn run(cli: Cli) -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    log::info!(
        "sigproc-channelizer v{} starting",
        env!("CARGO_PKG_VERSION")
    );
    log::info!("config path: {}", cli.config.display());

    let cfg = Config::from_file(&cli.config)?;
    let channel = cfg.channel.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no [channel] section: the channelizer needs offset_hz and \
             output_rate_hz to know which channel to isolate",
            cli.config.display()
        )
    })?;

    let input_rate = cfg.rf.sample_rate;
    // One mapping from config to plan, shared with the integration tests.
    let (plan, service_cfg) = ServiceConfig::from_channel(channel, input_rate)?;

    log::info!("resolved channel plan:");
    log::info!("  input_rate:   {input_rate} Hz");
    log::info!("  offset:       {} Hz", channel.offset_hz);
    log::info!(
        "  output_rate:  {} Hz (decimation {})",
        channel.output_rate_hz,
        channel.decimation(input_rate)?
    );
    log::info!(
        "  passband:     +/- {} Hz, stopband {} dB at {} Hz",
        channel.passband(),
        channel.stopband_db,
        channel.stopband()
    );
    for (i, stage) in plan.iter().enumerate() {
        log::info!(
            "  stage {i}: /{} {} Hz -> {} Hz, {} taps, cutoff {} Hz, stopband {} Hz",
            stage.factor,
            stage.input_rate,
            stage.output_rate,
            stage.taps,
            stage.cutoff_hz,
            stage.stopband_hz
        );
    }

    let mode = match (cli.input, cfg.input.mode.as_str()) {
        (Some(m), _) => m,
        (None, "udp") => InputMode::Udp,
        (None, "replay") => InputMode::Replay,
        (None, other) => anyhow::bail!("[input] mode {other:?} is not udp or replay"),
    };
    let file = cli
        .file
        .or_else(|| cfg.input.file.as_ref().map(PathBuf::from));
    let realtime = cli.realtime || cfg.input.realtime;

    if cli.dry_run {
        log::info!("dry-run: configuration and channel plan are valid");
        return Ok(());
    }

    // Metrics/health surface.
    let metrics = Arc::new(Metrics::new());
    let metrics_addr = cfg
        .input
        .metrics_listen
        .parse()
        .map_err(|e| anyhow::anyhow!("[input] metrics_listen: {e}"))?;
    // Keep the handle so the listener lives as long as the process.
    let _http = spawn_http(
        metrics_addr,
        "channelizer".to_string(),
        Arc::clone(&metrics),
    )?;

    // Downstream destination for the narrowband stream.
    let forwarder = Vita49Forwarder::new(
        &cfg.forwarding.vita49_dest_host,
        cfg.forwarding.vita49_dest_port,
        0,
    )?;
    log::info!(
        "narrowband output -> {}:{}",
        cfg.forwarding.vita49_dest_host,
        cfg.forwarding.vita49_dest_port
    );

    let mut service = ChannelizerService::new(plan, service_cfg, forwarder, Arc::clone(&metrics));
    log::info!(
        "filters settle after {} input samples; level/offset are published only after that",
        service.settling_samples()
    );

    match mode {
        InputMode::Replay => {
            let path = file.ok_or_else(|| {
                anyhow::anyhow!("replay mode needs a packet file (--file or [input] file)")
            })?;
            let bytes = std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
            log::info!("replay: {} bytes from {}", bytes.len(), path.display());

            let mut cursor = PacketCursor::new(&bytes);
            let mut started = std::time::Instant::now();
            let mut replayed_packets: u64 = 0;
            // Feed the exact file bytes into the same parse path UDP uses.
            while let Some(next) = cursor.next_slice() {
                match next {
                    Ok(raw) => {
                        let outcome = service.process_datagram(raw);
                        replayed_packets += 1;
                        if realtime {
                            let elapsed = started.elapsed().as_secs_f64();
                            let produced = outcome.samples_in as f64 / input_rate;
                            if produced > elapsed {
                                std::thread::sleep(std::time::Duration::from_secs_f64(
                                    produced - elapsed,
                                ));
                            }
                            started = std::time::Instant::now();
                        }
                    }
                    Err(e) => {
                        // Keep going: one corrupt packet in a file should not hide
                        // the rest of it, but it must still be counted so the
                        // report cannot claim ok for a file it could not read.
                        service.note_malformed(&e.to_string());
                    }
                }
            }

            let report = service.finish();
            emit_report(&report, cli.report.as_deref())?;
            if !report.ok {
                log::error!("replay finished with ok=false: {:?}", report);
            }
            log::info!("replayed {replayed_packets} packets");
        }
        InputMode::Udp => {
            let listen = &cfg.input.listen;
            let socket = std::net::UdpSocket::bind(listen)
                .map_err(|e| anyhow::anyhow!("cannot bind {listen}: {e}"))?;
            socket.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
            log::info!("listening for wideband VITA49 on {listen}");

            // Receive buffers sized for the largest VITA49 datagram this chain
            // produces (2048-sample packets are 8212 bytes) with headroom.
            let mut buf = vec![0u8; 65_536];
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((n, _from)) => {
                        service.process_datagram(&buf[..n]);
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        // Idle: nothing to do, the metrics endpoint is live.
                    }
                    Err(e) => log::error!("receive error: {e}"),
                }
            }
        }
    }

    Ok(())
}

/// Write the report where the gates look for it: a file if asked, and always one
/// machine-readable line on stdout.
fn emit_report(report: &Report, path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let json = serde_json::to_string(report)?;
    if let Some(path) = path {
        std::fs::write(path, serde_json::to_string_pretty(report)?)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
        log::info!("wrote report to {}", path.display());
    }
    println!("{json}");
    Ok(())
}
