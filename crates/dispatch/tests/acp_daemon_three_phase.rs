//! D2 (delivery contract §C3/§C5 · QS-2 lane-coverage audit) — the ACP/claude-code
//! DIRECTLY-OBSERVED 3-PHASE conformance cell.
//!
//! QS-2 clause 2 ("Lane-coverage audit"): *"for every carrier lane testable on
//! the box … ONE observed send produces all three phases at the declared
//! strength, graded conformance-suite, not vibes."* This is the single
//! verb-level drive proving exactly that for the acp/claude-code lane:
//!
//!   ONE `qd send:relay` → the resident ACP daemon → ONE `qd wait`
//!        ⇓ delivery log (`<uuid>.events.jsonl`), for that ONE send's send_id:
//!     phase 1  send-initiated   (verb=send:relay, send_path=acp/claude-code)
//!     phase 2  turn-accepted    (NON-terminal DELIVERED)
//!     phase 3  message-seen     (TERMINAL, StopReason=end_turn → landed)
//!   all three IN SEQUENCE, correlated by the SAME send_id + content_sha256.
//!
//! WHY THIS CELL EXISTS (the gap it closes): before this, the ACP 3-phase was
//! proven only COMPOSITIONALLY — per-phase unit tests (`send_relay.rs` /
//! `wait.rs`) + the real-`AcpHost` terminal-correlation cell (`acp_cc_live.rs`)
//! + the codex DOOR end-to-end (`delivery_daemon_door_conformance.rs`). No
//! single verb-level drive tied "one observed send" to all three phases. A
//! skeptical fresh reader could find that short of QS-2. This cell IS that
//! drive, at the seam the real product uses (`run_acp_send` + `run_acp_wait`).
//!
//! COVERAGE: acp/claude-code DIRECTLY; opencode COMPOSITIONALLY — opencode
//! routes through the IDENTICAL `run_acp_send`/`run_acp_wait` verb path (proven
//! by the shared ACP tests + `acp_opencode_live.rs`); the only difference is the
//! bridge binary, which this cell fakes.
//!
//! CRED-FREE / DETERMINISTIC: the bridge is a canned ~30-line node script that
//! answers `end_turn`; no model, no network, no API budget, no OAuth. Auto-
//! skips only if `node` is absent (the bridge runtime).
//!
//! ADAPTED (per the artifacts doctrine — fork the how, meet the target) from
//! three committed patterns:
//!   * `tests/acp_cc_live.rs`      — `write_fake_bridge` (end_turn) + `node_program`
//!   * `tests/acp_opencode_live.rs`— spawn the REAL `qd acp-daemon --listen/--cwd/
//!                                   --bridge-cmd/--bridge-arg`; poll readiness
//!   * `tests/delivery_daemon_door_conformance.rs` — the HOME-registry /
//!                                   QD_HOME-events jail; drive the REAL
//!                                   `CARGO_BIN_EXE_qd` verb; read the delivery log.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use dispatch::provider::acp::AcpConnection;

/// `node` on PATH? (the bridge — real or fake — is a node program.) Auto-skip
/// mirrors `acp_cc_live.rs`: a box without node cannot host the bridge runtime.
fn node_program() -> Option<String> {
    for cand in ["node", "/usr/bin/node"] {
        if Command::new(cand)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(cand.to_string());
        }
    }
    None
}

