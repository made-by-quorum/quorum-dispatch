//! `qd resume` + `qd kill` for CODEX sessions (codex-p2-spec
//! §7.6 resume + kill paragraphs; ADD-26(2): resume is a FIRST-CLASS AGENT verb =
//! `thread/resume` revive-to-DRIVABLE with NO interactive-attach tail; attach /
//! `--remote` is SEVERED entirely). This is the W7 lib core — the verb layer
//! (resume.rs / kill.rs) holds ONLY a thin `hosting()==Daemon` dispatch branch
//! that calls in here and returns BEFORE any claude attach/resume internals
//! (the R-a hot-file discipline).
//!
//! Split out of the old flat `dispatch::resume_daemon`, whose doc claimed to be
//! "DAEMON-hosted (codex)" while a THIRD of its body was the ACP adapter's kill /
//! resume-claim / verify machinery. That half is now
//! [`crate::provider::acp::resume`]; this module is codex and nothing else. The
//! shared kill result lives with the pgid ladder it reports on
//! ([`crate::create_daemon::DaemonKillOutcome`]).
//!
//! THE TOPOLOGY (W4): ONE qd-owned `codex app-server` per codex session; qd is a
//! short-lived CLI, so the daemon pid + the on-disk rollout are the durable truth.
//! resume/kill reconnect (or respawn) to that truth; the registry row carries the
//! daemon pid (the file name), `provider:"codex"`, the REAL thread id (m2), and the
//! ws `endpoint`.
//!
//! ── KILL (deliverable A) ────────────────────────────────────────────────────
//! A codex row's daemon pid IS the session. Reap it GROUP-addressed — the W4
//! `RealDaemonSpawner::kill` pgid ladder (SIGTERM/-pgid → grace → SIGKILL/-pgid +
//! zombie reap): the homebrew/npm `codex` launcher IGNORES SIGTERM after a ws
//! session and a launcher-only SIGKILL ORPHANS the native child, so the GROUP kill
//! is MANDATORY (the W4/W6 carry). NO zmx/mux reap (a codex row has no pane). Then
//! `ensure_tombstone` with the captured row (carries provider + endpoint). HONEST
//! EDGE: a codex row whose daemon pid is ALREADY dead → no process to kill, still
//! tombstone (the dead-row seal).
//!
//! ── RESUME (deliverable B) ──────────────────────────────────────────────────
//! Resume is revive-to-DRIVABLE; agents have no TTY, so NO attach is EVER forced
//! or appended (there is no attach API to call — every codex path returns success
//! WITHOUT invoking one). Cases:
//!   - Daemon ALIVE (pid alive + endpoint set): the thread is already drivable →
//!     a SUCCESS no-op + a clear agent-facing message ("send to it"), exit 0. NO
//!     spawn, NO attach.
//!   - Daemon DEAD (qd restart/reboot class) but the row + rollout exist → REVIVE:
//!     respawn a codex app-server (reuse the create machinery: version sniff, port
//!     alloc outside 8900-9000, detached `process_group(0)` spawn, ws connect +
//!     initialize), then `thread/resume{thread_id}` (hydrates from the on-disk
//!     rollout), then UPDATE the registry row (new pid, new endpoint, updatedAt).
//!     Drivable now; NO attach.
//!   - Cold/foreign codex row (discovered, no live pid) → the SAME revive path
//!     (this is what makes cold rows first-class).
//!   - HONEST EDGE "no rollout found": a thread that never completed a turn has NO
//!     persisted rollout (W4 lazy-materialization), so `thread/resume` errors
//!     "no rollout found". We do NOT fabricate a new thread (that would break m2
//!     identity) — we report a clean agent-facing error, KILL the just-spawned
//!     daemon (group ladder), and write NO row change.
//!
//! Rename-safe by construction: rollouts are date+uuid keyed, no cwd-slug, so a
//! renamed session resumes against the same rollout the daemon already knows.
//!
//! SEAMS (offline-testable, mirroring [`crate::create_daemon::DaemonDeps`]):
//! spawning behind [`crate::create_daemon::DaemonSpawner`], the rpc via an injected
//! [`crate::create_daemon::RpcConnector`], port via a
//! [`crate::create_daemon::PortAllocator`], plus env/exec/clock house seams. The
//! revive core opens no socket itself; tests pass a fixture rpc + fake spawner.

use std::path::PathBuf;

use crate::create_daemon::{
    cmdline_is_our_daemon, CmdlineProbe, DaemonError, DaemonKillOutcome, DaemonSpawner,
    PortAllocator, RealDaemonSpawner, RpcConnector, SpawnedDaemon,
};
use crate::effects::{is_pid_alive, Clock, Env};
use crate::exec::Exec;
use crate::provider::{LaunchRequest, Provider, ProviderFx};
use crate::registry::{self, ensure_tombstone, RegistryEntry};

use super::rpc::{AppServerRpc, ClientInfo, RpcError};
use super::version::{self, SniffOutcome, VersionVerdict};

// ===========================================================================
// Shared constants (mirror create_daemon — same posture, same ranges).
// ===========================================================================

/// The ws client identity sent on `initialize` (matches the create path + boot
/// waiter + spike probe `clientInfo`).
const CLIENT_NAME: &str = "qd-manager";

/// The relay-probe port range qd scans (codex-p2-spec §3.2): a revive daemon port
/// landing here is RE-ROLLED (the real allocator owns this — restated for the
/// retry honesty of the revive ladder).
const RELAY_RANGE: std::ops::RangeInclusive<u16> = 8900..=9000;

/// How many full (alloc → spawn → connect) attempts before giving up (the create
/// path's ×3 TOCTOU ladder, reused for revive).
const SPAWN_RETRIES: u32 = 3;

