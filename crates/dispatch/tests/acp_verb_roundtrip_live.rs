//! Item 3 (red-team round-2) — the VERB-LEVEL user-path round-trip, BY NAME, through the
//! REAL `qd` verbs. Every prior FM-R1 proof drove `kill_acp` + the resume adapter DIRECTLY,
//! BYPASSING the `qd resume <name>` resolve — and that resolve was where the join-shadow bug
//! lived. This test drives the SHIPPED verbs end-to-end:
//!
//!   qd start --provider acp/claude-code  →  qd send:relay <name> / qd wait <name>  (turn 1)
//!   →  qd stop <name>  (run_acp_kill, proper tombstone)
//!   →  qd resume <name>  [BY NAME — must resolve + route to run_acp_resume, not claude]
//!   →  qd send:relay <name> / qd wait <name>  (turn 2)
//!   →  assert the bridge's CC JSONL CONTINUED (SAME sessionId, both turns, nonce recalled).
//!
//! Jailed HOME (a tempdir with the real CC creds copied in) so the dispatch registry +
//! bridge JSONL stay inside the jail — NO real-registry pollution. Gated `QD_ACP_LIVE=1`
//! (real bridge + ~2 model turns). Run:
//!   QD_ACP_LIVE=1 ~/cap-cargo.sh test -p quorum-dispatch --test acp_verb_roundtrip_live -- --nocapture --test-threads=1

use std::path::{Path, PathBuf};
use std::process::Command;

fn live() -> bool {
    std::env::var("QD_ACP_LIVE").as_deref() == Ok("1")
}

fn bridge_bin_dir() -> PathBuf {
    std::env::var("QD_ACP_BRIDGE_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").expect("HOME")).join("work/acp-step0/node_modules/.bin")
        })
}

/// Set up a jail HOME with the real CC creds copied in (so the bridge can auth while the
/// dispatch registry + bridge JSONL live entirely inside the jail). Returns None if the
/// real creds are absent (cannot run the live lane).
fn setup_jail(jail: &Path) -> Option<()> {
    std::fs::create_dir_all(jail.join(".claude")).ok()?;
    std::fs::create_dir_all(jail.join("work")).ok()?;
    let real_home = PathBuf::from(std::env::var("HOME").ok()?);
    let creds = real_home.join(".claude/.credentials.json");
    if !creds.exists() {
        return None;
    }
    std::fs::copy(&creds, jail.join(".claude/.credentials.json")).ok()?;
    // .claude.json (main config) — best-effort.
    let cfg = real_home.join(".claude.json");
    if cfg.exists() {
        let _ = std::fs::copy(&cfg, jail.join(".claude.json"));
    }
    Some(())
}

