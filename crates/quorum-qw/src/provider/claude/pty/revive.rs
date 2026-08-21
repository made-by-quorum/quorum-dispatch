//! `provider::claude::pty::revive` — the SHARED cold→drivable claude revive.
//!
//! LIVES UNDER `provider/claude/pty/` (harness-first reorg): this is the
//! claude PTY/mux-pane lane's revive step — it relaunches into a native TUI
//! pane, exactly the family the codex and pi `pty/` groupings hold for their
//! own harnesses. `claude/mod.rs` carries `pub use self::pty::revive;` so
//! `crate::provider::claude::revive::…` (all five pre-reorg callers) keeps
//! resolving unchanged.
//!
//! W1 phase 2. This is the seam `qd resume`, `qd attach`'s cold arm, `qd send`'s
//! wake path, the adoption relaunch and the lane `wake` all ride: it relaunches a
//! native-TUI `claude --resume <id>` DETACHED and ready-gated, then hands back the
//! attach coordinates so a caller that wants a terminal can `mux.attach` (NO fused
//! `zmx attach … bash -lc`).
//!
//! Split out of the `qd resume` verb body for the [`crate::create::run_new`]
//! reason. Nothing here prints and nothing here exits.
//!
//! SCOPE: this is the ZMX/embedded DETACHED revive — it deliberately does NOT
//! carry resume's `--no-zmx` bare-exec branch nor a fused-default `zmx attach`
//! path. Every caller wants detached-then-maybe-attach, which is exactly this plus
//! a follow-up `mux.attach`.
//!
//! ── WHY THIS IS TWO PHASES, AND WHY THAT ORDER IS LOAD-BEARING ──────────────
//! [`plan_claude_revive`] runs everything that must land BEFORE the mux backend
//! exists — the cwd reality-check, the provider resolution, the D4 same-name
//! guard, the identity mint, the env-file write — and [`run_claude_revive`] runs
//! the backend-dependent half. They are separate entry points because the caller
//! resolves its backend, socket dirs and mux BETWEEN them, which is the order the
//! pre-split verb had: a session whose name is held by a live session says so even
//! when `QD_MUX` is also bogus, and no env file is written for a launch that the
//! same-name guard was going to refuse.
//!
//! [`plan_claude_revive`] is NOT idempotent — it writes the per-session env file —
//! so [`run_claude_revive`] takes the plan as an input rather than re-deriving it.
//!
//! ── AND WHY THE PLAN HAS A SECOND SHAPE (punch R21) ─────────────────────────
//! Not every `qd attach` on a claude row wants a relaunch. A session that was
//! started and never messaged has a RUNNING pane and no provider session id at
//! all — claude mints one when it writes its first transcript record, and there
//! is no first record. The join can only surface that as a `ZmxOnly` row
//! (`session_id: ""`), and this module used to answer it with
//! [`ReviveClaudeError::NoSessionId`] — a dead end on a session that is sitting
//! right there, idle, waiting to be typed at.
//!
//! So [`plan_claude_revive`] can now settle on
//! [`ClaudeRevivePlan::SeedLivePane`] instead: send the pane an opening message
//! (which is what mints the id) and hand back ITS coordinates. Same two phases,
//! same [`ReviveHandle`], same `mux.attach` on the other side — the caller does
//! not learn which shape it got, because its next move does not change.
//! [`crate::onboarding::HOW_TO_REACH_PEERS`] is the message, shared with the
//! relay MCP instructions so a session hears one story about talking to peers no
//! matter which door it came in by.
//!
//! ── THE VERB-PREFIX BUG THIS MOVE FIXES ─────────────────────────────────────
//! The pre-split `revive_claude` hard-coded `qd attach:` on every one of its own
//! error lines and `qd resume:` on every line its shared helpers emitted — from
//! ALL FIVE callers. So `qd resume <session>` on a name-held session reported
//! `qd attach: name "wk" is held …`, and `qd attach` on a missing zmx reported
//! `qd resume: could not launch zmx …`. Each line named a command the user had not
//! typed, and pointed the reader at the wrong verb's docs.
//!
//! The codex and pi pane revives already took a `verb: &str` for exactly this
//! reason. This one takes a typed error instead, which is strictly better: the
//! variant carries the FACTS, [`ReviveClaudeError::line`] takes the verb as an
//! argument, and there is no way to construct a line without saying which command
//! it belongs to. The wording is otherwise byte-identical — only the verb changes,
//! and only where it was WRONG.
//!
//! Lines that carry NO `qd <verb>:` prefix in the first place (`ERROR: …`,
//! `Cannot resume: …`, `Failed to resume session: …`) are left exactly as they
//! were. They are not verb-attributed and never were.

use std::path::{Path, PathBuf};

use crate::boot::{EventBootWaiter, RealSleeper};
use crate::create::BootWaiter;
use crate::effects::{Clock, Env};
use crate::lanes::ReviveHandle;
use crate::launch::{
    build_claude_cmd, capture_backend_env, claude_bin, claude_flags, launch_env_pairs,
    render_env_unsets, session_env_prefix, write_session_env_file_with_unsets, RenderMode,
};
use crate::model::Session;
use crate::mux::Mux;
use crate::paths::QdPaths;
use crate::resume::{
    clear_stale_panes, derive_zmx_name, live_zmx_name_holder, resolve_resume_cwd,
    validate_session_name, ResumeCwd, StalePaneRefusal,
};

