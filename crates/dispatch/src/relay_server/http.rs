//! Localhost HTTP/1.1 listener — the SERVER mirror of [`crate::relay_http`]'s
//! client reader (spec §2 `relay_server/http.rs`).
//!
//! This is the server half of the cc-relay HTTP transport: it accepts the EXACT
//! requests `CcRelay` (the frozen client) issues and produces the responses that
//! client parses. `relay_http.rs` is the reference consumer; everything here is
//! its mirror image (it WRITES a request + READS a response — we READ a request
//! + WRITE a response).
//!
//! ## Concurrency (spec §2, P-G3)
//! `std::net::TcpListener` on `127.0.0.1:<port>`; accept loop → `thread::spawn`
//! per connection (thread-per-conn; peer count is dozens of fleet sessions, not
//! thousands — the blocking model fits). The shared [`RelayServer`] is held as
//! an `Arc` and cloned into each connection thread.
//!
//! ## Hostile-counterpart belts (spec W2 + §5 red-team target)
//! The request reader bounds BOTH memory and wall-clock the SAME way the client's
//! reader does (`relay_http.rs` `MAX_RELAY_BODY` + a per-read re-armed wall-clock
//! deadline), so a slow-drip / oversized / malformed counterpart can never hang a
//! connection thread or balloon memory. A bad request → a clean 400, never a hang.
//! Getting this right in M2 is load-bearing — it is a §5 red-team scenario.
//!
//! ## Endpoint parity (server.ts line anchors in each handler)
//! - `GET  /health`        → P-A1, server.ts:251-253
//! - `POST /message`       → P-A2/P-C1/P-B4, server.ts:255-298
//! - `GET  /replies/<id>`  → P-A3/P-F3/P-F3b, server.ts:300-318
//! - `GET  /inbox`         → P-A4, server.ts:320-331
//! - anything else         → P-A5 404, server.ts:333
//!
//! ## Lock / IO discipline (P-G6 / P-G3 — the reviewer-checked invariant)
//! ONE lock (`RelayServer.state`). It is NEVER held across a blocking syscall:
//! - `/message`: state ops (mint + record_origin) under the lock, RELEASE, THEN
//!   the inbox file IO (no IO under the state lock).
//! - `/replies`: peek under the lock; if absent, park on the Condvar which
//!   ATOMICALLY releases the lock while parked; re-check peek on each wake. The
//!   socket write happens AFTER the lock is dropped.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::relay_server::state::is_reply_text;
use crate::relay_server::RelayServer;

/// Maximum accepted REQUEST size (headers + body), mirroring the client's
/// response ceiling (`relay_http.rs` `MAX_RELAY_BODY`). Relay request bodies are
/// tiny JSON (`{text, from_session}`); a request that would exceed 1 MiB — a
/// mis-framed or hostile counterpart streaming without end — is rejected as a
/// clean 400 rather than accumulated unbounded into memory. spec W2.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Maximum number of concurrent connection-handler threads (M5a item i). The
/// thread-per-conn accept loop is otherwise UNCAPPED — a flood (hostile or a
/// misbehaving peer) could spawn unbounded threads and exhaust the per-uid
/// process/thread ceiling (the same ceiling the 06-08 fleet incident hit from a
/// different source). The real peer count is dozens of fleet sessions on
/// localhost, so 128 is generous headroom: it never rejects a legitimate caller
/// yet hard-bounds the blast radius of a connection flood. On accept at the cap we
/// CLOSE the connection immediately (drop the stream) instead of spawning — a
/// bounded, non-hanging rejection (the client surfaces it as a connection error
/// and retries, the same as any transient transport failure).
const MAX_CONN_THREADS: usize = 128;

// The request-read wall-clock budget (the request-side mirror of the client's
// per-read re-armed timeout, `relay_http.rs` `timed_read`) is no longer a const:
// it is INJECTABLE via `RelayServer::request_read_timeout` (orc carry 4), so the QA
// slow-drip harness row can pass a SHORT budget through this SAME reader.
// Production passes 10s (`PROD_REQUEST_READ_TIMEOUT` in `mod.rs`).
//
// A slow-drip counterpart that dribbles bytes below any per-read window can still
// never exceed this TOTAL budget — when it drains, the next read arms a near-zero
// timeout and surfaces a read error → we drop the connection. spec W2.
//
// This is a REQUEST-read deadline only. It is DISTINCT from the `/replies`
// long-poll park deadline (P-F3): the park happens AFTER the request is fully read,
// on the Condvar, and is NOT bounded by it (the only park deadline is the Condvar
// `wait_timeout`). No `SO_RCVTIMEO` shorter than the park deadline is ever set on
// the parked connection (P-F3).

