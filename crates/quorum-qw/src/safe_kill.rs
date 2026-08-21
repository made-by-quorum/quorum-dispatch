//! safe_group_kill — the ONLY place in the crate that issues a process-group /
//! negative-pid signal.
//!
//! Box-killer guard: `kill(-1)` (x==1) SIGKILLs every process the caller can
//! signal (the 2026-06-30 outage); `kill(0)` (x==0) signals the caller's own
//! process group. Reject x<=1 and cast-overflow.
//!
//! Every group-kill in the crate MUST route through here. The conformance lint
//! test below scans `src/` + `tests/` and FAILS if a raw `libc::kill(-…)` or
//! `libc::killpg(…)` appears anywhere OUTSIDE this file.

/// Issue a process-group signal to the group whose id is `x` (the helper negates
/// internally, so pass the positive pid/pgid). Hard no-op for `x <= 1` (rejects
/// the box-killer `kill(-1)`/`kill(0)` foot-guns) and for values that would
/// overflow the `i32` `pid_t` cast.
pub fn safe_group_kill(x: i64, sig: libc::c_int) {
    if x <= 1 || x > i32::MAX as i64 {
        return;
    }
    unsafe {
        libc::kill(-(x as i32), sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// True if `pid` is alive (or a zombie we haven't reaped) — `kill(pid, 0)`
    /// returns 0 while the pid exists and ESRCH once it is gone.
    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Reachability proof: the helper is a HARD no-op for x in {0, 1, -1} and it
    /// actually reaps a real process group for x > 1.
    ///
    /// We fork a harmless `sleep` sentinel into its OWN process group, then fire
    /// the three foot-gun values. If the guard were broken, `safe_group_kill(0,
    /// SIGKILL)` would signal THIS test's own process group and kill the test
    /// runner; surviving to the assertions IS the proof. Only the final real
    /// pgid call is allowed to reap the sentinel.
    #[test]
    fn noop_for_zero_one_negone_then_reaps_real_group() {
        let self_pid = std::process::id() as i32;

        // Sentinel child, leader of its own new process group (pgid == its pid).
        let mut child = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .expect("spawn sentinel sleep child");
        let child_pid = child.id() as i32;

        // Give the child a moment to be schedulable / group-established.
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            pid_alive(child_pid),
            "sentinel must be alive before the test"
        );

        // The three box-killer values — every one must be a NO-OP.
        for x in [0i64, 1i64, -1i64] {
            safe_group_kill(x, libc::SIGKILL);
        }
        // Let any (erroneous) signal be delivered before we check survival.
        std::thread::sleep(Duration::from_millis(150));

        assert!(
            pid_alive(self_pid),
            "GUARD BROKEN: the test process (or its group) was signalled by x in {{0,1,-1}}"
        );
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "GUARD BROKEN: sentinel child died from a no-op x in {{0,1,-1}}"
        );
        assert!(
            pid_alive(child_pid),
            "sentinel must still be alive after the no-ops"
        );

        // Now the real thing: reap the sentinel's group.
        safe_group_kill(child_pid as i64, libc::SIGKILL);

        // Reap and confirm death within a bounded settle window.
        let _ = child.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_alive(child_pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !pid_alive(child_pid),
            "sentinel must be DEAD after safe_group_kill(real_pgid, SIGKILL)"
        );
        // The test process itself is trivially still alive — we reached here.
        assert!(
            pid_alive(self_pid),
            "test process must survive the real group kill"
        );
    }

    /// Recursively collect every `.rs` file under `dir`.
    fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rs_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    /// Conformance lint: NO raw process-group signal may exist outside this file.
    /// Scans `src/` + `tests/` (from disk, at runtime) for `libc::kill(-` or
    /// `libc::killpg(` and FAILS naming any offender.
    #[test]
    fn no_raw_group_kill_outside_safe_kill() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // The one file allowed to hold the raw negative-pid syscall + these
        // literal search patterns.
        let allowed = crate_root.join("src").join("safe_kill.rs");

        // Assembled so this test's OWN source does not self-match the scan.
        let pat_kill = concat!("libc::", "kill(-");
        let pat_killpg = concat!("libc::", "killpg(");

        let mut files = Vec::new();
        rs_files(&crate_root.join("src"), &mut files);
        rs_files(&crate_root.join("tests"), &mut files);

        let mut offenders = Vec::new();
        for path in files {
            if path == allowed {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            for (lineno, line) in text.lines().enumerate() {
                if line.contains(pat_kill) || line.contains(pat_killpg) {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        lineno + 1,
                        line.trim()
                    ));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "raw process-group kill found OUTSIDE src/safe_kill.rs — route through \
             safe_kill::safe_group_kill:\n{}",
            offenders.join("\n")
        );
    }
}
