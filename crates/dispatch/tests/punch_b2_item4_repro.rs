//! sbx 0.1 punch-list B2, item 4 — PHASE-1 REPRO + MECHANISM PINS for the relay
//! reply-tool body-loss on the no-waiting-sender push-back path (the fleet's
//! standing pain item). Spec: ~/work/ws/switchboard/sbx/punch/b2-relay-spec.md.
//!
//! REPRO-FIRST DISCIPLINE: in phase 1 the `defect_pin_*` rows asserted the
//! then-current defective behavior; phase 2 FLIPPED them to pin the fixes
//! (stale-waiter → liveness ticks + resolve-and-verify bounded ack; wire
//! truncation / text-less bodies → 400, never a minted id). The `falsify_*`
//! rows are the staged candidate windows that HELD (no loss) — committed as
//! falsification evidence per the spec's hunt list, and doubling as
//! byte-intact pins the phase-2 invariant keeps.
//!
//! ## Harness
//! In-process `RelayServer::spawn_for_test` servers (the M4 red-team pattern):
//! `deliver_reply` is the ONE delivery code path (mod.rs:227 doc — the MCP
//! tools/call loop calls exactly this method), so driving it in-process tests
//! the same decision + push-back machinery the fleet exercises, without
//! subprocess timing flake. The full MCP-stdin delivery stack is already
//! covered by relay_server_mcp_delivery.rs.
//!
//! Receipt oracle: a push-back is a `POST /message` on the ORIGIN's relay,
//! which durably writes `<origin-home>/.claude/channels/relay/inbox/<id>.json`
//! BEFORE notifying (P-C1 persist-before-notify). Each server here gets its own
//! HOME, so "origin received the reply byte-intact" == an inbox file under the
//! origin's home whose `text` is byte-equal to `[REPLY to <M>] <body>`, and
//! "origin received NOTHING" == no such file. `deliver_reply` is synchronous —
//! when it returns, any push-back it was going to do has already happened, so
//! the negative check has no timing window.
//!
//! Hermetic: every server binds an OS-assigned ephemeral port (port_base 0 —
//! never the live 8900-8999 band); every HOME is a tempdir.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

use dispatch::paths::SbPaths;
use dispatch::relay::{RelayContract, RelayError};
use dispatch::relay_http::CcRelay;
use dispatch::relay_server::{RelayServer, TestServerHandle};

/// Spawn one in-process relay server under its own tempdir HOME.
/// (park, request-read) timeouts per row; port_base 0 = ephemeral (never the
/// live band).
fn spawn(park: Duration) -> (tempfile::TempDir, TestServerHandle) {
    let home = tempfile::tempdir().expect("tempdir HOME");
    let handle = RelayServer::spawn_for_test(home.path(), 0, park, Duration::from_secs(5));
    (home, handle)
}

/// Plant a sidecar for `session_id`@`port` into `relay_dir` (the file name is
/// arbitrary — `read_sidecars` reads every *.json; content pid 1 = launchd,
/// always alive, so the stale-sidecar sweeper never reaps it mid-test).
fn plant_sidecar(relay_dir: &Path, fname: &str, port: u16, session_id: &str, started_at: &str) {
    std::fs::create_dir_all(relay_dir).unwrap();
    let rec = serde_json::json!({
        "port": port,
        "pid": 1,
        "sessionId": session_id,
        "startedAt": started_at,
    });
    std::fs::write(relay_dir.join(fname), rec.to_string()).unwrap();
}

/// Scan `home`'s inbox dir for a persisted message whose text starts with
/// `[REPLY to <message_id>]`. Returns the FULL text (byte-exact) if present.
fn pushed_reply_text_at(home: &Path, message_id: &str) -> Option<String> {
    let inbox = SbPaths::from_home(home).inbox_dir;
    let prefix = format!("[REPLY to {message_id}]");
    let entries = std::fs::read_dir(inbox).ok()?;
    for ent in entries.flatten() {
        let Ok(bytes) = std::fs::read(ent.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
            if text.starts_with(&prefix) {
                return Some(text.to_string());
            }
        }
    }
    None
}

/// An ISO timestamp helper for sidecar startedAt ordering.
const OLDER: &str = "2026-06-11T00:00:00.000Z";
const NEWER: &str = "2026-06-11T12:00:00.000Z";

/// A JSON-hostile reply body: quotes, backslashes, newlines, tabs, a raw
/// control byte, unicode, and enough bulk (~64 KiB) to span many 4 KiB reads
/// on the server's body loop.
fn hostile_body() -> String {
    let mut s = String::from(
        "line1 \"quoted\" back\\slash\nline2\twith tab and ctrl-\u{1}-byte café ☕ 日本語 — ",
    );
    while s.len() < 64 * 1024 {
        s.push_str("supervision content block; ");
    }
    s
}

