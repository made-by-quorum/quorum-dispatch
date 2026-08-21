//! `qd resume` + `qd kill` for the resident **ACP adapter** — the ACP-CC lane's
//! half of what used to be the flat `dispatch::resume_daemon`.
//!
//! That module's doc called itself "DAEMON-hosted (codex)". It was not: a third of
//! its body was this — the `qd acp-daemon` group-reap, the ACP alive-vs-revive
//! decision, the concurrent-resume flock claim, and the post-resume bridge-continuation
//! verify. None of it touches the codex app-server. The codex half is
//! [`crate::provider::codex::resume`]; the shared kill result lives with the pgid
//! ladder it reports on ([`crate::create_daemon::DaemonKillOutcome`]).
//!
//! The four pieces, each the ACP analog of a codex rung (the structural mirror
//! [`crate::provider::acp::residence`] documents seam-by-seam):
//!   - **[`kill_acp`]** — the [`kill_codex`](crate::provider::codex::resume::kill_codex)
//!     analog. GROUP-reaps the resident adapter's pgid (adapter + its
//!     `claude-code-acp` bridge child together, the S8 two-level teardown), gated on
//!     the ACP identity predicate rather than codex's, then tombstones.
//!   - **[`acp_resume_is_alive`]** — the alive-vs-revive (R-c) decision, mirroring
//!     [`resume_codex`](crate::provider::codex::resume::resume_codex)'s AlreadyRunning
//!     gate 1:1. Deliberately CONNECTIONLESS: the resident serve loop is
//!     single-connection, so a probe against a live-but-camped adapter would time out
//!     and be misread as dead → double-spawn.
//!   - **[`acquire_resume_claim`]** — the FINDING #3 flock claim. ACP PATH ONLY; codex
//!     resume has no concurrent-resume guard (a named follow-on).
//!   - **the VERIFY-THE-BRIDGE check** ([`verify_post_resume_continuation`] and its
//!     pure core [`classify_post_resume_continuation`]) — FINDING #2 PART 2. It reads
//!     *claude* transcripts, but it is not claude code: the ACP-CC **bridge** writes
//!     CC-shaped JSONL, and this is the check that the bridge did not FORK that file
//!     on `session/load`. It belongs to the bridge, so it belongs here.
//!
//! There is no revive choreography in this module. ACP revive is spawned by the verb
//! layer through [`crate::provider::acp::residence`]; what lives here is the decision,
//! the claim, and the verdict.

use crate::create_daemon::{CmdlineProbe, DaemonKillOutcome, DaemonSpawner};
use crate::effects::is_pid_alive;
use crate::registry::{ensure_tombstone, RegistryEntry};

/// scoped-ACP-CC kill (F3 / super6 rider-4 kill-no-leak): the ACP analog of
/// [`kill_codex`](crate::provider::codex::resume::kill_codex). GROUP-reaps the resident `qd acp-daemon` adapter pid — which is the
/// pgid OUR spawn created with `process_group(0)`, so the SIGTERM→grace→SIGKILL group
/// ladder reaps the adapter AND its `claude-code-acp` bridge child TOGETHER (the proven
/// S8 discipline) — then tombstones. The ONLY difference from [`kill_codex`] (see the
/// module doc) is the
/// identity predicate: [`cmdline_is_our_acp_daemon`](crate::provider::acp::residence::cmdline_is_our_acp_daemon)
/// (the adapter marker + recorded `--listen <endpoint>`), not the codex
/// `codex`+`app-server` match — so a reused pid running a foreign group is NOT signaled
/// (tombstone only). This is also the de-facto UNCONDITIONAL wedge-unblock for the
/// no-WS-R case: killing the adapter pgid tears down a wedged turn's bridge.
pub fn kill_acp(
    sessions_dir: &std::path::Path,
    pid: i64,
    captured: Option<&RegistryEntry>,
    spawner: &dyn DaemonSpawner,
    cmdline_probe: &CmdlineProbe,
) -> DaemonKillOutcome {
    let endpoint = captured.and_then(|e| e.endpoint.as_deref());
    let was_alive = pid > 0
        && is_pid_alive(pid as i32)
        && crate::provider::acp::residence::cmdline_is_our_acp_daemon(
            cmdline_probe(pid).as_deref(),
            endpoint,
        );
    if was_alive {
        // GROUP-addressed reap by the recorded pgid (= the adapter pid). Reaps the
        // adapter + bridge child together; the identity guard guarantees it is OURS.
        spawner.kill(pid);
    }
    if pid > 0 {
        ensure_tombstone(sessions_dir, pid, captured);
        // R2-1 (belt-and-suspenders): a stopped session leaves NO aliasable resume-verify
        // marker behind (the load-bearing defense is the consumer's sid cross-check, but a
        // stop should not leak a stale marker — consistent with the tombstone discipline).
        let _ = std::fs::remove_file(resume_verify_marker_path(sessions_dir, pid));
    }
    DaemonKillOutcome { was_alive }
}

