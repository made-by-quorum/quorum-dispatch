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

/// WS-B / B1 — relay-inbox message TTL (§2.1). An UN-acked inbox message is
/// collectible only after this window past its `received_at`. 7 days, matching
/// [`GC_STALE_MS`]: the relay inbox is a durability *fallback*, not a durable
/// queue, so it shares the operator's existing 7-day GC mental model.
pub const INBOX_TTL_MS: i64 = GC_STALE_MS;

/// WS-B / B1 — ack grace (§2.3). An ACKED inbox message (a live recipient
/// demonstrably took delivery) is collectible this long past its `acked_at_ms`.
/// 1 hour — short, because there is no loss risk once acked; the grace only lets
/// an in-flight reply or re-read settle.
pub const ACK_GRACE_MS: i64 = 60 * 60 * 1000;

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
    /// WS-B / B1 — a collectible relay-inbox message (ack+TTL+GC hygiene). Keyed
    /// by the inbox DIRECTORY, never by extension, so its `.json` files are never
    /// confused with the [`CandidateType::OcSidecar`] class (which is `.json` too).
    RelayInbox,
}

impl CandidateType {
    /// The trash subtree (`trashDirFor`, `gc.ts:224-227`): cc-* → ~/.claude/trash,
    /// else ~/.opencode/trash.
    pub fn is_claude(&self) -> bool {
        matches!(
            self,
            CandidateType::CcJsonl | CandidateType::CcTombstone | CandidateType::RelayInbox
        )
    }

