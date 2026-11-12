use serde_json::Value;
use sigproc_common::{
    dsp::{sc16_to_iq, Iq},
    metrics::{spawn_http, MetricsRenderer},
    vita49::{build_context_packet, build_data_packet, StreamContext},
};
use sigproc_demod::{
    recording::Recorder,
    service::{run_udp, DemodService},
    telemetry::Telemetry,
    AmDemod, Demod,
};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::{Duration, Instant},
};

static ID: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/demod-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                ID.fetch_add(1, Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn files(&self, suffix: &str) -> Vec<PathBuf> {
        fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().ends_with(suffix))
            .collect()
    }
    fn records(&self) -> Vec<Value> {
        let mut records: Vec<Value> = self
            .files(".json")
            .into_iter()
            .filter(|p| p.is_file())
            .map(|p| serde_json::from_slice(&fs::read(p).unwrap()).unwrap())
            .filter(|m: &Value| m["owner"] == "sigproc-demod-v1")
            .collect();
        records.sort_by_key(|m| m["first_output_sample"].as_u64().unwrap());
        records
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn context(rate: f64, rf: f64) -> Vec<u8> {
    build_context_packet(
        &StreamContext {
            sample_rate_hz: rate,
            rf_freq_hz: rf,
            bandwidth_hz: 20_000.0,
            gain_db: 12.0,
        },
        0.0,
        7,
        0,
        true,
    )
}
fn data(counter: u64, samples: usize) -> Vec<u8> {
    build_data_packet(
        &[8192, 4096].repeat(samples),
        counter as f64 / 50_000.0,
        50_000.0,
        7,
        ((counter / samples as u64) % 16) as u8,
    )
}
fn service(dir: &Directory, limit: u64, keep: usize) -> (DemodService, Arc<Telemetry>) {
    let metrics = Arc::new(Telemetry::default());
    (
        DemodService::with_demod(
            Recorder::new(dir.0.clone(), limit, keep).unwrap(),
            metrics.clone(),
            Box::new(AmDemod::new()),
            "am",
        ),
        metrics,
    )
}
fn pcm(dir: &Directory, record: &Value) -> Vec<i16> {
    let bytes = fs::read(dir.0.join(record["wav_file"].as_str().unwrap())).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..16], b"WAVEfmt ");
    assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        50_000
    );
    assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
    assert_eq!(
        u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize,
        bytes.len() - 44
    );
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
        bytes.len() - 8
    );
    bytes[44..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}
