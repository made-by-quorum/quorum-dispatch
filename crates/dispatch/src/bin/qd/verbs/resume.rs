//! REAL `qd resume` backend (spec §5.3; TS `commands/lifecycle.ts:408-530`).
//!
//! Relaunch a COLD session in zmx (by default). The pure preflight deciders live
//! in `dispatch::resume`; this verb drives the live effects:
//!   - OC refusal (server-managed) → must-be-cold,
//!   - F3 cwd reality-check (clean error, never raw ENOENT),
//!   - F1 env-file capture (the launch.rs mechanism) + S2 zmx-name validation,
//!   - kill a stale same-name zmx (destructive sub-step),
//!   - launch: --no-zmx bare / --no-attach detached + ready-wait / default attach.
//!
//! The claude relaunch flag is `--resume <session-id>` exactly as TS's
//! `buildClaudeCmd(["--resume", session.sessionId])` does at the resume call-site
//! (lifecycle.ts:474). Ready-wait keys on the PID-file/busy EVENT (ADR 0005 —
//! zero blind keystrokes), reusing the A2 EventBootWaiter. Exit inherits the
//! child / 1 on a preflight error.

use std::path::{Path, PathBuf};

use clap::ArgMatches;

use dispatch::boot::{EventBootWaiter, RealSleeper};
use dispatch::create::BootWaiter;
use dispatch::effects::{Env, RealClock, RealEnv};
use dispatch::exec::RealExec;
use dispatch::join::JoinOpts;
use dispatch::launch::{
    build_claude_cmd, capture_backend_env, claude_bin, claude_flags, launch_env_pairs,
    session_env_prefix, write_session_env_file_with_unsets, RenderMode,
};
use dispatch::model::SessionStatus;
use dispatch::mux::Mux;
use dispatch::paths::SbPaths;
use dispatch::resume::{derive_zmx_name, resolve_resume_cwd, validate_session_name, ResumeCwd};
use dispatch::zmx_dir::{legacy_zmx_dirs, resolve_zmx_dir, XdgFamily};

use super::common;
use super::common::resolve_or_die;

/// The `--no-attach` boot-confirm-failure stderr line (NAMED DIVERGENCE, ADD-9a).
/// Factored so the EXACT wording is pinned by a unit test: m-4 (ack3-spec §8)
/// retyped `wait_ready` to return a typed `BootFailure`, and this line must stay
/// byte-identical to the pre-m-4 form (it printed the waiter's `String` directly;
/// now it prints `failure.detail`). The contract: prefix + the waiter detail.
fn resume_boot_unconfirmed_line(detail: &str) -> String {
    format!("qd resume: session launched but did not confirm ready: {detail}")
}

/// W2 send-pointer: the codex-resume success lines point the agent at the WORKING
/// channel, `qd send:relay` (bare `qd send` is a moved stub; `send:pty` has no pane
/// for a daemon-hosted codex session). Factored so the EXACT pointer is pinned by a
/// unit test (mirrors `resume_boot_unconfirmed_line`).
fn codex_already_running_line(name: &str) -> String {
    format!("session \"{name}\" is running; send to it with: qd send:relay {name} <text>")
}

fn codex_revived_line(name: &str, pid: i64, endpoint: &str) -> String {
    format!(
        "resumed codex session \"{name}\" (daemon pid {pid}, {endpoint}); \
         send to it with: qd send:relay {name} <text>"
    )
}

/// Item 3 RESUME (acp) — the AlreadyRunning no-op line. A genuinely-alive acp row is
/// drivable RIGHT NOW; resume is a success no-op (NO second adapter, ZERO row mutation).
/// Mirrors `codex_already_running_line`; pinned by a unit test.
fn acp_already_running_line(name: &str) -> String {
    format!("session \"{name}\" is already alive; send to it with: qd send:relay {name} <text>")
}

/// Item 3 RESUME (acp) — the revived line. The resident adapter was re-spawned in
/// LOAD mode (real `session/load`, SAME sessionId, the CC conversation continues).
fn acp_revived_line(name: &str, pid: i64, endpoint: &str) -> String {
    format!(
        "resumed acp session \"{name}\" (adapter pid {pid}, {endpoint}); \
         send to it with: qd send:relay {name} <text>"
    )
}

