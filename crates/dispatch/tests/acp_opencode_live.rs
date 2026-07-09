//! opencode-ACP LIVE lane — the crate-backed driver driving REAL `opencode acp` end-to-end.
//!
//! The "working live driver" proof for the opencode atomic (migration-spec §7). Gated on
//! `QD_OPENCODE_LIVE=1` so the default suite never spawns opencode (heavy: each spawn = node +
//! opencode's internal HTTP server). It drives the crate-backed `AcpHost` against real
//! `opencode acp`: `initialize` → `session/new` → `prompt` → the `session/update` stream → the
//! terminal `StopReason` → the transcript tee reply, then a long turn + `session/cancel`.
//!
//! The RETAINED ws front (`serve`/`AcpConnection`) is UNCHANGED and separately proven by the
//! `wire.rs` faithfulness tests + the `acp_residence.rs` tests (all green byte-for-byte) — the
//! `AcpHost` is `!Sync` (single-`RefCell`) so the front is exercised cross-PROCESS by the actual
//! `qd acp-daemon` (see the handoff runbook), not shared across threads in-process. This lane
//! isolates the NEW surface: the crate driver on real opencode.
//!
//! Evidence (RUN-not-read) → `$QD_OPENCODE_EVIDENCE`.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dispatch::provider::acp::{
    AcpClient, AcpConnection, AcpEvent, AcpHost, AcpTurnCompletion, StopReason,
};
use dispatch::provider::acp::{Confidence, TerminalReason};
use dispatch::wait::{TurnCompletion, TurnCompletionProbe};
use tempfile::TempDir;

fn live() -> bool {
    std::env::var("QD_OPENCODE_LIVE").as_deref() == Ok("1")
}

fn opencode_on_path() -> bool {
    std::process::Command::new("opencode")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn evidence_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("work/ws/quorum/underway/multi-harness/evidence/opencode-acp")
}

