//! REAL `qd wait` backend (spec §3.2; wart-wave W6+W7 content keying, ADD-15) —
//! claude-code path only.
//!
//! Resolve → entry idle check (resolved status) → no-pid error → STATUS +
//! TRANSCRIPT-CONTENT keyed poll loop (`dispatch::wait::run_wait_content_loop`; see
//! wait.rs module doc for the sanction + degradation contract). The OpenCode
//! SSE branch is a named parked exclusion (provider never opencode in the
//! engine).

use clap::ArgMatches;

use dispatch::boot::RealSleeper;
use dispatch::effects::{RealClock, RealEnv};
use dispatch::fmt::truncate_id_default;
use dispatch::model::SessionStatus;
use dispatch::wait::{
    entry_is_idle, run_wait_content_loop, ChannelStatusObservation, ChannelStatusSource,
    ChannelTurnCompletion, PidFileStatus, RealTurnEndProbe, RealWaitContentDeps, TurnCompletion,
    TurnCompletionProbe, WaitStatusOutcome,
};

use super::acp_loss;
use super::common;

/// RESIDUAL #2 (WP-B-CS-1): the prod §6.0 ENTRY-GATE resolver, extracted from
/// `run_wait` so the invariant is TEST-GUARDED. B-CS-1 makes the §6.0 flip LIVE,
/// so a regression that lets a disk-sourced status decide entry-idle on a healthy
/// channel must RED, not go silent. Thin delegate over [`entry_is_idle`]: on a
/// `Live` channel the daemon-written status decides and `disk_idle` is NEVER
/// consulted; `Down` is the ONLY path that reads disk (channel-down mode).
fn entry_gate_idle(
    entry_channel: ChannelStatusObservation,
    disk_idle: impl FnOnce() -> bool,
) -> bool {
    entry_is_idle(entry_channel, disk_idle)
}

/// RESIDUAL #2 (WP-B-CS-1): the prod §6.0 STATUS-SOURCE composition, extracted
/// from `run_wait`. On the healthy path `status_source` is the channel seam (the
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
    sleeper: &'a RealSleeper,
) -> RealWaitContentDeps<'a> {
    RealWaitContentDeps {
        status_source,
        status_fallback: Box::new(PidFileStatus { pid_file }),
        completion,
        clock,
        sleeper,
    }
}

/// Bound on how long the entry-idle gate (RESIDUAL #1) waits for the live channel
/// to SETTLE — `await_status` returns the moment the first `Republish*` frame lands
/// (Live) or the reader thread finishes without connecting (Down). Covers the
/// connect + first-frame latency of a real headless turn; a non-headless session
/// (no socket / daemon refuses) decides Down well under this, adding no latency.
const ENTRY_SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);

/// The pi wait poll floor (B1 clause 5 — pi-native timing). A daemon-hosted pi
/// session is gated by re-reading `get_state().is_streaming` over the resident ws
/// front on pi's OWN ~150ms cadence (the resident's `POLL_INTERVAL`), NEVER a
/// claude-shaped in-process "promptly" bar. A resident + ws + point-read has this
/// floor by architecture; the loop honors it rather than busy-spinning.
const PI_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

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

/// Append a content-free A6 usage line for a SUCCESSFUL `wait` (exit 0). Best-
/// effort: a failure warns but NEVER changes the verb's exit code (spec §4.1).
fn usage_wait(session_id: &str, name: Option<&str>) {
    if let Err(e) =
        dispatch::telemetry::append_usage(&RealEnv, &RealClock, "wait", Some(session_id), name)
    {
        eprintln!("qd wait: telemetry usage append failed (non-fatal): {e}");
    }
}

/// `qd wait <session>` — block until a session goes busy→idle. Port of
/// status.ts:214-260 + 359-390 (claude path).
pub fn run_wait(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    let timeout = m
        .get_one::<String>("timeout")
        .map(String::as_str)
        .unwrap_or("120");
    let timeout_ms = timeout
        .trim()
        .parse::<i64>()
        .unwrap_or(0)
        .saturating_mul(1000);

    // Resolve through the sealed uncapped entry. D-2 reject-set: can't wait on a
    // stopped session, so a tombstone is FOUND then rejected post-resolve with the
    // clear "resume it first" message.
    let session = match common::resolve_session_uncapped(query) {
        Ok(s) => s,
        Err(code) => return code,
    };
    if let Err(code) = common::reject_if_tombstoned(query, &session) {
        return code;
    }
    let session = &session;

    // label = name || truncateId(sessionId) (status.ts:223).
    let label = session
        .name
        .clone()
        .unwrap_or_else(|| truncate_id_default(&session.session_id));

    // codex P2 W6 (codex-p2-spec section 7.6): a codex row's RESOLVED status
    // short-circuits the entry-idle gate here — the CONNECTIONLESS rollout-tail
    // derivation (W5's gather); an already-idle thread completes with NO socket
    // opened — then the codex daemon-hosted wait loop (live notifications + a
    // rollout-tail fallback if the daemon is unreachable), NOT the claude pid-file +
    // transcript-content loop. RESIDUAL #1's channel routing below is CLAUDE-ONLY:
    // codex's resolved-status entry path stays byte-stable (no subscriber built).
    if session.provider == "codex" {
        if session.status == SessionStatus::Idle {
            println!("{label} is idle");
            usage_wait(&session.session_id, session.name.as_deref());
            return 0;
        }
        return run_codex_wait(session, &label, timeout_ms);
    }

    // scoped-ACP-CC residence WAIT (S7): an `acp/*` row's completion is observed by
    // PULLING the resident's event stream (next_update) over the residence socket until
    // the turn's terminal arrives. We deliberately do NOT gate on the disk `status` (a
    // send does not mutate the row, so the disk status is stale for acp) — the live pull
    // is the truth. A dead/degraded endpoint reports cold (no hang).
    if session.provider.starts_with("acp/") {
        return run_acp_wait(session, &label, timeout_ms);
    }

    // pi (B1): the daemon-hosted reconnect shape of acp/codex, but the busy/idle
    // gate is a `get_state().is_streaming` POINT-READ (drop-immune BY CONSTRUCTION —
    // a dropped stream event cannot starve a point-read), NEVER a watch for an
    // `agent_end`. Without this arm a pi row falls through to the claude pid-file +
    // transcript loop below, which is stale-disk / false-idle for a daemon-hosted
    // session that has no pid-file turn state.
    if session.provider == "pi" {
        return run_pi_wait(session, &label, timeout_ms);
    }

    run_claude_wait(session, &label, timeout_ms)
}