/// `qd resume <session>` — cold-session relaunch.
pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    // D3 (Fork A): `--no-zmx` / `--no-attach` are the dropped interactive/PTY
    // escapes — resume is ALWAYS headless now, so they are no longer read here
    // (the flags stay PARSE-ACCEPTED in cli.rs so scripted callers don't break;
    // they are inert on the headless route). `--alt-screen`/`--inline` (render) and
    // the zmx render mode are likewise interactive-TUI concerns, not consulted on
    // the headless path.
    let zmx_name_opt = m.get_one::<String>("zmx-name").map(|s| s.as_str());
    let cwd_override = m.get_one::<String>("cwd").map(|s| s.as_str());

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd resume: HOME is not set — cannot resolve the session state dir.");
            return 1;
        }
    };
    let paths = SbPaths::from_home(&home);

    // Resolve via A1 (includeAll so a cold session resolves).
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: false,
        limit: Some(50),
    };
    let sessions = match common::all_sessions(opts) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let session = match resolve_or_die(query, &sessions) {
        Ok(s) => s.clone(),
        Err(code) => return code,
    };

    // OpenCode refusal (lifecycle.ts:415-435) — parked: the A1 join design only
    // constructs provider:"claude-code" rows (model.rs:92 + join.rs sites), so this
    // branch is structurally unreachable until an OC-join lands (ADD-9a). Kept
    // structurally honest. NOTE: spec §2 carries no OC exclusion — the real ground
    // is the A1 join design.
    if session.provider == "opencode" {
        eprintln!("qd resume: OpenCode resume is not supported in the Rust engine (parked).");
        return 1;
    }
    // codex P2 W7 (codex-p2-spec §7.6; ADD-26(2)): a codex row is DAEMON-hosted —
    // resume is a first-class AGENT verb = thread/resume revive-to-DRIVABLE with NO
    // interactive-attach tail (agents have no TTY; attach/--remote is SEVERED). This
    // THIN dispatch branch routes to the NEW resume_daemon module and RETURNS before
    // ANY claude attach/resume internals (the R-a hot-file discipline). Placed BEFORE
    // refuse_unknown_provider (which would otherwise refuse "codex" as unknown) and
    // before the must-be-cold gate (codex revive is drivable from any non-alive state,
    // not just Cold).
    if session.provider == "codex" {
        return run_codex_resume(&session);
    }
    // scoped-ACP-CC Item 3 (RESUME): an acp/* row is ALSO daemon-hosted (the resident
    // `qd acp-daemon` adapter + its bridge). Resume re-establishes the SAME CC session
    // via real `session/load` (Component-0-proven faithful), mirroring the codex revive.
    // Placed BESIDE the codex branch — BEFORE refuse_unknown_provider (which would
    // refuse "acp/claude-code" as unknown) and before the must-be-cold gate (a
    // daemon-hosted row is revivable from any non-alive state, incl. a tombstoned stop).
    if session.provider.starts_with("acp/") {
        return run_acp_resume(&session);
    }
    // codex P1, R1 (codex-p1-spec section 2.3): refuse an unknown provider LOUDLY.
    if let Some(code) = common::refuse_unknown_provider("resume", &session) {
        return code;
    }

    // Pete feedback #6 — live-id-collision preflight over the RAW registry. The
    // deduped join collapses two same-id LIVE rows to one (hiding a genuine
    // duplicate-id collision) and can report the survivor Cold via dedup of a stale
    // row. We check the unmerged truth (raw rows + is_pid_alive) BEFORE the must-be-
    // cold gate so a collision is surfaced even when the join reports the survivor
    // busy/idle, and so a Cold-MISREAD of an actually-live session is refused (it
    // would otherwise spawn a SECOND process on the same id — the orchestrator
    // revival-ladder hazard). SHARED with connect via `common::refuse_id_collision`.
    if let Some(code) =
        common::refuse_id_collision("resume", &session.session_id, &paths.sessions_dir)
    {
        return code;
    }
    if let Some(pid) = common::alive_pid_for_id(&paths.sessions_dir, &session.session_id) {
        eprintln!(
            "qd resume: session \"{}\" is already alive (PID {pid}). Use \"qd connect\" instead.",
            session.name.as_deref().unwrap_or(&session.session_id)
        );
        return 1;
    }

    // Must be cold (lifecycle.ts:437-441). Retained as the byte-stable fast-path +
    // the stale-status edge the pid-based preflight above does not cover (a row with
    // a non-Cold status STRING whose pid is already dead → 0 alive rows above).
    if session.status != SessionStatus::Cold {
        // P0 qafix R3 (orc ruling 2026-06-10): a tombstoned (Killed) row that is
        // NOT resumable (no provider session id, or no transcript) has NOTHING to
        // resume — "still alive" would be a false statement of fact. The gate's
        // LOGIC is unchanged (anything non-Cold refuses, exit 1); only this arm's
        // message states the true condition. Genuinely-alive statuses keep the
        // byte-pinned pointer (now `qd connect` — attach retired, STATE 22).
        // (Join structure note: a killed session WHOSE
        // TRANSCRIPT EXISTS surfaces as a ColdJsonl row — Cold, resumable, never
        // here; the Killed branch emits only sids no transcript row claimed. The
        // jsonl_path guard keeps this arm honest if that ever drifts.)
        if session.status == SessionStatus::Killed
            && (session.session_id.is_empty() || session.jsonl_path.is_none())
        {
            eprintln!(
                "qd resume: session \"{}\" was stopped and has no resumable transcript — \
                 nothing to resume.",
                session.name.as_deref().unwrap_or(query)
            );
            return 1;
        }
        eprintln!(
            "Session is still alive (status: {}). Use \"qd connect\" instead.",
            session.status.as_str()
        );
        return 1;
    }

    if session.session_id.is_empty() {
        eprintln!("Cannot resume: no session ID found.");
        return 1;
    }

    // F3: cwd reality-check BEFORE any spawn (lifecycle.ts:451-462). A
    // renamed/deleted project dir → clean actionable error, never raw ENOENT.
    let fallback = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let exists = |p: &str| Path::new(p).exists();
    // D3 (headless): the recorded cwd is still validated (refuse a vanished project
    // dir — keep the F3 safety), but the resumed headless session inherits the
    // daemon's cwd; per-session cwd threading into LaunchHeadless is daemon-side
    // (deferred + flagged), so the validated value itself is not consumed here.
    if let ResumeCwd::Error(e) =
        resolve_resume_cwd(session.cwd.as_deref(), cwd_override, &exists, &fallback)
    {
        eprintln!("ERROR: {e}");
        return 1;
    }

    // --- D3 (WP-B-CS-1, Fork A): qd resume is an AGENT verb — ALWAYS headless. ---
    // FULL REPLACEMENT of the zmx/PTY relaunch + interactive attach: route to the
    // per-session qrmux daemon's LaunchHeadless with resume_session_id = the
    // session's provider id, so the revived session rides the headless stream-json
    // channel (the §6.0 intent — an agent-driven session is NOT on a PTY/zmx
    // surface). The live-ownership lock / id-collision / must-be-cold / cwd
    // preflight ABOVE is preserved (non-negotiable). There is NO `--interactive`
    // escape here, even at a TTY: a human re-entering a session is `qd connect`
    // (WP-B-CS-2), not `qd resume` (`driver::resume_is_headless` documents the
    // always-headless decision; the resolver is intentionally not consulted).
    //
    // GUARDRAIL 1 (do not orphan-rot): the native-interactive zmx/PTY revive
    // helpers (`prepare_claude_resume_env` / `run_detached_revive` / `revive_claude`)
    // are NOT dead and need NO `#[allow(dead_code)]` — `qd connect` already calls
    // `revive_claude` (connect.rs:65 → it uses both other helpers), and WP-B-CS-2's
    // turn-boundary cutover reuses exactly that native-TUI revive+attach. They are
    // KEPT in place (+ their unit tests stay green), never deleted.
    //
    // GUARDRAIL 2 (golden delta): the a5_lifecycle / a5rec_resume SUBPROCESS goldens
    // (separate harness, NOT the cargo floor) encode resume's OLD zmx/attach
    // behavior; this (B) spec deliberately changes it. The delta is recorded in the
    // WP-B-CS-1 response for the red-team + WP-B7 integration; the cargo floor stays
    // green (the gated helpers keep their unit tests live).
    //
    // IDENTITY/ADDRESSABILITY DEFERRED (Fork C escape hatch, lead + supervisor-
    // ratified → B5): this launches the resumed headless session but does not yet
    // mint/bind a registry identity or a daemon-flipped status row, and the prompt
    // is empty (revive-to-headless; the resumed turn's input + per-session cwd are
    // daemon-side, deferred). Launched-but-not-yet-addressable until B5.
    // Consult the single source of resume's I/O policy (driver::resume_is_headless),
    // which ignores the driver — wiring it HERE (not hard-coding `true`) means a
    // regression that makes resume driver-conditional is caught at this call site,
    // not just in the unit test. resolve_driver_real may read Human at a TTY; the
    // policy overrides it (resume is never interactive — that is `qd connect`).
    if !crate::driver::resume_is_headless(crate::driver::resolve_driver_real(
        crate::driver::DriverOverride::None,
        &env,
    )) {
        // Structurally unreachable today; the honest fall-through if the policy ever
        // flips — refuse rather than silently mis-route to a retired interactive path.
        eprintln!("qd resume: internal: resume I/O policy is not headless — use \"qd connect\".");
        return 1;
    }

    let name = derive_zmx_name(zmx_name_opt, session.name.as_deref(), &session.session_id);
    if let Some(err) = validate_session_name(&name) {
        eprintln!("ERROR: {err}");
        return 1;
    }
    println!(
        "Resuming session \"{name}\" (headless) from {}...",
        dispatch::fmt::truncate_id_default(&session.session_id)
    );
    match dispatch::embedded_mux::launch_headless_embedded(
        &home,
        &env,
        &name,
        "",
        Some(&session.session_id),
        // WP-B5-i threads per-session cwd/claudeArgs on the START path only; resume
        // keeps today's behaviour (the revived headless turn inherits the daemon's
        // cwd, no extra passthrough flags) — `None`/empty preserve it byte-for-byte.
        None,
        &[],
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("qd resume: {e}");
            1
        }
    }
}

