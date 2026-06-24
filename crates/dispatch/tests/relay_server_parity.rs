//! §4a parity harness — the REAL Rust relay server driven by the REAL `CcRelay`
//! client (spec §4.4a). This is the INVERSE of `tests/relay_contract.rs`: that
//! file drives the frozen client against an in-process *fake* server; here we
//! spawn the REAL `relay:serve` HTTP server (`RelayServer::spawn_for_test`) and
//! drive it with the SAME unchanged `CcRelay` client the fleet uses. The whole
//! point: prove the server speaks EXACTLY what the frozen reference consumer
//! expects.
//!
//! Written by QA (checker), SEPARATE from the M2 server implementer. This file
//! ADDS scenario rows only — it never modifies the server. Any divergence found
//! is REPORTED, not fixed.
//!
//! ## What M2 covers (and what it does not)
//! M2 is HTTP-only — there is NO MCP stdio loop and NO `reply` tool yet, so NO
//! waiter is ever resolved by a real reply (that is M4). Every `/replies`
//! long-poll therefore times out to 408 UNLESS a reply was buffered directly
//! into state (the legitimate way to simulate what M4's reply tool will do:
//! `handle.server.state.lock().unwrap().buffer_reply(..)`).
//!
//! ## Scenario rows (each cites its P-assertion + server.ts anchor)
//! 1. /health roundtrip (P-A1, server.ts:251-253)
//! 2. POST /message + inbox persist (P-A2/P-C1, server.ts:255-298 / 268-279)
//! 3. /inbox count==2 (P-A4, server.ts:320-331)
//! 4. /replies cached branch + idempotent re-peek (P-A3 cached, server.ts:303-306)
//! 5. /replies long-poll timeout → 408 (P-A3 timeout / P-F3b, server.ts:308-317)
//! 6. 404 unknown path (P-A5, server.ts:333)
//! 7. hostile-request belts (W2 / §5 red-team: oversized, slow-drip, garbage)
//! 8. concurrent senders distinct ids + inbox files (P-G5 partial, server.ts:258)
//!
//! ## Jail / hermeticity discipline
//! Every row uses `tempfile::tempdir()` for HOME (sidecar + inbox land in temp,
//! never the real ~/.claude) and `port_base = 0` (OS-assigned ephemeral port,
//! outside 8900-9000 so the client's 8900-9000 probe never collides — L: jail
//! ports OUT of 8900-9000). The injected `park` is SHORT (≤200ms) so the 408
//! timeout row runs fast through the SAME production park code path (P-F3b).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use dispatch::relay::{RelayContract, RelayError};
use dispatch::relay_http::CcRelay;
use dispatch::relay_server::RelayServer;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Spin until the spawned server's `/health` answers (the detached listener
/// thread may not have called `accept()` the instant `spawn_for_test` returns).
/// Mirrors the wait loop in the server's in-module smoke. Panics if the server
/// never comes up (a real failure, not a flake — we give it a generous budget).
fn wait_until_up(port: u16) {
    let client = CcRelay::new();
    for _ in 0..100 {
        if client.health(port, 1000).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("server on port {port} never became healthy");
}

/// A minimal RAW HTTP/1.1 response capture: status code + body bytes. Used for
/// the endpoints `CcRelay` has no typed method for (`/inbox`, 404) and for the
/// hostile-request rows where we must send bytes the typed client would never
/// produce. We are the SERVER's counterpart here — a deliberately minimal raw
/// reader, NOT a reuse of the frozen client (which would refuse to send a
/// malformed request).
struct RawResponse {
    status: u16,
    body: String,
}

/// Send `raw_request` bytes to `127.0.0.1:port` over a fresh socket, read the
/// full response to EOF (the server always sends `Connection: close`), and parse
/// out the status code + body. `read_timeout` bounds the whole exchange so a
/// hung server FAILS the test loudly instead of hanging the suite.
fn raw_request(
    port: u16,
    raw_request: &[u8],
    read_timeout: Duration,
) -> std::io::Result<RawResponse> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(read_timeout))?;
    stream.write_all(raw_request)?;
    stream.flush()?;

    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.len() > 4 * 1024 * 1024 {
                    break; // never balloon — bounded read
                }
            }
            Err(_) => break, // timeout / reset → return what we have
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
    Ok(RawResponse { status, body })
}

