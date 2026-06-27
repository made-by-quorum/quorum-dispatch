//! The codex app-server RPC transport CONTRACT (codex-p2-spec sections 3.1,
//! 6.2, ADD-5 pattern: the contract stays the contract; [`super::ws`] is the
//! driver, fixtures drive every offline test).
//!
//! **Wire law (codex-p2-spec section 1.3 + the q1c spike evidence).** The
//! app-server speaks a JSON-RPC-*shaped* protocol over a single ws frame per
//! message, but the frames in the wire evidence DO NOT carry a
//! `jsonrpc:"2.0"` field — they are bare `{"id",..,"result"|"error"}` /
//! `{"method",..,"params"}` objects. We REPLICATE WHAT THE EVIDENCE SHOWS, not
//! textbook JSON-RPC: outbound requests omit `jsonrpc`, inbound frames are
//! classified by which keys are present. Every envelope below is permissive
//! (`#[serde(default)]` / unknown fields ignored) and documents the evidence
//! line it mirrors.
//!
//! Evidence files (exec/codex-spike-evidence/jail/):
//!   - q1c-clientA-events.jsonl — full turn lifecycle, two completed turns.
//!   - q1c-clientB-events.jsonl — co-injection client: the stale-`expectedTurnId`
//!     structured ERROR (line 12) + the `no rollout found` resume error (line 4).
//!   - appserver-events.jsonl   — stdio session (initialize/thread/start/turn/start).
//!
//! Request shapes come from the spike probe scripts (appserver-probe.py,
//! q1-coinjection.py), which are the client side those events answered.

use serde::{Deserialize, Serialize};

// ===========================================================================
// Typed errors (codex-p2-spec section 6.2).
// ===========================================================================

/// A parsed structured server error (the `error` object on the wire).
///
/// Mirrors q1c-clientB-events.jsonl line 12:
/// `{"error":{"code":-32600,"message":"expected active turn id `...` but found
/// `...`"},"id":5}` and line 4:
/// `{"error":{"code":-32600,"message":"no rollout found for thread id ..."},
/// "id":2}`. Schema: JSONRPCErrorError.json — `code` is an int64, `message` a
/// string, `data` optional/arbitrary (we ignore `data`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ServerError {
    pub code: i64,
    pub message: String,
}

/// Transport-level + protocol-level failure classes the evidence supports
/// (codex-p2-spec section 6.2).
#[derive(Debug)]
pub enum RpcError {
    /// The ws transport itself failed (connect, send, or a non-text/malformed
    /// frame). Carries a human string; the real driver wraps the tungstenite
    /// error text here.
    Transport(String),
    /// A structured `{"error":{...}}` frame from the server. Carries the parsed
    /// [`ServerError`]; the stale-steer fence (code -32600 "expected active turn
    /// id …") arrives this way.
    Protocol(ServerError),
    /// A read deadline elapsed before the correlated response arrived (the
    /// socket read timeout fired). Distinct from a clean close.
    Timeout,
    /// The connection was closed (server sent a Close frame or the socket hit
    /// EOF) before the correlated response arrived.
    Closed,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Transport(s) => write!(f, "transport error: {s}"),
            RpcError::Protocol(e) => write!(f, "protocol error {}: {}", e.code, e.message),
            RpcError::Timeout => write!(f, "rpc timed out waiting for a response"),
            RpcError::Closed => write!(f, "connection closed before a response arrived"),
        }
    }
}

impl std::error::Error for RpcError {}

/// The stale-`expectedTurnId` fence code observed on the wire
/// (q1c-clientB-events.jsonl line 12 + line 4 both use -32600). codex reuses
/// the JSON-RPC "invalid request" code for the precondition fence; `turn_steer`
/// maps this code to [`StaleTurn`](SteerOutcome::StaleTurn) by CODE ONLY —
/// deliberately NOT by message text (churn posture: the fence wording can move
/// across 0.x minors; the code is the stable part). A non-fence -32600 on steer
/// therefore also maps to StaleTurn, which is SELF-CORRECTING: the W6 ladder's
/// `turn/start` fallback surfaces the real error on the very next call.
pub const INVALID_REQUEST_CODE: i64 = -32600;

// ===========================================================================
// Request param envelopes — fields ONLY what we send (permissive).
// ===========================================================================

/// `initialize` params (appserver-probe.py:43 / q1-coinjection.py:53):
/// `{"clientInfo":{"name":..,"title":..,"version":..}}`. Schema:
/// v1/InitializeParams.json `ClientInfo` (name+version required, title nullable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

