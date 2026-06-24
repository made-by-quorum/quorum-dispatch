//! `sb gc` core (spec §5.2; TS `commands/gc.ts`).
//!
//! Prune stale, DEAD, file-class artifacts to recoverable trash:
//!   - dead CC JSONL transcripts (≥7d old AND no live PID),
//!   - stale OC sidecars (PID dead + port unresponsive),
//!   - orphaned OC logs (no matching sidecar),
//!   - old tombstones (≥7d).
//!
//! # L10 discipline is STRUCTURAL
//!
//! Every candidate is a FILE keyed by a specific dead identity (a session id
//! whose PID is dead, a sidecar whose PID is dead + port unresponsive). We never
//! match patterns, never touch a live/attached session, never kill a process.
//! The liveness gate (`live_session_ids`) is computed from the registry PIDs the
//! caller probes — a session id is "live" iff its registry PID is alive OR it is
//! under a live zmx wrapper (`gc.ts:91-113`). Anything live is excluded BEFORE a
//! file is ever a candidate.
//!
//! # Clock seam
//!
//! All ages (the 7-day GC window, the 30-day purge window) compare an injected
//! mtime against an injected `now_ms`; tests forge both, no real clock touched.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 7 days in ms — the GC staleness window (`gc.ts:91`, `:212`).
pub const GC_STALE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// 30 days in ms — the purge window (`gc.ts:441`).
pub const PURGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Candidate class (`GcCandidate.type`, `gc.ts:28`). Drives the trash dir, file
/// extension, and label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateType {
    /// Dead Claude-Code JSONL transcript.
    CcJsonl,
    /// Old Claude-Code tombstone.
    CcTombstone,
    /// Stale OpenCode sidecar.
    OcSidecar,
    /// Orphaned OpenCode log.
    OcLog,
}

impl CandidateType {
    /// The trash subtree (`trashDirFor`, `gc.ts:224-227`): cc-* → ~/.claude/trash,
    /// else ~/.opencode/trash.
    pub fn is_claude(&self) -> bool {
        matches!(self, CandidateType::CcJsonl | CandidateType::CcTombstone)
    }

    /// File extension for the trash name (`extFor`, `gc.ts:229-237`).
    pub fn ext(&self) -> &'static str {
        match self {
            CandidateType::CcJsonl => ".jsonl",
            CandidateType::CcTombstone => ".json.tombstoned",
            CandidateType::OcSidecar => ".json",
            CandidateType::OcLog => ".log",
        }
    }

    /// Human label (`typeLabel`, `gc.ts:239-247`).
    pub fn label(&self) -> &'static str {
        match self {
            CandidateType::CcJsonl => "CC JSONL",
            CandidateType::CcTombstone => "CC tombstone",
            CandidateType::OcSidecar => "OC sidecar",
            CandidateType::OcLog => "OC log",
        }
    }
}

/// One GC candidate (`GcCandidate`, `gc.ts:28-34`).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub kind: CandidateType,
    pub path: PathBuf,
    pub reason: String,
    pub size: u64,
    /// sessionId for CC, port for OC.
    pub identifier: String,
}

/// PURE: is a CC JSONL transcript a GC candidate? (`scanDeadCcJsonl`,
/// `gc.ts:117-145`). A transcript is dead when its session id is NOT live AND its
/// mtime is older than the 7-day window. The liveness set is the caller's
/// (registry PIDs alive ∪ under-live-zmx-wrapper).
pub fn jsonl_is_candidate(
    session_id: &str,
    mtime_ms: i64,
    now_ms: i64,
    live_session_ids: &HashSet<String>,
) -> bool {
    if live_session_ids.contains(session_id) {
        return false; // live → never a candidate (L10).
    }
    let stale_before = now_ms - GC_STALE_MS;
    mtime_ms <= stale_before
}

/// PURE: is a tombstone a GC candidate? (`scanOldTombstones`, `gc.ts:204-220`).
/// Older than the 7-day window.
pub fn tombstone_is_candidate(mtime_ms: i64, now_ms: i64) -> bool {
    mtime_ms <= now_ms - GC_STALE_MS
}

