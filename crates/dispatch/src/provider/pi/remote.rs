//! [`PiRemote`] — the qd-VERB-side [`PiRpc`] client: it connects to a pi
//! resident's loopback front ([`super::residence::serve_pi`]) and drives it over
//! the thin `{id, m, …args}` → `{id, ok}` / `{id, e}` request/reply protocol.
//! Structural mirror of [`crate::provider::acp::wire::AcpConnection`] (and the
//! codex [`crate::provider::codex::ws::WsAppServer`] one-in-flight discipline).
//!
//! **This is the `fx.pi_rpc` at the VERB layer.** A short-lived `qd` invocation
//! resolves the resident's `endpoint` off the registry row, `connect`s, and hands
//! this object to `PiProvider::{boot_waiter, inject}` as `&dyn PiRpc`. The
//! in-daemon truth ([`super::stdio::PiStdio`]) is the OTHER `PiRpc` impl — it
//! actually owns pi; this one is the cross-process reach to it.
//!
//! **Events do NOT traverse the front.** The resident owns pi's event stream and
//! feeds it through [`super::status::PiStatusMapper`] into the registry sink
//! (item 2). So [`PiRpc::next_event`] here returns `Ok(None)` — a verb that wants
//! live turn state reads the registry row / transcript, not this client. The
//! request/reply methods (`get_state`/`prompt`/`steer`/…) are what the verb layer
//! needs (readiness + inject).

use std::cell::{Cell, RefCell};
use std::io;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use super::rpc::{PiEvent, PiRpc, PiRpcError, RpcSessionState, StreamingBehavior};

/// Default per-request read deadline (a front request is a single resident
/// round-trip; long turns are the resident's concern, not the verb's).
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Socket read poll granularity — how often a blocked read wakes to re-check the
/// request deadline.
const READ_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Parse a `ws://127.0.0.1:<port>` endpoint into a connectable socket addr.
fn parse_ws_addr(url: &str) -> Result<std::net::SocketAddr, PiRpcError> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| PiRpcError::Transport(format!("not a ws:// url: {url}")))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    host_port
        .parse()
        .map_err(|e| PiRpcError::Transport(format!("bad ws addr {host_port}: {e}")))
}

/// A single resident OBSERVATION (B1): the live streaming flag + the resident's
/// drop-immune completed-turn count, from ONE `get_state` front round-trip. The
/// ls/info gather ([`crate::join`]) renders a live pi row's status + turns off
/// this without a second call. `is_streaming` is the SAME drop-immune point-read
/// `qd wait` gates on; `turns` is the resident's busy→idle-edge count (never a raw
/// `agent_end` tally).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PiObservation {
    pub is_streaming: bool,
    pub turns: u64,
}

/// The qd-side resident client. `!Sync` by design (one [`RefCell`] socket); one
/// in-flight request at a time (the resident serves one connection serially).
pub struct PiRemote {
    sock: RefCell<WebSocket<MaybeTlsStream<TcpStream>>>,
    next_id: Cell<u64>,
    request_timeout: Cell<Duration>,
}

impl PiRemote {
    /// Connect to `url` (a resident `ws://127.0.0.1:<port>` endpoint). `timeout`
    /// bounds the TCP connect, the ws handshake I/O, and the socket read
    /// granularity, and floors the per-request deadline — the `AcpConnection`
    /// fail-fast discipline (a contended connect against a camped resident fails
    /// fast rather than hanging).
    pub fn connect(url: &str, timeout: Duration) -> Result<PiRemote, PiRpcError> {
        let addr = parse_ws_addr(url)?;
        let tcp = TcpStream::connect_timeout(&addr, timeout)
            .map_err(|e| PiRpcError::Transport(format!("connect {url}: {e}")))?;
        tcp.set_read_timeout(Some(timeout))
            .and_then(|()| tcp.set_write_timeout(Some(timeout)))
            .map_err(|e| {
                PiRpcError::Transport(format!("connect {url}: set handshake timeout: {e}"))
            })?;
        let (sock, _resp) = tungstenite::client(url, MaybeTlsStream::Plain(tcp))
            .map_err(|e| PiRpcError::Transport(format!("handshake {url}: {e}")))?;
        let me = PiRemote {
            sock: RefCell::new(sock),
            next_id: Cell::new(0),
            request_timeout: Cell::new(timeout.max(DEFAULT_REQUEST_TIMEOUT)),
        };
        me.apply_read_timeout(timeout.min(READ_POLL_INTERVAL))?;
        Ok(me)
    }

    /// Override the per-request read deadline (verbs raise it for long ops).
    pub fn set_request_timeout(&self, timeout: Duration) {
        self.request_timeout.set(timeout);
    }

