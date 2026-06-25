//! §4a M4 DELIVERY-SUCCESS parity rows — the FULL-STACK reply path through the real
//! MCP `tools/call` stdin, driven against the live `sb relay:serve` subprocess.
//!
//! Written by QA (the CHECKER), SEPARATE from the M4 implementer AND from the
//! concurrency red-teamer. This file ADDS rows only — it NEVER modifies the server.
//! Any divergence from the M4 contract is REPORTED, not fixed.
//!
//! ## The specific gap this file closes
//! The M3 MCP harness (`relay_server_mcp.rs`) covers the handshake + the honest
//! NOT-DELIVERED path through `tools/call`. The concurrency red-teamer hammered
//! `deliver_reply` IN-PROCESS + `/replies` over the socket (zero drops in 20k
//! lost-wakeup trials). NEITHER drives the **DELIVERY-SUCCESS** path through the real
//! MCP `tools/call` stdin — i.e. a reply submitted via the MCP `reply` TOOL actually
//! (a) resolving a waiting sender (P-E2) or (b) pushing back to an origin (P-E3),
//! driven through the real `sb relay:serve` subprocess. THAT is the drop-in contract
//! Claude Code depends on. These rows close it.
//!
//! ## Why a real subprocess
//! `mcp::serve_stdio` reads the process's REAL stdin and writes its REAL stdout —
//! there is no in-process seam for it (the in-process `spawn_for_test` drives ONLY
//! the HTTP half, never the MCP loop). So every row spawns `target/debug/sb
//! relay:serve` as a child, pipes JSON-RPC frames into its stdin, and reads
//! response/notification lines off its stdout. The child stdin handle is held ALIVE
//! for the test; dropping it → EOF → clean exit.
//!
//! ## No-hang discipline
//! EVERY stdout read AND the parked-fetch thread are bounded by a timeout, so a
//! regression (a reply that does NOT deliver, a waiter that is NEVER resolved) FAILS
//! the row LOUDLY instead of hanging the suite.
//!
//! ## P-assertions covered (M4 plan P-E2/E3/E4/E6)
//! - Row 1 (P-E2): a reply via the MCP `reply` tool RESOLVES a parked `/replies`
//!   long-poll end-to-end — the tools/call result is DELIVERED (not isError) AND the
//!   parked `fetch_reply` returns the reply text. The CENTRAL drop-in contract.
//! - Row 2 (P-E3): a reply via the MCP `reply` tool PUSHES BACK to a second session's
//!   sidecar — A's result is DELIVERED AND session B receives a
//!   `notifications/claude/channel` line whose content is `[REPLY to M] ...` and
//!   meta.from_session is A. The cross-session delivery contract.
//! - Row 3 (P-E6): honest NOT-DELIVERED — a reply for an unrecorded id is isError +
//!   carries the fresh-message guidance.
//! - Row 4 (P-E4): the loop belt — a reply for an inbound that is itself a
//!   `[REPLY to ...]` is refused (isError "loop prevention"), never auto-posts.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use dispatch::relay::{RelayContract, RelayError};
use dispatch::relay_http::CcRelay;

/// Per-frame read budget. A healthy localhost subprocess answers a request well
/// under this; a read that blocks past it FAILS the row (never hangs the suite).
const READ_BUDGET: Duration = Duration::from_secs(8);

/// A high port base OUTSIDE the 8900-9000 jail band (sb's own probe scans that band).
/// Two subprocesses under the same HOME bind distinct ports from this base.
const PORT_BASE: u16 = 31700;

/// A running `sb relay:serve` child with a line-reader thread draining its stdout.
struct McpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// The HOME for this child — borrowed when two children must SHARE a relay_dir
    /// (the push-back row), so it is kept as a path the caller can reuse.
    home: PathBuf,
    /// Owns the tempdir when this child created it; `None` when it shares another's.
    _home_guard: Option<tempfile::TempDir>,
}

impl McpChild {
    /// Spawn `sb relay:serve` with a FRESH hermetic HOME (its own relay_dir + inbox).
    fn spawn(session_id: &str) -> Self {
        let home = tempfile::tempdir().expect("tempdir for HOME");
        let home_path = home.path().to_path_buf();
        Self::spawn_with_home(session_id, &home_path, Some(home))
    }

