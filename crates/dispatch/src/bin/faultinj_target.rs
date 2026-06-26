//! `faultinj_target` — a controllable VICTIM process for the WS-R R3a-Step-0
//! fault-injection harness.
//!
//! This binary is the real process the injectors crash / wedge / race against.
//! It is NOT a `#[cfg(test)]` fixture: the harness drives the REAL dispatch
//! classifier/registry/reconcile code against a REAL OS process whose pid +
//! start-time the harness reads from `/proc`, exactly as the production
//! `(pid, start_ms)` identity path does.
//!
//! ## Protocol
//! On startup the target prints exactly ONE line to stdout and flushes it:
//!   `READY pid=<pid> start_ms=<start_ms_or_NONE>`
//! so the harness can record the victim's identity BEFORE driving a fault. Then
//! it behaves per its mode and otherwise blocks until killed.
//!
//! ## Modes (argv[1])
//! - (none) / `idle`  — print READY, then sleep forever (the CRASH/PID-REUSE/
//!   CONCURRENCY victim: a live registered process the harness SIGKILLs).
//! - `--wedge`        — print READY + one initial output line, then go fully
//!   silent forever while still alive (the alive-but-not-progressing case).
//! - `--longturn`     — print READY, then emit a per-line "progress" token every
//!   ~200ms forever (a HEALTHY long streaming turn — the RF-1 false-positive
//!   control; it is alive AND continuously producing output).
//! - `--sparse-output`— print READY + ONE line, then silent (quiet-reasoning).
//! - `--fork-child`   — print READY, fork a child that survives the parent, then
//!   the parent blocks (the CRASH-WITH-SURVIVING-CHILD inherited-fd case). The
//!   child prints `CHILD pid=<cpid>` so the harness can reap it.
//! - `--flock-hold <state_dir> <session_id>` — acquire the PRODUCTION liveness
//!   flock via `dispatch::livelock::LivenessLock::acquire` (the SAME primitive the
//!   daemon-lifetime path uses) and hold it for life. The load-bearing victim: a
//!   live holder reads `probe_dead == false`; after SIGKILL the kernel releases the
//!   lock and `probe_dead == true`. Reverting `acquire` to a no-op flips it.
//! - `--flock-fork-exec <state_dir> <session_id>` — acquire the flock, then fork a
//!   child that **execs** a fresh image (the realistic fleet case: an agent spawns
//!   a tool subprocess). The parent prints READY and blocks; the harness SIGKILLs
//!   the parent and the child survives. With the scoped-CLOEXEC fix the lock fd is
//!   `FD_CLOEXEC`, so the child's exec CLOSES it → parent death frees the lock
//!   (`probe_dead == true`, false-alive 0). With the rev0 blanket-clear the child
//!   inherits it across exec → `probe_dead == false` forever (false-alive ∞). The
//!   child prints `CHILD pid=<cpid>` before exec so the harness can reap it.
//! - `--flock-child-sleep` — the post-exec child image of `--flock-fork-exec`: it
//!   just survives, doing nothing with any inherited fd.
//! - `--ram-spike`    — print READY, then allocate toward the cgroup MemoryMax
//!   cap (the real RAM-wedge case; MUST be run inside a MemoryMax cgroup so the
//!   kernel kills it at the cap, never the box).
//!
//! Not shipped in the release `dispatch` binary — it is a separate bin only the
//! harness spawns.

use std::io::Write;
use std::time::Duration;

fn read_start_ms(pid: i32) -> Option<i64> {
    // Read start-time the SAME way the production identity path does, via the
    // dispatch crate's own effect, so the harness compares apples-to-apples.
    dispatch::effects::proc_start_ms(pid)
}