    /// The resident's established session id (the `status` health probe) — the
    /// readiness poll ([`super::residence::connect_ready`]) reads this.
    pub fn status_session_id(&self) -> Result<Option<String>, PiRpcError> {
        let result = self.request("status", json!({}))?;
        Ok(result
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// B1 observability point-read: `is_streaming` + the resident's completed-turn
    /// count in ONE `get_state` round-trip (the front reply carries both). Used by
    /// the ls/info gather so a live pi row's status and turns come from a single
    /// drop-immune resident read. A missing `turns` (an older resident) degrades to
    /// 0 permissively.
    pub fn observe(&self) -> Result<PiObservation, PiRpcError> {
        let result = self.request("get_state", json!({}))?;
        Ok(PiObservation {
            is_streaming: result
                .get("is_streaming")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            turns: result.get("turns").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    fn apply_read_timeout(&self, timeout: Duration) -> Result<(), PiRpcError> {
        match self.sock.borrow_mut().get_mut() {
            MaybeTlsStream::Plain(tcp) => tcp
                .set_read_timeout(Some(timeout))
                .map_err(|e| PiRpcError::Transport(format!("set_read_timeout: {e}"))),
            _ => Err(PiRpcError::Transport(
                "unexpected TLS stream on a ws:// connection".into(),
            )),
        }
    }

    fn mint_id(&self) -> u64 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        id
    }

    /// Send `{"id",m,…args}` and correlate the `{id, ok|e}` reply by id, honoring
    /// the per-request deadline (the ws.rs poll-until-deadline pattern). An `e`
    /// frame is a [`PiRpcError::Protocol`] (the resident relayed pi's error).
    fn request(&self, method: &str, mut args: Value) -> Result<Value, PiRpcError> {
        let id = self.mint_id();
        if let Some(obj) = args.as_object_mut() {
            obj.insert("id".into(), json!(id));
            obj.insert("m".into(), json!(method));
        }
        let text = serde_json::to_string(&args)
            .map_err(|e| PiRpcError::Transport(format!("serialize: {e}")))?;
        self.sock
            .borrow_mut()
            .send(Message::Text(text.into()))
            .map_err(map_ws_err)?;
        self.read_response(id)
    }

    fn read_response(&self, want_id: u64) -> Result<Value, PiRpcError> {
        let deadline = Instant::now() + self.request_timeout.get();
        loop {
            if Instant::now() >= deadline {
                return Err(PiRpcError::Timeout);
            }
            let msg = match self.sock.borrow_mut().read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(io_err))
                    if io_err.kind() == io::ErrorKind::WouldBlock
                        || io_err.kind() == io::ErrorKind::TimedOut =>
                {
                    continue; // poll granularity; keep polling until the deadline
                }
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed) => return Err(PiRpcError::Closed),
                Err(e) => return Err(PiRpcError::Transport(e.to_string())),
            };
            let text = match msg {
                Message::Text(t) => t.as_str().to_owned(),
                Message::Close(_) => return Err(PiRpcError::Closed),
                _ => continue,
            };
            let frame: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    return Err(PiRpcError::Transport(format!(
                        "non-JSON frame from resident: {e}"
                    )))
                }
            };
            // Correlate by id; an `e` is the relayed pi error, `ok` the result.
            if frame.get("id").and_then(Value::as_u64) != Some(want_id) {
                continue; // one-in-flight: a stray frame, drop rather than wedge
            }
            if let Some(e) = frame.get("e").and_then(Value::as_str) {
                return Err(PiRpcError::Protocol(e.to_string()));
            }
            return Ok(frame.get("ok").cloned().unwrap_or(Value::Null));
        }
    }
}

impl PiRpc for PiRemote {
    fn get_state(&self) -> Result<RpcSessionState, PiRpcError> {
        let result = self.request("get_state", json!({}))?;
        Ok(RpcSessionState {
            session_id: result
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            is_streaming: result
                .get("is_streaming")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ..RpcSessionState::default()
        })
    }

    fn prompt(
        &self,
        message: &str,
        behavior: Option<StreamingBehavior>,
    ) -> Result<String, PiRpcError> {
        // The resident defaults to steer; carry the caller's choice when given.
        let mut args = json!({ "message": message });
        if let Some(b) = behavior {
            args["streamingBehavior"] = serde_json::to_value(b).unwrap_or(Value::Null);
        }
        let result = self.request("prompt", args)?;
        // The attributable turn id = the resident's minted command id.
        result
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| PiRpcError::Protocol("prompt reply had no turn_id".into()))
    }

    fn steer(&self, message: &str) -> Result<(), PiRpcError> {
        self.request("steer", json!({ "message": message })).map(|_| ())
    }

    fn follow_up(&self, message: &str) -> Result<(), PiRpcError> {
        self.request("follow_up", json!({ "message": message })).map(|_| ())
    }

    fn abort(&self) -> Result<(), PiRpcError> {
        self.request("abort", json!({})).map(|_| ())
    }

    fn switch_session(&self, session_path: &str) -> Result<(), PiRpcError> {
        self.request("switch_session", json!({ "sessionPath": session_path }))
            .map(|_| ())
    }

    fn new_session(&self, parent: Option<&str>) -> Result<(), PiRpcError> {
        let args = match parent {
            Some(p) => json!({ "parentSession": p }),
            None => json!({}),
        };
        self.request("new_session", args).map(|_| ())
    }

    /// Events do NOT traverse the front — the resident routes pi's stream into the
    /// registry sink (item 2). A verb wanting live turn state reads the registry /
    /// transcript, not this client. So a quiet `Ok(None)` is the honest answer.
    fn next_event(&self, _timeout: Duration) -> Result<Option<PiEvent>, PiRpcError> {
        Ok(None)
    }

    fn close(&self) -> Result<(), PiRpcError> {
        match self.sock.borrow_mut().close(None) {
            Ok(()) => Ok(()),
            Err(tungstenite::Error::ConnectionClosed)
            | Err(tungstenite::Error::AlreadyClosed) => Ok(()),
            Err(e) => Err(PiRpcError::Transport(e.to_string())),
        }
    }
}

/// Map a tungstenite send/connect error to a typed [`PiRpcError`].
fn map_ws_err(e: tungstenite::Error) -> PiRpcError {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            PiRpcError::Closed
        }
        other => PiRpcError::Transport(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ws_addr_ok_and_bad() {
        assert!(parse_ws_addr("ws://127.0.0.1:9000").is_ok());
        assert!(parse_ws_addr("http://127.0.0.1:9000").is_err());
        assert!(parse_ws_addr("ws://not-an-addr").is_err());
    }
}