/// Convenience: a raw GET of `path` against `port`, bounded.
fn raw_get(port: u16, path: &str, read_timeout: Duration) -> std::io::Result<RawResponse> {
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    raw_request(port, req.as_bytes(), read_timeout)
}

/// The inbox-file path for a minted message id under `home`.
fn inbox_file(home: &Path, message_id: &str) -> std::path::PathBuf {
    home.join(".claude")
        .join("channels")
        .join("relay")
        .join("inbox")
        .join(format!("{message_id}.json"))
}

// ---------------------------------------------------------------------------
// Row 1 — /health roundtrip (P-A1, server.ts:251-253)
// ---------------------------------------------------------------------------

/// The frozen client's `health()` round-trips against the REAL server: a
/// non-empty session_id, the bound port echoed, status "ok". `health()` rejects
/// an empty sessionId as BadResponse, so a passing row proves the server emits a
/// usable record. P-A1.
#[test]
fn health_roundtrips_via_real_client() {
    let home = tempfile::tempdir().expect("tempdir");
    let handle = RelayServer::spawn_for_test(
        home.path(),
        0,
        Duration::from_millis(150),
        Duration::from_secs(10),
    );
    wait_until_up(handle.port);

    let client = CcRelay::new();
    let health = client
        .health(handle.port, 1000)
        .expect("/health must round-trip via the frozen client");

    assert_eq!(
        health.port, handle.port,
        "health.port must echo the bound port"
    );
    assert!(
        !health.session_id.is_empty(),
        "sessionId must be non-empty (client rejects empty)"
    );
    assert_eq!(health.status, "ok");

    handle.shutdown();
}

// ---------------------------------------------------------------------------
// Row 2 — POST /message + persist-before-notify inbox file (P-A2/P-C1,
// server.ts:255-298 / 268-279)
// ---------------------------------------------------------------------------

/// `send_message` returns a `relay-<digits>-<digits>` message_id (P-A2), AND the
/// inbox file is written with the full record (P-C1 durability belt). We assert
/// BOTH the wire response and the on-disk persistence — the persist-before-notify
/// ordering is load-bearing (a notify failure stays recoverable from the inbox).
#[test]
fn post_message_mints_id_and_persists_inbox_file() {
    let home = tempfile::tempdir().expect("tempdir");
    let handle = RelayServer::spawn_for_test(
        home.path(),
        0,
        Duration::from_millis(150),
        Duration::from_secs(10),
    );
    wait_until_up(handle.port);

    let client = CcRelay::new();
    let id = client
        .send_message(handle.port, "hello", "sess-A")
        .expect("send_message must return a message_id");

    // P-A2: message_id shape is `relay-<digits>-<digits>`.
    assert!(!id.is_empty(), "message_id must be non-empty");
    let parts: Vec<&str> = id.split('-').collect();
    assert_eq!(parts.len(), 3, "id must be relay-<ms>-<seq>, got {id}");
    assert_eq!(parts[0], "relay", "id must start with 'relay', got {id}");
    assert!(
        !parts[1].is_empty() && parts[1].chars().all(|c| c.is_ascii_digit()),
        "epoch_ms segment must be all digits, got {id}"
    );
    assert!(
        !parts[2].is_empty() && parts[2].chars().all(|c| c.is_ascii_digit()),
        "seq segment must be all digits, got {id}"
    );

    // P-C1: the inbox file was persisted with text + from_session + message_id +
    // received_at. Read it directly off disk (under the temp HOME).
    let path = inbox_file(home.path(), &id);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("inbox file {path:?} must exist (persist-before-notify): {e}"));
    let json: serde_json::Value = serde_json::from_str(&raw).expect("inbox file must be JSON");
    assert_eq!(json.get("text").and_then(|v| v.as_str()), Some("hello"));
    assert_eq!(
        json.get("from_session").and_then(|v| v.as_str()),
        Some("sess-A")
    );
    assert_eq!(
        json.get("message_id").and_then(|v| v.as_str()),
        Some(id.as_str())
    );
    assert!(
        json.get("received_at").and_then(|v| v.as_str()).is_some(),
        "received_at must be present"
    );

    handle.shutdown();
}