fn run_qd(qd: &str, jail: &Path, args: &[&str]) -> std::process::Output {
    let bridge = bridge_bin_dir();
    let path = format!(
        "{}:{}",
        bridge.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(qd)
        .args(args)
        .env("HOME", jail)
        .env("PATH", path)
        .output()
        .expect("run qd")
}

fn out_str(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Find the single session `<sid>.jsonl` under the jail's CC projects dir (non agent-).
fn find_jail_jsonl(jail: &Path) -> Option<PathBuf> {
    let projects = jail.join(".claude/projects");
    for d in std::fs::read_dir(&projects).ok()?.flatten() {
        if d.path().is_dir() {
            for f in std::fs::read_dir(d.path()).ok()?.flatten() {
                let p = f.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.ends_with(".jsonl") && !name.starts_with("agent-") {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn jsonl_lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// The bridge's CC-SDK appends the JSONL a beat AFTER the wire `wait`/`done` returns
/// (eventual-consistency vs the wire). Poll the jail JSONL until `pred(lines)` holds or
/// the budget expires; return the settled lines.
fn wait_for_jail_jsonl(
    jail: &Path,
    budget: std::time::Duration,
    pred: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    let deadline = std::time::Instant::now() + budget;
    let mut last = Vec::new();
    loop {
        if let Some(p) = find_jail_jsonl(jail) {
            last = jsonl_lines(&p);
            if pred(&last) {
                return last;
            }
        }
        if std::time::Instant::now() >= deadline {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[test]
fn acp_verb_roundtrip_by_name_live() {
    if !live() {
        eprintln!("QD_ACP_LIVE != 1 — skipping the acp verb-level by-name round-trip");
        return;
    }
    let qd = env!("CARGO_BIN_EXE_qd");
    let bridge = bridge_bin_dir();
    assert!(bridge.join("claude-code-acp").exists(), "bridge shim not found at {bridge:?}");

    let tmp = tempfile::tempdir().unwrap();
    let jail = tmp.path();
    if setup_jail(jail).is_none() {
        eprintln!("real CC creds absent — skipping (cannot auth the bridge)");
        return;
    }
    let nonce = format!("MARBLE{}", std::process::id());
    let work = jail.join("work");
    let works = work.to_string_lossy().into_owned();
    let name = "acpwk";

    // 1. CREATE a proper DISPATCH acp row.
    let o = run_qd(qd, jail, &["start", name, "--provider", "acp/claude-code", "--cwd", &works]);
    assert!(out_str(&o).contains("Started detached acp session"), "start: {}", out_str(&o));

    // 2. turn 1 (tell the nonce) via the real send:relay + wait verbs.
    let send1 = format!("Remember this word: {nonce}. Reply with exactly: OK");
    let o = run_qd(qd, jail, &["send:relay", name, &send1]);
    assert!(out_str(&o).contains("#t"), "send:relay turn1: {}", out_str(&o));
    let o = run_qd(qd, jail, &["wait", name]);
    assert!(out_str(&o).contains("done"), "wait turn1: {}", out_str(&o));

    // The bridge writes the CC JSONL a beat after `wait` returns — poll until the nonce
    // (the turn-1 user message) is flushed to disk.
    let lines_pre = wait_for_jail_jsonl(jail, std::time::Duration::from_secs(20), |ls| {
        ls.iter().any(|l| l.contains(&nonce))
    });
    let jsonl = find_jail_jsonl(jail).expect("turn1 wrote a bridge CC JSONL");
    let sid = jsonl.file_stem().unwrap().to_string_lossy().into_owned();
    assert!(
        lines_pre.iter().any(|l| l.contains(&nonce)),
        "pre-stop JSONL records the nonce"
    );
    eprintln!("CREATE+turn1: sessionId={sid} JSONL lines={}", lines_pre.len());

    // 3. STOP by name → run_acp_kill (proper acp tombstone; 'zmx -' is its normal format).
    let o = run_qd(qd, jail, &["stop", name]);
    assert!(out_str(&o).contains("killed"), "stop: {}", out_str(&o));

    // 4. ★ RESUME BY NAME — the bug surface. MUST resolve by name AND route to
    //    run_acp_resume (the acp session/load path), NOT the claude resume path.
    let o = run_qd(qd, jail, &["resume", name]);
    let resume_out = out_str(&o);
    assert!(
        resume_out.contains("resumed acp session"),
        "(verb) resume <name> must resolve + route to run_acp_resume (acp), got: {resume_out}"
    );
    assert!(
        !resume_out.to_lowercase().contains("claude-") && !resume_out.contains("No session matching"),
        "(verb) resume must NOT misroute to the claude path nor fail name-resolve: {resume_out}"
    );
    eprintln!("RESUME by name: {}", resume_out.trim());

    // 5. turn 2 (recall) — proves faithful continuation post-resume.
    let o = run_qd(qd, jail, &["send:relay", name, "What word did I ask you to remember? Reply with only that word."]);
    assert!(out_str(&o).contains("#t"), "send:relay turn2: {}", out_str(&o));
    let o = run_qd(qd, jail, &["wait", name]);
    assert!(out_str(&o).contains("done"), "wait turn2: {}", out_str(&o));

    // 6. assert the SAME bridge JSONL CONTINUED (same sessionId, grew, nonce recalled).
    let lines_post = wait_for_jail_jsonl(jail, std::time::Duration::from_secs(20), |ls| {
        ls.len() > lines_pre.len()
    });
    let jsonl_post = find_jail_jsonl(jail).expect("JSONL still present");
    assert_eq!(
        jsonl_post.file_stem().unwrap().to_string_lossy(),
        sid,
        "(R-a) post-resume turn continued the SAME sessionId file (no fork)"
    );
    assert!(
        lines_post.len() > lines_pre.len(),
        "(R-a) the post-resume turn appended to the SAME file ({} -> {})",
        lines_pre.len(),
        lines_post.len()
    );
    let recalled = lines_post.iter().rev().take(3).any(|l| l.contains(&nonce));
    assert!(recalled, "(R-a) the post-resume turn RECALLED the pre-stop nonce {nonce} (faithful)");

    // cleanup: stop the resumed adapter (the jail tempdir is auto-removed).
    let _ = run_qd(qd, jail, &["stop", name]);
    eprintln!(
        "=== VERB-LEVEL GREEN: qd start->send->wait->stop->resume <name>->send->wait round-trips faithfully via run_acp_resume (SAME sessionId {sid}, JSONL {} -> {}, nonce recalled) ===",
        lines_pre.len(),
        lines_post.len()
    );
}
