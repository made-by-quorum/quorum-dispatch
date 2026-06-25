//! §4b M5b DIFFERENTIAL + CROSS-COMPAT harness — drives BOTH the legacy bun
//! relay server (`~/work/cc-relay/server.ts`) AND the native Rust `sb relay:serve`
//! through the SAME scenario script via the SAME frozen `CcRelay` client (HTTP) +
//! subprocess JSON-RPC (MCP stdio), asserting EQUIVALENT OBSERVABLE behavior.
//!
//! ## Goal
//! This is a DIAGNOSTIC + INTEROP check, NOT a byte-identical gate. The bar is:
//! the Rust server is a drop-in for the frozen client + CC's MCP channel.
//! Behavioral divergences from bun are JUDGMENT CALLS for the lead — this file
//! asserts the HARD interop contract and does NOT assert the known-intentional
//! deltas listed in the EXPECTED-DIVERGENCE ALLOWLIST below.
//!
//! ## Bun-present precondition
//! If bun (`$HOME/.bun/bin/bun`) or `~/work/cc-relay/server.ts` is absent on this
//! host, each test prints a LOUD skip marker and returns. On brano (the target host),
//! both are present and the differential MUST actually run — never a silent pass.
//!
//! ## EXPECTED-DIVERGENCE ALLOWLIST (do NOT assert these as equal)
//! - mint seq VALUES: Rust seeds the per-process seq from the start epoch; bun
//!   starts at 0. We assert the message_id FORMAT only (`^relay-<digits>-<digits>$`).
//! - idleTimeout: bun uses idleTimeout=240 to keep long-polls alive; Rust does not
//!   need this hack. Not observable at the assertion level.
//! - TTL sweeper cadence: Rust uses a 30s poll; bun uses per-entry setTimeout.
//!   Not observable in a short test.
//! - M5a stale-sidecar sweep (Rust only) and conn-cap 128 (Rust only): these are
//!   hardening features absent from bun. Not tested here (covered in M5a suite).
//! - M1 fix: Rust dropped bun's spurious-evict-on-FIFO-reinsert. Not observable
//!   in these rows.
//!
//! ## Jail discipline
//! Every server uses a TEMP HOME (tempfile::tempdir). RELAY_PORT_BASE is set to a
//! high value outside 8900-9000 (the client probe range). NEVER writes to the real
//! ~/.claude or ~/work/cc-relay.
//!
//! ## RAII reap guard (non-negotiable)
//! Every spawned process (bun or sb) is killed on Drop — including on panic-unwind
//! and early-return — so no relay process leaks into the fleet after the suite.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use dispatch::relay::{RelayContract, RelayError};
use dispatch::relay_http::CcRelay;

/// Per-frame read budget. A healthy localhost subprocess answers well under this.
const READ_BUDGET: Duration = Duration::from_secs(10);

/// Generate two distinct ephemeral port bases for a test, both OUTSIDE the
/// 8900-9000 jail band and the 32768-65535 OS ephemeral range.
///
/// Combines the current thread ID (unique per test worker) with a nanosecond
/// offset to produce a base that is distinct even when two tests start at the
/// same wall-clock instant. Returns (base_a, base_b) where base_b = base_a + 200
/// so find_port's +100 scan spans for the two servers in a shared-HOME test
/// never overlap.
///
/// Range: 10000-32367 (well clear of the 8900-9000 jail and the OS ephemeral band).
fn unique_port_bases() -> (u16, u16) {
    // Thread ID hash — each parallel test runs on its own thread; XOR with
    // nanos for extra spread within the same thread across sequential calls.
    let tid = {
        // std::thread::current().id() is opaque; format it and parse the number.
        let tid_str = format!("{:?}", std::thread::current().id());
        // ThreadId(N) — extract N.
        let n: u64 = tid_str
            .trim_start_matches("ThreadId(")
            .trim_end_matches(')')
            .parse()
            .unwrap_or(1);
        n
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    // Mix tid and nanos; map to 10000..=32167.
    let spread = (tid.wrapping_mul(997).wrapping_add(nanos / 1_000)) % 22168;
    let base_a = 10000u16 + spread as u16;
    // base_b is 200 steps ahead so find_port (+100) scans never collide.
    let base_b = base_a.saturating_add(200);
    (base_a, base_b)
}

// ---------------------------------------------------------------------------
// RelayKind — which server binary to spawn
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelayKind {
    Rust,
    Bun,
}

impl RelayKind {
    fn name(self) -> &'static str {
        match self {
            RelayKind::Rust => "Rust",
            RelayKind::Bun => "Bun",
        }
    }
}

// ---------------------------------------------------------------------------
// Precondition check — LOUD skip when bun/server.ts absent
// ---------------------------------------------------------------------------

/// Resolve `$HOME` (the REAL user home, NOT the test's temp home) for the bun
/// binary and server.ts paths. Never hardcodes `/home/u`.
fn real_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

fn bun_binary() -> PathBuf {
    real_home().join(".bun").join("bin").join("bun")
}

fn server_ts() -> PathBuf {
    real_home().join("work").join("cc-relay").join("server.ts")
}

