//! `provider::acp::daemon` — the acp DAEMON-RESIDENCE create + resume
//! choreography (scoped-ACP-CC S5 + Item 3).
//!
//! The acp twin of [`crate::provider::pi::daemon`], and the acp counterpart to
//! [`crate::provider::codex::resume`]'s revive: allocate a loopback port, spawn
//! the resident `qd acp-daemon` adapter DETACHED (reusing the codex
//! [`RealDaemonSpawner`](crate::create_daemon::RealDaemonSpawner)'s
//! `process_group(0)` discipline — so a later group-kill reaps adapter + bridge
//! together), poll it to readiness, write the registry row with the recorded
//! `endpoint` so later verbs reconnect. The adapter OUTLIVES the call — that is
//! cross-process residence.
//!
//! Split out of the `qd` verb bodies. These two were the LAST provider
//! choreographies still living entirely inside verb functions: unlike the codex
//! and pi daemon lanes, there was no lib-side core behind them at all, so the
//! whole sequence was interleaved with its own printing. Nothing here prints and
//! nothing here exits.
//!
//! ── THE LOAD-BEARING ORDER IN [`resume_acp`] ────────────────────────────────
//! Three gates run in a fixed order, and the order is the safety property:
//!
//!   1. **Case-1 already-alive gate** ([`acp_resume_is_alive`]): pid-alive ∧ the
//!      live cmdline carries the recorded `--listen`. A hit is a SUCCESS no-op
//!      with ZERO mutation and NO second adapter (the (R-c) seam). No
//!      reachability connect — that would misread a busy-but-alive adapter,
//!      camped in another client's wait, as dead and double-spawn.
//!   2. **R5-1 raw-registry id preflights**
//!      ([`crate::resume::id_collision_refusal`] + [`crate::resume::alive_pid_for_id`]).
//!      These MUST come after (1) so the ratified already-running no-op is
//!      unchanged when the single live holder is this session's OWN
//!      identity-verified daemon — and before (3) so no claim is taken for a
//!      resume that is about to refuse.
//!   3. **The concurrent-resume atomic claim**
//!      ([`acquire_resume_claim`]), held across the whole spawn→row-write
//!      critical section.
//!
//! Getting (2) before (1) turns every already-running no-op into a refusal;
//! getting (2) after (3) revives a SECOND live writer onto one CC transcript
//! while holding the lock that was supposed to prevent exactly that. The
//! pre-split verb pinned this order with a SOURCE-TEXT test that grepped its own
//! function body for the two preflight call sites — the only tool available when
//! the sequence lived in a verb. It is pinned here by
//! `preflights_run_after_the_alive_gate_and_before_the_claim`, which drives the
//! real function with recording fakes and asserts the observed order. That is
//! strictly stronger: the source-text version could not tell a call from a
//! comment, and could not see the claim at all.
//!
//! ── THE WARNING SINK ────────────────────────────────────────────────────────
//! Both lanes emit NON-FATAL notices mid-flow (a stable-id mint that failed
//! soft, a bind that did not take, a create-prompt that would not enqueue). They
//! are interleaved with the choreography — a later hard failure must still leave
//! the earlier notice on the user's terminal — so they cannot ride the success
//! outcome. They go through [`AcpDaemonDeps::warn`] instead: the TEXT is here,
//! typed, in one match ([`AcpWarning`]); only the emission is the binary's.
//!
//! ── WHY THE `qd <verb>:` PREFIX IS IN THE `Display` HERE ────────────────────
//! Unlike [`crate::provider::claude::revive`], whose five callers each type a
//! different command, these two lanes have ONE verb each — `qd start` for the
//! create, `qd resume` for the revive, from every caller including the send-side
//! wake and the lane seam. The pre-split verbs hard-coded exactly that, so the
//! whole line lives in the match here (the [`crate::create::NewError`] shape) and
//! the binary is a bare `eprintln!("{e}")`. Two error enums, one verb each.

use std::path::PathBuf;
use std::time::Duration;

use crate::create_daemon::{CmdlineProbe, DaemonSpawner, PortAllocator};
use crate::effects::Clock;
use crate::paths::QdPaths;
use crate::registry::{self, RegistryEntry};
use crate::resume::IdCollisionRefusal;

use super::rpc::AcpClient;
use super::residence::{build_adapter_argv, connect_ready};
use super::resume::{
    acp_resume_is_alive, acquire_resume_claim, resume_verify_marker_path,
    write_resume_verify_marker, ResumeVerifyMarker,
};

/// Readiness budget for the resident adapter's front to come up AND establish the
/// ACP session (create) / re-load it (resume).
const READY_BUDGET: Duration = Duration::from_secs(30);

/// A NON-FATAL notice emitted mid-choreography. See the module docs on why these
/// go through a sink rather than riding the outcome.
///
/// `Display` carries the COMPLETE line, `qd <verb>:` prefix included — each
/// variant belongs to exactly one lane, so the verb is not in question.
#[derive(Debug, Clone, PartialEq)]
pub enum AcpWarning {
    /// CREATE: the pre-minted stable id could not be bound to the ACP session
    /// UUID once readiness disclosed it. The session is LIVE; the id just stays
    /// unbound, which `resolve_to_uuid` will miss.
    CreateIdBindFailed { session_id: String, detail: String },
    /// CREATE: the create-time prompt would not enqueue. The session is up; we do
    /// NOT tear it down over a prompt-enqueue error.
    CreatePromptEnqueueFailed { detail: String },
    /// RESUME: the stable id could not be got/minted. Fail-SOFT here (unlike the
    /// create lane's fail-closed mint): the adapter is spawned with an EXPLICITLY
    /// EMPTY `QD_SESSION_ID` so it cannot inherit the caller's id.
    ResumeIdMintFailed { detail: String },
}

impl std::fmt::Display for AcpWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpWarning::CreateIdBindFailed { session_id, detail } => write!(
                f,
                "qd start: could not bind stable id to acp session {session_id}: {detail}"
            ),
            AcpWarning::CreatePromptEnqueueFailed { detail } => {
                write!(f, "qd start: acp create-prompt enqueue failed: {detail}")
            }
            AcpWarning::ResumeIdMintFailed { detail } => write!(
                f,
                "qd resume: could not get/mint stable id for acp session: {detail}"
            ),
        }
    }
}

/// Injected effects for one acp create or resume. Nothing is constructed in here:
/// HOME, the paths and the self-exe are the binary's to resolve.
pub struct AcpDaemonDeps<'a> {
    /// The self-exe path. The adapter IS this binary under the hidden
    /// `acp-daemon` verb, so the argv starts with it.
    pub exe: PathBuf,
    /// The resolved HOME — the adapter's stdout/stderr log lands under it (L9a:
    /// a jailed HOME puts the log in the jail).
    pub home: PathBuf,
    /// Home→state layout: the registry row, the resume claim, the ids store and
    /// the projects dir all hang off this.
    pub paths: &'a QdPaths,
    /// Clock — the row stamps and the idstore lines.
    pub clock: &'a dyn Clock,
    /// The detached-spawn seam (`process_group(0)` + the `-pgid` group kill).
    pub spawner: &'a dyn DaemonSpawner,
    /// The loopback port allocator (real OS bind in production; a deterministic
    /// fake in units).
    pub alloc_port: &'a PortAllocator<'a>,
    /// The connectionless `pid -> cmdline` probe the Case-1 already-alive gate
    /// consults. RESUME only.
    pub cmdline_probe: &'a CmdlineProbe<'a>,
    /// The non-fatal notice sink. See the module docs.
    pub warn: &'a dyn Fn(&AcpWarning),
}

// ===========================================================================
// CREATE (scoped-ACP-CC S5)
// ===========================================================================

/// Params for one acp-residence create.
pub struct AcpCreateParams {
    /// The session name (the row's `name` and the adapter's log file stem).
    pub name: String,
    /// THIS row's provider id — `acp/claude-code` or `acp/opencode`. A-OC.1: the
    /// bridge is re-derived from it, and it is persisted so every other verb
    /// (kill/wait/resume/send:relay) routes and re-derives the same way.
    pub provider_id: String,
    /// The adapter's cwd. The bridge resolves the CC JSONL by
    /// `encodeProjectPath(cwd)`, so this is identity-bearing, not cosmetic.
    pub cwd: PathBuf,
    /// An optional create-time prompt, driven over the SAME connection once the
    /// session is established.
    pub prompt: Option<String>,
}

