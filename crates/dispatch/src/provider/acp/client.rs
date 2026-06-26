//! `provider/acp/client.rs` — [`AcpHost`], the LIVE headless ACP driver: the real
//! `claude-code-acp` bridge subprocess + the SC-5 single-reader demux + the SC-1
//! outbound queue + the Pete's-view transcript tee. It IMPLEMENTS the
//! [`AcpClient`](super::rpc::AcpClient) transport CONTRACT (the verb layer hands a
//! connected `&AcpHost` into [`crate::provider::ProviderFx::acp_client`], ADD-5) and
//! provides the [`AcpTurnObserver`](super::completion::AcpTurnObserver) the
//! `--wait` loop wraps in an [`AcpTurnCompletion`](super::completion::AcpTurnCompletion).
//!
//! **Why a reader THREAD (the one structural divergence from the codex
//! `WsAppServer` precedent).** Codex reads inline because a `TcpStream` honors
//! `set_read_timeout`, so a verb can block-read-with-deadline on the one socket.
//! The ACP bridge speaks over a stdio PIPE, and a pipe has no portable read
//! deadline — so the SC-5 single reader is a dedicated background TASK that owns
//! `ChildStdout`, parses each ndjson line into a [`RawFrame`], and republishes it
//! onto an `mpsc` BUS (exactly the spec's "single-reader demux task republishing
//! `AcpEvent` onto a per-session bus"). The host stays single-threaded / `!Sync`
//! (its mutable state is one [`RefCell`]); the reader thread touches NO host state
//! — it only pumps `stdout → channel`. The host (on its own thread) drains the bus
//! in [`AcpClient::next_update`], doing all the queue/progress/transcript bookkeeping
//! and the rpc-id→turn correlation there, so the SC-2b release ordering is preserved:
//! because notifications and the `session/prompt` response travel IN ORDER on the one
//! stdout stream (STEP-0), the single reader necessarily enqueues a turn's trailing
//! `session/update`s before its terminal response, and `next_update` processes them
//! in that order.
//!
//! Wire law (STEP-0, `~/work/acp-cc-coord/STEP0-FINDING.md`): newline-delimited
//! JSON-RPC **2.0** (carries `"jsonrpc":"2.0"`, UNLIKE the codex bare frames) over
//! stdio. Methods `initialize`, `session/new`, `session/prompt`, `session/cancel`,
//! `session/load`; `session/update` notifications; reverse requests
//! (`session/request_permission`, `fs/*`, `terminal/*`) the client MUST answer or a
//! turn that needs one wedges.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::completion::{AcpTurnObservation, AcpTurnObserver, TurnProgress};
use super::queue::OutboundQueue;
use super::rpc::{
    AcpClient, AcpError, AcpEvent, InitializeResult, SessionId, StopReason, TurnId,
};
use super::BRIDGE_ENV_STRIP;

/// Default SC-1 outbound-queue depth (waiting backlog, excluding the in-flight turn).
const DEFAULT_QUEUE_CAPACITY: usize = 16;

/// The ACP protocol version this client speaks (STEP-0: the bridge negotiates v1).
const PROTOCOL_VERSION: i64 = 1;

/// One classified inbound line off the bridge's stdout, produced by the reader
/// thread and consumed on the host thread. Lower-level than [`AcpEvent`]: the host
/// owns the rpc-id→turn mapping, so the reader emits the raw rpc id and the host
/// translates a [`Self::Response`]/[`Self::ErrorResponse`] into an
/// [`AcpEvent::Terminal`]/[`AcpEvent::TerminalError`] after correlating it.
#[derive(Debug)]
enum RawFrame {
    /// A `session/update` notification (`method == "session/update"`, no id).
    Update {
        session: String,
        kind: String,
        update: Value,
    },
    /// A JSON-RPC response `{"id":N,"result":..}` (no `method`).
    Response { id: u64, result: Value },
    /// A JSON-RPC error response `{"id":N,"error":{..}}` (no `method`).
    ErrorResponse { id: u64, message: String },
    /// A reverse REQUEST from the agent (`method` + `id`) — e.g.
    /// `session/request_permission` — which the host MUST answer.
    ReverseRequest { id: Value, method: String },
    /// stdout reached EOF / a fatal read error — the bridge child is gone.
    Closed(String),
}

