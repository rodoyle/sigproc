//! vita49-sink — the verification consumer for the channelizer's output.
//!
//! Binds a UDP port, parses VITA49 IF Data Packets with the shared
//! `sigproc_common::vita49` parser, and prints one JSON verdict line on stdout:
//!
//! ```json
//! {"ok":true,"packets":977,"samples":50000,"rate_hz":50000.0,"seq_gaps":0,
//!  "decoded_hz":1000.02,"tone_error_hz":0.02}
//! ```
//!
//! It exists so the end-to-end gate has a deterministic, in-repo assertion
//! target: gates grep this line instead of judging audio by ear, and the receive
//! path it exercises is the same parser the real consumers use, so it doubles as
//! the narrowband wire-format contract test.
//!
//! Two conventions worth knowing:
//!
//! * **`rate_hz` is the decoding rate the sink was told to expect, and the tone
//!   measurement is what validates it.** VITA49 carries no sample-rate field, so
//!   the rate must be supplied; but a wrong rate would move the recovered tone
//!   proportionally, so `decoded_hz` landing on the expected tone is evidence the
//!   rate is right — not a restatement of the input.
//! * **The exit code reports only whether the sink ran.** The verdict is the JSON
//!   `ok` field, so a gate can inspect it either way (and a failing assertion
//!   should come from the assertion, not from an incidental exit status).

use clap::Parser;
use sigproc_common::dsp::{envelope_tone_hz, mean_power, sc16_to_iq, GapTracker, Iq};
use sigproc_common::vita49;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// VITA49 UDP assertion sink: parse packets, print one JSON verdict line.
#[derive(Parser, Debug)]
#[command(name = "vita49-sink", version, about)]
struct Cli {
    /// Address to bind for incoming VITA49 packets.
    #[arg(long, default_value = "0.0.0.0:4920")]
    listen: SocketAddr,

    /// Give up waiting after this many seconds. `0` means run forever (a standing
    /// consumer), which is how the chain is deployed until the demod exists.
    #[arg(long, default_value_t = 10.0)]
    duration: f64,

    /// Sample rate to decode with.
    #[arg(long, default_value_t = 50_000.0)]
    expect_rate: f64,

    /// Expected modulation tone in Hz. When set, the verdict fails unless the
    /// recovered envelope tone is within `--tone-tolerance`.
    #[arg(long)]
    expect_tone: Option<f64>,

    /// Allowed tone error in Hz.
    #[arg(long, default_value_t = 5.0)]
    tone_tolerance: f64,

    /// Exit once this long has passed with no packets, after at least one has
    /// arrived. Keeps a replay gate fast without guessing a duration. `0` disables
    /// the idle exit, which a long-running consumer needs.
    #[arg(long, default_value_t = 1.5)]
    idle_exit_secs: f64,

    /// Log a verdict line every this many seconds while running. `0` logs only at
    /// exit. A standing consumer needs this: its logs are the only way to see that
    /// the stream is still healthy.
    #[arg(long, default_value_t = 0.0)]
    report_interval: f64,

    /// Keep at most this many samples for measurement (most recent wins, so the
    /// measured block sits after the filters have settled).
    #[arg(long, default_value_t = 100_000)]
    max_measure_samples: usize,

    /// Also write the verdict JSON to this path.
    #[arg(long)]
    json: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(cli)
}

