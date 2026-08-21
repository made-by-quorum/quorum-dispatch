//! `provider::codex::pty::pane` — the MUX-PANE create + revive choreography for
//! a codex TUI (`qd start --provider codex --interactive`, and the cold arms of
//! `qd resume` / `qd attach` / `qd send`).
//!
//! The pane twin of [`crate::provider::codex::app_server::resume`] (which is the
//! DAEMON lane). Split out of
//! the `qd` verb bodies for the [`crate::create::run_new`] reason: the
//! choreography is a library concern, the printing and the CLI verb attribution
//! are not. Nothing here prints and nothing here exits.
//!
//! ── WHY THIS REUSES THE CLAUDE CREATE PIPELINE ──────────────────────────────
//! [`crate::create::run_new`] is provider-generic already: it builds its launch
//! command through `provider.launch_plan` and drives an injected
//! [`crate::create::BootWaiter`], so hosting a codex TUI needs no new pipeline,
//! only a different argv. The atomic name claim, the scan-under-claim, the zmx
//! preflight, the I6 attachability verify, the socket-dir-split detection and the
//! stale-ended-pane reap are therefore the SAME code claude sessions have been
//! hardened on.
//!
//! ── IDENTITY: THE ONE THING `resume_thread` DECIDES ─────────────────────────
//! `resume_thread` is what separates the two callers, and it decides identity:
//!   - `None` (fresh start) — argv is a bare `codex`, and the row is written with
//!     NO `sessionId`; the gather step binds one later
//!     (`join::backfill_codex_thread_ids`).
//!   - `Some(id)` (revive) — argv is `codex resume <id>` (verified against the
//!     codex CLI: a positional session UUID on the `resume` subcommand bypasses
//!     the picker and reopens that thread interactively), and the row is written
//!     WITH that id. A revived session is identified from birth: we are not
//!     discovering identity, we are carrying it forward, so no backfill and no
//!     window in which the session cannot be addressed.
//!
//! THE ONE THING THE PIPELINE CANNOT DO FOR US is the registry row. claude writes
//! its own (the create path merely waits for it to appear); the codex TUI knows
//! nothing about qd, so this writes it.
//!
//! ── THE VERB IS NOT A PARAMETER ─────────────────────────────────────────────
//! The pre-split functions threaded a `verb: &str` through every `eprintln!` so
//! the same code could say `qd start:` / `qd resume:` / `qd attach:` / `qd send:`
//! depending on who called. That is pure CLI attribution — it names the command
//! the user typed, which a library has no business knowing — so it does NOT cross
//! into [`CodexTuiError`]. The variants carry the FACTS; the binary stamps the
//! verb onto the `Display` body at print time. The one exception is
//! [`CodexTuiError::Create`], whose inner [`crate::create::NewError`] carries its
//! own attribution already (`qd start: …` / `ERROR: …`) and is therefore printed
//! verbatim — exactly as the pre-split verb printed it, from every caller.

use std::path::PathBuf;

use crate::create::{run_new, NewDeps, NewError, NewParams, OkBootWaiter};
use crate::lanes::ReviveHandle;
use crate::launch::RenderMode;
use crate::provider::pane::PaneDeps;
use crate::provider::Hosting;
use crate::registry::{self, RegistryEntry};

/// Params for one codex-TUI create.
pub struct CodexTuiParams {
    /// The zmx session name (also the row's `name`).
    pub name: String,
    /// Working dir for the pane. The CLI resolves the default.
    pub cwd: PathBuf,
    /// punch item 7: the resolved render mode, a VALUE here — the `--alt-screen`
    /// / `--inline` clap parsing and the `render-default` config read both happen
    /// in the binary, exactly as [`NewParams::render`] receives it.
    pub render: RenderMode,
    /// `None` = fresh start (bare `codex`, row with no `sessionId`);
    /// `Some(id)` = revive (`codex resume <id>`, row identified from birth). See
    /// the module docs.
    pub resume_thread: Option<String>,
}

/// Params for one codex-TUI revive.
pub struct CodexReviveParams {
    /// The session name, ALREADY through [`revive_preconditions`].
    pub name: String,
    /// The row's recorded `session_id` — the thread `codex resume <id>` reopens.
    pub session_id: String,
    /// The revive cwd. The binary resolves the recorded-cwd-or-current-dir
    /// fallback (process cwd is a CLI fact, not a library one).
    pub cwd: PathBuf,
    /// The resolved render mode (see [`CodexTuiParams::render`]).
    pub render: RenderMode,
    /// The OLD row's pid, whose tombstone is consumed on success so one session
    /// never leaves two rows behind (the `run_acp_resume` precedent).
    pub old_pid: Option<i64>,
}