/// Why a claude revive failed.
///
/// DELIBERATELY NO `Display`. Most of these lines are `qd <verb>:`-attributed and
/// the verb is the CALLER's — see the module docs on the bug that came from
/// guessing it. A `Display` would let a caller print a line with no verb, or with
/// the wrong one, by accident; [`line`](Self::line) makes the verb impossible to
/// omit. The arms that carry no verb prefix ignore the argument, which is the
/// honest encoding of "this line was never verb-attributed".
#[derive(Debug)]
pub enum ReviveClaudeError {
    /// The row carries no provider session id — there is nothing to resume
    /// against. NOT verb-attributed (it never was).
    ///
    /// NARROWED BY PUNCH R21. This used to answer every id-less row; it now
    /// answers only the ones with no live pane either. A row whose pane is still
    /// running is missing its id because nothing has been said to that session
    /// yet, and refusing it was refusing the one case a first message would fix —
    /// see [`ClaudeRevivePlan::SeedLivePane`] and [`live_pane_to_seed`]. The
    /// WORDING is unchanged: for a row with no pane, "Cannot resume: no session
    /// ID found." is still exactly what happened.
    NoSessionId,
    /// F3 cwd reality-check: the recorded project dir (or an explicit override)
    /// does not exist. Carries the actionable message
    /// [`crate::resume::resolve_resume_cwd`] produced. NOT verb-attributed.
    CwdUnresolvable(String),
    /// `provider_for` did not recognise the row's provider string.
    UnknownProvider { provider: String },
    /// S2: the DERIVED zmx name failed the safety whitelist. Carries the
    /// validator's message. NOT verb-attributed.
    NameUnsafe(String),
    /// D4 same-name guard: a DIFFERENT live session already holds this zmx name,
    /// so the stale-same-name kill would destroy ITS pane. Nothing was launched.
    NameHeldLive { zmx_name: String, holder: String },
    /// Fail-closed identity: the stable id could not be minted, so the relaunched
    /// session's env would silently miss it. Nothing was launched.
    IdMintFailed { detail: String },
    /// The per-session env file could not be written. Fail closed BEFORE launch.
    EnvFileWriteFailed { detail: String },
    /// [`crate::resume::clear_stale_panes`] refused: a live or unprovable
    /// same-name pane holds the slot.
    StalePane(StalePaneRefusal),
    /// `zmx run` exited nonzero. Carries zmx's (trimmed) stderr. Fails IMMEDIATELY
    /// — the boot waiter never runs on a failed launch (the
    /// no-swallow-into-boot-timeout contract, punch item 6). NOT verb-attributed.
    LaunchFailed { stderr: String },
    /// The launch could not be SPAWNED at all (zmx not on PATH). Same
    /// fail-immediately contract as [`ReviveClaudeError::LaunchFailed`].
    ZmxMissing,
    /// NAMED DIVERGENCE (loud>silent, ADD-9a): the session launched but the
    /// ADR-0005 EVENT ready-wait timed out, so boot did not CONFIRM. Carries the
    /// waiter's detail (the typed phase is not printed — the human surface is
    /// byte-stable across m-4's retype).
    BootUnconfirmed { detail: String },
}

impl ReviveClaudeError {
    /// The complete stderr line, with the CALLER's verb stamped in where the line
    /// is verb-attributed. See the type doc.
    ///
    /// Expressed in terms of [`body`](Self::body) +
    /// [`is_self_attributed`](Self::is_self_attributed) so the three renderings
    /// cannot drift: this one, and the two a caller that has no verb has to
    /// build for itself.
    pub fn line(&self, verb: &str) -> String {
        if self.is_self_attributed() {
            self.body()
        } else {
            format!("qd {verb}: {}", self.body())
        }
    }

    /// The line WITHOUT its attribution — everything after `qd <verb>: ` for a
    /// verb-attributed variant, and the WHOLE line for a self-attributed one
    /// (which has no `qd <verb>:` to remove).
    ///
    /// This exists because [`crate::contract::LaneOps::wake`] has no verb and
    /// must not invent one. `qd resume`, `qd attach` and `qd send` all reach the
    /// same revive, and the line each user reads must name the command THEY
    /// typed; a lane that stamped `qd wake:` would be naming a command that does
    /// not exist. So the lane carries the body out and the verb stamps it — see
    /// [`crate::contract::LaneError::WakeFailed`].
    pub fn body(&self) -> String {
        match self {
            ReviveClaudeError::NoSessionId => "Cannot resume: no session ID found.".to_string(),
            ReviveClaudeError::CwdUnresolvable(e) => format!("ERROR: {e}"),
            ReviveClaudeError::UnknownProvider { provider } => format!(
                "unknown provider \"{provider}\" — this engine supports: claude-code."
            ),
            ReviveClaudeError::NameUnsafe(err) => format!("ERROR: {err}"),
            // The D4 guard's EXACT error line (pinned by a unit).
            ReviveClaudeError::NameHeldLive { zmx_name, holder } => format!(
                "name \"{zmx_name}\" is held by running session {holder}; \
                 rename or stop it first"
            ),
            ReviveClaudeError::IdMintFailed { detail } => {
                format!("could not mint a stable session id: {detail}")
            }
            ReviveClaudeError::EnvFileWriteFailed { detail } => {
                format!("failed to write session env file: {detail}")
            }
            ReviveClaudeError::StalePane(r) => r.body(),
            // punch item 6: a nonzero `zmx run` carries zmx's stderr verbatim.
            ReviveClaudeError::LaunchFailed { stderr } => {
                format!("Failed to resume session: {}", stderr.trim())
            }
            // punch item 6: a spawn-level Err is the missing-binary guidance.
            ReviveClaudeError::ZmxMissing => {
                "could not launch zmx (is it installed and on PATH?).".to_string()
            }
            // ORC CONDITION (i), ack3-spec §8: a NAMED ADD-9a divergence contract
            // — m-4's retype of `wait_ready` to a typed `BootFailure` must NOT
            // drift this wording. The prefix + the waiter detail, nothing else.
            ReviveClaudeError::BootUnconfirmed { detail } => {
                format!("session launched but did not confirm ready: {detail}")
            }
        }
    }

    /// Whether [`body`](Self::body) is ALREADY a complete line and must be
    /// printed verbatim, with no `qd <verb>:` stamped on it.
    ///
    /// The four that are: the two `ERROR: …` refusals (the S2 name check and the
    /// F3 cwd reality-check, whose wording is the TS port's and carries no verb),
    /// `Cannot resume: no session ID found.`, and the `Failed to resume session:`
    /// passthrough that carries zmx's own stderr. Same discipline as
    /// `CodexTuiError::is_self_attributed`.
    pub fn is_self_attributed(&self) -> bool {
        matches!(
            self,
            ReviveClaudeError::NoSessionId
                | ReviveClaudeError::CwdUnresolvable(_)
                | ReviveClaudeError::NameUnsafe(_)
                | ReviveClaudeError::LaunchFailed { .. }
        )
    }

    /// Process exit code. Every revive failure is exit 1 (the verb precedent).
    pub fn exit_code(&self) -> i32 {
        1
    }
}

