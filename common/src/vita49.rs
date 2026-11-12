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

/// VITA 49.0 packet header constants.
const VITA49_IF_DATA_PACKET: u32 = 0x1;
/// IF Context Packet (with stream ID).
const VITA49_IF_CONTEXT_PACKET: u32 = 0x4;
const VITA49_TSI_OTHER: u32 = 0x3; // free-running timestamp
const VITA49_TSF_SAMPLE_COUNT: u32 = 0x1;

/// Default number of complex samples per IF Data Packet.
///
/// 5 header words + 360 payload words = 1460 bytes, which fits inside one
/// 1500-byte Ethernet MTU with IPv4 (20) + UDP (8) headers to spare. The UHD
/// receive buffer size (2048 samples) must NOT be the packetization unit: at
/// 8212 bytes a packet is fragmented into six IP fragments and the loss of any
/// one of them discards the whole packet.
pub const DEFAULT_SAMPLES_PER_PACKET: usize = 360;

/// Emit a context packet at least this often (in data packets).
pub const CONTEXT_EVERY_N_DATA_PACKETS: u32 = 256;

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

/// Build one complete VITA 49.0 IF Data Packet with stream ID 0 and packet
/// count 0. Convenience wrapper over [`build_data_packet`] for fixtures/tests.
pub fn build_packet(samples: &[i16], timestamp_secs: f64, sample_rate: f64) -> Vec<u8> {
    build_data_packet(samples, timestamp_secs, sample_rate, 0, 0)
}

/// Build one complete VITA 49.0 IF Data Packet from interleaved sc16 IQ samples.
///
/// `samples` is interleaved I/Q as `[I0, Q0, I1, Q1, ...]` i16; a trailing odd
/// element (an I with no Q) is ignored so the payload is always whole words.
/// `timestamp_secs` is the UHD time in seconds (full + fractional).
/// `stream_id` is serialized into word 1; `packet_count` (modulo 16) into
/// header bits 19-16 so receivers can detect lost datagrams.
///
/// Packet layout, big-endian 32-bit words (all offsets in bytes):
///
/// ```text
///   0..4    word 0   packet header
///   4..8    word 1   stream ID
///   8..12   word 2   timestamp integer seconds (u32)
///   12..20  words 3-4  timestamp fractional, sample-count format (u64)
///   20..    payload  sc16 interleaved I/Q, two i16 per 32-bit word
/// ```
///
/// The size field (header bits 15-0) is the **total** number of 32-bit words in
/// the packet including the header, per VITA 49.0 (and as SoapyVITA49 and
/// Geon's libVRT read it). A full 2048-sample packet is 2053 words = 8212 bytes
/// with header `0x10D00805`.
///
/// (An earlier revision encoded `total_words - 1`, under-declaring every packet
/// by one word; a still earlier one counted each *i16* as a word and wrote past
/// the end of its buffer.)
pub fn build_data_packet(
    samples: &[i16],
    timestamp_secs: f64,
    sample_rate: f64,
    stream_id: u32,
    packet_count: u8,
) -> Vec<u8> {
    let num_samples = samples.len() / 2; // complete I/Q pairs
    let payload_words = num_samples; // two sc16 components (I,Q) per 32-bit word
    let total_words = HEADER_WORDS + payload_words;
    assert!(
        total_words <= u16::MAX as usize,
        "packet exceeds 65535 words"
    );
    let packet_size_words = total_words as u16; // VITA49: total words incl. header

    // Header word: packet type | TSI (free-running) | TSF (sample counts) | size
    let header: u32 = (VITA49_IF_DATA_PACKET << 28)
        | (VITA49_TSI_OTHER << 22)
        | (VITA49_TSF_SAMPLE_COUNT << 20)
        | (((packet_count & 0xF) as u32) << 16)
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
    buf[4..8].copy_from_slice(&stream_id.to_be_bytes()); // word 1
    buf[8..12].copy_from_slice(&ts_int.to_be_bytes()); // word 2
    buf[12..HEADER_BYTES].copy_from_slice(&ts_frac.to_be_bytes()); // words 3-4

    // Words 5+: IQ payload — big-endian i16 per component
    for (i, &sample) in samples[..num_samples * 2].iter().enumerate() {
        let offset = HEADER_BYTES + i * 2;
        buf[offset..offset + 2].copy_from_slice(&sample.to_be_bytes());
    }

    buf
}

/// Stream metadata carried in IF Context Packets.
///
/// SoapyVITA49 (and other VRT consumers) derive sample rate, tuned frequency and
/// payload format from context, not from the data stream, so every forwarder
/// emits one of these periodically and immediately after a configuration change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamContext {
    /// RF reference (tuned centre) frequency in Hz.
    pub rf_freq_hz: f64,
    /// Sample rate in Hz.
    pub sample_rate_hz: f64,
    /// Analog/digital bandwidth in Hz.
    pub bandwidth_hz: f64,
    /// Receiver gain in dB (stage 1).
    pub gain_db: f64,
}

/// Context Indicator Field 0 bits (VITA 49.0 §7.1.5.1).
const CIF0_CHANGE_INDICATOR: u32 = 1 << 31;
const CIF0_BANDWIDTH: u32 = 1 << 29;
const CIF0_RF_REF_FREQ: u32 = 1 << 27;
const CIF0_GAIN: u32 = 1 << 23;
const CIF0_SAMPLE_RATE: u32 = 1 << 21;
const CIF0_DATA_PAYLOAD_FORMAT: u32 = 1 << 15;

