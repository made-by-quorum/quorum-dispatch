//! WS-B / B1 — the relay-inbox ack+TTL+GC SCANNER + presence gate, shared by the
//! `qd gc` verb (the on-demand / backlog driver) and the relay server's 30 s
//! sweeper (the self-bounding live pass). ONE home, never a fork: both consume the
//! pure decider ([`crate::gc::inbox_is_collectible`]), the same recipient-protection
//! gate, and the same recoverable trash primitive ([`crate::gc::move_to_trash_at`]).
//!
//! Presence is **read-only** (R3c §4 / the SC-1↔WS-R boundary): this module reads
//! [`crate::presence`] and [`crate::recovery`] but never mutates the registry, fires
//! the ladder, or rewrites a recovery row. The gate is **fail-closed** — an outage /
//! indeterminate liveness protects the mail (loss is worse than a stale file).

use std::path::Path;

use crate::gc::{
    inbox_is_collectible, move_to_trash_at, parse_iso_ms, relative_time, Candidate, CandidateType,
    InboxRecord, INBOX_TTL_MS,
};
use crate::recovery::read_recovery_row;

/// Scan ONLY `inbox_dir` for collectible relay-inbox messages. This is the `.json`
/// extension-collision guard in its structural form (§3.1 T5): the SOLE scanner that
/// walks `inbox_dir`, and it walks nothing else, so an inbox `*.json` is never
/// confused with the `OcSidecar` class (whose scanner walks the OC servers dir).
/// Each record is judged by [`recipient_protected`] (presence) then the pure
/// [`inbox_is_collectible`] decider. `now_ms` is the injected driver clock.
pub fn scan_collectible(inbox_dir: &Path, state_dir: &Path, now_ms: i64) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(files) = std::fs::read_dir(inbox_dir) else {
        return out; // missing inbox dir → nothing (mirrors handle_inbox).
    };
    for f in files.flatten() {
        let path = f.path();
        // Only *.json inbox records (mirrors handle_inbox's filter, http.rs:587).
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        // Unparseable record → SKIP (never collect what we cannot judge — loss-safe).
        let Ok(rec) = serde_json::from_slice::<InboxRecord>(&bytes) else {
            continue;
        };
        let meta = std::fs::metadata(&path).ok();
        let mtime_ms = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        // `received_at` is the primary TTL clock; a malformed/absent stamp falls back
        // to the file mtime (the injected-mtime surface the other scans use), so the
        // ~2019 legacy files are robustly judged.
        let received_at_ms = parse_iso_ms(&rec.received_at).unwrap_or(mtime_ms);
        let ttl_ms = rec.ttl_ms.unwrap_or(INBOX_TTL_MS);
        let protected = recipient_protected(rec.to_session.as_deref(), state_dir, now_ms);
        if inbox_is_collectible(received_at_ms, rec.acked_at_ms, ttl_ms, now_ms, protected) {
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let identifier = if rec.message_id.is_empty() {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                rec.message_id.clone()
            };
            let reason = match rec.acked_at_ms {
                Some(_) => format!(
                    "delivered (acked), received {}",
                    relative_time(received_at_ms, now_ms)
                ),
                None => format!(
                    "undelivered, expired — received {}",
                    relative_time(received_at_ms, now_ms)
                ),
            };
            out.push(Candidate {
                kind: CandidateType::RelayInbox,
                path,
                reason,
                size,
                identifier,
            });
        }
    }
    out
}

/// Sweep `inbox_dir` once: scan + move every collectible message to recoverable
/// trash (copy-then-unlink) under `trash_dir`. Returns the count moved. Used by the
/// relay server's 30 s sweeper (throttled), presence-read-only; holds NO state lock
/// (it never touches relay state — pure file IO + presence reads). `now_ms` injected.
pub fn sweep_inbox_once(inbox_dir: &Path, state_dir: &Path, trash_dir: &Path, now_ms: i64) -> usize {
    let mut moved = 0;
    for c in scan_collectible(inbox_dir, state_dir, now_ms) {
        if move_to_trash_at(
            &c.path,
            trash_dir,
            c.kind,
            &c.identifier,
            &c.reason,
            c.size,
            now_ms,
        ) {
            moved += 1;
        }
    }
    moved
}

