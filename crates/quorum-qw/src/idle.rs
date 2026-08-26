//! `LaneOps::await_idle` — the four per-harness idle watchers, and the two
//! delivery OBSERVERS that ride with them.
//!
//! # What moved here, and why it had to
//!
//! These four bodies were `qd`'s `verbs/wait.rs`. Two of them do not merely READ
//! qw's delivery log — the ACP arm and the pi arm **write** `message-seen` into
//! it — which made them qw-owned emitters sitting inside a qd verb, and made
//! `qd wait` the last shared-file path across the split. Ruling D2
//! (`doc/tbd/provider-architecture/11-stage3-plan.md`) grew a method whose SUBJECT
//! is the session so they could travel: see [`crate::contract::LaneOps::await_idle`]
//! and [`crate::contract::TurnState`].
//!
//! # D8, and the line it draws through this module
//!
//! **A budget belongs to the caller; the ledger belongs to the session.** Nothing
//! here writes anything because a budget elapsed —
//! [`TurnState::BudgetElapsed`](crate::contract::TurnState::BudgetElapsed) is a
//! fact about the observer and corresponds to no record.
//!
//! The ACP and pi observers DO emit, and that is the distinction rather than an
//! exception: [`emit_acp_terminal`] fires on a turn TERMINAL pulled off the
//! residence socket, and the pi observer
//! ([`crate::delivery::pi::observe_landed_sends`]) fires on the sent bytes being
//! FOUND as a user record in the resident's rollout. Both are evidence about a
//! send, in qw's own log. Neither is reachable from the timeout arm — grep the
//! deadline branches: they `return` before any emitter.
//!
//! # These functions print, and that is deliberate
//!
//! An answer that can take two minutes to arrive has to say so WHILE it is
//! arriving, and only the watcher knows whether it is going to wait at all — the
//! entry-idle arms return without ever opening a progress line. So each arm that
//! decides to wait writes `Waiting for <label>...` (no newline) to stderr before
//! it blocks, and the CALLER closes that line from the returned
//! [`TurnState`](crate::contract::TurnState). That is the same split
//! [`crate::delivery::pty`] already runs on `qd send:pty --wait`, and it is safe
//! across the wire for the same reason: qw's stderr is INHERITED, its stdout is
//! the protocol. **Nothing here may write stdout.**
//!
//! # What stayed in qd
//!
//! - The codex ENTRY gate. `run_wait`'s short-circuit reads the JOIN's
//!   `session.status` — a cross-backend gather qd owns and a `SessionId` cannot
//!   reconstruct — and prints on stdout. It never opens a socket, so keeping it
//!   costs a codex row nothing.
//! - Every line naming the label, except the progress prefix above.
//! - `verify_post_resume_if_marked`, the ACP bridge-continuation check: it runs
//!   AFTER the ` done` the caller prints, and its verdict replaces the verb's exit
//!   code.
//! - The A6 `invoked` telemetry line.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::contract::{LaneError, SessionId, TurnState};
use crate::effects::{Env, RealClock};
use crate::model::{Session, SessionStatus};
use crate::paths::QdPaths;
use crate::wait::{
    entry_is_idle, run_wait_content_loop, ChannelStatusObservation, ChannelStatusSource,
    ChannelTurnCompletion, PidFileStatus, RealTurnEndProbe, RealWaitContentDeps, TurnCompletion,
    TurnCompletionProbe, WaitStatusOutcome,
};

/// Bound on how long the entry-idle gate (RESIDUAL #1) waits for the live channel
/// to SETTLE — `await_status` returns the moment the first `Republish*` frame lands
/// (Live) or the reader thread finishes without connecting (Down). Covers the
/// connect + first-frame latency of a real headless turn; a non-headless session
/// (no socket / daemon refuses) decides Down well under this, adding no latency.
const ENTRY_SETTLE: Duration = Duration::from_millis(1500);

/// The pi wait poll floor (B1 clause 5 — pi-native timing). A daemon-hosted pi
/// session is gated by re-reading `get_state().is_streaming` over the resident ws
/// front on pi's OWN ~150ms cadence (the resident's `POLL_INTERVAL`), NEVER a
/// claude-shaped in-process "promptly" bar. A resident + ws + point-read has this
/// floor by architecture; the loop honors it rather than busy-spinning.
const PI_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// The wait default a 0/absent `--timeout` has always meant. `budget_ms == 0` is
/// "the caller passed no bound", never "do not wait" — see
/// [`crate::contract::LaneOps::await_idle`].
const DEFAULT_TOTAL: Duration = Duration::from_secs(120);

/// The verb every `qd <verb>:`-attributed line in this module names.
///
/// The four bodies here are the arms of
/// [`LaneOps::await_idle`](crate::contract::LaneOps::await_idle), and its only
/// caller is `bin/qd/verbs/wait.rs::run_wait` — so `wait` is the command the
/// user typed on every path through this file. It is a constant rather than a
/// parameter because the contract method takes no verb; threading one through
/// `LaneOps::await_idle` is the shape this module would prefer, and that edit
/// lands in `contract.rs`, so it is reported rather than made here.
const WAIT_VERB: &str = "wait";

/// Open the progress line. The caller closes it from the [`TurnState`].
fn open_progress(label: &str) {
    eprint!("Waiting for {label}...");
    let _ = std::io::stderr().flush();
}

// ── THE TWO REFUSALS ───────────────────────────────────────────────────────
//
// An idle watcher can refuse in exactly two ways, and they cross as DIFFERENT
// `LaneError` variants rather than as one variant with a discriminating string —
// because the verb renders two different lines, and a caller that had to recover
// the class by matching on prose would be one rewording away from printing the
// wrong one.
//
//   - `LaneError::Cold`      — there is no live process to wait on at all.
//                              "Session has no PID (cold/dead). Nothing to wait
//                              for."
//   - `LaneError::Transport` — the row exists but its daemon could not be
//                              reached, identified, or connected to. "… session
//                              daemon not reachable (try qd resume …)", which
//                              names a recovery act because the state is
//                              recoverable.
//
// `detail` is carried for a caller that wants it; `qd wait`'s wording predates
// this seam and does not print it.

/// There is no live process behind this row — nothing to wait on.
fn no_pid(id: &SessionId) -> LaneError {
    LaneError::Cold { id: id.clone() }
}

/// The row's daemon could not be reached, identified, or connected to.
fn unreachable(detail: &str) -> LaneError {
    LaneError::Transport {
        detail: detail.to_string(),
    }
}

/// The `--timeout` knob as a duration, honouring the 0-means-default rule.
fn total_budget(budget_ms: u64) -> Duration {
    if budget_ms > 0 {
        Duration::from_millis(budget_ms)
    } else {
        DEFAULT_TOTAL
    }
}

// ===========================================================================
// claude-code — the pid-file/channel status + transcript-content poll
// ===========================================================================

/// RESIDUAL #2 (WP-B-CS-1): the prod §6.0 ENTRY-GATE resolver, extracted from the
/// arm so the invariant is TEST-GUARDED. B-CS-1 makes the §6.0 flip LIVE, so a
/// regression that lets a disk-sourced status decide entry-idle on a healthy
/// channel must RED, not go silent. Thin delegate over [`entry_is_idle`]: on a
/// `Live` channel the daemon-written status decides and `disk_idle` is NEVER
/// consulted; `Down` is the ONLY path that reads disk (channel-down mode).
fn entry_gate_idle(entry_channel: ChannelStatusObservation, disk_idle: impl FnOnce() -> bool) -> bool {
    entry_is_idle(entry_channel, disk_idle)
}

/// RESIDUAL #2 (WP-B-CS-1): the prod §6.0 STATUS-SOURCE composition, extracted
/// from the arm. On the healthy path `status_source` is the channel seam (the
/// loop sources control status off the daemon channel, NOT `pid.json`); `None`
/// (no subscriber) is honest channel-DOWN mode, where `status_fallback` reads the
/// disk. The §H.2 purity invariant lives or dies on this wiring — guard it: a
/// regression that drops `status_source` to `None` while a live channel exists
/// silently reverts `qd wait` to a disk-as-status read, exactly the bug class
/// (B) eliminates.
fn build_wait_deps<'a>(
    status_source: Option<Box<dyn ChannelStatusSource>>,
    pid_file: std::path::PathBuf,
    completion: Option<Box<dyn TurnCompletion>>,
    clock: &'a RealClock,
    sleeper: &'a crate::boot::RealSleeper,
) -> RealWaitContentDeps<'a> {
    RealWaitContentDeps {
        status_source,
        status_fallback: Box::new(PidFileStatus { pid_file }),
        completion,
        clock,
        sleeper,
    }
}

/// A [`TurnCompletion`] that never reports completion evidence — the channel-DOWN
/// fallback used when a live subscriber exists but NO transcript was resolvable, so
/// the channel `result` seam stays load-bearing on the healthy path while the
/// down-path degrades to status-only (today's exact no-transcript behavior).
struct NoTranscriptFallback;
impl TurnCompletion for NoTranscriptFallback {
    fn poll_completion(&self) -> TurnCompletionProbe {
        TurnCompletionProbe::Pending
    }
}

