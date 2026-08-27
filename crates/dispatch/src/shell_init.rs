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
//! Every wrapper names its harness on the `qd start` it routes to
//! (`--provider claude-code` / `codex` / `pi` / `opencode`) and pins the lane
//! `qd attach` can open. `claude`'s provider id is NOT its command name, which
//! is exactly why the argument is spelled out rather than left to the engine's
//! default.
//!
//! ## The codex, pi and opencode wrappers (same shape, one deliberate difference)
//!
//! Each of the other three harnesses has an attachable lane of its own —
//! `codex/mux-pane` (`--interactive`), `pi/extension` (`--extension`) and
//! `opencode/acp` (`--acp`, whose `qd attach` opens a real opencode TUI as a
//! second client on the bridge's server) — so `codex`, `pi` and `opencode` get
//! the same treatment as `claude`: bare interactive launch outside zmx →
//! tracked, attachable session; every other shape → the real binary;
//! `command <prog> ...` is the escape hatch; `<PROG>_NO_ZMX` disables routing;
//! `QD_<PROG>_WRAPPER_FLAGS` rides passthrough REAL launches only.
//!
//! THE DIFFERENCE: the claude wrapper forwards `"$@"` through `qd start ... --
//! "$@"`, and the other three forward NOTHING — an invocation carrying ANY
//! argument passes through to the real binary instead of being routed. That is
//! not timidity, it is the engine's contract: `qd start` populates the launch
//! passthrough for the CLAUDE PANE LANE ALONE (`verbs/lifecycle.rs`:
//! `passthrough: if claude_pane { claude_args } else { Vec::new() }`), so
//! routing an argument-carrying invocation would silently drop what the user
//! typed — including `codex exec ...`, `pi "fix the parser"` and
//! `opencode run ...`, whose whole content is the argument. Passing it through
//! loses only the session tracking, and says so by doing the obvious thing;
//! routing it would lose the prompt. When one of those lanes learns to accept a
//! launch argv, its entry in `PASSTHROUGH_WRAPPERS` is the one place to revisit.
//!
//! Library-first: everything here is PURE (string emission + path math); the
//! bin verb wires the real env/zmx-dir.

use std::path::{Path, PathBuf};

use quorum_qw::lane::Harness;

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
        Shell::Bash | Shell::Zsh => posix_script(shell, zmx_dir),
        Shell::Fish => fish_script(zmx_dir),
    }
}

/// The shell-specific word-splitting expansion of a wrapper-flags variable.
///
/// bash splits an unquoted parameter; zsh does NOT, and `${=VAR}` is the
/// spelling that forces the sh-style split the flag list needs. One function
/// because four wrappers now ask the same question, and a per-wrapper literal
/// is a per-wrapper chance to forget the zsh form.
fn flags_expansion(shell: Shell, var: &str) -> String {
    match shell {
        Shell::Zsh => format!("${{={var}}}"),
        _ => format!("${var}"),
    }
}

