//! ACK-1 fakerepl seam rows (ack1-spec §6 R-FR1..R-FR4) — process-level,
//! reproducible-by-command: the binary is driven over plain pipes (raw-mode
//! setup no-ops on a non-tty; pipe writes separated by > GAP_MS form distinct
//! bursts) inside a synthetic jail-shaped env (the belt checks env SHAPE, not
//! dir contents — fakerepl/src/jail.rs).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Build a jail-shaped env rooted in a tempdir: HOME=*/sbrg-runs/<x>/home,
/// QD_HOME/ZMX_DIR/TMPDIR siblings under the same root.
fn jail_env(root: &Path) -> Vec<(String, String)> {
    let run_root = root.join("sbrg-runs").join("ack1seams");
    for sub in ["home", "sb_home", "zmx", "tmp"] {
        std::fs::create_dir_all(run_root.join(sub)).unwrap();
    }
    vec![
        (
            "HOME".into(),
            run_root.join("home").to_string_lossy().into_owned(),
        ),
        (
            "QD_HOME".into(),
            run_root.join("sb_home").to_string_lossy().into_owned(),
        ),
        (
            "ZMX_DIR".into(),
            run_root.join("zmx").to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".into(),
            run_root.join("tmp").to_string_lossy().into_owned(),
        ),
        ("PATH".into(), "/usr/bin:/bin".into()),
    ]
}

/// Spawn fakerepl with the given extra env; stdin piped, stderr piped.
fn spawn_fakerepl(env: &[(String, String)], extra: &[(&str, String)]) -> std::io::Result<Child> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fakerepl"));
    cmd.env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.spawn()
}

/// Write `bytes`, wait past the burst gap so the next write is its own burst.
fn write_burst(stdin: &mut impl Write, bytes: &[u8]) {
    stdin.write_all(bytes).unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_millis(80)); // > GAP_MS (50)
}

/// Parse the report JSONL into serde_json values.
fn read_report(path: &Path) -> Vec<serde_json::Value> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

struct SeamRun {
    report: PathBuf,
    convo: PathBuf,
    child: Child,
}

/// Common run scaffold: tempdir jail env + report + convo paths.
fn start(extra: &[(&str, String)], tmp: &Path) -> SeamRun {
    let env = jail_env(tmp);
    let report = tmp.join("report.jsonl");
    let convo = tmp.join("convo.jsonl");
    let mut all: Vec<(&str, String)> = vec![
        ("QD_FAKEREPL_REPORT", report.to_string_lossy().into_owned()),
        (
            "QD_FAKEREPL_CONVO_JSONL",
            convo.to_string_lossy().into_owned(),
        ),
        ("QD_FAKEREPL_BUSY_MS", "50".to_string()),
    ];
    all.extend(extra.iter().cloned());
    let child = spawn_fakerepl(&env, &all).expect("spawn fakerepl");
    // Wait for READINESS (the registry row, written at Repl::start) before any
    // stdin write: pipe bytes written pre-boot coalesce into one burst (spawn
    // latency would glue content + CR together and break burst-split rows).
    let home = env
        .iter()
        .find(|(k, _)| k == "HOME")
        .map(|(_, v)| PathBuf::from(v))
        .unwrap();
    let row = home
        .join(".claude")
        .join("sessions")
        .join(format!("{}.json", child.id()));
    let start_t = std::time::Instant::now();
    while !row.exists() && start_t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        row.exists(),
        "fakerepl did not become ready (no registry row)"
    );
    SeamRun {
        report,
        convo,
        child,
    }
}

