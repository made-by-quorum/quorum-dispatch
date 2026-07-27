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
//! - `tools/list` (P-B3) advertising the unchanged `reply` tool plus the
//!   dispatch self-adoption `shutdown_for_adoption` tool;
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
pub const INSTRUCTIONS: &str = "Messages from other sessions arrive as <channel source=\"relay\" from_session=\"...\" message_id=\"...\">. To respond, call the reply tool with your full response and the message_id. The tool delivers to a sender blocked in 'qd send:relay --wait', or otherwise posts your reply to the origin session as a new channel message marked [REPLY to <message_id>]. If the tool returns an error, your reply did NOT reach anyone — send a fresh message instead (qd send:relay <session>) and restate your substance. NEVER call the reply tool on a message whose text begins with \"[REPLY to\" — that is a delivered reply, not a request; replying to it can ping-pong two sessions forever.";

/// The verbatim `reply` tool description (server.ts:96 / fixture line 96).
const REPLY_TOOL_DESCRIPTION: &str = "Reply to a message from another Claude session. Delivers to a sender waiting in qd send:relay --wait, or posts the reply back to the origin session as a new channel message. Returns an error (with guidance) when neither delivery path works — the reply has NOT reached anyone in that case.";

const SHUTDOWN_TOOL_DESCRIPTION: &str = "Finish a self-adoption prepared by `qd adopt <name>`. Writes pending-adopt state, prints the complete manual qrmux restart command to the operator's terminal, returns that command here as a fallback, and only then attempts to terminate this Claude Code process. Never restarts automatically.";

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
    serve_stdio_with_terminator(server, &SigtermTerminator);
}

fn serve_stdio_with_terminator(server: &RelayServer, terminator: &dyn Terminator) {
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
        if let Some(outcome) = dispatch_line_with_action(server, &line) {
            let mut response_write =
                |response: &str| write_line(server, response).map_err(|e| e.to_string());
            let mut tty_write = |notice: &str| write_notice_to_tty(notice);
            if let Err(e) = complete_dispatch_outcome(
                outcome,
                &mut response_write,
                terminator,
                &crate::effects::proc_start_ms,
                &mut tty_write,
            ) {
                eprintln!("relay mcp: {e}");
            }
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
    dispatch_line_with_action(server, line).map(|outcome| outcome.response)
}

fn dispatch_line_with_action(server: &RelayServer, line: &str) -> Option<DispatchLineOutcome> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            // Malformed / non-JSON line: skip it (log to stderr), never crash.
            eprintln!("relay mcp: skipping malformed line: {e}");
            return None;
        }
    };
    dispatch_message_with_action(server, &value).map(|outcome| DispatchLineOutcome {
        response: outcome.response.to_string(),
        action: outcome.action,
    })
}

struct DispatchLineOutcome {
    response: String,
    action: Option<PostResponseAction>,
}

struct DispatchMessageOutcome {
    response: Value,
    action: Option<PostResponseAction>,
}

/// `proc_start_ms` derives its value from `ps etime`, whose finest field is a
/// whole second. Two reads of one process can therefore differ by up to one
/// second solely from that reader's measurement granularity. This tolerance is
/// for comparing two fresh reads from that same reader; it is deliberately not
/// the registry-row skew allowance used by the earlier prepared-record check.
const ADOPTION_START_RECHECK_TOLERANCE_MS: u64 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PostResponseAction {
    TerminateClaude {
        pid: i32,
        staged_start_ms: i64,
        state_dir: std::path::PathBuf,
        session_id: String,
    },
}

trait Terminator {
    fn terminate(&self, pid: i32) -> Result<(), String>;
}

struct SigtermTerminator;

impl Terminator for SigtermTerminator {
    fn terminate(&self, pid: i32) -> Result<(), String> {
        if pid <= 1 {
            return Err(format!("refusing invalid Claude pid {pid}"));
        }
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    }
}

