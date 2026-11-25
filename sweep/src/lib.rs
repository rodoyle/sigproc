//! Bounded 140-150 MHz NBFM sweep controller.
//!
//! The approved design (`docs/plans/service-c2-and-networking.md`, sequencing
//! step 4) fixes **one** wideband RF capture across 140-150 MHz and moves a
//! narrowband NBFM slice through that band *digitally*, so a sweep costs a
//! channelizer retune per step and never a B210 stop/start. This crate is the
//! controller for that motion.
//!
//! Three properties are load-bearing and are the reason this is a library with
//! hardware-free tests rather than a script:
//!
//! * **Bounded.** The plan is validated against the licensed window before any
//!   mutation. A sweep that would step outside 140-150 MHz is a plan error, not
//!   a runtime surprise halfway through a band.
//! * **Applied readback, never an assumed setter result.** Every step records
//!   what the service says it applied, and refuses to treat a request that was
//!   sent as a setting that took effect.
//! * **No retry of an uncertain execute.** A mutation whose reply was lost may
//!   or may not have been applied. This controller records it as uncertain,
//!   stops, and restores — it never sends it twice. [`ControlError`] encodes
//!   that distinction at the type level so a caller cannot silently collapse
//!   "definitely not applied" into "unknown".
//!
//! On every exit path — completion, budget, deadline, operator interrupt, a
//! refused or uncertain step — the original channel is restored exactly once and
//! the outcome of that restore is reported. The original state is also written to
//! a recovery file *before* the first mutation, so a hard kill that bypasses
//! unwinding can still be repaired with `--restore-from`.
//!
//! Frequencies are integer hertz throughout. The channelizer's C2 wire carries
//! floats for its DSP types, and this crate accepts that at the boundary only:
//! a non-integral readback is reported as uncertain rather than rounded, because
//! a value we cannot represent is a value we cannot verify.

pub mod http;

use std::{
    fmt,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Lowest frequency the controller will sweep, in hertz.
pub const SWEEP_START_HZ: u64 = 140_000_000;
/// Highest frequency the controller will sweep, in hertz.
pub const SWEEP_STOP_HZ: u64 = 150_000_000;
/// One NBFM channel: the step, the channel width, and the sweep granularity.
pub const NBFM_STEP_HZ: u64 = 12_500;
/// Half an NBFM channel: the inset from a band edge to the first channel centre.
///
/// A channel *centred* on 140.000 MHz reaches down to 139.99375 MHz, which is
/// outside a capture whose band starts at 140.000 MHz. Aligning centres to the
/// band edges by half a channel makes the 800 channels tile 140-150 MHz exactly,
/// with every channel fully sampled.
pub const NBFM_HALF_STEP_HZ: u64 = NBFM_STEP_HZ / 2;
/// Wideband rate the band plan assumes: 10 MS/s holds 140-150 MHz at 145 MHz centre.
pub const DEFAULT_WIDEBAND_RATE_HZ: u64 = 10_000_000;
/// Centre of the fixed wideband capture the sweep moves through.
pub const DEFAULT_RF_CENTER_HZ: u64 = 145_000_000;

// ── Plan ────────────────────────────────────────────────────────────────────

/// A bounded, validated sweep plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SweepPlan {
    pub start_hz: u64,
    pub stop_hz: u64,
    pub step_hz: u64,
}

