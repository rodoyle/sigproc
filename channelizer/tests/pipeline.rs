//! End-to-end pipeline tests: fixture bytes in, narrowband output out.
//!
//! These exercise the whole service — strict VITA49 parse, gap tracking, mixer,
//! multi-stage decimation, re-framing and the measurement path — without a
//! socket, a cluster or a radio. The gate that runs the binaries over real UDP
//! asserts the same properties; these make a failure point at a line of code
//! rather than at "the pod did not produce a file".

use sigproc_channelizer::{
    ChannelizerService, CollectingSink, ServiceConfig, OUTPUT_PACKET_SAMPLES,
};
use sigproc_common::config::ChannelConfig;
use sigproc_common::dsp::envelope_tone_hz;
use sigproc_common::metrics::Metrics;
use sigproc_common::vectors::{vita49_stream, AmTone};
use sigproc_common::vita49::{build_packet, PacketCursor};
use std::sync::Arc;

const INPUT_RATE: f64 = 2_000_000.0;
const OUTPUT_RATE: f64 = 50_000.0;

fn service(offset_hz: f64) -> ChannelizerService<CollectingSink> {
    let channel = ChannelConfig {
        offset_hz,
        output_rate_hz: OUTPUT_RATE,
        passband_hz: None,
        stopband_db: 80.0,
        max_taps: 512,
    };
    let (plan, cfg) =
        ServiceConfig::from_channel(&channel, INPUT_RATE).expect("plan is affordable");
    ChannelizerService::new(
        plan,
        cfg,
        CollectingSink::default(),
        Arc::new(Metrics::new()),
    )
}

/// A deterministic AM/DSB fixture, framed the way `sigproc-fixture` frames it.
fn fixture(signal_offset_hz: f64, seconds: f64) -> Vec<u8> {
    let spec = AmTone {
        tone_hz: 1000.0,
        offset_hz: signal_offset_hz,
        sample_rate: INPUT_RATE,
        seconds,
        depth: 0.5,
        amplitude: 0.5,
    };
    vita49_stream(&spec.generate(), spec.sample_rate, 2048, 0.0).0
}

fn feed_all(svc: &mut ChannelizerService<CollectingSink>, bytes: &[u8]) -> usize {
    let mut cursor = PacketCursor::new(bytes);
    let mut fed = 0;
    while let Some(next) = cursor.next_slice() {
        match next {
            Ok(raw) => {
                svc.process_datagram(raw);
                fed += 1;
            }
            // Mirror the binary's replay loop exactly: an unframeable record is
            // counted and skipped, never fatal.
            Err(e) => svc.note_malformed(&e.to_string()),
        }
    }
    fed
}

#[test]
fn replay_of_a_fixture_delivers_the_expected_rate_tone_and_centred_channel() {
    let bytes = fixture(200_000.0, 0.25);
    let mut svc = service(200_000.0);
    let fed = feed_all(&mut svc, &bytes);
    assert_eq!(fed, 245, "0.25 s at 2 MS/s in 2048-sample packets");

    let (report, sink) = svc.finish_with_sink();

    assert!(report.ok, "clean replay must be ok: {report:?}");
    assert_eq!(report.malformed_packets, 0);
    assert_eq!(report.packets_dropped, 0);
    assert_eq!(report.seq_gaps, 0);
    assert_eq!(report.samples_in, 500_000);
    // Exact integer decimation: 500 000 / 40.
    assert_eq!(report.samples_out, 12_500);
    assert_eq!(report.decimation, 40);
    assert_eq!(sink.rate, OUTPUT_RATE);

    // The modulation survived channelization: the envelope still carries 1 kHz.
    let tone = envelope_tone_hz(&sink.samples(), OUTPUT_RATE, 50.0, 5_000.0).expect("tone found");
    assert!((tone - 1000.0).abs() < 1.0, "recovered {tone} Hz");

    // The carrier sits at the channel centre, so the offset metric is ~0 — not
    // ±1 kHz from a sideband.
    let offset = report.peak_offset_hz.expect("offset measured");
    assert!(
        offset.abs() < 50.0,
        "centred channel reported {offset} Hz of offset"
    );

    let power = report.output_power_dbfs.expect("power measured");
    // A 0.5-amplitude carrier is -6.02 dBFS; 50% AM adds a little sideband power.
    assert!(
        (-7.0..-4.0).contains(&power),
        "output power {power} dBFS outside the expected band"
    );
}

/// Contract gate: a missing packet must be counted, located on **both** timelines,
/// and must make the run fail — never silently splice the output.
#[test]
fn gap_marking() {
    let bytes = fixture(200_000.0, 0.05);

    // Split by packet, not by byte arithmetic: the final packet is partial, so a
    // fixed stride would mis-slice the tail.
    let mut cursor = PacketCursor::new(&bytes);
    let mut packets: Vec<&[u8]> = Vec::new();
    while let Some(next) = cursor.next_slice() {
        packets.push(next.expect("fixture parses"));
    }
    assert_eq!(packets.len(), 49, "0.05 s at 2048-sample packets");

    // Ablate the fourth packet.
    let mut ablated = Vec::new();
    for (i, packet) in packets.iter().enumerate() {
        if i == 3 {
            continue;
        }
        ablated.extend_from_slice(packet);
    }

    let mut svc = service(200_000.0);
    let fed = feed_all(&mut svc, &ablated);
    let (report, _sink) = svc.finish_with_sink();

    assert_eq!(fed, 48, "49 packets minus the ablated one");
    assert_eq!(report.seq_gaps, 1, "the discontinuity must be counted");
    assert_eq!(report.gap_samples, 2048, "and measured, not just flagged");
    assert!(!report.ok, "a discontinuity must fail the report");

    assert_eq!(report.discontinuities.len(), 1);
    let d = &report.discontinuities[0];
    // Input timeline: three packets (3 x 2048 = 6144 samples) were received, so
    // the stream resumes at input sample 8192 and 2048 samples went missing.
    assert_eq!(d.at_input_sample, 2048 * 4);
    assert_eq!(d.missing_samples, 2048);
    // Output timeline: 6144 input samples / 40 produced 153 narrowband samples,
    // then the timeline skips ceil(2048 / 40) = 52 so downstream cannot mistake
    // the result for continuous audio.
    assert_eq!(d.at_output_sample, 153 + 52);
    assert!(d.at_output_sample < report.samples_out);
}

