//! zmx capability preflight (spec §4 deliverable 5; LESSONS **L3** carrier).
//!
//! Ported from `src/utils.ts:240-346` with the war-story comments carried
//! forward (LESSONS rule 1, comments-carry; citing file:line).
//!
//! qd drives sessions by injecting raw keystrokes through the session PTY via
//! `zmx send <name> <text>` (boot-popup dismiss, prompt delivery, relay). zmx
//! gained `send` in 0.6; older zmx (e.g. 0.5.x, which exposes only run/write/
//! attach) silently no-ops every keystroke qd tries to type. The symptom is not
//! an error — it's a HANG: `qd new` boots a session, the auto-[enter] never
//! lands, the PID file never appears, and the boot loop times out "not found"
//! ~40s later. This guard converts that silent multi-machine footgun into an
//! immediate, actionable error. We key on the `send` SUBCOMMAND in `zmx --help`
//! rather than a version string (more robust to version-format drift) and only
//! hard-fail on a DEFINITE "present but lacks send" signal — a missing/unreadable
//! zmx falls through to startDetached's existing `zmx run` failure path.
//! (utils.ts:242-252)

use crate::exec::Exec;

/// zmx `send` support verdict (port of the `"yes"|"no"|"unknown"` union,
/// utils.ts:300). Crucially, only a RECOGNIZABLE zmx help listing can yield a
/// definite `No`; everything else is `Unknown` so callers fall through to the
/// real `zmx run` failure path rather than a false "too old".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Yes,
    No,
    Unknown,
}

/// PURE: does this `zmx --help` text advertise a `send` subcommand?
/// (Port of `parseZmxHasSend`, utils.ts:254-269.)
///
/// Match the `send` SUBCOMMAND at line-leading position, followed by an angle-
/// bracket arg placeholder — zmx's real format is `  [s]end <name> <text...>`.
/// Two guards keep DESCRIPTION PROSE from registering (the false-positive that
/// would silently reintroduce the boot hang): (1) the line anchor `(^|\n)[ \t]*`
/// requires `send` to begin a subcommand entry, not appear mid-sentence — so
/// run's "Send command without attaching" never matches; (2) the required
/// `<…>` placeholder rejects prose like a line-leading "send REQUEST to relay"
/// or a stray "[S]END" token. The `[s]` abbrev bracket and a leading bullet are
/// tolerated; case-insensitive. Every real zmx (0.5.x and 0.6.x) uses angle-
/// bracket placeholders, so this matches all real formats — exotic drift that
/// omits `<…>` or uses clap-style `s, send` aliases is NOT recognized and
/// degrades upstream to "unknown" (looks_like_zmx_help) rather than a false "yes".
///
/// TS regex (utils.ts:268): `/(^|\n)[ \t]*(?:[-*][ \t]+)?\[?s\]?end\b[ \t]*<\S/i`.
/// Hand-rolled below to avoid a regex-crate dependency; line-scanned for the
/// `(^|\n)` anchor, the rest matched per-line.
pub fn parse_zmx_has_send(help_text: &str) -> bool {
    // The `(^|\n)` anchor means "at the start of a line"; scan line by line.
    for line in help_text.split('\n') {
        if line_matches_send(line) {
            return true;
        }
    }
    false
}

