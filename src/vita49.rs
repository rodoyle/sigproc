//! Minimal VITA 49.0 (VRT) IF Data Packet framing and UDP forwarding.
//!
//! Encodes IQ samples as sc16 (interleaved 16-bit signed) VITA49 packets
//! and sends them via UDP to the configured destination.

use std::net::UdpSocket;

/// VITA 49.0 IF Data Packet header constants.
const VITA49_IF_DATA_PACKET: u32 = 0x1;
const VITA49_TSI_OTHER: u32 = 0x3; // free-running timestamp
const VITA49_TSF_SAMPLE_COUNT: u32 = 0x1;

/// Builder for VITA49 IF Data Packets.
///
/// Each packet carries:
/// - 28-byte VRT header (packet type, stream ID, timestamp fields)
/// - Payload: IQ samples as interleaved `[I0, Q0, I1, Q1, ...]` i16 pairs (sc16)
pub struct Vita49Forwarder {
    socket: UdpSocket,
    stream_id: u32,
    packet_count: u32,
}

impl Vita49Forwarder {
    /// Create a new VITA49 forwarder bound to an ephemeral local port.
    pub fn new(dest_host: &str, dest_port: u16, stream_id: u32) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.connect(format!("{}:{}", dest_host, dest_port))?;
        // Non-blocking send to avoid stalling the capture loop
        socket.set_nonblocking(true)?;
        log::info!(
            "VITA49 forwarder: stream_id=0x{:08x} -> {}:{}",
            stream_id,
            dest_host,
            dest_port
        );
        Ok(Self {
            socket,
            stream_id,
            packet_count: 0,
        })
    }

    /// Pack IQ samples into a VITA49 IF Data Packet and send via UDP.
    ///
    /// `samples` is interleaved I/Q as `[I0, Q0, I1, Q1, ...]` i16.
    /// `timestamp` is the UHD time in seconds (full + fractional).
    ///
    /// Packet layout (big-endian words):
    ///   Word 0: Packet header
    ///   Word 1: Stream ID
    ///   Word 2: Timestamp integer seconds (u32)
    ///   Word 3-4: Timestamp fractional seconds (u64, sample-count format)
    ///   Words 5+: IQ payload (sc16 interleaved)
    pub fn send(&mut self, samples: &[i16], timestamp_secs: f64, sample_rate: f64) {
        self.packet_count = self.packet_count.wrapping_add(1);

        let num_samples = samples.len() / 2; // I/Q pairs
        let payload_words = samples.len(); // each i16 = 1 16-bit word
        let total_words = 5 + payload_words; // header(1) + stream_id(1) + ts_int(1) + ts_frac(2) + payload
        let packet_size_words = (total_words - 1) as u16; // VITA49: total words minus 1

        // Build header word
        let header: u32 = (VITA49_IF_DATA_PACKET << 28)
            | (VITA49_TSI_OTHER << 22)
            | (VITA49_TSF_SAMPLE_COUNT << 20)
            | (packet_size_words as u32);

        // Timestamp fields
        let ts_int = timestamp_secs as u32;
        let ts_frac = ((timestamp_secs - timestamp_secs.floor()) * sample_rate) as u64;

        // Assemble packet buffer
        let buf_len = total_words * 2; // 2 bytes per word
        let mut buf = vec![0u8; buf_len];

        // Word 0: Header
        buf[0..4].copy_from_slice(&header.to_be_bytes());

        // Word 1: Stream ID
        buf[4..8].copy_from_slice(&self.stream_id.to_be_bytes());

        // Word 2: Timestamp integer seconds
        buf[8..12].copy_from_slice(&ts_int.to_be_bytes());

        // Words 3-4: Timestamp fractional (sample count)
        buf[12..20].copy_from_slice(&ts_frac.to_be_bytes());

        // Words 5+: IQ payload — pack each i16 as big-endian
        for (i, &sample) in samples.iter().enumerate() {
            let offset = 20 + i * 2;
            buf[offset..offset + 2].copy_from_slice(&sample.to_be_bytes());
        }

        // Fire-and-forget UDP send (non-blocking)
        match self.socket.send(&buf) {
            Ok(n) if n == buf.len() => {
                log::debug!(
                    "VITA49 pkt #{}: {} samples, {} bytes -> {}:{}",
                    self.packet_count,
                    num_samples,
                    n,
                    self.socket
                        .peer_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_default(),
                    ""
                );
            }
            Ok(n) => {
                log::warn!("VITA49 partial send: {}/{} bytes", n, buf.len());
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // UDP send buffer full — drop packet, continue
                log::debug!(
                    "VITA49 send would block, dropping packet #{}",
                    self.packet_count
                );
            }
            Err(e) => {
                log::error!("VITA49 send error: {}", e);
            }
        }
    }
}
