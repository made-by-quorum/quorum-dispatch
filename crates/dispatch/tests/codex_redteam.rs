//! C-RED — adversarial red-team of the codex adapter's rollout parser
//! (`rollout.rs`) and RPC envelope serde (`rpc.rs`) at the HARDEST input.
//!
//! POSTURE: try to BREAK it. The standard is DEGRADE-NOT-PANIC: under malformed /
//! oversized / adversarial input the adapter must never panic/crash, must derive
//! correct status / open-turn, and must map/discriminate RPC errors correctly —
//! generic codes (`-32600`) discriminated on MESSAGE, not the bare code.
//!
//! Structure: the bulk is DETERMINISTIC + DAEMON-FREE (parser + serde fixtures,
//! always-on — a panic surfaces as a test failure). ONE test is gated on
//! `QD_CODEX_LIVE=1`: the real `-32602 input_too_large` boundary, which must be
//! elicited from a live daemon (oversized `turn/start` input rejected pre-model).
//!
//! Each probe writes an evidence file (adversarial input → observed degradation)
//! into a per-run bundle (`$CCONF_EVIDENCE_DIR/redteam` or a CARGO_TARGET_TMPDIR
//! default), so the oracle (mechanical exhaustion) reads outcomes at
//! source. These probes PUSH PAST the inline `rollout::tests` / `rpc::tests` to
//! hunt NEW break classes.

mod common;
use common::live::*;

use std::path::{Path, PathBuf};

use dispatch::model::SessionStatus;
use dispatch::provider::codex::rollout::{
    derive_status, open_turn_id, parse_filename, parse_line, read_lines, read_stats, RolloutLine,
    RolloutRecord,
};
use dispatch::provider::codex::rpc::{ServerError, TurnResult, INVALID_REQUEST_CODE};

/// The C-RED evidence bundle dir (stable per run; distinct files per probe).
fn red_bundle() -> PathBuf {
    let root = std::env::var("CCONF_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cred-evidence"));
    let dir = root.join("redteam");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a `RolloutRecord` directly (bypassing parse_line) so derive_status /
/// open_turn_id can be probed with ADVERSARIAL line sequences the wire would
/// never emit (foreign / criss-crossed / duplicate turn ids).
fn started(id: Option<&str>) -> RolloutRecord {
    RolloutRecord {
        timestamp: None,
        line: RolloutLine::TaskStarted {
            turn_id: id.map(str::to_owned),
        },
    }
}
fn complete(id: Option<&str>) -> RolloutRecord {
    RolloutRecord {
        timestamp: None,
        line: RolloutLine::TaskComplete {
            turn_id: id.map(str::to_owned),
        },
    }
}

