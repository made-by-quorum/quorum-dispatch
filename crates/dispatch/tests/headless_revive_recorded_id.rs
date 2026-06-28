//! WP-B5-ii-b PROOF 1 — connect→Cold→revive resumes via the RECORDED sessionId.
//!
//! The durability half of identity, the one gap the B5-i (D) round-trip never
//! exercised: (D) proved live-Observe addressability, but NOT the COLD revive
//! path (there is no exit-1 gate there, so it was carried forward, not closed).
//!
//! Drives the REAL `qd connect` binary end-to-end against a JAILED HOME — ONLY
//! `claude` is faked (a fixture script via `CLAUDE_BIN` that LOGS its argv on
//! every invocation, so the revive spawn's exact `--resume <id>` fragment is
//! observable). The COLD headless row is shaped EXACTLY like the B5-i
//! daemon-minted row (`daemon_status::MintIdentity`: child-pid-keyed,
//! `entrypoint:"headless"`, NO `provider` field, recorded `sessionId`) — a
//! faithful stand-in for "a minted row that has gone quiet". A fresh jail (no
//! lingering per-session daemon / pane) is deliberate: the revive must COLD-START
//! and genuinely re-spawn `claude --resume <recorded-id>`, not reattach a stale
//! pane / confirm boot off a still-live daemon.
//!
//!   cargo test -p qd --test headless_revive_recorded_id -- --ignored --nocapture
//!
//! Proves: `qd connect` on the COLD row → `resolve_target_mode(headless,
//! pid_alive=false)` → `TargetMode::Cold` → `revive_claude`, which spawns
//! `claude --resume <recorded-SID>` (the recorded id is load-bearing — a resumed
//! session, never a fresh one).
//!
//! FIX-SHAPED MUTATION (red-before): in `revive_resume_args` (resume.rs) replace
//! `id: &session.session_id` with `id: ""` → the revive spawns `--resume` with an
//! EMPTY id (a fresh claude session, not the recorded one) → the
//! `--resume <recorded-SID>` assert reds (the revived fixture echoes a DIFFERENT /
//! empty session_id — lost identity continuity).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

const SESSION: &str = "hlrev";
/// The COLD row's recorded `session_id` — what the revive must resume via
/// `--resume <id>`.
const SID: &str = "fa4ec110-0000-4000-8000-0000000000b1";
/// A dead pid for the forged child-pid-keyed row (no `/proc/<pid>` → `pid_alive`
/// false → `TargetMode::Cold`).
const DEAD_PID: i64 = 999_111;

/// A fake `claude` that LOGS its argv to `$QD_ARGV_LOG` on every invocation, then,
/// when revived with `--resume <id>`, records `RESUME <id>` and echoes a
/// `system/init` carrying THAT id (continuity). A `--resume ""` (the mutation)
/// records the empty id — a non-resumed identity. It also stamps an `idle`
/// registry row for its own pid so the revive boot-waiter confirms promptly.
fn write_fixture(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake_claude.sh");
    let body = "#!/bin/bash\n\
         printf '%s\\n' \"$*\" >> \"$QD_ARGV_LOG\"\n\
         resume_id=\"\"\n\
         prev=\"\"\n\
         has_resume=no\n\
         for a in \"$@\"; do\n\
           if [ \"$prev\" = \"--resume\" ]; then resume_id=\"$a\"; fi\n\
           if [ \"$a\" = \"--resume\" ]; then has_resume=yes; fi\n\
           prev=\"$a\"\n\
         done\n\
         if [ \"$has_resume\" = yes ]; then\n\
           printf 'RESUME %s\\n' \"$resume_id\" >> \"$QD_ARGV_LOG\"\n\
           # Confirm boot for the revive waiter: an idle row named for this session.\n\
           printf '{\"pid\":%s,\"name\":\"%s\",\"status\":\"idle\",\"sessionId\":\"%s\"}' \\\n\
             \"$$\" \"$QD_SESSION\" \"$resume_id\" > \"$HOME/.claude/sessions/$$.json\"\n\
           echo \"{\\\"type\\\":\\\"system\\\",\\\"subtype\\\":\\\"init\\\",\\\"session_id\\\":\\\"$resume_id\\\"}\"\n\
           sleep 0.8\n\
           exit 0\n\
         fi\n\
         # (unused fresh path — this test forges the cold row directly)\n\
         sleep 0.3\n"
        .to_string();
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// The COLD, child-pid-keyed headless row, shaped exactly like the B5-i daemon
/// mint: recorded `sessionId`, `entrypoint:"headless"`, NO `provider` field (claude
/// rows carry none; the join defaults absent → claude-code), a DEAD pid.
fn cold_headless_row(cwd: &str) -> String {
    format!(
        r#"{{"pid":{DEAD_PID},"sessionId":"{SID}","cwd":"{cwd}","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"{SESSION}","entrypoint":"headless"}}"#
    )
}

struct Jail {
    home: PathBuf,
    xdg: PathBuf,
    fixture: PathBuf,
    argv_log: PathBuf,
}

fn jail(root: &Path) -> Jail {
    let home = root.join("home");
    let xdg = root.join("x");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(home.join(".claude").join("projects")).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    if let Ok(real) = std::env::var("HOME") {
        assert_ne!(
            home,
            PathBuf::from(real),
            "test home must never equal the real HOME"
        );
    }
    // Forge the COLD headless row (cwd = the jail home so revive's F3 reality-check
    // passes — the row points at a real, existing per-session directory).
    let cwd = home.to_string_lossy().into_owned();
    std::fs::write(
        sessions.join(format!("{DEAD_PID}.json")),
        cold_headless_row(&cwd),
    )
    .unwrap();
    let fixture = write_fixture(root);
    let argv_log = root.join("argv.log");
    std::fs::write(&argv_log, b"").unwrap();
    Jail {
        home,
        xdg,
        fixture,
        argv_log,
    }
}

impl Jail {
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(qd_bin())
            .args(args)
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.xdg)
            .env("CLAUDE_BIN", &self.fixture)
            .env("QD_ARGV_LOG", &self.argv_log)
            .env("QD_SESSION", SESSION)
            .env_remove("QD_HOME")
            .env_remove("QD_MUX")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .output()
            .expect("spawn qd")
    }

    fn argv_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.argv_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[test]
