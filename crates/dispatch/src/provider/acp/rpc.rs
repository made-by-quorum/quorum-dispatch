//! `provider/acp/rpc.rs` — the `AcpClient` transport CONTRACT (ADD-5 pattern), the
//! ACP analog of [`crate::provider::codex::AppServerRpc`] (`provider/codex/rpc.rs:221`).
//!
//! `&self` + interior mutability, exactly as `AppServerRpc` (codex-p2-spec §6.2): a
//! CONNECTED `&dyn AcpClient` is handed in via [`crate::provider::ProviderFx::acp_client`];
//! the verb layer connects the real client and passes the trait object to the
//! provider's `inject`/`boot_waiter`. The trait NEVER opens a socket or holds a raw
//! endpoint string (the banned claude-shaped contortion `relay_port` avoids). The real
//! driver ([`super::client::AcpHost`]) wraps its mutable state (child process, id
//! counter, the SC-5 single-reader demux bus) in a `RefCell`; single-threaded / `!Sync`.
//!
//! STEP-0 grounding (live, this box, 2026-06-25 — see `~/work/acp-cc-coord/STEP0-FINDING.md`):
//! the official `@zed-industries/claude-code-acp@0.16.2` bridge speaks ACP v1 over
//! newline-delimited JSON-RPC on stdio; wire methods `initialize`, `session/new`,
//! `session/prompt`, `session/cancel`; `session/update` notifications stream during a
//! turn and arrive, in order, BEFORE the `session/prompt` response on the one stdout
//! stream — which is exactly why the SC-5 single reader naturally drains a turn's
//! trailing updates before the terminal `StopReason` (release timing, SC-2b).

use std::time::Duration;

/// A turn-attributable id (LB#5): assigned by the SC-1 queue at ENQUEUE and returned
/// from `inject` at SEND time; the correlation key the terminal `StopReason` releases
/// the queue slot by (SC-2b). The real client maps it to the `session/prompt` JSON-RPC
/// request id so the correlated response can be matched back.
pub type TurnId = String;

/// An ACP session id (the `session/new` response's `sessionId`).
pub type SessionId = String;

/// ACP terminal stop reason — the wire enum of THIS bridge's ACP schema
/// (`@agentclientprotocol/sdk .../schema/types.gen.d.ts:2395`:
/// `end_turn | max_tokens | max_turn_requests | refusal | cancelled`).
///
/// **STEP-0 / R8 finding (primary, `acp-agent.js:341-405`):** the claude-code-acp bridge
/// EMITS only `end_turn`, `max_turn_requests`, and `cancelled`. `refusal` is schema-valid
/// but the bridge NEVER produces it (a Claude refusal arrives as ordinary
/// `agent_message_chunk` text and still terminates `end_turn`); `max_tokens` is likewise
/// not emitted by this bridge today. The `Refusal`/`MaxTokens` arms are kept for
/// forward-compat and other ACP agents, but CC refusal-handling MUST NOT be gated on
/// `Refusal` (it is dead against CC). Hard failures arrive as a JSON-RPC ERROR response
/// (authRequired / internalError), surfaced as [`AcpEvent::TerminalError`], NOT a `StopReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

impl StopReason {
    /// Parse the ACP wire string. Unknown → `None` (the caller treats an unmappable
    /// terminal as a degraded/failed turn rather than a clean completion).
    pub fn parse(s: &str) -> Option<StopReason> {
        match s {
            "end_turn" => Some(StopReason::EndTurn),
            "max_tokens" => Some(StopReason::MaxTokens),
            "max_turn_requests" => Some(StopReason::MaxTurnRequests),
            "refusal" => Some(StopReason::Refusal),
            "cancelled" => Some(StopReason::Cancelled),
            _ => None,
        }
    }

    /// The ACP wire string (round-trips with [`StopReason::parse`]).
    pub fn as_wire(&self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::MaxTurnRequests => "max_turn_requests",
            StopReason::Refusal => "refusal",
            StopReason::Cancelled => "cancelled",
        }
    }
}

/// SC-2a terminal REASON classification. Carried by [`super::completion::AcpTurnCompletion`]
/// and asserted by CONF-EVENT (§3) — deliberately NOT folded into the shared
/// [`crate::wait::TurnCompletionProbe`] (Visible/Pending/Degraded), so the codex provider
/// and the `wait.rs` contract stay byte-identical (SC-2a: "carried in the turn-record /
/// the probe, NOT forced into events.rs"; F2.1 byte-identity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReason {
    Completed,
    Refusal,
    MaxTokens,
    Cancelled,
    Failed,
    Crashed,
    TransportLost,
}

