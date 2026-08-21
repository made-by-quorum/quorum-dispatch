//! `qd delivery:recover` — the one-shot, dispatch-only delivery recovery verb (D1,
//! spec §C2). Closes DEAD-DANGLING pty/new-p sends: a `send-initiated` with no
//! terminal whose WRITER incarnation is gone (a sender killed mid-send / a SIGKILL
//! that bypassed the WatchGuard Drop).
//!
//! ## WHERE THE WORK HAPPENS (stage-3 phase 3A, `09-ledger-split.md`)
//! The transcript SEARCH is [`LaneOps::recover`]'s, not this verb's. Recovery-read
//! opens the recipient's transcript and parses it PER-HARNESS — session-artifact
//! access qd must not have — so it lives behind the lane, which resolves that
//! transcript through the row's OWN provider. This verb keeps the three things that
//! are qd's: the sweep enumeration, the liveness fence below, and the report.
//!
//! ## THE LIVENESS FENCE (hard obligation — cycle-3 finding)
//! This verb runs as a SEPARATE process from the original sender, so it MUST NOT
//! ask for a terminal on a send whose writer is still LIVE. Neither
//! `emit_recovery_verdict` (which the lane calls) nor [`LaneOps::recover`] has a
//! dead-writer gate of its own — the only re-check down there is idempotence
//! against a raced-in *terminal*, NOT a live *writer* — so calling `recover` on a
//! live-writer `send-initiated` would append a PREMATURE terminal on a still-LIVE
//! send, violating QS-1 ("the ledger lies") through the very path built to keep it
//! honest. This verb therefore REPLICATES the gate `await_received` holds: it calls
//! [`events::writer_gone_and_stale`] itself and calls the lane ONLY when it returns
//! true.
//! "Dangling" here means DEAD-dangling, never merely unresolved. A live-writer send
//! is left untouched (stays dangling-but-live). Because the verb is a foreign
//! process, `writer_gone_and_stale`'s own-pid short-circuit does NOT fire — a foreign
//! live pid is correctly evaluated via the RF-6 `start_ms` arm.
//!
//! Clauses (b) "the writer incarnation is gone" and (c) "age > `T_ANCHOR_IDLE_MS`"
//! read qd's OWN record and qd's OWN process, so they stay here in full and are
//! evaluated BEFORE any lane call, on qd's own intent log. Only clause (a) "is there
//! a terminal for this send_id" crosses, as [`LaneOps::resolved`] — a pure poll of
//! qw's log that writes nothing.
//!
//! ## THE TWO LOGS (stage-3 phase 3C, `09-ledger-split.md`)
//! The sweep enumerates `<state>/intent/` — qd's OWN records, written before each
//! send crossed the boundary. It used to enumerate `<state>/sessions/`, which is
//! qw's, and that read is the one the ledger split removes (ruling D4). The
//! terminals this verb produces still land in qw's log, because `recover` writes
//! them and `recover` is qw's.
//!
//! ## SCOPE (minimal, per the plan)
//! No flags beyond target selection; no scheduling; no residency. The resident /
//! scheduled recovery runner is the deferred `recovery_coordinator` bin (C6) — NOT
//! touched here. The sweep is restricted to transcript-anchored pty/new-p sends: the
//! sends recovery-read was designed for and that `await_received` already handles.
//! Relay/daemon sends resolve via their recipient-side observers (C5/C6); running
//! recovery-read on a relay `send-initiated` (no transcript, one-shot writer already
//! dead) would manufacture a FALSE `pending-abandoned` — the opposite of honesty.

use std::path::Path;

use clap::ArgMatches;

use dispatch::effects::{Clock, Env, RealClock, RealEnv};
use dispatch::events::{self, EventRecord};
use dispatch::paths::QdPaths;
use quorum_qw::contract::{LaneOps, LedgerAddress, MessageId, SessionId, Terminal};
use quorum_qw::lane::{Harness, Lane, Mode};

