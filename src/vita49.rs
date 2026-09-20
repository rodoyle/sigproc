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

/// Number of 32-bit words in the VRT header + timestamp fields: the packet header
/// (word 0), stream ID (word 1), ts integer seconds (word 2) and the 64-bit
/// fractional timestamp (words 3-4).
const HEADER_WORDS: usize = 5;

/// Bytes occupied by [`HEADER_WORDS`] (5 x 4).
pub const HEADER_BYTES: usize = HEADER_WORDS * 4;

/// Build one complete VITA 49.0 IF Data Packet from interleaved sc16 IQ samples.
///
/// `samples` is interleaved I/Q as `[I0, Q0, I1, Q1, ...]` i16; a trailing odd
/// element (an I with no Q) is ignored so the payload is always whole words.
/// `timestamp_secs` is the UHD time in seconds (full + fractional).
///
/// Packet layout, big-endian 32-bit words (all offsets in bytes):
///
/// ```text
///   0..4    word 0   packet header
///   4..8    word 1   stream ID (0 today — see the module notes)
///   8..12   word 2   timestamp integer seconds (u32)
///   12..20  words 3-4  timestamp fractional, sample-count format (u64)
///   20..    payload  sc16 interleaved I/Q, two i16 per 32-bit word
/// ```
///
/// The size field (header bits 15-0) is `total_words - 1` where a word is a real
/// 32-bit word, so a full 2048-sample packet is 2053 words = **8212 bytes** with
/// header `0x10D00804` and the datagram is 4-byte aligned as VITA 49 requires.
///
/// (An earlier revision counted each *i16* as a word: it declared 4101 words,
/// allocated 8202 bytes, then wrote the payload past the end of that buffer and
/// panicked on the first packet ever actually sent.)
pub fn build_packet(samples: &[i16], timestamp_secs: f64, sample_rate: f64) -> Vec<u8> {
    let num_samples = samples.len() / 2; // complete I/Q pairs
    let payload_words = num_samples; // two sc16 components (I,Q) per 32-bit word
    let total_words = HEADER_WORDS + payload_words;
    let packet_size_words = (total_words - 1) as u16; // VITA49: total words minus 1

    // Header word: packet type | TSI (free-running) | TSF (sample counts) | size
    let header: u32 = (VITA49_IF_DATA_PACKET << 28)
        | (VITA49_TSI_OTHER << 22)
        | (VITA49_TSF_SAMPLE_COUNT << 20)
        | (packet_size_words as u32);

    // Timestamp fields. TSI=Other means ts_int is free-running seconds, and the
    // fractional part is expressed in sample counts (TSF), not sub-seconds.
    let ts_int = timestamp_secs as u32;
    let ts_frac = ((timestamp_secs - timestamp_secs.floor()) * sample_rate) as u64;

    // Whole 32-bit words only — never size this from a count that mixes units.
    let mut buf = vec![0u8; total_words * 4];

    buf[0..4].copy_from_slice(&header.to_be_bytes()); // word 0
    buf[4..8].copy_from_slice(&0u32.to_be_bytes()); // word 1 (stream id: 0 today)
    buf[8..12].copy_from_slice(&ts_int.to_be_bytes()); // word 2
    buf[12..HEADER_BYTES].copy_from_slice(&ts_frac.to_be_bytes()); // words 3-4

    // Words 5+: IQ payload — big-endian i16 per component
    for (i, &sample) in samples[..num_samples * 2].iter().enumerate() {
        let offset = HEADER_BYTES + i * 2;
        buf[offset..offset + 2].copy_from_slice(&sample.to_be_bytes());
    }

    buf
}

