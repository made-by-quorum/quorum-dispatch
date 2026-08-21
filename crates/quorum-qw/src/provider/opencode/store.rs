//! READ-ONLY best-effort store reader over OpenCode's monolithic
//! `opencode.db` (lsview A3; built from R1's scanner contract,
//! `findings/R1-opencode-store.md`, authored against the real brano store,
//! OpenCode 1.15.5).
//!
//! **A BEST-EFFORT READER, NEVER A CONTRACT (L8, the codex `index.rs`
//! precedent).** OpenCode persists ALL sessions as rows in ONE SQLite database
//! at `${XDG_DATA_HOME:-$HOME/.local/share}/opencode/opencode.db` — there is NO
//! per-session file tree (the `storage/` dir is vestigial; R1 §1.2). qd reads it
//! read-only to surface cold OpenCode rows in `qd ls`. EVERY failure — a missing
//! db, a non-sqlite blob, a missing `session` table, a wrong-typed column, a
//! query error — degrades to an EMPTY result. Nothing here may error out the
//! `ls` (the permissive-degrade standard A3 is held to; R1 §5, §7).
//!
//! Opened with `SQLITE_OPEN_READ_ONLY` (the `?mode=ro` equivalent) so qd never
//! writes/migrates OpenCode's db. We do **NOT** open `immutable=1`: that silently
//! drops WAL-resident rows (R1 §7 measured 15 with the WAL vs 14 without), and a
//! read-only open applies the `-wal` so every committed row is seen. The only
//! side effect is bumping the regenerable `opencode.db-shm` mtime (R1 §0 — no
//! data harm).
//!
//! **Enumerate + stats in ONE query (R1 §4).** Unlike claude (one file slurp per
//! transcript), OpenCode's per-session stats are pre-aggregated COLUMNS on the
//! `session` row, so the whole store is read in a single indexed query over a
//! ~330 KB db — far cheaper than a single claude transcript read. `turns` counts
//! the `message` table (NOT `session_message`, which holds agent/model-switch
//! events — a documented red herring, R1 §3.3).
//!
//! We SELECT only the columns we consume BY NAME; OpenCode's many other columns
//! (and any migration-added ones) are irrelevant, and a drop/rename of one of
//! OURS is a prepare error → degrade to empty (schema-drift safety, the codex
//! posture).

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::effects::Env;

/// The monolithic session db under the OpenCode store dir.
pub const OPENCODE_DB_FILENAME: &str = "opencode.db";

/// The provider id an OpenCode qd row carries (the harness identity — R1's
/// scanner contract "Provider identity"). NOT the per-session `model.providerID`
/// (openrouter/anthropic/…), which is the LLM vendor, metadata only.
pub const PROVIDER_ID: &str = "opencode";

/// One row of the OpenCode `session` table (the subset qd consumes). All fields
/// are best-effort: a wrong-typed column makes the WHOLE row drop (the caller
/// simply does not surface that session). Raw columns — the row-level derivation
/// (name fallback, cumulative-token sum, ms timestamps) happens in the gather
/// step, keeping this reader a faithful transcription of the store.
#[derive(Debug, Clone, PartialEq)]
pub struct OpencodeSession {
    /// `session.id` — `ses_<26 base62>` (PRIMARY KEY).
    pub id: String,
    /// `session.directory` — the session's absolute cwd (R1 §2, corroborated by
    /// `message.data.path.cwd`).
    pub directory: String,
    /// `session.title` — human/auto title (default `"New session - <ISO>"`).
    pub title: String,
    /// `session.slug` — the secondary friendly slug (e.g. `nimble-nebula`); the
    /// fallback when `title` is empty.
    pub slug: String,
    /// `session.time_updated` — epoch MILLISECONDS; the "last activity" signal
    /// (the only timestamp a cold row renders — `lastActive`; R1 §2).
    pub time_updated_ms: i64,
    /// `session.tokens_input` — cumulative input tokens over the session (R1 §3.2).
    pub tokens_input: i64,
    /// `session.tokens_cache_read` — cumulative cache-read tokens.
    pub tokens_cache_read: i64,
    /// `session.tokens_cache_write` — cumulative cache-write tokens.
    pub tokens_cache_write: i64,
    /// `count(*) FROM message WHERE session_id = id` — turns (user+assistant
    /// rows), counted from `message`, never `session_message` (R1 §3.1, §3.3).
    pub turns: i64,
}

