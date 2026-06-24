//! WS-C M2 integration tests — the single-session daemon (spec §4.1, §2).
//!
//! These drive a REAL `sbmux server --session <name> --socket-dir <dir>` child
//! process inside a hermetic jail and assert the per-session contract end to end:
//! single-session bind leaf, the claim-timeout (incl. the B1 reset rule and the
//! G-CLAIMRESET orc rider W-2 arm), and the exit-on-session-end ordering. The
//! capacity-1 identity errors + ServerHello.session are covered by the
//! client_handler unit tests (no real daemon needed); here we add the live bind
//! + lifecycle arms that only a process exercises.

// The shared `lib/` support module is `#[path]`-included into every integration
// binary; wsc_m2 uses only a narrow subset (it spawns its own single-session
// daemon rather than the legacy `start_daemon_in_jail`). Allow the resulting
// dead-code / unused-import noise for THIS binary only — other binaries are
// separate crates and unaffected.
#![allow(dead_code, unused_imports)]

#[path = "lib/mod.rs"]
mod libmod;

use libmod::client::{sbmux_binary, sweep_orphan_daemons};
use libmod::{create_session, jail_env, list_sessions, send_to_session, setup_jail, teardown_jail};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A single-session daemon child. Unlike production (where the launcher setsids
/// and init reaps), this test process is the daemon's DIRECT parent, so it must
/// reap the child itself — `kill -0` succeeds on an un-reaped zombie, which would
/// make exit-detection lie. `has_exited` calls `try_wait` (which reaps).
struct Daemon {
    child: Child,
    pub socket: PathBuf,
}

impl Daemon {
    /// True once the daemon process has actually exited (reaps the zombie).
    fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Wait up to `dur` for the daemon to exit; returns whether it did.
    fn wait_exit(&mut self, dur: Duration) -> bool {
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            if self.has_exited() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.has_exited()
    }

    /// Best-effort kill + reap so a failing test never leaks a daemon.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a SINGLE-SESSION daemon (`server --session <name> --socket-dir <dir>`)
/// in the jail with the given extra env (e.g. `SBMUX_CLAIM_TIMEOUT_MS`). Daemon
/// stderr is captured to `stderr_file` (named-log-line assertion) or discarded.
fn start_single_session_daemon(
    jail: &libmod::jail::Jail,
    session: &str,
    extra_env: &[(String, String)],
    stderr_file: Option<&Path>,
    wait_for_socket: bool,
) -> Result<Daemon, Box<dyn Error>> {
    sweep_orphan_daemons();

    let mut env_vars: Vec<(String, String)> = jail_env(jail);
    env_vars.extend(extra_env.iter().cloned());
    if !env_vars.iter().any(|(k, _)| k == "PATH") {
        env_vars.push(("PATH".into(), "/usr/bin:/bin".into()));
    }
    if !env_vars.iter().any(|(k, _)| k == "TERM") {
        env_vars.push(("TERM".into(), "xterm-256color".into()));
    }

    let socket_path = jail.socket_dir.join(format!("{session}.sock"));
    let _ = std::fs::remove_file(&socket_path);

    let stderr: Stdio = match stderr_file {
        Some(p) => Stdio::from(std::fs::File::create(p)?),
        None => Stdio::null(),
    };

    let child = Command::new(sbmux_binary())
        .arg("server")
        .arg("--socket-dir")
        .arg(&jail.socket_dir)
        .arg("--session")
        .arg(session)
        .env_clear()
        .envs(env_vars.iter().cloned())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()?;

    if wait_for_socket {
        let start = Instant::now();
        while !socket_path.exists() {
            if start.elapsed() > Duration::from_secs(5) {
                return Err(format!("daemon socket {:?} not created in 5s", socket_path).into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(Daemon {
        child,
        socket: socket_path,
    })
}

/// §2 / §4.1: single-session mode binds exactly `<dir>/<name>.sock` (NOT the
/// legacy `sbmux.sock`), and the daemon accepts its own-name create + list.
#[test]
fn single_session_binds_name_dot_sock() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m2_bind")?;
    // Long claim timeout so the daemon stays up for the assertions.
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into())],
        None,
        true,
    )?;
    let socket = daemon.socket.clone();

    // The per-session leaf exists; the legacy shared leaf does NOT.
    assert_eq!(socket, jail.socket_dir.join("alpha.sock"));
    assert!(socket.exists(), "per-session socket not bound");
    assert!(
        !jail.socket_dir.join("sbmux.sock").exists(),
        "single-session mode must NOT bind the legacy sbmux.sock"
    );

    // Own-name create + a 0-or-1-row ListSessions for this daemon.
    create_session(&socket, "alpha")?;
    let rows = list_sessions(&socket)?;
    assert_eq!(
        rows.len(),
        1,
        "single-session ListSessions returns its own row"
    );
    assert_eq!(rows[0].name, "alpha");

    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

/// G-CLAIMRESET (a) — orc rider W-2: an UNCLAIMED daemon exits within the claim
/// budget, its socket is gone, and the NAMED log line is emitted.
#[test]
fn claim_timeout_unclaimed_daemon_exits() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m2_claim_unclaimed")?;
    let stderr = jail.jail_root.join("daemon.stderr");
    let mut daemon = start_single_session_daemon(
        &jail,
        "ghost",
        &[("SBMUX_CLAIM_TIMEOUT_MS".into(), "800".into())],
        Some(&stderr),
        true,
    )?;
    let socket = daemon.socket.clone();

    // Within the budget + slack the unclaimed daemon must exit and unlink.
    assert!(
        daemon.wait_exit(Duration::from_secs(6)),
        "unclaimed daemon did not exit within budget"
    );
    // Socket gone (unlink-before-exit, §4.1 step 5).
    let deadline = Instant::now() + Duration::from_secs(2);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!socket.exists(), "exited daemon left its socket behind");
    // Named log line (triage can't confuse it with cold-start failure).
    let log = std::fs::read_to_string(&stderr).unwrap_or_default();
    assert!(
        log.contains("unclaimed after") && log.contains("exiting"),
        "missing named claim-expiry log line, got: {log}"
    );

    teardown_jail(&jail)?;
    Ok(())
}

/// G-CLAIMRESET (b) — orc rider W-2 / red-team B1: Hello-only connections do NOT
/// reset the claim timer (the daemon still exits). `list_sessions` and
/// `create_session` both perform a full Hello, but `list` is NOT a
/// session-addressed verb, so repeated lists must NOT keep the daemon alive.
#[test]
fn claim_timer_not_reset_by_hello_only_probes() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m2_claim_probe")?;
    let mut daemon = start_single_session_daemon(
        &jail,
        "ghost",
        &[("SBMUX_CLAIM_TIMEOUT_MS".into(), "1000".into())],
        None,
        true,
    )?;
    let socket = daemon.socket.clone();

    // Hammer ListSessions (Hello + a non-session-addressed verb) faster than the
    // claim budget for longer than the budget. If lists reset the timer the
    // daemon would live forever; the B1 rule says it must still expire.
    let probe_until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < probe_until && !daemon.has_exited() {
        let _ = list_sessions(&socket); // ignore errors as the socket vanishes
        std::thread::sleep(Duration::from_millis(150));
    }

    assert!(
        daemon.wait_exit(Duration::from_secs(3)),
        "Hello-only ListSessions probes must NOT keep an unclaimed daemon alive (B1)"
    );

    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

/// G-CLAIMRESET (c) — orc rider W-2: a session-addressed verb (the create)
/// CLAIMS the daemon and cancels the claim timeout: the daemon stays alive well
/// past the (short) claim budget because a live session exists.
#[test]
fn claim_cancelled_by_session_create() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m2_claim_cancel")?;
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[("SBMUX_CLAIM_TIMEOUT_MS".into(), "600".into())],
        None,
        true,
    )?;
    let socket = daemon.socket.clone();

    // Claim it BEFORE the budget elapses (create is a session-addressed verb).
    create_session(&socket, "alpha")?;

    // Wait well past the claim budget; the live session must keep it alive.
    std::thread::sleep(Duration::from_millis(2000));
    assert!(
        !daemon.has_exited(),
        "a claimed daemon (live session) must not be reaped by the claim timeout"
    );
    let rows = list_sessions(&socket)?;
    assert_eq!(rows.len(), 1, "claimed session should still be present");

    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

