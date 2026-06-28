//! WP-B-CS-2-LIVE — the REAL-claude SEED (ruling Q2a, MANDATORY, non-deferrable):
//! prove the live OBSERVE run-loop + turn-boundary cutover EXECUTION ONCE,
//! end-to-end, against a GENUINE live busy headless `claude` window — driving the
//! REAL `qd` binary (so `run_headless_observe` + `revive_claude` run for real), with
//! ONLY the HOME isolated (the `headless_b2b2b` / lib.sh `env -i` contract: a
//! synthetic HOME + CLAUDE_CONFIG_DIR with copied creds, NEVER the live `~/.claude`).
//!
//!   cargo test -p qd --test headless_observe_real_claude_seed -- --ignored --nocapture
//!   (needs network + live claude credentials; isolated tempdir HOME)
//!
//! What it proves on a genuine claude turn (ruling Q2a):
//!  (1) connect→OBSERVE renders CONTROL FACTS ONLY through the REAL socket — the
//!      model's actual reply text (it is told to emit a tripwire token) NEVER appears
//!      in the observe output (§2a hard line, not just the pure fold).
//!  (2) the busy→idle transition is observed live (in-turn: yes → in-turn: no).
//!  (3) choice-2 mid-turn DEFERS ("Cutover queued") and FIRES at the turn boundary
//!      ("Turn boundary reached") — never mid-turn — on the REAL stream.
//!  (4) teardown→`revive_claude` runs FOR REAL; how far the native-TUI/zmx attach
//!      PHYSICALLY drives is recorded EMPIRICALLY (the Q2b boundary) — NOT
//!      pre-conceded.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

const SESSION: &str = "hlseed";
/// The model is INSTRUCTED to emit this token in its reply, so it is real assistant
/// text on the live stream. The §2a tripwire: it must NEVER appear in the read-only
/// observe output (the republish protocol carries no content; the dashboard renders
/// control facts only).
const TRIPWIRE: &str = "SEEDLEAK_TRIPWIRE_b2c2live";

fn real_claude_bin() -> String {
    std::env::var("CLAUDE_BIN")
        .unwrap_or_else(|_| "/home/u/.local/bin/claude".to_string())
}
fn live_creds() -> String {
    std::env::var("LIVE_CREDENTIALS")
        .unwrap_or_else(|_| "/home/u/.claude/.credentials.json".to_string())
}

/// An isolated jail: a synthetic HOME + an isolated CLAUDE_CONFIG_DIR carrying the
/// copied creds + a controlled `.claude.json` (onboarding done, cwd trusted, no MCP).
/// The REAL claude (launched by the daemon with `clear_env=false`) inherits HOME +
/// CLAUDE_CONFIG_DIR from the `qd` process env, so it reads THIS jail — never the
/// live `~/.claude`.
struct Jail {
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
    xdg: PathBuf,
}

fn jail(root: &Path) -> Jail {
    let home = root.join("home");
    let config = root.join("cfg");
    let xdg = root.join("x");
    for d in [&home, &config, &xdg] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
    std::fs::create_dir_all(home.join(".claude").join("projects")).unwrap();
    // L9a belt: never the real HOME.
    if let Ok(real) = std::env::var("HOME") {
        assert_ne!(
            home,
            PathBuf::from(real),
            "seed HOME must never equal the real HOME"
        );
    }
    let creds = live_creds();
    assert!(
        Path::new(&creds).exists(),
        "live credentials not found at {creds}; set LIVE_CREDENTIALS"
    );
    std::fs::copy(&creds, config.join(".credentials.json")).expect("copy creds into jail");
    let home_str = home.to_string_lossy();
    let claude_json = format!(
        r#"{{ "hasCompletedOnboarding": true, "numStartups": 0, "autoUpdates": false, "bypassPermissionsModeAccepted": true, "mcpServers": {{}}, "projects": {{ "{home_str}": {{ "hasTrustDialogAccepted": true, "projectOnboardingSeenCount": 1, "allowedTools": [] }} }} }}"#
    );
    std::fs::write(config.join(".claude.json"), claude_json).unwrap();
    Jail {
        root: root.to_path_buf(),
        home,
        config,
        xdg,
    }
}