/// SC-2a completion CONFIDENCE. For ACP every protocol terminal is `ProtocolConfirmed`
/// (the `StopReason`/JSON-RPC response is the bridge's authoritative signal — never a
/// screen-scrape); a lost transport is `TransportLost`/`ProtocolConfirmed`-absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    ProtocolConfirmed,
    LocalProcessExit,
    Heuristic,
}

impl StopReason {
    /// Map the ACP `StopReason` to the SC-2a [`TerminalReason`]. `EndTurn` is the clean
    /// completion; `MaxTurnRequests`/`MaxTokens` are model-limit terminals; `Cancelled`
    /// is the interrupt terminal; `Refusal` is forward-compat (never from CC).
    pub fn to_terminal_reason(self) -> TerminalReason {
        match self {
            StopReason::EndTurn => TerminalReason::Completed,
            StopReason::MaxTokens | StopReason::MaxTurnRequests => TerminalReason::MaxTokens,
            StopReason::Refusal => TerminalReason::Refusal,
            StopReason::Cancelled => TerminalReason::Cancelled,
        }
    }
}

/// One normalized event off the SC-5 single-reader demux bus. The connection-owning task
/// reads the raw `session/update` notifications and `session/prompt` responses from the ONE
/// stdout stream and republishes them here; all consumers (the `TurnCompletion` probe, the
/// status sink, the queue-release, the Pete's-view transcript tee) read THIS, never the raw
/// transport (SC-5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpEvent {
    /// A `session/update` notification — the started/streaming half of SC-2 (SC-2c) AND the
    /// Pete's-view feed (the transcript tee subscribes here). `kind` is the
    /// `update.sessionUpdate` discriminator (`agent_message_chunk`, `agent_thought_chunk`,
    /// `tool_call`, `tool_call_update`, `plan`, `available_commands_update`,
    /// `current_mode_update`, …). `payload` is the raw `update` object.
    Update {
        session: SessionId,
        kind: String,
        payload: serde_json::Value,
    },
    /// The terminal: the `session/prompt` response for `turn` carried `stop` — the REAL
    /// terminal (SC-2/SC-2a). Released the queue slot for `turn` AFTER all of this turn's
    /// trailing `Update`s were drained (guaranteed by stream order, SC-2b).
    Terminal {
        session: SessionId,
        turn: TurnId,
        stop: StopReason,
    },
    /// The `session/prompt` request for `turn` failed at the JSON-RPC layer (authRequired /
    /// internalError). Turn-terminal `Failed` (R8) — distinct from a `StopReason` completion;
    /// releases the queue slot and surfaces the error.
    TerminalError {
        session: SessionId,
        turn: TurnId,
        message: String,
    },
}

/// Transport/protocol failure for the [`AcpClient`] contract (the ACP analog of
/// `provider/codex/rpc.rs`'s `RpcError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpError {
    /// The connection closed / the bridge child exited.
    Closed,
    /// A transport-level failure (write failed, framing error, spawn failed).
    Transport(String),
    /// A protocol-level failure (a JSON-RPC error to `initialize`/`session/new`, a
    /// malformed response).
    Protocol(String),
    /// The SC-1 bounded outbound queue is full (flow-control backpressure, SC-1a) — the
    /// provider maps this to `InjectError::Precondition("queue full")`, DISTINCT from the
    /// SC-3 busy-refusal ban. The one legitimate rejection of an `inject`.
    QueueFull,
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::Closed => write!(f, "acp connection closed"),
            AcpError::Transport(s) => write!(f, "acp transport error: {s}"),
            AcpError::Protocol(s) => write!(f, "acp protocol error: {s}"),
            AcpError::QueueFull => write!(f, "acp outbound queue full"),
        }
    }
}

impl std::error::Error for AcpError {}

/// The result of the `initialize` handshake — the bridge's advertised capabilities. Kept
/// minimal (the verb/provider layer only needs to know the handshake SUCCEEDED and whether
/// auth is required); the full `agentCapabilities` object is carried raw for callers that want it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializeResult {
    pub protocol_version: i64,
    /// `agentInfo.name`/`version` — e.g. `@zed-industries/claude-code-acp` `0.16.2`.
    pub agent_name: Option<String>,
    pub agent_version: Option<String>,
    /// True if `authMethods` is non-empty AND no usable credential is present — the caller
    /// surfaces an auth blocker rather than driving a turn that will `authRequired`.
    pub auth_required: bool,
}

