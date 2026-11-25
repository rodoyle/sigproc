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

    /// Retune the oscillator to a new offset at the same sample rate.
    ///
    /// Called on a restart-free offset change. Phase is preserved across the
    /// change so there is no phase step (spectral splatter) at the boundary;
    /// only the phase increment updates.
    pub fn set_offset_hz(&mut self, offset_hz: f64, sample_rate: f64) {
        self.phase_inc = if sample_rate > 0.0 {
            (TAU * offset_hz / sample_rate).rem_euclid(TAU)
        } else {
            0.0
        };
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
    fn mixing_a_tone_at_the_offset_moves_it_to_dc() {
        let fs = 1_000_000.0;
        let offset = 100_000.0;
        let tone = complex_tone(offset, fs, 4096);
        let mut nco = Nco::new(offset, fs);
        let mixed: Vec<Iq> = tone.iter().map(|s| nco.mix(*s)).collect();
        let mean_i: f64 = mixed.iter().map(|s| s.i as f64).sum::<f64>() / mixed.len() as f64;
        let mean_q: f64 = mixed.iter().map(|s| s.q as f64).sum::<f64>() / mixed.len() as f64;
        assert!(
            mean_i.abs() > 0.9,
            "DC component should dominate, got {mean_i}"
        );
        assert!(
            mean_q.abs() < 0.1,
            "quadrature should be near zero, got {mean_q}"
        );
    }

    #[test]
    fn a_channel_below_the_centre_is_a_negative_offset() {
        let fs = 1_000_000.0;
        let offset = -100_000.0;
        let tone = complex_tone(offset, fs, 4096);
        let mut nco = Nco::new(offset, fs);
        let mixed: Vec<Iq> = tone.iter().map(|s| nco.mix(*s)).collect();
        let mean_i: f64 = mixed.iter().map(|s| s.i as f64).sum::<f64>() / mixed.len() as f64;
        assert!(
            mean_i.abs() > 0.9,
            "negative offset still moves the tone to DC"
        );
    }

    #[test]
    fn phase_is_preserved_across_a_retune() {
        let fs = 1_000_000.0;
        let mut nco = Nco::new(0.0, fs);
        for _ in 0..100 {
            let _ = nco.next_phasor();
        }
        let phase_before = nco.phase();
        nco.set_offset_hz(50_000.0, fs);
        assert_eq!(nco.phase(), phase_before);
        let _ = nco.next_phasor();
        let inc = nco.phase() - phase_before;
        let expected = (TAU * 50_000.0 / fs).rem_euclid(TAU);
        assert!(
            (inc - expected).abs() < 1e-9,
            "increment should track new offset"
        );
    }
}