impl Jail {
    fn sessions_dir(&self) -> PathBuf {
        self.home.join(".claude").join("sessions")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(qd_bin());
        c.args(args)
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", &self.config)
            .env("CLAUDE_BIN", real_claude_bin())
            .env("XDG_RUNTIME_DIR", &self.xdg)
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .env_remove("QD_HOME")
            .env_remove("QD_MUX")
            .env_remove("CLAUDE_CODE_SESSION_ID");
        c
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd(args).output().expect("spawn qd")
    }

    /// Reap EVERY process whose argv or cwd references this jail's unique tempdir
    /// path — the per-session daemon(s), the real claude child, and any revive child
    /// — without touching the operator's own live claude (different paths). Scans
    /// `/proc`; SIGKILL; excludes self.
    fn reap(&self) {
        let needle = self.root.to_string_lossy().into_owned();
        let me = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return;
        };
        for e in entries.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if pid == me {
                continue;
            }
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
                .unwrap_or_default();
            let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if cmdline.contains(&needle) || cwd.contains(&needle) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }
}

fn ls_find(out_stdout: &str, name: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<Vec<serde_json::Value>>(out_stdout)
        .ok()?
        .into_iter()
        .find(|r| r.get("name").and_then(|v| v.as_str()) == Some(name))
}