/// The deterministic fake ACP bridge: mirrors the real bridge wire shape for the
/// methods the daemon's boot + one-turn path exercise, but canned/instant/free.
/// `session/prompt` streams ONE update then answers `stopReason:"end_turn"` —
/// the landed (message-seen) terminal. Byte-identical in spirit to
/// `acp_cc_live.rs::write_fake_bridge`.
fn write_fake_bridge(dir: &Path) -> PathBuf {
    let script = r#"
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
function send(o){ process.stdout.write(JSON.stringify(o) + "\n"); }
rl.on('line', (line) => {
  line = line.trim(); if (!line) return;
  let m; try { m = JSON.parse(line); } catch { return; }
  const { id, method, params } = m;
  if (method === 'initialize') {
    send({ jsonrpc:"2.0", id, result:{ protocolVersion:1, agentInfo:{name:"fake-acp",version:"0"}, authMethods:[] }});
  } else if (method === 'session/new') {
    send({ jsonrpc:"2.0", id, result:{ sessionId:"fake-sess-3phase" }});
  } else if (method === 'session/prompt') {
    // stream ONE update, THEN the terminal response carrying the SAME rpc id.
    send({ jsonrpc:"2.0", method:"session/update", params:{ sessionId: params.sessionId, update:{ sessionUpdate:"agent_message_chunk", content:{type:"text", text:"OK"} }}});
    send({ jsonrpc:"2.0", id, result:{ stopReason:"end_turn" }});
  } else if (method === 'session/load') {
    send({ jsonrpc:"2.0", id, result:{}});
  } else if (id !== undefined && id !== null) {
    send({ jsonrpc:"2.0", id, error:{code:-32601, message:"unknown method"}});
  }
});
"#;
    let path = dir.join("fake-acp-bridge.js");
    std::fs::write(&path, script).unwrap();
    path
}

/// The jail. Everything is HOME-derived and `QD_HOME` is left UNSET on every
/// child (`env_remove`), so the two delivery-store resolutions coincide:
///   * the SEND emitter (`emit_daemon_send_events` → `QdPaths::from_home_env`)
///   * the WAIT terminal emitter (`emit_acp_terminal` → `common::paths_from_home`
///     → `QdPaths::from_home`, which IGNORES `QD_HOME`)
/// With `QD_HOME` unset both resolve `state_dir = <HOME>/.quorum/dispatch/state`,
/// so send-initiated/turn-accepted and message-seen land in the SAME log —
/// the honest configuration this cell must prove the contract under.
///
/// HISTORY: under a `QD_HOME` override ≠ `<HOME>/.quorum/dispatch`, these two
/// resolutions once DIVERGED — the send phases went to `<QD_HOME>/state/sessions`
/// while the acp `message-seen` terminal went to
/// `<HOME>/.quorum/dispatch/state/sessions`, orphaning the terminal from its send.
/// That was a real D2 delivery-lie surfaced by this cell's first cross-process
/// run; it is FIXED at commit `b6a65da9` (`emit_acp_terminal` now self-resolves
/// via `from_home_env`). This cell keeps `QD_HOME` unset (the resolutions coincide);
/// the sibling cell
/// `acp_daemon_three_phase_under_qd_home_override_lands_in_qd_home_log_never_orphans`
/// proves the fix end-to-end under a divergent `QD_HOME`.
struct Jail {
    _root: tempfile::TempDir,
    home: PathBuf,
    registry_dir: PathBuf,
    events_dir: PathBuf,
}

fn jail() -> Jail {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    // The pid REGISTRY (resolution store) is HOME-derived: <HOME>/.claude/sessions.
    let registry_dir = home.join(".claude").join("sessions");
    // The delivery EVENTS store with QD_HOME unset: <HOME>/.quorum/dispatch/state/sessions.
    let events_dir = home
        .join(".quorum")
        .join("dispatch")
        .join("state")
        .join("sessions");
    std::fs::create_dir_all(&registry_dir).unwrap();
    std::fs::create_dir_all(&events_dir).unwrap();
    Jail {
        _root: root,
        home,
        registry_dir,
        events_dir,
    }
}

/// Reap the daemon child (SIGTERM lets `qd acp-daemon` drain its bridge, then wait).
fn reap(mut child: Child) {
    let _ = Command::new("kill").arg(child.id().to_string()).status();
    let _ = child.wait();
}