/// Classify one parsed inbound JSON value into a [`RawFrame`]. `None` = a frame we
/// deliberately ignore (an unknown notification: not part of the CC ACP surface).
fn classify(v: Value) -> Option<RawFrame> {
    let obj = v.as_object()?;
    let has_method = obj.contains_key("method");
    let id = obj.get("id").cloned();

    // No `method` → it is a response (success or error) to one of OUR requests.
    if !has_method {
        let idn = id.as_ref().and_then(Value::as_u64)?;
        if let Some(err) = obj.get("error") {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown jsonrpc error")
                .to_string();
            return Some(RawFrame::ErrorResponse { id: idn, message });
        }
        let result = obj.get("result").cloned().unwrap_or(Value::Null);
        return Some(RawFrame::Response { id: idn, result });
    }

    let method = obj.get("method").and_then(Value::as_str)?.to_string();

    // `method` + `id` → a server-initiated REQUEST awaiting our response.
    if let Some(id) = id {
        return Some(RawFrame::ReverseRequest { id, method });
    }

    // `method`, no id → a notification. Only `session/update` is in our surface.
    if method == "session/update" {
        let params = obj.get("params")?;
        let session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let update = params.get("update").cloned().unwrap_or(Value::Null);
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Some(RawFrame::Update {
            session,
            kind,
            update,
        });
    }
    None
}

/// The reader thread body: own `stdout`, parse each ndjson line, republish onto the
/// bus. The ONE consumer of the bridge's stdout (SC-5). Exits on EOF, a read error,
/// or the host dropping the receiver.
fn reader_loop(stdout: ChildStdout, tx: Sender<RawFrame>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = tx.send(RawFrame::Closed("stdout eof".to_string()));
                return;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    // Tolerate a non-JSON line (a stray bridge log on stdout) rather
                    // than wedging the stream.
                    Err(_) => continue,
                };
                if let Some(frame) = classify(value) {
                    if tx.send(frame).is_err() {
                        return; // host dropped the receiver
                    }
                }
            }
            Err(_) => {
                let _ = tx.send(RawFrame::Closed("stdout read error".to_string()));
                return;
            }
        }
    }
}

/// The host's mutable state, single-threaded behind one [`RefCell`].
struct HostInner {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<RawFrame>,
    reader: Option<JoinHandle<()>>,
    /// JSON-RPC request-id counter.
    next_id: u64,
    /// The established ACP session id (`None` before `session/new`).
    session: Option<SessionId>,
    /// The SC-1a CONFIGURED outbound-queue bound (A2 SC-1a: "configurable N"), set at
    /// spawn and PRESERVED across `session/new`/`session/load` so the queue is rebuilt
    /// at the host's real capacity, not silently reset to the default.
    capacity: usize,
    /// The SC-1 outbound queue (created at `session/new`, keyed by the ACP session).
    queue: Option<OutboundQueue>,
    /// The SC-2c started→streaming classifier for the in-flight turn.
    progress: TurnProgress,
    /// rpc-id → turn-id, for correlating a `session/prompt` response to its turn.
    inflight: HashMap<u64, TurnId>,
    /// `AcpEvent`s read while a blocking handshake was correlating its response —
    /// served FIRST by [`AcpClient::next_update`] so nothing is lost.
    pending: VecDeque<AcpEvent>,
    /// The latest terminal observation, for the [`AcpTurnObserver`] the completion
    /// probe reads (SC-2a). Starts `Pending`.
    last_obs: AcpTurnObservation,
    /// The Pete's-view transcript tee: every `session/update`'s raw `update` object,
    /// in arrival order (A2 §5 — the headless engine has no TUI, so the demux tees
    /// the full content feed to a transcript Pete watches).
    transcript: Vec<Value>,
}

