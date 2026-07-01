//! `provider::pi::daemon` — the pi create / resume **choreography** + the item-3
//! pgid teardown. This is the codex `create_daemon`/`resume_daemon` mirror (the
//! CHOREOGRAPHY half of the split pi-lead affirmed): claim → port-alloc → spawn
//! the resident DETACHED → readiness → write the registry row, with a group-scoped
//! teardown on every failure. The HOST half (the resident body, the front) lives
//! in [`super::residence`]; the in-daemon driver in [`super::stdio`].
//!
//! **What differs from codex `create_daemon`:**
//!   - NO version sniff here (pi pins by exact npm version + an RPC-shape smoke
//!     check, item 6 — a build/CI concern, not a per-create gate).
//!   - We spawn OUR resident (`qd pi-daemon --listen …`, [`super::residence::
//!     build_pi_adapter_argv`]) — NOT a self-listening agent server. pi is
//!     stdio-only; the resident is the thing that listens (item 1).
//!   - The birth-id is read AFTER connect (`status`/`get_state` over the front),
//!     not from a `thread/start` result — pi mints its session id internally at
//!     boot and the resident binds it.
//!
//! **Reuse (not re-implementation):** [`crate::create_daemon::RealDaemonSpawner`]
//! (the `process_group(0)` detached spawn + the `-pgid` SIGTERM→grace→SIGKILL
//! group kill — item 3), [`crate::create_daemon::real_alloc_port`] (the
//! 8900-9000-avoiding allocator), [`crate::registry`] (claim + row write), and
//! [`super::residence`] (argv / identity / readiness).

use std::path::PathBuf;
use std::time::Duration;

use crate::create_daemon::{real_alloc_port, DaemonSpawner, RealDaemonSpawner, SpawnedDaemon};
use crate::registry::{self, RegistryEntry};

use super::residence::{build_pi_adapter_argv, cmdline_is_our_pi_daemon, connect_ready};

/// Full (alloc → spawn → connect) attempts before giving up — the codex ×3 ladder
/// for the TOCTOU window where the resident later fails to bind the port we picked.
const SPAWN_RETRIES: u32 = 3;

/// Readiness budget for the resident's front to come up + establish the pi
/// session (the codex `CONNECT_BUDGET` analog; the resident's own boot is ~2s).
const READY_BUDGET: Duration = Duration::from_secs(8);

/// Why a pi create failed. Every variant means: the spawned resident (if any) was
/// group-killed, NO registry row was written, and the verb maps this to a loud
/// message + nonzero exit (the codex `DaemonError` discipline).
#[derive(Debug)]
pub enum PiCreateError {
    /// The atomic O_EXCL name-claim lost the race / the name is mid-create.
    NameClaimed { name: String, holder: String },
    /// The OS would not hand us a usable port after the re-roll attempts.
    PortAllocFailed { detail: String },
    /// The resident could not be spawned / did not become ready after the ladder.
    SpawnFailed { detail: String },
    /// The resident came up but its registry row could not be written (the
    /// resident was group-killed — we never leave an orphan with no row).
    RowWriteFailed { detail: String },
}

impl std::fmt::Display for PiCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PiCreateError::NameClaimed { name, holder } => write!(
                f,
                "qd start: name '{name}' is being created by another process \
                 (claim held: {holder}). No pi session was created."
            ),
            PiCreateError::PortAllocFailed { detail } => write!(
                f,
                "qd start: could not allocate a local port for the pi resident \
                 ({detail}). No pi session was created."
            ),
            PiCreateError::SpawnFailed { detail } => write!(
                f,
                "qd start: the pi adapter resident failed to start ({detail}). \
                 No pi session was created."
            ),
            PiCreateError::RowWriteFailed { detail } => write!(
                f,
                "qd start: the pi session started but its registry row could not be \
                 written ({detail}); the resident was stopped. No pi session was created."
            ),
        }
    }
}

impl std::error::Error for PiCreateError {}

impl PiCreateError {
    pub fn exit_code(&self) -> i32 {
        1
    }
}