/// Resolve the OpenCode store dir off `fx.env` ONLY (L9a — never raw
/// `std::env`), mirroring how codex resolves `$CODEX_HOME` and pi reads
/// `$PI_CODING_AGENT_SESSION_DIR`. `${XDG_DATA_HOME:-$HOME/.local/share}/opencode`
/// (R1 §1.1, proven both directions). `None` when neither `XDG_DATA_HOME` nor
/// `HOME` is set (a caller with no home cannot resolve a root → the union treats
/// it as a clean zero).
pub fn store_dir(env: &dyn Env) -> Option<PathBuf> {
    let data_home = match env.var("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => {
            let home = env.var("HOME").filter(|h| !h.is_empty())?;
            PathBuf::from(home).join(".local").join("share")
        }
    };
    Some(data_home.join("opencode"))
}

/// Read every `session` row from `<store_dir>/opencode.db`, READ-ONLY,
/// best-effort. Returns an EMPTY vec on ANY failure or when the db is absent —
/// NEVER errors, never opens an absent db (the zero-shape gate, R1 §5: stat
/// first; Z1/Z2 absent → clean zero without opening; Z3 valid-empty returns 0
/// naturally; Z4 garbage/no-`session`-table → prepare fails → empty).
pub fn sessions(store_dir: &Path) -> Vec<OpencodeSession> {
    let db_path = store_dir.join(OPENCODE_DB_FILENAME);
    // Zero-shape gate: a missing db (store-absent or root-present-no-db) is a
    // clean zero — do not open (R1 §5 Z1/Z2). `exists()` is a stat; a race that
    // removes it before the open just degrades to empty below.
    if !db_path.exists() {
        return Vec::new();
    }
    read_sessions(&db_path).unwrap_or_default()
}