/// A wrapper that routes ONLY the zero-argument launch — codex, pi, opencode.
///
/// # Why these three share one shape and claude does not
///
/// The divergence is the engine's, not a stylistic one. `qd start` forwards a
/// `-- <argv>` tail into the launch for the CLAUDE PANE LANE ALONE
/// (`verbs/lifecycle.rs`: `passthrough: if claude_pane { claude_args } else {
/// Vec::new() }`), so on every other lane an argument-carrying invocation would
/// be created with the argument SILENTLY DROPPED — including `codex "fix the
/// parser"` and `pi "fix the parser"`, whose whole content is the argument.
///
/// So each of these routes a BARE invocation and passes everything else to the
/// real binary. Passing through costs only the session tracking; routing would
/// cost what the user typed. When a lane learns to accept a launch argv, its
/// entry here is the one place to revisit.
struct PassthroughWrapper {
    /// The command being shadowed — also the shell function's name.
    program: &'static str,
    /// The harness this wrapper wraps. Held as the ENUM rather than the id
    /// string so the `--provider` argument is `Harness::provider_id`'s answer
    /// and can never drift from what `qd start` parses — the id is not always
    /// the command name (`claude` is `claude-code`), and the one place that
    /// mapping lives is `quorum_qw::lane`.
    provider: Harness,
    /// The topology flag that pins the ATTACHABLE lane, with its leading space
    /// (`" --interactive"`), or `""` to take the create default.
    ///
    /// Every one of these names the harness's default lane today, and each is
    /// passed anyway: the wrapper's requirement is a lane `qd attach` can open,
    /// not "whatever the default is", and `Harness::create_default_mode` is the
    /// one default in this codebase that is explicitly allowed to move.
    lane_flag: &'static str,
    /// Set to disable routing entirely (always passthrough).
    no_zmx_var: &'static str,
    /// Extra flags injected on passthrough REAL launches only.
    flags_var: &'static str,
    /// Prefix of the generated session name.
    name_prefix: &'static str,
    /// Subcommands that are NOT a run — they pass through UNFLAGGED.
    management: &'static [&'static str],
    /// The help/version flags, honoured anywhere in argv and always unflagged.
    help_flags: &'static [&'static str],
    /// Why some run-shaped subcommand is missing from `management`, as a
    /// comment body (no leading `#`, no indentation — the emitters add both).
    /// Empty for a program with no such subcommand.
    management_note: &'static str,
}

/// The three non-claude wrappers, in emission order.
const PASSTHROUGH_WRAPPERS: &[PassthroughWrapper] = &[
    PassthroughWrapper {
        program: "codex",
        provider: Harness::Codex,
        // codex's create default is `codex/app-server` — a headless resident
        // with a viewer. `--interactive` asks for the plain TUI in a pane,
        // which is what a human who typed `codex` was about to get.
        lane_flag: " --interactive",
        no_zmx_var: "CODEX_NO_ZMX",
        flags_var: "QD_CODEX_WRAPPER_FLAGS",
        name_prefix: "cx",
        management: &[
            "login",
            "logout",
            "mcp",
            "mcp-server",
            "app-server",
            "remote-control",
            "app",
            "plugin",
            "completion",
            "update",
            "doctor",
            "sandbox",
            "debug",
            "apply",
            "archive",
            "unarchive",
            "delete",
            "features",
            "help",
        ],
        help_flags: &["--version", "-V", "-h", "--help"],
        management_note:
            "`exec`/`review`/`resume`/`fork` are deliberately ABSENT: they are real runs,\n\
                          so they take the wrapper flags on the passthrough below.",
    },
    PassthroughWrapper {
        program: "pi",
        provider: Harness::Pi,
        // pi's create default IS `pi/extension`; naming it keeps the wrapper on
        // the lane whose pane carries the quorum control channel, so `qd send`
        // can drive the same session the human is typing into.
        lane_flag: " --extension",
        no_zmx_var: "PI_NO_ZMX",
        flags_var: "QD_PI_WRAPPER_FLAGS",
        name_prefix: "pi",
        management: &[
            "install",
            "remove",
            "uninstall",
            "update",
            "list",
            "config",
            "auth",
        ],
        // pi's version flag is lowercase `-v`, codex's is `-V`. They are
        // per-program facts, which is why this is a field.
        help_flags: &["--version", "-v", "-h", "--help"],
        // Every pi subcommand is management; its RUN shape is a bare
        // `pi [@files...] [messages...]`, caught by the argument rule below.
        management_note: "",
    },
    PassthroughWrapper {
        program: "opencode",
        provider: Harness::Opencode,
        // opencode has exactly one live lane and this is it, so `--acp` names
        // the truth rather than overriding anything. `qd attach` opens a real
        // opencode TUI on the bridge's own server as a second client.
        lane_flag: " --acp",
        no_zmx_var: "OPENCODE_NO_ZMX",
        flags_var: "QD_OPENCODE_WRAPPER_FLAGS",
        name_prefix: "oc",
        management: &[
            "completion",
            "acp",
            "mcp",
            "attach",
            "debug",
            "providers",
            "auth",
            "agent",
            "upgrade",
            "uninstall",
            "serve",
            "web",
            "models",
            "stats",
            "export",
            "import",
            "github",
            "pr",
            "session",
            "plugin",
            "plug",
            "db",
        ],
        help_flags: &["--version", "-v", "-h", "--help"],
        management_note:
            "`run` is deliberately ABSENT: it is a real run, so it takes the wrapper\n\
             flags on the passthrough below. `acp` IS here — it is the bridge qd itself\n\
             spawns, and routing it would ask qd to start a session through qd.",
    },
];