/// Match one line against `[ \t]*(?:[-*][ \t]+)?\[?s\]?end\b[ \t]*<\S` (case-insensitive).
fn line_matches_send(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    // `[ \t]*`
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    // `(?:[-*][ \t]+)?` — an optional leading bullet followed by ≥1 space/tab.
    if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'*') {
        let mut j = i + 1;
        let mut saw_ws = false;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
            saw_ws = true;
        }
        if saw_ws {
            i = j;
        }
        // If no whitespace followed the bullet, the optional group simply didn't
        // match; leave `i` where it was (the bullet then can't begin `\[?s`).
    }
    // `\[?` — optional `[`
    if i < bytes.len() && bytes[i] == b'[' {
        i += 1;
    }
    // `s` (case-insensitive)
    if i >= bytes.len() || !bytes[i].eq_ignore_ascii_case(&b's') {
        return false;
    }
    i += 1;
    // `\]?` — optional `]`
    if i < bytes.len() && bytes[i] == b']' {
        i += 1;
    }
    // `end` (case-insensitive)
    if i + 3 > bytes.len()
        || !bytes[i].eq_ignore_ascii_case(&b'e')
        || !bytes[i + 1].eq_ignore_ascii_case(&b'n')
        || !bytes[i + 2].eq_ignore_ascii_case(&b'd')
    {
        return false;
    }
    i += 3;
    // `\b` — word boundary: the next char (if any) must NOT be a word char.
    if i < bytes.len() && is_word_byte(bytes[i]) {
        return false;
    }
    // `[ \t]*`
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    // `<` then `\S` (a non-whitespace char must follow the angle bracket).
    if i < bytes.len() && bytes[i] == b'<' {
        i += 1;
        if i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            return true;
        }
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// PURE: is this text recognizably a zmx `--help` listing (vs. a missing-binary
/// error, empty output, or garbage)? (Port of `looksLikeZmxHelp`, utils.ts:271-287.)
///
/// We only trust a send verdict — and in particular only conclude a DEFINITE "no"
/// — when the output actually looks like zmx's subcommand help. Otherwise a
/// `command not found` string (non-empty!) or an empty/garbage probe would be
/// misread as "stale" and wrongly block spawn. Anchored on stable zmx help
/// structure: a Commands:/Usage: section, or the run+attach subcommand pair that
/// every zmx version (0.5.x included) advertises.
pub fn looks_like_zmx_help(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    // TS: `/\bCommands:/i` / `/\bUsage:/i` — the LEADING `\b` matters: a help
    // text containing only "subcommands:" must not register (lead review fix;
    // exact-regex parity with utils.ts:282).
    if contains_ci_word_start(text, "commands:") || contains_ci_word_start(text, "usage:") {
        return true;
    }
    // No section headers — fall back to the universal run+attach subcommand pair.
    // TS: `/\[r\]un\b/i || /(^|\n)[ \t]*run\b/i` and the attach analogue. The
    // TRAILING `\b` on the bracketed form matters: "[r]unxyz" must not register
    // (lead review fix; utils.ts:284-285).
    let has_run = contains_ci_word_end(text, "[r]un") || line_leading_word(text, "run");
    let has_attach = contains_ci_word_end(text, "[a]ttach") || line_leading_word(text, "attach");
    has_run && has_attach
}

/// Case-insensitive substring with a LEADING word boundary (`\b<needle>`): the
/// char before the match (if any) must not be a word char. `needle` must be
/// lowercase ASCII.
fn contains_ci_word_start(haystack: &str, needle: &str) -> bool {
    let lower = haystack.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(needle) {
        let abs = from + pos;
        let prev_is_word = abs > 0 && is_word_byte(lower.as_bytes()[abs - 1]);
        if !prev_is_word {
            return true;
        }
        from = abs + 1;
    }
    false
}

/// Case-insensitive substring with a TRAILING word boundary (`<needle>\b`): the
/// char after the match (if any) must not be a word char. `needle` must be
/// lowercase ASCII.
fn contains_ci_word_end(haystack: &str, needle: &str) -> bool {
    let lower = haystack.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(needle) {
        let abs = from + pos;
        let end = abs + needle.len();
        let next_is_word = end < lower.len() && is_word_byte(lower.as_bytes()[end]);
        if !next_is_word {
            return true;
        }
        from = abs + 1;
    }
    false
}

/// `(^|\n)[ \t]*<word>\b` (case-insensitive): does any line start (after leading
/// blanks) with `word` followed by a word boundary?
fn line_leading_word(text: &str, word: &str) -> bool {
    for line in text.split('\n') {
        let trimmed = line.trim_start_matches([' ', '\t']);
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix(word) {
            // `\b`: next char (if any) is not a word char.
            if rest.bytes().next().map(is_word_byte) != Some(true) {
                return true;
            }
        }
    }
    false
}

/// Probe the installed zmx for `send` support (port of `zmxSendCapability`,
/// utils.ts:289-315).
///
///   `Yes`     — recognizable help, send present
///   `No`      — recognizable help, send absent (stale zmx — qd cannot drive it)
///   `Unknown` — zmx absent / --help unreadable / output not recognizably zmx help
///
/// The `Unknown` guard is load-bearing: only a recognizable zmx help listing can
/// yield a DEFINITE `No`. A missing binary, empty output, or garbage resolves to
/// `Unknown` so callers fall through to the real `zmx run` failure path instead
/// of hard-failing with a misleading "too old" message.
pub fn zmx_send_capability(exec: &impl Exec) -> Capability {
    // `zmx --help`, 5s timeout: a wedged zmx binary must not reintroduce the very
    // hang this guards against (utils.ts:306-307). On nonzero/--help-to-stderr,
    // salvage BOTH streams (utils.ts:311); timeout/ENOENT → empty → Unknown.
    let out = match exec.run("zmx", &["--help".to_string()], &[], None, Some(5000)) {
        Ok(r) => format!("{}{}", r.stdout, r.stderr),
        // ENOENT (zmx not on PATH) throws here in TS's execSync; in Rust a spawn
        // failure is an Err. Either way → empty → Unknown (never a false "no").
        Err(_) => String::new(),
    };
    if !looks_like_zmx_help(&out) {
        return Capability::Unknown;
    }
    if parse_zmx_has_send(&out) {
        Capability::Yes
    } else {
        Capability::No
    }
}

