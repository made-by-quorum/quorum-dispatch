//! The relay pin in `~/.claude.json` (R15 item 4) — LOAD-BEARING for agent
//! messaging, and the reason this port is not optional.
//!
//! Port of `qrm/src/verbs.rs::{wire_relay_pin, ensure_relay_entry}` +
//! `qrm/src/rewrite/claude_json.rs`. `qrm` does not ship, so the surgical
//! rewrite has to live here.
//!
//! # Why the rewrite is surgical
//!
//! `~/.claude.json` is not a small config file — it holds Claude Code's whole
//! per-project state (`projects`, history, counters). Re-serialising it from a
//! naively parsed value would reorder every key in the file and produce a diff
//! nobody can review. So: parse with `serde_json`'s `preserve_order` (on by
//! default for this crate — see the `json-insertion-order` feature in
//! Cargo.toml), touch ONLY `mcpServers.relay.command` and `.args`, and write
//! the value back. `Map::insert` on an existing key updates in place and keeps
//! its position, so every other key — and their order — survives exactly as
//! found.
//!
//! # The three cases
//!
//! `qrm` learned the hard way that Claude Code commonly writes a
//! `~/.claude.json` with no `mcpServers.relay` entry at all, so the surgical
//! rewriter (which errors on a missing entry, by design) needs a preparation
//! step. All three cases are handled here:
//!
//! 1. **no file** — synthesize `{}`, ensure the entry, write.
//! 2. **file, no `mcpServers.relay`** — ensure the entry in place, rewrite.
//! 3. **file with an entry** — rewrite in place; idempotent.
//!
//! # Why the BARE `qd` and not an absolute path
//!
//! Claude Code resolves the stored command via `PATH` at spawn time. A bare
//! `qd` therefore survives a binary move or a `brew upgrade`; an absolute path
//! goes stale the first time either happens. This matches
//! [`crate::relay_server::register::RELAY_BARE_COMMAND`], which is what
//! `qd bootstrap`'s `claude mcp add` path registers — the two wiring paths must
//! agree, or a `qd setup` run would fight a `qd bootstrap` run. It is also why
//! the PATH check ([`crate::setup::rc_block`]) is a hard requirement rather
//! than a convenience.

use std::path::Path;

use serde_json::{Map, Value};

/// The MCP server name Claude Code knows the relay by.
pub const RELAY_SERVER_NAME: &str = "relay";

/// The bare command the pin stores. Kept equal to
/// [`crate::relay_server::register::RELAY_BARE_COMMAND`] — asserted in tests.
pub const RELAY_COMMAND: &str = "qd";

/// The argv the relay entry must carry.
pub const RELAY_ARGS: &[&str] = &["relay:serve"];

/// What setup found in `~/.claude.json`. Gathered by the bin layer, ruled on by
/// [`crate::setup::assess`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinState {
    /// No `~/.claude.json` at all — a machine where Claude Code has never run,
    /// or a fresh HOME.
    Absent,
    /// Present but not parseable as JSON. We NEVER rewrite this: clobbering a
    /// file we cannot read would destroy the user's project state.
    Unparsable(String),
    /// Parsed, but there is no `mcpServers.relay`.
    NoEntry,
    /// Parsed, with a relay entry.
    Entry { command: String, args: Vec<String> },
}

impl PinState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PinState::Absent => "absent",
            PinState::Unparsable(_) => "unparsable",
            PinState::NoEntry => "no-entry",
            PinState::Entry { .. } => "entry",
        }
    }
}

/// Read the relay pin out of a parsed `~/.claude.json` (port of `qrm`'s
/// `claude_json::read_relay`).
pub fn read_relay(value: &Value) -> Option<(String, Vec<String>)> {
    let relay = value.get("mcpServers")?.get(RELAY_SERVER_NAME)?;
    let command = relay.get("command")?.as_str()?.to_string();
    let args = relay
        .get("args")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Some((command, args))
}

/// Classify the file's contents (or its absence) into a [`PinState`].
/// `contents` is `None` when the file does not exist.
pub fn classify(contents: Option<&str>) -> PinState {
    let raw = match contents {
        None => return PinState::Absent,
        Some(c) => c,
    };
    let value: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return PinState::Unparsable(e.to_string()),
    };
    match read_relay(&value) {
        Some((command, args)) => PinState::Entry { command, args },
        None => PinState::NoEntry,
    }
}

