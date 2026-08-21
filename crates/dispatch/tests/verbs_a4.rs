//! A4 verb-level integration tests (send:pty / send:http / wait) driving the
//! REAL `qd` binary against a JAILED, empty HOME (L9a / ADD-4 — never the real
//! home; HOME + ZMX_DIR point into a per-test tempdir with no sessions).
//!
//! These cover each new verb's resolve-failure path end-to-end through the bin:
//! an empty registry → `resolveOrDie` → `No session matching "<q>"` + exit 1.
//! The rich success/queue/idle/wait-loop logic is unit-tested in `dispatch::sendpty`
//! and `dispatch::wait` (instant seamed deps); the golden scenarios (M4) exercise the
//! live PTY + exit-contract paths. This file pins the bin wiring + exit codes.

mod common;

use std::path::Path;
use std::process::Command;

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// Run `qd <args...>` with HOME + ZMX_DIR jailed into `home`/`zmx` under `dir`.
/// Returns (exit_code, stdout, stderr).
fn run_qd(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    common::assert_not_real_home(&home);

    let out = Command::new(qd_bin())
        .args(args)
        // L9a: jailed HOME; ZMX_DIR pinned to an empty dir so zmx finds nothing.
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        // Keep zmx from being on PATH-relevant for these resolve-only paths; the
        // empty registry + empty zmx dir already yield zero sessions.
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// --- P0 W1 (qb spec-cli §11): new/kill are RETIRED erroring stubs ---------
// They error helpfully on ANY args (accept-and-ignore, never a clap usage
// error), exit 1, and never touch state. start/stop are the live verbs.

#[test]
fn new_retired_stub_errors_helpfully_exits_1() {
    let temp = tempfile::tempdir().unwrap();
    for args in [
        vec!["new"],
        vec!["new", "wk"],
        vec!["new", "wk", "-p", "hello", "--model", "opus"],
    ] {
        let (code, _out, err) = run_qd(temp.path(), &args);
        assert_eq!(
            code, 1,
            "retired `new` → exit 1 for {args:?} (stderr: {err})"
        );
        assert!(
            err.contains("qd new: `new` is retired; use `qd start`"),
            "retired-stub stderr for {args:?}, got: {err}"
        );
    }
}

#[test]
fn kill_retired_stub_errors_helpfully_exits_1() {
    let temp = tempfile::tempdir().unwrap();
    for args in [
        vec!["kill"],
        vec!["kill", "wk"],
        vec!["kill", "--force", "wk"],
    ] {
        let (code, _out, err) = run_qd(temp.path(), &args);
        assert_eq!(
            code, 1,
            "retired `kill` → exit 1 for {args:?} (stderr: {err})"
        );
        assert!(
            err.contains("qd kill: `kill` is retired; use `qd stop`"),
            "retired-stub stderr for {args:?}, got: {err}"
        );
    }
}

#[test]
fn stop_unknown_session_exits_1() {
    // The LIVE stop verb reaches the real backend (resolveOrDie path).
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(temp.path(), &["stop", "nosuch"]);
    assert_eq!(code, 1, "unknown session → exit 1 (stderr: {err})");
    assert!(
        err.contains("No session matching \"nosuch\""),
        "stderr should be resolveOrDie's message, got: {err}"
    );
}

#[test]
fn send_pty_unknown_session_exits_1() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(temp.path(), &["send:pty", "nosuch", "hello"]);
    assert_eq!(code, 1, "unknown session → exit 1 (stderr: {err})");
    assert!(
        err.contains("No session matching \"nosuch\""),
        "stderr should be resolveOrDie's message, got: {err}"
    );
}

#[test]
fn send_http_unknown_session_exits_1() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(temp.path(), &["send:http", "nosuch", "hello"]);
    assert_eq!(code, 1, "unknown session → exit 1 (stderr: {err})");
    assert!(
        err.contains("No session matching \"nosuch\""),
        "stderr should be resolveOrDie's message, got: {err}"
    );
}

#[test]
fn wait_unknown_session_exits_1() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(temp.path(), &["wait", "nosuch"]);
    assert_eq!(code, 1, "unknown session → exit 1 (stderr: {err})");
    assert!(
        err.contains("No session matching \"nosuch\""),
        "stderr should be resolveOrDie's message, got: {err}"
    );
}

#[test]
fn send_http_engine_session_is_never_opencode_error_block() {
    // A seeded LIVE claude session: send:http must take the "not an OpenCode
    // session" ERROR block (engine sessions are never opencode) + exit 1, and
    // point the user at send:relay / send:pty (the guidance bullets).
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(temp.path().join("zmx")).unwrap();
    common::assert_not_real_home(&home);
    // Forge a registry row for a live idle claude session named "wk".
    std::fs::write(
        sessions.join("90001.json"),
        r#"{"pid":90001,"sessionId":"sid-wk-0000","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"wk","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}"#,
    )
    .unwrap();

    let out = Command::new(qd_bin())
        .args(["send:http", "wk", "hello"])
        .env("HOME", &home)
        .env("ZMX_DIR", temp.path().join("zmx"))
        .output()
        .expect("spawn qd");
    let code = out.status.code().unwrap_or(-1);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        code, 1,
        "send:http on a claude session → exit 1 (stderr: {err})"
    );
    assert!(
        err.contains("is not an OpenCode session"),
        "expected the not-an-OpenCode ERROR block, got: {err}"
    );
    assert!(
        err.contains("qd send:relay wk") && err.contains("qd send:pty wk"),
        "expected the send:relay / send:pty guidance bullets, got: {err}"
    );
}

