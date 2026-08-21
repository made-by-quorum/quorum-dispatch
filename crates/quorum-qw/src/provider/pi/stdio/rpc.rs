//! The pi `--mode rpc` stdio-JSONL transport CONTRACT + envelope types
//! (mirrors [`crate::provider::codex::rpc`], the ADD-5 pattern: the contract
//! stays the contract; a real stdio driver + fixtures drive every test).
//!
//! **Wire law (pi 0.80.2, mined from the dist typings — see the WS-A.2 spike
//! map).** pi speaks line-delimited JSON over the child's stdio: one JSON object
//! per `\n`-terminated line on stdin (commands) and stdout (responses AND
//! events, interleaved). There is **no JSON-RPC envelope and ZERO
//! `protocolVersion`** on the wire — frames are bare objects keyed by `type`.
//!
//! Three frame kinds share the stdout stream, demuxed on `type` ([`classify`]):
//!   - a **response** — `{"id"?,"type":"response","command":..,"success":bool,
//!     "data"?|"error"?}`, correlated to a command by the echoed `id`. Note
//!     pi's success/error is a `success` BOOLEAN, and an error carries a bare
//!     `error` STRING (no JSON-RPC `code`) — unlike codex's structured
//!     `{code,message}` (so [`PiRpcError::Protocol`] is a String, not a code).
//!   - an **event** — a bare `{"type":"agent_start"|"message_update"|
//!     "agent_end"|..}` emitted raw (rpc-mode.js does `JSON.stringify(event)`
//!     with no wrapper). These drive the status sink (item 2).
//!   - there is **no JSON "eof"/"shutdown" frame** — EOF is the OS-level stdout
//!     close on `process.exit` (the driver maps that to [`PiRpcError::Closed`] /
//!     a `TurnOutcome`).
//!
//! Every envelope below is permissive (`#[serde(default)]`, unknown fields
//! ignored) — pi's 29-command surface moved 19→29 across 0.x minors, so we read
//! only the fields we consume and tolerate the rest.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ===========================================================================
// Typed errors (mirrors codex `RpcError`, adapted to pi's stringy errors).
// ===========================================================================

/// Transport- + protocol-level failure classes for the pi stdio transport.
#[derive(Debug)]
pub enum PiRpcError {
    /// The stdio pipe itself failed (write to a closed stdin, a malformed/
    /// non-UTF8 line, a broken pipe). Carries the driver's error text.
    Transport(String),
    /// A `{"success":false,"error":"..."}` response from pi. pi errors are a
    /// bare STRING (no JSON-RPC code) — e.g. the `command:"parse"` reply for an
    /// unparseable input line (PA6/PA7 seam: a raw `TypeError` text can leak
    /// here, and `bash` with no command surfaces a literal `undefined`).
    Protocol(String),
    /// A read deadline elapsed before the correlated response arrived.
    Timeout,
    /// pi exited / stdout hit EOF before the correlated response arrived (the
    /// daemon-liveness signal; distinct from a quiet socket).
    Closed,
}

impl std::fmt::Display for PiRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PiRpcError::Transport(s) => write!(f, "pi transport error: {s}"),
            PiRpcError::Protocol(s) => write!(f, "pi protocol error: {s}"),
            PiRpcError::Timeout => write!(f, "pi rpc timed out waiting for a response"),
            PiRpcError::Closed => write!(f, "pi process closed before a response arrived"),
        }
    }
}

impl std::error::Error for PiRpcError {}

// ===========================================================================
// Response envelope (rpc-mode.js:30-38; rpc-types.d.ts:148-346).
// ===========================================================================

/// A parsed inbound RESPONSE frame: `{"id"?,"type":"response","command":..,
/// "success":bool,"data"?,"error"?}`. `data` is kept raw so per-command payloads
/// (e.g. `get_state`'s [`RpcSessionState`]) decode at the call site. Permissive.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RpcResponse {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
    pub success: bool,
    pub data: Option<Value>,
    pub error: Option<String>,
}