/// Success outcome of [`create_acp_daemon`].
#[derive(Debug, Clone, PartialEq)]
pub struct AcpCreateOutcome {
    pub name: String,
    /// The STABLE qd id minted for this session, plumbed out so a caller that did
    /// not mint it can still report it (`qd start --json`'s `qdId`, and
    /// [`crate::contract::SessionHandle::qd_id`]). The mint happens inside this
    /// function, so before this field the only way to learn the id was to be the
    /// minter.
    pub qd_session_id: String,
    /// The resident adapter pid (the registry row key + the pgid teardown key).
    pub pid: i64,
    /// The ws endpoint the row records as the residence reconnect handle (S5).
    pub endpoint: String,
    /// The ACP session UUID, read off the front after readiness.
    pub session_id: String,
}

/// Why an acp create failed. Every variant means: the spawned adapter (if any)
/// was group-killed and NO registry row survives.
///
/// `Display` carries the COMPLETE `qd start: …` line — see the module docs.
#[derive(Debug)]
pub enum AcpCreateError {
    PortAllocFailed { detail: String },
    ExeUnresolved { detail: String },
    IdMintFailed { detail: String },
    SpawnFailed { detail: String },
    /// Readiness never landed. Carries the adapter's log path — the only place
    /// the bridge's own failure is visible.
    NotReady { detail: String, log_path: PathBuf },
    RowWriteFailed { detail: String },
}

impl AcpCreateError {
    /// Process exit code. All acp create failures are exit 1 (the verb precedent).
    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl std::fmt::Display for AcpCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpCreateError::PortAllocFailed { detail } => {
                write!(f, "qd start: acp port allocation failed: {detail}")
            }
            AcpCreateError::ExeUnresolved { detail } => write!(
                f,
                "qd start: cannot resolve own executable for acp adapter: {detail}"
            ),
            AcpCreateError::IdMintFailed { detail } => write!(
                f,
                "qd start: could not mint stable id for acp session: {detail}"
            ),
            AcpCreateError::SpawnFailed { detail } => {
                write!(f, "qd start: acp adapter spawn failed: {detail}")
            }
            AcpCreateError::NotReady { detail, log_path } => {
                write!(f, "qd start: {detail} (see {})", log_path.display())
            }
            AcpCreateError::RowWriteFailed { detail } => {
                write!(f, "qd start: acp registry write failed: {detail}")
            }
        }
    }
}

impl std::error::Error for AcpCreateError {}

/// The adapter's stdout/stderr log: `<home>/.quorum/dispatch/log/acp-<name>.log`.
fn acp_log_path(home: &std::path::Path, name: &str) -> PathBuf {
    home.join(".quorum")
        .join("dispatch")
        .join("log")
        .join(format!("acp-{name}.log"))
}

/// A-OC.1: re-derive THIS provider's bridge so create and resume spawn the SAME
/// one. `acp/claude-code` keeps the `BRIDGE_BIN` default (`bridge_cmd` None →
/// `build_adapter_argv` emits NO `--bridge-cmd`, byte-identical to before this
/// existed); `acp/opencode` yields `--bridge-cmd opencode --bridge-arg acp`.
/// Without this a resumed opencode session would respawn `claude-code-acp`
/// loading an opencode session — the verb-routing-arms trap.
fn bridge_for(provider_id: &str, harness_port: Option<u16>) -> (Option<&'static str>, Vec<String>) {
    let acp = super::acp_provider_for(provider_id);
    let bridge_cmd = acp.and_then(|p| p.bridge_cmd());
    let mut bridge_args: Vec<String> = acp
        .map(|p| p.bridge_args().iter().map(|a| a.to_string()).collect())
        .unwrap_or_default();
    // Pin the bridge's OWN server, for a bridge that runs one. This is the whole
    // mechanism: the adapter already forwards every `--bridge-arg` verbatim, so
    // pinning opencode's HTTP port needs no adapter change at all — it is two
    // more args on a list the residence layer has always passed through.
    //
    // The two Options are read TOGETHER on purpose. A port allocated for a bridge
    // with no server would be a port bound to nothing, and a server spec with no
    // port would silently leave the default `0` in place — an ephemeral port, a
    // live server, and no way to name it, which is exactly the state this
    // function exists to end. Either half missing ⇒ the pre-existing argv.
    if let (Some(server), Some(port)) = (acp.and_then(|p| p.harness_server()), harness_port) {
        bridge_args.extend(server.port_args(port));
    }
    (bridge_cmd, bridge_args)
}

/// Allocate the bridge's own server port — for a bridge that HAS one.
///
/// `Ok(None)` is the ordinary answer for `acp/claude-code` and means "asked and
/// there is nothing to allocate", not "failed". Only a real allocator failure is
/// `Err`, and it is fatal for the same reason the ws port's is: a create that
/// pressed on would leave the bridge listening on an ephemeral port, which is
/// the un-nameable server this whole path exists to replace.
fn alloc_harness_port(
    provider_id: &str,
    alloc: &PortAllocator<'_>,
) -> Result<Option<u16>, std::io::Error> {
    match super::acp_provider_for(provider_id).and_then(|p| p.harness_server()) {
        Some(_) => alloc().map(Some),
        None => Ok(None),
    }
}

/// The `harnessEndpoint` string for a row, given the port [`alloc_harness_port`]
/// handed out. `None` whenever there is no server or no port — the row then
/// carries no such key at all, which is what keeps every `acp/claude-code` row
/// byte-stable.
fn harness_endpoint_for(provider_id: &str, harness_port: Option<u16>) -> Option<String> {
    let server = super::acp_provider_for(provider_id).and_then(|p| p.harness_server())?;
    Some(server.endpoint(harness_port?))
}


/// Drive the optional create-time prompt over `conn`, returning whether a structured
/// send was DISPATCHED (bytes confirmed on the wire — [`AcpClient::prompt`]'s
/// `on_dispatched`, fired before the reply is read).
///
/// Factored out of the create choreography (Child B, opencode D1, F1 fix) so the
/// dispatch-to-`structured_send_issued` mapping is unit-testable against a fake
/// client with no socket: red-team round 1 found a create-time `--prompt`
/// dispatched a real structured send while the row was ALWAYS written
/// `structured_send_issued: None`, making the session look pre-send forever.
///
/// `None`/`""` both mean "no create-time prompt" and report `false` — a genuinely
/// pre-send session. A prompt that FAILS before reaching the wire also reports
/// `false`, and warns: the session is up, and we do not tear it down over a
/// prompt-enqueue error.
pub fn drive_create_prompt(
    conn: &dyn AcpClient,
    session_id: &str,
    prompt: Option<&str>,
    name: &str,
    warn: &dyn Fn(&AcpWarning),
) -> bool {
    let dispatched = std::cell::Cell::new(false);
    if let Some(p) = prompt.filter(|s| !s.is_empty()) {
        let mark_dispatched = || dispatched.set(true);
        if let Err(e) = conn.prompt(session_id, p, name, &mark_dispatched) {
            warn(&AcpWarning::CreatePromptEnqueueFailed {
                detail: e.to_string(),
            });
        }
    }
    dispatched.get()
}