/// `initialize` result (q1c-clientA-events.jsonl line 1):
/// `{"userAgent":..,"codexHome":..,"platformFamily":..,"platformOs":..}`. We
/// consume only `codexHome` (the rest is diagnostic); unknown fields ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct InitializeResult {
    #[serde(rename = "codexHome")]
    pub codex_home: String,
    #[serde(rename = "userAgent")]
    pub user_agent: String,
}

/// The minimal thread descriptor we read out of `thread/start` /
/// `thread/started` (q1c-clientA-events.jsonl lines 3-4): we consume only the
/// `id` and the rollout `path`; everything else (sessionId, status, cwd,
/// cliVersion, turns, …) is ignored permissively.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct ThreadDescriptor {
    pub id: String,
    /// The rollout JSONL path (date+uuid keyed, NO cwd-slug). Empty when the
    /// frame omits it.
    pub path: String,
}

/// `thread/start` result (q1c-clientA-events.jsonl line 3):
/// `{"thread":{"id":..,"path":..,..},"model":..,"cwd":..,..}`. We read the
/// nested `thread` only. Schema: v2/ThreadStartResponse.json (`thread` required).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct ThreadStartResult {
    pub thread: ThreadDescriptor,
}

/// `turn/start` / `turn/steer` result — TWO wire shapes, both observed:
///   - q1c-clientA-events.jsonl line 5 / appserver-events.jsonl line 5:
///     `{"turn":{"id":..,"status":"inProgress",..}}` (schema TurnStartResponse).
///   - q1c-clientB-events.jsonl line 11: `{"turnId":".."}` (schema
///     TurnSteerResponse shape returned for a co-injected turn/start too).
///
/// We accept EITHER and normalize to the turn id via [`TurnResult::turn_id`].
/// This is a NAMED schema-vs-wire divergence (see module docs / the W2 report).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct TurnResult {
    /// Present in the nested-`turn` shape (clientA / stdio).
    pub turn: Option<TurnDescriptor>,
    /// Present in the flat `turnId` shape (clientB co-injection).
    #[serde(rename = "turnId")]
    pub turn_id_flat: Option<String>,
}

/// The nested turn descriptor (q1c-clientA-events.jsonl line 5): we read `id`
/// only; status/items/timestamps ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct TurnDescriptor {
    pub id: String,
}

impl TurnResult {
    /// The turn id, from whichever of the two wire shapes the server used.
    /// Returns `None` if neither shape carried an id (malformed result).
    pub fn turn_id(&self) -> Option<&str> {
        if let Some(id) = self.turn_id_flat.as_deref() {
            if !id.is_empty() {
                return Some(id);
            }
        }
        self.turn
            .as_ref()
            .map(|t| t.id.as_str())
            .filter(|s| !s.is_empty())
    }
}

/// A parsed inbound NOTIFICATION (a frame with `method` + `params`, no `id`):
/// `{"method":"thread/status/changed","params":{..}}` (q1c-clientA-events.jsonl
/// line 7, 19, …). `params` is kept raw so the status-mapping layer
/// ([`crate::provider::codex`] / `parse_status`) can interpret it without this
/// transport binding the notification taxonomy.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: serde_json::Value,
}

/// The outcome of a `turn/steer`: either the steer landed (returns the turn id)
/// or the server's `expectedTurnId` fence rejected it as stale. The send ladder
/// (W6) falls back to `turn/start` on [`StaleTurn`](SteerOutcome::StaleTurn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerOutcome {
    /// The steer was accepted; carries the (still-active) turn id.
    Steered(String),
    /// The `expectedTurnId` precondition failed — the active turn moved on. The
    /// server error is preserved (q1c-clientB-events.jsonl line 12).
    StaleTurn(ServerError),
}

// ===========================================================================
// The contract (codex-p2-spec section 6.2): sync / blocking, one in-flight
// request at a time.
// ===========================================================================

/// The codex app-server transport contract. Sync/blocking: every method drives
/// one request → one correlated response (or a deadline). The real driver is
/// [`super::ws::WsAppServer`]; conformance + every offline test uses a fixture
/// impl.
///
/// `&self` + interior mutability (W3 refactor, codex-p2-spec section 6.2): the
/// contract is consumed through a SHARED `&dyn AppServerRpc` out of
/// [`crate::provider::ProviderFx::app_server`] — the verb layer connects, then
/// hands the trait object to `CodexProvider::{boot_waiter, inject}`, which take
/// `&ProviderFx` (a shared borrow). A `&mut self` method is uncallable through a
/// shared trait object, so the methods take `&self` and the real driver wraps its
/// mutable state (socket + id counter + notification buffer) in a `RefCell`
/// ([`super::ws::WsAppServer`]). The driver is SINGLE-THREADED / `!Sync` by
/// design — one in-flight request at a time, no concurrent callers (the engine is
/// a short-lived sync CLI; every verb reconnects, does its RPC, disconnects).
pub trait AppServerRpc {
    /// `initialize` handshake (readiness). Sends `clientInfo`, returns the
    /// server's [`InitializeResult`]. The spike follows it with a bare
    /// `{"method":"initialized"}` notification — [`Self::initialized`] sends it.
    fn initialize(&self, client: &ClientInfo) -> Result<InitializeResult, RpcError>;

