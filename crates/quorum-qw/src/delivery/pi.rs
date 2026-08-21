//! The `pi_daemon` carrier — the pi resident, floor sub-lane included.
//!
//! WS-A.5 pi residence SEND path. The pi analog of [`super::acp::send_acp`]:
//! re-read the row's recorded `endpoint`, verify the resident pi-daemon's IDENTITY
//! (pid alive AND the live `/proc` cmdline carries our `pi-daemon --listen
//! <endpoint>` marker — defeats PID reuse: a connect-success is liveness, the
//! cmdline + recorded endpoint is identity), connect a fresh short-lived
//! `PiRemote` to the resident ws front, and drive `PiProvider::inject`.
//!
//! `inject` mints a live turn via `prompt{streamingBehavior:"steer"}` — a single
//! call that starts a fresh turn when the resident is idle and steers the open turn
//! when busy (no per-turn believed-state read; contrast the codex `expectedTurnId`
//! ladder). Events do NOT return through this client (`PiRemote::next_event` is
//! `Ok(None)` by design — the resident routes pi's stream into the registry sink);
//! a caller wanting the turn OUTCOME reads the registry/transcript, not the send
//! reply. A dead/wrong-identity endpoint or a failed connect DEGRADES to the SEND
//! "not reachable" surface — never a hang on a dead endpoint, never a fake.
//!
//! Nothing here prints and nothing here exits; see [`super`].

use std::path::PathBuf;
use std::time::Duration;

use crate::delivery::{
    append_send_invoked, emit_daemon_seen, emit_daemon_send_events, emit_door_failure,
    CarrierError, CarrierResult, Delivered, Notes, Refused, SendParams,
};
use crate::effects::{Clock, Env};
use crate::model::Session;
use crate::paths::QdPaths;

/// How long the SEND seam waits for a pi prompt to appear in the rollout before
/// reporting the send as PENDING. Bounded and short: this is a confirmation
/// window, not a wait — a message that has not been written as a user record by
/// now is one the resident has not taken up, and saying so beats a silent
/// turn-accepted. `qd wait` remains the unbounded resolver.
pub const PI_LANDING_WINDOW: Duration = Duration::from_millis(2000);
const PI_LANDING_POLL: Duration = Duration::from_millis(100);

/// The pi carrier's injected effects. Adds the one thing a library cannot own
/// that the DEAD-ONLY floor sub-lane needs: the process cwd.
pub struct PiSendDeps<'a> {
    pub env: &'a dyn Env,
    pub paths: &'a QdPaths,
    pub clock: &'a dyn Clock,
    /// The floor's one-shot child runs here when the row records no cwd of its
    /// own. The caller resolves it (`std::env::current_dir()`), because a library
    /// reading the process cwd is a hidden input.
    pub fallback_cwd: PathBuf,
}

/// Why the pi carrier could not deliver. DELIBERATELY NO `Display` — see
/// [`CarrierError`]. Every variant is `qd <verb>:`-attributed.
#[derive(Debug)]
pub enum PiSendError {
    /// No live identity-verified resident to reach, or a failed ws connect.
    NotReachable { name: String },
    /// A refused/timed-out prompt (PA9: the sink stays idle) — the resident was
    /// live and said no.
    InjectFailed { name: String, detail: String },
    /// The dead-only floor could not create its dedicated `--session-dir`.
    FloorSetupFailed { name: String, detail: String },
    /// The dead-only floor's one-shot turn failed to deliver.
    FloorFailed { name: String, detail: String },
}

impl CarrierError for PiSendError {
    fn line(&self, verb: &str) -> Option<String> {
        Some(match self {
            PiSendError::NotReachable { name } => format!(
                "qd {verb}: \"{name}\": pi session daemon not reachable (try qd resume {name})"
            ),
            PiSendError::InjectFailed { name, detail } => {
                format!("qd {verb}: \"{name}\": send failed ({detail}).")
            }
            PiSendError::FloorSetupFailed { name, detail } => {
                format!("qd {verb}: \"{name}\": pi floor could not create session dir: {detail}")
            }
            PiSendError::FloorFailed { name, detail } => {
                format!("qd {verb}: \"{name}\": pi floor delivery failed ({detail}).")
            }
        })
    }

    fn exit_code(&self) -> i32 {
        1
    }
}