fn write_bytes(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cred-rollouts");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

// ===========================================================================
// PARSER — parse_line / read_lines: degrade-not-panic across the malformed zoo.
// ===========================================================================

#[test]
fn red_parser_nonobject_and_scalar_degrade() {
    // Non-object JSON values + non-JSON bytes → parse_line returns None, never panics.
    let cases = [
        "[1,2,3]",
        "42",
        "-9999999999999999999999999",
        "3.14",
        "\"a bare string\"",
        "true",
        "null",
        "",
        "   ",
        "{",
        "}{",
        "not json at all",
        "{\"unterminated\": ",
        "{\"a\":\"\\uD800\"}", // lone surrogate escape
    ];
    let mut report = String::from("# parse_line non-object / malformed → None (no panic)\n");
    for c in cases {
        let got = parse_line(c);
        // Only an object yields Some; everything here is non-object or invalid.
        assert!(
            got.is_none() || matches!(got.as_ref().unwrap().line, RolloutLine::Other),
            "non-object/malformed must degrade to None/Other, not panic: {c:?} -> {got:?}"
        );
        report.push_str(&format!("{c:?} -> {got:?}\n"));
    }
    ev_text(&red_bundle(), "parser-nonobject.txt", &report);
}

#[test]
fn red_parser_malformed_payload_and_type_degrade() {
    // type / payload of the WRONG json type must not panic; classify to Other or
    // a degraded variant with None fields.
    let cases = [
        r#"{"type":42,"payload":{}}"#,                                  // type non-string
        r#"{"type":["event_msg"],"payload":{}}"#,                       // type array
        r#"{"type":"event_msg","payload":"a string not an object"}"#,  // payload non-object
        r#"{"type":"event_msg","payload":[1,2,3]}"#,                    // payload array
        r#"{"type":"event_msg","payload":{"type":99}}"#,               // inner type non-string
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":123}}"#, // turn_id non-string
        r#"{"type":"session_meta","payload":{"id":[],"cwd":{}}}"#,      // id/cwd wrong types
        r#"{"type":"session_meta"}"#,                                    // payload absent
        r#"{"payload":{"type":"task_started"}}"#,                        // top type absent
        r#"{}"#,                                                         // empty object
    ];
    let mut report = String::from("# parse_line malformed type/payload → degrade (no panic)\n");
    for c in cases {
        let got = parse_line(c);
        assert!(got.is_some(), "a JSON object always yields Some(record): {c}");
        report.push_str(&format!("{c} -> {:?}\n", got.unwrap().line));
    }
    // turn_id:123 (non-string) → TaskStarted{turn_id:None} (degrade), NOT a panic.
    let ts = parse_line(r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":123}}"#)
        .unwrap();
    assert_eq!(ts.line, RolloutLine::TaskStarted { turn_id: None });
    ev_text(&red_bundle(), "parser-malformed-payload.txt", &report);
}

#[test]
fn red_parser_deep_nesting_degrades_not_overflow() {
    // 20k-deep nested array: serde_json's recursion limit must reject it as a parse
    // error (None) — NOT a stack overflow / panic. THE classic parser break class.
    let depth = 20_000;
    let deep = format!(
        r#"{{"type":"event_msg","payload":{}{}{}}}"#,
        "[".repeat(depth),
        "1",
        "]".repeat(depth)
    );
    let got = parse_line(&deep);
    assert!(
        got.is_none(),
        "20k-deep nesting must degrade to None via the serde recursion limit, not overflow"
    );
    // Same through the file path.
    let p = write_bytes("deep.jsonl", deep.as_bytes());
    assert_eq!(read_lines(&p).len(), 0, "deep-nesting line skipped, no panic");
    ev_text(
        &red_bundle(),
        "parser-deep-nesting.txt",
        &format!("depth={depth} -> parse_line None (recursion-limited), read_lines empty\n"),
    );
}

#[test]
fn red_parser_huge_and_nonnumeric_token_count_degrade() {
    // occupancy from last_token_usage.total_tokens: huge/float/string/negative →
    // as_u64 None → occupancy None (no panic, no wrong number).
    let cases = [
        (r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":18446744073709551615}}}}"#, Some(u64::MAX)),
        (r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":99999999999999999999999}}}}"#, None), // > u64::MAX
        (r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":-5}}}}"#, None), // negative
        (r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":3.5}}}}"#, None), // float
        (r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":"123"}}}}"#, None), // string
        (r#"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"#, None), // empty info
        (r#"{"type":"event_msg","payload":{"type":"token_count"}}"#, None), // no info
    ];
    let mut report = String::from("# token_count occupancy adversarial → None on non-u64 (no panic)\n");
    for (c, want) in cases {
        let got = parse_line(c).unwrap().line;
        match got {
            RolloutLine::TokenCount { occupancy } => {
                assert_eq!(occupancy, want, "occupancy degrade: {c}");
                report.push_str(&format!("{c} -> occupancy={occupancy:?}\n"));
            }
            other => panic!("expected TokenCount, got {other:?} for {c}"),
        }
    }
    ev_text(&red_bundle(), "parser-token-count.txt", &report);
}

#[test]
fn red_parser_oversized_line_degrades() {
    // A ~2 MiB single line (valid JSON with a huge string) must parse OR degrade —
    // never panic / never OOM at this bounded size.
    let big = "x".repeat(2 * 1024 * 1024);
    let line = format!(
        r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":"{big}"}}}}"#
    );
    let got = parse_line(&line).expect("a 2MiB valid line parses (object)");
    match got.line {
        RolloutLine::AgentMessage { message } => {
            assert_eq!(message.as_deref().map(str::len), Some(2 * 1024 * 1024));
        }
        other => panic!("expected AgentMessage, got {other:?}"),
    }
    // An oversized line of GARBAGE (not JSON) → None, no panic.
    let garbage = "{".repeat(2 * 1024 * 1024);
    assert!(parse_line(&garbage).is_none(), "2MiB of '{{' degrades to None");
    ev_text(
        &red_bundle(),
        "parser-oversized.txt",
        "2MiB valid line parses; 2MiB garbage -> None; no panic/OOM at bounded size\n",
    );
}

#[test]
fn red_parser_crlf_bom_nul_torn_tail() {
    let good = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"A"}}"#;
    let done = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}}"#;
    let mut report = String::from("# CRLF / BOM / NUL / torn-tail file probes\n");

    // CRLF line endings: split('\n') leaves a trailing '\r' (JSON-trailing whitespace) → parses.
    let crlf = format!("{good}\r\n{done}\r\n");
    let p = write_bytes("crlf.jsonl", crlf.as_bytes());
    let lines = read_lines(&p);
    assert_eq!(lines.len(), 2, "CRLF lines parse (trailing \\r is JSON whitespace)");
    assert_eq!(derive_status(&lines), Some(SessionStatus::Idle));
    report.push_str(&format!("CRLF -> {} lines, Idle\n", lines.len()));

    // BOM prefix on the first line → that line fails to parse (BOM not JSON ws) →
    // skipped; the second line survives. Degrade, no panic.
    let bom = format!("\u{FEFF}{good}\n{done}\n");
    let p = write_bytes("bom.jsonl", bom.as_bytes());
    let lines = read_lines(&p);
    assert_eq!(lines.len(), 1, "BOM-prefixed first line skipped, second survives");
    report.push_str(&format!("BOM -> {} lines (first skipped)\n", lines.len()));

    // Embedded NUL bytes (valid UTF-8 U+0000) inside an otherwise-good file: the
    // NUL-bearing line is invalid JSON → skipped; clean lines survive. No panic.
    let nul = format!("{good}\n\u{0000}{done}\n");
    let p = write_bytes("nul.jsonl", nul.as_bytes());
    let lines = read_lines(&p);
    assert!(lines.len() <= 2 && !lines.is_empty(), "NUL line degrades, others survive");
    report.push_str(&format!("NUL -> {} lines\n", lines.len()));

    // Torn tail: a truncated final line (partial JSON, no newline) → skipped; the
    // complete prior lines survive and still derive correctly.
    let torn = format!("{good}\n{done}\n{{\"type\":\"event_msg\",\"payl");
    let p = write_bytes("torn.jsonl", torn.as_bytes());
    let lines = read_lines(&p);
    assert_eq!(lines.len(), 2, "torn final line skipped, prior survive");
    assert_eq!(derive_status(&lines), Some(SessionStatus::Idle));
    report.push_str(&format!("torn-tail -> {} lines, Idle\n", lines.len()));

    ev_text(&red_bundle(), "parser-crlf-bom-nul-torn.txt", &report);
}

#[test]
fn red_read_lines_interleaved_and_read_stats_degrade() {
    let good = r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}}"#;
    // Interleave good lines with every flavor of UTF-8 garbage; good lines survive,
    // bad lines skipped PER-LINE (the file stays valid UTF-8 so read_to_string ok).
    let content = format!("{good}\n\u{0000}not json\nplain text\n[]\n42\nnull\n\n   \n{good}\n");
    let p = write_bytes("interleaved.jsonl", content.as_bytes());
    let lines = read_lines(&p);
    assert_eq!(lines.len(), 2, "two good lines survive a sea of UTF-8 garbage");
    let stats = read_stats(&p, true);
    assert_eq!(stats.turns, 2);

    // NON-UTF-8 byte ANYWHERE → read_to_string fails → the WHOLE file degrades to
    // EMPTY (L8 designed-degrade: it is whole-file, not per-line — a documented
    // property worth pinning: a single 0xff byte loses every good line too).
    let mut mixed: Vec<u8> = Vec::new();
    mixed.extend_from_slice(good.as_bytes());
    mixed.push(b'\n');
    mixed.push(0xff); // invalid UTF-8
    mixed.extend_from_slice(good.as_bytes());
    let p2 = write_bytes("nonutf8.jsonl", &mixed);
    assert!(
        read_lines(&p2).is_empty(),
        "a single non-UTF-8 byte degrades the WHOLE file to empty (whole-file, not per-line)"
    );

    ev_text(
        &red_bundle(),
        "parser-interleaved.txt",
        &format!(
            "UTF-8 interleaved good+garbage -> {} good lines, turns={} (per-line skip)\n\
             non-UTF-8 byte anywhere -> WHOLE file empty (read_to_string degrade, documented)\n",
            lines.len(),
            stats.turns
        ),
    );
}

// ===========================================================================
// derive_status / open_turn_id — adversarial turn-id sequences.
// ===========================================================================

#[test]
fn red_derive_status_adversarial_turn_ids() {
    let mut report = String::from("# derive_status / open_turn_id adversarial turn-id sequences\n");
    let probe = |name: &str, recs: &[RolloutRecord], want: Option<SessionStatus>| -> String {
        let got = derive_status(recs);
        assert_eq!(got, want, "{name}: derive_status");
        // Channel invariant (the WEAKER, TRUE one): an open_turn_id implies Busy.
        if let Some(id) = open_turn_id(recs) {
            assert!(!id.is_empty(), "{name}: open_turn_id never returns empty");
            assert_eq!(got, Some(SessionStatus::Busy), "{name}: open id ⇒ Busy");
        }
        format!("{name}: derive={got:?} open_turn_id={:?}\n", open_turn_id(recs))
    };

    // Lone task_complete (no start) → anchor present, nothing open → Idle (degrade).
    report.push_str(&probe("lone_complete", &[complete(Some("X"))], Some(SessionStatus::Idle)));
    // Duplicate started same id, one complete → one still open → Busy.
    report.push_str(&probe(
        "dup_start_one_done",
        &[started(Some("A")), started(Some("A")), complete(Some("A"))],
        Some(SessionStatus::Busy),
    ));
    // Criss-cross: start A, start B, complete B → A still open → Busy, open id = A.
    report.push_str(&probe(
        "crisscross_complete_b",
        &[started(Some("A")), started(Some("B")), complete(Some("B"))],
        Some(SessionStatus::Busy),
    ));
    assert_eq!(
        open_turn_id(&[started(Some("A")), started(Some("B")), complete(Some("B"))]).as_deref(),
        Some("A"),
        "completing B leaves A as the open steer target"
    );
    // id-less start + id-less complete → the "" placeholder is balanced → Idle.
    report.push_str(&probe(
        "idless_start_idless_done",
        &[started(None), complete(None)],
        Some(SessionStatus::Idle),
    ));
    // id-less open turn → Busy, but open_turn_id None (not a usable steer precond).
    {
        let recs = [started(None)];
        assert_eq!(derive_status(&recs), Some(SessionStatus::Busy));
        assert_eq!(open_turn_id(&recs), None, "id-less open ⇒ Busy but no steer id");
        report.push_str("idless_open: derive=Busy open_turn_id=None (documented channel exception)\n");
    }
    // Many completes, few starts → never underflow/panic → Idle.
    report.push_str(&probe(
        "extra_completes",
        &[started(Some("A")), complete(Some("A")), complete(Some("B")), complete(Some("C"))],
        Some(SessionStatus::Idle),
    ));

    ev_text(&red_bundle(), "derive-adversarial-ids.txt", &report);
}

#[test]
fn red_derive_status_foreign_complete_balances_by_design() {
    // KNOWN/ACCEPTED LIMITATION (RULED, NOT a break-class): a FOREIGN
    // task_complete (an id matching no open turn) closes the OLDEST open turn
    // (best-effort balance, rollout.rs:240-259 docstring) → a genuinely-open turn A
    // is reported IDLE when a stray complete for an unrelated id arrives. A tampered
    // rollout can therefore misreport status. RULING RATIONALE: (1) the rollout is
    // codex's OWN durable local state under CODEX_HOME — tampering requires local
    // write (an attacker there can do worse), so it is OUTSIDE the threat model;
    // (2) close-oldest is the DELIBERATE co-attach id-misalignment handling, and
    // exact-id-only matching would BREAK the legitimate co-attach case on a trusted
    // file. So this stays by-design; encoded here as the documented behavior.
    let recs = [started(Some("REAL-OPEN-A")), complete(Some("FOREIGN-ID-Z"))];
    assert_eq!(
        derive_status(&recs),
        Some(SessionStatus::Idle),
        "BY-DESIGN best-effort balance: a foreign complete closes the oldest open turn"
    );
    ev_text(
        &red_bundle(),
        "derive-foreign-complete-BY-DESIGN.txt",
        "started(REAL-OPEN-A) + complete(FOREIGN-ID-Z) -> Idle (best-effort balance, rollout.rs:240-259).\n\
         KNOWN/ACCEPTED LIMITATION, RULED (NOT a break-class): a tampered \
         local rollout can misreport status. Out of threat model (tampering needs local write \
         under CODEX_HOME); close-oldest is deliberate co-attach id-misalignment handling and \
         exact-id-only would break the legitimate co-attach case. Stays by-design.\n",
    );
}

#[test]
fn red_parse_filename_adversarial() {
    let bad = [
        "rollout-.jsonl",
        "rollout-2026-06-07T02-09-07-not-a-real-uuid-here.jsonl",
        "rollout-xxxxxxxx-04d3-7400-8d95-f55d41e961e4.jsonl", // non-hex first group
        "rollout-019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl", // no timestamp (all 5 groups = uuid, ts empty)
        "rollout-2026-019ea0b3-04d3-7400-8d95-f55d41e96Ze4.jsonl", // non-hex char Z
        "notes.jsonl",
        "",
        "rollout-",
        "rollout-\u{1f600}-04d3-7400-8d95-f55d41e961e4.jsonl", // emoji group
    ];
    let mut report = String::from("# parse_filename adversarial → None (no panic)\n");
    for b in bad {
        let got = parse_filename(b);
        assert!(got.is_none(), "adversarial filename must degrade to None: {b:?} -> {got:?}");
        report.push_str(&format!("{b:?} -> None\n"));
    }
    // A well-formed one still parses (control).
    let ok = parse_filename("rollout-2026-06-07T02-09-07-019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl");
    assert!(ok.is_some());
    ev_text(&red_bundle(), "parse-filename-adversarial.txt", &report);
}

// ===========================================================================
// rpc.rs — envelope serde + error discrimination.
// ===========================================================================

#[test]
fn red_server_error_serde_strictness() {
    // ServerError REQUIRES code:i64 + message:String (NOT serde(default)). Malformed
    // error objects FAIL to deserialize — the driver's classify() then falls through
    // to Unknown (degrade to Timeout), never a panic. Pin the requirement here.
    let must_fail = [
        r#"{"code":-32600}"#,                 // no message
        r#"{"message":"x"}"#,                  // no code
        r#"{"code":"-32600","message":"x"}"#,  // code as string
        r#"{"code":-32600.5,"message":"x"}"#,  // code fractional float
        r#"{"code":99999999999999999999999,"message":"x"}"#, // code > i64
        r#"{}"#,                                // empty
        r#"{"code":null,"message":"x"}"#,       // code null
        r#"{"code":-32600,"message":42}"#,      // message non-string
    ];
    let mut report = String::from("# ServerError serde strictness (malformed → Err, not panic)\n");
    for c in must_fail {
        let got: Result<ServerError, _> = serde_json::from_str(c);
        assert!(got.is_err(), "malformed ServerError must fail to deserialize: {c}");
        report.push_str(&format!("{c} -> Err (degraded by driver to Unknown/Timeout)\n"));
    }
    // The two REAL -32600 frames parse (extra fields ignored).
    for c in [
        r#"{"code":-32600,"message":"no rollout found for thread id X","data":{"any":1}}"#,
        r#"{"code":-32602,"message":"input too large"}"#,
        r#"{"code":-9223372036854775808,"message":"i64::MIN ok"}"#,
    ] {
        let got: ServerError = serde_json::from_str(c).expect("well-formed ServerError parses");
        report.push_str(&format!("{c} -> code={}\n", got.code));
    }
    ev_text(&red_bundle(), "rpc-servererror-serde.txt", &report);
}

#[test]
fn red_turn_result_shapes_and_corruption() {
    // turn_id() across both wire shapes + corruption. Malformed-typed ids FAIL serde
    // (→ driver maps to Transport, no panic); structurally-empty → None.
    let mut report = String::from("# TurnResult shapes + corruption\n");
    let id_cases = [
        (r#"{"turn":{"id":"NESTED"}}"#, Some("NESTED")),
        (r#"{"turnId":"FLAT"}"#, Some("FLAT")),
        (r#"{"turnId":"FLAT","turn":{"id":"NESTED"}}"#, Some("FLAT")), // flat wins
        (r#"{}"#, None),
        (r#"{"turn":null,"turnId":null}"#, None),
        (r#"{"turnId":""}"#, None),                 // empty flat filtered
        (r#"{"turn":{"id":""}}"#, None),            // empty nested filtered
        (r#"{"turn":{}}"#, None),                    // nested default id ""
    ];
    for (c, want) in id_cases {
        let parsed: TurnResult = serde_json::from_str(c).expect("permissive TurnResult parses");
        assert_eq!(parsed.turn_id(), want, "turn_id() for {c}");
        report.push_str(&format!("{c} -> turn_id()={:?}\n", parsed.turn_id()));
    }
    // Corruption: wrong-typed ids FAIL deserialization (driver → Transport error).
    for c in [r#"{"turn":{"id":123}}"#, r#"{"turnId":123}"#, r#"{"turn":[1,2]}"#] {
        let got: Result<TurnResult, _> = serde_json::from_str(c);
        assert!(got.is_err(), "wrong-typed turn id must fail serde (→Transport): {c}");
        report.push_str(&format!("{c} -> Err (driver: Transport)\n"));
    }
    ev_text(&red_bundle(), "rpc-turnresult.txt", &report);
}

#[test]
fn red_neg32600_discriminated_on_message_not_code() {
    // -32600 is codex's GENERIC code (rpc.rs:85). The production discriminator
    // (resume_daemon.rs:923 is_no_rollout) matches the MESSAGE PREFIX
    // "no rollout found", NOT the bare code. Pin the CONTRACT that predicate relies
    // on: the two real -32600 messages are separable by prefix, and the stale-steer
    // message is NOT mistaken for no-rollout.
    const NO_ROLLOUT_PREFIX: &str = "no rollout found"; // mirrors resume_daemon.rs:93
    let is_no_rollout = |m: &str| m.trim_start().starts_with(NO_ROLLOUT_PREFIX);

    let no_rollout = ServerError {
        code: INVALID_REQUEST_CODE,
        message: "no rollout found for thread id 019e9f4b-adb9-7ec1-b4ed-08247847426a".into(),
    };
    let stale_steer = ServerError {
        code: INVALID_REQUEST_CODE,
        message: "expected active turn id `A` but found `B`".into(),
    };
    assert_eq!(no_rollout.code, stale_steer.code, "same generic code -32600");
    assert!(is_no_rollout(&no_rollout.message), "no-rollout matched by message prefix");
    assert!(!is_no_rollout(&stale_steer.message), "stale-steer NOT matched (correct discrimination)");

    // Adversarial: leading whitespace tolerated (trim_start); a message that merely
    // CONTAINS the phrase later is NOT matched (starts_with, not contains) — so a
    // steer message that happened to mention 'no rollout found' downstream is safe.
    assert!(is_no_rollout("   no rollout found for thread id Z"));
    assert!(!is_no_rollout("turn failed: no rollout found later in text"));
    // Casing churn risk (DOCUMENTED): an upper-cased variant would NOT match — a
    // wire-wording drift across 0.x would break discrimination. Flagged, by-design.
    assert!(!is_no_rollout("No Rollout Found for thread id Z"));

    ev_text(
        &red_bundle(),
        "rpc-neg32600-discrimination.txt",
        "two real -32600 messages separable by prefix; stale-steer not false-matched; \
         starts_with (not contains); leading ws trimmed. CASING-DRIFT risk documented \
         (mirrors resume_daemon.rs is_no_rollout @ :923).\n",
    );
}

// ===========================================================================
// LIVE (gated QD_CODEX_LIVE=1) — the real -32602 input_too_large @ 1 MiB.
// ===========================================================================

#[test]
fn red_live_oversized_turn_input_is_32602_at_1mib() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping the live -32602 boundary probe");
        return;
    }
    use dispatch::provider::codex::{AppServerRpc, ClientInfo, RpcError, WsAppServer};
    use std::sync::Arc;
    use std::time::Duration;

    let jail = make_jail("red-32602");
    let codex_home = jail.join("codex-home");
    let bundle = red_bundle();
    let pids = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
    let _belt = ReapAll(pids.clone());

    // Spawn a real codex daemon (cred-free: thread/start is pre-model).
    let start = run_qd(
        &jail,
        &["start", "red32602", "--provider", "codex", "--cwd",
          jail.join("work").to_string_lossy().as_ref()],
    );
    assert!(start.status.success(), "daemon up: {}", String::from_utf8_lossy(&start.stderr));
    let rows = codex_rows(&jail);
    assert_eq!(rows.len(), 1, "one codex row");
    let row = &rows[0];
    let pid = row.pid.unwrap();
    pids.lock().unwrap().push(pid);
    let endpoint = row.endpoint.clone().unwrap();
    let thread_id = row.session_id.clone().unwrap();

    // Connect raw RPC and submit an oversized turn/start input. The 1 MiB size
    // rejection (-32602 input_too_large, A3 P18) fires PRE-MODEL, so no credential
    // is consumed. We assert the OVER case is -32602; the boundary is bracketed by
    // the under case NOT being -32602 (no valid model cred in the jail → a different
    // error class, never input_too_large).
    let rpc = WsAppServer::connect(&endpoint, Duration::from_secs(10)).expect("connect");
    let client = ClientInfo { name: "cred".into(), title: None, version: "0".into() };
    rpc.initialize(&client).expect("initialize");
    let _ = rpc.initialized();
    rpc.set_request_timeout(Duration::from_secs(30));

    let one_mib = 1024 * 1024usize;
    let over = "x".repeat(one_mib + 64);
    let over_res = rpc.turn_start(&thread_id, &over);
    let over_code = match &over_res {
        Err(RpcError::Protocol(e)) => Some(e.code),
        _ => None,
    };
    assert_eq!(
        over_code,
        Some(-32602),
        "oversized (>1 MiB) turn/start input must elicit -32602 input_too_large, got {over_res:?}"
    );

    // Under-boundary: a tiny input is NOT rejected for size — without a valid model
    // cred it errors differently (or the call dispatches); in NO case is it -32602.
    let under_res = rpc.turn_start(&thread_id, "ok");
    let under_is_32602 = matches!(&under_res, Err(RpcError::Protocol(e)) if e.code == -32602);
    assert!(
        !under_is_32602,
        "a tiny input must NOT yield -32602 (brackets the 1 MiB boundary): {under_res:?}"
    );
    let _ = rpc.close();

    ev_text(
        &bundle,
        "live-32602-boundary.txt",
        &format!(
            "over_bytes={} -> {over_res:?} (code={over_code:?}, want -32602)\n\
             under('ok') -> {under_res:?} (is_32602={under_is_32602}, want false)\n\
             => -32602 input_too_large fires at the >1 MiB boundary, pre-model (A3 P18).\n",
            one_mib + 64
        ),
    );

    // Teardown + no-survivor belt.
    let _ = run_qd(&jail, &["stop", "red32602"]);
    wait_dead(pid);
    assert!(!dispatch::effects::is_pid_alive(pid as i32), "daemon reaped");
    assert!(!jail_codex_daemon_alive(&codex_home), "no survivor");
    *pids.lock().unwrap() = Vec::new();
    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// ROUND 2 — tail-clustered seams the round-1 battery did not reach:
//   - read_stats preview truncation at the `chars().take(200)` boundary under
//     extreme unicode (combining marks / RTL / zero-width / 4-byte emoji);
//   - JSON duplicate keys (serde last-wins) on type / payload / turn_id / tokens;
//   - very-many-open-turns SCALE (~100k) for derive_status / open_turn_id;
//   - adversarial top-level `timestamp` (non-string / huge / empty) → last_ts;
//   - read_stats preview `split_off` boundary (exactly 5 / 6 / 7 agent_messages);
//   - fabricate-rollout-at-transcript_path → drive the parser END-TO-END through
//     the REAL `qd ls` cold-discovery surface (connectionless: no daemon / pid /
//     cred — a parser panic would crash the binary, exit 101 + "panicked").
// ===========================================================================

/// Build one rollout JSONL line as a string with serde-correct escaping (so
/// adversarial unicode in a message can never break the surrounding JSON).
fn agent_msg_line(message: &str) -> String {
    serde_json::json!({"type":"event_msg","payload":{"type":"agent_message","message":message}})
        .to_string()
}

/// Write a fabricated rollout into the codex date tree
/// (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`) so the real
/// `scan_transcripts` / `date_walk_for_id` surfaces it. The filename keeps the
/// `rollout-<ISO>-<uuidv7>` shape `parse_filename` requires.
fn write_rollout_in_tree(codex_home: &Path, uuid: &str, body: &str) -> PathBuf {
    let dir = codex_home.join("sessions").join("2026").join("06").join("29");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("rollout-2026-06-29T12-00-00-{uuid}.jsonl"));
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn red_read_stats_preview_unicode_truncation_boundary() {
    // The preview is `message.chars().take(200).collect()` — it truncates on
    // UNICODE SCALAR VALUES (chars), not bytes and not grapheme CLUSTERS. Probe
    // that boundary with the nastiest unicode: the result must always be (a) valid
    // UTF-8 (a String always is), (b) ≤ 200 chars, and (c) produced without panic.
    // The characterization worth pinning: take(200) can SPLIT a grapheme cluster
    // (base + combining marks) but NEVER a scalar — so no mojibake / no panic.
    let mut report = String::from("# read_stats preview unicode truncation @ chars().take(200)\n");

    // (1) 4-byte emoji: each is ONE scalar → 250 emoji = 250 chars → take(200) =
    //     200 emoji = 800 bytes. Bounded, valid, exactly 200 chars.
    let emoji = "\u{1F600}".repeat(250);
    // (2) base + TWO combining marks ("e" + U+0301 + U+0302) = 3 chars/grapheme;
    //     70 graphemes = 210 chars → take(200) splits the 67th grapheme mid-cluster.
    let combining = "e\u{0301}\u{0302}".repeat(70);
    // (3) RTL override + zero-width + BOM interleaved with ASCII, > 200 chars.
    let bidi = "a\u{202E}b\u{200B}c\u{FEFF}".repeat(50); // 6 chars * 50 = 300 chars
    // (4) a lone-ish ASCII control mix (TAB/VT) — valid UTF-8, no panic.
    let controls = "x\u{0009}\u{000B}\u{007F}".repeat(60); // 240 chars

    for (name, msg) in [
        ("emoji", &emoji),
        ("combining", &combining),
        ("bidi_zerowidth_bom", &bidi),
        ("controls", &controls),
    ] {
        let p = write_bytes(&format!("preview-{name}.jsonl"), agent_msg_line(msg).as_bytes());
        let stats = read_stats(&p, true);
        let previews = stats.last_turns.expect("preview present for a non-empty agent_message");
        assert_eq!(previews.len(), 1, "{name}: one agent_message → one preview");
        let text = &previews[0].text;
        let nchars = text.chars().count();
        assert!(nchars <= 200, "{name}: preview ≤ 200 scalar values, got {nchars}");
        // The full message had > 200 chars, so the preview must be exactly 200.
        assert_eq!(nchars, 200, "{name}: truncated to exactly 200 chars");
        // Byte-safety: the preview is a prefix of the message by SCALAR boundary
        // (chars().take(200)); reconstruct the expected prefix and compare.
        let want: String = msg.chars().take(200).collect();
        assert_eq!(text, &want, "{name}: preview == scalar-prefix(200), never a torn scalar");
        report.push_str(&format!(
            "{name}: msg_chars={} preview_chars={} preview_bytes={} (valid UTF-8, no panic)\n",
            msg.chars().count(),
            nchars,
            text.len()
        ));
    }
    ev_text(&red_bundle(), "round2-preview-unicode-boundary.txt", &report);
}

#[test]
fn red_parser_duplicate_keys_last_wins() {
    // serde_json is LAST-WINS on duplicate object keys (a documented, deterministic
    // degrade — NOT a panic and NOT first-wins). Pin the contract at every level the
    // parser reads: top-level `type`, `payload`, nested `turn_id`, and `total_tokens`.
    let mut report = String::from("# duplicate JSON keys → serde last-wins (deterministic, no panic)\n");

    // top-level `type` duplicated: session_meta then event_msg → event_msg wins,
    // so the nested task_started is classified.
    let c1 = r#"{"type":"session_meta","type":"event_msg","payload":{"type":"task_started","turn_id":"A"}}"#;
    assert_eq!(
        parse_line(c1).unwrap().line,
        RolloutLine::TaskStarted { turn_id: Some("A".into()) },
        "top-level type dup → last (event_msg) wins"
    );

    // nested `turn_id` duplicated → LAST.
    let c2 = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"FIRST","turn_id":"LAST"}}"#;
    assert_eq!(
        parse_line(c2).unwrap().line,
        RolloutLine::TaskStarted { turn_id: Some("LAST".into()) },
        "turn_id dup → LAST"
    );

    // `payload` duplicated → last payload object wins (agent_message → task_complete).
    let c3 = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"X"},"payload":{"type":"task_complete","turn_id":"Z"}}"#;
    assert_eq!(
        parse_line(c3).unwrap().line,
        RolloutLine::TaskComplete { turn_id: Some("Z".into()) },
        "payload dup → last payload wins"
    );

    // `total_tokens` duplicated inside last_token_usage → last (42) wins.
    let c4 = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":1,"total_tokens":42}}}}"#;
    assert_eq!(
        parse_line(c4).unwrap().line,
        RolloutLine::TokenCount { occupancy: Some(42) },
        "total_tokens dup → last (42) wins"
    );

    // `type` discriminator duplicated INSIDE payload (event sub-type) → last wins.
    let c5 = r#"{"type":"event_msg","payload":{"type":"task_started","type":"task_complete","turn_id":"Q"}}"#;
    assert_eq!(
        parse_line(c5).unwrap().line,
        RolloutLine::TaskComplete { turn_id: Some("Q".into()) },
        "payload.type dup → last sub-type wins"
    );

    for (c, label) in [
        (c1, "toplevel-type"),
        (c2, "turn_id"),
        (c3, "payload"),
        (c4, "total_tokens"),
        (c5, "payload-type"),
    ] {
        report.push_str(&format!("{label}: {c} -> {:?}\n", parse_line(c).unwrap().line));
    }
    ev_text(&red_bundle(), "round2-duplicate-keys.txt", &report);
}

#[test]
fn red_derive_status_many_open_turns_scale() {
    // SCALE: ~100k still-open task_started records. derive_status must stay correct
    // (Busy) and open_turn_id must return the LAST open id — with NO overflow / no
    // panic / no quadratic blowup for the all-open shape (open_turns is an O(n) push).
    let n = 100_000usize;
    let mut recs: Vec<RolloutRecord> = Vec::with_capacity(n);
    for i in 0..n {
        let id = format!("turn-{i}");
        recs.push(started(Some(id.as_str())));
    }
    assert_eq!(derive_status(&recs), Some(SessionStatus::Busy), "100k open turns → Busy");
    assert_eq!(
        open_turn_id(&recs).as_deref(),
        Some("turn-99999"),
        "open_turn_id = the LAST still-open turn id at scale"
    );

    // A fully-BALANCED large rollout: 2k started then 2k completed (same ids, FIFO).
    // Correctness at scale: every turn balances → Idle, no underflow, no panic.
    // NOTE (characterized perf seam, NOT a break): the matched-complete path is
    // O(n²) (position()+remove on a shrinking Vec) — a 100k-COMPLETED rollout would
    // be quadratic. It degrades to SLOW, never crashes; codex's own rollout never
    // reaches that scale (one session, finite turns). Flagged to the oracle, not a
    // break-class. Kept at 2k here so the test itself stays sub-second.
    let m = 2_000usize;
    let mut bal: Vec<RolloutRecord> = Vec::with_capacity(m * 2);
    for i in 0..m {
        let id = format!("b-{i}");
        bal.push(started(Some(id.as_str())));
    }
    for i in 0..m {
        let id = format!("b-{i}");
        bal.push(complete(Some(id.as_str())));
    }
    assert_eq!(derive_status(&bal), Some(SessionStatus::Idle), "2k balanced → Idle");
    assert_eq!(open_turn_id(&bal), None, "2k balanced → no open turn");

    ev_text(
        &red_bundle(),
        "round2-scale-open-turns.txt",
        &format!(
            "{n} open task_started -> Busy, open_turn_id=turn-{}\n\
             {m} started + {m} completed (balanced) -> Idle, open_turn_id=None\n\
             PERF SEAM (characterized, not a break): matched-complete path is O(n^2) \
             (position()+remove on a shrinking Vec); a 100k-COMPLETED rollout degrades \
             to SLOW, never panics. Out of realistic scale for a single codex session.\n",
            n - 1
        ),
    );
}

#[test]
fn red_read_stats_adversarial_timestamps() {
    // The top-level `timestamp` feeds `stats.last_timestamp` (last-wins among
    // NON-EMPTY STRING timestamps). Adversarial timestamps (number / array / object
    // / empty / huge) must be ignored (parse_line takes it via `as_str` only), never
    // panic, and never displace a real string timestamp.
    let num = r#"{"timestamp":12345,"type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}}"#;
    let arr = r#"{"timestamp":[1,2,3],"type":"event_msg","payload":{"type":"task_complete","turn_id":"B"}}"#;
    let empty = r#"{"timestamp":"","type":"event_msg","payload":{"type":"task_complete","turn_id":"C"}}"#;
    let obj = r#"{"timestamp":{"nested":true},"type":"event_msg","payload":{"type":"task_complete","turn_id":"D"}}"#;
    let huge = r#"{"timestamp":99999999999999999999999999,"type":"event_msg","payload":{"type":"task_complete","turn_id":"E"}}"#;
    let good = r#"{"timestamp":"2026-06-29T00:00:00Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"F"}}"#;

    // Mixed file: the only valid STRING timestamp is `good` → it must be last_ts,
    // regardless of the adversarial neighbors before/after it.
    let mixed = format!("{num}\n{arr}\n{empty}\n{good}\n{obj}\n{huge}\n");
    let p = write_bytes("ts-mixed.jsonl", mixed.as_bytes());
    let stats = read_stats(&p, false);
    assert_eq!(stats.turns, 6, "all six task_complete lines counted (no line lost)");
    assert_eq!(
        stats.last_timestamp.as_deref(),
        Some("2026-06-29T00:00:00Z"),
        "last_timestamp = the only valid non-empty STRING ts; adversarial ones ignored"
    );

    // All-adversarial file (no string timestamp anywhere) → last_timestamp None.
    let all_bad = format!("{num}\n{arr}\n{empty}\n{obj}\n{huge}\n");
    let p2 = write_bytes("ts-all-bad.jsonl", all_bad.as_bytes());
    let stats2 = read_stats(&p2, false);
    assert_eq!(stats2.turns, 5);
    assert_eq!(stats2.last_timestamp, None, "no valid string ts → last_timestamp None (degrade)");

    // A huge timestamp delivered AS A STRING is a valid string → kept verbatim
    // (it is opaque text to us; we never parse it as a number).
    let huge_str = r#"{"timestamp":"99999999999999999999999999","type":"event_msg","payload":{"type":"task_complete","turn_id":"G"}}"#;
    let p3 = write_bytes("ts-huge-string.jsonl", huge_str.as_bytes());
    assert_eq!(
        read_stats(&p3, false).last_timestamp.as_deref(),
        Some("99999999999999999999999999"),
        "a huge NUMERIC-LOOKING string ts is opaque text, kept verbatim"
    );

    ev_text(
        &red_bundle(),
        "round2-adversarial-timestamps.txt",
        "number/array/object/empty/huge-number timestamps ignored (as_str None); a real \
         string ts survives as last_timestamp; an all-adversarial file → None; a huge \
         numeric-looking STRING ts kept verbatim. No panic.\n",
    );
}

#[test]
fn red_read_stats_preview_split_off_boundary() {
    // The preview keeps the LAST 6 agent_messages via `split_off(n.saturating_sub(6))`.
    // Probe the exact boundary at 5 / 6 / 7 — saturating_sub guards underflow, and at
    // 7 the FIRST message must be dropped (only the last 6 survive), in order.
    let build = |k: usize| -> PathBuf {
        let mut lines = String::new();
        for i in 0..k {
            lines.push_str(&agent_msg_line(&format!("m{i}")));
            lines.push('\n');
        }
        write_bytes(&format!("split-{k}.jsonl"), lines.as_bytes())
    };

    // 5 → all 5 kept (split_off(0)).
    let s5 = read_stats(&build(5), true).last_turns.expect("preview");
    assert_eq!(s5.len(), 5, "5 messages → 5 previews");
    assert_eq!(s5[0].text, "m0");
    assert_eq!(s5[4].text, "m4");

    // 6 → all 6 kept (split_off(0), the boundary).
    let s6 = read_stats(&build(6), true).last_turns.expect("preview");
    assert_eq!(s6.len(), 6, "6 messages → 6 previews (boundary)");
    assert_eq!(s6[0].text, "m0");
    assert_eq!(s6[5].text, "m5");

    // 7 → only the LAST 6 (split_off(1)); m0 dropped, m1..m6 kept in order.
    let s7 = read_stats(&build(7), true).last_turns.expect("preview");
    assert_eq!(s7.len(), 6, "7 messages → last 6 previews");
    assert_eq!(s7[0].text, "m1", "m0 dropped; the window starts at m1");
    assert_eq!(s7[5].text, "m6", "the most-recent message is last");

    ev_text(
        &red_bundle(),
        "round2-split-off-boundary.txt",
        "agent_message previews: 5→5, 6→6 (boundary), 7→last-6 (m0 dropped). \
         split_off(n.saturating_sub(6)) — no underflow, no panic.\n",
    );
}

#[test]
fn red_fabricate_rollout_cold_discovery_end_to_end() {
    // END-TO-END through the REAL `qd ls` verb surface — CONNECTIONLESS (no daemon,
    // no pid, no credential). We fabricate rollouts at `transcript_path()` (the codex
    // date tree) and let `qd ls --all` drive its cold-discovery gather
    // (scan_transcripts → parse_filename → read_stats) over them. The standard under
    // test: the BINARY degrades-not-panics on a fabricated/adversarial rollout (a
    // parser panic would crash qd: exit 101 + "panicked"). We assert at SOURCE by
    // also calling the production parser on the same files (the fns the gather uses).
    let jail = make_jail("red-cold");
    let codex_home = jail.join("codex-home");
    let bundle = red_bundle();

    // (A) A WELL-FORMED fabricated rollout: a completed turn with a cwd + a preview.
    let uuid_ok = "019ea0b3-04d3-7400-8d95-f55d41e961e4";
    let body_ok = [
        r#"{"timestamp":"2026-06-29T12:00:00.000Z","type":"session_meta","payload":{"id":"019ea0b3-04d3-7400-8d95-f55d41e961e4","cwd":"/jail/work","cli_version":"0.143.0"}}"#,
        r#"{"timestamp":"2026-06-29T12:00:01.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"T1"}}"#,
        r#"{"timestamp":"2026-06-29T12:00:02.000Z","type":"event_msg","payload":{"type":"agent_message","message":"COLD-FABRICATED-PREVIEW"}}"#,
        r#"{"timestamp":"2026-06-29T12:00:03.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"T1"}}"#,
    ]
    .join("\n")
        + "\n";
    let p_ok = write_rollout_in_tree(&codex_home, uuid_ok, &body_ok);

    // (B) An ADVERSARIAL fabricated rollout (VALID filename so the scan picks it up,
    // hostile CONTENT): oversized line + recursion-bomb line + foreign frame kinds +
    // an UNBALANCED open turn + torn tail + dup keys + adversarial timestamp. UTF-8
    // throughout so read_to_string succeeds and the PER-LINE parser runs on each.
    let uuid_bad = "019e9f3b-deea-7392-9861-b5d8ad376e2b";
    let big = "x".repeat(2 * 1024 * 1024);
    let bomb = format!("{}1{}", "[".repeat(20_000), "]".repeat(20_000));
    let body_bad = format!(
        "{meta}\n{oversize}\n{recursion}\n{foreign}\n{open}\n{dupts}\n{torn}",
        meta = r#"{"timestamp":"2026-06-29T12:00:00Z","type":"session_meta","payload":{"id":"019e9f3b-deea-7392-9861-b5d8ad376e2b","cwd":"/jail/adversarial"}}"#,
        oversize = format!(r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":"{big}"}}}}"#),
        recursion = format!(r#"{{"type":"event_msg","payload":{bomb}}}"#),
        foreign = r#"{"type":"totally_unknown_kind","payload":{"type":"also_unknown"}}"#,
        open = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"OPEN-NEVER-DONE"}}"#,
        dupts = r#"{"timestamp":42,"timestamp":"2026-06-29T12:00:09Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"OPEN-NEVER-DONE"}}"#,
        torn = r#"{"type":"event_msg","payl"#, // truncated final line, no newline
    );
    let p_bad = write_rollout_in_tree(&codex_home, uuid_bad, &body_bad);

    // Drive the REAL verb surface. `--all` surfaces cold codex rows (the gather runs
    // scan_transcripts + read_stats over BOTH fabricated rollouts). The contract: the
    // process must NOT panic (exit 101) — degrade-not-panic AT THE BINARY LAYER.
    let ls = run_qd(&jail, &["ls", "--all", "--json"]);
    let ls_err = String::from_utf8_lossy(&ls.stderr).into_owned();
    let ls_out = String::from_utf8_lossy(&ls.stdout).into_owned();
    assert!(
        !ls_err.contains("panicked"),
        "qd ls must not panic on fabricated/adversarial rollouts; stderr:\n{ls_err}"
    );
    assert!(
        ls.status.success(),
        "qd ls --all degrades-not-crashes (exit {:?}); stderr:\n{ls_err}",
        ls.status.code()
    );

    // `qd info <id>` over the cold thread id — assert NO-PANIC (cold-row addressing
    // may legitimately not resolve; the standard under test is degrade-not-crash).
    let info = run_qd(&jail, &["info", uuid_ok]);
    let info_err = String::from_utf8_lossy(&info.stderr).into_owned();
    assert!(
        !info_err.contains("panicked"),
        "qd info must not panic on a fabricated rollout id; stderr:\n{info_err}"
    );

    // AT-SOURCE cross-check: the production parser (the very fns the gather calls)
    // sees the fabricated facts and degrades on the adversarial file without panic.
    let stats_ok = read_stats(&p_ok, true);
    assert_eq!(stats_ok.cwd.as_deref(), Some("/jail/work"), "well-formed: cwd from session_meta");
    assert_eq!(stats_ok.turns, 1, "well-formed: one completed turn");
    assert_eq!(
        stats_ok.last_turns.as_ref().and_then(|v| v.first()).map(|t| t.text.as_str()),
        Some("COLD-FABRICATED-PREVIEW"),
        "well-formed: preview = last agent_message"
    );
    // The adversarial file: read_stats must DEGRADE (cwd enriched from the valid
    // session_meta line; the oversized/bomb/foreign/torn lines skipped) — no panic.
    let stats_bad = read_stats(&p_bad, false);
    assert_eq!(
        stats_bad.cwd.as_deref(),
        Some("/jail/adversarial"),
        "adversarial: cwd still enriched from the valid session_meta; hostile lines skipped"
    );
    // The OPEN turn (task_started OPEN-NEVER-DONE) is balanced by its task_complete
    // on the dup-ts line → Idle (anchors present, balanced). No panic deriving it.
    assert_eq!(
        derive_status(&read_lines(&p_bad)),
        Some(SessionStatus::Idle),
        "adversarial: the open turn is completed on the dup-ts line → Idle"
    );

    ev_text(
        &bundle,
        "round2-cold-discovery-end-to-end.txt",
        &format!(
            "qd ls --all --json: exit={:?} panicked={} (degrade-not-crash at the binary layer)\n\
             qd info <ok-id>: panicked={}\n\
             AT SOURCE: well-formed rollout -> cwd=/jail/work turns=1 preview=COLD-FABRICATED-PREVIEW\n\
             AT SOURCE: adversarial rollout (2MiB line + 20k-deep bomb + foreign kind + torn tail \
             + dup keys + adversarial ts) -> cwd=/jail/adversarial, derive_status=Idle, NO PANIC\n\
             ls stdout bytes={}\n",
            ls.status.code(),
            ls_err.contains("panicked"),
            info_err.contains("panicked"),
            ls_out.len(),
        ),
    );
    ev_text(&bundle, "round2-cold-ls-stdout.json", &ls_out);

    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// ROUND 3 — the remaining not-yet-covered angles:
//   - LIVE-row derive_status driven END-TO-END through the REAL `qd ls` binary
//     via a fabricated codex registry row (the live-codex gather branch that
//     cold-discovery never reaches — it skips derive_status);
//   - parse_occupancy camelCase branch (lastTokenUsage / totalTokens — UNCOVERED
//     by rounds 1-2) + session_meta cwd / preview edge cases;
//   - parse_filename boundary v2 (uppercase hex, opaque-ts tolerance, empty-ts
//     via double-dash, non-hex deep group, trailing extra group);
//   - rpc fresh corruption: turn_id() complementary branches (nested-wins,
//     whitespace-kept) + ServerError boundary codes + a recursion-bomb in the
//     IGNORED `data` field (even a skipped field is recursion-limited).
// ===========================================================================

/// Kill+reap a spawned child on drop (panic-safe) — used to keep the fabricated
/// live registry row's process alive only for the qd-ls window.
struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn red_live_registry_row_drives_status_end_to_end_via_qd_ls() {
    // END-TO-END through the REAL `qd ls --json` binary, exercising the LIVE-codex
    // gather branch (join.rs: for each live codex registry row → transcript_path
    // tier-2 → derive_status + read_stats). Cold-discovery (round 2 #6) never runs
    // derive_status; this fabricates a LIVE registry row (a real sleeping pid) so
    // the live branch runs. Connectionless: NO socket opened (rollout-tail status).
    // Standard under test: the binary degrades-not-panics; cross-checked AT SOURCE.
    let jail = make_jail("red-liverow");
    let codex_home = jail.join("codex-home");
    let bundle = red_bundle();
    let sdir = sessions_dir(&jail);
    std::fs::create_dir_all(&sdir).unwrap();

    // A BUSY rollout (open task_started, no matching complete) at transcript_path.
    let uuid_busy = "019ea0b3-04d3-7400-8d95-f55d41e961e4";
    let busy_body = [
        r#"{"timestamp":"2026-06-29T12:00:00Z","type":"session_meta","payload":{"id":"019ea0b3-04d3-7400-8d95-f55d41e961e4","cwd":"/jail/work"}}"#,
        r#"{"timestamp":"2026-06-29T12:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"OPEN-NO-COMPLETE"}}"#,
    ]
    .join("\n")
        + "\n";
    let p_busy = write_rollout_in_tree(&codex_home, uuid_busy, &busy_body);

    // A real LIVE process so the fabricated row is treated as live by the scan.
    let child = std::process::Command::new("sleep")
        .arg("120")
        .spawn()
        .expect("spawn sleep");
    let live_pid = child.id() as i64;
    let _guard = KillOnDrop(child); // kill+reap on drop (panic-safe).

    // Fabricate the codex registry row pointing at the busy thread uuid.
    let entry = dispatch::registry::RegistryEntry {
        pid: Some(live_pid),
        session_id: Some(uuid_busy.into()),
        provider: Some("codex".into()),
        cwd: Some("/jail/work".into()),
        name: Some("red-live-row".into()),
        ..Default::default()
    };
    dispatch::registry::write_entry(&sdir, &entry).expect("write registry row");

    // The gather reads the row the same way codex_rows does — confirm at source.
    let rows = codex_rows(&jail);
    assert_eq!(rows.len(), 1, "exactly one fabricated codex row");
    assert_eq!(rows[0].session_id.as_deref(), Some(uuid_busy));

    // Drive the REAL `qd ls --json`. The live-row gather resolves the rollout and
    // derives status — NO panic / NO exit-101 is the contract.
    let ls = run_qd(&jail, &["ls", "--json"]);
    let ls_err = String::from_utf8_lossy(&ls.stderr).into_owned();
    let ls_out = String::from_utf8_lossy(&ls.stdout).into_owned();
    assert!(
        !ls_err.contains("panicked"),
        "qd ls must not panic on a fabricated live codex row; stderr:\n{ls_err}"
    );
    assert!(
        ls.status.success(),
        "qd ls --json degrades-not-crashes (exit {:?}); stderr:\n{ls_err}",
        ls.status.code()
    );

    // AT SOURCE: derive_status over the SAME rollout the live-row branch resolves
    // == Busy (the source-of-truth the binary computes connectionlessly).
    assert_eq!(
        derive_status(&read_lines(&p_busy)),
        Some(SessionStatus::Busy),
        "the open-turn rollout → Busy at source (what the live-row gather derives)"
    );
    // SOFT signal (logged, not asserted — render filtering is not the surface under
    // test): whether the binary surfaced the thread id end-to-end in its JSON.
    let surfaced = ls_out.contains(uuid_busy);

    ev_text(
        &bundle,
        "round3-live-row-end-to-end.txt",
        &format!(
            "qd ls --json: exit={:?} panicked={} (degrade-not-crash; live-row gather branch)\n\
             AT SOURCE: derive_status(open-turn rollout)=Busy (connectionless, no socket)\n\
             registry row: pid={live_pid} provider=codex session_id={uuid_busy}\n\
             SOFT: ls --json surfaced the thread id = {surfaced}\n",
            ls.status.code(),
            ls_err.contains("panicked"),
        ),
    );
    ev_text(&bundle, "round3-live-row-ls-stdout.json", &ls_out);

    drop(_guard); // reap the sleep before reclaiming the jail.
    let _ = std::fs::remove_dir_all(&jail);
}

#[test]
fn red_parser_occupancy_camelcase_and_session_meta_edges() {
    // parse_occupancy accepts BOTH the rollout snake_case (last_token_usage /
    // total_tokens) AND the app-server camelCase (lastTokenUsage / totalTokens) —
    // the camelCase branch was UNCOVERED by rounds 1-2. Plus session_meta cwd /
    // preview edges read_stats guards.
    let mut report = String::from("# occupancy camelCase + session_meta / preview edges\n");

    // camelCase BOTH levels → occupancy parsed (the .or_else fallbacks fire).
    let cc = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"lastTokenUsage":{"totalTokens":12345}}}}"#;
    assert_eq!(
        parse_line(cc).unwrap().line,
        RolloutLine::TokenCount { occupancy: Some(12345) },
        "camelCase lastTokenUsage/totalTokens → occupancy (uncovered branch)"
    );

    // snake_case OUTER + camelCase INNER totalTokens → inner or_else fires.
    let mix_inner = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"totalTokens":7}}}}"#;
    assert_eq!(
        parse_line(mix_inner).unwrap().line,
        RolloutLine::TokenCount { occupancy: Some(7) },
        "snake outer + camel inner totalTokens → 7"
    );

    // BOTH snake + camel present at the outer level → snake WINS (get() before or_else).
    let both = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":1},"lastTokenUsage":{"totalTokens":2}}}}"#;
    assert_eq!(
        parse_line(both).unwrap().line,
        RolloutLine::TokenCount { occupancy: Some(1) },
        "snake_case precedence: last_token_usage before lastTokenUsage → 1"
    );
    report.push_str("camelCase=12345; snake+camelInner=7; snake-precedence=1\n");

    // session_meta: empty cwd ignored; multiple metas → FIRST non-empty cwd wins;
    // a WHITESPACE cwd is non-empty so it is KEPT (characterized — guard is
    // !is_empty(), not !trim().is_empty()).
    let meta_empty = r#"{"type":"session_meta","payload":{"id":"X","cwd":""}}"#;
    let meta_first = r#"{"type":"session_meta","payload":{"id":"Y","cwd":"/first"}}"#;
    let meta_second = r#"{"type":"session_meta","payload":{"id":"Z","cwd":"/second"}}"#;
    let tc = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"T"}}"#;
    let multi = format!("{meta_empty}\n{meta_first}\n{meta_second}\n{tc}\n");
    let p = write_bytes("meta-multi.jsonl", multi.as_bytes());
    let stats = read_stats(&p, false);
    assert_eq!(stats.cwd.as_deref(), Some("/first"), "first non-empty cwd wins; empty skipped");

    let ws = format!(
        "{}\n{tc}\n",
        r#"{"type":"session_meta","payload":{"id":"W","cwd":"   "}}"#
    );
    let pw = write_bytes("meta-ws.jsonl", ws.as_bytes());
    assert_eq!(
        read_stats(&pw, false).cwd.as_deref(),
        Some("   "),
        "whitespace cwd is non-empty → KEPT (characterized; not a break)"
    );

    // Preview edges: include_preview=true with ZERO agent_messages → last_turns is
    // Some(EMPTY vec) (split_off(0) on empty), NOT None. An EMPTY-string message is
    // guarded out (never previewed).
    let only_tc = format!("{tc}\n");
    let p_notext = write_bytes("preview-empty.jsonl", only_tc.as_bytes());
    let s_notext = read_stats(&p_notext, true);
    let lt = s_notext
        .last_turns
        .expect("include_preview + no agent_message → Some(empty), not None (characterized)");
    assert!(lt.is_empty(), "preview list is empty when there is no agent_message");
    let empty_msg = r#"{"type":"event_msg","payload":{"type":"agent_message","message":""}}"#;
    let p_emptymsg = write_bytes("preview-emptymsg.jsonl", format!("{empty_msg}\n{tc}\n").as_bytes());
    let lt2 = read_stats(&p_emptymsg, true)
        .last_turns
        .expect("include_preview → Some even with only an empty-string message");
    assert!(lt2.is_empty(), "empty-string agent_message is guarded out of the preview");
    report.push_str("multi-meta first-wins=/first; ws-cwd kept; no-text preview=Some([]); empty-msg skipped\n");

    ev_text(&red_bundle(), "round3-occupancy-meta-edges.txt", &report);
}