/// A parsed inbound HTTP/1.1 request: just the fields the relay endpoints need.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// RAII counter guard for the in-flight connection-handler cap (M5a item i). Holds
/// a clone of the shared `AtomicUsize`; on `Drop` it decrements EXACTLY once. This
/// is what makes the cap panic-safe: a handler thread that panics mid-request still
/// runs the guard's `Drop` (Rust unwinds through stack-allocated values), so the
/// in-flight count never leaks even when a connection handler panics. The guard is
/// constructed AFTER a successful increment-under-cap, moved into the handler
/// closure, and dropped when the closure returns or unwinds.
struct ConnGuard(Arc<AtomicUsize>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        // Release the slot. `fetch_sub` is the exact inverse of the `compare/add`
        // that admitted this connection; one increment ⇒ one decrement.
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Run the accept loop FOREVER on `listener`, spawning a thread per connection up
/// to `MAX_CONN_THREADS` concurrent handlers (M5a item i). Blocks the caller (the
/// boot path joins this in production). `accept()` errors are skipped (transient);
/// the loop never exits on its own.
pub fn serve(listener: TcpListener, server: Arc<RelayServer>) {
    // Shared in-flight handler counter (item i). An atomic — never under the state
    // lock, so the cap check is contention-free and cannot interact with relay
    // state (P-G6 posture: the accept loop touches no state lock).
    let in_flight = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                // Reserve a slot: increment, and if that pushed us OVER the cap,
                // back out and REJECT (close the connection by dropping `stream`).
                // The fetch_add returns the PRIOR value, so `prior >= cap` means we
                // were already full — undo our increment and drop the connection.
                let prior = in_flight.fetch_add(1, Ordering::AcqRel);
                if prior >= MAX_CONN_THREADS {
                    in_flight.fetch_sub(1, Ordering::AcqRel);
                    // Bounded, non-hanging rejection: dropping `stream` closes the
                    // socket. We do NOT spawn a thread (the whole point of the cap).
                    drop(stream);
                    continue;
                }
                let guard = ConnGuard(Arc::clone(&in_flight));
                let server = Arc::clone(&server);
                // Thread-per-connection (spec §2). A panic in one handler must
                // not poison the others; thread isolation provides that. The
                // `guard` is moved in and decrements the in-flight count on drop —
                // including on a panic-unwind, so the cap never leaks (item i).
                thread::spawn(move || {
                    let _guard = guard;
                    handle_connection(stream, &server);
                });
            }
            // A transient accept error (e.g. EMFILE) must not kill the listener.
            Err(_) => continue,
        }
    }
}

/// Handle one connection end-to-end: parse the request (bounded), dispatch to the
/// matching endpoint, write the response. Any parse failure → a clean 400; an
/// unknown route → 404 (P-A5). Errors are swallowed (a broken pipe = peer gone,
/// normal — never spiral; spec P-F2).
fn handle_connection(mut stream: TcpStream, server: &RelayServer) {
    let parsed = match read_request(&mut stream, server.request_read_timeout) {
        Ok(p) => p,
        // Malformed / oversized / slow-drip → 400, never a hang (W2).
        Err(_) => {
            let _ = write_response(&mut stream, 400, "{\"error\":\"bad request\"}");
            return;
        }
    };
    dispatch(&parsed, &mut stream, server);
}

/// Read + parse one HTTP/1.1 request off `stream`, bounded by `MAX_REQUEST_BYTES`
/// and a wall-clock deadline (W2). Handles `Content-Length` bodies (the client
/// always sends `Connection: close` + `Content-Length` on `POST /message`).
///
/// Returns `Err(())` on any framing failure / oversize / timeout — the caller
/// turns that into a 400. Mirrors `relay_http.rs::read_response` on the read side.
fn read_request(stream: &mut TcpStream, read_timeout: Duration) -> Result<ParsedRequest, ()> {
    let deadline = Instant::now() + read_timeout;

    // --- Read the header block (terminated by CRLFCRLF), possibly overshooting
    // into the body. The header read is bounded by the same wall-clock deadline
    // and byte ceiling as the body (a headers-forever counterpart must not hang
    // or balloon memory). Mirror of relay_http.rs:287-306.
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        // Crisp ceiling (M5a ii): reject AT MAX_REQUEST_BYTES, not ceiling+chunk.
        // The OLD check (`> MAX_REQUEST_BYTES`, AFTER a 1KiB chunk was appended)
        // allowed up to ~1KiB of overshoot before rejecting; we now reject the
        // moment the accumulated bytes REACH the ceiling without a complete header
        // block. A legitimate header block that completes within the ceiling is
        // still accepted — it would have matched `find_subslice` above first.
        if buf.len() >= MAX_REQUEST_BYTES {
            return Err(());
        }
        // Bound the chunk so `buf` can never exceed MAX_REQUEST_BYTES even on the
        // read that crosses the ceiling: read at most the bytes remaining to the
        // ceiling. The next loop iteration then rejects at the `>=` check above.
        let room = MAX_REQUEST_BYTES - buf.len();
        let mut chunk = [0u8; 1024];
        let want = room.min(chunk.len());
        match timed_read(stream, deadline, &mut chunk[..want]) {
            Ok(0) => return Err(()), // EOF before headers complete
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(()) => return Err(()),
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_text.split("\r\n");

    // Request line: `METHOD PATH HTTP/1.1`.
    let request_line = lines.next().ok_or(())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(())?.to_string();
    let path = parts.next().ok_or(())?.to_string();
    // (HTTP version is parsed-and-ignored; the client always speaks /1.1.)

    // Headers (case-insensitive names) — we only need Content-Length.
    let mut content_length: usize = 0;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue; // permissive: skip malformed header lines (mirror client)
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            // A non-numeric / oversized Content-Length is a bad request.
            let len: usize = value.trim().parse().map_err(|_| ())?;
            if len > MAX_REQUEST_BYTES {
                return Err(());
            }
            content_length = len;
        }
    }

    // --- Body: read until we hold `content_length` bytes past the header block.
    // We may already have some body bytes (overshot during the header read).
    // `content_length` is already capped at MAX_REQUEST_BYTES above, so the body
    // ceiling is enforced by the loop bound itself.
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        // Crisp ceiling (M5a ii): read at most the bytes still NEEDED, so `body`
        // never overshoots `content_length` by up to a 4KiB chunk (the OLD code
        // read a full 4096 then `truncate`d the slop). With `content_length` capped
        // at MAX_REQUEST_BYTES, `body` now never exceeds the ceiling at any instant.
        let need = content_length - body.len();
        let mut chunk = [0u8; 4096];
        let want = need.min(chunk.len());
        match timed_read(stream, deadline, &mut chunk[..want]) {
            // B2 item 4b (delivered-means-byte-intact at the wire, ratified
            // Q2): EOF with bytes still owed = the peer died mid-write. The
            // request is a FRAMING VIOLATION → 400. The old `break` returned
            // the truncated body, which handle_message then recorded/emitted
            // as empty text under a freshly minted id while answering 200 —
            // silent byte loss reported as success (the phase-1 defect pin).
            Ok(0) => return Err(()),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(()) => return Err(()),
        }
    }
    body.truncate(content_length);

    Ok(ParsedRequest { method, path, body })
}

