//! Headless launch + drain-fast/decoupled/bounded reader (WP-B2a) — wires the
//! pure WP-B1 [`stream_json`](crate::stream_json) parser to a real `claude -p
//! --output-format stream-json --verbose` child on PIPES (not a PTY), alongside
//! the existing interactive PTY path ([`crate::pty`]).
//!
//! Realizes the §2.3 binding contract (the core of this WP):
//! - **Drain-fast:** a dedicated reader thread pulls the child's stdout as fast
//!   as the kernel delivers it, feeds [`StreamParser::push`], and pushes the
//!   resulting [`StreamEvent`]s into an internal **bounded queue**. The reader
//!   NEVER blocks on a slow downstream sink (spike-1 Q3: a >64 KB turn behind a
//!   stalled reader blocks `claude` on EAGAIN) and NEVER closes the read end
//!   mid-turn (EPIPE).
//! - **Decoupled fan-out:** a SEPARATE pump thread drains the bounded queue into
//!   the [`Sink`]. A stalled/closed sink stalls only the pump, never the reader.
//! - **Bounded:** on queue overflow we drop *coalescible* status updates
//!   (`SystemInit`/`Assistant`/`RateLimit`/`Other`) — keeping the latest — but
//!   **NEVER drop a `result` event** (§2.3 / §H.5).
//! - **Circuit-breaker → child-kill (§H.5):** when the parser emits
//!   [`StreamEvent::CircuitBreaker`] (a pathological never-terminating line),
//!   the reader **kills the child** (the parser only signals; WP-B2 owns the
//!   actual kill — that owner is here).
//!
//! Everything is unit-testable over a FAKE pipe (any [`Read`]) + a FAKE
//! [`Sink`] + a FAKE [`Child`]; only the one `#[ignore]`d integration test
//! launches a real isolated `claude`.

use crate::stream_json::{
    ResultEvent, StreamEvent, StreamParser, TurnOutcome, DEFAULT_MAX_LINE_BYTES,
};
use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child as StdChild, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// The three facts the republish sink carries (§0/§2.4). In-process for this WP;
/// socket fan-out + registry-write are WP-B2b, behind this trait boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum Republish {
    /// Readiness: the first event of a turn (`system/init`) — producer is up and
    /// has bound its identity (`session_id`). Known before turn completion.
    Ready { session_id: String },
    /// Raw event passthrough (assistant content, rate-limit, unknown types) —
    /// the coalescible status stream. A consumer may log/forward these.
    Event(StreamEvent),
    /// Turn-end: the `result` event. NEVER dropped on overflow (§2.3).
    TurnEnd(ResultEvent),
    /// Circuit-breaker fired: the child was (or is being) killed. Terminal.
    Breaker {
        cap_bytes: usize,
        observed_bytes: usize,
    },
    /// EOF: the child's stdout closed. Carries the in-flight turn's outcome.
    Eof(TurnOutcome),
    /// Daemon-driven wake (R3c-Step-1): the per-session control socket received a
    /// `WakeInbox` and asked the daemon to nudge a PARKED headless session. Injected
    /// out-of-band via [`HeadlessReader::enqueue`] (NOT from the child's stdout), it
    /// rides the SAME pump → sink path so the daemon-status sink advances signal-B
    /// (`note_output`) — the observable "the daemon drove the wake" evidence — and
    /// the socket fan-out treats it as a coalescible passthrough. Never a must-keep
    /// (a wake is droppable under overload; the ladder's Rung-2 re-send is the
    /// backstop).
    Wake,
}

/// The republish sink (in-process for WP-B2a). A clean trait so WP-B2b can swap
/// in the socket fan-out + registry-writer without touching the reader/pump.
///
/// `deliver` MAY block (a slow socket client) — that is exactly why it lives
/// behind the decoupled pump thread, never on the reader's hot path.
pub trait Sink: Send {
    fn deliver(&self, msg: Republish);
}

/// Boxed sinks ARE sinks (WP-B2b-2b): the daemon's registry-status half of the
/// composite is injected qd-side as a `Box<dyn Sink>` (qrmux cannot name
/// `RegistryStatusSink`), so it must compose into [`Fanout`] like any other sink.
impl Sink for Box<dyn Sink> {
    fn deliver(&self, msg: Republish) {
        (**self).deliver(msg);
    }
}

