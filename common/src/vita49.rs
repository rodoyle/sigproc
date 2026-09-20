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

/// How many backpressured (would-block) packets between transport-loss warnings.
///
/// The socket is non-blocking with the OS default send buffer, so a congested
/// consumer causes `send_to` to return `WouldBlock` and the packet to be
/// dropped silently. This interval bounds how loudly we report that loss
/// without flooding the log during exactly the congestion we are measuring.
const SEND_DROP_WARN_INTERVAL: u64 = 1000;

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
    //
    // Round rather than truncate. `as u64` on a product such as
    // `0.000048 * 2_000_000` can yield 95.99999999999999 -> 95, a ONE-SAMPLE
    // timestamp error. Consumers use this counter to detect loss (see
    // `crate::dsp::gaps`), so a truncation artefact reads as a lost sample --
    // and the sequence-gap gate asserts that count is zero.
    let whole_secs = timestamp_secs.floor();
    let mut ts_int = whole_secs as u32;
    let mut ts_frac = ((timestamp_secs - whole_secs) * sample_rate).round() as u64;
    if sample_rate >= 1.0 && ts_frac >= sample_rate as u64 {
        // The fraction rounded up to a whole second; carry it.
        ts_frac -= sample_rate as u64;
        ts_int = ts_int.wrapping_add(1);
    }

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
    /// Total packets dropped because the send buffer was full (backpressure).
    send_dropped: u64,
    /// Backpressured drops since the last such warning was logged.
    send_dropped_since_warn: u64,
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
            send_dropped: 0,
            send_dropped_since_warn: 0,
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

    /// Count one packet lost to send-buffer backpressure.
    ///
    /// Returns `Some(count)` for the just-finished reporting window when the
    /// periodic warn threshold was reached (the since-last report counter is
    /// reset here), or `None` when this drop is not yet reportable. Split out
    /// from [`Self::send`] so the accounting is testable without having to force
    /// a real `WouldBlock` from the OS.
    fn record_send_drop(&mut self) -> Option<u64> {
        self.send_dropped += 1;
        self.send_dropped_since_warn += 1;
        if self.send_dropped_since_warn >= SEND_DROP_WARN_INTERVAL {
            let window = self.send_dropped_since_warn;
            self.send_dropped_since_warn = 0;
            return Some(window);
        }
        None
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
            if self.dropped_since_warn.is_multiple_of(DROP_WARN_INTERVAL) {
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
                // Non-blocking socket + default SO_SNDBUF: a congested or
                // stalled consumer lands here. Count it, because the packet is
                // genuinely lost, and surface it periodically rather than at
                // debug level only.
                log::debug!(
                    "VITA49 send would block, dropping packet #{} ({} dropped so far)",
                    self.packet_count,
                    self.send_dropped
                );
                if let Some(window) = self.record_send_drop() {
                    log::warn!(
                        "VITA49 backpressure: {} packets dropped this window, \
                         {} total (consumer {} not draining)",
                        window,
                        self.send_dropped,
                        dest
                    );
                }
            }
            Err(e) => log::error!("VITA49 send error to {}: {}", dest, e),
        }
    }
}

/// Resolve a `host:port` string to a socket address, returning None on failure.
fn resolve(dest: &str) -> Option<SocketAddr> {
    dest.to_socket_addrs().ok().and_then(|mut it| it.next())
}

/// A parsed VITA 49.0 IF Data Packet, borrowing its payload from the buffer.
///
/// The wire format is defined by [`build_packet`] in this module and parsed
/// here, so a change to one side fails the round-trip test immediately rather
/// than leaving two implementations that disagree in production.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet<'a> {
    /// Packet type field (0x1 = IF Data Packet).
    pub packet_type: u32,
    /// Stream identifier word.
    pub stream_id: u32,
    /// Timestamp integer seconds (TSI).
    pub ts_int: u32,
    /// Timestamp fractional field — sample counts when TSF is sample-count.
    pub ts_frac: u64,
    /// Timestamp fractional mode (0x1 = sample count).
    pub tsf: u32,
    /// Whole 32-bit words of payload: two sc16 components per word.
    pub payload: &'a [u8],
}

