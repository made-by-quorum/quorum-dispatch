//! WP-B2b-2b — live daemon-launch wiring, integration (§6 DoD "fake-claude
//! end-to-end").
//!
//! Drives the REAL `sbmux::server::run_server` (the embedded daemon entry sb
//! itself runs) with the sb-side `DaemonHeadlessFactory` injected — exactly the
//! production seam (`crates/dispatch/src/bin/qd/daemon.rs`), only the fixture binary +
//! the isolated registry differ. A fixture shell script emits canned stream-json;
//! one client `LaunchHeadless`es, a second `SubscribeRepublish`es and receives
//! `RepublishReady`/`RepublishTurnEnd`/`RepublishEnd` AND the registry `<pid>.json`
//! flips busy→idle — proving the `Fanout` drives BOTH the socket sink and the
//! registry-status sink off ONE reader/pump.
//!
//! HOME isolation: the fixture launches under `clear_env` + a synthetic HOME (the
//! `env -i` contract — never the live `~/.claude*`); the registry is a tempdir.

use sbmux::protocol::{encode, ClientMsg, FrameReader, ServerMsg};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use dispatch::wait::{
    run_wait_content_loop, ChannelTurnCompletion, RealWaitContentDeps, StatusFallback,
    TurnCompletion, TurnCompletionProbe, WaitStatusOutcome,
};
use dispatch::wait_channel::ChannelSubscriber;

/// §H.2 canary on the channel-DOWN `pid.json` status read: counts every disk-status
/// read and returns `idle` if/when consulted. On the HEALTHY channel path it must
/// show ZERO reads (the §6.0 invariant, proven end-to-end over a real socket).
struct CanaryDisk(Arc<AtomicUsize>);
impl StatusFallback for CanaryDisk {
    fn read_disk_status(&self) -> Option<String> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Some("idle".to_string())
    }
}

/// §H.2 canary on the channel-DOWN transcript barrier: counts every probe and
/// returns `Visible` (which would FALSE-complete the turn if ever consulted on the
/// healthy path) — so a non-zero count is both a purity violation AND a wrong-answer.
struct CanaryProbe(Arc<AtomicUsize>);
impl TurnCompletion for CanaryProbe {
    fn poll_completion(&self) -> TurnCompletionProbe {
        self.0.fetch_add(1, Ordering::Relaxed);
        TurnCompletionProbe::Visible
    }
}

