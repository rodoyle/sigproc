//! Simulation of a sweep against a stand-in channelizer over a real socket.
//!
//! The unit tests in `src/tests.rs` exercise the controller through its traits and
//! `src/http.rs` exercises the client against canned responses. Neither covers
//! the two together, which is where the operator-visible contract actually lives:
//! a real TCP exchange, a real `POST /config`, a real readback, and — crucially —
//! a real restore request after the sweep stops.
//!
//! These tests run the whole stack against a stub that speaks the channelizer's
//! wire format and records every retune it applied.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use sigproc_sweep::{
    http::{ChannelizerHttp, HttpLimits},
    ChannelState, RestoreOutcome, StdClock, StopReason, SweepConfig, SweepController, SweepPlan,
    NBFM_STEP_HZ,
};

/// What the stub actually applied, in order.
type Applied = Arc<Mutex<Vec<ChannelState>>>;

/// A stand-in channelizer: `GET /state` reports its current channel, `POST
/// /config` applies the requested offset and rate and echoes the readback.
struct Stub {
    addr: SocketAddr,
    applied: Applied,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Stub {
    /// `fail_on_connection` closes that connection without answering, which is
    /// how a lost reply looks to the controller.
    fn start(initial: ChannelState, fail_on_connection: Option<usize>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();

        let applied: Applied = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let state = Arc::new(Mutex::new(initial));
        let applied_clone = applied.clone();
        let stop_clone = stop.clone();
        let handle = thread::spawn(move || {
            let mut served = 0usize;
            while !stop_clone.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        serve(stream, &state, &applied_clone, served, fail_on_connection);
                        served += 1;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            applied,
            stop,
            handle: Some(handle),
        }
    }

    fn applied(&self) -> Vec<ChannelState> {
        self.applied.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    stream: TcpStream,
    state: &Arc<Mutex<ChannelState>>,
    applied: &Applied,
    index: usize,
    fail_on_connection: Option<usize>,
) {
    if fail_on_connection == Some(index) {
        // Accept and close without a reply: the request was delivered, the
        // answer was not.
        return;
    }
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut head = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        let done = line == "\r\n";
        head.push_str(&line);
        if done {
            break;
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).unwrap();
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let mut stream = stream;
    if head.starts_with("GET /state") {
        let current = *state.lock().unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n\
             {{\"offset_hz\":{},\"output_rate_hz\":{},\"passband_hz\":5000,\
             \"input_rate_hz\":10000000,\"decimation\":800,\"settling_samples\":64,\
             \"packets_received\":10,\"packets_dropped\":0,\"seq_gaps\":0,\
             \"output_power_dbfs\":-40.0,\"peak_offset_hz\":0.0}}",
            current.offset_hz, current.output_rate_hz
        );
        let _ = stream.write_all(response.as_bytes());
    } else if head.starts_with("POST /config") {
        let offset = field(&body, "offset_hz").unwrap_or(0);
        let rate = field(&body, "output_rate_hz").unwrap_or(NBFM_STEP_HZ as i64) as u64;
        let applied_state = ChannelState {
            offset_hz: offset,
            output_rate_hz: rate,
        };
        *state.lock().unwrap() = applied_state;
        applied.lock().unwrap().push(applied_state);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n\
             {{\"ok\":true,\"offset_hz\":{offset},\"output_rate_hz\":{rate},\
             \"settling_samples\":64,\"error\":null}}"
        );
        let _ = stream.write_all(response.as_bytes());
    } else {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
    }
    let _ = stream.flush();
}

/// Pull an integer field out of the flat JSON bodies these tests exchange.
fn field(body: &str, name: &str) -> Option<i64> {
    let key = format!("\"{name}\":");
    let start = body.find(&key)? + key.len();
    let rest = &body[start..];
    let end = rest
        .find(|c: char| c != '-' && !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn limits() -> HttpLimits {
    HttpLimits {
        connect: Duration::from_secs(2),
        read: Duration::from_secs(2),
    }
}

const ORIGINAL: ChannelState = ChannelState {
    offset_hz: -1_250_000,
    output_rate_hz: 12_500,
};

#[test]
fn a_simulated_sweep_visits_every_endpoint_and_restores_the_original() {
    let stub = Stub::start(ORIGINAL, None);
    let client = ChannelizerHttp::new(stub.addr, None, limits());
    let stop = Arc::new(AtomicBool::new(false));
    let mut controller = SweepController::new(Box::new(client), Box::new(StdClock::new()), stop);

    let plan = SweepPlan::new(140_000_000, 140_025_000, NBFM_STEP_HZ).unwrap();
    let report = controller.run(&SweepConfig {
        plan,
        settle_ms: 0,
        observe: false,
        ..SweepConfig::default()
    });

    assert_eq!(report.stop, StopReason::Completed);
    assert_eq!(report.steps.len(), 3);
    assert_eq!(report.applied_matching(), 3, "{:?}", report.steps);
    assert_eq!(report.restore, RestoreOutcome::Restored);

    // What actually crossed the wire: three retunes ending on the last endpoint,
    // then the original channel put back.
    let applied = stub.applied();
    assert_eq!(
        applied,
        vec![
            ChannelState {
                offset_hz: -5_000_000,
                output_rate_hz: NBFM_STEP_HZ
            },
            ChannelState {
                offset_hz: -4_987_500,
                output_rate_hz: NBFM_STEP_HZ
            },
            ChannelState {
                offset_hz: -4_975_000,
                output_rate_hz: NBFM_STEP_HZ
            },
            ORIGINAL,
        ]
    );
}

#[test]
fn a_lost_reply_mid_sweep_stops_it_and_still_restores() {
    // Connection 0 is the preflight GET /state, connection 1 the first retune,
    // connection 2 the second retune — which is answered with silence.
    let stub = Stub::start(ORIGINAL, Some(2));
    let client = ChannelizerHttp::new(stub.addr, None, limits());
    let stop = Arc::new(AtomicBool::new(false));
    let mut controller = SweepController::new(Box::new(client), Box::new(StdClock::new()), stop);

    let plan = SweepPlan::new(140_000_000, 140_050_000, NBFM_STEP_HZ).unwrap();
    let report = controller.run(&SweepConfig {
        plan,
        settle_ms: 0,
        observe: false,
        ..SweepConfig::default()
    });

    assert_eq!(report.stop, StopReason::Uncertain);
    // One applied step, one uncertain step, and no retry of the uncertain one.
    assert_eq!(report.steps.len(), 2);
    assert_eq!(report.restore, RestoreOutcome::Restored);

    let applied = stub.applied();
    let first = ChannelState {
        offset_hz: -5_000_000,
        output_rate_hz: NBFM_STEP_HZ,
    };
    assert_eq!(applied.first(), Some(&first));
    // The second offset was sent once and never re-sent, so it never appears as
    // an applied retune; the only later write is the restore.
    assert_eq!(
        applied.iter().filter(|s| **s == first).count(),
        1,
        "the uncertain request was retried: {applied:?}"
    );
    assert_eq!(applied.last(), Some(&ORIGINAL));
}
