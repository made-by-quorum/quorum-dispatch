//! §4 B-group MCP-over-stdio harness — the REAL `sb relay:serve` subprocess driven
//! as Claude Code drives it: newline-delimited JSON-RPC 2.0 over stdin/stdout, plus
//! the outbound `notifications/claude/channel` emitted on a `POST /message`.
//!
//! Written by QA (checker), SEPARATE from the M3 implementer. This file ADDS B-group
//! rows only — it NEVER modifies the server. Any divergence from the bun fixture is
//! REPORTED, not fixed.
//!
//! ## Why a real subprocess (not the in-process spawn_for_test)
//! The MCP loop (`mcp::serve_stdio`) reads the process's REAL stdin and writes the
//! REAL stdout — there is no in-process seam for it (the in-process `spawn_for_test`
//! drives ONLY the HTTP half). So every row here spawns `target/debug/sb relay:serve`
//! as a child, pipes JSON-RPC frames into its stdin, and reads response/notification
//! lines off its stdout. The child stdin handle is held ALIVE for the duration of a
//! test (dropping it → EOF → child exits 0 after unlinking its sidecar — that is the
//! clean-shutdown row by itself).
//!
//! ## Interop contract = the captured bun fixture
//! `~/work/ws/switchboard/rust/exec/ccrelay-mcp-handshake-fixture.json`. The MCP
//! surface (initialize result, tools/list, the notification shape, the instructions
//! string) must match what Claude Code depends on. We load that fixture at runtime
//! and assert the live subprocess's frames against it byte-for-byte where the brief
//! calls for it (instructions value, tool description, notification shape).
//!
//! ## No-hang discipline
//! EVERY stdout read is bounded by a reader thread + a `recv_timeout` on OUR side, so
//! a server that never answers FAILS the row loudly instead of hanging the suite.
//!
//! ## P-assertions covered
//! - P-B1: unknown method → -32601; malformed line → no crash, loop keeps serving.
//! - P-B2: initialize handshake shape + protocolVersion echo (incl. non-standard).
//! - P-B3: tools/list = exactly one `reply` tool, required {text, message_id}, desc.
//! - P-B4: POST /message → outbound notifications/claude/channel (the PRIMARY fn).
//! - P-B5: tools/call reply DELIVERY (M4 — honest NOT-DELIVERED for an unrecorded
//!   id, P-E6) + unknown tool isError:true.
//! - P-B6: notifications/initialized emits NO response line.
//! - P-D4/P-F2: clean EOF shutdown → exit 0 + sidecar unlinked.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use dispatch::relay::RelayContract;
use dispatch::relay_http::CcRelay;

/// Per-line read budget. A healthy localhost subprocess answers a request in well
/// under this; if a read blocks past it, the row FAILS (never hangs the suite).
const READ_BUDGET: Duration = Duration::from_secs(5);

/// A high port base OUTSIDE the 8900-9000 jail band (sb's own probe scans that band).
/// Each spawn binds an ephemeral port from here; the actual bound port is read from
/// the sidecar.
const PORT_BASE: u16 = 29700;

/// A running `sb relay:serve` child with a line-reader thread draining its stdout.
/// stdin is held open via `stdin` until the test drops the harness (or calls
/// [`McpChild::close_stdin`]).
struct McpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    home: tempfile::TempDir,
}