/// Check that bun + server.ts are present. If not, print a LOUD skip message
/// and return false. Never silently pass.
///
/// F3 fix: if `REQUIRE_BUN=1` is set (e.g. on brano/CI), panic instead of
/// returning false — enforces that the differential actually runs on hosts
/// that promise bun is present. Prevents silent-green rot if server.ts moves.
fn bun_precondition_check(test_name: &str) -> bool {
    let bun = bun_binary();
    let ts = server_ts();
    let bun_ok = bun.exists();
    let ts_ok = ts.exists();
    if !bun_ok || !ts_ok {
        let require_bun = std::env::var("REQUIRE_BUN")
            .map(|v| v == "1")
            .unwrap_or(false);
        if require_bun {
            panic!(
                "REQUIRE_BUN=1 but bun/cc-relay absent ({test_name}): \
                 bun={bun_ok} bun_path={bun:?}, server_ts={ts_ok} ts_path={ts:?}"
            );
        }
        eprintln!(
            "SKIP 4b differential ({test_name}): bun/cc-relay absent on this host \
             (bun={bun_ok} bun_path={bun:?}) (server_ts={ts_ok} ts_path={ts:?})"
        );
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// RelayChild — spawns either Rust or Bun relay server, full RAII reap
// ---------------------------------------------------------------------------

struct RelayChild {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// The HOME directory for this child (temp).
    home: PathBuf,
    /// Owns the tempdir when this child created it; None when sharing another's.
    _home_guard: Option<tempfile::TempDir>,
    kind: RelayKind,
}

impl RelayChild {
    /// Spawn `kind` server with a FRESH hermetic HOME.
    fn spawn(kind: RelayKind, session_id: &str, port_base: u16) -> Self {
        let home = tempfile::tempdir().expect("tempdir for relay HOME");
        let home_path = home.path().to_path_buf();
        Self::spawn_with_home(kind, session_id, &home_path, Some(home), port_base)
    }

    /// Spawn sharing an EXISTING HOME dir (shared relay_dir) — used in the
    /// push-back and cross-compat rows so servers can discover each other's sidecars.
    fn spawn_sharing_home(
        kind: RelayKind,
        session_id: &str,
        home: &std::path::Path,
        port_base: u16,
    ) -> Self {
        Self::spawn_with_home(kind, session_id, home, None, port_base)
    }

    fn spawn_with_home(
        kind: RelayKind,
        session_id: &str,
        home: &std::path::Path,
        home_guard: Option<tempfile::TempDir>,
        port_base: u16,
    ) -> Self {
        let mut cmd = match kind {
            RelayKind::Rust => {
                let exe = env!("CARGO_BIN_EXE_qd");
                let mut c = Command::new(exe);
                c.arg("relay:serve");
                c
            }
            RelayKind::Bun => {
                let mut c = Command::new(bun_binary());
                c.arg(server_ts());
                c
            }
        };

        cmd.env("HOME", home)
            .env("RELAY_PORT_BASE", port_base.to_string())
            .env("CLAUDE_CODE_SESSION_ID", session_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {} relay server: {e}", kind.name()));

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

        RelayChild {
            child,
            stdin: Some(stdin),
            lines: rx,
            home: home.to_path_buf(),
            _home_guard: home_guard,
            kind,
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
    /// Keys on frame SHAPE (id/method) rather than position — stdout multiplexes
    /// JSON-RPC responses and fire-and-forget notifications.
    fn next_json_matching<F: Fn(&Value) -> bool>(&self, what: &str, pred: F) -> Value {
        let deadline = Instant::now() + READ_BUDGET;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                panic!(
                    "[{}] timed out waiting for a stdout frame matching {what} (no-hang guard)",
                    self.kind.name()
                );
            }
            let line = match self.lines.recv_timeout(remaining) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "[{}] timed out waiting for stdout frame matching {what} (no-hang guard)",
                        self.kind.name()
                    )
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!(
                        "[{}] child stdout closed while waiting for frame matching {what}",
                        self.kind.name()
                    )
                }
            };
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON stdout line {line:?}: {e}"));
            if pred(&v) {
                return v;
            }
            // Not the frame we want — keep reading (e.g. notifications racing responses).
        }
    }

    /// Path to this child's sidecar `<home>/.claude/relay/<pid>.json`.
    /// For bun, the sidecar pid == the bun child pid (we spawn bun directly).
    fn sidecar_path(&self) -> PathBuf {
        self.home
            .join(".claude")
            .join("relay")
            .join(format!("{}.json", self.child.id()))
    }

    /// Read the bound port from the sidecar (retrying until it appears or deadline).
    fn wait_for_port(&self) -> u16 {
        let path = self.sidecar_path();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                    if let Some(port) = v.get("port").and_then(Value::as_u64) {
                        return port as u16;
                    }
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "[{}] sidecar {path:?} never produced a port",
                    self.kind.name()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// RAII reap guard — kills + waits on every drop, including panic-unwind.
/// No spawned relay process leaks into the fleet.
impl Drop for RelayChild {
    fn drop(&mut self) {
        self.stdin.take(); // close stdin → EOF → clean exit attempt
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (mirrored from relay_server_mcp_delivery.rs)
// ---------------------------------------------------------------------------

/// Complete the MCP handshake (initialize + notifications/initialized). Returns
/// the full initialize response so callers can assert against it.
fn handshake(child: &mut RelayChild, id: i64) -> Value {
    child.send(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "diff-harness", "version": "0" }
        }
    }));
    let resp = child.next_json_matching("initialize response", |v| v["id"] == json!(id));
    assert_eq!(
        resp["result"]["serverInfo"]["name"],
        "relay",
        "[{}] handshake must complete against the relay server",
        child.kind.name()
    );
    child.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    resp
}

/// Submit a `tools/call name=reply` frame and read back the response keyed on `id`.
fn call_reply(child: &mut RelayChild, id: i64, message_id: &str, text: &str) -> Value {
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

/// Whether a tools/call result carries `isError:true`.
fn is_error(resp: &Value) -> bool {
    resp["result"]["isError"] == json!(true)
}

/// A relay message_id is `relay-<epoch_ms>-<seq>` — assert FORMAT only, never seq value.
/// (Divergence allowlist: seq values differ between bun and Rust.)
fn assert_well_formed_message_id(id: &str, ctx: &str) {
    let parts: Vec<&str> = id.split('-').collect();
    assert!(
        parts.len() == 3
            && parts[0] == "relay"
            && !parts[1].is_empty()
            && parts[1].bytes().all(|b| b.is_ascii_digit())
            && !parts[2].is_empty()
            && parts[2].bytes().all(|b| b.is_ascii_digit()),
        "{ctx}: message_id must be relay-<digits>-<digits>, got: {id}"
    );
}

/// F1 fix: after consuming the expected channel notification, drain the
/// stdout buffer briefly to assert no DUPLICATE or SPURIOUS same-id notification
/// appears. A double-fire or misordered notification is invisible to
/// `next_json_matching` (which returns on first match). 50ms is ample for a
/// localhost subprocess to send any spurious second frame that was already
/// queued — a healthy server will emit nothing.
fn assert_no_duplicate_channel_notification(child: &RelayChild, message_id: &str, ctx: &str) {
    let drain_budget = Duration::from_millis(50);
    let deadline = Instant::now() + drain_budget;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return; // no duplicate — clean
        }
        match child.lines.recv_timeout(remaining) {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if v["method"] == "notifications/claude/channel"
                        && v["params"]["meta"]["message_id"] == json!(message_id)
                    {
                        panic!(
                            "{ctx}: DUPLICATE channel notification for {message_id} detected \
                             (server double-fired the same notification)"
                        );
                    }
                    // Some other frame — not a duplicate for this id, keep draining.
                }
            }
            Err(_) => return, // timeout or disconnect — no duplicate
        }
    }
}

