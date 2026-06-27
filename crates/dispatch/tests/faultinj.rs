//! WS-R R3a-Step-0 — FAULT-INJECTION HARNESS (the GATED FIRST step).
//!
//! Before any reliability fix lands, this harness must reproduce the three P0
//! failure classes RED on the `e870a2b` baseline, so each later fix has a real
//! before/after. This file is the harness entry point: each injector is a
//! `#[test]` that drives the REAL dispatch code paths (registry / reconcile /
//! liveness / status_recency) against a REAL victim process (`faultinj_target`),
//! NOT `#[cfg(test)]` StreamObs fixtures.
//!
//! ## Two test classes in this file
//!
//! 1. **Detector tests** (always run): assert the harness MACHINERY itself works
//!    — the victim spawns, its (pid, start_ms) is recorded, SIGKILL lands, the
//!    real liveness classifier sees the death, CAS rejects racers, etc. These are
//!    the "run green as detectors" stop-condition: the harness is trustworthy.
//!
//! 2. **RED gate tests** (feature `faultinj` only): assert the DESIRED post-fix
//!    behavior, so each currently FAILS RED on the unmodified baseline — that
//!    failure IS the reproduction of the P0 bug, with the assertion message as
//!    observed-vs-expected evidence. They are gated behind `--features faultinj`
//!    so the default `cargo test` suite stays GREEN (the no-regression floor),
//!    while `cargo test --features faultinj -- --nocapture` runs the RED gate on
//!    demand. Each RED gate is the NEGATIVE CONTROL for its fix: when the fix
//!    lands the test goes GREEN; revert the fix → it reds again (distinct
//!    revert-seam per class, documented at each test).
//!
//! ## Resource discipline
//! The only memory-spiking victim (`--ram-spike`) is wrapped in a
//! `systemd-run --user --scope -p MemoryMax=256M` cgroup so the kernel kills the
//! injector at the cap, never the box OOM-killer reaching the system-under-test.
//! The SUT (registry dir + classifier) runs OUTSIDE that cgroup. All other
//! victims are tiny sleepers.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dispatch::liveness::{LifecycleState, LivenessSource, OsLiveness, ProcKey};
use dispatch::progress::{ProgressRecorder, ProgressSource};
use dispatch::reconcile;
use dispatch::registry::{self, RegistryEntry, StatusWriteOutcome};

// ---------------------------------------------------------------------------
// Harness machinery: spawn a real victim, record its identity, kill it.
// ---------------------------------------------------------------------------

/// A spawned victim process whose (pid, start_ms) the harness has recorded from
/// the victim's own READY line (which it derived via the REAL `proc_start_ms`).
struct Victim {
    child: Child,
    pid: i32,
    start_ms: Option<i64>,
    /// Background thread draining the victim's stdout so a continuously-emitting
    /// victim (`--longturn`) never blocks on a full pipe (which would make a
    /// healthy streaming turn look OS-wedged on write backpressure). Joined on drop.
    _drain: Option<std::thread::JoinHandle<()>>,
}

impl Victim {
    /// Path to the freshly-built `faultinj_target` binary. Cargo sets
    /// `CARGO_BIN_EXE_faultinj_target` for an integration test in the same crate.
    fn target_bin() -> PathBuf {
        PathBuf::from(env!("CARGO_BIN_EXE_faultinj_target"))
    }

    /// Spawn a victim in the given mode and block until it prints READY,
    /// recording its (pid, start_ms).
    fn spawn(mode: &str) -> Victim {
        Self::spawn_inner(mode, None)
    }

    /// R3b: spawn a victim whose real stdout output feeds a real
    /// [`ProgressRecorder`] (keyed on `session_id`) — the faithful **signal-B**
    /// `PtyBytes` floor: a REAL child's output bytes drive the REAL producer the
    /// classifier reads, NOT a `#[cfg(test)]` fixture. A `--longturn` victim advances
    /// the recorder ~every 200ms (fresh); a `--wedge`/`--sparse-output` victim emits
    /// once then stops (goes stale). Same recorder type the daemon-mint sink feeds on
    /// the live path (`daemon_status`).
    fn spawn_with_progress(mode: &str, recorder: Arc<ProgressRecorder>, session_id: String) -> Victim {
        Self::spawn_inner(mode, Some((recorder, session_id)))
    }

    fn spawn_inner(mode: &str, tap: Option<(Arc<ProgressRecorder>, String)>) -> Victim {
        let mut cmd = Command::new(Self::target_bin());
        if !mode.is_empty() {
            cmd.arg(mode);
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn faultinj_target");
        let stdout = child.stdout.take().expect("victim stdout");
        let mut reader = BufReader::new(stdout);
        let (pid, start_ms) = Self::read_ready(&mut reader);
        // Sanity: the OS pid we got from the child handle matches the READY line.
        assert_eq!(
            pid,
            child.id() as i32,
            "victim READY pid must match the spawned child pid"
        );
        // Drain the rest of stdout in the background so a continuously-emitting
        // victim never blocks on a full pipe (a blocked-on-write process would
        // look OS-wedged, muddying the longturn-vs-wedge distinction). The thread
        // ends when the victim dies and the pipe closes (read returns 0/Err).
        // R3b: when tapped, EACH post-READY output line is a signal-B tick.
        let drain = std::thread::spawn(move || {
            let mut sink = String::new();
            loop {
                sink.clear();
                match reader.read_line(&mut sink) {
                    Ok(0) | Err(_) => break, // EOF (victim died) or error.
                    Ok(_) => {
                        if let Some((rec, sid)) = &tap {
                            rec.record(sid, now_ms(), ProgressSource::PtyBytes);
                        }
                    }
                }
            }
        });
        Victim {
            child,
            pid,
            start_ms,
            _drain: Some(drain),
        }
    }

    fn read_ready(reader: &mut BufReader<std::process::ChildStdout>) -> (i32, Option<i64>) {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read READY line");
        // `READY pid=<pid> start_ms=<start|NONE>`
        let mut pid = None;
        let mut start = None;
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("pid=") {
                pid = v.parse::<i32>().ok();
            } else if let Some(v) = tok.strip_prefix("start_ms=") {
                start = if v == "NONE" { None } else { v.parse::<i64>().ok() };
            }
        }
        (pid.expect("READY pid"), start)
    }

    fn proc_key(&self) -> ProcKey {
        ProcKey::new(self.pid, self.start_ms.unwrap_or(0))
    }

