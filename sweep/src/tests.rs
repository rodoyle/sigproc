//! Hardware-free tests for the sweep controller.
//!
//! Every test drives the real controller code through the same trait the HTTP
//! client implements; only the wire is faked. The properties under test are the
//! ones the objective makes acceptance criteria: both endpoints are visited, bad
//! geometry is refused before any mutation, a refused/undelivered/uncertain step
//! stops the sweep without retrying, an interruption stops it cleanly, and the
//! original channel is restored on every path.

use super::*;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

// ── Fakes ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeChannel {
    applied: ChannelState,
    /// Every `apply_channel` call in order: (offset_hz, output_rate_hz).
    attempts: Vec<(i64, u64)>,
    /// Scripted per-call outcome, consumed in order. An empty queue echoes the
    /// request back as applied.
    script: VecDeque<Result<ChannelApplied, ControlError>>,
    /// Fail exactly this call index.
    fail_at: Option<(usize, ControlError)>,
    /// Fail this call index and every later one (used to break the restore).
    fail_from: Option<(usize, ControlError)>,
    /// Report an offset this far from the requested one (a service that lies).
    lie_offset_by: i64,
    observe_error: Option<ControlError>,
    state_error: Option<ControlError>,
    /// Called after each successful apply with the call index.
    on_success: Option<Arc<dyn Fn(usize) + Send + Sync>>,
}

impl ChannelControl for FakeChannel {
    fn channel_state(&mut self) -> Result<ChannelState, ControlError> {
        match &self.state_error {
            Some(e) => Err(e.clone()),
            None => Ok(self.applied),
        }
    }

    fn apply_channel(
        &mut self,
        offset_hz: i64,
        output_rate_hz: u64,
    ) -> Result<ChannelApplied, ControlError> {
        let index = self.attempts.len();
        self.attempts.push((offset_hz, output_rate_hz));

        if let Some((at, err)) = &self.fail_at {
            if index == *at {
                return Err(err.clone());
            }
        }
        if let Some((from, err)) = &self.fail_from {
            if index >= *from {
                return Err(err.clone());
            }
        }
        let applied = match self.script.pop_front() {
            Some(Ok(applied)) => applied,
            Some(Err(err)) => return Err(err),
            None => ChannelApplied {
                offset_hz: offset_hz + self.lie_offset_by,
                output_rate_hz,
                settling_samples: 32,
            },
        };
        self.applied = ChannelState {
            offset_hz: applied.offset_hz,
            output_rate_hz: applied.output_rate_hz,
        };
        if let Some(cb) = self.on_success.clone() {
            cb(index);
        }
        Ok(applied)
    }

    fn observe(&mut self) -> Result<Observation, ControlError> {
        match &self.observe_error {
            Some(e) => Err(e.clone()),
            None => Ok(Observation {
                output_power_dbfs: Some(-42.5),
                peak_offset_hz: Some(125.0),
                packets_received: 1_000,
                seq_gaps: 2,
            }),
        }
    }
}

/// Handle to a fake channel that outlives the controller that owns the trait
/// object, so a test can assert on what the controller actually did.
#[derive(Clone)]
struct FakeHandle {
    inner: Arc<Mutex<FakeChannel>>,
}

impl FakeHandle {
    fn new(applied: ChannelState) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeChannel {
                applied,
                ..Default::default()
            })),
        }
    }

    fn configure(&self, f: impl FnOnce(&mut FakeChannel)) {
        let mut guard = self.inner.lock().unwrap();
        f(&mut guard);
    }

    fn attempts(&self) -> Vec<(i64, u64)> {
        self.inner.lock().unwrap().attempts.clone()
    }

    fn applied(&self) -> ChannelState {
        self.inner.lock().unwrap().applied
    }

    fn channel(&self) -> Box<dyn ChannelControl> {
        Box::new(self.clone())
    }
}

impl ChannelControl for FakeHandle {
    fn channel_state(&mut self) -> Result<ChannelState, ControlError> {
        self.inner.lock().unwrap().channel_state()
    }
    fn apply_channel(
        &mut self,
        offset_hz: i64,
        output_rate_hz: u64,
    ) -> Result<ChannelApplied, ControlError> {
        self.inner
            .lock()
            .unwrap()
            .apply_channel(offset_hz, output_rate_hz)
    }
    fn observe(&mut self) -> Result<Observation, ControlError> {
        self.inner.lock().unwrap().observe()
    }
}