impl HostInner {
    /// Mint the next JSON-RPC request id.
    fn mint_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Write one JSON-RPC frame (request or notification) + a newline (ndjson).
    fn write_frame(&mut self, frame: &Value) -> Result<(), AcpError> {
        let mut text = serde_json::to_string(frame)
            .map_err(|e| AcpError::Transport(format!("serialize: {e}")))?;
        text.push('\n');
        self.stdin
            .write_all(text.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| AcpError::Transport(format!("write: {e}")))
    }

    /// Send a JSON-RPC REQUEST and return its id (correlate the response later).
    fn send_request(&mut self, method: &str, params: Value) -> Result<u64, AcpError> {
        let id = self.mint_id();
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write_frame(&frame)?;
        Ok(id)
    }

    /// Send a JSON-RPC NOTIFICATION (no id, no response).
    fn send_notification(&mut self, method: &str, params: Value) -> Result<(), AcpError> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write_frame(&frame)
    }

    /// Answer a reverse request. `session/request_permission` → cancel the requested
    /// permission (the headless host does not interactively grant tool permissions;
    /// a turn that needs one terminates rather than wedging — STEP-0 §8 / §5). Any
    /// other reverse request (`fs/*`, `terminal/*` — which we declared OFF in
    /// `clientCapabilities`) → a JSON-RPC method-not-found error so the bridge does
    /// not block on a response.
    fn answer_reverse(&mut self, id: Value, method: &str) -> Result<(), AcpError> {
        let reply = if method == "session/request_permission" {
            json!({"jsonrpc":"2.0","id": id, "result": {"outcome": {"outcome": "cancelled"}}})
        } else {
            json!({"jsonrpc":"2.0","id": id, "error": {"code": -32601, "message": format!("method {method} not supported by this client")}})
        };
        self.write_frame(&reply)
    }

    /// Block until the response to `want_id` arrives, buffering `session/update`s
    /// into `pending` (served later by `next_update`) and answering reverse requests
    /// inline. Used by the synchronous handshake verbs (`initialize`/`session/new`/
    /// `session/load`), which are one-in-flight with no queue involvement.
    fn pump_until_response(&mut self, want_id: u64, timeout: Duration) -> Result<Value, AcpError> {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(AcpError::Protocol(format!(
                    "timed out waiting for response to request {want_id}"
                )));
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(RawFrame::Response { id, result }) if id == want_id => return Ok(result),
                Ok(RawFrame::ErrorResponse { id, message }) if id == want_id => {
                    return Err(AcpError::Protocol(message))
                }
                Ok(RawFrame::Update {
                    session,
                    kind,
                    update,
                }) => {
                    self.transcript.push(update.clone());
                    self.pending.push_back(AcpEvent::Update {
                        session,
                        kind,
                        payload: update,
                    });
                }
                Ok(RawFrame::ReverseRequest { id, method, .. }) => {
                    self.answer_reverse(id, &method)?;
                }
                // A stray response for a different id (no other request in flight
                // during a handshake) — drop.
                Ok(RawFrame::Response { .. }) | Ok(RawFrame::ErrorResponse { .. }) => continue,
                Ok(RawFrame::Closed(why)) => {
                    self.last_obs = AcpTurnObservation::TransportLost(why);
                    return Err(AcpError::Closed);
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(AcpError::Protocol(format!(
                        "timed out waiting for response to request {want_id}"
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.last_obs = AcpTurnObservation::TransportLost("reader gone".to_string());
                    return Err(AcpError::Closed);
                }
            }
        }
    }

    /// If the session is turn-idle and an item waits, make it in-flight and issue it
    /// as `session/prompt` (the SC-1 release → send). Records the rpc-id→turn map so
    /// the correlated terminal releases the slot.
    fn release_next(&mut self) -> Result<(), AcpError> {
        let session = match &self.session {
            Some(s) => s.clone(),
            None => return Ok(()),
        };
        let item = match self.queue.as_mut().and_then(OutboundQueue::try_release) {
            Some(item) => item,
            None => return Ok(()),
        };
        // A new turn begins → reset the started/streaming classifier.
        self.progress = TurnProgress::new();
        let params = json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": item.message}],
        });
        let id = self.send_request("session/prompt", params)?;
        self.inflight.insert(id, item.turn);
        Ok(())
    }
}