// ---------------------------------------------------------------------------
// ★ DEFECT PIN — the diagnosed mechanism for item 4.
//
// A `/replies/<M>` long-poll waiter is registered the moment the sender's GET
// is parsed (http.rs:405 register_waiter) and is removed ONLY when the park
// loop wakes (resolve or park-timeout, http.rs:428). The server CANNOT observe
// the sender hanging up (the parked thread never touches the socket again), so
// after the sender times out / dies, a STALE waiter marker persisted for up to
// the full park budget (production 120s — independent of the client's
// --timeout), shadowing the push-back path: the reply resolved into the dead
// socket, the replier was told DELIVERED, the origin never saw the body.
//
// THE FIX (phase 2, ratified shape — waiter liveness + bounded ack):
// (i)  the park loop probes client liveness each WAITER_LIVENESS_TICK
//      (http.rs client_alive — read-side EOF detection) and deregisters a
//      hung-up sender's waiter immediately;
// (ii) deliver_reply no longer trusts a registered waiter: on ResolveWaiter it
//      parks bounded (WAITER_ACK_TIMEOUT) for the parked thread's WRITE
//      OUTCOME; nack/timeout → deregister + re-decide without the waiter →
//      push-back. DELIVERED is an observation, never an inference.
//
// This row was the phase-1 defect pin (reply marked DELIVERED via the dead
// waiter, origin got nothing); it now pins the FIXED behavior: the same
// staging ends in a byte-intact push-back to the live origin.
// ---------------------------------------------------------------------------
#[test]
fn stale_waiter_falls_through_to_pushback_byte_intact() {
    // A's park budget far exceeds the client's wait, modelling prod (120s park
    // vs a sender that gave up / was killed).
    let (_home_a, a) = spawn(Duration::from_secs(30));
    let (home_b, b) = spawn(Duration::from_secs(2));
    let b_sid = b.server.session_id.clone();

    // B's relay is LIVE and discoverable from A — push-back must succeed.
    plant_sidecar(
        &a.server.paths.relay_dir,
        "origin-b.json",
        b.port,
        &b_sid,
        OLDER,
    );

    // B sends A a message → A records origin = B (in-mem + inbox file).
    let m = CcRelay::new()
        .send_message(a.port, "please report status", &b_sid)
        .expect("POST /message to A");

    // B's --wait long-poll parks on A... then the SENDER GIVES UP (client read
    // timeout — the same socket-death as a killed session / expired --timeout).
    // The REAL client code path: fetch_reply's read deadline fires, the
    // connection drops.
    let gone = CcRelay::new().fetch_reply(a.port, &m, 400);
    assert!(
        matches!(gone, Err(RelayError::Timeout)),
        "the sender's wait must have timed out (client gone), got: {gone:?}"
    );

    // Now the recipient replies (the MCP reply tool runs exactly this method).
    let body = "the supervision content the fleet keeps losing";
    let outcome = a.server.deliver_reply(&m, body);

    // ---- FIXED SHAPE (was the phase-1 defect pin) ----
    // (1) The reply is DELIVERED via PUSH-BACK, not via the dead waiter.
    assert!(
        !outcome.is_error,
        "the reply must be delivered (push-back), got: {}",
        outcome.text
    );
    assert!(
        outcome.text.contains(&format!("posted to session {b_sid}")),
        "delivery must be the push-back to the live origin, got: {}",
        outcome.text
    );
    assert!(
        !outcome.text.contains("long-poll resolved"),
        "delivery must NOT be claimed via the dead waiter, got: {}",
        outcome.text
    );
    // (2) The origin received the body BYTE-INTACT (race-free: deliver_reply
    // is synchronous, the push-back completed before it returned).
    assert_eq!(
        pushed_reply_text_at(home_b.path(), &m).as_deref(),
        Some(format!("[REPLY to {m}] {body}").as_str()),
        "the origin session receives the full body despite the stale waiter"
    );
    // (3) The resolved buffer still serves --wait retries (P-E1 unchanged).
    let buffered = CcRelay::new()
        .fetch_reply(a.port, &m, 1000)
        .expect("the resolved buffer still serves the text");
    assert_eq!(buffered.text.as_deref(), Some(body));
}