/// Deliver `message` into a pi resident's turn, or — when the resident is
/// PROVABLY dead — through the structured floor sub-lane.
///
/// Answers the minted turn id, which [`emit_daemon_send_events`] already writes as
/// `Payload::SendInitiated.send_id`. The dead-only floor sub-lane
/// ([`send_pi_floor`]) is keyed too — it uses the CALLER's `SendParams::send_id`,
/// because it has no resident turn to borrow one from — so BOTH pi sub-lanes
/// answer with an id and neither is silently id-less.
pub fn send_pi(deps: &PiSendDeps<'_>, params: &SendParams<'_>) -> CarrierResult<PiSendError> {
    use crate::provider::pi::residence::cmdline_is_our_pi_daemon;
    use crate::provider::pi::{PiProvider, PiRemote, PiRpc};
    use crate::provider::{InjectError, Provider, ProviderFx, SessionKey};

    let session = params.session;
    let message = params.message;
    let name = session.name.clone().unwrap_or_default();
    let mut notes = Notes::new();

    // §C1 (door-inventory B3, resident door) — record-then-fail-loud: every
    // pi-door "not reachable" refusal leaves a `send-failed` account (best-effort,
    // keyed to the target session, `send_id` omitted pre-wire) BEFORE the refusal.
    // `reason` names the surface; the emit never alters the refusal.
    let not_reachable = |reason: &str| {
        emit_door_failure(deps.env, deps.clock, &name, Some(session), message, reason);
        PiSendError::NotReachable { name: name.clone() }
    };

    // Identity + liveness fence (residence S6): a connect-success is liveness only — the
    // live cmdline carrying OUR `pi-daemon --listen <endpoint>` marker (+ pid alive + a
    // recorded endpoint) is the identity guard against PID reuse.
    let pid = session.pid.filter(|&p| p != 0);
    // The endpoint lives in the registry row (re-read by pid; NOT on the --json surface).
    let endpoint = pid
        .and_then(|pid| crate::registry::read_entry(&deps.paths.sessions_dir, pid))
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty());
    let (pid_alive, cmdline_is_ours) = match pid {
        Some(pid) => {
            let cmdline = crate::create_daemon::real_cmdline_probe(pid);
            (
                crate::effects::is_pid_alive(pid as i32),
                cmdline_is_our_pi_daemon(cmdline.as_deref(), endpoint.as_deref()),
            )
        }
        None => (false, false),
    };

    // A6.1 DEAD-ONLY floor (super-22 acceptance condition 8): when the native rpc
    // resident is PROVABLY DEAD/GONE (no pid / pid dead / wrong-identity cmdline /
    // missing endpoint — the S6 "not reachable" branch) DROP to the structured
    // `-p --mode json` floor instead of erroring. A LIVE identity-verified resident
    // NEVER floors — even the failed/slow ws connect below stays "not reachable" (the
    // rpc retry/steer surface), because a one-shot `-c --session-dir` concurrent with
    // the resident's OPEN session JSONL would race/corrupt it (single-writer safety).
    // dead ⇒ floor + reuse-dir; alive ⇒ never floor.
    if crate::provider::pi::floor::floor_may_fire(
        pid.is_some(),
        pid_alive,
        cmdline_is_ours,
        endpoint.is_some(),
    ) {
        return send_pi_floor(deps, params);
    }
    let endpoint = endpoint.expect("live identity-verified resident implies a recorded endpoint");

    // Connect a fresh short-lived remote to the resident ws front (the AcpConnection
    // fail-fast discipline — a contended connect fails fast rather than hanging).
    let remote = match PiRemote::connect(&endpoint, Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => return Err(not_reachable("daemon-unreachable").into()),
    };
    let rpc_ref: &dyn PiRpc = &remote;
    let fx = ProviderFx {
        await_relay: None,
        env: deps.env,
        paths: deps.paths,
        socket_dir: deps.paths.sessions_dir.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: Some(rpc_ref),
        acp_pre_dispatch: None,
    };
    let key = SessionKey {
        id: &session.session_id,
        name: session.name.as_deref(),
        cwd: session.cwd.as_deref(),
        pid: session.pid,
    };
    // punch R10: `PiProvider::inject` SPENDS this — a pi prompt is plain text, so
    // the identity is rendered into the message as the `<channel source="qd" …>`
    // envelope (`provider::shared::attribution`) unless it is `"cli"`. The ledger
    // and the landing check below are unmoved: both key sha256 of the RAW
    // `message`, and `floor::rollout_landed` un-wraps the rollout record.
    let from = super::derive_from_session(deps.env);
    // PRE-inject busy read, for the DISPOSITION of the post-send landing check
    // below — NOT for routing (inject does its own, later, read: see
    // `PiProvider::inject`). A busy resident means inject will route `FollowUp`,
    // i.e. the message is QUEUED behind the open turn and legitimately lands
    // minutes from now — polling the rollout for it would burn the window and
    // prove nothing. An unreadable probe is treated as idle (we poll, and a
    // no-show is reported as pending, which is the honest answer either way).
    let queued = remote.get_state().map(|st| st.is_streaming);
    let queued_behind_open_turn = queued.unwrap_or(false);
    let result = PiProvider.inject(&fx, &key, message, &from);
    // Best-effort close of our short-lived client (the resident daemon stays up).
    let _ = remote.close();
    match result {
        Ok(turn_id) => {
            // C5/C3: sent + delivered (turn-accepted) into the TARGET's log on the
            // inject ACK — NON-terminal.
            emit_daemon_send_events(
                deps.env,
                deps.clock,
                &name,
                Some(session),
                message,
                &turn_id,
                &session.provider,
            );
            // The terminal used to be reachable ONLY through the wait observer, so
            // a plain fire-and-forget send left a turn-accepted with no terminal
            // and nothing to contradict it — a dropped message was indistinguishable
            // from a delivered one. Close the loop HERE, bounded, for the send that
            // is expected to land NOW.
            if queued_behind_open_turn {
                notes.push(format!(
                    "qd send:relay: \"{name}\": queued behind the open turn (follow-up); \
                     delivery stays PENDING until it runs (qd wait {name} resolves it)."
                ));
            } else if confirm_landing(
                deps.env,
                deps.clock,
                deps.paths,
                session,
                message,
                PI_LANDING_WINDOW,
            ) {
                notes.push(format!(
                    "qd send:relay: \"{name}\": landed (message-seen)."
                ));
            } else {
                // NO terminal — the send stays recoverable. Never a hard "didn't
                // land": the rollout write may simply be slower than the window.
                notes.push(format!(
                    "qd send:relay: \"{name}\": accepted, but not yet present in the \
                     rollout after {}ms — delivery PENDING, not confirmed.",
                    PI_LANDING_WINDOW.as_millis()
                ));
            }
            notes.extend(append_send_invoked(deps.env, deps.clock, &name));
            Ok(Delivered {
                stdout: Some(turn_id.clone()),
                message_id: turn_id,
                notes,
            })
        }
        Err(InjectError::Precondition(s)) => {
            // §C1: record the door failure before refusing.
            emit_door_failure(
                deps.env,
                deps.clock,
                &name,
                Some(session),
                message,
                "inject-failed",
            );
            Err(Refused {
                notes,
                error: PiSendError::InjectFailed {
                    name: name.clone(),
                    detail: s,
                },
                message_id: None,
            })
        }
        Err(_) => Err(Refused {
            notes,
            error: not_reachable("daemon-unreachable"),
            message_id: None,
        }),
    }
}

