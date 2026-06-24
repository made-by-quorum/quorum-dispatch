//! REAL `sb bootstrap` backend. Thin binding over the pure [`dispatch::bootstrap`]
//! library: resolve HOME → engine paths, wire the real seams (fs / `RealExec` /
//! TTY prompt / relay probe / native relay registration / shell-integration
//! offer), run, and print the `[bootstrap]` report. Exit 0/1 ONLY (ADR 0008):
//! 1 only if a state-dir mkdir failed; the zmx + relay + shell steps are
//! consent-gated notices and never fail bootstrap.
//!
//! 2026-06-10 ruling (ADR 0017): the relay step REGISTERS the native
//! `dispatch relay:serve` with Claude Code via `claude mcp add -s user` (consent-
//! gated, as the bare command (resolved via PATH)), detecting via `claude mcp get` — Claude
//! Code owns its own MCP config location/format (the hand-written
//! `~/.claude/.mcp.json` of ADR 0016 is NOT read by CC 2.1.x). The shell step
//! offers the ONE-LINE eval-init hook; the wrapper body ships in the binary via
//! `sb init` and is never baked into rc files.

use std::path::Path;

use dispatch::bootstrap::{
    discover_relays, real_command_exists, real_zmx_has_send, resolve_bootstrap_paths,
    run_bootstrap, BootstrapFs, ExtensionsDeps, RelayDeps, WrapperDeps, ZmxDeps,
};
use dispatch::effects::{Env, RealEnv};
use dispatch::exec::{Exec, RealExec};
use dispatch::paths::SbPaths;
use dispatch::relay_http::HttpRelayProbe;
use dispatch::relay_server::register;
use dispatch::shell_init::{init_line, rc_path, Shell};

use super::super::tty;

