//! Deterministic signal vectors, shared by the fixture generator, the gates and
//! the unit tests.
//!
//! Every gate in this repo has to assert on *known* input. Generating that input
//! here — rather than inside a test — means the fixture binary and the tests use
//! the same generator, so a gate failure can never be blamed on the test having
//! its own private idea of what the signal was.
//!
//! The modulation is plain AM/DSB: a carrier with its amplitude varied by a tone,
//! i.e. `A (1 + m cos(2 pi f_m t)) e^(j 2 pi f_c t)`. That is the signal an
//! envelope detector recovers, and it produces a baseband vector whose *envelope*
//! carries a single known tone — which is exactly what the verification gates
//! measure.

use crate::dsp::{iq_to_sc16, Iq};
use crate::vita49::build_packet;
use std::f64::consts::TAU;

/// Specification of a deterministic AM/DSB test signal.
#[derive(Debug, Clone, PartialEq)]
pub struct AmTone {
    /// Modulating tone frequency in Hz — what the envelope should contain.
    pub tone_hz: f64,
    /// Carrier offset from the wideband centre in Hz (may be negative).
    pub offset_hz: f64,
    /// Wideband sample rate in Hz.
    pub sample_rate: f64,
    /// Duration in seconds.
    pub seconds: f64,
    /// Modulation depth, 0.0..=1.0. Clamped: at depth > 1 the envelope inverts
    /// and the signal stops being plain AM/DSB, so an envelope detector would
    /// recover a doubled tone rather than the one asked for.
    pub depth: f64,
    /// Unmodulated carrier amplitude, 0.0..=1.0 of full scale.
    pub amplitude: f64,
}

impl AmTone {
    /// Number of whole complex samples in this vector.
    pub fn samples(&self) -> usize {
        (self.seconds * self.sample_rate).round().max(0.0) as usize
    }

    /// Peak envelope magnitude — the sc16 conversion clips above 1.0, so callers
    /// generating a fixture must keep this at or below full scale.
    pub fn peak_amplitude(&self) -> f64 {
        self.amplitude * (1.0 + self.depth.clamp(0.0, 1.0))
    }

    /// Generate the complex baseband vector.
    pub fn generate(&self) -> Vec<Iq> {
        let depth = self.depth.clamp(0.0, 1.0);
        let n = self.samples();
        (0..n)
            .map(|k| {
                let t = k as f64 / self.sample_rate;
                let envelope = self.amplitude * (1.0 + depth * (TAU * self.tone_hz * t).cos());
                let phase = TAU * self.offset_hz * t;
                // Envelope is real, so it scales both components: this is a
                // complex carrier amplitude-modulated in place, i.e. a channel
                // sitting at `offset_hz` with no image.
                Iq::new(
                    (envelope * phase.cos()) as f32,
                    (envelope * phase.sin()) as f32,
                )
            })
            .collect()
    }
}

