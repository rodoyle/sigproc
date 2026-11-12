use super::*;
use sigproc_common::vita49::{self, ControlUpdate, StreamContext};
use std::time::{Duration, Instant};

#[derive(Default)]
struct MockBackend {
    calls: Vec<ControlUpdate>,
    fail: bool,
    state: Option<StreamContext>,
}

impl ControlBackend for MockBackend {
    fn apply_and_readback(&mut self, update: ControlUpdate) -> anyhow::Result<StreamContext> {
        self.calls.push(update);
        anyhow::ensure!(!self.fail, "mock hardware failed");
        let mut state = self.state.unwrap();
        if let Some(hz) = update.rf_freq_hz {
            state.rf_freq_hz = hz + 10.0;
        }
        if let Some(hz) = update.sample_rate_hz {
            state.sample_rate_hz = hz;
        }
        if let Some(hz) = update.bandwidth_hz {
            state.bandwidth_hz = hz;
        }
        if let Some(db) = update.gain_db {
            state.gain_db = db;
        }
        self.state = Some(state);
        Ok(state)
    }
}

fn initial() -> StreamContext {
    StreamContext {
        rf_freq_hz: 100e6,
        sample_rate_hz: 1e6,
        bandwidth_hz: 1e6,
        gain_db: 10.0,
    }
}

fn harness() -> (ControlSocket, UdpSocket, Controllee, MockBackend) {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let endpoint = ControlSocket {
        socket,
        peer: peer.local_addr().unwrap(),
    };
    let controllee = Controllee::new(initial(), 17);
    let backend = MockBackend {
        state: Some(initial()),
        ..Default::default()
    };
    (endpoint, peer, controllee, backend)
}

fn poll(endpoint: &ControlSocket) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(datagram) = endpoint.poll().unwrap() {
            return datagram;
        }
        assert!(Instant::now() < deadline, "control packet not received");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn receive(peer: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 256];
    let len = peer.recv(&mut buf).unwrap();
    buf[..len].to_vec()
}

fn q20(hz: f64) -> [u8; 8] {
    ((hz * (1u64 << 20) as f64) as i64).to_be_bytes()
}
fn word(packet: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(packet[offset..offset + 4].try_into().unwrap())
}
fn hz(packet: &[u8], offset: usize) -> f64 {
    i64::from_be_bytes(packet[offset..offset + 8].try_into().unwrap()) as f64 / (1u64 << 20) as f64
}

#[test]
fn udp_four_settings_acknowledge_backend_readback_and_unsupported_does_not_mutate() {
    let (endpoint, peer, mut ctl, mut backend) = harness();
    let requests = [
        (1 << 27, q20(915e6).to_vec()),
        (1 << 21, q20(2e6).to_vec()),
        (1 << 29, q20(1.5e6).to_vec()),
        (1 << 23, (12u32 * 128).to_be_bytes().to_vec()),
    ];
    for (index, (bit, value)) in requests.into_iter().enumerate() {
        let request = vita49::hand_command(17, bit, &value);
        peer.send_to(&request, endpoint.socket.local_addr().unwrap())
            .unwrap();
        let datagram = poll(&endpoint);
        endpoint
            .dispatch(&datagram, &mut ctl, &mut backend)
            .unwrap();
        let ack = receive(&peer);
        assert_eq!(backend.calls.len(), index + 1);
        assert_eq!(word(&ack, 0) >> 28, 6);
        assert_eq!(word(&ack, 0) & 0xffff, (ack.len() / 4) as u32);
        assert_eq!(word(&ack, 4), 17);
        assert_ne!(word(&ack, 0) & (1 << 26), 0); // ACK, not echoed request
        assert_eq!(word(&ack, 24), 1); // echoed Message ID
        assert_eq!(
            word(&ack, 20) & ((1 << 19) | (1 << 10)),
            (1 << 19) | (1 << 10)
        );
        assert_eq!(word(&ack, 20) & (1 << 16), 0);
        assert_eq!(hz(&ack, 32), ctl.applied.bandwidth_hz);
        assert_eq!(hz(&ack, 40), ctl.applied.rf_freq_hz);
        assert_eq!(
            word(&ack, 48) as u16 as i16 as f64 / 128.0,
            ctl.applied.gain_db
        );
        assert_eq!(hz(&ack, 52), ctl.applied.sample_rate_hz);
        assert_eq!(Some(ctl.applied), backend.state);
    }
    assert_eq!(
        ctl.applied,
        StreamContext {
            rf_freq_hz: 915e6 + 10.0,
            sample_rate_hz: 2e6,
            bandwidth_hz: 1.5e6,
            gain_db: 12.0,
        }
    );
    let before = ctl.applied;
    for (stream, bit, value) in [
        (17, 1 << 26, vec![0; 8]),
        (99, 1 << 27, q20(900e6).to_vec()),
        (17, (1 << 27) | (1 << 26), q20(900e6).to_vec()),
    ] {
        peer.send_to(
            &vita49::hand_command(stream, bit, &value),
            endpoint.socket.local_addr().unwrap(),
        )
        .unwrap();
        endpoint
            .dispatch(&poll(&endpoint), &mut ctl, &mut backend)
            .unwrap();
        let ack = receive(&peer);
        assert_ne!(word(&ack, 20) & (1 << 16), 0); // errors reported, not executed
        assert_eq!(word(&ack, 20) & (1 << 10), 0);
        assert_eq!(ctl.applied, before);
        assert_eq!(backend.state, Some(before));
        assert_eq!(backend.calls.len(), 4);
    }
}

