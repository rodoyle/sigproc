//! Collector-only metrics. Atomics keep the HTTP thread off the UHD owner.
use sigproc_common::{metrics::MetricsRenderer, vita49::ForwarderStats};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

#[derive(Default)]
struct Destination {
    packets: AtomicU64,
    bytes: AtomicU64,
    dropped: AtomicU64,
}

impl Destination {
    fn update(&self, stats: ForwarderStats) {
        self.packets.store(stats.packets_sent, Relaxed);
        self.bytes.store(stats.bytes_sent, Relaxed);
        self.dropped.store(stats.packets_dropped, Relaxed);
    }
}

#[derive(Default)]
pub struct CaptureMetrics {
    rate_bits: AtomicU64,
    samples: AtomicU64,
    receive_errors: AtomicU64,
    empty_reads: AtomicU64,
    controls: AtomicU64,
    multicast_enabled: AtomicBool,
    primary: Destination,
    multicast: Destination,
}

impl CaptureMetrics {
    pub fn set_rate(&self, rate: f64) {
        self.rate_bits.store(rate.to_bits(), Relaxed);
    }
    pub fn samples(&self, n: usize) {
        self.samples.fetch_add(n as u64, Relaxed);
    }
    pub fn receive_error(&self) {
        self.receive_errors.fetch_add(1, Relaxed);
    }
    pub fn empty_read(&self) {
        self.empty_reads.fetch_add(1, Relaxed);
    }
    pub fn control(&self) {
        self.controls.fetch_add(1, Relaxed);
    }
    pub fn primary(&self, stats: ForwarderStats) {
        self.primary.update(stats);
    }
    pub fn multicast(&self, stats: ForwarderStats) {
        self.multicast_enabled.store(true, Relaxed);
        self.multicast.update(stats);
    }
}

impl MetricsRenderer for CaptureMetrics {
    fn render_metrics(&self, prefix: &str) -> String {
        let mut text = String::new();
        let mut metric = |name: &str, kind: &str, help: &str, value: String| {
            text.push_str(&format!("# HELP {prefix}_{name} {help}\n# TYPE {prefix}_{name} {kind}\n{prefix}_{name} {value}\n"));
        };
        metric(
            "sample_rate_hz",
            "gauge",
            "Actual UHD receive sample rate in samples per second",
            f64::from_bits(self.rate_bits.load(Relaxed)).to_string(),
        );
        metric(
            "samples_received_total",
            "counter",
            "Complex samples read from UHD",
            self.samples.load(Relaxed).to_string(),
        );
        metric(
            "receive_errors_total",
            "counter",
            "UHD receive call or metadata errors",
            self.receive_errors.load(Relaxed).to_string(),
        );
        metric(
            "empty_reads_total",
            "counter",
            "UHD receive calls returning no samples",
            self.empty_reads.load(Relaxed).to_string(),
        );
        metric(
            "control_datagrams_total",
            "counter",
            "Authorized control datagrams processed, including malformed requests",
            self.controls.load(Relaxed).to_string(),
        );
        metric(
            "multicast_enabled",
            "gauge",
            "Whether the multicast destination is configured",
            u8::from(self.multicast_enabled.load(Relaxed)).to_string(),
        );
        for (destination, stats) in [("primary", &self.primary), ("multicast", &self.multicast)] {
            for (name, help, value) in [
                ("packets_sent_total", "VITA49 data and context datagrams accepted by the local UDP socket", stats.packets.load(Relaxed)),
                ("bytes_sent_total", "VITA49 payload bytes accepted by the local UDP socket, excluding network headers", stats.bytes.load(Relaxed)),
                ("packets_dropped_total", "VITA49 datagrams not sent due to resolution or socket errors", stats.dropped.load(Relaxed)),
            ] {
                // Fixed destination labels avoid unbounded cardinality or exposing addresses.
                text.push_str(&format!("# HELP {prefix}_{destination}_{name} {help}\n# TYPE {prefix}_{destination}_{name} counter\n{prefix}_{destination}_{name} {value}\n"));
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::Arc,
    };

    #[test]
    fn real_http_exposes_actual_rate_and_per_destination_accounting() {
        let m = Arc::new(CaptureMetrics::default());
        m.set_rate(250_000.0);
        m.samples(12);
        m.receive_error();
        m.empty_read();
        m.control();
        m.primary(ForwarderStats {
            packets_sent: 2,
            bytes_sent: 100,
            packets_dropped: 1,
        });
        m.multicast(ForwarderStats {
            packets_sent: 1,
            bytes_sent: 40,
            packets_dropped: 0,
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let _server =
            sigproc_common::metrics::spawn_http(addr, "sigproc_capture".into(), m.clone()).unwrap();
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        for expected in [
            "HTTP/1.1 200 OK",
            "sigproc_capture_sample_rate_hz 250000",
            "sigproc_capture_samples_received_total 12",
            "sigproc_capture_receive_errors_total 1",
            "sigproc_capture_empty_reads_total 1",
            "sigproc_capture_control_datagrams_total 1",
            "sigproc_capture_primary_packets_sent_total 2",
            "sigproc_capture_primary_bytes_sent_total 100",
            "sigproc_capture_primary_packets_dropped_total 1",
            "sigproc_capture_multicast_packets_sent_total 1",
            "sigproc_capture_multicast_enabled 1",
        ] {
            assert!(
                response.contains(expected),
                "missing {expected}: {response}"
            );
        }
        m.set_rate(125_000.0);
        assert!(m
            .render_metrics("sigproc_capture")
            .contains("sigproc_capture_sample_rate_hz 125000"));
    }
}
