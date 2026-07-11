//! D2 (delivery contract §C1) — the DAEMON-ARM DOOR-FAILURE conformance cell,
//! END-TO-END through the ACTUAL COMPILED `qd send:relay` verb (never a bare
//! library call). QS-2: "one injected DOOR failure per lane producing a truthful
//! send-failed." This drives a real, cred-free door injection: a resolved daemon
//! session whose resident is unreachable (a cold codex row, pid absent) → the
//! verb routes to `run_codex_send` → the no-live-pid door → `emit_door_failure`
//! records a `send-failed` terminal into the TARGET's delivery log BEFORE failing
//! loud (exit 1). No model, no daemon, no network.
//!
//! Codex is the CLEANEST door lane to inject end-to-end here: a cold row hits the
//! reachability door directly (no floor, no spawn). The other daemon doors
//! (acp/pi) funnel the identical `emit_door_failure` call — the unit-level
//! `door_failure_emits_send_failed_terminal_keyed_to_target` proves the record
//! shape, and D5's matrix injects the remaining per-lane door cells against this
//! same inventory.

use std::path::PathBuf;
use std::process::Command;

struct Jail {
    _root: tempfile::TempDir,
    home: PathBuf,
    qd_home: PathBuf,
    /// PID registry rows (`<pid>.json`) — the RESOLUTION store, HOME-only
    /// (`<HOME>/.claude/sessions`, QdPaths).
    registry_dir: PathBuf,
    /// Delivery events (`<uuid>.events.jsonl`) — the QD_HOME hot-state store
    /// (`<QD_HOME>/state/sessions`).
    events_dir: PathBuf,
}

fn jail() -> Jail {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let qd_home = root.path().join("qd");
    let registry_dir = home.join(".claude").join("sessions");
    let events_dir = qd_home.join("state").join("sessions");
    std::fs::create_dir_all(&registry_dir).unwrap();
    std::fs::create_dir_all(&events_dir).unwrap();
    Jail {
        _root: root,
        home,
        qd_home,
        registry_dir,
        events_dir,
    }
}

#[test]
fn codex_door_failure_records_send_failed_before_failing_loud() {
    let j = jail();
    let uuid = "019ea0b3-04d3-7400-8d95-doorfailurecel";
    let name = "codex-dead-door";
    let message = "steer: please ack — resident is gone";

    // A real LIVE process backs the row so the session RESOLVES (the scan filters
    // dead pids), but the row carries NO recorded endpoint → the "daemon not
    // reachable" door in run_codex_send (the endpoint-None arm; pid passes the !=0
    // filter, then the endpoint lookup finds none). The live pid is NOT our codex
    // daemon — it is a bare `sleep` — so no wire is ever reachable.
    let mut child = Command::new("sleep").arg("120").spawn().expect("spawn sleep");
    let live_pid = child.id() as i64;
    let entry = dispatch::registry::RegistryEntry {
        pid: Some(live_pid),
        session_id: Some(uuid.into()),
        provider: Some("codex".into()),
        name: Some(name.into()),
        cwd: Some(j.home.to_string_lossy().to_string()),
        ..Default::default()
    };
    dispatch::registry::write_entry(&j.registry_dir, &entry).expect("write codex registry row");

    // Drive the REAL `qd send:relay <name> <msg>`, jailed.
    let out = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args(["send:relay", name, message])
        .env("HOME", &j.home)
        .env("QD_HOME", &j.qd_home)
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output()
        .expect("run qd send:relay");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    // C1 record-then-fail-loud: the verb FAILS LOUD (exit non-zero) ...
    assert!(
        !out.status.success(),
        "a door failure fails loud (exit non-zero); stderr:\n{stderr}"
    );

    // ... AND leaves a truthful send-failed terminal in the TARGET's delivery log.
    let log_path = j.events_dir.join(format!("{uuid}.events.jsonl"));
    let raw = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("target delivery log {log_path:?} must exist: {e}\nstderr:\n{stderr}"));
    let sf = raw
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["event"] == "send-failed")
        .unwrap_or_else(|| panic!("a send-failed terminal must be recorded; log:\n{raw}"));

    assert_eq!(sf["event"], "send-failed");
    assert_eq!(
        sf["reason"], "daemon-unreachable",
        "the codex reachability door's honest surface token"
    );
    assert_eq!(
        sf["content_sha256"].as_str().unwrap(),
        dispatch::events::sha256_hex(message.as_bytes()),
        "content_sha256 over the raw message (the consumer's correlation key)"
    );
    assert!(
        sf.get("send_id").is_none(),
        "send_id OMITTED at a pre-wire door (spec `send-failed {{ send_id? }}`)"
    );
    // It is a terminal — the send is RESOLVED (never a silent stderr-only exit).
    assert!(dispatch::events::is_terminal("send-failed"));

    // Evidence dump when requested.
    if let Ok(dir) = std::env::var("DISPATCH_PROOF_DIR") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            std::path::Path::new(&dir).join("D2-codex-door-send-failed.jsonl"),
            &raw,
        );
    }

    // Reap the backing sleep.
    let _ = child.kill();
    let _ = child.wait();
}
