//! `qd resume` decision core (spec §5.3; TS `commands/lifecycle.ts:408-530` +
//! `utils.ts:470-565`).
//!
//! Resume relaunches a COLD session in zmx (by default). The PURE pieces here are
//! the preflight deciders the bin verb drives BEFORE any spawn:
//!   - F3 cwd reality-check ([`resolve_resume_cwd`], `utils.ts:480-505`),
//!   - S2 session-name validation ([`validate_session_name`], `utils.ts:553-565`),
//!   - the zmx-name derivation ([`derive_zmx_name`], `lifecycle.ts:475-477`).
//!
//! The claude relaunch flag is `--resume <session-id>` exactly as the TS
//! `buildClaudeCmd(["--resume", session.sessionId])` does at the resume call-site
//! (`lifecycle.ts:474`). The Rust [`crate::launch::build_claude_cmd`] takes
//! `extra` args, so the bin verb passes `["--resume", session_id]` through
//! `build_new_extra_args`-free (resume does not add `--name`; it reuses the
//! existing session id) — see the bin verb for the exact assembly.

/// Result of [`resolve_resume_cwd`] — a resolved cwd, or an actionable error.
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeCwd {
    Cwd(String),
    Error(String),
}

/// PURE: resolve the working directory for resume with a REALITY CHECK on the
/// recorded cwd (F3 / DUR-3+DUR-5, `resolveResumeCwd`, utils.ts:480-505).
///
/// A renamed/deleted project dir previously reached the spawn and crashed with a
/// raw ENOENT; this turns that into a clean, actionable error suggesting `--cwd`.
/// Order: explicit `--cwd` override (must exist) → recorded session cwd (must
/// exist, else error) → caller's cwd fallback. `exists` is injected (tests forge
/// the filesystem; no real fs touched).
pub fn resolve_resume_cwd(
    recorded_cwd: Option<&str>,
    override_cwd: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
    fallback: &str,
) -> ResumeCwd {
    if let Some(over) = override_cwd {
        if !exists(over) {
            return ResumeCwd::Error(format!("--cwd directory does not exist: {over}"));
        }
        return ResumeCwd::Cwd(over.to_string());
    }
    if let Some(rec) = recorded_cwd {
        if !exists(rec) {
            return ResumeCwd::Error(format!(
                "Session's recorded directory no longer exists: {rec}\n  \
                 The project may have been moved, renamed, or deleted.\n  \
                 Resume it elsewhere with: qd resume <session> --cwd <dir>"
            ));
        }
        return ResumeCwd::Cwd(rec.to_string());
    }
    ResumeCwd::Cwd(fallback.to_string())
}

/// PURE: session-name safety guard (S2, `validateSessionName`, utils.ts:553-565).
///
/// Reject names that could cause path traversal via the env-file path or inject
/// shell metacharacters into the single-quoted `bash -lc` prefix (a `'` in the
/// name breaks the surrounding single-quote context). Allowed: alphanumeric,
/// hyphen, underscore, dot. Returns `None` if valid, `Some(message)` if invalid.
pub fn validate_session_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("Session name must not be empty.".to_string());
    }
    let ok = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-');
    if !ok {
        return Some(format!(
            "Session name \"{name}\" contains unsafe characters. \
             Names may only contain letters, digits, hyphens, underscores, and dots."
        ));
    }
    None
}

