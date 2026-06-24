//! A4 verb-level integration tests (send:pty / send:http / wait) driving the
//! REAL `sb` binary against a JAILED, empty HOME (L9a / ADD-4 — never the real
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

fn sb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dispatch")
}

/// Run `sb <args...>` with HOME + ZMX_DIR jailed into `home`/`zmx` under `dir`.
/// Returns (exit_code, stdout, stderr).
fn run_sb(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    common::assert_not_real_home(&home);

    let out = Command::new(sb_bin())
        .args(args)
        // L9a: jailed HOME; ZMX_DIR pinned to an empty dir so zmx finds nothing.
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        // Keep zmx from being on PATH-relevant for these resolve-only paths; the
        // empty registry + empty zmx dir already yield zero sessions.
        .output()
        .expect("spawn sb");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// --- P0 W1 (sbx spec-cli §11): new/kill are RETIRED erroring stubs ---------
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
        let (code, _out, err) = run_sb(temp.path(), &args);
        assert_eq!(
            code, 1,
            "retired `new` → exit 1 for {args:?} (stderr: {err})"
        );
        assert!(
            err.contains("sb new: `new` is retired; use `sb start`"),
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
        let (code, _out, err) = run_sb(temp.path(), &args);
        assert_eq!(
            code, 1,
            "retired `kill` → exit 1 for {args:?} (stderr: {err})"
        );
        assert!(
            err.contains("sb kill: `kill` is retired; use `sb stop`"),
            "retired-stub stderr for {args:?}, got: {err}"
        );
    }
}

#[test]
fn stop_unknown_session_exits_1() {
    // The LIVE stop verb reaches the real backend (resolveOrDie path).
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_sb(temp.path(), &["stop", "nosuch"]);
    assert_eq!(code, 1, "unknown session → exit 1 (stderr: {err})");
    assert!(
        err.contains("No session matching \"nosuch\""),
        "stderr should be resolveOrDie's message, got: {err}"
    );
}

#[test]
fn send_pty_unknown_session_exits_1() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_sb(temp.path(), &["send:pty", "nosuch", "hello"]);
    assert_eq!(code, 1, "unknown session → exit 1 (stderr: {err})");
    assert!(
        err.contains("No session matching \"nosuch\""),
        "stderr should be resolveOrDie's message, got: {err}"
    );
}

#[test]
fn send_http_unknown_session_exits_1() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_sb(temp.path(), &["send:http", "nosuch", "hello"]);
    assert_eq!(code, 1, "unknown session → exit 1 (stderr: {err})");
    assert!(
        err.contains("No session matching \"nosuch\""),
        "stderr should be resolveOrDie's message, got: {err}"
    );
}

#[test]
fn wait_unknown_session_exits_1() {
    let temp = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_sb(temp.path(), &["wait", "nosuch"]);
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

    let out = Command::new(sb_bin())
        .args(["send:http", "wk", "hello"])
        .env("HOME", &home)
        .env("ZMX_DIR", temp.path().join("zmx"))
        .output()
        .expect("spawn sb");
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
        err.contains("sb send:relay wk") && err.contains("sb send:pty wk"),
        "expected the send:relay / send:pty guidance bullets, got: {err}"
    );
}

#[test]
fn wait_idle_session_reports_idle_exit_0() {
    // A seeded LIVE IDLE claude session: `sb wait` entry idle check →
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

    let out = Command::new(sb_bin())
        .args(["wait", "idlewk"])
        .env("HOME", &home)
        .env("ZMX_DIR", temp.path().join("zmx"))
        .output()
        .expect("spawn sb");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(code, 0, "idle entry → exit 0 (stdout: {stdout})");
    assert!(
        stdout.contains("idlewk is idle"),
        "expected '<label> is idle', got: {stdout}"
    );
}