/// What [`create_codex_tui`] produced. `thread_id` is `None` on a fresh start (the
/// gather step binds it later) and `Some` on a revive (carried forward).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexTuiOutcome {
    pub name: String,
    pub zmx_name: String,
    pub socket_dir: PathBuf,
    pub thread_id: Option<String>,
    /// The STABLE qd id minted for this session, plumbed out so a caller that did
    /// not mint it can still report it (`qd start --json`'s `qdId`, and
    /// [`crate::contract::SessionHandle::qd_id`]). The mint happens inside this
    /// function, so before this field the only way to learn the id was to be the
    /// minter.
    pub qd_session_id: String,
}

/// Why a codex-TUI create or revive failed.
///
/// `Display` emits the BODY only — no `qd <verb>:` prefix — because the verb is
/// the caller's, not this lane's (see the module docs). [`CodexTuiError::Create`]
/// is the deliberate exception: it wraps [`NewError`], which is already fully
/// attributed, and is printed verbatim.
#[derive(Debug)]
pub enum CodexTuiError {
    /// Revive gate: the row has no name, so there is nothing to revive it UNDER
    /// (the pane is keyed by name). Nothing was created.
    NoName,
    /// Revive gate: the row has no `session_id`. It was never used, so codex
    /// never opened a thread — there is no conversation to reopen. Refusing here
    /// is the point: launching a bare `codex` would silently hand back a
    /// DIFFERENT session under the same name.
    NeverUsed { name: String },
    /// The stable id could not be minted. Fail-closed, like the claude lane —
    /// nothing was created (the mint runs before any launch).
    IdMintFailed { detail: String },
    /// The shared create pipeline refused or failed: claim lost, live name,
    /// preflight, launch, I6 verify. Carries [`NewError`] verbatim — it already
    /// reaped or left exactly what it says it did.
    Create(NewError),
    /// The pane booted and passed I6, but had vanished from the socket dir by the
    /// time we listed it to key the row by its pid. NOTHING is tracked; retrying
    /// is safe.
    PaneVanished { name: String, canonical: PathBuf },
    /// The pane is RUNNING but its registry row could not be written, so qd
    /// cannot track it. Deliberately not a reap: the session is real and the user
    /// can still reach it by name.
    RowWriteFailed { name: String, detail: String },
}

impl CodexTuiError {
    /// Process exit code. Every gate/row failure is exit 1 (the verb precedent);
    /// the create passthrough defers to [`NewError::exit_code`] so the shared
    /// pipeline keeps owning its own mapping.
    pub fn exit_code(&self) -> i32 {
        match self {
            CodexTuiError::Create(e) => e.exit_code(),
            _ => 1,
        }
    }

    /// Whether this variant's `Display` already carries its own `qd <verb>:`
    /// attribution and must therefore be printed VERBATIM, without the caller
    /// stamping a verb on it. True only for [`CodexTuiError::Create`]; see the
    /// module docs.
    pub fn is_self_attributed(&self) -> bool {
        matches!(self, CodexTuiError::Create(_))
    }
}

impl std::fmt::Display for CodexTuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexTuiError::NoName => write!(
                f,
                "this codex session has no name, so there is nothing to revive it \
                 under. Start a fresh one with \"qd start <name> --provider codex --interactive\"."
            ),
            CodexTuiError::NeverUsed { name } => write!(
                f,
                "\"{name}\" was never used, so codex never opened a thread for it — \
                 there is no conversation to resume. Start a fresh session with \
                 \"qd start {name} --provider codex --interactive\"."
            ),
            CodexTuiError::IdMintFailed { detail } => write!(
                f,
                "could not mint a stable session id: {detail}. Nothing was created."
            ),
            CodexTuiError::Create(e) => write!(f, "{e}"),
            CodexTuiError::PaneVanished { name, canonical } => write!(
                f,
                "codex session \"{name}\" booted but its pane vanished from {} before \
                 the registry row could be written. Nothing is tracked; retrying is safe.",
                canonical.display()
            ),
            CodexTuiError::RowWriteFailed { name, detail } => write!(
                f,
                "codex session \"{name}\" is running but its registry row could not be \
                 written ({detail}), so qd cannot track it. Attach with \"qd attach {name}\" or stop \
                 the pane and retry."
            ),
        }
    }
}