// ---------------------------------------------------------------------------
// Fix (i) pinned directly: a hung-up sender's waiter is DEREGISTERED within a
// liveness tick or two — even with NO reply in flight — so it can never
// shadow anything later.
// ---------------------------------------------------------------------------
#[test]
fn hung_up_waiter_is_deregistered_within_liveness_ticks() {
    let (_home_a, a) = spawn(Duration::from_secs(30));

    let m = CcRelay::new()
        .send_message(a.port, "q", "sess-someone")
        .expect("POST /message");

    // Park then hang up (client read timeout drops the connection).
    let gone = CcRelay::new().fetch_reply(a.port, &m, 300);
    assert!(matches!(gone, Err(RelayError::Timeout)));

    // Within a couple of 250ms ticks the probe must detect the EOF and
    // deregister. Poll up to 2s (generous vs the tick) for the marker to go.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let waiting = {
            let state = a.server.state.lock().unwrap();
            state.has_waiter(&m)
        };
        if !waiting {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the hung-up sender's waiter must be deregistered within liveness \
             ticks (still registered after 2s)"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// Fix (ii) regression guard: a LIVE waiter still resolves end-to-end — the
// verify handshake must not break the common happy path (the parked sender
// receives the text AND the replier is told long-poll resolved), and must not
// double-deliver into the origin's channel.
// ---------------------------------------------------------------------------
#[test]
fn live_waiter_still_resolves_and_never_double_delivers() {
    let (_home_a, a) = spawn(Duration::from_secs(10));
    let (home_b, b) = spawn(Duration::from_secs(2));
    let b_sid = b.server.session_id.clone();
    plant_sidecar(
        &a.server.paths.relay_dir,
        "origin-b.json",
        b.port,
        &b_sid,
        OLDER,
    );

    let m = CcRelay::new()
        .send_message(a.port, "live question", &b_sid)
        .expect("POST /message");

    // A REAL live waiter parks (generous budget).
    let port = a.port;
    let mid = m.clone();
    let poll = std::thread::spawn(move || CcRelay::new().fetch_reply(port, &mid, 8000));
    std::thread::sleep(Duration::from_millis(300)); // waiter registered

    let outcome = a.server.deliver_reply(&m, "the live answer");
    assert!(!outcome.is_error, "got: {}", outcome.text);
    assert!(
        outcome.text.contains("long-poll resolved"),
        "a live waiter is a verified long-poll delivery, got: {}",
        outcome.text
    );

    let resolved = poll
        .join()
        .expect("poll thread")
        .expect("fetch_reply resolves");
    assert_eq!(resolved.text.as_deref(), Some("the live answer"));

    // No push-back happened — the origin's channel saw nothing (no double
    // delivery on the common path; the ratified ruling rejected blind
    // always-push-back for exactly this reason).
    assert_eq!(
        pushed_reply_text_at(home_b.path(), &m),
        None,
        "a verified waiter delivery must not ALSO push back"
    );
}

// ---------------------------------------------------------------------------
// Wire-layer byte-intact contract (B2 item 4b, ratified Q2).
//
// PHASE-1 DEFECT (pinned then, fixed now): an early EOF mid-body was "return
// what we have" and handle_message's permissive parse turned the mangled body
// into a recorded, persisted, channel-emitted EMPTY message under a 200 — the
// sender believed a body it never landed was delivered.
//
// FIXED SHAPE: EOF with bytes still owed = framing violation → 400; a body
// that is not JSON or lacks a string `text` → 400. NEVER a minted id, no
// inbox record, no channel emission. (Sanctioned divergence from server.ts's
// permissive `body.text` read.)
// ---------------------------------------------------------------------------
#[test]
fn truncated_post_body_is_rejected_400_no_minted_id() {
    let (home_a, a) = spawn(Duration::from_secs(2));

    // A valid JSON body... of which we deliver only a prefix, then EOF.
    let full = serde_json::json!({
        "text": "[REPLY to relay-123-1] the long supervision reply body",
        "from_session": "sess-origin",
    })
    .to_string();
    let sent = &full[..40]; // cut mid-JSON

    let mut stream = TcpStream::connect(("127.0.0.1", a.port)).expect("connect");
    let req = format!(
        "POST /message HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        full.len(),
        sent
    );
    stream.write_all(req.as_bytes()).unwrap();
    // Peer dies mid-body: half-close the write side → the server's body read
    // sees EOF with bytes still owed.
    stream.shutdown(std::net::Shutdown::Write).unwrap();

    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read response");

    // ---- FIXED SHAPE (was the phase-1 defect pin) ----
    assert!(
        resp.starts_with("HTTP/1.1 400"),
        "a truncated body must be rejected 400, got: {resp}"
    );
    assert!(
        !resp.contains("message_id"),
        "no message_id may be minted for a truncated body, got: {resp}"
    );
    // Nothing was persisted: the inbox dir holds no record of this request.
    let inbox = SbPaths::from_home(home_a.path()).inbox_dir;
    let count = std::fs::read_dir(&inbox)
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "a rejected body must leave no inbox record (silent loss is dead)"
    );

    // And the server survives + still serves intact posts.
    let mid = CcRelay::new()
        .send_message(a.port, "intact after the bad request", "sess-x")
        .expect("an intact POST still succeeds");
    assert!(mid.starts_with("relay-"));
}

