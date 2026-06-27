//! Verb-level `qd send:relay` test (spec §4.5): the no-relay error path.
//!
//! Drives the REAL `qd` binary (via `CARGO_BIN_EXE_qd`) with a hermetic tempdir
//! HOME and NO relay listening, asserting the exact stderr wording + exit 1
//! (send.ts:407-408: `Session "<name>" has no relay.`).
//!
//! Hermetic discipline (rule 9 / ADD-4): HOME, SB_HOME, ZMX_DIR, TMPDIR,
//! XDG_RUNTIME_DIR all point into the tempdir; the test NEVER touches the real
//! `~/.claude` or a real relay. A dummy relay sidecar is written so the engine
//! takes the sidecar path and NEVER runs the live HTTP port-scan probe (which
//! would connect to 100 real localhost ports).

use std::fs;
use std::path::Path;
use std::process::Command;

/// Build a hermetic env for the `qd` child: every state root inside `home`.
fn hermetic_env(cmd: &mut Command, home: &Path) {
    let claude = home.join(".claude");
    fs::create_dir_all(claude.join("sessions")).unwrap();
    fs::create_dir_all(claude.join("projects")).unwrap();
    fs::create_dir_all(claude.join("relay")).unwrap();
    let zmx = home.join("zmx");
    fs::create_dir_all(&zmx).unwrap();
    let tmp = home.join("tmp");
    fs::create_dir_all(&tmp).unwrap();

    // A dummy relay sidecar: makes read_sidecars non-empty so get_relay_ports
    // never invokes the live HTTP port-scan probe. Its pid (424242) has no
    // ancestry link to our session pid, so it never matches as OUR relay.
    fs::write(
        claude.join("relay").join("dummy.json"),
        r#"{"port":8999,"sessionId":"other-sess","pid":424242}"#,
    )
    .unwrap();

    cmd.env("HOME", home)
        .env("SB_HOME", home.join(".quorum").join("dispatch"))
        .env("ZMX_DIR", &zmx)
        .env("TMPDIR", &tmp)
        .env("XDG_RUNTIME_DIR", &tmp)
        // Clear anything that could leak a real session id into from_session.
        .env_remove("CLAUDE_CODE_SESSION_ID");
}

/// Write a live registry entry (a session with a name + pid, NO relay ancestry).
fn write_session(home: &Path, pid: i64, name: &str, session_id: &str) {
    let entry = format!(
        r#"{{"pid":{pid},"sessionId":"{session_id}","name":"{name}","status":"idle","cwd":"/work/x","updatedAt":1717495000000,"startedAt":1717490000000}}"#
    );
    fs::write(
        home.join(".claude")
            .join("sessions")
            .join(format!("{pid}.json")),
        entry,
    )
    .unwrap();
}

#[test]
fn send_relay_no_relay_errors_exit_1() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    // L9a: never the real home (the tempdir guarantees this, but assert it).
    if let Ok(real) = std::env::var("HOME") {
        let real = std::path::PathBuf::from(real)
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from("/nonexistent-real-home"));
        assert_ne!(
            temp.path(),
            real,
            "test home must never equal the real HOME"
        );
    }

    // A live session "lonely" with a pid that no relay's ancestry reaches → the
    // fast lookup finds the name but no relay matches; the full-scan fallback
    // resolves the session with relay_port = None → "has no relay." exit 1.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qd"));
    hermetic_env(&mut cmd, &home); // creates the .claude dirs first
    write_session(&home, 31337, "lonely", "lonely-sid");
    cmd.args(["send:relay", "lonely", "hello"]);

    let out = cmd.output().expect("run qd send:relay");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        out.status.code(),
        Some(1),
        "no-relay must exit 1; stderr was: {stderr}"
    );
    assert!(
        stderr.contains(r#"Session "lonely" has no relay."#),
        "expected the exact no-relay wording, got: {stderr}"
    );
}
