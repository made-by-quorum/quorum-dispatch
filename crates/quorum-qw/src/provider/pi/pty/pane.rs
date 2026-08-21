//! `provider::pi::pane` — the MUX-PANE create + revive choreography for a pi TUI
//! (`qd start --provider pi --interactive`, and the cold arms of `qd resume` /
//! `qd attach`).
//!
//! The pane twin of [`crate::provider::pi::daemon`] (which is the RESIDENT lane), and the pi
//! twin of [`crate::provider::codex::pane`]. Split out of the `qd` verb bodies for
//! the [`crate::create::run_new`] reason: the choreography is a library concern,
//! the printing and the CLI verb attribution are not. Nothing here prints and
//! nothing here exits.
//!
//! THE SAME REUSE ARGUMENT AS THE CODEX PANE LANE, for the same reason: the
//! mux-pane create pipeline is provider-generic (it builds its launch command
//! through `provider.launch_plan` and drives an injected `BootWaiter`), so hosting
//! a pi TUI needs no new pipeline, only a different argv. The atomic name claim,
//! the scan-under-claim, the zmx preflight, the I6 attachability verify, the
//! socket-dir-split detection and the stale-ended-pane reap are therefore the SAME
//! code claude and codex sessions have been hardened on.
//!
//! WHERE THIS DIVERGES FROM CODEX, AND WHY IT IS SIMPLER. codex forced identity to
//! be discovered after the fact — its TUI opens no rollout until a human types, so
//! the codex lane writes a row with NO `sessionId` and the gather step binds one
//! later by attribution. pi's `--session-id` NAMES the session at launch
//! ("creating it if missing"), so this path knows the id BEFORE it spawns
//! anything:
//!
//!   - `session_id = None` (fresh start) — mint a v4 UUID and pass it. The row is
//!     identified from its first instant: no unidentified window, no backfill, and
//!     structurally no way to adopt a stranger's conversation.
//!   - `session_id = Some(id)` (revive) — pass the row's recorded id back, and pi
//!     reopens that exact conversation.
//!
//! Both are the same argv, so unlike codex there is no fresh-vs-revive branch in
//! the launch at all — only in where the id comes from.
//!
//! BOOT DOES NOT WAIT FOR A TRANSCRIPT, and must not. pi writes no session file
//! until the first assistant reply (the persist law recorded in [`super::tui`]),
//! so waiting for one would mean blocking `qd start` until a human typed AND a
//! model answered. The pane is usable the instant it is up, and
//! [`crate::create::run_new`] has already verified it is registered and ATTACHABLE
//! (its I6 step) before the boot waiter runs — which is why the waiter here is the
//! trivial [`crate::create::OkBootWaiter`]. Identity does not depend on the
//! transcript either way; only transcript-derived surfaces (turns, preview) are
//! blank until that first reply, and they say so honestly.
//!
//! THE ONE THING THE PIPELINE CANNOT DO FOR US is the registry row: claude writes
//! its own, the pi TUI knows nothing about qd, so this writes it.
//!
//! ── WHY THIS LANE IS TWO PHASES, AND WHY THAT ORDER IS LOAD-BEARING ─────────
//! [`plan_pi_tui`] holds every refusal that must land BEFORE a name is claimed or
//! a pane is spawned — the `--session-id` capability preflight above all — and it
//! is a SEPARATE public entry point from [`create_pi_tui`] rather than its opening
//! lines. The reason is ordering the user can see: in the pre-split verb these
//! refusals ran before the mux backend, the socket dirs, the mux and the ids store
//! were resolved, so an ancient pi binary is named as the problem even when
//! `QD_MUX` is also bogus. The caller resolves its effects BETWEEN the two phases,
//! which is exactly the interleave that keeps that true.
//!
//! [`plan_pi_tui`] is also NOT idempotent — a fresh start MINTS an id in it — so
//! `create_pi_tui` takes the plan as an input instead of re-deriving it. Calling
//! the planner twice would mint two ids and launch onto the second while the first
//! is what the anti-adoption guard cleared.
//!
//! ── THE VERB IS NOT A PARAMETER ─────────────────────────────────────────────
//! The pre-split functions threaded a `verb: &str` through every `eprintln!` so
//! the same code could say `qd start:` / `qd resume:` / `qd attach:`. That is pure
//! CLI attribution — it names the command the user typed, which a library has no
//! business knowing — so it does NOT cross into [`PiTuiError`]. The variants carry
//! the FACTS; the binary stamps the verb onto the `Display` body at print time.
//! The one exception is [`PiTuiError::Create`], whose inner
//! [`crate::create::NewError`] carries its own attribution already (`qd start: …` /
//! `ERROR: …`) and is therefore printed verbatim — exactly as the pre-split verb
//! printed it, from every caller.