/// The live ACP host: a `claude-code-acp` bridge subprocess + its SC-5 reader + the
/// SC-1 queue + the transcript tee. `!Sync` by design (one [`RefCell`]); the `&self`
/// methods are callable through the shared `&dyn AcpClient` out of `ProviderFx`.
pub struct AcpHost {
    inner: RefCell<HostInner>,
}

impl AcpHost {
    /// Spawn the bridge `program args…` (e.g. `("claude-code-acp", [])` in prod, or
    /// `("node", ["…/dist/index.js"])` against a local install) with the working dir
    /// `cwd`, **stripping [`BRIDGE_ENV_STRIP`] from the child env** (STEP-0 nesting
    /// guard — dispatch runs as a CC child, so `session/new` fails without this) and
    /// starting the SC-5 reader thread. Does NOT handshake — call [`Self::initialize`]
    /// then [`Self::new_session`] (the boot waiter drives `initialize`).
    pub fn spawn(program: &str, args: &[String], cwd: &Path) -> Result<AcpHost, AcpError> {
        Self::spawn_with_capacity(program, args, cwd, DEFAULT_QUEUE_CAPACITY)
    }

    /// [`Self::spawn`] with an explicit SC-1 backlog capacity (for backpressure tests).
    pub fn spawn_with_capacity(
        program: &str,
        args: &[String],
        cwd: &Path,
        capacity: usize,
    ) -> Result<AcpHost, AcpError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Bridge stderr → our stderr: visible diagnostics, never on the protocol
            // stream. The reader only ever reads stdout (SC-5).
            .stderr(Stdio::inherit());
        // The nesting guard (STEP-0 load-bearing finding #1): remove the CC env vars
        // so the bridge's Agent SDK does not refuse to launch "inside another Claude
        // Code session". `LaunchPlan.env` can only ADD, so this STRIP is a spawn-time
        // concern, applied here and pinned by `spawn_strips_nesting_env`.
        for var in BRIDGE_ENV_STRIP {
            cmd.env_remove(var);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| AcpError::Transport(format!("spawn {program}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpError::Transport("bridge stdin not piped".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpError::Transport("bridge stdout not piped".to_string()))?;
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("acp-reader".to_string())
            .spawn(move || reader_loop(stdout, tx))
            .map_err(|e| AcpError::Transport(format!("reader thread: {e}")))?;

        Ok(AcpHost {
            inner: RefCell::new(HostInner {
                child,
                stdin,
                rx,
                reader: Some(reader),
                next_id: 0,
                session: None,
                capacity,
                queue: Some(OutboundQueue::new("pending-session", capacity)),
                progress: TurnProgress::new(),
                inflight: HashMap::new(),
                pending: VecDeque::new(),
                last_obs: AcpTurnObservation::Pending,
                transcript: Vec::new(),
            }),
        })
    }

    /// The established ACP session id, if `session/new`/`session/load` has run.
    pub fn session_id(&self) -> Option<String> {
        self.inner.borrow().session.clone()
    }

    /// (N-idle, Item 3) Is a turn IN FLIGHT? Reads the SC-1 [`OutboundQueue`]'s primary
    /// `is_idle` truth (the in-flight turn is released only on its REAL terminal), so a
    /// `wait` can short-circuit a genuinely-idle session yet never false-idle mid-turn.
    pub fn in_flight(&self) -> bool {
        self.inner
            .borrow()
            .queue
            .as_ref()
            .map(|q| !q.is_idle())
            .unwrap_or(false)
    }

    /// (W) WEDGED-BUT-ALIVE (Item 3, red-team round-1 #1) — is the bridge child CONFIRMED
    /// DEAD? A non-blocking `try_wait`: `Ok(Some(_))` = the child exited (and is REAPED
    /// here — no zombie left), so the bridge is genuinely gone; `Ok(None)` = STILL ALIVE;
    /// `Err(_)` = indeterminate → treated as NOT dead (conservative — never self-terminate
    /// a possibly-recoverable session). This is the silent-loss guard: it is a CONFIRMED
    /// child-death check, NOT the transport-loss observation — a TRANSIENT transport blip
    /// with the child still alive returns `false` here, so the adapter SURVIVES it. A
    /// reaped child necessarily closed its stdout (transport loss), so child-death
    /// subsumes "transport-loss AND child-dead" while being race-free.
    pub fn bridge_confirmed_dead(&self) -> bool {
        matches!(self.inner.borrow_mut().child.try_wait(), Ok(Some(_)))
    }

    /// `session/load` (resume): re-establish a prior session by id on this bridge.
    /// The bridge replays the session's history as `session/update`s (buffered into
    /// `pending`). Proves the resume verb on the LIVE bridge (pillar 2).
    pub fn load_session(&self, session_id: &str, cwd: &str) -> Result<(), AcpError> {
        let mut inner = self.inner.borrow_mut();
        let params = json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []});
        let id = inner.send_request("session/load", params)?;
        inner.pump_until_response(id, Duration::from_secs(60))?;
        inner.session = Some(session_id.to_string());
        let capacity = inner.capacity;
        inner.queue = Some(OutboundQueue::new(session_id.to_string(), capacity));
        Ok(())
    }