/// Ensure `value.mcpServers.relay` exists as a minimal `{"type":"stdio"}`
/// object, so the order-preserving [`rewrite_relay_command`] has an entry to
/// update. Idempotent; preserves any existing entry and every other key.
/// (Port of `qrm`'s `ensure_relay_entry`.)
pub fn ensure_relay_entry(value: &mut Value) -> Result<(), String> {
    let obj: &mut Map<String, Value> = value
        .as_object_mut()
        .ok_or_else(|| "~/.claude.json is not a JSON object".to_string())?;
    let mcp = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let mcp = mcp
        .as_object_mut()
        .ok_or_else(|| "~/.claude.json: mcpServers is not a JSON object".to_string())?;
    mcp.entry(RELAY_SERVER_NAME)
        .or_insert_with(|| serde_json::json!({ "type": "stdio" }));
    Ok(())
}

/// Set `mcpServers.relay.command` to `command` and normalise `args`,
/// preserving every other key and its order. Idempotent. Errors if the entry is
/// absent — call [`ensure_relay_entry`] first. (Port of `qrm`'s
/// `rewrite_relay_command`.)
pub fn rewrite_relay_command(mut value: Value, command: &str) -> Result<Value, String> {
    let relay = value
        .get_mut("mcpServers")
        .and_then(|m| m.get_mut(RELAY_SERVER_NAME))
        .ok_or_else(|| "~/.claude.json has no mcpServers.relay entry".to_string())?;
    let obj = relay
        .as_object_mut()
        .ok_or_else(|| "mcpServers.relay is not a JSON object".to_string())?;
    // insert() on an EXISTING key updates in place and keeps its position
    // (serde_json's preserve_order Map is an IndexMap) — this is the whole
    // reason the rewrite can claim to be order-preserving.
    obj.insert("command".into(), Value::String(command.to_string()));
    obj.insert(
        "args".into(),
        Value::Array(RELAY_ARGS.iter().map(|a| Value::String((*a).to_string())).collect()),
    );
    Ok(value)
}

/// The pure end-to-end transform: raw file contents (or `None`) → the exact
/// bytes to write. Covers all three cases; refuses case 2b (unparsable).
///
/// Kept separate from [`wire_relay_pin`] so the transform is testable with no
/// filesystem at all, and so the fs wrapper is four lines of I/O with no logic.
pub fn pinned_contents(existing: Option<&str>, command: &str) -> Result<String, String> {
    let mut value: Value = match existing {
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| format!("~/.claude.json is not valid JSON ({e}) — refusing to rewrite it"))?,
        None => serde_json::json!({}),
    };
    ensure_relay_entry(&mut value)?;
    let rewritten = rewrite_relay_command(value, command)?;
    serde_json::to_string_pretty(&rewritten).map_err(|e| e.to_string())
}

