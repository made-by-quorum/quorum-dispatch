//! Teardown-leak belt — prefix-scoped reap at jail setup, per-target by pidfile.
//!
//! THE LEAK CLASS (live finding, orc-adopted option (b)): jailed qrmux daemons
//! setsid (ppid 1) and outlive a SIGTERM'd / SIGKILL'd test runner — DaemonGuard
//! Drop and Jail teardown NEVER run on an abrupt runner death, so the daemon
//! orphans and survives past its jail. The N-session split multiplies this.
//!
//! BELT SHAPE (NOT a sweep — ADD-12-compatible, per-target by construction).
//!
//! OWNER STAMP: every jail records the OWNING test-harness pid (`process::id()`)
//! into `<jail_root>/owner.pid` at setup. This is the CONCURRENCY DISCRIMINATOR:
//! sibling tests in a live `cargo test` process share the family root, so "is this
//! a PRIOR (dead) run vs a CONCURRENT (live) sibling?" is answered by "is the
//! owner pid still alive?". The reaper NEVER touches a dir whose owner is alive.
//!
//! RECORD: every harness daemon spawn (`start_daemon_in_jail_with_stderr`) appends
//! `pid<TAB>identity` to `<jail_root>/daemons.pids`. The identity is the daemon's
//! `--socket-dir` path (unique per run dir) — a pid-reuse impostor cannot carry it
//! in its argv. (The qrmux harness spawns daemons directly, so the pid is always
//! known at spawn — no post-boot lookup is needed here, unlike the engine `sb new`
//! family.)
//!
//! REAP-AT-SETUP: when a NEW jail initializes (`Jail::establish`), scan the family
//! root (`/tmp/qrmux-runs/*`) for run dirs whose OWNER PID IS DEAD (prior runs);
//! for each recorded `pid<TAB>identity`, kill ONLY if the daemon pid is ALIVE and
//! its CURRENT argv still carries the recorded identity (the pid-reuse belt —
//! never kill on pid alone), then remove the stale dir. Bounded + best-effort:
//! reap failures warn (eprintln) and NEVER fail the new run's setup.
//!
//! INDEPENDENT of `client::sweep_orphan_daemons` (ppid==1 + exact-binary-path
//! sweep, fires per spawn): that one is binary-scoped; this one is run-dir-scoped,
//! owner-gated, and identity-pinned by recorded pidfile. ~Mirror of the engine-side
//! `crates/sb/tests/c1_gate_inc/daemon_reaper.rs` (cross-crate dup is intentional
//! — no shared dependency forced; WS-C follow-up scope note).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The fixed family root all qrmux test jails live under (see `jail::Jail`).
const QRMUX_FAMILY_ROOT: &str = "/tmp/qrmux-runs";

/// Stamp the OWNING test-harness pid into `<run_root>/owner.pid`. The reaper uses
/// this to tell a PRIOR (dead-owner) run from a CONCURRENT (live-owner) sibling in
/// the shared family root. Best-effort: a failed write warns, never panics.
pub fn stamp_owner_pid(run_root: &Path) {
    let f = run_root.join("owner.pid");
    if let Err(e) = std::fs::write(&f, format!("{}\n", std::process::id())) {
        eprintln!("[daemon-reaper] warn: cannot write owner stamp {f:?}: {e}");
    }
}

/// Is `pid` alive? (`kill -0` semantics via `/bin/kill`.)
fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The current process argv for `pid`, via `ps -p <pid> -o args=`. `None` if the
/// pid is gone or ps can't report it (→ caller treats as "no identity match").
fn pid_argv(pid: u32) -> Option<String> {
    let out = Command::new("/bin/ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Append `pid<TAB>identity` to `<run_root>/daemons.pids`. Best-effort: a failed
/// write warns but never panics. `identity` MUST be a token that appears in the
/// daemon's live argv (we use its `--socket-dir`) so the reap-side check is sound.
pub fn record_daemon_pid(run_root: &Path, pid: u32, identity: &str) {
    let pidfile = run_root.join("daemons.pids");
    use std::io::Write as _;
    let line = format!("{pid}\t{identity}\n");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&pidfile)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!("[daemon-reaper] warn: cannot append to {pidfile:?}: {e}");
            }
        }
        Err(e) => eprintln!("[daemon-reaper] warn: cannot open {pidfile:?}: {e}"),
    }
}

