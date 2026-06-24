//! Pete feedback #6 (duplicate session ids) — end-to-end, driving the REAL `sb`
//! binary against a JAILED HOME (L9a / ADD-4 — HOME + ZMX_DIR point into a
//! per-test tempdir, never the real home). Mirrors the provider_field.rs harness
//! shape (forge registry rows, run the bin, assert exit + stderr).
//!
//! sb does NOT mint its own session id (`session_id` == the provider's id), so
//! uniqueness rides on the engine never DRIVING an ambiguous id. The deduped join
//! collapses two same-id LIVE rows to one — hiding the collision from
//! `resolve_or_die`'s loud `Many` path. The resume PREFLIGHT scans the RAW registry
//! plus `is_pid_alive` so a genuine collision (two distinct ALIVE pids on one id)
//! is refused loudly instead of silently picking a survivor.
//!
//! Each test carries a MUTATION-EVIDENCE comment naming the mutation it kills.

mod common;

use std::path::Path;
use std::process::{Child, Command};

fn sb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dispatch")
}

/// Spawn a real, short-lived child so we have a genuinely-ALIVE pid distinct from
/// the test runner's. `sb`'s `is_pid_alive` (kill(pid,0)) sees it live while the
/// binary runs. Caller kills + reaps it after the assertion.
fn live_child() -> Child {
    Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep")
}

/// Forge the given `<pid>.json` rows under a freshly-jailed HOME and run
/// `sb <args...>`. Returns (exit, stdout, stderr).
fn run_sb_with_rows(dir: &Path, rows: &[(i64, String)], args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    common::assert_not_real_home(&home);
    for (pid, json) in rows {
        std::fs::write(sessions.join(format!("{pid}.json")), json).unwrap();
    }
    let out = Command::new(sb_bin())
        .args(args)
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        .output()
        .expect("spawn sb");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn row(pid: i64, session_id: &str, name: &str, updated_at: i64) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":1717000000000,"updatedAt":{updated_at},"status":"idle","name":"{name}","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
    )
}

/// THE BUG: two distinct ALIVE processes registered under ONE session id (the
/// `sb-rust-orc-13` dup Pete hit). The deduped join collapses them to one row, so a
/// naive `sb resume <name>` would silently relaunch the survivor — compounding the
/// collision. The preflight must REFUSE loudly (exit 1) and surface the duplicates.
///
/// MUTATION EVIDENCE: removing the `common::refuse_id_collision` call site in
/// resume.rs (or the helper itself) reds this — resume would fall through to the
/// must-be-cold gate and either relaunch or print the generic "still alive" line,
/// never the id-collision refusal.
#[test]
fn resume_refuses_a_duplicate_id_collision() {
    let mut c1 = live_child();
    let mut c2 = live_child();
    let p1 = c1.id() as i64;
    let p2 = c2.id() as i64;
    // Same sessionId AND name; distinct ALIVE pids; distinct updatedAt so the join
    // deterministically keeps one (proving the OTHER is hidden by the dedup).
    let rows = [
        (
            p1,
            row(p1, "orc-dup-id-0001", "sb-rust-orc-dup", 1717000001000),
        ),
        (
            p2,
            row(p2, "orc-dup-id-0001", "sb-rust-orc-dup", 1717000002000),
        ),
    ];

    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_sb_with_rows(t.path(), &rows, &["resume", "sb-rust-orc-dup"]);

    let _ = c1.kill();
    let _ = c1.wait();
    let _ = c2.kill();
    let _ = c2.wait();

    assert_eq!(
        code, 1,
        "a duplicate-id collision must refuse with exit 1; stderr: {err}"
    );
    assert!(
        err.contains("id collision"),
        "stderr must name the id collision (never silently pick a survivor), got: {err}"
    );
}

/// A pid that is reliably DEAD: never a running process. `is_pid_alive` (kill(pid,0))
/// → false (ESRCH), so a row keyed by it is the "stale dead-pid leftover" case.
const DEAD_PID: i64 = 2_147_483_646;