/// Why a plan was refused. Every variant is a mistake worth stopping for before
/// any hardware or channel moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// A zero step would never terminate.
    EmptyStep,
    /// The range is backwards.
    Inverted { start_hz: u64, stop_hz: u64 },
    /// The range leaves the licensed 140-150 MHz window.
    OutsideBand { start_hz: u64, stop_hz: u64 },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyStep => write!(f, "step_hz must be non-zero"),
            Self::Inverted { start_hz, stop_hz } => write!(
                f,
                "stop_hz ({stop_hz}) is below start_hz ({start_hz}); a sweep cannot run backwards"
            ),
            Self::OutsideBand { start_hz, stop_hz } => write!(
                f,
                "range {start_hz}..{stop_hz} Hz leaves the licensed window \
                 {SWEEP_START_HZ}..{SWEEP_STOP_HZ} Hz"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

impl SweepPlan {
    /// The operational plan: the whole 140-150 MHz band in 12.5 kHz NBFM steps.
    pub fn nbfm_band() -> Self {
        Self {
            // Centres inset by half a channel so the 800 channels tile the band
            // edge-to-edge and none is clipped by the capture boundary.
            start_hz: SWEEP_START_HZ + NBFM_HALF_STEP_HZ,
            stop_hz: SWEEP_STOP_HZ - NBFM_HALF_STEP_HZ,
            step_hz: NBFM_STEP_HZ,
        }
    }

    /// Validate a plan against the licensed window.
    pub fn new(start_hz: u64, stop_hz: u64, step_hz: u64) -> Result<Self, PlanError> {
        if step_hz == 0 {
            return Err(PlanError::EmptyStep);
        }
        if stop_hz < start_hz {
            return Err(PlanError::Inverted { start_hz, stop_hz });
        }
        if start_hz < SWEEP_START_HZ || stop_hz > SWEEP_STOP_HZ {
            return Err(PlanError::OutsideBand { start_hz, stop_hz });
        }
        Ok(Self {
            start_hz,
            stop_hz,
            step_hz,
        })
    }

    /// Every dwell point, inclusive of both endpoints.
    ///
    /// Inclusive because the objective is a bounded sweep: 140.000 MHz and
    /// 150.000 MHz are channels an operator expects to be visited, not fences
    /// the sweep stops short of.
    pub fn steps(&self) -> Vec<u64> {
        let mut steps = Vec::new();
        let mut hz = self.start_hz;
        while hz <= self.stop_hz {
            steps.push(hz);
            match hz.checked_add(self.step_hz) {
                Some(next) => hz = next,
                None => break,
            }
        }
        steps
    }

    /// Channel offsets in hertz relative to the fixed wideband centre.
    ///
    /// Integer arithmetic on purpose: `145_000_000 - 140_000_000` is exactly
    /// `5_000_000`, and staying in integers means a step can never drift by a
    /// rounding error as the sweep walks the band.
    pub fn offsets_hz(&self, rf_center_hz: u64) -> Vec<i64> {
        self.steps()
            .into_iter()
            .map(|hz| hz as i64 - rf_center_hz as i64)
            .collect()
    }

    /// The widest excursion this plan needs from a capture centred on
    /// `rf_center_hz`, in hertz. Used for the coverage preflight.
    pub fn required_half_width_hz(&self, rf_center_hz: u64) -> u64 {
        let start = rf_center_hz.abs_diff(self.start_hz);
        let stop = rf_center_hz.abs_diff(self.stop_hz);
        start.max(stop)
    }
}

// ── Control surface ─────────────────────────────────────────────────────────

/// Applied channelizer state, in integer hertz.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChannelState {
    pub offset_hz: i64,
    pub output_rate_hz: u64,
}

/// What the channelizer reported after a retune.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChannelApplied {
    pub offset_hz: i64,
    pub output_rate_hz: u64,
    pub settling_samples: u64,
}

/// A non-mutating observation taken while the channel is settled.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Observation {
    pub output_power_dbfs: Option<f32>,
    pub peak_offset_hz: Option<f32>,
    pub packets_received: u64,
    pub seq_gaps: u64,
}

/// Applied RF state of the capture service, in integer hertz.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RfState {
    pub rf_freq_hz: u64,
    pub sample_rate_hz: u64,
}