/// R-FR1: EAT_INPUT consumes bytes (per-burst `eaten` events sum to the sent
/// total), produces NO turn, NO user record.
#[test]
fn r_fr1_eat_input() {
    let tmp = tempfile::tempdir().unwrap();
    let mut run = start(&[("QD_FAKEREPL_EAT_INPUT", "1".into())], tmp.path());
    let mut stdin = run.child.stdin.take().unwrap();
    write_burst(&mut stdin, b"hello eaten input");
    write_burst(&mut stdin, b"\r");
    run.child.stdin = None;
    drop(stdin);
    let _ = run.child.wait();
    let report = read_report(&run.report);
    let convo = std::fs::read_to_string(&run.convo).unwrap_or_default();

    let eaten_total: u64 = report
        .iter()
        .filter(|v| v["event"] == "eaten")
        .map(|v| v["bytes"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(
        eaten_total,
        ("hello eaten input".len() + 1) as u64,
        "eaten events must account for every sent byte: {report:?}"
    );
    assert!(
        !report.iter().any(|v| v["event"] == "turn"),
        "no turn may start under EAT_INPUT: {report:?}"
    );
    assert!(
        !report.iter().any(|v| v["event"] == "burst"),
        "no burst classification under EAT_INPUT: {report:?}"
    );
    assert!(
        convo.is_empty(),
        "no user record may be written under EAT_INPUT: {convo}"
    );
}

/// R-FR2: TRUNCATE cuts the user record to the exact byte prefix (ASCII:
/// requested == actual); the turn itself proceeds with the FULL byte count.
#[test]
fn r_fr2_truncate_user_record() {
    let tmp = tempfile::tempdir().unwrap();
    let mut run = start(
        &[("QD_FAKEREPL_TRUNCATE_USER_RECORD_BYTES", "8".into())],
        tmp.path(),
    );
    let mut stdin = run.child.stdin.take().unwrap();
    // 21 ASCII bytes, then a lone CR (own burst, non-paste → submit).
    write_burst(&mut stdin, b"twenty-one bytes here");
    write_burst(&mut stdin, b"\r");
    std::thread::sleep(Duration::from_millis(200)); // let the busy window end
    drop(stdin);
    let _ = run.child.wait();
    let report = read_report(&run.report);
    let convo = std::fs::read_to_string(&run.convo).unwrap_or_default();

    let trunc = report
        .iter()
        .find(|v| v["event"] == "truncated_user_record")
        .unwrap_or_else(|| panic!("missing truncated_user_record event: {report:?}"));
    assert_eq!(trunc["requested"], 8);
    assert_eq!(trunc["actual"], 8, "ASCII cut is byte-exact");

    let user = convo
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "user")
        .unwrap_or_else(|| panic!("missing user record: {convo}"));
    assert_eq!(
        user["message"]["content"], "twenty-o",
        "user record must be the exact 8-byte prefix"
    );
    // The turn itself ran on the FULL composer (app contract unchanged).
    let turn = report
        .iter()
        .find(|v| v["event"] == "turn")
        .expect("turn event");
    assert_eq!(turn["bytes"], 21, "turn bytes= stays the full composer");
}

/// R-FR4: a cut landing mid-multibyte-codepoint rounds DOWN to the previous
/// UTF-8 boundary — no panic, no U+FFFD in the kept prefix; the report shows
/// actual < requested.
#[test]
fn r_fr4_truncate_rounds_down_at_utf8_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let mut run = start(
        &[("QD_FAKEREPL_TRUNCATE_USER_RECORD_BYTES", "3".into())],
        tmp.path(),
    );
    let mut stdin = run.child.stdin.take().unwrap();
    // "ab€cd": € is 3 bytes at offsets 2..5 — a cut at 3 lands mid-codepoint.
    write_burst(&mut stdin, "ab\u{20AC}cd".as_bytes());
    write_burst(&mut stdin, b"\r");
    std::thread::sleep(Duration::from_millis(200));
    drop(stdin);
    let _ = run.child.wait();
    let report = read_report(&run.report);
    let convo = std::fs::read_to_string(&run.convo).unwrap_or_default();

    let trunc = report
        .iter()
        .find(|v| v["event"] == "truncated_user_record")
        .unwrap_or_else(|| panic!("missing truncated_user_record event: {report:?}"));
    assert_eq!(trunc["requested"], 3);
    assert_eq!(trunc["actual"], 2, "mid-codepoint cut rounds down to 2");

    let user = convo
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "user")
        .expect("user record");
    assert_eq!(
        user["message"]["content"], "ab",
        "no U+FFFD in the kept prefix"
    );
}

/// R-FR3: EAT_INPUT + TRUNCATE both set → startup refusal, exit 13, stderr
/// names the conflict (fail-loud, no silent precedence).
#[test]
fn r_fr3_conflicting_seams_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let env = jail_env(tmp.path());
    let child = spawn_fakerepl(
        &env,
        &[
            ("QD_FAKEREPL_EAT_INPUT", "1".to_string()),
            ("QD_FAKEREPL_TRUNCATE_USER_RECORD_BYTES", "8".to_string()),
        ],
    )
    .expect("spawn fakerepl");
    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(13), "refusal exit code");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("mutually exclusive"),
        "stderr must name the conflict (not the jail belt): {stderr}"
    );
}
