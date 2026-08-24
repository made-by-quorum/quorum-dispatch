//! `provider/opencode/acp` — opencode's ACP bridge spec.
//!
//! # opencode's ONLY live transport
//!
//! Unlike claude, codex and pi, opencode has no native lane in qd at all: the
//! sibling [`super::store`] reads opencode's `opencode.db` at rest, and
//! everything live goes over ACP. That makes this file opencode's entire drive
//! path — and it is two declarations, because the protocol is the shared driver
//! in [`crate::provider::shared::acp`] and opencode contributes only the facts
//! about ITSELF: which bridge program to spawn (`opencode acp`), and that the
//! bridge runs an HTTP server of its own that a human TUI can join
//! ([`OPENCODE_HTTP_SERVER`]).
//!
//! That economy is the argument for keeping the ACP driver shared. A-OC.1 added
//! opencode to qd without writing a single line of protocol code; the pi-acp
//! spike (`doc/tbd/pi-acp-exploration/`) sizes pi the same way.
//!
//! Re-exported from `shared/acp` under its pre-reorg path, so
//! `provider::acp::ACP_OPENCODE_PROVIDER` still resolves.

use crate::provider::shared::acp::{AcpProvider, HarnessServer};

/// opencode's own HTTP server — the one `opencode acp` starts for itself.
///
/// # This is not a port we opened; it is one we stopped losing
///
/// `opencode acp` is not a stdio shim over some other process. Its handler calls
/// `Server.listen(opts)` and then adapts ACP onto that server through opencode's
/// own SDK client — the bridge and the server are one process, and the server is
/// where the session actually lives. The listen port defaults to `0`, so before
/// this spec qd spawned a real HTTP server per session and then had no way to
/// name it. Pinning it is the whole change: `--port <p>` rides through as
/// `--bridge-arg`s, and the address goes on the row.
///
/// # What it buys
///
/// `opencode attach http://127.0.0.1:<p> --session <id>` — a human TUI as a
/// second client of the very server the ACP bridge is driving. Same property
/// `codex --remote` gives `codex/app-server`: nothing stops, nothing converts,
/// the agent keeps driving. `--fork` is opt-in on that command, so a plain attach
/// JOINS the session rather than branching it.
///
/// Verified against opencode v1.18.21: `cli/network.ts` (`port` default `0`,
/// `hostname` default `127.0.0.1`), `cli/cmd/acp.ts` (the `Server.listen` call),
/// `cli/cmd/attach.ts` (the positional url + `--session`).
pub static OPENCODE_HTTP_SERVER: HarnessServer = HarnessServer {
    port_flag: "--port",
    scheme: "http",
};

/// The registered acp/opencode provider (A-OC.1) — the SAME ACP driver bridged to `opencode acp`
/// instead of `claude-code-acp`. CLI `--provider opencode` resolves to this via
/// [`crate::provider::provider_for`] and rides the existing `acp/`-prefix verb dispatch.
pub static ACP_OPENCODE_PROVIDER: AcpProvider = AcpProvider::new(
    "acp/opencode",
    Some("opencode"),
    &["acp"],
    Some(&OPENCODE_HTTP_SERVER),
);