/// P0 wave-2 (spec-w2-env D1+D4) — the SHARED resume/revive env prep, the exact
/// same sequence on both paths (`run`'s claude path and connect's
/// [`revive_claude`]): resolve the ids store ONCE; run the D4 same-name guard
/// BEFORE any side effect (the spike hazard — the stale-kill must never destroy
/// a DIFFERENT live session's pane; the target's own stale pane keeps the
/// kill-then-relaunch flow); D1 mint/fetch the stable id (the UUID is known
/// here, so `mint_or_get` keys it directly — lazy-mints for pre-stable-id
/// sessions; fail-closed: never relaunch a session whose env would silently
/// miss its identity); write the UNCONDITIONAL self-deleting env file
/// (lifecycle.ts:483-485) carrying `export QD_SESSION_ID='<id>'` (an explicit
/// set, overriding anything inherited through the caller's subtree) plus the
/// captured backend pairs — so the env file + dot-source prefix are
/// unconditional on every resume/revive branch (`--no-zmx` bare, `--no-attach`
/// detached, default attach, and connect's revive all share the one
/// `claude_cmd` this returns). `verb` keys the per-path error wording
/// ("resume" / "connect"); every `Err(code)` has already printed its error.
#[allow(clippy::too_many_arguments)]
fn prepare_claude_resume_env(
    verb: &str,
    env: &dyn Env,
    home: &Path,
    paths: &SbPaths,
    zmx_name: &str,
    session_id: &str,
    session_name: Option<&str>,
    backend_env: Vec<(String, String)>,
    render: RenderMode,
    base_claude_cmd: &str,
) -> Result<String, i32> {
    let ids_path = common::ids_store_path(env)?;
    if let Some(code) =
        common::refuse_held_zmx_name(verb, &paths.sessions_dir, &ids_path, zmx_name, session_id)
    {
        return Err(code);
    }
    let sb_id =
        match dispatch::idstore::mint_or_get(&ids_path, session_id, session_name, &RealClock) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("qd {verb}: could not mint a stable session id: {e}");
                return Err(1);
            }
        };
    // punch item 7: the render-mode birth property rides the SAME shared
    // assembly as create's path (launch_env_pairs — one assembly point, every
    // launch site). R2 (override-never-inherit): an --alt-screen revive must
    // EXPLICITLY `unset -v` the inline var — omitting the export alone leaves
    // the child inheriting it from an inline parent env — so the unset list
    // rides this path too (the with-unsets writer; empty for inline, whose
    // export clobbers anything inherited).
    let env_pairs = launch_env_pairs(backend_env, Some(sb_id), render);
    let env_unsets = dispatch::launch::render_env_unsets(render);
    if let Err(e) = write_session_env_file_with_unsets(home, zmx_name, &env_pairs, &env_unsets) {
        eprintln!("qd {verb}: failed to write session env file: {e}");
        return Err(1);
    }
    let env_prefix = session_env_prefix(home, zmx_name, &env_pairs, &env_unsets);
    Ok(format!("{env_prefix}{base_claude_cmd}"))
}

/// The SHARED detached-revive seam (W1 phase 2): `run_detached` + the ADR-0005
/// EVENT ready-wait, factored out of resume's `--no-attach` branch so `connect`
/// can reuse the EXACT same revive-to-drivable mechanics before its TTY attach.
/// Returns `Ok(())` when the session is detached + confirmed ready, or `Err(code)`
/// (already printed) on a launch / boot-confirm failure. The caller owns any
/// success stdout line (resume's "Resumed detached…" stays at its call site).
/// punch item 6: the revive launch-failure stderr lines, factored so the EXACT
/// wording is pinned by unit tests (the resume_boot_unconfirmed_line pattern).
/// A nonzero `zmx run` carries zmx's stderr; a spawn-level Err is the
/// missing-binary guidance. BOTH fail immediately — the boot waiter never runs
/// on a failed launch (the no-swallow-into-boot-timeout contract).
fn revive_launch_failed_line(stderr: &str) -> String {
    format!("Failed to resume session: {}", stderr.trim())
}

fn revive_zmx_missing_line() -> String {
    "qd resume: could not launch zmx (is it installed and on PATH?).".to_string()
}

fn run_detached_revive(
    mux: &dyn Mux,
    canonical: &Path,
    zmx_name: &str,
    claude_cmd: &str,
    cwd_path: &Path,
    paths: &SbPaths,
) -> Result<(), i32> {
    match mux.run_detached(canonical, zmx_name, claude_cmd, cwd_path) {
        Ok(r) if r.status == Some(0) => {}
        Ok(r) => {
            eprintln!("{}", revive_launch_failed_line(&r.stderr));
            return Err(1);
        }
        Err(_) => {
            eprintln!("{}", revive_zmx_missing_line());
            return Err(1);
        }
    }
    // Ready-wait keys on the PID-file/busy EVENT (ADR 0005 — zero blind
    // keystrokes), reusing the A2 boot waiter.
    let clock = RealClock;
    let sleeper = RealSleeper;
    let waiter = EventBootWaiter::new(
        mux,
        canonical.to_path_buf(),
        paths.sessions_dir.clone(),
        &clock,
        &sleeper,
    );
    if let Err(failure) = waiter.wait_ready(zmx_name) {
        // NAMED DIVERGENCE (loud>silent, ADD-9a): see the historical note at the
        // resume `--no-attach` call site. The Rust ready-wait is the ADR-0005 EVENT
        // waiter, so a timeout genuinely means "boot did not confirm" → exit 1.
        // Pinned byte-identical by `resume_boot_unconfirmed_line`'s unit test.
        eprintln!("{}", resume_boot_unconfirmed_line(&failure.detail));
        return Err(1);
    }
    Ok(())
}

