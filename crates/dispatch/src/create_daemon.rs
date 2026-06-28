//! `qd new --provider <daemon-hosted>` create pipeline (codex-p2-spec §7.2) — the
//! daemon-hosted sibling of [`crate::create::run_new`].
//!
//! THE TOPOLOGY (orc ruling 02:18 06-07): ONE qd-owned `codex app-server`
//! process PER codex session, listening on `ws://127.0.0.1:<port>`. qd is a
//! short-lived CLI; the daemon + the rollout file are the durable truth, so qd
//! writes the registry row ITSELF (claude precedent inverted — claude writes its
//! own row, codex's daemon knows nothing of qd), keyed by the daemon pid,
//! carrying `provider:"codex"`, the REAL thread id as sessionId (m2), and the new
//! `endpoint` field.
//!
//! THE SEQUENCE (codex-p2-spec §7.2; each step names its failure mode, every
//! failure kills the spawned daemon + writes NO row + returns a typed error):
//!   1. version sniff (§3.4) — Breaking verdict → loud named error (found vs pin
//!      + the re-pin ceremony) unless `QD_CODEX_UNPINNED=1`; PatchDrift → warn.
//!   2. port allocation (§3.2) — bind `127.0.0.1:0`, take the OS port, drop the
//!      listener; RE-ROLL any port in 8900-9000 (qd's relay probe range — fleet
//!      lesson); ×N retry ladder if the daemon later fails to bind.
//!   3. spawn DETACHED (§3.2, P-2-proven) — argv = provider launch_plan argv +
//!      `["--listen", "ws://127.0.0.1:<port>"]`, `process_group(0)`, stdout/stderr
//!      → `<qd_home>/.quorum/dispatch/log/codex-<name>.log`, cwd = session cwd.
//!   4. ws connect (bounded retry — the daemon needs a moment to listen) →
//!      initialize handshake (+ initialized) → thread/start(cwd, FULL-BYPASS
//!      policy) → the thread id.
//!   5. write the registry row via [`crate::registry::write_entry`] (its FIRST
//!      production caller).
//!   6. optional first prompt: turn/start it (NON-fatal — the session exists).
//!   7. FAILURE CLEANUP at every step.
//!
//! SEAMS (offline-testable by construction): spawning is behind [`DaemonSpawner`]
//! (real + test fake); the rpc arrives via an injected CONNECTOR closure (tests
//! pass a fixture rpc, never a real socket); clock/env/exec are the house seams.
//! The pure/offline core is everything except the real spawn + real connect.

use std::path::PathBuf;

use crate::effects::{Clock, Env};
use crate::exec::Exec;
use crate::provider::codex::version::{self, SniffOutcome, Version, VersionVerdict};
use crate::provider::codex::{AppServerRpc, ClientInfo, RpcError};
use crate::provider::{LaunchRequest, Provider, ProviderFx};
use crate::registry::{self, RegistryEntry};

// ===========================================================================
// The R-a FULL-BYPASS posture (codex-p2-spec §3.3).
// ===========================================================================

/// qd-launched codex threads run codex's full-bypass posture — the claude
/// `--dangerously-skip-permissions` parity defaults (codex-p2-spec §3.3).
///
/// ⚠ FLIPPING THESE IS A ONE-LINE PETE-READBACK ITEM. They give an qd-launched
/// codex thread approval-policy `never` + sandbox `danger-full-access` (the same
/// "no prompts, full machine access" posture claude sessions run under today).
/// They are passed to `AppServerRpc::thread_start(cwd, approval_policy, sandbox)`
/// — codex-p2-spec §3.3 keeps them at THIS caller, not in the W3 launch_plan.
const APPROVAL_POLICY: &str = "never";
const SANDBOX: &str = "danger-full-access";

/// The ws client identity sent on `initialize` (matches the W3 boot waiter +
/// the spike probe's `clientInfo`).
const CLIENT_NAME: &str = "qd-manager";

/// The relay-probe port range qd scans (codex-p2-spec §3.2 + fleet lesson). A
/// daemon port landing here is RE-ROLLED so the relay scan never collides with a
/// codex daemon.
const RELAY_RANGE: std::ops::RangeInclusive<u16> = 8900..=9000;

/// How many full (alloc → spawn → connect) attempts before giving up — the ×3
/// retry ladder for the TOCTOU window where the daemon later fails to bind the
/// port we picked (codex-p2-spec §3.2).
const SPAWN_RETRIES: u32 = 3;

/// Default bound on the post-spawn ws connect (the daemon needs a moment to
/// bind+listen). The P-2 probe observed the listener up well within this; the
/// connector retries until this elapses.
const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

// ===========================================================================
// Seams.
// ===========================================================================

/// A spawned daemon handle — just its pid (the durable identity is the registry
/// row keyed by this pid; the daemon process is owned by the OS after detach).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnedDaemon {
    pub pid: i64,
}

// ===========================================================================
// W9 FIX M-1/Mo-2: cmdline-identity guard before treating a STORED pid as OUR
// codex daemon (the group-kill in kill_codex + the resume-alive decision in
// resume_codex). Under exact-pid-reuse the stored daemon pid can be re-assigned
// by the OS to an UNRELATED process that happens to be a group leader; a blind
// `-pgid` SIGKILL would then reap a foreign group, and a blind alive-check would
// falsely report "running". The fix: cheaply + connectionlessly read the live
// pid's command line and require it to look like our daemon BEFORE signaling /
// before believing it alive. The probe is a `pid -> Option<cmdline>` closure so
// units feed a matching / non-matching / absent cmdline with no real process.
// ===========================================================================

/// A connectionless "pid → its command line" probe. Production wraps the existing
/// `ps` seam ([`real_cmdline_probe`]); tests inject a deterministic closure.
pub type CmdlineProbe<'a> = dyn Fn(i64) -> Option<String> + 'a;

/// PURE: does this command line look like OUR codex app-server daemon?
///
/// We require the two stable, codex-specific tokens our launch argv always
/// carries — `"codex"` AND `"app-server"` — and, when an `endpoint` is supplied,
/// ALSO the `--listen <endpoint>` we spawned it with (the per-instance
/// discriminator: two codex daemons differ only by their listen port). An
/// `endpoint` we cannot find on the cmdline ⇒ NOT a match (a reused pid that is
/// some OTHER codex daemon must not be mistaken for THIS session's). A `None`
/// cmdline (pid not visible / read failed) ⇒ NOT a match (treat as gone).
///
/// `endpoint` is the recorded `ws://127.0.0.1:<port>` — we match on the
/// `--listen <endpoint>` argv pair the spawn appends, falling back to just the
/// `ws://…:<port>` substring (the launcher may reformat its own argv).
pub fn cmdline_is_our_daemon(cmdline: Option<&str>, endpoint: Option<&str>) -> bool {
    let Some(cmd) = cmdline else {
        return false;
    };
    if !cmd.contains("codex") || !cmd.contains("app-server") {
        return false;
    }
    match endpoint {
        // No endpoint to discriminate on (None or empty) → the codex+app-server
        // tokens are the best we can do (the resume-alive caller always has the
        // recorded endpoint; this covers a row that somehow lost it).
        None | Some("") => true,
        // The recorded endpoint must appear on the live cmdline (we spawned it
        // with `--listen <endpoint>`). Match the endpoint substring so a launcher
        // that reformats `--listen=<ep>` vs `--listen <ep>` still matches.
        Some(ep) => cmd.contains(ep),
    }
}