/// The `get_state` `data` payload ([`RpcSessionState`], rpc-types.d.ts:134-147).
/// This is the ONLY place `sessionId`/`isStreaming` live — they are NOT on the
/// response envelope. We read the fields the adapter needs; the rest (model,
/// thinkingLevel, steeringMode, …) is ignored permissively.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RpcSessionState {
    /// The session UUID — the adapter's birth-id, bound at boot.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// Live turn state — the native status signal (`true`→Busy, `false`→Idle).
    #[serde(rename = "isStreaming")]
    pub is_streaming: bool,
    /// The computed `<ts>_<uuid>.jsonl` path. Populated IMMEDIATELY (at session
    /// construction) even though the FILE is lazy-written — so it is a poor
    /// "did a turn happen" signal; check on-disk existence separately.
    #[serde(rename = "sessionFile")]
    pub session_file: Option<String>,
    #[serde(rename = "isCompacting")]
    pub is_compacting: bool,
    #[serde(rename = "messageCount")]
    pub message_count: u64,
    #[serde(rename = "pendingMessageCount")]
    pub pending_message_count: u64,
}

/// How a `prompt` is routed when a turn is already streaming (rpc-types.d.ts:13).
/// `Steer` interrupts now; `FollowUp` queues after. Sending a `prompt` with
/// `Some(Steer)` is the option-(i) single-call inject: it starts a fresh turn
/// when idle and steers the open turn when busy — no believed-state needed
/// (contrast codex's `expectedTurnId` ladder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

// ===========================================================================
// Inbound EVENT frames (the raw, un-enveloped stream — pi-agent-core types).
// ===========================================================================

/// A classified inbound stdout frame: either a correlated [`RpcResponse`] or a
/// streaming [`PiEvent`]. The driver demuxes on the `type` field.
#[derive(Debug, Clone)]
pub enum Frame {
    Response(RpcResponse),
    Event(PiEvent),
}

/// The streaming events the status mapper cares about (item 2). Discriminated on
/// the top-level `type`; everything we don't map is [`PiEvent::Other`] (carrying
/// the raw discriminator) so unknown/new event kinds never break the parse.
///
/// Event → [`crate::provider::pi::republish::Republish`] mapping (retained for A2's status
/// poll; OPTION B derives status on-read, so nothing is pushed to a sink):
///   `AgentStart`           → `Republish::Ready`
///   `MessageUpdate`(delta) → `Republish::Event`   (signal-B only, no status move)
///   `AgentEnd`             → `Republish::TurnEnd`  (→ idle)
///   abort / breaker        → `Republish::Breaker`  (see [`AssistantEvent::Error`])
///   stdout EOF             → `Republish::Eof`       (OS-level, not a frame)
#[derive(Debug, Clone)]
pub enum PiEvent {
    /// `{"type":"agent_start"}` — a run begins (bare, no payload).
    AgentStart,
    /// `{"type":"message_update","assistantMessageEvent":{..}}` — the deltas
    /// live in the nested [`AssistantEvent`]; an abort surfaces as
    /// `AssistantEvent::Error{reason:"aborted"}` inside the final update.
    MessageUpdate {
        assistant_event: Option<AssistantEvent>,
    },
    /// `{"type":"agent_end","willRetry":bool,..}` — the LAST event of a run
    /// (an aborted run still ends here). `will_retry` distinguishes an
    /// auto-retry boundary from a true terminal.
    AgentEnd { will_retry: bool },
    /// Any other event kind, carrying its `type` discriminator verbatim.
    Other(String),
}

/// The nested `assistantMessageEvent` (pi-ai AssistantMessageEvent). We read the
/// inner discriminator + the delta text; an `Error{reason:"aborted"}` is the
/// abort/breaker signal.
#[derive(Debug, Clone)]
pub enum AssistantEvent {
    /// `{"type":"text_delta","delta":".."}` — an assistant text chunk.
    TextDelta { delta: String },
    /// `{"type":"done","reason":"stop"|"length"|"toolUse"}`.
    Done { reason: String },
    /// `{"type":"error","reason":"aborted"|"error"}` — the abort/error surface.
    Error { reason: String },
    /// Any other inner kind (start/*_start/*_end/toolcall_delta/…).
    Other(String),
}

