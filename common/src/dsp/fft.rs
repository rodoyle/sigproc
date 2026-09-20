//! Radix-2 Cooley-Tukey FFT, used for spectral measurement (peak channel
//! offset, recovered tone frequency) rather than for filtering.
//!
//! This exists because the naive approach — evaluating a DFT at a few hundred
//! candidate frequencies per metrics update — is O(frequencies x samples) and
//! costs hundreds of milliseconds on a 50 kS/s block. An FFT turns the same
//! measurement into single-digit milliseconds, which is what makes a
//! per-second metrics cadence affordable.
//!
//! f64 throughout: the recurrences below accumulate twiddle rotation over
//! N/2 steps, and f32 drift is enough to blur a spectral peak.

use std::f64::consts::PI;

/// Forward FFT, in place: `X[k] = sum_n x[n] e^(-j 2 pi k n / N)`.
///
/// `re` and `im` must have the same length, and that length must be a power of
/// two (the caller zero-pads). Panics otherwise — a wrong length is a caller
/// bug, not a runtime condition to paper over.
pub fn fft_in_place(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    assert_eq!(n, im.len(), "re and im must have the same length");
    if n <= 1 {
        return;
    }
    assert!(
        n.is_power_of_two(),
        "FFT length must be a power of two, got {n} (zero-pad instead)"
    );

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    // Butterflies, doubling the sub-transform length each pass.
    let mut len = 2usize;
    while len <= n {
        let angle = -2.0 * PI / len as f64;
        let (wr, wi) = (angle.cos(), angle.sin());
        let half = len / 2;
        let mut start = 0usize;
        while start < n {
            let mut cur_r = 1.0f64;
            let mut cur_i = 0.0f64;
            for k in 0..half {
                let i = start + k;
                let m = start + k + half;
                let tr = re[m] * cur_r - im[m] * cur_i;
                let ti = re[m] * cur_i + im[m] * cur_r;
                re[m] = re[i] - tr;
                im[m] = im[i] - ti;
                re[i] += tr;
                im[i] += ti;
                // Advance the twiddle factor by one step.
                let nr = cur_r * wr - cur_i * wi;
                cur_i = cur_r * wi + cur_i * wr;
                cur_r = nr;
            }
            start += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independent O(N^2) reference so the FFT is checked against the
    /// definition, not against itself.
    fn naive_dft(re: &[f64], im: &[f64], k: usize) -> (f64, f64) {
        let n = re.len();
        let mut sr = 0.0;
        let mut si = 0.0;
        for i in 0..n {
            let ang = -2.0 * PI * (k * i) as f64 / n as f64;
            let (s, c) = ang.sin_cos();
            sr += re[i] * c - im[i] * s;
            si += re[i] * s + im[i] * c;
        }
        (sr, si)
    }

    /// Deterministic pseudo-random input (no rng dependency).
    fn pseudo(n: usize) -> (Vec<f64>, Vec<f64>) {
        let mut re = Vec::with_capacity(n);
        let mut im = Vec::with_capacity(n);
        let mut state = 12345u64;
        for _ in 0..n {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            re.push(((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0);
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            im.push(((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0);
        }
        (re, im)
    }

    #[test]
    fn matches_the_naive_dft_definition() {
        for n in [2usize, 4, 8, 16, 64] {
            let (mut re, mut im) = pseudo(n);
            let (want_re, want_im) = (re.clone(), im.clone());
            fft_in_place(&mut re, &mut im);
            for k in 0..n {
                let (er, ei) = naive_dft(&want_re, &want_im, k);
                assert!(
                    (re[k] - er).abs() < 1e-9 && (im[k] - ei).abs() < 1e-9,
                    "n={n} k={k}: got ({}, {}), want ({er}, {ei})",
                    re[k],
                    im[k]
                );
            }
        }
    }

    #[test]
    fn a_pure_tone_lands_in_the_expected_bin() {
        let n = 64usize;
        let k0 = 7usize;
        let mut re = vec![0.0; n];
        let mut im = vec![0.0; n];
        for k in 0..n {
            let ang = 2.0 * PI * (k0 * k) as f64 / n as f64;
            re[k] = ang.cos();
            im[k] = ang.sin();
        }
        fft_in_place(&mut re, &mut im);
        let mag = |k: usize| (re[k] * re[k] + im[k] * im[k]).sqrt();
        // e^(+j2pi k0 n/N) transforms to a single N-magnitude bin at k0.
        assert!((mag(k0) - n as f64).abs() < 1e-6, "peak at k0={k0}");
        for k in 0..n {
            if k != k0 {
                assert!(mag(k) < 1e-6, "leakage at bin {k}");
            }
        }
    }

    #[test]
    fn lengths_below_two_are_no_ops() {
        let mut re = [3.0];
        let mut im = [-1.0];
        fft_in_place(&mut re, &mut im);
        assert_eq!((re[0], im[0]), (3.0, -1.0));
    }
}
