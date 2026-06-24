//! RED-TEAM r1 REPRO (b1-launch), adapted into the suite as the F1 PIN.
//!
//! Original finding (red-team round 1 on c1703fb): a HEALTHY slow boot was
//! declared dead by the punch-6 fail-fast when list_raw degrades errors to
//! Ok-with-no-rows — which is EXACTLY what both production muxes do:
//!   - ZmxMux::scan: any zmx failure (spawn Err OR nonzero exit) -> Ok(vec![])
//!     (zmx_mux.rs, pinned by its own test list_missing_dir_is_empty_not_error)
//!   - EmbeddedMux list_raw -> scan_sessions: a per-daemon probe timeout (2s),
//!     handshake error, or identity-skip silently OMITS the row from an Ok
//!     result (discovery.rs ProbeOutcome::Skip/Empty).
//!
//! The waiter's "a list ERROR is unknown, never death" arm was therefore
//! unreachable on the zmx lane and bypassable on the embedded lane — two
//! transient list failures convicted a healthy pane, and on the `sb start`
//! path the verdict deleted the env file the pane had not yet sourced (the
//! fail-closed prefix then KILLED the healthy session).
//!
//! ADAPTED (F1 fix): the assertion is INVERTED — absence with no prior alive
//! sighting is now UNKNOWN (never convicts), so the waiter survives the
//! degraded list and the healthy boot completes when the PID file lands.
//! If the F1 regression returns, this test fails fast with the death verdict.

use dispatch::boot::{BootTimeouts, EventBootWaiter, RealSleeper};
use dispatch::create::BootWaiter;
use dispatch::exec::ExecResult;
use dispatch::mux::{Mux, MuxSession};
use std::io;
use std::path::{Path, PathBuf};

/// Models the PRODUCTION ZmxMux while `zmx list` is failing (e.g. fork EAGAIN
/// under a fleet boot storm): list_raw returns Ok(vec![]) — never Err.
/// (FIXTURE FIDELITY IS BIDIRECTIONAL: this fixture deliberately has NO Err
/// channel, matching production's Ok-erasure honesty exactly.)
struct DegradedListMux;

impl Mux for DegradedListMux {
    fn list(&self, _d: &Path) -> io::Result<Vec<MuxSession>> {
        unreachable!("boot waiter must use list_raw, never the filtered list")
    }
    fn list_raw(&self, _d: &Path) -> io::Result<Vec<MuxSession>> {
        // ZmxMux::scan maps zmx failure -> "" -> Ok(vec![]) (TS catch->[]).
        Ok(vec![])
    }
    fn run_detached(&self, _d: &Path, _n: &str, _c: &str, _w: &Path) -> io::Result<ExecResult> {
        unreachable!()
    }
    fn send(&self, _d: &Path, _n: &str, _t: &str) -> io::Result<ExecResult> {
        unreachable!("waiter must not type at a session it thinks is dead")
    }
    fn kill(&self, _d: &Path, _n: &str) -> io::Result<i32> {
        unreachable!()
    }
    fn history(&self, _d: &Path, _n: &str) -> io::Result<String> {
        // The booting pane is alive and printing (healthy boot, no dialog).
        Ok("Claude Code is booting...\n".to_string())
    }
    fn wait(&self, _d: &Path, _n: &[String]) -> io::Result<i32> {
        unreachable!()
    }
    fn attach(&self, _d: &Path, _n: &str) -> io::Result<i32> {
        unreachable!()
    }
}

/// The healthy boot completes: claude writes its registry pid file ~600ms in
/// (well inside the phase budget).
fn spawn_pid_writer(dir: PathBuf, name: String) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(600));
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(
            dir.join("100.json"),
            format!(r#"{{"pid":100,"name":"{name}","status":"idle"}}"#),
        )
        .ok();
    });
}

#[test]
fn healthy_slow_boot_survives_degraded_ok_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mux = DegradedListMux;
    spawn_pid_writer(sessions.clone(), "sess".to_string());

    let clock = dispatch::effects::RealClock;
    let sleeper = RealSleeper;
    let mut waiter =
        EventBootWaiter::new(&mux, tmp.path().join("zmx-501"), sessions, &clock, &sleeper);
    // Real clock, bounded budget: a regression to the false verdict fails the
    // assert below; a regression to never-finding-the-pid-file fails in 10s,
    // not the production 40s.
    waiter.timeouts = BootTimeouts {
        overall_ms: 10_000,
        pid_phase_ms: 10_000,
        poll_ms: 25,
        settle_ms: 1,
    };
    let started = std::time::Instant::now();
    let result = waiter.wait_ready("sess");
    let elapsed = started.elapsed();

    // THE F1 PIN: Ok(vec![]) throughout (the production degraded-list shape,
    // indistinguishable from "row not registered yet") must NOT convict — the
    // waiter polls through the degraded rounds and the healthy boot lands.
    assert!(
        result.is_ok(),
        "a degraded Ok-empty list must never convict a healthy boot; got: {result:?}"
    );
    // It genuinely WAITED through the degraded window (the pid file landed at
    // ~600ms — an instant Ok would mean the scan short-circuited some other way).
    assert!(
        elapsed >= std::time::Duration::from_millis(500),
        "the waiter should have polled until the pid file landed: {elapsed:?}"
    );
}