/// A control-plane outcome, split by *what is known about the application*.
///
/// This split is the whole "never retry an uncertain execute" rule made
/// mechanical: only one of these three variants is safe to send again, and this
/// controller sends none of them twice mid-sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// The request never reached the service: connect failed, or the write was
    /// refused before the service could read it. The setting cannot have moved.
    NotSent(String),
    /// The request left this process and the reply was lost, unreadable, or
    /// carried a value that cannot be represented in integer hertz. It may or
    /// may not have been applied; the controller must not assume either.
    Uncertain(String),
    /// The service answered and refused. The applied state is known and unchanged.
    Rejected(String),
}

impl ControlError {
    pub fn detail(&self) -> &str {
        match self {
            Self::NotSent(d) | Self::Uncertain(d) | Self::Rejected(d) => d,
        }
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(d) => write!(f, "not sent: {d}"),
            Self::Uncertain(d) => write!(f, "uncertain: {d}"),
            Self::Rejected(d) => write!(f, "rejected: {d}"),
        }
    }
}

/// The channelizer control plane the sweep drives.
pub trait ChannelControl {
    /// Current applied channel state. Read-only, never mutates.
    fn channel_state(&mut self) -> Result<ChannelState, ControlError>;
    /// Apply an offset/rate and return the service's applied readback.
    fn apply_channel(
        &mut self,
        offset_hz: i64,
        output_rate_hz: u64,
    ) -> Result<ChannelApplied, ControlError>;
    /// Observe the settled channel without changing it.
    fn observe(&mut self) -> Result<Observation, ControlError>;
}

/// The capture control plane, used only to verify (and optionally re-apply) the
/// fixed wideband RF the sweep moves through.
pub trait CaptureControl {
    fn rf_state(&mut self) -> Result<RfState, ControlError>;
    fn apply_rf(&mut self, target: RfState) -> Result<RfState, ControlError>;
}

/// Injectable time, so the sweep's dwell and deadline logic is testable without
/// sleeping.
pub trait Clock {
    fn sleep_ms(&mut self, ms: u64);
    /// Milliseconds since the controller was built.
    fn elapsed_ms(&mut self) -> u64;
}

/// Wall-clock implementation used by the binary.
pub struct StdClock {
    start: Instant,
}

impl Default for StdClock {
    fn default() -> Self {
        Self::new()
    }
}

impl StdClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Clock for StdClock {
    fn sleep_ms(&mut self, ms: u64) {
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
    fn elapsed_ms(&mut self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

// ── Run configuration and results ───────────────────────────────────────────

/// Everything a single sweep run needs.
#[derive(Debug, Clone)]
pub struct SweepConfig {
    pub plan: SweepPlan,
    /// Centre of the fixed wideband capture the channel offsets are relative to.
    pub rf_center_hz: u64,
    /// Channel rate to request: one 12.5 kHz NBFM channel by default.
    pub channel_rate_hz: u64,
    /// Dwell after each retune before observing, so measurements describe the
    /// settled channel rather than the filter's transient.
    pub settle_ms: u64,
    /// Take a read-only observation after each settled step.
    pub observe: bool,
    /// Stop after this many steps (a bounded smoke run).
    pub max_steps: Option<u64>,
    /// Stop after this much elapsed time.
    pub deadline_ms: Option<u64>,
    /// Wideband RF to apply when coverage is insufficient *and* `apply_rf` is
    /// set. `None` means "derive from plan + centre".
    pub rf_target: Option<RfState>,
    /// Whether this run is allowed to move the RF front end at all.
    pub apply_rf: bool,
    /// Wideband rate the plan is expected to live in. Used for the channel-fit
    /// check when no capture control plane is configured; the measured RF rate
    /// replaces it whenever one is.
    pub wideband_rate_hz: u64,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            plan: SweepPlan::nbfm_band(),
            rf_center_hz: DEFAULT_RF_CENTER_HZ,
            channel_rate_hz: NBFM_STEP_HZ,
            settle_ms: 120,
            observe: true,
            max_steps: None,
            deadline_ms: None,
            rf_target: None,
            apply_rf: false,
            wideband_rate_hz: DEFAULT_WIDEBAND_RATE_HZ,
        }
    }
}

impl SweepConfig {
    /// Wideband state that would cover the whole plan.
    pub fn required_rf(&self) -> RfState {
        self.rf_target.unwrap_or(RfState {
            rf_freq_hz: self.rf_center_hz,
            sample_rate_hz: self.plan.required_half_width_hz(self.rf_center_hz) * 2,
        })
    }
}

/// Outcome of one dwell point.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StepOutcome {
    /// The service reported the retune applied; this carries its readback.
    Applied {
        applied: ChannelApplied,
        observation: Option<Observation>,
        observation_error: Option<String>,
    },
    /// The service answered and refused; the channel did not move.
    Refused { error: String },
    /// The request never reached the service.
    Unsent { error: String },
    /// The request may or may not have been applied. Never retried.
    Uncertain { error: String },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StepRecord {
    pub target_hz: u64,
    pub offset_hz: i64,
    #[serde(flatten)]
    pub outcome: StepOutcome,
}

/// Why the sweep stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StopReason {
    /// Every dwell point was visited.
    Completed,
    /// The configured step budget was reached.
    StepBudget { reached: u64 },
    /// The configured deadline was reached.
    Deadline,
    /// The operator asked it to stop.
    Interrupted,
    /// A step was refused by the service.
    Refused,
    /// A step's request could not be delivered.
    Unsent,
    /// A step's outcome is unknown. This is the one that must never be retried.
    Uncertain,
    /// Nothing was swept: the preflight could not establish a safe starting
    /// point, so no mutation was attempted.
    PreflightFailed,
}