fn run(cli: Cli) -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    log::info!("vita49-sink v{} starting", env!("CARGO_PKG_VERSION"));

    let socket = std::net::UdpSocket::bind(cli.listen)
        .map_err(|e| anyhow::anyhow!("cannot bind {}: {e}", cli.listen))?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    log::info!(
        "listening for narrowband VITA49 on {} (decode rate {} Hz)",
        cli.listen,
        cli.expect_rate
    );

    let started = Instant::now();
    // A non-positive duration means "no deadline": a standing consumer should not
    // die on a timer.
    let deadline = if cli.duration > 0.0 {
        Some(started + Duration::from_secs_f64(cli.duration))
    } else {
        None
    };
    let idle_limit = if cli.idle_exit_secs > 0.0 {
        Some(Duration::from_secs_f64(cli.idle_exit_secs))
    } else {
        None
    };

    let mut buf = vec![0u8; 65_536];
    let mut packets: u64 = 0;
    let mut samples: u64 = 0;
    let mut malformed: u64 = 0;
    let mut last_packet_at: Option<Instant> = None;
    let mut tracker = GapTracker::new();
    let mut measure: Vec<Iq> = Vec::new();
    let mut last_report = Instant::now();

    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, _from)) => {
                match vita49::parse(&buf[..n]) {
                    Ok(packet) => {
                        packets += 1;
                        samples += packet.samples() as u64;
                        tracker.observe(
                            packet.sample_counter(cli.expect_rate),
                            packet.samples() as u64,
                        );
                        measure.extend(sc16_to_iq(&packet.iq_i16()));
                        // Keep the most recent samples: an early block would sit
                        // in the filter startup transient where the measured
                        // level is wrong by a large factor.
                        if measure.len() > cli.max_measure_samples {
                            let excess = measure.len() - cli.max_measure_samples;
                            measure.drain(..excess);
                        }
                        last_packet_at = Some(Instant::now());
                    }
                    Err(e) => {
                        malformed += 1;
                        log::warn!("malformed VITA49 packet: {e}");
                    }
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                let now = Instant::now();
                if let (Some(last), Some(limit)) = (last_packet_at, idle_limit) {
                    if now.duration_since(last) >= limit {
                        log::info!("stream idle for {limit:?}; finishing");
                        break;
                    }
                }
                if let Some(deadline) = deadline {
                    if now >= deadline {
                        log::info!("reached --duration; finishing");
                        break;
                    }
                }
                // Periodic health line for a standing consumer: without it the
                // pod is silent until the day it exits.
                if cli.report_interval > 0.0
                    && now.duration_since(last_report) >= Duration::from_secs_f64(cli.report_interval)
                {
                    last_report = now;
                    log::info!(
                        "standing: packets={packets} samples={samples} malformed={malformed} \
                         seq_gaps={} gap_samples={}",
                        tracker.gaps(),
                        tracker.gap_samples()
                    );
                }
            }
            Err(e) => log::error!("receive error: {e}"),
        }
    }

    let decoded_hz = if measure.is_empty() {
        None
    } else {
        // Envelope tone is what an AM detector recovers; the shared helper keeps
        // the sink and the tests measuring exactly the same quantity.
        envelope_tone_hz(&measure, cli.expect_rate, 50.0, 0.1 * cli.expect_rate)
    };
    let power_dbfs = mean_power(&measure).map(|p| {
        if p > 0.0 {
            10.0 * p.log10()
        } else {
            f32::NEG_INFINITY
        }
    });

    let tone_error_hz = match (decoded_hz, cli.expect_tone) {
        (Some(got), Some(want)) => Some(got - want),
        _ => None,
    };
    let tone_ok = match tone_error_hz {
        Some(err) => err.abs() <= cli.tone_tolerance,
        None => cli.expect_tone.is_none(),
    };
    let ok = packets > 0 && malformed == 0 && tracker.gaps() == 0 && tone_ok;

    let verdict = serde_json::json!({
        "ok": ok,
        "packets": packets,
        "samples": samples,
        "malformed": malformed,
        "rate_hz": cli.expect_rate,
        "seq_gaps": tracker.gaps(),
        "gap_samples": tracker.gap_samples(),
        "resets": tracker.resets(),
        "decoded_hz": decoded_hz,
        "expected_tone_hz": cli.expect_tone,
        "tone_error_hz": tone_error_hz,
        "tone_tolerance_hz": cli.tone_tolerance,
        "power_dbfs": power_dbfs.filter(|p| p.is_finite()),
        "listen": cli.listen.to_string(),
        "elapsed_secs": started.elapsed().as_secs_f64(),
    });

    if let Some(path) = &cli.json {
        std::fs::write(path, serde_json::to_string_pretty(&verdict)?)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
        log::info!("wrote verdict to {}", path.display());
    }
    println!("{}", serde_json::to_string(&verdict)?);

    if !ok {
        log::error!("verdict not ok: {verdict}");
    }
    Ok(())
}