fn print_ready() {
    let pid = std::process::id() as i32;
    let start = read_start_ms(pid)
        .map(|m| m.to_string())
        .unwrap_or_else(|| "NONE".to_string());
    let mut out = std::io::stdout();
    let _ = writeln!(out, "READY pid={pid} start_ms={start}");
    let _ = out.flush();
}

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "" | "idle" => {
            print_ready();
            sleep_forever();
        }
        "--wedge" => {
            print_ready();
            // One initial output line, then total silence forever (alive but
            // not progressing — the real wedge).
            let mut out = std::io::stdout();
            let _ = writeln!(out, "OUTPUT turn-start");
            let _ = out.flush();
            sleep_forever();
        }
        "--longturn" => {
            print_ready();
            // A HEALTHY long streaming turn: emit output continuously. This is
            // the RF-1 false-positive control — it must NOT be classified
            // Wedged once signal-B exists.
            let mut out = std::io::stdout();
            let mut n: u64 = 0;
            loop {
                let _ = writeln!(out, "OUTPUT progress {n}");
                let _ = out.flush();
                n += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        "--sparse-output" => {
            print_ready();
            let mut out = std::io::stdout();
            let _ = writeln!(out, "OUTPUT one-line");
            let _ = out.flush();
            sleep_forever();
        }
        "--fork-child" => {
            // Fork a child that survives the parent. The harness SIGKILLs the
            // parent; the child must be reaped by the harness afterwards. The
            // child inherits any fds the parent opened (the false-alive-via-
            // inherited-flock case the R3a-Step-1 scoped-CLOEXEC fix closes).
            print_ready();
            // SAFETY: fork in a single-threaded program before any threads are
            // spawned; the child only writes its pid and sleeps.
            let child = unsafe { libc::fork() };
            if child == 0 {
                // Child: announce, then sleep forever (survives the parent).
                let cpid = std::process::id();
                let mut out = std::io::stdout();
                let _ = writeln!(out, "CHILD pid={cpid}");
                let _ = out.flush();
                sleep_forever();
            } else {
                // Parent: block until SIGKILLed by the harness.
                sleep_forever();
            }
        }
        "--ram-spike" => {
            print_ready();
            // Allocate toward the cgroup cap. MUST run inside a MemoryMax
            // cgroup: the kernel kills THIS process at the cap, never the box.
            // Touch each page so it is resident (not lazily reserved).
            let mut chunks: Vec<Vec<u8>> = Vec::new();
            loop {
                let mut chunk = vec![0u8; 16 * 1024 * 1024];
                for i in (0..chunk.len()).step_by(4096) {
                    chunk[i] = 1;
                }
                chunks.push(chunk);
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        "--flock-hold" => {
            // Acquire the PRODUCTION liveness flock (the SAME primitive the
            // daemon-lifetime path calls) and hold it for life. The harness asserts
            // probe_dead==false while alive and probe_dead==true after SIGKILL — the
            // load-bearing negative control (revert acquire → no-op → RED).
            let state_dir = std::env::args().nth(2).unwrap_or_default();
            let session_id = std::env::args().nth(3).unwrap_or_default();
            match dispatch::livelock::LivenessLock::acquire(
                std::path::Path::new(&state_dir),
                &session_id,
            ) {
                Ok(Some(held)) => {
                    print_ready();
                    let _held = held; // hold the fd for the process's whole life
                    sleep_forever();
                }
                Ok(None) => {
                    eprintln!("faultinj_target: --flock-hold: lock already held");
                    std::process::exit(3);
                }
                Err(e) => {
                    eprintln!("faultinj_target: --flock-hold: acquire failed: {e}");
                    std::process::exit(4);
                }
            }
        }
        "--flock-fork-exec" => {
            // Acquire the flock, then fork a child that EXECs a fresh image (the
            // realistic fleet tool-subprocess case). The scoped-CLOEXEC fix
            // (FD_CLOEXEC SET in acquire) makes the child's exec CLOSE the inherited
            // lock fd, so the parent's death frees the lock; the rev0 blanket-clear
            // would keep it held by the surviving child → false-alive.
            let state_dir = std::env::args().nth(2).unwrap_or_default();
            let session_id = std::env::args().nth(3).unwrap_or_default();
            match dispatch::livelock::LivenessLock::acquire(
                std::path::Path::new(&state_dir),
                &session_id,
            ) {
                Ok(Some(held)) => {
                    // Print the PARENT's READY line BEFORE forking, so the harness
                    // reads READY (parent pid) deterministically ahead of the
                    // child's CHILD line on the shared stdout.
                    print_ready();
                    // SAFETY: fork in a single-threaded program before any threads
                    // are spawned; the child only writes its pid and execs.
                    let child = unsafe { libc::fork() };
                    if child == 0 {
                        // CHILD: announce pid (so the harness can reap it), then
                        // EXEC a fresh image so FD_CLOEXEC (if set) takes effect on
                        // the inherited lock fd. exec does NOT run Drop, so the fd's
                        // fate is decided purely by CLOEXEC — exactly the property
                        // under test.
                        let cpid = std::process::id();
                        {
                            let mut out = std::io::stdout();
                            let _ = writeln!(out, "CHILD pid={cpid}");
                            let _ = out.flush();
                        }
                        let exe = std::fs::read_link("/proc/self/exe")
                            .ok()
                            .and_then(|p| p.to_str().map(str::to_string))
                            .unwrap_or_default();
                        if let Ok(exe_c) = std::ffi::CString::new(exe) {
                            let arg1 =
                                std::ffi::CString::new("--flock-child-sleep").unwrap();
                            let argv =
                                [exe_c.as_ptr(), arg1.as_ptr(), std::ptr::null()];
                            // SAFETY: execv with a NULL-terminated argv; on success
                            // it never returns. CLOEXEC-set fds are closed here.
                            unsafe {
                                libc::execv(exe_c.as_ptr(), argv.as_ptr());
                            }
                        }
                        // exec failed — survive anyway (the inherited-fd state is
                        // unchanged by a no-op child; the test depends on survival).
                        sleep_forever();
                    } else {
                        // PARENT: hold the lock and block until the harness SIGKILLs
                        // us. `held` stays bound (sleep_forever never returns), so
                        // the parent keeps the fd open until its death. READY was
                        // already printed before the fork.
                        let _held = held;
                        sleep_forever();
                    }
                }
                Ok(None) => {
                    eprintln!("faultinj_target: --flock-fork-exec: lock already held");
                    std::process::exit(3);
                }
                Err(e) => {
                    eprintln!("faultinj_target: --flock-fork-exec: acquire failed: {e}");
                    std::process::exit(4);
                }
            }
        }
        "--flock-child-sleep" => {
            // The post-exec child image of `--flock-fork-exec`: just survive. It
            // does nothing with any inherited fd; whether the lock is still held
            // depends solely on FD_CLOEXEC at acquire (the fix vs the rev0 clear).
            sleep_forever();
        }
        other => {
            eprintln!("faultinj_target: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}
