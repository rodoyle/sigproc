//! sigproc-fixture — deterministic VITA49 test vectors.
//!
//! Generates a wideband IQ buffer containing a known analogue signal and writes
//! it as a VITA49 IF Data Packet file, so every gate in this repo asserts on
//! *known* input instead of whatever the antenna happens to hear:
//!
//! ```text
//!   sigproc-fixture --am --tone-hz 1000 --offset-hz 200000 --seconds 1 \
//!                   --out /tmp/am.vita49
//!        │
//!        ▼
//!   sigproc-channelizer --input=replay --file=/tmp/am.vita49
//!        │
//!        ▼
//!   vita49-sink  →  {"ok":true,"rate_hz":50000,"decoded_hz":1000.0,...}
//! ```
//!
//! Conventions that the gates depend on:
//!
//! * **stdout carries exactly one machine-readable JSON verdict line**; all
//!   human-facing logging goes to stderr. A gate greps stdout for `"ok":true`
//!   without having to filter log noise.
//! * The sidecar `<out>.meta.json` records what was generated, so a gate can
//!   compare a measured tone against the intended one rather than against a
//!   constant duplicated in a shell script.
//! * The generator **refuses** to write a vector whose peak envelope would clip
//!   sc16. A silently clipped fixture would make every downstream amplitude
//!   assertion meaningless.

use clap::Parser;
use sigproc_common::vectors::{vita49_stream, AmTone};
use std::io::Write;
use std::path::PathBuf;

/// Deterministic VITA49 test-vector generator.
#[derive(Parser, Debug)]
#[command(name = "sigproc-fixture", version, about)]
struct Cli {
    /// Emit an amplitude-modulated (DSB, carrier present) signal.
    #[arg(long)]
    am: bool,

    /// Modulating tone in Hz.
    #[arg(long, alias = "tone", default_value_t = 1000.0)]
    tone_hz: f64,

    /// Channel offset from the wideband centre, in Hz.
    #[arg(long, alias = "offset", default_value_t = 200_000.0)]
    offset_hz: f64,

    /// Wideband sample rate in Hz.
    #[arg(long, default_value_t = 2_000_000.0)]
    sample_rate: f64,

    /// Duration in seconds.
    #[arg(long, default_value_t = 1.0)]
    seconds: f64,

    /// Modulation depth, 0.0-1.0.
    #[arg(long, default_value_t = 0.5)]
    depth: f64,

    /// Unmodulated carrier amplitude as a fraction of full scale.
    #[arg(long, default_value_t = 0.5)]
    amplitude: f64,

    /// Samples per VITA49 packet (each packet carries one I/Q pair per sample).
    #[arg(long, default_value_t = 2048)]
    samples_per_packet: usize,

    /// Timestamp of the first sample, in seconds.
    #[arg(long, default_value_t = 0.0)]
    start_secs: f64,

    /// Output VITA49 packet file.
    #[arg(long)]
    out: PathBuf,

    /// Sidecar metadata path (default: `<out>.meta.json`).
    #[arg(long)]
    meta: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(cli)
}

fn run(cli: Cli) -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    if !cli.am {
        anyhow::bail!(
            "only --am (AM/DSB) vectors are implemented; NBFM is a deferred item. \
             Pass --am to generate the AM test vector."
        );
    }
    if cli.samples_per_packet == 0 {
        anyhow::bail!("--samples-per-packet must be at least 1");
    }

    let spec = AmTone {
        tone_hz: cli.tone_hz,
        offset_hz: cli.offset_hz,
        sample_rate: cli.sample_rate,
        seconds: cli.seconds,
        depth: cli.depth,
        amplitude: cli.amplitude,
    };

    // Refuse to clip. A clipped fixture would silently turn every downstream
    // amplitude assertion into a measurement of the clipper.
    let peak = spec.peak_amplitude();
    if peak > 1.0 {
        anyhow::bail!(
            "peak envelope amplitude {peak} exceeds full scale: amplitude {} with depth {} \
             would clip sc16. Use --amplitude {} or less (peak = amplitude x (1 + depth)).",
            spec.amplitude,
            spec.depth.clamp(0.0, 1.0),
            1.0 / (1.0 + spec.depth.clamp(0.0, 1.0))
        );
    }

    let iq = spec.generate();
    let (bytes, packets) = vita49_stream(
        &iq,
        spec.sample_rate,
        cli.samples_per_packet,
        cli.start_secs,
    );

    let file = std::fs::File::create(&cli.out)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", cli.out.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    writer.write_all(&bytes)?;
    writer.flush()?;

    let meta_path = cli.meta.clone().unwrap_or_else(|| {
        let mut p = cli.out.clone();
        let name = format!(
            "{}.meta.json",
            p.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "fixture".to_string())
        );
        p.set_file_name(name);
        p
    });

    let meta = serde_json::json!({
        "ok": true,
        "mode": "am",
        "tone_hz": spec.tone_hz,
        "offset_hz": spec.offset_hz,
        "sample_rate_hz": spec.sample_rate,
        "seconds": spec.seconds,
        "depth": spec.depth.clamp(0.0, 1.0),
        "amplitude": spec.amplitude,
        "peak_amplitude": peak,
        "samples": iq.len(),
        "samples_per_packet": cli.samples_per_packet,
        "packets": packets,
        "start_secs": cli.start_secs,
        "bytes": bytes.len(),
        "file": cli.out.display().to_string(),
    });
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;

    log::info!(
        "wrote {} ({} samples, {} packets, {} bytes) and {}",
        cli.out.display(),
        iq.len(),
        packets,
        bytes.len(),
        meta_path.display()
    );

    // Single machine-readable line on stdout.
    println!("{}", serde_json::to_string(&meta)?);
    Ok(())
}