/// The claude-code idle watcher (status.ts:214-260 + 359-390) — status +
/// transcript-content keyed poll.
pub fn await_idle_claude(
    env: &dyn Env,
    paths: &QdPaths,
    session: &Session,
    label: &str,
    budget_ms: u64,
) -> Result<TurnState, LaneError> {
    // WP-B2b-2b: the live republish subscriber — ONE socket connection backing BOTH
    // B3 loop seams (control status + turn-completion) AND the entry-idle gate
    // (RESIDUAL #1), so "channel-down" is a SINGLE shared truth. Name-gated (no name
    // → no channel) and ONLY when the session's daemon socket actually EXISTS — a
    // non-headless claude session has none, so we skip the connect entirely:
    // status_source stays None = today's exact disk-keyed wait, no thread spawned.
    // The subscriber must outlive the loop (kept in `subscriber`).
    let subscriber = session.name.as_deref().and_then(|name| {
        let dir = crate::qrmux_dir::resolve_qrmux_dir(&paths.home, env).ok()?;
        let socket_path = qrmux::server::session_socket_path_for(Some(&dir), name).ok()?;
        socket_path
            .exists()
            .then(|| crate::wait_channel::ChannelSubscriber::connect(socket_path, name.to_string()))
    });

    // Entry-idle gate (RESIDUAL #1, BINDING): on the HEALTHY channel the
    // daemon-written control status decides whether the session is ALREADY idle; the
    // disk-resolved `session.status` is the channel-DOWN fallback ONLY — no control
    // decision rests on a disk-sourced status outside explicit channel-down mode
    // (the §6.0 invariant, EXTENDED to the pre-loop gate B3's loop-purity test does
    // not cover; §H.2 entry-idle purity test). `await_status` settles a real
    // headless turn onto the channel before we'd fall back to disk; a non-headless
    // session (no socket / daemon refuses) decides Down fast, no added latency.
    let entry_channel = subscriber
        .as_ref()
        .map(|s| s.await_status(ENTRY_SETTLE))
        .unwrap_or(ChannelStatusObservation::Down);
    if entry_gate_idle(entry_channel, || session.status == SessionStatus::Idle) {
        return Ok(TurnState::IdleAtEntry);
    }

    // No pid → nothing to wait for (status.ts:354-357).
    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        return Err(no_pid(&SessionId(session.session_id.clone())));
    };

    let pid_file = paths.sessions_dir.join(format!("{pid}.json"));

    // W6+W7 (ADD-15): resolve the transcript for the content key — the SAME
    // resolution chain as send:pty --wait (send.rs): the session's recorded
    // jsonl_path (exists-filtered), else find_jsonl_path. The entry-time offset
    // bounds the evidence window (only records written AFTER wait began count —
    // the stale-record guard). Unresolvable → warn ONCE, run status-only
    // (today's exact pre-fix behavior; degradation contract in wait.rs).
    // codex P1 W7 (codex-p1-spec section 7.2): the transcript-location fallback
    // dispatches through the provider seam, keyed on this row's provider value.
    // `wait` is DELIBERATELY UNARMED (codex-p1-spec section 2.3 lists the acting
    // verbs — wait is excluded; it has no `refuse_unknown_provider`). So we
    // `provider_for(...).unwrap_or` the claude default: an unknown-provider row
    // degrades to status-only waiting EXACTLY as an unresolvable transcript
    // already does (the `jsonl.is_none()` warn-once below) — never a NEW error
    // surface (L8). The resolved path is byte-identical to the old
    // `find_jsonl_path` call (ClaudeProvider::transcript_path delegates).
    let provider = crate::provider::provider_for(&session.provider)
        .unwrap_or_else(|| crate::provider::provider_for("claude-code").unwrap());
    // The (A) transcript visibility barrier — the channel-DOWN turn-completion
    // fallback (and today's whole-impl when no subscriber exists).
    let probe: Option<Box<dyn TurnCompletion>> = session
        .jsonl_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
        .or_else(|| {
            provider.transcript_path(
                &paths.projects_dir,
                &crate::provider::SessionKey {
                    id: &session.session_id,
                    name: session.name.as_deref(),
                    cwd: session.cwd.as_deref(),
                    pid: session.pid,
                },
            )
        })
        .map(|p| {
            let entry_offset = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            Box::new(RealTurnEndProbe::new(p, entry_offset)) as Box<dyn TurnCompletion>
        });
    if probe.is_none() {
        eprintln!("qd {WAIT_VERB}: no transcript found for this session — status-keyed only");
    }

    // WP-B2b-2b deliverable 2 — the B3 disk-free FLIP. With a live subscriber the
    // completion evidence is the daemon-republished `result` event (turn_source);
    // the (A) transcript barrier survives ONLY as the channel-DOWN fallback. No
    // subscriber (non-headless / no socket) → the bare probe = today's channel-down
    // mode. Subscriber but no transcript → a never-visible fallback keeps the
    // channel `result` seam live while degrading the down-path to status-only.
    let completion: Option<Box<dyn TurnCompletion>> = match &subscriber {
        Some(sub) => Some(Box::new(ChannelTurnCompletion {
            source: sub.turn_source(),
            fallback: probe
                .unwrap_or_else(|| Box::new(NoTranscriptFallback) as Box<dyn TurnCompletion>),
        })),
        None => probe,
    };

    open_progress(label);

    let clock = RealClock;
    let sleeper = crate::boot::RealSleeper;
    // WP-B2b-2b deliverable 2 (R1-a): `status_source = Some(..)` on the healthy path
    // sources the control status off the SAME subscriber that backs the completion
    // seam above — flipping `qd wait`'s control path off disk. `None` (no subscriber)
    // = honest channel-DOWN mode: `pid.json` (StatusFallback) + the (A) transcript
    // barrier, exactly as before. No control decision rests on a disk status outside
    // channel-down mode (the §6.0 invariant; §H.2 purity test).
    let deps = build_wait_deps(
        subscriber.as_ref().map(|s| s.status_source()),
        pid_file,
        completion,
        &clock,
        &sleeper,
    );

    // `timeout_ms` stays the loop's own i64 contract (0 = its own default).
    // The loop takes the verb rather than hardcoding one (see
    // `wait::run_wait_content_loop`); this is the caller that supplies it.
    Ok(
        match run_wait_content_loop(&deps, WAIT_VERB, budget_ms as i64, 500) {
            WaitStatusOutcome::Done => TurnState::WentIdle,
            WaitStatusOutcome::SessionExited => TurnState::SessionExited,
            WaitStatusOutcome::Timeout => TurnState::BudgetElapsed,
        },
    )
}

// ===========================================================================
// codex — `thread/status/changed` + the rollout-tail fallback
// ===========================================================================

/// A minimal `ProviderFx` for resolving the codex `transcript_root` off env only
/// (codex's `transcript_root` reads `fx.env` $CODEX_HOME/$HOME — never paths).
fn codex_root_fx<'a>(env: &'a dyn Env, paths: &'a QdPaths) -> crate::provider::ProviderFx<'a> {
    crate::provider::ProviderFx {
        await_relay: None,
        env,
        paths,
        socket_dir: paths.sessions_dir.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    }
}

/// codex P2 W6 (codex-p2-spec section 7.6 wait paragraph): block until a codex
/// thread goes IDLE. Connect a fresh ws client to the row's recorded endpoint
/// (re-read by pid — endpoint is NOT on the Session/--json surface) + initialize;
/// observe `thread/status/changed` broadcasts until idle. If the connect fails or
/// the daemon drops, the loop's deps fall back to polling the rollout tail
/// (`derive_status`) — the same connectionless path W5's ls uses, so the wait
/// still resolves if the daemon is gone. The existing timeout knob is honored.
///
/// The ENTRY-idle short-circuit is NOT here: it is the join's, and it stays in
/// `qd`'s verb (see the module docs).
pub fn await_idle_codex(
    env: &dyn Env,
    paths: &QdPaths,
    session: &Session,
    label: &str,
    budget_ms: u64,
) -> Result<TurnState, LaneError> {
    use crate::provider::codex::{AppServerRpc, ClientInfo, WsAppServer};
    use crate::provider::Provider;
    use crate::wait::{run_codex_wait_loop, RealCodexWaitDeps};

    // Resolve the rollout path (the fallback channel + the live anchor): the row's
    // recorded jsonl_path (exists-filtered), else CodexProvider::transcript_path
    // under its $CODEX_HOME/sessions root.
    let provider = crate::provider::codex::CodexProvider;
    let rollout_path = session
        .jsonl_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
        .or_else(|| {
            let fx = codex_root_fx(env, paths);
            let root = provider.transcript_root(&fx);
            let key = crate::provider::SessionKey {
                id: &session.session_id,
                name: session.name.as_deref(),
                cwd: session.cwd.as_deref(),
                pid: session.pid,
            };
            provider.transcript_path(&root, &key)
        });

    // The endpoint (live channel): the registry row's recorded endpoint, re-read by
    // pid. A dead/cold row (no pid / no endpoint) goes straight to the rollout-tail
    // fallback (the deps start fell-back when rpc is None).
    let endpoint = session
        .pid
        .filter(|&p| p != 0)
        .and_then(|pid| crate::registry::read_entry(&paths.sessions_dir, pid))
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty());

    // Best-effort connect + initialize. A failure → rpc None → the deps start on the
    // rollout-tail channel (daemon-unreachable fallback, codex-p2-spec section 7.6).
    let connected: Option<WsAppServer> = endpoint.as_deref().and_then(|ep| {
        let rpc = WsAppServer::connect(ep, Duration::from_secs(5)).ok()?;
        let client = ClientInfo {
            name: "qd-manager".to_string(),
            title: None,
            version: "0".to_string(),
        };
        if rpc.initialize(&client).is_err() {
            return None;
        }
        let _ = rpc.initialized();
        Some(rpc)
    });

    open_progress(label);

    let clock = RealClock;
    let sleeper = crate::boot::RealSleeper;
    let rpc_ref: Option<&dyn AppServerRpc> = connected.as_ref().map(|c| c as &dyn AppServerRpc);
    let deps = RealCodexWaitDeps::new(
        rpc_ref,
        rollout_path,
        // Per-poll notification read bound: short enough to re-check the deadline
        // promptly, long enough to catch a broadcast (a turn that runs long stays
        // Busy on the rollout tail anyway).
        Duration::from_millis(500),
        &clock,
        &sleeper,
    );

    let outcome = run_codex_wait_loop(&deps, budget_ms as i64, 500);
    if let Some(c) = connected {
        let _ = c.close();
    }
    Ok(match outcome {
        WaitStatusOutcome::Done => TurnState::WentIdle,
        WaitStatusOutcome::Timeout => TurnState::BudgetElapsed,
        // UNREACHABLE by construction — the codex loop has no pid file and the
        // rollout tail is the truth even if the daemon is gone, so it never
        // observes a session exit. The old verb rendered it as ` timeout` beside
        // the real timeout, which was the only answer an exit code could carry;
        // now that the answer is a type, it is the honest variant instead. Both
        // still print ` timeout`, so no byte moves — what changed is that a
        // reader can tell "the budget ran out" from "the loop reported something
        // it cannot observe".
        WaitStatusOutcome::SessionExited => TurnState::Undetermined {
            reason: "the codex wait loop reported a session exit it has no source for".to_string(),
        },
    })
}

// ===========================================================================
// scoped-ACP-CC residence WAIT (S7) — pull `next_update` over the residence socket
// ===========================================================================

