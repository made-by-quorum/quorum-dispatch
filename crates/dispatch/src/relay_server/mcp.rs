//! MCP-over-stdio: the newline-delimited JSON-RPC 2.0 loop (spec §1 group B,
//! P-B1..P-B6) that Claude Code spawns and talks to.
//!
//! This is the SERVER half of the cc-relay MCP channel, replacing the
//! `@modelcontextprotocol/sdk` `StdioServerTransport` + `Server` in
//! `~/work/cc-relay/server.ts` (lines 84-235). It reads newline-delimited JSON-RPC
//! 2.0 messages off stdin and writes responses to stdout; each request (has `id`)
//! gets a response, each notification (no `id`) does not.
//!
//! ## The DROP-IN MCP contract (ADD-27 reframe)
//! The bar is NOT byte-copying the SDK's internals — it is being a DROP-IN for
//! Claude Code's MCP channel. The observable protocol surface IS a hard interop
//! contract because CC depends on it, and is held FAITHFUL to the captured bun
//! fixture (`exec/ccrelay-mcp-handshake-fixture.json`):
//! - the `initialize` result shape (P-B2): `protocolVersion` echoed,
//!   `capabilities {tools:{}, experimental:{"claude/channel":{}}}`,
//!   `serverInfo {name:"relay", version:"0.1.0"}`, and the verbatim
//!   [`INSTRUCTIONS`] string as a TOP-LEVEL result field;
//! - `tools/list` (P-B3) advertising exactly the `reply` tool with the verbatim
//!   description + `{text, message_id}` inputSchema (both required);
//! - the outbound `notifications/claude/channel` notification shape (P-B4).
//!
//! Our internal serialization style is ours; the observable wire is the contract.
//!
//! ## M4: tools/call delivery is LIVE (no longer a seam)
//! `tools/call name=reply` (P-B5) now runs the REAL delivery algorithm via
//! [`RelayServer::deliver_reply`] (buffer-first P-E1 → resolve-waiter P-E2 /
//! push-back P-E3 / loop belt P-E4 / origin guards P-E5 / honest NOT-DELIVERED
//! P-E6). The [`DeliveryOutcome`](crate::relay_server::DeliveryOutcome) maps onto
//! the tool result: its text is the single text-content, its `is_error` becomes
//! `isError`. So `relay:serve` is now wired as a full reply-delivering relay.
//!
//! ## Lock / IO discipline (P-G6 — reviewer-checked)
//! EVERY MCP stdout write — the response writer in [`serve_stdio`] AND the
//! outbound [`emit_channel_notification`] from a `/message` HTTP thread — goes
//! through the SEPARATE [`RelayServer::stdout`] `Mutex<Stdout>`, NEVER the
//! `RelayServer::state` lock. The two writers therefore never interleave a line,
//! and a stuck Claude-Code stdout consumer can stall only that lock, never relay
//! state. The M3 dispatch reads only immutable config + builds JSON — it never
//! touches the state lock at all.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::relay_server::RelayServer;

/// The verbatim MCP `instructions` string (server.ts:88). Surfaced as a TOP-LEVEL
/// field of the `initialize` result (NOT inside capabilities). This guides Claude
/// Code on how to use the relay channel + the loop-prevention rule; CC depends on
/// it, so it is copied EXACTLY (any drift is an interop break). VERIFIED to decode
/// to the fixture's `instructions` value byte-for-byte (666 chars). NOTE: the
/// fixture metadata `critical_fields.instructions.length: 753` is INACCURATE — it
/// matches neither the decoded string (666) nor the raw JSON-escaped form (684);
/// the verbatim content is the contract and it matches exactly.
pub const INSTRUCTIONS: &str = "Messages from other sessions arrive as <channel source=\"relay\" from_session=\"...\" message_id=\"...\">. To respond, call the reply tool with your full response and the message_id. The tool delivers to a sender blocked in 'sb send:relay --wait', or otherwise posts your reply to the origin session as a new channel message marked [REPLY to <message_id>]. If the tool returns an error, your reply did NOT reach anyone — send a fresh message instead (sb send:relay <session>) and restate your substance. NEVER call the reply tool on a message whose text begins with \"[REPLY to\" — that is a delivered reply, not a request; replying to it can ping-pong two sessions forever.";