#[test]
fn wait_idle_session_reports_idle_exit_0() {
    // A seeded LIVE IDLE claude session: `qd wait` entry idle check →
    // `<label> is idle` on stdout, exit 0 (no polling).
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(temp.path().join("zmx")).unwrap();
    common::assert_not_real_home(&home);
    std::fs::write(
        sessions.join("90002.json"),
        r#"{"pid":90002,"sessionId":"sid-idle-000","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"idlewk","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}"#,
    )
    .unwrap();

    let out = Command::new(qd_bin())
        .args(["wait", "idlewk"])
        .env("HOME", &home)
        .env("ZMX_DIR", temp.path().join("zmx"))
        .output()
        .expect("spawn qd");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(code, 0, "idle entry → exit 0 (stdout: {stdout})");
    assert!(
        stdout.contains("idlewk is idle"),
        "expected '<label> is idle', got: {stdout}"
    );
}

// ===========================================================================
// VERB ATTRIBUTION (2026-08) — every user-facing refusal on these paths names
// the command that produced it. These pin the BYTES the fix moved, driving the
// real `qd` (which spawns the real `qw`) against a jailed HOME.
// ===========================================================================

/// A live-but-pid-less row is `LaneError::Cold`, and the verb's rendering of it
/// used to be the ONE refusal on `qd wait` that named no command at all (its two
/// siblings — the `Transport` arm and the generic arm — have always opened
/// `qd wait: `). Pins the prefixed line.
#[test]
fn wait_cold_row_refusal_is_qd_wait_attributed() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("home").join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // pid 0 = "no pid recorded" (the resolver keeps such a row on its status
    // alone), status busy so the entry-idle gate does not short-circuit.
    std::fs::write(
        sessions.join("90101.json"),
        r#"{"pid":0,"sessionId":"sid-cold-0001","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"busy","name":"coldwk","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}"#,
    )
    .unwrap();

    let (code, _out, err) = run_qd(temp.path(), &["wait", "coldwk"]);
    assert_eq!(code, 1, "cold row → exit 1 (stderr: {err})");
    assert!(
        err.contains("qd wait: Session has no PID (cold/dead). Nothing to wait for."),
        "the cold refusal must name its verb, got: {err}"
    );
}

/// `acp_loss::preserve_identity`'s observability line is written by the `qw`
/// child and is SHARED by two seams, so it carries the CALLER's verb — the one
/// the user typed — and matches the refusal it immediately precedes. It used to
/// open with a bare `qd:` on both. Drives both seams against the same row.
#[test]
fn acp_identity_preserved_line_names_the_verb_on_both_seams() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("home").join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("90102.json"),
        r#"{"pid":0,"sessionId":"019ea0b3-04d3-7400-8d95-acpcase2cell","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"busy","name":"acpwk","version":"0.1.0","kind":"claude-code","entrypoint":"claude","provider":"acp/claude-code"}"#,
    )
    .unwrap();

    // The WAIT seam (`quorum_qw::idle::await_idle_acp`).
    let (code, _out, err) = run_qd(temp.path(), &["wait", "acpwk"]);
    assert_eq!(code, 1, "acp transport loss → exit 1 (stderr: {err})");
    assert!(
        err.contains("qd wait: acp: session \"acpwk\" identity preserved at "),
        "the wait seam's identity line must open `qd wait:`, got: {err}"
    );
    assert!(
        !err.contains("qd: acp:"),
        "the pre-fix bare `qd:` prefix must be gone, got: {err}"
    );

    // The SEND seam (`quorum_qw::delivery::acp::send_acp`), same body, same row,
    // the other command — and the same verb its refusal line carries.
    let (code, _out, err) = run_qd(temp.path(), &["send:relay", "acpwk", "hello"]);
    assert_eq!(code, 1, "acp transport loss → exit 1 (stderr: {err})");
    assert!(
        err.contains("qd send:relay: acp: session \"acpwk\" identity preserved at "),
        "the send seam's identity line must open `qd send:relay:`, got: {err}"
    );
    assert!(
        !err.contains("qd: acp:"),
        "the pre-fix bare `qd:` prefix must be gone, got: {err}"
    );
}

// ===========================================================================
// qd–qf W3: unified `qd send` origin-mode surface (write-then-deliver +
// --expires + the Refusal {class,reason} type). These drive the REAL binary
// through cheap, hermetic paths (a malformed --expires SYNC refusal; a valid
// --expires that still resolves normally) — the success write-then-deliver +
// disposition wiring is proven at the unit level (send_unified.rs
// `deliver_with_durability` seam tests) since a full live carrier is heavy.
// ===========================================================================