/// PURE: is an OC sidecar stale? (`scanStaleOcSidecars`, `gc.ts:147-176`). Stale
/// when its PID is dead AND its port is unresponsive — both probed by the caller.
pub fn sidecar_is_stale(pid_alive: bool, port_healthy: bool) -> bool {
    !pid_alive && !port_healthy
}

/// PURE: the trash item base name (`moveToTrash`, `gc.ts:282-285`):
/// `<timestamp>_<identifier><ext>`. The timestamp is the caller's ISO-derived
/// stamp (`isoTimestamp`, `gc.ts:84-86`).
pub fn trash_name(timestamp: &str, identifier: &str, kind: CandidateType) -> String {
    format!("{timestamp}_{identifier}{ext}", ext = kind.ext())
}

/// PURE: ISO-8601 timestamp with `:`/`.` replaced by `-`, truncated to seconds
/// (`isoTimestamp`, `gc.ts:84-86`): `2026-06-05T01-23-45`. Takes the formatted
/// ISO string so the Clock stays injected.
pub fn iso_stamp_from(iso: &str) -> String {
    // TS: `new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19)`.
    let replaced: String = iso
        .chars()
        .map(|c| if c == ':' || c == '.' { '-' } else { c })
        .collect();
    replaced.chars().take(19).collect()
}

/// PURE: should this trash item be purged? (`purgeTrash`, `gc.ts:438-446`).
/// Pruned-at older than the 30-day window.
pub fn should_purge(pruned_at_ms: i64, now_ms: i64) -> bool {
    pruned_at_ms < now_ms - PURGE_MS
}

/// Trash metadata sidecar (`TrashMeta`, `gc.ts:36-42`). Serialized as
/// `<name>_meta.json` next to the trashed file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TrashMeta {
    #[serde(rename = "originalPath")]
    pub original_path: String,
    pub reason: String,
    #[serde(rename = "prunedAt")]
    pub pruned_at: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub size: u64,
}

/// The on-disk type token for a candidate kind (matches the TS `type` strings
/// written into `_meta.json`).
pub fn type_token(kind: CandidateType) -> &'static str {
    match kind {
        CandidateType::CcJsonl => "cc-jsonl",
        CandidateType::CcTombstone => "cc-tombstone",
        CandidateType::OcSidecar => "oc-sidecar",
        CandidateType::OcLog => "oc-log",
    }
}

/// PURE: human relative age `<n>{s,m,h,d} ago` (`relativeTime`,
/// `session.ts:1243-1255`). Deterministic given the injected `now_ms` — the
/// candidate `reason` strings and the `--list-trash` Age line both carry it, so
/// dropping it for static text would be an undocumented byte divergence.
pub fn relative_time(date_ms: i64, now_ms: i64) -> String {
    let diff = now_ms - date_ms;
    let seconds = diff.div_euclid(1000);
    if seconds < 60 {
        return format!("{seconds}s ago");
    }
    let minutes = seconds.div_euclid(60);
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes.div_euclid(60);
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours.div_euclid(24);
    format!("{days}d ago")
}

/// Human-readable byte size (`formatBytes`, `gc.ts:78-82`).
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

