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
//! The DSP itself (NCO mixer, FIR low-pass, integer decimator, sequence-gap
//! tracking) lives in `sigproc-common::dsp` and is scheduled for milestone M2;
//! the receive/forward loop for milestone M3. This file is the M1 skeleton: the
//! CLI surface is fixed here so the contract does not move under the tests, and
//! every path that is not yet implemented fails loudly instead of pretending to
//! work.
//!
//! Input modes:
//! * `--input=udp`    — subscribe to the wideband VITA49 stream (production).
//! * `--input=replay` — read VITA49 packets from a file written by
//!   `sigproc-fixture`. This is the verification path: the code after the
//!   VITA49 receive boundary is identical, so the DSP is exercised for real
//!   rather than mocked.

use clap::{Parser, ValueEnum};
use std::path::PathBuf;

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

    /// VITA49 packet source.
    #[arg(long, value_enum, default_value_t = InputMode::Udp)]
    input: InputMode,

    /// Packet file for `--input=replay`.
    #[arg(long, required_if_eq("input", "replay"))]
    file: Option<PathBuf>,

    /// In replay mode, pace packets at the configured sample rate instead of
    /// processing them as fast as possible.
    #[arg(long)]
    realtime: bool,

    /// Validate configuration and report the resolved channel plan, then exit.
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
    log::info!("input mode:  {:?}", cli.input);

    // Deliberately explicit: a skeleton that silently exits 0 would let a gate
    // pass on an unimplemented channelizer.
    anyhow::bail!(
        "channelizer receive/forward loop is not implemented yet \
         (milestone M3; DSP primitives land in M2). CLI surface is fixed: \
         --config, --input=udp|replay, --file, --realtime, --dry-run"
    );
}
