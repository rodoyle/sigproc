//! FIR filter design (Kaiser window) and a stateful decimating filter.
//!
//! Two things here are easy to get subtly wrong, and both are load-bearing for
//! the channelizer:
//!
//! 1. **Taps versus transition width.** A Kaiser design trades stopband
//!    attenuation against transition width; asking for a narrow transition at a
//!    high input rate is what makes "just one big decimating FIR" collapse into
//!    two thousand taps. [`estimate_taps`] makes that cost explicit so the
//!    stage planner can reject a plan instead of silently burning a CPU.
//! 2. **State across blocks.** The delay line is per-instance state, never
//!    per-block. Zeroing it at a VITA49 packet boundary restarts the filter at
//!    every packet, which splatters the spectrum at the packet rate.

use std::f64::consts::PI;

/// Modified Bessel function of the first kind, order 0, via its power series.
///
/// Converges in a few terms for the `beta` range Kaiser design needs (< 10);
/// the loop guard is belt-and-braces against a caller passing something absurd.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    for k in 1..=64 {
        let t = x / (2.0 * k as f64);
        term *= t * t;
        sum += term;
        if term < 1e-18 * sum {
            break;
        }
    }
    sum
}

/// Kaiser window shape parameter for a target stopband attenuation.
pub fn kaiser_beta(stopband_db: f64) -> f64 {
    if stopband_db > 50.0 {
        0.1102 * (stopband_db - 8.7)
    } else if stopband_db >= 21.0 {
        let a = stopband_db - 21.0;
        0.5842 * a.powf(0.4) + 0.07886 * a
    } else {
        0.0
    }
}

/// Kaiser's estimate of the taps needed for `stopband_db` over a transition of
/// `transition_hz` at `sample_rate`. Always odd (so the filter has an integer
/// group delay) and at least 3.
///
/// The formula is `N ~= (A - 8) / (2.285 * dw)` with `dw` the transition width
/// in radians per sample — the standard Kaiser result, and the reason a narrow
/// transition at a high rate is expensive.
pub fn estimate_taps(stopband_db: f64, transition_hz: f64, sample_rate: f64) -> usize {
    if transition_hz <= 0.0 || sample_rate <= 0.0 {
        return usize::MAX;
    }
    let dw = 2.0 * PI * transition_hz / sample_rate;
    let n = ((stopband_db - 8.0) / (2.285 * dw)).ceil();
    if !n.is_finite() || n < 0.0 || n > (usize::MAX / 2) as f64 {
        return usize::MAX;
    }
    let mut taps = n as usize + 2;
    if taps.is_multiple_of(2) {
        taps += 1;
    }
    taps.max(3)
}

/// Design a linear-phase low-pass FIR with unity DC gain.
///
/// `cutoff_hz` is the -6 dB point (the passband edge is below it, the stopband
/// above); `stopband_db` sets the Kaiser window's sidelobe level.
pub fn kaiser_lowpass(taps: usize, cutoff_hz: f64, sample_rate: f64, stopband_db: f64) -> Vec<f32> {
    assert!(
        taps >= 3 && !taps.is_multiple_of(2),
        "taps must be odd >= 3"
    );
    assert!(sample_rate > 0.0, "sample_rate must be positive");
    let fc = (cutoff_hz / sample_rate).clamp(1e-9, 0.499_999);
    let m = (taps - 1) as f64;
    let beta = kaiser_beta(stopband_db);
    let i0_beta = bessel_i0(beta);

    let mut h = vec![0.0f64; taps];
    let mut dc_gain = 0.0f64;
    for (n, tap) in h.iter_mut().enumerate() {
        let x = n as f64 - m / 2.0;
        // Ideal low-pass impulse response: 2 fc sinc(2 fc x), with the
        // removable singularity taken by its limit at x = 0.
        let sinc = if x.abs() < 1e-12 {
            2.0 * fc
        } else {
            (2.0 * PI * fc * x).sin() / (PI * x)
        };
        // Kaiser window.
        let r = 2.0 * x / m;
        let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
        *tap = sinc * w;
        dc_gain += *tap;
    }

    // Unity DC gain: without this, every stage would change the level and the
    // "is the channel the same power as the input" check becomes meaningless.
    let scale = if dc_gain.abs() > 0.0 {
        1.0 / dc_gain
    } else {
        1.0
    };
    h.iter().map(|v| (v * scale) as f32).collect()
}