/// `sb bootstrap` — set up the engine's local data dir (`~/.sb` + `~/.sb/state`),
/// run the zmx capability notice, the native relay-registration offer, and the
/// shell-integration offer. Idempotent: a re-run on an existing `~/.sb` is a
/// no-op apart from re-checks.
pub fn run() -> i32 {
    let env = RealEnv;
    let exec = RealExec;

    // HOME → engine paths (L9a: through the env seam, never the real home directly).
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => h,
        None => {
            eprintln!("sb bootstrap: HOME is not set — cannot resolve the engine state dir.");
            return 1;
        }
    };
    let home = Path::new(&home).to_path_buf();
    let paths = resolve_bootstrap_paths(&home, &env);

    // State-dir fs seam (real).
    let exists = |p: &Path| p.exists();
    let mkdir_p = |p: &Path| std::fs::create_dir_all(p).map_err(|e| e.to_string());
    let bfs = BootstrapFs {
        exists: &exists,
        mkdir_p: &mkdir_p,
    };

    // zmx step seams (real probes / TTY prompt / brew install).
    let interactive = tty::stdin_and_stdout_are_tty();
    let command_exists = |name: &str| real_command_exists(&exec, name);
    let zmx_has_send = || real_zmx_has_send(&exec);
    let prompt_yes_no = |q: &str| tty::prompt_yes_no_default_no(q);
    let install_zmx = || {
        // `brew install <formula>`, inherit stdio. Only called on an explicit yes.
        matches!(
            exec.spawn_inherit(
                "brew",
                &[
                    "install".to_string(),
                    dispatch::bootstrap::ZMX_BREW_FORMULA.to_string(),
                ],
                &[],
                None,
            ),
            Ok(0)
        )
    };
    let zmx_deps = ZmxDeps {
        command_exists: &command_exists,
        zmx_has_send: &zmx_has_send,
        platform: std::env::consts::OS.to_string(),
        interactive,
        prompt_yes_no: &prompt_yes_no,
        install_zmx: &install_zmx,
    };

    // Relay step: health discovery via A4's sidecar path (FYI line), registration
    // state + the register closure via `claude mcp` (register.rs).
    //
    // The HTTP port-scan FALLBACK (HttpRelayProbe) probes fixed localhost ports
    // 8900-9000 — a HOST-WIDE probe that is NOT jail-scoped (the jail isolates
    // HOME/SB_HOME/ZMX_DIR but cannot sandbox a localhost TCP scan). For the
    // hermetic gate's relay-ABSENT arm to be deterministic on a shared host,
    // `SB_RELAY_DISABLE_SCAN=1` suppresses the scan so discovery sees ONLY
    // jailed sidecar files. Default (unset) keeps the production scan.
    let sb_paths = SbPaths::from_home(&home);
    let scan_disabled = env
        .var("SB_RELAY_DISABLE_SCAN")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let http_probe = HttpRelayProbe::new();
    let empty_probe = dispatch::effects::FixtureRelayProbe(vec![]);
    let probe: &dyn dispatch::effects::RelayProbe = if scan_disabled {
        &empty_probe
    } else {
        &http_probe
    };
    let relays = discover_relays(&sb_paths.relay_dir, probe);

    // Registration is driven through Claude Code's own `claude mcp` CLI — so it
    // requires `claude` on PATH, and detection/registration both shell out.
    let claude_present = real_command_exists(&exec, "claude");
    let relay_registered = if claude_present {
        register::relay_is_registered(&exec)
    } else {
        None
    };
    let register = || -> Result<String, String> {
        // Register the BARE `sb` command (resolved via PATH) — never goes stale on
        // a binary move/upgrade (relay-path hardening v2). Return the command for
        // the report line.
        register::register_relay(&exec).map(|_| register::RELAY_BARE_COMMAND.to_string())
    };
    let r_prompt = |q: &str| tty::prompt_yes_no_default_no(q);
    let relay_deps = RelayDeps {
        interactive,
        claude_present,
        relay_registered,
        prompt_yes_no: &r_prompt,
        register: &register,
    };

    // Shell-integration step: classify $SHELL, pre-read the rc file, and wire
    // the one-line append (creating the file + parents when needed — the fish
    // conf.d path does not exist until first use).
    let shell = env.var("SHELL").as_deref().and_then(Shell::from_name);
    let rc = shell.map(|s| rc_path(s, &home, &env));
    let rc_display = rc
        .as_ref()
        .map(|p| {
            let s = p.to_string_lossy().into_owned();
            match s.strip_prefix(&format!("{}/", home.display())) {
                Some(rest) => format!("~/{rest}"),
                None => s,
            }
        })
        .unwrap_or_else(|| "your shell rc file".to_string());
    let rc_contents = rc.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
    let add_rc = rc.clone();
    let add_init_line = move || -> Result<(), String> {
        let (rc, shell) = match (add_rc.as_ref(), shell) {
            (Some(rc), Some(shell)) => (rc, shell),
            _ => return Err("no rc path resolved".to_string()),
        };
        if let Some(parent) = rc.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let existing = std::fs::read_to_string(rc).unwrap_or_default();
        let mut out = existing.clone();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n# sb shell integration (claude wrapper) — see `sb init --help`.\n");
        out.push_str(init_line(shell));
        out.push('\n');
        std::fs::write(rc, out).map_err(|e| e.to_string())
    };
    let w_prompt = |q: &str| tty::prompt_yes_no_default_no(q);
    let wrapper_deps = WrapperDeps {
        shell,
        rc_display,
        rc_contents,
        interactive,
        prompt_yes_no: &w_prompt,
        add_init_line: &add_init_line,
    };

    // Extension-install step (plan D-install): offer to install the PINNED
    // extension binary + work-model plugin. The engine is content-free — the
    // ACTUAL install actions live in an EXTERNAL script (`install_script`,
    // resolved by path below); these closures shell to it with an opaque
    // subcommand and inherit stdio (so the installer's progress is visible). The
    // pinned refs come from the baked manifest (dispatch::extensions).
    //
    // OPT-IN gate: the cascade prompts (and runs real network installs:
    // `cargo install --git` + `claude plugin ...`) ONLY when
    // `SB_BOOTSTRAP_INSTALL_EXTENSIONS=1`. Default OFF so (a) the hermetic
    // bootstrap gate's PTY arm is not disturbed by extra prompts/real installs,
    // and (b) a default bootstrap never reaches out to the network unasked. With
    // the flag unset the step is forced non-interactive: it prints only the
    // "install later" FYI and prompts/installs nothing. Once the gate is
    // extended to feed the two extra answers, this opt-in can become the default.
    let install_extensions_enabled = env
        .var("SB_BOOTSTRAP_INSTALL_EXTENSIONS")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let ext_interactive = interactive && install_extensions_enabled;
    let pins = dispatch::extensions::pinned();
    let sbx_pin_label = short_pin(&pins.sbx.rev);
    let plugin_pin_label = short_pin(&pins.plugins.rev);
    let install_script = resolve_install_script(&env);
    let run_installer = |sub: &str| -> Result<(), String> {
        let script = install_script.clone().ok_or_else(|| {
            "install script not found — set SB_INSTALL_EXTENSIONS_SCRIPT to its path \
             (ships in the repo under scripts/; a packaged binary needs it staged)"
                .to_string()
        })?;
        match exec.spawn_inherit(
            "bash",
            &[script.to_string_lossy().into_owned(), sub.to_string()],
            &[],
            None,
        ) {
            Ok(0) => Ok(()),
            Ok(code) => Err(format!("installer exited {code}")),
            Err(e) => Err(e.to_string()),
        }
    };
    let e_prompt = |q: &str| tty::prompt_yes_no_default_no(q);
    let install_sbx = || run_installer("sbx");
    let install_plugin = || run_installer("plugin");
    let extensions_deps = ExtensionsDeps {
        interactive: ext_interactive,
        sbx_pin_label,
        plugin_pin_label,
        prompt_yes_no: &e_prompt,
        install_sbx: &install_sbx,
        install_plugin: &install_plugin,
    };

    match run_bootstrap(
        paths,
        &bfs,
        &zmx_deps,
        &relays,
        &relay_deps,
        &wrapper_deps,
        &extensions_deps,
    ) {
        Ok(result) => {
            for line in &result.report {
                println!("{line}");
            }
            0
        }
        Err(e) => {
            eprintln!("sb bootstrap: {e}");
            1
        }
    }
}