/// The production cmdline probe: read one pid's command line via the existing
/// process-table `ps` seam ([`crate::effects::RealProcessTable`] over
/// [`crate::exec::RealExec`]) — the SAME single `ps` spawn point the rest of the
/// engine uses (no raw shell-out). A non-visible pid / a `ps` failure ⇒ `None`.
pub fn real_cmdline_probe(pid: i64) -> Option<String> {
    use crate::effects::{ProcessTable, RealProcessTable};
    use crate::exec::RealExec;
    if pid <= 0 || pid > i32::MAX as i64 {
        return None;
    }
    RealProcessTable::new(RealExec).cmdline(pid as i32)
}

/// The detached-spawn seam (codex-p2-spec §3.2). The real impl runs
/// `std::process::Command` + `process_group(0)` with stdout/stderr → a log file
/// (the P-2-proven detach); the test fake records the request + hands back a
/// canned pid without spawning anything.
pub trait DaemonSpawner {
    /// Spawn `argv` (already including `--listen ws://…`) DETACHED, in `cwd`,
    /// with `env` overrides layered on, stdout+stderr → `log_path` (parent dirs
    /// created). Returns the spawned pid or an io error.
    fn spawn_detached(
        &self,
        argv: &[String],
        env: &[(String, String)],
        cwd: &std::path::Path,
        log_path: &std::path::Path,
    ) -> std::io::Result<SpawnedDaemon>;

    /// Kill a spawned daemon (SIGTERM → grace → SIGKILL by the recorded PGID —
    /// the codex launcher exec-spawns a native child a launcher-only SIGKILL
    /// orphans, so the real impl signals the whole process group `-pgid`;
    /// instance-addressed by the pgid OUR spawn created, never a name/pattern,
    /// L10). Used by the failure-cleanup path. The fake records the pid.
    fn kill(&self, pid: i64);
}

/// A connector that, given a `ws://…` url, returns a connected [`AppServerRpc`]
/// (or a transport error). Boxed so production hands a real `WsAppServer` and
/// tests hand a fixture rpc — the create sequence never opens a socket itself.
pub type RpcConnector<'a> = dyn Fn(&str) -> Result<Box<dyn AppServerRpc + 'a>, RpcError> + 'a;

/// A port allocator: bind `127.0.0.1:0`, read the OS port, drop the listener,
/// RE-ROLLING any port in [`RELAY_RANGE`]. Boxed so a test can inject a
/// deterministic allocator (e.g. forcing a relay-range port first to prove the
/// re-roll). The real impl is [`real_alloc_port`].
pub type PortAllocator<'a> = dyn Fn() -> std::io::Result<u16> + 'a;

/// Injected effects + seams for one [`run_new_daemon`] call. Mirrors the
/// [`crate::create::NewDeps`] house style — plain references, each documented.
pub struct DaemonDeps<'a> {
    /// The resolved provider (codex). Used for `launch_plan` (the daemon argv,
    /// minus `--listen`) only — the create sequence appends `--listen` + drives
    /// the rpc directly (the trait's boot_waiter/inject are W3's; W4 owns the
    /// create choreography, blast-radius rule §7.1).
    pub provider: &'a dyn Provider,
    /// Env seam (L9a): version-override read (`QD_CODEX_UNPINNED`), CODEX_HOME
    /// passthrough resolution via the provider's launch_plan, and `fx.env`.
    pub env: &'a dyn Env,
    /// Exec seam — the one-shot `codex --version` sniff (§3.4) routes through it.
    pub exec: &'a dyn Exec,
    /// Clock — startedAt/updatedAt stamps on the written row.
    pub clock: &'a dyn Clock,
    /// The sessions dir the row is written into (`<home>/.claude/sessions`).
    pub sessions_dir: PathBuf,
    /// W9 FIX M-2: the claims dir for the atomic O_EXCL name-claim (`<.claude>/
    /// claims`, alongside `sessions/` — the claude `NewDeps::claims_dir()` layout).
    /// The claim is taken BEFORE the spawn loop, held through the row write, and
    /// released on EVERY exit path (RAII), mirroring create.rs exactly.
    pub claims_dir: PathBuf,
    /// The log dir root for the daemon's stdout/stderr (`<qd_home>/.quorum/dispatch/log`);
    /// the file is `codex-<name>.log` (codex-p2-spec §3.2).
    pub log_dir: PathBuf,
    /// The detached-spawn seam (real spawner in production; a fake in units).
    pub spawner: &'a dyn DaemonSpawner,
    /// The rpc connector (real `WsAppServer::connect` in production; a fixture in
    /// units). Reconnected per spawn attempt.
    pub connect: &'a RpcConnector<'a>,
    /// The port allocator (real OS bind in production; a deterministic fake in
    /// units). Re-rolled per spawn attempt.
    pub alloc_port: &'a PortAllocator<'a>,
    /// P0 wave-2 (spec-w2-env D1 site 3): the idstore path (`<state>/ids.jsonl`).
    /// The stable id is minted UNBOUND before the spawn loop, injected as
    /// `QD_SESSION_ID` in the daemon's process env, and BOUND to the thread id
    /// after `thread/start` (the codex analog of the claude mint-at-start /
    /// bind-at-boot-confirm flow — the thread uuid does not exist at spawn time).
    pub ids_path: PathBuf,
}

/// Parameters for one daemon create (the launch-relevant `NewParams` subset +
/// the optional first prompt).
pub struct DaemonParams {
    /// Session name (also the registry row name + the log filename component).
    pub name: String,
    /// Working dir for the session (the daemon's cwd AND thread/start cwd).
    pub cwd: PathBuf,
    /// `--agent`, carried into the launch request (provider-neutral; codex's
    /// launch_plan ignores it today, but the request stays faithful).
    pub agent: Option<String>,
    /// Pass-through args (after `--`).
    pub passthrough: Vec<String>,
    /// `-p/--prompt`, if given → the first `turn/start` (NON-fatal on failure).
    pub prompt: Option<String>,
}

/// Success outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonOutcome {
    pub name: String,
    pub pid: i64,
    /// The REAL thread id (m2 — the row's sessionId).
    pub thread_id: String,
    pub endpoint: String,
    /// The first turn id, if a prompt was delivered (None when no prompt, or when
    /// the non-fatal prompt delivery failed).
    pub first_turn_id: Option<String>,
}

/// Why a daemon create failed. EVERY variant means: the spawned daemon (if any)
/// was killed, NO registry row was written, and the verb maps this to a loud
/// message + a nonzero exit.
#[derive(Debug)]
pub enum DaemonError {
    /// W9 FIX M-2: the atomic O_EXCL name-claim lost the race / the name is
    /// mid-create (the claude `NameClaimed` precedent, create.rs). Carries the name
    /// plus the holder's claim payload (best-effort). NOTHING was spawned, no row
    /// written — this fires BEFORE the spawn loop.
    NameClaimed { name: String, holder: String },
    /// §3.4: the installed codex is a (major,minor) drift from the pin and
    /// `QD_CODEX_UNPINNED=1` was NOT set. Carries found vs pin for the message.
    VersionBreaking { found: Version, pin: Version },
    /// §3.4: `codex --version` could not be sniffed (spawn failed / nonzero /
    /// unparseable). Carries the detail.
    VersionUnknown { detail: String },
    /// §3.2: the OS would not hand us a usable port after the re-roll attempts.
    PortAllocFailed { detail: String },
    /// §3.2: the daemon could not be spawned / connected after the retry ladder.
    /// Carries the last underlying error.
    SpawnFailed { detail: String },
    /// §7.2: the ws handshake (connect/initialize) failed after the daemon was
    /// up. Carries the detail. (The daemon was killed before this returned.)
    HandshakeFailed { detail: String },
    /// §7.2: `thread/start` failed. Carries the detail. (Daemon killed.)
    ThreadStartFailed { detail: String },
    /// §7.2: the registry row could not be written (e.g. the sessions dir is
    /// unwritable). Carries the io detail. (Daemon killed — we do NOT leave an
    /// orphan daemon with no row.)
    RowWriteFailed { detail: String },
    /// P0 wave-2: the stable id could not be minted (idstore IO). Fail-closed
    /// BEFORE the spawn loop — never boot a daemon whose env would silently
    /// miss its identity. Nothing spawned, no row written.
    IdMintFailed { detail: String },
}

