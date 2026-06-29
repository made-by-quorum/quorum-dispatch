//! WP-B-CS-2-LIVE — the live OBSERVE run-loop + turn-boundary cutover EXECUTION,
//! driven END-TO-END through the REAL `qd` binary against a JAILED HOME with a real
//! per-session qrmux daemon, a real registry, a real socket — ONLY `claude` is faked
//! (a `CLAUDE_BIN` fixture script, the `headless_cli_addressability` pattern).
//!
//! Two `#[ignore]` integration proofs (run explicitly — they spawn real `qd`
//! subprocesses + a detached daemon + sleep):
//!
//!   cargo test -p quorum-dispatch --test headless_observe_cutover -- --ignored --nocapture
//!
//!  (A) `live_observe_rerenders_control_facts_only_busy_to_idle` — across ONE
//!      `qd connect`, the read-only dashboard RE-RENDERS live as the turn goes
//!      busy→idle (in-turn: yes → in-turn: no), and NEVER leaks the agent's
//!      assistant text — §2a CONTROL FACTS ONLY proven through the real socket path,
//!      not just the pure fold.
//!  (B) `cutover_choice2_fires_at_boundary_and_reaps_headless` — driving the 1/2/3
//!      menu's choice 2 (with a buffered input) during the BUSY window DEFERS the
//!      cutover; at the turn boundary it FIRES and the EXECUTION tears the headless
//!      session down (reaps the still-alive fake-claude child) before reviving. The
//!      fixture keeps the child ALIVE past `result` so a dead child PROVES the
//!      teardown ran (a self-exiting fake would confound it). How far `revive_claude`
//!      then physically drives is recorded empirically (the Q2b boundary), not
//!      pre-conceded.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

const SESSION: &str = "hlobs";
const SID: &str = "fa4ec110-0000-4000-8000-0000000000c2";
/// The assistant text the fake emits in its stream-json — the §2a tripwire: it must
/// NEVER appear in `qd connect`'s observe output (the dashboard is control-facts
/// only; the republish protocol carries no content).
const SECRET: &str = "SECRET_ASSISTANT_SCROLLBACK_must_never_render";

