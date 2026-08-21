//! Relay MCP registration via Claude Code's own `claude mcp` CLI (ADR 0017).
//!
//! ## Why shell out instead of writing config ourselves
//!
//! Claude Code reads MCP server definitions from ITS files, in ITS format,
//! and that location has moved across versions. The retired approach hand-wrote
//! `~/.claude/.mcp.json` — a path Claude Code 2.1.x does NOT read (it loads
//! user-scope servers from `~/.claude.json`'s `mcpServers`). Rather than track
//! Claude Code's storage format/location ourselves and re-break on every
//! upgrade, we drive `claude mcp add/remove/get`, which always writes wherever
//! THAT version expects. Claude Code owns its own config; we own only the
//! intent ("register the relay, pointing at this binary, at user scope").
//!
//! All three operations go through the [`Exec`] seam so the bin layer's wiring
//! is the only thing that touches a real `claude`, and tests assert the exact
//! argv via `ScriptedExec`.

use crate::exec::Exec;

/// The MCP server name we register the relay under.
pub const RELAY_SERVER_NAME: &str = "relay";

/// The BARE command we register the relay under (owner decision, relay-path
/// hardening v2). We register `qd relay:serve` — the bare program name `qd`,
/// NOT an absolute path to a specific binary — so the registration NEVER goes
/// stale when the `qd` binary is moved or upgraded out from under it (the root
/// cause of the recurring "no relay" bug). Claude Code resolves MCP `command`
/// values via PATH (proven by the working `npx`-based servers), and `qd` is on
/// PATH for any working setup. Args stay `["relay:serve"]`.
pub const RELAY_BARE_COMMAND: &str = "qd";

/// A bounded deadline for every `claude mcp` shell-out — a wedged `claude`
/// must never hang bootstrap (mirrors the zmx-probe L3 discipline).
const CLAUDE_TIMEOUT_MS: u64 = 20_000;

/// Whether the relay is registered with Claude Code (user scope). Drives
/// `claude mcp get relay`, which exits 0 when the server exists and non-zero
/// when it does not. Returns:
/// - `Some(true)`  — registered,
/// - `Some(false)` — not registered (claude ran, reported absent),
/// - `None`        — undetermined (claude not on PATH / spawn error / timeout).
pub fn relay_is_registered(exec: &impl Exec) -> Option<bool> {
    let args = [
        "mcp".to_string(),
        "get".to_string(),
        RELAY_SERVER_NAME.to_string(),
    ];
    match exec.run("claude", &args, &[], None, Some(CLAUDE_TIMEOUT_MS)) {
        Ok(r) if r.timed_out => None,
        Ok(r) => r.status.map(|code| code == 0),
        Err(_) => None,
    }
}

/// PURE (warranty belt, 2026-06-11): is the relay registered in Claude Code's
/// user-scope MCP config? Given the CONTENTS of `~/.claude.json`, returns
/// whether a top-level `mcpServers.relay` entry exists. This is the CHEAP path
/// (a file read + parse, NO `claude` subprocess) used by `dispatch start` to warn at
/// session birth when a session would be born transport-less — the failure
/// class the P0 cutover hit (every post-cutover session had no relay because
/// the user-scope registration step was omitted; the defect surfaced only at
/// first `send:relay`, in P1 soak, not at the flip).
///
/// Returns `None` on unparseable JSON (best-effort: a malformed config must not
/// produce a false "missing" nag) and `Some(false)` only when the parse
/// succeeded and the `relay` key is genuinely absent. Keying on the top-level
/// `mcpServers` matches what `claude mcp add -s user` writes and what
/// `claude mcp get relay` reports as `Scope: User`.
pub fn user_relay_registered(claude_json: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(claude_json).ok()?;
    Some(
        v.get("mcpServers")
            .and_then(|m| m.get(RELAY_SERVER_NAME))
            .is_some(),
    )
}

