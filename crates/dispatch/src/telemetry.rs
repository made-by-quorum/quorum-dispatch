//! A6 telemetry — FRESH design (shape ruled ADD-3a/3b; spec §4). The single
//! `marks.jsonl` stream gains TWO new line kinds and a pure snapshot fold.
//!
//! # Module name
//!
//! `telemetry` (not `lineage`): it owns BOTH the lineage `create` events AND the
//! content-free `usage` occurrence records AND the snapshot fold — broader than
//! lineage alone. The spec offered either name; this is the wider hat.
//!
//! # Line kinds in `marks.jsonl` (single stream, one file, O_APPEND 0600)
//!
//! - **Mark lines (existing, UNTOUCHED):** `{"ts","sessionId","payload"}` — the
//!   engine NEVER inspects `payload` keys (ADD-3a/3b). Built by `mark.rs`.
//! - **Engine event lines (new):** distinguished by a top-level `"event"` key.
//!   Mark lines have `"payload"` and never `"event"`; event lines never carry
//!   `"payload"`. A `"event"` key INSIDE a mark payload stays inside the payload
//!   object and is NEVER confused with a top-level event line (the fold keys on
//!   the TOP-LEVEL `"event"` only). No collision.
//!   - create: `{"ts","event":"create","name",sessionId?,spawnedBy?,
//!     spawnedBySessionId?,backend?}`
//!   - usage:  `{"ts","event":"usage","verb",sessionId?,name?}`
//!
//! # Engine-content-free discipline (Pete-ruled ADD-3a/3b)
//!
//! The engine never inspects mark payload KEYS. No org vocabulary
//! (on_behalf_of/role_claimed/reports_to/succeeds) anywhere here. The event lines
//! carry ONLY the engine-owned fields above (lineage NAMES + the via-backend
//! name + a verb tag) — never message bodies, payloads, or byte counts.
//!
//! # Durability posture (spec §4.1, red-team F8)
//!
//! Event/usage lines are small (sub-`PIPE_BUF`) → an `O_APPEND` single write is
//! atomic for THEM. `qd mark` payloads above `PIPE_BUF` can interleave under
//! concurrency — a documented v1 limitation (fresh surface, no TS counterpart).
//! Append failures on usage/create lines are NON-FATAL (the caller warns to
//! stderr and leaves the verb's exit code unchanged) — telemetry must never break
//! a working verb.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::effects::{Clock, Env};
use crate::exec::Exec;
use crate::paths::QdPaths;
use crate::registry::{self, RegistryEntry};
use crate::render::epoch_ms_to_iso;

// ===========================================================================
// Caller-identity ppid walk (hoisted from verbs/whoami.rs — spec §4.2 / F11)
// ===========================================================================

/// Port of `findCallerSession` (commands/status.ts:159-184). Start at the
/// caller's parent PID, walk UP the process tree up to 10 levels; at each PID try
/// to read `<sessions_dir>/<pid>.json` (the registry entry) — first hit wins.
/// Walk up via `ps -o ppid= -p <pid>` through the injected [`Exec`] seam.
///
/// Behavior-preserving HOIST of `whoami.rs`'s private fn (F11): identical walk,
/// identical stop conditions (`pid <= 1`, unreadable parent). The Exec seam is
/// now a parameter (was a hard-coded `RealExec`) so create.rs and unit tests can
/// inject a fake — a pure signature generalization, no behavior change for the
/// real-deps caller (`whoami` passes `&RealExec`).
pub fn find_caller_session(paths: &QdPaths, exec: &dyn Exec) -> Option<RegistryEntry> {
    let mut pid: i32 = unsafe { libc::getppid() };
    for _ in 0..10 {
        if pid <= 1 {
            break;
        }
        if let Some(entry) = registry::read_entry(&paths.sessions_dir, pid as i64) {
            return Some(entry);
        }
        // Step to the parent: `ps -o ppid= -p <pid>` (commands/status.ts:170-178).
        let parent = ps_ppid(exec, pid)?;
        if parent <= 1 {
            break;
        }
        pid = parent;
    }
    None
}

/// `ps -o ppid= -p <pid>` → the parent pid, or `None` on any error / non-numeric
/// output (TS `catch { break }` / `isNaN(parent)`).
///
/// Generalized from `&RealExec` to `&dyn Exec` (F11) — the ONLY behavior change
/// is that a substitutable Exec is now accepted; the real path is identical.
pub fn ps_ppid(exec: &dyn Exec, pid: i32) -> Option<i32> {
    let args = vec![
        "-o".to_string(),
        "ppid=".to_string(),
        "-p".to_string(),
        pid.to_string(),
    ];
    let out = exec.run("ps", &args, &[], None, None).ok()?;
    out.stdout.trim().parse::<i32>().ok()
}

