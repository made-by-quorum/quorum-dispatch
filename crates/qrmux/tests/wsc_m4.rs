//! WS-C M4 QRMUX-LEVEL gate arms (spec §7). These drive a REAL per-session
//! `qrmux server --session <name> --socket-dir <dir>` daemon inside a hermetic
//! jail and exercise the wire-level teeth that only a live socket exposes:
//!
//!   * **G-SKEW (live twins)** — the codec-level halves landed in M1; these are
//!     the LIVE arms against a running daemon: a v2-preamble client → the EXACT
//!     framed version-mismatch refusal; a non-Hello first frame → the EXACT
//!     "expected Hello" refusal; the ServerHello.session identity belt fires on a
//!     wrong-daemon connect (exact mismatch error).
//!   * **G-COLDSTART-N (c) gate-grade** — a launched-but-unclaimed daemon with a
//!     SHORT `QRMUX_CLAIM_TIMEOUT_MS`: `qd ls`-shaped scan shows NO phantom row
//!     WHILE unclaimed (red-team M3), Hello-only probes do NOT extend its life
//!     (B1 reset rule), and it exits within budget with the socket gone.
//!   * **G-COLDSTART-N (e) W-2 ROWS** — (i) the KillSession reply-flush race:
//!     the `SessionKilled` reply frame ARRIVES intact (now held by the §4.1
//!     bounded in-flight WAIT that replaced the deleted fixed 150ms grace —
//!     orc ruling relay-1780796003401-33), asserted not assumed; (ii) the
//!     ended-window refusal: a NEW connect issuing a session-addressed verb
//!     while teardown is underway gets the named session-ended error (never a
//!     hang, never a success). Row (ii) holds the window open DETERMINISTICALLY
//!     via a slow KillSession drop rather than racing a fixed grace.
//!
//! The engine-level isolation / cold-start / events / legacy arms (G-ISOL,
//! G-COLDSTART-N a/b/d, G-EVSPLIT, G-LEGACY) drive the real `qd` binary and live
//! in `crates/qd/tests/c1_gate_inc/wsc_m4_rows.rs`.

#![allow(dead_code, unused_imports)]

#[path = "lib/mod.rs"]
mod libmod;

use libmod::client::{pid_alive, qrmux_binary, sweep_orphan_daemons, ProtocolClient};
use libmod::{create_session, jail_env, list_sessions, send_to_session, setup_jail, teardown_jail};
use std::error::Error;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use qrmux::protocol::handshake::{PREAMBLE_MAGIC, PROTOCOL_VERSION};
use qrmux::protocol::{encode, ClientMsg, FrameReader, ServerMsg, ERR_EXPECTED_HELLO};
use qrmux::server::client_handler::ERR_SESSION_ENDED;

/// A single-session daemon child (mirrors the wsc_m2/wsc_m3a harness): the test
/// is the daemon's DIRECT parent and must reap it so `kill -0`/`try_wait` don't
/// lie on a zombie.
struct Daemon {
    child: Child,
    pub socket: PathBuf,
}

