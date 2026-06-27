//! WP-B5-ii-a — the DAEMON-DEATH FAIL-CLOSED integration test (the §H.1 keystone
//! first half; the pre-split plan's `g_daemonkill` slot, realized here as a real
//! headless daemon-kill rather than the retired shared-daemon `c1_gate_inc::rows::
//! g_daemonkill` topology, which asserted a DIFFERENT, superseded blast-radius).
//!
//! Drives the REAL `sb` binary end-to-end against a JAILED HOME (the
//! `headless_cli_addressability` pattern): `sb start --headless` mints a row via a
//! real per-session qrmux daemon, ONLY `claude` is faked (a fixture script via
//! `CLAUDE_BIN` that HOLDS busy for a long window). We then SIGKILL the OWNING
//! per-session daemon (`sb qrmux-server --session <name>`) while the fixture is
//! still in its busy sleep — so the claude child is ORPHANED but STILL ALIVE (the
//! daemon spawns claude on a PIPE, not a controlling PTY, so the daemon's death
//! does not hang up the child; it survives, reparented, until its next write). That
//! is the exact stale-busy precondition the three guarantees must survive.
//!
//! `#[ignore]` (spawns real `sb` subprocesses + a detached daemon + sleeps), run:
//!   cargo test -p dispatch --test headless_daemon_death -- --ignored --nocapture
//!
//! Proves the three §7.5/R2 daemon-death guarantees, each fail-CLOSED:
//!  (i)   CONTROL FAILS CLOSED — `sb wait` NEVER returns " done" after the daemon
//!        dies (no false-done); it exits status-only via timeout/session-exited.
//!  (ii)  STATUS reads daemon-down, NOT stale-busy — `sb ls --json` renders the row
//!        `cold` once the daemon is down, EVEN THOUGH the orphan claude pid is still
//!        alive AND the raw registry row on disk is still the stale `busy`.
//!  (iii) `sb wait` does NOT hang — it returns within a hard wall-clock bound via
//!        one of its three typed exits (Timeout/SessionExited), never blocking past
//!        the deadline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn sb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

const SESSION: &str = "hldk";
const SID: &str = "fa4ec110-0000-4000-8000-0000000000d1";

/// A fake `claude -p` that mints a busy row, HOLDS busy for `SBX_FAKE_BUSY_SECS`
/// (long — the assertions run while it is still sleeping = the live orphan), then
/// would emit `result` and EOF. After the daemon dies its stdout pipe is gone, so
/// the trailing `echo`/`result` simply never reaches a transcript (SIGPIPE) — the
/// daemon that would write `idle`/the transcript is dead, so the row stays busy.
fn write_fixture(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake_claude.sh");
    let body = format!(
        "#!/bin/bash\n\
         sleep 0.3\n\
         echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{SID}\"}}'\n\
         echo '{{\"type\":\"assistant\",\"session_id\":\"{SID}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}'\n\
         sleep \"${{SBX_FAKE_BUSY_SECS:-60}}\"\n\
         echo '{{\"type\":\"result\",\"session_id\":\"{SID}\",\"is_error\":false,\"stop_reason\":\"end_turn\"}}'\n"
    );
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

struct Jail {
    home: PathBuf,
    xdg: PathBuf,
    fixture: PathBuf,
    busy_secs: u64,
}

fn jail(root: &Path, busy_secs: u64) -> Jail {
    let home = root.join("home");
    let xdg = root.join("x");
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
    std::fs::create_dir_all(home.join(".claude").join("projects")).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    if let Ok(real) = std::env::var("HOME") {
        assert_ne!(
            home,
            PathBuf::from(real),
            "test home must never equal the real HOME"
        );
    }
    let fixture = write_fixture(root);
    Jail {
        home,
        xdg,
        fixture,
        busy_secs,
    }
}

impl Jail {
    fn sessions_dir(&self) -> PathBuf {
        self.home.join(".claude").join("sessions")
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(sb_bin())
            .args(args)
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.xdg)
            .env("CLAUDE_BIN", &self.fixture)
            .env("SBX_FAKE_BUSY_SECS", self.busy_secs.to_string())
            .env_remove("SB_HOME")
            .env_remove("SB_MUX")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .output()
            .expect("spawn sb")
    }
}

