//! WS-C M3a integration tests — the qrmux client-library half of the engine
//! flip (spec §3.2 identity belt, §4.2 launcher four liveness states, §4.3
//! discovery probe). These drive REAL per-session `qrmux server --session
//! <name> --socket-dir <dir>` daemons inside hermetic jails and exercise the
//! NEW client surface (`ensure_session_server_running`, `connect_session_stream`,
//! `scan_sessions`) end to end. The legacy paths stay intact and untouched.

#![allow(dead_code, unused_imports)]

#[path = "lib/mod.rs"]
mod libmod;

use libmod::client::{qrmux_binary, sweep_orphan_daemons};
use libmod::{jail_env, setup_jail, teardown_jail};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use qrmux::client::discovery::scan_sessions;
use qrmux::client::server_launcher::{ensure_session_server_running, ServerLaunchSpec};
use qrmux::client::session_client::connect_session_stream;

/// Build a [`ServerLaunchSpec`] that re-execs the qrmux TEST binary with
/// `["server"]` (the standalone daemon entry — the embedder's `sb qrmux-server`
/// spec is exercised by the engine tests in M3b). The launcher appends
/// `--socket-dir <dir> --session <name>`.
fn test_launch_spec() -> ServerLaunchSpec {
    ServerLaunchSpec {
        program: qrmux_binary(),
        args_prefix: vec!["server".to_string()],
    }
}

/// Run an async body on a fresh single-thread runtime with the jail env applied
/// to THIS process for the duration. The library reads `XDG_RUNTIME_DIR` /
/// `SB_HOME` from the env when `socket_dir = None`, but we always pass an
/// explicit `Some(socket_dir)` so the launcher/probe/scan resolve the jail dir
/// directly without env mutation (sound under the multi-threaded test runner).
fn block_on<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(fut)
}

/// Env for spawning daemons via the launcher: the jail env plus a short claim
/// timeout (so leaked daemons reap fast) and a short launch budget (so the
/// crashed-state respawn assertion has a tight wall-clock bound). The launcher
/// spawns the daemon with the CURRENT process env, so we set these on the
/// process for the duration of a test that launches. To stay sound under the
/// parallel runner we DON'T mutate global env; instead each launching test runs
/// with the env applied via the spawned child inheriting our process env — so we
/// set the few knobs we need with `std::env::set_var` guarded by serial markers.
struct EnvGuard {
    keys: Vec<String>,
}

impl EnvGuard {
    fn set(pairs: &[(&str, &str)]) -> Self {
        let mut keys = Vec::new();
        for (k, v) in pairs {
            std::env::set_var(k, v);
            keys.push(k.to_string());
        }
        Self { keys }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for k in &self.keys {
            std::env::remove_var(k);
        }
    }
}

/// Apply the jail env to the current process (so launched daemons inherit HOME,
/// XDG_*, SB_HOME, TMPDIR, lock dir). Returns a guard that restores on drop.
fn apply_jail_env(jail: &libmod::jail::Jail) -> EnvGuard {
    let pairs = jail_env(jail);
    let refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    EnvGuard::set(&refs)
}