fn raw_status(sessions_dir: &Path, pid: i64) -> Option<String> {
    let txt = std::fs::read_to_string(sessions_dir.join(format!("{pid}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    v.get("status").and_then(|s| s.as_str()).map(str::to_string)
}

fn pid_alive(pid: i64) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
#[ignore = "needs network + live claude credentials; isolated tempdir HOME; run with --ignored --nocapture"]
fn real_claude_observe_cutover_seed() {
    let root = tempfile::tempdir().unwrap();
    let j = jail(root.path());
    let sessions_dir = j.sessions_dir();

    // --- qd start --headless against the REAL claude. The prompt emits a tripwire
    //     token (real assistant text) and then a long count, so the turn stays BUSY
    //     long enough to connect mid-turn. ------------------------------------------
    let prompt = format!(
        "Output this exact token on its own line first: {TRIPWIRE}. Then write a \
         thorough 250-word explanation of how the TCP three-way handshake works, then \
         slowly count from 1 to 40 with one number per line."
    );
    let start = j.run(&["start", SESSION, "--headless", "-p", &prompt]);
    let s_out = String::from_utf8_lossy(&start.stdout);
    let s_err = String::from_utf8_lossy(&start.stderr);
    assert_eq!(
        start.status.code(),
        Some(0),
        "qd start --headless (real claude) must exit 0; stdout={s_out} stderr={s_err}"
    );

    // --- poll until the real claude turn is BUSY (mid-turn) -----------------------
    let deadline = Instant::now() + Duration::from_secs(30);
    let pid = loop {
        if Instant::now() >= deadline {
            j.reap();
            panic!("real claude row never went busy within 30s (network/creds?); start_out={s_out} start_err={s_err}");
        }
        let ls = j.run(&["ls", "--json"]);
        if ls.status.code() == Some(0) {
            let stdout = String::from_utf8_lossy(&ls.stdout);
            if let Some(row) = ls_find(&stdout, SESSION) {
                let pid = row.get("pid").and_then(|v| v.as_i64()).unwrap_or(0);
                if pid != 0 && raw_status(&sessions_dir, pid).as_deref() == Some("busy") {
                    break pid;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    eprintln!("[seed] real claude row busy: pid={pid}");

    // --- qd connect during the busy window; drive choice 2 + a buffered input -----
    let mut child = match j
        .cmd(&["connect", SESSION])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            j.reap();
            panic!("spawn qd connect: {e}");
        }
    };
    {
        let mut sin = child.stdin.take().expect("connect stdin piped");
        std::thread::sleep(Duration::from_millis(1200)); // let observe settle + render
        let _ = writeln!(sin, "2");
        std::thread::sleep(Duration::from_millis(250));
        let _ = writeln!(sin, "buffered seed line for the operator");
        // The cutover is now PENDING; the run-loop stays alive to the boundary even
        // after stdin EOF (termination gates on !is_pending), so close stdin and let
        // connect run to its NATURAL conclusion.
        drop(sin);
    }

    // Let connect exit NATURALLY (cutover fires at the real boundary → teardown →
    // revive_claude ready-wait → attach → return), capturing the genuine exit as the
    // empirical Q2b boundary. A watchdog SIGKILLs only as a backstop if connect hangs
    // past the bounded boot ready-wait window (~60s) — so wait_with_output records
    // the real exit unless it genuinely wedged.
    let connect_pid = child.id();
    let watchdog = std::thread::spawn(move || {
        for _ in 0..1400 {
            std::thread::sleep(Duration::from_millis(100));
            if !Path::new(&format!("/proc/{connect_pid}")).exists() {
                return;
            }
        }
        unsafe {
            libc::kill(connect_pid as i32, libc::SIGKILL);
        }
    });
    let out = child.wait_with_output().expect("connect exits");
    let _ = watchdog.join();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Reap the whole jail (daemon(s) + real claude child + any revive child) BEFORE
    // asserting, so a failed assert never leaks real-claude orphans.
    j.reap();

    // (1) connect→OBSERVE.
    assert!(
        stdout.contains("Observing headless session"),
        "must reach OBSERVE; stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("Revived \""),
        "observe must not take the Cold→revive entry path"
    );

    // (1, §2a) the model's real reply text NEVER leaks into the observe output.
    assert!(
        !stdout.contains(TRIPWIRE),
        "§2a VIOLATION: the model's assistant text ({TRIPWIRE}) leaked into the read-only \
         observe output through the REAL socket; stdout={stdout}"
    );

    // (2) busy→idle observed live (seeded busy from the row, then the real TurnEnd).
    assert!(
        stdout.contains("in-turn: yes"),
        "expected a busy render (in-turn: yes) on the real turn; stdout={stdout}"
    );
    assert!(
        stdout.contains("in-turn: no"),
        "expected a live idle re-render (in-turn: no) after the real turn ended; stdout={stdout}"
    );

    // (3) choice-2 mid-turn DEFERS then FIRES at the boundary (never mid-turn).
    assert!(
        stdout.contains("Cutover queued"),
        "choice 2 during the real busy turn must DEFER; stdout={stdout}"
    );
    assert!(
        stdout.contains("Turn boundary reached"),
        "the deferred cutover must FIRE at the real turn boundary; stdout={stdout}"
    );

    // (4) teardown→revive_claude ran for real. The Q2b boundary (how far the
    // native-TUI/zmx attach physically drives) is recorded EMPIRICALLY — a real
    // headless `claude -p` child exits at its own turn end, so teardown reaps an
    // already-exited child (no-op kill) and the LOAD-BEARING real step here is the
    // `revive_claude` resume + where the terminal attach stops. NOT asserted to a
    // fixed outcome — printed for the close-verify to read the genuine boundary.
    eprintln!(
        "[seed] Q2b EMPIRICAL boundary — qd connect exit={:?}, child still alive={}\n\
         ----- connect stdout -----\n{stdout}\n----- connect stderr -----\n{stderr}",
        out.status.code(),
        pid_alive(pid)
    );
    eprintln!(
        "REAL-CLAUDE OBSERVE+CUTOVER SEED: PASS (claims 1-3 asserted; claim 4 boundary recorded)"
    );
}