/// Injected deps for [`create_pi_session`] — the codex `DaemonDeps` shape, pared
/// to pi's needs. Plain references, the house style.
pub struct PiCreateDeps<'a> {
    /// The self-exe path (`/proc/self/exe`) the resident is spawned from
    /// (`<exe> pi-daemon …`).
    pub exe: PathBuf,
    /// The pinned pi binary (`QD_PI_BIN`; not on PATH) passed to the resident.
    pub pi_bin: Option<String>,
    /// `PI_CODING_AGENT_SESSION_DIR` passed to the resident (sessions root).
    pub session_dir: Option<String>,
    /// The sessions dir the registry row is written into.
    pub sessions_dir: PathBuf,
    /// The claims dir for the O_EXCL name-claim.
    pub claims_dir: PathBuf,
    /// The log dir root for the resident's stdout/stderr.
    pub log_dir: PathBuf,
    /// The detached-spawn seam (real spawner in prod; a fake in units). Reuse of
    /// the codex `DaemonSpawner` — same `process_group(0)` + `-pgid` group kill.
    pub spawner: &'a dyn DaemonSpawner,
    /// now-ms clock for the row stamps.
    pub now_ms: &'a dyn Fn() -> i64,
}

/// Params for one pi create.
pub struct PiCreateParams {
    pub name: String,
    pub cwd: PathBuf,
    /// Resume: re-attach a prior session id (the resident boots in load mode).
    pub load_session: Option<String>,
}

/// Success outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct PiCreateOutcome {
    pub name: String,
    /// The resident pid (the registry row key + the liveness/pgid key).
    pub pid: i64,
    /// The endpoint the resident listens on (`ws://127.0.0.1:<port>`).
    pub endpoint: String,
    /// pi's session birth-id, read off the front after readiness.
    pub session_id: String,
}

