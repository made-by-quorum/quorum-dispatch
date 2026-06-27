//! ACK-1 integration rows (ack1-spec §6) — daemon event stream + fault seams.
//!
//! Every row keys on the EVENTS FILE (and where named, captured daemon stderr
//! or app output) — never on PTY echo (ADD-6). Mutation controls R-MUT m1/m2
//! prove the rows have teeth (suppressed emitter / tampered sha must RED);
//! the unarmed-fault negative controls live INSIDE each fault row (a
//! non-matching session/sha must behave normally on the same daemon).

#[path = "lib/mod.rs"]
#[allow(dead_code, unused_imports)]
mod libmod;
use libmod::*;

use qrmux::events::{parse_line, CloseReason, DaemonEvent};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ============================================================================
// Shared helpers
// ============================================================================

/// Evidence directory (same convention as integration_tests.rs).
fn evidence_dir(scenario: &str) -> PathBuf {
    let runid = std::env::var("QRMUX_GATE_RUNID").unwrap_or_else(|_| "dev".to_string());
    let dir = PathBuf::from("target/test-evidence")
        .join(runid)
        .join(scenario);
    fs::create_dir_all(&dir).expect("create evidence dir");
    dir
}

/// `<socket-dir>/events/<session>.daemon.<epoch>.jsonl` for a jail.
fn events_file(jail: &libmod::jail::Jail, session: &str, epoch: u64) -> PathBuf {
    jail.socket_dir
        .join("events")
        .join(format!("{session}.daemon.{epoch}.jsonl"))
}

/// Read + parse an events file (typed; unknown lines skipped per the
/// forward-compat rule).
fn read_events(path: &Path) -> Result<Vec<DaemonEvent>, Box<dyn Error>> {
    let content = fs::read_to_string(path)?;
    Ok(content.lines().filter_map(parse_line).collect())
}

fn meta_of(ev: &DaemonEvent) -> &qrmux::events::EventMeta {
    match ev {
        DaemonEvent::SessionOpened { meta, .. }
        | DaemonEvent::PtyBytesWritten { meta, .. }
        | DaemonEvent::PtyWriteFailed { meta, .. }
        | DaemonEvent::SessionClosed { meta, .. }
        | DaemonEvent::EventsTruncated { meta, .. }
        | DaemonEvent::Heartbeat { meta } => meta,
    }
}

/// sha256 lowercase hex of a string's bytes (the event join key).
fn sha_hex(s: &str) -> String {
    sha256(s.as_bytes())
}