impl Daemon {
    fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
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
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a single-session daemon (`server --session <name> --socket-dir <dir>`)
/// in the jail with extra env (e.g. a short `QRMUX_CLAIM_TIMEOUT_MS`), and
/// optionally wait for the socket leaf to appear.
fn start_single_session_daemon(
    jail: &libmod::jail::Jail,
    session: &str,
    extra_env: &[(String, String)],
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
    if wait_for_socket {
        let start = Instant::now();
        while !socket_path.exists() {
            if start.elapsed() > Duration::from_secs(5) {
                return Err(format!("daemon socket {socket_path:?} not created in 5s").into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(Daemon {
        child,
        socket: socket_path,
    })
}

/// ECONNREFUSED-retry at connect (punch item 16, launcher-lane parallel). The
/// daemon-start helper returns on socket-FILE existence, not accept-readiness;
/// under full-suite load the bound socket can momentarily refuse a connect before
/// its accept loop is scheduled — a transient ECONNREFUSED, not a dead daemon.
/// Retry the refusal with backoff; ENOENT or a refusal past the deadline still
/// fails honestly.
fn connect_live_std(socket: &std::path::Path) -> std::io::Result<StdUnixStream> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match StdUnixStream::connect(socket) {
            Ok(s) => return Ok(s),
            Err(e)
                if e.kind() == std::io::ErrorKind::ConnectionRefused
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Read exactly ONE framed `ServerMsg` from a RAW (no-preamble-yet) std socket,
/// bounded. Used by the skew arms that hand-roll the preamble/first-frame bytes
/// (so the codec's normal client path can't paper over the refusal).
fn read_one_server_msg_raw(stream: &mut StdUnixStream, budget: Duration) -> Option<ServerMsg> {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set read timeout");
    let mut acc: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + budget;
    loop {
        // Try to decode whatever we have so far (FrameReader owns the accumulated
        // bytes; rebuilt each pass since we only need the first frame).
        let mut reader = FrameReader::with_leftover(acc.clone());
        if let Ok(Some(msg)) = reader.decode_next::<ServerMsg>() {
            return Some(msg);
        }
        if Instant::now() > deadline {
            return None;
        }
        match stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => acc.extend_from_slice(&buf[..n]),
            // Read timed out (WouldBlock): keep looping until the budget.
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return None,
        }
    }
}

// ===========================================================================
// G-SKEW (live twins) — §7 G-SKEW, M1 codec halves' live counterparts.
// ===========================================================================

/// G-SKEW (a): a v2-preamble client connecting to a LIVE v3 per-session daemon
/// gets the EXACT framed version-mismatch refusal, then the daemon closes. The
/// refusal is a clean frame, never a hang or a misparse.
#[test]
fn g_skew_live_v2_preamble_refused_exact() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m4_skew_v2")?;
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[("QRMUX_CLAIM_TIMEOUT_MS".into(), "60000".into())],
        true,
    )?;
    let socket = daemon.socket.clone();

    let mut stream = connect_live_std(&socket)?;
    // A v2 client: same magic, version byte 2 (a stale client below the server's
    // current PROTOCOL_VERSION; the framed refusal interpolates the live version).
    let mut preamble = [0u8; 5];
    preamble[..4].copy_from_slice(&PREAMBLE_MAGIC);
    preamble[4] = 2;
    stream.write_all(&preamble)?;
    stream.flush()?;

    let msg = read_one_server_msg_raw(&mut stream, Duration::from_secs(3));
    let expected = format!(
        "protocol version mismatch: client v2, server v{PROTOCOL_VERSION} — refusing connection"
    );
    match msg {
        Some(ServerMsg::Error(e)) => assert_eq!(e, expected, "exact v2→v3 framed refusal"),
        other => panic!("expected framed version-mismatch Error, got {other:?}"),
    }

    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

/// G-SKEW (b): a client that completes the preamble but sends a NON-Hello first
/// frame gets the EXACT "expected Hello" framed refusal, then close.
#[test]
fn g_skew_live_non_hello_first_frame_refused_exact() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m4_skew_hello")?;
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[("QRMUX_CLAIM_TIMEOUT_MS".into(), "60000".into())],
        true,
    )?;
    let socket = daemon.socket.clone();

    let mut stream = connect_live_std(&socket)?;
    // Correct v3 preamble...
    let mut preamble = [0u8; 5];
    preamble[..4].copy_from_slice(&PREAMBLE_MAGIC);
    preamble[4] = PROTOCOL_VERSION;
    stream.write_all(&preamble)?;
    // ...but the FIRST frame is ListSessions, not Hello (a Hello-first violation).
    let bad_first = encode(&ClientMsg::ListSessions).expect("encode ListSessions");
    stream.write_all(&bad_first)?;
    stream.flush()?;

    let msg = read_one_server_msg_raw(&mut stream, Duration::from_secs(3));
    match msg {
        Some(ServerMsg::Error(e)) => {
            assert_eq!(e, ERR_EXPECTED_HELLO, "exact expected-Hello framed refusal")
        }
        other => panic!("expected framed expected-Hello Error, got {other:?}"),
    }

    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

/// G-SKEW (c): the ServerHello identity belt — a daemon serving 'alpha' reached
/// via a `beta.sock` hard link reports `session: "alpha"`. A client expecting
/// 'beta' (via `connect_session_stream`) must fail with the EXACT mismatch error.
#[test]
fn g_skew_live_identity_belt_wrong_daemon() -> Result<(), Box<dyn Error>> {
    use qrmux::client::session_client::connect_session_stream;

    let jail = setup_jail("wsc_m4_skew_identity")?;
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[("QRMUX_CLAIM_TIMEOUT_MS".into(), "60000".into())],
        true,
    )?;
    let alpha = daemon.socket.clone();
    let dir = jail.socket_dir.clone();
    let beta = dir.join("beta.sock");
    std::fs::hard_link(&alpha, &beta).expect("hard link alpha.sock → beta.sock");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let err = rt
        .block_on(async { connect_session_stream(Some(&dir), "beta").await })
        .expect_err("connect to a wrong-identity daemon must fail");
    let expected = format!(
        "qrmux daemon at {} identifies as session 'alpha', expected 'beta'",
        beta.display()
    );
    assert_eq!(
        err.to_string(),
        expected,
        "exact identity-belt mismatch error"
    );

    let _ = std::fs::remove_file(&beta);
    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

// ===========================================================================
// G-COLDSTART-N (c) GATE-GRADE — claim-timeout: a launched-but-unclaimed daemon
// shows NO phantom row, Hello-only probes do NOT extend its life, and it exits
// within budget with the socket gone (red-team M3 + B1, promoted to gate-grade).
// ===========================================================================

#[test]
fn g_coldstart_claim_timeout_no_phantom_then_reaped() -> Result<(), Box<dyn Error>> {
    use qrmux::client::discovery::scan_sessions;

    let jail = setup_jail("wsc_m4_claim_phantom")?;
    let claim_ms = 1200u64;
    let mut daemon = start_single_session_daemon(
        &jail,
        "ghost",
        &[("QRMUX_CLAIM_TIMEOUT_MS".into(), claim_ms.to_string())],
        true,
    )?;
    let socket = daemon.socket.clone();
    let dir = jail.socket_dir.clone();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // WHILE unclaimed: the scan (the `qd ls` engine path) shows NO phantom row —
    // an unclaimed daemon (0 ListSessions rows) is INVISIBLE (red-team M3).
    let rows = rt
        .block_on(async { scan_sessions(Some(&dir)).await })
        .expect("scan failed");
    assert!(
        rows.is_empty(),
        "unclaimed daemon must NOT surface a phantom ls row, got {rows:?}"
    );
    // The probe (a Hello-only connect-and-list) must NOT have extended the
    // daemon's life: it still exists right after the scan...
    assert!(
        socket.exists(),
        "scan should not have reaped the daemon yet"
    );

    // ...and despite repeated Hello-only scans faster than the budget, the claim
    // timer is NOT reset (B1): the daemon still expires within budget + slack.
    let probe_until = Instant::now() + Duration::from_millis(claim_ms + 600);
    while Instant::now() < probe_until && !daemon.has_exited() {
        let _ = rt.block_on(async { scan_sessions(Some(&dir)).await });
        std::thread::sleep(Duration::from_millis(150));
    }
    assert!(
        daemon.wait_exit(Duration::from_secs(4)),
        "Hello-only scans must NOT keep an unclaimed daemon alive (B1); it must expire within budget"
    );

    // Socket gone (unlink-before-exit, §4.1 step 5).
    let deadline = Instant::now() + Duration::from_secs(2);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!socket.exists(), "reaped daemon left its socket behind");

    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

// ===========================================================================
// G-COLDSTART-N (e) W-2 GRACE ROWS (orc-required).
// ===========================================================================

/// W-2 row (i): the KillSession reply-flush race. KillSession empties the
/// manager in the SAME handler that still owes its `SessionKilled` reply; the
/// §4.1 lost-reply fix's BOUNDED IN-FLIGHT WAIT (which replaced the deleted fixed
/// 150ms grace — orc ruling relay-1780796003401-33) holds the lifecycle exit
/// until that reply (and the SessionEnded flush) reach the socket, instead of
/// gambling a fixed sleep. ASSERTED: the `SessionKilled` frame ARRIVES intact
/// (not assumed). The deterministic differential evidence for the wait closing
/// this race lives in the CR-3 arms (`tests/evidence-41fix/`), not here.
#[test]
fn w2_grace_kill_reply_flush_arrives_intact() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m4_grace_killreply")?;
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[("QRMUX_CLAIM_TIMEOUT_MS".into(), "60000".into())],
        true,
    )?;
    let socket = daemon.socket.clone();
    create_session(&socket, "alpha")?;

    // Issue KillSession over the wire and assert the SessionKilled reply lands
    // (what the §4.1 bounded in-flight wait now guarantees, in place of the
    // deleted fixed grace). Run on a dedicated runtime thread.
    let socket_for_kill = socket.clone();
    let reply: Result<String, String> = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut c = ProtocolClient::connect(&socket_for_kill).await?;
            match c
                .send_and_receive(ClientMsg::KillSession {
                    name: "alpha".into(),
                })
                .await?
            {
                ServerMsg::SessionKilled { name } => Ok(name),
                other => Err(format!("expected SessionKilled, got {other:?}")),
            }
        })
    })
    .join()
    .map_err(|_| "kill thread panicked")?;

    let name = reply.map_err(|e| -> Box<dyn Error> { e.into() })?;
    assert_eq!(name, "alpha", "SessionKilled reply frame arrived intact");

    // The daemon then drives exit-on-end (unlink + exit 0) within a bound.
    assert!(
        daemon.wait_exit(Duration::from_secs(8)),
        "daemon did not exit after KillSession"
    );
    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