/// Spawn a SINGLE-SESSION daemon DIRECTLY as our child (so we capture its pid
/// and can SIGKILL it deterministically — the launcher setsids the daemon, which
/// would reparent it to init and complicate the crashed/stale arms). Mirrors the
/// wsc_m2 `start_single_session_daemon` shape.
fn spawn_daemon_direct(
    jail: &libmod::jail::Jail,
    session: &str,
    extra_env: &[(&str, &str)],
) -> Result<Child, Box<dyn Error>> {
    let mut env_vars = jail_env(jail);
    for (k, v) in extra_env {
        env_vars.push((k.to_string(), v.to_string()));
    }
    if !env_vars.iter().any(|(k, _)| k == "PATH") {
        env_vars.push(("PATH".into(), "/usr/bin:/bin".into()));
    }
    if !env_vars.iter().any(|(k, _)| k == "TERM") {
        env_vars.push(("TERM".into(), "xterm-256color".into()));
    }
    let socket = jail.socket_dir.join(format!("{session}.sock"));
    let _ = std::fs::remove_file(&socket);
    let child = Command::new(qrmux_binary())
        .arg("server")
        .arg("--socket-dir")
        .arg(&jail.socket_dir)
        .arg("--session")
        .arg(session)
        .env_clear()
        .envs(env_vars.iter().cloned())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Wait for the socket to appear (bounded).
    let start = Instant::now();
    while !socket.exists() {
        if start.elapsed() > Duration::from_secs(5) {
            return Err(format!("daemon socket {socket:?} not created in 5s").into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(child)
}

/// Clean up launcher artifacts before `teardown_jail`. The launcher creates
/// per-session `<name>.lock` and `<name>.log` siblings (which the strict jail
/// leftover check would flag) and setsids the daemon (orphaned → not reaped by
/// the socket sweep). This sweeps our orphan daemons, then removes every
/// `.sock`/`.lock`/`.log` leaf so teardown's strict check passes.
fn cleanup_launcher_artifacts(dir: &Path) {
    // Kill setsid'd daemons our launcher spawned (ppid 1, exact-binary match —
    // fail-closed, never touches other binaries' parented daemons).
    sweep_orphan_daemons();
    std::thread::sleep(Duration::from_millis(150));
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let leaf = entry.file_name();
            let leaf = leaf.to_string_lossy();
            if leaf.ends_with(".sock") || leaf.ends_with(".lock") || leaf.ends_with(".log") {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Count live per-session sockets in the jail dir (excludes the legacy leaf).
fn count_session_sockets(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".sock") && n != "qrmux.sock")
                .count()
        })
        .unwrap_or(0)
}

// These tests mutate process env (HOME/XDG_*/SB_HOME via apply_jail_env and the
// claim/budget knobs) and spawn daemons that inherit it; they must NOT run
// concurrently with each other. The shared build-lock serializes whole `cargo
// test` invocations across the host, but WITHIN this binary tests run in
// parallel by default. We force a process-wide mutex so env-mutating tests
// serialize.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// §4.2 (1)+(3): a cold start spawns a daemon and the probe handshakes ready;
/// a SECOND ensure is idempotent — no second daemon (exactly one socket, and
/// the same daemon serves the claim window).
#[test]
fn ensure_cold_start_then_idempotent() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_cold")?;
    let _env = apply_jail_env(&jail);
    let _claim = EnvGuard::set(&[("QRMUX_CLAIM_TIMEOUT_MS", "60000")]);
    sweep_orphan_daemons();
    let dir = jail.socket_dir.clone();
    let spec = test_launch_spec();

    block_on(async {
        ensure_session_server_running(Some(&dir), "alpha", Some(&spec))
            .await
            .expect("cold-start ensure failed");
    });
    let socket = dir.join("alpha.sock");
    assert!(socket.exists(), "cold-start did not bind alpha.sock");
    assert_eq!(
        count_session_sockets(&dir),
        1,
        "expected exactly one daemon"
    );

    // Second ensure: idempotent fast path, NO second daemon.
    block_on(async {
        ensure_session_server_running(Some(&dir), "alpha", Some(&spec))
            .await
            .expect("idempotent ensure failed");
    });
    assert_eq!(
        count_session_sockets(&dir),
        1,
        "second ensure spawned a second daemon (not idempotent)"
    );

    // Kill the daemon by sweeping the socket dir at teardown.
    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// §4.2 fourth-state-adjacent CRASHED: a SIGKILLed daemon leaves a stale socket;
/// the next ensure detects ECONNREFUSED (NAMED crashed state), unlinks under the
/// flock, and respawns — WITHOUT burning the legacy 5s poll worst case. We bound
/// the wall-clock tightly.
#[test]
fn ensure_crashed_daemon_respawns_fast() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_crashed")?;
    let _env = apply_jail_env(&jail);
    let _claim = EnvGuard::set(&[("QRMUX_CLAIM_TIMEOUT_MS", "60000")]);
    sweep_orphan_daemons();
    let dir = jail.socket_dir.clone();
    let socket = dir.join("alpha.sock");
    let spec = test_launch_spec();

    // Bring a daemon up DIRECTLY (our child), then SIGKILL it leaving the stale
    // socket behind (SIGKILL does not run exit-on-end, so no unlink).
    let mut daemon = spawn_daemon_direct(&jail, "alpha", &[("QRMUX_CLAIM_TIMEOUT_MS", "60000")])?;
    assert!(socket.exists());
    let _ = daemon.kill();
    let _ = daemon.wait(); // reap
                           // The socket file should still be present (SIGKILL doesn't unlink).
    assert!(
        socket.exists(),
        "test setup: SIGKILL should leave the stale socket"
    );

    // Now ensure again: must detect crashed → unlink → respawn, FAST.
    let start = Instant::now();
    block_on(async {
        ensure_session_server_running(Some(&dir), "alpha", Some(&spec))
            .await
            .expect("crashed-state respawn failed");
    });
    let elapsed = start.elapsed();
    assert!(socket.exists(), "respawn did not rebind the socket");
    // Legacy worst case is ~5s of pure poll; the crashed path must not burn it.
    assert!(
        elapsed < Duration::from_secs(4),
        "crashed-state respawn took {:?} (should be well under the 5s legacy poll)",
        elapsed
    );
    assert_eq!(count_session_sockets(&dir), 1);

    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// §4.2 (2): K concurrent ensures of the SAME absent session → exactly ONE
/// daemon (the per-session flock serializes same-session births).
#[test]
fn ensure_concurrent_same_session_one_daemon() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_concur")?;
    let _env = apply_jail_env(&jail);
    let _claim = EnvGuard::set(&[("QRMUX_CLAIM_TIMEOUT_MS", "60000")]);
    sweep_orphan_daemons();
    let dir = jail.socket_dir.clone();

    // 4 threads, each its own runtime, all racing ensure("alpha").
    let mut handles = Vec::new();
    for _ in 0..4 {
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            let spec = test_launch_spec();
            block_on(async {
                ensure_session_server_running(Some(&dir), "alpha", Some(&spec)).await
            })
        }));
    }
    let mut ok = 0;
    for h in handles {
        if h.join().expect("ensure thread panicked").is_ok() {
            ok += 1;
        }
    }
    assert_eq!(ok, 4, "all 4 concurrent ensures should succeed");
    assert_eq!(
        count_session_sockets(&dir),
        1,
        "concurrent same-session ensure must produce exactly ONE daemon"
    );

    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// §4.2 retiring state: a daemon mid-teardown answers the session-ended refusal;