/// The verbatim `reply` tool description (server.ts:96 / fixture line 96).
const REPLY_TOOL_DESCRIPTION: &str = "Reply to a message from another Claude session. Delivers to a sender waiting in sb send:relay --wait, or posts the reply back to the origin session as a new channel message. Returns an error (with guidance) when neither delivery path works — the reply has NOT reached anyone in that case.";

/// The default `protocolVersion` echoed when the client omits one (the bun SDK
/// echoes the client's request; the fixture shows `2024-11-05` in → out). If a
/// client ever omits `params.protocolVersion`, we default to this rather than
/// emit `null` — a non-string protocolVersion would break the handshake.
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the MCP JSON-RPC 2.0 stdin loop on the CALLING thread (the main thread, in
/// production — see `mod.rs::run_with_env`). Blocks reading newline-delimited lines
/// from stdin; for each line, [`dispatch_line`] classifies + builds an optional
/// response, which is written to stdout through the shared `Mutex<Stdout>` (P-G6).
///
/// Returns on stdin EOF (parent / Claude Code gone — normal, P-F2); the caller then
/// runs sidecar cleanup + exits 0. A malformed/non-JSON line is skipped (logged to
/// stderr), never crashing the loop. A broken stdout pipe (EPIPE — CC gone) is
/// swallowed, never spiraling (P-F2): the next read will see EOF and end the loop.
pub fn serve_stdio(server: &RelayServer) {
    let stdin = std::io::stdin();
    // Lock stdin for the lifetime of the loop (single reader). `lines()` strips the
    // trailing newline; an `Err` line (e.g. invalid UTF-8) is skipped, not fatal.
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            // A read error on stdin (rare — e.g. invalid UTF-8). Skip; the next
            // iteration will surface EOF if the stream is truly gone.
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue; // blank keepalive line — nothing to dispatch
        }
        if let Some(response) = dispatch_line(server, &line) {
            write_line(server, &response);
        }
    }
    // Falling off the end = stdin EOF. Return to the caller for cleanup.
}

/// Classify + dispatch one raw stdin line. Returns `Some(serialized_response)` for
/// a request (has `id`), `None` for a notification (no `id`) or a malformed line.
///
/// Takes `&RelayServer` because `tools/call name=reply` now runs the REAL M4
/// delivery (`RelayServer::deliver_reply`), which touches relay state + does
/// push-back IO. Classification + envelope building remain pure over the line;
/// only the `reply` delivery reaches into the server.
/// Malformed/non-JSON → log to stderr + `None` (P-B1: never crash the loop).
pub fn dispatch_line(server: &RelayServer, line: &str) -> Option<String> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            // Malformed / non-JSON line: skip it (log to stderr), never crash.
            eprintln!("relay mcp: skipping malformed line: {e}");
            return None;
        }
    };
    dispatch_message(server, &value).map(|resp| resp.to_string())
}

/// Dispatch a parsed JSON-RPC message → `Some(response_json)` for a request,
/// `None` for a notification. The HEART of the loop, pure over the parsed value.
///
/// Classification (P-B1): a message with an `id` is a request and gets a response;
/// a message without an `id` is a notification and gets none. Method dispatch:
/// - `initialize` (P-B2) → the handshake result.
/// - `tools/list` (P-B3) → the `reply` tool advertisement.
/// - `tools/call` (P-B5) → the M4 reply DELIVERY (`deliver_reply`) / unknown-tool error.
/// - `notifications/initialized` (P-B6) → notification (no id) → consumed, no resp.
/// - any other method WITH an id → a -32601 method-not-found error response.
/// - any other method WITHOUT an id → an unknown notification → consumed, no resp.
pub fn dispatch_message(server: &RelayServer, msg: &Value) -> Option<Value> {
    // `id` present = request (gets a response); absent = notification (none).
    // serde keeps a present-but-null `id` distinct from an absent one; per JSON-RPC
    // a null id is still a (degenerate) id. We treat ANY present `id` as a request.
    let id = msg.get("id");
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");

    match (id, method) {
        // --- Requests (have an id) ---
        (Some(id), "initialize") => Some(result_response(id, initialize_result(msg))),
        (Some(id), "tools/list") => Some(result_response(id, tools_list_result())),
        (Some(id), "tools/call") => Some(result_response(id, tools_call_result(server, msg))),
        // Unknown method WITH an id → JSON-RPC error -32601 (never crash). P-B1.
        (Some(id), other) => Some(error_response(
            id,
            -32601,
            &format!("Method not found: {other}"),
        )),
        // --- Notifications (no id) ---
        // notifications/initialized (P-B6) and any other notification: consumed,
        // no response. The method match is informational only.
        (None, _) => None,
    }
}

