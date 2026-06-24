//! `sb init <shell>` — shell-integration emission (the eval-init pattern).
//!
//! ## Why emission instead of a baked rc block
//!
//! The retired TS bootstrap WROTE a `claude()` wrapper function into the user's
//! `~/.bashrc` between markers. That block was a fossil the moment it was
//! written: when the engine's `new` verb changed its argument contract, every
//! baked wrapper on every machine silently broke (observed live 2026-06-09:
//! `sb new -- "$@"` with no name → `error: missing required argument 'name'`).
//!
//! The eval-init pattern (starship / zoxide / direnv precedent) inverts the
//! ownership: the rc file carries ONE stable line —
//!
//! ```text
//! eval "$(sb init bash)"          # ~/.bashrc / ~/.zshrc (zsh variant)
//! sb init fish | source           # ~/.config/fish/conf.d/sb.fish
//! ```
//!
//! — and the wrapper BODY ships inside this binary, so it can never drift from
//! what the engine's verbs actually accept.
//!
//! ## What the wrapper does
//!
//! `claude` typed bare in an interactive terminal (outside zmx) creates a
//! tracked session detached (`sb start <generated-name>`) and connects to it
//! (`sb connect`). `sb start --attach` would be the one-shot form, but it is an
//! engine-deferred surface today, so start-then-connect is the supported path.
//! If `sb start` fails for any reason — most commonly a first-run folder-trust
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
//! `SB_CLAUDE_WRAPPER_FLAGS` (whitespace-split) is injected on passthrough REAL
//! launches only (headless / non-TTY / inside-zmx) — never on management
//! subcommands or help/version. sb-routed launches do NOT need it: the engine's
//! launcher already applies `SB_CLAUDE_FLAGS` / config / built-in defaults
//! (launch.rs). The two seams are deliberately separate: `SB_CLAUDE_FLAGS`
//! would also override the engine launcher's defaults, which is not what a
//! wrapper-only flag preference means.
//!
//! Library-first: everything here is PURE (string emission + path math); the
//! bin verb wires the real env/zmx-dir.

use std::path::{Path, PathBuf};

use crate::effects::Env;

/// Shells `sb init` can emit integration for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// Canonical lowercase name (the `sb init <shell>` argument).
    pub fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
        }
    }

    /// Parse a shell name from an `sb init` argument or a `$SHELL` value.
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
pub const LEGACY_BLOCK_MARKER: &str = ">>> sb bootstrap >>>";

/// The one stable line the user's rc file carries, per shell.
pub fn init_line(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => r#"eval "$(sb init bash)""#,
        Shell::Zsh => r#"eval "$(sb init zsh)""#,
        Shell::Fish => "sb init fish | source",
    }
}

/// Does this rc-file content already carry the init line for `shell`?
/// Substring match on `sb init <shell>` so wrapped/guarded variants (e.g.
/// `command -v sb >/dev/null && eval "$(sb init bash)"`) still count.
pub fn rc_has_init_line(contents: &str, shell: Shell) -> bool {
    let needle = format!("sb init {}", shell.name());
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
            .join("sb.fish"),
    }
}

/// Emit the full shell-integration script for `shell`. `zmx_dir` is the
/// engine-resolved zmx socket dir, baked as the DEFAULT (a pre-set `ZMX_DIR`
/// always wins — the export only fills the unset case).
pub fn init_script(shell: Shell, zmx_dir: &str) -> String {
    match shell {
        Shell::Bash => posix_script(zmx_dir, "$SB_CLAUDE_WRAPPER_FLAGS", "bash"),
        // zsh does not word-split unquoted parameters; `${=VAR}` forces the
        // sh-style split the flag list needs.
        Shell::Zsh => posix_script(zmx_dir, "${=SB_CLAUDE_WRAPPER_FLAGS}", "zsh"),
        Shell::Fish => fish_script(zmx_dir),
    }
}

