//! Numerically controlled oscillator and the complex mixer that selects a channel.
//!
//! Channel selection is a single complex multiplication per sample: multiply by
//! `e^(-j 2 pi f t)` and the component sitting at `+f` moves to DC, where the
//! decimating filter can remove everything else.
//!
//! The oscillator's phase is **state that persists across input blocks**. Reset
//! it per packet and every block starts at phase 0, which is a phase step at
//! each boundary — that produces spectral splatter proportional to the block
//! rate, and it corrupts exactly the tone measurement the verification gate
//! depends on.

use super::iq::Iq;
use std::f64::consts::TAU;

/// Complex oscillator producing `e^(-j 2 pi f n / fs)` used to mix a channel to DC.
pub struct Nco {
    /// Current phase in radians of the *conjugate* phasor (i.e. the rotation
    /// applied to the signal), kept in `[0, TAU)` so it never drifts in
    /// magnitude over a long capture.
    phase: f64,
    /// Phase advance per input sample.
    phase_inc: f64,
}

impl Nco {
    /// Create an oscillator that shifts `offset_hz` to DC at `sample_rate`.
    ///
    /// `offset_hz` may be negative (a channel below the wideband centre).
    pub fn new(offset_hz: f64, sample_rate: f64) -> Self {
        let phase_inc = if sample_rate > 0.0 {
            (TAU * offset_hz / sample_rate).rem_euclid(TAU)
        } else {
            0.0
        };
        Nco {
            phase: 0.0,
            phase_inc,
        }
    }

    /// Advance one sample and return the conjugate phasor `(cos, -sin)`.
    #[inline]
    pub fn next_phasor(&mut self) -> (f32, f32) {
        let (sin, cos) = self.phase.sin_cos();
        self.phase += self.phase_inc;
        if self.phase >= TAU {
            self.phase -= TAU;
        }
        (cos as f32, -(sin as f32))
    }

    /// Mix one sample: multiply by the conjugate phasor, shifting `+offset_hz`
    /// to DC.
    #[inline]
    pub fn mix(&mut self, s: Iq) -> Iq {
        let (c, ms) = self.next_phasor();
        // (i + jq)(c + j*ms) with ms = -sin
        Iq::new(s.i * c - s.q * ms, s.i * ms + s.q * c)
    }

    /// Current phase in radians (test observability).
    pub fn phase(&self) -> f64 {
        self.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complex_tone(freq: f64, sample_rate: f64, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|k| {
                let ang = TAU * freq * k as f64 / sample_rate;
                Iq::new(ang.cos() as f32, ang.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn a_tone_at_the_offset_mixes_to_dc() {
        // +200 kHz channel in a 2 MS/s stream -> constant 1+0j.
        let sig = complex_tone(200_000.0, 2_000_000.0, 4096);
        let mut nco = Nco::new(200_000.0, 2_000_000.0);
        let mixed: Vec<Iq> = sig.iter().map(|s| nco.mix(*s)).collect();

        let mean_i = mixed.iter().map(|s| s.i as f64).sum::<f64>() / mixed.len() as f64;
        let mean_q = mixed.iter().map(|s| s.q as f64).sum::<f64>() / mixed.len() as f64;
        assert!((mean_i - 1.0).abs() < 1e-3, "mean I = {mean_i}");
        assert!(mean_q.abs() < 1e-3, "mean Q = {mean_q}");
        // Every sample is DC, not just the mean.
        for s in &mixed {
            assert!((s.i - 1.0).abs() < 1e-2 && s.q.abs() < 1e-2, "{s:?}");
        }
    }

    #[test]
    fn a_negative_offset_is_shifted_to_dc_too() {
        let sig = complex_tone(-150_000.0, 2_000_000.0, 4096);
        let mut nco = Nco::new(-150_000.0, 2_000_000.0);
        let mixed: Vec<Iq> = sig.iter().map(|s| nco.mix(*s)).collect();
        let mean_i = mixed.iter().map(|s| s.i as f64).sum::<f64>() / mixed.len() as f64;
        assert!((mean_i - 1.0).abs() < 1e-3, "mean I = {mean_i}");
    }

    /// The state bug this type exists to avoid: mixing a stream in two blocks
    /// must equal mixing it in one, sample for sample.
    #[test]
    fn phase_continuity_holds_across_block_boundaries() {
        let sig = complex_tone(137_000.0, 2_000_000.0, 3000);

        let mut one_shot = Nco::new(137_000.0, 2_000_000.0);
        let want: Vec<Iq> = sig.iter().map(|s| one_shot.mix(*s)).collect();

        let mut chunked = Nco::new(137_000.0, 2_000_000.0);
        let mut got = Vec::new();
        for block in [&sig[..1024], &sig[1024..1025], &sig[1025..]] {
            for s in block {
                got.push(chunked.mix(*s));
            }
        }

        assert_eq!(got.len(), want.len());
        for (a, b) in got.iter().zip(want.iter()) {
            assert!(
                (a.i - b.i).abs() < 1e-6 && (a.q - b.q).abs() < 1e-6,
                "blocked mixing diverged: {a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn phase_stays_bounded_over_a_long_run() {
        let mut nco = Nco::new(1_999_999.0, 2_000_000.0);
        for _ in 0..1_000_000 {
            nco.next_phasor();
        }
        assert!(
            (0.0..TAU).contains(&nco.phase()),
            "phase escaped [0, TAU): {}",
            nco.phase()
        );
    }

    #[test]
    fn zero_offset_is_a_passthrough() {
        let mut nco = Nco::new(0.0, 2_000_000.0);
        let s = Iq::new(0.25, -0.5);
        let out = nco.mix(s);
        assert!((out.i - s.i).abs() < 1e-9 && (out.q - s.q).abs() < 1e-9);
    }

    /// Guard against a degenerate call: a zero sample rate must not produce NaN.
    #[test]
    fn zero_sample_rate_does_not_produce_nan() {
        let mut nco = Nco::new(1000.0, 0.0);
        let out = nco.mix(Iq::new(1.0, 0.0));
        assert!(out.i.is_finite() && out.q.is_finite(), "{out:?}");
    }
}