/// A malformed `--expires` is a SYNC refusal rendered through the shared Refusal
/// type: `qd send: refused{expires}: …` on stderr + the distinct exit code 12.
/// It refuses BEFORE any resolution, so an unknown session is irrelevant.
#[test]
fn send_bad_expires_is_a_sync_refusal_exit_12() {
    // NOTE: leading-`-` values (e.g. "-5m") are caught by clap as an unknown
    // option BEFORE our parser sees them (a clap parse error, exit 1) — not a
    // refused{expires}. `parse_expires`'s own unit tests cover the "-5m" reject at
    // the function level; here we assert the forms that actually reach our parser.
    let temp = tempfile::tempdir().unwrap();
    for bad in ["12x", "1.5h", "h", "abc", "12h30m"] {
        let (code, _out, err) = run_qd(temp.path(), &["send", "--expires", bad, "wk", "hello"]);
        assert_eq!(code, 12, "malformed --expires {bad:?} → exit 12 (stderr: {err})");
        assert!(
            err.contains("refused{expires}"),
            "expected the refused{{expires}} render for {bad:?}, got: {err}"
        );
        assert!(
            err.starts_with("qd send: refused{expires}:"),
            "machine-stable prefix for {bad:?}, got: {err}"
        );
    }
}

/// A well-formed `--expires` parses cleanly and does NOT disturb resolution: an
/// unknown session still reaches the resolver. qd–qf W6: origin `send` now renders
/// the resolver's outcomes through the SHARED Refusal (refused{unknown} exit 12),
/// consistent with the W4 inbound door — NOT the old resolve_or_die exit 1 (that
/// path is unchanged for the OTHER verbs: stop / send:pty / send:http / wait).
/// This still proves the flag is accepted and the value is consumed (it is NOT a
/// refused{expires}).
#[test]
fn send_good_expires_parses_then_resolves_normally() {
    let temp = tempfile::tempdir().unwrap();
    for good in ["12h", "30m", "45s", "1d", "90"] {
        let (code, _out, err) = run_qd(temp.path(), &["send", "--expires", good, "nope", "hi"]);
        assert_eq!(code, 12, "valid --expires {good:?} + unknown session → W6 refused{{unknown}} exit 12 (stderr: {err})");
        assert!(
            err.contains("refused{unknown}") && err.contains("no session matching \"nope\""),
            "valid --expires {good:?} must reach the resolver (refused{{unknown}}), got: {err}"
        );
        assert!(
            !err.contains("refused{expires}"),
            "a valid --expires {good:?} must NOT be refused as a bad expiry, got: {err}"
        );
    }
}

/// The unified `qd send` default (no `--expires`) also resolves on an empty
/// registry — the flag being absent is the 12h default, never an error. qd–qf W6:
/// an unknown target is refused{unknown} exit 12 (the aligned origin surface).
#[test]
fn send_default_expires_resolves_normally() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(temp.path(), &["send", "ghost", "body"]);
    assert_eq!(code, 12, "unknown session → W6 refused{{unknown}} exit 12 (stderr: {err})");
    assert!(
        err.contains("refused{unknown}") && err.contains("no session matching \"ghost\""),
        "default-expires send reaches the resolver (refused{{unknown}}), got: {err}"
    );
}

// ===========================================================================
// qd–qf W3b: resume-and-deliver — a stopped/cold/killed target is ACCEPTED and
// WOKEN, not refused. "stopped is not a refusal class." These drive the REAL
// binary against a forged NOT-live registry row. To stay hermetic + fast they
// use an UNKNOWN-provider row, which hits the wake path's "cannot be woken
// headlessly" arm IMMEDIATELY (no ~40-60s live revive) — enough to prove (a) the
// old cold/stopped/killed REFUSALS are gone (the path proceeds to a WAKE, not a
// "resume it first" refusal) and (b) the failed{wake} contract under the R8
// event model: exit 12, failed{wake} stderr, envelope (with `origin`) logged
// FIRST + the not-live funnel EVENT rows `attempted, queued,
// delivery-failed{wake}` in file order (never one terminal state row), folding
// to a summary `state=failed, last_event=delivery-failed` — NOT absorbing: a
// later retry may still deliver.
// The claude cold wake reaching the real revive machinery is the #[ignore]d
// live test at the bottom (mirrors attach's cold_claude_attach test).
// ===========================================================================