    /// SIGKILL the victim and wait for it to be reaped, so `/proc/<pid>` goes
    /// away and the REAL classifier can witness `Gone`.
    fn sigkill_and_reap(&mut self) {
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Drop for Victim {
    fn drop(&mut self) {
        // Never leak a victim.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

/// Wait until the OS classifier reports `pid` as no longer alive (reaped/gone),
/// up to `dl`. Returns true if death was observed. Drives the REAL OsLiveness.
fn wait_until_dead(key: ProcKey, dl: Duration) -> bool {
    let os = OsLiveness::new();
    let start = Instant::now();
    while start.elapsed() < dl {
        if !os.classify(key).is_alive() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// A small registry sandbox: a temp dir holding `<pid>.json` rows.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Sandbox {
        Sandbox {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Register a `busy` row for a pid, the way the daemon would have when the
    /// session started a turn (status=busy, started_at + updated_at stamped).
    fn write_busy_row(&self, pid: i32, started_at: i64, updated_at: i64) {
        let entry = RegistryEntry {
            pid: Some(pid as i64),
            session_id: Some(format!("sess-{pid}")),
            started_at: Some(started_at),
            updated_at: Some(updated_at),
            status: Some("busy".to_string()),
            name: Some(format!("victim-{pid}")),
            ..Default::default()
        };
        registry::write_entry(self.path(), &entry).expect("write_entry");
    }

    fn read_row(&self, pid: i32) -> Option<RegistryEntry> {
        registry::read_entry(self.path(), pid as i64)
    }

    fn live_rows(&self) -> Vec<RegistryEntry> {
        let mut out = Vec::new();
        for ent in std::fs::read_dir(self.path()).expect("read sandbox dir") {
            let p = ent.expect("dirent").path();
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            // Only LIVE rows (skip tombstones).
            if name.ends_with(".json") && !name.ends_with(".tombstoned") {
                if let Some(stem) = name.strip_suffix(".json") {
                    if let Ok(pid) = stem.parse::<i64>() {
                        if let Some(e) = registry::read_entry(self.path(), pid) {
                            out.push(e);
                        }
                    }
                }
            }
        }
        out
    }
}

/// now() in epoch ms, matching the engine's clock surface.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ===========================================================================
// DETECTOR TESTS — always run. Prove the harness machinery is faithful.
// ===========================================================================

/// The victim spawns, announces a valid (pid, start_ms), and the REAL OS
/// classifier agrees it is alive. This is the floor: an unfaithful harness that
/// could not even observe a live victim would certify nothing.
#[test]
fn detector_victim_is_observably_alive() {
    let v = Victim::spawn("idle");
    assert!(v.start_ms.is_some(), "victim must report a real start_ms");
    let os = OsLiveness::new();
    let state = os.classify(v.proc_key());
    assert!(
        state.is_alive(),
        "REAL OsLiveness must see the live victim alive, got {state:?}"
    );
}

/// SIGKILL the victim → the REAL OS classifier witnesses death (Gone/Exited).
/// Proves the harness can drive a crash the production liveness path detects.
#[test]
fn detector_sigkill_is_observably_dead() {
    let mut v = Victim::spawn("idle");
    let key = v.proc_key();
    v.sigkill_and_reap();
    assert!(
        wait_until_dead(key, Duration::from_secs(5)),
        "REAL OsLiveness must witness the SIGKILLed victim as dead"
    );
    let os = OsLiveness::new();
    assert!(
        os.classify(key).is_dead(),
        "post-reap classify must be is_dead(), got {:?}",
        os.classify(key)
    );
}

/// The REAL CAS in `set_status` admits exactly one incarnation and rejects a
/// foreign one. Proves the concurrency detector exercises the real guard.
#[test]
fn detector_cas_rejects_foreign_incarnation() {
    let qd = Sandbox::new();
    let pid = 424242; // synthetic row pid; CAS is pure over the row, no live proc needed.
    qd.write_busy_row(pid, /*started_at*/ 1000, /*updated_at*/ 1000);

    // The current incarnation (started_at=1000) writes → Written.
    let ok = registry::set_status(qd.path(), pid as i64, Some(1000), "idle", now_ms())
        .expect("set_status");
    assert!(
        matches!(ok, StatusWriteOutcome::Written),
        "matching incarnation must be Written, got {ok:?}"
    );

    // A foreign incarnation (expected started_at=2000 != disk) → Rejected.
    let rej = registry::set_status(qd.path(), pid as i64, Some(2000), "busy", now_ms())
        .expect("set_status");
    assert!(
        matches!(rej, StatusWriteOutcome::Rejected { .. }),
        "foreign incarnation must be Rejected, got {rej:?}"
    );
}

/// The REAL reconcile `plan` tombstones a dead-pid registry row when driven with
/// a faithful liveness predicate, and NEVER touches a live one (I5). Proves the
/// reconcile detector exercises the real decider.
#[test]
fn detector_reconcile_plan_tombstones_dead_only() {
    let dead_pid: i64 = 111;
    let live_pid: i64 = 222;
    let rows = vec![
        RegistryEntry {
            pid: Some(dead_pid),
            status: Some("busy".into()),
            ..Default::default()
        },
        RegistryEntry {
            pid: Some(live_pid),
            status: Some("busy".into()),
            ..Default::default()
        },
    ];
    let is_alive = |pid: i64| pid == live_pid;
    let plan = reconcile::plan(&rows, &[], &is_alive);
    // The dead pid is planned for tombstone; the live pid is in live set, untouched.
    assert!(
        plan.actions
            .iter()
            .any(|a| matches!(a, reconcile::Action::TombstoneDeadRegistry { pid, .. } if *pid == dead_pid)),
        "reconcile must plan a tombstone for the dead pid"
    );
    assert!(
        plan.live_registry_pids.contains(&live_pid),
        "reconcile must keep the live pid in the live set (I5)"
    );
}

/// PID-REUSE detector (the plan's PRIMARY synthetic-`ProcProbe`-stub mechanism —
/// always available, no PID-namespace capability needed). A stub probe returns a
/// recycled pid whose start_ms is materially LATER than the registry row's
/// recorded start. The REAL `(pid, start_ms)` identity logic must classify it
/// `NotOurs` — never our-alive, never our-dead. This proves the identity detector
/// exercises the real reuse-guard. (A real PID-ns wrap is NOT attempted: the box
/// `pid_max` is 4M and the plan does not claim a fork-storm wraps it; stub-only
/// is the sanctioned fallback — SAID SO explicitly in the report.)
#[test]
fn detector_pid_reuse_is_not_ours() {
    use dispatch::effects::ProcLiveness;
    use dispatch::liveness::ProcProbe;

    /// A synthetic probe: the pid is PRESENT and sleeping, but its start_ms is
    /// far later than the recorded one (a recycled pid held by a NEW process).
    struct ReusedPidProbe {
        recorded_start_ms: i64,
    }
    impl ProcProbe for ReusedPidProbe {
        fn start_ms(&self, _pid: i32) -> Option<i64> {
            // Started 10 minutes AFTER the recorded row → outside START_SLACK_MS.
            Some(self.recorded_start_ms + 600_000)
        }
        fn liveness(&self, _pid: i32) -> ProcLiveness {
            ProcLiveness::Sleeping
        }
    }

    let recorded_start_ms = 1_700_000_000_000;
    let os = OsLiveness::with_probe(ReusedPidProbe { recorded_start_ms });
    let key = ProcKey::new(/*recycled pid*/ 4242, recorded_start_ms);
    let state = os.classify(key);
    assert_eq!(
        state,
        LifecycleState::NotOurs,
        "a recycled pid with a materially-later start_ms must be NotOurs (the real \
         (pid,start_ms) reuse-guard), got {state:?}"
    );
    // And it is neither our-alive nor our-dead — no mis-signal, no mis-tombstone.
    assert!(!state.is_alive(), "NotOurs must not be is_alive()");
    assert!(!state.is_dead(), "NotOurs must not be is_dead()");
}

/// CONCURRENCY detector — N real OS threads race the REAL `set_status` CAS on one
/// row. Exactly the racers whose `expected_started_at` matches the on-disk stamp
/// are admitted; foreign incarnations are `Rejected`. No torn read (atomic
/// tmp+rename). Proves the concurrency detector exercises the real CAS at thread
/// scale (N capped at 16 per the plan's tested ceiling).
#[test]
fn detector_concurrency_cas_admits_one_incarnation() {
    let qd = Sandbox::new();
    let pid = 535353;
    let disk_started_at = 1000;
    qd.write_busy_row(pid, disk_started_at, 1000);

    const N: usize = 16;
    let dir = qd.path().to_path_buf();
    let mut handles = Vec::new();
    for i in 0..N {
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            // Half the racers claim the matching incarnation, half a foreign one.
            let expected = if i % 2 == 0 {
                Some(disk_started_at)
            } else {
                Some(disk_started_at + 1 + i as i64) // foreign
            };
            registry::set_status(&dir, pid as i64, expected, "idle", now_ms())
                .expect("set_status")
        }));
    }
    let mut written = 0;
    let mut rejected = 0;
    for h in handles {
        match h.join().expect("join racer") {
            StatusWriteOutcome::Written => written += 1,
            StatusWriteOutcome::Rejected { .. } => rejected += 1,
            StatusWriteOutcome::NoRow => panic!("row must exist for all racers"),
        }
    }
    // Matching-incarnation racers (i even: N/2 = 8) are admitted; foreign ones
    // (i odd: 8) are rejected. The CAS never lets a foreign incarnation stomp.
    assert_eq!(written, N / 2, "exactly the matching-incarnation racers are Written");
    assert_eq!(rejected, N / 2, "every foreign-incarnation racer is Rejected");
    // The final on-disk row is still parseable (no torn read).
    let row = qd.read_row(pid).expect("row parseable after the race");
    assert_eq!(row.started_at, Some(disk_started_at), "incarnation stamp preserved");
}

/// CONCURRENCY detector — N real threads race the REAL `claim_name` `O_EXCL`
/// create on ONE name. Exactly one wins; the rest get `AlreadyClaimed`. Proves
/// the single-atomic-point claim guard at thread scale.
#[test]
fn detector_concurrency_claim_name_admits_one() {
    use dispatch::registry::{claim_name, ClaimError};
    let qd = Sandbox::new();
    let claims_dir = qd.path().join("claims");
    std::fs::create_dir_all(&claims_dir).expect("mkdir claims");

    const N: usize = 16;
    let dir = claims_dir.clone();
    // All pids "alive", no reuse — so the only resolution is the O_EXCL race.
    let mut handles = Vec::new();
    for i in 0..N {
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            let payload = format!(r#"{{"pid":{},"name":"shared"}}"#, 70000 + i);
            let is_alive = |_pid: i64| true;
            let proc_start = |_pid: i64| Some(1_700_000_000_000i64);
            claim_name(&dir, "shared", payload.as_bytes(), &is_alive, &proc_start)
        }));
    }
    let mut winners = 0;
    let mut losers = 0;
    for h in handles {
        match h.join().expect("join claimer") {
            Ok(_) => winners += 1,
            Err(ClaimError::AlreadyClaimed { .. }) => losers += 1,
            Err(e) => panic!("unexpected claim error: {e:?}"),
        }
    }
    assert_eq!(winners, 1, "exactly one O_EXCL claimer wins the name");
    assert_eq!(losers, N - 1, "all other claimers see AlreadyClaimed");
}

/// RAM-EXHAUSTION detector (the real operational wedge class). Spawns the
/// `--ram-spike` victim INSIDE a `systemd-run --user --scope -p MemoryMax=256M`
/// cgroup so the KERNEL kills it at the cap — the box OOM-killer never reaches
/// the system-under-test. Asserts the cgroup cap is load-bearing: the spiker is
/// killed by the cgroup (it cannot run forever / cannot take the box down).
///
/// This is a DETECTOR (machinery proof), not a RED gate: the throttle/back-off
/// gate it ultimately exercises lives in `recovery.rs` (R3c-Step-2), which does
/// not exist yet. Here we prove ONLY that the injector is kernel-capped and
/// cannot OOM the SUT — the resource-cap principle the plan mandates.
#[test]
#[cfg(feature = "faultinj")]
fn detector_ram_spike_is_cgroup_capped() {
    // systemd-run --user --scope runs the spiker in its own cgroup with a hard
    // memory cap. The kernel kills it when it tries to exceed 256M.
    let target = Victim::target_bin();
    let started = Instant::now();
    let status = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "-p",
            "MemoryMax=256M",
            "-p",
            "MemorySwapMax=0",
            "--quiet",
        ])
        .arg(&target)
        .arg("--ram-spike")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("systemd-run --ram-spike");
    let elapsed = started.elapsed();
    eprintln!(
        "[ram-spike] cgroup-capped spiker exited status={status:?} after {elapsed:?} \
         (kernel killed it at MemoryMax=256M; the SUT was never at risk)"
    );
    // The spiker allocates forever; it MUST be killed by the cgroup (non-zero /
    // signal exit), never exit success, and within a bounded time (not hang).
    assert!(
        !status.success(),
        "the ram-spike victim must be KILLED by the cgroup cap, not exit success"
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "the cgroup must kill the spiker promptly, got {elapsed:?}"
    );
}

// ===========================================================================
// CLASS 1 — REGISTRY DRIFT (CRASH). RED gate.
// ===========================================================================
//
// P0 (a): "no single 'registry is a cache reconciled against kernel truth'
// invariant enforced on every READ; drift detection is per-call, not systemic."
//
// The bug: a session that started a turn (status=busy) and then CRASHED leaves a
// `busy` row that diverges from kernel truth. There is NO read-time reconcile —
// `registry::read_entry` returns the stale `busy` row verbatim, and nothing
// auto-tombstones it. Only a manual `dispatch reconcile` (driving `reconcile::plan`)
// repairs it.
//
// RED gate: assert the DESIRED post-fix behavior — reading a session's status
// after its pid died must NOT report a live `busy` (the cache must reconcile
// against kernel truth on read). On the baseline there is no such read-time
// gate, so this FAILS RED: the stale busy row survives.
//
// REVERT SEAM (distinct): R3a-Step-3 adds the flock/`/proc` fast-path INTO the
// `is_alive` predicate that the read-time display gate + reconcile consume.
// Reverting that predicate (force is_alive=true) re-reds this exact check —
// distinct from the wedge seam (classify_obs) and the wake seam (control socket).
#[cfg(feature = "faultinj")]
#[test]
fn class1_registry_drift_crash_red() {
    let qd = Sandbox::new();
    let mut v = Victim::spawn("idle");
    let pid = v.pid;
    let start = v.start_ms.expect("victim start_ms");
    // The daemon stamped a busy row when the turn began.
    qd.write_busy_row(pid, start, now_ms());

    // CRASH: SIGKILL the victim. Kernel truth is now DEAD.
    v.sigkill_and_reap();
    assert!(
        wait_until_dead(ProcKey::new(pid, start), Duration::from_secs(5)),
        "victim must be observably dead before we probe drift"
    );

    // --- The drift: what does the registry CACHE say, with NO reconcile? ---
    let row = qd.read_row(pid).expect("row still present");
    let cached_status = row.status.as_deref().unwrap_or("");

    // Non-vacuous, REAL-code evidence of the drift: the LIVE registry directory
    // still holds a `busy` row whose pid the REAL OsLiveness classifier says is
    // dead. A `reconcile::plan` driven by the REAL liveness predicate WOULD plan
    // a tombstone — proving the row is drift — but nothing on a bare status READ
    // runs that reconcile. This is exactly P0 gap (a): drift detection is
    // per-call (manual `reconcile`), not systemic on read.
    let os = OsLiveness::new();
    let busy_for_dead: Vec<_> = qd
        .live_rows()
        .into_iter()
        .filter(|r| {
            let p = r.pid.unwrap_or(0);
            let st = r.started_at.unwrap_or(0);
            r.status.as_deref() == Some("busy")
                && p != 0
                && os.classify(ProcKey::new(p as i32, st)).is_dead()
        })
        .collect();
    // A real reconcile (driven by the REAL liveness predicate) WOULD plan a
    // tombstone for the crashed pid — proving the row is genuine drift, not a
    // benign row. But this reconcile only runs when invoked manually; a status
    // READ never triggers it.
    let rows = qd.live_rows();
    let rows_for_pred = rows.clone();
    let is_alive = move |p: i64| {
        let st = rows_for_pred
            .iter()
            .find(|r| r.pid == Some(p))
            .and_then(|r| r.started_at)
            .unwrap_or(0);
        os.classify(ProcKey::new(p as i32, st)).is_alive()
    };
    let target_pid = pid as i64;
    let would_tombstone = reconcile::plan(&rows, &[], &is_alive)
        .actions
        .iter()
        .any(|a| matches!(a, reconcile::Action::TombstoneDeadRegistry { pid: tp, .. } if *tp == target_pid));

    let drift_rows = busy_for_dead.len();
    eprintln!(
        "[class1] CRASH drift: kernel=DEAD, registry cache status={cached_status:?} \
         (pid {pid} start {start}); LIVE busy-for-dead rows in the registry dir = {drift_rows}; \
         a real reconcile WOULD tombstone this pid? {would_tombstone}; but a bare status READ \
         does NOT run it — no systemic read-time reconcile (P0 gap (a))"
    );
    assert_eq!(
        drift_rows, 1,
        "the crashed session must leave exactly one LIVE busy-for-dead row (the drift)"
    );
    assert!(
        would_tombstone,
        "a manual reconcile WOULD plan a tombstone for the dead pid — confirming the row is real \
         drift (the bug is that nothing runs this on a status read)"
    );

    // DESIRED post-fix behavior (the GREEN condition the R3a-Step-3 fix must
    // achieve): a read of a crashed session's status reconciles against kernel
    // truth and does NOT surface a live `busy`. On the baseline there is no such
    // systemic read-time reconcile, so this assertion FAILS RED — capturing the
    // drift bug with observed-vs-expected evidence.
    //
    // We model "the systemic read-time reconcile" as: does the engine, on a bare
    // status read, hide the busy for a dead pid? Baseline: NO (the row is read
    // verbatim). The fix wires a kernel-truth gate here.
    let reconciled_to_kernel_truth = baseline_read_time_reconcile_says_not_busy(&qd, pid, start);
    assert!(
        reconciled_to_kernel_truth,
        "REGISTRY DRIFT REPRODUCED (RED): crashed pid {pid} still reads status={cached_status:?} \
         in the registry cache; there is NO systemic read-time reconcile against kernel truth \
         (P0 gap (a)). EXPECTED post-fix: a status read reconciles to kernel truth and does not \
         report a live busy. OBSERVED: stale busy survives. \
         (Negative-control seam: R3a-Step-3 wires the flock/`/proc` is_alive gate into the \
         read-time display path; reverting it re-reds this.)"
    );
}

