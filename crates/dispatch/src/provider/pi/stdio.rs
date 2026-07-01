//! [`PiStdio`] — the real, in-daemon [`PiRpc`] driver: it OWNS the `pi --mode
//! rpc` child process and speaks pi's line-delimited JSONL over the child's
//! stdin/stdout (the [`super::rpc`] wire law). Structural mirror of
//! [`crate::provider::acp::client::AcpHost`] (child ownership + a single reader
//! thread feeding an mpsc) crossed with
//! [`crate::provider::codex::ws::WsAppServer`] (one-in-flight request, correlate
//! the response by the echoed command id).
//!
//! **Where it runs.** Inside the per-session pi adapter daemon (item 1, the
//! `pi-daemon` resident — [`super::residence`]). The resident owns ONE `PiStdio`
//! for the life of the session; qd verbs reach it across invocations through the
//! resident's loopback front (the [`super::rpc::PiRpc`] object at the *verb*
//! layer is a remote client, NOT this type — this type is the daemon-internal
//! truth that actually drives pi).
//!
//! **Why a reader thread.** pi interleaves correlated RESPONSES and bare EVENTS
//! on the SAME stdout stream ([`super::rpc::classify`]). A single owned reader
//! thread is the sole consumer of that stdout (the `AcpHost` SC-5 discipline):
//! it classifies every line and republishes it onto an mpsc bus. The driver
//! correlates a response by its echoed `id` and buffers events (served by
//! [`PiRpc::next_event`], and — in the resident — fed through
//! [`super::status::PiStatusMapper`] into the registry sink, item 2).
//!
//! **Interior mutability.** `&self` methods (not `&mut self`) so the resident can
//! hand the driver to `PiProvider::{boot_waiter, inject}` through a shared
//! `&ProviderFx` borrow (the W3 codex precedent). All mutable state lives behind
//! one [`RefCell`]; `!Sync` by design — one in-flight command at a time.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::rpc::{
    classify, Frame, PiEvent, PiRpc, PiRpcError, RpcSessionState, StreamingBehavior,
};

/// Default per-command read deadline — pi answers `get_state`/`prompt`-ack
/// promptly (PA1: max observed 788ms boot; a prompt ACK is the command echo, not
/// the turn). The boot waiter (`GetStateWaiter`) calls `get_state` under this.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// A reader-thread frame: the pure [`Frame`] kinds plus a transport-`Closed`
/// sentinel ([`classify`] itself has no EOF notion — that is an OS-level stdout
/// close on `pi`'s `process.exit`, the daemon-liveness signal).
#[derive(Debug)]
enum RawFrame {
    Response(super::rpc::RpcResponse),
    Event(PiEvent),
    /// stdout EOF or a read error — pi is gone (maps to [`PiRpcError::Closed`]).
    Closed(String),
}

/// The reader thread body: own `stdout`, classify each `\n`-delimited line, and
/// republish onto the bus. The ONE consumer of pi's stdout. Exits on EOF, a read
/// error, or the driver dropping the receiver. A non-JSON line (a stray pi log on
/// stdout) is tolerated by skipping it, never wedged (PA6 framing robustness).
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
                    // A torn / non-JSON line: skip, never wedge the stream.
                    Err(_) => continue,
                };
                let raw = match classify(&value) {
                    Some(Frame::Response(r)) => RawFrame::Response(r),
                    Some(Frame::Event(e)) => RawFrame::Event(e),
                    // Not an object (garbage) — skip.
                    None => continue,
                };
                if tx.send(raw).is_err() {
                    return; // driver dropped the receiver
                }
            }
            Err(_) => {
                let _ = tx.send(RawFrame::Closed("stdout read error".to_string()));
                return;
            }
        }
    }
}

// ===========================================================================
// Pure command framing (unit-testable; the I/O loop is RUN-not-read, item 7).
// ===========================================================================