use std::path::PathBuf;

use crate::create::{run_new, NewDeps, NewError, NewParams, OkBootWaiter};
use crate::effects::Env;
use crate::lanes::ReviveHandle;
use crate::launch::RenderMode;
use crate::provider::pane::PaneDeps;
use crate::provider::Hosting;
use crate::registry::{self, RegistryEntry};

use super::tui;

/// Params for one pi-TUI create — the inputs [`plan_pi_tui`] decides from.
pub struct PiTuiParams {
    /// The zmx session name (also the row's `name`).
    pub name: String,
    /// Working dir for the pane, as the caller spelled it. [`plan_pi_tui`]
    /// canonicalizes it.
    pub cwd: PathBuf,
    /// punch item 7: the resolved render mode, a VALUE here — the `--alt-screen` /
    /// `--inline` clap parsing and the `render-default` config read both happen in
    /// the binary, exactly as [`NewParams::render`] receives it.
    pub render: RenderMode,
    /// `None` = fresh start (mint an id); `Some(id)` = revive (carry it). See the
    /// module docs.
    pub session_id: Option<String>,
}

/// What [`plan_pi_tui`] settled: every refusal has passed and the identity is
/// fixed. Hand it straight to [`create_pi_tui`] — see the module docs on why it
/// must not be re-derived.
#[derive(Debug, Clone, PartialEq)]
pub struct PiTuiPlan {
    pub name: String,
    /// The CANONICALIZED cwd, as a path. The launch and the row use this one.
    pub cwd: PathBuf,
    /// The same canonical cwd as a string — what the row stores. See
    /// [`plan_pi_tui`] on why the caller's spelling must never be persisted.
    pub cwd_str: String,
    /// The session id this launch binds to: carried on a revive, minted fresh
    /// otherwise, and validated either way.
    pub session_id: String,
    pub render: RenderMode,
    /// `pi/extension` only: the control socket to launch with. `None` is the
    /// `pi/mux-pane` launch, byte-identical to before this field existed.
    ///
    /// It lives on the PLAN rather than being a second argument to
    /// [`create_pi_tui`] for the reason the module docs give about plans
    /// generally: phase 1 settles every decision, and phase 2 must not
    /// re-derive one. The socket path is a decision — it is what the row's
    /// `endpoint` will claim — so it is settled where the rest of the identity
    /// is settled.
    pub control_socket: Option<String>,
}

/// What [`create_pi_tui`] produced. `session_id` is ALWAYS present — a pi row is
/// identified from birth on both the fresh and the revive lane (contrast
/// [`crate::provider::codex::pane::CodexTuiOutcome::thread_id`], which is `None`
/// until a codex rollout appears).
#[derive(Debug, Clone, PartialEq)]
pub struct PiTuiOutcome {
    pub name: String,
    pub zmx_name: String,
    pub socket_dir: PathBuf,
    pub session_id: String,
    /// The STABLE qd id minted for this session, plumbed out so a caller that did
    /// not mint it can still report it (`qd start --json`'s `qdId`, and
    /// [`crate::contract::SessionHandle::qd_id`]). The mint happens inside this
    /// function, so before this field the only way to learn the id was to be the
    /// minter.
    pub qd_session_id: String,
}