    /// File extension for the trash name (`extFor`, `gc.ts:229-237`).
    pub fn ext(&self) -> &'static str {
        match self {
            CandidateType::CcJsonl => ".jsonl",
            CandidateType::CcTombstone => ".json.tombstoned",
            CandidateType::OcSidecar => ".json",
            CandidateType::OcLog => ".log",
            CandidateType::RelayInbox => ".json",
        }
    }

    /// Human label (`typeLabel`, `gc.ts:239-247`).
    pub fn label(&self) -> &'static str {
        match self {
            CandidateType::CcJsonl => "CC JSONL",
            CandidateType::CcTombstone => "CC tombstone",
            CandidateType::OcSidecar => "OC sidecar",
            CandidateType::OcLog => "OC log",
            CandidateType::RelayInbox => "relay inbox",
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

/// WS-B / B1 — PURE, clock-injected: is a relay-inbox message collectible? (§2.3).
///
/// The no-loss invariant lives here. `recipient_protected` (the presence gate —
/// the recipient is LIVE or in active recovery / has a live successor) **short-
/// circuits before any age test**: a protected recipient's mail is NEVER collected
/// regardless of age or ack. Otherwise the asymmetric window applies — an ACKED
/// message gets the short [`ACK_GRACE_MS`] past its ack (the recipient already has
/// it, no loss risk), an UN-acked message gets the full `ttl_ms` (default
/// [`INBOX_TTL_MS`]) past `received_at` (the loss-risk case, widest berth).
///
/// NO real clock is read here; `now_ms`, `received_at_ms`, and `acked_at_ms` are
/// all injected — exactly the `gc.rs` discipline ([`jsonl_is_candidate`]). Only the
/// `qd gc` driver (and the relay sweeper) supply a `RealClock`.
pub fn inbox_is_collectible(
    received_at_ms: i64,
    acked_at_ms: Option<i64>,
    ttl_ms: i64,
    now_ms: i64,
    recipient_protected: bool,
) -> bool {
    if recipient_protected {
        // Presence wins over age (the no-loss invariant, §5.1): never collect a
        // message a live/returning recipient may still need, even if expired.
        return false;
    }
    match acked_at_ms {
        Some(a) => now_ms - a >= ACK_GRACE_MS, // taken → short grace
        None => now_ms - received_at_ms >= ttl_ms, // un-taken → full TTL
    }
}

/// WS-B / B1 — the on-disk inbox record (`<inbox>/<message_id>.json`), additive +
/// backward-compatible. The ~2019 LEGACY files carry only `text`/`from_session`/
/// `message_id`/`received_at`; the three NEW fields are `#[serde(default)]` so a
/// fieldless legacy file parses (new fields → `None`) and `skip_serializing_if`
/// keeps a freshly-written record byte-minimal. Mirrors R2's proven additive
/// `incarnation`-claim pattern.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InboxRecord {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub from_session: String,
    #[serde(default)]
    pub message_id: String,
    /// ISO-8601 mint time (`epoch_ms_to_iso`); the primary TTL clock source.
    #[serde(default)]
    pub received_at: String,
    /// NEW — the addressed recipient session (the presence-gate key). Absent on a
    /// legacy file ⇒ unaddressable ⇒ not presence-protected (judged on TTL alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_session: Option<String>,
    /// NEW — epoch ms of ack-on-read / reply delivery. Absent ⇒ PENDING (full TTL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at_ms: Option<i64>,
    /// NEW (reserved) — per-message TTL override; absent ⇒ [`INBOX_TTL_MS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<i64>,
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

/// PURE: format epoch ms as an ISO-8601 UTC string (`new Date().toISOString()`
/// shape: `YYYY-MM-DDTHH:MM:SS.mmmZ`). Hand-rolled (no chrono dep) via the civil-
/// date algorithm. Single home for the ms↔ISO conversion (the `qd gc` verb's trash
/// stamping and the WS-B trash primitive both consume it) — the Clock stays
/// injected (the caller supplies `ms`).
pub fn iso_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// PURE: parse a ms-precision UTC ISO string back to epoch ms (inverse of
/// [`iso_from_ms`]). Permissive: an unparseable string yields `None` (the caller
/// falls back, e.g. to the file mtime for an inbox record's TTL clock).
pub fn parse_iso_ms(iso: &str) -> Option<i64> {
    // Expect YYYY-MM-DDTHH:MM:SS(.mmm)?Z
    let bytes = iso.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let y: i64 = iso.get(0..4)?.parse().ok()?;
    let mo: i64 = iso.get(5..7)?.parse().ok()?;
    let d: i64 = iso.get(8..10)?.parse().ok()?;
    let h: i64 = iso.get(11..13)?.parse().ok()?;
    let mi: i64 = iso.get(14..16)?.parse().ok()?;
    let s: i64 = iso.get(17..19)?.parse().ok()?;
    let millis: i64 = if iso.len() >= 23 && iso.as_bytes().get(19) == Some(&b'.') {
        iso.get(20..23)?.parse().ok()?
    } else {
        0
    };
    let days = days_from_civil(y, mo, d);
    Some(((days * 86_400 + h * 3600 + mi * 60 + s) * 1000) + millis)
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date from days since the Unix epoch (inverse of [`days_from_civil`]).
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A process-unique suffix for a trash basename. `pid` disambiguates concurrent
/// GC *processes* (the `qd gc` verb vs the relay sweeper, or two `qd gc` runs — they
/// never share a live pid); the monotonic counter disambiguates concurrent *calls*
/// within one process. Together they guarantee two callers can NEVER compute the
/// same `trash_path`/`meta_path`, even when picking the same file in the same
/// wall-clock second (the F1 root cause: `iso_stamp_from` truncates to seconds).
fn trash_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", std::process::id(), n)
}

/// Write `bytes` to `path` with O_EXCL (`create_new`): fails if the path already
/// exists, so this call only ever writes a file IT created — it can never clobber a
/// concurrent call's sidecar/copy.
fn write_new_excl(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()
}

/// Copy `src` → `dst` with an O_EXCL destination (`create_new`): the byte copy
/// lands in a file THIS call created, never overwriting an existing trash entry.
/// An ENOENT on `src` (a concurrent driver already unlinked it) surfaces as the
/// `File::open` error and is handled by the caller (tolerated, no loss).
fn copy_new_excl(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut reader = std::fs::File::open(src)?;
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    std::io::copy(&mut reader, &mut writer)?;
    use std::io::Write;
    writer.flush()
}

/// The canonical recoverable trash move (copy-then-unlink) — the `qd gc` verb's
/// `move_to_trash` discipline (`gc.ts:271-300`), lifted so BOTH the verb and the
/// relay sweeper share ONE implementation (never a fork). Order is loss-SAFER than
/// a rename: write `<name>_meta.json` → copy `src → trash` → `fs::remove_file(src)`
/// ONLY after the copy lands, cleaning up any partial trash on failure. A crash at
/// any point leaves the file readable in ≥1 of {src, trash} — never neither.
/// `now_ms` is injected (the Clock stays out of this function). Returns `true` iff
/// the message is now in trash (it was moved here, or a concurrent driver already
/// trashed/delivered it while this call's copy stands recoverable).
///
/// # F1 no-loss hardening (GC-vs-GC concurrency)
///
/// Two GC drivers with NO cross-driver lock can pick the same file in the same
/// wall-clock second. The old code derived the trash name from second-precision
/// time alone → both computed the SAME `trash_path`/`meta_path`, and the loser's
/// `remove_file(src)=ENOENT` cleanup deleted the WINNER's just-written copy (hard
/// delete) or stripped the shared `_meta.json` (unrecoverable orphan). Two layers
/// close the hole: **(a)** the basename carries a per-call nonce ([`trash_nonce`])
/// so two callers never share a path; **(b)** the sidecar + copy are created with
/// O_EXCL and the cleanup only ever removes paths THIS call created — and a final
/// `remove_file(src)=ENOENT` (src already gone ⇒ the message is provably trashed/
/// delivered by the other driver) KEEPS this call's recoverable copy instead of
/// deleting it.
#[allow(clippy::too_many_arguments)]
pub fn move_to_trash_at(
    src: &Path,
    trash_dir: &Path,
    kind: CandidateType,
    identifier: &str,
    reason: &str,
    size: u64,
    now_ms: i64,
) -> bool {
    let iso = iso_from_ms(now_ms);
    let timestamp = iso_stamp_from(&iso);
    // F1 fix (a): a per-call nonce makes the basename process-unique, so two
    // concurrent drivers in the same second never share trash_path/meta_path. It is
    // folded into the IDENTIFIER (before the class extension) so the trash file keeps
    // its `<ext>` suffix — `--recover`/`--list-trash` strip `_meta.json` and any
    // extension-keyed reader still sees a `.json`/`.jsonl` file.
    let name = trash_name(&timestamp, &format!("{identifier}.{}", trash_nonce()), kind);
    let trash_path = trash_dir.join(&name);
    let meta_path = trash_dir.join(format!("{name}_meta.json"));

    if std::fs::create_dir_all(trash_dir).is_err() {
        return false;
    }
    let meta = TrashMeta {
        original_path: src.to_string_lossy().into_owned(),
        reason: reason.to_string(),
        pruned_at: iso,
        kind: type_token(kind).to_string(),
        size,
    };
    let Ok(meta_json) = serde_json::to_string_pretty(&meta) else {
        return false;
    };
    // Meta first, then copy, then unlink the original — only after both land.
    // F1 fix (b): both writes are O_EXCL — this call only ever touches paths it
    // created, so a failed step can never delete another driver's trash entry.
    if write_new_excl(&meta_path, meta_json.as_bytes()).is_err() {
        return false;
    }
    if copy_new_excl(src, &trash_path).is_err() {
        // Copy failed (e.g. src already unlinked by a concurrent driver before our
        // copy): no durable copy landed here, so drop the sidecar WE just wrote.
        // The message is unharmed — still in src, or already in the other driver's
        // trash entry.
        let _ = std::fs::remove_file(&meta_path);
        return false;
    }
    match std::fs::remove_file(src) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // F1 fix (b): src already gone — a concurrent driver delivered or trashed
            // it between our copy and our unlink. Our copy at `trash_path` is a valid,
            // durable byte-copy with its own `_meta.json`, so we KEEP it (recoverable).
            // The message is provably in ≥1 of {our trash entry, the other driver's} —
            // never neither. (The old code deleted trash_path+meta_path here, which
            // under a SHARED name was the winner's copy → the F1a hard delete.)
            true
        }
        Err(_) => {
            // A real unlink failure (not ENOENT): src still exists, so the message is
            // safe there. Roll back OUR trash entry and report no-move.
            let _ = std::fs::remove_file(&meta_path);
            let _ = std::fs::remove_file(&trash_path);
            false
        }
    }
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
        CandidateType::RelayInbox => "relay-inbox",
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

    // ---- WS-B / B1 — inbox ack+TTL+GC -------------------------------------

    #[test]
    fn inbox_unacked_ttl_boundary() {
        // un-acked, age = TTL-1 → not collectible; age = TTL → collectible (`>=`).
        let recv = NOW - INBOX_TTL_MS + 1; // age = TTL-1
        assert!(!inbox_is_collectible(recv, None, INBOX_TTL_MS, NOW, false));
        let recv = NOW - INBOX_TTL_MS; // age = TTL exactly
        assert!(inbox_is_collectible(recv, None, INBOX_TTL_MS, NOW, false));
    }

    #[test]
    fn inbox_unacked_ttl_negative_control_drop_ttl_arm() {
        // Negative control for "the TTL arm shrinks the backlog": a message one ms
        // SHY of the TTL is NOT collectible — i.e. without the age reaching the TTL
        // the backlog never collects (revert/raise the TTL → RED: backlog immortal).
        let recv = NOW - INBOX_TTL_MS + 1;
        assert!(!inbox_is_collectible(recv, None, INBOX_TTL_MS, NOW, false));
    }

    #[test]
    fn inbox_acked_grace_boundary() {
        // acked, age-since-ack < grace → not collectible; >= grace → collectible,
        // and the grace collects an acked message FAR sooner than the 7-day TTL.
        let recv = NOW - INBOX_TTL_MS / 2; // well within TTL
        let acked = NOW - ACK_GRACE_MS + 1; // grace-1
        assert!(!inbox_is_collectible(recv, Some(acked), INBOX_TTL_MS, NOW, false));
        let acked = NOW - ACK_GRACE_MS; // grace exactly
        assert!(inbox_is_collectible(recv, Some(acked), INBOX_TTL_MS, NOW, false));
        // grace really is shorter than the TTL: acked-and-graced collects while the
        // same-age un-acked message (judged on received_at) would still be held.
        assert!(ACK_GRACE_MS < INBOX_TTL_MS);
    }

    #[test]
    fn inbox_presence_protect_short_circuits_age() {
        // recipient_protected → never collectible, even at 10× the TTL, acked or not.
        let ancient = NOW - 10 * INBOX_TTL_MS;
        assert!(!inbox_is_collectible(ancient, None, INBOX_TTL_MS, NOW, true));
        assert!(!inbox_is_collectible(
            ancient,
            Some(NOW - 10 * ACK_GRACE_MS),
            INBOX_TTL_MS,
            NOW,
            true
        ));
        // Negative control (the no-loss keystone): flip protection OFF → the same
        // ancient message IS collected (revert the presence gate → RED = loss).
        assert!(inbox_is_collectible(ancient, None, INBOX_TTL_MS, NOW, false));
    }

    #[test]
    fn inbox_ttl_override_respected() {
        // A tighter per-message ttl_ms collects sooner than INBOX_TTL_MS.
        let recv = NOW - 120_000; // 2 minutes old
        assert!(inbox_is_collectible(recv, None, 60_000, NOW, false)); // 60s ttl → yes
        assert!(!inbox_is_collectible(recv, None, INBOX_TTL_MS, NOW, false)); // 7d ttl → no
    }

    #[test]
    fn inbox_record_legacy_parses_and_is_ttl_judged() {
        // A REAL legacy file (no to_session/acked_at_ms/ttl_ms) must parse.
        let legacy = r#"{"text":"Reply with exactly: RELAY_OK","from_session":"1f17","message_id":"relay-1781113031221-4266362474","received_at":"2026-06-10T17:37:11.221Z"}"#;
        let rec: InboxRecord = serde_json::from_str(legacy).expect("legacy file must parse");
        assert_eq!(rec.to_session, None); // unaddressable → not presence-protected
        assert_eq!(rec.acked_at_ms, None); // PENDING → judged on full TTL
        assert_eq!(rec.ttl_ms, None);
        let recv = parse_iso_ms(&rec.received_at).expect("received_at parses");
        // 14 days after the mint, un-acked, unaddressed → collectible.
        let now = recv + 14 * 24 * 60 * 60 * 1000;
        assert!(inbox_is_collectible(recv, rec.acked_at_ms, INBOX_TTL_MS, now, false));
    }

    #[test]
    fn inbox_record_roundtrip_skips_none_fields() {
        // Fresh record (to_session set, no ack yet) serializes WITHOUT acked_at_ms/ttl_ms.
        let rec = InboxRecord {
            text: "hi".into(),
            from_session: "a".into(),
            message_id: "relay-1-2".into(),
            received_at: "2026-06-25T00:00:00.000Z".into(),
            to_session: Some("recip".into()),
            acked_at_ms: None,
            ttl_ms: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"to_session\":\"recip\""));
        assert!(!json.contains("acked_at_ms"), "None ack must be skipped");
        assert!(!json.contains("ttl_ms"), "None ttl must be skipped");
        assert_eq!(serde_json::from_str::<InboxRecord>(&json).unwrap(), rec);
    }

    #[test]
    fn relay_inbox_type_metadata() {
        assert!(CandidateType::RelayInbox.is_claude()); // under ~/.claude
        assert_eq!(CandidateType::RelayInbox.ext(), ".json");
        assert_eq!(CandidateType::RelayInbox.label(), "relay inbox");
        assert_eq!(type_token(CandidateType::RelayInbox), "relay-inbox");
    }

    #[test]
    fn iso_ms_round_trips() {
        let ms = 1_780_622_625_678;
        assert_eq!(parse_iso_ms(&iso_from_ms(ms)), Some(ms));
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(parse_iso_ms("not-a-date"), None);
    }

    #[test]
    fn move_to_trash_at_copy_then_unlink() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("relay-1-2.json");
        fs::write(&src, b"{\"text\":\"x\"}").unwrap();
        let trash = dir.path().join("trash");
        let ok = move_to_trash_at(
            &src,
            &trash,
            CandidateType::RelayInbox,
            "relay-1-2",
            "expired inbox message",
            12,
            0,
        );
        assert!(ok);
        // Original gone, copy + meta present (recoverable), copy bytes intact.
        assert!(!src.exists(), "original unlinked after copy landed");
        // The basename carries a per-call nonce (F1 fix), so discover the entry the
        // way --recover does: find the `_meta.json`, derive the data file by stripping
        // the suffix. The expected second-precision stamp must still prefix it.
        let stamp = iso_stamp_from(&iso_from_ms(0));
        let meta = fs::read_dir(&trash)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.file_name().unwrap().to_string_lossy().ends_with("_meta.json"))
            .expect("a _meta.json sidecar is present");
        let meta_name = meta.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            meta_name.starts_with(&format!("{stamp}_relay-1-2."))
                && meta_name.ends_with(".json_meta.json"),
            "trash name keeps the stamp + id, gains a nonce, and keeps the .json ext — got {meta_name}"
        );
        let base = meta_name.strip_suffix("_meta.json").unwrap();
        assert!(base.ends_with(".json"), "trash data file keeps its .json extension — got {base}");
        let trashed = trash.join(base);
        assert_eq!(fs::read(&trashed).unwrap(), b"{\"text\":\"x\"}");
        let m: TrashMeta = serde_json::from_str(&fs::read_to_string(&meta).unwrap()).unwrap();
        assert_eq!(m.kind, "relay-inbox");
        assert_eq!(m.reason, "expired inbox message");
    }
}
