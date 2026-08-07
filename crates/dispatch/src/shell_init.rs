//! `qd init <shell>` — shell-integration emission (the eval-init pattern).
//!
//! ## Why emission instead of a baked rc block
//!
//! The retired TS bootstrap WROTE a `claude()` wrapper function into the user's
//! `~/.bashrc` between markers. That block was a fossil the moment it was
//! written: when the engine's `new` verb changed its argument contract, every
//! baked wrapper on every machine silently broke (observed live 2026-06-09:
//! `qd new -- "$@"` with no name → `error: missing required argument 'name'`).
//!
//! The eval-init pattern (starship / zoxide / direnv precedent) inverts the
//! ownership: the rc file carries ONE stable line —
//!
//! ```text
//! eval "$(qd init bash)"          # ~/.bashrc / ~/.zshrc (zsh variant)
//! qd init fish | source           # ~/.config/fish/conf.d/qd.fish
//! ```
//!
//! — and the wrapper BODY ships inside this binary, so it can never drift from
//! what the engine's verbs actually accept.
//!
//! ## What the wrapper does
//!
//! `claude` typed bare in an interactive terminal (outside zmx) creates a
//! tracked session detached (`qd start <generated-name>`) and attaches to it
//! (`qd attach`). `qd start --attach` would be the one-shot form, but it is an
//! engine-deferred surface today, so start-then-attach is the supported path.
//! If `qd start` fails for any reason — most commonly a first-run folder-trust
//! dialog that blocks the boot-to-idle wait — the wrapper FALLS BACK to
//! launching claude directly, so `claude` never leaves you worse off than
//! running it raw (and the folder gets trusted for next time, when the tracked
//! path then succeeds). Everything else passes through to the real binary:
//! - inside zmx / `CLAUDE_NO_ZMX` set → passthrough (never nest)
//! - management subcommands (`config`, `login`, `mcp`, ...) → passthrough
//! - `--version`/`-h`/`--help` anywhere → passthrough, no flag injection
//! - headless (`-p`/`--print`) or stdout-not-a-TTY → passthrough
//! - escape hatch: `command claude ...`
//!
//! `QD_CLAUDE_WRAPPER_FLAGS` (whitespace-split) is injected on passthrough REAL
//! launches only (headless / non-TTY / inside-zmx) — never on management
//! subcommands or help/version. qd-routed launches do NOT need it: the engine's
//! launcher already applies `QD_CLAUDE_FLAGS` / config / built-in defaults
//! (launch.rs). The two seams are deliberately separate: `QD_CLAUDE_FLAGS`
//! would also override the engine launcher's defaults, which is not what a
//! wrapper-only flag preference means.
//!
//! ## The codex wrapper (same shape, one deliberate difference)
//!
//! codex now has a mux-pane lane of its own (`qd start <name> --provider codex
//! --interactive`, verbs/lifecycle.rs), so `codex` gets the same treatment:
//! bare interactive launch outside zmx → tracked, attachable session; every
//! other shape → the real binary; `command codex ...` is the escape hatch;
//! `CODEX_NO_ZMX` disables routing; `QD_CODEX_WRAPPER_FLAGS` rides passthrough
//! REAL launches only.
//!
//! THE DIFFERENCE: the claude wrapper forwards `"$@"` through `qd start ... --
//! "$@"`, and the codex wrapper forwards NOTHING — an invocation carrying ANY
//! argument passes through to the real codex instead of being routed. That is
//! not timidity, it is the engine's contract: the interactive codex lane builds
//! its argv as a bare `codex` (`create_codex_tui` passes `claude_args: vec![]`)
//! and `qd start`'s `-p` is explicitly refused there, so routing an
//! argument-carrying invocation would silently drop what the user typed —
//! including `codex "fix the parser"`, whose whole content is the argument.
//! Passing it through loses only the session tracking, and says so by doing the
//! obvious thing; routing it would lose the prompt. When the engine's codex lane
//! learns to accept a launch argv, this is the one line to revisit.
//!
//! Library-first: everything here is PURE (string emission + path math); the
//! bin verb wires the real env/zmx-dir.

use std::path::{Path, PathBuf};

use crate::effects::Env;

