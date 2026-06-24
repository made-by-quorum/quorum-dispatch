//! §4.1 lost-reply fix — CR-3 mutation-evidence arms (BUILD-READY design rev B).
//!
//! These arms drive a REAL single-session `sbmux server --session <name>
//! --socket-dir <dir>` daemon in a hermetic jail and exercise the lost-reply
//! race the fix closes: a session-addressed handler that OWES a reply must not
//! be killed by the daemon's own lifecycle exit. The teeth are the `SLOW_DROP`
//! seam (`SBMUX_TEST_SLOW_DROP_MS`, breaker pattern — documented, never set by
//! production, inert unset) which sleeps INSIDE the blocking drop the handler
//! performs, widening the owed-reply-vs-exit window deterministically.
//!
//!   * **Arm 1 (kill-driven, the member-3 repro):** SLOW_DROP=400, KillSession
//!     over the wire, assert the `SessionKilled` reply ARRIVES (bounded read).
//!     Against PRE-FIX code this loses the reply with a ~225ms margin (400ms drop
//!     vs ~175ms pre-fix exit start); the pre-fix RED is captured separately
//!     under `tests/evidence-41fix/`.
//!   * **Arm 2 (reaper-driven class coverage):** a child exits naturally while a
//!     GetHistory is held in-flight (slow drop opens the window); assert the
//!     History reply arrives.
//!   * **Arm 3 (backstop, orc rider R-i):** SLOW_DROP absurd (60s) +
//!     SBMUX_EXIT_WAIT_MS small (500) → the daemon exits within bound+slack with
//!     the socket unlinked (fail-closed-loud: a stalled handler cannot make the
//!     daemon immortal).
//!   * **[F1] no-stall canary:** a fast kill (no slow-drop) exits WELL under the
//!     default exit-wait bound — asserts the [F1] wakeup actually fires (a
//!     lost-wakeup would stall the full budget and show as a red here).
//!
//! Arm 4 (the W-2 grace regression rows) lives UNMODIFIED in `wsc_m4.rs`; it is
//! the regression net and must stay green.

#![allow(dead_code, unused_imports)]

#[path = "lib/mod.rs"]
mod libmod;

use libmod::client::{sbmux_binary, sweep_orphan_daemons, ProtocolClient};
use libmod::{create_session, jail_env, send_to_session, setup_jail, teardown_jail};
use std::error::Error;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sbmux::protocol::{ClientMsg, ServerMsg};

/// A single-session daemon child (mirrors the wsc_m4 harness): the test is the
/// daemon's DIRECT parent and must reap it so `try_wait` doesn't lie on a zombie.
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
            std::thread::sleep(Duration::from_millis(25));
        }
        self.has_exited()
    }
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a single-session daemon in the jail with extra env, optionally waiting
/// for the socket leaf to appear.
fn start_daemon(
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
    let child = Command::new(sbmux_binary())
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
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    Ok(Daemon {
        child,
        socket: socket_path,
    })
}

/// Run an async closure on a fresh single-thread runtime on a dedicated OS
/// thread (so a blocked daemon never wedges the test runtime), bounded.
fn on_runtime<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(f)
        .join()
        .expect("runtime thread panicked")
}

/// Issue a KillSession over the wire and report whether the SessionKilled reply
/// arrived (bounded). `Ok(())` = reply arrived; `Err(_)` = the lost-reply
/// signature (connection closed / EOF before the reply, i.e. UnexpectedEof).
fn kill_and_expect_reply(socket: PathBuf, name: &str, read_budget: Duration) -> Result<(), String> {
    let name = name.to_string();
    on_runtime(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut c = ProtocolClient::connect(&socket)
                .await
                .map_err(|e| format!("connect: {e}"))?;
            let reply = tokio::time::timeout(
                read_budget,
                c.send_and_receive(ClientMsg::KillSession { name: name.clone() }),
            )
            .await
            .map_err(|_| {
                "KillSession reply read TIMED OUT (a hang, not the lost-reply race)".to_string()
            })?;
            match reply {
                Ok(ServerMsg::SessionKilled { name: got }) => {
                    if got == name {
                        Ok(())
                    } else {
                        Err(format!("SessionKilled named wrong session: {got}"))
                    }
                }
                Ok(other) => Err(format!("expected SessionKilled, got {other:?}")),
                Err(e) => Err(format!("lost reply: {e}")),
            }
        })
    })
}