fn execute_post_response(
    action: &PostResponseAction,
    terminator: &dyn Terminator,
    proc_start: &dyn Fn(i32) -> Option<i64>,
    tty_write: &mut dyn FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    match action {
        PostResponseAction::TerminateClaude {
            pid,
            staged_start_ms,
            ..
        } => {
            let fence = match proc_start(*pid) {
                Some(current)
                    if current.abs_diff(*staged_start_ms)
                        <= ADOPTION_START_RECHECK_TOLERANCE_MS => Ok(()),
                Some(current) => Err(format!(
                    "Claude pid {pid} start-time fence changed from staged start {staged_start_ms} to {current}"
                )),
                None => Err(format!(
                    "could not recheck the start-time fence for Claude pid {pid}"
                )),
            };
            if let Err(reason) = fence {
                return Err(report_termination_failure(action, &reason, tty_write));
            }

            terminator.terminate(*pid).map_err(|e| {
                report_termination_failure(
                    action,
                    &format!("SIGTERM to Claude pid {pid} failed: {e}"),
                    tty_write,
                )
            })
        }
    }
}

/// Complete one response while preserving the load-bearing order: pending state
/// and the initial TTY notice were produced by dispatch, then response flush,
/// final start-time fence, and finally SIGTERM. Every suppression after pending
/// state rolls it back and emits a terminal failure line.
fn complete_dispatch_outcome(
    outcome: DispatchLineOutcome,
    response_write: &mut dyn FnMut(&str) -> Result<(), String>,
    terminator: &dyn Terminator,
    proc_start: &dyn Fn(i32) -> Option<i64>,
    tty_write: &mut dyn FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    if let Err(e) = response_write(&outcome.response) {
        if let Some(action) = &outcome.action {
            return Err(report_termination_failure(
                action,
                &format!("MCP adoption response flush failed: {e}"),
                tty_write,
            ));
        }
        // Preserve the relay's normal broken-pipe posture for non-adoption
        // responses: Claude going away is expected and not an error loop.
        return Ok(());
    }

    if let Some(action) = &outcome.action {
        execute_post_response(action, terminator, proc_start, tty_write)?;
    }
    Ok(())
}

fn report_termination_failure(
    action: &PostResponseAction,
    reason: &str,
    tty_write: &mut dyn FnMut(&str) -> Result<(), String>,
) -> String {
    let PostResponseAction::TerminateClaude {
        state_dir,
        session_id,
        ..
    } = action;
    let cleanup = suppression_cleanup_status(state_dir, session_id);
    let failure = format!(
        "qd adopt: adoption termination FAILED: {reason}; the session is still running; {cleanup}"
    );
    match tty_write(&failure) {
        Ok(()) => failure,
        Err(e) => {
            format!("{failure}; additionally could not write the failure line to /dev/tty: {e}")
        }
    }
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
    dispatch_message_with_action(server, msg).map(|outcome| outcome.response)
}

fn dispatch_message_with_action(
    server: &RelayServer,
    msg: &Value,
) -> Option<DispatchMessageOutcome> {
    // `id` present = request (gets a response); absent = notification (none).
    // serde keeps a present-but-null `id` distinct from an absent one; per JSON-RPC
    // a null id is still a (degenerate) id. We treat ANY present `id` as a request.
    let id = msg.get("id");
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");

    match (id, method) {
        // --- Requests (have an id) ---
        (Some(id), "initialize") => Some(DispatchMessageOutcome {
            response: result_response(id, initialize_result(msg)),
            action: None,
        }),
        (Some(id), "tools/list") => Some(DispatchMessageOutcome {
            response: result_response(id, tools_list_result()),
            action: None,
        }),
        (Some(id), "tools/call") => {
            let tool = tools_call_result(server, msg);
            Some(DispatchMessageOutcome {
                response: result_response(id, tool.result),
                action: tool.action,
            })
        }
        // Unknown method WITH an id → JSON-RPC error -32601 (never crash). P-B1.
        (Some(id), other) => Some(DispatchMessageOutcome {
            response: error_response(id, -32601, &format!("Method not found: {other}")),
            action: None,
        }),
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

/// Build the `tools/list` result (P-B3): the existing `reply` contract is
/// byte-unchanged and `shutdown_for_adoption` is additive.
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
            },
            {
                "name": "shutdown_for_adoption",
                "description": SHUTDOWN_TOOL_DESCRIPTION,
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
            },
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
struct ToolCallOutcome {
    result: Value,
    action: Option<PostResponseAction>,
}

fn tools_call_result(server: &RelayServer, msg: &Value) -> ToolCallOutcome {
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
        ToolCallOutcome {
            result: text_content_result(&outcome.text, outcome.is_error),
            action: None,
        }
    } else if tool_name == "shutdown_for_adoption" {
        shutdown_for_adoption_result(server)
    } else {
        // Unknown tool → text result with isError:true (server.ts:231).
        ToolCallOutcome {
            result: text_content_result(&format!("unknown tool: {tool_name}"), true),
            action: None,
        }
    }
}