#[derive(Default)]
struct FakeClock {
    slept: Vec<u64>,
    elapsed_ms: u64,
}

impl FakeClock {
    fn handle() -> (Self, Arc<Mutex<FakeClock>>) {
        let shared = Arc::new(Mutex::new(FakeClock::default()));
        (Self::default(), shared)
    }
}

struct SharedClock(Arc<Mutex<FakeClock>>);

impl Clock for SharedClock {
    fn sleep_ms(&mut self, ms: u64) {
        let mut guard = self.0.lock().unwrap();
        guard.slept.push(ms);
        guard.elapsed_ms += ms;
    }
    fn elapsed_ms(&mut self) -> u64 {
        self.0.lock().unwrap().elapsed_ms
    }
}

#[derive(Default)]
struct FakeCapture {
    state: RfState,
    applies: Vec<RfState>,
    state_error: Option<ControlError>,
    apply_error: Option<ControlError>,
    /// Read back this instead of what was requested.
    readback: Option<RfState>,
}

impl CaptureControl for FakeCapture {
    fn rf_state(&mut self) -> Result<RfState, ControlError> {
        match &self.state_error {
            Some(e) => Err(e.clone()),
            None => Ok(self.state),
        }
    }
    fn apply_rf(&mut self, target: RfState) -> Result<RfState, ControlError> {
        self.applies.push(target);
        if let Some(e) = &self.apply_error {
            return Err(e.clone());
        }
        let applied = self.readback.unwrap_or(target);
        self.state = applied;
        Ok(applied)
    }
}

#[derive(Clone, Default)]
struct CaptureHandle(Arc<Mutex<FakeCapture>>);

impl CaptureHandle {
    fn new(state: RfState) -> Self {
        Self(Arc::new(Mutex::new(FakeCapture {
            state,
            ..Default::default()
        })))
    }
    fn configure(&self, f: impl FnOnce(&mut FakeCapture)) {
        f(&mut self.0.lock().unwrap());
    }
    fn applies(&self) -> Vec<RfState> {
        self.0.lock().unwrap().applies.clone()
    }
    fn capture(&self) -> Box<dyn CaptureControl> {
        Box::new(self.clone())
    }
}

impl CaptureControl for CaptureHandle {
    fn rf_state(&mut self) -> Result<RfState, ControlError> {
        self.0.lock().unwrap().rf_state()
    }
    fn apply_rf(&mut self, target: RfState) -> Result<RfState, ControlError> {
        self.0.lock().unwrap().apply_rf(target)
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

const ORIGINAL: ChannelState = ChannelState {
    offset_hz: -1_250_000,
    output_rate_hz: 12_500,
};

fn plan(start_hz: u64, stop_hz: u64) -> SweepPlan {
    SweepPlan::new(start_hz, stop_hz, NBFM_STEP_HZ).expect("test plan is valid")
}

fn config(plan: SweepPlan) -> SweepConfig {
    SweepConfig {
        plan,
        settle_ms: 10,
        ..SweepConfig::default()
    }
}

fn controller(handle: &FakeHandle, clock: SharedClock, stop: Arc<AtomicBool>) -> SweepController {
    SweepController::new(handle.channel(), Box::new(clock), stop)
}

fn temp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "sigproc-sweep-{}-{name}-{nanos}",
        std::process::id()
    ))
}

// ── Plan geometry ───────────────────────────────────────────────────────────

#[test]
fn nbfm_band_plan_is_inclusive_and_bounded() {
    let plan = SweepPlan::nbfm_band();
    let steps = plan.steps();

    // 10 MHz / 12.5 kHz = 800 channels, so 800 dwell points whose channels tile
    // the band edge-to-edge.
    assert_eq!(steps.len(), 800);
    assert_eq!(steps[0], SWEEP_START_HZ + NBFM_HALF_STEP_HZ);
    assert_eq!(steps[799], SWEEP_STOP_HZ - NBFM_HALF_STEP_HZ);
    // The first channel's lower edge and the last channel's upper edge are the
    // band edges themselves, so no channel is clipped by the capture boundary.
    assert_eq!(steps[0] - NBFM_HALF_STEP_HZ, SWEEP_START_HZ);
    assert_eq!(steps[799] + NBFM_HALF_STEP_HZ, SWEEP_STOP_HZ);
    for pair in steps.windows(2) {
        assert_eq!(pair[1] - pair[0], NBFM_STEP_HZ, "step size must not drift");
    }
}

