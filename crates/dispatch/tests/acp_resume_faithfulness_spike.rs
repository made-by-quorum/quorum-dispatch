//! Component-0 — BUILD-ENTRY FAITHFULNESS SPIKE (Item 3 ACP follow-ons).
//!
//! The entire item-3 resume meat rests on ONE unproven primitive:
//! [`AcpHost::load_session`] (`provider/acp/client.rs`, issues real `session/load`)
//! has ZERO production callers — asserted-faithful but only pillar-2-HARNESS-proven,
//! NEVER proven on the real `@zed-industries/claude-code-acp` bridge cross-process.
//!
//! This spike PROVES BY PRIMARY SOURCE — the bridge's OWN CC JSONL keyed by the
//! sessionId (`~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`) — that a real
//! `session/load` on a FRESH bridge re-loads the SAME conversation and a post-load
//! turn CONTINUES it (NOT a new sessionId, NOT a fresh/empty JSONL, NOT a fork).
//!
//! Three independent faithfulness axes, all asserted:
//!   (A) SAME FILE — the post-load turn appends to the SAME `<sessionId>.jsonl`
//!       (line count grows; path unchanged; the pre-load nonce still present).
//!   (B) NO FORK — no NEW `<otherId>.jsonl` session file appears for that cwd.
//!   (C) SEMANTIC CONTINUITY — after load on a fresh bridge, the model RECALLS a
//!       nonce told ONLY in the pre-stop turn (history was truly loaded into the
//!       model's context, not merely file-appended).
//!
//! Gated on `QD_ACP_LIVE=1` (real bridge + real CC creds + model budget). A no-op
//! otherwise, so the default suite never spends budget. Run:
//!   QD_ACP_LIVE=1 ~/cap-cargo.sh test -p dispatch --test acp_resume_faithfulness_spike -- --nocapture --include-ignored

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dispatch::provider::acp::{AcpClient, AcpEvent, AcpHost, StopReason};
use tempfile::TempDir;

fn live() -> bool {
    std::env::var("QD_ACP_LIVE").as_deref() == Ok("1")
}

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