/// Shells `qd init` can emit integration for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// Canonical lowercase name (the `qd init <shell>` argument).
    pub fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
        }
    }

    /// Parse a shell name from an `qd init` argument or a `$SHELL` value.
    /// Accepts a bare name (`bash`), an absolute path (`/bin/zsh`), and the
    /// login-shell dash prefix (`-bash`).
    pub fn from_name(raw: &str) -> Option<Shell> {
        let base = Path::new(raw)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(raw);
        match base.trim_start_matches('-') {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            _ => None,
        }
    }
}

/// The marker comment of the RETIRED TS-era baked wrapper block. Bootstrap
/// detects it and tells the user to remove it (the init line supersedes it; a
/// live function defined AFTER the eval line would shadow the shipped wrapper).
pub const LEGACY_BLOCK_MARKER: &str = ">>> qd bootstrap >>>";

/// The one stable line the user's rc file carries, per shell.
pub fn init_line(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => r#"eval "$(qd init bash)""#,
        Shell::Zsh => r#"eval "$(qd init zsh)""#,
        Shell::Fish => "qd init fish | source",
    }
}

/// Does this rc-file content already carry the init line for `shell`?
/// Substring match on `qd init <shell>` so wrapped/guarded variants (e.g.
/// `command -v qd >/dev/null && eval "$(qd init bash)"`) still count.
pub fn rc_has_init_line(contents: &str, shell: Shell) -> bool {
    let needle = format!("qd init {}", shell.name());
    contents
        .lines()
        .any(|l| l.contains(&needle) && !l.trim_start().starts_with('#'))
}

/// Does this rc-file content carry the retired TS-era baked wrapper block?
pub fn rc_has_legacy_block(contents: &str) -> bool {
    contents.contains(LEGACY_BLOCK_MARKER)
}

/// The rc file the init line belongs in, per shell. zsh honors `ZDOTDIR`
/// (read through the injected env seam); fish uses a dedicated conf.d file
/// (auto-sourced, no append-to-shared-rc needed).
pub fn rc_path(shell: Shell, home: &Path, env: &dyn Env) -> PathBuf {
    match shell {
        Shell::Bash => home.join(".bashrc"),
        Shell::Zsh => {
            let zdot = env
                .var("ZDOTDIR")
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.to_path_buf());
            zdot.join(".zshrc")
        }
        Shell::Fish => home
            .join(".config")
            .join("fish")
            .join("conf.d")
            .join("qd.fish"),
    }
}

/// Emit the full shell-integration script for `shell`. `zmx_dir` is the
/// engine-resolved zmx socket dir, baked as the DEFAULT (a pre-set `ZMX_DIR`
/// always wins — the export only fills the unset case).
pub fn init_script(shell: Shell, zmx_dir: &str) -> String {
    match shell {
        Shell::Bash => posix_script(
            zmx_dir,
            "$QD_CLAUDE_WRAPPER_FLAGS",
            "$QD_CODEX_WRAPPER_FLAGS",
            "bash",
        ),
        // zsh does not word-split unquoted parameters; `${=VAR}` forces the
        // sh-style split the flag list needs.
        Shell::Zsh => posix_script(
            zmx_dir,
            "${=QD_CLAUDE_WRAPPER_FLAGS}",
            "${=QD_CODEX_WRAPPER_FLAGS}",
            "zsh",
        ),
        Shell::Fish => fish_script(zmx_dir),
    }
}