#[test]
fn plan_refuses_bad_geometry_before_any_mutation() {
    assert_eq!(
        SweepPlan::new(140_000_000, 150_000_000, 0),
        Err(PlanError::EmptyStep)
    );
    assert_eq!(
        SweepPlan::new(150_000_000, 140_000_000, NBFM_STEP_HZ),
        Err(PlanError::Inverted {
            start_hz: 150_000_000,
            stop_hz: 140_000_000
        })
    );
    // Bounded means bounded: a sweep may not leave the licensed window on either
    // end, even by one step.
    assert!(matches!(
        SweepPlan::new(139_987_500, 150_000_000, NBFM_STEP_HZ),
        Err(PlanError::OutsideBand { .. })
    ));
    assert!(matches!(
        SweepPlan::new(140_000_000, 150_012_500, NBFM_STEP_HZ),
        Err(PlanError::OutsideBand { .. })
    ));
}

#[test]
fn offsets_are_exact_integer_hertz_from_the_fixed_centre() {
    let offsets = SweepPlan::nbfm_band().offsets_hz(DEFAULT_RF_CENTER_HZ);
    assert_eq!(offsets.len(), 800);
    assert_eq!(offsets[0], -4_993_750);
    // 800 channels is an even count, so no channel sits exactly on the centre:
    // the two middle ones straddle it.
    assert_eq!(offsets[399], -6_250);
    assert_eq!(offsets[400], 6_250);
    assert_eq!(offsets[799], 4_993_750);
    for offset in &offsets {
        // Edge-aligned centres sit half a channel off the 12.5 kHz grid, so every
        // offset is congruent to half a step rather than to zero.
        assert_eq!(
            offset.rem_euclid(NBFM_STEP_HZ as i64),
            NBFM_HALF_STEP_HZ as i64
        );
        assert!((-5_000_000..=5_000_000).contains(offset));
    }
}

#[test]
fn partial_plan_still_visits_its_own_endpoints() {
    let steps = plan(140_000_000, 140_050_000).steps();
    assert_eq!(steps.len(), 5);
    assert_eq!(
        steps,
        vec![
            140_000_000,
            140_012_500,
            140_025_000,
            140_037_500,
            140_050_000
        ]
    );
}

// ── Happy path: applied readback and observations ───────────────────────────

#[test]
fn every_step_records_applied_readback_and_observation() {
    let handle = FakeHandle::new(ORIGINAL);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let cfg = config(plan(140_000_000, 140_025_000));
    let report = ctl.run(&cfg);

    assert_eq!(report.stop, StopReason::Completed);
    assert_eq!(report.steps.len(), 3);
    assert_eq!(report.applied_matching(), 3);
    assert_eq!(report.original, Some(ORIGINAL));

    for (record, target) in report
        .steps
        .iter()
        .zip([140_000_000, 140_012_500, 140_025_000])
    {
        assert_eq!(record.target_hz, target);
        assert_eq!(
            record.offset_hz,
            target as i64 - DEFAULT_RF_CENTER_HZ as i64
        );
        match &record.outcome {
            StepOutcome::Applied {
                applied,
                observation,
                observation_error,
            } => {
                assert_eq!(applied.offset_hz, record.offset_hz);
                assert_eq!(applied.output_rate_hz, NBFM_STEP_HZ);
                assert_eq!(*observation_error, None);
                let obs = observation.expect("observation recorded");
                assert_eq!(obs.packets_received, 1_000);
                assert_eq!(obs.output_power_dbfs, Some(-42.5));
            }
            other => panic!("expected an applied step, got {other:?}"),
        }
    }

    // Three retunes plus exactly one restore back to the original channel.
    let attempts = handle.attempts();
    assert_eq!(attempts.len(), 4);
    assert_eq!(
        attempts.last().copied(),
        Some((ORIGINAL.offset_hz, ORIGINAL.output_rate_hz))
    );
    assert_eq!(report.restore, RestoreOutcome::Restored);
    assert_eq!(handle.applied(), ORIGINAL);
}

#[test]
fn a_lying_readback_is_recorded_not_trusted() {
    let handle = FakeHandle::new(ORIGINAL);
    handle.configure(|f| f.lie_offset_by = NBFM_STEP_HZ as i64);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let report = ctl.run(&config(plan(140_000_000, 140_012_500)));

    assert_eq!(report.steps.len(), 2);
    // The service answered, so the steps are Applied — but the readback does not
    // match what was asked, and the confirmed count says so instead of assuming
    // the setter took effect.
    assert_eq!(report.applied_matching(), 0);
}

