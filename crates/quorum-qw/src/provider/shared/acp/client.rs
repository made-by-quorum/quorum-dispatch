//! `provider/acp/client.rs` — [`AcpHost`], the LIVE headless ACP driver, now built on the
//! **official `agent-client-protocol` crate (=1.0.1)** as the inner ACP client, strictly
//! BEHIND the retained [`AcpClient`](super::rpc::AcpClient) provider seam. The hand-rolled
//! spawn + SC-5 single-reader demux + JSON-RPC framing + response correlation +
//! reverse-request answering are REPLACED by the crate; the SC-1 outbound queue, the SC-2c
//! progress classifier, the Pete's-view transcript tee, and the SC-2a terminal observation
//! are RETAINED, sitting above the crate exactly as they sat above the old reader thread.
//! The seam (`rpc.rs` trait + normalized `AcpEvent`/`AcpError`/`StopReason`/`InitializeResult`)
//! is unchanged, so `wire.rs`/`queue.rs`/`completion.rs`/`ladder.rs` and every consumer above
//! `AcpClient` are untouched.
//!
//! **Structure — a background connection thread bridged to a sync `!Sync` host.** The retained
//! `AcpClient` contract is synchronous and blocking (`&self`, `next_update(timeout)`), but the
//! crate's connection is async (`Client::builder()…connect_with(transport, main_fn)` — one
//! self-contained future; the crate is runtime-agnostic, using `async-process` + `futures`, so
//! we drive it with `futures::executor::block_on` on a dedicated thread, NO tokio). So:
//!   * a **connection thread** spawns the bridge child ITSELF (via `async-process`) — keeping
//!     `env_remove(BRIDGE_ENV_STRIP)` expressible (MF1; a no-op for opencode which has no
//!     nesting guard, load-bearing for the downstream claude atomic) and `.stderr(inherit)` →
//!     the daemon's `acp-<name>.log` (MF3; the crate's convenience `AcpAgent` pipes+hides it) —
//!     hands the child's byte streams to the crate's custom `ByteStreams` transport, and runs
//!     `connect_with`. Its `main_fn` is a command loop owning the `ConnectionTo`.
//!   * the **sync host** marshals `initialize`/`new_session`/`load_session`/`prompt`/`cancel`
//!     to the connection thread over a `futures` mpsc, and reads normalized `AcpEvent`s back off
//!     a single ordered `std::sync::mpsc` **bus**. It owns the SC-1 queue, the progress
//!     classifier, the transcript tee, and `last_obs` — single-threaded behind one `RefCell`.
//!
//! **SC-2b terminal ordering (SRT-2/MF2), re-established above the crate.** The crate delivers
//! `session/update`s via `on_receive_notification` callbacks AND the `session/prompt` result via
//! the awaited request — two sources. We funnel BOTH into the ONE ordered bus: the notification
//! callback pushes each `Update` **synchronously, inline** in the crate's dispatch loop (the loop
//! blocks on the callback before reading the next frame — verified in the 1.0.1 dispatch actor),
//! and the prompt result — routed via `on_receiving_result`, which can only fire AFTER the
//! response frame is dispatched, i.e. after every notification frame that preceded it on the wire
//! — pushes `Terminal` onto the SAME bus. FIFO on the bus therefore equals wire order: a turn's
//! trailing `agent_message_chunk`s are drained before its terminal `StopReason`, so Pete's view
//! is never truncated. (Proven by the fixture-forced ordering test, NOT by opencode's incidental
//! timing.)

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::executor::block_on;
use futures::StreamExt;
use serde_json::Value;

use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, Implementation, InitializeRequest,
    InitializeResponse, LoadSessionRequest, NewSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification, StopReason as CrateStopReason,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Responder};

use super::completion::{AcpTurnObservation, AcpTurnObserver, TurnProgress};
use super::queue::OutboundQueue;
use super::rpc::{AcpClient, AcpError, AcpEvent, InitializeResult, SessionId, StopReason, TurnId};
use super::BRIDGE_ENV_STRIP;

/// Default SC-1 outbound-queue depth (waiting backlog, excluding the in-flight turn).
const DEFAULT_QUEUE_CAPACITY: usize = 16;

/// How long a blocked handshake waits for the connection thread's terminal [`DeadCause`]
/// before falling back to the generic [`AcpError::Closed`].
///
/// The two events race by construction: the thread drops `cmd_rx` (waking the handshake's
/// `rx.recv()` with a disconnect) as part of the SAME unwind that records the cause. The
/// thread publishes the cause BEFORE anything is dropped, so this grace is only ever the
/// few microseconds of scheduling between the two — it is a bound, not a delay, and it is
/// only ever paid on a handshake that has already failed.
const DEAD_CAUSE_GRACE: Duration = Duration::from_millis(2000);