#[test]
fn unauthorized_peer_and_malformed_packet_never_apply_and_hardware_failure_has_no_ack() {
    let (endpoint, peer, mut ctl, mut backend) = harness();
    let observer = UdpSocket::bind("127.0.0.1:0").unwrap();
    observer
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    observer
        .send_to(
            &vita49::hand_command(17, 1 << 27, &q20(915e6)),
            endpoint.socket.local_addr().unwrap(),
        )
        .unwrap();
    for _ in 0..10 {
        assert!(endpoint.poll().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(observer.recv(&mut [0u8; 256]).is_err());
    assert!(backend.calls.is_empty());
    peer.send_to(&[0; 3], endpoint.socket.local_addr().unwrap())
        .unwrap();
    endpoint
        .dispatch(&poll(&endpoint), &mut ctl, &mut backend)
        .unwrap();
    assert!(backend.calls.is_empty());
    assert!(peer.recv(&mut [0u8; 256]).is_err());
    backend.fail = true;
    peer.send_to(
        &vita49::hand_command(17, 1 << 27, &q20(915e6)),
        endpoint.socket.local_addr().unwrap(),
    )
    .unwrap();
    assert!(endpoint
        .dispatch(&poll(&endpoint), &mut ctl, &mut backend)
        .is_err());
    assert_eq!(ctl.applied, initial());
    assert!(peer.recv(&mut [0u8; 256]).is_err());
}

#[test]
fn independent_data_fixture_is_reachable_without_uhd() {
    let fixture = [
        0x10, 0xd7, 0, 6, 0x56, 0x49, 0x54, 0x41, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 1, 0xff,
        0xff,
    ];
    let packet = vita49::parse(&fixture).unwrap();
    assert_eq!(packet.stream_id, u32::from_be_bytes(*b"VITA"));
    assert_eq!(packet.packet_count, 7);
    assert_eq!(packet.ts_int, 1);
    assert_eq!(packet.ts_frac, 2);
    assert_eq!(packet.iq_i16(), vec![1, -1]);
    assert!(vita49::parse(&fixture[..23]).is_err());
}

#[test]
fn two_host_subscribers_receive_context_and_samples_without_uhd() {
    // One production sender -> host UDP fanout relay -> two already-bound
    // subscribers. This proves the contract without assuming multicast routing.
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let relay = UdpSocket::bind("127.0.0.1:0").unwrap();
    for receiver in [&a, &b, &relay] {
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
    }
    let mut forwarder =
        vita49::Vita49Forwarder::new("127.0.0.1", relay.local_addr().unwrap().port(), 17).unwrap();
    forwarder.set_context(initial());
    forwarder.send(&[1, -1], 1.0, 1e6);
    for expected_type in [4, 1] {
        let packet = receive(&relay);
        assert_eq!(word(&packet, 0) >> 28, expected_type);
        assert_eq!(word(&packet, 4), 17);
        for subscriber in [&a, &b] {
            relay
                .send_to(&packet, subscriber.local_addr().unwrap())
                .unwrap();
        }
        let got_a = receive(&a);
        let got_b = receive(&b);
        assert_eq!(got_a, packet);
        assert_eq!(got_b, packet);
        if expected_type == 1 {
            assert_eq!(vita49::parse(&got_a).unwrap().iq_i16(), vec![1, -1]);
        } else {
            // Independent fixed-offset decoder for the documented CIF0 subset.
            assert_eq!(word(&got_a, 20), 0xa8a08000);
            assert_eq!(hz(&got_a, 24), 1e6);
            assert_eq!(hz(&got_a, 32), 100e6);
            assert_eq!(hz(&got_a, 44), 1e6);
            assert_eq!(word(&got_a, 52), 0x200003cf);
        }
    }
}

#[test]
fn python_rate_client_talks_to_real_udp_controllee() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = peer.local_addr().unwrap().port();
    drop(peer);
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let endpoint = ControlSocket {
        socket,
        peer: format!("127.0.0.1:{port}").parse().unwrap(),
    };
    let control_port = endpoint.socket.local_addr().unwrap().port();
    let client = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test/vrt_set_rate.py");
    // An explicit override is authoritative; otherwise use the platform's
    // conventional Python 3 executable (Windows does not ship `python3`).
    let python = std::env::var_os("SIGPROC_TEST_PYTHON").unwrap_or_else(|| {
        if cfg!(windows) { "python" } else { "python3" }.into()
    });
    let mut child = std::process::Command::new(python)
        .arg(client)
        .args([
            "250000",
            "--port",
            &control_port.to_string(),
            "--source-port",
            &port.to_string(),
            "--stream-id",
            "17",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut ctl = Controllee::new(initial(), 17);
    let mut backend = MockBackend {
        state: Some(initial()),
        ..Default::default()
    };
    let datagram = poll(&endpoint);
    endpoint
        .dispatch(&datagram, &mut ctl, &mut backend)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("rate client timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("actual=250000"));
    assert_eq!(ctl.applied.sample_rate_hz, 250_000.0);
    assert_eq!(backend.calls.len(), 1);
}

#[test]
fn default_control_endpoint_is_disabled() {
    assert!(ControlSocket::bind(&ControlConfig::default())
        .unwrap()
        .is_none());
}

/// Build the query-state command a Soapy client sends on open: no action, the
/// query-state request bit, and zeroed field bodies for the requested fields.
fn query_command(stream_id: u32, cif0: u32, message_id: u32) -> Vec<u8> {
    let body_bytes = [(1u32 << 29, 8usize), (1 << 27, 8), (1 << 23, 4), (1 << 21, 8)]
        .iter()
        .filter(|(bit, _)| cif0 & bit != 0)
        .map(|(_, size)| size)
        .sum::<usize>();
    let mut buf = Vec::new();
    let header = (6u32 << 28) | (6 + body_bytes as u32 / 4);
    for value in [
        header,
        stream_id,
        (1 << 31) | (1 << 18),
        message_id,
        stream_id,
        cif0,
    ] {
        buf.extend_from_slice(&value.to_be_bytes());
    }
    buf.extend(std::iter::repeat_n(0u8, body_bytes));
    buf
}

#[test]
fn query_state_answers_from_confirmed_state_without_touching_hardware() {
    let (endpoint, peer, mut ctl, backend) = harness();
    let cif0 = (1 << 29) | (1 << 27) | (1 << 23) | (1 << 21);
    let request = query_command(17, cif0, 42);
    peer.send_to(&request, endpoint.socket.local_addr().unwrap())
        .unwrap();
    assert!(endpoint
        .prepare_streaming(&poll(&endpoint), &mut ctl, || panic!("query stopped RX"))
        .unwrap()
        .is_none(), "query must not request a streamer restart");
    let reply = receive(&peer);

    assert_eq!(word(&reply, 4), 17, "stream id echoed");
    // The request carried a controllee ID, so the acknowledgement echoes it and
    // every later word shifts by one.
    let cam = word(&reply, 20);
    assert_eq!(cam & (1 << 16), 0, "query must not report errors");
    assert_ne!(cam & (1 << 18), 0, "query-state acknowledgement bit");
    assert_eq!(cam & ((1 << 19) | (1 << 10)), 0, "query must not claim execution");
    assert_eq!(word(&reply, 24), 42, "message id echoed");
    assert_eq!(word(&reply, 32), cif0, "supported fields reported");
    assert_eq!(hz(&reply, 36), 1e6, "bandwidth from confirmed state");
    assert_eq!(hz(&reply, 44), 100e6, "rf frequency from confirmed state");
    assert_eq!(hz(&reply, 56), 1e6, "sample rate from confirmed state");
    assert_eq!(word(&reply, 52) as u16 as i16 as f64 / 128.0, 10.0, "gain from confirmed state");
    assert!(
        backend.calls.is_empty(),
        "a query must never reach the radio"
    );
    assert_eq!(ctl.applied, initial(), "a query must not mutate state");

    let request = query_command(17, 1 << 27, 43);
    peer.send_to(&request, endpoint.socket.local_addr().unwrap()).unwrap();
    assert!(endpoint
        .prepare_streaming(&poll(&endpoint), &mut ctl, || panic!("subset query stopped RX"))
        .unwrap()
        .is_none());
    let reply = receive(&peer);
    assert_eq!(word(&reply, 24), 43);
    assert_eq!(word(&reply, 20) & ((1 << 19) | (1 << 10) | (1 << 16)), 0);
    assert_eq!(hz(&reply, 44), initial().rf_freq_hz);
    assert!(backend.calls.is_empty());
}

#[test]
fn unsupported_dry_run_never_applies_even_with_valid_values() {
    let (endpoint, peer, mut ctl, mut backend) = harness();
    let mut request = vita49::hand_command(17, 1 << 27, &q20(433e6));
    // Rewrite the CAM action from execute (2) to dry run (1).
    let cam = (word(&request, 8) & !(3 << 23)) | (1 << 23);
    request[8..12].copy_from_slice(&cam.to_be_bytes());
    peer.send_to(&request, endpoint.socket.local_addr().unwrap())
        .unwrap();
    endpoint
        .dispatch(&poll(&endpoint), &mut ctl, &mut backend)
        .unwrap();
    let reply = receive(&peer);

    let cam = word(&reply, 20);
    assert_ne!(cam & (1 << 16), 0, "dry-run is not hardware validation");
    assert_eq!(cam & (1 << 10), 0, "a dry run executes nothing");
    assert_eq!(hz(&reply, 40), 100e6, "dry run reports unapplied state");
    assert!(backend.calls.is_empty(), "a dry run must not reach the radio");
    assert_eq!(ctl.applied, initial(), "a dry run must not mutate state");

    let mut bad = vita49::hand_command(17, 1 << 27, &q20(-5.0));
    let cam = (word(&bad, 8) & !(3 << 23)) | (1 << 23);
    bad[8..12].copy_from_slice(&cam.to_be_bytes());
    peer.send_to(&bad, endpoint.socket.local_addr().unwrap())
        .unwrap();
    endpoint
        .dispatch(&poll(&endpoint), &mut ctl, &mut backend)
        .unwrap();
    let reply = receive(&peer);
    assert_ne!(
        word(&reply, 20) & (1 << 16),
        0,
        "an invalid dry run must be rejected"
    );
    assert!(backend.calls.is_empty());
}

#[test]
fn no_action_without_query_request_is_rejected() {
    let (endpoint, peer, mut ctl, mut backend) = harness();
    let mut request = vita49::hand_command(17, 1 << 27, &q20(915e6));
    let cam = word(&request, 8) & !(3 << 23);
    request[8..12].copy_from_slice(&cam.to_be_bytes());
    peer.send_to(&request, endpoint.socket.local_addr().unwrap())
        .unwrap();
    endpoint
        .dispatch(&poll(&endpoint), &mut ctl, &mut backend)
        .unwrap();
    let reply = receive(&peer);
    assert_ne!(word(&reply, 20) & (1 << 16), 0);
    assert!(backend.calls.is_empty());
    assert_eq!(ctl.applied, initial());
}

#[test]
fn only_validated_execute_requests_a_streamer_restart() {
    let (endpoint, peer, mut ctl, mut backend) = harness();
    let valid = vita49::hand_command(17, 1 << 27, &q20(915e6));
    let mut cases = vec![
        vita49::hand_command(99, 1 << 27, &q20(915e6)),
        vita49::hand_command(17, 1 << 26, &[0; 8]),
        vita49::hand_command(17, (1 << 27) | (1 << 26), &[0; 16]),
        vita49::hand_command(17, 1 << 27, &q20(-1.0)),
        vita49::hand_command(17, 0, &[]),
        query_command(17, 0, 1),
    ];
    for action in [0u32, 1, 3] {
        let mut request = valid.clone();
        let cam = (word(&request, 8) & !(3 << 23)) | (action << 23);
        request[8..12].copy_from_slice(&cam.to_be_bytes());
        cases.push(request);
    }
    for action in [1u32, 2, 3] {
        let mut request = valid.clone();
        let cam = (action << 23) | (1 << 18);
        request[8..12].copy_from_slice(&cam.to_be_bytes());
        cases.push(request);
    }
    let mut foreign_controllee = query_command(17, 1 << 27, 3);
    foreign_controllee[16..20].copy_from_slice(&99u32.to_be_bytes());
    cases.push(foreign_controllee);
    let mut cancelled = valid.clone();
    let header = word(&cancelled, 0) | (1 << 24);
    cancelled[..4].copy_from_slice(&header.to_be_bytes());
    cases.push(cancelled);
    for request in cases {
        peer.send_to(&request, endpoint.socket.local_addr().unwrap()).unwrap();
        assert!(endpoint.prepare_streaming(&poll(&endpoint), &mut ctl, || panic!("rejected control stopped RX")).unwrap().is_none());
        let ack = receive(&peer);
        assert_eq!(word(&ack, 20) & (1 << 10), 0, "nonmutating ACK cannot claim execute");
        assert_ne!(word(&ack, 20) & (1 << 16), 0, "invalid/unsupported request must reject");
        assert!(backend.calls.is_empty());
        assert_eq!(ctl.applied, initial());
    }

    // Malformed packets are dropped and must not stop the streamer either.
    peer.send_to(&[0; 7], endpoint.socket.local_addr().unwrap()).unwrap();
    assert!(endpoint.prepare_streaming(&poll(&endpoint), &mut ctl, || panic!("malformed control stopped RX")).unwrap().is_none());
    assert!(peer.recv(&mut [0; 256]).is_err());

    peer.send_to(&valid, endpoint.socket.local_addr().unwrap()).unwrap();
    let mut stops = 0;
    let pending = endpoint
        .prepare_streaming(&poll(&endpoint), &mut ctl, || {
            stops += 1;
            Ok(())
        })
        .unwrap()
        .expect("only a validated execute requests streamer release");
    assert_eq!(stops, 1, "valid execute must stop RX exactly once");
    assert!(backend.calls.is_empty(), "preparation must not reach hardware");
    assert_eq!(ctl.applied, initial());
    assert!(peer.recv(&mut [0; 256]).is_err(), "no ACK before hardware readback");
    endpoint.execute(pending, &mut ctl, &mut backend).unwrap();
    let ack = receive(&peer);
    assert_ne!(word(&ack, 20) & (1 << 10), 0);
    assert_eq!(backend.calls.len(), 1);
    assert_eq!(hz(&ack, 40), 915e6 + 10.0, "ACK contains backend readback");
}

#[test]
fn prepared_execute_cannot_apply_to_changed_or_foreign_state() {
    for foreign in [false, true] {
        let (endpoint, _, mut ctl, mut backend) = harness();
        let request = vita49::hand_command(17, 1 << 27, &q20(915e6));
        let pending = endpoint.prepare(&request, &mut ctl).unwrap().unwrap();
        let mut target = if foreign {
            Controllee::new(initial(), 99)
        } else {
            ctl.applied.sample_rate_hz = 2e6;
            ctl
        };
        assert!(endpoint.execute(pending, &mut target, &mut backend).is_err());
        assert!(backend.calls.is_empty(), "stale plan must fail before hardware");
    }
}

#[test]
fn streamer_stop_failure_cannot_proceed_to_hardware_or_ack() {
    let (endpoint, peer, mut ctl, backend) = harness();
    let request = vita49::hand_command(17, 1 << 27, &q20(915e6));
    let mut stops = 0;
    let result = endpoint.prepare_streaming(&request, &mut ctl, || {
        stops += 1;
        anyhow::bail!("streamer failed to stop")
    });
    assert!(result.is_err());
    assert_eq!(stops, 1);
    assert_eq!(ctl.applied, initial());
    assert!(backend.calls.is_empty());
    assert!(peer.recv(&mut [0; 256]).is_err(), "no ACK before apply/readback");
}
