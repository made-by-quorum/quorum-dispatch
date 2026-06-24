//! `golden` — support crate for the Phase 0b golden-master oracle.
//!
//! This crate is NOT the engine. It exists to give the layer-2 dirty-state JSON
//! corpus an **executable carrier** under `cargo test`, and to encode the
//! repo-wide permissive-parse convention (CONVENTIONS.md) as a reference
//! `SessionRecord` that the corpus tests exercise.
//!
//! The permissive-parse rule (CONVENTIONS.md): schema structs deserialized from
//! external/persisted data must use `#[serde(default)]` + `Option<T>` and must
//! NOT use `deny_unknown_fields`, so legacy / missing-field / unknown-field data
//! never hard-fails. A *genuinely corrupt* blob must fail CLEANLY (an `Err`),
//! never panic — a known TS lesson (dirty registry must not crash `sb ls`).

use serde::Deserialize;

/// Reference permissive session record. Mirrors the convention the engine crates
/// will follow: every field optional/defaulted, unknown fields ignored.
///
/// This is a *reference* shape for the corpus tests, not the engine's schema.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct SessionRecord {
    /// Stable session id, if present.
    pub session_id: Option<String>,
    /// Human name, if present.
    pub name: Option<String>,
    /// Claude PID, if present.
    pub pid: Option<i64>,
    /// Status string (idle/busy/...), if present.
    pub status: Option<String>,
    /// Attached client count; defaults to 0 when absent.
    pub clients: u32,
}

/// Parse a single JSON object permissively.
///
/// Returns `Ok(record)` for valid JSON (including legacy/missing-field/unknown-
/// field data — defaults fill the gaps), and `Err` for genuinely corrupt JSON.
/// It NEVER panics: malformed input is a clean `Err`, not a crash.
pub fn parse_record(input: &str) -> Result<SessionRecord, serde_json::Error> {
    serde_json::from_str(input)
}

/// Outcome of parsing one JSONL line.
#[derive(Debug, PartialEq)]
pub enum LineOutcome {
    /// A well-formed record.
    Ok(SessionRecord),
    /// A line that failed to parse (e.g. a truncated trailing line). Carries the
    /// error message so callers can log it. The reader keeps going — one bad line
    /// must not discard the good records (the partial-JSONL lesson).
    Skipped(String),
}

/// Parse a JSONL stream permissively. Good lines become records; a bad/partial
/// line is `Skipped` (recorded, not fatal). Blank lines are ignored. NEVER panics.
pub fn parse_jsonl(input: &str) -> Vec<LineOutcome> {
    let mut out = Vec::new();
    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionRecord>(line) {
            Ok(rec) => out.push(LineOutcome::Ok(rec)),
            Err(e) => out.push(LineOutcome::Skipped(e.to_string())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_default_not_fail() {
        let r = parse_record(r#"{"name":"work"}"#).expect("must parse");
        assert_eq!(r.name.as_deref(), Some("work"));
        assert_eq!(r.clients, 0); // defaulted
        assert_eq!(r.pid, None);
    }

    #[test]
    fn unknown_legacy_fields_ignored() {
        let r = parse_record(r#"{"name":"work","legacyField":"x","pid":42}"#)
            .expect("unknown fields must not fail");
        assert_eq!(r.pid, Some(42));
    }

    #[test]
    fn corrupt_json_is_clean_err_not_panic() {
        let r = parse_record(r#"{"name":"work","status":"bus"#);
        assert!(r.is_err(), "genuinely corrupt JSON must be an Err");
    }

    #[test]
    fn partial_jsonl_keeps_good_records() {
        let input = "{\"name\":\"a\",\"pid\":1}\n{\"name\":\"b\",\"pid\":2}\n{\"name\":\"c\",\"pi";
        let outcomes = parse_jsonl(input);
        assert_eq!(outcomes.len(), 3);
        let good = outcomes
            .iter()
            .filter(|o| matches!(o, LineOutcome::Ok(_)))
            .count();
        let skipped = outcomes
            .iter()
            .filter(|o| matches!(o, LineOutcome::Skipped(_)))
            .count();
        assert_eq!(good, 2, "two good records survive");
        assert_eq!(
            skipped, 1,
            "the partial trailing line is skipped, not fatal"
        );
    }
}
