//! `qd new` create pipeline (spec §6) — a testable decider + injected effects.
//!
//! Port of the `qd new` action (lifecycle.ts:707-809) + `startDetached`
//! (utils.ts:348-375), with TWO hardenings that intentionally DIVERGE from TS:
//!
//!   - **HARDENING #1 — fail-closed `--agent`** (spec §6.2): TS's `qd new`
//!     BLINDLY forwards `--agent X` to claude (lifecycle.ts:732 via
//!     `buildNewExtraArgs`), so an unknown agent boots a GENERIC session that
//!     goes busy → a false success. We resolve `<agents_dir>/<X>.md` BEFORE
//!     creating anything and refuse to boot if it is missing. This mirrors the
//!     posture/wording of the retired `spawn` verb's agent-def fail-closed check
//!     (spawn.ts:213-230) — the prior art for this divergence.
//!
//!   - **HARDENING #2 — atomic name claim** (spec §6.3): TS relies on a
//!     check-then-create live-name pre-check (racy). We add a race-free
//!     `registry::claim_name` O_EXCL claim that closes the create window; the
//!     TS pre-check semantics survive ONLY as a cheap UX fast-path.
//!
//! Order of operations (each step names its failure mode; EVERY failure exits
//! nonzero and leaves NO state — or reaps exactly what it created):
//!   1. preflight (L3)  → [`NewError::PreflightStale`]
//!   2. (retired) static `--agent` path — `qd start --agent` is refused at
//!      the verb layer now; role content lives in the work-model plugin.
//!   3. atomic name claim (#2) → [`NewError::NameClaimed`]
//!      (live-name pre-check fast-path → [`NewError::NameInUse`])
//!   4. start_detached → [`NewError::ZmxMissing`] / [`NewError::ZmxRunFailed`]
//!   5. I6 verify → [`NewError::NotAttachable`] / [`NewError::SocketDirSplit`]
//!   6. boot wait → [`NewError::BootTimeout`]
//!
//! The claim is held by an RAII guard for the WHOLE window: it is released on
//! EVERY exit path after acquisition (success AND failure). The durable record
//! is the session's registry row / PID file; the claim only closes the
//! check-then-create gap, so it is correct to drop it once `run_new` returns.

use std::path::PathBuf;

use crate::effects::{Clock, Env};
use crate::exec::Exec;
use crate::launch::{
    build_claude_cmd_from_argv, launch_env_pairs, remove_session_env_file, render_env_unsets,
    session_env_prefix, write_session_env_file_with_unsets, RenderMode,
};
use crate::mux::Mux;
use crate::paths::QdPaths;
use crate::preflight;
use crate::provider::{LaunchRequest, Provider, ProviderFx};
use crate::registry::{self, ClaimError, NameClaim};

/// Boot-readiness seam (spec §6 step 6 / §8). M2 ships ONLY this trait + a
/// trivial fixture; M3 ships the real PID-file + went-busy waiter. `run_new`
/// calls [`BootWaiter::wait_ready`] AFTER the I6 verify; an `Err` maps to the
/// ported boot-failure error text (lifecycle.ts:782-789) + a nonzero exit.
pub trait BootWaiter {
    /// Block until the named session is ready, or return `Err(failure)` on
    /// timeout. The [`BootFailure`](crate::boot::BootFailure) carries the TYPED
    /// boot phase + the detail string carried into [`NewError::BootTimeout`]
    /// (m-4, ack3-spec §8 — phase is typed from the source, never re-derived by
    /// string-matching the detail downstream).
    fn wait_ready(&self, name: &str) -> Result<(), crate::boot::BootFailure>;
}

/// A boot waiter that always succeeds — the default for create-path units that
/// only exercise the steps BEFORE boot. M3 replaces this with the real waiter.
pub struct OkBootWaiter;

impl BootWaiter for OkBootWaiter {
    fn wait_ready(&self, _name: &str) -> Result<(), crate::boot::BootFailure> {
        Ok(())
    }
}

/// Injected effects + resolved paths for one `run_new` call.
pub struct NewDeps<'a> {
    /// The mux (real `ZmxMux` in production; `FixtureMux` in units).
    pub mux: &'a dyn Mux,
    /// The exec seam — used ONLY by the preflight probe here (the mux owns all
    /// zmx drive verbs). Kept explicit so a unit can assert the probe ran.
    pub exec: &'a dyn Exec,
    /// Env for flags resolution + agent-dir precedence (L9a: never bare homedir).
    pub env: &'a dyn Env,
    /// Clock for the claim payload timestamp.
    pub clock: &'a dyn Clock,
    /// Home→state layout (L9a). `claims_dir` is derived from `sessions_dir`'s
    /// parent (the `.claude` root) so the claim lives beside the registry.
    pub paths: &'a QdPaths,
    /// The canonical zmx socket dir (from `resolve_zmx_dir`) — the dir the
    /// session is created in AND reaped-at-canonical in (Bug D keystone, L1).
    pub canonical_dir: PathBuf,
    /// Legacy candidate socket dirs (from `legacy_zmx_dirs`). The I6 verify +
    /// live-name pre-check scan canonical THEN these (canonical-wins dedupe),
    /// matching TS `getZmxSessions` (session.ts:273-291): a session that landed
    /// in a non-canonical dir (socket-dir split, Bug D) is found HERE, carrying
    /// that dir as its `socket_dir`, so I6 can detect + reap the split.
    pub legacy_dirs: Vec<PathBuf>,
    /// The boot-readiness waiter (M3's real impl in production).
    ///
    /// codex P1 W3 (codex-p1-spec section 7.1 step 4): in PRODUCTION the bin verb
    /// now obtains this from `provider.boot_waiter(fx)` (the SAME EventBootWaiter,
    /// wired from the same mux/clock/sleeper/socket_dir/sessions_dir) and passes
    /// the box's `&dyn BootWaiter` here — so the boot wait routes THROUGH the seam
    /// at the construction site, while create.rs keeps driving the injected
    /// `BootWaiter` trait exactly as before (the box satisfies it). Units inject a
    /// trivial waiter directly (the create-path deciders never construct one).
    pub boot_waiter: &'a dyn BootWaiter,
    /// codex P1 W3 (codex-p1-spec section 7.1 steps 2-3): the resolved provider
    /// this create drives. The bin verb resolves it ONCE via
    /// `provider::provider_for` from the validated `--provider` value (absent ⇒
    /// "claude-code"); the launch cmd is built through `provider.launch_plan(fx,
    /// req)` then the SAME shell-assembly (`build_claude_cmd_from_argv`), so
    /// claude's assembled cmd is BYTE-IDENTICAL to the pre-rewire
    /// `build_claude_cmd(bin, flags, extra)` (the existing create/launch unit pins
    /// prove it; `provider_routed_cmd_equals_prerewire_assembly` pins it directly).
    pub provider: &'a dyn Provider,
    /// The selected mux backend (C1 M4fix). The launch-failure error path names
    /// the ACTUAL backend: an embedded daemon-launch failure must say "qrmux
    /// daemon failed to launch" (+ underlying error), not the zmx-lane
    /// missing-binary guidance. The zmx-lane text stays byte-stable (G-NEG).
    pub backend: crate::mux_selector::Backend,
}

impl NewDeps<'_> {
    /// List sessions across canonical THEN legacy dirs, deduped canonical-wins
    /// (port of `getZmxSessions`'s cross-dir scan, session.ts:273-291). Each row
    /// is tagged with the dir it was FOUND in (`socket_dir`), so a split is
    /// detectable. A missing/erroring dir contributes nothing (Mux::list →
    /// Ok([]) on a gone dir).
    fn scan_all(&self) -> Vec<crate::mux::MuxSession> {
        let mut scans: Vec<(PathBuf, Vec<crate::mux::MuxSession>)> = Vec::new();
        scans.push((
            self.canonical_dir.clone(),
            self.mux.list(&self.canonical_dir).unwrap_or_default(),
        ));
        for d in &self.legacy_dirs {
            scans.push((d.clone(), self.mux.list(d).unwrap_or_default()));
        }
        crate::mux::merge_canonical_wins(scans)
    }

    /// The claims dir: `<.claude>/claims`, alongside `sessions/`. Derived from
    /// `sessions_dir`'s parent so the claim shares the registry's state root
    /// (L9a: rooted at the injected home, never the real one).
    pub fn claims_dir(&self) -> PathBuf {
        self.paths
            .sessions_dir
            .parent()
            .map(|p| p.join("claims"))
            // sessions_dir is always `<home>/.claude/sessions`, so a parent
            // always exists; the fallback only guards a degenerate root path.
            .unwrap_or_else(|| self.paths.home.join(".claude").join("claims"))
    }
}

/// Parameters for one `qd new` (mirrors the TS action's args + opts).
pub struct NewParams {
    /// The zmx session name (also the claude --name).
    pub name: String,
    /// `--agent X`, if given. Triggers HARDENING #1 fail-closed resolution.
    pub agent: Option<String>,
    /// The fork-source provider UUID + fork marker, carried into
    /// `buildNewExtraArgs` (`--resume <uuid>` + `--fork-session`). P0
    /// start-surface rework (STATE 21): the verb layer only ever sets these
    /// TOGETHER — `start <name> --fork <session>` resolves the target session
    /// and passes its UUID here with `fork: true`; a fresh start passes
    /// (None, false). The fields stay separate because the launch argv builder
    /// (and unit fixtures) compose them independently.
    pub resume: Option<String>,
    pub fork: bool,
    /// Pass-through claude args (everything after `--`).
    pub claude_args: Vec<String>,
    /// `--model <m>` launch flag (warranty #2). Emitted into the claude argv via
    /// `build_new_extra_args`; replaces the post-boot `/model` delivery (which
    /// polluted the global default + raced `-p`). None = inherit the account/
    /// settings default at launch.
    pub model: Option<String>,
    /// Working dir for the session (TS `opts.cwd || process.cwd()` — the CLI
    /// resolves the default; here it is always explicit).
    pub cwd: PathBuf,
    /// F1 / `--via` composed backend-env pairs (spec §2.2 + §3.2.3). The verb
    /// layer captures the caller's backend env (`capture_backend_env`,
    /// lifecycle.ts:874) and, when `--via` is given, overlays the resolved
    /// profile (`compose_via_env`). EMPTY ⇒ no env file, no prefix, byte-zero
    /// change (lifecycle.ts:875-879: empty capture → no file). The VALUE of any
    /// credential lives ONLY here in memory and in the 0600 file — never argv.
    pub backend_env: Vec<(String, String)>,
    /// A7 F12 fix: whitelist keys the env file must explicitly `unset -v` BEFORE
    /// the exports (a `--via` session's whitelisted env = EXACTLY the composed
    /// set; composed-pair removal alone leaves inherited/profile-re-exported
    /// values riding — observed live on Lima 2026-06-05). EMPTY for non-`--via`
    /// sessions (the F1 capture surface stays TS-parity, no unsets ever).
    pub backend_env_unset: Vec<String>,
    /// P0 wave-2 (spec-w2-env D1): the session's STABLE 8-char id, pre-minted by
    /// the verb layer BEFORE create (idstore `mint_unbound` — every start,
    /// fresh or forked, boots a session whose provider UUID does not exist yet;
    /// the STATE-21 rework removed the old `--resume` pre-bound `mint_or_get`
    /// arm with the flag). When `Some`, the per-session env file is written
    /// UNCONDITIONALLY and carries `export QD_SESSION_ID='<id>'` — an explicit
    /// set at every launch, overriding anything inherited through the
    /// commissioner's process subtree. `None` ⇒ legacy behavior (no injection;
    /// unit fixtures that don't exercise identity pass None).
    pub qd_session_id: Option<String>,
    /// punch item 7: the session's resolved render mode (per-start flag >
    /// `render-default` config > inline). [`RenderMode::Inline`] (the default)
    /// injects `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` as a launch-time birth
    /// property via the env file; [`RenderMode::AltScreen`] injects nothing.
    pub render: RenderMode,
}

/// Success outcome of [`run_new`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewOutcome {
    /// The created session name.
    pub name: String,
    /// The canonical socket dir it landed in.
    pub socket_dir: PathBuf,
    /// The assembled `command 'claude' ...` shell command (for diagnostics).
    pub claude_cmd: String,
}