/// Write the pin. The ONLY function here that touches a filesystem; `path` is
/// always injected, so tests run against a temp HOME and never the real `~`.
pub fn wire_relay_pin(path: &Path, command: &str) -> Result<(), String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(c) => Some(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let out = pinned_contents(existing.as_deref(), command)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(path, out).map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live() -> String {
        // Mirrors the real file: unrelated keys around mcpServers, ordered.
        serde_json::to_string_pretty(&json!({
            "numStartups": 42,
            "mcpServers": {
                "relay": { "type": "stdio", "command": "qd", "args": ["relay:serve"], "env": {} }
            },
            "projects": { "/x": {} }
        }))
        .unwrap()
    }

    #[test]
    fn the_bare_command_matches_the_claude_mcp_registration_path() {
        // Two wiring paths (`qd setup` writes the file; `qd bootstrap` shells
        // `claude mcp add`) must agree, or they would fight each other.
        assert_eq!(RELAY_COMMAND, crate::relay_server::register::RELAY_BARE_COMMAND);
    }

    // --- classify: the three cases (+ the refusal) --------------------------

    #[test]
    fn classify_covers_all_three_cases() {
        assert_eq!(classify(None), PinState::Absent);
        assert_eq!(classify(Some("{}")), PinState::NoEntry);
        assert_eq!(classify(Some(r#"{"mcpServers":{}}"#)), PinState::NoEntry);
        assert_eq!(
            classify(Some(&live())),
            PinState::Entry {
                command: "qd".into(),
                args: vec!["relay:serve".into()]
            }
        );
        assert!(matches!(classify(Some("{not json")), PinState::Unparsable(_)));
    }

    #[test]
    fn an_entry_without_args_classifies_with_an_empty_argv() {
        let s = r#"{"mcpServers":{"relay":{"command":"qd"}}}"#;
        assert_eq!(
            classify(Some(s)),
            PinState::Entry {
                command: "qd".into(),
                args: vec![]
            }
        );
    }

    // --- the transform ------------------------------------------------------

    #[test]
    fn case_1_no_file_synthesizes_a_minimal_pin() {
        let out = pinned_contents(None, RELAY_COMMAND).unwrap();
        assert_eq!(
            classify(Some(&out)),
            PinState::Entry {
                command: "qd".into(),
                args: vec!["relay:serve".into()]
            }
        );
        // stdio is the transport Claude Code expects for a command server.
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["relay"]["type"], json!("stdio"));
    }

    #[test]
    fn case_2_file_without_an_entry_gains_one_and_keeps_everything_else() {
        let before = serde_json::to_string_pretty(&json!({
            "numStartups": 7,
            "mcpServers": { "other": { "command": "x" } },
            "projects": { "/p": { "k": 1 } }
        }))
        .unwrap();
        let out = pinned_contents(Some(&before), RELAY_COMMAND).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["numStartups"], json!(7));
        assert_eq!(v["mcpServers"]["other"]["command"], json!("x"));
        assert_eq!(v["projects"]["/p"]["k"], json!(1));
        assert_eq!(v["mcpServers"]["relay"]["command"], json!("qd"));
    }

    #[test]
    fn case_3_existing_entry_is_idempotent() {
        let once = pinned_contents(Some(&live()), RELAY_COMMAND).unwrap();
        let twice = pinned_contents(Some(&once), RELAY_COMMAND).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_stale_absolute_command_is_repointed_without_disturbing_the_entry() {
        let before = serde_json::to_string_pretty(&json!({
            "mcpServers": {
                "relay": { "type": "stdio", "command": "/old/bin/qd", "args": ["x"], "env": {"A":"b"} }
            }
        }))
        .unwrap();
        let out = pinned_contents(Some(&before), RELAY_COMMAND).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["relay"]["command"], json!("qd"));
        assert_eq!(v["mcpServers"]["relay"]["args"], json!(["relay:serve"]));
        // Keys we do not own survive.
        assert_eq!(v["mcpServers"]["relay"]["env"], json!({"A":"b"}));
        assert_eq!(v["mcpServers"]["relay"]["type"], json!("stdio"));
    }

    #[test]
    fn key_order_is_preserved_at_both_levels() {
        // The property that makes this rewrite reviewable on a real
        // ~/.claude.json: no key moves.
        let out = pinned_contents(Some(&live()), "/abs/qd").unwrap();
        let p_num = out.find("numStartups").unwrap();
        let p_mcp = out.find("mcpServers").unwrap();
        let p_proj = out.find("projects").unwrap();
        assert!(p_num < p_mcp && p_mcp < p_proj, "top-level order changed:\n{out}");
        let p_type = out.find("\"type\"").unwrap();
        let p_cmd = out.find("\"command\"").unwrap();
        let p_args = out.find("\"args\"").unwrap();
        let p_env = out.find("\"env\"").unwrap();
        assert!(
            p_type < p_cmd && p_cmd < p_args && p_args < p_env,
            "relay key order changed:\n{out}"
        );
    }

    #[test]
    fn an_unparsable_file_is_refused_never_clobbered() {
        let err = pinned_contents(Some("{ not json"), RELAY_COMMAND).unwrap_err();
        assert!(err.contains("refusing to rewrite"), "{err}");
    }

    #[test]
    fn a_non_object_root_is_refused() {
        let err = pinned_contents(Some("[1,2,3]"), RELAY_COMMAND).unwrap_err();
        assert!(err.contains("not a JSON object"), "{err}");
    }

    #[test]
    fn a_non_object_mcpservers_is_refused() {
        let err = pinned_contents(Some(r#"{"mcpServers": 3}"#), RELAY_COMMAND).unwrap_err();
        assert!(err.contains("mcpServers is not a JSON object"), "{err}");
    }

    #[test]
    fn rewrite_errors_when_no_entry_was_ensured_first() {
        // The precise trap `qrm`'s ensure_relay_entry exists to close.
        let err = rewrite_relay_command(json!({"mcpServers": {}}), "qd").unwrap_err();
        assert!(err.contains("no mcpServers.relay"), "{err}");
    }

    // --- the fs wrapper (temp dirs only; never the real ~) ------------------

    #[test]
    fn wire_writes_all_three_cases_under_a_temp_home() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude.json");

        // 1. no file
        wire_relay_pin(&path, RELAY_COMMAND).unwrap();
        let after_1 = std::fs::read_to_string(&path).unwrap();
        assert!(matches!(classify(Some(&after_1)), PinState::Entry { .. }));

        // 3. existing entry — byte-identical re-run
        wire_relay_pin(&path, RELAY_COMMAND).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_1);

        // 2. file without an entry
        std::fs::write(&path, r#"{"numStartups":1}"#).unwrap();
        wire_relay_pin(&path, RELAY_COMMAND).unwrap();
        let after_2 = std::fs::read_to_string(&path).unwrap();
        assert!(after_2.contains("numStartups"));
        assert!(matches!(classify(Some(&after_2)), PinState::Entry { .. }));
    }
}
