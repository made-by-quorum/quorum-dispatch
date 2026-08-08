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
/// unknown session still yields the normal `No session matching` + exit 1 (NOT a
/// refused{expires}). Proves the flag is accepted and the value is consumed.
#[test]
fn send_good_expires_parses_then_resolves_normally() {
    let temp = tempfile::tempdir().unwrap();
    for good in ["12h", "30m", "45s", "1d", "90"] {
        let (code, _out, err) = run_qd(temp.path(), &["send", "--expires", good, "nope", "hi"]);
        assert_eq!(code, 1, "valid --expires {good:?} + unknown session → the normal resolve-miss exit 1 (stderr: {err})");
        assert!(
            err.contains("No session matching \"nope\""),
            "valid --expires {good:?} must reach the resolver, got: {err}"
        );
        assert!(
            !err.contains("refused{expires}"),
            "a valid --expires {good:?} must NOT be refused, got: {err}"
        );
    }
}

/// The unified `qd send` default (no `--expires`) also resolves normally on an
/// empty registry — the flag being absent is the 12h default, never an error.
#[test]
fn send_default_expires_resolves_normally() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(temp.path(), &["send", "ghost", "body"]);
    assert_eq!(code, 1, "unknown session → exit 1 (stderr: {err})");
    assert!(
        err.contains("No session matching \"ghost\""),
        "default-expires send reaches the resolver, got: {err}"
    );
}

// ===========================================================================
// qd–qf W3b: resume-and-deliver — a stopped/cold/killed target is ACCEPTED and
// WOKEN, not refused. "stopped is not a refusal class." These drive the REAL
// binary against a forged NOT-live registry row. To stay hermetic + fast they
// use an UNKNOWN-provider row, which hits the wake path's "cannot be woken
// headlessly" arm IMMEDIATELY (no ~40-60s live revive) — enough to prove (a) the
// old cold/stopped/killed REFUSALS are gone (the path proceeds to a WAKE, not a
// "resume it first" refusal) and (b) the failed{wake} contract: exit 12,
// failed{wake} stderr, envelope logged FIRST + a failed{wake} disposition row.
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

/// A COLD target is no longer refused with "resume it first" / "dead, use resume":
/// the send proceeds to a WAKE. With an unwakeable (unknown-provider) row the wake
/// fails → the failed{wake} contract: exit 12, `failed{wake}` stderr, and (because
/// the envelope is logged BEFORE the wake) an envelope in log.jsonl joined by a
/// failed{wake} disposition.
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
    // The NEW behavior: accepted → wake attempted → failed{wake} (exit 12).
    assert_eq!(code, 12, "unwakeable cold target → failed{{wake}} exit 12 (stderr: {err})");
    assert!(
        err.contains("failed{wake}"),
        "expected the failed{{wake}} render, got: {err}"
    );
    // Envelope logged FIRST (write-then-deliver) + a failed{wake} disposition.
    assert!(
        log.contains("mystery-cold-1") || log.contains("coldwk"),
        "the envelope must be logged before the wake, got log.jsonl: {log:?}"
    );
    assert!(
        disps.contains("\"state\":\"failed\"") && disps.contains("\"reason\":\"wake\""),
        "a failed{{wake}} disposition row must be written, got dispositions.jsonl: {disps:?}"
    );
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
        log.contains("mystery-tomb-2") || log.contains("tombwk"),
        "envelope logged before the wake, got log.jsonl: {log:?}"
    );
    assert!(
        disps.contains("\"state\":\"failed\"") && disps.contains("\"reason\":\"wake\""),
        "failed{{wake}} disposition written, got dispositions.jsonl: {disps:?}"
    );
}

/// LIVE-target-unchanged (regression guard): a live IDLE claude row with no relay
/// and (in this empty-zmx jail) no joined mux pane still refuses IMMEDIATELY with
/// the transport-shape "no live receive path" message and exit 1 — NO wake, NO
/// envelope logged, NO failed{wake}. The live path is byte-identical to W3a: a
/// live target that select_carrier can't route is a plain exit-1 refusal, not a
/// resume-and-deliver.
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
/// runs `resume::revive_claude`, which drives the detached boot + ADR-0005
/// ready-wait to a genuine ~40-60s timeout under a forged row with no real
/// claude). The load-bearing observation: the send WAKES (does not refuse) and its
/// failure is a `failed{wake}` carrying the revive's own error — proving the
/// claude arm of the wake table is wired to the actual revive. `#[ignore]`d in the
/// fast lane exactly like `cold_claude_attach_attempts_revive_then_fails_loudly`.
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
    assert!(
        err.contains("could not revive claude session"),
        "the wake ran the real claude revive (its failure surfaced), got: {err}"
    );
    // Write-then-deliver still held: envelope logged, failed{wake} stamped.
    assert!(log.contains("cold-claude-4") || log.contains("coldclaudewk"), "envelope logged: {log:?}");
    assert!(
        disps.contains("\"state\":\"failed\"") && disps.contains("\"reason\":\"wake\""),
        "failed{{wake}} disposition written: {disps:?}"
    );
}
