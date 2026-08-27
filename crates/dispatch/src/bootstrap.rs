//! `qd bootstrap` — ENGINE-ONLY (A5 spec §4.1/§4.2; named divergence §9 item 4).
//!
//! Ported from `0d0fa9e:src/commands/bootstrap.ts`, SHRUNK to the engine core:
//! the qb-owned deploy steps (artifact deploy, shell-profile patch, plugin
//! registration) are DROPPED (ruled — that content is the qb deploy's, not the
//! engine's; the dropped TS step names live in the A5 spec, never in this
//! repo). What survives is the state-dir creation (`~/.quorum/dispatch` +
//! `~/.quorum/dispatch/state`). On top of that, A5 adds the ADD-5 relay-driver
//! detect→offer→self-install step (§4.2).
//!
//! RETIRED STEP — the TS-era terminal-multiplexer capability notice (its pure
//! decider + runtime probe pair) is GONE (FTUE R1). It probed for a multiplexer
//! qd no longer drives and, on a fresh macOS box, made a `brew install` offer
//! for it the FIRST prompt a new human ever saw. Removed rather than repaired:
//! with nothing driving that mux there is no capability left to notice, so the
//! step had no truth to report. Do not reintroduce a probe here.
//!
//! Library-first (spec §2): the decision logic is PURE
//! ([`classify_relay_finding`] / [`decide_wrapper_offer`]); the runtime
//! ([`check_relay`] / [`run_bootstrap`]) is a thin shell over injected effects so
//! tests never prompt a real TTY or shell out a real relay installer.
//!
//! Output is `[bootstrap]`-prefixed, engine-truthful, and CONTENT-FREE: it
//! names none of the qb-side content concepts (carry 5; the forbidden-token
//! set lives in `scenarios/bootstrap_output_audit.sh`, which runtime-asserts
//! the shipped binary's output, gate row G-B5).

use std::path::{Path, PathBuf};

use crate::effects::Env;
use crate::exec::Exec;
use crate::model::RelayHealth;
use crate::relay;

// ----------------------------------------------------------------------------
// Paths (shrunk port of resolveBootstrapPaths, 0d0fa9e:src/commands/bootstrap.ts:87).
//
// The TS struct carried five qb-deploy-owned dirs + a provenance stamp — ALL
// DROPPED here (engine is content-free; the dropped field names live in the A5
// spec). The engine owns ONLY its state home: `~/.quorum/dispatch` and `~/.quorum/dispatch/state`. QD_HOME
// comes through the injected `Env` seam (L9a — nothing resolves the real home),
// never raw `std::env`.
// ----------------------------------------------------------------------------

/// The engine's state-dir layout (shrunk `BootstrapPaths`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapPaths {
    /// The qd data root, `QD_HOME` or `<home>/.quorum/dispatch` (bootstrap.ts:88-96).
    pub qd_home: PathBuf,
    /// Reserved hot-state dir, `<qdHome>/state` (bootstrap.ts:90 stateDir).
    pub state_dir: PathBuf,
}

