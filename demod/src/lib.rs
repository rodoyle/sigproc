//! Stateful narrowband demodulation, independent of transport and audio sinks.
//!
//! NBFM is the production default and AM/DSB is an optional carrier-present
//! mode. Both operate at the channelizer's native 50 kHz: one mono `f32` audio
//! sample per normalized IQ sample, with no resampling or output encoding.
//! Suppressed-carrier USB/SSB is deliberately a separate future mode.
pub mod recording;
pub mod service;
pub mod telemetry;

use sigproc_common::dsp::Iq;
use std::f64::consts::TAU;

/// Native IQ and audio rate for [`AmDemod`]. Callers must supply IQ at this rate.
pub const NATIVE_SAMPLE_RATE_HZ: u32 = 50_000;

/// A streaming IQ-to-mono-audio processor with an explicit stream lifecycle.
///
/// Implementations retain history across calls. Packet boundaries are not stream
/// boundaries: call [`Demod::reset`] only for a new stream or a discontinuity.
/// Input samples must be finite, normalized IQ. Output is linear floating-point
/// audio, not integer PCM; it need not fill or be limited to the range ±1.
/// This object-safe interface is also the extension seam for future NBFM DSP.
pub trait Demod: Send {
    /// Input IQ and output audio sample rate; no resampling is performed.
    fn sample_rate_hz(&self) -> u32;

    /// Return one audio sample per input sample. Empty input leaves state intact.
    fn process(&mut self, iq: &[Iq]) -> Vec<f32>;

    /// Discard all signal history, restoring the initial zero-state response.
    fn reset(&mut self);
}

/// AM magnitude detector with a 30 Hz DC blocker and a one-pole audio low-pass.
///
/// The DC blocker is `y[n] = x[n] - x[n-1] + r*y[n-1]`, where
/// `r = exp(-2*pi*30/50000)`. A following exponential smoother uses
/// `alpha = 1 - exp(-2*pi*5000/50000)` to attenuate high-frequency envelope noise.
/// Both filters retain history across blocks. Gain is fixed (no normalization or
/// clipping), and the initial carrier step produces a short settling transient.
pub struct AmDemod {
    dc_pole: f64,
    audio_alpha: f64,
    previous_envelope: f64,
    previous_dc_output: f64,
    previous_audio: f64,
}

impl AmDemod {
    pub fn new() -> Self {
        let rate = f64::from(NATIVE_SAMPLE_RATE_HZ);
        Self {
            dc_pole: (-TAU * 30.0 / rate).exp(),
            audio_alpha: 1.0 - (-TAU * 5_000.0 / rate).exp(),
            previous_envelope: 0.0,
            previous_dc_output: 0.0,
            previous_audio: 0.0,
        }
    }
}

impl Default for AmDemod {
    fn default() -> Self {
        Self::new()
    }
}

impl Demod for AmDemod {
    fn sample_rate_hz(&self) -> u32 {
        NATIVE_SAMPLE_RATE_HZ
    }

    fn process(&mut self, iq: &[Iq]) -> Vec<f32> {
        iq.iter()
            .map(|sample| {
                let envelope = f64::from(sample.magnitude());
                let dc_output =
                    envelope - self.previous_envelope + self.dc_pole * self.previous_dc_output;
                self.previous_envelope = envelope;
                self.previous_dc_output = dc_output;
                self.previous_audio += self.audio_alpha * (dc_output - self.previous_audio);
                self.previous_audio as f32
            })
            .collect()
    }

    fn reset(&mut self) {
        self.previous_envelope = 0.0;
        self.previous_dc_output = 0.0;
        self.previous_audio = 0.0;
    }
}

/// Default peak NBFM deviation for a 12.5 kHz-class voice channel.
pub const NBFM_DEVIATION_HZ: f64 = 2_500.0;

/// Quadrature NBFM discriminator with DC blocking and a 3 kHz audio low-pass.
///
/// The phase difference between adjacent complex samples is converted to
/// instantaneous frequency and normalized by the configured peak deviation.
/// A 30 Hz DC blocker removes static carrier-frequency error; it is not a
/// de-emphasis filter. Silence and the first sample produce zero audio.
pub struct NbfmDemod {
    deviation_hz: f64,
    dc_pole: f64,
    audio_alpha: f64,
    previous_iq: Option<Iq>,
    previous_frequency: f64,
    previous_dc_output: f64,
    previous_audio: f64,
}

impl NbfmDemod {
    pub fn new() -> Self {
        Self::with_deviation(NBFM_DEVIATION_HZ).expect("default NBFM deviation is valid")
    }

    pub fn with_deviation(deviation_hz: f64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            deviation_hz.is_finite() && deviation_hz > 0.0,
            "NBFM deviation must be finite and positive"
        );
        let rate = f64::from(NATIVE_SAMPLE_RATE_HZ);
        Ok(Self {
            deviation_hz,
            dc_pole: (-TAU * 30.0 / rate).exp(),
            audio_alpha: 1.0 - (-TAU * 3_000.0 / rate).exp(),
            previous_iq: None,
            previous_frequency: 0.0,
            previous_dc_output: 0.0,
            previous_audio: 0.0,
        })
    }
}

impl Default for NbfmDemod {
    fn default() -> Self {
        Self::new()
    }
}

impl Demod for NbfmDemod {
    fn sample_rate_hz(&self) -> u32 {
        NATIVE_SAMPLE_RATE_HZ
    }

    fn process(&mut self, iq: &[Iq]) -> Vec<f32> {
        iq.iter()
            .map(|&sample| {
                let previous = self.previous_iq.replace(sample);
                let Some(previous) = previous else {
                    return 0.0;
                };
                let cross = f64::from(previous.i * sample.q - previous.q * sample.i);
                let dot = f64::from(previous.i * sample.i + previous.q * sample.q);
                let frequency = if cross == 0.0 && dot == 0.0 {
                    0.0
                } else {
                    cross.atan2(dot) * f64::from(NATIVE_SAMPLE_RATE_HZ)
                        / (TAU * self.deviation_hz)
                };
                let dc_output = frequency - self.previous_frequency
                    + self.dc_pole * self.previous_dc_output;
                self.previous_frequency = frequency;
                self.previous_dc_output = dc_output;
                self.previous_audio += self.audio_alpha * (dc_output - self.previous_audio);
                self.previous_audio as f32
            })
            .collect()
    }

    fn reset(&mut self) {
        self.previous_iq = None;
        self.previous_frequency = 0.0;
        self.previous_dc_output = 0.0;
        self.previous_audio = 0.0;
    }
}