/// ensure backs off, re-probes, and relaunches cleanly once the daemon unlinks
/// (ENOENT → absent). Driven by ending a live session (KillSession) then
/// ensuring again — the relaunch must yield a fresh live daemon.
#[test]
fn ensure_relaunches_after_session_end() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_retiring")?;
    let _env = apply_jail_env(&jail);
    let _claim = EnvGuard::set(&[("QRMUX_CLAIM_TIMEOUT_MS", "60000")]);
    sweep_orphan_daemons();
    let dir = jail.socket_dir.clone();
    let socket = dir.join("alpha.sock");
    let spec = test_launch_spec();

    // Cold-start + claim the session by creating it.
    block_on(async {
        ensure_session_server_running(Some(&dir), "alpha", Some(&spec))
            .await
            .expect("ensure failed");
        // Create the session so a real session exists, then kill it to drive
        // exit-on-end (the daemon will unlink + exit).
        let mut s = connect_session_stream(Some(&dir), "alpha")
            .await
            .expect("connect failed");
        send_verb(
            &mut s,
            qrmux::protocol::ClientMsg::CreateDetached {
                name: "alpha".into(),
                shell_cmd: "sleep 30".into(),
                cwd: dir.clone(),
                history: 100,
            },
        )
        .await
        .expect("create failed");
    });

    // Kill the session → daemon drives exit-on-end (unlink + exit 0).
    block_on(async {
        let mut s = connect_session_stream(Some(&dir), "alpha")
            .await
            .expect("connect for kill failed");
        send_verb(
            &mut s,
            qrmux::protocol::ClientMsg::KillSession {
                name: "alpha".into(),
            },
        )
        .await
        .expect("kill failed");
    });

    // The daemon should exit and unlink within a bounded window.
    let deadline = Instant::now() + Duration::from_secs(8);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    // ensure again → must relaunch cleanly (fresh daemon, socket bound again).
    block_on(async {
        ensure_session_server_running(Some(&dir), "alpha", Some(&spec))
            .await
            .expect("relaunch after session-end failed");
    });
    assert!(socket.exists(), "relaunch did not rebind the socket");
    assert_eq!(count_session_sockets(&dir), 1);

    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// §3.2 identity belt: a daemon serving 'alpha' is reachable via a HARD LINK
