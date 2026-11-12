use sigproc_common::dsp::{estimate_frequency, sc16_to_iq, Iq};
use sigproc_common::vectors::{vita49_stream, AmTone};
use sigproc_common::vita49::PacketCursor;
use sigproc_demod::{AmDemod, Demod, NATIVE_SAMPLE_RATE_HZ};

fn am_tone(tone_hz: f64, offset_hz: f64) -> Vec<Iq> {
    AmTone {
        tone_hz,
        offset_hz,
        sample_rate: f64::from(NATIVE_SAMPLE_RATE_HZ),
        seconds: 0.5,
        depth: 0.6,
        amplitude: 0.4,
    }
    .generate()
}

fn rms(audio: &[f32]) -> f64 {
    (audio.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / audio.len() as f64).sqrt()
}

#[test]
fn am_demod_recovers_fixture_tones_from_vita49_within_five_hz() {
    for (tone_hz, offset_hz) in [(400.0, 0.0), (1_000.0, -4_000.0), (3_700.0, 7_500.0)] {
        let input = am_tone(tone_hz, offset_hz);
        let (bytes, _) = vita49_stream(&input, f64::from(NATIVE_SAMPLE_RATE_HZ), 360, 0.0);
        let mut cursor = PacketCursor::new(&bytes);
        let mut am = AmDemod::new();
        // Exercise the public mode-independent extension seam.
        let demod: &mut dyn Demod = &mut am;
        assert_eq!(demod.sample_rate_hz(), 50_000);
        let mut audio = Vec::new();
        while let Some(packet) = cursor.next_packet() {
            let iq = sc16_to_iq(&packet.expect("fixture packet parses").iq_i16());
            audio.extend(demod.process(&iq));
        }
        assert_eq!(audio.len(), input.len());
        assert!(audio.iter().all(|x| x.is_finite()));
        let settled = &audio[5_000..];
        assert!(
            rms(settled) > 0.1,
            "recovered audio must have useful energy"
        );
        let mean = settled.iter().map(|&x| f64::from(x)).sum::<f64>() / settled.len() as f64;
        assert!(mean.abs() < 0.001, "carrier DC leaked into audio: {mean}");
        let real: Vec<Iq> = settled.iter().map(|&x| Iq::new(x, 0.0)).collect();
        let got = estimate_frequency(&real, 50_000.0, 100.0, 5_000.0).expect("audio tone found");
        assert!(
            (got - tone_hz).abs() <= 5.0,
            "expected {tone_hz} Hz, got {got} Hz"
        );
    }
}

#[test]
fn am_demod_is_exactly_invariant_to_block_boundaries_and_empty_blocks() {
    let mut input = am_tone(1_237.0, -3_000.0);
    input.extend(vec![Iq::ZERO; 1_001]);
    input.extend(am_tone(837.0, 2_500.0));
    let expected = AmDemod::new().process(&input);
    for block_size in [1, 7, 360, 1_024, 4_093] {
        let mut demod = AmDemod::new();
        let mut actual = Vec::new();
        for block in input.chunks(block_size) {
            assert!(demod.process(&[]).is_empty());
            actual.extend(demod.process(block));
        }
        assert_eq!(actual, expected, "block size {block_size}");
    }
}

#[test]
fn am_demod_silence_is_finite_and_warm_state_settles_back_to_zero() {
    let mut demod = AmDemod::new();
    let silence = vec![Iq::ZERO; 25_000];
    assert_eq!(demod.process(&silence), vec![0.0; silence.len()]);
    demod.process(&am_tone(1_000.0, 3_000.0));
    let tail = demod.process(&silence);
    assert!(tail.iter().all(|x| x.is_finite()));
    assert!(rms(&tail[20_000..]) < 1e-8);
}

#[test]
fn am_demod_removes_a_constant_carrier_and_conditions_high_frequency_audio() {
    let carrier = vec![Iq::new(0.4, 0.3); 25_000];
    let audio = AmDemod::new().process(&carrier);
    assert!(
        audio[0] > 0.0,
        "initial carrier step should exercise filter state"
    );
    assert!(
        rms(&audio[20_000..]) < 1e-8,
        "DC blocker must reject the carrier"
    );
    let low = AmDemod::new().process(&am_tone(1_000.0, 0.0));
    let high = AmDemod::new().process(&am_tone(12_000.0, 0.0));
    assert!(rms(&high[5_000..]) < 0.6 * rms(&low[5_000..]));
}

#[test]
fn am_demod_reset_restores_fresh_instance_response_and_is_idempotent() {
    let input = am_tone(1_137.0, 2_300.0);
    let expected = AmDemod::default().process(&input);
    let mut demod = AmDemod::new();
    demod.process(&input[..3_001]);
    assert_ne!(
        demod.process(&input),
        expected,
        "warm history must affect the response"
    );
    demod.reset();
    demod.reset();
    assert!(demod.process(&[]).is_empty());
    assert_eq!(demod.process(&input), expected);
    demod.reset();
    assert_eq!(demod.process(&[Iq::ZERO; 100]), vec![0.0; 100]);
    assert_eq!(demod.process(&input), expected);
}
