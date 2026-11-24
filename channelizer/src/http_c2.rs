//! Channelizer HTTP control plane: `/state` and `/config`.
//!
//! The DSP pipeline runs single-threaded in the receive loop. This server
//! parses incoming HTTP requests and hands them to the receive loop over one
//! channel. The loop computes the state snapshot or applies the reconfiguration
//! inline and writes the HTTP response directly — no reply channels needed.
//!
//! * `GET  /state` — reply with a `ChannelizerState` snapshot.
//! * `POST /config` — apply the reconfiguration, reply with a `ReconfigResponse`.
//!
//! The receive loop drains the channel with `try_recv` after each UDP receive
//! (which already has a read timeout), so HTTP never blocks sample processing
//! and the receive loop never blocks HTTP.

use crate::ReconfigRequest;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// A parsed HTTP request carried to the receive loop for processing. The loop
/// computes the reply and writes the HTTP response directly to the socket.
#[derive(Debug)]
pub enum LoopRequest {
    /// `GET /state` — reply with a state snapshot.
    State(TcpStream),
    /// `POST /config` — apply the reconfiguration, reply with the result.
    Config(ReconfigRequest, TcpStream),
}

/// Spawn the channelizer control-plane HTTP server on `listen`.
///
/// Returns a handle to the acceptor thread and the receiver the receive loop
/// must drain to process HTTP requests.
pub fn spawn_c2_http(
    listen: SocketAddr,
) -> std::io::Result<(
    std::thread::JoinHandle<()>,
    Receiver<LoopRequest>,
    SocketAddr,
)> {
    let listener = TcpListener::bind(listen)?;
    let bound = listener.local_addr()?;
    log::info!("channelizer C2 listening on http://{bound}/state");

    let (tx, rx) = mpsc::sync_channel::<LoopRequest>(8);

    let tx2 = tx.clone();
    let acceptor = std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let _ = tx2.try_send(parse_and_build_request(stream));
                }
                Err(e) => log::warn!("C2 accept error: {e}"),
            }
        }
    });

    Ok((acceptor, rx, bound))
}

/// Parse one HTTP request from a TCP stream and build the corresponding loop
/// request with the socket carried through.
pub fn parse_and_build_request(mut stream: TcpStream) -> LoopRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or(());
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => {
            // Dead connection; drop silently.
            return LoopRequest::State(stream);
        }
    };
    let request = String::from_utf8_lossy(&buf[..n]);
    let method = request.split_whitespace().next().unwrap_or("");
    let path = request.split_whitespace().nth(1).unwrap_or("/");

    match (method, path) {
        ("GET", "/state") => LoopRequest::State(stream),
        ("POST", "/config") => {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
            let req: ReconfigRequest = serde_json::from_str(body).unwrap_or(ReconfigRequest {
                offset_hz: None,
                output_rate_hz: None,
            });
            LoopRequest::Config(req, stream)
        }
        _ => LoopRequest::State(stream),
    }
}

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
