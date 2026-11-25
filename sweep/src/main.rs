//! `sigproc-sweep` — bounded 140-150 MHz NBFM sweep controller.
//!
//! Drives the channelizer's HTTP C2 plane through the plan, prints the applied
//! readback and observation for every dwell point, and restores the original
//! channel on exit. See `docs/plans/sweep-controller.md` for the operating
//! procedure and the reasoning behind the safety rules.
//!
//! Exit codes:
//!   0  sweep finished (completed, or stopped at its own step budget/deadline)
//!   2  interrupted by the operator
//!   3  a control-plane step failed (refused, undelivered, or uncertain)
//!   4  the restore did not read back — the recovery file holds the original
//!   5  preflight refused to sweep, so nothing was mutated
//!   6  usage, plan, or lock error — nothing was mutated
//!
//! Interruption: press Enter on stdin for a clean stop, which finishes the
//! current step and restores. A hard Ctrl-C kills the process without unwinding,
//! so the recovery file is written before the first mutation and
//! `--restore-from` re-applies it afterwards.

use std::{
    io::BufRead,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use clap::Parser;
use sigproc_sweep::{
    http::{CaptureHttp, ChannelizerHttp, HttpLimits},
    read_recovery_file, ControllerLock, LockError, PlanError, RestoreOutcome, RfHandling, StdClock,
    StopReason, SweepConfig, SweepController, SweepPlan, SweepReport, DEFAULT_RF_CENTER_HZ,
    DEFAULT_WIDEBAND_RATE_HZ, NBFM_STEP_HZ, SWEEP_START_HZ, SWEEP_STOP_HZ,
};

const EXIT_OK: i32 = 0;
const EXIT_INTERRUPTED: i32 = 2;
const EXIT_CONTROL_FAILED: i32 = 3;
const EXIT_RESTORE_FAILED: i32 = 4;
const EXIT_REFUSED: i32 = 5;
const EXIT_USAGE: i32 = 6;

#[derive(Parser, Debug)]
#[command(
    name = "sigproc-sweep",
    about = "Bounded 140-150 MHz sweep controller driving the channelizer C2 plane",
    long_about = None
)]
struct Cli {
    /// Channelizer C2 endpoint, e.g. 10.42.0.169:8080 (host:port)
    #[arg(long, env = "SIGPROC_CHANNELIZER_C2")]
    channelizer_c2: Option<String>,

    /// Bearer token for the channelizer C2 plane, if it is configured to require one
    #[arg(long, env = "SIGPROC_CHANNELIZER_TOKEN")]
    channelizer_token: Option<String>,

    /// Capture C2 endpoint (host:port). Required to verify the fixed wideband RF
    #[arg(long, env = "SIGPROC_CAPTURE_C2")]
    capture_c2: Option<String>,

    /// Bearer token for the capture C2 plane
    #[arg(long, env = "SIGPROC_C2_TOKEN")]
    capture_token: Option<String>,

    /// Lowest frequency to visit, in hertz
    #[arg(long, default_value_t = SWEEP_START_HZ)]
    start_hz: u64,

    /// Highest frequency to visit, in hertz (inclusive)
    #[arg(long, default_value_t = SWEEP_STOP_HZ)]
    stop_hz: u64,

    /// Step between dwell points, in hertz (one 12.5 kHz NBFM channel)
    #[arg(long, default_value_t = NBFM_STEP_HZ)]
    step_hz: u64,

    /// Centre of the fixed wideband capture the offsets are relative to, in hertz
    #[arg(long, default_value_t = DEFAULT_RF_CENTER_HZ)]
    rf_center_hz: u64,

    /// Channel output rate to request, in hertz
    #[arg(long, default_value_t = NBFM_STEP_HZ)]
    channel_rate_hz: u64,

    /// Wideband rate the plan is expected to live in, in hertz. Used for the
    /// channel-fit check when --capture-c2 is not given
    #[arg(long, default_value_t = DEFAULT_WIDEBAND_RATE_HZ)]
    wideband_rate_hz: u64,

    /// Dwell after each retune before observing, in milliseconds
    #[arg(long, default_value_t = 120)]
    settle_ms: u64,

    /// Stop after this many dwell points (bounded smoke run)
    #[arg(long)]
    max_steps: Option<u64>,

