//! Minimal VITA 49.0 (VRT) IF Data Packet framing and UDP forwarding.
//!
//! Encodes IQ samples as sc16 (interleaved 16-bit signed) VITA49 packets
//! and sends them via UDP to the configured destination.
//!
//! The destination is resolved lazily and its absence is non-fatal: this
//! process is the RF capture front-end, and it must keep capturing even when
//! the downstream consumer (waterfall) is not deployed yet. Until the name
//! resolves, packets are dropped and a periodic warning is logged.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// VITA 49.0 IF Data Packet header constants.
const VITA49_IF_DATA_PACKET: u32 = 0x1;
const VITA49_TSI_OTHER: u32 = 0x3; // free-running timestamp
const VITA49_TSF_SAMPLE_COUNT: u32 = 0x1;

/// How long to wait between DNS re-resolution attempts when the destination
/// is unresolved.
const RESOLVE_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// How many dropped packets between "still unresolved" warnings.
const DROP_WARN_INTERVAL: u64 = 1000;

/// Builder for VITA49 IF Data Packets.
///
/// Each packet carries:
/// - 28-byte VRT header (packet type, stream ID, timestamp fields)
/// - Payload: IQ samples as interleaved `[I0, Q0, I1, Q1, ...]` i16 pairs (sc16)
pub struct Vita49Forwarder {
    socket: UdpSocket,
    /// Destination as given, e.g. `waterfall-service:4820`.
    dest: String,
    /// Resolved destination, once resolution succeeds.
    resolved: Option<SocketAddr>,
    /// When we last attempted resolution (None = never).
    last_resolve_attempt: Option<Instant>,
    packet_count: u32,
    /// Packets dropped since the last unresolved warning was logged.
    dropped_since_warn: u64,
}

impl Vita49Forwarder {
    /// Create a new VITA49 forwarder bound to an ephemeral local port.
    ///
    /// Does NOT fail when the destination cannot be resolved — the consumer
    /// may legitimately not exist yet.
    pub fn new(dest_host: &str, dest_port: u16, stream_id: u32) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_nonblocking(true)?;
        let dest = format!("{}:{}", dest_host, dest_port);

        let resolved = resolve(&dest);
        match resolved {
            Some(addr) => log::info!(
                "VITA49 forwarder: stream_id=0x{:08x} -> {} ({})",
                stream_id,
                dest,
                addr
            ),
            None => log::warn!(
                "VITA49 destination {} does not resolve yet; capture will continue and \
                 forwarding will begin automatically once it resolves",
                dest
            ),
        }

        Ok(Self {
            socket,
            dest,
            resolved,
            last_resolve_attempt: Some(Instant::now()),
            packet_count: 0,
            dropped_since_warn: 0,
        })
    }

    /// Re-attempt resolution if the destination is currently unresolved and
    /// the retry interval has elapsed.
    fn resolve_if_needed(&mut self) {
        if self.resolved.is_some() {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_resolve_attempt {
            if now.duration_since(last) < RESOLVE_RETRY_INTERVAL {
                return;
            }
        }
        self.last_resolve_attempt = Some(now);

        if let Some(addr) = resolve(&self.dest) {
            log::info!(
                "VITA49 destination {} resolved to {}; forwarding started",
                self.dest,
                addr
            );
            self.resolved = Some(addr);
            self.dropped_since_warn = 0;
        }
    }

    /// Pack IQ samples into a VITA49 IF Data Packet and send via UDP.
    ///
    /// `samples` is interleaved I/Q as `[I0, Q0, I1, Q1, ...]` i16.
    /// `timestamp_secs` is the UHD time in seconds (full + fractional).
    ///
    /// Packet layout (big-endian words):
    ///   Word 0: Packet header
    ///   Word 1: Stream ID
    ///   Word 2: Timestamp integer seconds (u32)
    ///   Word 3-4: Timestamp fractional seconds (u64, sample-count format)
    ///   Words 5+: IQ payload (sc16 interleaved)
    pub fn send(&mut self, samples: &[i16], timestamp_secs: f64, sample_rate: f64) {
        self.packet_count = self.packet_count.wrapping_add(1);
        self.resolve_if_needed();

        let Some(dest) = self.resolved else {
            // No consumer yet — drop but keep capturing.
            self.dropped_since_warn += 1;
            if self.dropped_since_warn % DROP_WARN_INTERVAL == 0 {
                log::warn!(
                    "VITA49 destination {} still unresolved; {} packets dropped so far",
                    self.dest,
                    self.dropped_since_warn
                );
            }
            return;
        };

        let num_samples = samples.len() / 2; // I/Q pairs
        let payload_words = samples.len(); // each i16 = 1 16-bit word
        let total_words = 5 + payload_words; // header + stream_id + ts_int + ts_frac(2) + payload
        let packet_size_words = (total_words - 1) as u16; // VITA49: total words minus 1

        // Header word
        let header: u32 = (VITA49_IF_DATA_PACKET << 28)
            | (VITA49_TSI_OTHER << 22)
            | (VITA49_TSF_SAMPLE_COUNT << 20)
            | (packet_size_words as u32);

        // Timestamp fields
        let ts_int = timestamp_secs as u32;
        let ts_frac = ((timestamp_secs - timestamp_secs.floor()) * sample_rate) as u64;

        // Assemble packet buffer
        let mut buf = vec![0u8; total_words * 2];

        buf[0..4].copy_from_slice(&header.to_be_bytes()); // Word 0
        buf[4..8].copy_from_slice(&0u32.to_be_bytes()); // Word 1 (stream id, set below)
        buf[4..8].copy_from_slice(&0u32.to_be_bytes());
        buf[8..12].copy_from_slice(&ts_int.to_be_bytes()); // Word 2
        buf[12..20].copy_from_slice(&ts_frac.to_be_bytes()); // Words 3-4

        // Words 5+: IQ payload — big-endian i16 per sample
        for (i, &sample) in samples.iter().enumerate() {
            let offset = 20 + i * 2;
            buf[offset..offset + 2].copy_from_slice(&sample.to_be_bytes());
        }

        match self.socket.send_to(&buf, dest) {
            Ok(n) if n == buf.len() => {
                log::debug!(
                    "VITA49 pkt #{}: {} samples, {} bytes -> {}",
                    self.packet_count,
                    num_samples,
                    n,
                    dest
                );
            }
            Ok(n) => log::warn!("VITA49 partial send: {}/{} bytes", n, buf.len()),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                log::debug!(
                    "VITA49 send would block, dropping packet #{}",
                    self.packet_count
                );
            }
            Err(e) => log::error!("VITA49 send error to {}: {}", dest, e),
        }
    }
}

/// Resolve a `host:port` string to a socket address, returning None on failure.
fn resolve(dest: &str) -> Option<SocketAddr> {
    dest.to_socket_addrs().ok().and_then(|mut it| it.next())
}