/// A revived claude session's attach coordinates (W1 phase 2): the socket dir +
/// zmx name a caller (`connect`) attaches to AFTER `revive_claude` brings the
/// session up detached + drivable.
pub struct ReviveHandle {
    pub socket_dir: PathBuf,
    pub zmx_name: String,
}

/// WP-B5-ii-b (PROOF 1) — the resume argv fragment a cold-row revive passes to
/// claude, built from the row's RECORDED `session_id` (`model::Session::session_id`
/// — the durable identity the daemon minted onto the child-pid-keyed row). The
/// connect→Cold→revive durability proof pins THIS wiring: the recorded id flows
/// into `--resume <id>` so revive resumes the SAME claude session, never a fresh
/// one. Factored pure (no spawn) so the wiring is unit-testable on the default
/// floor — the cheap mirror of the `#[ignore]` end-to-end seed
/// (`headless_revive_recorded_id.rs`).
///
/// FIX-SHAPED MUTATION (PROOF 1 red-before): replace `id: &session.session_id`
/// with `id: ""` → the fragment loses `--resume <recorded-id>` → revive starts a
/// FRESH claude session → the recorded-id resume proof reds.
fn revive_resume_args(
    provider: &dyn dispatch::provider::Provider,
    session: &dispatch::model::Session,
) -> Vec<String> {
    let resume_key = dispatch::provider::SessionKey {
        id: &session.session_id,
        name: session.name.as_deref(),
        cwd: session.cwd.as_deref(),
        pid: session.pid,
    };
    provider.resume_args(&resume_key, false)
}

/// W1 phase 2 — the SHARED cold→drivable claude revive, callable by `connect` for
/// the human "just works" auto-revive-then-attach path. This factors resume's
/// claude relaunch PREP (cwd reality-check, claude_cmd build via the provider
/// seam, env-file capture, zmx-name derive/validate, backend + canonical-dir
/// resolution, stale-same-name kill) and then drives the SHARED
/// [`run_detached_revive`] seam (run_detached + ADR-0005 ready-wait). On success it
/// returns the [`ReviveHandle`] so the caller can attach the live pane with a plain
/// `mux.attach` (NO fused `zmx attach … bash -lc`). On any failure it has ALREADY
/// printed a loud error and returns `Err(code)`.
///
/// SCOPE: this is the ZMX/embedded detached revive — it deliberately does NOT carry
/// resume's `--no-zmx` bare-exec branch nor resume's fused-default `zmx attach`
/// path (those stay byte-stable in `run`). `connect` always wants detached-then-
/// attach, which is exactly this seam + a follow-up `mux.attach`.
pub fn revive_claude(
    session: &dispatch::model::Session,
    cwd_override: Option<&str>,
    render: RenderMode,
) -> Result<ReviveHandle, i32> {
    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd connect: HOME is not set — cannot resolve the session state dir.");
            return Err(1);
        }
    };
    let paths = SbPaths::from_home(&home);

    if session.session_id.is_empty() {
        eprintln!("Cannot resume: no session ID found.");
        return Err(1);
    }

    // F3: cwd reality-check BEFORE any spawn (lifecycle.ts:451-462).
    let fallback = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let exists = |p: &str| Path::new(p).exists();
    let cwd = match resolve_resume_cwd(session.cwd.as_deref(), cwd_override, &exists, &fallback) {
        ResumeCwd::Cwd(c) => c,
        ResumeCwd::Error(e) => {
            eprintln!("ERROR: {e}");
            return Err(1);
        }
    };

    // The claude relaunch argv via the provider seam (fork=false), identical to
    // resume's claude path.
    let config_toml = home.join(".quorum").join("dispatch").join("config.toml");
    let bin = claude_bin(&env);
    let flags = claude_flags(&env, &config_toml);
    let Some(provider_impl) = dispatch::provider::provider_for(&session.provider) else {
        eprintln!(
            "qd connect: unknown provider \"{}\" — this engine supports: claude-code.",
            session.provider
        );
        return Err(1);
    };
    let extra = revive_resume_args(provider_impl, session);
    let base_claude_cmd = build_claude_cmd(&bin, &flags, &extra);

    // F1: capture backend env + write the self-deleting env file (lifecycle.ts:466-485).
    let backend_env = capture_backend_env(&env);
    let zmx_name = derive_zmx_name(None, session.name.as_deref(), &session.session_id);
    if let Some(err) = validate_session_name(&zmx_name) {
        eprintln!("ERROR: {err}");
        return Err(1);
    }

    // P0 wave-2 (spec-w2-env D1+D4) — IDENTICAL to resume's claude path, via the
    // shared prepare_claude_resume_env ("connect" keys the error wording).
    let claude_cmd = prepare_claude_resume_env(
        "connect",
        &env,
        &home,
        &paths,
        &zmx_name,
        &session.session_id,
        session.name.as_deref(),
        backend_env,
        render,
        &base_claude_cmd,
    )?;

    // Backend + canonical dir (C1 D2/D3).
    let backend = common::select_backend(&env)?;
    let canonical = match backend {
        dispatch::mux_selector::Backend::Zmx => resolve_zmx_dir(&env),
        dispatch::mux_selector::Backend::Embedded => {
            match dispatch::qrmux_dir::resolve_qrmux_dir(&home, &env) {
                Ok(d) => d,
                Err(msg) => {
                    eprintln!("qd connect: {msg}");
                    return Err(1);
                }
            }
        }
    };
    let cwd_path = PathBuf::from(&cwd);

    // Kill a stale same-name session so we get a fresh one (lifecycle.ts:500-505).
    let legacy = match backend {
        dispatch::mux_selector::Backend::Zmx => {
            let scan_roots = dispatch::zmx_dir::legacy_scan_roots(&env, Path::new("/tmp"));
            let xdg = XdgFamily::from_env(&env, env.uid());
            legacy_zmx_dirs(env.uid(), &canonical, &scan_roots, Some(&xdg))
        }
        dispatch::mux_selector::Backend::Embedded => Vec::new(),
    };
    let mux_box = common::build_mux(backend, &home, &env)?;
    let mux: &dyn Mux = mux_box.as_ref();
    let mut dirs = vec![canonical.clone()];
    dirs.extend(legacy);
    // r6 F1: the SAFE stale-pane clear (see `run`'s twin call; same contract).
    common::clear_stale_panes("connect", mux, &dirs, &zmx_name)?;

    // Detached revive + ready-wait via the SHARED seam.
    run_detached_revive(mux, &canonical, &zmx_name, &claude_cmd, &cwd_path, &paths)?;

    Ok(ReviveHandle {
        socket_dir: canonical,
        zmx_name,
    })
}