/// Forge one registry row `<pid>[.tombstoned].json` under a freshly-jailed HOME
/// (QD_HOME UNSET so the transport files land in the jail) and run `qd send …`.
/// Returns (exit, stdout, stderr, log.jsonl body, dispositions.jsonl body).
fn run_send_with_row(
    dir: &Path,
    pid: i64,
    tombstoned: bool,
    row_json: &str,
    args: &[&str],
) -> (i32, String, String, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    common::assert_not_real_home(&home);
    let fname = if tombstoned {
        format!("{pid}.json.tombstoned")
    } else {
        format!("{pid}.json")
    };
    std::fs::write(sessions.join(fname), row_json).unwrap();

    let out = Command::new(qd_bin())
        .args(args)
        .env_remove("QD_HOME") // transport files land under <home>/.quorum/dispatch
        .env_remove("QD_HOST") // the envelope's origin stamps as the "local" v1 placeholder
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        .output()
        .expect("spawn qd");
    let root = home.join(".quorum").join("dispatch");
    let log = std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default();
    let disps = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        log,
        disps,
    )
}

/// Parse a dispositions.jsonl body into its raw EVENT rows (R8/R14: one typed
/// normalized event per line — never state records).
fn parse_event_rows(disps: &str) -> Vec<serde_json::Value> {
    disps
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad event row {l:?}: {e}")))
        .collect()
}

/// Run `qd dispositions <args...>` against the SAME jailed HOME a
/// `run_send_with_row` call used (so the summary view folds the rows that send
/// just wrote). Returns (exit, stdout, stderr).
fn run_dispositions_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let mut full = vec!["dispositions"];
    full.extend_from_slice(args);
    let out = Command::new(qd_bin())
        .args(&full)
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .env("HOME", &home)
        .output()
        .expect("spawn qd dispositions");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A COLD target is no longer refused with "resume it first" / "dead, use resume":
/// the send proceeds to a WAKE. With an unwakeable (unknown-provider) row the wake
/// fails → the failed{wake} contract under R8: exit 12, `failed{wake}` stderr,
/// the envelope (carrying `origin`) logged BEFORE the wake, and the not-live
/// FUNNEL event rows `attempted, queued, delivery-failed{wake}` in file order —
/// folding to summary `state=failed, last_event=delivery-failed` (NOT absorbing).
#[test]
fn send_cold_target_wakes_and_is_not_refused_as_stopped() {
    let temp = tempfile::tempdir().unwrap();
    let row = r#"{"pid":90099,"sessionId":"mystery-cold-1","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"cold","name":"coldwk","version":"0.1.0","provider":"mystery"}"#;
    let (code, _out, err, log, disps) =
        run_send_with_row(temp.path(), 90099, false, row, &["send", "coldwk", "hi"]);

    // The OLD cold refusals are GONE.
    assert!(
        !err.contains("resume it first") && !err.contains("Use 'qd resume'") && !err.contains("is dead"),
        "a cold target must NOT be refused with a resume-it-first message, got: {err}"
    );
    // The NEW behavior: wake attempted → failed{wake} (exit 12). `accepted` is
    // retired (R14.3); origin-mode admission is marked by `attempted`.
    assert_eq!(code, 12, "unwakeable cold target → failed{{wake}} exit 12 (stderr: {err})");
    assert!(
        err.contains("failed{wake}"),
        "expected the failed{{wake}} render, got: {err}"
    );
    // Envelope logged FIRST (write-then-deliver), in the renamed wire shape
    // (`origin`, never `authority`).
    assert!(
        log.contains("coldwk") && log.contains("\"origin\":"),
        "the envelope (with origin) must be logged before the wake, got log.jsonl: {log:?}"
    );
    // The funnel EVENT rows, in file order: attempted, queued, then
    // delivery-failed carrying the REQUIRED machine `class` (the wake class).
    let rows = parse_event_rows(&disps);
    let kinds: Vec<&str> = rows.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["attempted", "queued", "delivery-failed"],
        "the origin not-live funnel in file order, got: {disps:?}"
    );
    assert_eq!(rows[2]["class"], "wake", "delivery-failed carries class wake (R14.2): {disps:?}");
    assert!(
        rows[0].get("class").is_none() && rows[1].get("class").is_none(),
        "class FORBIDDEN on the plain attempted/queued variants: {disps:?}"
    );

    // The summary VIEW folds the funnel to failed (latest event delivery-failed,
    // pre-expiry, no delivered event) — with last_event surfacing the detail.
    let (dcode, dstdout, dstderr) = run_dispositions_in(temp.path(), &[]);
    assert_eq!(dcode, 0, "qd dispositions exit 0 (stderr: {dstderr})");
    let summary: serde_json::Value = serde_json::from_str(dstdout.trim())
        .unwrap_or_else(|e| panic!("one summary row expected, got {dstdout:?} ({e})"));
    assert_eq!(summary["state"], "failed", "summary folds to failed: {dstdout}");
    assert_eq!(
        summary["last_event"], "delivery-failed",
        "last_event carries the fine grain: {dstdout}"
    );
    assert_eq!(summary["attempts"], 1, "one attempted event: {dstdout}");
}

