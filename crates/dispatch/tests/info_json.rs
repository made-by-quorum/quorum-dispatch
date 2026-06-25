//! P0 `sb info <target> --json` (spec-w8-info-json) — bin-level golden pins
//! driving the REAL `sb` binary against a JAILED HOME (L9a / ADD-4 discipline;
//! harness mirrors p0_qafix.rs for the forged-registry rows — integration test
//! binaries cannot import each other, duplication is the sanctioned shape).
//!
//! The json object is the point-resolution surface bond joins against; the
//! field list was promised to P1 exactly as the goldens freeze it:
//! name, sessionId, sbId?, sbIdPrefix?, status, live, pid, provider.
//!
//! Rows pinned here: mapped + live, unmapped sbId, cold row,
//! stale-idle-dead-pid (live:false), ambiguity error json-free on stderr.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use common::{assert_not_real_home, set_mtime_ms};

fn sb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Read a golden file, or — when `SB_REGEN_GOLDEN=1` — write `actual` and return
/// (so the first run freezes it). Byte-equality assert. Mirrors codex_ls.rs.
fn assert_golden(name: &str, actual: &str) {
    let path = golden_dir().join(name);
    if std::env::var("SB_REGEN_GOLDEN").is_ok() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, actual).unwrap();
        eprintln!("regenerated golden {name}");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {path:?}: {e} (run SB_REGEN_GOLDEN=1)"));
    assert_eq!(actual, expected, "golden mismatch for {name}");
}

/// A pid that is reliably DEAD (never a running process) — `is_pid_alive` →
/// false (p0_qafix.rs convention).
const DEAD_PID: i64 = 2_147_483_646;

/// Spawn a real, short-lived child so we have a genuinely-ALIVE pid distinct
/// from the test runner's. Caller kills + reaps it after the assertion.
fn live_child() -> Child {
    Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep")
}

/// Frozen registry timestamps (deterministic fixtures; the json surface carries
/// no timestamp fields, this is belt-level determinism only).
const UPDATED_MS: i64 = 1_717_495_300_000;

fn registry_row(pid: i64, session_id: &str, name: &str) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":1717000000000,"updatedAt":{UPDATED_MS},"status":"idle","name":"{name}","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
    )
}

/// One idstore mint line (the ids.jsonl on-disk shape, idstore.rs).
fn mint_line(id: &str, session_id: &str, name: &str) -> String {
    format!(
        r#"{{"v":1,"ts":"t","event":"mint","id":"{id}","session_id":"{session_id}","name":"{name}"}}"#
    )
}

/// Build a jail under `dir`: registry rows in `<home>/.claude/sessions/`,
/// optional ids.jsonl mints in `<home>/.quorum/dispatch/state/`, optional cold transcripts
/// in `<home>/.claude/projects/proj/`. Returns nothing; `run_sb` runs against it.
struct Jail {
    home: PathBuf,
    zmx: PathBuf,
}

fn jail(dir: &Path) -> Jail {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    assert_not_real_home(&home);
    Jail { home, zmx }
}

impl Jail {
    fn write_row(&self, pid: i64, session_id: &str, name: &str) {
        let sessions = self.home.join(".claude").join("sessions");
        std::fs::write(
            sessions.join(format!("{pid}.json")),
            registry_row(pid, session_id, name),
        )
        .unwrap();
    }