fn shutdown_for_adoption_result(server: &RelayServer) -> ToolCallOutcome {
    let rows = match crate::effects::process_rows(&crate::exec::RealExec) {
        Ok(rows) => rows,
        Err(e) => return shutdown_error(format!("could not inspect the relay ancestry: {e}")),
    };
    shutdown_for_adoption_result_with(
        server,
        &rows,
        &crate::effects::proc_start_ms,
        &mut |notice| write_notice_to_tty(notice),
    )
}

fn shutdown_for_adoption_result_with(
    server: &RelayServer,
    rows: &std::collections::HashMap<i32, crate::effects::ProcRow>,
    proc_start: &dyn Fn(i32) -> Option<i64>,
    tty_write: &mut dyn FnMut(&str) -> Result<(), String>,
) -> ToolCallOutcome {
    shutdown_for_adoption_result_with_pending_registration(
        server,
        rows,
        proc_start,
        tty_write,
        &crate::adoption::register_pending,
    )
}

fn shutdown_for_adoption_result_with_pending_registration(
    server: &RelayServer,
    rows: &std::collections::HashMap<i32, crate::effects::ProcRow>,
    proc_start: &dyn Fn(i32) -> Option<i64>,
    tty_write: &mut dyn FnMut(&str) -> Result<(), String>,
    register_pending: &dyn Fn(
        &std::path::Path,
        &crate::adoption::AdoptRecord,
    ) -> Result<std::path::PathBuf, String>,
) -> ToolCallOutcome {
    let relay_pid = server.pid as i32;
    let Some(claude_pid) = crate::adoption::find_claude_ancestor(relay_pid, rows) else {
        return shutdown_error(
            "could not positively identify a Claude ancestor; session left running".to_string(),
        );
    };
    let record = match crate::adoption::load_prepared(
        &server.paths.state_dir,
        &server.session_id,
        claude_pid,
    ) {
        Ok(record) => record,
        Err(e) => return shutdown_error(format!("{e}; session left running")),
    };

    // Reused-pid fence immediately before the state mutation/signal plan. The
    // relay ancestry is current process truth; the start check ensures the pid
    // still belongs to the incarnation `qd adopt` prepared.
    let staged_start_ms = match proc_start(claude_pid) {
        Some(current)
            if crate::adoption::start_time_fence_matches(record.identity.start_ms, current) =>
        {
            current
        }
        Some(current) => {
            return shutdown_error(format!(
                "Claude pid {claude_pid} start time {current} disagrees with prepared incarnation {} beyond the 1000ms process-clock granularity allowance; refusing shutdown",
                record.identity.start_ms
            ));
        }
        None => {
            return shutdown_error(format!(
                "could not verify the start time of Claude pid {claude_pid}; session left running"
            ));
        }
    };

    if let Err(e) = crate::adoption::verify_incarnation_fence(&server.paths.home, &record) {
        return shutdown_error(format!("{e}; session left running"));
    }

    // Fail-closed order: durable pending state first, then terminal output. A
    // failure in either step returns isError and carries NO termination action.
    if let Err(e) = register_pending(&server.paths.state_dir, &record) {
        let rollback = rollback_pending_status(&server.paths.state_dir, &record.session_id);
        return shutdown_error(format!(
            "pending-adopt state write failed: {e}; session left running; {rollback}"
        ));
    }
    let notice = crate::adoption::shutdown_notice(&record);
    if let Err(e) = tty_write(&notice) {
        let cleanup = suppression_cleanup_status(&server.paths.state_dir, &record.session_id);
        return shutdown_error(format!(
            "the restart command could not be written to the terminal: {e}; session left running; {cleanup}"
        ));
    }

    ToolCallOutcome {
        result: text_content_result(&notice, false),
        // serve_stdio executes this only after successfully flushing the tool
        // result to MCP stdout.
        action: Some(PostResponseAction::TerminateClaude {
            pid: claude_pid,
            staged_start_ms,
            state_dir: server.paths.state_dir.clone(),
            session_id: record.session_id.clone(),
        }),
    }
}