// ---------------------------------------------------------------------------
// Row 3 — /inbox count==2 with both messages (P-A4, server.ts:320-331)
// ---------------------------------------------------------------------------

/// After sending 2 messages, a RAW `GET /inbox` returns `{messages:[...], count:2}`
/// with both texts present. The client has no /inbox method, so we exercise the
/// endpoint with a raw socket GET (the real server handler, not the on-disk
/// shortcut — we want to prove the /inbox endpoint itself reads + serializes).
/// P-A4.
#[test]
fn inbox_endpoint_returns_count_and_both_messages() {
    let home = tempfile::tempdir().expect("tempdir");
    let handle = RelayServer::spawn_for_test(
        home.path(),
        0,
        Duration::from_millis(150),
        Duration::from_secs(10),
    );
    wait_until_up(handle.port);

    let client = CcRelay::new();
    client
        .send_message(handle.port, "first message", "sess-A")
        .expect("send 1");
    client
        .send_message(handle.port, "second message", "sess-B")
        .expect("send 2");

    let resp = raw_get(handle.port, "/inbox", Duration::from_secs(3)).expect("/inbox GET");
    assert_eq!(
        resp.status, 200,
        "/inbox must be 200, got {} body={}",
        resp.status, resp.body
    );

    let json: serde_json::Value = serde_json::from_str(&resp.body)
        .unwrap_or_else(|e| panic!("/inbox body must be JSON: {e}; body={}", resp.body));
    assert_eq!(
        json.get("count").and_then(|v| v.as_u64()),
        Some(2),
        "count must be 2"
    );
    let messages = json
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(messages.len(), 2, "messages array must hold 2 entries");

    // Both texts present (order is filesystem-dependent, so collect + check set).
    let texts: Vec<&str> = messages
        .iter()
        .filter_map(|m| m.get("text").and_then(|v| v.as_str()))
        .collect();
    assert!(
        texts.contains(&"first message"),
        "first message missing: {texts:?}"
    );
    assert!(
        texts.contains(&"second message"),
        "second message missing: {texts:?}"
    );

    handle.shutdown();
}

// ---------------------------------------------------------------------------
// Row 4 — /replies cached branch + idempotent re-peek (P-A3 cached, server.ts:303-306)
// ---------------------------------------------------------------------------

/// Buffer a reply DIRECTLY into state (the legitimate simulation of what M4's
/// `reply` tool will do: `buffer_reply`), then `fetch_reply` returns the cached
/// text IMMEDIATELY (P-A3 cached branch). Fetch AGAIN with the same id → the
/// SAME text: peek is idempotent (the P1 fix — a non-destructive read so a
/// --wait client whose long-poll response is lost can RE-GET it; a consuming
/// read would be a fresh instance of the 61-loss class).
#[test]
fn replies_cached_branch_returns_buffered_reply_idempotently() {
    let home = tempfile::tempdir().expect("tempdir");
    let handle = RelayServer::spawn_for_test(
        home.path(),
        0,
        Duration::from_millis(150),
        Duration::from_secs(10),
    );
    wait_until_up(handle.port);

    let msg_id = "relay-1700000000000-1";
    // Buffer a reply directly (M4's reply tool path simulation). Deadline well in
    // the future so the cached branch is hit (not a TTL eviction).
    {
        let mut state = handle.server.state.lock().unwrap();
        state.buffer_reply(
            msg_id.to_string(),
            "the buffered reply".to_string(),
            Instant::now() + Duration::from_secs(300),
        );
    }

    let client = CcRelay::new();
    // First fetch — must return the cached text IMMEDIATELY (no 150ms park).
    let started = Instant::now();
    let reply1 = client
        .fetch_reply(handle.port, msg_id, 5000)
        .expect("cached reply must be returned, not an error");
    let elapsed = started.elapsed();
    assert_eq!(
        reply1.text.as_deref(),
        Some("the buffered reply"),
        "cached text"
    );
    assert_eq!(reply1.error, None, "cached branch has no error");
    assert!(
        elapsed < Duration::from_millis(120),
        "cached branch must return immediately (no park), took {elapsed:?}"
    );

    // Second fetch with the SAME id — idempotent peek must return the SAME text
    // (entry survives the first read — the P1 fix). A consuming read would yield
    // None/timeout here.
    let reply2 = client
        .fetch_reply(handle.port, msg_id, 5000)
        .expect("idempotent re-peek must still return the cached reply");
    assert_eq!(
        reply2.text.as_deref(),
        Some("the buffered reply"),
        "re-peek must return the SAME text (idempotent — entry survives the read)"
    );

    handle.shutdown();
}

