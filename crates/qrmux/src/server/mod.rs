//! Daemon server that manages sessions and accepts client connections over a Unix socket.

pub mod client_handler;
pub mod fault;
pub mod session_bridge;
mod session_relay;
mod session_setup;
pub mod socket;

use crate::session::SessionManager;
use client_handler::DaemonCtx;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// WS-C M2 (§4.1): default claim timeout. If the single-session daemon's session
/// is never created within this window (timer reset only by session-addressed
/// verbs, red-team B1), the daemon exits 0 — the unclaimed-orphan reaper. Tunable
/// via `QRMUX_CLAIM_TIMEOUT_MS` (jail tests run ~1–2s).
const CLAIM_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(30_000);

fn claim_timeout_from_env() -> std::time::Duration {
    std::env::var("QRMUX_CLAIM_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(CLAIM_TIMEOUT)
}

// ===== R3c-Step-1: per-session control socket servicer =====================
//
// Wire opcodes (1 byte). MIRROR of `dispatch::control_sock`'s `OP_*` — there is NO
// shared crate (dispatch depends on qrmux, not vice versa), so these are duplicated
// by necessity; keep them in lockstep with `control_sock.rs`. A datagram's first
// byte is the opcode; the rest (≤64B) is reserved.
mod ctrl_op {
    /// New inbox mail → nudge a parked session (the R3c-Step-1 payload).
    pub const WAKE_INBOX: u8 = 1;
    /// Liveness probe (recovery Rung 2). Reserved here.
    pub const PING: u8 = 2;
    /// Checkpoint request (recovery Rung 2 / pre-respawn). Reserved here.
    pub const CHECKPOINT: u8 = 3;
    /// Orderly turn-boundary stop (lifecycle). Reserved here.
    pub const GRACEFUL_STOP: u8 = 4;
}

/// Bind the control socket: ensure its parent dir, clear a stale file (the `.sock`
/// bind already proved no live duplicate, so a leftover ctrl file is stale), bind.
fn bind_ctrl_socket(path: &std::path::Path) -> std::io::Result<tokio::net::UnixDatagram> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    tokio::net::UnixDatagram::bind(path)
}

/// RAII cleanup of the control socket file on daemon exit (mirrors `SocketGuard`).
struct CtrlGuard(std::path::PathBuf);
impl Drop for CtrlGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Await one control datagram and return its opcode byte. Parks FOREVER when no
/// ctrl socket is bound (bare qrmux) so the `tokio::select!` arm — guarded by
/// `if ctrl.is_some()` — stays inert. A 0-byte datagram or recv error yields `None`
/// (ignored, forward-compat). Owns its buffer so no `&mut` is borrowed across the
/// select arm.
async fn recv_ctrl(ctrl: &Option<tokio::net::UnixDatagram>) -> Option<u8> {
    match ctrl {
        Some(sock) => {
            let mut buf = [0u8; 64];
            match sock.recv_from(&mut buf).await {
                Ok((n, _addr)) if n >= 1 => Some(buf[0]),
                _ => None,
            }
        }
        None => std::future::pending().await,
    }
}

/// Service one control opcode. On `WakeInbox` it drives the session-appropriate
/// wake. THE KEYSTONE (step-0 verdict point 2): clone the wake handle UNDER the
/// `manager` lock, RELEASE the lock, then wake OUTSIDE it — the PTY write can block
/// on a full buffer, so it must never run while holding the lock, or a real wedge
/// would starve the select loop. Interactive → PTY nudge (off the runtime via
/// `spawn_blocking`).
async fn handle_ctrl_op(op: u8, session: &str, manager: &Arc<Mutex<SessionManager>>) {
    match op {
        ctrl_op::WAKE_INBOX => {
            enum Wake {
                Pty(crate::pty::SharedPtyWriter),
                None,
            }
            // Lock ONLY to clone the wake handle — mirrors the accept arm's
            // clone-then-spawn (`server/mod.rs` accept). Released at the block end.
            let wake = {
                let mgr = manager.lock().await;
                if let Some(s) = mgr.get(session) {
                    Wake::Pty(s.pty_writer_arc())
                } else {
                    Wake::None
                }
            };
            match wake {
                Wake::Pty(writer) => {
                    // PTY write may block on a full buffer → run it OFF the tokio
                    // runtime (and the manager lock is already released), so the
                    // select loop keeps being scheduled. Best-effort: a bare newline
                    // re-renders / re-polls a parked prompt.
                    tokio::task::spawn_blocking(move || {
                        use std::io::Write as _;
                        if let Ok(mut w) = writer.lock() {
                            let _ = w.write_all(b"\n");
                            let _ = w.flush();
                        }
                    });
                }
                Wake::None => {
                    warn!(session, "WakeInbox for an unknown session; ignored");
                }
            }
        }
        // Reserved for the recovery ladder (R3c-Step-2). No-op for now.
        ctrl_op::PING | ctrl_op::CHECKPOINT | ctrl_op::GRACEFUL_STOP => {}
        // Unknown opcode → ignore (forward-compat).
        _ => {}
    }
}