/// Re-arm `stream`'s read timeout to the budget REMAINING until `deadline`, then
/// run one `read`. The single point where the request-read wall-clock deadline is
/// enforced (W2): a drained budget arms a 1ns timeout (0 disables the deadline on
/// some platforms) so the pending read surfaces an error → `Err(())`. Mirror of
/// `relay_http.rs::timed_read`.
fn timed_read(stream: &mut TcpStream, deadline: Instant, buf: &mut [u8]) -> Result<usize, ()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .unwrap_or(Duration::from_nanos(1));
    stream.set_read_timeout(Some(remaining)).map_err(|_| ())?;
    stream.read(buf).map_err(|_| ())
}

/// Dispatch a parsed request to the matching endpoint (server.ts:248-334 fetch
/// handler). Anything unmatched → 404 `not found` (P-A5, server.ts:333).
fn dispatch(req: &ParsedRequest, stream: &mut TcpStream, server: &RelayServer) {
    let result = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => handle_health(server),
        // B2 item 4b: a /message body that fails the text contract is a 400,
        // never a minted id (see handle_message).
        ("POST", "/message") => match handle_message(req, server) {
            Ok(body) => body,
            Err(()) => {
                let _ = write_response(stream, 400, "{\"error\":\"bad request\"}");
                return;
            }
        },
        ("GET", "/inbox") => handle_inbox(server),
        // /replies writes the response itself (it may park on the Condvar and
        // must not hold any borrow across the wait) — handled specially below.
        ("GET", p) if p.starts_with("/replies/") => {
            handle_replies(p, stream, server);
            return;
        }
        // P-A5: unknown route/method → 404 `not found` (server.ts:333).
        _ => {
            let _ = write_response_text(stream, 404, "not found");
            return;
        }
    };
    let _ = write_response(stream, 200, &result);
}

/// `GET /health` → 200 JSON `{sessionId, port, pid, status:"ok"}` (P-A1,
/// server.ts:251-253). `sessionId` non-empty (the client's `health()` rejects an
/// empty one as BadResponse). Reads only immutable config — no state lock needed.
fn handle_health(server: &RelayServer) -> String {
    serde_json::json!({
        "sessionId": server.session_id,
        "port": server.port,
        "pid": server.pid,
        "status": "ok",
    })
    .to_string()
}

