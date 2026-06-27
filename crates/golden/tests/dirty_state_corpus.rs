//! Layer-2 dirty-state JSON corpus test (Phase 0b, deliverable 4).
//!
//! Reads the on-disk fixtures under `test/golden/fixtures/layer2/dirty-state/`
//! and asserts the documented permissive-parse behavior (CONVENTIONS.md):
//!   - legacy/unknown-field JSON parses (unknown fields ignored)
//!   - missing-field JSON parses (defaults fill the gaps)
//!   - genuinely corrupt JSON fails CLEANLY (Err, never a panic)
//!   - partial JSONL keeps the good records and skips only the bad line
//!
//! This is the EXECUTABLE carrier for the permissive-parse lesson; the fixtures
//! double as the synthetic corpus the bash harness references. No TS pin needed.

use std::path::PathBuf;

use golden::{parse_jsonl, parse_record, LineOutcome};

fn fixture_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/golden ; corpus = ../../test/golden/...
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // repo root
    p.push("test/golden/fixtures/layer2/dirty-state");
    p
}

fn read(name: &str) -> String {
    let p = fixture_dir().join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {:?}: {e}", p))
}

#[test]
fn clean_fixture_parses_fully() {
    let r = parse_record(read("clean.json").trim()).expect("clean.json must parse");
    assert_eq!(r.name.as_deref(), Some("work"));
    assert_eq!(r.status.as_deref(), Some("busy"));
}

#[test]
fn legacy_fixture_ignores_unknown_fields() {
    let r = parse_record(read("legacy.json").trim())
        .expect("legacy.json with unknown fields must parse permissively");
    assert_eq!(r.name.as_deref(), Some("work"));
    assert_eq!(r.pid, Some(4242));
}

#[test]
fn missing_field_fixture_uses_defaults() {
    let r = parse_record(read("missing-field.json").trim())
        .expect("missing-field.json must parse with defaults");
    assert_eq!(r.name.as_deref(), Some("work"));
    assert_eq!(r.clients, 0);
    assert_eq!(r.pid, None);
}

#[test]
fn corrupt_fixture_fails_cleanly() {
    // Must be an Err, never a panic. (If from_str panicked, the test binary would
    // abort — so reaching the assert at all proves no panic.)
    let r = parse_record(read("corrupt.json").trim());
    assert!(r.is_err(), "corrupt.json must fail cleanly as Err");
}

/// W4 (A4 pass-(b) F3): the wrong-typed fixture (string `pid`/`startedAt`) is a
/// CLEAN whole-row FAILURE against the *reference* `SessionRecord` shape, which is
/// whole-row serde (`#[serde(default)]` covers MISSING, not WRONG-TYPED). This
/// asserts the reference-crate behavior HONESTLY: for `SessionRecord`, a
/// wrong-typed `pid` IS a parse error.
///
/// The PRODUCTION behavior is DIFFERENT and intentional: the engine's
/// `registry::parse_file` (`crates/qd`) reads per-field-permissively, so the SAME
/// fixture SURVIVES there with `pid`/`startedAt` degraded to default and the row
/// still visible to `ls`. `crates/golden`'s parser is 0b's reference shape and is
/// deliberately left unchanged.
#[test]
fn wrong_typed_fixture_is_clean_whole_row_failure_in_reference_shape() {
    let r = parse_record(read("wrong-typed.json").trim());
    assert!(
        r.is_err(),
        "wrong-typed.json (string pid) must fail CLEANLY against the whole-row \
         reference SessionRecord — the engine's per-field-lenient read is the \
         production behavior (see crates/qd registry::parse_file)"
    );
}

#[test]
fn partial_jsonl_fixture_keeps_good_records() {
    let outcomes = parse_jsonl(&read("partial.jsonl"));
    let good = outcomes
        .iter()
        .filter(|o| matches!(o, LineOutcome::Ok(_)))
        .count();
    let skipped = outcomes
        .iter()
        .filter(|o| matches!(o, LineOutcome::Skipped(_)))
        .count();
    assert_eq!(good, 2, "two valid JSONL records survive");
    assert_eq!(
        skipped, 1,
        "the truncated trailing line is skipped, not fatal"
    );
}
