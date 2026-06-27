//! `qd bootstrap` — ENGINE-ONLY (A5 spec §4.1/§4.2; named divergence §9 item 4).
//!
//! Ported from `0d0fa9e:src/commands/bootstrap.ts`, SHRUNK to the engine core:
//! the qb-owned deploy steps (artifact deploy, shell-profile patch, plugin
//! registration) are DROPPED (ruled — that content is the qb deploy's, not the
//! engine's; the dropped TS step names live in the A5 spec, never in this
//! repo). What survives is the state-dir creation (`~/.quorum/dispatch` + `~/.quorum/dispatch/state`) and
//! the zmx capability notice (`decideZmxAction`/`checkZmx`, the engine core the
//! TS test "decideZmxAction (pure decider — every branch)" pins). On top of
//! that, A5 adds the ADD-5 relay-driver detect→offer→self-install step (§4.2).
//!
//! Library-first (spec §2): the decision logic is PURE
//! ([`decide_zmx_action`] / [`decide_relay_offer`]); the runtime
//! ([`check_zmx`] / [`run_bootstrap`]) is a thin shell over injected effects so
//! tests never probe real zmx/brew, prompt a real TTY, run a real `brew
//! install`, or shell out a real relay installer.
//!
//! Output is `[bootstrap]`-prefixed, engine-truthful, and CONTENT-FREE: it
//! names none of the qb-side content concepts (carry 5; the forbidden-token
//! set lives in `scenarios/bootstrap_output_audit.sh`, which runtime-asserts
//! the shipped binary's output, gate row G-B5).

use std::path::{Path, PathBuf};

use crate::effects::Env;
use crate::exec::Exec;
use crate::model::RelayHealth;
use crate::preflight::{self, Capability};
use crate::relay;

// ----------------------------------------------------------------------------
// Paths (shrunk port of resolveBootstrapPaths, 0d0fa9e:src/commands/bootstrap.ts:87).
//
// The TS struct carried five qb-deploy-owned dirs + a provenance stamp — ALL
// DROPPED here (engine is content-free; the dropped field names live in the A5
// spec). The engine owns ONLY its state home: `~/.quorum/dispatch` and `~/.quorum/dispatch/state`. SB_HOME
// comes through the injected `Env` seam (L9a — nothing resolves the real home),
// never raw `std::env`.
// ----------------------------------------------------------------------------

/// The engine's state-dir layout (shrunk `BootstrapPaths`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapPaths {
    /// The qd data root, `SB_HOME` or `<home>/.quorum/dispatch` (bootstrap.ts:88-96).
    pub sb_home: PathBuf,
    /// Reserved hot-state dir, `<sbHome>/state` (bootstrap.ts:90 stateDir).
    pub state_dir: PathBuf,
}

/// Resolve the engine's bootstrap paths from the injected home + env
/// (`sbHome = SB_HOME || <home>/.quorum/dispatch`, bootstrap.ts:88-96). The home is injected
/// (never resolved from the real environment here, L9a); SB_HOME is read ONLY
/// through `env`.
pub fn resolve_bootstrap_paths(home: &Path, env: &dyn Env) -> BootstrapPaths {
    let sb_home = env
        .var("SB_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".quorum").join("dispatch"));
    let state_dir = sb_home.join("state");
    BootstrapPaths { sb_home, state_dir }
}

// ----------------------------------------------------------------------------
// zmx step (port of decideZmxAction/checkZmx, bootstrap.ts:786-925).
//
// zmx (the terminal multiplexer qd sessions run inside) is NOT in homebrew-core
// — it ships as the tap formula `neurosnap/tap/zmx` (macOS/brew only). bootstrap
// DETECTS it and, only in an interactive macOS+brew context, OFFERS to install
// it (default No). It NEVER installs silently and NEVER fails bootstrap on a
// missing zmx — this is a notice step. The non-TTY path only warns (the TS
// postinstall path: `bun install`'s postinstall ran bootstrap WITHOUT a TTY, so
// it had to only warn — never prompt or install; bootstrap.ts:781-783).
// ----------------------------------------------------------------------------

/// Homebrew tap formula that ships zmx (macOS/brew only; not homebrew-core).
/// (bootstrap.ts:756 `ZMX_BREW_FORMULA`.)
pub const ZMX_BREW_FORMULA: &str = "neurosnap/tap/zmx";

/// Guidance for installing zmx, by platform (port of `zmxGuidance`,
/// bootstrap.ts:759-764). Byte-matches the TS strings.
pub fn zmx_guidance(platform: &str) -> String {
    if platform == "darwin" || platform == "macos" {
        format!("Install zmx (needed for qd sessions):  brew install {ZMX_BREW_FORMULA}")
    } else {
        "Install zmx (needed for qd sessions): see https://github.com/neurosnap/zmx".to_string()
    }
}

/// The PURE zmx-step decision (port of `ZmxAction`, bootstrap.ts:766-769).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZmxAction {
    /// present + capable → nothing to do.
    Ok,
    /// missing + interactive + darwin + brew → offer the opt-in install.
    Prompt,
    /// every other state → warn with guidance.
    Warn { guidance: String },
}

/// Injected inputs for the pure zmx decider (port of `decideZmxAction`'s input
/// object, bootstrap.ts:786-792).
#[derive(Debug, Clone)]
pub struct ZmxDecisionInput {
    pub has_zmx: bool,
    /// `None` = capability not probed (zmx absent); `Some(false)` = present but
    /// stale (no `send`); `Some(true)` = present + capable.
    pub zmx_capable: Option<bool>,
    /// Host platform string (`std::env::consts::OS`-shaped: "macos"/"linux"/…;
    /// the TS used NodeJS.Platform "darwin"). Both spellings are accepted.
    pub platform: String,
    pub has_brew: bool,
    pub interactive: bool,
}

/// PURE decider for the zmx step (port of `decideZmxAction`, bootstrap.ts:786-803):
///   - present + capable (has `send`)              → Ok (nothing to do)
///   - present + STALE (no `send`, zmx < 0.6)      → Warn (upgrade guidance)
///   - missing + interactive + darwin + brew       → Prompt (offer install)
///   - everything else (incl. ANY non-TTY)         → Warn (platform guidance)
///
/// Every branch is unit-tested below (the TS test
/// "decideZmxAction (pure decider — every branch)").
pub fn decide_zmx_action(input: &ZmxDecisionInput) -> ZmxAction {
    if input.has_zmx {
        if input.zmx_capable == Some(false) {
            // present-but-stale → warn with the UPGRADE guidance (not the
            // platform install guidance), bootstrap.ts:794-796.
            return ZmxAction::Warn {
                guidance: preflight::zmx_upgrade_guidance(),
            };
        }
        return ZmxAction::Ok;
    }
    let is_darwin = input.platform == "darwin" || input.platform == "macos";
    if input.interactive && is_darwin && input.has_brew {
        return ZmxAction::Prompt;
    }
    ZmxAction::Warn {
        guidance: zmx_guidance(&input.platform),
    }
}

/// The zmx-step outcome (port of `ZmxResult.zmx`, bootstrap.ts:870-873).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZmxStatus {
    /// installed + capable.
    Present,
    /// we just installed it via brew.
    Installed,
    /// installed but too old to drive (no `send`).
    Stale,
    /// absent.
    Missing,
}

/// The zmx step's full result (port of `ZmxResult`, bootstrap.ts:867-875).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZmxResult {
    pub zmx: ZmxStatus,
    /// Lines to print as part of the bootstrap summary (may be empty).
    pub messages: Vec<String>,
}

/// Injected effects for the zmx step runtime (port of `ZmxDeps`,
/// bootstrap.ts:806-833). The decider is pure; this seam covers the live
/// probes / prompt / install so tests never touch real zmx/brew/TTY.
pub struct ZmxDeps<'a> {
    /// Is a command available on PATH? (real: `command -v <name>`.)
    pub command_exists: &'a dyn Fn(&str) -> bool,
    /// Does the installed zmx support the `send` subcommand? (real:
    /// [`preflight::zmx_send_capability`].) Only consulted when zmx is present.
    pub zmx_has_send: &'a dyn Fn() -> bool,
    pub platform: String,
    pub interactive: bool,
    /// Ask a yes/no question; default No (real: visible `[y/N]` prompt).
    pub prompt_yes_no: &'a dyn Fn(&str) -> bool,
    /// Install zmx via brew; returns success (real: `brew install <formula>`,
    /// inherit). Only called on an explicit interactive yes in the macOS+brew
    /// path.
    pub install_zmx: &'a dyn Fn() -> bool,
}