/// `POST /message` body `{text, from_session?}` → 200 `{message_id}` (P-A2),
/// or `Err(())` → 400 when the body is not JSON or carries no string `text`
/// (B2 item 4b — never a minted id for a payload that did not arrive intact).
///
/// Flow (P-G6 + P-C1 ordering is BINDING):
/// (a) LOCK state → mint message_id → record_origin → UNLOCK (no IO under lock).
/// (b) Persist `<inbox>/<message_id>.json` BEFORE notifying — the "persist before
///     notify" durability belt (P-C1, server.ts:268-279) — done OUTSIDE the lock.
/// (c) Emit the MCP `notifications/claude/channel` (P-B4, M3) — done OUTSIDE the
///     state lock under the SEPARATE `Mutex<Stdout>` (P-G6). The persist-first
///     ordering (b) means a later notify failure is still recoverable from the inbox.
fn handle_message(req: &ParsedRequest, server: &RelayServer) -> Result<String, ()> {
    // B2 item 4b (delivered-means-byte-intact, ratified Q2 — a DIVERGENCE
    // from server.ts's permissive `body.text` read): a body that does not
    // parse as JSON, or that lacks a STRING `text`, is REJECTED (the caller
    // answers 400) and NO message_id is minted. The old permissive read
    // turned any mangled body into a recorded, persisted, channel-emitted
    // EMPTY message under a 200 — the sender (e.g. a push-back) then believed
    // a body it never landed was delivered. An explicitly empty string text
    // is still legal (present and typed).
    let body: serde_json::Value = serde_json::from_slice(&req.body).map_err(|_| ())?;
    let text = body
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or(())?
        .to_string();
    // from_session stays permissive ("unknown") — it is attribution metadata,
    // not payload; the relay contract marks it optional.
    let from_session = body
        .get("from_session")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // (a) State ops UNDER the lock, then RELEASE before any IO (P-G6).
    let message_id = {
        // Poison-resilience (cond 4): recover the guard rather than propagate a
        // panic — a panic in ANY state critical section must not brick the relay
        // fleet-wide by poisoning this Mutex. Same posture as mcp::write_line.
        let mut state = server.state.lock().unwrap_or_else(|p| p.into_inner());
        let id = state.mint_message_id(now_ms());
        // Remember the origin so a later reply (M4) can route back — with the
        // is_reply flag feeding the loop-prevention belt (P-E4, server.ts:262-266).
        state.record_origin(id.clone(), from_session.clone(), is_reply_text(&text));
        id
        // lock dropped here
    };

    // (b) Persist to inbox BEFORE notify (P-C1, server.ts:268-279) — OUTSIDE the
    // lock (no IO under the state lock, P-G6). A write failure is non-fatal
    // (logged-equivalent: silently tolerated in M2; M5 adds the log).
    let received_at = crate::render::epoch_ms_to_iso(now_ms() as i64);
    let record = serde_json::json!({
        "text": text,
        "from_session": from_session,
        "message_id": message_id,
        "received_at": received_at,
        // WS-B / B1 (additive): the addressed recipient = THIS server's own session
        // (the message is *for* this relay's Claude Code). This is the presence-gate
        // key the inbox GC reads (gc.rs `inbox_is_collectible`); absent on the ~2019
        // legacy files, which are then TTL-judged as unaddressable.
        "to_session": server.session_id,
    });
    write_inbox_file(server, &message_id, &record.to_string());

    // (b') R3c-Step-1 enqueue hook: AFTER persisting the inbox file, nudge the
    // (possibly parked/wedged) recipient session on its ALWAYS-SERVICED control fd.
    // The recipient is THIS server's own session (`server.session_id`); its sbmux
    // daemon binds the matching `control_sock_path` (derived from the SAME env) and
    // drains the `WakeInbox`. If there is no live servicer (ENOENT/ECONNREFUSED) the
    // wake degrades to a logged PTY-inject fallback — and the in-process stdout
    // notification (step c) remains the delivery to a LIVE consumer. The fallback
    // branch is the load-bearing proof: revert this hook or kill the ctrl reader and
    // the always-serviced-fd wake is gone (the §3 R3c-1 negative control).
    {
        use crate::control_sock::{control_sock_path, wake_inbox, WakeOutcome};
        let ctrl = control_sock_path(&server.paths.state_dir, &server.session_id);
        if let WakeOutcome::PtyFallback { reason } = wake_inbox(&ctrl) {
            eprintln!(
                "relay[wake]: control socket {ctrl:?} for session {} absent ({reason}); \
                 inbox wake fell back to PTY-inject path",
                server.session_id
            );
        }
    }

    // (c) P-B4 (M3): emit the outbound MCP `notifications/claude/channel` here
    // (server.ts:281-295). The state lock was ALREADY released by step (a), so this
    // stdout write happens with NO state lock held — the P-G6 invariant holds for
    // free. The write itself takes the SEPARATE `Mutex<Stdout>` (inside
    // `emit_channel_notification`), so a stuck/slow Claude-Code stdout consumer can
    // stall only that lock, never relay state. A broken stdout pipe (CC gone) is
    // swallowed there — persist-before-notify (step b) is the durability belt.
    crate::relay_server::mcp::emit_channel_notification(server, &text, &from_session, &message_id);

    Ok(serde_json::json!({ "message_id": message_id }).to_string())
}

