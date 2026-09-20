//! Counters and a minimal HTTP surface for `/metrics` and `/healthz`.
//!
//! Every service in the chain needs the same observability primitives, and two of
//! them are load-bearing for verification rather than nice-to-have:
//!
//! * **`..._seq_gaps_total`** — loss that a level or tone measurement would
//!   otherwise hide. A gate asserts this is zero for an intact stream, so it must
//!   be a counter, not a log line.
//! * **`..._output_power_dbfs`** — the level of the channel actually isolated.
//!   Consumers must take it *after* the filter settling region; see
//!   [`crate::dsp::Channelizer::settling_samples`].
//!
//! Deliberately dependency-free: a hand-rolled HTTP/1.1 responder over
//! `std::net::TcpListener` handling one request per connection. Pulling a web
//! framework into the capture image (which is already carrying UHD on an arm64
//! node) to serve two endpoints would be a poor trade.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Atomic counters shared between the processing thread and the HTTP thread.
#[derive(Debug)]
pub struct Metrics {
    packets_received: AtomicU64,
    packets_dropped: AtomicU64,
    seq_gaps: AtomicU64,
    gap_samples: AtomicU64,
    samples_in: AtomicU64,
    samples_out: AtomicU64,
    /// f32 bits: dBFS of the channel output.
    output_power_dbfs: AtomicU32,
    /// f32 bits: measured channel peak offset in Hz.
    peak_offset_hz: AtomicU32,
    /// f32 bits: dBFS of the wideband input.
    input_power_dbfs: AtomicU32,
}

impl Default for Metrics {
    /// The float gauges start at **NaN**, not 0. Zero would be a lie with
    /// consequences: 0 dBFS means full scale, so an unset power gauge would be
    /// plotted as a real, maximal measurement. NaN says "not measured yet".
    fn default() -> Self {
        let unset = f32::NAN.to_bits();
        Metrics {
            packets_received: AtomicU64::new(0),
            packets_dropped: AtomicU64::new(0),
            seq_gaps: AtomicU64::new(0),
            gap_samples: AtomicU64::new(0),
            samples_in: AtomicU64::new(0),
            samples_out: AtomicU64::new(0),
            output_power_dbfs: AtomicU32::new(unset),
            peak_offset_hz: AtomicU32::new(unset),
            input_power_dbfs: AtomicU32::new(unset),
        }
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_packet_received(&self) {
        self.packets_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_packets_dropped(&self, n: u64) {
        self.packets_dropped.fetch_add(n, Ordering::Relaxed);
    }

    pub fn record_gap(&self, missing_samples: u64) {
        self.seq_gaps.fetch_add(1, Ordering::Relaxed);
        self.gap_samples
            .fetch_add(missing_samples, Ordering::Relaxed);
    }

    pub fn record_samples(&self, input: u64, output: u64) {
        self.samples_in.fetch_add(input, Ordering::Relaxed);
        self.samples_out.fetch_add(output, Ordering::Relaxed);
    }

    pub fn set_output_power_dbfs(&self, v: f32) {
        self.output_power_dbfs.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn set_input_power_dbfs(&self, v: f32) {
        self.input_power_dbfs.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn set_peak_offset_hz(&self, v: f32) {
        self.peak_offset_hz.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn packets_received(&self) -> u64 {
        self.packets_received.load(Ordering::Relaxed)
    }

    pub fn packets_dropped(&self) -> u64 {
        self.packets_dropped.load(Ordering::Relaxed)
    }

    pub fn seq_gaps(&self) -> u64 {
        self.seq_gaps.load(Ordering::Relaxed)
    }

    pub fn gap_samples(&self) -> u64 {
        self.gap_samples.load(Ordering::Relaxed)
    }

    pub fn samples_in(&self) -> u64 {
        self.samples_in.load(Ordering::Relaxed)
    }

    pub fn samples_out(&self) -> u64 {
        self.samples_out.load(Ordering::Relaxed)
    }

    pub fn output_power_dbfs(&self) -> f32 {
        f32::from_bits(self.output_power_dbfs.load(Ordering::Relaxed))
    }

    pub fn peak_offset_hz(&self) -> f32 {
        f32::from_bits(self.peak_offset_hz.load(Ordering::Relaxed))
    }

    pub fn input_power_dbfs(&self) -> f32 {
        f32::from_bits(self.input_power_dbfs.load(Ordering::Relaxed))
    }

    /// Prometheus text exposition. `prefix` namespaces the counters per service
    /// (`channelizer`, `demod`, ...) so two services in one cluster never collide.
    pub fn render(&self, prefix: &str) -> String {
        let mut s = String::new();
        let mut counter = |name: &str, help: &str, value: u64| {
            s.push_str(&format!("# HELP {prefix}_{name} {help}\n"));
            s.push_str(&format!("# TYPE {prefix}_{name} counter\n"));
            s.push_str(&format!("{prefix}_{name} {value}\n"));
        };
        counter(
            "vita49_packets_received_total",
            "VITA49 packets received from upstream",
            self.packets_received(),
        );
        counter(
            "vita49_packets_dropped_total",
            "VITA49 packets that could not be delivered downstream",
            self.packets_dropped(),
        );
        counter(
            "vita49_seq_gaps_total",
            "Forward discontinuities detected in the upstream sample counter",
            self.seq_gaps(),
        );
        counter(
            "vita49_gap_samples_total",
            "Samples lost to those discontinuities",
            self.gap_samples(),
        );
        counter(
            "samples_in_total",
            "Wideband samples consumed",
            self.samples_in(),
        );
        counter(
            "samples_out_total",
            "Narrowband samples produced",
            self.samples_out(),
        );
        for (name, help, value) in [
            (
                "input_power_dbfs",
                "Wideband input mean power (0 dBFS = full-scale complex)",
                self.input_power_dbfs(),
            ),
            (
                "output_power_dbfs",
                "Channel output mean power, after filter settling",
                self.output_power_dbfs(),
            ),
            (
                "peak_offset_hz",
                "Offset of the strongest component in the channel, including its carrier \
                 (0 Hz = centred on the signal; an AM sideband is NOT reported here)",
                self.peak_offset_hz(),
            ),
        ] {
            s.push_str(&format!("# HELP {prefix}_{name} {help}\n"));
            s.push_str(&format!("# TYPE {prefix}_{name} gauge\n"));
            // A gauge that has never been set must not render as NaN text.
            let rendered = if value.is_finite() {
                format!("{value}")
            } else {
                "NaN".to_string()
            };
            s.push_str(&format!("{prefix}_{name} {rendered}\n"));
        }
        s
    }
}

/// Serve `/metrics` and `/healthz` on `listen` from a background thread.
pub fn spawn_http(
    listen: SocketAddr,
    prefix: String,
    metrics: Arc<Metrics>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(listen)?;
    log::info!("metrics listening on http://{listen}/metrics");
    Ok(std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut s) => {
                    if let Err(e) = handle_connection(&mut s, &prefix, &metrics) {
                        log::debug!("metrics connection error: {e}");
                    }
                }
                Err(e) => log::warn!("metrics accept error: {e}"),
            }
        }
    }))
}