/// The zmx step runtime (port of `checkZmx`, bootstrap.ts:883-925). Detects zmx;
/// in an interactive macOS+brew context OFFERS to install (default No);
/// otherwise warns with guidance. NEVER installs without an explicit yes, NEVER
/// fails bootstrap. `install_zmx` is only called on an explicit interactive yes
/// in the macOS+brew path.
pub fn check_zmx(deps: &ZmxDeps) -> ZmxResult {
    let has_zmx = (deps.command_exists)("zmx");
    // Only probe capability when zmx is actually present (bootstrap.ts:890).
    let zmx_capable = if has_zmx {
        Some((deps.zmx_has_send)())
    } else {
        None
    };
    let decision = decide_zmx_action(&ZmxDecisionInput {
        has_zmx,
        zmx_capable,
        platform: deps.platform.clone(),
        has_brew: (deps.command_exists)("brew"),
        interactive: deps.interactive,
    });
    let guidance = zmx_guidance(&deps.platform);

    match decision {
        ZmxAction::Ok => ZmxResult {
            zmx: ZmxStatus::Present,
            messages: vec![],
        },
        ZmxAction::Warn { guidance: g } => {
            // present-but-stale vs genuinely-missing — same warn shape, different
            // state (bootstrap.ts:903-904).
            let status = if has_zmx {
                ZmxStatus::Stale
            } else {
                ZmxStatus::Missing
            };
            ZmxResult {
                zmx: status,
                messages: vec![g],
            }
        }
        ZmxAction::Prompt => {
            // interactive macOS + brew — offer the opt-in install (default N),
            // bootstrap.ts:907-921.
            let question = format!(
                "zmx not found (needed for qd sessions). Install via \
                 `brew install {ZMX_BREW_FORMULA}` now? [y/N] "
            );
            if !(deps.prompt_yes_no)(&question) {
                return ZmxResult {
                    zmx: ZmxStatus::Missing,
                    messages: vec![guidance],
                };
            }
            if (deps.install_zmx)() {
                ZmxResult {
                    zmx: ZmxStatus::Installed,
                    messages: vec!["zmx installed.".to_string()],
                }
            } else {
                ZmxResult {
                    zmx: ZmxStatus::Missing,
                    messages: vec![format!("zmx install failed. {guidance}")],
                }
            }
        }
    }
}

/// Render the zmx summary status word (port of `zmxStatus`, bootstrap.ts:980-987).
fn zmx_status_word(status: ZmxStatus) -> &'static str {
    match status {
        ZmxStatus::Present => "ok (present)",
        ZmxStatus::Installed => "installed via brew",
        ZmxStatus::Stale => "TOO OLD (no `send` — upgrade required)",
        ZmxStatus::Missing => "missing",
    }
}

// ----------------------------------------------------------------------------
// Relay step (2026-06-10 ruling; SUPERSEDES the ~/.claude/.mcp.json target of
// ADR 0016 — see doc/adr/0017-relay-via-claude-mcp.md).
//
// The relay transport is NATIVE (`qd relay:serve` IS the MCP server), but
// Claude Code loads MCP servers from ITS OWN user-scope config, whose location
// has moved across versions — `~/.claude/.mcp.json` (what ADR 0016 wrote) is
// NOT read by Claude Code 2.1.x. Rather than track Claude Code's storage, we
// REGISTER through `claude mcp add -s user` (and detect via `claude mcp get`,
// roll back via `claude mcp remove`): Claude Code owns its own config; we own
// only the intent. The shell-out lives in [`crate::relay_server::register`];
// this step is the pure consent/report decider over injected effects.
//
// Precondition: `claude` must be on PATH (we drive it). Absent → a notice +
// manual pointer, never a failure. Consent discipline unchanged: offer ONLY on
// a TTY, default No, NEVER fail bootstrap. Runtime health (sidecar discovery)
// is still reported as an FYI line — it is orthogonal to whether NEW sessions
// will load the relay (that's the registration).
// ----------------------------------------------------------------------------

/// The relay runtime-health finding (sidecar/port discovery — an FYI, NOT the
/// signal the offer keys off; registration is).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayFinding {
    /// a relay sidecar exists and reports healthy.
    PresentHealthy,
    /// a relay sidecar exists but does NOT report healthy.
    PresentUnhealthy,
    /// no relay present.
    Absent,
}

/// Classify a relay-health finding from the discovered relay-health records
/// (PURE). The records come from [`relay::get_relay_ports`] (sidecars, else the
/// HTTP port-scan probe). A record whose `status` is `"ok"` is healthy; any
/// other status is unhealthy; no records at all = absent.
pub fn classify_relay_finding(relays: &[RelayHealth]) -> RelayFinding {
    if relays.is_empty() {
        return RelayFinding::Absent;
    }
    if relays.iter().any(|r| r.status == "ok") {
        RelayFinding::PresentHealthy
    } else {
        RelayFinding::PresentUnhealthy
    }
}

/// The relay-step outcome (for the bin layer to report + for tests to assert).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayStepOutcome {
    /// `claude` is not on PATH — cannot register; manual pointer printed.
    ClaudeMissing,
    /// already registered with Claude Code + non-TTY — left as-is (the re-point
    /// is offered only on a TTY; a stale path is reported as a how-to FYI).
    ConfiguredAlready,
    /// already registered + TTY, user DECLINED the re-point — left as-is.
    RepointDeclined,
    /// already registered + TTY, user accepted the re-point — the command path
    /// was re-pointed at the running binary (the recurrence fix: a moved `qd`
    /// orphans the relay path until something re-points it; bootstrap does).
    Repointed { command: String },
    /// already registered + TTY, user accepted but the re-point FAILED (the old
    /// registration is left untouched; bootstrap still exits 0).
    RepointFailed { error: String },
    /// not registered + non-TTY — never offered; how-to line printed.
    NotOffered,
    /// not registered + TTY, user DECLINED — how-to line printed.
    Declined,
    /// user accepted; registration succeeded (command = registered binary).
    Registered { command: String },
    /// user accepted; registration FAILED (bootstrap still exits 0 — the relay
    /// step never fails bootstrap).
    RegisterFailed { error: String },
}

/// Injected effects for the relay step runtime. Kept separate from [`ZmxDeps`]
/// so the two steps' seams don't entangle. The `claude mcp` shell-out lives in
/// [`crate::relay_server::register`]; the bin layer wires it into these.
pub struct RelayDeps<'a> {
    pub interactive: bool,
    /// Is `claude` on PATH? We drive `claude mcp` to register/detect, so this is
    /// a hard precondition for the registration path.
    pub claude_present: bool,
    /// Is the relay already registered with Claude Code (user scope)?
    /// `Some(true)`/`Some(false)` = a definite answer from `claude mcp get`;
    /// `None` = undetermined (claude missing / probe error) → treated as not
    /// registered for the offer.
    pub relay_registered: Option<bool>,
    /// Ask a yes/no question; default No (real: visible `[y/N]` prompt).
    pub prompt_yes_no: &'a dyn Fn(&str) -> bool,
    /// Register/re-point the relay at user scope as the BARE `qd` command
    /// (real: `register::register_relay`, which remove-then-adds → idempotently
    /// re-points the command). The bare command is resolved via PATH and never
    /// goes stale on a binary move (relay-path hardening v2). Returns the
    /// now-registered command (the bare `qd`). Used BOTH for the first-time
    /// registration (not-registered path) and the re-point (already-registered
    /// path) — the action is the same idempotent re-point; only the consent
    /// prompt differs. Only called on an explicit interactive yes.
    pub register: &'a dyn Fn() -> Result<String, String>,
}