/// A FIR low-pass that only emits every `factor`-th input sample.
///
/// The tap loop runs per *output* sample rather than per input sample: with
/// `taps` MACs per output, a decimating stage costs `taps / factor` per input
/// sample, which is what keeps a 49-tap stage at 2 MS/s cheap.
pub struct FirDecimator {
    taps: Vec<f32>,
    /// Delay line, newest sample at `pos - 1`.
    hist: Vec<f32>,
    pos: usize,
    factor: usize,
    /// Input samples since the last output.
    phase: usize,
}

impl FirDecimator {
    /// Create a decimating FIR. `taps` are in `x[n-k]` order (so `taps[0]`
    /// multiplies the newest sample).
    pub fn new(taps: Vec<f32>, factor: usize) -> Self {
        let len = taps.len();
        assert!(len > 0, "taps must not be empty");
        assert!(factor >= 1, "decimation factor must be >= 1");
        FirDecimator {
            taps,
            hist: vec![0.0; len],
            pos: 0,
            factor,
            phase: 0,
        }
    }

    /// Push one input sample; returns the filtered output on every `factor`-th call.
    #[inline]
    pub fn process(&mut self, x: f32) -> Option<f32> {
        self.hist[self.pos] = x;
        self.pos += 1;
        if self.pos == self.hist.len() {
            self.pos = 0;
        }

        self.phase += 1;
        if self.phase < self.factor {
            return None;
        }
        self.phase = 0;

        // Newest sample is at pos - 1; walk backwards pairing taps[k] with x[n-k].
        let taps = self.taps.len();
        let mut idx = if self.pos == 0 {
            taps - 1
        } else {
            self.pos - 1
        };
        let mut acc = 0.0f32;
        for k in 0..taps {
            acc += self.taps[k] * self.hist[idx];
            idx = if idx == 0 { taps - 1 } else { idx - 1 };
        }
        Some(acc)
    }