/// Item 3 RESUME — the alive-vs-revive decision for an acp (daemon-hosted) row: the
/// ACP analog of [`resume_codex`](crate::provider::codex::resume::resume_codex)'s AlreadyRunning gate (mirrored 1:1), and THE distinct
/// (R-c) revert seam. A row is "already running" (→ clean no-op, NO second adapter) ONLY
/// when its recorded pid is alive AND an endpoint is recorded AND that pid's live `/proc`
/// cmdline is OUR acp adapter carrying the recorded `--listen <endpoint>` (defeats PID
/// reuse — a recycled pid running something else fails this). Anything else (pid dead /
/// tombstoned / identity-fail / no endpoint) → revive.
///
/// NO reachability CONNECT probe (deliberately — the codex gate has none either): the
/// resident [`serve`](crate::provider::acp::serve) loop is single-connection, so a connect against a genuinely-ALIVE
/// adapter that is camped in another client's long `wait` would TIME OUT and be misread
/// as dead → resume would revive a live session and DOUBLE-SPAWN an adapter (the R-c/R-d
/// hazard). `cmdline_is_our_acp_daemon` already confirms the adapter is serving the
/// recorded endpoint, so pid-alive ∧ identity is the faithful, hazard-free liveness.
///
/// Pure (pid-alive via [`is_pid_alive`] + the injected cmdline probe) so a unit pins both
/// arms. REVERTING this to always-`false` makes `resume` on a LIVE acp row spawn a SECOND
/// adapter — exactly the hazard the oracle reverts this seam to catch.
pub fn acp_resume_is_alive(
    current_pid: Option<i64>,
    current_endpoint: Option<&str>,
    cmdline_probe: impl Fn(i64) -> Option<String>,
) -> bool {
    let endpoint_set = current_endpoint.map(|e| !e.is_empty()).unwrap_or(false);
    let pid = current_pid.filter(|&p| p != 0);
    let pid_alive = pid.map(|p| is_pid_alive(p as i32)).unwrap_or(false);
    if !(pid_alive && endpoint_set) {
        return false;
    }
    crate::provider::acp::residence::cmdline_is_our_acp_daemon(
        pid.and_then(&cmdline_probe).as_deref(),
        current_endpoint,
    )
}

// ===========================================================================
// Item 3 FINDING #3 — ACP concurrent-resume ATOMIC + SELF-HEALING claim.
// ===========================================================================

