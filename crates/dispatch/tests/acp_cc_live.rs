//! scoped-ACP-CC pillar 2 — the LIVE Claude-Code-over-ACP end-to-end lane + the
//! deterministic terminal-correlation DISCRIMINATING test.
//!
//! Two classes (the codex `*_live.rs` precedent):
//!
//! 1. **`acp_terminal_correlation_through_real_subprocess`** (DEFAULT suite, auto-
//!    skips only if `node` is absent): drives the REAL [`AcpHost`] — real bridge
//!    SUBPROCESS + real SC-5 reader thread + real `mpsc` bus + real SC-1 queue —
//!    against a DETERMINISTIC fake ACP agent (a tiny node script, NO model budget,
//!    NO network). It proves the rpc-id→turn correlation end-to-end: `session/prompt`
//!    is sent with rpc id N, the agent's response echoes N, and `next_update`
//!    correlates N back to the exact turn and emits `Terminal{turn}`. **This is the
//!    discriminating test (§2 charge): a revert that breaks terminal-correlation —
//!    drop the `inflight.insert`/`remove`, or release the wrong turn — makes the
//!    response uncorrelated, so no `Terminal` is ever emitted and the test reds.**
//!
//! 2. **`acp_cc_live_full_lifecycle`** (gated on `SB_ACP_LIVE=1`): drives a REAL
//!    Claude Code session over the official `claude-code-acp` bridge through the
//!    `AcpProvider` seam: boot (`initialize`) → `session/new` → send-turn (the
//!    production `Provider::inject` ladder) → `session/update` STREAM → terminal
//!    `StopReason` → CONF-EVENT live (the real host bus driving `AcpTurnCompletion`)
//!    → interrupt (`session/cancel`) → resume (`session/load` on a fresh bridge).
//!    Primary evidence: the captured transcript tee (Pete's-view feed) + the live
//!    stop reasons, written to `$SB_ACP_EVIDENCE` (default under
//!    `~/work/acp-cc-coord/`). A no-op unless `SB_ACP_LIVE=1` so the default suite
//!    never spawns a real CC session or spends API budget.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dispatch::effects::MapEnv;
use dispatch::paths::SbPaths;
use dispatch::provider::acp::{AcpClient, AcpEvent, AcpHost, AcpTurnCompletion, StopReason};
use dispatch::provider::acp::{Confidence, TerminalReason};
use dispatch::provider::{provider_for, InjectError, ProviderFx, SessionKey};
use dispatch::wait::{TurnCompletion, TurnCompletionProbe};
use tempfile::TempDir;