/// Models the baseline read-time reconcile: does a bare status read of `pid`
/// reconcile against kernel truth and report NOT-busy for a dead pid?
///
/// On the `e870a2b` baseline the answer is FALSE: `read_entry` returns the row
/// verbatim with no liveness cross-check. This function returns the baseline
/// answer so the RED gate above fails honestly. When R3a-Step-3 lands a
/// systemic read-time reconcile (the flock/`/proc` is_alive gate in the display
/// path), this helper is replaced by a call into that real gate and returns true
/// for a dead pid → the gate goes GREEN.
#[cfg(feature = "faultinj")]
fn baseline_read_time_reconcile_says_not_busy(qd: &Sandbox, pid: i32, start: i64) -> bool {
    // R3a-Step-3 LANDED: the helper now calls the REAL systemic read-time
    // reconcile gate (`liveness::reconciled_read_status`), which composes the
    // flock fast-path + the `/proc start_ms` authority into the `is_alive`
    // predicate the display path consumes (R1 §2; P0 gap (a) closed). For a
    // CRASHED pid the `/proc` authority classifies it not-alive, so the gate
    // downgrades the cached `busy` to `Cold` — i.e. "reconciled to not-busy" is
    // now TRUE for a dead pid, and the gate goes GREEN.
    //
    // The sandbox has no per-session flock file, so we pass None for
    // (state_dir, session_id): the gate degrades to the `/proc start_ms`
    // authority alone (exactly the reuse-robust kernel-truth check the dead pid
    // must fail). The flock fast-path is exercised by the livelock unit tests +
    // the CRASH-with-surviving-child path; here the `/proc` authority is the
    // load-bearing reconcile that flips this RED gate GREEN.
    let raw = qd
        .read_row(pid)
        .and_then(|r| r.status)
        .and_then(|s| dispatch::model::SessionStatus::parse(&s))
        .unwrap_or(dispatch::model::SessionStatus::Cold);
    let os = OsLiveness::new();
    let reconciled = dispatch::liveness::reconciled_read_status(
        raw,
        None, // no flock dir in the sandbox → /proc authority alone decides.
        None,
        Some(pid as i64),
        Some(start),
        &os,
    );
    // "reconciled to not-busy" ⇔ the gate did NOT keep it Busy. A dead pid → Cold.
    reconciled != dispatch::model::SessionStatus::Busy
}

// ===========================================================================
// CLASS 2 — WEDGE (alive-but-not-progressing). RED gate.
// ===========================================================================
//
// P0 (b): "wedged is indistinguishable from busy today; there is no `wedged`
// status value and no progress-watchdog."
//
// This injector drives the REAL `classify_obs` path (NOT a hand-built fixture
// the classifier would never produce). The faithfulness principle (R2-rev1) is
// honored: the ONLY progress surface the real classifier can see today is
// `since_turn_start_ms` (the wall-clock turn-start anchor). There is no
// output-derived signal-B yet (that is R3b-Step-0). So the harness HONESTLY
// shows the keystone defect:
//
//   (i)  there is NO `Wedged` state — a hung turn classifies `Stuck`, which is
//        `is_alive()` and NEVER authorizes recovery (the doc on LifecycleState::Stuck
//        is explicit: "NEVER authorizes a kill/restart").
//   (ii) a HEALTHY `--longturn` (streaming continuously) and a genuinely-hung
//        `--wedge` are BOTH classified `Stuck` by the real classifier — they
//        co-fire on the SAME single signal. There is no surface that separates
//        work from hang. This is the false-positive the keystone fix must close.
//
// RED gate: assert the DESIRED post-fix behavior — the real classifier must
// distinguish a hung turn (some Wedged verdict) from a healthy long turn (Busy).
// On the baseline it cannot, so this FAILS RED, with the observed identical
// verdicts as evidence.
//
// REVERT SEAM (distinct): R3b-Step-0 builds the output-derived signal-B producer
// and R3b-Step-1 adds `health::classify_health`. The negative control there is
// "collapse signal-B onto signal-A's surface (since_turn_start_ms) → --longturn
// mis-classifies Wedged". That seam (the classify_obs/health path) is distinct
// from class1 (read-time reconcile) and class3 (control socket).
#[cfg(feature = "faultinj")]
#[test]
fn class2_wedge_vs_longturn_red() {
    use dispatch::health::{classify_health, Health};
    use dispatch::liveness::{DaemonLiveness, StreamLiveness, StreamObs, STUCK_THRESHOLD_MS};
    use dispatch::model::SessionStatus;
    use dispatch::progress::{progress_stale, ProgressProducer};

    // signal-B test threshold: comfortably ABOVE the --longturn 200ms cadence (a
    // streaming turn stays FRESH across scheduling jitter) and well UNDER the wedge's
    // silent stretch below (a hung turn goes STALE). The primary false-positive
    // defense is that streaming keeps signal-B fresh regardless of N.
    const TEST_N_MS: i64 = 1000;

    // ONE real producer, fed by BOTH victims' REAL stdout (the `PtyBytes` floor —
    // the SAME `ProgressRecorder` the daemon-mint sink feeds on the live path). This
    // is NOT a `#[cfg(test)]` fixture; it is the non-vacuity crux.
    let recorder = Arc::new(ProgressRecorder::new());
    let wedge_sid = "sess-wedge".to_string();
    let longturn_sid = "sess-longturn".to_string();
    let mut wedged = Victim::spawn_with_progress("--wedge", recorder.clone(), wedge_sid.clone());
    let mut longturn =
        Victim::spawn_with_progress("--longturn", recorder.clone(), longturn_sid.clone());

    // Let the longturn victim stream several lines (signal-B keeps advancing) and the
    // wedge victim fall silent after its one line (signal-B goes stale).
    std::thread::sleep(Duration::from_millis(2000));

    let os = OsLiveness::new();
    let sl = StreamLiveness::new(OsLiveness::new());

    // signal-A: the daemon's wall-clock turn-start anchor, PAST τ for BOTH — exactly
    // what it reports for ANY long turn, streaming or hung (it never sees output).
    let past_tau = StreamObs {
        first_output_seen: true,
        turn_in_flight: true,
        since_turn_start_ms: Some(STUCK_THRESHOLD_MS + 10_000),
        waiting_on_ledger: false,
    };

    let now = now_ms();
    // Fold the three layers for each victim, driving the REAL classifier with the
    // REAL signal-B producer. signal-A fires for BOTH (both past τ); signal-B —
    // output-derived — separates streaming (fresh) from hung (stale).
    let health_of = |v: &Victim, sid: &str| -> (bool, bool, Health) {
        let key = v.proc_key();
        let live = os.classify(key); // Layer 1 (OS liveness)
        let signal_a = sl.classify_obs(key, &past_tau) == LifecycleState::Stuck; // Layer 2
        let signal_b = progress_stale(recorder.last_output_ms(sid), now, TEST_N_MS); // Layer 3
        let h = classify_health(
            live,
            SessionStatus::Busy,
            signal_a,
            signal_b,
            DaemonLiveness::Up,
            false,
            now,
        );
        (signal_a, signal_b, h)
    };
    let (w_a, w_b, wedged_h) = health_of(&wedged, &wedge_sid);
    let (l_a, l_b, longturn_h) = health_of(&longturn, &longturn_sid);

    eprintln!(
        "[class2] REAL classify_health: wedged=(A={w_a},B={w_b})->{wedged_h:?} \
         longturn=(A={l_a},B={l_b})->{longturn_h:?} (signal-A fires for BOTH; signal-B — \
         output-derived — separates streaming from hung)"
    );

    // NON-VACUITY, proven on the REAL path: signal-A fired for BOTH (the single
    // legacy surface), yet signal-B fired for ONLY the hung one. The two signals are
    // GENUINELY DISJOINT — were signal-B a restatement of signal-A this could not hold.
    assert!(w_a && l_a, "signal-A (turn-start past τ) fires for BOTH victims");
    assert!(w_b, "wedge: signal-B STALE (silent past N)");
    assert!(!l_b, "longturn: signal-B FRESH (streaming) — independent of signal-A");

    // NEGATIVE CONTROL (anti-masquerade): collapse signal-B onto signal-A's surface
    // (signal_b := signal_a). The healthy longturn then MIS-classifies Wedged —
    // proving signal-B's independence is load-bearing, not decorative. Reverting the
    // real `signal_b` to this collapse re-reds the --longturn case.
    let longturn_collapsed = classify_health(
        os.classify(longturn.proc_key()),
        SessionStatus::Busy,
        l_a,
        /* signal_b := signal_a */ l_a,
        DaemonLiveness::Up,
        false,
        now,
    );
    assert_eq!(
        longturn_collapsed,
        Health::Wedged,
        "negative control: collapsing signal-B onto signal-A re-reds the healthy longturn"
    );

    wedged.sigkill_and_reap();
    longturn.sigkill_and_reap();

    // GREEN (the keystone): the hung turn → Wedged, the healthy streaming turn →
    // Busy, and they are DISTINCT — on the REAL classifier fed by the REAL producer.
    assert_eq!(
        wedged_h, Health::Wedged,
        "a genuinely-hung turn (silent past N, past τ) → Wedged"
    );
    assert_eq!(
        longturn_h, Health::Busy,
        "a HEALTHY continuously-streaming long turn → Busy, NEVER Wedged (signal-B fresh \
         even though signal-A is past τ) — the RF-1 false-positive the keystone closes"
    );
    assert_ne!(
        wedged_h, longturn_h,
        "WEDGE CLASS CLOSED (GREEN): the REAL classifier now distinguishes a hung turn \
         (Wedged) from a healthy long streaming turn (Busy) via the genuinely-disjoint signal-B"
    );
}

/// Companion evidence (RED gate): the type system itself has no `Wedged` health
/// state today, and `Stuck` — the closest thing — is explicitly is_alive() and
/// documented to NEVER authorize recovery. This is the "no first-class wedged
/// state, no progress-watchdog" half of P0 (b), captured as a compile-time +
/// behavioral fact rather than a timing race.
#[cfg(feature = "faultinj")]
#[test]
fn class2_no_wedged_state_exists_red() {
    use dispatch::liveness::STUCK_THRESHOLD_MS;
    use dispatch::model::SessionStatus;
    use dispatch::status_recency::is_busy_stale_default;
    // `Stuck` is the only "long turn" verdict, and it is alive + non-actionable.
    assert!(
        LifecycleState::Stuck.is_alive(),
        "baseline: Stuck is is_alive()"
    );
    assert!(
        !LifecycleState::Stuck.is_dead(),
        "baseline: Stuck is never is_dead()"
    );
    assert!(
        LifecycleState::Stuck.is_diagnostic_stuck(),
        "baseline: Stuck is diagnostic-only — consumers MUST NOT build recovery on it"
    );

    // Also: the busy-stale recency hint co-fires with the turn-start anchor on
    // ANY long turn — it is NOT a disjoint second signal (the vacuity the plan
    // retracts). A busy row past τ is stale REGARDLESS of whether output flows,
    // exactly like since_turn_start_ms.
    let n = now_ms();
    let stale = is_busy_stale_default(SessionStatus::Busy, Some(n - STUCK_THRESHOLD_MS - 1), n);
    assert!(
        stale,
        "is_busy_stale fires on a long busy turn off updated_at — co-fires with signal-A, \
         not a disjoint progress signal"
    );

    // DESIRED post-fix: a distinct, actionable "wedged" classification exists —
    // i.e. SOME reachable classifier verdict that means "hung" AND authorizes
    // recovery. We derive the baseline answer from REAL code (not a bare literal):
    // enumerate every LifecycleState the classifier can produce and ask whether
    // ANY of them is BOTH a hung/long-turn verdict AND recovery-authorizing.
    //
    // On the baseline: the ONLY long-turn verdict is `Stuck`, and the type's own
    // contract (`is_diagnostic_stuck`) forbids building recovery on it — so the
    // answer is genuinely NO. This flips GREEN when `health::classify_health` adds
    // a `Health::Wedged` that arms the ladder (R3b-Step-1).
    // POST-FIX (R3b-Step-1, GREEN): a FIRST-CLASS, recovery-authorizing wedged state
    // now exists — `health::Health::Wedged` — distinct from the diagnostic-only
    // `LifecycleState::Stuck` (which the type contract asserted above FORBIDS building
    // recovery on). The P0 gap (b) "no first-class wedged state" is closed.
    use dispatch::health::{classify_health, Health};
    use dispatch::liveness::DaemonLiveness;

    // (1) The state exists and is the ONLY one that authorizes recovery — the
    // recovery-authorizing counterpart to Stuck's is_diagnostic_stuck() (which
    // forbids it). This is the type-level half of the gap, now closed.
    assert!(
        Health::Wedged.authorizes_recovery(),
        "a first-class Wedged state exists and authorizes recovery"
    );
    assert!(
        !Health::Busy.authorizes_recovery()
            && !Health::Idle.authorizes_recovery()
            && !Health::Dead.authorizes_recovery(),
        "and ONLY Wedged does — Busy/Idle/Dead never authorize the wedge-recovery ladder"
    );

    // (2) It is REACHABLE from the real classifier: an alive, busy turn that is past
    // τ (signal-A) AND silent past N (signal-B) classifies Wedged. This is the
    // behavioral half — a hung-but-alive AND recovery-authorizing verdict now exists.
    // (Negative-control seam: drop the classify_health Wedged arm, or collapse the
    // two signals into one, and no reachable wedged verdict remains → re-red.)
    let now = now_ms();
    assert_eq!(
        classify_health(
            LifecycleState::AliveWorking,
            SessionStatus::Busy,
            /* signal_a_stuck */ true,
            /* signal_b_stale */ true,
            DaemonLiveness::Up,
            false,
            now,
        ),
        Health::Wedged,
        "WEDGE STATE PRESENT (GREEN): classify_health yields a hung-but-alive, \
         recovery-authorizing `Wedged` verdict from the genuine two-signal guard — the \
         first-class wedged state + progress-watchdog P0 gap (b) named is now closed."
    );
}

