use sigproc_common::dsp::{estimate_frequency, sc16_to_iq, Iq};
use sigproc_common::vectors::{vita49_stream, NbfmTone};
use sigproc_common::vita49::PacketCursor;
use sigproc_demod::{Demod, NbfmDemod, NATIVE_SAMPLE_RATE_HZ};

fn nbfm_tone(tone_hz: f64, offset_hz: f64) -> Vec<Iq> {
    NbfmTone {
        tone_hz,
        offset_hz,
        sample_rate: f64::from(NATIVE_SAMPLE_RATE_HZ),
        seconds: 0.5,
        deviation_hz: 2_000.0,
        amplitude: 0.4,
    }
    .generate()
}

fn rms(audio: &[f32]) -> f64 {
    (audio.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / audio.len() as f64).sqrt()
}

#[test]
fn nbfm_demod_recovers_fixture_tones_from_vita49_within_five_hz() {
    for (tone_hz, offset_hz) in [(400.0, 0.0), (1_000.0, -1_000.0), (3_000.0, 750.0)] {
        let input = nbfm_tone(tone_hz, offset_hz);
        let (bytes, _) = vita49_stream(&input, f64::from(NATIVE_SAMPLE_RATE_HZ), 360, 0.0);
        let mut cursor = PacketCursor::new(&bytes);
        let mut nbfm = NbfmDemod::new();
        let demod: &mut dyn Demod = &mut nbfm;
        let mut audio = Vec::new();
        while let Some(packet) = cursor.next_packet() {
            let iq = sc16_to_iq(&packet.expect("fixture packet parses").iq_i16());
            audio.extend(demod.process(&iq));
        }
        assert_eq!(audio.len(), input.len());
        assert!(audio.iter().all(|x| x.is_finite()));
        let settled = &audio[5_000..];
        assert!(rms(settled) > 0.2, "recovered audio must have useful energy");
        let mean = settled.iter().map(|&x| f64::from(x)).sum::<f64>() / settled.len() as f64;
        assert!(mean.abs() < 0.01, "carrier offset leaked into audio: {mean}");
        let real: Vec<Iq> = settled.iter().map(|&x| Iq::new(x, 0.0)).collect();
        let got = estimate_frequency(&real, 50_000.0, 100.0, 5_000.0).expect("audio tone found");
        assert!(
            (got - tone_hz).abs() <= 5.0,
            "expected {tone_hz} Hz, got {got} Hz"
        );
    }
}

#[test]
fn nbfm_demod_is_exactly_invariant_to_packet_boundaries_and_empty_blocks() {
    let input = nbfm_tone(1_000.0, 500.0);
    let expected = NbfmDemod::new().process(&input);
    for block_size in [1, 7, 360, 1_024, 4_093] {
        let mut demod = NbfmDemod::new();
        let mut actual = Vec::new();
        for block in input.chunks(block_size) {
            assert!(demod.process(&[]).is_empty());
            actual.extend(demod.process(block));
        }
        assert_eq!(actual, expected, "block size {block_size}");
    }
}

#[test]
fn nbfm_demod_rejects_bad_deviation_and_reset_restores_fresh_response() {
    for deviation in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(NbfmDemod::with_deviation(deviation).is_err());
    }
    let input = nbfm_tone(1_000.0, 0.0);
    let expected = NbfmDemod::new().process(&input);
    let mut demod = NbfmDemod::new();
    demod.process(&input[..3_001]);
    assert_ne!(demod.process(&input), expected);
    demod.reset();
    demod.reset();
    assert_eq!(demod.process(&input), expected);
}

#[test]
fn nbfm_demod_silence_is_finite_and_constant_frequency_offset_is_blocked() {
    let mut demod = NbfmDemod::new();
    let silence = vec![Iq::ZERO; 25_000];
    assert_eq!(demod.process(&silence), vec![0.0; silence.len()]);
    let carrier: Vec<Iq> = (0..25_000)
        .map(|n| {
            let phase = std::f64::consts::TAU * 1_000.0 * n as f64 / 50_000.0;
            Iq::new((0.4 * phase.cos()) as f32, (0.4 * phase.sin()) as f32)
        })
        .collect();
    let audio = demod.process(&carrier);
    assert!(audio.iter().all(|x| x.is_finite()));
    assert!(rms(&audio[20_000..]) < 1e-7);
}

#[test]
fn default_service_records_nbfm_wav_and_sidecar_with_expected_tone() {
    use sigproc_common::dsp::iq_to_sc16;
    use sigproc_common::vita49::{build_context_packet, build_data_packet, StreamContext};
    use sigproc_demod::{recording::Recorder, service::DemodService, telemetry::Telemetry};
    use std::sync::Arc;

    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target/demod-tests")
        .join(format!("nbfm-wav-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let recorder = Recorder::new(directory.clone(), 50_000, 2).unwrap();
    let metrics = Arc::new(Telemetry::default());
    let mut service = DemodService::new(recorder, metrics);
    service
        .process_datagram(&build_context_packet(
            &StreamContext {
                bandwidth_hz: 12_500.0,
                rf_freq_hz: 915_200_000.0,
                gain_db: 30.0,
                sample_rate_hz: 50_000.0,
            },
            0.0,
            7,
            0,
            true,
        ))
        .unwrap();
    let input = nbfm_tone(1_000.0, 0.0);
    for (sequence, chunk) in input.chunks(360).enumerate() {
        service
            .process_datagram(&build_data_packet(
                &iq_to_sc16(chunk),
                (sequence * 360) as f64 / 50_000.0,
                50_000.0,
                7,
                sequence as u8,
            ))
            .unwrap();
    }
    service.finish("test").unwrap();

    let sidecar = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "json"))
        .expect("completed sidecar");
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(sidecar).unwrap()).unwrap();
    assert_eq!(metadata["mode"], "nbfm");
    assert_eq!(metadata["samples"], input.len() as u64);
    assert_eq!(metadata["end_reason"], "test");
    let wav = std::fs::read(directory.join(metadata["wav_file"].as_str().unwrap())).unwrap();
    let audio: Vec<Iq> = wav[44..]
        .chunks_exact(2)
        .skip(5_000)
        .map(|bytes| Iq::new(i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32767.0, 0.0))
        .collect();
    let measured = estimate_frequency(&audio, 50_000.0, 100.0, 5_000.0).unwrap();
    assert!((measured - 1_000.0).abs() <= 5.0, "measured {measured} Hz");
    std::fs::remove_dir_all(directory).unwrap();
}
