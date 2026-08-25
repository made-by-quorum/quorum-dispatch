//! REAL `qd resume` backend (spec §5.3; TS `commands/lifecycle.ts:408-530`).
//!
//! Relaunch a COLD session under the embedded mux. The pure preflight deciders
//! live in `dispatch::resume`; this verb drives the live effects:
//!   - OC refusal (server-managed) → must-be-cold,
//!   - F3 cwd reality-check (clean error, never raw ENOENT),
//!   - F1 env-file capture (the launch.rs mechanism) + S2 mux-name validation,
//!   - kill a stale same-name mux session (destructive sub-step),
//!   - launch: --no-attach detached + ready-wait / default attach.
//!
//! The claude relaunch flag is `--resume <session-id>` exactly as TS's
//! `buildClaudeCmd(["--resume", session.sessionId])` does at the resume call-site
//! (lifecycle.ts:474). Ready-wait keys on the PID-file/busy EVENT (ADR 0005 —
//! zero blind keystrokes), reusing the A2 EventBootWaiter. Exit inherits the
//! child / 1 on a preflight error.
//!
//! # The lane
//!
//! The verb resolves the target with qd's fuzzy resolver, derives the
//! [`Lane`](quorum_qw::lane::Lane) from the row's provider + hosting, and revives
//! through [`LaneOps::wake`] — the shape `qd attach` established. That replaced
//! FIVE guarded provider if-chains dispatching into SIX revive routes (claude
//! pane, codex pane, codex daemon, pi pane, pi daemon, and the two acp lanes'
//! shared core), whose relative ORDER was load-bearing and enforced by comment.
//!
//! # What deliberately stays here
//!
//! - **Every success and failure line**, including the `qd send:relay` pointers.
//!   Six routes collapsed; eight lines did not, and must not — each names a
//!   different drive channel and they are pinned as prose.
//! - **The claude PANE lane's preflights**: the id-collision pair, the
//!   must-be-cold gate, the "no session ID" refusal and the F3 cwd reality-check.
//!   They were reachable only from the claude pane arm and they stay scoped to it
//!   — `claude-code/acp` is a resident and takes the daemon path, as it did when
//!   it was spelled `acp/claude-code`.
//! - **The codex / pi PANE preconditions**, ahead of the lane, because their
//!   position relative to dep resolution is user-visible (see the call).
//! - The `ColdJsonl` fallback: a claude transcript with no registry row cannot be
//!   addressed by [`SessionId`], and resume is THE verb for one.
//!
//! `run_codex_resume` / `run_pi_resume` / `run_acp_resume` used to be listed here
//! as "no longer reached from `run`, still the seam `send_unified`'s wake revives
//! through". That seam is gone — `qd send` delivers through
//! [`LaneOps::deliver`], which performs its own wake — so the three had no caller
//! left and are DELETED. See the tombstone where they lived.

use std::path::{Path, PathBuf};

use clap::ArgMatches;

use dispatch::effects::{Env, RealClock, RealEnv};
use dispatch::launch::RenderMode;
use dispatch::model::SessionStatus;
use dispatch::paths::QdPaths;
use dispatch::resume::{resolve_resume_cwd, ResumeCwd};
use dispatch::zmx_dir::{legacy_zmx_dirs, resolve_zmx_dir, XdgFamily};

use quorum_qw::contract::{LaneError, LaneOps, SessionId, WakeOutcome, WakeState};
use quorum_qw::lane::{Harness, Lane, Mode, CLAUDE_PANE};

use super::common;
use super::lifecycle;

/// W2 send-pointer: the codex-resume success lines point the agent at the WORKING
/// channel, `qd send:relay` (bare `qd send` is a moved stub; `send:pty` has no pane
/// for a daemon-hosted codex session). Factored so the EXACT pointer is pinned by a
/// unit test (mirrors `resume_boot_unconfirmed_line`).
fn codex_already_running_line(name: &str) -> String {
    format!("session \"{name}\" is running; send to it with: qd send:relay {name} <text>")
}

fn codex_revived_line(name: &str, pid: i64, endpoint: &str) -> String {
    format!(
        "resumed codex session \"{name}\" (daemon pid {pid}, {endpoint}); \
         send to it with: qd send:relay {name} <text>"
    )
}

/// Item 3 RESUME (acp) — the AlreadyRunning no-op line. A genuinely-alive acp row is
/// drivable RIGHT NOW; resume is a success no-op (NO second adapter, ZERO row mutation).
/// Mirrors `codex_already_running_line`; pinned by a unit test.
fn acp_already_running_line(name: &str) -> String {
    format!("session \"{name}\" is already alive; send to it with: qd send {name} <text>")
}

/// Item 3 RESUME (acp) — the revived line. The resident adapter was re-spawned in
/// LOAD mode (real `session/load`, SAME sessionId, the CC conversation continues).
fn acp_revived_line(name: &str, pid: i64, endpoint: &str) -> String {
    format!(
        "resumed acp session \"{name}\" (adapter pid {pid}, {endpoint}); \
         send to it with: qd send {name} <text>"
    )
}

