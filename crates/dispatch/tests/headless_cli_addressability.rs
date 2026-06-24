//! WP-B5-i (D) — the live-CLI addressability round-trip (the headline DoD).
//!
//! Drives the REAL `sb` binary end-to-end (`sb start --headless` → `sb ls` →
//! `sb connect` by id AND name) against a JAILED HOME, with a real per-session
//! sbmux daemon, a real registry, and a real socket — ONLY `claude` is faked
//! (a fixture script via `CLAUDE_BIN`, the existing `headless_b2b2b` pattern,
//! lifted to the CLI surface). The fixture HOLDS BUSY for a controlled window so
//! the `connect → Observe` dispatch (which is inherently BUSY-window behaviour —
//! a child-pid-keyed headless row whose claude child has exited is correctly
//! Cold→revive) is DETERMINISTIC, not racy. This is `#[ignore]` (it spawns real
//! `sb` subprocesses + a detached daemon + sleeps), run explicitly:
//!
//!   cargo test -p dispatch --test headless_cli_addressability -- --ignored --nocapture
//!
//! Proves (lead's D requirements):
//!  (1) in the held-busy window: `sb connect <name>` AND `sb connect <id>` both
//!      reach OBSERVE (the read-only dashboard), NOT Cold→revive; the raw
//!      registry status flips busy→idle across the turn.
//!  (2) the minted row is ls/resolve-addressable by id AND name — asserted both
//!      DURING busy and AFTER idle (registry-read durability, the seed B5-ii
//!      proves survives daemon death).
//!  (+) child-pid keying at the CLI level: the addressable row's pid is the LIVE
//!      fake-claude child (its /proc cmdline names the fixture), never the daemon.
//!      The strong daemon-pid-key fix-shaped mutation lives in the in-process
//!      `headless_b2b2b::fake_claude_launch_subscribe_and_registry_flip`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn sb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dispatch")
}

const SESSION: &str = "hlcli";
/// The fixture's fixed `system/init` session_id — the minted row's stable id, so
/// the round-trip can `sb connect <id>` deterministically.
const SID: &str = "fa4ec110-0000-4000-8000-000000000001";

