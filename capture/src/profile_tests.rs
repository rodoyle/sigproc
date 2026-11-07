//! Protocol vectors transcribed as words independently of the encoder.
use sigproc_common::vita49::{self, ControlBackend, ControlUpdate, Controllee, StreamContext};
fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_be_bytes()).collect()
}
fn context() -> StreamContext {
    StreamContext {
        rf_freq_hz: 100e6,
        sample_rate_hz: 1e6,
        bandwidth_hz: 1e6,
        gain_db: 10.0,
    }
}
#[test]
fn independent_context_and_data_words_pin_the_wire_profile() {
    // Type 4, TSI other, TSF samples, seq7, 15 total words. Q20 Hz,
    // gain stage1 Q7, complex cartesian signed 16-bit payload format.
    let fixture = bytes(&[
        0x40d7000f, 17, 1, 0, 2, 0xa8a08000, 0x000000f4, 0x24000000, 0x00005f5e, 0x10000000,
        0x00000500, 0x000000f4, 0x24000000, 0x200003cf, 0,
    ]);
    assert_eq!(
        vita49::build_context_packet(&context(), 1.000002, 17, 7, true),
        fixture
    );
    let data = bytes(&[0x10df0006, 17, 1, 0, 2, 0x0001ffff]);
    let parsed = vita49::parse(&data).unwrap();
    assert_eq!(
        (
            parsed.stream_id,
            parsed.packet_count,
            parsed.ts_int,
            parsed.ts_frac
        ),
        (17, 15, 1, 2)
    );
    assert_eq!(parsed.iq_i16(), [1, -1]);
    assert_eq!(
        vita49::build_data_packet(&[1, -1], 1.000002, 1e6, 17, 31),
        data
    );
    // Off-by-one word count must not be silently accepted.
    let mut bad = data;
    bad[3] = 5;
    assert!(vita49::parse(&bad).is_err());
}
struct Backend(StreamContext, usize);
impl ControlBackend for Backend {
    fn apply_and_readback(&mut self, update: ControlUpdate) -> anyhow::Result<StreamContext> {
        self.1 += 1;
        if let Some(hz) = update.rf_freq_hz {
            self.0.rf_freq_hz = hz;
        }
        Ok(self.0)
    }
}
#[test]
fn external_layout_with_ids_and_message_id_is_not_our_builder_roundtrip() {
    // Layout checked against SoapyVITA49 Control.cpp at 6f209e47812424fb2038a83e30ba5613f68c2725:
    // no timestamps, execute+execution ack, 32-bit controllee, message 0x1234.
    let fixture = bytes(&[
        0x60000008, 17, 0x81080000, 0x1234, 17, 0x08000000, 0x00005f5e, 0x10000000,
    ]);
    let mut ctl = Controllee::new(context(), 17);
    let mut backend = Backend(context(), 0);
    let ack = ctl.handle(&fixture, &mut backend).unwrap();
    assert_eq!(backend.1, 1);
    assert_eq!(ctl.applied.rf_freq_hz, 100e6);
    // Independent decode: header ACK flag, CAM IDs, echoed Message ID and ID.
    let words: Vec<u32> = ack
        .chunks_exact(4)
        .map(|w| u32::from_be_bytes(w.try_into().unwrap()))
        .collect();
    assert_ne!(words[0] & (1 << 26), 0);
    assert_eq!(words[0] & 0xffff, words.len() as u32);
    assert_eq!(words[5], 0x80080400);
    assert_eq!((words[6], words[7]), (0x1234, 17));
    // Reject a fifth field before invoking the backend, even mixed with RF.
    let unsupported = bytes(&[0x60000008, 17, 0x81080000, 0x1235, 17, 0x0c000000, 0, 0]);
    let ack = ctl.handle(&unsupported, &mut backend).unwrap();
    assert_eq!(backend.1, 1);
    assert_eq!(u32::from_be_bytes(ack[24..28].try_into().unwrap()), 0x1235);
    assert_ne!(
        u32::from_be_bytes(ack[20..24].try_into().unwrap()) & (1 << 16),
        0
    );
}