fn handle_connection(
    stream: &mut TcpStream,
    prefix: &str,
    metrics: &Metrics,
) -> std::io::Result<()> {
    // A client that connects and never sends must not wedge the loop.
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request.split_whitespace().nth(1).unwrap_or("/");

    let (status, content_type, body) = match path {
        "/metrics" => (
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            metrics.render(prefix),
        ),
        "/healthz" => ("200 OK", "text/plain; charset=utf-8", "ok\n".to_string()),
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found\n".to_string(),
        ),
    };

    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_render_the_metric_names_the_gates_assert() {
        let m = Metrics::new();
        m.record_packet_received();
        m.record_packet_received();
        m.record_packets_dropped(3);
        m.record_gap(2048);
        m.record_samples(1000, 25);
        m.set_output_power_dbfs(-6.02);
        m.set_peak_offset_hz(300.5);

        let text = m.render("channelizer");
        for expected in [
            "channelizer_vita49_packets_received_total 2",
            "channelizer_vita49_packets_dropped_total 3",
            "channelizer_vita49_seq_gaps_total 1",
            "channelizer_vita49_gap_samples_total 2048",
            "channelizer_samples_in_total 1000",
            "channelizer_samples_out_total 25",
            "channelizer_output_power_dbfs -6.02",
            "channelizer_peak_offset_hz 300.5",
        ] {
            assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
        }
        // Prometheus requires a TYPE line before each sample.
        assert_eq!(text.matches("# TYPE").count(), 9);
    }

    /// A gauge that was never set must render as `NaN`, not as the text `inf` or
    /// a bogus 0 that a dashboard would plot as a real measurement.
    #[test]
    fn unset_gauges_render_as_nan_not_zero() {
        let text = Metrics::new().render("channelizer");
        assert!(text.contains("channelizer_output_power_dbfs NaN"), "{text}");
        assert!(!text.contains("channelizer_output_power_dbfs 0"), "{text}");
    }

    #[test]
    fn the_prefix_namespaces_each_service() {
        let m = Metrics::new();
        assert!(m
            .render("demod")
            .starts_with("# HELP demod_vita49_packets_received_total"));
        assert!(
            m.render("demod").contains("\ndemod_seq_gaps_total")
                || m.render("demod").contains("\ndemod_vita49_seq_gaps_total")
        );
    }

    #[test]
    fn http_serves_metrics_healthz_and_404_over_a_real_socket() {
        let metrics = Arc::new(Metrics::new());
        metrics.record_packet_received();
        // Port 0 lets the OS pick a free port; the handle is bound already.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let handle = spawn_http(addr, "channelizer".to_string(), metrics).expect("serve");

        let get = |path: &str| -> String {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .expect("write");
            let mut out = String::new();
            s.read_to_string(&mut out).expect("read");
            out
        };

        let metrics_response = get("/metrics");
        assert!(
            metrics_response.starts_with("HTTP/1.1 200 OK"),
            "{metrics_response}"
        );
        assert!(
            metrics_response.contains("channelizer_vita49_packets_received_total 1"),
            "{metrics_response}"
        );

        let health = get("/healthz");
        assert!(health.starts_with("HTTP/1.1 200 OK"), "{health}");
        assert!(health.ends_with("ok\n"), "{health}");

        let missing = get("/nope");
        assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");

        // The server thread is a loop; the test process exiting ends it.
        drop(handle);
    }
}
