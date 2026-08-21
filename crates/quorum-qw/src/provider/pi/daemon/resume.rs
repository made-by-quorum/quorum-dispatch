//! `qd resume` for pi (daemon-residence) sessions — the WS-A.2 revive core.
//!
//! The pi analog of [`crate::provider::codex::resume`] and
//! [`crate::provider::acp::resume`]: a stopped/dead pi row revives to DRIVABLE
//! (there is no interactive attach — the resident is a daemon, not a pane). The
//! choreography is the SAME one create runs, with `load_session` set:
//! name-claim → spawn `pi-daemon --load-session <id>` DETACHED → `connect_ready`
//! → read the birth-id → write the NEW row (new pid + endpoint, SAME session id).
//! All of that already lives in [`super::create::create_pi_session`]; this module
//! is the resume DECISION around it — the resumability gate, the already-alive
//! no-op, the identity mint — split out of the `qd resume` verb body so the
//! printing lives in the binary and the choreography lives here.
//!
//! ── THE LOAD-BEARING GATE ───────────────────────────────────────────────────
//! An ALREADY-ALIVE resident ([`super::create::pi_daemon_is_alive`]: pid alive ∧
//! the live cmdline is OUR pi-daemon carrying the recorded `--listen <endpoint>`)
//! is a clean SUCCESS no-op. This is the double-spawn guard, not a nicety: a
//! fresh `create_pi_session` would claim the (now-free) name and spawn a DUPLICATE
//! resident against the same durable session. The resident OUTLIVES this call
//! (residence holds again).
//!
//! ── WHY [`PiResumeError`]'s `Display` CARRIES THE `qd resume:` PREFIX ────────
//! [`crate::provider::codex::resume::ResumeError`]'s `Display` is UNPREFIXED and
//! its verb stamps `qd resume: "<name>": ` around it, because every codex arm has
//! that one shape. pi's arms do NOT: the resumability refusal reads
//! `qd resume: session "<name>" has no pi session id — nothing to resume.`, which
//! is not `qd resume: "<name>": <body>`. Byte-parity with the pre-split verb is
//! the hard constraint, so the whole line lives in the match here — the
//! [`crate::create::NewError`] shape — and the verb is a bare `eprintln!("{e}")`.
//! Splitting the difference (some arms prefixed here, some stamped there) would
//! put the user-facing text in two places, which is the thing this split exists
//! to stop.

use std::path::PathBuf;

use crate::create_daemon::{CmdlineProbe, DaemonSpawner};
use crate::effects::Clock;

use super::create::{
    create_pi_session, pi_daemon_is_alive, PiCreateDeps, PiCreateError, PiCreateParams,
};

/// What [`resume_pi`] decided + did. Both arms are SUCCESS (exit 0); the verb
/// maps each to its agent-facing line.
#[derive(Debug, Clone, PartialEq)]
pub enum PiResumeOutcome {
    /// The resident was already alive (pid alive ∧ our-pi-daemon cmdline carrying
    /// the recorded endpoint) — a clean no-op. NO second resident was spawned.
    /// The load-bearing double-spawn guard; see the module docs.
    AlreadyRunning { name: String },
    /// The resident was dead (or cold) and was REVIVED in LOAD mode on the SAME
    /// durable session id. Carries the NEW pid + endpoint (a new row was written).
    Revived {
        name: String,
        pid: i64,
        endpoint: String,
    },
}