/// Bound on the post-spawn ws connect (the daemon needs a moment to bind+listen).
const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The "no rollout found" server-error message prefix (q1c-clientB-events.jsonl
/// line 4, INVALID_REQUEST_CODE -32600). We match on the MESSAGE PREFIX, not the
/// code: -32600 is reused for several precondition fences (the steer fence shares
/// it), so the code alone is ambiguous — the "no rollout found" text is the
/// stable discriminator for a thread that never persisted a rollout.
const NO_ROLLOUT_PREFIX: &str = "no rollout found";

// ===========================================================================
// KILL (deliverable A).
// ===========================================================================
/// Kill a codex (daemon-hosted) session: GROUP-reap the daemon pid via the W4
/// `RealDaemonSpawner::kill` pgid ladder, then `ensure_tombstone` with the captured
/// row. `captured` is the registry row read BEFORE the kill (carries provider +
/// endpoint for the tombstone audit trail). NO zmx/mux reap (no pane).
///
/// W9 FIX M-1 (identity guard): we signal the group ONLY when the recorded pid is
/// alive AND its live command line is OUR codex daemon (`cmdline_is_our_daemon`
/// over the `endpoint` discriminator). Under exact-pid-reuse the stored pid may
/// have been re-assigned to an UNRELATED group leader — signaling `-pgid` then
/// would SIGKILL a FOREIGN group. The guard makes a non-matching (reused) pid the
/// SAME as already-dead: tombstone only, NEVER a foreign-group signal.
///
/// HONEST EDGE: if the daemon pid is already dead (or not-our-daemon), no signal
/// is sent — we still tombstone (the dead-row seal). The kill seam + the cmdline
/// probe are injected so a unit asserts the exact pgid we group-kill (and that we
/// DON'T signal a foreign group) without spawning a real process.
pub fn kill_codex(
    sessions_dir: &std::path::Path,
    pid: i64,
    captured: Option<&RegistryEntry>,
    spawner: &dyn DaemonSpawner,
    cmdline_probe: &CmdlineProbe,
) -> DaemonKillOutcome {
    // The recorded endpoint is the per-instance discriminator (we spawned the
    // daemon with `--listen <endpoint>`); read it off the captured row.
    let endpoint = captured.and_then(|e| e.endpoint.as_deref());
    // Identity-checked liveness: alive AND the live cmdline matches OUR daemon. A
    // reused pid (live, but a foreign group leader) fails the cmdline check → we
    // treat it as NOT-our-daemon → tombstone only, NO group signal.
    let was_alive = pid > 0
        && is_pid_alive(pid as i32)
        && cmdline_is_our_daemon(cmdline_probe(pid).as_deref(), endpoint);
    if was_alive {
        // GROUP-addressed reap by the recorded pgid (= the daemon pid, the pgid OUR
        // spawn created with process_group(0)). The launcher ignores SIGTERM and a
        // launcher-only SIGKILL orphans the native child, so the group ladder is
        // mandatory — this is the W4/W6 carry routed through the SAME impl the
        // create-failure path uses. The identity guard above guarantees the pgid is
        // OUR daemon's, never a reused-pid foreign group.
        spawner.kill(pid);
    }
    // Tombstone regardless (a dead-row seal when already gone / not-our-daemon).
    // ensure_tombstone renames a live <pid>.json, else synthesizes one from
    // `captured` (which carries provider:"codex" + endpoint).
    if pid > 0 {
        ensure_tombstone(sessions_dir, pid, captured);
    }
    DaemonKillOutcome { was_alive }
}

// codex-interactive, use case 2: `reap_viewer_pane` MOVED to
// [`crate::provider::shared::viewer::reap_pane`] and is re-exported here so
// every `codex::resume::reap_viewer_pane` call site keeps resolving.
//
// Same reason `viewer_pane_name` moved (see `codex::pane`): the doc here used to
// say "Lives here, not in the verb, because it is a CODEX AFFORDANCE: no other
// lane has a viewer" — and `acp/opencode` now has one. The body never mentioned
// codex; it kills a pane by name. The acp kill arm calls the same function for
// the same reason this one does: a viewer left pointed at a server we just
// killed sits there rendering a dead connection.
pub use crate::provider::shared::viewer::reap_pane as reap_viewer_pane;

// ===========================================================================
// RESUME (deliverable B).
// ===========================================================================

/// What `resume_codex` decided + did. The verb maps each to an agent-facing
/// message + exit code (success cases exit 0).
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeOutcome {
    /// The daemon was already alive (pid alive + endpoint set): the thread is
    /// drivable RIGHT NOW. A success no-op — NO spawn, NO attach. The verb prints
    /// "session <name> is running; send to it" and exits 0.
    AlreadyRunning,
    /// The daemon was dead (or cold/foreign): we REVIVED it. Carries the new pid +
    /// endpoint (the row was updated). Drivable now — NO attach.
    Revived { pid: i64, endpoint: String },
}

/// Why a codex resume failed (the verb maps each to a clean agent-facing message
/// plus a nonzero exit). NO attach is EVER part of any path (there is no attach
/// API to call — the whole verb returns without one).
#[derive(Debug)]
pub enum ResumeError {
    /// The row carries no thread id (cold/dead with no sessionId) — nothing to
    /// resume against.
    NoThreadId,
    /// The HONEST EDGE: `thread/resume` returned "no rollout found" — the thread
    /// never completed a turn, so it has no persisted history to revive from. We
    /// did NOT fabricate a new thread (m2 identity preserved); the just-spawned
    /// daemon was killed; NO row change was written. Carries the session name for
    /// the message.
    NoResumableHistory { name: String },
    /// A revive spawn/connect/handshake/resume/row-write failure (reuses the
    /// create path's typed `DaemonError` for the spawn+connect rungs; the
    /// resume-specific rungs add their own detail via `Other`).
    Daemon(DaemonError),
    /// A non-no-rollout protocol/transport failure on `thread/resume`, or a
    /// row-write failure after a successful resume. Carries the detail.
    Other { detail: String },
}

impl ResumeError {
    /// Process exit code — all resume failures are exit 1 (the verb precedent).
    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl std::fmt::Display for ResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResumeError::NoThreadId => write!(
                f,
                "this codex session has no thread id to resume (cold/dead row)."
            ),
            ResumeError::NoResumableHistory { name } => write!(
                f,
                "session \"{name}\" has no resumable history yet \
                 (it never completed a turn, so codex persisted no rollout to revive from). \
                 Start it fresh with: qd start --provider codex."
            ),
            ResumeError::Daemon(e) => write!(f, "{e}"),
            ResumeError::Other { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ResumeError {}

/// Injected seams + effects for one [`resume_codex`] call. Mirrors
/// [`crate::create_daemon::DaemonDeps`] so the revive path is offline-testable by
/// construction (the create path's fixture-rpc + fake-spawner style extends here).
pub struct ReviveDeps<'a> {
    /// The resolved provider (codex). Used for `launch_plan` (the daemon argv,
    /// minus `--listen`) — the revive sequence appends `--listen` + drives the rpc
    /// directly, exactly like the create path.
    pub provider: &'a dyn Provider,
    /// Env seam (L9a): the `QD_CODEX_UNPINNED` override read + CODEX_HOME
    /// passthrough via the provider's launch_plan.
    pub env: &'a dyn Env,
    /// Exec seam — the one-shot `codex --version` sniff (§3.4).
    pub exec: &'a dyn Exec,
    /// Clock — the updatedAt stamp on the revived row.
    pub clock: &'a dyn Clock,
    /// The sessions dir the row is read from / written into.
    pub sessions_dir: PathBuf,
    /// The log dir root for the revived daemon's stdout/stderr.
    pub log_dir: PathBuf,
    /// The detached-spawn seam (real in production; a fake in units).
    pub spawner: &'a dyn DaemonSpawner,
    /// The rpc connector (real `WsAppServer::connect` in production; a fixture in
    /// units). Reconnected per spawn attempt.
    pub connect: &'a RpcConnector<'a>,
    /// The port allocator (real OS bind in production; a deterministic fake in
    /// units). Re-rolled per spawn attempt.
    pub alloc_port: &'a PortAllocator<'a>,
    /// W9 FIX Mo-2: the connectionless `pid -> cmdline` probe the AlreadyRunning
    /// gate consults to VERIFY a live recorded pid is OUR codex daemon before
    /// reporting it running. Under exact-pid-reuse a live recorded pid may be an
    /// UNRELATED process — without this check resume would falsely report
    /// "AlreadyRunning" against a foreign pid. Production:
    /// `create_daemon::real_cmdline_probe`; units inject a closure.
    pub cmdline_probe: &'a CmdlineProbe<'a>,
    /// P0 wave-2 (spec-w2-env D1 site 3): the idstore path. The revive KNOWS the
    /// thread uuid up front, so the stable id is `mint_or_get`-keyed directly
    /// (lazy-mints for pre-stable-id sessions) and injected as `QD_SESSION_ID`
    /// in the revived daemon's process env — the id rides through every resume
    /// unchanged.
    pub ids_path: PathBuf,
}

/// The resume input: the resolved row's identity fields (the verb supplies them
/// from the `Session` + the re-read registry row). `name` is the session name (for
/// messages + the rewritten row), `thread_id` is the REAL uuid (m2 — the row's
/// sessionId), `cwd` is the session cwd (the revived daemon's cwd + thread/resume
/// has no cwd, but we keep the row's cwd faithful), `pid`/`endpoint` are the
/// CURRENT row values (alive-check inputs).
pub struct ResumeParams {
    pub name: String,
    pub thread_id: String,
    pub cwd: Option<String>,
    /// The row's CURRENT recorded daemon pid (the alive-check + the OLD row file to
    /// supersede). 0/None ⇒ cold/foreign ⇒ revive.
    pub current_pid: Option<i64>,
    /// The row's CURRENT recorded endpoint (alive-check input — alive requires BOTH
    /// a live pid AND an endpoint).
    pub current_endpoint: Option<String>,
    /// The `hosting` token to STAMP ON THE REWRITTEN ROW — the SAME field, with
    /// the SAME meaning and the same two legal values, as
    /// [`super::create::DaemonParams::hosting`]. `Some("daemon")` for
    /// `codex/daemon`, `Some("app-server")` for `codex/app-server`.
    ///
    /// # Why a revive has to carry this at all
    ///
    /// Step 7 does not PATCH the row, it REWRITES it: a revive is a new daemon
    /// process, so the row is keyed by a new pid and every field is restated from
    /// this struct. Anything the caller does not supply is therefore not
    /// "unchanged" — it is *erased*. `hosting` was the one field with no
    /// argument, hard-coded `None`, and a row's lane is derived from exactly that
    /// field (`crate::provider::row_hosting`). So reviving a `codex/app-server`
    /// session silently DEMOTED it to `codex/daemon`, and it lost the single
    /// thing the app-server lane exists for: [`crate::lanes::LaneImpl::attach`]
    /// stopped opening a `codex --remote` viewer and started answering the daemon
    /// lane's "a daemon-hosted session has no terminal of its own" refusal — for
    /// a session that has one.
    ///
    /// # The invariant this field exists to hold
    ///
    /// **A resumed row must be byte-identical in `hosting` to a freshly created
    /// row of the same lane.** A create and a revive are the two writers of this
    /// field and they must not disagree, because every later verb re-derives the
    /// lane from what they wrote. `a_revive_preserves_the_rows_lane_stamp` pins
    /// it against the create's own value rather than against a literal, so the
    /// two cannot drift apart by editing one of them.
    ///
    /// `Option` for the same reason the create side keeps it: it has to be able
    /// to express "no opinion" for rows written before lanes were stamped, which
    /// [`crate::lane::Harness::row_default_mode`] answers for and which is frozen
    /// so that they never move.
    pub hosting: Option<String>,
}

/// The PRODUCTION wiring for [`resume_codex`] — every real seam assembled in one
/// place, so the verb layer holds nothing but the resolved paths and the outcome
/// match.
///
/// This is deliberately NOT part of `resume_codex`: that function takes
/// [`ReviveDeps`] so units can drive it with a fixture rpc and a fake spawner, and
/// folding the real constructors into it would destroy that. What moved off the
/// verb is the ASSEMBLY — which spawner, which connector, which allocator, which
/// probe — because every one of those choices is a library fact, and the verb was
/// re-stating all of them just to call in here.
///
/// `sessions_dir`, `log_dir` and `ids_path` stay arguments because they descend
/// from HOME, and reading HOME (and failing loudly when it is unset) is the
/// binary's job — the L9a discipline every other qw entry point follows.
pub fn resume_codex_real(
    sessions_dir: PathBuf,
    log_dir: PathBuf,
    ids_path: PathBuf,
    params: &ResumeParams,
) -> Result<ResumeOutcome, ResumeError> {
    let env = crate::effects::RealEnv;
    let exec = crate::exec::RealExec;
    let clock = crate::effects::RealClock;
    let spawner = RealDaemonSpawner;
    let connect = |url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
        super::ws::WsAppServer::connect(url, CONNECT_BUDGET).map(|c| {
            let b: Box<dyn AppServerRpc> = Box::new(c);
            b
        })
    };
    let alloc = crate::create_daemon::real_alloc_port;
    // W9 FIX Mo-2: the cmdline-identity guard — the AlreadyRunning gate reports
    // "running" ONLY when the live recorded pid's command line is OUR codex daemon
    // (never a false AlreadyRunning against a reused foreign pid). The probe reads
    // one pid's cmdline via the existing `ps` seam.
    let probe = crate::create_daemon::real_cmdline_probe;

    let deps = ReviveDeps {
        // codex/mod.rs (two levels up from app_server/resume.rs) defines
        // CODEX_PROVIDER; a bare `super::` would resolve to `app_server`, which
        // does not.
        provider: &super::super::CODEX_PROVIDER,
        env: &env,
        exec: &exec,
        clock: &clock,
        sessions_dir,
        log_dir,
        spawner: &spawner,
        connect: &connect,
        alloc_port: &alloc,
        cmdline_probe: &probe,
        ids_path,
    };
    resume_codex(&deps, params)
}

/// Resume a codex (daemon-hosted) session (codex-p2-spec §7.6; ADD-26(2)). See the
/// module docs for the case table. NO attach is ever invoked on ANY path.
pub fn resume_codex<'a>(
    deps: &'a ReviveDeps<'a>,
    params: &ResumeParams,
) -> Result<ResumeOutcome, ResumeError> {
    if params.thread_id.is_empty() {
        return Err(ResumeError::NoThreadId);
    }

    // Case 1: daemon ALIVE (pid alive AND endpoint set AND the live cmdline is OUR
    // codex daemon) → already drivable, no-op. NO spawn, NO attach. (An alive pid
    // with NO endpoint is treated as not-alive for resume purposes: without an
    // endpoint there is no channel to drive, so we revive — the endpoint is what
    // makes it drivable.)
    //
    // W9 FIX Mo-2 (identity guard): under exact-pid-reuse the recorded pid may be
    // LIVE but belong to an UNRELATED process. `cmdline_is_our_daemon` cross-checks
    // the live command line against the recorded endpoint; a non-match means the
    // real daemon is gone and the pid was recycled → we treat it as NOT running and
    // REVIVE (never a false AlreadyRunning against a foreign pid).
    let endpoint_set = params
        .current_endpoint
        .as_deref()
        .map(|e| !e.is_empty())
        .unwrap_or(false);
    let pid_alive = params
        .current_pid
        .filter(|&p| p != 0)
        .map(|p| is_pid_alive(p as i32))
        .unwrap_or(false);
    let identity_ok = pid_alive
        && cmdline_is_our_daemon(
            params
                .current_pid
                .and_then(|p| (deps.cmdline_probe)(p))
                .as_deref(),
            params.current_endpoint.as_deref(),
        );
    if pid_alive && endpoint_set && identity_ok {
        return Ok(ResumeOutcome::AlreadyRunning);
    }

    // Cases 2/3: daemon DEAD (qd restart/reboot) or cold/foreign → REVIVE. The
    // no-rollout HONEST EDGE surfaces during the resume rung below.
    revive(deps, params)
}