impl std::error::Error for CodexTuiError {}

/// codex-interactive, use case 2: the mux-pane name of a HUMAN VIEWER opened on
/// a session (`qd attach` on a live daemon row spawns `codex --remote <endpoint>
/// …` into a pane of this name).
///
/// Distinct from the session's own name because a viewer is NOT the session — it
/// is a second process looking at it, with its own lifetime.
///
/// `.view` rather than something unmintable: qrmux restricts pane names to
/// `a-zA-Z0-9_-.`, so there is no separator available that a session name could
/// not also contain. A user COULD therefore have a real session literally named
/// `foo.view`, and reusing a pane on name alone would attach them to that instead
/// of a viewer on `foo`. So reuse is gated on the pane's COMMAND matching our
/// viewer argv, never on the name — see `verbs::lifecycle::attach_codex_viewer`.
pub fn viewer_pane_name(session_name: &str) -> String {
    format!("{session_name}.view")
}

/// The revive preconditions — PURE, and deliberately separate from
/// [`revive_codex_tui`] so a caller can refuse before it resolves HOME, the mux
/// backend, the socket dirs or the ids store. The pre-split verb checked both of
/// these first for exactly that reason, and the order is user-visible (a nameless
/// row with no HOME set must still say "no name"), so it is preserved here rather
/// than folded into the core's opening lines.
///
/// [`revive_codex_tui`] calls this itself as well — it is idempotent and pure, so
/// the core stays complete for callers that skip the early gate.
pub fn revive_preconditions(
    name: Option<&str>,
    session_id: &str,
) -> Result<String, CodexTuiError> {
    let name = match name.filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => return Err(CodexTuiError::NoName),
    };
    if session_id.is_empty() {
        return Err(CodexTuiError::NeverUsed { name });
    }
    Ok(name)
}

