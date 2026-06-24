//! sbx 0.1 punch-list B2, item 5 — channel-header attribution pins.
//! Spec: ~/work/ws/switchboard/sbx/punch/b2-relay-spec.md.
//!
//! PHASE-1 DEFECT (pinned then, fixed now): `sb send:relay` derived
//! `from_session` from the INHERITED `CLAUDE_CODE_SESSION_ID`, so any process
//! inheriting another session's env mis-attributed its messages — even when
//! the engine's OWN identity (`SB_SESSION_ID`, idstore-resolvable) said
//! otherwise.
//!
//! PHASE-2 FIX (ratified precedence, pinned here full-stack): ENGINE-ASSERTED
//! identity first — `SB_SESSION_ID` resolved through the idstore to the
//! claude uuid (namespace preserved) — then `CLAUDE_CODE_SESSION_ID` only
//! when no engine identity resolves, then `"cli"`. The unit precedence matrix
//! lives in verbs/send_relay.rs; these rows pin the surface end-to-end.
//!
//! Full-stack: the REAL `sb send:relay` binary against a REAL `sb relay:serve`
//! subprocess. The channel header asserted is the
//! `notifications/claude/channel` frame on the target relay's MCP stdout —
//! the exact surface where the fleet observed the wrong identity.
//!
//! Hermetic: tempdir HOME/SB_HOME; relay binds from RELAY_PORT_BASE 33700
//! (outside the live 8900-8999 band); registry/idstore are staged files.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Port base OUTSIDE the live 8900-8999 scan band and clear of the other
/// suites' bases (29700 mcp / 31700 mcp_delivery / 31000+ hardening fakes).
const PORT_BASE: u16 = 33700;

/// Per-frame stdout read budget (no-hang guard).
const READ_BUDGET: Duration = Duration::from_secs(8);

/// The three identities in play:
/// - the TARGET relay session (who we send to);
/// - the IMPOSTER uuid planted in the inherited CLAUDE_CODE_SESSION_ID;
/// - the TRUE identity: SB_SESSION_ID stable id, idstore-bound to a claude uuid.
const TARGET_UUID: &str = "11111111-aaaa-4bbb-8ccc-222222222222";
const IMPOSTER_UUID: &str = "99999999-dead-4bee-8eef-888888888888";
const TRUE_STABLE_ID: &str = "ab3kx9mq";
const TRUE_UUID: &str = "33333333-feed-4abc-8def-444444444444";

/// A running `sb relay:serve` child (the target session's relay) with a
/// line-reader draining its MCP stdout. Same pattern as
/// relay_server_mcp_delivery.rs.
struct RelayChild {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    home: PathBuf,
}

