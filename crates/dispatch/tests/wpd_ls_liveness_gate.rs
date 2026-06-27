//! WP-D part (a) — the `qd ls` LIVENESS GATE, CONSUMER-EXERCISING red/green over
//! the REAL `qd` binary (not the pure unit). A single registry row backed by a
//! REAL pid is observed through `qd ls --json` across the pid's alive→reaped
//! transition:
//!
//!   - pid ALIVE  → the row shows its registry status `idle` (LIVE) — NOT gated.
//!   - pid REAPED → the SAME row is GATED to `cold` (a dead pid is no longer shown
//!     live; "stop counting zombies alive" / stop showing a gone pid live).
//!
//! RED-WITHOUT: with the gate removed (ls.rs returns the raw status) the reaped-pid
//! row would still report `idle` — the live false-"alive" the WP fixes. GREEN-WITH:
//! it reports `cold`. This uses a REAL pid that genuinely transitions, so it is
//! immune to the synthetic-pid scaffolding question entirely.

use std::path::{Path, PathBuf};
use std::process::Command;

fn sb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

struct Jail {
    home: PathBuf,
}

impl Jail {
    fn new(dir: &Path) -> Self {
        std::fs::create_dir_all(dir.join("home").join(".claude").join("sessions")).unwrap();
        Jail {
            home: dir.join("home"),
        }
    }

    /// Write a LIVE-shaped claude registry row `<pid>.json` with an explicit
    /// `startedAt` (must be the pid's real start so the classifier's reuse-guard
    /// matches identity).
    fn write_row(&self, pid: i64, session_id: &str, name: &str, started_ms: i64, updated_ms: i64) {
        let sessions = self.home.join(".claude").join("sessions");
        let row = format!(
            r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":{started_ms},"updatedAt":{updated_ms},"status":"idle","name":"{name}","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
        );
        std::fs::write(sessions.join(format!("{pid}.json")), row).unwrap();
    }

    fn ls_json(&self) -> Vec<serde_json::Value> {
        let out = Command::new(sb_bin())
            .args(["ls", "--json"])
            .env("HOME", &self.home)
            .env_remove("QD_HOME")
            .env_remove("QD_MUX")
            .output()
            .expect("spawn qd ls --json");
        assert_eq!(out.status.code(), Some(0), "qd ls --json exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str::<Vec<serde_json::Value>>(&stdout).expect("ls --json is an array")
    }
}

fn status_of<'a>(rows: &'a [serde_json::Value], sid: &str) -> Option<&'a str> {
    rows.iter()
        .find(|r| r.get("sessionId").and_then(|v| v.as_str()) == Some(sid))
        .and_then(|r| r.get("status").and_then(|v| v.as_str()))
}

#[test]
fn ls_gates_a_real_pid_when_it_dies_not_while_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let jail = Jail::new(tmp.path());

    // A REAL live child (the silent-window shape — /proc state S, like a quiet
    // claude). Record wall-clock now as the row's startedAt — the production shape
    // (registry startedAt = registration time ≈ process start), within the
    // classifier's 120s reuse-slack of the real proc start. (Reading proc_start_ms
    // of the just-forked child would be unreliable — ps etime of a sub-second-old
    // process can misparse — a registration-time read the real fleet never does.)
    let mut child = Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id() as i64;
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let sid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    jail.write_row(pid, sid, "wpdlive", start, start + 1_000);

    // GREEN-WITH, alive half: the pid is alive → NOT gated → shows its registry
    // status `idle` (LIVE). (A bare kill(0) would also say alive here — the gate's
    // value is being right on the DEAD half below, where kill(0) miscounts.)
    let alive = jail.ls_json();
    assert_eq!(
        status_of(&alive, sid),
        Some("idle"),
        "a live pid keeps its registry status (not gated): {alive:?}"
    );

    // Kill + REAP the child → /proc/<pid> is gone.
    let _ = child.kill();
    let _ = child.wait();

    // GREEN-WITH, dead half: the SAME row, pid now reaped → GATED to `cold`. This
    // is the load-bearing assertion: with the gate removed it would still be
    // `idle` (the live false-"alive" bug = RED-WITHOUT).
    let dead = jail.ls_json();
    assert_eq!(
        status_of(&dead, sid),
        Some("cold"),
        "a reaped pid must be gated out of live (→ cold): {dead:?}"
    );
}
