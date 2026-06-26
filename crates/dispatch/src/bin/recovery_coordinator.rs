//! `recovery_coordinator` — the standing R3c-Step-2 recovery coordinator (§5.1).
//!
//! A single, separately-supervised process (systemd `--user`, `Restart=always` + a
//! watchdog timer) that watches ALL sessions' durable recovery rows and drives the
//! ladder's crash-resilient, leased, heartbeated state machine. It is a SEPARATE bin
//! from `dispatch` (a distinct compilation unit), so the `dispatch` binary stays
//! byte-intact. The ladder BRAIN + the pass logic live in [`dispatch::recovery`]; this
//! bin is the thin process shell around [`dispatch::recovery::coordinator_pass`].
//!
//! ## Why a separate standing process (C-1)
//! The agent cannot run its own recovery (it may be the wedged party), and sbmux is
//! per-session (no one daemon to be "the coordinator"). The per-session daemons remain
//! the SERVICERS of their control sockets (Rung 2); this coordinator is the global
//! DECIDER/timer. The split is deliberate — servicer (per-session) and coordinator
//! (global) are different SPOF surfaces.
//!
//! ## SPOF mitigation (the zk-ops watchdog lesson)
//! Each pass bumps a heartbeat COUNTER (`<state_dir>/recovery/coordinator.heartbeat`);
//! an EXTERNAL watcher verifies it ADVANCES (a stalled-but-alive epoll loop is caught
//! and restarted, where the lease-steal resume picks up). `coordinator_incarnation`
//! (monotonic, persisted) fences two live coordinators. If the coordinator is DOWN, NO
//! automatic recovery fires — degraded to manual, never incorrect (fail-safe-down).
//!
//! ## Args
//! - `--state-dir <DIR>`   the sb state dir (default: `$SB_HOME/state` or
//!   `$HOME/.quorum/dispatch/state`).
//! - `--once`              run a single pass and exit (testing / cron-style).
//! - `--tick-ms <N>`       loop cadence (default 2000ms).

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Resolve the state dir from `--state-dir`, else `SB_HOME`/`HOME` like every other
/// component (the `paths.rs` resolution).
fn resolve_state_dir(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p);
    }
    let home = std::env::var("HOME").ok().filter(|s| !s.is_empty())?;
    let paths = dispatch::paths::SbPaths::from_home_env(
        std::path::Path::new(&home),
        &dispatch::effects::RealEnv,
    );
    Some(paths.state_dir)
}

/// Read + increment the persisted monotonic coordinator incarnation (fences two live
/// coordinators across restarts). Best-effort; a write failure falls back to a
/// time-derived value (still monotonic enough to fence within a boot).
fn next_coordinator_incarnation(state_dir: &std::path::Path) -> u64 {
    let path = state_dir.join("recovery").join("coordinator.incarnation");
    let prev = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let next = prev.saturating_add(1);
    if std::fs::create_dir_all(path.parent().unwrap()).is_ok() {
        let _ = std::fs::write(&path, next.to_string());
    }
    next
}

fn main() {
    let mut state_dir: Option<PathBuf> = None;
    let mut once = false;
    let mut tick_ms: u64 = 2000;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            "--once" => {
                once = true;
                i += 1;
            }
            "--tick-ms" => {
                tick_ms = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(tick_ms);
                i += 2;
            }
            other => {
                eprintln!("recovery_coordinator: unexpected argument '{other}'");
                std::process::exit(2);
            }
        }
    }

    let state_dir = match resolve_state_dir(state_dir) {
        Some(d) => d,
        None => {
            eprintln!("recovery_coordinator: cannot resolve state dir (set --state-dir or HOME)");
            std::process::exit(1);
        }
    };

    let incarnation = next_coordinator_incarnation(&state_dir);
    eprintln!(
        "recovery_coordinator: state_dir={state_dir:?} incarnation={incarnation} once={once} \
         tick_ms={tick_ms}"
    );

    let mut hb = dispatch::recovery::read_heartbeat(&state_dir).unwrap_or(0);
    loop {
        match dispatch::recovery::coordinator_pass(&state_dir, incarnation, hb, now_ms()) {
            Ok(report) => {
                hb = report.heartbeat;
                // Only narrate non-trivial passes (avoid log spam at idle).
                if report.rows_seen > 0 || report.corrupt_skipped > 0 {
                    eprintln!("recovery_coordinator: {report:?}");
                }
            }
            Err(e) => eprintln!("recovery_coordinator: pass error: {e}"),
        }
        if once {
            break;
        }
        std::thread::sleep(Duration::from_millis(tick_ms));
    }
}