    /// Spawn `sb relay:serve` reusing an EXISTING HOME dir (shared relay_dir) — used
    /// by the two-subprocess push-back row so A can discover B's sidecar. The caller
    /// owns the tempdir lifetime (the FIRST child's `_home_guard`); this child holds
    /// no guard.
    fn spawn_sharing_home(session_id: &str, home: &std::path::Path) -> Self {
        Self::spawn_with_home(session_id, home, None)
    }

    fn spawn_with_home(
        session_id: &str,
        home: &std::path::Path,
        home_guard: Option<tempfile::TempDir>,
    ) -> McpChild {
        let exe = env!("CARGO_BIN_EXE_qd");
        let mut child = Command::new(exe)
            .arg("relay:serve")
            .env("HOME", home)
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
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        McpChild {
            child,
            stdin: Some(stdin),
            lines: rx,
            home: home.to_path_buf(),
            _home_guard: home_guard,
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

    /// Read stdout frames until one satisfies `pred` (or a total deadline elapses).
    /// Stdout multiplexes the JSON-RPC RESPONSE to a request AND the fire-and-forget
    /// `notifications/claude/channel` from a `/message` POST, so a positional read is
    /// fragile: this helper keys on the frame's SHAPE (id/method) instead. A frame
    /// that does not match is discarded (returned to the caller for inspection if
    /// needed via the `discarded` accumulator the caller may ignore).
    fn next_json_matching<F: Fn(&Value) -> bool>(&self, what: &str, pred: F) -> Value {
        let deadline = Instant::now() + READ_BUDGET;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                panic!("timed out waiting for a stdout frame matching {what} (no-hang guard)");
            }
            let line = match self.lines.recv_timeout(remaining) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for a stdout frame matching {what} (no-hang guard)")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("child stdout closed while waiting for a frame matching {what}")
                }
            };
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON stdout line {line:?}: {e}"));
            if pred(&v) {
                return v;
            }
            // Not the frame we want (e.g. a channel notification when we want the
            // tools/call response, or vice versa) — keep reading.
        }
    }

    /// Path to this child's sidecar `<home>/.claude/relay/<pid>.json`.
    fn sidecar_path(&self) -> PathBuf {
        self.home
            .join(".claude")
            .join("relay")
            .join(format!("{}.json", self.child.id()))
    }

    /// Read the bound port from the sidecar (retrying until it appears).
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
}

impl Drop for McpChild {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Complete the MCP handshake (initialize + notifications/initialized) so the loop is
/// live and the server is fully booted; returns nothing — the handshake response is
/// drained here, so subsequent reads see only the frames the row cares about.
fn handshake(child: &mut McpChild, id: i64) {
    child.send(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "qa-delivery", "version": "0" }
        }
    }));
    // Drain the initialize response (keyed on id, in case anything raced ahead).
    let resp = child.next_json_matching("initialize response", |v| v["id"] == json!(id));
    assert_eq!(
        resp["result"]["serverInfo"]["name"], "relay",
        "handshake must complete against the relay server"
    );
    child.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
}

/// Submit a `tools/call name=reply` frame via the child's MCP stdin and read back the
/// response keyed on `id` (skipping any channel notifications that arrive first).
fn call_reply(child: &mut McpChild, id: i64, message_id: &str, text: &str) -> Value {
    child.send(&json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": "reply", "arguments": { "text": text, "message_id": message_id } }
    }));
    child.next_json_matching("tools/call reply response", |v| v["id"] == json!(id))
}