/// Poll a fresh-attach capture of `session` until `pred(text)` holds or
/// timeout (the integration_tests.rs wait_for_text pattern; app output only,
/// never echo — ADD-6).
fn wait_for_session_text(
    socket: &Path,
    session: &str,
    timeout: Duration,
    pred: impl Fn(&str) -> bool,
) -> Result<String, Box<dyn Error>> {
    let start = Instant::now();
    loop {
        let cap = capture_session(socket, session, 150)?;
        let text = cap.text();
        if pred(&text) {
            return Ok(text);
        }
        if start.elapsed() > timeout {
            let tail: String = text.chars().skip(text.len().saturating_sub(300)).collect();
            return Err(format!(
                "timed out after {timeout:?} waiting for session text (tail: {tail:?})"
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Poll until `pred(events)` holds on the parsed file or timeout.
fn wait_for_events(
    path: &Path,
    timeout: Duration,
    pred: impl Fn(&[DaemonEvent]) -> bool,
) -> Result<Vec<DaemonEvent>, Box<dyn Error>> {
    let start = Instant::now();
    loop {
        if path.exists() {
            let evs = read_events(path)?;
            if pred(&evs) {
                return Ok(evs);
            }
        }
        if start.elapsed() > timeout {
            let got = if path.exists() {
                format!("{:?}", read_events(path)?)
            } else {
                "<no file>".to_string()
            };
            return Err(format!(
                "timed out after {timeout:?} waiting for events predicate; got: {got}"
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Kill a session via the binary's `kill` verb (drives the C1 close site).
fn kill_session_via_cli(env: &[(String, String)], session: &str) -> Result<(), Box<dyn Error>> {
    let mut cmd_env: Vec<(String, String)> = env.to_vec();
    if !cmd_env.iter().any(|(k, _)| k == "PATH") {
        cmd_env.push(("PATH".into(), "/usr/bin:/bin".into()));
    }
    let status = std::process::Command::new(libmod::client::qrmux_binary())
        .arg("kill")
        .arg(session)
        .env_clear()
        .envs(cmd_env)
        .status()?;
    if !status.success() {
        return Err(format!("qrmux kill '{session}' failed: {status}").into());
    }
    Ok(())
}

/// The R-LIVE assertion body, Result-shaped so the R-MUT mutation controls can
/// assert it FAILS (ack1-spec §6: the meta-test proves the rows have teeth).
fn live_row_assertions(
    events_path: &Path,
    session: &str,
    sent: &str,
) -> Result<Vec<DaemonEvent>, String> {
    if !events_path.exists() {
        return Err(format!("events file missing: {}", events_path.display()));
    }
    let evs = read_events(events_path).map_err(|e| e.to_string())?;
    // Bookends + content row.
    match evs.first() {
        Some(DaemonEvent::SessionOpened {
            meta,
            pid,
            schema_version,
            ..
        }) => {
            if meta.seq != 1 {
                return Err(format!("session-opened seq must be 1, got {}", meta.seq));
            }
            if *pid == 0 {
                return Err("session-opened pid must be nonzero (captured at spawn)".into());
            }
            if *schema_version != qrmux::events::DAEMON_EVENTS_SCHEMA_VERSION {
                return Err(format!("schema_version mismatch: {schema_version}"));
            }
            if meta.session != session {
                return Err(format!("session field mismatch: {}", meta.session));
            }
        }
        other => {
            return Err(format!(
                "first record must be session-opened, got {other:?}"
            ))
        }
    }
    check_bytes_written(&evs, sent)?;
    let closed_killed = evs.iter().any(|e| {
        matches!(
            e,
            DaemonEvent::SessionClosed {
                reason: CloseReason::Killed,
                ..
            }
        )
    });
    if !closed_killed {
        return Err("missing session-closed(killed) bookend".into());
    }
    // Pre-cap file: seq gapless 1..n, strictly increasing.
    let seqs: Vec<u64> = evs.iter().map(|e| meta_of(e).seq).collect();
    for (i, s) in seqs.iter().enumerate() {
        if *s != (i as u64) + 1 {
            return Err(format!("seq not gapless: {seqs:?}"));
        }
    }
    // Default-off heartbeat: none in this file.
    if evs
        .iter()
        .any(|e| matches!(e, DaemonEvent::Heartbeat { .. }))
    {
        return Err("heartbeat records present with the knob off".into());
    }
    Ok(evs)
}

/// Content check used by R-LIVE and (against a tampered file) R-MUT m2:
/// a pty-bytes-written record must exist whose sha AND len match `sent`.
fn check_bytes_written(evs: &[DaemonEvent], sent: &str) -> Result<(), String> {
    let want_sha = sha_hex(sent);
    let want_len = sent.len() as u64;
    let hit = evs.iter().any(|e| {
        matches!(e, DaemonEvent::PtyBytesWritten { bytes, content_sha256, content_len, .. }
            if *content_sha256 == want_sha && *content_len == want_len && *bytes == want_len)
    });
    if hit {
        Ok(())
    } else {
        Err(format!(
            "no pty-bytes-written with sha {want_sha} len {want_len}"
        ))
    }
}

// ============================================================================
// R-LIVE — open→send→kill happy path (also the fault rows' baseline control)
// ============================================================================

/// R-LIVE: session-opened(seq1, pid≠0, schema_version) → pty-bytes-written
/// (sha/len recomputed in-test) → session-closed(killed); seq gapless; no
/// heartbeat (knob off). The events file lives under the SAME resolved socket
/// dir as qrmux.sock (<jail>/xdg_runtime/qrmux/events/...).
#[test]
fn r_live_open_send_kill() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_live")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "al1")?;
    let session = "al1";
    create_session(&socket, session)?;

    let sent = "echo ACK1''LIVE_ROW\n";
    send_to_session(&socket, &env, session, sent)?;
    kill_session_via_cli(&env, session)?;

    let path = events_file(&jail, session, 1);
    let evs = wait_for_events(&path, Duration::from_secs(10), |evs| {
        evs.iter()
            .any(|e| matches!(e, DaemonEvent::SessionClosed { .. }))
    })?;
    live_row_assertions(&path, session, sent).map_err(|e| -> Box<dyn Error> { e.into() })?;

    let ev = evidence_dir("ack1_r_live");
    fs::copy(&path, ev.join("events.jsonl"))?;
    fs::write(
        ev.join("result.txt"),
        format!(
            "R-LIVE PASS\nrecords={}\nfile={}\n",
            evs.len(),
            path.display()
        ),
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R-FENCE — epoch fencing across abrupt daemon respawn
// ============================================================================

/// R-FENCE: SIGKILL the daemon (NO close bookend — at-most-once honesty),
/// respawn on the same socket dir, recreate the same session name → the new
/// owner opens `.daemon.2.jsonl` (seq restarts at 1) and the predecessor's
/// `.daemon.1.jsonl` is byte-identical (the never-append fence).
#[test]
fn r_fence_epoch_across_daemon_respawn() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_fence")?;
    let env = jail_env(&jail);
    let session = "af1";
    let path1 = events_file(&jail, session, 1);
    let path2 = events_file(&jail, session, 2);

    // Daemon A: open + one write. SIGKILL it (bypassing every close site).
    let pid_a = {
        let (daemon_a, socket) = start_daemon_in_jail(&jail, &env, "af1")?;
        create_session(&socket, session)?;
        send_to_session(&socket, &env, session, "echo FENCE''ROW_A\n")?;
        wait_for_events(&path1, Duration::from_secs(10), |evs| {
            evs.iter()
                .any(|e| matches!(e, DaemonEvent::PtyBytesWritten { .. }))
        })?;
        let pid = daemon_a.pid;
        let _ = std::process::Command::new("/bin/kill")
            .arg("-9")
            .arg(pid.to_string())
            .status()?;
        std::mem::forget(daemon_a); // already SIGKILLed; skip the guard's TERM
        pid
    };
    // Wait until the daemon process is gone.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let alive = std::process::Command::new("/bin/kill")
            .arg("-0")
            .arg(pid_a.to_string())
            .status()?
            .success();
        if !alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let bytes1_before = fs::read(&path1)?;
    // SIGKILL means NO close bookend — assert its absence (honesty row).
    let evs1 = read_events(&path1)?;
    assert!(
        !evs1
            .iter()
            .any(|e| matches!(e, DaemonEvent::SessionClosed { .. })),
        "SIGKILL must leave no close bookend (at-most-once): {evs1:?}"
    );

    // Daemon B on the SAME socket dir; same session name → next epoch.
    let (_daemon_b, socket_b) = start_daemon_in_jail(&jail, &env, "af1")?;
    create_session(&socket_b, session)?;
    let evs2 = wait_for_events(&path2, Duration::from_secs(10), |evs| !evs.is_empty())?;
    match evs2.first() {
        Some(DaemonEvent::SessionOpened { meta, .. }) => {
            assert_eq!(meta.epoch, 2, "new owner must open epoch 2");
            assert_eq!(meta.seq, 1, "seq restarts per epoch file");
        }
        other => panic!("epoch-2 file must start with session-opened, got {other:?}"),
    }
    // The fence: epoch-1 file untouched by the new owner.
    let bytes1_after = fs::read(&path1)?;
    assert_eq!(
        bytes1_before, bytes1_after,
        "predecessor epoch file must be byte-identical (never-append fence)"
    );

    let ev = evidence_dir("ack1_r_fence");
    fs::copy(&path1, ev.join("epoch1.jsonl"))?;
    fs::copy(&path2, ev.join("epoch2.jsonl"))?;
    fs::write(
        ev.join("result.txt"),
        "R-FENCE PASS\nepoch1 byte-identical across respawn; epoch2 opened seq1; no close bookend after SIGKILL\n",
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R-REAP / R-SHUT — the other two close reasons
// ============================================================================

/// R-REAP: a detached session whose child exits → cleanup task emits
/// session-closed(child-exited). Interval lowered via QRMUX_CLEANUP_INTERVAL_MS
/// (evidences the reap→emit path, not the 30s production regime — spec §2 C2).
#[test]
fn r_reap_child_exited() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_reap")?;
    let mut env = jail_env(&jail);
    env.push(("QRMUX_CLEANUP_INTERVAL_MS".into(), "300".into()));
    let (_daemon, _socket) = start_daemon_in_jail(&jail, &env, "ar1")?;
    let session = "ar1";

    let rt = tokio::runtime::Runtime::new()?;
    let cwd = std::fs::canonicalize(&jail.jail_root)?;
    rt.block_on(qrmux::client::session_client::create_detached_session(
        Some(jail.socket_dir.as_path()),
        None,
        session,
        "true", // exits immediately
        cwd,
        100,
    ))?;

    let path = events_file(&jail, session, 1);
    let evs = wait_for_events(&path, Duration::from_secs(15), |evs| {
        evs.iter().any(|e| {
            matches!(
                e,
                DaemonEvent::SessionClosed {
                    reason: CloseReason::ChildExited,
                    ..
                }
            )
        })
    })?;

    let ev = evidence_dir("ack1_r_reap");
    fs::copy(&path, ev.join("events.jsonl"))?;
    fs::write(
        ev.join("result.txt"),
        format!("R-REAP PASS\nrecords={}\n", evs.len()),
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

/// R-SHUT: graceful SIGTERM → session-closed(daemon-shutdown) for each live
/// session, then the daemon exits (socket removed).
#[test]
fn r_shut_daemon_shutdown() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_shut")?;
    let env = jail_env(&jail);
    let (daemon, socket) = start_daemon_in_jail(&jail, &env, "as1")?;
    let session = "as1";
    create_session(&socket, session)?;
    let path = events_file(&jail, session, 1);
    wait_for_events(&path, Duration::from_secs(10), |evs| !evs.is_empty())?;

    let _ = std::process::Command::new("/bin/kill")
        .arg("-TERM")
        .arg(daemon.pid.to_string())
        .status()?;
    // Graceful exit removes the socket file (SocketGuard).
    let start = Instant::now();
    while socket.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }

    let evs = wait_for_events(&path, Duration::from_secs(10), |evs| {
        evs.iter().any(|e| {
            matches!(
                e,
                DaemonEvent::SessionClosed {
                    reason: CloseReason::DaemonShutdown,
                    ..
                }
            )
        })
    })?;

    let ev = evidence_dir("ack1_r_shut");
    fs::copy(&path, ev.join("events.jsonl"))?;
    fs::write(
        ev.join("result.txt"),
        format!("R-SHUT PASS\nrecords={}\n", evs.len()),
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R-ROT — size cap, marker, terminal reserve, seq-gap drop signal
// ============================================================================

/// R-ROT: tiny cap → exactly one events-truncated; non-terminal records
/// suppressed after it (but still consuming seq → in-file gap = the positive
/// drop signal); session-closed still admitted (terminal reserve).
#[test]
fn r_rot_cap_truncation() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_rot")?;
    let mut env = jail_env(&jail);
    env.push(("QRMUX_EVENTS_MAX_BYTES".into(), "600".into()));
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "ao1")?;
    let session = "ao1";
    create_session(&socket, session)?;

    for i in 0..5 {
        send_to_session(&socket, &env, session, &format!("echo ROT''ROW_{i}\n"))?;
    }
    kill_session_via_cli(&env, session)?;

    let path = events_file(&jail, session, 1);
    let evs = wait_for_events(&path, Duration::from_secs(10), |evs| {
        evs.iter()
            .any(|e| matches!(e, DaemonEvent::SessionClosed { .. }))
    })?;

    let marker_count = evs
        .iter()
        .filter(|e| matches!(e, DaemonEvent::EventsTruncated { .. }))
        .count();
    assert_eq!(
        marker_count, 1,
        "exactly one events-truncated marker: {evs:?}"
    );
    let marker_idx = evs
        .iter()
        .position(|e| matches!(e, DaemonEvent::EventsTruncated { .. }))
        .unwrap();
    assert!(
        evs[marker_idx + 1..].iter().all(|e| matches!(
            e,
            DaemonEvent::SessionClosed { .. } | DaemonEvent::EventsTruncated { .. }
        )),
        "only terminal-class records after the marker: {evs:?}"
    );
    assert!(
        evs.iter().any(|e| matches!(
            e,
            DaemonEvent::SessionClosed {
                reason: CloseReason::Killed,
                ..
            }
        )),
        "close bookend must survive rotation (terminal reserve): {evs:?}"
    );
    // Seq-gap drop signal: suppressed records consumed seq.
    let seqs: Vec<u64> = evs.iter().map(|e| meta_of(e).seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "in-file seq strictly increasing: {seqs:?}"
    );
    assert!(
        *seqs.last().unwrap() as usize > evs.len(),
        "suppressed records must leave a seq gap: max {} vs {} records",
        seqs.last().unwrap(),
        evs.len()
    );

    let ev = evidence_dir("ack1_r_rot");
    fs::copy(&path, ev.join("events.jsonl"))?;
    fs::write(
        ev.join("result.txt"),
        format!(
            "R-ROT PASS\ncap=600 marker=1 records={} max_seq={}\n",
            evs.len(),
            seqs.last().unwrap()
        ),
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R-CHUNK — one record per SendInput frame, seq order == send order
// ============================================================================

/// R-CHUNK: two frames → two pty-bytes-written with per-frame shas in send
/// order (the per-chunk join shape ACK-2's send-initiated.chunk_sha256s keys on).
#[test]
fn r_chunk_two_frames() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_chunk")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "ac1")?;
    let session = "ac1";
    create_session(&socket, session)?;

    let chunk1 = "echo CHU''NK_ONE\n";
    let chunk2 = "echo CHU''NK_TWO\n";
    send_to_session(&socket, &env, session, chunk1)?;
    send_to_session(&socket, &env, session, chunk2)?;
    kill_session_via_cli(&env, session)?;

    let path = events_file(&jail, session, 1);
    let evs = wait_for_events(&path, Duration::from_secs(10), |evs| {
        evs.iter()
            .any(|e| matches!(e, DaemonEvent::SessionClosed { .. }))
    })?;

    let writes: Vec<(&str, u64)> = evs
        .iter()
        .filter_map(|e| match e {
            DaemonEvent::PtyBytesWritten {
                meta,
                content_sha256,
                ..
            } => Some((content_sha256.as_str(), meta.seq)),
            _ => None,
        })
        .collect();
    assert_eq!(writes.len(), 2, "one record per frame: {evs:?}");
    assert_eq!(writes[0].0, sha_hex(chunk1), "frame 1 sha");
    assert_eq!(writes[1].0, sha_hex(chunk2), "frame 2 sha");
    assert!(writes[0].1 < writes[1].1, "seq order == send order");

    let ev = evidence_dir("ack1_r_chunk");
    fs::copy(&path, ev.join("events.jsonl"))?;
    fs::write(ev.join("result.txt"), "R-CHUNK PASS\n")?;
    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R-HB — heartbeat knob on (default-off half is asserted inside R-LIVE)
// ============================================================================

/// R-HB: QRMUX_EVENTS_HEARTBEAT_MS=100 → heartbeat records appear, seq
/// interleaves monotonically with the rest of the stream.
#[test]
fn r_hb_heartbeat_knob_on() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_hb")?;
    let mut env = jail_env(&jail);
    env.push(("QRMUX_EVENTS_HEARTBEAT_MS".into(), "100".into()));
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "ah1")?;
    let session = "ah1";
    create_session(&socket, session)?;

    let path = events_file(&jail, session, 1);
    let evs = wait_for_events(&path, Duration::from_secs(10), |evs| {
        evs.iter()
            .filter(|e| matches!(e, DaemonEvent::Heartbeat { .. }))
            .count()
            >= 2
    })?;
    let seqs: Vec<u64> = evs.iter().map(|e| meta_of(e).seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "heartbeat seq interleaves monotonically: {seqs:?}"
    );

    kill_session_via_cli(&env, session)?;
    let ev = evidence_dir("ack1_r_hb");
    fs::copy(&path, ev.join("events.jsonl"))?;
    fs::write(ev.join("result.txt"), "R-HB PASS\n")?;
    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R-F1/F2/F3 — daemon fault rows (each with an in-row negative control)
// ============================================================================

/// R-F1 (injection 2): QRMUX_FAULT_PTY_WRITE=error on session s →
/// ServerMsg::Error at the client AND pty-write-failed{errno, sha, len}.
/// In-row negative control: a NON-matching session on the same daemon writes
/// normally (filter precision = the unarmed identity for that session).
#[test]
fn r_f1_fault_error() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_f1")?;
    let mut env = jail_env(&jail);
    env.push(("QRMUX_FAULT_PTY_WRITE".into(), "error".into()));
    env.push(("QRMUX_FAULT_SESSION".into(), "af_target".into()));
    // WS-C M3b: per-session split — each session gets its OWN daemon. Both
    // daemons inherit the SAME fault env (QRMUX_FAULT_SESSION=af_target), so the
    // fault-session gate is what's under test: it bites ONLY the daemon whose
    // --session matches (af_target), and the af_control daemon flows normally.
    // The filter-precision negative control is preserved; it now spans two
    // per-session daemons instead of two sessions on one daemon.
    let (_daemon_t, socket_t) = start_daemon_in_jail(&jail, &env, "af_target")?;
    let (_daemon_c, socket_c) = start_daemon_in_jail(&jail, &env, "af_control")?;
    create_session(&socket_t, "af_target")?;
    create_session(&socket_c, "af_control")?;

    let sent = "echo FAULT''ERR_ROW\n";
    // Faulted session: client sees the server error.
    let res = send_to_session(&socket_t, &env, "af_target", sent);
    assert!(
        res.is_err() && format!("{}", res.unwrap_err()).contains("failed to write input"),
        "matching session must surface the write error"
    );
    let path = events_file(&jail, "af_target", 1);
    let evs = wait_for_events(&path, Duration::from_secs(10), |evs| {
        evs.iter()
            .any(|e| matches!(e, DaemonEvent::PtyWriteFailed { .. }))
    })?;
    let hit = evs.iter().any(|e| {
        matches!(
            e,
            DaemonEvent::PtyWriteFailed { errno: Some(5), content_sha256, content_len, .. }
                if *content_sha256 == sha_hex(sent) && *content_len == sent.len() as u64
        )
    });
    assert!(
        hit,
        "pty-write-failed must carry errno 5 + join keys: {evs:?}"
    );

    // Negative control: non-matching session writes fine on the SAME daemon.
    send_to_session(&socket_c, &env, "af_control", sent)?;
    let ctl = wait_for_events(
        &events_file(&jail, "af_control", 1),
        Duration::from_secs(10),
        |evs| {
            evs.iter()
                .any(|e| matches!(e, DaemonEvent::PtyBytesWritten { .. }))
        },
    )?;
    assert!(
        !ctl.iter()
            .any(|e| matches!(e, DaemonEvent::PtyWriteFailed { .. })),
        "control session must not be faulted: {ctl:?}"
    );

    let ev = evidence_dir("ack1_r_f1");
    fs::copy(&path, ev.join("events_target.jsonl"))?;
    fs::write(
        ev.join("result.txt"),
        "R-F1 PASS (error mode + filter-precision negative control)\n",
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

/// R-F2 (injection 3): swallow mode keyed on MATCH_SHA256 — the daemon ACKS
/// and records pty-bytes-written, but the bytes never reach the PTY (the
/// deception the advisory declaration warns about, on purpose). The
/// (3)-vs-(1) discriminator is the DAEMON LOG (present here, absent in R-F3);
/// the child side merely corroborates (executed output absent).
#[test]
fn r_f2_fault_swallow() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_f2")?;
    let swallowed = "echo SWAL''LOW_X\n";
    let alive = "echo ALI''VE_MARK\n";
    let mut env = jail_env(&jail);
    env.push(("QRMUX_FAULT_PTY_WRITE".into(), "swallow".into()));
    env.push(("QRMUX_FAULT_MATCH_SHA256".into(), sha_hex(swallowed)));
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "aw1")?;
    let session = "aw1";
    create_session(&socket, session)?;

    // Matching content: ACK + event, no bytes to the PTY.
    send_to_session(&socket, &env, session, swallowed)?; // InputSent ack (Ok)
                                                         // Non-matching content on the SAME session: flows through (in-row negative
                                                         // control + proves the session is alive).
    send_to_session(&socket, &env, session, alive)?;

    // Child-side corroboration (executed output, never echo — ADD-6): the
    // alive marker lands; the swallowed one never does.
    let text = wait_for_session_text(&socket, session, Duration::from_secs(30), |t| {
        t.contains("ALIVE_MARK")
    })?;
    assert!(
        !text.contains("SWALLOW_X"),
        "swallowed bytes must never reach the child (capture: {} chars)",
        text.len()
    );

    let path = events_file(&jail, session, 1);
    let evs = read_events(&path)?;
    // The daemon-log discriminator: BOTH writes recorded as pty-bytes-written.
    check_bytes_written(&evs, swallowed).map_err(|e| -> Box<dyn Error> { e.into() })?;
    check_bytes_written(&evs, alive).map_err(|e| -> Box<dyn Error> { e.into() })?;

    let ev = evidence_dir("ack1_r_f2");
    fs::copy(&path, ev.join("events.jsonl"))?;
    fs::write(ev.join("capture.txt"), &text)?;
    fs::write(
        ev.join("result.txt"),
        "R-F2 PASS (swallow recorded-as-written; child never saw it; sha filter precise)\n",
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

/// R-F3 (injection 1): drop-frame parks the connection open — NO reply within
/// the bounded wait (silence, not close), NO event for that sha. In-row
/// negative control: a non-matching session sends fine on the same daemon.
#[test]
fn r_f3_fault_dropframe() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_f3")?;
    let mut env = jail_env(&jail);
    env.push(("QRMUX_FAULT_DROP_FRAMES".into(), "send-input".into()));
    env.push(("QRMUX_FAULT_SESSION".into(), "ad_target".into()));
    // WS-C M3b: per-session split — two per-session daemons sharing the SAME
    // fault env (QRMUX_FAULT_SESSION=ad_target). The drop-frame fault bites only
    // the daemon whose --session matches; the ad_control daemon flows. Same
    // filter-precision negative control, now across two daemons.
    let (daemon, socket) = start_daemon_in_jail(&jail, &env, "ad_target")?;
    let (_daemon_c, socket_ctl) = start_daemon_in_jail(&jail, &env, "ad_control")?;
    create_session(&socket, "ad_target")?;
    create_session(&socket_ctl, "ad_control")?;

    let dropped = "echo DROP''PED_ROW\n";
    // Bounded-wait recipe (spec §4.1): the send must produce NO reply of any
    // kind within 2s (the test client has no read timeout — recv_timeout is
    // the bound; daemon teardown unblocks the parked thread afterwards).
    let (tx, rx) = std::sync::mpsc::channel();
    let socket_c = socket.clone();
    let env_c = env.clone();
    let dropped_c = dropped.to_string();
    let sender = std::thread::spawn(move || {
        let res = send_to_session(&socket_c, &env_c, "ad_target", &dropped_c);
        let _ = tx.send(res.map_err(|e| e.to_string()));
    });
    match rx.recv_timeout(Duration::from_secs(2)) {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {} // SILENCE — the row's signature
        Ok(r) => panic!("dropped frame must produce NO reply, got {r:?}"),
        Err(e) => panic!("channel error: {e}"),
    }

    // No event for that sha (absence keyed on sha, not count).
    let path = events_file(&jail, "ad_target", 1);
    let evs = read_events(&path)?;
    assert!(
        !evs.iter().any(|e| matches!(
            e,
            DaemonEvent::PtyBytesWritten { content_sha256, .. } if *content_sha256 == sha_hex(dropped)
        )),
        "dropped frame must leave no event: {evs:?}"
    );

    // Negative control: non-matching session on the same daemon flows.
    send_to_session(&socket_ctl, &env, "ad_control", dropped)?;
    wait_for_events(
        &events_file(&jail, "ad_control", 1),
        Duration::from_secs(10),
        |evs| {
            evs.iter()
                .any(|e| matches!(e, DaemonEvent::PtyBytesWritten { .. }))
        },
    )?;

    // Unblock the parked sender: kill the daemon, then join.
    drop(daemon);
    let _ = sender.join();

    let ev = evidence_dir("ack1_r_f3");
    fs::copy(&path, ev.join("events_target.jsonl"))?;
    fs::write(
        ev.join("result.txt"),
        "R-F3 PASS (silence within 2s bound; no event for dropped sha; control session flows)\n",
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R-ARM (rider R-a) — the loud-arm warn is an executable assertion
// ============================================================================

/// R-ARM: "FAULT INJECTION ARMED" present in captured daemon stderr when any
/// fault env is set; ABSENT when not (same capture mechanism both arms).
#[test]
fn r_arm_loud_warn_on_stderr() -> Result<(), Box<dyn Error>> {
    // Armed daemon.
    let jail_a = setup_jail("ack1_r_arm_on")?;
    let stderr_a = jail_a.jail_root.join("daemon_stderr.log");
    let mut env_a = jail_env(&jail_a);
    env_a.push(("QRMUX_FAULT_PTY_WRITE".into(), "swallow".into()));
    {
        let (_d, _s) = libmod::client::start_daemon_in_jail_with_stderr(
            &jail_a,
            &env_a,
            "arm-a",
            Some(&stderr_a),
        )?;
        // The warn fires at startup; the socket existing means startup ran.
    }
    let log_a = fs::read_to_string(&stderr_a)?;
    assert!(
        log_a.contains("FAULT INJECTION ARMED"),
        "armed daemon must announce on stderr; got: {log_a}"
    );

    // Unarmed daemon: same capture, no announcement.
    let jail_b = setup_jail("ack1_r_arm_off")?;
    let stderr_b = jail_b.jail_root.join("daemon_stderr.log");
    let env_b = jail_env(&jail_b);
    {
        let (_d, _s) = libmod::client::start_daemon_in_jail_with_stderr(
            &jail_b,
            &env_b,
            "arm-b",
            Some(&stderr_b),
        )?;
    }
    let log_b = fs::read_to_string(&stderr_b)?;
    assert!(
        !log_b.contains("FAULT INJECTION ARMED"),
        "unarmed daemon must NOT announce; got: {log_b}"
    );

    let ev = evidence_dir("ack1_r_arm");
    fs::write(ev.join("armed_stderr.log"), &log_a)?;
    fs::write(ev.join("unarmed_stderr.log"), &log_b)?;
    fs::write(ev.join("result.txt"), "R-ARM PASS (rider R-a)\n")?;
    teardown_jail(&jail_a)?;
    teardown_jail(&jail_b)?;
    Ok(())
}

// ============================================================================
// R-PRIV — no payload text in the events file OR daemon stderr
// ============================================================================

/// R-PRIV: the events file carries sha+len ONLY — the raw payload appears
/// neither there nor in the daemon's captured stderr (rev C §2.2 Pete-visible
/// row; the sha IS present, stated confirmation-oracle framing unchanged).
#[test]
fn r_priv_no_payload_text() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_priv")?;
    let stderr_path = jail.jail_root.join("daemon_stderr.log");
    let env = jail_env(&jail);
    let (_daemon, socket) =
        libmod::client::start_daemon_in_jail_with_stderr(&jail, &env, "ap1", Some(&stderr_path))?;
    let session = "ap1";
    create_session(&socket, session)?;

    let secret = "echo PRIV''ATE_PAYLOAD_XYZZY\n";
    send_to_session(&socket, &env, session, secret)?;
    kill_session_via_cli(&env, session)?;

    let path = events_file(&jail, session, 1);
    wait_for_events(&path, Duration::from_secs(10), |evs| {
        evs.iter()
            .any(|e| matches!(e, DaemonEvent::SessionClosed { .. }))
    })?;

    let file_raw = fs::read_to_string(&path)?;
    assert!(
        !file_raw.contains("ATE_PAYLOAD_XYZZY") && !file_raw.contains("PRIV''"),
        "events file must not contain payload text"
    );
    assert!(
        file_raw.contains(&sha_hex(secret)),
        "events file must carry the content sha (join key)"
    );
    let stderr_raw = fs::read_to_string(&stderr_path).unwrap_or_default();
    assert!(
        !stderr_raw.contains("ATE_PAYLOAD_XYZZY"),
        "daemon stderr must not contain payload text"
    );

    let ev = evidence_dir("ack1_r_priv");
    fs::copy(&path, ev.join("events.jsonl"))?;
    fs::write(
        ev.join("result.txt"),
        "R-PRIV PASS (sha present; payload absent in file + stderr)\n",
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

/// C-3 (ACK-3 spec §7) — R-PRIV-armed: the privacy contract holds with a FAULT
/// ARMED, the case the unarmed R-PRIV row can't reach. Sibling of
/// [`r_priv_no_payload_text`]: same daemon-stderr capture, but the daemon runs
/// with `QRMUX_FAULT_PTY_WRITE=error` armed on the test session, so the send
/// trips the fault (the client sees the Error reply — that is part of the
/// point: the leak surface is widest on the failure path). Asserts on the
/// CAPTURED STDERR: (a) the loud-arm warn is present (proves the capture IS the
/// armed run); (b) the raw payload text is absent; (c) the FULL 64-char
/// lowercase-hex sha of the canary does NOT appear (a ≤16-char prefix would be
/// acceptable — a full 64-hex run in logs would gratuitously strengthen the
/// confirmation oracle). The EVENTS FILE, by contrast, DOES carry the full sha
/// (the file is the contract surface; stderr is the leak surface).
#[test]
fn r_priv_armed_fault_stderr_sha_prefix_only() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_priv_armed")?;
    let stderr_path = jail.jail_root.join("daemon_stderr.log");
    let session = "apa";
    let mut env = jail_env(&jail);
    // Arm the PTY-write-error fault on exactly the test session (writes to any
    // OTHER session pass the filter; the canary send to this one trips it).
    // No sha filter needed here: this raw-daemon harness has no boot-time
    // writes — the canary send is the only write to the armed session. (The
    // engine-driven e2e matrix DOES need MATCH_SHA256 — ack3-spec §2.)
    env.push(("QRMUX_FAULT_PTY_WRITE".into(), "error".into()));
    env.push(("QRMUX_FAULT_SESSION".into(), session.into()));
    // WS-C M3b merge-boundary catch-up (W-5, flagged to orc): the harness is
    // per-session now — the daemon serves exactly this arm's session identity.
    let (_daemon, socket) =
        libmod::client::start_daemon_in_jail_with_stderr(&jail, &env, session, Some(&stderr_path))?;
    create_session(&socket, session)?;

    // Distinctive canary payload (unique by construction so the sha is the
    // unambiguous join key).
    let canary = format!(
        "echo R-PRIV''-ARMED-CANARY-{}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let full_sha = sha_hex(&canary);
    assert_eq!(full_sha.len(), 64, "sha join key must be 64 lowercase hex");

    // The send trips the armed fault: the client sees the server write error.
    let res = send_to_session(&socket, &env, session, &canary);
    assert!(
        res.is_err() && format!("{}", res.unwrap_err()).contains("failed to write input"),
        "the armed session's send must surface the write error"
    );

    // The events file IS the contract surface: pty-write-failed carries the
    // FULL sha for the canary.
    let path = events_file(&jail, session, 1);
    let evs = wait_for_events(&path, Duration::from_secs(10), |evs| {
        evs.iter().any(|e| {
            matches!(
                e,
                DaemonEvent::PtyWriteFailed { content_sha256, .. } if *content_sha256 == full_sha
            )
        })
    })?;
    assert!(
        evs.iter().any(|e| matches!(
            e,
            DaemonEvent::PtyWriteFailed { content_sha256, .. } if *content_sha256 == full_sha
        )),
        "events file must carry pty-write-failed with the full canary sha: {evs:?}"
    );

    // Stderr IS the leak surface: armed-warn present; payload + full sha absent.
    let stderr_raw = fs::read_to_string(&stderr_path)?;
    assert!(
        stderr_raw.contains("FAULT INJECTION ARMED"),
        "captured stderr must be the ARMED run (loud-arm warn present); got: {stderr_raw}"
    );
    assert!(
        !stderr_raw.contains("ARMED-CANARY") && !stderr_raw.contains("R-PRIV''"),
        "daemon stderr must not contain the raw canary payload"
    );
    // The bar (spec §7(c), TIGHTENED per the ACK-3 merge-ruling C-2): a sha
    // PREFIX of ≤16 hex chars is fine in logs; anything longer gratuitously
    // strengthens the confirmation oracle. Prefixes are start-anchored, so
    // "no prefix longer than 16" == "the 17-char prefix never appears" (which
    // also subsumes the old full-64-hex check).
    assert!(
        !stderr_raw.contains(&full_sha[..17]),
        "daemon stderr must not contain a >16-char prefix of the canary sha \
         (checked the 17-char prefix {})",
        &full_sha[..17]
    );

    let ev = evidence_dir("ack1_r_priv_armed");
    fs::copy(&path, ev.join("events.jsonl"))?;
    fs::write(ev.join("armed_stderr.log"), &stderr_raw)?;
    fs::write(
        ev.join("result.txt"),
        "R-PRIV-armed PASS (armed-warn present; payload absent + sha prefix \u{2264}16 in stderr; full sha in file)\n",
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R-MUT — mutation controls (teeth): m1 wiring, m2 content
// ============================================================================

/// R-MUT m1: QRMUX_EVENTS_DISABLED=1 → the R-LIVE assertion body MUST FAIL
/// (suppressed emitter turns the rows red — the brief's named control).
#[test]
fn r_mut_m1_kill_switch_reds_live_row() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_mut1")?;
    let mut env = jail_env(&jail);
    env.push(("QRMUX_EVENTS_DISABLED".into(), "1".into()));
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "am1")?;
    let session = "am1";
    create_session(&socket, session)?;
    let sent = "echo MUT''ONE_ROW\n";
    send_to_session(&socket, &env, session, sent)?;
    kill_session_via_cli(&env, session)?;

    let path = events_file(&jail, session, 1);
    // Give any (buggy) emitter a moment to write, then assert the row REDs.
    std::thread::sleep(Duration::from_millis(500));
    let verdict = live_row_assertions(&path, session, sent);
    assert!(
        verdict.is_err(),
        "R-LIVE body must FAIL with the emitter suppressed, got {verdict:?}"
    );
    assert!(
        !path.exists(),
        "no events file may exist under the kill switch"
    );

    let ev = evidence_dir("ack1_r_mut1");
    fs::write(
        ev.join("result.txt"),
        format!(
            "R-MUT m1 PASS (live row REDs under kill switch: {:?})\n",
            verdict.unwrap_err()
        ),
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

/// R-MUT m2: after a GREEN run, flipping one hex char of the recorded sha must
/// make the content check FAIL — the sha comparison BINDS (rows cannot pass on
/// file-presence alone; red-team F8).
#[test]
fn r_mut_m2_sha_tamper_reds_content_check() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("ack1_r_mut2")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "am2")?;
    let session = "am2";
    create_session(&socket, session)?;
    let sent = "echo MUT''TWO_ROW\n";
    send_to_session(&socket, &env, session, sent)?;
    kill_session_via_cli(&env, session)?;

    let path = events_file(&jail, session, 1);
    wait_for_events(&path, Duration::from_secs(10), |evs| {
        evs.iter()
            .any(|e| matches!(e, DaemonEvent::SessionClosed { .. }))
    })?;
    // Green first.
    let evs = read_events(&path)?;
    check_bytes_written(&evs, sent).map_err(|e| -> Box<dyn Error> { e.into() })?;

    // Tamper: flip the first hex char of the recorded sha.
    let want = sha_hex(sent);
    let flipped = if want.starts_with('0') { "1" } else { "0" };
    let tampered_raw =
        fs::read_to_string(&path)?.replacen(&want, &format!("{flipped}{}", &want[1..]), 1);
    let tampered_path = jail.jail_root.join("tampered_events.jsonl");
    fs::write(&tampered_path, tampered_raw)?;

    let tampered = read_events(&tampered_path)?;
    let verdict = check_bytes_written(&tampered, sent);
    assert!(
        verdict.is_err(),
        "content check must FAIL against a tampered sha (the comparison binds)"
    );

    let ev = evidence_dir("ack1_r_mut2");
    fs::write(
        ev.join("result.txt"),
        "R-MUT m2 PASS (sha tamper REDs the content check)\n",
    )?;
    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R-DIR (integration form) — emitter failure must not fail the daemon op
// ============================================================================

/// R-DIR: an unwritable events dir (created 0o000 before daemon start) means
/// sessions run UNLOGGED — but SendInput still succeeds with InputSent (the
/// advisory stream never holds the PTY contract hostage).
#[test]
fn r_dir_emitter_failure_isolated() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    let jail = setup_jail("ack1_r_dir")?;
    let env = jail_env(&jail);
    // Pre-create the events dir unwritable. The daemon must boot, the session
    // must work, the send must ack.
    let events_dir = jail.socket_dir.join("events");
    fs::create_dir_all(&events_dir)?;
    fs::set_permissions(&events_dir, fs::Permissions::from_mode(0o000))?;

    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "ad1")?;
    let session = "ad1";
    create_session(&socket, session)?;
    send_to_session(&socket, &env, session, "echo DIR''ROW_OK\n")?; // must Ok

    // Executed output proves the write reached the child despite no logging.
    let text = wait_for_session_text(&socket, session, Duration::from_secs(30), |t| {
        t.contains("DIRROW_OK")
    })?;
    assert!(text.contains("DIRROW_OK"));

    // Restore perms so teardown can sweep.
    fs::set_permissions(&events_dir, fs::Permissions::from_mode(0o700))?;
    assert!(
        !events_file(&jail, session, 1).exists(),
        "session ran unlogged (writer open failed)"
    );

    let ev = evidence_dir("ack1_r_dir");
    fs::write(
        ev.join("result.txt"),
        "R-DIR PASS (emitter failure isolated from the PTY contract)\n",
    )?;
    teardown_jail(&jail)?;
    Ok(())
}