/// The bash/zsh emission. `wflags` / `cxflags` are the (shell-specific)
/// word-splitting expansions of `QD_CLAUDE_WRAPPER_FLAGS` /
/// `QD_CODEX_WRAPPER_FLAGS`.
fn posix_script(zmx_dir: &str, wflags: &str, cxflags: &str, shell_name: &str) -> String {
    format!(
        r#"# qd shell integration — emitted by `qd init {shell_name}`; do not edit.
# Pin the zmx socket dir so every shell + qd agree on one control socket
# (a pre-set ZMX_DIR wins; this only fills the unset case).
export ZMX_DIR="${{ZMX_DIR:-{zmx_dir}}}"

# claude wrapper: passthrough by default; only a bare interactive launch
# OUTSIDE zmx routes through 'qd start'. Escape hatch: 'command claude ...'.
# QD_CLAUDE_WRAPPER_FLAGS (whitespace-split) is injected on passthrough REAL
# launches only — never on management subcommands or --version/--help.
claude() {{
  local _qd_arg _qd_name
  # Already inside zmx, or zmx explicitly disabled → never nest, passthrough.
  if [ -n "$ZMX_SESSION" ] || [ -n "$CLAUDE_NO_ZMX" ]; then
    command claude {wflags} "$@"; return
  fi
  # Management subcommands and help/version pass straight through, unflagged.
  case "${{1:-}}" in
    logout|login|config|mcp|plugin|doctor|update|install|--version|-h|--help)
      command claude "$@"; return ;;
  esac
  for _qd_arg in "$@"; do
    case "$_qd_arg" in
      --version|-h|--help) command claude "$@"; return ;;
      -p|--print) command claude {wflags} "$@"; return ;;
    esac
  done
  # stdout is not a TTY → passthrough.
  if [ ! -t 1 ]; then
    command claude {wflags} "$@"; return
  fi
  # Remaining case: a bare interactive launch outside zmx → tracked session.
  # Create detached, then attach (--attach on `qd start` is engine-deferred;
  # start-then-attach is the supported path). If create fails — e.g. a first-run
  # folder-trust dialog blocks the boot-to-idle wait — fall back to launching
  # claude directly so `claude` is never worse than running it raw.
  _qd_name="cc-$(date +%Y%m%d-%H%M%S)-$$"
  if qd start "$_qd_name" -- "$@"; then
    qd attach "$_qd_name"
  else
    command claude {wflags} "$@"
  fi
}}

# codex wrapper: the same shape as claude's — only a bare interactive launch
# OUTSIDE zmx routes through 'qd start --provider codex --interactive'.
# Escape hatch: 'command codex ...'. QD_CODEX_WRAPPER_FLAGS (whitespace-split)
# is injected on passthrough REAL launches only.
codex() {{
  local _qd_arg _qd_name
  # Already inside zmx, or zmx explicitly disabled → never nest, passthrough.
  if [ -n "$ZMX_SESSION" ] || [ -n "$CODEX_NO_ZMX" ]; then
    command codex {cxflags} "$@"; return
  fi
  # Management subcommands and help/version pass straight through, unflagged.
  # `exec`/`review`/`resume`/`fork` are deliberately ABSENT: they are real runs,
  # so they take the wrapper flags on the passthrough below.
  case "${{1:-}}" in
    login|logout|mcp|mcp-server|app-server|remote-control|app|plugin|completion|update|doctor|sandbox|debug|apply|archive|unarchive|delete|features|help|--version|-V|-h|--help)
      command codex "$@"; return ;;
  esac
  for _qd_arg in "$@"; do
    case "$_qd_arg" in
      --version|-V|-h|--help) command codex "$@"; return ;;
    esac
  done
  # ANY remaining argument → the real codex. The engine's interactive codex lane
  # launches a BARE `codex` (it accepts no passthrough argv and refuses -p), so
  # routing an argument-carrying invocation — `codex exec ...`, `codex resume`,
  # or a bare prompt like `codex "fix the parser"` — would silently drop it.
  # Passing through costs only the session tracking; routing would cost the args.
  if [ "$#" -gt 0 ]; then
    command codex {cxflags} "$@"; return
  fi
  # stdout is not a TTY → passthrough.
  if [ ! -t 1 ]; then
    command codex {cxflags}; return
  fi
  # Remaining case: a bare interactive launch outside zmx → tracked session.
  # Create detached, then attach; on failure fall back to a direct launch so
  # `codex` is never worse than running it raw.
  _qd_name="cx-$(date +%Y%m%d-%H%M%S)-$$"
  if qd start "$_qd_name" --provider codex --interactive; then
    qd attach "$_qd_name"
  else
    command codex {cxflags}
  fi
}}
"#
    )
}