/// A TOMBSTONED (killed) target is likewise no longer rejected by the send path's
/// tombstone gate — it is a WAKE trigger. Same failed{wake} contract on an
/// unwakeable row. This is the direct retirement proof for the send-path
/// `reject_if_tombstoned` call.
#[test]
fn send_tombstoned_target_wakes_and_is_not_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let row = r#"{"pid":90100,"sessionId":"mystery-tomb-2","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"tombwk","version":"0.1.0","provider":"mystery"}"#;
    let (code, _out, err, log, disps) =
        run_send_with_row(temp.path(), 90100, true, row, &["send", "tombwk", "hi"]);

    // The OLD tombstone refusal ("found … but it is stopped — resume it first")
    // is GONE for the send path.
    assert!(
        !err.contains("but it is stopped — resume it first"),
        "a tombstoned target must NOT hit the reject_if_tombstoned refusal, got: {err}"
    );
    assert_eq!(code, 12, "unwakeable tombstoned target → failed{{wake}} exit 12 (stderr: {err})");
    assert!(err.contains("failed{wake}"), "expected failed{{wake}}, got: {err}");
    assert!(
        log.contains("tombwk") && log.contains("\"origin\":"),
        "envelope (with origin) logged before the wake, got log.jsonl: {log:?}"
    );
    // Same R8 funnel as the cold arm: attempted, queued, delivery-failed{wake}.
    let rows = parse_event_rows(&disps);
    let kinds: Vec<&str> = rows.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["attempted", "queued", "delivery-failed"],
        "funnel event rows written, got dispositions.jsonl: {disps:?}"
    );
    assert_eq!(rows[2]["class"], "wake", "the wake-failure class (R14.2), got: {disps:?}");
}

// ===========================================================================
// qd–qf W3c: caller-supplied `--correlation-id` (the frame↔qd origin seam,
// provider-contract §4). Frame's ledger event id rides through the flag as the
// envelope's correlation_id; qd mints its own ULID only for BARE sends. Driven
// through the REAL binary to a deterministic outcome: an UNWAKEABLE
// (unknown-provider cold) row fails{wake} FAST, and because write-then-deliver
// logs the envelope FIRST + stamps the funnel events, BOTH the log.jsonl
// envelope AND every dispositions.jsonl EVENT row are observable and must
// carry the SUPPLIED id (not a minted ULID) — the whole funnel correlates on
// the one origin-minted id. The empty-id + inbound-conflict refusals are sync
// (no state) and are asserted via run_qd.
// ===========================================================================

/// `qd send --correlation-id FRAME-EVT-123 <target> <body>` — the supplied id
/// becomes the envelope's correlation_id AND rides on EVERY stamped event row
/// of the funnel (attempted, queued, delivery-failed), keyed on the same id
/// (never a minted ULID). Uses an unwakeable cold row so the path drives to a
/// deterministic failed{wake} outcome hermetically (no live fleet).
#[test]
fn send_correlation_id_rides_into_envelope_and_every_event_row() {
    let temp = tempfile::tempdir().unwrap();
    let row = r#"{"pid":90110,"sessionId":"mystery-cid-1","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"cold","name":"cidwk","version":"0.1.0","provider":"mystery"}"#;
    let (code, _out, err, log, disps) = run_send_with_row(
        temp.path(),
        90110,
        false,
        row,
        &["send", "--correlation-id", "FRAME-EVT-123", "cidwk", "hi"],
    );
    // Drives to the failed{wake} outcome (unwakeable), exit 12 — the point here
    // is the id, not the outcome; the funnel is stamped either way.
    assert_eq!(code, 12, "unwakeable target still stamps its funnel (stderr: {err})");
    // The SUPPLIED id is in the log envelope — NOT a ULID.
    assert!(
        log.contains("\"correlation_id\":\"FRAME-EVT-123\""),
        "the log envelope must carry the caller-supplied id, got log.jsonl: {log:?}"
    );
    // …AND on EVERY event row of the funnel (the frame↔qd seam: frame's ledger
    // event id correlates the whole funnel).
    let rows = parse_event_rows(&disps);
    let kinds: Vec<&str> = rows.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["attempted", "queued", "delivery-failed"],
        "the funnel rows, got dispositions.jsonl: {disps:?}"
    );
    for r in &rows {
        assert_eq!(
            r["correlation_id"], "FRAME-EVT-123",
            "every event row keys on the SAME supplied id, got: {disps:?}"
        );
    }
}

/// Absent `--correlation-id` ⇒ qd mints a 26-char ULID (unchanged bare-send
/// default). Same unwakeable row; assert the logged id is NOT the sentinel and is
/// ULID-shaped (26 Crockford chars).
#[test]
fn send_without_correlation_id_mints_a_ulid() {
    let temp = tempfile::tempdir().unwrap();
    let row = r#"{"pid":90111,"sessionId":"mystery-cid-2","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"cold","name":"mintwk","version":"0.1.0","provider":"mystery"}"#;
    let (code, _out, _err, log, _disps) =
        run_send_with_row(temp.path(), 90111, false, row, &["send", "mintwk", "hi"]);
    assert_eq!(code, 12);
    // Pull the correlation_id out of the envelope line and check it is a 26-char
    // ULID (Crockford base32), never the W3c sentinel.
    let cid = log
        .split("\"correlation_id\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap_or("");
    assert_eq!(cid.len(), 26, "a minted ULID is 26 chars, got {cid:?} from {log:?}");
    assert!(
        cid.bytes().all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b)),
        "minted id is Crockford base32, got {cid:?}"
    );
}