/// `GET /replies/<id>` (P-A3). FIRST peek the resolved buffer under the lock — if
/// present, return it IMMEDIATELY (the cached branch, server.ts:303-306; peek is
/// idempotent — re-GET returns the same text). Else LONG-POLL: register a waiter
/// and park on the Condvar in [`WAITER_LIVENESS_TICK`]-capped slices up to the
/// REMAINING budget, re-checking peek on each wake (guards spurious wakeups +
/// lost-wakeup P-G1) and probing client liveness each tick (B2 item 4 fix i —
/// a hung-up sender's waiter is deregistered within one tick, never left to
/// shadow a push-back). On budget drain → HTTP 408 `{error:"timeout",
/// message_id}` (server.ts:309-312). On resolve, the write outcome is reported
/// to any verify-pending `deliver_reply` (B2 item 4 fix ii — resolve-and-verify).
///
/// CRITICAL (P-G6/P-G3): the Condvar `wait_timeout` ATOMICALLY releases the state
/// lock while parked. The lock is NEVER held across the wait, across the liveness
/// probe, or across the socket write. No `SO_RCVTIMEO` is set on this connection
/// (P-F3) — the park deadline lives in the Condvar `wait_timeout` slices.
fn handle_replies(path: &str, stream: &mut TcpStream, server: &RelayServer) {
    let message_id = &path["/replies/".len()..];
    // The park deadline is INJECTABLE via the SAME code path (P-F3b): production
    // sets `reply_park_timeout = 120s`; tests inject e.g. 200ms. No forked branch.
    let deadline = Instant::now() + server.reply_park_timeout;

    // Poison-resilience (cond 4): recover the guard (see handle_message).
    let mut state = server.state.lock().unwrap_or_else(|p| p.into_inner());

    // Cached branch (server.ts:303-306): a buffered reply returns immediately.
    // NOT a verify-window resolve — no waiter was registered, no ack is owed.
    if let Some(text) = state.peek_resolved(message_id, Instant::now()) {
        drop(state); // release before the socket write (P-G6)
        let body = serde_json::json!({ "text": text, "message_id": message_id }).to_string();
        let _ = write_response(stream, 200, &body);
        return;
    }

    // LONG-POLL: register intent, then park on the Condvar. The waiter is checked
    // under the lock before sleeping, so a `reply` that lands in the park-register
    // window is not lost (P-G1; the buffer-first write is the belt, this is the
    // suspenders).
    //
    // B2 item 4 fix (i) — WAITER LIVENESS: each park is capped at
    // [`WAITER_LIVENESS_TICK`] so the loop wakes periodically and PROBES the
    // client socket (non-blocking read, lock RELEASED — P-G6). A sender that
    // hung up (its --timeout fired, process killed, ^C) is detected within one
    // tick: the stale waiter is DEREGISTERED immediately, so a later reply
    // falls through to push-back instead of resolving into a dead socket (the
    // diagnosed item-4 body-loss mechanism, punch_b2_item4_repro.rs). The probe
    // is read-side EOF detection — reliable once the peer's FIN arrived, unlike
    // a first write (which often "succeeds" into the kernel buffer).
    state.register_waiter(message_id.to_string());
    let resolved_text = loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) if !d.is_zero() => d,
            _ => break None, // budget drained → timeout
        };
        // Park at most one liveness tick; the FULL deadline is enforced by the
        // re-check above (a tick expiry is NOT a park timeout).
        // wait_timeout ATOMICALLY releases the lock while parked, re-acquires on
        // wake (P-G3 — the lock is NOT held across the wait).
        // wait_timeout can also surface a poisoned guard — recover it (cond 4).
        let park = remaining.min(WAITER_LIVENESS_TICK);
        let (guard, _wait_result) = server
            .cvar
            .wait_timeout(state, park)
            .unwrap_or_else(|p| p.into_inner());
        state = guard;
        // Re-check the buffer on EVERY wake (spurious wakeups + real resolves).
        if let Some(text) = state.peek_resolved(message_id, Instant::now()) {
            break Some(text);
        }
        // Tick wake with no reply yet: probe client liveness with the lock
        // RELEASED (the probe is a syscall — P-G6), then re-acquire and re-check
        // the buffer (a reply may have landed while we probed).
        drop(state);
        let alive = client_alive(stream);
        state = server.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(text) = state.peek_resolved(message_id, Instant::now()) {
            break Some(text);
        }
        if !alive {
            // The sender hung up: DEREGISTRATION is the live mechanism here —
            // remove the waiter NOW so it can never shadow a push-back.
            //
            // The offer_ack(false) + notify below is a BELT that is provably a
            // no-op today: this branch is reachable only after the re-peek
            // above returned None, and deliver_reply sets the verify marker
            // under the SAME lock acquisition as its buffer write (lock span
            // #1, with RESOLVED_TTL = 5min dwarfing this 250ms tick) — so a
            // verify marker cannot be present here without the buffer also
            // being peekable. The belt becomes live only if mark_verify ever
            // decouples from the buffer-write lock acquisition; it is kept so
            // that refactor fails SAFE (instant nack → push-back) instead of
            // stranding the verify window to its 250ms timeout.
            state.remove_waiter(message_id);
            state.offer_ack(message_id, false); // belt — dropped today (above)
            server.cvar.notify_all();
            drop(state);
            return; // no socket write — the peer is gone.
        }
    };
    state.remove_waiter(message_id);
    drop(state); // release BEFORE the socket write (P-G6)

    match resolved_text {
        Some(text) => {
            // B2 item 4 fix (ii) — RESOLVE-AND-VERIFY: report the WRITE OUTCOME
            // to any verify-pending deliver_reply (state.offer_ack drops the
            // report when no verify window is open, so nothing leaks). Probe
            // liveness BEFORE the write — read-side EOF is the reliable
            // dead-peer signal; only a successful write to a live peer counts
            // as delivered.
            let body = serde_json::json!({ "text": text, "message_id": message_id }).to_string();
            let written = client_alive(stream) && write_response(stream, 200, &body).is_ok();
            let mut state = server.state.lock().unwrap_or_else(|p| p.into_inner());
            state.offer_ack(message_id, written);
            server.cvar.notify_all();
        }
        None => {
            // Timeout → HTTP 408 (server.ts:309-312). The client's `fetch_reply`
            // surfaces this as a non-2xx `ServerError`, then reads `{error}`.
            let body =
                serde_json::json!({ "error": "timeout", "message_id": message_id }).to_string();
            let _ = write_response(stream, 408, &body);
        }
    }
}

/// How long a parked `/replies` long-poll sleeps between client-liveness probes
/// (B2 item 4 fix i). Bounds the stale-waiter window to one tick — formerly it
/// was the FULL park budget (up to 120s in production, and INDEPENDENT of the
/// client's own --timeout), during which a reply would resolve into the dead
/// socket and be falsely marked delivered. Dozens of parked fleet connections
/// waking 4×/s is negligible.
const WAITER_LIVENESS_TICK: Duration = Duration::from_millis(250);