/// W-2 row (ii) — REWRITE v2 (exit-reorder addendum; orc-17 F1 ruling
/// relay-1780802523592-41). The ended-window named refusal, observed on a
/// PRE-ACCEPTED held connection, plus the fresh-connect ENOENT clean-absent half.
///
/// RETIRE-WITH-REASON (rider a — names BOTH retirements + CITES all three
/// rulings so the amendment trail survives):
///   - RETIREMENT 1 (orc-14 relay-1780796003401-33): the ORIGINAL test asserted
///     the OBSERVABILITY of a window whose WIDTH was the deleted fixed 150ms
///     grace — an externally-polled fresh-connect race. The §4.1 lost-reply fix
///     replaced that grace with a bounded in-flight WAIT, collapsing the window
///     to ~microseconds when nothing is in flight; that 150ms width-assert died
///     with the grace it was coupled to.
///   - RETIREMENT 2 (orc-16 reorder relay-1780801873547-39 + orc-17 F1 yes
///     relay-1780802523592-41): the exit-reorder unlinks the socket FIRST (step
///     3), so a FRESH connect during teardown gets ENOENT — the named refusal is
///     now IMPOSSIBLE-BY-CONSTRUCTION to observe on a fresh connect. The
///     "fresh-connect observes the named refusal" observable is therefore
///     retired too; orc-17 ruled the named refusal moves to a PRE-ACCEPTED
///     connection (accepted + Hello-complete BEFORE teardown; pre-accepted
///     connections drain INSIDE the bounded wait), and fresh connects become the
///     ENOENT clean-absent half.
///
/// MECHANICS: a held `ProtocolClient` completes its Hello handshake BEFORE any
/// teardown (so its connection is already accepted; the listener-drop in step 3
/// cannot abort it — it owns its own stream). A concurrent KillSession with
/// `QRMUX_TEST_SLOW_DROP_MS=600` empties the manager then BLOCKS in its drop,
/// holding the daemon in `ended==true` for a deterministic ~600ms window
/// (well under the 7s exit-wait bound; the wait will not give up on it). DURING
/// that window we send a session-addressed verb (GetHistory) ON THE HELD
/// connection: the dispatch loop is still alive (the daemon is in the post-loop
/// bounded wait, not yet exited), decodes the frame, hits the
/// `ctx.session_has_ended()` branch, and writes the named `ERR_SESSION_ENDED`.
///
/// RIDER (b) — NON-VACUOUS: we assert the held connection ACTUALLY RECEIVED the
/// named `ERR_SESSION_ENDED`, never merely "did not hang". Deleting the
/// `ctx.session_has_ended()` refusal branch (or renaming/removing the
/// `ERR_SESSION_ENDED` constant) makes the held probe get History/closed instead
/// → this test FAILS (the mutation tooth, unchanged from the original intent).
///
/// RIDER (c) — the ENOENT clean-absent half: fresh connects DURING/AFTER the
/// window get a bounded-time CLEAN error with NO partial reply bytes (the socket
/// is already unlinked → connect-time ENOENT/ECONNREFUSED, never a half-written
/// reply frame). We assert the fresh connect fails at CONNECT time (no bytes ever
/// exchanged) within a bound — the D-LISTRAW absent shape.
#[test]
fn w2_grace_ended_window_refuses_new_connect_named() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m4_grace_ended")?;
    // SLOW_DROP holds the KillSession handler in its blocking drop, keeping the
    // daemon in the ended-but-waiting state for a DETERMINISTIC window. 600ms is
    // comfortably wide for the held-connection verb roundtrip on a local socket,
    // and well under the default 7s exit-wait bound (the wait will not give up on
    // it). Do NOT lower this toward the connect-roundtrip latency.
    let slow_drop_ms = 600u64;
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[
            ("QRMUX_CLAIM_TIMEOUT_MS".into(), "60000".into()),
            ("QRMUX_TEST_SLOW_DROP_MS".into(), slow_drop_ms.to_string()),
        ],
        true,
    )?;
    let socket = daemon.socket.clone();
    create_session(&socket, "alpha")?;

    // HELD connection + coordinated barrier so the in-window verb lands
    // deterministically (accept + Hello-complete BEFORE teardown; verb sent only
    // once teardown is armed and `ended` has flipped).
    use std::sync::mpsc;
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (fire_tx, fire_rx) = mpsc::channel::<()>();

    // Held-connection worker: connect+Hello, announce ready, await the fire
    // signal (sent once teardown is armed), THEN send the in-window verb.
    let socket_held = socket.clone();
    let held_worker = std::thread::spawn(move || -> Result<&'static str, String> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut held = ProtocolClient::connect(&socket_held)
                .await
                .map_err(|e| format!("held connect (pre-teardown): {e}"))?;
            // Connection is accepted + Hello-complete BEFORE teardown.
            ready_tx.send(()).map_err(|_| "ready send".to_string())?;
            // Wait until the main thread has fired the slow KillSession and given
            // the end-watch its 25ms to flip `ended` (fire signal + small beat).
            fire_rx.recv().map_err(|_| "fire recv".to_string())?;
            tokio::time::sleep(Duration::from_millis(120)).await;
            // In-window verb on the HELD connection. Bounded so a hang fails.
            let reply = tokio::time::timeout(
                Duration::from_secs(5),
                held.send_and_receive(ClientMsg::GetHistory {
                    name: "alpha".into(),
                }),
            )
            .await
            .map_err(|_| "held in-window verb HUNG".to_string())?;
            match reply {
                Ok(ServerMsg::Error(e)) if e == ERR_SESSION_ENDED => Ok("named_ended"),
                Ok(ServerMsg::Error(e)) => Err(format!("wrong error (not ENDED): {e}")),
                Ok(other) => Err(format!("expected ERR_SESSION_ENDED, got {other:?}")),
                Err(e) => Err(format!("held connection closed before refusal: {e}")),
            }
        })
    });

    // Wait for the held connection to be accepted + Hello-complete.
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| -> Box<dyn Error> { "held connection never became ready".into() })?;

    // (2) Arm teardown deterministically: a real in-flight KillSession. Its
    // handler empties the manager, then BLOCKS in the slow drop (holding its own
    // in-flight guard) before writing SessionKilled. The 25ms end-watch flips
    // `ended=true` while it sleeps; the post-loop bounded wait then holds the
    // daemon alive (it cannot exit until both guards drop). Detached thread.
    let socket_kill = socket.clone();
    let kill_thread = std::thread::spawn(move || -> Result<(), String> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut c = ProtocolClient::connect(&socket_kill)
                .await
                .map_err(|e| format!("kill connect: {e}"))?;
            match tokio::time::timeout(
                Duration::from_secs(9),
                c.send_and_receive(ClientMsg::KillSession {
                    name: "alpha".into(),
                }),
            )
            .await
            .map_err(|_| "KillSession reply read timed out".to_string())?
            {
                Ok(ServerMsg::SessionKilled { .. }) => Ok(()),
                Ok(other) => Err(format!("expected SessionKilled, got {other:?}")),
                Err(e) => Err(format!("kill reply lost: {e}")),
            }
        })
    });

    // Give the kill handler a beat to empty the manager + the end-watch to flip
    // `ended` (25ms poll), THEN release the held worker to send its in-window
    // verb. The 600ms slow drop guarantees the window is still open.
    std::thread::sleep(Duration::from_millis(150));
    fire_tx.send(()).map_err(|_| "fire send".to_string())?;

    // RIDER (b): the held connection MUST have received the NAMED refusal.
    let held_outcome = held_worker
        .join()
        .map_err(|_| "held worker panicked")?
        .map_err(|e| -> Box<dyn Error> { e.into() })?;
    assert_eq!(
        held_outcome, "named_ended",
        "RIDER (b): the PRE-ACCEPTED held connection must ACTUALLY RECEIVE \
         ERR_SESSION_ENDED during the ended window — non-vacuous, never merely no-hang"
    );

    // Reap the kill thread (returns once its slow drop completes + reply lands).
    kill_thread
        .join()
        .map_err(|_| "kill thread panicked")?
        .map_err(|e| -> Box<dyn Error> { e.into() })?;

    // RIDER (c): the ENOENT clean-absent half. After the early-unlink, FRESH
    // connects fail at CONNECT time (the socket is gone) — a bounded-time CLEAN
    // error with NO partial reply bytes (never a half-written reply frame). We
    // wait for the socket to vanish (early-unlink runs at the top of the post-loop
    // path) then assert a fresh connect cannot even establish.
    let dl = Instant::now() + Duration::from_secs(8);
    while socket.exists() && Instant::now() < dl {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !socket.exists(),
        "early-unlink (reorder step 3) should have removed the socket"
    );
    let socket_fresh = socket.clone();
    let fresh_outcome = std::thread::spawn(move || -> &'static str {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Bounded: a clean absent must fail FAST at connect, never hang.
            match tokio::time::timeout(
                Duration::from_secs(3),
                ProtocolClient::connect(&socket_fresh),
            )
            .await
            {
                // ENOENT/ECONNREFUSED at connect = clean absent, no bytes exchanged.
                Ok(Err(_)) => "clean_absent_at_connect",
                // A connection that established (then any reply) would be a
                // contract break: the socket was supposed to be gone.
                Ok(Ok(_)) => "unexpectedly_connected",
                Err(_) => "connect_hung",
            }
        })
    })
    .join()
    .map_err(|_| "fresh-connect thread panicked")?;
    assert_eq!(
        fresh_outcome, "clean_absent_at_connect",
        "RIDER (c): a fresh connect during/after the ENOENT window must fail CLEANLY \
         at connect time (no partial reply bytes), within a bound"
    );

    // Daemon exits and the socket stays unlinked within a bound.
    assert!(
        daemon.wait_exit(Duration::from_secs(8)),
        "daemon did not exit after session end"
    );
    assert!(!socket.exists(), "socket reappeared after exit-on-end");

    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