/// Append the runtime-health FYI line for a discovered relay server (nothing
/// when none is up — silence is the normal idle-fleet state).
fn push_relay_health_fyi(relays: &[RelayHealth], lines: &mut Vec<String>) {
    match classify_relay_finding(relays) {
        RelayFinding::PresentHealthy => {
            lines.push("relay: a relay server is up (healthy).".to_string())
        }
        RelayFinding::PresentUnhealthy => {
            lines.push("relay: a relay server is up but NOT healthy.".to_string())
        }
        RelayFinding::Absent => {}
    }
}

/// The relay step runtime: precondition-check `claude`, report registration
/// state (+ the runtime-health FYI), decide the offer, and (on an explicit yes)
/// run the injected registration. Returns the outcome + the report lines.
/// NEVER hangs on a declined / non-TTY path; NEVER fails bootstrap.
pub fn check_relay(relays: &[RelayHealth], deps: &RelayDeps) -> (RelayStepOutcome, Vec<String>) {
    // Precondition: we register by driving `claude mcp`. No claude → notice.
    if !deps.claude_present {
        let mut lines = vec![
            "relay: cannot configure — `claude` is not on PATH. Install Claude Code, \
             then run: qd relay:register"
                .to_string(),
        ];
        push_relay_health_fyi(relays, &mut lines);
        return (RelayStepOutcome::ClaudeMissing, lines);
    }

    let registered = deps.relay_registered.unwrap_or(false);
    let mut lines = vec![if registered {
        "relay: configured (registered with Claude Code, user scope).".to_string()
    } else {
        "relay: not configured (no relay MCP server registered with Claude Code).".to_string()
    }];
    push_relay_health_fyi(relays, &mut lines);

    if registered {
        // RECURRENCE FIX (relay-path hardening v2): an absolute-path relay command
        // went stale whenever the `qd` binary moved, so sessions born after the
        // move couldn't spawn their relay sidecar. We now register the BARE `qd`
        // command (resolved via PATH), which never goes stale on a move. This
        // offered re-point exists to migrate a LEGACY absolute-path entry to the
        // bare form (`register` remove-then-adds the bare command, an idempotent
        // re-point). Consent-gated + non-interactive-safe: offered ONLY on a TTY
        // (default No); a non-TTY run leaves the existing registration untouched
        // and prints a how-to FYI, never hanging.
        if !deps.interactive {
            lines.push(
                "relay: re-point to this binary later (after moving `qd`) with: qd relay:repoint"
                    .to_string(),
            );
            return (RelayStepOutcome::ConfiguredAlready, lines);
        }
        let question = "Re-point the relay registration at THIS qd binary (idempotent; \
             fixes a stale path after the `qd` binary moves)? [y/N] ";
        if !(deps.prompt_yes_no)(question) {
            lines.push("relay: left as-is — re-point later with: qd relay:repoint".to_string());
            return (RelayStepOutcome::RepointDeclined, lines);
        }
        return match (deps.register)() {
            Ok(command) => {
                lines.push(format!(
                    "relay: re-pointed — new sessions will load `{command} relay:serve`."
                ));
                (RelayStepOutcome::Repointed { command }, lines)
            }
            Err(error) => {
                lines.push(format!(
                    "relay: re-point FAILED ({error}) — old registration kept; \
                     retry with: qd relay:repoint"
                ));
                (RelayStepOutcome::RepointFailed { error }, lines)
            }
        };
    }

    // Not registered. Offer only on a TTY.
    if !deps.interactive {
        lines.push("relay: register later with: qd relay:register".to_string());
        return (RelayStepOutcome::NotOffered, lines);
    }

    let question = "Register qd's relay MCP server with Claude Code (runs \
         `claude mcp add`; enables cross-session messaging in NEW sessions)? [y/N] ";
    if !(deps.prompt_yes_no)(question) {
        lines.push("relay: skipped — register later with: qd relay:register".to_string());
        return (RelayStepOutcome::Declined, lines);
    }

    match (deps.register)() {
        Ok(command) => {
            lines.push(format!(
                "relay: registered — new sessions will load `{command} relay:serve`."
            ));
            (RelayStepOutcome::Registered { command }, lines)
        }
        Err(error) => {
            lines.push(format!(
                "relay: registration FAILED ({error}) — register later with: qd relay:register"
            ));
            (RelayStepOutcome::RegisterFailed { error }, lines)
        }
    }
}

// ----------------------------------------------------------------------------
// Shell-integration step (2026-06-09 ruling; supersedes the A5 §9-item-4
// "profile patching is not engine bootstrap's job" divergence — see
// doc/adr/0016-native-relay-and-eval-init.md).
//
// Bootstrap OFFERS (TTY only, default No) to add the ONE-LINE eval-init hook
// to the user's shell rc file (`eval "$(qd init bash)"` / zsh / fish conf.d).
// The wrapper BODY is never written into the rc file — it ships in the binary
// via `qd init` (crate::shell_init), so it cannot fossilize the way the
// TS-era baked block did. The retired baked block (the `>>> qd bootstrap >>>`
// markers) is DETECTED + REPORTED — a live function defined after the eval
// line would shadow the shipped wrapper — but never edited: bootstrap adds
// one line with consent; it does not rewrite user rc files.
// ----------------------------------------------------------------------------

/// The wrapper-step outcome (for the bin layer to report + for tests to assert).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapperStepOutcome {
    /// The rc file already carries the init line. Nothing to do.
    AlreadyConfigured,
    /// `$SHELL` did not classify to a supported shell — manual pointer printed.
    UnknownShell,
    /// Not configured + non-TTY — never offered; how-to line printed.
    NotOffered,
    /// Not configured + TTY, user DECLINED — how-to line printed.
    Declined,
    /// User accepted; the init line was added to the rc file.
    Added,
    /// User accepted; the rc write FAILED (bootstrap still exits 0).
    AddFailed { error: String },
}

/// PURE wrapper-offer decision: offer ONLY when the shell is recognised, the
/// rc does not already carry the init line, and stdin is a TTY.
pub fn decide_wrapper_offer(
    shell: Option<crate::shell_init::Shell>,
    rc_contents: Option<&str>,
    interactive: bool,
) -> bool {
    match shell {
        None => false,
        Some(s) => {
            let configured = rc_contents
                .map(|c| crate::shell_init::rc_has_init_line(c, s))
                .unwrap_or(false);
            !configured && interactive
        }
    }
}

/// Injected effects for the wrapper step runtime. The rc contents are pre-read
/// by the caller (`None` = file absent/unreadable) so the decider stays pure
/// and tests stay hermetic.
pub struct WrapperDeps<'a> {
    /// The user's shell, classified from `$SHELL` (`None` = unrecognised).
    pub shell: Option<crate::shell_init::Shell>,
    /// Display form of the rc path for report lines (e.g. `~/.bashrc`).
    pub rc_display: String,
    /// Pre-read rc-file contents (`None` = absent/unreadable).
    pub rc_contents: Option<String>,
    pub interactive: bool,
    /// Ask a yes/no question; default No (real: visible `[y/N]` prompt).
    pub prompt_yes_no: &'a dyn Fn(&str) -> bool,
    /// Append the init line to the rc file (creating it + parents if needed).
    /// Only called on an explicit interactive yes.
    pub add_init_line: &'a dyn Fn() -> Result<(), String>,
}

