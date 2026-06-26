//! REAL `sb wait` backend (spec §3.2; wart-wave W6+W7 content keying, ADD-15) —
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
/// silently reverts `sb wait` to a disk-as-status read, exactly the bug class
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
        eprintln!("sb wait: telemetry usage append failed (non-fatal): {e}");
    }
}

/// `sb wait <session>` — block until a session goes busy→idle. Port of
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

    let sessions = match common::all_sessions(dispatch::join::JoinOpts::default()) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let session = match common::resolve_or_die(query, &sessions) {
        Ok(s) => s,
        Err(code) => return code,
    };

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

    // --- claude path ---------------------------------------------------------
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
        let dir = dispatch::sbmux_dir::resolve_sbmux_dir(&paths.home, &env).ok()?;
        let socket_path = sbmux::server::session_socket_path_for(Some(&dir), name).ok()?;
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
        eprintln!("sb wait: no transcript found for this session — status-keyed only");
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
    // seam above — flipping `sb wait`'s control path off disk. `None` (no subscriber)
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
            name: "sb-manager".to_string(),
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

fn run_acp_wait(session: &dispatch::model::Session, label: &str, timeout_ms: i64) -> i32 {
    use dispatch::provider::acp::{
        derive_tier, AcpClient, AcpConnection, AcpTurnCompletion, AcpTurnObservation, TerminalReason,
        Tier,
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
        eprintln!("sb wait: \"{label}\": acp session daemon not reachable (try sb resume {label}).");
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
    let transport_field = entry.as_ref().and_then(|e| e.transport.clone());

    // S6 identity + S7 ladder (same gate as the SEND path — connect-success is liveness,
    // cmdline+pid is identity; drive only on Tier::Acp).
    let cmdline = dispatch::create_daemon::real_cmdline_probe(pid);
    let endpoint_alive = endpoint.is_some()
        && dispatch::effects::is_pid_alive(pid as i32)
        && dispatch::acp_residence::cmdline_is_our_acp_daemon(cmdline.as_deref(), endpoint.as_deref());
    if derive_tier("acp/claude-code", transport_field.as_deref(), endpoint_alive) != Tier::Acp {
        return cold(label);
    }
    let endpoint = endpoint.expect("Tier::Acp implies a live endpoint");

    let conn = match AcpConnection::connect(&endpoint, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(_) => return cold(label),
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
        let obs = acp_wait_observation(conn.next_update(remaining));
        let observer = OneShotObserver(obs.clone());
        let completion = AcpTurnCompletion::new(&observer);
        match completion.poll_completion() {
            // Visible = the turn is DONE. The SC-2a reason decides done(0) vs failed(1):
            // a JSON-RPC-error terminal is `Failed` (an operator scripting `qd wait X &&
            // next` must not proceed on a failed turn); every other terminal is a clean
            // completion (end_turn / cancelled / max — all exit 0, as before).
            TurnCompletionProbe::Visible => {
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
            // Transport gone before any terminal → source integrity lost → exit 1.
            TurnCompletionProbe::Degraded(_) => {
                eprintln!(" channel closed");
                return 1;
            }
        }
    }
}

/// FINDING #2 PART 2 — the cold-path VERIFY-THE-BRIDGE consumer. Returns `Some(exit)`
/// ONLY on a detected fork (fail loud, nonzero); `None` to proceed (Continued, or an
/// Unconfirmed that emits a LOUD degraded-confidence warning but does not fail the turn).
/// One-time: the marker is consumed (removed) whatever the verdict.
fn verify_post_resume_if_marked(
    paths: &dispatch::paths::SbPaths,
    session: &dispatch::model::Session,
) -> Option<i32> {
    use dispatch::resume_daemon::{
        read_resume_verify_marker, resume_verify_marker_path, verify_post_resume_continuation,
        ResumeContinuation,
    };
    let pid = session.pid.filter(|&p| p != 0)?;
    let marker_path = resume_verify_marker_path(&paths.sessions_dir, pid);
    let marker = read_resume_verify_marker(&marker_path)?; // absent → normal wait (one stat).
    // Bounded retry for the JSONL flush lag (eventual-consistency vs the wire terminal).
    let verdict = verify_post_resume_continuation(&paths.projects_dir, &marker, 8, 250);
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
    paths: &'a dispatch::paths::SbPaths,
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
        acp_client: None,    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch::wait::{ChannelStatus, WaitContentDeps};
    use std::cell::Cell;

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
}