/// An exclusive, SELF-HEALING claim on resuming ONE acp sessionId — held across the
/// verb's spawn→row-write critical section so two concurrent `qd resume` of the SAME
/// stopped acp row cannot BOTH spawn a load-mode adapter (the FINDING #3 double-spawn:
/// two live rows / two bridges interleaving the SAME jsonl / a later-stop leak).
///
/// Mechanism: `flock(LOCK_EX|LOCK_NB)` on a per-sessionId lock file. flock is the PREFERRED
/// (cc4) implementation because it is INHERENTLY SELF-HEALING — the OS releases the lock
/// automatically when the holding fd closes, INCLUDING on holder process death. So a
/// crashed claim-holder NEVER bricks future resumes (a stale lock that wedges resume
/// forever would be WORSE than the race — super7 binding); the next legitimate resume
/// simply re-acquires. The claim is ATOMIC (exactly one concurrent caller gets `Some`,
/// the rest get `None` and refuse cleanly), NOT a racy check-then-spawn.
///
/// ACP PATH ONLY — codex resume is untouched (acp adds a concurrent-resume atomic guard
/// codex lacks; codex daemon-resume parity is a named follow-on).
pub struct ResumeClaim {
    // The flock'd fd; Drop closes it → the OS releases the advisory lock. Held only to
    // keep the lock alive for the critical section.
    _file: std::fs::File,
}

/// Try to claim exclusive resume rights for `session_id`. `Ok(Some)` = WON (proceed to
/// spawn; hold the returned guard through the row-write, then drop). `Ok(None)` = another
/// resume holds the claim → the caller REFUSES cleanly (no spawn, no mutation). `Err` =
/// the lock file could not be opened (a real fs error, surfaced).
pub fn acquire_resume_claim(
    sessions_dir: &std::path::Path,
    session_id: &str,
) -> std::io::Result<Option<ResumeClaim>> {
    use std::os::unix::io::AsRawFd;
    // sessionId is a uuid, but sanitize defensively so the lock name is always a safe file.
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::fs::create_dir_all(sessions_dir)?;
    let path = sessions_dir.join(format!("{safe}.resume.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    // SAFETY: flock on a valid owned fd; LOCK_NB makes it a non-blocking atomic try-claim.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(ResumeClaim { _file: file }))
    } else {
        let err = std::io::Error::last_os_error();
        // EWOULDBLOCK/EAGAIN = someone else holds the exclusive lock → a clean LOSER.
        match err.raw_os_error() {
            Some(e) if e == libc::EWOULDBLOCK || e == libc::EAGAIN => Ok(None),
            _ => Err(err),
        }
    }
}

// ===========================================================================
// Item 3 FINDING #2 PART 2 — production VERIFY-THE-BRIDGE post-resume check.
// ===========================================================================
//
// Converts resume from TRUST-the-bridge to VERIFY-the-bridge: after a revive AND the
// FIRST post-resume turn, read the REQUESTED-sessionId's bridge CC JSONL ON DISK (the
// PRIMARY source — never the adapter's cached id echo, which is the Finding #2 vacuity)
// and confirm the post-resume turn CONTINUED the SAME file. A bridge fork-on-load (the
// turn landing in a DIFFERENT/NEW session file) is DETECTED and surfaced as a resume
// FAILURE. Cold-path: one-time, gated by a marker the resume drops + the first wait
// consumes; ZERO steady-state per-turn work beyond a single marker `stat`.

/// The verdict of the post-resume continuation check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeContinuation {
    /// The requested-sessionId JSONL grew — the post-resume turn CONTINUED the SAME
    /// conversation. Faithful.
    Continued,
    /// The requested JSONL did NOT grow but a DIFFERENT/NEW session file did — the bridge
    /// FORKED on load (the turn landed elsewhere). Carries the forked file's name. FAIL.
    Forked(String),
    /// Neither confirmed within the retry budget (a read/timing hiccup) — AMBIGUOUS.
    /// Must NOT silently pass (no false-faithful) and must NOT fail a good turn (no
    /// false-loss) → the verb emits a LOUD degraded-confidence signal.
    Unconfirmed,
}