    /// The full Pete's-view transcript tee: every `session/update`'s raw `update`
    /// object in arrival order (the live-view feed; A2 §5).
    pub fn transcript(&self) -> Vec<Value> {
        self.inner.borrow().transcript.clone()
    }

    /// The accumulated assistant TEXT across all `agent_message_chunk` updates (what
    /// Pete reads as the model's reply). Convenience over [`Self::transcript`].
    pub fn assistant_text(&self) -> String {
        self.inner
            .borrow()
            .transcript
            .iter()
            .filter(|u| u.get("sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk"))
            .filter_map(|u| u.get("content").and_then(|c| c.get("text")).and_then(Value::as_str))
            .collect()
    }

    /// Best-effort teardown: kill the bridge child and join the reader. Idempotent.
    pub fn shutdown(&self) {
        let mut inner = self.inner.borrow_mut();
        let _ = inner.child.kill();
        let _ = inner.child.wait();
        if let Some(handle) = inner.reader.take() {
            // The child is already killed+reaped above → its stdout is closed → the
            // reader's `read_line` hits EOF and the thread returns, so this join cannot
            // hang on a wedged reader. Deterministic teardown (LOW-2: was `drop`/detach).
            let _ = handle.join();
        }
    }
}

impl Drop for AcpHost {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        let _ = inner.child.kill();
        let _ = inner.child.wait();
        if let Some(handle) = inner.reader.take() {
            let _ = handle.join();
        }
    }
}