/// Production entry point: reap leaked daemons under the real family root.
/// (Thin wrapper over `reap_in_family` so the self-test can scan a PRIVATE family
/// root and never contend with concurrently-running real qrmux jails.)
pub fn reap_prior_run_daemons(current_run_root: &Path) -> usize {
    reap_in_family(Path::new(QRMUX_FAMILY_ROOT), current_run_root)
}

/// Reap daemons leaked by PRIOR (dead-owner) runs under `family`. Scans `family/*`,
/// SKIPPING `current_run_root` and any dir whose `owner.pid` is still alive (a
/// concurrent sibling — NEVER touched). For each remaining (prior) dir's
/// `daemons.pids` entry, kills the daemon pid ONLY if it is alive AND its current
/// argv still carries the recorded identity (pid-reuse belt), then removes the
/// stale dir. Best-effort: every failure warns, never fails setup. Returns the
/// count actually reaped.
fn reap_in_family(family: &Path, current_run_root: &Path) -> usize {
    let entries = match std::fs::read_dir(family) {
        Ok(e) => e,
        Err(_) => return 0, // family root absent on first-ever run → nothing to reap
    };
    let mut reaped = 0usize;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || dir == current_run_root {
            continue;
        }
        // CONCURRENCY GUARD: if this dir's owner test-harness pid is still alive,
        // it is a CONCURRENT sibling (not a prior run) — never touch it.
        if let Ok(owner) = std::fs::read_to_string(dir.join("owner.pid")) {
            if let Ok(owner_pid) = owner.trim().parse::<u32>() {
                if pid_alive(owner_pid) {
                    continue;
                }
            }
        }
        let pidfile = dir.join("daemons.pids");
        // ONLY touch dirs that recorded a pidfile — a real prior run that spawned
        // at least one daemon.
        let Ok(contents) = std::fs::read_to_string(&pidfile) else {
            continue;
        };
        for line in contents.lines() {
            let Some((pid_s, identity)) = line.split_once('\t') else {
                continue;
            };
            let Ok(pid) = pid_s.trim().parse::<u32>() else {
                continue;
            };
            // The pid-reuse belt: kill ONLY if the pid is alive AND its CURRENT
            // argv still carries the recorded identity. A reused pid running a
            // DIFFERENT process won't match → it SURVIVES.
            let alive = pid_alive(pid);
            let identity_matches = pid_argv(pid)
                .map(|argv| argv.contains(identity))
                .unwrap_or(false);
            if alive && identity_matches {
                let _ = Command::new("/bin/kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .stderr(Stdio::null())
                    .status();
                std::thread::sleep(Duration::from_millis(150));
                let _ = Command::new("/bin/kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .stderr(Stdio::null())
                    .status();
                reaped += 1;
            } else if alive {
                eprintln!(
                    "[daemon-reaper] pid {pid} alive but identity {identity:?} not in its argv \
                     — leaving it (pid-reuse belt)"
                );
            }
        }
        // Remove the stale prior run dir (best-effort) — only reached for
        // dead-owner dirs that had a pidfile.
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            eprintln!("[daemon-reaper] warn: cannot remove stale prior run dir {dir:?}: {e}");
        }
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// Negative + positive control: a recorded pid whose argv carries the identity
    /// (in a dead-owner prior dir) IS reaped; a pid-reuse impostor (recorded with a
    /// wrong identity) SURVIVES.
    #[test]
    fn identity_pinned_reap_negative_and_positive() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        // PRIVATE family root for this self-test: isolates it from concurrently-
        // running real qrmux jails (whose setup-reapers scan the real family root)
        // so the controls are deterministic and the test can never disturb — or be
        // disturbed by — a sibling.
        let family = Path::new(QRMUX_FAMILY_ROOT).join(format!("reaper-selftest-{nanos}"));
        std::fs::create_dir_all(&family).unwrap();
        let prior = family.join("prior");
        std::fs::create_dir_all(&prior).unwrap();
        let current = family.join("current");
        std::fs::create_dir_all(&current).unwrap();

        // A guaranteed-DEAD owner pid for the prior dir: spawn `true`, reap it,
        // reuse its (now-exited) pid so the concurrency guard sees a dead owner.
        let mut dead = Command::new("/usr/bin/true").spawn().expect("spawn true");
        let dead_pid = dead.id();
        let _ = dead.wait();
        std::fs::write(prior.join("owner.pid"), format!("{dead_pid}\n")).unwrap();

        // Long-lived stand-in processes whose argv carries a unique marker. We use
        // `tail -f <markerfile>` (a marker-named real file): it blocks indefinitely
        // AND its argv contains the marker path, so the reaper's `ps args=` identity
        // check is exercised for real. (`sleep` can't carry an extra token — macOS
        // sleep rejects a second argument.) `tail` stands in for the daemon; the SUT
        // is the kill-by-identity decision.
        let pos_marker = format!("reaper_pos_marker_{nanos}");
        let pos_file = family.join(&pos_marker);
        std::fs::write(&pos_file, b"").unwrap();
        let mut positive = Command::new("/usr/bin/tail")
            .arg("-f")
            .arg(&pos_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn positive-control tail");
        let pos_pid = positive.id();
        let pos_identity = pos_marker.clone();

        // NEGATIVE control (pid-reuse impostor): a DIFFERENT live process recorded
        // against an identity NOT in its argv → MUST survive.
        let neg_marker = format!("reaper_neg_impostor_{nanos}");
        let neg_file = family.join(&neg_marker);
        std::fs::write(&neg_file, b"").unwrap();
        let mut negative = Command::new("/usr/bin/tail")
            .arg("-f")
            .arg(&neg_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn negative-control tail");
        let neg_pid = negative.id();
        let neg_identity = format!("wrong_identity_not_in_argv_{nanos}");

        std::thread::sleep(Duration::from_millis(300));

        let body = format!("{pos_pid}\t{pos_identity}\n{neg_pid}\t{neg_identity}\n");
        std::fs::write(prior.join("daemons.pids"), body).unwrap();

        let reaped = reap_in_family(&family, &current);

        std::thread::sleep(Duration::from_millis(250));

        // Liveness via `try_wait()`, NOT bare `kill -0`: these controls are our OWN
        // children, so a reaper-killed positive becomes a ZOMBIE until we wait it —
        // `kill -0` reports a zombie as "alive". `try_wait()` reaps and reports the
        // true exit, so `Some(_)` = exited (reaped), `None` = genuinely running.
        let pos_exited = positive.try_wait().ok().flatten().is_some();
        let neg_exited = negative.try_wait().ok().flatten().is_some();

        // Cleanup regardless of assertions: SIGKILL both (no-op if already dead),
        // then wait to reap any lingering zombie.
        let _ = Command::new("/bin/kill")
            .arg("-9")
            .arg(pos_pid.to_string())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("/bin/kill")
            .arg("-9")
            .arg(neg_pid.to_string())
            .stderr(Stdio::null())
            .status();
        let _ = positive.wait();
        let _ = negative.wait();
        // Remove the whole private family root (`prior` is removed by the reaper).
        let _ = std::fs::remove_dir_all(&family);

        assert!(
            pos_exited,
            "positive control (identity in argv, dead owner) MUST be reaped, pid {pos_pid} still running"
        );
        assert!(
            !neg_exited,
            "negative control (pid-reuse impostor) MUST survive, pid {neg_pid} was killed"
        );
        assert!(reaped >= 1, "reaper should report >=1 reaped; got {reaped}");
    }
}