/// The recipient-protection gate (§3.2), FAIL-CLOSED. An ADDRESSED recipient that is
/// live, in active recovery (will return), OR has a live / recovering Rung-4
/// successor (the P8 `session-replaced` seam) is PROTECTED — its mail is never
/// collected regardless of age. [`crate::presence::is_live`] / `presence_of` are
/// themselves fail-closed (ENOENT ⇒ alive), so a presence outage protects the mail.
/// An UNADDRESSED record (no `to_session`, e.g. every legacy backlog file) is NOT
/// presence-protected — it is judged on TTL alone.
pub fn recipient_protected(to_session: Option<&str>, state_dir: &Path, now_ms: i64) -> bool {
    let Some(sid) = to_session else {
        return false; // unaddressable → TTL-only (the legacy backlog path)
    };
    if recipient_live_or_recovering(sid, state_dir, now_ms) {
        return true;
    }
    // Respawn mail-survival (P8 `session-replaced`): mail addressed to an OLD
    // session_id replaced by a Rung-4 respawn must reach the SUCCESSOR, not be
    // orphaned. If a recovery row records a minted successor, protect while THAT
    // successor is live/recovering — the message stays in the shared inbox, readable
    // by the successor on its next poll. (No on-disk recovery rows today ⇒ the seam
    // is wired but dormant; it activates the moment WS-R writes a successor row.)
    if let Some(row) = read_recovery_row(state_dir, sid) {
        if let Some(succ) = row.new_session_id.as_deref() {
            if recipient_live_or_recovering(succ, state_dir, now_ms) {
                return true;
            }
        }
    }
    false
}