/// Build the outbound JSON frame for a pi command. pi frames are BARE objects
/// keyed by `type` (no JSON-RPC envelope, no `jsonrpc`/`method`) carrying the
/// minted correlation `id` (the [`super::rpc`] wire law). Field names mined from
/// the 0.80.2 `rpc-types.d.ts` 29-command union (the WS-A.2 spike map); the
/// exact spelling is re-verified at first-compile + by the item-7 conformance
/// harness (RUN-not-read), never assumed green.
fn build_frame(id: &str, kind: &str, extra: &[(&str, Value)]) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".to_string(), Value::String(id.to_string()));
    obj.insert("type".to_string(), Value::String(kind.to_string()));
    for (k, v) in extra {
        obj.insert((*k).to_string(), v.clone());
    }
    Value::Object(obj)
}

/// `StreamingBehavior` → its on-wire camelCase string (`steer`/`followUp`),
/// reusing the [`StreamingBehavior`] `Serialize` so the spelling has ONE source.
fn behavior_field(behavior: StreamingBehavior) -> Value {
    serde_json::to_value(behavior).unwrap_or(Value::Null)
}

// ===========================================================================
// The driver.
// ===========================================================================

/// Mutable driver state, single-threaded behind one [`RefCell`].
struct StdioInner {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<RawFrame>,
    reader: Option<JoinHandle<()>>,
    /// Monotonic command-id counter (rendered `c{n}` — pi echoes it back on the
    /// correlated response; `prompt` returns it as the attributable turn id).
    next_id: u64,
    /// Events read while correlating a command response — served FIRST by
    /// [`PiRpc::next_event`] so nothing on the stream is lost.
    pending: VecDeque<PiEvent>,
    /// Latched once the reader reports EOF/read-error: every subsequent call
    /// short-circuits to [`PiRpcError::Closed`] (pi is gone).
    closed: Option<String>,
}

impl StdioInner {
    fn mint_id(&mut self) -> String {
        self.next_id += 1;
        format!("c{}", self.next_id)
    }

    /// Write one bare-JSON command frame + a newline (pi ndjson), then flush.
    fn write_frame(&mut self, frame: &Value) -> Result<(), PiRpcError> {
        if let Some(why) = &self.closed {
            return Err(PiRpcError::Transport(format!("pi closed: {why}")));
        }
        let mut text = serde_json::to_string(frame)
            .map_err(|e| PiRpcError::Transport(format!("serialize: {e}")))?;
        text.push('\n');
        self.stdin
            .write_all(text.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| PiRpcError::Transport(format!("write: {e}")))
    }

    /// Send a command and block until the response carrying the SAME `id`
    /// arrives, buffering any events seen meanwhile into `pending` and honoring
    /// `timeout`. Returns the response's `data` (or `None` for a bare success);
    /// a `success:false` is [`PiRpcError::Protocol`] (pi's stringy error).
    fn request(
        &mut self,
        kind: &str,
        extra: &[(&str, Value)],
        timeout: Duration,
    ) -> Result<(String, Option<Value>), PiRpcError> {
        let id = self.mint_id();
        let frame = build_frame(&id, kind, extra);
        self.write_frame(&frame)?;
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(PiRpcError::Timeout);
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(RawFrame::Response(r)) if r.id.as_deref() == Some(id.as_str()) => {
                    if r.success {
                        return Ok((id, r.data));
                    }
                    return Err(PiRpcError::Protocol(
                        r.error.unwrap_or_else(|| "pi reported success:false".to_string()),
                    ));
                }
                // An event seen while correlating — buffer it (FIFO), keep waiting.
                Ok(RawFrame::Event(e)) => self.pending.push_back(e),
                // A response for some OTHER id — one-in-flight means this should not
                // happen; drop it rather than wedge.
                Ok(RawFrame::Response(_)) => continue,
                Ok(RawFrame::Closed(why)) => {
                    self.closed = Some(why);
                    return Err(PiRpcError::Closed);
                }
                Err(RecvTimeoutError::Timeout) => return Err(PiRpcError::Timeout),
                Err(RecvTimeoutError::Disconnected) => {
                    self.closed = Some("reader gone".to_string());
                    return Err(PiRpcError::Closed);
                }
            }
        }
    }

    /// Pull the next buffered or freshly-read EVENT, blocking up to `timeout`.
    /// `Ok(None)` on a quiet deadline (a silent stream between turns is normal);
    /// [`PiRpcError::Closed`] on EOF with nothing buffered. A correlated response
    /// arriving here (no command in flight) is unexpected — dropped, not wedged.
    fn next_event(&mut self, timeout: Duration) -> Result<Option<PiEvent>, PiRpcError> {
        if let Some(e) = self.pending.pop_front() {
            return Ok(Some(e));
        }
        if let Some(why) = &self.closed {
            return Err(PiRpcError::Transport(format!("pi closed: {why}")));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(RawFrame::Event(e)) => return Ok(Some(e)),
                Ok(RawFrame::Response(_)) => continue, // no command in flight — drop
                Ok(RawFrame::Closed(why)) => {
                    self.closed = Some(why);
                    return Err(PiRpcError::Closed);
                }
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) => {
                    self.closed = Some("reader gone".to_string());
                    return Err(PiRpcError::Closed);
                }
            }
        }
    }
}