/// codex P2 W7 (codex-p2-spec §7.6; ADD-26(2)) — the codex RESUME path at the verb
/// layer. A codex row is a daemon-hosted protocol thread; `qd resume` for it is
/// revive-to-DRIVABLE with NO interactive-attach tail (agents have no TTY — attach
/// is SEVERED). ALL revive logic lives in [`dispatch::resume_daemon`]; this is the thin
/// glue: resolve the row's CURRENT pid/endpoint (endpoint is NOT on the
/// `Session`/`--json` surface — re-read by pid), build the production seams, call
/// [`dispatch::resume_daemon::resume_codex`], map the outcome to an agent-facing message.
fn run_codex_resume(session: &dispatch::model::Session) -> i32 {
    use dispatch::create_daemon::{real_alloc_port, real_cmdline_probe, RealDaemonSpawner};
    use dispatch::provider::codex::{AppServerRpc, RpcError, WsAppServer};
    use dispatch::resume_daemon::{resume_codex, ResumeOutcome, ResumeParams, ReviveDeps};

    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());

    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    // The revived daemon's stdout/stderr log root: `<sb_home>/.quorum/dispatch/log` (codex-p2-spec
    // §3.2), resolved off the injected home so a jailed HOME points the log into the
    // jail (L9a) — identical to the W4 create path's resolution.
    let log_dir = paths.home.join(".quorum").join("dispatch").join("log");

    // The current pid/endpoint (the alive-check inputs). endpoint is re-read off the
    // registry row by pid (it is NOT on the Session surface). A row whose pid is dead
    // / absent has no live endpoint → revive.
    let current_endpoint = session
        .pid
        .filter(|&p| p != 0)
        .and_then(|pid| dispatch::registry::read_entry(&paths.sessions_dir, pid))
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty());

    let exec = RealExec;
    let clock = RealClock;
    let spawner = RealDaemonSpawner;
    let connect = |url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
        WsAppServer::connect(url, std::time::Duration::from_secs(5)).map(|c| {
            let b: Box<dyn AppServerRpc> = Box::new(c);
            b
        })
    };
    let alloc = real_alloc_port;
    // W9 FIX Mo-2: the cmdline-identity guard — the AlreadyRunning gate reports
    // "running" ONLY when the live recorded pid's command line is OUR codex daemon
    // (never a false AlreadyRunning against a reused foreign pid). The probe reads
    // one pid's cmdline via the existing `ps` seam.
    let probe = real_cmdline_probe;

    // P0 wave-2: the ids store for the revived daemon's QD_SESSION_ID injection.
    let ids_path = match common::ids_store_path(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let deps = ReviveDeps {
        provider: &dispatch::provider::codex::CODEX_PROVIDER,
        env: &env,
        exec: &exec,
        clock: &clock,
        sessions_dir: paths.sessions_dir.clone(),
        log_dir,
        spawner: &spawner,
        connect: &connect,
        alloc_port: &alloc,
        cmdline_probe: &probe,
        ids_path,
    };
    let params = ResumeParams {
        name: name.clone(),
        thread_id: session.session_id.clone(),
        cwd: session.cwd.clone(),
        current_pid: session.pid,
        current_endpoint,
    };

    match resume_codex(&deps, &params) {
        Ok(ResumeOutcome::AlreadyRunning) => {
            // Drivable RIGHT NOW — a success no-op. NO attach (severed): tell the
            // agent to send to it. (Do NOT print "use qd attach".) W2: the pointer
            // is `qd send:relay` — bare `qd send` is a moved stub and `send:pty` has
            // no pane for a codex daemon; `send:relay` is the working agent channel.
            println!("{}", codex_already_running_line(&name));
            0
        }
        Ok(ResumeOutcome::Revived { pid, endpoint }) => {
            // Revived to drivable — NO attach. Report the new daemon + how to drive it.
            println!("{}", codex_revived_line(&name, pid, &endpoint));
            0
        }
        Err(e) => {
            eprintln!("qd resume: \"{name}\": {e}");
            e.exit_code()
        }
    }
}