/// The ACP transport contract. Mirrors `AppServerRpc`'s `new_session`/`prompt`/`cancel`/
/// `next_update` shape (the A2 §A3-ACP named contract), `&self` so a SHARED `&dyn AcpClient`
/// out of `ProviderFx` is callable.
pub trait AcpClient {
    /// `initialize` handshake (ACP v1). Sends `clientCapabilities`/`clientInfo`, returns the
    /// agent's [`InitializeResult`].
    fn initialize(&self) -> Result<InitializeResult, AcpError>;

    /// `session/new` `{cwd, mcpServers:[]}` → the new `sessionId`.
    fn new_session(&self, cwd: &str) -> Result<SessionId, AcpError>;

    /// The inject-facing send (LB#5): ENQUEUE `text` on the host's long-lived SC-1 queue for
    /// `session` and return the attributable [`TurnId`] IMMEDIATELY (does NOT block for the
    /// terminal `StopReason`). The host releases the queued item as `session/prompt` when the
    /// session is turn-idle (SC-1), maps the turn id to the JSON-RPC request id, and the
    /// correlated response surfaces as [`AcpEvent::Terminal`]`{turn,..}` via [`Self::next_update`].
    /// `from` carries the peer attribution. A 2nd send mid-turn QUEUES (SC-3); a bounded-overflow
    /// → `Err`(mapped to `InjectError::Precondition("queue full")` at the provider, SC-1a).
    ///
    /// `on_dispatched` (Child B, opencode D1 exactly-once guard) is called the moment this
    /// turn's bytes are confirmed handed to the transport — for [`AcpConnection`](super::wire::AcpConnection)
    /// that is right after the ws `send()` succeeds, BEFORE the reply is read. A caller durably
    /// persists its "structured send issued" marker from this callback so a socket drop between
    /// dispatch and reply-read still leaves the correct history for the next process to read
    /// (never gated on this call's `Ok` return alone — a crash right after `send()` must not
    /// leave a false never-sent record in the row's durable wire-history). In-process hosts
    /// with no dispatch/reply split
    /// (no ambiguous partial-completion) may call it immediately or ignore it.
    fn prompt(
        &self,
        session: &str,
        text: &str,
        from: &str,
        on_dispatched: &dyn Fn(),
    ) -> Result<TurnId, AcpError>;

    /// `session/cancel` `{sessionId}` (notification) — best-effort interrupt of the in-flight
    /// turn (SC-1a). The bridge resolves the in-flight `prompt` with `cancelled`; a wedged
    /// bridge may ignore it (the guaranteed unblock is transport teardown, SC-1a).
    fn cancel(&self, session: &str) -> Result<(), AcpError>;

    /// SC-5 single-reader demux pull: the next normalized [`AcpEvent`] off the bus, blocking
    /// up to `timeout`. `Ok(None)` = the deadline elapsed with no event (a quiet stream is
    /// normal — NOT an error). `Err(AcpError::Closed)` = the connection closed with the bus empty.
    fn next_update(&self, timeout: Duration) -> Result<Option<AcpEvent>, AcpError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_wire_roundtrips_all_variants() {
        for r in [
            StopReason::EndTurn,
            StopReason::MaxTokens,
            StopReason::MaxTurnRequests,
            StopReason::Refusal,
            StopReason::Cancelled,
        ] {
            assert_eq!(StopReason::parse(r.as_wire()), Some(r), "round-trip {r:?}");
        }
        assert_eq!(StopReason::parse("bogus"), None, "unknown wire → None");
    }

    #[test]
    fn r8_terminal_reason_mapping_is_explicit() {
        // The mapping the adapter relies on — pinned so a drift reds.
        assert_eq!(StopReason::EndTurn.to_terminal_reason(), TerminalReason::Completed);
        assert_eq!(StopReason::Cancelled.to_terminal_reason(), TerminalReason::Cancelled);
        assert_eq!(StopReason::MaxTurnRequests.to_terminal_reason(), TerminalReason::MaxTokens);
        assert_eq!(StopReason::MaxTokens.to_terminal_reason(), TerminalReason::MaxTokens);
        // Forward-compat only — CC never emits this; do NOT gate CC refusal handling on it.
        assert_eq!(StopReason::Refusal.to_terminal_reason(), TerminalReason::Refusal);
    }
}
