//! `qd wait` — resolve a session, ask its lane whether it has gone idle, and
//! render the answer.
//!
//! # What this file is now
//!
//! It used to be the four per-provider wait loops. Those are
//! [`quorum_qw::idle`], reached through
//! [`LaneOps::await_idle`](quorum_qw::contract::LaneOps::await_idle) — ruling D2,
//! `doc/tbd/provider-architecture/11-stage3-plan.md`. Two of them WROTE
//! `message-seen` into qw's delivery log from qd's process, which made `qd wait`
//! the last shared-file path across the split; moving them retired the last of
//! `ledger_gate`'s `wait.rs` debt.
//!
//! What is left is what only a verb can do: address a session by a human query,
//! print, and exit.
//!
//! # The four things that deliberately did NOT move
//!
//! 1. **The codex entry gate.** It reads the JOIN's `session.status` — the
//!    cross-backend gather, which is qd's and which a `SessionId` cannot
//!    reconstruct — and it answers on stdout without opening a socket. Moving it
//!    would mean either handing qw the join or having qw connect where today it
//!    does not.
//! 2. **Every line naming the label**, except the progress prefix. The claude and
//!    codex idle lines go to STDOUT, and on the wire qw's stdout is the protocol —
//!    it structurally cannot print them.
//! 3. **`verify_post_resume_if_marked`**, the ACP bridge-continuation check. It
//!    runs after the ` done` below and its verdict REPLACES the exit code, which
//!    is a verb decision.
//! 4. **The A6 `invoked` telemetry line**, which is per-VERB.
//!
//! # Why the renderer keys on the lane
//!
//! `qd wait`'s wording is not uniform across providers and never was: claude and
//! codex print `<label> is idle` on stdout, ACP and pi print `<label> is idle.`
//! (with a period) on stderr, ACP's entry-idle emits no telemetry where pi's does,
//! and the unreachable-daemon line names the harness. Those are pinned bytes. So
//! the renderer matches on `(lane, TurnState)` — the same four arms the verb has
//! always had, now over a typed answer instead of an exit code.

use clap::ArgMatches;

use dispatch::effects::{RealClock, RealEnv};
use dispatch::fmt::truncate_id_default;
use dispatch::model::SessionStatus;

use quorum_qw::contract::{LaneError, SessionId, TurnState};
use quorum_qw::lane::{Harness, Lane, Mode};

use super::common;

/// Append a content-free A6 invoked line for a SUCCESSFUL `wait` (exit 0). Best-
/// effort: a failure warns but NEVER changes the verb's exit code (spec §4.1).
fn invoked_wait(session_id: &str, name: Option<&str>) {
    if let Err(e) =
        dispatch::telemetry::append_invoked(&RealEnv, &RealClock, "wait", Some(session_id), name)
    {
        eprintln!("qd wait: telemetry invoked append failed (non-fatal): {e}");
    }
}