/// What happened to a value we had to put back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "restore", rename_all = "snake_case")]
pub enum RestoreOutcome {
    /// The original value was applied and read back.
    Restored,
    /// No mutation was attempted, so there was nothing to undo.
    NothingToRestore,
    /// The restore itself failed. The recovery file still holds the original.
    Failed { error: String },
}

/// How the RF front end was handled.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "rf", rename_all = "snake_case")]
pub enum RfHandling {
    /// No capture control plane was configured; the RF was not consulted.
    NotUsed,
    /// The RF already covered the plan and was left alone.
    AlreadySufficient { state: RfState },
    /// The RF was moved for the sweep and put back afterwards.
    AppliedThenRestored {
        original: RfState,
        applied: RfState,
        restore: RestoreOutcome,
    },
    /// The RF could not cover the plan and this run was not allowed to move it.
    Refused {
        reason: String,
        state: Option<RfState>,
    },
}

/// The full record of a sweep run: per-step readback and observations, why it
/// stopped, and what was restored.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SweepReport {
    pub steps: Vec<StepRecord>,
    pub stop: StopReason,
    /// Applied channel state before the sweep, when it could be read.
    pub original: Option<ChannelState>,
    pub restore: RestoreOutcome,
    pub rf: RfHandling,
    /// Set when the preflight refused to sweep.
    pub detail: Option<String>,
    /// Non-fatal findings: things an operator should know that did not stop the
    /// sweep (for example a channel that reaches past the captured band).
    pub warnings: Vec<String>,
}

impl SweepReport {
    fn new() -> Self {
        Self {
            steps: Vec::new(),
            stop: StopReason::PreflightFailed,
            original: None,
            restore: RestoreOutcome::NothingToRestore,
            rf: RfHandling::NotUsed,
            detail: None,
            warnings: Vec::new(),
        }
    }

    /// Steps whose applied readback matched what was requested. The number an
    /// operator reads as "channels confirmed", as opposed to "requests sent".
    pub fn applied_matching(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| match &s.outcome {
                StepOutcome::Applied { applied, .. } => applied.offset_hz == s.offset_hz,
                _ => false,
            })
            .count()
    }

    /// Whether any step could have changed the service, which decides if a
    /// restore is owed.
    fn mutations_possible(&self) -> bool {
        self.steps.iter().any(|s| {
            matches!(
                s.outcome,
                StepOutcome::Applied { .. } | StepOutcome::Uncertain { .. }
            )
        })
    }
}