/// Injected effects for [`plan_claude_revive`] — phase 1, everything that happens
/// before a mux exists.
pub struct ClaudePlanDeps<'a> {
    /// Env: the claude bin/flags resolution, the F1 backend-env capture (L9a).
    pub env: &'a dyn Env,
    /// The resolved HOME. The per-session env file is written under it.
    pub home: &'a Path,
    /// Home→state layout (L9a) — the D4 guard scans `paths.sessions_dir`.
    pub paths: &'a QdPaths,
    /// The stable-id store, resolved ONCE by the caller.
    pub ids_path: PathBuf,
    /// Clock — the idstore mint line.
    pub clock: &'a dyn Clock,
    /// The cwd to fall back to when the row records none. Process cwd is a CLI
    /// fact, so the caller supplies it.
    pub fallback_cwd: String,
}

/// Params for one claude revive.
pub struct ClaudeReviveParams<'a> {
    /// The row being revived. Its `session_id` is the durable identity the
    /// relaunch resumes; `name`/`cwd`/`pid` feed the provider's resume key.
    pub session: &'a Session,
    /// An explicit `--cwd` override, if the caller parsed one.
    pub cwd_override: Option<&'a str>,
    /// The resolved render mode — a launch-time birth property, a VALUE here (the
    /// `--alt-screen` / `--inline` clap parsing and the `render-default` config
    /// read both happen in the binary).
    pub render: RenderMode,
    /// adoption:relaunch for ZERO-TURN sessions. See [`plan_claude_revive`].
    pub fresh: bool,
}

/// What [`plan_claude_revive`] settled. Hand it straight to
/// [`run_claude_revive`]; see the module docs on why it must not be re-derived.
///
/// TWO SHAPES because there are two ways a `qd attach` on a claude row can end
/// with the user in a live pane, and they are not variants of one launch. See
/// [`ClaudeRevivePlan::SeedLivePane`] for the second one (punch R21).
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeRevivePlan {
    /// The ordinary revive: the row is genuinely cold, so relaunch `claude` into
    /// a fresh detached pane and wait for boot.
    Relaunch {
        /// The derived, S2-validated zmx session name.
        zmx_name: String,
        /// The complete `<env prefix>command 'claude' …` shell command.
        claude_cmd: String,
        /// The reality-checked working dir the pane is launched in.
        cwd: PathBuf,
    },
    /// PUNCH R21 — the row has NO provider session id but DOES have a live pane,
    /// so there is nothing to relaunch and nothing to resume: seed the pane that
    /// is already running and hand its coordinates back.
    ///
    /// This is the never-messaged claude session. Its pane is up, but claude has
    /// written no transcript and no registry row, so the join can only build it
    /// as a `ZmxOnly` row — `session_id: ""`, `status: Cold` (the port's honest
    /// literal, not a claim the process is gone). Revive used to refuse it with
    /// [`ReviveClaudeError::NoSessionId`], which was a true statement about the
    /// row and a dead end for the user: the id it wanted does not exist YET, and
    /// the only thing that mints it is a first turn.
    ///
    /// So: type the first turn. [`crate::onboarding::HOW_TO_REACH_PEERS`] is what
    /// gets typed — the same wording punch R9 put in the relay MCP instructions —
    /// which makes one send do both jobs, minting the id AND telling the agent
    /// how to reach its peers. Then attach to the pane that was there all along.
    ///
    /// RELAUNCHING WOULD NOT WORK, and that is worth recording rather than
    /// rediscovering: [`crate::resume::clear_stale_panes`] refuses to kill a pane
    /// whose process is ALIVE, so the relaunch arm cannot even reach its launch
    /// on this row — it would trade `Cannot resume: no session ID found.` for
    /// `a RUNNING pane named "…" holds this name`. The pane is not in the way of
    /// the fix; the pane IS the fix.
    SeedLivePane {
        /// The live pane's name, taken from the row rather than derived — a
        /// ZmxOnly row's name IS its pane's name, and deriving one from an empty
        /// session id would address a pane that does not exist.
        zmx_name: String,
        /// The socket dir the pane was actually FOUND in (Bug D /
        /// cs-owns-session-identity: per-session ops MUST target this dir, never
        /// a re-resolved `ZMX_DIR`). `None` when the row carries none, which the
        /// launch phase reads as "use the caller's canonical dir".
        socket_dir: Option<PathBuf>,
    },
}

/// WP-B5-ii-b (PROOF 1) — the resume argv fragment a cold-row revive passes to
/// claude, built from the row's RECORDED `session_id` ([`Session::session_id`] —
/// the durable identity the daemon minted onto the child-pid-keyed row). The
/// attach→Cold→revive durability proof pins THIS wiring: the recorded id flows
/// into `--resume <id>` so revive resumes the SAME claude session, never a fresh
/// one. Factored pure (no spawn) so the wiring is unit-testable on the default
/// floor — the cheap mirror of the `#[ignore]` end-to-end seed
/// (`headless_revive_recorded_id.rs`).
///
/// FIX-SHAPED MUTATION (PROOF 1 red-before): replace `id: &session.session_id`
/// with `id: ""` → the fragment loses `--resume <recorded-id>` → revive starts a
/// FRESH claude session → the recorded-id resume proof reds.
pub fn revive_resume_args(
    provider: &dyn crate::provider::Provider,
    session: &Session,
) -> Vec<String> {
    let resume_key = crate::provider::SessionKey {
        id: &session.session_id,
        name: session.name.as_deref(),
        cwd: session.cwd.as_deref(),
        pid: session.pid,
    };
    provider.resume_args(&resume_key, false)
}