// ---------------------------------------------------------------------------
// Row 5 — /replies long-poll timeout → 408 (P-A3 timeout / P-F3b, server.ts:308-317)
// ---------------------------------------------------------------------------

/// `fetch_reply` for an id with NO buffered reply parks for the SERVER's injected
/// short budget (150ms — the SAME production park code path, P-F3b), then the
/// server returns HTTP 408 `{error:"timeout"}`. The frozen client maps a non-2xx
/// status to `RelayError::ServerError` carrying the status. We assert the ACTUAL
/// client behavior (confirmed by reading relay_http.rs `read_response`: status
/// not in 200..300 → `ServerError("HTTP 408: ...")`) AND that it returned within
/// a bounded time (proving the 150ms park + 408 fired, not the 120s production
/// deadline). P-A3 timeout / P-F3b.
#[test]
fn replies_long_poll_times_out_to_408_within_injected_budget() {
    let home = tempfile::tempdir().expect("tempdir");
    // Inject a 150ms park (production = 120s) through the SAME park code path.
    let handle = RelayServer::spawn_for_test(
        home.path(),
        0,
        Duration::from_millis(150),
        Duration::from_secs(10),
    );
    wait_until_up(handle.port);

    let client = CcRelay::new();
    let started = Instant::now();
    // Give the long-poll a generous 5s budget; the SERVER park (150ms) ends it.
    let result = client.fetch_reply(handle.port, "relay-no-such-id-1", 5000);
    let elapsed = started.elapsed();

    // The 408 surfaces as a ServerError carrying the status (the client's non-2xx
    // mapping in read_response). NOT a Timeout (that is the CLIENT's read budget
    // elapsing; here the SERVER actively returned 408 well within budget).
    match &result {
        Err(RelayError::ServerError(s)) => {
            assert!(
                s.contains("408"),
                "ServerError must carry the 408 status, got: {s}"
            );
            assert!(
                s.contains("timeout"),
                "408 body should carry the timeout error, got: {s}"
            );
        }
        other => panic!("a 408 must surface as ServerError(408), got {other:?}"),
    }

    // The park honored the INJECTED 150ms budget, not the production 120s, and not
    // the client's 5s read budget. A regression that ignores the injected budget
    // (or hangs) FAILS here.
    assert!(
        elapsed >= Duration::from_millis(120),
        "server must actually park ~150ms before 408, returned too fast: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "injected 150ms park must bound the 408, took {elapsed:?}"
    );

    handle.shutdown();
}

// ---------------------------------------------------------------------------
// Row 6 — 404 unknown path (P-A5, server.ts:333)
// ---------------------------------------------------------------------------

/// A raw GET to an unknown path returns HTTP 404. P-A5.
#[test]
fn unknown_path_returns_404() {
    let home = tempfile::tempdir().expect("tempdir");
    let handle = RelayServer::spawn_for_test(
        home.path(),
        0,
        Duration::from_millis(150),
        Duration::from_secs(10),
    );
    wait_until_up(handle.port);

    let resp = raw_get(handle.port, "/nonsense", Duration::from_secs(3)).expect("404 GET");
    assert_eq!(
        resp.status, 404,
        "unknown path must 404, got {} body={}",
        resp.status, resp.body
    );

    // A POST to an unknown method/path is also 404 (dispatch only matches the
    // four known routes). Exercise a wrong method on a known path too.
    let put = "PUT /message HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    let resp2 = raw_request(handle.port, put.as_bytes(), Duration::from_secs(3)).expect("PUT");
    assert_eq!(
        resp2.status, 404,
        "PUT /message (wrong method) must 404, got {}",
        resp2.status
    );

    handle.shutdown();
}

// ---------------------------------------------------------------------------
// Row 7 — hostile-request belts (W2 / §5 red-team). Each MUST be wall-clock
// bounded so a regression that hangs a server thread FAILS loudly.
// ---------------------------------------------------------------------------