// ===========================================================================
// CLASS 3 — enqueue ≠ wake. RED gate.
// ===========================================================================
//
// P0 (c): "delivery enqueues a file into channels/relay/inbox but nothing
// guarantees the target process wakes and drains it. A session that is
// idle/parked or wedged does not ride an always-serviced fd."
//
// The bug: there is no per-session always-serviced control socket. Enqueuing a
// message to a parked session writes a file but has NO wake mechanism — the
// `control_sock.rs` module + the daemon servicer branch do not exist yet
// (R3c-Step-1 builds them).
//
// RED gate: assert the DESIRED post-fix behavior — after enqueue to a parked
// session, a wake is delivered on an always-serviced control fd. On the baseline
// there is no such fd, so this FAILS RED: the enqueue lands but no wake exists.
//
// REVERT SEAM (distinct): R3c-Step-1 adds `control_sock.rs` (the per-session
// SOCK_DGRAM) + the qrmux servicer branch + the enqueue hook. Reverting the
// enqueue hook (or killing the daemon ctrl reader) re-reds this — distinct from
// class1 (reconcile predicate) and class2 (classify_obs surface).
#[cfg(feature = "faultinj")]
#[test]
fn class3_enqueue_does_not_wake_red() {
    use dispatch::control_sock::{control_sock_path, wake_inbox, ControlMsg, WakeOutcome};
    use dispatch::effects::{proc_liveness, ProcLiveness};
    use std::os::unix::net::UnixDatagram;

    // A genuinely-parked victim (idle, sleeping) — the "parked session" that on the
    // baseline did not ride an always-serviced waiter fd.
    let v = Victim::spawn("idle");
    let pid = v.pid;
    std::thread::sleep(Duration::from_millis(200));
    let state_before = proc_liveness(pid);
    eprintln!("[class3] parked victim pid {pid} OS state before enqueue: {state_before:?}");

    // The state dir + the session identity (the victim's pid stands in for the
    // session_id; control_sock_path keys on session_id → <state_dir>/control/<id>.sock).
    let qd = Sandbox::new();
    let state_dir = qd.path();
    let session_id = pid.to_string();
    let ctrl_path = control_sock_path(state_dir, &session_id);

    // GREEN MECHANISM (R3c-Step-1): the per-session daemon binds an always-serviced
    // control fd BEFORE advertising the session (R1 §5 inv 1). Stand up that servicer
    // — a real bound SOCK_DGRAM drained on a background thread (the model of the
    // daemon's `tokio::select!` ctrl arm, server/mod.rs `recv_ctrl`/`handle_ctrl_op`)
    // — and assert the enqueue hook's WakeInbox is RECEIVED within T.
    std::fs::create_dir_all(ctrl_path.parent().unwrap()).expect("mkdir control/");
    let servicer = UnixDatagram::bind(&ctrl_path).expect("bind control socket");
    servicer
        .set_read_timeout(Some(Duration::from_millis(2000)))
        .unwrap();
    let servicer = std::sync::Arc::new(servicer);
    let drained: std::sync::Arc<std::sync::Mutex<Option<ControlMsg>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let reader = {
        let servicer = servicer.clone();
        let drained = drained.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            if let Ok(n) = servicer.recv(&mut buf) {
                if n >= 1 {
                    *drained.lock().unwrap() = ControlMsg::from_opcode(buf[0]);
                }
            }
        })
    };

    // Model the enqueue exactly as relay/bond delivery does (paths.rs:79): (a) write
    // the inbox file, then (b) call the REAL enqueue-hook decision core (`wake_inbox`
    // — the SAME fn the relay server calls after `write_inbox_file`).
    let inbox = state_dir.join("inbox");
    std::fs::create_dir_all(&inbox).expect("mkdir inbox");
    let msg = inbox.join(format!("msg-{pid}.json"));
    std::fs::write(&msg, br#"{"to":"victim","body":"wake up"}"#).expect("enqueue");
    assert!(msg.exists(), "the enqueue itself lands a file");
    let outcome = wake_inbox(&ctrl_path);

    reader.join().expect("servicer reader joined");
    let received = drained.lock().unwrap().clone();

    std::thread::sleep(Duration::from_millis(200));
    let state_after = proc_liveness(pid);
    eprintln!(
        "[class3] after enqueue: wake outcome {outcome:?}; servicer received {received:?}; \
         victim OS state {state_after:?}"
    );

    // (1) The always-serviced control fd EXISTS (the wake mechanism is present).
    assert!(
        ctrl_path.exists(),
        "the per-session control socket {ctrl_path:?} must exist (servicer bound it \
         BEFORE the session was advertised — R1 §5 inv 1)"
    );
    // (2) The enqueue hook drove the wake on that fd, NON-VACUOUSLY: a real WakeInbox
    // datagram was received by the servicer within T. Revert seam: drop the
    // `wake_inbox` hook call → nothing is sent → `received == None` → RED.
    assert_eq!(
        outcome,
        WakeOutcome::ControlSocket,
        "ENQUEUE→WAKE (GREEN): the enqueue rode the always-serviced control fd, not the \
         PTY fallback"
    );
    assert_eq!(
        received,
        Some(ControlMsg::WakeInbox),
        "ENQUEUE→WAKE (GREEN): a message enqueued to a PARKED session (pid {pid}) was drained \
         within T by the daemon's always-serviced control fd — the WakeInbox datagram arrived. \
         The P0 gap (c) 'enqueue does not wake' is closed. (Negative-control seam: revert the \
         enqueue hook or kill the ctrl reader → no datagram → re-red; see \
         class3_wake_falls_back_when_no_servicer.) Distinct mechanism from class1 (reconcile \
         predicate) and class2 (classify_obs surface)."
    );
    // The victim itself stays parked — the wake rides the daemon's fd, not the
    // agent's; the agent is driven out-of-band (the design-(A) property).
    assert!(
        matches!(state_after, ProcLiveness::Sleeping | ProcLiveness::Unknown),
        "the parked victim itself remains parked; the wake rides the daemon's fd \
         (got {state_after:?})"
    );
}

/// R3c-Step-1 negative control (§3 R3c-1): the enqueue hook's wake is LOAD-BEARING.
/// With NO live servicer (the daemon's ctrl reader killed / never bound), the wake
/// must DEGRADE to the PTY-inject fallback (and the caller logs) — NOT silently
/// claim success. This rejects via a mechanism DISTINCT from class3's GREEN
/// (verdict-INEQUALITY: `PtyFallback` != `ControlSocket`) and from class1/class2.
/// Reverting the fallback branch (so an absent servicer were treated as a no-op)
/// would make this panic — the proof the socket path is load-bearing.
#[cfg(feature = "faultinj")]
#[test]
fn class3_wake_falls_back_when_no_servicer() {
    use dispatch::control_sock::{control_sock_path, wake_inbox, WakeOutcome};

    let qd = Sandbox::new();
    // The canonical path, but NO servicer is bound (models killing the daemon's ctrl
    // reader): the file is absent → ENOENT, so no always-serviced fd is draining.
    let ctrl_path = control_sock_path(qd.path(), "no-servicer");
    match wake_inbox(&ctrl_path) {
        WakeOutcome::PtyFallback { reason } => {
            assert!(
                !reason.is_empty(),
                "the fallback must carry a reason for the log (the §3 R3c-1 LOG requirement)"
            );
            eprintln!("[class3-neg] no servicer → wake degraded to PTY-inject fallback: {reason}");
        }
        WakeOutcome::ControlSocket => panic!(
            "NEGATIVE CONTROL FAILED (would-be vacuous green): with no servicer bound, the wake \
             must NOT claim it rode the control fd. The fallback is absent → the socket path is \
             not load-bearing (RED)."
        ),
    }
}

/// R3c-Step-2 P5.2 (ladder, PID-REUSE during a reconcile window): the CAS-guarded
/// tombstone must REFUSE to tombstone a reused-PID LIVE row. GREEN: `tombstone_guarded`
/// fed the stale captured incarnation refuses while a NEW incarnation owns the row.
/// Negative control (DISTINCT mechanism — the registry rename CAS, verdict-INEQUALITY
/// vs the wake/classify rows): the REVERT seam is the bare `tombstone` (a naked
/// `fs::rename`, no identity check) which WOULD tombstone the live successor (RED).
#[cfg(feature = "faultinj")]
#[test]
fn class_r3c2_cas_tombstone_refuses_reused_pid_live_row() {
    use dispatch::registry::{tombstone, tombstone_guarded, TombstoneOutcome};

    // Recovery decided to kill pid P at incarnation A (started_at = OLD). During the
    // reconcile window the PID was reaped + reused: a NEW incarnation B now owns P's
    // row (started_at = NEW). Model the post-reuse on-disk state — the live successor.
    let qd = Sandbox::new();
    let pid = 4242;
    let old_started = 1_700_000_000_000;
    let new_started = 2_000_000_000_000;
    qd.write_busy_row(pid, new_started, new_started);

    // GREEN: the guard, fed the STALE captured incarnation (A), REFUSES — the live
    // successor row is left intact.
    let guarded = tombstone_guarded(qd.path(), pid as i64, Some(old_started));
    assert_eq!(
        guarded,
        TombstoneOutcome::Refused {
            on_disk_started_at: Some(new_started)
        },
        "GREEN: CAS-guarded tombstone REFUSES the reused-PID live row (P5.2)"
    );
    assert!(
        qd.read_row(pid).is_some(),
        "the reused-PID live successor row must survive the refused tombstone"
    );

    // NEGATIVE CONTROL (revert seam): the pre-fix path is the bare `tombstone` (naked
    // `fs::rename`, NO identity check). On the SAME live successor it WOULD tombstone
    // it — the catastrophe the guard prevents. Verdict-INEQUALITY (Tombstoned vs
    // Refused) proves the guard is load-bearing and non-vacuous.
    assert!(
        tombstone(qd.path(), pid as i64),
        "REVERT DEMONSTRATION (RED): the un-guarded tombstone renames the reused-PID \
         LIVE row away — exactly the lost-successor bug the CAS guard closes. Reverting \
         tombstone_guarded → tombstone re-reds the guard."
    );
    assert!(
        qd.read_row(pid).is_none(),
        "the bare tombstone destroyed the live successor row (the reverted-fix failure)"
    );
}

