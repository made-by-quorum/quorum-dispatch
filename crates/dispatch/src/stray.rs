//! M3: stray-discovery decider (spec §7, brief deliverable 5).
//!
//! NEW behavior — no TS counterpart. The TS already lists registry-less
//! transcripts as `cold` (dead); the A1 delta is detecting the LIVE ones and
//! badging them.
//!
//! A **stray** is a live, unmanaged session: a transcript
//! `<projects_dir>/<cwd-slug>/<uuid>.jsonl` whose `session_id` has NO registry
//! entry (live OR tombstoned — the caller passes the UNION) AND a live claude
//! process attributable to it.
//!
//! **DEAD transcripts (no live proc) are NOT strays** — they stay ordinary cold
//! listings (the TS cold behavior, unchanged). Nothing new is emitted for them.
//!
//! **TAKEOVER is PARKED.** [`Stray`] deliberately carries `pid` + paths so a
//! future adopt/attach verb is not precluded, but NO such verb exists here —
//! this decider only *lists*.
//!
//! Attribution is **best-effort / cosmetic** (per the brief): a claude process
//! whose `cwd` maps (via [`crate::jsonl::cwd_to_project_path`]) to the
//! transcript's project dir, where that process is NOT already accounted for by
//! a registry pid. A process with `cwd == None` is unattributable and produces
//! NO stray (we never guess). Tie-break: the most-recent transcript mtime in
//! the matched project dir (one stray per proc).

use std::collections::HashSet;

use crate::effects::ProcInfo;
use crate::jsonl::{cwd_to_project_path, TranscriptMeta};

/// How recent a transcript's mtime must be (vs the injected clock) to badge a
/// stray as "active". 5 minutes — COSMETIC and fixture-frozen; may be re-ruled
/// at pass (b).
const ACTIVE_RECENT_WINDOW_MS: i64 = 5 * 60 * 1000;

/// A detected live, unmanaged session.
#[derive(Debug, Clone, PartialEq)]
pub struct Stray {
    pub session_id: String,
    pub jsonl_path: std::path::PathBuf,
    pub project_dir: String,
    /// The attributed live claude process. PARKED-takeover evidence: kept so a
    /// future adopt/attach can target it; unused by this lister.
    pub pid: Option<i32>,
    pub mtime_ms: i64,
    /// Cosmetic activity badge: mtime within [`ACTIVE_RECENT_WINDOW_MS`] of now.
    pub active_recent: bool,
}