/// scoped-ACP-CC daemon-residence create path (S5). See the module docs.
pub fn create_acp_daemon(
    deps: &AcpDaemonDeps<'_>,
    params: &AcpCreateParams,
) -> Result<AcpCreateOutcome, AcpCreateError> {
    let name = params.name.as_str();

    // 1. allocate a loopback port → the resident ws endpoint.
    let port = (deps.alloc_port)().map_err(|e| AcpCreateError::PortAllocFailed {
        detail: e.to_string(),
    })?;
    let endpoint = format!("ws://127.0.0.1:{port}");
    // 1b. and, for a bridge that runs a server of its own, a SECOND loopback port
    //     — the one the bridge listens on rather than the one the adapter does.
    //     Allocated here, beside the ws port, so both addresses this session will
    //     ever answer at are decided before anything is spawned.
    let harness_port = alloc_harness_port(&params.provider_id, deps.alloc_port).map_err(|e| {
        AcpCreateError::PortAllocFailed {
            detail: e.to_string(),
        }
    })?;

    // 2. self-exec: the adapter IS this binary under the hidden `acp-daemon` verb.
    if deps.exe.as_os_str().is_empty() {
        return Err(AcpCreateError::ExeUnresolved {
            detail: "no executable path".to_string(),
        });
    }

    // 3. spawn the adapter DETACHED (codex RealDaemonSpawner reuse: process_group(0),
    //    stdin null, stdout/stderr → log). The bridge child inherits the group.
    // create path: no `--load-session` (a brand-new session/new, not a resume).
    let (bridge_cmd, bridge_args) = bridge_for(&params.provider_id, harness_port);
    let argv = build_adapter_argv(
        &deps.exe,
        &endpoint,
        &params.cwd,
        bridge_cmd,
        &bridge_args,
        None,
    );
    let log_path = acp_log_path(&deps.home, name);
    // Mint an unbound stable id for this ACP session (mirrors the Codex create path).
    // The ACP session UUID is not known until after readiness; mint_unbound creates a
    // stable id entry with session_id=null; bind() attaches the UUID after the adapter
    // is ready. Fail-closed: nothing spawns if the mint fails.
    let ids_path = crate::idstore::ids_path(&deps.paths.state_dir);
    let acp_qd_id = crate::idstore::mint_unbound(&ids_path, Some(name), deps.clock)
        .map_err(|detail| AcpCreateError::IdMintFailed { detail })?;
    let acp_env = vec![("QD_SESSION_ID".to_string(), acp_qd_id.clone())];
    let spawned = deps
        .spawner
        .spawn_detached(&argv, &acp_env, &params.cwd, &log_path)
        .map_err(|e| AcpCreateError::SpawnFailed {
            detail: e.to_string(),
        })?;

    // 4. readiness: poll connect+status until the resident ACP session is established
    //    (the codex connect-with-retry analog). On failure: group-kill the adapter (no
    //    orphan), surface the error.
    let conn = match connect_ready(&endpoint, READY_BUDGET) {
        Ok(c) => c,
        Err(detail) => {
            deps.spawner.kill(spawned.pid);
            return Err(AcpCreateError::NotReady { detail, log_path });
        }
    };
    let session_id = conn.status_session_id().ok().flatten().unwrap_or_default();

    // Bind the pre-minted stable id to the ACP session UUID now that we have it.
    // Best-effort: a bind failure is warned but does not abort the create (the session
    // is live; the id just remains unbound, which resolve_to_uuid will miss).
    if !session_id.is_empty() {
        if let Err(detail) = crate::idstore::bind(&ids_path, &acp_qd_id, &session_id, deps.clock) {
            (deps.warn)(&AcpWarning::CreateIdBindFailed {
                session_id: session_id.clone(),
                detail,
            });
        }
    }

    // 5. optional create-time prompt: drive it over the SAME connection (the resident
    //    keeps streaming after we disconnect). Non-blocking — `wait` observes the turn.
    //
    // F1 (red-team round 1, Child B era): the registry row doesn't exist yet at
    // this point (written at step 6, below) — but a dispatched create-time prompt
    // is EXACTLY the "a structured send was issued" case `structured_send_issued`
    // exists to record. Getting this wrong leaves a false "never sent"
    // wire-history on the row (in the retired auto-degrade era that meant
    // double-delivery risk; under Child D the record is history truth the
    // resume seam consumes).
    let dispatched = drive_create_prompt(
        &conn,
        &session_id,
        params.prompt.as_deref(),
        name,
        deps.warn,
    );
    drop(conn); // resident stays up; this was a short-lived create connection.

    // 6. write the registry row (the endpoint is the residence reconnect handle, S5).
    let now = deps.clock.now_ms();
    let entry = RegistryEntry {
        pid: Some(spawned.pid),
        session_id: Some(session_id.clone()),
        cwd: Some(params.cwd.to_string_lossy().into_owned()),
        started_at: Some(now),
        updated_at: Some(now),
        status: Some("idle".to_string()),
        name: Some(name.to_string()),
        version: None,
        kind: None,
        entrypoint: None,
        backend: None,
        spawned_by: None,
        // A-OC.1: persist THIS provider's id (acp/claude-code OR acp/opencode) so the other
        // verbs (kill/wait/resume/send:relay) route + re-derive the bridge from the row.
        provider: Some(params.provider_id.clone()),
        endpoint: Some(endpoint.clone()),
        // A freshly-created healthy row carries NO `transport` field (the tier
        // is DERIVED per verb; the field is write-retired — historical
        // Child-B-era latch, see registry.rs's field doc).
        transport: None,
        // Child B (opencode D1), F1 fix: `Some(true)` iff the create-time prompt
        // was DISPATCHED above — i.e. a structured send genuinely went out for
        // this session before its row ever existed. `None` only for a truly
        // prompt-less create.
        structured_send_issued: dispatched.then_some(true),
        // acp/* is daemon-hosted with no second topology, so this token can
        // never be anything else — which is exactly why it is written rather
        // than left to be inferred. An absent field means "ask the harness
        // default", and that is a question with an answer somewhere else; a
        // present one means "this row is a daemon row" and needs no lookup.
        hosting: Some(crate::lane::Mode::Daemon.hosting_token().to_string()),
        // The bridge's OWN server, for the one bridge that has one — the viewer's
        // reconnect handle (`acp/opencode`), absent for `acp/claude-code`. Written
        // from the port pinned at step 1b, so the row names the address the bridge
        // was actually told to listen on rather than one discovered afterwards.
        harness_endpoint: harness_endpoint_for(&params.provider_id, harness_port),
    };
    if let Err(e) = registry::write_entry(&deps.paths.sessions_dir, &entry) {
        deps.spawner.kill(spawned.pid);
        return Err(AcpCreateError::RowWriteFailed {
            detail: e.to_string(),
        });
    }

    Ok(AcpCreateOutcome {
        name: params.name.clone(),
        qd_session_id: acp_qd_id,
        pid: spawned.pid,
        endpoint,
        session_id,
    })
}

// ===========================================================================
// RESUME (scoped-ACP-CC Item 3)
// ===========================================================================

/// Params for one acp resume — the resolved row's identity fields.
pub struct AcpResumeParams {
    pub name: String,
    /// The ACP `session/load` handle. Empty ⇒ the resumability gate refuses.
    pub session_id: String,
    /// THIS row's provider id. A-OC.1: gates the CC-transcript requirement AND
    /// re-derives the bridge.
    pub provider_id: String,
    /// The row's recorded cwd. Empty/absent ⇒ `"."`.
    pub cwd: Option<String>,
    /// Whether a CC transcript was resolved for the row. `acp/claude-code`
    /// REQUIRES one; `acp/opencode` does not (it persists to opencode's own
    /// store).
    pub has_jsonl: bool,
    /// The row's CURRENT recorded adapter pid (Case-1 gate input + the OLD row to
    /// supersede).
    pub current_pid: Option<i64>,
    /// The row's CURRENT recorded endpoint. NOT on the `Session` surface — the
    /// binary re-reads it off the row by pid, mirroring the codex/pi arms.
    pub current_endpoint: Option<String>,
}

/// What [`resume_acp`] decided + did. Both arms are SUCCESS (exit 0).
#[derive(Debug, Clone, PartialEq)]
pub enum AcpResumeOutcome {
    /// The adapter was already alive (pid-alive ∧ our cmdline carries the
    /// recorded `--listen`). ZERO mutation, NO second adapter — the (R-c) seam.
    AlreadyRunning { name: String },
    /// A fresh resident was spawned in LOAD mode and confirmed serving the SAME
    /// sessionId; the row now carries the NEW pid + endpoint.
    Revived {
        name: String,
        pid: i64,
        endpoint: String,
    },
}

/// Why an acp resume failed.
///
/// `Display` carries the COMPLETE `qd resume: …` line — see the module docs.
#[derive(Debug)]
pub enum AcpResumeError {
    /// Resumability gate: no sessionId (the ACP `session/load` handle), or —
    /// for `acp/claude-code` only — no CC transcript for the bridge's load to
    /// read.
    NoResumableTranscript { name: String },
    /// R5-1: ≥2 ALIVE rows share this id. Carries the rows to list.
    IdCollision(IdCollisionRefusal),
    /// R5-1: exactly one OTHER live holder of this id. Reviving beside it would
    /// put two live writers on one CC transcript.
    AlreadyAliveElsewhere { name: String, pid: i64 },
    /// FINDING #3: another `qd resume` of this SAME row holds the atomic claim.
    ResumeInProgress { name: String },
    ClaimLockFailed { name: String, detail: String },
    PortAllocFailed { name: String, detail: String },
    ExeUnresolved { name: String, detail: String },
    SpawnFailed { name: String, detail: String },
    NotReady {
        name: String,
        detail: String,
        log_path: PathBuf,
    },
    /// WRONG-ADAPTER GUARD: the endpoint we connected to reports a DIFFERENT
    /// sessionId — a stale/reused port now serving someone else. The
    /// just-spawned adapter was group-killed and NO row was written.
    WrongAdapter {
        name: String,
        established: String,
        requested: String,
    },
    RowWriteFailed { name: String, detail: String },
}

