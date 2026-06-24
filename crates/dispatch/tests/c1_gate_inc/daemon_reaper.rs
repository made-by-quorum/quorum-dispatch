// ===========================================================================
// Teardown-leak belt — prefix-scoped reap at jail setup, per-target by pidfile.
//
// THE LEAK CLASS (live finding, orc-adopted option (b)): jailed sbmux daemons
// setsid (ppid 1) and outlive a SIGTERM'd / SIGKILL'd test runner — DaemonGuard
// Drop and Jail teardown NEVER run on an abrupt runner death, so the daemon
// orphans and survives past its jail. The N-session split multiplies this.
//
// BELT SHAPE (NOT a sweep — ADD-12-compatible, per-target by construction):
//   1. OWNER STAMP: every jail records the OWNING test-harness pid
//      (`std::process::id()`) into `<run-root>/owner.pid` at setup. This is the
//      CONCURRENCY DISCRIMINATOR: sibling tests in a live `cargo test` process
//      share the family root, so "is this a PRIOR (dead) run vs a CONCURRENT
//      (live) sibling?" is answered by "is the owner pid still alive?". The
//      reaper NEVER touches a dir whose owner is alive.
//   2. RECORD: every harness daemon spawn appends `pid<TAB>identity` to
//      `<run-root>/daemons.pids`. The identity is the daemon's `--socket-dir`
//      path (unique per run dir) — a pid-reuse impostor can't carry it in argv.
//      * Direct spawns (`start_daemon`) record at spawn (pid known immediately).
//      * ENGINE-cold-started daemons (`sb new` auto-launches them) are recorded
//        POST-BOOT via the exact-socket-dir `ps` lookup (`record_engine_daemons`,
//        mirroring `all_session_daemon_pids`) — per-target because the run dir
//        is unique to this jail.
//   3. REAP-AT-SETUP: when a NEW jail of this family initializes, scan the family
//      root (`/tmp/sb-c1gate-runs/*`) for run dirs whose OWNER PID IS DEAD (prior
//      runs); for each recorded `pid<TAB>identity`, kill ONLY if the daemon pid is
//      ALIVE *and* its CURRENT argv still carries the recorded identity (the
//      pid-reuse belt — never kill on pid alone), then remove the stale dir.
//   Bounded + best-effort: reap failures warn (eprintln) and NEVER fail the new
//   run's setup. The family root is a fixed short /tmp literal (jail invariant).
//
// This is INDEPENDENT of the sbmux test-lib `sweep_orphan_daemons` (ppid==1 +
// exact-binary-path sweep): that one is binary-scoped and fires per spawn; this
// one is run-dir-scoped, owner-gated, and identity-pinned by recorded pidfile.
// ===========================================================================

/// The fixed family root all c1_gate jails live under (see `Jail::establish`).
const C1GATE_FAMILY_ROOT: &str = "/tmp/sb-c1gate-runs";

/// Stamp the OWNING test-harness pid into `<run_root>/owner.pid`. The reaper uses
/// this to tell a PRIOR (dead-owner) run from a CONCURRENT (live-owner) sibling in
/// the shared family root. Best-effort: a failed write warns, never panics.
fn stamp_owner_pid(run_root: &Path) {
    let f = run_root.join("owner.pid");
    if let Err(e) = std::fs::write(&f, format!("{}\n", std::process::id())) {
        eprintln!("[daemon-reaper] warn: cannot write owner stamp {f:?}: {e}");
    }
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
/// write warns but never panics (the pidfile is a teardown convenience, not a
/// test assertion). `identity` MUST be a token that appears in the daemon's live
/// argv (we use its `--socket-dir`) so the reap-side identity check is sound.
fn record_daemon_pid(run_root: &Path, pid: u32, identity: &str) {
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

/// Record EVERY engine-cold-started per-session daemon bound to `dir` (the jail's
/// resolved socket dir) into `<run_root>/daemons.pids`. Uses the exact-socket-dir
/// `ps` lookup (`all_session_daemon_pids` shape) — `sb new` auto-launches these,
/// so the harness never sees their pid at spawn. Call AFTER the session is live.
/// Identity recorded is `dir` (carried in the daemon's `--socket-dir` argv).
#[allow(dead_code)]
fn record_engine_daemons(run_root: &Path, dir: &Path) {
    let out = match Command::new("/bin/ps").args(["-axo", "pid=,args="]).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[daemon-reaper] warn: ps failed recording engine daemons: {e}");
            return;
        }
    };
    let want_dir = dir.to_string_lossy();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_start();
        let Some((pid, args)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if args.contains("sbmux-server") && args.contains(want_dir.as_ref()) {
            if let Ok(p) = pid.trim().parse::<u32>() {
                record_daemon_pid(run_root, p, &want_dir);
            }
        }
    }
}

/// Production entry point: reap leaked daemons under the real family root.
/// (Thin wrapper over `reap_in_family` so the self-test can scan a PRIVATE family
/// root and never contend with concurrently-running real c1_gate jails.)
fn reap_prior_run_daemons(current_run_root: &Path) -> usize {
    reap_in_family(Path::new(C1GATE_FAMILY_ROOT), current_run_root)
}

/// Reap daemons leaked by PRIOR (dead-owner) runs under `family`. Scans
/// `family/*`, SKIPPING `current_run_root` and any dir whose `owner.pid` is still
/// alive (a concurrent sibling — NEVER touched). For each remaining (prior) dir's
/// `daemons.pids` entry, kills the daemon pid ONLY if it is alive AND its current
/// argv still carries the recorded identity (the pid-reuse belt), then removes the
/// stale dir. Best-effort: every failure warns, never fails setup. Returns the
/// count of daemons actually reaped (self-test).
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
        // at least one daemon. (A dead-owner dir with no daemons.pids is left for a
        // later cleanup pass; harmless and avoids racing dir creation.)
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