/// scoped-ACP-CC Item 3 — the acp RESUME path at the verb layer. An acp/* row is a
/// daemon-hosted resident adapter (+ its `claude-code-acp` bridge); `qd resume` for it
/// is revive-to-DRIVABLE with NO interactive attach (agents have no TTY). Mirrors
/// [`run_codex_resume`] 1:1, substituting `session/load` (the ACP resume primitive,
/// driven by the load-mode adapter) for `thread/resume`:
///   - resumability gate (no sessionId / no jsonl → nothing to resume),
///   - ALIVE acp row (pid alive ∧ OUR cmdline carries the recorded `--listen`) →
///     AlreadyRunning no-op, ZERO mutation, NO second adapter (the (R-c) seam),
///   - else REVIVE: spawn a fresh resident adapter in LOAD mode (`--load-session <id>`,
///     detached `process_group(0)`, the SAME create spawn path), confirm it re-loaded
///     the SAME sessionId, then rewrite the row (NEW pid + NEW endpoint, SAME sessionId)
///     and consume the prior tombstone. A later `send`/`wait` round-trips on the SAME
///     sessionId; the CC JSONL continues (Component-0-proven faithful).
fn run_acp_resume(session: &dispatch::model::Session) -> i32 {
    use dispatch::acp_residence::{build_adapter_argv, connect_ready};
    use dispatch::create_daemon::{real_alloc_port, real_cmdline_probe, DaemonSpawner, RealDaemonSpawner};
    use dispatch::effects::Clock;
    use dispatch::resume_daemon::acp_resume_is_alive;
    use std::time::Duration;

    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd resume: HOME is not set — cannot resolve the session state dir.");
            return 1;
        }
    };
    let paths = SbPaths::from_home(&home);

    // Resumability gate (the acp analog of resume.rs's `no resumable transcript` arm):
    // a stopped acp row needs BOTH a sessionId (to `session/load`) and a jsonl_path
    // (the bridge's CC store the load reads). Either missing → nothing to resume.
    if session.session_id.is_empty() || session.jsonl_path.is_none() {
        eprintln!(
            "qd resume: session \"{name}\" was stopped and has no resumable transcript — \
             nothing to resume."
        );
        return 1;
    }

    // The CURRENT pid/endpoint (alive-check inputs). The endpoint is NOT on the Session
    // surface — re-read it off the registry row by pid (mirrors run_codex_resume).
    let current_endpoint = session
        .pid
        .filter(|&p| p != 0)
        .and_then(|pid| dispatch::registry::read_entry(&paths.sessions_dir, pid))
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty());

    // Case 1: ALREADY ALIVE → clean no-op, ZERO mutation, NO second adapter. pid-alive ∧
    // identity (the cmdline carries the recorded `--listen <endpoint>`), mirroring the
    // codex gate — NO reachability connect (it would misread a busy-but-alive adapter,
    // camped in another client's wait, as dead and double-spawn). The (R-c) seam.
    let probe = real_cmdline_probe;
    if acp_resume_is_alive(session.pid, current_endpoint.as_deref(), probe) {
        println!("{}", acp_already_running_line(&name));
        return 0;
    }

    // FINDING #3 — CONCURRENT-RESUME ATOMIC CLAIM (acp-only): take an exclusive,
    // self-healing flock on this sessionId BEFORE spawning, held across the whole
    // spawn→row-write critical section. Two concurrent `qd resume` of the SAME stopped
    // row → exactly ONE wins the claim and spawns; the LOSER refuses cleanly (no spawn,
    // no mutation). flock auto-releases on holder death → a crashed holder NEVER bricks a
    // later resume (self-healing; NOT a bare lock). NOTE: acp adds this concurrent-resume
    // atomic guard that codex lacks — codex daemon-resume parity is a named follow-on.
    let _resume_claim = match dispatch::resume_daemon::acquire_resume_claim(
        &paths.sessions_dir,
        &session.session_id,
    ) {
        Ok(Some(claim)) => claim, // WON — held until end of fn (drop releases the flock).
        Ok(None) => {
            eprintln!(
                "qd resume: \"{name}\": another resume of this session is already in \
                 progress — refusing (no double-spawn). Try again once it completes."
            );
            return 1;
        }
        Err(e) => {
            eprintln!("qd resume: \"{name}\": could not take the resume claim lock: {e}");
            return 1;
        }
    };

    // Case 2/3: REVIVE — re-spawn the resident adapter in LOAD mode. Mirrors the create
    // path `run_new_acp_daemon`, with `--load-session <sessionId>` substituted for the
    // fresh `session/new`. The `_resume_claim` flock above serializes concurrent revives.
    let port = match real_alloc_port() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("qd resume: \"{name}\": acp port allocation failed: {e}");
            return 1;
        }
    };
    let endpoint = format!("ws://127.0.0.1:{port}");
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("qd resume: \"{name}\": cannot resolve own executable for acp adapter: {e}");
            return 1;
        }
    };
    // The adapter's cwd = the row's cwd (faithful to the original session). A row with
    // no cwd falls back to "." (the adapter must have a cwd; the bridge resolves the
    // CC JSONL by encodeProjectPath(cwd), so this must match the create-time cwd).
    let cwd_str = session.cwd.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| ".".to_string());
    let cwd = PathBuf::from(&cwd_str);

    // LOAD MODE: `--load-session <sessionId>` → the adapter boots via real `session/load`.
    let argv = build_adapter_argv(&exe, &endpoint, &cwd, None, &[], Some(&session.session_id));
    let log_path = home
        .join(".quorum")
        .join("dispatch")
        .join("log")
        .join(format!("acp-{name}.log"));
    let spawner = RealDaemonSpawner;
    let spawned = match spawner.spawn_detached(&argv, &[], &cwd, &log_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("qd resume: \"{name}\": acp adapter spawn failed: {e}");
            return 1;
        }
    };

    // Readiness: poll connect+status until the resident session is re-established. On
    // failure: group-kill the just-spawned adapter (no orphan).
    let conn = match connect_ready(&endpoint, Duration::from_secs(30)) {
        Ok(c) => c,
        Err(e) => {
            spawner.kill(spawned.pid);
            eprintln!("qd resume: \"{name}\": {e} (see {})", log_path.display());
            return 1;
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
    if established != session.session_id {
        drop(conn);
        spawner.kill(spawned.pid);
        eprintln!(
            "qd resume: \"{name}\": the endpoint is serving a DIFFERENT acp session \
             ({established:?} != {:?}) — refusing (wrong/stale adapter, not our row).",
            session.session_id
        );
        return 1;
    }
    drop(conn); // the resident stays up; this was a short-lived readiness connection.

    // Rewrite the registry row: NEW adapter pid + NEW endpoint, SAME sessionId (m2
    // identity preserved), status live. The old dead-pid tombstone is consumed below.
    let clock = RealClock;
    let now = clock.now_ms();
    let entry = dispatch::registry::RegistryEntry {
        pid: Some(spawned.pid),
        session_id: Some(session.session_id.clone()),
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
        provider: Some(session.provider.clone()),
        endpoint: Some(endpoint.clone()),
        // A resumed healthy row carries NO degradation latch (tier is derived per verb).
        transport: None,
    };
    if let Err(e) = dispatch::registry::write_entry(&paths.sessions_dir, &entry) {
        spawner.kill(spawned.pid);
        eprintln!(
            "qd resume: \"{name}\": revived the acp adapter but its registry row could not \
             be written ({e}); the adapter was stopped."
        );
        return 1;
    }

    // Consume the prior tombstone (`<old_pid>.json.tombstoned`) so no dangling tombstone
    // / double live-row survives (R-b). Best-effort: a missing tombstone is fine (a row
    // stopped a different way), and the new live row is already authoritative. Also drop
    // any stale resume-verify marker keyed by the OLD pid (cleanup).
    if let Some(old_pid) = session.pid.filter(|&p| p != 0) {
        let tomb = paths.sessions_dir.join(format!("{old_pid}.json.tombstoned"));
        let _ = std::fs::remove_file(&tomb);
        let _ = std::fs::remove_file(dispatch::resume_daemon::resume_verify_marker_path(
            &paths.sessions_dir,
            old_pid,
        ));
    }

    // FINDING #2 PART 2 — drop a VERIFY-THE-BRIDGE marker: record the requested JSONL's
    // baseline (line count + the project dir's current session-file set) so the FIRST
    // post-resume wait can confirm the turn CONTINUED the SAME bridge JSONL (fork-on-load
    // detection) from PRIMARY source. Best-effort: a marker-write failure does not fail
    // the resume (the turn still works; we just lose the one-time verification).
    {
        use dispatch::resume_daemon::{
            resume_verify_marker_path, write_resume_verify_marker, ResumeVerifyMarker,
        };
        let requested = dispatch::jsonl::find_jsonl_path(
            &paths.projects_dir,
            &session.session_id,
            session.cwd.as_deref(),
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
        let baseline_files: Vec<String> = session
            .cwd
            .as_deref()
            .map(|cwd| paths.projects_dir.join(dispatch::jsonl::cwd_to_project_path(cwd)))
            .into_iter()
            .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                (n.ends_with(".jsonl") && !n.starts_with("agent-")).then_some(n)
            })
            .collect();
        let marker = ResumeVerifyMarker {
            session_id: session.session_id.clone(),
            cwd: session.cwd.clone(),
            baseline_lines,
            baseline_files,
        };
        let _ = write_resume_verify_marker(
            &resume_verify_marker_path(&paths.sessions_dir, spawned.pid),
            &marker,
        );
    }

    println!("{}", acp_revived_line(&name, spawned.pid, &endpoint));
    0
}