/// Drive a REAL `run_wait_content_loop` to completion off the LIVE daemon channel:
/// stand up `run_server` + the sb-side factory, `LaunchHeadless`, then (on a
/// blocking thread, so the daemon's runtime stays free) a real [`ChannelSubscriber`]
/// settles the channel and backs BOTH wait seams; the disk + transcript fallbacks
/// are CANARIES that must never fire. Returns `(outcome, disk_reads, probe_reads,
/// channel_came_up)`. Shared by the fake-claude and `#[ignore]` real-claude rows.
async fn drive_channel_wait(
    claude_bin: String,
    home: &Path,
    sessions_dir: &Path,
    sock_dir: &Path,
    timeout_ms: i64,
) -> (WaitStatusOutcome, usize, usize, bool) {
    let fac = factory(claude_bin, home, sessions_dir);
    let sock_dir_owned = sock_dir.to_path_buf();
    let server = tokio::spawn(async move {
        sbmux::server::run_server(Some(sock_dir_owned), SESSION.to_string(), Some(fac)).await
    });

    let sock_path = sbmux::server::session_socket_path_for(Some(sock_dir), SESSION).unwrap();
    wait_for_socket(&sock_path).await;

    // Client A: LaunchHeadless → drain the Connected ack. Held open for the turn.
    let mut a = UnixStream::connect(&sock_path).await.unwrap();
    handshake(&mut a).await;
    a.write_all(
        &encode(&ClientMsg::LaunchHeadless {
            name: SESSION.to_string(),
            prompt: "hi".to_string(),
            resume_session_id: None,
            cwd: None,
            claude_args: vec![],
        })
        .unwrap(),
    )
    .await
    .unwrap();
    {
        let mut fr = FrameReader::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "no LaunchHeadless ack");
            if fr.fill_from(&mut a).await.unwrap()
                && fr.decode_next::<ServerMsg>().unwrap().is_some()
            {
                break;
            }
        }
    }

    // The wait loop is SYNCHRONOUS (blocks on RealSleeper) — run it on a blocking
    // thread so the daemon's current-thread runtime keeps driving the headless
    // reader + the subscribe relay. The ChannelSubscriber connects from there.
    let sock_path2 = sock_path.clone();
    let (outcome, disk, probe, came_up) = tokio::task::spawn_blocking(move || {
        let disk = Arc::new(AtomicUsize::new(0));
        let probe = Arc::new(AtomicUsize::new(0));
        let sub = ChannelSubscriber::connect(sock_path2, SESSION.to_string());
        // Settle the channel onto the live turn BEFORE the loop (entry-idle gate's
        // contract) — proves the channel comes up, so the no-disk claim is real.
        let settled = sub.await_status(Duration::from_secs(10));
        let came_up = matches!(settled, dispatch::wait::ChannelStatusObservation::Live(_));

        let clock = dispatch::effects::RealClock;
        let sleeper = dispatch::boot::RealSleeper;
        let completion = ChannelTurnCompletion {
            source: sub.turn_source(),
            fallback: Box::new(CanaryProbe(probe.clone())),
        };
        let deps = RealWaitContentDeps {
            status_source: Some(sub.status_source()),
            status_fallback: Box::new(CanaryDisk(disk.clone())),
            completion: Some(Box::new(completion)),
            clock: &clock,
            sleeper: &sleeper,
        };
        let outcome = run_wait_content_loop(&deps, timeout_ms, 50);
        (
            outcome,
            disk.load(Ordering::Relaxed),
            probe.load(Ordering::Relaxed),
            came_up,
        )
    })
    .await
    .unwrap();

    drop(a);
    let exited = tokio::time::timeout(Duration::from_secs(15), server).await;
    assert!(exited.is_ok(), "daemon did not exit-on-end after the turn");
    (outcome, disk, probe, came_up)
}

const SESSION: &str = "test-hl";
const PID: i64 = 4242;
const BOOT_TS: i64 = 1000;