/// Why `run_new` failed. EVERY variant maps to a nonzero exit ([`NewError::exit_code`])
/// and leaves NO state — or says exactly what it left: the I6 split variant
/// reaps the identity-positive wrong-dir row; the I6 absence variant reaps
/// NOTHING (punch item 17 — absence is not a kill target).
#[derive(Debug)]
pub enum NewError {
    /// §3.6 (redteam-retro #4 carry): the name carries a path separator (`/`,
    /// `\`), a `..` component, or a NUL byte. Rejected at the create boundary
    /// BEFORE the claim so distinct raw names can never collide on a sanitized
    /// claim stem AND a crafted name can never reach zmx. Carries the offending
    /// name + the class that tripped. Nothing created.
    NameRejected { name: String, reason: String },
    /// S2-at-new pin reconciliation (spec §2.1, lifecycle.ts:857-861): the name
    /// failed the S2 whitelist (`validateSessionName`). Carries the validation
    /// message; the Display emits the exact TS `ERROR: <msg>` shape. Nothing
    /// created (S2 runs before any FS/env op).
    NameUnsafeS2 { message: String },
    /// Preflight: zmx is recognizably too old to drive (`Capability::No`, L3).
    /// Carries the upgrade guidance. Nothing created.
    PreflightStale(String),
    /// P0 wave-2 (spec-w2-env D3, scan-under-claim): a LIVE registry session
    /// (alive pid, not tombstoned) already holds the name. Found by the
    /// UNDER-CLAIM scan, so it is authoritative (racers serialize at the claim).
    /// `holder` is the live session's display id (stable id when mapped, else
    /// the truncated provider UUID). Nothing created; the claim is released.
    NameHeldLive { name: String, holder: String },
    /// Scan-under-claim (spec-w2-env D3): a mux (zmx/qrmux) pane already holds
    /// the name with NO live registry row behind it. Previously the advisory
    /// PRE-claim check; now runs UNDER the claim, so it is authoritative.
    /// Nothing created; the claim is released.
    NameInUse(String),
    /// HARDENING #2: the atomic claim lost the race / the name is mid-create.
    /// Carries the holder's claim payload (best-effort). Nothing created.
    NameClaimed { name: String, holder: String },
    /// `start_detached`: zmx is not on PATH (ENOENT). Carries the missing
    /// guidance. Nothing created (the spawn never started). ZMX LANE ONLY — the
    /// embedded lane uses [`NewError::EmbeddedDaemonLaunchFailed`].
    ZmxMissing(String),
    /// `start_detached` (EMBEDDED lane, C1 M4fix): the qrmux daemon could not be
    /// launched / the create op failed. Carries the underlying error detail.
    /// Nothing durable created. Distinct from [`NewError::ZmxMissing`] so the
    /// message names the ACTUAL backend (the Lima delta: the generic
    /// "Failed to launch zmx" misled under embedded).
    EmbeddedDaemonLaunchFailed(String),
    /// `start_detached`: `zmx run` exited nonzero. Carries the trimmed stderr.
    /// Nothing durable created (zmx failed to register the task).
    ZmxRunFailed(String),
    /// punch item 18 (b3-kill-spec): a same-name ENDED pane — the pty-reuse
    /// cwd-hijack trap — was reaped pre-launch, but its row did not clear
    /// within the bounded wait. Launching into the dying-socket window loses
    /// the launch entirely or re-arms the hijack, so we refuse. Nothing was
    /// launched; the claim is released; retrying is safe.
    StaleEndedPane { name: String },
    /// I6: the session is NOT attachable in the canonical dir after creation
    /// (Bug D) — absent from every scan of the punch-17 retry budget. NOTHING
    /// was killed: absence carries no identity evidence (the old kill-by-name
    /// here could land on a healthy pane that registered one beat after the
    /// last scan). A genuinely stray wrapper is cleared by the next same-name
    /// start's identity-positive stale-pane machinery.
    NotAttachable { name: String, canonical: PathBuf },
    /// I6: the session registered in a DIFFERENT dir than canonical (socket-dir
    /// split, Bug D). It was REAPED at the dir it was found in before returning.
    SocketDirSplit {
        name: String,
        found: PathBuf,
        canonical: PathBuf,
    },
    /// Boot wait timed out. Carries the TYPED boot `phase` (m-4, ack3-spec §8)
    /// plus the waiter's detail. The session exists but did not reach readiness
    /// (TS lifecycle.ts:782-789 — NOT reaped: the user is told to attach/inspect
    /// it). `phase` is NOT printed by `Display` (the human surface is unchanged);
    /// it exists so consumers read the phase typed instead of string-matching.
    BootTimeout {
        name: String,
        phase: crate::boot::BootPhase,
        detail: String,
    },
    /// F1 (spec §2.2): the per-session env file could not be written (e.g. the
    /// state dir is unwritable). Fail closed BEFORE launch — never boot a session
    /// that would silently miss its backend env. Nothing created. Carries the
    /// io detail; NEVER a credential value (only the captured KEY NAMES + io error
    /// reach this string — values are never formatted here).
    EnvFileWriteFailed { name: String, detail: String },
}

impl NewError {
    /// Process exit code for this error. ALL create-path failures are exit 1
    /// (TS `process.exit(1)` everywhere in this action) — the variant set exists
    /// for testability + precise stderr, not for distinct exit codes.
    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl std::fmt::Display for NewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // §3.6: actionable, names the rejected class. Narrows accepted inputs
            // vs TS (which passes such names through to zmx) — ADR 0007 Class 4.
            NewError::NameRejected { name, reason } => write!(
                f,
                "qd start: name '{name}' is not allowed ({reason}). \
                 Choose a name without path separators, '..', or control characters. \
                 No session was created."
            ),
            // S2-at-new (lifecycle.ts:858): `ERROR: ${nameErr}` — exact TS shape.
            NewError::NameUnsafeS2 { message } => write!(f, "ERROR: {message}"),
            // Preflight guidance is already the full actionable message (L3).
            NewError::PreflightStale(g) => write!(f, "{g}"),
            // P0 wave-2 (spec-w2-env D3): the distinct under-claim live-holder
            // error — names the RUNNING session by id so the caller can stop or
            // address it. "Taken" means a LIVE holder; historical/cold names are
            // reusable (the scan skips tombstones + dead pids).
            NewError::NameHeldLive { name, holder } => write!(
                f,
                "qd start: name \"{name}\" is taken by running session {holder} — \
                 stop it or choose another name. No session was created."
            ),
            NewError::NameInUse(name) => {
                write!(
                    f,
                    "qd start: name '{name}' is already in use by a live session"
                )
            }
            NewError::NameClaimed { name, holder } => {
                // S3: the on-disk file is the ENCODED stem (case-folded +
                // percent-escaped), NOT the raw name — print the real basename
                // so `rm` works on a case-sensitive fs.
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
            NewError::ZmxMissing(g) => write!(f, "{g}"),
            // C1 M4fix: embedded-lane daemon launch failure names the ACTUAL
            // backend (the qrmux daemon), unlike the zmx-lane ZmxMissing guidance.
            NewError::EmbeddedDaemonLaunchFailed(detail) => write!(
                f,
                "qd start: the embedded qrmux daemon failed to launch ({detail}). \
                 No session was created."
            ),
            // Byte-parity with TS startDetached (utils.ts:371).
            NewError::ZmxRunFailed(err) => write!(f, "Failed to create session: {err}"),
            // punch item 18: refusing to launch into a dying same-name pty.
            NewError::StaleEndedPane { name } => write!(
                f,
                "qd start: a previous ended session still holds the zmx slot \"{name}\" \
                 and did not clear after reap — not launching into a dying pty (the \
                 cwd-hijack window). Retry in a moment, or inspect with: zmx ls. \
                 No session was created."
            ),
            // TS I6 verify shape (lifecycle.ts:762-766), with the punch-17
            // divergence: the trailing "Reaping the stray wrapper." is GONE —
            // this path no longer kills on absence (b3-kill-spec item 17).
            NewError::NotAttachable { name, canonical } => write!(
                f,
                "ERROR: Session \"{name}\" is not attachable in the zmx socket dir \
                 ({}) after creation — registration failed (Bug D).\n  \
                 Nothing was killed (absence is not a kill target); a stray wrapper, \
                 if any, is cleared by the next same-name start. Inspect with: qd ls",
                canonical.display()
            ),
            // Byte-parity with TS I6 verify (lifecycle.ts:771-774).
            NewError::SocketDirSplit {
                name,
                found,
                canonical,
            } => write!(
                f,
                "ERROR: Session \"{name}\" registered in {}, not the canonical dir {} \
                 — socket-dir split (Bug D).",
                found.display(),
                canonical.display()
            ),
            // Byte-parity with TS boot-failure (lifecycle.ts:782-787); `detail`
            // is the waiter's reason, appended for diagnostics. `phase` is TYPED
            // (m-4, ack3-spec §8) but NOT printed — the human surface is byte-stable.
            NewError::BootTimeout { name, detail, .. } => write!(
                f,
                "ERROR: Session \"{name}\" did not reach idle state within timeout.\n\
                 The zmx session exists but Claude Code may not have booted.\n  \
                 Check: qd ls\n  Attach: qd connect {name}\n  ({detail})"
            ),
            // F1 fail-closed (spec §2.2): `detail` is the io error only — no value.
            NewError::EnvFileWriteFailed { name, detail } => write!(
                f,
                "qd start: failed to write the session env file for \"{name}\": {detail}. \
                 No session was created."
            ),
        }
    }
}

impl std::error::Error for NewError {}