impl McpChild {
    /// Spawn the real `sb relay:serve` with a hermetic HOME (sidecar + inbox land in
    /// temp, never the real ~/.claude) and a fixed session id for deterministic
    /// assertions. Stdin + stdout are piped; a background thread feeds stdout lines
    /// into a channel so reads can be bounded.
    fn spawn(session_id: &str) -> Self {
        let home = tempfile::tempdir().expect("tempdir for HOME");
        let exe = env!("CARGO_BIN_EXE_qd");
        let mut child = Command::new(exe)
            .arg("relay:serve")
            .env("HOME", home.path())
            .env("RELAY_PORT_BASE", PORT_BASE.to_string())
            .env("CLAUDE_CODE_SESSION_ID", session_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sb relay:serve");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");

        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        // Forward every non-empty line; on a closed receiver, stop.
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break, // child stdout closed
                }
            }
        });

        McpChild {
            child,
            stdin: Some(stdin),
            lines: rx,
            home,
        }
    }

    /// Write one newline-delimited JSON-RPC frame to the child's stdin.
    fn send(&mut self, frame: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        let mut buf = frame.to_string().into_bytes();
        buf.push(b'\n');
        stdin.write_all(&buf).expect("write frame to child stdin");
        stdin.flush().expect("flush child stdin");
    }

    /// Write a RAW line (used for the malformed/non-JSON row).
    fn send_raw(&mut self, raw: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        let mut buf = raw.as_bytes().to_vec();
        buf.push(b'\n');
        stdin.write_all(&buf).expect("write raw to child stdin");
        stdin.flush().expect("flush child stdin");
    }

    /// Read the next stdout line within [`READ_BUDGET`], parsed as JSON. Panics
    /// (fails the row) on timeout — a hang must be loud, never silent.
    fn next_json(&self) -> Value {
        let line = self.next_line();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON stdout line {line:?}: {e}"))
    }

    /// Read the next raw stdout line within [`READ_BUDGET`].
    fn next_line(&self) -> String {
        match self.lines.recv_timeout(READ_BUDGET) {
            Ok(l) => l,
            Err(RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for a stdout line (no-hang guard fired)")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("child stdout closed unexpectedly while waiting for a line")
            }
        }
    }

    /// Path to this child's sidecar `<home>/.claude/relay/<pid>.json`.
    fn sidecar_path(&self) -> PathBuf {
        self.home
            .path()
            .join(".claude")
            .join("relay")
            .join(format!("{}.json", self.child.id()))
    }

    /// Read the bound port from the sidecar (retrying until it appears — the child
    /// writes it early in boot but not necessarily before the first stdin write).
    fn wait_for_port(&self) -> u16 {
        let path = self.sidecar_path();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                    if let Some(port) = v.get("port").and_then(Value::as_u64) {
                        return port as u16;
                    }
                }
            }
            if Instant::now() >= deadline {
                panic!("sidecar {path:?} never produced a port");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Drop the child's stdin → EOF → the child should exit 0 and unlink its sidecar.
    fn close_stdin(&mut self) {
        self.stdin.take(); // dropping the handle closes the pipe
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        // Be a good citizen even if a test panicked mid-row: close stdin, then kill.
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Load the captured bun fixture (the interop contract), or `None` when it is
/// absent on this host — print a LOUD skip and let the test return early,
/// mirroring `relay_server_differential.rs`'s bun_precondition_check (never
/// silently pass). The fixture was recorded on the QA host at an absolute
/// path; `SB_MCP_FIXTURE` overrides the location, and `REQUIRE_MCP_FIXTURE=1`
/// panics instead of skipping (the QA-host posture — prevents silent-green
/// rot where the fixture moves and the interop rows stop running).
fn load_fixture(test_name: &str) -> Option<Value> {
    let path = std::env::var("SB_MCP_FIXTURE").unwrap_or_else(|_| {
        "/home/u/work/ws/switchboard/rust/exec/ccrelay-mcp-handshake-fixture.json".to_string()
    });
    let path = Path::new(&path);
    if !path.exists() {
        if std::env::var("REQUIRE_MCP_FIXTURE").map(|v| v == "1") == Ok(true) {
            panic!("REQUIRE_MCP_FIXTURE=1 but interop fixture absent ({test_name}): {path:?}");
        }
        eprintln!(
            "SKIP mcp interop ({test_name}): captured fixture absent on this host ({path:?}) \
             — set SB_MCP_FIXTURE to point at it, or REQUIRE_MCP_FIXTURE=1 to enforce."
        );
        return None;
    }
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("read interop fixture {path:?}: {e} (required for QA)"));
    Some(serde_json::from_slice(&bytes).expect("fixture is valid JSON"))
}

/// Extract the fixture's initialize-response `result.instructions` value (the
/// contract; the fixture's `length` metadata is known-inaccurate — assert the VALUE).
fn fixture_instructions(fixture: &Value) -> String {
    for entry in fixture["handshake_sequence"].as_array().expect("sequence") {
        if entry["direction"] == "server -> client" && entry["method"] == "initialize" {
            return entry["response"]["result"]["instructions"]
                .as_str()
                .expect("fixture instructions value")
                .to_string();
        }
    }
    panic!("fixture has no initialize response with instructions");
}

/// Extract the fixture's `reply` tool description (the contract).
fn fixture_reply_description(fixture: &Value) -> String {
    for entry in fixture["handshake_sequence"].as_array().expect("sequence") {
        if entry["direction"] == "server -> client" && entry["method"] == "tools/list" {
            return entry["response"]["result"]["tools"][0]["description"]
                .as_str()
                .expect("fixture reply description")
                .to_string();
        }
    }
    panic!("fixture has no tools/list response");
}

/// Send `initialize` and return the parsed response.
fn do_initialize(child: &mut McpChild, id: i64, protocol_version: &str) -> Value {
    child.send(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": { "name": "qa", "version": "0" }
        }
    }));
    child.next_json()
}

