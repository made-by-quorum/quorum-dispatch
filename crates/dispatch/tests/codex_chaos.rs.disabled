//! C-CHAOS — tier-a FAULT-INJECTION for the codex adapter (codex-adapter-impl-plan
//! §4.3 C-CHAOS row). Posture: inject a fault, then prove the adapter DEGRADES-NOT-
//! CRASHES, leaves NO orphan, and never LATCHES a stale endpoint / rollout-tail.
//! Cred-free, PRE-MODEL (no model turn, no OPENAI_API_KEY).
//!
//! Four fault classes, each asserting the OBSERVED EFFECT AT SOURCE (registry row,
//! process tree via `pgrep -x codex`, ws endpoint, rollout file) — NEVER a return
//! string:
//!   1. reconnect-across-invocations — a fresh qd invocation re-addresses the
//!      running daemon via the recorded ws endpoint + `cmdline_is_our_daemon`;
//!      `qd info --json` hardened to parse endpoint+pid (not the exit code).
//!   2. crash + rebind — capture the port from row.endpoint (never assume); crash
//!      the daemon; prove the stale endpoint is NOT latched (the reconnect re-check
//!      rejects the dead daemon); rebind re-addresses correctly; port characterized.
//!   3. daemon-gone wait-fallback — fabricate-rollout-at-transcript_path()
//!      CONNECTIONLESS; with no daemon, `qd wait` derives status from the durable
//!      rollout tail (the rollout is the truth even when the daemon is gone).
//!   4. pgid-teardown orphan — (a) the real-daemon in-pgid teardown reaps the npm
//!      2-proc launcher's vendored child (load-bearing -pgid, non-vacuous); (b) the
//!      ESCAPING-orphan CHARACTERIZATION: a descendant that setsids OUT of the
//!      group is OUTSIDE the -pgid reach BY CONSTRUCTION — we PROVE the escape at
//!      source (different pgid) and PIN the teardown's reach boundary, NOT a
//!      vacuous "zero survivors" (the mirror of the vacuous green). The tier-a
//!      oracle rules whether an escaped descendant is in/out of threat model.
//!
//! ALL chaos tests are `#[ignore]` — they NEVER run under a plain `cargo test`.
//! Run them deliberately only: `cargo test -- --ignored` (real-daemon classes
//! 1,2,4a additionally gate on `QD_CODEX_LIVE=1` since they spawn `codex
//! app-server`). This hard-gate (Pete, 2026-06-30) keeps the destructive
//! group-reaping path off any acceptance/zero-NEW run. Evidence per class under
//! `$CCONF_EVIDENCE_DIR/<class>/` (round dir set by the cargo).

mod common;
use common::live::*;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dispatch::create_daemon::cmdline_is_our_daemon;
use dispatch::effects::is_pid_alive;
use dispatch::model::SessionStatus;
use dispatch::provider::codex::rollout::{derive_status, read_lines};
use dispatch::registry::{write_entry, RegistryEntry};

// ===========================================================================
// Helpers (at-source process / pgid / proctree probes; -x not -f per the
// pgrep-self-match lesson; the probe is a compiled Command, not a shell whose
// argv would self-match).
// ===========================================================================

/// Per-class evidence bundle under `$CCONF_EVIDENCE_DIR/<class>/` (the round dir
/// is set by the cargo, e.g. target/cred-evidence/cchaos/round1).
fn chaos_bundle(class: &str) -> PathBuf {
    let root = std::env::var("CCONF_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cchaos-evidence"));
    let dir = root.join(class);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Fabricate a rollout in the codex date tree so `transcript_path`/`date_walk_for_id`
/// resolves it by the embedded uuid (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`).
fn write_rollout_in_tree(codex_home: &Path, uuid: &str, body: &str) -> PathBuf {
    let dir = codex_home.join("sessions").join("2026").join("06").join("29");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("rollout-2026-06-29T12-00-00-{uuid}.jsonl"));
    std::fs::write(&p, body).unwrap();
    p
}

/// The process-group id of a pid (`ps -o pgid=`). `None` if the pid is gone.
fn pgid_of(pid: i64) -> Option<i64> {
    let out = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<i64>().ok()
}