impl Packet<'_> {
    /// Number of complex (I/Q) samples carried.
    pub fn samples(&self) -> usize {
        self.payload.len() / 4
    }

    /// Sample-counter timestamp: TSI seconds scaled by the sample rate plus the
    /// TSF sample count. This is the stream's continuity signal — see
    /// [`crate::dsp::gaps::GapTracker`].
    pub fn sample_counter(&self, sample_rate: f64) -> u64 {
        (self.ts_int as f64 * sample_rate) as u64 + self.ts_frac
    }

    /// Payload as interleaved sc16 I/Q (big-endian on the wire).
    pub fn iq_i16(&self) -> Vec<i16> {
        self.payload
            .chunks_exact(2)
            .map(|c| i16::from_be_bytes([c[0], c[1]]))
            .collect()
    }
}

/// Why a buffer is not a usable IF Data Packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Shorter than the 20-byte header.
    TooShort { got: usize },
    /// The header's size field disagrees with the bytes actually present.
    LengthMismatch { declared: usize, actual: usize },
    /// Only IF Data Packets (type 0x1) are understood by this chain.
    UnsupportedPacketType(u32),
    /// A trailer or header extension is present, which this parser does not skip.
    Trailerized,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::TooShort { got } => {
                write!(
                    f,
                    "packet shorter than the {HEADER_BYTES}-byte header ({got})"
                )
            }
            ParseError::LengthMismatch { declared, actual } => {
                write!(
                    f,
                    "header declares {declared} bytes but {actual} are present"
                )
            }
            ParseError::UnsupportedPacketType(t) => write!(f, "unsupported packet type {t:#x}"),
            ParseError::Trailerized => write!(f, "trailerized packets are not supported"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse exactly one IF Data Packet from `buf`.
///
/// Strict by design: the buffer must be exactly one packet. A short read is an
/// error rather than a truncated packet, because a consumer that silently
/// accepts a partial payload turns a transport fault into corrupt samples.
pub fn parse(buf: &[u8]) -> Result<Packet<'_>, ParseError> {
    if buf.len() < HEADER_BYTES {
        return Err(ParseError::TooShort { got: buf.len() });
    }
    let header = u32::from_be_bytes(buf[0..4].try_into().expect("4 bytes"));
    let packet_type = header >> 28;
    if packet_type != VITA49_IF_DATA_PACKET {
        return Err(ParseError::UnsupportedPacketType(packet_type));
    }
    if (header >> 24) & 0x3 != 0 {
        return Err(ParseError::Trailerized);
    }

    let declared = ((header & 0xFFFF) as usize + 1) * 4;
    if declared != buf.len() {
        return Err(ParseError::LengthMismatch {
            declared,
            actual: buf.len(),
        });
    }

    Ok(Packet {
        packet_type,
        stream_id: u32::from_be_bytes(buf[4..8].try_into().expect("4 bytes")),
        ts_int: u32::from_be_bytes(buf[8..12].try_into().expect("4 bytes")),
        ts_frac: u64::from_be_bytes(buf[12..20].try_into().expect("8 bytes")),
        tsf: (header >> 20) & 0x3,
        payload: &buf[HEADER_BYTES..],
    })
}