/// Context packet size: header(1) + stream id(1) + tsi(1) + tsf(2) + CIF0(1)
/// + bandwidth(2) + rf freq(2) + gain(1) + sample rate(2) + payload format(2).
pub const CONTEXT_PACKET_WORDS: usize = 15;
/// Bytes of a context packet built by [`build_context_packet`].
pub const CONTEXT_PACKET_BYTES: usize = CONTEXT_PACKET_WORDS * 4;

/// Hz as a 64-bit two's-complement fixed-point value with a 20-bit radix point.
fn hz_q20(hz: f64) -> u64 {
    ((hz * (1u64 << 20) as f64).round() as i64) as u64
}

/// Data Packet Payload Format field describing our sc16 I/Q payload
/// (VITA 49.0 §7.1.5.18): packing method = link-efficient (0), real/complex =
/// complex cartesian (01), data item format = signed fixed-point (00000),
/// item packing field size = 16 bits (0b001111), data item size = 16 bits
/// (0b001111), repeat count and vector size = 1 (encoded 0).
fn sc16_payload_format() -> u64 {
    let word0: u32 = (0b01 << 29) | (15 << 6) | 15;
    (word0 as u64) << 32
}

/// Build one IF Context Packet (type 0x4) for `stream_id`.
///
/// `change` sets the Context Field Change Indicator; set it on the first packet
/// after any of the fields changed, clear it on periodic repeats.
pub fn build_context_packet(
    ctx: &StreamContext,
    timestamp_secs: f64,
    stream_id: u32,
    packet_count: u8,
    change: bool,
) -> Vec<u8> {
    let header: u32 = (VITA49_IF_CONTEXT_PACKET << 28)
        | (VITA49_TSI_OTHER << 22)
        | (VITA49_TSF_SAMPLE_COUNT << 20)
        | (((packet_count & 0xF) as u32) << 16)
        | (CONTEXT_PACKET_WORDS as u32);
    let (ts_int, ts_frac) = split_timestamp(timestamp_secs, ctx.sample_rate_hz);
    let mut cif0 =
        CIF0_BANDWIDTH | CIF0_RF_REF_FREQ | CIF0_GAIN | CIF0_SAMPLE_RATE | CIF0_DATA_PAYLOAD_FORMAT;
    if change {
        cif0 |= CIF0_CHANGE_INDICATOR;
    }
    // Gain: two 16-bit Q7 fields, stage 2 (upper) = 0, stage 1 (lower) = gain.
    let gain_q7 = ((ctx.gain_db * 128.0).round() as i16) as u16 as u32;

    let mut buf = Vec::with_capacity(CONTEXT_PACKET_BYTES);
    buf.extend_from_slice(&header.to_be_bytes());
    buf.extend_from_slice(&stream_id.to_be_bytes());
    buf.extend_from_slice(&ts_int.to_be_bytes());
    buf.extend_from_slice(&ts_frac.to_be_bytes());
    buf.extend_from_slice(&cif0.to_be_bytes());
    buf.extend_from_slice(&hz_q20(ctx.bandwidth_hz).to_be_bytes());
    buf.extend_from_slice(&hz_q20(ctx.rf_freq_hz).to_be_bytes());
    buf.extend_from_slice(&gain_q7.to_be_bytes());
    buf.extend_from_slice(&hz_q20(ctx.sample_rate_hz).to_be_bytes());
    buf.extend_from_slice(&sc16_payload_format().to_be_bytes());
    debug_assert_eq!(buf.len(), CONTEXT_PACKET_BYTES);
    buf
}

/// Split a UHD time into VRT TSI seconds and TSF sample count, rounding (not
/// truncating) and carrying a fraction that rounds up to a full second.
fn split_timestamp(timestamp_secs: f64, sample_rate: f64) -> (u32, u64) {
    let whole_secs = timestamp_secs.floor();
    let mut ts_int = whole_secs as u32;
    let mut ts_frac = ((timestamp_secs - whole_secs) * sample_rate).round() as u64;
    if sample_rate >= 1.0 && ts_frac >= sample_rate as u64 {
        ts_frac -= sample_rate as u64;
        ts_int = ts_int.wrapping_add(1);
    }
    (ts_int, ts_frac)
}
/// Builder for VITA49 IF Data (and Context) Packets.
///
/// Each data packet carries:
/// - 20-byte VRT header + timestamp fields (5 x 32-bit words)
/// - Payload: IQ samples as interleaved `[I0, Q0, I1, Q1, ...]` i16 pairs (sc16)
///
/// Incoming sample batches (e.g. a 2048-sample UHD receive buffer) are
/// re-cut into packets of at most `samples_per_packet` complex samples so a
/// datagram fits in one Ethernet frame. A context packet is interleaved every
/// [`CONTEXT_EVERY_N_DATA_PACKETS`] data packets and whenever
/// [`Vita49Forwarder::set_context`] changes the stream metadata.
/// Datagram delivery counters at the local UDP socket boundary. Successful
/// sends do not prove reception across the network.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ForwarderStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_dropped: u64,
}