/// Run the pi create choreography (the `run_new_daemon` mirror). See the module
/// docs for the differences from codex.
///
/// TODO(first-compile): the name-claim + idstore bind are sketched against the
/// codex helpers (`registry::claim_name` + `ClaimGuard`, `idstore::mint_unbound`/
/// `bind`); the exact borrow plumbing (the RAII guard lifetime, the `is_alive`/
/// `proc_start` predicates) binds at first compile against create_daemon.rs:389-441.
/// This draft encodes the SEQUENCE + the reuse points; it is not yet compiled.
pub fn create_pi_session(
    deps: &PiCreateDeps,
    params: &PiCreateParams,
) -> Result<PiCreateOutcome, PiCreateError> {
    // HARDENING (pi-lead ruling B): the atomic O_EXCL name-claim BEFORE spawning any
    // resident, so two concurrent same-name `qd start` creates fail LOUD before a
    // resident/row exists — the create-race guard the standard's concurrency clause
    // motivates (incarnation-CAS guards STATUS writes, NOT the create claim; C-CHAOS
    // exercises crash+PID-reuse + create-races). Held by an RAII guard through the
    // retry loop + row write, released on EVERY exit path. idstore is deliberately
    // SKIPPED: pi's session_id from get_state IS its durable identity (written to the
    // row, used for --session resume) — the codex stable-id layer has no pi equivalent.
    let payload = claim_payload(deps, &params.name);
    let is_alive = |pid: i64| crate::effects::is_pid_alive(pid as i32);
    let proc_start = |pid: i64| crate::effects::proc_start_ms(pid as i32);
    let _claim = match registry::claim_name(
        &deps.claims_dir,
        &params.name,
        payload.as_bytes(),
        &is_alive,
        &proc_start,
    ) {
        Ok(c) => ClaimGuard::new(c),
        Err(registry::ClaimError::AlreadyClaimed { existing_payload }) => {
            let holder = String::from_utf8_lossy(&existing_payload).into_owned();
            return Err(PiCreateError::NameClaimed {
                name: params.name.clone(),
                holder,
            });
        }
        Err(registry::ClaimError::Io(e)) => {
            return Err(PiCreateError::NameClaimed {
                name: params.name.clone(),
                holder: format!("<claim io error: {e}>"),
            });
        }
    };

    // The retry ladder: alloc port → build argv → spawn resident detached →
    // connect_ready → read birth-id → write row. A resident that fails to bind the
    // port (TOCTOU) surfaces as a readiness failure; re-roll + respawn. Runs UNDER
    // the held `_claim` (RAII release on return).
    let mut last: Option<PiCreateError> = None;
    for _ in 0..SPAWN_RETRIES {
        let port = match real_alloc_port() {
            Ok(p) => p,
            Err(e) => {
                return Err(PiCreateError::PortAllocFailed {
                    detail: e.to_string(),
                })
            }
        };
        let endpoint = format!("ws://127.0.0.1:{port}");
        // Mint THIS incarnation's started_at BEFORE spawn so the row we write below
        // carries this incarnation's stamp (on-read status derivation reads it).
        // `--started-at`/`--registry-dir` are still passed on the resident argv
        // (reserved for A2's status poll), but under OPTION B the resident derives
        // status on-read and constructs no sink — see `residence::run_pi_adapter`.
        let started_at = (deps.now_ms)();
        let argv = build_pi_adapter_argv(
            &deps.exe,
            &endpoint,
            &params.cwd,
            deps.pi_bin.as_deref(),
            deps.session_dir.as_deref(),
            Some(deps.sessions_dir.as_path()),
            Some(started_at),
            params.load_session.as_deref(),
        );
        let log_path = deps.log_dir.join(format!("pi-{}.log", params.name));
        // Spawn DETACHED with process_group(0) (RealDaemonSpawner) — the resident is
        // the pgid leader; the pi child + grandchildren inherit the group (item 3).
        let spawned: SpawnedDaemon = match deps.spawner.spawn_detached(
            &argv,
            &[], // the resident reads its config off argv, not env
            &params.cwd,
            &log_path,
        ) {
            Ok(s) => s,
            Err(e) => {
                last = Some(PiCreateError::SpawnFailed {
                    detail: e.to_string(),
                });
                continue;
            }
        };

        // Readiness: poll the front until the resident's pi session is established.
        if let Err(e) = connect_ready(&endpoint, READY_BUDGET) {
            // Group-kill the just-spawned resident before retrying (no orphan).
            deps.spawner.kill(spawned.pid);
            last = Some(PiCreateError::SpawnFailed { detail: e });
            continue;
        }

        // Read the birth-id off the front (the resident bound it at boot).
        let session_id = match read_birth_id(&endpoint) {
            Ok(sid) => sid,
            Err(e) => {
                deps.spawner.kill(spawned.pid);
                last = Some(PiCreateError::SpawnFailed { detail: e });
                continue;
            }
        };

        // Write the registry row (provider="pi", endpoint, pid, birth-id).
        let now = (deps.now_ms)();
        let entry = RegistryEntry {
            pid: Some(spawned.pid),
            session_id: Some(session_id.clone()),
            cwd: Some(params.cwd.to_string_lossy().into_owned()),
            started_at: Some(started_at),
            updated_at: Some(now),
            status: Some("idle".to_string()),
            name: Some(params.name.clone()),
            version: None,
            kind: None,
            entrypoint: None,
            backend: None,
            spawned_by: None,
            provider: Some("pi".to_string()),
            endpoint: Some(endpoint.clone()),
            transport: None,
        };
        if let Err(e) = registry::write_entry(&deps.sessions_dir, &entry) {
            deps.spawner.kill(spawned.pid);
            return Err(PiCreateError::RowWriteFailed {
                detail: e.to_string(),
            });
        }

        return Ok(PiCreateOutcome {
            name: params.name.clone(),
            pid: spawned.pid,
            endpoint,
            session_id,
        });
    }
    Err(last.unwrap_or_else(|| PiCreateError::SpawnFailed {
        detail: "exhausted spawn retries".to_string(),
    }))
}

/// The O_EXCL claim payload (the shared `registry::claim_payload` writer): stamps
/// the claimant's pid + exec-proof start, so the claude/codex/pi create paths emit
/// a byte-identical claim protocol.
fn claim_payload(deps: &PiCreateDeps<'_>, name: &str) -> String {
    let pid = std::process::id();
    let start = crate::effects::proc_start_ms(pid as i32);
    registry::claim_payload(pid, start, (deps.now_ms)(), name)
}

/// RAII guard releasing a held [`registry::NameClaim`] on drop — the success path
/// AND every failure path after acquisition (the create_daemon.rs `ClaimGuard`).
/// Best-effort release: a failed removal is swallowed (the written registry row
/// already closed the create window; a leaked claim file is at worst a stale UX
/// hint, never a correctness problem).
struct ClaimGuard {
    claim: Option<registry::NameClaim>,
}