// ---------------------------------------------------------------------------
// Row 1 — initialize handshake (P-B2)
// ---------------------------------------------------------------------------

#[test]
fn initialize_handshake_matches_fixture_shape() {
    let Some(fixture) = load_fixture("initialize_handshake_matches_fixture_shape") else {
        return;
    };
    let expected_instructions = fixture_instructions(&fixture);

    let mut child = McpChild::spawn("sess-init");
    let resp = do_initialize(&mut child, 1, "2024-11-05");

    assert_eq!(resp["jsonrpc"], "2.0", "jsonrpc version");
    assert_eq!(resp["id"], json!(1), "id echoed");
    let result = &resp["result"];
    // protocolVersion echoed.
    assert_eq!(
        result["protocolVersion"], "2024-11-05",
        "protocolVersion echoed"
    );
    // capabilities.experimental contains the claude/channel key (empty object).
    assert_eq!(
        result["capabilities"]["experimental"]["claude/channel"],
        json!({}),
        "capabilities.experimental['claude/channel'] present"
    );
    // capabilities.tools present.
    assert!(
        result["capabilities"]["tools"].is_object(),
        "capabilities.tools present, got {:?}",
        result["capabilities"]["tools"]
    );
    // serverInfo {relay, 0.1.0}.
    assert_eq!(
        result["serverInfo"],
        json!({ "name": "relay", "version": "0.1.0" }),
        "serverInfo matches"
    );
    // instructions EQUALS the fixture value byte-for-byte (the contract).
    assert_eq!(
        result["instructions"]
            .as_str()
            .expect("instructions string"),
        expected_instructions,
        "instructions must match the fixture value byte-for-byte"
    );
}

// ---------------------------------------------------------------------------
// Row 2 — protocolVersion echo of a non-standard version (P-B2)
// ---------------------------------------------------------------------------

#[test]
fn initialize_echoes_nonstandard_protocol_version() {
    let mut child = McpChild::spawn("sess-proto");
    let resp = do_initialize(&mut child, 2, "2025-06-18");
    assert_eq!(resp["id"], json!(2));
    // Proves the echo path (not a hardcoded 2024-11-05).
    assert_eq!(
        resp["result"]["protocolVersion"], "2025-06-18",
        "non-standard protocolVersion must be echoed, proving echo (not a pin)"
    );
}

// ---------------------------------------------------------------------------
// Row 3 — tools/list (P-B3)
// ---------------------------------------------------------------------------