    /// The post-initialize `{"method":"initialized"}` notification (no response).
    fn initialized(&self) -> Result<(), RpcError>;

    /// `thread/start` (appserver-probe.py:47): `{"cwd":..,"approvalPolicy":..,
    /// "sandbox":..}`. Returns the new thread id. `approval_policy` and
    /// `sandbox` are passed through verbatim (the launch path picks the values —
    /// codex-p2-spec section 3.3; W2 does not encode the full-bypass choice).
    fn thread_start(
        &self,
        cwd: &str,
        approval_policy: &str,
        sandbox: &str,
    ) -> Result<String, RpcError>;

    /// `thread/resume` (q1-coinjection.py:62): `{"threadId":..}`. Hydrates a
    /// thread from its on-disk rollout. Surfaces the `no rollout found` server
    /// error (q1c-clientB-events.jsonl line 4) as [`RpcError::Protocol`].
    fn thread_resume(&self, thread_id: &str) -> Result<(), RpcError>;

    /// `turn/start` (appserver-probe.py:52): `{"threadId":..,"input":[{"type":
    /// "text","text":..}]}`. Returns the new turn id (normalized across both
    /// result shapes — see [`TurnResult`]).
    fn turn_start(&self, thread_id: &str, text: &str) -> Result<String, RpcError>;

    /// `turn/steer` (q1-coinjection.py:93): `{"threadId":..,"expectedTurnId":..,
    /// "input":[..]}`. Returns a [`SteerOutcome`] distinguishing a landed steer
    /// from the stale-`expectedTurnId` fence (the typed StalePrecondition — a
    /// `Protocol` error with the fence code is mapped to
    /// [`SteerOutcome::StaleTurn`], NOT bubbled as an error).
    fn turn_steer(
        &self,
        thread_id: &str,
        expected_turn_id: &str,
        text: &str,
    ) -> Result<SteerOutcome, RpcError>;

    /// `turn/interrupt` (schema TurnInterruptParams): `{"threadId":..,"turnId":
    /// ..}`. Cancels an in-flight turn.
    fn turn_interrupt(&self, thread_id: &str, turn_id: &str) -> Result<(), RpcError>;

    /// Pull the next buffered NOTIFICATION (FIFO), blocking up to `timeout`.
    /// Returns `Ok(None)` if the deadline elapses with no notification (NOT a
    /// [`RpcError::Timeout`] — a quiet socket is normal). Returns
    /// [`RpcError::Closed`] if the connection closed with the buffer empty.
    fn next_notification(
        &self,
        timeout: std::time::Duration,
    ) -> Result<Option<Notification>, RpcError>;

    /// Close the transport (best-effort Close frame + socket teardown).
    fn close(&self) -> Result<(), RpcError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // === Envelope serde round-trips ===

    #[test]
    fn client_info_serializes_omitting_none_title() {
        let ci = ClientInfo {
            name: "qd-manager".into(),
            title: None,
            version: "0".into(),
        };
        let v = serde_json::to_value(&ci).unwrap();
        // Matches q1-coinjection.py:53 minus the title the probe DID send;
        // None title must be ABSENT (skip_serializing_if), not null.
        assert_eq!(v, serde_json::json!({"name":"qd-manager","version":"0"}));
        assert!(v.get("title").is_none());
    }

