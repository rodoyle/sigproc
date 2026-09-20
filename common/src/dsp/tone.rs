//! Frequency measurement: where is the tone?
//!
//! Two questions in this chain need the same machinery, and both are answered
//! from a spectrum rather than by ear:
//!
//! * the channelizer's `peak_offset_hz` — is the channel we isolated centred on
//!   the signal we meant to isolate?
//! * the sink's `decoded_hz` — did the AM envelope we recovered actually carry
//!   the 1 kHz tone the fixture modulated?
//!
//! Both use the same estimator: mean-remove, Hann-window, zero-padded FFT, peak
//! bin, parabolic interpolation on log magnitude. The window matters (a
//! rectangular window's leakage skirts shift the peak of a weak neighbouring
//! component) and the zero-padding plus interpolation is what buys sub-bin
//! accuracy without a long transform.

use super::fft::fft_in_place;
use super::iq::Iq;
use std::f64::consts::PI;

/// How far below the strongest component anywhere in the spectrum the in-band
/// peak may sit before the band is treated as empty.
///
/// A windowed spectrum always has leakage, so without this a band containing no
/// signal at all still yields a confident-looking frequency (measured: a tone at
/// -2 kHz produces a "found" 100.7 Hz in the positive band). A verification
/// artifact that reports a fabricated tone when the channel is empty is worse
/// than one that reports nothing, so this threshold makes "no signal in this
/// band" an explicit answer. 40 dB is deliberately loose: a real in-band signal
/// is normally near the top of the spectrum, while leakage from a distant strong
/// component is far below it.
const BAND_SIGNIFICANCE_DB: f64 = 40.0;