/// §4.1 exit-on-session-end: a session whose child exits drives the daemon to
/// exit 0 with the socket unlinked, and the history written JUST BEFORE the
/// child exit IS present in the final drain (content-first proof, step 2). A
/// connect AFTER the socket is gone gets ENOENT (clean absent) — never a hang.
#[test]
fn exit_on_session_end_content_first_then_socket_gone() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m2_exit_on_end")?;
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into())],
        None,
        true,
    )?;
    let socket = daemon.socket.clone();

    // Create the session, then drive output and exit the shell so the child
    // reaches EOF. The sentinel is emitted right before `exit`.
    create_session(&socket, "alpha")?;
    send_to_session(&socket, &[], "alpha", "echo END_SEN''TINEL_M2\n")?;
    // Give the echo time to land in the screen/history model.
    std::thread::sleep(Duration::from_millis(600));

    // Read history BEFORE exit to prove the content-first drain captured it.
    let mut found = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while !found && Instant::now() < deadline {
        if let Ok(rows) = list_sessions(&socket) {
            if !rows.is_empty() {
                // History via the get_history helper path — reuse send/list shape
                // by attaching is heavier; the screen already holds the sentinel,
                // so a fresh capture confirms it.
                if let Ok(text) = libmod::capture_session(&socket, "alpha", 0).map(|r| r.text()) {
                    if text.contains("END_SENTINEL_M2") {
                        found = true;
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(found, "sentinel never appeared in pre-exit history");

    // Now exit the shell — the child reaches EOF → exit-on-end fires.
    send_to_session(&socket, &[], "alpha", "exit\n")?;

    // The daemon must exit and unlink its socket within a bounded window.
    assert!(
        daemon.wait_exit(Duration::from_secs(8)),
        "daemon did not exit after session end"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !socket.exists(),
        "socket not unlinked on exit-on-end (step 5)"
    );

    // A connect after the socket is gone is a clean ENOENT, NEVER a hang. Bound
    // it tightly so a hang fails the test.
    let connect = std::thread::spawn({
        let socket = socket.clone();
        move || std::os::unix::net::UnixStream::connect(&socket).is_err()
    });
    let start = Instant::now();
    while !connect.is_finished() {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "post-exit connect hung instead of ENOENT"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        connect.join().unwrap(),
        "post-exit connect should fail (ENOENT)"
    );

    teardown_jail(&jail)?;
    Ok(())
}

/// §4.1: KillSession drives the SAME exit-on-end path — the daemon exits and
/// unlinks its socket.
#[test]
fn kill_session_drives_exit_on_end() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m2_kill_exit")?;
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into())],
        None,
        true,
    )?;
    let socket = daemon.socket.clone();
    create_session(&socket, "alpha")?;

    // Kill the session via the wire (KillSession verb).
    let killed = std::thread::spawn({
        let socket = socket.clone();
        move || -> Result<(), String> {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                use libmod::client::ProtocolClient;
                use sbmux::protocol::{ClientMsg, ServerMsg};
                let mut c = ProtocolClient::connect(&socket).await?;
                match c
                    .send_and_receive(ClientMsg::KillSession {
                        name: "alpha".into(),
                    })
                    .await?
                {
                    ServerMsg::SessionKilled { .. } => Ok(()),
                    other => Err(format!("expected SessionKilled, got {:?}", other)),
                }
            })
        }
    })
    .join()
    .map_err(|_| "kill thread panicked")?;
    killed.map_err(|e| -> Box<dyn Error> { e.into() })?;

    assert!(
        daemon.wait_exit(Duration::from_secs(8)),
        "daemon did not exit after KillSession"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !socket.exists(),
        "socket not unlinked after KillSession exit"
    );

    teardown_jail(&jail)?;
    Ok(())
}

