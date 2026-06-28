//! `qd mark <session> <payload>` — NET-NEW (ADD-3a/3b; spec §4). Dumb opaque
//! append: resolve the session, then append ONE line to the qd state dir's
//! `marks.jsonl`: `{"ts": <iso8601>, "sessionId": "<id>", "payload": <object>}`.
//!
//! The marks file lives at `<qdHome>/state/marks.jsonl` where
//! `qdHome = QD_HOME || <home>/.quorum/dispatch` (commands/bootstrap.ts:88-96
//! `resolveBootstrapPaths`). QD_HOME is read ONLY through the injected `Env` seam
//! via [`QdPaths::from_home_env`] (L9a — never raw `std::env`), so a jailed
//! QD_HOME relocates the marks file correctly (H4).
//!
//! Discipline (spec §4):
//! - `<payload>` MUST parse as a JSON **object**; the engine NEVER inspects keys.
//! - Append is open-append-write-close (O_APPEND single write); no rewrite, no
//!   read-back, no dedup — the append-only stream is the durable thing (ADD-3).
//! - Exit 0 on append; 1 on unresolvable session / non-object payload / write error.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use clap::ArgMatches;
use serde_json::Value;

use dispatch::effects::{Clock, Env, RealClock, RealEnv};
use dispatch::join::{resolve_session, JoinOpts, Resolution};
use dispatch::paths::QdPaths;
use dispatch::render::epoch_ms_to_iso;

use super::common;

pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    let payload_raw = m.get_one::<String>("payload").expect("required by clap");

    // Payload MUST be a JSON object (spec §4). Non-object → stderr + exit 1.
    let payload: Value = match validate_payload(payload_raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("qd mark: {e}");
            return 1;
        }
    };

    // Resolve <session> → sessionId via the SAME A1 resolver info/attach use.
    let sessions = match common::all_sessions(JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: false,
        limit: None,
    }) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let session_id = match resolve_session(query, &sessions) {
        Resolution::One(s) => s.session_id.clone(),
        Resolution::None => {
            eprintln!("No session matching \"{query}\"");
            return 1;
        }
        Resolution::Many(v) => {
            eprintln!("Ambiguous — \"{query}\" matches {} sessions.", v.len());
            return 1;
        }
    };

    // Resolve the marks path via QdPaths.state_dir, which honors QD_HOME through
    // the injected Env seam (H4 / L9a): <qdHome>/state/marks.jsonl.
    let env = RealEnv;
    let marks_path = match marks_path(&env) {
        Some(p) => p,
        None => {
            eprintln!("qd mark: HOME is not set — cannot resolve the state dir.");
            return 1;
        }
    };

    // Build the mark line: {"ts", "sessionId", "payload"} (object key order
    // preserved by serde_json preserve_order). ts = ISO-8601 UTC ms (Date.toJSON).
    let now = RealClock.now_ms();
    let line = build_mark_line(&epoch_ms_to_iso(now), &session_id, payload);

    // Append to <state_dir>/marks.jsonl (create parent if missing). O_APPEND
    // single write, then close (ADD-3 JSONL discipline).
    match append_mark(&marks_path, &line) {
        Ok(()) => {
            // A6 §4.1: mark appends its payload line AND a usage line, on the SAME
            // SUCCESSFUL invocation. Best-effort: a usage-append failure warns but
            // NEVER changes the verb's exit code (telemetry must not break mark).
            if let Err(e) =
                dispatch::telemetry::append_usage(&env, &RealClock, "mark", Some(&session_id), None)
            {
                eprintln!("qd mark: telemetry usage append failed (non-fatal): {e}");
            }
            0
        }
        Err(e) => {
            eprintln!("qd mark: {e}");
            1
        }
    }
}

/// Resolve `<qdHome>/state/marks.jsonl` via `QdPaths::from_home_env` (honors
/// QD_HOME through the injected `Env` seam; L9a). `None` if HOME is unset.
pub fn marks_path(env: &dyn Env) -> Option<std::path::PathBuf> {
    let home = env.var("HOME").filter(|s| !s.is_empty())?;
    let paths = QdPaths::from_home_env(Path::new(&home), env);
    Some(paths.state_dir.join("marks.jsonl"))
}

/// Validate a raw payload string is a JSON object and return it (the engine NEVER
/// inspects keys — spec §4). Pure, testable.
pub fn validate_payload(raw: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err("<payload> must be a JSON object".to_string()),
        Err(e) => Err(format!("<payload> is not valid JSON: {e}")),
    }
}

/// Build the mark line `{"ts","sessionId","payload"}` (key order preserved by
/// serde_json preserve_order). Pure, testable.
pub fn build_mark_line(ts: &str, session_id: &str, payload: Value) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("ts".into(), Value::String(ts.to_string()));
    obj.insert("sessionId".into(), Value::String(session_id.to_string()));
    obj.insert("payload".into(), payload);
    Value::Object(obj).to_string()
}