    fn write_ids(&self, lines: &[String]) {
        let state = self.home.join(".quorum").join("dispatch").join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("ids.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    /// Seed the transcript a REAL claude session would have left behind (the
    /// p0_id_matrix shape): agent-name record (cold-row name derivation) + one
    /// user record. Frozen mtime so the cold row's lastActive is deterministic.
    fn write_cold_transcript(&self, uuid: &str, name: &str) {
        let proj = self.home.join(".claude").join("projects").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join(format!("{uuid}.jsonl"));
        let body = format!(
            "{{\"type\":\"agent-name\",\"agentName\":\"{name}\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":\"hello\"}},\
             \"cwd\":\"/w\",\"sessionId\":\"{uuid}\"}}\n"
        );
        std::fs::write(&path, body).unwrap();
        set_mtime_ms(&path, UPDATED_MS);
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(sb_bin())
            .args(args)
            .env("HOME", &self.home)
            .env("ZMX_DIR", &self.zmx)
            // The ids store resolves SB_HOME || <home>/.sb — pin to the jail.
            .env_remove("SB_HOME")
            .env_remove("SB_MUX")
            .output()
            .expect("spawn sb");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// Replace a varying live pid number with `"<PID>"` so goldens are run-stable.
fn normalize_pid(out: &str, pid: i64) -> String {
    out.replace(&format!("\"pid\": {pid}"), "\"pid\": \"<PID>\"")
}

const SID_LIVE: &str = "11111111-aaaa-4aaa-8aaa-111111111111";
const SID_STALE: &str = "22222222-bbbb-4bbb-8bbb-222222222222";
const SID_BARE: &str = "33333333-cccc-4ccc-8ccc-333333333333";
const SID_COLD: &str = "44444444-dddd-4ddd-8ddd-444444444444";

/// The two-mapped-rows jail: a LIVE "wk" row + a STALE-idle-dead-pid "stale"
/// row, both with minted stable ids sharing a 2-char prefix (→ "ab3"/"ab4",
/// proving the shortest-unique computation runs among the LIST).
fn mapped_jail(dir: &Path, live_pid: i64) -> Jail {
    let j = jail(dir);
    j.write_row(live_pid, SID_LIVE, "wk");
    j.write_row(DEAD_PID, SID_STALE, "stale");
    j.write_ids(&[
        mint_line("ab3kx9mq", SID_LIVE, "wk"),
        mint_line("ab47qrst", SID_STALE, "stale"),
    ]);
    j
}

/// Mapped + live: every promised field present; live:true (idle + alive pid).
///
/// MUTATION EVIDENCE: dropping any field, emitting null-instead-of-absent for
/// a MAPPED sbId, or breaking the prefix computation reds the golden.
#[test]
fn info_json_mapped_live_golden() {
    let t = tempfile::tempdir().unwrap();
    let mut child = live_child();
    let j = mapped_jail(t.path(), child.id() as i64);
    let (code, out, err) = j.run(&["info", "wk", "--json"]);
    child.kill().ok();
    child.wait().ok();
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(err, "", "json path emits nothing on stderr");
    assert_golden(
        "info-json-mapped-live.json",
        &normalize_pid(&out, child.id() as i64),
    );
}

/// Stale-idle-dead-pid: the registry row still says "idle" but the process is
/// gone → status stays "idle", `live` is FALSE (the pid arm of the contract).
///
/// MUTATION EVIDENCE: computing `live` from status alone greens live:true and
/// reds the golden.
#[test]
fn info_json_stale_idle_dead_pid_golden() {
    let t = tempfile::tempdir().unwrap();
    let mut child = live_child();
    let j = mapped_jail(t.path(), child.id() as i64);
    let (code, out, err) = j.run(&["info", "stale", "--json"]);
    child.kill().ok();
    child.wait().ok();
    assert_eq!(code, 0, "stderr: {err}");
    // DEAD_PID is a constant → the golden is fully deterministic (no normalize).
    assert_golden("info-json-stale-dead-pid.json", &out);
    assert!(
        out.contains("\"live\": false"),
        "stale idle row with a dead pid must be live:false: {out}"
    );
}

/// Unmapped sbId: no ids.jsonl mint → sbId/sbIdPrefix keys ABSENT (the
/// ls --json absent-not-null convention).
#[test]
fn info_json_unmapped_sb_id_golden() {
    let t = tempfile::tempdir().unwrap();
    let mut child = live_child();
    let j = jail(t.path());
    j.write_row(child.id() as i64, SID_BARE, "bare");
    let (code, out, err) = j.run(&["info", "bare", "--json"]);
    child.kill().ok();
    child.wait().ok();
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        !out.contains("sbId"),
        "unmapped → NO sbId/sbIdPrefix: {out}"
    );
    assert_golden(
        "info-json-unmapped.json",
        &normalize_pid(&out, child.id() as i64),
    );
}

/// Cold row (JSONL-only history, no registry row): status "cold", pid null,
/// live:false regardless of pid.
#[test]
fn info_json_cold_row_golden() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    j.write_cold_transcript(SID_COLD, "coldy");
    let (code, out, err) = j.run(&["info", "coldy", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_golden("info-json-cold.json", &out);
    assert!(
        out.contains("\"status\": \"cold\"") && out.contains("\"live\": false"),
        "cold row must be live:false: {out}"
    );
}

/// Ambiguity: two ALIVE rows share the exact name — the standard resolver's
/// loud listing fires on stderr, exit 1, and stdout carries NO json (the error
/// paths are UNCHANGED by the flag).
#[test]
fn info_json_ambiguous_exits_1_json_free() {
    let t = tempfile::tempdir().unwrap();
    let mut a = live_child();
    let mut b = live_child();
    let j = jail(t.path());
    j.write_row(a.id() as i64, SID_LIVE, "dup");
    j.write_row(b.id() as i64, SID_STALE, "dup");
    let (code, out, err) = j.run(&["info", "dup", "--json"]);
    a.kill().ok();
    a.wait().ok();
    b.kill().ok();
    b.wait().ok();
    assert_eq!(code, 1, "ambiguous → exit 1; stderr: {err}");
    assert_eq!(out, "", "NO json on the ambiguity path, got: {out}");
    assert!(
        err.contains("Ambiguous") && err.contains("2 sessions"),
        "the standard loud listing on stderr, got: {err}"
    );
}

/// Not-found: same contract — loud stderr, exit 1, json-free stdout.
#[test]
fn info_json_not_found_exits_1_json_free() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    let (code, out, err) = j.run(&["info", "nosuch", "--json"]);
    assert_eq!(code, 1, "not-found → exit 1; stderr: {err}");
    assert_eq!(out, "", "NO json on the not-found path, got: {out}");
    assert!(
        err.contains("No session matching \"nosuch\""),
        "resolveOrDie's message, got: {err}"
    );
}

/// Without the flag the human surface still renders (byte-parity is owned by
/// the existing info goldens; this belt just pins "not json" at the bin level).
#[test]
fn info_without_flag_stays_human() {
    let t = tempfile::tempdir().unwrap();
    let mut child = live_child();
    let j = mapped_jail(t.path(), child.id() as i64);
    let (code, out, err) = j.run(&["info", "wk"]);
    child.kill().ok();
    child.wait().ok();
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        !out.trim_start().starts_with('{'),
        "human text, not json: {out}"
    );
    assert!(
        out.contains("Name:") && out.contains("Session ID:"),
        "the human info surface: {out}"
    );
}