/// The bash/zsh emission. `wflags` is the (shell-specific) word-splitting
/// expansion of `SB_CLAUDE_WRAPPER_FLAGS`.
fn posix_script(zmx_dir: &str, wflags: &str, shell_name: &str) -> String {
    format!(
        r#"# sb shell integration — emitted by `sb init {shell_name}`; do not edit.
# Pin the zmx socket dir so every shell + sb agree on one control socket
# (a pre-set ZMX_DIR wins; this only fills the unset case).
export ZMX_DIR="${{ZMX_DIR:-{zmx_dir}}}"

# claude wrapper: passthrough by default; only a bare interactive launch
# OUTSIDE zmx routes through 'sb start'. Escape hatch: 'command claude ...'.
# SB_CLAUDE_WRAPPER_FLAGS (whitespace-split) is injected on passthrough REAL
# launches only — never on management subcommands or --version/--help.
claude() {{
  local _sb_arg _sb_name
  # Already inside zmx, or zmx explicitly disabled → never nest, passthrough.
  if [ -n "$ZMX_SESSION" ] || [ -n "$CLAUDE_NO_ZMX" ]; then
    command claude {wflags} "$@"; return
  fi
  # Management subcommands and help/version pass straight through, unflagged.
  case "${{1:-}}" in
    logout|login|config|mcp|plugin|doctor|update|install|--version|-h|--help)
      command claude "$@"; return ;;
  esac
  for _sb_arg in "$@"; do
    case "$_sb_arg" in
      --version|-h|--help) command claude "$@"; return ;;
      -p|--print) command claude {wflags} "$@"; return ;;
    esac
  done
  # stdout is not a TTY → passthrough.
  if [ ! -t 1 ]; then
    command claude {wflags} "$@"; return
  fi
  # Remaining case: a bare interactive launch outside zmx → tracked session.
  # Create detached, then connect (--attach on `sb start` is engine-deferred;
  # start-then-connect is the supported path). If create fails — e.g. a first-run
  # folder-trust dialog blocks the boot-to-idle wait — fall back to launching
  # claude directly so `claude` is never worse than running it raw.
  _sb_name="cc-$(date +%Y%m%d-%H%M%S)-$$"
  if sb start "$_sb_name" -- "$@"; then
    sb connect "$_sb_name"
  else
    command claude {wflags} "$@"
  fi
}}
"#
    )
}

