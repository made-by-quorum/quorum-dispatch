//! `qd delivery:recover` — the one-shot, dispatch-only delivery recovery verb (D1,
//! spec §C2). Closes DEAD-DANGLING pty/new-p sends: a `send-initiated` with no
//! terminal whose WRITER incarnation is gone (a sender killed mid-send / a SIGKILL
//! that bypassed the WatchGuard Drop). It runs the tested lib recovery-read
//! ([`events::recovery_read`]) and appends the verdict via
//! [`events::emit_recovery_verdict`].
//!
//! ## THE LIVENESS FENCE (hard obligation — cycle-3 finding)
//! This verb runs as a SEPARATE process from the original sender, so it MUST NOT
//! write a terminal for a send whose writer is still LIVE. `emit_recovery_verdict`
//! has NO dead-writer gate of its own — its only re-check is idempotence against a
//! raced-in *terminal*, NOT a live *writer* — so calling it directly on a live-writer
//! `send-initiated` would append a PREMATURE terminal on a still-LIVE send, violating
//! QS-1 ("the ledger lies") through the very path built to keep it honest. This verb
//! therefore REPLICATES the gate `await_received` holds: it calls
//! [`events::is_dead_dangling`] itself and emits ONLY when it returns true.
//! "Dangling" here means DEAD-dangling, never merely unresolved. A live-writer send
//! is left untouched (stays dangling-but-live). Because the verb is a foreign
//! process, `is_dead_dangling`'s own-pid short-circuit does NOT fire — a foreign live
//! pid is correctly evaluated via the RF-6 `start_ms` arm.
//!
//! ## SCOPE (minimal, per the plan)
//! No flags beyond target selection; no scheduling; no residency. The resident /
//! scheduled recovery runner is the deferred `recovery_coordinator` bin (C6) — NOT
//! touched here. The sweep is restricted to transcript-anchored pty/new-p sends: the
//! sends recovery-read was designed for and that `await_received` already handles.
//! Relay/daemon sends resolve via their recipient-side observers (C5/C6); running
//! recovery-read on a relay `send-initiated` (no transcript, one-shot writer already
//! dead) would manufacture a FALSE `pending-abandoned` — the opposite of honesty.

use std::path::{Path, PathBuf};

use clap::ArgMatches;

use dispatch::effects::{Clock, Env, RealClock, RealEnv};
use dispatch::events::{self, EventRecord, EventWriter, ReaderCtx, RecoveryDeps, RecoveryVerdict};
use dispatch::paths::QdPaths;

/// Real fs/clock backing for [`events::RecoveryDeps`] (the verb's production seam).
/// `now_ms` is captured ONCE at verb start so every send in a sweep is judged
/// against the same clock (and matches the `now_ms` the fence used).
struct RealRecoveryDeps {
    projects_dir: PathBuf,
    now_ms: i64,
}

impl RecoveryDeps for RealRecoveryDeps {
    fn read_transcript(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
    fn resolve_transcript(&self, session_id: Option<&str>, _name: Option<&str>) -> Option<String> {
        // Offset-absent fallback (§6.1): re-resolve the transcript NOW via the same
        // registry scan the live path uses. cwd is unknown here → the scan tier.
        let sid = session_id?;
        dispatch::jsonl::find_jsonl_path(&self.projects_dir, sid, None)
            .map(|p| p.display().to_string())
    }
    fn now_ms(&self) -> i64 {
        self.now_ms
    }
}

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

    let report = recover_sweep(
        &paths.state_dir,
        &paths.projects_dir,
        target_send_id,
        &clock,
        now_ms,
    );

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

/// The core sweep (testable, pure over its inputs). Enumerate every session's
/// events file, collect the transcript-anchored (pty/new-p) `send-initiated`
/// records, and for each — GATED BY [`events::is_dead_dangling`] — append a recovery
/// verdict. `target` optionally narrows to one send_id.
fn recover_sweep(
    state_dir: &Path,
    projects_dir: &Path,
    target: Option<&str>,
    clock: &dyn Clock,
    now_ms: i64,
) -> RecoverReport {
    let mut report = RecoverReport::default();
    let initiations = collect_pty_initiations(state_dir, target);

    for si in &initiations {
        report.scanned += 1;
        let ctx = ReaderCtx {
            state_dir,
            session_id: si.session.as_deref(),
            name: si.name.as_deref(),
        };
        let merged = ctx.read();

        // THE FENCE: emit ONLY when the writer incarnation is gone (dead-dangling).
        // is_dead_dangling returns false BOTH when a terminal already exists AND when
        // the writer is still live — the two skip reasons the summary distinguishes.
        if !events::is_dead_dangling(&merged.records, si, now_ms) {
            let sid = si.send_id().unwrap_or_default();
            if events::first_terminal_for(&merged.records, &sid).is_some() {
                report.skipped_resolved += 1;
            } else {
                report.skipped_live += 1;
            }
            continue;
        }

        // Dead-dangling → recover. Key the verdict to the send-initiated's own file
        // (session uuid when known, else byname); the merged read covers both.
        let key = si
            .session
            .clone()
            .unwrap_or_else(|| events::byname_key(si.name.as_deref().unwrap_or("")));
        let writer = EventWriter::for_key(state_dir, &key, si.session.clone(), si.name.clone());
        let deps = RealRecoveryDeps {
            projects_dir: projects_dir.to_path_buf(),
            now_ms,
        };
        // The recovery-terminus lattice (R6): a terminal is written for anchored/
        // mismatch (landed) and the two DISCLOSED abandoned closers (c/d); NO terminal
        // for the two undetermined states (a/b) — those stay dead-dangling for a later
        // run, counted as "left recoverable" so the summary stays honest.
        match events::emit_recovery_verdict(&deps, &writer, clock, ctx, si) {
            Ok(RecoveryVerdict::Anchored { .. }) => report.recovered_anchored += 1,
            Ok(RecoveryVerdict::Truncated { .. }) => report.recovered_mismatch += 1,
            Ok(RecoveryVerdict::Abandoned { .. }) => report.abandoned_no_candidate += 1,
            Ok(RecoveryVerdict::Unattributable) => report.abandoned_unattributable += 1,
            Ok(RecoveryVerdict::SourceUnavailable) => report.left_source_unavailable += 1,
            Ok(RecoveryVerdict::EmptyWindow) => report.left_window_empty += 1,
            Err(_) => report.emit_errors += 1,
        }
    }

    report
}

/// Read every `*.events.jsonl` under the state dir's `sessions/` dir and return the
/// transcript-anchored `send-initiated` records (verb ∈ {send:pty, new-p}) — the
/// recovery-read-eligible set — optionally narrowed to `target` send_id.
fn collect_pty_initiations(state_dir: &Path, target: Option<&str>) -> Vec<EventRecord> {
    let dir = events::events_dir(state_dir);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out; // no sessions dir yet → nothing to recover
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_events_file = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(".events.jsonl"))
            .unwrap_or(false);
        if !is_events_file {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for rec in events::parse_events(&text).records {
            if rec.event != "send-initiated" {
                continue;
            }
            // Transcript-anchored sends only (pty/new-p) — see the scope note above.
            match rec.str_field("verb").as_deref() {
                Some("send:pty") | Some("new-p") => {}
                _ => continue,
            }
            if let Some(t) = target {
                if rec.send_id().as_deref() != Some(t) {
                    continue;
                }
            }
            out.push(rec);
        }
    }
    out
}
