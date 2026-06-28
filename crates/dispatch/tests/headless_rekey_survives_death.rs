//! WP-B5-ii-b PROOF 2 — re-key-survives-death: the child-pid-keyed identity row
//! SURVIVES a daemon kill + respawn.
//!
//! B5-i made identity = the daemon-minted, CHILD-pid-keyed `<pid>.json` row.
//! BECAUSE it is keyed on the claude CHILD pid (not the daemon's
//! `std::process::id()`), the row survives the daemon's pid CHANGING on respawn —
//! that is what "re-key-survives-death" means (the keying CHOICE survives; the
//! respawned daemon performs no re-write — the §H.6 starttime-CAS forbids it from
//! clobbering a foreign incarnation's row anyway).
//!
//! This drives the REAL `qd` binary against a JAILED HOME with a real per-session
//! qrmux daemon (ONLY `claude` is faked; the fixture HOLDS busy so the claude child
//! stays alive across the kill+respawn — the live orphan). We:
//!   start --headless → mint the child-pid row → SIGKILL the owning daemon (orphan
//!   claude stays alive, row persists) → RESPAWN the daemon → assert EXACTLY ONE
//!   row, the SAME child-pid `<P>.json` (same pid + sessionId + qdId; NOT a
//!   daemon-pid row, NOT a duplicate), its pid the LIVE orphan claude child, and it
//!   is still RESOLVABLE by `qd ls`/`qd connect` by id AND name.
//!
//!   cargo test -p qd --test headless_rekey_survives_death -- --ignored --nocapture
//!
//! Per the lead ruling: addressability is asserted at ROW-RESOLUTION level (found by
//! id + name, single child-pid-keyed row, CAS-protected) — NOT a live Observe
//! re-stream of the post-respawn orphan (that re-attach is the out-of-scope costlier
//! bucket; the respawned daemon booted bare and is not driving the orphan's
//! dead-with-D1 stdout pipe).
//!
//! FIX-SHAPED MUTATION (red-before): in `daemon_headless.rs` `resolve`, key the
//! mint on `std::process::id() as i64` (the DAEMON pid) instead of the `child_pid`
//! the factory is handed → the row is daemon-pid-keyed → after SIGKILL the row's
//! pid is the DEAD daemon (not the live claude child) → the "surviving row's pid is
//! the live orphan claude child" assert reds (addressability did not survive death).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

const SESSION: &str = "hlrk";
const SID: &str = "fa4ec110-0000-4000-8000-0000000000c1";

/// A fake `claude -p` that mints a busy row then HOLDS busy for `QD_FAKE_BUSY_SECS`
/// (long — the whole kill+respawn+assert runs while it is still sleeping = the live
/// orphan child).
fn write_fixture(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake_claude.sh");
    let body = format!(
        "#!/bin/bash\n\
         sleep 0.3\n\
         echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{SID}\"}}'\n\
         echo '{{\"type\":\"assistant\",\"session_id\":\"{SID}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}'\n\
         sleep \"${{QD_FAKE_BUSY_SECS:-60}}\"\n\
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
    fn qrmux_dir(&self) -> PathBuf {
        self.xdg.join("qrmux")
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd(args).output().expect("spawn qd")
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(qd_bin());
        c.args(args)
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.xdg)
            .env("CLAUDE_BIN", &self.fixture)
            .env("QD_FAKE_BUSY_SECS", self.busy_secs.to_string())
            .env_remove("QD_HOME")
            .env_remove("QD_MUX")
            .env_remove("CLAUDE_CODE_SESSION_ID");
        c
    }
    /// Respawn the per-session daemon exactly as the embedder spec does:
    /// `qd qrmux-server --socket-dir <qrmux_dir> --session <name>`, detached.
    fn respawn_daemon(&self) -> std::process::Child {
        self.cmd(&[
            "qrmux-server",
            "--socket-dir",
            self.qrmux_dir().to_str().unwrap(),
            "--session",
            SESSION,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("respawn qrmux-server")
    }
}

fn ls_rows(stdout: &str) -> Vec<serde_json::Value> {
    serde_json::from_str::<Vec<serde_json::Value>>(stdout).unwrap_or_default()
}

fn ls_find(stdout: &str, name: &str) -> Option<serde_json::Value> {
    ls_rows(stdout)
        .into_iter()
        .find(|r| r.get("name").and_then(|v| v.as_str()) == Some(name))
}

fn pid_alive(pid: i64) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn proc_cmdline(pid: i64) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

fn find_daemon_pid(session: &str) -> Option<i64> {
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i64>().ok())
        else {
            continue;
        };
        let cmd = proc_cmdline(pid);
        if cmd.contains("qrmux-server") && cmd.contains(session) {
            return Some(pid);
        }
    }
    None
}