#[test]
fn malformed_input_is_counted_not_fatal_on_the_udp_path() {
    // Datagrams are independently framed, so one corrupt datagram costs exactly
    // one packet: the service must keep running.
    let first = build_packet(&vec![100i16; 2048 * 2], 0.0, INPUT_RATE);
    let second = build_packet(&vec![100i16; 2048 * 2], 2048.0 / INPUT_RATE, INPUT_RATE);
    let garbage = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x00];

    let mut svc = service(200_000.0);
    assert!(!svc.process_datagram(&first).malformed);
    assert!(
        svc.process_datagram(&garbage).malformed,
        "garbage must be reported as malformed"
    );
    assert!(!svc.process_datagram(&second).malformed);

    let (report, sink) = svc.finish_with_sink();
    assert_eq!(report.malformed_packets, 1, "the bad datagram is counted");
    assert_eq!(
        report.packets_received, 2,
        "both good datagrams were processed"
    );
    assert!(!report.ok, "a malformed packet must fail the report");
    assert!(
        !sink.packets.is_empty(),
        "the service kept running and produced output"
    );
}

/// The file path cannot resynchronise, and saying so is the point: a corrupt
/// record ends the walk rather than having the remainder guessed at.
#[test]
fn a_framing_error_in_a_file_is_counted_and_ends_the_walk() {
    let first = build_packet(&vec![100i16; 2048 * 2], 0.0, INPUT_RATE);
    let second = build_packet(&vec![100i16; 2048 * 2], 2048.0 / INPUT_RATE, INPUT_RATE);

    let mut stream = first;
    stream.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00]);
    stream.extend_from_slice(&second);

    let mut svc = service(200_000.0);
    let fed = feed_all(&mut svc, &stream);
    let (report, _sink) = svc.finish_with_sink();

    assert_eq!(fed, 1, "the walk stops at the corrupt record");
    assert_eq!(report.malformed_packets, 1, "and it is counted");
    assert!(
        !report.ok,
        "a file the reader could not finish must not report ok"
    );
}

#[test]
fn output_packets_are_bounded_and_timestamps_are_contiguous() {
    let bytes = fixture(200_000.0, 0.1);
    let mut svc = service(200_000.0);
    feed_all(&mut svc, &bytes);
    let (report, sink) = svc.finish_with_sink();
    assert!(report.ok, "{report:?}");

    assert!(
        sink.packets.len() > 2,
        "expected several output packets, got {}",
        sink.packets.len()
    );
    for (i, (sc16, _)) in sink.packets.iter().enumerate() {
        let iq = sc16.len() / 2;
        if i + 1 == sink.packets.len() {
            assert!(
                iq <= OUTPUT_PACKET_SAMPLES,
                "final packet {i} has {iq} samples"
            );
        } else {
            assert_eq!(iq, OUTPUT_PACKET_SAMPLES, "packet {i} must be full");
        }
    }

    // Timestamps advance by exactly one packet's worth of time: no drift, and no
    // dependence on packet arrival timing.
    let step = OUTPUT_PACKET_SAMPLES as f64 / OUTPUT_RATE;
    let stamps = sink.timestamps();
    for (i, pair) in stamps.windows(2).enumerate() {
        let delta = pair[1] - pair[0];
        assert!(
            (delta - step).abs() < 1e-9,
            "packet {} -> {}: step {delta}, expected {step}",
            i,
            i + 1
        );
    }
    assert!(stamps[0].abs() < 1e-9, "the stream starts at zero");
}

/// The settling rule, at the level a metric consumer sees it: before a full
/// measurement block has arrived there is no honest number to publish, so none is
/// published (the gauges stay NaN and the report carries null).
#[test]
fn level_and_offset_are_not_published_before_settling() {
    let bytes = fixture(200_000.0, 0.01); // 20 000 input samples -> 500 output
    let mut svc = service(200_000.0);
    feed_all(&mut svc, &bytes);
    let (report, _sink) = svc.finish_with_sink();

    assert_eq!(report.samples_out, 500);
    assert!(
        report.samples_out < 4096,
        "precondition: less than one measurement block"
    );
    assert_eq!(report.output_power_dbfs, None, "no level published yet");
    assert_eq!(report.peak_offset_hz, None, "no offset published yet");
}

/// Proves the offset metric measures something: putting the channel 800 Hz away
/// from the signal must show up as an 800 Hz offset, with the sign that says which
/// side it landed on.
#[test]
fn an_off_centre_channel_reports_a_non_zero_offset() {
    let bytes = fixture(200_000.0, 0.25); // signal at +200 000
    let mut svc = service(200_800.0); // channel 800 Hz high
    feed_all(&mut svc, &bytes);
    let (report, _sink) = svc.finish_with_sink();

    let offset = report.peak_offset_hz.expect("offset measured");
    assert!(
        (offset + 800.0).abs() < 30.0,
        "expected about -800 Hz (signal below the channel centre), got {offset}"
    );
}