/// Assemble the BASE `command 'claude' ...` shell command (flags + extra args)
/// for this `qd new` — WITHOUT the F1 env-file prefix. Factored so `run_to_boot`
/// builds it once after the claim is held. The env prefix is layered on in
/// `run_to_boot` after the file is written (lifecycle.ts:879). Not public.
///
/// codex P1 W3 (codex-p1-spec section 7.1): this routes THROUGH the provider seam.
/// `provider.launch_plan(fx, req)` yields the PRE-QUOTE argv (`[bin, ...flags,
/// ...extra]`, the exact token list `build_claude_cmd` used to quote); the SAME
/// shell-assembly step (`build_claude_cmd_from_argv`) single-quotes it behind the
/// `command` builtin. For claude this is BYTE-IDENTICAL to the pre-rewire
/// `build_claude_cmd(claude_bin, claude_flags, build_new_extra_args)` — the
/// existing `happy_path_creates_and_boots` / golden create pins are the proof, and
/// the new `provider_routed_cmd_equals_prerewire_assembly` unit pins it directly.
fn build_claude_command(deps: &NewDeps, params: &NewParams) -> String {
    // The provider resolves its OWN launch surfaces (claude_bin + claude_flags +
    // build_new_extra_args) off `fx`; the F1 env-file mechanism stays a create.rs
    // concern (LaunchPlan.env is [] for claude — the prefix is layered on in
    // run_to_boot, not via the trait). `socket_dir`/`mux`/`clock`/`sleeper`/relay
    // are boot/inject-only and absent for the launch-cmd build (launch_plan reads
    // only env + paths).
    let fx = ProviderFx {
        env: &deps.env,
        paths: deps.paths,
        socket_dir: deps.canonical_dir.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        // codex-only transport; the claude create path never speaks app-server.
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    let req = LaunchRequest {
        name: params.name.clone(),
        cwd: Some(params.cwd.to_string_lossy().into_owned()),
        resume: params.resume.clone(),
        fork: params.fork,
        agent: params.agent.clone(),
        model: params.model.clone(),
        passthrough: params.claude_args.clone(),
    };
    let plan = deps.provider.launch_plan(&fx, &req);
    build_claude_cmd_from_argv(&plan.argv)
}

/// §3.6 name-reject (redteam-retro #4). Reject a session name that carries a
/// path separator (`/`, `\`), a `..` traversal component, or a NUL byte. Returns
/// `Some(reason)` with the tripped class, or `None` if the name is acceptable.
///
/// The `..` check matches any component that is exactly `..` (split on both `/`
/// and `\`), so `..`, `a/../b`, and `..\\x` are all caught while a benign name
/// like `my..thing` (no separator) is rejected too — the substring `..` is the
/// red-team's collision vector (`../../etc/passwd` → `etcpasswd`), so we reject
/// it wherever it appears, conservatively. Plain names are unaffected.
fn reject_unsafe_name(name: &str) -> Option<String> {
    if name.contains('\0') {
        return Some("contains a NUL byte".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Some("contains a path separator ('/' or '\\\\')".to_string());
    }
    if name.contains("..") {
        return Some("contains '..'".to_string());
    }
    None
}

/// S2-at-new PIN RECONCILIATION (spec §2.1; orc-4 ruled ADOPT 2026-06-05).
///
/// TS at pin validates EVERY `qd new` name against the S2 whitelist BEFORE any
/// FS/env op (lifecycle.ts:857-861), exiting 1 with `ERROR: <validateSessionName
/// msg>`. Rust's create path historically had only the looser
/// [`reject_unsafe_name`], so whitelist-failing names like `a'b` / `a b` booted
/// (a quote-injection / shell-break risk into the env-file prefix). This closes
/// that parity gap TO the pin.
///
/// Implementation: REUSE [`crate::resume::validate_session_name`] — its message
/// text already byte-matches `utils.ts:591-597` (it is the same ported S2 guard
/// the resume path uses), so there is one S2 implementation, not two.
///
/// Isolated in this ONE function with ONE call site (run_new step 0a) so the
/// placement is auditable. Returns the validation message (caller prefixes
/// `ERROR: ` per lifecycle.ts:858).
fn s2_validate_new_name(name: &str) -> Option<String> {
    crate::resume::validate_session_name(name)
}

/// Run the `qd new` create pipeline (spec §6). See the module docs for the order
/// of operations and the claim-release discipline.
pub fn run_new(deps: &NewDeps, params: &NewParams) -> Result<NewOutcome, NewError> {
    // --- Step 0a: S2-at-new whitelist (spec §2.1, lifecycle.ts:857-861) ------
    // PIN RECONCILIATION (orc-4 ruled ADOPT): S2 runs FIRST, before ANY FS/env
    // op, exactly as TS. Names like `a'b` / `a b` that booted under the looser
    // gate below now exit 1 here with the ported `ERROR: <msg>` wording. Single
    // call site of [`s2_validate_new_name`] (the placement is auditable).
    if let Some(message) = s2_validate_new_name(&params.name) {
        return Err(NewError::NameUnsafeS2 { message });
    }

    // --- Step 0b: name-reject carry (§3.6, redteam-retro #4) -----------------
    // KEPT AFTER S2 as LIVE defense-in-depth, NOT dead code: the S2 whitelist
    // ALLOWS dots, so the `..`-family (`..`, `a..b`) PASSES S2 and is caught ONLY
    // here. Rust rejecting `..`-class names that pin TS accepts is an EXISTING
    // safety-motivated divergence (redteam-retro #4 carry) — it keeps a name like
    // `../../etc/passwd` from ever reaching the claim/zmx surface and removes the
    // distinct-name-same-stem footgun. (S2's charset gate is otherwise a strict
    // superset of separator/NUL rejection, so those classes never reach here.)
    if let Some(reason) = reject_unsafe_name(&params.name) {
        return Err(NewError::NameRejected {
            name: params.name.clone(),
            reason,
        });
    }

    // --- Step 1: Preflight (L3, spec §6.1) -----------------------------------
    // `No` (recognizably too old) → fail fast. `Unknown` (missing/garbage) falls
    // through to the real `zmx run` failure path below — NEVER a false "too old".
    // `deps.exec` is `&dyn Exec`; the blanket `impl Exec for &T` (exec.rs) lets
    // it satisfy `assert_zmx_capable`'s `&impl Exec`.
    if let Err(guidance) = preflight::assert_zmx_capable(&deps.exec) {
        return Err(NewError::PreflightStale(guidance));
    }

    // --- (Retired) static `--agent` fail-closed path -------------------------
    // The old HARDENING #1 step resolved `<agents_dir>/<agent>.md` here and
    // fail-closed booted that role. `qd start --agent` is now REFUSED at the verb
    // layer (verbs/stubs.rs::run_start_agent_retired) — role/agent content lives
    // in the work-model plugin and is spawned via `frame commission`, so the engine
    // no longer owns or resolves that agents dir. `params.agent` therefore never
    // arrives `Some` from the CLI; the launch-argv plumbing (NewOpts.agent →
    // build_new_extra_args) is retained as a generic builder seam, not a static
    // dependency on a deployed agents directory.

    // --- Step 3: HARDENING #2 — name claim, then SCAN-UNDER-CLAIM ------------
    // (P0 wave-2, spec-w2-env D3.) The atomic claim FIRST: the single point
    // where exactly one concurrent racer wins the name (O_EXCL open). Held by
    // an RAII guard for the WHOLE window; released on EVERY exit path after
    // this point.
    let payload = claim_payload(deps, &params.name);
    // P0 redfix F2: the real pid-liveness predicate — a stale claim whose holder
    // died (SIGKILL mid-boot; ClaimGuard never ran) is reaped instead of
    // bricking the name.
    let is_alive = |pid: i64| crate::effects::is_pid_alive(pid as i32);
    // B4 item 10: the exec-proof start-time probe — a live pid whose occupant
    // started after the claimed start is a recycled pid, reaped as stale.
    let proc_start = |pid: i64| crate::effects::proc_start_ms(pid as i32);
    let claim = match registry::claim_name(
        &deps.claims_dir(),
        &params.name,
        payload.as_bytes(),
        &is_alive,
        &proc_start,
    ) {
        Ok(c) => ClaimGuard::new(c),
        Err(ClaimError::AlreadyClaimed { existing_payload }) => {
            let holder = String::from_utf8_lossy(&existing_payload).into_owned();
            return Err(NewError::NameClaimed {
                name: params.name.clone(),
                holder,
            });
        }
        Err(ClaimError::Io(e)) => {
            // An unexpected claim I/O error (e.g. unsanitizable name) is treated
            // as "could not claim" — fail closed, nothing created.
            return Err(NewError::NameClaimed {
                name: params.name.clone(),
                holder: format!("<claim io error: {e}>"),
            });
        }
    };

    // UNDER the claim, the live-name scan is authoritative: two racers
    // serialize at the claim (the loser errored above); the winner's scan
    // cannot race another create. "Taken" means a LIVE holder — tombstoned /
    // dead-pid rows are skipped, so historical/cold names are reusable.
    //
    // (a) Registry rows with an alive pid holding the name → the distinct
    //     "taken by running session <id>" error, naming the holder's stable id
    //     (read-only idstore fold; falls back to the truncated provider UUID).
    if let Some(holder) = live_registry_name_holder(deps, &params.name) {
        return Err(NewError::NameHeldLive {
            name: params.name.clone(),
            holder,
        });
    }
    // (b) The mux scan (formerly the advisory PRE-claim check): a pane holding
    //     the name with no live registry row behind it. CASE-FOLDED (r4 F1):
    //     names are case-insensitive for uniqueness, matching the resolver.
    if deps
        .scan_all()
        .iter()
        .any(|z| z.name.eq_ignore_ascii_case(&params.name))
    {
        return Err(NewError::NameInUse(params.name.clone()));
    }

    // From here on EVERY return drops `claim` (RAII release) — the durable record
    // is the registry row the booted session writes; the claim only closes the
    // create window. `run_to_boot` performs steps 3.5-6 (the punch-18 ended-row
    // reap through boot wait); the guard outlives it.
    let outcome = run_to_boot(deps, params);
    drop(claim); // explicit for clarity; Drop would do this anyway on any return.
    outcome
}

/// The under-claim registry scan (spec-w2-env D3 step 2): is a LIVE session
/// (non-tombstoned row, alive pid) already holding `name`? Returns the holder's
/// display id via the shared fallback chain ([`crate::idstore::holder_display_id`]).
///
/// CASE-FOLDED match (red-team r4 F1, lead-adjudicated): names are
/// CASE-INSENSITIVE for uniqueness — the resolver's name tiers case-fold, so a
/// byte-exact gate here let `start WORKER` slip past a live `worker` and strip
/// name-addressability from BOTH (permanent loud ambiguity). Fold matches the
/// resolver; ASCII fold is total (names are ASCII-whitelisted at create).
fn live_registry_name_holder(deps: &NewDeps, name: &str) -> Option<String> {
    let holder = registry::read_entries(&deps.paths.sessions_dir, false)
        .into_iter()
        .filter(|s| !s.tombstoned)
        .find(|s| {
            s.entry
                .name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
                && s.entry
                    .pid
                    .is_some_and(|p| p != 0 && crate::effects::is_pid_alive(p as i32))
        })?;
    // QD_HOME-honoring state dir (the same resolution the verb layer uses).
    let state_dir = crate::paths::QdPaths::from_home_env(&deps.paths.home, deps.env).state_dir;
    Some(crate::idstore::holder_display_id(
        &crate::idstore::ids_path(&state_dir),
        holder.entry.session_id.as_deref(),
        holder.entry.pid,
    ))
}

/// Steps 4-6 (start_detached → I6 verify → boot wait), factored so the claim
/// guard in [`run_new`] wraps the whole thing and releases on every exit.
///
/// F1 (spec §2.2 / §2.3, lifecycle.ts:874-879): if `params.backend_env` is
/// non-empty, write the per-session 0600 env file and bake the self-deleting
/// dot-source prefix into the launch cmd FIRST; on ANY launch/boot failure after
/// the file is written, best-effort `remove_session_env_file` (cleanup parity,
/// utils.ts:669-680). Empty backend_env → no file, no prefix, byte-zero change.
fn run_to_boot(deps: &NewDeps, params: &NewParams) -> Result<NewOutcome, NewError> {
    // F1 write happens here, before launch, so cleanup is scoped to this fn.
    // S2 validation (run_new step 0a) + the `..` reject (step 0b) have already
    // guaranteed `params.name` is a safe env-file path component (no
    // `'`/`..`/separator), so the path cannot traverse and the single-quoted
    // prefix cannot be broken.
    //
    // P0 wave-2 (spec-w2-env D1): the env file is now UNCONDITIONAL whenever the
    // verb minted a stable id — it always carries `export QD_SESSION_ID='<id>'`
    // (an explicit set, clobbering anything inherited through the commissioner's
    // subtree) in addition to the F1 backend pairs. punch item 7: an INLINE
    // launch (the default) additionally carries the alt-screen-disable birth
    // property, which makes the pair set non-empty on EVERY inline launch
    // (including unit fixtures with no id and no backend env).
    let env_pairs = launch_env_pairs(
        params.backend_env.clone(),
        params.qd_session_id.clone(),
        params.render,
    );
    // R2 (override-never-inherit): the unset list = the --via clobber list
    // (A7 F12) + the render-mode unset (an --alt-screen launch must EXPLICITLY
    // `unset -v CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN` — omitting the export
    // alone leaves the child inheriting it from an inline parent env).
    let mut env_unsets = params.backend_env_unset.clone();
    env_unsets.extend(render_env_unsets(params.render));
    let has_env_content = !env_pairs.is_empty() || !env_unsets.is_empty();
    if has_env_content {
        if let Err(e) = write_session_env_file_with_unsets(
            &deps.paths.home,
            &params.name,
            &env_pairs,
            &env_unsets,
        ) {
            return Err(NewError::EnvFileWriteFailed {
                name: params.name.clone(),
                detail: e.to_string(),
            });
        }
    }

    // F1 (red-team r1) — cleanup is scoped to PRE-LAUNCH failures ONLY: zmx-run
    // spawn errors / nonzero exits mean nothing was launched, so nothing will
    // ever source the file — delete it (the original cleanup-parity intent,
    // utils.ts removeSessionEnvFile). POST-LAUNCH failures (I6 verify, the
    // pane-death verdict, boot timeout) DO NOT delete: the pane's `bash -lc`
    // may not have dot-sourced the file yet, and the prefix is FAIL-CLOSED
    // (`. file || exit 97`) — deleting here turned a FALSE death verdict into
    // a real kill of the healthy session (the F1 amplifier). TRADEOFF: if the
    // failure is genuine, a 0600 file (id + backend keys, never printed) leaks
    // at rest until the next same-name launch's unlink-first write replaces it
    // — bounded, and strictly better than killing healthy sessions.
    let result = run_to_boot_inner(deps, params, &env_pairs, &env_unsets);
    if has_env_content {
        if let Err(
            NewError::ZmxMissing(_)
            | NewError::EmbeddedDaemonLaunchFailed(_)
            | NewError::ZmxRunFailed(_)
            // punch item 18: the stale-pane refusal fires BEFORE run_detached
            // — nothing was launched, nothing will ever source the file.
            | NewError::StaleEndedPane { .. },
        ) = &result
        {
            remove_session_env_file(&deps.paths.home, &params.name);
        }
    }
    result
}

/// punch item 17 (b3-kill-spec, discharging the parked g7c retry-budget
/// question): sleeps between I6 verify re-scans — 6 scans total over ~6.2s
/// (0 / 200ms / 800ms / 2s / 4s / 6.2s marks). Sized against the g7c evidence
/// (g7c-flake-evidence.md): the registration race fires when the embedded
/// daemon registers slower than the verify's patience under heavy concurrent
/// load.
///
/// WIDENED in the b3 adversarial round (declared): the original ~2.5s budget
/// ([200, 600, 1200]) was sized against the evidence's "triple-concurrent-
/// cargo" condition, but the FULL 67-suite `cargo test --workspace` is a
/// heavier load than that — and the race fired once there, the create losing
/// to the budget by a hair at exactly the 2.5s mark (`[G7c] readiness took
/// 2.5s`, an I6 NotAttachable instead of the intended boot path). This is
/// the item-17 mandate ("tune to the g7c evidence; declare your numbers"),
/// not a re-gate of the P0 watch: ~6.2s clears the heaviest in-repo load
/// condition with margin while keeping a genuine Bug-D failure loud and
/// bounded (a true non-registration still errors after the budget; the cost
/// is paid ONLY on the failing path — a healthy registration exits on its
/// first hit). Deliberately NOT env-tunable: a knob could silently re-narrow
/// the window the evidence sized.
const I6_VERIFY_RESCAN_SLEEPS_MS: [u64; 5] = [200, 600, 1200, 2000, 2200];

/// punch item 18: bounded wait for a reaped ended row to drop out of
/// `list_raw` before `run_detached` — 20 polls × 100ms (~2s; the b3 probe
/// observed the dying-socket window resolving well under 1s). Exhausting the
/// budget refuses the launch ([`NewError::StaleEndedPane`]) instead of
/// running into a dying pty.
const ENDED_REAP_GONE_ROUNDS: u32 = 20;
const ENDED_REAP_GONE_POLL_MS: u64 = 100;

/// The launch + verify body (steps 3.5-6), separated so [`run_to_boot`] can wrap it
/// with the F1 env-file write/cleanup. The env prefix (when any) is layered onto
/// the base claude cmd here (lifecycle.ts:879 `envPrefix + buildClaudeCmd`).
fn run_to_boot_inner(
    deps: &NewDeps,
    params: &NewParams,
    env_pairs: &[(String, String)],
    env_unsets: &[String],
) -> Result<NewOutcome, NewError> {
    let base_cmd = build_claude_command(deps, params);
    // F1 + wave-2 + R2: prefix is empty only when there is NOTHING to apply
    // (no exports AND no unsets); in production the minted id is always
    // present, so the prefix is unconditional — and an --alt-screen launch's
    // unset-only content still gets sourced.
    let env_prefix = session_env_prefix(&deps.paths.home, &params.name, env_pairs, env_unsets);
    let claude_cmd = format!("{env_prefix}{base_cmd}");

    // --- Step 3.5 (punch item 18): pre-run ended-row reap ---------------------
    // zmx pty-reuse hijack (B1 r2; re-validated against zmx 0.6.0 in b3):
    // `zmx run` on a name whose previous task ENDED does NOT create a fresh
    // pane — it types the new command into the OLD pty, IGNORING the cwd
    // argument (the session "starts" in the dead session's directory, while
    // the list row even reports the REQUESTED start_dir — the metadata lies).
    // The name gates in run_new read the FILTERED list, where ended rows are
    // invisible, so the launch reached zmx with the trap armed.
    //
    // The reap scans list_raw at the CANONICAL dir only (run_detached is
    // pinned there — an ended row in a legacy dir cannot be reused by a
    // canonical-pinned run) for EXACT-name rows whose own row says ended
    // (`ended.is_some()` — identity-positive death evidence; killing a dead
    // pane's row releases zmx state, victim-safe). A LIVE same-name row
    // cannot reach here (gate (b) refused it loudly — unchanged); rows that
    // are merely unreachable/err carry NO death evidence and are left alone
    // (the r7 M1 refuse-on-missing-evidence posture).
    //
    // After the kill we WAIT (bounded) for the row to actually drop out of
    // list_raw: re-running into the dying-socket window either loses the
    // launch entirely (observed 3/3 swallowed runs in the b3 probe) or
    // re-arms the hijack — if the row never clears, refuse loudly
    // (StaleEndedPane) rather than launch into it.
    // CASE-FOLDED match (b3 adversarial concern 3): zmx names are case-
    // preserving but the create name gates fold case (gate (b),
    // resolver), so a same-name session can have ended under a DIFFERENT
    // case (Sess vs sess). An exact match would miss it and leave the
    // hijack armed on case-insensitive APFS. We fold the match but kill +
    // poll by the FOUND row's actual name (panes are case-preserving —
    // killing by the wrong case is a no-op). Dead-row-only, so victim-safe.
    let ended_name = deps
        .mux
        .list_raw(&deps.canonical_dir)
        .unwrap_or_default()
        .into_iter()
        .find(|z| z.name.eq_ignore_ascii_case(&params.name) && z.ended.is_some())
        .map(|z| z.name);
    if let Some(ended_name) = ended_name {
        let _ = deps.mux.kill(&deps.canonical_dir, &ended_name);
        let mut cleared = false;
        for round in 0..ENDED_REAP_GONE_ROUNDS {
            let still_there = deps
                .mux
                .list_raw(&deps.canonical_dir)
                .unwrap_or_default()
                .into_iter()
                .any(|z| z.name == ended_name);
            if !still_there {
                cleared = true;
                break;
            }
            if round + 1 < ENDED_REAP_GONE_ROUNDS {
                std::thread::sleep(std::time::Duration::from_millis(ENDED_REAP_GONE_POLL_MS));
            }
        }
        if !cleared {
            return Err(NewError::StaleEndedPane {
                name: params.name.clone(),
            });
        }
    }

    // --- Step 4: start_detached (utils.ts:348-375) ---------------------------
    // `zmx run <name> -d bash -lc <claudeCmd>` pinned to the canonical dir (L1).
    // A spawn-level Err (ENOENT — zmx not on PATH) → missing guidance, NOT a
    // stack trace (the fresh-machine war story: preflight passes a MISSING zmx
    // through to HERE rather than false-flagging it stale, utils.ts:361-366).
    let run_result =
        match deps
            .mux
            .run_detached(&deps.canonical_dir, &params.name, &claude_cmd, &params.cwd)
        {
            Ok(r) => r,
            // C1 M4fix: backend-aware spawn-failure error. The zmx lane keeps the
            // byte-stable missing-binary guidance (G-NEG asserts it); the embedded
            // lane names the qrmux daemon + carries the underlying error (the Lima
            // delta — a daemon-launch failure under embedded must NOT print the
            // misleading zmx guidance). The error detail was previously discarded.
            Err(e) => {
                return Err(match deps.backend {
                    crate::mux_selector::Backend::Zmx => {
                        NewError::ZmxMissing(preflight::zmx_missing_guidance())
                    }
                    crate::mux_selector::Backend::Embedded => {
                        NewError::EmbeddedDaemonLaunchFailed(e.to_string())
                    }
                });
            }
        };
    // Nonzero `zmx run` → "Failed to create session: <stderr>" (utils.ts:369-372).
    if run_result.status != Some(0) {
        return Err(NewError::ZmxRunFailed(run_result.stderr.trim().to_string()));
    }

    // --- Step 5: I6 verify (lifecycle.ts:759-777) ----------------------------
    // Verify the session is attachable in the canonical dir BEFORE waiting for
    // boot. If the write/read socket dirs disagreed (Bug D) the session would be
    // invisible+unkillable; fail loudly.
    //
    // We scan across canonical + legacy dirs (TS getZmxSessions) and look for the
    // name. A row found ONLY in a legacy dir carries that dir as `socket_dir` —
    // the split case. (canonical-wins dedupe means a row also present in
    // canonical reports the canonical dir, which is the healthy case.)
    //
    // punch item 17 (b3-kill-spec; g7c-flake-evidence.md): detection carries a
    // RETRY BUDGET. Under extreme box load the mux registers the new pane
    // slower than a single scan's patience (observed once ever, under
    // triple-concurrent-cargo load) — the old single scan turned that
    // transient into a false NotAttachable. Re-scan up to 4 times over ~2s;
    // a healthy registration exits on its first hit (zero added latency), and
    // genuine non-registration (Bug D) still errors loudly after the budget.
    let mut registered = deps.scan_all().into_iter().find(|z| z.name == params.name);
    for sleep_ms in I6_VERIFY_RESCAN_SLEEPS_MS {
        if registered.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        registered = deps.scan_all().into_iter().find(|z| z.name == params.name);
    }

    let canonical_str = deps.canonical_dir.to_string_lossy();
    match registered {
        None => {
            // punch item 17 — NO KILL ON ABSENCE (the F1/r6-r7 invariant:
            // destruction requires positive identity evidence). The retired
            // `mux.kill(canonical, name)` here was a kill-by-NAME fired into
            // the registration window: a pane registering one beat after the
            // last scan would be the HEALTHY just-launched session, and the
            // kill destroyed it (wrong victim). An absent row is the one
            // shape that carries no evidence at all — both production muxes
            // erase list failures into Ok-empty. TRADEOFF (documented leak):
            // a genuinely never-registering wrapper is NOT reaped here; the
            // next same-name start clears it via the identity-positive
            // stale/ended-pane machinery (the dead row itself is evidence).
            return Err(NewError::NotAttachable {
                name: params.name.clone(),
                canonical: deps.canonical_dir.clone(),
            });
        }
        Some(row) => {
            // socket_dir present AND != canonical → split; reap at the FOUND dir
            // (lifecycle.ts:770-776).
            if let Some(found) = &row.socket_dir {
                if found.as_str() != canonical_str {
                    let found_path = PathBuf::from(found);
                    let _ = deps.mux.kill(&found_path, &params.name);
                    return Err(NewError::SocketDirSplit {
                        name: params.name.clone(),
                        found: found_path,
                        canonical: deps.canonical_dir.clone(),
                    });
                }
            }
        }
    }

    // --- Step 6: boot wait (spec §6.6 / §8 seam) -----------------------------
    // M3 ships the real waiter; we call the injected seam and map Err → the
    // ported boot-failure text (lifecycle.ts:782-789). The session is NOT reaped
    // on timeout (TS tells the user to attach/inspect it).
    if let Err(failure) = deps.boot_waiter.wait_ready(&params.name) {
        return Err(NewError::BootTimeout {
            name: params.name.clone(),
            phase: failure.phase,
            detail: failure.detail,
        });
    }

    Ok(NewOutcome {
        name: params.name.clone(),
        socket_dir: deps.canonical_dir.clone(),
        claude_cmd,
    })
}

/// The O_EXCL claim payload for this create (spec §6.3 + B4 item 10): gathers
/// our pid + the exec-proof `start` probe (on our OWN pid) + the clock, then
/// delegates to [`registry::claim_payload`] — the writer that lives beside the
/// `claim_name` parser (B4 S4), so the 2-shape protocol cannot drift.
fn claim_payload(deps: &NewDeps, name: &str) -> String {
    let pid = std::process::id();
    let start = crate::effects::proc_start_ms(pid as i32);
    registry::claim_payload(pid, start, deps.clock.now_ms(), name)
}

/// RAII guard that releases a [`NameClaim`] on drop. The claim file is removed
/// when the guard goes out of scope — on the success path AND every failure
/// path after acquisition (spec §6.3: "released on EVERY exit path"). Release is
/// best-effort: a failed removal is swallowed (the create window is already
/// closed by the booted session's registry row; a leaked claim file is at worst
/// a stale UX hint, never a correctness problem).
struct ClaimGuard {
    claim: Option<NameClaim>,
}

impl ClaimGuard {
    fn new(claim: NameClaim) -> Self {
        Self { claim: Some(claim) }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if let Some(claim) = self.claim.take() {
            // Best-effort: an Err here means the file was already gone (fine).
            let _ = claim.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{FixedClock, MapEnv};
    use crate::exec::ScriptedExec;
    use crate::mux::FixtureMux;
    use std::collections::HashMap;
    use std::path::Path;
    use tempfile::TempDir;

    // The frozen real zmx 0.6.0 --help (send present) so preflight → Yes.
    const ZMX_HELP_OK: &str = "\
Commands:
  [s]end <name> <text...>                  Send raw input to session PTY
  [r]un <name> [-d] [command...]           Send command without attaching
";

    // A canned `zmx list` line for `name` in `dir`, attachable.
    fn list_line(name: &str) -> String {
        format!("name={name}\tpid=111\tclients=0\tcreated=1700000000\tstart_dir=/w\tcmd=claude\n")
    }

    struct Fix {
        _home: TempDir,
        paths: QdPaths,
        canonical: PathBuf,
        env: MapEnv,
        clock: FixedClock,
    }

    fn fixture() -> Fix {
        let home = tempfile::tempdir().unwrap();
        let paths = QdPaths::from_home(&home.path().join("home"));
        let canonical = home.path().join("zmx-501");
        // codex P1 W3: the create path no longer takes an explicit config-toml
        // path — the provider derives it off `fx.paths.home/.quorum/dispatch/config.toml`
        // (nonexistent here → DEFAULT_FLAGS, the pre-rewire jail behavior).
        Fix {
            _home: home,
            paths,
            canonical,
            env: MapEnv {
                vars: HashMap::new(),
                uid: 501,
            },
            clock: FixedClock(1_700_000_000_000),
        }
    }

    fn params(name: &str) -> NewParams {
        NewParams {
            name: name.to_string(),
            agent: None,
            resume: None,
            fork: false,
            claude_args: vec![],
            model: None,
            cwd: PathBuf::from("/work"),
            backend_env: vec![],
            backend_env_unset: vec![],
            qd_session_id: None,
            // Fixtures pin the PRODUCTION default (inline → the alt-screen-
            // disable birth property rides every fixture launch, punch item 7).
            render: RenderMode::Inline,
        }
    }

    fn ok_exec() -> ScriptedExec {
        ScriptedExec::new().on("zmx", &["--help"], Some(0), ZMX_HELP_OK, "")
    }

    use crate::exec::ExecResult;
    use crate::mux::{MuxCall, MuxSession};
    use std::cell::RefCell;

    /// A test mux modeling the REAL create timeline: the session is NOT in the
    /// list before `run_detached`, and APPEARS (in `appear_dir`, tagged with that
    /// dir as `socket_dir`) AFTER `run_detached`. This is what the static
    /// `FixtureMux` cannot do — it lets the live-name pre-check pass (empty) and
    /// the I6 verify find the freshly-created session. Records calls like
    /// `FixtureMux` so zero-keystroke / reap asserts work.
    struct StagedMux {
        /// The dir the session appears in after creation. For the healthy case
        /// this is the canonical dir; for the split case a foreign dir.
        appear_dir: PathBuf,
        name: String,
        created: RefCell<bool>,
        calls: RefCell<Vec<MuxCall>>,
    }
    impl StagedMux {
        fn new(appear_dir: PathBuf, name: &str) -> Self {
            Self {
                appear_dir,
                name: name.to_string(),
                created: RefCell::new(false),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<MuxCall> {
            self.calls.borrow().clone()
        }
        fn row(&self, dir: &Path) -> MuxSession {
            MuxSession {
                name: self.name.clone(),
                pid: 111,
                clients: 0,
                created: 0,
                start_dir: "/w".into(),
                cmd: "claude".into(),
                current: false,
                socket_dir: Some(dir.to_string_lossy().into_owned()),
                ended: None,
                exit_code: None,
                zmx_status: None,
                err: None,
            }
        }
    }
    impl Mux for StagedMux {
        fn list(&self, socket_dir: &Path) -> std::io::Result<Vec<MuxSession>> {
            // Only visible after creation, and only in `appear_dir`.
            if *self.created.borrow() && socket_dir == self.appear_dir {
                Ok(vec![self.row(socket_dir)])
            } else {
                Ok(vec![])
            }
        }
        fn list_raw(&self, socket_dir: &Path) -> std::io::Result<Vec<MuxSession>> {
            self.list(socket_dir)
        }
        fn run_detached(
            &self,
            socket_dir: &Path,
            name: &str,
            shell_cmd: &str,
            _cwd: &Path,
        ) -> std::io::Result<ExecResult> {
            *self.created.borrow_mut() = true;
            self.calls.borrow_mut().push(MuxCall {
                verb: "run_detached",
                socket_dir: socket_dir.to_path_buf(),
                name: name.to_string(),
                payload: shell_cmd.to_string(),
            });
            Ok(ExecResult {
                status: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            })
        }
        fn send(&self, socket_dir: &Path, name: &str, text: &str) -> std::io::Result<ExecResult> {
            self.calls.borrow_mut().push(MuxCall {
                verb: "send",
                socket_dir: socket_dir.to_path_buf(),
                name: name.to_string(),
                payload: text.to_string(),
            });
            Ok(ExecResult {
                status: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            })
        }
        fn kill(&self, socket_dir: &Path, name: &str) -> std::io::Result<i32> {
            self.calls.borrow_mut().push(MuxCall {
                verb: "kill",
                socket_dir: socket_dir.to_path_buf(),
                name: name.to_string(),
                payload: String::new(),
            });
            Ok(0)
        }
        fn history(&self, _d: &Path, _n: &str) -> std::io::Result<String> {
            Ok(String::new())
        }
        fn wait(&self, _d: &Path, _n: &[String]) -> std::io::Result<i32> {
            Ok(0)
        }
        fn attach(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
            Ok(0)
        }
    }

    // Build a deps bundle. The mux must report the session as attachable in
    // canonical AFTER run_detached for I6 to pass; the FixtureMux's list is
    // driven by its `with_dir` text, so we pre-seed the canonical listing.
    fn deps<'a>(
        fix: &'a Fix,
        exec: &'a ScriptedExec,
        mux: &'a dyn Mux,
        waiter: &'a dyn BootWaiter,
    ) -> NewDeps<'a> {
        NewDeps {
            mux,
            exec,
            env: &fix.env,
            clock: &fix.clock,
            paths: &fix.paths,
            canonical_dir: fix.canonical.clone(),
            legacy_dirs: vec![],
            boot_waiter: waiter,
            // codex P1 W3: the create path now builds the launch cmd through the
            // resolved provider. The claude impl derives its config-toml off
            // `fx.paths.home/.quorum/dispatch/config.toml` (nonexistent in the jail → DEFAULT_FLAGS,
            // identical to the pre-rewire `fix.config` no-config path).
            provider: &crate::provider::ClaudeProvider,
            // Default test backend = Zmx: the existing units assert the zmx-lane
            // ZmxMissing/ZmxRunFailed byte-stable paths. The embedded-lane error is
            // covered by a dedicated unit below.
            backend: crate::mux_selector::Backend::Zmx,
        }
    }

    // A deps builder with explicit legacy dirs (for the split test).
    fn deps_with_legacy<'a>(
        fix: &'a Fix,
        exec: &'a ScriptedExec,
        mux: &'a dyn Mux,
        waiter: &'a dyn BootWaiter,
        legacy: Vec<PathBuf>,
    ) -> NewDeps<'a> {
        let mut d = deps(fix, exec, mux, waiter);
        d.legacy_dirs = legacy;
        d
    }

    #[test]
    fn happy_path_creates_and_boots() {
        let fix = fixture();
        let exec = ok_exec();
        // Real timeline: pre-check sees nothing; after run_detached the session
        // appears in canonical (I6 verify finds it).
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap();
        assert_eq!(out.name, "sess");
        assert_eq!(out.socket_dir, fix.canonical);
        // run_detached issued exactly once.
        let runs: Vec<_> = mux
            .calls()
            .into_iter()
            .filter(|c| c.verb == "run_detached")
            .collect();
        assert_eq!(runs.len(), 1);
        // ZERO send keystrokes (dialog-free boot contract, L5).
        assert!(!mux.calls().iter().any(|c| c.verb == "send"));
        // No reap on the happy path.
        assert!(!mux.calls().iter().any(|c| c.verb == "kill"));
        // The claim file was released (no leftover under claims/).
        assert!(!fix
            .paths
            .sessions_dir
            .parent()
            .unwrap()
            .join("claims")
            .join("sess.claim")
            .exists());
    }

    #[test]
    fn preflight_no_errors_and_creates_nothing() {
        let fix = fixture();
        // A recognizable zmx help WITHOUT send → Capability::No.
        let exec = ScriptedExec::new().on(
            "zmx",
            &["--help"],
            Some(0),
            "Commands:\n  [r]un <name> [-d] [command...]   Send command without attaching\n  [l]ist|ls\n",
            "",
        );
        let mux = FixtureMux::new();
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap_err();
        assert!(matches!(err, NewError::PreflightStale(_)));
        assert_eq!(err.exit_code(), 1);
        // No zmx run ever issued.
        assert!(!mux.calls().iter().any(|c| c.verb == "run_detached"));
    }

    #[test]
    fn preflight_unknown_proceeds() {
        let fix = fixture();
        // Garbage --help (not recognizably zmx) → Capability::Unknown → proceed.
        let exec = ScriptedExec::new().on("zmx", &["--help"], Some(127), "", "command not found");
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap();
        assert_eq!(out.name, "sess");
    }

    // The static `--agent` path is RETIRED: `qd start --agent` is refused at
    // the verb layer (verbs/stubs.rs), so `params.agent` never arrives `Some` from
    // the CLI. This pins that the create path no longer consults any agents dir —
    // even an explicitly-set `params.agent` boots straight through
    // (the launch-argv plumbing forwards it to the claude builder, but the engine
    // resolves / fail-closes on NOTHING). No agents dir is created or read.
    #[test]
    fn agent_field_no_longer_consults_agents_dir() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let mut p = params("sess");
        // A value that would have fail-closed under the retired HARDENING #1
        // (no `<agents_dir>/ghost.md` exists) now boots through unimpeded.
        p.agent = Some("ghost".to_string());
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &p).unwrap();
        assert_eq!(out.name, "sess");
        assert_eq!(
            mux.calls()
                .iter()
                .filter(|c| c.verb == "run_detached")
                .count(),
            1
        );
    }

    #[test]
    fn name_in_use_mux_holder_under_claim() {
        let fix = fixture();
        let exec = ok_exec();
        // The canonical dir already has a LIVE mux pane named "taken".
        let mux = FixtureMux::new().with_dir(fix.canonical.clone(), list_line("taken"));
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("taken")).unwrap_err();
        assert!(matches!(err, NewError::NameInUse(_)));
        // No run issued — the under-claim scan short-circuits before launch.
        assert!(!mux.calls().iter().any(|c| c.verb == "run_detached"));
        // P0 wave-2 (scan-under-claim): the claim was taken FIRST and released
        // on the error return — the name is claimable again.
        let claims_dir = fix.paths.sessions_dir.parent().unwrap().join("claims");
        assert!(!claims_dir.join("taken.claim").exists());
    }

    // === P0 wave-2 (spec-w2-env D3): scan-under-claim, registry live-holder ===

    /// A registry row helper: `<pid>.json` with name + sessionId.
    fn registry_row(fix: &Fix, pid: i64, name: &str, uuid: &str) {
        let e = crate::registry::RegistryEntry {
            pid: Some(pid),
            session_id: Some(uuid.to_string()),
            name: Some(name.to_string()),
            status: Some("idle".to_string()),
            ..Default::default()
        };
        crate::registry::write_entry(&fix.paths.sessions_dir, &e).unwrap();
    }

    /// The D3 sequence proof: A claimed+booted+released (modeled by A's LIVE
    /// registry row — the claim is long gone); B starts the same name → B's
    /// UNDER-CLAIM scan finds A live → the distinct "taken by running session
    /// <id>" error, naming A's STABLE id (seeded in the idstore).
    #[test]
    fn live_registry_holder_errors_with_stable_id() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "wk");
        // A is LIVE: an alive pid (this test process) + a registry row.
        registry_row(&fix, std::process::id() as i64, "wk", "uuid-holder");
        // A's stable id is mapped in the idstore (QD_HOME unset → <home>/.quorum/dispatch/state).
        let ids_path = crate::idstore::ids_path(
            &fix.paths
                .home
                .join(".quorum")
                .join("dispatch")
                .join("state"),
        );
        let mut g = || "ab3kx9mq".to_string();
        crate::idstore::mint_or_get_with(&ids_path, "uuid-holder", Some("wk"), &fix.clock, &mut g)
            .unwrap();

        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("wk")).unwrap_err();
        match &err {
            NewError::NameHeldLive { name, holder } => {
                assert_eq!(name, "wk");
                assert_eq!(holder, "ab3kx9mq", "the holder is named by STABLE id");
            }
            other => panic!("expected NameHeldLive, got {other:?}"),
        }
        // The distinct error text names the running session.
        let msg = err.to_string();
        assert!(
            msg.contains("name \"wk\" is taken by running session ab3kx9mq"),
            "{msg}"
        );
        // Nothing launched; the claim was released.
        assert!(!mux.calls().iter().any(|c| c.verb == "run_detached"));
        let claims_dir = fix.paths.sessions_dir.parent().unwrap().join("claims");
        assert!(!claims_dir.join("wk.claim").exists());
    }

    /// Red-team r4 F1 (lead-adjudicated: names are CASE-INSENSITIVE for
    /// uniqueness, matching the resolver's name tiers): a CASE-VARIANT of a
    /// live session's name is "taken" — `start WK` beside live `wk` must
    /// refuse exactly like `start wk` does. Pre-fix, the byte-exact gate let
    /// it through and BOTH sessions lost name-addressability (permanent loud
    /// ambiguity at the resolver).
    #[test]
    fn live_registry_holder_case_variant_is_still_taken() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "WK");
        registry_row(&fix, std::process::id() as i64, "wk", "uuid-holder");

        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("WK")).unwrap_err();
        assert!(
            matches!(&err, NewError::NameHeldLive { name, .. } if name == "WK"),
            "case-variant of a live name must be NameHeldLive, got {err:?}"
        );
        assert!(!mux.calls().iter().any(|c| c.verb == "run_detached"));
    }

    /// The mux scan is case-folded too: a PANE named `taken` (no registry row
    /// behind it) blocks `start TAKEN` with NameInUse.
    #[test]
    fn mux_scan_case_variant_is_still_in_use() {
        let fix = fixture();
        let exec = ok_exec();
        // The canonical dir already has a LIVE mux pane named "taken".
        let mux = FixtureMux::new().with_dir(fix.canonical.clone(), list_line("taken"));
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("TAKEN")).unwrap_err();
        assert!(
            matches!(&err, NewError::NameInUse(n) if n == "TAKEN"),
            "case-variant of a live pane name must be NameInUse, got {err:?}"
        );
        assert!(!mux.calls().iter().any(|c| c.verb == "run_detached"));
    }

    /// Unmapped holder (no idstore line) → the truncated provider UUID names it.
    #[test]
    fn live_registry_holder_without_stable_id_falls_back_to_uuid() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "wk");
        registry_row(
            &fix,
            std::process::id() as i64,
            "wk",
            "0123456789abcdef-uuid",
        );
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("wk")).unwrap_err();
        match &err {
            NewError::NameHeldLive { holder, .. } => {
                assert_eq!(
                    holder,
                    &crate::fmt::truncate_id_default("0123456789abcdef-uuid")
                );
            }
            other => panic!("expected NameHeldLive, got {other:?}"),
        }
    }

    /// "Taken" means a LIVE holder (ruled): a stopped/tombstoned A and a
    /// dead-pid A are both reusable — B succeeds.
    #[test]
    fn dead_or_tombstoned_holder_is_reusable() {
        // (a) dead pid: the row exists but its process is gone.
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "wk");
        registry_row(&fix, 2_147_483_646, "wk", "uuid-dead"); // reliably-dead pid
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("wk")).unwrap();
        assert_eq!(out.name, "wk", "dead-pid holder must not block the name");

        // (b) tombstoned: an alive pid behind a tombstone is NOT a live holder.
        let fix2 = fixture();
        let exec2 = ok_exec();
        let mux2 = StagedMux::new(fix2.canonical.clone(), "wk");
        let e = crate::registry::RegistryEntry {
            pid: Some(std::process::id() as i64),
            session_id: Some("uuid-tomb".to_string()),
            name: Some("wk".to_string()),
            status: Some("idle".to_string()),
            ..Default::default()
        };
        crate::registry::write_entry(&fix2.paths.sessions_dir, &e).unwrap();
        crate::registry::ensure_tombstone(
            &fix2.paths.sessions_dir,
            std::process::id() as i64,
            Some(&e),
        );
        let out2 = run_new(&deps(&fix2, &exec2, &mux2, &OkBootWaiter), &params("wk")).unwrap();
        assert_eq!(out2.name, "wk", "tombstoned holder must not block the name");
    }

    // === P0 wave-2 (spec-w2-env D1): QD_SESSION_ID injection at create =======

    /// With a minted id and NO backend env, the env file + dot-source prefix
    /// are now UNCONDITIONAL: the launch cmd carries the prefix and the file
    /// carries the explicit `export QD_SESSION_ID='<id>'`.
    #[test]
    fn qd_session_id_injected_unconditionally() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let mut p = params("sess");
        p.qd_session_id = Some("ab3kx9mq".to_string());
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &p).unwrap();

        // The launch cmd is PREFIXED with the self-deleting dot-source.
        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "sess");
        assert!(
            out.claude_cmd
                .starts_with(&format!("{{ . '{}'; }}", env_file.display())),
            "unconditional prefix: {}",
            out.claude_cmd
        );
        // The run_detached payload (what the mux actually launched) carries it too.
        let run = mux
            .calls()
            .into_iter()
            .find(|c| c.verb == "run_detached")
            .unwrap();
        assert!(run
            .payload
            .contains(".quorum/dispatch/session-env/sess.env"));
        // The file body carries the explicit export (the id is NOT a secret —
        // a 0600 file is fine; the VALUE never appears in argv). Item-1 FORCE
        // birth property + punch item 7 render birth property ride the same
        // file, id LAST.
        let body = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(
            body,
            "export CLAUDE_CODE_FORCE_SESSION_PERSISTENCE='1'\n\
             export CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN='1'\nexport QD_SESSION_ID='ab3kx9mq'\n"
        );
    }

    /// D1 site 4 — override, never inherit: a child launched from INSIDE a
    /// parent session (parent env carries QD_SESSION_ID=parent-id) boots with
    /// ITS OWN id. The dot-sourced `export` is an explicit set in the child's
    /// shell; the parent's value is never captured into the file (the F1
    /// whitelist does not include QD_SESSION_ID) and never reaches the cmd.
    #[test]
    fn child_env_overrides_inherited_parent_id() {
        let mut fix = fixture();
        // The COMMISSIONER's environment carries its own identity.
        fix.env
            .vars
            .insert("QD_SESSION_ID".to_string(), "parentid".to_string());
        // The F1 capture (verb layer) ignores it — whitelist-pinned here too.
        let captured = crate::launch::capture_backend_env(&fix.env);
        assert!(
            captured.iter().all(|(k, _)| k != "QD_SESSION_ID"),
            "QD_SESSION_ID must never be captured/inherited: {captured:?}"
        );

        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "child");
        let mut p = params("child");
        p.qd_session_id = Some("childid2".to_string());
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &p).unwrap();

        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "child");
        let body = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(
            body,
            "export CLAUDE_CODE_FORCE_SESSION_PERSISTENCE='1'\n\
             export CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN='1'\nexport QD_SESSION_ID='childid2'\n",
            "the explicit set carries the CHILD's id"
        );
        assert!(
            !body.contains("parentid"),
            "parent id never reaches the file"
        );
        assert!(
            !out.claude_cmd.contains("parentid"),
            "parent id never reaches the launch cmd"
        );
    }

    /// Backend env + the id compose: exports for both land in the one file
    /// (backend pairs first, QD_SESSION_ID last).
    #[test]
    fn qd_session_id_composes_with_backend_env() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let mut p = params("sess");
        p.backend_env = vec![("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string())];
        p.qd_session_id = Some("ab3kx9mq".to_string());
        run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &p).unwrap();
        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "sess");
        let body = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(
            body,
            "export ANTHROPIC_BASE_URL='http://r'\n\
             export CLAUDE_CODE_FORCE_SESSION_PERSISTENCE='1'\n\
             export CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN='1'\nexport QD_SESSION_ID='ab3kx9mq'\n"
        );
    }

    /// P0 QA (spec-w4-qa B): the `--via` unset-clobber list composes with the
    /// identity pair WITHOUT stripping it. Even when the unset list names every
    /// backend whitelist key, the file is `unset -v` lines FIRST (A7 F12: an
    /// unset after an export would clobber it), then the composed exports, then
    /// `QD_SESSION_ID` LAST — and no `unset -v QD_SESSION_ID` ever appears.
    #[test]
    fn via_unsets_compose_with_identity_pair_without_stripping_it() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let mut p = params("sess");
        p.backend_env = vec![("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string())];
        p.backend_env_unset = vec![
            "ANTHROPIC_API_KEY".to_string(),
            "ANTHROPIC_BASE_URL".to_string(),
            "ANTHROPIC_MODEL".to_string(),
        ];
        p.qd_session_id = Some("ab3kx9mq".to_string());
        run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &p).unwrap();
        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "sess");
        let body = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(
            body,
            "unset -v ANTHROPIC_API_KEY\nunset -v ANTHROPIC_BASE_URL\nunset -v ANTHROPIC_MODEL\n\
             export ANTHROPIC_BASE_URL='http://r'\n\
             export CLAUDE_CODE_FORCE_SESSION_PERSISTENCE='1'\n\
             export CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN='1'\nexport QD_SESSION_ID='ab3kx9mq'\n",
            "unsets first, exports after, identity LAST and never unset"
        );
        assert!(
            !body.contains("unset -v QD_SESSION_ID"),
            "the unset-clobber list must never strip the identity pair: {body}"
        );
    }

    /// R2 (override-never-inherit, re-pinning the punch-7 byte-zero guard this
    /// REPLACES): an --alt-screen launch is no longer content-free — it must
    /// EXPLICITLY `unset -v` the inline var (the child would otherwise inherit
    /// it from an inline parent env and render inline despite the flag), and the
    /// launched cmd still carries the dot-source prefix (the unset must actually
    /// apply). With no id and no backend env the file is the alt-screen unset
    /// PLUS the unconditional FORCE birth property (item 1) — there is NO
    /// prefix-free launch anymore: every launch asserts the render birth
    /// property in one direction or the other, and FORCE always.
    #[test]
    fn alt_screen_create_writes_unset_plus_force_env_file() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let mut p = params("sess");
        p.render = RenderMode::AltScreen;
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &p).unwrap();
        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "sess");
        let body = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(
            body,
            "unset -v CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN\n\
             export CLAUDE_CODE_FORCE_SESSION_PERSISTENCE='1'\n",
            "alt-screen asserts the render property by EXPLICIT unset (never by \
             absence); FORCE-persistence still rides unconditionally"
        );
        // The prefix rides the cmd — an unset-only file MUST still be sourced.
        assert!(
            out.claude_cmd
                .contains(".quorum/dispatch/session-env/sess.env"),
            "unset-only env file must be dot-sourced: {}",
            out.claude_cmd
        );
    }

    /// punch item 7: the DEFAULT (inline) launch injects the alt-screen-disable
    /// birth property through the env file on the CREATE path — and the
    /// run_detached payload (what the mux actually launched) carries the
    /// dot-source prefix that delivers it. (The export is an explicit set, so
    /// inline needs no unset — it clobbers anything inherited.)
    #[test]
    fn inline_default_injects_alt_screen_disable_at_create() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap();
        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "sess");
        let body = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(
            body,
            "export CLAUDE_CODE_FORCE_SESSION_PERSISTENCE='1'\n\
             export CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN='1'\n",
            "inline default carries the FORCE + render birth properties"
        );
        let run = mux
            .calls()
            .into_iter()
            .find(|c| c.verb == "run_detached")
            .unwrap();
        assert!(
            run.payload
                .contains(".quorum/dispatch/session-env/sess.env"),
            "the launched cmd dot-sources the env file: {}",
            run.payload
        );
    }

    #[test]
    fn claim_contention_second_loses_one_run() {
        let fix = fixture();
        let exec1 = ok_exec();
        let exec2 = ok_exec();
        // Model an in-flight create by occupying the create-path claim
        // out-of-band BEFORE the second run_new for the same name. The holder
        // pid must be genuinely ALIVE (and OURS — kill(pid,0) on a foreign pid
        // is EPERM = "dead" to is_pid_alive): P0 redfix F2 reaps dead-holder
        // claims, so a made-up pid would be reaped instead of refused.
        let claims_dir = deps(&fix, &exec1, &FixtureMux::new(), &OkBootWaiter).claims_dir();
        let own_pid = std::process::id();
        let payload = format!("{{\"pid\":{own_pid},\"timestamp\":0,\"name\":\"sess\"}}");
        let occupier =
            registry::claim_name(&claims_dir, "sess", payload.as_bytes(), &|_| true, &|_| {
                None
            })
            .unwrap();

        // Now a real run_new for the SAME name must lose at the claim (the
        // live-name pre-check passes because the mux lists nothing).
        let mux = FixtureMux::new(); // empty list → pre-check passes
        let err = run_new(&deps(&fix, &exec2, &mux, &OkBootWaiter), &params("sess")).unwrap_err();
        match &err {
            NewError::NameClaimed { name, holder } => {
                assert_eq!(name, "sess");
                assert!(
                    holder.contains(&format!("\"pid\":{own_pid}")),
                    "holder payload named: {holder}"
                );
            }
            other => panic!("expected NameClaimed, got {other:?}"),
        }
        // The loser issued NO run_detached.
        assert!(!mux.calls().iter().any(|c| c.verb == "run_detached"));
        occupier.release().unwrap();
    }

    #[test]
    fn claim_released_after_i6_failure_allows_retry() {
        let fix = fixture();
        let exec = ok_exec();
        // I6 FAILS: run_detached succeeds but the canonical dir lists NOTHING
        // (not attachable) → NotAttachable, NO kill (punch item 17).
        let mux = FixtureMux::new(); // empty canonical list
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap_err();
        assert!(matches!(err, NewError::NotAttachable { .. }));
        // punch item 17: the None arm fires ZERO kills (was: reap-by-name).
        assert!(!mux.calls().iter().any(|c| c.verb == "kill"));
        // CRITICAL: the claim was released, so the name is claimable again.
        let claims_dir = deps(&fix, &exec, &mux, &OkBootWaiter).claims_dir();
        let reclaim = registry::claim_name(&claims_dir, "sess", b"retry", &|_| true, &|_| None);
        assert!(
            reclaim.is_ok(),
            "claim must be released on the I6 failure path"
        );
        reclaim.unwrap().release().unwrap();
    }

    /// B4 item 10: the claim payload stamps the claimant's own start time
    /// (`"start"`, epoch ms) alongside pid — the exec-proof identity a later
    /// contender verifies against the live pid. (The probe runs on OUR OWN pid
    /// via `ps`, so a real value is expected on any host that runs the suite.)
    #[test]
    fn claim_payload_carries_exec_proof_start() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = FixtureMux::new();
        let d = deps(&fix, &exec, &mux, &OkBootWaiter);
        let p = claim_payload(&d, "sess");
        let v: serde_json::Value = serde_json::from_str(&p).expect("payload is JSON");
        assert_eq!(v["pid"].as_i64(), Some(std::process::id() as i64));
        assert_eq!(v["name"].as_str(), Some("sess"));
        let start = v["start"].as_i64().expect("claimant start stamped");
        // Sanity: the stamp is OUR start — no later than now, and recent
        // enough to be this test process (a day of slack absorbs CI weirdness).
        let now = crate::effects::RealClock.now_ms();
        assert!(
            start <= now,
            "start {start} must not be in the future {now}"
        );
        assert!(now - start < 86_400_000, "start {start} too old vs {now}");
    }

    /// B4 S3 PIN (recovery hint correctness): the `NameClaimed` error names the
    /// ENCODED on-disk claim file. For a case-variant name `MyAgent` the file is
    /// `myagent.claim` — the hint must NOT print `MyAgent.claim` (rm would fail
    /// on a case-sensitive fs).
    #[test]
    fn name_claimed_error_prints_encoded_claim_file() {
        let e = NewError::NameClaimed {
            name: "MyAgent".to_string(),
            holder: "{\"pid\":1}".to_string(),
        };
        let msg = e.to_string();
        assert!(
            msg.contains("'myagent.claim'"),
            "must name the encoded on-disk file: {msg}"
        );
        assert!(
            !msg.contains("'MyAgent.claim'"),
            "must NOT name the raw (mis-cased) file: {msg}"
        );
    }

    /// punch item 17 PIN (b3-kill-spec): never-registers → loud NotAttachable
    /// with ZERO kill calls on the recording mux. The retired kill-by-name on
    /// absence was a destructive race: fired into the registration window it
    /// killed the healthy just-launched pane (wrong victim, F1-erasure class).
    /// Bug-D detection itself stays loud (the error still surfaces, exit 1).
    #[test]
    fn i6_not_attachable_errors_loudly_with_zero_kills() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = FixtureMux::new(); // canonical lists nothing — every scan
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap_err();
        match &err {
            NewError::NotAttachable { name, canonical } => {
                assert_eq!(name, "sess");
                assert_eq!(canonical, &fix.canonical);
            }
            other => panic!("expected NotAttachable, got {other:?}"),
        }
        assert_eq!(err.exit_code(), 1);
        // THE PIN: zero kill calls — absence is not a target.
        assert!(
            !mux.calls().iter().any(|c| c.verb == "kill"),
            "the I6 None arm must never kill: {:?}",
            mux.calls()
        );
    }

    /// A mux modeling the g7c registration race: the pane registers LATE — the
    /// first `appears_after_lists` post-run list calls return empty, then the
    /// row appears in the canonical dir. (Pre-run lists are always empty, so
    /// the live-name pre-check passes like StagedMux.)
    struct LateRegisterMux {
        canonical: PathBuf,
        name: String,
        appears_after_lists: usize,
        created: RefCell<bool>,
        post_run_lists: RefCell<usize>,
        calls: RefCell<Vec<MuxCall>>,
    }
    impl LateRegisterMux {
        fn new(canonical: PathBuf, name: &str, appears_after_lists: usize) -> Self {
            Self {
                canonical,
                name: name.to_string(),
                appears_after_lists,
                created: RefCell::new(false),
                post_run_lists: RefCell::new(0),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<MuxCall> {
            self.calls.borrow().clone()
        }
    }
    impl Mux for LateRegisterMux {
        fn list(&self, socket_dir: &Path) -> std::io::Result<Vec<MuxSession>> {
            if !*self.created.borrow() {
                return Ok(vec![]); // pre-run: name free
            }
            let seen = {
                let mut n = self.post_run_lists.borrow_mut();
                *n += 1;
                *n
            };
            if seen > self.appears_after_lists && socket_dir == self.canonical {
                Ok(vec![MuxSession {
                    name: self.name.clone(),
                    pid: 111,
                    clients: 0,
                    created: 0,
                    start_dir: "/w".into(),
                    cmd: "claude".into(),
                    current: false,
                    socket_dir: Some(socket_dir.to_string_lossy().into_owned()),
                    ended: None,
                    exit_code: None,
                    zmx_status: None,
                    err: None,
                }])
            } else {
                Ok(vec![]) // still registering
            }
        }
        fn list_raw(&self, socket_dir: &Path) -> std::io::Result<Vec<MuxSession>> {
            self.list(socket_dir)
        }
        fn run_detached(
            &self,
            socket_dir: &Path,
            name: &str,
            shell_cmd: &str,
            _cwd: &Path,
        ) -> std::io::Result<ExecResult> {
            *self.created.borrow_mut() = true;
            self.calls.borrow_mut().push(MuxCall {
                verb: "run_detached",
                socket_dir: socket_dir.to_path_buf(),
                name: name.to_string(),
                payload: shell_cmd.to_string(),
            });
            Ok(ExecResult {
                status: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            })
        }
        fn send(&self, _d: &Path, _n: &str, _t: &str) -> std::io::Result<ExecResult> {
            unreachable!("not exercised")
        }
        fn kill(&self, socket_dir: &Path, name: &str) -> std::io::Result<i32> {
            self.calls.borrow_mut().push(MuxCall {
                verb: "kill",
                socket_dir: socket_dir.to_path_buf(),
                name: name.to_string(),
                payload: String::new(),
            });
            Ok(0)
        }
        fn history(&self, _d: &Path, _n: &str) -> std::io::Result<String> {
            Ok(String::new())
        }
        fn wait(&self, _d: &Path, _n: &[String]) -> std::io::Result<i32> {
            Ok(0)
        }
        fn attach(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
            Ok(0)
        }
    }

    /// punch item 18: a mux with a pre-existing ENDED same-name row in
    /// `list_raw` at canonical (invisible to the FILTERED gates). `kill`
    /// clears it (the well-behaved zmx shape) unless `kill_clears` is false
    /// (the stuck dying-socket shape). After run_detached the live row
    /// appears (StagedMux timeline).
    struct EndedRowMux {
        canonical: PathBuf,
        name: String,
        /// The case-preserved name the ENDED row carries (panes are case-
        /// preserving; the create gates fold case). Defaults to `name`; the
        /// concern-3 pin sets it to a case variant.
        ended_name: String,
        ended_present: RefCell<bool>,
        kill_clears: bool,
        created: RefCell<bool>,
        calls: RefCell<Vec<MuxCall>>,
    }
    impl EndedRowMux {
        fn new(canonical: PathBuf, name: &str, kill_clears: bool) -> Self {
            Self {
                canonical,
                name: name.to_string(),
                ended_name: name.to_string(),
                ended_present: RefCell::new(true),
                kill_clears,
                created: RefCell::new(false),
                calls: RefCell::new(Vec::new()),
            }
        }
        /// The ended row carries `ended_name` (a possibly case-variant name);
        /// the live post-run row still carries `name` (== params.name).
        fn with_ended_name(canonical: PathBuf, name: &str, ended_name: &str) -> Self {
            let mut m = Self::new(canonical, name, true);
            m.ended_name = ended_name.to_string();
            m
        }
        fn calls(&self) -> Vec<MuxCall> {
            self.calls.borrow().clone()
        }
        fn live_row(&self, dir: &Path) -> MuxSession {
            MuxSession {
                name: self.name.clone(),
                pid: 111,
                clients: 0,
                created: 0,
                start_dir: "/w".into(),
                cmd: "claude".into(),
                current: false,
                socket_dir: Some(dir.to_string_lossy().into_owned()),
                ended: None,
                exit_code: None,
                zmx_status: None,
                err: None,
            }
        }
    }
    impl Mux for EndedRowMux {
        fn list(&self, socket_dir: &Path) -> std::io::Result<Vec<MuxSession>> {
            // FILTERED: the ended row is invisible here (the hijack's cover).
            if *self.created.borrow() && socket_dir == self.canonical {
                Ok(vec![self.live_row(socket_dir)])
            } else {
                Ok(vec![])
            }
        }
        fn list_raw(&self, socket_dir: &Path) -> std::io::Result<Vec<MuxSession>> {
            let mut rows = self.list(socket_dir)?;
            if *self.ended_present.borrow() && socket_dir == self.canonical {
                let mut ended = self.live_row(socket_dir);
                ended.name = self.ended_name.clone(); // case-preserved row name
                ended.ended = Some(1_700_000_000);
                ended.exit_code = Some(0);
                rows.push(ended);
            }
            Ok(rows)
        }
        fn run_detached(
            &self,
            socket_dir: &Path,
            name: &str,
            shell_cmd: &str,
            _cwd: &Path,
        ) -> std::io::Result<ExecResult> {
            *self.created.borrow_mut() = true;
            self.calls.borrow_mut().push(MuxCall {
                verb: "run_detached",
                socket_dir: socket_dir.to_path_buf(),
                name: name.to_string(),
                payload: shell_cmd.to_string(),
            });
            Ok(ExecResult {
                status: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            })
        }
        fn send(&self, _d: &Path, _n: &str, _t: &str) -> std::io::Result<ExecResult> {
            unreachable!("not exercised")
        }
        fn kill(&self, socket_dir: &Path, name: &str) -> std::io::Result<i32> {
            // Only an EXACT-name kill clears the row (panes are case-
            // preserving): this proves the verb killed by the FOUND row's
            // name, not the case-folded params.name (concern-3).
            if self.kill_clears && name == self.ended_name {
                *self.ended_present.borrow_mut() = false;
            }
            self.calls.borrow_mut().push(MuxCall {
                verb: "kill",
                socket_dir: socket_dir.to_path_buf(),
                name: name.to_string(),
                payload: String::new(),
            });
            Ok(0)
        }
        fn history(&self, _d: &Path, _n: &str) -> std::io::Result<String> {
            Ok(String::new())
        }
        fn wait(&self, _d: &Path, _n: &[String]) -> std::io::Result<i32> {
            Ok(0)
        }
        fn attach(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
            Ok(0)
        }
    }

    /// punch item 18 PIN: an ended same-name row at canonical is reaped
    /// (identity-positive: the row itself says ended) BEFORE run_detached,
    /// and the create proceeds to success — the hijack trap is disarmed.
    #[test]
    fn ended_row_reaped_before_run_then_create_succeeds() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = EndedRowMux::new(fix.canonical.clone(), "sess", true);
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap();
        assert_eq!(out.name, "sess");
        // Exactly ONE kill — the ended-row reap, at canonical, by name —
        // and it happened BEFORE run_detached.
        let calls = mux.calls();
        let kill_idx = calls.iter().position(|c| c.verb == "kill").unwrap();
        let run_idx = calls.iter().position(|c| c.verb == "run_detached").unwrap();
        assert!(kill_idx < run_idx, "reap must precede run_detached");
        assert_eq!(calls.iter().filter(|c| c.verb == "kill").count(), 1);
        assert_eq!(calls[kill_idx].socket_dir, fix.canonical);
        assert_eq!(calls[kill_idx].name, "sess");
    }

    /// b3 adversarial concern 3 PIN: the ended-row match is CASE-FOLDED. A
    /// previous session that ended under a DIFFERENT case ("Sess") must be
    /// reaped when starting "sess" — an exact match would miss it and leave
    /// the hijack armed on case-insensitive APFS. The kill targets the FOUND
    /// dead row's actual name ("Sess"), not the requested "sess".
    #[test]
    fn ended_row_case_variant_is_reaped() {
        let fix = fixture();
        let exec = ok_exec();
        // Requested name "sess"; the ended row in list_raw is "Sess".
        let mux = EndedRowMux::with_ended_name(fix.canonical.clone(), "sess", "Sess");
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap();
        assert_eq!(out.name, "sess");
        let calls = mux.calls();
        let kill = calls.iter().find(|c| c.verb == "kill").expect("must reap");
        // Killed by the FOUND row's case-preserved name, before run_detached.
        assert_eq!(
            kill.name, "Sess",
            "must kill the found dead row's actual name"
        );
        let kill_idx = calls.iter().position(|c| c.verb == "kill").unwrap();
        let run_idx = calls.iter().position(|c| c.verb == "run_detached").unwrap();
        assert!(kill_idx < run_idx, "reap must precede run_detached");
    }

    /// punch item 18 PIN: a reaped ended row that never clears refuses the
    /// launch loudly (StaleEndedPane) — run_detached is NEVER reached
    /// (launching into a dying pty loses the launch or re-arms the hijack) —
    /// and the claim is released for a retry.
    #[test]
    fn ended_row_unclearable_refuses_before_launch() {
        let fix = fixture();
        let exec = ok_exec();
        let mux = EndedRowMux::new(fix.canonical.clone(), "sess", false);
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap_err();
        match &err {
            NewError::StaleEndedPane { name } => assert_eq!(name, "sess"),
            other => panic!("expected StaleEndedPane, got {other:?}"),
        }
        assert!(
            !mux.calls().iter().any(|c| c.verb == "run_detached"),
            "must not launch into a dying pty"
        );
        // Claim released → retryable. (B4 added claim_name's proc_start probe
        // param — the merge resolution threads the same &|_| None used by the
        // other I6-failure reclaim sites.)
        let claims_dir = deps(&fix, &exec, &mux, &OkBootWaiter).claims_dir();
        let reclaim = registry::claim_name(&claims_dir, "sess", b"retry", &|_| true, &|_| None);
        assert!(reclaim.is_ok(), "claim must be released on the refusal");
        reclaim.unwrap().release().unwrap();
    }

    /// punch item 17 PIN (b3-kill-spec): the g7c shape FLIPPED — a pane that
    /// registers on the third scan (slower than the old single scan's patience)
    /// now SUCCEEDS: no error, no kill, create proceeds to boot.
    #[test]
    fn i6_registration_on_third_scan_succeeds_no_error_no_kill() {
        let fix = fixture();
        let exec = ok_exec();
        // Appears after 2 post-run lists: scan 1 and 2 miss, scan 3 hits —
        // inside the 4-scan budget.
        let mux = LateRegisterMux::new(fix.canonical.clone(), "sess", 2);
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap();
        assert_eq!(out.name, "sess");
        assert_eq!(out.socket_dir, fix.canonical);
        assert!(
            !mux.calls().iter().any(|c| c.verb == "kill"),
            "late registration within the budget must not kill anything"
        );
    }

    #[test]
    fn i6_socket_dir_split_reaps_at_found_dir() {
        let fix = fixture();
        let exec = ok_exec();
        // Socket-dir split (Bug D): after run_detached the session appears NOT in
        // canonical but in a LEGACY dir — so the cross-dir scan finds it tagged
        // with the legacy dir (non-canonical socket_dir). StagedMux models the
        // post-create timeline so the live-name pre-check still passes (empty
        // before creation).
        let legacy = fix._home.path().join("legacy-zmx-501");
        let mux = StagedMux::new(legacy.clone(), "sess");
        let d = deps_with_legacy(&fix, &exec, &mux, &OkBootWaiter, vec![legacy.clone()]);
        let err = run_new(&d, &params("sess")).unwrap_err();
        match &err {
            NewError::SocketDirSplit {
                name,
                found,
                canonical,
            } => {
                assert_eq!(name, "sess");
                assert_eq!(found, &legacy);
                assert_eq!(canonical, &fix.canonical);
            }
            other => panic!("expected SocketDirSplit, got {other:?}"),
        }
        // Reap at the FOUND (legacy) dir, not canonical.
        let kill = mux.calls().into_iter().find(|c| c.verb == "kill").unwrap();
        assert_eq!(kill.socket_dir, legacy);
    }

    #[test]
    fn boot_timeout_maps_to_error_no_reap() {
        struct FailWaiter;
        impl BootWaiter for FailWaiter {
            fn wait_ready(&self, _name: &str) -> Result<(), crate::boot::BootFailure> {
                Err(crate::boot::BootFailure {
                    phase: crate::boot::BootPhase::PidFile,
                    detail: "PID file never appeared".to_string(),
                })
            }
        }
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let err = run_new(&deps(&fix, &exec, &mux, &FailWaiter), &params("sess")).unwrap_err();
        match &err {
            NewError::BootTimeout {
                name,
                phase,
                detail,
            } => {
                assert_eq!(name, "sess");
                assert_eq!(*phase, crate::boot::BootPhase::PidFile);
                assert!(detail.contains("PID file"));
            }
            other => panic!("expected BootTimeout, got {other:?}"),
        }
        // The session is NOT reaped on boot timeout (TS tells user to attach).
        assert!(!mux.calls().iter().any(|c| c.verb == "kill"));
    }

    #[test]
    fn zmx_run_nonzero_maps_to_run_failed() {
        // A mux that returns a nonzero run_detached. We need a custom mux for
        // this since FixtureMux always returns 0; use a tiny inline impl.
        struct FailRunMux;
        impl Mux for FailRunMux {
            fn list(&self, _d: &Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
                Ok(vec![])
            }
            fn list_raw(&self, _d: &Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
                Ok(vec![])
            }
            fn run_detached(
                &self,
                _d: &Path,
                _n: &str,
                _c: &str,
                _w: &Path,
            ) -> std::io::Result<crate::exec::ExecResult> {
                Ok(crate::exec::ExecResult {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "boom from zmx".to_string(),
                    timed_out: false,
                })
            }
            fn send(
                &self,
                _d: &Path,
                _n: &str,
                _t: &str,
            ) -> std::io::Result<crate::exec::ExecResult> {
                unreachable!()
            }
            fn kill(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
                Ok(0)
            }
            fn history(&self, _d: &Path, _n: &str) -> std::io::Result<String> {
                Ok(String::new())
            }
            fn wait(&self, _d: &Path, _n: &[String]) -> std::io::Result<i32> {
                Ok(0)
            }
            fn attach(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
                Ok(0)
            }
        }
        let fix = fixture();
        let exec = ok_exec();
        let mux = FailRunMux;
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("sess")).unwrap_err();
        match &err {
            NewError::ZmxRunFailed(stderr) => assert_eq!(stderr, "boom from zmx"),
            other => panic!("expected ZmxRunFailed, got {other:?}"),
        }
    }

    /// C1 M4fix item 3/5: a `run_detached` SPAWN error (Err, not nonzero status)
    /// maps backend-aware. A mux whose run_detached errors stands in for an
    /// unlaunchable daemon. Under Embedded → `EmbeddedDaemonLaunchFailed` carrying
    /// the underlying detail + an qrmux-named message; under Zmx → the byte-stable
    /// `ZmxMissing` guidance (G-NEG unchanged). This is the misleading-error
    /// negative control: it proves the embedded message isn't dead text.
    #[test]
    fn run_detached_spawn_err_maps_backend_aware() {
        struct ErrRunMux;
        impl Mux for ErrRunMux {
            fn list(&self, _d: &Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
                Ok(vec![])
            }
            fn list_raw(&self, _d: &Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
                Ok(vec![])
            }
            fn run_detached(
                &self,
                _d: &Path,
                _n: &str,
                _c: &str,
                _w: &Path,
            ) -> std::io::Result<crate::exec::ExecResult> {
                Err(std::io::Error::other("daemon program not found"))
            }
            fn send(
                &self,
                _d: &Path,
                _n: &str,
                _t: &str,
            ) -> std::io::Result<crate::exec::ExecResult> {
                unreachable!()
            }
            fn kill(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
                Ok(0)
            }
            fn history(&self, _d: &Path, _n: &str) -> std::io::Result<String> {
                Ok(String::new())
            }
            fn wait(&self, _d: &Path, _n: &[String]) -> std::io::Result<i32> {
                Ok(0)
            }
            fn attach(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
                Ok(0)
            }
        }
        let fix = fixture();
        let exec = ok_exec();
        let mux = ErrRunMux;

        // Embedded lane: qrmux-named error carrying the underlying detail.
        let mut d = deps(&fix, &exec, &mux, &OkBootWaiter);
        d.backend = crate::mux_selector::Backend::Embedded;
        let err = run_new(&d, &params("sess")).unwrap_err();
        match &err {
            NewError::EmbeddedDaemonLaunchFailed(detail) => {
                assert!(
                    detail.contains("daemon program not found"),
                    "carries the underlying error: {detail}"
                );
            }
            other => panic!("expected EmbeddedDaemonLaunchFailed, got {other:?}"),
        }
        let text = err.to_string();
        assert!(
            text.contains("embedded qrmux daemon") && !text.to_lowercase().contains("zmx"),
            "embedded message names qrmux, not zmx: {text}"
        );

        // Zmx lane: byte-stable missing-binary guidance (G-NEG unchanged).
        let mut dz = deps(&fix, &exec, &mux, &OkBootWaiter);
        dz.backend = crate::mux_selector::Backend::Zmx;
        let errz = run_new(&dz, &params("sess")).unwrap_err();
        assert!(
            matches!(errz, NewError::ZmxMissing(_)),
            "zmx lane keeps ZmxMissing: {errz:?}"
        );
        assert_eq!(errz.to_string(), preflight::zmx_missing_guidance());
    }

    // --- §3.6 name-reject carry (redteam-retro #4) ------------------------

    #[test]
    fn name_reject_each_class_zero_mux() {
        // POST-S2 LAYERING (spec §2.1 reconciliation): S2's charset whitelist now
        // runs FIRST, so separator / NUL classes are caught by S2 (NameUnsafeS2);
        // only the `..`-family (which PASSES S2 because dots are allowed) reaches
        // reject_unsafe_name (NameRejected). EVERY class still exits 1 with ZERO
        // mux calls (nothing created) — the property the row guards.
        //
        // (bad, expected-gate): "s2" = caught by the charset whitelist,
        // "reject" = passed S2, caught by the `..` defense-in-depth.
        for (bad, gate) in [
            ("with/slash", "s2"),
            ("back\\slash", "s2"),
            ("dot..dot", "reject"),
            ("nul\0byte", "s2"),
        ] {
            let fix = fixture();
            let exec = ok_exec();
            let mux = FixtureMux::new();
            let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params(bad)).unwrap_err();
            match (gate, &err) {
                ("s2", NewError::NameUnsafeS2 { message }) => {
                    assert!(
                        message.contains("unsafe characters"),
                        "S2 msg {message:?} for {bad:?}"
                    );
                }
                ("reject", NewError::NameRejected { name, reason }) => {
                    assert_eq!(name, bad);
                    assert!(reason.contains("'..'"), "reason {reason:?} for {bad:?}");
                }
                (g, other) => panic!("expected gate {g:?} for {bad:?}, got {other:?}"),
            }
            assert_eq!(err.exit_code(), 1);
            // ZERO mux calls — rejected at the boundary, before preflight/claim/zmx.
            assert!(
                mux.calls().is_empty(),
                "no mux calls on rejection of {bad:?}"
            );
        }
    }

    #[test]
    fn name_reject_redteam_pair_first_rejected_second_fine() {
        // The redteam-retro #4 collision pair: `../../etc/passwd` (rejected) vs
        // `etcpasswd` (a plain name that boots fine). They used to collide on one
        // sanitized stem; now the first never reaches the claim.
        let fix = fixture();
        let exec = ok_exec();
        let mux = FixtureMux::new();
        let err = run_new(
            &deps(&fix, &exec, &mux, &OkBootWaiter),
            &params("../../etc/passwd"),
        )
        .unwrap_err();
        // POST-S2: `../../etc/passwd` contains `/`, so S2's charset whitelist now
        // catches it FIRST (NameUnsafeS2) — still rejected before the claim/zmx,
        // still zero mux calls. (The `..` defense-in-depth would also catch it,
        // but S2 fires first now.) Either way the dangerous name never boots.
        assert!(matches!(err, NewError::NameUnsafeS2 { .. }));
        assert!(mux.calls().is_empty());

        // `etcpasswd` is a normal name — boots through (StagedMux models the
        // post-create timeline).
        let fix2 = fixture();
        let exec2 = ok_exec();
        let mux2 = StagedMux::new(fix2.canonical.clone(), "etcpasswd");
        let out = run_new(
            &deps(&fix2, &exec2, &mux2, &OkBootWaiter),
            &params("etcpasswd"),
        )
        .unwrap();
        assert_eq!(out.name, "etcpasswd");
    }

    #[test]
    fn name_reject_plain_names_unaffected() {
        // Names with dots, dashes, underscores but no separator/`..`/NUL pass the
        // gate (None) — only the dangerous classes are rejected.
        for ok in ["wk", "my-worker", "agent_1", "qd.rust", "v1.2.3", "a.b.c"] {
            assert_eq!(reject_unsafe_name(ok), None, "{ok:?} must be accepted");
        }
        // And the dangerous classes are caught by the pure helper.
        assert!(reject_unsafe_name("a/b").is_some());
        assert!(reject_unsafe_name("a\\b").is_some());
        assert!(reject_unsafe_name("..").is_some());
        assert!(reject_unsafe_name("x\0y").is_some());
    }

    #[test]
    fn error_display_byte_parity_with_ts() {
        // I6 not-attachable wording (lifecycle.ts:762-766).
        let e = NewError::NotAttachable {
            name: "s".to_string(),
            canonical: PathBuf::from("/tmp/zmx-501"),
        };
        let msg = e.to_string();
        assert!(msg.contains("is not attachable in the zmx socket dir (/tmp/zmx-501)"));
        assert!(msg.contains("registration failed (Bug D)"));
        // run-failed wording (utils.ts:371).
        let e = NewError::ZmxRunFailed("err text".to_string());
        assert_eq!(e.to_string(), "Failed to create session: err text");
    }

    // --- G-A1b: S2-at-new name-class MATRIX (spec §7 G-A1b; orc-4 rider #3) ---
    // Three name classes, each asserting the exact exit code AND the exact stderr
    // wording (the Display string the verb prints). Before/after for the
    // reconciliation-delta class (b) is recorded in the per-row comments.

    #[test]
    fn g_a1b_class_a_rejected_by_both_s2_first() {
        // CLASS (a) rejected-by-BOTH gates: S2 fires first (charset), so the
        // surfaced error is NameUnsafeS2 with the ported `ERROR: ...` shape.
        // Empty name → S2's empty-name message; `/`-containing → S2 charset msg.
        let fix = fixture();
        let exec = ok_exec();
        let mux = FixtureMux::new();

        // empty name (BEFORE: reject_unsafe_name passed empty → it would reach the
        // claim with a degenerate name; AFTER: S2 rejects empty first).
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params("")).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.to_string(), "ERROR: Session name must not be empty.");
        assert!(mux.calls().is_empty());

        // `/`-containing: S2 charset rejection, exact wording.
        let fix2 = fixture();
        let exec2 = ok_exec();
        let mux2 = FixtureMux::new();
        let err2 = run_new(&deps(&fix2, &exec2, &mux2, &OkBootWaiter), &params("a/b")).unwrap_err();
        assert_eq!(err2.exit_code(), 1);
        assert_eq!(
            err2.to_string(),
            "ERROR: Session name \"a/b\" contains unsafe characters. \
             Names may only contain letters, digits, hyphens, underscores, and dots."
        );
        assert!(mux2.calls().is_empty());
    }

    #[test]
    fn g_a1b_class_b_formerly_accepted_now_rejected() {
        // CLASS (b) the RECONCILIATION DELTA: names that BOOTED before S2-at-new
        // and now exit 1. This is the behavior change orc-4 ruled IN as
        // pin-reconciliation (it matches TS at pin, which always validated here).
        //
        //   BEFORE (no S2-at-new): `a'b` and `a b` passed reject_unsafe_name (no
        //   separator/`..`/NUL) and reached the claim → booted (with a `'` that
        //   could break the env-file single-quote prefix — the risk S2 closes).
        //   AFTER: both exit 1 with the ported S2 `ERROR: ... unsafe characters`.
        for bad in ["a'b", "a b"] {
            let fix = fixture();
            let exec = ok_exec();
            let mux = FixtureMux::new();
            let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params(bad)).unwrap_err();
            assert_eq!(err.exit_code(), 1, "{bad:?} must exit 1 after S2-at-new");
            assert_eq!(
                err.to_string(),
                format!(
                    "ERROR: Session name \"{bad}\" contains unsafe characters. \
                     Names may only contain letters, digits, hyphens, underscores, and dots."
                )
            );
            // Nothing created — rejected before the claim/zmx.
            assert!(mux.calls().is_empty(), "{bad:?} must create nothing");
        }
    }

    #[test]
    fn g_a1b_class_c_rust_only_dotdot_family() {
        // CLASS (c) the KEPT Rust-only divergence (redteam-retro #4): `..`-family
        // names PASS S2 (dots are in the whitelist) and are caught ONLY by
        // reject_unsafe_name → NameRejected. Pin TS ACCEPTS these (its S2 allows
        // dots); Rust's extra `..` rejection is the documented safety divergence.
        for bad in ["..", "a..b"] {
            let fix = fixture();
            let exec = ok_exec();
            let mux = FixtureMux::new();
            let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &params(bad)).unwrap_err();
            assert_eq!(err.exit_code(), 1);
            match &err {
                NewError::NameRejected { name, reason } => {
                    assert_eq!(name, bad);
                    assert!(reason.contains("'..'"), "reason {reason:?} for {bad:?}");
                }
                other => panic!("class (c) {bad:?} must be NameRejected, got {other:?}"),
            }
            // Exact Display (the `..` reject wording).
            assert!(
                err.to_string().contains("contains '..'"),
                "display: {}",
                err
            );
            assert!(mux.calls().is_empty());
        }
        // PROOF the divergence is real: pin S2 would ACCEPT `a..b` (it passes the
        // charset whitelist) — so without reject_unsafe_name it would boot.
        assert_eq!(
            s2_validate_new_name("a..b"),
            None,
            "S2 allows dots → `..` passes S2"
        );
    }

    // --- F1 create-path wiring (spec §2.2; G-A1 unit leg + G-A2 positive) ----

    #[test]
    fn f1_empty_backend_env_adds_no_exports_cmd_is_prefix_plus_base() {
        // G-A1 unit leg, R2-reshaped (there is no content-free launch anymore:
        // every launch asserts the render property — inline by export,
        // alt-screen by explicit unset — and item 1's FORCE birth property
        // always). The F1 negative control's surviving truth: an EMPTY backend
        // capture adds NO BACKEND export lines, and the launched cmd is EXACTLY
        // the env prefix + the base composition (the only deltas are the engine
        // birth-property mechanisms, never a captured backend var).
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let mut p = params("sess"); // backend_env defaults to empty
        p.render = RenderMode::AltScreen;
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &p).unwrap();

        // No BACKEND exports — the file is exactly the render unset plus the
        // unconditional FORCE birth property (no ANTHROPIC_* capture rode in).
        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "sess");
        let body = std::fs::read_to_string(&env_file).unwrap();
        assert!(
            !body.contains("export ANTHROPIC"),
            "empty capture must add no backend exports: {body}"
        );
        assert_eq!(
            body,
            "unset -v CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN\n\
             export CLAUDE_CODE_FORCE_SESSION_PERSISTENCE='1'\n",
            "alt-screen empty-capture body = render unset + FORCE only: {body}"
        );
        // The cmd is prefix + base, byte-exactly — nothing else rode in.
        let base = build_claude_command(&deps(&fix, &exec, &mux, &OkBootWaiter), &p);
        let prefix = session_env_prefix(
            &fix.paths.home,
            "sess",
            &[],
            &["CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".to_string()],
        );
        assert_eq!(out.claude_cmd, format!("{prefix}{base}"));
    }

    #[test]
    fn f1_nonempty_writes_0600_file_and_prefixes_cmd() {
        // G-A2 positive (unit leg): non-empty backend_env → a 0600 env file with
        // the exact exported content, AND the launched cmd carries the
        // self-deleting dot-source prefix referencing that file (value never in
        // the cmd — only the file PATH).
        use std::os::unix::fs::PermissionsExt;
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let mut p = params("sess");
        p.backend_env = vec![
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "http://127.0.0.1:3456".to_string(),
            ),
            // Obviously-FAKE value (credential hard line: never a real-looking key).
            (
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "sk-FAKE-test".to_string(),
            ),
        ];
        let out = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &p).unwrap();

        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "sess");
        assert!(env_file.exists(), "env file must be written");
        // 0600.
        let mode = std::fs::metadata(&env_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "env file must be 0600, got {mode:o}");
        // Content: exact export lines.
        let body = std::fs::read_to_string(&env_file).unwrap();
        assert!(body.contains("export ANTHROPIC_BASE_URL='http://127.0.0.1:3456'"));
        assert!(body.contains("export ANTHROPIC_AUTH_TOKEN='sk-FAKE-test'"));
        // The launched cmd carries the prefix (file PATH) but NOT the value.
        assert!(out
            .claude_cmd
            .contains(&env_file.to_string_lossy().to_string()));
        assert!(
            !out.claude_cmd.contains("sk-FAKE-test"),
            "credential value must NEVER appear in the launch cmd: {}",
            out.claude_cmd
        );
    }

    #[test]
    fn f1_cleanup_removes_env_file_on_prelaunch_failure() {
        // Spec §2.3 cleanup parity, F1-rescoped (red-team r1): a PRE-LAUNCH
        // failure (here: nonzero `zmx run` → ZmxRunFailed) means nothing was
        // launched and nothing will ever source the file — it is removed so a
        // credential is not left at rest.
        struct FailRunMux;
        impl Mux for FailRunMux {
            fn list(&self, _d: &Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
                Ok(vec![])
            }
            fn list_raw(&self, _d: &Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
                Ok(vec![])
            }
            fn run_detached(
                &self,
                _d: &Path,
                _n: &str,
                _c: &str,
                _w: &Path,
            ) -> std::io::Result<crate::exec::ExecResult> {
                Ok(crate::exec::ExecResult {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "boom".to_string(),
                    timed_out: false,
                })
            }
            fn send(
                &self,
                _d: &Path,
                _n: &str,
                _t: &str,
            ) -> std::io::Result<crate::exec::ExecResult> {
                unreachable!()
            }
            fn kill(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
                Ok(0)
            }
            fn history(&self, _d: &Path, _n: &str) -> std::io::Result<String> {
                Ok(String::new())
            }
            fn wait(&self, _d: &Path, _n: &[String]) -> std::io::Result<i32> {
                Ok(0)
            }
            fn attach(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
                Ok(0)
            }
        }
        let fix = fixture();
        let exec = ok_exec();
        let mux = FailRunMux;
        let mut p = params("sess");
        p.backend_env = vec![("ANTHROPIC_BASE_URL".to_string(), "http://x:1".to_string())];
        let err = run_new(&deps(&fix, &exec, &mux, &OkBootWaiter), &p).unwrap_err();
        assert!(matches!(err, NewError::ZmxRunFailed(_)));
        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "sess");
        assert!(
            !env_file.exists(),
            "env file must be cleaned up on a pre-launch failure"
        );
    }

    /// F1 amplifier pin (red-team r1): the env file SURVIVES a POST-LAUNCH
    /// failure. The pane's `bash -lc` may not have dot-sourced the file yet —
    /// the prefix is fail-closed (`. file || exit 97`), so deleting it on a
    /// (possibly FALSE) death verdict / boot timeout killed healthy sessions.
    /// Covers both post-launch classes: the boot-wait Err and the I6 reap.
    #[test]
    fn f1_env_file_survives_postlaunch_failures() {
        struct FailWaiter2;
        impl BootWaiter for FailWaiter2 {
            fn wait_ready(&self, _name: &str) -> Result<(), crate::boot::BootFailure> {
                Err(crate::boot::BootFailure {
                    phase: crate::boot::BootPhase::PidFile,
                    detail: "session pane \"sess\" is gone (verdict)".to_string(),
                })
            }
        }
        // (a) Boot-wait failure (the death verdict / timeout shape).
        let fix = fixture();
        let exec = ok_exec();
        let mux = StagedMux::new(fix.canonical.clone(), "sess");
        let mut p = params("sess");
        p.backend_env = vec![("ANTHROPIC_BASE_URL".to_string(), "http://x:1".to_string())];
        let err = run_new(&deps(&fix, &exec, &mux, &FailWaiter2), &p).unwrap_err();
        assert!(matches!(err, NewError::BootTimeout { .. }));
        let env_file = crate::launch::session_env_file_path(&fix.paths.home, "sess");
        assert!(
            env_file.exists(),
            "a post-launch verdict must NOT delete the env file (a false verdict \
             would kill the healthy pane via the fail-closed prefix)"
        );

        // (b) I6 NotAttachable (post-launch, session reaped) — also survives;
        // the leak is bounded by the next same-name launch's unlink-first.
        let fix2 = fixture();
        let exec2 = ok_exec();
        let mux2 = FixtureMux::new(); // empty canonical list → NotAttachable
        let mut p2 = params("sess");
        p2.backend_env = vec![("ANTHROPIC_BASE_URL".to_string(), "http://x:1".to_string())];
        let err2 = run_new(&deps(&fix2, &exec2, &mux2, &OkBootWaiter), &p2).unwrap_err();
        assert!(matches!(err2, NewError::NotAttachable { .. }));
        assert!(crate::launch::session_env_file_path(&fix2.paths.home, "sess").exists());
    }

    // --- codex P1 W3 (codex-p1-spec section 7.1 step 3): byte-identity pin ----

    /// The provider-routed launch cmd `build_claude_command` produces is
    /// BYTE-IDENTICAL to the PRE-REWIRE assembly (`build_claude_cmd(claude_bin,
    /// claude_flags, build_new_extra_args)`) for a representative `NewParams`
    /// exercising resume + fork + agent + passthrough — the full
    /// `build_new_extra_args` shape. This is the W3 obligation: routing through
    /// `provider.launch_plan(fx, req)` + `build_claude_cmd_from_argv` must not move
    /// a single byte of the shell command the mux receives.
    ///
    /// MUTATION EVIDENCE: any provider-impl drift — an argv reorder, a dropped
    /// flag, a different bin/flags resolution, or a quoting change in
    /// `build_claude_cmd_from_argv` — reds this (the routed string would diverge
    /// from the hand-built reference). It is the create-side twin of the
    /// conformance lane's `claude_launch_plan_matches_launch_rs_helpers`.
    #[test]
    fn provider_routed_cmd_equals_prerewire_assembly() {
        use crate::launch::{build_claude_cmd, build_new_extra_args, claude_bin, claude_flags};

        let fix = fixture();
        let mut p = params("wk");
        p.resume = Some("sess-1".to_string());
        p.fork = true;
        p.agent = Some("reviewer".to_string());
        p.claude_args = vec!["--model".to_string(), "opus".to_string()];

        let exec = ok_exec();
        let mux = FixtureMux::new();
        let deps = deps(&fix, &exec, &mux, &OkBootWaiter);

        // The PRE-REWIRE reference: exactly what build_claude_command did before
        // routing through the provider — claude_bin + claude_flags (off the SAME
        // config path the provider derives, `paths.home/.quorum/dispatch/config.toml`,
        // nonexistent → DEFAULT_FLAGS) + build_new_extra_args.
        let bin = claude_bin(&fix.env);
        let config = fix
            .paths
            .home
            .join(".quorum")
            .join("dispatch")
            .join("config.toml");
        let flags = claude_flags(&fix.env, &config);
        let opts = crate::launch::NewOpts {
            resume: p.resume.clone(),
            fork: p.fork,
            agent: p.agent.clone(),
            model: p.model.clone(),
        };
        let extra = build_new_extra_args(&p.name, &opts, &p.claude_args, &flags);
        let expected = build_claude_cmd(&bin, &flags, &extra);

        let routed = build_claude_command(&deps, &p);
        assert_eq!(
            routed, expected,
            "provider-routed launch cmd must be byte-identical to the pre-rewire assembly"
        );
    }
}