/// The claude-code wait loop (status.ts:214-260 + 359-390) — status + transcript-
/// content keyed poll. Factored out of [`run_wait`] as a parameter generalization
/// (an arbitrary `Session` rather than always the top-level resolved one) during
/// Child B (opencode D1), whose floor-drive caller Child D removed — today its
/// one caller is the plain claude-code arm of `run_wait`, byte-identical to the
/// pre-factoring behavior.
fn run_claude_wait(session: &dispatch::model::Session, label: &str, timeout_ms: i64) -> i32 {
    // paths_from_home cannot fail here: `common::all_sessions` above already
    // resolved it (same seam), so reaching this line proves it succeeds.
    let paths = match common::paths_from_home(&dispatch::effects::RealEnv) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // WP-B2b-2b: the live republish subscriber — ONE socket connection backing BOTH
    // B3 loop seams (control status + turn-completion) AND the entry-idle gate
    // (RESIDUAL #1), so "channel-down" is a SINGLE shared truth. Name-gated (no name
    // → no channel) and ONLY when the session's daemon socket actually EXISTS — a
    // non-headless claude session has none, so we skip the connect entirely:
    // status_source stays None = today's exact disk-keyed wait, no thread spawned.
    // The subscriber must outlive the loop (kept in `subscriber`).
    let subscriber = session.name.as_deref().and_then(|name| {
        let env = dispatch::effects::RealEnv;
        let dir = dispatch::qrmux_dir::resolve_qrmux_dir(&paths.home, &env).ok()?;
        let socket_path = qrmux::server::session_socket_path_for(Some(&dir), name).ok()?;
        socket_path.exists().then(|| {
            dispatch::wait_channel::ChannelSubscriber::connect(socket_path, name.to_string())
        })
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
        println!("{label} is idle");
        usage_wait(&session.session_id, session.name.as_deref());
        return 0;
    }

    // No pid → nothing to wait for (status.ts:354-357).
    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        eprintln!("Session has no PID (cold/dead). Nothing to wait for.");
        return 1;
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
    let provider = dispatch::provider::provider_for(&session.provider)
        .unwrap_or_else(|| dispatch::provider::provider_for("claude-code").unwrap());
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
                &dispatch::provider::SessionKey {
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
        eprintln!("qd wait: no transcript found for this session — status-keyed only");
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

    eprint!("Waiting for {label}...");
    use std::io::Write;
    let _ = std::io::stderr().flush();

    let clock = RealClock;
    let sleeper = RealSleeper;
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

    // Capture identity for the usage line before the loop (cheap clones).
    let wait_sid = session.session_id.clone();
    let wait_name = session.name.clone();

    match run_wait_content_loop(&deps, timeout_ms, 500) {
        WaitStatusOutcome::Done => {
            eprintln!(" done");
            usage_wait(&wait_sid, wait_name.as_deref());
            0
        }
        WaitStatusOutcome::SessionExited => {
            eprintln!(" session exited");
            1
        }
        WaitStatusOutcome::Timeout => {
            eprintln!(" timeout");
            1
        }
    }
}

/// codex P2 W6 (codex-p2-spec section 7.6 wait paragraph): block until a codex
/// thread goes IDLE. Connect a fresh ws client to the row's recorded endpoint
/// (re-read by pid — endpoint is NOT on the Session/--json surface) + initialize;
/// observe `thread/status/changed` broadcasts until idle. If the connect fails or
/// the daemon drops, the loop's deps fall back to polling the rollout tail
/// (`derive_status`) — the same connectionless path W5's ls uses, so the wait
/// still resolves if the daemon is gone. The existing timeout knob is honored.
fn run_codex_wait(session: &dispatch::model::Session, label: &str, timeout_ms: i64) -> i32 {
    use dispatch::boot::RealSleeper;
    use dispatch::effects::RealClock;
    use dispatch::provider::codex::{AppServerRpc, ClientInfo, WsAppServer};
    use dispatch::provider::Provider;
    use dispatch::wait::{run_codex_wait_loop, RealCodexWaitDeps};

    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // Resolve the rollout path (the fallback channel + the live anchor): the row's
    // recorded jsonl_path (exists-filtered), else CodexProvider::transcript_path
    // under its $CODEX_HOME/sessions root.
    let provider = dispatch::provider::codex::CodexProvider;
    let rollout_path = session
        .jsonl_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
        .or_else(|| {
            let fx = codex_root_fx(&env, &paths);
            let root = provider.transcript_root(&fx);
            let key = dispatch::provider::SessionKey {
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
        .and_then(|pid| dispatch::registry::read_entry(&paths.sessions_dir, pid))
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty());

    // Best-effort connect + initialize. A failure → rpc None → the deps start on the
    // rollout-tail channel (daemon-unreachable fallback, codex-p2-spec section 7.6).
    let connected: Option<WsAppServer> = endpoint.as_deref().and_then(|ep| {
        let rpc = WsAppServer::connect(ep, std::time::Duration::from_secs(5)).ok()?;
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

    eprint!("Waiting for {label}...");
    use std::io::Write;
    let _ = std::io::stderr().flush();

    let clock = RealClock;
    let sleeper = RealSleeper;
    let rpc_ref: Option<&dyn AppServerRpc> = connected.as_ref().map(|c| c as &dyn AppServerRpc);
    let deps = RealCodexWaitDeps::new(
        rpc_ref,
        rollout_path,
        // Per-poll notification read bound: short enough to re-check the deadline
        // promptly, long enough to catch a broadcast (a turn that runs long stays
        // Busy on the rollout tail anyway).
        std::time::Duration::from_millis(500),
        &clock,
        &sleeper,
    );

    let sid = session.session_id.clone();
    let name = session.name.clone();
    let outcome = run_codex_wait_loop(&deps, timeout_ms, 500);
    if let Some(c) = connected {
        let _ = c.close();
    }
    match outcome {
        WaitStatusOutcome::Done => {
            eprintln!(" done");
            usage_wait(&sid, name.as_deref());
            0
        }
        // No SessionExited on the codex path (the rollout tail is the truth even if
        // the daemon is gone); only Done / Timeout are reachable.
        WaitStatusOutcome::SessionExited | WaitStatusOutcome::Timeout => {
            eprintln!(" timeout");
            1
        }
    }
}

/// scoped-ACP-CC residence WAIT loop (S7). The ACP analog of [`run_codex_wait`]: reconnect
/// to the resident adapter (endpoint + S6 identity + S7 ladder, exactly as the SEND path),
/// then PULL `next_update` until the turn's `Terminal`/`TerminalError` event arrives or the
/// deadline elapses. The events are the REAL bridge stream relayed through the resident
/// (faithfulness keystone) — wait never synthesizes completion. No idle short-circuit on
/// the (stale-for-acp) disk status; a dead/degraded endpoint reports cold, never hangs.
/// (N-O3, Item 3) — the pure observation source the completion probe consumes: map ONE
/// `next_update` pull (the SAME real terminal observation the raw loop already saw) onto
/// the SC-2 [`AcpTurnObservation`]. This is THE distinct (O3) revert seam: corrupting
/// this mapping (e.g. always `Pending`, or `Terminal`→`Pending`) makes the wait verdict
/// diverge from the raw pull — the redundancy the oracle reverts here to catch. NO new
/// completion source: it consumes the existing `next_update`, just routed through the
/// completion contract.
fn acp_wait_observation(
    res: Result<Option<dispatch::provider::acp::AcpEvent>, dispatch::provider::acp::AcpError>,
) -> dispatch::provider::acp::AcpTurnObservation {
    use dispatch::provider::acp::{AcpEvent, AcpTurnObservation};
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

/// A single-shot [`AcpTurnObserver`] holding ONE mapped observation — lets the wait verb
/// (which drives the ws connection, not the host) feed [`AcpTurnCompletion`] the SAME
/// observation the raw `next_update` produced.
struct OneShotObserver(dispatch::provider::acp::AcpTurnObservation);
impl dispatch::provider::acp::AcpTurnObserver for OneShotObserver {
    fn observe(&self) -> dispatch::provider::acp::AcpTurnObservation {
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
/// `emit_acp_terminal` LANDING-CHECKS them against the ACP `~/.claude/projects`
/// record (a Cancelled turn adds the user prompt BEFORE interrupting the response),
/// and only a provably-not-landed one becomes `seen-failed`.
enum AcpDeliveryDisposition {
    /// Delivered + landed → `message-seen` (no landing check — a completed/limit
    /// turn consumed the prompt).
    Seen,
    /// A FAILURE StopReason — its `seen-failed` reason IF the rider-3 landing check
    /// proves the prompt did NOT land; landed → `message-seen`; ambiguous →
    /// recoverable (see `emit_acp_terminal`).
    Failed(&'static str),
}

fn acp_delivery_disposition(
    reason: dispatch::provider::acp::TerminalReason,
) -> AcpDeliveryDisposition {
    use dispatch::provider::acp::TerminalReason as R;
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
    raw: &Result<Option<dispatch::provider::acp::AcpEvent>, dispatch::provider::acp::AcpError>,
) -> Option<String> {
    use dispatch::provider::acp::AcpEvent;
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
    let Some(path) = dispatch::jsonl::find_jsonl_path(projects_dir, session_id, None) else {
        return AcpLanded::Unknown; // transcript not resolvable → recoverable
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return AcpLanded::Unknown; // unreadable → recoverable
    };
    for p in dispatch::sendpty::parse_jsonl_slice(&content) {
        let rec: dispatch::sendpty::JsonlRecord =
            serde_json::from_value(p.value.clone()).unwrap_or_default();
        if let Some(text) = dispatch::sendpty::user_record_text(&rec) {
            if dispatch::events::sha256_hex(text.as_bytes()) == content_sha256 {
                return AcpLanded::Yes;
            }
        }
    }
    AcpLanded::No // readable, prompt absent (degraded to recoverable at R6 — see emit_acp_terminal)
}

/// C3/C5 (D2 obligation (b)) + rider-3 — emit the ACP delivery TERMINAL keyed to
/// the send by its turn id, best-effort, into the TARGET's delivery log.
/// content_sha256 is read by the send_id-join from the correlated `send-initiated`
/// (run_acp_wait holds the turn id, not the sent bytes; the join is the
/// protocol-native correlation ACP's turn id gives us). A SUCCESS StopReason
/// (Completed/MaxTokens) → `message-seen` (the completed turn consumed the prompt).
/// A FAILURE StopReason is POST-delivery, so it is LANDING-CHECKED against the ACP
/// session's `~/.claude/projects` record (rider-3): landed → `message-seen`. R6
/// DEGRADE (write-ordering unprovable on brano — bridge absent): NOT-landed and
/// ambiguous BOTH → NO terminal (the send stays RECOVERABLE, disclosed as a C6
/// pending-forever residual) — never a foreclosing hard fail on an unprovable
/// not-landed. So this path emits ONLY `message-seen` (never `seen-failed`).
/// Idempotent:
/// first-terminal-wins skips a send that already has a terminal. A landed turn with
/// NO correlated send-initiated emits nothing — there is no send to terminate.
fn emit_acp_terminal(
    env: &dyn dispatch::effects::Env,
    target: &dispatch::model::Session,
    send_id: &str,
    reason: dispatch::provider::acp::TerminalReason,
) {
    // QD_HOME-consistency (delivery-lie fix): resolve the delivery-log `state_dir`
    // (and the landing-check `projects_dir`) with the QD_HOME-HONORING `from_home_env`
    // — EXACTLY as the ACP send-phase emitters do (send_relay.rs) and the pi terminal
    // does. run_acp_wait's `common::paths_from_home` uses `from_home`, which IGNORES
    // QD_HOME (paths.rs:48-51), so under any QD_HOME override ≠ <HOME>/.quorum/dispatch
    // the terminal landed in a DIFFERENT delivery log than its send-initiated/
    // turn-accepted phases → an orphaned / pending-forever send (the terminal never
    // joins by send_id). Best-effort (no HOME → return), as the send emitters are.
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return;
    };
    let paths = dispatch::paths::QdPaths::from_home_env(std::path::Path::new(&home), env);
    let log = std::fs::read_to_string(dispatch::events::events_path(
        &paths.state_dir,
        &target.session_id,
    ))
    .unwrap_or_default();
    let records = dispatch::events::parse_events(&log).records;
    if dispatch::events::first_terminal_for(&records, send_id).is_some() {
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
    let seen = || dispatch::events::Payload::MessageSeen {
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
    let writer = dispatch::events::EventWriter::for_key(
        &paths.state_dir,
        &target.session_id,
        Some(target.session_id.clone()),
        target.name.clone(),
    );
    dispatch::events::warn_emit(&writer, &RealClock, &payload);
}

fn run_acp_wait(session: &dispatch::model::Session, label: &str, timeout_ms: i64) -> i32 {
    use dispatch::provider::acp::{
        classify_connect_failure, derive_tier, AcpClient, AcpConnection, AcpTurnCompletion,
        AcpTurnObservation, ConnectFailure, TerminalReason, Tier,
    };
    use dispatch::wait::{TurnCompletion, TurnCompletionProbe};
    use std::io::Write;
    use std::time::{Duration, Instant};

    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let cold = |label: &str| {
        eprintln!("qd wait: \"{label}\": acp session daemon not reachable (try qd resume {label}).");
        1
    };

    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        // Child D (opencode D1): for acp/claude-code a pid-less row is
        // already-lost transport (the claude CLI's janitor may have reaped the
        // registry row entirely) — preserve identity and refuse to the SAME
        // human-recovery surface as the dead-tier branch below. Any other
        // acp/* provider keeps the pre-Child-D wording, byte-identical.
        if session.provider == "acp/claude-code" {
            acp_loss::preserve_identity(session, "acp session has no live daemon pid at wait entry");
            return cold(label);
        }
        eprintln!("Session has no PID (cold/dead). Nothing to wait for.");
        return 1;
    };
    let entry = dispatch::registry::read_entry(&paths.sessions_dir, pid);
    let endpoint = entry
        .as_ref()
        .and_then(|e| e.endpoint.clone())
        .filter(|s| !s.is_empty());
    let transport_field = entry.as_ref().and_then(|e| e.transport.clone());

    // S6 identity + S7 ladder (same gate as the SEND path — connect-success is liveness,
    // cmdline+pid is identity; drive only on Tier::Acp).
    let cmdline = dispatch::create_daemon::real_cmdline_probe(pid);
    let endpoint_alive = endpoint.is_some()
        && dispatch::effects::is_pid_alive(pid as i32)
        && dispatch::acp_residence::cmdline_is_our_acp_daemon(cmdline.as_deref(), endpoint.as_deref());
    if derive_tier("acp/claude-code", transport_field.as_deref(), endpoint_alive) != Tier::Acp {
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
        acp_loss::preserve_identity(session, "acp endpoint not reachable at wait entry");
        return cold(label);
    }
    let endpoint = endpoint.expect("Tier::Acp implies a live endpoint");

    let conn = match AcpConnection::connect(&endpoint, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(_) => {
            // Round-1 TOCTOU fix (same split as run_acp_send's connect arm):
            // the pre-connect probe cannot confirm liveness across the connect
            // boundary. Re-probe NOW: a genuine wedge (alive + identity-verified)
            // refuses with no tombstone — its live-pid row is not janitor-
            // reapable; a daemon that died in the window is a transport LOSS —
            // preserve identity, then the same cold refusal. The wedge-dies-later
            // residual is stated + defended in ladder.rs clause 3.
            let pid_alive_now = dispatch::effects::is_pid_alive(pid as i32);
            let cmdline_is_ours_now = dispatch::acp_residence::cmdline_is_our_acp_daemon(
                dispatch::create_daemon::real_cmdline_probe(pid).as_deref(),
                Some(endpoint.as_str()),
            );
            if classify_connect_failure(pid_alive_now, cmdline_is_ours_now)
                == ConnectFailure::TransportLost
            {
                acp_loss::preserve_identity(
                    session,
                    "acp daemon died across the connect boundary (was live at the pre-connect probe)",
                );
            }
            return cold(label);
        }
    };

    // (N-idle, Item 3) ENTRY-IDLE short-circuit: a genuinely-idle session (no turn in
    // flight — the SC-1 queue's primary truth) returns PROMPTLY instead of camping to the
    // timeout (the codex entry-idle gate analog). A turn IN FLIGHT reports in_flight=true
    // → we fall through to the wait loop, so a mid-turn wait NEVER false-idles. A failed
    // status probe falls through too (the loop handles a dead channel).
    if let Ok(false) = conn.status_in_flight() {
        eprintln!("{label} is idle.");
        return 0;
    }

    eprint!("Waiting for {label}...");
    let _ = std::io::stderr().flush();

    // Honor the timeout knob; a 0/absent timeout defaults to the 120s wait default.
    let total = if timeout_ms > 0 {
        Duration::from_millis(timeout_ms as u64)
    } else {
        Duration::from_secs(120)
    };
    let deadline = Instant::now() + total;
    loop {
        let now = Instant::now();
        if now >= deadline {
            eprintln!(" timeout");
            return 1;
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
                // the wait's own exit below.
                if let (Some(tid), Some((reason, _))) =
                    (turn_id.as_deref(), completion.terminal_reason())
                {
                    // env (RealEnv) — emit_acp_terminal resolves its delivery-log
                    // state_dir via the QD_HOME-honoring from_home_env itself, so it
                    // shares the log with the send phases under any QD_HOME.
                    emit_acp_terminal(&env, session, tid, reason);
                }
                if matches!(completion.terminal_reason(), Some((TerminalReason::Failed, _))) {
                    let msg = match &obs {
                        AcpTurnObservation::Failed(m) => m.clone(),
                        _ => String::new(),
                    };
                    eprintln!(" failed: {msg}");
                    return 1;
                }
                eprintln!(" done");
                // FINDING #2 PART 2 — VERIFY-THE-BRIDGE (cold-path, one-time): if this is
                // the FIRST wait after a resume (a marker exists for the row's pid),
                // confirm from PRIMARY source that the post-resume turn CONTINUED the SAME
                // bridge JSONL — a fork-on-load is FAILED LOUD. Gated on the marker so a
                // normal wait pays only one cheap stat; a non-resume wait does nothing.
                if let Some(code) = verify_post_resume_if_marked(&paths, &session) {
                    return code; // fork detected → fail loud (nonzero); else proceed.
                }
                usage_wait(&session.session_id, session.name.as_deref());
                return 0;
            }
            // Still in flight (streaming / quiet poll) → keep pulling until the terminal.
            TurnCompletionProbe::Pending => continue,
            // Transport gone before any terminal → source integrity lost →
            // exit 1 (surface unchanged). Child D: the daemon died UNDER a
            // camped wait — the same identity-preservation duty as an
            // entry-lane loss (the janitor will reap the row ~1s from now),
            // so record the tombstone before surfacing.
            TurnCompletionProbe::Degraded(_) => {
                acp_loss::preserve_identity(
                    session,
                    "acp transport lost mid-wait (channel closed before the turn's terminal)",
                );
                eprintln!(" channel closed");
                return 1;
            }
        }
    }
}

/// The pi wait gate decision from ONE `is_streaming` point-read — the drop-immune
/// core of [`run_pi_wait`], shared by the ENTRY gate and the poll loop. This is the
/// single seam that separates the THREE gate cases (B1 clause 2):
///   * `false` → RELEASE. Covers BOTH "finished" and "never-started": a session
///     with no turn in flight reads `is_streaming:false`, so neither hangs — a
///     just-booted pi that never got a prompt releases immediately.
///   * `true`  → BLOCK. A turn is genuinely in flight; keep point-reading.
/// A live endpoint that ERRORS is neither case — the caller maps it to `cold`
/// (exit 1), never a false release and never a hang.
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
fn run_pi_wait(session: &dispatch::model::Session, label: &str, timeout_ms: i64) -> i32 {
    use dispatch::provider::pi::{residence::cmdline_is_our_pi_daemon, PiRemote};
    use std::io::Write;
    use std::time::{Duration, Instant};

    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let cold = |label: &str| {
        eprintln!("qd wait: \"{label}\": pi session daemon not reachable (try qd resume {label}).");
        1
    };

    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        eprintln!("Session has no PID (cold/dead). Nothing to wait for.");
        return 1;
    };
    let entry = dispatch::registry::read_entry(&paths.sessions_dir, pid);
    let endpoint = entry
        .as_ref()
        .and_then(|e| e.endpoint.clone())
        .filter(|s| !s.is_empty());

    // Identity + liveness gate (the acp SEND/WAIT posture): a live endpoint, the pid
    // alive, and the `/proc` cmdline is OUR pi-daemon for THIS endpoint (defeats pid
    // reuse — a connect-success is liveness, cmdline+endpoint is identity). Any miss
    // → cold, never drive a stranger's socket.
    let cmdline = dispatch::create_daemon::real_cmdline_probe(pid);
    let alive = endpoint.is_some()
        && dispatch::effects::is_pid_alive(pid as i32)
        && cmdline_is_our_pi_daemon(cmdline.as_deref(), endpoint.as_deref());
    if !alive {
        return cold(label);
    }
    let endpoint = endpoint.expect("alive implies a live endpoint");

    let remote = match PiRemote::connect(&endpoint, Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => return cold(label),
    };

    // ENTRY gate — the drop-immune point-read separates the three cases.
    match remote.observe() {
        Ok(obs) => {
            if pi_wait_verdict(obs.is_streaming) == PiWaitVerdict::Release {
                // C5/C3 (obligation (c)): at turn close, content-key the rollout
                // and emit message-seen for any pending pi send that landed.
                pi_observe_landed_sends(&env, &paths, session);
                eprintln!("{label} is idle.");
                usage_wait(&session.session_id, session.name.as_deref());
                return 0;
            }
        }
        Err(_) => return cold(label),
    }

    eprint!("Waiting for {label}...");
    let _ = std::io::stderr().flush();

    // Honor the timeout knob; a 0/absent timeout defaults to the 120s wait default.
    let total = if timeout_ms > 0 {
        Duration::from_millis(timeout_ms as u64)
    } else {
        Duration::from_secs(120)
    };
    let deadline = Instant::now() + total;
    loop {
        if Instant::now() >= deadline {
            eprintln!(" timeout");
            return 1;
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
                    pi_observe_landed_sends(&env, &paths, session);
                    eprintln!(" done");
                    usage_wait(&session.session_id, session.name.as_deref());
                    return 0;
                }
                PiWaitVerdict::Block => continue,
            },
            // The resident dropped / went unreachable mid-wait → source integrity
            // lost → exit 1 (never a false done, never a hang).
            Err(_) => {
                eprintln!(" channel closed");
                return 1;
            }
        }
    }
}

// ===========================================================================
// C5/C3 — the pi CONTENT-KEYED rollout OBSERVER (D2 obligation (c))
// ===========================================================================
//
// A resident pi send is FIRE-AND-FORGET: run_pi_send emits sent (send-initiated) +
// delivered (turn-accepted) at the inject ACK and returns; the success TERMINAL is
// the OBSERVER's job. The observer confirms ENTRY by finding the SENT BYTES as a
// USER-turn record in the provider's rollout jsonl, matched on the send's
// content_sha256 (the SAME key relay message-seen matches on) — NEVER on the turn
// terminal (a steer into an open turn yields a terminal that says nothing about
// whether the steered text was consumed; obligation (c) point 1). pi lazy-writes
// its session jsonl only after a turn, and dispatch keeps one-shot writers off the
// resident's OPEN file (single-writer discipline), so the observer reads READ-ONLY
// at/after turn close; mid-turn confirmation is structurally unavailable ((c) pt 2).
// The emitted message-seen is the FLOOR (record-presence) reading; the reader
// recovers "floor vs strong" from the paired send-initiated's send_path (the lane)
// + D4's per-carrier reading table (C3 honesty).

// The content-keyed LANDING test (obligation (c) point 1) lives in the pi provider
// (`dispatch::provider::pi::floor::rollout_landed`) — the home of pi session-file
// format knowledge — so the RESIDENT observer here and the DEAD-ONLY floor
// (`run_pi_floor_send`) share ONE matcher. It matches a USER-turn record's text
// against the send's content_sha256; a steer's text lands as its own user record,
// and an assistant echo is never a landing.

/// The pending pi sends whose content has LANDED — pure over the delivery-log
/// records + the rollout. A pending pi send = a `send-initiated` on the pi lane
/// (send_path=="pi") with NO terminal yet (first-terminal-wins); it has LANDED when
/// its content_sha256 is present as a user-turn record in the rollout. Returns
/// (send_id, content_sha256) for each — exactly one message-seen apiece.
fn pi_pending_landed_sends(
    records: &[dispatch::events::EventRecord],
    rollout_jsonl: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for rec in records {
        if rec.event != "send-initiated" {
            continue;
        }
        // pi lane only; other lanes have their own terminal path.
        if rec.str_field("send_path").as_deref() != Some("pi") {
            continue;
        }
        let Some(sid) = rec.send_id() else {
            continue;
        };
        // Un-terminated only (first-terminal-wins) — never a second terminal.
        if dispatch::events::first_terminal_for(records, &sid).is_some() {
            continue;
        }
        let Some(sha) = rec.str_field("content_sha256") else {
            continue;
        };
        if dispatch::provider::pi::floor::rollout_landed(rollout_jsonl, &sha) {
            out.push((sid, sha));
        }
    }
    out
}

/// Emit `message-seen{send_id, content_sha256}` for each pending pi send whose
/// content has LANDED in `rollout_jsonl`, into the TARGET's delivery log (keyed by
/// uuid). Pure over its (env, target, delivery-log text, rollout text) inputs so
/// the observer is hermetically testable. Best-effort: a write failure warns and
/// never changes the wait outcome. Returns the count emitted.
fn emit_pi_seen_for_landed(
    env: &dyn dispatch::effects::Env,
    target: &dispatch::model::Session,
    delivery_log: &str,
    rollout_jsonl: &str,
) -> usize {
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return 0;
    };
    let state_dir =
        dispatch::paths::QdPaths::from_home_env(std::path::Path::new(&home), env).state_dir;
    let records = dispatch::events::parse_events(delivery_log).records;
    let landed = pi_pending_landed_sends(&records, rollout_jsonl);
    if landed.is_empty() {
        return 0;
    }
    let writer = dispatch::events::EventWriter::for_key(
        &state_dir,
        &target.session_id,
        Some(target.session_id.clone()),
        target.name.clone(),
    );
    let clock = RealClock;
    for (send_id, content_sha256) in &landed {
        dispatch::events::warn_emit(
            &writer,
            &clock,
            &dispatch::events::Payload::MessageSeen {
                send_id: send_id.clone(),
                content_sha256: content_sha256.clone(),
            },
        );
    }
    landed.len()
}

/// Drive the pi content-keyed observer from the wait seam (at/after turn close):
/// read the resident's rollout jsonl (READ-ONLY, via the row's recorded
/// `jsonl_path`) + the target's delivery log, then emit message-seen for every
/// pending pi send whose content LANDED. Best-effort; never changes the wait exit.
/// Resident lane only. (The deferred live steer-into-open-turn cell exercises this
/// end-to-end under pi OAuth; the observer's CORRECTNESS is proven hermetically via
/// `emit_pi_seen_for_landed`.)
fn pi_observe_landed_sends(
    env: &RealEnv,
    paths: &dispatch::paths::QdPaths,
    session: &dispatch::model::Session,
) {
    let Some(rollout_path) = session
        .jsonl_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
    else {
        return;
    };
    let Ok(rollout_jsonl) = std::fs::read_to_string(&rollout_path) else {
        return;
    };
    let delivery_log =
        std::fs::read_to_string(dispatch::events::events_path(&paths.state_dir, &session.session_id))
            .unwrap_or_default();
    let _ = emit_pi_seen_for_landed(env, session, &delivery_log, &rollout_jsonl);
}

/// FINDING #2 PART 2 — the cold-path VERIFY-THE-BRIDGE consumer. Returns `Some(exit)`
/// ONLY on a detected fork (fail loud, nonzero); `None` to proceed (Continued, or an
/// Unconfirmed that emits a LOUD degraded-confidence warning but does not fail the turn).
/// One-time: the marker is consumed (removed) whatever the verdict.
fn verify_post_resume_if_marked(
    paths: &dispatch::paths::QdPaths,
    session: &dispatch::model::Session,
) -> Option<i32> {
    let pid = session.pid.filter(|&p| p != 0)?;
    verify_post_resume_for(&paths.sessions_dir, &paths.projects_dir, pid, &session.session_id)
}

/// The testable core (primitives, no `Session`): consume the resume-verify marker keyed by
/// `pid` and rule on the post-resume continuation for the CURRENT row's `current_session_id`.
///
/// R2-1 (red-team round-2) — TWO independently-revertible paths:
///   (V)(5) FALSE-POSITIVE FIX [the sid cross-check]: a marker whose `session_id` ≠ the
///     CURRENT row's `current_session_id` is STALE/ALIASED — it survived a stop and `pid`
///     was REUSED by a different session. Remove it and SKIP the verify (a NORMAL wait).
///     Without this, a faithful pid-reused session B reads stale marker M_A and FALSE-FAILS
///     as a fork. This is a SEPARATE branch — reverting ONLY it re-opens the false-fail.
///   (V)(1) GENUINE-FORK DETECTION [unchanged]: when the marker IS for this session
///     (`session_id` matches), run the real on-disk fork-check and FAIL LOUD on a true
///     fork. Reverting ONLY the fork-detection re-opens the real-fork hole. The two seams
///     never co-fire: the skip applies STRICTLY to a sid-mismatch, never to a sid-match.
fn verify_post_resume_for(
    sessions_dir: &std::path::Path,
    projects_dir: &std::path::Path,
    pid: i64,
    current_session_id: &str,
) -> Option<i32> {
    use dispatch::resume_daemon::{
        read_resume_verify_marker, resume_verify_marker_path, verify_post_resume_continuation,
        ResumeContinuation,
    };
    let marker_path = resume_verify_marker_path(sessions_dir, pid);
    let marker = read_resume_verify_marker(&marker_path)?; // absent → normal wait (one stat).

    // (V)(5) sid cross-check — the LOAD-BEARING R2-1 fix. STRICTLY a mismatch skip: an
    // aliased marker (different session) is removed and IGNORED, never verified against the
    // wrong session. Defends against pid-reuse regardless of stop-cleanup.
    if marker.session_id != current_session_id {
        let _ = std::fs::remove_file(&marker_path);
        return None; // stale/aliased marker → treat as a normal wait (no false fork-fail).
    }

    // (V)(1) the marker IS for THIS session → the genuine post-resume fork-check (unchanged).
    // Bounded retry for the JSONL flush lag (eventual-consistency vs the wire terminal).
    let verdict = verify_post_resume_continuation(projects_dir, &marker, 8, 250);
    let _ = std::fs::remove_file(&marker_path); // one-time: consume the marker.
    match verdict {
        ResumeContinuation::Continued => None, // faithful continuation — proceed.
        ResumeContinuation::Forked(other) => {
            eprintln!(
                " FAITHFULNESS FAILURE: the post-resume turn did NOT continue session {} \
                 — the bridge forked on load (the turn landed in {other}). The resumed \
                 conversation is NOT continuous; treat this resume as failed.",
                marker.session_id
            );
            Some(1)
        }
        ResumeContinuation::Unconfirmed => {
            // AMBIGUOUS (super7 stance): do NOT silently pass, do NOT fail a good turn —
            // a LOUD degraded-confidence warning, then proceed (exit 0).
            eprintln!(
                " WARNING (degraded confidence): could not confirm on disk that the \
                 post-resume turn continued session {} (no JSONL growth and no fork \
                 detected within the retry budget). The turn completed; continuation is \
                 UNVERIFIED.",
                marker.session_id
            );
            None
        }
    }
}

/// A minimal `ProviderFx` for resolving the codex `transcript_root` off env only
/// (codex's `transcript_root` reads `fx.env` $CODEX_HOME/$HOME — never paths).
fn codex_root_fx<'a>(
    env: &'a RealEnv,
    paths: &'a dispatch::paths::QdPaths,
) -> dispatch::provider::ProviderFx<'a> {
    dispatch::provider::ProviderFx {
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

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch::wait::{ChannelStatus, WaitContentDeps};
    use std::cell::Cell;

    // Child D structural guard (opencode D1 — clerk-4's Arm-B ratification, bond
    // note 01KX01BY7G): `run_acp_wait` must have NO reachable auto-deliver/drive
    // path on transport loss — Child B's latched-drive lane was REMOVED, not
    // gated (this subsumes R5-3's never-fresh-degrade: wait now never degrades,
    // latches, or spawns ANYTHING). The replacement disposition is pinned as the
    // positive control: identity preservation (`acp_loss::preserve_identity`) at
    // the pid-less entry, the dead-tier entry, the connect-boundary death
    // (round-1 TOCTOU fix: the re-probe's TransportLost classification), and
    // the camped-loss (channel closed) exit — each surfacing through an
    // existing refusal. Structural
    // (source-text), scoped to `run_acp_wait`'s own body so this test's own
    // assertion strings can't make it vacuously true. MUTATION EVIDENCE:
    // reintroducing a floor drive or latch write reds the bans; deleting a
    // preserve_identity call reds the count.
    #[test]
    fn acp_wait_transport_loss_refuses_with_identity_preserved_and_never_drives() {
        let src = include_str!("wait.rs");
        let fn_start = src
            .find("fn run_acp_wait(session: &dispatch::model::Session, label: &str, timeout_ms: i64) -> i32 {")
            .expect("run_acp_wait must still exist verbatim");
        let after_start = &src[fn_start..];
        let fn_end = after_start
            .find("\nenum PiWaitVerdict")
            .expect("PiWaitVerdict must still follow run_acp_wait");
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
                "run_acp_wait must never reach `{banned}` — an observation verb neither \
                 latches, degrades, drives a floor, nor branches on send history; \
                 acp/claude-code transport loss REFUSES with identity preserved. \
                 Body:\n{body}"
            );
        }
        let occurrences = body.matches("acp_loss::preserve_identity(").count();
        assert_eq!(
            occurrences, 4,
            "run_acp_wait must preserve identity at exactly its four transport-loss \
             surfaces (pid-less entry, dead-tier entry, connect-boundary death \
             [round-1 TOCTOU fix], camped channel-closed) — found {occurrences}. \
             Body:\n{body}"
        );
        // And the refusal surface itself still lands (a body that preserved
        // identity but silently stopped refusing would pass the counts above).
        assert!(
            body.contains("return cold(label);"),
            "the cold human-recovery refusal must still be the loss surface"
        );
        // Round-1 TOCTOU fix pin: the connect-Err arm must classify from a
        // FRESH re-probe (never trust the stale pre-connect reading).
        assert!(
            body.contains("classify_connect_failure(pid_alive_now, cmdline_is_ours_now)"),
            "run_acp_wait's connect-Err arm must re-probe and classify wedge-vs-loss"
        );
    }

    /// (N-O3) the wait terminal-detection factored through the SC-2 completion contract:
    /// each `next_update` outcome maps to the right observation AND the SAME completion
    /// verdict the raw inline match produced — behaviorally identical, no new source.
    /// REVERT SEAM: corrupt `acp_wait_observation` (e.g. Terminal→Pending) → the verdict
    /// diverges (a completed turn never reads Visible) → this REDs.
    #[test]
    fn acp_wait_observation_factors_through_completion_contract() {
        use dispatch::provider::acp::{
            AcpEvent, AcpTurnCompletion, AcpTurnObservation, StopReason, TerminalReason,
        };
        use dispatch::wait::{TurnCompletion, TurnCompletionProbe};

        let ev = |e: AcpEvent| Ok(Some(e));
        let verdict = |obs: AcpTurnObservation| {
            let o = super::OneShotObserver(obs);
            AcpTurnCompletion::new(&o).poll_completion()
        };

        // clean end_turn terminal → Terminal(EndTurn) → Visible, reason Completed (done/0).
        let t = super::acp_wait_observation(ev(AcpEvent::Terminal {
            session: "s".into(),
            turn: "t".into(),
            stop: StopReason::EndTurn,
        }));
        assert_eq!(t, AcpTurnObservation::Terminal(StopReason::EndTurn));
        assert_eq!(verdict(t.clone()), TurnCompletionProbe::Visible);
        let o = super::OneShotObserver(t);
        assert_eq!(
            AcpTurnCompletion::new(&o).terminal_reason().unwrap().0,
            TerminalReason::Completed
        );

        // a JSON-RPC error terminal → Failed → Visible, reason Failed (failed/1).
        let f = super::acp_wait_observation(ev(AcpEvent::TerminalError {
            session: "s".into(),
            turn: "t".into(),
            message: "internalError".into(),
        }));
        assert_eq!(f, AcpTurnObservation::Failed("internalError".into()));
        assert_eq!(verdict(f.clone()), TurnCompletionProbe::Visible);
        let o = super::OneShotObserver(f);
        assert_eq!(
            AcpTurnCompletion::new(&o).terminal_reason().unwrap().0,
            TerminalReason::Failed
        );

        // a streamed update / a quiet poll → Pending (keep polling).
        let u = super::acp_wait_observation(ev(AcpEvent::Update {
            session: "s".into(),
            kind: "agent_message_chunk".into(),
            payload: serde_json::Value::Null,
        }));
        assert_eq!(u, AcpTurnObservation::Pending);
        assert_eq!(verdict(u), TurnCompletionProbe::Pending);
        assert_eq!(
            super::acp_wait_observation(Ok(None)),
            AcpTurnObservation::Pending
        );

        // transport gone → TransportLost → Degraded (never a false done).
        let d = super::acp_wait_observation(Err(dispatch::provider::acp::AcpError::Closed));
        assert!(matches!(d, AcpTurnObservation::TransportLost(_)));
        assert!(matches!(verdict(d), TurnCompletionProbe::Degraded(_)));
    }

    // ================= R2-1 — marker pid-reuse aliasing (the two seams) =================
    use dispatch::jsonl::cwd_to_project_path;
    use dispatch::resume_daemon::{
        resume_verify_marker_path, write_resume_verify_marker, ResumeVerifyMarker,
    };

    /// Plant the RED-TEAM's exact repro: a marker for session `marker_sid` (baseline 3,
    /// baseline_files=[A.jsonl]) + the project dir with `<marker_sid>.jsonl` UNCHANGED at
    /// 3 lines + a NEW `B.jsonl`. Returns (sessions_dir, projects_dir, pid).
    fn plant_r2_1_repro(tmp: &std::path::Path, marker_sid: &str, cwd: &str, pid: i64) -> (std::path::PathBuf, std::path::PathBuf) {
        let sessions = tmp.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let projects = tmp.join("projects");
        let dir = projects.join(cwd_to_project_path(cwd));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{marker_sid}.jsonl")), "l1\nl2\nl3\n").unwrap(); // baseline 3, flat
        std::fs::write(dir.join("B.jsonl"), "u1\na1\n").unwrap(); // the new (would-be "fork") file
        write_resume_verify_marker(
            &resume_verify_marker_path(&sessions, pid),
            &ResumeVerifyMarker {
                session_id: marker_sid.into(),
                cwd: Some(cwd.into()),
                baseline_lines: 3,
                baseline_files: vec![format!("{marker_sid}.jsonl")],
            },
        )
        .unwrap();
        (sessions, projects)
    }

    /// (V)(5) FALSE-POSITIVE FIX [the sid cross-check seam]: a stale marker for session A,
    /// the adapter pid REUSED by a different session B (same cwd) — the first `wait B`
    /// MUST SKIP the aliased marker (no false fork-fail) and remove it. REVERT CONTROL:
    /// delete the `marker.session_id != current_session_id` skip → this calls verify for A
    /// → A.jsonl flat + B.jsonl new → `Forked` → `Some(1)` → the `None` assert REDs.
    #[test]
    fn r2_1_aliased_marker_skipped_not_false_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = 4242;
        let (sessions, projects) = plant_r2_1_repro(tmp.path(), "A", "/w/projX", pid);
        // pid reused by session "B" (≠ the marker's "A") → aliased marker → SKIP.
        let code = super::verify_post_resume_for(&sessions, &projects, pid, "B");
        assert_eq!(code, None, "(V)(5) an aliased (sid-mismatch) marker is SKIPPED — no false fork-fail on faithful B");
        assert!(
            !resume_verify_marker_path(&sessions, pid).exists(),
            "the stale/aliased marker is removed"
        );
    }

    /// (V)(1) GENUINE-FORK DETECTION still fires [the fork-detection seam]: when the marker
    /// IS for the CURRENT session (sid MATCH) and a real fork happened (requested flat, the
    /// turn in a new file), `wait` FAILS LOUD. REVERT CONTROL: weaken the fork-detection →
    /// this no longer returns `Some(1)` → RED. (Distinct seam from (V)(5).)
    #[test]
    fn r2_1_genuine_fork_still_detected_on_sid_match() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = 4243;
        let (sessions, projects) = plant_r2_1_repro(tmp.path(), "A", "/w/projX", pid);
        // the marker IS for session "A" and "A".jsonl did NOT grow + B.jsonl is new → fork.
        let code = super::verify_post_resume_for(&sessions, &projects, pid, "A");
        assert_eq!(code, Some(1), "(V)(1) a genuine fork (sid match, turn in a NEW file) STILL fails loud");
    }

    /// Happy path: sid MATCH + the requested file GREW → faithful continuation → proceed.
    #[test]
    fn r2_1_faithful_continuation_on_sid_match_proceeds() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = 4244;
        let (sessions, projects) = plant_r2_1_repro(tmp.path(), "A", "/w/projX", pid);
        // grow A.jsonl beyond baseline 3 → faithful continuation.
        let dir = projects.join(cwd_to_project_path("/w/projX"));
        std::fs::write(dir.join("A.jsonl"), "l1\nl2\nl3\nl4\n").unwrap();
        let code = super::verify_post_resume_for(&sessions, &projects, pid, "A");
        assert_eq!(code, None, "sid match + requested file grew → faithful → proceed (exit 0)");
    }

    /// No marker for the pid → a NORMAL wait (no-op), zero work beyond the stat.
    #[test]
    fn r2_1_no_marker_is_a_normal_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let projects = tmp.path().join("projects");
        assert_eq!(super::verify_post_resume_for(&sessions, &projects, 9999, "anything"), None);
    }

    /// B1 — the pi wait gate separates the THREE cases off a single is_streaming
    /// point-read. REVERT SEAM: invert `pi_wait_verdict` (streaming→Release) and a
    /// mid-turn wait would false-idle; collapse it to always-Block and a finished/
    /// never-started session would hang — either way this REDs.
    #[test]
    fn pi_wait_verdict_separates_the_three_gate_cases() {
        use super::{pi_wait_verdict, PiWaitVerdict};
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
    /// `run_wait`'s `status_source` to `None` and `build_wait_deps` carries `None`
    /// → `read_status()` falls through to the disk "busy" → this REDs. The disk-as-
    /// status read B eliminates cannot creep back silently.
    #[test]
    fn build_wait_deps_live_channel_beats_disagreeing_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = plant_pid_json(tmp.path(), "busy"); // disk DISAGREES
        let clock = RealClock;
        let sleeper = RealSleeper;
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
        let sleeper = RealSleeper;
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
    // C5/C3 — the pi content-keyed rollout OBSERVER (D2 obligation (c)),
    // proven HERMETICALLY (cred-free, no model): a fixture rollout + delivery
    // log drive the real observer emission. The full live steer-into-open-turn
    // cell is deferred on pi OAuth; the observer's CORRECTNESS holds here.
    // =======================================================================

    fn pi_target(uuid: &str) -> dispatch::model::Session {
        dispatch::model::Session {
            name: Some("pi-obs-1".to_string()),
            user_named: None,
            session_id: uuid.to_string(),
            code: None,
            qd_id: None,
            pid: Some(9191),
            status: dispatch::model::SessionStatus::Idle,
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
            provider: "pi".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: dispatch::model::SessionBranch::LiveRegistry,
        }
    }

    /// A pi session-jsonl with ONE user-turn record carrying `user_text` (the
    /// pi `appendMessage` shape), plus the assistant turn + agent_end. If
    /// `include_user` is false, the user record is omitted (the "not flushed yet
    /// / not landed" case) but the assistant still ECHOES `user_text` — the
    /// false-positive guard: an assistant echo must NOT be read as landed.
    fn pi_rollout(user_text: &str, include_user: bool) -> String {
        let mut s = String::new();
        s.push_str("{\"type\":\"session\",\"version\":3,\"id\":\"s1\"}\n");
        s.push_str("{\"type\":\"agent_start\"}\n{\"type\":\"turn_start\"}\n");
        if include_user {
            let user = serde_json::json!({
                "type": "message", "id": "m1", "parentId": null, "timestamp": "t",
                "message": {"role": "user", "content": [{"type": "text", "text": user_text}]}
            });
            s.push_str(&serde_json::to_string(&user).unwrap());
            s.push('\n');
        }
        let asst = serde_json::json!({
            "type": "turn_end",
            "message": {"role": "assistant", "content": [{"type": "text", "text": user_text}], "stopReason": "stop"}
        });
        s.push_str(&serde_json::to_string(&asst).unwrap());
        s.push_str("\n{\"type\":\"agent_end\",\"messages\":[],\"willRetry\":false}\n");
        s
    }

    /// Emit a pending pi send-initiated (send_path=pi, no terminal) into the
    /// target's log and return (env, target, delivery_log text, content_sha256).
    fn seed_pending_pi_send(
        home: &std::path::Path,
        uuid: &str,
        send_id: &str,
        message: &str,
    ) -> (dispatch::effects::MapEnv, dispatch::model::Session, String, String) {
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.to_string_lossy().to_string());
        let env = dispatch::effects::MapEnv { vars, uid: 501 };
        let state_dir =
            dispatch::paths::QdPaths::from_home_env(home, &env).state_dir;
        let target = pi_target(uuid);
        let content_sha256 = dispatch::events::sha256_hex(message.as_bytes());
        let writer = dispatch::events::EventWriter::for_key(
            &state_dir,
            uuid,
            Some(uuid.to_string()),
            target.name.clone(),
        );
        // A daemon-lane send-initiated on the pi lane, no recovery keys.
        writer
            .emit(
                &RealClock,
                &dispatch::events::Payload::SendInitiated {
                    send_id: send_id.to_string(),
                    verb: "send:relay".to_string(),
                    send_path: "pi".to_string(),
                    content_sha256: content_sha256.clone(),
                    content_len: message.as_bytes().len() as u64,
                    chunks: 1,
                    chunk_sha256s: vec![content_sha256.clone()],
                    chunk_sha256s_capped: false,
                    transcript: None,
                    transcript_offset: None,
                    content_preview: None,
                },
            )
            .unwrap();
        let delivery_log = std::fs::read_to_string(dispatch::events::events_path(&state_dir, uuid))
            .unwrap();
        (env, target, delivery_log, content_sha256)
    }

    #[test]
    fn pi_observer_content_keyed_landing_emits_message_seen() {
        let home = tempfile::tempdir().unwrap();
        let uuid = "cccccccc-1111-2222-3333-444444444444";
        let msg = "steer: land this exact text mid-turn";
        let (env, target, delivery_log, sha) =
            seed_pending_pi_send(home.path(), uuid, "turn-9", msg);

        // The sent bytes ARE present as a user record → landed → message-seen.
        let rollout = pi_rollout(msg, true);
        let n = emit_pi_seen_for_landed(&env, &target, &delivery_log, &rollout);
        assert_eq!(n, 1, "one landed pi send → one message-seen");

        let state_dir = dispatch::paths::QdPaths::from_home_env(home.path(), &env).state_dir;
        let raw = std::fs::read_to_string(dispatch::events::events_path(&state_dir, uuid)).unwrap();
        let ms: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("message-seen")).unwrap()).unwrap();
        assert_eq!(ms["event"], "message-seen");
        assert_eq!(ms["send_id"], "turn-9", "keyed to the send's turn id");
        assert_eq!(ms["content_sha256"].as_str().unwrap(), sha);
        assert!(
            dispatch::events::is_terminal("message-seen"),
            "message-seen is the terminal success"
        );
    }

    #[test]
    fn pi_observer_no_terminal_when_content_absent_or_only_echoed() {
        let home = tempfile::tempdir().unwrap();
        let uuid = "dddddddd-1111-2222-3333-555555555555";
        let msg = "steer: this must not false-land";
        let (env, target, delivery_log, _sha) =
            seed_pending_pi_send(home.path(), uuid, "turn-10", msg);

        // NO user record (not flushed / not landed) — even though the ASSISTANT
        // echoes the exact text. A steer's landing is a USER record, never an
        // assistant echo (the false-positive guard) → NO message-seen (PENDING).
        let rollout = pi_rollout(msg, false);
        let n = emit_pi_seen_for_landed(&env, &target, &delivery_log, &rollout);
        assert_eq!(n, 0, "assistant echo / absent user record is NOT landed");

        let state_dir = dispatch::paths::QdPaths::from_home_env(home.path(), &env).state_dir;
        let raw = std::fs::read_to_string(dispatch::events::events_path(&state_dir, uuid)).unwrap();
        assert!(
            !raw.contains("message-seen"),
            "no success terminal until the SENT bytes land as a user record"
        );
        // And the pure content-keyed check agrees (shared pi-provider matcher).
        use dispatch::provider::pi::floor::rollout_landed;
        assert!(rollout_landed(&pi_rollout(msg, true), &_sha), "present user record → landed");
        assert!(!rollout_landed(&pi_rollout(msg, false), &_sha), "absent user record → not landed");
    }

    #[test]
    fn pi_observer_is_idempotent_never_double_terminal() {
        let home = tempfile::tempdir().unwrap();
        let uuid = "eeeeeeee-1111-2222-3333-666666666666";
        let msg = "steer: land once";
        let (env, target, _log0, _sha) =
            seed_pending_pi_send(home.path(), uuid, "turn-11", msg);
        let rollout = pi_rollout(msg, true);
        let state_dir = dispatch::paths::QdPaths::from_home_env(home.path(), &env).state_dir;

        // First observe → emits one message-seen.
        let log1 = std::fs::read_to_string(dispatch::events::events_path(&state_dir, uuid)).unwrap();
        assert_eq!(emit_pi_seen_for_landed(&env, &target, &log1, &rollout), 1);
        // Second observe re-reads the (now-terminated) log → first-terminal-wins
        // skips it → NO second message-seen.
        let log2 = std::fs::read_to_string(dispatch::events::events_path(&state_dir, uuid)).unwrap();
        assert_eq!(
            emit_pi_seen_for_landed(&env, &target, &log2, &rollout),
            0,
            "a send that already has a terminal is never re-terminated (first-terminal-wins)"
        );
    }

    // =======================================================================
    // C3/C5 — the ACP variant-exact delivery-terminal mapping (D2 obligation
    // (b)), ASSERTED against the SETTLED classification (never re-derived), +
    // the emission/correlation/idempotence. All hermetic, no model.
    // =======================================================================

    #[test]
    fn acp_delivery_terminal_mapping_is_variant_exact() {
        use dispatch::provider::acp::{StopReason, TerminalReason as R};
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
        use dispatch::provider::acp::{AcpTurnCompletion, AcpTurnObservation, StopReason};
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

    fn acp_target(uuid: &str) -> dispatch::model::Session {
        let mut s = pi_target(uuid);
        s.name = Some("acp-obs-1".to_string());
        s.provider = "acp/claude-code".to_string();
        s
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
        let sha = dispatch::events::sha256_hex(message.as_bytes());
        let writer =
            dispatch::events::EventWriter::for_key(state_dir, uuid, Some(uuid.to_string()), name);
        writer
            .emit(
                &RealClock,
                &dispatch::events::Payload::SendInitiated {
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
        use dispatch::provider::acp::TerminalReason;
        let home = tempfile::tempdir().unwrap();
        let uuid = "ffffffff-1111-2222-3333-777777777777";
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.path().to_string_lossy().to_string());
        let env = dispatch::effects::MapEnv { vars, uid: 501 };
        let paths = dispatch::paths::QdPaths::from_home_env(home.path(), &env);
        let target = acp_target(uuid);
        let name = target.name.clone();

        // A completed turn → message-seen, content_sha256 read by send_id-join.
        let sha = emit_pending_si(&paths.state_dir, uuid, name.clone(), "turn-42", "acp body", "acp/claude-code");
        emit_acp_terminal(&env, &target, "turn-42", TerminalReason::Completed);
        let raw = std::fs::read_to_string(dispatch::events::events_path(&paths.state_dir, uuid)).unwrap();
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
        let raw2 = std::fs::read_to_string(dispatch::events::events_path(&paths.state_dir, uuid)).unwrap();
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
        let raw3 = std::fs::read_to_string(dispatch::events::events_path(&paths.state_dir, uuid)).unwrap();
        assert!(
            !raw3.contains("seen-failed"),
            "R6 degrade: a readable-but-absent Cancelled is RECOVERABLE, never a hard seen-failed"
        );
        assert!(
            dispatch::events::first_terminal_for(
                &dispatch::events::parse_events(&raw3).records,
                "turn-43"
            )
            .is_none(),
            "turn-43 stays non-terminal (recoverable) after the degraded Cancelled"
        );

        // A landed turn with NO correlated send-initiated emits nothing.
        emit_acp_terminal(&env, &target, "turn-nonexistent", TerminalReason::Completed);
        let raw4 = std::fs::read_to_string(dispatch::events::events_path(&paths.state_dir, uuid)).unwrap();
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
        reason: dispatch::provider::acp::TerminalReason,
        uuid: &str,
        prompt: &str,
        landed_prompt: Option<&str>,
    ) -> Option<(String, Option<String>)> {
        let home = tempfile::tempdir().unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.path().to_string_lossy().to_string());
        let env = dispatch::effects::MapEnv { vars, uid: 501 };
        let paths = dispatch::paths::QdPaths::from_home_env(home.path(), &env);
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
            std::fs::read_to_string(dispatch::events::events_path(&paths.state_dir, uuid)).unwrap();
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
        use dispatch::provider::acp::TerminalReason as R;
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
            // prompt is absent). R6 DEGRADE (write-ordering unprovable on brano):
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
        use dispatch::provider::acp::{AcpTurnCompletion, AcpTurnObservation, StopReason, TerminalReason as R};
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
        let env = dispatch::effects::MapEnv { vars, uid: 501 };

        let uuid = "99999999-0000-0000-0000-000000000077";
        let msg = "acp turn under a QD_HOME override";
        // The QD_HOME-honoring delivery log (where the send phases + terminal MUST land).
        let qd_state = dispatch::paths::QdPaths::from_home_env(&home, &env).state_dir;
        // The from_home (QD_HOME-IGNORANT) log a regressed terminal would orphan into.
        let bad_state = dispatch::paths::QdPaths::from_home(&home).state_dir;
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
            dispatch::provider::acp::TerminalReason::Completed,
        );

        // The terminal joins its send phase in the QD_HOME log by send_id.
        let qd_log =
            std::fs::read_to_string(dispatch::events::events_path(&qd_state, uuid)).unwrap();
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
        let bad_log = dispatch::events::events_path(&bad_state, uuid);
        assert!(
            !bad_log.exists()
                || !std::fs::read_to_string(&bad_log).unwrap().contains("message-seen"),
            "the terminal must NOT land in the QD_HOME-ignorant from_home log (the delivery-lie)"
        );
    }
}