/// P0 wave-2 (spec-w2-env D1+D4) — the SHARED resume/revive env prep, in the exact
/// same sequence every revive path needs: run the D4 same-name guard BEFORE any
/// side effect (the spike hazard — the stale-kill must never destroy a DIFFERENT
/// live session's pane; the target's own stale pane keeps the kill-then-relaunch
/// flow); D1 mint/fetch the stable id (the UUID is known here, so `mint_or_get`
/// keys it directly — lazy-mints for pre-stable-id sessions; fail-closed: never
/// relaunch a session whose env would silently miss its identity); write the
/// UNCONDITIONAL self-deleting env file (lifecycle.ts:483-485) carrying
/// `export QD_SESSION_ID='<id>'` (an explicit set, overriding anything inherited
/// through the caller's subtree) plus the captured backend pairs — so the env file
/// + dot-source prefix are unconditional on every revive branch.
pub fn prepare_claude_resume_env(
    home: &Path,
    paths: &QdPaths,
    ids_path: &Path,
    clock: &dyn Clock,
    zmx_name: &str,
    session_id: &str,
    session_name: Option<&str>,
    backend_env: Vec<(String, String)>,
    render: RenderMode,
    base_claude_cmd: &str,
) -> Result<String, ReviveClaudeError> {
    if let Some(holder) =
        live_zmx_name_holder(&paths.sessions_dir, ids_path, zmx_name, session_id)
    {
        return Err(ReviveClaudeError::NameHeldLive {
            zmx_name: zmx_name.to_string(),
            holder,
        });
    }
    let qd_id = match crate::idstore::mint_or_get(ids_path, session_id, session_name, clock) {
        Ok(id) => id,
        Err(detail) => return Err(ReviveClaudeError::IdMintFailed { detail }),
    };
    // punch item 7: the render-mode birth property rides the SAME shared
    // assembly as create's path (launch_env_pairs — one assembly point, every
    // launch site). R2 (override-never-inherit): an --alt-screen revive must
    // EXPLICITLY `unset -v` the inline var — omitting the export alone leaves
    // the child inheriting it from an inline parent env — so the unset list
    // rides this path too (the with-unsets writer; empty for inline, whose
    // export clobbers anything inherited).
    let env_pairs = launch_env_pairs(backend_env, Some(qd_id), render);
    let env_unsets = render_env_unsets(render);
    if let Err(e) = write_session_env_file_with_unsets(home, zmx_name, &env_pairs, &env_unsets) {
        return Err(ReviveClaudeError::EnvFileWriteFailed {
            detail: e.to_string(),
        });
    }
    let env_prefix = session_env_prefix(home, zmx_name, &env_pairs, &env_unsets);
    Ok(format!("{env_prefix}{base_claude_cmd}"))
}

/// PUNCH R21 — the pure half of the never-messaged decision: does this row carry
/// a pane that is ALREADY RUNNING, so that the missing provider session id is a
/// "not yet" rather than a "never"?
///
/// `Some` ⇒ [`ClaudeRevivePlan::SeedLivePane`]; `None` ⇒ there is genuinely
/// nothing to talk to and [`ReviveClaudeError::NoSessionId`] is still the honest
/// answer. Two facts, both from the row, both required:
///
/// - a **pane name**. A ZmxOnly row's `name` and `zmx_name` are its pane's name;
///   a row with neither is a transcript or registry artifact with no pane behind
///   it.
/// - a **live pid**. `is_pid_alive` is the same proof
///   [`crate::resume::clear_stale_panes`] demands before it will touch a pane,
///   and for the same reason: the row's `Cold` status is a port literal, not
///   evidence, so the process is asked directly. A dead or absent pid means the
///   pane is gone — seeding it would type into nothing and report success.
///
/// FACTORED PURE (no mux, no fs, no clock) so the decision is unit-testable on
/// the default floor, the same shape [`revive_resume_args`] takes for its own
/// wiring proof.
///
/// FIX-SHAPED MUTATION: drop the `is_pid_alive` conjunct → a cold ZmxOnly ghost
/// (a row whose pane died) plans a seed instead of refusing → the
/// `dead_pane_is_still_no_session_id` unit reds.
pub fn live_pane_to_seed(session: &Session) -> Option<ClaudeRevivePlan> {
    let zmx_name = session
        .zmx_name
        .as_deref()
        .or(session.name.as_deref())
        .filter(|n| !n.is_empty())?;
    // `> 0`, not `!= 0`: `is_pid_alive` bottoms out in `kill(pid, 0)`, where 0
    // means "this process group" and a negative pid means "that process group" —
    // both would answer ALIVE for a row that records no usable pid at all.
    let pid = session.pid.filter(|p| *p > 0)?;
    if !crate::effects::is_pid_alive(pid as i32) {
        return None;
    }
    Some(ClaudeRevivePlan::SeedLivePane {
        zmx_name: zmx_name.to_string(),
        socket_dir: session.socket_dir.as_deref().map(PathBuf::from),
    })
}

/// Phase 1 — resolve the cwd, assemble the claude argv through the provider seam,
/// derive + validate the zmx name, run the D4 guard, mint the identity and write
/// the env file. Touches no mux; creates nothing but the env file. See the module
/// docs on why this is a separate entry point.
pub fn plan_claude_revive(
    deps: &ClaudePlanDeps<'_>,
    params: &ClaudeReviveParams<'_>,
) -> Result<ClaudeRevivePlan, ReviveClaudeError> {
    let session = params.session;

    // PUNCH R21: no provider session id is TWO different situations, and the old
    // single refusal answered both with the harsher one. A row whose pane is
    // still running has no id because nothing has been said to it yet — so say
    // something (see [`ClaudeRevivePlan::SeedLivePane`]). A row with no live pane
    // has no id and nothing to mint one: that one is still a dead end, and this
    // is the FIRST thing checked so it stays the same dead end it always was —
    // before the cwd probe, before the argv build, before the env file.
    if session.session_id.is_empty() {
        return live_pane_to_seed(session).ok_or(ReviveClaudeError::NoSessionId);
    }

    // F3: cwd reality-check BEFORE any spawn (lifecycle.ts:451-462).
    let exists = |p: &str| Path::new(p).exists();
    let cwd = match resolve_resume_cwd(
        session.cwd.as_deref(),
        params.cwd_override,
        &exists,
        &deps.fallback_cwd,
    ) {
        ResumeCwd::Cwd(c) => c,
        ResumeCwd::Error(e) => return Err(ReviveClaudeError::CwdUnresolvable(e)),
    };

    // The claude relaunch argv via the provider seam (fork=false), identical to
    // resume's claude path.
    let config_toml = deps.home.join(".quorum").join("dispatch").join("config.toml");
    let bin = claude_bin(deps.env);
    let flags = claude_flags(deps.env, &config_toml);
    let Some(provider_impl) = crate::provider::provider_for(&session.provider) else {
        return Err(ReviveClaudeError::UnknownProvider {
            provider: session.provider.clone(),
        });
    };
    // adoption:relaunch for zero-turn sessions: no JSONL exists, so --resume
    // fails with "No conversation found". Use --session-id to start fresh under
    // the same UUID. Also pass --name so claude writes the name into its new
    // registry row; without it, the row's name is None and the adopt identity
    // check (which requires the relaunched session to carry the requested name)
    // fails with "resume identity mismatch".
    let extra = if params.fresh {
        let mut args = vec!["--session-id".to_string(), session.session_id.clone()];
        if let Some(name) = session.name.as_deref().filter(|n| !n.is_empty()) {
            args.push("--name".to_string());
            args.push(name.to_string());
        }
        args
    } else {
        revive_resume_args(provider_impl, session)
    };
    let base_claude_cmd = build_claude_cmd(&bin, &flags, &extra);

    // F1: capture backend env + write the self-deleting env file (lifecycle.ts:466-485).
    let backend_env = capture_backend_env(deps.env);
    let zmx_name = derive_zmx_name(None, session.name.as_deref(), &session.session_id);
    if let Some(err) = validate_session_name(&zmx_name) {
        return Err(ReviveClaudeError::NameUnsafe(err));
    }

    // P0 wave-2 (spec-w2-env D1+D4) — IDENTICAL to resume's claude path.
    let claude_cmd = prepare_claude_resume_env(
        deps.home,
        deps.paths,
        &deps.ids_path,
        deps.clock,
        &zmx_name,
        &session.session_id,
        session.name.as_deref(),
        backend_env,
        params.render,
        &base_claude_cmd,
    )?;

    Ok(ClaudeRevivePlan::Relaunch {
        zmx_name,
        claude_cmd,
        cwd: PathBuf::from(&cwd),
    })
}