/// THE DUP-SESSION BUG: ONE logical session shows up as TWO rows — a genuinely-ALIVE
/// process and a STALE leftover whose pid is DEAD but whose on-disk status still says
/// "idle". They share the name (and code) but have DISTINCT session ids (so the join
/// does NOT dedup them — dedup is by id). The OLD resolver derived liveness from the
/// status STRING, so the dead leftover counted as "live" → two "live" rows →
/// `resolve_or_die` died with "Ambiguous — matches 2 sessions". The pid-aware
/// refinement drops the dead-pid row, so `sb resume <name>` / `<code>` resolves to
/// the one truly-alive session (and then correctly reports it already-alive — NOT
/// "Ambiguous").
///
/// MUTATION EVIDENCE: reverting `resolve_or_die` to status-only liveness (or the NAME/
/// CODE-tier refinement in resolve.rs) reds this — both rows count live → "Ambiguous".
#[test]
fn resume_resolves_past_a_dead_pid_stale_namesake() {
    let mut child = live_child();
    let live_pid = child.id() as i64;
    // Same name; DISTINCT session ids (no dedup); one alive pid + one dead pid; BOTH
    // status "idle". The shared display prefix ("dup-...") is incidental.
    let rows = [
        (
            live_pid,
            row(live_pid, "dup-alive-0001", "sb-dup-name", 1717000002000),
        ),
        (
            DEAD_PID,
            row(DEAD_PID, "dup-stale-0001", "sb-dup-name", 1717000001000),
        ),
    ];

    let t = tempfile::tempdir().unwrap();
    // Exact-NAME query exercises the NAME tier's pid-aware refinement.
    let (code, _out, err) = run_sb_with_rows(t.path(), &rows, &["resume", "sb-dup-name"]);

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        !err.contains("Ambiguous"),
        "a dead-pid stale namesake must NOT cause an ambiguity, got: {err}"
    );
    // Resolved to the one live row → resume refuses as already-alive (the correct
    // downstream verdict), naming the LIVE pid — never the dead one.
    assert_eq!(
        code, 1,
        "resume of the resolved-live session refuses; stderr: {err}"
    );
    assert!(
        err.contains("already alive") && err.contains(&live_pid.to_string()),
        "must resolve to the LIVE pid {live_pid}, got: {err}"
    );
}

/// W1 phase 2 (ADD-8 residual): the SAME id-collision must be refused by the SHARED
/// `attach_resolved` guard. (Was pinned over BOTH `connect` and demoted `attach`;
/// the attach VERB is a retired erroring stub since STATE 22, so `connect` — the
/// mechanic's one caller — carries the pin alone.) A `sb connect <name>` over two
/// same-id alive rows would otherwise silently attach to the deduped survivor
/// (the exact ADD-8 hole).
///
/// MUTATION EVIDENCE: removing the `refuse_id_collision` preflight at the top of
/// `attach_resolved` reds this — connect would fall through to the cold-vs-
/// live dispatch and silently target the survivor (no "id collision" stderr).
#[test]
fn connect_refuses_a_duplicate_id_collision() {
    {
        let verb = "connect";
        let mut c1 = live_child();
        let mut c2 = live_child();
        let p1 = c1.id() as i64;
        let p2 = c2.id() as i64;
        let rows = [
            (
                p1,
                row(p1, "add8-dup-id-0001", "sb-add8-dup", 1717000001000),
            ),
            (
                p2,
                row(p2, "add8-dup-id-0001", "sb-add8-dup", 1717000002000),
            ),
        ];

        let t = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_sb_with_rows(t.path(), &rows, &[verb, "sb-add8-dup"]);

        let _ = c1.kill();
        let _ = c1.wait();
        let _ = c2.kill();
        let _ = c2.wait();

        assert_eq!(
            code, 1,
            "{verb}: a duplicate-id collision must refuse with exit 1; stderr: {err}"
        );
        assert!(
            err.contains("id collision"),
            "{verb}: stderr must name the id collision (never silently pick a survivor), got: {err}"
        );
    }
}

/// W1 phase 2 NUANCE: a SINGLE alive row must NOT be refused by the collision guard
/// (it is not a collision). `refuse_id_collision` returns None for the one-alive
/// case, so connect proceeds normally (attach retired, STATE 22) — here, with no
/// live mux pane in the jail, that means the cold-session dispatch, NOT the
/// id-collision refusal. The load-bearing assertion: the guard does NOT fire
/// ("id collision" absent).
///
/// MUTATION EVIDENCE: swapping `refuse_id_collision` for an `alive_pid_for_id`-style
/// single-alive refusal in `attach_resolved` would red this (it would wrongly refuse
/// a legitimate single-live attach with an "id collision" / "already alive" line).
#[test]
fn connect_does_not_refuse_a_single_alive_session() {
    {
        let verb = "connect";
        let mut child = live_child();
        let pid = child.id() as i64;
        let rows = [(
            pid,
            row(pid, "add8-single-0001", "sb-add8-single", 1717000001000),
        )];

        let t = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_sb_with_rows(t.path(), &rows, &[verb, "sb-add8-single"]);

        let _ = child.kill();
        let _ = child.wait();

        // The single-alive case is NOT a collision — the guard must stay silent.
        assert!(
            !err.contains("id collision"),
            "{verb}: a single-alive session must NOT trip the collision guard, got: {err}"
        );
        // Exit 1 here is the downstream cold dispatch (no live pane in the jail), not
        // the collision refusal — the point is the guard did not short-circuit.
        assert_eq!(code, 1, "{verb}: stderr: {err}");
    }
}