/// The shared MUX-PANE create choreography for a codex TUI — driven by
/// `qd start --interactive` (fresh) and by [`revive_codex_tui`] (a stopped row).
pub fn create_codex_tui(
    deps: &PaneDeps<'_>,
    params: &CodexTuiParams,
) -> Result<CodexTuiOutcome, CodexTuiError> {
    let name = params.name.as_str();
    let resume_thread = params.resume_thread.as_deref();

    // THE DISCOVERY FLOOR, sampled BEFORE anything is launched and persisted as
    // the row's `startedAt`. On a fresh start the backfill compares it against
    // each rollout's OWN recorded start time, so a thread that predates this
    // session — the codex the user already had open in this repo — can never be
    // mistaken for ours no matter how long identification takes.
    let since_ms = deps.clock.now_ms();

    // Pre-mint the stable id: it must exist at env-bake time so the pane's env
    // file can export QD_SESSION_ID (a `qd` run from inside the session then knows
    // which session it is). Fail-closed, like the claude lane.
    let minted = match resume_thread {
        // A revive already knows the provider session id, so bind the stable id to
        // it directly (the resume-path parity the claude lane uses): the row and
        // the env agree from the first instant.
        Some(tid) => crate::idstore::mint_or_get(&deps.ids_path, tid, Some(name), deps.clock),
        None => crate::idstore::mint_unbound(&deps.ids_path, Some(name), deps.clock),
    };
    let qd_session_id = match minted {
        Ok(id) => id,
        Err(detail) => return Err(CodexTuiError::IdMintFailed { detail }),
    };

    let cwd_str = params.cwd.to_string_lossy().into_owned();
    // Readiness IS the I6 attachability verify that runs before this waiter.
    let boot_waiter = OkBootWaiter;

    let new_deps = NewDeps {
        mux: deps.mux,
        exec: deps.exec,
        env: deps.env,
        clock: deps.clock,
        paths: deps.paths,
        canonical_dir: deps.canonical_dir.clone(),
        legacy_dirs: deps.legacy_dirs.clone(),
        boot_waiter: &boot_waiter,
        // codex/mod.rs (two levels up from pty/pane.rs) defines CODEX_PROVIDER;
        // a bare `super::` would resolve to `pty`, which does not.
        provider: &super::super::CODEX_PROVIDER,
        backend: deps.backend,
    };
    let new_params = NewParams {
        name: name.to_string(),
        agent: None,
        resume: resume_thread.map(str::to_string),
        // `fork` is meaningless for codex (one thread appends to one rollout), and
        // the codex launch_plan ignores it.
        fork: false,
        claude_args: vec![],
        model: None,
        cwd: params.cwd.clone(),
        // No F1 backend-env capture on this lane: those pairs are claude-backend
        // credentials (`--via` profiles), meaningless to codex. The env file is
        // still written, carrying QD_SESSION_ID + the render birth property.
        backend_env: vec![],
        backend_env_unset: vec![],
        qd_session_id: Some(qd_session_id.clone()),
        render: params.render,
        interactive: true,
        // codex has no extension surface.
        control_socket: None,
    };

    // Claim → scan-under-claim → preflight → launch the pane → I6 verify. A live
    // pane already holding this name fails HERE, loudly, which is also the guard
    // that keeps a revive from starting a second process on one session.
    let out = run_new(&new_deps, &new_params).map_err(CodexTuiError::Create)?;

    // Key the row by the LIVE pane's pid. Everything downstream — liveness,
    // `qd stop`, the ls join — reads pid, and the pane process IS the session's
    // process here (there is no daemon and no self-registering child).
    let Some(pane) = deps
        .mux
        .list(&deps.canonical_dir)
        .unwrap_or_default()
        .into_iter()
        .find(|z| z.name == name)
    else {
        return Err(CodexTuiError::PaneVanished {
            name: name.to_string(),
            canonical: deps.canonical_dir.clone(),
        });
    };

    // The row. `hosting: "mux-pane"` is the load-bearing field — it tells attach to
    // hand over the terminal instead of printing the daemon redirect, and stop to
    // reap the pane instead of group-killing an app-server that was never spawned.
    // NO endpoint (an interactive pane has no ws). On a FRESH start `sessionId` is
    // absent, which is the honest record of the moment: codex will not disclose a
    // thread until someone types.
    let entry = RegistryEntry {
        pid: Some(pane.pid as i64),
        session_id: resume_thread.map(str::to_string),
        cwd: Some(cwd_str),
        started_at: Some(since_ms),
        updated_at: Some(deps.clock.now_ms()),
        status: Some("idle".to_string()),
        name: Some(name.to_string()),
        version: None,
        kind: None,
        entrypoint: None,
        backend: None,
        spawned_by: None,
        provider: Some("codex".to_string()),
        endpoint: None,
        transport: None,
        structured_send_issued: None,
        hosting: Some(Hosting::MuxPane.as_str().to_string()),
    };
    if let Err(detail) = registry::write_entry(&deps.paths.sessions_dir, &entry) {
        return Err(CodexTuiError::RowWriteFailed {
            name: name.to_string(),
            detail: detail.to_string(),
        });
    }

    Ok(CodexTuiOutcome {
        name: out.name,
        zmx_name: name.to_string(),
        socket_dir: deps.canonical_dir.clone(),
        thread_id: resume_thread.map(str::to_string),
        qd_session_id,
    })
}

/// codex-interactive: revive a STOPPED pane-hosted codex session into the SAME
/// thread, detached — the codex twin of the claude revive, and deliberately the
/// same shape so `attach`'s cold arm can call either.
///
/// Identity is carried, not rediscovered: the row's recorded `session_id` becomes
/// `codex resume <id>`, so the revived pane reopens that conversation and the new
/// row is addressable immediately.
///
/// The old tombstone is consumed on success, so one session never leaves two rows
/// behind (the `run_acp_resume` precedent).
pub fn revive_codex_tui(
    deps: &PaneDeps<'_>,
    params: &CodexReviveParams,
) -> Result<ReviveHandle, CodexTuiError> {
    let name = revive_preconditions(Some(&params.name), &params.session_id)?;

    let out = create_codex_tui(
        deps,
        &CodexTuiParams {
            name,
            cwd: params.cwd.clone(),
            render: params.render,
            resume_thread: Some(params.session_id.clone()),
        },
    )?;

    // Consume the prior tombstone (`<old_pid>.json.tombstoned`) so one session does
    // not leave a dangling second row. Best-effort: a missing tombstone is fine (a
    // session stopped a different way), and the new live row is authoritative.
    if let Some(old_pid) = params.old_pid.filter(|&p| p != 0) {
        let _ = std::fs::remove_file(
            deps.paths
                .sessions_dir
                .join(format!("{old_pid}.json.tombstoned")),
        );
    }

    Ok(ReviveHandle {
        socket_dir: out.socket_dir,
        zmx_name: out.zmx_name,
    })
}