impl AcpClient for AcpHost {
    fn initialize(&self) -> Result<InitializeResult, AcpError> {
        let mut inner = self.inner.borrow_mut();
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false,
            },
            "clientInfo": {"name": "dispatch-acp", "version": env!("CARGO_PKG_VERSION")},
        });
        let id = inner.send_request("initialize", params)?;
        let result = inner.pump_until_response(id, Duration::from_secs(30))?;
        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_i64)
            .unwrap_or(PROTOCOL_VERSION);
        let agent_name = result
            .get("agentInfo")
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let agent_version = result
            .get("agentInfo")
            .and_then(|a| a.get("version"))
            .and_then(Value::as_str)
            .map(str::to_string);
        // `authMethods` is ALWAYS advertised (e.g. `claude-login`) even when a usable
        // OAuth credential is present — STEP-0 round-tripped with NO key. So the
        // handshake succeeding IS readiness; a genuine auth failure surfaces later as
        // a JSON-RPC error to `session/new`/`session/prompt` (→ TerminalError), not
        // here. We therefore do NOT gate boot on `authMethods` non-empty.
        Ok(InitializeResult {
            protocol_version,
            agent_name,
            agent_version,
            auth_required: false,
        })
    }

    fn new_session(&self, cwd: &str) -> Result<SessionId, AcpError> {
        let mut inner = self.inner.borrow_mut();
        let params = json!({"cwd": cwd, "mcpServers": []});
        let id = inner.send_request("session/new", params)?;
        let result = inner.pump_until_response(id, Duration::from_secs(60))?;
        let session = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::Protocol("session/new result had no sessionId".to_string()))?
            .to_string();
        // The queue is rebuilt keyed by the real ACP session, at the host's CONFIGURED
        // capacity (SC-1a) — NOT silently reset to the default (LOW-1).
        let capacity = inner.capacity;
        inner.queue = Some(OutboundQueue::new(session.clone(), capacity));
        inner.session = Some(session.clone());
        Ok(session)
    }

    fn prompt(&self, _session: &str, text: &str, from: &str) -> Result<TurnId, AcpError> {
        let mut inner = self.inner.borrow_mut();
        if inner.session.is_none() {
            return Err(AcpError::Protocol("no acp session established".to_string()));
        }
        // ENQUEUE (mints + returns the attributable turn id; LB#5). A 2nd inject
        // mid-turn QUEUES (SC-3); a bounded-overflow → Err(QueueFull) (SC-1a).
        let turn = inner
            .queue
            .as_mut()
            .ok_or_else(|| AcpError::Protocol("no acp session established".to_string()))?
            .enqueue(text, from)
            .map_err(|_| AcpError::QueueFull)?;
        // Release+send NOW iff the session is turn-idle (never interrupts an
        // in-flight turn — SC-1); otherwise it sends when the in-flight terminal
        // releases the slot in `next_update`.
        inner.release_next()?;
        Ok(turn)
    }

    fn cancel(&self, _session: &str) -> Result<(), AcpError> {
        let mut inner = self.inner.borrow_mut();
        let session = inner
            .session
            .clone()
            .ok_or_else(|| AcpError::Protocol("no acp session established".to_string()))?;
        // Best-effort interrupt (a notification): the bridge sets cancelled + calls
        // query.interrupt(); the in-flight prompt then resolves `{stopReason:cancelled}`.
        inner.send_notification("session/cancel", json!({"sessionId": session}))
    }

    fn next_update(&self, timeout: Duration) -> Result<Option<AcpEvent>, AcpError> {
        let mut inner = self.inner.borrow_mut();
        // Serve anything buffered during a blocking handshake first (FIFO).
        if let Some(ev) = inner.pending.pop_front() {
            return Ok(Some(ev));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            match inner.rx.recv_timeout(deadline - now) {
                Ok(RawFrame::Update {
                    session,
                    kind,
                    update,
                }) => {
                    inner.progress.on_update();
                    inner.transcript.push(update.clone());
                    return Ok(Some(AcpEvent::Update {
                        session,
                        kind,
                        payload: update,
                    }));
                }
                Ok(RawFrame::Response { id, result }) => {
                    // A correlated `session/prompt` terminal? (Handshake responses are
                    // consumed inline by `pump_until_response`, never reach here.)
                    if let Some(turn) = inner.inflight.remove(&id) {
                        let session = inner.session.clone().unwrap_or_default();
                        let stop = result
                            .get("stopReason")
                            .and_then(Value::as_str)
                            .and_then(StopReason::parse);
                        match stop {
                            Some(stop) => {
                                if let Some(q) = inner.queue.as_mut() {
                                    q.complete(&turn);
                                }
                                inner.last_obs = AcpTurnObservation::Terminal(stop);
                                // Release+send the next queued turn (drain continuity).
                                inner.release_next()?;
                                return Ok(Some(AcpEvent::Terminal {
                                    session,
                                    turn,
                                    stop,
                                }));
                            }
                            None => {
                                // A terminal we cannot map (no/unknown stopReason) →
                                // treat as a failed turn, not a clean completion.
                                if let Some(q) = inner.queue.as_mut() {
                                    q.complete(&turn);
                                }
                                let message = "session/prompt response had no mappable stopReason"
                                    .to_string();
                                inner.last_obs = AcpTurnObservation::Failed(message.clone());
                                inner.release_next()?;
                                return Ok(Some(AcpEvent::TerminalError {
                                    session,
                                    turn,
                                    message,
                                }));
                            }
                        }
                    }
                    // Uncorrelated response — drop and keep draining.
                    continue;
                }
                Ok(RawFrame::ErrorResponse { id, message }) => {
                    if let Some(turn) = inner.inflight.remove(&id) {
                        let session = inner.session.clone().unwrap_or_default();
                        if let Some(q) = inner.queue.as_mut() {
                            q.complete(&turn);
                        }
                        inner.last_obs = AcpTurnObservation::Failed(message.clone());
                        inner.release_next()?;
                        return Ok(Some(AcpEvent::TerminalError {
                            session,
                            turn,
                            message,
                        }));
                    }
                    continue;
                }
                Ok(RawFrame::ReverseRequest { id, method, .. }) => {
                    inner.answer_reverse(id, &method)?;
                    continue;
                }
                Ok(RawFrame::Closed(why)) => {
                    inner.last_obs = AcpTurnObservation::TransportLost(why);
                    return Err(AcpError::Closed);
                }
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) => {
                    inner.last_obs =
                        AcpTurnObservation::TransportLost("reader gone".to_string());
                    return Err(AcpError::Closed);
                }
            }
        }
    }
}

