//! Demod-specific semantics, without reusing the wideband/channelizer labels.
use sigproc_common::metrics::MetricsRenderer;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
#[derive(Default)]
pub struct Telemetry {
    pub received: AtomicU64,
    pub rejected: AtomicU64,
    pub gaps: AtomicU64,
    pub gap_samples: AtomicU64,
    pub resets: AtomicU64,
    pub samples: AtomicU64,
    pub recording_errors: AtomicU64,
    pub context_valid: AtomicU64,
    gauges: std::sync::Mutex<Option<(f32, Option<f64>)>>,
}
impl Telemetry {
    pub fn clear_measurement(&self) {
        *self.gauges.lock().unwrap() = None;
    }
    pub fn measure(&self, power: f32, offset: Option<f64>) {
        *self.gauges.lock().unwrap() = Some((power, offset));
    }
}
impl MetricsRenderer for Telemetry {
    fn render_metrics(&self, prefix: &str) -> String {
        let mut text = String::new();
        for (name, help, value) in [
            (
                "vita49_packets_received_total",
                "UDP datagrams received, including context and rejects",
                &self.received,
            ),
            (
                "vita49_packets_dropped_total",
                "Locally rejected datagrams, not estimated network packet loss",
                &self.rejected,
            ),
            (
                "vita49_seq_gaps_total",
                "Forward sample timestamp discontinuities, not 4-bit sequence loss",
                &self.gaps,
            ),
            (
                "vita49_gap_samples_total",
                "Missing samples inferred from forward timestamps",
                &self.gap_samples,
            ),
            (
                "vita49_resets_total",
                "Backward timestamps including duplicate or reordered data",
                &self.resets,
            ),
            (
                "samples_out_total",
                "Demodulated audio samples successfully written",
                &self.samples,
            ),
            (
                "recording_errors_total",
                "Fatal recording IO errors",
                &self.recording_errors,
            ),
        ] {
            text.push_str(&format!("# HELP {prefix}_{name} {help}\n# TYPE {prefix}_{name} counter\n{prefix}_{name} {}\n", value.load(Relaxed)));
        }
        let gauges = self.gauges.lock().unwrap();
        let (power, offset) = gauges.unwrap_or((f32::NAN, None));
        for (name, help, value) in [
            ("rssi_dbfs", "Latest contiguous 4096-sample IQ mean power; not calibrated dBm", f64::from(power)),
            ("frequency_offset_estimate_hz", "Strongest IQ spectral peak including DC/carrier, not modulation tone; native window resolution ~12.2 Hz", offset.unwrap_or(f64::NAN)),
            ("snr_db", "Unavailable: no validated signal/noise separation estimator", f64::NAN),
            ("snr_available", "Zero until a validated SNR estimator exists", 0.0),
            ("context_valid", "One after supported native context, zero otherwise", self.context_valid.load(Relaxed) as f64),
        ] {
            let rendered = if value == f64::INFINITY { "+Inf".into() }
                else if value == f64::NEG_INFINITY { "-Inf".into() } else { value.to_string() };
            text.push_str(&format!("# HELP {prefix}_{name} {help}\n# TYPE {prefix}_{name} gauge\n{prefix}_{name} {rendered}\n"));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn telemetry_silence_uses_prometheus_infinity_not_an_invalid_float_token() {
        let metrics = Telemetry::default();
        metrics.measure(f32::NEG_INFINITY, None);
        let text = metrics.render_metrics("demod");
        assert!(text.contains("demod_rssi_dbfs -Inf\n"));
        assert!(text.contains("demod_frequency_offset_estimate_hz NaN\n"));
        assert!(text.contains("demod_snr_db NaN\n"));
        assert!(!text.contains(" -inf\n"));
    }
}