/// A fake `claude -p` that: mints a busy row, emits an assistant text frame, HOLDS
/// busy for `QD_FAKE_BUSY_SECS`, emits `result` (→ the turn boundary), and — unless
/// `QD_FAKE_EXIT_AFTER_RESULT=1` — STAYS ALIVE (so a dead child after a cutover
/// proves the TEARDOWN reaped it, not a natural exit).
fn write_fixture(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake_claude.sh");
    let body = format!(
        "#!/bin/bash\n\
         sleep 0.3\n\
         echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{SID}\"}}'\n\
         echo '{{\"type\":\"assistant\",\"session_id\":\"{SID}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{SECRET}\"}}]}}}}'\n\
         sleep \"${{QD_FAKE_BUSY_SECS:-4}}\"\n\
         echo '{{\"type\":\"result\",\"session_id\":\"{SID}\",\"is_error\":false,\"stop_reason\":\"end_turn\"}}'\n\
         if [ \"${{QD_FAKE_EXIT_AFTER_RESULT:-0}}\" = \"1\" ]; then exit 0; fi\n\
         sleep 3600\n"
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

    fn cmd(&self, args: &[&str], exit_after_result: bool) -> Command {
        let mut c = Command::new(qd_bin());
        c.args(args)
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.xdg)
            .env("CLAUDE_BIN", &self.fixture)
            .env("QD_FAKE_BUSY_SECS", self.busy_secs.to_string())
            .env(
                "QD_FAKE_EXIT_AFTER_RESULT",
                if exit_after_result { "1" } else { "0" },
            )
            .env_remove("QD_HOME")
            .env_remove("QD_MUX")
            .env_remove("CLAUDE_CODE_SESSION_ID");
        c
    }

    /// Blocking `qd <args>` with the jailed env (stdin closed by `output()`).
    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd(args, false).output().expect("spawn qd")
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

/// Start a headless fake-claude session and poll until its child-pid-keyed row is
/// busy. Returns the live fake-claude child pid.
fn start_headless_busy(j: &Jail) -> i64 {
    let start = j.run(&["start", SESSION, "--headless", "-p", "hi"]);
    assert_eq!(
        start.status.code(),
        Some(0),
        "qd start --headless must exit 0; stderr={}",
        String::from_utf8_lossy(&start.stderr)
    );
    let sessions_dir = j.sessions_dir();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
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
                    // The addressable pid is the live fake-claude child, not the daemon.
                    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                    let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
                    assert!(
                        cmdline.contains("fake_claude.sh"),
                        "row pid must be the fake-claude child; cmdline={cmdline:?}"
                    );
                    return pid;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// (A) live re-render busy→idle, CONTROL FACTS ONLY (§2a) through the real socket
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spawns real qd subprocesses + a detached daemon + sleeps; run with --ignored --nocapture"]
fn live_observe_rerenders_control_facts_only_busy_to_idle() {
    let root = tempfile::tempdir().unwrap();
    // Short busy window + exit-after-result so the daemon closes the stream cleanly
    // (RepublishEnd) and `qd connect` terminates without us managing its stdin.
    let j = jail(root.path(), 3);
    let _pid = start_headless_busy(&j);

    // Connect DURING the busy window; hold the observe loop open across the
    // busy→idle transition by keeping stdin open until after the turn ends, then
    // close it (EOF) so the loop terminates and we can read the captured output.
    let mut child = j
        .cmd(&["connect", SESSION], /*exit_after_result=*/ true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn qd connect");
    // Let the turn complete (busy 3s + the result frame) so the loop re-renders idle.
    std::thread::sleep(Duration::from_secs(6));
    // Close stdin → the loop's stdin reader hits EOF → it terminates (if it hasn't
    // already on the stream's RepublishEnd).
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("connect exits");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Reached OBSERVE (not Cold→revive).
    assert!(
        stdout.contains("Observing headless session"),
        "must reach OBSERVE; stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("Revived") && !stderr.contains("Revived"),
        "must not revive a live target"
    );

    // LIVE RE-RENDER: the dashboard showed the turn BUSY (in-turn: yes) and then
    // IDLE (in-turn: no) within ONE connect — the loop folded the live stream, not a
    // single snapshot.
    assert!(
        stdout.contains("in-turn: yes"),
        "expected a busy render (in-turn: yes); stdout={stdout}"
    );
    assert!(
        stdout.contains("in-turn: no"),
        "expected a live idle re-render (in-turn: no) after the turn ended; stdout={stdout}"
    );

    // §2a HARD LINE through the real socket path: the agent's assistant text is
    // NEVER rendered (the dashboard is control facts only).
    assert!(
        !stdout.contains(SECRET),
        "the observe dashboard must NEVER render assistant text (§2a); stdout={stdout}"
    );
    println!("(A) live re-render busy→idle + §2a control-facts-only: PASS");
}

// ---------------------------------------------------------------------------
// (B) choice 2 → DEFERRED mid-turn → FIRES at the boundary → teardown reaps child
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spawns real qd subprocesses + a detached daemon + sleeps; run with --ignored --nocapture"]
fn cutover_choice2_fires_at_boundary_and_reaps_headless() {
    let root = tempfile::tempdir().unwrap();
    // A longer busy window so we can drive choice 2 DURING the turn; the fake STAYS
    // ALIVE past `result`, so a dead child afterwards proves the teardown reaped it.
    let j = jail(root.path(), 6);
    let pid = start_headless_busy(&j);
    assert!(
        pid_alive(pid),
        "fake-claude child must be live at connect time"
    );

    let mut child = j
        .cmd(&["connect", SESSION], /*exit_after_result=*/ false)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn qd connect");

    // Drive the menu: choose 2 (cut over), then a BUFFERED input line — DURING the
    // busy window, so the cutover DEFERS to the boundary.
    {
        let mut sin = child.stdin.take().expect("connect stdin piped");
        std::thread::sleep(Duration::from_millis(800)); // let the menu render
        writeln!(sin, "2").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        writeln!(sin, "buffered hello from the operator").unwrap();
        // Keep stdin OPEN (do not drop `sin` yet) so the loop keeps running until
        // the boundary fires the deferred cutover.
        std::thread::sleep(Duration::from_secs(10)); // past the 6s busy window
        drop(sin); // now allow EOF
    }

    // The cutover fires at the boundary → teardown reaps the (still-alive) child.
    let dead_deadline = Instant::now() + Duration::from_secs(15);
    while pid_alive(pid) && Instant::now() < dead_deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    let reaped = !pid_alive(pid);

    // Reap the connect process regardless of how far revive/attach drove.
    let _ = child.kill();
    let out = child.wait_with_output().expect("connect exits");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stdout.contains("Observing headless session"),
        "must reach OBSERVE; stdout={stdout}"
    );
    // The cutover was DEFERRED (chosen mid-turn) and announced.
    assert!(
        stdout.contains("Cutover queued"),
        "choice 2 mid-turn must DEFER to the boundary; stdout={stdout}"
    );
    // It FIRED at the turn boundary (never mid-turn).
    assert!(
        stdout.contains("Turn boundary reached"),
        "the deferred cutover must FIRE at the turn boundary; stdout={stdout}"
    );
    // TEARDOWN ran: the still-alive fake-claude child is now dead.
    assert!(
        reaped,
        "the cutover EXECUTION must tear the headless session down (reap the child); \
         child pid {pid} still alive. stdout={stdout} stderr={stderr}"
    );

    // EMPIRICAL Q2b boundary: record how far revive_claude/attach physically drove
    // (the fake script is not a real native-TUI claude, so the revive ready-wait /
    // attach is where the fixture path stops — the genuine boundary is found by the
    // real-claude seed; here we only assert the LOGIC up to + including teardown).
    println!(
        "(B) cutover deferred→fired→reaped: PASS. revive/attach tail (empirical): \
         exit={:?}\n--- connect stdout ---\n{stdout}\n--- connect stderr ---\n{stderr}",
        out.status.code()
    );
}