// ===========================================================================
// G-ISOL NEGATIVE control (shared-fate, MUST RED) — the gate-integrity twin of
// the engine-level POSITIVE g_isol arm (crates/qd/tests/c1_gate_inc/wsc_m4_rows.rs).
//
// Construction (spec §7, red-team M4): the QRMUX_TEST_SHARED=1 seam collapses
// the per-session split into the retired shared-fate world — (a) the socket leaf
// collapses to one fixed `shared.sock`, and (b) the daemon's capacity-1 gate
// accepts ANY name so the SessionManager goes genuinely multi-session. We spawn
// ONE daemon under the seam, create TWO sessions (alpha, bravo) on it via the
// wire, and assert ONE pid serves BOTH (the precondition that makes the inversion
// non-vacuous). Then the SAME alive-by-operation logic the positive arm uses:
// SIGKILL the one daemon → BOTH children die (shared fate). The negative control
// PASSES by DETECTING that RED — proving G-ISOL cannot pass vacuously.
//
// WHY qrmux-level: two sessions on ONE daemon is a world production NEVER builds;
// driving it through the full `qd new` engine path entangles the artificial mode
// with the boot-waiter / registry-join / shared-manager end-watch in
// production-irrelevant ways. The control's job is gate integrity (the inversion
// can't pass vacuously), not engine-path coverage — the positive arm covers the
// engine path. Here the construction is deterministic.
#[test]
fn g_isol_negative_shared_fate_red() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_m4_isol_neg")?;
    // ONE daemon under the shared-fate seam. Long claim timeout so it stays up.
    // Don't wait on the per-name leaf in the helper (the seam collapses it to
    // shared.sock) — spawn then poll for shared.sock below.
    let mut daemon = start_single_session_daemon(
        &jail,
        "alpha",
        &[
            ("QRMUX_CLAIM_TIMEOUT_MS".into(), "60000".into()),
            ("QRMUX_TEST_SHARED".into(), "1".into()),
        ],
        false,
    )?;
    // The leaf collapsed to shared.sock (both halves of the seam, red-team M4).
    let shared_sock = jail.socket_dir.join("shared.sock");
    let start = Instant::now();
    while !shared_sock.exists() {
        if start.elapsed() > Duration::from_secs(5) {
            daemon.kill();
            teardown_jail(&jail)?;
            return Err("shared.sock not bound under the seam in 5s".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !jail.socket_dir.join("alpha.sock").exists()
            && !jail.socket_dir.join("bravo.sock").exists(),
        "seam must collapse the socket leaf to shared.sock (no per-name sockets)"
    );

    // Create BOTH sessions on the ONE daemon (the capacity gate is open under the
    // seam — the manager genuinely holds two).
    create_session(&shared_sock, "alpha")?;
    create_session(&shared_sock, "bravo")?;

    // PRECONDITION: ONE pid serves BOTH — assert the manager carries two distinct
    // child sessions, and there is exactly ONE daemon process (this one).
    let rows = list_sessions(&shared_sock)?;
    let names: std::collections::HashSet<String> = rows.iter().map(|r| r.name.clone()).collect();
    assert!(
        names.contains("alpha") && names.contains("bravo"),
        "the ONE shared daemon must serve BOTH sessions (manager multi-session), got {names:?}"
    );
    // Both children alive pre-kill (alive BY the existence of their PTY child pids).
    let child_a = rows
        .iter()
        .find(|r| r.name == "alpha")
        .map(|r| r.pid)
        .unwrap_or(0);
    let child_b = rows
        .iter()
        .find(|r| r.name == "bravo")
        .map(|r| r.pid)
        .unwrap_or(0);
    assert!(
        child_a != 0 && child_b != 0 && child_a != child_b,
        "two distinct live children"
    );
    assert!(
        pid_alive(child_a) && pid_alive(child_b),
        "both children must be ALIVE pre-kill"
    );

    // SIGKILL the ONE daemon (abrupt). The SAME alive-by-operation logic the
    // positive arm uses: under shared-fate BOTH children must now die (the RED).
    let daemon_pid = daemon.child.id();
    let _ = daemon.child.kill();
    let _ = daemon.child.wait();

    // The shared-fate RED: BOTH children dead within a bound (one daemon owned
    // both PTY masters; its death tears down BOTH worlds at once).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut a_dead = false;
    let mut b_dead = false;
    while Instant::now() < deadline && !(a_dead && b_dead) {
        a_dead = !pid_alive(child_a);
        b_dead = !pid_alive(child_b);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        a_dead && b_dead,
        "SHARED-FATE RED NOT DETECTED: SIGKILL of the one daemon (pid {daemon_pid}) must kill \
         BOTH children (alpha pid {child_a} dead={a_dead}, bravo pid {child_b} dead={b_dead}) — \
         if B survived, the negative control would not constrain G-ISOL and the inversion could \
         pass vacuously"
    );

    teardown_jail(&jail)?;
    Ok(())
}