/// The PURE non-vacuous core (the revert/simulate-fork control rules against THIS): given
/// whether the requested file grew and whether a foreign session file received the turn,
/// classify. A SIMULATED fork (`requested_grew=false`, `forked_into=Some`) → `Forked`.
pub fn classify_post_resume_continuation(
    requested_grew: bool,
    forked_into: Option<String>,
) -> ResumeContinuation {
    if requested_grew {
        ResumeContinuation::Continued
    } else if let Some(f) = forked_into {
        ResumeContinuation::Forked(f)
    } else {
        ResumeContinuation::Unconfirmed
    }
}

/// The post-resume verify marker (sidecar `<sessions_dir>/<pid>.resume-verify`): dropped
/// by `run_acp_resume` after a revive, consumed ONCE by the first post-resume wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeVerifyMarker {
    /// The resumed sessionId — the file that MUST grow if the resume is faithful.
    pub session_id: String,
    /// The session cwd (→ the bridge's project dir, where a fork-on-load would write).
    pub cwd: Option<String>,
    /// Line count of `<session_id>.jsonl` at revive time (the baseline to grow beyond).
    pub baseline_lines: usize,
    /// The `*.jsonl` basenames present in the project dir at revive — a file NOT in this
    /// set appearing with content = the fork target.
    pub baseline_files: Vec<String>,
}

/// `<sessions_dir>/<pid>.resume-verify` — the marker path keyed by the resumed adapter pid.
pub fn resume_verify_marker_path(sessions_dir: &std::path::Path, pid: i64) -> std::path::PathBuf {
    sessions_dir.join(format!("{pid}.resume-verify"))
}

/// Write the marker (best-effort JSON). A write failure is surfaced to the caller.
pub fn write_resume_verify_marker(
    path: &std::path::Path,
    marker: &ResumeVerifyMarker,
) -> std::io::Result<()> {
    let v = serde_json::json!({
        "session_id": marker.session_id,
        "cwd": marker.cwd,
        "baseline_lines": marker.baseline_lines,
        "baseline_files": marker.baseline_files,
    });
    std::fs::write(path, serde_json::to_string(&v).unwrap_or_default())
}

