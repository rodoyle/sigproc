//! Complex sample container, sc16 conversion, and level measurement.
//!
//! The wire carries interleaved sc16; the DSP works in normalized f32. Keeping
//! one small `Iq` type (rather than pulling in `num-complex`) means this crate —
//! and therefore every DSP test — builds with no dependencies beyond
//! serde/toml/anyhow/log.

/// One complex baseband sample, normalized so full scale is ±1.0.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Iq {
    pub i: f32,
    pub q: f32,
}

impl Iq {
    /// The origin.
    pub const ZERO: Iq = Iq { i: 0.0, q: 0.0 };

    #[inline]
    pub fn new(i: f32, q: f32) -> Self {
        Iq { i, q }
    }

    /// Instantaneous power `|s|^2` (no square root).
    #[inline]
    pub fn power(self) -> f32 {
        self.i * self.i + self.q * self.q
    }

    /// Instantaneous magnitude `|s|`.
    #[inline]
    pub fn magnitude(self) -> f32 {
        self.power().sqrt()
    }
}

/// Full-scale magnitude of an sc16 component (i16 range is ±32767).
pub const SC16_FULL_SCALE: f32 = 32768.0;

/// Convert interleaved sc16 `[I0, Q0, I1, Q1, ...]` to normalized f32 IQ.
///
/// A trailing unpaired element is ignored: the wire format carries whole I/Q
/// pairs, and a half sample is not a sample.
pub fn sc16_to_iq(interleaved: &[i16]) -> Vec<Iq> {
    interleaved
        .chunks_exact(2)
        .map(|p| Iq::new(p[0] as f32 / SC16_FULL_SCALE, p[1] as f32 / SC16_FULL_SCALE))
        .collect()
}

/// Convert normalized f32 IQ back to interleaved sc16, saturating rather than
/// wrapping — a wrapped sample is a loud click, a saturated one is a bounded
/// error, and gain mistakes upstream are the common cause of both.
pub fn iq_to_sc16(iq: &[Iq]) -> Vec<i16> {
    let mut out = Vec::with_capacity(iq.len() * 2);
    for s in iq {
        for v in [s.i, s.q] {
            let scaled = (v * SC16_FULL_SCALE).round();
            out.push(scaled.clamp(-32768.0, 32767.0) as i16);
        }
    }
    out
}

/// Mean power `E{|s|^2}` over the slice; `None` for an empty slice.
pub fn mean_power(iq: &[Iq]) -> Option<f32> {
    if iq.is_empty() {
        return None;
    }
    let sum: f64 = iq.iter().map(|s| s.power() as f64).sum();
    Some((sum / iq.len() as f64) as f32)
}

/// Mean power in dBFS, referenced to a **full-scale complex** signal
/// (`|I| = |Q| = 1`, mean power 1.0), so a full-scale complex tone reads
/// 0.0 dBFS and a full-scale real sine reads -3.01 dBFS.
///
/// This is the SDR convention — the value is the complex power relative to a
/// full-scale complex signal. It is stated explicitly because the alternative
/// reference (0 dBFS = a full-scale real sine, mean power 0.5) differs by
/// exactly 3 dB, and a metrics field that silently means something else is
/// worse than no field at all.
pub fn power_dbfs(iq: &[Iq]) -> f32 {
    match mean_power(iq) {
        Some(p) if p > 0.0 => 10.0 * p.log10(),
        _ => f32::NEG_INFINITY,
    }
}

/// Magnitude envelope as a real signal (`q == 0`), for AM envelope detection
/// and for tone measurement on the recovered envelope.
pub fn envelope(iq: &[Iq]) -> Vec<Iq> {
    iq.iter().map(|s| Iq::new(s.magnitude(), 0.0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sc16_round_trips_within_one_lsb() {
        let original = vec![0i16, 32767, -32768, 16384, -16384, 1, -1, 100];
        let iq = sc16_to_iq(&original);
        assert_eq!(iq.len(), 4, "4 I/Q pairs");
        let back = iq_to_sc16(&iq);
        for (a, b) in original.iter().zip(back.iter()) {
            assert!((*a as i32 - *b as i32).abs() <= 1, "{a} -> {b}");
        }
    }

    #[test]
    fn a_trailing_unpaired_sample_is_dropped() {
        let iq = sc16_to_iq(&[100, 200, 300]);
        assert_eq!(iq.len(), 1);
        assert_eq!(back_lens(&iq), 2);
    }

    fn back_lens(iq: &[Iq]) -> usize {
        iq_to_sc16(iq).len()
    }

    #[test]
    fn quantization_saturates_instead_of_wrapping() {
        // 2.0 is far above full scale; it must clamp to the i16 rail, not wrap
        // around to a negative value (which would render as a click).
        let out = iq_to_sc16(&[Iq::new(2.0, -2.0)]);
        assert_eq!(out, vec![32767, -32768]);
    }

    #[test]
    fn full_scale_complex_tone_reads_zero_dbfs_and_a_sine_reads_minus_three() {
        // 1024 samples of a unit-amplitude complex tone: |s|^2 = 1 everywhere.
        let n = 1024;
        let complex: Vec<Iq> = (0..n)
            .map(|k| {
                let ang = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
                Iq::new(ang.cos() as f32, ang.sin() as f32)
            })
            .collect();
        assert!(
            (power_dbfs(&complex) - 0.0).abs() < 0.01,
            "{}",
            power_dbfs(&complex)
        );

        // A full-scale REAL sine has mean power 0.5, i.e. -3.01 dBFS under the
        // full-scale-complex reference.
        let sine: Vec<Iq> = (0..n)
            .map(|k| {
                let ang = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
                Iq::new(ang.cos() as f32, 0.0)
            })
            .collect();
        assert!(
            (power_dbfs(&sine) - (-3.0103)).abs() < 0.01,
            "{}",
            power_dbfs(&sine)
        );
    }

    #[test]
    fn silence_and_empty_are_negative_infinity_not_nan() {
        assert!(power_dbfs(&[Iq::ZERO; 16]).is_infinite());
        assert!(power_dbfs(&[]).is_infinite());
        assert_eq!(mean_power(&[]), None);
    }

    #[test]
    fn envelope_of_an_am_signal_tracks_the_modulation() {
        // A(1 + m cos(wt)) carrier at baseband: the envelope must rise and fall.
        let rate = 50_000.0;
        let iq: Vec<Iq> = (0..5000)
            .map(|k| {
                let t = k as f64 / rate;
                let amp = 1.0 + 0.5 * (2.0 * std::f64::consts::PI * 1000.0 * t).cos();
                Iq::new(amp as f32, 0.0)
            })
            .collect();
        let env = envelope(&iq);
        assert_eq!(env.len(), iq.len());
        let peak = env.iter().map(|s| s.i).fold(f32::MIN, f32::max);
        let trough = env.iter().map(|s| s.i).fold(f32::MAX, f32::min);
        assert!((peak - 1.5).abs() < 1e-3, "peak {peak}");
        assert!((trough - 0.5).abs() < 1e-3, "trough {trough}");
    }
}