fn fish_script(zmx_dir: &str) -> String {
    format!(
        r#"# qd shell integration — emitted by `qd init fish`; do not edit.
# Pin the zmx socket dir (a pre-set ZMX_DIR wins; this fills the unset case).
if not set -q ZMX_DIR
    set -gx ZMX_DIR {zmx_dir}
end

# claude wrapper: passthrough by default; only a bare interactive launch
# OUTSIDE zmx routes through 'qd start'. Escape hatch: 'command claude ...'.
function claude
    set -l _qd_wflags (string split -n ' ' -- "$QD_CLAUDE_WRAPPER_FLAGS")
    if test -n "$ZMX_SESSION"; or test -n "$CLAUDE_NO_ZMX"
        command claude $_qd_wflags $argv
        return
    end
    if test (count $argv) -gt 0
        switch $argv[1]
            case logout login config mcp plugin doctor update install '--version' '-h' '--help'
                command claude $argv
                return
        end
    end
    for _qd_arg in $argv
        switch $_qd_arg
            case '--version' '-h' '--help'
                command claude $argv
                return
            case '-p' '--print'
                command claude $_qd_wflags $argv
                return
        end
    end
    if not isatty stdout
        command claude $_qd_wflags $argv
        return
    end
    # Create detached, then attach (--attach on `qd start` is engine-deferred;
    # start-then-attach is the supported path). If create fails — e.g. a first-run
    # folder-trust dialog blocks the boot-to-idle wait — fall back to launching
    # claude directly so `claude` is never worse than running it raw.
    set -l _qd_name cc-(date +%Y%m%d-%H%M%S)-$fish_pid
    if qd start $_qd_name -- $argv
        qd attach $_qd_name
    else
        command claude $_qd_wflags $argv
    end
end

# codex wrapper: the same shape as claude's — only a bare interactive launch
# OUTSIDE zmx routes through 'qd start --provider codex --interactive'.
# Escape hatch: 'command codex ...'.
function codex
    set -l _qd_wflags (string split -n ' ' -- "$QD_CODEX_WRAPPER_FLAGS")
    if test -n "$ZMX_SESSION"; or test -n "$CODEX_NO_ZMX"
        command codex $_qd_wflags $argv
        return
    end
    if test (count $argv) -gt 0
        # Management subcommands and help/version pass through unflagged
        # (`exec`/`review`/`resume`/`fork` are real runs — flagged, below).
        switch $argv[1]
            case login logout mcp mcp-server app-server remote-control app plugin completion update doctor sandbox debug apply archive unarchive delete features help '--version' '-V' '-h' '--help'
                command codex $argv
                return
        end
        for _qd_arg in $argv
            switch $_qd_arg
                case '--version' '-V' '-h' '--help'
                    command codex $argv
                    return
            end
        end
        # ANY remaining argument → the real codex. The engine's interactive codex
        # lane launches a BARE `codex` (no passthrough argv, and -p is refused),
        # so routing an argument-carrying invocation would silently drop it.
        command codex $_qd_wflags $argv
        return
    end
    if not isatty stdout
        command codex $_qd_wflags
        return
    end
    # Create detached, then attach; on failure fall back to a direct launch so
    # `codex` is never worse than running it raw.
    set -l _qd_name cx-(date +%Y%m%d-%H%M%S)-$fish_pid
    if qd start $_qd_name --provider codex --interactive
        qd attach $_qd_name
    else
        command codex $_qd_wflags
    end
end
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use std::collections::HashMap;

    fn map_env(pairs: &[(&str, &str)]) -> MapEnv {
        let mut vars = HashMap::new();
        for (k, v) in pairs {
            vars.insert(k.to_string(), v.to_string());
        }
        MapEnv { vars, uid: 501 }
    }

    // --- Shell::from_name -------------------------------------------------

    #[test]
    fn from_name_accepts_bare_path_and_login_forms() {
        assert_eq!(Shell::from_name("bash"), Some(Shell::Bash));
        assert_eq!(Shell::from_name("/bin/zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::from_name("/usr/local/bin/fish"), Some(Shell::Fish));
        // login-shell convention: argv[0] prefixed with '-'.
        assert_eq!(Shell::from_name("-bash"), Some(Shell::Bash));
        assert_eq!(Shell::from_name("tcsh"), None);
        assert_eq!(Shell::from_name(""), None);
    }

    // --- init_line / rc detection ------------------------------------------

    #[test]
    fn rc_detects_init_line_including_guarded_variants() {
        let s = Shell::Bash;
        assert!(rc_has_init_line(init_line(s), s));
        assert!(rc_has_init_line(
            "command -v qd >/dev/null && eval \"$(qd init bash)\"",
            s
        ));
        // Wrong shell's line does not count.
        assert!(!rc_has_init_line(init_line(Shell::Zsh), s));
        // A commented-out line does not count.
        assert!(!rc_has_init_line("# eval \"$(qd init bash)\"", s));
        assert!(!rc_has_init_line("", s));
    }

    #[test]
    fn rc_detects_legacy_block() {
        assert!(rc_has_legacy_block("# >>> qd bootstrap >>>\nclaude() {\n}"));
        assert!(!rc_has_legacy_block("eval \"$(qd init bash)\""));
    }

    // --- rc_path ------------------------------------------------------------

    #[test]
    fn rc_paths_per_shell() {
        let home = Path::new("/jail/home");
        let env = map_env(&[]);
        assert_eq!(
            rc_path(Shell::Bash, home, &env),
            PathBuf::from("/jail/home/.bashrc")
        );
        assert_eq!(
            rc_path(Shell::Zsh, home, &env),
            PathBuf::from("/jail/home/.zshrc")
        );
        assert_eq!(
            rc_path(Shell::Fish, home, &env),
            PathBuf::from("/jail/home/.config/fish/conf.d/qd.fish")
        );
    }

    #[test]
    fn zsh_rc_honors_zdotdir() {
        let home = Path::new("/jail/home");
        let env = map_env(&[("ZDOTDIR", "/jail/zdot")]);
        assert_eq!(
            rc_path(Shell::Zsh, home, &env),
            PathBuf::from("/jail/zdot/.zshrc")
        );
        // Blank ZDOTDIR counts as unset.
        let env = map_env(&[("ZDOTDIR", "  ")]);
        assert_eq!(
            rc_path(Shell::Zsh, home, &env),
            PathBuf::from("/jail/home/.zshrc")
        );
    }

    // --- emission invariants -------------------------------------------------
    //
    // The wrapper CONTRACT, asserted per shell:
    //   1. routes a bare launch through `qd start <generated-name>` then
    //      `qd attach` (named-detached-then-attach — `qd start --attach` is an
    //      A5-deferred surface the backend honestly rejects),
    //   1b. falls back to a direct `command claude` when `qd start` fails (so a
    //      first-run trust dialog can never leave `claude` worse than raw),
    //   2. passthrough for management subcommands,
    //   3. headless (-p/--print) passthrough WITH wrapper flags,
    //   4. --version/--help passthrough WITHOUT wrapper flags,
    //   5. bakes the zmx dir as an overridable default.
    //
    // The codex wrapper's contract is the same, minus argv forwarding: it routes
    // ONLY the zero-argument launch (`qd start --provider codex --interactive`
    // takes no passthrough argv, so anything else must reach the real binary).

    #[test]
    fn bash_script_invariants() {
        let s = init_script(Shell::Bash, "/run/user/501");
        assert!(
            s.contains(r#"if qd start "$_qd_name" -- "$@"; then"#)
                && s.contains(r#"qd attach "$_qd_name""#),
            "route: {s}"
        );
        // Create-fail fallback to a direct claude launch (with wrapper flags).
        assert!(
            s.contains(
                r#"else
    command claude $QD_CLAUDE_WRAPPER_FLAGS "$@""#
            ),
            "fallback: {s}"
        );
        assert!(
            !s.contains("qd start --attach"),
            "must NOT invoke the A5-deferred `qd start --attach`: {s}"
        );
        assert!(s.contains("logout|login|config|mcp|plugin|doctor|update|install"));
        assert!(s.contains("-p|--print) command claude $QD_CLAUDE_WRAPPER_FLAGS"));
        assert!(s.contains(r#"--version|-h|--help) command claude "$@""#));
        assert!(s.contains(r#"export ZMX_DIR="${ZMX_DIR:-/run/user/501}""#));
        assert!(s.contains("CLAUDE_NO_ZMX"));
    }

    #[test]
    fn bash_codex_wrapper_invariants() {
        let s = init_script(Shell::Bash, "/run/user/501");
        assert!(s.contains("codex() {"), "codex wrapper missing: {s}");
        // Routes through the interactive codex lane, then attaches.
        assert!(
            s.contains(r#"if qd start "$_qd_name" --provider codex --interactive; then"#),
            "route: {s}"
        );
        // ...and forwards NO argv (the lane accepts none — see the module docs).
        assert!(
            !s.contains(r#"--provider codex --interactive -- "$@""#),
            "codex lane takes no passthrough argv: {s}"
        );
        // Any argument at all reaches the real binary instead of being dropped.
        assert!(
            s.contains(
                r#"if [ "$#" -gt 0 ]; then
    command codex $QD_CODEX_WRAPPER_FLAGS "$@""#
            ),
            "argument passthrough: {s}"
        );
        // Create-fail fallback to a direct (argument-free) codex launch.
        assert!(
            s.contains(
                r#"else
    command codex $QD_CODEX_WRAPPER_FLAGS
  fi"#
            ),
            "fallback: {s}"
        );
        assert!(s.contains("login|logout|mcp|mcp-server|app-server"));
        assert!(s.contains(r#"--version|-V|-h|--help) command codex "$@""#));
        assert!(s.contains("CODEX_NO_ZMX"));
        // `exec`/`review`/`resume`/`fork` are REAL runs — they must not sit in
        // the unflagged management arm.
        assert!(!s.contains("|exec|"), "exec must take wrapper flags: {s}");
        assert!(
            !s.contains("|resume|"),
            "resume must take wrapper flags: {s}"
        );
    }

    #[test]
    fn zsh_script_uses_forced_word_split() {
        let s = init_script(Shell::Zsh, "/run/user/501");
        // zsh must use ${=VAR} (no implicit word splitting in zsh).
        assert!(s.contains("${=QD_CLAUDE_WRAPPER_FLAGS}"), "{s}");
        assert!(!s.contains(" $QD_CLAUDE_WRAPPER_FLAGS "), "{s}");
        assert!(s.contains("${=QD_CODEX_WRAPPER_FLAGS}"), "{s}");
        assert!(!s.contains(" $QD_CODEX_WRAPPER_FLAGS "), "{s}");
        assert!(s.contains("qd attach"));
        assert!(s.contains("if qd start"));
        assert!(!s.contains("qd start --attach"));
        assert!(s.contains("codex() {"));
        assert!(s.contains("--provider codex --interactive"));
    }

    #[test]
    fn fish_script_invariants() {
        let s = init_script(Shell::Fish, "/run/user/501");
        assert!(s.contains("function claude"));
        assert!(s.contains("if qd start $_qd_name -- $argv"));
        assert!(s.contains("qd attach $_qd_name"));
        // Create-fail fallback to a direct claude launch.
        assert!(s.contains("command claude $_qd_wflags $argv"));
        assert!(!s.contains("qd start --attach"));
        assert!(s.contains("case logout login config mcp plugin doctor update install"));
        assert!(s.contains("set -gx ZMX_DIR /run/user/501"));
        assert!(s.contains("isatty stdout"));
        // fish list-splits the wrapper flags explicitly.
        assert!(s.contains("string split"));
    }

    #[test]
    fn fish_codex_wrapper_invariants() {
        let s = init_script(Shell::Fish, "/run/user/501");
        assert!(s.contains("function codex"));
        assert!(s.contains("if qd start $_qd_name --provider codex --interactive"));
        // No argv forwarding into the codex lane; args reach the real binary.
        assert!(
            !s.contains("--provider codex --interactive -- $argv"),
            "{s}"
        );
        assert!(s.contains("command codex $_qd_wflags $argv"), "{s}");
        assert!(s.contains("case login logout mcp mcp-server app-server"));
        assert!(s.contains("CODEX_NO_ZMX"));
        assert!(s.contains("string split -n ' ' -- \"$QD_CODEX_WRAPPER_FLAGS\""));
    }

    #[test]
    fn bash_script_is_parseable_by_bash() {
        // Syntax-check the emission with a real bash if one is on PATH (skip
        // silently otherwise — unit tests must not hard-require a shell).
        let script = init_script(Shell::Bash, "/run/user/501");
        let out = std::process::Command::new("bash")
            .arg("-n")
            .arg("-c")
            .arg(&script)
            .output();
        if let Ok(out) = out {
            assert!(
                out.status.success(),
                "bash -n rejected the emission:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn zsh_script_is_parseable_by_zsh() {
        // The zsh emission diverges from bash's (${=VAR}), so it needs its own
        // syntax check. Same skip-if-absent posture as the bash one.
        let script = init_script(Shell::Zsh, "/run/user/501");
        let out = std::process::Command::new("zsh")
            .arg("-n")
            .arg("-c")
            .arg(&script)
            .output();
        if let Ok(out) = out {
            assert!(
                out.status.success(),
                "zsh -n rejected the emission:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