impl AcpResumeError {
    /// Process exit code. All acp resume failures are exit 1 (the verb precedent).
    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl std::fmt::Display for AcpResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpResumeError::NoResumableTranscript { name } => write!(
                f,
                "qd resume: session \"{name}\" was stopped and has no resumable transcript — \
                 nothing to resume."
            ),
            // MULTI-LINE by construction (a header plus one line per colliding
            // row). The pre-split verb emitted it as N separate `eprintln!`s,
            // which is byte-identical to one write with the newlines embedded.
            AcpResumeError::IdCollision(r) => write!(f, "{}", r.lines("resume").join("\n")),
            AcpResumeError::AlreadyAliveElsewhere { name, pid } => write!(
                f,
                "qd resume: session \"{name}\" is already alive (PID {pid}). \
                 Use \"qd attach\" instead."
            ),
            AcpResumeError::ResumeInProgress { name } => write!(
                f,
                "qd resume: \"{name}\": another resume of this session is already in \
                 progress — refusing (no double-spawn). Try again once it completes."
            ),
            AcpResumeError::ClaimLockFailed { name, detail } => write!(
                f,
                "qd resume: \"{name}\": could not take the resume claim lock: {detail}"
            ),
            AcpResumeError::PortAllocFailed { name, detail } => {
                write!(f, "qd resume: \"{name}\": acp port allocation failed: {detail}")
            }
            AcpResumeError::ExeUnresolved { name, detail } => write!(
                f,
                "qd resume: \"{name}\": cannot resolve own executable for acp adapter: {detail}"
            ),
            AcpResumeError::SpawnFailed { name, detail } => {
                write!(f, "qd resume: \"{name}\": acp adapter spawn failed: {detail}")
            }
            AcpResumeError::NotReady {
                name,
                detail,
                log_path,
            } => write!(
                f,
                "qd resume: \"{name}\": {detail} (see {})",
                log_path.display()
            ),
            AcpResumeError::WrongAdapter {
                name,
                established,
                requested,
            } => write!(
                f,
                "qd resume: \"{name}\": the endpoint is serving a DIFFERENT acp session \
                 ({established:?} != {requested:?}) — refusing (wrong/stale adapter, not our row)."
            ),
            AcpResumeError::RowWriteFailed { name, detail } => write!(
                f,
                "qd resume: \"{name}\": revived the acp adapter but its registry row could not \
                 be written ({detail}); the adapter was stopped."
            ),
        }
    }
}

impl std::error::Error for AcpResumeError {}

/// R4-1 (conformance round 4, confirmed real): read the PRE-resume row's
/// `structured_send_issued` bit so a resume can carry it forward — a session's
/// send-history is a fact about its whole life, not the connection that just
/// died, so it must survive `qd resume`. (Under Child D the loss disposition no
/// longer branches on it — pre- and post-send loss both refuse — but the marker
/// stays the durable wire-history truth, and losing it on resume would leave a
/// false "never sent" record for any consumer, present or future.)
///
/// Checks the LIVE row first (the crash-path shape: the process died but its
/// `<pid>.json` was never renamed), falling back to the TOMBSTONED row (the
/// ordinary `qd stop` shape: `<pid>.json` renamed to `<pid>.json.tombstoned`) —
/// `qd stop` is the common case, so the fallback is load-bearing, not a rare
/// edge. `None` (no `pid`, no row either way, or the row never dispatched a
/// structured send) means "no history to carry" — a genuinely fresh row.
pub fn carry_forward_structured_send_issued(
    sessions_dir: &std::path::Path,
    pid: Option<i64>,
) -> Option<bool> {
    pid.and_then(|pid| {
        registry::read_entry(sessions_dir, pid)
            .or_else(|| registry::read_tombstoned_entry(sessions_dir, pid))
            .and_then(|e| e.structured_send_issued)
    })
}

