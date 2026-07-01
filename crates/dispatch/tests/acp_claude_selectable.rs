//! claude-migration — SELECTABLE-BEHIND-SEAM containment proof (isolation-safe; NO live claude).
//!
//! The claude atomic makes `@agentclientprotocol/claude-agent-acp` ([`CLAUDE_AGENT_ACP_BIN`])
//! reachable behind the retained seam WITHOUT flipping Pete's live default (`claude-code-acp`,
//! [`BRIDGE_BIN`]). The daemon's `--bridge-cmd` lever (`acp_residence.rs`) already provides the
//! selectability; this test pins the three acceptance conditions at the arg-plumbing layer, so no
//! bridge is spawned and Pete's auth/session/quota is never touched:
//!
//! 1. DEFAULT BYTE-FOR-BYTE UNTOUCHED — the production create/resume paths call
//!    `build_adapter_argv(.., bridge_cmd = None, ..)` (see `lifecycle.rs::run_new_acp_daemon` and
//!    `resume.rs`); with `None` the argv carries NO `--bridge-cmd`, so the spawned daemon's
//!    `parse_adapter_args` falls back to `BRIDGE_BIN` = `claude-code-acp`.
//! 2. SELECTOR NOT ACCIDENTALLY ENGAGEABLE — the ONLY way to reach `claude-agent-acp` is an
//!    explicit `--bridge-cmd claude-agent-acp` on the `qd acp-daemon` argv; no production path
//!    emits it (both hardcode `None`), so Pete's daily `qd start`/`qd resume` cannot inadvertently
//!    engage the new bridge. Unset ⇒ default.
//! 3. CLEAN REVIEWABLE FLIP-POINT — the live cutover is a single edit repointing `BRIDGE_BIN` at
//!    `CLAUDE_AGENT_ACP_BIN`; this test pins both const values so that flip is a labeled one-liner
//!    and a typo of the target string reds here.

use std::path::Path;

use dispatch::acp_residence::{build_adapter_argv, parse_adapter_args};
use dispatch::provider::acp::{BRIDGE_BIN, CLAUDE_AGENT_ACP_BIN};

const EXE: &str = "/usr/bin/qd";
const EP: &str = "ws://127.0.0.1:9000";
const CWD: &str = "/work";

/// Re-parse a full built argv the way the resident daemon does: drop `exe` + the `acp-daemon`
/// verb marker (argv[0], argv[1]) and feed the rest to `parse_adapter_args`.
fn reparse(argv: &[String]) -> dispatch::acp_residence::AdapterOpts {
    parse_adapter_args(&argv[2..]).expect("built argv must parse back")
}

#[test]
fn production_create_path_resolves_default_claude_code_acp() {
    // EXACT shape of lifecycle.rs::run_new_acp_daemon: bridge_cmd = None, no load-session.
    let argv = build_adapter_argv(Path::new(EXE), EP, Path::new(CWD), None, &[], None);
    assert!(
        !argv.iter().any(|a| a == "--bridge-cmd"),
        "production create argv must NOT carry --bridge-cmd (else Pete's default could drift): {argv:?}"
    );
    let opts = reparse(&argv);
    assert_eq!(
        opts.bridge_cmd, BRIDGE_BIN,
        "unset --bridge-cmd MUST resolve the compiled default"
    );
    assert_eq!(
        opts.bridge_cmd, "claude-code-acp",
        "Pete's live default bridge is byte-for-byte unchanged"
    );
}

#[test]
fn production_resume_path_resolves_default_claude_code_acp() {
    // EXACT shape of resume.rs: bridge_cmd = None, load_session = Some(..).
    let argv = build_adapter_argv(
        Path::new(EXE),
        EP,
        Path::new(CWD),
        None,
        &[],
        Some("sess-XYZ"),
    );
    assert!(
        !argv.iter().any(|a| a == "--bridge-cmd"),
        "production resume argv must NOT carry --bridge-cmd: {argv:?}"
    );
    let opts = reparse(&argv);
    assert_eq!(opts.bridge_cmd, "claude-code-acp");
    assert_eq!(opts.load_session.as_deref(), Some("sess-XYZ"));
}

#[test]
fn claude_agent_acp_is_selectable_behind_the_seam() {
    // Explicit selection — the ONLY route to the new bridge (tests + the deferred Pete-awake runs).
    let argv = build_adapter_argv(
        Path::new(EXE),
        EP,
        Path::new(CWD),
        Some(CLAUDE_AGENT_ACP_BIN),
        &[],
        None,
    );
    // The argv carries the explicit selector...
    let pos = argv
        .iter()
        .position(|a| a == "--bridge-cmd")
        .expect("explicit selection MUST emit --bridge-cmd");
    assert_eq!(argv.get(pos + 1).map(String::as_str), Some(CLAUDE_AGENT_ACP_BIN));
    // ...and it round-trips back to the new bridge through the daemon's own parser.
    let opts = reparse(&argv);
    assert_eq!(
        opts.bridge_cmd, CLAUDE_AGENT_ACP_BIN,
        "claude-agent-acp must be reachable behind the seam via --bridge-cmd"
    );
    assert_eq!(opts.bridge_cmd, "claude-agent-acp");
}

#[test]
fn flip_point_constants_are_pinned() {
    // The live cutover = repointing BRIDGE_BIN at CLAUDE_AGENT_ACP_BIN. Pin both so the flip is a
    // labeled one-liner and any drift in the target string reds here (super18's Pete-awake gate).
    assert_eq!(BRIDGE_BIN, "claude-code-acp", "default must stay claude-code-acp tonight");
    assert_eq!(CLAUDE_AGENT_ACP_BIN, "claude-agent-acp", "migration target");
    assert_ne!(BRIDGE_BIN, CLAUDE_AGENT_ACP_BIN, "the flip actually changes the bridge");
}