/// WS-A.2 RESUME (pi) — the AlreadyRunning no-op line. Factored for symmetry with
/// the codex/acp pairs above (it was inline in the verb body until the revive core
/// moved to `dispatch::provider::pi::resume`).
///
/// NAMED INCONSISTENCY, PRESERVED DELIBERATELY: the pi pair points at
/// "wait/stop it by name", NOT at `qd send:relay <name> <text>` the way the codex
/// and acp pairs do. That predates this split and is NOT normalised here — a pi
/// resident's drive channel is a separate question from where its revive code
/// lives, and silently rewriting a user-facing line during a code move is exactly
/// the kind of change that goes unnoticed. Change it on purpose or not at all.
fn pi_already_running_line(name: &str) -> String {
    format!("session \"{name}\" is already alive (pi resident live); wait/stop it by name.")
}

/// WS-A.2 RESUME (pi) — the revived line. The resident was re-spawned in LOAD mode
/// on the SAME durable session id (new pid + endpoint, new row). See
/// `pi_already_running_line` on the "wait/stop it by name" pointer.
fn pi_revived_line(name: &str, pid: i64, endpoint: &str) -> String {
    format!("resumed pi session \"{name}\" (daemon pid {pid}, {endpoint}); wait/stop it by name.")
}

/// `qd resume <session>` — cold-session relaunch.
pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    // P4DB Phase A: `--no-attach` is a dropped interactive/PTY escape — it stays
    // PARSE-ACCEPTED in cli.rs so scripted callers don't break, but `qd resume`
    // now revives DETACHED through the shared `revive_claude` seam, so it is
    // inert here. Its two former siblings, `--no-zmx` and `--zmx-name`, were
    // REMOVED from the parser by FTUE punch R1 (zmx retirement): a parked flag
    // that is documented and does nothing is worse than no flag at all.
    // `--alt-screen`/`--inline` (render) ARE consulted: the seam launches a native
    // claude pane whose render mode is a launch-time birth property, resolved via the
    // shared `common::resolve_render_mode` below (identical to `qd attach`/`qd start`).
    let cwd_override = m.get_one::<String>("cwd").map(|s| s.as_str());

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd resume: HOME is not set — cannot resolve the session state dir.");
            return 1;
        }
    };
    let paths = QdPaths::from_home(&home);

    // Resolve through the sealed uncapped entry. D-2 accept-set: resume IS the
    // remedy the "it is stopped — resume it first" message names, so it acts on a
    // tombstone directly (no post-resolve rejection). Uncapped + include_all so a
    // cold / auto-named session far outside the `ls` display cap resolves.
    let session = match common::resolve_session_uncapped(query) {
        Ok(s) => s,
        Err(code) => return code,
    };

    // --- THE LANE, from the ROW's provider + hosting -------------------------
    //
    // ONE call, in place of the five guarded if-chains that used to stand here:
    // codex+Daemon, codex-pane, `starts_with("acp/")`, pi+Daemon, pi-pane. Each
    // returned early into its own revive, and their ORDER — every daemon and TUI
    // arm ahead of `refuse_unknown_provider`, ahead of the id preflights, ahead of
    // the must-be-cold gate — was load-bearing and enforced by comment alone.
    // [`LaneOps::wake`] routes all six revives on `(harness, mode)` itself, so the
    // chain is one call and the routing is a total match rather than an ordering.
    //
    // `None` ⇒ a genuinely unknown provider ⇒ the pre-existing loud refusal, fired
    // from the position it always fired from: after the provider branches, before
    // the collision preflights.
    //
    // NAMED USER-VISIBLE CHANGE — bare `opencode`. `refuse_unknown_provider` waves
    // "opencode" through, and `row_hosting("opencode", None)` answers `Daemon`, so a
    // row carrying that bare provider used to fall PAST every branch above into the
    // CLAUDE arm and be revived by `revive_claude` — a `claude … --resume <ses_…>`
    // argv, the wrong harness against an id it cannot read. `lane_for` accepts
    // `opencode` as the CLI alias it is and resolves it to the acp/opencode DAEMON
    // lane, which is what such a row has always been. In practice these rows come
    // from the opencode store's cold scan (`join.rs`'s opencode branch: no registry
    // record, no pid), so the visible change is that qd now says it cannot revive
    // one in place instead of launching claude at it.
    let Some(lane) = quorum_qw::lane_for(&session.provider, session.hosting.as_deref()) else {
        return common::refuse_unknown_provider("resume", &session).unwrap_or(1);
    };

    // Render mode is a launch-time BIRTH property — flag > `render-default` config >
    // inline, the same `common::resolve_render_mode` `qd attach` and `qd start` use.
    // It is THREADED into `wake`, never defaulted: all three pane arms resolved it
    // correctly before this rewire, and handing the lane `RenderMode::default()`
    // instead would silently discard a user's `--alt-screen` / `--inline` /
    // `render-default` — the exact defect the contract revision that added the
    // parameter exists to prevent. Resolved ONCE, where three arms each resolved
    // their own. The four daemon lanes ignore it: a resident has no pane to build.
    let render = common::resolve_render_mode(m, &env);

    // codex-interactive / pi-interactive: the TWO PANE PRECONDITIONS, kept HERE —
    // a qd-side ORDER pin, the same kind as `qd attach`'s codex viewer.
    //
    // Both cores re-check these, so this is not the gate; what this call pins is the
    // gate's POSITION. `revive_{codex,pi}_tui`'s verb wrappers ran them BEFORE any
    // dep resolution so that a nameless / never-used row says so even when HOME is
    // unset or `QD_MUX` is bogus, and the lane's arms resolve the mux and the socket
    // dirs first. That ordering is user-visible, it predates the lane, and it is
    // this verb's to keep.
    if lane.mode == Mode::Pane {
        let precondition_failure = match lane.harness {
            Harness::Codex => dispatch::provider::codex::pane::revive_preconditions(
                session.name.as_deref(),
                &session.session_id,
            )
            .err()
            .map(|e| (lifecycle::codex_tui_failure_line("resume", &e), e.exit_code())),
            Harness::Pi => dispatch::provider::pi::pane::revive_preconditions(
                session.name.as_deref(),
                &session.session_id,
            )
            .err()
            .map(|e| (lifecycle::pi_tui_failure_line("resume", &e), e.exit_code())),
            // claude's own preconditions are the gates below, and the two ACP
            // harnesses have no pane lane at all.
            _ => None,
        };
        if let Some((line, code)) = precondition_failure {
            eprintln!("{line}");
            return code;
        }
    }

    // --- The CLAUDE PANE LANE's preflights. They stay in qd, and they stay that
    // --- lane's. ---
    //
    // Every gate below was reachable only by a row that had fallen through all five
    // provider branches above — which is to say a claude PANE row (and, until this
    // rewire, a bare-opencode one; see the `lane_for` note). The daemon and TUI
    // revives deliberately ran AHEAD of them, because a daemon-hosted row is
    // revivable from any non-alive state including a tombstoned stop, and a codex /
    // pi pane's own refusals are what its user must hear first. Scoping the block to
    // the claude pane lane keeps every one of those orders exactly as it was, and
    // states the scope that used to be implied by five early returns.
    //
    // `CLAUDE_PANE`, NOT `harness == ClaudeCode`. `claude-code/acp` answers the
    // harness test too, and every gate here would be wrong for it: it is a
    // headless resident, so "already alive … use qd attach instead" points at a
    // terminal it does not have, the must-be-cold gate contradicts the
    // revivable-from-any-non-alive-state rule the daemon lanes are built on, and
    // its `wake` already reports an already-running resident as the success no-op
    // `resumed` renders. The ACP row reached none of this while it was spelled
    // `acp/claude-code` and fell out at the `starts_with("acp/")` branch; the lane
    // remodel must not have walked it in.
    if lane == CLAUDE_PANE {
        // Pete feedback #6 — live-id-collision preflight over the RAW registry. The
        // deduped join collapses two same-id LIVE rows to one (hiding a genuine
        // duplicate-id collision) and can report the survivor Cold via dedup of a stale
        // row. We check the unmerged truth (raw rows + is_pid_alive) BEFORE the must-be-
        // cold gate so a collision is surfaced even when the join reports the survivor
        // busy/idle, and so a Cold-MISREAD of an actually-live session is refused (it
        // would otherwise spawn a SECOND process on the same id — the orchestrator
        // revival-ladder hazard). SHARED with attach via `common::refuse_id_collision`.
        if let Some(code) =
            common::refuse_id_collision("resume", &session.session_id, &paths.sessions_dir)
        {
            return code;
        }
        if let Some(pid) = common::alive_pid_for_id(&paths.sessions_dir, &session.session_id) {
            eprintln!(
                "qd resume: session \"{}\" is already alive (PID {pid}). \
                 Use \"qd attach\" instead.",
                session.name.as_deref().unwrap_or(&session.session_id)
            );
            return 1;
        }

        // Must be cold (lifecycle.ts:437-441). Retained as the byte-stable fast-path +
        // the stale-status edge the pid-based preflight above does not cover (a row with
        // a non-Cold status STRING whose pid is already dead → 0 alive rows above).
        if session.status != SessionStatus::Cold {
            // P0 qafix R3 (orc ruling 2026-06-10): a tombstoned (Killed) row that is
            // NOT resumable (no provider session id, or no transcript) has NOTHING to
            // resume — "still alive" would be a false statement of fact. The gate's
            // LOGIC is unchanged (anything non-Cold refuses, exit 1); only this arm's
            // message states the true condition. Genuinely-alive statuses keep the
            // byte-pinned pointer to the human attach verb.
            // (Join structure note: a killed session WHOSE
            // TRANSCRIPT EXISTS surfaces as a ColdJsonl row — Cold, resumable, never
            // here; the Killed branch emits only sids no transcript row claimed. The
            // jsonl_path guard keeps this arm honest if that ever drifts.)
            if session.status == SessionStatus::Killed
                && (session.session_id.is_empty() || session.jsonl_path.is_none())
            {
                eprintln!(
                    "qd resume: session \"{}\" was stopped and has no resumable transcript — \
                     nothing to resume.",
                    session.name.as_deref().unwrap_or(query)
                );
                return 1;
            }
            eprintln!(
                "Session is still alive (status: {}). Use \"qd attach\" instead.",
                session.status.as_str()
            );
            return 1;
        }

        if session.session_id.is_empty() {
            eprintln!("Cannot resume: no session ID found.");
            return 1;
        }

        // F3: cwd reality-check BEFORE any spawn (lifecycle.ts:451-462). A
        // renamed/deleted project dir → clean actionable error, never raw ENOENT.
        let fallback = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
        let exists = |p: &str| Path::new(p).exists();
        // D3 (headless): the recorded cwd is still validated (refuse a vanished project
        // dir — keep the F3 safety), but the resumed headless session inherits the
        // daemon's cwd; per-session cwd threading into LaunchHeadless is daemon-side
        // (deferred + flagged), so the validated value itself is not consumed here.
        if let ResumeCwd::Error(e) =
            resolve_resume_cwd(session.cwd.as_deref(), cwd_override, &exists, &fallback)
        {
            eprintln!("ERROR: {e}");
            return 1;
        }
    }

    // --- THE REVIVE, through the lane ---------------------------------------
    //
    // `cwd_override` is `qd resume --cwd <dir>` — the F3 escape for a project that
    // moved — and it is THREADED, not dropped: it reaches `plan_claude_revive`,
    // which resolves it against the recorded cwd and carries the result all the way
    // to the detached launch. The other six arms do not consult it (a resident
    // resolves its own cwd; the two TUI revives take the row's recorded one), which
    // is the lane's answer, not this verb's omission.
    let ops = dispatch::lane::open(lane, &env, paths);
    let id = SessionId(session.session_id.clone());
    match ops.wake(&id, render, cwd_override.map(str::to_string)) {
        Ok(out) => resumed(&session, lane, &out),

        // A row qd RESOLVED that the lane cannot ADDRESS. `row_for_id` is
        // registry-keyed — tombstone-aware, but still registry-keyed — while the
        // join also emits `ColdJsonl` rows: a claude transcript with no registry
        // record behind it (a session run outside qd, or one whose row was removed).
        // Resume is THE verb for those; refusing one with "no such session" would be
        // a capability regression, not a cleanup. Same precedent, same guard, same
        // wording as `qd attach`'s arm.
        Err(LaneError::NotFound { .. }) if session.provider == "claude-code" => {
            match revive_claude(&session, cwd_override, render, false, "resume") {
                Ok(handle) => {
                    println!(
                        "Resumed session \"{}\" from {} (detached); attach with \"qd attach {}\".",
                        handle.zmx_name,
                        dispatch::fmt::truncate_id_default(&session.session_id),
                        handle.zmx_name
                    );
                    0
                }
                // revive_claude already printed its own loud `qd resume:` error.
                Err(code) => code,
            }
        }

        // Same shape for a NON-claude row: `revive_claude` builds a `claude …
        // --resume <sid>` argv, so feeding it a foreign session id would launch the
        // wrong harness against an id it cannot read.
        Err(LaneError::NotFound { .. }) => {
            eprintln!(
                "qd resume: \"{}\" is a stopped {} session and qd cannot revive it in \
                 place. Start a fresh one with \"qd start <name> --provider {}\".",
                session.name.as_deref().unwrap_or(&session.session_id),
                session.provider,
                session.provider
            );
            1
        }

        // ATTRIBUTION, and the CORE's exit code. The lane's revives return typed
        // errors and do not print; `self_attributed` says whether `detail` is
        // already a complete line (a resident's `qd resume: …` Display, an
        // `ERROR: …` refusal) or a body this verb must stamp its own name onto —
        // which is exactly what `line("resume")` / `codex_tui_failure_line` /
        // `pi_tui_failure_line` did for the six arms this replaced. `exit_code` is
        // the core's, because every one of those arms returned the core's rather
        // than a flattened 1.
        Err(LaneError::WakeFailed {
            detail,
            exit_code,
            self_attributed,
        }) => {
            if self_attributed {
                eprintln!("{detail}");
            } else {
                eprintln!("qd resume: {detail}");
            }
            exit_code
        }

        // Transport / Refused / the rest: the lane's own words, exit 1.
        Err(e) => {
            eprintln!("qd resume: {e}");
            1
        }
    }
}