/// named 'beta.sock'; connecting with `connect_session_stream(.., "beta")` must
/// fail with the EXACT mismatch error (ServerHello.session = 'alpha' ≠ 'beta').
#[test]
fn identity_belt_rejects_wrong_daemon() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_identity")?;
    let _env = apply_jail_env(&jail);
    let _claim = EnvGuard::set(&[("QRMUX_CLAIM_TIMEOUT_MS", "60000")]);
    sweep_orphan_daemons();
    let dir = jail.socket_dir.clone();
    let spec = test_launch_spec();

    // Bring up the 'alpha' daemon.
    block_on(async {
        ensure_session_server_running(Some(&dir), "alpha", Some(&spec))
            .await
            .expect("ensure alpha failed");
    });
    let alpha = dir.join("alpha.sock");
    let beta = dir.join("beta.sock");
    // Hard link alpha.sock as beta.sock — connecting to beta reaches alpha's
    // daemon, whose ServerHello.session = "alpha".
    std::fs::hard_link(&alpha, &beta).expect("hard link failed");

    let err = block_on(async { connect_session_stream(Some(&dir), "beta").await })
        .expect_err("connecting to a wrong-identity daemon must fail");
    let msg = err.to_string();
    let expected = format!(
        "qrmux daemon at {} identifies as session 'alpha', expected 'beta'",
        beta.display()
    );
    assert_eq!(msg, expected, "identity mismatch error not exact");

    let _ = std::fs::remove_file(&beta);
    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// §4.3: empty dir → empty scan.
#[test]
fn scan_empty_dir_is_empty() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_scan_empty")?;
    let dir = jail.socket_dir.clone();
    let rows = block_on(async { scan_sessions(Some(&dir)).await }).expect("scan failed");
    assert!(rows.is_empty(), "empty dir should scan to no rows");
    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// §4.3: one LIVE CLAIMED session → exactly one row, identity-matched.
#[test]
fn scan_one_claimed_session_one_row() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_scan_one")?;
    let _env = apply_jail_env(&jail);
    let _claim = EnvGuard::set(&[("QRMUX_CLAIM_TIMEOUT_MS", "60000")]);
    sweep_orphan_daemons();
    let dir = jail.socket_dir.clone();
    let spec = test_launch_spec();

    block_on(async {
        ensure_session_server_running(Some(&dir), "alpha", Some(&spec))
            .await
            .expect("ensure failed");
        // Claim it: create a real session so ListSessions returns ≥1 row.
        let mut s = connect_session_stream(Some(&dir), "alpha")
            .await
            .expect("connect failed");
        send_verb(
            &mut s,
            qrmux::protocol::ClientMsg::CreateDetached {
                name: "alpha".into(),
                shell_cmd: "sleep 30".into(),
                cwd: dir.clone(),
                history: 100,
            },
        )
        .await
        .expect("create failed");
    });

    let rows = block_on(async { scan_sessions(Some(&dir)).await }).expect("scan failed");
    assert_eq!(rows.len(), 1, "expected exactly one claimed-session row");
    assert_eq!(rows[0].name, "alpha");

    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// §4.3 / red-team M3: an UNCLAIMED daemon (socket present, Hello answers, 0
