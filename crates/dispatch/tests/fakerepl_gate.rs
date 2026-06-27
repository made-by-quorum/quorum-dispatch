//! A4 M4a — Level-1 fake-REPL PTY gate (a4-spec §5, gate item 1, LEVEL 1 ONLY).
//!
//! Spawns the `fakerepl` binary on a REAL PTY inside a jail-shaped tempdir and
//! drives the M1 submit discipline (`verify_accepted_then_cr` / `deliver_prompt`)
//! against it with REAL timing. The turn-count ORACLE keys on APPLICATION OUTPUT
//! (the `[turn N] accepted` lines parsed from the PTY byte stream), NEVER on echo
//! and NEVER on the report (ADD-6); the report JSONL is a CROSS-CHECK only.
//!
//! Rows (a4-spec §5 Level 1):
//! - L1-paste: a ≥4KB single write incl. trailing `\r` → EXACTLY ONE turn.
//! - L1-frag (W7): a paste split across a forced >50ms stall → STILL one turn.
//! - L1-soak: ≥100 iterations, per-iteration knobs from a FIXED seed table → zero
//!   dropped, zero double (app-output tally + report cross-check).
//! - neg-control A: inject a CR while busy → report shows cr_while_busy>0 AND the
//!   harness check FAILS (asserted RED).
//! - neg-control B (W8): swallow the remediation CR, threshold forced below
//!   message size → soak iteration goes RED (asserted detected).
//! - jail-refusal: clean env → 13; partial spoof → 13; valid jail → starts.
//! - coalescing_note (W7): MEASURE portable-pty chunk coalescing for the README.
//!
//! Level 2 (the golden `qd new` exit-code scenario through real zmx) is a LATER
//! milestone (depends on M2) — NOT in this file.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize};

use dispatch::boot::{read_pid_status, RealSleeper, Sleeper};
use dispatch::effects::{Clock, RealClock};
use dispatch::submit::{
    deliver_idle_two_write, deliver_prompt, payload_needs_verify, verify_accepted_then_cr,
    verify_chunked_payload, DeliverDeps, DeliverOutcome, IdleDeliverDeps, PayloadVerifyOutcome,
    SubmitDeps, SubmitOptions, VerifyDeps, DELIVER_TIMEOUT_S, VERIFY_POLL_MS, VERIFY_TIMEOUT_S,
};

// ===========================================================================
// Locating the fakerepl binary.
//
// fakerepl is a DIFFERENT workspace crate, so `CARGO_BIN_EXE_fakerepl` is not
// available to qd's tests. `cargo test --workspace` builds all workspace
// binaries before running tests, so `<target>/<profile>/fakerepl` exists. We
// derive `<target>/<profile>` from the running test exe path
// (`.../target/<profile>/deps/<testbin>`), which is robust to debug/release and
// to a custom CARGO_TARGET_DIR. If absent (e.g. a `cargo test -p dispatch` that did
// not build fakerepl), we shell out to `cargo build -p fakerepl` once.
// ===========================================================================

fn fakerepl_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    // exe = <target>/<profile>/deps/<testbin>; profile dir is two parents up.
    let profile_dir = exe
        .parent() // deps/
        .and_then(|p| p.parent()) // <profile>/
        .expect("profile dir")
        .to_path_buf();
    let bin = profile_dir.join(if cfg!(windows) {
        "fakerepl.exe"
    } else {
        "fakerepl"
    });
    // STALENESS GUARD (A4 chunking follow-up, lead re-verification incident): an
    // existing binary OLDER than the newest fakerepl source is a STALE ORACLE —
    // a gate run against it silently tests the wrong model (observed live: an
    // M4a-era binary without SB_FAKEREPL_DROP_OVER_BYTES made the overflow
    // negative-control fail as a false-RED-missing). Fail LOUD; never trust an
    // out-of-date oracle. (We cannot `cargo build` here unconditionally — a
    // nested cargo invocation can deadlock on the parent `cargo test`'s target
    // lock — so missing → build-once, stale → loud instructions.)
    if bin.exists() {
        if let Some(bin_mtime) = mtime(&bin) {
            let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../fakerepl/src")
                .canonicalize()
                .expect("fakerepl src dir");
            let newest_src = std::fs::read_dir(&src_dir)
                .expect("read fakerepl src")
                .flatten()
                .filter_map(|e| mtime(&e.path()))
                .max();
            if let Some(newest) = newest_src {
                assert!(
                    bin_mtime >= newest,
                    "STALE fakerepl binary at {bin:?} (older than {src_dir:?} sources) — \
                     the gate would test an outdated oracle. Run: cargo build -p fakerepl"
                );
            }
        }
        return bin;
    }
    // Fallback: build it once (documented mechanism — keeps `cargo test -p dispatch`
    // working without a prior `--workspace` build).
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "fakerepl"])
        .status()
        .expect("spawn cargo build -p fakerepl");
    assert!(status.success(), "cargo build -p fakerepl failed");
    assert!(
        bin.exists(),
        "fakerepl binary missing at {bin:?} after build"
    );
    bin
}

/// Modification time helper for the staleness guard.
fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

// ===========================================================================
// Jail-shaped tempdir.
//
// Constructs the EXACT exported isolation layout the fakerepl belt requires
// (a4-spec §5): HOME=<root>/sbrg-runs/<id>/home, SB_HOME=<root>/.../sb_home,
// ZMX_DIR=.../zmx, TMPDIR=.../tmp. This doubles as a positive control that the
// belt ACCEPTS a valid jail (every non-refusal row proves it).
// ===========================================================================

struct Jail {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    sb_home: PathBuf,
    zmx: PathBuf,
    tmp: PathBuf,
    sessions_dir: PathBuf,
}

impl Jail {
    fn new(id: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Layout: <tempdir>/sbrg-runs/<id>/{home,sb_home,zmx,tmp}
        let root = tmp.path().join("sbrg-runs").join(id);
        let home = root.join("home");
        let sb_home = root.join("sb_home");
        let zmx = root.join("zmx");
        let tmpd = root.join("tmp");
        for d in [&home, &sb_home, &zmx, &tmpd] {
            std::fs::create_dir_all(d).expect("mkdir jail subtree");
        }
        let sessions_dir = home.join(".claude").join("sessions");
        Self {
            _tmp: tmp,
            home,
            sb_home,
            zmx,
            tmp: tmpd,
            sessions_dir,
        }
    }

    /// Apply the jail env to a CommandBuilder (env-cleared, then the belt set).
    fn apply(&self, cmd: &mut CommandBuilder) {
        cmd.env_clear();
        cmd.env("HOME", &self.home);
        cmd.env("SB_HOME", &self.sb_home);
        cmd.env("ZMX_DIR", &self.zmx);
        cmd.env("TMPDIR", &self.tmp);
        // PATH kept minimal but present so the child can exec (it execs nothing,
        // but a totally empty PATH trips some libc paths on macOS).
        cmd.env("PATH", "/usr/bin:/bin");
    }
}

// ===========================================================================
// PTY-spawned fakerepl child + the SUT deps bound to it.
// ===========================================================================

/// A running fakerepl on a real PTY. The reader thread drains the master into
/// `output` (the app-output oracle parses this); `writer` is the master write
/// half the SUT's `send_cr`/`send_message` push keystrokes into.
struct PtyChild {
    writer: Mutex<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    sessions_dir: PathBuf,
    session_name: String,
    report_path: PathBuf,
    /// Swallow the remediation CR (negative control B): `send_cr` becomes a
    /// no-op.
    swallow_cr: bool,
}