/// Report ONE successful revive — the eight success lines, unchanged.
///
/// The six revive ROUTES collapsed into [`LaneOps::wake`]; the lines did not, and
/// must not: each names a different drive channel (`qd send:relay` for codex and
/// acp, "wait/stop it by name" for pi, `qd attach` for the two TUIs) and they are
/// pinned as prose. What the lane made possible is that this is a rendering match
/// on data, with no control flow in it — and that the `AlreadyRunning` / `Revived`
/// verdict is the CORE's, taken at the instant it made the decision, rather than
/// re-derived here by comparing pids.
fn resumed(session: &dispatch::model::Session, lane: Lane, out: &WakeOutcome) -> i32 {
    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    // The pane a PANE revive built. Its `zmx_name` is derived inside the revive
    // plan and is NOT the row's name — it is what the success line and its `qd
    // attach <…>` pointer must both name, which is why the lane reports it.
    let pane = out.pane.as_ref().map(|p| p.zmx_name.clone()).unwrap_or_default();
    match (lane.harness, lane.mode) {
        // `Mode::Pane`, NOT a mode wildcard. claude-code has two lanes now, and
        // the other one is a headless ACP resident whose revive prints the daemon
        // line below — a wildcard here would swallow it and promise an
        // attachable pane that does not exist.
        (Harness::ClaudeCode, Mode::Pane) => {
            println!(
                "Resumed session \"{}\" from {} (detached); attach with \"qd attach {}\".",
                pane,
                dispatch::fmt::truncate_id_default(&session.session_id),
                pane
            );
            0
        }
        (Harness::Codex, Mode::Pane) => {
            println!("Revived codex session \"{pane}\" — attach with \"qd attach {pane}\"");
            0
        }
        (Harness::Pi, Mode::Pane) => {
            println!("Revived pi session \"{pane}\" — attach with \"qd attach {pane}\"");
            0
        }
        // The extension lane names BOTH channels, because it is the one lane
        // that has both: a human attaches to the pane, and the agent drives the
        // same session over its control channel without attaching to anything.
        // A line naming only `qd attach` would read as "this is the pi TUI
        // lane", which is exactly the confusion the lane exists to resolve.
        (Harness::Pi, Mode::Extension) => {
            println!(
                "Revived pi session \"{pane}\" — attach with \"qd attach {pane}\", \
                 or drive it with \"qd send {pane}\""
            );
            0
        }
        // Not lanes; `Lane::new` refuses them. Unreachable except by hand.
        (Harness::Codex, Mode::Extension) => {
            println!("Revived session \"{name}\"");
            0
        }
        (Harness::Codex, Mode::Daemon) => {
            println!("{}", daemon_line(out, &name, codex_already_running_line, codex_revived_line));
            0
        }
        // The one daemon lane with an attach to point at. Everything else about
        // the revive is `codex/daemon`'s, so the verdict line is too — what
        // differs is the follow-on, and telling a user to `qd send` to a session
        // they could also be WATCHING is the whole affordance going unmentioned.
        (Harness::Codex, Mode::AppServer) => {
            println!("{}", daemon_line(out, &name, codex_already_running_line, codex_revived_line));
            println!("Open a terminal on it with \"qd attach {name}\".");
            0
        }
        // pi has no app-server residence; unreachable through `lane_for`. An arm
        // rather than a wildcard so the compiler keeps forcing this decision.
        (Harness::Pi, Mode::AppServer) => {
            eprintln!("qd resume: pi has no app-server residence");
            1
        }
        (Harness::Pi, Mode::Daemon) => {
            println!("{}", daemon_line(out, &name, pi_already_running_line, pi_revived_line));
            0
        }
        (_, Mode::Acp) => {
            println!("{}", daemon_line(out, &name, acp_already_running_line, acp_revived_line));
            0
        }
        // Not lanes; `Lane::new` refuses each, so no resume can reach here.
        // Enumerated rather than wildcarded so a new mode has to be decided.
        (Harness::ClaudeCode, Mode::Daemon | Mode::Extension | Mode::AppServer)
        | (Harness::Opencode, Mode::Pane | Mode::Daemon | Mode::Extension | Mode::AppServer) => {
            eprintln!("qd resume: {} is not a lane this engine can build", lane.id());
            1
        }
    }
}