/// Resolve the engine's bootstrap paths from the injected home + env
/// (`qdHome = QD_HOME || <home>/.quorum/dispatch`, bootstrap.ts:88-96). The home is injected
/// (never resolved from the real environment here, L9a); QD_HOME is read ONLY
/// through `env`.
pub fn resolve_bootstrap_paths(home: &Path, env: &dyn Env) -> BootstrapPaths {
    let qd_home = env
        .var("QD_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".quorum").join("dispatch"));
    let state_dir = qd_home.join("state");
    BootstrapPaths { qd_home, state_dir }
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
//
// RENDER RULE (FTUE R2 — the step must not contradict itself). The step reports
// TWO INDEPENDENT AXES and a fresh machine can legitimately be "no" on one and
// "yes" on the other:
//   - REGISTRATION — is a relay MCP server registered with Claude Code? This is
//     the durable fact, and the only one that decides whether NEW sessions load
//     a relay.
//   - RUNNING NOW — is a relay server PROCESS alive on this host right now? A
//     transient fact about existing sessions; it registers nothing.
// Rendered bare, those read as a flat contradiction ("not configured" directly
// above "a relay server is up"). So EVERY line names its axis, and the
// running-now FYI is emitted LAST — after the registration fact AND its how-to
// — so the registration story reads start-to-finish before the process aside.
// Keep both properties if these strings are ever reworded.
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

/// Injected effects for the relay step runtime. Kept separate from the other
/// steps' dep structs so their seams don't entangle. The `claude mcp` shell-out
/// lives in [`crate::relay_server::register`]; the bin layer wires it into these.
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

/// Append the RUNNING-NOW axis line for a discovered relay server (nothing when
/// none is up — silence is the normal idle-fleet state).
///
/// The line explicitly disclaims the registration axis. Without that, it lands
/// under a "not registered" line and reads as a denial of it (FTUE R2) — the two
/// are different questions, and the answers are allowed to differ.
fn push_relay_health_fyi(relays: &[RelayHealth], lines: &mut Vec<String>) {
    match classify_relay_finding(relays) {
        RelayFinding::PresentHealthy => lines.push(
            "relay: running now — a relay server process is up and healthy \
             (a live process on this host, not a registration)."
                .to_string(),
        ),
        RelayFinding::PresentUnhealthy => lines.push(
            "relay: running now — a relay server process is up but NOT healthy \
             (a live process on this host, not a registration)."
                .to_string(),
        ),
        RelayFinding::Absent => {}
    }
}

/// The relay step runtime: precondition-check `claude`, report registration
/// state, decide the offer, run the injected registration on an explicit yes,
/// and close with the runtime-health FYI. Returns the outcome + the report
/// lines. NEVER hangs on a declined / non-TTY path; NEVER fails bootstrap.
pub fn check_relay(relays: &[RelayHealth], deps: &RelayDeps) -> (RelayStepOutcome, Vec<String>) {
    // Registration axis first (state + how-to), running-now axis LAST — see the
    // RENDER RULE above. Splitting the registration half into its own function is
    // what makes "the FYI is always last" true by construction rather than by
    // remembering to append it on each of the nine return paths.
    let (outcome, mut lines) = check_relay_registration(deps);
    push_relay_health_fyi(relays, &mut lines);
    (outcome, lines)
}

/// The REGISTRATION axis alone: is a relay MCP server registered with Claude
/// Code, and (consent-gated) should we register or re-point one? Every line it
/// emits is prefixed `relay: registration` so it cannot be misread as a claim
/// about a running process.
fn check_relay_registration(deps: &RelayDeps) -> (RelayStepOutcome, Vec<String>) {
    // Precondition: we register by driving `claude mcp`. No claude → notice.
    if !deps.claude_present {
        let lines = vec![
            "relay: registration — cannot configure: `claude` is not on PATH. \
             Install Claude Code, then run: qd relay:register"
                .to_string(),
        ];
        return (RelayStepOutcome::ClaudeMissing, lines);
    }

    let registered = deps.relay_registered.unwrap_or(false);
    let mut lines = vec![if registered {
        "relay: registration — configured (registered with Claude Code, user scope); \
         new sessions will load it."
            .to_string()
    } else {
        "relay: registration — not configured (no relay MCP server registered with \
         Claude Code); new sessions will not load one."
            .to_string()
    }];

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
                "relay: registration — re-point to this binary later (after moving `qd`) \
                 with: qd relay:repoint"
                    .to_string(),
            );
            return (RelayStepOutcome::ConfiguredAlready, lines);
        }
        let question = "Re-point the relay registration at THIS qd binary (idempotent; \
             fixes a stale path after the `qd` binary moves)? [y/N] ";
        if !(deps.prompt_yes_no)(question) {
            lines.push(
                "relay: registration — left as-is; re-point later with: qd relay:repoint"
                    .to_string(),
            );
            return (RelayStepOutcome::RepointDeclined, lines);
        }
        return match (deps.register)() {
            Ok(command) => {
                lines.push(format!(
                    "relay: registration — re-pointed; new sessions will load `{command} relay:serve`."
                ));
                (RelayStepOutcome::Repointed { command }, lines)
            }
            Err(error) => {
                lines.push(format!(
                    "relay: registration — re-point FAILED ({error}); old registration kept, \
                     retry with: qd relay:repoint"
                ));
                (RelayStepOutcome::RepointFailed { error }, lines)
            }
        };
    }

    // Not registered. Offer only on a TTY.
    if !deps.interactive {
        lines.push("relay: registration — register later with: qd relay:register".to_string());
        return (RelayStepOutcome::NotOffered, lines);
    }

    let question = "Register qd's relay MCP server with Claude Code (runs \
         `claude mcp add`; enables cross-session messaging in NEW sessions)? [y/N] ";
    if !(deps.prompt_yes_no)(question) {
        lines.push(
            "relay: registration — skipped; register later with: qd relay:register".to_string(),
        );
        return (RelayStepOutcome::Declined, lines);
    }

    match (deps.register)() {
        Ok(command) => {
            lines.push(format!(
                "relay: registration — registered; new sessions will load `{command} relay:serve`."
            ));
            (RelayStepOutcome::Registered { command }, lines)
        }
        Err(error) => {
            lines.push(format!(
                "relay: registration — FAILED ({error}); register later with: qd relay:register"
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
        "Add the claude/codex/pi/opencode shell wrappers to {} (routes a bare \
         `claude`, `codex`, `pi` or `opencode` into a tracked qd session; adds one \
         line: {})? [y/N] ",
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
    pub qb_pin_label: String,
    /// A short opaque label for the pinned plugin ref, for the report line.
    pub plugin_pin_label: String,
    /// Ask a yes/no question; default No.
    pub prompt_yes_no: &'a dyn Fn(&str) -> bool,
    /// Run the external installer for the `qb` extension. Returns Ok on success,
    /// Err(message) on failure (toolchain/auth/build). Only called on a yes.
    pub install_qb: &'a dyn Fn() -> Result<(), String>,
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
            deps.qb_pin_label
        ),
        deps.install_qb,
        deps.prompt_yes_no,
        "extensions: binary — install later with: qd bootstrap (on a TTY)",
        &format!(
            "extensions: binary — installed (pinned {}).",
            deps.qb_pin_label
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
    /// True if `qd_home`/`state_dir` already existed (idempotent re-run).
    pub already_existed: bool,
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
/// native relay-registration step, the shell-integration step and the
/// extension-install step, and build the `[bootstrap]` report. Returns the
/// result; the caller prints `report` and maps to an exit code (0 unless a
/// state-dir mkdir failed — the only hard failure; the relay/shell/extension
/// steps are consent-gated notices and NEVER fail bootstrap, ADR 0008).
pub fn run_bootstrap(
    paths: BootstrapPaths,
    fs: &BootstrapFs,
    relays: &[RelayHealth],
    relay_deps: &RelayDeps,
    wrapper_deps: &WrapperDeps,
    extensions_deps: &ExtensionsDeps,
) -> Result<BootstrapResult, String> {
    let already_existed = (fs.exists)(&paths.qd_home) && (fs.exists)(&paths.state_dir);

    // Ensure the state dirs (idempotent: mkdir -p on an existing dir is a no-op).
    (fs.mkdir_p)(&paths.qd_home)?;
    (fs.mkdir_p)(&paths.state_dir)?;

    let (relay, relay_lines) = check_relay(relays, relay_deps);
    let (wrapper, wrapper_lines) = check_wrapper(wrapper_deps);
    let (extensions, extension_lines) = check_extensions(extensions_deps);

    // Build the engine-truthful, content-free report.
    let mut report = Vec::new();
    report.push(format!(
        "[bootstrap] state dir: {}",
        paths.qd_home.display()
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
    fn paths_default_to_home_dot_qd() {
        let env = map_env(&[]);
        let p = resolve_bootstrap_paths(Path::new("/jail/home"), &env);
        assert_eq!(p.qd_home, PathBuf::from("/jail/home/.quorum/dispatch"));
        assert_eq!(
            p.state_dir,
            PathBuf::from("/jail/home/.quorum/dispatch/state")
        );
    }

    #[test]
    fn paths_honor_qd_home_override() {
        let env = map_env(&[("QD_HOME", "/jail/qdhome")]);
        let p = resolve_bootstrap_paths(Path::new("/jail/home"), &env);
        assert_eq!(p.qd_home, PathBuf::from("/jail/qdhome"));
        assert_eq!(p.state_dir, PathBuf::from("/jail/qdhome/state"));
    }

    #[test]
    fn paths_ignore_empty_qd_home() {
        let env = map_env(&[("QD_HOME", "")]);
        let p = resolve_bootstrap_paths(Path::new("/jail/home"), &env);
        assert_eq!(p.qd_home, PathBuf::from("/jail/home/.quorum/dispatch"));
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
        assert!(lines
            .iter()
            .any(|l| l.contains("running now — a relay server process is up and healthy")));
    }

    // --- R2: the two axes must not read as a contradiction ------------------
    //
    // The exact case that produced the punch-list report: a fresh machine that is
    // NOT registered but DOES have a relay process running. Both facts are true;
    // rendered bare they read as "not configured" / "a relay server is up" — a
    // flat self-contradiction. These tests pin the fix at the RENDERING level:
    // every line names its axis, and the transient running-now fact is emitted
    // LAST so the registration story reads start-to-finish first.

    #[test]
    fn relay_lines_name_both_axes_when_unregistered_but_running() {
        let f = RelayFx {
            interactive: false,
            relay_registered: Some(false),
            ..rfx()
        };
        let (o, lines) = run_relay(&[rh("ok")], &f);
        assert_eq!(o, RelayStepOutcome::NotOffered);
        // Registration axis: the durable fact + what it costs the user.
        assert!(
            lines[0].starts_with("relay: registration — not configured"),
            "{lines:?}"
        );
        assert!(
            lines[0].contains("new sessions will not load one"),
            "{lines:?}"
        );
        // Its how-to stays with it, BEFORE the process aside.
        assert!(
            lines[1].starts_with("relay: registration — register later"),
            "{lines:?}"
        );
        // Running-now axis: LAST, self-labelled, and explicitly not a registration.
        assert!(lines[2].starts_with("relay: running now —"), "{lines:?}");
        assert!(lines[2].contains("not a registration"), "{lines:?}");
        assert_eq!(lines.len(), 3, "{lines:?}");
        // The retired rendering: no line may claim (un)configured without saying
        // WHICH axis it means — that ambiguity is what read as a contradiction.
        for l in &lines {
            assert!(
                l.starts_with("relay: registration —") || l.starts_with("relay: running now —"),
                "unaxed relay line: {l}"
            );
        }
    }

    #[test]
    fn relay_health_fyi_is_always_last_even_when_registration_acts() {
        // The FYI trails the registration RESULT too, not just the state line —
        // otherwise an accepted registration reads as interrupted by the aside.
        let f = RelayFx {
            interactive: true,
            relay_registered: Some(false),
            yes: true,
            register_ok: true,
            ..rfx()
        };
        let (_, lines) = run_relay(&[rh("degraded")], &f);
        let last = lines.last().expect("lines");
        assert!(last.starts_with("relay: running now —"), "{lines:?}");
        assert!(last.contains("NOT healthy"), "{lines:?}");
        assert!(
            lines[lines.len() - 2].contains("registration — registered"),
            "{lines:?}"
        );
    }

    #[test]
    fn relay_claude_missing_still_names_the_registration_axis() {
        let f = RelayFx {
            interactive: true,
            claude_present: false,
            relay_registered: None,
            ..rfx()
        };
        let (o, lines) = run_relay(&[rh("ok")], &f);
        assert_eq!(o, RelayStepOutcome::ClaudeMissing);
        assert!(
            lines[0].starts_with("relay: registration — cannot configure"),
            "{lines:?}"
        );
        assert!(
            lines.last().unwrap().starts_with("relay: running now —"),
            "{lines:?}"
        );
    }

    #[test]
    fn relay_absent_process_emits_no_running_now_line() {
        // Silence is the normal idle-fleet state — an "and nothing is running"
        // line would be noise on every fresh machine.
        let f = rfx();
        let (_, lines) = run_relay(&[], &f);
        assert!(
            !lines.iter().any(|l| l.contains("running now")),
            "{lines:?}"
        );
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
        qb_ok: bool,
        plugin_ok: bool,
        qb_called: Cell<bool>,
        plugin_called: Cell<bool>,
    }

    fn run_ext(fx: &ExtFx) -> (ExtensionsStepOutcome, Vec<String>) {
        let prompt = |_q: &str| fx.yes;
        let install_qb = || {
            fx.qb_called.set(true);
            if fx.qb_ok {
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
            qb_pin_label: "abc1234".to_string(),
            plugin_pin_label: "def5678".to_string(),
            prompt_yes_no: &prompt,
            install_qb: &install_qb,
            install_plugin: &install_plugin,
        };
        check_extensions(&deps)
    }

    fn efx() -> ExtFx {
        ExtFx {
            interactive: false,
            yes: false,
            qb_ok: true,
            plugin_ok: true,
            qb_called: Cell::new(false),
            plugin_called: Cell::new(false),
        }
    }

    #[test]
    fn check_extensions_non_tty_never_offers_never_installs() {
        let f = efx();
        let (o, lines) = run_ext(&f);
        assert_eq!(o.qb, ExtInstallOutcome::NotOffered);
        assert_eq!(o.plugin, ExtInstallOutcome::NotOffered);
        assert!(!f.qb_called.get(), "non-TTY must NEVER install");
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
        assert!(!f.qb_called.get(), "declined → no install");
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
        assert!(f.qb_called.get());
        assert!(f.plugin_called.get());
        assert!(lines.iter().any(|l| l.contains("binary — installed")));
        assert!(lines.iter().any(|l| l.contains("plugin — installed")));
    }

    #[test]
    fn check_extensions_partial_safe_qb_fails_plugin_still_offered() {
        // Partial-safe: an qb install FAILURE does NOT short-circuit the plugin
        // offer — the two are independent.
        let f = ExtFx {
            interactive: true,
            yes: true,
            qb_ok: false,
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
        // relay: configured native (no offer), wrapper: already configured (no
        // offer) — every step non-interactive so the harness never prompts.
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
        let e_install_qb = || Ok(());
        let e_install_plugin = || Ok(());
        let extensions_deps = ExtensionsDeps {
            interactive: false,
            qb_pin_label: "abc1234".to_string(),
            plugin_pin_label: "def5678".to_string(),
            prompt_yes_no: &e_prompt,
            install_qb: &e_install_qb,
            install_plugin: &e_install_plugin,
        };
        let paths = resolve_bootstrap_paths(home, env);
        let relays = vec![rh("ok")];
        run_bootstrap(
            paths,
            &bfs,
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
        // report names only its own engine concepts (state dir, relay, shell,
        // extensions). The
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
        assert!(joined.contains("relay:"), "missing relay line:\n{joined}");
        assert!(joined.contains("shell:"), "missing shell line:\n{joined}");
        assert!(
            joined.contains("extensions:"),
            "missing extensions line:\n{joined}"
        );
    }
}