/// The host IS the SC-5 demux the completion probe reads (SC-2a): the latest terminal
/// observation, updated as `next_update` correlates terminals. The `--wait` loop wraps
/// this in an [`AcpTurnCompletion`](super::completion::AcpTurnCompletion).
impl AcpTurnObserver for AcpHost {
    fn observe(&self) -> AcpTurnObservation {
        self.inner.borrow().last_obs.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // classify() is the wire-law heart of the SC-5 reader — pin each branch so a
    // drift in frame discrimination reds (no live bridge needed: pure parsing).

    #[test]
    fn classify_session_update_notification() {
        let v = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "PONG"}}}
        });
        match classify(v).expect("classified") {
            RawFrame::Update { session, kind, .. } => {
                assert_eq!(session, "s1");
                assert_eq!(kind, "agent_message_chunk");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn classify_prompt_response_is_response_not_notification() {
        let v = json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}});
        match classify(v).expect("classified") {
            RawFrame::Response { id, result } => {
                assert_eq!(id, 3);
                assert_eq!(result.get("stopReason").unwrap(), "end_turn");
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn classify_error_response() {
        let v = json!({"jsonrpc": "2.0", "id": 4, "error": {"code": -32603, "message": "internalError"}});
        match classify(v).expect("classified") {
            RawFrame::ErrorResponse { id, message } => {
                assert_eq!(id, 4);
                assert_eq!(message, "internalError");
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn classify_reverse_request_has_method_and_id() {
        let v = json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {"x": 1}});
        match classify(v).expect("classified") {
            RawFrame::ReverseRequest { method, .. } => {
                assert_eq!(method, "session/request_permission")
            }
            other => panic!("expected ReverseRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_unknown_notification_is_ignored() {
        let v = json!({"jsonrpc": "2.0", "method": "some/unknown", "params": {}});
        assert!(classify(v).is_none(), "unknown notification → ignored");
    }
}