#[test]
fn a_failed_observation_does_not_discard_the_step() {
    let handle = FakeHandle::new(ORIGINAL);
    handle.configure(|f| f.observe_error = Some(ControlError::Uncertain("read timed out".into())));
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let report = ctl.run(&config(plan(140_000_000, 140_000_000)));

    assert_eq!(report.stop, StopReason::Completed);
    match &report.steps[0].outcome {
        StepOutcome::Applied {
            observation,
            observation_error,
            ..
        } => {
            assert_eq!(*observation, None);
            assert!(observation_error
                .as_deref()
                .is_some_and(|e| e.contains("uncertain")));
        }
        other => panic!("expected applied, got {other:?}"),
    }
    // A read-only failure is not a mutation, so the sweep still completes and
    // still restores.
    assert_eq!(report.restore, RestoreOutcome::Restored);
}

// ── Failure paths: stop, never retry, restore ───────────────────────────────

#[test]
fn a_refused_step_stops_the_sweep_and_restores() {
    let handle = FakeHandle::new(ORIGINAL);
    handle.configure(|f| f.fail_at = Some((1, ControlError::Rejected("bad rate".into()))));
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let report = ctl.run(&config(plan(140_000_000, 140_050_000)));

    assert_eq!(report.stop, StopReason::Refused);
    assert_eq!(report.steps.len(), 2, "the refused step is recorded");
    assert!(matches!(
        &report.steps[1].outcome,
        StepOutcome::Refused { error } if error.contains("bad rate")
    ));
    // Two apply attempts (one ok, one refused) plus the restore — no retry.
    assert_eq!(handle.attempts().len(), 3);
    assert_eq!(report.restore, RestoreOutcome::Restored);
    assert_eq!(handle.applied(), ORIGINAL);
}

#[test]
fn an_uncertain_execute_is_attempted_once_and_never_retried() {
    let handle = FakeHandle::new(ORIGINAL);
    handle.configure(|f| {
        f.fail_at = Some((
            0,
            ControlError::Uncertain("reply lost after the request was written".into()),
        ))
    });
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let report = ctl.run(&config(plan(140_000_000, 140_050_000)));

    assert_eq!(report.stop, StopReason::Uncertain);
    assert!(matches!(
        &report.steps[0].outcome,
        StepOutcome::Uncertain { .. }
    ));
    // The first offset appears exactly once in the attempt log: an uncertain
    // execute must never be re-sent.
    let first_offset = 140_000_000i64 - DEFAULT_RF_CENTER_HZ as i64;
    let attempts = handle.attempts();
    assert_eq!(
        attempts.iter().filter(|(o, _)| *o == first_offset).count(),
        1,
        "uncertain request was retried: {attempts:?}"
    );
    // Nothing after the uncertain step was attempted; the only later call is the
    // restore, because an uncertain mutation still owes a restore.
    assert_eq!(attempts.len(), 2);
    assert_eq!(report.restore, RestoreOutcome::Restored);
}

#[test]
fn an_undelivered_execute_is_not_treated_as_applied_and_needs_no_restore() {
    let handle = FakeHandle::new(ORIGINAL);
    handle.configure(|f| f.fail_at = Some((0, ControlError::NotSent("connection refused".into()))));
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let report = ctl.run(&config(plan(140_000_000, 140_050_000)));

    assert_eq!(report.stop, StopReason::Unsent);
    assert!(matches!(
        &report.steps[0].outcome,
        StepOutcome::Unsent { .. }
    ));
    // A request that never left the process cannot have changed anything, so
    // there is nothing to restore and no restore call is made.
    assert_eq!(handle.attempts().len(), 1);
    assert_eq!(report.restore, RestoreOutcome::NothingToRestore);
}