/// A6.1 pi structured-floor SEND — the DEAD-ONLY fallback lane. Reached from
/// [`send_pi`] ONLY when the native rpc resident is provably dead (the S6
/// identity+liveness fence failed; [`crate::provider::pi::floor::floor_may_fire`]).
/// Delivers the turn via a ONE-SHOT `pi -p --mode json -c --session-dir <dedicated>`
/// child and captures the outcome SCRAPE-FREE from the `turn_end`/`agent_end` ndjson
/// (see [`crate::provider::pi::floor`]). Continuity = `-c` + one dedicated
/// per-qd-session `--session-dir` (turn-2 resumes turn-1's single appended session
/// file). The drop is OBSERVABLE (a drop-log NOTE — a degradation is never silent;
/// pi's own floor doctrine). pi's OWN settings cred (openai-codex, `~/.pi/agent`) is
/// inherited — NEVER a `--provider`/credential override or swap.
///
/// The floor does NOT echo an id on stdout ([`Delivered::stdout`] is `None`): it is
/// a synchronous one-shot that reports on stderr, and the id it carries is one it
/// minted for the ledger rather than one a resident handed back.
///
/// ── ONE ORDERING CHANGED, AND IT IS RECORDED HERE ───────────────────────────
/// The drop-log line used to be `eprintln!`d BEFORE `run_floor_turn` spawned the
/// `pi` child, and that child INHERITS stderr (`floor.rs`: `.stderr(Stdio::inherit())`).
/// As a note it is printed by the caller AFTER this function returns, so on a
/// floor turn whose child writes to stderr the drop log now trails the child's
/// output instead of leading it. The line itself, its wording, the exit codes and
/// every ledger record are unchanged, and the doctrine the line serves — "a
/// degradation is never silent" — is about the line EXISTING, not about where it
/// sits relative to a child's diagnostics. Restoring the lead would mean handing
/// the core a print sink, which is the callback phase 3B exists to delete.
pub fn send_pi_floor(
    deps: &PiSendDeps<'_>,
    params: &SendParams<'_>,
) -> CarrierResult<PiSendError> {
    use crate::provider::pi::{floor, PiProvider};
    use crate::provider::Provider;

    let session = params.session;
    let message = params.message;
    let name = session.name.clone().unwrap_or_default();
    let mut notes = Notes::new();

    // pi binary: the QD_PI_BIN override (pi is NOT on PATH in quorum boxes) else "pi".
    let pi_bin = deps
        .env
        .var("QD_PI_BIN")
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "pi".to_string());

    // pi's sessions root off env ONLY (PI_CODING_AGENT_SESSION_DIR else
    // $HOME/.pi/agent/sessions) — reuse the provider's public resolver via a minimal
    // env-only fx (transcript_root reads fx.env, never paths).
    let fx = resolve_fx(deps.env, deps.paths);
    let sessions_root = PiProvider.transcript_root(&fx);
    // One dedicated floor dir per qd-session, isolated from the rpc resident's
    // sessions so `-c`'s most-recent pick is unambiguous + turn-2 resumes turn-1.
    let session_dir = floor::floor_session_dir(&sessions_root, &session.session_id);
    if let Err(e) = std::fs::create_dir_all(&session_dir) {
        // §C1 (door-inventory B4, dead-only floor door) — record then refuse.
        emit_door_failure(
            deps.env,
            deps.clock,
            &name,
            Some(session),
            message,
            "floor-setup-failed",
        );
        return Err(Refused {
            notes,
            error: PiSendError::FloorSetupFailed {
                name,
                detail: e.to_string(),
            },
            message_id: None,
        });
    }
    let continue_session = floor::dir_has_session(&session_dir);
    let cwd = session
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| deps.fallback_cwd.clone());

    // Observable degrade (never silent — pi's floor doctrine; the Child-B ACP
    // ladder's analogous drop-log was retired with that floor in Child D).
    notes.push(floor::floor_drop_log_line(
        &name,
        "native rpc resident not identity-verified",
    ));

    // punch R10 — the floor is a pi SUB-lane, not a different lane: a delegated
    // task must arrive attributed whichever sub-lane carried it, or the sender a
    // peer sees would depend on whether the resident happened to be alive. The
    // floor reaches no `Provider::inject` (it spawns a one-shot `pi -p` child), so
    // it derives `from` and renders the SAME envelope here — the one
    // implementation, with the same "no envelope for `cli`" rule. The ledger below
    // still keys sha256 of the RAW `message`; `floor::rollout_landed` un-wraps the
    // appended session record before it hashes, so the content-keyed terminal is
    // unaffected.
    let from = super::derive_from_session(deps.env);
    let wire = crate::provider::shared::attribution::attribute(message, &from);
    let turn = floor::FloorTurn {
        pi_bin: &pi_bin,
        session_dir: &session_dir,
        cwd: &cwd,
        prompt: &wire,
        continue_session,
    };
    match floor::run_floor_turn(&turn, floor::DEFAULT_FLOOR_TIMEOUT) {
        Ok(outcome) => {
            // C5/C3 (obligation (c), the DEAD-ONLY floor sub-lane): emit the three
            // phases into the target's log. The floor is a SYNCHRONOUS one-shot, so
            // sent + delivered + the success TERMINAL all resolve here — but the
            // terminal is CONTENT-KEYED against the APPENDED session record (the
            // floor's own observation seam; its `stopReason` readback is NOT the
            // resident observer). A minted send_id anchors all three (the floor has
            // no resident turn id). Best-effort; never alters the exit.
            let content_sha256 = crate::events::sha256_hex(message.as_bytes());
            // The caller's id (`SendParams::send_id`), not a fresh mint: qd wrote
            // its intent record against it before this call. Same shape, same
            // opacity — see [`crate::contract::Message::id`].
            let send_id = params.send_id.to_string();
            emit_daemon_send_events(
                deps.env,
                deps.clock,
                &name,
                Some(session),
                message,
                &send_id,
                "pi",
            );
            if floor::rollout_landed(&floor_session_jsonl(&session_dir), &content_sha256) {
                emit_daemon_seen(
                    deps.env,
                    deps.clock,
                    &name,
                    Some(session),
                    &send_id,
                    &content_sha256,
                );
            }
            // Best-effort structured delivery: the turn landed and was captured
            // scrape-free. The outcome persists in the appended session file (the
            // transcript-read analog of the async rpc send). Confirm delivery on
            // stderr; SEND-vocabulary only.
            notes.push(format!(
                "qd send:relay: \"{name}\": delivered via pi structured floor (stopReason={}).",
                outcome.stop_reason.as_deref().unwrap_or("?")
            ));
            notes.extend(append_send_invoked(deps.env, deps.clock, &name));
            Ok(Delivered {
                message_id: send_id,
                stdout: None,
                notes,
            })
        }
        Err(e) => {
            // §C1: the dead-only floor turn failed to deliver — record then refuse.
            emit_door_failure(
                deps.env,
                deps.clock,
                &name,
                Some(session),
                message,
                "floor-failed",
            );
            Err(Refused {
                notes,
                error: PiSendError::FloorFailed {
                    name,
                    detail: e.to_string(),
                },
                message_id: None,
            })
        }
    }
}