/// The wrapper step runtime: report the state, surface the retired baked block
/// if present, and (TTY + explicit yes) add the one-line eval-init hook.
/// NEVER hangs on a declined / non-TTY path; NEVER fails bootstrap.
pub fn check_wrapper(deps: &WrapperDeps) -> (WrapperStepOutcome, Vec<String>) {
    use crate::shell_init::{init_line, rc_has_init_line, rc_has_legacy_block};

    let mut lines = Vec::new();

    // Surface the retired TS-era baked wrapper block wherever we can see it —
    // it shadows the shipped wrapper if it is defined after the eval line.
    if deps
        .rc_contents
        .as_deref()
        .map(rc_has_legacy_block)
        .unwrap_or(false)
    {
        lines.push(format!(
            "shell: RETIRED baked wrapper block detected in {} (the `>>> qd bootstrap >>>` \
             markers) — remove it; the init line replaces it.",
            deps.rc_display
        ));
    }

    let shell = match deps.shell {
        Some(s) => s,
        None => {
            lines.push(
                "shell: unrecognised $SHELL — add the integration manually: \
                 eval \"$(qd init bash|zsh)\" or `qd init fish | source`."
                    .to_string(),
            );
            return (WrapperStepOutcome::UnknownShell, lines);
        }
    };
    let line = init_line(shell);

    if deps
        .rc_contents
        .as_deref()
        .map(|c| rc_has_init_line(c, shell))
        .unwrap_or(false)
    {
        lines.push(format!(
            "shell: integration configured ({} in {}).",
            line, deps.rc_display
        ));
        return (WrapperStepOutcome::AlreadyConfigured, lines);
    }

    if !decide_wrapper_offer(deps.shell, deps.rc_contents.as_deref(), deps.interactive) {
        lines.push(format!(
            "shell: integration not configured — add to {}:  {}",
            deps.rc_display, line
        ));
        return (WrapperStepOutcome::NotOffered, lines);
    }

    let question = format!(
        "Add the claude shell wrapper to {} (routes a bare `claude` into a tracked \
         qd session; adds one line: {})? [y/N] ",
        deps.rc_display, line
    );
    if !(deps.prompt_yes_no)(&question) {
        lines.push(format!(
            "shell: skipped — add to {} later:  {}",
            deps.rc_display, line
        ));
        return (WrapperStepOutcome::Declined, lines);
    }

    match (deps.add_init_line)() {
        Ok(()) => {
            lines.push(format!(
                "shell: integration added to {} — takes effect in new shells.",
                deps.rc_display
            ));
            (WrapperStepOutcome::Added, lines)
        }
        Err(error) => {
            lines.push(format!(
                "shell: write FAILED ({error}) — add to {} manually:  {}",
                deps.rc_display, line
            ));
            (WrapperStepOutcome::AddFailed { error }, lines)
        }
    }
}

// ----------------------------------------------------------------------------
// Extension-install step (plan 0001 child D-install; ADR 0018).
//
// After the engine's own state/relay/shell steps, bootstrap OFFERS (consent-
// gated, default No, TTY only) to install the PINNED extensions this `qd`
// blesses: (a) the `qb` engine-extension binary, (b) the work-model plugin.
//
// CONTENT-FREE ENGINE (scope-audit, success criterion #7): the engine owns ONLY
// the consent + the invocation. The ACTUAL install actions — which must name the
// deploy concepts the engine is forbidden to — live in the EXTERNAL
// `scripts/install-extensions.sh`, invoked by path through the injected
// `install_*` closures. This step never spells those concepts.
//
// Idempotent (the external script re-runs as a refresh), partial-safe (the two
// offers are independent), non-interactive-safe (a non-TTY run never prompts —
// it prints a how-to FYI and installs nothing).
// ----------------------------------------------------------------------------

/// The per-extension install outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtInstallOutcome {
    /// Non-TTY — never offered; how-to FYI printed.
    NotOffered,
    /// TTY, user DECLINED — nothing installed.
    Declined,
    /// User accepted; the external installer succeeded.
    Installed,
    /// User accepted; the external installer FAILED (bootstrap still exits 0).
    Failed { error: String },
}

/// The extension-cascade outcome (one per offered extension).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionsStepOutcome {
    /// The pinned `qb` binary install.
    pub qb: ExtInstallOutcome,
    /// The pinned work-model plugin install.
    pub plugin: ExtInstallOutcome,
}

/// Injected effects for the extension-install step. The `install_*` closures are
/// the ONLY contact with the external installer; the engine never names the
/// deploy concepts (the closures shell to `scripts/install-extensions.sh`). The
/// pinned refs to report come pre-rendered as opaque display strings so this
/// step stays content-free.
pub struct ExtensionsDeps<'a> {
    pub interactive: bool,
    /// A short opaque label for the pinned `qb` ref (e.g. a short sha), for the
    /// report line. Engine-truthful, content-free.
    pub sbx_pin_label: String,
    /// A short opaque label for the pinned plugin ref, for the report line.
    pub plugin_pin_label: String,
    /// Ask a yes/no question; default No.
    pub prompt_yes_no: &'a dyn Fn(&str) -> bool,
    /// Run the external installer for the `qb` extension. Returns Ok on success,
    /// Err(message) on failure (toolchain/auth/build). Only called on a yes.
    pub install_sbx: &'a dyn Fn() -> Result<(), String>,
    /// Run the external installer for the work-model plugin. Same contract.
    pub install_plugin: &'a dyn Fn() -> Result<(), String>,
}

/// One offer→install sub-step (shared by both extensions). Non-TTY → NotOffered;
/// declined → Declined; accepted → run the injected installer and map the
/// result. NEVER hangs on a non-TTY/declined path; NEVER fails bootstrap.
fn offer_install(
    interactive: bool,
    question: &str,
    install: &dyn Fn() -> Result<(), String>,
    prompt_yes_no: &dyn Fn(&str) -> bool,
    later_hint: &str,
    label_ok: &str,
    label_fail: &str,
    lines: &mut Vec<String>,
) -> ExtInstallOutcome {
    if !interactive {
        lines.push(later_hint.to_string());
        return ExtInstallOutcome::NotOffered;
    }
    if !prompt_yes_no(question) {
        lines.push(format!("{later_hint} (skipped)"));
        return ExtInstallOutcome::Declined;
    }
    match install() {
        Ok(()) => {
            lines.push(label_ok.to_string());
            ExtInstallOutcome::Installed
        }
        Err(error) => {
            lines.push(format!("{label_fail} ({error})"));
            ExtInstallOutcome::Failed { error }
        }
    }
}

/// The extension-install step: offer (TTY only) to install the pinned `qb`
/// binary, then the pinned work-model plugin. Returns the outcomes + report
/// lines. Partial-safe (the two offers are independent); non-interactive-safe;
/// never fails bootstrap.
pub fn check_extensions(deps: &ExtensionsDeps) -> (ExtensionsStepOutcome, Vec<String>) {
    let mut lines = Vec::new();

    let qb = offer_install(
        deps.interactive,
        &format!(
            "Install the pinned extension binary ({}) via the install script? [y/N] ",
            deps.sbx_pin_label
        ),
        deps.install_sbx,
        deps.prompt_yes_no,
        "extensions: binary — install later with: qd bootstrap (on a TTY)",
        &format!(
            "extensions: binary — installed (pinned {}).",
            deps.sbx_pin_label
        ),
        "extensions: binary — install FAILED",
        &mut lines,
    );

    let plugin = offer_install(
        deps.interactive,
        &format!(
            "Install the pinned work-model plugin ({}) via the install script? [y/N] ",
            deps.plugin_pin_label
        ),
        deps.install_plugin,
        deps.prompt_yes_no,
        "extensions: plugin — install later with: qd bootstrap (on a TTY)",
        &format!(
            "extensions: plugin — installed (pinned {}).",
            deps.plugin_pin_label
        ),
        "extensions: plugin — install FAILED",
        &mut lines,
    );

    (ExtensionsStepOutcome { qb, plugin }, lines)
}

// ----------------------------------------------------------------------------
// run_bootstrap — the engine bootstrap (shrunk port of runBootstrap,
// bootstrap.ts:937-959 + the bin reporter, :961-1013).
// ----------------------------------------------------------------------------

/// The full engine-bootstrap result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapResult {
    pub paths: BootstrapPaths,
    /// True if `sb_home`/`state_dir` already existed (idempotent re-run).
    pub already_existed: bool,
    pub zmx: ZmxResult,
    pub relay: RelayStepOutcome,
    pub wrapper: WrapperStepOutcome,
    pub extensions: ExtensionsStepOutcome,
    /// The `[bootstrap]`-prefixed report lines (engine-truthful, content-free).
    pub report: Vec<String>,
}

/// Filesystem seam for state-dir creation, injected so tests stay hermetic.
pub struct BootstrapFs<'a> {
    /// Does this path already exist?
    pub exists: &'a dyn Fn(&Path) -> bool,
    /// `mkdir -p` the path; returns Ok or an error string.
    pub mkdir_p: &'a dyn Fn(&Path) -> Result<(), String>,
}

