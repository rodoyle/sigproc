//! Bounded VITA 49.2 command controllee. No device is opened here.
use super::{hz_q20, ParseError, StreamContext};

const ACK_HEADER: u32 = 1 << 26;
const CAM_EXECUTE: u32 = 2 << 23;
const CAM_EXECUTION_ACK: u32 = 1 << 19;
const CAM_EXECUTED: u32 = 1 << 10;
const CAM_ERRORS: u32 = 1 << 16;
const BW: u32 = 1 << 29;
const RF: u32 = 1 << 27;
const GAIN: u32 = 1 << 23;
const RATE: u32 = 1 << 21;
const SUPPORTED: u32 = BW | RF | GAIN | RATE;
#[cfg(test)]
pub(super) const CIF0_REFERENCE_LEVEL: u32 = 1 << 26;
#[cfg(test)]
pub(super) const CAM_ACK_APPLIED: u32 = 2;
#[cfg(test)]
pub(super) const CAM_ACK_REJECTED: u32 = 3;

/// Only fields present in a request are sent to hardware.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ControlUpdate {
    pub rf_freq_hz: Option<f64>,
    pub sample_rate_hz: Option<f64>,
    pub bandwidth_hz: Option<f64>,
    pub gain_db: Option<f64>,
}

/// On failure the radio might have changed partially. The owner must terminate
/// capture instead of continuing to advertise an unconfirmed state.
pub trait ControlBackend {
    fn apply_and_readback(&mut self, update: ControlUpdate) -> anyhow::Result<StreamContext>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Controllee {
    pub applied: StreamContext,
    stream_id: u32,
    packet_count: u8,
}

pub(super) struct ParsedCommand {
    stream_id: u32,
    message_id: u32,
    header: u32,
    cam: u32,
    id_flags: u32,
    ids: Vec<u8>,
    controllee_id: Option<u32>,
    pub cif0: u32,
    #[cfg(test)]
    pub action: u32,
    update: ControlUpdate,
}

fn word(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(buf[offset..offset + 4].try_into().expect("checked word"))
}

/// Timestamps and class ID are optional in incoming commands. The required CAM
/// and Message ID follow the VRT prologue, NOT a fixed assumed offset.
pub(super) fn parse_command(buf: &[u8]) -> Result<ParsedCommand, ParseError> {
    if buf.len() < 8 {
        return Err(ParseError::TooShort { got: buf.len() });
    }
    let header = word(buf, 0);
    if header >> 28 != 6 {
        return Err(ParseError::UnsupportedPacketType(header >> 28));
    }
    let declared = (header & 0xffff) as usize * 4;
    if declared != buf.len() {
        return Err(ParseError::LengthMismatch {
            declared,
            actual: buf.len(),
        });
    }
    let tsi = (header >> 22) & 3;
    let tsf = (header >> 20) & 3;
    let mut offset = 8 + if header & (1 << 27) != 0 { 8 } else { 0 };
    offset += if tsi != 0 { 4 } else { 0 };
    offset += if tsf != 0 { 8 } else { 0 };
    if offset + 8 > buf.len() {
        return Err(ParseError::TooShort { got: buf.len() });
    }
    let cam = word(buf, offset);
    let message_id = word(buf, offset + 4);
    offset += 8;
    let ids_start = offset;
    let mut controllee_id = None;
    for (present, uuid) in [(1 << 31, 1 << 30), (1 << 29, 1 << 28)] {
        if cam & present == 0 {
            continue;
        }
        let n = if cam & uuid != 0 { 16 } else { 4 };
        if offset + n > buf.len() {
            return Err(ParseError::TooShort { got: buf.len() });
        }
        if present == 1 << 31 && n == 4 {
            controllee_id = Some(word(buf, offset));
        }
        offset += n;
    }
    let ids = buf[ids_start..offset].to_vec();
    if offset + 4 > buf.len() {
        return Err(ParseError::TooShort { got: buf.len() });
    }
    let cif0 = word(buf, offset);
    offset += 4;
    let mut values = [None; 4];
    // Unknown fields have unknown lengths/order: reject the whole request before
    // interpreting ANY field bodies. A truncated known field remains malformed.
    if cif0 & !SUPPORTED == 0 {
        for (index, bit) in [BW, RF, GAIN, RATE].into_iter().enumerate() {
            if cif0 & bit == 0 {
                continue;
            }
            let n = if bit == GAIN { 4 } else { 8 };
            if offset + n > buf.len() {
                return Err(ParseError::TooShort { got: buf.len() });
            }
            values[index] = Some(if bit == GAIN {
                (word(buf, offset) as u16 as i16) as f64 / 128.0
            } else {
                i64::from_be_bytes(buf[offset..offset + 8].try_into().expect("checked Q20")) as f64
                    / (1u64 << 20) as f64
            });
            offset += n;
        }
        if offset != buf.len() {
            return Err(ParseError::LengthMismatch {
                declared: offset,
                actual: buf.len(),
            });
        }
    }
    Ok(ParsedCommand {
        stream_id: word(buf, 4),
        message_id,
        header,
        cam,
        id_flags: cam & 0xf0000000,
        ids,
        controllee_id,
        cif0,
        #[cfg(test)]
        action: if header & ACK_HEADER != 0 {
            if cam & CAM_ERRORS != 0 {
                CAM_ACK_REJECTED
            } else {
                CAM_ACK_APPLIED
            }
        } else {
            (cam >> 23) & 3
        },
        update: ControlUpdate {
            bandwidth_hz: values[0],
            rf_freq_hz: values[1],
            gain_db: values[2],
            sample_rate_hz: values[3],
        },
    })
}

fn valid(ctx: StreamContext) -> bool {
    [ctx.rf_freq_hz, ctx.sample_rate_hz, ctx.bandwidth_hz]
        .into_iter()
        .all(|hz| hz.is_finite() && hz > 0.0)
        && ctx.gain_db.is_finite()
        && (-256.0..256.0).contains(&ctx.gain_db)
}

impl Controllee {
    pub fn new(applied: StreamContext, stream_id: u32) -> Self {
        Self {
            applied,
            stream_id,
            packet_count: 0,
        }
    }