/// The lane an UNPLACEABLE row waits in — a row whose provider id `lane_for`
/// cannot place at all.
///
/// This is not a default and not a guess: it is the pre-existing contract written
/// down. `qd wait` is DELIBERATELY UNARMED (codex-p1-spec section 2.3 — it has no
/// `refuse_unknown_provider`), so before the lanes existed such a row fell through
/// the three string arms into `run_claude_wait` and waited status-only. The claude
/// pane lane IS that body, so naming it here preserves the behaviour exactly.
///
/// Written as the struct literal (the same form [`Lane::ALL`] uses) rather than
/// `Lane::from_id("claude-code/mux-pane").expect("a real lane")`: the id string was
/// re-parsed on every unknown-provider wait only to be `expect`-ed back into the
/// value it always was, and an `expect` on a fallback path is a panic waiting for
/// the day someone renames a hosting token. `(ClaudeCode, Pane)` is total — it is
/// the harness's only mode — so the constant needs no refusal at all.
const UNPLACEABLE_ROW_LANE: Lane = Lane {
    harness: Harness::ClaudeCode,
    mode: Mode::Pane,
};

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
    // opened. This is THE gate that stays in the verb: `session.status` for a codex
    // row is `inputs.codex_status_for`, folded by the gather from every rollout in
    // one pass, and qw has no such map — `row_for_id` would re-read the registry
    // string, which for codex is not the source of truth at all.
    //
    // codex-interactive: this arm serves BOTH codex topologies unchanged, and
    // deliberately does not branch on hosting. The entry gate reads the join's
    // codex status, which is derived CONNECTIONLESSLY from the rollout tail for
    // every codex row (join.rs `codex_status_for`) — a TUI session writes that
    // same rollout, so its busy/idle is as live as a daemon thread's.
    if session.provider == "codex" && session.status == SessionStatus::Idle {
        println!("{label} is idle");
        invoked_wait(&session.session_id, session.name.as_deref());
        return 0;
    }

    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // THE ROW'S LANE. This replaces the four `session.provider` arms verbatim:
    // `lane_for` keys on the same provider id, and the four `await_idle` bodies
    // route on the HARNESS alone, so codex/pi keep serving both their topologies
    // exactly as the string arms did.
    //
    // An UNKNOWN provider degrades to [`UNPLACEABLE_ROW_LANE`], which is not a
    // fallback but the pre-existing contract restated — see that constant. Refusing
    // here instead would be a NEW error surface on a verb that has never had one (L8).
    let lane = quorum_qw::lane_for(&session.provider, session.hosting.as_deref())
        .unwrap_or(UNPLACEABLE_ROW_LANE);

    let ops = dispatch::lane::open(lane, &env, paths.clone());
    // The contract's budget is unsigned wall-clock ms, and 0 means "no bound given"
    // — which is what a missing or unparseable `--timeout` already parsed to, and
    // what every one of the four bodies already read as their own default. A
    // NEGATIVE `--timeout` clamps to 0 and keeps its old meaning too: the two poll
    // loops test `elapsed < timeout_ms` and exit immediately either way.
    let state = ops.await_idle(&SessionId(session.session_id.clone()), timeout_ms.max(0) as u64);

    match state {
        // The watcher found the session already idle — it never opened a progress
        // line, so the whole line is ours.
        // Keyed on the LANE, not the harness. It was `match lane.harness` while
        // ACP was one, and `Harness::ClaudeCode` then meant the pane lane and
        // nothing else; it now covers both claude lanes, whose entry-idle
        // reporting differs in all three respects below. The ACP arm must come
        // from the MODE or claude's pane wording would swallow its own bridge.
        Ok(TurnState::IdleAtEntry) => match (lane.harness, lane.mode) {
            // stdout, no period. Pinned by `verbs_a4::wait_idle_session_reports_idle_exit_0`.
            (Harness::ClaudeCode, Mode::Pane) | (Harness::Codex, _) => {
                println!("{label} is idle");
                invoked_wait(&session.session_id, session.name.as_deref());
                0
            }
            // stderr, WITH a period, and — deliberately — NO telemetry. The ACP
            // entry-idle arm has never appended an `invoked` line; that asymmetry
            // with pi is preserved rather than tidied, because tidying it would
            // change what `qd telemetry` reports for an existing workflow.
            (_, Mode::Acp) => {
                eprintln!("{label} is idle.");
                0
            }
            (Harness::Pi, _) => {
                eprintln!("{label} is idle.");
                invoked_wait(&session.session_id, session.name.as_deref());
                0
            }
            // Not lanes; unreachable through `Lane::new`. Reported on stderr
            // without telemetry — the conservative half of the split above.
            (Harness::ClaudeCode, Mode::Daemon | Mode::Extension | Mode::AppServer)
            | (Harness::Opencode, Mode::Pane | Mode::Daemon | Mode::Extension | Mode::AppServer) => {
                eprintln!("{label} is idle.");
                0
            }
        },
        // The watcher waited, so a progress line is open on stderr and these words
        // close it.
        Ok(TurnState::WentIdle) => {
            eprintln!(" done");
            // FINDING #2 PART 2 — VERIFY-THE-BRIDGE (cold-path, one-time): if this is
            // the FIRST wait after a resume (a marker exists for the row's pid),
            // confirm from PRIMARY source that the post-resume turn CONTINUED the SAME
            // bridge JSONL — a fork-on-load is FAILED LOUD. Gated on the marker so a
            // normal wait pays only one cheap stat; a non-resume wait does nothing.
            // ACP-only, as it always was: the marker is written by the ACP resume.
            if lane.mode == Mode::Acp {
                if let Some(code) = verify_post_resume_if_marked(&paths, session) {
                    return code; // fork detected → fail loud; else proceed.
                }
            }
            invoked_wait(&session.session_id, session.name.as_deref());
            0
        }
        // D8: the budget was the CALLER's, and nothing was stamped on the way here.
        // `Undetermined` shares this arm because it says the same thing this line
        // says — no verdict — and reading it as either "idle" or "still busy" would
        // be inventing one. Both exit 1: a watcher that cannot report idleness has
        // not seen idleness, and `qd wait X && next` must not proceed.
        Ok(TurnState::BudgetElapsed) | Ok(TurnState::Undetermined { .. }) => {
            eprintln!(" timeout");
            1
        }
        Ok(TurnState::SessionExited) => {
            eprintln!(" session exited");
            1
        }
        Ok(TurnState::ChannelClosed) => {
            eprintln!(" channel closed");
            1
        }
        Ok(TurnState::TurnFailed { detail }) => {
            eprintln!(" failed: {detail}");
            1
        }
        // There is no live process to wait on. The lane says which of its two
        // refusals this is by the VARIANT, never by a string — see
        // `quorum_qw::idle`'s `no_pid`/`unreachable`.
        //
        // The `qd wait: ` prefix is NEW (2026-08). This was the one refusal on
        // this verb that named no command — the `Transport` arm below and the
        // generic `Err(e)` arm under it have always carried it — so a user who
        // ran `qd wait X && next` and got this line back had nothing on it
        // saying which of the two commands wrote it. Only the prefix is added;
        // the sentence, and the fact that this arm does NOT quote the label
        // (its wording predates the label-quoting siblings), are unchanged.
        Err(LaneError::Cold { .. }) => {
            eprintln!("qd wait: Session has no PID (cold/dead). Nothing to wait for.");
            1
        }
        // The row exists but its daemon could not be reached, identified, or
        // connected to. The harness word is the verb's, because the line names a
        // recovery act the user types.
        //
        // A wire-level `Transport` (a missing or unanswering `qw`) also lands here.
        // For an acp/pi row that reads as "the daemon is not reachable", which is
        // imprecise about the cause but correct about the state and about the
        // recovery; for the other lanes it falls to the honest generic arm below,
        // because they never produce this variant themselves.
        Err(LaneError::Transport { .. })
            if lane.mode == Mode::Acp || lane.harness == Harness::Pi =>
        {
            let word = if lane.harness == Harness::Pi {
                "pi"
            } else {
                "acp"
            };
            eprintln!(
                "qd wait: \"{label}\": {word} session daemon not reachable (try qd resume {label})."
            );
            1
        }
        // Everything a lane can refuse with that this verb has no wording of its
        // own for. Unreachable through the four bodies today — they answer only the
        // two above — so this is the honest handling of a refusal that could only
        // come from the boundary itself (no `qw`, a version mismatch, an
        // unreadable frame). Printed rather than swallowed: a `qd wait` that exits
        // 1 in silence is the failure mode this whole seam was built to avoid.
        Err(e) => {
            eprintln!("qd wait: \"{label}\": {e}");
            1
        }
    }
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

#[cfg(test)]
mod tests {
    // ================= R2-1 — marker pid-reuse aliasing (the two seams) =================
    //
    // These stayed with `verify_post_resume_for`, which stayed in the verb: its
    // verdict REPLACES `qd wait`'s exit code, and an exit code is not a lane's to
    // choose. The other twenty-two tests in this module moved with their functions
    // into `quorum_qw::idle` and `quorum_qw::delivery::pi`.
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
}
