//! cc-relay HTTP adapter (spec §4.2): the localhost-only HTTP/1.1 transport
//! behind [`crate::relay::RelayContract`].
//!
//! Hand-rolled over `std::net::TcpStream` — NO new dependencies (workspace
//! posture). The relay server is the cc-relay MCP HTTP endpoint on
//! `127.0.0.1:<port>`; this client ports the three `fetch()` calls the TS makes:
//!
//! - `POST /message` JSON `{text, from_session}` → `message_id` (send.ts:413-426).
//! - `GET /replies/<id>` long-poll → `{text}` | `{error}` (send.ts:444-460).
//! - `GET /health` → [`RelayHealth`] (the port-scan probe, session.ts:189-204).
//!
//! ## Read-timeout policy is PER-ENDPOINT (spec W4 — load-bearing)
//!
//! Every call uses `TcpStream::connect_timeout` (a short, fixed connect window)
//! so a dead port can NEVER hang. The READ timeout differs by endpoint:
//!
//! - `health` / `send_message`: a SHORT read timeout (seconds). These are
//!   fast request/response round-trips; a short read timeout bounds them.
//! - `fetch_reply`: a LONG-POLL — its read timeout is the caller's FULL
//!   `timeout_ms` budget (TS gives `/replies` the whole 120s AbortController
//!   window, send.ts:444-446). A uniform short read timeout would abort the
//!   poll mid-flight and burn the verb's 3 retries on a slow-but-healthy reply.
//!   The no-hang guarantee comes from the connect timeout + the short
//!   health/send timeouts, NEVER from truncating the long-poll.
//!
//! Response parsing handles both `Content-Length` and `Transfer-Encoding:
//! chunked` (the server is Bun/Node — chunked is realistic). `Connection: close`
//! is sent on every request to keep framing simple (read to EOF when neither
//! length signal is present).

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

use crate::model::RelayHealth;
use crate::relay::{RelayContract, RelayError, RelayReply};

/// Short read timeout for `health` / `send_message` (spec W4). Generous enough
/// for a healthy localhost round-trip, short enough that a hung server never
/// blocks the caller for long.
const SHORT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum accepted response body (spec W2 — hostile-counterpart belt). Relay
/// bodies are tiny JSON (`{message_id}`, `{text}`, the `/health` record), so a
/// 1 MiB ceiling is orders of magnitude above any legitimate reply. A response
/// that would exceed it — a mis-framed or hostile counterpart streaming without
/// end — is rejected as `BadResponse` rather than accumulated unbounded into
/// memory. Enforced on every body path (chunked / content-length / read-to-EOF).
const MAX_RELAY_BODY: usize = 1024 * 1024;

/// Connect timeout for every call — the no-hang guarantee. A refused or dead
/// port fails fast (ConnectionRefused is immediate; an unroutable host hits
/// this bound).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The cc-relay HTTP transport. Stateless: every call opens a fresh connection
/// (TS `fetch` does the same; `Connection: close` keeps framing trivial).
#[derive(Debug, Clone, Copy, Default)]
pub struct CcRelay;

impl CcRelay {
    pub fn new() -> Self {
        CcRelay
    }
}