/// Why a pi-TUI plan, create or revive failed.
///
/// `Display` emits the BODY only — no `qd <verb>:` prefix — because the verb is
/// the caller's, not this lane's (see the module docs). [`PiTuiError::Create`] is
/// the deliberate exception: it wraps [`NewError`], which is already fully
/// attributed, and is printed verbatim.
#[derive(Debug)]
pub enum PiTuiError {
    /// Revive gate: the row has no name, so there is nothing to revive it UNDER
    /// (the pane is keyed by name). Nothing was created.
    NoName,
    /// Revive gate: the row has no recorded pi session id. Not reachable through
    /// this lane's own create path (which always writes one), so this is a row
    /// from somewhere else. Refuse rather than mint a new id and pretend it is the
    /// old session.
    NoSessionId { name: String },
    /// Capability preflight: the pi binary answers `--help` but does not advertise
    /// `--session-id`, which this whole lane rides on. `found` is the
    /// already-formatted version decoration (empty when the version could not be
    /// probed) — it decorates, it never becomes the error.
    SessionIdUnsupported { bin: String, found: String },
    /// Capability preflight: the pi binary could not be RUN at all (missing,
    /// unrunnable, timed out), so we cannot TELL. Reported verbatim rather than
    /// guessed in either direction — see [`plan_pi_tui`].
    CapabilityProbeFailed { bin: String, why: String },
    /// The carried session id is one pi will not accept. Only reachable from a
    /// revive carrying a row written by something else; a fresh mint is valid by
    /// construction (unit-pinned).
    InvalidSessionId { name: String, session_id: String },
    /// THE ANTI-ADOPTION GUARD tripped: the freshly minted id already exists on
    /// disk. Nothing was created; retrying mints a different id.
    SessionIdTaken {
        name: String,
        session_id: String,
        root: PathBuf,
    },
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
    /// The pane is RUNNING but its registry row could not be written, so qd cannot
    /// track it. Deliberately not a reap: the session is real and the user can
    /// still reach it by name.
    RowWriteFailed { name: String, detail: String },
}

impl PiTuiError {
    /// Process exit code. Every gate/row failure is exit 1 (the verb precedent);
    /// the create passthrough defers to [`NewError::exit_code`] so the shared
    /// pipeline keeps owning its own mapping.
    pub fn exit_code(&self) -> i32 {
        match self {
            PiTuiError::Create(e) => e.exit_code(),
            _ => 1,
        }
    }

    /// Whether this variant's `Display` already carries its own `qd <verb>:`
    /// attribution and must therefore be printed VERBATIM, without the caller
    /// stamping a verb on it. True only for [`PiTuiError::Create`]; see the module
    /// docs.
    pub fn is_self_attributed(&self) -> bool {
        matches!(self, PiTuiError::Create(_))
    }
}

impl std::fmt::Display for PiTuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PiTuiError::NoName => write!(
                f,
                "this pi session has no name, so there is nothing to revive it \
                 under. Start a fresh one with \"qd start <name> --provider pi --interactive\"."
            ),
            PiTuiError::NoSessionId { name } => write!(
                f,
                "\"{name}\" has no recorded pi session id, so there is no conversation \
                 to reopen. Start a fresh session with \"qd start {name} --provider pi \
                 --interactive\"."
            ),
            PiTuiError::SessionIdUnsupported { bin, found } => write!(
                f,
                "the pi binary {bin:?}{found} does not support --session-id, which \
                 this lane needs to name and re-open the session. qd pins {}. Nothing was \
                 created.\n  \
                 If a newer pi is installed elsewhere, an older one is earlier on PATH — point \
                 qd at the right one with QD_PI_BIN=/path/to/pi, or fix the PATH order.",
                crate::provider::pi::pin::PIN_SPEC
            ),
            PiTuiError::CapabilityProbeFailed { bin, why } => write!(
                f,
                "could not check the pi binary {bin:?} before starting a session: \
                 {why}. Nothing was created. Install pi ({}) and make sure it is on PATH, or \
                 set QD_PI_BIN=/path/to/pi.",
                crate::provider::pi::pin::PIN_SPEC
            ),
            PiTuiError::InvalidSessionId { name, session_id } => write!(
                f,
                "\"{name}\" records the session id {session_id:?}, which pi will not \
                 accept (ids must be alphanumeric with '.', '_' or '-' inside, and start and end \
                 alphanumeric). Start a fresh session with \"qd start <name> --provider pi \
                 --interactive\"."
            ),
            PiTuiError::SessionIdTaken {
                name,
                session_id,
                root,
            } => write!(
                f,
                "refusing to start \"{name}\" — the freshly minted session id \
                 {session_id} already exists under {}. Nothing was created; retrying \
                 mints a different id.",
                root.display()
            ),
            PiTuiError::IdMintFailed { detail } => write!(
                f,
                "could not mint a stable session id: {detail}. Nothing was created."
            ),
            PiTuiError::Create(e) => write!(f, "{e}"),
            PiTuiError::PaneVanished { name, canonical } => write!(
                f,
                "pi session \"{name}\" booted but its pane vanished from {} before the \
                 registry row could be written. Nothing is tracked; retrying is safe.",
                canonical.display()
            ),
            PiTuiError::RowWriteFailed { name, detail } => write!(
                f,
                "pi session \"{name}\" is running but its registry row could not be \
                 written ({detail}), so qd cannot track it. Attach with \"qd attach {name}\" or stop \
                 the pane and retry."
            ),
        }
    }
}