#[test]
fn red_parse_filename_boundary_adversarial_v2() {
    // Fresh parse_filename boundaries beyond round 1.
    let mut report = String::from("# parse_filename boundary v2\n");

    // (1) UPPERCASE hex uuid groups → is_ascii_hexdigit accepts A-F → PARSES
    //     (characterized: casing of the uuid is tolerated).
    let upper = "rollout-2026-06-07T02-09-07-019EA0B3-04D3-7400-8D95-F55D41E961E4.jsonl";
    let got = parse_filename(upper).expect("uppercase hex uuid parses");
    assert_eq!(got.id, "019EA0B3-04D3-7400-8D95-F55D41E961E4");
    report.push_str(&format!("uppercase-hex -> id={} (parses)\n", got.id));

    // (2) An EMOJI in the TIMESTAMP portion (NOT the uuid) → still parses; the ts is
    //     opaque text (characterized: only the uuid tail is validated).
    let emoji_ts = "rollout-2026-\u{1F600}-019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl";
    let got2 = parse_filename(emoji_ts).expect("emoji in ts portion still parses");
    assert_eq!(got2.id, "019ea0b3-04d3-7400-8d95-f55d41e961e4");
    assert!(got2.timestamp.contains('\u{1F600}'), "ts portion is opaque, emoji kept");
    report.push_str("emoji-in-ts -> parses (ts opaque)\n");

    // (3) A long arbitrary ts (many dash groups) + valid uuid tail → parses.
    let long_ts = "rollout-a-b-c-d-e-f-g-019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl";
    assert!(parse_filename(long_ts).is_some(), "arbitrary long ts + valid uuid tail parses");

    // Degrade cases (→ None, no panic):
    let bad = [
        // double-dash right after the prefix → empty timestamp → None.
        "rollout--019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl",
        // a non-hex char in the FIRST uuid group (correct length 8) → None.
        "rollout-2026-06-07T02-09-07-gggggggg-04d3-7400-8d95-f55d41e961e4.jsonl",
        // a trailing EXTRA group shifts the 5-group window → wrong lengths → None.
        "rollout-2026-06-07T02-09-07-019ea0b3-04d3-7400-8d95-f55d41e961e4-extra.jsonl",
        // last group one char short (11, not 12) → None.
        "rollout-2026-06-07T02-09-07-019ea0b3-04d3-7400-8d95-f55d41e961e.jsonl",
    ];
    for b in bad {
        assert!(parse_filename(b).is_none(), "must degrade to None: {b:?}");
        report.push_str(&format!("{b:?} -> None\n"));
    }
    ev_text(&red_bundle(), "round3-parse-filename-v2.txt", &report);
}