struct Evidence {
    path: PathBuf,
    buf: String,
}
impl Evidence {
    fn open() -> Self {
        let path = std::env::var("QD_OPENCODE_EVIDENCE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| evidence_dir().join("opencode-live-drive.log"));
        Evidence { path, buf: String::new() }
    }
    fn open_named(name: &str) -> Self {
        Evidence { path: evidence_dir().join(name), buf: String::new() }
    }
    fn line(&mut self, s: &str) {
        eprintln!("{s}");
        self.buf.push_str(s);
        self.buf.push('\n');
    }
    fn flush(&self) {
        if let Ok(mut f) = std::fs::File::create(&self.path) {
            let _ = f.write_all(self.buf.as_bytes());
            eprintln!("[evidence → {:?}]", self.path);
        }
    }
}

fn drive_to_terminal(host: &AcpHost, budget: Duration) -> (Vec<AcpEvent>, AcpEvent) {
    let mut updates = Vec::new();
    let deadline = Instant::now() + budget;
    loop {
        assert!(Instant::now() < deadline, "no terminal within budget; updates={updates:#?}");
        match host.next_update(Duration::from_millis(500)) {
            Ok(Some(ev @ AcpEvent::Update { .. })) => updates.push(ev),
            Ok(Some(term @ AcpEvent::Terminal { .. })) => return (updates, term),
            Ok(Some(term @ AcpEvent::TerminalError { .. })) => return (updates, term),
            Ok(None) => continue,
            Err(e) => panic!("transport error before terminal: {e}"),
        }
    }
}

#[test]
fn opencode_live_drive_end_to_end() {
    if !live() {
        eprintln!("QD_OPENCODE_LIVE != 1 — skipping the opencode live drive");
        return;
    }
    assert!(opencode_on_path(), "opencode must be on PATH for the live drive");
    let mut ev = Evidence::open();
    ev.line("=== opencode-ACP LIVE: crate-backed driver + real `opencode acp` ===");

    let work = TempDir::new().unwrap();
    let cwd = work.path().to_string_lossy().to_string();

    let host =
        AcpHost::spawn("opencode", &["acp".to_string()], work.path()).expect("spawn opencode acp");

    // Boot: the ACP initialize handshake with real opencode.
    let init = host.initialize().expect("initialize handshake");
    ev.line(&format!(
        "initialize OK: agent={:?} v{:?} proto={} auth_required={}",
        init.agent_name, init.agent_version, init.protocol_version, init.auth_required
    ));
    assert_eq!(init.protocol_version, 1);
    let session = host.new_session(&cwd).expect("session/new");
    ev.line(&format!("session/new -> {session}"));

    // TURN 1: a trivial prompt to a real terminal StopReason.
    let turn = host
        .prompt(&session, "Reply with exactly the single word PONG and nothing else. Do not use any tools.", "opencode-live", &|| {})
        .expect("prompt");
    ev.line(&format!("prompt -> turn={turn}"));
    let (updates, terminal) = drive_to_terminal(&host, Duration::from_secs(120));
    ev.line(&format!("streamed {} update(s)", updates.len()));
    match &terminal {
        AcpEvent::Terminal { turn: t, stop, .. } => {
            assert_eq!(t, &turn, "terminal correlates to the injected turn");
            assert_eq!(*stop, StopReason::EndTurn, "clean turn terminates end_turn");
            ev.line(&format!("TERMINAL stop={} turn={t}", stop.as_wire()));
        }
        other => panic!("expected a Terminal, got {other:?}"),
    }
    assert!(!updates.is_empty(), "the live turn streamed session/update notifications");

    // CONF-EVENT through the real host bus.
    let completion = AcpTurnCompletion::new(&host);
    assert_eq!(completion.poll_completion(), TurnCompletionProbe::Visible);
    assert_eq!(
        completion.terminal_reason(),
        Some((TerminalReason::Completed, Confidence::ProtocolConfirmed))
    );

    // Transcript tee (Pete's-view): the assistant reply.
    let assistant = host.assistant_text();
    ev.line(&format!("assistant_text={assistant:?}"));
    assert!(assistant.contains("PONG"), "the tee captured the opencode reply (PONG)");

    // TURN 2 + cancel (gate-c bit): a long turn, cancelled mid-stream.
    let turn2 = host
        .prompt(&session, "Write a detailed 600-word essay about the history of the ocean. Do not use any tools.", "opencode-live", &|| {})
        .expect("prompt long turn");
    ev.line(&format!("long turn -> turn={turn2}; cancelling mid-stream"));
    let mut saw = false;
    let d = Instant::now() + Duration::from_secs(60);
    while Instant::now() < d && !saw {
        match host.next_update(Duration::from_millis(500)) {
            Ok(Some(AcpEvent::Update { .. })) => saw = true,
            Ok(Some(other)) => panic!("long turn ended before cancel: {other:?}"),
            Ok(None) => {}
            Err(e) => panic!("transport error awaiting first update: {e}"),
        }
    }
    assert!(saw, "the long turn started streaming before cancel");
    host.cancel(&session).expect("session/cancel");
    ev.line("session/cancel sent");
    let (_u2, terminal2) = drive_to_terminal(&host, Duration::from_secs(60));
    match &terminal2 {
        AcpEvent::Terminal { turn: t, stop, .. } => {
            assert_eq!(*t, turn2, "cancelled terminal correlates to the long turn");
            ev.line(&format!("INTERRUPT terminal stop={}", stop.as_wire()));
            assert!(
                matches!(stop, StopReason::Cancelled | StopReason::EndTurn),
                "cancel yields a terminal (cancelled, or end_turn if it raced)"
            );
        }
        other => panic!("expected a terminal after cancel, got {other:?}"),
    }

    host.shutdown();
    ev.line("=== LIVE GREEN: crate driver + real opencode: init -> turn -> terminal -> cancel ===");
    ev.flush();
}

/// SIGTERM a pid via `kill` (integration tests cannot use `libc`, a normal-not-dev dep). SIGTERM
/// (not SIGKILL) lets `qd acp-daemon`'s handler drain opencode cleanly.
fn sigterm(pid: u32) {
    let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
}

/// Cross-PROCESS residence happy-path SMOKE: the ACTUAL `qd acp-daemon` binary fronting real
/// `opencode acp` (the RESIDENT), driven by a SEPARATE driver (this process, via `AcpConnection`
/// over the RETAINED ws front). This is the target-item-2 proof — the crate-backed driver drives
/// opencode at runtime, cross-process, through the retained front — under a THROWAWAY QD_HOME.
/// It is also the recipe the conformance instance reproduces for gates a/b/c.
#[test]
fn opencode_live_daemon_smoke_cross_process() {
    if !live() {
        eprintln!("QD_OPENCODE_LIVE != 1 — skipping cross-process daemon smoke");
        return;
    }
    assert!(opencode_on_path(), "opencode must be on PATH");
    let mut ev = Evidence::open_named("opencode-daemon-smoke.log");
    ev.line("=== opencode-ACP cross-process SMOKE: qd acp-daemon(opencode) + AcpConnection driver ===");

    let qd = env!("CARGO_BIN_EXE_qd");
    let qd_home = TempDir::new().unwrap(); // THROWAWAY QD_HOME — never the live ~/.quorum fabric
    let work = TempDir::new().unwrap();
    let cwd = work.path().to_string_lossy().to_string();

    // A free loopback port for the resident's ws front.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("ws://127.0.0.1:{port}");

    // The resident's stdout+stderr → a captured log (opencode diagnostics route here via the
    // daemon's inherited stderr — MF3 evidence).
    let daemon_log = evidence_dir().join("opencode-daemon-stderr.log");
    let out = std::fs::File::create(&daemon_log).unwrap();

    // Spawn the RESIDENT: real qd acp-daemon fronting `opencode acp`, under throwaway QD_HOME.
    let mut daemon = std::process::Command::new(qd)
        .args([
            "acp-daemon", "--listen", &url, "--cwd", &cwd,
            "--bridge-cmd", "opencode", "--bridge-arg", "acp",
        ])
        .env("QD_HOME", qd_home.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(out))
        .spawn()
        .expect("spawn qd acp-daemon");
    ev.line(&format!(
        "resident: qd acp-daemon pid={} listen={url} bridge=(opencode acp) QD_HOME={:?} log={:?}",
        daemon.id(), qd_home.path(), daemon_log
    ));

    // Readiness: poll connect + status until the resident's opencode session is established.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut ready = None;
    while Instant::now() < deadline {
        if let Ok(c) = AcpConnection::connect(&url, Duration::from_millis(500)) {
            if let Ok(Some(sid)) = c.status_session_id() {
                ev.line(&format!("resident ready: sessionId={sid}"));
                ready = Some((c, sid));
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let (conn, session) = match ready {
        Some(x) => x,
        None => {
            sigterm(daemon.id());
            let _ = daemon.wait();
            ev.flush();
            panic!("resident not ready in 60s (see {daemon_log:?})");
        }
    };

    // DRIVE one real turn cross-process through the RETAINED ws front (separate driver process).
    let turn = conn
        .prompt(&session, "Reply with exactly the single word PONG and nothing else. Do not use any tools.", "opencode-smoke", &|| {})
        .expect("prompt over ws front");
    ev.line(&format!("prompt -> turn={turn}"));
    let (updates, terminal) = {
        let mut updates = Vec::new();
        let d = Instant::now() + Duration::from_secs(120);
        loop {
            assert!(Instant::now() < d, "no terminal within budget");
            match conn.next_update(Duration::from_millis(500)) {
                Ok(Some(u @ AcpEvent::Update { .. })) => {
                    if let AcpEvent::Update { kind, .. } = &u {
                        ev.line(&format!("  update kind={kind}"));
                    }
                    updates.push(u);
                }
                Ok(Some(t @ (AcpEvent::Terminal { .. } | AcpEvent::TerminalError { .. }))) => {
                    break (updates, t)
                }
                Ok(None) => continue,
                Err(e) => panic!("ws transport error before terminal: {e}"),
            }
        }
    };
    ev.line(&format!("streamed {} update(s) cross-front", updates.len()));
    match &terminal {
        AcpEvent::Terminal { turn: t, stop, .. } => {
            assert_eq!(t, &turn, "terminal correlates to the injected turn cross-process");
            assert_eq!(*stop, StopReason::EndTurn);
            ev.line(&format!("TERMINAL stop={} turn={t}", stop.as_wire()));
        }
        other => panic!("expected a Terminal through the front, got {other:?}"),
    }
    assert!(!updates.is_empty(), "the live turn streamed updates cross-front");

    // Teardown: SIGTERM the resident so it drains opencode cleanly (S8).
    sigterm(daemon.id());
    let status = daemon.wait().ok();
    ev.line(&format!("resident exit status={status:?}"));
    ev.line("=== SMOKE GREEN: cross-process opencode turn -> terminal through the retained ws front ===");
    ev.flush();
}