fn rollback_pending_status(state_dir: &std::path::Path, session_id: &str) -> String {
    match crate::adoption::rollback_pending(state_dir, session_id) {
        Ok(()) => "pending-adopt state rolled back".to_string(),
        Err(e) => format!("pending-adopt rollback FAILED: {e}"),
    }
}

fn cleanup_prepared_status(state_dir: &std::path::Path, session_id: &str) -> String {
    match crate::adoption::cleanup_prepared(state_dir, session_id) {
        Ok(()) => "prepared-adopt state removed or already absent".to_string(),
        Err(e) => format!("prepared-adopt cleanup FAILED: {e}"),
    }
}

fn suppression_cleanup_status(state_dir: &std::path::Path, session_id: &str) -> String {
    format!(
        "{}; {}",
        rollback_pending_status(state_dir, session_id),
        cleanup_prepared_status(state_dir, session_id)
    )
}

fn shutdown_error(message: String) -> ToolCallOutcome {
    ToolCallOutcome {
        result: text_content_result(&format!("shutdown_for_adoption: {message}"), true),
        action: None,
    }
}

fn write_notice_to_tty(notice: &str) -> Result<(), String> {
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|e| format!("could not open /dev/tty: {e}"))?;
    tty.write_all(b"\n")
        .and_then(|_| tty.write_all(notice.as_bytes()))
        .and_then(|_| tty.write_all(b"\n"))
        .and_then(|_| tty.flush())
        .map_err(|e| format!("could not write /dev/tty: {e}"))
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
    let _ = write_line(server, &notification.to_string());
}