/// scoped-ACP-CC Item 3 — the acp RESUME choreography. Mirrors the codex revive
/// 1:1, substituting `session/load` (the ACP resume primitive, driven by the
/// load-mode adapter) for `thread/resume`. See the module docs for the gate order.
pub fn resume_acp(
    deps: &AcpDaemonDeps<'_>,
    params: &AcpResumeParams,
) -> Result<AcpResumeOutcome, AcpResumeError> {
    let name = params.name.clone();
    let sessions_dir = &deps.paths.sessions_dir;

    // Resumability gate (the acp analog of resume.rs's `no resumable transcript` arm):
    // a stopped acp row always needs a sessionId (the ACP `session/load` handle). acp/claude-code
    // ADDITIONALLY needs a jsonl_path — the CC store the bridge's load reads. A-OC.1: acp/opencode
    // persists to opencode's OWN store (NOT the CC projects dir), so it has no jsonl_path; gate it
    // on the sessionId alone (opencode advertises the `loadSession` capability). The provider
    // check keeps acp/claude-code's gate BYTE-IDENTICAL.
    let needs_cc_transcript = params.provider_id == "acp/claude-code";
    if params.session_id.is_empty() || (needs_cc_transcript && !params.has_jsonl) {
        return Err(AcpResumeError::NoResumableTranscript { name });
    }

    // Case 1: ALREADY ALIVE → clean no-op, ZERO mutation, NO second adapter. pid-alive ∧
    // identity (the cmdline carries the recorded `--listen <endpoint>`), mirroring the
    // codex gate — NO reachability connect (it would misread a busy-but-alive adapter,
    // camped in another client's wait, as dead and double-spawn). The (R-c) seam.
    if acp_resume_is_alive(
        params.current_pid,
        params.current_endpoint.as_deref(),
        deps.cmdline_probe,
    ) {
        return Ok(AcpResumeOutcome::AlreadyRunning { name });
    }

    // R5-1 (red-team round 5, confirmed at source): the raw-registry preflights the
    // non-acp resume path runs (Pete feedback #6) were structurally bypassed for
    // acp/* rows — the acp arm dispatches BEFORE them, and this function had no
    // equivalent (`acp_resume_is_alive` probes only the daemon's OWN pid/identity,
    // never other live holders of the id). Consequence: `qd resume` of an acp
    // session while another live process holds this SAME session_id/transcript
    // (Child B's retired floor companion then; today a leftover dev companion row
    // or a manually-launched `claude --resume`) would revive a fresh acp daemon
    // (`session/load`, the same transcript re-opened) BESIDE the live holder — two
    // live writers on one CC transcript, the precise collision these gates exist to
    // prevent everywhere else — and silently clear any historical degradation latch
    // (`transport: None`, below) under it.
    //
    // Placed AFTER the Case-1 gate above so the ratified already-running no-op
    // (exit 0) is unchanged when the single live holder is this session's OWN
    // identity-verified daemon; any OTHER live holder of the id (a plain twin
    // on the same transcript, a reused pid on a stale row, a genuine duplicate)
    // refuses here — recoverable by connecting to the holder, or killing the
    // holder and resuming again. Both checks run on the RAW registry
    // (pre-dedup), exactly like the non-acp path, because the join hides
    // same-id rows.
    if let Some(refusal) = crate::resume::id_collision_refusal(sessions_dir, &params.session_id) {
        return Err(AcpResumeError::IdCollision(refusal));
    }
    if let Some(pid) = crate::resume::alive_pid_for_id(sessions_dir, &params.session_id) {
        return Err(AcpResumeError::AlreadyAliveElsewhere { name, pid });
    }

    // FINDING #3 — CONCURRENT-RESUME ATOMIC CLAIM (acp-only): take an exclusive,
    // self-healing flock on this sessionId BEFORE spawning, held across the whole
    // spawn→row-write critical section. Two concurrent `qd resume` of the SAME stopped
    // row → exactly ONE wins the claim and spawns; the LOSER refuses cleanly (no spawn,
    // no mutation). flock auto-releases on holder death → a crashed holder NEVER bricks a
    // later resume (self-healing; NOT a bare lock). NOTE: acp adds this concurrent-resume
    // atomic guard that codex lacks — codex daemon-resume parity is a named follow-on.
    let _resume_claim = match acquire_resume_claim(sessions_dir, &params.session_id) {
        Ok(Some(claim)) => claim, // WON — held until end of fn (drop releases the flock).
        Ok(None) => return Err(AcpResumeError::ResumeInProgress { name }),
        Err(e) => {
            return Err(AcpResumeError::ClaimLockFailed {
                name,
                detail: e.to_string(),
            })
        }
    };

    // Case 2/3: REVIVE — re-spawn the resident adapter in LOAD mode. Mirrors the create
    // path, with `--load-session <sessionId>` substituted for the fresh `session/new`.
    // The `_resume_claim` flock above serializes concurrent revives.
    let port = match (deps.alloc_port)() {
        Ok(p) => p,
        Err(e) => {
            return Err(AcpResumeError::PortAllocFailed {
                name,
                detail: e.to_string(),
            })
        }
    };
    let endpoint = format!("ws://127.0.0.1:{port}");
    // A revive re-allocates the bridge's own server port too, and it MUST: the
    // old row's `harnessEndpoint` named a port belonging to a process that is
    // gone. Carrying it forward would publish a dead address as a live one, and a
    // viewer pointed at it would fail to connect — the same mistake reading
    // `endpoint` without a liveness check makes, written into the record instead.
    let harness_port = match alloc_harness_port(&params.provider_id, deps.alloc_port) {
        Ok(p) => p,
        Err(e) => {
            return Err(AcpResumeError::PortAllocFailed {
                name,
                detail: e.to_string(),
            })
        }
    };
    if deps.exe.as_os_str().is_empty() {
        return Err(AcpResumeError::ExeUnresolved {
            name,
            detail: "no executable path".to_string(),
        });
    }
    // The adapter's cwd = the row's cwd (faithful to the original session). A row with
    // no cwd falls back to "." (the adapter must have a cwd; the bridge resolves the
    // CC JSONL by encodeProjectPath(cwd), so this must match the create-time cwd).
    let cwd_str = params
        .cwd
        .clone()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| ".".to_string());
    let cwd = PathBuf::from(&cwd_str);

    let (bridge_cmd, bridge_args) = bridge_for(&params.provider_id, harness_port);
    // LOAD MODE: `--load-session <sessionId>` → the adapter boots via real `session/load`.
    let argv = build_adapter_argv(
        &deps.exe,
        &endpoint,
        &cwd,
        bridge_cmd,
        &bridge_args,
        Some(&params.session_id),
    );
    let log_path = acp_log_path(&deps.home, &name);
    // Get or mint the stable id for this ACP session. On resume the session UUID is
    // already known → mint_or_get returns the existing id (no-op if already present).
    // Mirrors the non-acp claude resume path.
    // Fail-soft: a mint error is warned but does not abort the resume.
    let acp_env = {
        let ids_path = crate::idstore::ids_path(&deps.paths.state_dir);
        match crate::idstore::mint_or_get(&ids_path, &params.session_id, Some(&name), deps.clock) {
            Ok(id) => vec![("QD_SESSION_ID".to_string(), id)],
            Err(detail) => {
                (deps.warn)(&AcpWarning::ResumeIdMintFailed { detail });
                // Explicitly clear the value so the adapter cannot inherit the caller's id.
                vec![("QD_SESSION_ID".to_string(), String::new())]
            }
        }
    };
    let spawned = match deps.spawner.spawn_detached(&argv, &acp_env, &cwd, &log_path) {
        Ok(s) => s,
        Err(e) => {
            return Err(AcpResumeError::SpawnFailed {
                name,
                detail: e.to_string(),
            })
        }
    };

    // Readiness: poll connect+status until the resident session is re-established. On
    // failure: group-kill the just-spawned adapter (no orphan).
    let conn = match connect_ready(&endpoint, READY_BUDGET) {
        Ok(c) => c,
        Err(detail) => {
            deps.spawner.kill(spawned.pid);
            return Err(AcpResumeError::NotReady {
                name,
                detail,
                log_path,
            });
        }
    };
    // WRONG-ADAPTER GUARD (NOT a bridge-fork / FM-R1 guard — honest scope, red-team #2.1):
    // confirm the resident we just connected to reports OUR sessionId. NOTE what this can
    // and CANNOT catch: `AcpHost::load_session` CACHES the requested id on Ok and the ACP
    // `session/load` reply carries NO sessionId, so on the SUCCESS path `status` echoes the
    // id we asked for → `established == requested` ALWAYS; this check therefore does NOT
    // detect a bridge-SIDE fork (the FM-R1 mirage). What it DOES catch: we connected to a
    // DIFFERENT resident — a stale/reused endpoint/port now serving another acp session
    // (a different cached id) — in which case we'd be about to bless the wrong adapter as
    // this row; refuse instead. The real FM-R1 faithfulness (same CC conversation) is
    // established out-of-band by Component-0 + the JSONL-continuation round-trip, not by
    // this runtime echo. (A production post-resume JSONL-continuation check is a disclosed
    // residual, red-team #2.2 — HELD.)
    let established = conn.status_session_id().ok().flatten().unwrap_or_default();
    if established != params.session_id {
        drop(conn);
        deps.spawner.kill(spawned.pid);
        return Err(AcpResumeError::WrongAdapter {
            name,
            established,
            requested: params.session_id.clone(),
        });
    }
    drop(conn); // the resident stays up; this was a short-lived readiness connection.

    // Rewrite the registry row: NEW adapter pid + NEW endpoint, SAME sessionId (m2
    // identity preserved), status live. The old dead-pid tombstone is consumed below.
    let now = deps.clock.now_ms();
    let entry = RegistryEntry {
        pid: Some(spawned.pid),
        session_id: Some(params.session_id.clone()),
        cwd: Some(cwd_str),
        started_at: Some(now),
        updated_at: Some(now),
        status: Some("idle".to_string()),
        name: Some(name.clone()),
        version: None,
        kind: None,
        entrypoint: None,
        backend: None,
        spawned_by: None,
        provider: Some(params.provider_id.clone()),
        endpoint: Some(endpoint.clone()),
        // A resumed healthy row carries NO degradation latch (tier is derived per verb).
        transport: None,
        // Child B (opencode D1), kept under Child D: a session's send-history
        // bit must SURVIVE a resume — it is a fact about the whole session's
        // life, not the old (now-dead) connection. Carry it forward from the
        // pre-resume row (if any) rather than resetting it, or the resumed row
        // carries a false "never sent" wire-history (no disposition branches on
        // it anymore — every loss refuses — but the record is durable truth;
        // see the R4-1 fn doc above and registry.rs's field doc).
        //
        // R4-1 (conformance round 4, confirmed real): the pre-resume row is
        // almost always TOMBSTONED, not live — `qd stop` tombstones by RENAMING
        // `<pid>.json` to `<pid>.json.tombstoned`, so a live-only `read_entry`
        // finds nothing for the ordinary stop→resume path (only a crash, which
        // leaves the live file in place with a dead pid, was actually covered).
        // Read the live row first (the crash-path shape), falling back to the
        // tombstoned row (the ordinary `qd stop` shape) when the live read misses.
        structured_send_issued: carry_forward_structured_send_issued(
            sessions_dir,
            params.current_pid,
        ),
        // acp/* is daemon-hosted with no second topology. Stamped for the same
        // reason the create stamps it, and the "byte-identical to a freshly
        // created row" property that used to argue for `None` now argues for
        // the token: the create writes it, so a revive that did not would make
        // a resumed session's row differ from a fresh one in the one field that
        // decides which lane drives it.
        hosting: Some(crate::lane::Mode::Daemon.hosting_token().to_string()),
        // The NEW bridge's server address (see the re-allocation note above) —
        // never the old row's, which named a dead port.
        harness_endpoint: harness_endpoint_for(&params.provider_id, harness_port),
    };
    if let Err(e) = registry::write_entry(sessions_dir, &entry) {
        deps.spawner.kill(spawned.pid);
        return Err(AcpResumeError::RowWriteFailed {
            name,
            detail: e.to_string(),
        });
    }

    // Consume the prior tombstone (`<old_pid>.json.tombstoned`) so no dangling tombstone
    // / double live-row survives (R-b). Best-effort: a missing tombstone is fine (a row
    // stopped a different way), and the new live row is already authoritative. Also drop
    // any stale resume-verify marker keyed by the OLD pid (cleanup).
    if let Some(old_pid) = params.current_pid.filter(|&p| p != 0) {
        let tomb = sessions_dir.join(format!("{old_pid}.json.tombstoned"));
        let _ = std::fs::remove_file(&tomb);
        let _ = std::fs::remove_file(resume_verify_marker_path(sessions_dir, old_pid));
    }

    // FINDING #2 PART 2 — drop a VERIFY-THE-BRIDGE marker: record the requested JSONL's
    // baseline (line count + the project dir's current session-file set) so the FIRST
    // post-resume wait can confirm the turn CONTINUED the SAME bridge JSONL (fork-on-load
    // detection) from PRIMARY source. Best-effort: a marker-write failure does not fail
    // the resume (the turn still works; we just lose the one-time verification).
    // A-OC.1: this is a claude-bridge fork-on-load check against the CC projects JSONL; it does
    // NOT apply to acp/opencode (opencode persists to its own store, no CC JSONL to baseline), so
    // skip it for non-claude bridges — otherwise the wait-side verify would always read
    // Unconfirmed and emit a misleading degraded-confidence warning on every opencode resume.
    if params.provider_id == "acp/claude-code" {
        let projects_dir = &deps.paths.projects_dir;
        let requested = crate::jsonl::find_jsonl_path(
            projects_dir,
            &params.session_id,
            params.cwd.as_deref(),
        );
        let baseline_lines = requested
            .as_ref()
            .map(|p| {
                std::fs::read_to_string(p)
                    .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        // The project dir's current *.jsonl basenames (the fork-detection baseline).
        let baseline_files: Vec<String> = params
            .cwd
            .as_deref()
            .map(|cwd| projects_dir.join(crate::jsonl::cwd_to_project_path(cwd)))
            .into_iter()
            .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                (n.ends_with(".jsonl") && !n.starts_with("agent-")).then_some(n)
            })
            .collect();
        let marker = ResumeVerifyMarker {
            session_id: params.session_id.clone(),
            cwd: params.cwd.clone(),
            baseline_lines,
            baseline_files,
        };
        let _ = write_resume_verify_marker(
            &resume_verify_marker_path(sessions_dir, spawned.pid),
            &marker,
        );
    }

    Ok(AcpResumeOutcome::Revived {
        name,
        pid: spawned.pid,
        endpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::RegistryEntry;

    // R4-1 (conformance round 4, confirmed real by source): `structured_send_issued`
    // was silently lost across the ORDINARY `qd stop` → `qd resume` cycle, because
    // `qd stop` tombstones a row by RENAMING `<pid>.json` to
    // `<pid>.json.tombstoned`, and the carry-forward read only ever consulted the
    // live path. These pin `carry_forward_structured_send_issued` directly against
    // real registry files (no process spawn needed — this is pure filesystem I/O),
    // covering both shapes it must handle.

    fn write_registry_row(
        sessions_dir: &std::path::Path,
        pid: i64,
        structured_send_issued: Option<bool>,
    ) {
        let entry = RegistryEntry {
            pid: Some(pid),
            session_id: Some("sess-r4-1".to_string()),
            provider: Some("acp/claude-code".to_string()),
            structured_send_issued,
            ..RegistryEntry::default()
        };
        crate::registry::write_entry(sessions_dir, &entry).unwrap();
    }

    // === The bridge's OWN server: pinned at spawn, recorded on the row. ===
    //
    // `opencode acp` starts a full opencode HTTP server and adapts ACP onto it.
    // Its listen port defaults to `0`, so before the pin qd spawned a real server
    // per session and had no way to name it afterwards — which is precisely the
    // state that made `qd attach` impossible on this lane. These pin the three
    // pure functions that end it. The spawn itself is covered live
    // (`tests/acp_opencode_live.rs`); what is checkable without a process is that
    // the argv carries the flag, that the row carries the address, and that
    // neither happens for a bridge with no server.

    /// A fake allocator handing out DISTINCT ports, so a test can tell the ws
    /// port from the harness port. A fixture that returned one port twice would
    /// pass while the two addresses silently collapsed onto each other.
    fn counting_alloc(next: &std::cell::Cell<u16>) -> impl Fn() -> std::io::Result<u16> + '_ {
        move || {
            let p = next.get();
            next.set(p + 1);
            Ok(p)
        }
    }

    #[test]
    fn opencode_bridge_argv_carries_the_pinned_port() {
        let (cmd, args) = bridge_for("acp/opencode", Some(41234));
        assert_eq!(cmd, Some("opencode"));
        assert_eq!(args, vec!["acp", "--port", "41234"]);
    }

    #[test]
    fn a_stdio_only_bridge_is_untouched_by_the_pin() {
        // BYTE-STABILITY, and it is the point of the `harness_server: None` arm:
        // `acp/claude-code` must spawn exactly the argv it spawned before this
        // existed, whether or not a port is offered. The `Some(41234)` case is
        // the one that would regress if `bridge_for` appended unconditionally.
        assert_eq!(bridge_for("acp/claude-code", None), (None, vec![]));
        assert_eq!(bridge_for("acp/claude-code", Some(41234)), (None, vec![]));
    }

    /// The pin must survive the `--bridge-arg` ENCODING, which is the one place
    /// it could silently be lost.
    ///
    /// `--port` is passed to the adapter as the VALUE of a `--bridge-arg`, and it
    /// looks exactly like an adapter flag. If `parse_adapter_args` treated a
    /// value beginning with `--` as the next flag rather than as the value it was
    /// taking, the port would land on the adapter (which does not accept it) or
    /// be dropped, and the bridge would go back to an ephemeral port — with
    /// everything still spawning, still working, and `qd attach` failing later
    /// against a recorded address nothing is bound to.
    ///
    /// So this walks the whole encode/decode: `bridge_for` → `build_adapter_argv`
    /// → `parse_adapter_args` → the exact `program args…` the adapter hands
    /// `AcpHost::spawn`.
    #[test]
    fn the_pinned_port_survives_the_bridge_arg_round_trip() {
        use super::super::residence::{build_adapter_argv, parse_adapter_args};

        let (cmd, args) = bridge_for("acp/opencode", Some(41234));
        let argv = build_adapter_argv(
            std::path::Path::new("/usr/bin/qd"),
            "ws://127.0.0.1:9000",
            std::path::Path::new("/work"),
            cmd,
            &args,
            None,
        );
        // `--port` and its value ride as bridge-arg VALUES, never as adapter flags.
        assert_eq!(
            argv,
            vec![
                "/usr/bin/qd",
                "acp-daemon",
                "--listen",
                "ws://127.0.0.1:9000",
                "--cwd",
                "/work",
                "--bridge-cmd",
                "opencode",
                "--bridge-arg",
                "acp",
                "--bridge-arg",
                "--port",
                "--bridge-arg",
                "41234",
            ]
        );
        // And the adapter decodes them back to the command it will spawn:
        // `opencode acp --port 41234`.
        let parsed = parse_adapter_args(&argv[2..]).expect("the adapter parses its own argv");
        assert_eq!(parsed.bridge_cmd, "opencode");
        assert_eq!(parsed.bridge_args, vec!["acp", "--port", "41234"]);
        assert_eq!(parsed.listen, "ws://127.0.0.1:9000");
    }

    #[test]
    fn only_a_bridge_with_a_server_gets_a_port_allocated() {
        let next = std::cell::Cell::new(41000);
        let alloc = counting_alloc(&next);
        // opencode: allocated, and it consumes a port.
        assert_eq!(alloc_harness_port("acp/opencode", &alloc).unwrap(), Some(41000));
        // claude-code: asked, nothing to allocate, and NO port consumed — an
        // allocator call here would bind a socket for a server that will never
        // exist.
        assert_eq!(alloc_harness_port("acp/claude-code", &alloc).unwrap(), None);
        assert_eq!(next.get(), 41001, "the no-server arm must not burn a port");
    }

    #[test]
    fn the_recorded_address_is_the_one_the_bridge_was_told_to_listen_on() {
        assert_eq!(
            harness_endpoint_for("acp/opencode", Some(41234)).as_deref(),
            Some("http://127.0.0.1:41234"),
        );
        // No server ⇒ no key on the row at all (skip-None), which is what keeps
        // every `acp/claude-code` row byte-stable.
        assert_eq!(harness_endpoint_for("acp/claude-code", Some(41234)), None);
        // And a server spec with no port is `None` rather than a URL with a hole
        // in it: the address must name a port something is actually bound to.
        assert_eq!(harness_endpoint_for("acp/opencode", None), None);
    }

    #[test]
    fn the_two_ports_are_different_servers_in_different_processes() {
        // The ws endpoint is qd's adapter; the harness endpoint is the bridge's
        // own server. They are allocated from the same allocator and must never
        // be assumed equal — the row records BOTH because a viewer needs the
        // second one and every verb needs the first.
        let next = std::cell::Cell::new(41000);
        let alloc = counting_alloc(&next);
        let ws = alloc().unwrap();
        let harness = alloc_harness_port("acp/opencode", &alloc).unwrap().unwrap();
        assert_ne!(ws, harness);
        assert_eq!(
            harness_endpoint_for("acp/opencode", Some(harness)).as_deref(),
            Some("http://127.0.0.1:41001")
        );
    }

    #[test]
    fn carries_forward_true_across_the_ordinary_stop_then_resume_tombstone_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path();
        write_registry_row(sessions_dir, 4242, Some(true));
        // `qd stop`'s tombstone mechanic: rename, don't leave the live file.
        crate::registry::tombstone(sessions_dir, 4242);
        assert!(
            crate::registry::read_entry(sessions_dir, 4242).is_none(),
            "the live file must be gone after tombstoning — this is the exact \
             condition that made the pre-fix read silently miss"
        );

        assert_eq!(
            carry_forward_structured_send_issued(sessions_dir, Some(4242)),
            Some(true),
            "the ordinary qd stop -> qd resume path must carry the bit forward"
        );
    }

    #[test]
    fn carries_forward_true_across_the_crash_path_live_row_still_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path();
        write_registry_row(sessions_dir, 4242, Some(true));
        // No tombstone — models a crash: the process died but its row was never
        // renamed. This shape already worked before the R4-1 fix; pinned here as
        // the non-regression control.
        assert_eq!(
            carry_forward_structured_send_issued(sessions_dir, Some(4242)),
            Some(true)
        );
    }

    #[test]
    fn a_genuinely_fresh_row_carries_forward_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path();
        write_registry_row(sessions_dir, 4242, None);
        crate::registry::tombstone(sessions_dir, 4242);
        assert_eq!(carry_forward_structured_send_issued(sessions_dir, Some(4242)), None);
    }

    #[test]
    fn no_row_at_all_carries_forward_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            carry_forward_structured_send_issued(tmp.path(), Some(9999)),
            None
        );
    }

    #[test]
    fn no_pid_carries_forward_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(carry_forward_structured_send_issued(tmp.path(), None), None);
    }


    // -------------------------------------------------------------------------
    // Child B (opencode D1), F1 fix: `drive_create_prompt`'s dispatch-to-marker
    // wiring. Red-team round 1 found that a create-time `--prompt` dispatched a
    // real structured send but the row was ALWAYS written with
    // `structured_send_issued: None` — making the session look pre-send forever,
    // so its first transport loss would incorrectly auto-degrade to the floor
    // instead of refusing (conditions 3/4). These pin the fix directly: a fake
    // `AcpClient` stands in for a real acp connection, so the exact
    // dispatch-confirmed-on-the-wire → `true` mapping is testable without a
    // socket.
    // -------------------------------------------------------------------------

    /// A fake `AcpClient` whose `prompt` invokes `on_dispatched` before returning
    /// `Ok` (mirrors `AcpConnection::prompt`'s real dispatch-timing contract —
    /// the SAME fixture shape as `tests/acp_fallback.rs`'s `FakeDispatchingClient`).
    struct FakeDispatchingClient;
    impl crate::provider::acp::AcpClient for FakeDispatchingClient {
        fn initialize(
            &self,
        ) -> Result<crate::provider::acp::InitializeResult, crate::provider::acp::AcpError>
        {
            unimplemented!()
        }
        fn new_session(&self, _cwd: &str) -> Result<String, crate::provider::acp::AcpError> {
            unimplemented!()
        }
        fn prompt(
            &self,
            _session: &str,
            _text: &str,
            _from: &str,
            on_dispatched: &dyn Fn(),
        ) -> Result<String, crate::provider::acp::AcpError> {
            on_dispatched();
            Ok("turn-1".to_string())
        }
        fn cancel(&self, _session: &str) -> Result<(), crate::provider::acp::AcpError> {
            unimplemented!()
        }
        fn next_update(
            &self,
            _timeout: std::time::Duration,
        ) -> Result<
            Option<crate::provider::acp::AcpEvent>,
            crate::provider::acp::AcpError,
        > {
            unimplemented!()
        }
    }

    /// A fake `AcpClient` whose `prompt` ALWAYS fails WITHOUT ever invoking
    /// `on_dispatched` — models a genuine pre-send failure (never reached the
    /// wire), the negative control for the dispatched case below.
    struct FakeNonDispatchingClient;
    impl crate::provider::acp::AcpClient for FakeNonDispatchingClient {
        fn initialize(
            &self,
        ) -> Result<crate::provider::acp::InitializeResult, crate::provider::acp::AcpError>
        {
            unimplemented!()
        }
        fn new_session(&self, _cwd: &str) -> Result<String, crate::provider::acp::AcpError> {
            unimplemented!()
        }
        fn prompt(
            &self,
            _session: &str,
            _text: &str,
            _from: &str,
            _on_dispatched: &dyn Fn(),
        ) -> Result<String, crate::provider::acp::AcpError> {
            Err(crate::provider::acp::AcpError::Closed)
        }
        fn cancel(&self, _session: &str) -> Result<(), crate::provider::acp::AcpError> {
            unimplemented!()
        }
        fn next_update(
            &self,
            _timeout: std::time::Duration,
        ) -> Result<
            Option<crate::provider::acp::AcpEvent>,
            crate::provider::acp::AcpError,
        > {
            unimplemented!()
        }
    }

    #[test]
    fn create_time_prompt_that_dispatches_reports_true() {
        let client = FakeDispatchingClient;
        let dispatched = drive_create_prompt(&client, "sess-1", Some("hello"), "my-session", &|_| {});
        assert!(
            dispatched,
            "a create-time prompt whose bytes reach the wire MUST report dispatched=true, \
             so the row's structured_send_issued becomes Some(true) — the F1 regression"
        );
    }

    #[test]
    fn create_time_prompt_that_never_dispatches_reports_false() {
        let client = FakeNonDispatchingClient;
        let dispatched = drive_create_prompt(&client, "sess-1", Some("hello"), "my-session", &|_| {});
        assert!(
            !dispatched,
            "a prompt that failed before reaching the wire must report dispatched=false \
             (structured_send_issued stays None — a genuinely pre-send session)"
        );
    }

    #[test]
    fn no_create_time_prompt_reports_false() {
        let client = FakeDispatchingClient;
        // None and empty-string both mean "no create-time prompt" (the same
        // `.filter(|s| !s.is_empty())` gate the production call site uses).
        assert!(!drive_create_prompt(&client, "sess-1", None, "my-session", &|_| {}));
        assert!(!drive_create_prompt(&client, "sess-1", Some(""), "my-session", &|_| {}));
    }


    // -------------------------------------------------------------------------
    // R5-1 ORDER GUARD (red-team round 5, Child B era — still binding).
    //
    // `resume_acp` must run the SAME two raw-registry preflights the non-acp
    // resume path runs (Pete feedback #6: `id_collision_refusal` +
    // `alive_pid_for_id`) AFTER the Case-1 already-alive gate and BEFORE the
    // concurrent-resume claim. The acp arm dispatches before `run`'s own
    // preflights, so without in-body equivalents a `qd resume` of an acp session
    // while ANOTHER live process holds the same session_id/transcript
    // (historically Child B's floor companion; today a leftover dev companion row
    // or a manually-launched `claude --resume`) revives a SECOND live writer onto
    // one CC transcript.
    //
    // This REPLACES the source-text guard the pre-split verb carried, which
    // grepped its own function body with `include_str!` for two call-site strings.
    // That was the only tool available while the sequence lived in a verb body,
    // and it could not survive the move — but it was also weaker: it could not
    // tell a call from a comment mentioning one, and it never observed the claim
    // at all. These drive the REAL function with recording fakes and assert the
    // OBSERVED order.
    //
    // MUTATION EVIDENCE: deleting either preflight from `resume_acp` reds
    // `preflights_refuse_a_live_other_holder`; moving them above the Case-1 gate
    // reds `alive_gate_short_circuits_before_the_preflights`; moving them below
    // the claim reds `preflights_run_before_the_claim_is_taken`.
    // -------------------------------------------------------------------------

    /// A spawner that must never be reached in these tests — every one of them is
    /// supposed to refuse before anything is launched.
    struct NeverSpawner;
    impl DaemonSpawner for NeverSpawner {
        fn spawn_detached(
            &self,
            _argv: &[String],
            _env: &[(String, String)],
            _cwd: &std::path::Path,
            _log: &std::path::Path,
        ) -> std::io::Result<crate::create_daemon::SpawnedDaemon> {
            unreachable!("no revive may spawn: the gates above it must refuse first")
        }
        fn kill(&self, _pid: i64) {
            unreachable!("nothing was spawned, so nothing can be killed")
        }
    }

    /// Deps whose port allocator FAILS and whose `alloc_port` call is RECORDED.
    /// A failing allocator is the cheapest way to stop the sequence exactly one
    /// step past the claim, which is what makes "the claim was taken" observable
    /// without spawning anything.
    fn ordering_deps<'a>(
        paths: &'a QdPaths,
        clock: &'a dyn Clock,
        spawner: &'a dyn DaemonSpawner,
        alloc: &'a PortAllocator<'a>,
        probe: &'a CmdlineProbe<'a>,
        warn: &'a dyn Fn(&AcpWarning),
    ) -> AcpDaemonDeps<'a> {
        AcpDaemonDeps {
            exe: PathBuf::from("/nonexistent/qd"),
            home: paths.home.clone(),
            paths,
            clock,
            spawner,
            alloc_port: alloc,
            cmdline_probe: probe,
            warn,
        }
    }

    fn ordering_params(session_id: &str, pid: Option<i64>, endpoint: Option<&str>) -> AcpResumeParams {
        AcpResumeParams {
            name: "wk".to_string(),
            session_id: session_id.to_string(),
            provider_id: "acp/claude-code".to_string(),
            cwd: None,
            has_jsonl: true,
            current_pid: pid,
            current_endpoint: endpoint.map(str::to_string),
        }
    }

    /// The ORDER, in one test — because it is ONE contract, and because it
    /// replaces exactly one source-text guard. Four phases, each of which reds on
    /// a different mis-ordering; the phase comments say which.
    #[test]
    fn preflights_run_after_the_alive_gate_and_before_the_claim() {
        let clock = crate::effects::FixedClock(0);
        let spawner = NeverSpawner;
        let warn = |_: &AcpWarning| {};
        let me = std::process::id() as i64;

        // --- Phase 1: Case 1 WINS. A pid-alive row whose cmdline carries the
        // recorded `--listen` is a SUCCESS no-op, so the preflights never run and
        // a second live holder of the id (planted here) does NOT turn the ratified
        // already-running no-op into a refusal. REDS if the preflights move ABOVE
        // the gate.
        {
            let tmp = tempfile::tempdir().unwrap();
            let paths = QdPaths::from_home(tmp.path());
            std::fs::create_dir_all(&paths.sessions_dir).unwrap();
            let endpoint = "ws://127.0.0.1:18951";
            write_row(&paths.sessions_dir, me, "sid-alive", Some(endpoint));
            let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
            write_row(&paths.sessions_dir, child.id() as i64, "sid-alive", Some(endpoint));

            let alloc = || -> std::io::Result<u16> {
                unreachable!("an already-alive row must not reach the revive")
            };
            let probe = |_pid: i64| Some(format!("qd acp-daemon --listen {endpoint}"));
            let deps = ordering_deps(&paths, &clock, &spawner, &alloc, &probe, &warn);

            let got = resume_acp(&deps, &ordering_params("sid-alive", Some(me), Some(endpoint)));
            let _ = child.kill();
            let _ = child.wait();
            assert_eq!(
                got.expect("already-alive is a success no-op"),
                AcpResumeOutcome::AlreadyRunning {
                    name: "wk".to_string()
                }
            );
        }

        // --- Phase 2: once Case 1 MISSES, the preflights run and refuse a live
        // OTHER holder of the id — before anything is spawned or allocated. REDS
        // if `alive_pid_for_id` is deleted from the sequence.
        {
            let tmp = tempfile::tempdir().unwrap();
            let paths = QdPaths::from_home(tmp.path());
            std::fs::create_dir_all(&paths.sessions_dir).unwrap();
            write_row(&paths.sessions_dir, me, "sid-held", Some("ws://127.0.0.1:1"));

            let alloc =
                || -> std::io::Result<u16> { unreachable!("a refused resume must not allocate") };
            let probe = |_pid: i64| None; // no cmdline ⇒ Case 1 misses
            let deps = ordering_deps(&paths, &clock, &spawner, &alloc, &probe, &warn);

            // current_pid None ⇒ this call is NOT the live row, so `me` is an OTHER holder.
            let err = resume_acp(&deps, &ordering_params("sid-held", None, None)).unwrap_err();
            assert!(
                matches!(err, AcpResumeError::AlreadyAliveElsewhere { pid, .. } if pid == me),
                "a live other holder must refuse: {err}"
            );
        }

        // --- Phase 3: ≥2 live holders is the loud collision arm, and it too lands
        // before any spawn. REDS if `id_collision_refusal` is deleted.
        {
            let tmp = tempfile::tempdir().unwrap();
            let paths = QdPaths::from_home(tmp.path());
            std::fs::create_dir_all(&paths.sessions_dir).unwrap();
            let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
            write_row(&paths.sessions_dir, me, "sid-dup", None);
            write_row(&paths.sessions_dir, child.id() as i64, "sid-dup", None);

            let alloc =
                || -> std::io::Result<u16> { unreachable!("a refused resume must not allocate") };
            let probe = |_pid: i64| None;
            let deps = ordering_deps(&paths, &clock, &spawner, &alloc, &probe, &warn);

            let got = resume_acp(&deps, &ordering_params("sid-dup", None, None));
            let _ = child.kill();
            let _ = child.wait();
            let err = got.unwrap_err();
            assert!(
                matches!(&err, AcpResumeError::IdCollision(r) if r.rows.len() == 2),
                "two live holders must refuse loudly: {err}"
            );
        }

        // --- Phase 4: with a clean registry the sequence gets past BOTH preflights,
        // takes the claim, and only then starts the revive proper — whose first step
        // is the allocator. A failing allocator stops it exactly one step past the
        // claim, which is what makes "the claim was taken" observable without
        // spawning. REDS if the preflights move BELOW the claim (phase 2's
        // `unreachable!` allocator fires instead).
        {
            let tmp = tempfile::tempdir().unwrap();
            let paths = QdPaths::from_home(tmp.path());
            std::fs::create_dir_all(&paths.sessions_dir).unwrap();

            let allocated = std::cell::Cell::new(0u32);
            let alloc = || -> std::io::Result<u16> {
                allocated.set(allocated.get() + 1);
                Err(std::io::Error::other("no ports for you"))
            };
            let probe = |_pid: i64| None;
            let deps = ordering_deps(&paths, &clock, &spawner, &alloc, &probe, &warn);

            let err = resume_acp(&deps, &ordering_params("sid-clean", None, None)).unwrap_err();
            assert!(
                matches!(err, AcpResumeError::PortAllocFailed { .. }),
                "a clean registry must get past both preflights and the claim: {err}"
            );
            assert_eq!(
                allocated.get(),
                1,
                "the revive proper runs exactly once, after the claim"
            );
            // The claim file exists ⇒ the claim really was taken before the allocator.
            let entries: Vec<String> = std::fs::read_dir(&paths.sessions_dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            assert!(
                entries.iter().any(|n| n.contains("sid-clean")),
                "the resume claim must have been taken before the revive: {entries:?}"
            );
        }
    }

    /// A live registry row for `session_id`, keyed by `pid`, optionally carrying
    /// an endpoint. Distinct from `write_registry_row` above, which pins the
    /// R4-1 carry-forward bit and always uses one fixed id.
    fn write_row(
        sessions_dir: &std::path::Path,
        pid: i64,
        session_id: &str,
        endpoint: Option<&str>,
    ) {
        let entry = RegistryEntry {
            pid: Some(pid),
            session_id: Some(session_id.to_string()),
            name: Some("wk".to_string()),
            provider: Some("acp/claude-code".to_string()),
            status: Some("idle".to_string()),
            endpoint: endpoint.map(str::to_string),
            ..RegistryEntry::default()
        };
        crate::registry::write_entry(sessions_dir, &entry).unwrap();
    }
}
