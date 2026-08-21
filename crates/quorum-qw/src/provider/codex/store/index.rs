//! READ-ONLY best-effort cache over the codex `$CODEX_HOME/state_5.sqlite`
//! `threads` table (codex-p2-spec sections 6.3, 6.4).
//!
//! **A CACHE, NEVER A CONTRACT (L8 / codex-p2-spec section 6.3).** The sqlite
//! index is codex's own thread metadata store; qd reads it best-effort to enrich
//! ls/info (title, cwd, rollout_path) and to resolve a thread's rollout_path
//! directly. EVERY failure — missing file, schema drift (a renamed/dropped
//! column), a query error, wrong column types — returns EMPTY / `None` so the
//! caller falls back to the rollout-dir scan. Nothing here may error out a caller
//! (cut-ladder rung C2: if this whole layer is cut, callers degrade to
//! rollout-only metadata).
//!
//! Opened with `SQLITE_OPEN_READ_ONLY` so qd never writes/migrates codex's db.
//!
//! **The REAL schema (mined read-only from the jail evidence,
//! `exec/codex-p2-evidence/rss/rss-jail/codex-home/state_5.sqlite` and
//! `exec/codex-spike-evidence/jail/codex-home/state_5.sqlite`, schema version
//! `_5`):**
//!
//! ```sql
//! CREATE TABLE threads (
//!     id TEXT PRIMARY KEY,
//!     rollout_path TEXT NOT NULL,
//!     created_at INTEGER NOT NULL,
//!     updated_at INTEGER NOT NULL,
//!     source TEXT NOT NULL,
//!     model_provider TEXT NOT NULL,
//!     cwd TEXT NOT NULL,
//!     title TEXT NOT NULL,
//!     sandbox_policy TEXT NOT NULL,
//!     approval_mode TEXT NOT NULL,
//!     tokens_used INTEGER NOT NULL DEFAULT 0,
//!     ...                       -- many later-added columns we do NOT read
//! );
//! ```
//!
//! We SELECT only the columns we consume (`id, rollout_path, cwd, title,
//! updated_at`) BY NAME — so the many trailing columns codex has added across
//! migrations (cli_version, first_user_message, preview, model, …) are irrelevant
//! and a future drop of one of THOSE does not break us. A drop/rename of one of
//! OUR five columns is a query error → degrade to empty.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// One row of the codex `threads` index (the subset qd consumes). All fields are
/// best-effort: a wrong-typed column makes the WHOLE row drop (the caller falls
/// back to the rollout scan for that thread).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    /// The thread uuidv7 (PRIMARY KEY) — the SessionKey id (m2).
    pub id: String,
    /// The absolute rollout JSONL path (date+uuid keyed).
    pub rollout_path: String,
    /// The thread's working dir.
    pub cwd: String,
    /// The human title (codex derives it from the first user message).
    pub title: String,
    /// Last-update epoch SECONDS (codex stores both `updated_at` (s) and
    /// `updated_at_ms`; we read the seconds column).
    pub updated_at: i64,
}

/// The sqlite filename under a codex home (`$CODEX_HOME/state_5.sqlite`).
pub const STATE_DB_FILENAME: &str = "state_5.sqlite";

/// Read every `threads` row from `<codex_home>/state_5.sqlite`, READ-ONLY,
/// best-effort (codex-p2-spec section 6.4). `codex_home` is the dir that contains
/// `state_5.sqlite` AND the `sessions/` rollout tree (i.e. `$CODEX_HOME`).
///
/// Returns an EMPTY vec on ANY failure (missing file, not-a-db, schema drift,
/// query error) — the caller falls back to the rollout-dir scan. NEVER errors.
pub fn threads(codex_home: &Path) -> Vec<ThreadRow> {
    let db_path = codex_home.join(STATE_DB_FILENAME);
    read_threads(&db_path).unwrap_or_default()
}

/// The fallible inner read — every `?` short-circuits to the empty-vec degrade in
/// [`threads`]. Read-only open; SELECT only the consumed columns by name.
fn read_threads(db_path: &Path) -> Option<Vec<ThreadRow>> {
    // Missing file or a non-sqlite blob → open/exec fails → None → empty.
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // SELECT only OUR columns by name — trailing migration-added columns are
    // irrelevant; a drop/rename of one of these five is a prepare error → None.
    let mut stmt = conn
        .prepare("SELECT id, rollout_path, cwd, title, updated_at FROM threads")
        .ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ThreadRow {
                id: row.get(0)?,
                rollout_path: row.get(1)?,
                cwd: row.get(2)?,
                title: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })
        .ok()?;
    // A per-row type error (e.g. a NULL in a NOT-NULL-typed column) drops THAT
    // row but keeps the rest — flatten the Results, skipping the bad ones.
    Some(rows.flatten().collect())
}