/// Shorten a path under `home` to `~/...` for display. gc.ts imports the shared
/// `shortenPath` from session.ts (gc.ts:17), so we delegate to the same
/// [`crate::fmt::shorten_path`] port — which ALSO carries the Dropbox
/// CloudStorage special-case (`~/Library/CloudStorage/Dropbox` → `~/Dropbox`).
/// A hand-rolled home-only strip would silently diverge on Dropbox-hosted paths.
pub fn shorten_path(p: &Path, home: &Path) -> String {
    crate::fmt::shorten_path(&p.to_string_lossy(), &home.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn jsonl_dead_and_old_is_candidate() {
        let old = NOW - GC_STALE_MS - 1;
        assert!(jsonl_is_candidate("sid", old, NOW, &ids(&[])));
    }

    #[test]
    fn jsonl_live_session_never_candidate_even_if_old() {
        // L10: a live session id is excluded BEFORE age is even considered.
        let old = NOW - GC_STALE_MS - 1_000_000;
        assert!(!jsonl_is_candidate("sid", old, NOW, &ids(&["sid"])));
    }

    #[test]
    fn jsonl_recent_dead_is_not_candidate() {
        // Dead but within the 7-day window → not yet a candidate.
        let recent = NOW - 1000;
        assert!(!jsonl_is_candidate("sid", recent, NOW, &ids(&[])));
    }

    #[test]
    fn jsonl_boundary_exactly_seven_days_is_candidate() {
        // mtime exactly at the window edge → `<=` includes it.
        let edge = NOW - GC_STALE_MS;
        assert!(jsonl_is_candidate("sid", edge, NOW, &ids(&[])));
    }

    #[test]
    fn tombstone_age_gate() {
        assert!(tombstone_is_candidate(NOW - GC_STALE_MS - 1, NOW));
        assert!(!tombstone_is_candidate(NOW - 1, NOW));
    }

    #[test]
    fn sidecar_stale_only_when_pid_dead_and_port_down() {
        assert!(sidecar_is_stale(false, false));
        assert!(!sidecar_is_stale(true, false)); // pid alive → protected
        assert!(!sidecar_is_stale(false, true)); // port healthy → protected
        assert!(!sidecar_is_stale(true, true));
    }

    #[test]
    fn trash_name_shape() {
        assert_eq!(
            trash_name("2026-06-05T01-23-45", "sid-abc", CandidateType::CcJsonl),
            "2026-06-05T01-23-45_sid-abc.jsonl"
        );
        assert_eq!(
            trash_name("2026-06-05T01-23-45", "9999", CandidateType::OcSidecar),
            "2026-06-05T01-23-45_9999.json"
        );
    }

    #[test]
    fn iso_stamp_replaces_colons_dots_and_truncates() {
        assert_eq!(
            iso_stamp_from("2026-06-05T01:23:45.678Z"),
            "2026-06-05T01-23-45"
        );
    }

    #[test]
    fn purge_gate() {
        assert!(should_purge(NOW - PURGE_MS - 1, NOW));
        assert!(!should_purge(NOW - PURGE_MS, NOW)); // strict `<` like TS
        assert!(!should_purge(NOW - 1000, NOW));
    }

    #[test]
    fn relative_time_buckets() {
        assert_eq!(relative_time(NOW - 5_000, NOW), "5s ago");
        assert_eq!(relative_time(NOW - 5 * 60_000, NOW), "5m ago");
        assert_eq!(relative_time(NOW - 5 * 3_600_000, NOW), "5h ago");
        assert_eq!(relative_time(NOW - 5 * 86_400_000, NOW), "5d ago");
        // boundary: exactly 60s → minutes bucket.
        assert_eq!(relative_time(NOW - 60_000, NOW), "1m ago");
    }

    #[test]
    fn format_bytes_thresholds() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(2_500_000), "2.5 MB");
    }

    #[test]
    fn shorten_path_tilde() {
        let home = Path::new("/home/u");
        assert_eq!(
            shorten_path(Path::new("/home/u/.claude/x.jsonl"), home),
            "~/.claude/x.jsonl"
        );
        assert_eq!(shorten_path(Path::new("/var/tmp/y"), home), "/var/tmp/y");
    }

    #[test]
    fn type_tokens_and_dirs() {
        assert_eq!(type_token(CandidateType::CcJsonl), "cc-jsonl");
        assert!(CandidateType::CcJsonl.is_claude());
        assert!(CandidateType::CcTombstone.is_claude());
        assert!(!CandidateType::OcSidecar.is_claude());
        assert!(!CandidateType::OcLog.is_claude());
    }

    #[test]
    fn trash_meta_round_trips_camelcase() {
        let m = TrashMeta {
            original_path: "/h/.claude/projects/d/s.jsonl".to_string(),
            reason: "dead session".to_string(),
            pruned_at: "2026-06-05T01:23:45.000Z".to_string(),
            kind: "cc-jsonl".to_string(),
            size: 4096,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"originalPath\""));
        assert!(json.contains("\"prunedAt\""));
        let back: TrashMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