/// A pure fan-out combinator (WP-B2b-2a §C, design §C layer-1): deliver one
/// [`Republish`] to BOTH `a` then `b`. Composed on the `qd` side as
/// `Fanout { a: RegistryStatusSink, b: SocketFanoutSink }` so a single reader/pump
/// drives both the registry-status write AND the socket republish — neither sink
/// can be referenced from the other's crate. `Republish: Clone` (above) lets us
/// hand each side its own copy. Order is fixed: `a` first, then `b`.
pub struct Fanout<A, B> {
    pub a: A,
    pub b: B,
}

impl<A: Sink, B: Sink> Sink for Fanout<A, B> {
    fn deliver(&self, msg: Republish) {
        self.a.deliver(msg.clone());
        self.b.deliver(msg);
    }
}

#[cfg(test)]
mod fanout_tests {
    use super::*;
    use crate::stream_json::TurnOutcome;
    use std::sync::Mutex;

    /// A recording sink for the fan-out unit test (mirrors the headless test
    /// `RecordingSink`, minimal here to keep the combinator self-contained).
    struct Recorder {
        msgs: std::sync::Arc<Mutex<Vec<Republish>>>,
    }
    impl Sink for Recorder {
        fn deliver(&self, msg: Republish) {
            self.msgs.lock().unwrap().push(msg);
        }
    }

    #[test]
    fn fanout_delivers_to_both_sinks_in_order() {
        let a_msgs = std::sync::Arc::new(Mutex::new(Vec::new()));
        let b_msgs = std::sync::Arc::new(Mutex::new(Vec::new()));
        let fan = Fanout {
            a: Recorder {
                msgs: a_msgs.clone(),
            },
            b: Recorder {
                msgs: b_msgs.clone(),
            },
        };

        let seq = vec![
            Republish::Ready {
                session_id: "s1".into(),
            },
            Republish::Event(StreamEvent::SystemInit {
                session_id: "s1".into(),
            }),
            Republish::Eof(TurnOutcome::Completed),
        ];
        for m in &seq {
            fan.deliver(m.clone());
        }

        let a = a_msgs.lock().unwrap().clone();
        let b = b_msgs.lock().unwrap().clone();
        assert_eq!(a, seq, "sink A must receive every delivered msg in order");
        assert_eq!(b, seq, "sink B must receive every delivered msg in order");
    }
}

/// The child-process abstraction — so the breaker→kill path is deterministically
/// testable with a fake child (no real process needed). The real impl wraps a
/// `std::process::Child`.
pub trait Child: Send {
    /// Kill the child (best-effort; SIGKILL semantics). Idempotent: a second
    /// call after the child is gone is a no-op-or-error we swallow.
    fn kill(&mut self);
    /// True once the child has been observed killed by this handle (test hook +
    /// idempotency guard).
    fn was_killed(&self) -> bool;
}

/// Real child: a `claude` process launched on pipes. `kill()` sends SIGKILL via
/// `std::process::Child::kill` (the §H.5 child-kill).
pub struct RealChild {
    inner: StdChild,
    killed: bool,
}

impl RealChild {
    /// The OS pid of the spawned `claude` process. WP-B5-i (identity option B):
    /// the daemon spawned `claude` DIRECTLY (`HeadlessLaunch::spawn` →
    /// `Command::new(claude_bin)`, no shell wrapper), so this pid IS claude's own
    /// `getpid()` — the durable child key the registry row is minted on and the
    /// `(pid, /proc)` liveness classifier reads. Returned as `i64` to match the
    /// registry row's `pid` type.
    pub fn pid(&self) -> i64 {
        self.inner.id() as i64
    }
}

impl Child for RealChild {
    fn kill(&mut self) {
        if !self.killed {
            // Best-effort: an already-exited child returns InvalidInput; ignore.
            let _ = self.inner.kill();
            let _ = self.inner.wait();
            self.killed = true;
        }
    }
    fn was_killed(&self) -> bool {
        self.killed
    }
}