/// Human-facing guidance for upgrading a too-old zmx (port of
/// `zmxUpgradeGuidance`, utils.ts:317-325). Byte-matches the TS string.
pub fn zmx_upgrade_guidance() -> String {
    "zmx is too old: it has no `send` subcommand, which qd needs to drive\n\
     sessions (boot, prompt delivery, relay). Upgrade to zmx >= 0.6, then retry:\n  \
     brew upgrade neurosnap/tap/zmx   # or: brew install neurosnap/tap/zmx\n  \
     (no brew: see https://github.com/neurosnap/zmx)"
        .to_string()
}

/// Human-facing guidance for a zmx that isn't installed / not on PATH (port of
/// `zmxMissingGuidance`, utils.ts:327-334). Byte-matches the TS string.
pub fn zmx_missing_guidance() -> String {
    "Failed to launch zmx — is it installed and on PATH?\n  \
     Install: brew install neurosnap/tap/zmx\n  \
     (no brew: see https://github.com/neurosnap/zmx; qd needs zmx >= 0.6)"
        .to_string()
}

/// Fail fast BEFORE launching a session if the installed zmx can't be driven
/// (port of `assertZmxCapable`, utils.ts:336-346). Only blocks on the definite
/// stale signal (`No`); a missing zmx (`Unknown`) is left to the downstream
/// `zmx run` failure path so this never produces a false positive.
///
/// Returns `Err(guidance)` instead of TS's `process.exit(1)` so the caller owns
/// the exit (Rust separates the decider from the process-exit; the CLI maps this
/// to stderr + nonzero, M2). `Ok(())` means "proceed" (Yes OR Unknown).
pub fn assert_zmx_capable(exec: &impl Exec) -> Result<(), String> {
    if zmx_send_capability(exec) == Capability::No {
        return Err(zmx_upgrade_guidance());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ScriptedExec;

    // The REAL zmx 0.6.0 `--help` output, frozen as a fixture — FULL capture.
    // Provenance: `zmx --help` on brano against /opt/homebrew/bin/zmx (pin
    // 0.6.0, `zmx version` -> "zmx\t0.6.0"), re-captured 2026-06-05 at A2
    // pass-(b) closure (finding F-A2b-1): the original 2026-06-04 capture had
    // frozen only the first ~20 lines (header + Commands block) and encoded
    // ONE backslash on the detach line where live zmx emits TWO (`ctrl+\\`,
    // zmx's own help-string escaping). The zmx binary is UNCHANGED since
    // before the original capture (Cellar mtime 2026-05-16) — this is a
    // capture-quality fix, not version drift. Raw string: byte-exact with
    // live output incl. the single trailing newline. The `send` SUBCOMMAND
    // line is `  [s]end <name> <text...>` — the matched shape.
    const ZMX_060_HELP: &str = r#"zmx - session persistence for terminal processes

Usage: zmx <command> [args...]

Commands:
  [a]ttach <name> [command...]             Attach to session, creating if needed
  [r]un <name> [-d] [command...]           Send command without attaching
  [s]end <name> <text...>                  Send raw input to session PTY
  [p]rint <name> <text...>                 Inject text into session display
  [wr]ite <name> <file_path>               Write stdin to file_path through the session
  [d]etach                                 Detach all clients (ctrl+\\ for current client)
  [l]ist|ls [--short]                      List active sessions
  [k]ill <name>... [--force]               Kill session and all attached clients
  [hi]story <name> [--vt|--html]           Output session scrollback
  [w]ait <name>...                         Wait for session tasks to complete
  [t]ail <name>...                         Follow session output
  [c]ompletions <shell>                    Shell completions (bash, zsh, fish)
  [v]ersion                                Show version
  [h]elp                                   Show this help

Attach:
  This will spawn a login $SHELL with a PTY.  You can provide a
  command instead of creating a shell.

  Examples:
    zmx attach dev
    zmx attach dev vim

History:
  This should generally be used with `tail` to print the last lines
  of the session's scrollback history.

  Examples:
    zmx history <session> | tail -100

Run:
  Commands are passed as-is: do not wrap in quotes.
  Commands run sequentially: do not send multiple in parallel.
  Avoid interactive programs (pagers, editors, prompts): they hang.

  If the command hangs, send Ctrl+C to recover:
    zmx run <session> $(printf '\x03')

  If the command hangs, print the history to see the error:
    zmx history <session> | tail -100

  `-d` will detach from the calling terminal. Use `wait` to track
  its status.

  Examples:
    zmx run dev ls
    zmx run dev zig build
    zmx run dev grep -r TODO src
    zmx run dev git -c core.pager=cat diff

Send:
  Sends raw text to the session's PTY input (fire-and-forget).
  Unlike `run`, no completion marker is appended and no exit code
  is tracked.  Useful for TUI applications, interactive prompts,
  or any program that reads stdin directly.

  Text is sent byte-for-byte with no automatic carriage return.
  Append \r yourself when you want the shell to execute a command.

  Text can also be piped via stdin:
    printf 'ls -la\r' | zmx send dev

  Examples:
    printf 'echo hello\r' | zmx send dev
    zmx send dev $(printf '\x03')
    zmx send dev /compact

Print:
  Injects text directly into the session display and scrollback.
  Never touches the PTY input -- the shell sees nothing.
  Caller is responsible for newlines (\\r\\n).

  Examples:
    printf '\\r\\nhello\\r\\n' | zmx print dev
    zmx print dev "$(printf '\\r\\nalert\\r\\n')"

Write:
  Writes stdin to file_path inside the session. Works over SSH.
  file_path can be absolute or relative to the session shell's cwd.
  Requires base64 and printf in the remote environment.
  Large files are chunked automatically (~48KB per chunk).
  File path must not contain single quotes.

  Examples:
    echo "hello" | zmx write dev /tmp/hello.txt
    cat main.zig | zmx write dev src/main.zig

Wait:
  Used with a detached run task to track its status.  Multiple
  sessions can be provided.

  Examples:
    zmx run -d dev sleep 10
    zmx wait dev
    zmx wait dev other

Environment variables:
  SHELL                Default shell for new sessions
  ZMX_DIR              Socket directory (priority 1)
  XDG_RUNTIME_DIR      Socket directory (priority 2)
  TMPDIR               Socket directory (priority 3)
  ZMX_SESSION          Session name (injected automatically)
  ZMX_SESSION_PREFIX   Prefix added to all session names
  ZMX_DIR_MODE         Sets mode for socket and log directories (octal, defaults to 0750)
  ZMX_LOG_MODE         Sets mode for log files (octal, defaults to 0640)
"#;

    // A 0.5.x-SHAPED help: run/attach/write present, but NO `send` subcommand.
    // (Synthetic but structurally faithful: the very version this preflight
    // exists to reject. Note it DOES contain run's prose "Send command without
    // attaching" — the false-positive the line-anchor guard must reject.)
    const ZMX_05X_HELP: &str = "\
zmx - session persistence for terminal processes

Usage: zmx <command> [args...]

Commands:
  [a]ttach <name> [command...]             Attach to session, creating if needed
  [r]un <name> [-d] [command...]           Send command without attaching
  [wr]ite <name> <file_path>               Write stdin to file_path through the session
  [l]ist|ls [--short]                      List active sessions
  [k]ill <name>... [--force]               Kill session and all attached clients
";

    #[test]
    fn real_060_help_advertises_send() {
        assert!(looks_like_zmx_help(ZMX_060_HELP));
        assert!(parse_zmx_has_send(ZMX_060_HELP));
        let exec = ScriptedExec::new().on("zmx", &["--help"], Some(0), ZMX_060_HELP, "");
        assert_eq!(zmx_send_capability(&exec), Capability::Yes);
    }

    #[test]
    fn old_05x_shaped_help_lacks_send_is_no() {
        // Recognizable help (run+attach) but NO send subcommand → DEFINITE No.
        assert!(looks_like_zmx_help(ZMX_05X_HELP));
        assert!(!parse_zmx_has_send(ZMX_05X_HELP));
        let exec = ScriptedExec::new().on("zmx", &["--help"], Some(0), ZMX_05X_HELP, "");
        assert_eq!(zmx_send_capability(&exec), Capability::No);
    }

    #[test]
    fn run_prose_send_command_must_not_false_positive() {
        // The killer false-positive: run's description "Send command without
        // attaching" begins a line with "Send" but is PROSE, not a subcommand.
        // The line anchor + the required `<…>` placeholder must reject it.
        assert!(!parse_zmx_has_send(
            "  [r]un <name> [-d] [command...]           Send command without attaching"
        ));
        // Also a standalone prose line that happens to start with "send REQUEST".
        assert!(!parse_zmx_has_send("send REQUEST to the relay daemon"));
        // And a stray "[S]END" token with no angle bracket.
        assert!(!parse_zmx_has_send("  [S]END something"));
    }

    #[test]
    fn command_not_found_is_unknown_never_no() {
        // `bash: zmx: command not found` is non-empty but NOT zmx help → Unknown.
        let exec = ScriptedExec::new().on(
            "zmx",
            &["--help"],
            Some(127),
            "",
            "bash: zmx: command not found\n",
        );
        assert_eq!(zmx_send_capability(&exec), Capability::Unknown);
    }

    #[test]
    fn empty_output_is_unknown() {
        let exec = ScriptedExec::new().on("zmx", &["--help"], Some(0), "", "");
        assert_eq!(zmx_send_capability(&exec), Capability::Unknown);
    }

    #[test]
    fn word_boundaries_match_ts_regexes() {
        // `/\bCommands:/i`: "subcommands:" must NOT register as a section header
        // (and alone it is not zmx help). Lead review fix — exact-regex parity.
        assert!(!looks_like_zmx_help("subcommands: none here"));
        // But a genuine header does (case-insensitive).
        assert!(looks_like_zmx_help("COMMANDS:\n  stuff"));
        // `/\[r\]un\b/i`: a glued suffix breaks the trailing boundary; with no
        // other anchors this text must not register as help.
        assert!(!looks_like_zmx_help("[r]unxyz things\n[a]ttachxyz things"));
        // The real bracketed pair still registers without section headers.
        assert!(looks_like_zmx_help("[r]un <name>\n[a]ttach <name>"));
    }

    #[test]
    fn garbage_output_is_unknown() {
        let exec = ScriptedExec::new().on(
            "zmx",
            &["--help"],
            Some(0),
            "the quick brown fox jumps over the lazy dog",
            "",
        );
        assert_eq!(zmx_send_capability(&exec), Capability::Unknown);
    }

    #[test]
    fn spawn_failure_is_unknown_not_no() {
        // No canned --help match + ScriptedExec returns benign empty → Unknown.
        // (Models ENOENT: a missing zmx must fall through, never a false "no".)
        let exec = ScriptedExec::new();
        assert_eq!(zmx_send_capability(&exec), Capability::Unknown);
    }

    #[test]
    fn wedged_zmx_carries_timeout_and_does_not_hang() {
        // The capability probe MUST pass a 5s timeout (L3: a wedged zmx must not
        // hang). Assert the invocation carries timeout_ms=5000.
        let exec = ScriptedExec::new().on("zmx", &["--help"], Some(0), ZMX_060_HELP, "");
        let _ = zmx_send_capability(&exec);
        assert_eq!(exec.log()[0].timeout_ms, Some(5000));
    }

    #[test]
    fn help_to_stderr_is_salvaged() {
        // Some zmx builds print --help to stderr on nonzero exit; salvage it.
        let exec = ScriptedExec::new().on("zmx", &["--help"], Some(1), "", ZMX_060_HELP);
        assert_eq!(zmx_send_capability(&exec), Capability::Yes);
    }

    #[test]
    fn assert_capable_blocks_only_on_definite_no() {
        let no = ScriptedExec::new().on("zmx", &["--help"], Some(0), ZMX_05X_HELP, "");
        assert!(assert_zmx_capable(&no).is_err());
        let yes = ScriptedExec::new().on("zmx", &["--help"], Some(0), ZMX_060_HELP, "");
        assert!(assert_zmx_capable(&yes).is_ok());
        // Unknown → Ok (proceed; the downstream `zmx run` is the real gate).
        let unknown = ScriptedExec::new();
        assert!(assert_zmx_capable(&unknown).is_ok());
    }

    #[test]
    fn guidance_strings_match_ts() {
        // Byte-match against the TS strings (utils.ts:319-333).
        assert_eq!(
            zmx_upgrade_guidance(),
            "zmx is too old: it has no `send` subcommand, which qd needs to drive\n\
             sessions (boot, prompt delivery, relay). Upgrade to zmx >= 0.6, then retry:\n  \
             brew upgrade neurosnap/tap/zmx   # or: brew install neurosnap/tap/zmx\n  \
             (no brew: see https://github.com/neurosnap/zmx)"
        );
        assert_eq!(
            zmx_missing_guidance(),
            "Failed to launch zmx — is it installed and on PATH?\n  \
             Install: brew install neurosnap/tap/zmx\n  \
             (no brew: see https://github.com/neurosnap/zmx; qd needs zmx >= 0.6)"
        );
    }
}