/// Emit `body` as a comment block at `indent`, one `# ` per line. Empty in,
/// empty out — a wrapper with nothing to explain emits no blank comment.
fn comment_block(body: &str, indent: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    body.lines()
        .map(|l| format!("{indent}# {l}\n"))
        .collect::<String>()
}

/// The bash/zsh emission.
fn posix_script(shell: Shell, zmx_dir: &str) -> String {
    let shell_name = shell.name();
    let wflags = flags_expansion(shell, "QD_CLAUDE_WRAPPER_FLAGS");
    // Asked of the lane crate rather than spelled here: `claude-code` is the
    // PROVIDER ID and `claude` is the command, and this file has no business
    // holding a second copy of that mapping.
    let cc = Harness::ClaudeCode.provider_id();
    let mut out = format!(
        r#"# qd shell integration — emitted by `qd init {shell_name}`; do not edit.
# Pin the zmx socket dir so every shell + qd agree on one control socket
# (a pre-set ZMX_DIR wins; this only fills the unset case).
export ZMX_DIR="${{ZMX_DIR:-{zmx_dir}}}"

# claude wrapper: passthrough by default; only a bare interactive launch
# OUTSIDE zmx routes through 'qd start --provider {cc}'. Escape hatch:
# 'command claude ...'. QD_CLAUDE_WRAPPER_FLAGS (whitespace-split) is injected
# on passthrough REAL launches only — never on management subcommands or
# --version/--help.
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
  #
  # `--provider {cc}` is claude's PROVIDER ID, not its command name, and
  # it is passed explicitly for the same reason the other three wrappers pass
  # theirs: the wrapper names the harness it wraps rather than relying on the
  # engine's default provider staying claude.
  _qd_name="cc-$(date +%Y%m%d-%H%M%S)-$$"
  if qd start "$_qd_name" --provider {cc} -- "$@"; then
    qd attach "$_qd_name"
  else
    command claude {wflags} "$@"
  fi
}}
"#
    );
    for w in PASSTHROUGH_WRAPPERS {
        out.push_str(&posix_wrapper(shell, w));
    }
    out
}

/// One bash/zsh passthrough wrapper. See [`PassthroughWrapper`] for why these
/// three share a body and claude does not.
fn posix_wrapper(shell: Shell, w: &PassthroughWrapper) -> String {
    let PassthroughWrapper {
        program,
        provider,
        lane_flag,
        no_zmx_var,
        flags_var,
        name_prefix,
        management,
        help_flags,
        management_note,
    } = w;
    let provider = provider.provider_id();
    let flags = flags_expansion(shell, flags_var);
    // The first-argument arm catches management subcommands AND help/version;
    // the argv scan below catches help/version anywhere else.
    let first_arm = management
        .iter()
        .chain(help_flags.iter())
        .copied()
        .collect::<Vec<_>>()
        .join("|");
    let help_arm = help_flags.join("|");
    let note = comment_block(management_note, "  ");
    format!(
        r#"
# {program} wrapper: the same shape as claude's — only a bare interactive launch
# OUTSIDE zmx routes through 'qd start --provider {provider}{lane_flag}'.
# Escape hatch: 'command {program} ...'. {flags_var} (whitespace-split)
# is injected on passthrough REAL launches only.
{program}() {{
  local _qd_arg _qd_name
  # Already inside zmx, or zmx explicitly disabled → never nest, passthrough.
  if [ -n "$ZMX_SESSION" ] || [ -n "${no_zmx_var}" ]; then
    command {program} {flags} "$@"; return
  fi
  # Management subcommands and help/version pass straight through, unflagged.
{note}  case "${{1:-}}" in
    {first_arm})
      command {program} "$@"; return ;;
  esac
  for _qd_arg in "$@"; do
    case "$_qd_arg" in
      {help_arm}) command {program} "$@"; return ;;
    esac
  done
  # ANY remaining argument → the real {program}. The engine forwards a launch
  # argv on the claude pane lane ALONE, so routing an argument-carrying
  # invocation here would silently DROP what the user typed. Passing through
  # costs only the session tracking; routing would cost the args.
  if [ "$#" -gt 0 ]; then
    command {program} {flags} "$@"; return
  fi
  # stdout is not a TTY → passthrough.
  if [ ! -t 1 ]; then
    command {program} {flags}; return
  fi
  # Remaining case: a bare interactive launch outside zmx → tracked session.
  # Create detached, then attach; on failure fall back to a direct launch so
  # `{program}` is never worse than running it raw.
  _qd_name="{name_prefix}-$(date +%Y%m%d-%H%M%S)-$$"
  if qd start "$_qd_name" --provider {provider}{lane_flag}; then
    qd attach "$_qd_name"
  else
    command {program} {flags}
  fi
}}
"#
    )
}