/// The revive path: respawn an app-server (reuse the create machinery: version
/// sniff, port alloc, detached spawn, ws connect + initialize), then
/// `thread/resume{thread_id}` (hydrates from the on-disk rollout), then UPDATE the
/// registry row (new pid/endpoint/updatedAt). Every failure kills the just-spawned
/// daemon (group ladder) + writes NO row change.
fn revive<'a>(
    deps: &'a ReviveDeps<'a>,
    params: &ResumeParams,
) -> Result<ResumeOutcome, ResumeError> {
    // Step 1: version sniff (§3.4) — the same gate the create path applies.
    check_version(deps)?;

    // P0 wave-2 (spec-w2-env D1 site 3): the thread uuid is known here, so the
    // stable id is keyed directly (mint_or_get — the SAME id across every
    // resume; lazy-mints for pre-stable-id sessions). Fail-closed on a mint
    // error (nothing spawned).
    let qd_session_id = crate::idstore::mint_or_get(
        &deps.ids_path,
        &params.thread_id,
        Some(&params.name),
        deps.clock,
    )
    .map_err(|detail| ResumeError::Daemon(DaemonError::IdMintFailed { detail }))?;

    // Step 2-4: the alloc → spawn → connect ladder (×SPAWN_RETRIES). A connected rpc
    // breaks the loop; a bind-race connect failure re-rolls + respawns.
    let mut last_err: Option<DaemonError> = None;
    for _ in 0..SPAWN_RETRIES {
        match try_spawn_and_connect(deps, params, &qd_session_id) {
            Ok((spawned, endpoint, rpc)) => {
                // Steps 5-7 with the connected rpc; any failure kills the daemon.
                return finish_revive(deps, params, spawned, &endpoint, rpc.as_ref());
            }
            Err((err, spawned)) => {
                if let Some(s) = spawned {
                    deps.spawner.kill(s.pid);
                }
                if matches!(
                    err,
                    DaemonError::PortAllocFailed { .. }
                ) {
                    return Err(ResumeError::Daemon(err));
                }
                last_err = Some(err);
            }
        }
    }
    Err(ResumeError::Daemon(last_err.unwrap_or_else(|| {
        DaemonError::SpawnFailed {
            detail: "exhausted spawn retries".to_string(),
        }
    })))
}

/// Step 1: version sniff + override mapping (§3.4) — mirrors create_daemon's
/// `check_version` (the verdict is pure; the override is mapped here). Maps the
/// create path's `DaemonError` straight through `ResumeError::Daemon`.
fn check_version(deps: &ReviveDeps) -> Result<(), ResumeError> {
    match version::sniff(deps.exec) {
        SniffOutcome::Verdict(VersionVerdict::Exact) => Ok(()),
        SniffOutcome::Verdict(VersionVerdict::PatchDrift { found }) => {
            eprintln!(
                "WARNING: codex {}.{}.{} is a patch drift from the pin {} \
                 (proceeding). Re-pin at the next minor.",
                found.major,
                found.minor,
                found.patch,
                version::PINNED
            );
            Ok(())
        }
        SniffOutcome::Verdict(VersionVerdict::Breaking { found, pin }) => {
            // RULED 2026-08-17: drift warns, it does not refuse. The create
            // path's `check_version` carries the full reasoning and the pointer
            // to the guard that actually looks at the protocol; this is its twin
            // and must not diverge from it.
            if !unpinned_override(deps.env) {
                eprintln!(
                    "WARNING: codex {}.{}.{} is a BREAKING drift from the pin {}.{} \
                     — proceeding. Re-pin with dispatch/scripts/codex-schema-diff.sh \
                     + bump VERSION.pin + append to doc/codex-repin-log.md. Silence \
                     this with QD_CODEX_UNPINNED=1.",
                    found.major, found.minor, found.patch, pin.major, pin.minor
                );
            }
            Ok(())
        }
        SniffOutcome::Unparseable { stdout } => {
            Err(ResumeError::Daemon(DaemonError::VersionUnknown {
                detail: format!("`codex --version` output did not parse: {}", stdout.trim()),
            }))
        }
        SniffOutcome::ExecFailed { detail } => {
            Err(ResumeError::Daemon(DaemonError::VersionUnknown { detail }))
        }
    }
}

/// `QD_CODEX_UNPINNED=1` read off the env SEAM (L9a). Any non-"1" (incl. unset) is
/// NOT the override.
fn unpinned_override(env: &dyn Env) -> bool {
    env.var("QD_CODEX_UNPINNED").as_deref() == Some("1")
}