#[test]
fn a_failed_restore_is_reported_and_the_recovery_file_keeps_the_original() {
    let handle = FakeHandle::new(ORIGINAL);
    // Call 0 applies the single step; call 1 is the restore, which fails.
    handle.configure(|f| {
        f.fail_from = Some((
            1,
            ControlError::Uncertain("reply lost during restore".into()),
        ))
    });
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let recovery = temp_path("recovery-restore-fail.json");
    let mut ctl = controller(&handle, SharedClock(clock), stop).with_recovery_file(&recovery);

    let report = ctl.run(&config(plan(140_000_000, 140_000_000)));

    assert_eq!(report.stop, StopReason::Completed);
    assert!(
        matches!(report.restore, RestoreOutcome::Failed { .. }),
        "a restore that did not read back must be reported: {:?}",
        report.restore
    );
    // The recovery file still carries the state an operator can re-apply by hand.
    let saved = read_recovery_file(&recovery).expect("recovery file readable");
    assert_eq!(saved.channel, ORIGINAL);
    let _ = std::fs::remove_file(&recovery);
}

#[test]
fn a_state_read_failure_refuses_to_sweep_and_mutates_nothing() {
    let handle = FakeHandle::new(ORIGINAL);
    handle.configure(|f| f.state_error = Some(ControlError::Uncertain("connect timeout".into())));
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let report = ctl.run(&config(plan(140_000_000, 140_050_000)));

    assert_eq!(report.stop, StopReason::PreflightFailed);
    assert!(report.steps.is_empty());
    assert!(handle.attempts().is_empty(), "no mutation may be attempted");
    assert_eq!(report.restore, RestoreOutcome::NothingToRestore);
    assert!(report
        .detail
        .as_deref()
        .is_some_and(|d| d.contains("nothing to restore")));
}

// ── Bounds: budget, deadline, interruption ──────────────────────────────────

#[test]
fn the_step_budget_bounds_the_run_and_still_restores() {
    let handle = FakeHandle::new(ORIGINAL);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let mut cfg = config(plan(140_000_000, 150_000_000));
    cfg.max_steps = Some(2);
    let report = ctl.run(&cfg);

    assert_eq!(report.stop, StopReason::StepBudget { reached: 2 });
    assert_eq!(report.steps.len(), 2);
    assert_eq!(handle.attempts().len(), 3, "two steps plus the restore");
    assert_eq!(report.restore, RestoreOutcome::Restored);
}

#[test]
fn the_deadline_bounds_the_run_and_still_restores() {
    let handle = FakeHandle::new(ORIGINAL);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let mut cfg = config(plan(140_000_000, 150_000_000));
    cfg.settle_ms = 100;
    cfg.deadline_ms = Some(250);
    let report = ctl.run(&cfg);

    // Each step takes 100 ms of dwell, so the third step's pre-check sees 200 ms
    // elapsed... and 300 ms only after it runs. Either way the run is bounded.
    assert!(
        matches!(report.stop, StopReason::Deadline),
        "expected the deadline to stop the sweep, got {:?}",
        report.stop
    );
    assert!(report.steps.len() >= 2 && report.steps.len() <= 3);
    assert_eq!(report.restore, RestoreOutcome::Restored);
}

#[test]
fn an_interrupt_stops_cleanly_and_restores() {
    let handle = FakeHandle::new(ORIGINAL);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();

    // Trip the operator stop flag after the third successful retune.
    let flag = stop.clone();
    handle.configure(move |f| {
        f.on_success = Some(Arc::new(move |index| {
            if index == 2 {
                flag.store(true, Ordering::SeqCst);
            }
        }))
    });

    let mut ctl = controller(&handle, SharedClock(clock), stop);
    let report = ctl.run(&config(plan(140_000_000, 150_000_000)));

    assert_eq!(report.stop, StopReason::Interrupted);
    assert_eq!(report.steps.len(), 3);
    assert_eq!(report.applied_matching(), 3);
    assert_eq!(handle.attempts().len(), 4, "three steps plus the restore");
    assert_eq!(report.restore, RestoreOutcome::Restored);
    assert_eq!(handle.applied(), ORIGINAL);
}

// ── RF front end: verify, optionally move, always put back ──────────────────