impl std::error::Error for PiTuiError {}

/// The revive preconditions — PURE, and deliberately separate from
/// [`revive_pi_tui`] so a caller can refuse before it resolves HOME. The pre-split
/// verb checked both of these first for exactly that reason, and the order is
/// user-visible (a nameless row with no HOME set must still say "no name"), so it
/// is preserved here rather than folded into the core's opening lines.
///
/// Returns the resolved session name. [`revive_pi_tui`] calls this itself as well
/// — it is idempotent and pure, so the core stays complete for callers that skip
/// the early gate.
pub fn revive_preconditions(name: Option<&str>, session_id: &str) -> Result<String, PiTuiError> {
    let name = match name.filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => return Err(PiTuiError::NoName),
    };
    if session_id.is_empty() {
        return Err(PiTuiError::NoSessionId { name });
    }
    Ok(name)
}

/// Phase 1 — every refusal that must land before a name is claimed or a pane is
/// spawned, plus the identity decision. Reads the environment (the pi binary, the
/// sessions root) and the pi binary itself; touches no registry state and creates
/// nothing. See the module docs on why this is a separate entry point.
pub fn plan_pi_tui(env: &dyn Env, params: &PiTuiParams) -> Result<PiTuiPlan, PiTuiError> {
    // CAPABILITY PREFLIGHT, before a name is claimed or a pane is spawned.
    //
    // The whole lane rides `pi --session-id`, and that flag is not ancient: pi
    // 0.74.2 answers `Error: Unknown option: --session-id` and exits. Without this
    // check that failure happens INSIDE a freshly-spawned pane nobody is attached
    // to — pi dies instantly, the pane dies with it, and `qd start` reports
    // whatever its attachability verify happened to see, which says nothing about
    // the actual cause. Refusing here turns a mysterious dead pane into a sentence
    // naming the binary, its version, and the fix.
    //
    // Reported verbatim when we cannot TELL (missing/unrunnable binary), rather
    // than guessing in either direction: blessing an unknown binary would restore
    // the dead-pane failure, and refusing one would block a working setup we
    // simply could not probe.
    let bin = crate::provider::pi::pi_bin(env);
    match tui::supports_session_id(std::path::Path::new(&bin)) {
        Ok(true) => {}
        Ok(false) => {
            let found = tui::probe_version(std::path::Path::new(&bin))
                .map(|v| format!(" (reports version {v})"))
                .unwrap_or_default();
            return Err(PiTuiError::SessionIdUnsupported { bin, found });
        }
        Err(why) => return Err(PiTuiError::CapabilityProbeFailed { bin, why }),
    }

    // CANONICALIZE THE CWD ONCE, and use the SAME string for the launch and the
    // row. pi encodes the cwd its process resolved into the session DIRECTORY NAME
    // (`--private-tmp-foo--`), so a row storing the caller's spelling (`/tmp/foo`)
    // would send every later transcript lookup into a directory that does not
    // exist — not a mismatch that degrades, a lookup that can never succeed. codex
    // hit the string-compare form of this in end-to-end validation; pi's encoding
    // makes it structural.
    let cwd_str = crate::provider::canonical_dir(&params.cwd.to_string_lossy());
    let cwd = PathBuf::from(&cwd_str);

    // The id: carried on a revive, minted on a fresh start. `is_fresh` is captured
    // here because the anti-adoption guard below applies to a MINTED id only.
    let is_fresh = params.session_id.is_none();
    let session_id = match params.session_id.as_deref() {
        Some(id) => id.to_string(),
        None => tui::mint_session_id(),
    };
    if !tui::is_valid_session_id(&session_id) {
        // Only reachable from a revive carrying a row written by something else —
        // a fresh mint is valid by construction (unit-pinned). pi would
        // `process.exit(1)` on this argv inside the new pane, so the pane would
        // flash and die and the failure would name nothing useful.
        return Err(PiTuiError::InvalidSessionId {
            name: params.name.clone(),
            session_id,
        });
    }

    // THE ANTI-ADOPTION GUARD, and it only applies to a FRESH id. `--session-id`
    // OPENS an existing session of that id rather than failing, so launching onto
    // one we did not create would silently point this row's transcript, turns and
    // `qd stop` at another conversation. A v4 UUID makes that essentially
    // impossible; this makes it impossible. On a REVIVE the id being present is
    // the entire point, so the check is deliberately not run there.
    if is_fresh {
        if let Some(root) = crate::provider::pi::sessions_root(env) {
            if tui::session_id_is_taken(&root, &cwd_str, &session_id) {
                return Err(PiTuiError::SessionIdTaken {
                    name: params.name.clone(),
                    session_id,
                    root,
                });
            }
        }
    }

    Ok(PiTuiPlan {
        name: params.name.clone(),
        cwd,
        cwd_str,
        session_id,
        render: params.render,
        // The pane lane launches an UNCHANNELLED pi. `pi/extension` builds its
        // plan through this same function and then fills this in.
        control_socket: None,
    })
}