impl DaemonError {
    /// Process exit code — all daemon-create failures are exit 1 (the
    /// create.rs precedent: distinct variants for testability/stderr, one code).
    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // W9 FIX M-2: the claude `NameClaimed` wording verbatim (the same UX as
            // the MuxPane create path — a name being created by another process).
            DaemonError::NameClaimed { name, holder } => {
                // S3: print the ENCODED on-disk basename (case-folded +
                // percent-escaped), not the raw name — so `rm` works on a
                // case-sensitive fs.
                let claim_file =
                    registry::claim_file_name(name).unwrap_or_else(|| format!("{name}.claim"));
                write!(
                    f,
                    "qd start: name '{name}' is being created by another process \
                     (claim held: {holder}). No session was created. If no create \
                     is in flight, the claim is wedged — delete the '{claim_file}' \
                     file under ~/.claude/claims/ to recover (the claim file only \
                     closes the create window; a booted session's durable record \
                     is its registry row)."
                )
            }
            // §3.4 loud named error: names found vs pin + the re-pin ceremony +
            // the override knob. Mirrors the spec's example wording.
            DaemonError::VersionBreaking { found, pin } => write!(
                f,
                "qd start: codex {}.{}.{} detected, pinned {}.{} — run the schema \
                 fixture-diff and re-pin (scripts/codex-schema-diff.sh + bump \
                 VERSION.pin). To proceed at your own risk: QD_CODEX_UNPINNED=1. \
                 No session was created.",
                found.major, found.minor, found.patch, pin.major, pin.minor
            ),
            DaemonError::VersionUnknown { detail } => write!(
                f,
                "qd start: could not determine the codex version ({detail}). \
                 Is `codex` on PATH? No session was created."
            ),
            DaemonError::PortAllocFailed { detail } => write!(
                f,
                "qd start: could not allocate a local port for the codex daemon \
                 ({detail}). No session was created."
            ),
            DaemonError::SpawnFailed { detail } => write!(
                f,
                "qd start: the codex app-server daemon failed to start ({detail}). \
                 No session was created."
            ),
            DaemonError::HandshakeFailed { detail } => write!(
                f,
                "qd start: the codex daemon started but the initialize handshake \
                 failed ({detail}). No session was created."
            ),
            DaemonError::ThreadStartFailed { detail } => write!(
                f,
                "qd start: the codex daemon started but thread/start failed \
                 ({detail}). No session was created."
            ),
            DaemonError::RowWriteFailed { detail } => write!(
                f,
                "qd start: the codex session started but its registry row could not \
                 be written ({detail}); the daemon was stopped. No session was \
                 created."
            ),
            DaemonError::IdMintFailed { detail } => write!(
                f,
                "qd start: could not mint a stable session id ({detail}). \
                 No session was created."
            ),
        }
    }
}

impl std::error::Error for DaemonError {}

// ===========================================================================
// The create sequence.
// ===========================================================================

/// Run the daemon-hosted create pipeline (codex-p2-spec §7.2). See the module
/// docs for the step order + the cleanup discipline.
pub fn run_new_daemon<'a>(
    deps: &'a DaemonDeps<'a>,
    params: &DaemonParams,
) -> Result<DaemonOutcome, DaemonError> {
    // --- Step 1: version sniff (§3.4) ----------------------------------------
    // Breaking → loud named error UNLESS QD_CODEX_UNPINNED=1 (read off the env
    // SEAM, never raw std::env). PatchDrift → warn-and-go. The verdict itself is
    // a pure function of (found, pin); the override is mapped HERE (version.rs is
    // deliberately override-free).
    check_version(deps)?;

    // --- Step 2: HARDENING — atomic name-claim (W9 FIX M-2) ------------------
    // Mirror create.rs: claim the name with an O_EXCL open BEFORE spawning ANY
    // daemon (so a duplicate-name create fails LOUD before a daemon/row exists,
    // closing the create race the claude path already closes). The claim is held
    // by an RAII guard for the WHOLE window — released on EVERY exit path after
    // this point (success + every failure), matching the claude claim LIFETIME:
    // the durable record is the registry row this fn writes; the claim only closes
    // the create window, so it drops once the sequence returns. If the name is
    // already claimed → `NameClaimed` BEFORE the spawn loop (no daemon, no row).
    let payload = claim_payload(deps, &params.name);
    // P0 redfix F2: the real pid-liveness predicate — a stale claim whose holder
    // died (SIGKILL mid-boot; ClaimGuard never ran) is reaped instead of
    // bricking the name.
    let is_alive = |pid: i64| crate::effects::is_pid_alive(pid as i32);
    // B4 item 10: the exec-proof start-time probe — a live pid whose occupant
    // started after the claimed start is a recycled pid, reaped as stale.
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
            return Err(DaemonError::NameClaimed {
                name: params.name.clone(),
                holder,
            });
        }
        Err(registry::ClaimError::Io(e)) => {
            // An unexpected claim I/O error (e.g. an unsanitizable name) → fail
            // CLOSED, nothing spawned (the claude precedent, create.rs).
            return Err(DaemonError::NameClaimed {
                name: params.name.clone(),
                holder: format!("<claim io error: {e}>"),
            });
        }
    };

    // From here EVERY return drops `_claim` (RAII release). The retry ladder + the
    // finish_create row-write happen UNDER the held claim.
    //
    // --- P0 wave-2 (spec-w2-env D1 site 3): pre-mint the stable id ------------
    // The thread uuid does not exist until thread/start; the daemon env needs
    // the id at spawn time → mint UNBOUND now (ONE id across all retry
    // attempts — the same daemon identity whichever spawn wins), bind to the
    // thread id in finish_create. Fail-closed on a mint error (nothing spawned).
    let qd_session_id =
        crate::idstore::mint_unbound(&deps.ids_path, Some(&params.name), deps.clock)
            .map_err(|detail| DaemonError::IdMintFailed { detail })?;

    // --- The retry ladder (§3.2): alloc → spawn → connect, ×SPAWN_RETRIES. A
    // daemon that fails to BIND the port we picked (TOCTOU) surfaces as a connect
    // failure; we re-roll the port and respawn. A connected rpc breaks the loop.
    let mut last_err: Option<DaemonError> = None;
    for _ in 0..SPAWN_RETRIES {
        match try_spawn_and_connect(deps, params, &qd_session_id) {
            Ok((spawned, endpoint, rpc)) => {
                // Steps 4b-6 happen with the connected rpc; any failure there
                // kills the daemon + returns (no further retry — the daemon IS
                // up, the failure is protocol/IO, not a bind race).
                return finish_create(
                    deps,
                    params,
                    spawned,
                    &endpoint,
                    rpc.as_ref(),
                    &qd_session_id,
                );
            }
            Err((err, spawned)) => {
                // Cleanup whatever this attempt spawned before retrying/giving up.
                if let Some(s) = spawned {
                    deps.spawner.kill(s.pid);
                }
                // A version/port-alloc error is NOT a bind race — do not retry.
                if matches!(
                    err,
                    DaemonError::PortAllocFailed { .. } | DaemonError::VersionBreaking { .. }
                ) {
                    return Err(err);
                }
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| DaemonError::SpawnFailed {
        detail: "exhausted spawn retries".to_string(),
    }))
}