// ---------------------------------------------------------------------------
// Belt self-test (negative + positive control). Synthesizes a PRIOR run dir
// (dead owner pid) with a daemons.pids file and asserts the identity-pinned,
// owner-gated reap behavior. Uses its OWN family root so it can never disturb a
// concurrently-running c1_gate jail.
// ---------------------------------------------------------------------------

#[test]
fn daemon_reaper_identity_pinned_reap() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    // PRIVATE family root for this self-test: isolates it from concurrently-running
    // real c1_gate jails (whose setup-reapers scan the real family root) so the
    // controls are deterministic and the test can never disturb — or be disturbed
    // by — a sibling. Lives under the same /tmp base for the short-path invariant.
    let family = Path::new(C1GATE_FAMILY_ROOT).join(format!("reaper-selftest-{nanos}"));
    std::fs::create_dir_all(&family).unwrap();

    // A synthesized PRIOR run dir under the PRIVATE family root, stamped with a
    // DEAD owner pid so the concurrency guard treats it as a prior run.
    let prior = family.join("prior");
    std::fs::create_dir_all(&prior).unwrap();

    // A synthesized CURRENT run dir (must be skipped by the reaper).
    let current = family.join("current");
    std::fs::create_dir_all(&current).unwrap();

    // A guaranteed-DEAD pid for the owner stamp: spawn `true`, reap it, reuse its
    // pid number (it has exited, so pid_alive is false). Stamp prior with it.
    let mut dead = Command::new("/usr/bin/true").spawn().expect("spawn true");
    let dead_pid = dead.id();
    let _ = dead.wait();
    std::fs::write(prior.join("owner.pid"), format!("{dead_pid}\n")).unwrap();

    // Long-lived stand-in processes whose argv carries a unique marker. We use
    // `tail -f <markerfile>` (a marker-named real file): it blocks indefinitely AND
    // its argv contains the marker path, so the reaper's `ps args=` identity check
    // is exercised for real. (`sleep` can't carry an extra token — macOS sleep
    // rejects a second argument — so it is NOT a usable identity carrier here.)
    // `tail` is a stand-in for the daemon; the SUT is the kill-by-identity decision.
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
    // The recorded identity IS a substring of the positive's argv → reaped.
    let pos_identity = pos_marker.clone();

    // NEGATIVE control (the pid-reuse impostor): a DIFFERENT live process whose pid
    // we record against an identity NOT in its argv. The reaper must NOT kill it.
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
    // The recorded identity is NOT in the negative's argv (its argv carries
    // neg_marker, not this token) → the pid-reuse belt leaves it alive.
    let neg_identity = format!("wrong_identity_not_in_argv_{nanos}");

    // Give the children a moment to be visible to ps.
    std::thread::sleep(Duration::from_millis(300));

    // Write the prior run's daemons.pids:
    //  - positive: real pid + identity that IS in its argv  → MUST be reaped
    //  - negative: real pid + identity that is NOT in its argv → MUST survive
    let pidfile = prior.join("daemons.pids");
    let body = format!("{pos_pid}\t{pos_identity}\n{neg_pid}\t{neg_identity}\n");
    std::fs::write(&pidfile, body).unwrap();

    // Run the reaper over the PRIVATE family root, current = the run-dir to skip.
    let reaped = reap_in_family(&family, &current);

    // Let signals settle.
    std::thread::sleep(Duration::from_millis(250));

    // Liveness via `try_wait()`, NOT bare `kill -0`: these controls are our OWN
    // children, so a reaper-killed positive becomes a ZOMBIE until we wait it
    // (`kill -0` reports a zombie as "alive"). `try_wait()` reaps and reports the
    // true exit: `Some(_)` = exited (reaped), `None` = genuinely running.
    let pos_exited = positive.try_wait().ok().flatten().is_some();
    let neg_exited = negative.try_wait().ok().flatten().is_some();

    // Clean up our synthesized processes regardless of assertions: SIGKILL both
    // (no-op if already dead), then wait to reap any lingering zombie.
    let _ = Command::new("/bin/kill").arg("-9").arg(pos_pid.to_string()).stderr(Stdio::null()).status();
    let _ = Command::new("/bin/kill").arg("-9").arg(neg_pid.to_string()).stderr(Stdio::null()).status();
    let _ = positive.wait();
    let _ = negative.wait();
    // Remove the whole private family root (`prior` is removed by the reaper itself).
    let _ = std::fs::remove_dir_all(&family);

    // POSITIVE: identity matched + dead owner → reaped, process exited.
    assert!(
        pos_exited,
        "positive control (identity in argv, dead owner) MUST be reaped, but pid {pos_pid} still running"
    );
    // NEGATIVE: identity did NOT match → impostor SURVIVES (still running).
    assert!(
        !neg_exited,
        "negative control (pid-reuse impostor, wrong identity) MUST survive, but pid {neg_pid} was killed"
    );
    assert!(
        reaped >= 1,
        "reaper should report >=1 reaped (the positive control); got {reaped}"
    );
    // The reaper must have removed the stale prior run dir.
    assert!(
        !pidfile.exists(),
        "reaper must remove the stale prior run dir (pidfile {pidfile:?} still present)"
    );
}