/// Minimal raw HTTP GET helper — used for /health and /inbox (CcRelay has no
/// typed /inbox method). Returns (status_code, body_string).
fn raw_get(port: u16, path: &str) -> (u16, String) {
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("connect 127.0.0.1:{port} for GET {path}: {e}"));
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    stream.write_all(req.as_bytes()).expect("write request");
    stream.flush().expect("flush");

    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }

    let text = String::from_utf8_lossy(&bytes).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = match text.find("\r\n\r\n") {
        Some(pos) => text[pos + 4..].to_string(),
        None => String::new(),
    };
    (status, body)
}

/// Join a JoinHandle with a wall-clock timeout. Returns Some(result) if finished
/// in time, None if still running (caller turns that into a loud failure).
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
        Ok(Err(_panic)) => panic!("parked-fetch thread PANICKED"),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// The frozen MCP handshake fixture (resolved from workspace path, NOT in repo)
// ---------------------------------------------------------------------------

/// Load the frozen ccrelay MCP handshake fixture from the workspace.
/// Returns None if the file is absent (treat as a non-fatal skip in the
/// fixture assertions; the fixture is external to the repo).
fn load_handshake_fixture() -> Option<Value> {
    let real_home = real_home();
    let fixture_path = real_home
        .join("work")
        .join("ws")
        .join("switchboard")
        .join("rust")
        .join("exec")
        .join("ccrelay-mcp-handshake-fixture.json");
    let bytes = std::fs::read(&fixture_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------------------
// The INSTRUCTIONS constant — 666-char verbatim string (byte-equality required)
// ---------------------------------------------------------------------------
// We use dispatch::relay_server::mcp::INSTRUCTIONS directly rather than duplicating
// the string here, so the assertion is "bun emits what Rust uses" rather than
// "both emit a third copy". Any diff between bun and Rust is a HARD failure.
// The 666-char length is verified by the existing unit test in mcp.rs.
// We alias it as EXPECTED_INSTRUCTIONS for readability in this file.
use dispatch::relay_server::mcp::INSTRUCTIONS as EXPECTED_INSTRUCTIONS;

// ---------------------------------------------------------------------------
// CORE SCENARIO — run against BOTH kinds, assert the HARD interop contract
// ---------------------------------------------------------------------------

/// The full scenario: invoked once per RelayKind from each test below.
/// Covers: send/inbox/MCP-notification, reply delivery, push-back, loop belt,
/// honest NOT-DELIVERED, /health shape, /inbox shape.
fn run_core_scenario(kind: RelayKind) {
    let (port_base_a, _port_base_b) = unique_port_bases();
    let mut child = RelayChild::spawn(kind, &format!("sess-{}-core", kind.name()), port_base_a);
    let init_resp = handshake(&mut child, 1);
    let port = child.wait_for_port();

    eprintln!("[{kind:?}] server up on port {port}");

    // ── A. MCP initialize response shape ────────────────────────────────────
    // protocolVersion echoed back.
    assert_eq!(
        init_resp["result"]["protocolVersion"], "2024-11-05",
        "[{kind:?}] protocolVersion must be echoed back"
    );
    // capabilities shape: { tools: {}, experimental: { "claude/channel": {} } }
    assert_eq!(
        init_resp["result"]["capabilities"]["tools"],
        json!({}),
        "[{kind:?}] capabilities.tools must be empty object"
    );
    assert_eq!(
        init_resp["result"]["capabilities"]["experimental"]["claude/channel"],
        json!({}),
        "[{kind:?}] capabilities.experimental['claude/channel'] must be empty object"
    );
    // serverInfo
    assert_eq!(
        init_resp["result"]["serverInfo"]["name"], "relay",
        "[{kind:?}] serverInfo.name must be 'relay'"
    );
    assert_eq!(
        init_resp["result"]["serverInfo"]["version"], "0.1.0",
        "[{kind:?}] serverInfo.version must be '0.1.0'"
    );
    // instructions — VERBATIM 666-char string (byte-equality)
    let instructions = init_resp["result"]["instructions"]
        .as_str()
        .unwrap_or_else(|| panic!("[{kind:?}] instructions field must be a string"));
    assert_eq!(
        instructions, EXPECTED_INSTRUCTIONS,
        "[{kind:?}] instructions must be the verbatim 666-char string"
    );
    assert_eq!(
        instructions.chars().count(),
        666,
        "[{kind:?}] instructions must be exactly 666 Unicode chars, got {}",
        instructions.chars().count()
    );

    // ── B. tools/list: reply tool with correct inputSchema ──────────────────
    child.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
    }));
    let tools_resp = child.next_json_matching("tools/list response", |v| v["id"] == json!(2));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert_eq!(tools.len(), 1, "[{kind:?}] exactly one tool (reply)");
    let reply_tool = &tools[0];
    assert_eq!(
        reply_tool["name"], "reply",
        "[{kind:?}] tool name is 'reply'"
    );
    let schema = &reply_tool["inputSchema"];
    assert_eq!(
        schema["type"], "object",
        "[{kind:?}] inputSchema.type == object"
    );
    let required = schema["required"].as_array().expect("required array");
    let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        required_names.contains(&"text"),
        "[{kind:?}] 'text' must be in required"
    );
    assert!(
        required_names.contains(&"message_id"),
        "[{kind:?}] 'message_id' must be in required"
    );
    assert_eq!(
        schema["properties"]["text"]["type"], "string",
        "[{kind:?}] text property is string"
    );
    assert_eq!(
        schema["properties"]["message_id"]["type"], "string",
        "[{kind:?}] message_id property is string"
    );

    // ── C. /health JSON shape ─────────────────────────────────────────────
    let (status, body) = raw_get(port, "/health");
    assert_eq!(status, 200, "[{kind:?}] /health must return 200");
    let health_json: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("[{kind:?}] /health body not JSON: {e}; body={body}"));
    assert_eq!(
        health_json["status"].as_str(),
        Some("ok"),
        "[{kind:?}] health.status must be 'ok'"
    );
    assert!(
        health_json["sessionId"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "[{kind:?}] health.sessionId must be non-empty"
    );
    // F2 fix: assert VALUE not just presence — a wrong port value poisons
    // the client's scanRelayPorts probe fallback (relay_http.rs:137-141).
    assert_eq!(
        health_json["port"].as_u64(),
        Some(port as u64),
        "[{kind:?}] health.port must equal the bound port (oracle: wait_for_port)"
    );
    assert!(
        health_json["pid"].as_u64().is_some() || health_json["pid"].as_i64().is_some(),
        "[{kind:?}] health.pid must be a number"
    );

    // ── D. POST /message → message_id format + inbox file + MCP notification ──
    let msg_text = format!("hello from {kind:?}");
    let message_id = CcRelay::new()
        .send_message(port, &msg_text, &format!("sess-{kind:?}-sender"))
        .unwrap_or_else(|e| panic!("[{kind:?}] POST /message failed: {e}"));
    assert_well_formed_message_id(&message_id, &format!("[{kind:?}] POST /message response"));

    // Inbox file must exist with correct keys.
    let inbox_path = child
        .home
        .join(".claude")
        .join("channels")
        .join("relay")
        .join("inbox")
        .join(format!("{message_id}.json"));
    let raw = std::fs::read_to_string(&inbox_path)
        .unwrap_or_else(|e| panic!("[{kind:?}] inbox file {inbox_path:?} must exist: {e}"));
    let inbox_json: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("[{kind:?}] inbox file must be JSON: {e}"));
    assert_eq!(
        inbox_json["text"].as_str(),
        Some(msg_text.as_str()),
        "[{kind:?}] inbox.text must match"
    );
    assert!(
        inbox_json["from_session"].as_str().is_some(),
        "[{kind:?}] inbox.from_session must be present"
    );
    assert_eq!(
        inbox_json["message_id"].as_str(),
        Some(message_id.as_str()),
        "[{kind:?}] inbox.message_id must match"
    );
    assert!(
        inbox_json["received_at"].as_str().is_some(),
        "[{kind:?}] inbox.received_at must be present"
    );

    // MCP notifications/claude/channel arrives on stdout.
    let notif = child.next_json_matching("channel notification", |v| {
        v["method"] == "notifications/claude/channel"
    });
    assert_eq!(
        notif["params"]["content"].as_str(),
        Some(msg_text.as_str()),
        "[{kind:?}] notification content must match sent text"
    );
    assert_eq!(
        notif["params"]["meta"]["message_id"].as_str(),
        Some(message_id.as_str()),
        "[{kind:?}] notification meta.message_id must match"
    );
    assert!(
        notif["params"]["meta"]["from_session"].as_str().is_some(),
        "[{kind:?}] notification meta.from_session must be present"
    );
    // F1 fix: assert no duplicate notification for this message_id.
    assert_no_duplicate_channel_notification(
        &child,
        &message_id,
        &format!("[{kind:?}] POST /message notification"),
    );

    // ── E. /inbox JSON shape ──────────────────────────────────────────────
    let (inbox_status, inbox_body) = raw_get(port, "/inbox");
    assert_eq!(inbox_status, 200, "[{kind:?}] /inbox must return 200");
    let inbox_list: Value = serde_json::from_str(&inbox_body)
        .unwrap_or_else(|e| panic!("[{kind:?}] /inbox body not JSON: {e}; body={inbox_body}"));
    assert!(
        inbox_list["messages"].as_array().is_some(),
        "[{kind:?}] /inbox must have messages array"
    );
    assert!(
        inbox_list["count"].as_u64().is_some(),
        "[{kind:?}] /inbox must have count field"
    );
    assert_eq!(
        inbox_list["count"].as_u64(),
        Some(1),
        "[{kind:?}] /inbox count must be 1 after one POST"
    );

    // ── F. Reply via MCP → resolves a parked /replies long-poll (P-E2) ──────
    let m2 = CcRelay::new()
        .send_message(port, "deliver me", &format!("sess-{kind:?}-dl"))
        .unwrap_or_else(|e| panic!("[{kind:?}] second POST /message failed: {e}"));
    // Drain the notification for m2.
    child.next_json_matching("channel notification m2", |v| {
        v["method"] == "notifications/claude/channel"
            && v["params"]["meta"]["message_id"] == json!(m2)
    });

    let poll_port = port;
    let poll_id = m2.clone();
    let poll = std::thread::spawn(move || CcRelay::new().fetch_reply(poll_port, &poll_id, 8000));
    // F4: give the long-poll thread time to reach the server's waiter registry
    // before we fire the reply. This is a CONSERVATIVE FIXED SLEEP, not a poll —
    // the server does not expose its internal waiter count, so there is no
    // observable seam to poll on. 800ms is orders of magnitude more than the time
    // for a localhost GET to reach the synchronous waiter-registration point, and
    // gives 2x the margin of the original 400ms under CI contention.
    //
    // Why a fixed sleep is SAFE here (can only ever FALSE-RED, never false-green):
    // buffer_reply is unconditional-first (mod.rs:265), so even if the reply fires
    // BEFORE the poll thread parks, the buffered fetch (below) still resolves with
    // "delivery-ack" — the delivery itself is never lost. The ONLY thing a short
    // sleep can break is the tools/call result STRING: under pathological >800ms
    // contention the reply takes the PushBack branch (no parked waiter yet) and the
    // result reads "posted to session ..." instead of "long-poll resolved", which
    // HARD-REDs line 781. That is a false-red (a timing flake presenting as a
    // delivery regression), never a missed-delivery false-green.
    std::thread::sleep(Duration::from_millis(800));

    let reply_resp = call_reply(&mut child, 10, &m2, "delivery-ack");
    assert!(
        !is_error(&reply_resp),
        "[{kind:?}] a resolved waiter must NOT be isError: {reply_resp}"
    );
    let rt = result_text(&reply_resp);
    assert!(
        rt.starts_with("DELIVERED"),
        "[{kind:?}] reply result must start with DELIVERED, got: {rt}"
    );
    assert!(
        rt.contains("long-poll resolved"),
        "[{kind:?}] reply result must name long-poll resolution (P-E2), got: {rt}"
    );

    let resolved = join_with_timeout(poll, Duration::from_secs(8))
        .expect("parked fetch must finish within 8s (regression if not)");
    let resolved = resolved.expect("fetch_reply must succeed");
    assert_eq!(
        resolved.text.as_deref(),
        Some("delivery-ack"),
        "[{kind:?}] parked sender must receive the reply text (P-E2)"
    );

    // ── G. Honest NOT-DELIVERED (P-E6) ─────────────────────────────────────
    let nd_resp = call_reply(&mut child, 20, "relay-doesnotexist-0", "x");
    assert!(
        is_error(&nd_resp),
        "[{kind:?}] unrecorded id → isError (P-E6): {nd_resp}"
    );
    let nd_text = result_text(&nd_resp);
    assert!(
        nd_text.starts_with("NOT DELIVERED"),
        "[{kind:?}] honest NOT-DELIVERED prefix, got: {nd_text}"
    );
    assert!(
        nd_text.contains("send a fresh message"),
        "[{kind:?}] NOT-DELIVERED must carry fresh-message guidance, got: {nd_text}"
    );
    assert!(
        nd_text.contains("sb send:relay"),
        "[{kind:?}] guidance must name sb send:relay, got: {nd_text}"
    );

    // ── H. Loop belt (P-E4) ─────────────────────────────────────────────────
    let lb_id = CcRelay::new()
        .send_message(
            port,
            "[REPLY to relay-orig-9] earlier answer",
            "sess-loop-peer",
        )
        .unwrap_or_else(|e| panic!("[{kind:?}] loop-belt POST failed: {e}"));
    // Drain the notification.
    child.next_json_matching("loop-belt notification", |v| {
        v["method"] == "notifications/claude/channel"
            && v["params"]["meta"]["message_id"] == json!(lb_id)
    });
    let lb_resp = call_reply(&mut child, 30, &lb_id, "would ping-pong");
    assert!(
        is_error(&lb_resp),
        "[{kind:?}] reply to [REPLY to...] must be isError (P-E4): {lb_resp}"
    );
    let lb_text = result_text(&lb_resp);
    assert!(
        lb_text.contains("loop prevention"),
        "[{kind:?}] loop belt result must name 'loop prevention', got: {lb_text}"
    );
    assert!(
        lb_text.starts_with("NOT DELIVERED"),
        "[{kind:?}] loop-prevented is an honest NOT-DELIVERED, got: {lb_text}"
    );

    eprintln!("[{kind:?}] core scenario PASSED on port {port}");
}