/// Concatenate every `*.jsonl` in the pi floor's dedicated `--session-dir` (pi
/// writes ONE appended session file there under `-c`). The floor content-keys THIS
/// (the appended record) for its success terminal — not the stdout, which need not
/// carry the user prompt record.
pub fn floor_session_jsonl(session_dir: &std::path::Path) -> String {
    let mut out = String::new();
    let Ok(entries) = std::fs::read_dir(session_dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            if let Ok(s) = std::fs::read_to_string(&p) {
                out.push_str(&s);
                out.push('\n');
            }
        }
    }
    out
}

/// A minimal `ProviderFx` for resolving pi's env-only `transcript_root`
/// (`PI_CODING_AGENT_SESSION_DIR`/$HOME — never paths). Borrow lifetimes are
/// bounded by the caller's `env`/`paths`.
fn resolve_fx<'a>(env: &'a dyn Env, paths: &'a QdPaths) -> crate::provider::ProviderFx<'a> {
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

// ===========================================================================
// C5/C3 — the pi CONTENT-KEYED rollout OBSERVER (D2 obligation (c))
// ===========================================================================
//
// A resident pi send is FIRE-AND-FORGET: `send_pi` emits sent (send-initiated) +
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
//
// The content-keyed LANDING test (obligation (c) point 1) lives in the pi provider
// (`crate::provider::pi::floor::rollout_landed`) — the home of pi session-file
// format knowledge — so the RESIDENT observer here and the DEAD-ONLY floor
// (`send_pi_floor`) share ONE matcher. It matches a USER-turn record's text
// against the send's content_sha256; a steer's text lands as its own user record,
// and an assistant echo is never a landing.
//
// ── ONE OBSERVER, TWO SEAMS (the twin is retired) ──────────────────────────
// `bin/qd/verbs/wait.rs` used to carry a field-for-field COPY of
// `pending_landed_sends` / `observe_landed_sends`, because the wait arm that drove
// it was a qd verb function. Ruling D2's `await_idle` moved that arm into
// `crate::idle`, and the copy is DELETED rather than left in step by hand: both
// seams — the SEND seam's post-inject `confirm_landing` and the WAIT seam's
// at-turn-close release in `idle::await_idle_pi` — now call THIS function.
// `send_and_wait_seams_never_double_emit` (in `bin/qd/verbs/send_relay.rs`) still
// drives both, and its subject survives the convergence: it no longer asks whether
// two implementations agree, it asks whether two SEAMS firing over the same landed
// send produce exactly one terminal. First-terminal-wins is what makes that true,
// and it is a property of the ledger rather than of the copy that is gone.

/// The pending pi sends whose content has LANDED — pure over the delivery-log
/// records + the rollout. A pending pi send = a `send-initiated` on the pi lane
/// (send_path=="pi") with NO terminal yet (first-terminal-wins); it has LANDED when
/// its content_sha256 is present as a user-turn record in the rollout. Returns
/// (send_id, content_sha256) for each — exactly one message-seen apiece.
fn pending_landed_sends(
    records: &[crate::events::EventRecord],
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
        if crate::events::first_terminal_for(records, &sid).is_some() {
            continue;
        }
        let Some(sha) = rec.str_field("content_sha256") else {
            continue;
        };
        if crate::provider::pi::floor::rollout_landed(rollout_jsonl, &sha) {
            out.push((sid, sha));
        }
    }
    out
}

