//! Blocking HTTP/1.1 clients for the two control planes the sweep drives.
//!
//! Hand-rolled on purpose. The controller needs two verbs per service, and — more
//! importantly — it needs the *classification* of a failure, which a
//! general-purpose client hides behind one opaque error:
//!
//! * the connection never opened → the request was never sent
//!   ([`ControlError::NotSent`], nothing to restore);
//! * the request was written and the reply was lost, unreadable, or carried a
//!   value we cannot represent in whole hertz → the outcome is unknown
//!   ([`ControlError::Uncertain`], restore owed, never retried);
//! * the service answered and refused → the applied state is known and unchanged
//!   ([`ControlError::Rejected`]).
//!
//! A write that fails after the connection opened is treated as *uncertain*, not
//! as not-sent: the bytes may already have reached the service, and guessing in
//! that direction is the one guess this controller must not make.
//!
//! Requests carry integer hertz. The services' JSON fields are `f64` for their
//! DSP types, and an integer JSON number round-trips exactly through them, so no
//! float formatting ever enters a request; readback is converted back to integers
//! and a fractional value is reported as uncertain rather than rounded.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use crate::{
    CaptureControl, ChannelApplied, ChannelControl, ChannelState, ControlError, Observation,
    RfState,
};

/// Per-request wall-clock bounds. Bounded so a wedged service cannot hold the
/// sweep open indefinitely.
#[derive(Debug, Clone, Copy)]
pub struct HttpLimits {
    pub connect: Duration,
    pub read: Duration,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(3),
            read: Duration::from_secs(5),
        }
    }
}

/// Send one request and return the response body, or a classified failure.
fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    bearer: Option<&str>,
    limits: HttpLimits,
) -> Result<String, ControlError> {
    let mut stream = TcpStream::connect_timeout(&addr, limits.connect).map_err(|e| {
        ControlError::NotSent(format!("connect to {addr} failed, request not sent: {e}"))
    })?;
    stream.set_read_timeout(Some(limits.read)).ok();
    stream.set_write_timeout(Some(limits.connect)).ok();

    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(token) = bearer {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(body) = body {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("\r\n");
    if let Some(body) = body {
        request.push_str(body);
    }

    stream.write_all(request.as_bytes()).map_err(|e| {
        ControlError::Uncertain(format!(
            "connection to {addr} opened but the request write failed ({e}); \
             the service may have received part of it, so the outcome is unknown"
        ))
    })?;
    if let Err(e) = stream.flush() {
        return Err(ControlError::Uncertain(format!(
            "connection to {addr} opened but the request could not be flushed ({e})"
        )));
    }

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| {
        ControlError::Uncertain(format!(
            "request sent to {addr} but the response could not be read ({e})"
        ))
    })?;
    if raw.is_empty() {
        return Err(ControlError::Uncertain(format!(
            "request sent to {addr} but the connection closed without a response"
        )));
    }
    let text = String::from_utf8_lossy(&raw);
    let (status, body) = split_response(&text).ok_or_else(|| {
        ControlError::Uncertain(format!(
            "request sent to {addr} but the response had no header terminator"
        ))
    })?;
    if status != 200 {
        return Err(ControlError::Uncertain(format!(
            "request sent to {addr} and the service answered HTTP {status}: {body}"
        )));
    }
    Ok(body.to_string())
}

/// Split an HTTP response into status code and body.
fn split_response(text: &str) -> Option<(u16, &str)> {
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((status, body))
}

/// A read-back value that must be a whole, non-negative number of hertz.
fn hz_u64(value: f64, field: &str) -> Result<u64, ControlError> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return Err(ControlError::Uncertain(format!(
            "{field} read back as {value}, which is not a non-negative whole number of hertz"
        )));
    }
    Ok(value as u64)
}