/// The backend-dependent half's effects. The caller resolves the backend, the
/// socket dirs and the mux between the two phases — see the module docs.
pub struct ClaudeLaunchDeps<'a> {
    /// The backend-selected mux.
    pub mux: &'a dyn Mux,
    /// The canonical socket dir the pane is created in, and the one the returned
    /// [`ReviveHandle`] points at.
    pub canonical_dir: PathBuf,
    /// Canonical THEN legacy dirs — every dir the stale-same-name sweep scans.
    pub scan_dirs: Vec<PathBuf>,
    /// Home→state layout — the boot waiter watches `paths.sessions_dir`.
    pub paths: &'a QdPaths,
}

/// The SHARED detached-revive seam (W1 phase 2): `run_detached` + the ADR-0005
/// EVENT ready-wait. Returns `Ok(())` when the session is detached + confirmed
/// ready. The caller owns any success stdout line.
///
/// punch item 6: BOTH launch failures fail IMMEDIATELY — the boot waiter never
/// runs on a failed launch (the no-swallow-into-boot-timeout contract).
pub fn run_detached_revive(
    mux: &dyn Mux,
    canonical: &Path,
    zmx_name: &str,
    claude_cmd: &str,
    cwd_path: &Path,
    paths: &QdPaths,
    clock: &dyn Clock,
) -> Result<(), ReviveClaudeError> {
    match mux.run_detached(canonical, zmx_name, claude_cmd, cwd_path) {
        Ok(r) if r.status == Some(0) => {}
        Ok(r) => return Err(ReviveClaudeError::LaunchFailed { stderr: r.stderr }),
        Err(_) => return Err(ReviveClaudeError::ZmxMissing),
    }
    // Ready-wait keys on the PID-file/busy EVENT (ADR 0005 — zero blind
    // keystrokes), reusing the A2 boot waiter.
    let sleeper = RealSleeper;
    let waiter = EventBootWaiter::new(
        mux,
        canonical.to_path_buf(),
        paths.sessions_dir.clone(),
        clock,
        &sleeper,
    );
    if let Err(failure) = waiter.wait_ready(zmx_name) {
        // NAMED DIVERGENCE (loud>silent, ADD-9a). The Rust ready-wait is the
        // ADR-0005 EVENT waiter, so a timeout genuinely means "boot did not
        // confirm" → exit 1.
        return Err(ReviveClaudeError::BootUnconfirmed {
            detail: failure.detail,
        });
    }
    Ok(())
}

/// Phase 2 — clear any stale same-name pane, then run the detached launch + the
/// ready-wait. On success returns the [`ReviveHandle`] so the caller can attach
/// the live pane with a plain `mux.attach`.
///
/// On a [`ClaudeRevivePlan::SeedLivePane`] plan (punch R21) NONE of that runs:
/// there is a live pane already, so the work is the opening send and the handle
/// points at the pane that was there. Both arms answer the same
/// [`ReviveHandle`] because the caller's next move is the same either way —
/// `mux.attach` on the coordinates it gets back.
pub fn run_claude_revive(
    deps: &ClaudeLaunchDeps<'_>,
    clock: &dyn Clock,
    plan: &ClaudeRevivePlan,
) -> Result<ReviveHandle, ReviveClaudeError> {
    let (zmx_name, claude_cmd, cwd) = match plan {
        ClaudeRevivePlan::Relaunch {
            zmx_name,
            claude_cmd,
            cwd,
        } => (zmx_name, claude_cmd, cwd),
        // PUNCH R21. The pane is up; give it its first turn and hand it over.
        ClaudeRevivePlan::SeedLivePane {
            zmx_name,
            socket_dir,
        } => {
            // Bug D: the dir the pane was FOUND in wins over the caller's
            // canonical one, which may be a different socket root entirely.
            let dir = socket_dir
                .clone()
                .unwrap_or_else(|| deps.canonical_dir.clone());
            seed_live_pane(deps.mux, clock, &dir, zmx_name, deps.paths);
            return Ok(ReviveHandle {
                socket_dir: dir,
                zmx_name: zmx_name.clone(),
            });
        }
    };

    // r6 F1: the SAFE stale-pane clear.
    clear_stale_panes(deps.mux, &deps.scan_dirs, zmx_name).map_err(ReviveClaudeError::StalePane)?;

    // Detached revive + ready-wait via the SHARED seam.
    run_detached_revive(
        deps.mux,
        &deps.canonical_dir,
        zmx_name,
        claude_cmd,
        cwd,
        deps.paths,
        clock,
    )?;

    Ok(ReviveHandle {
        socket_dir: deps.canonical_dir.clone(),
        zmx_name: zmx_name.clone(),
    })
}