// ===========================================================================
// sync host <-> connection thread messages
// ===========================================================================

/// A command marshalled from a sync `AcpClient` method to the connection thread's command
/// loop (which owns the crate `ConnectionTo`). Handshake commands carry a reply channel the
/// sync side blocks on; `Prompt`/`Cancel` are fire-and-forget (their results surface as
/// [`BusEvent`]s / are best-effort notifications).
enum Command {
    Initialize(std_mpsc::Sender<Result<InitializeResult, String>>),
    NewSession {
        cwd: String,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    LoadSession {
        session_id: String,
        cwd: String,
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    /// Issue `session/prompt` for the already-attributed `turn`. Its terminal surfaces as a
    /// [`BusEvent::Terminal`]/[`BusEvent::TerminalError`] carrying the SAME `turn`.
    Prompt {
        session: String,
        turn: TurnId,
        text: String,
    },
    Cancel {
        session: String,
    },
    /// Clean teardown: the command loop returns, the crate connection tears down, the child
    /// is killed, the thread exits.
    Shutdown,
}

/// One normalized event off the crate connection, pushed onto the single ordered bus the sync
/// host drains in [`AcpClient::next_update`]. The crate-backed analog of the old reader thread's
/// `RawFrame`, but already normalized (the crate owns framing + response correlation, and the
/// turn id is carried on the terminal so no rpc-id map is needed).
enum BusEvent {
    Update {
        session: SessionId,
        kind: String,
        payload: Value,
    },
    Terminal {
        session: SessionId,
        turn: TurnId,
        stop: StopReason,
    },
    TerminalError {
        session: SessionId,
        turn: TurnId,
        message: String,
    },
    /// The crate connection ended abnormally (transport lost / child gone). Surfaced as
    /// `AcpError::Closed` + a `TransportLost` observation.
    Closed(String),
}

/// Map the crate wire `StopReason` to the retained seam [`StopReason`]. Unknown (the crate enum
/// is `#[non_exhaustive]`) → `None`: an unmappable terminal is a degraded/failed turn, never a
/// clean completion.
fn map_stop(s: CrateStopReason) -> Option<StopReason> {
    match s {
        CrateStopReason::EndTurn => Some(StopReason::EndTurn),
        CrateStopReason::MaxTokens => Some(StopReason::MaxTokens),
        CrateStopReason::MaxTurnRequests => Some(StopReason::MaxTurnRequests),
        CrateStopReason::Refusal => Some(StopReason::Refusal),
        CrateStopReason::Cancelled => Some(StopReason::Cancelled),
        _ => None,
    }
}

/// Map the crate `initialize` response to the retained [`InitializeResult`]. `auth_required` is
/// always `false` (STEP-0 / the opencode probe: `authMethods` is advertised even when a usable
/// credential is present — a real auth failure surfaces later as a JSON-RPC error to
/// `session/new`/`session/prompt`, i.e. a `TerminalError`, not here).
fn map_initialize(r: InitializeResponse) -> InitializeResult {
    InitializeResult {
        protocol_version: r.protocol_version.as_u16() as i64,
        agent_name: r.agent_info.as_ref().map(|i| i.name.clone()),
        agent_version: r.agent_info.as_ref().map(|i| i.version.clone()),
        auth_required: false,
    }
}

// ===========================================================================
// the connection thread
// ===========================================================================

/// Why the connection thread ended — drives the (W) confirmed-dead signal AND, since it is
/// now RECORDED on the host (see [`run_connection`]), the operator-facing explanation for a
/// handshake that died with the connection.
#[derive(Clone)]
enum DeadCause {
    /// A clean `Command::Shutdown` (the command loop returned Ok) — NOT dead.
    CleanShutdown,
    /// The bridge child process EXITED (the authoritative confirmed-dead signal — the async
    /// analog of the old `child.try_wait()`). Carries the rendered exit status: a program that
    /// is not an ACP bridge at all exits promptly, and saying so is the whole diagnosis.
    ChildExited(String),
    /// The spawn failed, or the crate connection errored (transport lost) — treated as dead.
    Error(String),
}

impl DeadCause {
    /// The operator-facing sentence for a NON-clean end, or `None` for a clean shutdown
    /// (which is not a failure and must never be reported as one).
    fn message(&self) -> Option<&str> {
        match self {
            DeadCause::CleanShutdown => None,
            DeadCause::ChildExited(why) | DeadCause::Error(why) => Some(why),
        }
    }
}

/// The recorded terminal [`DeadCause`], shared between the connection thread (which writes it
/// exactly once) and the sync host (which reads it to explain a lost handshake).
type DeadCauseSlot = Arc<Mutex<Option<DeadCause>>>;

/// Did the bridge child already exit? Polls for a short bounded window, because the
/// connection error and the child's reaping are concurrent: the failing write can be
/// observed a beat before the exit status is. Only ever called on a connection that has
/// ALREADY failed, so the wait costs nothing on any healthy path.
fn reap_briefly(child: &mut async_process::Child) -> Option<std::process::ExitStatus> {
    for _ in 0..40 {
        if let Ok(Some(status)) = child.try_status() {
            return Some(status);
        }
        // We own this thread (the connection thread runs one `block_on`), so a short
        // blocking sleep here parks nothing else.
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

/// The connection thread body: spawn the bridge child, hand its byte streams to the crate's
/// `ByteStreams` transport, register the reverse-request + notification handlers, and run the
/// command loop as `connect_with`'s `main_fn`. It RACES the connection future against the child's
/// actual process exit (`child.status()`): a child exit is the authoritative (W) confirmed-dead
/// signal even if `connect_with` does not return promptly when the child EOFs while the command
/// loop idles. On child-exit / connection-error it flags `dead` and posts `Closed` so the sync
/// host + the `serve` self-terminate guard see the death; a clean shutdown does neither.
fn run_connection(
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    cmd_rx: UnboundedReceiver<Command>,
    bus_tx: std_mpsc::Sender<BusEvent>,
    dead: Arc<AtomicBool>,
    cause_slot: DeadCauseSlot,
) {
    let bus_end = bus_tx.clone();
    // RECORD-BEFORE-UNWIND. Every terminal cause goes through `record`, which publishes the
    // `dead` flag and then the cause while the async block is still on the stack — i.e.
    // BEFORE `cmd_rx` (and with it every pending handshake's reply channel) is dropped. A
    // sync handshake blocked in `rx.recv()` wakes on exactly that drop, so a cause recorded
    // after the block returned would lose the race with the error it exists to explain.
    let record = move |c: DeadCause| -> DeadCause {
        if c.message().is_some() {
            dead.store(true, Ordering::SeqCst);
            if let Ok(mut slot) = cause_slot.lock() {
                *slot = Some(c.clone());
            }
        }
        c
    };
    let cause: DeadCause = block_on(async move {
        // Spawn the bridge child OURSELVES (as the old `AcpHost::spawn` did) so the nesting-guard
        // strip stays expressible (MF1) and stderr routes to the daemon log (MF3) — NOT the
        // crate's convenience `AcpAgent`, which can only ADD env and pipes+hides stderr.
        let mut cmd = async_process::Command::new(&program);
        cmd.args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Bridge stderr → our stderr (the daemon's acp-<name>.log): visible diagnostics,
            // never on the protocol stream. The crate reads only the byte streams we hand it.
            .stderr(Stdio::inherit());
        for var in BRIDGE_ENV_STRIP {
            cmd.env_remove(var);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return record(DeadCause::Error(format!("spawn {program}: {e}"))),
        };
        let stdin = match child.stdin.take() {
            Some(s) => s,
            None => return record(DeadCause::Error("bridge stdin not piped".to_string())),
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => return record(DeadCause::Error("bridge stdout not piped".to_string())),
        };
        // ByteStreams::new(outgoing = child stdin (AsyncWrite), incoming = child stdout (AsyncRead));
        // it does the ndjson line-framing internally.
        let transport = ByteStreams::new(stdin, stdout);

        let bus_notif = bus_tx.clone();
        let conn_fut = Client
            .builder()
            .name("dispatch-acp")
            // session/update → push an Update onto the bus, INLINE (the dispatch loop blocks on
            // this callback before reading the next frame → SC-2b ordering). We serialize the
            // crate's typed `SessionUpdate` back to the raw `update` JSON the retained
            // AcpEvent/transcript/assistant_text expect (keeps the seam types unchanged).
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let bus = bus_notif.clone();
                    async move {
                        let payload = serde_json::to_value(&notif.update).unwrap_or(Value::Null);
                        let kind = payload
                            .get("sessionUpdate")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let session = notif.session_id.to_string();
                        let _ = bus.send(BusEvent::Update {
                            session,
                            kind,
                            payload,
                        });
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            // Reverse-request policy — behaviorally identical to the old `answer_reverse`:
            // permission → `cancelled` outcome (the headless host never interactively grants a
            // tool permission; a turn that needs one terminates rather than wedging). fs/* and
            // terminal/* are declared OFF in clientCapabilities and left unregistered → the crate
            // responds method-not-found (design-checked; RUN-verified by gate (a) with a
            // tool-forcing prompt — opencode no-tool turns raise none, per the wire probe).
            .on_receive_request(
                |_req: RequestPermissionRequest,
                 responder: Responder<RequestPermissionResponse>,
                 _cx| async move {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, move |cx| command_loop(cx, cmd_rx, bus_tx));

        // RACE the connection against the child's real process exit. A child exit is the
        // authoritative confirmed-dead signal (the old `try_wait` semantics), robust even if
        // `connect_with` does not return promptly when the child EOFs while the loop idles.
        let conn_fut = Box::pin(conn_fut);
        let child_exit = Box::pin(child.status());
        let cause = match futures::future::select(conn_fut, child_exit).await {
            futures::future::Either::Left((conn_result, child_exit)) => {
                drop(child_exit); // release the &mut child borrow before kill()
                match conn_result {
                    Ok(()) => DeadCause::CleanShutdown,
                    // A connection error is usually the SYMPTOM of a dead child (the first
                    // write to a gone bridge fails as a broken pipe, and that write loses
                    // the race with observing the exit). Ask the child before blaming the
                    // transport: "bridge child exited (exit status: 0)" is a diagnosis a
                    // human can act on, where a crate-internal broken-pipe dump is not.
                    Err(e) => match reap_briefly(&mut child) {
                        Some(st) => DeadCause::ChildExited(format!("bridge child exited ({st})")),
                        None => DeadCause::Error(e.to_string()),
                    },
                }
            }
            futures::future::Either::Right((exit_status, conn_fut)) => {
                drop(conn_fut); // tear the connection down
                                // The status is the diagnosis when the "bridge" is not a bridge: a program
                                // that simply runs and returns (say `/bin/echo`) lands here with a clean
                                // exit code, which reads very differently from a crash or a signal.
                DeadCause::ChildExited(match exit_status {
                    Ok(st) => format!("bridge child exited ({st})"),
                    Err(e) => format!("bridge child exited (status unavailable: {e})"),
                })
            }
        };
        // Teardown: kill the child (async-process does not kill on drop). Harmless no-op if it
        // already exited.
        let _ = child.kill();
        record(cause)
    });

    // `record` already published `dead` + the cause; all that is left is the bus post the
    // sync host + the `serve` self-terminate guard observe.
    if let Some(why) = cause.message() {
        let _ = bus_end.send(BusEvent::Closed(why.to_string()));
    }
}

/// `connect_with`'s `main_fn`: own the `ConnectionTo` and translate commands into crate calls.
/// Handshakes block here (one-in-flight, no queue) via `block_task` — legal in `main_fn` (never
/// inside a handler, which would deadlock the dispatch loop). `Prompt` is fire-and-forget:
/// `on_receiving_result` posts the terminal to the bus after the response frame is dispatched
/// (SC-2b), WITHOUT blocking the loop, so a `Cancel` can be processed mid-turn.
async fn command_loop(
    cx: ConnectionTo<Agent>,
    mut cmd_rx: UnboundedReceiver<Command>,
    bus_tx: std_mpsc::Sender<BusEvent>,
) -> Result<(), agent_client_protocol::Error> {
    while let Some(cmd) = cmd_rx.next().await {
        match cmd {
            Command::Initialize(reply) => {
                let req = InitializeRequest::new(ProtocolVersion::V1)
                    .client_capabilities(ClientCapabilities::default())
                    .client_info(Implementation::new(
                        "dispatch-acp",
                        env!("CARGO_PKG_VERSION"),
                    ));
                let res = cx.send_request(req).block_task().await;
                let _ = reply.send(res.map(map_initialize).map_err(|e| e.to_string()));
            }
            Command::NewSession { cwd, reply } => {
                let res = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await;
                let _ = reply.send(
                    res.map(|r| r.session_id.to_string())
                        .map_err(|e| e.to_string()),
                );
            }
            Command::LoadSession {
                session_id,
                cwd,
                reply,
            } => {
                let res = cx
                    .send_request(LoadSessionRequest::new(session_id, cwd))
                    .block_task()
                    .await;
                let _ = reply.send(res.map(|_| ()).map_err(|e| e.to_string()));
            }
            Command::Prompt {
                session,
                turn,
                text,
            } => {
                let block: ContentBlock = text.into();
                let sent = cx.send_request(PromptRequest::new(session.clone(), vec![block]));
                let bus = bus_tx.clone();
                let session_id = SessionId::from(session);
                let _ = sent.on_receiving_result(move |result| async move {
                    match result {
                        Ok(resp) => match map_stop(resp.stop_reason) {
                            Some(stop) => {
                                let _ = bus.send(BusEvent::Terminal {
                                    session: session_id,
                                    turn,
                                    stop,
                                });
                            }
                            None => {
                                let _ = bus.send(BusEvent::TerminalError {
                                    session: session_id,
                                    turn,
                                    message: "session/prompt response had no mappable stopReason"
                                        .to_string(),
                                });
                            }
                        },
                        Err(e) => {
                            let _ = bus.send(BusEvent::TerminalError {
                                session: session_id,
                                turn,
                                message: e.to_string(),
                            });
                        }
                    }
                    Ok(())
                });
            }
            Command::Cancel { session } => {
                // Best-effort interrupt (a notification): the bridge resolves the in-flight prompt
                // with `cancelled`; a wedged bridge may ignore it (teardown is the guaranteed unblock).
                let _ = cx.send_notification(CancelNotification::new(session));
            }
            Command::Shutdown => break,
        }
    }
    Ok(())
}

// ===========================================================================
// the sync host
// ===========================================================================

/// The host's mutable state, single-threaded behind one [`RefCell`]. The connection thread
/// never touches it — they communicate only over `cmd_tx`/`bus_rx`.
struct HostInner {
    /// Sync → connection-thread command channel.
    cmd_tx: UnboundedSender<Command>,
    /// Connection-thread → sync ordered event bus (the SC-2b ordering guarantee lives in the
    /// FIFO of this channel; see the module doc).
    bus_rx: std_mpsc::Receiver<BusEvent>,
    /// The connection thread handle (joined on teardown).
    thread: Option<JoinHandle<()>>,
    /// Set by the connection thread when the connection ends abnormally (transport lost / child
    /// gone) — the (W) confirmed-dead signal the `serve` self-terminate guard reads.
    dead: Arc<AtomicBool>,
    /// WHY the connection ended abnormally, recorded by the connection thread alongside
    /// `dead` (and always published just after it). This is what turns a handshake failure
    /// from "acp connection closed" into "spawn claude-code-acp: No such file or directory".
    dead_cause: DeadCauseSlot,
    /// The established ACP session id (`None` before `session/new`/`session/load`).
    session: Option<SessionId>,
    /// The SC-1a CONFIGURED outbound-queue bound, set at spawn and preserved across
    /// `session/new`/`session/load` so the queue rebuilds at the host's real capacity.
    capacity: usize,
    /// The SC-1 outbound queue (keyed by the ACP session).
    queue: Option<OutboundQueue>,
    /// The SC-2c started→streaming classifier for the in-flight turn.
    progress: TurnProgress,
    /// The latest terminal observation, for the [`AcpTurnObserver`] the completion probe reads.
    last_obs: AcpTurnObservation,
    /// The Pete's-view transcript tee: every `session/update`'s raw `update` object, in arrival order.
    transcript: Vec<Value>,
}

/// The live ACP host: a crate connection thread + the SC-1 queue + the transcript tee. `!Sync`
/// by design (one [`RefCell`]); the `&self` methods are callable through the shared
/// `&dyn AcpClient` out of `ProviderFx`.
pub struct AcpHost {
    inner: RefCell<HostInner>,
}

impl AcpHost {
    /// Spawn the bridge `program args…` (e.g. `("opencode", ["acp"])` for the opencode harness,
    /// or `("claude-code-acp", [])` for claude) with working dir `cwd`, **stripping
    /// [`BRIDGE_ENV_STRIP`] from the child env** (the STEP-0 nesting guard — a no-op for opencode,
    /// load-bearing for claude), starting the crate connection on a background thread. Does NOT
    /// handshake — call [`Self::initialize`] then [`Self::new_session`] (the boot waiter drives
    /// `initialize`).
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
        let (cmd_tx, cmd_rx) = unbounded::<Command>();
        let (bus_tx, bus_rx) = std_mpsc::channel::<BusEvent>();
        let dead = Arc::new(AtomicBool::new(false));
        let dead_cause: DeadCauseSlot = Arc::new(Mutex::new(None));

        let program = program.to_string();
        let args = args.to_vec();
        let cwd = cwd.to_path_buf();
        let bus_for_thread = bus_tx;
        let dead_for_thread = dead.clone();
        let cause_for_thread = dead_cause.clone();
        let thread = std::thread::Builder::new()
            .name("acp-crate-conn".to_string())
            .spawn(move || {
                run_connection(
                    program,
                    args,
                    cwd,
                    cmd_rx,
                    bus_for_thread,
                    dead_for_thread,
                    cause_for_thread,
                )
            })
            .map_err(|e| AcpError::Transport(format!("connection thread: {e}")))?;

        Ok(AcpHost {
            inner: RefCell::new(HostInner {
                cmd_tx,
                bus_rx,
                thread: Some(thread),
                dead,
                dead_cause,
                session: None,
                capacity,
                queue: Some(OutboundQueue::new("pending-session", capacity)),
                progress: TurnProgress::new(),
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
    /// `is_idle` truth, so a `wait` can short-circuit a genuinely-idle session yet never
    /// false-idle mid-turn.
    pub fn in_flight(&self) -> bool {
        self.inner
            .borrow()
            .queue
            .as_ref()
            .map(|q| !q.is_idle())
            .unwrap_or(false)
    }

    /// (W) WEDGED-BUT-ALIVE (Item 3) — is the bridge CONFIRMED gone? True once the crate
    /// connection has ended abnormally (transport lost / child exited → the connection future
    /// returned `Err`), which necessarily means the bridge child's stdout closed. A clean
    /// shutdown does NOT set this. Conservative: a transient blip that does not end the
    /// connection returns `false`, so the adapter survives it.
    pub fn bridge_confirmed_dead(&self) -> bool {
        self.inner.borrow().dead.load(Ordering::SeqCst)
    }

    /// WHY the bridge is confirmed dead, if it is — the connection thread's recorded terminal
    /// cause (`spawn <program>: No such file or directory`, `bridge child exited (exit status:
    /// 0)`, a transport error…). `None` while the connection is healthy, and `None` after a
    /// CLEAN shutdown, which is not a failure. The `acp-daemon` boot reads this to say what
    /// actually happened instead of "the connection closed".
    pub fn bridge_dead_cause(&self) -> Option<String> {
        let slot = self.inner.borrow().dead_cause.clone();
        let cause = slot.lock().ok()?;
        cause.as_ref()?.message().map(str::to_string)
    }

    /// The error a blocked handshake should report when its reply channel died: the recorded
    /// terminal cause when there is one, else the generic [`AcpError::Closed`] (a connection
    /// that really did just close). Waits up to [`DEAD_CAUSE_GRACE`] for the cause, because
    /// the wake-up and the recording are the same unwind — see [`run_connection`].
    fn handshake_closed_error(&self) -> AcpError {
        let slot = self.inner.borrow().dead_cause.clone();
        let deadline = Instant::now() + DEAD_CAUSE_GRACE;
        loop {
            if let Ok(cause) = slot.lock() {
                if let Some(recorded) = cause.as_ref() {
                    return match recorded.message() {
                        Some(why) => AcpError::Transport(why.to_string()),
                        // A clean shutdown raced the handshake: nothing failed, the host was
                        // told to go away. That IS `Closed`.
                        None => AcpError::Closed,
                    };
                }
            }
            if Instant::now() >= deadline {
                return AcpError::Closed;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// `session/load` (resume): re-establish a prior session by id on this bridge. The bridge
    /// replays the session's history as `session/update`s (drained via [`Self::next_update`]).
    /// Proves the resume verb on the LIVE bridge (pillar 2 / gate b).
    pub fn load_session(&self, session_id: &str, cwd: &str) -> Result<(), AcpError> {
        let cmd_tx = self.inner.borrow().cmd_tx.clone();
        let (tx, rx) = std_mpsc::channel();
        cmd_tx
            .unbounded_send(Command::LoadSession {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
                reply: tx,
            })
            .map_err(|_| self.handshake_closed_error())?;
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(AcpError::Protocol(e)),
            Err(_) => return Err(self.handshake_closed_error()),
        }
        let mut inner = self.inner.borrow_mut();
        inner.session = Some(session_id.to_string());
        let capacity = inner.capacity;
        inner.queue = Some(OutboundQueue::new(session_id.to_string(), capacity));
        Ok(())
    }

    /// The full Pete's-view transcript tee: every `session/update`'s raw `update` object in
    /// arrival order (the live-view feed; A2 §5).
    pub fn transcript(&self) -> Vec<Value> {
        self.inner.borrow().transcript.clone()
    }

    /// The accumulated assistant TEXT across all `agent_message_chunk` updates (what Pete reads
    /// as the model's reply). Convenience over [`Self::transcript`].
    pub fn assistant_text(&self) -> String {
        self.inner
            .borrow()
            .transcript
            .iter()
            .filter(|u| {
                u.get("sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk")
            })
            .filter_map(|u| {
                u.get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
            })
            .collect()
    }

    /// Best-effort teardown: ask the connection thread to shut down (the crate connection tears
    /// down + the child is killed) and join it. Idempotent.
    pub fn shutdown(&self) {
        let (cmd_tx, thread) = {
            let mut inner = self.inner.borrow_mut();
            (inner.cmd_tx.clone(), inner.thread.take())
        };
        let _ = cmd_tx.unbounded_send(Command::Shutdown);
        if let Some(handle) = thread {
            let _ = handle.join();
        }
    }

    /// If the session is turn-idle and an item waits, make it in-flight and dispatch it as
    /// `session/prompt` to the connection thread (the SC-1 release → send).
    fn release_next(inner: &mut HostInner) -> Result<(), AcpError> {
        let session = match &inner.session {
            Some(s) => s.clone(),
            None => return Ok(()),
        };
        let item = match inner.queue.as_mut().and_then(OutboundQueue::try_release) {
            Some(item) => item,
            None => return Ok(()),
        };
        // A new turn begins → reset the started/streaming classifier.
        inner.progress = TurnProgress::new();
        inner
            .cmd_tx
            .unbounded_send(Command::Prompt {
                session,
                turn: item.turn,
                text: item.message,
            })
            .map_err(|_| AcpError::Closed)
    }
}

impl Drop for AcpHost {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        let _ = inner.cmd_tx.unbounded_send(Command::Shutdown);
        if let Some(handle) = inner.thread.take() {
            let _ = handle.join();
        }
    }
}

impl AcpClient for AcpHost {
    fn initialize(&self) -> Result<InitializeResult, AcpError> {
        let cmd_tx = self.inner.borrow().cmd_tx.clone();
        let (tx, rx) = std_mpsc::channel();
        cmd_tx
            .unbounded_send(Command::Initialize(tx))
            .map_err(|_| self.handshake_closed_error())?;
        match rx.recv() {
            Ok(Ok(init)) => Ok(init),
            Ok(Err(e)) => Err(AcpError::Protocol(e)),
            // The connection died under the handshake. THIS is where a missing/!ACP bridge
            // used to become the indistinguishable "acp connection closed"; now it reports
            // the connection thread's recorded cause.
            Err(_) => Err(self.handshake_closed_error()),
        }
    }

    fn new_session(&self, cwd: &str) -> Result<SessionId, AcpError> {
        let cmd_tx = self.inner.borrow().cmd_tx.clone();
        let (tx, rx) = std_mpsc::channel();
        cmd_tx
            .unbounded_send(Command::NewSession {
                cwd: cwd.to_string(),
                reply: tx,
            })
            .map_err(|_| self.handshake_closed_error())?;
        let session = match rx.recv() {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(AcpError::Protocol(e)),
            Err(_) => return Err(self.handshake_closed_error()),
        };
        // The queue is rebuilt keyed by the real ACP session, at the CONFIGURED capacity (SC-1a).
        let mut inner = self.inner.borrow_mut();
        let capacity = inner.capacity;
        inner.queue = Some(OutboundQueue::new(session.clone(), capacity));
        inner.session = Some(session.clone());
        Ok(session)
    }

    fn prompt(
        &self,
        _session: &str,
        text: &str,
        from: &str,
        on_dispatched: &dyn Fn(),
    ) -> Result<TurnId, AcpError> {
        let mut inner = self.inner.borrow_mut();
        if inner.session.is_none() {
            return Err(AcpError::Protocol("no acp session established".to_string()));
        }
        // ENQUEUE (mints + returns the attributable turn id; LB#5). A 2nd inject mid-turn QUEUES
        // (SC-3); a bounded-overflow → Err(QueueFull) (SC-1a).
        let turn = inner
            .queue
            .as_mut()
            .ok_or_else(|| AcpError::Protocol("no acp session established".to_string()))?
            .enqueue(text, from)
            .map_err(|_| AcpError::QueueFull)?;
        // In-process enqueue is a single atomic local step — no dispatch/reply split, so there is
        // no ambiguous partial-completion window. Fire the hook once the turn is durably queued.
        on_dispatched();
        // Release+send NOW iff the session is turn-idle (never interrupts an in-flight turn —
        // SC-1); otherwise it sends when the in-flight terminal releases the slot in `next_update`.
        Self::release_next(&mut inner)?;
        Ok(turn)
    }

    fn cancel(&self, _session: &str) -> Result<(), AcpError> {
        let inner = self.inner.borrow();
        let session = inner
            .session
            .clone()
            .ok_or_else(|| AcpError::Protocol("no acp session established".to_string()))?;
        inner
            .cmd_tx
            .unbounded_send(Command::Cancel { session })
            .map_err(|_| AcpError::Closed)
    }

    fn next_update(&self, timeout: Duration) -> Result<Option<AcpEvent>, AcpError> {
        // Block on the ordered bus up to `timeout`. Every event returns (the crate owns framing +
        // correlation, so there are no drop-and-keep-draining frames here). A quiet stream →
        // Ok(None) (NOT an error).
        let event = {
            let inner = self.inner.borrow();
            inner.bus_rx.recv_timeout(timeout)
        };
        match event {
            Ok(BusEvent::Update {
                session,
                kind,
                payload,
            }) => {
                let mut inner = self.inner.borrow_mut();
                inner.progress.on_update();
                inner.transcript.push(payload.clone());
                Ok(Some(AcpEvent::Update {
                    session,
                    kind,
                    payload,
                }))
            }
            Ok(BusEvent::Terminal {
                session,
                turn,
                stop,
            }) => {
                let mut inner = self.inner.borrow_mut();
                if let Some(q) = inner.queue.as_mut() {
                    q.complete(&turn);
                }
                inner.last_obs = AcpTurnObservation::Terminal(stop);
                // Release+send the next queued turn (drain continuity).
                Self::release_next(&mut inner)?;
                Ok(Some(AcpEvent::Terminal {
                    session,
                    turn,
                    stop,
                }))
            }
            Ok(BusEvent::TerminalError {
                session,
                turn,
                message,
            }) => {
                let mut inner = self.inner.borrow_mut();
                if let Some(q) = inner.queue.as_mut() {
                    q.complete(&turn);
                }
                inner.last_obs = AcpTurnObservation::Failed(message.clone());
                Self::release_next(&mut inner)?;
                Ok(Some(AcpEvent::TerminalError {
                    session,
                    turn,
                    message,
                }))
            }
            Ok(BusEvent::Closed(why)) => {
                self.inner.borrow_mut().last_obs = AcpTurnObservation::TransportLost(why);
                Err(AcpError::Closed)
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                self.inner.borrow_mut().last_obs =
                    AcpTurnObservation::TransportLost("connection thread gone".to_string());
                Err(AcpError::Closed)
            }
        }
    }
}

/// The host IS the source the completion probe reads (SC-2a): the latest terminal observation,
/// updated as `next_update` processes terminals. The `--wait` loop wraps this in an
/// [`AcpTurnCompletion`](super::completion::AcpTurnCompletion).
impl AcpTurnObserver for AcpHost {
    fn observe(&self) -> AcpTurnObservation {
        self.inner.borrow().last_obs.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this file's `dead_cause` plumbing exists to kill: the connection thread knew
    /// EXACTLY why it died (`spawn <program>: No such file or directory`) and threw it away,
    /// so `initialize` reported the one message that fits every failure equally badly.
    ///
    /// Spawning a program that cannot exist is hermetic — nothing is installed, nothing is
    /// contacted, and the failure is the operating system's.
    #[test]
    fn a_missing_bridge_program_is_named_in_the_initialize_error() {
        let host = AcpHost::spawn(
            "definitely-not-a-real-binary-xyz",
            &[],
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .expect("the host spawns; only the BRIDGE is missing");
        let err = host
            .initialize()
            .expect_err("a missing bridge cannot handshake");
        let text = err.to_string();
        assert!(
            text.contains("definitely-not-a-real-binary-xyz"),
            "the error must NAME the program that could not be spawned: {text}"
        );
        assert!(
            !matches!(err, AcpError::Closed),
            "a failed spawn is not a closed connection: {text}"
        );
        assert_eq!(
            host.bridge_dead_cause()
                .as_deref()
                .map(|c| c.contains("spawn")),
            Some(true),
            "the recorded cause is the spawn failure itself"
        );
    }

    /// The OTHER half of the indistinguishability: a program that exists but is not an ACP
    /// bridge. It used to produce a byte-identical message to the missing one; now it says
    /// the child exited, and carries its exit status.
    #[test]
    fn a_program_that_is_not_a_bridge_reports_the_child_exit() {
        let host = AcpHost::spawn("/bin/echo", &[], Path::new(env!("CARGO_MANIFEST_DIR")))
            .expect("/bin/echo spawns fine — it just does not speak ACP");
        let err = host
            .initialize()
            .expect_err("a program that exits cannot complete the handshake");
        let text = err.to_string();
        assert!(
            text.contains("bridge child exited"),
            "a bridge that ran and quit must say so: {text}"
        );
        assert!(
            !matches!(err, AcpError::Closed),
            "a child that exited under the handshake is not a clean close: {text}"
        );
    }

    /// The two failures above must not be the SAME sentence — that identity was the whole
    /// complaint (`--bridge-cmd nonexistent` and `--bridge-cmd /bin/echo` were byte-equal).
    #[test]
    fn the_two_bridge_failures_are_distinguishable() {
        let missing = AcpHost::spawn(
            "definitely-not-a-real-binary-xyz",
            &[],
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .unwrap()
        .initialize()
        .expect_err("missing bridge")
        .to_string();
        let not_a_bridge = AcpHost::spawn("/bin/echo", &[], Path::new(env!("CARGO_MANIFEST_DIR")))
            .unwrap()
            .initialize()
            .expect_err("not a bridge")
            .to_string();
        assert_ne!(
            missing, not_a_bridge,
            "a missing bridge and a present-but-wrong one are different diagnoses"
        );
    }
}

// Retired tests (called out, not silently dropped): the hand-rolled reader-frame `classify_*`
// tests and `pump_until_response` mechanics that lived here pinned the old demux + framing +
// response correlation — all now owned by the crate, so they are removed. Their behavioral
// coverage is re-expressed at the new layer: the retained `rpc.rs` faithfulness/terminal-mapping
// tests (unchanged), the SC-2b terminal-ordering guarantee proven by the fixture-forced SRT-2
// ordering test (see `tests/`), and the live opencode wire evidence
// (evidence/opencode-acp/opencode-wire-probe.log). See the handoff for the per-test mapping.