    /// Stop after this many milliseconds
    #[arg(long)]
    deadline_ms: Option<u64>,

    /// Skip the read-only observation after each settled step
    #[arg(long)]
    no_observe: bool,

    /// Allow this run to move the RF front end when the fixed wideband does not
    /// cover the plan. The original RF is restored on exit either way
    #[arg(long)]
    apply_rf: bool,

    /// Controller lock file; a second controller cannot sweep while this is held
    #[arg(long, default_value = "sigproc-sweep.lock")]
    lock_file: PathBuf,

    /// Break a lock older than --stale-after-ms and take it
    #[arg(long)]
    break_stale_lock: bool,

    /// Age at which --break-stale-lock considers a lock abandoned
    #[arg(long, default_value_t = 600_000)]
    stale_after_ms: u64,

    /// Original state is written here before the first mutation
    #[arg(long, default_value = "sigproc-sweep-original.json")]
    recovery_file: PathBuf,

    /// Write the full per-step JSON report here
    #[arg(long)]
    report_file: Option<PathBuf>,

    /// Re-apply a recovery file's state and exit, instead of sweeping.
    /// Use this after a hard kill that bypassed the controller's restore
    #[arg(long)]
    restore_from: Option<PathBuf>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(EXIT_USAGE);
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<i32> {
    // The lock is taken for both modes: a restore racing a live sweep would put
    // the channel somewhere neither operator chose.
    let lock = acquire_lock(&cli)?;

    let limits = HttpLimits::default();
    let channelizer_addr: SocketAddr = require_addr(&cli, cli.channelizer_c2.as_deref())?;
    let mut channelizer =
        ChannelizerHttp::new(channelizer_addr, cli.channelizer_token.clone(), limits);

    if let Some(path) = &cli.restore_from {
        return restore_from_file(&cli, &mut channelizer, path);
    }

    let plan = SweepPlan::new(cli.start_hz, cli.stop_hz, cli.step_hz).map_err(plan_error)?;
    let cfg = SweepConfig {
        plan,
        rf_center_hz: cli.rf_center_hz,
        channel_rate_hz: cli.channel_rate_hz,
        settle_ms: cli.settle_ms,
        observe: !cli.no_observe,
        max_steps: cli.max_steps,
        deadline_ms: cli.deadline_ms,
        rf_target: None,
        apply_rf: cli.apply_rf,
        wideband_rate_hz: cli.wideband_rate_hz,
    };

    let steps = plan.steps();
    println!(
        "sweep: {}..{} Hz step {} Hz -> {} dwell points, channel rate {} Hz, centre {} Hz",
        plan.start_hz,
        plan.stop_hz,
        plan.step_hz,
        steps.len(),
        cfg.channel_rate_hz,
        cfg.rf_center_hz
    );
    println!(
        "offsets: {}..{} Hz relative to the fixed wideband",
        steps[0] as i64 - cfg.rf_center_hz as i64,
        *steps.last().unwrap() as i64 - cfg.rf_center_hz as i64
    );

    let stop = Arc::new(AtomicBool::new(false));
    spawn_stdin_stop(stop.clone());

    let mut controller =
        SweepController::new(Box::new(channelizer), Box::new(StdClock::new()), stop)
            .with_recovery_file(&cli.recovery_file);

    if let Some(addr) = cli.capture_c2.as_deref() {
        let addr = require_addr(&cli, Some(addr))?;
        let capture = CaptureHttp::new(addr, cli.capture_token.clone(), limits);
        controller = controller.with_capture(Box::new(capture));
    } else if cli.apply_rf {
        anyhow::bail!(
            "--apply-rf needs --capture-c2: the RF cannot be moved without the capture C2 plane"
        );
    }

    let report = controller.run(&cfg);
    print_report(&report);

    if let Some(path) = &cli.report_file {
        let body = serde_json::to_vec_pretty(&report)?;
        std::fs::write(path, [body.as_slice(), b"\n"].concat())?;
        println!("report: {}", path.display());
    }

    if let RestoreOutcome::Failed { .. } = report.restore {
        println!(
            "recovery: the original channel is recorded in {} — re-apply it with \
             --restore-from {}",
            cli.recovery_file.display(),
            cli.recovery_file.display()
        );
    }
    drop(lock);

    Ok(exit_code(&report))
}

/// Take the controller lock, honouring `--break-stale-lock`.
fn acquire_lock(cli: &Cli) -> anyhow::Result<ControllerLock> {
    let take = || -> Result<ControllerLock, LockError> {
        if cli.break_stale_lock {
            ControllerLock::break_stale(cli.lock_file.clone(), cli.stale_after_ms)
        } else {
            ControllerLock::acquire(cli.lock_file.clone())
        }
    };
    take().map_err(|e| match e {
        LockError::Held { holder } => anyhow::anyhow!(
            "{holder}\nrefusing to sweep: another controller holds {}. Wait for it to finish, \
             or pass --break-stale-lock if you know the holder is dead",
            cli.lock_file.display()
        ),
        LockError::Io(detail) => anyhow::anyhow!(detail),
    })
}

fn require_addr(cli: &Cli, value: Option<&str>) -> anyhow::Result<SocketAddr> {
    let Some(value) = value else {
        anyhow::bail!(
            "a control plane endpoint is required (--channelizer-c2 host:port); \
             recovery file in {}",
            cli.recovery_file.display()
        );
    };
    value
        .parse()
        .map_err(|e| anyhow::anyhow!("{value} is not a host:port endpoint: {e}"))
}

fn plan_error(e: PlanError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Re-apply a recorded original state after an interrupted run.
fn restore_from_file(
    cli: &Cli,
    channelizer: &mut ChannelizerHttp,
    path: &std::path::Path,
) -> anyhow::Result<i32> {
    let saved = read_recovery_file(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    println!(
        "restoring from {}: channel offset {} Hz rate {} Hz, written {} ms since epoch",
        path.display(),
        saved.channel.offset_hz,
        saved.channel.output_rate_hz,
        saved.written_unix_ms
    );

    use sigproc_sweep::ChannelControl;
    let applied = channelizer
        .apply_channel(saved.channel.offset_hz, saved.channel.output_rate_hz)
        .map_err(|e| anyhow::anyhow!("channel restore failed: {e}"))?;
    println!(
        "channel readback: offset {} Hz rate {} Hz (settling {} samples)",
        applied.offset_hz, applied.output_rate_hz, applied.settling_samples
    );
    if applied.offset_hz != saved.channel.offset_hz
        || applied.output_rate_hz != saved.channel.output_rate_hz
    {
        eprintln!("restore did not read back as applied; leaving the recovery file in place");
        return Ok(EXIT_RESTORE_FAILED);
    }

    if let Some(rf) = saved.rf {
        let Some(addr) = cli.capture_c2.as_deref() else {
            eprintln!(
                "the recovery file records RF {} Hz / {} Hz but no --capture-c2 was given; \
                 the channel is restored, the RF is not",
                rf.rf_freq_hz, rf.sample_rate_hz
            );
            return Ok(EXIT_RESTORE_FAILED);
        };
        let addr = require_addr(cli, Some(addr))?;
        let mut capture = CaptureHttp::new(addr, cli.capture_token.clone(), HttpLimits::default());
        use sigproc_sweep::CaptureControl;
        let applied = capture
            .apply_rf(rf)
            .map_err(|e| anyhow::anyhow!("RF restore failed: {e}"))?;
        println!(
            "RF readback: {} Hz / {} Hz",
            applied.rf_freq_hz, applied.sample_rate_hz
        );
        if applied != rf {
            return Ok(EXIT_RESTORE_FAILED);
        }
    }

    println!("restore complete");
    Ok(EXIT_OK)
}

/// Set the stop flag when the operator presses Enter.
///
/// End-of-file is not an interrupt: a non-interactive run must not stop itself
/// the moment stdin is closed.
fn spawn_stdin_stop(stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        if matches!(stdin.lock().lines().next(), Some(Ok(_))) {
            stop.store(true, Ordering::SeqCst);
            println!("stop requested: finishing the current step, then restoring");
        }
    });
}

fn print_report(report: &SweepReport) {
    let total = report.steps.len();
    for (index, step) in report.steps.iter().enumerate() {
        let position = format!("{}/{}", index + 1, total);
        match &step.outcome {
            sigproc_sweep::StepOutcome::Applied {
                applied,
                observation,
                observation_error,
            } => {
                let observation = match observation {
                    Some(o) => format!(
                        "power {} dBFS, peak {} Hz, packets {}, gaps {}",
                        o.output_power_dbfs
                            .map_or("-".to_string(), |v| v.to_string()),
                        o.peak_offset_hz.map_or("-".to_string(), |v| v.to_string()),
                        o.packets_received,
                        o.seq_gaps
                    ),
                    None => format!(
                        "observation unavailable ({})",
                        observation_error.as_deref().unwrap_or("no reason given")
                    ),
                };
                println!(
                    "step {position} target {} Hz offset {} Hz -> applied offset {} Hz rate {} Hz \
                     settling {} samples | {observation}",
                    step.target_hz,
                    step.offset_hz,
                    applied.offset_hz,
                    applied.output_rate_hz,
                    applied.settling_samples
                );
            }
            sigproc_sweep::StepOutcome::Refused { error } => println!(
                "step {position} target {} Hz offset {} Hz -> REFUSED: {error}",
                step.target_hz, step.offset_hz
            ),
            sigproc_sweep::StepOutcome::Unsent { error } => println!(
                "step {position} target {} Hz offset {} Hz -> NOT SENT: {error}",
                step.target_hz, step.offset_hz
            ),
            sigproc_sweep::StepOutcome::Uncertain { error } => println!(
                "step {position} target {} Hz offset {} Hz -> UNCERTAIN (not retried): {error}",
                step.target_hz, step.offset_hz
            ),
        }
    }

    println!(
        "stop: {} — {} steps applied with matching readback of {} recorded",
        describe_stop(&report.stop),
        report.applied_matching(),
        total
    );
    if let Some(original) = report.original {
        println!(
            "original channel: offset {} Hz rate {} Hz",
            original.offset_hz, original.output_rate_hz
        );
    }
    println!("restore: {}", describe_restore(&report.restore));
    println!("rf: {}", describe_rf(&report.rf));
    if let Some(detail) = &report.detail {
        println!("detail: {detail}");
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}

fn describe_stop(stop: &StopReason) -> String {
    match stop {
        StopReason::Completed => "completed".to_string(),
        StopReason::StepBudget { reached } => format!("step budget of {reached} reached"),
        StopReason::Deadline => "deadline reached".to_string(),
        StopReason::Interrupted => "interrupted by operator".to_string(),
        StopReason::Refused => "a step was refused".to_string(),
        StopReason::Unsent => "a step could not be delivered".to_string(),
        StopReason::Uncertain => "a step is uncertain".to_string(),
        StopReason::PreflightFailed => "preflight refused to sweep".to_string(),
    }
}

fn describe_restore(restore: &RestoreOutcome) -> String {
    match restore {
        RestoreOutcome::Restored => "original channel read back as applied".to_string(),
        RestoreOutcome::NothingToRestore => "nothing was mutated".to_string(),
        RestoreOutcome::Failed { error } => format!("FAILED: {error}"),
    }
}

fn describe_rf(rf: &RfHandling) -> String {
    match rf {
        RfHandling::NotUsed => "not consulted (no capture C2 configured)".to_string(),
        RfHandling::AlreadySufficient { state } => format!(
            "already covered the plan at {} Hz / {} Hz, left alone",
            state.rf_freq_hz, state.sample_rate_hz
        ),
        RfHandling::AppliedThenRestored {
            original,
            applied,
            restore,
        } => format!(
            "moved to {} Hz / {} Hz, then {} to {} Hz / {} Hz",
            applied.rf_freq_hz,
            applied.sample_rate_hz,
            describe_restore(restore),
            original.rf_freq_hz,
            original.sample_rate_hz
        ),
        RfHandling::Refused { reason, .. } => format!("refused: {reason}"),
    }
}

fn exit_code(report: &SweepReport) -> i32 {
    match report.stop {
        StopReason::Completed | StopReason::StepBudget { .. } | StopReason::Deadline => {}
        StopReason::Interrupted => return EXIT_INTERRUPTED,
        StopReason::Refused | StopReason::Unsent | StopReason::Uncertain => {
            return EXIT_CONTROL_FAILED
        }
        StopReason::PreflightFailed => return EXIT_REFUSED,
    }
    match report.restore {
        RestoreOutcome::Failed { .. } => EXIT_RESTORE_FAILED,
        _ => EXIT_OK,
    }
}