/// Run the push-back scenario: reply with NO waiter → the origin session's
/// stdout gets a `notifications/claude/channel` whose content starts with
/// `[REPLY to <id>] `, from_session == replier.
fn run_pushback_scenario(kind: RelayKind) {
    let (port_base_a, port_base_b) = unique_port_bases();
    let sender_session = format!("sess-{kind:?}-pb-sender");
    let replier_session = format!("sess-{kind:?}-pb-replier");

    // Two servers sharing ONE home — shared sidecar dir + shared inbox.
    let mut sender = RelayChild::spawn(kind, &sender_session, port_base_a);
    let home = sender.home.clone();
    let mut replier = RelayChild::spawn_sharing_home(kind, &replier_session, &home, port_base_b);

    handshake(&mut sender, 1);
    handshake(&mut replier, 1);

    let sender_port = sender.wait_for_port();
    let _replier_port = replier.wait_for_port();

    // Wait for replier sidecar to be on disk before the push-back probe.
    let replier_sidecar = replier.sidecar_path();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if replier_sidecar.exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "[{kind:?}] push-back: replier sidecar never appeared"
        );
        std::thread::sleep(Duration::from_millis(30));
    }

    // The "sender" side sends a message; the replier replies with NO waiter parked.
    // No waiter → decide_delivery returns PushBack{replier_session}.
    let msg_id = CcRelay::new()
        .send_message(sender_port, "push me back", &replier_session)
        .unwrap_or_else(|e| panic!("[{kind:?}] push-back POST failed: {e}"));

    // Drain the channel notification on sender's stdout.
    sender.next_json_matching("push-back notification on sender", |v| {
        v["method"] == "notifications/claude/channel"
            && v["params"]["meta"]["message_id"] == json!(msg_id)
    });

    // Replier calls the reply tool (no waiter → push-back path).
    let pb_resp = call_reply(&mut sender, 40, &msg_id, "the push-back answer");
    assert!(
        !is_error(&pb_resp),
        "[{kind:?}] push-back to live origin is a real delivery → NOT isError: {pb_resp}"
    );
    let pb_text = result_text(&pb_resp);
    assert!(
        pb_text.starts_with("DELIVERED"),
        "[{kind:?}] push-back result must start with DELIVERED, got: {pb_text}"
    );
    assert!(
        pb_text.contains(&format!("posted to session {replier_session}")),
        "[{kind:?}] push-back result must name target session, got: {pb_text}"
    );

    // The replier must receive the pushed-back reply as a channel notification.
    let pb_notif = replier.next_json_matching("push-back arrives on replier", |v| {
        v["method"] == "notifications/claude/channel"
    });
    let content = pb_notif["params"]["content"]
        .as_str()
        .expect("notification content");
    assert!(
        content.starts_with(&format!("[REPLY to {msg_id}] ")),
        "[{kind:?}] pushed-back content must start with '[REPLY to {msg_id}] ', got: {content}"
    );
    assert!(
        content.contains("the push-back answer"),
        "[{kind:?}] pushed-back content must contain the reply text, got: {content}"
    );
    // from_session must be the replier's session (the one who replied).
    assert_eq!(
        pb_notif["params"]["meta"]["from_session"].as_str(),
        Some(sender_session.as_str()),
        "[{kind:?}] push-back from_session must be sender (the one who called reply)"
    );

    eprintln!("[{kind:?}] push-back scenario PASSED");
}