// ── Recovery file ───────────────────────────────────────────────────────────

/// The state written before the first mutation, so a hard kill that bypasses
/// unwinding can still be repaired.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecoveryState {
    pub channel: ChannelState,
    pub rf: Option<RfState>,
    pub written_unix_ms: u64,
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn write_recovery_file(path: &Path, state: &RecoveryState) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut file = std::fs::File::create(path)?;
    file.write_all(&body)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

pub fn read_recovery_file(path: &Path) -> std::io::Result<RecoveryState> {
    let body = std::fs::read(path)?;
    serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// ── Exclusive controller lock ───────────────────────────────────────────────

/// An exclusive, cross-platform controller lock.
///
/// Two sweep controllers, or a sweep controller and an interactive retune, must
/// not drive the same channelizer at once: the second one's idea of "the
/// original state" would be the first one's mid-sweep state, and the restore
/// would put the channel somewhere nobody chose. `create_new` gives that
/// exclusivity without a platform-specific file-lock dependency.
#[derive(Debug)]
pub struct ControllerLock {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// Another controller holds the lock. The payload is its recorded holder.
    Held { holder: String },
    /// The lock file could not be created or inspected.
    Io(String),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Held { holder } => write!(f, "another controller holds the lock: {holder}"),
            Self::Io(d) => write!(f, "{d}"),
        }
    }
}

impl std::error::Error for LockError {}