pub use socket::{
    session_lock_path_for, session_socket_path_for, shared_fate_test_mode, socket_dir,
    socket_dir_for, validate_session_identity,
};

/// §4.1 lost-reply fix [F2]: the blocking-drop budget, promoted from a bare
/// `from_secs(5)` to a shared const so the exit-wait bound (`QRMUX_EXIT_WAIT_MS`
/// default = `DROP_BUDGET + EXIT_WAIT_SLACK`) is DERIVED from it and the two
/// cannot drift. A `KillSession` or reaper drop blocks up to this long
/// (kill()+wait() on a child whose grandchildren keep the PTY alive); the exit
/// wait MUST EXCEED it by real slack so the post-drop encode+write of the owed
/// reply still has margin — equal budgets reintroduce the lost reply (the
/// slow-drop case empties the manager BEFORE the drop, so exit-wait and drop
/// would otherwise count down in parallel with zero post-drop margin).
pub(crate) const DROP_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Drop a value on a blocking thread with a timeout. Used for Session drops
/// which call blocking kill()+wait() on child processes. The `DROP_BUDGET`
/// timeout prevents hangs when grandchild processes keep the PTY alive after kill.
pub(super) async fn drop_blocking_with_timeout<T: Send + 'static>(value: T, label: &str) {
    let label = label.to_string();
    let result = tokio::time::timeout(
        DROP_BUDGET,
        tokio::task::spawn_blocking(move || {
            // §4.1 CR-3 test-lane seam: when `QRMUX_TEST_SLOW_DROP_MS` is set,
            // sleep INSIDE the blocking drop the handler/reaper performs (this is
            // the BLOCKING drop path, NOT an async sleep) to widen the
            // owed-reply-vs-exit race deterministically. Breaker pattern
            // (RETACH_B1_BREAK precedent): documented, never set by production,
            // inert when unset.
            test_slow_drop_injection();
            drop(value)
        }),
    )
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(join_err)) => warn!(%label, error = %join_err, "drop task panicked"),
        Err(_) => warn!(%label, "timed out dropping value on blocking thread"),
    }
}

