//! HTTP C2 control-plane tests: `/state` and `/config` endpoints.
//!
//! These exercise the bounded request channel, the parse path, and the
//! response-writing helpers directly. The receive-loop integration is covered
//! by the pipeline tests; this suite focuses on the HTTP surface and the
//! reconfiguration readback contract.

use sigproc_channelizer::http_c2::{
    parse_and_build_request, spawn_c2_http, write_response, LoopRequest,
};
use sigproc_channelizer::{ChannelizerState, ReconfigResponse};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

/// Helper: make one HTTP request and read the full response body + status.
///
/// Retries on connection reset, which can occur on Windows if the server drops
/// the socket before the client finishes reading the response.
fn http_get(addr: std::net::SocketAddr, path: &str) -> (String, String) {
    for attempt in 0..3 {
        match TcpStream::connect(addr) {
            Ok(mut stream) => {
                if stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .is_err()
                {
                    continue;
                }
                if write!(
                    stream,
                    "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                )
                .is_err()
                {
                    continue;
                }
                let mut buf = Vec::new();
                match stream.read_to_end(&mut buf) {
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&buf).to_string();
                        let status = text.lines().next().unwrap_or("").to_string();
                        let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                        return (status, body);
                    }
                    Err(_) if attempt < 2 => {
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    Err(e) => panic!("http_get failed after retries: {e}"),
                }
            }
            Err(_) if attempt < 2 => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("http_get connect failed: {e}"),
        }
    }
    unreachable!()
}

fn http_post(addr: std::net::SocketAddr, path: &str, body: &str) -> (String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    write!(stream, "{request}").unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text.lines().next().unwrap_or("").to_string();
    let resp_body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, resp_body)
}

/// Helper: get a connected socket pair for parse/write tests that don't need a listener.
fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).unwrap();
    let server = listener.accept().unwrap().0;
    (client, server)
}

#[test]
fn get_state_returns_current_fields() {
    // Build a state snapshot and write it through the response helper; verify
    // the JSON carries offset_hz, output_rate_hz, passband_hz and metrics.
    let (mut client, mut server) = socket_pair();
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let state = ChannelizerState {
        offset_hz: 1_000_000.0,
        output_rate_hz: 50_000.0,
        passband_hz: 20_000.0,
        input_rate_hz: 2_000_000.0,
        decimation: 40,
        settling_samples: 1024,
        packets_received: 5,
        packets_dropped: 0,
        seq_gaps: 1,
        output_power_dbfs: Some(-12.5),
        peak_offset_hz: Some(1234.0),
    };
    let body = serde_json::to_string(&state).unwrap();
    write_response(&mut server, "200 OK", "application/json", &body).unwrap();
    drop(server);
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("\"offset_hz\":1000000"));
    assert!(text.contains("\"output_rate_hz\":50000"));
    assert!(text.contains("\"passband_hz\":20000"));
    assert!(text.contains("\"output_power_dbfs\":-12.5"));
    assert!(text.contains("HTTP/1.1 200 OK"));
}