/// Bounded internal queue (§2.3): a `result` is NEVER dropped; coalescible
/// status events are dropped (oldest-coalescible-first) when the queue is at
/// cap. Drained by the pump thread.
struct BoundedQueue {
    inner: Mutex<QueueState>,
    not_empty: Condvar,
}

struct QueueState {
    items: VecDeque<Republish>,
    cap: usize,
    closed: bool,
    dropped: usize,
}

impl BoundedQueue {
    fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(QueueState {
                items: VecDeque::new(),
                cap: cap.max(1),
                closed: false,
                dropped: 0,
            }),
            not_empty: Condvar::new(),
        }
    }

    /// Push a message. NEVER blocks the caller (the reader). On overflow drop a
    /// coalescible item to make room; a `TurnEnd`/`Breaker`/`Eof` is admitted
    /// even past cap (it must never be lost), but we first evict coalescibles.
    fn push(&self, msg: Republish) {
        let mut st = self.inner.lock().expect("queue mutex poisoned");
        if st.closed {
            return;
        }
        let must_keep = matches!(
            msg,
            Republish::TurnEnd(_)
                | Republish::Breaker { .. }
                | Republish::Eof(_)
                | Republish::Ready { .. }
        );
        if st.items.len() >= st.cap {
            // Make room by evicting the OLDEST coalescible status event
            // (`Republish::Event`) — this both bounds the queue AND keeps the
            // LATEST status (§2.3: "keep the latest liveness/turn-state"). A
            // must-keep (`TurnEnd`/`Breaker`/`Eof`/`Ready`) is never evicted.
            if let Some(pos) = st
                .items
                .iter()
                .position(|m| matches!(m, Republish::Event(_)))
            {
                st.items.remove(pos);
                st.dropped += 1;
            } else if !must_keep {
                // No coalescible to evict and the incoming item is itself
                // coalescible: drop the incoming one (the queue is full of
                // must-keeps). Bounds growth deterministically.
                st.dropped += 1;
                return;
            }
            // else: incoming is a must-keep and the queue is all must-keeps —
            // admit it anyway (never lose a `result`). In practice there is one
            // `result` per turn, so this soft-overshoot is negligible.
        }
        st.items.push_back(msg);
        self.not_empty.notify_one();
    }

    /// Block until an item is available or the queue is closed-and-drained.
    /// Returns `None` only when closed AND empty.
    fn pop(&self) -> Option<Republish> {
        let mut st = self.inner.lock().expect("queue mutex poisoned");
        loop {
            if let Some(m) = st.items.pop_front() {
                return Some(m);
            }
            if st.closed {
                return None;
            }
            st = self.not_empty.wait(st).expect("queue mutex poisoned");
        }
    }

    fn close(&self) {
        let mut st = self.inner.lock().expect("queue mutex poisoned");
        st.closed = true;
        self.not_empty.notify_all();
    }

    fn dropped(&self) -> usize {
        self.inner.lock().expect("queue mutex poisoned").dropped
    }

    fn len(&self) -> usize {
        self.inner.lock().expect("queue mutex poisoned").items.len()
    }
}

/// Default bound on the internal queue (number of buffered [`Republish`] items).
/// Coalescible status beyond this is dropped; a `result` is never dropped.
pub const DEFAULT_QUEUE_CAP: usize = 1024;

/// A running headless session: the reader + pump threads draining a child's
/// stdout into the [`Sink`]. Drop joins both threads (never closes the read end
/// mid-turn by construction — the reader runs to EOF or breaker).
pub struct HeadlessReader {
    reader: Option<JoinHandle<TurnOutcome>>,
    pump: Option<JoinHandle<()>>,
    queue: Arc<BoundedQueue>,
    breaker_fired: Arc<AtomicBool>,
}