/// A short, opaque label for a pinned git ref (first 8 chars of the sha, or
/// `"unset"` if the manifest field was empty). Content-free; report-only.
fn short_pin(rev: &str) -> String {
    if rev.is_empty() {
        return "unset".to_string();
    }
    rev.chars().take(8).collect()
}

/// Locate the external extension installer script. Resolution order:
///   1. `$SB_INSTALL_EXTENSIONS_SCRIPT` (explicit override — the packaged-install
///      path: a binary install has no repo, so the packager stages the script and
///      points this at it).
///   2. `<dir-of-running-exe>/../scripts/install-extensions.sh` and
///      `<dir-of-running-exe>/scripts/install-extensions.sh` (dev: running from a
///      target/ build inside the repo).
/// Returns `None` if none exist — the install closures then report a clean
/// "install script not found" rather than silently skipping.
fn resolve_install_script(env: &impl Env) -> Option<std::path::PathBuf> {
    if let Some(p) = env
        .var("SB_INSTALL_EXTENSIONS_SCRIPT")
        .filter(|s| !s.is_empty())
    {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidates = [
        dir.join("scripts/install-extensions.sh"),
        dir.join("../scripts/install-extensions.sh"),
        // target/release/sb → repo-root/scripts.
        dir.join("../../scripts/install-extensions.sh"),
        dir.join("../../../scripts/install-extensions.sh"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}