/// `--correlation-id ""` (empty) is a SYNC refusal before any resolution / side
/// effect: refused{correlation-id} exit 12, and nothing is logged. An empty id is
/// no id.
#[test]
fn send_empty_correlation_id_is_a_sync_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(temp.path(), &["send", "--correlation-id", "", "wk", "hi"]);
    assert_eq!(code, 12, "empty --correlation-id → sync refusal exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{correlation-id}:"),
        "expected the refused{{correlation-id}} render, got: {err}"
    );
}

/// `--correlation-id` + `--inbound-envelope` is a contradiction (an inbound
/// envelope carries its own origin-minted id) ⇒ a sync refused{args} exit 12 —
/// the same posture as `--expires` + inbound.
#[test]
fn send_correlation_id_with_inbound_envelope_is_refused_args() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(
        temp.path(),
        &["send", "--inbound-envelope", "/tmp/env.json", "--correlation-id", "X"],
    );
    assert_eq!(code, 12, "correlation-id + inbound → refused{{args}} exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{args}:"),
        "expected refused{{args}} for the mode conflict, got: {err}"
    );
    assert!(
        err.contains("correlation-id"),
        "the refusal names the offending flag, got: {err}"
    );
}

/// LIVE-target-unchanged (regression guard): a live IDLE claude row with no relay
/// and (in this empty-zmx jail) no joined mux pane still refuses IMMEDIATELY with
/// the transport-shape "no live receive path" message and exit 1 — NO wake, NO
/// envelope logged, NO failed{wake}. The live path is byte-identical to W3a: a
/// live target the lane can't route (asked of `LaneOps::receive_path`, before any
/// envelope is written) is a plain exit-1 refusal, not a resume-and-deliver.
///
/// It is also THE fixture that splits the repo's two liveness readings: pid 90101
/// with `"status":"idle"` is forged dead, so `send_unified::is_live` (the status
/// enum alone) calls it LIVE while `LaneOps::health` (status plus
/// `(pid, start_time)`) calls it COLD. That disagreement is why the live path
/// passes `wake_if_cold: false` and why `deliver` ATTEMPTS on `false` instead of
/// refusing `Cold` — see `LaneOps::deliver`'s docs. Reconciling the two readings
/// is deliberately a separate, user-visible commit.
#[test]
fn send_live_unroutable_claude_is_unchanged_no_wake_no_envelope() {
    let temp = tempfile::tempdir().unwrap();
    // Live idle claude, no relay_port, empty zmx ⇒ NoLiveReceivePath.
    let row = r#"{"pid":90101,"sessionId":"live-claude-3","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"livewk","version":"0.1.0","kind":"claude-code"}"#;
    let (code, _out, err, log, disps) =
        run_send_with_row(temp.path(), 90101, false, row, &["send", "livewk", "hi"]);
    assert_eq!(code, 1, "a live-but-unroutable target keeps its W3a exit-1 refusal (stderr: {err})");
    assert!(
        err.contains("no live receive path"),
        "expected the transport-shape NoLiveReceivePath refusal, got: {err}"
    );
    assert!(!err.contains("failed{wake}"), "a live target must NOT wake, got: {err}");
    // W3a: this refusal happens BEFORE any envelope is logged (sync, immediate).
    assert!(log.is_empty(), "no envelope logged for a live sync refusal, got: {log:?}");
    assert!(disps.is_empty(), "no disposition for a live sync refusal, got: {disps:?}");
}