fn ls_find(out_stdout: &str, name: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<Vec<serde_json::Value>>(out_stdout)
        .ok()?
        .into_iter()
        .find(|r| r.get("name").and_then(|v| v.as_str()) == Some(name))
}

/// The raw registry row's `status` field (authoritative on-disk value — NOT the ls
/// render gate, which is exactly what (ii) downgrades).
fn raw_status(sessions_dir: &Path, pid: i64) -> Option<String> {
    let txt = std::fs::read_to_string(sessions_dir.join(format!("{pid}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    v.get("status").and_then(|s| s.as_str()).map(str::to_string)
}

fn pid_alive(pid: i64) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn proc_cmdline(pid: i64) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

/// Find the OWNING per-session daemon pid by scanning `/proc` for the embedded
/// daemon entry `sb qrmux-server … --session <SESSION>` (daemon.rs). Returns the
/// first match; `None` if no such process exists (already dead).
fn find_daemon_pid(session: &str) -> Option<i64> {
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        let cmd = proc_cmdline(pid);
        if cmd.contains("qrmux-server") && cmd.contains(session) {
            return Some(pid);
        }
    }
    None
}

/// Can we CONNECT to the per-session daemon socket? `true` = a daemon is listening
/// (Up); `false` = ECONNREFUSED / missing (Down) — the exact signal the (ii) gate
/// reads via `SocketDaemonLiveness`.
fn socket_connectable(xdg: &Path, session: &str) -> bool {
    // Mirror resolve_qrmux_dir's embedded layout: $XDG_RUNTIME_DIR/qrmux/<name>.sock.
    let candidates = [
        xdg.join("qrmux").join(format!("{session}.sock")),
        xdg.join(format!("{session}.sock")),
    ];
    candidates
        .iter()
        .any(|p| std::os::unix::net::UnixStream::connect(p).is_ok())
}

fn kill9(pid: i64) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

#[test]
#[ignore = "spawns real sb subprocesses + a detached daemon + sleeps; run explicitly with --ignored --nocapture"]
fn daemon_death_fails_closed_control_status_wait() {
    let root = tempfile::tempdir().unwrap();
    let j = jail(root.path(), 60); // long busy window — assertions run while the orphan is alive
    let sessions_dir = j.sessions_dir();

    // --- sb start --headless --------------------------------------------------
    let start = j.run(&["start", SESSION, "--headless", "-p", "hi"]);
    let s_out = String::from_utf8_lossy(&start.stdout);
    let s_err = String::from_utf8_lossy(&start.stderr);
    assert_eq!(
        start.status.code(),
        Some(0),
        "sb start: stdout={s_out} stderr={s_err}"
    );
    assert!(
        s_out.contains("Launched headless session"),
        "launch ack; stdout={s_out}"
    );

    // --- poll `sb ls --json` until the minted row is busy; capture child pid ---
    let deadline = Instant::now() + Duration::from_secs(8);
    let (pid, busy_row) = loop {
        assert!(
            Instant::now() < deadline,
            "minted busy row never appeared within 8s"
        );
        let ls = j.run(&["ls", "--json"]);
        if ls.status.code() == Some(0) {
            let stdout = String::from_utf8_lossy(&ls.stdout);
            if let Some(row) = ls_find(&stdout, SESSION) {
                let pid = row.get("pid").and_then(|v| v.as_i64()).unwrap_or(0);
                if pid != 0 && raw_status(&sessions_dir, pid).as_deref() == Some("busy") {
                    break (pid, row);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    println!(
        "[mint] pid={pid} entrypoint={:?}",
        busy_row.get("entrypoint")
    );

    // BEFORE (daemon alive): the socket connects AND ls renders the row `busy` —
    // the gate's pass-through path. This is the in-test before/after baseline.
    assert!(
        socket_connectable(&j.xdg, SESSION),
        "daemon socket must be live pre-kill"
    );
    assert_eq!(
        busy_row.get("status").and_then(|v| v.as_str()),
        Some("busy"),
        "pre-kill: daemon alive → ls renders busy (gate passes through)"
    );
    assert!(
        pid_alive(pid) && proc_cmdline(pid).contains("fake_claude.sh"),
        "row pid is the live fake-claude child"
    );

    // --- KILL the owning per-session daemon -----------------------------------
    let daemon =
        find_daemon_pid(SESSION).expect("the owning qrmux-server daemon must be found in /proc");
    assert_ne!(
        daemon, pid,
        "the daemon is a DIFFERENT process from the claude child"
    );
    println!("[kill] daemon pid={daemon} (claude child pid={pid})");
    kill9(daemon);

    // Wait for the daemon to be reaped + its socket to stop accepting connections,
    // bounded. The orphan claude child must remain ALIVE throughout (the disease).
    let down_deadline = Instant::now() + Duration::from_secs(10);
    while socket_connectable(&j.xdg, SESSION) {
        assert!(
            Instant::now() < down_deadline,
            "daemon socket still accepting after SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!pid_alive(daemon), "daemon process reaped");
    assert!(
        pid_alive(pid) && proc_cmdline(pid).contains("fake_claude.sh"),
        "THE DISEASE PRECONDITION: the orphaned claude child is STILL ALIVE after the daemon died"
    );
    // The on-disk registry row is STILL the stale `busy` — nobody flips it now.
    assert_eq!(
        raw_status(&sessions_dir, pid).as_deref(),
        Some("busy"),
        "the raw registry row is still the stale daemon-written busy (the disease)"
    );

    // === (ii) STATUS reads daemon-down, NOT stale-busy ========================
    let ls2 = j.run(&["ls", "--json"]);
    let ls2_out = String::from_utf8_lossy(&ls2.stdout);
    let row2 = ls_find(&ls2_out, SESSION)
        .expect("row still listed after daemon death (durable addressability)");
    assert_eq!(
        row2.get("status").and_then(|v| v.as_str()),
        Some("cold"),
        "(ii) daemon down → ls renders COLD, not the stale busy — even though the \
         orphan claude pid {pid} is still alive and the raw row is still busy. row={row2}"
    );
    println!(
        "[ii] daemon-down → ls status=cold (raw registry still busy, orphan pid {pid} alive): PASS"
    );

    // === (iii) `sb wait` does NOT hang + (i) CONTROL FAILS CLOSED (no false-done) ===
    let wait_start = Instant::now();
    let w = j.run(&["wait", SESSION, "--timeout", "3"]);
    let elapsed = wait_start.elapsed();
    let w_out = String::from_utf8_lossy(&w.stdout);
    let w_err = String::from_utf8_lossy(&w.stderr);
    // (iii) bounded: a 3s wait must return well within a generous hard bound (no hang).
    assert!(
        elapsed < Duration::from_secs(20),
        "(iii) sb wait must not hang past its deadline — took {elapsed:?}"
    );
    // (i) no false-done: it must NOT report " done" / exit 0-as-done after the daemon died.
    assert!(
        !w_err.contains(" done") && !w_out.contains(" done"),
        "(i) sb wait must NEVER report done after daemon death (no false-done); out={w_out} err={w_err}"
    );
    // It exits via a typed status-only exit (timeout or session-exited), exit != 0.
    assert_ne!(
        w.status.code(),
        Some(0),
        "(i)/(iii) daemon-death wait exits non-zero (status-only)"
    );
    assert!(
        w_err.contains(" timeout") || w_err.contains(" session exited"),
        "(iii) wait exits via a typed exit (timeout/session-exited); err={w_err}"
    );
    println!(
        "[i+iii] sb wait returned in {elapsed:?} via{} (no false-done, no hang): PASS",
        w_err.trim_end()
    );

    // Best-effort: reap the orphan so the test leaves no leaked fake-claude.
    kill9(pid);
    println!("DAEMON-DEATH FAIL-CLOSED (control/status/wait): PASS");
}
