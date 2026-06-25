//! Punch B5 item 9 — the P0 r9/r10-endorsed STRAND-WITH-NOTE verb-glue pin
//! (deferred at P0 close for zero tree-delta between clean rounds; cutover
//! packet: "the verb glue end-to-end pin is the gap").
//!
//! Class under pin: a registry row WITHOUT a usable `startedAt` (absent, or a
//! stale/past one) whose live pid the cmdline arm cannot identify as the
//! session program. The two-armed identity predicate (kill.rs
//! `pid_is_foreign`) judges that pid FOREIGN, and `sb stop` then — BY
//! DOCUMENTED DESIGN (r9 m1, accepted minor; real Claude always stamps its
//! boot instant so real rows are immune):
//!   - never signals the process (the stranger survives — STRANDED),
//!   - prints the foreign NOTE on stderr,
//!   - tombstones the row anyway (the session is judged dead),
//!   - exits 0 ("killed ...").
//!
//! Freezing this stops a future change from silently flipping the strand into
//! an over-kill (the worse direction). Unit coverage exists (truth-table
//! foreign cells + pid_is_foreign pins); THIS file is the missing e2e glue,
//! driving the real binary in a jail (harness mirrors info_json.rs).
//!
//! Also pinned (the read-surface half of the item): rows of this class SURVIVE
//! the read verbs — `ls --json` renders them with the `startedAt` key ABSENT
//! (absent-not-null; a wrong-typed startedAt degrades to None at the registry
//! read, registry.rs per-field-permissive units :928/:1250) and `info --json`
//! resolves them. PIN-ONLY: behavior was verified as documented before writing.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use common::assert_not_real_home;

fn sb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// A genuinely-ALIVE process whose cmdline is visibly NOT the session program
/// (models an exec'd-away custom app — the r9 m1 shape). Caller kills + reaps.
fn live_stranger() -> Child {
    Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep")
}

const UPDATED_MS: i64 = 1_717_495_300_000;
/// A startedAt FAR in the past (June 2024): any live occupant of the pid
/// started ≫ START_TIME_SLACK_MS after it → the start-time arm cannot claim it.
const PAST_STARTED_AT: i64 = 1_717_000_000_000;

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
    fn sessions_dir(&self) -> PathBuf {
        self.home.join(".claude").join("sessions")
    }

    /// A registry row with a CALLER-CONTROLLED startedAt JSON fragment:
    /// `None` → field absent; `Some(r#""12345""#)` / `Some("1717000000000")` →
    /// verbatim value (string = the wrong-typed shape).
    fn write_row_started_at(
        &self,
        pid: i64,
        session_id: &str,
        name: &str,
        started_at: Option<&str>,
    ) {
        let started = started_at
            .map(|v| format!(r#""startedAt":{v},"#))
            .unwrap_or_default();
        let row = format!(
            r#"{{"pid":{pid},"sessionId":"{session_id}",{started}"cwd":"/w","updatedAt":{UPDATED_MS},"status":"idle","name":"{name}","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
        );
        std::fs::write(self.sessions_dir().join(format!("{pid}.json")), row).unwrap();
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(sb_bin())
            .args(args)
            .env("HOME", &self.home)
            .env("ZMX_DIR", &self.zmx)
            .env_remove("SB_HOME")
            .env_remove("SB_MUX")
            .env_remove("CLAUDE_BIN")
            .output()
            .expect("spawn sb");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// Drive `sb stop <name>` against a live-unidentifiable pid and assert the
/// FULL documented strand-with-note contract.
fn assert_strand_with_note(j: &Jail, child: &mut Child, name: &str) {
    let pid = child.id() as i64;
    let (code, out, err) = j.run(&["stop", name]);

    // (1) exit 0 — the verb reports success ("killed ...").
    assert_eq!(code, 0, "strand path exits 0; stdout: {out} stderr: {err}");
    assert!(
        out.contains(&format!("killed {name}")),
        "the success line names the session: {out}"
    );
    // (2) the foreign NOTE — the de-silenced honest edge (r7 OPEN-Q1).
    assert!(
        err.contains("belongs to a different process") && err.contains("not signaled"),
        "the strand note on stderr: {err}"
    );
    // (3) the row is TOMBSTONED (the session is judged dead)...
    assert!(
        !j.sessions_dir().join(format!("{pid}.json")).exists(),
        "live row renamed away"
    );
    assert!(
        j.sessions_dir()
            .join(format!("{pid}.json.tombstoned"))
            .exists(),
        "tombstone written"
    );
    // (4) ...but the process was NEVER SIGNALED — the stranger survives.
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "the live process must survive a strand (not be killed)"
    );
}

/// r9 m1 leg: NO startedAt at all (legacy/non-conforming row) — only the
/// cmdline arm exists, it cannot identify `sleep`, the pid is foreign.
#[test]
fn stop_strands_with_note_on_no_started_at_row() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    let mut child = live_stranger();
    j.write_row_started_at(
        child.id() as i64,
        "9999aaaa-aaaa-4aaa-8aaa-111111111111",
        "app",
        None,
    );
    assert_strand_with_note(&j, &mut child, "app");
    child.kill().ok();
    child.wait().ok();
}

/// PAST-startedAt leg: the row claims a boot far in the past; the live
/// occupant started now (≫ slack) → start-time arm refuses, cmdline arm
/// cannot identify → foreign. Same strand-with-note contract.
#[test]
fn stop_strands_with_note_on_past_started_at_row() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    let mut child = live_stranger();
    j.write_row_started_at(
        child.id() as i64,
        "9999bbbb-bbbb-4bbb-8bbb-222222222222",
        "oldboot",
        Some(&PAST_STARTED_AT.to_string()),
    );
    assert_strand_with_note(&j, &mut child, "oldboot");
    child.kill().ok();
    child.wait().ok();
}