/// §4.1 CR-3 test-lane seam (breaker pattern, RETACH_B1_BREAK precedent). When
/// `QRMUX_TEST_SLOW_DROP_MS` is set to a millisecond count, the BLOCKING drop
/// path sleeps that long — widening the window where a `KillSession`/reaper drop
/// holds while the handler still owes its reply, so the pre-fix lost-reply race
/// reproduces deterministically and the post-fix wait can be proven to close it.
/// Documented, NEVER set by production, inert (no syscall) when unset. Read at
/// each drop so a test can arm it per-process via the daemon's env.
fn test_slow_drop_injection() {
    if let Some(ms) = std::env::var("QRMUX_TEST_SLOW_DROP_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// §4.1 lost-reply fix [F2]: slack the exit-wait bound adds ON TOP of
/// `DROP_BUDGET`. The bound MUST EXCEED the drop budget (never equal it) so the
/// owed reply's post-drop encode+write keeps real margin. 2s is generous for a
/// single frame write to a local Unix socket.
const EXIT_WAIT_SLACK: std::time::Duration = std::time::Duration::from_secs(2);

/// §4.1 lost-reply fix: the bounded exit-on-end / claim-timeout in-flight wait.
/// Default = `DROP_BUDGET + EXIT_WAIT_SLACK` (= 7s), DERIVED from the drop budget
/// so the two cannot drift. Tunable via `QRMUX_EXIT_WAIT_MS` (CR-3 arm 3 sets a
/// small bound to prove fail-closed-loud: a stalled handler cannot make the
/// daemon immortal).
fn exit_wait_budget() -> std::time::Duration {
    std::env::var("QRMUX_EXIT_WAIT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(DROP_BUDGET + EXIT_WAIT_SLACK)
}

/// §4.1 lost-reply fix [F9]: the SIGTERM/SIGINT in-flight wait uses its OWN
/// SHORTER bound. An operator kill wants the daemon gone; one second flushes any
/// non-pathological owed reply ahead of `drain_all`'s stream teardown without
/// making a `kill` feel sticky. Deliberate divergence from the exit-on-end bound,
/// not a copy-paste.
const SIGNAL_INFLIGHT_WAIT: std::time::Duration = std::time::Duration::from_millis(1_000);

/// §4.1 lost-reply fix: wait for in-flight one-shot work to drain, bounded. On
/// timeout, emit the NAMED fail-closed-loud line ("exit-wait: N handler(s) still
/// in flight after Nms — exiting") so a stalled client/handler is visible and the
/// daemon exits anyway (orc rider R-i: a stall can never make the daemon
/// immortal). Returns nothing — the caller exits regardless.
async fn wait_in_flight_or_log(in_flight: &client_handler::InFlight, budget: std::time::Duration) {
    if !in_flight.wait_for_zero(budget).await {
        warn!(
            in_flight = in_flight.count(),
            budget_ms = budget.as_millis() as u64,
            "exit-wait: {} handler(s) still in flight after {}ms — exiting (stalled client?)",
            in_flight.count(),
            budget.as_millis()
        );
    }
}

/// Default interval between dead session cleanup sweeps. WS-C M3b: 25ms (FAST) —
/// the per-session daemon's reaper is the path that emits the ChildExited close
/// bookend (§4.1 step 3) right before the lifecycle task observes the empty
/// manager and exits-on-end, so a slow sweep would stall teardown. (The legacy
/// shared daemon used 30s to bound lock contention across many sessions; that
/// mode is retired.) Env-tunable via `QRMUX_CLEANUP_INTERVAL_MS` (ACK-1 R-REAP
/// test knob — ack1-spec §2 C2).
const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

fn cleanup_interval_from_env() -> std::time::Duration {
    std::env::var("QRMUX_CLEANUP_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(CLEANUP_INTERVAL)
}

/// Start the daemon server: bind the Unix socket, spawn the cleanup task, and accept clients.
///
/// **Socket-dir override (C1 D1/R26):** `socket_dir` of `Some(dir)` binds the
/// socket under `dir` (the client launcher passes `server --socket-dir <dir>`).
/// `None` falls back to env-tier resolution.
///
/// **`session` (WS-C M2/M3b, §4.1) — REQUIRED:** the daemon ALWAYS runs in
/// SINGLE-SESSION mode now (the legacy shared-daemon mode is RETIRED, spec §1/§9).
/// It validates the identity, binds `<dir>/<name>.sock`, enforces capacity-1 +
/// identity errors, runs the claim-timeout, and performs the §4.1
/// exit-on-session-end ordering. There is no longer a `<dir>/qrmux.sock` bind path.
pub async fn run_server(
    socket_dir: Option<std::path::PathBuf>,
    session: String,
) -> anyhow::Result<()> {
    // Additive forwarder (R3c-Step-1): the legacy 2-arg entry keeps the bare qrmux
    // main.rs caller byte-unchanged. Only the dispatch daemon entry adopts
    // [`run_server_ctrl`] to bind the per-session control socket.
    run_server_ctrl(socket_dir, session, None).await
}

/// As [`run_server`], plus the R3c-Step-1 per-session control socket. `ctrl_sock`
/// is the canonical [`dispatch::control_sock::control_sock_path`] (keyed on
/// session_id, resolved by the dispatch daemon entry which CAN name the dispatch
/// crate); bare qrmux passes `None` and runs with no ctrl servicer. Binding it is
/// best-effort — a bind failure degrades the wake to the sender-side PTY-inject
/// fallback, it never fails the primary `.sock` serve.
pub async fn run_server_ctrl(
    socket_dir: Option<std::path::PathBuf>,
    session: String,
    ctrl_sock: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    // Ignore SIGHUP so SSH disconnects don't kill us
    use nix::sys::signal::{signal, SigHandler, Signal};
    // SAFETY: SIG_IGN is async-signal-safe for SIGHUP.
    unsafe { signal(Signal::SIGHUP, SigHandler::SigIgn) }
        .map_err(|e| anyhow::anyhow!("failed to ignore SIGHUP: {}", e))?;

    // WS-C M2: validate the --session identity at startup (charset + reserved +
    // budget). Refuse-don't-escape: a bad identity bails loudly, never binds.
    socket::validate_session_identity(&session)?;

    // Resolve the socket DIR explicitly (ack1-spec §1, red-team F3:
    // socket_path_for alone never binds the dir to a variable) — the ACK-1
    // events dir derives from this same resolved dir, C1 override seam
    // included. socket_dir_for is idempotent with the call inside
    // socket_path_for.
    let resolved_dir = socket::socket_dir_for(socket_dir.as_deref())?;
    // WS-C M2/M3b: single-session binds `<dir>/<name>.sock` (dynamic sun_path
    // budget checked here). The legacy shared `<dir>/qrmux.sock` bind is gone.
    let path = socket::session_socket_path_for(socket_dir.as_deref(), &session)?;

    // ACK-1: events context (None = QRMUX_EVENTS_DISABLED kill switch) and
    // the PTY-write fault layer (no-op identity unless env-armed; loud warn
    // when armed — rider R-a).
    let events_ctx = crate::events::EventsCtx::from_env(&resolved_dir);
    let fault_layer = fault::FaultLayer::from_env();
    // Only remove socket if it's stale (no server is listening).
    // This prevents a second server from yanking the socket out from under
    // an already-running server.
    //
    // TOCTOU note: there is a small race window between remove_file() and
    // bind() below where another process could create a socket at the same
    // path. This is acceptable because:
    //   1. retach is a single-user tool — concurrent server starts are rare
    //   2. If a race occurs, bind() fails with EADDRINUSE and the user retries
    //   3. Using O_EXCL or flock() would add complexity for a near-zero-probability scenario
    if path.exists() {
        match tokio::net::UnixStream::connect(&path).await {
            Ok(_) => {
                anyhow::bail!("another server is already running on {:?}", path);
            }
            Err(_) => {
                // Stale socket — safe to remove
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!(path = ?path, error = %e, "failed to remove stale socket");
                }
            }
        }
    }

    let listener = UnixListener::bind(&path)?;
    info!(path = ?path, "server listening");

    // RAII guard to clean up socket file on exit. MUTABLE so the shutdown
    // reorder's EARLY-UNLINK (step 3) can `take_and_unlink()` the path BEFORE the
    // bounded in-flight wait, while the final `Drop` (a no-op once taken) still
    // covers any early-error/panic path that returns before step 3 ([RT-F3]).
    let mut socket_guard = SocketGuard(Some(path.clone()));

    // R3c-Step-1: bind the per-session control socket (SOCK_DGRAM) ADJACENT to the
    // `<name>.sock` listener and BEFORE the registry-row advertise / any session
    // create, so there is no window where a `WakeInbox` arrives but no waker is
    // listening (R1 §5 inv 1; the confirmed design-(A) seam, step-0 verdict). The
    // `.sock` bind above already proved no live duplicate daemon, so any leftover
    // ctrl file is stale and safe to clear. Binding is best-effort: a failure logs
    // and runs without a ctrl servicer (the sender's PTY-inject fallback covers it),
    // never aborting the primary serve.
    let ctrl: Option<tokio::net::UnixDatagram> = match ctrl_sock.as_ref() {
        Some(cp) => match bind_ctrl_socket(cp) {
            Ok(sock) => {
                info!(path = ?cp, "control socket listening");
                Some(sock)
            }
            Err(e) => {
                warn!(path = ?cp, error = %e,
                    "failed to bind control socket; wake degrades to PTY-inject fallback");
                None
            }
        },
        None => None,
    };
    // Unlink the ctrl socket on daemon exit (mirrors SocketGuard for `.sock`).
    let _ctrl_guard = ctrl.is_some().then(|| {
        CtrlGuard(ctrl_sock.clone().expect("ctrl bound implies a path"))
    });

    let manager = Arc::new(Mutex::new(SessionManager::with_events(events_ctx)));

    // attended-UX M1 (RT-R1, QS-4): on-boot pending-delivery reconciliation.
    // A prior (crashed) incarnation of THIS session's mux may have left spooled
    // sends; resolve each to EXACTLY ONE honest terminal before serving. The hosted
    // session DIES with the mux (PTY-survival fact), so `session_alive = false` on a
    // fresh boot — a spooled send is NEVER re-injected (an unknown inject outcome
    // terminals honestly; a session-gone draft is retained + reported). Best-effort:
    // a sweep failure is logged and never aborts the serve.
    {
        use crate::attended::{driver, fire::TranscriptLandingProbe, spool, sweep, SystemClock};
        if let Some(state_dir) = driver::resolve_state_dir() {
            let spool_dir = resolved_dir.join("pending").join(&session);
            match spool::Spool::open(&spool_dir) {
                Ok(sp) => {
                    let session_name = session.clone();
                    let ledger_for = move |r: &spool::PendingRecord| {
                        driver::ledger_path(&state_dir, r.session.as_deref(), &session_name)
                    };
                    match sweep::reconcile_spool(
                        &sp,
                        &SystemClock,
                        &TranscriptLandingProbe,
                        false,
                        &ledger_for,
                    ) {
                        Ok(resolved) if !resolved.is_empty() => info!(
                            session = %session,
                            count = resolved.len(),
                            "attended: reconciled spooled pending sends on boot"
                        ),
                        Ok(_) => {}
                        Err(e) => warn!(session = %session, error = %e,
                            "attended: on-boot spool reconciliation failed"),
                    }
                }
                Err(e) => warn!(session = %session, error = %e,
                    "attended: could not open spool for on-boot reconciliation"),
            }
        }
    }

    // WS-C M2/M3b (§4.1): the per-connection DaemonCtx and the single-session
    // lifecycle plumbing. ALWAYS single-session now — the legacy shared mode is
    // retired (spec §9).
    let claim_reset = Arc::new(tokio::sync::Notify::new());
    let ended = Arc::new(AtomicBool::new(false));
    // §4.1 lost-reply fix: the in-flight counter the lifecycle waits on.
    let in_flight = client_handler::InFlight::new();
    let ctx = DaemonCtx {
        session: Some(Arc::from(session.as_str())),
        claim_reset: Some(claim_reset.clone()),
        ended: Some(ended.clone()),
        in_flight: Some(in_flight.clone()),
    };

    // WS-C M2 (§4.1): single-session lifecycle task.
    //  - claim phase: if the session is never created within the claim timeout
    //    (timer reset ONLY by session-addressed verbs via `claim_reset` — Hello
    //    probes do NOT reset it, red-team B1), fire `exit_signal` with the named
    //    log line. The reset pulse arrives BEFORE the create runs, so a create
    //    racing expiry wins.
    //  - end-watch phase: once the session has been claimed, watch the manager;
    //    when it becomes empty again (cleanup-task reap after ChildExited, or
    //    KillSession), the session has ended → set `ended` (refuse new connects,
    //    step 4) and fire `exit_signal`. Steps 2–3 (drain + events bookend +
    //    SessionEnded) are already done by the reader/cleanup/relay paths before
    //    the manager shows empty; the unlink (step 5) is the SocketGuard drop and
    //    exit 0 (step 6) is the run_server return.
    let exit_signal = Arc::new(tokio::sync::Notify::new());
    let lifecycle_handle = {
        let claim_reset = claim_reset.clone();
        let ended = ended.clone();
        let lc_manager = manager.clone();
        let exit_signal = exit_signal.clone();
        let claim_timeout = claim_timeout_from_env();
        tokio::spawn(async move {
            // Claim phase.
            let mut claimed = false;
            while !claimed {
                tokio::select! {
                    // A session-addressed verb arrived: reset the timer and
                    // check whether the session now exists.
                    _ = claim_reset.notified() => {
                        if !lc_manager.lock().await.is_empty() {
                            claimed = true;
                        }
                    }
                    _ = tokio::time::sleep(claim_timeout) => {
                        // Double-check under the lock: a create may have landed
                        // in the same instant as expiry.
                        if !lc_manager.lock().await.is_empty() {
                            claimed = true;
                        } else {
                            info!(
                                timeout_ms = claim_timeout.as_millis() as u64,
                                "unclaimed after {}ms — exiting",
                                claim_timeout.as_millis()
                            );
                            // Exit-reorder (addendum, orc-16): the in-flight
                            // wait MOVED out of this task into the shared
                            // post-accept-loop shutdown path. The claim-timeout
                            // arm now SIGNALS immediately; the post-loop path
                            // closes the listener + unlinks FIRST, THEN runs the
                            // bounded wait (an unclaimed daemon can still owe a
                            // ListSessions/error reply to a probe in flight at
                            // expiry — that reply drains inside the post-loop
                            // wait, same 7s bound, same named fail-closed line).
                            // [RT-F6.1] signal-during-claim follows the SAME
                            // reordered path; no claim-phase special-casing.
                            exit_signal.notify_one();
                            return;
                        }
                    }
                }
            }
            // End-watch phase: poll for the session to vanish. The cleanup task
            // (ChildExited reap) and KillSession both empty the manager AFTER
            // emitting the close bookend, so an empty manager here means steps
            // 2–3 are complete.
            loop {
                if lc_manager.lock().await.is_empty() {
                    // §4.1 step 4: refuse NEW connects from here on (deterministic,
                    // never a hang). Set BEFORE the in-flight wait so a racing
                    // connect gets the named session-ended error — this also stops
                    // NEW guards from meaningfully extending the wait (post-`ended`
                    // verbs fail fast through the same guarded path, bounded).
                    // (lifecycle paths only) set `ended` FIRST — BEFORE
                    // everything (exit-reorder step 1). New session-addressed
                    // verbs are refused with the named error from here on; this
                    // also stops NEW guards from meaningfully extending the wait
                    // (post-`ended` verbs fail fast through the same guarded path,
                    // bounded). Deterministic, never a hang.
                    ended.store(true, Ordering::Release);
                    // Exit-reorder (addendum, orc-16): the bounded in-flight WAIT
                    // (which replaced the deleted fixed 150ms grace) MOVED out of
                    // this task into the shared post-accept-loop shutdown path. We
                    // SIGNAL immediately here; the post-loop path closes the
                    // listener + unlinks the socket FIRST (step 3 — new arrivals
                    // get ENOENT clean-absent, a same-name relaunch can bind), THEN
                    // runs the bounded wait (step 4) so the KillSession handler's
                    // owed SessionKilled reply — held across a slow blocking drop —
                    // drains before process exit, bounded by `QRMUX_EXIT_WAIT_MS`
                    // (default DROP_BUDGET + 2s, derived to EXCEED the drop budget;
                    // expiry = named fail-closed-loud line, orc rider R-i).
                    info!("session ended — exiting");
                    exit_signal.notify_one();
                    return;
                }
                // The 25ms end-watch poll stays (the fast single-session reaper
                // cadence); only the fixed post-empty grace was replaced.
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
    };

    // Dead session cleanup task — drops dead sessions outside the lock.
    //
    // WS-C M2/M3b: single-session mode runs the reaper FAST (this is the path that
    // emits the ChildExited close bookend, §4.1 step 3, BEFORE the lifecycle task
    // observes the now-empty manager and exits). A slow sweep would stall
    // exit-on-end behind a half-minute; the single-session daemon is reaped as soon
    // as the bookend lands. `QRMUX_CLEANUP_INTERVAL_MS` still tunes it (ACK-1
    // R-REAP knob); the default is FAST (25ms) now that legacy mode is gone — the
    // 30s production cadence the env default once carried no longer applies.
    let cleanup_interval = cleanup_interval_from_env();
    let cleanup_manager = manager.clone();
    let cleanup_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(cleanup_interval);
        loop {
            interval.tick().await;
            let dead_sessions = {
                let mut mgr = cleanup_manager.lock().await;
                mgr.take_dead_sessions()
            };
            if !dead_sessions.is_empty() {
                // ACK-1 close bookend (spec §2 C2): the reaper found the PTY
                // child dead — emit BEFORE the blocking drop (the Drop belt
                // would otherwise record the less-specific "dropped").
                for s in &dead_sessions {
                    if let Some(ev) = s.events_writer() {
                        ev.emit_closed(crate::events::CloseReason::ChildExited);
                    }
                }
                drop_blocking_with_timeout(dead_sessions, "dead session cleanup").await;
            }
        }
    });

    // ACK-1 optional heartbeat (spec §2 H): DEFAULT OFF; armed by
    // QRMUX_EVENTS_HEARTBEAT_MS. Emits via the same EventWriter path (seq/cap
    // rules apply); post-close emits are no-ops (benign teardown race).
    let heartbeat_handle = std::env::var("QRMUX_EVENTS_HEARTBEAT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| {
            let hb_manager = manager.clone();
            tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_millis(ms.max(1)));
                loop {
                    interval.tick().await;
                    let writers = {
                        let mgr = hb_manager.lock().await;
                        mgr.event_writers()
                    };
                    for w in writers {
                        if !w.is_closed() {
                            w.emit_heartbeat();
                        }
                    }
                }
            })
        });

    // Graceful shutdown via signals
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // [RT-F2 MAJOR fold — bound selection, NORMATIVE]: exit_signal / SIGTERM /
    // SIGINT converge on a bare `break`, but the SHARED post-loop wait cannot know
    // which bound applies. Each arm CAPTURES its bound into `exit_bound` at break;
    // the post-loop wait uses the captured value. Neither bound may silently
    // regress into the other (lifecycle/claim = exit_wait_budget() [7s derived];
    // signal = SIGNAL_INFLIGHT_WAIT [1s]).
    let exit_bound: std::time::Duration;
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let manager = manager.clone();
                        let fault = fault_layer.clone();
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                client_handler::handle_client(stream, manager, fault, ctx).await
                            {
                                warn!(error = %e, "client error");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "accept failed, retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            // R3c-Step-1 control-socket arm (the confirmed design-(A) seam). Active
            // ONLY when a ctrl socket is bound (`if ctrl.is_some()`); `recv_ctrl`
            // parks forever when None, so the guard + the pending future keep this
            // arm inert for bare qrmux. The handler clones the wake handle UNDER the
            // `manager` lock then RELEASES it before waking — so a real wedge (or a
            // blocking PTY write) can never starve THIS select loop (the keystone,
            // step-0 verdict point 2 / §3 R3c-0).
            opcode = recv_ctrl(&ctrl), if ctrl.is_some() => {
                if let Some(op) = opcode {
                    handle_ctrl_op(op, &session, &manager).await;
                }
            }
            // WS-C M2/M3b (§4.1): single-session claim-timeout or exit-on-session-end.
            // Lifecycle/claim arm — the 7s derived exit-on-end bound ([RT-F2]).
            _ = exit_signal.notified() => {
                info!("single-session lifecycle requested exit");
                exit_bound = exit_wait_budget();
                break;
            }
            // Signal arms — the SHORTER 1s SIGNAL_INFLIGHT_WAIT ([RT-F2]/[F9]): an
            // operator kill wants the daemon gone; one second flushes any
            // non-pathological owed reply. [RT-F6.1] a SIGTERM/SIGINT DURING the
            // claim phase lands here too and follows the SAME reordered path.
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                exit_bound = SIGNAL_INFLIGHT_WAIT;
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down");
                exit_bound = SIGNAL_INFLIGHT_WAIT;
                break;
            }
        }
    }

    // ===== Exit-reorder (addendum, orc-16 relay-1780801873547-39): the SHARED
    // post-accept-loop shutdown path, run for ALL exit reasons (exit-on-end,
    // claim-timeout, SIGTERM/SIGINT). Order is NORMATIVE:
    //
    //   step 3: CLOSE THE LISTENER + UNLINK THE SOCKET FIRST. New arrivals now get
    //           ENOENT = clean-absent (spec-blessed D-LISTRAW shape; the launcher
    //           §4.2(d) treats it as absent→relaunch). dropping the `UnixListener`
    //           stops NEW accepts ONLY — it does NOT abort the tasks of
    //           already-accepted connections (each owns its own stream), so their
    //           owed replies still drain in step 4.
    //   step 4: THEN the bounded in-flight wait, using the bound the breaking arm
    //           CAPTURED ([RT-F2]): exit-on-end/claim = 7s; SIGTERM/SIGINT = 1s.
    //           Already-accepted connections' owed replies/refusals drain inside
    //           it; expiry = the named fail-closed-loud line (orc rider R-i).
    //   step 5: abort tasks, drain_all, return (process exit), below.
    //
    // NAMED RESIDUAL (ruled accept-as-residual, [RT-F4]): a frame-decode on an
    // ALREADY-ACCEPTED connection racing the final zero-observation can still be
    // cut (closing it needs connection-draining machinery — diminishing returns);
    // and during the ENOENT window the OLD draining daemon and a same-name
    // successor can both hold append handles to `<name>.log` (O_APPEND keeps it
    // line-ish — forensic surface only, not a contract). Both are accepted
    // residuals, carried as soak-ledger rows at merge. =====

    // step 3 — early-unlink, then close the listener.
    socket_guard.take_and_unlink();
    drop(listener);

    // step 4 — the bounded in-flight wait, using the captured per-arm bound. Owed
    // replies/refusals on already-accepted connections drain here; on the
    // exit-on-end/claim paths the lifecycle task has already set `ended`, so no
    // NEW guarded work can meaningfully extend it.
    wait_in_flight_or_log(&in_flight, exit_bound).await;

    // step 5 — cancel the cleanup + lifecycle tasks before draining sessions
    cleanup_handle.abort();
    let _ = cleanup_handle.await;
    lifecycle_handle.abort();
    let _ = lifecycle_handle.await;
    if let Some(h) = heartbeat_handle {
        h.abort();
        let _ = h.await;
    }

    // Explicitly drop all sessions on a blocking thread with a timeout,
    // so server shutdown doesn't hang if child processes are unresponsive.
    let all_sessions: Vec<crate::session::Session> = {
        let mut mgr = manager.lock().await;
        mgr.drain_all()
    };
    if !all_sessions.is_empty() {
        info!(
            count = all_sessions.len(),
            "cleaning up sessions on shutdown"
        );
        // ACK-1 close bookend (spec §2 C3): graceful daemon shutdown.
        for s in &all_sessions {
            if let Some(ev) = s.events_writer() {
                ev.emit_closed(crate::events::CloseReason::DaemonShutdown);
            }
        }
        drop_blocking_with_timeout(all_sessions, "shutdown session cleanup").await;
    }

    Ok(())
    // `socket_guard` drops here: a NO-OP on the graceful paths (step 3's
    // early-unlink already `take()`'d the path), so it can never delete a
    // same-name successor's live socket. It still unlinks on any early-error /
    // panic path that returned before step 3 ([RT-F3] unlink-exactly-once).
}