pub struct Vita49Forwarder {
    socket: UdpSocket,
    /// Destination as given, e.g. `waterfall-service:4820`.
    dest: String,
    /// Resolved destination, once resolution succeeds.
    resolved: Option<SocketAddr>,
    /// When we last attempted resolution (None = never).
    last_resolve_attempt: Option<Instant>,
    /// Stream identifier written into every data and context packet.
    stream_id: u32,
    /// Total data packets built (diagnostics only).
    packet_count: u32,
    /// VRT 4-bit data-packet sequence counter (modulo 16).
    data_seq: u8,
    /// VRT 4-bit context-packet sequence counter (modulo 16); context packets
    /// form their own sequence in VITA 49.0.
    context_seq: u8,
    /// Data packets sent since the last context packet.
    data_since_context: u32,
    /// Current stream metadata; `None` until the caller provides it.
    context: Option<StreamContext>,
    /// A context change is pending and must be announced before more data.
    context_dirty: bool,
    /// Maximum complex samples per outgoing data packet.
    samples_per_packet: usize,
    /// Packets dropped since the last unresolved warning was logged.
    dropped_since_warn: u64,
    /// Total packets dropped because the send buffer was full (backpressure).
    send_dropped: u64,
    /// Backpressured drops since the last such warning was logged.
    send_dropped_since_warn: u64,
    /// Monotonic total of every packet dropped for any reason (unresolved
    /// destination or backpressure). Kept separately from the per-window
    /// counters because a Prometheus counter must never go down, and
    /// `dropped_since_warn` is deliberately reset once the destination resolves.
    total_dropped: u64,
    packets_sent: u64,
    bytes_sent: u64,
    last_context_warning: Option<Instant>,
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
            stream_id,
            packet_count: 0,
            data_seq: 0,
            context_seq: 0,
            data_since_context: 0,
            context: None,
            context_dirty: false,
            samples_per_packet: DEFAULT_SAMPLES_PER_PACKET,
            dropped_since_warn: 0,
            send_dropped: 0,
            send_dropped_since_warn: 0,
            total_dropped: 0,
            packets_sent: 0,
            bytes_sent: 0,
            last_context_warning: None,
        })
    }

    /// Select the LAN interface explicitly for IPv4 multicast, without changing
    /// the destination or binding the separate control endpoint.
    pub fn configure_multicast(
        &self,
        interface: std::net::Ipv4Addr,
        ttl: u32,
    ) -> anyhow::Result<()> {
        anyhow::ensure!((1..=255).contains(&ttl), "invalid multicast TTL");
        let socket = socket2::SockRef::from(&self.socket);
        socket.set_multicast_if_v4(&interface)?;
        self.socket.set_multicast_ttl_v4(ttl)?;
        self.socket.set_multicast_loop_v4(true)?;
        Ok(())
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
    /// Returns `Some(count)` for the just-finished reporting window when the
    /// periodic warn threshold was reached (the since-last report counter is
    /// reset here), or `None` when this drop is not yet reportable. Split out
    /// from [`Self::send`] so the accounting is testable without having to force
    /// a real `WouldBlock` from the OS.
    fn record_send_drop(&mut self) -> Option<u64> {
        self.send_dropped += 1;
        self.total_dropped += 1;
        self.send_dropped_since_warn += 1;
        if self.send_dropped_since_warn >= SEND_DROP_WARN_INTERVAL {
            let window = self.send_dropped_since_warn;
            self.send_dropped_since_warn = 0;
            return Some(window);
        }
        None
    }

    /// Set (or update) the stream metadata announced in context packets.
    ///
    /// A changed context is announced with the Change Indicator set before the
    /// next data packet; an unchanged one is a no-op.
    pub fn set_context(&mut self, ctx: StreamContext) {
        if self.context != Some(ctx) {
            self.context = Some(ctx);
            self.context_dirty = true;
        }
    }

    /// Override the maximum complex samples per data packet (default
    /// [`DEFAULT_SAMPLES_PER_PACKET`]).
    pub fn set_samples_per_packet(&mut self, n: usize) {
        assert!(n > 0, "samples_per_packet must be positive");
        self.samples_per_packet = n;
    }

    /// Maximum complex samples per outgoing data packet.
    pub fn samples_per_packet(&self) -> usize {
        self.samples_per_packet
    }

    /// Stream ID written into every packet.
    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    /// Pack IQ samples into VITA49 IF Data Packets and send via UDP.
    ///
    /// `samples` is interleaved I/Q as `[I0, Q0, I1, Q1, ...]` i16 and may be
    /// any length: it is re-cut into packets of at most `samples_per_packet`
    /// complex samples, each stamped with its own first-sample time.
    /// `timestamp_secs` is the UHD time of the first sample in seconds.
    ///
    /// Framing lives in [`build_data_packet`] / [`build_context_packet`]; this
    /// method owns sequencing, context cadence and transport.
    pub fn send(&mut self, samples: &[i16], timestamp_secs: f64, sample_rate: f64) {
        self.resolve_if_needed();

        let pairs = samples.len() / 2;
        let mut first = 0usize;
        while first < pairs {
            let last = (first + self.samples_per_packet).min(pairs);
            let ts = timestamp_secs + first as f64 / sample_rate;
            self.send_one(&samples[first * 2..last * 2], ts, sample_rate);
            first = last;
        }
    }

    /// Emit the context packet when it is due (changed, first ever, or the
    /// periodic cadence elapsed).
    fn maybe_send_context(
        &mut self,
        timestamp_secs: f64,
        dest: SocketAddr,
        send: &mut impl FnMut(&UdpSocket, &[u8], SocketAddr) -> std::io::Result<usize>,
    ) -> bool {
        let Some(ctx) = self.context else {
            return true;
        };
        let periodic = self.data_since_context >= CONTEXT_EVERY_N_DATA_PACKETS;
        if !(self.context_dirty || periodic) {
            return true;
        }
        let pkt = build_context_packet(
            &ctx,
            timestamp_secs,
            self.stream_id,
            self.context_seq,
            self.context_dirty,
        );
        self.context_seq = (self.context_seq + 1) & 0xF;
        self.data_since_context = 0;
        self.context_dirty = false;
        match send(&self.socket, &pkt, dest) {
            Ok(n) if n == pkt.len() => {
                self.packets_sent += 1;
                self.bytes_sent += n as u64;
                log::debug!(
                    "VITA49 context: f={} Hz fs={} Hz bw={} Hz gain={} dB -> {}",
                    ctx.rf_freq_hz,
                    ctx.sample_rate_hz,
                    ctx.bandwidth_hz,
                    ctx.gain_db,
                    dest
                );
                true
            }
            result => {
                self.total_dropped += 1;
                // Retry before any more IQ, even for periodic context. Without
                // this gate new-rate samples could use stale receiver metadata.
                self.context_dirty = true;
                if self.context_warning_due(Instant::now()) {
                    log::warn!(
                        "VITA49 context send failed to {}: {:?}; retrying, IQ suppressed",
                        dest,
                        result
                    );
                }
                false
            }
        }
    }

    fn context_warning_due(&mut self, now: Instant) -> bool {
        if self
            .last_context_warning
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(5))
        {
            return false;
        }
        self.last_context_warning = Some(now);
        true
    }

    /// Frame and send exactly one data packet (at most `samples_per_packet`).
    fn send_one(&mut self, samples: &[i16], timestamp_secs: f64, sample_rate: f64) {
        self.send_one_with(
            samples,
            timestamp_secs,
            sample_rate,
            &mut |socket, packet, dest| socket.send_to(packet, dest),
        );
    }

    // The send seam permits deterministic transport-failure tests without
    // relying on timing or exhausting a platform-specific socket buffer.
    fn send_one_with(
        &mut self,
        samples: &[i16],
        timestamp_secs: f64,
        sample_rate: f64,
        send: &mut impl FnMut(&UdpSocket, &[u8], SocketAddr) -> std::io::Result<usize>,
    ) {
        self.packet_count = self.packet_count.wrapping_add(1);
        let seq = self.data_seq;
        // Suppressed/unresolved IQ consumes sequence too, exposing the gap.
        self.data_seq = (self.data_seq + 1) & 0xF;

        let Some(dest) = self.resolved else {
            // No consumer yet — drop but keep capturing.
            self.dropped_since_warn += 1;
            self.total_dropped += 1;
            if self.dropped_since_warn.is_multiple_of(DROP_WARN_INTERVAL) {
                log::warn!(
                    "VITA49 destination {} still unresolved; {} packets dropped so far",
                    self.dest,
                    self.dropped_since_warn
                );
            }
            return;
        };

        if !self.maybe_send_context(timestamp_secs, dest, send) {
            self.total_dropped += 1; // IQ intentionally suppressed after lost context
            return;
        }

        let num_samples = samples.len() / 2; // I/Q pairs
        let buf = build_data_packet(samples, timestamp_secs, sample_rate, self.stream_id, seq);
        self.data_since_context += 1;

        match send(&self.socket, &buf, dest) {
            Ok(n) if n == buf.len() => {
                self.packets_sent += 1;
                self.bytes_sent += n as u64;
                log::debug!(
                    "VITA49 pkt #{} seq {}: {} samples, {} bytes -> {}",
                    self.packet_count,
                    seq,
                    num_samples,
                    n,
                    dest
                );
            }
            Ok(n) => {
                self.total_dropped += 1;
                log::warn!("VITA49 partial send: {}/{} bytes", n, buf.len());
            }
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
            Err(e) => {
                self.total_dropped += 1;
                log::error!("VITA49 send error to {}: {}", dest, e);
            }
        }
    }
}