/// Run the engine bootstrap: ensure the state dirs exist (idempotent), run the
/// zmx notice step, the native relay-registration step, and the shell-
/// integration step, and build the `[bootstrap]` report. Returns the result;
/// the caller prints `report` and maps to an exit code (0 unless a state-dir
/// mkdir failed — the only hard failure; the zmx/relay/shell steps are
/// consent-gated notices and NEVER fail bootstrap).
pub fn run_bootstrap(
    paths: BootstrapPaths,
    fs: &BootstrapFs,
    zmx_deps: &ZmxDeps,
    relays: &[RelayHealth],
    relay_deps: &RelayDeps,
    wrapper_deps: &WrapperDeps,
    extensions_deps: &ExtensionsDeps,
) -> Result<BootstrapResult, String> {
    let already_existed = (fs.exists)(&paths.sb_home) && (fs.exists)(&paths.state_dir);

    // Ensure the state dirs (idempotent: mkdir -p on an existing dir is a no-op).
    (fs.mkdir_p)(&paths.sb_home)?;
    (fs.mkdir_p)(&paths.state_dir)?;

    let zmx = check_zmx(zmx_deps);
    let (relay, relay_lines) = check_relay(relays, relay_deps);
    let (wrapper, wrapper_lines) = check_wrapper(wrapper_deps);
    let (extensions, extension_lines) = check_extensions(extensions_deps);

    // Build the engine-truthful, content-free report.
    let mut report = Vec::new();
    report.push(format!(
        "[bootstrap] state dir: {}",
        paths.sb_home.display()
    ));
    report.push(format!(
        "[bootstrap]   state: {} ({})",
        paths.state_dir.display(),
        if already_existed {
            "already present"
        } else {
            "created"
        }
    ));
    report.push(format!("[bootstrap] zmx: {}", zmx_status_word(zmx.zmx)));
    for line in &zmx.messages {
        report.push(format!("[bootstrap]   {line}"));
    }
    for line in &relay_lines {
        report.push(format!("[bootstrap] {line}"));
    }
    for line in &wrapper_lines {
        report.push(format!("[bootstrap] {line}"));
    }
    for line in &extension_lines {
        report.push(format!("[bootstrap] {line}"));
    }

    Ok(BootstrapResult {
        paths,
        already_existed,
        zmx,
        relay,
        wrapper,
        extensions,
        report,
    })
}

// ----------------------------------------------------------------------------
// Real-deps helpers for the bin layer.
// ----------------------------------------------------------------------------

/// Real `command -v <name>` via the injected exec (TS `commandExists`,
/// bootstrap.ts:809-818). Available = the probe exits 0.
pub fn real_command_exists(exec: &impl Exec, name: &str) -> bool {
    // `command -v` is a shell builtin; run it through `sh -c`.
    let arg = format!("command -v '{}'", name.replace('\'', "'\\''"));
    match exec.run("sh", &["-c".to_string(), arg], &[], None, Some(5000)) {
        Ok(r) => r.status == Some(0),
        Err(_) => false,
    }
}

/// Real zmx `send` capability probe via the injected exec
/// ([`preflight::zmx_send_capability`]).
pub fn real_zmx_has_send(exec: &impl Exec) -> bool {
    preflight::zmx_send_capability(exec) != Capability::No
}