// ===========================================================================
// Marks path (shared with mark.rs's resolution — QD_HOME-honoring, L9a)
// ===========================================================================

/// Resolve `<qdHome>/state/marks.jsonl` via `QdPaths::from_home_env` (honors
/// QD_HOME through the injected `Env` seam; L9a). `None` if HOME is unset. Same
/// resolution `mark.rs` uses, hoisted so create/send/wait/mark all agree.
pub fn marks_path(env: &dyn Env) -> Option<PathBuf> {
    let home = env.var("HOME").filter(|s| !s.is_empty())?;
    let paths = QdPaths::from_home_env(Path::new(&home), env);
    Some(paths.state_dir.join("marks.jsonl"))
}

// ===========================================================================
// Event line builders (spec §4.1 — exact key order)
// ===========================================================================

/// A `create` lineage event (spec §4.1). `name` is the new session's name (the
/// only REQUIRED field beyond ts/event); the rest are present only when known.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CreateEvent {
    pub name: String,
    pub session_id: Option<String>,
    pub spawned_by: Option<String>,
    pub spawned_by_session_id: Option<String>,
    pub backend: Option<String>,
}

/// Build the `create` event line — key order EXACTLY:
/// `ts, event, name, sessionId?, spawnedBy?, spawnedBySessionId?, backend?`
/// (serde_json `preserve_order`). Optional fields are OMITTED when `None` (never
/// emitted as empty strings — spec §4.2: "fields absent, never empty strings").
pub fn build_create_line(ts: &str, ev: &CreateEvent) -> String {
    let mut obj = Map::new();
    obj.insert("ts".into(), Value::String(ts.to_string()));
    obj.insert("event".into(), Value::String("create".into()));
    obj.insert("name".into(), Value::String(ev.name.clone()));
    insert_opt(&mut obj, "sessionId", &ev.session_id);
    insert_opt(&mut obj, "spawnedBy", &ev.spawned_by);
    insert_opt(&mut obj, "spawnedBySessionId", &ev.spawned_by_session_id);
    insert_opt(&mut obj, "backend", &ev.backend);
    Value::Object(obj).to_string()
}

/// Build a `usage` line — key order EXACTLY: `ts, event, verb, sessionId?, name?`.
/// At least one of `session_id`/`name` is expected present (spec §4.1); the
/// builder does not enforce it (callers supply what they have — a usage line with
/// neither is harmless to the fold, which simply can't key it).
pub fn build_usage_line(
    ts: &str,
    verb: &str,
    session_id: Option<&str>,
    name: Option<&str>,
) -> String {
    let mut obj = Map::new();
    obj.insert("ts".into(), Value::String(ts.to_string()));
    obj.insert("event".into(), Value::String("usage".into()));
    obj.insert("verb".into(), Value::String(verb.to_string()));
    if let Some(s) = session_id.filter(|s| !s.is_empty()) {
        obj.insert("sessionId".into(), Value::String(s.to_string()));
    }
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        obj.insert("name".into(), Value::String(n.to_string()));
    }
    Value::Object(obj).to_string()
}

fn insert_opt(obj: &mut Map<String, Value>, key: &str, v: &Option<String>) {
    if let Some(v) = v.as_ref().filter(|s| !s.is_empty()) {
        obj.insert(key.to_string(), Value::String(v.clone()));
    }
}

// ===========================================================================
// Appender (O_APPEND 0600 single write, create-parent — spec §4.1)
// ===========================================================================

/// Append one line + newline to `marks_path` via `O_APPEND` single write; create
/// the parent dir if missing. Mode 0600. Returns a human error on any failure.
///
/// Same discipline as `mark.rs`'s `append_mark` (independent fn, identical
/// behavior) — kept here so the event/usage builders have a local appender that
/// the additive surfaces own without reaching into the verbs/ binary layer.
pub fn append_line(marks_path: &Path, line: &str) -> Result<(), String> {
    if let Some(parent) = marks_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not create state dir: {e}"))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(marks_path)
        .map_err(|e| format!("could not open {}: {e}", marks_path.display()))?;
    f.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("write failed: {e}"))
}