/// Builder for VITA49 IF Data Packets.
///
/// Each packet carries:
/// - 20-byte VRT header + timestamp fields (5 x 32-bit words)
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
    /// Framing lives in [`build_packet`]; this method only owns transport.
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
        let buf = build_packet(samples, timestamp_secs, sample_rate);

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

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f64 = 2_000_000.0;
    const FULL_SAMPLES: usize = 2048;

    /// Deterministic interleaved sc16 I/Q, `n` complex samples.
    fn make_samples(n: usize) -> Vec<i16> {
        (0..n * 2)
            .map(|i| (i as i16).wrapping_mul(37).wrapping_sub(1000))
            .collect()
    }

    fn header(pkt: &[u8]) -> u32 {
        u32::from_be_bytes(pkt[0..4].try_into().unwrap())
    }

    fn ts_int(pkt: &[u8]) -> u32 {
        u32::from_be_bytes(pkt[8..12].try_into().unwrap())
    }

    fn ts_frac(pkt: &[u8]) -> u64 {
        u64::from_be_bytes(pkt[12..20].try_into().unwrap())
    }

    /// The regression this fixes: the buffer used to be sized by a count that
    /// mixed i16 halves with 32-bit words, so the payload write ran past the end
    /// and panicked on the FIRST packet ever sent.
    #[test]
    fn every_payload_size_frames_without_panicking() {
        for n in 0..=FULL_SAMPLES {
            let pkt = build_packet(&make_samples(n), 0.0, SAMPLE_RATE);
            assert_eq!(pkt.len(), HEADER_BYTES + n * 4, "n={n}");
            assert_eq!(pkt.len() % 4, 0, "n={n}: VITA49 datagrams are word aligned");
        }
    }

    #[test]
    fn full_packet_is_8212_bytes_with_spec_correct_size_field() {
        let samples = make_samples(FULL_SAMPLES);
        let pkt = build_packet(&samples, 12.0, SAMPLE_RATE);

        assert_eq!(pkt.len(), 8212, "5 header words + 2048 payload words");
        assert_eq!(header(&pkt), 0x10D00804);
        // Size field is total 32-bit words minus one.
        assert_eq!((header(&pkt) & 0xFFFF) as usize + 1, pkt.len() / 4);
        assert_eq!(u32::from_be_bytes(pkt[4..8].try_into().unwrap()), 0, "stream id");

        // Payload round-trips byte-for-byte, big-endian, right after the header.
        for (i, &s) in samples.iter().enumerate() {
            let off = HEADER_BYTES + i * 2;
            assert_eq!(i16::from_be_bytes(pkt[off..off + 2].try_into().unwrap()), s);
        }
    }

    #[test]
    fn header_bit_fields_match_the_vita49_contract() {
        let h = header(&build_packet(&make_samples(FULL_SAMPLES), 0.0, SAMPLE_RATE));
        assert_eq!(h >> 28, VITA49_IF_DATA_PACKET, "packet type");
        assert_eq!((h >> 26) & 0x3, 0, "class id absent");
        assert_eq!((h >> 24) & 0x3, 0, "no trailer");
        assert_eq!((h >> 22) & 0x3, VITA49_TSI_OTHER, "free-running time");
        assert_eq!((h >> 20) & 0x3, VITA49_TSF_SAMPLE_COUNT, "sample-count fraction");
        assert_eq!((h >> 16) & 0xF, 0, "packet count is 0 today (known, recorded)");
    }

    #[test]
    fn fractional_timestamp_is_sample_counts_not_subseconds() {
        let pkt = build_packet(&make_samples(4), 12.5, SAMPLE_RATE);
        assert_eq!(ts_int(&pkt), 12);
        assert_eq!(ts_frac(&pkt), 1_000_000, "0.5 s at 2 MS/s");
        assert!(ts_frac(&pkt) < SAMPLE_RATE as u64);

        let carried = build_packet(&make_samples(4), 13.0, SAMPLE_RATE);
        assert_eq!(ts_int(&carried), 13);
        assert_eq!(ts_frac(&carried), 0, "fraction resets on carry");
    }

    /// Consecutive full packets must unwrap to a ~2048-sample step — this is the
    /// consumer's only loss signal, since sequence fields are not populated.
    #[test]
    fn consecutive_packets_unwrap_to_a_2048_sample_step() {
        let a = build_packet(&make_samples(FULL_SAMPLES), 1000.0, SAMPLE_RATE);
        let b = build_packet(&make_samples(FULL_SAMPLES), 1000.001024, SAMPLE_RATE);
        let counter = |p: &[u8]| ts_int(p) as u64 * SAMPLE_RATE as u64 + ts_frac(p);
        let step = counter(&b) as i64 - counter(&a) as i64;
        assert!((step - 2048).abs() <= 1, "step={step}");
    }

    #[test]
    fn short_packets_are_framed_from_their_own_size() {
        let pkt = build_packet(&make_samples(3), 0.0, SAMPLE_RATE);
        assert_eq!(pkt.len(), HEADER_BYTES + 3 * 4);
        assert_eq!(header(&pkt) & 0xFFFF, 7, "(5 + 3) - 1");

        // A trailing I with no Q cannot form a word: drop it, keep the packet valid.
        let mut odd = make_samples(2);
        odd.push(1234);
        let pkt = build_packet(&odd, 0.0, SAMPLE_RATE);
        assert_eq!(pkt.len(), HEADER_BYTES + 2 * 4);
        assert_eq!(pkt.len() % 4, 0);
    }
}