/// `node` on PATH? (the bridge — real or fake — is a node program).
fn node_program() -> Option<String> {
    for cand in ["node", "/usr/bin/node"] {
        if std::process::Command::new(cand)
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

/// The live gate: skip unless `SB_ACP_LIVE=1`.
fn live() -> bool {
    std::env::var("SB_ACP_LIVE").as_deref() == Ok("1")
}

/// The real `claude-code-acp` bridge entry script (`$SB_ACP_BRIDGE` overrides the
/// STEP-0 install default).
fn real_bridge_script() -> PathBuf {
    if let Ok(p) = std::env::var("SB_ACP_BRIDGE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home)
        .join("work/acp-step0/node_modules/@zed-industries/claude-code-acp/dist/index.js")
}

/// Write the deterministic fake ACP agent script and return its path (under the test
/// tempdir). Mirrors the real bridge's wire shape EXACTLY for the methods the
/// correlation path exercises — but canned, instant, free.
fn write_fake_bridge(dir: &std::path::Path) -> PathBuf {
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
    send({ jsonrpc:"2.0", id, result:{ sessionId:"fake-sess-1" }});
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

/// Pull updates until a terminal, with an overall budget. Returns
/// `(updates, terminal_event)`.
fn drive_to_terminal(host: &AcpHost, budget: Duration) -> (Vec<AcpEvent>, AcpEvent) {
    let mut updates = Vec::new();
    let deadline = Instant::now() + budget;
    loop {
        assert!(Instant::now() < deadline, "no terminal within budget — updates so far: {updates:#?}");
        match host.next_update(Duration::from_millis(500)) {
            Ok(Some(ev @ AcpEvent::Update { .. })) => updates.push(ev),
            Ok(Some(term @ AcpEvent::Terminal { .. })) => return (updates, term),
            Ok(Some(term @ AcpEvent::TerminalError { .. })) => return (updates, term),
            Ok(None) => continue, // quiet stream — keep waiting until the budget
            Err(e) => panic!("transport error before terminal: {e}"),
        }
    }
}

// ===========================================================================
// 1. DEFAULT-SUITE discriminating test: terminal correlation through the REAL
//    subprocess + reader thread + bus + queue, against a deterministic agent.
// ===========================================================================

#[test]
fn acp_terminal_correlation_through_real_subprocess() {
    let node = match node_program() {
        Some(n) => n,
        None => {
            eprintln!("node not found — skipping acp correlation test (no fake bridge runtime)");
            return;
        }
    };
    let tmp = TempDir::new().unwrap();
    let fake = write_fake_bridge(tmp.path());
    let host = AcpHost::spawn(&node, &[fake.to_string_lossy().to_string()], tmp.path())
        .expect("spawn fake bridge");

    // Handshake.
    let init = host.initialize().expect("initialize");
    assert_eq!(init.protocol_version, 1);
    assert!(!init.auth_required, "fake agent advertises no auth");
    let session = host.new_session(&tmp.path().to_string_lossy()).expect("session/new");
    assert_eq!(session, "fake-sess-1");

    // Send a turn → get its attributable id back IMMEDIATELY (SC-1, does not block).
    let turn = host.prompt(&session, "ping", "correlation-test").expect("prompt");

    // The single reader correlates: first an Update (the stream), then the terminal
    // whose rpc id maps back to THIS turn.
    let (updates, terminal) = drive_to_terminal(&host, Duration::from_secs(10));
    assert!(!updates.is_empty(), "expected the streamed session/update");
    match terminal {
        AcpEvent::Terminal { turn: t, stop, .. } => {
            assert_eq!(t, turn, "terminal MUST correlate to the turn that was sent (rpc-id→turn)");
            assert_eq!(stop, StopReason::EndTurn);
        }
        other => panic!("expected a correlated Terminal, got {other:?}"),
    }

    // CONF-EVENT through the REAL host bus: the completion probe reads the host's
    // observation and latches Visible + (Completed, ProtocolConfirmed).
    let completion = AcpTurnCompletion::new(&host);
    assert_eq!(completion.poll_completion(), TurnCompletionProbe::Visible);
    assert_eq!(
        completion.terminal_reason(),
        Some((TerminalReason::Completed, Confidence::ProtocolConfirmed))
    );

    // The transcript tee captured the assistant text (Pete's-view feed).
    assert_eq!(host.assistant_text(), "OK");
}

// SC-1a: the configured outbound-queue bound is HONORED across session/new (LOW-1).
// A host spawned at capacity 2 must overflow on the 4th inject — NOT silently widen
// back to the default 16. This is the regression guard for the LOW-1 fix: on the
// pre-fix code (new_session hardcoded DEFAULT_QUEUE_CAPACITY) the 4th inject FITS and
// this test reds. Driven through the production Provider::inject ladder so the overflow
// surfaces as the real InjectError::Precondition("queue full") (SC-1a backpressure,
// distinct from the SC-3 busy ban). We never call next_update, so the in-flight turn
// never completes and the backlog fills deterministically (no model, no network).
#[test]
fn acp_host_queue_overflow_honors_configured_capacity() {
    let node = match node_program() {
        Some(n) => n,
        None => {
            eprintln!("node not found — skipping acp capacity test");
            return;
        }
    };
    let tmp = TempDir::new().unwrap();
    let fake = write_fake_bridge(tmp.path());
    // CONFIGURED bound = 2 (deliberately not the default 16).
    let host =
        AcpHost::spawn_with_capacity(&node, &[fake.to_string_lossy().to_string()], tmp.path(), 2)
            .expect("spawn fake bridge");
    host.initialize().expect("initialize");
    let session = host.new_session(&tmp.path().to_string_lossy()).expect("session/new");

    let env = MapEnv::default();
    let paths = SbPaths::from_home(tmp.path());
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: tmp.path().to_path_buf(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: Some(&host),
    };
    let provider = provider_for("acp/claude-code").expect("acp/claude-code registered");
    let key = SessionKey { id: &session, name: Some("cap"), cwd: Some("/work/proj"), pid: None };

    // inject #1 → in-flight; #2,#3 → backlog (len reaches the bound of 2).
    for i in 1..=3 {
        provider
            .inject(&fx, &key, "m", "cap-test")
            .unwrap_or_else(|e| panic!("inject #{i} should fit at capacity 2: {e:?}"));
    }
    // #4 → overflow at the CONFIGURED bound of 2 (would fit on the pre-fix always-16 code).
    let overflow = provider
        .inject(&fx, &key, "m", "cap-test")
        .expect_err("the 4th inject must overflow at the configured capacity of 2");
    match overflow {
        InjectError::Precondition(msg) => assert_eq!(msg, "queue full"),
        other => panic!("expected Precondition(\"queue full\"), got {other:?}"),
    }
}

// (W) WEDGED-BUT-ALIVE (Item 3, red-team round-1 #1): `AcpHost::bridge_confirmed_dead`
// is TRUE only when the bridge child has genuinely EXITED (reaped, no zombie), and FALSE
// while it is alive — the silent-loss-proof gate the adapter self-terminates on. Uses a
// trivial `sh` child as a bridge stand-in (no model/network); the check is pure child
// liveness, not protocol. (ii)/(iii): a live child is never "confirmed dead".
#[test]
fn bridge_confirmed_dead_flips_only_when_child_exits() {
    let tmp = TempDir::new().unwrap();
    // A child that STAYS ALIVE → NOT confirmed dead (a transient blip / busy turn arm).
    let alive = AcpHost::spawn("sh", &["-c".to_string(), "sleep 30".to_string()], tmp.path())
        .expect("spawn alive bridge stand-in");
    assert!(!alive.bridge_confirmed_dead(), "a live bridge child is NOT confirmed dead");
    drop(alive); // Drop kills + reaps it (no leak).
    // A child that EXITS → confirmed dead (and reaped here → no zombie).
    let dead = AcpHost::spawn("sh", &["-c".to_string(), "exit 0".to_string()], tmp.path())
        .expect("spawn exiting bridge stand-in");
    let mut confirmed = false;
    for _ in 0..100 {
        if dead.bridge_confirmed_dead() {
            confirmed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(confirmed, "an exited bridge child is confirmed dead (reaped)");
    assert!(dead.bridge_confirmed_dead(), "confirmed-dead is stable after reaping (cached)");
}

// ===========================================================================
// 2. LIVE Claude Code lane (SB_ACP_LIVE=1): the real bridge, the real engine.
// ===========================================================================

#[test]
fn acp_cc_live_full_lifecycle() {
    if !live() {
        eprintln!("SB_ACP_LIVE != 1 — skipping the live Claude-Code-over-ACP lifecycle");
        return;
    }
    let node = node_program().expect("node required for the live bridge");
    let bridge = real_bridge_script();
    assert!(bridge.exists(), "bridge script not found at {bridge:?} (set SB_ACP_BRIDGE)");

    let mut log = EvidenceLog::open();
    log.line("=== scoped-ACP-CC pillar 2 LIVE lane ===");
    log.line(&format!("bridge={bridge:?} node={node}"));

    let work = TempDir::new().unwrap();
    let cwd = work.path().to_string_lossy().to_string();

    // The long-lived host: spawn the REAL bridge (BRIDGE_ENV_STRIP applied — we run
    // INSIDE a CC session, so this exercises the real nesting guard).
    let host = AcpHost::spawn(&node, &[bridge.to_string_lossy().to_string()], work.path())
        .expect("spawn live bridge");

    // --- Drive the PROVIDER SEAM: boot readiness = the ACP initialize handshake. ---
    let provider = provider_for("acp/claude-code").expect("acp/claude-code is registered");
    let env = MapEnv::default();
    let paths = SbPaths::from_home(work.path());
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: work.path().to_path_buf(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: Some(&host),
    };
    provider
        .boot_waiter(&fx)
        .wait_ready("acp-cc-live")
        .expect("ACP boot (initialize) reaches ready");
    log.line("boot_waiter.wait_ready OK — initialize handshake succeeded through the provider seam");

    // session/new (host-level: the verb layer drives this at create time).
    let session = host.new_session(&cwd).expect("session/new");
    log.line(&format!("session/new -> sessionId={session}"));

    // --- SEND a turn through the production Provider::inject ladder. ---
    let key = SessionKey { id: &session, name: Some("acp-cc-live"), cwd: Some(&cwd), pid: None };
    let turn = provider
        .inject(&fx, &key, "Reply with exactly the single word PONG and nothing else. Do not use any tools.", "acp-cc-live-test")
        .expect("Provider::inject (LIVE) returns a turn id");
    log.line(&format!("Provider::inject -> turn={turn}"));

    // --- STREAM + TERMINAL: drive next_update to the real terminal StopReason. ---
    let (updates, terminal) = drive_to_terminal(&host, Duration::from_secs(120));
    log.line(&format!("streamed {} session/update(s)", updates.len()));
    for ev in &updates {
        if let AcpEvent::Update { kind, .. } = ev {
            log.line(&format!("  update kind={kind}"));
        }
    }
    let (term_turn, term_stop) = match &terminal {
        AcpEvent::Terminal { turn, stop, .. } => (turn.clone(), *stop),
        other => panic!("expected a Terminal StopReason, got {other:?}"),
    };
    assert_eq!(term_turn, turn, "the terminal correlates to the injected turn");
    assert_eq!(term_stop, StopReason::EndTurn, "a clean PONG turn terminates end_turn");
    assert!(!updates.is_empty(), "the live turn streamed session/update notifications");
    log.line(&format!("TERMINAL stopReason={} turn={term_turn}", term_stop.as_wire()));

    // --- CONF-EVENT LIVE: the real host bus drives the completion contract. ---
    let completion = AcpTurnCompletion::new(&host);
    assert_eq!(completion.poll_completion(), TurnCompletionProbe::Visible);
    assert_eq!(
        completion.terminal_reason(),
        Some((TerminalReason::Completed, Confidence::ProtocolConfirmed))
    );
    log.line("CONF-EVENT live: poll_completion=Visible, reason=(Completed, ProtocolConfirmed)");

    // --- Transcript tee (Pete's-view): the assistant text Pete reads. ---
    let assistant = host.assistant_text();
    log.line(&format!("transcript tee assistant_text={assistant:?}"));
    assert!(assistant.contains("PONG"), "the tee captured the assistant reply (PONG)");

    // --- INTERRUPT: a long turn, cancel it after the first update, observe cancelled. ---
    let turn2 = provider
        .inject(&fx, &key, "Write a long, detailed 600-word essay about the history of the ocean. Do not use any tools.", "acp-cc-live-test")
        .expect("inject the long turn");
    log.line(&format!("inject long turn -> turn={turn2}; will cancel mid-stream"));
    // Wait for the first streamed update (the turn is genuinely in flight)...
    let mut saw_update = false;
    let first_deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < first_deadline && !saw_update {
        match host.next_update(Duration::from_millis(500)) {
            Ok(Some(AcpEvent::Update { .. })) => saw_update = true,
            Ok(Some(other)) => panic!("turn terminated before we could cancel: {other:?}"),
            Ok(None) => {}
            Err(e) => panic!("transport error awaiting first update: {e}"),
        }
    }
    assert!(saw_update, "the long turn started streaming before cancel");
    host.cancel(&session).expect("session/cancel notification");
    log.line("session/cancel sent");
    // ...then pull to the terminal: the bridge resolves the in-flight prompt cancelled.
    let (_u2, terminal2) = drive_to_terminal(&host, Duration::from_secs(60));
    match &terminal2 {
        AcpEvent::Terminal { turn: t, stop, .. } => {
            assert_eq!(*t, turn2, "the cancelled terminal correlates to the long turn");
            log.line(&format!("INTERRUPT terminal stopReason={}", stop.as_wire()));
            // The bridge maps an interrupt to `cancelled`; accept end_turn only if the
            // generation raced to completion (documented), but assert it is terminal.
            assert!(
                matches!(stop, StopReason::Cancelled | StopReason::EndTurn),
                "interrupt yields a terminal (cancelled, or end_turn if it raced)"
            );
            assert_eq!(*stop, StopReason::Cancelled, "session/cancel interrupted the in-flight turn");
        }
        other => panic!("expected a terminal after cancel, got {other:?}"),
    }

    // --- RESUME: load the prior session on a FRESH bridge (session/load). ---
    let host2 = AcpHost::spawn(&node, &[bridge.to_string_lossy().to_string()], work.path())
        .expect("spawn second bridge for resume");
    host2.initialize().expect("initialize the resume bridge");
    host2.load_session(&session, &cwd).expect("session/load resumes the prior session");
    assert_eq!(host2.session_id().as_deref(), Some(session.as_str()));
    log.line(&format!("RESUME: session/load re-established sessionId={session} on a fresh bridge"));

    log.line("=== LIVE lane GREEN: send -> stream -> terminal -> interrupt -> resume ===");
    log.flush_to_disk();
}

/// A primary-evidence sink: tees the live lane's narration to stderr (visible under
/// `--nocapture`) AND to `$SB_ACP_EVIDENCE` (default under `~/work/acp-cc-coord/`).
struct EvidenceLog {
    path: PathBuf,
    buf: String,
}
impl EvidenceLog {
    fn open() -> Self {
        let path = std::env::var("SB_ACP_EVIDENCE").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join("work/acp-cc-coord/acp-cc-live-evidence.log")
        });
        EvidenceLog { path, buf: String::new() }
    }
    fn line(&mut self, s: &str) {
        eprintln!("{s}");
        self.buf.push_str(s);
        self.buf.push('\n');
    }
    fn flush_to_disk(&self) {
        if let Ok(mut f) = std::fs::File::create(&self.path) {
            let _ = f.write_all(self.buf.as_bytes());
            eprintln!("[evidence written to {:?}]", self.path);
        }
    }
}