/// Classify one parsed stdout frame into a [`Frame`]. `type:"response"` → a
/// correlated [`RpcResponse`]; anything else → a [`PiEvent`] (permissive: an
/// unrecognized event becomes `PiEvent::Other`). Returns `None` only when the
/// value is not a JSON object (a torn/garbage line — the driver skips it).
pub fn classify(v: &Value) -> Option<Frame> {
    let obj = v.as_object()?;
    let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "response" {
        // Permissive: a malformed response still decodes via #[serde(default)].
        let resp: RpcResponse = serde_json::from_value(v.clone()).unwrap_or_default();
        return Some(Frame::Response(resp));
    }
    let event = match kind {
        "agent_start" => PiEvent::AgentStart,
        "agent_end" => PiEvent::AgentEnd {
            will_retry: obj
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "message_update" => PiEvent::MessageUpdate {
            assistant_event: obj
                .get("assistantMessageEvent")
                .map(classify_assistant_event),
        },
        other => PiEvent::Other(other.to_string()),
    };
    Some(Frame::Event(event))
}

/// Classify the nested `assistantMessageEvent` object.
fn classify_assistant_event(v: &Value) -> AssistantEvent {
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text_delta" => AssistantEvent::TextDelta {
            delta: v
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "done" => AssistantEvent::Done {
            reason: v
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "error" => AssistantEvent::Error {
            reason: v
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        other => AssistantEvent::Other(other.to_string()),
    }
}

// ===========================================================================
// The contract (mirrors codex `AppServerRpc`): sync / blocking, &self + interior
// mutability so a SHARED `&dyn PiRpc` out of `ProviderFx::pi_rpc` is callable.
// ===========================================================================

/// The pi stdio-RPC transport contract. Sync/blocking, one in-flight command at
/// a time. The real driver (a future `pi_stdio::PiStdio`, owned by the per-
/// session adapter daemon — item 1) wraps the child's stdin/stdout + an id
/// counter + an event buffer in a `RefCell`; conformance + every offline test
/// uses a fixture impl. `&self` (not `&mut self`) so the daemon can hand the
/// trait object to `PiProvider::{boot_waiter, inject}` through a shared
/// `&ProviderFx` borrow (the codex W3 precedent).
pub trait PiRpc {
    /// `{"type":"get_state"}` → [`RpcSessionState`] (the boot-readiness probe AND
    /// the native status read). Birth-id = `state.session_id`.
    fn get_state(&self) -> Result<RpcSessionState, PiRpcError>;

    /// `{"type":"prompt","message":..,"streamingBehavior"?:..}` — start/route a
    /// turn. Mints the command `id`, returns it as the attributable turn id
    /// (pi's `prompt` response carries NO turn id — the echoed command id IS the
    /// correlation). `Some(StreamingBehavior::Steer)` makes one call
    /// start-if-idle / steer-if-busy (option-(i)).
    fn prompt(
        &self,
        message: &str,
        behavior: Option<StreamingBehavior>,
    ) -> Result<String, PiRpcError>;

    /// `{"type":"steer","message":..}` — interrupt-inject into the open turn.
    fn steer(&self, message: &str) -> Result<(), PiRpcError>;

    /// `{"type":"follow_up","message":..}` — queue for after the current turn.
    fn follow_up(&self, message: &str) -> Result<(), PiRpcError>;

    /// `{"type":"abort"}` — cancel the in-flight turn. A trailing `agent_end`
    /// still arrives (the abort surfaces as `AssistantEvent::Error` first).
    fn abort(&self) -> Result<(), PiRpcError>;

    /// `{"type":"switch_session","sessionPath":..}` — open an existing session
    /// file (the daemon-path resume; the CLI analog is `--session <id>`).
    fn switch_session(&self, session_path: &str) -> Result<(), PiRpcError>;

    /// `{"type":"new_session","parentSession"?:..}` — start a fresh session
    /// (a `Some(parent)` is the fork-like lineage).
    fn new_session(&self, parent: Option<&str>) -> Result<(), PiRpcError>;

    /// Pull the next inbound EVENT (FIFO), blocking up to `timeout`. `Ok(None)`
    /// on a quiet deadline (NOT a `Timeout` — a silent stream is normal between
    /// turns); [`PiRpcError::Closed`] on stdout EOF with the buffer empty (the
    /// daemon-liveness / `Eof` signal).
    fn next_event(&self, timeout: Duration) -> Result<Option<PiEvent>, PiRpcError>;

    /// Close the transport (best-effort: drop stdin, let pi exit).
    fn close(&self) -> Result<(), PiRpcError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real success response (bare success, no data — the prompt/steer/abort
    // shape from rpc-mode.js:30-38).
    const EV_BARE_SUCCESS: &str =
        r#"{"id":"c1","type":"response","command":"prompt","success":true}"#;
    // A real get_state success carrying RpcSessionState in `data`.
    const EV_GET_STATE: &str = r#"{"id":"b","type":"response","command":"get_state","success":true,"data":{"isStreaming":false,"sessionId":"019e-uuid","messageCount":2,"pendingMessageCount":0}}"#;
    // A real error response (stringy error, no code).
    const EV_ERROR: &str = r#"{"type":"response","command":"parse","success":false,"error":"Unexpected token u in JSON"}"#;

    #[test]
    fn classifies_bare_success_response() {
        let v: Value = serde_json::from_str(EV_BARE_SUCCESS).unwrap();
        match classify(&v).unwrap() {
            Frame::Response(r) => {
                assert!(r.success);
                assert_eq!(r.command, "prompt");
                assert_eq!(r.id.as_deref(), Some("c1"));
                assert!(r.data.is_none());
            }
            _ => panic!("expected a response frame"),
        }
    }

    #[test]
    fn get_state_data_decodes_to_session_state() {
        let v: Value = serde_json::from_str(EV_GET_STATE).unwrap();
        let Frame::Response(r) = classify(&v).unwrap() else {
            panic!("expected response")
        };
        let st: RpcSessionState = serde_json::from_value(r.data.unwrap()).unwrap();
        assert_eq!(st.session_id, "019e-uuid");
        assert!(!st.is_streaming);
        assert_eq!(st.message_count, 2);
    }

    #[test]
    fn error_response_carries_a_string_not_a_code() {
        let v: Value = serde_json::from_str(EV_ERROR).unwrap();
        let Frame::Response(r) = classify(&v).unwrap() else {
            panic!("expected response")
        };
        assert!(!r.success);
        assert_eq!(r.error.as_deref(), Some("Unexpected token u in JSON"));
    }

    #[test]
    fn classifies_the_three_sink_events() {
        let start: Value = serde_json::from_str(r#"{"type":"agent_start"}"#).unwrap();
        assert!(matches!(
            classify(&start),
            Some(Frame::Event(PiEvent::AgentStart))
        ));

        let end: Value = serde_json::from_str(r#"{"type":"agent_end","willRetry":false}"#).unwrap();
        assert!(matches!(
            classify(&end),
            Some(Frame::Event(PiEvent::AgentEnd { will_retry: false }))
        ));

        let delta: Value = serde_json::from_str(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hi"}}"#,
        )
        .unwrap();
        match classify(&delta) {
            Some(Frame::Event(PiEvent::MessageUpdate {
                assistant_event: Some(AssistantEvent::TextDelta { delta }),
            })) => assert_eq!(delta, "hi"),
            other => panic!("expected a text_delta message_update, got {other:?}"),
        }
    }

    #[test]
    fn abort_surfaces_as_an_inner_error_event() {
        let v: Value = serde_json::from_str(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"error","reason":"aborted"}}"#,
        )
        .unwrap();
        match classify(&v) {
            Some(Frame::Event(PiEvent::MessageUpdate {
                assistant_event: Some(AssistantEvent::Error { reason }),
            })) => assert_eq!(reason, "aborted"),
            other => panic!("expected an aborted error event, got {other:?}"),
        }
    }

    #[test]
    fn streaming_behavior_serializes_camel_case() {
        assert_eq!(
            serde_json::to_value(StreamingBehavior::Steer).unwrap(),
            serde_json::json!("steer")
        );
        assert_eq!(
            serde_json::to_value(StreamingBehavior::FollowUp).unwrap(),
            serde_json::json!("followUp")
        );
    }

    #[test]
    fn unknown_event_kind_is_other_not_a_panic() {
        let v: Value = serde_json::from_str(r#"{"type":"queue_update","steering":[]}"#).unwrap();
        assert!(matches!(
            classify(&v),
            Some(Frame::Event(PiEvent::Other(k))) if k == "queue_update"
        ));
    }
}