/// (N-O3, Item 3) — the pure observation source the completion probe consumes: map ONE
/// `next_update` pull (the SAME real terminal observation the raw loop already saw) onto
/// the SC-2 [`AcpTurnObservation`]. This is THE distinct (O3) revert seam: corrupting
/// this mapping (e.g. always `Pending`, or `Terminal`→`Pending`) makes the wait verdict
/// diverge from the raw pull — the redundancy the oracle reverts here to catch. NO new
/// completion source: it consumes the existing `next_update`, just routed through the
/// completion contract.
///
/// [`AcpTurnObservation`]: crate::provider::acp::AcpTurnObservation
fn acp_wait_observation(
    res: Result<Option<crate::provider::acp::AcpEvent>, crate::provider::acp::AcpError>,
) -> crate::provider::acp::AcpTurnObservation {
    use crate::provider::acp::{AcpEvent, AcpTurnObservation};
    match res {
        // A correlated protocol terminal (stopReason) → the turn completed (SC-2a).
        Ok(Some(AcpEvent::Terminal { stop, .. })) => AcpTurnObservation::Terminal(stop),
        // A bridge JSON-RPC error terminal → Failed (DISTINCT from a clean completion).
        Ok(Some(AcpEvent::TerminalError { message, .. })) => AcpTurnObservation::Failed(message),
        // Streaming update / a quiet poll → still in flight (idle-without-evidence rule).
        Ok(Some(AcpEvent::Update { .. })) | Ok(None) => AcpTurnObservation::Pending,
        // Transport gone before any terminal → source integrity lost (never a false done).
        Err(_) => AcpTurnObservation::TransportLost("channel closed".into()),
    }
}

/// A single-shot `AcpTurnObserver` holding ONE mapped observation — lets the arm
/// (which drives the ws connection, not the host) feed `AcpTurnCompletion` the SAME
/// observation the raw `next_update` produced.
struct OneShotObserver(crate::provider::acp::AcpTurnObservation);
impl crate::provider::acp::AcpTurnObserver for OneShotObserver {
    fn observe(&self) -> crate::provider::acp::AcpTurnObservation {
        self.0.clone()
    }
}

/// The DELIVERY disposition for an ACP terminal, WIRED to the SETTLED in-tree
/// classification `TerminalReason` (rpc.rs `StopReason::to_terminal_reason` +
/// completion.rs — commit 94a827a1) — ASSERTED verbatim in conformance, NEVER
/// re-derived here. Option B (coord ruling; a new success-with-reason terminal
/// kind is a gate-item-5 vocabulary change deferred to C6/D3): a clean EndTurn
/// (Completed — the STRONG turn-consumption reading, the StopReason answers the
/// session/prompt that carried the message) AND a MaxTokens/MaxTurnRequests limit
/// are both LANDED → `message-seen`; the limit reason is preserved in
/// `TerminalReason` (conformance asserts MaxTokens ≠ Completed), never laundered
/// into a clean completion. Cancelled / Refusal / terminal-time Failed (a
/// correlated JSON-RPC error response) / Crashed / TransportLost / unparseable are
/// delivery-FAILURE candidates — but rider-3 REFINES the R5 assumption that an
/// interrupted turn "never proves entry": these are POST-delivery, so
/// [`emit_acp_terminal`] LANDING-CHECKS them against the ACP `~/.claude/projects`
/// record (a Cancelled turn adds the user prompt BEFORE interrupting the response),
/// and only a provably-not-landed one becomes `seen-failed`.
enum AcpDeliveryDisposition {
    /// Delivered + landed → `message-seen` (no landing check — a completed/limit
    /// turn consumed the prompt).
    Seen,
    /// A FAILURE StopReason — its `seen-failed` reason IF the rider-3 landing check
    /// proves the prompt did NOT land; landed → `message-seen`; ambiguous →
    /// recoverable (see [`emit_acp_terminal`]).
    Failed(&'static str),
}

fn acp_delivery_disposition(
    reason: crate::provider::acp::TerminalReason,
) -> AcpDeliveryDisposition {
    use crate::provider::acp::TerminalReason as R;
    match reason {
        R::Completed | R::MaxTokens => AcpDeliveryDisposition::Seen,
        R::Cancelled => AcpDeliveryDisposition::Failed("cancelled"),
        R::Refusal => AcpDeliveryDisposition::Failed("refusal"),
        R::Failed => AcpDeliveryDisposition::Failed("failed"),
        R::Crashed => AcpDeliveryDisposition::Failed("crashed"),
        R::TransportLost => AcpDeliveryDisposition::Failed("transport-lost"),
    }
}

/// The turn id carried by a terminal `AcpEvent` (the delivery `send_id`), if any.
fn acp_terminal_turn_id(
    raw: &Result<Option<crate::provider::acp::AcpEvent>, crate::provider::acp::AcpError>,
) -> Option<String> {
    use crate::provider::acp::AcpEvent;
    match raw {
        Ok(Some(AcpEvent::Terminal { turn, .. }))
        | Ok(Some(AcpEvent::TerminalError { turn, .. })) => Some(turn.clone()),
        _ => None,
    }
}

/// rider-3 landing verdict for a POST-delivery ACP FAILURE StopReason: did the
/// prompt LAND in the session's `~/.claude/projects` record (the native CC store
/// the ACP bridge writes CC-shaped JSONL into — provider/acp/mod.rs)?
enum AcpLanded {
    /// The prompt is present as a USER record (content-keyed) → it entered context.
    Yes,
    /// The transcript is resolvable + readable and the prompt is ABSENT. This is a
    /// `No` ONLY under the write-ordering assumption (user record precedes the
    /// terminal response) — which is UNPROVABLE on this box (bridge absent). So at
    /// R6 the `No` arm is DEGRADED and treated exactly like `Unknown` (recoverable);
    /// the variant is kept so the hard-fail is a one-line re-enable once the ordering
    /// is pinned by the live cell.
    No,
    /// The transcript is not resolvable / unreadable → we cannot prove either way →
    /// leave the send RECOVERABLE (never a foreclosing hard fail).
    Unknown,
}

/// rider-3 (qd-ctl3's tripwire ruling) — the content-keyed LANDING check for the
/// ACP lane, modeled on the relay observer's `landed_message_ids`
/// (relay_server/mod.rs): resolve the session's CC transcript under `projects_dir`,
/// then look for a USER record whose text matches the prompt's `content_sha256`.
/// The ACP inject sends the RAW prompt text as a single `ContentBlock` — no channel
/// wrapper, no `from` prefix (provider/acp/client.rs) — so `sha256(user text) ==
/// content_sha256` for a landed prompt. Same extractor (`sendpty::user_record_text`)
/// the relay observer uses on the recipient's CC transcript.
fn acp_prompt_landed(
    projects_dir: &std::path::Path,
    session_id: &str,
    content_sha256: &str,
) -> AcpLanded {
    let Some(path) = crate::jsonl::find_jsonl_path(projects_dir, session_id, None) else {
        return AcpLanded::Unknown; // transcript not resolvable → recoverable
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return AcpLanded::Unknown; // unreadable → recoverable
    };
    for p in crate::sendpty::parse_jsonl_slice(&content) {
        let rec: crate::sendpty::JsonlRecord =
            serde_json::from_value(p.value.clone()).unwrap_or_default();
        if let Some(text) = crate::sendpty::user_record_text(&rec) {
            if crate::events::sha256_hex(text.as_bytes()) == content_sha256 {
                return AcpLanded::Yes;
            }
        }
    }
    AcpLanded::No // readable, prompt absent (degraded to recoverable at R6 — see emit_acp_terminal)
}

/// C3/C5 (D2 obligation (b)) + rider-3 — emit the ACP delivery TERMINAL keyed to
/// the send by its turn id, best-effort, into the TARGET's delivery log.
/// content_sha256 is read by the send_id-join from the correlated `send-initiated`
/// (the wait arm holds the turn id, not the sent bytes; the join is the
/// protocol-native correlation ACP's turn id gives us). A SUCCESS StopReason
/// (Completed/MaxTokens) → `message-seen` (the completed turn consumed the prompt).
/// A FAILURE StopReason is POST-delivery, so it is LANDING-CHECKED against the ACP
/// session's `~/.claude/projects` record (rider-3): landed → `message-seen`. R6
/// DEGRADE (write-ordering unprovable on devbox — bridge absent): NOT-landed and
/// ambiguous BOTH → NO terminal (the send stays RECOVERABLE, disclosed as a C6
/// pending-forever residual) — never a foreclosing hard fail on an unprovable
/// not-landed. So this path emits ONLY `message-seen` (never `seen-failed`).
/// Idempotent: first-terminal-wins skips a send that already has a terminal. A
/// landed turn with NO correlated send-initiated emits nothing — there is no send
/// to terminate.
///
/// **D8**: this fires on an observed TURN TERMINAL, never on budget exhaustion.
/// The deadline arm above returns before reaching it.
pub fn emit_acp_terminal(
    env: &dyn Env,
    target: &Session,
    send_id: &str,
    reason: crate::provider::acp::TerminalReason,
) {
    // QD_HOME-consistency (delivery-lie fix): resolve the delivery-log `state_dir`
    // (and the landing-check `projects_dir`) with the QD_HOME-HONORING `from_home_env`
    // — EXACTLY as the ACP send-phase emitters do (delivery/acp.rs) and the pi terminal
    // does. The arm's own `QdPaths` comes from `from_home`, which IGNORES QD_HOME
    // (paths.rs:48-51), so under any QD_HOME override ≠ <HOME>/.quorum/dispatch the
    // terminal would land in a DIFFERENT delivery log than its send-initiated/
    // turn-accepted phases → an orphaned / pending-forever send (the terminal never
    // joins by send_id). Best-effort (no HOME → return), as the send emitters are.
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return;
    };
    let paths = QdPaths::from_home_env(std::path::Path::new(&home), env);
    let log = std::fs::read_to_string(crate::events::events_path(
        &paths.state_dir,
        &target.session_id,
    ))
    .unwrap_or_default();
    let records = crate::events::parse_events(&log).records;
    if crate::events::first_terminal_for(&records, send_id).is_some() {
        return; // first-terminal-wins → idempotent
    }
    // content_sha256 from the correlated send-initiated — needed for message-seen
    // AND the rider-3 landing check. No correlated send → nothing to terminate.
    let Some(content_sha256) = records
        .iter()
        .find(|r| r.event == "send-initiated" && r.send_id().as_deref() == Some(send_id))
        .and_then(|r| r.str_field("content_sha256"))
    else {
        return;
    };
    let seen = || crate::events::Payload::MessageSeen {
        send_id: send_id.to_string(),
        content_sha256: content_sha256.clone(),
    };
    let payload = match acp_delivery_disposition(reason) {
        // A completed / limit turn CONSUMED the prompt → landed (the strong reading).
        AcpDeliveryDisposition::Seen => seen(),
        // rider-3: a FAILURE StopReason is POST-delivery (the inject succeeded, the
        // prompt reached the resident). A Cancelled turn ADDS the user prompt to the
        // session (landed) and interrupts only the RESPONSE; Failed/Crashed/
        // TransportLost can likewise fire AFTER the prompt was consumed. So NEVER
        // hard-fail without checking the ACP record (content-keyed) — a landed send
        // must never read a permanent hard FAILED (D1's F3 lie-shape).
        AcpDeliveryDisposition::Failed(_reason_tok) => {
            match acp_prompt_landed(&paths.projects_dir, &target.session_id, &content_sha256) {
                // Landed → message-seen (it entered context; the turn just did not
                // complete cleanly — the reason is preserved in the settled
                // terminal_reason classification, option B).
                AcpLanded::Yes => seen(),
                // rider-3 R6 DEGRADE (companion ruling 01KX8MP43N, condition 1): the
                // `No → seen-failed` arm rests on "the bridge writes the user record
                // BEFORE the terminal response" — UNPROVABLE on this box (the
                // claude-code-acp bridge is absent, so the required cancel-mid-turn
                // conformance cell cannot run cred-free). A readable-but-absent record
                // could be an UNFLUSHED landing (the empty-window shape — the ws
                // Terminal can arrive before the transcript fsync). So NEITHER absent
                // nor unresolvable is hard-failed: both leave the send RECOVERABLE (no
                // foreclosing terminal), disclosed as a C6 pending-forever residual
                // (condition 2 = B). Re-enable `No → seen-failed{_reason_tok}` when the
                // write-ordering is pinned by the live cell (bridge installed).
                AcpLanded::No | AcpLanded::Unknown => return,
            }
        }
    };
    let writer = crate::events::EventWriter::for_key(
        &paths.state_dir,
        &target.session_id,
        Some(target.session_id.clone()),
        target.name.clone(),
    );
    crate::events::warn_emit(&writer, &RealClock, &payload);
}