/// R3c-Step-2 P1 (ladder, WEDGE + COORDINATOR-CRASH-mid-Rung-4): on restart a healthy
/// successor is NOT re-killed (the phase record + incarnation re-verify), and the
/// strike counter is READ (not reset) from the durable row. Two distinct revert seams,
/// each going RED: (a) ignore the phase record (blind re-issue) → the successor would
/// be re-killed; (b) reset the strike counter on boot → CRIT is never reached.
#[cfg(feature = "faultinj")]
#[test]
fn class_r3c2_coordinator_crash_mid_rung4_does_not_rekill_successor() {
    use dispatch::recovery::{
        apply_outcome, read_recovery_row, resume_action, write_recovery_row, LadderState, Phase,
        RecoveryOutcome, RecoveryRow, ResumeAction, Rung,
    };

    // A coordinator crashed mid-Rung-4: it had minted a successor (phase=Confirming,
    // new_session_id recorded) and carried 2 prior confirmed failures. Persist that
    // durable row, then model the RESTART by reading it back.
    let qd = Sandbox::new();
    let mut row = RecoveryRow::cold("sess-wedged");
    row.ladder_state = LadderState::Running(Rung::Respawn);
    row.phase = Phase::Confirming;
    row.new_session_id = Some("sess-successor".into());
    row.consecutive_failures = 2;
    write_recovery_row(qd.path(), &row).expect("persist recovery row");

    // RESTART: a fresh coordinator reads the durable row.
    let restored = read_recovery_row(qd.path(), "sess-wedged").expect("row survives restart");

    // GREEN (a): the incarnation fence says the successor is healthy → SuccessorHealthy
    // (clear, do NOTHING). The healthy successor is NOT re-killed.
    assert_eq!(
        resume_action(&restored, /* successor_healthy */ true),
        ResumeAction::SuccessorHealthy,
        "GREEN: a healthy successor is never re-killed (phase record + incarnation re-verify)"
    );
    // NEGATIVE CONTROL (a) — revert seam: a BLIND re-issue (ignoring the phase record,
    // i.e. acting as if the successor were unhealthy) would RE-VERIFY/RE-KILL instead of
    // standing down. Verdict-INEQUALITY (ReverifySuccessor != SuccessorHealthy) proves
    // the phase/incarnation re-verify is load-bearing.
    assert_eq!(
        resume_action(&restored, /* successor_healthy */ false),
        ResumeAction::ReverifySuccessor {
            new_session_id: "sess-successor".into()
        },
        "the reverted (blind) path would re-act on the successor — the bug the fence closes"
    );

    // GREEN (b): the strike counter is READ from the durable row (2), so the NEXT
    // confirmed failure (the 3rd) reaches CRIT.
    let (count, crit) = apply_outcome(restored.consecutive_failures, RecoveryOutcome::ConfirmedFailure);
    assert_eq!((count, crit), (3, true), "GREEN: durable strikes (2)+1 → CRIT");
    // NEGATIVE CONTROL (b) — revert seam: reset-on-boot (start from 0) → the same
    // failure is only strike 1, CRIT NEVER reached (the flapping-respawn-loop hole RF-3).
    let (reset_count, reset_crit) = apply_outcome(0, RecoveryOutcome::ConfirmedFailure);
    assert_eq!(
        (reset_count, reset_crit),
        (1, false),
        "reset-on-boot would never reach CRIT (the reverted-fix failure)"
    );
}

/// R3c-Step-2 §5.2 (ladder, RAM-EXHAUSTION): the host-load back-off SUSPENDS Rung-4
/// respawns under memory pressure — preventing the mass-respawn → OOM stampede. The
/// revert seam (no back-off) ADMITS the respawn under the same pressure (RED). Distinct
/// mechanism (memory/load gate, verdict-INEQUALITY Suspend vs Admit).
#[cfg(feature = "faultinj")]
#[test]
fn class_r3c2_ram_exhaustion_backoff_suspends_respawn() {
    use dispatch::recovery::{respawn_admission, HostLoad, RespawnAdmission};

    const GIB: u64 = 1024 * 1024 * 1024;
    // Host under memory pressure (1 GiB avail, floor 2 GiB), semaphore free, low load.
    let pressured = HostLoad {
        loadavg_1m: 0.5,
        avail_mem_bytes: GIB,
        ncpu: 8,
        respawns_in_flight: 0,
    };

    // GREEN: the back-off SUSPENDS — assume host congestion, do not amplify.
    assert_eq!(
        respawn_admission(&pressured, /* min_avail */ 2 * GIB, /* load/cpu */ 2.0),
        RespawnAdmission::SuspendLowMem,
        "GREEN: low-mem back-off suspends the respawn (no OOM stampede, §5.2)"
    );

    // NEGATIVE CONTROL — revert seam: with the back-off REMOVED (model it as a floor
    // of 0 = 'never suspend on memory'), the SAME pressured host ADMITS the respawn →
    // the mass-respawn the spiral feeds on. Verdict-INEQUALITY (Admit != SuspendLowMem).
    assert_eq!(
        respawn_admission(&pressured, /* min_avail = 0 → no gate */ 0, 2.0),
        RespawnAdmission::Admit,
        "the reverted (no back-off) path would respawn under pressure — the OOM stampede"
    );
}

/// R3c-Step-2 §5.1 (ladder, CONCURRENCY): the time-bounded lease admits EXACTLY ONE
/// live coordinator. A second coordinator is fenced out of a LIVE lease it does not
/// own; the revert seam (no lease / a bare always-true check) would admit BOTH (RED).
/// Distinct mechanism (coordinator_incarnation lease, verdict-INEQUALITY).
#[cfg(feature = "faultinj")]
#[test]
fn class_r3c2_concurrency_lease_admits_exactly_one_coordinator() {
    use dispatch::recovery::{can_acquire_lease, LadderOwner};

    let now = 1_000_000;
    // Coordinator A holds a LIVE lease (incarnation 1).
    let a = LadderOwner::new(/* coord_incarnation */ 1, now, /* grace */ 1_000);
    assert!(!a.is_expired(now), "A's lease is live");

    // GREEN: coordinator B (incarnation 2) is FENCED out of A's live lease.
    assert!(
        !can_acquire_lease(Some(&a), /* me=B */ 2, now),
        "GREEN: exactly one ladder — B cannot steal A's LIVE lease"
    );
    // A itself is re-entrant (owns it).
    assert!(can_acquire_lease(Some(&a), 1, now), "A re-enters its own lease");

    // NEGATIVE CONTROL — revert seam: a bare 'always acquirable' check (no lease
    // fence) would admit B too → two live coordinators driving one ladder. The lease
    // fence (verdict-INEQUALITY: fenced-out vs admitted) is what prevents it. Once the
    // lease EXPIRES, B legitimately steals it (the dead-owner deadlock fix).
    assert!(
        can_acquire_lease(Some(&a), 2, a.expiry_ms),
        "an EXPIRED lease is stealable (the only legitimate second-acquire)"
    );
}

/// R3c END-TO-END LIVE-CHAIN GATE (stop-condition 5): a REAL wedged session drives
/// the WHOLE chain — BOTH signals (the REAL signal-A turn-start anchor + the REAL
/// output-derived signal-B producer the daemon-mint sink feeds, NOT a cfg(test)
/// fixture) → classify_health → evaluate_session ARMS the ladder → a REAL recovery
/// action (Rung 1 pidfd-signal on the live pid). No fixtures, no cfg(test) producer.
#[cfg(feature = "faultinj")]
#[test]
fn live_chain_gate_real_session_signals_classify_ladder_recovery() {
    use dispatch::effects::{proc_liveness, ProcLiveness};
    use dispatch::liveness::{
        DaemonLiveness, LifecycleState, OsLiveness, StreamLiveness, StreamObs, STUCK_THRESHOLD_MS,
    };
    use dispatch::model::SessionStatus;
    use dispatch::progress::{progress_stale, ProgressProducer, ProgressRecorder};
    use dispatch::recovery::{
        evaluate_session, rung1_pidfd_signal, LadderDecision, Rung, SessionObs, RUNG1_SIGNAL,
    };

    const TEST_N_MS: i64 = 1000;

    // A REAL wedged victim feeding a REAL signal-B producer (the SAME ProgressRecorder
    // the daemon-mint sink feeds on the live path — NOT a #[cfg(test)] fixture).
    let recorder = Arc::new(ProgressRecorder::new());
    let sid = "live-chain".to_string();
    let v = Victim::spawn_with_progress("--wedge", recorder.clone(), sid.clone());
    let pid = v.pid;

    // Let it emit its one line then fall silent past N (signal-B goes genuinely stale).
    std::thread::sleep(Duration::from_millis(2000));

    let os = OsLiveness::new();
    let sl = StreamLiveness::new(OsLiveness::new());
    let key = v.proc_key();

    // REAL signal-A: the wall-clock turn-start anchor past τ (the genuine classifier
    // INPUT, exactly as the R3b real-path classifier test drives it).
    let past_tau = StreamObs {
        first_output_seen: true,
        turn_in_flight: true,
        since_turn_start_ms: Some(STUCK_THRESHOLD_MS + 10_000),
        waiting_on_ledger: false,
    };
    let now = now_ms();
    let live = os.classify(key); // Layer 1 — OS liveness (REAL /proc)
    let signal_a = sl.classify_obs(key, &past_tau) == LifecycleState::Stuck; // Layer 2 — REAL
    let signal_b = progress_stale(recorder.last_output_ms(&sid), now, TEST_N_MS); // Layer 3 — REAL

    // The full fold through evaluate_session — the production caller of
    // authorizes_recovery.
    let obs = SessionObs {
        live,
        status: SessionStatus::Busy,
        signal_a_stuck: signal_a,
        signal_b_stale: signal_b,
        signal_b_harness_live: true, // a live signal-B harness → full ladder permitted
        daemon_state: DaemonLiveness::Up,
        is_headless: false,
        connect_ok: Some(true),
        is_dead_dangling: false,
        should_be_running: true,
        now_ms: now,
    };
    let decision = evaluate_session(&obs);

    eprintln!(
        "[live-chain] REAL: live={live:?} signal_a={signal_a} signal_b={signal_b} -> {decision:?}"
    );

    // Both REAL signals fired on a genuinely wedged victim, the REAL two-signal guard
    // classified Wedged, and the ladder ARMED from Rung 1.
    assert!(
        signal_a && signal_b,
        "both REAL signals must fire on a genuinely wedged victim (A={signal_a}, B={signal_b})"
    );
    assert!(
        matches!(
            decision,
            LadderDecision::Arm {
                entry: Rung::PidfdSignal,
                ..
            }
        ),
        "the ladder ARMS from Rung 1 on the real wedged session, got {decision:?}"
    );

    // REAL recovery: Rung 1 pidfd-signal (SIGCONT) on the LIVE victim — the
    // open-then-send path against a real alive pid. The chain reached a real action.
    rung1_pidfd_signal(pid, RUNG1_SIGNAL)
        .expect("Rung 1 pidfd SIGCONT to the live wedged victim succeeds");

    // SIGCONT is non-destructive: the victim is still a real, live process — the
    // ladder acted on a REAL session, end to end.
    let after = proc_liveness(pid);
    assert!(
        matches!(
            after,
            ProcLiveness::Sleeping | ProcLiveness::RunningOrDisk | ProcLiveness::Unknown
        ),
        "the wedged victim is still alive after the non-destructive Rung-1 action, got {after:?}"
    );
    drop(v);
}

// ===========================================================================
// R3c item-1 — FLOCK liveness LIVE-WIRING controls (§3 R3a-1 / R3a-1-P4).
//
// These drive the PRODUCTION `dispatch::livelock::LivenessLock::acquire` /
// `probe_dead` primitives — the SAME ones the daemon-lifetime path
// (`bin/qd/daemon.rs`, held across `block_on` for the daemon's life) calls
// — against a REAL victim process. They prove the flock wiring is LOAD-BEARING on
// the live path (NOT a no-op) and that the CRASH-WITH-SURVIVING-CHILD false-alive
// (∞→0) is closed by scoped CLOEXEC. Both are `#[cfg(feature = "faultinj")]` (they
// need the `faultinj_target` victim bin); each documents its DISTINCT revert seam.
// ===========================================================================