/// Why a pi resume failed. Every variant leaves NO new resident and NO row change
/// beyond what `create_pi_session` itself reaps on its own failure paths.
///
/// `Display` emits the COMPLETE stderr line, `qd resume:` prefix included — see
/// the module docs for why this diverges from the codex arm's unprefixed form.
#[derive(Debug)]
pub enum PiResumeError {
    /// Resumability gate: pi's `session_id` (the `get_state` birth-id) IS its
    /// durable identity — with none there is nothing to load.
    NoSessionId { name: String },
    /// The stable id could not be minted/read from the idstore. Fail-closed:
    /// NOTHING was respawned (a resident that cannot self-identify would inherit
    /// the commissioner's `QD_SESSION_ID`).
    IdMintFailed { name: String, detail: String },
    /// The create choreography itself failed (claim lost, port, spawn/readiness,
    /// row write). Carries the typed [`PiCreateError`] verbatim — it already
    /// reaped whatever it spawned.
    Create {
        name: String,
        source: PiCreateError,
    },
}

impl PiResumeError {
    /// Process exit code. The gate failures are exit 1 (the verb precedent); the
    /// create passthrough defers to [`PiCreateError::exit_code`] so the
    /// choreography keeps owning its own mapping.
    pub fn exit_code(&self) -> i32 {
        match self {
            PiResumeError::NoSessionId { .. } | PiResumeError::IdMintFailed { .. } => 1,
            PiResumeError::Create { source, .. } => source.exit_code(),
        }
    }
}

impl std::fmt::Display for PiResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PiResumeError::NoSessionId { name } => write!(
                f,
                "qd resume: session \"{name}\" has no pi session id — nothing to resume."
            ),
            PiResumeError::IdMintFailed { name, detail } => {
                write!(f, "qd resume: \"{name}\": could not resolve stable id: {detail}")
            }
            PiResumeError::Create { name, source } => {
                write!(f, "qd resume: \"{name}\": {source}")
            }
        }
    }
}

impl std::error::Error for PiResumeError {}

/// Injected effects + resolved paths for one [`resume_pi`] call. Nothing is
/// constructed in here: the verb resolves HOME, the self-exe and the env-derived
/// pi knobs and hands them in, exactly as it does for
/// [`super::create::PiCreateDeps`] on the create path.
pub struct PiResumeDeps<'a> {
    /// The self-exe path the resident is spawned from (`<exe> pi-daemon …`).
    /// Resolved by the verb (`std::env::current_exe`), which owns that failure —
    /// it is a deps-construction error, not a resume decision.
    pub exe: PathBuf,
    /// The pinned pi binary (`QD_PI_BIN`; not on PATH) passed to the resident.
    pub pi_bin: Option<String>,
    /// `PI_CODING_AGENT_SESSION_DIR` passed to the resident (sessions root).
    pub session_dir: Option<String>,
    /// The sessions dir the NEW row is written into.
    pub sessions_dir: PathBuf,
    /// The claims dir for the O_EXCL name-claim the respawn takes.
    pub claims_dir: PathBuf,
    /// The log dir root for the revived resident's stdout/stderr.
    pub log_dir: PathBuf,
    /// The detached-spawn seam (real spawner in prod; a fake in units).
    pub spawner: &'a dyn DaemonSpawner,
    /// Clock — the row stamps AND the idstore mint line.
    pub clock: &'a dyn Clock,
    /// The idstore path the stable id is `mint_or_get`-keyed in.
    pub ids_path: PathBuf,
    /// Liveness probe for the already-alive gate (`pid -> alive`). Production:
    /// [`crate::effects::is_pid_alive`]; units inject a closure.
    pub is_pid_alive: &'a dyn Fn(i64) -> bool,
    /// The connectionless `pid -> cmdline` probe the already-alive gate consults
    /// to VERIFY a live recorded pid is OUR pi resident. Under exact-pid-reuse a
    /// live recorded pid may be an UNRELATED process; without this the gate would
    /// falsely report AlreadyRunning and the session would never come back.
    /// Production: [`crate::create_daemon::real_cmdline_probe`].
    pub cmdline_probe: &'a CmdlineProbe<'a>,
}