/// A read-back value that must be a whole number of hertz, sign allowed.
fn hz_i64(value: f64, field: &str) -> Result<i64, ControlError> {
    if !value.is_finite() || value.fract() != 0.0 {
        return Err(ControlError::Uncertain(format!(
            "{field} read back as {value}, which is not a whole number of hertz"
        )));
    }
    Ok(value as i64)
}

// ── Channelizer ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ChannelizerStateJson {
    offset_hz: f64,
    output_rate_hz: f64,
    #[serde(default)]
    packets_received: u64,
    #[serde(default)]
    seq_gaps: u64,
    #[serde(default)]
    output_power_dbfs: Option<f32>,
    #[serde(default)]
    peak_offset_hz: Option<f32>,
}

#[derive(serde::Deserialize)]
struct ReconfigJson {
    ok: bool,
    offset_hz: f64,
    output_rate_hz: f64,
    #[serde(default)]
    settling_samples: u64,
    #[serde(default)]
    error: Option<String>,
}

/// Client for the channelizer's `GET /state` and `POST /config`.
pub struct ChannelizerHttp {
    addr: SocketAddr,
    limits: HttpLimits,
    bearer: Option<String>,
}

impl ChannelizerHttp {
    pub fn new(addr: SocketAddr, bearer: Option<String>, limits: HttpLimits) -> Self {
        Self {
            addr,
            limits,
            bearer,
        }
    }

    fn state_json(&mut self) -> Result<ChannelizerStateJson, ControlError> {
        let body = request(
            self.addr,
            "GET",
            "/state",
            None,
            self.bearer.as_deref(),
            self.limits,
        )?;
        serde_json::from_str(&body).map_err(|e| {
            ControlError::Uncertain(format!("channelizer /state body was not valid JSON: {e}"))
        })
    }
}

impl ChannelControl for ChannelizerHttp {
    fn channel_state(&mut self) -> Result<ChannelState, ControlError> {
        let state = self.state_json()?;
        Ok(ChannelState {
            offset_hz: hz_i64(state.offset_hz, "offset_hz")?,
            output_rate_hz: hz_u64(state.output_rate_hz, "output_rate_hz")?,
        })
    }

    fn apply_channel(
        &mut self,
        offset_hz: i64,
        output_rate_hz: u64,
    ) -> Result<ChannelApplied, ControlError> {
        let payload = format!("{{\"offset_hz\":{offset_hz},\"output_rate_hz\":{output_rate_hz}}}");
        let body = request(
            self.addr,
            "POST",
            "/config",
            Some(&payload),
            self.bearer.as_deref(),
            self.limits,
        )?;
        let reconfig: ReconfigJson = serde_json::from_str(&body).map_err(|e| {
            ControlError::Uncertain(format!(
                "channelizer /config body was not valid JSON, so the retune outcome is unknown: {e}"
            ))
        })?;
        if !reconfig.ok {
            return Err(ControlError::Rejected(
                reconfig
                    .error
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| "channelizer refused the retune".to_string()),
            ));
        }
        Ok(ChannelApplied {
            offset_hz: hz_i64(reconfig.offset_hz, "offset_hz")?,
            output_rate_hz: hz_u64(reconfig.output_rate_hz, "output_rate_hz")?,
            settling_samples: reconfig.settling_samples,
        })
    }

    fn observe(&mut self) -> Result<Observation, ControlError> {
        let state = self.state_json()?;
        Ok(Observation {
            output_power_dbfs: state.output_power_dbfs,
            peak_offset_hz: state.peak_offset_hz,
            packets_received: state.packets_received,
            seq_gaps: state.seq_gaps,
        })
    }
}

// ── Capture ─────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct RfStateJson {
    rf_freq_hz: f64,
    sample_rate_hz: f64,
}

#[derive(serde::Deserialize)]
struct CaptureConfigJson {
    ok: bool,
    rf_freq_hz: f64,
    sample_rate_hz: f64,
    #[serde(default)]
    error: Option<String>,
}

