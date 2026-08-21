//! `provider/acp/wire.rs` — the ACP **cross-process residence transport**: a ws
//! front for a resident [`AcpHost`](super::client::AcpHost) (S3, the SERVER) + the
//! socket-backed [`AcpClient`] (S4, the CLIENT — the `AcpHost::connect` analog of
//! `provider/codex/ws.rs`'s [`WsAppServer`](crate::provider::codex::WsAppServer)).
//!
//! # Why this layer exists (the codex divergence, STEP0-RESIDENCE-SCOPE §2b)
//! Codex's app-server NATIVELY listens on `ws://`, so codex spawns it detached and a
//! later verb just `WsAppServer::connect`s. The `claude-code-acp` bridge speaks ONLY
//! stdio (ndjson pipes) — pipes cannot be re-dialed across processes. So ACP needs a
//! dispatch-side resident ADAPTER process (S1, `qd acp-daemon`) that owns the bridge's
//! stdio AND fronts it with this ws transport. The DISCIPLINE mirrors codex; S3 (a ws
//! *server* relaying an `AcpClient`) and the adapter entry are NET-NEW.
//!
//! # ★ Faithfulness keystone (rider-3) — PROVABLE by primary source
//! The `next_update` events the socket emits are the **real bridge stream relayed
//! VERBATIM**. [`serve`] calls the resident host's [`AcpClient::next_update`] and
//! serializes whatever [`AcpEvent`] it returns; there is **no synthesis path** in this
//! module — an `Update`'s `payload` is the raw bridge `update` object the SC-5 reader
//! parsed (`client.rs` builds it straight off the bus). Primary-source provable: the
//! bytes a (fake or real) bridge emits round-trip to a byte-identical `AcpEvent` at the
//! S4 client (`wire_relays_event_payload_verbatim`); the non-vacuity revert-probe is
//! `serve` synthesizing instead of relaying — see the test module.
//!
//! # Protocol (text JSON frames over ws; one-in-flight req/resp, classify-by-key)
//! Pure request/response — there are NO server-initiated frames. The "stream" is the
//! client PULLING `next_update` in a loop, each response carrying the next real event
//! in SC-5 bus order (the same pull the in-process consumer does). Reverse-requests
//! (permission/fs/terminal) are answered SERVER-SIDE inside the host's `next_update`
//! (`client.rs` `answer_reverse`) and never cross this socket.
//!
//!   request : `{"id":N,"m":<method>, …args}`
//!   ok      : `{"id":N,"ok":<result>}`
//!   err     : `{"id":N,"e":{"k":<kind>,"m":<msg>}}`
//!
//! methods: `initialize` · `new_session{cwd}` · `prompt{text,from}` · `cancel` ·
//! `next_update{timeout_ms}` · `status` (resident health: `{session_id}`).

use std::cell::{Cell, RefCell};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use super::rpc::{AcpClient, AcpError, AcpEvent, InitializeResult, SessionId, StopReason, TurnId};

/// Socket read poll granularity — how often a blocked `read()` wakes so the loop can
/// re-check deadlines / the shutdown flag (mirrors `ws.rs::READ_POLL_INTERVAL`).
const READ_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Server-side cap on a single `next_update` block, so the connection handler stays
/// responsive to teardown and one pull can't wedge the resident indefinitely. The
/// CLIENT loops its own (longer) deadline across pulls, so this is a poll bound, not a
/// turn deadline — `Ok(None)` (no event yet) is the documented quiet-stream signal.
const SERVER_NEXT_UPDATE_CAP: Duration = Duration::from_secs(2);

/// Default per-request client read deadline (mirrors `ws.rs::DEFAULT_REQUEST_TIMEOUT`).
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ===========================================================================
// AcpEvent / error wire (de)serialization — VERBATIM relay, no synthesis.
// ===========================================================================

/// Encode an [`AcpEvent`] to its wire JSON. The `Update` `payload` is relayed
/// **verbatim** (the raw bridge `update` object) — this is the faithfulness seam.
pub(crate) fn encode_event(ev: &AcpEvent) -> Value {
    match ev {
        AcpEvent::Update {
            session,
            kind,
            payload,
        } => json!({
            "t": "update",
            "session": session,
            "kind": kind,
            "payload": payload, // verbatim bridge `update` object
        }),
        AcpEvent::Terminal {
            session,
            turn,
            stop,
        } => json!({
            "t": "terminal",
            "session": session,
            "turn": turn,
            "stop": stop.as_wire(),
        }),
        AcpEvent::TerminalError {
            session,
            turn,
            message,
        } => json!({
            "t": "terminal_error",
            "session": session,
            "turn": turn,
            "message": message,
        }),
    }
}