/// The lane a swept row falls back to when the registry cannot name one.
///
/// A `send-initiated` record carries no provider — only `(session?, name?)` — so
/// the harness comes from the row's registry entry, and a recovery sweep runs
/// precisely when that row may be gone (the janitor reaps a dead session's row long
/// before its dangling send stops being recoverable). Refusing an unplaceable row
/// would fail the case the verb exists for, so it degrades, exactly as
/// [`quorum_qw::lanes::row_for_id`] degrades an unknown provider string.
///
/// **claude/pane is not a guess about the harness — it is the status quo, spelled
/// out.** Before this rewiring the verb resolved EVERY harness's transcript through
/// `jsonl::find_jsonl_path` over `<home>/.claude/projects`, and claude's
/// `transcript_root`/`transcript_path` are literally that pair. So a row the
/// registry cannot place is answered exactly as it was, while every row it CAN
/// place now routes to its own harness's layout — which is the defect the move
/// closes (a codex rollout or a pi session was silently `source-unavailable`).
///
/// For a BYNAME-only address the fallback is inert, and that is what keeps it from
/// forcing a row through a wrong lane: `recover`'s transcript resolve short-circuits
/// on the absent session id (there is no registry row, so no cwd and no id to key
/// on), so no lane can change the answer. Such a row is left dangling as
/// `Undetermined` — no terminal — exactly as before. The one byname case that DOES
/// resolve is a record carrying its own `transcript` + `transcript_offset`, and that
/// path never consults a provider at all.
const FALLBACK_LANE: Lane = Lane {
    harness: Harness::ClaudeCode,
    mode: Mode::Pane,
};

/// The ledger's OWN reason token for the (b) empty-window verdict, as
/// `quorum_qw::lanes::terminal_from_verdict` restates it. The R6 lattice has six
/// termini and [`Terminal`] has four variants, so the two UNDETERMINED states are
/// told apart by this token — the same string the ledger writes, not a second
/// vocabulary invented at the boundary.
const WINDOW_EMPTY: &str = "window-empty";
/// The (c) disclosed closer's reason — `pending-abandoned{recovery-no-candidate}`.
const RECOVERY_NO_CANDIDATE: &str = "recovery-no-candidate";
/// The (d) disclosed closer's reason — `pending-abandoned{recovery-unattributable}`.
const RECOVERY_UNATTRIBUTABLE: &str = "recovery-unattributable";

/// Per-run recovery outcome (drives the summary + is the testable return value).
#[derive(Debug, Default, PartialEq)]
struct RecoverReport {
    /// pty/new-p send-initiated records considered.
    scanned: usize,
    // Recovered anchors — the content landed (Anchored §6.2 / Truncated §6.3).
    recovered_anchored: usize,
    recovered_mismatch: usize,
    // ABANDONED (a terminal WAS written) — the recovery-terminus closers (R6):
    /// (c) SEARCHED-no-match — candidates existed past the anchor, none matched → the
    /// DISCLOSED `pending-abandoned{recovery-no-candidate}` (recovered:true + attribution).
    abandoned_no_candidate: usize,
    /// (d) MISSING recovery keys — no `content_sha256`, a search can never run →
    /// `pending-abandoned{recovery-unattributable}` (never claims "no-candidate").
    abandoned_unattributable: usize,
    // LEFT RECOVERABLE (NO terminal written) — undetermined, dead-dangling for a later run:
    /// (a) transcript UNREADABLE/UNRESOLVABLE (build_window → None).
    left_source_unavailable: usize,
    /// (b) EMPTY window — read OK, zero candidates past the anchor (still growable:
    /// busy-turn flush lag / rotation-in-place).
    left_window_empty: usize,
    /// the FENCE held — writer still live (no terminal written).
    skipped_live: usize,
    /// already carried a terminal (idempotent no-op).
    skipped_resolved: usize,
    emit_errors: usize,
}

impl RecoverReport {
    /// Anchored recoveries (the content landed). Abandoned closers are counted
    /// separately (they are NOT "recovered").
    fn recovered_total(&self) -> usize {
        self.recovered_anchored + self.recovered_mismatch
    }
    /// Disclosed abandoned closers — a terminal was written (c + d).
    fn abandoned_total(&self) -> usize {
        self.abandoned_no_candidate + self.abandoned_unattributable
    }
    /// Left dead-dangling-recoverable — NO terminal written (a + b).
    fn left_recoverable_total(&self) -> usize {
        self.left_source_unavailable + self.left_window_empty
    }
}

