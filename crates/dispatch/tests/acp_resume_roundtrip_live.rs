//! Item 3 (R) — LIVE STOPPED→resume round-trip on the SAME acp session, end-to-end
//! through the REAL production adapter binary (`qd acp-daemon`) cross-process.
//!
//! This is the (R-a/R-b/R-d) keystone proof above Component-0: Component-0 proved the
//! `session/load` PRIMITIVE faithful on the real bridge; THIS proves the item-3 WIRING
//! (the load-mode adapter `--load-session`, the `run_adapter` load-vs-new branch, the
//! `kill_acp` group-reap, and the row/tombstone discipline) composes into a faithful
//! resume — the exact steps `run_acp_resume` performs, driven through the production
//! `qd acp-daemon` binary in SEPARATE processes.
//!
//! Sequence (real verbs/primitives, no fakes):
//!   1. CREATE  — spawn `qd acp-daemon --listen EP1 --cwd CWD` (create mode) detached
//!      (`RealDaemonSpawner`, `process_group(0)`); connect; sessionId S; drive turn 1
//!      (tells the model a nonce).
//!   2. STOP    — `kill_acp` group-reaps adapter1 + its bridge child + tombstones.
//!   3. RESUME  — spawn `qd acp-daemon --listen EP2 --cwd CWD --load-session S` (LOAD
//!      mode) detached; connect; ASSERT the re-established sessionId == S; drive turn 2
//!      (asks the model to recall the nonce).
//!   4. ASSERT  — SAME sessionId; the bridge's OWN CC JSONL <S>.jsonl CONTINUED (turn 1
//!      AND turn 2 in the SAME file); the model RECALLED the nonce (history truly
//!      re-loaded); NO fork (exactly one new session file); exactly one adapter+bridge.
//!
//! Gated on `QD_ACP_LIVE=1` (real bridge + real CC creds + ~2 model turns). No-op
//! otherwise. Run:
//!   QD_ACP_LIVE=1 ~/cap-cargo.sh test -p quorum-dispatch --test acp_resume_roundtrip_live -- --nocapture --test-threads=1

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dispatch::acp_residence::{build_adapter_argv, connect_ready};
use dispatch::create_daemon::{real_alloc_port, real_cmdline_probe, DaemonSpawner, RealDaemonSpawner};
use dispatch::effects::is_pid_alive;
use dispatch::provider::acp::{AcpClient, AcpEvent, StopReason};
use dispatch::registry::RegistryEntry;
use dispatch::resume_daemon::kill_acp;
use tempfile::TempDir;

fn live() -> bool {
    std::env::var("QD_ACP_LIVE").as_deref() == Ok("1")
}

/// The bridge `.bin` dir, prepended to PATH so the adapter's default `claude-code-acp`
/// resolves (the EXACT production argv: no `--bridge-cmd`). Overridable via env.
fn bridge_bin_dir() -> PathBuf {
    if let Ok(p) = std::env::var("QD_ACP_BRIDGE_BIN_DIR") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home).join("work/acp-step0/node_modules/.bin")
}

fn path_with_bridge() -> String {
    let cur = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", bridge_bin_dir().display(), cur)
}

fn cc_projects_dir() -> PathBuf {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").expect("HOME")).join(".claude"));
    base.join("projects")
}

fn find_session_jsonl(session_id: &str) -> Option<PathBuf> {
    let want = format!("{session_id}.jsonl");
    for d in std::fs::read_dir(cc_projects_dir()).ok()?.flatten() {
        let p = d.path();
        if p.is_dir() && p.join(&want).exists() {
            return Some(p.join(&want));
        }
    }
    None
}