#[test]
fn acp_daemon_one_send_produces_all_three_phases_in_sequence() {
    let node = match node_program() {
        Some(n) => n,
        None => {
            eprintln!("node not found — skipping the acp 3-phase cell (no fake bridge runtime)");
            return;
        }
    };
    let j = jail();
    let cwd = j.home.to_string_lossy().to_string();
    let fake = write_fake_bridge(&j.home);

    // A free loopback port for the resident's ws front (same as the opencode smoke).
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("ws://127.0.0.1:{port}");

    // Spawn the REAL `qd acp-daemon` fronting the fake node bridge — jailed on
    // HOME, QD_HOME left UNSET (store-coincidence, see jail()). stdout/stderr →
    // a captured log for evidence.
    let daemon_log = j.home.join("acp-daemon.log");
    let logf = std::fs::File::create(&daemon_log).unwrap();
    let daemon = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args([
            "acp-daemon",
            "--listen",
            &url,
            "--cwd",
            &cwd,
            "--bridge-cmd",
            &node,
            "--bridge-arg",
            &fake.to_string_lossy(),
        ])
        .env("HOME", &j.home)
        .env_remove("QD_HOME")
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(logf.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(logf))
        .spawn()
        .expect("spawn qd acp-daemon");

    // Readiness: poll connect + status until the resident's ACP session is up.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready_sid = None;
    while Instant::now() < deadline {
        if let Ok(c) = AcpConnection::connect(&url, Duration::from_millis(500)) {
            if let Ok(Some(sid)) = c.status_session_id() {
                ready_sid = Some(sid);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if ready_sid.is_none() {
        let log = std::fs::read_to_string(&daemon_log).unwrap_or_default();
        reap(daemon);
        panic!("acp daemon not ready in 30s (fake bridge); daemon log:\n{log}");
    }

    // Register the session so `qd send:relay <name>` / `qd wait <name>` resolve
    // to THIS resident (by name → live pid → endpoint). The registry session_id
    // is the delivery-log key (the target uuid) — it need NOT equal the bridge's
    // ACP session id (the ws front ignores the session arg; wire.rs).
    let name = "acp-3phase";
    let uuid = "019ea0b3-04d3-7400-8d95-acp3phasecell";
    let entry = dispatch::registry::RegistryEntry {
        pid: Some(daemon.id() as i64),
        session_id: Some(uuid.into()),
        provider: Some("acp/claude-code".into()),
        name: Some(name.into()),
        cwd: Some(cwd.clone()),
        endpoint: Some(url.clone()),
        ..Default::default()
    };
    dispatch::registry::write_entry(&j.registry_dir, &entry).expect("write acp registry row");

    let message = "steer: please ack — the directly-observed three-phase drive";
    let expect_sha = dispatch::events::sha256_hex(message.as_bytes());

    // ---- PHASES 1+2: the REAL `qd send:relay <name> <msg>` (run_acp_send). ----
    // Injects the turn over the ws front → prints the turn id → emits
    // send-initiated + turn-accepted into the target delivery log.
    let send = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args(["send:relay", name, message])
        .env("HOME", &j.home)
        .env_remove("QD_HOME")
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output()
        .expect("run qd send:relay");
    let send_err = String::from_utf8_lossy(&send.stderr).to_string();
    if !send.status.success() {
        let log = std::fs::read_to_string(&daemon_log).unwrap_or_default();
        reap(daemon);
        panic!("qd send:relay must succeed (a live resident accepted the turn); stderr:\n{send_err}\ndaemon log:\n{log}");
    }
    let turn_id = String::from_utf8_lossy(&send.stdout).trim().to_string();
    assert!(
        !turn_id.is_empty(),
        "send:relay prints the injected turn id (the send_id)"
    );

    // ---- PHASE 3: the REAL `qd wait <name>` (run_acp_wait). ----
    // The turn is still in-flight (its terminal sits buffered on the resident's
    // bus — `in_flight` is only cleared when a consumer drains via next_update,
    // client.rs), so `qd wait` passes the entry-idle short-circuit, pulls the
    // end_turn terminal, and emits message-seen. Bounded timeout so a hang fails
    // loud rather than camping the default 120s.
    let wait = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args(["wait", name, "--timeout", "30000"])
        .env("HOME", &j.home)
        .env_remove("QD_HOME")
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output()
        .expect("run qd wait");
    let wait_err = String::from_utf8_lossy(&wait.stderr).to_string();

    // Read the target delivery log FIRST so a wait-exit assertion failure still
    // dumps the primary source.
    let log_path = j.events_dir.join(format!("{uuid}.events.jsonl"));
    let raw = std::fs::read_to_string(&log_path).unwrap_or_else(|e| {
        let dlog = std::fs::read_to_string(&daemon_log).unwrap_or_default();
        panic!("target delivery log {log_path:?} must exist: {e}\nwait stderr:\n{wait_err}\ndaemon log:\n{dlog}");
    });

    // Evidence dump when requested (mirrors the door cell's DISPATCH_PROOF_DIR).
    if let Ok(dir) = std::env::var("DISPATCH_PROOF_DIR") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(Path::new(&dir).join("D2-acp-3phase.jsonl"), &raw);
    }

    assert!(
        wait.status.success(),
        "qd wait must return 0 on a clean end_turn terminal; stderr:\n{wait_err}\nlog:\n{raw}"
    );

    // ---- The 3-phase assertion: all three phases, IN SEQUENCE, one send. ----
    let recs: Vec<serde_json::Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each delivery-log line is JSON"))
        .collect();

    // The index (append order) of each phase for THIS send_id.
    let phase_idx = |event: &str| -> Option<usize> {
        recs.iter()
            .position(|r| r["event"] == event && r["send_id"].as_str() == Some(turn_id.as_str()))
    };
    let i_init = phase_idx("send-initiated").unwrap_or_else(|| {
        panic!("phase 1 send-initiated (send_id={turn_id}) missing; log:\n{raw}")
    });
    let i_acc = phase_idx("turn-accepted").unwrap_or_else(|| {
        panic!("phase 2 turn-accepted (send_id={turn_id}) missing; log:\n{raw}")
    });
    let i_seen = phase_idx("message-seen")
        .unwrap_or_else(|| panic!("phase 3 message-seen (send_id={turn_id}) missing; log:\n{raw}"));

    // SEQUENCE: send-initiated < turn-accepted < message-seen (append order).
    assert!(
        i_init < i_acc && i_acc < i_seen,
        "the three phases must appear IN ORDER (sent<delivered<terminal): \
         send-initiated@{i_init} turn-accepted@{i_acc} message-seen@{i_seen}\nlog:\n{raw}"
    );

    let init = &recs[i_init];
    let acc = &recs[i_acc];
    let seen = &recs[i_seen];

    // Phase 1 — send-initiated: verb + lane (send_path=provider) + content key.
    assert_eq!(
        init["verb"], "send:relay",
        "phase 1 carries the send:relay verb"
    );
    assert_eq!(
        init["send_path"], "acp/claude-code",
        "phase 1 send_path is the LANE (session.provider) — the reader recovers strong-vs-floor from it (C3)"
    );
    assert_eq!(
        init["content_sha256"].as_str(),
        Some(expect_sha.as_str()),
        "phase 1 content_sha256 = sha256(raw message)"
    );

    // Phase 2 — turn-accepted: NON-terminal DELIVERED, same send_id + content key.
    assert_eq!(
        acc["content_sha256"].as_str(),
        Some(expect_sha.as_str()),
        "phase 2 turn-accepted carries the same content_sha256"
    );
    assert!(
        !dispatch::events::is_terminal("turn-accepted"),
        "turn-accepted is the NON-terminal DELIVERED phase (never a terminal)"
    );

    // Phase 3 — message-seen: the TERMINAL, same send_id, content key joined
    // from the correlated send-initiated (emit_acp_terminal reads it back).
    assert_eq!(
        seen["content_sha256"].as_str(),
        Some(expect_sha.as_str()),
        "phase 3 message-seen content_sha256 = the correlated send's content key (send_id-join)"
    );
    assert!(
        dispatch::events::is_terminal("message-seen"),
        "message-seen is the TERMINAL (landed) phase"
    );

    // One observed send ⇒ exactly one of each phase (no double terminal, etc.).
    let count = |event: &str| {
        recs.iter()
            .filter(|r| r["event"] == event && r["send_id"].as_str() == Some(turn_id.as_str()))
            .count()
    };
    assert_eq!(
        count("send-initiated"),
        1,
        "exactly one send-initiated for the one send"
    );
    assert_eq!(
        count("turn-accepted"),
        1,
        "exactly one turn-accepted for the one send"
    );
    assert_eq!(
        count("message-seen"),
        1,
        "exactly one message-seen (terminal) for the one send"
    );

    eprintln!(
        "3-PHASE GREEN (acp/claude-code, one observed send, send_id={turn_id}): \
         send-initiated@{i_init} -> turn-accepted@{i_acc} -> message-seen@{i_seen}"
    );

    reap(daemon);
}

/// R6 QD_HOME-fix — the CROSS-PROCESS, END-TO-END proof of commit `b6a65da9`
/// (bonus strengthening, additive). This is the SAME cross-process drive as the
/// sibling cell, but run under a `QD_HOME` override that DIFFERS from
/// `<HOME>/.quorum/dispatch` — the exact configuration that regressed BEFORE the
/// fix (the acp `message-seen` terminal resolved its store via `from_home`
/// (QD_HOME-ignorant) and orphaned into a different log than its send phases,
/// which used `from_home_env`). After the fix `emit_acp_terminal` self-resolves
/// via `from_home_env(env)`, so:
///   1. all THREE phases land in the ONE `QD_HOME` delivery log (`<QD_HOME>/state`),
///      correlated by send_id, through the REAL `qd send:relay` + `qd wait` verbs; and
///   2. the terminal NEVER orphans into the `from_home` log
///      (`<HOME>/.quorum/dispatch/state`) — the delivery-lie guard, cross-process.
/// Complements the executor's unit-level guard
/// (`wait::tests::acp_terminal_shares_delivery_log_with_send_phases_under_qd_home`)
/// by proving the fix through the actual verb processes, not the emit fn directly.
#[test]
fn acp_daemon_three_phase_under_qd_home_override_lands_in_qd_home_log_never_orphans() {
    let node = match node_program() {
        Some(n) => n,
        None => {
            eprintln!("node not found — skipping the acp QD_HOME 3-phase cell");
            return;
        }
    };
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let qd_home = root.path().join("qd"); // ≠ <HOME>/.quorum/dispatch (the divergence)
                                          // Registry (resolution store) is HOME-derived regardless of QD_HOME.
    let registry_dir = home.join(".claude").join("sessions");
    // The QD_HOME-honoring delivery log (where all 3 phases MUST land, post-fix).
    let qd_events_dir = qd_home.join("state").join("sessions");
    // The from_home (QD_HOME-ignorant) log a REGRESSED terminal would orphan into.
    let home_events_dir = home
        .join(".quorum")
        .join("dispatch")
        .join("state")
        .join("sessions");
    std::fs::create_dir_all(&registry_dir).unwrap();
    std::fs::create_dir_all(&qd_events_dir).unwrap();
    let cwd = home.to_string_lossy().to_string();
    let fake = write_fake_bridge(&home);

    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("ws://127.0.0.1:{port}");

    // Spawn the daemon under HOME + the divergent QD_HOME.
    let daemon_log = home.join("acp-daemon.log");
    let logf = std::fs::File::create(&daemon_log).unwrap();
    let daemon = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args([
            "acp-daemon",
            "--listen",
            &url,
            "--cwd",
            &cwd,
            "--bridge-cmd",
            &node,
            "--bridge-arg",
            &fake.to_string_lossy(),
        ])
        .env("HOME", &home)
        .env("QD_HOME", &qd_home)
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(logf.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(logf))
        .spawn()
        .expect("spawn qd acp-daemon");

    // Readiness.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Ok(c) = AcpConnection::connect(&url, Duration::from_millis(500)) {
            if let Ok(Some(_)) = c.status_session_id() {
                ready = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !ready {
        let log = std::fs::read_to_string(&daemon_log).unwrap_or_default();
        reap(daemon);
        panic!("acp daemon not ready in 30s (QD_HOME cell); daemon log:\n{log}");
    }

    let name = "acp-3phase-qdh";
    let uuid = "019ea0b3-04d3-7400-8d95-acp3phaseqdh0";
    let entry = dispatch::registry::RegistryEntry {
        pid: Some(daemon.id() as i64),
        session_id: Some(uuid.into()),
        provider: Some("acp/claude-code".into()),
        name: Some(name.into()),
        cwd: Some(cwd.clone()),
        endpoint: Some(url.clone()),
        ..Default::default()
    };
    dispatch::registry::write_entry(&registry_dir, &entry).expect("write registry row");

    let message = "steer: three-phase under a QD_HOME override";
    let expect_sha = dispatch::events::sha256_hex(message.as_bytes());

    // Phases 1+2 via the REAL verb, under QD_HOME.
    let send = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args(["send:relay", name, message])
        .env("HOME", &home)
        .env("QD_HOME", &qd_home)
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output()
        .expect("run qd send:relay");
    if !send.status.success() {
        let dlog = std::fs::read_to_string(&daemon_log).unwrap_or_default();
        reap(daemon);
        panic!(
            "qd send:relay must succeed under QD_HOME; stderr:\n{}\ndaemon log:\n{dlog}",
            String::from_utf8_lossy(&send.stderr)
        );
    }
    let turn_id = String::from_utf8_lossy(&send.stdout).trim().to_string();
    assert!(!turn_id.is_empty(), "send:relay prints the turn id");

    // Phase 3 via the REAL verb, under QD_HOME.
    let wait = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args(["wait", name, "--timeout", "30000"])
        .env("HOME", &home)
        .env("QD_HOME", &qd_home)
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output()
        .expect("run qd wait");
    let wait_err = String::from_utf8_lossy(&wait.stderr).to_string();

    // Read the QD_HOME delivery log — the 3 phases MUST be here (post-fix).
    let qd_log_path = qd_events_dir.join(format!("{uuid}.events.jsonl"));
    let raw = std::fs::read_to_string(&qd_log_path).unwrap_or_else(|e| {
        let dlog = std::fs::read_to_string(&daemon_log).unwrap_or_default();
        panic!("QD_HOME delivery log {qd_log_path:?} must exist: {e}\nwait stderr:\n{wait_err}\ndaemon log:\n{dlog}");
    });
    if let Ok(dir) = std::env::var("DISPATCH_PROOF_DIR") {
        let _ = std::fs::write(Path::new(&dir).join("D2-acp-3phase-qdhome.jsonl"), &raw);
    }
    assert!(
        wait.status.success(),
        "qd wait must return 0 under QD_HOME; stderr:\n{wait_err}\nlog:\n{raw}"
    );

    let recs: Vec<serde_json::Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("delivery-log line is JSON"))
        .collect();
    let idx = |event: &str| -> Option<usize> {
        recs.iter()
            .position(|r| r["event"] == event && r["send_id"].as_str() == Some(turn_id.as_str()))
    };
    let i_init = idx("send-initiated")
        .unwrap_or_else(|| panic!("send-initiated missing in the QD_HOME log:\n{raw}"));
    let i_acc = idx("turn-accepted")
        .unwrap_or_else(|| panic!("turn-accepted missing in the QD_HOME log:\n{raw}"));
    let i_seen = idx("message-seen").unwrap_or_else(|| {
        panic!("message-seen missing in the QD_HOME log — the delivery-lie would put it in the from_home log:\n{raw}")
    });
    assert!(
        i_init < i_acc && i_acc < i_seen,
        "all 3 phases in order in the ONE QD_HOME log: si@{i_init} ta@{i_acc} ms@{i_seen}\n{raw}"
    );
    assert_eq!(
        recs[i_seen]["content_sha256"].as_str(),
        Some(expect_sha.as_str()),
        "the message-seen content_sha256 joins its send in the QD_HOME log"
    );

    // The delivery-lie guard, cross-process: the terminal must NOT orphan into the
    // from_home log. (Pre-fix, message-seen landed HERE and was orphaned from the
    // QD_HOME-logged send phases.)
    let orphan_path = home_events_dir.join(format!("{uuid}.events.jsonl"));
    let orphaned = std::fs::read_to_string(&orphan_path)
        .map(|s| s.contains("message-seen"))
        .unwrap_or(false);
    assert!(
        !orphaned,
        "REGRESSION: the acp terminal orphaned into the from_home log {orphan_path:?} (QD_HOME delivery-lie)"
    );

    eprintln!(
        "QD_HOME 3-PHASE GREEN (cross-process, send_id={turn_id}): all 3 phases in <QD_HOME>/state, \
         zero orphan in <HOME>/.quorum/dispatch/state"
    );
    reap(daemon);
}
