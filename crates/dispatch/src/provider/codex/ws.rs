//! [`WsAppServer`] — the real tungstenite-based sync driver for
//! [`AppServerRpc`] (codex-p2-spec sections 3.1, 6.2, 6.3).
//!
//! Single-threaded blocking, one in-flight request at a time. Each verb sends
//! one frame, then reads frames until the response whose `id` matches the
//! request id arrives. Any frame read while waiting that is NOT that response —
//! a server NOTIFICATION (`method`+`params`, no id), or a response for a
//! different id — is buffered: notifications go into a FIFO queue served by
//! [`AppServerRpc::next_notification`]; a stray non-matching response is
//! dropped (the protocol is one-in-flight, so this should not occur, but we
//! tolerate it rather than wedge).
//!
//! Wire law (see [`super::rpc`]): frames carry NO `jsonrpc` field; we classify
//! by key presence. Read timeouts are honored via `set_read_timeout` on the
//! underlying [`std::net::TcpStream`] (plaintext `ws://` only — NO TLS feature
//! on the tungstenite dep, codex-p2-spec section 6.3).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use super::rpc::{
    AppServerRpc, ClientInfo, InitializeResult, Notification, RpcError, ServerError, SteerOutcome,
    ThreadStartResult, TurnResult, INVALID_REQUEST_CODE,
};

/// The default per-request read deadline. A verb that does not see its response
/// within this window returns [`RpcError::Timeout`]. The send/wait verbs (W6)
/// raise it via [`WsAppServer::set_request_timeout`] for long turns.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The socket read timeout — the granularity at which a blocked `read()` wakes
/// so the loop can re-check the (possibly longer) request/notification deadline.
/// Capped to the caller's connect timeout so a short connect timeout still
/// yields snappy deadline checks.
const READ_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The real ws driver. Owns the socket, the request-id counter, and the
/// notification FIFO.
///
/// W3 interior-mutability refactor (codex-p2-spec section 6.2): the
/// [`AppServerRpc`] methods are `&self` so the driver is consumable through a
/// shared `&dyn AppServerRpc`. The mutable state (socket + id counter + FIFO)
/// lives behind a [`RefCell`]; `request_timeout` is a [`Cell`] (Copy). The driver
/// is single-threaded / `!Sync` by design (`RefCell`/`Cell` are not `Sync`): one
/// in-flight request at a time, no concurrent borrows. A second simultaneous
/// borrow would `RefCell::borrow_mut`-panic, which the one-in-flight protocol
/// makes impossible.
pub struct WsAppServer {
    sock: RefCell<WebSocket<MaybeTlsStream<TcpStream>>>,
    next_id: Cell<u64>,
    /// Notifications read off the wire while correlating a response, served in
    /// arrival order by [`AppServerRpc::next_notification`].
    pending: RefCell<VecDeque<Notification>>,
    /// Per-request read deadline.
    request_timeout: Cell<Duration>,
}

impl WsAppServer {
    /// Connect to `url` (a `ws://127.0.0.1:<port>` endpoint). `timeout` bounds
    /// the socket-level read granularity (capped at [`READ_POLL_INTERVAL`]) AND
    /// is the initial per-request deadline floor — `connect` sets the request
    /// deadline to the larger of `timeout` and [`DEFAULT_REQUEST_TIMEOUT`] so a
    /// short connect timeout does not starve a legitimate response. The
    /// handshake itself uses tungstenite's blocking `connect`.
    pub fn connect(url: &str, timeout: Duration) -> Result<WsAppServer, RpcError> {
        let (sock, _resp) =
            tungstenite::connect(url).map_err(|e| RpcError::Transport(format!("connect: {e}")))?;
        let request_timeout = timeout.max(DEFAULT_REQUEST_TIMEOUT);
        let me = WsAppServer {
            sock: RefCell::new(sock),
            next_id: Cell::new(0),
            pending: RefCell::new(VecDeque::new()),
            request_timeout: Cell::new(request_timeout),
        };
        // The socket read timeout is the poll granularity, NOT the request
        // deadline — bounded so read() wakes often enough to re-check deadlines.
        me.apply_read_timeout(timeout.min(READ_POLL_INTERVAL))?;
        Ok(me)
    }

    /// Apply a read timeout to the underlying TcpStream (plaintext only).
    fn apply_read_timeout(&self, timeout: Duration) -> Result<(), RpcError> {
        match self.sock.borrow_mut().get_mut() {
            MaybeTlsStream::Plain(tcp) => tcp
                .set_read_timeout(Some(timeout))
                .map_err(|e| RpcError::Transport(format!("set_read_timeout: {e}"))),
            // ws:// only (no TLS feature on the dep) — a TLS stream here is a
            // build-level impossibility, but degrade rather than panic.
            _ => Err(RpcError::Transport(
                "unexpected TLS stream on a ws:// connection".into(),
            )),
        }
    }

    /// Override the per-request read deadline (the verbs use this floor; W6 may
    /// raise it for long turns). `&self` (Cell-backed) so a verb holding the
    /// transport as `&dyn AppServerRpc`'s concrete type can raise it.
    pub fn set_request_timeout(&self, timeout: Duration) {
        self.request_timeout.set(timeout);
    }