/// Non-blocking client-liveness probe on a parked long-poll connection: a read
/// returning EOF (`Ok(0)`) or a non-WouldBlock error means the peer closed or
/// reset — the waiter is stale. `WouldBlock` (no data, connection open) is the
/// healthy case. The client never sends bytes after its request (`Connection:
/// close` request/response), so a stray readable byte is treated as alive and
/// discarded. Blocking mode is restored before returning (the response write
/// path relies on it).
///
/// KNOWN PROBE LIMITATION (red-team round 1, W2 — comment-pin): a client that
/// HALF-CLOSES its write side (`shutdown(SHUT_WR)`) after sending its request
/// while still reading would deliver the same FIN this probe reads as EOF —
/// indistinguishable from a hangup, so its waiter would be deregistered and
/// the reply routed to push-back (+ the resolved buffer) instead of its
/// long-poll. No real counterpart does this: the engine's `CcRelay` client
/// holds its socket fully open for the duration of `fetch_reply`, and curl
/// likewise; half-close-after-request is not part of the relay protocol.
fn client_alive(stream: &mut TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut buf = [0u8; 1];
    let alive = match stream.read(&mut buf) {
        Ok(0) => false, // EOF — the peer closed (FIN received)
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(_) => false, // reset / hard error — peer gone
    };
    let _ = stream.set_nonblocking(false);
    alive
}

/// `GET /inbox` → 200 `{messages:[...], count}` reading every `*.json` in the
/// inbox dir; unreadable files skipped; a missing dir → `{messages:[], count:0}`
/// (P-A4, server.ts:320-331). Pure file IO — no state lock.
fn handle_inbox(server: &RelayServer) -> String {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&server.paths.inbox_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // Only *.json (server.ts:322 `.filter(f => f.endsWith('.json'))`).
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Unreadable / unparseable files are SKIPPED, not fatal (server.ts:324-325).
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    // WS-B / B1 — ack-on-read: a session draining its inbox has taken
                    // delivery of the mail addressed to it; stamp it so GC can collect
                    // it after the short ack grace instead of the full TTL.
                    maybe_ack_on_read(server, &path, &mut value);
                    messages.push(value);
                }
            }
        }
    }
    let count = messages.len();
    serde_json::json!({ "messages": messages, "count": count }).to_string()
}

/// WS-B / B1 — ack-on-read. If `value` is addressed to THIS server's session
/// (`to_session == server.session_id`) and carries no `acked_at_ms`, stamp it
/// `now` and rewrite the file via an **atomic tmp+rename** (§5.4) so a concurrent
/// reader / GC never sees a torn record. The in-memory `value` is mutated in place
/// so the returned listing reflects the ack. BEST-EFFORT and LOSS-SAFE: ack is
/// additive metadata, never a delete — any IO failure leaves the file un-acked
/// (it re-acks on the next poll, or TTL-expires), so a crash mid-stamp loses
/// nothing. Idempotent: a second read is a no-op (numeric `acked_at_ms` present).
fn maybe_ack_on_read(server: &RelayServer, path: &std::path::Path, value: &mut serde_json::Value) {
    ack_stamp_file(&server.session_id, path, value, now_ms() as i64);
}