/// PURE: the `command` string Claude Code currently has registered for the relay
/// MCP server, given the CONTENTS of `~/.claude.json`. This is the program the
/// entry spawns (`<command> relay:serve`) — now the BARE `qd` we register
/// (resolved via PATH), but it may also be a legacy ABSOLUTE PATH or a stale
/// bare name (e.g. `dispatch` from a prior rename) that self-heal must repair.
/// Returns `None` when the config is unparseable, the `relay` entry is absent,
/// or it carries no string `command`.
pub fn registered_relay_command(claude_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(claude_json).ok()?;
    v.get("mcpServers")
        .and_then(|m| m.get(RELAY_SERVER_NAME))
        .and_then(|relay| relay.get("command"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
}

/// PURE self-heal decision (relay-path hardening v2, owner ruling): given the
/// CONTENTS of `~/.claude.json` and a probe for whether a stored ABSOLUTE path
/// still names an existing file, decide whether the registration is genuinely
/// BROKEN and must be repaired (re-pointed to the bare [`RELAY_BARE_COMMAND`]).
///
/// A registration is STALE (broken) when EITHER:
/// - its stored command is an ABSOLUTE path that names a file which no longer
///   exists (the legacy binary was moved/deleted), OR
/// - its stored command is a BARE name that does not match [`RELAY_BARE_COMMAND`]
///   (a leftover from a prior rename, e.g. `dispatch` after the dispatch→qd rename).
///
/// VALID and left alone:
/// - the current [`RELAY_BARE_COMMAND`] (`qd`) — Claude Code resolves it via PATH
///   at spawn time; self-heal must NOT rewrite it (would re-introduce staleness).
/// - an ABSOLUTE path that still EXISTS — a legacy entry that still works.
///
/// `command_exists` is injected (the bin layer stats the path) so this stays
/// pure + unit-testable. We DELIBERATELY do NOT touch an unregistered relay
/// (`registered_relay_command` is `None`) — self-heal only REPAIRS a broken
/// existing entry; it never registers one the user never asked for (consent).
pub fn relay_command_is_stale(claude_json: &str, command_exists: impl Fn(&str) -> bool) -> bool {
    match registered_relay_command(claude_json) {
        Some(stored) if is_absolute_path(&stored) => !command_exists(&stored),
        // A bare name that is not the current command is a stale rename remnant.
        Some(stored) => stored != RELAY_BARE_COMMAND,
        None => false,
    }
}

/// Is `command` an absolute filesystem path (vs. a bare program name resolved via
/// PATH)? Unix absolute paths start with `/`; a bare command like `qd` does not.
/// (dispatch targets Unix only — macOS/Linux — so a `/` prefix is the right test.)
fn is_absolute_path(command: &str) -> bool {
    command.starts_with('/')
}

/// Register the relay at USER scope as the BARE command [`RELAY_BARE_COMMAND`]
/// (`qd relay:serve`). IDEMPOTENT and REPOINTING: `claude mcp add` refuses to
/// overwrite an existing entry (it reports "already exists" and leaves the old
/// command untouched), so we `remove` first (harmless when absent — `claude mcp
/// remove` exits 0 either way) and then `add`, guaranteeing the command becomes
/// the bare `qd`.
///
/// We register the BARE program name (not an absolute path) so the entry NEVER
/// goes stale when the `qd` binary moves/upgrades — Claude Code resolves the
/// command via PATH at spawn time (owner decision; relay-path hardening v2).
///
/// The argv is `claude mcp add -s user relay -- qd relay:serve` (stdio is
/// claude's default transport). Returns `Ok(())` on a clean add, else the
/// child's stderr/stdout (or the spawn error) so the caller can report it.
pub fn register_relay(exec: &impl Exec) -> Result<(), String> {
    // Remove any existing entry so the subsequent add REPOINTS the command.
    let _ = exec.run("claude", &remove_args(), &[], None, Some(CLAUDE_TIMEOUT_MS));

    let add_args = [
        "mcp".to_string(),
        "add".to_string(),
        "-s".to_string(),
        "user".to_string(),
        RELAY_SERVER_NAME.to_string(),
        "--".to_string(),
        RELAY_BARE_COMMAND.to_string(),
        "relay:serve".to_string(),
    ];
    match exec.run("claude", &add_args, &[], None, Some(CLAUDE_TIMEOUT_MS)) {
        Ok(r) if r.timed_out => Err("`claude mcp add` timed out".to_string()),
        Ok(r) if r.status == Some(0) => Ok(()),
        Ok(r) => Err(first_nonempty_line(&r.stderr, &r.stdout)),
        Err(e) => Err(e.to_string()),
    }
}

/// Unregister the relay at user scope (rollback). IDEMPOTENT: `claude mcp
/// remove` exits 0 whether or not the entry was present.
pub fn unregister_relay(exec: &impl Exec) -> Result<(), String> {
    match exec.run("claude", &remove_args(), &[], None, Some(CLAUDE_TIMEOUT_MS)) {
        Ok(r) if r.timed_out => Err("`claude mcp remove` timed out".to_string()),
        Ok(r) if r.status == Some(0) => Ok(()),
        Ok(r) => Err(first_nonempty_line(&r.stderr, &r.stdout)),
        Err(e) => Err(e.to_string()),
    }
}

fn remove_args() -> [String; 5] {
    [
        "mcp".to_string(),
        "remove".to_string(),
        "-s".to_string(),
        "user".to_string(),
        RELAY_SERVER_NAME.to_string(),
    ]
}

/// First non-empty line of stderr, else of stdout, else a generic message —
/// for a terse one-line error in the report.
fn first_nonempty_line(stderr: &str, stdout: &str) -> String {
    for src in [stderr, stdout] {
        if let Some(line) = src.lines().map(str::trim).find(|l| !l.is_empty()) {
            return line.to_string();
        }
    }
    "claude mcp command failed".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ScriptedExec;

    #[test]
    fn is_registered_true_on_exit_0() {
        let exec = ScriptedExec::new().on("claude", &["mcp", "get", "relay"], Some(0), "", "");
        assert_eq!(relay_is_registered(&exec), Some(true));
    }

    // ===== warranty belt: the cheap user-scope config predicate =====

    #[test]
    fn user_relay_registered_true_when_present() {
        let json =
            r#"{"mcpServers":{"playwright":{},"relay":{"command":"qd","args":["relay:serve"]}}}"#;
        assert_eq!(user_relay_registered(json), Some(true));
    }

    #[test]
    fn user_relay_registered_false_when_absent() {
        // The P0-cutover shape: other servers present, relay missing.
        let json = r#"{"mcpServers":{"computer-use-local":{},"playwright":{}}}"#;
        assert_eq!(user_relay_registered(json), Some(false));
        // No mcpServers block at all is also a genuine absence.
        assert_eq!(user_relay_registered(r#"{"projects":{}}"#), Some(false));
    }

    #[test]
    fn user_relay_registered_none_on_unparseable() {
        // Best-effort: a malformed/huge-truncated config must NOT produce a
        // false "missing" nag at every session birth.
        assert_eq!(user_relay_registered("{not json"), None);
        assert_eq!(user_relay_registered(""), None);
    }

    // ===== self-heal: stored-command extraction + stale decision =====

    #[test]
    fn registered_relay_command_extracts_the_command_path() {
        let json = r#"{"mcpServers":{"relay":{"command":"/old/gone/qd","args":["relay:serve"]}}}"#;
        assert_eq!(
            registered_relay_command(json).as_deref(),
            Some("/old/gone/qd")
        );
    }

    #[test]
    fn registered_relay_command_none_when_absent_or_unparseable() {
        assert_eq!(registered_relay_command(r#"{"mcpServers":{}}"#), None);
        assert_eq!(registered_relay_command("{not json"), None);
        // relay present but no string command (e.g. a malformed/partial entry).
        assert_eq!(
            registered_relay_command(r#"{"mcpServers":{"relay":{"args":[]}}}"#),
            None
        );
    }

    #[test]
    fn relay_command_stale_only_when_absolute_path_is_missing() {
        // The only BROKEN shape: a stored ABSOLUTE path naming a file that is
        // gone (the legacy `qd` was moved/deleted). → repair (re-point to bare).
        let json = r#"{"mcpServers":{"relay":{"command":"/old/gone/qd"}}}"#;
        assert!(relay_command_is_stale(json, |_| false));
    }

    #[test]
    fn relay_command_not_stale_for_bare_command() {
        // The form we NOW register — bare `qd`, resolved via PATH. NEVER stale,
        // even though it is not an absolute path: the correct bare command never
        // goes stale when the binary moves. Self-heal must leave it alone (NOT
        // rewrite it to an absolute path, which would re-introduce staleness).
        let json = r#"{"mcpServers":{"relay":{"command":"qd","args":["relay:serve"]}}}"#;
        // `command_exists` is irrelevant for the correct bare command — never consulted.
        assert!(!relay_command_is_stale(json, |_| false));
        assert!(!relay_command_is_stale(json, |_| true));
    }

    #[test]
    fn relay_legacy_bare_dispatch_command_is_stale() {
        // A leftover BARE `dispatch` entry from the pre-rename (dispatch→qd) era is
        // flagged as stale — `dispatch` is gone from PATH and is not the current
        // RELAY_BARE_COMMAND. Self-heal re-points it to bare `qd`.
        let legacy = r#"{"mcpServers":{"relay":{"command":"dispatch","args":["relay:serve"]}}}"#;
        assert!(relay_command_is_stale(legacy, |_| false));
        assert!(relay_command_is_stale(legacy, |_| true));
    }

    #[test]
    fn relay_command_not_stale_for_existing_absolute_path() {
        // A legacy absolute-path entry that STILL EXISTS is valid — leave it
        // alone (no need to disturb a working registration).
        let json = r#"{"mcpServers":{"relay":{"command":"/here/qd"}}}"#;
        assert!(!relay_command_is_stale(json, |_| true));
    }

    #[test]
    fn relay_command_not_stale_when_unregistered() {
        // No relay entry → self-heal never fabricates a registration (consent).
        assert!(!relay_command_is_stale(r#"{"mcpServers":{}}"#, |_| false));
        // Unparseable config → no-op (best-effort, never errors a relay boot).
        assert!(!relay_command_is_stale("{not json", |_| false));
    }

    #[test]
    fn is_registered_false_on_nonzero() {
        let exec = ScriptedExec::new().on(
            "claude",
            &["mcp", "get", "relay"],
            Some(1),
            "",
            "No MCP server found",
        );
        assert_eq!(relay_is_registered(&exec), Some(false));
    }

    #[test]
    fn is_registered_none_on_signal_kill() {
        // status None (signalled / undetermined) → None, not a false "absent".
        let exec = ScriptedExec::new().on("claude", &["mcp", "get", "relay"], None, "", "");
        assert_eq!(relay_is_registered(&exec), None);
    }

    #[test]
    fn register_removes_then_adds_with_repoint_argv() {
        let exec = ScriptedExec::new().on(
            "claude",
            &["mcp", "add"],
            Some(0),
            "Added stdio MCP server relay",
            "",
        );
        register_relay(&exec).expect("register ok");
        let log = exec.log();
        // remove BEFORE add (the repoint guarantee).
        let remove_idx = log
            .iter()
            .position(|i| {
                i.cmd == "claude"
                    && i.args.first().map(String::as_str) == Some("mcp")
                    && i.args.get(1).map(String::as_str) == Some("remove")
            })
            .expect("remove ran");
        let add_idx = log
            .iter()
            .position(|i| i.cmd == "claude" && i.args.get(1).map(String::as_str) == Some("add"))
            .expect("add ran");
        assert!(remove_idx < add_idx, "remove must precede add: {log:?}");
        // The add argv is the user-scope stdio registration pointing at the BARE
        // `qd` command (resolved via PATH — never goes stale on a binary move).
        let add = &log[add_idx];
        assert_eq!(
            add.args,
            vec![
                "mcp",
                "add",
                "-s",
                "user",
                "relay",
                "--",
                "qd",
                "relay:serve"
            ]
        );
        // Both commands carry a bounded timeout (no-hang discipline).
        assert!(add.timeout_ms.is_some());
    }

    #[test]
    fn register_surfaces_add_failure() {
        let exec = ScriptedExec::new().on(
            "claude",
            &["mcp", "add"],
            Some(1),
            "",
            "claude: config write failed",
        );
        let err = register_relay(&exec).unwrap_err();
        assert!(err.contains("config write failed"), "got: {err}");
    }

    #[test]
    fn register_errors_on_signalled_add_never_panics() {
        // A signalled/undetermined add (status None) maps to a non-empty Err,
        // never a panic or a false success. (The timed_out arm is the same
        // shape; ScriptedExec can't set timed_out, so this covers the branch.)
        let exec = ScriptedExec::new().on("claude", &["mcp", "add"], None, "", "");
        let err = register_relay(&exec).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn unregister_ok_on_exit_0() {
        let exec = ScriptedExec::new().on("claude", &["mcp", "remove"], Some(0), "Removed", "");
        unregister_relay(&exec).expect("unregister ok");
        assert!(exec.ran("claude", &["mcp", "remove", "-s", "user", "relay"]));
    }

    #[test]
    fn unregister_surfaces_failure() {
        let exec = ScriptedExec::new().on(
            "claude",
            &["mcp", "remove"],
            Some(2),
            "",
            "exists in multiple scopes",
        );
        let err = unregister_relay(&exec).unwrap_err();
        assert!(err.contains("multiple scopes"), "got: {err}");
    }
}
