//! Identity tombstones — qd-OWNED preservation of a transport-lost
//! `acp/claude-code` session's identity (Child D, opencode D1; clerk-4's Arm-B
//! ratification, bond note 01KX01BY7G: refuse-and-surface with identity
//! PRESERVED).
//!
//! **Why this store exists.** An `acp/claude-code` session's only durable
//! identity row is `~/.claude/sessions/<pid>.json` — a store the claude CLI
//! itself owns and janitors: a dead-pid row is reaped ~1s after the daemon
//! dies (Child C, live-observed 3/3 runs; qd's send tree makes ZERO
//! unlink/rename on the row — strace-attested). A transport-lost session is
//! dead-pid BY DEFINITION, so anything qd persists there is structurally
//! unpersistable. When a verb detects transport loss and refuses (the
//! named-divergence disposition), it first records the session's identity
//! HERE — under `state_dir` (`<QD_HOME || ~/.quorum/dispatch>/state`), which
//! no claude-CLI janitor ever touches — so a human can still find the
//! session_id/transcript and recover by hand (`qd resume`, `claude --resume`).
//!
//! **Store shape: per-session atomic JSON**,
//! `<state_dir>/tombstones/<session_id>.json` — the `recovery.rs` precedent
//! (per-session_id file, tmp+rename atomic write, permissive corrupt⇒None
//! read), chosen over the `idstore.rs` flocked-JSONL precedent because:
//! - the record is a per-session "identity as of the last observed loss" fact,
//!   not an event history — latest-wins overwrite is the correct semantics for
//!   repeated losses (identity fields are stable; only reason/timestamp move,
//!   and `first_recorded_at_ms` is carried forward so the earliest loss stays
//!   visible);
//! - concurrent writers (a send and a wait racing on the same lost session)
//!   each rename a complete record into place — no interleaving, no lock to
//!   contend, a reader never sees a torn file;
//! - discoverability: one `cat`-able file per session, named by the
//!   session_id a user already sees in `qd ls`.
//!
//! **Scope.** Written ONLY for provider `acp/claude-code`, only on transport
//! LOSS (dead endpoint at entry / death across the probe→connect boundary,
//! per the re-probe's `TransportLost` classification / mid-flight
//! `NoTransport` / lost wait channel) — never for healthy sessions, never for
//! codex/opencode (their refusal
//! behavior is byte-identical to before and writes nothing), and never on a
//! wedged-but-alive daemon (its registry row is live-pid — the janitor does
//! not reap it, so nothing is at risk).
//!
//! **Not the registry tombstone.** `qd kill`'s registry "tombstone" is a
//! RENAME of the claude-owned row to `<pid>.json.tombstoned`
//! (`registry.rs::tombstone`) — a deliberate kill marker in the janitored
//! store. This module is unrelated: a qd-owned identity record that survives
//! that whole store's lifecycle.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version stamped into every record (`v`).
pub const TOMBSTONE_VERSION: u32 = 1;

/// Everything a human (or a future revival path) needs to find and recover a
/// transport-lost session after the claude CLI's janitor has reaped its
/// registry row. Missing/extra fields never fail a parse (`serde(default)` —
/// the registry's L8 permissive-read posture).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentityTombstone {
    /// Schema version ([`TOMBSTONE_VERSION`]).
    pub v: u32,
    /// The provider session UUID (ACP sessionId) — the key `claude --resume`
    /// and `qd resume` recovery act on, and this record's filename.
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The dead daemon's pid — forensic only (the pid is gone; never probe it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Always `acp/claude-code` today (the write scope) — recorded anyway so
    /// the file is self-describing.
    pub provider: String,
    /// The recorded ACP endpoint (`ws://…`) at loss time, if the registry row
    /// still held one when the loss was observed (the janitor may already have
    /// reaped it — then `None`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The session's transcript JSONL path, when derivable — the recovery
    /// artifact a human most wants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    /// Human-readable description of the observed loss (which verb, which lane).
    pub loss_reason: String,
    /// Epoch ms of the FIRST recorded loss for this session — carried forward
    /// across repeated losses so the original event stays visible.
    pub first_recorded_at_ms: i64,
    /// Epoch ms of the most recent recorded loss (this write).
    pub recorded_at_ms: i64,
}