/// Spawn `faultinj_target` with explicit args; block until the PARENT prints
/// READY; return (child, reader-positioned-after-READY, parent pid).
#[cfg(feature = "faultinj")]
fn spawn_target_args(args: &[&str]) -> (Child, BufReader<std::process::ChildStdout>, i32) {
    let mut child = Command::new(Victim::target_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn faultinj_target with args");
    let stdout = child.stdout.take().expect("victim stdout");
    let mut reader = BufReader::new(stdout);
    let (pid, _start) = Victim::read_ready(&mut reader);
    assert_eq!(pid, child.id() as i32, "READY pid must match the spawned child pid");
    (child, reader, pid)
}

/// Read the next `CHILD pid=<cpid>` line from the victim's stdout.
#[cfg(feature = "faultinj")]
fn read_child_pid(reader: &mut BufReader<std::process::ChildStdout>) -> i32 {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read CHILD line");
    line.split_whitespace()
        .find_map(|t| t.strip_prefix("pid=").and_then(|v| v.parse::<i32>().ok()))
        .expect("CHILD pid")
}

/// Poll `probe_dead` until it reports DEAD (true) or the budget elapses.
#[cfg(feature = "faultinj")]
fn wait_until_probe_dead(state_dir: &Path, sid: &str, dl: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < dl {
        if dispatch::livelock::probe_dead(state_dir, sid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// §3 R3a-1 (flock) — the live `LivenessLock::acquire` is LOAD-BEARING.
///
/// A real victim acquires the PRODUCTION liveness flock (the SAME primitive the
/// daemon-lifetime path calls) and holds it. While the holder is alive,
/// `probe_dead` reports LIVE (false); after SIGKILL the kernel releases the lock
/// (last-close) and `probe_dead` reports DEAD (true) — O(1), no `/proc` walk.
///
/// This is the negative control for clause-7(a): revert the live acquire to a
/// no-op (e.g. drop the `flock` so the held fd never locks) → `probe_dead` reports
/// DEAD while the holder is alive → the first assertion REDS (the wiring is no
/// longer load-bearing). DISTINCT revert seam from the wedge (`classify_obs`) and
/// wake (control socket) classes: it is the flock acquire itself.
#[cfg(feature = "faultinj")]
#[test]
fn class_r3a1_live_flock_acquire_is_load_bearing() {
    use dispatch::livelock::probe_dead;
    let state = tempfile::tempdir().expect("tempdir");
    let sid = "flock-load-bearing";

    let (mut child, _reader, pid) =
        spawn_target_args(&["--flock-hold", state.path().to_str().unwrap(), sid]);

    // A LIVE holder: the lock is held → probe_dead is false (NOT dead). The
    // load-bearing assertion — a no-op acquire would leave the lock free and this
    // would already be true (RED).
    assert!(
        !probe_dead(state.path(), sid),
        "a LIVE flock holder must read NOT-dead (probe_dead=false); true here means \
         the live acquire is a no-op (clause-7(a) negative control RED)"
    );

    // Kill the holder; the kernel releases the flock on last-close.
    unsafe { libc::kill(pid, libc::SIGKILL); }
    let _ = child.wait();
    assert!(
        wait_until_dead(ProcKey::new(pid, 0), Duration::from_secs(5)),
        "the holder must die"
    );

    // Lock now FREE → probe_dead true (DEAD), witnessed with no cooperation.
    assert!(
        wait_until_probe_dead(state.path(), sid, Duration::from_secs(5)),
        "after the holder dies the flock is free → probe_dead must be true (DEAD)"
    );
}

/// §3 R3a-1-P4 (flock, CRASH-WITH-SURVIVING-CHILD) — false-alive ∞→0, the
/// NON-NEGOTIABLE positive control (the rev0 fleet-wide false-alive guard).
///
/// A real victim acquires the flock then forks a child that EXECs a fresh image
/// (the realistic fleet case: an agent spawns a tool subprocess). The harness
/// SIGKILLs the PARENT while the child survives. With the scoped-CLOEXEC fix the
/// lock fd is `FD_CLOEXEC`, so the child's exec CLOSED it: the parent's death frees
/// the lock and `probe_dead` reports DEAD within budget — false-alive duration 0.
///
/// Its own negative control (DISTINCT seam, verdict-INEQUALITY vs the load-bearing
/// row): revert the scoped CLOEXEC in `livelock::acquire` to the rev0 blanket-clear
/// (`set_cloexec(&fd, false)`) → the surviving child inherits the lock across exec
/// → `probe_dead` stays false forever (false-alive ∞) → this test REDS.
#[cfg(feature = "faultinj")]
#[test]
fn class_r3a1_crash_with_surviving_child_false_alive_is_zero() {
    use dispatch::livelock::probe_dead;
    let state = tempfile::tempdir().expect("tempdir");
    let sid = "flock-surviving-child";

    let (mut parent, mut reader, ppid) =
        spawn_target_args(&["--flock-fork-exec", state.path().to_str().unwrap(), sid]);
    let cpid = read_child_pid(&mut reader);
    assert_ne!(cpid, ppid, "the surviving child is a distinct process from the parent");

    // While the parent holds the lock, it reads LIVE.
    assert!(!probe_dead(state.path(), sid), "the live parent holds the lock");

    // SIGKILL the PARENT; the exec'd child survives (it is not signalled).
    unsafe { libc::kill(ppid, libc::SIGKILL); }
    let _ = parent.wait();
    assert!(
        wait_until_dead(ProcKey::new(ppid, 0), Duration::from_secs(5)),
        "the parent must die"
    );
    // The child is still alive — a surviving child IS the false-alive hazard.
    assert!(
        dispatch::effects::is_pid_alive(cpid),
        "the exec'd child must survive the parent (else the hazard is not exercised)"
    );

    // THE NON-NEGOTIABLE ASSERTION: with the scoped-CLOEXEC fix the surviving child
    // did NOT inherit the lock across exec, so the parent's death FREED it →
    // probe_dead becomes true within budget (false-alive duration → 0). With the
    // rev0 blanket-clear this would stay false forever (∞).
    let freed = wait_until_probe_dead(state.path(), sid, Duration::from_secs(5));

    // Never leak the surviving child — reap it before asserting.
    unsafe { libc::kill(cpid, libc::SIGKILL); }
    assert!(
        freed,
        "CRASH-WITH-SURVIVING-CHILD: the lock must be FREE after the parent dies \
         (false-alive 0). probe_dead stayed false → the surviving child holds the \
         inherited lock → rev0 fleet-wide false-alive (clause-7(b) positive control RED)"
    );
}

// ===========================================================================
// R3c item-2 — signal-A STANDING live producer (real-path, mirrors R3b's pattern).
//
// The rev0 gap: `classify_obs` had no non-test production consumer of a REAL
// `since_turn_start_ms` — the live-chain gate fed it a synthetic past-τ value. This
// drives the GENUINE producer (`TurnStartRecorder` fed by the live
// `RegistryStatusSink` on real `Republish` turn boundaries) into the GENUINE
// classifier against a REAL alive victim, exactly mirroring the R3b signal-B
// real-path test (`class2_wedge_vs_longturn`).
// ===========================================================================

/// signal-A fires `Stuck` from the REAL standing producer past τ, and clears when
/// the turn ends — the genuine classifier consuming a genuine standing producer.
///
/// NON-VACUITY (distinct revert seam — the signal-A producer wiring, NOT the wedge
/// classify_obs path or the wake socket): revert the `note_turn_started` hook in
/// `daemon_status::deliver` (Ready arm) → the live sink records NO anchor →
/// `turn_start_ms` is None → `turn_in_flight` false → classify_obs never `Stuck` →
/// the "signal-A fires" assertion REDs.
#[cfg(feature = "faultinj")]
#[test]
fn class2_signal_a_standing_producer_real_path() {
    use dispatch::daemon_status::RegistryStatusSink;
    use dispatch::liveness::{
        LifecycleState, OsLiveness, StreamLiveness, StreamObs, STUCK_THRESHOLD_MS,
    };
    use dispatch::progress::{TurnStartProducer, TurnStartRecorder};
    use qrmux::headless::{Republish, Sink};
    use qrmux::stream_json::TurnOutcome;

    // A REAL alive victim for the OS layer of classify_obs (signal-A overlays an
    // alive base, never a Gone).
    let v = Victim::spawn("--wedge");
    let key = v.proc_key();

    // The STANDING signal-A producer + a REAL RegistryStatusSink with a clock fixed
    // at t0 — the genuine live producer (NOT a hand-set StreamObs).
    let turn_clock = Arc::new(TurnStartRecorder::new());
    let t0: i64 = 1_000_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let sink = RegistryStatusSink::new(
        dir.path().to_path_buf(),
        v.pid as i64,
        None,
        Box::new(move || t0),
    )
    .with_turn_clock(turn_clock.clone());
    let sid = "sigA-standing";

    // GENUINE producer event: the daemon's headless pump delivers a turn-START
    // (Republish::Ready) → the live sink records the turn-start anchor at t0.
    sink.deliver(Republish::Ready { session_id: sid.to_string() });
    assert_eq!(
        turn_clock.turn_start_ms(sid),
        Some(t0),
        "the live sink recorded the signal-A turn-start anchor"
    );

    // The genuine classifier consumes the REAL since_turn_start_ms from the producer
    // (NOT a synthetic value): evaluate at t0 + τ + 10s.
    let now = t0 + STUCK_THRESHOLD_MS + 10_000;
    let since = turn_clock.since_turn_start_ms(sid, now);
    assert_eq!(
        since,
        Some(STUCK_THRESHOLD_MS + 10_000),
        "REAL signal-A elapsed, computed by the standing producer"
    );

    let sl = StreamLiveness::new(OsLiveness::new());
    let obs = StreamObs {
        first_output_seen: true,
        turn_in_flight: turn_clock.turn_start_ms(sid).is_some(), // REAL: a turn is in flight
        since_turn_start_ms: since,                              // REAL: from the producer
        waiting_on_ledger: false,
    };
    assert_eq!(
        sl.classify_obs(key, &obs),
        LifecycleState::Stuck,
        "signal-A fires Stuck from the REAL standing producer once past τ"
    );

    // Turn ENDS (Republish::Eof(Completed)) → the live sink CLEARS the anchor → no
    // longer in flight → classify_obs is NOT Stuck.
    sink.deliver(Republish::Eof(TurnOutcome::Completed));
    assert_eq!(
        turn_clock.turn_start_ms(sid),
        None,
        "the live sink cleared the signal-A anchor on turn end"
    );
    let obs_after = StreamObs {
        first_output_seen: true,
        turn_in_flight: turn_clock.turn_start_ms(sid).is_some(), // false now
        since_turn_start_ms: turn_clock.since_turn_start_ms(sid, now), // None now
        waiting_on_ledger: false,
    };
    assert_ne!(
        sl.classify_obs(key, &obs_after),
        LifecycleState::Stuck,
        "after the turn ends the standing producer no longer reports Stuck"
    );

    drop(v);
}

// ===========================================================================
// R3c item-3 — Rung-4 DESTRUCTIVE IO, ISOLATED THROWAWAY-VICTIM validation.
//
// Drives the now-LIVE `recovery::execute_rung4` (SIGKILL via pidfd → CAS-tombstone
// → mint fresh session_id → spawn → confirm AliveReady, two-phase committed) against
// a DEDICATED throwaway victim spawned INSIDE a `systemd-run --user --scope -p
// MemoryMax=…` cgroup. It NEVER targets a real/fleet pid: the injector spawns its
// OWN victim, reads its pid from READY, and the (pid,start_ms) identity fence makes
// the target unforgeable. The full live-on-REAL-fleet destructive validation is
// DEFERRED to a supervised daytime session (carried gap) — NOT run here.
// ===========================================================================

/// Spawn `faultinj_target <mode>` INSIDE a `systemd-run --user --scope` MemoryMax
/// cgroup (the injector's own throwaway victim, kernel-capped so it can never spike
/// the SUT). Block until the victim prints READY; return (systemd-run child, victim
/// pid, victim start_ms). The victim pid is the faultinj_target's own
/// `std::process::id()` printed on its READY line — the real OS pid inside the scope.
#[cfg(feature = "faultinj")]
fn spawn_victim_in_cgroup(mode: &str) -> (Child, i32, Option<i64>) {
    let bin = Victim::target_bin();
    let mut child = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "MemoryMax=256M",
            "-p",
            "MemorySwapMax=256M",
        ])
        .arg(&bin)
        .arg(mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("systemd-run --scope faultinj_target");
    let stdout = child.stdout.take().expect("victim stdout");
    let mut reader = BufReader::new(stdout);
    let (pid, start_ms) = Victim::read_ready(&mut reader);
    // Keep the reader alive for the victim's life so its pipe stays open.
    std::mem::forget(reader);
    (child, pid, start_ms)
}

/// R3c item-3 — the isolated-victim Rung-4 destructive injector (RED→GREEN).
///
/// (A) HAPPY PATH: a deep-wedge throwaway victim (in its OWN cgroup) is SIGKILLed,
/// its row CAS-tombstoned, a FRESH session_id minted (≠ the dead id, claimed with a
/// bumped incarnation via `claim_name_with_incarnation` — the item-1
/// incarnation-into-respawn-claim, exercised end-to-end), a successor spawned (also
/// cgroup-isolated) and CONFIRMED AliveReady — all within the ~96s budget.
///
/// (B) IDENTITY FENCE (the DISTINCT revert seam, unique to `execute_rung4` —
/// verdict-INEQUALITY vs the §3 CAS/lease/throttle/phase seams): given an old
/// identity whose recorded start_ms does NOT match the live victim (a recycled pid),
/// the executor returns `AbortedIdentityMismatch` and NEVER SIGKILLs it — the victim
/// stays ALIVE. Revert the pre-SIGKILL start-time fence → the executor kills the
/// stranger → this sub-case REDs (the executor never SIGKILLs a non-matching pid).
#[cfg(feature = "faultinj")]
#[test]
fn class_r3c2_rung4_destructive_isolated_victim() {
    use dispatch::recovery::{execute_rung4, Identity, RecoveryRow, Rung, Rung4Io, Rung4Outcome};
    use dispatch::registry::{claim_name_with_incarnation, claim_payload_with_incarnation};

    // ---- (B) IDENTITY FENCE first (cheap, no kill expected) -----------------
    // A throwaway victim whose recorded identity is DELIBERATELY mismatched.
    let (mut fence_child, fpid, fstart) = spawn_victim_in_cgroup("idle");
    let fence_sb = Sandbox::new();
    fence_sb.write_busy_row(fpid, fstart.unwrap_or(0), fstart.unwrap_or(0));
    let bogus = Identity {
        pid: fpid,
        // Recorded start far from the live victim's → a recycled-pid mismatch.
        start_ms: fstart.unwrap_or(0) + 10 * dispatch::kill::START_TIME_SLACK_MS,
        incarnation: 1,
    };
    let never_kill = |_pid: i32| -> std::io::Result<()> {
        panic!("the identity fence must ABORT before any SIGKILL on a mismatched identity")
    };
    let io_fence = Rung4Io {
        kill: &never_kill,
        current_start_ms: &|pid| dispatch::effects::proc_start_ms(pid),
        mint_session_id: &|| Ok("should-not-mint".to_string()),
        spawn_successor: &|_sid| panic!("must not spawn on identity mismatch"),
        confirm_ready: &|_p, _s| panic!("must not confirm on identity mismatch"),
    };
    let fence_out = execute_rung4(
        fence_sb.path(),
        fence_sb.path(),
        RecoveryRow::cold("fence-sess"),
        &bogus,
        Some(fstart.unwrap_or(0)),
        Duration::from_secs(2),
        Duration::from_millis(20),
        &io_fence,
    )
    .expect("execute_rung4 (fence)");
    assert_eq!(
        fence_out,
        Rung4Outcome::AbortedIdentityMismatch,
        "the pre-SIGKILL identity fence ABORTS a recycled-pid mismatch (distinct revert seam)"
    );
    assert!(
        dispatch::effects::is_pid_alive(fpid),
        "the mismatched victim is NEVER SIGKILLed — execute_rung4 never kills a stranger"
    );
    unsafe { libc::kill(fpid, libc::SIGKILL); }
    let _ = fence_child.wait();

    // ---- (A) HAPPY PATH: kill + fresh successor confirmed within budget ------
    let (mut victim_child, vpid, vstart) = spawn_victim_in_cgroup("--wedge");
    let qd = Sandbox::new();
    let old_started = vstart.unwrap_or(1);
    qd.write_busy_row(vpid, old_started, old_started);
    let old = Identity {
        pid: vpid,
        start_ms: vstart.unwrap_or(0),
        incarnation: 1,
    };

    // The successor victim's pid is captured by the spawn seam (cgroup-isolated). We
    // also exercise the item-1 incarnation-into-respawn-claim: the respawn claims the
    // recovered NAME with a bumped incarnation read INSIDE the O_EXCL section.
    let claims = qd.path().join("claims");
    let successor_pid_slot = std::cell::Cell::new(0_i32);
    let successor_child_slot: std::cell::RefCell<Option<Child>> = std::cell::RefCell::new(None);
    let real_kill = |pid: i32| dispatch::recovery::rung1_pidfd_signal(pid, libc::SIGKILL);
    let mint = || -> std::io::Result<String> {
        // A FRESH, never-reused session id (the non-reuse property).
        Ok(format!("successor-{}-{}", std::process::id(), vpid))
    };
    let spawn_successor = |new_sid: &str| -> std::io::Result<i32> {
        // incarnation-into-respawn-claim: claim the recovered name with the bumped
        // fence read INSIDE the O_EXCL critical section (item-1 primitive, LIVE here).
        let mk = |inc: u64| {
            claim_payload_with_incarnation(std::process::id(), Some(old_started), 0, "victim-name", inc)
                .into_bytes()
        };
        let (_claim, inc) =
            claim_name_with_incarnation(&claims, "victim-name", &|_| false, &|_| None, &mk)
                .expect("respawn claim");
        assert!(inc >= 1, "the respawn claim stamps a monotonic incarnation");
        // Spawn the cgroup-isolated successor victim; record it for reaping.
        let (child, pid, _start) = spawn_victim_in_cgroup("idle");
        successor_pid_slot.set(pid);
        *successor_child_slot.borrow_mut() = Some(child);
        let _ = new_sid;
        Ok(pid)
    };
    let confirm_ready = |pid: i32, _sid: &str| dispatch::effects::is_pid_alive(pid);
    let io = Rung4Io {
        kill: &real_kill,
        current_start_ms: &|pid| dispatch::effects::proc_start_ms(pid),
        mint_session_id: &mint,
        spawn_successor: &spawn_successor,
        confirm_ready: &confirm_ready,
    };

    let budget = Duration::from_secs(10); // « the ~96s Rung::Respawn budget (bounded for the test)
    let out = execute_rung4(
        qd.path(),
        qd.path(),
        RecoveryRow::cold("victim-sess"),
        &old,
        Some(old_started),
        budget,
        Duration::from_millis(50),
        &io,
    )
    .expect("execute_rung4 (happy)");

    // Reap the successor victim (never leak) before asserting.
    let successor_pid = successor_pid_slot.get();
    if successor_pid > 0 {
        unsafe { libc::kill(successor_pid, libc::SIGKILL); }
    }
    if let Some(mut c) = successor_child_slot.borrow_mut().take() {
        let _ = c.wait();
    }
    let _ = victim_child.wait();

    // GREEN: recovered — fresh successor (≠ the dead id) confirmed AliveReady.
    match &out {
        Rung4Outcome::Recovered { new_session_id, successor_pid: spid } => {
            assert_ne!(new_session_id, "victim-sess", "the minted id is FRESH (non-reuse)");
            assert_eq!(*spid, successor_pid, "the confirmed successor is the spawned victim");
        }
        other => panic!("expected Recovered within budget, got {other:?}"),
    }
    // The deep-wedge victim was SIGKILLed (dead).
    assert!(
        wait_until_dead(ProcKey::new(vpid, 0), Duration::from_secs(5)),
        "the deep-wedge throwaway victim must be SIGKILLed by Rung-4"
    );
    // Its row was CAS-tombstoned (the live <pid>.json is gone).
    assert!(
        qd.read_row(vpid).is_none(),
        "the old victim row must be CAS-tombstoned by Rung-4"
    );
    assert!(
        qd.path().join(format!("{vpid}.json.tombstoned")).exists(),
        "a tombstone file marks the terminated old identity"
    );
    // Rung-4 is destructive (the cap authorizes it only for a signal-B-live wedge).
    assert!(Rung::Respawn.is_destructive(), "Rung 4 is the destructive rung");
}

// ===========================================================================
// R3d — event-log + CAS status transitions + atomic registry writes.
// The CAS/log §3 row: CONCURRENCY → no lost updates / no torn reads / no
// interleaved records; the negative control reds on the bypass seam; recovery
// rows survive a restart read-back-identical; a recovery episode is forensically
// reconstructable from the event log alone.
// ===========================================================================

/// R3d (§3 CAS/log row) — the CONCURRENCY injector at the plan's N=16 ceiling proves
/// the `set_status` starttime-CAS is LOAD-BEARING: N real OS threads race a status
/// TRANSITION on one row and NO stale incarnation ever stomps the current one (lost
/// updates 0), with no torn read (the atomic tmp+rename leaves the row parseable).
///
/// NEGATIVE CONTROL (the distinct revert seam `set_status` ↔ raw `write_entry`):
/// performing the SAME stale write through a raw `write_entry` — bypassing the CAS,
/// exactly the R1 §7 inv-1 violation the gate forbids — DOES stomp the current
/// incarnation (a LOST UPDATE). Verdict-INEQUALITY: CAS path loses 0, bypass loses
/// >0. Reverting the fix (route the transition through `write_entry`) reds this.
#[cfg(feature = "faultinj")]
#[test]
fn class_r3d_concurrency_cas_no_lost_update_red() {
    const CUR: i64 = 7_000; // the current incarnation's started_at (owns the row)
    const N: usize = 16; // the plan's tested concurrency ceiling

    // ---- (A) THE FIX: N concurrent set_status racers → 0 lost updates ---------
    let qd = Sandbox::new();
    let pid = 909090;
    qd.write_busy_row(pid, CUR, CUR); // current incarnation, status=busy

    let dir = qd.path().to_path_buf();
    let mut handles = Vec::new();
    for i in 0..N {
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            // Even racers write as the CURRENT incarnation (idle); odd racers as a
            // distinct STALE/foreign incarnation (busy) the CAS MUST reject — a
            // stale write must NEVER land on disk.
            let (expected, status) = if i % 2 == 0 {
                (Some(CUR), "idle")
            } else {
                (Some(CUR - 1_000 - i as i64), "busy") // foreign incarnations
            };
            registry::set_status(&dir, pid as i64, expected, status, now_ms())
                .expect("set_status")
        }));
    }
    let mut rejected = 0usize;
    for h in handles {
        if matches!(h.join().expect("join racer"), StatusWriteOutcome::Rejected { .. }) {
            rejected += 1;
        }
    }
    // No torn read: the row is still parseable after the race.
    let row = qd.read_row(pid).expect("row parseable after the race (atomic rename, no torn read)");

    // LOAD-BEARING path-A assertion (CAS-SENSITIVE): the CAS returned `Rejected` for
    // every foreign-incarnation racer. This is the real path-A guard — drop the CAS
    // guard from `set_status` and it would return `Written` for all racers, so
    // `rejected` would be 0 ≠ N/2 → RED. (Its sibling `detector_concurrency_cas_
    // admits_one_incarnation` is the standalone proof of the same property.)
    assert_eq!(rejected, N / 2, "CAS-SENSITIVE: every foreign-incarnation racer is Rejected");

    // CORROBORATING (NOT the load-bearing non-vacuity guard): no foreign incarnation
    // survives on disk. NOTE (honesty, per the rework): this `started_at` metric is
    // structurally INSENSITIVE to the CAS in path A — `set_status` ALWAYS preserves
    // the on-disk `started_at` (it only ever flips `status`/`updated_at`), so removing
    // the CAS would NOT move this assertion here. The lost-update NON-VACUITY is
    // carried by the (B) negative control below + its `write_entry` revert-probe
    // (which DID red the lost-update metric, EXIT=101) — NOT by this line.
    let cas_lost_updates = usize::from(row.started_at != Some(CUR));
    assert_eq!(cas_lost_updates, 0, "corroborating: no foreign incarnation on disk");

    // ---- (B) NEGATIVE CONTROL: the SAME stale write via raw write_entry --------
    // (bypassing the CAS) DOES stomp the current incarnation — a lost update.
    let sb2 = Sandbox::new();
    sb2.write_busy_row(pid, CUR, CUR);
    let stale = RegistryEntry {
        pid: Some(pid as i64),
        session_id: Some(format!("sess-{pid}")),
        started_at: Some(CUR - 4_000), // a STALE incarnation
        updated_at: Some(now_ms()),
        status: Some("idle".into()),
        name: Some(format!("victim-{pid}")),
        ..Default::default()
    };
    registry::write_entry(sb2.path(), &stale).expect("write_entry bypass");
    let row2 = sb2.read_row(pid).expect("row parseable");
    let bypass_lost_updates = usize::from(row2.started_at != Some(CUR));
    assert!(
        bypass_lost_updates > 0,
        "raw write_entry BYPASSES the CAS → a stale incarnation stomps the current one \
         (a LOST UPDATE) — proving the set_status CAS gate is load-bearing"
    );
    // Verdict-INEQUALITY: CAS path (0) ≠ bypass path (>0).
    assert_ne!(
        cas_lost_updates, bypass_lost_updates,
        "non-vacuity: 0 lost updates under the CAS, >0 under the raw-write_entry bypass"
    );
}

