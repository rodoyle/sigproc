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
        assert_eq!(word(&ack, 20) & ((1 << 19) | (1 << 10)), (1 << 19) | (1 << 10));
        assert_eq!(word(&ack, 20) & (1 << 16), 0);
        assert_eq!(hz(&ack, 32), ctl.applied.bandwidth_hz);
        assert_eq!(hz(&ack, 40), ctl.applied.rf_freq_hz);
        assert_eq!(word(&ack, 48) as u16 as i16 as f64 / 128.0, ctl.applied.gain_db);
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
        receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    }
    let mut forwarder = vita49::Vita49Forwarder::new(
        "127.0.0.1", relay.local_addr().unwrap().port(), 17,
    ).unwrap();
    forwarder.set_context(initial());
    forwarder.send(&[1, -1], 1.0, 1e6);
    for expected_type in [4, 1] {
        let packet = receive(&relay);
        assert_eq!(word(&packet, 0) >> 28, expected_type);
        assert_eq!(word(&packet, 4), 17);
        for subscriber in [&a, &b] {
            relay.send_to(&packet, subscriber.local_addr().unwrap()).unwrap();
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
fn default_control_endpoint_is_disabled() {
    assert!(ControlSocket::bind(&ControlConfig::default())
        .unwrap()
        .is_none());
}