/// Drive the pi content-keyed observer: read the resident's rollout jsonl
/// (READ-ONLY, via the row's recorded `jsonl_path`) + the target's delivery log,
/// then emit message-seen for every pending pi send whose content LANDED.
/// Best-effort; never changes the caller's exit. Resident lane only.
///
/// Emission is idempotent across seams: [`pending_landed_sends`] skips any send
/// that already has a terminal (first-terminal-wins).
pub fn observe_landed_sends(
    env: &dyn Env,
    clock: &dyn Clock,
    paths: &QdPaths,
    session: &Session,
) -> usize {
    let Some(rollout_path) = session
        .jsonl_path
        .as_ref()
        .map(PathBuf::from)
        .filter(|p| p.exists())
    else {
        return 0;
    };
    let Ok(rollout_jsonl) = std::fs::read_to_string(&rollout_path) else {
        return 0;
    };
    let delivery_log = std::fs::read_to_string(crate::events::events_path(
        &paths.state_dir,
        &session.session_id,
    ))
    .unwrap_or_default();
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return 0;
    };
    let state_dir = QdPaths::from_home_env(std::path::Path::new(&home), env).state_dir;
    let records = crate::events::parse_events(&delivery_log).records;
    let landed = pending_landed_sends(&records, &rollout_jsonl);
    if landed.is_empty() {
        return 0;
    }
    let writer = crate::events::EventWriter::for_key(
        &state_dir,
        &session.session_id,
        Some(session.session_id.clone()),
        session.name.clone(),
    );
    for (send_id, content_sha256) in &landed {
        crate::events::warn_emit(
            &writer,
            clock,
            &crate::events::Payload::MessageSeen {
                send_id: send_id.clone(),
                content_sha256: content_sha256.clone(),
            },
        );
    }
    landed.len()
}