/// Record `session`'s identity in the qd-owned tombstone store before a refusal,
/// and print the one observability line it produces. Never changes the outcome.
///
/// Silent for any provider but `acp/claude-code` (the named divergence's scope) —
/// the core self-gates, so codex/opencode refusals stay byte-identical. This
/// three-line adapter is all that is left of `bin/qd/verbs/acp_loss.rs`, whose own
/// doc named this move as the thing that would retire it.
fn preserve_identity(env: &dyn Env, session: &Session, reason: &str) {
    if let Some(note) =
        crate::delivery::acp_loss::preserve_identity(env, session, reason, WAIT_VERB)
    {
        eprintln!("{note}");
    }
}

/// scoped-ACP-CC residence WAIT loop (S7). The ACP analog of [`await_idle_codex`]:
/// reconnect to the resident adapter (endpoint + S6 identity + S7 ladder, exactly as
/// the SEND path), then PULL `next_update` until the turn's `Terminal`/`TerminalError`
/// event arrives or the deadline elapses. The events are the REAL bridge stream relayed
/// through the resident (faithfulness keystone) — wait never synthesizes completion. No
/// idle short-circuit on the (stale-for-acp) disk status; a dead/degraded endpoint
/// reports cold, never hangs.
pub fn await_idle_acp(
    env: &dyn Env,
    paths: &QdPaths,
    session: &Session,
    label: &str,
    budget_ms: u64,
) -> Result<TurnState, LaneError> {
    use crate::provider::acp::{
        classify_connect_failure, derive_tier, AcpClient, AcpConnection, AcpTurnCompletion,
        AcpTurnObservation, ConnectFailure, TerminalReason, Tier,
    };

    let id = SessionId(session.session_id.clone());
    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        // Child D (opencode D1): for acp/claude-code a pid-less row is
        // already-lost transport (the claude CLI's janitor may have reaped the
        // registry row entirely) — preserve identity and refuse to the SAME
        // human-recovery surface as the dead-tier branch below. Any other
        // acp/* provider keeps the pre-Child-D wording, byte-identical.
        // The CLAUDE bridge specifically — asked of the lane, because the row now
        // says `claude-code` and a provider-string equality would never fire.
        if crate::lane::lane_for(&session.provider, session.hosting.as_deref())
            == crate::lane::Lane::new(crate::lane::Harness::ClaudeCode, crate::lane::Mode::Acp)
        {
            preserve_identity(env, session, "acp session has no live daemon pid at wait entry");
            return Err(unreachable("acp session has no live daemon pid at wait entry"));
        }
        return Err(no_pid(&id));
    };
    let entry = crate::registry::read_entry(&paths.sessions_dir, pid);
    let endpoint = entry
        .as_ref()
        .and_then(|e| e.endpoint.clone())
        .filter(|s| !s.is_empty());
    let transport_field = entry.as_ref().and_then(|e| e.transport.clone());

    // S6 identity + S7 ladder (same gate as the SEND path — connect-success is liveness,
    // cmdline+pid is identity; drive only on Tier::Acp).
    let cmdline = crate::create_daemon::real_cmdline_probe(pid);
    let endpoint_alive = endpoint.is_some()
        && crate::effects::is_pid_alive(pid as i32)
        && crate::provider::acp::residence::cmdline_is_our_acp_daemon(
            cmdline.as_deref(),
            endpoint.as_deref(),
        );
    if derive_tier(crate::lane::Mode::Acp, transport_field.as_deref(), endpoint_alive) != Tier::Acp {
        // Child D (opencode D1 — clerk-4's Arm-B ratification, bond note
        // 01KX01BY7G): `acp/claude-code` is a NAMED DIVERGENCE — a dead (or
        // historically pty-latched) row REFUSES cold with identity preserved in
        // the qd-owned tombstone store; there is NO floor drive here (Child B's
        // latched-drive lane was removed, not gated — the latch no longer
        // selects any behavior beyond this refusal). `qd wait` thus never
        // spawns anything as a side effect of being asked a question (R5-3's
        // posture, now unconditional), and `qd resume` stays the one recovery
        // act. `acp/opencode` (the other provider routed through this fn) keeps
        // this exact refusal, byte-identical — `preserve_identity` self-gates
        // on the provider, so it writes nothing.
        preserve_identity(env, session, "acp endpoint not reachable at wait entry");
        return Err(unreachable("acp endpoint not reachable at wait entry"));
    }
    let endpoint = endpoint.expect("Tier::Acp implies a live endpoint");

    let conn = match AcpConnection::connect(&endpoint, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(_) => {
            // Round-1 TOCTOU fix (same split as send_acp's connect arm):
            // the pre-connect probe cannot confirm liveness across the connect
            // boundary. Re-probe NOW: a genuine wedge (alive + identity-verified)
            // refuses with no tombstone — its live-pid row is not janitor-
            // reapable; a daemon that died in the window is a transport LOSS —
            // preserve identity, then the same cold refusal. The wedge-dies-later
            // residual is stated + defended in ladder.rs clause 3.
            let pid_alive_now = crate::effects::is_pid_alive(pid as i32);
            let cmdline_is_ours_now = crate::provider::acp::residence::cmdline_is_our_acp_daemon(
                crate::create_daemon::real_cmdline_probe(pid).as_deref(),
                Some(endpoint.as_str()),
            );
            if classify_connect_failure(pid_alive_now, cmdline_is_ours_now)
                == ConnectFailure::TransportLost
            {
                preserve_identity(
                    env,
                    session,
                    "acp daemon died across the connect boundary (was live at the pre-connect probe)",
                );
            }
            return Err(unreachable("acp connect failed at wait entry"));
        }
    };

    // (N-idle, Item 3) ENTRY-IDLE short-circuit: a genuinely-idle session (no turn in
    // flight — the SC-1 queue's primary truth) returns PROMPTLY instead of camping to the
    // timeout (the codex entry-idle gate analog). A turn IN FLIGHT reports in_flight=true
    // → we fall through to the wait loop, so a mid-turn wait NEVER false-idles. A failed
    // status probe falls through too (the loop handles a dead channel).
    if let Ok(false) = conn.status_in_flight() {
        return Ok(TurnState::IdleAtEntry);
    }

    open_progress(label);

    let deadline = Instant::now() + total_budget(budget_ms);
    loop {
        let now = Instant::now();
        if now >= deadline {
            // D8: the budget belongs to the caller. No emitter runs on this path.
            return Ok(TurnState::BudgetElapsed);
        }
        let remaining = (deadline - now).min(Duration::from_secs(1));
        // (N-O3) Factor the terminal detection through the SC-2 completion contract: map
        // the SAME `next_update` pull onto an observation, then let `AcpTurnCompletion`
        // render the verdict (behaviorally identical to the prior inline match — Terminal
        // → done/0, TerminalError → failed/1, Update/None → keep polling, Err → closed).
        let raw = conn.next_update(remaining);
        let turn_id = acp_terminal_turn_id(&raw);
        let obs = acp_wait_observation(raw);
        let observer = OneShotObserver(obs.clone());
        let completion = AcpTurnCompletion::new(&observer);
        match completion.poll_completion() {
            // Visible = the turn is DONE. The SC-2a reason decides done(0) vs failed(1):
            // a JSON-RPC-error terminal is `Failed` (an operator scripting `qd wait X &&
            // next` must not proceed on a failed turn); every other terminal is a clean
            // completion (end_turn / cancelled / max — all exit 0, as before).
            TurnCompletionProbe::Visible => {
                // C3/C5 (D2 obligation (b)): emit the delivery terminal, wired to
                // the SETTLED terminal_reason classification (never re-derived),
                // keyed to the send by its turn id. Best-effort — never changes
                // the wait's own answer below.
                if let (Some(tid), Some((reason, _))) =
                    (turn_id.as_deref(), completion.terminal_reason())
                {
                    // emit_acp_terminal resolves its delivery-log state_dir via the
                    // QD_HOME-honoring from_home_env itself, so it shares the log
                    // with the send phases under any QD_HOME.
                    emit_acp_terminal(env, session, tid, reason);
                }
                if matches!(completion.terminal_reason(), Some((TerminalReason::Failed, _))) {
                    let msg = match &obs {
                        AcpTurnObservation::Failed(m) => m.clone(),
                        _ => String::new(),
                    };
                    return Ok(TurnState::TurnFailed { detail: msg });
                }
                return Ok(TurnState::WentIdle);
            }
            // Still in flight (streaming / quiet poll) → keep pulling until the terminal.
            TurnCompletionProbe::Pending => continue,
            // Transport gone before any terminal → source integrity lost →
            // never a false done. Child D: the daemon died UNDER a camped wait —
            // the same identity-preservation duty as an entry-lane loss (the
            // janitor will reap the row ~1s from now), so record the tombstone
            // before surfacing.
            TurnCompletionProbe::Degraded(_) => {
                preserve_identity(
                    env,
                    session,
                    "acp transport lost mid-wait (channel closed before the turn's terminal)",
                );
                return Ok(TurnState::ChannelClosed);
            }
        }
    }
}

// ===========================================================================
// pi — the drop-immune `is_streaming` point-read
// ===========================================================================