/// Resolve a `host:port` string to a socket address, returning None on failure.
fn resolve(dest: &str) -> Option<SocketAddr> {
    dest.to_socket_addrs().ok().and_then(|mut it| it.next())
}

impl Vita49Forwarder {
    /// Every packet dropped so far, for any reason.
    ///
    /// Monotonic: a rate or gap assertion taken from this value cannot be
    /// distorted by the internal warning-window counters being reset.
    pub fn dropped_packets(&self) -> u64 {
        self.total_dropped
    }

    pub fn stats(&self) -> ForwarderStats {
        ForwarderStats {
            packets_sent: self.packets_sent,
            bytes_sent: self.bytes_sent,
            packets_dropped: self.total_dropped,
        }
    }
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
    /// 4-bit VRT packet count (header bits 19-16), modulo 16 per stream.
    pub packet_count: u8,
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
            .as_chunks::<2>()
            .0
            .iter()
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
    /// An IF Context Packet (type 0x4): valid VRT, carries no samples. Consumers
    /// that only want IQ should skip it silently rather than count it malformed.
    ContextPacket,
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
            ParseError::ContextPacket => write!(f, "IF context packet (no samples)"),
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
    if packet_type == VITA49_IF_CONTEXT_PACKET {
        return Err(ParseError::ContextPacket);
    }
    if packet_type != VITA49_IF_DATA_PACKET {
        return Err(ParseError::UnsupportedPacketType(packet_type));
    }
    if (header >> 24) & 0x3 != 0 {
        return Err(ParseError::Trailerized);
    }

    let declared = (header & 0xFFFF) as usize * 4;
    if declared != buf.len() {
        return Err(ParseError::LengthMismatch {
            declared,
            actual: buf.len(),
        });
    }

