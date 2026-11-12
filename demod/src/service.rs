//! Single-stream native 50 kHz ingress; common data parser and timestamp GapTracker.
use crate::recording::{Boundary, Recorder};
use crate::telemetry::Telemetry;
use crate::{Demod, NbfmDemod, NATIVE_SAMPLE_RATE_HZ};
use sigproc_common::dsp::{estimate_frequency_keeping_dc, power_dbfs, sc16_to_iq, GapTracker, Iq};
use sigproc_common::vita49::{self, StreamContext};
use std::net::UdpSocket;
use std::sync::{
    atomic::{AtomicBool, Ordering::Relaxed},
    Arc,
};
use std::time::Duration;

/// Only the exact fixed-layout context emitted by common::build_context_packet.
/// This is intentionally not a general VITA49 context implementation.
fn native_context(bytes: &[u8]) -> Option<(u32, StreamContext)> {
    if bytes.len() != vita49::CONTEXT_PACKET_BYTES {
        return None;
    }
    let word = |at| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    let q20 = |at| i64::from_be_bytes(bytes[at..at + 8].try_into().unwrap()) as f64 / 1_048_576.0;
    if word(0) & !(15 << 16) != 0x40d0000f
        || u64::from_be_bytes(bytes[12..20].try_into().unwrap()) >= 50_000
    {
        return None;
    }
    let context = StreamContext {
        bandwidth_hz: q20(24),
        rf_freq_hz: q20(32),
        gain_db: f64::from(word(40) as u16 as i16) / 128.0,
        sample_rate_hz: q20(44),
    };
    if context.sample_rate_hz != f64::from(NATIVE_SAMPLE_RATE_HZ)
        || context.bandwidth_hz <= 0.0
        || context.bandwidth_hz > 50_000.0
        || context.rf_freq_hz < 0.0
    {
        return None;
    }
    let canonical = vita49::build_context_packet(&context, 0.0, word(4), 0, word(20) >> 31 != 0);
    if canonical[20..] != bytes[20..] {
        return None;
    }
    Some((word(4), context))
}
pub struct DemodService {
    recorder: Recorder,
    metrics: Arc<Telemetry>,
    context: Option<(u32, StreamContext)>,
    demod: Box<dyn Demod>,
    tracker: GapTracker,
    measure: Vec<Iq>,
    output_samples: u64,
    boundary: Option<Boundary>,
    stopped: bool,
}
impl DemodService {
    /// Construct the production default: NBFM at the standard 2.5 kHz deviation.
    pub fn new(recorder: Recorder, metrics: Arc<Telemetry>) -> Self {
        Self::with_demod(recorder, metrics, Box::new(NbfmDemod::new()), "nbfm")
    }