/// (a) A POST declaring a Content-Length far over the 1 MiB ceiling → the server
/// rejects with 400 up front (never reads the announced body), connection not
/// hung. We declare the oversized length and send NO body — the server must
/// reject on the header alone (mirror of the client's `read_with_length` cap).
#[test]
fn hostile_oversized_content_length_is_400_bounded() {
    let home = tempfile::tempdir().expect("tempdir");
    let handle = RelayServer::spawn_for_test(
        home.path(),
        0,
        Duration::from_millis(150),
        Duration::from_secs(10),
    );
    wait_until_up(handle.port);

    let oversized = 8 * 1024 * 1024; // 8 MiB declared, over the 1 MiB ceiling
    let req = format!(
        "POST /message HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {oversized}\r\n\r\n"
    );
    let started = Instant::now();
    let resp = raw_request(handle.port, req.as_bytes(), Duration::from_secs(3))
        .expect("oversized request must get a response, not hang");
    let elapsed = started.elapsed();

    assert_eq!(
        resp.status, 400,
        "oversized Content-Length must be 400, got {}",
        resp.status
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "oversized request must be rejected promptly, took {elapsed:?} (regression = hang)"
    );

    // The server is still alive afterwards (the bad request didn't kill the
    // listener) — a fresh /health must still answer.
    assert!(
        CcRelay::new().health(handle.port, 1000).is_ok(),
        "server must survive a hostile request"
    );

    handle.shutdown();
}

/// (b) A slow-drip / never-terminated request: we send a partial request line and
/// then NOTHING (never the CRLFCRLF terminator), holding the socket open. The
/// server's `request_read_timeout` (the production `REQUEST_READ_TIMEOUT`, now
/// INJECTABLE — orc carry 4) must drop it. We inject a SHORT 300ms budget through
/// the SAME request-reader code path (not a forked branch), so this row proves the
/// bound FIRES while running in well under a second instead of waiting out the real
/// 10s. We assert the connection is closed (read returns) within that short budget
/// (+ generous slack), never a hang — either a 400 or a clean EOF.
#[test]
fn hostile_slow_drip_request_is_bounded_no_hang() {
    // SHORT injected request-read budget (orc carry 4): proves the bound fires fast.
    let drip_budget = Duration::from_millis(300);
    let home = tempfile::tempdir().expect("tempdir");
    let handle =
        RelayServer::spawn_for_test(home.path(), 0, Duration::from_millis(150), drip_budget);
    wait_until_up(handle.port);

    // Send an incomplete request (no terminating CRLFCRLF) then leave it dangling.
    // We can't reuse raw_request (it writes a complete request); do it inline with
    // a bounded read so the TEST itself can never hang.
    let started = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", handle.port)).expect("connect");
    // Bound the whole exchange on OUR side WELL ABOVE the injected 300ms budget, so
    // it is the SERVER (not our read timeout) that closes the connection. 5s slack
    // is huge vs. the 300ms server budget yet still bounds a regression that hangs.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"POST /message HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .unwrap();
    stream.flush().unwrap();
    // Now never finish the headers. The server must eventually close the connection
    // (EOF) or send a 400, within its (injected) request-read deadline. A read of 0
    // = clean close. We bound the assertion just above the 300ms budget + slack.
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).unwrap_or(0); // timeout/reset → 0
    let elapsed = started.elapsed();

    // The server must drop the slow-drip within its SHORT injected budget — proving
    // the bound fires, in a fraction of the old ~10s. 2s ceiling = 300ms budget +
    // ample slack for scheduling, while still failing a regression that hangs.
    assert!(
        elapsed < Duration::from_secs(2),
        "slow-drip must be dropped within the injected 300ms request-read budget (+slack), took {elapsed:?}"
    );
    if n > 0 {
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(
            text.contains("400"),
            "if the server responds to a slow-drip it must be a 400, got: {text}"
        );
    }
    drop(stream);

    // The server is still alive: the dangling connection didn't wedge the listener.
    assert!(
        CcRelay::new().health(handle.port, 1000).is_ok(),
        "server must survive a slow-drip"
    );

    handle.shutdown();
}