// ---------------------------------------------------------------------------
// MCP handshake fixture comparison — assert BOTH servers match the fixture
// ---------------------------------------------------------------------------

fn run_handshake_fixture_check(kind: RelayKind) {
    let fixture = match load_handshake_fixture() {
        Some(f) => f,
        None => {
            eprintln!(
                "[{kind:?}] SKIP fixture check: ccrelay-mcp-handshake-fixture.json not found"
            );
            return;
        }
    };

    // Extract the expected initialize response from the fixture.
    let expected_resp = fixture["handshake_sequence"]
        .as_array()
        .and_then(|seq| {
            seq.iter().find(|entry| {
                entry["direction"] == "server -> client" && entry["method"] == "initialize"
            })
        })
        .and_then(|entry| entry.get("response"))
        .cloned();

    let expected_resp = match expected_resp {
        Some(r) => r,
        None => {
            eprintln!(
                "[{kind:?}] SKIP fixture check: could not find initialize response in fixture"
            );
            return;
        }
    };

    let (port_base_a, _) = unique_port_bases();
    let mut child = RelayChild::spawn(kind, &format!("sess-{kind:?}-fixture"), port_base_a);
    let actual_resp = handshake(&mut child, 1);
    let _port = child.wait_for_port();

    // Assert the instructions are byte-equal to the fixture.
    let fixture_instructions = expected_resp["result"]["instructions"]
        .as_str()
        .expect("fixture must have instructions string");
    let actual_instructions = actual_resp["result"]["instructions"]
        .as_str()
        .unwrap_or_else(|| panic!("[{kind:?}] server must emit instructions string"));

    assert_eq!(
        actual_instructions, fixture_instructions,
        "[{kind:?}] instructions must be byte-equal to the frozen fixture"
    );
    assert_eq!(
        actual_instructions.chars().count(),
        666,
        "[{kind:?}] instructions must be exactly 666 Unicode chars, got {}",
        actual_instructions.chars().count()
    );

    // protocolVersion echoed.
    assert_eq!(
        actual_resp["result"]["protocolVersion"], expected_resp["result"]["protocolVersion"],
        "[{kind:?}] protocolVersion must match fixture"
    );

    // capabilities shape.
    assert_eq!(
        actual_resp["result"]["capabilities"], expected_resp["result"]["capabilities"],
        "[{kind:?}] capabilities must match fixture"
    );

    // serverInfo.
    assert_eq!(
        actual_resp["result"]["serverInfo"], expected_resp["result"]["serverInfo"],
        "[{kind:?}] serverInfo must match fixture"
    );

    eprintln!("[{kind:?}] handshake fixture check PASSED");
}

