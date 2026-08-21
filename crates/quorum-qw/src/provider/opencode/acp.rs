//! `provider/opencode/acp` — opencode's ACP bridge spec.
//!
//! # opencode's ONLY live transport
//!
//! Unlike claude, codex and pi, opencode has no native lane in qd at all: the
//! sibling [`super::store`] reads opencode's `opencode.db` at rest, and
//! everything live goes over ACP. That makes this file opencode's entire drive
//! path — and it is four lines, because the protocol is the shared driver in
//! [`crate::provider::shared::acp`] and opencode contributes only the bridge
//! spawn (`opencode acp`).
//!
//! That economy is the argument for keeping the ACP driver shared. A-OC.1 added
//! opencode to qd without writing a single line of protocol code; the pi-acp
//! spike (`doc/tbd/pi-acp-exploration/`) sizes pi the same way.
//!
//! Re-exported from `shared/acp` under its pre-reorg path, so
//! `provider::acp::ACP_OPENCODE_PROVIDER` still resolves.

use crate::provider::shared::acp::AcpProvider;

/// The registered acp/opencode provider (A-OC.1) — the SAME ACP driver bridged to `opencode acp`
/// instead of `claude-code-acp`. CLI `--provider opencode` resolves to this via
/// [`crate::provider::provider_for`] and rides the existing `acp/`-prefix verb dispatch.
pub static ACP_OPENCODE_PROVIDER: AcpProvider =
    AcpProvider::new("acp/opencode", Some("opencode"), &["acp"]);