/// Walks a buffer containing one or more concatenated VITA49 packets (the
/// fixture file format) or a receive buffer of back-to-back datagrams.
pub struct PacketCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> PacketCursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        PacketCursor { buf, pos: 0 }
    }

    /// True when every byte has been consumed as a packet.
    pub fn is_exhausted(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Parse the next packet, advancing past it. `None` at end of buffer.
    pub fn next_packet(&mut self) -> Option<Result<Packet<'a>, ParseError>> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let rest = &self.buf[self.pos..];
        if rest.len() < HEADER_BYTES {
            self.pos = self.buf.len();
            return Some(Err(ParseError::TooShort { got: rest.len() }));
        }
        let header = u32::from_be_bytes(rest[0..4].try_into().expect("4 bytes"));
        let declared = ((header & 0xFFFF) as usize + 1) * 4;
        if declared > rest.len() {
            self.pos = self.buf.len();
            return Some(Err(ParseError::LengthMismatch {
                declared,
                actual: rest.len(),
            }));
        }
        let (packet_buf, _next) = rest.split_at(declared);
        self.pos += declared;
        Some(parse(packet_buf))
    }
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
        assert_eq!(
            u32::from_be_bytes(pkt[4..8].try_into().unwrap()),
            0,
            "stream id"
        );

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
        assert_eq!(
            (h >> 20) & 0x3,
            VITA49_TSF_SAMPLE_COUNT,
            "sample-count fraction"
        );
        assert_eq!(
            (h >> 16) & 0xF,
            0,
            "packet count is 0 today (known, recorded)"
        );
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

    /// Consecutive full packets must unwrap to an exact 2048-sample step — this
    /// is the consumer's only loss signal, since sequence fields are not
    /// populated. Exact, not ±1: a one-sample error here is indistinguishable
    /// from a lost sample downstream.
    #[test]
    fn consecutive_packets_unwrap_to_an_exact_2048_sample_step() {
        let counter = |p: &[u8]| ts_int(p) as u64 * SAMPLE_RATE as u64 + ts_frac(p);
        let mut previous = counter(&build_packet(
            &make_samples(FULL_SAMPLES),
            1000.0,
            SAMPLE_RATE,
        ));
        // Fractional steps chosen to land on awkward floating-point values.
        for step in 1..200 {
            let ts = 1000.0 + step as f64 * FULL_SAMPLES as f64 / SAMPLE_RATE;
            let now = counter(&build_packet(&make_samples(FULL_SAMPLES), ts, SAMPLE_RATE));
            assert_eq!(now - previous, FULL_SAMPLES as u64, "step {step} (ts {ts})");
            previous = now;
        }
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

    /// Transport loss is only observable through this counter when the OS send
    /// buffer is full, so assert the accounting directly: the report fires every
    /// `SEND_DROP_WARN_INTERVAL` drops, not once per drop (which would flood the
    /// log precisely during the congestion being reported).
    #[test]
    fn backpressure_drops_are_counted_and_reported_periodically() {
        let mut fwd = Vita49Forwarder::new("127.0.0.1", 4820, 0).unwrap();
        assert_eq!(fwd.send_dropped, 0);

        let reports = (1..SEND_DROP_WARN_INTERVAL)
            .filter(|_| fwd.record_send_drop().is_some())
            .count();
        assert_eq!(reports, 0, "must not warn before the interval is reached");
        assert_eq!(fwd.send_dropped, SEND_DROP_WARN_INTERVAL - 1);

        assert_eq!(
            fwd.record_send_drop(),
            Some(SEND_DROP_WARN_INTERVAL),
            "interval-th drop reports its window size"
        );
        assert_eq!(fwd.send_dropped, SEND_DROP_WARN_INTERVAL);
        assert_eq!(
            fwd.send_dropped_since_warn, 0,
            "since-last-report counter resets so the next window is independent"
        );

        assert_eq!(fwd.record_send_drop(), None, "next window starts fresh");
        assert_eq!(fwd.send_dropped, SEND_DROP_WARN_INTERVAL + 1);
    }

    // ── Parser ───────────────────────────────────────────────────────────────

    #[test]
    fn parse_round_trips_build_packet_exactly() {
        for n in [0usize, 1, 7, 64, FULL_SAMPLES] {
            let samples = make_samples(n);
            let pkt = build_packet(&samples, 1234.5, SAMPLE_RATE);
            let parsed = parse(&pkt).expect("parses");
            assert_eq!(parsed.packet_type, VITA49_IF_DATA_PACKET);
            assert_eq!(parsed.samples(), n);
            assert_eq!(parsed.ts_int, 1234);
            assert_eq!(parsed.ts_frac, 1_000_000, "0.5 s in sample counts");
            assert_eq!(parsed.tsf, VITA49_TSF_SAMPLE_COUNT);
            assert_eq!(parsed.iq_i16(), samples, "payload round-trips intact");
        }
    }

    #[test]
    fn parse_rejects_a_short_or_overlong_buffer() {
        let full = build_packet(&make_samples(16), 0.0, SAMPLE_RATE);
        assert_eq!(parse(&full[..8]), Err(ParseError::TooShort { got: 8 }));

        let mut overlong = full.clone();
        overlong.push(0);
        assert_eq!(
            parse(&overlong),
            Err(ParseError::LengthMismatch {
                declared: full.len(),
                actual: full.len() + 1
            })
        );

        let mut truncated = full;
        truncated.truncate(truncated.len() - 4);
        assert!(matches!(
            parse(&truncated),
            Err(ParseError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn parse_rejects_packet_types_this_chain_does_not_understand() {
        let mut pkt = build_packet(&make_samples(4), 0.0, SAMPLE_RATE);
        // Rewrite the type field to a context packet (0x4).
        let mut header = u32::from_be_bytes(pkt[0..4].try_into().unwrap());
        header = (header & 0x0FFF_FFFF) | (0x4 << 28);
        pkt[0..4].copy_from_slice(&header.to_be_bytes());
        assert_eq!(parse(&pkt), Err(ParseError::UnsupportedPacketType(0x4)));
    }

    /// A trailer bit means extra words this parser does not skip; accepting it
    /// would mean treating trailer bytes as samples.
    #[test]
    fn parse_refuses_trailerized_packets_rather_than_reading_trailers_as_samples() {
        let mut pkt = build_packet(&make_samples(4), 0.0, SAMPLE_RATE);
        let header = u32::from_be_bytes(pkt[0..4].try_into().unwrap());
        // Set the trailer bit. Written as a plain expression (not `|=`) — an
        // AST-level linter used in this repo mis-parses the compound-assignment
        // form on a field of a local as an invalid assignment target.
        let header = header | (0x1 << 24);
        pkt[0..4].copy_from_slice(&header.to_be_bytes());
        assert_eq!(parse(&pkt), Err(ParseError::Trailerized));
    }

    #[test]
    fn a_packet_file_is_walked_packet_by_packet() {
        let packets: Vec<Vec<u8>> = (0..5)
            .map(|i| {
                // Timestamps advance by exactly one packet's worth of samples,
                // as a real capture does.
                let ts = 100.0 + i as f64 * 32.0 / SAMPLE_RATE;
                build_packet(&make_samples(32), ts, SAMPLE_RATE)
            })
            .collect();
        let mut file = Vec::new();
        for p in &packets {
            file.extend_from_slice(p);
        }

        let mut cursor = PacketCursor::new(&file);
        let mut counters = Vec::new();
        while let Some(next) = cursor.next_packet() {
            let parsed = next.expect("parses");
            counters.push(parsed.sample_counter(SAMPLE_RATE));
        }
        assert!(cursor.is_exhausted());
        assert_eq!(counters.len(), 5);

        // A file of 32-sample packets at 2 MS/s advances by 32 samples each time.
        for (i, pair) in counters.windows(2).enumerate() {
            assert_eq!(
                pair[1] - pair[0],
                32,
                "packet {i} to {}: sample counter must advance by the payload length",
                i + 1
            );
        }
        assert_eq!(
            counters[0],
            100 * SAMPLE_RATE as u64,
            "first packet starts at 100 s"
        );
    }

    #[test]
    fn a_truncated_tail_in_a_packet_file_is_reported_not_silently_dropped() {
        let mut file = build_packet(&make_samples(16), 0.0, SAMPLE_RATE);
        file.extend_from_slice(&build_packet(&make_samples(16), 0.0, SAMPLE_RATE)[..12]);

        let mut cursor = PacketCursor::new(&file);
        assert!(cursor.next_packet().expect("first").is_ok());
        let err = cursor
            .next_packet()
            .expect("second")
            .expect_err("truncated tail must be reported");
        assert!(matches!(err, ParseError::TooShort { .. }), "{err:?}");
    }
}