impl ControllerLock {
    /// Take the lock, or fail because someone else holds it.
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self, LockError> {
        let path = path.into();
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let holder = format!(
                    "{{\"pid\":{},\"acquired_unix_ms\":{}}}",
                    std::process::id(),
                    now_unix_ms()
                );
                let _ = file.write_all(holder.as_bytes());
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path)
                    .unwrap_or_else(|_| "<unreadable>".to_string())
                    .trim()
                    .to_string();
                Err(LockError::Held { holder })
            }
            Err(e) => Err(LockError::Io(format!("{}: {e}", path.display()))),
        }
    }

    /// Break a lock whose holder is presumed dead.
    ///
    /// Deliberately explicit and age-bounded: a stale lock is broken only when
    /// the operator says so *and* the file is older than `stale_after_ms`, so a
    /// live sweep cannot be clobbered by an impatient second invocation.
    pub fn break_stale(path: impl Into<PathBuf>, stale_after_ms: u64) -> Result<Self, LockError> {
        let path = path.into();
        if let Ok(meta) = std::fs::metadata(&path) {
            let age_ms = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if age_ms < stale_after_ms {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                return Err(LockError::Held {
                    holder: format!(
                        "{} (only {age_ms} ms old, under the {stale_after_ms} ms stale threshold)",
                        holder.trim()
                    ),
                });
            }
            std::fs::remove_file(&path)
                .map_err(|e| LockError::Io(format!("{}: {e}", path.display())))?;
        }
        Self::acquire(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ControllerLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ── Controller ──────────────────────────────────────────────────────────────

/// Drives the channel through the plan, recording applied readback, and restores
/// the original channel on every exit path.
pub struct SweepController {
    channel: Box<dyn ChannelControl>,
    capture: Option<Box<dyn CaptureControl>>,
    clock: Box<dyn Clock>,
    stop: Arc<AtomicBool>,
    recovery_path: Option<PathBuf>,
}

impl SweepController {
    pub fn new(
        channel: Box<dyn ChannelControl>,
        clock: Box<dyn Clock>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            channel,
            capture: None,
            clock,
            stop,
            recovery_path: None,
        }
    }

    /// Attach the capture control plane used to verify (and optionally move) the
    /// fixed wideband RF.
    pub fn with_capture(mut self, capture: Box<dyn CaptureControl>) -> Self {
        self.capture = Some(capture);
        self
    }

    /// Persist the original state here before the first mutation.
    pub fn with_recovery_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.recovery_path = Some(path.into());
        self
    }

    /// Run the sweep to completion, budget, deadline, interrupt, or first bad step.
    pub fn run(&mut self, cfg: &SweepConfig) -> SweepReport {
        let mut report = SweepReport::new();

        // Preflight: establish the restore target *before* anything moves. If we
        // cannot read the current channel we must not start, because there would
        // be nothing to restore to.
        let original = match self.channel.channel_state() {
            Ok(state) => state,
            Err(e) => {
                report.detail = Some(format!(
                    "refusing to sweep: cannot read the applied channel state, so there \
                     would be nothing to restore ({e})"
                ));
                return report;
            }
        };
        report.original = Some(original);

        // Preflight: the fixed wideband capture has to cover the plan.
        let rf_original: Option<RfState> = match self.preflight_rf(cfg, &mut report) {
            Ok(applied_original) => applied_original,
            Err(()) => return report,
        };

        // Channel-fit check: a channel centred too close to a band edge reaches
        // outside the captured wideband and would come back clipped. Recorded
        // rather than fatal — an operator may want the edge centres — but never
        // silent.
        let effective_wideband = match &report.rf {
            RfHandling::AlreadySufficient { state } => state.sample_rate_hz,
            RfHandling::AppliedThenRestored { applied, .. } => applied.sample_rate_hz,
            _ => cfg.wideband_rate_hz,
        };
        report.warnings = channel_fit_warnings(cfg, effective_wideband);

        // The recovery file exists before the first mutation, so a hard kill is
        // still repairable.
        if let Some(path) = &self.recovery_path {
            let state = RecoveryState {
                channel: original,
                rf: rf_original,
                written_unix_ms: now_unix_ms(),
            };
            if let Err(e) = write_recovery_file(path, &state) {
                report.detail = Some(format!(
                    "refusing to sweep: cannot write the recovery file {} ({e})",
                    path.display()
                ));
                // The RF may already have moved for the sweep; put it back.
                if let Some(rf) = rf_original {
                    let restore = self.restore_rf(&report, rf);
                    report.rf = RfHandling::AppliedThenRestored {
                        original: rf,
                        applied: cfg.required_rf(),
                        restore,
                    };
                }
                return report;
            }
        }

        let steps = cfg.plan.steps();
        let mut stop = StopReason::Completed;
        for (index, target_hz) in steps.iter().enumerate() {
            if self.stop.load(Ordering::Relaxed) {
                stop = StopReason::Interrupted;
                break;
            }
            if let Some(max) = cfg.max_steps {
                if index as u64 >= max {
                    stop = StopReason::StepBudget { reached: max };
                    break;
                }
            }
            if let Some(deadline) = cfg.deadline_ms {
                if self.clock.elapsed_ms() >= deadline {
                    stop = StopReason::Deadline;
                    break;
                }
            }

            let offset_hz = *target_hz as i64 - cfg.rf_center_hz as i64;
            match self.channel.apply_channel(offset_hz, cfg.channel_rate_hz) {
                Ok(applied) => {
                    self.clock.sleep_ms(cfg.settle_ms);
                    let (observation, observation_error) = if cfg.observe {
                        match self.channel.observe() {
                            Ok(o) => (Some(o), None),
                            // A failed read is not a failed mutation: the step
                            // stands, and the gap is recorded rather than
                            // retried or hidden.
                            Err(e) => (None, Some(e.to_string())),
                        }
                    } else {
                        (None, None)
                    };
                    report.steps.push(StepRecord {
                        target_hz: *target_hz,
                        offset_hz,
                        outcome: StepOutcome::Applied {
                            applied,
                            observation,
                            observation_error,
                        },
                    });
                }
                Err(ControlError::Rejected(error)) => {
                    report.steps.push(StepRecord {
                        target_hz: *target_hz,
                        offset_hz,
                        outcome: StepOutcome::Refused { error },
                    });
                    stop = StopReason::Refused;
                    break;
                }
                Err(ControlError::NotSent(error)) => {
                    report.steps.push(StepRecord {
                        target_hz: *target_hz,
                        offset_hz,
                        outcome: StepOutcome::Unsent { error },
                    });
                    stop = StopReason::Unsent;
                    break;
                }
                Err(ControlError::Uncertain(error)) => {
                    report.steps.push(StepRecord {
                        target_hz: *target_hz,
                        offset_hz,
                        outcome: StepOutcome::Uncertain { error },
                    });
                    // No retry, ever: the request may already have applied.
                    stop = StopReason::Uncertain;
                    break;
                }
            }
        }
        report.stop = stop;

        // Restore, on every path, exactly once.
        report.restore = self.restore_channel(&report, original);
        if let Some(rf) = rf_original {
            let restore = self.restore_rf(&report, rf);
            report.rf = RfHandling::AppliedThenRestored {
                original: rf,
                applied: cfg.required_rf(),
                restore,
            };
        }
        report
    }

    /// Verify the fixed wideband covers the plan, applying it only when this run
    /// is allowed to and it does not.
    ///
    /// Returns `Ok(Some(original))` when the RF was moved (so the caller knows to
    /// restore it), `Ok(None)` when it was sufficient or unused, and `Err(())`
    /// when the preflight refuses to sweep.
    fn preflight_rf(
        &mut self,
        cfg: &SweepConfig,
        report: &mut SweepReport,
    ) -> Result<Option<RfState>, ()> {
        let Some(capture) = self.capture.as_mut() else {
            report.rf = RfHandling::NotUsed;
            return Ok(None);
        };
        let required = cfg.required_rf();
        let state = match capture.rf_state() {
            Ok(state) => state,
            Err(e) => {
                let reason = format!(
                    "refusing to sweep: cannot read the applied RF state to confirm the \
                     fixed wideband covers the plan ({e})"
                );
                report.rf = RfHandling::Refused {
                    reason: reason.clone(),
                    state: None,
                };
                report.detail = Some(reason);
                return Err(());
            }
        };
        let (low, high) = coverage_edges(state);
        let needed_low = cfg.rf_center_hz.saturating_sub(required.sample_rate_hz / 2);
        let needed_high = cfg.rf_center_hz.saturating_add(required.sample_rate_hz / 2);
        let covered = low <= cfg.plan.start_hz && high >= cfg.plan.stop_hz;
        if covered {
            report.rf = RfHandling::AlreadySufficient { state };
            return Ok(None);
        }
        if !cfg.apply_rf {
            let reason = format!(
                "refusing to sweep: the applied RF {} Hz +/- {} Hz covers {low}..{high} Hz, \
                 which does not reach the plan's {}..{} Hz, and this run is not allowed to \
                 move the RF front end (needs {needed_low}..{needed_high} Hz)",
                state.rf_freq_hz,
                state.sample_rate_hz / 2,
                cfg.plan.start_hz,
                cfg.plan.stop_hz
            );
            report.rf = RfHandling::Refused {
                reason: reason.clone(),
                state: Some(state),
            };
            report.detail = Some(reason);
            return Err(());
        }
        match capture.apply_rf(required) {
            Ok(applied) => {
                if applied != required {
                    let reason = format!(
                        "refusing to sweep: asked the capture service for {} Hz / {} Hz and it \
                         read back {} Hz / {} Hz",
                        required.rf_freq_hz,
                        required.sample_rate_hz,
                        applied.rf_freq_hz,
                        applied.sample_rate_hz
                    );
                    report.rf = RfHandling::Refused {
                        reason: reason.clone(),
                        state: Some(applied),
                    };
                    report.detail = Some(reason);
                    // The RF did move (we just cannot confirm it is what we
                    // asked for), so the caller still owes it a restore.
                    return Err(());
                }
                Ok(Some(state))
            }
            Err(e) => {
                let reason = format!("refusing to sweep: could not apply the wideband RF ({e})");
                report.rf = RfHandling::Refused {
                    reason: reason.clone(),
                    state: Some(state),
                };
                report.detail = Some(reason);
                Err(())
            }
        }
    }

    fn restore_channel(&mut self, report: &SweepReport, original: ChannelState) -> RestoreOutcome {
        if !report.mutations_possible() {
            return RestoreOutcome::NothingToRestore;
        }
        match self
            .channel
            .apply_channel(original.offset_hz, original.output_rate_hz)
        {
            Ok(applied)
                if applied.offset_hz == original.offset_hz
                    && applied.output_rate_hz == original.output_rate_hz =>
            {
                RestoreOutcome::Restored
            }
            Ok(applied) => RestoreOutcome::Failed {
                error: format!(
                    "restore read back offset {} Hz / rate {} Hz, wanted {} Hz / {} Hz",
                    applied.offset_hz,
                    applied.output_rate_hz,
                    original.offset_hz,
                    original.output_rate_hz
                ),
            },
            Err(e) => RestoreOutcome::Failed {
                error: e.to_string(),
            },
        }
    }

    fn restore_rf(&mut self, report: &SweepReport, original: RfState) -> RestoreOutcome {
        if report.original.is_none() && report.detail.is_some() {
            // The sweep never started far enough to have moved anything.
            return RestoreOutcome::NothingToRestore;
        }
        let Some(capture) = self.capture.as_mut() else {
            return RestoreOutcome::NothingToRestore;
        };
        match capture.apply_rf(original) {
            Ok(applied) if applied == original => RestoreOutcome::Restored,
            Ok(applied) => RestoreOutcome::Failed {
                error: format!(
                    "RF restore read back {} Hz / {} Hz, wanted {} Hz / {} Hz",
                    applied.rf_freq_hz,
                    applied.sample_rate_hz,
                    original.rf_freq_hz,
                    original.sample_rate_hz
                ),
            },
            Err(e) => RestoreOutcome::Failed {
                error: e.to_string(),
            },
        }
    }
}