pub fn run(m: &ArgMatches) -> i32 {
    let target_send_id = m.get_one::<String>("send-id").map(String::as_str);
    let env = RealEnv;
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        eprintln!("qd delivery:recover: HOME unset — cannot resolve the state dir");
        return 1;
    };
    let paths = QdPaths::from_home_env(Path::new(&home), &env);
    let clock = RealClock;
    let now_ms = clock.now_ms();

    let report = recover_sweep(&paths, &env, target_send_id, now_ms);

    // Content-free summary (a recovery tool, not a chatty one). Best-effort — the
    // real product is the appended terminals in the delivery log.
    println!(
        "qd delivery:recover: scanned {} dangling-eligible send(s); recovered {} \
         (anchored {}, mismatch {}); abandoned {} (no-candidate {}, unattributable {}); \
         left recoverable {} (source-unavailable {}, window-empty {}); left {} live-writer \
         send(s) untouched (fence held), {} already resolved; {} emit error(s).",
        report.scanned,
        report.recovered_total(),
        report.recovered_anchored,
        report.recovered_mismatch,
        report.abandoned_total(),
        report.abandoned_no_candidate,
        report.abandoned_unattributable,
        report.left_recoverable_total(),
        report.left_source_unavailable,
        report.left_window_empty,
        report.skipped_live,
        report.skipped_resolved,
        report.emit_errors,
    );
    0
}

/// The core sweep. Enumerate every key's INTENT file, collect the recovery-eligible
/// `send-initiated` records, and for each — GATED BY
/// [`events::writer_gone_and_stale`], then by [`LaneOps::resolved`] — ask that row's
/// LANE to resolve it. `target` optionally narrows to one send_id.
fn recover_sweep(
    paths: &QdPaths,
    env: &dyn Env,
    target: Option<&str>,
    now_ms: i64,
) -> RecoverReport {
    let state_dir = paths.state_dir.as_path();
    let mut report = RecoverReport::default();
    let initiations = collect_pty_initiations(state_dir, target);

    for si in &initiations {
        report.scanned += 1;

        // THE FENCE, on qd's OWN record: clauses (b) "the writer incarnation is
        // gone" and (c) "age > T_ANCHOR_IDLE_MS" read this record's `pid`,
        // `start_ms` and `ts` — all three written by qd, into qd's intent log, and
        // none of them needing a single byte of qw's. `events::is_dead_dangling`
        // is now literally clause (a) plus this call, so the fence is the SAME
        // code it always was rather than a copy of it, and the RF-6 start_ms arm
        // (a recycled pid held by a stranger) is unchanged.
        //
        // A live writer is refused HERE, before any lane call is made — which is
        // stronger than before, not weaker: `recover` is never reached.
        if !events::writer_gone_and_stale(si, now_ms) {
            report.skipped_live += 1;
            continue;
        }

        // Dead-dangling → the address is the record's OWN `(session?, name?)`
        // pair, which is exactly what keyed the file this record came out of:
        // `LedgerAddress::writer_key` reproduces the session-uuid-else-
        // `byname-<name>` key the verdict is written under, and `parts()` feeds
        // the same merged read.
        let at = LedgerAddress {
            session: si.session.clone().map(SessionId),
            name: si.name.clone(),
        };
        let ops = dispatch::lane::open(lane_for_row(paths, env, &at), env, paths.clone());
        let message = MessageId(si.send_id().unwrap_or_default());

        // CLAUSE (a), and the ONLY clause that crosses: "is there already a
        // terminal for this send_id" is answerable only from qw's delivery log,
        // which qd does not read. `LaneOps::resolved` is that read and nothing
        // else — no transcript, no lock, no emission.
        //
        // It must come BEFORE `recover`. `recover`'s under-flock idempotence
        // ADOPTS an existing terminal rather than writing a second one, so the
        // LEDGER would be safe either way — but its adopt path reports through
        // `RecoveryVerdict`, which has no "already resolved" state, so a send
        // delivered and seen months ago would come back as
        // `NotDelivered{recovery-no-candidate}` and this summary would report it
        // as abandoned. Every resolved send in a state dir's history, on every
        // run, each paying a transcript read to get there.
        //
        // An `Err` is an un-read, not an absence: counted as an error and the send
        // is LEFT ALONE, never handed to `recover` on the assumption that nothing
        // closed it.
        match ops.resolved(&at, &message) {
            Ok(Some(_)) => {
                report.skipped_resolved += 1;
                continue;
            }
            Ok(None) => {}
            Err(_) => {
                report.emit_errors += 1;
                continue;
            }
        }

        // The recovery-terminus lattice (R6) as the contract restates it: a terminal
        // is written for anchored/mismatch (landed) and the two DISCLOSED abandoned
        // closers (c/d); NO terminal for the two undetermined states (a/b) — those
        // stay dead-dangling for a later run, counted as "left recoverable" so the
        // summary stays honest.
        //
        // `Terminal` has four variants against the lattice's six termini, so the pair
        // inside each collapsed variant is split back apart on the ledger's own reason
        // token. `Undetermined` is NEVER a foreclosing outcome — the catch-all arm
        // routes any reason we do not recognise (an address naming neither session nor
        // target; a send_id with no `send-initiated` to search from) to
        // source-unavailable, "there was nothing to look at", which is the honest
        // reading and the one that leaves the send recoverable.
        match ops.recover(&at, &message) {
            Ok(Terminal::Seen) => report.recovered_anchored += 1,
            Ok(Terminal::Mismatch) => report.recovered_mismatch += 1,
            Ok(Terminal::NotDelivered { ref reason }) if reason == RECOVERY_NO_CANDIDATE => {
                report.abandoned_no_candidate += 1
            }
            Ok(Terminal::NotDelivered { ref reason }) if reason == RECOVERY_UNATTRIBUTABLE => {
                report.abandoned_unattributable += 1
            }
            Ok(Terminal::Undetermined { ref reason }) if reason == WINDOW_EMPTY => {
                report.left_window_empty += 1
            }
            Ok(Terminal::Undetermined { .. }) => report.left_source_unavailable += 1,
            // Unreachable by contract: `recover` mints only the six verdicts above.
            // A foreclosing terminal we cannot name is counted as an ERROR rather than
            // silently as a recovery — miscounting an unrecognised terminal as a
            // success is the one thing this summary must not do.
            Ok(_) | Err(_) => report.emit_errors += 1,
        }
    }

    report
}

