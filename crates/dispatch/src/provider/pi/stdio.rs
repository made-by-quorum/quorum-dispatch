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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::rpc::{classify, Frame, PiEvent, PiRpc, PiRpcError, RpcSessionState, StreamingBehavior};

/// Default per-command read deadline — pi answers `get_state`/`prompt`-ack
/// promptly (PA1: max observed 788ms boot; a prompt ACK is the command echo, not
/// the turn). The boot waiter (`GetStateWaiter`) calls `get_state` under this.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Max raw frames queued between the stdout reader thread and the driver.
///
/// This mirrors `MAX_PENDING_EVENTS`: enough slack for bursts behind the
/// resident's bounded drain, but fixed so sustained above-ceiling output cannot
/// grow memory without limit. The reader uses nonblocking `try_send`, so a full
/// channel drops frames instead of stalling pi's stdout.
pub(crate) const CHANNEL_CAPACITY: usize = 4096;

/// Max pi events retained while a command response is being correlated.
///
/// The resident currently drains and discards pi events as buffer hygiene. While a
/// front request is in-flight, though, events can arrive before the correlated
/// response and must not migrate into an unbounded side buffer. Retained events
/// keep FIFO order; overflow events are dropped until the response lands and the
/// regular resident drain resumes.
const MAX_PENDING_EVENTS: usize = 4096;

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

struct DrainedEvents {
    frames: usize,
    events: Vec<PiEvent>,
    error: Option<PiRpcError>,
}

/// The reader thread body: own `stdout`, classify each `\n`-delimited line, and
/// republish onto the bus. The ONE consumer of pi's stdout. Exits on EOF, a read
/// error, or the driver dropping the receiver. A non-JSON line (a stray pi log on
/// stdout) is tolerated by skipping it, never wedged (PA6 framing robustness).
fn reader_loop(
    stdout: ChildStdout,
    tx: SyncSender<RawFrame>,
    received: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = tx.try_send(RawFrame::Closed("stdout eof".to_string()));
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
                received.fetch_add(1, Ordering::Relaxed);
                match tx.try_send(raw) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    Err(TrySendError::Disconnected(_)) => return, // driver dropped the receiver
                }
            }
            Err(_) => {
                let _ = tx.try_send(RawFrame::Closed("stdout read error".to_string()));
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

    fn push_pending_event(&mut self, event: PiEvent) {
        if self.pending.len() < MAX_PENDING_EVENTS {
            self.pending.push_back(event);
        }
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
                        r.error
                            .unwrap_or_else(|| "pi reported success:false".to_string()),
                    ));
                }
                // An event seen while correlating — buffer it (FIFO), keep waiting.
                Ok(RawFrame::Event(e)) => self.push_pending_event(e),
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

    /// Consume buffered pi frames without blocking, bounded by `max_frames`.
    ///
    /// Pending events are yielded before fresh channel frames, matching
    /// `next_event`'s FIFO contract for events captured during request
    /// correlation. Responses with no command in flight are discarded, as in
    /// `next_event`.
    fn drain_events(&mut self, max_frames: usize) -> DrainedEvents {
        let mut result = DrainedEvents {
            frames: 0,
            events: Vec::new(),
            error: None,
        };

        if max_frames == 0 {
            return result;
        }

        while result.frames < max_frames {
            if let Some(event) = self.pending.pop_front() {
                result.events.push(event);
                result.frames += 1;
                continue;
            }

            if let Some(why) = &self.closed {
                result.error = Some(PiRpcError::Transport(format!("pi closed: {why}")));
                return result;
            }

            match self.rx.try_recv() {
                Ok(RawFrame::Event(event)) => {
                    result.events.push(event);
                    result.frames += 1;
                }
                Ok(RawFrame::Response(_)) => {
                    result.frames += 1;
                }
                Ok(RawFrame::Closed(why)) => {
                    self.closed = Some(why);
                    result.error = Some(PiRpcError::Closed);
                    return result;
                }
                Err(TryRecvError::Empty) => return result,
                Err(TryRecvError::Disconnected) => {
                    self.closed = Some("reader gone".to_string());
                    result.error = Some(PiRpcError::Closed);
                    return result;
                }
            }
        }

        result
    }
}