fn socket_connectable(qrmux_dir: &Path, session: &str) -> bool {
    std::os::unix::net::UnixStream::connect(qrmux_dir.join(format!("{session}.sock"))).is_ok()
}

fn kill9(pid: i64) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

/// Live (non-tombstoned) `<pid>.json` registry rows whose `name` == session.
fn live_row_files(sessions_dir: &Path, name: &str) -> Vec<(i64, serde_json::Value)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(sessions_dir) else {
        return out;
    };
    for dent in rd.flatten() {
        let fname = dent.file_name();
        let fname = fname.to_string_lossy();
        // live rows only: "<pid>.json" (exclude "<pid>.json.tombstoned").
        let Some(stem) = fname.strip_suffix(".json") else {
            continue;
        };
        let Ok(pid) = stem.parse::<i64>() else {
            continue;
        };
        let Ok(txt) = std::fs::read_to_string(dent.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
            continue;
        };
        if v.get("name").and_then(|n| n.as_str()) == Some(name) {
            out.push((pid, v));
        }
    }
    out
}

#[test]
#[ignore = "spawns real qd subprocesses + a detached daemon + SIGKILL/respawn + sleeps; run explicitly with --ignored --nocapture"]
fn rekey_survives_daemon_kill_and_respawn() {
    let root = tempfile::tempdir().unwrap();
    let j = jail(root.path(), 60);
    let sessions_dir = j.sessions_dir();
    let qrmux_dir = j.qrmux_dir();

    // --- mint the child-pid-keyed headless row (the B5-i identity path) ---------
    let start = j.run(&["start", SESSION, "--headless", "-p", "hi"]);
    assert_eq!(
        start.status.code(),
        Some(0),
        "qd start --headless exits 0; stderr={}",
        String::from_utf8_lossy(&start.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let (child_pid, qd_id) = loop {
        assert!(
            Instant::now() < deadline,
            "minted row never appeared within 8s"
        );
        let ls = j.run(&["ls", "--json"]);
        if ls.status.code() == Some(0) {
            let stdout = String::from_utf8_lossy(&ls.stdout);
            if let Some(row) = ls_find(&stdout, SESSION) {
                let pid = row.get("pid").and_then(|v| v.as_i64()).unwrap_or(0);
                let sid = row.get("sessionId").and_then(|v| v.as_str());
                if pid != 0 && sid == Some(SID) {
                    let qd_id = row.get("qdId").and_then(|v| v.as_str()).map(str::to_string);
                    break (pid, qd_id);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // The minted row's pid is the LIVE claude CHILD (child-pid keyed, not the daemon).
    assert!(
        pid_alive(child_pid),
        "the minted row's pid is a live process"
    );
    assert!(
        proc_cmdline(child_pid).contains("fake_claude.sh"),
        "the minted row's pid is the claude CHILD (identity on the child, not the daemon); \
         cmdline={:?}",
        proc_cmdline(child_pid)
    );
    let qd_id = qd_id.expect("the minted row carries a stable qdId");
    println!("[mint] child_pid={child_pid} sessionId={SID} qdId={qd_id}");

    // --- SIGKILL the owning per-session daemon (D1) -----------------------------
    let d1 = find_daemon_pid(SESSION).expect("owning qrmux-server daemon found in /proc");
    assert_ne!(
        d1, child_pid,
        "the daemon is a DIFFERENT process from the claude child"
    );
    println!("[kill] daemon D1={d1} (claude child={child_pid})");
    kill9(d1);
    let down = Instant::now() + Duration::from_secs(10);
    while socket_connectable(&qrmux_dir, SESSION) {
        assert!(
            Instant::now() < down,
            "daemon socket still accepting after SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // The disease precondition: the orphaned claude child is STILL ALIVE, and its
    // child-pid-keyed row persists on disk.
    assert!(
        pid_alive(child_pid),
        "the orphaned claude child survives the daemon death"
    );
    assert!(
        sessions_dir.join(format!("{child_pid}.json")).exists(),
        "the child-pid-keyed row file persists across daemon death"
    );
    println!("[killed] D1 down; orphan child {child_pid} alive; row <{child_pid}>.json persists");

    // --- RESPAWN the per-session daemon (D2, a NEW pid) -------------------------
    let mut d2_child = j.respawn_daemon();
    let up = Instant::now() + Duration::from_secs(10);
    while !socket_connectable(&qrmux_dir, SESSION) {
        assert!(Instant::now() < up, "respawned daemon socket never came up");
        std::thread::sleep(Duration::from_millis(100));
    }
    let d2 = find_daemon_pid(SESSION).expect("respawned qrmux-server daemon found in /proc");
    assert_ne!(d2, d1, "the respawned daemon has a NEW pid (D1 was killed)");
    assert_ne!(
        d2, child_pid,
        "the respawned daemon is not the claude child"
    );
    println!("[respawn] daemon D2={d2} up (D1 was {d1})");

    // === SURVIVAL: exactly ONE row, the SAME child-pid `<P>.json` ===============
    let rows = live_row_files(&sessions_dir, SESSION);
    assert_eq!(
        rows.len(),
        1,
        "exactly ONE live row survives kill+respawn (no daemon-pid row, no duplicate); rows={:?}",
        rows.iter().map(|(p, _)| *p).collect::<Vec<_>>()
    );
    let (row_pid, row) = &rows[0];
    assert_eq!(
        *row_pid, child_pid,
        "the surviving row is the SAME child-pid-keyed row <{child_pid}>.json"
    );
    // The surviving row's pid is the LIVE orphan claude child — identity survived
    // BECAUSE it was keyed on the CHILD, not the (now-dead) daemon. This is the
    // assertion the daemon-pid mint mutation reds.
    assert!(
        pid_alive(child_pid),
        "the surviving row's pid is a LIVE process (the orphan claude child) — identity \
         survived the daemon death; a daemon-pid-keyed row would point at the DEAD daemon D1={d1}"
    );
    assert!(
        proc_cmdline(child_pid).contains("fake_claude.sh"),
        "the surviving row's pid is the claude CHILD (cmdline={:?}), never the daemon",
        proc_cmdline(child_pid)
    );
    // Same identity: sessionId + qdId unchanged across kill+respawn.
    assert_eq!(
        row.get("sessionId").and_then(|v| v.as_str()),
        Some(SID),
        "the surviving row keeps its recorded sessionId"
    );

    // --- still RESOLVABLE by `qd ls`/`qd connect` by id AND name ----------------
    let ls2 = j.run(&["ls", "--json"]);
    let ls2_out = String::from_utf8_lossy(&ls2.stdout);
    let ls2_row = ls_find(&ls2_out, SESSION).expect("row still listed by name after respawn");
    assert_eq!(
        ls2_row.get("sessionId").and_then(|v| v.as_str()),
        Some(SID),
        "post-respawn row resolvable by id AND name"
    );
    assert_eq!(
        ls2_row.get("qdId").and_then(|v| v.as_str()),
        Some(qd_id.as_str()),
        "post-respawn row keeps the SAME stable qdId (identity survived)"
    );
    // connect RESOLVES the target by id AND name (resolution, not a live re-stream).
    for (label, target) in [("name", SESSION), ("id", SID)] {
        let c = j.run(&["connect", target]);
        let c_err = String::from_utf8_lossy(&c.stderr);
        assert!(
            !c_err.contains("No session matching"),
            "qd connect by {label} must RESOLVE the surviving row (not 'No session matching'); \
             stderr={c_err}"
        );
        println!("[connect {label}] resolved (code={:?})", c.status.code());
    }
    println!("RE-KEY SURVIVES DAEMON KILL + RESPAWN: PASS");

    // --- teardown: kill D2 + the orphan child -----------------------------------
    if let Some(d2_now) = find_daemon_pid(SESSION) {
        kill9(d2_now);
    }
    let _ = d2_child.kill();
    let _ = d2_child.wait();
    kill9(child_pid);
}