#[ignore = "spawns real qd subprocesses + a detached daemon + sleeps; run explicitly with --ignored --nocapture"]
fn connect_cold_revive_resumes_recorded_session_id() {
    let root = tempfile::tempdir().unwrap();
    let j = jail(root.path());

    // Sanity: the COLD row is resolvable by name AND recorded id BEFORE revive.
    let ls = j.run(&["ls", "--all", "--json"]);
    let ls_out = String::from_utf8_lossy(&ls.stdout);
    assert!(
        ls_out.contains(SESSION) && ls_out.contains(SID),
        "the cold headless row is addressable by name + recorded id pre-revive; ls={ls_out}"
    );

    // --- the proof: `qd connect <name>` on the COLD row → revive via recorded id -
    // connect → resolve_target_mode(headless, pid_alive=false) → Cold →
    // revive_claude → spawns `claude --resume <recorded-SID>`. The detached-pane
    // attach tail exits nonzero under the test's no-TTY shell — the load-bearing
    // observation is the revive SPAWN argv (logged by the fixture in run_detached,
    // BEFORE any boot wait / attach), not connect's exit code.
    let connect = j.run(&["connect", SESSION]);
    println!(
        "[connect] code={:?} stdout={} stderr={}",
        connect.status.code(),
        String::from_utf8_lossy(&connect.stdout).trim(),
        String::from_utf8_lossy(&connect.stderr).trim()
    );

    // Give the detached revive pane a moment to exec the fixture + log its argv.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !j.argv_lines().iter().any(|l| l.contains("--resume")) {
        std::thread::sleep(Duration::from_millis(150));
    }

    let lines = j.argv_lines();
    println!("[argv-log]\n{}", lines.join("\n"));
    let revive_argv = lines
        .iter()
        .find(|l| l.contains("--resume"))
        .unwrap_or_else(|| {
            panic!(
                "revive never spawned claude with --resume; argv log:\n{}",
                lines.join("\n")
            )
        });
    assert!(
        revive_argv.contains(&format!("--resume {SID}")),
        "revive must resume the RECORDED session_id (`--resume {SID}`), not a fresh/empty session; \
         revive argv was: {revive_argv:?}"
    );
    // …and the fixture, given that id, echoed the SAME session_id back (continuity).
    assert!(
        lines.iter().any(|l| l == &format!("RESUME {SID}")),
        "the revived fixture must continue the RECORDED session ({SID}), not a fresh one; log:\n{}",
        lines.join("\n")
    );
    println!("CONNECT→COLD→REVIVE RESUMES THE RECORDED SESSION_ID: PASS");

    // Best-effort reap of the detached revive pane's fixture child.
    let _ = Command::new("pkill")
        .args(["-9", "-f", "[f]ake_claude.sh"])
        .status();
}