impl RelayChild {
    fn spawn(home: &Path, session_uuid: &str) -> Self {
        let exe = env!("CARGO_BIN_EXE_dispatch");
        let mut child = Command::new(exe)
            .arg("relay:serve")
            .env("HOME", home)
            .env("RELAY_PORT_BASE", PORT_BASE.to_string())
            .env("CLAUDE_CODE_SESSION_ID", session_uuid)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sb relay:serve");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        RelayChild {
            child,
            stdin: Some(stdin),
            lines: rx,
            home: home.to_path_buf(),
        }
    }

    fn send(&mut self, frame: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        let mut buf = frame.to_string().into_bytes();
        buf.push(b'\n');
        stdin.write_all(&buf).expect("write frame");
        stdin.flush().expect("flush");
    }

    /// MCP handshake so the loop is live (faithful to the production boot).
    fn handshake(&mut self) {
        self.send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2024-11-05", "capabilities": {},
                         "clientInfo": { "name": "punch-b2", "version": "0" } }
        }));
        let resp = self.next_json_matching("initialize response", |v| v["id"] == json!(1));
        assert_eq!(resp["result"]["serverInfo"]["name"], "relay");
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }

    fn next_json_matching<F: Fn(&Value) -> bool>(&self, what: &str, pred: F) -> Value {
        let deadline = Instant::now() + READ_BUDGET;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                panic!("timed out waiting for {what}");
            }
            let line = match self.lines.recv_timeout(remaining) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what}"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("relay stdout closed while waiting for {what}")
                }
            };
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON relay stdout line {line:?}: {e}"));
            if pred(&v) {
                return v;
            }
        }
    }

    /// Wait for this relay's sidecar `<home>/.claude/relay/<pid>.json` (it
    /// carries the bound port; existence = boot complete).
    fn wait_for_sidecar(&self) -> u16 {
        let path = self
            .home
            .join(".claude")
            .join("relay")
            .join(format!("{}.json", self.child.id()));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                    if let Some(port) = v.get("port").and_then(Value::as_u64) {
                        return port as u16;
                    }
                }
            }
            if Instant::now() >= deadline {
                panic!("sidecar {path:?} never appeared");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for RelayChild {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Stage the hermetic HOME: .claude dirs, a registry row binding the name
/// "tgt" to TARGET_UUID with pid = THIS TEST PROCESS (alive; the relay child
/// is our direct child, so the verb's fast-path pid-ancestry walk —
/// relay pid → test pid — matches in one hop), zmx/tmp dirs, and the idstore
/// mint binding TRUE_STABLE_ID ↔ TRUE_UUID under SB_HOME/state.
fn stage_home(home: &Path) {
    let claude = home.join(".claude");
    std::fs::create_dir_all(claude.join("sessions")).unwrap();
    std::fs::create_dir_all(claude.join("projects")).unwrap();
    std::fs::create_dir_all(claude.join("relay")).unwrap();
    std::fs::create_dir_all(home.join("zmx")).unwrap();
    std::fs::create_dir_all(home.join("tmp")).unwrap();

    let test_pid = std::process::id();
    let row = format!(
        r#"{{"pid":{test_pid},"sessionId":"{TARGET_UUID}","name":"tgt","status":"idle","cwd":"/work/x","updatedAt":1781000000000,"startedAt":1780990000000}}"#
    );
    std::fs::write(
        claude.join("sessions").join(format!("{test_pid}.json")),
        row,
    )
    .unwrap();

    // idstore: the TRUE engine identity, resolvable stable-id → claude uuid.
    let state = home.join(".quorum").join("dispatch").join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("ids.jsonl"),
        format!(
            "{}\n",
            json!({ "event": "mint", "id": TRUE_STABLE_ID, "session_id": TRUE_UUID })
        ),
    )
    .unwrap();
}

/// Run `sb send:relay tgt <message>` under the staged home. `plant_env`
/// mutates the child env (identity planting per row).
fn run_send_relay(home: &Path, message: &str, plant_env: impl FnOnce(&mut Command)) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dispatch"));
    cmd.args(["send:relay", "tgt", message])
        .env("HOME", home)
        .env("SB_HOME", home.join(".quorum").join("dispatch"))
        .env("ZMX_DIR", home.join("zmx"))
        .env("TMPDIR", home.join("tmp"))
        .env("XDG_RUNTIME_DIR", home.join("tmp"))
        // Keep any fallback port scan inside the jail band.
        .env("RELAY_PORT_BASE", PORT_BASE.to_string())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("SB_SESSION_ID");
    plant_env(&mut cmd);
    let out = cmd.output().expect("run sb send:relay");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "send:relay must succeed; stderr: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let message_id = stdout.lines().next().unwrap_or("").trim().to_string();
    assert!(
        message_id.starts_with("relay-"),
        "stdout must carry the minted message id, got: {stdout}"
    );
    message_id
}