#[test]
fn an_rf_that_cannot_cover_the_plan_is_refused_without_mutating_anything() {
    let handle = FakeHandle::new(ORIGINAL);
    let capture = CaptureHandle::new(RfState {
        rf_freq_hz: 145_000_000,
        sample_rate_hz: 2_000_000,
    });
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop).with_capture(capture.capture());

    // apply_rf defaults to false: this run may not move the front end.
    let report = ctl.run(&config(plan(140_000_000, 150_000_000)));

    assert_eq!(report.stop, StopReason::PreflightFailed);
    assert!(report.steps.is_empty());
    assert!(handle.attempts().is_empty());
    assert!(capture.applies().is_empty(), "RF must not be moved");
    match &report.rf {
        RfHandling::Refused { reason, state } => {
            assert!(reason.contains("covers"));
            assert_eq!(state.unwrap().sample_rate_hz, 2_000_000);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_covering_rf_is_left_alone() {
    let handle = FakeHandle::new(ORIGINAL);
    let wide = RfState {
        rf_freq_hz: 145_000_000,
        sample_rate_hz: 10_000_000,
    };
    let capture = CaptureHandle::new(wide);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop).with_capture(capture.capture());

    let report = ctl.run(&config(plan(140_000_000, 150_000_000)));

    assert_eq!(report.stop, StopReason::Completed);
    assert_eq!(report.rf, RfHandling::AlreadySufficient { state: wide });
    assert!(capture.applies().is_empty());
}

#[test]
fn an_rf_that_must_move_is_applied_then_restored() {
    let handle = FakeHandle::new(ORIGINAL);
    let narrow = RfState {
        rf_freq_hz: 145_000_000,
        sample_rate_hz: 2_000_000,
    };
    let capture = CaptureHandle::new(narrow);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop).with_capture(capture.capture());

    let mut cfg = config(plan(140_000_000, 140_025_000));
    cfg.apply_rf = true;
    let report = ctl.run(&cfg);

    assert_eq!(report.stop, StopReason::Completed);
    // The RF moved to cover the plan, then went back exactly as it was.
    let applies = capture.applies();
    assert_eq!(applies.len(), 2);
    assert_eq!(applies[1], narrow);
    match &report.rf {
        RfHandling::AppliedThenRestored {
            original,
            applied,
            restore,
        } => {
            assert_eq!(*original, narrow);
            assert_eq!(applied.sample_rate_hz, 10_000_000);
            assert_eq!(*restore, RestoreOutcome::Restored);
        }
        other => panic!("expected applied-then-restored, got {other:?}"),
    }
}

#[test]
fn an_rf_moved_for_the_sweep_is_restored_even_when_a_step_fails() {
    let handle = FakeHandle::new(ORIGINAL);
    handle.configure(|f| {
        f.fail_at = Some((
            0,
            ControlError::Uncertain("reply lost after the request was written".into()),
        ))
    });
    let narrow = RfState {
        rf_freq_hz: 145_000_000,
        sample_rate_hz: 2_000_000,
    };
    let capture = CaptureHandle::new(narrow);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop).with_capture(capture.capture());

    let mut cfg = config(plan(140_000_000, 140_050_000));
    cfg.apply_rf = true;
    let report = ctl.run(&cfg);

    assert_eq!(report.stop, StopReason::Uncertain);
    let applies = capture.applies();
    assert_eq!(applies.len(), 2, "wideband applied, then restored");
    assert_eq!(applies[1], narrow);
    assert!(matches!(
        &report.rf,
        RfHandling::AppliedThenRestored {
            restore: RestoreOutcome::Restored,
            ..
        }
    ));
}

#[test]
fn an_rf_apply_whose_readback_disagrees_stops_the_sweep() {
    let handle = FakeHandle::new(ORIGINAL);
    let capture = CaptureHandle::new(RfState {
        rf_freq_hz: 145_000_000,
        sample_rate_hz: 2_000_000,
    });
    // The service says it applied something else.
    capture.configure(|c| {
        c.readback = Some(RfState {
            rf_freq_hz: 144_000_000,
            sample_rate_hz: 8_000_000,
        })
    });
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop).with_capture(capture.capture());

    let mut cfg = config(plan(140_000_000, 150_000_000));
    cfg.apply_rf = true;
    let report = ctl.run(&cfg);

    assert_eq!(report.stop, StopReason::PreflightFailed);
    assert!(handle.attempts().is_empty(), "no channel mutation");
    assert!(report
        .detail
        .as_deref()
        .is_some_and(|d| d.contains("read back")));
}

#[test]
fn an_unreadable_rf_state_refuses_the_sweep() {
    let handle = FakeHandle::new(ORIGINAL);
    let capture = CaptureHandle::new(RfState::default());
    capture.configure(|c| c.state_error = Some(ControlError::Uncertain("connect timeout".into())));
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop).with_capture(capture.capture());

    let report = ctl.run(&config(plan(140_000_000, 150_000_000)));

    assert_eq!(report.stop, StopReason::PreflightFailed);
    assert!(handle.attempts().is_empty());
    assert!(matches!(report.rf, RfHandling::Refused { state: None, .. }));
}