impl RelayContract for CcRelay {
    fn send_message(
        &self,
        port: u16,
        text: &str,
        from_session: &str,
    ) -> Result<String, RelayError> {
        // JSON body via serde_json (escaping handled correctly).
        let body = serde_json::json!({ "text": text, "from_session": from_session }).to_string();
        let body_bytes = request(port, "POST", "/message", Some(&body), SHORT_READ_TIMEOUT)?;
        let json = parse_json(&body_bytes)?;
        // TS: `messageId = data.message_id` (send.ts:425). A missing/non-string
        // message_id is a malformed response.
        json.get("message_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or(RelayError::BadResponse)
    }

    fn fetch_reply(
        &self,
        port: u16,
        message_id: &str,
        timeout_ms: u64,
    ) -> Result<RelayReply, RelayError> {
        // ADD-8 N1: the message_id is SERVER-SUPPLIED and lands in the request
        // line — reject (never silently rewrite) ids carrying CR/LF/space so a
        // malformed/hostile id cannot inject headers or split the request.
        if message_id
            .chars()
            .any(|c| c == '\r' || c == '\n' || c == ' ')
        {
            return Err(RelayError::BadResponse);
        }
        // LONG-POLL: the read timeout IS the caller's full budget (W4). The
        // server holds the connection open until the reply resolves or the
        // budget elapses.
        let read_timeout = Duration::from_millis(timeout_ms.max(1));
        let path = format!("/replies/{message_id}");
        let body_bytes = request(port, "GET", &path, None, read_timeout)?;
        let json = parse_json(&body_bytes)?;
        // The relay returns `{text}` or `{error}` (send.ts:455-459). Carry both
        // as Options; the verb decides which path to take.
        let text = json
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let error = json
            .get("error")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(RelayReply { text, error })
    }

    fn health(&self, port: u16, timeout_ms: u64) -> Result<RelayHealth, RelayError> {
        // SHORT read timeout — the probe scans many ports and must not stall on
        // a slow one. The caller (the port-scan probe) passes a short budget;
        // we floor it so a 0 never disables the read deadline.
        let read_timeout = Duration::from_millis(timeout_ms.max(1));
        let body_bytes = request(port, "GET", "/health", None, read_timeout)?;
        let json = parse_json(&body_bytes)?;
        // TS reads the body AS a RelayHealth (session.ts:200-203) and keeps it
        // only when `data.sessionId` is present. A missing sessionId → not a
        // usable relay → BadResponse (the probe treats it as "no relay here").
        let session_id = json
            .get("sessionId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or(RelayError::BadResponse)?
            .to_string();
        let scan_port = json
            .get("port")
            .and_then(|v| v.as_u64())
            .map(|p| p as u16)
            .unwrap_or(port);
        let pid = json.get("pid").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let status = json
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("ok")
            .to_string();
        Ok(RelayHealth {
            session_id,
            port: scan_port,
            pid,
            status,
        })
    }
}

/// The production [`RelayProbe`](crate::effects::RelayProbe): the HTTP
/// `/health` port-scan FALLBACK (`scanRelayPorts`, session.ts:185-212), used by
/// the join when no relay sidecar files exist. Ports the TS scan EXACTLY:
///
/// - probe ports `8900..9000` (TS `for (let port = 8900; port < 9000; port++)`,
///   session.ts:189),
/// - SHORT per-port timeout (TS aborts each `/health` fetch after 1500ms,
///   session.ts:191-192),
/// - keep a port only when `/health` returns a record WITH a `sessionId`
///   (session.ts:200-203 — `health()` already enforces this, returning
///   `BadResponse` otherwise).
///
/// TS scans all 100 ports concurrently (`Promise.all`); we scan sequentially.
/// The short per-port timeout bounds the worst case, and a missing sidecar set
/// (the only time this runs) is rare. A thread-pool fan-out would add no new
/// behavior, only deps/complexity.
pub struct HttpRelayProbe {
    client: CcRelay,
}

impl Default for HttpRelayProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpRelayProbe {
    pub fn new() -> Self {
        HttpRelayProbe {
            client: CcRelay::new(),
        }
    }
}

/// Per-port `/health` timeout for the scan (TS `setTimeout(... 1500)`,
/// session.ts:191).
const SCAN_HEALTH_TIMEOUT_MS: u64 = 1500;

/// The TS scan range: `8900..9000` (session.ts:189).
const SCAN_PORT_START: u16 = 8900;
const SCAN_PORT_END: u16 = 9000;

impl crate::effects::RelayProbe for HttpRelayProbe {
    fn scan(&self) -> Vec<RelayHealth> {
        let mut relays = Vec::new();
        for port in SCAN_PORT_START..SCAN_PORT_END {
            // A port with nothing listening fails the connect fast
            // (ConnectionFailed); a healthy relay returns its record. Any error
            // class → skip (TS per-port `catch {}`).
            if let Ok(h) = self.client.health(port, SCAN_HEALTH_TIMEOUT_MS) {
                relays.push(h);
            }
        }
        relays
    }
}

/// Issue one HTTP/1.1 request to `127.0.0.1:<port>` and return the decoded
/// response BODY bytes. `read_timeout` is the per-endpoint read deadline (W4).
///
/// Maps low-level failures to [`RelayError`] classes: connect/connection errors
/// → `ConnectionFailed`; a read timeout (`WouldBlock`/`TimedOut`) → `Timeout`;
/// malformed framing → `BadResponse`; non-2xx status → `ServerError`.
fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    read_timeout: Duration,
) -> Result<Vec<u8>, RelayError> {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let mut stream = TcpStream::connect_timeout(&addr.into(), CONNECT_TIMEOUT)
        .map_err(|_| RelayError::ConnectionFailed)?;
    // WALL-CLOCK DEADLINE (spec W2): `read_timeout` is the caller's TOTAL budget,
    // not a per-read window. Capture the absolute deadline at request start; the
    // read loops re-arm `set_read_timeout` with the REMAINING budget before each
    // read (`read_chunked` / `read_with_length` / `read_to_eof` + the header
    // read), so a slow-drip counterpart whose individual reads each beat the
    // window can still never exceed the total budget — when it drains, the next
    // read arms a 0-remaining timeout and surfaces `Timeout`. A per-stream
    // `set_read_timeout` alone would NOT bound total elapsed (red-team M3).
    let deadline = Instant::now() + read_timeout;
    stream
        .set_write_timeout(Some(SHORT_READ_TIMEOUT))
        .map_err(|_| RelayError::ConnectionFailed)?;

    // Build the request. Connection: close so the body framing can fall back to
    // read-to-EOF if no Content-Length / chunked signal is present.
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(b) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }

    stream
        .write_all(req.as_bytes())
        .map_err(|_| RelayError::ConnectionFailed)?;
    stream.flush().map_err(|_| RelayError::ConnectionFailed)?;

    read_response(&mut stream, deadline)
}

/// Re-arm `stream`'s read timeout to the budget REMAINING until `deadline`, then
/// run one `stream.read(buf)`. The single point where the wall-clock deadline is
/// enforced: if the budget is already drained we arm a 1ns timeout (a 0 disables
/// the deadline on some platforms), so the pending read returns `WouldBlock`/
/// `TimedOut` → `Timeout`. Any read error is classified by [`read_err`].
fn timed_read(
    stream: &mut TcpStream,
    deadline: Instant,
    buf: &mut [u8],
) -> Result<usize, RelayError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .unwrap_or(Duration::from_nanos(1));
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|_| RelayError::ConnectionFailed)?;
    stream.read(buf).map_err(read_err)
}

/// Read + parse an HTTP/1.1 response off `stream`. Handles `Content-Length` and
/// `Transfer-Encoding: chunked`; falls back to read-to-EOF (Connection: close).
/// Returns the decoded body bytes; a non-2xx status → `ServerError`.
fn read_response(stream: &mut TcpStream, deadline: Instant) -> Result<Vec<u8>, RelayError> {
    // Read until we have the full header block (terminated by CRLFCRLF). We may
    // overshoot into the body — keep the leftover. The header read is bounded by
    // the same wall-clock deadline as the body (slow headers burn the budget too).
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        // Cap header accumulation by the same body ceiling — a headers-forever
        // counterpart must not balloon memory before the CRLFCRLF ever arrives.
        if buf.len() > MAX_RELAY_BODY {
            return Err(RelayError::BadResponse);
        }
        let mut chunk = [0u8; 1024];
        match timed_read(stream, deadline, &mut chunk)? {
            0 => return Err(RelayError::BadResponse), // EOF before headers done
            n => buf.extend_from_slice(&chunk[..n]),
        }
    };

    let header_bytes = &buf[..header_end];
    let header_text = String::from_utf8_lossy(header_bytes);
    let mut lines = header_text.split("\r\n");

    // Status line: `HTTP/1.1 <code> <reason>`.
    let status_line = lines.next().ok_or(RelayError::BadResponse)?;
    let status = parse_status_code(status_line)?;

    // Headers (case-insensitive names).
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue; // permissive: skip malformed header lines
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => content_length = value.parse::<usize>().ok(),
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                chunked = true;
            }
            _ => {}
        }
    }

    // Body starts after the CRLFCRLF; we may already hold some of it.
    let mut body_buf = buf[header_end + 4..].to_vec();

    let body = if chunked {
        read_chunked(stream, &mut body_buf, deadline)?
    } else if let Some(len) = content_length {
        read_with_length(stream, body_buf, len, deadline)?
    } else {
        // Connection: close → read to EOF.
        read_to_eof(stream, body_buf, deadline)?
    };

    if !(200..300).contains(&status) {
        return Err(RelayError::ServerError(format!(
            "HTTP {status}: {}",
            String::from_utf8_lossy(&body).trim()
        )));
    }

    Ok(body)
}