/// sessions) is INVISIBLE to scan — 0 rows — AND the scan does NOT kill it.
#[test]
fn scan_unclaimed_daemon_is_invisible_and_survives() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_scan_unclaimed")?;
    let _env = apply_jail_env(&jail);
    // Long claim timeout so the unclaimed daemon stays up across the scan.
    let _claim = EnvGuard::set(&[("QRMUX_CLAIM_TIMEOUT_MS", "60000")]);
    sweep_orphan_daemons();
    let dir = jail.socket_dir.clone();
    let socket = dir.join("alpha.sock");
    let spec = test_launch_spec();

    // ensure brings up a daemon but does NOT claim it (no session created).
    block_on(async {
        ensure_session_server_running(Some(&dir), "alpha", Some(&spec))
            .await
            .expect("ensure failed");
    });
    assert!(socket.exists(), "unclaimed daemon socket should exist");

    let rows = block_on(async { scan_sessions(Some(&dir)).await }).expect("scan failed");
    assert!(
        rows.is_empty(),
        "an unclaimed daemon must be INVISIBLE to scan (red-team M3), got {:?}",
        rows
    );
    // The scan must NOT have killed it: socket still present, daemon still
    // answers a handshake.
    assert!(
        socket.exists(),
        "scan unlinked an unclaimed daemon's socket"
    );
    let still_up = block_on(async { connect_session_stream(Some(&dir), "alpha").await }).is_ok();
    assert!(still_up, "scan killed the unclaimed daemon");

    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// §4.3 / red-team M9: a stale socket (SIGKILLed daemon) is SKIPPED, the socket