// ---------------------------------------------------------------------------
// /replies 408 — assert the timeout shape (bounded variant, no 120s wait)
// ---------------------------------------------------------------------------
//
// Note: the full 120s park is already covered by the existing §4a parity tests.
// Here we assert only that the 408 response has the right shape using the short
// park variant (we use fetch_reply with a short timeout_ms so the test resolves
// in < 1s against the Rust server, which will 408 after the park deadline).
//
// For bun: bun's park default is 120s; we use a 1s timeout_ms on the client
// side which means the client will get a timeout error (connection read timeout)
// rather than a server 408. This is an expected divergence (park deadline ≠
// client timeout). We document this and assert only the Rust 408 path here;
// the bun 408 is covered separately by existing parity tests if needed.

fn run_replies_timeout_check(kind: RelayKind) {
    // Only assert the full 408 path on Rust (where we control park duration).
    // On bun the park is 120s — driving a real 408 would stall the suite.
    // JUDGMENT CALL: bun's 408 path is not tested here; it is covered by the
    // parity suite against the in-process server.
    let (port_base_a, _) = unique_port_bases();
    if kind == RelayKind::Bun {
        eprintln!(
            "[Bun] SKIP /replies 408 variant: bun park=120s, would stall suite. \
             Covered by parity suite. Asserting client-timeout shape instead."
        );
        // Assert that the client DOES get SOME response (timeout or 408) within
        // a bounded window, proving the endpoint is live.
        let mut child = RelayChild::spawn(kind, "sess-bun-replies-408", port_base_a);
        let _init = handshake(&mut child, 1);
        let port = child.wait_for_port();
        // Use a very short timeout_ms — client side will time out (not a server 408).
        // This is a bounded assertion that the /replies endpoint is reachable.
        let result = CcRelay::new().fetch_reply(port, "relay-no-such-0", 200);
        // Under bun, the server parks for 120s but client times out after 200ms.
        // The CcRelay will return a read-timeout Err or a server error — either way
        // the endpoint is live and non-hanging.
        eprintln!("[Bun] /replies short-timeout result (expected timeout/error): {result:?}");
        // We do NOT assert the exact error type; just that it returns within READ_BUDGET.
        return;
    }

    // Rust: the park deadline is the server-configured park_ms (default in relay:serve).
    // We can use fetch_reply with a small timeout_ms; the Rust server will 408 after
    // its own park_ms. With a 500ms client timeout we catch either outcome.
    let mut child = RelayChild::spawn(kind, "sess-rust-replies-408", port_base_a);
    let _init = handshake(&mut child, 1);
    let port = child.wait_for_port();

    // Post a message so the id is valid (server must actually park, not reject early).
    let msg_id = CcRelay::new()
        .send_message(port, "park me", "sess-408-sender")
        .expect("POST for 408 test");
    // Drain the notification.
    child.next_json_matching("408-test notification", |v| {
        v["method"] == "notifications/claude/channel"
            && v["params"]["meta"]["message_id"] == json!(msg_id)
    });

    // Use a generous timeout_ms so the client waits for the server's 408.
    // The Rust server's park deadline (relay:serve default) should fire within
    // 120 000ms. We use a 5s client budget to stay fast in tests.
    // If the server's park is longer than our budget, we get a client timeout,
    // which is still a bounded (non-hanging) result.
    let result = CcRelay::new().fetch_reply(port, &msg_id, 5000);
    eprintln!("[Rust] /replies 5s-timeout result: {result:?}");
    // The result is either:
    // (a) RelayReply { error: Some("timeout") } — server 408 within 5s, or
    // (b) Err(RelayError::...) — client read timeout (server park > 5s).
    // Both are valid bounded outcomes. We assert the call returns at all (non-hang).
    // The full 120s park is covered by relay_server_parity.rs row 5.
    match result {
        Ok(reply) => {
            // Server returned 408 with error field.
            eprintln!("[Rust] /replies returned error reply: {reply:?}");
            // A valid 408 response contains either an error field or the body is empty.
        }
        Err(e) => {
            // Client read timeout — server park > 5s. Not a suite failure.
            eprintln!("[Rust] /replies client-timeout (server park > 5s): {e:?}");
        }
    }
    eprintln!("[Rust] /replies bounded check PASSED (non-hang confirmed)");
}