#[test]
fn parse_state_request_builds_state_variant() {
    let (mut client, server) = socket_pair();
    write!(client, "GET /state HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    drop(client);
    let req = parse_and_build_request(server);
    match req {
        LoopRequest::State(_) => {}
        other => panic!("expected State variant, got {other:?}"),
    }
}

#[test]
fn parse_config_request_parses_json_body() {
    let (mut client, server) = socket_pair();
    let body = r#"{"offset_hz": 2500000.0, "output_rate_hz": 100000.0}"#;
    write!(
        client,
        "POST /config HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    drop(client);
    let req = parse_and_build_request(server);
    match req {
        LoopRequest::Config(r, _) => {
            assert_eq!(r.offset_hz, Some(2_500_000.0));
            assert_eq!(r.output_rate_hz, Some(100_000.0));
        }
        other => panic!("expected Config variant, got {other:?}"),
    }
}

#[test]
fn parse_config_request_defaults_on_bad_json() {
    let (mut client, server) = socket_pair();
    let body = "not json";
    write!(
        client,
        "POST /config HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    drop(client);
    let req = parse_and_build_request(server);
    match req {
        LoopRequest::Config(r, _) => {
            assert_eq!(r.offset_hz, None);
            assert_eq!(r.output_rate_hz, None);
        }
        other => panic!("expected Config variant, got {other:?}"),
    }
}

#[test]
fn unknown_path_defaults_to_state() {
    let (mut client, server) = socket_pair();
    write!(client, "GET /unknown HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    drop(client);
    let req = parse_and_build_request(server);
    assert!(matches!(req, LoopRequest::State(_)));
}

#[test]
fn write_response_sets_content_length_and_status() {
    let (mut client, mut server) = socket_pair();
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write_response(&mut server, "400 Bad Request", "application/json", "{}").unwrap();
    drop(server);
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("HTTP/1.1 400 Bad Request"));
    assert!(text.contains("Content-Length: 2"));
    assert!(text.contains("Content-Type: application/json"));
}

#[test]
fn spawn_c2_http_returns_receiver_and_acceptor() {
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (_handle, _rx, _bound) = spawn_c2_http(addr).unwrap();
}

#[test]
fn server_accepts_and_queues_requests() {
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (_handle, rx, bound) = spawn_c2_http(addr).unwrap();

    // Connect and send a state request; the acceptor should queue it.
    let mut stream = TcpStream::connect(bound).unwrap();
    write!(
        stream,
        "GET /state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    drop(stream);

    // Give the acceptor a moment to parse and push.
    thread::sleep(Duration::from_millis(200));
    let req = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("request should be queued");
    assert!(matches!(req, LoopRequest::State(_)));
}

#[test]
fn reconfig_response_serializes_fields() {
    let resp = ReconfigResponse {
        ok: true,
        offset_hz: 1_500_000.0,
        output_rate_hz: 75_000.0,
        settling_samples: 2048,
        error: None,
    };
    let body = serde_json::to_string(&resp).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["offset_hz"], 1_500_000.0);
    assert_eq!(parsed["output_rate_hz"], 75_000.0);
    assert_eq!(parsed["settling_samples"], 2048);
    assert!(parsed["error"].is_null());
}

#[test]
fn state_serializes_live_metrics() {
    let state = ChannelizerState::empty();
    let body = serde_json::to_string(&state).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["offset_hz"], 0.0);
    assert_eq!(parsed["packets_received"], 0);
    assert!(parsed["output_power_dbfs"].is_null());
}

#[test]
fn end_to_end_config_request_over_server() {
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (_handle, rx, bound) = spawn_c2_http(addr).unwrap();

    let worker = thread::spawn(move || {
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match req {
            LoopRequest::Config(r, mut stream) => {
                let resp = ReconfigResponse {
                    ok: true,
                    offset_hz: r.offset_hz.unwrap_or(0.0),
                    output_rate_hz: r.output_rate_hz.unwrap_or(0.0),
                    settling_samples: 1024,
                    error: None,
                };
                let body = serde_json::to_string(&resp).unwrap();
                let _ = write_response(&mut stream, "200 OK", "application/json", &body);
            }
            LoopRequest::State(_) => panic!("expected Config"),
        }
    });

    let (status, body) = http_post(
        bound,
        "/config",
        r#"{"offset_hz": 3000000.0, "output_rate_hz": 125000.0}"#,
    );
    worker.join().unwrap();
    assert!(status.contains("200 OK"));
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["offset_hz"], 3_000_000.0);
    assert_eq!(parsed["output_rate_hz"], 125_000.0);
}

#[test]
fn end_to_end_state_request_over_server() {
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (_handle, rx, bound) = spawn_c2_http(addr).unwrap();

    let worker = thread::spawn(move || {
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match req {
            LoopRequest::State(mut stream) => {
                let state = ChannelizerState {
                    offset_hz: 200_000.0,
                    output_rate_hz: 50_000.0,
                    passband_hz: 20_000.0,
                    input_rate_hz: 2_000_000.0,
                    decimation: 40,
                    settling_samples: 1024,
                    packets_received: 7,
                    packets_dropped: 0,
                    seq_gaps: 0,
                    output_power_dbfs: Some(-6.0),
                    peak_offset_hz: Some(123.0),
                };
                let body = serde_json::to_string(&state).unwrap();
                let _ = write_response(&mut stream, "200 OK", "application/json", &body);
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
            LoopRequest::Config(_, _) => panic!("expected State"),
        }
    });

    let (status, body) = http_get(bound, "/state");
    worker.join().unwrap();
    assert!(status.contains("200 OK"));
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["offset_hz"], 200_000.0);
    assert_eq!(parsed["output_rate_hz"], 50_000.0);
    assert_eq!(parsed["packets_received"], 7);
}

#[test]
fn invalid_config_returns_error_and_leaves_state_unchanged() {
    use sigproc_channelizer::{ChannelizerService, CollectingSink, ReconfigRequest, ServiceConfig};
    use sigproc_common::config::ChannelConfig;
    use sigproc_common::metrics::Metrics;
    use std::sync::Arc;

    let channel = ChannelConfig {
        offset_hz: 200_000,
        output_rate_hz: 50_000,
        passband_hz: None,
        stopband_db: 80.0,
        max_taps: 512,
    };
    let input_rate = 2_000_000u32;
    let (plan, cfg) =
        ServiceConfig::from_channel(&channel, input_rate).expect("plan is affordable");
    let mut svc = ChannelizerService::new(
        plan,
        cfg,
        CollectingSink::default(),
        Arc::new(Metrics::new()),
    );
    let before = svc.state();

    // Non-integer decimation ratio must be rejected.
    let bad = ReconfigRequest {
        offset_hz: None,
        output_rate_hz: Some(48_000.0),
    };
    let resp = svc.apply_reconfig(&bad);
    assert!(!resp.ok, "non-integer ratio must be rejected");
    assert!(resp.error.is_some(), "error message must be present");
    // Config unchanged on failure.
    let after = svc.state();
    assert_eq!(after.offset_hz, before.offset_hz);
    assert_eq!(after.output_rate_hz, before.output_rate_hz);

    // Valid change succeeds.
    let good = ReconfigRequest {
        offset_hz: Some(300_000.0),
        output_rate_hz: None,
    };
    let resp = svc.apply_reconfig(&good);
    assert!(resp.ok, "valid offset change must succeed");
    assert_eq!(resp.offset_hz, 300_000.0);
    assert_eq!(svc.state().offset_hz, 300_000.0);
}