/// Append one line + newline to `marks_path` via O_APPEND single write; create
/// the parent dir if missing. Returns a human error on any failure.
pub fn append_mark(marks_path: &Path, line: &str) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn non_object_payload_rejected() {
        assert!(validate_payload("[1,2,3]").is_err());
        assert!(validate_payload("\"a string\"").is_err());
        assert!(validate_payload("42").is_err());
        assert!(validate_payload("not json").is_err());
        assert!(validate_payload("{\"k\":1}").is_ok());
    }

    #[test]
    fn opaque_payload_round_trips_byte_identical() {
        // The engine never inspects keys: arbitrary org vocabulary flows through
        // verbatim. Round-trip the exact object back out of the built line.
        let payload = json!({
            "role_claimed": "impl",
            "reports_to": "lead-2",
            "nested": {"a": [1, 2, {"b": "c"}]},
            "unicode": "café ☕"
        });
        let line = build_mark_line("2026-06-04T10:00:00.000Z", "sess-1", payload.clone());
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            parsed["payload"], payload,
            "payload must round-trip identical"
        );
        assert_eq!(parsed["ts"], json!("2026-06-04T10:00:00.000Z"));
        assert_eq!(parsed["sessionId"], json!("sess-1"));
        // Key order: ts, sessionId, payload.
        let obj = parsed.as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["ts", "sessionId", "payload"]);
    }

    #[test]
    fn opaque_payload_with_inner_event_key_does_not_collide_with_a6_event_lines() {
        // A6 (G-A6): a mark payload may itself carry an "event" key (and any org
        // vocabulary). It MUST flow through untouched and stay INSIDE the payload
        // object — never confused with a top-level A6 engine event line. The fold
        // keys only on the TOP-LEVEL "event"; the round-trip here proves the mark
        // line's top level has "payload" (not "event") and the inner key survives.
        let payload = json!({
            "event": "create",          // inner key — must NOT promote to top level
            "backend": "SPOOF",
            "on_behalf_of": "lead",
            "nested": {"event": "usage"}
        });
        let line = build_mark_line("2026-06-05T10:00:00.000Z", "sess-z", payload.clone());
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let top_keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        // Top level is a MARK line: ts, sessionId, payload — never a bare "event".
        assert_eq!(top_keys, vec!["ts", "sessionId", "payload"]);
        // The inner event key + everything else round-trips verbatim.
        assert_eq!(parsed["payload"], payload);
    }

    #[test]
    fn appends_exactly_one_valid_jsonl_line() {
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join(".quorum")
            .join("dispatch")
            .join("state")
            .join("marks.jsonl");
        let line = build_mark_line("2026-06-04T10:00:00.000Z", "s", json!({"k": 1}));
        append_mark(&path, &line).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "exactly one line");
        let parsed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["payload"], json!({"k": 1}));
        // Trailing newline present.
        assert!(contents.ends_with('\n'));
    }

    #[test]
    fn append_is_additive_not_rewrite() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("marks.jsonl");
        append_mark(&path, &build_mark_line("t1", "a", json!({"n": 1}))).unwrap();
        append_mark(&path, &build_mark_line("t2", "b", json!({"n": 2}))).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 2, "append-only stream grows");
    }

    // --- H4: marks path honors QD_HOME via the injected Env seam ---

    use dispatch::effects::MapEnv;

    #[test]
    fn marks_path_default_under_home_dot_qd_state() {
        // QD_HOME unset → <HOME>/.quorum/dispatch/state/marks.jsonl (commands/bootstrap.ts:88-96 default).
        let mut env = MapEnv::default();
        env.vars
            .insert("HOME".to_string(), "/jail/home".to_string());
        assert_eq!(
            marks_path(&env).unwrap(),
            std::path::Path::new("/jail/home/.quorum/dispatch/state/marks.jsonl")
        );
    }

    #[test]
    fn marks_path_honors_qd_home_override() {
        // QD_HOME set → <QD_HOME>/state/marks.jsonl, NOT <HOME>/.quorum/dispatch/state.
        let mut env = MapEnv::default();
        env.vars
            .insert("HOME".to_string(), "/jail/home".to_string());
        env.vars
            .insert("QD_HOME".to_string(), "/jail/qddata".to_string());
        assert_eq!(
            marks_path(&env).unwrap(),
            std::path::Path::new("/jail/qddata/state/marks.jsonl")
        );
    }

    #[test]
    fn marks_path_none_when_home_unset() {
        let env = MapEnv::default(); // no HOME
        assert!(marks_path(&env).is_none());
    }

    /// H4 end-to-end against the real filesystem (tempdirs): a SET QD_HOME lands
    /// the marks file under `<QD_HOME>/state/`, and an UNSET QD_HOME lands it
    /// under `<HOME>/.quorum/dispatch/state/` — using the SAME resolve→build→append path the
    /// `run()` backend uses (minus the session resolver, which needs live state).
    #[test]
    fn marks_land_under_qd_home_when_set_else_home_dot_qd() {
        // (a) QD_HOME set.
        let home = tempdir().unwrap();
        let qdhome = tempdir().unwrap();
        let mut env = MapEnv::default();
        env.vars.insert(
            "HOME".to_string(),
            home.path().to_string_lossy().into_owned(),
        );
        env.vars.insert(
            "QD_HOME".to_string(),
            qdhome.path().to_string_lossy().into_owned(),
        );
        let p = marks_path(&env).unwrap();
        append_mark(&p, &build_mark_line("t", "s", json!({"k": 1}))).unwrap();
        let expected = qdhome.path().join("state").join("marks.jsonl");
        assert!(expected.exists(), "marks.jsonl must be under QD_HOME/state");
        // And NOT under HOME/.quorum/dispatch/state.
        assert!(!home
            .path()
            .join(".quorum/dispatch/state/marks.jsonl")
            .exists());

        // (b) QD_HOME unset.
        let home2 = tempdir().unwrap();
        let mut env2 = MapEnv::default();
        env2.vars.insert(
            "HOME".to_string(),
            home2.path().to_string_lossy().into_owned(),
        );
        let p2 = marks_path(&env2).unwrap();
        append_mark(&p2, &build_mark_line("t", "s", json!({"k": 2}))).unwrap();
        assert!(
            home2
                .path()
                .join(".quorum/dispatch/state/marks.jsonl")
                .exists(),
            "marks.jsonl must default to HOME/.quorum/dispatch/state"
        );
    }
}