/// Live now (flock fast-path) OR in the recovery ladder (`Running(_)` ⇒ "will
/// return"). Consumes the BANKED [`crate::presence`] surface read-only.
fn recipient_live_or_recovering(sid: &str, state_dir: &Path, now_ms: i64) -> bool {
    if crate::presence::is_live(state_dir, sid) {
        return true;
    }
    matches!(
        crate::presence::presence_of(state_dir, sid, now_ms).recovery,
        Some(crate::presence::LadderState::Running(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::iso_from_ms;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    const RECV_MS: i64 = 1_780_000_000_000;
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    fn write_inbox(dir: &Path, id: &str, json: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{id}.json")), json).unwrap();
    }

    /// A legacy backlog file: ONLY the 4 legacy fields (no to_session/acked_at_ms).
    fn legacy_json(id: &str, recv_ms: i64) -> String {
        format!(
            r#"{{"text":"Reply with exactly: RELAY_OK","from_session":"sender","message_id":"{id}","received_at":"{}"}}"#,
            iso_from_ms(recv_ms)
        )
    }

    #[test]
    fn scan_collects_expired_legacy_keeps_fresh() {
        let tmp = tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let state = tmp.path().join("state");
        write_inbox(&inbox, "relay-old", &legacy_json("relay-old", RECV_MS));
        write_inbox(
            &inbox,
            "relay-new",
            &legacy_json("relay-new", RECV_MS + 13 * DAY_MS),
        );
        let now = RECV_MS + 14 * DAY_MS; // old is 14d (>7d TTL); new is 1d
        let cands = scan_collectible(&inbox, &state, now);
        assert_eq!(cands.len(), 1, "only the >7d legacy file collects");
        assert_eq!(cands[0].kind, CandidateType::RelayInbox);
        assert_eq!(cands[0].identifier, "relay-old");
    }

    #[test]
    fn scan_skips_unparseable() {
        let tmp = tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        write_inbox(&inbox, "garbage", "this is not json {{{");
        let cands = scan_collectible(&inbox, &tmp.path().join("state"), RECV_MS + 999 * DAY_MS);
        assert!(
            cands.is_empty(),
            "an unparseable record is never collected (loss-safe)"
        );
    }

    #[test]
    fn scan_failclosed_protects_addressed_recipient() {
        // An ADDRESSED 14d message with NO presence data on disk → is_live is
        // fail-closed ALIVE → protected → NOT collected (the no-loss keystone).
        let tmp = tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let addressed = format!(
            r#"{{"text":"x","from_session":"s","message_id":"relay-addr","received_at":"{}","to_session":"recip"}}"#,
            iso_from_ms(RECV_MS)
        );
        write_inbox(&inbox, "relay-addr", &addressed);
        let now = RECV_MS + 14 * DAY_MS;
        assert!(
            scan_collectible(&inbox, &state, now).is_empty(),
            "fail-closed presence protects an addressed recipient's mail"
        );
        // Negative control: the SAME record UNADDRESSED (legacy) IS collected — so it
        // is the presence gate, not age, that spared the addressed file.
        fs::remove_file(inbox.join("relay-addr.json")).unwrap();
        write_inbox(&inbox, "relay-addr", &legacy_json("relay-addr", RECV_MS));
        assert_eq!(
            scan_collectible(&inbox, &state, now).len(),
            1,
            "unaddressed expired legacy file collects (presence gate is load-bearing)"
        );
    }

    #[test]
    fn scan_protects_acked_within_grace_collects_after() {
        let tmp = tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let state = tmp.path().join("state");
        let now = RECV_MS + 14 * DAY_MS;
        // Unaddressed but ACKED 10 min ago → within the 1h grace → protected.
        let acked_recent = format!(
            r#"{{"text":"x","from_session":"s","message_id":"relay-ack","received_at":"{}","acked_at_ms":{}}}"#,
            iso_from_ms(RECV_MS),
            now - 10 * 60 * 1000
        );
        write_inbox(&inbox, "relay-ack", &acked_recent);
        assert!(
            scan_collectible(&inbox, &state, now).is_empty(),
            "acked within grace held"
        );
        // Acked 2h ago → past the 1h grace → collected.
        fs::remove_file(inbox.join("relay-ack.json")).unwrap();
        let acked_old = format!(
            r#"{{"text":"x","from_session":"s","message_id":"relay-ack","received_at":"{}","acked_at_ms":{}}}"#,
            iso_from_ms(RECV_MS),
            now - 2 * 60 * 60 * 1000
        );
        write_inbox(&inbox, "relay-ack", &acked_old);
        assert_eq!(
            scan_collectible(&inbox, &state, now).len(),
            1,
            "acked past grace collected"
        );
    }

    #[test]
    fn respawn_successor_seam_protects_then_releases() {
        // Mail addressed to an OLD session id that a Rung-4 respawn replaced: while
        // the recovery row records a successor, the OLD id resolves through it. The
        // successor's liveness is fail-closed (empty state → alive) so the mail is
        // protected (reaches the successor via the shared inbox), not orphaned.
        use crate::recovery::{write_recovery_row, Identity, LadderState, RecoveryRow};
        let tmp = tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let old = format!(
            r#"{{"text":"x","from_session":"s","message_id":"relay-old-id","received_at":"{}","to_session":"old-sess"}}"#,
            iso_from_ms(RECV_MS)
        );
        write_inbox(&inbox, "relay-old-id", &old);
        let now = RECV_MS + 14 * DAY_MS;
        // A recovery row for old-sess recording a minted successor.
        let mut row = RecoveryRow::cold("old-sess");
        row.ladder_state = LadderState::Idle;
        row.old_identity = Some(Identity {
            pid: 999_999,
            start_ms: 1,
            incarnation: 3,
        });
        row.new_session_id = Some("succ-sess".to_string());
        write_recovery_row(&state, &row).unwrap();
        // Successor protected (fail-closed alive) → the old-addressed mail survives.
        assert!(
            scan_collectible(&inbox, &state, now).is_empty(),
            "respawn-successor seam protects mail across a Rung-4 replacement"
        );
    }

    #[test]
    fn sweep_inbox_once_moves_to_trash_recoverable() {
        let tmp = tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let state = tmp.path().join("state");
        let trash = tmp.path().join("trash");
        write_inbox(&inbox, "relay-dead", &legacy_json("relay-dead", RECV_MS));
        let now = RECV_MS + 14 * DAY_MS;
        let moved = sweep_inbox_once(&inbox, &state, &trash, now);
        assert_eq!(moved, 1);
        // Original gone from the inbox, recoverable copy + meta in trash.
        assert!(!inbox.join("relay-dead.json").exists());
        let trashed: Vec<PathBuf> = fs::read_dir(&trash)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        // One trashed file + its _meta.json sidecar.
        assert_eq!(trashed.len(), 2, "trashed file + meta sidecar present");
    }
}