impl PtyChild {
    fn spawn(jail: &Jail, name: &str, extra_env: &[(&str, &str)]) -> Self {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let report_path = jail.tmp.join(format!("report-{name}.jsonl"));

        let mut cmd = CommandBuilder::new(fakerepl_bin());
        jail.apply(&mut cmd);
        cmd.arg("--name");
        cmd.arg(name);
        cmd.env("SB_FAKEREPL_REPORT", &report_path);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        // Start in the jail tmp so any relative writes stay inside the jail.
        cmd.cwd(&jail.tmp);

        let child = pair.slave.spawn_command(cmd).expect("spawn fakerepl");
        // Drop the slave so EOF propagates when we close the master.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone reader");
        let output = Arc::new(Mutex::new(Vec::new()));
        let out2 = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => out2.lock().unwrap().extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
        });

        let writer = pair.master.take_writer().expect("take writer");

        Self {
            writer: Mutex::new(writer),
            output,
            child,
            sessions_dir: jail.sessions_dir.clone(),
            session_name: name.to_string(),
            report_path,
            swallow_cr: false,
        }
    }

    fn write_raw(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    /// Snapshot the app-output captured so far, as a UTF-8-lossy string.
    fn output_text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Count `[turn N] accepted` lines in the app-output — the GATE ORACLE.
    fn accepted_turns(&self) -> usize {
        self.output_text()
            .lines()
            .filter(|l| l.contains("] accepted bytes="))
            .count()
    }

    /// Synthesize the COMPOSER SCREEN for the R4 two-write content-verified-CR
    /// predicate (ADR 0009). The fakerepl is not a TUI — it emits app-output, not a
    /// `❯`-prefixed screen dump — so we render a faithful composer screen from its
    /// OBSERVABLE state: while NO turn has been accepted yet, the pasted `message`
    /// is still sitting unsubmitted in the composer (render `❯ <message>`); once a
    /// turn lands, the composer is empty (`❯ `). This is exactly what
    /// `composer_holds_message` keys on, so the REAL `deliver_idle_two_write` CR
    /// gate is driven against the fakerepl's real turn-acceptance over a real PTY.
    /// (The predicate's own glyph/strip/wrap logic is unit-tested directly in
    /// `sendpty.rs`; here it integrates with the live two-write helper.)
    fn composer_screen(&self, message: &str) -> String {
        if self.accepted_turns() == 0 {
            format!("\u{276f} {message}")
        } else {
            "\u{276f} ".to_string()
        }
    }

    /// Wait until the pid file appears (the registry row), bounded.
    fn wait_for_row(&self, timeout_ms: u64) -> Option<PathBuf> {
        let clock = RealClock;
        let sleeper = RealSleeper;
        dispatch::boot::find_pid_file(
            &self.sessions_dir,
            &self.session_name,
            timeout_ms as i64,
            25,
            &clock,
            &sleeper,
        )
    }

    /// Parse the report JSONL into events (cross-check only). Empty if absent.
    fn report_events(&self) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(&self.report_path).unwrap_or_default();
        text.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .collect()
    }

    fn report_turn_count(&self) -> usize {
        self.report_events()
            .iter()
            .filter(|e| e.get("event").and_then(|v| v.as_str()) == Some("turn"))
            .count()
    }

    fn report_cr_while_busy(&self) -> usize {
        self.report_events()
            .iter()
            .filter(|e| {
                e.get("event").and_then(|v| v.as_str()) == Some("cr")
                    && e.get("cr_kind").and_then(|v| v.as_str()) == Some("while_busy")
            })
            .count()
    }

    /// The `bytes=` of the LAST accepted turn in the report (the composer length the
    /// fakerepl submitted). `None` if no turn was accepted. Used by the jumbo rows to
    /// assert the FULL payload landed (sent bytes == submitted composer bytes).
    fn report_last_turn_bytes(&self) -> Option<u64> {
        self.report_events()
            .iter()
            .filter(|e| e.get("event").and_then(|v| v.as_str()) == Some("turn"))
            .filter_map(|e| e.get("bytes").and_then(|v| v.as_u64()))
            .next_back()
    }

    /// Count `drop` events (tty-queue overflow, ADR 0009 mode (a)) — a burst that
    /// exceeded SB_FAKEREPL_DROP_OVER_BYTES and was dropped wholesale.
    fn report_drop_count(&self) -> usize {
        self.report_events()
            .iter()
            .filter(|e| e.get("event").and_then(|v| v.as_str()) == Some("drop"))
            .count()
    }

    /// Terminate the fakerepl and reap it, returning its exit code.
    ///
    /// We send SIGTERM (NOT relying on stdin EOF): the reader thread holds a
    /// clone of the PTY master, so the slave never sees EOF while the test is
    /// alive — EOF-based shutdown is therefore unobservable here. SIGTERM is the
    /// clean path: the fakerepl's handler unlinks the registry row and exits 0
    /// (so a clean termination returns 0, exactly the "removes it on SIGTERM"
    /// contract). If it does not exit promptly we escalate to SIGKILL so a
    /// lingering child never wedges cargo / the build lock.
    fn finish(mut self) -> i32 {
        *self.writer.lock().unwrap() = Box::new(std::io::sink());
        self.terminate()
    }

    /// SIGTERM → wait (clean, returns the handler's exit 0); SIGKILL fallback.
    /// Returns the child's exit code, or -1 if it had to be SIGKILLed.
    fn terminate(&mut self) -> i32 {
        if let Some(pid) = self.child.process_id() {
            // SAFETY: pid is the child's pid; SIGTERM is a valid signal.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_millis(2000);
        loop {
            match self.child.try_wait() {
                Ok(Some(s)) => return s.exit_code() as i32,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill(); // SIGKILL fallback
                        let _ = self.child.wait();
                        return -1;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return -1,
            }
        }
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        // Belt-and-suspenders: a test that panics before calling `finish()` must
        // not orphan a fakerepl child holding the PTY (that wedges cargo and the
        // build lock). SIGTERM then reap (SIGKILL fallback inside terminate).
        *self.writer.lock().unwrap() = Box::new(std::io::sink());
        let _ = self.terminate();
    }
}

// --- SUT deps bound to the PTY child ---------------------------------------

struct PtySubmitDeps<'a> {
    child: &'a PtyChild,
    pid_file: PathBuf,
    clock: RealClock,
    sleeper: RealSleeper,
}

impl SubmitDeps for PtySubmitDeps<'_> {
    fn read_status(&self) -> Option<String> {
        read_pid_status(&self.pid_file)
    }
    fn send_cr(&self) {
        if self.child.swallow_cr {
            // Negative control B: the remediation CR is DROPPED. The composer
            // stays unsubmitted → the discipline cannot make it go busy.
            return;
        }
        self.child.write_raw(b"\r");
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// An [`IdleDeliverDeps`] bound to the PTY child — drives the R4 two-write idle
/// delivery helper (`deliver_idle_two_write`) over a real PTY. `send_text` writes
/// the text ALONE; `send_cr` writes a lone "\r"; `read_screen` synthesizes the
/// composer screen so the content-verified CR keys on real turn-acceptance.
struct PtyIdleDeliverDeps<'a> {
    child: &'a PtyChild,
    pid_file: PathBuf,
    message: String,
    /// W7 (red-team #5): split the text write into two sub-bursts straddling a
    /// stall longer than GAP_MS, INSIDE the delivery path, to prove the oracle/SUT
    /// timing layers are independent on the real two-write helper. `None` keeps the
    /// text as one write.
    fragment_text_at: Option<usize>,
    clock: RealClock,
    sleeper: RealSleeper,
}

impl IdleDeliverDeps for PtyIdleDeliverDeps<'_> {
    fn send_text(&self, text: &str) {
        // First of the two writes: the TEXT ALONE, no CR (ADR 0009). At ≥ threshold
        // this is a paste burst with NO trailing \r — nothing submits yet.
        match self.fragment_text_at {
            Some(at) if at < text.len() => {
                // W7: two sub-bursts with a >50ms stall between (each its own paste
                // burst, neither submits — there is no CR in either half).
                self.child.write_raw(&text.as_bytes()[..at]);
                std::thread::sleep(Duration::from_millis(120));
                self.child.write_raw(&text.as_bytes()[at..]);
            }
            _ => self.child.write_raw(text.as_bytes()),
        }
    }
    fn send_cr(&self) {
        if self.child.swallow_cr {
            // Negative control: drop the CR (the composer can never submit).
            return;
        }
        // A LONE "\r" — its own non-paste keystroke burst (the >GAP settle closed
        // the text burst), so the fakerepl treats it as a SUBMIT, not absorbed.
        self.child.write_raw(b"\r");
    }
    fn read_screen(&self) -> String {
        self.child.composer_screen(&self.message)
    }
    fn read_status(&self) -> Option<String> {
        read_pid_status(&self.pid_file)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

struct PtyDeliverDeps<'a> {
    child: &'a PtyChild,
}

impl DeliverDeps for PtyDeliverDeps<'_> {
    fn send_message(&self, message: &str) {
        // R4 TWO-WRITE delivery (ADR 0009 LEAD EXTENSION): text ALONE, ~200ms
        // settle, "\r" ALONE — NOT a single message+"\r" write (paste-absorbed at
        // ≥ threshold). Mirrors deliver_idle_two_write's two writes; the priming
        // prompt is the likeliest ≥4KB case.
        self.child.write_raw(message.as_bytes());
        std::thread::sleep(Duration::from_millis(dispatch::submit::TWO_WRITE_SETTLE_MS));
        if self.child.swallow_cr {
            // Negative control B: drop the delivery CR too. With BOTH the delivery
            // CR and the (PtyIdleDeliverDeps) remediation CR swallowed, the composer
            // never submits → the soak invariant goes red (assert RED).
            return;
        }
        self.child.write_raw(b"\r");
    }
    fn read_screen(&self) -> String {
        // Not consulted by deliver_prompt directly; the per-round content
        // verification lives in the submit_deps's IdleDeliverDeps::read_screen.
        String::new()
    }
    fn find_pid_file(&self) -> Option<PathBuf> {
        self.child.wait_for_row(5000)
    }
    fn submit_deps(&self, pid_file: PathBuf, message: &str) -> Box<dyn SubmitDeps + '_> {
        // CONTENT-VERIFIED per-round SubmitDeps (ADR 0009): wrap a PTY-bound
        // IdleDeliverDeps so each remediation CR fires only while the composer
        // still holds `message`.
        Box::new(dispatch::submit::ContentVerifiedSubmit::new(
            PtyIdleDeliverDeps {
                child: self.child,
                pid_file,
                message: message.to_string(),
                fragment_text_at: None,
                clock: RealClock,
                sleeper: RealSleeper,
            },
            message,
        ))
    }
}