/// Live/slow: a COLD CLAUDE target REACHES the real revive machinery (the wake
/// runs the claude pane revive, which drives the detached boot + ADR-0005
/// ready-wait to a genuine ~40-60s timeout under a forged row with no real
/// claude). The load-bearing observation: the send WAKES (does not refuse) and its
/// failure is a `failed{wake}` carrying the revive's own error — proving the
/// claude arm of the wake table is wired to the actual revive. `#[ignore]`d in the
/// fast lane exactly like `cold_claude_attach_attempts_revive_then_fails_loudly`.
///
/// **The revive's error is now the CORE's own, and that is the improvement.**
/// `qd send` used to route its wake through `send_unified::RealWaker`, whose
/// claude arm DISCARDED the revive's typed error and answered the fixed string
/// `could not revive claude session "<name>"` — a sentence that named the session
/// and said nothing about what went wrong. The wake is `LaneOps::wake` now, and
/// `LaneError::WakeFailed` carries `ReviveClaudeError::body()` out unchanged, so
/// the line says which failure it was. The class (`failed{wake}`) and the exit
/// (12) are unmoved.
///
/// Run: `cargo test -p quorum-dispatch --test verbs_a4 -- --ignored send_cold_claude`.
#[test]
#[ignore = "live/slow: drives resume::revive_claude to a ~40-60s boot timeout"]
fn send_cold_claude_wakes_via_real_revive_then_failed_wake() {
    let temp = tempfile::tempdir().unwrap();
    let row = r#"{"pid":90102,"sessionId":"cold-claude-4","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"cold","name":"coldclaudewk","version":"0.1.0","kind":"claude-code"}"#;
    let (code, _out, err, log, disps) =
        run_send_with_row(temp.path(), 90102, false, row, &["send", "coldclaudewk", "hi"]);
    // It WOKE (did not refuse as stopped) and the wake ultimately failed.
    assert_eq!(code, 12, "cold claude whose revive fails → failed{{wake}} exit 12 (stderr: {err})");
    assert!(err.contains("failed{wake}"), "expected failed{{wake}}, got: {err}");
    // The REVIVE's own body, not a wrapper. Which of `ReviveClaudeError`'s bodies
    // lands depends on what the host is missing (no zmx on PATH vs. a launch that
    // never confirms ready), so accept the set — the point is that ONE of the
    // core's sentences is what the user reads.
    assert!(
        ["did not confirm ready", "could not launch zmx", "Failed to resume session"]
            .iter()
            .any(|m| err.contains(m)),
        "the wake ran the real claude revive and carried ITS error out, got: {err}"
    );
    assert!(
        !err.contains("could not revive claude session"),
        "the retired RealWaker wrapper must not come back — it discarded the \
         revive's typed error, got: {err}"
    );
    // Write-then-deliver still held: envelope logged, funnel stamped through to
    // the delivery-failed{wake} event.
    assert!(log.contains("cold-claude-4") || log.contains("coldclaudewk"), "envelope logged: {log:?}");
    assert!(
        disps.contains("\"event\":\"delivery-failed\"") && disps.contains("\"reason\":\"wake\""),
        "delivery-failed{{wake}} event written: {disps:?}"
    );
}

// ===========================================================================
// qd–qf W6 — ADDRESSING: `name@host` sugar → `--host`, single-machine
// host-qualified refusal (refused{no-fleet-state}), --host/@host precedence
// (refused{host}), name@local ≡ bare, and origin ambiguity/unknown aligned to
// the shared Refusal (refused{ambiguous} / refused{unknown}, exit 12, never
// first-match). These drive the REAL binary against a jailed HOME. A foreign
// host is set via QD_HOST so local_host != the target host.
// ===========================================================================

/// Like `run_qd` but with a `QD_HOST` override, so `local_host` (which reads
/// QD_HOST) is a known value and a `--host`/`@host` for a DIFFERENT host is
/// genuinely foreign (single-machine box, no `remote/<h>/`).
fn run_qd_host(dir: &Path, qd_host: &str, args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    common::assert_not_real_home(&home);
    let out = Command::new(qd_bin())
        .args(args)
        .env_remove("QD_HOME")
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        .env("QD_HOST", qd_host)
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `qd send alpha@brano hi` where local_host ("thisbox") != "brano" and there
/// is no `remote/brano/` ⇒ the single-machine host-qualified refusal:
/// refused{no-fleet-state} exit 12. Bare/local is unaffected (proven elsewhere).
#[test]
fn send_name_at_foreign_host_is_refused_no_fleet_state_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_host(temp.path(), "thisbox", &["send", "alpha@brano", "hi"]);
    assert_eq!(code, 12, "host-qualified for a host with no fleet state → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{no-fleet-state}:"),
        "expected the single-machine no-fleet-state refusal, got: {err}"
    );
    assert!(
        err.contains("brano") && err.contains("no fleet state"),
        "the refusal names the host + the absent-fleet-state reason, got: {err}"
    );
}

/// `qd send --host brano alpha hi` (the flag form of the sugar) reaches the SAME
/// refused{no-fleet-state} — the sugar and the flag desugar to one path.
#[test]
fn send_host_flag_foreign_is_refused_no_fleet_state_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_host(temp.path(), "thisbox", &["send", "--host", "brano", "alpha", "hi"]);
    assert_eq!(code, 12, "--host foreign → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{no-fleet-state}:"),
        "--host desugars to the same host-qualified path, got: {err}"
    );
    assert!(err.contains("brano"), "names the host, got: {err}");
}

/// `name@local` where local == local_host ("thisbox") is treated as THIS host
/// (≡ bare): it does NOT hit the no-fleet-state refusal — it falls through to LOCAL
/// resolution and, on an empty registry, is refused{unknown} exit 12 (the aligned
/// local-resolution miss), never refused{no-fleet-state}.
#[test]
fn send_name_at_local_host_resolves_locally_like_bare() {
    let temp = tempfile::tempdir().unwrap();
    // Address host == QD_HOST ⇒ local. Empty registry ⇒ the local resolver misses.
    let (code, _out, err) = run_qd_host(temp.path(), "thisbox", &["send", "ghost@thisbox", "hi"]);
    assert_eq!(code, 12, "name@local → local resolution miss → refused{{unknown}} exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{unknown}:"),
        "name@local is LOCAL (not host-qualified) → the local resolver miss, got: {err}"
    );
    assert!(
        !err.contains("no-fleet-state"),
        "name@local must NOT hit the host-qualified refusal, got: {err}"
    );
    // The bare form of the same name is identical (local resolver miss).
    let (code_b, _out_b, err_b) = run_qd_host(temp.path(), "thisbox", &["send", "ghost", "hi"]);
    assert_eq!(code_b, 12, "bare ghost → refused{{unknown}} exit 12 (stderr: {err_b})");
    assert!(err_b.starts_with("qd send: refused{unknown}:"), "bare local miss, got: {err_b}");
}