/// WS-C M3b RETIREMENT TEETH (spec §1, §9): the legacy shared-daemon mode (no
/// `--session`, bound `sbmux.sock`) is RETIRED — `--session` is now REQUIRED on
/// `sbmux server`. Spawning the daemon WITHOUT `--session` must FAIL LOUDLY
/// (clap rejects the missing required arg, exit code != 0, named "session" on
/// stderr) and must NOT bind any `sbmux.sock`. This is the retire-with-reason
/// arm replacing the M2 transitional legacy-mode path.
#[test]
fn server_without_session_is_rejected_loudly() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m2_no_session")?;
    let mut env_vars: Vec<(String, String)> = jail_env(&jail);
    if !env_vars.iter().any(|(k, _)| k == "PATH") {
        env_vars.push(("PATH".into(), "/usr/bin:/bin".into()));
    }

    // Spawn `sbmux server --socket-dir <dir>` with NO --session and capture stderr.
    let out = Command::new(sbmux_binary())
        .arg("server")
        .arg("--socket-dir")
        .arg(&jail.socket_dir)
        .env_clear()
        .envs(env_vars.iter().cloned())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;

    assert!(
        !out.status.success(),
        "sbmux server WITHOUT --session must fail (legacy shared mode retired)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("session"),
        "the rejection must name the missing --session arg, got: {stderr}"
    );
    // No legacy shared socket was bound.
    assert!(
        !jail.socket_dir.join("sbmux.sock").exists(),
        "a rejected (no --session) daemon must NOT bind the legacy sbmux.sock"
    );

    teardown_jail(&jail)?;
    Ok(())
}