/// The pi wait gate decision from ONE `is_streaming` point-read — the drop-immune
/// core of [`await_idle_pi`], shared by the ENTRY gate and the poll loop. This is the
/// single seam that separates the THREE gate cases (B1 clause 2):
///   * `false` → RELEASE. Covers BOTH "finished" and "never-started": a session
///     with no turn in flight reads `is_streaming:false`, so neither hangs — a
///     just-booted pi that never got a prompt releases immediately.
///   * `true`  → BLOCK. A turn is genuinely in flight; keep point-reading.
/// A live endpoint that ERRORS is neither case — the caller maps it to `cold`,
/// never a false release and never a hang.
#[derive(Debug, PartialEq, Eq)]
enum PiWaitVerdict {
    Release,
    Block,
}

fn pi_wait_verdict(is_streaming: bool) -> PiWaitVerdict {
    if is_streaming {
        PiWaitVerdict::Block
    } else {
        PiWaitVerdict::Release
    }
}

/// pi (B1): block until a daemon-hosted pi session goes busy→idle. The acp/codex
/// daemon-reconnect shape (resolve the row's recorded endpoint, gate on identity +
/// liveness, connect), but completion is observed by re-reading
/// `PiRemote::observe().is_streaming` — a POINT-READ — on pi's ~150ms floor, NOT by
/// watching the event stream for an `agent_end`. That is the inviolable B1
/// constraint: a dropped stream event can starve a stream-watch but can NEVER
/// starve a point-read of the live state. A dead/degraded endpoint reports cold
/// (no hang); a never-started session reads idle and releases (no hang).
///
/// At every RELEASE — entry and loop alike — the C5/C3 obligation-(c) observer runs:
/// content-key the rollout and emit `message-seen` for any pending pi send that
/// landed. That is [`crate::delivery::pi::observe_landed_sends`], the SAME function
/// the SEND seam's post-inject landing check drives, so the two seams cannot
/// disagree and cannot double-emit (first-terminal-wins). It is NOT reached from the
/// deadline arm — D8.
pub fn await_idle_pi(
    env: &dyn Env,
    paths: &QdPaths,
    session: &Session,
    label: &str,
    budget_ms: u64,
) -> Result<TurnState, LaneError> {
    use crate::provider::pi::{residence::cmdline_is_our_pi_daemon, PiRemote};

    let clock = RealClock;
    let id = SessionId(session.session_id.clone());
    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        return Err(no_pid(&id));
    };
    let entry = crate::registry::read_entry(&paths.sessions_dir, pid);
    let endpoint = entry
        .as_ref()
        .and_then(|e| e.endpoint.clone())
        .filter(|s| !s.is_empty());

    // Identity + liveness gate (the acp SEND/WAIT posture): a live endpoint, the pid
    // alive, and the `/proc` cmdline is OUR pi-daemon for THIS endpoint (defeats pid
    // reuse — a connect-success is liveness, cmdline+endpoint is identity). Any miss
    // → cold, never drive a stranger's socket.
    let cmdline = crate::create_daemon::real_cmdline_probe(pid);
    let alive = endpoint.is_some()
        && crate::effects::is_pid_alive(pid as i32)
        && cmdline_is_our_pi_daemon(cmdline.as_deref(), endpoint.as_deref());
    if !alive {
        return Err(unreachable("pi endpoint not reachable at wait entry"));
    }
    let endpoint = endpoint.expect("alive implies a live endpoint");

    let remote = match PiRemote::connect(&endpoint, Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => return Err(unreachable("pi connect failed at wait entry")),
    };

    // ENTRY gate — the drop-immune point-read separates the three cases.
    match remote.observe() {
        Ok(obs) => {
            if pi_wait_verdict(obs.is_streaming) == PiWaitVerdict::Release {
                // C5/C3 (obligation (c)): at turn close, content-key the rollout
                // and emit message-seen for any pending pi send that landed.
                crate::delivery::pi::observe_landed_sends(env, &clock, paths, session);
                return Ok(TurnState::IdleAtEntry);
            }
        }
        Err(_) => return Err(unreachable("pi status probe failed at wait entry")),
    }

    open_progress(label);

    let deadline = Instant::now() + total_budget(budget_ms);
    loop {
        if Instant::now() >= deadline {
            // D8: no observer runs here. A budget is the caller's fact.
            return Ok(TurnState::BudgetElapsed);
        }
        // pi-native cadence: re-read is_streaming on the ~150ms floor (NOT a busy
        // spin, NOT a claude-shaped promptly bar).
        std::thread::sleep(PI_POLL_INTERVAL);
        match remote.observe() {
            Ok(obs) => match pi_wait_verdict(obs.is_streaming) {
                PiWaitVerdict::Release => {
                    // C5/C3 (obligation (c)): the turn closed — content-key the
                    // rollout and emit message-seen for any pending pi send that
                    // landed (incl. a steer's text in the just-closed turn).
                    crate::delivery::pi::observe_landed_sends(env, &clock, paths, session);
                    return Ok(TurnState::WentIdle);
                }
                PiWaitVerdict::Block => continue,
            },
            // The resident dropped / went unreachable mid-wait → source integrity
            // lost → never a false done, never a hang.
            Err(_) => return Ok(TurnState::ChannelClosed),
        }
    }
}

// ===========================================================================
// pi/extension — the control channel's own idle RPC
// ===========================================================================

/// `pi/extension` (P2): block until the pi TUI in the pane reports itself
/// settled, over the session's unix-socket control channel.
///
/// # Why this exists rather than reusing [`await_idle_pi`]
///
/// It is a FIFTH body because the fourth one cannot answer for this lane, and
/// the way it fails is silent-shaped. [`await_idle_pi`]'s identity gate demands
/// a pid whose `/proc` cmdline is **our `pi-daemon`** — the right gate for the
/// lane it was written for, because driving a stranger's ws socket is the thing
/// it is defending against. A `pi/extension` session is a real pi TUI: its
/// cmdline is `pi`, never `<exe> pi-daemon`, so the gate can never pass, and
/// every `qd wait` on the lane failed `"pi endpoint not reachable at wait
/// entry"` — a transport refusal about a session that was up, answering, and
/// holding an idle RPC nobody called. `LaneOps::await_idle` routed on the
/// HARNESS, so there was no arm to write; keying it on the LANE is what made
/// this function's absence a compile error instead of a runtime lie.
///
/// # The shape is [`await_idle_pi`]'s, and deliberately so
///
/// Entry point-read → release or open a progress line → block → map. Two things
/// differ, and both are the channel being better than a poll:
///
///   - **The block is a SUBSCRIPTION, not a poll.** `Client::await_idle` waits
///     on the pushed `idle` frame, so the answer lands at the instant pi settles
///     rather than up to one poll interval later, and there is no cadence floor
///     to honour. The drop-immunity argument that makes `await_idle_pi` a
///     point-read is preserved differently here: a dropped connection is not a
///     starved watch, it is [`TurnState::ChannelClosed`] (see the `Vanished`
///     arm), so a lost stream can still never be read as "went idle".
///   - **`agent_settled`, not `agent_end`.** That choice lives in the extension
///     server and is documented on
///     [`crate::provider::pi::extension::Client::await_idle`]; pi may auto-retry
///     or auto-compact after `agent_end`, so releasing there would hand control
///     back mid-turn.
///
/// # The entry read, and why the entry answer can be `WentIdle`
///
/// The entry gate is one `health` frame — the SAME `busy` bit
/// `lane_read::extension_live_status` reads for `qd ls`, so what a user is told
/// about this session by `ls` and by `wait` cannot disagree. Idle at entry ⇒
/// [`TurnState::IdleAtEntry`], with no progress line opened, exactly as the
/// other four bodies do.
///
/// If it says BUSY the progress line opens, and `Client::await_idle` then opens
/// its OWN connection and subscribes — and its subscribe response may already
/// say `idle`, because the turn can settle in the gap between the two reads.
/// That is reported as [`TurnState::WentIdle`], not `IdleAtEntry`, and it is the
/// honest answer rather than a convenient one: this watcher observed busy and
/// then observed idle, which IS the busy→idle transition `WentIdle` names. It
/// also closes the progress line the way the caller expects — `qd wait`'s
/// `IdleAtEntry` arm writes a whole line on the assumption none is open.
///
/// # What it does NOT do: no `message-seen` observer
///
/// [`await_idle_pi`] runs [`crate::delivery::pi::observe_landed_sends`] at every
/// release, under C5/C3 obligation (c). This does not, and that is a finding
/// rather than an omission: that observer matches sends whose recorded
/// `send_path` is `"pi"`, and this lane's `deliver` records `"pi/extension"`
/// (`lanes.rs`, the `(Pi, Extension)` carrier), so calling it here would be a
/// guaranteed no-op dressed as an obligation. `pi/extension` sends currently get
/// `send-initiated` + `turn-accepted` and no terminal at all; giving them one is
/// a delivery-seam change with its own ledger consequences, not something an
/// idle watcher may invent on the way past. D8 is unaffected either way — the
/// deadline arm below returns before anything could be emitted.
pub fn await_idle_pi_extension(
    env: &dyn Env,
    paths: &QdPaths,
    session: &Session,
    label: &str,
    budget_ms: u64,
) -> Result<TurnState, LaneError> {
    use crate::provider::pi::extension::{socket_for, AwaitOutcome, Client};

    let id = SessionId(session.session_id.clone());
    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        return Err(no_pid(&id));
    };
    // A dead pid cannot be serving a socket, and asking the process table first
    // turns the common cold case into a `kill(0)` instead of a `connect(2)` that
    // has to fail — the same order `lane_read::extension_live_status` takes.
    // This is the `Cold` refusal, not the `Transport` one: there is no live
    // process to wait on, which is what `no_pid` says and what `qd wait` renders
    // as "cold/dead. Nothing to wait for."
    if !crate::effects::is_pid_alive(pid as i32) {
        return Err(no_pid(&id));
    }

    // The socket the ROW recorded, falling back to the derived path — the same
    // resolution `deliver` and `health` use, and in that order for the same
    // reason: recomputing from `$TMPDIR` would address a different socket than
    // the live pi bound whenever the environment moved between create and now.
    //
    // NO PANE GATE, and the asymmetry with `deliver` is deliberate. There the
    // pane IS the carrier's fallback and a stale socket file reads as "the
    // channel broke" when the truth is "the session is gone", so the pane check
    // earns its place. Here the channel is the only thing on the path: gating on
    // pane coordinates would refuse a live, answering session whose coordinates
    // the mux join could not supply (an unnamed row, or a backend that would not
    // resolve), and the pid-liveness check above already does the "is it gone"
    // job the pane check does over there.
    let sock = socket_for(env, session_endpoint(paths, pid).as_deref(), &session.session_id);

    // ENTRY gate. `connect` failing is `NotListening` — the session is up but
    // its channel is not, which is a transport refusal and never an idle answer.
    // Failing LOUD here is the same posture `deliver` takes on this lane: the
    // alternative would be to degrade to the transcript, and pi's transcript
    // reports Busy NEVER (see `PI_PANE_HAS_NO_IDLE_SOURCE`), so the degraded
    // answer would be a false "idle" — the one thing this module may not say.
    let mut probe = Client::connect(&sock).map_err(|e| unreachable(&e.to_string()))?;
    let health = probe.health().map_err(|e| unreachable(&e.to_string()))?;
    drop(probe);
    if !health.busy {
        return Ok(TurnState::IdleAtEntry);
    }

    open_progress(label);

    match Client::await_idle(&sock, total_budget(budget_ms)) {
        // It settled between the entry read and the subscribe. Observed busy,
        // then observed idle — a transition this watcher saw, and the progress
        // line it opened is closed by the caller's ` done`.
        Ok(AwaitOutcome::AlreadyIdle | AwaitOutcome::WentIdle) => Ok(TurnState::WentIdle),
        // D8: a budget is the caller's fact and nothing was written on the way
        // to answering it.
        Ok(AwaitOutcome::TimedOut) => Ok(TurnState::BudgetElapsed),
        // The peer went away mid-wait. The session may well still be running —
        // what died is this watcher's view of it — so it is `ChannelClosed`, the
        // variant that exists to keep exactly that from reading as `WentIdle`.
        Ok(AwaitOutcome::Vanished) => Ok(TurnState::ChannelClosed),
        Err(e) => Err(unreachable(&e.to_string())),
    }
}