/// Step 1: version sniff + override mapping (§3.4).
fn check_version(deps: &DaemonDeps) -> Result<(), DaemonError> {
    match version::sniff(deps.exec) {
        SniffOutcome::Verdict(VersionVerdict::Exact) => Ok(()),
        SniffOutcome::Verdict(VersionVerdict::PatchDrift { found }) => {
            // Patch drift: warn-and-go (the protocol surface is pin-compatible at
            // the patch level; the schema harness owns true detection).
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
            // Breaking UNLESS the override env is set (read off the SEAM).
            if unpinned_override(deps.env) {
                eprintln!(
                    "WARNING: codex {}.{}.{} is a BREAKING drift from the pin \
                     {}.{} but QD_CODEX_UNPINNED=1 is set — proceeding at risk.",
                    found.major, found.minor, found.patch, pin.major, pin.minor
                );
                Ok(())
            } else {
                Err(DaemonError::VersionBreaking { found, pin })
            }
        }
        SniffOutcome::Unparseable { stdout } => Err(DaemonError::VersionUnknown {
            detail: format!("`codex --version` output did not parse: {}", stdout.trim()),
        }),
        SniffOutcome::ExecFailed { detail } => Err(DaemonError::VersionUnknown { detail }),
    }
}

/// `QD_CODEX_UNPINNED=1` read off the env SEAM (L9a — never raw std::env). Any
/// non-"1" value (including unset) is NOT the override.
fn unpinned_override(env: &dyn Env) -> bool {
    env.var("QD_CODEX_UNPINNED").as_deref() == Some("1")
}

/// One attempt: alloc a port → build the daemon argv (provider launch_plan +
/// `--listen`) → spawn detached → connect the rpc with a bounded retry. On
/// failure returns the typed error PLUS whatever was spawned (so the caller can
/// kill it). On success returns (spawned, endpoint, connected rpc).
#[allow(clippy::type_complexity)]
fn try_spawn_and_connect<'a>(
    deps: &'a DaemonDeps<'a>,
    params: &DaemonParams,
    qd_session_id: &str,
) -> Result<(SpawnedDaemon, String, Box<dyn AppServerRpc + 'a>), (DaemonError, Option<SpawnedDaemon>)>
{
    // Step 2: port allocation (re-roll handled inside the allocator).
    let port = (deps.alloc_port)().map_err(|e| {
        (
            DaemonError::PortAllocFailed {
                detail: e.to_string(),
            },
            None,
        )
    })?;
    let endpoint = format!("ws://127.0.0.1:{port}");

    // Step 3: build the daemon argv = provider launch_plan argv + --listen. The
    // launch_plan resolves the bin + CODEX_HOME passthrough off `fx.env` (W3); we
    // append the transport flag (port allocation is a create-path concern).
    let fx = ProviderFx {
        env: deps.env,
        // launch_plan reads only env (codex) — paths/socket_dir/mux/etc. unused;
        // a placeholder QdPaths keeps the borrow valid without a real home.
        paths: &placeholder_paths(),
        socket_dir: PathBuf::new(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,    };
    let req = LaunchRequest {
        name: params.name.clone(),
        cwd: Some(params.cwd.to_string_lossy().into_owned()),
        resume: None,
        fork: false,
        agent: params.agent.clone(),
        // codex daemon argv has no claude --model surface (warranty #2 is claude-lane).
        model: None,
        passthrough: params.passthrough.clone(),
    };
    let plan = deps.provider.launch_plan(&fx, &req);
    let mut argv = plan.argv;
    argv.push("--listen".to_string());
    argv.push(endpoint.clone());

    // P0 wave-2 (spec-w2-env D1 site 3): the daemon process env carries the
    // pre-minted stable id — an explicit set layered over the launch plan's env,
    // so it overrides anything inherited through the commissioner's subtree.
    let mut spawn_env = plan.env.clone();
    spawn_env.push(("QD_SESSION_ID".to_string(), qd_session_id.to_string()));

    let log_path = deps.log_dir.join(format!("codex-{}.log", params.name));
    let spawned = match deps
        .spawner
        .spawn_detached(&argv, &spawn_env, &params.cwd, &log_path)
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

    // Step 4a: ws connect with a bounded retry (the daemon needs a moment to
    // listen). The connector is injected; in production it is WsAppServer::connect.
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

/// Steps 4b-6 with a connected rpc: initialize → thread/start → write the row →
/// optional first prompt. Any failure kills the daemon + writes NO row.
fn finish_create(
    deps: &DaemonDeps,
    params: &DaemonParams,
    spawned: SpawnedDaemon,
    endpoint: &str,
    rpc: &dyn AppServerRpc,
    qd_session_id: &str,
) -> Result<DaemonOutcome, DaemonError> {
    // Step 4b: initialize handshake (readiness) + the bare `initialized`
    // notification the spike follows it with.
    let client = ClientInfo {
        name: CLIENT_NAME.to_string(),
        title: None,
        version: "0".to_string(),
    };
    if let Err(e) = rpc.initialize(&client) {
        deps.spawner.kill(spawned.pid);
        return Err(DaemonError::HandshakeFailed {
            detail: format!("initialize: {e}"),
        });
    }
    // Best-effort `initialized` (a failure here does not un-ready us — W3 parity).
    let _ = rpc.initialized();

    // Step 4c: thread/start with the FULL-BYPASS posture (the R-a constants).
    let cwd_str = params.cwd.to_string_lossy().into_owned();
    let thread_id = match rpc.thread_start(&cwd_str, APPROVAL_POLICY, SANDBOX) {
        Ok(id) => id,
        Err(e) => {
            deps.spawner.kill(spawned.pid);
            return Err(DaemonError::ThreadStartFailed {
                detail: format!("thread/start: {e}"),
            });
        }
    };

    // P0 wave-2: bind the pre-minted unbound id to the thread id the moment it
    // exists — from here the id in the daemon's env equals the id the
    // registry/ls surface. A bind failure warns (the daemon is up; tearing it
    // down would not help) — never silent.
    if let Err(e) = crate::idstore::bind(&deps.ids_path, qd_session_id, &thread_id, deps.clock) {
        eprintln!(
            "WARNING: could not bind stable id {qd_session_id} to codex thread \
             {thread_id}: {e} — `qd ls` may surface a different id than the \
             daemon's QD_SESSION_ID."
        );
    }

    // Step 5: write the registry row (write_entry's FIRST production caller).
    let now = deps.clock.now_ms();
    let entry = RegistryEntry {
        pid: Some(spawned.pid),
        session_id: Some(thread_id.clone()),
        cwd: Some(cwd_str.clone()),
        started_at: Some(now),
        updated_at: Some(now),
        status: Some("idle".to_string()),
        name: Some(params.name.clone()),
        version: None,
        kind: None,
        entrypoint: None,
        backend: None,
        // spawnedBy: claude rows get this from the bin-side ppid walk
        // (telemetry::find_caller_session); the daemon-create lib has no such
        // qd-side data, and the spec says None is fine when absent. The verb's
        // telemetry create-stamp still records lineage out-of-band.
        spawned_by: None,
        provider: Some("codex".to_string()),
        endpoint: Some(endpoint.to_string()),
        // scoped-ACP-CC: no degradation latch on a freshly-created healthy row
        // (the tier is DERIVED; only degradation persists transport).
        transport: None,
    };
    if let Err(e) = registry::write_entry(&deps.sessions_dir, &entry) {
        deps.spawner.kill(spawned.pid);
        return Err(DaemonError::RowWriteFailed {
            detail: e.to_string(),
        });
    }

    // Step 6: optional first prompt (the priming analog / dogfood path). NON-fatal
    // — the session EXISTS; a failed first turn just warns. We never tear down a
    // created session because its first prompt did not land.
    let first_turn_id = match &params.prompt {
        Some(p) if !p.is_empty() => match rpc.turn_start(&thread_id, p) {
            Ok(turn) => Some(turn),
            Err(e) => {
                eprintln!(
                    "WARNING: codex session \"{}\" created, but the first prompt \
                     did not start a turn ({e}). The session exists — send again \
                     with: qd send {} <text>",
                    params.name, params.name
                );
                None
            }
        },
        _ => None,
    };

    // Best-effort close of our short-lived client (the daemon stays up).
    let _ = rpc.close();

    Ok(DaemonOutcome {
        name: params.name.clone(),
        pid: spawned.pid,
        thread_id,
        endpoint: endpoint.to_string(),
        first_turn_id,
    })
}

/// Connect with a bounded retry: the just-spawned daemon needs a moment to begin
/// listening, so a `ConnectionRefused`-class transport error is retried until the
/// `budget` elapses. Returns the connected rpc or the last transport error.
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

/// A throwaway [`crate::paths::QdPaths`] for the launch_plan fx — codex's
/// launch_plan reads ONLY env, never paths, so an empty-home layout is a valid
/// value (the minimal-fx negative control extends to the create sequence).
fn placeholder_paths() -> crate::paths::QdPaths {
    crate::paths::QdPaths::from_home(std::path::Path::new(""))
}

/// W9 FIX M-2 + B4 item 10/S4: the O_EXCL claim payload, built by the shared
/// [`registry::claim_payload`] writer (beside the `claim_name` parser) so the
/// daemon + claude create paths emit a byte-identical 2-shape protocol. Stamps
/// the claimant's own `start` (exec-proof identity); probe failure omits it.
fn claim_payload(deps: &DaemonDeps, name: &str) -> String {
    let pid = std::process::id();
    let start = crate::effects::proc_start_ms(pid as i32);
    registry::claim_payload(pid, start, deps.clock.now_ms(), name)
}

/// W9 FIX M-2: RAII guard that releases a held [`registry::NameClaim`] on drop —
/// on the success path AND every failure path after acquisition (the claude
/// `ClaimGuard`, create.rs). Release is best-effort: a failed removal is swallowed
/// (the create window is already closed by the written registry row; a leaked
/// claim file is at worst a stale UX hint, never a correctness problem).
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

// ===========================================================================
// Real seams (production).
// ===========================================================================

/// Real port allocator (codex-p2-spec §3.2): bind `127.0.0.1:0`, read the OS
/// port, drop the listener, RE-ROLLING any port in [`RELAY_RANGE`]. A handful of
/// attempts is plenty — the OS rarely reuses a just-freed port (the codex_ws.rs
/// test-server precedent).
pub fn real_alloc_port() -> std::io::Result<u16> {
    let mut held = Vec::new();
    let port = loop {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let p = l.local_addr()?.port();
        if !RELAY_RANGE.contains(&p) {
            // Drop `l` here (end of scope) so the port is free for the daemon.
            break p;
        }
        held.push(l); // hold the bad one so the next bind differs
        if held.len() >= 64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "could not get a port outside 8900-9000 in 64 tries",
            ));
        }
    };
    drop(held);
    Ok(port)
}