/// Decode an [`AcpEvent`] from its wire JSON. Inverse of [`encode_event`]; a malformed
/// frame is a [`AcpError::Protocol`] (never silently dropped).
pub(crate) fn decode_event(v: &Value) -> Result<AcpEvent, AcpError> {
    let t = v
        .get("t")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::Protocol("event frame missing discriminator".into()))?;
    let session: SessionId = v
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match t {
        "update" => Ok(AcpEvent::Update {
            session,
            kind: v
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            // verbatim: whatever the bridge produced, byte-for-byte through serde
            payload: v.get("payload").cloned().unwrap_or(Value::Null),
        }),
        "terminal" => {
            let stop = v
                .get("stop")
                .and_then(Value::as_str)
                .and_then(StopReason::parse)
                .ok_or_else(|| AcpError::Protocol("terminal event had no mappable stop".into()))?;
            Ok(AcpEvent::Terminal {
                session,
                turn: turn_field(v),
                stop,
            })
        }
        "terminal_error" => Ok(AcpEvent::TerminalError {
            session,
            turn: turn_field(v),
            message: v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        other => Err(AcpError::Protocol(format!("unknown event kind {other:?}"))),
    }
}

fn turn_field(v: &Value) -> TurnId {
    v.get("turn")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Encode an [`AcpError`] to its wire form `{"k":<kind>,"m":<msg>}`.
fn encode_err(e: &AcpError) -> Value {
    let (k, m) = match e {
        AcpError::Closed => ("closed", "acp connection closed".to_string()),
        AcpError::Transport(s) => ("transport", s.clone()),
        AcpError::Protocol(s) => ("protocol", s.clone()),
        AcpError::QueueFull => ("queuefull", "acp outbound queue full".to_string()),
    };
    json!({ "k": k, "m": m })
}

/// Decode an [`AcpError`] from its wire form (inverse of [`encode_err`]).
fn decode_err(v: &Value) -> AcpError {
    let m = v.get("m").and_then(Value::as_str).unwrap_or("").to_string();
    match v.get("k").and_then(Value::as_str) {
        Some("closed") => AcpError::Closed,
        Some("queuefull") => AcpError::QueueFull,
        Some("protocol") => AcpError::Protocol(m),
        _ => AcpError::Transport(m),
    }
}

fn encode_init(r: &InitializeResult) -> Value {
    json!({
        "protocol_version": r.protocol_version,
        "agent_name": r.agent_name,
        "agent_version": r.agent_version,
        "auth_required": r.auth_required,
    })
}

fn decode_init(v: &Value) -> InitializeResult {
    InitializeResult {
        protocol_version: v
            .get("protocol_version")
            .and_then(Value::as_i64)
            .unwrap_or(1),
        agent_name: v
            .get("agent_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        agent_version: v
            .get("agent_version")
            .and_then(Value::as_str)
            .map(str::to_string),
        auth_required: v
            .get("auth_required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

// ===========================================================================
// S3 — the SERVER: a ws front relaying a resident AcpClient.
// ===========================================================================

/// The resident the [`serve`] loop fronts: an [`AcpClient`] plus the inherent
/// `session_id` health accessor (the adapter's `status` probe). `AcpHost` implements
/// this; tests use a fake to exercise the wire WITHOUT a live bridge.
pub trait AcpResident: AcpClient {
    /// The established ACP session id, if `session/new`/`session/load` has run.
    fn resident_session_id(&self) -> Option<String>;

    /// (N-idle, Item 3) Is a turn IN FLIGHT right now? The SC-1 queue's primary truth
    /// (`OutboundQueue::is_idle` — `in_flight.is_some()`). `false` = genuinely turn-idle,
    /// so a `wait` can short-circuit instead of camping to timeout; `true` while a turn
    /// runs, so a mid-turn `wait` never false-idles. Default `false` (test residents that
    /// don't model a queue are reported idle).
    fn resident_in_flight(&self) -> bool {
        false
    }

    /// (W) WEDGED-BUT-ALIVE (Item 3) — is the bridge child CONFIRMED DEAD? The [`serve`]
    /// loop polls this so a zombie adapter (bridge gone, process lingering with intact
    /// pid+cmdline) SELF-TERMINATES, making `pid_alive=false` the honest signal BOTH the
    /// resume (R-c) and ls (L) gates read — instead of a `/proc`-only lie of 'live'.
    /// Default `false` (test residents with no bridge never self-terminate).
    fn bridge_confirmed_dead(&self) -> bool {
        false
    }
}

impl AcpResident for super::client::AcpHost {
    fn resident_session_id(&self) -> Option<String> {
        self.session_id()
    }
    fn resident_in_flight(&self) -> bool {
        self.in_flight()
    }
    fn bridge_confirmed_dead(&self) -> bool {
        self.bridge_confirmed_dead()
    }
}

/// Run the ws server loop, fronting `resident`, until `shutdown` is set. Connections
/// are handled **serially** (one driver at a time): the SC-1 queue already serializes
/// turns, and the residence round-trip is a single sequential driver — so serial
/// handling is correct for the keystone and cleanly handles client-disconnect-mid-stream
/// (a dropped connection just frees the loop for the next). The resident outlives every
/// connection — that IS cross-process residence.
pub fn serve(
    resident: &dyn AcpResident,
    listener: &TcpListener,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    listener.set_nonblocking(false)?;
    // Bound accept() so we re-check `shutdown` between would-be-blocking waits.
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        // (W) WEDGED-BUT-ALIVE self-terminate: if the bridge child is CONFIRMED DEAD
        // (reaped — not a transient blip), this zombie adapter exits NONZERO so
        // `pid_alive=false` becomes the honest signal the resume/ls gates read (instead
        // of a `/proc`-only 'live' lie). Polled here between connections (idle path);
        // the shutdown check above wins first, so the normal SIGTERM/kill teardown is
        // UNAFFECTED. REVERT SEAM (W): drop this check → a bridge-killed adapter lingers
        // 'live' (the bug reappears) → the (W) repro reds.
        if resident.bridge_confirmed_dead() {
            return Err(io::Error::other(
                "bridge confirmed dead — adapter self-terminating (W: wedged-but-alive guard)",
            ));
        }
        listener.set_nonblocking(true)?;
        match listener.accept() {
            Ok((stream, _peer)) => {
                listener.set_nonblocking(false)?;
                // A failed handshake on one connection must not down the resident.
                if let Ok(ws) = tungstenite::accept(stream) {
                    handle_connection(resident, ws, shutdown);
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(READ_POLL_INTERVAL);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Serve one ws connection: read request frames, dispatch each to `resident`, write the
/// response. Returns on Close/EOF/transport error (the resident is untouched — the next
/// connection re-attaches). Sets a read timeout so a quiet client still lets us poll
/// `shutdown`.
fn handle_connection(
    resident: &dyn AcpResident,
    mut ws: WebSocket<TcpStream>,
    shutdown: &AtomicBool,
) {
    if let Ok(stream) = ws.get_ref().set_read_timeout(Some(READ_POLL_INTERVAL)) {
        let _ = stream; // best-effort; a missing timeout only costs shutdown latency
    } else {
        let _ = ws.get_ref().set_read_timeout(Some(READ_POLL_INTERVAL));
    }
    loop {
        if shutdown.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            return;
        }
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(io_err))
                if io_err.kind() == io::ErrorKind::WouldBlock
                    || io_err.kind() == io::ErrorKind::TimedOut =>
            {
                continue; // poll granularity; re-check shutdown
            }
            // Close / EOF / transport gone → done with this connection.
            Err(_) => return,
        };
        let text = match msg {
            Message::Text(t) => t.as_str().to_owned(),
            Message::Close(_) => return,
            // Out-of-protocol frames: ignore (tungstenite auto-pongs pings).
            _ => continue,
        };
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue, // a non-JSON frame is not a protocol request
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let reply = dispatch_request(resident, &req);
        let frame = match reply {
            Ok(result) => json!({ "id": id, "ok": result }),
            Err(e) => json!({ "id": id, "e": encode_err(&e) }),
        };
        if let Ok(text) = serde_json::to_string(&frame) {
            if ws.send(Message::Text(text.into())).is_err() {
                return; // peer gone
            }
        }
    }
}

/// Dispatch one decoded request to the resident. The `next_update` arm is THE
/// faithfulness seam: it returns exactly what `resident.next_update` produced, encoded
/// verbatim — no synthesis.
fn dispatch_request(resident: &dyn AcpResident, req: &Value) -> Result<Value, AcpError> {
    let method = req
        .get("m")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::Protocol("request missing method".into()))?;
    match method {
        "initialize" => resident.initialize().map(|r| encode_init(&r)),
        "new_session" => {
            let cwd = req.get("cwd").and_then(Value::as_str).unwrap_or(".");
            resident.new_session(cwd).map(|s| json!({ "session": s }))
        }
        "prompt" => {
            let text = req.get("text").and_then(Value::as_str).unwrap_or("");
            let from = req.get("from").and_then(Value::as_str).unwrap_or("");
            // The host ignores the session arg (uses its resident session); pass "".
            // Server-side dispatch (in-process to the resident) — the exactly-once
            // durable marker is a verb-layer (client-side) concern; no-op here.
            resident
                .prompt("", text, from, &|| {})
                .map(|t| json!({ "turn": t }))
        }
        "cancel" => resident.cancel("").map(|()| Value::Null),
        "next_update" => {
            let req_ms = req.get("timeout_ms").and_then(Value::as_u64).unwrap_or(0);
            let timeout = Duration::from_millis(req_ms).min(SERVER_NEXT_UPDATE_CAP);
            // VERBATIM relay of the real bridge stream — no synthesis path here.
            match resident.next_update(timeout)? {
                Some(ev) => Ok(json!({ "event": encode_event(&ev) })),
                None => Ok(json!({ "event": Value::Null })),
            }
        }
        // (N-idle) `status` additionally carries `in_flight` — a `wait` queries it to
        // short-circuit a genuinely-idle session. Additive: pre-Item-3 clients read only
        // `session_id` (the field is ignored), so the probe stays back-compatible.
        "status" => Ok(json!({
            "session_id": resident.resident_session_id(),
            "in_flight": resident.resident_in_flight(),
        })),
        other => Err(AcpError::Protocol(format!("unknown method {other:?}"))),
    }
}

// ===========================================================================
// S4 — the CLIENT: a socket-backed AcpClient (the AcpHost::connect analog).
// ===========================================================================

/// A connected ws client implementing [`AcpClient`] against a resident [`serve`]
/// endpoint — the `AcpHost::connect` analog of [`WsAppServer`](crate::provider::codex::WsAppServer).
/// `&self` + interior mutability so a shared `&dyn AcpClient` is handed into
/// [`ProviderFx::acp_client`](crate::provider::ProviderFx); single-threaded / `!Sync`
/// (one in-flight request at a time, exactly the codex ws discipline).
pub struct AcpConnection {
    sock: RefCell<WebSocket<MaybeTlsStream<TcpStream>>>,
    next_id: Cell<u64>,
    request_timeout: Cell<Duration>,
}

impl AcpConnection {
    /// Connect to `url` (a `ws://127.0.0.1:<port>` resident endpoint). `timeout` bounds
    /// the socket read granularity and floors the per-request deadline (mirrors
    /// `WsAppServer::connect`) AND — (N-conc, Item 3) — bounds the ws HANDSHAKE itself.
    ///
    /// SERIALIZE BOUND (honest, documented): the resident [`serve`] loop handles exactly
    /// ONE connection at a time (`!Sync` host, one `RefCell` — `client.rs`). While a
    /// client is camped in a long `wait` (a repeated `next_update` block), the listener
    /// does not `accept()` a second client until the first disconnects. Pre-Item-3,
    /// `tungstenite::connect` applied NO handshake deadline, so a concurrent verb's
    /// connect could STALL for the camped wait's entire lifetime (up to 120s). We now
    /// bound the TCP-connect AND the handshake I/O by `timeout`, so a contended connect
    /// fails FAST (`Transport`/timeout) instead of hanging — the caller surfaces a clean
    /// error / retries rather than wedging. True concurrency (multiple in-flight clients)
    /// would require `Arc<Mutex<HostInner>>` in the host (DECLINED for Item 3 — a bigger
    /// surface + keystone risk for a sequential-keystone workload); this LEAN bound +
    /// this note is the ratified disposition.
    pub fn connect(url: &str, timeout: Duration) -> Result<AcpConnection, AcpError> {
        // Bound the TCP connect (the SYN) ...
        let addr = parse_ws_addr(url)?;
        let tcp = TcpStream::connect_timeout(&addr, timeout)
            .map_err(|e| AcpError::Transport(format!("connect {url}: {e}")))?;
        // ... and the ws UPGRADE read/write (the part that stalls behind a camped serve
        // loop — the TCP connect itself succeeds into the accept backlog, but the upgrade
        // is not serviced until the resident calls `accept`). A read/write deadline on
        // the stream makes the handshake fail fast rather than block indefinitely.
        tcp.set_read_timeout(Some(timeout))
            .and_then(|()| tcp.set_write_timeout(Some(timeout)))
            .map_err(|e| {
                AcpError::Transport(format!("connect {url}: set handshake timeout: {e}"))
            })?;
        let (sock, _resp) = tungstenite::client(url, MaybeTlsStream::Plain(tcp))
            .map_err(|e| AcpError::Transport(format!("handshake {url}: {e}")))?;
        let me = AcpConnection {
            sock: RefCell::new(sock),
            next_id: Cell::new(0),
            request_timeout: Cell::new(timeout.max(DEFAULT_REQUEST_TIMEOUT)),
        };
        me.apply_read_timeout(timeout.min(READ_POLL_INTERVAL))?;
        Ok(me)
    }

    /// Override the per-request read deadline (verbs raise it for long turns / a long
    /// `next_update` block). `&self` (Cell-backed).
    pub fn set_request_timeout(&self, timeout: Duration) {
        self.request_timeout.set(timeout);
    }

    /// (N-idle, Item 3) Is a turn IN FLIGHT on the resident? Reads the `status` probe's
    /// `in_flight` flag (absent/false on a pre-Item-3 resident → `false`). A `wait`
    /// short-circuits a genuinely-idle session (in_flight=false) instead of camping.
    pub fn status_in_flight(&self) -> Result<bool, AcpError> {
        let result = self.request("status", json!({}))?;
        Ok(result
            .get("in_flight")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    /// The resident's established session id (the `status` health probe). Used by the
    /// create verb's readiness poll and by reconnecting verbs to confirm liveness.
    pub fn status_session_id(&self) -> Result<Option<String>, AcpError> {
        let result = self.request("status", json!({}))?;
        Ok(result
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    fn apply_read_timeout(&self, timeout: Duration) -> Result<(), AcpError> {
        match self.sock.borrow_mut().get_mut() {
            MaybeTlsStream::Plain(tcp) => tcp
                .set_read_timeout(Some(timeout))
                .map_err(|e| AcpError::Transport(format!("set_read_timeout: {e}"))),
            _ => Err(AcpError::Transport(
                "unexpected TLS stream on a ws:// connection".into(),
            )),
        }
    }

    fn mint_id(&self) -> u64 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        id
    }

    /// Send a request `{"id",m,…args}`, returning the correlation id once the bytes are
    /// confirmed handed to the socket (the `request`/`prompt` dispatch-timing split — see
    /// [`Self::request`] and [`AcpClient::prompt`]'s `on_dispatched`).
    fn send_request(&self, method: &str, mut args: Value) -> Result<u64, AcpError> {
        let id = self.mint_id();
        if let Some(obj) = args.as_object_mut() {
            obj.insert("id".into(), json!(id));
            obj.insert("m".into(), json!(method));
        }
        let text = serde_json::to_string(&args)
            .map_err(|e| AcpError::Transport(format!("serialize: {e}")))?;
        self.sock
            .borrow_mut()
            .send(Message::Text(text.into()))
            .map_err(map_ws_err)?;
        Ok(id)
    }

    /// Send a request and correlate the response by id, honoring the per-request read
    /// deadline (poll-until-deadline, exactly the ws.rs pattern). A thin wrapper over
    /// [`Self::send_request`] + [`Self::read_response`] — kept for callers (`initialize`,
    /// `new_session`, `cancel`, `status_*`) that have no dispatch-timing concern of their
    /// own; `prompt` calls the two phases separately (see [`AcpClient::prompt`] below).
    fn request(&self, method: &str, args: Value) -> Result<Value, AcpError> {
        let id = self.send_request(method, args)?;
        self.read_response(id)
    }

    fn read_response(&self, want_id: u64) -> Result<Value, AcpError> {
        let deadline = Instant::now() + self.request_timeout.get();
        loop {
            if Instant::now() >= deadline {
                return Err(AcpError::Transport("request timeout".into()));
            }
            let msg = match self.sock.borrow_mut().read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(io_err))
                    if io_err.kind() == io::ErrorKind::WouldBlock
                        || io_err.kind() == io::ErrorKind::TimedOut =>
                {
                    continue; // poll granularity; keep polling until the deadline
                }
                Err(e) => return Err(map_read_err(e)),
            };
            let text = match msg {
                Message::Text(t) => t.as_str().to_owned(),
                Message::Close(_) => return Err(AcpError::Closed),
                _ => continue,
            };
            let frame: Value = serde_json::from_str(&text)
                .map_err(|e| AcpError::Protocol(format!("non-JSON frame: {e}")))?;
            let fid = frame.get("id").and_then(Value::as_u64);
            if fid != Some(want_id) {
                continue; // one-in-flight: a stray id, drop rather than wedge
            }
            if let Some(e) = frame.get("e") {
                return Err(decode_err(e));
            }
            return Ok(frame.get("ok").cloned().unwrap_or(Value::Null));
        }
    }
}

/// Parse the `127.0.0.1:<port>` socket addr out of a `ws://127.0.0.1:<port>` endpoint
/// (for the bounded `TcpStream::connect_timeout` in [`AcpConnection::connect`]).
fn parse_ws_addr(url: &str) -> Result<std::net::SocketAddr, AcpError> {
    let hostport = url.strip_prefix("ws://").unwrap_or(url);
    let hostport = hostport.split('/').next().unwrap_or(hostport);
    hostport
        .parse()
        .map_err(|e| AcpError::Transport(format!("bad ws url {url}: {e}")))
}

fn map_ws_err(e: tungstenite::Error) -> AcpError {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            AcpError::Closed
        }
        other => AcpError::Transport(other.to_string()),
    }
}

fn map_read_err(e: tungstenite::Error) -> AcpError {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            AcpError::Closed
        }
        other => AcpError::Transport(other.to_string()),
    }
}

impl AcpClient for AcpConnection {
    fn initialize(&self) -> Result<InitializeResult, AcpError> {
        self.request("initialize", json!({}))
            .map(|v| decode_init(&v))
    }

    fn new_session(&self, cwd: &str) -> Result<SessionId, AcpError> {
        let v = self.request("new_session", json!({ "cwd": cwd }))?;
        v.get("session")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| AcpError::Protocol("new_session reply had no session".into()))
    }

    /// Split into `send_request` + `read_response` (rather than the shared `request`
    /// helper) so `on_dispatched` fires the MOMENT this turn's bytes are confirmed on the
    /// wire — before we know whether the reply ever arrives. A caller's durable marker
    /// write happens here, not after `read_response`, so a socket drop between dispatch
    /// and reply-read still records that this turn's bytes genuinely went out — the
    /// row's wire-history stays true for any later reader (Child D: no disposition
    /// branches on it, but the resume seam consumes it).
    fn prompt(
        &self,
        _session: &str,
        text: &str,
        from: &str,
        on_dispatched: &dyn Fn(),
    ) -> Result<TurnId, AcpError> {
        let id = self.send_request("prompt", json!({ "text": text, "from": from }))?;
        on_dispatched();
        let v = self.read_response(id)?;
        v.get("turn")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| AcpError::Protocol("prompt reply had no turn".into()))
    }

    fn cancel(&self, _session: &str) -> Result<(), AcpError> {
        self.request("cancel", json!({})).map(|_| ())
    }

    fn next_update(&self, timeout: Duration) -> Result<Option<AcpEvent>, AcpError> {
        // Floor the read deadline above the requested poll so the response (which the
        // server may take up to its own cap to produce) is not cut off as a timeout.
        let prev = self.request_timeout.get();
        self.request_timeout
            .set(timeout.max(SERVER_NEXT_UPDATE_CAP) + READ_POLL_INTERVAL);
        let out = self.request(
            "next_update",
            json!({ "timeout_ms": timeout.as_millis() as u64 }),
        );
        self.request_timeout.set(prev);
        let v = out?;
        match v.get("event") {
            Some(Value::Null) | None => Ok(None),
            Some(ev) => decode_event(ev).map(Some),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// A fake resident standing in for `AcpHost` — drives the wire WITHOUT a live
    /// bridge. It hands back a SCRIPTED sequence of events so the test can assert the
    /// S4 client receives them byte-identically (faithfulness) — and so the revert-probe
    /// (a `serve` that synthesizes) would diverge.
    struct FakeResident {
        events: RefCell<std::collections::VecDeque<AcpEvent>>,
        session: Option<String>,
        in_flight: bool,
        bridge_dead: bool,
    }
    impl Default for FakeResident {
        fn default() -> Self {
            FakeResident {
                events: RefCell::new(Default::default()),
                session: Some("sess-1".into()),
                in_flight: false,
                bridge_dead: false,
            }
        }
    }
    impl AcpClient for FakeResident {
        fn initialize(&self) -> Result<InitializeResult, AcpError> {
            Ok(InitializeResult {
                protocol_version: 1,
                agent_name: Some("fake".into()),
                agent_version: Some("0".into()),
                auth_required: false,
            })
        }
        fn new_session(&self, _cwd: &str) -> Result<SessionId, AcpError> {
            Ok("sess-1".into())
        }
        fn prompt(
            &self,
            _s: &str,
            _t: &str,
            _f: &str,
            on_dispatched: &dyn Fn(),
        ) -> Result<TurnId, AcpError> {
            on_dispatched();
            Ok("turn-1".into())
        }
        fn cancel(&self, _s: &str) -> Result<(), AcpError> {
            Ok(())
        }
        fn next_update(&self, _timeout: Duration) -> Result<Option<AcpEvent>, AcpError> {
            Ok(self.events.borrow_mut().pop_front())
        }
    }
    impl AcpResident for FakeResident {
        fn resident_session_id(&self) -> Option<String> {
            self.session.clone()
        }
        fn resident_in_flight(&self) -> bool {
            self.in_flight
        }
        fn bridge_confirmed_dead(&self) -> bool {
            self.bridge_dead
        }
    }

    /// Spawn `serve` on a loopback listener in a scoped thread, returning the bound url.
    /// The resident is MOVED into the server thread (production runs `serve` on the
    /// adapter's own main thread — single-threaded ownership, no `Sync` needed; only the
    /// test spawns it elsewhere, so it takes the resident by value: `Send`, not `Sync`).
    fn with_server<R: AcpResident + Send, F: FnOnce(String)>(resident: R, body: F) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("ws://127.0.0.1:{port}");
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        // F5: a Drop guard sets `shutdown` + nudges the accept loop on EVERY exit path —
        // including a body assertion PANIC. Without it, a panicking body skipped the
        // shutdown store, so `thread::scope` blocked forever joining the still-looping
        // serve thread and a faithfulness regression HUNG instead of failing clean.
        struct StopGuard<'a> {
            flag: &'a AtomicBool,
            port: u16,
        }
        impl Drop for StopGuard<'_> {
            fn drop(&mut self) {
                self.flag.store(true, Ordering::SeqCst);
                // Nudge the (possibly blocked) accept loop awake so the thread exits.
                let _ = TcpStream::connect(("127.0.0.1", self.port));
            }
        }
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _ = serve(&resident, &listener, &sd);
            });
            let _stop = StopGuard {
                flag: &shutdown,
                port,
            };
            body(url);
            // `_stop` drops here on the success path, or during unwind on a panic —
            // either way the serve thread is signaled and the scope join completes.
        });
    }

    #[test]
    fn wire_relays_event_payload_verbatim() {
        // A distinctive payload the server must relay byte-for-byte (the faithfulness
        // assertion). If `serve` synthesized an event instead of relaying the real one,
        // this nested payload would not survive — the non-vacuity property.
        let payload = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "PONG-7f3a"},
            "_nonce": [1, 2, 3, {"deep": true}],
        });
        let resident = FakeResident {
            events: RefCell::new(
                vec![
                    AcpEvent::Update {
                        session: "sess-1".into(),
                        kind: "agent_message_chunk".into(),
                        payload: payload.clone(),
                    },
                    AcpEvent::Terminal {
                        session: "sess-1".into(),
                        turn: "turn-1".into(),
                        stop: StopReason::EndTurn,
                    },
                ]
                .into(),
            ),
            ..Default::default()
        };
        with_server(resident, |url| {
            let client = AcpConnection::connect(&url, Duration::from_secs(2)).unwrap();
            // First pull: the Update, payload byte-identical to what the fake emitted.
            let ev1 = client.next_update(Duration::from_secs(1)).unwrap().unwrap();
            match ev1 {
                AcpEvent::Update {
                    kind, payload: got, ..
                } => {
                    assert_eq!(kind, "agent_message_chunk");
                    assert_eq!(got, payload, "payload must relay VERBATIM (faithfulness)");
                }
                other => panic!("expected Update, got {other:?}"),
            }
            // Second pull: the Terminal, stop reason preserved.
            let ev2 = client.next_update(Duration::from_secs(1)).unwrap().unwrap();
            assert_eq!(
                ev2,
                AcpEvent::Terminal {
                    session: "sess-1".into(),
                    turn: "turn-1".into(),
                    stop: StopReason::EndTurn,
                }
            );
            // Third pull: nothing left → Ok(None) (quiet stream, not an error).
            assert_eq!(
                client.next_update(Duration::from_millis(300)).unwrap(),
                None
            );
        });
    }

    #[test]
    fn wire_round_trips_the_five_methods_and_status() {
        let resident = FakeResident::default();
        with_server(resident, |url| {
            let c = AcpConnection::connect(&url, Duration::from_secs(2)).unwrap();
            assert_eq!(c.initialize().unwrap().agent_name.as_deref(), Some("fake"));
            assert_eq!(c.new_session(".").unwrap(), "sess-1");
            assert_eq!(c.prompt("", "hi", "tester", &|| {}).unwrap(), "turn-1");
            c.cancel("").unwrap();
            assert_eq!(c.status_session_id().unwrap().as_deref(), Some("sess-1"));
        });
    }

    /// (N-idle, Item 3) the `status` probe carries `in_flight` over the wire BOTH ways:
    /// an idle resident → `status_in_flight()` false (a `wait` short-circuits); a resident
    /// with a turn in flight → true (a mid-turn `wait` does NOT false-idle). REVERT SEAM:
    /// drop the `in_flight` field from the `status` reply → `status_in_flight` reads the
    /// `unwrap_or(false)` default → the in-flight (true) arm REDs.
    #[test]
    fn status_carries_in_flight_both_arms() {
        // idle resident → in_flight=false.
        let idle = FakeResident {
            in_flight: false,
            ..Default::default()
        };
        with_server(idle, |url| {
            let c = AcpConnection::connect(&url, Duration::from_secs(2)).unwrap();
            assert!(
                !c.status_in_flight().unwrap(),
                "an idle resident reports in_flight=false"
            );
        });
        // mid-turn resident → in_flight=true (never false-idle).
        let busy = FakeResident {
            in_flight: true,
            ..Default::default()
        };
        with_server(busy, |url| {
            let c = AcpConnection::connect(&url, Duration::from_secs(2)).unwrap();
            assert!(
                c.status_in_flight().unwrap(),
                "a mid-turn resident reports in_flight=true"
            );
        });
    }

    /// (W) WEDGED-BUT-ALIVE — `serve` SELF-TERMINATES (returns Err, → adapter exits
    /// nonzero) when the bridge is CONFIRMED DEAD, WITHOUT `shutdown` being set. This is
    /// what makes `pid_alive=false` the honest signal the resume/ls gates read. REVERT
    /// SEAM (W): drop the `bridge_confirmed_dead` check in `serve` → serve never returns
    /// on a dead bridge → this hangs/REDs (the bug: the zombie adapter lingers 'live').
    #[test]
    fn serve_self_terminates_on_confirmed_dead_bridge() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // shutdown is NEVER set → ONLY the dead-bridge guard can end serve.
        let shutdown = Arc::new(AtomicBool::new(false));
        let resident = FakeResident {
            bridge_dead: true,
            ..Default::default()
        };
        let sd = shutdown.clone();
        let h = std::thread::spawn(move || serve(&resident, &listener, &sd));
        let start = Instant::now();
        while !h.is_finished() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "serve must SELF-TERMINATE on a confirmed-dead bridge (no shutdown set)"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let res = h.join().unwrap();
        assert!(
            res.is_err(),
            "self-terminate returns Err → adapter exits nonzero"
        );
    }

    /// (W) NEGATIVE CONTROL (arms ii+iii) — a LIVE bridge (bridge_dead=false, incl. a
    /// busy-but-alive mid-turn or a transient blip) must NOT self-terminate: `serve` keeps
    /// running and only ends when `shutdown` is set (the normal teardown). Proves the fix
    /// never kills a recoverable session (no silent loss).
    #[test]
    fn serve_does_not_self_terminate_on_live_bridge() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let resident = FakeResident {
            bridge_dead: false,
            ..Default::default()
        };
        let sd = shutdown.clone();
        let h = std::thread::spawn(move || serve(&resident, &listener, &sd));
        // Give it time: a live bridge must keep serving (NOT self-terminate).
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !h.is_finished(),
            "a live bridge must NOT self-terminate (no silent loss)"
        );
        // Normal teardown ends it cleanly.
        shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", port)); // nudge the accept loop awake
        let start = Instant::now();
        while !h.is_finished() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "serve must end on shutdown"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            h.join().unwrap().is_ok(),
            "shutdown teardown returns Ok (graceful)"
        );
    }

    /// The encode/decode pair is an exact inverse for every event arm (a pure check,
    /// independent of the socket — pins the relay's serialization contract).
    #[test]
    fn event_encode_decode_roundtrips_all_arms() {
        for ev in [
            AcpEvent::Update {
                session: "s".into(),
                kind: "tool_call".into(),
                payload: json!({"a": 1, "b": [true, null, "x"]}),
            },
            AcpEvent::Terminal {
                session: "s".into(),
                turn: "t".into(),
                stop: StopReason::Cancelled,
            },
            AcpEvent::TerminalError {
                session: "s".into(),
                turn: "t".into(),
                message: "internalError".into(),
            },
        ] {
            let back = decode_event(&encode_event(&ev)).unwrap();
            assert_eq!(back, ev, "encode/decode must round-trip {ev:?}");
        }
    }

    /// (N-conc, Item 3) the ws-handshake connect timeout: a server that accepts the TCP
    /// connection but NEVER performs the ws handshake (modelling the single-conn `serve`
    /// loop camped in a long `wait`) must make `connect` fail FAST (~timeout), NOT hang
    /// for the camped client's lifetime. REVERT SEAM: restore `tungstenite::connect(url)`
    /// (no handshake deadline) → this connect blocks until the server acts → the elapsed
    /// assert REDs (the test would hang past the bound).
    #[test]
    fn connect_handshake_is_bounded_against_a_camped_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept the TCP conn, then sit WITHOUT handshaking (never `tungstenite::accept`).
        let server = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_millis(800));
                drop(stream);
            }
        });
        let url = format!("ws://{addr}");
        let start = Instant::now();
        let res = AcpConnection::connect(&url, Duration::from_millis(300));
        let elapsed = start.elapsed();
        assert!(
            res.is_err(),
            "a camped (non-handshaking) server must not yield a connection"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "connect must fail FAST (bounded by the handshake timeout), took {elapsed:?}"
        );
        let _ = server.join();
    }

    /// Child B (opencode D1) exactly-once dispatch-timing guard, kept under Child D:
    /// `on_dispatched` MUST fire once this turn's bytes are confirmed on the wire, even
    /// when the reply is NEVER read (a socket drop between "bytes sent" and "local read
    /// of the reply" — the scenario that must never look pre-send in the row's durable
    /// wire-history). A server that completes the ws handshake, reads the ONE `prompt` frame
    /// (proving the write left the client), then drops the connection WITHOUT ever
    /// replying models this. REVERT SEAM: moving `on_dispatched()` to fire only after
    /// a successful `read_response` (the bug this guards against) would make this
    /// test's `dispatched.load()` assertion FAIL — the marker would never be set on
    /// this no-reply path, exactly the double-delivery risk the guard exists to close.
    #[test]
    fn prompt_on_dispatched_fires_even_when_no_reply_ever_arrives() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            // Read the ONE `prompt` request frame, then drop — never reply.
            let _ = ws.read();
        });
        let url = format!("ws://{addr}");
        let conn = AcpConnection::connect(&url, Duration::from_secs(2)).unwrap();
        conn.set_request_timeout(Duration::from_millis(500));

        let dispatched = Arc::new(AtomicBool::new(false));
        let d = dispatched.clone();
        let on_dispatched = move || d.store(true, Ordering::SeqCst);

        let result = conn.prompt("sess-1", "hi", "tester", &on_dispatched);

        assert!(
            result.is_err(),
            "no reply ever arrives — prompt must surface an error, not hang or fabricate a turn id"
        );
        assert!(
            dispatched.load(Ordering::SeqCst),
            "on_dispatched must have fired BEFORE the (failed) reply read — a caller's durable \
             structured_send_issued marker must survive a socket drop right after dispatch, or \
             the row carries a false never-sent wire-history for every later reader"
        );
        let _ = server.join();
    }
}