/// ROOT-CAUSE COMPLEMENT (feat/kill-cleanup-dead-records): `sb stop` must sweep
/// away OTHER dead-pid registry leftovers so they stop accumulating into the
/// dup-session "Ambiguous — matches 2 sessions" failure that the resolver fix only
/// papers over. After killing one live session, a SEPARATE stale dead-pid record
/// must be tombstoned, while a genuinely-ALIVE unrelated session's record must be
/// left untouched (I5: never touch a live pid).
///
/// MUTATION EVIDENCE: removing the dead-pid sweep in kill.rs (the
/// `reconcile_plan` plus `TombstoneDeadRegistry` loop) reds the "dead record is
/// gone" assertion — the `<DEAD_PID>.json` would survive. Widening the sweep to
/// touch live pids (an I5 regression) reds the "live record untouched" assertion.
#[test]
fn kill_sweeps_dead_pid_registry_leftovers_but_spares_live_ones() {
    // The session we kill. We forge it as an ALREADY-DEAD pid (a session that
    // exited ungracefully): kill resolves it, the dual-reap finds the pid dead and
    // tombstones it, then the sweep runs. Using a forged dead pid (not a real
    // child) avoids the zombie-vs-`kill(pid,0)` race — a SIGKILLed child stays a
    // zombie until reaped, which `is_pid_alive` still sees as alive and would fail
    // the verify. (The sweep logic under test is identical regardless.)
    let victim_pid: i64 = 2_147_483_645;
    // A genuinely-ALIVE unrelated session — its record must NOT be swept (I5).
    let mut bystander = live_child();
    let bystander_pid = bystander.id() as i64;

    let rows = [
        (
            victim_pid,
            row(
                victim_pid,
                "kill-victim-0001",
                "sb-kill-victim",
                1717000003000,
            ),
        ),
        // The stale leftover: a SEPARATE dead pid, on-disk status still "idle".
        (
            DEAD_PID,
            row(DEAD_PID, "kill-stale-0001", "sb-kill-stale", 1717000001000),
        ),
        (
            bystander_pid,
            row(
                bystander_pid,
                "kill-bystander-0001",
                "sb-kill-bystander",
                1717000002000,
            ),
        ),
    ];

    let t = tempfile::tempdir().unwrap();
    let (code, out, err) = run_sb_with_rows(t.path(), &rows, &["stop", "sb-kill-victim"]);

    let _ = bystander.kill();
    let _ = bystander.wait();

    assert_eq!(
        code, 0,
        "kill of the resolved (dead-pid) victim must succeed; stderr: {err}"
    );

    let sessions = t.path().join("home").join(".claude").join("sessions");
    // The stale dead-pid record is swept (tombstoned): live <pid>.json gone, a
    // <pid>.json.tombstoned present.
    assert!(
        !sessions.join(format!("{DEAD_PID}.json")).exists(),
        "the stale dead-pid record must be tombstoned (live json gone), stdout: {out}"
    );
    assert!(
        sessions
            .join(format!("{DEAD_PID}.json.tombstoned"))
            .exists(),
        "the stale dead-pid record must be tombstoned (tombstone present), stdout: {out}"
    );
    // I5: the genuinely-alive bystander's record is untouched.
    assert!(
        sessions.join(format!("{bystander_pid}.json")).exists(),
        "an alive unrelated session's record must NOT be swept (I5), stdout: {out}"
    );
    assert!(
        !sessions
            .join(format!("{bystander_pid}.json.tombstoned"))
            .exists(),
        "an alive unrelated session's record must NOT be tombstoned (I5), stdout: {out}"
    );
    // The one-line summary fires (exactly one stale record cleaned).
    assert!(
        out.contains("cleaned 1 dead record"),
        "kill should report the sweep summary, stdout: {out}"
    );
}

/// THE COLD-MISREAD HARDENING (orchestrator revival-ladder floor): a session whose
/// underlying process is ALIVE but which the deduped join reports resolvable as a
/// resume target must NOT be relaunched (it would spawn a SECOND process on the
/// same id). Here a single live row is present; resume must refuse — pointing at
/// attach — rather than launch a duplicate.
///
/// MUTATION EVIDENCE: removing the `alive_pid_for_id` already-alive guard reds this
/// (resume would proceed past the preflight on a row the dedup-status could mask).
#[test]
fn resume_refuses_an_already_alive_session() {
    let mut child = live_child();
    let pid = child.id() as i64;
    let rows = [(
        pid,
        row(pid, "live-single-0001", "sb-rust-live", 1717000001000),
    )];

    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_sb_with_rows(t.path(), &rows, &["resume", "sb-rust-live"]);

    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        code, 1,
        "resuming an already-alive session must refuse; stderr: {err}"
    );
    assert!(
        err.contains("already alive") || err.contains("still alive"),
        "stderr must point the user at attach, got: {err}"
    );
}