/// Which lane owns a swept row.
///
/// The sweep walks LEDGER rows, and a ledger row's identity is `(session?, name?)`
/// — no provider, no hosting. The harness lives in the registry, so a
/// session-addressed row is placed by the same exact, id-keyed lookup every other
/// rewired verb uses ([`quorum_qw::lanes::row_for_id`]) and then by `lane_for` over
/// that row's `provider`/`hosting`, byte-identically to `verbs/attach.rs`. A
/// byname-only row has no registry row to look up by construction, and a
/// session-addressed row may have had its row reaped; both land on
/// [`FALLBACK_LANE`], whose docs explain why that changes no answer.
fn lane_for_row(paths: &QdPaths, env: &dyn Env, at: &LedgerAddress) -> Lane {
    at.session
        .as_ref()
        .and_then(|id| quorum_qw::lanes::row_for_id(paths, env, None, id))
        .and_then(|s| quorum_qw::lane_for(&s.provider, s.hosting.as_deref()))
        .unwrap_or(FALLBACK_LANE)
}

/// The verbs whose sends this sweep may close.
///
/// `send:pty` and `new-p` are the transcript-anchored sends recovery-read was
/// designed for. [`super::intent::VERB_SEND`] is qd's own token for the unified
/// `qd send`, and it is here because `LaneOps::deliver` picks the carrier
/// privately: a unified send that lands on the pane carrier is exactly as
/// recoverable as a `send:pty` one — qw's record carries the same id — and there
/// is no way to tell at intent time which carrier it will be. One that lands on a
/// resident carrier answers `Undetermined`, because qw's ledger has no
/// `send-initiated` under qd's id (a resident keys its own on the turn id it
/// minted), and `Undetermined` mints no terminal. So the scoping defence holds
/// either way: nothing is closed on evidence that was never read.
///
/// `send:relay` is still absent, which is the defence
/// `delivery_recover_verb::relay_send_is_not_swept` pins.
const SWEPT_VERBS: &[&str] = &["send:pty", "new-p", super::intent::VERB_SEND];

/// The recovery-eligible `send-initiated` records in qd's own intent log,
/// optionally narrowed to `target` send_id.
///
/// The enumeration itself is [`super::intent::scan`]'s: every read of the intent
/// tree lives in the one module that owns it, so `ledger_gate` can pin the ledger
/// reads by file. This used to `read_dir` qw's `sessions/` directly; after the
/// ledger split those records are qd's own (`09-ledger-split.md`, ruling D4), so
/// the enumeration stops being a boundary crossing at all.
fn collect_pty_initiations(state_dir: &Path, target: Option<&str>) -> Vec<EventRecord> {
    super::intent::scan(state_dir, SWEPT_VERBS, target)
}