/// PURE: sanitize an auto-derived session name into a mux/zmx-safe name.
///
/// An auto-named session's `name` comes from the claude transcript TITLE (join.rs:
/// `stats.name`), which routinely contains spaces and punctuation — e.g.
/// "gdb debug helper". Feeding that straight into the zmx name (which also becomes
/// an env-file path component and a `bash -lc` single-quoted token) is what
/// `validate_session_name` rejects, so a `resume`/`connect` of an auto-named cold
/// session used to die with "Session name … contains unsafe characters". Pete's
/// ruling: connect/resume must "just work" — we must not punish the user for a
/// title claude generated. So we MAP unsafe bytes to `-` (then collapse runs and
/// trim edge dashes for readability) instead of refusing.
///
/// Allowed (mirrors `validate_session_name`): ASCII alphanumeric, `_`, `.`, `-`.
/// Returns "" when nothing survives (e.g. an all-emoji title) so the caller can
/// fall back to `claude-<id>`. The result always passes `validate_session_name`.
pub fn sanitize_zmx_name(name: &str) -> String {
    let mapped = name.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
            c
        } else {
            '-'
        }
    });
    // Collapse runs of '-' (so "a  b" → "a-b", not "a--b").
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in mapped {
        if c == '-' {
            if !prev_dash {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

/// PURE: derive the zmx session name for a resume (`lifecycle.ts:475-477`):
/// `opts.zmxName || session.name || "claude-" + sessionId[0..8]`.
///
/// An explicit `--zmx-name` is returned VERBATIM (the user typed it; the caller's
/// `validate_session_name` guards it loudly). An auto-derived `session.name` is run
/// through [`sanitize_zmx_name`] so a spacey/auto title produces a valid name
/// instead of a hard error; if nothing survives sanitization we fall through to the
/// `claude-<id>` fallback.
pub fn derive_zmx_name(
    zmx_name_opt: Option<&str>,
    session_name: Option<&str>,
    session_id: &str,
) -> String {
    if let Some(z) = zmx_name_opt.filter(|s| !s.is_empty()) {
        return z.to_string();
    }
    if let Some(n) = session_name.filter(|s| !s.is_empty()) {
        let safe = sanitize_zmx_name(n);
        if !safe.is_empty() {
            return safe;
        }
    }
    let slice: String = session_id.chars().take(8).collect();
    format!("claude-{slice}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exists_set(set: &[&'static str]) -> impl Fn(&str) -> bool {
        let owned: Vec<String> = set.iter().map(|s| s.to_string()).collect();
        move |p: &str| owned.iter().any(|x| x == p)
    }

    // --- resolveResumeCwd (utils.test.ts:270-289) ---

    #[test]
    fn recorded_cwd_exists_used() {
        let f = exists_set(&["/proj/x"]);
        assert_eq!(
            resolve_resume_cwd(Some("/proj/x"), None, &f, "/caller"),
            ResumeCwd::Cwd("/proj/x".to_string())
        );
    }

    #[test]
    fn recorded_cwd_missing_clean_error_not_enoent() {
        let f = exists_set(&[]);
        match resolve_resume_cwd(Some("/proj/x"), None, &f, "/caller") {
            ResumeCwd::Error(e) => {
                assert!(e.contains("no longer exists: /proj/x"));
                assert!(e.contains("--cwd"));
            }
            other => panic!("expected clean error, got {other:?}"),
        }
    }

    #[test]
    fn override_wins_when_it_exists() {
        let f = exists_set(&["/proj/y"]);
        assert_eq!(
            resolve_resume_cwd(Some("/gone"), Some("/proj/y"), &f, "/c"),
            ResumeCwd::Cwd("/proj/y".to_string())
        );
    }

    #[test]
    fn override_missing_is_error() {
        let f = exists_set(&[]);
        match resolve_resume_cwd(Some("/gone"), Some("/also-gone"), &f, "/c") {
            ResumeCwd::Error(e) => assert!(e.contains("--cwd directory does not exist")),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn no_recorded_no_override_falls_back_to_caller() {
        let f = exists_set(&[]);
        assert_eq!(
            resolve_resume_cwd(None, None, &f, "/caller"),
            ResumeCwd::Cwd("/caller".to_string())
        );
    }

    // --- validateSessionName (S2) ---

    #[test]
    fn valid_names_accepted() {
        assert_eq!(validate_session_name("my-sess_1.2"), None);
        assert_eq!(validate_session_name("qdrg-abc-kill1"), None);
    }

    #[test]
    fn empty_name_rejected() {
        assert!(validate_session_name("").is_some());
    }

    #[test]
    fn traversal_and_injection_rejected() {
        // Path traversal.
        assert!(validate_session_name("../etc/passwd").is_some());
        // Shell-injection via single quote (would break the bash -lc prefix).
        assert!(validate_session_name("a'b").is_some());
        // Spaces and slashes.
        assert!(validate_session_name("a b").is_some());
        assert!(validate_session_name("a/b").is_some());
        // A semicolon (command separator).
        assert!(validate_session_name("a;rm -rf").is_some());
    }

    // --- derive_zmx_name (lifecycle.ts:475-477) ---

    #[test]
    fn zmx_name_opt_wins() {
        assert_eq!(
            derive_zmx_name(Some("custom"), Some("name"), "abcdefgh12345"),
            "custom"
        );
    }

    #[test]
    fn session_name_when_no_opt() {
        assert_eq!(derive_zmx_name(None, Some("name"), "abcdefgh12345"), "name");
    }

    #[test]
    fn claude_prefix_fallback_takes_first_eight() {
        assert_eq!(
            derive_zmx_name(None, None, "abcdefgh12345"),
            "claude-abcdefgh"
        );
        // Short id: takes what's there.
        assert_eq!(derive_zmx_name(None, None, "abc"), "claude-abc");
    }

    #[test]
    fn empty_opt_and_name_fall_through() {
        assert_eq!(
            derive_zmx_name(Some(""), Some(""), "abcdefgh"),
            "claude-abcdefgh"
        );
    }

    // --- sanitize_zmx_name + auto-named derivation (the connect/resume "spaces in
    //     the name" fix) ---

    #[test]
    fn sanitize_spaces_to_hyphens() {
        assert_eq!(sanitize_zmx_name("gdb debug helper"), "gdb-debug-helper");
    }

    #[test]
    fn sanitize_collapses_runs_and_trims_edges() {
        // Multiple/edge unsafe chars don't leak stray or doubled hyphens.
        assert_eq!(sanitize_zmx_name("  a   b  "), "a-b");
        assert_eq!(sanitize_zmx_name("a/b;c"), "a-b-c");
        assert_eq!(
            sanitize_zmx_name("-leading.and.trailing-"),
            "leading.and.trailing"
        );
    }

    #[test]
    fn sanitize_keeps_already_valid_names_unchanged() {
        assert_eq!(sanitize_zmx_name("my-sess_1.2"), "my-sess_1.2");
    }

    #[test]
    fn sanitize_all_unsafe_yields_empty() {
        // Nothing survives → caller falls back to claude-<id>.
        assert_eq!(sanitize_zmx_name("   "), "");
        assert_eq!(sanitize_zmx_name("🙂🙂"), "");
    }

    #[test]
    fn auto_named_session_derives_a_valid_zmx_name() {
        // The end-to-end bug: an auto-named cold session's title has spaces. The
        // derived name must be sanitized AND pass validate_session_name (no longer
        // the "contains unsafe characters" hard error).
        let derived = derive_zmx_name(None, Some("gdb debug helper"), "abcdefgh12345");
        assert_eq!(derived, "gdb-debug-helper");
        assert_eq!(validate_session_name(&derived), None);
    }

    #[test]
    fn all_unsafe_session_name_falls_back_to_claude_id() {
        assert_eq!(
            derive_zmx_name(None, Some("🙂 🙂"), "abcdefgh12345"),
            "claude-abcdefgh"
        );
    }

    #[test]
    fn explicit_zmx_name_opt_is_not_sanitized() {
        // An explicit --zmx-name is the user's typed choice: returned verbatim so the
        // caller's validate_session_name can reject it LOUDLY rather than silently
        // rewriting their input.
        assert_eq!(derive_zmx_name(Some("a b"), Some("name"), "id"), "a b");
    }
}
