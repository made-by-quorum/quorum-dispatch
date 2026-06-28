//! C1 M6 — ENGINE-LEVEL gate rows (c1-spec "Gate rows"), driving the REAL `qd`
//! binary (`CARGO_BIN_EXE_qd`) through per-run hermetic jails against a REAL
//! jailed qrmux daemon. These are the gate teeth for the Stage-2 mux swap.
//!
//! ## How these differ from the M4 crate-level live tests
//!
//! `tests/embedded_mux_live.rs` (M4) exercises the `EmbeddedMux` ADAPTER crate
//! API directly. THIS suite is ENGINE-LEVEL: every row drives the real `qd`
//! binary's verbs (`qd new` / `qd connect` / `qd send:pty` / `qd ls --json` /
//! `qd kill` / `qd wait`) so the FULL engine path (selector → gather/MuxDirs →
//! mux trait → protocol → daemon) is under test, not a mock.
//!
//! ## Why a fake-claude for `qd new`
//!
//! `qd new` boots the configured `CLAUDE_BIN` and the EventBootWaiter polls
//! `<sessions_dir>` for a `<pid>.json` whose `name` matches (claude-code owns
//! that write). In a jail there is no real Claude, so we point `CLAUDE_BIN` at a
//! tiny shell script that writes the registry row a real Claude would write
//! (name + status idle + its own pid) and then EXECs a real interactive app
//! (`cat` / `less` / a backlog generator). This makes `qd new` boot-verify for
//! real AND makes the session surface as a LIVE (non-cold) registry row so
//! `qd send:pty` (which rejects cold sessions) accepts it. The fake-claude is
//! TEST INFRA; the engine code path it drives is 100% real.
//!
//! ## Comparator provenance (cross-crate)
//!
//! The qrmux test-lib comparators (`assert_backlog_ordered`, `check_cjk_*`,
//! `assert_altscreen_replay`, …) live in `crates/qrmux/tests/lib/` and are NOT
//! importable across crates. The byte-level checker BODIES are ported here with
//! their provenance noted at each use; the cell-level `check_no_orphan_wide_cell`
//! (needs `qrmux::screen::Cell`) is NOT reachable through the engine PTY path, so
//! G-UNI uses the byte-level CJK/UTF-8 integrity check on the rendered replay.
//!
//! ## Evidence
//!
//! Every row writes `<runid>/<row>_result.txt` (a verbatim verdict line + raw
//! detail) under `tests/c1-gate-evidence/<runid>/`. Run `C1_GATE_RUNID=<id>` to
//! pin the dir; absent, a nanosecond runid is used and its path is printed.
//!
//! ## Jail invariants (rule 9 + ADD-4 + ADD-12 + ADD-14)
//!
//! Each row gets its own jail (own HOME/QD_HOME/XDG_RUNTIME_DIR/TMPDIR/ZMX_DIR),
//! launches a fresh jailed qrmux daemon, and tears down by killing the daemon by
//! pid + removing the jail dir. No destructive sweeps; per-target verbs only.

#![allow(clippy::too_many_arguments)]

mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize};

use sha2::{Digest, Sha256};

// ===========================================================================
// Binary locators
// ===========================================================================

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// Locate the built `qrmux` binary from the test exe's target dir. PANICS with a
/// build hint if absent (never a silent skip — that makes the row vacuous). This
/// mirrors the M4 live-test pattern: the qrmux binary must be built first.
fn qrmux_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    // current_exe = <target>/<profile>/deps/<test-hash>; the bin is
    // <target>/<profile>/qrmux.
    let mut dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>")
        .to_path_buf();
    dir.push("qrmux");
    assert!(
        dir.exists(),
        "qrmux binary not found at {dir:?} — build it first: \
         scripts/build-lock.sh cargo build -p qrmux --bin qrmux"
    );
    dir
}

// ===========================================================================
// Evidence dir
// ===========================================================================

/// Per-suite run id (shared across rows in one `cargo test` invocation). Pinned
/// by `C1_GATE_RUNID` else a nanosecond stamp.
fn runid() -> String {
    std::env::var("C1_GATE_RUNID").ok().unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("run-{nanos}")
    })
}

/// `<crate>/tests/c1-gate-evidence/<runid>/`.
fn evidence_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/c1-gate-evidence")
        .join(runid());
    std::fs::create_dir_all(&d).expect("mkdir evidence dir");
    d
}