/// is UNLINKED (ConnectionRefused cleanup), and the `.log` is NOT touched.
#[test]
fn scan_stale_socket_unlinked_log_preserved() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_scan_stale")?;
    let _env = apply_jail_env(&jail);
    let _claim = EnvGuard::set(&[("QRMUX_CLAIM_TIMEOUT_MS", "60000")]);
    sweep_orphan_daemons();
    let dir = jail.socket_dir.clone();
    let socket = dir.join("alpha.sock");
    let log = dir.join("alpha.log");

    // Spawn the daemon DIRECTLY with stderr → the per-session `<name>.log` (so a
    // log file exists for the "don't touch the log" assertion) and capture the
    // pid so we can SIGKILL deterministically.
    let mut env_vars = jail_env(&jail);
    env_vars.push(("QRMUX_CLAIM_TIMEOUT_MS".into(), "60000".into()));
    env_vars.push(("PATH".into(), "/usr/bin:/bin".into()));
    env_vars.push(("TERM".into(), "xterm-256color".into()));
    let _ = std::fs::remove_file(&socket);
    let mut daemon = Command::new(qrmux_binary())
        .arg("server")
        .arg("--socket-dir")
        .arg(&dir)
        .arg("--session")
        .arg("alpha")
        .env_clear()
        .envs(env_vars.iter().cloned())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&log)?))
        .spawn()?;
    let start = Instant::now();
    while !socket.exists() {
        if start.elapsed() > Duration::from_secs(5) {
            daemon.kill().ok();
            return Err("daemon socket not created in 5s".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(socket.exists());
    assert!(log.exists(), "per-session .log should have been created");
    let log_contents_before = std::fs::read(&log).unwrap_or_default();

    // SIGKILL the daemon → stale socket left behind (no unlink).
    let _ = daemon.kill();
    let _ = daemon.wait();
    std::thread::sleep(Duration::from_millis(200));
    assert!(socket.exists(), "SIGKILL should leave a stale socket");

    // Scan: ConnectionRefused → skip + per-target unlink of the SOCKET only.
    let rows = block_on(async { scan_sessions(Some(&dir)).await }).expect("scan failed");
    assert!(rows.is_empty(), "stale socket must not surface a row");
    // Socket unlinked.
    assert!(!socket.exists(), "stale socket should have been unlinked");
    // .log preserved (red-team M9: discovery never deletes logs).
    assert!(log.exists(), "scan must NOT delete the .log (red-team M9)");
    let log_contents_after = std::fs::read(&log).unwrap_or_default();
    assert_eq!(
        log_contents_before, log_contents_after,
        ".log contents must be untouched by the scan"
    );

    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// §4.3 / §5.3: a legacy `qrmux.sock` present is EXCLUDED from the scan — never
/// probed as a session.
#[test]
fn scan_excludes_legacy_qrmux_sock() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_scan_legacy")?;
    let _env = apply_jail_env(&jail);
    let _claim = EnvGuard::set(&[("QRMUX_CLAIM_TIMEOUT_MS", "60000")]);
    sweep_orphan_daemons();
    let dir = jail.socket_dir.clone();
    let spec = test_launch_spec();

    // A live per-session daemon (claimed) AND a planted legacy qrmux.sock file.
    block_on(async {
        ensure_session_server_running(Some(&dir), "alpha", Some(&spec))
            .await
            .expect("ensure failed");
        let mut s = connect_session_stream(Some(&dir), "alpha")
            .await
            .expect("connect failed");
        send_verb(
            &mut s,
            qrmux::protocol::ClientMsg::CreateDetached {
                name: "alpha".into(),
                shell_cmd: "sleep 30".into(),
                cwd: dir.clone(),
                history: 100,
            },
        )
        .await
        .expect("create failed");
    });
    // Plant a bogus legacy socket FILE (not a live listener). If the scan probed
    // it, ConnectionRefused would unlink it; we assert it is left ALONE (never
    // probed) by making it a plain file the scan must skip by NAME.
    let legacy = dir.join("qrmux.sock");
    std::fs::write(&legacy, b"not a socket").expect("plant legacy file");

    let rows = block_on(async { scan_sessions(Some(&dir)).await }).expect("scan failed");
    assert_eq!(rows.len(), 1, "only the per-session daemon should surface");
    assert_eq!(rows[0].name, "alpha");
    // The legacy leaf was excluded by NAME — never probed, so never unlinked.
    assert!(
        legacy.exists(),
        "legacy qrmux.sock must be excluded by name, not probed/unlinked"
    );

    let _ = std::fs::remove_file(&legacy);
    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

/// punch item 16 (b3-kill-spec) PIN: TRANSIENT refusal must NOT unlink — the
/// socket SURVIVES and the row is returned once the daemon answers. The
/// macOS shape this defends: a LIVE daemon under a full listen backlog
/// refuses a connect; the old single-refusal classification unlinked its
/// socket (permanent orphan, wrong victim). Deterministic simulation: an
/// AF_UNIX socket BOUND but not yet LISTENING refuses exactly like a full
/// backlog; calling `listen()` on the SAME fd flips it to accepting with no
/// unlink/rebind window. A helper thread listens at +150ms and then speaks
/// the v3 probe protocol (ServerHello + a 1-row SessionList), so the scan's
/// confirmation retries (at ~100ms and ~350ms) land on a live daemon.
#[test]
fn scan_transient_refusal_survives_and_returns_row() -> Result<(), Box<dyn Error>> {
    use nix::libc;
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let jail = setup_jail("wsc_m3a_scan_transient")?;
    let dir = jail.socket_dir.clone();
    let socket = dir.join("alpha.sock");

    // Bind WITHOUT listen: connects now fail ECONNREFUSED (validated shape).
    let fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(fd >= 0, "socket() failed");
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path_bytes = socket.as_os_str().as_encoded_bytes();
        assert!(path_bytes.len() < std::mem::size_of_val(&addr.sun_path));
        for (i, b) in path_bytes.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        let rc = libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        assert_eq!(rc, 0, "bind failed: {}", std::io::Error::last_os_error());
        fd
    };
    assert!(socket.exists(), "bound socket file must exist");

    // The fake daemon: at +150ms (inside the scan's 100ms+250ms backoff
    // budget, with margin on both sides) flip to LISTENING on the same fd,
    // accept the retry probe, and answer ServerHello{session:"alpha"} +
    // SessionList([alpha]) — the minimum the probe needs for a Rows outcome.
    let daemon = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        unsafe {
            assert_eq!(libc::listen(fd, 16), 0, "listen failed");
        }
        let cfd = unsafe { libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if cfd >= 0 {
            let mut s = unsafe { StdUnixStream::from_raw_fd(cfd) };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf); // the pipelined preamble+Hello+ListSessions
            let hello = qrmux::protocol::encode(&qrmux::protocol::ServerMsg::Hello {
                caps: vec![],
                session: "alpha".into(),
            })
            .unwrap();
            let list = qrmux::protocol::encode(&qrmux::protocol::ServerMsg::SessionList(vec![
                qrmux::protocol::SessionInfo {
                    name: "alpha".into(),
                    pid: 4242,
                    cols: 80,
                    rows: 24,
                    created: Some(0),
                },
            ]))
            .unwrap();
            let _ = s.write_all(&hello);
            let _ = s.write_all(&list);
            let _ = s.flush();
            // Hold the stream open long enough for the probe to read.
            std::thread::sleep(Duration::from_millis(300));
        }
        unsafe { libc::close(fd) };
    });

    // Scan: first probe refused (transient), a confirmation retry reaches the
    // now-listening daemon → the row is returned and NOTHING is unlinked.
    let rows = block_on(async { scan_sessions(Some(&dir)).await }).expect("scan failed");
    // Join the helper FIRST (S4): if its listen()/accept asserts fired, surface
    // that panic directly rather than letting the row assertions below fail with
    // a misleading "got []" that discards the real errno.
    daemon.join().expect("daemon thread");
    assert_eq!(
        rows.len(),
        1,
        "transient refusal then accept must surface the row, got {rows:?}"
    );
    assert_eq!(rows[0].name, "alpha");
    assert!(
        socket.exists(),
        "WRONG VICTIM: the transiently-refusing live daemon's socket was unlinked"
    );

    let _ = std::fs::remove_file(&socket);
    cleanup_launcher_artifacts(&dir);
    teardown_jail(&jail)?;
    Ok(())
}

// ===========================================================================
// helpers
// ===========================================================================

/// Send a single verb on an already-handshaken stream and await its reply,
/// returning Ok on a non-Error reply, Err on a framed Error.
async fn send_verb(
    stream: &mut tokio::net::UnixStream,
    msg: qrmux::protocol::ClientMsg,
) -> Result<qrmux::protocol::ServerMsg, String> {
    use qrmux::protocol::{encode, FrameReader, ServerMsg};
    use tokio::io::AsyncWriteExt;
    let bytes = encode(&msg).map_err(|e| e.to_string())?;
    stream.write_all(&bytes).await.map_err(|e| e.to_string())?;
    let mut frames = FrameReader::new();
    loop {
        if let Some(reply) = frames
            .decode_next::<ServerMsg>()
            .map_err(|e| e.to_string())?
        {
            return match reply {
                ServerMsg::Error(e) => Err(e),
                other => Ok(other),
            };
        }
        match frames.fill_from(stream).await {
            Ok(true) => {}
            Ok(false) => return Err("closed before reply".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
}