fn real_bridge_script() -> PathBuf {
    if let Ok(p) = std::env::var("QD_ACP_BRIDGE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home)
        .join("work/acp-step0/node_modules/@zed-industries/claude-code-acp/dist/index.js")
}

/// `~/.claude/projects` — the CC JSONL home (the bridge's primary store).
fn cc_projects_dir() -> PathBuf {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").expect("HOME")).join(".claude"));
    base.join("projects")
}

/// Locate `<sessionId>.jsonl` anywhere under `~/.claude/projects/*/`. sessionId is a
/// UUID → globally unique, so a scan is unambiguous (avoids replicating the bridge's
/// `encodeProjectPath`). Returns the absolute path if present.
fn find_session_jsonl(session_id: &str) -> Option<PathBuf> {
    let want = format!("{session_id}.jsonl");
    let projects = cc_projects_dir();
    let dirs = std::fs::read_dir(&projects).ok()?;
    for d in dirs.flatten() {
        let p = d.path();
        if p.is_dir() {
            let cand = p.join(&want);
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    None
}

/// Count every `<id>.jsonl` (non `agent-*`) session file across all project dirs —
/// the fork detector: a faithful resume MUST NOT mint a new session file.
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

fn jsonl_lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// The bridge's CC-SDK appends the user+assistant JSONL lines a beat AFTER the wire
/// `stopReason` terminal arrives — the primary store is eventually-consistent vs the
/// wire. Poll `<sessionId>.jsonl` until `pred(lines)` holds or the budget expires;
/// return the settled lines. (A genuine race fix, not a faithfulness compromise: we
/// still assert against the bridge's OWN file, just after it has flushed.)
fn wait_for_jsonl(
    session_id: &str,
    budget: Duration,
    pred: impl Fn(&[String]) -> bool,
) -> Vec<String> {
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

/// Pull updates until a terminal, with an overall budget.
fn drive_to_terminal(host: &AcpHost, budget: Duration) -> (Vec<AcpEvent>, AcpEvent) {
    let mut updates = Vec::new();
    let deadline = Instant::now() + budget;
    loop {
        assert!(Instant::now() < deadline, "no terminal within budget");
        match host.next_update(Duration::from_millis(500)) {
            Ok(Some(ev @ AcpEvent::Update { .. })) => updates.push(ev),
            Ok(Some(term @ AcpEvent::Terminal { .. })) => return (updates, term),
            Ok(Some(term @ AcpEvent::TerminalError { .. })) => return (updates, term),
            Ok(None) => continue,
            Err(e) => panic!("transport error before terminal: {e}"),
        }
    }
}

struct EvidenceLog {
    path: PathBuf,
    buf: String,
}
impl EvidenceLog {
    fn open() -> Self {
        let path = std::env::var("QD_ACP_SPIKE_EVIDENCE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join("work/acp-cc-coord/acp-resume-faithfulness-spike.log")
            });
        EvidenceLog { path, buf: String::new() }
    }
    fn line(&mut self, s: &str) {
        eprintln!("{s}");
        self.buf.push_str(s);
        self.buf.push('\n');
    }
    fn flush_to_disk(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::File::create(&self.path) {
            let _ = f.write_all(self.buf.as_bytes());
            eprintln!("[spike evidence written to {:?}]", self.path);
        }
    }
}

/// COMPONENT-0: prove `session/load` faithfully re-loads the SAME CC session on the
/// REAL bridge, cross-(bridge)-process, by the bridge's OWN CC JSONL.
#[test]
fn acp_resume_faithfulness_spike() {
    if !live() {
        eprintln!("QD_ACP_LIVE != 1 — skipping Component-0 faithfulness spike");
        return;
    }
    let node = node_program().expect("node required for the live bridge");
    let bridge = real_bridge_script();
    assert!(bridge.exists(), "bridge script not found at {bridge:?} (set QD_ACP_BRIDGE)");
    let bridge_arg = bridge.to_string_lossy().to_string();

    let mut log = EvidenceLog::open();
    log.line("=== Component-0: ACP resume faithfulness spike (session/load, real bridge) ===");
    log.line(&format!("bridge={bridge:?} node={node}"));

    // A unique nonce told ONLY in the pre-stop turn — the semantic-continuity probe.
    let nonce = format!("ZEPHYR{}", std::process::id());
    log.line(&format!("nonce(told pre-stop only)={nonce}"));

    let work = TempDir::new().unwrap();
    let cwd = work.path().to_string_lossy().to_string();
    log.line(&format!("cwd={cwd}"));

    let before_files = all_session_files();

    // ---- PHASE 1: create + drive a real session so it has >=1 real turn. ----
    let host1 = AcpHost::spawn(&node, &[bridge_arg.clone()], work.path()).expect("spawn bridge 1");
    host1.initialize().expect("initialize bridge 1");
    let session = host1.new_session(&cwd).expect("session/new");
    log.line(&format!("PHASE1 session/new -> sessionId={session}"));

    let turn1 = host1
        .prompt(
            &session,
            &format!("Remember this magic word for later: {nonce}. Reply with exactly: OK. Do not use any tools."),
            "spike-pre",
        )
        .expect("prompt 1");
    let (_u1, term1) = drive_to_terminal(&host1, Duration::from_secs(120));
    match &term1 {
        AcpEvent::Terminal { turn, stop, .. } => {
            assert_eq!(*turn, turn1, "pre-stop terminal correlates");
            assert_eq!(*stop, StopReason::EndTurn, "clean pre-stop turn end_turn");
        }
        other => panic!("expected pre-stop Terminal, got {other:?}"),
    }
    log.line(&format!("PHASE1 pre-stop turn done; assistant_text={:?}", host1.assistant_text()));

    // PRIMARY SOURCE before stop: the bridge's own CC JSONL keyed by sessionId.
    // Wait for the SDK to flush the user turn (the nonce) — eventual-consistency vs wire.
    let lines_pre = wait_for_jsonl(&session, Duration::from_secs(15), |ls| {
        ls.iter().any(|l| l.contains(&nonce))
    });
    let jsonl = find_session_jsonl(&session)
        .unwrap_or_else(|| panic!("no JSONL for sessionId={session} under {:?}", cc_projects_dir()));
    let nonce_in_pre = lines_pre.iter().filter(|l| l.contains(&nonce)).count();
    log.line(&format!(
        "PHASE1 JSONL={jsonl:?}\n  lines_pre={} nonce_hits_pre={nonce_in_pre}",
        lines_pre.len()
    ));
    assert!(!lines_pre.is_empty(), "pre-stop JSONL non-empty");
    assert!(nonce_in_pre >= 1, "pre-stop JSONL records the nonce (the user turn)");

    // ---- PHASE 2: STOP — kill bridge 1 (cross-process boundary). ----
    host1.shutdown();
    drop(host1);
    log.line("PHASE2 bridge 1 shut down (cross-process: a FRESH bridge will load)");

    // ---- PHASE 3: a FRESH bridge re-loads via real session/load. ----
    let host2 = AcpHost::spawn(&node, &[bridge_arg.clone()], work.path()).expect("spawn bridge 2");
    host2.initialize().expect("initialize bridge 2");
    host2.load_session(&session, &cwd).expect("session/load on a fresh bridge");
    assert_eq!(
        host2.session_id().as_deref(),
        Some(session.as_str()),
        "load_session re-established the SAME sessionId (no re-mint)"
    );
    log.line(&format!("PHASE3 session/load re-established sessionId={session} on a fresh bridge"));

    // ---- PHASE 4: a post-load turn that depends on prior context. ----
    let turn2 = host2
        .prompt(
            &session,
            "What exact magic word did I ask you to remember earlier? Reply with ONLY that word, nothing else. Do not use any tools.",
            "spike-post",
        )
        .expect("prompt 2 (post-load)");
    let (_u2, term2) = drive_to_terminal(&host2, Duration::from_secs(120));
    match &term2 {
        AcpEvent::Terminal { turn, stop, .. } => {
            assert_eq!(*turn, turn2, "post-load terminal correlates");
            assert_eq!(*stop, StopReason::EndTurn, "clean post-load turn end_turn");
        }
        other => panic!("expected post-load Terminal, got {other:?}"),
    }
    let recalled = host2.assistant_text();
    log.line(&format!("PHASE4 post-load assistant_text={recalled:?}"));

    // ---- ASSERT the three faithfulness axes from PRIMARY SOURCE. ----
    // (C) SEMANTIC CONTINUITY: the model recalled the pre-stop-only nonce.
    assert!(
        recalled.contains(&nonce),
        "FAITHFULNESS (C): post-load reply must recall the pre-stop nonce {nonce:?}; got {recalled:?}"
    );

    // (A) SAME FILE: the SAME <sessionId>.jsonl grew and still holds the nonce.
    // Wait for the post-load user+assistant turn to flush (line count grows).
    let lines_post = wait_for_jsonl(&session, Duration::from_secs(15), |ls| {
        ls.len() > lines_pre.len()
    });
    let jsonl_post = find_session_jsonl(&session)
        .unwrap_or_else(|| panic!("post-load: JSONL for sessionId={session} vanished"));
    assert_eq!(jsonl_post, jsonl, "FAITHFULNESS (A): post-load appended to the SAME JSONL path");
    let nonce_in_post = lines_post.iter().filter(|l| l.contains(&nonce)).count();
    log.line(&format!(
        "POST JSONL={jsonl_post:?}\n  lines_post={} nonce_hits_post={nonce_in_post}",
        lines_post.len()
    ));
    assert!(
        lines_post.len() > lines_pre.len(),
        "FAITHFULNESS (A): post-load turn APPENDED to the same file (lines {} -> {})",
        lines_pre.len(),
        lines_post.len()
    );
    assert!(
        nonce_in_post >= nonce_in_pre,
        "FAITHFULNESS (A): the pre-stop history is STILL present after continuation"
    );

    // (B) NO FORK: no NEW session file appeared beyond the one <sessionId>.jsonl.
    let after_files = all_session_files();
    let new_files: Vec<_> = after_files
        .iter()
        .filter(|f| !before_files.contains(f))
        .cloned()
        .collect();
    log.line(&format!("NEW session files since start: {new_files:?}"));
    assert_eq!(
        new_files.len(),
        1,
        "FAITHFULNESS (B): exactly ONE new session file (the resumed one), no fork; got {new_files:?}"
    );
    assert_eq!(
        new_files[0], jsonl,
        "FAITHFULNESS (B): the single new file IS the resumed <sessionId>.jsonl (no forked session)"
    );

    host2.shutdown();

    log.line("=== COMPONENT-0 GREEN: session/load faithfully CONTINUES the SAME CC session ===");
    log.line(&format!(
        "  axis A (same file, appended): lines {}->{}  | axis B (no fork): 1 new file == resumed | axis C (semantic recall): YES",
        lines_pre.len(),
        lines_post.len()
    ));
    log.flush_to_disk();
}