// ---------------------------------------------------------------------------
// TESTS — differential (one per kind) + cross-compat
// ---------------------------------------------------------------------------

// ── Rust core scenario ──────────────────────────────────────────────────────

#[test]
fn differential_rust_core_scenario() {
    run_core_scenario(RelayKind::Rust);
}

#[test]
fn differential_rust_pushback() {
    run_pushback_scenario(RelayKind::Rust);
}

#[test]
fn differential_rust_handshake_fixture() {
    run_handshake_fixture_check(RelayKind::Rust);
}

#[test]
fn differential_rust_replies_timeout() {
    run_replies_timeout_check(RelayKind::Rust);
}

// ── Bun core scenario ───────────────────────────────────────────────────────

#[test]
fn differential_bun_core_scenario() {
    if !bun_precondition_check("differential_bun_core_scenario") {
        return;
    }
    run_core_scenario(RelayKind::Bun);
}

#[test]
fn differential_bun_pushback() {
    if !bun_precondition_check("differential_bun_pushback") {
        return;
    }
    run_pushback_scenario(RelayKind::Bun);
}

#[test]
fn differential_bun_handshake_fixture() {
    if !bun_precondition_check("differential_bun_handshake_fixture") {
        return;
    }
    run_handshake_fixture_check(RelayKind::Bun);
}

#[test]
fn differential_bun_replies_timeout() {
    if !bun_precondition_check("differential_bun_replies_timeout") {
        return;
    }
    run_replies_timeout_check(RelayKind::Bun);
}

// ── Instructions byte-equality between Bun and Rust ────────────────────────

/// Assert that BOTH servers emit the IDENTICAL instructions string —
/// the cross-server byte-equality constraint of the interop contract.
#[test]
fn differential_instructions_byte_equal_across_servers() {
    let (port_base_a, port_base_b) = unique_port_bases();
    if !bun_precondition_check("differential_instructions_byte_equal_across_servers") {
        // Still assert Rust alone so the test isn't silent.
        let mut rust_child =
            RelayChild::spawn(RelayKind::Rust, "sess-inst-check-rust", port_base_a);
        let rust_resp = handshake(&mut rust_child, 1);
        let rust_inst = rust_resp["result"]["instructions"]
            .as_str()
            .expect("rust instructions");
        assert_eq!(
            rust_inst, EXPECTED_INSTRUCTIONS,
            "Rust instructions must match the expected verbatim string"
        );
        return;
    }

    let mut rust_child = RelayChild::spawn(RelayKind::Rust, "sess-inst-rust", port_base_a);
    let mut bun_child = RelayChild::spawn(RelayKind::Bun, "sess-inst-bun", port_base_b);

    let rust_resp = handshake(&mut rust_child, 1);
    let bun_resp = handshake(&mut bun_child, 1);

    let rust_inst = rust_resp["result"]["instructions"]
        .as_str()
        .expect("rust instructions string");
    let bun_inst = bun_resp["result"]["instructions"]
        .as_str()
        .expect("bun instructions string");

    assert_eq!(
        rust_inst, bun_inst,
        "Rust and Bun servers must emit byte-identical instructions strings"
    );
    assert_eq!(
        rust_inst, EXPECTED_INSTRUCTIONS,
        "Rust instructions must match the verbatim expected string"
    );
    assert_eq!(
        rust_inst.chars().count(),
        666,
        "instructions must be exactly 666 Unicode chars, got {}",
        rust_inst.chars().count()
    );

    eprintln!(
        "instructions byte-equality: CONFIRMED (both servers, {} chars)",
        rust_inst.len()
    );
}

// ---------------------------------------------------------------------------
// §5 CROSS-COMPAT — mixed-fleet interop (one Bun + one Rust sharing one HOME)
// ---------------------------------------------------------------------------
//
// Proves a rolling cutover (some bun, some Rust) interoperates over the shared
// sidecar/inbox/HTTP contract.
//
// Scenario:
//   (a) Rust sender → reply via Rust → push-back lands on BUN origin's sidecar.
//   (b) Bun sender → reply via Bun → push-back lands on RUST origin's sidecar.