/// `--host` AND a DIFFERENT `@host` in the address ⇒ a sync refused{host} (the two
/// host qualifiers disagree), BEFORE any resolution. When they AGREE it is fine
/// (proven via the agreeing pair reaching the same no-fleet-state refusal).
#[test]
fn send_host_flag_and_address_host_disagree_is_refused_host_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    // @brano vs --host zonk ⇒ disagreement.
    let (code, _out, err) = run_qd_host(temp.path(), "thisbox", &["send", "--host", "zonk", "alpha@brano", "hi"]);
    assert_eq!(code, 12, "disagreeing host qualifiers → refused{{host}} exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{host}:"),
        "expected the sync refused{{host}} for disagreement, got: {err}"
    );
    assert!(
        err.contains("brano") && err.contains("zonk"),
        "the refusal names BOTH qualifiers, got: {err}"
    );

    // AGREEING qualifiers (--host brano == @brano) pass the reconciliation gate and
    // reach the host-qualified path → refused{no-fleet-state}, NOT refused{host}.
    let (code_ok, _out_ok, err_ok) = run_qd_host(temp.path(), "thisbox", &["send", "--host", "brano", "alpha@brano", "hi"]);
    assert_eq!(code_ok, 12, "agreeing qualifiers still host-qualified → exit 12 (stderr: {err_ok})");
    assert!(
        err_ok.starts_with("qd send: refused{no-fleet-state}:"),
        "agreeing --host/@host is NOT a host disagreement, got: {err_ok}"
    );
}

/// A stable_id resolves EXACTLY and never trips the address parser (ids carry no
/// `@`). On an empty registry an unknown id is a local resolver miss
/// (refused{unknown}), NOT a host refusal — proving the id path is host-None/local.
#[test]
fn send_stable_id_shaped_query_is_local_not_host_qualified() {
    let temp = tempfile::tempdir().unwrap();
    // An 8-char id-shaped query (no '@') ⇒ addr_host None ⇒ local resolution.
    let (code, _out, err) = run_qd_host(temp.path(), "thisbox", &["send", "ab3kx9mq", "hi"]);
    assert_eq!(code, 12, "unknown id-shaped query → local miss refused{{unknown}} (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{unknown}:") && !err.contains("no-fleet-state"),
        "an id-shaped (no-@) query is local, got: {err}"
    );
}

/// Origin ambiguity is ALIGNED to the shared Refusal: two GENUINELY-LIVE sessions
/// sharing one name ⇒ `qd send <name> hi` is refused{ambiguous} exit 12 — never
/// first-match. Both rows carry THIS test process's live pid so the resolver's
/// pid-aware liveness sees both as alive (two dead-pid rows would collapse to
/// unknown). This is the origin twin of the inbound ambiguity door.
#[test]
fn send_origin_ambiguous_name_is_refused_ambiguous_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(temp.path().join("zmx")).unwrap();
    common::assert_not_real_home(&home);
    let live_pid = std::process::id() as i64; // the test runner — definitely alive.
    // Two DISTINCT sessionIds, SAME name, both idle + live-pid ⇒ Resolution::Many.
    for (fname, sid) in [("a.json", "ambi-A"), ("b.json", "ambi-B")] {
        std::fs::write(
            sessions.join(fname),
            format!(
                r#"{{"pid":{live_pid},"sessionId":"{sid}","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"twin","version":"0.1.0","provider":"mystery"}}"#
            ),
        )
        .unwrap();
    }

    let out = Command::new(qd_bin())
        .args(["send", "twin", "hi"])
        .env_remove("QD_HOME")
        .env("HOME", &home)
        .env("ZMX_DIR", temp.path().join("zmx"))
        .output()
        .expect("spawn qd");
    let code = out.status.code().unwrap_or(-1);
    let err = String::from_utf8_lossy(&out.stderr);
    let root = home.join(".quorum").join("dispatch");
    let log = std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default();
    assert_eq!(code, 12, "ambiguous origin target → refused{{ambiguous}} exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{ambiguous}:"),
        "expected refused{{ambiguous}} (never first-match), got: {err}"
    );
    assert!(err.contains("matches 2 sessions"), "names the collision, got: {err}");
    // A pre-resolution ambiguity refusal logs NOTHING (no envelope, no first-match).
    assert!(log.is_empty(), "an ambiguity refusal must not log/deliver, got: {log:?}");
}