#[test]
fn tools_list_advertises_exactly_reply_matching_fixture() {
    let Some(fixture) = load_fixture("tools_list_advertises_exactly_reply_matching_fixture") else {
        return;
    };
    let expected_desc = fixture_reply_description(&fixture);

    let mut child = McpChild::spawn("sess-tools");
    do_initialize(&mut child, 1, "2024-11-05");
    // notifications/initialized → no response.
    child.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    child.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }));
    let resp = child.next_json();

    assert_eq!(resp["id"], json!(2));
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1, "exactly one tool advertised");
    let reply = &tools[0];
    assert_eq!(reply["name"], "reply", "the one tool is `reply`");
    assert_eq!(
        reply["description"].as_str().expect("description"),
        expected_desc,
        "reply description must match the fixture byte-for-byte"
    );
    let required = reply["inputSchema"]["required"]
        .as_array()
        .expect("required array");
    assert_eq!(
        required,
        &vec![json!("text"), json!("message_id")],
        "required == [text, message_id]"
    );
    assert_eq!(
        reply["inputSchema"]["properties"]["text"]["type"], "string",
        "text is a string property"
    );
    assert_eq!(
        reply["inputSchema"]["properties"]["message_id"]["type"], "string",
        "message_id is a string property"
    );
}

// ---------------------------------------------------------------------------
// Row 4 — tools/call reply DELIVERY + unknown tool (P-B5, M4)
// ---------------------------------------------------------------------------

#[test]
fn tools_call_reply_is_valid_result_and_unknown_tool_is_error() {
    let mut child = McpChild::spawn("sess-call");
    do_initialize(&mut child, 1, "2024-11-05");

    // M4: tools/call name=reply now runs REAL delivery (no longer a seam). This id
    // (`relay-1-1`) was never recorded on this fresh subprocess server and has no
    // parked waiter, so delivery takes the HONEST NOT-DELIVERED path (NoOrigin →
    // P-E6): a well-formed single text-content result with isError:true + guidance.
    child.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "reply", "arguments": { "text": "hi", "message_id": "relay-1-1" } }
    }));
    let resp = child.next_json();
    assert_eq!(resp["id"], json!(2));
    let content = resp["result"]["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1, "one content block");
    assert_eq!(content[0]["type"], "text");
    let text = content[0]["text"].as_str().expect("text content");
    // Honest NOT-DELIVERED for an unrecorded id (P-E6): isError + guidance.
    assert_eq!(
        resp["result"]["isError"], true,
        "no-origin reply → result.isError == true (P-E6)"
    );
    assert!(
        text.starts_with("NOT DELIVERED"),
        "honest not-delivered prefix: {text}"
    );
    assert!(
        text.contains("send a fresh message"),
        "fresh-message guidance present: {text}"
    );

    // unknown tool → result with isError:true.
    child.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "frobnicate", "arguments": {} }
    }));
    let resp = child.next_json();
    assert_eq!(resp["id"], json!(3));
    assert_eq!(
        resp["result"]["isError"], true,
        "unknown tool → result.isError == true"
    );
}

// ---------------------------------------------------------------------------
// Row 5 — notifications/initialized produces NO response (P-B6)
// ---------------------------------------------------------------------------

#[test]
fn notifications_initialized_emits_no_response_line() {
    let mut child = McpChild::spawn("sess-noresp");
    // Drain the initialize response first.
    do_initialize(&mut child, 1, "2024-11-05");

    // Send the notification; it must emit nothing. Then send a SUBSEQUENT request
    // and assert the next line we read is THAT request's response (id 5) — proving
    // the notification itself produced no intervening line.
    child.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    child.send(&json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list", "params": {} }));

    let resp = child.next_json();
    assert_eq!(
        resp["id"],
        json!(5),
        "next line must be the tools/list response (id 5); notifications/initialized emitted nothing"
    );
    assert!(resp["result"]["tools"].is_array());
}

// ---------------------------------------------------------------------------
// Row 6 — unknown method → -32601; malformed line → loop keeps serving (P-B1)
// ---------------------------------------------------------------------------

#[test]
fn unknown_method_is_minus_32601_error() {
    let mut child = McpChild::spawn("sess-unknown");
    do_initialize(&mut child, 1, "2024-11-05");

    child.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" }));
    let resp = child.next_json();
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], json!(2));
    assert_eq!(
        resp["error"]["code"], -32601,
        "unknown method → JSON-RPC -32601"
    );
    assert!(
        resp["result"].is_null(),
        "an error response carries no result"
    );
}

