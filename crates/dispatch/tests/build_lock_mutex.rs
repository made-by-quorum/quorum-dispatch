//! build-lock.sh mutual-exclusion teeth (flake-rootcause pass, F-2 + orc-13 M-1).
//!
//! THE BUGS THIS PINS (two defective reclaim designs, both red against this
//! storm):
//! 1. ORIGINAL (`rm -rf $LOCK_DIR; continue`, pre-130b7c5): two waiters both
//!    pass the dead-pid check; the late one's rm deletes a successor's LIVE
//!    lock → concurrent holders. RED 3/3
//!    (flake-evidence/buildlock_prefix_red.txt).
//! 2. v1 FIX (mv-aside + putback, 130b7c5): mv is NAME-addressed, not
//!    instance-addressed — a waiter holding a one-beat-stale dead-verdict movs
//!    whatever instance NOW bears the name (a successor's live lock) out from
//!    under it; macOS mv-into-existing-dir then NESTS the victim's instance on
//!    putback. Caught by orc-13's independent re-run (M-1, 2-for-2) and
//!    reproduced here 6/10 (flake-evidence/buildlock_v1_mvaside_red.txt).
//!
//! THE FIX THIS GUARDS (v2): INSTANCE-PINNED marker CAS — `mkdir
//! $LOCK_DIR/reclaim` atomically claims the very instance examined (a
//! successor is a different dir; the marker dies with its instance), identity
//! via reclaim/by, deadness re-verified post-claim on the pinned instance,
//! and only then rm -rf. No mv, no aside, no putback. GREEN 0/25-failed
//! (flake-evidence/buildlock_v2_marker_green.txt; reps recorded with true
//! binary exit codes per the m-2 house rule).
//!
//! The critical-section detector is itself atomic (mkdir), so a violation can
//! never be missed for overlap ≥ the section length.
//!
//! Hermetic per Rule 9: SB_RUST_LOCK_DIR points at a per-test temp dir — the
//! real ~/.sb-rust is never touched.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Repo-root scripts/build-lock.sh (CARGO_MANIFEST_DIR = crates/sb).
fn build_lock_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/build-lock.sh")
        .canonicalize()
        .expect("scripts/build-lock.sh not found")
}

/// A guaranteed-DEAD pid: spawn /usr/bin/true, wait it, reuse its pid (the
/// daemon_reaper selftest pattern).
fn dead_pid() -> u32 {
    let mut child = Command::new("/usr/bin/true").spawn().expect("spawn true");
    let pid = child.id();
    let _ = child.wait();
    pid
}

/// The critical section each contender runs UNDER the lock. Overlap detection
/// is atomic: `mkdir cs` fails iff another holder is inside — that failure is
/// recorded as a violation. Each contender also stamps a done-line so the test
/// can assert everyone eventually ran.
const CRITICAL_SECTION: &str = r#"
d="$1"
if mkdir "$d/cs" 2>/dev/null; then
  sleep 0.25
  rmdir "$d/cs"
else
  echo "violation pid=$$" >> "$d/violations"
fi
echo "done pid=$$" >> "$d/done"
"#;

/// Spawn one build-lock.sh contender (stdout/stderr -> a per-contender log so
/// no pipe is ever held; bounded waits only — the .output() lesson).
fn spawn_contender(
    script: &PathBuf,
    lock_base: &PathBuf,
    work: &PathBuf,
    i: usize,
) -> std::process::Child {
    let log = std::fs::File::create(work.join(format!("contender-{i}.log"))).expect("log file");
    let log2 = log.try_clone().expect("clone log");
    Command::new("/bin/bash")
        .arg(script)
        .arg("/bin/sh")
        .arg("-c")
        .arg(CRITICAL_SECTION)
        .arg("sh")
        .arg(work)
        .env("SB_RUST_LOCK_DIR", lock_base)
        .env("SB_RUST_LOCK_TIMEOUT", "60")
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .expect("spawn build-lock.sh contender")
}

/// Bounded reap of all contenders (never an unbounded join).
fn reap_all(mut children: Vec<std::process::Child>, budget: Duration) {
    let t0 = Instant::now();
    while !children.is_empty() {
        children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        if children.is_empty() {
            break;
        }
        if t0.elapsed() > budget {
            for c in &mut children {
                let _ = c.kill();
                let _ = c.wait();
            }
            panic!("contenders did not finish within {budget:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_storm(tag: &str, pre_stale: bool) -> (usize, usize) {
    let root = std::env::temp_dir().join(format!(
        "buildlock-mutex-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let lock_base = root.join("lockbase");
    let work = root.join("work");
    std::fs::create_dir_all(&lock_base).unwrap();
    std::fs::create_dir_all(&work).unwrap();

    if pre_stale {
        // Plant a STALE lock: dead owner pid in the meta file. Every contender's
        // first probe takes the reclaim path simultaneously — the storm the old
        // code loses.
        let lock_dir = lock_base.join("build.lock");
        std::fs::create_dir_all(&lock_dir).unwrap();
        std::fs::write(
            lock_dir.join("owner"),
            format!("owner=test\npid={}\ntimestamp=t\n", dead_pid()),
        )
        .unwrap();
    }

    let script = build_lock_script();
    let n = 8;
    let children: Vec<_> = (0..n)
        .map(|i| spawn_contender(&script, &lock_base, &work, i))
        .collect();
    reap_all(children, Duration::from_secs(90));

    let done = std::fs::read_to_string(work.join("done"))
        .map(|s| s.lines().count())
        .unwrap_or(0);
    let violations = std::fs::read_to_string(work.join("violations"))
        .map(|s| s.lines().count())
        .unwrap_or(0);
    // Keep the dir on failure for forensics; clean on success.
    if violations == 0 && done == n {
        let _ = std::fs::remove_dir_all(&root);
    } else {
        eprintln!("[buildlock-mutex] forensics kept at {}", root.display());
    }
    (done, violations)
}

/// 8 contenders racing a PRE-PLANTED stale lock (dead owner): exactly the
/// double-reclaim storm. Old code: lock theft → overlapping critical sections.
/// Fixed code: zero violations, all 8 serialize and complete.
#[test]
fn stale_reclaim_storm_mutual_exclusion() {
    let (done, violations) = run_storm("stale", true);
    assert_eq!(
        violations, 0,
        "mutual exclusion violated {violations}x under stale-reclaim storm \
         (two holders ran concurrently — the double-reclaim race)"
    );
    assert_eq!(done, 8, "all contenders must eventually run ({done}/8 ran)");
}

/// Control: 8 contenders, NO stale lock (clean cold start). Pins that plain
/// contention (mkdir-race only, no reclaim path) also serializes — and that
/// the test harness itself doesn't manufacture violations.
#[test]
fn clean_contention_mutual_exclusion() {
    let (done, violations) = run_storm("clean", false);
    assert_eq!(
        violations, 0,
        "mutual exclusion violated {violations}x under clean contention"
    );
    assert_eq!(done, 8, "all contenders must eventually run ({done}/8 ran)");
}