/// Discover relay-health records the same way A4's join does: sidecars first,
/// else the HTTP port-scan probe ([`relay::get_relay_ports`]).
pub fn discover_relays(
    relay_dir: &Path,
    probe: &dyn crate::effects::RelayProbe,
) -> Vec<RelayHealth> {
    relay::get_relay_ports(relay_dir, probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use std::cell::Cell;
    use std::collections::HashMap;

    fn map_env(pairs: &[(&str, &str)]) -> MapEnv {
        let mut vars = HashMap::new();
        for (k, v) in pairs {
            vars.insert(k.to_string(), v.to_string());
        }
        MapEnv { vars, uid: 501 }
    }

    // --- resolve_bootstrap_paths ------------------------------------------

    #[test]
    fn paths_default_to_home_dot_sb() {
        let env = map_env(&[]);
        let p = resolve_bootstrap_paths(Path::new("/jail/home"), &env);
        assert_eq!(p.sb_home, PathBuf::from("/jail/home/.quorum/dispatch"));
        assert_eq!(
            p.state_dir,
            PathBuf::from("/jail/home/.quorum/dispatch/state")
        );
    }

    #[test]
    fn paths_honor_sb_home_override() {
        let env = map_env(&[("SB_HOME", "/jail/sbhome")]);
        let p = resolve_bootstrap_paths(Path::new("/jail/home"), &env);
        assert_eq!(p.sb_home, PathBuf::from("/jail/sbhome"));
        assert_eq!(p.state_dir, PathBuf::from("/jail/sbhome/state"));
    }

    #[test]
    fn paths_ignore_empty_sb_home() {
        let env = map_env(&[("SB_HOME", "")]);
        let p = resolve_bootstrap_paths(Path::new("/jail/home"), &env);
        assert_eq!(p.sb_home, PathBuf::from("/jail/home/.quorum/dispatch"));
    }

    // --- decide_zmx_action: EVERY branch (TS "every branch" test) ---------

    fn zin(
        has_zmx: bool,
        zmx_capable: Option<bool>,
        platform: &str,
        has_brew: bool,
        interactive: bool,
    ) -> ZmxDecisionInput {
        ZmxDecisionInput {
            has_zmx,
            zmx_capable,
            platform: platform.to_string(),
            has_brew,
            interactive,
        }
    }

    #[test]
    fn zmx_present_capable_is_ok() {
        // present + capable → Ok (both platform spellings, both interactivities).
        assert_eq!(
            decide_zmx_action(&zin(true, Some(true), "darwin", true, true)),
            ZmxAction::Ok
        );
        assert_eq!(
            decide_zmx_action(&zin(true, Some(true), "linux", false, false)),
            ZmxAction::Ok
        );
    }

    #[test]
    fn zmx_present_capability_unprobed_is_ok() {
        // present + capability None (assumed true; TS `?? true`) → Ok.
        assert_eq!(
            decide_zmx_action(&zin(true, None, "darwin", true, true)),
            ZmxAction::Ok
        );
    }

    #[test]
    fn zmx_present_stale_warns_with_upgrade_guidance() {
        // present + STALE → Warn with the UPGRADE guidance (not platform).
        let a = decide_zmx_action(&zin(true, Some(false), "darwin", true, true));
        match a {
            ZmxAction::Warn { guidance } => {
                assert_eq!(guidance, preflight::zmx_upgrade_guidance());
                assert!(guidance.contains("too old"));
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn zmx_missing_interactive_darwin_brew_prompts() {
        assert_eq!(
            decide_zmx_action(&zin(false, None, "darwin", true, true)),
            ZmxAction::Prompt
        );
        // "macos" spelling (std::env::consts::OS) also prompts.
        assert_eq!(
            decide_zmx_action(&zin(false, None, "macos", true, true)),
            ZmxAction::Prompt
        );
    }

    #[test]
    fn zmx_missing_non_tty_never_prompts_darwin() {
        // Non-TTY on darwin+brew → Warn (the postinstall/CI path), never Prompt.
        let a = decide_zmx_action(&zin(false, None, "darwin", true, false));
        match a {
            ZmxAction::Warn { guidance } => assert!(guidance.contains("brew install")),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn zmx_missing_no_brew_warns() {
        // interactive darwin but NO brew → Warn (can't offer the install).
        let a = decide_zmx_action(&zin(false, None, "darwin", false, true));
        assert!(matches!(a, ZmxAction::Warn { .. }));
    }

    #[test]
    fn zmx_missing_linux_warns_with_platform_guidance() {
        // interactive linux (no brew offer on non-darwin) → Warn platform guidance.
        let a = decide_zmx_action(&zin(false, None, "linux", false, true));
        match a {
            ZmxAction::Warn { guidance } => assert!(guidance.contains("github.com/neurosnap/zmx")),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    // --- check_zmx runtime (injected effects) -----------------------------

    struct ZmxFx {
        zmx_present: bool,
        brew_present: bool,
        has_send: bool,
        platform: String,
        interactive: bool,
        yes: bool,
        install_ok: bool,
        prompted: Cell<bool>,
        installed: Cell<bool>,
    }

    fn run_check(fx: &ZmxFx) -> ZmxResult {
        let command_exists = |name: &str| match name {
            "zmx" => fx.zmx_present,
            "brew" => fx.brew_present,
            _ => false,
        };
        let zmx_has_send = || fx.has_send;
        let prompt_yes_no = |_q: &str| {
            fx.prompted.set(true);
            fx.yes
        };
        let install_zmx = || {
            fx.installed.set(true);
            fx.install_ok
        };
        let deps = ZmxDeps {
            command_exists: &command_exists,
            zmx_has_send: &zmx_has_send,
            platform: fx.platform.clone(),
            interactive: fx.interactive,
            prompt_yes_no: &prompt_yes_no,
            install_zmx: &install_zmx,
        };
        check_zmx(&deps)
    }

    fn fx() -> ZmxFx {
        ZmxFx {
            zmx_present: false,
            brew_present: false,
            has_send: true,
            platform: "darwin".to_string(),
            interactive: false,
            yes: false,
            install_ok: false,
            prompted: Cell::new(false),
            installed: Cell::new(false),
        }
    }

    #[test]
    fn check_zmx_present_capable_no_prompt_no_install() {
        let f = ZmxFx {
            zmx_present: true,
            has_send: true,
            ..fx()
        };
        let r = run_check(&f);
        assert_eq!(r.zmx, ZmxStatus::Present);
        assert!(r.messages.is_empty());
        assert!(!f.prompted.get());
        assert!(!f.installed.get());
    }

    #[test]
    fn check_zmx_present_stale_is_stale() {
        let f = ZmxFx {
            zmx_present: true,
            has_send: false,
            ..fx()
        };
        let r = run_check(&f);
        assert_eq!(r.zmx, ZmxStatus::Stale);
        assert_eq!(r.messages.len(), 1);
        assert!(!f.installed.get());
    }

    #[test]
    fn check_zmx_missing_non_tty_warns_never_prompts() {
        let f = ZmxFx {
            zmx_present: false,
            brew_present: true,
            interactive: false,
            ..fx()
        };
        let r = run_check(&f);
        assert_eq!(r.zmx, ZmxStatus::Missing);
        assert!(!f.prompted.get(), "non-TTY must NEVER prompt");
        assert!(!f.installed.get());
    }

    #[test]
    fn check_zmx_missing_tty_declined_no_install() {
        let f = ZmxFx {
            zmx_present: false,
            brew_present: true,
            interactive: true,
            yes: false,
            ..fx()
        };
        let r = run_check(&f);
        assert_eq!(r.zmx, ZmxStatus::Missing);
        assert!(f.prompted.get());
        assert!(!f.installed.get(), "declined → no install");
    }

    #[test]
    fn check_zmx_missing_tty_accepted_installs() {
        let f = ZmxFx {
            zmx_present: false,
            brew_present: true,
            interactive: true,
            yes: true,
            install_ok: true,
            ..fx()
        };
        let r = run_check(&f);
        assert_eq!(r.zmx, ZmxStatus::Installed);
        assert!(f.installed.get());
    }

    #[test]
    fn check_zmx_missing_tty_accepted_install_fails() {
        let f = ZmxFx {
            zmx_present: false,
            brew_present: true,
            interactive: true,
            yes: true,
            install_ok: false,
            ..fx()
        };
        let r = run_check(&f);
        assert_eq!(r.zmx, ZmxStatus::Missing);
        assert!(f.installed.get());
        assert_eq!(r.messages.len(), 1);
        assert!(r.messages[0].contains("install failed"));
    }

    // --- relay finding + offer decider ------------------------------------

    fn rh(status: &str) -> RelayHealth {
        RelayHealth {
            session_id: "s1".to_string(),
            port: 8901,
            pid: 100,
            status: status.to_string(),
        }
    }

    #[test]
    fn relay_finding_absent_when_empty() {
        assert_eq!(classify_relay_finding(&[]), RelayFinding::Absent);
    }

    #[test]
    fn relay_finding_healthy_when_ok() {
        assert_eq!(
            classify_relay_finding(&[rh("ok")]),
            RelayFinding::PresentHealthy
        );
    }

    #[test]
    fn relay_finding_unhealthy_when_not_ok() {
        assert_eq!(
            classify_relay_finding(&[rh("degraded")]),
            RelayFinding::PresentUnhealthy
        );
    }

    // --- check_relay runtime (claude-mcp registration model, ADR 0017) -----

    struct RelayFx {
        interactive: bool,
        claude_present: bool,
        relay_registered: Option<bool>,
        yes: bool,
        register_ok: bool,
        prompted: Cell<bool>,
        registered: Cell<bool>,
    }

    fn run_relay(relays: &[RelayHealth], fx: &RelayFx) -> (RelayStepOutcome, Vec<String>) {
        let prompt = |_q: &str| {
            fx.prompted.set(true);
            fx.yes
        };
        let register = || {
            fx.registered.set(true);
            if fx.register_ok {
                Ok("/jail/deployed/qd".to_string())
            } else {
                Err("config write failed".to_string())
            }
        };
        let deps = RelayDeps {
            interactive: fx.interactive,
            claude_present: fx.claude_present,
            relay_registered: fx.relay_registered,
            prompt_yes_no: &prompt,
            register: &register,
        };
        check_relay(relays, &deps)
    }

    fn rfx() -> RelayFx {
        RelayFx {
            interactive: false,
            claude_present: true,
            relay_registered: Some(false),
            yes: false,
            register_ok: true,
            prompted: Cell::new(false),
            registered: Cell::new(false),
        }
    }

    #[test]
    fn check_relay_claude_missing_is_a_notice_not_a_prompt() {
        let f = RelayFx {
            interactive: true,
            claude_present: false,
            relay_registered: None,
            ..rfx()
        };
        let (o, lines) = run_relay(&[], &f);
        assert_eq!(o, RelayStepOutcome::ClaudeMissing);
        assert!(!f.prompted.get(), "no claude → never prompt");
        assert!(!f.registered.get());
        assert!(lines.iter().any(|l| l.contains("`claude` is not on PATH")));
    }

    #[test]
    fn check_relay_already_registered_non_tty_left_as_is_no_prompt() {
        // RECURRENCE FIX: registered + non-TTY leaves the existing registration
        // untouched and never prompts (no-hang), with a re-point how-to FYI.
        let f = RelayFx {
            interactive: false,
            relay_registered: Some(true),
            ..rfx()
        };
        let (o, lines) = run_relay(&[], &f);
        assert_eq!(o, RelayStepOutcome::ConfiguredAlready);
        assert!(!f.prompted.get(), "non-TTY must NEVER prompt");
        assert!(!f.registered.get());
        assert!(lines.iter().any(|l| l.contains("configured")));
        assert!(lines.iter().any(|l| l.contains("qd relay:repoint")));
    }

    #[test]
    fn check_relay_already_registered_tty_offers_repoint_declined() {
        // RECURRENCE FIX: registered + TTY OFFERS to re-point (consent-gated);
        // declining leaves the registration untouched.
        let f = RelayFx {
            interactive: true,
            relay_registered: Some(true),
            yes: false,
            ..rfx()
        };
        let (o, lines) = run_relay(&[], &f);
        assert_eq!(o, RelayStepOutcome::RepointDeclined);
        assert!(f.prompted.get(), "registered + TTY must offer the re-point");
        assert!(!f.registered.get(), "declined → re-point NEVER runs");
        assert!(lines.iter().any(|l| l.contains("left as-is")));
    }

    #[test]
    fn check_relay_already_registered_tty_accepted_repoints() {
        // RECURRENCE FIX: accepting re-points at the running binary (idempotent).
        let f = RelayFx {
            interactive: true,
            relay_registered: Some(true),
            yes: true,
            register_ok: true,
            ..rfx()
        };
        let (o, lines) = run_relay(&[], &f);
        assert_eq!(
            o,
            RelayStepOutcome::Repointed {
                command: "/jail/deployed/qd".to_string()
            }
        );
        assert!(f.registered.get(), "accepted → re-point ran");
        assert!(lines.iter().any(|l| l.contains("re-pointed")));
    }

    #[test]
    fn check_relay_already_registered_tty_repoint_fails_keeps_old() {
        let f = RelayFx {
            interactive: true,
            relay_registered: Some(true),
            yes: true,
            register_ok: false,
            ..rfx()
        };
        let (o, lines) = run_relay(&[], &f);
        assert_eq!(
            o,
            RelayStepOutcome::RepointFailed {
                error: "config write failed".to_string()
            }
        );
        assert!(lines.iter().any(|l| l.contains("re-point FAILED")));
    }

    #[test]
    fn check_relay_health_is_fyi_not_offer_signal() {
        // A HEALTHY RUNNING relay with NO registration still offers (registration
        // is the durable signal; runtime health is an FYI line).
        let f = RelayFx {
            interactive: true,
            relay_registered: Some(false),
            ..rfx()
        };
        let (o, lines) = run_relay(&[rh("ok")], &f);
        assert_eq!(o, RelayStepOutcome::Declined);
        assert!(f.prompted.get(), "unregistered must offer despite health");
        assert!(lines.iter().any(|l| l.contains("server is up")));
    }

    #[test]
    fn check_relay_undetermined_registration_treated_as_not_registered() {
        // relay_registered None (probe error) → offered on a TTY (fail toward
        // offering, not toward a silent skip).
        let f = RelayFx {
            interactive: true,
            relay_registered: None,
            yes: false,
            ..rfx()
        };
        let (o, _) = run_relay(&[], &f);
        assert_eq!(o, RelayStepOutcome::Declined);
        assert!(f.prompted.get());
    }

    #[test]
    fn check_relay_not_registered_non_tty_not_offered() {
        let f = RelayFx {
            interactive: false,
            ..rfx()
        };
        let (o, lines) = run_relay(&[], &f);
        assert_eq!(o, RelayStepOutcome::NotOffered);
        assert!(!f.prompted.get(), "non-TTY must NEVER prompt");
        assert!(!f.registered.get());
        assert!(lines.iter().any(|l| l.contains("qd relay:register")));
    }

    #[test]
    fn check_relay_tty_declined_no_register_no_hang() {
        let f = RelayFx {
            interactive: true,
            yes: false,
            ..rfx()
        };
        let (o, lines) = run_relay(&[], &f);
        assert_eq!(o, RelayStepOutcome::Declined);
        assert!(f.prompted.get());
        assert!(!f.registered.get(), "declined → registration NEVER runs");
        assert!(lines.iter().any(|l| l.contains("register later")));
    }

    #[test]
    fn check_relay_tty_accepted_registers() {
        let f = RelayFx {
            interactive: true,
            yes: true,
            register_ok: true,
            ..rfx()
        };
        let (o, lines) = run_relay(&[], &f);
        assert_eq!(
            o,
            RelayStepOutcome::Registered {
                command: "/jail/deployed/qd".to_string()
            }
        );
        assert!(f.registered.get());
        assert!(lines.iter().any(|l| l.contains("registered")));
    }

    #[test]
    fn check_relay_tty_accepted_register_fails_reports() {
        let f = RelayFx {
            interactive: true,
            yes: true,
            register_ok: false,
            ..rfx()
        };
        let (o, lines) = run_relay(&[], &f);
        assert_eq!(
            o,
            RelayStepOutcome::RegisterFailed {
                error: "config write failed".to_string()
            }
        );
        assert!(lines.iter().any(|l| l.contains("FAILED")));
    }

    // --- check_wrapper runtime ----------------------------------------------

    use crate::shell_init::Shell;

    struct WrapFx {
        shell: Option<Shell>,
        rc_contents: Option<String>,
        interactive: bool,
        yes: bool,
        add_ok: bool,
        prompted: Cell<bool>,
        added: Cell<bool>,
    }

    fn run_wrap(fx: &WrapFx) -> (WrapperStepOutcome, Vec<String>) {
        let prompt = |_q: &str| {
            fx.prompted.set(true);
            fx.yes
        };
        let add = || {
            fx.added.set(true);
            if fx.add_ok {
                Ok(())
            } else {
                Err("read-only fs".to_string())
            }
        };
        let deps = WrapperDeps {
            shell: fx.shell,
            rc_display: "~/.bashrc".to_string(),
            rc_contents: fx.rc_contents.clone(),
            interactive: fx.interactive,
            prompt_yes_no: &prompt,
            add_init_line: &add,
        };
        check_wrapper(&deps)
    }

    fn wfx() -> WrapFx {
        WrapFx {
            shell: Some(Shell::Bash),
            rc_contents: None,
            interactive: false,
            yes: false,
            add_ok: true,
            prompted: Cell::new(false),
            added: Cell::new(false),
        }
    }

    #[test]
    fn check_wrapper_already_configured_no_prompt() {
        let f = WrapFx {
            interactive: true,
            rc_contents: Some("eval \"$(qd init bash)\"\n".to_string()),
            ..wfx()
        };
        let (o, lines) = run_wrap(&f);
        assert_eq!(o, WrapperStepOutcome::AlreadyConfigured);
        assert!(!f.prompted.get());
        assert!(!f.added.get());
        assert!(lines.iter().any(|l| l.contains("configured")));
    }

    #[test]
    fn check_wrapper_unknown_shell_manual_pointer() {
        let f = WrapFx {
            shell: None,
            interactive: true,
            ..wfx()
        };
        let (o, lines) = run_wrap(&f);
        assert_eq!(o, WrapperStepOutcome::UnknownShell);
        assert!(!f.prompted.get());
        assert!(lines.iter().any(|l| l.contains("manually")));
    }

    #[test]
    fn check_wrapper_non_tty_never_prompts() {
        let f = WrapFx {
            interactive: false,
            ..wfx()
        };
        let (o, lines) = run_wrap(&f);
        assert_eq!(o, WrapperStepOutcome::NotOffered);
        assert!(!f.prompted.get(), "non-TTY must NEVER prompt");
        assert!(!f.added.get());
        assert!(lines.iter().any(|l| l.contains("qd init bash")));
    }

    #[test]
    fn check_wrapper_tty_declined_no_write() {
        let f = WrapFx {
            interactive: true,
            yes: false,
            ..wfx()
        };
        let (o, _) = run_wrap(&f);
        assert_eq!(o, WrapperStepOutcome::Declined);
        assert!(f.prompted.get());
        assert!(!f.added.get(), "declined → rc NEVER written");
    }

    #[test]
    fn check_wrapper_tty_accepted_adds_line() {
        let f = WrapFx {
            interactive: true,
            yes: true,
            ..wfx()
        };
        let (o, lines) = run_wrap(&f);
        assert_eq!(o, WrapperStepOutcome::Added);
        assert!(f.added.get());
        assert!(lines.iter().any(|l| l.contains("added")));
    }

    #[test]
    fn check_wrapper_add_failure_reports_and_keeps_exit_zero_semantics() {
        let f = WrapFx {
            interactive: true,
            yes: true,
            add_ok: false,
            ..wfx()
        };
        let (o, lines) = run_wrap(&f);
        assert_eq!(
            o,
            WrapperStepOutcome::AddFailed {
                error: "read-only fs".to_string()
            }
        );
        assert!(lines.iter().any(|l| l.contains("FAILED")));
    }

    #[test]
    fn check_wrapper_surfaces_retired_baked_block() {
        // The retired TS-era baked block is surfaced EVEN when the init line is
        // already present (it shadows the shipped wrapper if defined after).
        let f = WrapFx {
            interactive: true,
            rc_contents: Some(
                "# >>> qd bootstrap >>>\nclaude() { :; }\n# <<< qd bootstrap <<<\n\
                 eval \"$(qd init bash)\"\n"
                    .to_string(),
            ),
            ..wfx()
        };
        let (o, lines) = run_wrap(&f);
        assert_eq!(o, WrapperStepOutcome::AlreadyConfigured);
        assert!(
            lines.iter().any(|l| l.contains("RETIRED baked wrapper")),
            "{lines:?}"
        );
    }

    // --- check_extensions runtime (consent-gated install cascade) ----------

    struct ExtFx {
        interactive: bool,
        yes: bool,
        sbx_ok: bool,
        plugin_ok: bool,
        sbx_called: Cell<bool>,
        plugin_called: Cell<bool>,
    }

    fn run_ext(fx: &ExtFx) -> (ExtensionsStepOutcome, Vec<String>) {
        let prompt = |_q: &str| fx.yes;
        let install_sbx = || {
            fx.sbx_called.set(true);
            if fx.sbx_ok {
                Ok(())
            } else {
                Err("cargo not found".to_string())
            }
        };
        let install_plugin = || {
            fx.plugin_called.set(true);
            if fx.plugin_ok {
                Ok(())
            } else {
                Err("claude not found".to_string())
            }
        };
        let deps = ExtensionsDeps {
            interactive: fx.interactive,
            sbx_pin_label: "abc1234".to_string(),
            plugin_pin_label: "def5678".to_string(),
            prompt_yes_no: &prompt,
            install_sbx: &install_sbx,
            install_plugin: &install_plugin,
        };
        check_extensions(&deps)
    }

    fn efx() -> ExtFx {
        ExtFx {
            interactive: false,
            yes: false,
            sbx_ok: true,
            plugin_ok: true,
            sbx_called: Cell::new(false),
            plugin_called: Cell::new(false),
        }
    }

    #[test]
    fn check_extensions_non_tty_never_offers_never_installs() {
        let f = efx();
        let (o, lines) = run_ext(&f);
        assert_eq!(o.qb, ExtInstallOutcome::NotOffered);
        assert_eq!(o.plugin, ExtInstallOutcome::NotOffered);
        assert!(!f.sbx_called.get(), "non-TTY must NEVER install");
        assert!(!f.plugin_called.get(), "non-TTY must NEVER install");
        assert!(lines.iter().any(|l| l.contains("install later")));
    }

    #[test]
    fn check_extensions_tty_declined_installs_nothing() {
        let f = ExtFx {
            interactive: true,
            yes: false,
            ..efx()
        };
        let (o, _) = run_ext(&f);
        assert_eq!(o.qb, ExtInstallOutcome::Declined);
        assert_eq!(o.plugin, ExtInstallOutcome::Declined);
        assert!(!f.sbx_called.get(), "declined → no install");
        assert!(!f.plugin_called.get(), "declined → no install");
    }

    #[test]
    fn check_extensions_tty_accepted_installs_both() {
        let f = ExtFx {
            interactive: true,
            yes: true,
            ..efx()
        };
        let (o, lines) = run_ext(&f);
        assert_eq!(o.qb, ExtInstallOutcome::Installed);
        assert_eq!(o.plugin, ExtInstallOutcome::Installed);
        assert!(f.sbx_called.get());
        assert!(f.plugin_called.get());
        assert!(lines.iter().any(|l| l.contains("binary — installed")));
        assert!(lines.iter().any(|l| l.contains("plugin — installed")));
    }

    #[test]
    fn check_extensions_partial_safe_sbx_fails_plugin_still_offered() {
        // Partial-safe: an qb install FAILURE does NOT short-circuit the plugin
        // offer — the two are independent.
        let f = ExtFx {
            interactive: true,
            yes: true,
            sbx_ok: false,
            plugin_ok: true,
            ..efx()
        };
        let (o, lines) = run_ext(&f);
        assert!(matches!(o.qb, ExtInstallOutcome::Failed { .. }));
        assert_eq!(o.plugin, ExtInstallOutcome::Installed);
        assert!(
            f.plugin_called.get(),
            "plugin still attempted after qb fail"
        );
        assert!(lines.iter().any(|l| l.contains("binary — install FAILED")));
    }

    // --- run_bootstrap: idempotence + content-free report -----------------

    struct FsFx {
        existing: std::cell::RefCell<Vec<PathBuf>>,
        made: std::cell::RefCell<Vec<PathBuf>>,
    }

    fn run_bs(fs_fx: &FsFx, home: &Path, env: &dyn Env) -> BootstrapResult {
        let exists = |p: &Path| fs_fx.existing.borrow().iter().any(|e| e == p);
        let mkdir_p = |p: &Path| {
            fs_fx.made.borrow_mut().push(p.to_path_buf());
            // mkdir -p makes the dir exist for subsequent checks.
            fs_fx.existing.borrow_mut().push(p.to_path_buf());
            Ok(())
        };
        let bfs = BootstrapFs {
            exists: &exists,
            mkdir_p: &mkdir_p,
        };
        // zmx: present+capable (no prompts), relay: configured native (no
        // offer), wrapper: already configured (no offer).
        let command_exists = |_n: &str| true;
        let zmx_has_send = || true;
        let prompt_yes_no = |_q: &str| false;
        let install_zmx = || false;
        let zmx_deps = ZmxDeps {
            command_exists: &command_exists,
            zmx_has_send: &zmx_has_send,
            platform: "linux".to_string(),
            interactive: false,
            prompt_yes_no: &prompt_yes_no,
            install_zmx: &install_zmx,
        };
        let r_prompt = |_q: &str| false;
        let r_register = || Ok("/jail/deployed/qd".to_string());
        // relay: already registered (claude present) → no offer, clean report.
        let relay_deps = RelayDeps {
            interactive: false,
            claude_present: true,
            relay_registered: Some(true),
            prompt_yes_no: &r_prompt,
            register: &r_register,
        };
        let w_prompt = |_q: &str| false;
        let w_add = || Ok(());
        let wrapper_deps = WrapperDeps {
            shell: Some(crate::shell_init::Shell::Bash),
            rc_display: "~/.bashrc".to_string(),
            rc_contents: Some("eval \"$(qd init bash)\"\n".to_string()),
            interactive: false,
            prompt_yes_no: &w_prompt,
            add_init_line: &w_add,
        };
        // extensions: non-interactive → never offered (no installs), clean FYI.
        let e_prompt = |_q: &str| false;
        let e_install_sbx = || Ok(());
        let e_install_plugin = || Ok(());
        let extensions_deps = ExtensionsDeps {
            interactive: false,
            sbx_pin_label: "abc1234".to_string(),
            plugin_pin_label: "def5678".to_string(),
            prompt_yes_no: &e_prompt,
            install_sbx: &e_install_sbx,
            install_plugin: &e_install_plugin,
        };
        let paths = resolve_bootstrap_paths(home, env);
        let relays = vec![rh("ok")];
        run_bootstrap(
            paths,
            &bfs,
            &zmx_deps,
            &relays,
            &relay_deps,
            &wrapper_deps,
            &extensions_deps,
        )
        .unwrap()
    }

    #[test]
    fn bootstrap_idempotent_same_state_both_runs() {
        let env = map_env(&[]);
        let home = PathBuf::from("/jail/home");
        let fs_fx = FsFx {
            existing: std::cell::RefCell::new(vec![]),
            made: std::cell::RefCell::new(vec![]),
        };
        // First run: nothing exists → created.
        let r1 = run_bs(&fs_fx, &home, &env);
        assert!(!r1.already_existed);
        // Second run: dirs now exist → already-present, identical paths.
        let r2 = run_bs(&fs_fx, &home, &env);
        assert!(r2.already_existed);
        assert_eq!(r1.paths, r2.paths);
    }

    #[test]
    fn bootstrap_report_is_well_formed() {
        // POSITIVE structure: every report line is `[bootstrap]`-prefixed and the
        // report names only its own engine concepts (state dir, zmx, relay). The
        // NEGATIVE forbidden-token enumeration (carry 5 / G-B5) deliberately lives
        // in `scenarios/bootstrap_output_audit.sh`, NOT here: spelling the banned
        // tokens out as string literals in engine source would itself trip the CI
        // scope-audit (scope-audit.sh: the engine is content-free). The scenario
        // greps the SHIPPED binary's real output for those tokens.
        let env = map_env(&[]);
        let home = PathBuf::from("/jail/home");
        let fs_fx = FsFx {
            existing: std::cell::RefCell::new(vec![]),
            made: std::cell::RefCell::new(vec![]),
        };
        let r = run_bs(&fs_fx, &home, &env);
        assert!(!r.report.is_empty());
        for line in &r.report {
            assert!(line.starts_with("[bootstrap]"), "unprefixed line: {line}");
        }
        // The report surfaces the four engine concepts it owns.
        let joined = r.report.join("\n");
        assert!(joined.contains("state"), "missing state line:\n{joined}");
        assert!(joined.contains("zmx:"), "missing zmx line:\n{joined}");
        assert!(joined.contains("relay:"), "missing relay line:\n{joined}");
        assert!(joined.contains("shell:"), "missing shell line:\n{joined}");
        assert!(
            joined.contains("extensions:"),
            "missing extensions line:\n{joined}"
        );
    }
}