/// The two-arm daemon line, shared by the three daemon renderers above. The pair of
/// formatters differs per lane; the SHAPE — already-running is a success no-op,
/// revived names the new pid and endpoint — does not.
///
/// A `Revived` with no resident cannot happen (the four daemon cores return both
/// fields with the verdict) and is rendered as the already-running line rather than
/// with an invented pid: a fabricated endpoint in a success line is worse than a
/// slightly-wrong one, and there is nothing honest to print.
fn daemon_line(
    out: &WakeOutcome,
    name: &str,
    already: fn(&str) -> String,
    revived: fn(&str, i64, &str) -> String,
) -> String {
    match (out.state, out.resident.as_ref()) {
        (WakeState::Revived, Some(r)) => revived(name, r.pid, &r.endpoint),
        _ => already(name),
    }
}

/// A revived claude session's attach coordinates (W1 phase 2): the socket dir +
/// zmx name a caller (`attach`) attaches to AFTER [`revive_claude`] brings the
/// session up detached + drivable.
///
/// Re-exported from `dispatch::lanes` rather than redeclared: the lane seam and
/// this verb had two field-for-field identical structs, and the conversions
/// between them were pure noise.
pub use quorum_qw::lanes::ReviveHandle;

/// W1 phase 2 — the SHARED cold→drivable claude revive, callable by `attach` for
/// the human "just works" auto-revive-then-attach path, by `send`'s wake path, by
/// the adoption relaunch and by the lane seam.
///
/// Verb-layer adapter only. The revive itself — the cwd reality-check, the argv
/// build through the provider seam, the D4 same-name guard, the identity mint, the
/// env-file write, the stale-pane clear, the detached launch and the ADR-0005
/// ready-wait — lives in [`dispatch::provider::claude::revive`]. What is here is
/// what a library cannot own: HOME, the process cwd fallback, the mux backend and
/// socket dirs, and stamping the caller's verb onto every failure line.
///
/// THE PHASE ORDER IS THE POINT OF THIS FUNCTION. `plan_claude_revive` runs FIRST,
/// before the backend / dirs / mux are resolved, because its refusals — the
/// same-name guard above all — must be what the user hears about even when
/// `QD_MUX` is also wrong, and because no env file should be written for a launch
/// that guard was going to refuse. That interleave is only expressible here, which
/// is why the core exposes the two phases instead of one call.
///
/// `verb` NAMES THE COMMAND THE USER TYPED, and it is no longer guessed. The
/// pre-split body hard-coded `qd attach:` on its own lines and `qd resume:` on its
/// helpers' lines regardless of caller; `ReviveClaudeError::line(verb)` ends that.
/// See the core's module docs.
pub fn revive_claude(
    session: &dispatch::model::Session,
    cwd_override: Option<&str>,
    render: RenderMode,
    fresh: bool,
    verb: &str,
) -> Result<ReviveHandle, i32> {
    use dispatch::provider::claude::revive::{
        plan_claude_revive, run_claude_revive, ClaudeLaunchDeps, ClaudePlanDeps, ClaudeReviveParams,
    };

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd {verb}: HOME is not set — cannot resolve the session state dir.");
            return Err(1);
        }
    };
    let paths = QdPaths::from_home(&home);
    let ids_path = common::ids_store_path(&env)?;
    let clock = RealClock;
    let fallback_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());

    let plan_deps = ClaudePlanDeps {
        env: &env,
        home: &home,
        paths: &paths,
        ids_path,
        clock: &clock,
        fallback_cwd,
    };
    let params = ClaudeReviveParams {
        session,
        cwd_override,
        render,
        fresh,
    };
    let plan = plan_claude_revive(&plan_deps, &params).map_err(|e| {
        eprintln!("{}", e.line(verb));
        e.exit_code()
    })?;

    // Backend + canonical dir (C1 D2/D3).
    let backend = common::select_backend(&env)?;
    let canonical = match backend {
        dispatch::mux_selector::Backend::Zmx => resolve_zmx_dir(&env),
        dispatch::mux_selector::Backend::Embedded => {
            match dispatch::qrmux_dir::resolve_qrmux_dir(&home, &env) {
                Ok(d) => d,
                Err(msg) => {
                    eprintln!("qd {verb}: {msg}");
                    return Err(1);
                }
            }
        }
    };

    // Kill a stale same-name session so we get a fresh one (lifecycle.ts:500-505).
    let legacy = match backend {
        dispatch::mux_selector::Backend::Zmx => {
            let scan_roots = dispatch::zmx_dir::legacy_scan_roots(&env, Path::new("/tmp"));
            let xdg = XdgFamily::from_env(&env, env.uid());
            legacy_zmx_dirs(env.uid(), &canonical, &scan_roots, Some(&xdg))
        }
        dispatch::mux_selector::Backend::Embedded => Vec::new(),
    };
    let mux_box = common::build_mux(backend, &home, &env)?;
    let mut scan_dirs = vec![canonical.clone()];
    scan_dirs.extend(legacy);

    let launch_deps = ClaudeLaunchDeps {
        mux: mux_box.as_ref(),
        canonical_dir: canonical,
        scan_dirs,
        paths: &paths,
    };
    run_claude_revive(&launch_deps, &clock, &plan).map_err(|e| {
        eprintln!("{}", e.line(verb));
        e.exit_code()
    })
}