/// The SEND-seam half of the pi content-keyed observer: poll the resident's
/// rollout for at most `window` for the sent bytes as a USER record
/// (content-keyed on the send's `content_sha256`, the same key the wait observer
/// and the dead-only floor sub-lane match on). On a hit, drive
/// [`observe_landed_sends`] to emit the `message-seen` TERMINAL into the target's
/// log and return true.
///
/// Returns false — and emits NOTHING — when the window closes with no record.
/// That is deliberately NOT a `seen-failed`: an un-landed-yet send is
/// indistinguishable from a slow rollout write, so it stays PENDING/recoverable,
/// per the "never claim didn't-land" rule the recovery keys are built on.
pub fn confirm_landing(
    env: &dyn Env,
    clock: &dyn Clock,
    paths: &QdPaths,
    session: &Session,
    message: &str,
    window: Duration,
) -> bool {
    let Some(rollout_path) = session
        .jsonl_path
        .as_ref()
        .map(PathBuf::from)
        .filter(|p| p.exists())
    else {
        return false;
    };
    let content_sha256 = crate::events::sha256_hex(message.as_bytes());
    let deadline = std::time::Instant::now() + window;
    loop {
        let rollout = std::fs::read_to_string(&rollout_path).unwrap_or_default();
        if crate::provider::pi::floor::rollout_landed(&rollout, &content_sha256) {
            observe_landed_sends(env, clock, paths, session);
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(PI_LANDING_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{MapEnv, RealClock};
    use crate::events::sha256_hex;
    use crate::model::{SessionBranch, SessionStatus};

    fn blank_session() -> Session {
        Session {
            name: None,
            user_named: None,
            session_id: String::new(),
            code: None,
            qd_id: None,
            pid: None,
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
            provider: String::new(),
            entrypoint: None,
            lineage: None,
            hosting: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    /// D2 §C5/C3 (obligation (c), the DEAD-ONLY floor sub-lane) — the floor's
    /// success terminal is CONTENT-KEYED against the APPENDED session record:
    /// [`floor_session_jsonl`] reads the dedicated `--session-dir`, the shared
    /// `rollout_landed` confirms the sent bytes as a user record, and
    /// [`emit_daemon_seen`] appends the message-seen terminal. Hermetic — the live
    /// floor drive needs pi/OAuth (deferred).
    #[test]
    fn floor_content_keyed_seen_from_appended_session_record() {
        let home = tempfile::tempdir().unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.path().to_string_lossy().to_string());
        let env = MapEnv { vars, uid: 501 };
        let uuid = "abababab-cdcd-efef-0101-232323232323";
        let msg = "floor prompt that must land";
        let target = Session {
            name: Some("pi-floor-1".to_string()),
            session_id: uuid.to_string(),
            provider: "pi".to_string(),
            ..blank_session()
        };

        // A dedicated floor `--session-dir` holding the appended user record.
        let session_dir = home.path().join("floordir");
        std::fs::create_dir_all(&session_dir).unwrap();
        let user = serde_json::json!({
            "type": "message",
            "message": {"role": "user", "content": [{"type": "text", "text": msg}]}
        });
        std::fs::write(
            session_dir.join("sess.jsonl"),
            format!("{}\n", serde_json::to_string(&user).unwrap()),
        )
        .unwrap();

        let jsonl = floor_session_jsonl(&session_dir);
        let sha = sha256_hex(msg.as_bytes());
        assert!(
            crate::provider::pi::floor::rollout_landed(&jsonl, &sha),
            "the appended user record makes the send content-keyed LANDED"
        );

        emit_daemon_seen(
            &env,
            &RealClock,
            "pi-floor-1",
            Some(&target),
            "floor-send-1",
            &sha,
        );
        let state_dir = QdPaths::from_home_env(home.path(), &env).state_dir;
        let raw = std::fs::read_to_string(crate::events::events_path(&state_dir, uuid)).unwrap();
        let ms: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("message-seen")).unwrap())
                .unwrap();
        assert_eq!(ms["event"], "message-seen");
        assert_eq!(ms["send_id"], "floor-send-1");
        assert_eq!(ms["content_sha256"].as_str().unwrap(), sha);
    }

    // === The SEND-seam landing check (the busy-drop fix, part 3) ===============

    /// Build a pi target whose rollout is `<home>/rollout.jsonl`, with `rollout`
    /// as its contents, and a `send-initiated` already in its delivery log for
    /// `msg` (what [`emit_daemon_send_events`] writes on the inject ACK).
    fn pi_landing_fixture(
        home: &std::path::Path,
        msg: &str,
        rollout: &str,
    ) -> (MapEnv, QdPaths, Session) {
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.to_string_lossy().to_string());
        let env = MapEnv { vars, uid: 501 };
        let paths = QdPaths::from_home_env(home, &env);
        let rollout_path = home.join("rollout.jsonl");
        std::fs::write(&rollout_path, rollout).unwrap();
        let target = Session {
            name: Some("pi-resident-1".to_string()),
            session_id: "cdcdcdcd-0101-2323-4545-676767676767".to_string(),
            pid: Some(4242),
            jsonl_path: Some(rollout_path.to_string_lossy().to_string()),
            provider: "pi".to_string(),
            ..blank_session()
        };
        // The inject-ACK records the observer joins against.
        emit_daemon_send_events(
            &env,
            &RealClock,
            "pi-resident-1",
            Some(&target),
            msg,
            "turn-1",
            "pi",
        );
        (env, paths, target)
    }

    fn pi_user_record(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::to_string(&serde_json::json!({
                "type": "message",
                "message": {"role": "user", "content": [{"type": "text", "text": text}]}
            }))
            .unwrap()
        )
    }

    /// [`confirm_landing`] with a ZERO window — the tests exercise the content
    /// key and the emission, never the polling clock.
    fn landed(env: &MapEnv, paths: &QdPaths, target: &Session, msg: &str) -> bool {
        confirm_landing(
            env,
            &RealClock,
            paths,
            target,
            msg,
            Duration::from_millis(0),
        )
    }

    fn terminals_in_log(paths: &QdPaths, uuid: &str) -> Vec<String> {
        let raw = std::fs::read_to_string(crate::events::events_path(&paths.state_dir, uuid))
            .unwrap_or_default();
        raw.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v["event"].as_str().map(str::to_owned))
            .filter(|e| crate::events::is_terminal(e))
            .collect()
    }

    /// THE FIX: a plain fire-and-forget send now reaches its TERMINAL at the send
    /// seam. Before, `message-seen` was reachable only through the wait observer,
    /// so a delivered send and a dropped one left identical logs — a turn-accepted
    /// and nothing else.
    #[test]
    fn send_seam_landing_check_emits_the_terminal() {
        let home = tempfile::tempdir().unwrap();
        let msg = "the message that must land";
        let (env, paths, target) = pi_landing_fixture(home.path(), msg, &pi_user_record(msg));
        assert!(
            terminals_in_log(&paths, &target.session_id).is_empty(),
            "the inject ACK alone is NON-terminal"
        );
        assert!(landed(&env, &paths, &target, msg));
        assert_eq!(
            terminals_in_log(&paths, &target.session_id),
            vec!["message-seen".to_string()]
        );
    }

    /// A send absent from the rollout emits NOTHING — no `seen-failed`, no
    /// terminal of any kind. An un-landed-yet send is indistinguishable from a slow
    /// rollout write, so it stays PENDING/recoverable: the recovery keys never
    /// claim "didn't land."
    #[test]
    fn unlanded_send_emits_no_terminal_ever() {
        let home = tempfile::tempdir().unwrap();
        let msg = "the message that never lands";
        let (env, paths, target) =
            pi_landing_fixture(home.path(), msg, &pi_user_record("some other turn"));
        assert!(!landed(&env, &paths, &target, msg));
        assert!(
            terminals_in_log(&paths, &target.session_id).is_empty(),
            "no terminal — the send stays recoverable, never a hard didn't-land"
        );
    }

    /// An assistant ECHO of the prompt is not a landing (the shared
    /// `rollout_landed` false-positive guard, pinned at THIS seam too).
    #[test]
    fn assistant_echo_is_not_a_landing_at_the_send_seam() {
        let home = tempfile::tempdir().unwrap();
        let msg = "echoed but never accepted";
        let echo = format!(
            "{}\n",
            serde_json::to_string(&serde_json::json!({
                "type": "turn_end",
                "message": {"role": "assistant", "content": [{"type": "text", "text": msg}]}
            }))
            .unwrap()
        );
        let (env, paths, target) = pi_landing_fixture(home.path(), msg, &echo);
        assert!(!landed(&env, &paths, &target, msg));
        assert!(terminals_in_log(&paths, &target.session_id).is_empty());
    }

    /// punch R10, at the SEND seam: an attributed send lands in the rollout as the
    /// WIRE text (sender envelope + body) while the ledger keys sha256 of the RAW
    /// message. The terminal must still fire — the seam un-wraps through
    /// `rollout_landed`. Without it, EVERY delegated pi send (the only kind that
    /// carries a sender) would report PENDING and never reach a terminal, which is
    /// exactly the "delivered and dropped are indistinguishable" state the landing
    /// check exists to end.
    #[test]
    fn an_attributed_send_still_lands_at_the_send_seam() {
        let home = tempfile::tempdir().unwrap();
        let msg = "the delegated task that must land";
        let wire = crate::provider::shared::attribution::attribute(msg, "sess-A").into_owned();
        assert_ne!(wire, msg, "non-vacuity: this send IS enveloped");
        let (env, paths, target) = pi_landing_fixture(home.path(), msg, &pi_user_record(&wire));
        assert!(landed(&env, &paths, &target, msg));
        assert_eq!(
            terminals_in_log(&paths, &target.session_id),
            vec!["message-seen".to_string()]
        );
        // The ledger key is the RAW message on BOTH records — the envelope moved
        // no content key (`sha256(wrapped) != sha256(inner)`, the relay ruling).
        let raw = std::fs::read_to_string(crate::events::events_path(
            &paths.state_dir,
            &target.session_id,
        ))
        .unwrap();
        let want = sha256_hex(msg.as_bytes());
        for line in raw.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(sha) = v["content_sha256"].as_str() {
                assert_eq!(sha, want, "every record keys the RAW message: {line}");
            }
        }
    }

    /// The WAIT seam agrees with the send seam on the attributed form: the
    /// observer reads the content key off the `send-initiated` record (the RAW
    /// message) and finds it under the envelope in the rollout. One key, two
    /// seams, first-terminal-wins.
    #[test]
    fn an_attributed_send_lands_at_the_wait_seam_too() {
        let home = tempfile::tempdir().unwrap();
        let msg = "the delegated task the wait seam resolves";
        let wire = crate::provider::shared::attribution::attribute(msg, "sess-A").into_owned();
        let (env, paths, target) = pi_landing_fixture(home.path(), msg, &pi_user_record(&wire));
        assert_eq!(observe_landed_sends(&env, &RealClock, &paths, &target), 1);
        assert_eq!(
            terminals_in_log(&paths, &target.session_id),
            vec!["message-seen".to_string()]
        );
    }

    /// A row with no recorded rollout path cannot be content-keyed: false, and no
    /// terminal. Never a panic, never an invented landing.
    #[test]
    fn missing_rollout_path_is_pending_not_landed() {
        let home = tempfile::tempdir().unwrap();
        let msg = "no rollout on the row";
        let (env, paths, mut target) = pi_landing_fixture(home.path(), msg, &pi_user_record(msg));
        target.jsonl_path = None;
        assert!(!landed(&env, &paths, &target, msg));
        assert!(terminals_in_log(&paths, &target.session_id).is_empty());
    }

    // === The WAIT-seam observer (D2 obligation (c)) ============================
    //
    // MOVED from `bin/qd/verbs/wait.rs`, where they drove that file's copy of this
    // function through a text-in/text-out wrapper (`emit_pi_seen_for_landed`). The
    // copy is gone, so they drive `observe_landed_sends` itself — the entry point
    // `idle::await_idle_pi` calls at every RELEASE. Subjects unchanged: the
    // content-keyed landing, the assistant-echo false-positive guard, and
    // first-terminal-wins idempotence.
    //
    // Hermetic (cred-free, no model): a fixture rollout + delivery log drive the
    // real emission. The full live steer-into-open-turn cell is deferred on pi
    // OAuth; the observer's CORRECTNESS holds here.

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

    /// Jail a pi target whose rollout is `<home>/rollout.jsonl` holding `rollout`,
    /// with ONE pending `send-initiated` on the pi lane (send_path=="pi", no
    /// terminal) already in its delivery log. Returns (env, paths, target, sha).
    fn seed_pending_pi_send(
        home: &std::path::Path,
        uuid: &str,
        send_id: &str,
        message: &str,
        rollout: &str,
    ) -> (MapEnv, QdPaths, Session, String) {
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.to_string_lossy().to_string());
        let env = MapEnv { vars, uid: 501 };
        let paths = QdPaths::from_home_env(home, &env);
        let rollout_path = home.join("rollout.jsonl");
        std::fs::write(&rollout_path, rollout).unwrap();
        let target = Session {
            name: Some("pi-obs-1".to_string()),
            session_id: uuid.to_string(),
            pid: Some(9191),
            jsonl_path: Some(rollout_path.to_string_lossy().to_string()),
            provider: "pi".to_string(),
            ..blank_session()
        };
        let content_sha256 = sha256_hex(message.as_bytes());
        let writer = crate::events::EventWriter::for_key(
            &paths.state_dir,
            uuid,
            Some(uuid.to_string()),
            target.name.clone(),
        );
        // A daemon-lane send-initiated on the pi lane, no recovery keys.
        writer
            .emit(
                &RealClock,
                &crate::events::Payload::SendInitiated {
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
        (env, paths, target, content_sha256)
    }

    #[test]
    fn pi_observer_content_keyed_landing_emits_message_seen() {
        let home = tempfile::tempdir().unwrap();
        let uuid = "cccccccc-1111-2222-3333-444444444444";
        let msg = "steer: land this exact text mid-turn";
        // The sent bytes ARE present as a user record → landed → message-seen.
        let (env, paths, target, sha) =
            seed_pending_pi_send(home.path(), uuid, "turn-9", msg, &pi_rollout(msg, true));

        let n = observe_landed_sends(&env, &RealClock, &paths, &target);
        assert_eq!(n, 1, "one landed pi send → one message-seen");

        let raw =
            std::fs::read_to_string(crate::events::events_path(&paths.state_dir, uuid)).unwrap();
        let ms: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("message-seen")).unwrap())
                .unwrap();
        assert_eq!(ms["event"], "message-seen");
        assert_eq!(ms["send_id"], "turn-9", "keyed to the send's turn id");
        assert_eq!(ms["content_sha256"].as_str().unwrap(), sha);
        assert!(
            crate::events::is_terminal("message-seen"),
            "message-seen is the terminal success"
        );
    }

    #[test]
    fn pi_observer_no_terminal_when_content_absent_or_only_echoed() {
        let home = tempfile::tempdir().unwrap();
        let uuid = "dddddddd-1111-2222-3333-555555555555";
        let msg = "steer: this must not false-land";
        // NO user record (not flushed / not landed) — even though the ASSISTANT
        // echoes the exact text. A steer's landing is a USER record, never an
        // assistant echo (the false-positive guard) → NO message-seen (PENDING).
        let (env, paths, target, sha) =
            seed_pending_pi_send(home.path(), uuid, "turn-10", msg, &pi_rollout(msg, false));

        let n = observe_landed_sends(&env, &RealClock, &paths, &target);
        assert_eq!(n, 0, "assistant echo / absent user record is NOT landed");

        let raw =
            std::fs::read_to_string(crate::events::events_path(&paths.state_dir, uuid)).unwrap();
        assert!(
            !raw.contains("message-seen"),
            "no success terminal until the SENT bytes land as a user record"
        );
        // And the pure content-keyed check agrees (shared pi-provider matcher).
        use crate::provider::pi::floor::rollout_landed;
        assert!(
            rollout_landed(&pi_rollout(msg, true), &sha),
            "present user record → landed"
        );
        assert!(
            !rollout_landed(&pi_rollout(msg, false), &sha),
            "absent user record → not landed"
        );
    }

    #[test]
    fn pi_observer_is_idempotent_never_double_terminal() {
        let home = tempfile::tempdir().unwrap();
        let uuid = "eeeeeeee-1111-2222-3333-666666666666";
        let msg = "steer: land once";
        let (env, paths, target, _sha) =
            seed_pending_pi_send(home.path(), uuid, "turn-11", msg, &pi_rollout(msg, true));

        // First observe → emits one message-seen.
        assert_eq!(observe_landed_sends(&env, &RealClock, &paths, &target), 1);
        // Second observe re-reads the (now-terminated) log → first-terminal-wins
        // skips it → NO second message-seen.
        assert_eq!(
            observe_landed_sends(&env, &RealClock, &paths, &target),
            0,
            "a send that already has a terminal is never re-terminated (first-terminal-wins)"
        );
    }
}