/// (c) Garbage / non-HTTP bytes → the server returns 400 or cleanly closes, never
/// a hang. We blast random non-HTTP bytes and assert a bounded response + a live
/// server afterwards.
#[test]
fn hostile_garbage_bytes_are_bounded_no_hang() {
    let home = tempfile::tempdir().expect("tempdir");
    let handle = RelayServer::spawn_for_test(
        home.path(),
        0,
        Duration::from_millis(150),
        Duration::from_secs(10),
    );
    wait_until_up(handle.port);

    // Garbage that contains a CRLFCRLF so the parser tries to parse it as a
    // request line and rejects (no valid METHOD/PATH or unmatched route). Without
    // a terminator it would hit the read deadline; with one it rejects promptly.
    let garbage = b"\x00\x01\x02 not http at all \xff\xfe\r\n\r\n";
    let started = Instant::now();
    let resp = raw_request(handle.port, garbage, Duration::from_secs(3))
        .expect("garbage must get a bounded response");
    let elapsed = started.elapsed();

    // Either a 400 (bad request line) or 404 (parsed a token but no route matched)
    // — never a hang, never a 200.
    assert!(
        resp.status == 400 || resp.status == 404,
        "garbage must be 400/404, got {} body={}",
        resp.status,
        resp.body
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "garbage request must be rejected promptly, took {elapsed:?}"
    );

    assert!(
        CcRelay::new().health(handle.port, 1000).is_ok(),
        "server must survive garbage"
    );

    handle.shutdown();
}

// ---------------------------------------------------------------------------
// Row 8 — concurrent senders distinct ids + inbox files (P-G5 partial,
// server.ts:258)
// ---------------------------------------------------------------------------

/// N=20 threads each `send_message` concurrently. The monotonic `seq` under the
/// single state lock must mint N DISTINCT message_ids (no two collide), and all N
/// inbox files must exist on disk. This covers the M2 concurrency that exists;
/// the full lost-wakeup/reply red-team is M4. P-G5 (partial).
#[test]
fn concurrent_senders_mint_distinct_ids_and_all_persist() {
    let home = tempfile::tempdir().expect("tempdir");
    let handle = RelayServer::spawn_for_test(
        home.path(),
        0,
        Duration::from_millis(150),
        Duration::from_secs(10),
    );
    wait_until_up(handle.port);

    const N: usize = 20;
    let port = handle.port;
    let mut threads = Vec::with_capacity(N);
    for i in 0..N {
        threads.push(thread::spawn(move || {
            let client = CcRelay::new();
            client
                .send_message(port, &format!("concurrent-{i}"), &format!("sess-{i}"))
                .expect("concurrent send must succeed")
        }));
    }
    let ids: Vec<String> = threads
        .into_iter()
        .map(|t| t.join().expect("thread join"))
        .collect();

    // All N ids distinct (the seq under the lock never collided).
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        N,
        "all {N} message_ids must be distinct, got {ids:?}"
    );

    // All N ids are well-formed and the seq segments are N CONSECUTIVE integers
    // (monotonic mint, no collision). NOTE (M5a item iv): the seq counter is now
    // SEEDED with a pid/random base at server construction, so the run starts at
    // `seed+1`, NOT necessarily 1 — we assert the run is N contiguous integers
    // rather than the literal 1..=N (the seed kills the two-fresh-servers same-ms
    // shared-inbox collision; the within-process monotonic property is unchanged).
    let mut seqs: Vec<u64> = ids
        .iter()
        .map(|id| {
            id.rsplit('-')
                .next()
                .unwrap()
                .parse::<u64>()
                .expect("seq is digits")
        })
        .collect();
    seqs.sort_unstable();
    let base = seqs[0];
    assert_eq!(
        seqs,
        (base..base + N as u64).collect::<Vec<_>>(),
        "seqs must be N consecutive integers starting at the seeded base {base}, got {seqs:?}"
    );

    // All N inbox files exist on disk (no interleaving corrupted the persist path).
    for id in &ids {
        let path = inbox_file(home.path(), id);
        assert!(path.exists(), "inbox file for {id} must exist: {path:?}");
    }
    // And /inbox reports exactly N.
    let resp = raw_get(port, "/inbox", Duration::from_secs(3)).expect("/inbox");
    let json: serde_json::Value = serde_json::from_str(&resp.body).expect("inbox json");
    assert_eq!(
        json.get("count").and_then(|v| v.as_u64()),
        Some(N as u64),
        "/inbox count must be N"
    );

    handle.shutdown();
}