// `run_codex_resume`, `run_pi_resume` and `run_acp_resume` — the three
// verb-layer daemon-revive adapters — lived HERE, and are DELETED rather than
// left unused, on the `revive_pi_tui` precedent (`verbs/lifecycle.rs`). `qd
// resume` stopped calling them when `run` moved onto
// [`quorum_qw::contract::LaneOps::wake`]; `send_unified::RealWaker` was their
// last caller, and it is gone with the rest of qd's duplicated routing. The
// revives themselves did not move — `LaneOps::wake`'s three daemon arms drive
// the SAME `dispatch::resume_daemon::resume_codex_real`,
// `dispatch::provider::pi::resume::resume_pi` and
// `dispatch::provider::acp::daemon::resume_acp` cores these wrapped, from the
// one place that now routes them.

#[cfg(test)]
mod tests {
    use super::{
        acp_already_running_line, acp_revived_line, codex_already_running_line,
        codex_revived_line, daemon_line, pi_already_running_line, pi_revived_line,
    };
    use dispatch::launch::launch_env_pairs;
    use quorum_qw::contract::{Resident, SessionHandle, WakeOutcome, WakeState};

    fn outcome(state: WakeState, resident: Option<Resident>) -> WakeOutcome {
        WakeOutcome {
            state,
            handle: SessionHandle {
                id: None,
                qd_id: None,
                pid: None,
                started_at_ms: None,
                socket_dir: None,
                notes: Vec::new(),
            },
            resident,
            pane: None,
        }
    }