/// The live pi stdio driver: a `pi --mode rpc` child + its sole stdout reader +
/// the command-id counter + the event buffer. `!Sync` by design (one
/// [`RefCell`]); the `&self` methods are callable through a shared `&dyn PiRpc`.
pub struct PiStdio {
    inner: RefCell<StdioInner>,
    /// The per-command read deadline (raised by long-turn callers if needed).
    timeout: std::cell::Cell<Duration>,
}

impl PiStdio {
    /// Spawn `program args…` (e.g. `(<pinned pi bin>, ["--mode","rpc"])`) in
    /// `cwd` with `env` overrides layered on, stdin/stdout piped, stderr inherited
    /// (pi diagnostics → our stderr, never the protocol stream), and start the
    /// sole reader thread. Does NOT probe readiness — the boot waiter drives the
    /// first `get_state` (the `AcpHost::spawn` discipline: own first, handshake
    /// after).
    ///
    /// Spawned WITHOUT `process_group(0)` here: the *resident* ([`super::residence`])
    /// is the process-group leader (item 3) and this child inherits its group, so a
    /// group-scoped `-pgid` teardown reaps the resident + this pi child together
    /// (the codex/acp two-level teardown). A bare child kill would orphan pi's own
    /// `&`-detached grandchildren (PA11) — the group is the only correct scope.
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<PiStdio, PiRpcError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| PiRpcError::Transport(format!("spawn {program}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PiRpcError::Transport("pi stdin not piped".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PiRpcError::Transport("pi stdout not piped".to_string()))?;
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("pi-reader".to_string())
            .spawn(move || reader_loop(stdout, tx))
            .map_err(|e| PiRpcError::Transport(format!("reader thread: {e}")))?;
        Ok(PiStdio {
            inner: RefCell::new(StdioInner {
                child,
                stdin,
                rx,
                reader: Some(reader),
                next_id: 0,
                pending: VecDeque::new(),
                closed: None,
            }),
            timeout: std::cell::Cell::new(DEFAULT_COMMAND_TIMEOUT),
        })
    }

    /// Raise/lower the per-command read deadline (long-turn callers).
    pub fn set_timeout(&self, timeout: Duration) {
        self.timeout.set(timeout);
    }

    /// The pid of the owned pi child (the resident records the process-GROUP for
    /// teardown; this is the child within it).
    pub fn child_pid(&self) -> u32 {
        self.inner.borrow().child.id()
    }

    /// Best-effort teardown: kill+reap the pi child and join the reader. Idempotent.
    /// The reader's `read_line` hits EOF once stdout closes, so the join cannot hang.
    /// NOTE: this is the SINGLE-child path; the resident's pgid teardown (item 3) is
    /// the authoritative one that also reaps pi's grandchildren.
    pub fn shutdown(&self) {
        let mut inner = self.inner.borrow_mut();
        let _ = inner.child.kill();
        let _ = inner.child.wait();
        if let Some(h) = inner.reader.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PiStdio {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        let _ = inner.child.kill();
        let _ = inner.child.wait();
        if let Some(h) = inner.reader.take() {
            let _ = h.join();
        }
    }
}

impl PiRpc for PiStdio {
    fn get_state(&self) -> Result<RpcSessionState, PiRpcError> {
        let timeout = self.timeout.get();
        let (_id, data) = self.inner.borrow_mut().request("get_state", &[], timeout)?;
        // get_state's payload is in `data`; an absent/garbled data degrades to a
        // default state (permissive — readiness is the round-trip landing OK).
        match data {
            Some(v) => serde_json::from_value(v)
                .map_err(|e| PiRpcError::Protocol(format!("get_state data parse: {e}"))),
            None => Ok(RpcSessionState::default()),
        }
    }

    fn prompt(
        &self,
        message: &str,
        behavior: Option<StreamingBehavior>,
    ) -> Result<String, PiRpcError> {
        let timeout = self.timeout.get();
        let mut extra: Vec<(&str, Value)> = vec![("message", Value::String(message.to_string()))];
        if let Some(b) = behavior {
            extra.push(("streamingBehavior", behavior_field(b)));
        }
        // pi's prompt response carries NO turn id — the minted command id IS the
        // attributable turn id (the `Provider::inject` contract).
        let (id, _data) = self.inner.borrow_mut().request("prompt", &extra, timeout)?;
        Ok(id)
    }

    fn steer(&self, message: &str) -> Result<(), PiRpcError> {
        let timeout = self.timeout.get();
        self.inner
            .borrow_mut()
            .request("steer", &[("message", Value::String(message.to_string()))], timeout)
            .map(|_| ())
    }

    fn follow_up(&self, message: &str) -> Result<(), PiRpcError> {
        let timeout = self.timeout.get();
        self.inner
            .borrow_mut()
            .request("follow_up", &[("message", Value::String(message.to_string()))], timeout)
            .map(|_| ())
    }

    fn abort(&self) -> Result<(), PiRpcError> {
        let timeout = self.timeout.get();
        self.inner.borrow_mut().request("abort", &[], timeout).map(|_| ())
    }

    fn switch_session(&self, session_path: &str) -> Result<(), PiRpcError> {
        let timeout = self.timeout.get();
        self.inner
            .borrow_mut()
            .request(
                "switch_session",
                &[("sessionPath", Value::String(session_path.to_string()))],
                timeout,
            )
            .map(|_| ())
    }

    fn new_session(&self, parent: Option<&str>) -> Result<(), PiRpcError> {
        let timeout = self.timeout.get();
        let extra: Vec<(&str, Value)> = match parent {
            Some(p) => vec![("parentSession", Value::String(p.to_string()))],
            None => vec![],
        };
        self.inner.borrow_mut().request("new_session", &extra, timeout).map(|_| ())
    }

    fn next_event(&self, timeout: Duration) -> Result<Option<PiEvent>, PiRpcError> {
        self.inner.borrow_mut().next_event(timeout)
    }

    fn close(&self) -> Result<(), PiRpcError> {
        // Best-effort: shut down the child + reader. A double-close is harmless.
        self.shutdown();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The framing is the pure, offline-testable surface (the I/O loop is driven
    // RUN-not-read by the item-7 conformance harness against a live pi).

    #[test]
    fn build_frame_is_a_bare_typed_object_with_id() {
        let f = build_frame("c1", "get_state", &[]);
        assert_eq!(f["id"], json!("c1"));
        assert_eq!(f["type"], json!("get_state"));
        // No JSON-RPC envelope leaks in (the pi wire law).
        assert!(f.get("jsonrpc").is_none());
        assert!(f.get("method").is_none());
    }

    #[test]
    fn build_frame_carries_extra_fields() {
        let f = build_frame(
            "c2",
            "prompt",
            &[
                ("message", json!("hello")),
                ("streamingBehavior", json!("steer")),
            ],
        );
        assert_eq!(f["type"], json!("prompt"));
        assert_eq!(f["message"], json!("hello"));
        assert_eq!(f["streamingBehavior"], json!("steer"));
        assert_eq!(f["id"], json!("c2"));
    }

    #[test]
    fn behavior_field_serializes_camel_case() {
        assert_eq!(behavior_field(StreamingBehavior::Steer), json!("steer"));
        assert_eq!(behavior_field(StreamingBehavior::FollowUp), json!("followUp"));
    }
}
