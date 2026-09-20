//! sigproc-fixture — deterministic VITA49 test vectors.
//!
//! Generates a wideband IQ buffer containing a known analogue signal and writes
//! it as a VITA49 IF Data Packet file, so every gate in this repo can assert on
//! *known* input instead of whatever the antenna happens to hear:
//!
//! ```text
//!   sigproc-fixture --am --tone 1000 --offset 200000 --seconds 1 \
//!                   --out /tmp/am.vita49
//!        │
//!        ▼
//!   sigproc-channelizer --input=replay --file=/tmp/am.vita49
//!        │
//!        ▼
//!   vita49-sink  →  {"ok":true,"rate_hz":50000,"decoded_hz":1000.0,...}
//! ```
//!
//! Scheduled for milestone M2. The CLI surface is fixed here so the gates and
//! manifests do not have to change when the generator is implemented.

use clap::Parser;
use std::path::PathBuf;

/// Deterministic VITA49 test-vector generator.
#[derive(Parser, Debug)]
#[command(name = "sigproc-fixture", version, about)]
struct Cli {
    /// Emit an amplitude-modulated (DSB, carrier present) signal.
    #[arg(long)]
    am: bool,

    /// Modulating tone in Hz.
    #[arg(long, default_value_t = 1000.0)]
    tone_hz: f64,

    /// Channel offset from the wideband centre, in Hz.
    #[arg(long, default_value_t = 200_000.0)]
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

    /// Output VITA49 packet file.
    #[arg(long)]
    out: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(cli)
}

fn run(cli: Cli) -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    log::info!("sigproc-fixture v{} starting", env!("CARGO_PKG_VERSION"));

    anyhow::bail!(
        "fixture generator is not implemented yet (milestone M2). CLI surface is \
         fixed: --am, --tone-hz={}, --offset-hz={}, --sample-rate={}, \
         --seconds={}, --depth={}, --out={}",
        cli.tone_hz,
        cli.offset_hz,
        cli.sample_rate,
        cli.seconds,
        cli.depth,
        cli.out.display()
    );
}