/// The ack-on-read core (clock injected — testable). Returns `true` iff it stamped
/// and rewrote the file. Stamps `acked_at_ms = now_ms` IFF `value.to_session ==
/// session_id` and no numeric `acked_at_ms` is present; mutates `value` in place
/// and rewrites via atomic tmp+rename. Any IO failure ⇒ `false`, file left
/// un-acked (loss-safe — ack is a GC hint, never a delete).
fn ack_stamp_file(
    session_id: &str,
    path: &std::path::Path,
    value: &mut serde_json::Value,
    now_ms: i64,
) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let addressed_to_me = obj
        .get("to_session")
        .and_then(|v| v.as_str())
        .map(|s| s == session_id)
        .unwrap_or(false);
    if !addressed_to_me {
        return false; // another session's mail, or a legacy unaddressed file.
    }
    if obj.get("acked_at_ms").and_then(|v| v.as_i64()).is_some() {
        return false; // already acked → idempotent no-op.
    }
    obj.insert("acked_at_ms".to_string(), serde_json::Value::from(now_ms));
    let Ok(serialized) = serde_json::to_vec(value) else {
        return false;
    };
    let (Some(dir), Some(fname)) = (path.parent(), path.file_name().and_then(|f| f.to_str()))
    else {
        return false;
    };
    let tmp = dir.join(format!(".{fname}.ack.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &serialized).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Persist one inbox file `<inbox>/<message_id>.json` (P-C1). Creates the inbox
/// dir if absent (server.ts:66 `mkdirSync(..., {recursive:true})`). A write
/// failure is non-fatal (the notification path is the live channel; the inbox is
/// the durability fallback) — M5 adds the log line.
fn write_inbox_file(server: &RelayServer, message_id: &str, contents: &str) {
    let dir = &server.paths.inbox_dir;
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join(format!("{message_id}.json"));
    let _ = std::fs::write(path, contents);
}

/// Current wall-clock epoch ms (the `relay-<epoch_ms>-<seq>` mint + `received_at`).
/// The relay server is a real long-running process — the real clock is correct
/// here (spec §2 "Time"). Mirrors `RealClock::now_ms` without pulling the seam in
/// (the server is not unit-tested against a fixed clock; the 4a harness drives it
/// live).
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write a `200`/`408`/etc JSON response with `Content-Length` framing +
/// `Connection: close` (P-A6 — the client accepts Content-Length, and emitting it
/// keeps framing minimal). The client reads to EOF on `Connection: close` even
/// without a length, but we send both for robustness.
fn write_response(stream: &mut TcpStream, status: u16, json_body: &str) -> std::io::Result<()> {
    write_raw(stream, status, "application/json", json_body.as_bytes())
}

/// Write a plain-text response (the 404 `not found` body, server.ts:333).
fn write_response_text(stream: &mut TcpStream, status: u16, text: &str) -> std::io::Result<()> {
    write_raw(stream, status, "text/plain", text.as_bytes())
}

/// Shared response writer: status line + minimal headers + body. `Connection:
/// close` matches the client's per-request connection model (`relay_http.rs`
/// sends `Connection: close` and reads to EOF).
fn write_raw(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = status_reason(status);
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Reason phrase for the status codes this server emits.
fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        408 => "Request Timeout",
        _ => "OK",
    }
}

/// Find the first index of `needle` in `haystack` (small linear scan — request
/// headers are tiny). Mirror of `relay_http.rs::find_subslice`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener as StdTcpListener;

    // --- WS-B / B1 ack-on-read (ack_stamp_file) ---

    fn write_json(dir: &std::path::Path, name: &str, json: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, json).unwrap();
        p
    }

    #[test]
    fn ack_stamps_addressed_unacked_and_rewrites_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_json(
            tmp.path(),
            "relay-1.json",
            r#"{"text":"hi","from_session":"a","message_id":"relay-1","received_at":"2026-06-25T00:00:00.000Z","to_session":"me"}"#,
        );
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert!(ack_stamp_file("me", &p, &mut v, 1_700_000_000_000));
        // In-memory value reflects the ack...
        assert_eq!(v["acked_at_ms"].as_i64(), Some(1_700_000_000_000));
        // ...and so does the rewritten file (durable, all fields preserved).
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(on_disk["acked_at_ms"].as_i64(), Some(1_700_000_000_000));
        assert_eq!(on_disk["text"].as_str(), Some("hi"));
        assert_eq!(on_disk["to_session"].as_str(), Some("me"));
    }

    #[test]
    fn ack_idempotent_and_respects_addressing() {
        let tmp = tempfile::tempdir().unwrap();
        // Already acked → no-op.
        let p = write_json(
            tmp.path(),
            "a.json",
            r#"{"message_id":"a","to_session":"me","acked_at_ms":42}"#,
        );
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert!(!ack_stamp_file("me", &p, &mut v, 999));
        assert_eq!(v["acked_at_ms"].as_i64(), Some(42), "ack is idempotent");

        // Addressed to ANOTHER session → never acked.
        let mut other: serde_json::Value =
            serde_json::json!({"message_id":"b","to_session":"someone-else"});
        assert!(!ack_stamp_file("me", tmp.path(), &mut other, 999));
        assert!(other.get("acked_at_ms").is_none());

        // Legacy unaddressed file (no to_session) → never acked.
        let mut legacy: serde_json::Value =
            serde_json::json!({"message_id":"c","text":"x","from_session":"s"});
        assert!(!ack_stamp_file("me", tmp.path(), &mut legacy, 999));
        assert!(legacy.get("acked_at_ms").is_none());
    }

    // --- find_subslice ---

    #[test]
    fn find_subslice_basic() {
        assert_eq!(find_subslice(b"GET / HTTP", b" / "), Some(3));
        assert_eq!(find_subslice(b"a\r\n\r\nb", b"\r\n\r\n"), Some(1));
        assert_eq!(find_subslice(b"abc", b"xyz"), None);
        assert_eq!(find_subslice(b"", b"x"), None);
    }

    // --- request parser ---
    //
    // We drive `read_request` against a real loopback socket pair: a client
    // thread writes the raw request bytes, the test reads + parses the server
    // end. This exercises the SAME parser production uses (not a string fake).

    /// Connect a client/server TcpStream pair over loopback, run `client_write`
    /// on the client end, and return the parsed result of the server end.
    fn parse_over_socket(raw: &[u8]) -> Result<ParsedRequest, ()> {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let raw = raw.to_vec();
        let client = thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(&raw).unwrap();
            // Drop closes the connection → server read sees EOF after the bytes.
        });
        let (mut server_stream, _) = listener.accept().unwrap();
        // The parser tests pass a generous fixed budget — they assert framing, not
        // the timeout (the slow-drip timeout is a QA harness row).
        let parsed = read_request(&mut server_stream, Duration::from_secs(10));
        client.join().unwrap();
        parsed
    }

    #[test]
    fn parse_valid_post_message() {
        let body = "{\"text\":\"hi\",\"from_session\":\"sess-A\"}";
        let raw = format!(
            "POST /message HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let parsed = parse_over_socket(raw.as_bytes()).expect("valid request parses");
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/message");
        assert_eq!(parsed.body, body.as_bytes());
    }

    #[test]
    fn parse_valid_get_health_no_body() {
        let raw = "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        let parsed = parse_over_socket(raw.as_bytes()).expect("GET with no body parses");
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.path, "/health");
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn parse_replies_path_preserved() {
        let raw =
            "GET /replies/relay-123-1 HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        let parsed = parse_over_socket(raw.as_bytes()).expect("parses");
        assert_eq!(parsed.path, "/replies/relay-123-1");
    }

    #[test]
    fn parse_oversized_content_length_rejected() {
        // A declared Content-Length over the ceiling is rejected up front (W2) —
        // never read a body that announces itself as oversized.
        let raw = format!(
            "POST /message HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_REQUEST_BYTES + 1
        );
        assert!(
            parse_over_socket(raw.as_bytes()).is_err(),
            "oversized Content-Length must be rejected"
        );
    }

    #[test]
    fn parse_garbage_content_length_rejected() {
        let raw = "POST /message HTTP/1.1\r\nContent-Length: not-a-number\r\n\r\n";
        assert!(
            parse_over_socket(raw.as_bytes()).is_err(),
            "non-numeric Content-Length must be rejected"
        );
    }

    #[test]
    fn parse_eof_before_headers_complete_rejected() {
        // No CRLFCRLF terminator before EOF → bad request (not a hang).
        let raw = "GET /health HTTP/1.1\r\nHost: 127.0.0.1";
        assert!(
            parse_over_socket(raw.as_bytes()).is_err(),
            "truncated headers must be rejected"
        );
    }

    #[test]
    fn parse_empty_request_rejected() {
        // Immediate EOF (empty connection) → bad request, never a hang.
        assert!(parse_over_socket(b"").is_err());
    }

    #[test]
    fn parse_body_shorter_than_content_length_rejected() {
        // B2 item 4b (FLIPPED from the old return-what-we-have pin): EOF with
        // bytes still owed against Content-Length = the peer died mid-write —
        // a framing violation → Err (the caller answers 400). The old
        // tolerance let a truncated /message body mint an id and report 200
        // with empty text (silent byte loss as success — the phase-1 defect
        // pin in punch_b2_item4_repro.rs).
        let raw = "POST /message HTTP/1.1\r\nContent-Length: 100\r\n\r\n{\"text\":\"x\"}";
        assert!(
            parse_over_socket(raw.as_bytes()).is_err(),
            "a body short of its Content-Length must be rejected"
        );
    }

    #[test]
    fn parse_case_insensitive_content_length_header() {
        let body = "{}";
        let raw = format!(
            "POST /message HTTP/1.1\r\ncOnTeNt-LeNgTh: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let parsed = parse_over_socket(raw.as_bytes()).expect("parses");
        assert_eq!(parsed.body, body.as_bytes());
    }

    // --- M5a item ii: crisp request-byte ceiling (no overshoot) ---

    #[test]
    fn parse_oversized_header_block_rejected_at_ceiling() {
        // A header block that never terminates and exceeds MAX_REQUEST_BYTES must be
        // rejected (no CRLFCRLF within the ceiling). We send a giant header line and
        // then close — the server must Err (400), never accumulate past the ceiling.
        let mut raw = b"GET /".to_vec();
        // Push well past the 1 MiB ceiling with no CRLFCRLF terminator.
        raw.extend(std::iter::repeat_n(b'a', MAX_REQUEST_BYTES + 5000));
        assert!(
            parse_over_socket(&raw).is_err(),
            "an over-ceiling unterminated header block must be rejected"
        );
    }

    // --- M5a item i: connection-cap counter (increment/decrement + panic-safety) ---

    #[test]
    fn conn_guard_decrements_on_drop() {
        let counter = Arc::new(AtomicUsize::new(0));
        counter.fetch_add(1, Ordering::AcqRel);
        {
            let _g = ConnGuard(Arc::clone(&counter));
            assert_eq!(counter.load(Ordering::Acquire), 1);
        } // guard dropped here
        assert_eq!(
            counter.load(Ordering::Acquire),
            0,
            "ConnGuard must decrement the in-flight count on drop"
        );
    }

    #[test]
    fn conn_guard_decrements_on_panic_unwind() {
        // The guard's Drop runs during a panic unwind, so a handler that panics
        // mid-request never leaks the in-flight count (item i panic-safety).
        let counter = Arc::new(AtomicUsize::new(0));
        counter.fetch_add(1, Ordering::AcqRel);
        let c2 = Arc::clone(&counter);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = ConnGuard(Arc::clone(&c2));
            panic!("simulated handler panic");
        }));
        assert!(result.is_err(), "the closure must have panicked");
        assert_eq!(
            counter.load(Ordering::Acquire),
            0,
            "ConnGuard must decrement even when the handler panics (no leak)"
        );
    }

    #[test]
    fn conn_cap_rejects_at_capacity_via_fetch_add_protocol() {
        // Mirror the accept-loop admission protocol: fetch_add returns the PRIOR
        // value; prior >= cap means REJECT (and back out the increment). Drive the
        // exact arithmetic to prove the cap admits up to MAX_CONN_THREADS and
        // rejects beyond it, with the counter restored on rejection.
        let in_flight = Arc::new(AtomicUsize::new(0));
        let mut guards = Vec::new();
        // Admit exactly the cap.
        for _ in 0..MAX_CONN_THREADS {
            let prior = in_flight.fetch_add(1, Ordering::AcqRel);
            assert!(prior < MAX_CONN_THREADS, "must admit below cap");
            guards.push(ConnGuard(Arc::clone(&in_flight)));
        }
        assert_eq!(in_flight.load(Ordering::Acquire), MAX_CONN_THREADS);
        // The next admission attempt is at the cap → reject + back out.
        let prior = in_flight.fetch_add(1, Ordering::AcqRel);
        assert!(prior >= MAX_CONN_THREADS, "at cap, prior must be >= cap");
        in_flight.fetch_sub(1, Ordering::AcqRel); // back out (the reject path)
        assert_eq!(
            in_flight.load(Ordering::Acquire),
            MAX_CONN_THREADS,
            "a rejected connection must restore the in-flight count"
        );
        // Dropping all guards returns to zero (every admitted slot is released).
        drop(guards);
        assert_eq!(in_flight.load(Ordering::Acquire), 0);
    }
}