/// Resolve a single thread's `rollout_path` from the index (codex-p2-spec section
/// 6.4 transcript_path tier 1). Best-effort: `None` on any failure OR a missing
/// id, so the caller falls back to the date-walk filename match.
pub fn rollout_path_for(codex_home: &Path, thread_id: &str) -> Option<String> {
    threads(codex_home)
        .into_iter()
        .find(|r| r.id == thread_id)
        .map(|r| r.rollout_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Mint a real sqlite db at `<dir>/state_5.sqlite` with the REAL `_5` schema
    /// (the exact CREATE TABLE mined from the jail evidence) + the supplied rows.
    /// Each row is `(id, rollout_path, cwd, title, updated_at)`; the other
    /// NOT-NULL columns get harmless defaults so the INSERT satisfies the schema.
    fn mint_db(dir: &TempDir, rows: &[(&str, &str, &str, &str, i64)]) -> std::path::PathBuf {
        let path = dir.path().join(STATE_DB_FILENAME);
        let conn = Connection::open(&path).unwrap();
        // The REAL _5 schema (mined read-only from the jail db, columns through
        // the migration tail abbreviated to the NOT-NULL set we must satisfy).
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                model_provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                title TEXT NOT NULL,
                sandbox_policy TEXT NOT NULL,
                approval_mode TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                cli_version TEXT NOT NULL DEFAULT '',
                first_user_message TEXT NOT NULL DEFAULT '',
                preview TEXT NOT NULL DEFAULT ''
            );",
        )
        .unwrap();
        for (id, rollout_path, cwd, title, updated_at) in rows {
            conn.execute(
                "INSERT INTO threads
                 (id, rollout_path, created_at, updated_at, source, model_provider,
                  cwd, title, sandbox_policy, approval_mode)
                 VALUES (?1, ?2, ?3, ?4, 'vscode', 'openrouter', ?5, ?6,
                         '{\"type\":\"read-only\"}', 'never')",
                rusqlite::params![id, rollout_path, updated_at, updated_at, cwd, title],
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn reads_real_schema_rows() {
        let tmp = TempDir::new().unwrap();
        // The real row shape (cited from the rss-jail db row).
        mint_db(
            &tmp,
            &[(
                "019ea0b3-04d3-7400-8d95-f55d41e961e4",
                "/jail/codex-home/sessions/2026/06/07/rollout-2026-06-07T02-09-07-019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl",
                "/jail/work",
                "Reply with exactly the text RSS-PROBE and nothing else.",
                1780812577,
            )],
        );
        let rows = threads(tmp.path());
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.id, "019ea0b3-04d3-7400-8d95-f55d41e961e4");
        assert!(r
            .rollout_path
            .ends_with("rollout-2026-06-07T02-09-07-019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl"));
        assert_eq!(r.cwd, "/jail/work");
        assert_eq!(
            r.title,
            "Reply with exactly the text RSS-PROBE and nothing else."
        );
        assert_eq!(r.updated_at, 1780812577);
    }

    #[test]
    fn rollout_path_for_resolves_then_misses() {
        let tmp = TempDir::new().unwrap();
        mint_db(
            &tmp,
            &[
                ("id-a", "/r/a.jsonl", "/w/a", "A", 10),
                ("id-b", "/r/b.jsonl", "/w/b", "B", 20),
            ],
        );
        assert_eq!(
            rollout_path_for(tmp.path(), "id-b").as_deref(),
            Some("/r/b.jsonl")
        );
        // A thread not in the index → None (caller date-walks).
        assert_eq!(rollout_path_for(tmp.path(), "id-missing"), None);
    }

    // === DEGRADE tests (codex-p2-spec section 13: EVERY failure → empty, NEVER an
    // error escape — a hard error here would red the degrade tests) ===

    // MUTATION EVIDENCE (codex-p2-spec section 13 "sqlite degrade removal"): if
    // `threads` propagated the open/query error instead of degrading to empty,
    // these tests red (a missing file / garbage blob / schema drift would panic or
    // bubble). NAMED.
    #[test]
    fn missing_db_is_empty_not_error() {
        let tmp = TempDir::new().unwrap();
        // No state_5.sqlite minted.
        assert!(threads(tmp.path()).is_empty());
        assert_eq!(rollout_path_for(tmp.path(), "anything"), None);
    }

    #[test]
    fn not_a_db_garbage_file_is_empty() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(STATE_DB_FILENAME),
            b"this is not a sqlite database at all \x00\xff",
        )
        .unwrap();
        assert!(
            threads(tmp.path()).is_empty(),
            "a non-db blob degrades to empty"
        );
    }

    #[test]
    fn missing_column_schema_drift_is_empty() {
        // A db whose threads table is missing one of OUR five columns (here
        // `title`) → the SELECT prepare fails → degrade to empty (schema drift).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(STATE_DB_FILENAME);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO threads (id, rollout_path, cwd, updated_at)
            VALUES ('x', '/r/x.jsonl', '/w/x', 1);",
        )
        .unwrap();
        drop(conn);
        assert!(
            threads(tmp.path()).is_empty(),
            "a dropped consumed column (schema drift) degrades to empty"
        );
    }

    #[test]
    fn no_threads_table_is_empty() {
        // A valid sqlite db that simply has no `threads` table (e.g. a different
        // schema version) → prepare fails → empty.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(STATE_DB_FILENAME);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE other (x INTEGER);")
            .unwrap();
        drop(conn);
        assert!(threads(tmp.path()).is_empty());
    }
}