/// Estimate the dominant frequency in `signal` within `[min_hz, max_hz]`.
///
/// The input is complex, which covers both uses: pass the signal itself to find
/// its carrier offset, or pass [`super::iq::envelope`] of it (which has `q == 0`)
/// to find the modulation tone.
///
/// The band is **signed**, because for complex input a negative frequency is a
/// distinct, real thing: a tone below the channel centre must be reported below
/// zero, not folded to a positive magnitude. Pass `[-hi, -lo]` to search below
/// DC. Callers that only care about magnitude fold it themselves.
///
/// Returns `None` when the band is empty, the signal is too short to measure, or
/// nothing in the band carries energy.
pub fn estimate_frequency(
    signal: &[Iq],
    sample_rate: f64,
    min_hz: f64,
    max_hz: f64,
) -> Option<f64> {
    let n = signal.len();
    if n < 8 || sample_rate <= 0.0 || max_hz <= min_hz {
        return None;
    }

    // Mean removal: a DC term is a real peak at bin 0 and would otherwise
    // dominate the search when the band starts near DC.
    let mean_i = signal.iter().map(|s| s.i as f64).sum::<f64>() / n as f64;
    let mean_q = signal.iter().map(|s| s.q as f64).sum::<f64>() / n as f64;

    // Zero-pad to at least twice the length: halves the bin width, and the
    // parabolic interpolation below then resolves well below it.
    let fft_len = (n * 2).next_power_of_two();
    let mut re = vec![0.0f64; fft_len];
    let mut im = vec![0.0f64; fft_len];
    let window_denom = (n - 1).max(1) as f64;
    for (k, s) in signal.iter().enumerate() {
        // Hann window.
        let w = 0.5 - 0.5 * (2.0 * PI * k as f64 / window_denom).cos();
        re[k] = (s.i as f64 - mean_i) * w;
        im[k] = (s.q as f64 - mean_q) * w;
    }

    fft_in_place(&mut re, &mut im);

    let bin_hz = sample_rate / fft_len as f64;
    let half = (fft_len / 2) as i64;
    let to_bin = |f: f64| (f / bin_hz).round() as i64;
    let k_lo = to_bin(min_hz).clamp(-half, half);
    let k_hi = to_bin(max_hz).clamp(-half, half);
    if k_lo > k_hi {
        return None;
    }

    // Signed bin -> FFT index: negative frequencies live in the upper half.
    let index = |k: i64| -> usize {
        if k >= 0 {
            k as usize
        } else {
            (fft_len as i64 + k) as usize
        }
    };
    let mag = |k: i64| {
        let i = index(k);
        (re[i] * re[i] + im[i] * im[i]).sqrt()
    };

    let mut best = k_lo;
    let mut best_mag = mag(k_lo);
    for k in (k_lo + 1)..=k_hi {
        let m = mag(k);
        if m > best_mag {
            best = k;
            best_mag = m;
        }
    }
    if best_mag <= 0.0 {
        return None;
    }

    // Reject a band that holds only leakage from a component somewhere else.
    let mut global_max = 0.0f64;
    for k in 0..=half {
        let m = mag(k);
        if m > global_max {
            global_max = m;
        }
    }
    if 20.0 * (best_mag / global_max.max(f64::MIN_POSITIVE)).log10() < -BAND_SIGNIFICANCE_DB {
        return None;
    }

    // Parabolic interpolation on log magnitude (the log makes the main lobe
    // near-parabolic for a Hann window).
    let (y0, y1, y2) = (
        mag(best - 1).max(1e-30).ln(),
        best_mag.max(1e-30).ln(),
        mag(best + 1).max(1e-30).ln(),
    );
    let denom = y0 - 2.0 * y1 + y2;
    let delta = if denom.abs() < 1e-12 {
        0.0
    } else {
        (0.5 * (y0 - y2) / denom).clamp(-1.0, 1.0)
    };

    Some((best as f64 + delta) * bin_hz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::iq::envelope;

    fn complex_tone(freq: f64, sample_rate: f64, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|k| {
                let ang = 2.0 * PI * freq * k as f64 / sample_rate;
                Iq::new(ang.cos() as f32, ang.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn recovers_a_complex_tone_offset_to_sub_hertz() {
        let rate = 50_000.0;
        let sig = complex_tone(1234.5, rate, 8192);
        let got = estimate_frequency(&sig, rate, 100.0, 5000.0).expect("tone found");
        assert!((got - 1234.5).abs() < 0.5, "got {got} Hz");
    }

    #[test]
    fn recovers_the_am_modulation_tone_from_the_envelope() {
        // 50% AM on a DC carrier: envelope = 1 + 0.5 cos(2 pi 1000 t).
        let rate = 50_000.0;
        let iq: Vec<Iq> = (0..20_000)
            .map(|k| {
                let t = k as f64 / rate;
                let amp = 1.0 + 0.5 * (2.0 * PI * 1000.0 * t).cos();
                Iq::new(amp as f32, 0.0)
            })
            .collect();
        let env = envelope(&iq);
        let got = estimate_frequency(&env, rate, 50.0, 5000.0).expect("tone found");
        assert!((got - 1000.0).abs() < 0.5, "got {got} Hz");
    }

    #[test]
    fn negative_offsets_are_reported_with_their_sign() {
        // For complex input a tone below the centre is a real, distinct thing:
        // reporting it as +2 kHz would hide which side of the channel it is on.
        let rate = 50_000.0;
        let sig = complex_tone(-2000.0, rate, 8192);
        let got = estimate_frequency(&sig, rate, -5000.0, -100.0).expect("tone found");
        assert!((got - (-2000.0)).abs() < 0.5, "got {got} Hz");
        // And it is absent from the positive band.
        assert_eq!(estimate_frequency(&sig, rate, 100.0, 5000.0), None);
    }

    #[test]
    fn rejects_degenerate_inputs_instead_of_returning_a_lie() {
        let rate = 50_000.0;
        assert_eq!(estimate_frequency(&[], rate, 100.0, 1000.0), None);
        assert_eq!(
            estimate_frequency(&[Iq::ZERO; 4], rate, 100.0, 1000.0),
            None
        );
        // Empty band.
        assert_eq!(
            estimate_frequency(&complex_tone(1000.0, rate, 1024), rate, 5000.0, 1000.0),
            None
        );
        // Pure silence carries no peak.
        assert_eq!(
            estimate_frequency(&[Iq::ZERO; 1024], rate, 100.0, 1000.0),
            None
        );
    }

    #[test]
    fn picks_the_strongest_of_two_tones() {
        let rate = 50_000.0;
        let n = 16_384;
        let sig: Vec<Iq> = (0..n)
            .map(|k| {
                let t = k as f64 / rate;
                let weak = 0.2 * (2.0 * PI * 800.0 * t).cos();
                let strong = 1.0 * (2.0 * PI * 3000.0 * t).cos();
                Iq::new((weak + strong) as f32, 0.0)
            })
            .collect();
        let got = estimate_frequency(&sig, rate, 100.0, 6000.0).expect("tone found");
        assert!((got - 3000.0).abs() < 1.0, "got {got} Hz");
    }
}
