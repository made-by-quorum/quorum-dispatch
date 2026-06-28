//! codex P1, R2 (codex-p1-spec section 4) — provider-field byte-stability +
//! read-back + acting-verb refusal, driving the REAL `qd` binary
//! (`CARGO_BIN_EXE_qd`) against a JAILED, empty HOME (L9a / ADD-4 — never the real
//! home; HOME + ZMX_DIR point into a per-test tempdir).
//!
//! These prove the NEW provider paths WITHOUT touching the rule-8 golden files
//! (the existing golden tests are the no-delta proof; these are additive). They
//! follow the verbs_a4.rs harness shape (forge a registry row, run the bin,
//! assert exit + stdout/stderr) — no new harness invented.
//!
//! Each test carries a MUTATION-EVIDENCE comment naming the mutation it kills.

mod common;

use std::path::Path;
use std::process::Command;

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// Forge a single registry row `<pid>.json` under a freshly-jailed HOME and run
/// `qd <args...>`. Returns (exit_code, stdout, stderr). The jail mirrors
/// verbs_a4.rs: HOME → `<dir>/home`, ZMX_DIR → an empty `<dir>/zmx`.
fn run_qd_with_row(dir: &Path, pid: i64, row_json: &str, args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    common::assert_not_real_home(&home);
    std::fs::write(sessions.join(format!("{pid}.json")), row_json).unwrap();

    let out = Command::new(qd_bin())
        .args(args)
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Strip the P0 wave-1 stable-id lines (`"qdId"` / `"qdIdPrefix"`) before a
/// cross-run byte compare: ids are RANDOM at mint, so two separate jails mint
/// different ids for the same row — the only legitimately-nondeterministic
/// lines in `ls --json`.
fn strip_qd_id_lines(json: &str) -> String {
    json.lines()
        .filter(|l| !l.contains("\"qdId\"") && !l.contains("\"qdIdPrefix\""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// (a) BYTE-STABILITY (codex-p1-spec section 4): a row WITH explicit
/// `"provider": "claude-code"` produces `ls --json` output BYTE-IDENTICAL to the
/// SAME row with the field ABSENT — the absent→claude-code default lives in the
/// join read-back boundary, never on disk. (P0 wave-1: modulo the random
/// stable-id lines, stripped by `strip_qd_id_lines` — see its doc.)
///
/// MUTATION EVIDENCE: flipping the join's absent-provider default to any value
/// other than "claude-code" reds this (the absent-field jail would no longer
/// match the explicit-claude-code jail) — and reds the rule-8 goldens too.
#[test]
fn explicit_claude_code_is_byte_identical_to_absent_field() {
    // Same row content modulo the provider field. Identical pid/sessionId/name so
    // the codes + every other rendered field are identical between the two jails.
    let base = r#"{"pid":90200,"sessionId":"sid-base-0000","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"wk","version":"0.1.0","kind":"claude-code","entrypoint":"claude""#;
    let absent = format!("{base}}}");
    let explicit = format!("{base},\"provider\":\"claude-code\"}}");

    let t_absent = tempfile::tempdir().unwrap();
    let (c1, absent_json, _e1) =
        run_qd_with_row(t_absent.path(), 90200, &absent, &["ls", "--json"]);
    let t_explicit = tempfile::tempdir().unwrap();
    let (c2, explicit_json, _e2) =
        run_qd_with_row(t_explicit.path(), 90200, &explicit, &["ls", "--json"]);

    assert_eq!(c1, 0, "absent-field ls --json exit 0");
    assert_eq!(c2, 0, "explicit-field ls --json exit 0");
    assert_eq!(
        strip_qd_id_lines(&absent_json),
        strip_qd_id_lines(&explicit_json),
        "explicit provider:claude-code must render byte-identical to the absent field"
    );
    assert!(
        absent_json.contains("\"provider\": \"claude-code\""),
        "sanity: the rendered json carries provider:claude-code, got: {absent_json}"
    );
}

/// (b) UNKNOWN-VALUE SURVIVAL + ACTING-VERB REFUSAL (codex-p1-spec section 4 /
/// section 2.3): a row with `"provider": "weird-prov"` SURVIVES the scan and
/// renders the value VERBATIM in `ls --json` (L8 permissive — render never kills
/// the row); but an ACTING verb (`connect` — was `attach`, retired STATE 22)
/// REFUSES it with the exact message + exit 1.
///
/// MUTATION EVIDENCE: bypassing/removing the `refuse_unknown_provider` call site
/// (or the helper) reds the refusal half; making render drop unknown-provider
/// rows reds the survival half.
#[test]
fn weird_provider_survives_render_but_acting_verb_refuses() {
    let row = r#"{"pid":90201,"sessionId":"sid-weird-001","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"wp","version":"0.1.0","kind":"claude-code","entrypoint":"claude","provider":"weird-prov"}"#;

    // Render survival: ls --json shows the value verbatim, row present, exit 0.
    let t1 = tempfile::tempdir().unwrap();
    let (code, json, _err) = run_qd_with_row(t1.path(), 90201, row, &["ls", "--json"]);
    assert_eq!(code, 0, "unknown provider value never kills the scan");
    assert!(
        json.contains("\"provider\": \"weird-prov\""),
        "the unknown provider value renders verbatim, got: {json}"
    );

    // Acting-verb refusal: `qd connect` refuses with the EXACT message + exit 1.
    let t2 = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_with_row(t2.path(), 90201, row, &["connect", "wp"]);
    assert_eq!(code, 1, "connect refuses an unknown provider with exit 1");
    assert_eq!(
        err.trim_end(),
        "qd connect: unknown provider \"weird-prov\" — this engine supports: claude-code.",
        "exact refusal wording (one source of truth: refuse_unknown_provider)"
    );
}

/// (c) WRONG-TYPED DEGRADE (codex-p1-spec section 4 / section 3.1): a wrong-typed
/// `"provider": 7` DEGRADES — the row SURVIVES and renders as claude-code
/// (the from_value field! degrades the wrong-typed value to None, then the join's
/// read-back default supplies "claude-code").
///
/// MUTATION EVIDENCE: dropping the `field!(entry.provider, ...)` row in
/// `from_value` would make a whole-struct parse drop the row (or silently lose the
/// field) — reds this. Flipping the join default reds the rendered value.
#[test]
fn wrong_typed_provider_degrades_to_claude_code() {
    let row = r#"{"pid":90202,"sessionId":"sid-wrong-000","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"wt","version":"0.1.0","kind":"claude-code","entrypoint":"claude","provider":7}"#;
    let t = tempfile::tempdir().unwrap();
    let (code, json, _err) = run_qd_with_row(t.path(), 90202, row, &["ls", "--json"]);
    assert_eq!(
        code, 0,
        "wrong-typed provider degrades, row survives, exit 0"
    );
    assert!(
        json.contains("\"sessionId\": \"sid-wrong-000\""),
        "the row SURVIVES the degrade, got: {json}"
    );
    assert!(
        json.contains("\"provider\": \"claude-code\""),
        "wrong-typed provider degrades to the read-back default, got: {json}"
    );
}