// ===========================================================================
// Helpers shared by the rows.
// ===========================================================================

/// Drive ONE delivery of `message` against a freshly-spawned fakerepl and return
/// `(accepted_turns_from_app_output, report_turn_count, deliver_outcome)`.
fn drive_once(
    jail: &Jail,
    name: &str,
    message: &str,
    extra_env: &[(&str, &str)],
) -> (usize, usize, DeliverOutcome) {
    let child = PtyChild::spawn(jail, name, extra_env);
    // Wait for the registry row so deliver_prompt's find_pid_file is fast.
    child
        .wait_for_row(5000)
        .expect("fakerepl registry row must appear");

    let deps = PtyDeliverDeps { child: &child };
    let outcome = deliver_prompt(&deps, message, DELIVER_TIMEOUT_S);

    // Let the accepted-turn app-output + report `turn` line flush. (The `turn`
    // report event and `[turn N] accepted` line are emitted at submit time,
    // BEFORE the busy hold, so a short settle suffices — we do NOT wait out the
    // whole busy window here.)
    settle(150);

    let app_turns = child.accepted_turns();
    let report_turns = child.report_turn_count();
    if std::env::var("FR_DEBUG").is_ok() {
        let bursts: Vec<u64> = child
            .report_events()
            .iter()
            .filter(|e| e.get("event").and_then(|v| v.as_str()) == Some("burst"))
            .filter_map(|e| e.get("size").and_then(|v| v.as_u64()))
            .collect();
        eprintln!(
            "FR_DEBUG drive_once({name}): outcome={outcome:?} app_turns={app_turns} \
             report_turns={report_turns} bursts={bursts:?} out={:?}",
            child.output_text()
        );
    }
    let _ = child.finish();
    (app_turns, report_turns, outcome)
}