/// Read until `body` has at least `len` bytes; truncate to exactly `len`.
fn read_with_length(
    stream: &mut TcpStream,
    mut body: Vec<u8>,
    len: usize,
    deadline: Instant,
) -> Result<Vec<u8>, RelayError> {
    // BODY CAP (W2): a declared Content-Length over the ceiling is rejected up
    // front — never read a body that announces itself as oversized.
    if len > MAX_RELAY_BODY {
        return Err(RelayError::BadResponse);
    }
    while body.len() < len {
        let mut chunk = [0u8; 4096];
        match timed_read(stream, deadline, &mut chunk)? {
            0 => break, // EOF early — return what we have
            n => body.extend_from_slice(&chunk[..n]),
        }
    }
    body.truncate(len);
    Ok(body)
}

/// Read to EOF (Connection: close framing).
fn read_to_eof(
    stream: &mut TcpStream,
    mut body: Vec<u8>,
    deadline: Instant,
) -> Result<Vec<u8>, RelayError> {
    loop {
        // BODY CAP (W2): a Connection-close counterpart that never sends EOF (or
        // floods bytes) must not accumulate past the ceiling.
        if body.len() > MAX_RELAY_BODY {
            return Err(RelayError::BadResponse);
        }
        let mut chunk = [0u8; 4096];
        match timed_read(stream, deadline, &mut chunk)? {
            0 => break,
            n => body.extend_from_slice(&chunk[..n]),
        }
    }
    Ok(body)
}

/// Minimal `Transfer-Encoding: chunked` decode. Each chunk is a hex size line
/// (CRLF-terminated), then that many bytes (CRLF-terminated), ending at a `0`
/// size chunk. `pending` holds any body bytes already read past the headers.
fn read_chunked(
    stream: &mut TcpStream,
    pending: &mut Vec<u8>,
    deadline: Instant,
) -> Result<Vec<u8>, RelayError> {
    let mut out = Vec::new();
    let mut buf = std::mem::take(pending);
    loop {
        // Read the chunk-size line.
        let line = read_line(stream, &mut buf, deadline)?;
        // The size is hex, possibly with a `;ext` suffix — take up to the `;`.
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| RelayError::BadResponse)?;
        if size == 0 {
            // Last chunk; consume the trailing CRLF (and any trailers) but the
            // body is complete.
            break;
        }
        // BODY CAP (W2): reject once the reassembled body would exceed the
        // ceiling — a counterpart streaming chunks without end cannot grow `out`
        // unbounded. Checked against the cumulative decoded size (out + this
        // chunk), so a single huge chunk is caught before it is buffered.
        if out.len().saturating_add(size) > MAX_RELAY_BODY {
            return Err(RelayError::BadResponse);
        }
        // Ensure we have `size` + 2 (trailing CRLF) bytes in buf.
        while buf.len() < size + 2 {
            let mut chunk = [0u8; 4096];
            match timed_read(stream, deadline, &mut chunk)? {
                0 => return Err(RelayError::BadResponse), // truncated chunk
                n => buf.extend_from_slice(&chunk[..n]),
            }
        }
        out.extend_from_slice(&buf[..size]);
        // Drop the chunk data + its trailing CRLF.
        buf.drain(..size + 2);
    }
    Ok(out)
}