/// Classify strays — PURE decider (spec §7).
///
/// - `transcripts`: every scanned transcript ([`crate::jsonl::scan_all`]).
/// - `registry_session_ids`: the UNION of live + tombstoned registry session ids
///   (caller-supplied). A transcript whose id is in this set is managed → never
///   a stray.
/// - `registry_pids_alive`: pids the registry already accounts for; a proc whose
///   pid is in this set is already managed → not stray evidence.
/// - `claude_procs`: live claude processes (from `ProcessTable`).
/// - `now_ms`: injected clock for the activity badge.
///
/// Algorithm: each candidate proc (live claude, `cwd` known, pid not registry-
/// owned) maps its cwd to a project-dir slug; among the UNMANAGED transcripts in
/// that dir we pick the most-recent mtime (tie-break) and emit ONE stray. A proc
/// with no cwd, or no unmanaged transcript in its dir, yields nothing.
pub fn classify(
    transcripts: &[TranscriptMeta],
    registry_session_ids: &HashSet<String>,
    registry_pids_alive: &HashSet<i32>,
    claude_procs: &[ProcInfo],
    now_ms: i64,
) -> Vec<Stray> {
    let mut strays = Vec::new();
    // Track transcripts already claimed by a stray so two procs in the same dir
    // don't both adopt the same transcript.
    let mut claimed: HashSet<&str> = HashSet::new();

    for proc in claude_procs {
        // Procs with cwd None are unattributable → NO stray (never guess).
        let Some(cwd) = proc.cwd.as_deref() else {
            continue;
        };
        // Already accounted for by a registry pid → not stray evidence.
        if registry_pids_alive.contains(&proc.pid) {
            continue;
        }
        let proc_dir = cwd_to_project_path(cwd);

        // Among UNMANAGED, UNCLAIMED transcripts in this proc's project dir, pick
        // the most-recent mtime (tie-break). Highest mtime wins; on equal mtime
        // the first scanned is kept (stable).
        let mut best: Option<&TranscriptMeta> = None;
        for t in transcripts {
            if t.project_dir != proc_dir {
                continue;
            }
            if registry_session_ids.contains(&t.session_id) {
                continue; // managed → not a stray
            }
            if claimed.contains(t.session_id.as_str()) {
                continue; // another proc already took it
            }
            match best {
                Some(b) if b.mtime_ms >= t.mtime_ms => {}
                _ => best = Some(t),
            }
        }

        if let Some(t) = best {
            claimed.insert(t.session_id.as_str());
            let active_recent = now_ms - t.mtime_ms < ACTIVE_RECENT_WINDOW_MS;
            strays.push(Stray {
                session_id: t.session_id.clone(),
                jsonl_path: t.path.clone(),
                project_dir: t.project_dir.clone(),
                pid: Some(proc.pid),
                mtime_ms: t.mtime_ms,
                active_recent,
            });
        }
    }

    strays
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn meta(session_id: &str, project_dir: &str, mtime_ms: i64) -> TranscriptMeta {
        TranscriptMeta {
            session_id: session_id.to_string(),
            path: PathBuf::from(format!("/projects/{project_dir}/{session_id}.jsonl")),
            mtime_ms,
            project_dir: project_dir.to_string(),
        }
    }

    fn proc(pid: i32, cwd: Option<&str>) -> ProcInfo {
        ProcInfo {
            pid,
            ppid: 1,
            cmd: "claude".to_string(),
            cwd: cwd.map(str::to_string),
            started_ms: None,
        }
    }

    fn ids(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn pids(v: &[i32]) -> HashSet<i32> {
        v.iter().copied().collect()
    }

    // For "/work/proj", cwd_to_project_path → "-work-proj".
    const CWD: &str = "/work/proj";
    const DIR: &str = "-work-proj";

    #[test]
    fn live_stray_detected() {
        let transcripts = vec![meta("sid-1", DIR, 1_000)];
        let procs = vec![proc(4242, Some(CWD))];
        let strays = classify(&transcripts, &ids(&[]), &pids(&[]), &procs, 1_000);
        assert_eq!(strays.len(), 1);
        assert_eq!(strays[0].session_id, "sid-1");
        assert_eq!(strays[0].pid, Some(4242));
        assert_eq!(strays[0].project_dir, DIR);
    }

    #[test]
    fn dead_transcript_no_proc_is_not_stray() {
        // A transcript with no attributable live proc → NOT a stray (stays cold).
        let transcripts = vec![meta("sid-1", DIR, 1_000)];
        let strays = classify(&transcripts, &ids(&[]), &pids(&[]), &[], 1_000);
        assert!(strays.is_empty());
    }

    #[test]
    fn registry_covered_session_id_is_not_stray() {
        let transcripts = vec![meta("sid-1", DIR, 1_000)];
        let procs = vec![proc(4242, Some(CWD))];
        // sid-1 is in the registry union (live or tombstoned) → managed.
        let strays = classify(&transcripts, &ids(&["sid-1"]), &pids(&[]), &procs, 1_000);
        assert!(strays.is_empty());
    }

    #[test]
    fn proc_with_no_cwd_is_not_stray() {
        let transcripts = vec![meta("sid-1", DIR, 1_000)];
        let procs = vec![proc(4242, None)];
        let strays = classify(&transcripts, &ids(&[]), &pids(&[]), &procs, 1_000);
        assert!(
            strays.is_empty(),
            "cwd=None is unattributable → never guess"
        );
    }

    #[test]
    fn proc_pid_owned_by_registry_is_not_evidence() {
        let transcripts = vec![meta("sid-1", DIR, 1_000)];
        let procs = vec![proc(4242, Some(CWD))];
        // pid 4242 is already a live registry pid → not stray evidence.
        let strays = classify(&transcripts, &ids(&[]), &pids(&[4242]), &procs, 1_000);
        assert!(strays.is_empty());
    }

    #[test]
    fn two_transcripts_same_dir_most_recent_wins() {
        let transcripts = vec![
            meta("old", DIR, 1_000),
            meta("new", DIR, 5_000),
            meta("mid", DIR, 3_000),
        ];
        let procs = vec![proc(4242, Some(CWD))];
        let strays = classify(&transcripts, &ids(&[]), &pids(&[]), &procs, 5_000);
        assert_eq!(strays.len(), 1);
        assert_eq!(strays[0].session_id, "new", "most-recent mtime wins");
    }

    #[test]
    fn badge_boundary() {
        // mtime exactly window-old → NOT active (strict `<`); one ms inside → active.
        let now = 1_000_000;
        let at_boundary = now - ACTIVE_RECENT_WINDOW_MS; // delta == window → false
        let inside = now - ACTIVE_RECENT_WINDOW_MS + 1; // delta < window → true

        let t_boundary = vec![meta("sid-b", DIR, at_boundary)];
        let s = classify(
            &t_boundary,
            &ids(&[]),
            &pids(&[]),
            &[proc(1, Some(CWD))],
            now,
        );
        assert_eq!(s.len(), 1);
        assert!(!s[0].active_recent, "delta == window is NOT recent");

        let t_inside = vec![meta("sid-i", DIR, inside)];
        let s = classify(&t_inside, &ids(&[]), &pids(&[]), &[proc(1, Some(CWD))], now);
        assert!(s[0].active_recent, "delta < window is recent");
    }

    #[test]
    fn two_procs_same_dir_take_distinct_transcripts() {
        // Two unmanaged transcripts + two procs in the same dir → each proc
        // claims a distinct transcript (one stray per proc, no double-claim).
        let transcripts = vec![meta("new", DIR, 5_000), meta("old", DIR, 1_000)];
        let procs = vec![proc(10, Some(CWD)), proc(20, Some(CWD))];
        let mut strays = classify(&transcripts, &ids(&[]), &pids(&[]), &procs, 5_000);
        strays.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        assert_eq!(strays.len(), 2);
        let claimed: Vec<&str> = strays.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(claimed, vec!["new", "old"]);
    }
}