impl HeadlessReader {
    /// Start draining `stdout` (any [`Read`] — a real pipe or a fake one),
    /// feeding the parser, decoupling fan-out to `sink` behind a bounded queue
    /// of `queue_cap`, and killing `child` on the circuit-breaker.
    ///
    /// This is the §2.3 reader. The reader thread runs to EOF or breaker; it is
    /// NEVER gated on the sink (decoupled pump) and NEVER closes the read end
    /// early. Returns immediately; join via [`Self::join`].
    pub fn start<R, S, C>(
        stdout: R,
        sink: S,
        child: C,
        queue_cap: usize,
        max_line_bytes: usize,
    ) -> Self
    where
        R: Read + Send + 'static,
        S: Sink + 'static,
        C: Child + 'static,
    {
        let queue = Arc::new(BoundedQueue::new(queue_cap));
        let breaker_fired = Arc::new(AtomicBool::new(false));

        // Pump thread: drains the queue into the sink. A slow/closed sink stalls
        // ONLY this thread.
        let pump = {
            let queue = queue.clone();
            std::thread::Builder::new()
                .name("headless-pump".into())
                .spawn(move || {
                    while let Some(msg) = queue.pop() {
                        sink.deliver(msg);
                    }
                })
                .expect("spawn pump thread")
        };

        // Reader thread: drains stdout fast, parses, enqueues. Kills the child
        // on the breaker. Owns the child handle.
        let reader = {
            let queue = queue.clone();
            let breaker_fired = breaker_fired.clone();
            std::thread::Builder::new()
                .name("headless-reader".into())
                .spawn(move || reader_loop(stdout, queue, child, breaker_fired, max_line_bytes))
                .expect("spawn reader thread")
        };

        Self {
            reader: Some(reader),
            pump: Some(pump),
            queue,
            breaker_fired,
        }
    }

    /// True once the circuit-breaker fired (the child was killed).
    pub fn breaker_fired(&self) -> bool {
        self.breaker_fired.load(Ordering::Acquire)
    }

    /// Enqueue an out-of-band [`Republish`] into the SAME bounded queue the reader
    /// feeds (R3c-Step-1 confirmed seam, point 4). The ONLY non-reader producer; it
    /// delegates to the private `BoundedQueue::push` (never blocks the caller). Used
    /// by the daemon's control-socket arm to inject [`Republish::Wake`].
    pub fn enqueue(&self, msg: Republish) {
        self.queue.push(msg);
    }

    /// A cloneable, lock-free handle that injects a wake into this session's queue
    /// WITHOUT borrowing the session (so the daemon's `tokio::select!` arm can clone
    /// it under the `manager` lock, RELEASE the lock, then wake — the keystone that
    /// keeps a real wedge from starving the select loop, R3c-Step-0 point 2).
    pub fn wake_handle(&self) -> HeadlessWake {
        HeadlessWake {
            queue: self.queue.clone(),
        }
    }