/// B2 item 4b companion: a parseable JSON body that carries NO string `text`
/// (absent or non-string) is 400 — never an empty-text message under a minted
/// id. An EXPLICIT empty-string text stays legal (present and typed).
#[test]
fn missing_or_nonstring_text_is_rejected_400_empty_string_stays_legal() {
    let (_home_a, a) = spawn(Duration::from_secs(2));

    let post = |body: &str| -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", a.port)).expect("connect");
        let req = format!(
            "POST /message HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("read response");
        resp
    };

    for bad in [
        "{}",                                // no text at all
        r#"{"from_session":"sess-x"}"#,      // metadata only
        r#"{"text":42,"from_session":"s"}"#, // non-string text
        r#"{"text":null}"#,                  // null text
        "not json at all",                   // unparseable
    ] {
        let resp = post(bad);
        assert!(
            resp.starts_with("HTTP/1.1 400"),
            "body {bad:?} must be 400, got: {resp}"
        );
        assert!(
            !resp.contains("message_id"),
            "body {bad:?} must not mint an id, got: {resp}"
        );
    }

    // Explicit empty string text: present and typed → still accepted.
    let resp = post(r#"{"text":"","from_session":"sess-x"}"#);
    assert!(
        resp.starts_with("HTTP/1.1 200") && resp.contains("message_id"),
        "an explicit empty-string text stays legal, got: {resp}"
    );
}

// ---------------------------------------------------------------------------
// Falsification + byte-intact control rows — the staged candidate windows that
// HELD. These double as the phase-2 byte-exact body pins on the push-back path.
// ---------------------------------------------------------------------------

/// Hunt row: in-memory origin hit + JSON-hostile / multi-read body.
/// STAGED: reply with quotes, backslashes, newlines, control byte, unicode,
/// ~64 KiB bulk, via the in-mem origin path. HELD: byte-intact at the origin.
#[test]
fn falsify_pushback_in_mem_origin_delivers_hostile_body_byte_intact() {
    let (_home_a, a) = spawn(Duration::from_millis(200));
    let (home_b, b) = spawn(Duration::from_millis(200));
    let b_sid = b.server.session_id.clone();
    plant_sidecar(
        &a.server.paths.relay_dir,
        "origin-b.json",
        b.port,
        &b_sid,
        OLDER,
    );

    let m = CcRelay::new()
        .send_message(a.port, "what is the answer?", &b_sid)
        .expect("POST to A");

    let body = hostile_body();
    let outcome = a.server.deliver_reply(&m, &body);
    assert!(!outcome.is_error, "delivered, got: {}", outcome.text);
    assert!(
        outcome.text.contains(&format!("posted to session {b_sid}")),
        "push-back path taken, got: {}",
        outcome.text
    );
    assert_eq!(
        pushed_reply_text_at(home_b.path(), &m).as_deref(),
        Some(format!("[REPLY to {m}] {body}").as_str()),
        "the origin's persisted copy must be BYTE-EQUAL to the submitted reply"
    );
}

/// Hunt row: the inbox-file fallback (origin relay restarted → in-mem origin
/// map lost). STAGED: a FRESH server on the same HOME (no in-mem origin for M)
/// delivers a reply for M via origin_from_inbox. HELD: byte-intact push-back.
#[test]
fn falsify_inbox_fallback_after_relay_restart_delivers_byte_intact() {
    let (home_a, a) = spawn(Duration::from_millis(200));
    let (home_b, b) = spawn(Duration::from_millis(200));
    let b_sid = b.server.session_id.clone();
    plant_sidecar(
        &a.server.paths.relay_dir,
        "origin-b.json",
        b.port,
        &b_sid,
        OLDER,
    );

    // The message lands on A (in-mem origin + the durable inbox file).
    let m = CcRelay::new()
        .send_message(a.port, "question before the restart", &b_sid)
        .expect("POST to A");

    // "Restart": a SECOND server on the SAME home — fresh state, no in-mem
    // origin for M; only the inbox file survives (the production restart
    // shape: new relay process, same HOME).
    let a2 = RelayServer::spawn_for_test(
        home_a.path(),
        0,
        Duration::from_millis(200),
        Duration::from_secs(5),
    );
    let body = "answer delivered through the inbox-fallback origin";
    let outcome = a2.server.deliver_reply(&m, body);
    assert!(
        !outcome.is_error,
        "inbox-fallback origin must still push back, got: {}",
        outcome.text
    );
    assert_eq!(
        pushed_reply_text_at(home_b.path(), &m).as_deref(),
        Some(format!("[REPLY to {m}] {body}").as_str()),
        "inbox-fallback delivery must be byte-intact"
    );
}

/// Hunt row: multiple sidecars for the origin, NEWEST-first pick where the
/// newest is DEAD (port closed). STAGED: a newer sidecar pointing at a freshly
/// closed port + the older live one. HELD: /health verification skips the dead
/// candidate; the live origin gets the body intact.
#[test]
fn falsify_dead_newest_sidecar_skipped_live_origin_delivered() {
    let (_home_a, a) = spawn(Duration::from_millis(200));
    let (home_b, b) = spawn(Duration::from_millis(200));
    let b_sid = b.server.session_id.clone();

    // A dead port: bind + drop → nothing listening.
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    plant_sidecar(
        &a.server.paths.relay_dir,
        "b-stale-newer.json",
        dead_port,
        &b_sid,
        NEWER,
    );
    plant_sidecar(
        &a.server.paths.relay_dir,
        "b-live-older.json",
        b.port,
        &b_sid,
        OLDER,
    );

    let m = CcRelay::new()
        .send_message(a.port, "q", &b_sid)
        .expect("POST to A");
    let outcome = a
        .server
        .deliver_reply(&m, "served despite the stale sidecar");
    assert!(!outcome.is_error, "got: {}", outcome.text);
    assert_eq!(
        pushed_reply_text_at(home_b.path(), &m).as_deref(),
        Some(format!("[REPLY to {m}] served despite the stale sidecar").as_str()),
    );
}

/// Hunt row: the origin's stale sidecar port REUSED by a DIFFERENT session's
/// live relay (the wrong-listener window). STAGED: newest sidecar claims
/// origin's session id but points at C's relay. HELD: the /health identity
/// check (h.session_id == origin, mod.rs:401-404) rejects C; the body reaches
/// the REAL origin and never leaks to C.
#[test]
fn falsify_port_reused_by_other_session_health_mismatch_skipped() {
    let (_home_a, a) = spawn(Duration::from_millis(200));
    let (home_b, b) = spawn(Duration::from_millis(200));
    let (home_c, c) = spawn(Duration::from_millis(200));
    let b_sid = b.server.session_id.clone();

    // Newest sidecar: claims to be B but the port is C's relay.
    plant_sidecar(
        &a.server.paths.relay_dir,
        "b-claims-c-port.json",
        c.port,
        &b_sid,
        NEWER,
    );
    plant_sidecar(
        &a.server.paths.relay_dir,
        "b-live-older.json",
        b.port,
        &b_sid,
        OLDER,
    );

    let m = CcRelay::new()
        .send_message(a.port, "q", &b_sid)
        .expect("POST to A");
    let outcome = a.server.deliver_reply(&m, "for B only");
    assert!(!outcome.is_error, "got: {}", outcome.text);
    assert_eq!(
        pushed_reply_text_at(home_b.path(), &m).as_deref(),
        Some(format!("[REPLY to {m}] for B only").as_str()),
        "the REAL origin receives the body"
    );
    assert_eq!(
        pushed_reply_text_at(home_c.path(), &m),
        None,
        "the wrong-session listener must NEVER receive the reply"
    );
}

/// Hunt row: origin relay fully dead at reply time. STAGED: only a dead-port
/// sidecar for the origin. HELD: honest NOT-DELIVERED (is_error true + the
/// fresh-message guidance) — never a silent success.
#[test]
fn falsify_origin_dead_is_honest_not_delivered() {
    let (_home_a, a) = spawn(Duration::from_millis(200));
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    plant_sidecar(
        &a.server.paths.relay_dir,
        "origin-dead.json",
        dead_port,
        "sess-dead-origin",
        OLDER,
    );

    let m = CcRelay::new()
        .send_message(a.port, "q", "sess-dead-origin")
        .expect("POST to A");
    let outcome = a.server.deliver_reply(&m, "reply into the void");
    assert!(
        outcome.is_error,
        "a dead origin must be an HONEST not-delivered, got: {}",
        outcome.text
    );
    assert!(
        outcome.text.starts_with("NOT DELIVERED"),
        "got: {}",
        outcome.text
    );
    assert!(
        outcome.text.contains("send a fresh message"),
        "must carry the fresh-message guidance, got: {}",
        outcome.text
    );
}

/// Hunt row: oversized reply (push-back POST body above the origin server's
/// 1 MiB request ceiling). STAGED: a >1 MiB reply body. HELD: the origin
/// rejects it (400) and the replier gets an HONEST NOT-DELIVERED — no silent
/// truncation. (Flagged for the lead: replies this large can never push back;
/// honest, but a cliff worth knowing about.)
#[test]
fn falsify_oversized_reply_is_honest_not_delivered_never_truncated() {
    let (_home_a, a) = spawn(Duration::from_millis(200));
    let (home_b, b) = spawn(Duration::from_millis(200));
    let b_sid = b.server.session_id.clone();
    plant_sidecar(
        &a.server.paths.relay_dir,
        "origin-b.json",
        b.port,
        &b_sid,
        OLDER,
    );

    let m = CcRelay::new()
        .send_message(a.port, "q", &b_sid)
        .expect("POST to A");
    let body = "x".repeat(1024 * 1024 + 64); // pushes the POST past 1 MiB
    let outcome = a.server.deliver_reply(&m, &body);
    assert!(
        outcome.is_error,
        "an undeliverable oversized reply must be an honest error, got is_error=false: {}",
        &outcome.text[..outcome.text.len().min(200)]
    );
    assert_eq!(
        pushed_reply_text_at(home_b.path(), &m),
        None,
        "no truncated fragment may ever be delivered as the reply"
    );
}

// ===========================================================================
// Red-team round 1, W1 — ack cross-wiring under CONCURRENT same-id replies.
//
// MECHANISM (red-team repro, 60/200 forced rounds inverted truth on 829340a):
// verify_pending is a per-id SET, acks a per-id MAP, buffer_reply overwrites
// unconditionally, and the ack carries no identity of WHICH text the socket
// write delivered — two barrier-synced deliver_reply calls for one message_id
// could consume each other's ack: the replier whose text WAS delivered was
// told NOT-DELIVERED (+ resend guidance → duplicate), the never-seen one was
// told DELIVERED (→ silent loss).
//
// RATIFIED FIX: SERIALIZE same-id verify windows — a second deliver_reply
// arriving while a window is OPEN parks on the Condvar until unmark_verify
// closes it (bounded by VERIFY_SERIALIZE_TIMEOUT; on expiry proceed anyway —
// never wedge), then proceeds FRESH. Sequentially: the first reply resolves
// the waiter truthfully; the second finds the waiter gone → push-back. BOTH
// verdicts are observations.
// ===========================================================================

/// ★ THE W1 GATE PIN — the adapted red-team repro, FLIPPED: N forced races,
/// ZERO cross-wired outcomes. Every replier that reports DELIVERED via
/// "long-poll resolved" must have had ITS text received by the waiter. (On
/// the pre-fix tree this inverted in 60/200 rounds.)
#[test]
fn w1_gate_concurrent_same_id_replies_zero_cross_wired_verdicts() {
    use std::sync::{Arc, Barrier};

    let (_home, handle) = spawn(Duration::from_secs(5));
    let port = handle.port;
    let server = &handle.server;

    let mut hits: Vec<String> = Vec::new();
    for round in 0..200 {
        let id = format!("relay-w1x-{round}");

        // Park a REAL waiter for the fabricated id.
        let idp = id.clone();
        let poll = std::thread::spawn(move || CcRelay::new().fetch_reply(port, &idp, 5000));
        for _ in 0..400 {
            {
                let s = server.state.lock().unwrap_or_else(|p| p.into_inner());
                if s.has_waiter(&id) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        // Two barrier-synced replies to the SAME id.
        let barrier = Arc::new(Barrier::new(2));
        let (sa, sb2) = (Arc::clone(server), Arc::clone(server));
        let (ba, bb) = (Arc::clone(&barrier), Arc::clone(&barrier));
        let (ia, ib) = (id.clone(), id.clone());
        let ta = std::thread::spawn(move || {
            ba.wait();
            sa.deliver_reply(&ia, "TEXT-A")
        });
        let tb = std::thread::spawn(move || {
            bb.wait();
            sb2.deliver_reply(&ib, "TEXT-B")
        });
        let oa = ta.join().unwrap();
        let ob = tb.join().unwrap();
        let got = poll
            .join()
            .unwrap()
            .ok()
            .and_then(|r| r.text)
            .unwrap_or_default();

        // The cross-wiring invariant: a "long-poll resolved" verdict must
        // match what the waiter ACTUALLY received.
        let a_del = !oa.is_error && oa.text.contains("long-poll resolved");
        let b_del = !ob.is_error && ob.text.contains("long-poll resolved");
        if (a_del && got != "TEXT-A") || (b_del && got != "TEXT-B") {
            hits.push(format!(
                "round {round}: waiter got {got:?}, A: del={a_del} ({}), B: del={b_del} ({})",
                oa.text, ob.text
            ));
        }
    }
    assert!(
        hits.is_empty(),
        "cross-wired DELIVERED verdicts ({}/200):\n{}",
        hits.len(),
        hits.join("\n")
    );
}

/// W1 bound pin, path (a) — NORMAL CLOSE: a second same-id deliver_reply
/// PARKS while the verify window is open and RELEASES when unmark_verify
/// fires (the window-close notify). The "second mark waits for first unmark"
/// serialization, pinned directly.
#[test]
fn w1_serialization_park_releases_on_window_close() {
    use std::sync::Arc;
    use std::time::Instant;

    let (_home, handle) = spawn(Duration::from_millis(200));
    let server = &handle.server;
    const HOLD: Duration = Duration::from_millis(150);

    // Open a verify window for the id by hand (the first replier's window).
    {
        let mut s = server.state.lock().unwrap_or_else(|p| p.into_inner());
        s.mark_verify("relay-w1-ser-a");
    }

    // Second replier for the SAME id: must park until the window closes.
    let s2 = Arc::clone(server);
    let started = Instant::now();
    let second =
        std::thread::spawn(move || s2.deliver_reply("relay-w1-ser-a", "second reply text"));

    // Hold the window open; the second replier must still be parked.
    std::thread::sleep(HOLD);
    assert!(
        !second.is_finished(),
        "the second same-id reply must park while the verify window is open"
    );

    // Close the window the way verify_waiter_delivery does: unmark + notify.
    {
        let mut s = server.state.lock().unwrap_or_else(|p| p.into_inner());
        s.unmark_verify("relay-w1-ser-a");
        server.cvar.notify_all();
    }

    let outcome = second.join().expect("second replier completes");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= HOLD,
        "the second replier waited for the window close (elapsed {elapsed:?})"
    );
    assert!(
        elapsed < VERIFY_SERIALIZE_SLACK,
        "released by the close notify, not by the bound (elapsed {elapsed:?})"
    );
    // It proceeded FRESH after the close: no waiter, no origin → honest
    // NOT-DELIVERED (an observation, not a cross-wired verdict).
    assert!(outcome.is_error, "got: {}", outcome.text);
    assert!(
        outcome.text.starts_with("NOT DELIVERED"),
        "{}",
        outcome.text
    );
}

/// W1 bound pin, path (b) — THE WEDGE GUARD: the window NEVER closes (its
/// owner died without unmark — not reachable today, but the bound is the
/// load-bearing never-wedge claim). The parked second replier must proceed
/// anyway once VERIFY_SERIALIZE_TIMEOUT (500ms) expires.
#[test]
fn w1_serialization_park_releases_at_bound_when_window_never_closes() {
    use std::time::Instant;

    let (_home, handle) = spawn(Duration::from_millis(200));
    let server = &handle.server;

    {
        let mut s = server.state.lock().unwrap_or_else(|p| p.into_inner());
        s.mark_verify("relay-w1-wedge");
        // NEVER unmarked.
    }

    let started = Instant::now();
    let outcome = server.deliver_reply("relay-w1-wedge", "must not wedge");
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(400),
        "the park honored (most of) the 500ms bound before proceeding, \
         elapsed {elapsed:?}"
    );
    assert!(
        elapsed < VERIFY_SERIALIZE_SLACK,
        "the bound RELEASED the park — never a wedge (elapsed {elapsed:?})"
    );
    // Proceeded anyway: no waiter, no origin → honest NOT-DELIVERED.
    assert!(outcome.is_error, "got: {}", outcome.text);
}

/// Generous ceiling for "the serialization park came back promptly" asserts:
/// far above the 500ms bound + scheduling, far below a wedge.
const VERIFY_SERIALIZE_SLACK: Duration = Duration::from_secs(5);

/// All `[REPLY to <message_id>]` texts persisted at `home` (the multi-copy
/// variant of [`pushed_reply_text_at`] — the both-push-back arm needs both).
fn pushed_reply_texts_at(home: &Path, message_id: &str) -> Vec<String> {
    let inbox = SbPaths::from_home(home).inbox_dir;
    let prefix = format!("[REPLY to {message_id}]");
    let mut texts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(inbox) {
        for ent in entries.flatten() {
            let Ok(bytes) = std::fs::read(ent.path()) else {
                continue;
            };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                if text.starts_with(&prefix) {
                    texts.push(text.to_string());
                }
            }
        }
    }
    texts
}