fn settle(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// DIAGNOSTIC (not a gate row): isolate the raw 4KB single-write burst behavior
/// from deliver_prompt. Spawns a fakerepl, writes 4096 P's + '\r' as ONE write,
/// settles, then prints the burst sizes the fakerepl recorded + its status.
/// Run with: cargo test ... diag_raw_4k -- --nocapture
#[test]
fn diag_raw_4k_write_burst_shape() {
    let jail = Jail::new("diag4k");
    let child = PtyChild::spawn(&jail, "diag4k", &[("SB_FAKEREPL_BUSY_MS", "150")]);
    let pid_file = child.wait_for_row(5000).expect("row");

    let mut payload = vec![b'P'; 4096];
    payload.push(b'\r');
    child.write_raw(&payload);
    settle(800);
    let status1 = read_pid_status(&pid_file);

    // Now a lone remediation CR (own burst). Read status IMMEDIATELY (within the
    // 150ms busy window) to prove the busy transition is observable when polled
    // promptly — the deliver_prompt poll interval is 250ms, so a 150ms busy
    // window can be MISSED (the root cause of the l1_paste Stalled).
    child.write_raw(b"\r");
    settle(40);
    let status_immediate = read_pid_status(&pid_file);
    settle(560);
    let status2 = read_pid_status(&pid_file);
    eprintln!("DIAG status 40ms-after-CR (in busy window?) = {status_immediate:?}");

    let bursts: Vec<(u64, bool)> = child
        .report_events()
        .iter()
        .filter(|e| e.get("event").and_then(|v| v.as_str()) == Some("burst"))
        .map(|e| {
            (
                e.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                e.get("paste").and_then(|v| v.as_bool()).unwrap_or(false),
            )
        })
        .collect();
    eprintln!(
        "DIAG raw-4k: bursts(size,paste)={bursts:?} status_after_paste={status1:?} \
         status_after_cr={status2:?} app={:?}",
        child.output_text()
    );
    let _ = child.finish();
}

// ===========================================================================
// ROW: L1-paste — a ≥4KB single write incl. trailing \r → EXACTLY ONE turn.
// ===========================================================================

#[test]
fn l1_paste_large_single_write_lands_exactly_one_turn() {
    let jail = Jail::new("l1paste");
    // 4KB+ message; deliver_prompt appends the trailing \r, so the WHOLE write is
    // one paste burst → the \r is absorbed → the discipline must remediate with
    // exactly one CR to submit → exactly one turn.
    let msg = "P".repeat(4096);
    // busy_ms MUST exceed deliver_prompt's 250ms status-poll interval (plus the
    // fakerepl's ≤50ms burst-close latency) so the acceptance (busy) transition
    // is observable — a sub-poll busy window would be missed and the discipline
    // would (correctly) judge the prompt un-accepted. 800ms gives comfortable
    // margin. (Real claude busy windows are seconds; a <250ms window is not a
    // realistic acceptance signal — see README "busy-window vs poll interval".)
    let (app_turns, report_turns, outcome) =
        drive_once(&jail, "l1paste", &msg, &[("SB_FAKEREPL_BUSY_MS", "800")]);

    assert_eq!(outcome, DeliverOutcome::Accepted, "must be accepted");
    // ORACLE: exactly one accepted turn in the APP OUTPUT.
    assert_eq!(
        app_turns, 1,
        "[turn 1] accepted present, [turn 2] absent (app-output keyed)"
    );
    // CROSS-CHECK: report agrees.
    assert_eq!(report_turns, 1, "report turn count cross-check");
}

// ===========================================================================
// ROW: L1-frag (W7) — a paste delivered as two sub-bursts straddling a forced
// >50ms stall must STILL land exactly one turn. This breaks the coincidence
// that the oracle (fakerepl burst-gap) and the SUT (deliver_prompt cadence)
// share a timing layer: even when the paste is fragmented across the gap,
// the discipline's acceptance-keyed remediation lands ONE turn.
//
// SCOPING (in-phase red-team #5): this row PRE-LOADS the composer (raw writes)
// and then runs the discipline — it bypasses the delivery CADENCE. The
// `r4_fragmented_paste_inside_delivery_lands_exactly_one_turn` row below closes
// that seam properly: it fragments the paste INSIDE the two-write delivery helper
// itself (text in two sub-bursts straddling a >50ms stall, driven through
// deliver_idle_two_write), so the oracle/SUT timing-layer independence is proven
// on the REAL delivery path, not a pre-loaded composer.
// ===========================================================================

#[test]
fn l1_fragmented_paste_across_stall_lands_exactly_one_turn() {
    let jail = Jail::new("l1frag");
    let name = "l1frag";
    let child = PtyChild::spawn(&jail, name, &[("SB_FAKEREPL_BUSY_MS", "150")]);
    child.wait_for_row(5000).expect("row appears");

    // Deliver a paste as TWO sub-bursts with a >50ms (here 120ms) stall between
    // them, NO trailing CR yet. Each half is ≥ threshold so each is its own paste
    // burst; neither submits.
    let half = "F".repeat(2048);
    child.write_raw(half.as_bytes());
    settle(120); // forces a burst boundary (>50ms gap)
    child.write_raw(half.as_bytes());
    settle(120);

    // Now run the discipline: the composer holds the fragmented paste, never
    // submitted. verify_accepted_then_cr sees not-busy, fires ONE remediation CR
    // (its own non-paste burst) → submit → exactly one turn.
    let pid_file = child.wait_for_row(2000).expect("row");
    let deps = PtySubmitDeps {
        child: &child,
        pid_file,
        clock: RealClock,
        sleeper: RealSleeper,
    };
    let out = verify_accepted_then_cr(
        &deps,
        SubmitOptions {
            settle_ms: 400, // short: nothing auto-submits, force remediation
            post_cr_ms: 4000,
            poll_ms: 50,
        },
    );
    settle(300);

    assert!(
        out.accepted,
        "fragmented paste must be accepted after one CR"
    );
    assert_eq!(out.crs_fired, 1, "exactly one remediation CR");
    assert_eq!(
        child.accepted_turns(),
        1,
        "fragmented paste lands EXACTLY ONE turn (app-output keyed)"
    );
    assert_eq!(child.report_turn_count(), 1, "report cross-check");
    let _ = child.finish();
}

// ===========================================================================
// R4 ROWS (ADR 0009; orc-2 RULED fix-in-phase, ruling relay-1780631655040-9
// item 2). The LIVE finding: on REAL claude 2.1.163 a ≥4KB single `message+"\r"`
// write on the IDLE send:pty path is paste-absorbed AND the remediation CR does
// not recover it (test/golden/dryrun/a4-live-evidence.md §FINDING + a4-paste-
// bytes.txt; 2-boot repro). These rows drive the REAL two-write delivery helper
// `deliver_idle_two_write` (the production code the idle bin path runs) over a
// real PTY, and a single-write NEGATIVE CONTROL proves the two-write mechanism is
// load-bearing.
// ===========================================================================

/// Drive ONE idle delivery of `message` via the REAL `deliver_idle_two_write`
/// helper against a freshly-spawned fakerepl. `fragment_text_at` optionally splits
/// the text write into two sub-bursts across a >50ms stall (W7). Returns
/// `(accepted_turns_from_app_output, SubmitOutcome)`.
fn drive_idle_two_write(
    jail: &Jail,
    name: &str,
    message: &str,
    extra_env: &[(&str, &str)],
    fragment_text_at: Option<usize>,
) -> (usize, dispatch::submit::SubmitOutcome) {
    let child = PtyChild::spawn(jail, name, extra_env);
    let pid_file = child
        .wait_for_row(5000)
        .expect("fakerepl registry row must appear");

    let deps = PtyIdleDeliverDeps {
        child: &child,
        pid_file,
        message: message.to_string(),
        fragment_text_at,
        clock: RealClock,
        sleeper: RealSleeper,
    };
    // Default SubmitOptions: 2500ms settle so the two-write CR's busy window (knob
    // ≥800ms below) is observable at the 250ms status poll (L14 floor).
    let outcome = deliver_idle_two_write(&deps, message, SubmitOptions::default());
    settle(150);
    let app_turns = child.accepted_turns();
    let _ = child.finish();
    (app_turns, outcome)
}

/// Drive ONE CHUNKED idle delivery of `message` via the REAL
/// `deliver_idle_two_write_with` helper, returning the live `PtyChild` (so the
/// caller can read the report's last-turn bytes + drop count) plus
/// `(accepted_turns, SubmitOutcome)`. `chunk_opts` injects the chunk_bytes/settle_ms
/// seams (the inter-chunk settle MUST exceed the fakerepl's 50ms GAP so each chunk
/// closes as its OWN burst — every chunk ≤ chunk_bytes is then independently under
/// any tty-queue drop bound). The child is kept ALIVE for report inspection and
/// terminated by the caller.
fn drive_idle_chunked(
    child: &PtyChild,
    message: &str,
    chunk_opts: dispatch::submit::ChunkSendOptions,
) -> (usize, dispatch::submit::SubmitOutcome) {
    let pid_file = child
        .wait_for_row(5000)
        .expect("fakerepl registry row must appear");
    let deps = PtyIdleDeliverDeps {
        child,
        pid_file,
        message: message.to_string(),
        fragment_text_at: None,
        clock: RealClock,
        sleeper: RealSleeper,
    };
    let outcome = dispatch::submit::deliver_idle_two_write_with(
        &deps,
        message,
        SubmitOptions::default(),
        chunk_opts,
    );
    settle(150);
    let app_turns = child.accepted_turns();
    (app_turns, outcome)
}

// ===========================================================================
// R4 ROW (a): a ≥4KB IDLE-path delivery exercising the REAL two-write code path
// (deliver_idle_two_write) must land EXACTLY ONE turn. The text alone (≥4KB) is a
// paste burst with NO CR → nothing submits; the SEPARATE "\r" (its own non-paste
// keystroke after the 200ms settle) submits it → one turn. This is the LIVE R4
// loss the single-write path could not deliver.
// ===========================================================================

#[test]
fn r4_idle_two_write_large_paste_lands_exactly_one_turn() {
    let jail = Jail::new("r4idle");
    let msg = "P".repeat(4096);
    // busy_ms ≥700 (L14 floor: observable at the 250ms status poll); 800 = margin.
    let (app_turns, outcome) = drive_idle_two_write(
        &jail,
        "r4idle",
        &msg,
        &[("SB_FAKEREPL_BUSY_MS", "800")],
        None,
    );

    assert!(outcome.accepted, "two-write idle delivery must be accepted");
    // The two-write CR submitted it; the content-verified remediation never needed
    // to fire (composer empty after submit) — so ZERO remediation CRs.
    assert_eq!(
        outcome.crs_fired, 0,
        "submitted off the separate \"\\r\" write; no remediation needed"
    );
    assert_eq!(
        app_turns, 1,
        "≥4KB idle paste lands EXACTLY ONE turn via two-write (app-output keyed)"
    );
}

// ===========================================================================
// R4 ROW (b) — NEGATIVE CONTROL: the OLD single-write mechanism under a fakerepl
// config modeling the LIVE behavior (SB_FAKEREPL_ABSORB_ALL_CRS=1 — every CR is
// absorbed as a literal newline, never a submit, exactly the observed "the
// remediation CR does NOT recover it" 2-boot finding). With this config a single
// `message+"\r"` write MUST fail to land a turn AND the remediation CR cannot
// rescue it — proving the two-write mechanism (row a) is load-bearing, not
// incidental. Mutation-style: assert the RED.
// ===========================================================================

#[test]
fn r4_single_write_under_absorb_all_crs_fails_to_land_negative_control() {
    let jail = Jail::new("r4neg");
    let name = "r4neg";
    // ABSORB_ALL_CRS models the live ≥4KB behavior: NO CR ever submits.
    let child = PtyChild::spawn(
        &jail,
        name,
        &[
            ("SB_FAKEREPL_BUSY_MS", "800"),
            ("SB_FAKEREPL_ABSORB_ALL_CRS", "1"),
        ],
    );
    let pid_file = child.wait_for_row(5000).expect("row");

    // OLD single-write mechanism: message + "\r" as ONE write (the pre-R4 idle
    // path). Under ABSORB_ALL_CRS the trailing \r is absorbed → no submit.
    let mut payload = msg_bytes(4096);
    payload.push(b'\r');
    child.write_raw(&payload);
    settle(300);

    // Now even the discipline's remediation CR (a lone "\r") is absorbed too — run
    // the verify-then-CR and confirm it can NOT make it go busy.
    let deps = PtySubmitDeps {
        child: &child,
        pid_file,
        clock: RealClock,
        sleeper: RealSleeper,
    };
    let out = verify_accepted_then_cr(
        &deps,
        SubmitOptions {
            settle_ms: 400,
            post_cr_ms: 1200,
            poll_ms: 100,
        },
    );
    settle(150);
    let app_turns = child.accepted_turns();
    let _ = child.finish();

    // The RED: single-write + absorbed remediation CR → ZERO turns, not accepted.
    // This is the exact live R4 loss; row (a)'s two-write is what fixes it.
    assert_eq!(
        app_turns, 0,
        "single-write under absorb-all-CRs lands ZERO turns (the live R4 loss)"
    );
    assert!(
        !out.accepted,
        "the remediation CR cannot rescue a single-write absorbed paste (R4)"
    );
}

// ===========================================================================
// R4 ROW (c) / W7 (red-team #5): fragment the paste INSIDE the two-write delivery
// helper — the text written in two sub-bursts straddling a >50ms stall, driven
// through deliver_idle_two_write — must STILL land exactly one turn. Unlike the
// l1-frag row (which pre-loads the composer and bypasses the cadence), this proves
// the oracle/SUT timing-layer independence on the REAL delivery path.
// ===========================================================================

#[test]
fn r4_fragmented_paste_inside_delivery_lands_exactly_one_turn() {
    let jail = Jail::new("r4frag");
    let msg = "F".repeat(4096);
    // Fragment the text write at the midpoint (each half ≥ threshold → its own
    // paste burst, no CR in either; the separate "\r" then submits the whole).
    let (app_turns, outcome) = drive_idle_two_write(
        &jail,
        "r4frag",
        &msg,
        &[("SB_FAKEREPL_BUSY_MS", "800")],
        Some(2048),
    );

    assert!(
        outcome.accepted,
        "fragmented-in-delivery paste must be accepted"
    );
    assert_eq!(
        app_turns, 1,
        "fragmented-INSIDE-delivery paste lands EXACTLY ONE turn (app-output keyed)"
    );
}

/// `message` bytes of length `n` (the negative control's single-write payload).
fn msg_bytes(n: usize) -> Vec<u8> {
    vec![b'P'; n]
}

// ===========================================================================
// A4 FOLLOW-UP — CHUNKED PTY TEXT DELIVERY (ADR 0009 mode (a): tty-queue overflow).
//
// The merged two-write discipline operates ABOVE the transport: a single large
// `zmx send` overflows the ~4096B canonical tty input queue before claude's reader
// drains it and is DROPPED WHOLESALE — composer EMPTY, delta 0, did-not-go-busy
// (test/golden/dryrun/a4-r6-probe-evidence.md; 12KB+16KB on brano, TS ~4KB). The
// content-verified CR correctly fires nothing (nothing reached the composer). The
// FIX (parity port of 8c59ec4:src/commands/submit.ts) chunks the text into ≤1024B
// code-point-safe pieces ~150ms apart so the reader drains between writes.
//
// fakerepl models the CLASS via SB_FAKEREPL_DROP_OVER_BYTES: a single burst longer
// than the bound is dropped wholesale (no composer content, a `drop` report event).
// 4096 is a representative model default — the live boundary is machine/load
// dependent; the INVARIANT proven here is the ≤1024B chunk size.
// ===========================================================================

const JUMBO_BYTES: usize = 16 * 1024; // 16KB — the live EMPTY-DROPPED size on brano.

/// Chunk opts for the gate: default 1024B chunk, but an 80ms inter-chunk settle
/// (> the fakerepl's 50ms GAP, so each chunk closes as its OWN burst) — faster than
/// the 150ms production default while preserving the per-chunk-burst property the
/// overflow model needs.
fn gate_chunk_opts() -> dispatch::submit::ChunkSendOptions {
    dispatch::submit::ChunkSendOptions {
        chunk_bytes: 1024,
        settle_ms: 80,
    }
}

// ROW (i): a JUMBO 16KB payload through the REAL shared chunked helper
// (deliver_idle_two_write_with) lands EXACTLY ONE turn, and the FULL payload arrives
// (report's accepted-turn bytes == the sent byte length). No drop env here — this is
// the plain jumbo delivery. (No "NKb passes unchunked" assertion exists anywhere:
// that would codify boundary luck — the invariant is the chunk size, not any size.)
#[test]
fn jumbo_16kb_chunked_lands_exactly_one_turn_full_payload() {
    let jail = Jail::new("jumbo16k");
    let msg = "J".repeat(JUMBO_BYTES);
    let child = PtyChild::spawn(&jail, "jumbo16k", &[("SB_FAKEREPL_BUSY_MS", "800")]);
    let (app_turns, outcome) = drive_idle_chunked(&child, &msg, gate_chunk_opts());

    assert!(
        outcome.accepted,
        "chunked 16KB idle delivery must be accepted"
    );
    assert_eq!(
        app_turns, 1,
        "16KB via chunking lands EXACTLY ONE turn (app-output keyed)"
    );
    // The FULL payload reached the composer: the submitted turn's byte count equals
    // the sent payload length (no chunk dropped, none duplicated).
    assert_eq!(
        child.report_last_turn_bytes(),
        Some(JUMBO_BYTES as u64),
        "the whole 16KB payload landed in the composer (sent bytes == turn bytes)"
    );
    let _ = child.finish();
}

// ROW (ii): a MULTIBYTE-STRADDLE jumbo payload (構築日本語café☕ repeated across many
// 1024B chunk boundaries) delivered intact — the submitted turn's byte count equals
// the sent UTF-8 byte length, proving no chunk edge split a multibyte code point in
// transit (B3 F-2: boundary bugs are live).
#[test]
fn jumbo_multibyte_straddle_chunked_delivered_intact() {
    let jail = Jail::new("jumbomb");
    // 構築日本語café☕ = 構築日本語(5×3=15) + café(4+1=... c,a,f=3 + é=2 =5) + ☕(3) = 23 bytes,
    // 8 code points. Repeat to comfortably exceed 16KB and cross ~16 chunk boundaries.
    let unit = "構築日本語café☕";
    let reps = (JUMBO_BYTES / unit.len()) + 1;
    let msg = unit.repeat(reps);
    let sent_bytes = msg.len();
    let child = PtyChild::spawn(&jail, "jumbomb", &[("SB_FAKEREPL_BUSY_MS", "800")]);
    let (app_turns, outcome) = drive_idle_chunked(&child, &msg, gate_chunk_opts());

    assert!(
        outcome.accepted,
        "multibyte jumbo delivery must be accepted"
    );
    assert_eq!(app_turns, 1, "multibyte jumbo lands exactly one turn");
    assert_eq!(
        child.report_last_turn_bytes(),
        Some(sent_bytes as u64),
        "every multibyte code point survived chunking (sent UTF-8 bytes == turn bytes)"
    );
    let _ = child.finish();
}

// ROW (iii) NEGATIVE-CONTROL PAIRING — chunking is load-bearing.
//
// Under SB_FAKEREPL_DROP_OVER_BYTES=4096 (the tty-queue overflow model):
//   - the UNCHUNKED mutation (chunk_bytes = usize::MAX → one giant write) sends 16KB
//     as a SINGLE burst > 4096 → DROPPED WHOLESALE → ZERO turns, not accepted, a
//     `drop` event recorded. We ASSERT THE RED.
//   - the CHUNKED delivery (chunk_bytes = 1024) sends 16 bursts each ≤1024 < 4096 →
//     all pass the bound → ONE turn, accepted, ZERO drops. We ASSERT THE GREEN.
// The SAME drop env flips red→green purely on the chunk size — proving chunking is
// what carries the payload past the overflow, not incidental timing.
#[test]
fn negctl_unchunked_jumbo_drops_under_overflow_model_red() {
    let jail = Jail::new("negunchunk");
    let msg = "U".repeat(JUMBO_BYTES);
    let child = PtyChild::spawn(
        &jail,
        "negunchunk",
        &[
            ("SB_FAKEREPL_BUSY_MS", "800"),
            ("SB_FAKEREPL_DROP_OVER_BYTES", "4096"),
        ],
    );
    // UNCHUNKED mutation: chunk_bytes = usize::MAX → chunk_text yields ONE chunk →
    // one 16KB write → one burst > 4096 → dropped wholesale.
    let unchunked = dispatch::submit::ChunkSendOptions {
        chunk_bytes: usize::MAX,
        settle_ms: 80,
    };
    let (app_turns, outcome) = drive_idle_chunked(&child, &msg, unchunked);

    // THE RED: the unchunked jumbo is dropped before the composer — no turn, not
    // accepted. (The two-write CR + content-verified remediation correctly fire
    // nothing: the composer is EMPTY, exactly the live EMPTY-DROPPED mode.)
    assert_eq!(
        app_turns, 0,
        "UNCHUNKED 16KB is dropped wholesale by the overflow model — ZERO turns (the RED)"
    );
    assert!(
        !outcome.accepted,
        "unchunked jumbo never goes busy — the wholesale-drop mode (R6 EMPTY-DROPPED)"
    );
    assert!(
        child.report_drop_count() >= 1,
        "the overflow model recorded a wholesale drop of the giant write"
    );
    assert_eq!(
        child.report_last_turn_bytes(),
        None,
        "nothing reached the composer — no accepted turn at all"
    );
    let _ = child.finish();
}

#[test]
fn negctl_chunked_jumbo_passes_under_same_overflow_model_green() {
    let jail = Jail::new("negchunk");
    let msg = "C".repeat(JUMBO_BYTES);
    // SAME overflow env as the RED row — only the chunk size differs.
    let child = PtyChild::spawn(
        &jail,
        "negchunk",
        &[
            ("SB_FAKEREPL_BUSY_MS", "800"),
            ("SB_FAKEREPL_DROP_OVER_BYTES", "4096"),
        ],
    );
    // CHUNKED: 1024B chunks, each < the 4096 drop bound → every chunk passes.
    let (app_turns, outcome) = drive_idle_chunked(&child, &msg, gate_chunk_opts());

    // THE GREEN: under the identical drop env, chunking carries the whole payload.
    assert!(
        outcome.accepted,
        "CHUNKED 16KB passes the SAME overflow model the unchunked write failed"
    );
    assert_eq!(
        app_turns, 1,
        "chunked 16KB lands EXACTLY ONE turn under the overflow model (the GREEN)"
    );
    assert_eq!(
        child.report_drop_count(),
        0,
        "no chunk exceeds the 4096 bound → ZERO drops (chunk size is the invariant)"
    );
    assert_eq!(
        child.report_last_turn_bytes(),
        Some(JUMBO_BYTES as u64),
        "the full payload landed — every 1024B chunk drained under the queue bound"
    );
    let _ = child.finish();
}

// ===========================================================================
// ROW: L1-soak — ≥100 iterations, per-iteration knobs from a FIXED seed table
// (no RNG): zero dropped (every send → exactly one accepted turn), zero double.
// Tallied from APP-OUTPUT; cross-checked against report JSONL.
//
// NOT #[ignore] — runs in the normal gate. busy_ms is tuned low so wall-clock
// stays sane (<~3min): the distribution centers well below the spec's 1500ms cap
// since 100 iterations × high busy_ms would blow the budget.
// ===========================================================================

/// Deterministic per-iteration knobs (a4-spec §5: "fixed seed table"). A simple
/// reproducible table — NO RNG. (i) cycles busy_ms, msg_size, threshold.
///
/// DEVIATION FROM SPEC RANGE (surfaced to lead): the spec says vary busy_ms
/// "100-1500ms", but deliver_prompt polls the registry status every 250ms
/// (SubmitOptions::default().poll_ms — a fixed property of the SUT we drive, not
/// a knob). A busy window shorter than the poll interval + the fakerepl's ≤50ms
/// burst-close latency is fundamentally UNOBSERVABLE: the discipline would
/// (correctly) read idle across the whole window and judge the prompt
/// un-accepted, producing FALSE drops that are a harness artifact, not a
/// discipline failure. Real claude busy windows are SECONDS, so a sub-250ms
/// acceptance window is not a realistic signal. We therefore floor busy_ms at
/// 700ms (poll 250 + gap 50 + margin) and vary 700-1500 across the table. This
/// keeps the soak honest (the discipline is genuinely exercised) and bounded
/// (avg ~900ms × 100 ≈ 90s busy + spawn/PTY overhead < 3min).
fn soak_knobs(i: usize) -> (u64, usize, usize) {
    // busy_ms: poll-safe range (≥700; see the doc-comment deviation note),
    // weighted toward the low end to keep the 100-iteration wall-clock sane.
    //
    // The ≥700 floor is RATIFIED, not arbitrary (lead ratification at M4a review;
    // orc-2 ruling 2026-06-05, relay-1780630993819-7 item 2): a sub-~300ms busy
    // window is unobservable at the SUT's OWN 250ms status poll — an
    // INHERITED-LIMITATION shared with TS production (same poll constant,
    // submit.ts pollMs 250) — so pinning gate determinism to it would be theater,
    // and "fixing" it Rust-side would be silent divergence. LESSONS L14 carries
    // the limitation with the TS citation.
    const BUSY: [u64; 6] = [700, 700, 800, 800, 1000, 1500];
    // msg_size: 10B .. 8KB, deterministic spread.
    const SIZE: [usize; 6] = [10, 64, 400, 1024, 4096, 8192];

    let busy = BUSY[i % BUSY.len()];
    let size = SIZE[i % SIZE.len()];

    // WALL-CLOCK NOTE: a REMEDIATION iteration (msg is a paste, trailing \r
    // absorbed) costs the discipline's full 2.5s settle window before it fires
    // the CR; an AUTO-SUBMIT iteration (msg below threshold → the \r submits)
    // returns as soon as busy appears. With 100 iterations a 50/50 split runs
    // ~4-5min. To stay under the ~3min budget while STILL exercising BOTH paths
    // (the spec wants the threshold varied so some auto-submit and some
    // remediate), every 3rd iteration is a remediation case (threshold forced
    // BELOW the message size) and the rest auto-submit (threshold ABOVE the
    // write size = msg+1). Both paths must still land EXACTLY ONE turn.
    let threshold = if i.is_multiple_of(3) {
        // Remediation: well below msg size so the whole write is a paste.
        2
    } else {
        // Auto-submit: above the write length (msg + the trailing '\r').
        size + 2
    };
    (busy, size, threshold)
}

#[test]
fn l1_soak_zero_dropped_zero_double() {
    // 100 by default (the gate). FR_SOAK_ITERS overrides for quick local probes.
    let iters: usize = std::env::var("FR_SOAK_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let start = Instant::now();
    let mut dropped = 0usize;
    let mut doubled = 0usize;
    let mut mismatches = 0usize;

    let timing = std::env::var("FR_SOAK_TIMING").is_ok();
    for i in 0..iters {
        let (busy_ms, size, threshold) = soak_knobs(i);
        let iter_start = Instant::now();
        let jail = Jail::new(&format!("soak{i}"));
        let name = format!("soak{i}");
        let msg = "S".repeat(size);
        let (app_turns, report_turns, outcome) = drive_once(
            &jail,
            &name,
            &msg,
            &[
                ("SB_FAKEREPL_BUSY_MS", &busy_ms.to_string()),
                ("SB_FAKEREPL_PASTE_THRESHOLD", &threshold.to_string()),
            ],
        );

        if outcome != DeliverOutcome::Accepted || app_turns == 0 {
            dropped += 1;
            eprintln!(
                "soak[{i}] DROPPED: outcome={outcome:?} app_turns={app_turns} \
                 (busy={busy_ms} size={size} thresh={threshold})"
            );
        }
        if app_turns > 1 {
            doubled += 1;
            eprintln!("soak[{i}] DOUBLE: app_turns={app_turns}");
        }
        if app_turns != report_turns {
            mismatches += 1;
            eprintln!("soak[{i}] REPORT MISMATCH: app={app_turns} report={report_turns}");
        }
        if timing {
            eprintln!(
                "soak[{i}] {:.0}ms (busy={busy_ms} size={size} thresh={threshold} \
                 turns={app_turns} {outcome:?})",
                iter_start.elapsed().as_millis()
            );
        }
        if (i + 1).is_multiple_of(10) {
            eprintln!(
                "soak progress: {}/{iters} in {:.1}s (dropped={dropped} \
                 doubled={doubled} mismatch={mismatches})",
                i + 1,
                start.elapsed().as_secs_f64()
            );
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "soak: {iters} iters in {:.1}s — dropped={dropped} doubled={doubled} \
         report_mismatches={mismatches}",
        elapsed.as_secs_f64()
    );
    assert_eq!(
        dropped, 0,
        "zero dropped sends (every send → ≥1 accepted turn)"
    );
    assert_eq!(doubled, 0, "zero double turns");
    assert_eq!(
        mismatches, 0,
        "app-output and report turn counts must agree"
    );
}

// ===========================================================================
// NEGATIVE CONTROL A — inject a CR while busy. The report must record
// cr_while_busy>0 AND the harness check MUST fail (mutation-test style: we
// assert the RED is produced, proving the oracle sees the violation).
// ===========================================================================

#[test]
fn neg_control_a_cr_while_busy_is_detected_and_fails() {
    let jail = Jail::new("negA");
    let name = "negA";
    // Wide busy window (1500ms) so a CR injected just after we observe busy lands
    // squarely INSIDE it — deterministic, not timing-racy.
    let child = PtyChild::spawn(&jail, name, &[("SB_FAKEREPL_BUSY_MS", "1500")]);
    let pid_file = child.wait_for_row(5000).expect("row");

    // Submit turn 1: a small non-paste content byte, then a lone CR (non-paste
    // keystroke → SUBMIT) → status busy. (The content byte is required now that an
    // EMPTY-composer CR is a no-op — claude does not start a turn for an empty
    // prompt; the A4-follow-up overflow model relies on that, see fakerepl
    // handle_cr. The control's INTENT — reach busy, then inject a stray CR while
    // busy — is unchanged; only the empty submit became a realistic 1-byte submit.)
    child.write_raw(b"x");
    child.write_raw(b"\r");
    // Wait until status is observably busy (poll the registry), bounded.
    let busy_deadline = Instant::now() + Duration::from_millis(1500);
    while read_pid_status(&pid_file).as_deref() != Some("busy") {
        if Instant::now() >= busy_deadline {
            break;
        }
        settle(10);
    }
    assert_eq!(
        read_pid_status(&pid_file).as_deref(),
        Some("busy"),
        "turn 1 must be busy before we inject the stray CR"
    );

    // Inject a stray CR WHILE busy — this is the violation the oracle must catch.
    // (busy_ms=1500 leaves plenty of window after the ~busy-onset.)
    child.write_raw(b"\r");
    // Wait out the busy hold so the cr_while_busy event is flushed to the report.
    settle(1800);

    let crs_while_busy = child.report_cr_while_busy();
    let _ = child.finish();

    // The harness CHECK that this control is designed to TRIP: "no CR may arrive
    // while busy". We assert it is RED (mutation-test style).
    let harness_check_passes = crs_while_busy == 0;
    assert!(
        !harness_check_passes,
        "neg-control A: the oracle MUST see cr_while_busy>0 (got {crs_while_busy}) \
         and the no-busy-CR check MUST fail"
    );
}

// ===========================================================================
// NEGATIVE CONTROL B (W8) — swallow the remediation CR, threshold forced BELOW
// every message size so the trailing \r is ALWAYS absorbed → the remediation CR
// is load-bearing on EVERY iteration → the swallow is always detectable. The
// soak iteration MUST go red; we assert the harness detects the drop.
// ===========================================================================

#[test]
fn neg_control_b_swallowed_remediation_cr_goes_red() {
    let jail = Jail::new("negB");
    let name = "negB";
    let mut child = PtyChild::spawn(
        &jail,
        name,
        &[
            ("SB_FAKEREPL_BUSY_MS", "100"),
            // W8 pinned config: threshold below the message size so the message's
            // trailing \r is ALWAYS absorbed (paste burst) — remediation is the
            // ONLY way to submit.
            ("SB_FAKEREPL_PASTE_THRESHOLD", "4"),
        ],
    );
    child.swallow_cr = true; // the mutation: drop every remediation CR
    child.wait_for_row(5000).expect("row");

    let msg = "B".repeat(64); // 64 >> threshold 4 → always a paste
    let deps = PtyDeliverDeps { child: &child };
    let outcome = deliver_prompt(&deps, &msg, /*short timeout*/ 2);
    settle(200);

    let app_turns = child.accepted_turns();
    let _ = child.finish();

    // With the remediation CR swallowed AND the \r always absorbed, NOTHING ever
    // submits: zero accepted turns and a Stalled outcome. The soak's
    // "every send → exactly one accepted turn" invariant is VIOLATED — assert the
    // harness detects it (mutation-test style: assert RED).
    assert_eq!(
        app_turns, 0,
        "swallowed remediation CR → zero turns (drop detected)"
    );
    assert_ne!(
        outcome,
        DeliverOutcome::Accepted,
        "swallowed CR must NOT be accepted — the soak invariant goes red"
    );
}

// ===========================================================================
// JAIL-REFUSAL rows: clean env → 13; partial spoof → 13; valid jail → starts
// (asserted by every other row implicitly; here we assert the exit-13 stderr
// names the failed check).
// ===========================================================================

/// Run fakerepl with a given env and NO stdin, capturing (exit_code, stderr).
fn run_fakerepl_env(env: &[(&str, &str)]) -> (i32, String) {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(fakerepl_bin());
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let out = cmd.output().expect("spawn fakerepl");
    let code = out.status.code().unwrap_or(-1);
    (code, String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn jail_refusal_clean_env_exits_13() {
    // No HOME at all → refuse, naming HOME.
    let (code, stderr) = run_fakerepl_env(&[]);
    assert_eq!(code, 13, "clean env must exit 13: stderr={stderr}");
    assert!(
        stderr.contains("HOME"),
        "stderr must name the failed check (HOME): {stderr}"
    );
}

#[test]
fn jail_refusal_partial_spoof_exits_13() {
    // HOME jail-shaped but SB_HOME points elsewhere → refuse, naming SB_HOME (the
    // coherence check, not just HOME).
    let jail = Jail::new("spoof");
    let env = [
        ("HOME", jail.home.to_str().unwrap()),
        ("SB_HOME", "/elsewhere/sb_home"),
        ("ZMX_DIR", jail.zmx.to_str().unwrap()),
        ("TMPDIR", jail.tmp.to_str().unwrap()),
    ];
    let (code, stderr) = run_fakerepl_env(&env);
    assert_eq!(code, 13, "partial spoof must exit 13: stderr={stderr}");
    assert!(
        stderr.contains("SB_HOME"),
        "stderr must name the failed coherence check (SB_HOME): {stderr}"
    );
}

#[test]
fn jail_refusal_valid_jail_starts() {
    // A valid jail layout must NOT refuse — it writes a registry row and waits on
    // stdin. We spawn, confirm the row appears (proves it got past the belt), then
    // SIGTERM it (finish()): the handler unlinks the row and exits 0.
    let jail = Jail::new("valid");
    let child = PtyChild::spawn(&jail, "valid", &[("SB_FAKEREPL_BUSY_MS", "50")]);
    let row = child.wait_for_row(5000);
    assert!(
        row.is_some(),
        "valid jail must start (registry row must appear)"
    );
    let row_path = row.unwrap();
    let code = child.finish();
    // Clean SIGTERM exit via the handler (exit 0) AND the registry row removed.
    assert_eq!(code, 0, "valid jail clean-exits 0 on SIGTERM (handler)");
    assert!(
        !row_path.exists(),
        "SIGTERM handler must unlink the registry row"
    );
}

// ===========================================================================
// COALESCING NOTE (W7) — MEASURE portable-pty chunk coalescing for the README.
//
// The spec requires the 50ms gap constant be MEASURED, not assumed. We write a
// split payload to the PTY master with a >50ms stall between halves and observe,
// via the fakerepl's report, whether the two writes coalesce into one burst or
// land as two. We print the observation; the README records it. This is a
// MEASUREMENT, not a hard assertion (timing-variable), but we DO assert the two
// halves did NOT collapse into a single sub-50ms burst (the gap is real).
// ===========================================================================

#[test]
fn coalescing_note_measures_pty_burst_boundaries() {
    let jail = Jail::new("coalesce");
    let name = "coalesce";
    // High threshold so neither half submits regardless; we only care about burst
    // SIZES the fakerepl reports.
    let child = PtyChild::spawn(
        &jail,
        name,
        &[
            ("SB_FAKEREPL_BUSY_MS", "50"),
            ("SB_FAKEREPL_PASTE_THRESHOLD", "1000000"),
        ],
    );
    child.wait_for_row(5000).expect("row");

    // Write two 100-byte halves with a 120ms stall between (well over the 50ms
    // gap). Expectation: TWO bursts (the gap forces a boundary).
    let half = "C".repeat(100);
    child.write_raw(half.as_bytes());
    settle(120);
    child.write_raw(half.as_bytes());
    settle(120);

    let bursts: Vec<u64> = child
        .report_events()
        .iter()
        .filter(|e| e.get("event").and_then(|v| v.as_str()) == Some("burst"))
        .filter_map(|e| e.get("size").and_then(|v| v.as_u64()))
        .collect();
    let _ = child.finish();

    eprintln!(
        "COALESCING NOTE (portable-pty, this machine): split 100B+[120ms gap]+100B \
         observed burst sizes = {bursts:?}"
    );

    // A real >50ms gap MUST produce a burst boundary: at least 2 bursts, and no
    // single burst swallowed both halves (200B).
    assert!(
        bursts.len() >= 2,
        "a >50ms stall must force ≥2 bursts (gap is real), got {bursts:?}"
    );
    assert!(
        !bursts.iter().any(|&s| s >= 200),
        "no single burst may swallow both 100B halves across the gap: {bursts:?}"
    );
}

// ===========================================================================
// W8 — verify-after-submit (silent mid-truncation closure; A4 R1 / D16 flip).
//
// The fakerepl now models the D16 reader-stall window: under
// SB_FAKEREPL_STALL_AFTER_BYTES/_MS/_QUEUE_CAP a mid-delivery reader pause drops
// payload bytes past the queue cap (saturation) — the silent loss the existing
// went-busy acceptance cannot see. SB_FAKEREPL_CONVO_JSONL gives the verify step a
// claude-shaped transcript to read back. These rows prove:
//   - the silent-loss window is REAL (the convo record is SHORTER than sent) and
//     the UNVERIFIED delivery path reports accepted/success (the pre-fix silence);
//   - verify_chunked_payload over the real-fs transcript catches it (Truncated);
//   - slow-but-complete delivery stays Verified (no false positive);
//   - a foreign-only record degrades (Unattributable, never Truncated);
//   - the mutation (delivering through the path WITHOUT the verify wrapper) leaves
//     the loss silent — the read-back, and ONLY it, is the belt;
//   - a single-chunk submit never triggers verify (scope guard, zero reads).
// ===========================================================================

/// A real-fs [`VerifyDeps`] reading a fakerepl convo JSONL from byte `offset` to
/// EOF: slice → [`parse_jsonl_slice`] → [`user_record_text`] collects the user
/// texts in file order (the exact real-wiring shape M5 binds). `Err` if the file
/// shrank below the offset (resolution failed this poll). `reads` counts polls so
/// the scope-guard row can assert ZERO reads on a single-chunk submit.
struct ConvoVerifyDeps {
    path: PathBuf,
    offset: u64,
    clock: RealClock,
    sleeper: RealSleeper,
    reads: std::cell::Cell<u32>,
}

impl ConvoVerifyDeps {
    fn new(path: PathBuf, offset: u64) -> Self {
        Self {
            path,
            offset,
            clock: RealClock,
            sleeper: RealSleeper,
            reads: std::cell::Cell::new(0),
        }
    }
    fn reads(&self) -> u32 {
        self.reads.get()
    }
}

impl VerifyDeps for ConvoVerifyDeps {
    fn read_user_texts(&self) -> Result<Vec<String>, String> {
        self.reads.set(self.reads.get() + 1);
        let bytes = std::fs::read(&self.path).map_err(|e| format!("read convo: {e}"))?;
        let off = self.offset as usize;
        if bytes.len() < off {
            return Err(format!(
                "convo shrank past offset ({} < {off})",
                bytes.len()
            ));
        }
        let slice = String::from_utf8_lossy(&bytes[off..]);
        let texts = dispatch::sendpty::parse_jsonl_slice(&slice)
            .iter()
            .filter_map(|p| {
                let rec: dispatch::sendpty::JsonlRecord =
                    serde_json::from_value(p.value.clone()).unwrap_or_default();
                dispatch::sendpty::user_record_text(&rec)
            })
            .collect();
        Ok(texts)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// Read the LAST user-record text from a fakerepl convo JSONL (None if absent /
/// empty). The gate keys the silent-loss assertion on this (the submitted composer
/// content the fakerepl recorded), NOT on app-output.
fn convo_last_user_text(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    dispatch::sendpty::parse_jsonl_slice(&text)
        .iter()
        .filter_map(|p| {
            let rec: dispatch::sendpty::JsonlRecord =
                serde_json::from_value(p.value.clone()).unwrap_or_default();
            dispatch::sendpty::user_record_text(&rec)
        })
        .next_back()
}

/// The stall-seam env the differential + slow-but-complete rows share, with the
/// queue cap injected (the only differing knob between RED and the negctl).
fn w8_stall_env<'a>(convo: &'a str, queue_cap: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("SB_FAKEREPL_BUSY_MS", "800"),
        ("SB_FAKEREPL_STALL_AFTER_BYTES", "3072"),
        ("SB_FAKEREPL_STALL_MS", "800"),
        ("SB_FAKEREPL_STALL_QUEUE_CAP", queue_cap),
        ("SB_FAKEREPL_CONVO_JSONL", convo),
    ]
}

// ROW: RED-without-fix differential — a 16KB chunked delivery under the stall seam
// truncates the composer SILENTLY (the unverified delivery reports accepted), and
// verify_chunked_payload over the convo transcript catches it (Truncated).
#[test]
fn w8_red_differential_stall_truncates_and_unverified_helper_stays_silent() {
    let jail = Jail::new("w8red");
    let msg = "J".repeat(JUMBO_BYTES); // 16KB → many chunks
    assert!(
        payload_needs_verify(&msg),
        "16KB is multi-chunk → verify is in scope"
    );
    let convo = jail.tmp.join("convo-w8red.jsonl");
    let convo_s = convo.to_string_lossy().into_owned();

    // STALL_QUEUE_CAP=2048 (< the in-flight volume during the 800ms pause) → the
    // reader saturates and mid-payload bytes drop.
    let child = PtyChild::spawn(&jail, "w8red", &w8_stall_env(&convo_s, "2048"));
    let (app_turns, outcome) = drive_idle_chunked(&child, &msg, gate_chunk_opts());

    // The PRE-FIX SILENCE: the unverified delivery path reports accepted/success
    // (went-busy is the only signal it has) AND exactly one turn landed.
    assert!(
        outcome.accepted,
        "the unverified delivery path reports ACCEPTED (went-busy) — the silence"
    );
    assert_eq!(app_turns, 1, "one turn landed (went busy) despite the loss");

    // The silent-loss window is REAL: the recorded user text is SHORTER than the
    // sent payload (mid-payload bytes dropped at the saturation boundary).
    let recorded = convo_last_user_text(&convo)
        .expect("a user record was written for the submitted (truncated) turn");
    assert!(
        recorded.len() < msg.len(),
        "the composer was truncated: recorded {} < sent {} (silent loss)",
        recorded.len(),
        msg.len()
    );
    assert!(
        msg.as_bytes()
            .starts_with(&recorded.as_bytes()[..64.min(recorded.len())]),
        "the truncated record still shares the message's leading bytes"
    );
    let _ = child.finish();

    // THE FIX: verify_chunked_payload over the real-fs convo transcript (offset 0,
    // a fresh file) catches the truncation, naming expected/recorded.
    let deps = ConvoVerifyDeps::new(convo.clone(), 0);
    let out = verify_chunked_payload(&deps, &msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
    match out {
        PayloadVerifyOutcome::Truncated {
            expected,
            recorded: rec,
        } => {
            assert_eq!(expected, msg.len(), "expected = sent byte length");
            assert!(rec < expected, "recorded {rec} < expected {expected}");
            assert_eq!(
                rec,
                recorded.len(),
                "recorded = the truncated record length"
            );
        }
        other => panic!("expected Truncated, got {other:?}"),
    }
}

// ROW: negative control — slow-but-complete reader (same 800ms stall) with the
// queue cap comfortably ABOVE the in-flight volume → the full payload lands;
// verify → Verified. No false positive on a slow-but-lossless reader.
#[test]
fn w8_negctl_slow_but_complete_stays_verified() {
    let jail = Jail::new("w8slow");
    let msg = "S".repeat(JUMBO_BYTES);
    let convo = jail.tmp.join("convo-w8slow.jsonl");
    let convo_s = convo.to_string_lossy().into_owned();

    // SAME 800ms stall, but cap = 64KB (>> the 16KB payload) → nothing drops.
    let child = PtyChild::spawn(&jail, "w8slow", &w8_stall_env(&convo_s, "65536"));
    let (app_turns, outcome) = drive_idle_chunked(&child, &msg, gate_chunk_opts());

    assert!(outcome.accepted, "slow-but-complete delivery is accepted");
    assert_eq!(app_turns, 1, "exactly one turn");
    let recorded = convo_last_user_text(&convo).expect("user record written");
    assert_eq!(
        recorded.len(),
        msg.len(),
        "the FULL payload landed (slow but lossless): recorded == sent"
    );
    let _ = child.finish();

    let deps = ConvoVerifyDeps::new(convo.clone(), 0);
    let out = verify_chunked_payload(&deps, &msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
    assert_eq!(
        out,
        PayloadVerifyOutcome::Verified,
        "verify must NOT punish a slow-but-lossless reader"
    );
}

// ROW: negative control — a FOREIGN user record (unrelated text) and never ours →
// Unattributable (degrade), NOT Truncated. Plus foreign + ours → Verified.
#[test]
fn w8_negctl_foreign_record_degrades_not_truncation() {
    let jail = Jail::new("w8foreign");
    let convo = jail.tmp.join("convo-w8foreign.jsonl");
    // Hand-write a foreign user record (no fakerepl needed — this is a pure
    // transcript-shape control over the real-fs deps).
    let foreign = serde_json::json!({
        "type": "user",
        "message": { "content": "a completely unrelated user message" },
    });
    std::fs::write(&convo, format!("{foreign}\n")).expect("write convo");

    let our_msg = "P".repeat(2048); // multi-chunk, in verify scope
    let deps = ConvoVerifyDeps::new(convo.clone(), 0);
    let out = verify_chunked_payload(&deps, &our_msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
    assert_eq!(
        out,
        PayloadVerifyOutcome::Unattributable,
        "a foreign-only record degrades — NOT a false truncation"
    );

    // Now append OUR intact record → exact match wins → Verified.
    let ours = serde_json::json!({
        "type": "user",
        "message": { "content": our_msg },
    });
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&convo)
            .expect("append convo");
        writeln!(f, "{ours}").expect("write our record");
    }
    let deps2 = ConvoVerifyDeps::new(convo, 0);
    let out2 = verify_chunked_payload(&deps2, &our_msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
    assert_eq!(
        out2,
        PayloadVerifyOutcome::Verified,
        "foreign + ours: the exact match wins"
    );
}

// ROW: MUTATION — the same injected truncation driven through the delivery WITHOUT
// the verify wrapper surfaces NO error anywhere. The mutation is WHICH LIBRARY
// ENTRY the gate calls: the delivery path alone (no verify_chunked_payload) reports
// plain success, proving the read-back — and ONLY it — is the belt. No test-bypass
// knob exists in the production binary (the truncation is injected by the fakerepl
// stall seam, a harness knob, not a production code path).
#[test]
fn w8_mutation_unverified_entry_is_silent() {
    let jail = Jail::new("w8mut");
    let msg = "M".repeat(JUMBO_BYTES);
    let convo = jail.tmp.join("convo-w8mut.jsonl");
    let convo_s = convo.to_string_lossy().into_owned();

    let child = PtyChild::spawn(&jail, "w8mut", &w8_stall_env(&convo_s, "2048"));
    let (app_turns, outcome) = drive_idle_chunked(&child, &msg, gate_chunk_opts());

    // The delivery path (the unverified library entry) reports SUCCESS — accepted,
    // one turn — even though the payload was truncated. NO error surfaces because
    // verify_chunked_payload was never called.
    assert!(
        outcome.accepted,
        "the unverified entry reports accepted (the silent mutation)"
    );
    assert_eq!(app_turns, 1, "one turn — the delivery path saw success");
    let recorded = convo_last_user_text(&convo).expect("a (truncated) user record was written");
    assert!(
        recorded.len() < msg.len(),
        "the loss IS present in the transcript ({} < {}), but the unverified path \
         surfaced nothing — only the read-back would catch it",
        recorded.len(),
        msg.len()
    );
    let _ = child.finish();
}

// ROW: single-chunk scope guard — a ≤1024B delivery: payload_needs_verify is FALSE
// and verify is never invoked (zero reads via a call-counting deps). Pins that the
// scope guard keeps single-chunk submits byte-for-byte unchanged.
#[test]
fn w8_single_chunk_scope_guard_no_verify_zero_reads() {
    let msg = "g".repeat(1024); // exactly one chunk
    assert!(
        !payload_needs_verify(&msg),
        "a ≤1024B single-chunk submit is OUT of verify scope (scope guard)"
    );

    // The production wiring (M5) gates the verify call on payload_needs_verify;
    // model that gate here and assert the deps are NEVER touched.
    let jail = Jail::new("w8single");
    let convo = jail.tmp.join("convo-w8single.jsonl");
    std::fs::write(&convo, "").expect("touch convo");
    let deps = ConvoVerifyDeps::new(convo, 0);
    if payload_needs_verify(&msg) {
        let _ = verify_chunked_payload(&deps, &msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
    }
    assert_eq!(
        deps.reads(),
        0,
        "a single-chunk submit triggers ZERO verify reads (no new fs cost)"
    );
}