/// A fake `claude -p` that mints a busy row, HOLDS busy for `SBX_FAKE_BUSY_SECS`,
/// then emits `result` and EOFs. It ignores the appended `-p ... --output-format
/// ...` argv (a real claude's flags are irrelevant to the addressability path).
fn write_fixture(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake_claude.sh");
    let body = format!(
        "#!/bin/bash\n\
         sleep 0.3\n\
         echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{SID}\"}}'\n\
         echo '{{\"type\":\"assistant\",\"session_id\":\"{SID}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}'\n\
         sleep \"${{SBX_FAKE_BUSY_SECS:-18}}\"\n\
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
    // A SHORT XDG_RUNTIME_DIR keeps the `<dir>/sbmux/<name>.sock` path under the
    // 104-byte sun_path budget `resolve_sbmux_dir` guards.
    let xdg = root.join("x");
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
    std::fs::create_dir_all(home.join(".claude").join("projects")).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    // L9a: never the real HOME.
    if let Ok(real) = std::env::var("HOME") {
        let real = PathBuf::from(real);
        assert_ne!(home, real, "test home must never equal the real HOME");
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

    /// Run `sb <args>` with the jailed env; `current_dir` = the jailed HOME so the
    /// per-session cwd threaded onto `sb start` (WP-B5-i C) is the jail home — a
    /// bonus end-to-end check of (C) through the real CLI.
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

/// Parse `sb ls --json` stdout (a top-level array) and find the row named `name`.
fn ls_find(out_stdout: &str, name: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<Vec<serde_json::Value>>(out_stdout)
        .ok()?
        .into_iter()
        .find(|r| r.get("name").and_then(|v| v.as_str()) == Some(name))
}

/// The raw registry row's `status` field (authoritative — NOT the ls liveness
/// gate, which downgrades a dead pid to `cold` after the child exits).
fn raw_status(sessions_dir: &Path, pid: i64) -> Option<String> {
    let txt = std::fs::read_to_string(sessions_dir.join(format!("{pid}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    v.get("status").and_then(|s| s.as_str()).map(str::to_string)
}

fn pid_alive(pid: i64) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
#[ignore = "spawns real sb subprocesses + a detached daemon + sleeps; run explicitly with --ignored --nocapture"]
fn live_cli_addressability_round_trip_fake_claude() {
    let root = tempfile::tempdir().unwrap();
    let j = jail(root.path(), 18);
    let sessions_dir = j.sessions_dir();

    // --- sb start --headless: launch the headless turn via the real CLI -------
    let start = j.run(&["start", SESSION, "--headless", "-p", "hi"]);
    let s_out = String::from_utf8_lossy(&start.stdout);
    let s_err = String::from_utf8_lossy(&start.stderr);
    assert_eq!(
        start.status.code(),
        Some(0),
        "sb start --headless must exit 0; stdout={s_out} stderr={s_err}"
    );
    assert!(
        s_out.contains("Launched headless session"),
        "expected the launch ack; stdout={s_out}"
    );
    println!("[start] {}", s_out.trim());

    // --- poll `sb ls --json` until the minted row appears (busy) --------------
    let deadline = Instant::now() + Duration::from_secs(8);
    let (pid, mut ls_row) = loop {
        assert!(
            Instant::now() < deadline,
            "minted row never appeared in `sb ls --json` within 8s"
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
    println!("[ls] minted row appeared: pid={pid} status=busy row={ls_row}");

    // (2) addressable by id AND name in the `sb ls` machine surface (DURING busy).
    assert_eq!(
        ls_row.get("name").and_then(|v| v.as_str()),
        Some(SESSION),
        "ls row addressable by name"
    );
    assert_eq!(
        ls_row.get("sessionId").and_then(|v| v.as_str()),
        Some(SID),
        "ls row addressable by stable id (sessionId)"
    );
    // (C bonus) the per-session cwd threaded onto `sb start` landed on the row.
    let home_canon = std::fs::canonicalize(&j.home).unwrap();
    assert_eq!(
        ls_row
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(PathBuf::from),
        Some(home_canon.clone()),
        "minted row records the per-session cwd threaded onto sb start (WP-B5-i C)"
    );

    // (+) CHILD-PID keying at the CLI level: the addressable pid is the LIVE
    // fake-claude child (its /proc cmdline names the fixture), NOT the daemon.
    assert!(
        pid_alive(pid),
        "the minted row's pid must be a live process"
    );
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
    assert!(
        cmdline.contains("fake_claude.sh"),
        "the addressable row pid must be the fake-claude child (identity on claude's \
         process, not the daemon); /proc/{pid}/cmdline was: {cmdline:?}"
    );
    assert_ne!(
        pid,
        std::process::id() as i64,
        "the row is never keyed on the test/launcher pid"
    );

    // (1) connect → OBSERVE in the busy window, BY NAME and BY STABLE ID --------
    for (label, target) in [("name", SESSION), ("id", SID)] {
        let c = j.run(&["connect", target]);
        let c_out = String::from_utf8_lossy(&c.stdout);
        let c_err = String::from_utf8_lossy(&c.stderr);
        assert_eq!(
            c.status.code(),
            Some(0),
            "sb connect by {label} must exit 0; stdout={c_out} stderr={c_err}"
        );
        assert!(
            c_out.contains("Observing headless session"),
            "sb connect by {label} must reach OBSERVE (not Cold→revive); stdout={c_out}"
        );
        assert!(
            !c_out.contains("Revived") && !c_err.contains("Revived"),
            "sb connect by {label} must NOT revive a live headless target; out={c_out} err={c_err}"
        );
        println!(
            "[connect {label}] {}",
            c_out.lines().next().unwrap_or("").trim()
        );
    }

    // --- wait for the turn to complete: raw status flips busy→idle -------------
    let idle_deadline = Instant::now() + Duration::from_secs(40);
    loop {
        assert!(
            Instant::now() < idle_deadline,
            "minted row never flipped busy→idle within 40s"
        );
        if raw_status(&sessions_dir, pid).as_deref() == Some("idle") {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("[status] busy→idle flip observed");

    // (2, durable) AFTER the turn: the row is STILL ls/resolve-addressable by id
    // AND name (registry-read; independent of the busy window). The ls liveness
    // gate may render it `cold` now (the child exited) — addressability is the
    // resolver finding the row, not its liveness class.
    let ls2 = j.run(&["ls", "--json"]);
    let ls2_out = String::from_utf8_lossy(&ls2.stdout);
    ls_row = ls_find(&ls2_out, SESSION)
        .expect("row still listed after the turn (durable addressability)");
    assert_eq!(
        ls_row.get("sessionId").and_then(|v| v.as_str()),
        Some(SID),
        "post-turn row still addressable by id AND name"
    );
    println!("[ls] post-turn row still addressable by id+name: {ls_row}");
    println!("LIVE-CLI ADDRESSABILITY ROUND-TRIP: PASS");
}