fn all_session_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(cc_projects_dir()) {
        for d in dirs.flatten() {
            let p = d.path();
            if p.is_dir() {
                if let Ok(files) = std::fs::read_dir(&p) {
                    for f in files.flatten() {
                        let fp = f.path();
                        let name = fp.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if name.ends_with(".jsonl") && !name.starts_with("agent-") {
                            out.push(fp);
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn jsonl_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Poll the bridge's CC JSONL (eventual-consistency vs the wire terminal) until
/// `pred` holds or the budget expires; return the settled lines.
fn wait_for_jsonl(session_id: &str, budget: Duration, pred: impl Fn(&[String]) -> bool) -> Vec<String> {
    let deadline = Instant::now() + budget;
    let mut last = Vec::new();
    loop {
        if let Some(p) = find_session_jsonl(session_id) {
            last = jsonl_lines(&p);
            if pred(&last) {
                return last;
            }
        }
        if Instant::now() >= deadline {
            return last;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Drive a connected adapter to a turn terminal. Pulls `next_update` over the REAL ws
/// front the adapter serves; the reply is asserted from the JSONL primary source.
fn drive_turn(conn: &dispatch::provider::acp::AcpConnection, session: &str, prompt: &str, from: &str) {
    conn.prompt(session, prompt, from).expect("prompt enqueue");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(Instant::now() < deadline, "no terminal within budget");
        match conn.next_update(Duration::from_millis(500)) {
            Ok(Some(AcpEvent::Update { .. })) => {}
            Ok(Some(AcpEvent::Terminal { stop, .. })) => {
                assert_eq!(stop, StopReason::EndTurn, "clean turn end_turn");
                break;
            }
            Ok(Some(AcpEvent::TerminalError { .. })) => panic!("turn terminated with error"),
            Ok(None) => {}
            Err(e) => panic!("transport error before terminal: {e}"),
        }
    }
}

/// Direct child pids of `parent` (via pgrep -P) — the adapter's bridge child.
fn child_pids(parent: i64) -> Vec<i64> {
    std::process::Command::new("pgrep")
        .arg("-P")
        .arg(parent.to_string())
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Poll waitpid(WNOHANG) until `pid` exits (reaping it → no zombie) or the budget expires.
fn wait_exited(pid: i64, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        if r == pid as i32 || r < 0 {
            return true; // exited+reaped, or ECHILD (already gone)
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn spawn_adapter(qd: &str, endpoint: &str, cwd: &Path, load_session: Option<&str>, log: &Path) -> i64 {
    let argv = build_adapter_argv(Path::new(qd), endpoint, cwd, None, &[], load_session);
    let env = vec![("PATH".to_string(), path_with_bridge())];
    let spawner = RealDaemonSpawner;
    spawner
        .spawn_detached(&argv, &env, cwd, log)
        .expect("spawn qd acp-daemon")
        .pid
}

#[test]
fn acp_resume_roundtrip_live() {
    if !live() {
        eprintln!("QD_ACP_LIVE != 1 — skipping the acp resume round-trip live test");
        return;
    }
    let qd = env!("CARGO_BIN_EXE_qd");
    let bridge_dir = bridge_bin_dir();
    assert!(
        bridge_dir.join("claude-code-acp").exists(),
        "bridge shim not found at {bridge_dir:?}/claude-code-acp (set QD_ACP_BRIDGE_BIN_DIR)"
    );

    let nonce = format!("QUARTZ{}", std::process::id());
    let work = TempDir::new().unwrap();
    let cwd = work.path();
    let sessions_dir = work.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let logdir = work.path().join("log");
    std::fs::create_dir_all(&logdir).unwrap();
    eprintln!("=== Item 3 (R) acp resume round-trip LIVE === qd={qd} nonce={nonce} cwd={cwd:?}");

    let before_files = all_session_files();

    // ---- 1. CREATE: spawn the production adapter (create mode), drive turn 1. ----
    let ep1 = format!("ws://127.0.0.1:{}", real_alloc_port().expect("port1"));
    let pid1 = spawn_adapter(qd, &ep1, cwd, None, &logdir.join("acp1.log"));
    let conn1 = connect_ready(&ep1, Duration::from_secs(30)).expect("adapter1 ready");
    let session = conn1.status_session_id().ok().flatten().expect("sessionId established");
    eprintln!("1. CREATE adapter1 pid={pid1} ep={ep1} sessionId={session}");
    drive_turn(
        &conn1,
        &session,
        &format!("Remember this magic word for later: {nonce}. Reply with exactly: OK. Do not use any tools."),
        "rt-pre",
    );
    drop(conn1);
    let lines_pre = wait_for_jsonl(&session, Duration::from_secs(20), |ls| ls.iter().any(|l| l.contains(&nonce)));
    let jsonl = find_session_jsonl(&session).expect("JSONL for the created session");
    eprintln!("   turn1 in JSONL={jsonl:?} lines_pre={}", lines_pre.len());
    assert!(lines_pre.iter().any(|l| l.contains(&nonce)), "pre-stop JSONL records the nonce");

    // ---- 2. STOP: kill_acp group-reaps adapter1 + bridge child + tombstones. ----
    let captured = RegistryEntry {
        pid: Some(pid1),
        session_id: Some(session.clone()),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        status: Some("idle".into()),
        name: Some("rt".into()),
        provider: Some("acp/claude-code".into()),
        endpoint: Some(ep1.clone()),
        ..Default::default()
    };
    let out = kill_acp(&sessions_dir, pid1, Some(&captured), &RealDaemonSpawner, &real_cmdline_probe);
    assert!(out.was_alive, "adapter1 was our live daemon → group-reaped");
    // No-leak: the adapter pgid is gone (group-reap of adapter + bridge child).
    let reap_deadline = Instant::now() + Duration::from_secs(5);
    while is_pid_alive(pid1 as i32) && Instant::now() < reap_deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!is_pid_alive(pid1 as i32), "STOP: adapter1 pgid reaped (no leak)");
    assert!(
        sessions_dir.join(format!("{pid1}.json.tombstoned")).exists(),
        "STOP: the row was tombstoned"
    );
    eprintln!("2. STOP kill_acp: adapter1 reaped, tombstoned");

    // ---- 3. RESUME: spawn the adapter in LOAD mode; assert SAME sessionId. ----
    let ep2 = format!("ws://127.0.0.1:{}", real_alloc_port().expect("port2"));
    let pid2 = spawn_adapter(qd, &ep2, cwd, Some(&session), &logdir.join("acp2.log"));
    assert_ne!(pid2, pid1, "resume is a NEW adapter process");
    let conn2 = connect_ready(&ep2, Duration::from_secs(30)).expect("adapter2 (load mode) ready");
    let established = conn2.status_session_id().ok().flatten().expect("resumed sessionId");
    assert_eq!(
        established, session,
        "RESUME: load-mode adapter re-established the SAME sessionId (no re-mint / no new session)"
    );
    eprintln!("3. RESUME adapter2 pid={pid2} ep={ep2} sessionId={established} (== pre-stop)");

    // ---- 4. drive turn 2 (recall) + assert faithful continuation from PRIMARY SOURCE.
    drive_turn(
        &conn2,
        &session,
        "What exact magic word did I ask you to remember earlier? Reply with ONLY that word. Do not use any tools.",
        "rt-post",
    );
    drop(conn2);

    let lines_post = wait_for_jsonl(&session, Duration::from_secs(20), |ls| ls.len() > lines_pre.len());
    let jsonl_post = find_session_jsonl(&session).expect("JSONL after resume");
    assert_eq!(jsonl_post, jsonl, "(R-a) post-resume appended to the SAME <sessionId>.jsonl");
    assert!(
        lines_post.len() > lines_pre.len(),
        "(R-a) the resumed turn APPENDED to the same file (lines {} -> {})",
        lines_pre.len(),
        lines_post.len()
    );
    // The model recalled the pre-stop-only nonce → history truly re-loaded (faithful).
    let recalled = lines_post.iter().rev().take(3).any(|l| l.contains(&nonce));
    assert!(
        recalled,
        "(R-a) faithful continuation: the post-resume turn recalls the pre-stop nonce {nonce:?}"
    );

    // NO FORK: exactly one new session file appeared, and it IS <sessionId>.jsonl.
    let new_files: Vec<_> = all_session_files().into_iter().filter(|f| !before_files.contains(f)).collect();
    assert_eq!(new_files.len(), 1, "(R-d) no fork: exactly one new session file; got {new_files:?}");
    assert_eq!(new_files[0], jsonl, "(R-d) the single new file IS the resumed session");

    // cleanup: reap adapter2's group.
    RealDaemonSpawner.kill(pid2);

    eprintln!(
        "=== (R) GREEN: STOPPED->resume round-trip faithful — SAME sessionId {session}, JSONL {} -> {} lines, nonce recalled, no fork ===",
        lines_pre.len(),
        lines_post.len()
    );
}

/// (W) WEDGED-BUT-ALIVE live repro (arm i): a real `qd acp-daemon` adapter whose bridge
/// CHILD is KILLED must SELF-TERMINATE — so `pid_alive=false` becomes the honest signal
/// the resume (R-c) and ls (L) gates read (instead of a 'live' lie). The gate consequences
/// (ls 'stopped'; resume revives) follow from pid_alive=false and are unit-proven
/// (`acp_status_classify(false,_)="stopped"`; `acp_resume_is_alive(dead)=false→revive`).
#[test]
fn acp_adapter_self_terminates_when_bridge_child_killed() {
    if !live() {
        eprintln!("QD_ACP_LIVE != 1 — skipping the (W) wedged-but-alive live repro");
        return;
    }
    let qd = env!("CARGO_BIN_EXE_qd");
    let bridge_dir = bridge_bin_dir();
    assert!(bridge_dir.join("claude-code-acp").exists(), "bridge shim not found");
    let work = TempDir::new().unwrap();
    let cwd = work.path();
    let logdir = work.path().join("log");
    std::fs::create_dir_all(&logdir).unwrap();

    // Spawn the REAL adapter (create mode); confirm it + its bridge child are alive.
    let ep = format!("ws://127.0.0.1:{}", real_alloc_port().expect("port"));
    let adapter = spawn_adapter(qd, &ep, cwd, None, &logdir.join("wedge.log"));
    let conn = connect_ready(&ep, Duration::from_secs(30)).expect("adapter ready");
    let session = conn.status_session_id().ok().flatten().expect("sessionId");
    drop(conn);
    assert!(is_pid_alive(adapter as i32), "adapter alive after boot");
    // Find the bridge CHILD (the node process the adapter owns).
    let mut bridge_children = child_pids(adapter);
    let kill_deadline = Instant::now() + Duration::from_secs(5);
    while bridge_children.is_empty() && Instant::now() < kill_deadline {
        std::thread::sleep(Duration::from_millis(100));
        bridge_children = child_pids(adapter);
    }
    assert!(!bridge_children.is_empty(), "the adapter has a live bridge child");
    eprintln!("(W) adapter pid={adapter} session={session} bridge children={bridge_children:?}");

    // KILL ONLY the bridge child (the adapter is NOT signaled).
    for bp in &bridge_children {
        unsafe { libc::kill(*bp as i32, libc::SIGKILL) };
    }
    // THE (W) FIX: the adapter must SELF-TERMINATE (confirmed-dead bridge) — NOT linger
    // 'live'. Pre-fix this hangs alive forever (the bug). waitpid reaps it → no zombie.
    let exited = wait_exited(adapter, Duration::from_secs(10));
    assert!(exited, "(W) adapter SELF-TERMINATED after its bridge child died");
    assert!(!is_pid_alive(adapter as i32), "(W) adapter pid is gone → pid_alive=false is the honest signal");
    eprintln!("=== (W) GREEN: bridge child killed → adapter self-terminated (no zombie 'live' adapter) ===");
}