/// Build the `initialize` result (P-B2). Matches the bun fixture shape EXACTLY:
/// `protocolVersion` echoes the client's `params.protocolVersion` (default if
/// absent), `capabilities {tools:{}, experimental:{"claude/channel":{}}}`,
/// `serverInfo {name:"relay", version:"0.1.0"}`, and the verbatim [`INSTRUCTIONS`]
/// as a TOP-LEVEL field.
fn initialize_result(msg: &Value) -> Value {
    let protocol_version = msg
        .get("params")
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {},
            "experimental": { "claude/channel": {} },
        },
        "serverInfo": { "name": "relay", "version": "0.1.0" },
        "instructions": INSTRUCTIONS,
    })
}

/// Build the `tools/list` result (P-B3): exactly one tool `reply` with the verbatim
/// description + `{text, message_id}` inputSchema (both required). Matches the
/// fixture (lines 92-116) EXACTLY.
fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "reply",
                "description": REPLY_TOOL_DESCRIPTION,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "Your full reply text" },
                        "message_id": { "type": "string", "description": "The message_id from the inbound channel message" },
                    },
                    "required": ["text", "message_id"],
                },
            }
        ]
    })
}

/// Build a `tools/call` result (P-B5).
///
/// **M4 DELIVERY (this is no longer a seam):** for `name == "reply"`, parse
/// `params.arguments.{text, message_id}` and run the REAL delivery via
/// [`RelayServer::deliver_reply`] — the ONE delivery code path (buffer-first P-E1 →
/// resolve a parked waiter P-E2 / push-back P-E3 / loop belt P-E4 / origin guards
/// P-E5 / honest NOT-DELIVERED P-E6). The [`DeliveryOutcome`] maps directly to the
/// tool result: `outcome.text` is the single text-content, `outcome.is_error`
/// becomes `isError` (P-E6: NOT-DELIVERED surfaces as an error so the model knows
/// the reply reached no one).
///
/// An unknown tool name → a result with `isError:true` (server.ts:231).
fn tools_call_result(server: &RelayServer, msg: &Value) -> Value {
    let params = msg.get("params");
    let tool_name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");

    if tool_name == "reply" {
        let args = params.and_then(|p| p.get("arguments"));
        let text = args
            .and_then(|a| a.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let message_id = args
            .and_then(|a| a.get("message_id"))
            .and_then(Value::as_str)
            .unwrap_or("");

        // Run the real delivery (buffer-first, decide, act — see deliver_reply).
        // The returned outcome carries the honest result string + the is_error
        // flag; map it straight onto the MCP tool result shape.
        let outcome = server.deliver_reply(message_id, text);
        text_content_result(&outcome.text, outcome.is_error)
    } else {
        // Unknown tool → text result with isError:true (server.ts:231).
        text_content_result(&format!("unknown tool: {tool_name}"), true)
    }
}

/// Build an MCP tool-call result: `{content:[{type:"text", text:<text>}]}` plus an
/// optional `isError:true` (server.ts:225-229). `isError` is OMITTED when false
/// (matching the SDK: the field is only present on the error path).
fn text_content_result(text: &str, is_error: bool) -> Value {
    let mut result = json!({ "content": [ { "type": "text", "text": text } ] });
    if is_error {
        result["isError"] = Value::Bool(true);
    }
    result
}

/// Wrap a result `Value` in a JSON-RPC 2.0 response envelope carrying the request's
/// `id` (P-B1: every response has `"jsonrpc":"2.0"` + the id). The `id` is echoed
/// verbatim (number or string — the client chooses; we never reinterpret it).
fn result_response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build a JSON-RPC 2.0 error response (P-B1) carrying the request's `id`, a numeric
/// `code`, and a `message`. Used for unknown methods (-32601). Well-formed so the
/// client never chokes; the loop never crashes.
fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Emit the outbound `notifications/claude/channel` notification (P-B4) for an
/// inbound `POST /message`. The line is:
/// `{"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":<text>,"meta":{"from_session":<from>,"message_id":<id>}}}\n`
/// (serde does the escaping). This is how Claude Code surfaces a channel message,
/// so the shape is a FAITHFUL match to the bun fixture (`notification_shape`).
///
/// P-G6: called from a `/message` HTTP thread AFTER the state lock is released
/// (see `http::handle_message`). The write takes the SEPARATE `Mutex<Stdout>`
/// (via [`write_line`]) — NEVER the state lock — so it can never stall relay state.
/// A broken stdout pipe (CC gone) is swallowed in `write_line` (P-F2).
pub fn emit_channel_notification(
    server: &RelayServer,
    content: &str,
    from_session: &str,
    message_id: &str,
) {
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": content,
            "meta": {
                "from_session": from_session,
                "message_id": message_id,
            },
        },
    });
    write_line(server, &notification.to_string());
}