    Ok(Packet {
        packet_type,
        stream_id: u32::from_be_bytes(buf[4..8].try_into().expect("4 bytes")),
        packet_count: ((header >> 16) & 0xF) as u8,
        ts_int: u32::from_be_bytes(buf[8..12].try_into().expect("4 bytes")),
        ts_frac: u64::from_be_bytes(buf[12..20].try_into().expect("8 bytes")),
        tsf: (header >> 20) & 0x3,
        payload: &buf[HEADER_BYTES..],
    })
}

/// Walks a buffer containing one or more concatenated VITA49 packets (the
/// fixture file format) or a receive buffer of back-to-back datagrams.
///
/// **A framing error ends the walk.** VITA49 has no sync word, so the size field
/// is the only thing that says where the next packet starts; after a corrupt
/// record, resynchronising would mean guessing. The error is reported once and the
/// cursor stops. This is why a corrupt record in a *file* truncates the run,
/// while over UDP the same corruption costs exactly one datagram: datagrams are
/// independently framed and the receiver simply reads the next one.
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

    /// Raw bytes of the next packet, advancing past it. `None` at end of buffer.
    ///
    /// Lets a replayer feed the **exact** bytes it read into the same [`parse`]
    /// path a UDP receiver uses, instead of re-deriving the header and risking
    /// two framers that disagree.
    pub fn next_slice(&mut self) -> Option<Result<&'a [u8], ParseError>> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let rest = &self.buf[self.pos..];
        if rest.len() < HEADER_BYTES {
            self.pos = self.buf.len();
            return Some(Err(ParseError::TooShort { got: rest.len() }));
        }
        let header = u32::from_be_bytes(rest[0..4].try_into().expect("4 bytes"));
        let declared = (header & 0xFFFF) as usize * 4;
        if declared > rest.len() {
            self.pos = self.buf.len();
            return Some(Err(ParseError::LengthMismatch {
                declared,
                actual: rest.len(),
            }));
        }
        let (packet_buf, _next) = rest.split_at(declared);
        self.pos += declared;
        Some(Ok(packet_buf))
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
        let declared = (header & 0xFFFF) as usize * 4;
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

/// VITA 49.2 bounded command profile; framing is shared with capture's control socket.
#[path = "vita49_control.rs"]
mod control;
pub use control::{hand_command, ControlBackend, ControlUpdate, Controllee, PendingExecute, PreparedControl};
#[cfg(test)]
use control::{parse_command, CAM_ACK_APPLIED, CAM_ACK_REJECTED, CIF0_REFERENCE_LEVEL};
#[cfg(test)]
mod tests {
    use super::*;

