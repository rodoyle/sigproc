//! vita49-sink — the verification consumer for the channelizer's output.
//!
//! Binds the narrowband UDP port, parses VITA49 IF Data Packets with the shared
//! `sigproc-common::vita49` parser, and prints a single JSON verdict line:
//!
//! ```json
//! {"ok":true,"packets":512,"rate_hz":50000,"seq_gaps":0,"decoded_hz":1000.0}
//! ```
//!
//! It exists so the end-to-end gate has a *deterministic, in-repo* assertion
//! target: gates grep this line rather than judging audio by ear, and the
//! receive path it exercises is the same parser the real consumers use, so it
//! doubles as the narrowband wire-format contract test.
//!
//! Scheduled for milestone M3 (alongside the channelizer's receive loop, so the
//! local end-to-end gate can run before any image is built). CLI surface fixed
//! here.

use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;

/// VITA49 UDP assertion sink: parse packets, print one JSON verdict line.
#[derive(Parser, Debug)]
#[command(name = "vita49-sink", version, about)]
struct Cli {
    /// Address to bind for incoming VITA49 packets.
    #[arg(long, default_value = "0.0.0.0:4920")]
    listen: SocketAddr,

    /// How long to collect before printing the verdict, in seconds.
    #[arg(long, default_value_t = 5.0)]
    duration: f64,

    /// Sample rate to expect on the wire; mismatch fails the verdict.
    #[arg(long, default_value_t = 50_000.0)]
    expect_rate: f64,

    /// Also write the verdict JSON to this path (and a `-audio.json` sidecar).
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

    anyhow::bail!(
        "sink is not implemented yet (milestone M3). CLI surface is fixed: \
         --listen={}, --duration={}, --expect-rate={}{}",
        cli.listen,
        cli.duration,
        cli.expect_rate,
        cli.json
            .as_ref()
            .map(|p| format!(", --json={}", p.display()))
            .unwrap_or_default()
    );
}