/// Pull the single text-content string out of a tools/call result.
fn result_text(resp: &Value) -> String {
    let content = resp["result"]["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1, "exactly one content block");
    assert_eq!(content[0]["type"], "text", "content type is text");
    content[0]["text"]
        .as_str()
        .expect("text content string")
        .to_string()
}

/// Whether a tools/call result carries `isError:true` (false when the field is
/// omitted — the SDK only sets it on the error path).
fn is_error(resp: &Value) -> bool {
    resp["result"]["isError"] == json!(true)
}

// ---------------------------------------------------------------------------
// Row 1 — ★ DELIVERY-SUCCESS resolves a parked waiter end-to-end via the MCP
//          reply tool (P-E2). The central drop-in contract.
// ---------------------------------------------------------------------------

#[test]
fn mcp_reply_tool_resolves_a_parked_replies_long_poll_end_to_end() {
    let mut child = McpChild::spawn("sess-deliver-A");
    handshake(&mut child, 1);
    let port = child.wait_for_port();

    // (a) POST /message (from "sess-A") → mint message_id M; this ALSO records the
    // origin in the server's in-mem map (sender = sess-A). The POST emits an
    // outbound channel notification on the child's OWN stdout — we let
    // next_json_matching skip past it later.
    let message_id = CcRelay::new()
        .send_message(port, "please ack", "sess-A")
        .expect("POST /message returns a message_id");
    assert!(
        !message_id.is_empty(),
        "server mints a non-empty message_id"
    );

    // (b) On a thread, park a /replies long-poll for M (the "sender waiting in
    // sb send:relay --wait"). Give it a generous budget; the resolve should win
    // well within it. The thread is bounded by a join-timeout below so a regression
    // (the waiter NEVER resolved) FAILS loudly instead of hanging.
    let poll_port = port;
    let poll_id = message_id.clone();
    let poll = std::thread::spawn(move || CcRelay::new().fetch_reply(poll_port, &poll_id, 6000));

    // (c) Wait until the long-poll is actually parked. We cannot read has_waiter over
    // the wire, so give it a real beat. The server registers the waiter synchronously
    // at the top of handle_replies before parking on the Condvar; 300ms is ample for
    // a localhost GET to reach that point.
    std::thread::sleep(Duration::from_millis(300));

    // (d) Submit the reply via the child's MCP stdin.
    let resp = call_reply(&mut child, 2, &message_id, "acknowledged");
    eprintln!("[ROW1] message_id minted by POST /message = {message_id}");
    eprintln!("[ROW1] captured tools/call reply RESPONSE frame = {resp}");

    // (e) ASSERT the tools/call RESULT came back DELIVERED — a text content, NOT
    // isError — and that it names the long-poll resolution.
    let text = result_text(&resp);
    assert!(
        !is_error(&resp),
        "a resolved waiter is a real delivery — result.isError must be absent/false, got: {resp}"
    );
    assert!(
        text.starts_with("DELIVERED"),
        "tools/call result must report DELIVERED, got: {text}"
    );
    assert!(
        text.contains("long-poll resolved"),
        "result must name the parked-waiter resolution (P-E2), got: {text}"
    );

    // AND the parked fetch_reply must return the buffered text. Bound the join so a
    // never-resolved waiter (a real regression) FAILS rather than hangs the suite.
    let resolved = join_with_timeout(poll, Duration::from_secs(8))
        .expect("the parked fetch_reply thread must finish within 8s (else: regression — waiter never resolved)")
        .expect("fetch_reply must succeed (the waiter was resolved)");
    eprintln!("[ROW1] parked fetch_reply RESOLVED with = {resolved:?}");
    assert_eq!(
        resolved.text.as_deref(),
        Some("acknowledged"),
        "the parked sender must receive the reply TEXT submitted via the MCP reply tool (P-E2 end-to-end)"
    );
    assert!(
        resolved.error.is_none(),
        "a resolved long-poll carries no error, got: {:?}",
        resolved.error
    );
}

// ---------------------------------------------------------------------------
// Row 2 — PUSH-BACK to an origin's sidecar via the MCP reply tool (P-E3).
//          Cross-session delivery contract: A's reply lands as a channel
//          notification on session B's stdout.
// ---------------------------------------------------------------------------

#[test]
fn mcp_reply_tool_pushes_back_to_origin_session_via_channel_notification() {
    // Two subprocesses under the SAME HOME so they share one relay_dir — A must be
    // able to discover B's sidecar to push back. A owns the tempdir lifetime.
    let mut a = McpChild::spawn("sessA");
    let home = a.home.clone();
    let mut b = McpChild::spawn_sharing_home("sessB", &home);

    handshake(&mut a, 1);
    handshake(&mut b, 1);

    let a_port = a.wait_for_port();
    let _b_port = b.wait_for_port();

    // Both sidecars must be visible in the shared relay_dir before the push-back
    // probe runs (A enumerates relay_dir, filters to sessionId==sessB, verifies via
    // /health). Confirm B's sidecar is on disk with sessionId=sessB.
    let b_sidecar = b.sidecar_path();
    assert!(
        b_sidecar.exists(),
        "B's sidecar must exist in the shared relay_dir"
    );

    // B POSTs a message to A's port (from="sessB") → A records origin=sessB for M.
    // NO waiter is ever parked on A.
    let message_id = CcRelay::new()
        .send_message(a_port, "what is the answer?", "sessB")
        .expect("B's POST to A returns a message_id");
    assert!(!message_id.is_empty());

    // A submits the reply via its MCP stdin. With no waiter parked, decide_delivery
    // returns PushBack{sessB}; A enumerates the shared relay_dir, finds B's sidecar,
    // verifies sessionId==sessB via /health, and POSTs `[REPLY to M] the answer` to
    // B's /message with from_session=sessA.
    let resp = call_reply(&mut a, 2, &message_id, "the answer");
    eprintln!("[ROW2] B's POST to A minted message_id = {message_id}");
    eprintln!("[ROW2] A's captured tools/call reply RESPONSE frame = {resp}");

    let text = result_text(&resp);
    assert!(
        !is_error(&resp),
        "push-back to a live origin sidecar is a real delivery — isError must be absent/false, got: {resp}"
    );
    assert!(
        text.starts_with("DELIVERED"),
        "A's tools/call result must report DELIVERED on a successful push-back, got: {text}"
    );
    assert!(
        text.contains("posted to session sessB"),
        "result must name the push-back target (P-E3), got: {text}"
    );

    // B must receive the pushed-back reply as an OUTBOUND channel notification on its
    // stdout — A's POST to B's /message triggers emit_channel_notification on B.
    // Skip B's handshake response (already drained) + key on the channel method.
    let notif = b.next_json_matching("B's channel notification", |v| {
        v["method"] == "notifications/claude/channel"
    });
    eprintln!("[ROW2] B's captured channel NOTIFICATION frame = {notif}");
    assert_eq!(notif["jsonrpc"], "2.0");
    assert!(notif["id"].is_null(), "a notification carries no id");
    assert_eq!(
        notif["params"]["content"]
            .as_str()
            .expect("notification content"),
        format!("[REPLY to {message_id}] the answer"),
        "B must receive the reply prefixed [REPLY to <id>] (the cross-session delivery contract)"
    );
    assert_eq!(
        notif["params"]["meta"]["from_session"], "sessA",
        "the pushed-back reply's from_session must be A (the replier)"
    );
    // The pushed-back message is a NEW /message on B, so it carries B's own freshly
    // minted message_id. We assert only that it is WELL-FORMED (relay-<digits>-<seq>).
    // We do NOT assert it differs from the original M: message_id = relay-<epoch_ms>-
    // <seq> where seq is PER-PROCESS, so two fresh servers minting in the same ms can
    // legitimately produce identical ids — cross-process id-inequality is not a
    // guaranteed property of the system, and asserting it is a load flake (orc-37).
    let pushed_mid = notif["params"]["meta"]["message_id"]
        .as_str()
        .expect("pushed-back notification has a message_id");
    assert!(
        is_well_formed_message_id(pushed_mid),
        "the pushed-back message_id must be well-formed relay-<digits>-<digits>, got: {pushed_mid}"
    );
}

/// A relay message_id is `relay-<epoch_ms>-<seq>` (both numeric). Checked WITHOUT a
/// regex dep: exactly three `-`-segments, first == "relay", the other two all-digit
/// and non-empty.
fn is_well_formed_message_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() == 3
        && parts[0] == "relay"
        && !parts[1].is_empty()
        && parts[1].bytes().all(|b| b.is_ascii_digit())
        && !parts[2].is_empty()
        && parts[2].bytes().all(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Row 3 — honest NOT-DELIVERED via the MCP reply tool (P-E6).
//          (relay_server_mcp.rs Row 4 asserts this for a fresh server already;
//          this re-confirms it on a server that ALSO has live HTTP state, and
//          pins the exact guidance string the model depends on.)
// ---------------------------------------------------------------------------

#[test]
fn mcp_reply_tool_unrecorded_id_is_honest_not_delivered() {
    let mut child = McpChild::spawn("sess-notdelivered");
    handshake(&mut child, 1);

    // An id never recorded on this server, no waiter parked, no origin, no inbox
    // file → NoOrigin → P-E6 honest NOT-DELIVERED.
    let resp = call_reply(&mut child, 2, "relay-doesnotexist-1", "x");
    let text = result_text(&resp);
    assert!(
        is_error(&resp),
        "an unrecorded reply reaches no one → isError:true (P-E6), got: {resp}"
    );
    assert!(
        text.starts_with("NOT DELIVERED"),
        "honest not-delivered prefix, got: {text}"
    );
    assert!(
        text.contains("send a fresh message"),
        "must carry the fresh-message guidance, got: {text}"
    );
    assert!(
        text.contains("sb send:relay"),
        "guidance must name the sb send:relay escape hatch, got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Row 4 — loop belt via the MCP reply tool (P-E4): a reply for an inbound that is
//          itself a [REPLY to ...] is refused, never auto-posted.
//          (The red-teamer covered this in-process; this confirms the belt holds
//          through the real MCP tools/call path.)
// ---------------------------------------------------------------------------

#[test]
fn mcp_reply_tool_refuses_reply_to_a_reply_loop_prevention() {
    let mut child = McpChild::spawn("sess-loopbelt");
    handshake(&mut child, 1);
    let port = child.wait_for_port();

    // POST a message whose text STARTS WITH "[REPLY to ...]" → the server records
    // origin with is_reply=true (is_reply_text true). from="sess-peer" is a real
    // addressable origin, so ONLY the loop belt (not NotAddressable) can refuse it.
    let message_id = CcRelay::new()
        .send_message(port, "[REPLY to relay-orig-9] earlier answer", "sess-peer")
        .expect("POST returns a message_id");

    // Reply to THAT inbound. Because the inbound was itself a pushed-back reply,
    // decide_delivery returns LoopPrevented → isError "loop prevention", NO
    // push-back is ever attempted (the ping-pong belt, P-E4).
    let resp = call_reply(&mut child, 2, &message_id, "would ping-pong forever");
    let text = result_text(&resp);
    assert!(
        is_error(&resp),
        "a reply to a [REPLY to ...] inbound must be refused → isError (P-E4), got: {resp}"
    );
    assert!(
        text.contains("loop prevention"),
        "result must name loop prevention, got: {text}"
    );
    assert!(
        text.starts_with("NOT DELIVERED"),
        "loop-prevented is an honest not-delivered, got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Sanity row — prove the NOT-DELIVERED guidance does NOT fire on a genuine delivery
// (guards against a regression where every reply is reported delivered, OR every
// reply is reported not-delivered — i.e. the result field is dead).
// This is the differential that makes Row 1's DELIVERED assertion meaningful.
// ---------------------------------------------------------------------------

#[test]
fn delivered_and_not_delivered_are_genuinely_distinguished() {
    let mut child = McpChild::spawn("sess-differential");
    handshake(&mut child, 1);
    let port = child.wait_for_port();

    // A genuine delivery (parked waiter) → NOT isError.
    let m_ok = CcRelay::new()
        .send_message(port, "deliver me", "sess-X")
        .expect("POST ok");
    let poll_port = port;
    let poll_id = m_ok.clone();
    let poll = std::thread::spawn(move || CcRelay::new().fetch_reply(poll_port, &poll_id, 6000));
    std::thread::sleep(Duration::from_millis(300));
    let ok_resp = call_reply(&mut child, 2, &m_ok, "yes");
    assert!(!is_error(&ok_resp), "delivered reply is NOT isError");
    let ok_text = result_text(&ok_resp);

    // A miss (unrecorded id) → isError.
    let bad_resp = call_reply(&mut child, 3, "relay-no-such-99", "no");
    assert!(is_error(&bad_resp), "missed reply IS isError");
    let bad_text = result_text(&bad_resp);

    // The two outcomes are genuinely different text + different isError — the result
    // field is LIVE, not stuck on one verdict.
    assert_ne!(
        ok_text, bad_text,
        "DELIVERED and NOT-DELIVERED must produce different result text"
    );
    assert!(ok_text.starts_with("DELIVERED"));
    assert!(bad_text.starts_with("NOT DELIVERED"));

    let resolved = join_with_timeout(poll, Duration::from_secs(8))
        .expect("parked fetch must finish")
        .expect("fetch ok");
    assert_eq!(resolved.text.as_deref(), Some("yes"));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Join a `JoinHandle` with a wall-clock timeout. Returns `Some(result)` if the
/// thread finished in time, `None` if it is still running past `timeout` (a hang —
/// the caller turns that into a loud failure). Implemented with a channel so we
/// never block the test thread forever.
fn join_with_timeout<T: Send + 'static>(
    handle: std::thread::JoinHandle<T>,
    timeout: Duration,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = handle.join();
        let _ = tx.send(r);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(v)) => Some(v),
        Ok(Err(_panic)) => panic!("the parked-fetch thread PANICKED"),
        Err(_) => None,
    }
}

/// Compile-time assurance the error type is in scope (a `fetch_reply` Err in Row 1
/// would be a regression we want named, not a type error). Touch it so an unused
/// import never silently drops the symbol.
#[allow(dead_code)]
fn _error_type_in_scope(e: RelayError) -> RelayError {
    e
}