// ── Serialization against other controllers ─────────────────────────────────

#[test]
fn the_controller_lock_is_exclusive_and_released_on_drop() {
    let path = temp_path("controller.lock");
    let first = ControllerLock::acquire(&path).expect("first controller takes the lock");

    let second = ControllerLock::acquire(&path);
    assert!(
        matches!(second, Err(LockError::Held { .. })),
        "a second controller must not drive the same channelizer: {second:?}"
    );

    drop(first);
    let third = ControllerLock::acquire(&path).expect("the lock is released on drop");
    assert_eq!(third.path(), path.as_path());
    drop(third);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_stale_lock_is_broken_only_on_request() {
    let path = temp_path("stale.lock");
    std::fs::write(&path, b"{\"pid\":1,\"acquired_unix_ms\":0}").unwrap();

    // Fresh enough to be presumed live: refusing is the safe answer.
    let held = ControllerLock::break_stale(&path, 60_000);
    assert!(matches!(held, Err(LockError::Held { .. })));

    // Explicitly breaking a lock older than the threshold succeeds.
    let broken = ControllerLock::break_stale(&path, 0).expect("stale lock broken on request");
    drop(broken);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_recovery_file_round_trips() {
    let path = temp_path("recovery.json");
    let state = RecoveryState {
        channel: ORIGINAL,
        rf: Some(RfState {
            rf_freq_hz: 145_000_000,
            sample_rate_hz: 10_000_000,
        }),
        written_unix_ms: 1_700_000_000_000,
    };
    write_recovery_file(&path, &state).expect("write recovery file");
    let read = read_recovery_file(&path).expect("read recovery file");
    assert_eq!(read, state);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_report_serializes_with_its_tagged_outcomes() {
    let handle = FakeHandle::new(ORIGINAL);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    let report = ctl.run(&config(plan(140_000_000, 140_012_500)));
    let json = serde_json::to_value(&report).expect("report serializes");

    assert_eq!(json["stop"]["reason"], "completed");
    assert_eq!(json["restore"]["restore"], "restored");
    assert_eq!(json["rf"]["rf"], "not_used");
    assert_eq!(json["steps"][0]["outcome"], "applied");
    assert_eq!(json["steps"][0]["target_hz"], 140_000_000u64);
    assert_eq!(json["steps"][0]["applied"]["offset_hz"], -5_000_000);
}

// ── Channel fit against the captured wideband ───────────────────────────────

#[test]
fn the_default_plan_fits_the_documented_wideband_exactly() {
    // The edge channels are the binding case: half a channel past the outermost
    // centre is exactly the half-band of a 10 MS/s capture.
    let offsets = SweepPlan::nbfm_band().offsets_hz(DEFAULT_RF_CENTER_HZ);
    let half_band = DEFAULT_WIDEBAND_RATE_HZ / 2;
    let lowest = offsets.first().unwrap().unsigned_abs() + NBFM_HALF_STEP_HZ;
    let highest = offsets.last().unwrap().unsigned_abs() + NBFM_HALF_STEP_HZ;
    assert_eq!(lowest, half_band);
    assert_eq!(highest, half_band);
}

#[test]
fn an_edge_aligned_plan_records_no_clipping_warning() {
    let handle = FakeHandle::new(ORIGINAL);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    // Centres inset by half a channel from both edges of a short slice.
    let report = ctl.run(&config(plan(140_006_250, 140_031_250)));

    assert_eq!(report.stop, StopReason::Completed);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn a_channel_centred_on_the_band_edge_is_reported_as_clipped() {
    let handle = FakeHandle::new(ORIGINAL);
    let stop = Arc::new(AtomicBool::new(false));
    let (_c, clock) = FakeClock::handle();
    let mut ctl = controller(&handle, SharedClock(clock), stop);

    // A centre at exactly 140.000 MHz reaches 6.25 kHz past the capture edge.
    let report = ctl.run(&config(plan(140_000_000, 140_012_500)));

    // Reported, not fatal: the sweep still ran and still restored.
    assert_eq!(report.stop, StopReason::Completed);
    assert_eq!(report.steps.len(), 2);
    assert_eq!(report.restore, RestoreOutcome::Restored);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    let warning = &report.warnings[0];
    assert!(warning.contains("clipped"), "{warning}");
    assert!(warning.contains("140006250"), "{warning}");
}