/// The live pi stdio driver: a `pi --mode rpc` child + its sole stdout reader +
/// the command-id counter + the event buffer. `!Sync` by design (one
/// [`RefCell`]); the `&self` methods are callable through a shared `&dyn PiRpc`.
pub struct PiStdio {
    inner: RefCell<StdioInner>,
    /// The per-command read deadline (raised by long-turn callers if needed).
    timeout: std::cell::Cell<Duration>,
    /// Frames the reader thread classified and attempted to publish (diagnostic
    /// only — see [`PiStdio::frame_counts`]).
    frames_received: Arc<AtomicUsize>,
    /// Frames dropped because the bounded channel was full at `try_send` time
    /// (the `CHANNEL_CAPACITY` drop-on-full path, diagnostic only).
    frames_dropped: Arc<AtomicUsize>,
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
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let frames_received = Arc::new(AtomicUsize::new(0));
        let frames_dropped = Arc::new(AtomicUsize::new(0));
        let reader = std::thread::Builder::new()
            .name("pi-reader".to_string())
            .spawn({
                let received = Arc::clone(&frames_received);
                let dropped = Arc::clone(&frames_dropped);
                move || reader_loop(stdout, tx, received, dropped)
            })
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
            frames_received,
            frames_dropped,
        })
    }

    /// Diagnostic snapshot: `(frames the reader thread classified and attempted
    /// to publish, frames dropped because `CHANNEL_CAPACITY` was full)`. Not used
    /// by any control path — read this to measure real drop-on-full behavior
    /// under load (B2 live-dogfood).
    pub fn frame_counts(&self) -> (usize, usize) {
        (
            self.frames_received.load(Ordering::Relaxed),
            self.frames_dropped.load(Ordering::Relaxed),
        )
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

    /// Nonblocking resident drain of buffered pi output.
    ///
    /// The callback is the retained status/republish seam: today's resident
    /// discards events, while a future consumer can map them without changing the
    /// stdio ownership or channel-drain mechanics.
    pub fn drain_events<F>(&self, max_frames: usize, on_event: F) -> Result<usize, PiRpcError>
    where
        F: FnMut(PiEvent),
    {
        let drained = self.inner.borrow_mut().drain_events(max_frames);
        drained.events.into_iter().for_each(on_event);
        match drained.error {
            Some(error) => Err(error),
            None => Ok(drained.frames),
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
            .request(
                "steer",
                &[("message", Value::String(message.to_string()))],
                timeout,
            )
            .map(|_| ())
    }

    fn follow_up(&self, message: &str) -> Result<(), PiRpcError> {
        let timeout = self.timeout.get();
        self.inner
            .borrow_mut()
            .request(
                "follow_up",
                &[("message", Value::String(message.to_string()))],
                timeout,
            )
            .map(|_| ())
    }

    fn abort(&self) -> Result<(), PiRpcError> {
        let timeout = self.timeout.get();
        self.inner
            .borrow_mut()
            .request("abort", &[], timeout)
            .map(|_| ())
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
        self.inner
            .borrow_mut()
            .request("new_session", &extra, timeout)
            .map(|_| ())
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
        assert_eq!(
            behavior_field(StreamingBehavior::FollowUp),
            json!("followUp")
        );
    }

    fn spawn_shell_fixture(script: &str) -> PiStdio {
        PiStdio::spawn(
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            &std::env::temp_dir(),
            &[],
        )
        .expect("spawn shell fixture")
    }

    #[test]
    fn drain_events_consumes_pending_before_channel_frames_with_cap() {
        let script = r#"
printf '{"type":"agent_start"}\n'
printf '{"id":"c1","type":"response","command":"get_state","success":true,"data":{"sessionId":"stub","isStreaming":false}}\n'
printf '{"type":"agent_end","willRetry":false}\n'
cat >/dev/null
"#;
        let pi = spawn_shell_fixture(script);
        pi.set_timeout(Duration::from_secs(2));

        let state = pi.get_state().expect("fixture get_state");
        assert_eq!(state.session_id, "stub");

        let mut first = Vec::new();
        let drained = pi
            .drain_events(1, |event| first.push(event))
            .expect("drain pending event");
        assert_eq!(drained, 1);
        assert!(matches!(first.as_slice(), [PiEvent::AgentStart]));

        std::thread::sleep(Duration::from_millis(20));
        let mut second = Vec::new();
        let drained = pi
            .drain_events(1, |event| second.push(event))
            .expect("drain channel event");
        assert_eq!(drained, 1);
        assert!(matches!(
            second.as_slice(),
            [PiEvent::AgentEnd { will_retry: false }]
        ));

        let _ = pi.close();
    }

    #[test]
    fn drain_events_callback_may_reenter_pi_stdio() {
        let script = r#"
printf '{"id":"c1","type":"response","command":"get_state","success":true,"data":{"sessionId":"stub","isStreaming":false}}\n'
printf '{"type":"agent_start"}\n'
cat >/dev/null
"#;
        let pi = spawn_shell_fixture(script);
        pi.set_timeout(Duration::from_secs(2));

        let state = pi.get_state().expect("fixture get_state");
        assert_eq!(state.session_id, "stub");
        std::thread::sleep(Duration::from_millis(20));

        let expected_pid = pi.child_pid();
        let mut callback_pid = None;
        let drained = pi
            .drain_events(1, |event| {
                assert!(matches!(event, PiEvent::AgentStart));
                callback_pid = Some(pi.child_pid());
            })
            .expect("drain event with reentrant callback");
        assert_eq!(drained, 1);
        assert_eq!(callback_pid, Some(expected_pid));

        let _ = pi.close();
    }

    // =======================================================================
    // B2 red-team round 1 — PROVOKED response-drop-on-full.
    //
    // `reader_loop`'s try_send (line ~119) treats `RawFrame::Response` and
    // `RawFrame::Event` identically: if the channel is full at the instant pi's
    // correlated response for an in-flight `request()` arrives, that response is
    // silently dropped exactly like an event would be. This test provokes that
    // exact race (not just reasons about it) and observes the outcome at the
    // driver, per the B2 standard's drop-on-full clause.
    //
    // Construction: a burst of CHANNEL_CAPACITY + slack `agent_start` events with
    // NO consumer draining yet (the test hasn't called `get_state` — nobody is
    // pulling from `rx`) deterministically fills the bounded channel; every frame
    // emitted after the first CHANNEL_CAPACITY successes free-falls into
    // `TrySendError::Full` and is dropped. Placing the correlated `c1` response
    // as the LAST line of that burst guarantees it lands in the dropped tail, not
    // by luck but by construction — the channel is provably full from the first
    // CHANNEL_CAPACITY events onward, and nothing drains until the test thread
    // calls `get_state`.
    #[test]
    fn response_dropped_on_full_channel_is_a_clean_bounded_timeout_not_a_hang_or_wedge() {
        let burst = CHANNEL_CAPACITY + 2000;
        let script = format!(
            r#"
i=0
while [ "$i" -lt {burst} ]; do printf '{{"type":"agent_start"}}\n'; i=$((i+1)); done
printf '{{"id":"c1","type":"response","command":"get_state","success":true,"data":{{"sessionId":"dropped-response","isStreaming":false}}}}\n'
sleep 3
printf '{{"type":"agent_start"}}\n'
printf '{{"id":"c2","type":"response","command":"get_state","success":true,"data":{{"sessionId":"second-command-ok","isStreaming":false}}}}\n'
cat >/dev/null
"#
        );
        let pi = spawn_shell_fixture(&script);

        // Let the whole (sleep-free) burst — CHANNEL_CAPACITY successes then a
        // long run of TrySendError::Full drops, including the c1 response —
        // finish landing in the channel/dropped before anyone drains it.
        std::thread::sleep(Duration::from_millis(600));

        // c1: mint_id() -> "c1". Its correlated response was dropped, so this
        // must resolve as a clean, bounded Timeout — not hang past the deadline.
        pi.set_timeout(Duration::from_millis(900));
        let start = Instant::now();
        let first = pi.get_state();
        let elapsed = start.elapsed();
        assert!(
            matches!(first, Err(PiRpcError::Timeout)),
            "expected a clean Timeout on the dropped c1 response, got {first:?}"
        );
        // Bounded: resolves at/near the declared deadline, well short of the
        // script's 3s sleep — proof this is a timeout, not a stall waiting on
        // something that never comes back on its own.
        assert!(
            elapsed < Duration::from_millis(1500),
            "get_state took {elapsed:?} — expected a bounded ~900ms timeout, not a hang"
        );

        let (_received, dropped) = pi.frame_counts();
        assert!(
            dropped > 0,
            "expected the reader thread to report dropped frames from the burst"
        );

        // Not wedged: a SECOND command (c2, whose response survives because by
        // now the test thread's own drain during the c1 wait emptied the
        // channel) must resolve cleanly and correlate to the RIGHT id — ruling
        // out id misattribution, latched-closed state, or a corrupted driver.
        pi.set_timeout(Duration::from_secs(5));
        let state = pi
            .get_state()
            .expect("driver must not be wedged after a dropped-response timeout");
        assert_eq!(state.session_id, "second-command-ok");

        let _ = pi.close();
    }

    // =======================================================================
    // B2 red-team round 2 — check 1: the SEND path specifically.
    //
    // Round 1 provoked the drop-on-full race via `get_state()`. `get_state` and
    // `prompt` share the exact same correlation code — `StdioInner::request`
    // (line ~213) matches a `RawFrame::Response` purely on `r.id` (line ~229),
    // never on the `command` field — so nothing in `request()` distinguishes a
    // dropped `get_state` ack from a dropped `prompt` ack. This test proves that
    // directly, on `prompt()` itself (the actual `PiProvider::inject` send
    // call, stdio.rs:502-516), rather than relying on that read-the-source
    // argument alone.
    #[test]
    fn prompt_response_dropped_on_full_channel_is_a_clean_bounded_timeout_not_a_hang_or_wedge() {
        let burst = CHANNEL_CAPACITY + 2000;
        let script = format!(
            r#"
i=0
while [ "$i" -lt {burst} ]; do printf '{{"type":"agent_start"}}\n'; i=$((i+1)); done
printf '{{"id":"c1","type":"response","command":"prompt","success":true,"data":null}}\n'
sleep 3
printf '{{"type":"agent_start"}}\n'
printf '{{"id":"c2","type":"response","command":"get_state","success":true,"data":{{"sessionId":"second-command-ok","isStreaming":false}}}}\n'
cat >/dev/null
"#
        );
        let pi = spawn_shell_fixture(&script);

        std::thread::sleep(Duration::from_millis(600));

        // c1: mint_id() -> "c1" on THIS fresh PiStdio's first command, which is
        // this very prompt() call. Its correlated ack was dropped in the burst,
        // so the send call itself must resolve as a clean, bounded Timeout —
        // not a hang — proving the race hits the send path, not just a
        // diagnostic side command.
        pi.set_timeout(Duration::from_millis(900));
        let start = Instant::now();
        let first = pi.prompt("hello", None);
        let elapsed = start.elapsed();
        assert!(
            matches!(first, Err(PiRpcError::Timeout)),
            "expected a clean Timeout on the dropped prompt-ack, got {first:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "prompt() took {elapsed:?} — expected a bounded ~900ms timeout, not a hang"
        );

        let (_received, dropped) = pi.frame_counts();
        assert!(
            dropped > 0,
            "expected the reader thread to report dropped frames from the burst"
        );

        // Not wedged: a second, different command still correlates by id.
        pi.set_timeout(Duration::from_secs(5));
        let state = pi
            .get_state()
            .expect("driver must not be wedged after a dropped prompt-ack timeout");
        assert_eq!(state.session_id, "second-command-ok");

        let _ = pi.close();
    }

    #[test]
    fn request_time_pending_events_are_capped() {
        let script = format!(
            r#"
i=0
while [ "$i" -lt {events} ]; do
  printf '{{"type":"agent_start"}}\n'
  i=$((i + 1))
done
printf '{{"id":"c1","type":"response","command":"get_state","success":true,"data":{{"sessionId":"stub","isStreaming":false}}}}\n'
cat >/dev/null
"#,
            events = MAX_PENDING_EVENTS + 5
        );
        let pi = spawn_shell_fixture(&script);
        pi.set_timeout(Duration::from_secs(2));

        let state = pi.get_state().expect("fixture get_state");
        assert_eq!(state.session_id, "stub");

        let drained = pi
            .drain_events(MAX_PENDING_EVENTS + 10, |_| {})
            .expect("drain capped pending events");
        assert_eq!(drained, MAX_PENDING_EVENTS);

        let _ = pi.close();
    }
}