/// Read one CRLF-terminated line from `buf`, refilling from `stream` as needed.
/// Returns the line WITHOUT the trailing CRLF; consumes it from `buf`.
fn read_line(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
) -> Result<String, RelayError> {
    loop {
        if let Some(pos) = find_subslice(buf, b"\r\n") {
            let line: Vec<u8> = buf.drain(..pos + 2).collect();
            let line = &line[..line.len() - 2];
            return Ok(String::from_utf8_lossy(line).to_string());
        }
        // BODY CAP (W2): a chunk-size LINE that never terminates (no CRLF) must
        // not balloon `buf` — the same ceiling bounds the line scan.
        if buf.len() > MAX_RELAY_BODY {
            return Err(RelayError::BadResponse);
        }
        let mut chunk = [0u8; 1024];
        match timed_read(stream, deadline, &mut chunk)? {
            0 => return Err(RelayError::BadResponse),
            n => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Classify a read `io::Error`: a timeout (the per-endpoint deadline elapsed)
/// → `Timeout`; anything else → `ConnectionFailed`. On most platforms a read
/// timeout surfaces as `WouldBlock`; some surface `TimedOut`.
fn read_err(e: std::io::Error) -> RelayError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => RelayError::Timeout,
        _ => RelayError::ConnectionFailed,
    }
}

/// Parse the status code out of an HTTP status line (`HTTP/1.1 200 OK`).
fn parse_status_code(line: &str) -> Result<u16, RelayError> {
    let mut parts = line.split_whitespace();
    let _version = parts.next().ok_or(RelayError::BadResponse)?;
    let code = parts.next().ok_or(RelayError::BadResponse)?;
    code.parse::<u16>().map_err(|_| RelayError::BadResponse)
}

/// Parse a JSON body, mapping any parse failure to `BadResponse`.
fn parse_json(body: &[u8]) -> Result<serde_json::Value, RelayError> {
    serde_json::from_slice(body).map_err(|_| RelayError::BadResponse)
}

/// Find the first index of `needle` in `haystack` (small linear scan; bodies
/// here are tiny).
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_subslice_basic() {
        assert_eq!(find_subslice(b"abcdef", b"cd"), Some(2));
        assert_eq!(find_subslice(b"abc\r\n\r\ndef", b"\r\n\r\n"), Some(3));
        assert_eq!(find_subslice(b"abc", b"xyz"), None);
        assert_eq!(find_subslice(b"", b"x"), None);
    }

    #[test]
    fn parse_status_code_ok() {
        assert_eq!(parse_status_code("HTTP/1.1 200 OK").unwrap(), 200);
        assert_eq!(
            parse_status_code("HTTP/1.1 503 Service Unavailable").unwrap(),
            503
        );
        assert!(parse_status_code("garbage").is_err());
    }

    #[test]
    fn fetch_reply_rejects_crlf_or_space_in_message_id() {
        // ADD-8 N1: a server-supplied id with CR/LF/space must be rejected
        // BEFORE any request is built (header-injection guard). Port 1 is never
        // contacted: BadResponse (the early reject), NOT ConnectionFailed
        // (which a connect attempt would yield).
        let c = CcRelay::new();
        for bad in ["abc\r\nX-Inj: 1", "abc\nxyz", "abc def"] {
            assert_eq!(
                c.fetch_reply(1, bad, 100).unwrap_err(),
                RelayError::BadResponse,
                "id {bad:?} must be rejected pre-request"
            );
        }
    }

    #[test]
    fn read_err_classifies_timeout() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            read_err(Error::from(ErrorKind::WouldBlock)),
            RelayError::Timeout
        );
        assert_eq!(
            read_err(Error::from(ErrorKind::TimedOut)),
            RelayError::Timeout
        );
        assert_eq!(
            read_err(Error::from(ErrorKind::ConnectionReset)),
            RelayError::ConnectionFailed
        );
    }
}