    #[test]
    fn client_info_with_title_round_trips() {
        let ci = ClientInfo {
            name: "qd-probe".into(),
            title: Some("qd probe".into()),
            version: "0.0.1".into(),
        };
        let v = serde_json::to_value(&ci).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"name":"qd-probe","title":"qd probe","version":"0.0.1"})
        );
    }

    // === REAL evidence lines parsed (cited inline) ===

    // q1c-clientA-events.jsonl line 1 (the initialize RESULT payload).
    const EV_INITIALIZE_RESULT: &str = r#"{"userAgent":"qd-manager/0.134.0 (Mac OS 26.5.0; arm64) dumb (qd-manager; 0)","codexHome":"/jail/codex-home","platformFamily":"unix","platformOs":"macos"}"#;

    #[test]
    fn parses_real_initialize_result() {
        let r: InitializeResult = serde_json::from_str(EV_INITIALIZE_RESULT).unwrap();
        assert_eq!(r.codex_home, "/jail/codex-home");
        assert!(r.user_agent.starts_with("qd-manager/0.134.0"));
    }

    // q1c-clientA-events.jsonl line 3 (the thread/start RESULT payload, trimmed
    // to the fields we consume + a few we deliberately ignore to prove
    // permissiveness).
    const EV_THREAD_START_RESULT: &str = r#"{"thread":{"id":"019e9f4b-adb9-7ec1-b4ed-08247847426a","sessionId":"019e9f4b-adb9-7ec1-b4ed-08247847426a","status":{"type":"idle"},"path":"/jail/codex-home/sessions/2026/06/06/rollout-2026-06-06T19-36-37-019e9f4b-adb9-7ec1-b4ed-08247847426a.jsonl","cwd":"/jail/work","cliVersion":"0.134.0","turns":[]},"model":"openai/gpt-4o-mini","approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}}"#;

    #[test]
    fn parses_real_thread_start_result() {
        let r: ThreadStartResult = serde_json::from_str(EV_THREAD_START_RESULT).unwrap();
        assert_eq!(r.thread.id, "019e9f4b-adb9-7ec1-b4ed-08247847426a");
        assert!(r
            .thread
            .path
            .ends_with("rollout-2026-06-06T19-36-37-019e9f4b-adb9-7ec1-b4ed-08247847426a.jsonl"));
    }

    // q1c-clientA-events.jsonl line 5 (turn/start RESULT, nested-`turn` shape).
    const EV_TURN_START_NESTED: &str = r#"{"turn":{"id":"019e9f4b-ae50-7ee3-a7eb-80c366198453","items":[],"itemsView":"notLoaded","status":"inProgress","error":null,"startedAt":null,"completedAt":null,"durationMs":null}}"#;
    // q1c-clientB-events.jsonl line 11 (turn/start RESULT, FLAT `turnId` shape).
    const EV_TURN_START_FLAT: &str = r#"{"turnId":"019e9f4c-8e65-7fa3-ae9d-9978019abd17"}"#;

    #[test]
    fn parses_both_turn_result_shapes_to_one_id() {
        let nested: TurnResult = serde_json::from_str(EV_TURN_START_NESTED).unwrap();
        assert_eq!(
            nested.turn_id(),
            Some("019e9f4b-ae50-7ee3-a7eb-80c366198453")
        );
        let flat: TurnResult = serde_json::from_str(EV_TURN_START_FLAT).unwrap();
        assert_eq!(flat.turn_id(), Some("019e9f4c-8e65-7fa3-ae9d-9978019abd17"));
    }

    #[test]
    fn empty_turn_result_has_no_id() {
        let empty: TurnResult = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.turn_id(), None);
    }

    // q1c-clientB-events.jsonl line 12 — the stale-`expectedTurnId` structured
    // ERROR (the centerpiece of the steer fence). The full wire frame including
    // the `id`; we parse the `error` object out of it.
    const EV_STALE_STEER_FRAME: &str = r#"{"error":{"code":-32600,"message":"expected active turn id `019e0000-0000-7000-8000-000000000000` but found `019e9f4c-8e65-7fa3-ae9d-9978019abd17`"},"id":5}"#;

    #[test]
    fn parses_real_stale_steer_error() {
        let frame: serde_json::Value = serde_json::from_str(EV_STALE_STEER_FRAME).unwrap();
        let err: ServerError = serde_json::from_value(frame["error"].clone()).unwrap();
        assert_eq!(err.code, INVALID_REQUEST_CODE);
        assert!(err.message.contains("expected active turn id"));
        assert!(err.message.contains("but found"));
    }

    // q1c-clientB-events.jsonl line 4 — the `no rollout found` resume error.
    const EV_NO_ROLLOUT_FRAME: &str = r#"{"error":{"code":-32600,"message":"no rollout found for thread id 019e9f4b-adb9-7ec1-b4ed-08247847426a"},"id":2}"#;

    #[test]
    fn parses_real_no_rollout_resume_error() {
        let frame: serde_json::Value = serde_json::from_str(EV_NO_ROLLOUT_FRAME).unwrap();
        let err: ServerError = serde_json::from_value(frame["error"].clone()).unwrap();
        assert_eq!(err.code, INVALID_REQUEST_CODE);
        assert!(err.message.starts_with("no rollout found"));
    }
}