// ===========================================================================
// Public verb-facing API (the lead calls append_create_event from create.rs;
// send/wait/mark call append_usage). NON-FATAL by contract — these return a
// Result the caller logs-and-ignores; they NEVER change a verb's exit code.
// ===========================================================================

/// Append a `create` lineage event line to `marks.jsonl`. Resolves the marks path
/// via the injected `Env` (QD_HOME-honoring). `Err` carries a human reason for the
/// caller to warn about; the caller MUST NOT change its exit code on failure
/// (spec §4.1 — telemetry is best-effort-durable).
pub fn append_create_event(
    env: &dyn Env,
    clock: &dyn Clock,
    ev: &CreateEvent,
) -> Result<(), String> {
    let path = marks_path(env)
        .ok_or_else(|| "HOME is not set — cannot resolve the state dir".to_string())?;
    let line = build_create_line(&epoch_ms_to_iso(clock.now_ms()), ev);
    append_line(&path, &line)
}

/// Append a content-free `usage` line for a successful verb invocation. Same
/// best-effort contract as [`append_create_event`].
pub fn append_usage(
    env: &dyn Env,
    clock: &dyn Clock,
    verb: &str,
    session_id: Option<&str>,
    name: Option<&str>,
) -> Result<(), String> {
    let path = marks_path(env)
        .ok_or_else(|| "HOME is not set — cannot resolve the state dir".to_string())?;
    let line = build_usage_line(&epoch_ms_to_iso(clock.now_ms()), verb, session_id, name);
    append_line(&path, &line)
}

// ===========================================================================
// Snapshot fold (spec §4.3) — pure, tolerant, last-write-wins
// ===========================================================================

/// One folded telemetry record for a session. Carries only the additive A6
/// surfacing fields. Both keys (sessionId, name) may have produced it; the join
/// uses sessionId-first then name-fallback (see [`SnapshotMap::lookup`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FoldedSession {
    pub backend: Option<String>,
    pub spawned_by: Option<String>,
}

impl FoldedSession {
    fn is_empty(&self) -> bool {
        self.backend.is_none() && self.spawned_by.is_none()
    }
}

/// The fold result: two independent indexes built from the SAME create events —
/// one keyed by sessionId, one keyed by name. The render seam looks up
/// sessionId-first, name-fallback (spec §4.3, red-team F6).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotMap {
    by_session_id: HashMap<String, FoldedSession>,
    by_name: HashMap<String, FoldedSession>,
}

impl SnapshotMap {
    /// Look up a session's folded telemetry. Join precedence (F6): an exact
    /// sessionId match wins; the name-keyed fallback is consulted ONLY when no
    /// sessionId-keyed entry exists for `session_id`.
    ///
    /// NAMED v1 LIMITATION (spec §4.3): a name reused across time (tombstoned +
    /// live) can mis-attribute via the name fallback until a sessionId-keyed line
    /// exists. Pinned by a deterministic unit test below.
    pub fn lookup(&self, session_id: &str, name: Option<&str>) -> Option<&FoldedSession> {
        if !session_id.is_empty() {
            if let Some(f) = self.by_session_id.get(session_id) {
                return Some(f);
            }
        }
        name.and_then(|n| self.by_name.get(n))
    }

    /// True when the fold yielded nothing (missing/empty/all-skipped input). The
    /// render seam treats this identically to `None` → today's bytes.
    pub fn is_empty(&self) -> bool {
        self.by_session_id.is_empty() && self.by_name.is_empty()
    }
}