/// A fake `claude -p` (ignores the appended `-p PROMPT --output-format ...`): a
/// startup sleep lets the subscriber attach BEFORE `system/init` (the hub has no
/// replay), a mid-stream sleep holds the row "busy" long enough to observe, then
/// the `result` lands and the process EOFs.
fn write_fixture(dir: &Path, sleep_after_init: &str) -> String {
    let p = dir.join("fake_claude.sh");
    let body = format!(
        "#!/bin/bash\n\
         sleep 0.5\n\
         echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fake-sid\"}}'\n\
         echo '{{\"type\":\"assistant\",\"session_id\":\"fake-sid\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}'\n\
         sleep {sleep_after_init}\n\
         echo '{{\"type\":\"result\",\"session_id\":\"fake-sid\",\"is_error\":false,\"stop_reason\":\"end_turn\"}}'\n"
    );
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

/// A fixture that aborts mid-turn: emits `system/init` (→ busy) then exits WITHOUT
/// a `result` — the EOF-without-result abort the fail-closed status path keys on.
fn write_abort_fixture(dir: &Path) -> String {
    let p = dir.join("abort_claude.sh");
    let body = "#!/bin/bash\n\
         sleep 0.5\n\
         echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fake-sid\"}'\n";
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

fn boot_row(sessions_dir: &Path) {
    let entry = dispatch::registry::RegistryEntry {
        pid: Some(PID),
        session_id: Some("boot-sid".into()),
        started_at: Some(BOOT_TS),
        updated_at: Some(BOOT_TS),
        status: Some("idle".into()),
        ..Default::default()
    };
    dispatch::registry::write_entry(sessions_dir, &entry).unwrap();
}

fn factory(
    claude_bin: String,
    home: &Path,
    sessions_dir: &Path,
) -> Arc<dyn sbmux::headless_session::HeadlessFactory> {
    Arc::new(dispatch::daemon_headless::DaemonHeadlessFactory {
        claude_bin,
        flags: vec![],
        // env -i isolation: ONLY these vars (never the live ~/.claude*).
        env: vec![
            ("HOME".to_string(), home.to_string_lossy().into_owned()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ],
        clear_env: true,
        cwd: None,
        sessions_dir: sessions_dir.to_path_buf(),
        // start-only launches here (resume_session_id=None) → ids_path is never read.
        ids_path: home.join(".quorum").join("dispatch").join("ids.jsonl"),
        progress: std::sync::Arc::new(dispatch::progress::ProgressRecorder::new()),
        turn_clock: std::sync::Arc::new(dispatch::progress::TurnStartRecorder::new()),
    })
}

/// WP-B5-i: find the daemon-MINTED row by sb name (it is keyed on the claude CHILD
/// pid, NOT the daemon pid `PID`, so we cannot read it by `PID`). Scans the
/// sessions dir for the live row whose `name` matches.
fn minted_row(sessions_dir: &Path, name: &str) -> Option<dispatch::registry::RegistryEntry> {
    dispatch::registry::read_entries(sessions_dir, false)
        .into_iter()
        .map(|s| s.entry)
        .find(|e| e.name.as_deref() == Some(name))
}

/// The minted row's current status (by name).
fn minted_status(sessions_dir: &Path, name: &str) -> Option<String> {
    minted_row(sessions_dir, name).and_then(|e| e.status)
}

/// v3 handshake: preamble → Hello → consume ServerHello.
async fn handshake(stream: &mut UnixStream) {
    sbmux::protocol::write_preamble(stream).await.unwrap();
    let hello = encode(&ClientMsg::Hello { caps: vec![] }).unwrap();
    stream.write_all(&hello).await.unwrap();
    let mut fr = FrameReader::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for ServerHello"
        );
        if fr.fill_from(stream).await.unwrap() {
            if let Some(ServerMsg::Hello { .. }) = fr.decode_next::<ServerMsg>().unwrap() {
                return;
            }
        }
    }
}

async fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "daemon socket never appeared at {path:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Read ServerMsg frames until a `RepublishEnd` (or deadline), returning all.
async fn read_until_end(stream: &mut UnixStream) -> Vec<ServerMsg> {
    let mut fr = FrameReader::new();
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if Instant::now() > deadline {
            panic!("timed out before RepublishEnd; got {out:?}");
        }
        tokio::select! {
            r = fr.fill_from(stream) => {
                if !r.unwrap() { break; } // EOF
                while let Some(m) = fr.decode_next::<ServerMsg>().unwrap() {
                    let terminal = matches!(m, ServerMsg::RepublishEnd { .. });
                    out.push(m);
                    if terminal { return out; }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    out
}

#[test]
fn fake_claude_launch_subscribe_and_registry_flip() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let sock_dir = root.path().join("sock");
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&sock_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let bin = write_fixture(root.path(), "0.4");
        // WP-B5-i: NO boot row seeded — the daemon MINTS the child-pid-keyed row
        // itself (the daemon-mint fallback); a pre-existing row is not required.

        let fac = factory(bin, &home, &sessions_dir);
        let sock_dir_owned = sock_dir.clone();
        let server = tokio::spawn(async move {
            sbmux::server::run_server(Some(sock_dir_owned), SESSION.to_string(), Some(fac)).await
        });

        let sock_path = sbmux::server::session_socket_path_for(Some(&sock_dir), SESSION).unwrap();
        wait_for_socket(&sock_path).await;

        // Client A: LaunchHeadless → Connected ack.
        let mut a = UnixStream::connect(&sock_path).await.unwrap();
        handshake(&mut a).await;
        a.write_all(
            &encode(&ClientMsg::LaunchHeadless {
                name: SESSION.to_string(),
                prompt: "hi".to_string(),
                resume_session_id: None,
                cwd: None,
                claude_args: vec![],
            })
            .unwrap(),
        )
        .await
        .unwrap();
        {
            let mut fr = FrameReader::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            let ack = loop {
                assert!(Instant::now() < deadline, "no LaunchHeadless ack");
                if fr.fill_from(&mut a).await.unwrap() {
                    if let Some(m) = fr.decode_next::<ServerMsg>().unwrap() {
                        break m;
                    }
                }
            };
            assert!(
                matches!(ack, ServerMsg::Connected { ref name, new_session: true } if name == SESSION),
                "LaunchHeadless must ack Connected, got {ack:?}"
            );
        }

        // Poll the registry in the background to capture the busy→idle transition.
        let stop = Arc::new(AtomicBool::new(false));
        let poll_dir = sessions_dir.clone();
        let stop2 = stop.clone();
        let poller = tokio::spawn(async move {
            let mut seen: Vec<String> = Vec::new();
            while !stop2.load(Ordering::Relaxed) {
                // WP-B5-i: the row is MINTED by the daemon, keyed on the claude
                // CHILD pid — read it by NAME, not by the daemon pid `PID`.
                if let Some(s) = minted_status(&poll_dir, SESSION) {
                    if seen.last() != Some(&s) {
                        seen.push(s);
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            seen
        });

        // Client B: SubscribeRepublish → Ready / TurnEnd / End.
        let mut b = UnixStream::connect(&sock_path).await.unwrap();
        handshake(&mut b).await;
        b.write_all(
            &encode(&ClientMsg::SubscribeRepublish {
                name: SESSION.to_string(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let frames = read_until_end(&mut b).await;

        stop.store(true, Ordering::Relaxed);
        let seen = poller.await.unwrap();

        // (1) The socket sink was driven: Ready, TurnEnd, End in order.
        assert!(
            frames.iter().any(|m| matches!(m, ServerMsg::RepublishReady { session_id } if session_id == "fake-sid")),
            "missing RepublishReady; got {frames:?}"
        );
        assert!(
            frames.iter().any(|m| matches!(m, ServerMsg::RepublishTurnEnd { session_id, is_error: false, .. } if session_id == "fake-sid")),
            "missing RepublishTurnEnd; got {frames:?}"
        );
        assert!(
            matches!(frames.last(), Some(ServerMsg::RepublishEnd { .. })),
            "last frame must be RepublishEnd; got {frames:?}"
        );

        // (2) WP-B5-i: the daemon MINTED a child-pid-keyed addressable row, and the
        // SAME Fanout drove it busy→idle. No pre-existing boot row was seeded — the
        // daemon stamps the row itself (the daemon-mint fallback).
        assert!(
            seen.iter().any(|s| s == "busy"),
            "minted row never went busy (Fanout did not drive the status sink); saw {seen:?}"
        );
        let row = minted_row(&sessions_dir, SESSION).expect("daemon must mint a row named SESSION");
        assert_eq!(
            row.status.as_deref(),
            Some("idle"),
            "minted row must end idle after the turn; saw {seen:?}"
        );
        // Identity: the row carries the system/init session_id, the sb name, the
        // headless discriminant, and NO provider field (claude rows carry none →
        // the join defaults absent to "claude-code" → addressable + connect-routable;
        // WP-B5-i D ruling).
        assert_eq!(row.session_id.as_deref(), Some("fake-sid"), "row session_id");
        assert_eq!(row.name.as_deref(), Some(SESSION), "row name");
        assert_eq!(row.entrypoint.as_deref(), Some("headless"), "headless marker");
        assert_eq!(row.provider, None, "claude rows carry no provider field");
        // CHILD-PID-keyed (option B, NOT option A): the row's pid is the spawned
        // claude/fixture child — NEVER the daemon's own pid (here, the test process).
        let daemon_pid = std::process::id() as i64;
        assert!(
            row.pid.is_some() && row.pid != Some(daemon_pid),
            "row must be keyed on the claude CHILD pid, not the daemon pid {daemon_pid}; got {:?}",
            row.pid
        );
        // updated_at advanced past the row's own started_at → the sink actually wrote.
        assert!(
            matches!((row.updated_at, row.started_at), (Some(u), Some(s)) if u >= s),
            "updated_at must be >= started_at; got updated={:?} started={:?}",
            row.updated_at,
            row.started_at
        );

        // (3) Lifecycle: the turn completed + was reaped → the single-session
        // daemon exits-on-end (is_empty true again). Bounded so a hang fails.
        let exited = tokio::time::timeout(Duration::from_secs(15), server).await;
        assert!(
            exited.is_ok(),
            "daemon did not exit-on-end after the headless turn completed"
        );
        assert!(exited.unwrap().unwrap().is_ok(), "run_server returned Err");
    });
}

/// breaker/EOF-abort → offline lifecycle: a fixture that emits `system/init`
/// (busy) then EOFs WITHOUT a `result`. The fail-closed status path must flip the
/// row to `offline` (never leave a stale `busy`), and the subscriber gets a
/// terminal `RepublishEnd`.
#[test]
fn eof_abort_flips_offline_and_ends() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let sock_dir = root.path().join("sock");
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&sock_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let bin = write_abort_fixture(root.path());
        // WP-B5-i: NO boot row — the daemon mints the child-pid-keyed row, and the
        // fail-closed EOF path must flip THAT row (not a seeded one) to offline.

        let fac = factory(bin, &home, &sessions_dir);
        let sock_dir_owned = sock_dir.clone();
        let server = tokio::spawn(async move {
            sbmux::server::run_server(Some(sock_dir_owned), SESSION.to_string(), Some(fac)).await
        });

        let sock_path = sbmux::server::session_socket_path_for(Some(&sock_dir), SESSION).unwrap();
        wait_for_socket(&sock_path).await;

        let mut a = UnixStream::connect(&sock_path).await.unwrap();
        handshake(&mut a).await;
        a.write_all(
            &encode(&ClientMsg::LaunchHeadless {
                name: SESSION.to_string(),
                prompt: "hi".to_string(),
                resume_session_id: None,
                cwd: None,
                claude_args: vec![],
            })
            .unwrap(),
        )
        .await
        .unwrap();
        // Drain the Connected ack.
        {
            let mut fr = FrameReader::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(Instant::now() < deadline, "no ack");
                if fr.fill_from(&mut a).await.unwrap()
                    && fr.decode_next::<ServerMsg>().unwrap().is_some()
                {
                    break;
                }
            }
        }

        let mut b = UnixStream::connect(&sock_path).await.unwrap();
        handshake(&mut b).await;
        b.write_all(
            &encode(&ClientMsg::SubscribeRepublish {
                name: SESSION.to_string(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let frames = read_until_end(&mut b).await;
        assert!(
            matches!(frames.last(), Some(ServerMsg::RepublishEnd { .. })),
            "aborted turn must still deliver a terminal RepublishEnd; got {frames:?}"
        );

        // Fail-closed: EOF without a result → offline (never stale-busy). Read the
        // MINTED row by name (child-pid-keyed; the daemon stamped it on `Ready`).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut final_status = None;
        while Instant::now() < deadline {
            final_status = minted_status(&sessions_dir, SESSION);
            if final_status.as_deref() == Some("offline") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            final_status.as_deref(),
            Some("offline"),
            "EOF-without-result must fail closed to offline, never stale-busy"
        );

        let exited = tokio::time::timeout(Duration::from_secs(15), server).await;
        assert!(
            exited.is_ok(),
            "daemon did not exit-on-end after the aborted turn"
        );
    });
}

/// WP-B2b-2b deliverable 2 + RESIDUAL #1, END-TO-END: a REAL `run_wait_content_loop`
/// completes a fake-claude turn off the LIVE daemon channel — and NEITHER the
/// `pid.json` disk-status read NOR the transcript barrier is consulted (both are
/// canaries; both must show 0). This is the §H.2 §6.0-purity keystone proven over a
/// real socket (the lib unit rows prove it over scripted seams).
#[test]
fn wait_loop_completes_off_live_channel_no_disk() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let sock_dir = root.path().join("sock");
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&sock_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let bin = write_fixture(root.path(), "0.4");
        boot_row(&sessions_dir);

        let (outcome, disk, probe, came_up) =
            drive_channel_wait(bin, &home, &sessions_dir, &sock_dir, 60_000).await;

        assert!(
            came_up,
            "the live channel must come up (Republish frames arrived)"
        );
        assert_eq!(
            outcome,
            WaitStatusOutcome::Done,
            "the turn completes off the live channel"
        );
        assert_eq!(
            disk, 0,
            "§6.0 violation: a pid.json status read on the HEALTHY channel wait path"
        );
        assert_eq!(
            probe, 0,
            "gate R1 violation: a transcript read on the HEALTHY channel wait path"
        );
    });
}

/// `#[ignore]` ONE real isolated claude turn driven to Done through the FULL live
/// path: `run_server` + the sb-side `DaemonHeadlessFactory` + a real `claude -p`,
/// waited via a real `ChannelSubscriber` + `run_wait_content_loop` — completing off
/// the channel with ZERO disk-status reads. Isolation = the lib.sh `env -i` contract
/// (synthetic HOME + CLAUDE_CONFIG_DIR, NEVER the live `~/.claude*`).
///
///   cargo test -p dispatch --test headless_b2b2b real_isolated_claude_wait -- --ignored --nocapture
#[test]
#[ignore = "needs network + live credentials; isolated via lib.sh-style env -i"]
fn real_isolated_claude_wait_completes_off_channel() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let claude_bin = std::env::var("CLAUDE_BIN")
            .unwrap_or_else(|_| "/home/u/.local/bin/claude".to_string());
        let live_creds = std::env::var("LIVE_CREDENTIALS")
            .unwrap_or_else(|_| "/home/u/.claude/.credentials.json".to_string());

        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let config = root.path().join("config");
        let sock_dir = root.path().join("sock");
        let sessions_dir = root.path().join("sessions");
        for d in [&home, &config, &sock_dir, &sessions_dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        if Path::new(&live_creds).exists() {
            std::fs::copy(&live_creds, config.join(".credentials.json")).expect("copy creds");
        }
        // Controlled baseline claude.json (onboarding done, no MCP, cwd trusted).
        let home_str = home.to_string_lossy();
        let claude_json = format!(
            r#"{{ "hasCompletedOnboarding": true, "numStartups": 0, "autoUpdates": false, "bypassPermissionsModeAccepted": true, "mcpServers": {{}}, "projects": {{ "{home_str}": {{ "hasTrustDialogAccepted": true, "projectOnboardingSeenCount": 1, "allowedTools": [] }} }} }}"#
        );
        std::fs::write(config.join(".claude.json"), claude_json).unwrap();

        // The factory's env carries the isolation argv. We need CLAUDE_CONFIG_DIR;
        // build the factory inline (the shared helper uses the fake-claude env set).
        let fac: Arc<dyn sbmux::headless_session::HeadlessFactory> =
            Arc::new(dispatch::daemon_headless::DaemonHeadlessFactory {
                claude_bin,
                flags: vec![],
                env: vec![
                    ("HOME".to_string(), home.to_string_lossy().into_owned()),
                    (
                        "CLAUDE_CONFIG_DIR".to_string(),
                        config.to_string_lossy().into_owned(),
                    ),
                    ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                    ("TERM".to_string(), "dumb".to_string()),
                ],
                clear_env: true,
                cwd: Some(home.to_string_lossy().into_owned()),
                sessions_dir: sessions_dir.clone(),
                // start-only launch (resume_session_id=None) → ids_path unused.
                ids_path: home.join(".quorum").join("dispatch").join("ids.jsonl"),
                progress: std::sync::Arc::new(dispatch::progress::ProgressRecorder::new()),
                turn_clock: std::sync::Arc::new(dispatch::progress::TurnStartRecorder::new()),
            });

        let sock_dir_owned = sock_dir.clone();
        let server = tokio::spawn(async move {
            sbmux::server::run_server(Some(sock_dir_owned), SESSION.to_string(), Some(fac)).await
        });
        let sock_path = sbmux::server::session_socket_path_for(Some(&sock_dir), SESSION).unwrap();
        wait_for_socket(&sock_path).await;

        let mut a = UnixStream::connect(&sock_path).await.unwrap();
        handshake(&mut a).await;
        a.write_all(
            &encode(&ClientMsg::LaunchHeadless {
                name: SESSION.to_string(),
                prompt: "Reply with exactly: PONG".to_string(),
                resume_session_id: None,
                cwd: None,
                claude_args: vec![],
            })
            .unwrap(),
        )
        .await
        .unwrap();
        {
            let mut fr = FrameReader::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(Instant::now() < deadline, "no LaunchHeadless ack");
                if fr.fill_from(&mut a).await.unwrap()
                    && fr.decode_next::<ServerMsg>().unwrap().is_some()
                {
                    break;
                }
            }
        }

        let sock_path2 = sock_path.clone();
        let (outcome, disk, came_up) = tokio::task::spawn_blocking(move || {
            let disk = Arc::new(AtomicUsize::new(0));
            let probe = Arc::new(AtomicUsize::new(0));
            let sub = ChannelSubscriber::connect(sock_path2, SESSION.to_string());
            let settled = sub.await_status(Duration::from_secs(10));
            let came_up = matches!(settled, dispatch::wait::ChannelStatusObservation::Live(_));
            let clock = dispatch::effects::RealClock;
            let sleeper = dispatch::boot::RealSleeper;
            let completion = ChannelTurnCompletion {
                source: sub.turn_source(),
                fallback: Box::new(CanaryProbe(probe.clone())),
            };
            let deps = RealWaitContentDeps {
                status_source: Some(sub.status_source()),
                status_fallback: Box::new(CanaryDisk(disk.clone())),
                completion: Some(Box::new(completion)),
                clock: &clock,
                sleeper: &sleeper,
            };
            let outcome = run_wait_content_loop(&deps, 120_000, 100);
            (outcome, disk.load(Ordering::Relaxed), came_up)
        })
        .await
        .unwrap();

        drop(a);
        let _ = tokio::time::timeout(Duration::from_secs(20), server).await;

        assert!(came_up, "the live channel must come up for a real turn");
        assert_eq!(
            outcome,
            WaitStatusOutcome::Done,
            "a real claude turn completes off the live channel"
        );
        assert_eq!(
            disk, 0,
            "§6.0: a real-turn wait must read NO pid.json status on the healthy channel"
        );

        // WP-B5-i (D, req 4): the REAL-claude mint is ls/resolve-ADDRESSABLE. The
        // daemon minted a child-pid-keyed registry row from claude's real
        // `system/init` session_id — read it back by NAME (the same join `sb ls`/
        // `sb connect` resolve through) and assert the addressability facts: a
        // headless-marked row carrying NO provider field (claude rows carry none →
        // resolver-uniform) + a real session_id, keyed on the claude CHILD pid
        // (never the daemon/test pid). This is the seed B5-ii proves survives
        // daemon death.
        let row = minted_row(&sessions_dir, SESSION)
            .expect("real-claude turn must mint an addressable row named SESSION");
        assert_eq!(row.name.as_deref(), Some(SESSION), "addressable by name");
        assert!(
            row.session_id.as_deref().is_some_and(|s| !s.is_empty()),
            "addressable by a real claude session id; got {:?}",
            row.session_id
        );
        assert_eq!(row.entrypoint.as_deref(), Some("headless"), "headless marker");
        assert_eq!(row.provider, None, "claude rows carry no provider field");
        let daemon_pid = std::process::id() as i64;
        assert!(
            row.pid.is_some() && row.pid != Some(daemon_pid),
            "child-pid-keyed (not the daemon/test pid {daemon_pid}); got {:?}",
            row.pid
        );
        println!(
            "REAL CLAUDE WAIT: Done off the live channel, 0 disk reads; \
             minted row addressable by name={SESSION} id={:?} pid={:?}",
            row.session_id, row.pid
        );
    });
}
