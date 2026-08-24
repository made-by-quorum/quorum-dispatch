//! `provider/claude/acp` — claude-code's ACP bridge spec.
//!
//! # Why this is three items and not a transport
//!
//! Driving claude-code over the Agent Client Protocol is not claude-code's own
//! transport the way [`super::relay`] is. It is the SHARED ACP driver
//! ([`crate::provider::shared::acp`]) pointed at a claude bridge program, and the
//! shared driver is deliberately the only implementation — `acp/claude-code` and
//! `acp/opencode` differ by three fields, and `doc/tbd/pi-acp-exploration/` sizes
//! pi as a third instance of the same driver.
//!
//! So the split is: the PROTOCOL lives in `shared/acp/`, and what lives here is
//! the part that is genuinely a fact about claude-code — WHICH bridge program
//! qd spawns to reach it. That is this file, and nothing else. If you came here
//! looking for the ACP wire, queue, correlation or residence code, it is in
//! [`crate::provider::shared::acp`] and it is shared on purpose.
//!
//! Every name here is re-exported from `shared/acp` under its pre-reorg path, so
//! `provider::acp::{BRIDGE_BIN, CLAUDE_AGENT_ACP_BIN, ACP_CC_PROVIDER}` still
//! resolves.

use crate::provider::shared::acp::AcpProvider;

/// The `claude-code-acp` bridge launch command (the official `@zed-industries/claude-code-acp`
/// bin; STEP-0 provenance). The host spawns this with [`BRIDGE_ENV_STRIP`] removed from the env.
///
/// This is Pete's LIVE DEFAULT and it is deliberately UNCHANGED by the claude-migration atomic:
/// the migration makes [`CLAUDE_AGENT_ACP_BIN`] REACHABLE behind the seam (selectable via the
/// `qd acp-daemon --bridge-cmd` lever, see `acp_residence.rs`) while this compiled default keeps
/// resolving `claude-code-acp` for every production create/resume path (which pass `bridge_cmd:
/// None`, so `parse_adapter_args` falls back here). The eventual LIVE CUTOVER is a single,
/// self-contained edit — repointing this const at [`CLAUDE_AGENT_ACP_BIN`] — held for super18's
/// Pete-awake gate after the deferred live gates (MF1/MF4) pass; it is NOT this atomic's to flip.
pub const BRIDGE_BIN: &str = "claude-code-acp";

/// The `claude-agent-acp` bridge launch command — the `@agentclientprotocol/claude-agent-acp`
/// successor to the deprecating `@zed-industries/claude-code-acp`, the claude-migration TARGET.
///
/// It is reachable behind the retained `rpc.rs` seam TODAY, without touching Pete's default:
/// `qd acp-daemon --bridge-cmd claude-agent-acp` selects it (the same custom-transport driver in
/// `client.rs` — `env_remove(BRIDGE_ENV_STRIP)` + `.stderr(inherit)` — drives it, agent-agnostic).
/// No production path engages it (create/resume hardcode `bridge_cmd: None` → [`BRIDGE_BIN`]); it
/// is exercised only by explicit selection (tests + the deferred Pete-awake live-runs). Flipping
/// the live default = repointing [`BRIDGE_BIN`] here (super18's separate gate — NOT tonight).
pub const CLAUDE_AGENT_ACP_BIN: &str = "claude-agent-acp";

/// The registered acp/claude-code provider — Pete's LIVE default. `bridge_cmd = None` ⇒ the
/// compiled [`BRIDGE_BIN`] (`claude-code-acp`), byte-identical to pre-A-OC.1. `'static` singleton
/// (`provider_for` hands out `&'static dyn Provider` without allocation, the `CLAUDE_PROVIDER`
/// precedent).
/// `harness_server = None` because `claude-code-acp` genuinely has no server: it
/// speaks ACP on stdin/stdout and listens on nothing. That is not a gap to fill
/// later — it is why this bridge has no viewer, and why its spawned argv and its
/// registry row are untouched by the `acp/opencode` viewer work.
pub static ACP_CC_PROVIDER: AcpProvider = AcpProvider::new("acp/claude-code", None, &[], None);