fn fish_script(zmx_dir: &str) -> String {
    format!(
        r#"# sb shell integration — emitted by `sb init fish`; do not edit.
# Pin the zmx socket dir (a pre-set ZMX_DIR wins; this fills the unset case).
if not set -q ZMX_DIR
    set -gx ZMX_DIR {zmx_dir}
end

# claude wrapper: passthrough by default; only a bare interactive launch
# OUTSIDE zmx routes through 'sb start'. Escape hatch: 'command claude ...'.
function claude
    set -l _sb_wflags (string split -n ' ' -- "$SB_CLAUDE_WRAPPER_FLAGS")
    if test -n "$ZMX_SESSION"; or test -n "$CLAUDE_NO_ZMX"
        command claude $_sb_wflags $argv
        return
    end
    if test (count $argv) -gt 0
        switch $argv[1]
            case logout login config mcp plugin doctor update install '--version' '-h' '--help'
                command claude $argv
                return
        end
    end
    for _sb_arg in $argv
        switch $_sb_arg
            case '--version' '-h' '--help'
                command claude $argv
                return
            case '-p' '--print'
                command claude $_sb_wflags $argv
                return
        end
    end
    if not isatty stdout
        command claude $_sb_wflags $argv
        return
    end
    # Create detached, then connect (--attach on `sb start` is engine-deferred;
    # start-then-connect is the supported path). If create fails — e.g. a first-run
    # folder-trust dialog blocks the boot-to-idle wait — fall back to launching
    # claude directly so `claude` is never worse than running it raw.
    set -l _sb_name cc-(date +%Y%m%d-%H%M%S)-$fish_pid
    if sb start $_sb_name -- $argv
        sb connect $_sb_name
    else
        command claude $_sb_wflags $argv
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
            "command -v sb >/dev/null && eval \"$(sb init bash)\"",
            s
        ));
        // Wrong shell's line does not count.
        assert!(!rc_has_init_line(init_line(Shell::Zsh), s));
        // A commented-out line does not count.
        assert!(!rc_has_init_line("# eval \"$(sb init bash)\"", s));
        assert!(!rc_has_init_line("", s));
    }

    #[test]
    fn rc_detects_legacy_block() {
        assert!(rc_has_legacy_block("# >>> sb bootstrap >>>\nclaude() {\n}"));
        assert!(!rc_has_legacy_block("eval \"$(sb init bash)\""));
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
            PathBuf::from("/jail/home/.config/fish/conf.d/sb.fish")
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
    //   1. routes a bare launch through `sb start <generated-name>` then
    //      `sb connect` (named-detached-then-connect — `sb start --attach` is an
    //      A5-deferred surface the backend honestly rejects),
    //   1b. falls back to a direct `command claude` when `sb start` fails (so a
    //      first-run trust dialog can never leave `claude` worse than raw),
    //   2. passthrough for management subcommands,
    //   3. headless (-p/--print) passthrough WITH wrapper flags,
    //   4. --version/--help passthrough WITHOUT wrapper flags,
    //   5. bakes the zmx dir as an overridable default.

    #[test]
    fn bash_script_invariants() {
        let s = init_script(Shell::Bash, "/run/user/501");
        assert!(
            s.contains(r#"if sb start "$_sb_name" -- "$@"; then"#)
                && s.contains(r#"sb connect "$_sb_name""#),
            "route: {s}"
        );
        // Create-fail fallback to a direct claude launch (with wrapper flags).
        assert!(
            s.contains(
                r#"else
    command claude $SB_CLAUDE_WRAPPER_FLAGS "$@""#
            ),
            "fallback: {s}"
        );
        assert!(
            !s.contains("sb start --attach"),
            "must NOT invoke the A5-deferred `sb start --attach`: {s}"
        );
        assert!(s.contains("logout|login|config|mcp|plugin|doctor|update|install"));
        assert!(s.contains("-p|--print) command claude $SB_CLAUDE_WRAPPER_FLAGS"));
        assert!(s.contains(r#"--version|-h|--help) command claude "$@""#));
        assert!(s.contains(r#"export ZMX_DIR="${ZMX_DIR:-/run/user/501}""#));
        assert!(s.contains("CLAUDE_NO_ZMX"));
    }

    #[test]
    fn zsh_script_uses_forced_word_split() {
        let s = init_script(Shell::Zsh, "/run/user/501");
        // zsh must use ${=VAR} (no implicit word splitting in zsh).
        assert!(s.contains("${=SB_CLAUDE_WRAPPER_FLAGS}"), "{s}");
        assert!(!s.contains(" $SB_CLAUDE_WRAPPER_FLAGS "), "{s}");
        assert!(s.contains("sb connect"));
        assert!(s.contains("if sb start"));
        assert!(!s.contains("sb start --attach"));
    }

    #[test]
    fn fish_script_invariants() {
        let s = init_script(Shell::Fish, "/run/user/501");
        assert!(s.contains("function claude"));
        assert!(s.contains("if sb start $_sb_name -- $argv"));
        assert!(s.contains("sb connect $_sb_name"));
        // Create-fail fallback to a direct claude launch.
        assert!(s.contains("command claude $_sb_wflags $argv"));
        assert!(!s.contains("sb start --attach"));
        assert!(s.contains("case logout login config mcp plugin doctor update install"));
        assert!(s.contains("set -gx ZMX_DIR /run/user/501"));
        assert!(s.contains("isatty stdout"));
        // fish list-splits the wrapper flags explicitly.
        assert!(s.contains("string split"));
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
}