/// Fold the `marks.jsonl` stream into a [`SnapshotMap`]. PURE over the text.
///
/// Tolerance (spec §4.3, 0b dirty-state lesson):
/// - a torn TRAILING line (no final newline / truncated JSON) is IGNORED — only
///   whole, parseable lines contribute;
/// - non-object lines, mark lines (top-level `"payload"`, no top-level
///   `"event"`), and unknown event kinds are SKIPPED;
/// - last-write-wins per key for `backend`/`spawnedBy`.
///
/// NOTHING here unwraps on external data — every parse degrades to a skip.
pub fn fold_marks(text: &str) -> SnapshotMap {
    let mut map = SnapshotMap::default();

    // A torn trailing line: if the text does not end in '\n', the final segment
    // may be a partial write. We only fold lines that are followed by a newline;
    // the trailing partial (if any) is dropped. (A clean final line WITH a
    // trailing newline produces an empty trailing segment from split, which is
    // skipped as non-JSON anyway.)
    let mut lines: Vec<&str> = text.split('\n').collect();
    if !text.ends_with('\n') {
        // Drop the unterminated trailing segment (torn write).
        lines.pop();
    }

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // unparseable → skip (tolerant)
        };
        let Value::Object(obj) = v else {
            continue; // non-object line → skip
        };
        // TOP-LEVEL "event" distinguishes an engine event line from a mark line.
        // A mark line has "payload" and no top-level "event"; an "event" key
        // nested inside a payload object is invisible here (we read obj["event"]
        // only). No collision.
        let Some(Value::String(kind)) = obj.get("event") else {
            continue; // mark line or eventless line → skip
        };
        if kind != "create" {
            // Only `create` events carry backend/spawnedBy. `usage` lines (and any
            // future/unknown kind) are pure occurrence records → skipped by the fold.
            continue;
        }

        let backend = str_field(&obj, "backend");
        let spawned_by = str_field(&obj, "spawnedBy");
        // A create event with neither field is SKIPPED entirely (we only store
        // when there is signal): it neither keys the session nor contributes to
        // render. Consequence: such a session still falls through to the name
        // fallback on lookup — acceptable, an empty record would render the same
        // nothing (lead review nit, integration commit).
        let folded = FoldedSession {
            backend,
            spawned_by,
        };
        if folded.is_empty() {
            continue;
        }

        // Last-write-wins per key. Index by sessionId AND name when present.
        if let Some(sid) = str_field(&obj, "sessionId") {
            map.by_session_id.insert(sid, folded.clone());
        }
        if let Some(name) = str_field(&obj, "name") {
            map.by_name.insert(name, folded);
        }
    }

    map
}