    /// Mint the next request id.
    fn mint_id(&self) -> u64 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        id
    }

    /// Send a NOTIFICATION (no id, no response expected).
    fn send_notification(&self, method: &str, params: Option<Value>) -> Result<(), RpcError> {
        let mut frame = json!({ "method": method });
        if let Some(p) = params {
            frame["params"] = p;
        }
        self.send_value(&frame)
    }

    /// Send a request `{"id",method,params}` and correlate the response by id.
    /// Returns the response's `result` value, or a typed error for an `error`
    /// frame / close / timeout.
    fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.mint_id();
        let frame = json!({ "id": id, "method": method, "params": params });
        self.send_value(&frame)?;
        self.read_response(id)
    }

    /// Serialize + write one text frame.
    fn send_value(&self, frame: &Value) -> Result<(), RpcError> {
        let text = serde_json::to_string(frame)
            .map_err(|e| RpcError::Transport(format!("serialize: {e}")))?;
        self.sock
            .borrow_mut()
            .send(Message::Text(text.into()))
            .map_err(map_ws_err)
    }

    /// Read frames until the response with `want_id` arrives, buffering
    /// notifications and honoring the read deadline.
    fn read_response(&self, want_id: u64) -> Result<Value, RpcError> {
        let deadline = Instant::now() + self.request_timeout.get();
        loop {
            if Instant::now() >= deadline {
                return Err(RpcError::Timeout);
            }
            let msg = match self.sock.borrow_mut().read() {
                Ok(m) => m,
                // The socket read timeout is POLL GRANULARITY (200ms), not the
                // request deadline: a quiet socket between polls is normal while
                // the server works — keep polling until the deadline at the loop
                // top expires. (Lead-review fix: returning map_read_err here made
                // any >200ms-quiet response a false Timeout — the 30s request
                // deadline was unreachable. Regression test:
                // codex_ws.rs::response_slower_than_poll_interval_still_arrives.)
                Err(tungstenite::Error::Io(io_err))
                    if io_err.kind() == io::ErrorKind::WouldBlock
                        || io_err.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => return Err(map_read_err(e)),
            };
            let text = match msg {
                Message::Text(t) => t.as_str().to_owned(),
                // Binary frames are not part of the protocol; tolerate by
                // continuing (a server that only ever sends text).
                Message::Binary(_) => continue,
                // tungstenite auto-replies to Ping with Pong; just continue.
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(_) => return Err(RpcError::Closed),
            };
            let frame: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    return Err(RpcError::Transport(format!(
                        "non-JSON frame from server: {e}"
                    )))
                }
            };
            match classify(&frame) {
                Frame::Response { id, result } if id == want_id => return Ok(result),
                Frame::Error { id, error } if id == want_id => {
                    return Err(RpcError::Protocol(error))
                }
                // A response/error for some other id — one-in-flight means this
                // should not happen; drop it rather than wedge.
                Frame::Response { .. } | Frame::Error { .. } => continue,
                Frame::Notification(n) => self.pending.borrow_mut().push_back(n),
                Frame::Unknown => continue,
            }
        }
    }
}

/// A classified inbound frame (wire law: classify by key presence, NO jsonrpc).
enum Frame {
    Response { id: u64, result: Value },
    Error { id: u64, error: ServerError },
    Notification(Notification),
    Unknown,
}

/// Classify an inbound frame by which keys are present.
///   - `{"id":N,"result":..}`            → Response
///   - `{"error":{..},"id":N}`           → Error
///   - `{"method":..,"params":..}` no id → Notification
fn classify(frame: &Value) -> Frame {
    let obj = match frame.as_object() {
        Some(o) => o,
        None => return Frame::Unknown,
    };
    if let Some(err) = obj.get("error") {
        if let (Some(id), Ok(error)) = (
            obj.get("id").and_then(Value::as_u64),
            serde_json::from_value::<ServerError>(err.clone()),
        ) {
            return Frame::Error { id, error };
        }
    }
    if obj.contains_key("result") {
        if let Some(id) = obj.get("id").and_then(Value::as_u64) {
            let result = obj.get("result").cloned().unwrap_or(Value::Null);
            return Frame::Response { id, result };
        }
    }
    if let Some(method) = obj.get("method").and_then(Value::as_str) {
        // A frame with both method and id is a server-initiated REQUEST (e.g.
        // requestApproval) — out of W2 scope; we treat it as a notification so
        // it surfaces and is not silently lost (params kept raw).
        return Frame::Notification(Notification {
            method: method.to_string(),
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        });
    }
    Frame::Unknown
}

/// Map a tungstenite send/connect error to a typed [`RpcError`].
fn map_ws_err(e: tungstenite::Error) -> RpcError {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            RpcError::Closed
        }
        other => RpcError::Transport(other.to_string()),
    }
}