/// Read the marker, or `None` if absent/unparseable (a non-resume wait → no marker).
pub fn read_resume_verify_marker(path: &std::path::Path) -> Option<ResumeVerifyMarker> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(ResumeVerifyMarker {
        session_id: v.get("session_id")?.as_str()?.to_string(),
        cwd: v.get("cwd").and_then(|c| c.as_str()).map(str::to_string),
        baseline_lines: v
            .get("baseline_lines")
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as usize,
        baseline_files: v
            .get("baseline_files")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Count non-empty lines in a JSONL file (0 if unreadable).
fn jsonl_line_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// The project dir the bridge writes for this session (from cwd; falls back to the dir of
/// the found requested file).
fn project_dir_for(
    projects_dir: &std::path::Path,
    marker: &ResumeVerifyMarker,
) -> Option<std::path::PathBuf> {
    if let Some(cwd) = marker.cwd.as_deref() {
        let d = projects_dir.join(crate::jsonl::cwd_to_project_path(cwd));
        if d.is_dir() {
            return Some(d);
        }
    }
    crate::jsonl::find_jsonl_path(projects_dir, &marker.session_id, marker.cwd.as_deref())
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// A session `*.jsonl` in the project dir that is NOT the requested one, NOT in the
/// revive-time baseline set, and has content → the fork-on-load target. `None` = no fork.
fn forked_session_file(
    projects_dir: &std::path::Path,
    marker: &ResumeVerifyMarker,
) -> Option<String> {
    let dir = project_dir_for(projects_dir, marker)?;
    let want_self = format!("{}.jsonl", marker.session_id);
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".jsonl") || name.starts_with("agent-") || name == want_self {
            continue;
        }
        if marker.baseline_files.contains(&name) {
            continue; // present already at revive — not the fork target
        }
        // a NEW session file with content → the turn landed here (fork-on-load).
        if jsonl_line_count(&entry.path()) > 0 {
            return Some(name);
        }
    }
    None
}

/// VERIFY-THE-BRIDGE (the I/O wrapper around the pure classify; injectable `projects_dir`
/// + a `retries`/`sleep_ms` bound for the JSONL flush lag — eventual-consistency vs the
/// wire terminal). Reads the REQUESTED-sessionId JSONL ON DISK (primary source). Returns
/// as soon as a definitive verdict is reached; on a persistent non-confirmation it returns
/// `Unconfirmed` (the verb then emits the loud degraded signal — never silent-pass, never
/// session-kill). Bounded ⇒ no unbounded hang.
pub fn verify_post_resume_continuation(
    projects_dir: &std::path::Path,
    marker: &ResumeVerifyMarker,
    retries: u32,
    sleep_ms: u64,
) -> ResumeContinuation {
    for attempt in 0..=retries {
        let requested =
            crate::jsonl::find_jsonl_path(projects_dir, &marker.session_id, marker.cwd.as_deref());
        let requested_grew = requested
            .as_ref()
            .map(|p| jsonl_line_count(p) > marker.baseline_lines)
            .unwrap_or(false);
        let forked_into = if requested_grew {
            None
        } else {
            forked_session_file(projects_dir, marker)
        };
        let verdict = classify_post_resume_continuation(requested_grew, forked_into);
        // A definitive verdict (Continued/Forked) returns immediately; Unconfirmed retries
        // for the flush lag, then gives up loudly.
        if !matches!(verdict, ResumeContinuation::Unconfirmed) || attempt == retries {
            return verdict;
        }
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
    }
    ResumeContinuation::Unconfirmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // === Fixtures. Deliberately NOT shared with the codex resume module's: the two
    //     modules are independent now, and this half's only spawner obligation is to
    //     satisfy the `&dyn DaemonSpawner` parameter of `kill_acp`. ===

    /// A spawner that records nothing — this module's kill unit drives the ALREADY-DEAD
    /// seal, where `kill_acp` sends no signal at all. (The pgid-argument assertions live
    /// with the codex fixture spawner, which shares the same `RealDaemonSpawner::kill`
    /// ladder.)
    struct NullSpawner;
    impl DaemonSpawner for NullSpawner {
        fn spawn_detached(
            &self,
            _argv: &[String],
            _env: &[(String, String)],
            _cwd: &std::path::Path,
            _log_path: &std::path::Path,
        ) -> std::io::Result<crate::create_daemon::SpawnedDaemon> {
            unreachable!("kill_acp never spawns")
        }
        fn kill(&self, _pid: i64) {}
    }

    struct Harness {
        _tmp: TempDir,
        sessions_dir: PathBuf,
    }
    fn harness() -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        Harness {
            sessions_dir: tmp.path().join("sessions"),
            _tmp: tmp,
        }
    }

    // ====================================================================
    // KILL (the acp adapter group-reap).
    // ====================================================================

    // === R2-1 (belt-and-suspenders): kill_acp removes the resume-verify marker so a
    //     stopped session leaves NOTHING aliasable on pid-reuse. ===
    #[test]
    fn kill_acp_removes_the_resume_verify_marker() {
        let h = harness();
        std::fs::create_dir_all(&h.sessions_dir).unwrap();
        let dead_pid = 2_000_000_009i64;
        // Plant a marker as if a resume had written one for this adapter pid.
        let marker = resume_verify_marker_path(&h.sessions_dir, dead_pid);
        write_resume_verify_marker(
            &marker,
            &ResumeVerifyMarker {
                session_id: "T-STOPME".into(),
                cwd: Some("/work/proj".into()),
                baseline_lines: 2,
                baseline_files: vec!["T-STOPME.jsonl".into()],
            },
        )
        .unwrap();
        assert!(marker.exists(), "marker planted");
        let spawner = NullSpawner;
        let probe = |_pid: i64| None;
        let captured = RegistryEntry {
            pid: Some(dead_pid),
            session_id: Some("T-STOPME".into()),
            provider: Some("acp/claude-code".into()),
            endpoint: Some("ws://127.0.0.1:18977".into()),
            ..Default::default()
        };
        kill_acp(&h.sessions_dir, dead_pid, Some(&captured), &spawner, &probe);
        assert!(
            !marker.exists(),
            "kill_acp removed the resume-verify marker (no stale alias)"
        );
    }

    // ====================================================================
    // Item 3 RESUME (acp): the alive-vs-revive (R-c) decision seam.
    // ====================================================================

    /// ALIVE acp row (our pid + matching cmdline carrying the recorded endpoint) →
    /// `acp_resume_is_alive` is TRUE → the verb no-ops (NO second adapter). Uses OUR OWN
    /// pid (guaranteed alive). REVERTING the gate to always-`false` flips this assert →
    /// resume would revive (double-spawn) a live row.
    #[test]
    fn acp_resume_alive_row_is_already_running() {
        let ep = "ws://127.0.0.1:18991";
        let my_pid = std::process::id() as i64;
        let alive = acp_resume_is_alive(Some(my_pid), Some(ep), |_p| {
            Some(format!("/usr/bin/qd acp-daemon --listen {ep} --cwd /w"))
        });
        assert!(
            alive,
            "an alive, identity-matched acp row is AlreadyRunning"
        );
    }

    /// The revive arms — each independently forces `false` (→ revive, never a false
    /// no-op): (a) dead pid, (b) reused pid whose cmdline is foreign, (c) no endpoint.
    #[test]
    fn acp_resume_revives_when_not_truly_alive() {
        let ep = "ws://127.0.0.1:18992";
        let my_pid = std::process::id() as i64;
        // (a) dead pid (a very large unlikely pid) → not alive → revive.
        assert!(!acp_resume_is_alive(Some(2_000_000_001), Some(ep), |_p| {
            Some(format!("qd acp-daemon --listen {ep}"))
        }));
        // (b) live pid but FOREIGN cmdline (pid reuse) → identity fails → revive.
        assert!(!acp_resume_is_alive(Some(my_pid), Some(ep), |_p| Some(
            "/usr/bin/some-unrelated --serve".to_string()
        )));
        // (c) live + ours cmdline but a DIFFERENT endpoint (the row's ep not on cmdline)
        //     → identity fails → revive (stale/mismatched endpoint).
        assert!(!acp_resume_is_alive(Some(my_pid), Some(ep), |_p| Some(
            "qd acp-daemon --listen ws://127.0.0.1:19999".to_string()
        )));
        // (d) no endpoint recorded → never drivable → revive.
        assert!(!acp_resume_is_alive(Some(my_pid), None, |_p| Some(
            "qd acp-daemon".to_string()
        )));
    }

    // ====================================================================
    // FINDING #3 — concurrent-resume ATOMIC + SELF-HEALING claim.
    // ====================================================================

    /// (a) ATOMIC claim + (self-healing) RECLAIM. Two claims on the SAME sessionId in the
    /// race window → exactly ONE wins (`Some`), the other LOSES (`None`); a DIFFERENT
    /// sessionId is independent. Releasing the holder (fd close — the SAME OS mechanism as
    /// holder-process-death) lets the next claim RECLAIM → never bricked. REVERT CONTROL:
    /// remove the flock from `acquire_resume_claim` → both claims return `Some` (the
    /// double-spawn race reappears) → this REDs.
    #[test]
    fn resume_claim_is_atomic_and_self_healing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let a = acquire_resume_claim(dir, "sess-x").unwrap();
        assert!(a.is_some(), "first claim WINS");
        // concurrent claim on the SAME session → LOSES (atomic, exactly one winner).
        let b = acquire_resume_claim(dir, "sess-x").unwrap();
        assert!(
            b.is_none(),
            "a concurrent claim on the same session LOSES (no double-spawn)"
        );
        // a DIFFERENT session is independent → wins.
        assert!(
            acquire_resume_claim(dir, "sess-y").unwrap().is_some(),
            "a different session's claim is independent"
        );
        // release the holder (fd close == holder-death-equivalent → OS releases flock).
        drop(a);
        // BOUNDED retry, and the bound is the assertion: a self-healing claim
        // reclaims essentially immediately, a BRICKED one never does.
        //
        // Why a retry at all, when `drop(a)` closed the only fd. flock is held per
        // OPEN FILE DESCRIPTION, and a `fork` duplicates every open description —
        // so between another test thread's `fork` and its `exec` (O_CLOEXEC only
        // closes AT exec), that child transiently holds a copy of THIS lock. Any
        // test in this binary that spawns a process opens that window, and the
        // suite runs in parallel by default. The window is microseconds; a real
        // brick is forever. Pre-fix this test failed ~50% of parallel runs and
        // passed 100% under `--test-threads=1`, which is the signature of exactly
        // this race and NOT of a claim contract that does not hold.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut c = acquire_resume_claim(dir, "sess-x").unwrap();
        while c.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
            c = acquire_resume_claim(dir, "sess-x").unwrap();
        }
        assert!(
            c.is_some(),
            "after the holder releases/dies, the claim is RECLAIMED (self-healing — never bricked)"
        );
    }

    /// (c) STALE-CLAIM RECLAIM after holder PROCESS DEATH (deterministic, primary-source).
    /// A subprocess takes the flock and sleeps; while it HOLDS the lock our claim LOSES;
    /// after we KILL it the OS releases the flock → the next claim SUCCEEDS (reclaims).
    /// Proves self-healing across real process death, not just fd-drop. Skips if the
    /// `flock(1)` CLI is absent (same flock(2) syscall).
    #[test]
    fn resume_claim_reclaims_after_holder_process_dies() {
        let flock_cli = std::process::Command::new("flock")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !flock_cli {
            eprintln!("flock(1) absent — skipping the process-death reclaim repro");
            return;
        }
        use std::os::unix::process::CommandExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // The lock file path MUST match acquire_resume_claim's (sessionId.resume.lock).
        let lock = dir.join("sess-z.resume.lock");
        std::fs::File::create(&lock).unwrap();
        // A holder subprocess in its OWN process group: `flock` takes an EXCLUSIVE lock,
        // then runs `sleep` (a child that inherits the lock fd). We kill the whole GROUP
        // so BOTH die and the inherited fd closes → the OS releases the lock.
        let mut holder = std::process::Command::new("flock")
            .arg("-x")
            .arg(&lock)
            .arg("-c")
            .arg("sleep 30")
            .process_group(0)
            .spawn()
            .expect("spawn flock holder");
        let pgid = holder.id() as i32;
        // Wait until the holder actually owns the lock (our claim LOSES).
        let mut held = false;
        for _ in 0..100 {
            match acquire_resume_claim(dir, "sess-z").unwrap() {
                None => {
                    held = true;
                    break;
                }
                Some(_claim) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        assert!(
            held,
            "while the holder lives, a concurrent claim LOSES (atomic)"
        );
        // KILL the holder GROUP mid-claim (flock + its sleep child) → the OS releases the
        // flock once every fd holding it is closed → a later resume must RECLAIM.
        crate::safe_kill::safe_group_kill(pgid as i64, libc::SIGKILL);
        holder.wait().ok(); // reap the flock parent (no zombie).
        let mut reclaimed = false;
        for _ in 0..100 {
            if acquire_resume_claim(dir, "sess-z").unwrap().is_some() {
                reclaimed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            reclaimed,
            "after the claim-holder process DIES, the next resume RECLAIMS (self-healing — never bricked)"
        );
    }

    // ====================================================================
    // FINDING #2 PART 2 — verify-the-bridge post-resume continuation.
    // ====================================================================

    /// The PURE non-vacuous core (the simulate-fork control): a fork (requested did NOT
    /// grow, a foreign file received the turn) classifies `Forked`; growth → `Continued`;
    /// neither → `Unconfirmed`. REVERT the classifier to always-`Continued` → the Forked
    /// arm REDs (the vacuity the oracle catches).
    #[test]
    fn classify_post_resume_continuation_is_non_vacuous() {
        assert_eq!(
            classify_post_resume_continuation(true, None),
            ResumeContinuation::Continued
        );
        assert_eq!(
            classify_post_resume_continuation(true, Some("other.jsonl".into())),
            ResumeContinuation::Continued,
            "growth wins even if a sibling file also moved"
        );
        assert_eq!(
            classify_post_resume_continuation(false, Some("fork-abc.jsonl".into())),
            ResumeContinuation::Forked("fork-abc.jsonl".into()),
            "no growth + a foreign file got the turn = FORK"
        );
        assert_eq!(
            classify_post_resume_continuation(false, None),
            ResumeContinuation::Unconfirmed
        );
    }

    /// The marker round-trips (write → read) — sessionId/cwd/baseline preserved.
    #[test]
    fn resume_verify_marker_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = resume_verify_marker_path(tmp.path(), 4242);
        let m = ResumeVerifyMarker {
            session_id: "sess-1".into(),
            cwd: Some("/w/p".into()),
            baseline_lines: 3,
            baseline_files: vec!["sess-1.jsonl".into()],
        };
        write_resume_verify_marker(&path, &m).unwrap();
        assert_eq!(read_resume_verify_marker(&path), Some(m));
        // absent marker → None (a non-resume wait).
        assert_eq!(
            read_resume_verify_marker(&tmp.path().join("nope.resume-verify")),
            None
        );
    }

    /// VERIFY-THE-BRIDGE over PRIMARY source (planted JSONL on disk), all three verdicts.
    /// (1) NON-VACUOUS: it reads the ACTUAL requested-sessionId file (never a cached echo),
    /// and the SIMULATED-FORK case (requested flat, a NEW file with the turn) → `Forked`.
    #[test]
    fn verify_post_resume_continuation_reads_disk_all_verdicts() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path();
        let cwd = "/w/proj";
        let dir = projects.join(crate::jsonl::cwd_to_project_path(cwd)); // "-w-proj"
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "sess-resume";
        let reqfile = dir.join(format!("{sid}.jsonl"));
        // baseline: 1 line in the requested file at revive.
        std::fs::write(&reqfile, "{\"type\":\"user\"}\n").unwrap();
        let marker = ResumeVerifyMarker {
            session_id: sid.into(),
            cwd: Some(cwd.into()),
            baseline_lines: 1,
            baseline_files: vec![format!("{sid}.jsonl")],
        };

        // CONTINUED: the requested file GREW (post-resume turn appended).
        std::fs::write(&reqfile, "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n").unwrap();
        assert_eq!(
            verify_post_resume_continuation(projects, &marker, 0, 0),
            ResumeContinuation::Continued
        );

        // FORK: requested back to baseline (no growth) + a NEW session file got the turn.
        std::fs::write(&reqfile, "{\"type\":\"user\"}\n").unwrap();
        let forkfile = dir.join("forked-xyz.jsonl");
        std::fs::write(&forkfile, "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n").unwrap();
        assert_eq!(
            verify_post_resume_continuation(projects, &marker, 0, 0),
            ResumeContinuation::Forked("forked-xyz.jsonl".into()),
            "no growth + a NEW file with content = fork-on-load DETECTED"
        );

        // UNCONFIRMED: requested flat, the fork file removed (nothing grew anywhere).
        std::fs::remove_file(&forkfile).unwrap();
        assert_eq!(
            verify_post_resume_continuation(projects, &marker, 0, 0),
            ResumeContinuation::Unconfirmed
        );
    }
}