/// W1 serialized-outcome pin WITH a live origin: one waiter + two concurrent
/// same-id replies → exactly ONE resolves the waiter (verdict matches the
/// waiter's received text) and the OTHER pushes back its OWN text byte-intact
/// to the origin. Both verdicts truthful, nothing lost, nothing duplicated.
#[test]
fn w1_concurrent_replies_with_live_origin_one_resolves_one_pushes_back() {
    use std::sync::{Arc, Barrier};

    let (_home_a, a) = spawn(Duration::from_secs(5));
    let (home_b, b) = spawn(Duration::from_millis(200));
    let b_sid = b.server.session_id.clone();
    plant_sidecar(
        &a.server.paths.relay_dir,
        "origin-b.json",
        b.port,
        &b_sid,
        OLDER,
    );

    let m = CcRelay::new()
        .send_message(a.port, "race with live origin", &b_sid)
        .expect("POST to A");

    // A real parked waiter.
    let port = a.port;
    let mid = m.clone();
    let poll = std::thread::spawn(move || CcRelay::new().fetch_reply(port, &mid, 5000));
    for _ in 0..400 {
        {
            let s = a.server.state.lock().unwrap_or_else(|p| p.into_inner());
            if s.has_waiter(&m) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let barrier = Arc::new(Barrier::new(2));
    let (sa, sb2) = (Arc::clone(&a.server), Arc::clone(&a.server));
    let (ba, bb) = (Arc::clone(&barrier), Arc::clone(&barrier));
    let (ia, ib) = (m.clone(), m.clone());
    let ta = std::thread::spawn(move || {
        ba.wait();
        sa.deliver_reply(&ia, "TEXT-A")
    });
    let tb = std::thread::spawn(move || {
        bb.wait();
        sb2.deliver_reply(&ib, "TEXT-B")
    });
    let oa = ta.join().unwrap();
    let ob = tb.join().unwrap();
    let got = poll
        .join()
        .unwrap()
        .expect("waiter resolves")
        .text
        .unwrap_or_default();

    // Exactly one verdict is the waiter resolution, and it matches what the
    // waiter received; the other is a push-back of ITS OWN text.
    let a_waiter = oa.text.contains("long-poll resolved");
    let b_waiter = ob.text.contains("long-poll resolved");
    assert!(
        a_waiter ^ b_waiter,
        "exactly one reply resolves the waiter; got A: {} / B: {}",
        oa.text,
        ob.text
    );
    let (winner_text, loser, loser_text) = if a_waiter {
        ("TEXT-A", &ob, "TEXT-B")
    } else {
        ("TEXT-B", &oa, "TEXT-A")
    };
    assert_eq!(got, winner_text, "the waiter received the WINNER's text");
    assert!(
        !loser.is_error && loser.text.contains(&format!("posted to session {b_sid}")),
        "the loser pushes back to the live origin, got: {}",
        loser.text
    );
    assert_eq!(
        pushed_reply_texts_at(home_b.path(), &m),
        vec![format!("[REPLY to {m}] {loser_text}")],
        "the origin received exactly the LOSER's text, byte-intact"
    );
}

/// Orc mechanism cross-check, both-push-back arm (verified-DISTINCT evidence):
/// push_back formats from its CALL-LOCAL `text` parameter (deliver_reply →
/// push_back parameter chain; it never reads the shared per-id buffer slot),
/// so two concurrent same-id replies with NO waiter each deliver their OWN
/// byte-intact posted copy — the W1 slot collision structurally cannot
/// cross-wire posted copies.
#[test]
fn w1_crosscheck_concurrent_replies_no_waiter_both_pushback_byte_intact() {
    use std::sync::{Arc, Barrier};

    let (_home_a, a) = spawn(Duration::from_millis(200));
    let (home_b, b) = spawn(Duration::from_millis(200));
    let b_sid = b.server.session_id.clone();
    plant_sidecar(
        &a.server.paths.relay_dir,
        "origin-b.json",
        b.port,
        &b_sid,
        OLDER,
    );

    let m = CcRelay::new()
        .send_message(a.port, "no waiter race", &b_sid)
        .expect("POST to A");
    // NO waiter parked.

    let barrier = Arc::new(Barrier::new(2));
    let (sa, sb2) = (Arc::clone(&a.server), Arc::clone(&a.server));
    let (ba, bb) = (Arc::clone(&barrier), Arc::clone(&barrier));
    let (ia, ib) = (m.clone(), m.clone());
    let ta = std::thread::spawn(move || {
        ba.wait();
        sa.deliver_reply(&ia, "COPY-A")
    });
    let tb = std::thread::spawn(move || {
        bb.wait();
        sb2.deliver_reply(&ib, "COPY-B")
    });
    let oa = ta.join().unwrap();
    let ob = tb.join().unwrap();

    for (o, label) in [(&oa, "A"), (&ob, "B")] {
        assert!(
            !o.is_error && o.text.contains(&format!("posted to session {b_sid}")),
            "replier {label} push-back must succeed, got: {}",
            o.text
        );
    }
    let mut texts = pushed_reply_texts_at(home_b.path(), &m);
    texts.sort();
    assert_eq!(
        texts,
        vec![
            format!("[REPLY to {m}] COPY-A"),
            format!("[REPLY to {m}] COPY-B"),
        ],
        "BOTH posted copies arrive byte-intact — per-call text, no slot sharing"
    );
}

/// RED-TEAM ROUND 2, X1 scratch: N>=3 barrier-synced same-id repliers against
/// one real waiter. The released parkers must re-serialize through the
/// while-loop re-check — zero cross-wired verdicts, no wedge, at N=3 and N=4.
#[test]
fn r2_x1_three_plus_concurrent_same_id_replies_no_cross_wire() {
    use std::sync::{Arc, Barrier};

    let (_home, handle) = spawn(Duration::from_secs(5));
    let port = handle.port;
    let server = &handle.server;

    for n in [3usize, 4] {
        let mut hits: Vec<String> = Vec::new();
        for round in 0..100 {
            let id = format!("relay-r2x1-{n}-{round}");
            let idp = id.clone();
            let poll = std::thread::spawn(move || CcRelay::new().fetch_reply(port, &idp, 5000));
            for _ in 0..400 {
                {
                    let s = server.state.lock().unwrap_or_else(|p| p.into_inner());
                    if s.has_waiter(&id) {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }

            let barrier = Arc::new(Barrier::new(n));
            let mut joins = Vec::new();
            for k in 0..n {
                let s = Arc::clone(server);
                let b = Arc::clone(&barrier);
                let idk = id.clone();
                joins.push(std::thread::spawn(move || {
                    b.wait();
                    (k, s.deliver_reply(&idk, &format!("TEXT-{k}")))
                }));
            }
            let outcomes: Vec<(usize, dispatch::relay_server::DeliveryOutcome)> =
                joins.into_iter().map(|j| j.join().unwrap()).collect();
            let got = poll
                .join()
                .unwrap()
                .ok()
                .and_then(|r| r.text)
                .unwrap_or_default();

            let mut resolved_count = 0;
            for (k, o) in &outcomes {
                let del = !o.is_error && o.text.contains("long-poll resolved");
                if del {
                    resolved_count += 1;
                    if got != format!("TEXT-{k}") {
                        hits.push(format!(
                            "n={n} round {round}: waiter got {got:?} but replier {k} told DELIVERED-via-waiter"
                        ));
                    }
                }
            }
            if resolved_count > 1 {
                hits.push(format!(
                    "n={n} round {round}: {resolved_count} repliers all told long-poll resolved"
                ));
            }
        }
        assert!(
            hits.is_empty(),
            "X1 cross-wires at n={n}:\n{}",
            hits.join("\n")
        );
    }
}