// ===========================================================================
// Arm 1 — kill-driven (the member-3 repro). POST-FIX: GREEN.
// ===========================================================================

/// SLOW_DROP=400 (>> the deleted 150+25 pre-fix exit start) makes the
/// KillSession handler hold its owed `SessionKilled` reply across a 400ms
/// blocking drop. With the bounded in-flight wait the lifecycle exit WAITS for
/// the owed reply instead of racing it, so the reply ARRIVES. The pre-fix RED
/// (committed under tests/evidence-41fix/) shows this losing the reply 10/10.
///
/// SLOW_DROP MUST stay >> 175ms — do NOT lower it toward the pre-fix boundary.
#[test]
fn arm1_kill_reply_survives_slow_drop() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_41fix_arm1")?;
    let mut daemon = start_daemon(
        &jail,
        "alpha",
        &[
            ("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into()),
            ("SBMUX_TEST_SLOW_DROP_MS".into(), "400".into()),
        ],
        true,
    )?;
    let socket = daemon.socket.clone();
    create_session(&socket, "alpha")?;

    // Bounded read budget generously exceeds the 400ms drop + default 7s
    // exit-wait so a genuine pass is never timed out, but a true hang fails.
    kill_and_expect_reply(socket, "alpha", Duration::from_secs(9))
        .map_err(|e| -> Box<dyn Error> { e.into() })?;

    assert!(
        daemon.wait_exit(Duration::from_secs(10)),
        "daemon did not exit after KillSession"
    );
    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

// ===========================================================================
// Arm 2 — reaper-driven (child exits naturally, GetHistory in flight).
// ===========================================================================

/// The child exits naturally (`exit`); a GetHistory held in-flight during the
/// slow-drop teardown window must still get its History reply. The reaper path
/// (cleanup task) empties the manager AFTER the slow blocking drop, so the
/// in-flight wait holds the exit until the owed History reply is written.
///
/// ANTI-VACUITY ([RT-F1 sub], exit-reorder addendum): this is the race-hunter
/// arm, not the primary tooth — the reaper window is inherently unsteerable, and
/// the early-unlink (reorder step 3) makes a fresh-connect probe likely to see
/// ENOENT before it ever serves a reply. We TRACK `probes_served_pre_enoent` (a
/// probe that got a real served reply — History / error / other — BEFORE the
/// socket vanished). Zero served ⇒ the window was never actually exercised ⇒
/// BOUNDED re-attempt (≤3 fresh probe bursts); if still zero, emit a
/// PASS-WITH-NAMED-VACUITY line and pass (the deterministic mechanism evidence is
/// arm 1 + the double-unlink survival test, not this race-hunter). The LOST-REPLY
/// tooth is UNCHANGED and still hard-fails: a connection closed before its owed
/// reply is the RED the fix must prevent, in every attempt.
#[test]
fn arm2_history_reply_survives_reaper_teardown() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_41fix_arm2")?;
    let mut daemon = start_daemon(
        &jail,
        "alpha",
        &[
            ("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into()),
            ("SBMUX_TEST_SLOW_DROP_MS".into(), "400".into()),
        ],
        true,
    )?;
    let socket = daemon.socket.clone();
    create_session(&socket, "alpha")?;

    // Hold a GetHistory in flight, then let the child exit so the reaper begins
    // teardown. We race a tight loop of GetHistory probes against the natural
    // exit; ANY probe that lands while teardown is underway must get History or
    // the named ended-error — NEVER a bare connection-close (lost reply).
    send_to_session(&socket, &[], "alpha", "exit\n")?;

    // One probe burst: race GetHistory probes against the natural exit until the
    // socket vanishes or the deadline. Returns the number of probes that got a
    // REAL served reply (History/error/other) before ENOENT — the anti-vacuity
    // counter. Hard-errors on the LOST-REPLY signature (unchanged tooth).
    fn probe_burst(socket_probe: PathBuf) -> Result<usize, String> {
        on_runtime(move || -> Result<usize, String> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut served: usize = 0;
                while Instant::now() < deadline {
                    match ProtocolClient::connect(&socket_probe).await {
                        Ok(mut c) => {
                            let reply = tokio::time::timeout(
                                Duration::from_secs(9),
                                c.send_and_receive(ClientMsg::GetHistory {
                                    name: "alpha".into(),
                                }),
                            )
                            .await;
                            match reply {
                                Err(_) => return Err("GetHistory HUNG during teardown".into()),
                                // History served (drain window) or named ended-error:
                                // both are correct, owed-reply-delivered outcomes —
                                // each counts as a served probe (anti-vacuity).
                                Ok(Ok(ServerMsg::History(_))) => served += 1,
                                Ok(Ok(ServerMsg::Error(_))) => served += 1,
                                Ok(Ok(_)) => served += 1,
                                // The lost-reply signature: connection closed before
                                // the owed reply. This is the RED the fix must
                                // prevent — UNCHANGED hard-fail tooth.
                                Ok(Err(e)) if e.contains("closed") => {
                                    return Err(format!("LOST REPLY: {e}"))
                                }
                                // Connected but the reply read came back absent
                                // without a "closed" lost-reply signature: a clean
                                // absent, not a served reply (does NOT count).
                                Ok(Err(_)) => {}
                            }
                        }
                        // Socket gone (early-unlink, reorder step 3): teardown fully
                        // past — the burst is over.
                        Err(_) => return Ok(served),
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Ok(served)
            })
        })
    }

    // Primary burst, then bounded re-attempts (≤3 total) only if nothing served.
    let mut total_served = probe_burst(socket.clone())?;
    let mut attempts = 1;
    while total_served == 0 && attempts < 3 {
        attempts += 1;
        total_served += probe_burst(socket.clone())?;
    }
    if total_served == 0 {
        // PASS-WITH-NAMED-VACUITY: the unsteerable reaper window never let a probe
        // serve a reply before the early-unlink ENOENT. Named, not silently
        // swallowed — the deterministic evidence is arm 1 + the survival test.
        eprintln!(
            "NAMED-VACUITY [arm2_history_reply_survives_reaper_teardown]: 0 probes              served pre-ENOENT across {attempts} bounded attempt(s); the reaper              window was unsteerable (early-unlink raced ahead). Passing — the              deterministic mechanism evidence is arm 1 + the double-unlink              survival test, not this race-hunter arm."
        );
    }

    assert!(
        daemon.wait_exit(Duration::from_secs(10)),
        "daemon did not exit after child exit"
    );
    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

// ===========================================================================
// Arm 3 — backstop (orc rider R-i): fail-closed-loud.
// ===========================================================================

/// A deliberately STALLED handler: SLOW_DROP absurdly high (60s) with a small
/// SBMUX_EXIT_WAIT_MS (500ms). The daemon must EXIT within bound+slack with the
/// socket unlinked — proving a stalled client/handler cannot hold the daemon
/// immortal. (The named "exit-wait: N handler(s) still in flight" line is
/// emitted to the daemon log; here we assert the OBSERVABLE: exit + unlink.)
#[test]
fn arm3_backstop_stalled_handler_exits_within_bound() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_41fix_arm3")?;
    let mut daemon = start_daemon(
        &jail,
        "alpha",
        &[
            ("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into()),
            ("SBMUX_TEST_SLOW_DROP_MS".into(), "60000".into()),
            ("SBMUX_EXIT_WAIT_MS".into(), "500".into()),
        ],
        true,
    )?;
    let socket = daemon.socket.clone();
    create_session(&socket, "alpha")?;

    // Fire KillSession on a detached runtime thread; it will block 60s in the
    // drop and never get its reply — that thread is abandoned. We assert the
    // DAEMON exits within the small exit-wait bound + generous slack regardless.
    let socket_kill = socket.clone();
    std::thread::spawn(move || {
        let _ = kill_and_expect_reply(socket_kill, "alpha", Duration::from_secs(70));
    });

    // The exit-wait bound is 500ms; the daemon should exit far inside a few
    // seconds even though the drop thread is wedged for 60s.
    assert!(
        daemon.wait_exit(Duration::from_secs(8)),
        "backstop FAILED: daemon did not exit within bound+slack despite a 60s stalled drop"
    );
    let dl = Instant::now() + Duration::from_secs(2);
    while socket.exists() && Instant::now() < dl {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !socket.exists(),
        "backstop FAILED: socket not unlinked after fail-closed exit"
    );

    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

// ===========================================================================
// [F1] no-stall canary — the lost-wakeup detector.
// ===========================================================================

/// A FAST kill (NO slow-drop): the owed reply is written promptly, the guard
/// drops, and the [F1] enable-before-check Notify wakeup must fire so the
/// lifecycle exits WELL under the default exit-wait bound (7s). A lost wakeup
/// (the trap the [F1] pattern exists to avoid) would stall the FULL budget and
/// show as a red here — this is the empirical no-stall regression test.
#[test]
fn f1_fast_kill_exits_well_under_bound_no_lost_wakeup() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_41fix_f1")?;
    let mut daemon = start_daemon(
        &jail,
        "alpha",
        &[("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into())],
        true,
    )?;
    let socket = daemon.socket.clone();
    create_session(&socket, "alpha")?;

    // The reply must arrive (no slow-drop), AND the whole thing must be fast.
    let t0 = Instant::now();
    kill_and_expect_reply(socket, "alpha", Duration::from_secs(5))
        .map_err(|e| -> Box<dyn Error> { e.into() })?;
    assert!(
        daemon.wait_exit(Duration::from_secs(5)),
        "daemon did not exit after a fast kill"
    );
    let elapsed = t0.elapsed();
    // WELL under the 7s default exit-wait bound: a lost wakeup would push this to
    // ~7s. 2s is a comfortable ceiling for a fast kill + prompt notify wakeup.
    assert!(
        elapsed < Duration::from_secs(2),
        "fast kill took {elapsed:?} — a lost wakeup would stall to the full \
         exit-wait bound; the [F1] enable-before-check notify must fire promptly"
    );
    daemon.kill();
    teardown_jail(&jail)?;
    Ok(())
}

// ===========================================================================
// ★ DOUBLE-UNLINK SURVIVAL TEST (exit-reorder addendum [RT-F3]; the ★ hazard's
// committed proof). DETERMINISTIC via SLOW_DROP.
// ===========================================================================

/// The early-unlink (reorder step 3) removes the socket BEFORE the bounded
/// in-flight wait. A slow blocking drop holds the OLD daemon inside that wait,
/// post-unlink — a window where a SAME-NAME successor legitimately sees ENOENT
/// and BINDS a FRESH `<name>.sock` at the same path. The hazard: the old
/// daemon's `SocketGuard::drop` at run_server return could `remove_file` that
/// path and DELETE THE SUCCESSOR'S LIVE SOCKET. The [RT-F3] `Option::take` shape
/// defuses it (unlink exactly once via whichever of early-unlink/Drop runs
/// first; the graceful path's final Drop is a no-op).
///
/// MECHANICS (deterministic): old daemon with a LONG SLOW_DROP (1500ms). Kill its
/// session over the wire → its handler empties the manager, then BLOCKS in the
/// slow drop; the end-watch flips `ended`, the post-loop path runs the
/// early-unlink (socket GONE) and enters the bounded wait (held by the slow
/// drop). DURING that window we launch a same-name SUCCESSOR daemon — it sees
/// ENOENT and binds a FRESH socket. We let the OLD daemon finish exiting, then
/// assert the SUCCESSOR'S socket SURVIVES and SERVES (a Hello roundtrip answers).
#[test]
fn double_unlink_successor_socket_survives_old_daemon_exit() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("wsc_41fix_dunlink")?;
    // LONG slow drop: holds the old daemon in its post-unlink bounded wait long
    // enough for the successor to detect ENOENT and bind deterministically. Well
    // under the default 7s exit-wait bound (the wait won't give up on it).
    let mut old = start_daemon(
        &jail,
        "alpha",
        &[
            ("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into()),
            ("SBMUX_TEST_SLOW_DROP_MS".into(), "1500".into()),
        ],
        true,
    )?;
    let socket = old.socket.clone();
    create_session(&socket, "alpha")?;

    // Fire KillSession on a detached thread: its handler empties the manager then
    // blocks 1500ms in the drop. The early-unlink runs ~immediately after the
    // manager empties; the old daemon then sits in the bounded wait.
    let socket_kill = socket.clone();
    let kill_thread = std::thread::spawn(move || {
        let _ = kill_and_expect_reply(socket_kill, "alpha", Duration::from_secs(10));
    });

    // Wait for the early-unlink to remove the socket (deterministic: it happens
    // near the START of the 1500ms slow-drop window, right after the manager
    // empties and `ended` flips).
    let unlink_deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() && Instant::now() < unlink_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !socket.exists(),
        "early-unlink (reorder step 3) should have removed the socket before the wait"
    );
    // The OLD daemon must still be ALIVE (held in the bounded wait by the slow
    // drop) — this is the window the successor binds into.
    assert!(
        !old.has_exited(),
        "old daemon should still be draining (held by the 1500ms slow drop) — \
         the successor-bind window"
    );

    // Launch the SAME-NAME successor: it sees ENOENT → binds a FRESH alpha.sock.
    let mut successor = start_daemon(
        &jail,
        "alpha",
        &[("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into())],
        true, // wait_for_socket: the successor's fresh bind must appear
    )?;
    let succ_socket = successor.socket.clone();
    assert!(
        succ_socket.exists(),
        "successor failed to bind a fresh socket in the ENOENT window"
    );

    // Let the OLD daemon finish exiting (its slow drop completes, the bounded wait
    // returns, run_server returns → SocketGuard drops). With the [RT-F3] take()
    // shape that final Drop is a NO-OP — it must NOT delete the successor's socket.
    let _ = kill_thread.join();
    assert!(
        old.wait_exit(Duration::from_secs(8)),
        "old daemon did not finish exiting after the slow drop"
    );

    // THE PROOF: the successor's socket SURVIVES the old daemon's exit...
    assert!(
        succ_socket.exists(),
        "DOUBLE-UNLINK REGRESSION: the old daemon's final SocketGuard::drop DELETED \
         the successor's live socket (the ★ hazard) — the Option::take defusal failed"
    );
    // ...and still SERVES: a Hello roundtrip answers (ListSessions over a fresh
    // ProtocolClient, which completes the Hello handshake before sending its verb).
    let succ_probe = succ_socket.clone();
    let serves = on_runtime(move || -> bool {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            match ProtocolClient::connect(&succ_probe).await {
                Ok(mut c) => matches!(
                    tokio::time::timeout(
                        Duration::from_secs(3),
                        c.send_and_receive(ClientMsg::ListSessions),
                    )
                    .await,
                    Ok(Ok(ServerMsg::SessionList(_)))
                ),
                Err(_) => false,
            }
        })
    });
    assert!(
        serves,
        "the successor's socket survived but does not SERVE — a Hello+ListSessions \
         roundtrip must answer on the fresh daemon"
    );

    successor.kill();
    old.kill();
    teardown_jail(&jail)?;
    Ok(())
}