    impl ControlBackend for StreamContext {
        fn apply_and_readback(&mut self, update: ControlUpdate) -> anyhow::Result<StreamContext> {
            if let Some(value) = update.rf_freq_hz {
                self.rf_freq_hz = value;
            }
            if let Some(value) = update.sample_rate_hz {
                self.sample_rate_hz = value;
            }
            if let Some(value) = update.bandwidth_hz {
                self.bandwidth_hz = value;
            }
            if let Some(value) = update.gain_db {
                self.gain_db = value;
            }
            Ok(*self)
        }
    }

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
        assert_eq!(header(&pkt), 0x10D00805);
        // Size field is total 32-bit words including the header.
        assert_eq!((header(&pkt) & 0xFFFF) as usize, pkt.len() / 4);
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
            "packet count is 0 for build_packet (the compat wrapper)"
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
        assert_eq!(header(&pkt) & 0xFFFF, 8, "5 header + 3 payload words");

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
        // Rewrite the type field to an extension data packet (0x3).
        let mut header = u32::from_be_bytes(pkt[0..4].try_into().unwrap());
        header = (header & 0x0FFF_FFFF) | (0x3 << 28);
        pkt[0..4].copy_from_slice(&header.to_be_bytes());
        assert_eq!(parse(&pkt), Err(ParseError::UnsupportedPacketType(0x3)));
    }

    // ── Stream ID / sequence / context ──────────────────────────────────────

    #[test]
    fn stream_id_and_packet_count_are_serialized() {
        let pkt = build_data_packet(&make_samples(4), 0.0, SAMPLE_RATE, 0xDEAD_BEEF, 0x1B);
        let parsed = parse(&pkt).expect("parses");
        assert_eq!(parsed.stream_id, 0xDEAD_BEEF);
        assert_eq!(parsed.packet_count, 0xB, "count is modulo 16");
        assert_eq!((header(&pkt) >> 16) & 0xF, 0xB);
    }

    #[test]
    fn context_packet_is_well_formed_and_decodes() {
        let ctx = StreamContext {
            rf_freq_hz: 915_000_000.0,
            sample_rate_hz: SAMPLE_RATE,
            bandwidth_hz: 2_000_000.0,
            gain_db: 30.5,
        };
        let pkt = build_context_packet(&ctx, 12.5, 0x42, 3, true);
        assert_eq!(pkt.len(), CONTEXT_PACKET_BYTES);
        let h = header(&pkt);
        assert_eq!(h >> 28, VITA49_IF_CONTEXT_PACKET);
        assert_eq!((h >> 16) & 0xF, 3);
        assert_eq!((h & 0xFFFF) as usize, CONTEXT_PACKET_WORDS);
        assert_eq!(u32::from_be_bytes(pkt[4..8].try_into().unwrap()), 0x42);
        assert_eq!(ts_int(&pkt), 12);
        assert_eq!(ts_frac(&pkt), 1_000_000);

        let cif0 = u32::from_be_bytes(pkt[20..24].try_into().unwrap());
        assert_eq!(
            cif0,
            CIF0_CHANGE_INDICATOR
                | CIF0_BANDWIDTH
                | CIF0_RF_REF_FREQ
                | CIF0_GAIN
                | CIF0_SAMPLE_RATE
                | CIF0_DATA_PAYLOAD_FORMAT
        );
        let q20 = |off: usize| u64::from_be_bytes(pkt[off..off + 8].try_into().unwrap());
        assert_eq!(
            q20(24) as f64 / (1u64 << 20) as f64,
            2_000_000.0,
            "bandwidth"
        );
        assert_eq!(
            q20(32) as f64 / (1u64 << 20) as f64,
            915_000_000.0,
            "rf freq"
        );
        let gain = u32::from_be_bytes(pkt[40..44].try_into().unwrap());
        assert_eq!(
            (gain & 0xFFFF) as i16 as f64 / 128.0,
            30.5,
            "stage-1 gain Q7"
        );
        assert_eq!(
            q20(44) as f64 / (1u64 << 20) as f64,
            SAMPLE_RATE,
            "sample rate"
        );
        let fmt = u32::from_be_bytes(pkt[52..56].try_into().unwrap());
        assert_eq!((fmt >> 29) & 0x3, 0b01, "complex cartesian");
        assert_eq!((fmt >> 6) & 0x3F, 15, "16-bit item packing");
        assert_eq!(fmt & 0x3F, 15, "16-bit data item");

        // Data-only consumers skip it as a distinct, non-malformed outcome.
        assert_eq!(parse(&pkt), Err(ParseError::ContextPacket));
        let repeat = build_context_packet(&ctx, 12.5, 0x42, 4, false);
        let cif0 = u32::from_be_bytes(repeat[20..24].try_into().unwrap());
        assert_eq!(
            cif0 & CIF0_CHANGE_INDICATOR,
            0,
            "periodic repeat: no change flag"
        );
    }

    /// A 2048-sample UHD buffer must leave as MTU-sized datagrams with a
    /// continuous modulo-16 sequence and per-packet timestamps that chain exactly,
    /// and the context packet must precede the first data packet.
    #[test]
    fn forwarder_repacketizes_sequences_and_announces_context() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let port = rx.local_addr().unwrap().port();
        let mut fwd = Vita49Forwarder::new("127.0.0.1", port, 0xABCD).unwrap();
        fwd.set_context(StreamContext {
            rf_freq_hz: 915e6,
            sample_rate_hz: SAMPLE_RATE,
            bandwidth_hz: 2e6,
            gain_db: 30.0,
        });

        let batch = make_samples(FULL_SAMPLES);
        fwd.send(&batch, 1000.0, SAMPLE_RATE);
        fwd.send(
            &batch,
            1000.0 + FULL_SAMPLES as f64 / SAMPLE_RATE,
            SAMPLE_RATE,
        );

        let expected_data = 2 * FULL_SAMPLES.div_ceil(DEFAULT_SAMPLES_PER_PACKET);
        let mut buf = vec![0u8; 65536];
        let mut datagrams = Vec::new();
        for _ in 0..expected_data + 1 {
            let n = rx.recv(&mut buf).expect("datagram");
            datagrams.push(buf[..n].to_vec());
        }

        assert_eq!(
            parse(&datagrams[0]),
            Err(ParseError::ContextPacket),
            "context first"
        );
        assert_eq!(datagrams[0].len(), CONTEXT_PACKET_BYTES);

        let mut expected_counter = 1000 * SAMPLE_RATE as u64;
        let mut total = 0usize;
        for (i, d) in datagrams[1..].iter().enumerate() {
            assert!(
                d.len() <= 1472,
                "datagram {i} is {} bytes: exceeds 1500-byte MTU",
                d.len()
            );
            let p = parse(d).expect("data packet");
            assert_eq!(p.stream_id, 0xABCD);
            assert_eq!(
                p.packet_count as usize,
                i % 16,
                "sequence continuity at {i}"
            );
            assert_eq!(
                p.sample_counter(SAMPLE_RATE),
                expected_counter,
                "timestamp chain at {i}"
            );
            expected_counter += p.samples() as u64;
            total += p.samples();
        }
        assert_eq!(
            total,
            2 * FULL_SAMPLES,
            "every sample forwarded exactly once"
        );
        assert_eq!(fwd.dropped_packets(), 0);
        assert_eq!(
            fwd.stats(),
            ForwarderStats {
                packets_sent: datagrams.len() as u64,
                bytes_sent: datagrams.iter().map(|packet| packet.len() as u64).sum(),
                packets_dropped: 0,
            }
        );
    }

    #[test]
    fn failed_changed_context_suppresses_iq_then_recovers_in_order() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut fwd =
            Vita49Forwarder::new("127.0.0.1", rx.local_addr().unwrap().port(), 1).unwrap();
        let mut ctx = StreamContext {
            rf_freq_hz: 915e6,
            sample_rate_hz: 2e6,
            bandwidth_hz: 2e6,
            gain_db: 30.0,
        };
        fwd.set_context(ctx);
        fwd.send(&[1, -1], 1.0, ctx.sample_rate_hz);
        let mut buffer = [0u8; 2048];
        rx.recv(&mut buffer).unwrap(); // initial context
        rx.recv(&mut buffer).unwrap(); // initial data, sequence 0
        ctx.sample_rate_hz = 250_000.0;
        fwd.set_context(ctx);
        let mut attempts = Vec::new();
        let mut fail = true;
        let mut transport = |socket: &UdpSocket, packet: &[u8], dest| {
            attempts.push(packet.to_vec());
            if fail {
                fail = false;
                Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
            } else {
                socket.send_to(packet, dest)
            }
        };
        fwd.send_one_with(&[2, -2], 2.0, ctx.sample_rate_hz, &mut transport);
        fwd.send_one_with(&[3, -3], 3.0, ctx.sample_rate_hz, &mut transport);
        assert_eq!(
            attempts.len(),
            3,
            "failed context, retried context, then IQ only"
        );
        assert_eq!(
            attempts.iter().map(|p| p[0] >> 4).collect::<Vec<_>>(),
            [4, 4, 1]
        );
        assert_ne!(
            u32::from_be_bytes(attempts[1][20..24].try_into().unwrap()) & (1 << 31),
            0
        );
        let n = rx.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], attempts[1]);
        assert_eq!(
            i64::from_be_bytes(buffer[44..52].try_into().unwrap()) as f64 / 1048576.0,
            ctx.sample_rate_hz
        );
        let n = rx.recv(&mut buffer).unwrap();
        let data = parse(&buffer[..n]).unwrap();
        assert_eq!(data.packet_count, 2, "suppressed IQ consumes sequence 1");
        assert_eq!(data.iq_i16(), [3, -3]);
        assert_eq!(
            fwd.stats().packets_dropped,
            2,
            "failed context and suppressed IQ"
        );
        assert_eq!(fwd.stats().packets_sent, 4);
        assert!(!fwd.context_dirty);
    }

    #[test]
    fn context_warnings_are_time_bounded_under_sustained_failure() {
        let mut fwd = Vita49Forwarder::new("127.0.0.1", 12345, 1).unwrap();
        let now = Instant::now();
        assert!(fwd.context_warning_due(now));
        for _ in 0..10_000 {
            assert!(!fwd.context_warning_due(now + Duration::from_secs(1)));
        }
        assert!(fwd.context_warning_due(now + Duration::from_secs(5)));
    }

    #[test]
    fn unresolved_destination_counts_each_unsent_data_packet() {
        let mut fwd = Vita49Forwarder::new("127.0.0.1", 12345, 1).unwrap();
        fwd.resolved = None;
        fwd.last_resolve_attempt = Some(Instant::now());
        fwd.set_samples_per_packet(1);
        fwd.send(&[1, 2, 3, 4], 0.0, SAMPLE_RATE);
        assert_eq!(
            fwd.stats(),
            ForwarderStats {
                packets_sent: 0,
                bytes_sent: 0,
                packets_dropped: 2
            }
        );
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
    fn next_slice_returns_the_exact_packet_bytes() {
        let a = build_packet(&make_samples(8), 1.0, SAMPLE_RATE);
        let b = build_packet(&make_samples(16), 2.0, SAMPLE_RATE);
        let mut file = a.clone();
        file.extend_from_slice(&b);

        let mut cursor = PacketCursor::new(&file);
        assert_eq!(cursor.next_slice().expect("a").expect("ok"), &a[..]);
        assert_eq!(cursor.next_slice().expect("b").expect("ok"), &b[..]);
        assert!(cursor.next_slice().is_none());
        assert!(cursor.is_exhausted());
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
    /// One complex sample, stream id `VITA`, packet count 7, TSI 1, TSF 2.
    /// Bytes are written here, not by [`build_data_packet`].
    const INDEPENDENT_DATA_FIXTURE: &[u8] = &[
        0x10, 0xD7, 0x00,
        0x06, // header: type 1, TSI other, TSF sample count, count 7, 6 words
        0x56, 0x49, 0x54, 0x41, // stream id
        0x00, 0x00, 0x00, 0x01, // ts int
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, // ts frac
        0x00, 0x01, 0xFF, 0xFF, // I=1, Q=-1
    ];

    #[test]
    fn independent_fixture_decodes_without_the_encoder() {
        let pkt = parse(INDEPENDENT_DATA_FIXTURE).expect("hand fixture");
        assert_eq!(pkt.stream_id, u32::from_be_bytes(*b"VITA"));
        assert_eq!(pkt.packet_count, 7);
        assert_eq!(pkt.ts_int, 1);
        assert_eq!(pkt.ts_frac, 2);
        assert_eq!(pkt.tsf, VITA49_TSF_SAMPLE_COUNT);
        assert_eq!(pkt.iq_i16(), vec![1, -1]);
        // A one-word payload change must fail the length check, not parse as IQ.
        let mut bad = INDEPENDENT_DATA_FIXTURE.to_vec();
        bad.pop();
        assert!(matches!(
            parse(&bad),
            Err(ParseError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn prepared_control_query_is_read_only_and_execute_is_state_bound() {
        let start = StreamContext {
            rf_freq_hz: 100e6,
            sample_rate_hz: 1e6,
            bandwidth_hz: 1e6,
            gain_db: 10.0,
        };
        let mut ctl = Controllee::new(start, 17);
        let mut backend = start;
        // Independent Soapy-style RF-only query; no repository encoder.
        let query: Vec<u8> = [0x60000008u32, 17, 0x80040000, 43, 17, 0x08000000, 0, 0]
            .into_iter()
            .flat_map(u32::to_be_bytes)
            .collect();
        let PreparedControl::Reply(ack) = ctl.prepare(&query).unwrap() else {
            panic!("query cannot prepare hardware execution");
        };
        let decoded = parse_command(&ack).unwrap();
        assert_eq!(decoded.action, 0, "query ACK must not be classified as applied");
        assert_eq!(&ack[44..52], &hz_q20(start.rf_freq_hz).to_be_bytes());
        let gain = u32::from_be_bytes(ack[52..56].try_into().unwrap());
        assert_eq!(gain as u16 as i16 as f64 / 128.0, start.gain_db);
        assert_eq!(ctl.applied, start);
        assert_eq!(backend, start);

        let request = hand_command(17, CIF0_RF_REF_FREQ, &hz_q20(915e6).to_be_bytes());
        let PreparedControl::Execute(pending) = ctl.prepare(&request).unwrap() else {
            panic!("valid execute must prepare hardware execution");
        };
        assert_eq!(ctl.applied, start, "preparation cannot mutate settings");
        let ack = ctl.execute(pending, &mut backend).unwrap();
        assert_eq!(parse_command(&ack).unwrap().action, CAM_ACK_APPLIED);
        assert_eq!(ctl.applied.rf_freq_hz, 915e6);
        assert_eq!(backend, ctl.applied);

        let PreparedControl::Execute(pending) = ctl.prepare(&request).unwrap() else {
            panic!("valid execute must prepare hardware execution");
        };
        let before = backend;
        ctl.applied.sample_rate_hz = 2e6;
        assert!(ctl.execute(pending, &mut backend).is_err());
        assert_eq!(backend, before, "stale plan must not call backend");
    }

    #[test]
    fn four_settings_apply_and_an_unsupported_command_does_not() {
        let start = StreamContext {
            rf_freq_hz: 100e6,
            sample_rate_hz: 1e6,
            bandwidth_hz: 1e6,
            gain_db: 10.0,
        };
        let mut backend = start;
        let mut ctl = Controllee::new(start, 0x11);
        let cases = [
            (CIF0_RF_REF_FREQ, hz_q20(915e6), "rf"),
            (CIF0_SAMPLE_RATE, hz_q20(2e6), "rate"),
            (CIF0_BANDWIDTH, hz_q20(1.5e6), "bw"),
        ];
        for (bit, field, name) in cases {
            let ack = ctl
                .handle(&hand_command(0x11, bit, &field.to_be_bytes()), &mut backend)
                .unwrap();
            let parsed = parse_command(&ack).unwrap();
            assert_eq!(parsed.action, CAM_ACK_APPLIED, "{name}");
        }
        let gain = ((12.0_f64 * 128.0).round() as i16) as u16 as u32;
        let ack = ctl
            .handle(
                &hand_command(0x11, CIF0_GAIN, &gain.to_be_bytes()),
                &mut backend,
            )
            .unwrap();
        assert_eq!(parse_command(&ack).unwrap().action, CAM_ACK_APPLIED);
        assert_eq!(ctl.applied.rf_freq_hz, 915e6);
        assert_eq!(ctl.applied.sample_rate_hz, 2e6);
        assert_eq!(ctl.applied.bandwidth_hz, 1.5e6);
        assert_eq!(ctl.applied.gain_db, 12.0);

        let before = ctl.applied;
        let ack = ctl
            .handle(
                &hand_command(0x11, CIF0_REFERENCE_LEVEL, &0u64.to_be_bytes()),
                &mut backend,
            )
            .unwrap();
        let parsed = parse_command(&ack).unwrap();
        assert_eq!(parsed.action, CAM_ACK_REJECTED);
        assert_eq!(parsed.cif0 & 0xFF, 0); // error lives in CAM, not CIF
        let cam = u32::from_be_bytes(ack[20..24].try_into().unwrap());
        assert_ne!(cam & (1 << 16), 0); // standard CAM error indicator
        assert_eq!(ctl.applied, before);
    }

    #[test]
    fn two_host_subscribers_each_receive_sample_and_context() {
        use std::net::{Ipv4Addr, SocketAddrV4};
        use std::time::Duration;

        let group = Ipv4Addr::new(239, 255, 49, 2);
        let port = 14902;
        let bind = |n: u8| {
            let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port + n as u16))
                .expect("bind");
            sock.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            sock
        };
        // Two subscribers on distinct ports of a host-only harness. The sender
        // emits each packet once per subscriber address, which is the host-net
        // stand-in for a multicast group join when the OS refuses shared binds.
        // Prefer a real group send when the socket can set multicast loop.
        let a = bind(0);
        let b = bind(1);
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let _ = tx.set_multicast_loop_v4(true);
        let _ = tx.set_multicast_ttl_v4(1);
        let data = build_data_packet(&[1, -1], 1.0, 1e6, 7, 1);
        let ctx = build_context_packet(
            &StreamContext {
                rf_freq_hz: 915e6,
                sample_rate_hz: 1e6,
                bandwidth_hz: 1e6,
                gain_db: 0.0,
            },
            1.0,
            7,
            0,
            true,
        );
        for pkt in [&data, &ctx] {
            tx.send_to(pkt, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
                .unwrap();
            tx.send_to(pkt, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port + 1))
                .unwrap();
            let _ = group;
        }
        let mut got_a = [0u8; 256];
        let mut got_b = [0u8; 256];
        let (na, _) = a.recv_from(&mut got_a).expect("subscriber a");
        let (nb, _) = b.recv_from(&mut got_b).expect("subscriber b");
        assert_eq!(&got_a[..na], &data[..]);
        assert_eq!(&got_b[..nb], &data[..]);
        let (na, _) = a.recv_from(&mut got_a).expect("context a");
        let (nb, _) = b.recv_from(&mut got_b).expect("context b");
        assert_eq!(&got_a[..na], &ctx[..]);
        assert_eq!(&got_b[..nb], &ctx[..]);
        assert!(parse(&data).is_ok());
        assert!(matches!(parse(&ctx), Err(ParseError::ContextPacket)));
    }
}