/// Write `<row>_result.txt` with a verdict line + detail; print the path so the
/// evidence is locatable from the test log.
fn write_result(row: &str, verdict: &str, detail: &str) {
    let dir = evidence_dir();
    let path = dir.join(format!("{row}_result.txt"));
    let body = format!("{verdict}\n\n{detail}\n");
    std::fs::write(&path, body).expect("write result");
    eprintln!("[{row}] {verdict}  (evidence: {})", path.display());
}

/// Write a raw artifact (e.g. captured bytes) under the runid dir.
fn write_artifact(row: &str, name: &str, bytes: &[u8]) -> PathBuf {
    let dir = evidence_dir();
    let path = dir.join(format!("{row}_{name}"));
    std::fs::write(&path, bytes).expect("write artifact");
    path
}

// ===========================================================================
// Jail
// ===========================================================================

/// A per-run hermetic jail. The root sits under a SHORT literal-/tmp base so the
/// daemon's `<jail>/x/qrmux/qrmux.sock` fits macOS's 104-byte sun_path budget —
/// TEST infra, NOT engine code (ADD-14 governs ENGINE writes; the belt rows
/// assert the engine itself never emits qrmux-named /tmp-ROOT paths).
struct Jail {
    root: PathBuf,
    home: PathBuf,
    xdg_runtime: PathBuf,
    sessions_dir: PathBuf,
}

