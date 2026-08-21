//! End-to-end validation of the `pi/extension` lane against the real `qd`
//! binary, a real mux pane, a real registry — and a real unix socket.
//!
//! # What this proves that `pi_interactive_lane.rs` cannot
//!
//! Its sibling pins that the launcher's chosen `--session-id` reaches the
//! process. This lane adds a SECOND channel, and the two failure modes it
//! introduces are both silent:
//!
//!   1. **The flag is dropped.** The pane comes up, the row says
//!      `hosting: "extension"`, every liveness assertion passes — and the pi
//!      inside it is an ordinary unchannelled TUI. Every later verb then
//!      addresses a socket nobody is serving. This is not hypothetical: it is
//!      exactly the state this lane was in before `control_socket` was plumbed
//!      through `NewParams`, and nothing but the argv shows it.
//!   2. **The row forgets.** Without `hosting`/`endpoint` on the row the session
//!      comes back as `pi/mux-pane` on the next call and silently reverts to
//!      typing into the PTY.
//!
//! So the assertions here are about the ARGV and the ROW, and then about the
//! socket actually answering.
//!
//! # What stands in for pi
//!
//! `QD_PI_BIN` points the launch argv at any binary. The stand-in here is a
//! Python script that does three things a shell script cannot: it records its
//! argv, it BINDS the socket it was handed and speaks the `quorum-lane` wire on
//! it, and it stays alive. That makes this suite a real exercise of
//! `receive_path`, `health` and `deliver` — over a real `connect(2)`, with real
//! frames — without pi installed, authenticated, or reachable on a network.
//!
//! # What this deliberately does NOT prove
//!
//! That real pi still loads the extension, still exposes `sendUserMessage`, or
//! still fires `agent_settled`. Those are live-evidence questions against a
//! pinned pi, recorded in `doc/tbd/provider-architecture/15-pi-extension-lane.md`
//! §1 where each was measured. This pins OUR half of the contract: that the
//! launch carries the channel, the row remembers it, and the lane speaks the
//! protocol it claims to.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use common::p0bins::{establish_jail, qd_bin, JailScaffold};

/// The stand-in: answers `--help`, records argv, serves the control channel.
///
/// Two files. The ENTRY POINT is a shell script named `pi`, and both facts
/// matter: qrmux classifies the harness by the launched binary's basename (a
/// stand-in called `fake-pi` silently rides claude's composer facts instead of
/// `PiFacts`), and answering `--help` in shell keeps an interpreter's startup
/// out of the create path's 10s capability probe. The server half is Python,
/// because a shell script cannot bind a unix socket and speak JSON frames on it.
///
/// Answering `--help` with a line naming `--session-id` is part of the contract,
/// not scaffolding: the create path probes for that flag before it claims a
/// name, and a stand-in that could not answer would be refused before any of
/// this ran.
fn install_fake_pi(dir: &Path) -> std::path::PathBuf {
    let bin_dir = dir.join("pi-bin");
    std::fs::create_dir_all(&bin_dir).expect("stand-in dir");

    // The server half, in Python: a shell script cannot bind a unix socket and
    // speak newline-delimited JSON on it.
    let server = bin_dir.join("serve.py");
    std::fs::write(
        &server,
        r#"import json, os, socket, sys, threading, time

argv = sys.argv[1:]
here = os.path.dirname(os.path.abspath(__file__))

# Serve the channel only when handed one — the same gate the real extension
# applies, so a launch that drops the flag produces a socket-less process here
# too rather than one that works by accident.
sock = argv[argv.index("--quorum-sock") + 1] if "--quorum-sock" in argv else None
state = {"busy": False, "turns": 0}

def serve(path):
    try:
        os.makedirs(os.path.dirname(path), exist_ok=True)
    except OSError:
        pass
    try:
        os.unlink(path)
    except OSError:
        pass
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    os.chmod(path, 0o600)
    srv.listen(8)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()

def handle(conn):
    buf = b""
    while True:
        try:
            chunk = conn.recv(65536)
        except OSError:
            return
        if not chunk:
            return
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            line = line.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except ValueError:
                conn.sendall(json.dumps(
                    {"err": {"code": "bad-json", "detail": "unparseable"}}).encode() + b"\n")
                continue
            rid, m = req.get("id"), req.get("m")
            if m == "hello":
                ok = {"v": 1, "session": "stand-in", "cwd": os.getcwd(),
                      "mode": "tui", "pi": "stand-in"}
            elif m == "health":
                ok = {"status": "busy" if state["busy"] else "idle",
                      "turns": state["turns"], "pending": False}
            elif m == "deliver":
                text = req.get("text")
                if not isinstance(text, str) or not text:
                    if rid is not None:
                        conn.sendall(json.dumps({"id": rid, "err": {
                            "code": "bad-request",
                            "detail": "deliver needs a non-empty string `text`"}}).encode() + b"\n")
                    continue
                with open(os.path.join(here, "..", "delivered.log"), "a") as f:
                    f.write(text + "\n")
                state["turns"] += 1
                ok = {"accepted": True, "queued_as": "immediate"}
            elif m == "subscribe":
                ok = {"subscribed": True,
                      "status": "busy" if state["busy"] else "idle"}
            else:
                if rid is not None:
                    conn.sendall(json.dumps({"id": rid, "err": {
                        "code": "unknown-verb", "detail": "no such verb"}}).encode() + b"\n")
                continue
            if rid is not None:
                conn.sendall(json.dumps({"id": rid, "ok": ok}).encode() + b"\n")

if sock:
    threading.Thread(target=serve, args=(sock,), daemon=True).start()

# Stay alive regardless. A stand-in that exited would leave every liveness
# assertion racing a dying pane.
time.sleep(600)
"#,
    )
    .expect("write stand-in server");

    // The entry point is SHELL, and `--help` is answered before Python is ever
    // started. The create path probes the binary for `--session-id` support
    // under a 10s budget, and paying an interpreter's startup inside that budget
    // made the probe time out under load — a flake that looks exactly like "pi
    // does not support the flag".
    let bin = bin_dir.join("pi");
    std::fs::write(
        &bin,
        r##"#!/bin/sh
case "$*" in *--help*) echo '  --session-id <id>  Use exact project session ID'; exit 0;; esac
here=$(dirname "$0")
printf 'LAUNCH %s\n' "$*" >> "$here/../argv.log"
exec python3 "$here/serve.py" "$@"
"##,
    )
    .expect("write fake pi");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    bin
}