/// The row's recorded `endpoint`, by pid — the registry read `await_idle_pi`
/// does inline, factored out because the extension body needs the identical one.
/// Empty is `None`: a row that recorded an empty endpoint recorded nothing.
fn session_endpoint(paths: &QdPaths, pid: i64) -> Option<String> {
    crate::registry::read_entry(&paths.sessions_dir, pid)
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty())
}

// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use crate::model::SessionBranch;
    use crate::wait::{ChannelStatus, WaitContentDeps};
    use std::cell::Cell;

    // Child D structural guard (opencode D1 — clerk-4's Arm-B ratification, bond
    // note 01KX01BY7G): the ACP idle watcher must have NO reachable
    // auto-deliver/drive path on transport loss — Child B's latched-drive lane was
    // REMOVED, not gated (this subsumes R5-3's never-fresh-degrade: wait now never
    // degrades, latches, or spawns ANYTHING). The replacement disposition is pinned
    // as the positive control: identity preservation at the pid-less entry, the
    // dead-tier entry, the connect-boundary death (round-1 TOCTOU fix: the
    // re-probe's TransportLost classification), and the camped-loss (channel closed)
    // exit — each surfacing through an existing refusal. Structural (source-text),
    // scoped to `await_idle_acp`'s own body so this test's own assertion strings
    // can't make it vacuously true.
    //
    // RETARGETED, not rewritten: the body it scans moved from
    // `bin/qd/verbs/wait.rs::run_acp_wait` into this file, and with it the two
    // surfaces the assertions name — `acp_loss::preserve_identity(` became the
    // module-local `preserve_identity(` adapter over the qw core, and
    // `return cold(label);` became the typed `unreachable(..)` refusal the verb
    // renders. The SUBJECT is unchanged: four losses recorded, one refusal surface,
    // nothing driven.
    //
    // MUTATION EVIDENCE: reintroducing a floor drive or latch write reds the bans;
    // deleting a preserve_identity call reds the count.
    #[test]
    fn acp_wait_transport_loss_refuses_with_identity_preserved_and_never_drives() {
        let src = include_str!("idle.rs");
        let fn_start = src
            .find("pub fn await_idle_acp(")
            .expect("await_idle_acp must still exist verbatim");
        let after_start = &src[fn_start..];
        let fn_end = after_start
            .find("\nenum PiWaitVerdict")
            .expect("PiWaitVerdict must still follow await_idle_acp");
        let body = &after_start[..fn_end];

        for banned in [
            "drive_acp_floor_wait",
            "ensure_floor_pane",
            "degrade_and_persist",
            "degrade_to_pty",
            "structured_send_issued",
            "drop_log_line",
        ] {
            assert!(
                !body.contains(banned),
                "await_idle_acp must never reach `{banned}` — an observation verb neither \
                 latches, degrades, drives a floor, nor branches on send history; \
                 acp/claude-code transport loss REFUSES with identity preserved. \
                 Body:\n{body}"
            );
        }
        let occurrences = body.matches("preserve_identity(").count();
        assert_eq!(
            occurrences, 4,
            "await_idle_acp must preserve identity at exactly its four transport-loss \
             surfaces (pid-less entry, dead-tier entry, connect-boundary death \
             [round-1 TOCTOU fix], camped channel-closed) — found {occurrences}. \
             Body:\n{body}"
        );
        // And the refusal surface itself still lands (a body that preserved
        // identity but silently stopped refusing would pass the counts above). The
        // surface is now the typed refusal the verb renders as the cold line.
        assert!(
            body.contains("return Err(unreachable("),
            "the cold human-recovery refusal must still be the loss surface"
        );
        // Round-1 TOCTOU fix pin: the connect-Err arm must classify from a
        // FRESH re-probe (never trust the stale pre-connect reading).
        assert!(
            body.contains("classify_connect_failure(pid_alive_now, cmdline_is_ours_now)"),
            "await_idle_acp's connect-Err arm must re-probe and classify wedge-vs-loss"
        );
        // D8: no emitter may hang off the deadline arm. The budget branch returns
        // BudgetElapsed and touches nothing else.
        assert!(
            body.contains("return Ok(TurnState::BudgetElapsed);"),
            "the deadline arm must answer BudgetElapsed and write nothing (D8)"
        );
    }

    /// (N-O3) the wait terminal-detection factored through the SC-2 completion contract:
    /// each `next_update` outcome maps to the right observation AND the SAME completion
    /// verdict the raw inline match produced — behaviorally identical, no new source.
    /// REVERT SEAM: corrupt `acp_wait_observation` (e.g. Terminal→Pending) → the verdict
    /// diverges (a completed turn never reads Visible) → this REDs.
    #[test]
    fn acp_wait_observation_factors_through_completion_contract() {
        use crate::provider::acp::{
            AcpEvent, AcpTurnCompletion, AcpTurnObservation, StopReason, TerminalReason,
        };

        let ev = |e: AcpEvent| Ok(Some(e));
        let verdict = |obs: AcpTurnObservation| {
            let o = OneShotObserver(obs);
            AcpTurnCompletion::new(&o).poll_completion()
        };

        // clean end_turn terminal → Terminal(EndTurn) → Visible, reason Completed (done/0).
        let t = acp_wait_observation(ev(AcpEvent::Terminal {
            session: "s".into(),
            turn: "t".into(),
            stop: StopReason::EndTurn,
        }));
        assert_eq!(t, AcpTurnObservation::Terminal(StopReason::EndTurn));
        assert_eq!(verdict(t.clone()), TurnCompletionProbe::Visible);
        let o = OneShotObserver(t);
        assert_eq!(
            AcpTurnCompletion::new(&o).terminal_reason().unwrap().0,
            TerminalReason::Completed
        );

        // a JSON-RPC error terminal → Failed → Visible, reason Failed (failed/1).
        let f = acp_wait_observation(ev(AcpEvent::TerminalError {
            session: "s".into(),
            turn: "t".into(),
            message: "internalError".into(),
        }));
        assert_eq!(f, AcpTurnObservation::Failed("internalError".into()));
        assert_eq!(verdict(f.clone()), TurnCompletionProbe::Visible);
        let o = OneShotObserver(f);
        assert_eq!(
            AcpTurnCompletion::new(&o).terminal_reason().unwrap().0,
            TerminalReason::Failed
        );

        // a streamed update / a quiet poll → Pending (keep polling).
        let u = acp_wait_observation(ev(AcpEvent::Update {
            session: "s".into(),
            kind: "agent_message_chunk".into(),
            payload: serde_json::Value::Null,
        }));
        assert_eq!(u, AcpTurnObservation::Pending);
        assert_eq!(verdict(u), TurnCompletionProbe::Pending);
        assert_eq!(acp_wait_observation(Ok(None)), AcpTurnObservation::Pending);

        // transport gone → TransportLost → Degraded (never a false done).
        let d = acp_wait_observation(Err(crate::provider::acp::AcpError::Closed));
        assert!(matches!(d, AcpTurnObservation::TransportLost(_)));
        assert!(matches!(verdict(d), TurnCompletionProbe::Degraded(_)));
    }

    /// B1 — the pi wait gate separates the THREE cases off a single is_streaming
    /// point-read. REVERT SEAM: invert `pi_wait_verdict` (streaming→Release) and a
    /// mid-turn wait would false-idle; collapse it to always-Block and a finished/
    /// never-started session would hang — either way this REDs.
    #[test]
    fn pi_wait_verdict_separates_the_three_gate_cases() {
        // in-progress (a turn in flight) → BLOCK.
        assert_eq!(pi_wait_verdict(true), PiWaitVerdict::Block);
        // finished AND never-started both read is_streaming:false → RELEASE (neither
        // hangs — the two non-blocking cases collapse to the same point-read value).
        assert_eq!(pi_wait_verdict(false), PiWaitVerdict::Release);
    }

    /// A fake channel status seam returning a fixed observation (no socket).
    struct FakeStatus(ChannelStatusObservation);
    impl ChannelStatusSource for FakeStatus {
        fn poll_status(&self) -> ChannelStatusObservation {
            self.0
        }
    }

    /// Plant a `<pid>.json` carrying `status`; return its path.
    fn plant_pid_json(dir: &std::path::Path, status: &str) -> std::path::PathBuf {
        let p = dir.join("4242.json");
        std::fs::write(&p, format!("{{\"status\":\"{status}\"}}")).unwrap();
        p
    }

    /// GREEN (§6.0 purity): a LIVE channel `status_source` decides control status;
    /// the `pid.json` disk fallback — planted with a DISAGREEING "busy" — is NEVER
    /// read on the healthy path, so `read_status()` is the channel value "idle".
    ///
    /// FIX-SHAPED MUTATION (the verifier WILL neuter the prod glue): regress
    /// `await_idle_claude`'s `status_source` to `None` and `build_wait_deps` carries
    /// `None` → `read_status()` falls through to the disk "busy" → this REDs. The
    /// disk-as-status read B eliminates cannot creep back silently.
    #[test]
    fn build_wait_deps_live_channel_beats_disagreeing_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = plant_pid_json(tmp.path(), "busy"); // disk DISAGREES
        let clock = RealClock;
        let sleeper = crate::boot::RealSleeper;
        let deps = build_wait_deps(
            Some(Box::new(FakeStatus(ChannelStatusObservation::Live(
                ChannelStatus::Idle,
            )))),
            pid_file,
            None,
            &clock,
            &sleeper,
        );
        assert_eq!(
            deps.read_status().as_deref(),
            Some("idle"),
            "the live channel decides; the disagreeing pid.json is never read"
        );
    }

    /// CHANNEL-DOWN (`None` status_source): the §6.0-sanctioned fallback reads the
    /// `pid.json` disk status — proving the mutation above is a REAL red (the disk
    /// path is reachable and returns the DIFFERENT value "busy").
    #[test]
    fn build_wait_deps_channel_down_reads_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = plant_pid_json(tmp.path(), "busy");
        let clock = RealClock;
        let sleeper = crate::boot::RealSleeper;
        let deps = build_wait_deps(None, pid_file, None, &clock, &sleeper);
        assert_eq!(
            deps.read_status().as_deref(),
            Some("busy"),
            "channel-down mode reads the pid.json disk status"
        );
    }

    /// ENTRY-GATE purity: on a LIVE channel the daemon status decides and the disk
    /// closure is NEVER consulted; only `Down` consults disk. A closure that
    /// records its invocation proves the lazy fallback stays untouched on Live.
    #[test]
    fn entry_gate_live_channel_never_consults_disk() {
        // Live(Idle) → idle, disk closure untouched.
        let consulted = Cell::new(false);
        let idle = entry_gate_idle(ChannelStatusObservation::Live(ChannelStatus::Idle), || {
            consulted.set(true);
            true
        });
        assert!(idle);
        assert!(!consulted.get(), "Live channel must not consult disk");

        // Live(Busy) → not idle, disk closure still untouched.
        let consulted2 = Cell::new(false);
        let busy = entry_gate_idle(ChannelStatusObservation::Live(ChannelStatus::Busy), || {
            consulted2.set(true);
            true
        });
        assert!(!busy);
        assert!(!consulted2.get(), "Live(Busy) must not consult disk");

        // Down → consults the disk closure (channel-down mode).
        let consulted3 = Cell::new(false);
        let down = entry_gate_idle(ChannelStatusObservation::Down, || {
            consulted3.set(true);
            true
        });
        assert!(down);
        assert!(
            consulted3.get(),
            "channel-down must consult the disk closure"
        );
    }

    // =======================================================================
    // C3/C5 — the ACP variant-exact delivery-terminal mapping (D2 obligation
    // (b)), ASSERTED against the SETTLED classification (never re-derived), +
    // the emission/correlation/idempotence. All hermetic, no model.
    // =======================================================================

    #[test]
    fn acp_delivery_terminal_mapping_is_variant_exact() {
        use crate::provider::acp::{StopReason, TerminalReason as R};
        // (1) The SETTLED StopReason → TerminalReason classification (rpc.rs,
        //     commit 94a827a1) — ASSERTED, not re-derived. D2 wires the delivery
        //     EMISSION to THIS.
        assert_eq!(StopReason::EndTurn.to_terminal_reason(), R::Completed);
        assert_eq!(StopReason::MaxTokens.to_terminal_reason(), R::MaxTokens);
        assert_eq!(StopReason::MaxTurnRequests.to_terminal_reason(), R::MaxTokens);
        assert_eq!(StopReason::Cancelled.to_terminal_reason(), R::Cancelled);
        assert_eq!(StopReason::Refusal.to_terminal_reason(), R::Refusal);
        // (2) The D2 delivery mapping (the charge's verbatim table), keyed off
        //     the settled TerminalReason:
        assert!(
            matches!(acp_delivery_disposition(R::Completed), AcpDeliveryDisposition::Seen),
            "EndTurn → message-seen (STRONG turn-consumption reading)"
        );
        assert!(
            matches!(acp_delivery_disposition(R::MaxTokens), AcpDeliveryDisposition::Seen),
            "MaxTokens/MaxTurnRequests → message-seen (graded success, landed)"
        );
        assert!(
            matches!(
                acp_delivery_disposition(R::Cancelled),
                AcpDeliveryDisposition::Failed("cancelled")
            ),
            "Cancelled → seen-failed (an interrupt never proves entry)"
        );
        assert!(matches!(
            acp_delivery_disposition(R::Refusal),
            AcpDeliveryDisposition::Failed("refusal")
        ));
        assert!(
            matches!(acp_delivery_disposition(R::Failed), AcpDeliveryDisposition::Failed("failed")),
            "terminal-time JSON-RPC error response → seen-failed (post-wire), never the door send-failed"
        );
        assert!(
            matches!(
                acp_delivery_disposition(R::TransportLost),
                AcpDeliveryDisposition::Failed("transport-lost")
            ),
            "unparseable stop / transport lost → seen-failed, never success"
        );
        assert!(matches!(
            acp_delivery_disposition(R::Crashed),
            AcpDeliveryDisposition::Failed("crashed")
        ));
        // (3) Reason preservation / non-laundering: MaxTurnRequests is NOT
        //     laundered into a clean completion (terminal_reason stays MaxTokens).
        assert_ne!(StopReason::MaxTurnRequests.to_terminal_reason(), R::Completed);
    }

    #[test]
    fn acp_observation_wires_through_completion_to_disposition() {
        // The full chain obs → (SETTLED) terminal_reason → D2 disposition, incl.
        // the failure observations that arrive OUTSIDE a StopReason (the (b) split).
        use crate::provider::acp::{AcpTurnCompletion, AcpTurnObservation, StopReason};
        let disp = |obs: AcpTurnObservation| -> AcpDeliveryDisposition {
            let observer = OneShotObserver(obs);
            let (reason, _) = AcpTurnCompletion::new(&observer)
                .terminal_reason()
                .expect("terminal");
            acp_delivery_disposition(reason)
        };
        assert!(matches!(
            disp(AcpTurnObservation::Terminal(StopReason::EndTurn)),
            AcpDeliveryDisposition::Seen
        ));
        assert!(matches!(
            disp(AcpTurnObservation::Terminal(StopReason::Cancelled)),
            AcpDeliveryDisposition::Failed("cancelled")
        ));
        // terminal-time JSON-RPC error RESPONSE → Failed (a DIFFERENT moment than
        // the send-time door send-failed).
        assert!(matches!(
            disp(AcpTurnObservation::Failed("internalError".into())),
            AcpDeliveryDisposition::Failed("failed")
        ));
        assert!(matches!(
            disp(AcpTurnObservation::TransportLost("gone".into())),
            AcpDeliveryDisposition::Failed("transport-lost")
        ));
        // Pending is non-terminal → no disposition.
        let observer = OneShotObserver(AcpTurnObservation::Pending);
        assert!(AcpTurnCompletion::new(&observer).terminal_reason().is_none());
    }

    fn acp_target(uuid: &str) -> Session {
        Session {
            name: Some("acp-obs-1".to_string()),
            user_named: None,
            session_id: uuid.to_string(),
            code: None,
            qd_id: None,
            pid: Some(9191),
            status: SessionStatus::Idle,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "acp/claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            hosting: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    /// Emit a pending daemon send-initiated (given send_path, no terminal) into the
    /// target's log; return its content_sha256.
    fn emit_pending_si(
        state_dir: &std::path::Path,
        uuid: &str,
        name: Option<String>,
        send_id: &str,
        message: &str,
        send_path: &str,
    ) -> String {
        let sha = crate::events::sha256_hex(message.as_bytes());
        let writer =
            crate::events::EventWriter::for_key(state_dir, uuid, Some(uuid.to_string()), name);
        writer
            .emit(
                &RealClock,
                &crate::events::Payload::SendInitiated {
                    send_id: send_id.to_string(),
                    verb: "send:relay".to_string(),
                    send_path: send_path.to_string(),
                    content_sha256: sha.clone(),
                    content_len: message.as_bytes().len() as u64,
                    chunks: 1,
                    chunk_sha256s: vec![sha.clone()],
                    chunk_sha256s_capped: false,
                    transcript: None,
                    transcript_offset: None,
                    content_preview: None,
                },
            )
            .unwrap();
        sha
    }

    #[test]
    fn acp_terminal_emission_correlated_and_idempotent() {
        use crate::provider::acp::TerminalReason;
        let home = tempfile::tempdir().unwrap();
        let uuid = "ffffffff-1111-2222-3333-777777777777";
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.path().to_string_lossy().to_string());
        let env = MapEnv { vars, uid: 501 };
        let paths = QdPaths::from_home_env(home.path(), &env);
        let target = acp_target(uuid);
        let name = target.name.clone();

        // A completed turn → message-seen, content_sha256 read by send_id-join.
        let sha = emit_pending_si(&paths.state_dir, uuid, name.clone(), "turn-42", "acp body", "acp/claude-code");
        emit_acp_terminal(&env, &target, "turn-42", TerminalReason::Completed);
        let raw = std::fs::read_to_string(crate::events::events_path(&paths.state_dir, uuid)).unwrap();
        let ms: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("message-seen")).unwrap()).unwrap();
        assert_eq!(ms["send_id"], "turn-42");
        assert_eq!(
            ms["content_sha256"].as_str().unwrap(),
            sha,
            "content_sha256 read by send_id-join from the correlated send-initiated"
        );

        // Idempotent: a second terminal call is a no-op (first-terminal-wins).
        emit_acp_terminal(&env, &target, "turn-42", TerminalReason::Completed);
        let raw2 = std::fs::read_to_string(crate::events::events_path(&paths.state_dir, uuid)).unwrap();
        assert_eq!(raw2.matches("\"message-seen\"").count(), 1, "never a second terminal");

        // A DIFFERENT send, Cancelled + readable-but-absent. R6 DEGRADE (the
        // write-ordering is unprovable → NOT hard-failed): NO terminal, RECOVERABLE.
        emit_pending_si(&paths.state_dir, uuid, name, "turn-43", "interrupted body", "acp/claude-code");
        let proj = paths.projects_dir.join("proj-slug");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(format!("{uuid}.jsonl")),
            "{\"type\":\"user\",\"message\":{\"content\":\"an unrelated earlier prompt\"}}\n",
        )
        .unwrap();
        emit_acp_terminal(&env, &target, "turn-43", TerminalReason::Cancelled);
        let raw3 = std::fs::read_to_string(crate::events::events_path(&paths.state_dir, uuid)).unwrap();
        assert!(
            !raw3.contains("seen-failed"),
            "R6 degrade: a readable-but-absent Cancelled is RECOVERABLE, never a hard seen-failed"
        );
        assert!(
            crate::events::first_terminal_for(
                &crate::events::parse_events(&raw3).records,
                "turn-43"
            )
            .is_none(),
            "turn-43 stays non-terminal (recoverable) after the degraded Cancelled"
        );

        // A landed turn with NO correlated send-initiated emits nothing.
        emit_acp_terminal(&env, &target, "turn-nonexistent", TerminalReason::Completed);
        let raw4 = std::fs::read_to_string(crate::events::events_path(&paths.state_dir, uuid)).unwrap();
        assert!(
            !raw4.contains("turn-nonexistent"),
            "no send-initiated for that turn → nothing to terminate"
        );
    }

    // =======================================================================
    // rider-3 (qd-ctl3's tripwire ruling) — ACP FAILURE-terminal FORECLOSURE
    // HONESTY: a POST-delivery failure StopReason is LANDING-CHECKED, so a
    // landed-but-interrupted send is NEVER a permanent hard FAILED (D1's F3
    // lie-shape). Covers ALL failure reasons (boundary 1). Hermetic — a fixture
    // CC-shaped ~/.claude/projects transcript, no model.
    // =======================================================================

    /// Jail an ACP session + a pending send-initiated + (optionally) a CC-shaped
    /// projects transcript, drive `emit_acp_terminal(reason)`, and return the
    /// emitted terminal (event, reason) — or None if none (recoverable/PENDING).
    /// `landed_prompt`: Some(text) writes a projects transcript whose USER record
    /// carries `text`; None writes NO transcript (find_jsonl_path → Unknown).
    fn drive_acp_failure(
        reason: crate::provider::acp::TerminalReason,
        uuid: &str,
        prompt: &str,
        landed_prompt: Option<&str>,
    ) -> Option<(String, Option<String>)> {
        let home = tempfile::tempdir().unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.path().to_string_lossy().to_string());
        let env = MapEnv { vars, uid: 501 };
        let paths = QdPaths::from_home_env(home.path(), &env);
        let target = acp_target(uuid);
        emit_pending_si(
            &paths.state_dir,
            uuid,
            target.name.clone(),
            "turn-x",
            prompt,
            "acp/claude-code",
        );
        if let Some(text) = landed_prompt {
            // A CC-shaped transcript under a project slug dir (find_jsonl_path scans
            // subdirs for <uuid>.jsonl). The USER record is what user_record_text
            // matches; the assistant record is a red-herring (never a landing).
            let proj = paths.projects_dir.join("proj-slug");
            std::fs::create_dir_all(&proj).unwrap();
            let user_rec = serde_json::json!({"type":"user","message":{"content":text}});
            let asst_rec = serde_json::json!({"type":"assistant","message":{"content":prompt}});
            std::fs::write(
                proj.join(format!("{uuid}.jsonl")),
                format!("{user_rec}\n{asst_rec}\n"),
            )
            .unwrap();
        }
        emit_acp_terminal(&env, &target, "turn-x", reason);
        let raw =
            std::fs::read_to_string(crate::events::events_path(&paths.state_dir, uuid)).unwrap();
        for line in raw.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let ev = v["event"].as_str().unwrap();
            if ev == "message-seen" || ev == "seen-failed" {
                return Some((
                    ev.to_string(),
                    v.get("reason").and_then(|r| r.as_str()).map(String::from),
                ));
            }
        }
        None
    }

    #[test]
    fn acp_failure_foreclosure_honesty_all_reasons() {
        use crate::provider::acp::TerminalReason as R;
        // boundary 1: EVERY failure StopReason gets the landing check — no sibling
        // arm left foreclosing.
        let reasons = [
            (R::Cancelled, "cancelled"),
            (R::Refusal, "refusal"),
            (R::Failed, "failed"),
            (R::Crashed, "crashed"),
            (R::TransportLost, "transport-lost"),
        ];
        for (i, (reason, tok)) in reasons.iter().enumerate() {
            let prompt = format!("prompt-{i}-{tok}");

            // LANDED (the prompt IS a user record in the projects transcript) →
            // message-seen, NEVER a foreclosing seen-failed (the F3 lie-shape).
            let landed = drive_acp_failure(
                *reason,
                &format!("aaaaaaaa-0000-0000-0000-00000000000{i}"),
                &prompt,
                Some(&prompt),
            );
            assert_eq!(
                landed,
                Some(("message-seen".to_string(), None)),
                "{tok}: a LANDED failure turn → message-seen, never a foreclosing hard fail"
            );

            // READABLE-BUT-ABSENT (transcript readable, a DECOY user record — the
            // prompt is absent). R6 DEGRADE (write-ordering unprovable on devbox):
            // this is NOT hard-failed (it could be an unflushed landing) → NO
            // terminal, RECOVERABLE. NEVER a foreclosing seen-failed.
            let not_landed = drive_acp_failure(
                *reason,
                &format!("bbbbbbbb-0000-0000-0000-00000000000{i}"),
                &prompt,
                Some("a completely different prompt"),
            );
            assert_eq!(
                not_landed, None,
                "{tok}: R6 degrade — readable-but-absent → RECOVERABLE, never a hard seen-failed"
            );

            // AMBIGUOUS (no projects transcript resolvable → Unknown) → NO terminal:
            // the send stays RECOVERABLE, never a foreclosing hard fail.
            let ambiguous = drive_acp_failure(
                *reason,
                &format!("cccccccc-0000-0000-0000-00000000000{i}"),
                &prompt,
                None,
            );
            assert_eq!(
                ambiguous, None,
                "{tok}: unresolvable record → RECOVERABLE (no foreclosing terminal)"
            );

            // The failure path emits ONLY message-seen (landed) or nothing — NEVER
            // a hard seen-failed at R6 (the foreclosure-honesty invariant).
            assert!(
                not_landed != Some(("seen-failed".to_string(), Some(tok.to_string()))),
                "{tok}: the degraded arm must never mint seen-failed"
            );
        }
    }

    #[test]
    fn acp_landed_failure_preserves_reason_not_laundered() {
        // boundary 3 (option-B discipline): a landed CANCELLED → message-seen (the
        // delivery event), BUT the reason is PRESERVED in the SETTLED terminal_reason
        // classification (Cancelled != Completed) — never laundered into a clean
        // completion, exactly as MaxTokens→message-seen preserves its limit reason.
        // No new terminal kind / field (boundary 4).
        use crate::provider::acp::{AcpTurnCompletion, AcpTurnObservation, StopReason, TerminalReason as R};
        let landed = drive_acp_failure(
            R::Cancelled,
            "dddddddd-0000-0000-0000-000000000001",
            "the cancelled prompt",
            Some("the cancelled prompt"),
        );
        assert_eq!(
            landed,
            Some(("message-seen".to_string(), None)),
            "landed-cancelled → message-seen (entered context; the turn just didn't complete cleanly)"
        );
        // The reason is preserved in the classification, not laundered.
        let observer = OneShotObserver(AcpTurnObservation::Terminal(StopReason::Cancelled));
        let (tr, _) = AcpTurnCompletion::new(&observer).terminal_reason().unwrap();
        assert_eq!(tr, R::Cancelled, "terminal_reason preserved as Cancelled");
        assert_ne!(tr, R::Completed, "a landed-cancelled is NEVER laundered into a clean Completed");
        assert!(
            matches!(acp_delivery_disposition(R::Cancelled), AcpDeliveryDisposition::Failed("cancelled")),
            "the disposition carries the cancelled reason for the not-landed branch"
        );
    }

    /// QD_HOME store-resolution regression (the delivery-lie / orphaned-terminal
    /// class): the ACP TERMINAL must land in the SAME delivery log as its SEND
    /// phases under a QD_HOME override ≠ <HOME>/.quorum/dispatch — join-by-send_id
    /// survives QD_HOME. Before the fix, the terminal resolved state_dir via
    /// `from_home` (QD_HOME-ignorant) while the send phases used `from_home_env`, so
    /// under QD_HOME the terminal orphaned into a different log. FAILS on the
    /// divergence, passes after the fix.
    #[test]
    fn acp_terminal_shares_delivery_log_with_send_phases_under_qd_home() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let qd_home = root.path().join("qd"); // ≠ <HOME>/.quorum/dispatch
        std::fs::create_dir_all(&home).unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.to_string_lossy().to_string());
        vars.insert("QD_HOME".to_string(), qd_home.to_string_lossy().to_string());
        let env = MapEnv { vars, uid: 501 };

        let uuid = "99999999-0000-0000-0000-000000000077";
        let msg = "acp turn under a QD_HOME override";
        // The QD_HOME-honoring delivery log (where the send phases + terminal MUST land).
        let qd_state = QdPaths::from_home_env(&home, &env).state_dir;
        // The from_home (QD_HOME-IGNORANT) log a regressed terminal would orphan into.
        let bad_state = QdPaths::from_home(&home).state_dir;
        assert_ne!(qd_state, bad_state, "the QD_HOME override must diverge from from_home");

        // Seed the SEND phase (send-initiated) into the QD_HOME log, as the send
        // emitters (from_home_env) do.
        let sha = emit_pending_si(
            &qd_state,
            uuid,
            Some("acp-qdh".into()),
            "turn-77",
            msg,
            "acp/claude-code",
        );

        // The TERMINAL: emit_acp_terminal now resolves its own state_dir via
        // from_home_env(env) → the QD_HOME log. A completed turn → message-seen.
        let target = acp_target(uuid);
        emit_acp_terminal(
            &env,
            &target,
            "turn-77",
            crate::provider::acp::TerminalReason::Completed,
        );

        // The terminal joins its send phase in the QD_HOME log by send_id.
        let qd_log = std::fs::read_to_string(crate::events::events_path(&qd_state, uuid)).unwrap();
        let ms: serde_json::Value =
            serde_json::from_str(qd_log.lines().find(|l| l.contains("message-seen")).unwrap())
                .unwrap();
        assert_eq!(ms["send_id"], "turn-77");
        assert_eq!(ms["content_sha256"].as_str().unwrap(), sha);
        assert!(
            qd_log.contains("send-initiated") && qd_log.contains("message-seen"),
            "the ACP send phase + terminal share the ONE QD_HOME delivery log (join survives QD_HOME)"
        );

        // Regression guard: the terminal must NOT orphan into the from_home log.
        let bad_log = crate::events::events_path(&bad_state, uuid);
        assert!(
            !bad_log.exists()
                || !std::fs::read_to_string(&bad_log).unwrap().contains("message-seen"),
            "the terminal must NOT land in the QD_HOME-ignorant from_home log (the delivery-lie)"
        );
    }
}