/// The fallible inner read — every `?` short-circuits to the empty-vec degrade in
/// [`sessions`]. Read-only open (NOT `immutable=1`, R1 §7); SELECT only the
/// consumed columns by name.
fn read_sessions(db_path: &Path) -> Option<Vec<OpencodeSession>> {
    // A non-sqlite blob (Z4 garbage) → open or prepare fails → None → empty.
    // READ_ONLY is the `?mode=ro` equivalent: applies the `-wal`, sees every
    // committed row, never writes the data files.
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // ONE query: enumerate + per-session stats. `turns` is a correlated subquery
    // over `message` (never `session_message`). A missing `session`/`message`
    // table or a dropped consumed column → prepare error → None → empty.
    let mut stmt = conn
        .prepare(
            "SELECT id, directory, title, slug, time_updated, \
                    tokens_input, tokens_cache_read, tokens_cache_write, \
                    (SELECT count(*) FROM message m WHERE m.session_id = session.id) AS turns \
             FROM session \
             ORDER BY time_updated DESC",
        )
        .ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok(OpencodeSession {
                id: row.get(0)?,
                directory: row.get(1)?,
                title: row.get(2)?,
                slug: row.get(3)?,
                time_updated_ms: row.get(4)?,
                tokens_input: row.get(5)?,
                tokens_cache_read: row.get(6)?,
                tokens_cache_write: row.get(7)?,
                turns: row.get(8)?,
            })
        })
        .ok()?;
    // A per-row type error (e.g. a NULL in a column we read as a value) drops
    // THAT row but keeps the rest — the permissive posture (R1 §7): a
    // malformed/partial session is skipped, never fatal.
    Some(rows.flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::Env;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// A test `Env` backed by a fixed map (no raw `std::env`, L9a).
    struct MapEnv(HashMap<String, String>);
    impl Env for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn uid(&self) -> u32 {
            0
        }
    }
    fn env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    /// The columns of the real OpenCode `session` table this reader consumes,
    /// as a minimal-but-faithful CREATE (the NOT-NULL shape mined in R1 §4.1),
    /// plus the `message` table `turns` counts. Extra real columns are omitted;
    /// we SELECT by name so their absence is irrelevant.
    fn mint_store(dir: &TempDir, sessions: &[SessionSeed], messages: &[(&str, &str)]) -> PathBuf {
        let db_path = dir.path().join(OPENCODE_DB_FILENAME);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL DEFAULT 'global',
                slug TEXT NOT NULL,
                directory TEXT NOT NULL,
                title TEXT NOT NULL,
                version TEXT NOT NULL DEFAULT '1.15.5',
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                agent TEXT,
                model TEXT,
                cost REAL NOT NULL DEFAULT 0,
                tokens_input INTEGER NOT NULL DEFAULT 0,
                tokens_output INTEGER NOT NULL DEFAULT 0,
                tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                tokens_cache_write INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL
            );",
        )
        .unwrap();
        for s in sessions {
            conn.execute(
                "INSERT INTO session
                 (id, slug, directory, title, version, time_created, time_updated,
                  tokens_input, tokens_cache_read, tokens_cache_write)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    s.id,
                    s.slug,
                    s.directory,
                    s.title,
                    s.version,
                    s.time_created_ms,
                    s.time_updated_ms,
                    s.tokens_input,
                    s.tokens_cache_read,
                    s.tokens_cache_write
                ],
            )
            .unwrap();
        }
        for (mid, sid) in messages {
            conn.execute(
                "INSERT INTO message (id, session_id) VALUES (?1, ?2)",
                rusqlite::params![mid, sid],
            )
            .unwrap();
        }
        db_path
    }

    struct SessionSeed {
        id: &'static str,
        slug: &'static str,
        directory: &'static str,
        title: &'static str,
        version: &'static str,
        time_created_ms: i64,
        time_updated_ms: i64,
        tokens_input: i64,
        tokens_cache_read: i64,
        tokens_cache_write: i64,
    }
    fn seed(id: &'static str, dir: &'static str, title: &'static str, tin: i64) -> SessionSeed {
        SessionSeed {
            id,
            slug: "nimble-nebula",
            directory: dir,
            title,
            version: "1.15.5",
            time_created_ms: 1_779_295_622_208,
            time_updated_ms: 1_779_298_563_419,
            tokens_input: tin,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
        }
    }

    // === store_dir resolution (R1 §1.1, both directions) ===

    #[test]
    fn store_dir_prefers_xdg_data_home() {
        let e = env(&[("XDG_DATA_HOME", "/x/data"), ("HOME", "/home/u")]);
        assert_eq!(store_dir(&e), Some(PathBuf::from("/x/data/opencode")));
    }

    #[test]
    fn store_dir_falls_back_to_home_local_share() {
        let e = env(&[("HOME", "/home/u")]);
        assert_eq!(
            store_dir(&e),
            Some(PathBuf::from("/home/u/.local/share/opencode"))
        );
    }

    #[test]
    fn store_dir_empty_xdg_falls_back_to_home() {
        // An empty XDG_DATA_HOME must not shadow HOME (R1 §Scanner-contract-1
        // "if set and non-empty").
        let e = env(&[("XDG_DATA_HOME", ""), ("HOME", "/home/u")]);
        assert_eq!(
            store_dir(&e),
            Some(PathBuf::from("/home/u/.local/share/opencode"))
        );
    }

    #[test]
    fn store_dir_none_without_home_or_xdg() {
        let e = env(&[]);
        assert_eq!(store_dir(&e), None);
    }

    // === real-shape enumeration + stats (R1 §2, §3) ===

    #[test]
    fn reads_real_schema_rows_with_turns_and_tokens() {
        let tmp = TempDir::new().unwrap();
        // 21 turns for ses_big (R1's decisive session), across the message table.
        let msg_ids: Vec<String> = (0..21).map(|i| format!("msg_{i}")).collect();
        let messages: Vec<(&str, &str)> = msg_ids.iter().map(|m| (m.as_str(), "ses_big")).collect();
        mint_store(
            &tmp,
            &[seed(
                "ses_big",
                "/home/u",
                "Testing functionality check",
                147_745,
            )],
            &messages,
        );
        let rows = sessions(tmp.path());
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.id, "ses_big");
        assert_eq!(r.directory, "/home/u");
        assert_eq!(r.title, "Testing functionality check");
        assert_eq!(r.turns, 21, "turns count the message table");
        assert_eq!(r.tokens_input, 147_745);
        assert_eq!(r.time_updated_ms, 1_779_298_563_419);
    }

    #[test]
    fn turns_count_message_not_session_message() {
        // The red herring (R1 §3.3): session_message holds agent/model-switch
        // events, NOT turns. A session with 2 messages but 8 session_message rows
        // must report turns=2.
        let tmp = TempDir::new().unwrap();
        let db = mint_store(
            &tmp,
            &[seed("ses_x", "/tmp", "T", 10)],
            &[("m1", "ses_x"), ("m2", "ses_x")],
        );
        // Add session_message rows that must NOT be counted.
        let conn = Connection::open(&db).unwrap();
        for i in 0..8 {
            conn.execute(
                "INSERT INTO session_message (id, session_id, type) VALUES (?1, 'ses_x', 'agent-switched')",
                rusqlite::params![format!("sm{i}")],
            )
            .unwrap();
        }
        drop(conn);
        let rows = sessions(tmp.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].turns, 2,
            "turns come from message, not session_message"
        );
    }

    #[test]
    fn enumerates_multiple_ordered_by_time_updated_desc() {
        let tmp = TempDir::new().unwrap();
        let mut newer = seed("ses_new", "/a", "New", 1);
        newer.time_updated_ms = 2000;
        let mut older = seed("ses_old", "/b", "Old", 1);
        older.time_updated_ms = 1000;
        mint_store(&tmp, &[older, newer], &[]);
        let rows = sessions(tmp.path());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "ses_new", "ORDER BY time_updated DESC");
        assert_eq!(rows[1].id, "ses_old");
    }

    // === DEGRADE tests (R1 §5 / L8: EVERY failure → empty, NEVER an error) ===
    //
    // MUTATION EVIDENCE: if `sessions` propagated the open/query error instead of
    // degrading to empty, each of these would panic or bubble. NAMED.

    #[test]
    fn absent_store_is_empty_not_error() {
        // Z1/Z2: the db path does not exist → clean zero, without opening.
        let tmp = TempDir::new().unwrap();
        assert!(sessions(tmp.path()).is_empty());
        assert!(sessions(&tmp.path().join("does-not-exist")).is_empty());
    }

    #[test]
    fn valid_empty_session_table_is_zero() {
        // Z3: a real, valid, empty `session` table returns 0 naturally (a
        // non-vacuous green — the db opens and the query runs, returning no rows).
        let tmp = TempDir::new().unwrap();
        mint_store(&tmp, &[], &[]);
        assert!(sessions(tmp.path()).is_empty());
    }

    #[test]
    fn garbage_db_is_empty() {
        // Z4: present but not a database (`file is not a database`, code 26).
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(OPENCODE_DB_FILENAME),
            b"this is not a sqlite database at all \x00\xff",
        )
        .unwrap();
        assert!(
            sessions(tmp.path()).is_empty(),
            "a non-db blob degrades to empty"
        );
    }

    #[test]
    fn no_session_table_is_empty() {
        // A valid sqlite db with no `session` table → prepare fails → empty.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(OPENCODE_DB_FILENAME);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE other (x INTEGER);")
            .unwrap();
        drop(conn);
        assert!(sessions(tmp.path()).is_empty());
    }

    #[test]
    fn schema_drift_missing_consumed_column_is_empty() {
        // A `session` table missing one of OUR consumed columns (here `slug`) →
        // the SELECT prepare fails → degrade to empty (schema-drift safety).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(OPENCODE_DB_FILENAME);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT NOT NULL,
                title TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                tokens_input INTEGER NOT NULL DEFAULT 0,
                tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                tokens_cache_write INTEGER NOT NULL DEFAULT 0,
                version TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL);
            INSERT INTO session (id, directory, title, time_created, time_updated)
            VALUES ('s', '/w', 't', 1, 1);",
        )
        .unwrap();
        drop(conn);
        assert!(
            sessions(tmp.path()).is_empty(),
            "a dropped consumed column (schema drift) degrades to empty"
        );
    }

    #[test]
    fn malformed_row_skipped_siblings_survive() {
        // L8 / R1 §7: a single malformed/partial session row is skipped
        // permissively — it never errors the scan and never drops the GOOD rows.
        // A NULL in a column we read as a value makes query_map yield an Err for
        // THAT row; `rows.flatten()` drops it and keeps the rest. Minted with a
        // NULL-permitting schema so one row can carry a NULL `directory`.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(OPENCODE_DB_FILENAME);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY, directory TEXT, title TEXT, slug TEXT,
                time_updated INTEGER,
                tokens_input INTEGER, tokens_cache_read INTEGER, tokens_cache_write INTEGER
            );
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL);
            -- a GOOD row
            INSERT INTO session VALUES ('ses_good','/w','T','s',100,1,0,0);
            -- a MALFORMED row: NULL directory (read as String → per-row Err → dropped)
            INSERT INTO session VALUES ('ses_bad',NULL,'T','s',200,1,0,0);
            INSERT INTO message VALUES ('m1','ses_good');",
        )
        .unwrap();
        drop(conn);
        let rows = sessions(tmp.path());
        assert_eq!(
            rows.len(),
            1,
            "the malformed row is dropped, the good one survives"
        );
        assert_eq!(rows[0].id, "ses_good");
        assert_eq!(rows[0].turns, 1);
    }

    #[test]
    fn missing_message_table_is_empty() {
        // The `turns` subquery references `message`; without that table the
        // prepare fails → empty (never a partial/erroring ls).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(OPENCODE_DB_FILENAME);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY, slug TEXT NOT NULL, directory TEXT NOT NULL,
                title TEXT NOT NULL, version TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                tokens_input INTEGER NOT NULL DEFAULT 0,
                tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                tokens_cache_write INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        drop(conn);
        assert!(sessions(tmp.path()).is_empty());
    }
}
