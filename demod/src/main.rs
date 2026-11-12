//! Standalone UDP consumer; never opens a radio or owns UHD.
use clap::{Parser, ValueEnum};
use sigproc_common::metrics::spawn_http;
use sigproc_demod::{
    recording::Recorder,
    service::{run_udp, DemodService},
    telemetry::Telemetry,
    AmDemod, Demod, NbfmDemod,
};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering::Relaxed},
    Arc,
};

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Mode {
    Nbfm,
    Am,
}

#[derive(Parser, Debug)]
#[command(
    name = "sigproc-demod",
    version,
    about = "Native 50 kHz VITA49 SC16 NBFM/AM to rotating mono WAV"
)]
struct Cli {
    /// Demodulation mode; suppressed-carrier USB/SSB is deliberately separate.
    #[arg(long, env = "DEMOD_MODE", value_enum, default_value = "nbfm")]
    mode: Mode,
    /// NBFM peak deviation used to normalize audio, in Hz.
    #[arg(long, env = "DEMOD_FM_DEVIATION_HZ", default_value_t = 2_500.0)]
    fm_deviation_hz: f64,
    #[arg(long, env = "DEMOD_LISTEN", default_value = "0.0.0.0:4930")]
    listen: SocketAddr,
    #[arg(long, env = "DEMOD_METRICS_LISTEN", default_value = "0.0.0.0:9093")]
    metrics_listen: SocketAddr,
    /// Dedicated writable directory, e.g. a mounted PVC or emptyDir.
    #[arg(long, env = "DEMOD_OUTPUT_DIR", default_value = "recordings")]
    output_dir: PathBuf,
    /// Maximum samples per WAV (50,000 samples = one second).
    #[arg(long, default_value_t = 3_000_000)]
    segment_samples: u64,
    /// Retained completed pairs; active and interrupted .part files are excluded.
    #[arg(long, default_value_t = 24)]
    max_recordings: usize,
}
fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let recorder = Recorder::new(cli.output_dir, cli.segment_samples, cli.max_recordings)?;
    let socket = UdpSocket::bind(cli.listen)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let signal = shutdown.clone();
    ctrlc::set_handler(move || signal.store(true, Relaxed))?;
    let metrics = Arc::new(Telemetry::default());
    let _http = spawn_http(cli.metrics_listen, "demod".into(), metrics.clone())?;
    let demod: Box<dyn Demod> = match cli.mode {
        Mode::Nbfm => Box::new(NbfmDemod::with_deviation(cli.fm_deviation_hz)?),
        Mode::Am => Box::new(AmDemod::new()),
    };
    let mut service = DemodService::with_demod(
        recorder,
        metrics,
        demod,
        match cli.mode {
            Mode::Nbfm => "nbfm",
            Mode::Am => "am",
        },
    );
    log::info!(
        "{:?} UDP {}: requires native 50 kHz SC16 context; no RF control",
        cli.mode,
        cli.listen
    );
    run_udp(socket, &mut service, &shutdown)
}