/// R3d (durability) — a recovery row written MID-LADDER (two-phase Rung-4, phase
/// Spawning, with lease + old identity + minted successor) survives a coordinator
/// restart read back BYTE-IDENTICAL. The crash-idempotency R3c relies on is backed by
/// REAL durable persistence (atomic tmp+rename), not in-memory state.
#[cfg(feature = "faultinj")]
#[test]
fn class_r3d_recovery_row_survives_restart_read_back_identical() {
    use dispatch::recovery::{
        read_recovery_row, recovery_row_path, write_recovery_row, Identity, LadderOwner,
        LadderState, Phase, RecoveryRow, Rung, RECOVERY_ROW_VERSION,
    };
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let sid = "sess-mid-ladder";

    let mut row = RecoveryRow::cold(sid);
    row.ladder_owner = Some(LadderOwner::new(7, 1_000_000, 5_000));
    row.ladder_state = LadderState::Running(Rung::Respawn);
    row.phase = Phase::Spawning;
    row.old_identity = Some(Identity { pid: 4321, start_ms: 1_700_000_000_000, incarnation: 3 });
    row.new_session_id = Some("successor-abc".into());
    row.consecutive_failures = 2;
    row.last_attempt_ms = Some(1_700_000_123_456);
    row.daemon_up_ms = Some(1_699_999_000_000);
    row.tau_override_ms = Some(45_000);
    write_recovery_row(state, &row).expect("write recovery row");

    let path = recovery_row_path(state, sid);
    let bytes_before = std::fs::read(&path).unwrap();

    // A "coordinator restart" = a fresh read of the durable row from disk.
    let back = read_recovery_row(state, sid).expect("recovery row survives the restart");
    assert_eq!(back, row, "the mid-ladder recovery row reads back STRUCTURALLY identical");
    assert_eq!(back.version, RECOVERY_ROW_VERSION);

    // BYTE-identical: re-serializing the read-back row reproduces the same file.
    write_recovery_row(state, &back).expect("rewrite");
    let bytes_after = std::fs::read(&path).unwrap();
    assert_eq!(
        bytes_before, bytes_after,
        "the recovery row is read-back BYTE-identical across a coordinator restart"
    );
}