/// The full argv of a pid (`ps -o args=`). Empty when the pid is gone.
fn proc_args(pid: i64) -> String {
    Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// `codex`-named pids (pgrep -x codex — the vendored child's comm is `codex`; the
/// node wrapper's comm is `node`, so `-x codex` finds the REAL codex binary procs)
/// whose env carries THIS jail's CODEX_HOME (instance-addressed, never a global
/// kill). `-x` (exact name), not `-f` (which self-matches a probe shell's argv).
fn jail_codex_pids(codex_home: &Path) -> Vec<i64> {
    let needle = format!("CODEX_HOME={}", codex_home.display());
    let Ok(out) = Command::new("pgrep").args(["-x", "codex"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<i64>().ok())
        .filter(|&pid| {
            Command::new("ps")
                .args(["eww", "-p", &pid.to_string()])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains(&needle))
                .unwrap_or(false)
        })
        .collect()
}

/// `qd start --provider codex` in the jail; return the row for `name` at source.
fn start_codex(jail: &Path, name: &str) -> RegistryEntry {
    let cwd = jail.join("work");
    let start = run_qd(
        jail,
        &["start", name, "--provider", "codex", "--cwd", cwd.to_string_lossy().as_ref()],
    );
    assert!(
        start.status.success(),
        "qd start {name} must succeed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    codex_rows(jail)
        .into_iter()
        .find(|r| r.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no codex registry row for {name} after start"))
}

/// Spawn then immediately reap a process to mint a definitely-DEAD pid (for the
/// daemon-gone fabricated row — a realistic crashed-daemon pid, not pid 0).
fn dead_pid() -> i64 {
    let mut c = Command::new("sleep").arg("30").spawn().expect("spawn sleep for dead pid");
    let pid = c.id() as i64;
    let _ = c.kill();
    let _ = c.wait();
    wait_dead(pid);
    pid
}

/// Panic-safe group reaper: SIGKILL each recorded pid AND its process group on
/// drop, so a mid-test panic never leaks a detached sleep / model root.
struct GroupReaper(Arc<Mutex<Vec<i64>>>);
impl Drop for GroupReaper {
    fn drop(&mut self) {
        for &pid in self.0.lock().unwrap().iter() {
            // SAFETY: `--` forces `-{pid}` to be a process-group OPERAND, never an
            // option-misparse → `kill(-1)` (kill-all). Guard pid>1 so we can never
            // target pgid 0 (caller's own group) or -1 (everything). See the audit-
            // proven 2026-06-30 outage: `kill -9 -<pid>` (no `--`) executed kill(-1)
            // and SIGKILLed the entire user slice.
            if pid > 1 {
                let _ = Command::new("kill").args(["-9", "--", &format!("-{pid}")]).status();
                let _ = Command::new("kill").args(["-9", "--", &pid.to_string()]).status();
            }
        }
    }
}

// ===========================================================================
// CLASS 1 — reconnect-across-invocations (gated). A fresh qd invocation
// re-addresses the running daemon via the recorded endpoint + cmdline_is_our_daemon;
// `qd info --json` parsed at source (endpoint+pid), not the exit code.
// ===========================================================================

#[test]
#[ignore = "destructive chaos — never auto-run; deliberate only: cargo test -- --ignored (+ QD_CODEX_LIVE=1)"]
fn chaos_reconnect_across_invocations() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping class-1 reconnect (needs a live daemon)");
        return;
    }
    let jail = make_jail("chaos1");
    let codex_home = jail.join("codex-home");
    let bundle = chaos_bundle("class1-reconnect");
    ev_text(&bundle, "RAN.txt", "class-1 reconnect ran live\n");
    let pids = Arc::new(Mutex::new(Vec::<i64>::new()));
    let _belt = ReapAll(pids.clone());

    let row = start_codex(&jail, "chaos1");
    let pid = row.pid.expect("row carries pid");
    let endpoint = row.endpoint.clone().expect("codex row carries endpoint");
    pids.lock().unwrap().push(pid);

    // The recorded endpoint is LIVE (the reconnect target answers).
    assert!(ws_initialize_ok(&endpoint), "recorded endpoint answers initialize (live target)");

    // RECONNECT — a SEPARATE qd invocation re-reads the row + re-addresses the
    // running daemon. `qd info --json` hardened: parse the structured output for
    // the endpoint + pid AT SOURCE, never the exit code.
    let info = run_qd(&jail, &["info", "chaos1", "--json"]);
    let info_out = String::from_utf8_lossy(&info.stdout).into_owned();
    let info_err = String::from_utf8_lossy(&info.stderr).into_owned();
    assert!(!info_err.contains("panicked"), "qd info must not panic; stderr:\n{info_err}");
    let parsed: serde_json::Value =
        serde_json::from_str(info_out.trim()).unwrap_or_else(|e| panic!("qd info --json is valid JSON: {e}\n{info_out}"));
    let blob = parsed.to_string();
    assert!(
        blob.contains(&pid.to_string()),
        "qd info --json surfaces the daemon pid at source (not the exit code): {blob}"
    );
    // ls --live --json: the row is live with the SAME pid (re-addressed, no respawn).
    let ls = run_qd(&jail, &["ls", "--live", "--json"]);
    let ls_out = String::from_utf8_lossy(&ls.stdout).into_owned();
    assert!(ls.status.success(), "qd ls --live --json succeeds");
    assert!(
        ls_out.contains(&row.session_id.clone().unwrap_or_default()),
        "ls --live --json surfaces the live codex session id"
    );

    // AT-SOURCE reconnect re-check: the PRODUCTION predicate confirms the live
    // daemon IS ours via the recorded endpoint on its cmdline.
    let cmdline = proc_args(pid);
    assert!(
        cmdline_is_our_daemon(Some(cmdline.as_str()), Some(endpoint.as_str())),
        "cmdline_is_our_daemon TRUE for the live daemon (reconnect addressing): {cmdline:?}"
    );

    // Exactly ONE daemon throughout — reconnect re-addressed, never respawned.
    let live_rows = codex_rows(&jail)
        .into_iter()
        .filter(|r| r.pid.map(|p| is_pid_alive(p as i32)).unwrap_or(false))
        .count();
    assert_eq!(live_rows, 1, "reconnect did NOT spawn a second daemon (one live codex row)");
    assert!(ws_initialize_ok(&endpoint), "endpoint still live after the reconnect invocations");

    ev_text(
        &bundle,
        "reconnect-at-source.txt",
        &format!(
            "pid={pid} endpoint={endpoint}\n\
             qd info --json: pid surfaced at source = true; panicked=false\n\
             cmdline_is_our_daemon(live cmdline, endpoint) = true\n\
             live codex rows after reconnect = 1 (no respawn)\n\
             endpoint answers initialize before AND after reconnect = true\n\
             cmdline={cmdline}\n"
        ),
    );

    let _ = run_qd(&jail, &["stop", "chaos1"]);
    wait_dead(pid);
    assert!(!jail_codex_daemon_alive(&codex_home), "no survivor after stop");
    *pids.lock().unwrap() = Vec::new();
    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// CLASS 2 — crash + rebind (gated). Capture the port; crash the daemon; prove the
// stale endpoint is NOT latched (the reconnect re-check rejects the dead daemon);
// rebind re-addresses correctly; the port is CHARACTERIZED (ephemeral — re-rolled
// + `--port` parked, so "same port" cannot be forced).
// ===========================================================================

#[test]
#[ignore = "destructive chaos — never auto-run; deliberate only: cargo test -- --ignored (+ QD_CODEX_LIVE=1)"]
fn chaos_crash_rebind_no_stale_endpoint_latch() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping class-2 crash+rebind (needs a live daemon)");
        return;
    }
    let jail = make_jail("chaos2");
    let codex_home = jail.join("codex-home");
    let bundle = chaos_bundle("class2-crash-rebind");
    ev_text(&bundle, "RAN.txt", "class-2 crash+rebind ran live\n");
    let pids = Arc::new(Mutex::new(Vec::<i64>::new()));
    let _belt = ReapAll(pids.clone());

    // Daemon A — capture pid, endpoint, PORT (from row.endpoint, never assumed).
    let a = start_codex(&jail, "chaos2a");
    let pid_a = a.pid.expect("pid");
    let endpoint_a = a.endpoint.clone().expect("endpoint");
    let port_a = endpoint_port(&endpoint_a);
    pids.lock().unwrap().push(pid_a);
    assert!(ws_initialize_ok(&endpoint_a), "A endpoint answers before crash");

    // CRASH A — abruptly SIGKILL the daemon's process group, BYPASSING `qd stop`, so
    // the registry ROW is left STALE (the codex daemon is a generic ws server that
    // does NOT own/remove its qd row; only `qd stop` tombstones it).
    //   - The abrupt `kill -9 -- -<pgid>` (a TRUE crash: pure SIGKILL, no SIGTERM
    //     grace window). The `--` STOPS option parsing so the leading-dash numeric
    //     `-<pgid>` is an operand (process group), not misparsed option flags —
    //     codex-lead-3 verified this reaps the group. The v1 bug was the MISSING `--`
    //     (`kill -9 -<pgid>` → exit 0 but the process survives, the misparse).
    //   - reap() follows as the proven belt: the production `-pgid` group-kill
    //     (RealDaemonSpawner.kill) guarantees the daemon is down even if the external
    //     `kill` form ever varies. NEITHER path tombstones the row → row left STALE.
    let pgid_a = pgid_of(pid_a).unwrap_or(pid_a);
    let _ = Command::new("kill").args(["-9", "--", &format!("-{pgid_a}")]).status();
    reap(pid_a);
    wait_dead(pid_a);

    // AT SOURCE (the stale-row premise the no-stale-latch assertion rests on): the
    // crash bypassed `qd stop`, so the registry row is STALE — still a live
    // `<pid>.json` carrying pid_a + endpoint_a, NOT tombstoned / cleared.
    let post_crash = codex_rows(&jail);
    let stale = post_crash
        .iter()
        .find(|r| r.session_id.as_deref() == a.session_id.as_deref())
        .expect("crashed daemon's row is STILL PRESENT (stale, not tombstoned)");
    assert_eq!(stale.pid, Some(pid_a), "stale row still carries the dead pid_a (not cleared)");
    assert_eq!(
        stale.endpoint.as_deref(),
        Some(endpoint_a.as_str()),
        "stale row still carries the dead endpoint_a (reap() left it stale, qd stop never tombstoned it)"
    );
    let stale_confirmed = stale.pid == Some(pid_a) && stale.endpoint.as_deref() == Some(endpoint_a.as_str());

    // NO STALE-ENDPOINT LATCH (the load-bearing subtle-wrongness assertion):
    //  - the crashed pid is dead;
    //  - the stale endpoint no longer answers;
    //  - the PRODUCTION reconnect re-check rejects the dead daemon (no live cmdline
    //    carrying the endpoint → cmdline_is_our_daemon FALSE) → a reconnect would
    //    NOT latch the stale endpoint.
    assert!(!is_pid_alive(pid_a as i32), "crashed daemon pid is dead");
    assert!(!ws_initialize_ok(&endpoint_a), "stale endpoint no longer answers (dead)");
    let dead_cmdline = proc_args(pid_a); // empty — pid gone.
    let stale_cmdline_arg = if dead_cmdline.is_empty() { None } else { Some(dead_cmdline.as_str()) };
    assert!(
        !cmdline_is_our_daemon(stale_cmdline_arg, Some(endpoint_a.as_str())),
        "reconnect re-check REJECTS the crashed daemon → no stale-endpoint latch"
    );

    // REBIND — a fresh daemon re-addresses correctly (new live endpoint).
    let b = start_codex(&jail, "chaos2b");
    let pid_b = b.pid.expect("pid");
    let endpoint_b = b.endpoint.clone().expect("endpoint");
    let port_b = endpoint_port(&endpoint_b);
    pids.lock().unwrap().push(pid_b);
    assert_ne!(pid_b, pid_a, "rebind is a NEW daemon (distinct pid)");
    assert!(is_pid_alive(pid_b as i32), "rebound daemon is alive");
    assert!(ws_initialize_ok(&endpoint_b), "rebound endpoint answers (correct re-addressing)");
    let cmdline_b = proc_args(pid_b);
    assert!(
        cmdline_is_our_daemon(Some(cmdline_b.as_str()), Some(endpoint_b.as_str())),
        "reconnect re-check ACCEPTS the rebound daemon"
    );
    // Exactly one LIVE codex daemon (the rebound B; the crashed A is dead).
    let live_rows = codex_rows(&jail)
        .into_iter()
        .filter(|r| r.pid.map(|p| is_pid_alive(p as i32)).unwrap_or(false))
        .count();
    assert_eq!(live_rows, 1, "one live codex daemon after rebind (crashed A not counted)");

    // PORT — CHARACTERIZED, not asserted (ephemeral allocator re-rolls; --port parked).
    ev_text(
        &bundle,
        "crash-rebind-at-source.txt",
        &format!(
            "A: pid={pid_a} endpoint={endpoint_a} port={port_a} pgid={pgid_a}\n\
             crash: abrupt `kill -9 -- -{pgid_a}` (+ reap belt), bypassing qd stop → STALE row\n\
             STALE-ROW AT SOURCE: row still present, carries pid_a + endpoint_a (NOT tombstoned) = {stale_confirmed}\n\
             no-stale-latch: A pid dead = true; A endpoint answers = false; reconnect re-check on crashed A = FALSE\n\
             B (rebind): pid={pid_b} endpoint={endpoint_b} port={port_b}\n\
             re-addressing: B endpoint answers = true; reconnect re-check on B = TRUE\n\
             live codex daemons after rebind = 1\n\
             PORT CHARACTERIZATION: port_a={port_a} port_b={port_b} same={} \
             (ephemeral re-roll; --port parked → 'same port' not forced; observed pinned)\n",
            port_a == port_b
        ),
    );

    let _ = run_qd(&jail, &["stop", "chaos2b"]);
    wait_dead(pid_b);
    assert!(!jail_codex_daemon_alive(&codex_home), "no survivor after stop");
    *pids.lock().unwrap() = Vec::new();
    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// CLASS 3 — daemon-gone wait-fallback (always-on, CONNECTIONLESS). Fabricate a
// rollout at transcript_path() + a dead-pid/no-endpoint row; `qd wait` must derive
// status from the durable rollout tail (NO socket, NO hang). Idle tail → resolves
// before the deadline; Busy tail → polls to the deadline (no false-Idle, no
// infinite hang) — the rollout is the truth even when the daemon is gone.
// ===========================================================================

#[test]
#[ignore = "destructive chaos — never auto-run; deliberate only: cargo test -- --ignored (+ QD_CODEX_LIVE=1)"]
fn chaos_daemon_gone_wait_fallback() {
    let jail = make_jail("chaos3");
    let codex_home = jail.join("codex-home");
    let bundle = chaos_bundle("class3-wait-fallback");

    // --- IDLE tail: a balanced rollout → wait resolves Idle from the tail. ---
    let uuid_idle = "019ea0b3-04d3-7400-8d95-f55d41e961e4";
    let idle_body = [
        r#"{"timestamp":"2026-06-29T12:00:00Z","type":"session_meta","payload":{"id":"019ea0b3-04d3-7400-8d95-f55d41e961e4","cwd":"/jail/work"}}"#,
        r#"{"timestamp":"2026-06-29T12:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"T1"}}"#,
        r#"{"timestamp":"2026-06-29T12:00:02Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"T1"}}"#,
    ]
    .join("\n")
        + "\n";
    let p_idle = write_rollout_in_tree(&codex_home, uuid_idle, &idle_body);
    assert_eq!(
        derive_status(&read_lines(&p_idle)),
        Some(SessionStatus::Idle),
        "AT SOURCE: balanced rollout → Idle (the tail truth qd wait must read)"
    );
    let sdir = sessions_dir(&jail);
    std::fs::create_dir_all(&sdir).unwrap();
    let row_idle = RegistryEntry {
        pid: Some(dead_pid()),
        session_id: Some(uuid_idle.into()),
        provider: Some("codex".into()),
        name: Some("chaos3idle".into()),
        cwd: Some("/jail/work".into()),
        ..Default::default()
    };
    write_entry(&sdir, &row_idle).expect("write idle row");

    // The codex wait path is EITHER the entry-gate short-circuit (resolved status
    // Idle from the connectionless rollout-tail gather → "<label> is idle", exit 0)
    // OR the run_codex_wait loop (which prints "Waiting for ...") that resolves Idle
    // on the rollout-tail fallback. Both are connectionless and BOTH exit 0; assert
    // the robust outcome (success + resolved-before-deadline), not a path-specific
    // message — the rollout tail is the truth even with the daemon gone.
    let t0 = Instant::now();
    let w_idle = run_qd(&jail, &["wait", "chaos3idle", "--timeout", "15"]);
    let dur_idle = t0.elapsed();
    let idle_err = String::from_utf8_lossy(&w_idle.stderr).into_owned();
    let idle_out = String::from_utf8_lossy(&w_idle.stdout).into_owned();
    assert!(!idle_err.contains("panicked"), "qd wait must not panic (idle); stderr:\n{idle_err}");
    assert!(
        w_idle.status.success(),
        "daemon-gone wait RESOLVED Idle from the rollout tail (exit 0); stdout={idle_out:?} stderr={idle_err:?}"
    );
    assert!(
        dur_idle < Duration::from_secs(13),
        "Idle resolved from the rollout tail BEFORE the 15s deadline (connectionless, no socket): {dur_idle:?}"
    );

    // --- BUSY tail: an open turn → wait polls to the deadline (no false-Idle). ---
    let uuid_busy = "019e9f3b-deea-7392-9861-b5d8ad376e2b";
    let busy_body = [
        r#"{"timestamp":"2026-06-29T12:00:00Z","type":"session_meta","payload":{"id":"019e9f3b-deea-7392-9861-b5d8ad376e2b","cwd":"/jail/work"}}"#,
        r#"{"timestamp":"2026-06-29T12:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"OPEN-NO-COMPLETE"}}"#,
    ]
    .join("\n")
        + "\n";
    let p_busy = write_rollout_in_tree(&codex_home, uuid_busy, &busy_body);
    assert_eq!(
        derive_status(&read_lines(&p_busy)),
        Some(SessionStatus::Busy),
        "AT SOURCE: open-turn rollout → Busy (qd wait must NOT false-resolve Idle)"
    );
    let row_busy = RegistryEntry {
        pid: Some(dead_pid()),
        session_id: Some(uuid_busy.into()),
        provider: Some("codex".into()),
        name: Some("chaos3busy".into()),
        cwd: Some("/jail/work".into()),
        ..Default::default()
    };
    write_entry(&sdir, &row_busy).expect("write busy row");

    // Busy tail: the entry gate does NOT short-circuit (status is not Idle), so it
    // enters run_codex_wait, polls the rollout tail (Busy, daemon gone), and TIMES
    // OUT at ~2s → NON-success exit. This proves it read Busy (no false-Idle) AND
    // honored the timeout (no infinite hang on a gone daemon).
    let t1 = Instant::now();
    let w_busy = run_qd(&jail, &["wait", "chaos3busy", "--timeout", "2"]);
    let dur_busy = t1.elapsed();
    let busy_err = String::from_utf8_lossy(&w_busy.stderr).into_owned();
    assert!(!busy_err.contains("panicked"), "qd wait must not panic (busy); stderr:\n{busy_err}");
    assert!(
        !w_busy.status.success(),
        "Busy tail must NOT false-resolve Idle — it times out (non-success): stderr={busy_err:?}"
    );
    assert!(
        dur_busy >= Duration::from_millis(1500),
        "Busy tail polled to the ~2s deadline (read Busy, not false-Idle): {dur_busy:?}"
    );
    assert!(
        dur_busy < Duration::from_secs(12),
        "Busy tail HONORED the timeout — no infinite hang on a gone daemon: {dur_busy:?}"
    );

    ev_text(
        &bundle,
        "wait-fallback-at-source.txt",
        &format!(
            "IDLE: rollout balanced → derive_status=Idle; qd wait --timeout 15 resolved in {dur_idle:?} \
             (< deadline → read the rollout tail, no socket); exit={:?}\n\
             BUSY: rollout open-turn → derive_status=Busy; qd wait --timeout 2 returned in {dur_busy:?} \
             (polled to deadline, no false-Idle, no infinite hang); exit={:?}\n\
             Both rows: dead pid, NO endpoint → straight to the rollout-tail channel (daemon-gone).\n",
            w_idle.status.code(),
            w_busy.status.code(),
        ),
    );
    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// CLASS 4a — pgid-teardown reaps the in-group npm-launcher child (gated, NON-
// VACUOUS). The npm 2-proc launcher's vendored `codex` child shares the daemon's
// pgid; the production `-pgid` teardown must reap it. A wrapper-only kill would
// orphan the child — asserting the child dies is the load-bearing -pgid proof.
// ===========================================================================

#[test]
#[ignore = "destructive chaos — never auto-run; deliberate only: cargo test -- --ignored (+ QD_CODEX_LIVE=1)"]
fn chaos_pgid_teardown_reaps_ingroup_child() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping class-4a in-group teardown (needs a live daemon)");
        return;
    }
    let jail = make_jail("chaos4a");
    let codex_home = jail.join("codex-home");
    let bundle = chaos_bundle("class4a-pgid-ingroup");
    ev_text(&bundle, "RAN.txt", "class-4a in-group teardown ran live\n");
    let pids = Arc::new(Mutex::new(Vec::<i64>::new()));
    let _belt = ReapAll(pids.clone());

    let row = start_codex(&jail, "chaos4a");
    let pid = row.pid.expect("pid");
    pids.lock().unwrap().push(pid);

    // BEFORE: the vendored codex child (pgrep -x codex ∩ this jail's CODEX_HOME) is
    // live and shares the daemon's pgid (the npm 2-proc launcher shape).
    let before = jail_codex_pids(&codex_home);
    assert!(!before.is_empty(), "the vendored codex child is live before teardown");
    let daemon_pgid = pgid_of(pid);
    let child_pgids: Vec<Option<i64>> = before.iter().map(|&p| pgid_of(p)).collect();
    ev_proctree(&bundle, "proctree-before.txt", &codex_home);

    // TEARDOWN — the production `-pgid` SIGTERM→SIGKILL group kill.
    let stop = run_qd(&jail, &["stop", "chaos4a"]);
    assert!(stop.status.success(), "qd stop succeeds: {}", String::from_utf8_lossy(&stop.stderr));
    wait_dead(pid);

    // AT SOURCE: the in-group vendored child is REAPED (load-bearing -pgid; a
    // wrapper-only kill would have orphaned it). Zero survivors for this jail.
    let after = jail_codex_pids(&codex_home);
    assert!(
        after.is_empty(),
        "the -pgid teardown reaped the in-group vendored child — zero survivors, got {after:?}"
    );
    assert!(!is_pid_alive(pid as i32), "daemon pid dead");
    assert!(!jail_codex_daemon_alive(&codex_home), "no codex app-server survivor (env-scoped)");
    ev_proctree(&bundle, "proctree-after.txt", &codex_home);

    ev_text(
        &bundle,
        "pgid-ingroup-at-source.txt",
        &format!(
            "daemon pid={pid} pgid={daemon_pgid:?}\n\
             vendored codex children before = {before:?} (pgids {child_pgids:?}, share the daemon pgid)\n\
             after -pgid teardown: jail codex pids = {after:?} (EMPTY — in-group child reaped)\n\
             load-bearing -pgid CONFIRMED non-vacuously (wrapper-only kill would orphan the child).\n"
        ),
    );

    *pids.lock().unwrap() = Vec::new();
    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// CLASS 4b — escaping-orphan REACH-BOUNDARY characterization (always-on). A
// descendant that setsids OUT of the root's process group is OUTSIDE the `-pgid`
// reach BY CONSTRUCTION. We model the daemon's spawn (own pgid via
// process_group(0)) + a descendant that setsids away (the realistic codex escape
// vector), PROVE the escape at source (root_pgid != escapee_pgid), apply the
// PRODUCTION kill ladder to the root's group, and PIN the boundary: the in-group
// root is reaped; the escaped descendant is NOT reached (kernel-guaranteed — a
// -pgid signal cannot reach a different group). This is a CHARACTERIZATION (the
// tier-a oracle rules whether an escaped descendant is in/out of threat model),
// NOT a vacuous "zero survivors" RED nor a presumed "must die".
// ===========================================================================

#[test]
#[ignore = "destructive chaos — never auto-run; deliberate only: cargo test -- --ignored (+ QD_CODEX_LIVE=1)"]
fn chaos_pgid_escape_reach_boundary_characterization() {
    let bundle = chaos_bundle("class4b-pgid-escape");
    let reap_pids = Arc::new(Mutex::new(Vec::<i64>::new()));
    let _reaper = GroupReaper(reap_pids.clone());

    // The escapee writes its OWN pid (a new session/group leader after setsid) here.
    let esc_pidfile = bundle.join("escapee.pid");
    let _ = std::fs::remove_file(&esc_pidfile);

    // Model root: its own process group (process_group(0), mimicking the daemon
    // spawn). Inside, `setsid sh -c '...'` forks a NEW-session descendant (the
    // escapee) that records its pid then execs a long sleep; the root also sleeps.
    let script = format!(
        "setsid sh -c 'echo $$ > \"{pf}\"; exec sleep 600' & sleep 600",
        pf = esc_pidfile.display()
    );
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&script);
    cmd.process_group(0); // root becomes its OWN group leader (root_pgid == root_pid).
    let root = cmd.spawn().expect("spawn model root");
    let root_pid = root.id() as i64;
    reap_pids.lock().unwrap().push(root_pid);

    // Capture the escapee pid (it writes after setsid detaches it).
    let mut escapee_pid = None;
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&esc_pidfile) {
            if let Ok(p) = s.trim().parse::<i64>() {
                escapee_pid = Some(p);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let escapee_pid = escapee_pid.expect("escapee descendant wrote its pid");
    reap_pids.lock().unwrap().push(escapee_pid);

    // PROVE THE ESCAPE AT SOURCE — non-vacuity: the descendant is in a DIFFERENT
    // process group than the root (so a -pgid kill on the root cannot reach it).
    let root_pgid = pgid_of(root_pid).expect("root pgid");
    let escapee_pgid = pgid_of(escapee_pid).expect("escapee pgid");
    assert_ne!(
        root_pgid, escapee_pgid,
        "ESCAPE PROVEN: the setsid descendant is in a different pgid than the root (non-vacuous)"
    );
    assert!(is_pid_alive(escapee_pid as i32), "escapee alive before teardown");
    assert!(is_pid_alive(root_pid as i32), "root alive before teardown");

    // TEARDOWN — the PRODUCTION kill ladder (SIGTERM→grace→SIGKILL by -pgid) on the
    // root's group, the exact ladder qd stop drives against a codex daemon.
    reap(root_pid);
    wait_dead(root_pid);

    // PIN THE REACH BOUNDARY (kernel-guaranteed, then oracle-ruled):
    //  - the in-group root IS reaped (the teardown reached its own group);
    //  - the escaped descendant is NOT reached (a -pgid signal cannot cross groups).
    assert!(!is_pid_alive(root_pid as i32), "in-group root reaped by the -pgid teardown");
    let escapee_survived = is_pid_alive(escapee_pid as i32);
    assert!(
        escapee_survived,
        "REACH BOUNDARY: the setsid-escaped descendant (pgid {escapee_pgid} != root pgid {root_pgid}) \
         is NOT reached by the -pgid teardown — characterized, not a presumed kill"
    );

    ev_text(
        &bundle,
        "pgid-escape-at-source.txt",
        &format!(
            "root pid={root_pid} pgid={root_pgid} (own group — mimics daemon process_group(0))\n\
             escapee pid={escapee_pid} pgid={escapee_pgid} (setsid → new session/group)\n\
             ESCAPE PROVEN: root_pgid {root_pgid} != escapee_pgid {escapee_pgid} (non-vacuous)\n\
             production -pgid teardown applied to the root group:\n\
               in-group root reaped = {} (teardown reached its own group)\n\
               escaped descendant survived = {escapee_survived} (REACH BOUNDARY — a -pgid signal \
               cannot cross groups; a setsid-escaped descendant is out of the -pgid reach)\n\
             CHARACTERIZATION: per impl-plan §4.1 the -pgid teardown is over-provisioned for the \
             IN-pgid npm-launcher child (class 4a confirms it dies); a child that ESCAPES via setsid \
             is plausibly out-of-threat-model — the tier-a oracle rules whether this boundary is correct. \
             NOT presumed to die, NOT presumed to survive: PINNED at source.\n",
            !is_pid_alive(root_pid as i32),
        ),
    );

    // Cleanup — reap the escapee (the GroupReaper belt also covers a panic).
    // `--` + pid>1 guard: never let `-{escapee_pid}` misparse into kill(-1) (kill-all).
    if escapee_pid > 1 {
        let _ = Command::new("kill").args(["-9", "--", &format!("-{escapee_pid}")]).status();
        let _ = Command::new("kill").args(["-9", "--", &escapee_pid.to_string()]).status();
    }
    wait_dead(escapee_pid);
    *reap_pids.lock().unwrap() = Vec::new();
}