    /// The `AlreadyRunning` / `Revived` distinction is the CORE's verdict, taken at
    /// the instant it decided (`pid_alive && endpoint_recorded && cmdline_is_ours`),
    /// and it SURVIVES the rewire onto [`quorum_qw::contract::LaneOps::wake`]. It
    /// cannot be re-derived here — comparing pids is invented logic and re-probing
    /// reimplements the gate and races it — so a lane that flattened both arms into
    /// one answer would make this verb print "resumed …" for a session it never
    /// revived, which is a false statement of fact.
    ///
    /// All three daemon renderers share `daemon_line`, so all three are pinned by
    /// driving it with each lane's own pair of formatters.
    ///
    /// MUTATION EVIDENCE: collapse `daemon_line`'s match to always take the
    /// `revived` arm and the three `already` assertions red; always take `already`
    /// and the three `revived` assertions red — each with the exact line the other
    /// verdict prints.
    #[test]
    fn already_running_and_revived_stay_two_different_answers() {
        let running = outcome(WakeState::AlreadyRunning, None);
        let revived = outcome(
            WakeState::Revived,
            Some(Resident {
                pid: 4242,
                endpoint: "ws://127.0.0.1:18951".to_string(),
            }),
        );
        for (already, revived_line) in [
            (
                codex_already_running_line as fn(&str) -> String,
                codex_revived_line as fn(&str, i64, &str) -> String,
            ),
            (acp_already_running_line, acp_revived_line),
            (pi_already_running_line, pi_revived_line),
        ] {
            assert_eq!(
                daemon_line(&running, "wk", already, revived_line),
                already("wk"),
                "an already-running resident is a success NO-OP; nothing was revived"
            );
            assert_eq!(
                daemon_line(&revived, "wk", already, revived_line),
                revived_line("wk", 4242, "ws://127.0.0.1:18951"),
                "a revive names the NEW pid and endpoint — the core's own answer, \
                 not a re-read of the row"
            );
        }
    }