/// Phase 2 — the shared MUX-PANE create choreography for a pi TUI: claim, launch,
/// I6 verify, write the row. Takes the [`PiTuiPlan`] phase 1 settled; see the
/// module docs on why it does not re-derive it.
pub fn create_pi_tui(
    deps: &PaneDeps<'_>,
    plan: &PiTuiPlan,
) -> Result<PiTuiOutcome, PiTuiError> {
    let name = plan.name.as_str();
    let since_ms = deps.clock.now_ms();

    // Pre-mint the stable id, bound to the pi session id we already know. Unlike
    // codex's fresh lane there is no `mint_unbound` case: pi identity exists
    // before the pane does, so the row and the pane env agree from the first
    // instant on BOTH ids.
    let qd_session_id = match crate::idstore::mint_or_get(
        &deps.ids_path,
        &plan.session_id,
        Some(name),
        deps.clock,
    ) {
        Ok(id) => id,
        Err(detail) => return Err(PiTuiError::IdMintFailed { detail }),
    };

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
        provider: &crate::provider::pi::PI_PROVIDER,
        backend: deps.backend,
    };
    let new_params = NewParams {
        name: name.to_string(),
        agent: None,
        // `resume` is how the id reaches `PiProvider::launch_plan`, on BOTH lanes
        // — pi's `--session-id` creates-or-opens, so "the id this launch binds to"
        // is one concept with one flag. See that method's doc.
        resume: Some(plan.session_id.clone()),
        // pi's `--fork` forks an EXISTING session into a new one, which is a
        // different verb's job; this lane never forks.
        fork: false,
        claude_args: vec![],
        model: None,
        cwd: plan.cwd.clone(),
        // No F1 backend-env capture on this lane: those pairs are claude-backend
        // credentials (`--via` profiles), meaningless to pi. The env file is still
        // written, carrying QD_SESSION_ID + the render birth property.
        backend_env: vec![],
        backend_env_unset: vec![],
        qd_session_id: Some(qd_session_id.clone()),
        render: plan.render,
        interactive: true,
        // The ONE argv difference between the two pi pane lanes. `None` here is
        // `pi/mux-pane`; `Some(path)` makes the launched pi serve a control
        // channel on it.
        control_socket: plan.control_socket.clone(),
    };

    // Claim → scan-under-claim → preflight → launch the pane → I6 verify. A live
    // pane already holding this name fails HERE, loudly, which is also the guard
    // that keeps a revive from starting a second process on one session.
    let out = run_new(&new_deps, &new_params).map_err(PiTuiError::Create)?;

    // Key the row by the LIVE pane's pid: the pane process IS the session's process
    // here (no daemon, no self-registering child), and everything downstream —
    // liveness, `qd stop`, the ls join — reads pid.
    let Some(pane) = deps
        .mux
        .list(&deps.canonical_dir)
        .unwrap_or_default()
        .into_iter()
        .find(|z| z.name == name)
    else {
        return Err(PiTuiError::PaneVanished {
            name: name.to_string(),
            canonical: deps.canonical_dir.clone(),
        });
    };

    // The row. `hosting: "mux-pane"` is the load-bearing field — it tells attach to
    // hand over the terminal instead of printing the daemon redirect, stop to reap
    // the pane instead of group-killing a resident that was never spawned, and send
    // to use the pane's PTY instead of a ws endpoint. NO endpoint (an interactive
    // pane has no resident front). `sessionId` is present from birth — the whole
    // point of the `--session-id` lane.
    let entry = RegistryEntry {
        pid: Some(pane.pid as i64),
        session_id: Some(plan.session_id.clone()),
        cwd: Some(plan.cwd_str.clone()),
        started_at: Some(since_ms),
        updated_at: Some(deps.clock.now_ms()),
        status: Some("idle".to_string()),
        name: Some(name.to_string()),
        version: None,
        kind: None,
        entrypoint: None,
        backend: None,
        spawned_by: None,
        provider: Some("pi".to_string()),
        endpoint: None,
        transport: None,
        structured_send_issued: None,
        hosting: Some(Hosting::MuxPane.as_str().to_string()),
    };
    if let Err(detail) = registry::write_entry(&deps.paths.sessions_dir, &entry) {
        return Err(PiTuiError::RowWriteFailed {
            name: name.to_string(),
            detail: detail.to_string(),
        });
    }

    Ok(PiTuiOutcome {
        name: out.name,
        zmx_name: name.to_string(),
        socket_dir: deps.canonical_dir.clone(),
        session_id: plan.session_id.clone(),
        qd_session_id,
    })
}