    /// True while the reader thread is still draining the child (the turn is in
    /// flight). False once the reader has run to EOF/breaker — the signal the
    /// daemon's `is_alive()`/reaper use to know a headless turn has completed.
    /// (WP-B2b-2b: the load-bearing liveness fact for the parallel `headless` map.)
    pub fn is_running(&self) -> bool {
        self.reader.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Coalescible status events dropped on overflow (a `result` is never here).
    pub fn dropped(&self) -> usize {
        self.queue.dropped()
    }

    /// Current internal-queue depth (for the bounded-cap proof).
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Join the reader + pump threads. Returns the in-flight turn's
    /// [`TurnOutcome`] at EOF.
    pub fn join(mut self) -> TurnOutcome {
        let outcome = self
            .reader
            .take()
            .map(|h| h.join().expect("reader thread panicked"))
            .unwrap_or(TurnOutcome::NoTurn);
        if let Some(p) = self.pump.take() {
            let _ = p.join();
        }
        outcome
    }
}

/// A cloneable wake handle over a headless session's bounded queue (R3c-Step-1).
///
/// Obtained from [`HeadlessReader::wake_handle`] / [`crate::headless_session::
/// HeadlessSession::wake_handle`]. Cloning is cheap (an `Arc` bump). [`Self::wake`]
/// pushes a [`Republish::Wake`] and NEVER blocks — so the daemon clones this under
/// the `manager` lock, releases the lock, and only THEN wakes (the no-mutex-across-
/// the-wake keystone, R3c-Step-0 point 2).
#[derive(Clone)]
pub struct HeadlessWake {
    queue: Arc<BoundedQueue>,
}

impl HeadlessWake {
    /// Inject a [`Republish::Wake`] into the session's queue. Non-blocking,
    /// best-effort (dropped under queue overload — the ladder re-sends).
    pub fn wake(&self) {
        self.queue.push(Republish::Wake);
    }
}

impl Drop for HeadlessReader {
    fn drop(&mut self) {
        // If not explicitly joined, ensure threads wind down: the reader runs to
        // EOF/breaker on its own; closing the queue releases the pump if it is
        // waiting. We never force-close the read end here (EPIPE guard).
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        self.queue.close();
        if let Some(p) = self.pump.take() {
            let _ = p.join();
        }
    }
}

/// The drain-fast reader loop (§2.3). Reads `stdout` in fixed chunks as fast as
/// the kernel delivers, feeds the parser, maps events → [`Republish`], enqueues
/// (never blocking on the sink). On a `CircuitBreaker` it kills the child and
/// stops. At EOF it reports the turn outcome and closes the queue.
fn reader_loop<R, C>(
    mut stdout: R,
    queue: Arc<BoundedQueue>,
    mut child: C,
    breaker_fired: Arc<AtomicBool>,
    max_line_bytes: usize,
) -> TurnOutcome
where
    R: Read,
    C: Child,
{
    let mut parser = StreamParser::with_cap(max_line_bytes);
    // 64 KiB read buffer — one kernel pipe's worth (spike-1 Q3: pipe = 65536).
    let mut readbuf = [0u8; 64 * 1024];
    let outcome;
    loop {
        match stdout.read(&mut readbuf) {
            Ok(0) => {
                // EOF: the child's stdout closed.
                outcome = parser.finish();
                break;
            }
            Ok(n) => {
                let mut tripped = false;
                for ev in parser.push(&readbuf[..n]) {
                    match ev {
                        StreamEvent::SystemInit { session_id } => {
                            queue.push(Republish::Ready {
                                session_id: session_id.clone(),
                            });
                            queue.push(Republish::Event(StreamEvent::SystemInit { session_id }));
                        }
                        StreamEvent::Result(r) => {
                            // NEVER dropped (§2.3).
                            queue.push(Republish::TurnEnd(r));
                        }
                        StreamEvent::CircuitBreaker {
                            cap_bytes,
                            observed_bytes,
                        } => {
                            // §H.5: kill the child (the parser only signals).
                            child.kill();
                            breaker_fired.store(true, Ordering::Release);
                            queue.push(Republish::Breaker {
                                cap_bytes,
                                observed_bytes,
                            });
                            tripped = true;
                            break;
                        }
                        other => {
                            queue.push(Republish::Event(other));
                        }
                    }
                }
                if tripped {
                    outcome = TurnOutcome::Aborted;
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                // Read error (e.g. the child died): treat as EOF for the turn.
                outcome = parser.finish();
                break;
            }
        }
    }
    queue.push(Republish::Eof(outcome));
    queue.close();
    outcome
}

/// The headless launch spec (§2.2): `claude -p PROMPT --output-format
/// stream-json --verbose` on pipes, with the daemon-set env (seam #1).
#[derive(Debug, Clone)]
pub struct HeadlessLaunch {
    /// Path to the `claude` binary.
    pub claude_bin: String,
    /// The single `-p PROMPT` for this turn (re-spawn default — keep-alive stdin
    /// is later, §2.2).
    pub prompt: String,
    /// `--resume <session_id>` for a multi-turn continuation; `None` = fresh.
    pub resume_session_id: Option<String>,
    /// Working directory for the child.
    pub cwd: Option<String>,
    /// Env injected DIRECTLY by the daemon — the certain path (seam #1, §D.1).
    /// When `clear_env` is set, ONLY these vars are passed (isolation: matches
    /// the harness `env -i HOME=.. CLAUDE_CONFIG_DIR=.. PATH=.. TERM=..`).
    pub env: Vec<(String, String)>,
    /// If true, start from an empty environment and pass only `env` (the lib.sh
    /// `env -i` isolation contract — NEVER inherit the live `~/.claude*` HOME).
    pub clear_env: bool,
    /// Daemon-resolved spawn-time flags — permission posture
    /// (`--dangerously-skip-permissions`) + relay channel injection
    /// (`--dangerously-load-development-channels server:relay`), emitted
    /// immediately after the binary. Populated by the daemon via `crates/qd`'s
    /// `launch::claude_flags` (the SAME resolver the interactive PTY path uses)
    /// so headless launches inherit the configured/overridable launch posture.
    /// Empty = no injection (a bare `-p` launch, e.g. an isolated unit/integration
    /// test).
    pub flags: Vec<String>,
}

impl HeadlessLaunch {
    /// The full argv token vector for this launch — `argv[0]` is the binary,
    /// then the daemon-resolved `flags` (spawn-time permission posture + relay
    /// channel injection, flags-FIRST to match the (A) `launch.rs` golden),
    /// then `-p PROMPT --output-format stream-json --verbose`, with
    /// `--resume <sid>` at the tail for a continuation. Pure (no I/O, no spawn)
    /// so flag injection + ordering are golden-testable without a real `claude`.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(7 + self.flags.len());
        argv.push(self.claude_bin.clone());
        argv.extend(self.flags.iter().cloned());
        argv.push("-p".to_string());
        argv.push(self.prompt.clone());
        argv.push("--output-format".to_string());
        argv.push("stream-json".to_string());
        argv.push("--verbose".to_string());
        if let Some(sid) = &self.resume_session_id {
            argv.push("--resume".to_string());
            argv.push(sid.clone());
        }
        argv
    }

    /// Spawn the child on pipes and return a [`RealChild`] + its stdout reader.
    /// stdin/stderr are piped too (stdin reserved for the later keep-alive
    /// option; stderr captured so it cannot interleave onto our stdout frames).
    pub fn spawn(&self) -> std::io::Result<(RealChild, std::process::ChildStdout)> {
        let argv = self.argv();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        if self.clear_env {
            cmd.env_clear();
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout was piped");
        Ok((
            RealChild {
                inner: child,
                killed: false,
            },
            stdout,
        ))
    }
}

/// Convenience: the default queue cap + default line cap.
pub fn start_default<R, S, C>(stdout: R, sink: S, child: C) -> HeadlessReader
where
    R: Read + Send + 'static,
    S: Sink + 'static,
    C: Child + 'static,
{
    HeadlessReader::start(
        stdout,
        sink,
        child,
        DEFAULT_QUEUE_CAP,
        DEFAULT_MAX_LINE_BYTES,
    )
}

/// R3c-Step-0 keystone (§3 R3c-0, the no-mutex-starvation guarantee — step-0 verdict
/// point 2). The wake handle MUST be detachable from the `manager` lock so the ctrl
/// select arm can clone it UNDER the lock, RELEASE the lock, and only THEN wake — a
/// blocking PTY write or a wedged session can never starve the select loop while the
/// lock is held. This pins the structural property the verdict CONFIRMED (a
/// code-structure argument, no runtime probe warranted): BOTH wake handles are
/// cloneable out of the manager. The revert seam is moving the wake INSIDE the lock
/// scope (`server::handle_ctrl_op` clones then drops `mgr` BEFORE waking); holding the
/// lock across the (blocking) wake reintroduces the starvation.
#[cfg(test)]
mod r3c0_keystone_tests {
    use super::HeadlessWake;

    #[test]
    fn wake_handles_are_detachable_from_the_manager_lock() {
        fn assert_clone_send<T: Clone + Send>() {}
        // Headless: HeadlessWake clones (Arc bump) + is Send → clone under the lock,
        // release, then wake.
        assert_clone_send::<HeadlessWake>();
        // Interactive: SharedPtyWriter (Arc<Mutex<Box<dyn Write + Send>>>) is cloneable
        // out of the lock too — the same clone-then-release pattern.
        fn assert_clone<T: Clone>() {}
        assert_clone::<crate::pty::SharedPtyWriter>();
    }
}

#[cfg(test)]
mod tests;