/// Read-surface half: no-/wrong-typed-startedAt rows SURVIVE `ls --json` at
/// the verb boundary with the `startedAt` key ABSENT (absent-not-null — the
/// registry per-field degrade surfacing through the render), while a
/// well-typed past startedAt renders. And `info --json` resolves a
/// no-startedAt row (the promised field list carries no startedAt at all).
#[test]
fn ls_and_info_render_no_started_at_rows_with_key_absent() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    // Dead fake pids: liveness is irrelevant to the read surface (rows keep
    // their registry status; the join never gates on pid-aliveness).
    j.write_row_started_at(
        2_147_483_640,
        "aaaa0001-aaaa-4aaa-8aaa-000000000001",
        "noboot",
        None,
    );
    j.write_row_started_at(
        2_147_483_641,
        "aaaa0002-aaaa-4aaa-8aaa-000000000002",
        "wrongboot",
        Some(r#""12345""#), // string where i64 is declared → degrades to None
    );
    j.write_row_started_at(
        2_147_483_642,
        "aaaa0003-aaaa-4aaa-8aaa-000000000003",
        "boot2024",
        Some(&PAST_STARTED_AT.to_string()),
    );

    let (code, out, err) = j.run(&["ls", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    let row = |name: &str| -> &serde_json::Value {
        rows.iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("row {name} SURVIVES the verb: {out}"))
    };
    assert!(
        row("noboot").get("startedAt").is_none(),
        "absent startedAt → key ABSENT (not null): {out}"
    );
    assert!(
        row("wrongboot").get("startedAt").is_none(),
        "wrong-typed startedAt degrades to absent at the verb boundary: {out}"
    );
    assert!(
        row("boot2024").get("startedAt").is_some(),
        "well-typed startedAt renders: {out}"
    );

    // info --json resolves the no-startedAt row (the load-bearing verb-glue
    // survival assert: exit 0 + the row resolves by name). S4: the global
    // "info carries no startedAt" shape assert was dropped — info omits
    // startedAt for EVERY row, so it can't distinguish this class-specific
    // case; that shape is owned by tests/info_json.rs's field-set goldens.
    let (code, out, err) = j.run(&["info", "noboot", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("info json");
    assert_eq!(
        v["name"], "noboot",
        "the no-startedAt row resolves at the verb"
    );
}