    /// A `Revived` verdict that carries no resident cannot be produced by any of the
    /// four daemon cores (each returns both fields WITH the verdict). If one ever
    /// did, the line must not carry an invented pid: a fabricated endpoint in a
    /// success line is worse than the conservative already-running wording.
    #[test]
    fn a_residentless_revive_never_invents_a_pid() {
        let line = daemon_line(
            &outcome(WakeState::Revived, None),
            "wk",
            codex_already_running_line,
            codex_revived_line,
        );
        assert_eq!(line, codex_already_running_line("wk"));
        assert!(!line.contains("pid"), "no invented pid: {line}");
    }

    /// P0 wave-2 (spec-w2-env D1 site 2): the resume env-pair set ALWAYS
    /// carries QD_SESSION_ID (last, after the backend pairs) — with and without
    /// a backend capture — so the env file + dot-source prefix are
    /// unconditional on every resume/revive branch. (The pair-set builder is
    /// the hoisted `dispatch::launch::launch_env_pairs`; resume always passes `Some`.)
    #[test]
    fn resume_env_pairs_always_carry_qd_session_id() {
        use dispatch::launch::RenderMode;
        // No backend env captured → the file STILL has the identity export
        // (punch item 7 adds the inline render birth property; item 1 adds the
        // unconditional FORCE birth property — both ahead of the id-last pair).
        let bare = launch_env_pairs(vec![], Some("ab3kx9mq".to_string()), RenderMode::Inline);
        assert_eq!(
            bare,
            vec![
                (
                    dispatch::launch::FORCE_SESSION_PERSISTENCE_KEY.to_string(),
                    "1".to_string()
                ),
                (
                    dispatch::launch::ALT_SCREEN_DISABLE_KEY.to_string(),
                    "1".to_string()
                ),
                ("QD_SESSION_ID".to_string(), "ab3kx9mq".to_string())
            ]
        );
        // The dot-source prefix is non-empty for this set (unconditional).
        let prefix = dispatch::launch::session_env_prefix(
            std::path::Path::new("/jail/home"),
            "wk",
            &bare,
            &[],
        );
        assert!(!prefix.is_empty(), "prefix must be unconditional");
        assert!(prefix.contains("/jail/home/.quorum/dispatch/session-env/wk.env"));

        // With backend pairs: backend FIRST, FORCE in the birth-property band,
        // the id LAST (an --alt-screen resume omits the render var; the id is
        // STILL last, FORCE STILL rides).
        let composed = launch_env_pairs(
            vec![("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string())],
            Some("ab3kx9mq".to_string()),
            RenderMode::AltScreen,
        );
        assert_eq!(composed.len(), 3);
        assert_eq!(composed[0].0, "ANTHROPIC_BASE_URL");
        assert_eq!(composed[1].0, "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE");
        assert_eq!(
            composed[2],
            ("QD_SESSION_ID".to_string(), "ab3kx9mq".to_string())
        );
    }

    /// W2 send-pointer: the codex-resume success lines MUST name `qd send:relay`
    /// (the working agent channel), NOT bare `qd send` (a moved stub) and NOT
    /// `send:pty` (no pane for a daemon-hosted session). `--wait` is NOT mentioned
    /// (codex ignores it). Pinned so a regression to `qd send` reds here.
    #[test]
    fn codex_resume_success_lines_point_at_send_relay() {
        let running = codex_already_running_line("wk");
        assert_eq!(
            running,
            "session \"wk\" is running; send to it with: qd send:relay wk <text>"
        );
        let revived = codex_revived_line("wk", 4242, "ws://127.0.0.1:18951");
        assert_eq!(
            revived,
            "resumed codex session \"wk\" (daemon pid 4242, ws://127.0.0.1:18951); \
             send to it with: qd send:relay wk <text>"
        );
        for line in [&running, &revived] {
            assert!(
                line.contains("qd send:relay wk"),
                "names send:relay: {line}"
            );
            // The bare `qd send <name>` stub must NOT be the pointer.
            assert!(
                !line.contains("send wk"),
                "must not point at bare `qd send`: {line}"
            );
            assert!(
                !line.contains("send:pty"),
                "no send:pty for a daemon: {line}"
            );
            assert!(!line.contains("--wait"), "codex ignores --wait: {line}");
        }
    }

    /// Item 3 (acp) resume success lines name `qd send`, NOT `send:pty` (no pane for
    /// a daemon-hosted session) and NOT `qd send:relay`.
    ///
    /// They named `send:relay` until the lane remodel made that command REFUSE the
    /// rows these lines are printed for. `send_relay.rs` takes its bare-destination
    /// exit before the acp routing arm, and `classify_session` only short-circuits to
    /// `NotApplicable` on `provider != "claude-code"` — which an acp row satisfied
    /// while it was stamped `acp/claude-code` and no longer does, so it is classified
    /// `Bare` and answered with "ask the human to run `qd wrap`". Verified live: a row
    /// created by `qd start --acp` refuses `send:relay` and accepts `qd send`.
    ///
    /// `qd send` is also the surface these lines should have named regardless —
    /// `send:relay` is the hidden compatibility/debug forcing verb (`cli.rs`), and
    /// `qd send` is what routes on the lane. It is what `lifecycle.rs`'s sibling
    /// daemon-hosted refusal says, and what `verbs/common.rs::daemon_redirect` says.
    ///
    /// The codex/pi pair still name `send:relay`: it WORKS for them (their providers
    /// hit the `NotApplicable` arm), so respelling those is a separate change with its
    /// own pinned lines, not fallout of this one.
    #[test]
    fn acp_resume_success_lines_point_at_send() {
        let running = acp_already_running_line("wk");
        assert_eq!(
            running,
            "session \"wk\" is already alive; send to it with: qd send wk <text>"
        );
        let revived = acp_revived_line("wk", 4242, "ws://127.0.0.1:18951");
        assert_eq!(
            revived,
            "resumed acp session \"wk\" (adapter pid 4242, ws://127.0.0.1:18951); \
             send to it with: qd send wk <text>"
        );
        for line in [&running, &revived] {
            assert!(line.contains("qd send wk"), "names send: {line}");
            assert!(
                !line.contains("send:relay"),
                "send:relay is refused for an acp row: {line}"
            );
            assert!(!line.contains("send:pty"), "no send:pty for a daemon: {line}");
            assert!(!line.contains("--wait"), "acp ignores --wait: {line}");
        }
    }

    /// codex P1 W4 (codex-p1-spec section 7.1): the resume argv fragment the verb
    /// now routes through `provider.resume_args(key, fork)` is BYTE-IDENTICAL to the
    /// verb's PRE-REWIRE hand-built `vec!["--resume", session.session_id]`. `qd
    /// resume` always passes `fork=false` (fork is a `qd new` concept, cli.rs:165-
    /// 169 — there is no resume fork flag), so the fragment is exactly `["--resume",
    /// <id>]`. We also pin the fork=true shape to prove the trait carries the
    /// correct `--fork-session` form even though this verb never requests it.
    ///
    /// MUTATION EVIDENCE: a provider drift (reordered/dropped `--resume`, a wrong
    /// fork shape, a mangled id) reds this — the routed fragment compares
    /// token-for-token against the frozen pre-rewire reference.
    #[test]
    fn resume_args_fragment_matches_prerewire() {
        use dispatch::provider::{provider_for, SessionKey};

        let id = "abc-123-resume".to_string();
        let key = SessionKey {
            id: &id,
            name: Some("wk"),
            cwd: Some("/work/proj"),
            pid: Some(4242),
        };
        let provider = provider_for("claude-code").expect("claude-code resolves");

        // The verb's path: fork=false → the EXACT pre-rewire hand-built fragment.
        let prerewire = vec!["--resume".to_string(), id.clone()];
        assert_eq!(
            provider.resume_args(&key, false),
            prerewire,
            "fork=false fragment must equal the pre-rewire ['--resume', id]"
        );

        // fork=true carries the claude `--fork-session` shape (not exercised by
        // this verb, but the trait must keep it for `qd new --fork` parity).
        assert_eq!(
            provider.resume_args(&key, true),
            vec![
                "--resume".to_string(),
                id.clone(),
                "--fork-session".to_string()
            ]
        );
    }

}