/// Client for the capture service's `GET /state` and authenticated
/// `POST /config`.
pub struct CaptureHttp {
    addr: SocketAddr,
    limits: HttpLimits,
    bearer: Option<String>,
}

impl CaptureHttp {
    pub fn new(addr: SocketAddr, bearer: Option<String>, limits: HttpLimits) -> Self {
        Self {
            addr,
            limits,
            bearer,
        }
    }

    fn state_json(&mut self) -> Result<RfStateJson, ControlError> {
        let body = request(
            self.addr,
            "GET",
            "/state",
            None,
            self.bearer.as_deref(),
            self.limits,
        )?;
        serde_json::from_str(&body).map_err(|e| {
            ControlError::Uncertain(format!("capture /state body was not valid JSON: {e}"))
        })
    }
}

impl CaptureControl for CaptureHttp {
    fn rf_state(&mut self) -> Result<RfState, ControlError> {
        let state = self.state_json()?;
        Ok(RfState {
            rf_freq_hz: hz_u64(state.rf_freq_hz, "rf_freq_hz")?,
            sample_rate_hz: hz_u64(state.sample_rate_hz, "sample_rate_hz")?,
        })
    }

    fn apply_rf(&mut self, target: RfState) -> Result<RfState, ControlError> {
        let payload = format!(
            "{{\"rf_freq_hz\":{},\"sample_rate_hz\":{}}}",
            target.rf_freq_hz, target.sample_rate_hz
        );
        let body = request(
            self.addr,
            "POST",
            "/config",
            Some(&payload),
            self.bearer.as_deref(),
            self.limits,
        )?;
        let response: CaptureConfigJson = serde_json::from_str(&body).map_err(|e| {
            ControlError::Uncertain(format!(
                "capture /config body was not valid JSON, so the RF outcome is unknown: {e}"
            ))
        })?;
        if !response.ok {
            return Err(ControlError::Rejected(
                response
                    .error
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| "capture refused the RF change".to_string()),
            ));
        }
        Ok(RfState {
            rf_freq_hz: hz_u64(response.rf_freq_hz, "rf_freq_hz")?,
            sample_rate_hz: hz_u64(response.sample_rate_hz, "sample_rate_hz")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
        thread,
    };

    /// Serve exactly one canned response, then close.
    fn stub_server(response: &'static str) -> (SocketAddr, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut captured = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                captured.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            // Read the body the client announced, if any.
            let length = captured
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if length > 0 {
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                captured.push_str(&String::from_utf8_lossy(&body));
            }
            let mut stream = stream;
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            captured
        });
        (addr, handle)
    }

    fn limits() -> HttpLimits {
        HttpLimits {
            connect: Duration::from_millis(500),
            read: Duration::from_millis(500),
        }
    }

    #[test]
    fn a_refused_connection_is_reported_as_not_sent() {
        // Bind and drop, so nothing is listening on the port.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let mut client = ChannelizerHttp::new(addr, None, limits());
        match client.apply_channel(-5_000_000, 12_500) {
            Err(ControlError::NotSent(detail)) => assert!(detail.contains("not sent")),
            other => panic!("expected NotSent, got {other:?}"),
        }
    }

    #[test]
    fn an_applied_retune_returns_the_service_readback() {
        let (addr, handle) = stub_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
             {\"ok\":true,\"offset_hz\":-5000000,\"output_rate_hz\":12500,\"settling_samples\":64,\"error\":null}",
        );
        let mut client = ChannelizerHttp::new(addr, None, limits());
        let applied = client.apply_channel(-5_000_000, 12_500).expect("applied");

        assert_eq!(applied.offset_hz, -5_000_000);
        assert_eq!(applied.output_rate_hz, 12_500);
        assert_eq!(applied.settling_samples, 64);
        let request = handle.join().unwrap();
        // Integer hertz on the wire, never a float literal.
        assert!(
            request.contains("{\"offset_hz\":-5000000,\"output_rate_hz\":12500}"),
            "request body was not integer hertz: {request}"
        );
    }

    #[test]
    fn a_service_refusal_is_rejected_not_uncertain() {
        let (addr, handle) = stub_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
             {\"ok\":false,\"offset_hz\":0,\"output_rate_hz\":50000,\"settling_samples\":64,\
             \"error\":\"output_rate_hz must divide input_rate_hz exactly\"}",
        );
        let mut client = ChannelizerHttp::new(addr, None, limits());
        match client.apply_channel(0, 12_500) {
            Err(ControlError::Rejected(detail)) => {
                assert!(detail.contains("must divide"), "detail was {detail}")
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn an_empty_response_is_uncertain_not_applied() {
        let (addr, handle) = stub_server("");
        let mut client = ChannelizerHttp::new(addr, None, limits());
        match client.apply_channel(0, 12_500) {
            Err(ControlError::Uncertain(detail)) => {
                assert!(detail.contains("without a response"), "detail was {detail}")
            }
            other => panic!("expected Uncertain, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_fractional_readback_is_uncertain_rather_than_rounded() {
        let (addr, handle) = stub_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
             {\"ok\":true,\"offset_hz\":-4999999.5,\"output_rate_hz\":12500,\"settling_samples\":64}",
        );
        let mut client = ChannelizerHttp::new(addr, None, limits());
        match client.apply_channel(-5_000_000, 12_500) {
            Err(ControlError::Uncertain(detail)) => assert!(
                detail.contains("not a whole number of hertz"),
                "detail was {detail}"
            ),
            other => panic!("expected Uncertain, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn state_and_observation_are_read_from_one_endpoint() {
        let (addr, handle) = stub_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
             {\"offset_hz\":-1250000,\"output_rate_hz\":12500,\"passband_hz\":5000,\
             \"input_rate_hz\":10000000,\"decimation\":800,\"settling_samples\":64,\
             \"packets_received\":42,\"packets_dropped\":0,\"seq_gaps\":3,\
             \"output_power_dbfs\":-38.5,\"peak_offset_hz\":12.5}",
        );
        let mut client = ChannelizerHttp::new(addr, None, limits());
        let state = client.channel_state().expect("state");
        assert_eq!(
            state,
            ChannelState {
                offset_hz: -1_250_000,
                output_rate_hz: 12_500
            }
        );
        handle.join().unwrap();

        let (addr, handle) = stub_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
             {\"offset_hz\":-1250000,\"output_rate_hz\":12500,\"packets_received\":42,\
             \"seq_gaps\":3,\"output_power_dbfs\":-38.5,\"peak_offset_hz\":12.5}",
        );
        let mut client = ChannelizerHttp::new(addr, None, limits());
        let observation = client.observe().expect("observation");
        assert_eq!(observation.packets_received, 42);
        assert_eq!(observation.seq_gaps, 3);
        assert_eq!(observation.output_power_dbfs, Some(-38.5));
        handle.join().unwrap();
    }

    #[test]
    fn the_capture_client_sends_its_bearer_token_and_reads_back_integer_hertz() {
        let (addr, handle) = stub_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
             {\"ok\":true,\"rf_freq_hz\":145000000,\"sample_rate_hz\":10000000,\
             \"bandwidth_hz\":10000000,\"gain_db\":20.0,\"error\":null}",
        );
        let mut client = CaptureHttp::new(addr, Some("s3cret".into()), limits());
        let applied = client
            .apply_rf(RfState {
                rf_freq_hz: 145_000_000,
                sample_rate_hz: 10_000_000,
            })
            .expect("applied");

        assert_eq!(applied.sample_rate_hz, 10_000_000);
        let request = handle.join().unwrap();
        assert!(
            request.contains("Authorization: Bearer s3cret"),
            "token missing from request: {request}"
        );
        assert!(
            request.contains("{\"rf_freq_hz\":145000000,\"sample_rate_hz\":10000000}"),
            "request body was not integer hertz: {request}"
        );
    }
}