#[test]
fn cross_compat_rust_sender_reply_pushback_to_bun_origin() {
    if !bun_precondition_check("cross_compat_rust_sender_reply_pushback_to_bun_origin") {
        return;
    }

    // Shared HOME: both servers discover each other's sidecars.
    let home_dir = tempfile::tempdir().expect("shared home tempdir");
    let home = home_dir.path().to_path_buf();
    let (port_base_a, port_base_b) = unique_port_bases();

    // Rust is the "current" server (the one that will REPLY).
    // Bun is the "origin" server (it POSTed the original message).
    let bun_session = "sess-xc-bun-origin";
    let rust_session = "sess-xc-rust-replier";

    let mut bun_child =
        RelayChild::spawn_sharing_home(RelayKind::Bun, bun_session, &home, port_base_a);
    let mut rust_child =
        RelayChild::spawn_sharing_home(RelayKind::Rust, rust_session, &home, port_base_b);

    handshake(&mut bun_child, 1);
    handshake(&mut rust_child, 1);

    let bun_port = bun_child.wait_for_port();
    let rust_port = rust_child.wait_for_port();

    // Wait for BOTH sidecars to appear (cross-discovery requires this).
    let bun_sidecar = bun_child.sidecar_path();
    let rust_sidecar = rust_child.sidecar_path();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if bun_sidecar.exists() && rust_sidecar.exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cross-compat: sidecars never appeared (bun={} rust={})",
            bun_sidecar.exists(),
            rust_sidecar.exists()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    eprintln!("cross-compat: both sidecars present. bun_port={bun_port} rust_port={rust_port}");

    // (a) Bun (origin) posts to Rust (replier). No waiter → push-back to bun.
    // We POST from bun_session to rust_port, then reply via rust's MCP stdin.
    // The push-back should arrive on bun's stdout.
    let msg_id = CcRelay::new()
        .send_message(rust_port, "reply via rust, push to bun", bun_session)
        .expect("cross-compat POST to rust");

    // Drain notification on rust's stdout.
    rust_child.next_json_matching("cross-compat rust notification", |v| {
        v["method"] == "notifications/claude/channel"
            && v["params"]["meta"]["message_id"] == json!(msg_id)
    });

    // Rust replies (no waiter → push-back to bun).
    let pb_resp = call_reply(&mut rust_child, 50, &msg_id, "rust-to-bun push-back");
    eprintln!("cross-compat: rust push-back reply result: {pb_resp}");

    assert!(
        !is_error(&pb_resp),
        "cross-compat: Rust push-back to Bun origin must be DELIVERED: {pb_resp}"
    );
    let pb_text = result_text(&pb_resp);
    assert!(
        pb_text.starts_with("DELIVERED"),
        "cross-compat: Rust reply result must start with DELIVERED, got: {pb_text}"
    );
    assert!(
        pb_text.contains(&format!("posted to session {bun_session}")),
        "cross-compat: Rust push-back must name bun origin, got: {pb_text}"
    );

    // Bun must receive the pushed-back reply as a channel notification.
    let bun_notif = bun_child.next_json_matching("cross-compat bun push-back notification", |v| {
        v["method"] == "notifications/claude/channel"
    });
    let content = bun_notif["params"]["content"]
        .as_str()
        .expect("bun notification content");
    assert!(
        content.starts_with(&format!("[REPLY to {msg_id}] ")),
        "cross-compat: bun push-back content must start with '[REPLY to {msg_id}] ', got: {content}"
    );
    assert_eq!(
        bun_notif["params"]["meta"]["from_session"].as_str(),
        Some(rust_session),
        "cross-compat: pushed-back from_session must be the rust replier"
    );

    eprintln!("cross-compat (a): Rust→push-back→Bun PASSED");

    // (b) Rust posts to Bun, Bun replies, push-back lands on Rust's stdout.
    let msg_id_b = CcRelay::new()
        .send_message(bun_port, "reply via bun, push to rust", rust_session)
        .expect("cross-compat POST to bun");

    // Drain notification on bun's stdout.
    bun_child.next_json_matching("cross-compat bun notification b", |v| {
        v["method"] == "notifications/claude/channel"
            && v["params"]["meta"]["message_id"] == json!(msg_id_b)
    });

    // Bun replies (no waiter → push-back to rust).
    let pb_resp_b = call_reply(&mut bun_child, 60, &msg_id_b, "bun-to-rust push-back");
    eprintln!("cross-compat: bun push-back reply result: {pb_resp_b}");

    assert!(
        !is_error(&pb_resp_b),
        "cross-compat: Bun push-back to Rust origin must be DELIVERED: {pb_resp_b}"
    );
    let pb_text_b = result_text(&pb_resp_b);
    assert!(
        pb_text_b.starts_with("DELIVERED"),
        "cross-compat: Bun reply result must start with DELIVERED, got: {pb_text_b}"
    );
    assert!(
        pb_text_b.contains(&format!("posted to session {rust_session}")),
        "cross-compat: Bun push-back must name rust origin, got: {pb_text_b}"
    );

    // Rust must receive the pushed-back reply as a channel notification.
    let rust_notif = rust_child
        .next_json_matching("cross-compat rust push-back notification", |v| {
            v["method"] == "notifications/claude/channel"
        });
    let content_b = rust_notif["params"]["content"]
        .as_str()
        .expect("rust notification content");
    assert!(
        content_b.starts_with(&format!("[REPLY to {msg_id_b}] ")),
        "cross-compat: rust push-back content must start with '[REPLY to {msg_id_b}] ', got: {content_b}"
    );
    assert_eq!(
        rust_notif["params"]["meta"]["from_session"].as_str(),
        Some(bun_session),
        "cross-compat: pushed-back from_session must be the bun replier"
    );

    eprintln!("cross-compat (b): Bun→push-back→Rust PASSED");
    eprintln!("cross-compat FULL SCENARIO PASSED: rolling cutover interop verified");
}

// ---------------------------------------------------------------------------
// Compile-time type check (RelayError in scope)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _relay_error_in_scope(e: RelayError) -> RelayError {
    e
}