    /// Clear the delay line and the decimation phase. Used when a stream
    /// restarts (a new capture session), never mid-stream.
    pub fn reset(&mut self) {
        self.hist.iter_mut().for_each(|v| *v = 0.0);
        self.pos = 0;
        self.phase = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_db(taps: &[f32], freq_hz: f64, sample_rate: f64) -> f64 {
        let w = 2.0 * PI * freq_hz / sample_rate;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (k, t) in taps.iter().enumerate() {
            re += *t as f64 * (w * k as f64).cos();
            im -= *t as f64 * (w * k as f64).sin();
        }
        let mag = (re * re + im * im).sqrt();
        20.0 * mag.max(1e-12).log10()
    }

    #[test]
    fn taps_are_odd_and_grow_as_the_transition_narrows() {
        let wide = estimate_taps(80.0, 100_000.0, 2_000_000.0);
        let narrow = estimate_taps(80.0, 5_000.0, 2_000_000.0);
        assert!(wide > 3 && narrow > 3);
        assert!(wide.is_multiple_of(2) == false && narrow.is_multiple_of(2) == false);
        assert!(
            narrow > wide * 5,
            "narrow transition must cost far more taps: {narrow} vs {wide}"
        );
        // Degenerate inputs must not yield a usable-looking small number.
        assert_eq!(estimate_taps(80.0, 0.0, 2_000_000.0), usize::MAX);
    }

    #[test]
    fn design_has_unity_dc_gain_and_a_real_passband() {
        let rate = 250_000.0;
        let taps = kaiser_lowpass(251, 20_000.0, rate, 80.0);
        assert_eq!(taps.len(), 251);
        assert!(response_db(&taps, 0.0, rate).abs() < 0.01, "DC gain");
        assert!(response_db(&taps, 5_000.0, rate) > -0.5, "passband");
        assert!(response_db(&taps, 15_000.0, rate) > -1.0, "passband edge");
    }

    #[test]
    fn design_meets_its_stopband_spec() {
        let rate = 250_000.0;
        let taps = kaiser_lowpass(251, 20_000.0, rate, 80.0);
        for f in [30_000.0, 50_000.0, 75_000.0, 100_000.0, 125_000.0] {
            let db = response_db(&taps, f, rate);
            assert!(db < -60.0, "{f} Hz only {db} dB");
        }
    }

    /// The delay-line orientation bug: with `taps[k]` paired against the wrong
    /// end of the history, an asymmetric filter silently time-reverses.
    #[test]
    fn taps_pair_with_the_newest_sample_first() {
        let mut d = FirDecimator::new(vec![1.0, 2.0, 3.0], 1);
        assert_eq!(d.process(1.0), Some(1.0));
        assert_eq!(d.process(1.0), Some(1.0 * 1.0 + 2.0 * 1.0));
        assert_eq!(d.process(1.0), Some(1.0 + 2.0 + 3.0));
        // y = x[n] + 2x[n-1] + 3x[n-2]: a new sample walks into the taps.
        assert_eq!(d.process(0.0), Some(0.0 + 2.0 * 1.0 + 3.0 * 1.0));
    }

    #[test]
    fn emits_exactly_one_output_per_factor_inputs() {
        let mut d = FirDecimator::new(vec![1.0, 1.0, 1.0], 5);
        let mut outs = 0;
        for k in 0..50 {
            if d.process(k as f32).is_some() {
                outs += 1;
            }
        }
        assert_eq!(outs, 10);
    }

    #[test]
    fn block_boundaries_do_not_restart_the_filter() {
        let taps = kaiser_lowpass(33, 20_000.0, 250_000.0, 60.0);
        let input: Vec<f32> = (0..600).map(|k| (k as f32 * 0.017).sin()).collect();

        let mut one = FirDecimator::new(taps.clone(), 4);
        let want: Vec<f32> = input.iter().filter_map(|x| one.process(*x)).collect();

        let mut chunked = FirDecimator::new(taps, 4);
        let mut got = Vec::new();
        for block in [&input[..7], &input[7..8], &input[8..200], &input[200..]] {
            for x in block {
                if let Some(y) = chunked.process(*x) {
                    got.push(y);
                }
            }
        }

        assert_eq!(got.len(), want.len());
        for (a, b) in got.iter().zip(want.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }

    #[test]
    fn reset_clears_state_so_a_new_session_starts_clean() {
        let mut d = FirDecimator::new(vec![1.0, 1.0, 1.0], 2);
        for k in 0..10 {
            d.process(k as f32);
        }
        d.reset();
        let mut fresh = FirDecimator::new(vec![1.0, 1.0, 1.0], 2);
        for k in 0..10 {
            assert_eq!(d.process(k as f32), fresh.process(k as f32));
        }
    }
}

#[cfg(test)]
mod spec {
    use super::*;

    fn response_db(taps: &[f32], freq_hz: f64, sample_rate: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq_hz / sample_rate;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (k, t) in taps.iter().enumerate() {
            re += *t as f64 * (w * k as f64).cos();
            im -= *t as f64 * (w * k as f64).sin();
        }
        20.0 * ((re * re + im * im).sqrt()).max(1e-15).log10()
    }

    /// The invariant behind every stopband claim in this repo: the tap count
    /// [`estimate_taps`] returns must actually achieve the attenuation it was
    /// asked for. Swept over the whole stopband, worst case — a filter that only
    /// meets its spec at the textbook edge frequency is not meeting it.
    #[test]
    fn the_tap_estimate_actually_achieves_the_requested_attenuation() {
        for (target, fs, trans, cut) in [
            (60.0, 2_000_000.0, 155_000.0, 97_500.0),
            (80.0, 2_000_000.0, 155_000.0, 97_500.0),
            (80.0, 200_000.0, 5_000.0, 22_500.0),
            (100.0, 200_000.0, 5_000.0, 22_500.0),
        ] {
            let taps = estimate_taps(target, trans, fs);
            assert!(taps < 100_000, "estimate must be finite: {taps}");
            let h = kaiser_lowpass(taps, cut, fs, target);
            let ctx = (fs, trans, cut);
            let stop = cut + trans / 2.0;
            let mut worst = f64::MIN;
            let mut f = stop;
            while f <= fs / 2.0 {
                worst = worst.max(response_db(&h, f, fs));
                f += trans / 8.0;
            }
            assert!(
                worst <= -target + 1.0,
                "target {target} dB at {ctx:?}: worst stopband {worst:.1} dB                  with {taps} taps"
            );
        }
    }
}