#[test]
fn red_rpc_turn_result_and_servererror_fresh_corruption() {
    // Fresh rpc corruption beyond round 1's matrix.
    let mut report = String::from("# rpc fresh corruption: turn_id branches + ServerError boundaries\n");

    // turn_id(): complementary branches to round 1's "flat wins".
    let nested_wins = r#"{"turnId":"","turn":{"id":"NESTED-WINS"}}"#; // flat empty → nested.
    let p1: TurnResult = serde_json::from_str(nested_wins).unwrap();
    assert_eq!(p1.turn_id(), Some("NESTED-WINS"), "flat empty → nested id");

    // Flat WHITESPACE is non-empty → flat wins even though it is whitespace
    // (characterized: the filter is !is_empty(), not !trim().is_empty()).
    let flat_ws = r#"{"turnId":" ","turn":{"id":"N"}}"#;
    let p2: TurnResult = serde_json::from_str(flat_ws).unwrap();
    assert_eq!(p2.turn_id(), Some(" "), "whitespace flat id is non-empty → kept (characterized)");

    // Nested whitespace-only id (no flat) → kept (characterized).
    let nested_ws = r#"{"turn":{"id":"  "}}"#;
    let p3: TurnResult = serde_json::from_str(nested_ws).unwrap();
    assert_eq!(p3.turn_id(), Some("  "), "whitespace nested id kept (characterized)");

    // Both empty → None.
    let both_empty = r#"{"turnId":"","turn":{"id":""}}"#;
    let p4: TurnResult = serde_json::from_str(both_empty).unwrap();
    assert_eq!(p4.turn_id(), None, "both shapes empty → None");
    report.push_str("nested-wins / ws-flat-kept / ws-nested-kept / both-empty-None\n");

    // ServerError boundary codes parse (i64 extremes + 0).
    for c in [
        r#"{"code":9223372036854775807,"message":"i64::MAX"}"#,
        r#"{"code":0,"message":"zero code"}"#,
        r#"{"code":-32602,"message":"input too large"}"#,
        r#"{"code":-32600,"message":"line1\nline2 with unicode ☃ é"}"#, // multiline+unicode message ok.
        r#"{"code":-32600,"message":"x","data":[1,2,3]}"#,              // ignored data: array.
        r#"{"code":-32600,"message":"x","data":null}"#,                 // ignored data: null.
        r#"{"code":-32600,"message":"x","data":"a string"}"#,           // ignored data: string.
    ] {
        let got: ServerError = serde_json::from_str(c).expect("well-formed ServerError parses (data ignored)");
        report.push_str(&format!("{c} -> code={}\n", got.code));
    }
    // i64::MAX sanity.
    let max: ServerError = serde_json::from_str(r#"{"code":9223372036854775807,"message":"m"}"#).unwrap();
    assert_eq!(max.code, i64::MAX);

    // A RECURSION-BOMB in the IGNORED `data` field. EMPIRICAL RESULT (corrected):
    // serde_json consumes the unconsumed value ITERATIVELY (an explicit skip-stack,
    // NOT typed recursion), so — unlike the typed `Value` path (round-1 deep-nesting
    // → Err at the 128 recursion limit) — it parses OK: code/message intact, the
    // bomb in `data` is skipped, with NO stack overflow and NO panic. The
    // degrade-not-panic standard HOLDS (a bomb in an unconsumed field is SAFELY
    // skipped, not a crash). CHARACTERIZED ASYMMETRY: typed-Value is recursion-
    // LIMITED (errors); the ignored-field skip is depth-tolerant-and-safe (Ok). The
    // 20k array did not overflow an 8 MiB stack → the skip is provably non-recursive
    // here. (My round-3 draft asserted Err on the false premise that the ignored
    // field is recursion-limited like Value; it is not — fixed, not a prod change.)
    let bomb = format!("{}1{}", "[".repeat(20_000), "]".repeat(20_000));
    let recursion_data = format!(r#"{{"code":-32600,"message":"x","data":{bomb}}}"#);
    let parsed: ServerError = serde_json::from_str(&recursion_data)
        .expect("a bomb in the IGNORED data field is safely skipped (parses Ok), no overflow");
    assert_eq!(parsed.code, -32600, "code intact despite the recursion bomb in data");
    assert_eq!(parsed.message, "x", "message intact despite the recursion bomb in data");
    report.push_str(
        "recursion-bomb in ignored `data` -> Ok, code/message intact, data skipped \
         (iterative skip; degrade-SAFE, no overflow). Asymmetry: typed-Value is \
         recursion-limited (Err), ignored-field skip is depth-tolerant-safe (Ok).\n",
    );

    ev_text(&red_bundle(), "round3-rpc-fresh-corruption.txt", &report);
}