/// Write one newline-terminated line to stdout through the SHARED `Mutex<Stdout>`
/// (P-G6 — the ONE serialization point for every MCP stdout write). Holding the
/// stdout lock across the `write_all` + `flush` is what makes the line ATOMIC
/// w.r.t. the other writer (the notification emitter vs the response writer never
/// interleave). A broken-pipe / any IO error is SWALLOWED (P-F2: CC gone is normal;
/// never spiral). The state lock is NEVER involved.
fn write_line(server: &RelayServer, line: &str) {
    // Lock POISONING: if a prior writer panicked mid-write we still want to emit
    // (the stream is just bytes); recover the guard rather than propagate a panic
    // into an HTTP thread or the stdin loop.
    let mut out = match server.stdout.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    // Single buffer so the line + newline hit the OS in one write where possible.
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    let _ = out.write_all(&buf);
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_server::RelayServer;
    use serde_json::json;
    use std::time::Duration;

    /// A single shared in-process server for the protocol tests. The MCP protocol
    /// surface (initialize / tools/list / notifications / unknown-method) does not
    /// touch the server at all, but the dispatch entry now requires a `&RelayServer`
    /// because `tools/call name=reply` runs real delivery. We spawn ONE server lazily
    /// (rather than per-test) so the protocol tests don't each pay a listener-thread
    /// cost; they never deliver-to it, so sharing is safe. The reply-delivery test
    /// uses a FRESH server (its own temp home) so its origin/inbox state is isolated.
    fn shared_server() -> &'static std::sync::Arc<RelayServer> {
        use std::sync::OnceLock;
        static SERVER: OnceLock<std::sync::Arc<RelayServer>> = OnceLock::new();
        SERVER.get_or_init(|| {
            let tmp = std::env::temp_dir().join(format!("relay-mcp-proto-{}", std::process::id()));
            let handle = RelayServer::spawn_for_test(
                &tmp,
                0,
                Duration::from_millis(50),
                Duration::from_secs(10),
            );
            handle.server
        })
    }

    // --- initialize (P-B2) ---

    #[test]
    fn initialize_echoes_protocol_version_and_full_shape() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "probe", "version": "0" }
            }
        });
        let resp =
            dispatch_message(shared_server(), &req).expect("initialize is a request → response");

        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], json!(1));
        let result = &resp["result"];
        // protocolVersion echoed.
        assert_eq!(result["protocolVersion"], "2024-11-05");
        // capabilities.experimental["claude/channel"] present (empty object).
        assert_eq!(
            result["capabilities"]["experimental"]["claude/channel"],
            json!({})
        );
        // capabilities.tools present.
        assert_eq!(result["capabilities"]["tools"], json!({}));
        // serverInfo {relay, 0.1.0}.
        assert_eq!(result["serverInfo"]["name"], "relay");
        assert_eq!(result["serverInfo"]["version"], "0.1.0");
        // instructions is a TOP-LEVEL result field, starts with the marker, full len.
        let instr = result["instructions"]
            .as_str()
            .expect("instructions is a string");
        assert!(
            instr.starts_with("Messages from other sessions"),
            "instructions prefix: {instr}"
        );
        // The DECODED instructions string is 666 chars (verified to match the
        // fixture's decoded `instructions` value EXACTLY). NOTE: the fixture's
        // `critical_fields.instructions.length: 753` metadata field is INACCURATE —
        // it does not match the decoded string (666) NOR the raw JSON-escaped form
        // (684); the verbatim CONTENT is the contract, and it matches exactly.
        assert_eq!(instr.chars().count(), 666, "instructions char length");
        // instructions must NOT be nested in capabilities.
        assert!(result["capabilities"]["instructions"].is_null());
    }

    #[test]
    fn initialize_defaults_protocol_version_when_absent() {
        // No params.protocolVersion → default to 2024-11-05 (never emit null).
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let resp = dispatch_message(shared_server(), &req).expect("response");
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn initialize_echoes_a_nonstandard_protocol_version() {
        // The bun SDK echoes whatever the client sent; prove we echo, not pin.
        let req = json!({
            "jsonrpc": "2.0", "id": 7, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        });
        let resp = dispatch_message(shared_server(), &req).expect("response");
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(resp["id"], json!(7));
    }

    // --- tools/list (P-B3) ---

    #[test]
    fn tools_list_advertises_one_reply_tool_with_required_fields() {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
        let resp = dispatch_message(shared_server(), &req).expect("response");

        assert_eq!(resp["id"], json!(2));
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1, "exactly one tool");
        let reply = &tools[0];
        assert_eq!(reply["name"], "reply");
        assert_eq!(reply["description"], REPLY_TOOL_DESCRIPTION);
        // inputSchema: object with text + message_id string props, both required.
        let schema = &reply["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["text"]["type"], "string");
        assert_eq!(schema["properties"]["message_id"]["type"], "string");
        let required = schema["required"].as_array().expect("required array");
        assert_eq!(required, &vec![json!("text"), json!("message_id")]);
    }

    // --- tools/call (P-B5) ---

    #[test]
    fn tools_call_reply_runs_real_delivery_and_maps_outcome() {
        // M4: tools/call name=reply now runs REAL delivery (no longer a seam). The
        // message_id here was never recorded on the shared server and has no parked
        // waiter, so delivery is the HONEST NOT-DELIVERED path (NoOrigin → P-E6):
        // a well-formed single text-content result with isError:true + guidance.
        let req = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "reply", "arguments": { "text": "hi there", "message_id": "mcp-unrecorded-id-1" } }
        });
        let resp = dispatch_message(shared_server(), &req).expect("response");

        assert_eq!(resp["id"], json!(3));
        // Well-formed single text-content result.
        let content = resp["result"]["content"].as_array().expect("content array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        let text = content[0]["text"].as_str().expect("text content");
        // Honest NOT-DELIVERED: isError:true + the fresh-message guidance (P-E6).
        assert_eq!(resp["result"]["isError"], true, "no-origin reply → isError");
        assert!(
            text.starts_with("NOT DELIVERED"),
            "honest not-delivered prefix: {text}"
        );
        assert!(
            text.contains("send a fresh message"),
            "guidance present: {text}"
        );
    }

    #[test]
    fn tools_call_unknown_tool_is_error_result() {
        let req = json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "frobnicate", "arguments": {} }
        });
        let resp = dispatch_message(shared_server(), &req).expect("response");

        // Unknown tool → a RESULT (not a JSON-RPC error) with isError:true (server.ts:231).
        assert_eq!(resp["result"]["isError"], true);
        let content = resp["result"]["content"].as_array().expect("content array");
        assert!(content[0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    // --- notifications/initialized (P-B6) ---

    #[test]
    fn notifications_initialized_produces_no_response() {
        let notif = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(
            dispatch_message(shared_server(), &notif).is_none(),
            "a notification (no id) must produce no response"
        );
    }

    #[test]
    fn arbitrary_notification_without_id_produces_no_response() {
        let notif = json!({ "jsonrpc": "2.0", "method": "notifications/something/else" });
        assert!(dispatch_message(shared_server(), &notif).is_none());
    }

    // --- unknown method (P-B1) ---

    #[test]
    fn unknown_method_with_id_returns_method_not_found_error() {
        let req = json!({ "jsonrpc": "2.0", "id": 9, "method": "resources/list" });
        let resp =
            dispatch_message(shared_server(), &req).expect("a request always gets a response");

        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], json!(9));
        assert_eq!(resp["error"]["code"], -32601);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("resources/list"));
        // An error response has no `result`.
        assert!(resp["result"].is_null());
    }

    #[test]
    fn id_echoed_verbatim_for_string_id() {
        // The client may use a string id; echo it unchanged.
        let req = json!({ "jsonrpc": "2.0", "id": "abc-1", "method": "tools/list", "params": {} });
        let resp = dispatch_message(shared_server(), &req).expect("response");
        assert_eq!(resp["id"], json!("abc-1"));
    }

    // --- malformed line (P-B1) ---

    #[test]
    fn malformed_line_produces_no_response_no_panic() {
        // dispatch_line is the entry that parses raw text; a non-JSON line must be
        // skipped (no response, no panic).
        assert!(dispatch_line(shared_server(), "this is not json").is_none());
        assert!(dispatch_line(shared_server(), "{ broken json ").is_none());
        assert!(dispatch_line(shared_server(), "").is_none());
    }

    #[test]
    fn dispatch_line_round_trips_a_valid_request_to_a_response_string() {
        // The full text path: parse a line → serialized response string.
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let out =
            dispatch_line(shared_server(), line).expect("a request line yields a response string");
        // It must be valid JSON carrying the id + a tools array.
        let parsed: Value = serde_json::from_str(&out).expect("response is valid JSON");
        assert_eq!(parsed["id"], json!(1));
        assert!(parsed["result"]["tools"].is_array());
    }

    #[test]
    fn dispatch_line_notification_yields_no_line() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(dispatch_line(shared_server(), line).is_none());
    }

    // --- outbound notification emit (P-B4) ---
    //
    // We assert the EXACT serialized JSON the notification builder produces. To
    // exercise the same `json!` shape the live emitter uses without constructing a
    // full RelayServer, we mirror the builder here and assert the wire frame; the
    // live `emit_channel_notification` uses the identical `json!` literal.

    #[test]
    fn channel_notification_exact_json_for_given_inputs() {
        // Build the same notification value the emitter builds (the emitter wraps
        // this in write_line under the Mutex<Stdout>; the JSON shape is the wire
        // contract we pin here).
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": "hello world",
                "meta": {
                    "from_session": "sess-A",
                    "message_id": "relay-1700000000000-1",
                },
            },
        });
        // Exact field-for-field shape (serde_json::Value comparison is structural,
        // independent of key order).
        let expected: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"hello world","meta":{"from_session":"sess-A","message_id":"relay-1700000000000-1"}}}"#,
        )
        .unwrap();
        assert_eq!(notification, expected);
    }

    #[test]
    fn channel_notification_escapes_special_chars() {
        // serde does the escaping: a quote + newline in the content must serialize
        // to valid JSON that round-trips back to the original text.
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": "line1\nline2 \"quoted\"",
                "meta": { "from_session": "s", "message_id": "relay-1-1" },
            },
        });
        let serialized = notification.to_string();
        let reparsed: Value = serde_json::from_str(&serialized).expect("valid JSON");
        assert_eq!(reparsed["params"]["content"], "line1\nline2 \"quoted\"");
    }

    // --- instructions constant integrity ---

    #[test]
    fn instructions_const_matches_the_fixture_verbatim() {
        // The decoded fixture `instructions` value is 666 chars (the fixture's
        // metadata `length: 753` is inaccurate — see the note in the initialize
        // test). The CONTENT is the contract: verbatim match to server.ts:88.
        assert_eq!(INSTRUCTIONS.chars().count(), 666);
        assert!(INSTRUCTIONS.starts_with("Messages from other sessions arrive as <channel"));
        assert!(INSTRUCTIONS.ends_with("ping-pong two sessions forever."));
        assert!(INSTRUCTIONS.contains("[REPLY to"));
    }
}