/// Serialize IQ as a VITA49 packet stream (the fixture file format: concatenated
/// packets, no container header — the size field in each packet is the framing).
///
/// Timestamps advance by exactly one packet's worth of samples, as UHD's do, so
/// the stream's sample counter is continuous and a consumer's gap detection sees
/// zero gaps for an intact file.
///
/// Returns `(bytes, packet_count)`.
pub fn vita49_stream(
    iq: &[Iq],
    sample_rate: f64,
    samples_per_packet: usize,
    start_secs: f64,
) -> (Vec<u8>, usize) {
    assert!(
        samples_per_packet > 0,
        "packets must carry at least one sample"
    );
    let interleaved = iq_to_sc16(iq);
    let mut out = Vec::new();
    let mut packets = 0usize;

    for (i, chunk) in interleaved.chunks(samples_per_packet * 2).enumerate() {
        // A trailing half pair cannot be framed; iq_to_sc16 always emits pairs,
        // so this only trips if `samples_per_packet` were misused.
        debug_assert!(chunk.len().is_multiple_of(2));
        let first_sample = i * samples_per_packet;
        let ts = start_secs + first_sample as f64 / sample_rate;
        out.extend_from_slice(&build_packet(chunk, ts, sample_rate));
        packets += 1;
    }

    (out, packets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{envelope, estimate_frequency};
    use crate::vita49::PacketCursor;

    fn spec() -> AmTone {
        AmTone {
            tone_hz: 1000.0,
            offset_hz: 200_000.0,
            sample_rate: 2_000_000.0,
            seconds: 0.05,
            depth: 0.5,
            amplitude: 0.5,
        }
    }

    #[test]
    fn sample_count_is_the_requested_duration() {
        assert_eq!(spec().samples(), 100_000);
        let mut s = spec();
        s.seconds = 0.0;
        assert_eq!(s.samples(), 0);
        assert!(s.generate().is_empty());
    }

    #[test]
    fn the_envelope_carries_the_requested_modulation() {
        let s = spec();
        let iq = s.generate();
        let env = envelope(&iq);
        let peak = env.iter().map(|v| v.i).fold(f32::MIN, f32::max) as f64;
        let trough = env.iter().map(|v| v.i).fold(f32::MAX, f32::min) as f64;
        // A (1 + m) and A (1 - m).
        assert!((peak - 0.75).abs() < 1e-3, "peak {peak}");
        assert!((trough - 0.25).abs() < 1e-3, "trough {trough}");
    }

    /// The generator and the DSP must agree on the same two facts the gate
    /// asserts: where the carrier sits, and what the envelope contains.
    #[test]
    fn the_generator_agrees_with_the_estimators_about_the_signal() {
        let s = spec();
        let iq = s.generate();

        let carrier = estimate_frequency(&iq, s.sample_rate, 100_000.0, 400_000.0)
            .expect("carrier offset found");
        assert!(
            (carrier - s.offset_hz).abs() < 5.0,
            "carrier at {carrier} Hz, expected {}",
            s.offset_hz
        );

        let env = envelope(&iq);
        let tone = estimate_frequency(&env, s.sample_rate, 50.0, 5_000.0).expect("tone found");
        assert!((tone - s.tone_hz).abs() < 0.5, "tone at {tone} Hz");
    }

    #[test]
    fn a_negative_offset_generates_a_channel_below_the_centre() {
        let mut s = spec();
        s.offset_hz = -200_000.0;
        let iq = s.generate();
        let carrier = estimate_frequency(&iq, s.sample_rate, -400_000.0, -100_000.0)
            .expect("carrier found below the centre");
        assert!(
            (carrier - s.offset_hz).abs() < 5.0,
            "carrier at {carrier} Hz"
        );
    }

    #[test]
    fn depth_is_clamped_because_over_modulation_is_not_plain_am() {
        let mut s = spec();
        s.depth = 3.0;
        assert_eq!(s.peak_amplitude(), s.amplitude * 2.0);
        let env = envelope(&s.generate());
        assert!(
            env.iter().all(|v| v.i >= 0.0),
            "an over-modulated envelope inverts; clamping keeps it non-negative"
        );
    }

    #[test]
    fn peak_amplitude_exposes_the_clipping_risk_to_callers() {
        let mut s = spec();
        assert!((s.peak_amplitude() - 0.75).abs() < 1e-12, "0.5 x (1 + 0.5)");
        s.amplitude = 0.8;
        s.depth = 0.5;
        assert!(
            s.peak_amplitude() > 1.0,
            "callers must be able to refuse this"
        );
    }

    #[test]
    fn a_packet_stream_round_trips_and_has_a_continuous_sample_counter() {
        let s = spec();
        let iq = s.generate();
        let (bytes, packets) = vita49_stream(&iq, s.sample_rate, 2048, 0.0);
        assert_eq!(packets, 49, "100 000 samples in 2048-sample packets");

        let mut cursor = PacketCursor::new(&bytes);
        let mut payload = Vec::new();
        let mut counters = Vec::new();
        while let Some(next) = cursor.next_packet() {
            let pkt = next.expect("parses");
            counters.push(pkt.sample_counter(s.sample_rate));
            payload.extend_from_slice(&pkt.iq_i16());
        }
        assert!(cursor.is_exhausted());
        assert_eq!(payload.len(), iq.len() * 2, "every sample is present");

        // Exactly one packet's worth of samples between packets, with no loss —
        // which is what makes a nonzero seq_gaps count meaningful later.
        for pair in counters.windows(2) {
            assert_eq!(pair[1] - pair[0], 2048);
        }
        assert_eq!(counters[0], 0);
    }

    #[test]
    fn a_stream_starting_at_a_nonzero_time_starts_there() {
        let s = spec();
        let (bytes, _) = vita49_stream(&s.generate(), s.sample_rate, 1024, 12.0);
        let mut cursor = PacketCursor::new(&bytes);
        let first = cursor.next_packet().expect("first").expect("parses");
        assert_eq!(first.sample_counter(s.sample_rate), 12 * 2_000_000);
    }

    /// The named gate from the verification contract: generate a fixture the way
    /// the binary does, then prove the sample count, rate and timestamps
    /// round-trip exactly through the shared parser.
    #[test]
    fn fixture_roundtrip() {
        let s = spec();
        let expected_samples = s.samples();
        let (bytes, packets) = vita49_stream(&s.generate(), s.sample_rate, 2048, 0.0);

        let mut cursor = PacketCursor::new(&bytes);
        let mut samples = 0usize;
        let mut first_ts = None;
        let mut last_ts = 0u64;
        while let Some(next) = cursor.next_packet() {
            let pkt = next.expect("every packet parses");
            samples += pkt.samples();
            let counter = pkt.sample_counter(s.sample_rate);
            first_ts.get_or_insert(counter);
            last_ts = counter;
        }
        assert!(cursor.is_exhausted());
        assert_eq!(
            samples, expected_samples,
            "sample count is preserved exactly"
        );
        assert_eq!(packets, 49);
        assert_eq!(first_ts, Some(0), "the first packet starts at t=0");
        // The final packet's counter plus its payload lands exactly on the end.
        let last_samples = expected_samples - (packets - 1) * 2048;
        assert_eq!(
            last_ts + last_samples as u64,
            expected_samples as u64,
            "timestamps account for every sample, with no drift"
        );
    }
}