    /// Select a DSP implementation explicitly; AM tests and optional AM service
    /// deployments use this seam rather than changing transport or recording.
    pub fn with_demod(
        recorder: Recorder,
        metrics: Arc<Telemetry>,
        demod: Box<dyn Demod>,
        mode: &'static str,
    ) -> Self {
        let mut recorder = recorder;
        recorder.set_mode(mode);
        Self {
            recorder,
            metrics,
            context: None,
            demod,
            tracker: GapTracker::new(),
            measure: Vec::with_capacity(4096),
            output_samples: 0,
            boundary: None,
            stopped: false,
        }
    }
    fn interrupt(
        &mut self,
        kind: &str,
        counter: Option<u64>,
        missing: Option<u64>,
    ) -> anyhow::Result<()> {
        self.demod.reset();
        self.measure.clear();
        self.metrics.clear_measurement();
        self.boundary = Some(Boundary {
            kind: kind.into(),
            input_sample: counter,
            missing_samples: missing,
            output_sample: self.output_samples,
            recording_sample: 0,
        });
        self.recorder.finish(kind)
    }
    /// Rejects are nonfatal; recording IO failures are fatal and latch the service closed.
    pub fn process_datagram(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(!self.stopped, "demod service is closed");
        self.metrics.received.fetch_add(1, Relaxed);
        let result = self.process(bytes);
        if result.is_err() {
            self.stopped = true;
            self.metrics.recording_errors.fetch_add(1, Relaxed);
        }
        result
    }
    fn reject(&mut self) -> anyhow::Result<()> {
        self.metrics.rejected.fetch_add(1, Relaxed);
        self.interrupt("rejected_datagram", None, None)
    }
    fn process(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        // Extended context (type 5) is unsupported and also revokes stale acceptance.
        if matches!(bytes.first().map(|b| b >> 4), Some(4) | Some(5)) {
            let next = native_context(bytes);
            if next.is_none() || next != self.context {
                // Clear acceptance before any fallible finalization; never keep stale metadata.
                self.context = None;
                self.metrics.context_valid.store(0, Relaxed);
                self.interrupt(
                    if next.is_some() {
                        "context_change"
                    } else {
                        "invalid_context"
                    },
                    None,
                    None,
                )?;
                self.tracker = GapTracker::new();
            }
            if let Some(context) = next {
                self.context = Some(context);
                self.metrics.context_valid.store(1, Relaxed);
            } else {
                self.metrics.rejected.fetch_add(1, Relaxed);
            }
            return Ok(());
        }
        let Some((stream_id, context)) = self.context else {
            return self.reject();
        };
        let packet = match vita49::parse(bytes) {
            Ok(packet) => packet,
            Err(_) => return self.reject(),
        };
        let header = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        if header & 0xfff00000 != 0x10d00000
            || packet.stream_id != stream_id
            || packet.samples() == 0
            || packet.ts_frac >= 50_000
        {
            return self.reject();
        }
        let counter = packet.sample_counter(50_000.0);
        let old_resets = self.tracker.resets();
        if let Some(gap) = self.tracker.observe(counter, packet.samples() as u64) {
            self.metrics.gaps.fetch_add(1, Relaxed);
            self.metrics
                .gap_samples
                .fetch_add(gap.missing_samples, Relaxed);
            self.interrupt("forward_gap", Some(counter), Some(gap.missing_samples))?;
        } else if self.tracker.resets() != old_resets {
            self.metrics.resets.fetch_add(1, Relaxed);
            self.interrupt("backward_timestamp", Some(counter), None)?;
        }
        let iq = sc16_to_iq(&packet.iq_i16());
        // Non-overlapping bounded blocks; never FFT once per UDP packet.
        for &sample in &iq {
            self.measure.push(sample);
            if self.measure.len() == 4096 {
                let power = power_dbfs(&self.measure);
                let offset =
                    estimate_frequency_keeping_dc(&self.measure, 50_000.0, -25_000.0, 25_000.0);
                self.metrics.measure(power, offset);
                self.measure.clear();
            }
        }
        let audio = self.demod.process(&iq);
        self.recorder.write(
            &audio,
            stream_id,
            context.rf_freq_hz,
            counter,
            self.output_samples,
            self.boundary.take(),
        )?;
        self.output_samples += audio.len() as u64;
        self.metrics.samples.fetch_add(audio.len() as u64, Relaxed);
        Ok(())
    }
    pub fn finish(&mut self, reason: &str) -> anyhow::Result<()> {
        self.stopped = true;
        let result = self.recorder.finish(reason);
        if result.is_err() {
            self.metrics.recording_errors.fetch_add(1, Relaxed);
        }
        result
    }
}
/// Polls shutdown even under continuous traffic. Fatal receive errors finalize before returning.
pub fn run_udp(
    socket: UdpSocket,
    service: &mut DemodService,
    shutdown: &AtomicBool,
) -> anyhow::Result<()> {
    let mut bytes = [0; 65_536];
    let result = (|| -> anyhow::Result<()> {
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        while !shutdown.load(Relaxed) {
            match socket.recv_from(&mut bytes) {
                Ok((count, _)) => service.process_datagram(&bytes[..count])?,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();
    let finalized = service.finish(if result.is_ok() {
        "shutdown"
    } else {
        "service_error"
    });
    result.and(finalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_extended_context_revokes_native_context_acceptance() {
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/demod-tests")
            .join(format!("extended-context-{}", std::process::id()));
        let metrics = Arc::new(Telemetry::default());
        let recorder = Recorder::new(directory.clone(), 1000, 2).unwrap();
        let mut service = DemodService::new(recorder, metrics.clone());
        let context = StreamContext {
            rf_freq_hz: 1.0,
            sample_rate_hz: 50_000.0,
            bandwidth_hz: 20_000.0,
            gain_db: 0.0,
        };
        let mut packet = vita49::build_context_packet(&context, 0.0, 7, 0, true);
        service.process_datagram(&packet).unwrap();
        assert_eq!(metrics.context_valid.load(Relaxed), 1);
        packet[0] = (packet[0] & 15) | 0x50;
        service.process_datagram(&packet).unwrap();
        assert_eq!(metrics.context_valid.load(Relaxed), 0);
        service
            .process_datagram(&vita49::build_data_packet(&[8192, 0], 0.0, 50_000.0, 7, 0))
            .unwrap();
        assert_eq!(metrics.samples.load(Relaxed), 0);
        service.finish("test").unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