/// Map a tungstenite READ error (close vs transport). NOTE: WouldBlock/TimedOut
/// poll wakeups are handled INLINE by both read loops (continue-until-deadline);
/// [`RpcError::Timeout`] is produced only by an expired request deadline, never
/// directly from a socket poll.
fn map_read_err(e: tungstenite::Error) -> RpcError {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            RpcError::Closed
        }
        other => RpcError::Transport(other.to_string()),
    }
}

impl AppServerRpc for WsAppServer {
    fn initialize(&self, client: &ClientInfo) -> Result<InitializeResult, RpcError> {
        let params = json!({ "clientInfo": client });
        let result = self.request("initialize", params)?;
        serde_json::from_value(result)
            .map_err(|e| RpcError::Transport(format!("initialize result parse: {e}")))
    }

    fn initialized(&self) -> Result<(), RpcError> {
        self.send_notification("initialized", None)
    }

    fn thread_start(
        &self,
        cwd: &str,
        approval_policy: &str,
        sandbox: &str,
    ) -> Result<String, RpcError> {
        let params = json!({
            "cwd": cwd,
            "approvalPolicy": approval_policy,
            "sandbox": sandbox,
        });
        let result = self.request("thread/start", params)?;
        let parsed: ThreadStartResult = serde_json::from_value(result)
            .map_err(|e| RpcError::Transport(format!("thread/start result parse: {e}")))?;
        if parsed.thread.id.is_empty() {
            return Err(RpcError::Transport(
                "thread/start result had no thread id".into(),
            ));
        }
        Ok(parsed.thread.id)
    }

    fn thread_resume(&self, thread_id: &str) -> Result<(), RpcError> {
        let params = json!({ "threadId": thread_id });
        self.request("thread/resume", params).map(|_| ())
    }

    fn turn_start(&self, thread_id: &str, text: &str) -> Result<String, RpcError> {
        let params = json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": text }],
        });
        let result = self.request("turn/start", params)?;
        let parsed: TurnResult = serde_json::from_value(result)
            .map_err(|e| RpcError::Transport(format!("turn/start result parse: {e}")))?;
        parsed
            .turn_id()
            .map(str::to_owned)
            .ok_or_else(|| RpcError::Transport("turn/start result had no turn id".into()))
    }

    fn turn_steer(
        &self,
        thread_id: &str,
        expected_turn_id: &str,
        text: &str,
    ) -> Result<SteerOutcome, RpcError> {
        let params = json!({
            "threadId": thread_id,
            "expectedTurnId": expected_turn_id,
            "input": [{ "type": "text", "text": text }],
        });
        match self.request("turn/steer", params) {
            Ok(result) => {
                let parsed: TurnResult = serde_json::from_value(result)
                    .map_err(|e| RpcError::Transport(format!("turn/steer result parse: {e}")))?;
                let id = parsed.turn_id().map(str::to_owned).ok_or_else(|| {
                    RpcError::Transport("turn/steer result had no turn id".into())
                })?;
                Ok(SteerOutcome::Steered(id))
            }
            // The stale-`expectedTurnId` fence: a Protocol error with the
            // invalid-request code is the typed StalePrecondition, NOT a bubbled
            // error (the W6 send ladder falls back to turn/start on it).
            Err(RpcError::Protocol(e)) if e.code == INVALID_REQUEST_CODE => {
                Ok(SteerOutcome::StaleTurn(e))
            }
            Err(other) => Err(other),
        }
    }

    fn turn_interrupt(&self, thread_id: &str, turn_id: &str) -> Result<(), RpcError> {
        let params = json!({ "threadId": thread_id, "turnId": turn_id });
        self.request("turn/interrupt", params).map(|_| ())
    }

    fn next_notification(&self, timeout: Duration) -> Result<Option<Notification>, RpcError> {
        if let Some(n) = self.pending.borrow_mut().pop_front() {
            return Ok(Some(n));
        }
        // Nothing buffered: read frames up to the deadline, buffering any
        // response/error (one-in-flight protocol — none expected here) and
        // returning the first notification.
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let msg = match self.sock.borrow_mut().read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(io_err))
                    if io_err.kind() == io::ErrorKind::WouldBlock
                        || io_err.kind() == io::ErrorKind::TimedOut =>
                {
                    // Socket read timeout fired before the caller deadline; a
                    // quiet socket is not an error — keep polling until the
                    // caller's own deadline.
                    continue;
                }
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed) => return Err(RpcError::Closed),
                Err(other) => return Err(RpcError::Transport(other.to_string())),
            };
            let text = match msg {
                Message::Text(t) => t.as_str().to_owned(),
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                    continue
                }
                Message::Close(_) => return Err(RpcError::Closed),
            };
            let frame: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match classify(&frame) {
                Frame::Notification(n) => return Ok(Some(n)),
                // A stray response/error with no in-flight request — drop.
                _ => continue,
            }
        }
    }

    fn close(&self) -> Result<(), RpcError> {
        // Best-effort Close frame; a server that already closed yields
        // AlreadyClosed/ConnectionClosed which we treat as a clean close.
        match self.sock.borrow_mut().close(None) {
            Ok(()) => Ok(()),
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                Ok(())
            }
            Err(e) => Err(RpcError::Transport(e.to_string())),
        }
    }
}