/// PUNCH R21 — type [`crate::onboarding::HOW_TO_REACH_PEERS`] into a live but
/// never-messaged claude pane, so the turn that onboards the agent is also the
/// turn that mints the provider session id the row was missing.
///
/// ── THE SEND IS BORROWED, NOT INVENTED ──────────────────────────────────────
/// [`crate::submit::deliver_prompt`] over [`crate::submit::RealDeliverDeps`] is
/// the SAME seam `qd start -p` primes a freshly-booted pane through
/// (`delivery::priming`), and it is the right one for exactly the reason that
/// path exists: it is addressed by NAME, not by id — "at `-p` time the provider
/// uuid may not have been written yet", which is this row's whole condition. It
/// owns the ADR-0009 two-write shape (chunked text, settle, a SEPARATE `\r`),
/// the content-verified remediation CR, and the bounded went-busy retry, none of
/// which should be re-derived here.
///
/// The five carriers in [`crate::delivery`] were the other candidates and every
/// one of them refuses this row before it writes a byte: `send_mux_pty` refuses
/// `status == Cold`, and a ZmxOnly row is Cold by the port's literal even while
/// its process runs.
///
/// ── WHY NO OUTCOME HERE IS AN ERROR ─────────────────────────────────────────
/// `deliver_prompt` sends FIRST and only then looks for the pid file, so the
/// message — and therefore the id mint — has already happened by the time any of
/// its three outcomes exists. What the outcomes describe is SUBMIT ACCEPTANCE:
/// `PidFileMissing` means no registry row appeared within the poll (the normal
/// reading for a session whose row is being written for the first time by the
/// turn we just started), `Stalled` means acceptance was never observed. Neither
/// is a reason to withhold the terminal the user asked for — they are about to
/// LOOK at this pane, which is a better report than anything this function could
/// return, and this module prints nothing by contract. So the send is made and
/// the handle is returned; the failure this arm can actually have — the mux
/// refusing the write — surfaces where every mux failure does, as an attach that
/// lands on a pane that did not move.
fn seed_live_pane(
    mux: &dyn Mux,
    clock: &dyn Clock,
    socket_dir: &Path,
    zmx_name: &str,
    paths: &QdPaths,
) {
    let sleeper = RealSleeper;
    let deliver = crate::submit::RealDeliverDeps {
        mux,
        clock,
        sleeper: &sleeper,
        zmx_name: zmx_name.to_string(),
        // A ZmxOnly row's registry key, if one ever appears, is keyed on the same
        // name the pane carries — there is no other name to look it up by.
        session_name: zmx_name.to_string(),
        sessions_dir: paths.sessions_dir.clone(),
        dir: socket_dir.to_path_buf(),
    };
    let _ = crate::submit::deliver_prompt(
        &deliver,
        crate::onboarding::HOW_TO_REACH_PEERS,
        crate::submit::DELIVER_TIMEOUT_S,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Session, SessionBranch, SessionStatus};

    /// A cold claude registry row carrying a RECORDED `session_id` — the durable
    /// identity the daemon minted onto the child-pid-keyed row (WP-B5-ii-b PROOF 1).
    fn cold_claude_row(session_id: &str) -> Session {
        Session {
            name: Some("wk".to_string()),
            user_named: Some(true),
            session_id: session_id.to_string(),
            code: None,
            qd_id: None,
            pid: None,
            status: SessionStatus::Cold,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: Some("headless".to_string()),
            lineage: None,
            hosting: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    /// WP-B5-ii-b PROOF 1 (cheap default-floor mirror of the `#[ignore]`
    /// end-to-end seed `headless_revive_recorded_id.rs`): a cold row's revive argv
    /// fragment carries `--resume <recorded session_id>`. The recorded id is
    /// LOAD-BEARING — revive resumes the SAME claude session, never a fresh one,
    /// and never a fork (a fork would mint a new session id / lose continuity).
    ///
    /// FIX-SHAPED MUTATION (red-before): in `revive_resume_args`, replace
    /// `id: &session.session_id` with `id: ""` → the fragment becomes
    /// `["--resume", ""]` → the recorded-id equality assert reds (revive would
    /// start a fresh claude session, not resume the recorded one).
    #[test]
    fn revive_resumes_via_recorded_session_id() {
        let provider = crate::provider::provider_for("claude-code").unwrap();
        let sid = "fa4ec110-0000-4000-8000-000000000001";
        let args = revive_resume_args(provider, &cold_claude_row(sid));
        assert_eq!(
            args,
            vec!["--resume".to_string(), sid.to_string()],
            "revive must pass the recorded session_id as `--resume <id>` (resumed, not fresh)"
        );
        assert!(
            !args.iter().any(|a| a == "--fork-session"),
            "revive resumes the recorded session, never forks a fresh one: {args:?}"
        );
    }

    /// R2 (override-never-inherit, the D1-site-4 pattern on the REVIVE path):
    /// an --alt-screen resume/attach-revive writes an env file carrying the
    /// EXPLICIT `unset -v CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN` (omitting the
    /// export alone would leave the child inheriting the var from an inline
    /// parent env), the unset PRECEDES the identity export, and the claude cmd
    /// dot-sources the file. An inline revive carries the export and NO unset.
    #[test]
    fn alt_screen_revive_env_file_carries_explicit_unset() {
        use crate::launch::RenderMode;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let paths = crate::paths::QdPaths::from_home(&home);
        // No env fixture any more: `prepare_claude_resume_env` no longer reads the
        // environment at all — the ids-store path is resolved by the caller and
        // arrives as an argument, which is the only thing it used `env` for.

        // AltScreen revive: unset-first file, identity export still rides.
        let ids_path = crate::idstore::ids_path(&paths.state_dir);
        let cmd = prepare_claude_resume_env(
            &home,
            &paths,
            &ids_path,
            &crate::effects::FixedClock(0),
            "wk",
            "uuid-1",
            Some("wk"),
            vec![],
            RenderMode::AltScreen,
            "command 'claude'",
        )
        .unwrap_or_else(|e| panic!("alt-screen revive prep: {}", e.line("resume")));
        let body =
            std::fs::read_to_string(crate::launch::session_env_file_path(&home, "wk")).unwrap();
        let unset_pos = body
            .find("unset -v CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN")
            .unwrap_or_else(|| panic!("explicit unset must ride the revive file: {body}"));
        let id_pos = body
            .find("export QD_SESSION_ID=")
            .unwrap_or_else(|| panic!("identity export must still ride: {body}"));
        assert!(unset_pos < id_pos, "unset precedes the exports: {body}");
        assert!(
            !body.contains("export CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"),
            "alt-screen never exports the inline var: {body}"
        );
        assert!(
            cmd.contains(".quorum/dispatch/session-env/wk.env")
                && cmd.ends_with("command 'claude'"),
            "the cmd dot-sources the file then runs claude: {cmd}"
        );

        // Inline revive: export (explicit set clobbers inherited), NO unset.
        prepare_claude_resume_env(
            &home,
            &paths,
            &ids_path,
            &crate::effects::FixedClock(0),
            "wk2",
            "uuid-2",
            Some("wk2"),
            vec![],
            RenderMode::Inline,
            "command 'claude'",
        )
        .unwrap_or_else(|e| panic!("inline revive prep: {}", e.line("resume")));
        let body2 =
            std::fs::read_to_string(crate::launch::session_env_file_path(&home, "wk2")).unwrap();
        assert!(
            body2.contains("export CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN='1'"),
            "{body2}"
        );
        assert!(
            !body2.contains("unset -v"),
            "inline needs no unset: {body2}"
        );
    }

    // --- punch item 6: revive launch failures fail IMMEDIATELY (never a
    // boot-wait timeout) and carry zmx's stderr. The mux below panics on any
    // boot-waiter verb (history/list/send), so a swallow-into-boot-wait
    // regression panics the test instead of passing slowly.

    struct FailingMux {
        /// Ok(nonzero+stderr) models a failed `zmx run`; Err models a spawn
        /// failure (zmx not on PATH).
        spawn_err: bool,
    }
    impl crate::mux::Mux for FailingMux {
        fn run_detached(
            &self,
            _d: &std::path::Path,
            _n: &str,
            _c: &str,
            _w: &std::path::Path,
        ) -> std::io::Result<crate::exec::ExecResult> {
            if self.spawn_err {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "ENOENT"))
            } else {
                Ok(crate::exec::ExecResult {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "zmx: cannot create session: boom\n".to_string(),
                    timed_out: false,
                })
            }
        }
        fn list(&self, _d: &std::path::Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
            unreachable!("a failed launch must NEVER reach the boot waiter")
        }
        fn list_raw(
            &self,
            _d: &std::path::Path,
        ) -> std::io::Result<Vec<crate::mux::MuxSession>> {
            unreachable!("a failed launch must NEVER reach the boot waiter")
        }
        fn send(
            &self,
            _d: &std::path::Path,
            _n: &str,
            _t: &str,
        ) -> std::io::Result<crate::exec::ExecResult> {
            unreachable!("a failed launch must NEVER reach the boot waiter")
        }
        fn kill(&self, _d: &std::path::Path, _n: &str) -> std::io::Result<i32> {
            unreachable!()
        }
        fn history(&self, _d: &std::path::Path, _n: &str) -> std::io::Result<String> {
            unreachable!("a failed launch must NEVER reach the boot waiter")
        }
        fn wait(&self, _d: &std::path::Path, _n: &[String]) -> std::io::Result<i32> {
            unreachable!()
        }
        fn attach(&self, _d: &std::path::Path, _n: &str) -> std::io::Result<i32> {
            unreachable!()
        }
    }

    #[test]
    fn revive_nonzero_zmx_run_fails_immediately_with_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::QdPaths::from_home(tmp.path());
        let mux = FailingMux { spawn_err: false };
        let err = run_detached_revive(
            &mux,
            tmp.path(),
            "wk",
            "command 'claude'",
            tmp.path(),
            &paths,
            &crate::effects::FixedClock(0),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        // (The FailingMux's unreachable!() boot-waiter verbs are the proof the
        // failure never degraded into a boot wait.)
    }

    #[test]
    fn revive_spawn_err_fails_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::QdPaths::from_home(tmp.path());
        let mux = FailingMux { spawn_err: true };
        let err = run_detached_revive(
            &mux,
            tmp.path(),
            "wk",
            "command 'claude'",
            tmp.path(),
            &paths,
            &crate::effects::FixedClock(0),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    /// Wording pins: the nonzero-exit line carries zmx's (trimmed) stderr; the
    /// spawn-failure line is the missing-binary guidance.
    ///
    /// Moved here from the verb with the lines themselves, which are now
    /// `ReviveClaudeError::line` arms (the `NewError` precedent). The launch
    /// failure is NOT verb-attributed and never was, so it ignores the verb; the
    /// zmx-missing line IS, and the third assert pins the fix — the pre-split
    /// helper hard-coded `qd resume:` even when `qd attach` was what ran.
    #[test]
    fn revive_failure_lines_are_pinned() {
        assert_eq!(
            ReviveClaudeError::LaunchFailed {
                stderr: "zmx: cannot create session: boom\n".to_string()
            }
            .line("resume"),
            "Failed to resume session: zmx: cannot create session: boom"
        );
        assert_eq!(
            ReviveClaudeError::ZmxMissing.line("resume"),
            "qd resume: could not launch zmx (is it installed and on PATH?)."
        );
        assert_eq!(
            ReviveClaudeError::ZmxMissing.line("attach"),
            "qd attach: could not launch zmx (is it installed and on PATH?).",
            "the verb is the CALLER's — the pre-split helper always said 'resume'"
        );
    }

    /// ORC CONDITION (i), ack3-spec §8: the `--no-attach` boot-confirm-failure
    /// stderr line is a NAMED ADD-9a divergence contract — m-4's retype of
    /// `wait_ready` to a typed `BootFailure` must NOT drift this wording. Pins the
    /// EXACT pre-m-4 strings for both boot phases' detail forms (the phase is NOT
    /// in this line — only the waiter detail is, exactly as before).
    #[test]
    fn resume_boot_unconfirmed_line_is_byte_identical_both_phases() {
        // Idle-phase detail (boot.rs run_idle_phase wording).
        assert_eq!(
            ReviveClaudeError::BootUnconfirmed {
                detail: "session \"wk\" did not reach idle status within timeout".to_string()
            }
            .line("resume"),
            "qd resume: session launched but did not confirm ready: \
             session \"wk\" did not reach idle status within timeout"
        );
        // PID-file-phase detail (boot.rs run_pid_phase wording).
        assert_eq!(
            ReviveClaudeError::BootUnconfirmed {
                detail: "PID file for \"wk\" did not appear within 40000ms — qd attach wk to inspect"
                    .to_string()
            }
            .line("resume"),
            "qd resume: session launched but did not confirm ready: \
             PID file for \"wk\" did not appear within 40000ms — qd attach wk to inspect"
        );
    }

    /// D4's exact error wording, pinned (spec-w2-env: "name \"wk\" is held by
    /// running session <id>; rename or stop it first").
    ///
    /// Moved here from the binary's `verbs::common` with the guard itself. The
    /// line is now a `ReviveClaudeError::line` arm, and the verb is the CALLER's:
    /// the pre-split `revive_claude` passed a hard-coded "attach" from every one
    /// of its five callers, so `qd resume` reported an `attach` failure.
    #[test]
    fn held_name_error_line_is_pinned() {
        let e = ReviveClaudeError::NameHeldLive {
            zmx_name: "wk".to_string(),
            holder: "ab3kx9mq".to_string(),
        };
        assert_eq!(
            e.line("resume"),
            "qd resume: name \"wk\" is held by running session ab3kx9mq; \
             rename or stop it first"
        );
        assert_eq!(
            e.line("attach"),
            "qd attach: name \"wk\" is held by running session ab3kx9mq; \
             rename or stop it first"
        );
    }

    // ── punch R21: a never-messaged pane is seeded, not refused ──────────────
    //
    // The decision under test is [`live_pane_to_seed`], which is where the
    // "no session id" fork now happens. It is pure over the row, so these run on
    // the default floor with no mux, no fs and no clock — the same shape the
    // recorded-id proof above takes.

    /// The row shape the join builds for a claude session that was STARTED and
    /// never messaged: a live mux pane, and nothing else. No transcript (claude
    /// writes one on its first turn), so no session id and no registry match —
    /// `join.rs`'s ZmxOnly branch, verbatim: `session_id: ""`, `user_named: None`,
    /// `status: Cold` even though the pid is alive.
    fn zmx_only_row(pid: i64) -> Session {
        Session {
            name: Some("wk".to_string()),
            user_named: None,
            session_id: String::new(),
            code: None,
            qd_id: None,
            pid: Some(pid),
            status: SessionStatus::Cold,
            zmx_name: Some("wk".to_string()),
            zmx_clients: Some(0),
            socket_dir: Some("/tmp/zmx-501".to_string()),
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: Some("/tmp".to_string()),
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            hosting: None,
            which_branch: SessionBranch::ZmxOnly,
        }
    }

    /// R21, the fix: a row with no provider session id but a LIVE pane plans a
    /// seed — addressed at the pane's own name, in the socket dir the pane was
    /// FOUND in (Bug D), not at a name derived from the empty id.
    #[test]
    fn never_messaged_live_pane_plans_a_seed() {
        // The test process is the one pid this suite can prove alive.
        let me = std::process::id() as i64;
        match live_pane_to_seed(&zmx_only_row(me)) {
            Some(ClaudeRevivePlan::SeedLivePane {
                zmx_name,
                socket_dir,
            }) => {
                assert_eq!(zmx_name, "wk", "the pane is addressed by ITS name");
                assert_eq!(
                    socket_dir,
                    Some(PathBuf::from("/tmp/zmx-501")),
                    "per-session ops target the dir the pane was found in (Bug D)"
                );
            }
            other => panic!("a live never-messaged pane must plan a seed, got {other:?}"),
        }
    }

    /// R21's other half, and the one that keeps the refusal honest: the pane is
    /// GONE. `Cannot resume: no session ID found.` is still the right answer —
    /// there is no id, and no process to type at that would mint one.
    ///
    /// FIX-SHAPED MUTATION (red-before): drop the `is_pid_alive` conjunct from
    /// `live_pane_to_seed` → this row plans a seed into a dead pane → red.
    #[test]
    fn dead_pane_is_still_no_session_id() {
        // Far above any pid this host will allocate; kill(pid, 0) answers ESRCH.
        assert!(
            live_pane_to_seed(&zmx_only_row(2_147_483_646)).is_none(),
            "a dead pane cannot be seeded — the refusal stands"
        );
    }

    /// A cold registry row with no id and no pane at all (the pre-R21 shape this
    /// refusal was written for) is untouched by the narrowing.
    #[test]
    fn row_with_no_pane_is_still_no_session_id() {
        let mut row = zmx_only_row(1);
        row.pid = None;
        row.zmx_name = None;
        row.name = None;
        assert!(
            live_pane_to_seed(&row).is_none(),
            "no pane, no pid, no name — nothing to seed"
        );
    }

    /// The refusal's WORDING did not move with the narrowing. It is
    /// self-attributed (no `qd <verb>:` prefix) and byte-identical to the line
    /// `qd attach` printed before R21 — a user who still hits it reads exactly
    /// what they read yesterday.
    #[test]
    fn no_session_id_line_is_unchanged_and_self_attributed() {
        assert!(ReviveClaudeError::NoSessionId.is_self_attributed());
        assert_eq!(
            ReviveClaudeError::NoSessionId.line("attach"),
            "Cannot resume: no session ID found."
        );
    }

    /// The opening message is the SHARED wording, not a second copy of it. If
    /// this ever stops being `onboarding::HOW_TO_REACH_PEERS`, the seed and the
    /// relay MCP instructions are teaching two different stories — which is the
    /// exact drift the shared macro exists to prevent (R9/R21 "shares its
    /// wording").
    #[test]
    fn the_seed_message_is_the_shared_onboarding_wording() {
        let msg = crate::onboarding::HOW_TO_REACH_PEERS;
        assert!(msg.contains("qd ls"), "{msg}");
        assert!(msg.contains("qd send:relay <session>"), "{msg}");
    }
}