/// The band an applied RF state actually covers, in hertz.
fn coverage_edges(state: RfState) -> (u64, u64) {
    let half = state.sample_rate_hz / 2;
    (
        state.rf_freq_hz.saturating_sub(half),
        state.rf_freq_hz.saturating_add(half),
    )
}

/// Channels at the ends of the plan that reach past the captured wideband.
///
/// A channel of width `w` centred `w/2` from the band edge is fully sampled;
/// centred on the edge itself, half of it lies outside and comes back aliased.
/// This reports that instead of letting it become a quiet measurement artefact.
fn channel_fit_warnings(cfg: &SweepConfig, wideband_rate_hz: u64) -> Vec<String> {
    let half_rate = wideband_rate_hz / 2;
    let channel_half = cfg.channel_rate_hz / 2;
    let steps = cfg.plan.steps();
    let (Some(first), Some(last)) = (steps.first(), steps.last()) else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    let mut seen: Vec<u64> = Vec::new();
    for target_hz in [*first, *last] {
        if seen.contains(&target_hz) {
            continue;
        }
        seen.push(target_hz);
        let offset_hz = target_hz as i64 - cfg.rf_center_hz as i64;
        let reach_hz = offset_hz.unsigned_abs() + channel_half;
        if reach_hz > half_rate {
            warnings.push(format!(
                "channel at {target_hz} Hz (offset {offset_hz} Hz) reaches {reach_hz} Hz from \
                 centre, past the {half_rate} Hz half-band of a {wideband_rate_hz} Hz capture, \
                 so it is clipped; channel-edge-aligned centres ({}..{} Hz) fit exactly",
                cfg.plan.start_hz + channel_half,
                cfg.plan.stop_hz.saturating_sub(channel_half)
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod tests;