/// One revive attempt: alloc port → build daemon argv (launch_plan + `--listen`) →
/// spawn detached → connect with a bounded retry. Identical machinery to the create
/// path's `try_spawn_and_connect`. On failure returns the typed error + whatever
/// was spawned (so the caller kills it). The thread/resume + row-update happen in
/// `finish_revive` with the connected rpc.
#[allow(clippy::type_complexity)]
fn try_spawn_and_connect<'a>(
    deps: &'a ReviveDeps<'a>,
    params: &ResumeParams,
    qd_session_id: &str,
) -> Result<(SpawnedDaemon, String, Box<dyn AppServerRpc + 'a>), (DaemonError, Option<SpawnedDaemon>)>
{
    let port = (deps.alloc_port)().map_err(|e| {
        (
            DaemonError::PortAllocFailed {
                detail: e.to_string(),
            },
            None,
        )
    })?;
    debug_assert!(
        !RELAY_RANGE.contains(&port),
        "the allocator must re-roll relay-range ports"
    );
    let endpoint = format!("ws://127.0.0.1:{port}");

    // The daemon's cwd = the row's cwd (faithful to the original session). A
    // cold/foreign row with no cwd falls back to "." (the daemon must have a cwd).
    let cwd = params
        .cwd
        .as_deref()
        .filter(|c| !c.is_empty())
        .unwrap_or(".");

    let fx = ProviderFx {
        await_relay: None,
        env: deps.env,
        paths: &placeholder_paths(),
        socket_dir: PathBuf::new(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    let req = LaunchRequest {
        name: params.name.clone(),
        cwd: Some(cwd.to_string()),
        resume: None,
        fork: false,
        agent: None,
        // codex daemon argv has no claude --model surface (warranty #2 is claude-lane).
        model: None,
        passthrough: vec![],
        interactive: false,
        control_socket: None,
    };
    let plan = deps.provider.launch_plan(&fx, &req);
    let mut argv = plan.argv;
    argv.push("--listen".to_string());
    argv.push(endpoint.clone());

    // P0 wave-2 (spec-w2-env D1 site 3): the revived daemon's process env
    // carries the stable id — an explicit set layered over the plan's env.
    let mut spawn_env = plan.env.clone();
    spawn_env.push(("QD_SESSION_ID".to_string(), qd_session_id.to_string()));

    let log_path = deps.log_dir.join(format!("codex-{}.log", params.name));
    let spawned =
        match deps
            .spawner
            .spawn_detached(&argv, &spawn_env, std::path::Path::new(cwd), &log_path)
        {
            Ok(s) => s,
            Err(e) => {
                return Err((
                    DaemonError::SpawnFailed {
                        detail: e.to_string(),
                    },
                    None,
                ))
            }
        };

    match connect_with_retry(deps.connect, &endpoint, CONNECT_BUDGET, deps.clock) {
        Ok(rpc) => Ok((spawned, endpoint, rpc)),
        Err(e) => Err((
            DaemonError::SpawnFailed {
                detail: format!("connect to {endpoint} failed: {e}"),
            },
            Some(spawned),
        )),
    }
}

/// Steps 5-7 with a connected rpc: initialize → `thread/resume{thread_id}` →
/// update the registry row. The HONEST EDGE: a "no rollout found" protocol error on
/// resume kills the daemon + returns `NoResumableHistory` WITHOUT fabricating a new
/// thread (m2 identity preserved) + writes NO row change.
fn finish_revive(
    deps: &ReviveDeps,
    params: &ResumeParams,
    spawned: SpawnedDaemon,
    endpoint: &str,
    rpc: &dyn AppServerRpc,
) -> Result<ResumeOutcome, ResumeError> {
    // Step 5: initialize handshake (readiness) + the bare `initialized`.
    let client = ClientInfo {
        name: CLIENT_NAME.to_string(),
        title: None,
        version: "0".to_string(),
    };
    if let Err(e) = rpc.initialize(&client) {
        deps.spawner.kill(spawned.pid);
        return Err(ResumeError::Daemon(DaemonError::HandshakeFailed {
            detail: format!("initialize: {e}"),
        }));
    }
    let _ = rpc.initialized();

    // Step 6: thread/resume{thread_id} — hydrate from the on-disk rollout. This is
    // the revive primitive (NOT thread/start — fabricating a fresh thread would
    // break the m2 identity the row records). The HONEST EDGE is here.
    if let Err(e) = rpc.thread_resume(&params.thread_id) {
        // Always kill the just-spawned daemon on a resume failure (no orphan).
        deps.spawner.kill(spawned.pid);
        // "no rollout found" → the clean agent-facing NoResumableHistory (the
        // thread never persisted a rollout — W4 lazy-materialization). Matched on
        // the message PREFIX (the code -32600 is shared with the steer fence).
        if is_no_rollout(&e) {
            return Err(ResumeError::NoResumableHistory {
                name: params.name.clone(),
            });
        }
        return Err(ResumeError::Other {
            detail: format!("resume failed: {e}"),
        });
    }

    // Step 7: UPDATE the registry row — new pid (file name changes), new endpoint,
    // updatedAt. The OLD row file (keyed by the dead pid) is left for reconcile/gc
    // to sweep (a dead-pid row is reconcile's job; we do NOT delete it here — the
    // create precedent never deletes a foreign row, and the dead pid's row is
    // harmless until reconcile/tombstone runs). The session id (thread id) is
    // PRESERVED (m2).
    let now = deps.clock.now_ms();
    let entry = RegistryEntry {
        pid: Some(spawned.pid),
        session_id: Some(params.thread_id.clone()),
        cwd: params.cwd.clone(),
        // startedAt: a revive is a NEW daemon process — stamp startedAt to now (the
        // OLD startedAt belonged to the dead daemon; the row's identity is the
        // thread, not the process, but the process timestamps track the live daemon).
        started_at: Some(now),
        updated_at: Some(now),
        status: Some("idle".to_string()),
        name: Some(params.name.clone()),
        version: None,
        kind: None,
        entrypoint: None,
        backend: None,
        spawned_by: None,
        provider: Some("codex".to_string()),
        endpoint: Some(endpoint.to_string()),
        // scoped-ACP-CC: a resumed healthy daemon row carries no degradation latch.
        transport: None,
        // Child B (opencode D1): a resumed healthy row carries no send-history bit.
        structured_send_issued: None,
        // THE LANE, PRESERVED. This row is a full rewrite, not a patch, so a
        // `None` here would not leave the stamp alone — it would erase it, and
        // the session would come back as whatever the row default answers that
        // day. See [`ResumeParams::hosting`] for the demotion that caused.
        hosting: params.hosting.clone(),
        // No second server to hand out: this harness's residence IS qd's
        // (`endpoint`), so there is no separate harness-side address a viewer
        // could join. See `RegistryEntry::harness_endpoint`.
        harness_endpoint: None,
    };
    if let Err(e) = registry::write_entry(&deps.sessions_dir, &entry) {
        deps.spawner.kill(spawned.pid);
        return Err(ResumeError::Other {
            detail: format!(
                "revived the codex daemon but its registry row could not be written ({e}); \
                 the daemon was stopped."
            ),
        });
    }

    // Best-effort close of our short-lived client (the daemon stays up — drivable).
    let _ = rpc.close();

    Ok(ResumeOutcome::Revived {
        pid: spawned.pid,
        endpoint: endpoint.to_string(),
    })
}

/// Is this rpc error the "no rollout found" resume fence? Matched on the protocol
/// message PREFIX (the -32600 code is shared with the steer fence, so the code
/// alone is ambiguous; the "no rollout found" text is the stable discriminator,
/// q1c-clientB-events.jsonl line 4).
fn is_no_rollout(e: &RpcError) -> bool {
    match e {
        RpcError::Protocol(se) => se.message.trim_start().starts_with(NO_ROLLOUT_PREFIX),
        _ => false,
    }
}

/// Connect with a bounded retry (the just-spawned daemon needs a moment to listen)
/// — identical to create_daemon's `connect_with_retry`.
fn connect_with_retry<'a>(
    connect: &RpcConnector<'a>,
    endpoint: &str,
    budget: std::time::Duration,
    clock: &dyn Clock,
) -> Result<Box<dyn AppServerRpc + 'a>, RpcError> {
    let start = clock.now_ms();
    let budget_ms = budget.as_millis() as i64;
    loop {
        match connect(endpoint) {
            Ok(rpc) => return Ok(rpc),
            Err(e) => {
                if clock.now_ms() - start >= budget_ms {
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// A throwaway `QdPaths` for the launch_plan fx — codex's launch_plan reads ONLY
/// env, never paths (the minimal-fx negative control).
fn placeholder_paths() -> crate::paths::QdPaths {
    crate::paths::QdPaths::from_home(std::path::Path::new(""))
}

// ===========================================================================
// Real production seams (the verb hands these in).
// ===========================================================================

/// The production revive spawner — the SAME `RealDaemonSpawner` the create path
/// uses (its `kill` is the GROUP-pgid ladder reused for revive-respawn cleanup AND
/// for the kill verb). The verb hands the port allocator in as the create path's
/// [`crate::create_daemon::real_alloc_port`] closure (the `RpcConnector` /
/// `PortAllocator` are passed as closures at the verb layer, mirroring W4/W6).
pub fn real_spawner() -> RealDaemonSpawner {
    RealDaemonSpawner
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::create_daemon::SpawnedDaemon;
    use crate::effects::{FixedClock, MapEnv};
    use crate::exec::ScriptedExec;
    use crate::provider::codex::{
        ClientInfo, InitializeResult, Notification, RpcError, ServerError, SteerOutcome,
        INVALID_REQUEST_CODE,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use tempfile::TempDir;

    // === A fixture AppServerRpc for the revive path: scripted resume outcome. ===

    struct FixtureRpc {
        initialize_err: Option<RpcError>,
        // The thread/resume outcome (Ok = hydrated; Err = the fence / transport).
        thread_resume: Result<(), RpcErrorKind>,
        calls: RefCell<Vec<String>>,
    }

    /// A cloneable RpcError stand-in (RpcError is not Clone — the fixture stores a
    /// recipe and rebuilds the error per call).
    #[derive(Clone)]
    enum RpcErrorKind {
        NoRollout,
        OtherProtocol,
        Transport,
    }

    impl FixtureRpc {
        fn happy() -> Self {
            FixtureRpc {
                initialize_err: None,
                thread_resume: Ok(()),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn resume_err(kind: RpcErrorKind) -> Self {
            FixtureRpc {
                initialize_err: None,
                thread_resume: Err(kind),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl AppServerRpc for FixtureRpc {
        fn initialize(&self, _c: &ClientInfo) -> Result<InitializeResult, RpcError> {
            self.calls.borrow_mut().push("initialize".into());
            match &self.initialize_err {
                Some(_) => Err(RpcError::Transport("init boom".into())),
                None => Ok(InitializeResult::default()),
            }
        }
        fn initialized(&self) -> Result<(), RpcError> {
            self.calls.borrow_mut().push("initialized".into());
            Ok(())
        }
        fn thread_start(&self, _cwd: &str, _ap: &str, _qd: &str) -> Result<String, RpcError> {
            // MUTATION EVIDENCE (identity preservation): the revive path must NEVER
            // call thread_start (that would mint a NEW thread id, breaking m2). If
            // the no-rollout handling were "fabricate a thread", this would be
            // reached — the no_rollout test asserts thread_start is NEVER in calls.
            self.calls.borrow_mut().push("thread_start".into());
            unreachable!("revive must never thread_start — it would break m2 identity");
        }
        fn thread_resume(&self, id: &str) -> Result<(), RpcError> {
            self.calls.borrow_mut().push(format!("thread_resume({id})"));
            match &self.thread_resume {
                Ok(()) => Ok(()),
                Err(RpcErrorKind::NoRollout) => Err(RpcError::Protocol(ServerError {
                    code: INVALID_REQUEST_CODE,
                    message: format!("no rollout found for thread id {id}"),
                })),
                Err(RpcErrorKind::OtherProtocol) => Err(RpcError::Protocol(ServerError {
                    code: INVALID_REQUEST_CODE,
                    message: "some other precondition failed".into(),
                })),
                Err(RpcErrorKind::Transport) => Err(RpcError::Transport("resume boom".into())),
            }
        }
        fn turn_start(&self, _t: &str, _x: &str) -> Result<String, RpcError> {
            unreachable!("revive never starts a turn")
        }
        fn turn_steer(&self, _t: &str, _e: &str, _x: &str) -> Result<SteerOutcome, RpcError> {
            unreachable!()
        }
        fn turn_interrupt(&self, _t: &str, _u: &str) -> Result<(), RpcError> {
            unreachable!()
        }
        fn next_notification(
            &self,
            _t: std::time::Duration,
        ) -> Result<Option<Notification>, RpcError> {
            Ok(None)
        }
        fn close(&self) -> Result<(), RpcError> {
            self.calls.borrow_mut().push("close".into());
            Ok(())
        }
    }

    // --- A fake spawner: records spawns + kills, hands back a canned pid. ---

    struct FakeSpawner {
        pid: i64,
        spawns: RefCell<Vec<(Vec<String>, PathBuf)>>,
        envs: RefCell<Vec<Vec<(String, String)>>>,
        kills: RefCell<Vec<i64>>,
    }
    impl FakeSpawner {
        fn ok(pid: i64) -> Self {
            FakeSpawner {
                pid,
                spawns: RefCell::new(Vec::new()),
                envs: RefCell::new(Vec::new()),
                kills: RefCell::new(Vec::new()),
            }
        }
        fn spawn_count(&self) -> usize {
            self.spawns.borrow().len()
        }
        fn kills(&self) -> Vec<i64> {
            self.kills.borrow().clone()
        }
        fn last_argv(&self) -> Vec<String> {
            self.spawns.borrow().last().unwrap().0.clone()
        }
        fn last_env(&self) -> Vec<(String, String)> {
            self.envs.borrow().last().unwrap().clone()
        }
    }
    impl DaemonSpawner for FakeSpawner {
        fn spawn_detached(
            &self,
            argv: &[String],
            env: &[(String, String)],
            _cwd: &std::path::Path,
            log_path: &std::path::Path,
        ) -> std::io::Result<SpawnedDaemon> {
            self.spawns
                .borrow_mut()
                .push((argv.to_vec(), log_path.to_path_buf()));
            self.envs.borrow_mut().push(env.to_vec());
            Ok(SpawnedDaemon { pid: self.pid })
        }
        fn kill(&self, pid: i64) {
            self.kills.borrow_mut().push(pid);
        }
    }

    /// codex --version sniff EXACT (the pinned binary; re-pinned 0.143.0-alpha.14
    /// → 0.146.1 — see doc/codex-repin-log.md).
    fn exact_exec() -> ScriptedExec {
        ScriptedExec::new().on("codex", &["--version"], Some(0), "codex-cli 0.146.1\n", "")
    }

    struct Harness {
        _tmp: TempDir,
        sessions_dir: PathBuf,
        log_dir: PathBuf,
        ids_path: PathBuf,
        env: MapEnv,
        clock: FixedClock,
    }
    fn harness() -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        Harness {
            sessions_dir: tmp.path().join("sessions"),
            log_dir: tmp.path().join("log"),
            ids_path: tmp.path().join("state").join("ids.jsonl"),
            env: MapEnv {
                vars: HashMap::new(),
                uid: 501,
            },
            clock: FixedClock(1_700_000_000_000),
            _tmp: tmp,
        }
    }

    // A connector forwarding to a SHARED &FixtureRpc (so the test can assert on it
    // after) — the create_daemon test's RpcRef pattern.
    struct RpcRef<'a>(&'a FixtureRpc);
    impl AppServerRpc for RpcRef<'_> {
        fn initialize(&self, c: &ClientInfo) -> Result<InitializeResult, RpcError> {
            self.0.initialize(c)
        }
        fn initialized(&self) -> Result<(), RpcError> {
            self.0.initialized()
        }
        fn thread_start(&self, cwd: &str, ap: &str, qd: &str) -> Result<String, RpcError> {
            self.0.thread_start(cwd, ap, qd)
        }
        fn thread_resume(&self, id: &str) -> Result<(), RpcError> {
            self.0.thread_resume(id)
        }
        fn turn_start(&self, t: &str, x: &str) -> Result<String, RpcError> {
            self.0.turn_start(t, x)
        }
        fn turn_steer(&self, t: &str, e: &str, x: &str) -> Result<SteerOutcome, RpcError> {
            self.0.turn_steer(t, e, x)
        }
        fn turn_interrupt(&self, t: &str, u: &str) -> Result<(), RpcError> {
            self.0.turn_interrupt(t, u)
        }
        fn next_notification(
            &self,
            d: std::time::Duration,
        ) -> Result<Option<Notification>, RpcError> {
            self.0.next_notification(d)
        }
        fn close(&self) -> Result<(), RpcError> {
            self.0.close()
        }
    }

    fn revive_params(name: &str, thread_id: &str) -> ResumeParams {
        ResumeParams {
            name: name.to_string(),
            thread_id: thread_id.to_string(),
            cwd: Some("/work/proj".to_string()),
            // Dead/cold by default (no live pid) → revive.
            current_pid: None,
            current_endpoint: None,
            // The default shared by most of these units: a `codex/daemon` row,
            // stamped the way the create stamps it. The lane-preservation test
            // below overrides it.
            hosting: Some(crate::lane::Mode::Daemon.hosting_token().to_string()),
        }
    }

    // === W9 FIX M-1/Mo-2: cmdline-identity probe test doubles. ===
    //
    // A probe that always reports a cmdline that MATCHES our codex daemon for the
    // given endpoint (so an alive pid is believed-ours). Used by the revive paths
    // where the pid is dead (probe never consulted) and by the alive-is-ours test.
    fn probe_matching(endpoint: &str) -> impl Fn(i64) -> Option<String> {
        let ep = endpoint.to_string();
        move |_pid| Some(format!("codex app-server --listen {ep}"))
    }
    /// A probe that reports a NON-matching cmdline (a reused pid that is now some
    /// unrelated process) — the foreign-group / false-AlreadyRunning hazard.
    fn probe_foreign() -> impl Fn(i64) -> Option<String> {
        |_pid| Some("/usr/bin/some-unrelated-daemon --serve".to_string())
    }
    /// A probe that reports the pid is not visible at all (absent / read failed).
    fn probe_absent() -> impl Fn(i64) -> Option<String> {
        |_pid| None
    }

    // ====================================================================
    // KILL (deliverable A) tests.
    // ====================================================================

    // === Alive codex daemon → group-kill called with the right pgid + tombstone
    //     written carrying provider + endpoint. ===
    //
    // We cannot make a real pid "alive" deterministically; instead we assert the
    // ALREADY-DEAD seal here (no kill, still tombstone) and the GROUP-pgid argument
    // wiring in the live lane. For the alive path the unit asserts the tombstone
    // carries provider + endpoint from the captured row.
    #[test]
    fn kill_already_dead_no_kill_still_tombstone() {
        let h = harness();
        std::fs::create_dir_all(&h.sessions_dir).unwrap();
        let spawner = FakeSpawner::ok(0);
        // A pid that is NOT alive (negative ⇒ is_pid_alive false; pid>0 for the
        // file name — use a real-but-dead pid by picking a very large unlikely pid).
        let dead_pid = 2_000_000_001i64; // > any real pid on a 32-bit pid host
        let captured = RegistryEntry {
            pid: Some(dead_pid),
            session_id: Some("T-DEAD".into()),
            cwd: Some("/work/proj".into()),
            status: Some("idle".into()),
            name: Some("cdx".into()),
            provider: Some("codex".into()),
            endpoint: Some("ws://127.0.0.1:18970".into()),
            ..Default::default()
        };
        let probe = probe_absent();
        let out = kill_codex(&h.sessions_dir, dead_pid, Some(&captured), &spawner, &probe);
        assert!(!out.was_alive, "a dead pid is reported not-alive");
        // NO kill signal sent (the daemon was already dead).
        assert!(spawner.kills().is_empty(), "no kill on an already-dead row");
        // Still tombstoned (the dead-row seal) — synthesized from the captured row.
        let tomb = h.sessions_dir.join(format!("{dead_pid}.json.tombstoned"));
        assert!(tomb.exists(), "tombstone written for the dead row");
        let body = std::fs::read_to_string(&tomb).unwrap();
        // The tombstone carries provider + endpoint (the audit trail).
        assert!(
            body.contains("\"provider\": \"codex\""),
            "tombstone: {body}"
        );
        assert!(
            body.contains("ws://127.0.0.1:18970"),
            "tombstone carries endpoint: {body}"
        );
    }

    // === Already tombstoned → idempotent no-op (no second tombstone, no kill). ===
    #[test]
    fn kill_idempotent_when_already_tombstoned() {
        let h = harness();
        std::fs::create_dir_all(&h.sessions_dir).unwrap();
        let dead_pid = 2_000_000_002i64;
        let tomb = h.sessions_dir.join(format!("{dead_pid}.json.tombstoned"));
        std::fs::write(&tomb, "{\"provider\":\"codex\"}").unwrap();
        let spawner = FakeSpawner::ok(0);
        let probe = probe_absent();
        let out = kill_codex(&h.sessions_dir, dead_pid, None, &spawner, &probe);
        assert!(!out.was_alive);
        assert!(spawner.kills().is_empty());
        // The pre-existing tombstone is untouched (idempotent).
        assert_eq!(
            std::fs::read_to_string(&tomb).unwrap(),
            "{\"provider\":\"codex\"}"
        );
    }

    // === Live <pid>.json present → ensure_tombstone RENAMES it (the live-row seal).
    //     We model "alive" by the row file presence (kill_codex's is_pid_alive on a
    //     dead pid won't fire spawner.kill, but ensure_tombstone still renames the
    //     live file). This proves the rename-not-synthesize branch. ===
    #[test]
    fn kill_renames_live_row_file_to_tombstone() {
        let h = harness();
        std::fs::create_dir_all(&h.sessions_dir).unwrap();
        let dead_pid = 2_000_000_003i64;
        let row = RegistryEntry {
            pid: Some(dead_pid),
            session_id: Some("T-LIVEFILE".into()),
            provider: Some("codex".into()),
            endpoint: Some("ws://127.0.0.1:18971".into()),
            name: Some("cdx".into()),
            ..Default::default()
        };
        registry::write_entry(&h.sessions_dir, &row).unwrap();
        let json_path = h.sessions_dir.join(format!("{dead_pid}.json"));
        assert!(json_path.exists());
        let spawner = FakeSpawner::ok(0);
        let probe = probe_absent();
        let _ = kill_codex(&h.sessions_dir, dead_pid, Some(&row), &spawner, &probe);
        // The live <pid>.json was RENAMED to .tombstoned (not synthesized).
        assert!(!json_path.exists(), "live row file consumed by the rename");
        let tomb = h.sessions_dir.join(format!("{dead_pid}.json.tombstoned"));
        assert!(tomb.exists(), "tombstone is the renamed live file");
        let body = std::fs::read_to_string(&tomb).unwrap();
        assert!(body.contains("ws://127.0.0.1:18971"));
    }

    // === W9 FIX M-1 (the no-foreign-group-signal proof): a LIVE recorded pid whose
    //     command line does NOT match our codex daemon (the exact-pid-reuse hazard:
    //     the recorded pid was recycled to an UNRELATED group leader). The kill MUST
    //     NOT group-signal that pid — it tombstones ONLY. ===
    //
    // MUTATION EVIDENCE: we use OUR OWN pid (guaranteed alive) + a probe reporting a
    // FOREIGN cmdline. The pre-fix code group-SIGKILLed any live recorded pid →
    // `spawner.kill` would record our pid (a foreign-group signal). The fix gates on
    // `cmdline_is_our_daemon`; a non-match means NO signal. If the identity guard
    // were removed, `spawner.kills()` would contain our pid and this reds.
    #[test]
    fn kill_reused_pid_not_our_daemon_no_group_signal() {
        let h = harness();
        std::fs::create_dir_all(&h.sessions_dir).unwrap();
        let spawner = FakeSpawner::ok(0);
        let my_pid = std::process::id() as i64; // guaranteed ALIVE
        let captured = RegistryEntry {
            pid: Some(my_pid),
            session_id: Some("T-REUSED".into()),
            cwd: Some("/work/proj".into()),
            status: Some("idle".into()),
            name: Some("cdx".into()),
            provider: Some("codex".into()),
            endpoint: Some("ws://127.0.0.1:18982".into()),
            ..Default::default()
        };
        // The live pid is alive, but its cmdline is a FOREIGN process (reused pid).
        let probe = probe_foreign();
        let out = kill_codex(&h.sessions_dir, my_pid, Some(&captured), &spawner, &probe);
        // NOT believed-ours → reported not-"alive" for kill purposes.
        assert!(
            !out.was_alive,
            "a reused (foreign) pid is NOT treated as our live daemon"
        );
        // THE PROOF: NO group signal was sent (never SIGKILL a foreign group).
        assert!(
            spawner.kills().is_empty(),
            "a reused pid must NEVER be group-signaled (would reap a foreign group): {:?}",
            spawner.kills()
        );
        // Still tombstoned (the daemon is effectively gone → the dead-row seal).
        let tomb = h.sessions_dir.join(format!("{my_pid}.json.tombstoned"));
        assert!(tomb.exists(), "tombstone written even for a reused-pid row");
    }

    // === W9 FIX M-1 control: a LIVE recorded pid whose cmdline MATCHES our codex
    //     daemon (the endpoint appears) → the group ladder proceeds. ===
    #[test]
    fn kill_matching_live_pid_group_signals() {
        let h = harness();
        std::fs::create_dir_all(&h.sessions_dir).unwrap();
        let spawner = FakeSpawner::ok(0);
        let my_pid = std::process::id() as i64; // guaranteed ALIVE
        let captured = RegistryEntry {
            pid: Some(my_pid),
            session_id: Some("T-MATCH".into()),
            cwd: Some("/work/proj".into()),
            status: Some("idle".into()),
            name: Some("cdx".into()),
            provider: Some("codex".into()),
            endpoint: Some("ws://127.0.0.1:18983".into()),
            ..Default::default()
        };
        // A cmdline that matches our daemon for THIS endpoint.
        let probe = probe_matching("ws://127.0.0.1:18983");
        let out = kill_codex(&h.sessions_dir, my_pid, Some(&captured), &spawner, &probe);
        assert!(out.was_alive, "a matching live pid IS our daemon");
        // The group ladder fired against OUR pid (the matched case keeps the W4
        // launcher-orphan reason).
        assert_eq!(
            spawner.kills(),
            vec![my_pid],
            "the matched live daemon is group-signaled"
        );
    }

    // ====================================================================
    // RESUME (deliverable B) tests.
    // ====================================================================

    // === Daemon ALIVE (live pid + endpoint) → no-op success, NO spawn, NO attach.
    //     We use OUR OWN pid (guaranteed alive) as the current_pid. ===
    #[test]
    fn resume_alive_is_noop_no_spawn() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy();
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18972u16);
        let spawner = FakeSpawner::ok(60001);
        // A probe whose cmdline MATCHES our daemon for the recorded endpoint → the
        // alive pid is believed-ours (AlreadyRunning).
        let probe = probe_matching("ws://127.0.0.1:18999");
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let my_pid = std::process::id() as i64; // guaranteed alive
        let params = ResumeParams {
            name: "cdx".into(),
            thread_id: "T-ALIVE".into(),
            cwd: Some("/work/proj".into()),
            current_pid: Some(my_pid),
            current_endpoint: Some("ws://127.0.0.1:18999".into()),
            hosting: Some(crate::lane::Mode::Daemon.hosting_token().to_string()),
        };
        let out = resume_codex(&deps, &params).expect("alive → no-op success");
        assert_eq!(out, ResumeOutcome::AlreadyRunning);
        // NO spawn (already drivable), and the fixture rpc was NEVER touched (no
        // initialize/resume/attach — there is no attach API; the alive path returns
        // before any rpc/spawn).
        assert_eq!(spawner.spawn_count(), 0, "alive → no respawn");
        assert!(spawner.kills().is_empty(), "alive → no kill");
        assert!(rpc.calls().is_empty(), "alive → no rpc at all (no attach)");
    }

    // === P0 QA (spec-w4-qa A4, codex column): stop→resume round-trip ×3 — the
    //     stable id rides through THREE consecutive revives unchanged, and no
    //     extra id is ever minted (mint_or_get keyed by the preserved thread
    //     uuid; orc Q4's identity-preservation contract at the codex seam). ===
    #[test]
    fn three_consecutive_revives_keep_the_same_stable_id() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy();
        // The thread's id exists from create (the create-side bind).
        let mut g = || "cd47qrst".to_string();
        crate::idstore::mint_or_get_with(&h.ids_path, "T-X3", Some("cdx"), &h.clock, &mut g)
            .unwrap();
        for cycle in 1..=3u32 {
            // Per-cycle deps (all borrows share ReviveDeps' one lifetime).
            let connect = |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
                Ok(Box::new(RpcRef(&rpc)))
            };
            let alloc = || Ok(18990u16);
            let probe = probe_absent();
            let spawner = FakeSpawner::ok(60100 + cycle as i64);
            let deps = ReviveDeps {
                provider: &crate::provider::codex::CODEX_PROVIDER,
                env: &h.env,
                exec: &exec,
                clock: &h.clock,
                sessions_dir: h.sessions_dir.clone(),
                log_dir: h.log_dir.clone(),
                spawner: &spawner,
                connect: &connect,
                alloc_port: &alloc,
                cmdline_probe: &probe,
                ids_path: h.ids_path.clone(),
            };
            let params = ResumeParams {
                name: "cdx".into(),
                thread_id: "T-X3".into(),
                cwd: Some("/work/proj".into()),
                current_pid: None, // cold every cycle → revive
                current_endpoint: None,
                hosting: Some(crate::lane::Mode::Daemon.hosting_token().to_string()),
            };
            let out = resume_codex(&deps, &params).expect("cold → revive");
            assert!(
                matches!(out, ResumeOutcome::Revived { .. }),
                "cycle {cycle} revives"
            );
            let env = spawner.last_env();
            assert!(
                env.iter()
                    .any(|(k, v)| k == "QD_SESSION_ID" && v == "cd47qrst"),
                "cycle {cycle}: the revived env carries the SAME id: {env:?}"
            );
        }
        let ids = crate::idstore::fold(&h.ids_path);
        assert_eq!(
            ids.by_session.get("T-X3"),
            Some(&"cd47qrst".to_string()),
            "the binding never moved"
        );
        assert_eq!(ids.by_id.len(), 1, "three revives minted ZERO extra ids");
    }

    // === Daemon alive-pid but NO endpoint → NOT drivable → revive (the endpoint is
    //     what makes it drivable; an alive pid with no channel is revived). ===
    #[test]
    fn resume_alive_pid_no_endpoint_revives() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy();
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18973u16);
        let spawner = FakeSpawner::ok(60002);
        let probe = probe_absent();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let my_pid = std::process::id() as i64;
        let params = ResumeParams {
            name: "cdx".into(),
            thread_id: "T-NOEP".into(),
            cwd: Some("/work/proj".into()),
            current_pid: Some(my_pid),
            current_endpoint: None, // no channel → not drivable → revive
            hosting: Some(crate::lane::Mode::Daemon.hosting_token().to_string()),
        };
        let out = resume_codex(&deps, &params).expect("no-endpoint → revive");
        assert!(matches!(out, ResumeOutcome::Revived { .. }));
        assert_eq!(spawner.spawn_count(), 1, "revived → one spawn");
    }

    // === P0 wave-2 (spec-w2-env D1 site 3): the revived daemon's env carries
    //     the SAME stable id across every resume (mint_or_get keyed by the
    //     thread uuid — a pre-existing mint rides through unchanged). ===
    #[test]
    fn revived_daemon_env_carries_the_existing_stable_id() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy();
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18985u16);
        let spawner = FakeSpawner::ok(60012);
        let probe = probe_absent();
        // The thread already has a stable id (minted at create / an earlier ls).
        let mut g = || "cd47qrst".to_string();
        crate::idstore::mint_or_get_with(&h.ids_path, "T-IDS", Some("cdx"), &h.clock, &mut g)
            .unwrap();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let params = ResumeParams {
            name: "cdx".into(),
            thread_id: "T-IDS".into(),
            cwd: Some("/work/proj".into()),
            current_pid: None, // cold → revive
            current_endpoint: None,
            hosting: Some(crate::lane::Mode::Daemon.hosting_token().to_string()),
        };
        resume_codex(&deps, &params).expect("cold → revive");
        let env = spawner.last_env();
        assert!(
            env.iter()
                .any(|(k, v)| k == "QD_SESSION_ID" && v == "cd47qrst"),
            "the revived env carries the EXISTING id unchanged: {env:?}"
        );
    }

    // === W9 FIX Mo-2 (the false-AlreadyRunning defusal): a LIVE recorded pid + a
    //     recorded endpoint, but the live cmdline does NOT match our codex daemon
    //     (exact-pid-reuse — the recorded pid is now an UNRELATED process). resume
    //     MUST treat it as dead and REVIVE, never falsely report AlreadyRunning. ===
    //
    // MUTATION EVIDENCE: OUR OWN pid (alive) + endpoint set + a FOREIGN probe. The
    // pre-fix gate (pid alive + endpoint set) reported AlreadyRunning here — a false
    // "running" against a recycled pid. The fix adds `cmdline_is_our_daemon`; a
    // non-match → revive. Removing the identity guard makes this red (AlreadyRunning,
    // spawn_count 0).
    #[test]
    fn resume_reused_pid_not_our_daemon_revives_not_false_alreadyrunning() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy();
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18984u16);
        let spawner = FakeSpawner::ok(60011);
        // The live pid is alive + has an endpoint, but its cmdline is FOREIGN.
        let probe = probe_foreign();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let my_pid = std::process::id() as i64; // guaranteed ALIVE
        let params = ResumeParams {
            name: "cdx".into(),
            thread_id: "T-REUSED-RESUME".into(),
            cwd: Some("/work/proj".into()),
            current_pid: Some(my_pid),
            current_endpoint: Some("ws://127.0.0.1:18999".into()),
            hosting: Some(crate::lane::Mode::Daemon.hosting_token().to_string()),
        };
        let out = resume_codex(&deps, &params).expect("reused pid → revive (not AlreadyRunning)");
        // THE PROOF: REVIVED, not a false AlreadyRunning.
        assert!(
            matches!(out, ResumeOutcome::Revived { .. }),
            "a reused (foreign) live pid must REVIVE, never false AlreadyRunning: {out:?}"
        );
        assert_eq!(spawner.spawn_count(), 1, "the foreign pid was revived past");
    }

    // === Daemon DEAD + rollout exists → respawn + thread_resume + row updated
    //     (assert new pid/endpoint, session id PRESERVED, NO thread_start). ===
    #[test]
    fn resume_dead_revives_and_updates_row() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy(); // thread_resume Ok = rollout hydrates
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18974u16);
        let spawner = FakeSpawner::ok(60003);
        let probe = probe_absent();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        // A dead recorded pid (huge, unlikely-alive) → revive.
        let params = ResumeParams {
            name: "cdx".into(),
            thread_id: "T-REAL-UUID".into(),
            cwd: Some("/work/proj".into()),
            current_pid: Some(2_000_000_010),
            current_endpoint: Some("ws://127.0.0.1:18000".into()),
            hosting: Some(crate::lane::Mode::Daemon.hosting_token().to_string()),
        };
        let out = resume_codex(&deps, &params).expect("dead+rollout → revive");
        match out {
            ResumeOutcome::Revived { pid, endpoint } => {
                assert_eq!(pid, 60003, "new daemon pid");
                assert_eq!(endpoint, "ws://127.0.0.1:18974", "new endpoint");
            }
            other => panic!("expected Revived, got {other:?}"),
        }
        // The revive used thread_resume (NOT thread_start — m2 identity).
        assert!(
            rpc.calls()
                .iter()
                .any(|c| c == "thread_resume(T-REAL-UUID)"),
            "thread_resume drove the revive: {:?}",
            rpc.calls()
        );
        assert!(
            !rpc.calls().iter().any(|c| c == "thread_start"),
            "revive must NOT thread_start (m2 identity): {:?}",
            rpc.calls()
        );
        // The NEW row (keyed by the NEW pid) carries the PRESERVED thread id +
        // the new endpoint + provider:"codex".
        let row = registry::read_entry(&h.sessions_dir, 60003).expect("revived row written");
        assert_eq!(row.pid, Some(60003));
        assert_eq!(
            row.session_id.as_deref(),
            Some("T-REAL-UUID"),
            "m2 preserved"
        );
        assert_eq!(row.endpoint.as_deref(), Some("ws://127.0.0.1:18974"));
        assert_eq!(row.provider.as_deref(), Some("codex"));
        assert_eq!(row.status.as_deref(), Some("idle"));
        assert_eq!(row.updated_at, Some(1_700_000_000_000));
        // No kill on the happy revive.
        assert!(spawner.kills().is_empty(), "no kill on a successful revive");
    }

    // === A REVIVE MAY NOT CHANGE THE ROW'S LANE. ===
    //
    // The row is REWRITTEN by step 7, not patched, so every field it does not
    // restate is erased rather than left alone — and `hosting` is the field a
    // row's LANE is derived from (`crate::lane::lane_for`). Before
    // `ResumeParams::hosting` existed this write hard-coded `None`, so
    // `qd resume` on a `codex/app-server` session handed back a row that
    // re-derived to `codex/daemon`: same process, same thread, same endpoint —
    // and no longer attachable, because `codex/daemon`'s attach is the "a
    // daemon-hosted session has no terminal of its own" refusal and
    // `codex/app-server` exists precisely to have one.
    //
    // Latent while the lane was reachable only over qw's wire; user-visible the
    // moment `codex/app-server` is what `qd start --provider codex` creates.
    //
    // WHAT THIS ASSERTS, and why it is worth two revives rather than one: the
    // stamp must SURVIVE for app-server AND must not be invented for daemon.
    // A revive that wrote `"app-server"` unconditionally would pass the first
    // half and promote every plain daemon row, which is the same bug pointing
    // the other way.
    //
    // The expected values are `Mode::…hosting_token()`, the SAME expression
    // `LaneImpl::start_codex_app_server` / `start_codex_daemon` pass into
    // `DaemonParams::hosting` on the create side. That is the byte-identity
    // claim stated as code: a resumed row and a freshly created row of the same
    // lane carry the same token, and a rename of either token moves both sides
    // together instead of turning this green while the lanes drift apart.
    #[test]
    fn a_revive_preserves_the_rows_lane_stamp() {
        for mode in [crate::lane::Mode::AppServer, crate::lane::Mode::Daemon] {
            let h = harness();
            let exec = exact_exec();
            let rpc = FixtureRpc::happy();
            let connect = |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
                Ok(Box::new(RpcRef(&rpc)))
            };
            let alloc = || Ok(18991u16);
            let spawner = FakeSpawner::ok(60031);
            let probe = probe_absent();
            let deps = ReviveDeps {
                provider: &crate::provider::codex::CODEX_PROVIDER,
                env: &h.env,
                exec: &exec,
                clock: &h.clock,
                sessions_dir: h.sessions_dir.clone(),
                log_dir: h.log_dir.clone(),
                spawner: &spawner,
                connect: &connect,
                alloc_port: &alloc,
                cmdline_probe: &probe,
                ids_path: h.ids_path.clone(),
            };
            let params = ResumeParams {
                name: "cdx".into(),
                thread_id: "T-LANE".into(),
                cwd: Some("/work/proj".into()),
                // Cold → the revive path, which is the path that rewrites the row.
                current_pid: None,
                current_endpoint: None,
                hosting: Some(mode.hosting_token().to_string()),
            };
            let out = resume_codex(&deps, &params).expect("cold → revive");
            assert!(matches!(out, ResumeOutcome::Revived { .. }), "{mode:?}");

            let row = registry::read_entry(&h.sessions_dir, 60031).expect("revived row written");
            assert_eq!(
                row.hosting.as_deref(),
                Some(mode.hosting_token()),
                "{mode:?}: the revived row must carry the SAME hosting token a \
                 create of this lane writes"
            );
            // The stamp is only interesting for what it DERIVES to. This is the
            // read every later verb performs on the row, so asserting it here is
            // asserting the thing the user actually loses.
            assert_eq!(
                crate::lane::lane_for(
                    row.provider.as_deref().unwrap_or_default(),
                    row.hosting.as_deref()
                ),
                Some(crate::lane::Lane::new(crate::lane::Harness::Codex, mode).expect("real lane")),
                "{mode:?}: a revive must not change the row's lane"
            );
        }
    }

    // === Cold/foreign row (no pid at all) → the SAME revive path. ===
    #[test]
    fn resume_cold_row_revives() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy();
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18975u16);
        let spawner = FakeSpawner::ok(60004);
        let probe = probe_absent();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        // current_pid None + current_endpoint None = a cold/foreign discovered row.
        let params = revive_params("cdx-cold", "T-COLD-UUID");
        let out = resume_codex(&deps, &params).expect("cold row → revive (first-class)");
        assert!(matches!(out, ResumeOutcome::Revived { pid: 60004, .. }));
        let row = registry::read_entry(&h.sessions_dir, 60004).expect("revived cold row");
        assert_eq!(row.session_id.as_deref(), Some("T-COLD-UUID"));
    }

    // === HONEST EDGE "no rollout found" → clean error + spawned daemon killed +
    //     NO row change + NO thread_start (identity-preservation MUTATION EVIDENCE). ===
    //
    // MUTATION EVIDENCE: if the no-rollout path FABRICATED a thread (called
    // thread_start to mint a fresh thread id), the FixtureRpc::thread_start panics
    // (`unreachable!`), reddening this test — proving the path does NOT fabricate.
    // The row file (keyed by the spawned pid) must be ABSENT (no row change), and
    // the just-spawned daemon must have been killed (group ladder → spawner.kill).
    #[test]
    fn resume_no_rollout_clean_error_kills_daemon_no_row() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::resume_err(RpcErrorKind::NoRollout);
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18976u16);
        let spawner = FakeSpawner::ok(60005);
        let probe = probe_absent();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let params = revive_params("cdx-nh", "T-NO-HISTORY");
        let err = resume_codex(&deps, &params).unwrap_err();
        match &err {
            ResumeError::NoResumableHistory { name } => assert_eq!(name, "cdx-nh"),
            other => panic!("expected NoResumableHistory, got {other:?}"),
        }
        // The message is agent-facing + names the session + says "no resumable
        // history" — NO attach/start/steer vocabulary.
        let msg = err.to_string();
        assert!(msg.contains("cdx-nh"), "msg names session: {msg}");
        assert!(
            msg.contains("no resumable history"),
            "msg is the clean no-history wording: {msg}"
        );
        assert!(!msg.contains("attach"), "no attach vocabulary: {msg}");
        // The just-spawned daemon was KILLED (group ladder via the seam).
        assert_eq!(spawner.kills(), vec![60005], "spawned daemon killed");
        // thread_resume was attempted; thread_start was NEVER called (no fabrication).
        assert!(rpc
            .calls()
            .iter()
            .any(|c| c == "thread_resume(T-NO-HISTORY)"));
        assert!(
            !rpc.calls().iter().any(|c| c == "thread_start"),
            "no thread fabrication (m2): {:?}",
            rpc.calls()
        );
        // NO row change: no row keyed by the spawned pid was written.
        assert!(
            !h.sessions_dir.join("60005.json").exists(),
            "no row written on the no-rollout edge"
        );
    }

    // === A non-no-rollout protocol error on resume → Other error, daemon killed,
    //     no row, no fabrication. ===
    #[test]
    fn resume_other_protocol_error_kills_daemon_no_row() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::resume_err(RpcErrorKind::OtherProtocol);
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18977u16);
        let spawner = FakeSpawner::ok(60006);
        let probe = probe_absent();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let params = revive_params("cdx-op", "T-OP");
        let err = resume_codex(&deps, &params).unwrap_err();
        assert!(matches!(err, ResumeError::Other { .. }), "got {err:?}");
        assert_eq!(spawner.kills(), vec![60006]);
        assert!(!h.sessions_dir.join("60006.json").exists(), "no row");
    }

    // === Transport failure on resume → Other, daemon killed, no row. ===
    #[test]
    fn resume_transport_error_kills_daemon_no_row() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::resume_err(RpcErrorKind::Transport);
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18978u16);
        let spawner = FakeSpawner::ok(60007);
        let probe = probe_absent();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let params = revive_params("cdx-tx", "T-TX");
        let err = resume_codex(&deps, &params).unwrap_err();
        assert!(matches!(err, ResumeError::Other { .. }), "got {err:?}");
        assert_eq!(spawner.kills(), vec![60007]);
    }

    // === No thread id → NoThreadId before any spawn. ===
    #[test]
    fn resume_empty_thread_id_errors_before_spawn() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy();
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18979u16);
        let spawner = FakeSpawner::ok(60008);
        let probe = probe_absent();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let params = revive_params("cdx", ""); // empty thread id
        let err = resume_codex(&deps, &params).unwrap_err();
        assert!(matches!(err, ResumeError::NoThreadId), "got {err:?}");
        assert_eq!(spawner.spawn_count(), 0, "no spawn without a thread id");
    }

    // === Version-breaking on revive → blocked before any spawn (the create gate
    //     reused). ===
    #[test]
    fn resume_version_breaking_blocks_revive() {
        let h = harness();
        let exec =
            ScriptedExec::new().on("codex", &["--version"], Some(0), "codex-cli 0.140.0\n", "");
        let rpc = FixtureRpc::happy();
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18980u16);
        let spawner = FakeSpawner::ok(60009);
        let probe = probe_absent();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let params = revive_params("cdx", "T-VB");
        // The 2026-08-17 ruling: a breaking verdict WARNS and proceeds, so the
        // revive reaches its spawn instead of stopping at the gate.
        resume_codex(&deps, &params).expect("breaking drift must proceed");
        assert_eq!(spawner.spawn_count(), 1, "the version gate no longer blocks");
    }

    // === The revive daemon argv = launch_plan argv + --listen (the create shape). ===
    #[test]
    fn revive_daemon_argv_carries_listen() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy();
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18981u16);
        let spawner = FakeSpawner::ok(60010);
        let probe = probe_absent();
        let deps = ReviveDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            ids_path: h.ids_path.clone(),
        };
        let params = revive_params("cdx", "T-ARGV");
        resume_codex(&deps, &params).expect("revive");
        let argv = spawner.last_argv();
        assert_eq!(
            argv,
            vec![
                "codex".to_string(),
                "app-server".to_string(),
                "--listen".to_string(),
                "ws://127.0.0.1:18981".to_string(),
            ]
        );
    }
}