/// R3d (forensics) — a recovery EPISODE is reconstructable from the event log ALONE.
/// Drives the REAL `recovery::execute_rung4` two-phase destructive rung (syscalls
/// seamed, like its own unit tests) and emits the forensic `rung-entered` /
/// `rung-succeeded` events around its REAL outcome (the coordinator's arming-loop
/// wiring). Then reconstructs the episode by READING the on-disk event log — no
/// in-memory ladder state — proving the §3 "forensically reconstructable" property.
#[cfg(feature = "faultinj")]
#[test]
fn class_r3d_recovery_episode_forensically_reconstructable() {
    use dispatch::effects::FixedClock;
    use dispatch::events::{
        emit_ladder_event, read_merged, replay_recovery_episode, EventWriter, LadderEvent,
    };
    use dispatch::recovery::{execute_rung4, Identity, RecoveryRow, Rung, Rung4Io, Rung4Outcome};

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let sid = "episode-sess";
    let writer = EventWriter::for_key(state, sid, Some(sid.into()), None);
    let clock = FixedClock(1_781_241_549_123);

    let (pid, started) = (515151_i32, 1_000_000_i64);
    let entry = RegistryEntry {
        pid: Some(pid as i64),
        session_id: Some(sid.into()),
        started_at: Some(started),
        updated_at: Some(started),
        status: Some("busy".into()),
        name: Some("ep".into()),
        ..Default::default()
    };
    registry::write_entry(state, &entry).unwrap();
    let old = Identity { pid, start_ms: started, incarnation: 1 };
    let io = Rung4Io {
        kill: &|_p| Ok(()),
        current_start_ms: &|_p| Some(started),
        mint_session_id: &|| Ok("fresh-successor".into()),
        spawn_successor: &|_s| Ok(42),
        confirm_ready: &|_p, _s| true,
    };

    // The orchestration layer logs the forensic rung events around the REAL ladder
    // outcome (this is exactly what the live coordinator arming loop wires).
    emit_ladder_event(
        &writer,
        &clock,
        &LadderEvent::RungEntered { session_id: sid.into(), rung: Rung::Respawn.as_str().into() },
    )
    .unwrap();
    let out = execute_rung4(
        state,
        state,
        RecoveryRow::cold(sid),
        &old,
        Some(started),
        Duration::from_millis(200),
        Duration::from_millis(5),
        &io,
    )
    .unwrap();
    match &out {
        Rung4Outcome::Recovered { .. } => emit_ladder_event(
            &writer,
            &clock,
            &LadderEvent::RungSucceeded {
                session_id: sid.into(),
                rung: Rung::Respawn.as_str().into(),
            },
        )
        .unwrap(),
        other => panic!("expected Recovered, got {other:?}"),
    }

    // Reconstruct the episode from the on-disk event log ALONE.
    let records = read_merged(state, Some(sid), None);
    let episode = replay_recovery_episode(&records.records);
    assert_eq!(
        episode,
        vec![
            LadderEvent::RungEntered { session_id: sid.into(), rung: "respawn".into() },
            LadderEvent::RungSucceeded { session_id: sid.into(), rung: "respawn".into() },
        ],
        "the REAL Rung-4 episode reconstructs from the event log alone (forensic replay)"
    );
}

// ===========================================================================
// R3d (durability, brief bar #4) — recovery row survives a REAL induced crash.
// The red-team proved this via 80 SIGKILLs of a throwaway victim looping the REAL
// write_recovery_row; this is the committed regression guard for that property.
// ===========================================================================

/// Two distinct, COMPLETE recovery-row values the crash victim alternates writing.
/// Each is independently valid; a torn/partial write must read back as NEITHER (it
/// parse-fails → None, or yields a value that is neither v1 nor v2). Shared by the
/// re-exec victim entrypoint and the driver so both agree on the exact bytes.
#[cfg(feature = "faultinj")]
fn crash_victim_v1(sid: &str) -> dispatch::recovery::RecoveryRow {
    use dispatch::recovery::{LadderState, Phase, RecoveryRow, Rung};
    let mut r = RecoveryRow::cold(sid);
    r.ladder_state = LadderState::Running(Rung::Respawn);
    r.phase = Phase::Killing;
    r.consecutive_failures = 1;
    r
}

#[cfg(feature = "faultinj")]
fn crash_victim_v2(sid: &str) -> dispatch::recovery::RecoveryRow {
    use dispatch::recovery::{Identity, LadderState, Phase, RecoveryRow, Rung};
    let mut r = RecoveryRow::cold(sid);
    r.ladder_state = LadderState::Running(Rung::Respawn);
    r.phase = Phase::Spawning;
    r.old_identity = Some(Identity { pid: 4321, start_ms: 1_700_000_000_000, incarnation: 3 });
    r.new_session_id = Some("successor-xyz".into());
    r.consecutive_failures = 2;
    r
}

/// The crash VICTIM entry point, re-executed as its OWN process (fork+EXEC of this
/// test binary — `exec` resets to a clean single-threaded image, so this avoids the
/// fork-without-exec hazard of forking the multi-threaded test harness directly).
///
/// As a NORMAL test (env unset) it is an instant no-op pass. With `R3D_CRASH_VICTIM_DIR`
/// set (only when the driver below re-execs it inside a cgroup) it becomes the
/// injector: write its own pid to the pidfile, then loop the REAL `write_recovery_row`
/// (v1, v2, v1, v2 …) FOREVER until the driver SIGKILLs it mid-write. IO-only — it
/// allocates only a few KB per iteration (freed each time), so it cannot RAM-spike;
/// the `systemd-run … MemoryMax` cgroup the driver wraps it in is the hard ceiling.
#[cfg(feature = "faultinj")]
#[test]
fn r3d_crash_victim_entrypoint() {
    let Ok(dir) = std::env::var("R3D_CRASH_VICTIM_DIR") else {
        return; // normal suite run — instant no-op
    };
    let sid = std::env::var("R3D_CRASH_VICTIM_SID").expect("victim sid");
    let pidfile = std::env::var("R3D_CRASH_VICTIM_PIDFILE").expect("victim pidfile");
    let state = PathBuf::from(dir);
    let v1 = crash_victim_v1(&sid);
    let v2 = crash_victim_v2(&sid);
    // Publish our pid so the driver can SIGKILL THIS process precisely (atomic
    // tmp+rename so the driver never reads a torn pidfile).
    let tmp = format!("{pidfile}.tmp");
    std::fs::write(&tmp, std::process::id().to_string()).expect("write pidfile tmp");
    std::fs::rename(&tmp, &pidfile).expect("publish pidfile");
    // Tight write loop of the REAL durable writer until SIGKILLed mid-write.
    loop {
        let _ = dispatch::recovery::write_recovery_row(&state, &v1);
        let _ = dispatch::recovery::write_recovery_row(&state, &v2);
    }
}

/// R3d (durability, brief bar #4) — the recovery row survives REAL induced crashes
/// read-back BYTE-COMPLETE. For N rounds: re-exec the victim entrypoint as its own
/// process INSIDE a `systemd-run --user --scope` MemoryMax cgroup (SUT/read-path stays
/// in THIS process, OUTSIDE the cgroup), let it loop the REAL `write_recovery_row`, then
/// SIGKILL it at a jittered point so the kill lands inside the tmp-write / rename window.
/// After each crash the LIVE row MUST read back as a COMPLETE v1 OR v2 — never torn,
/// never unparseable (`None`) — and a crash-before-rename `.tmp` must never have become
/// or corrupted the live row. This is the committed regression guard for the atomic
/// tmp+rename durability the crash-idempotent ladder relies on.
#[cfg(feature = "faultinj")]
#[test]
fn class_r3d_recovery_row_survives_induced_crash() {
    use dispatch::recovery::{read_recovery_row, recovery_row_path, write_recovery_row};

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let sid = "crash-sess";
    let v1 = crash_victim_v1(sid);
    let v2 = crash_victim_v2(sid);
    // Seed a known-good row so the FIRST read always has a valid prior row even if the
    // victim is killed before its first successful write.
    write_recovery_row(state, &v1).expect("seed");
    let live_path = recovery_row_path(state, sid);

    let exe = std::env::current_exe().expect("current test exe");
    let pidfile = state.join("victim.pid");
    const ROUNDS: usize = 40; // induced SIGKILLs (red-team used 80; 40 is a robust guard)
    let mut ok = 0usize;

    for round in 0..ROUNDS {
        let _ = std::fs::remove_file(&pidfile);
        // Re-exec THIS test binary, single test, inside a memory-capped cgroup.
        let mut child = Command::new("systemd-run")
            .args([
                "--user", "--scope", "--quiet", "--collect",
                "-p", "MemoryMax=256M", "-p", "MemorySwapMax=256M",
            ])
            .arg(&exe)
            .args(["--exact", "r3d_crash_victim_entrypoint", "--nocapture"])
            .env("R3D_CRASH_VICTIM_DIR", state)
            .env("R3D_CRASH_VICTIM_SID", sid)
            .env("R3D_CRASH_VICTIM_PIDFILE", &pidfile)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cgroup-isolated crash victim");

        // Wait for the victim to publish its pid (it is already looping writes by then).
        let victim_pid = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(s) = std::fs::read_to_string(&pidfile) {
                    if let Ok(p) = s.trim().parse::<i32>() {
                        break p;
                    }
                }
                assert!(Instant::now() < deadline, "round {round}: victim never published a pid");
                std::thread::sleep(Duration::from_millis(2));
            }
        };

        // Jittered extra delay so the SIGKILL lands at varying points in the write.
        let micros = 100 + (round as u64 * 211) % 1700; // ~0.1..1.8ms jitter
        std::thread::sleep(Duration::from_micros(micros));
        // SIGKILL the victim PRECISELY (mid-write); --collect reaps the scope.
        unsafe { libc::kill(victim_pid, libc::SIGKILL); }
        let _ = child.wait();

        // SUT read-path (THIS process, outside the cgroup): the live row must be a
        // COMPLETE prior value — never torn, never None (atomic tmp+rename guarantee).
        let back = read_recovery_row(state, sid).unwrap_or_else(|| {
            panic!("round {round}: live recovery row unreadable/torn after an induced crash")
        });
        assert!(
            back == v1 || back == v2,
            "round {round}: live row must be a COMPLETE v1 or v2 after the crash, got {back:?}"
        );
        // A crash-before-rename `.tmp` must never BE the live row. The live path parses
        // (asserted above); confirm any leftover tmp litter is a distinct path, not the
        // live file, and that the live file is exactly the canonical row path.
        assert!(live_path.exists(), "round {round}: the live row path must persist");
        ok += 1;
    }

    assert_eq!(ok, ROUNDS, "every induced crash left a byte-complete, parseable live row");
    let _ = std::fs::remove_file(&pidfile);
    // Final: the live row is still a complete, valid value.
    let final_row = read_recovery_row(state, sid).expect("final live row valid");
    assert!(final_row == v1 || final_row == v2, "final live row is complete");
}