impl Default for IdentityTombstone {
    fn default() -> Self {
        IdentityTombstone {
            v: TOMBSTONE_VERSION,
            session_id: String::new(),
            name: None,
            pid: None,
            cwd: None,
            provider: String::new(),
            endpoint: None,
            transcript: None,
            loss_reason: String::new(),
            first_recorded_at_ms: 0,
            recorded_at_ms: 0,
        }
    }
}

/// The qd-owned tombstone directory: `<state_dir>/tombstones`.
pub fn tombstone_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("tombstones")
}

/// The record path for one session: `<state_dir>/tombstones/<session_id>.json`.
/// The session_id is filename-sanitized ([`sanitize_id`]) — defensive only; a
/// real ACP sessionId is a UUID and passes through unchanged.
pub fn tombstone_path(state_dir: &Path, session_id: &str) -> PathBuf {
    tombstone_dir(state_dir).join(format!("{}.json", sanitize_id(session_id)))
}

/// Filename guard: keep `[A-Za-z0-9._-]`, replace anything else (path
/// separators included) with `_`, and never yield an empty/dot-only name. A
/// genuine session UUID is untouched; a hostile or corrupt id cannot escape
/// the tombstone dir.
fn sanitize_id(session_id: &str) -> String {
    let cleaned: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// Read a session's tombstone. Missing file ⇒ `None`; a CORRUPT/unparseable
/// record (torn pre-atomic-write crash residue, older schema) ⇒ `None` — the
/// `recovery.rs` permissive-read posture, so a corrupt record can always be
/// overwritten by the next loss, never wedge a verb.
pub fn read_tombstone(state_dir: &Path, session_id: &str) -> Option<IdentityTombstone> {
    let bytes = std::fs::read(tombstone_path(state_dir, session_id)).ok()?;
    serde_json::from_slice::<IdentityTombstone>(&bytes).ok()
}

/// Current wall-clock epoch ms — the bin verbs' timestamp source for
/// [`record_loss`] (tests pass explicit values instead).
pub fn wall_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Record (or refresh) a session's identity tombstone: latest-wins atomic
/// overwrite, carrying `first_recorded_at_ms` forward from any existing
/// readable record so repeated losses never erase when the session was first
/// lost. `now_ms` stamps `recorded_at_ms` (and `first_recorded_at_ms` on the
/// first write). Returns the record's path.
///
/// Write discipline: `create_dir_all` + tmp+rename in the tombstone dir (the
/// `recovery.rs` / `registry.rs:583` atomic idiom) — a reader (or a racing
/// second writer) never observes a torn record.
pub fn record_loss(
    state_dir: &Path,
    mut record: IdentityTombstone,
    now_ms: i64,
) -> io::Result<PathBuf> {
    record.v = TOMBSTONE_VERSION;
    record.recorded_at_ms = now_ms;
    record.first_recorded_at_ms = read_tombstone(state_dir, &record.session_id)
        .map(|prior| prior.first_recorded_at_ms)
        .filter(|&ms| ms > 0)
        .unwrap_or(now_ms);

    let dir = tombstone_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let final_path = tombstone_path(state_dir, &record.session_id);
    let tmp = dir.join(format!(
        ".{}.json.tmp.{}",
        sanitize_id(&record.session_id),
        std::process::id()
    ));
    let bytes = serde_json::to_vec_pretty(&record).map_err(io::Error::other)?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &final_path)?;
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample(session_id: &str) -> IdentityTombstone {
        IdentityTombstone {
            session_id: session_id.to_string(),
            name: Some("conf-d-a1".to_string()),
            pid: Some(4242),
            cwd: Some("/work/proj".to_string()),
            provider: "acp/claude-code".to_string(),
            endpoint: Some("ws://127.0.0.1:9999".to_string()),
            transcript: Some("/home/u/.claude/projects/x/uuid.jsonl".to_string()),
            loss_reason: "acp endpoint not reachable at send entry".to_string(),
            ..IdentityTombstone::default()
        }
    }

    #[test]
    fn record_and_read_round_trip() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path();
        let path = record_loss(state_dir, sample("sess-uuid-1"), 1_000).unwrap();
        assert_eq!(path, tombstone_path(state_dir, "sess-uuid-1"));

        let read = read_tombstone(state_dir, "sess-uuid-1").expect("record readable");
        assert_eq!(read.v, TOMBSTONE_VERSION);
        assert_eq!(read.session_id, "sess-uuid-1");
        assert_eq!(read.name.as_deref(), Some("conf-d-a1"));
        assert_eq!(read.pid, Some(4242));
        assert_eq!(read.provider, "acp/claude-code");
        assert_eq!(read.endpoint.as_deref(), Some("ws://127.0.0.1:9999"));
        assert_eq!(read.first_recorded_at_ms, 1_000);
        assert_eq!(read.recorded_at_ms, 1_000);
    }

    #[test]
    fn repeated_loss_keeps_first_recorded_and_refreshes_latest() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path();
        record_loss(state_dir, sample("sess-uuid-1"), 1_000).unwrap();

        let mut second = sample("sess-uuid-1");
        second.loss_reason = "acp transport lost mid-flight".to_string();
        record_loss(state_dir, second, 5_000).unwrap();

        let read = read_tombstone(state_dir, "sess-uuid-1").unwrap();
        assert_eq!(
            read.first_recorded_at_ms, 1_000,
            "the FIRST loss timestamp survives a repeated loss"
        );
        assert_eq!(read.recorded_at_ms, 5_000, "the latest loss is reflected");
        assert_eq!(read.loss_reason, "acp transport lost mid-flight");
    }

    #[test]
    fn corrupt_record_reads_none_and_is_overwritable() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path();
        std::fs::create_dir_all(tombstone_dir(state_dir)).unwrap();
        std::fs::write(tombstone_path(state_dir, "sess-x"), b"{ torn garbage").unwrap();

        assert!(read_tombstone(state_dir, "sess-x").is_none(), "corrupt ⇒ None, never an error");

        // The next loss overwrites the corrupt record cleanly (first_recorded
        // restarts — the corrupt record carried no readable history).
        record_loss(state_dir, sample("sess-x"), 7_000).unwrap();
        let read = read_tombstone(state_dir, "sess-x").unwrap();
        assert_eq!(read.first_recorded_at_ms, 7_000);
    }

    #[test]
    fn missing_record_reads_none() {
        let tmp = TempDir::new().unwrap();
        assert!(read_tombstone(tmp.path(), "never-written").is_none());
    }

    #[test]
    fn hostile_session_id_cannot_escape_the_tombstone_dir() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path();
        let path = record_loss(state_dir, sample("../../etc/passwd"), 1_000).unwrap();
        assert!(
            path.starts_with(tombstone_dir(state_dir)),
            "sanitized path stays inside the tombstone dir: {path:?}"
        );
        // And the sanitized name round-trips through read.
        assert!(read_tombstone(state_dir, "../../etc/passwd").is_some());
    }

    #[test]
    fn empty_session_id_gets_a_stable_fallback_name() {
        let tmp = TempDir::new().unwrap();
        let path = tombstone_path(tmp.path(), "");
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "unnamed.json");
    }

    #[test]
    fn unknown_extra_fields_do_not_fail_the_parse() {
        // Forward-compat (the L8 posture): a future writer's extra field must
        // not make an old reader drop the whole record.
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path();
        std::fs::create_dir_all(tombstone_dir(state_dir)).unwrap();
        std::fs::write(
            tombstone_path(state_dir, "sess-f"),
            br#"{"v":1,"session_id":"sess-f","provider":"acp/claude-code","loss_reason":"x","first_recorded_at_ms":1,"recorded_at_ms":2,"some_future_field":true}"#,
        )
        .unwrap();
        let read = read_tombstone(state_dir, "sess-f").expect("extra fields tolerated");
        assert_eq!(read.session_id, "sess-f");
    }
}