/// The resume input — the resolved row's identity fields, supplied by the verb
/// from the `Session` plus the re-read registry row.
pub struct PiResumeParams {
    /// The session name (messages, the name-claim, the rewritten row).
    pub name: String,
    /// pi's durable birth-id — the thing LOAD mode boots against. Empty ⇒ the
    /// resumability gate refuses.
    pub session_id: String,
    /// The row's recorded cwd. Empty/absent ⇒ `"."` (the pre-split verb's
    /// fallback, preserved).
    pub cwd: Option<String>,
    /// The row's CURRENT recorded resident pid (already-alive gate input).
    pub current_pid: Option<i64>,
    /// The row's CURRENT recorded endpoint. It is NOT on the `Session` surface —
    /// the verb re-reads it off the row by pid, mirroring the codex/ACP arms.
    /// Alive requires BOTH a live pid AND an endpoint.
    pub current_endpoint: Option<String>,
}

/// Resume a pi (daemon-hosted) session. See the module docs for the case table.
/// NO attach is ever invoked on any path.
pub fn resume_pi(
    deps: &PiResumeDeps<'_>,
    params: &PiResumeParams,
) -> Result<PiResumeOutcome, PiResumeError> {
    let name = params.name.clone();

    // Resumability gate: pi's session_id (the get_state birth-id) IS its durable
    // identity — with none there is nothing to load.
    if params.session_id.is_empty() {
        return Err(PiResumeError::NoSessionId { name });
    }

    // ALREADY ALIVE → clean no-op, NO second resident. pid-alive ∧ identity (the cmdline
    // carries the recorded `--listen <endpoint>`), the double-spawn guard.
    if pi_daemon_is_alive(
        params.current_pid.unwrap_or(0),
        params.current_endpoint.as_deref(),
        deps.is_pid_alive,
        deps.cmdline_probe,
    ) {
        return Ok(PiResumeOutcome::AlreadyRunning { name });
    }

    // WS-A.2 identity parity on RESUME (mirrors resume_daemon.rs ~647/802): pi's
    // durable id is KNOWN here (session.session_id, the birth-id preserved across
    // load mode), so `mint_or_get` keys it directly — returning the id minted at
    // create (the SAME id across every resume; lazy-mints for a pre-stable-id
    // session). Injected as `QD_SESSION_ID` so the resumed resident self-identifies
    // rather than inheriting the commissioner's env. `bind` inside create_pi_session
    // is then idempotent (birth-id already maps to this id). Fail-closed on a mint
    // error (nothing respawned).
    let qd_session_id = match crate::idstore::mint_or_get(
        &deps.ids_path,
        &params.session_id,
        Some(&name),
        deps.clock,
    ) {
        Ok(id) => Some(id),
        Err(detail) => return Err(PiResumeError::IdMintFailed { name, detail }),
    };

    // REVIVE: re-spawn the resident in LOAD mode via the SAME create choreography with
    // load_session set (name-claim → spawn `pi-daemon --load-session <id>` DETACHED →
    // connect_ready → read birth-id → write the NEW row).
    let now_ms = || deps.clock.now_ms();
    let cwd = PathBuf::from(
        params
            .cwd
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| ".".to_string()),
    );

    let create_deps = PiCreateDeps {
        exe: deps.exe.clone(),
        pi_bin: deps.pi_bin.clone(),
        session_dir: deps.session_dir.clone(),
        sessions_dir: deps.sessions_dir.clone(),
        claims_dir: deps.claims_dir.clone(),
        log_dir: deps.log_dir.clone(),
        spawner: deps.spawner,
        now_ms: &now_ms,
        ids_path: deps.ids_path.clone(),
    };
    let create_params = PiCreateParams {
        name: name.clone(),
        cwd,
        load_session: Some(params.session_id.clone()),
        qd_session_id,
    };
    match create_pi_session(&create_deps, &create_params) {
        Ok(out) => Ok(PiResumeOutcome::Revived {
            name: out.name,
            pid: out.pid,
            endpoint: out.endpoint,
        }),
        Err(source) => Err(PiResumeError::Create { name, source }),
    }
}