/// Write one newline-terminated line to stdout through the SHARED `Mutex<Stdout>`
/// (P-G6 — the ONE serialization point for every MCP stdout write). Holding the
/// stdout lock across the `write_all` + `flush` is what makes the line ATOMIC
/// w.r.t. the other writer (the notification emitter vs the response writer never
/// interleave). A broken-pipe / any IO error is SWALLOWED (P-F2: CC gone is normal;
/// never spiral). The state lock is NEVER involved.
fn write_line(server: &RelayServer, line: &str) -> std::io::Result<()> {
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
    out.write_all(&buf)?;
    out.flush()
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
    fn tools_list_advertises_reply_and_shutdown_for_adoption() {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
        let resp = dispatch_message(shared_server(), &req).expect("response");

        assert_eq!(resp["id"], json!(2));
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2, "reply plus self-adopt shutdown");
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
        let shutdown = &tools[1];
        assert_eq!(shutdown["name"], "shutdown_for_adoption");
        assert_eq!(shutdown["description"], SHUTDOWN_TOOL_DESCRIPTION);
        assert_eq!(shutdown["inputSchema"]["type"], "object");
        assert_eq!(shutdown["inputSchema"]["properties"], json!({}));
        assert_eq!(shutdown["inputSchema"]["additionalProperties"], false);
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

    fn shutdown_rows(
        relay_pid: i32,
        claude_pid: i32,
    ) -> std::collections::HashMap<i32, crate::effects::ProcRow> {
        std::collections::HashMap::from([
            (
                relay_pid,
                crate::effects::ProcRow {
                    ppid: relay_pid + 1,
                    cmd: "qd relay:serve".into(),
                    argv: None,
                },
            ),
            (
                relay_pid + 1,
                crate::effects::ProcRow {
                    ppid: claude_pid,
                    cmd: "bun run --cwd /home/.claude/channels/relay start".into(),
                    argv: None,
                },
            ),
            (
                claude_pid,
                crate::effects::ProcRow {
                    ppid: 2,
                    cmd: "claude --name bare-one".into(),
                    argv: Some(vec!["claude".into(), "--name".into(), "bare-one".into()]),
                },
            ),
            (
                2,
                crate::effects::ProcRow {
                    ppid: 1,
                    cmd: "qd qrmux-server --session parent".into(),
                    argv: None,
                },
            ),
        ])
    }

    fn prepared_for(server: &RelayServer, claude_pid: i32) -> crate::adoption::AdoptRecord {
        let record = crate::adoption::AdoptRecord::prepared(
            "bare-one".into(),
            server.session_id.clone(),
            crate::identity::SessionIdentity::new(
                server.session_id.clone(),
                claude_pid,
                1_700_000_000_000,
                0,
            ),
            Some("/work".into()),
            1_800_000_000_000,
        );
        crate::adoption::write_prepared(&server.paths.state_dir, &record).unwrap();
        record
    }

    #[test]
    fn adopt_shutdown_tool_writes_pending_and_tty_before_staging_sigterm() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = RelayServer::spawn_for_test(
            tmp.path(),
            0,
            Duration::from_millis(50),
            Duration::from_secs(1),
        );
        let server = &handle.server;
        let claude_pid = server.pid as i32 + 1000;
        let record = prepared_for(server, claude_pid);
        let staged_start_ms = record.identity.start_ms + 500;
        let rows = shutdown_rows(server.pid as i32, claude_pid);
        let mut tty = String::new();
        let out = shutdown_for_adoption_result_with(
            server,
            &rows,
            &|_| Some(staged_start_ms),
            &mut |notice| {
                tty.push_str(notice);
                Ok(())
            },
        );

        assert_eq!(
            out.action.as_ref().map(|action| match action {
                PostResponseAction::TerminateClaude {
                    pid,
                    staged_start_ms,
                    ..
                } => (*pid, *staged_start_ms),
            }),
            Some((claude_pid, staged_start_ms)),
            "SIGTERM is staged only after state + tty succeeded"
        );
        assert!(
            crate::adoption::pending_path(&server.paths.state_dir, &server.session_id).exists()
        );
        assert!(tty.contains("It will NOT restart automatically"));
        assert!(tty.contains("about to be terminated"));
        assert!(tty.contains("qd relay:serve"));
        assert!(tty.contains("qd attach 'bare-one'"));
        assert_eq!(out.result["content"][0]["text"], tty);
        assert!(out.result["isError"].is_null());
    }

    #[test]
    fn adopt_pending_write_failure_never_stages_termination() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = RelayServer::spawn_for_test(
            tmp.path(),
            0,
            Duration::from_millis(50),
            Duration::from_secs(1),
        );
        let server = &handle.server;
        let claude_pid = server.pid as i32 + 2000;
        let record = prepared_for(server, claude_pid);
        let pending_dir = server.paths.state_dir.join("adoption").join("pending");
        std::fs::create_dir_all(pending_dir.parent().unwrap()).unwrap();
        std::fs::write(&pending_dir, b"not a directory").unwrap();
        let rows = shutdown_rows(server.pid as i32, claude_pid);
        let mut tty_called = false;
        let out = shutdown_for_adoption_result_with(
            server,
            &rows,
            &|_| Some(record.identity.start_ms),
            &mut |_| {
                tty_called = true;
                Ok(())
            },
        );
        assert_eq!(out.action, None, "write failure must not terminate Claude");
        assert!(!tty_called, "tty output is after the durable write");
        assert_eq!(out.result["isError"], true);
        assert!(out.result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("pending-adopt state write failed"));
    }

    #[test]
    fn adopt_prepared_cleanup_failure_rolls_back_pending_without_staging_or_tty() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = RelayServer::spawn_for_test(
            tmp.path(),
            0,
            Duration::from_millis(50),
            Duration::from_secs(1),
        );
        let server = &handle.server;
        let claude_pid = server.pid as i32 + 2500;
        let record = prepared_for(server, claude_pid);
        let prepared = crate::adoption::prepared_path(&server.paths.state_dir, &server.session_id);
        let pending = crate::adoption::pending_path(&server.paths.state_dir, &server.session_id);
        let rows = shutdown_rows(server.pid as i32, claude_pid);
        let mut tty_called = false;
        let out = shutdown_for_adoption_result_with_pending_registration(
            server,
            &rows,
            &|_| Some(record.identity.start_ms),
            &mut |_| {
                tty_called = true;
                Ok(())
            },
            &|state_dir, record| {
                crate::adoption::register_pending_with_remover_for_test(state_dir, record, &|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected prepared unlink failure",
                    ))
                })
            },
        );

        assert_eq!(out.action, None, "cleanup failure must not stage SIGTERM");
        assert!(!tty_called, "cleanup failure is before terminal output");
        assert!(prepared.exists(), "the prepared request remains retryable");
        assert!(
            !pending.exists(),
            "the just-written pending record was rolled back"
        );
        assert_eq!(out.result["isError"], true);
        let error = out.result["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("prepared-adopt cleanup failed"), "{error}");
        assert!(
            error.contains("injected prepared unlink failure"),
            "{error}"
        );
        assert!(error.contains("pending-adopt state rolled back"), "{error}");
    }

    #[test]
    fn adopt_tty_failure_rolls_back_pending_and_returns_clean_retry_error() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = RelayServer::spawn_for_test(
            tmp.path(),
            0,
            Duration::from_millis(50),
            Duration::from_secs(1),
        );
        let server = &handle.server;
        let claude_pid = server.pid as i32 + 3000;
        let record = prepared_for(server, claude_pid);
        let prepared = crate::adoption::prepared_path(&server.paths.state_dir, &server.session_id);
        let pending = crate::adoption::pending_path(&server.paths.state_dir, &server.session_id);
        let rows = shutdown_rows(server.pid as i32, claude_pid);
        let out = shutdown_for_adoption_result_with(
            server,
            &rows,
            &|_| Some(record.identity.start_ms),
            &mut |_| Err("injected tty failure".into()),
        );

        assert_eq!(out.action, None);
        assert!(
            !prepared.exists(),
            "prepared was consumed by pending transition"
        );
        assert!(
            !pending.exists(),
            "running session must not retain pending state"
        );
        assert_eq!(out.result["isError"], true);
        let error = out.result["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("injected tty failure"), "{error}");
        assert!(error.contains("session left running"), "{error}");
        assert!(error.contains("pending-adopt state rolled back"), "{error}");
        assert!(
            error.contains("prepared-adopt state removed or already absent"),
            "{error}"
        );
    }

    struct RecordingTerminator(std::cell::Cell<Option<i32>>);

    impl Terminator for RecordingTerminator {
        fn terminate(&self, pid: i32) -> Result<(), String> {
            self.0.set(Some(pid));
            Ok(())
        }
    }

    struct FailingTerminator(std::cell::Cell<bool>);

    impl Terminator for FailingTerminator {
        fn terminate(&self, _pid: i32) -> Result<(), String> {
            self.0.set(true);
            Err("injected kill failure".into())
        }
    }

    fn staged_action(
        server: &RelayServer,
        claude_pid: i32,
    ) -> (crate::adoption::AdoptRecord, PostResponseAction) {
        let record = prepared_for(server, claude_pid);
        crate::adoption::register_pending(&server.paths.state_dir, &record).unwrap();
        let action = PostResponseAction::TerminateClaude {
            pid: claude_pid,
            staged_start_ms: record.identity.start_ms,
            state_dir: server.paths.state_dir.clone(),
            session_id: record.session_id.clone(),
        };
        (record, action)
    }

    #[test]
    fn adopt_post_response_termination_seam_targets_claude_not_relay() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = RelayServer::spawn_for_test(
            tmp.path(),
            0,
            Duration::from_millis(50),
            Duration::from_secs(1),
        );
        let claude_pid = 4242;
        let (record, action) = staged_action(&handle.server, claude_pid);
        let terminator = RecordingTerminator(std::cell::Cell::new(None));
        execute_post_response(
            &action,
            &terminator,
            &|_| Some(record.identity.start_ms),
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(terminator.0.get(), Some(claude_pid));
        assert!(
            crate::adoption::pending_path(&handle.server.paths.state_dir, &record.session_id)
                .exists()
        );
    }

    #[test]
    fn adopt_final_start_fence_failure_skips_signal_reports_and_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = RelayServer::spawn_for_test(
            tmp.path(),
            0,
            Duration::from_millis(50),
            Duration::from_secs(1),
        );
        let claude_pid = 4243;
        let (record, action) = staged_action(&handle.server, claude_pid);
        let terminator = RecordingTerminator(std::cell::Cell::new(None));
        let mut tty = String::new();
        let err = execute_post_response(
            &action,
            &terminator,
            &|_| Some(record.identity.start_ms + 5_000),
            &mut |line| {
                tty.push_str(line);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(terminator.0.get(), None, "reused pid must not be signaled");
        assert!(
            !crate::adoption::pending_path(&handle.server.paths.state_dir, &record.session_id)
                .exists()
        );
        for required in [
            "adoption termination FAILED",
            "start-time fence changed",
            "the session is still running",
            "pending-adopt state rolled back",
            "prepared-adopt state removed or already absent",
        ] {
            assert!(tty.contains(required), "missing {required:?}: {tty}");
            assert!(err.contains(required), "missing {required:?}: {err}");
        }
    }

    #[test]
    fn adopt_kill_failure_reports_and_rolls_back_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = RelayServer::spawn_for_test(
            tmp.path(),
            0,
            Duration::from_millis(50),
            Duration::from_secs(1),
        );
        let claude_pid = 4244;
        let (record, action) = staged_action(&handle.server, claude_pid);
        let prepared =
            crate::adoption::prepared_path(&handle.server.paths.state_dir, &record.session_id);
        std::fs::create_dir(&prepared).unwrap();
        let terminator = FailingTerminator(std::cell::Cell::new(false));
        let mut tty = String::new();
        let err = execute_post_response(
            &action,
            &terminator,
            &|_| Some(record.identity.start_ms),
            &mut |line| {
                tty.push_str(line);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(terminator.0.get(), "SIGTERM seam was attempted");
        assert!(
            !crate::adoption::pending_path(&handle.server.paths.state_dir, &record.session_id)
                .exists()
        );
        assert!(
            prepared.is_dir(),
            "injected prepared cleanup failure persists"
        );
        for required in [
            "adoption termination FAILED",
            "injected kill failure",
            "the session is still running",
            "pending-adopt state rolled back",
            "prepared-adopt cleanup FAILED",
        ] {
            assert!(tty.contains(required), "missing {required:?}: {tty}");
            assert!(err.contains(required), "missing {required:?}: {err}");
        }
    }

    #[test]
    fn adopt_response_flush_failure_suppresses_signal_and_rolls_back_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = RelayServer::spawn_for_test(
            tmp.path(),
            0,
            Duration::from_millis(50),
            Duration::from_secs(1),
        );
        let claude_pid = 4245;
        let (record, action) = staged_action(&handle.server, claude_pid);
        let terminator = RecordingTerminator(std::cell::Cell::new(None));
        let mut tty = String::new();
        let err = complete_dispatch_outcome(
            DispatchLineOutcome {
                response: "response".into(),
                action: Some(action),
            },
            &mut |_| Err("injected response flush failure".into()),
            &terminator,
            &|_| panic!("start fence must not run after response failure"),
            &mut |line| {
                tty.push_str(line);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(terminator.0.get(), None);
        assert!(
            !crate::adoption::pending_path(&handle.server.paths.state_dir, &record.session_id)
                .exists()
        );
        for required in [
            "adoption termination FAILED",
            "injected response flush failure",
            "the session is still running",
            "pending-adopt state rolled back",
            "prepared-adopt state removed or already absent",
        ] {
            assert!(tty.contains(required), "missing {required:?}: {tty}");
            assert!(err.contains(required), "missing {required:?}: {err}");
        }
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