impl ClaimGuard {
    fn new(claim: registry::NameClaim) -> Self {
        Self { claim: Some(claim) }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if let Some(claim) = self.claim.take() {
            let _ = claim.release();
        }
    }
}

/// Read pi's birth-id off the resident front (the `status` probe's session_id).
fn read_birth_id(endpoint: &str) -> Result<String, String> {
    let remote = super::remote::PiRemote::connect(endpoint, Duration::from_millis(500))
        .map_err(|e| format!("connect for birth-id: {e}"))?;
    match remote.status_session_id() {
        Ok(Some(sid)) => Ok(sid),
        Ok(None) => Err("resident has no established session id".to_string()),
        Err(e) => Err(format!("status: {e}")),
    }
}

// ===========================================================================
// item 3 — liveness + pgid teardown (reuse of the codex group-kill).
// ===========================================================================

/// Resume liveness predicate (the `resume_daemon` revive-decision analog): a pi
/// resident is ALIVE-and-ours when its recorded pid is alive AND that pid's live
/// `/proc` cmdline is OUR pi-daemon carrying the recorded `--listen <endpoint>`
/// ([`cmdline_is_our_pi_daemon`]) — pid-alive ∧ identity defeats PID reuse. A
/// `cmdline_probe` closure (`pid → Option<cmdline>`) keeps it unit-testable.
pub fn pi_daemon_is_alive(
    pid: i64,
    endpoint: Option<&str>,
    is_pid_alive: &dyn Fn(i64) -> bool,
    cmdline_probe: &dyn Fn(i64) -> Option<String>,
) -> bool {
    is_pid_alive(pid)
        && endpoint.is_some()
        && cmdline_is_our_pi_daemon(cmdline_probe(pid).as_deref(), endpoint)
}

/// item 3 — tear down a pi resident: a GROUP-scoped SIGTERM→grace→SIGKILL by the
/// recorded pgid (= the resident pid, spawned with `process_group(0)`), reusing
/// the codex [`RealDaemonSpawner::kill`]. This reaps the resident, the pi child,
/// AND pi's `&`-detached grandchildren together — a bare pid-kill orphans the
/// grandchildren (PA11). Gated on cmdline-identity so a reused pid running a
/// FOREIGN group is never signaled.
pub fn teardown_pi_daemon(
    pid: i64,
    endpoint: Option<&str>,
    cmdline_probe: &dyn Fn(i64) -> Option<String>,
) {
    if !cmdline_is_our_pi_daemon(cmdline_probe(pid).as_deref(), endpoint) {
        // Not ours (gone / reused pid / foreign group) — do not signal.
        return;
    }
    RealDaemonSpawner.kill(pid);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_alive_requires_pid_endpoint_and_identity() {
        let ep = "ws://127.0.0.1:9000";
        let ours = |_pid: i64| Some(format!("/usr/bin/qd pi-daemon --listen {ep} --cwd /w"));
        let alive = |_pid: i64| true;
        let dead = |_pid: i64| false;
        // alive + ours + endpoint → alive.
        assert!(pi_daemon_is_alive(42, Some(ep), &alive, &ours));
        // dead pid → not alive regardless of cmdline.
        assert!(!pi_daemon_is_alive(42, Some(ep), &dead, &ours));
        // no endpoint recorded → not alive (can't discriminate a reused pid).
        assert!(!pi_daemon_is_alive(42, None, &alive, &ours));
        // alive but a foreign cmdline → not ours.
        let foreign = |_pid: i64| Some("/usr/bin/python server.py".to_string());
        assert!(!pi_daemon_is_alive(42, Some(ep), &alive, &foreign));
    }

    #[test]
    fn teardown_skips_a_pid_that_is_not_ours() {
        // A foreign cmdline → teardown must NOT signal (no panic, no-op path).
        let foreign = |_pid: i64| Some("/usr/bin/other --listen ws://127.0.0.1:1".to_string());
        teardown_pi_daemon(999_999, Some("ws://127.0.0.1:9000"), &foreign);
        // A gone pid (None cmdline) → also a no-op.
        let gone = |_pid: i64| None;
        teardown_pi_daemon(999_999, Some("ws://127.0.0.1:9000"), &gone);
    }
}