// ---------------------------------------------------------------------------
// FIXED (was the phase-1 defect pin) — both identities planted: SB_SESSION_ID
// (the engine birth property, idstore-bound to TRUE_UUID) and a leaked
// CLAUDE_CODE_SESSION_ID from a different session. The channel header now
// carries the ENGINE-ASSERTED identity (the idstore-resolved claude uuid —
// same namespace, reply routing preserved); the leaked env var no longer
// mis-attributes the message.
// ---------------------------------------------------------------------------
#[test]
fn engine_identity_wins_over_inherited_env_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    stage_home(home);

    let mut relay = RelayChild::spawn(home, TARGET_UUID);
    relay.handshake();
    let _port = relay.wait_for_sidecar();

    let mid = run_send_relay(home, "attribution probe", |cmd| {
        cmd.env("CLAUDE_CODE_SESSION_ID", IMPOSTER_UUID)
            .env("SB_SESSION_ID", TRUE_STABLE_ID);
    });

    let notif = relay.next_json_matching("channel notification", |v| {
        v["method"] == "notifications/claude/channel"
    });
    assert_eq!(
        notif["params"]["content"], "attribution probe",
        "the body rides intact"
    );
    assert_eq!(
        notif["params"]["meta"]["message_id"].as_str(),
        Some(mid.as_str()),
        "the notification is for our send"
    );
    // ---- FIXED SHAPE (was the phase-1 defect pin) ----
    assert_eq!(
        notif["params"]["meta"]["from_session"].as_str(),
        Some(TRUE_UUID),
        "from_session must be the ENGINE-ASSERTED identity \
         (SB_SESSION_ID={TRUE_STABLE_ID} → idstore → {TRUE_UUID}); the leaked \
         CLAUDE_CODE_SESSION_ID ({IMPOSTER_UUID}) must no longer win"
    );
}

// ---------------------------------------------------------------------------
// Fallback leg pinned full-stack: SB_SESSION_ID present but UNRESOLVABLE
// (unknown to the idstore) → the derivation falls through to the inherited
// CLAUDE_CODE_SESSION_ID rather than inventing an identity.
// ---------------------------------------------------------------------------
#[test]
fn unresolvable_engine_identity_falls_back_to_claude_env() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    stage_home(home);

    let mut relay = RelayChild::spawn(home, TARGET_UUID);
    relay.handshake();
    let _port = relay.wait_for_sidecar();

    let mid = run_send_relay(home, "fallback probe", |cmd| {
        cmd.env("CLAUDE_CODE_SESSION_ID", IMPOSTER_UUID)
            // valid id SHAPE, but no idstore mint for it → unresolvable.
            .env("SB_SESSION_ID", "zzzzzzzz");
    });

    let notif = relay.next_json_matching("channel notification", |v| {
        v["method"] == "notifications/claude/channel"
    });
    assert_eq!(
        notif["params"]["meta"]["message_id"].as_str(),
        Some(mid.as_str())
    );
    assert_eq!(
        notif["params"]["meta"]["from_session"].as_str(),
        Some(IMPOSTER_UUID),
        "an unresolvable SB_SESSION_ID falls back to CLAUDE_CODE_SESSION_ID — \
         the derivation never invents an identity"
    );
}

// ---------------------------------------------------------------------------
// Control — the operator-shell attribution that phase 2 must NOT break: with
// neither CLAUDE_CODE_SESSION_ID nor SB_SESSION_ID in the env, from_session
// is "cli" (verbs/send_relay.rs:111 fallback).
// ---------------------------------------------------------------------------
#[test]
fn control_bare_shell_attributes_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    stage_home(home);

    let mut relay = RelayChild::spawn(home, TARGET_UUID);
    relay.handshake();
    let _port = relay.wait_for_sidecar();

    let mid = run_send_relay(home, "operator probe", |_cmd| {
        // both identity vars already removed by run_send_relay
    });

    let notif = relay.next_json_matching("channel notification", |v| {
        v["method"] == "notifications/claude/channel"
    });
    assert_eq!(
        notif["params"]["meta"]["message_id"].as_str(),
        Some(mid.as_str())
    );
    assert_eq!(
        notif["params"]["meta"]["from_session"], "cli",
        "a bare operator shell attributes as cli — phase 2 must preserve this"
    );
}