#[test]
fn malformed_line_does_not_crash_loop_keeps_serving() {
    let mut child = McpChild::spawn("sess-garbage");
    do_initialize(&mut child, 1, "2024-11-05");

    // Garbage / non-JSON line: must be skipped, no response, no crash.
    child.send_raw("this is not json at all");
    child.send_raw("{ broken json ");
    // A valid request after the garbage MUST still be answered — the loop survived.
    child.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }));
    let resp = child.next_json();
    assert_eq!(
        resp["id"],
        json!(2),
        "loop must keep serving after malformed lines"
    );
    assert!(resp["result"]["tools"].is_array());
}

// ---------------------------------------------------------------------------
// Row 7 — ★ end-to-end outbound notification (P-B4) — the PRIMARY relay function
// ---------------------------------------------------------------------------

#[test]
fn post_message_emits_channel_notification_end_to_end() {
    let Some(fixture) = load_fixture("post_message_emits_channel_notification_end_to_end") else {
        return;
    };

    let mut child = McpChild::spawn("sess-e2e");
    // Complete the handshake so the loop is live; stdin stays open.
    do_initialize(&mut child, 1, "2024-11-05");

    // Learn the bound port from the sidecar, then POST /message via the REAL client.
    let port = child.wait_for_port();
    let returned_id = CcRelay::new()
        .send_message(port, "hello body", "sess-A")
        .expect("POST /message must return a message_id");
    assert!(
        !returned_id.is_empty(),
        "server must mint a non-empty message_id"
    );

    // Now read a REAL line off the child's stdout: the outbound channel notification.
    // (The initialize response was already drained, so the next line is the notif.)
    let notif = child.next_json();

    // Match the fixture's notification_shape exactly: method + content + meta.
    let expected_method = fixture["notification_shape"]["notification"]["method"]
        .as_str()
        .expect("fixture notification method");
    assert_eq!(
        notif["method"], expected_method,
        "method == notifications/claude/channel (fixture shape)"
    );
    assert_eq!(notif["jsonrpc"], "2.0");
    // A notification has NO id (fire-and-forget).
    assert!(notif["id"].is_null(), "a notification carries no id");
    assert_eq!(
        notif["params"]["content"], "hello body",
        "content == the POSTed text"
    );
    assert_eq!(
        notif["params"]["meta"]["from_session"], "sess-A",
        "meta.from_session == the sender"
    );
    // The notification's message_id is the SAME id /message returned (end-to-end).
    assert_eq!(
        notif["params"]["meta"]["message_id"], returned_id,
        "meta.message_id == the message_id /message returned (end-to-end identity)"
    );
}

// ---------------------------------------------------------------------------
// Row 8 — clean EOF shutdown (P-D4 / P-F2)
// ---------------------------------------------------------------------------

#[test]
fn clean_eof_shutdown_exits_zero_and_unlinks_sidecar() {
    let mut child = McpChild::spawn("sess-eof");
    do_initialize(&mut child, 1, "2024-11-05");
    let sidecar = child.sidecar_path();
    // Sidecar should exist while running.
    child.wait_for_port();
    assert!(sidecar.exists(), "sidecar exists while the server runs");

    // Drop stdin → EOF → child should exit 0 within a bounded time and unlink it.
    child.close_stdin();

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None => {
                if Instant::now() >= deadline {
                    panic!("child did not exit within 5s of stdin EOF");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    assert_eq!(status.code(), Some(0), "clean EOF shutdown exits 0 (P-F2)");
    assert!(
        !sidecar.exists(),
        "sidecar must be unlinked on clean shutdown (P-D4/P-F2)"
    );
}