/// Block until the stand-in has recorded `n` launches, then return those lines.
///
/// Polling is REQUIRED: `qd start` returns once the pane is registered and
/// attachable, which is strictly earlier than the process inside it getting
/// scheduled. Reading immediately races that gap, and the race is silent —
/// "not written yet" and "launched with no arguments" are the same empty read.
fn launches(dir: &Path, n: usize) -> Vec<String> {
    let path = dir.join("argv.log");
    for _ in 0..300 {
        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("LAUNCH"))
            .map(|l| l.trim().to_string())
            .collect();
        if lines.len() >= n {
            return lines;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!(
        "stand-in never recorded {n} launch(es); log: {:?}",
        std::fs::read_to_string(&path)
    );
}

fn qd(
    j: &JailScaffold,
    pi_sessions: &Path,
    cwd: &Path,
    pane_bin: &Path,
    args: &[&str],
) -> (i32, String, String) {
    let out = Command::new(qd_bin())
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("HOME", &j.home)
        .env("QD_HOME", &j.qd_home)
        .env("XDG_RUNTIME_DIR", &j.xdg)
        .env("TMPDIR", j.root.join("tmp"))
        .env("PI_CODING_AGENT_SESSION_DIR", pi_sessions)
        .env("QD_PI_BIN", pane_bin)
        .env("QD_BOOT_AWAIT_RELAY", "0")
        .env("QD_TEST_NO_BARE_PROCS", "1")
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn row(j: &JailScaffold) -> BTreeMap<String, serde_json::Value> {
    let dir = j.home.join(".claude").join("sessions");
    let mut rows: Vec<_> = std::fs::read_dir(&dir)
        .expect("sessions dir")
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .collect();
    assert_eq!(rows.len(), 1, "expected exactly one registry row in {dir:?}");
    let bytes = std::fs::read(rows.remove(0).path()).expect("read row");
    serde_json::from_slice(&bytes).expect("row is json")
}

fn field(j: &JailScaffold, k: &str) -> Option<String> {
    row(j).get(k).map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

fn pid_alive(pid: i64) -> bool {
    pid > 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// The whole lane, in the order a user would exercise it.
///
/// One test rather than several because each step depends on the previous one's
/// side effects on a single real session, and re-establishing a jail and a live
/// pane per assertion would trade a great deal of wall time for no isolation
/// that matters — the sibling suite makes the same call for the same reason.
#[test]
fn the_extension_lane_launches_channelled_records_it_and_answers_on_the_socket() {
    let j = establish_jail(Path::new("/tmp/qd-piext"), "piext");
    let pane_bin = install_fake_pi(&j.root);
    let pi_sessions = j.root.join("pi-sessions");
    std::fs::create_dir_all(&pi_sessions).expect("pi sessions dir");
    let work = j.root.join("work");
    std::fs::create_dir_all(&work).expect("work dir");

    // --- create -----------------------------------------------------------
    let (code, out, err) = qd(
        &j,
        &pi_sessions,
        &work,
        &pane_bin,
        &["start", "extlane", "--provider", "pi", "--extension"],
    );
    assert_eq!(code, 0, "qd start --extension\nstdout={out}\nstderr={err}");

    // (1) THE ARGV. The lane's one launch difference, asserted directly rather
    //     than inferred from a row we wrote ourselves.
    let launch = launches(&j.root, 1).remove(0);
    assert!(
        launch.contains("--quorum-sock"),
        "the launch must carry the control-channel flag, or the pane is an \
         ordinary pi/mux-pane wearing an extension row: {launch}"
    );
    let session_id = field(&j, "sessionId").expect("row records a session id");
    assert!(
        launch.contains(&format!("--session-id {session_id}")),
        "the launch must carry the row's id: {launch}"
    );

    // (2) THE ROW. What every later verb re-derives the lane from.
    assert_eq!(
        field(&j, "hosting").as_deref(),
        Some("extension"),
        "a row that does not say `extension` comes back as pi/mux-pane and \
         silently reverts to typing into the PTY"
    );
    let endpoint = field(&j, "endpoint").expect("row records the control channel");
    assert!(
        endpoint.starts_with("unix://"),
        "the endpoint must name the socket, self-describingly: {endpoint}"
    );
    assert_eq!(
        field(&j, "provider").as_deref(),
        Some("pi"),
        "the extension lane is still pi — a mode, not a harness"
    );

    // The socket the row names is the socket the launch was handed. A row
    // pointing somewhere else is reachable-looking and unreachable.
    let sock_path = endpoint.trim_start_matches("unix://").to_string();
    assert!(
        launch.contains(&sock_path),
        "row endpoint {endpoint} must name the socket the launch got: {launch}"
    );

    // (3) THE CHANNEL ANSWERS. Poll: `qd start` returns once the pane is
    //     attachable, which is earlier than the process inside it binding.
    let sock = Path::new(&sock_path);
    let mut bound = false;
    for _ in 0..300 {
        if sock.exists() {
            bound = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(bound, "the stand-in never bound {sock_path}");

    let pid: i64 = row(&j)
        .get("pid")
        .and_then(|v| v.as_i64())
        .expect("row records the pane pid");
    assert!(pid_alive(pid), "the pane process must be live");

    // --- deliver over the channel -----------------------------------------
    let (code, out, err) = qd(
        &j,
        &pi_sessions,
        &work,
        &pane_bin,
        &["send", "extlane", "hello over the control channel"],
    );
    assert_eq!(code, 0, "qd send\nstdout={out}\nstderr={err}");

    // The message reached the PROCESS, not just the carrier. This is the
    // assertion the PTY lane cannot make at all: there, delivery is keystrokes
    // and acceptance is inferred from a transcript appearing later.
    let delivered = std::fs::read_to_string(j.root.join("delivered.log")).unwrap_or_default();
    assert!(
        delivered.contains("hello over the control channel"),
        "the text must arrive at the session over the socket; got {delivered:?}"
    );

    // --- stop: the socket goes with the session ----------------------------
    let (code, out, err) = qd(&j, &pi_sessions, &work, &pane_bin, &["stop", "extlane"]);
    assert_eq!(code, 0, "qd stop\nstdout={out}\nstderr={err}");

    // A reap is not a clean shutdown, so nothing inside the session unlinks the
    // socket — the lane must. Left behind, it fails `connect(2)` with
    // ECONNREFUSED, which reads as "the channel broke" rather than "the session
    // is gone".
    let mut gone = false;
    for _ in 0..100 {
        if !sock.exists() {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(gone, "the control socket must not outlive the session: {sock_path}");
}

/// `--extension` is pi's alone, and the refusal says so rather than blaming the
/// provider.
///
/// The wording matters: `Lane::for_create` answers `None` for this combination,
/// and the same `None` means "unknown provider" one branch over. A user who
/// typed `--provider codex --extension` must not be told codex is unknown.
#[test]
fn the_extension_flag_is_refused_for_every_other_harness_by_name() {
    let j = establish_jail(Path::new("/tmp/qd-piext"), "piextrefuse");
    let pane_bin = install_fake_pi(&j.root);
    let pi_sessions = j.root.join("pi-sessions");
    std::fs::create_dir_all(&pi_sessions).expect("pi sessions dir");
    let work = j.root.join("work");
    std::fs::create_dir_all(&work).expect("work dir");

    for provider in ["codex", "claude-code", "acp/claude-code"] {
        let (code, _out, err) = qd(
            &j,
            &pi_sessions,
            &work,
            &pane_bin,
            &["start", "nope", "--provider", provider, "--extension"],
        );
        assert_ne!(code, 0, "--extension on {provider} must refuse");
        assert!(
            err.contains("--extension is pi's alone"),
            "the refusal must name the real reason for {provider}; got {err:?}"
        );
        assert!(
            !err.contains("unknown provider"),
            "{provider} is a provider this engine supports — saying otherwise \
             sends the user to fix the wrong thing; got {err:?}"
        );
    }
}