#[test]
fn service_context_is_required_and_invalid_context_revokes_stale_acceptance() {
    let dir = Directory::new();
    let (mut s, m) = service(&dir, 1000, 100);
    s.process_datagram(&data(0, 10)).unwrap();
    assert_eq!(m.samples.load(Relaxed), 0);
    let valid = context(50_000.0, 915_000_000.0);
    let mut bad = vec![context(48_000.0, 915_000_000.0)];
    for offset in [0, 20, 24, 52, 56] {
        let mut packet = valid.clone();
        packet[offset] ^= 1;
        bad.push(packet);
    }
    for length in [1, 4, 19, 20, 59] {
        bad.push(valid[..length].to_vec());
    }
    let mut extra = valid.clone();
    extra.push(0);
    bad.push(extra);
    for packet in bad {
        s.process_datagram(&valid).unwrap();
        s.process_datagram(&packet).unwrap();
        assert_eq!(m.context_valid.load(Relaxed), 0);
        s.process_datagram(&data(0, 10)).unwrap();
        assert_eq!(m.samples.load(Relaxed), 0);
    }
    s.process_datagram(&valid).unwrap();
    s.process_datagram(&data(0, 10)).unwrap();
    s.finish("test").unwrap();
    assert_eq!(m.samples.load(Relaxed), 10);
    assert_eq!(dir.records().len(), 1);
}
#[test]
fn service_rejects_bad_data_headers_lengths_and_foreign_stream_without_panicking() {
    let dir = Directory::new();
    let (mut s, m) = service(&dir, 1000, 100);
    s.process_datagram(&context(50_000.0, 1.0)).unwrap();
    let valid = data(0, 10);
    for length in 0..valid.len() {
        s.process_datagram(&valid[..length]).unwrap();
    }
    for (offset, mask) in [(0, 8), (1, 0x10), (1, 0x40), (7, 1), (19, 255)] {
        let mut packet = valid.clone();
        packet[offset] ^= mask;
        if offset == 19 {
            packet[12..20].copy_from_slice(&50_000u64.to_be_bytes());
        }
        s.process_datagram(&packet).unwrap();
    }
    assert_eq!(m.samples.load(Relaxed), 0);
    s.process_datagram(&valid).unwrap();
    s.finish("test").unwrap();
    assert_eq!(m.samples.load(Relaxed), 10);
    assert!(m.rejected.load(Relaxed) > 0);
}
#[test]
fn service_gap_and_backward_timestamps_rotate_with_exact_indices_and_reset_dsp() {
    let dir = Directory::new();
    let (mut s, m) = service(&dir, 1000, 100);
    s.process_datagram(&context(50_000.0, 915_000_000.0))
        .unwrap();
    for counter in [0, 10, 50, 0] {
        s.process_datagram(&data(counter, 10)).unwrap();
    }
    s.finish("test").unwrap();
    s.finish("test_again").unwrap();
    assert!(s.process_datagram(&data(10, 10)).is_err());
    let records = dir.records();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["samples"], 20);
    assert_eq!(records[0]["end_reason"], "forward_gap");
    let boundary = &records[1]["boundary"];
    assert_eq!(boundary["kind"], "forward_gap");
    assert_eq!(boundary["input_sample"], 50);
    assert_eq!(boundary["missing_samples"], 30);
    assert_eq!(boundary["output_sample"], 20);
    assert_eq!(boundary["recording_sample"], 0);
    assert_eq!(records[2]["boundary"]["kind"], "backward_timestamp");
    assert_eq!(records[2]["first_output_sample"], 30);
    assert_eq!(pcm(&dir, &records[1]), pcm(&dir, &records[2]));
    let expected: Vec<i16> = AmDemod::new()
        .process(&sc16_to_iq(&[8192, 4096].repeat(10)))
        .into_iter()
        .map(|x| (x * 32767.0).round() as i16)
        .collect();
    assert_eq!(pcm(&dir, &records[1]), expected);
    assert_eq!(
        (
            m.gaps.load(Relaxed),
            m.gap_samples.load(Relaxed),
            m.resets.load(Relaxed)
        ),
        (1, 30, 1)
    );
    assert!(dir.files(".part").is_empty());
}
#[test]
fn service_timestamp_gap_survives_four_bit_sequence_wrap() {
    let dir = Directory::new();
    let (mut s, m) = service(&dir, 1000, 100);
    s.process_datagram(&context(50_000.0, 1.0)).unwrap();
    // Sixteen lost datagrams make the 4-bit sequence look contiguous.
    s.process_datagram(&data(0, 10)).unwrap();
    s.process_datagram(&data(170, 10)).unwrap();
    s.finish("test").unwrap();
    assert_eq!(m.gap_samples.load(Relaxed), 160);
    assert_eq!(dir.records()[1]["boundary"]["missing_samples"], 160);
}
#[test]
fn service_context_change_and_reject_end_recordings_and_restart_history() {
    let dir = Directory::new();
    let (mut s, _) = service(&dir, 1000, 100);
    s.process_datagram(&context(50_000.0, 1.0)).unwrap();
    s.process_datagram(&data(0, 10)).unwrap();
    s.process_datagram(&context(50_000.0, 1.0)).unwrap(); // periodic repeat must not rotate
    s.process_datagram(&data(10, 10)).unwrap();
    s.process_datagram(&context(50_000.0, 2.0)).unwrap();
    s.process_datagram(&data(0, 10)).unwrap();
    s.process_datagram(b"bad").unwrap();
    s.process_datagram(&data(10, 10)).unwrap();
    s.finish("test").unwrap();
    let records = dir.records();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["samples"], 20);
    assert_eq!(records[0]["rf_freq_hz"], 1.0);
    assert_eq!(records[1]["rf_freq_hz"], 2.0);
    assert_eq!(records[1]["boundary"]["kind"], "context_change");
    assert_eq!(records[2]["boundary"]["kind"], "rejected_datagram");
    assert_eq!(pcm(&dir, &records[1]), pcm(&dir, &records[2]));
}
#[test]
fn recording_rotates_at_exact_samples_preserves_dsp_and_retains_only_owned_pairs() {
    let dir = Directory::new();
    let (mut s, _) = service(&dir, 7, 100);
    s.process_datagram(&context(50_000.0, 1.0)).unwrap();
    s.process_datagram(&data(0, 20)).unwrap();
    s.finish("test").unwrap();
    let records = dir.records();
    assert_eq!(
        records
            .iter()
            .map(|r| r["samples"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [7, 7, 6]
    );
    let recorded: Vec<i16> = records.iter().flat_map(|r| pcm(&dir, r)).collect();
    let expected: Vec<i16> = AmDemod::new()
        .process(&[Iq::new(0.25, 0.125); 20])
        .iter()
        .map(|x| (x * 32767.0).round() as i16)
        .collect();
    assert_eq!(recorded, expected);
    fs::write(dir.0.join("unrelated.wav"), b"keep").unwrap();
    fs::write(dir.0.join("sigproc-demod-1-2-3.json"), b"{}").unwrap();
    fs::write(dir.0.join("sigproc-demod-1-2-3.wav"), b"keep").unwrap();
    fs::write(dir.0.join("sigproc-demod-1-2-4.wav.part"), b"keep").unwrap();
    let mut recorder = Recorder::new(dir.0.clone(), 3, 1).unwrap(); // restart retention
    recorder.write(&[2.0; 4], 7, 1.0, 100, 0, None).unwrap();
    recorder.finish("test").unwrap();
    let retained = dir.records();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0]["clipped_samples"], 1);
    assert_eq!(fs::read(dir.0.join("unrelated.wav")).unwrap(), b"keep");
    assert!(dir.0.join("sigproc-demod-1-2-3.wav").exists());
    assert!(dir.0.join("sigproc-demod-1-2-4.wav.part").exists());
}
#[test]
fn recording_invalid_limits_open_failure_and_failed_finalize_never_publish_completion() {
    let dir = Directory::new();
    for limit in [0, u32::MAX as u64] {
        assert!(Recorder::new(dir.0.clone(), limit, 1).is_err());
    }
    assert!(Recorder::new(dir.0.clone(), 1, 0).is_err());
    let mut recorder = Recorder::new(dir.0.clone(), 100, 1).unwrap();
    recorder.write(&[0.2; 10], 7, 1.0, 0, 0, None).unwrap();
    assert!(dir.files(".json").is_empty());
    let part = dir.files(".wav.part").pop().unwrap();
    let stem = part
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_suffix(".wav.part")
        .unwrap();
    let blocker = dir.0.join(format!("{stem}.json"));
    fs::create_dir(&blocker).unwrap();
    assert!(recorder.finish("test").is_err());
    drop(recorder);
    assert!(dir.records().is_empty());
    assert_eq!(dir.files(".json.part").len(), 1);
    fs::remove_dir(blocker).unwrap();
    let sub = Directory::new();
    let (mut s, metrics) = service(&sub, 100, 1);
    fs::remove_dir(&sub.0).unwrap();
    fs::write(&sub.0, b"not a directory").unwrap();
    s.process_datagram(&context(50_000.0, 1.0)).unwrap();
    assert!(s.process_datagram(&data(0, 10)).is_err());
    assert!(s.process_datagram(&data(10, 10)).is_err());
    assert_eq!(metrics.recording_errors.load(Relaxed), 1);
    fs::remove_file(&sub.0).unwrap();
}
#[test]
fn recording_drop_finalizes_but_poisoned_write_does_not() {
    let dir = Directory::new();
    {
        let mut r = Recorder::new(dir.0.clone(), 100, 10).unwrap();
        r.write(&[0.1; 5], 7, 1.0, 0, 0, None).unwrap();
    }
    assert_eq!(dir.records()[0]["end_reason"], "drop");
    {
        let mut r = Recorder::new(dir.0.clone(), 100, 10).unwrap();
        r.write(&[0.1; 5], 7, 1.0, 0, 0, None).unwrap();
        assert!(r.write(&[f32::NAN], 7, 1.0, 5, 5, None).is_err());
        assert!(r.finish("test").is_err());
    }
    assert_eq!(dir.records().len(), 1);
    assert_eq!(dir.files(".wav.part").len(), 1);
}
#[test]
fn service_metrics_measure_native_iq_and_never_invent_snr() {
    let dir = Directory::new();
    let (mut s, metrics) = service(&dir, 50_000, 10);
    assert!(metrics
        .render_metrics("demod")
        .contains("demod_rssi_dbfs NaN"));
    s.process_datagram(&context(50_000.0, 1.0)).unwrap();
    let iq: Vec<i16> = (0..4096)
        .flat_map(|n| {
            let phase = std::f64::consts::TAU * -1234.0 * n as f64 / 50_000.0;
            [(phase.cos() * 8192.0) as i16, (phase.sin() * 8192.0) as i16]
        })
        .collect();
    s.process_datagram(&build_data_packet(&iq, 0.0, 50_000.0, 7, 0))
        .unwrap();
    let text = metrics.render_metrics("demod");
    let gauge = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap()
            .parse::<f64>()
            .unwrap()
    };
    assert!((gauge("demod_rssi_dbfs ") + 12.041).abs() < 0.02);
    assert!((gauge("demod_frequency_offset_estimate_hz ") + 1234.0).abs() < 3.0);
    assert!(text.contains("demod_snr_db NaN"));
    assert!(text.contains("demod_snr_available 0"));
    s.process_datagram(&data(5000, 10)).unwrap();
    assert!(metrics
        .render_metrics("demod")
        .contains("demod_rssi_dbfs NaN"));
}
#[test]
fn service_udp_shutdown_finalizes_and_shared_http_serves_operational_metrics() {
    let dir = Directory::new();
    let (mut s, metrics) = service(&dir, 1000, 10);
    let receive = UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = receive.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    let worker = std::thread::spawn(move || run_udp(receive, &mut s, &signal));
    let send = UdpSocket::bind("127.0.0.1:0").unwrap();
    send.send_to(&context(50_000.0, 1.0), address).unwrap();
    send.send_to(&data(0, 10), address).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while metrics.samples.load(Relaxed) < 10 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    stop.store(true, Relaxed);
    worker.join().unwrap().unwrap();
    assert_eq!(metrics.samples.load(Relaxed), 10);
    assert_eq!(dir.records()[0]["end_reason"], "shutdown");
    let reserve = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = reserve.local_addr().unwrap();
    drop(reserve);
    let _http = spawn_http(addr, "demod".into(), metrics).unwrap();
    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: test\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("demod_samples_out_total 10"));
    assert!(response.contains("demod_snr_available 0"));
}