/// pi-interactive: revive a STOPPED pane-hosted pi session into the SAME session,
/// detached — the pi twin of [`crate::provider::codex::pane::revive_codex_tui`],
/// and deliberately the same shape so `attach`'s cold arm can call either.
///
/// Identity is carried, not rediscovered: the row's recorded id becomes `pi
/// --session-id <id>`, which reopens that conversation.
///
/// WHY THIS HAS NO "never used" REFUSAL, unlike the codex twin. The codex revive
/// must refuse a session that was never used, because codex only mints a thread id
/// once someone types — an unused codex row has NO id, and launching a bare `codex`
/// would silently hand back a different conversation under the old name. A pi row
/// has had its id since birth, so reviving an unused one is well defined: pi
/// recreates a session under that same id (its file having never been written), and
/// the row keeps addressing the same thing it always did. There is nothing to
/// refuse. ([`PiTuiError::NoSessionId`] is a different case — a row this lane did
/// not write.)
///
/// The old tombstone is consumed on success, so one session never leaves two rows
/// behind (the `run_acp_resume` precedent).
///
/// The [`PiTuiPlan`] is an argument for the phase-ordering reason in the module
/// docs: the caller ran phase 1 before it resolved the effects in `deps`. It also
/// already carries the name and the carried session id, so there is no separate
/// revive params struct — `old_pid` is the only thing a revive adds.
pub fn revive_pi_tui(
    deps: &PaneDeps<'_>,
    plan: &PiTuiPlan,
    old_pid: Option<i64>,
) -> Result<ReviveHandle, PiTuiError> {
    revive_preconditions(Some(&plan.name), &plan.session_id)?;

    let out = create_pi_tui(deps, plan)?;

    // Consume the prior tombstone (`<old_pid>.json.tombstoned`) so one session does
    // not leave a dangling second row. Best-effort: a missing tombstone is fine (a
    // session stopped a different way), and the new live row is authoritative.
    if let Some(old_pid) = old_pid.filter(|&p| p != 0) {
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