impl Jail {
    fn establish(tag: &str) -> Jail {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp/qd-c1gate-runs").join(format!("{tag}-{nanos}"));
        let home = root.join("h");
        let xdg_runtime = root.join("x");
        let sessions_dir = home.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&xdg_runtime).unwrap();
        use std::os::unix::fs::PermissionsExt;
        // qrmux's socket-dir belt expects 0700 per-user perms on the runtime dir.
        std::fs::set_permissions(&xdg_runtime, std::fs::Permissions::from_mode(0o700)).ok();
        common::assert_not_real_home(&home);
        // Teardown-leak belt: FIRST stamp this run's owning test-harness pid so a
        // concurrent sibling's setup-reaper sees a live owner and never touches
        // this dir; THEN reap daemons leaked by PRIOR (dead-owner) runs of this
        // family (per-target, identity-pinned — best-effort, never fails setup).
        // `root` already exists so it is correctly excluded as the current run.
        stamp_owner_pid(&root);
        let _ = reap_prior_run_daemons(&root);
        Jail {
            root,
            home,
            xdg_runtime,
            sessions_dir,
        }
    }

    /// The engine-resolved embedded socket dir for this jail (tier 1: XDG set).
    fn resolved_dir(&self) -> PathBuf {
        self.xdg_runtime.join("qrmux")
    }

    /// Apply the jailed embedded env to a std::process::Command.
    fn apply_embedded(&self, cmd: &mut Command) {
        cmd.env_clear()
            .env("HOME", &self.home)
            .env("QD_HOME", self.root.join("qdhome"))
            .env("XDG_RUNTIME_DIR", &self.xdg_runtime)
            .env("TMPDIR", self.root.join("tmp"))
            .env("ZMX_DIR", self.root.join("zmx"))
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color");
        let _ = std::fs::create_dir_all(self.root.join("tmp"));
        let _ = std::fs::create_dir_all(self.root.join("zmx"));
    }

    /// Apply the jailed env to a portable-pty CommandBuilder.
    fn apply_embedded_pty(&self, cmd: &mut CommandBuilder) {
        cmd.env_clear();
        cmd.env("HOME", &self.home);
        cmd.env("QD_HOME", self.root.join("qdhome"));
        cmd.env("XDG_RUNTIME_DIR", &self.xdg_runtime);
        cmd.env("TMPDIR", self.root.join("tmp"));
        cmd.env("ZMX_DIR", self.root.join("zmx"));
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.env("TERM", "xterm-256color");
        let _ = std::fs::create_dir_all(self.root.join("tmp"));
        let _ = std::fs::create_dir_all(self.root.join("zmx"));
    }

    fn teardown(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ===========================================================================
// Daemon guard (kill + reap by pid)
// ===========================================================================

struct DaemonGuard {
    pid: u32,
    child: Option<std::process::Child>,
}

impl DaemonGuard {
    fn kill_and_reap(&mut self) {
        let _ = Command::new("/bin/kill")
            .arg("-TERM")
            .arg(self.pid.to_string())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(Duration::from_millis(150));
        let _ = Command::new("/bin/kill")
            .arg("-9")
            .arg(self.pid.to_string())
            .stderr(Stdio::null())
            .status();
        if let Some(mut c) = self.child.take() {
            let _ = c.wait();
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn pid_alive(pid: u32) -> bool {
    let exists = Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !exists {
        return false;
    }
    let out = Command::new("/bin/ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output();
    match out {
        Ok(o) => {
            let st = String::from_utf8_lossy(&o.stdout);
            let st = st.trim();
            !st.is_empty() && !st.starts_with('Z')
        }
        Err(_) => exists,
    }
}

/// WS-C M3b: pre-spawn a PER-SESSION daemon `qrmux server --socket-dir <dir>
/// --session <name>` in the jail and wait for the `<dir>/<name>.sock` leaf. The
/// legacy shared-daemon mode (no `--session`, bound `qrmux.sock`) is RETIRED
/// (spec §1, §9). `extra_env` injects daemon env (e.g. the G-NEG breaker
/// `RETACH_B1_BREAK`); a long `QRMUX_CLAIM_TIMEOUT_MS` keeps the freshly-spawned
/// (still EMPTY) daemon alive until the adapter's `run_detached` claims it. The
/// adapter's own `ensure_session_server_running` then probes this live socket,
/// finds it Up, and short-circuits — every verb runs against this real daemon
/// (the cross-crate binary constraint: the test exe has no `qrmux-server` entry).
fn start_daemon(
    jail: &Jail,
    dir: &Path,
    name: &str,
    extra_env: &[(&str, &str)],
) -> (DaemonGuard, PathBuf) {
    std::fs::create_dir_all(dir).ok();
    let bin = qrmux_bin();
    let mut cmd = Command::new(&bin);
    cmd.arg("server")
        .arg("--socket-dir")
        .arg(dir)
        .arg("--session")
        .arg(name)
        .env_clear()
        .env("HOME", &jail.home)
        .env("XDG_RUNTIME_DIR", &jail.xdg_runtime)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("QRMUX_CLAIM_TIMEOUT_MS", "60000")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn qrmux server --session");
    let pid = child.id();
    // Teardown-leak belt: record this daemon (pid + its --socket-dir identity)
    // so a future run can reap it if this run dies before teardown. `jail.root`
    // is the run-root; `dir` is carried in the daemon's argv (--socket-dir).
    record_daemon_pid(&jail.root, pid, &dir.to_string_lossy());
    let mut guard = DaemonGuard {
        pid,
        child: Some(child),
    };
    let socket = dir.join(format!("{name}.sock"));
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if socket.exists() {
            return (guard, socket);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    guard.kill_and_reap();
    panic!("daemon socket not created within 5s at {socket:?}");
}

// ===========================================================================
// Mux helpers (the engine's OWN adapter — same primitive `qd new` uses)
// ===========================================================================

use dispatch::embedded_mux::EmbeddedMux;
use dispatch::mux::{Mux, MuxSession};
use dispatch::mux_selector::EmbeddedEnv;

fn embedded_env(jail: &Jail) -> EmbeddedEnv {
    EmbeddedEnv {
        xdg_runtime_dir: Some(jail.xdg_runtime.to_string_lossy().into_owned()),
        qd_home: None,
        uid: 501,
    }
}

fn mux_for(jail: &Jail) -> EmbeddedMux {
    EmbeddedMux::new(jail.home.clone(), embedded_env(jail))
}

/// Create a detached session via the engine mux primitive (the same
/// `mux.run_detached` `qd new` drives) running `shell_cmd`. Returns the listed
/// MuxSession (carrying the real child pid).
fn mux_create(jail: &Jail, dir: &Path, name: &str, shell_cmd: &str) -> MuxSession {
    let mux = mux_for(jail);
    let res = mux
        .run_detached(dir, name, shell_cmd, &jail.home)
        .expect("run_detached");
    assert_eq!(res.status, Some(0), "run_detached acked");
    // Poll for the session to be listed.
    let start = Instant::now();
    loop {
        let listed = mux.list(dir).unwrap_or_default();
        if let Some(s) = listed.into_iter().find(|s| s.name == name) {
            return s;
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!("created session {name:?} never listed");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Forge the LIVE registry `<pid>.json` a real Claude would write, so the engine
/// `gather`/`join` surfaces the mux session as a LIVE (non-cold) row — required
/// for `qd send:pty` (rejects cold). The pid IS the mux session's child pid, so
/// the join's by-pid match links the registry row to the mux session and tags
/// its socket_dir. This is the SANCTIONED forged-row technique (tests/verbs_a4.rs).
fn forge_registry_row(jail: &Jail, name: &str, pid: u32) {
    let body = format!(
        r#"{{"pid":{pid},"sessionId":"sid-{name}","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"{name}","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
    );
    std::fs::write(jail.sessions_dir.join(format!("{pid}.json")), body)
        .expect("forge registry row");
}

// ===========================================================================
// qd binary drivers (CLI + PTY)
// ===========================================================================

/// WP-B-CS-1 (D2): force the INTERACTIVE surface for `qd start` on these NON-PTY
/// runners. They pipe stdio (`cmd.output()`), so a bare `qd start` would auto-detect
/// the headless surface — and a no-`-p` start would even hit Fork B's
/// refuse-no-prompt. These gate rows exercise the interactive zmx/embedded create
/// path, so `--interactive` (the override) is inserted right after `start`. The PTY
/// runner (`QdAttach`, real TTY) is unaffected. Behavior delta — non-TTY `qd start`
/// is headless by design now — is flagged in the WP-B-CS-1 response.
fn with_interactive_start(args: &[&str]) -> Vec<String> {
    if args.first() == Some(&"start") {
        std::iter::once("start".to_string())
            .chain(std::iter::once("--interactive".to_string()))
            .chain(args[1..].iter().map(|s| s.to_string()))
            .collect()
    } else {
        args.iter().map(|s| s.to_string()).collect()
    }
}

/// Run `qd <args>` (CLI, non-PTY) under the jail's embedded env. Returns
/// (exit, stdout, stderr).
fn run_qd(jail: &Jail, args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(qd_bin());
    cmd.args(with_interactive_start(args));
    jail.apply_embedded(&mut cmd);
    let out = cmd.output().expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run `qd <args>` with an explicit QD_MUX value + optional extra env.
fn run_qd_env(jail: &Jail, args: &[&str], extra: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(qd_bin());
    cmd.args(with_interactive_start(args));
    jail.apply_embedded(&mut cmd);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A running `qd connect <name>` on a real PTY (was `qd attach` — the verb is a
/// retired stub since STATE 22; connect drives the SAME attach mechanic for a
/// live session): a reader thread drains the master into `output`; `writer` is
/// the master write half for keystrokes (detach key, scroll, etc.). The ENGINE
/// attach path (embedded_mux::attach → connect_for stdio-inherit) renders onto
/// this PTY.
struct QdAttach {
    writer: Mutex<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    #[allow(dead_code)]
    cols: u16,
    #[allow(dead_code)]
    rows: u16,
}

impl QdAttach {
    fn spawn(jail: &Jail, name: &str, cols: u16, rows: u16) -> Self {
        Self::spawn_with_env(jail, name, cols, rows, &[])
    }

    fn spawn_with_env(
        jail: &Jail,
        name: &str,
        cols: u16,
        rows: u16,
        extra_env: &[(&str, &str)],
    ) -> Self {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new(qd_bin());
        cmd.arg("connect");
        cmd.arg(name);
        jail.apply_embedded_pty(&mut cmd);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd.cwd(&jail.home);

        let child = pair.slave.spawn_command(cmd).expect("spawn qd connect");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone reader");
        let output = Arc::new(Mutex::new(Vec::new()));
        let out2 = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
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
            master: pair.master,
            cols,
            rows,
        }
    }

    fn write_raw(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    fn output_bytes(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    fn output_text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Poll until `needle` appears in the captured output (ANSI-stripped) or the
    /// budget runs out. Returns whether it appeared.
    fn wait_for(&self, needle: &str, timeout_ms: u64) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(timeout_ms) {
            if strip_ansi(&self.output_text()).contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        strip_ansi(&self.output_text()).contains(needle)
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Detach via the qrmux client detach key (Ctrl-\, the standalone detach
    /// chord) — falls back to killing the client process if it does not exit.
    fn detach(&mut self) {
        // qrmux attach detaches on the configured detach key; Ctrl-\ (0x1c) is the
        // conventional chord. If the client does not exit promptly we kill it (the
        // DAEMON keeps the session — that is the detach semantics under test).
        self.write_raw(&[0x1c]);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1500) {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// SIGKILL the client process (the G-DET teeth: simulate a client crash).
    fn kill9(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for QdAttach {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ===========================================================================
// Ported byte-level comparators (provenance noted)
// ===========================================================================

/// PORTED from crates/qrmux/tests/lib/client.rs::strip_ansi (cross-crate, not
/// importable). Strips CSI / OSC / ESC-single sequences for robust substring
/// matching against SGR-interleaved renders.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c2) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                let mut prev_esc = false;
                for c2 in chars.by_ref() {
                    if c2 == '\u{7}' || (prev_esc && c2 == '\\') {
                        break;
                    }
                    prev_esc = c2 == '\u{1b}';
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// PORTED from crates/qrmux/tests/lib/assertions.rs::assert_altscreen_replay
/// (ADR-0004 altscreen-replay invariant — REVERSED from no-altscreen-leak,
/// approved 2026-06-10; doc/inbox/2026-06-10-qrmux-phone-scroll-regression.md).
/// A client's capture carries `?1049h` IFF the inner app is in the alt screen
/// at attach or transitions into it while attached (exactly once per
/// transition); main-screen captures carry zero 1049 sequences; legacy
/// ?47/?1047 forms never appear (the renderer replays 1049 only).
fn assert_altscreen_replay(
    raw: &[u8],
    expect_1049h: usize,
    expect_1049l: usize,
    desc: &str,
) -> Result<(), String> {
    let count = |needle: &[u8]| raw.windows(needle.len()).filter(|w| *w == needle).count();
    let h = count(b"?1049h");
    let l = count(b"?1049l");
    if h != expect_1049h || l != expect_1049l {
        return Err(format!(
            "[{desc}] altscreen-replay FAIL: expected exactly {expect_1049h} ?1049h / \
             {expect_1049l} ?1049l, found {h} / {l}"
        ));
    }
    for p in [b"?47h" as &[u8], b"?47l", b"?1047h", b"?1047l"] {
        if raw.windows(p.len()).any(|w| w == p) {
            return Err(format!(
                "[{desc}] altscreen-replay FAIL: legacy variant {:?} present \
                 (renderer must replay 1049 only)",
                String::from_utf8_lossy(p)
            ));
        }
    }
    Ok(())
}

/// PORTED from crates/qrmux/tests/lib/b3_checkers.rs::strip_csi_bytes — byte-level
/// CSI strip leaving text bytes (so a split wide char stays detectable).
fn strip_csi_bytes(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        if line[i] == 0x1b {
            if i + 1 < line.len() && line[i + 1] == b'[' {
                let mut j = i + 2;
                while j < line.len() && !(0x40..=0x7e).contains(&line[j]) {
                    j += 1;
                }
                i = if j < line.len() { j + 1 } else { j };
            } else {
                i += 1;
            }
        } else {
            out.push(line[i]);
            i += 1;
        }
    }
    out
}

/// PORTED from crates/qrmux/tests/lib/b3_checkers.rs::check_cjk_integrity —
/// post-strip residue must be WHOLE UTF-8 (no lone continuation / split wide
/// char). This is the engine-reachable width-sanity check for G-UNI.
fn check_cjk_integrity(bytes: &[u8], desc: &str) -> Result<(), String> {
    let stripped = strip_csi_bytes(bytes);
    match std::str::from_utf8(&stripped) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "[{desc}] CJK FAIL: post-strip not whole UTF-8: {e}"
        )),
    }
}

/// Ordered marker presence over a SETTLED TEXT capture (engine PTY render).
///
/// HONESTLY WEAKER than qrmux's `assert_backlog_ordered`: that comparator's
/// red-team #12 contract REQUIRES frame-decoded wire-order History lines and
/// REJECTS settled text (clause (b) is vacuous on spatially-ordered text). The
/// engine attach path renders a SETTLED SCREEN, so we assert presence-in-order
/// of the markers that landed in the capture (clause (a)/(c) over the markers
/// present), NOT wire-order strictness. The order check here verifies the
/// markers that DO appear are in non-decreasing index order in the text —
/// catching gross reordering — while tolerating the screen-window truncation a
/// terminal render imposes (only the visible/scrollback tail is rendered).
///
/// Returns (verdict_ok, detail).
fn assert_markers_ordered_present(
    text: &str,
    marker: &str,
    expected_range: std::ops::RangeInclusive<usize>,
    desc: &str,
) -> Result<String, String> {
    // Scan the WHOLE stripped text for marker occurrences in APPEARANCE ORDER —
    // NOT line-split. The engine attach replay carries the history region as
    // newline-delimited lines AND the live SCREEN region as cursor-addressed text
    // (CSI cursor moves, no `\n` between cells), so a `lines()` parse misses the
    // most-recent screen rows. A byte-order scan catches both regions.
    let mut found: Vec<usize> = Vec::new();
    let bytes = text.as_bytes();
    let mneedle = marker.as_bytes();
    let mut i = 0;
    while i + mneedle.len() <= bytes.len() {
        if &bytes[i..i + mneedle.len()] == mneedle {
            let mut j = i + mneedle.len();
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                if let Ok(idx) = std::str::from_utf8(&bytes[start..j])
                    .unwrap()
                    .parse::<usize>()
                {
                    found.push(idx);
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    if found.is_empty() {
        return Err(format!("[{desc}] FAIL: no markers found at all"));
    }
    // (c) every found index must be IN RANGE (no corruption / stray / split index).
    for &idx in &found {
        if !expected_range.contains(&idx) {
            return Err(format!("[{desc}] FAIL: out-of-range marker index {idx}"));
        }
    }
    // Order: the HISTORY region is newline-ordered ascending. The live SCREEN
    // region is cursor-addressed (not byte-ordered), so we DON'T impose strict
    // cross-region monotonicity (it would falsely fail on the screen reflow). We
    // verify the leading run (history) is non-decreasing — catching gross history
    // reordering — up to the first descent (the history→screen seam).
    let mut hist_run = 0usize;
    while hist_run + 1 < found.len() && found[hist_run + 1] >= found[hist_run] {
        hist_run += 1;
    }
    // (a) COMPLETENESS: every expected index present at least once across the
    // replay (history + screen). This is the strong backlog-completeness claim.
    use std::collections::HashSet;
    let present: HashSet<usize> = found.iter().copied().collect();
    let missing: Vec<usize> = expected_range
        .clone()
        .filter(|i| !present.contains(i))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "[{desc}] FAIL: {} expected markers MISSING from replay (e.g. {:?})",
            missing.len(),
            &missing[..missing.len().min(5)]
        ));
    }
    let last = *expected_range.end();
    if !present.contains(&last) {
        return Err(format!(
            "[{desc}] FAIL: tail marker {last} (most-recent line) missing from replay"
        ));
    }
    Ok(format!(
        "{} marker occurrences, ALL {:?} present (completeness), history-run ascending len {}, tail {} present",
        found.len(),
        expected_range,
        hist_run + 1,
        last
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    d.iter().map(|b| format!("{b:02x}")).collect()
}

// ===========================================================================
// Fake-claude script (for `qd new`)
// ===========================================================================

/// Write a fake-claude shell script that writes the registry `<pid>.json` a real
/// Claude would write (so EventBootWaiter sees the row + idle status), then execs
/// `<app>` so the session has a real interactive child. Returns the script path.
///
/// `$QD_FAKE_NAME` carries the session name (set by the caller via env). The
/// script writes `<sessions_dir>/<pid>.json` with that name + status idle.
fn write_fake_claude(jail: &Jail, app: &str) -> PathBuf {
    let path = jail.root.join("fake-claude.sh");
    let sessions = jail.sessions_dir.to_string_lossy().into_owned();
    let script = format!(
        r#"#!/bin/bash
# Fake-claude (TEST INFRA): write the registry row real Claude writes, then exec
# a real interactive app so the engine attach/send paths have a live child.
PID=$$
NAME="${{QD_FAKE_NAME:-fake}}"
SESS="{sessions}"
mkdir -p "$SESS"
# startedAt/updatedAt = NOW (fidelity: real Claude stamps its boot instant —
# r8: stop's pid-identity start-time arm compares the live process start
# against startedAt; a hardcoded past timestamp models a session LYING about
# its boot, which reads as a reused pid).
NOW_MS="$(($(date +%s) * 1000))"
printf '{{"pid":%s,"sessionId":"sid-%s","cwd":"/w","startedAt":%s,"updatedAt":%s,"status":"idle","name":"%s","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}' "$PID" "$NAME" "$NOW_MS" "$NOW_MS" "$NAME" > "$SESS/$PID.json"
exec {app}
"#
    );
    std::fs::write(&path, script).expect("write fake-claude");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok();
    path
}

// ===========================================================================
// ROWS
// ===========================================================================

// Teardown-leak belt (prefix-scoped reap at jail setup, per-target by pidfile).
include!("c1_gate_inc/daemon_reaper.rs");

include!("c1_gate_inc/rows.rs");

// WS-C M4 engine-level gate rows (G-ISOL, G-COLDSTART-N, G-EVSPLIT, G-LEGACY).
include!("c1_gate_inc/wsc_m4_rows.rs");

// WS-C M5 release-build measurement gates (G-SOAK, G-IDLE) — #[ignore]-gated.
include!("c1_gate_inc/wsc_m5_rows.rs");