/// Best-effort fold from the marks file resolved via `env`. A missing/unreadable
/// file → an EMPTY map (never an error). The render seam treats empty ≡ today's
/// bytes. NOTHING here can fail the calling verb.
pub fn fold_from_env(env: &dyn Env) -> SnapshotMap {
    let Some(path) = marks_path(env) else {
        return SnapshotMap::default();
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    fold_marks(&text)
}

/// Pull a non-empty string field from a JSON object, else `None`. A wrong-typed
/// field (number, bool, ...) degrades to `None` (tolerant).
fn str_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{FixedClock, MapEnv};
    use crate::exec::ScriptedExec;
    use serde_json::json;
    use tempfile::tempdir;

    // --- ps_ppid / find_caller_session over the Exec seam (F11 new unit) ---

    #[test]
    fn ps_ppid_parses_through_injected_exec() {
        // A MapExec/fake (ScriptedExec) returns canned `ps` output; ps_ppid parses
        // the trimmed integer.
        let exec = ScriptedExec::new().on("ps", &["-o"], Some(0), " 4242 \n", "");
        assert_eq!(ps_ppid(&exec, 1234), Some(4242));
    }

    #[test]
    fn ps_ppid_none_on_nonnumeric() {
        let exec = ScriptedExec::new().on("ps", &["-o"], Some(0), "not-a-number", "");
        assert_eq!(ps_ppid(&exec, 1234), None);
    }

    #[test]
    fn find_caller_session_returns_none_when_no_entry_and_walk_stops() {
        // ps returns 1 (init) → the walk stops; no registry dir → None. Proves the
        // hoisted walk drives through the injected Exec, not a hard-coded RealExec.
        let dir = tempdir().unwrap();
        let paths = QdPaths::from_home(dir.path());
        let exec = ScriptedExec::new().on("ps", &["-o"], Some(0), "1", "");
        assert_eq!(find_caller_session(&paths, &exec), None);
    }

    // --- create / usage line builders: exact key order + omission ---

    #[test]
    fn create_line_full_key_order() {
        let ev = CreateEvent {
            name: "child".into(),
            session_id: Some("sid-1".into()),
            spawned_by: Some("parent".into()),
            spawned_by_session_id: Some("sid-0".into()),
            backend: Some("ccr-3456".into()),
        };
        let line = build_create_line("2026-06-05T10:00:00.000Z", &ev);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "ts",
                "event",
                "name",
                "sessionId",
                "spawnedBy",
                "spawnedBySessionId",
                "backend"
            ]
        );
        assert_eq!(parsed["event"], json!("create"));
        assert_eq!(parsed["backend"], json!("ccr-3456"));
    }

    #[test]
    fn create_line_omits_absent_optionals_never_empty_strings() {
        // Only name present → ts, event, name and nothing else (no empty strings).
        let ev = CreateEvent {
            name: "child".into(),
            ..Default::default()
        };
        let line = build_create_line("t", &ev);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["ts", "event", "name"]);
    }

    #[test]
    fn create_line_treats_empty_string_optionals_as_absent() {
        let ev = CreateEvent {
            name: "child".into(),
            session_id: Some(String::new()),
            spawned_by: Some(String::new()),
            ..Default::default()
        };
        let line = build_create_line("t", &ev);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["ts", "event", "name"],
            "empty-string optionals dropped"
        );
    }

    #[test]
    fn usage_line_key_order_and_omission() {
        let line = build_usage_line("t", "send", Some("sid-1"), Some("alpha"));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["ts", "event", "verb", "sessionId", "name"]);

        // name omitted when None.
        let line2 = build_usage_line("t", "mark", Some("sid-1"), None);
        let parsed2: Value = serde_json::from_str(&line2).unwrap();
        let keys2: Vec<&str> = parsed2
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys2, vec!["ts", "event", "verb", "sessionId"]);
    }

    // --- append + non-fatal API ---

    #[test]
    fn append_create_and_usage_are_jsonl() {
        let home = tempdir().unwrap();
        let mut env = MapEnv::default();
        env.vars
            .insert("HOME".into(), home.path().to_string_lossy().into_owned());
        let clock = FixedClock(1_717_530_000_000);

        append_create_event(
            &env,
            &clock,
            &CreateEvent {
                name: "child".into(),
                session_id: Some("sid-1".into()),
                spawned_by: Some("parent".into()),
                backend: Some("ccr-a".into()),
                ..Default::default()
            },
        )
        .unwrap();
        append_usage(&env, &clock, "send", Some("sid-1"), Some("child")).unwrap();

        let path = home.path().join(".quorum/dispatch/state/marks.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let c: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(c["event"], json!("create"));
        let u: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(u["event"], json!("usage"));
        assert!(text.ends_with('\n'));
    }

    // --- fold dirty-state matrix (spec §7 G-A3 / G-A6) ---

    #[test]
    fn fold_missing_or_empty_is_empty_map() {
        assert!(fold_marks("").is_empty());
        assert!(fold_marks("\n").is_empty());
        // fold_from_env with no file → empty.
        let home = tempdir().unwrap();
        let mut env = MapEnv::default();
        env.vars
            .insert("HOME".into(), home.path().to_string_lossy().into_owned());
        assert!(fold_from_env(&env).is_empty());
    }

    #[test]
    fn fold_ignores_torn_trailing_line() {
        // Two whole lines + a torn (unterminated, truncated) third.
        let whole = build_create_line(
            "t1",
            &CreateEvent {
                name: "a".into(),
                session_id: Some("sid-a".into()),
                backend: Some("be-a".into()),
                ..Default::default()
            },
        );
        let text = format!("{whole}\n{{\"ts\":\"t2\",\"event\":\"crea"); // torn, no newline
        let map = fold_marks(&text);
        // The whole line folded; the torn tail was ignored (no panic).
        assert_eq!(
            map.lookup("sid-a", None).unwrap().backend.as_deref(),
            Some("be-a")
        );
    }

    #[test]
    fn fold_skips_mark_lines_and_unknown_kinds_and_usage() {
        // Interleaved: a mark line (payload, no event), a usage line, an unknown
        // event kind, and a real create — only the create folds.
        let mark =
            json!({"ts":"t","sessionId":"sid-x","payload":{"event":"create","backend":"SPOOF"}})
                .to_string();
        let usage = build_usage_line("t", "send", Some("sid-x"), Some("x"));
        let unknown =
            json!({"ts":"t","event":"frobnicate","sessionId":"sid-x","backend":"NOPE"}).to_string();
        let create = build_create_line(
            "t",
            &CreateEvent {
                name: "real".into(),
                session_id: Some("sid-real".into()),
                backend: Some("be-real".into()),
                spawned_by: Some("p".into()),
                ..Default::default()
            },
        );
        let text = format!("{mark}\n{usage}\n{unknown}\n{create}\n");
        let map = fold_marks(&text);
        // The mark line's INNER "event":"create" + "backend":"SPOOF" did NOT leak.
        assert!(
            map.lookup("sid-x", Some("x")).is_none(),
            "mark/usage/unknown contributed nothing"
        );
        let f = map.lookup("sid-real", None).unwrap();
        assert_eq!(f.backend.as_deref(), Some("be-real"));
        assert_eq!(f.spawned_by.as_deref(), Some("p"));
    }

    #[test]
    fn fold_last_write_wins_per_key() {
        let first = build_create_line(
            "t1",
            &CreateEvent {
                name: "a".into(),
                session_id: Some("sid-a".into()),
                backend: Some("be-old".into()),
                ..Default::default()
            },
        );
        let second = build_create_line(
            "t2",
            &CreateEvent {
                name: "a".into(),
                session_id: Some("sid-a".into()),
                backend: Some("be-new".into()),
                ..Default::default()
            },
        );
        let map = fold_marks(&format!("{first}\n{second}\n"));
        assert_eq!(
            map.lookup("sid-a", None).unwrap().backend.as_deref(),
            Some("be-new")
        );
    }

    #[test]
    fn fold_join_precedence_sessionid_first_then_name() {
        // A create keyed by BOTH sid and name; a SECOND create keyed by name only
        // (different name) — the lookup prefers the sid-keyed record.
        let by_both = build_create_line(
            "t1",
            &CreateEvent {
                name: "alpha".into(),
                session_id: Some("sid-1".into()),
                backend: Some("be-sid".into()),
                ..Default::default()
            },
        );
        let map = fold_marks(&format!("{by_both}\n"));
        // sid present + matches → sid record (even if a different name is passed).
        assert_eq!(
            map.lookup("sid-1", Some("alpha"))
                .unwrap()
                .backend
                .as_deref(),
            Some("be-sid")
        );
        // sid present but UNKNOWN → fall back to the name index.
        assert_eq!(
            map.lookup("sid-unknown", Some("alpha"))
                .unwrap()
                .backend
                .as_deref(),
            Some("be-sid")
        );
        // neither key matches → None.
        assert!(map.lookup("sid-unknown", Some("other")).is_none());
    }

    #[test]
    fn fold_name_reuse_misattributes_via_name_fallback_named_limitation() {
        // SPEC §4.3 NAMED v1 LIMITATION pinned deterministically:
        // a tombstoned session "reused-name" wrote a create with backend be-OLD
        // keyed by name (no sessionId captured at the time). A NEW live session
        // reuses the same name but has a DIFFERENT sessionId and has NOT yet
        // emitted a sessionId-keyed create line. Looking it up by its real sessionId
        // (unknown to the fold) falls back to the NAME index → mis-attributes the
        // OLD backend. This is the documented behavior, not a bug to hide.
        let old = build_create_line(
            "t-old",
            &CreateEvent {
                name: "reused-name".into(),
                backend: Some("be-OLD".into()),
                ..Default::default() // no sessionId — name-only keying
            },
        );
        let map = fold_marks(&format!("{old}\n"));
        // The NEW session's real sessionId is not in the fold; name fallback hits OLD.
        let f = map.lookup("sid-NEW-live", Some("reused-name")).unwrap();
        assert_eq!(
            f.backend.as_deref(),
            Some("be-OLD"),
            "name reuse mis-attributes until a sessionId-keyed line exists (named v1 limitation)"
        );
    }

    #[test]
    fn fold_inner_event_key_in_payload_does_not_collide() {
        // OPAQUE-PAYLOAD property (G-A6): a mark whose payload object itself
        // contains an "event":"create" key + a "backend" key MUST NOT be read as an
        // engine create event. The fold reads only the TOP-LEVEL event key.
        let mark = json!({
            "ts":"t",
            "sessionId":"sid-z",
            "payload":{"event":"create","backend":"INJECT","spawnedBy":"INJECT"}
        })
        .to_string();
        let map = fold_marks(&format!("{mark}\n"));
        assert!(
            map.lookup("sid-z", None).is_none(),
            "inner event key stayed inside payload"
        );
        assert!(map.is_empty());
    }

    #[test]
    fn fold_tolerates_wrong_typed_event_fields() {
        // A create line with a numeric backend / boolean spawnedBy degrades those
        // fields to None (no panic), but the line is still a create.
        let line = json!({"ts":"t","event":"create","name":"a","sessionId":"sid-a","backend":123,"spawnedBy":true}).to_string();
        let map = fold_marks(&format!("{line}\n"));
        // Both fields degraded → the record is empty → not stored.
        assert!(map.lookup("sid-a", None).is_none());
    }
}