fn fish_script(zmx_dir: &str) -> String {
    let cc = Harness::ClaudeCode.provider_id();
    let mut out = format!(
        r#"# qd shell integration — emitted by `qd init fish`; do not edit.
# Pin the zmx socket dir (a pre-set ZMX_DIR wins; this fills the unset case).
if not set -q ZMX_DIR
    set -gx ZMX_DIR {zmx_dir}
end

# claude wrapper: passthrough by default; only a bare interactive launch
# OUTSIDE zmx routes through 'qd start --provider {cc}'. Escape hatch:
# 'command claude ...'.
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
    #
    # `--provider {cc}` is claude's PROVIDER ID, not its command name, and
    # it is passed explicitly for the same reason the other three wrappers pass
    # theirs: the wrapper names the harness it wraps.
    set -l _qd_name cc-(date +%Y%m%d-%H%M%S)-$fish_pid
    if qd start $_qd_name --provider {cc} -- $argv
        qd attach $_qd_name
    else
        command claude $_qd_wflags $argv
    end
end
"#
    );
    for w in PASSTHROUGH_WRAPPERS {
        out.push_str(&fish_wrapper(w));
    }
    out
}

/// One fish passthrough wrapper — the same contract as [`posix_wrapper`], in
/// fish's syntax. Flag words are quoted `case` items; subcommands are bare.
fn fish_wrapper(w: &PassthroughWrapper) -> String {
    let PassthroughWrapper {
        program,
        provider,
        lane_flag,
        no_zmx_var,
        flags_var,
        name_prefix,
        management,
        help_flags,
        management_note,
    } = w;
    let provider = provider.provider_id();
    let quoted = |f: &&'static str| format!("'{f}'");
    let first_arm = management
        .iter()
        .map(|s| s.to_string())
        .chain(help_flags.iter().map(quoted))
        .collect::<Vec<_>>()
        .join(" ");
    let help_arm = help_flags.iter().map(quoted).collect::<Vec<_>>().join(" ");
    let note = comment_block(management_note, "        ");
    format!(
        r#"
# {program} wrapper: the same shape as claude's — only a bare interactive launch
# OUTSIDE zmx routes through 'qd start --provider {provider}{lane_flag}'.
# Escape hatch: 'command {program} ...'.
function {program}
    set -l _qd_wflags (string split -n ' ' -- "${flags_var}")
    if test -n "$ZMX_SESSION"; or test -n "${no_zmx_var}"
        command {program} $_qd_wflags $argv
        return
    end
    if test (count $argv) -gt 0
        # Management subcommands and help/version pass through unflagged.
{note}        switch $argv[1]
            case {first_arm}
                command {program} $argv
                return
        end
        for _qd_arg in $argv
            switch $_qd_arg
                case {help_arm}
                    command {program} $argv
                    return
            end
        end
        # ANY remaining argument → the real {program}. The engine forwards a
        # launch argv on the claude pane lane ALONE, so routing an
        # argument-carrying invocation would silently drop what the user typed.
        command {program} $_qd_wflags $argv
        return
    end
    if not isatty stdout
        command {program} $_qd_wflags
        return
    end
    # Create detached, then attach; on failure fall back to a direct launch so
    # `{program}` is never worse than running it raw.
    set -l _qd_name {name_prefix}-(date +%Y%m%d-%H%M%S)-$fish_pid
    if qd start $_qd_name --provider {provider}{lane_flag}
        qd attach $_qd_name
    else
        command {program} $_qd_wflags
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
    //   1. routes a bare launch through `qd start <generated-name> --provider
    //      <id>` then `qd attach` (named-detached-then-attach — `qd start
    //      --attach` is an A5-deferred surface the backend honestly rejects),
    //   1b. falls back to a direct `command claude` when `qd start` fails (so a
    //      first-run trust dialog can never leave `claude` worse than raw),
    //   2. passthrough for management subcommands,
    //   3. headless (-p/--print) passthrough WITH wrapper flags,
    //   4. --version/--help passthrough WITHOUT wrapper flags,
    //   5. bakes the zmx dir as an overridable default.
    //
    // The codex / pi / opencode wrappers' contract is the same, minus argv
    // forwarding: they route ONLY the zero-argument launch, because `qd start`
    // populates its launch passthrough on the claude pane lane alone.

    #[test]
    fn bash_script_invariants() {
        let s = init_script(Shell::Bash, "/run/user/501");
        assert!(
            s.contains(r#"if qd start "$_qd_name" --provider claude-code -- "$@"; then"#)
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

    /// The provider argument is the harness's REAL id, not the command name —
    /// the one place those diverge is `claude` → `claude-code`, and a wrapper
    /// that shipped `--provider claude` would fail every launch at parse.
    #[test]
    fn every_wrapper_names_its_providers_real_id() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let s = init_script(shell, "/run/user/501");
            for h in Harness::ALL {
                assert!(
                    s.contains(&format!("--provider {}", h.provider_id())),
                    "{shell:?} emits no wrapper for {}",
                    h.provider_id()
                );
            }
            // The one place the command name and the provider id diverge. A
            // wrapper that shipped `--provider claude` would fail every launch
            // at parse, and the emission is the only place that could do it.
            assert!(
                !s.contains("--provider claude "),
                "`claude` is the COMMAND; `claude-code` is the provider id: {s}"
            );
        }
        assert!(
            quorum_qw::lane::parse_provider_arg("claude").is_none(),
            "if a bare `claude` ever parses as a provider id, the wrapper comments \
             explaining why the emission spells `claude-code` are stale"
        );
    }

    /// Every wrapper routes to a lane `qd attach` can open a terminal on. A
    /// wrapper that created a lane with no terminal would start a session and
    /// then fail to show it — the one failure mode the fallback cannot catch,
    /// because `qd start` would have SUCCEEDED.
    #[test]
    fn every_wrapper_pins_an_attachable_lane() {
        use quorum_qw::lane::{CreateTopology, Lane};
        let claude = (Harness::ClaudeCode, CreateTopology::Default);
        let others = PASSTHROUGH_WRAPPERS.iter().map(|w| {
            let topology = match w.lane_flag.trim() {
                "--interactive" => CreateTopology::Interactive,
                "--extension" => CreateTopology::Extension,
                "--acp" => CreateTopology::Acp,
                other => panic!("{} pins an unrecognised lane flag {other:?}", w.program),
            };
            (w.provider, topology)
        });
        for (harness, topology) in std::iter::once(claude).chain(others) {
            let id = harness.provider_id();
            let lane = Lane::for_create(id, topology)
                .unwrap_or_else(|| panic!("the {id} wrapper's lane must exist"));
            assert!(
                lane.is_pane() || lane.has_viewer(),
                "the {id} wrapper routes to {lane}, which `qd attach` cannot open"
            );
        }
    }

    /// The three passthrough wrappers are generated from one body, so the
    /// per-harness FACTS are what a test can still get wrong: the flag words
    /// each program actually accepts, and the subcommands that are real runs.
    #[test]
    fn passthrough_wrapper_table_is_per_harness_truth() {
        let by = |p: &str| {
            PASSTHROUGH_WRAPPERS
                .iter()
                .find(|w| w.program == p)
                .unwrap_or_else(|| panic!("no {p} wrapper"))
        };
        // codex spells version `-V`; pi and opencode spell it `-v`. Getting
        // this wrong routes `pi -v` into a tracked session instead of printing
        // a version.
        assert!(by("codex").help_flags.contains(&"-V"));
        assert!(by("pi").help_flags.contains(&"-v"));
        assert!(by("opencode").help_flags.contains(&"-v"));
        // Real runs must NOT sit in the unflagged management arm.
        for (program, run_verb) in [("codex", "exec"), ("codex", "resume"), ("opencode", "run")] {
            assert!(
                !by(program).management.contains(&run_verb),
                "`{program} {run_verb}` is a real run — it must take the wrapper flags"
            );
        }
        // Every wrapper must be able to say "not this shell function" and
        // "not this harness's flags".
        for w in PASSTHROUGH_WRAPPERS {
            assert!(w.no_zmx_var.ends_with("_NO_ZMX"), "{}", w.no_zmx_var);
            assert!(
                w.flags_var.starts_with("QD_") && w.flags_var.ends_with("_WRAPPER_FLAGS"),
                "{}",
                w.flags_var
            );
        }
        // Distinct session-name prefixes: two wrappers sharing one would make
        // `qd ls` unreadable about which harness a row came from.
        let mut prefixes: Vec<&str> = PASSTHROUGH_WRAPPERS.iter().map(|w| w.name_prefix).collect();
        prefixes.push("cc"); // claude's, which is inline
        prefixes.sort_unstable();
        let n = prefixes.len();
        prefixes.dedup();
        assert_eq!(prefixes.len(), n, "session-name prefixes must be distinct");
    }

    #[test]
    fn bash_passthrough_wrapper_invariants() {
        let s = init_script(Shell::Bash, "/run/user/501");
        // Each of the three defines a function, routes through its own lane,
        // forwards NO argv, and falls back to an argument-free direct launch.
        for (program, route, flags_var, no_zmx) in [
            (
                "codex",
                "--provider codex --interactive",
                "QD_CODEX_WRAPPER_FLAGS",
                "CODEX_NO_ZMX",
            ),
            (
                "pi",
                "--provider pi --extension",
                "QD_PI_WRAPPER_FLAGS",
                "PI_NO_ZMX",
            ),
            (
                "opencode",
                "--provider opencode --acp",
                "QD_OPENCODE_WRAPPER_FLAGS",
                "OPENCODE_NO_ZMX",
            ),
        ] {
            assert!(
                s.contains(&format!("{program}() {{")),
                "{program} missing: {s}"
            );
            assert!(
                s.contains(&format!(r#"if qd start "$_qd_name" {route}; then"#)),
                "{program} route: {s}"
            );
            assert!(
                !s.contains(&format!(r#"{route} -- "$@""#)),
                "{program}'s lane takes no passthrough argv: {s}"
            );
            // Any argument at all reaches the real binary instead of being dropped.
            assert!(
                s.contains(&format!(
                    r#"if [ "$#" -gt 0 ]; then
    command {program} ${flags_var} "$@""#
                )),
                "{program} argument passthrough: {s}"
            );
            // Create-fail fallback to a direct (argument-free) launch.
            assert!(
                s.contains(&format!(
                    r#"else
    command {program} ${flags_var}
  fi"#
                )),
                "{program} fallback: {s}"
            );
            assert!(s.contains(no_zmx), "{program} escape hatch: {s}");
        }
        assert!(s.contains("login|logout|mcp|mcp-server|app-server"));
        assert!(s.contains(r#"--version|-V|-h|--help) command codex "$@""#));
        assert!(s.contains("install|remove|uninstall|update|list|config|auth"));
        assert!(s.contains(r#"--version|-v|-h|--help) command pi "$@""#));
        assert!(s.contains("completion|acp|mcp|attach|debug|providers|auth"));
        assert!(s.contains(r#"--version|-v|-h|--help) command opencode "$@""#));
        // codex: `exec`/`review`/`resume`/`fork` are REAL runs — they must not
        // sit in the unflagged management arm. Same for `opencode run`.
        assert!(!s.contains("|exec|"), "exec must take wrapper flags: {s}");
        assert!(
            !s.contains("|resume|"),
            "resume must take wrapper flags: {s}"
        );
        assert!(
            !s.contains("|run|"),
            "opencode run must take wrapper flags: {s}"
        );
    }

    #[test]
    fn zsh_script_uses_forced_word_split() {
        let s = init_script(Shell::Zsh, "/run/user/501");
        // zsh must use ${=VAR} (no implicit word splitting in zsh).
        for var in [
            "QD_CLAUDE_WRAPPER_FLAGS",
            "QD_CODEX_WRAPPER_FLAGS",
            "QD_PI_WRAPPER_FLAGS",
            "QD_OPENCODE_WRAPPER_FLAGS",
        ] {
            assert!(s.contains(&format!("${{={var}}}")), "{var}: {s}");
            assert!(!s.contains(&format!(" ${var} ")), "{var} unsplit: {s}");
        }
        assert!(s.contains("qd attach"));
        assert!(s.contains("if qd start"));
        assert!(!s.contains("qd start --attach"));
        assert!(s.contains("--provider claude-code"));
        assert!(s.contains("--provider codex --interactive"));
        assert!(s.contains("--provider pi --extension"));
        assert!(s.contains("--provider opencode --acp"));
    }

    #[test]
    fn fish_script_invariants() {
        let s = init_script(Shell::Fish, "/run/user/501");
        assert!(s.contains("function claude"));
        assert!(s.contains("if qd start $_qd_name --provider claude-code -- $argv"));
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
    fn fish_passthrough_wrapper_invariants() {
        let s = init_script(Shell::Fish, "/run/user/501");
        for (program, route, flags_var, no_zmx) in [
            (
                "codex",
                "--provider codex --interactive",
                "QD_CODEX_WRAPPER_FLAGS",
                "CODEX_NO_ZMX",
            ),
            (
                "pi",
                "--provider pi --extension",
                "QD_PI_WRAPPER_FLAGS",
                "PI_NO_ZMX",
            ),
            (
                "opencode",
                "--provider opencode --acp",
                "QD_OPENCODE_WRAPPER_FLAGS",
                "OPENCODE_NO_ZMX",
            ),
        ] {
            assert!(s.contains(&format!("function {program}")), "{program}: {s}");
            assert!(
                s.contains(&format!("if qd start $_qd_name {route}")),
                "{program} route: {s}"
            );
            // No argv forwarding into these lanes; args reach the real binary.
            assert!(
                !s.contains(&format!("{route} -- $argv")),
                "{program} forwards no argv: {s}"
            );
            assert!(
                s.contains(&format!("command {program} $_qd_wflags $argv")),
                "{program} argument passthrough: {s}"
            );
            assert!(
                s.contains(&format!("string split -n ' ' -- \"${flags_var}\"")),
                "{program} flag split: {s}"
            );
            assert!(s.contains(no_zmx), "{program} escape hatch: {s}");
        }
        assert!(s.contains("case login logout mcp mcp-server app-server"));
        assert!(s.contains("case install remove uninstall update list config auth"));
        assert!(s.contains("case completion acp mcp attach debug providers auth"));
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

    /// fish's syntax shares nothing with the posix pair, so the two `-n` checks
    /// above cover none of it. Same skip-if-absent posture.
    #[test]
    fn fish_script_is_parseable_by_fish() {
        let script = init_script(Shell::Fish, "/run/user/501");
        let dir = std::env::temp_dir().join(format!("qd-init-fish-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("qd.fish");
        if std::fs::write(&path, &script).is_err() {
            return;
        }
        let out = std::process::Command::new("fish")
            .arg("-n")
            .arg(&path)
            .output();
        if let Ok(out) = out {
            assert!(
                out.status.success(),
                "fish -n rejected the emission:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