/// Real detached spawner (codex-p2-spec §3.2, P-2-proven): `std::process::Command`
/// + `process_group(0)`, stdin null, stdout/stderr → `log_path`, cwd set.
pub struct RealDaemonSpawner;

impl DaemonSpawner for RealDaemonSpawner {
    fn spawn_detached(
        &self,
        argv: &[String],
        env: &[(String, String)],
        cwd: &std::path::Path,
        log_path: &std::path::Path,
    ) -> std::io::Result<SpawnedDaemon> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        if argv.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty daemon argv",
            ));
        }
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let log_err = log.try_clone()?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .current_dir(cwd);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // The detach: own process group (setsid class) → the daemon survives qd's
        // exit + the terminal closing (P-2 GREEN).
        cmd.process_group(0);
        let child = cmd.spawn()?;
        Ok(SpawnedDaemon {
            pid: child.id() as i64,
        })
    }

    fn kill(&self, pid: i64) {
        // GROUP-scoped SIGTERM → grace → SIGKILL, addressed by the RECORDED pgid.
        //
        // W4 FINDING (revised at lead review): the homebrew/npm `codex` command is
        // a LAUNCHER that exec-spawns the native app-server as a child. AFTER a ws
        // session has touched it, the launcher IGNORES SIGTERM (>3s, verified
        // live) and only dies on SIGKILL — and SIGKILL to the LAUNCHER does NOT
        // propagate: the live-lane belt caught the NATIVE CHILD surviving as an
        // orphan after a pid-scoped kill (the implementer's run got lucky on
        // timing; the lead's re-run did not). We spawned with `process_group(0)`,
        // so the launcher pid IS the pgid of the whole daemon subtree — signaling
        // `-pgid` reaps launcher + native child together. This is still INSTANCE-
        // addressed (the pgid exists because OUR spawn created it — L10 bans
        // NAME/pattern addressing, not recorded-group addressing).
        // W7 CARRY: the kill verb for codex rows must use this same group ladder.
        if pid <= 0 || pid > i32::MAX as i64 {
            return;
        }
        let pgid = pid as i32;
        // SIGTERM the group; brief grace; SIGKILL the group. ESRCH = already gone.
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while std::time::Instant::now() < deadline {
            if !crate::effects::is_pid_alive(pgid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        // The launcher was spawned by THIS process (Command::spawn), so after a
        // SIGKILL it is a ZOMBIE until reaped. Reap it eagerly so the failure path
        // leaves no zombie (the native child re-parents to init, which reaps it).
        reap_zombie(pgid);
    }
}

/// Best-effort reap of a just-killed child pid (WNOHANG, bounded). A no-op if the
/// pid is not our child (`ECHILD`) or is already reaped. Defuses the zombie the
/// in-process `Command::spawn` + SIGKILL leaves (W4 finding).
pub fn reap_zombie(pid: i32) {
    for _ in 0..20 {
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid with WNOHANG is non-blocking; pid is a specific child.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        // r == pid: reaped; r == 0: not yet exited (kill in flight) — retry; r <
        // 0: ECHILD/EINTR — not our child or gone, stop.
        if r == pid || r < 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{FixedClock, MapEnv};
    use crate::exec::ScriptedExec;
    use crate::provider::codex::{
        ClientInfo, InitializeResult, Notification, RpcError, SteerOutcome,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use tempfile::TempDir;

    // --- A fixture AppServerRpc: scripted outcomes per method, recording calls. ---

    struct FixtureRpc {
        // Configured outcomes (None = success default).
        initialize_err: Option<RpcError>,
        thread_start: Result<String, RpcError>,
        turn_start: Result<String, RpcError>,
        // Recorded calls (verb names in order) for assertions.
        calls: RefCell<Vec<String>>,
    }

    impl FixtureRpc {
        fn happy(thread_id: &str) -> Self {
            FixtureRpc {
                initialize_err: None,
                thread_start: Ok(thread_id.to_string()),
                turn_start: Ok("TURN-1".to_string()),
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
        fn thread_start(&self, _cwd: &str, ap: &str, qd: &str) -> Result<String, RpcError> {
            self.calls
                .borrow_mut()
                .push(format!("thread_start({ap},{qd})"));
            match &self.thread_start {
                Ok(id) => Ok(id.clone()),
                Err(_) => Err(RpcError::Transport("thread/start boom".into())),
            }
        }
        fn thread_resume(&self, _id: &str) -> Result<(), RpcError> {
            unreachable!("create never resumes")
        }
        fn turn_start(&self, _tid: &str, _text: &str) -> Result<String, RpcError> {
            self.calls.borrow_mut().push("turn_start".into());
            match &self.turn_start {
                Ok(id) => Ok(id.clone()),
                Err(_) => Err(RpcError::Transport("turn/start boom".into())),
            }
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
        // When set, spawn_detached fails (the spawn-failure path).
        fail: bool,
    }
    impl FakeSpawner {
        fn ok(pid: i64) -> Self {
            FakeSpawner {
                pid,
                spawns: RefCell::new(Vec::new()),
                envs: RefCell::new(Vec::new()),
                kills: RefCell::new(Vec::new()),
                fail: false,
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
            if self.fail {
                return Err(std::io::Error::other("spawn boom"));
            }
            Ok(SpawnedDaemon { pid: self.pid })
        }
        fn kill(&self, pid: i64) {
            self.kills.borrow_mut().push(pid);
        }
    }

    /// codex --version sniff EXACT (the pinned 0.134.0 binary).
    fn exact_exec() -> ScriptedExec {
        ScriptedExec::new().on("codex", &["--version"], Some(0), "codex-cli 0.134.0\n", "")
    }

    struct Harness {
        _tmp: TempDir,
        sessions_dir: PathBuf,
        claims_dir: PathBuf,
        log_dir: PathBuf,
        ids_path: PathBuf,
        env: MapEnv,
        clock: FixedClock,
    }
    fn harness() -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        Harness {
            sessions_dir: tmp.path().join("sessions"),
            claims_dir: tmp.path().join("claims"),
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

    fn params(name: &str, prompt: Option<&str>) -> DaemonParams {
        DaemonParams {
            name: name.to_string(),
            cwd: PathBuf::from("/work/proj"),
            agent: None,
            passthrough: vec![],
            prompt: prompt.map(str::to_string),
        }
    }

    // A connector that hands back a clone-free reference to the SAME fixture rpc
    // (boxed via a thin newtype forwarding to a borrowed &FixtureRpc). We model it
    // by constructing the rpc inside the closure each call would be awkward (we
    // want to assert on it after), so the closure returns a Box wrapping a shared
    // borrow through a forwarding adapter.
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

    // === Port allocator never returns 8900-9000 (mutation-evidence comment). ===
    //
    // MUTATION EVIDENCE (§13 "port allocator returns 8900-9000"): the real
    // allocator re-rolls any port in the relay range. Removing the
    // `RELAY_RANGE.contains` guard in `real_alloc_port` would let the OS hand back
    // an 8900-9000 port; this invariant (run many times) reds if that guard goes.
    #[test]
    fn real_alloc_port_never_in_relay_range() {
        for _ in 0..40 {
            let p = real_alloc_port().expect("alloc a local port");
            assert!(
                !(8900..=9000).contains(&p),
                "allocator returned a relay-range port {p}"
            );
        }
    }

    // === Version gate (§3.4) ===

    #[test]
    fn version_breaking_blocks_with_named_text() {
        let h = harness();
        let exec =
            ScriptedExec::new().on("codex", &["--version"], Some(0), "codex-cli 0.140.0\n", "");
        let rpc = FixtureRpc::happy("T-1");
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || real_alloc_port();
        let spawner = FakeSpawner::ok(4242);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let err = run_new_daemon(&deps, &params("cdx", None)).unwrap_err();
        match &err {
            DaemonError::VersionBreaking { found, pin } => {
                assert_eq!((found.major, found.minor), (0, 140));
                assert_eq!((pin.major, pin.minor), (0, 134));
            }
            other => panic!("expected VersionBreaking, got {other:?}"),
        }
        // The named text mentions found, pin, and the override knob.
        let msg = err.to_string();
        assert!(msg.contains("0.140.0"), "msg: {msg}");
        assert!(msg.contains("0.134"), "msg: {msg}");
        assert!(msg.contains("QD_CODEX_UNPINNED=1"), "msg: {msg}");
        // NOTHING was spawned (version gate is BEFORE the spawn loop).
        assert_eq!(spawner.spawn_count(), 0, "version gate blocks before spawn");
        assert!(!h.sessions_dir.join("4242.json").exists(), "no row written");
    }

    #[test]
    fn version_breaking_override_env_honored() {
        let h = harness();
        let exec =
            ScriptedExec::new().on("codex", &["--version"], Some(0), "codex-cli 0.140.0\n", "");
        let mut env = MapEnv {
            vars: HashMap::new(),
            uid: 501,
        };
        // The override read off the env SEAM (fake env) — proceeds at risk.
        env.vars
            .insert("QD_CODEX_UNPINNED".to_string(), "1".to_string());
        let rpc = FixtureRpc::happy("T-OVR");
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18960u16);
        let spawner = FakeSpawner::ok(4243);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let out = run_new_daemon(&deps, &params("cdx", None)).expect("override proceeds");
        assert_eq!(out.thread_id, "T-OVR");
        assert_eq!(spawner.spawn_count(), 1, "override proceeded to spawn");
    }

    #[test]
    fn version_patch_drift_proceeds() {
        let h = harness();
        let exec =
            ScriptedExec::new().on("codex", &["--version"], Some(0), "codex-cli 0.134.7\n", "");
        let rpc = FixtureRpc::happy("T-PATCH");
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18961u16);
        let spawner = FakeSpawner::ok(4244);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let out = run_new_daemon(&deps, &params("cdx", None)).expect("patch drift proceeds");
        assert_eq!(out.thread_id, "T-PATCH");
    }

    // === P0 wave-2 (spec-w2-env D1 site 3): QD_SESSION_ID in the daemon env ===

    /// The daemon's spawn env carries a pre-minted stable id, and after
    /// thread/start the id is BOUND to the thread uuid — the id in the daemon's
    /// env equals the id the registry/ls surface (the codex analog of the
    /// claude mint-at-start / bind-at-boot-confirm flow).
    #[test]
    fn daemon_env_carries_stable_id_bound_to_thread() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy("T-IDS");
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18968u16);
        let spawner = FakeSpawner::ok(55777);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        run_new_daemon(&deps, &params("cdx", None)).expect("happy create");
        // The spawn env carries QD_SESSION_ID with a well-formed stable id.
        let env = spawner.last_env();
        let id = env
            .iter()
            .find(|(k, _)| k == "QD_SESSION_ID")
            .map(|(_, v)| v.clone())
            .expect("daemon env carries QD_SESSION_ID");
        assert!(crate::idstore::is_valid_id(&id), "well-formed id: {id}");
        // ...and the store binds that SAME id to the thread uuid.
        let map = crate::idstore::fold(&h.ids_path);
        assert_eq!(map.by_session.get("T-IDS"), Some(&id));
    }

    // === The full happy sequence writes a correct row (assert EVERY field). ===

    #[test]
    fn happy_sequence_writes_correct_row_all_fields() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy("019e9f4b-thread-uuid");
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18962u16);
        let spawner = FakeSpawner::ok(55501);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let out = run_new_daemon(&deps, &params("cdx", None)).expect("happy create");
        assert_eq!(out.pid, 55501);
        assert_eq!(out.thread_id, "019e9f4b-thread-uuid");
        assert_eq!(out.endpoint, "ws://127.0.0.1:18962");
        assert_eq!(out.first_turn_id, None, "no prompt → no turn");

        // The daemon argv = launch_plan argv + --listen ws://...
        let argv = spawner.last_argv();
        assert_eq!(
            argv,
            vec![
                "codex".to_string(),
                "app-server".to_string(),
                "--listen".to_string(),
                "ws://127.0.0.1:18962".to_string(),
            ]
        );

        // thread/start carried the FULL-BYPASS posture.
        assert!(
            rpc.calls()
                .iter()
                .any(|c| c == "thread_start(never,danger-full-access)"),
            "thread_start full-bypass: {:?}",
            rpc.calls()
        );

        // The row: assert EVERY field (codex-p2-spec §7.2).
        let row = registry::read_entry(&h.sessions_dir, 55501).expect("row written");
        assert_eq!(row.pid, Some(55501));
        assert_eq!(row.session_id.as_deref(), Some("019e9f4b-thread-uuid"));
        assert_eq!(row.cwd.as_deref(), Some("/work/proj"));
        assert_eq!(row.started_at, Some(1_700_000_000_000));
        assert_eq!(row.updated_at, Some(1_700_000_000_000));
        assert_eq!(row.status.as_deref(), Some("idle"));
        assert_eq!(row.name.as_deref(), Some("cdx"));
        assert_eq!(row.provider.as_deref(), Some("codex"));
        assert_eq!(row.endpoint.as_deref(), Some("ws://127.0.0.1:18962"));
        assert_eq!(row.spawned_by, None);
        assert_eq!(row.version, None);
        assert_eq!(row.kind, None);
        assert_eq!(row.backend, None);

        // The daemon was NOT killed on the happy path.
        assert!(spawner.kills().is_empty(), "no kill on happy create");
    }

    // === Failure at connect kills the spawned pid + writes NO row. ===

    #[test]
    fn connect_failure_kills_pid_no_row() {
        let h = harness();
        let exec = exact_exec();
        // connector always fails → spawn loop exhausts; each spawned pid killed.
        let connect = |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
            Err(RpcError::Transport("connection refused".into()))
        };
        let alloc = || Ok(18963u16);
        let spawner = FakeSpawner::ok(55502);
        // A clock that advances so connect_with_retry's budget elapses fast.
        struct StepClock(RefCell<i64>);
        impl Clock for StepClock {
            fn now_ms(&self) -> i64 {
                let mut v = self.0.borrow_mut();
                let now = *v;
                *v += 10_000; // jump 10s per read → budget (5s) elapses immediately
                now
            }
        }
        let clock = StepClock(RefCell::new(0));
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let err = run_new_daemon(&deps, &params("cdx", None)).unwrap_err();
        assert!(
            matches!(err, DaemonError::SpawnFailed { .. }),
            "got {err:?}"
        );
        // Every spawned daemon was killed (the retry ladder spawns SPAWN_RETRIES).
        assert_eq!(spawner.spawn_count() as u32, SPAWN_RETRIES);
        assert_eq!(spawner.kills().len() as u32, SPAWN_RETRIES);
        assert!(spawner.kills().iter().all(|&p| p == 55502));
        // NO row written.
        assert!(!h.sessions_dir.join("55502.json").exists(), "no row");
    }

    // === Failure at initialize kills the spawned pid + writes NO row. ===

    #[test]
    fn initialize_failure_kills_pid_no_row() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc {
            initialize_err: Some(RpcError::Transport("boom".into())),
            thread_start: Ok("T".into()),
            turn_start: Ok("X".into()),
            calls: RefCell::new(Vec::new()),
        };
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18964u16);
        let spawner = FakeSpawner::ok(55503);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let err = run_new_daemon(&deps, &params("cdx", None)).unwrap_err();
        assert!(
            matches!(err, DaemonError::HandshakeFailed { .. }),
            "got {err:?}"
        );
        // Daemon up then handshake failed → the ONE spawned pid killed, no retry
        // (the daemon IS up; the failure is protocol, not a bind race).
        assert_eq!(spawner.spawn_count(), 1, "no respawn on handshake failure");
        assert_eq!(spawner.kills(), vec![55503]);
        assert!(!h.sessions_dir.join("55503.json").exists(), "no row");
    }

    // === Failure at thread/start kills the spawned pid + writes NO row. ===

    #[test]
    fn thread_start_failure_kills_pid_no_row() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc {
            initialize_err: None,
            thread_start: Err(RpcError::Transport("ts boom".into())),
            turn_start: Ok("X".into()),
            calls: RefCell::new(Vec::new()),
        };
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18965u16);
        let spawner = FakeSpawner::ok(55504);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let err = run_new_daemon(&deps, &params("cdx", None)).unwrap_err();
        assert!(
            matches!(err, DaemonError::ThreadStartFailed { .. }),
            "got {err:?}"
        );
        assert_eq!(spawner.spawn_count(), 1);
        assert_eq!(spawner.kills(), vec![55504]);
        assert!(!h.sessions_dir.join("55504.json").exists(), "no row");
    }

    // === A prompt that fails to start a turn is NON-fatal: session exists. ===

    #[test]
    fn prompt_after_create_failure_is_non_fatal() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc {
            initialize_err: None,
            thread_start: Ok("T-NF".into()),
            // The first prompt's turn/start FAILS.
            turn_start: Err(RpcError::Transport("turn boom".into())),
            calls: RefCell::new(Vec::new()),
        };
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18966u16);
        let spawner = FakeSpawner::ok(55505);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let out =
            run_new_daemon(&deps, &params("cdx", Some("hello"))).expect("create succeeds anyway");
        // The session EXISTS: row written, daemon alive, but no turn id.
        assert_eq!(out.first_turn_id, None, "failed prompt → no turn id");
        assert!(spawner.kills().is_empty(), "session NOT torn down");
        let row = registry::read_entry(&h.sessions_dir, 55505).expect("row written");
        assert_eq!(row.session_id.as_deref(), Some("T-NF"));
        assert_eq!(row.provider.as_deref(), Some("codex"));
    }

    // === A prompt that DOES start returns the turn id. ===

    #[test]
    fn prompt_after_create_success_returns_turn_id() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy("T-OK"); // turn_start = Ok("TURN-1")
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18967u16);
        let spawner = FakeSpawner::ok(55506);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let out = run_new_daemon(&deps, &params("cdx", Some("hi"))).expect("create + prompt");
        assert_eq!(out.first_turn_id.as_deref(), Some("TURN-1"));
        assert!(rpc.calls().iter().any(|c| c == "turn_start"));
    }

    // === spawn_detached failure (no daemon ever up) → SpawnFailed, no kill. ===

    #[test]
    fn spawn_failure_no_kill_no_row() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy("T");
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18968u16);
        let spawner = FakeSpawner {
            pid: 0,
            spawns: RefCell::new(Vec::new()),
            kills: RefCell::new(Vec::new()),
            envs: RefCell::new(Vec::new()),
            fail: true,
        };
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let err = run_new_daemon(&deps, &params("cdx", None)).unwrap_err();
        assert!(
            matches!(err, DaemonError::SpawnFailed { .. }),
            "got {err:?}"
        );
        // Nothing to kill (spawn never produced a pid); the loop retried the spawn.
        assert!(spawner.kills().is_empty(), "no pid to kill");
        assert_eq!(spawner.spawn_count() as u32, SPAWN_RETRIES, "retried");
    }

    // ====================================================================
    // W9 FIX M-1: cmdline_is_our_daemon truth-table (the identity helper).
    // ====================================================================

    #[test]
    fn cmdline_is_our_daemon_matches_codex_appserver_with_endpoint() {
        // The happy match: codex + app-server + the recorded --listen endpoint.
        let cmd = "codex app-server --listen ws://127.0.0.1:18962";
        assert!(cmdline_is_our_daemon(
            Some(cmd),
            Some("ws://127.0.0.1:18962")
        ));
        // The endpoint substring is enough even if the launcher reformats `--listen`.
        let cmd2 = "node /x/codex app-server --listen=ws://127.0.0.1:18962 --foo";
        assert!(cmdline_is_our_daemon(
            Some(cmd2),
            Some("ws://127.0.0.1:18962")
        ));
    }

    #[test]
    fn cmdline_is_our_daemon_rejects_foreign_and_wrong_port() {
        // A FOREIGN process (a reused pid) → not ours, even when alive.
        assert!(!cmdline_is_our_daemon(
            Some("/usr/bin/some-daemon --serve"),
            Some("ws://127.0.0.1:18962")
        ));
        // A DIFFERENT codex daemon (right tokens, WRONG port) → not THIS session.
        assert!(!cmdline_is_our_daemon(
            Some("codex app-server --listen ws://127.0.0.1:19999"),
            Some("ws://127.0.0.1:18962")
        ));
        // Missing app-server token → not ours.
        assert!(!cmdline_is_our_daemon(
            Some("codex --version"),
            Some("ws://127.0.0.1:18962")
        ));
    }

    #[test]
    fn cmdline_is_our_daemon_absent_is_not_ours() {
        // A None cmdline (pid not visible / read failed) → treat as gone.
        assert!(!cmdline_is_our_daemon(None, Some("ws://127.0.0.1:18962")));
        // No endpoint discriminator → the codex+app-server tokens are the best we
        // can do (the resume-alive caller always has the endpoint; this is the
        // lost-endpoint fallback).
        assert!(cmdline_is_our_daemon(
            Some("codex app-server --listen ws://127.0.0.1:1"),
            None
        ));
    }

    // ====================================================================
    // W9 FIX M-2: atomic name-claim in the daemon create path.
    // ====================================================================

    // === A PRE-CLAIMED name → run_new_daemon refuses BEFORE the spawn loop (no
    //     spawn, no row). MUTATION EVIDENCE: dropping the claim would let this
    //     proceed to spawn (spawn_count 1, a row written) — the claim is what makes
    //     a duplicate-name create fail loud with NOTHING created. ===
    #[test]
    fn pre_claimed_name_refuses_before_spawn_no_row() {
        let h = harness();
        // A pre-existing claim for "cdx" (a concurrent create already holds it).
        // The holder pid must be genuinely ALIVE and OURS (P0 redfix F2 reaps
        // dead-holder claims; kill(pid,0) on a foreign pid is EPERM = "dead").
        let own_pid = std::process::id();
        registry::claim_name(
            &h.claims_dir,
            "cdx",
            format!("{{\"pid\":{own_pid},\"name\":\"cdx\"}}").as_bytes(),
            &|_| true,
            &|_| None,
        )
        .expect("seed the existing claim");
        let exec = exact_exec();
        let rpc = FixtureRpc::happy("T-CLAIMED");
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18990u16);
        let spawner = FakeSpawner::ok(55600);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let err = run_new_daemon(&deps, &params("cdx", None)).unwrap_err();
        match &err {
            DaemonError::NameClaimed { name, holder } => {
                assert_eq!(name, "cdx");
                assert!(
                    holder.contains(&format!("{own_pid}")),
                    "holder payload surfaced: {holder}"
                );
            }
            other => panic!("expected NameClaimed, got {other:?}"),
        }
        // NOTHING spawned, NO row written (the claim fires before the spawn loop).
        assert_eq!(spawner.spawn_count(), 0, "no spawn on a claimed name");
        assert!(spawner.kills().is_empty(), "no kill — nothing was spawned");
        assert!(
            !h.sessions_dir.join("55600.json").exists(),
            "no row on a claimed name"
        );
        // The named wording matches the claude `NameClaimed` precedent.
        let msg = err.to_string();
        assert!(
            msg.contains("being created by another process"),
            "msg: {msg}"
        );
        assert!(msg.contains("No session was created"), "msg: {msg}");
    }

    // === The claim is RELEASED on a mid-sequence failure: a thread/start failure
    //     tears down + releases the claim, so a SECOND create with the SAME name
    //     then SUCCEEDS. MUTATION EVIDENCE: if the claim were NOT released on the
    //     failure path, the second create would (wrongly) fail NameClaimed. ===
    #[test]
    fn claim_released_on_failure_then_second_create_succeeds() {
        let h = harness();
        let exec = exact_exec();
        // First create: thread/start FAILS (a mid-sequence failure after the claim).
        let rpc_fail = FixtureRpc {
            initialize_err: None,
            thread_start: Err(RpcError::Transport("ts boom".into())),
            turn_start: Ok("X".into()),
            calls: RefCell::new(Vec::new()),
        };
        let connect_fail = |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
            Ok(Box::new(RpcRef(&rpc_fail)))
        };
        let alloc = || Ok(18991u16);
        let spawner1 = FakeSpawner::ok(55601);
        let deps1 = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner1,
            connect: &connect_fail,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let err = run_new_daemon(&deps1, &params("cdx", None)).unwrap_err();
        assert!(
            matches!(err, DaemonError::ThreadStartFailed { .. }),
            "first create fails mid-sequence: {err:?}"
        );
        // The claim file for "cdx" was RELEASED on the failure path (RAII drop).
        // "cdx" is all `[a-z]`, so the path-safe claim stem is the identity "cdx".
        let claim_path = h.claims_dir.join("cdx.claim");
        assert!(
            !claim_path.exists(),
            "the claim must be released on the failure path: {claim_path:?}"
        );

        // Second create with the SAME name → SUCCEEDS (the claim was released).
        let exec2 = exact_exec();
        let rpc_ok = FixtureRpc::happy("T-SECOND");
        let connect_ok = |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
            Ok(Box::new(RpcRef(&rpc_ok)))
        };
        let alloc2 = || Ok(18992u16);
        let spawner2 = FakeSpawner::ok(55602);
        let deps2 = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec2,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner2,
            connect: &connect_ok,
            alloc_port: &alloc2,
            ids_path: h.ids_path.clone(),
        };
        let out = run_new_daemon(&deps2, &params("cdx", None))
            .expect("the second create reuses the released name");
        assert_eq!(out.thread_id, "T-SECOND");
        assert!(
            h.sessions_dir.join("55602.json").exists(),
            "the second create wrote its row"
        );
    }

    // === The happy path CLAIMS, writes the row, and RELEASES the claim (the claude
    //     claim lifetime: held through the row write, dropped at return). ===
    #[test]
    fn happy_path_releases_claim_after_row_write() {
        let h = harness();
        let exec = exact_exec();
        let rpc = FixtureRpc::happy("T-HAPPY-CLAIM");
        let connect =
            |_url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> { Ok(Box::new(RpcRef(&rpc))) };
        let alloc = || Ok(18993u16);
        let spawner = FakeSpawner::ok(55603);
        let deps = DaemonDeps {
            provider: &crate::provider::codex::CODEX_PROVIDER,
            env: &h.env,
            exec: &exec,
            clock: &h.clock,
            sessions_dir: h.sessions_dir.clone(),
            claims_dir: h.claims_dir.clone(),
            log_dir: h.log_dir.clone(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: h.ids_path.clone(),
        };
        let out = run_new_daemon(&deps, &params("cdx", None)).expect("happy create");
        assert_eq!(out.thread_id, "T-HAPPY-CLAIM");
        // The row exists (the durable record).
        assert!(h.sessions_dir.join("55603.json").exists(), "row written");
        // The claim was RELEASED at return (the create window is closed by the row).
        let claim_path = h.claims_dir.join("cdx.claim");
        assert!(
            !claim_path.exists(),
            "the claim is released after the row write (claude lifetime): {claim_path:?}"
        );
    }
}
