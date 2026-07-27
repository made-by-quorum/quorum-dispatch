//! `deobs_observe_target` — a controllable VICTIM process for the DE-observed
//! idempotency RED-TEAM (loop component 2, adversarial seat).
//!
//! This binary lets the red-team drive REAL process deaths against the REAL
//! `dispatch::telemetry::record_observed_in` code path, so the flock-release-on-
//! death and crash-between-claim-and-append properties are proven against a real
//! OS process — not simulated by an in-process RAII drop (which never exercises
//! the kernel's on-death lock reclamation).
//!
//! Gated behind the `deobs` feature (`required-features`) so it NEVER appears in
//! the release `qd` binary or the default build. The harness finds it via
//! `CARGO_BIN_EXE_deobs_observe_target` (Cargo sets that only when the bin builds).
//!
//! ## Modes (argv[1])
//! - `--race <state_dir> <host> <harness> <sid>` — call `record_observed_in`
//!   ONCE with default hooks (production behavior) and print the outcome:
//!   `RESULT ok=<true|false>` on Ok, `RESULT err=<msg>` on Err. The multi-PROCESS
//!   racer: N of these hammer the SAME key concurrently; the harness asserts the
//!   PHYSICAL stream holds exactly ONE line (write-time idempotency across real
//!   processes, not just threads — the reader-side-dedup-masquerade discriminator).
//!
//! - `--hold-in-section <state_dir> <host> <harness> <sid> <sentinel>` — enter
//!   `record_observed_in` and PIN the process IN the critical section: the
//!   `before_append` hook creates `<sentinel>` (signalling "I hold observed.lock,
//!   post-check, pre-append") and then blocks forever. The harness waits for the
//!   sentinel (deterministic: the child is provably past the under-lock check and
//!   holding the exclusive lock, with NO line appended yet), then SIGKILLs this
//!   process. That is the crash-between-claim-and-append case AND the wedged-holder
//!   case: the kernel must release the flock on death so the next caller records.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use dispatch::effects::FixedClock;
use dispatch::telemetry::{record_observed_in, RecordHooks};

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or_default();
    let clock = FixedClock(1_752_573_600_000);

    match mode {
        "--race" => {
            let state_dir = args.get(2).cloned().unwrap_or_default();
            let host = args.get(3).cloned().unwrap_or_default();
            let harness = args.get(4).cloned().unwrap_or_default();
            let sid = args.get(5).cloned().unwrap_or_default();
            let hooks = RecordHooks::default();
            let r = record_observed_in(
                Path::new(&state_dir),
                &clock,
                &host,
                &harness,
                &sid,
                None,
                &hooks,
            );
            let mut out = std::io::stdout();
            match r {
                Ok(appended) => {
                    let _ = writeln!(out, "RESULT ok={appended}");
                    let _ = out.flush();
                }
                Err(e) => {
                    let _ = writeln!(out, "RESULT err={e}");
                    let _ = out.flush();
                    std::process::exit(1);
                }
            }
        }
        "--hold-in-section" => {
            let state_dir = args.get(2).cloned().unwrap_or_default();
            let host = args.get(3).cloned().unwrap_or_default();
            let harness = args.get(4).cloned().unwrap_or_default();
            let sid = args.get(5).cloned().unwrap_or_default();
            let sentinel = args.get(6).cloned().unwrap_or_default();
            // The hook fires post-check, pre-append, WHILE holding observed.lock.
            // Create the sentinel there (so its existence PROVES we are in-section
            // and hold the lock), then block forever waiting for the SIGKILL.
            let hook = move || {
                // Best-effort: create + flush the sentinel, then hang.
                if let Ok(mut f) = std::fs::File::create(&sentinel) {
                    let _ = f.write_all(b"in-section\n");
                    let _ = f.flush();
                }
                sleep_forever();
            };
            let hooks = RecordHooks {
                before_append: Some(Box::new(hook)),
                fail_append: false,
            };
            // record_observed_in never returns here (the hook blocks forever);
            // the process dies only when the harness SIGKILLs it.
            let _ = record_observed_in(
                Path::new(&state_dir),
                &clock,
                &host,
                &harness,
                &sid,
                None,
                &hooks,
            );
            // Unreachable in the intended flow; if the hook ever returned, exit
            // loudly so a harness bug can't masquerade as a clean pass.
            eprintln!("deobs_observe_target: --hold-in-section hook returned unexpectedly");
            std::process::exit(3);
        }
        other => {
            eprintln!("deobs_observe_target: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}
