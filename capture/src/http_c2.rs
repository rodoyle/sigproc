//! Capture HTTP control plane: `GET /state` and `POST /config`.
//!
//! The capture loop runs single-threaded and owns the UHD radio exclusively.
//! This server parses incoming HTTP requests and hands them to the capture loop
//! over one bounded channel. The loop computes the state snapshot or applies the
//! reconfiguration inline via the SAME `Controllee`/`ControlBackend` path that
//! handles VITA49.2 commands, then writes the HTTP response directly — no reply
//! channels needed.
//!
//! * `GET  /state` — reply with applied RF settings + live capture metrics. No auth.
//! * `POST /config` — apply RF settings changes (`rf_freq_hz`, `sample_rate_hz`,
//!   `bandwidth_hz`, `gain_db`) through the sole-owner handler; reply with the
//!   applied hardware readback. Requires `Authorization: Bearer <token>`.
//!
//! The capture loop drains the channel with `try_recv` after each UDP batch, so
//! HTTP never blocks sample processing and the loop never blocks HTTP. Mutations
//! are authenticated; the read-only state endpoint is not.

// Built without `uhd-support` (e.g. `cargo clippy --all-targets` on a host
// without libuhd) this module has no consumer at all: the capture loop that
// drains it is feature-gated, and the test module is not compiled. Unused items
// are expected in that configuration rather than a defect worth silencing
// globally at the crate root.
#![cfg_attr(not(feature = "uhd-support"), allow(dead_code))]
use sigproc_common::vita49::{ControlUpdate, StreamContext};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// A parsed HTTP request carried to the capture loop for processing.
#[derive(Debug)]
pub enum LoopRequest {
    /// `GET /state` — reply with applied settings + metrics snapshot.
    State(TcpStream),
    /// `POST /config` — apply RF settings change via the sole-owner handler.
    Config {
        update: ControlUpdate,
        /// `None` means no Authorization header present.
        auth_token: Option<String>,
        stream: TcpStream,
    },
}

/// Response body for `POST /config`: applied hardware readback or error.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigResponse {
    pub ok: bool,
    /// Applied RF frequency in Hz.
    pub rf_freq_hz: f64,
    /// Applied sample rate in Hz.
    pub sample_rate_hz: f64,
    /// Applied bandwidth in Hz.
    pub bandwidth_hz: f64,
    /// Applied gain in dB.
    pub gain_db: f64,
    /// Error message when `ok` is false.
    pub error: Option<String>,
}

impl ConfigResponse {
    pub(crate) fn from_context(ctx: StreamContext) -> Self {
        Self {
            ok: true,
            rf_freq_hz: ctx.rf_freq_hz,
            sample_rate_hz: ctx.sample_rate_hz,
            bandwidth_hz: ctx.bandwidth_hz,
            gain_db: ctx.gain_db,
            error: None,
        }
    }
    pub(crate) fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            rf_freq_hz: 0.0,
            sample_rate_hz: 0.0,
            bandwidth_hz: 0.0,
            gain_db: 0.0,
            error: Some(msg.into()),
        }
    }
}

/// State snapshot for `GET /state`. Applied RF settings plus live capture metrics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StateResponse {
    pub rf_freq_hz: f64,
    pub sample_rate_hz: f64,
    pub bandwidth_hz: f64,
    pub gain_db: f64,
    pub stream_id: u32,
    pub packets_forwarded: u64,
    pub samples_forwarded: u64,
    pub receive_errors: u64,
    pub controls_processed: u64,
}

/// Spawn the capture control-plane HTTP server on `listen`.
///
/// Returns the acceptor thread handle and the receiver the capture loop must drain.
pub fn spawn_c2_http(
    listen: SocketAddr,
) -> std::io::Result<(std::thread::JoinHandle<()>, Receiver<LoopRequest>)> {
    let listener = TcpListener::bind(listen)?;
    let bound = listener.local_addr()?;
    log::info!("capture C2 listening on http://{bound}/state");

    // Bounded channel: a hostile client cannot grow memory unbounded.
    let (tx, rx) = mpsc::sync_channel::<LoopRequest>(8);

    let tx2 = tx.clone();
    let acceptor = std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let _ = tx2.try_send(parse_and_build_request(stream));
                }
                Err(e) => log::warn!("capture C2 accept error: {e}"),
            }
        }
    });

    Ok((acceptor, rx))
}

/// Parse one HTTP request and build the corresponding loop request with the socket.
fn parse_and_build_request(mut stream: TcpStream) -> LoopRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or(());
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return LoopRequest::State(stream),
    };
    let request = String::from_utf8_lossy(&buf[..n]);
    let method = request.split_whitespace().next().unwrap_or("");
    let path = request.split_whitespace().nth(1).unwrap_or("/");

    match (method, path) {
        ("GET", "/state") => LoopRequest::State(stream),
        ("POST", "/config") => {
            let headers = request.split("\r\n\r\n").next().unwrap_or("");
            let auth_token = extract_bearer_token(headers);
            let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
            let update = parse_config_body(body);
            LoopRequest::Config {
                update,
                auth_token,
                stream,
            }
        }
        _ => LoopRequest::State(stream),
    }
}

/// Extract the bearer token from an `Authorization: Bearer <token>` header, if present.
fn extract_bearer_token(headers: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("authorization:") {
            let value = line.split_once(':')?.1.trim();
            value
                .to_ascii_lowercase()
                .strip_prefix("bearer ")
                .map(|_| value[7..].trim().to_string())
        } else {
            None
        }
    })
}

/// Parse the JSON body of a `POST /config` request into a `ControlUpdate`.
/// Unknown/invalid fields are ignored; only present fields are applied.
fn parse_config_body(body: &str) -> ControlUpdate {
    #[derive(serde::Deserialize, Default)]
    struct Body {
        rf_freq_hz: Option<f64>,
        sample_rate_hz: Option<f64>,
        bandwidth_hz: Option<f64>,
        gain_db: Option<f64>,
    }
    let body: Body = serde_json::from_str(body).unwrap_or_default();
    ControlUpdate {
        rf_freq_hz: body.rf_freq_hz,
        sample_rate_hz: body.sample_rate_hz,
        bandwidth_hz: body.bandwidth_hz,
        gain_db: body.gain_db,
    }
}

/// Write an HTTP response to a stream.
pub fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

#[cfg(test)]
#[path = "http_c2_tests.rs"]
mod tests;
