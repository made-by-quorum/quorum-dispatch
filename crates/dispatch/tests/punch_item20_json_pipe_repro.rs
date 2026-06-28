//! Engine-hardening item 20 — `qd ls --json` must NOT panic-and-truncate when
//! its downstream pipe closes early ("stream-complete-or-error, never a silent
//! partial document").
//!
//! THE BUG THIS PINS (red against pre-fix code): `qd ls --json` emits its
//! payload with `println!`, which PANICS when the underlying write hits
//! `BrokenPipe` (exit 101 + a Rust backtrace note on stderr) while a PARTIAL
//! JSON document is already on stdout. The supervisor's watcher parses
//! `qd ls --json`; under load a consumer that closes the pipe early (a `| head`,
//! or a monitor that times out and stops reading) gets a truncated document and
//! — if it does not check the exit code — reads it as truth. Corrupt is worse
//! than wrong.
//!
//! THE FIX THIS GUARDS: the verb writes the (already whole-in-memory) payload
//! through a broken-pipe-clean writer that exits 141 (128 + SIGPIPE) — the
//! conventional "downstream went away" code — with NO panic and NO backtrace.
//! A complete document still exits 0; a truncated one exits 141, so a consumer
//! can tell them apart. The fix is SCOPED to the verb's stdout (NOT a
//! process-wide SIGPIPE reset — the embedded daemon/relay server in this same
//! binary rely on EPIPE-as-Result).
//!
//! Hermetic: HOME + ZMX_DIR point at a per-test temp dir; the registry is seeded
//! with forged rows only. The real `~/.claude/sessions` is never touched, and
//! `ls` is read-only regardless.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The built `qd` binary next to the test exe (`<target>/debug/qd`).
fn qd_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let mut dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target/debug")
        .to_path_buf();
    dir.push("qd");
    assert!(
        dir.exists(),
        "qd binary not found at {dir:?} — build it first: \
         scripts/build-lock.sh cargo build -p qd --bin qd"
    );
    dir
}

/// Seed a hermetic HOME whose registry holds `n` forged session rows. `n` is
/// chosen so `ls --all --json` exceeds the kernel pipe buffer (~64KB), so an
/// early-closing reader provably leaves qd mid-write (the EPIPE the bug
/// panicked on).
fn seed_home(n: usize) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "item20-jsonpipe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let sessions = root.join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // A separate empty ZMX_DIR so discovery does not scan host runtime sockets.
    std::fs::create_dir_all(root.join("zmx")).unwrap();
    for i in 0..n {
        let pid = 100_000 + i;
        let row = format!(
            r#"{{"pid":{pid},"sessionId":"00000000-0000-0000-0000-{i:012}","cwd":"/home/u/work/some/longish/path/number/{i}","startedAt":1700000000000,"updatedAt":1700000000000,"status":"idle","name":"forged-session-row-{i:04}","version":"2.1.175"}}"#
        );
        std::fs::write(sessions.join(format!("{pid}.json")), row).unwrap();
    }
    root
}

/// Spawn `qd ls --all --json`, read a few bytes, close the read end, and report
/// (exit_code, stderr). Closing the pipe while qd still has bytes to write is
/// exactly the broken-pipe condition.
fn run_with_early_close(home: &PathBuf) -> (Option<i32>, String) {
    let mut child = Command::new(qd_binary())
        .args(["ls", "--all", "--json"])
        .env("HOME", home)
        .env("QD_HOME", home)
        .env("ZMX_DIR", home.join("zmx"))
        .env("QD_DEBUG", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn qd ls --all --json");

    // Read a little, then DROP the read end -> qd's next write hits EPIPE.
    let mut stdout = child.stdout.take().unwrap();
    let mut sip = [0u8; 64];
    let n = stdout.read(&mut sip).unwrap_or(0);
    assert!(
        n > 0,
        "expected qd to write some JSON before we close the pipe"
    );
    drop(stdout);

    let status = child.wait().expect("wait qd");
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    (status.code(), err)
}

#[test]
fn ls_json_broken_pipe_exits_clean_not_panic() {
    // 400 rows => ~200KB of pretty JSON, far past the ~64KB pipe buffer.
    let home = seed_home(400);
    let (code, err) = run_with_early_close(&home);
    let _ = std::fs::remove_dir_all(&home);

    // The defect signature: a Rust panic ("failed printing to stdout: Broken
    // pipe") with the generic panic exit code 101. The fix must eliminate BOTH.
    assert!(
        !err.contains("panicked"),
        "qd ls --json panicked on a broken pipe instead of exiting cleanly \
         (item 20 — a machine surface must not crash-dump on EPIPE).\nstderr:\n{err}"
    );
    assert_eq!(
        code,
        Some(141),
        "broken-pipe stdout should exit 141 (128+SIGPIPE) — the conventional \
         downstream-closed code a consumer can detect — not {code:?} \
         (101 = the pre-fix panic).\nstderr:\n{err}"
    );
}