    /// Immediate execute only. Unsupported fields/modes receive an error ACK
    /// with the Message ID echoed and the last confirmed settings unchanged.
    pub fn handle<B: ControlBackend>(
        &mut self,
        datagram: &[u8],
        backend: &mut B,
    ) -> anyhow::Result<Vec<u8>> {
        let cmd = parse_command(datagram)?;
        let unsupported = cmd.stream_id != self.stream_id
            || cmd.controllee_id.is_some_and(|id| id != self.stream_id)
            || cmd.header & ((1 << 26) | (1 << 24) | (1 << 25)) != 0
            || (cmd.cam >> 23) & 3 != 2
            || cmd.cam & ((1 << 30) | (1 << 28) | (1 << 18) | (7 << 12)) != 0
            || cmd.cif0 == 0
            || cmd.cif0 & !SUPPORTED != 0;
        let before = self.applied;
        let requested = StreamContext {
            rf_freq_hz: cmd.update.rf_freq_hz.unwrap_or(before.rf_freq_hz),
            sample_rate_hz: cmd.update.sample_rate_hz.unwrap_or(before.sample_rate_hz),
            bandwidth_hz: cmd.update.bandwidth_hz.unwrap_or(before.bandwidth_hz),
            gain_db: cmd.update.gain_db.unwrap_or(before.gain_db),
        };
        let mut cam = CAM_EXECUTION_ACK;
        if unsupported || !valid(requested) {
            cam |= CAM_ERRORS;
        } else {
            let applied = backend.apply_and_readback(cmd.update)?;
            anyhow::ensure!(valid(applied), "invalid hardware control readback");
            self.applied = applied;
            cam |= CAM_EXECUTED;
            if applied != requested {
                cam |= 1 << 11;
            }
        }
        Ok(self.ack(&cmd, cam))
    }

    fn ack(&mut self, cmd: &ParsedCommand, cam: u32) -> Vec<u8> {
        let ctx = self.applied;
        let mut buf = vec![0u8; 20];
        buf[4..8].copy_from_slice(&self.stream_id.to_be_bytes());
        buf.extend_from_slice(&(cam | cmd.id_flags).to_be_bytes());
        buf.extend_from_slice(&cmd.message_id.to_be_bytes());
        buf.extend_from_slice(&cmd.ids);
        buf.extend_from_slice(&SUPPORTED.to_be_bytes());
        buf.extend_from_slice(&hz_q20(ctx.bandwidth_hz).to_be_bytes());
        buf.extend_from_slice(&hz_q20(ctx.rf_freq_hz).to_be_bytes());
        let gain = ((ctx.gain_db * 128.0).round() as i16) as u16 as u32;
        buf.extend_from_slice(&gain.to_be_bytes());
        buf.extend_from_slice(&hz_q20(ctx.sample_rate_hz).to_be_bytes());
        let header = (6 << 28)
            | ACK_HEADER
            | (3 << 22)
            | (1 << 20)
            | ((self.packet_count as u32 & 15) << 16)
            | (buf.len() as u32 / 4);
        buf[..4].copy_from_slice(&header.to_be_bytes());
        self.packet_count = self.packet_count.wrapping_add(1);
        buf
    }
}

/// Convenience request builder for local clients/tests (Message ID 1). External
/// clients should assign unique message IDs; the controllee echoes them unchanged.
/// No timestamp means immediate execution. Field bytes use standard CIF0 order.
pub fn hand_command(stream_id: u32, cif0: u32, field: &[u8]) -> Vec<u8> {
    assert!(field.len().is_multiple_of(4));
    let mut buf = Vec::new();
    let header = (6u32 << 28) | (5 + field.len() as u32 / 4);
    for value in [header, stream_id, CAM_EXECUTE | CAM_EXECUTION_ACK, 1, cif0] {
        buf.extend_from_slice(&value.to_be_bytes());
    }
    buf.extend_from_slice(field);
    buf
}