/// RAII guard that removes the socket file â [RT-F3 fold, exit-reorder spec
/// addendum; orc-16 relay-1780801873547-39]. The shutdown reorder unlinks the
/// socket EARLY (before the bounded in-flight wait) so new arrivals get ENOENT =
/// clean-absent and a same-name relaunch can legitimately bind a FRESH
/// `<name>.sock` at the same path WHILE the old daemon is still draining. If the
/// old daemon's `Drop` then `remove_file`'d the path unconditionally it would
/// DELETE THE NEW DAEMON'S LIVE SOCKET (the ★ double-unlink hazard).
///
/// INVARIANT: unlink EXACTLY ONCE, via WHICHEVER of the early-unlink (step 3) or
/// `Drop` runs FIRST. The `Option<PathBuf>` + [`Self::take_and_unlink`] shape
/// delivers BOTH directions for free:
///   - normal graceful exit: the early-unlink `take()`s the path and removes it;
///     the final `Drop` sees `None` and is a no-op — it can NEVER delete a
///     successor's socket.
///   - early-error / panic paths that never reach step 3: the path is still
///     `Some`, so `Drop` unlinks it (the socket does not leak). The earlier
///     "final drop is a no-op" phrasing was BACKWARDS for exactly these paths;
///     red-team proved panic-safety follows from this shape.
struct SocketGuard(Option<std::path::PathBuf>);

impl SocketGuard {
    /// Early-unlink (reorder step 3): take the path out and `remove_file` it NOW,
    /// BEFORE the bounded in-flight wait. Idempotent — a second call (or the
    /// final `Drop`) finds `None` and does nothing, so a same-name successor that
    /// binds the path while we drain is NEVER clobbered (the double-unlink defusal).
    fn take_and_unlink(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        // Unlinks ONLY if the early-unlink did not already run (Option::take left
        // it Some): the unlink-exactly-once-via-whichever-runs-first invariant.
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(&path);
        }
    }
}