#[cfg(test)]
mod tests {
    use super::{
        acp_already_running_line, acp_revived_line, codex_already_running_line, codex_revived_line,
        resume_boot_unconfirmed_line, revive_launch_failed_line, revive_resume_args,
        revive_zmx_missing_line, run_detached_revive,
    };
    use dispatch::launch::launch_env_pairs;
    use dispatch::model::{Session, SessionBranch, SessionStatus};

    /// A cold claude registry row carrying a RECORDED `session_id` — the durable
    /// identity the daemon minted onto the child-pid-keyed row (WP-B5-ii-b PROOF 1).
    fn cold_claude_row(session_id: &str) -> Session {
        Session {
            name: Some("wk".to_string()),
            user_named: Some(true),
            session_id: session_id.to_string(),
            code: None,
            sb_id: None,
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
        let provider = dispatch::provider::provider_for("claude-code").unwrap();
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

    /// P0 wave-2 (spec-w2-env D1 site 2): the resume env-pair set ALWAYS
    /// carries QD_SESSION_ID (last, after the backend pairs) — with and without
    /// a backend capture — so the env file + dot-source prefix are
    /// unconditional on every resume/revive branch. (The pair-set builder is
    /// the hoisted `dispatch::launch::launch_env_pairs`; resume always passes `Some`.)
    /// R2 (override-never-inherit, the D1-site-4 pattern on the REVIVE path):
    /// an --alt-screen resume/connect-revive writes an env file carrying the
    /// EXPLICIT `unset -v CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN` (omitting the
    /// export alone would leave the child inheriting the var from an inline
    /// parent env), the unset PRECEDES the identity export, and the claude cmd
    /// dot-sources the file. An inline revive carries the export and NO unset.
    #[test]
    fn alt_screen_revive_env_file_carries_explicit_unset() {
        use dispatch::effects::MapEnv;
        use dispatch::launch::RenderMode;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let paths = dispatch::paths::SbPaths::from_home(&home);
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        let env = MapEnv { vars, uid: 501 };

        // AltScreen revive: unset-first file, identity export still rides.
        let cmd = super::prepare_claude_resume_env(
            "resume",
            &env,
            &home,
            &paths,
            "wk",
            "uuid-1",
            Some("wk"),
            vec![],
            RenderMode::AltScreen,
            "command 'claude'",
        )
        .expect("alt-screen revive prep");
        let body =
            std::fs::read_to_string(dispatch::launch::session_env_file_path(&home, "wk")).unwrap();
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
        super::prepare_claude_resume_env(
            "resume",
            &env,
            &home,
            &paths,
            "wk2",
            "uuid-2",
            Some("wk2"),
            vec![],
            RenderMode::Inline,
            "command 'claude'",
        )
        .expect("inline revive prep");
        let body2 =
            std::fs::read_to_string(dispatch::launch::session_env_file_path(&home, "wk2")).unwrap();
        assert!(
            body2.contains("export CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN='1'"),
            "{body2}"
        );
        assert!(
            !body2.contains("unset -v"),
            "inline needs no unset: {body2}"
        );
    }

    #[test]
    fn resume_env_pairs_always_carry_sb_session_id() {
        use dispatch::launch::RenderMode;
        // No backend env captured → the file STILL has the identity export
        // (punch item 7 adds the inline render birth property; item 1 adds the
        // unconditional FORCE birth property — both ahead of the id-last pair).
        let bare = launch_env_pairs(vec![], Some("ab3kx9mq".to_string()), RenderMode::Inline);
        assert_eq!(
            bare,
            vec![
                (
                    dispatch::launch::FORCE_SESSION_PERSISTENCE_KEY.to_string(),
                    "1".to_string()
                ),
                (
                    dispatch::launch::ALT_SCREEN_DISABLE_KEY.to_string(),
                    "1".to_string()
                ),
                ("QD_SESSION_ID".to_string(), "ab3kx9mq".to_string())
            ]
        );
        // The dot-source prefix is non-empty for this set (unconditional).
        let prefix = dispatch::launch::session_env_prefix(
            std::path::Path::new("/jail/home"),
            "wk",
            &bare,
            &[],
        );
        assert!(!prefix.is_empty(), "prefix must be unconditional");
        assert!(prefix.contains("/jail/home/.quorum/dispatch/session-env/wk.env"));

        // With backend pairs: backend FIRST, FORCE in the birth-property band,
        // the id LAST (an --alt-screen resume omits the render var; the id is
        // STILL last, FORCE STILL rides).
        let composed = launch_env_pairs(
            vec![("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string())],
            Some("ab3kx9mq".to_string()),
            RenderMode::AltScreen,
        );
        assert_eq!(composed.len(), 3);
        assert_eq!(composed[0].0, "ANTHROPIC_BASE_URL");
        assert_eq!(composed[1].0, "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE");
        assert_eq!(
            composed[2],
            ("QD_SESSION_ID".to_string(), "ab3kx9mq".to_string())
        );
    }

    /// W2 send-pointer: the codex-resume success lines MUST name `qd send:relay`
    /// (the working agent channel), NOT bare `qd send` (a moved stub) and NOT
    /// `send:pty` (no pane for a daemon-hosted session). `--wait` is NOT mentioned
    /// (codex ignores it). Pinned so a regression to `qd send` reds here.
    #[test]
    fn codex_resume_success_lines_point_at_send_relay() {
        let running = codex_already_running_line("wk");
        assert_eq!(
            running,
            "session \"wk\" is running; send to it with: qd send:relay wk <text>"
        );
        let revived = codex_revived_line("wk", 4242, "ws://127.0.0.1:18951");
        assert_eq!(
            revived,
            "resumed codex session \"wk\" (daemon pid 4242, ws://127.0.0.1:18951); \
             send to it with: qd send:relay wk <text>"
        );
        for line in [&running, &revived] {
            assert!(
                line.contains("qd send:relay wk"),
                "names send:relay: {line}"
            );
            // The bare `qd send <name>` stub must NOT be the pointer.
            assert!(
                !line.contains("send wk"),
                "must not point at bare `qd send`: {line}"
            );
            assert!(
                !line.contains("send:pty"),
                "no send:pty for a daemon: {line}"
            );
            assert!(!line.contains("--wait"), "codex ignores --wait: {line}");
        }
    }

    /// Item 3 (acp) resume success lines name `qd send:relay` (the working agent
    /// channel), NOT bare `qd send` / `send:pty` (no pane for a daemon-hosted session).
    /// Mirrors `codex_resume_success_lines_point_at_send_relay`.
    #[test]
    fn acp_resume_success_lines_point_at_send_relay() {
        let running = acp_already_running_line("wk");
        assert_eq!(
            running,
            "session \"wk\" is already alive; send to it with: qd send:relay wk <text>"
        );
        let revived = acp_revived_line("wk", 4242, "ws://127.0.0.1:18951");
        assert_eq!(
            revived,
            "resumed acp session \"wk\" (adapter pid 4242, ws://127.0.0.1:18951); \
             send to it with: qd send:relay wk <text>"
        );
        for line in [&running, &revived] {
            assert!(line.contains("qd send:relay wk"), "names send:relay: {line}");
            assert!(!line.contains("send:pty"), "no send:pty for a daemon: {line}");
            assert!(!line.contains("--wait"), "acp ignores --wait: {line}");
        }
    }

    /// codex P1 W4 (codex-p1-spec section 7.1): the resume argv fragment the verb
    /// now routes through `provider.resume_args(key, fork)` is BYTE-IDENTICAL to the
    /// verb's PRE-REWIRE hand-built `vec!["--resume", session.session_id]`. `qd
    /// resume` always passes `fork=false` (fork is a `qd new` concept, cli.rs:165-
    /// 169 — there is no resume fork flag), so the fragment is exactly `["--resume",
    /// <id>]`. We also pin the fork=true shape to prove the trait carries the
    /// correct `--fork-session` form even though this verb never requests it.
    ///
    /// MUTATION EVIDENCE: a provider drift (reordered/dropped `--resume`, a wrong
    /// fork shape, a mangled id) reds this — the routed fragment compares
    /// token-for-token against the frozen pre-rewire reference.
    #[test]
    fn resume_args_fragment_matches_prerewire() {
        use dispatch::provider::{provider_for, SessionKey};

        let id = "abc-123-resume".to_string();
        let key = SessionKey {
            id: &id,
            name: Some("wk"),
            cwd: Some("/work/proj"),
            pid: Some(4242),
        };
        let provider = provider_for("claude-code").expect("claude-code resolves");

        // The verb's path: fork=false → the EXACT pre-rewire hand-built fragment.
        let prerewire = vec!["--resume".to_string(), id.clone()];
        assert_eq!(
            provider.resume_args(&key, false),
            prerewire,
            "fork=false fragment must equal the pre-rewire ['--resume', id]"
        );

        // fork=true carries the claude `--fork-session` shape (not exercised by
        // this verb, but the trait must keep it for `qd new --fork` parity).
        assert_eq!(
            provider.resume_args(&key, true),
            vec![
                "--resume".to_string(),
                id.clone(),
                "--fork-session".to_string()
            ]
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
    impl dispatch::mux::Mux for FailingMux {
        fn run_detached(
            &self,
            _d: &std::path::Path,
            _n: &str,
            _c: &str,
            _w: &std::path::Path,
        ) -> std::io::Result<dispatch::exec::ExecResult> {
            if self.spawn_err {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "ENOENT"))
            } else {
                Ok(dispatch::exec::ExecResult {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "zmx: cannot create session: boom\n".to_string(),
                    timed_out: false,
                })
            }
        }
        fn list(&self, _d: &std::path::Path) -> std::io::Result<Vec<dispatch::mux::MuxSession>> {
            unreachable!("a failed launch must NEVER reach the boot waiter")
        }
        fn list_raw(
            &self,
            _d: &std::path::Path,
        ) -> std::io::Result<Vec<dispatch::mux::MuxSession>> {
            unreachable!("a failed launch must NEVER reach the boot waiter")
        }
        fn send(
            &self,
            _d: &std::path::Path,
            _n: &str,
            _t: &str,
        ) -> std::io::Result<dispatch::exec::ExecResult> {
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
        let paths = dispatch::paths::SbPaths::from_home(tmp.path());
        let mux = FailingMux { spawn_err: false };
        let code = run_detached_revive(
            &mux,
            tmp.path(),
            "wk",
            "command 'claude'",
            tmp.path(),
            &paths,
        )
        .unwrap_err();
        assert_eq!(code, 1);
        // (The FailingMux's unreachable!() boot-waiter verbs are the proof the
        // failure never degraded into a boot wait.)
    }

    #[test]
    fn revive_spawn_err_fails_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = dispatch::paths::SbPaths::from_home(tmp.path());
        let mux = FailingMux { spawn_err: true };
        let code = run_detached_revive(
            &mux,
            tmp.path(),
            "wk",
            "command 'claude'",
            tmp.path(),
            &paths,
        )
        .unwrap_err();
        assert_eq!(code, 1);
    }

    /// Wording pins: the nonzero-exit line carries zmx's (trimmed) stderr; the
    /// spawn-failure line is the missing-binary guidance.
    #[test]
    fn revive_failure_lines_are_pinned() {
        assert_eq!(
            revive_launch_failed_line("zmx: cannot create session: boom\n"),
            "Failed to resume session: zmx: cannot create session: boom"
        );
        assert_eq!(
            revive_zmx_missing_line(),
            "qd resume: could not launch zmx (is it installed and on PATH?)."
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
            resume_boot_unconfirmed_line("session \"wk\" did not reach idle status within timeout"),
            "qd resume: session launched but did not confirm ready: \
             session \"wk\" did not reach idle status within timeout"
        );
        // PID-file-phase detail (boot.rs run_pid_phase wording).
        assert_eq!(
            resume_boot_unconfirmed_line(
                "PID file for \"wk\" did not appear within 40000ms — qd connect wk to inspect"
            ),
            "qd resume: session launched but did not confirm ready: \
             PID file for \"wk\" did not appear within 40000ms — qd connect wk to inspect"
        );
    }
}
