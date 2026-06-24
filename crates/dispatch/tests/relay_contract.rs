//! Harness-hosted relay contract tests (spec §4.5).
//!
//! An in-process `std::net::TcpListener` fake relay (bound to port 0 — NEVER a
//! fixed port, so parallel CI never collides) serves canned HTTP responses on a
//! spawned thread. The tests drive the REAL [`CcRelay`] HTTP adapter against it,
//! asserting the [`RelayContract`] surface (ADD-5): `/message`, `/replies/<id>`,
//! `/health`.
//!
//! The sharp rows (red-team targets):
//!
//! - **long-poll honors the budget** — the server sleeps 1-2s BEFORE replying to
//!   `/replies`; the fetch must still succeed. This row FAILS under a uniform
//!   short read timeout (W4): it proves the long-poll read timeout = the caller's
//!   full budget, not a truncated one.
//! - **chunked == content-length** — the SAME body delivered both ways must
//!   parse identically (the server is Bun/Node; chunked is realistic).
//! - **health-down (no hang)** — a sidecar file is present but NOTHING listens on
//!   the port. The probe/contract must return a clean degraded
//!   `RelayError::ConnectionFailed` and be WALL-CLOCK BOUNDED (elapsed <
//!   connect-timeout + slack), never hang.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use dispatch::relay::{RelayContract, RelayError};
use dispatch::relay_http::CcRelay;

/// How the fake relay should frame a response body.
///
/// The `Chunked*` variants exercise the riskiest branches of the real chunked
/// decoder (`relay_http::read_chunked` / `read_line`), per in-phase red-team #2 —
/// the realistic Bun/Node streaming cases that a single all-at-once chunk write
/// never hits. All must parse IDENTICAL to `ContentLength`.
#[derive(Clone, Copy)]
enum Framing {
    ContentLength,
    /// Two chunks, each written whole (the original coverage).
    Chunked,
    /// The chunk-SIZE LINE is split across two writes with a flush+sleep between
    /// (the Bun streaming case): the decoder's `read_line` must refill mid-line and
    /// still parse the size. THE riskiest untested branch (red-team #2).
    ChunkedSplitSizeLine,
    /// The chunk-size line carries a `;ext` chunk-extension suffix the decoder must
    /// ignore (it takes the hex up to the `;`).
    ChunkedExt,
    /// Trailer header lines after the terminating 0-chunk; the decoder must treat
    /// the 0-chunk as end-of-body and ignore the trailers.
    ChunkedTrailers,
    /// An OVERSIZED chunked body: stream `chunk_count` chunks of `chunk_bytes`
    /// each (no terminating 0-chunk), so the reassembled body exceeds the
    /// decoder's `MAX_RELAY_BODY` ceiling (W2). The decoder must reject with
    /// `BadResponse` instead of accumulating unbounded.
    ChunkedOversize {
        chunk_bytes: usize,
        chunk_count: usize,
    },
    /// An OVERSIZED `Content-Length`: the header DECLARES a body far above the
    /// ceiling. The decoder rejects on the declared length (W2), so the harness
    /// need not actually send the bytes.
    ContentLengthOversize {
        declared_len: usize,
    },
    /// A SLOW-DRIP chunked body: `chunk_count` small chunks, each preceded by a
    /// `gap` sleep WELL UNDER the caller's per-read window but whose CUMULATIVE
    /// total EXCEEDS the wall-clock budget. Every individual read beats its
    /// window; only a deadline THREADED INTO the loop (W2 / red-team M3) catches
    /// the total. The decoder must surface `Timeout`.
    ChunkedSlowDrip {
        chunk_count: usize,
        gap: Duration,
    },
}

/// A scripted fake-relay behavior for a single connection.
#[derive(Clone)]
struct Canned {
    /// HTTP status code to return.
    status: u16,
    /// Response JSON body.
    body: String,
    /// Framing for the body.
    framing: Framing,
    /// Sleep this long BEFORE writing ANY response bytes (simulates a long-poll
    /// that resolves late, or slow headers).
    delay: Duration,
}

impl Canned {
    fn ok(body: &str) -> Self {
        Canned {
            status: 200,
            body: body.to_string(),
            framing: Framing::ContentLength,
            delay: Duration::ZERO,
        }
    }
    fn chunked(mut self) -> Self {
        self.framing = Framing::Chunked;
        self
    }
    fn framed(mut self, f: Framing) -> Self {
        self.framing = f;
        self
    }
    fn with_delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }
}

/// A running fake relay. Holds the bound port and the join handle; the server
/// serves exactly `responses.len()` connections then exits.
struct FakeRelay {
    port: u16,
    handle: Option<thread::JoinHandle<()>>,
}

impl FakeRelay {
    /// Bind to 127.0.0.1:0, spawn a thread serving each queued `Canned` response
    /// to one connection in order.
    fn start(responses: Vec<Canned>) -> FakeRelay {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        // Signal the bound port is ready (bind already done; just spawn).
        let (ready_tx, ready_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            ready_tx.send(()).ok();
            for canned in responses {
                match listener.accept() {
                    Ok((stream, _)) => serve_one(stream, &canned),
                    Err(_) => break,
                }
            }
        });
        ready_rx.recv().ok();
        FakeRelay {
            port,
            handle: Some(handle),
        }
    }
}

impl Drop for FakeRelay {
    fn drop(&mut self) {
        // The server thread exits after serving its queued responses; join to
        // avoid leaking it. If a test sent fewer requests than queued responses,
        // the accept() blocks — so we detach rather than join in that case.
        if let Some(h) = self.handle.take() {
            // Best-effort: a quick connect to unblock a pending accept(), then
            // join. If it's already done, this no-ops.
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            let _ = h.join();
        }
    }
}

/// Read the client's request (headers; we don't need the body) then write the
/// canned response after the scripted delay.
fn serve_one(mut stream: TcpStream, canned: &Canned) {
    // Drain the request headers (read until CRLFCRLF or a small cap). We don't
    // parse the request — the tests assert on the RESPONSE path.
    let mut buf = [0u8; 4096];
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.read(&mut buf);

    if !canned.delay.is_zero() {
        thread::sleep(canned.delay);
    }

    let reason = if (200..300).contains(&canned.status) {
        "OK"
    } else {
        "ERR"
    };
    let mut resp = format!("HTTP/1.1 {} {reason}\r\n", canned.status);
    resp.push_str("Content-Type: application/json\r\n");
    resp.push_str("Connection: close\r\n");

    match canned.framing {
        Framing::ContentLength => {
            resp.push_str(&format!("Content-Length: {}\r\n\r\n", canned.body.len()));
            resp.push_str(&canned.body);
            let _ = stream.write_all(resp.as_bytes());
        }
        Framing::Chunked => {
            resp.push_str("Transfer-Encoding: chunked\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
            // Emit the body in two chunks to exercise multi-chunk reassembly,
            // then the terminating zero chunk.
            let bytes = canned.body.as_bytes();
            let mid = bytes.len() / 2;
            write_chunk(&mut stream, &bytes[..mid]);
            write_chunk(&mut stream, &bytes[mid..]);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        Framing::ChunkedSplitSizeLine => {
            resp.push_str("Transfer-Encoding: chunked\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            // ONE chunk, but its SIZE LINE is split mid-token across two writes with
            // a flush+sleep between (the realistic Bun streaming case): the decoder's
            // read_line must refill mid-line. e.g. body "2a" bytes → "2" then "a\r\n".
            let bytes = canned.body.as_bytes();
            let size_hex = format!("{:x}", bytes.len()); // ≥2 hex digits for typical bodies
            let split = size_hex.len() / 2; // split the hex token itself
            let _ = stream.write_all(&size_hex.as_bytes()[..split]);
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(60));
            let _ = stream.write_all(&size_hex.as_bytes()[split..]);
            let _ = stream.write_all(b"\r\n");
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(40));
            let _ = stream.write_all(bytes);
            let _ = stream.write_all(b"\r\n");
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        Framing::ChunkedExt => {
            resp.push_str("Transfer-Encoding: chunked\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
            // ONE chunk whose size line carries a `;ext=value` chunk extension the
            // decoder must ignore (hex up to the `;`).
            let bytes = canned.body.as_bytes();
            let header = format!("{:x};x-bun-stream=1\r\n", bytes.len());
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(bytes);
            let _ = stream.write_all(b"\r\n");
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        Framing::ChunkedTrailers => {
            resp.push_str("Transfer-Encoding: chunked\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
            // Body as one chunk, then the 0-chunk FOLLOWED BY trailer header lines
            // (a legal chunked tail the decoder must treat as end-of-body + ignore).
            let bytes = canned.body.as_bytes();
            write_chunk(&mut stream, bytes);
            let _ = stream.write_all(b"0\r\n");
            let _ = stream.write_all(b"X-Checksum: deadbeef\r\n");
            let _ = stream.write_all(b"X-Trailer: present\r\n");
            let _ = stream.write_all(b"\r\n");
        }
        Framing::ChunkedOversize {
            chunk_bytes,
            chunk_count,
        } => {
            resp.push_str("Transfer-Encoding: chunked\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
            // Stream `chunk_count` chunks of `chunk_bytes` filler each, NO
            // terminating 0-chunk: the reassembled body crosses the ceiling and
            // the decoder must bail mid-stream. A broken pipe once the client
            // gives up is expected (we ignore write errors).
            let filler = vec![b'x'; chunk_bytes];
            for _ in 0..chunk_count {
                let header = format!("{:x}\r\n", filler.len());
                if stream.write_all(header.as_bytes()).is_err() {
                    break;
                }
                if stream.write_all(&filler).is_err() {
                    break;
                }
                if stream.write_all(b"\r\n").is_err() {
                    break;
                }
            }
        }
        Framing::ContentLengthOversize { declared_len } => {
            // DECLARE an over-ceiling Content-Length; the decoder rejects on the
            // header alone, so we send only a token body (never the full size).
            resp.push_str(&format!("Content-Length: {declared_len}\r\n\r\n"));
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(b"{}");
        }
        Framing::ChunkedSlowDrip { chunk_count, gap } => {
            resp.push_str("Transfer-Encoding: chunked\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            // One small chunk per `gap`-spaced write, never terminating. Each
            // gap is under the per-read window but their sum blows the wall
            // clock. Stop on the first broken pipe (the client hit its deadline).
            for i in 0..chunk_count {
                thread::sleep(gap);
                let payload = format!("drip{i} ");
                let header = format!("{:x}\r\n", payload.len());
                if stream.write_all(header.as_bytes()).is_err() {
                    break;
                }
                if stream.write_all(payload.as_bytes()).is_err() {
                    break;
                }
                if stream.write_all(b"\r\n").is_err() {
                    break;
                }
                if stream.flush().is_err() {
                    break;
                }
            }
        }
    }
    let _ = stream.flush();
}

fn write_chunk(stream: &mut TcpStream, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let header = format!("{:x}\r\n", data.len());
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(data);
    let _ = stream.write_all(b"\r\n");
}

// --- Rows ---

#[test]
fn send_message_happy_round_trip() {
    let relay = FakeRelay::start(vec![Canned::ok(r#"{"message_id":"msg-abc123"}"#)]);
    let client = CcRelay::new();
    let id = client
        .send_message(relay.port, "hello there", "test-session")
        .expect("send_message should succeed");
    assert_eq!(id, "msg-abc123");
}

#[test]
fn fetch_reply_delayed_but_within_budget() {
    // The server sleeps ~1.5s BEFORE replying. The fetch budget is 30s, so the
    // long-poll must HONOR the budget and succeed. Under a uniform short read
    // timeout this would abort with Timeout — the row would FAIL. That is the
    // point (W4).
    let relay = FakeRelay::start(vec![
        Canned::ok(r#"{"text":"the slow reply"}"#).with_delay(Duration::from_millis(1500))
    ]);
    let client = CcRelay::new();
    let start = Instant::now();
    let reply = client
        .fetch_reply(relay.port, "msg-1", 30_000)
        .expect("delayed reply within budget must succeed");
    let elapsed = start.elapsed();
    assert_eq!(reply.text.as_deref(), Some("the slow reply"));
    assert_eq!(reply.error, None);
    // It really did wait for the delayed reply (proves it didn't short-circuit).
    assert!(
        elapsed >= Duration::from_millis(1400),
        "fetch returned too fast ({elapsed:?}) — did it actually long-poll?"
    );
    // And it didn't burn the whole budget.
    assert!(
        elapsed < Duration::from_secs(10),
        "fetch took too long: {elapsed:?}"
    );
}

#[test]
fn fetch_reply_error_reply() {
    let relay = FakeRelay::start(vec![Canned::ok(r#"{"error":"session refused the task"}"#)]);
    let client = CcRelay::new();
    let reply = client
        .fetch_reply(relay.port, "msg-2", 5_000)
        .expect("an error-reply is a valid response, not a transport error");
    assert_eq!(reply.error.as_deref(), Some("session refused the task"));
    assert_eq!(reply.text, None);
}

#[test]
fn chunked_and_content_length_parse_identical() {
    let body = r#"{"text":"identical body via two framings"}"#;

    // Content-Length variant.
    let r1 = FakeRelay::start(vec![Canned::ok(body)]);
    let reply_cl = CcRelay::new()
        .fetch_reply(r1.port, "m", 5_000)
        .expect("content-length parse");
    drop(r1);

    // Chunked variant (split into two chunks by the harness).
    let r2 = FakeRelay::start(vec![Canned::ok(body).chunked()]);
    let reply_chunked = CcRelay::new()
        .fetch_reply(r2.port, "m", 5_000)
        .expect("chunked parse");
    drop(r2);

    assert_eq!(
        reply_cl, reply_chunked,
        "the same body must parse identically regardless of framing"
    );
    assert_eq!(
        reply_cl.text.as_deref(),
        Some("identical body via two framings")
    );
}

/// The content-length reference reply for the chunked-branch rows below.
fn reference_reply(port: u16) -> dispatch::relay::RelayReply {
    CcRelay::new()
        .fetch_reply(port, "m", 5_000)
        .expect("content-length reference parse")
}

// --- chunked-decoder branch coverage (in-phase red-team #2) ---
//
// The riskiest chunked-decode branches were untested: a chunk-size LINE split
// across two writes (Bun streaming), a `;ext` chunk-extension suffix, and trailers
// after the 0-chunk. Each must parse IDENTICAL to the content-length variant —
// otherwise the relay transport silently drops/corrupts a reply under realistic
// streaming. The decoder (`relay_http::read_chunked`/`read_line`) handles all
// three; these rows exercise those branches end-to-end over a real socket.

#[test]
fn chunked_split_size_line_parses_identical() {
    let body = r#"{"text":"reply whose chunk-size line is split mid-token"}"#;

    let r_ref = FakeRelay::start(vec![Canned::ok(body)]);
    let reply_cl = reference_reply(r_ref.port);
    drop(r_ref);

    // The chunk-size line arrives in two writes (flush+sleep between) — read_line
    // must refill mid-line and still parse the hex size.
    let r = FakeRelay::start(vec![Canned::ok(body).framed(Framing::ChunkedSplitSizeLine)]);
    let reply = CcRelay::new()
        .fetch_reply(r.port, "m", 5_000)
        .expect("split chunk-size line must parse");
    drop(r);

    assert_eq!(
        reply, reply_cl,
        "a chunk-size line split across two writes must parse identical to content-length"
    );
}

#[test]
fn chunked_with_extension_suffix_parses_identical() {
    let body = r#"{"text":"reply with a chunk-extension on the size line"}"#;

    let r_ref = FakeRelay::start(vec![Canned::ok(body)]);
    let reply_cl = reference_reply(r_ref.port);
    drop(r_ref);

    let r = FakeRelay::start(vec![Canned::ok(body).framed(Framing::ChunkedExt)]);
    let reply = CcRelay::new()
        .fetch_reply(r.port, "m", 5_000)
        .expect("chunk-extension size line must parse");
    drop(r);

    assert_eq!(
        reply, reply_cl,
        "a `;ext` chunk-extension suffix must be ignored — parse identical to content-length"
    );
}

#[test]
fn chunked_with_trailers_parses_identical() {
    let body = r#"{"text":"reply followed by trailer headers"}"#;

    let r_ref = FakeRelay::start(vec![Canned::ok(body)]);
    let reply_cl = reference_reply(r_ref.port);
    drop(r_ref);

    let r = FakeRelay::start(vec![Canned::ok(body).framed(Framing::ChunkedTrailers)]);
    let reply = CcRelay::new()
        .fetch_reply(r.port, "m", 5_000)
        .expect("trailers after 0-chunk must parse");
    drop(r);

    assert_eq!(
        reply, reply_cl,
        "trailer headers after the 0-chunk must be ignored — parse identical to content-length"
    );
}

#[test]
fn health_slow_headers_within_short_timeout() {
    // Headers arrive after a modest delay (300ms), well under the SHORT health
    // timeout — health must still succeed.
    let relay = FakeRelay::start(vec![Canned::ok(
        r#"{"sessionId":"sess-h","port":8901,"pid":4242,"status":"ok"}"#,
    )
    .with_delay(Duration::from_millis(300))]);
    let client = CcRelay::new();
    let health = client
        .health(relay.port, 4_000)
        .expect("slow-but-bounded headers must succeed");
    assert_eq!(health.session_id, "sess-h");
    assert_eq!(health.port, 8901);
    assert_eq!(health.pid, 4242);
    assert_eq!(health.status, "ok");
}

#[test]
fn health_down_is_bounded_connection_failed_no_hang() {
    // Bind a port, then DROP the listener so nothing is listening — the OS will
    // refuse connections on that (now-free) port. This is the "sidecar present,
    // nothing listening" degraded case.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };

    let client = CcRelay::new();
    let start = Instant::now();
    let result = client.health(port, 1_000);
    let elapsed = start.elapsed();

    // The ERROR CLASS must be ConnectionFailed (a clean degraded signal), not a
    // timeout or a panic.
    match result {
        Err(RelayError::ConnectionFailed) => {}
        other => panic!("expected ConnectionFailed for a dead port, got {other:?}"),
    }
    // BOUNDED TIME: a refused connection returns fast. Even allowing for the
    // connect-timeout ceiling (5s) plus slack, it must never approach a hang.
    assert!(
        elapsed < Duration::from_secs(7),
        "health on a dead port took {elapsed:?} — must be bounded, never hang"
    );
}

#[test]
fn fetch_reply_dead_port_is_connection_failed_bounded() {
    // Same no-hang guarantee on the LONG-POLL path: a dead port must NOT consume
    // the (large) fetch budget waiting — the connect timeout bounds it.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let client = CcRelay::new();
    let start = Instant::now();
    // A 120s budget: if the dead port were honored as the read timeout we'd hang
    // ~120s. The connect timeout must short-circuit it.
    let result = client.fetch_reply(port, "msg", 120_000);
    let elapsed = start.elapsed();
    assert!(
        matches!(result, Err(RelayError::ConnectionFailed)),
        "dead port on fetch_reply must be ConnectionFailed, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(7),
        "fetch_reply on a dead port took {elapsed:?} — the connect timeout must bound it, \
         never the 120s read budget"
    );
}

#[test]
fn server_error_status_maps_to_server_error() {
    let relay = FakeRelay::start(vec![Canned {
        status: 503,
        body: r#"{"error":"overloaded"}"#.to_string(),
        framing: Framing::ContentLength,
        delay: Duration::ZERO,
    }]);
    let client = CcRelay::new();
    let result = client.send_message(relay.port, "hi", "cli");
    match result {
        Err(RelayError::ServerError(s)) => assert!(s.contains("503"), "status carried: {s}"),
        other => panic!("expected ServerError, got {other:?}"),
    }
}

// --- body-cap + wall-clock (spec W2) ---
//
// Three teeth for the W2 hardening: an oversized CHUNKED body and an oversized
// CONTENT-LENGTH body must each be rejected as `BadResponse` (the 1 MiB
// hostile-counterpart belt), and a slow-DRIP chunked response whose individual
// reads each beat the per-read window but whose TOTAL exceeds the wall clock
// must surface `Timeout` — proving the deadline is threaded INTO the read loop,
// not just set per-stream (red-team M3).

#[test]
fn oversized_chunked_body_is_bad_response() {
    // ~2 MiB streamed as 32 chunks × 64 KiB, no terminating 0-chunk: over the
    // 1 MiB ceiling. The decoder must bail with BadResponse mid-stream.
    let relay = FakeRelay::start(vec![Canned::ok(r#"{"text":"ignored"}"#).framed(
        Framing::ChunkedOversize {
            chunk_bytes: 64 * 1024,
            chunk_count: 32,
        },
    )]);
    let client = CcRelay::new();
    let result = client.fetch_reply(relay.port, "m", 30_000);
    assert!(
        matches!(result, Err(RelayError::BadResponse)),
        "oversized chunked body must be BadResponse, got {result:?}"
    );
}

#[test]
fn oversized_content_length_is_bad_response() {
    // A declared Content-Length of 8 MiB — over the ceiling. The decoder must
    // reject on the header alone, never reading the (un-sent) body.
    let relay = FakeRelay::start(vec![Canned::ok(r#"{"text":"ignored"}"#).framed(
        Framing::ContentLengthOversize {
            declared_len: 8 * 1024 * 1024,
        },
    )]);
    let client = CcRelay::new();
    let result = client.fetch_reply(relay.port, "m", 30_000);
    assert!(
        matches!(result, Err(RelayError::BadResponse)),
        "oversized content-length must be BadResponse, got {result:?}"
    );
}

#[test]
fn slow_drip_exceeding_wall_clock_is_timeout() {
    // Each chunk arrives after a 150ms gap — far under any per-read window — but
    // the stream never ends, so the cumulative wall clock crosses the caller's
    // 1s budget. A per-stream read timeout alone would NEVER fire (each read
    // beats its window); only the threaded wall-clock deadline catches this.
    let relay = FakeRelay::start(vec![Canned::ok(r#"{"text":"never finishes"}"#).framed(
        Framing::ChunkedSlowDrip {
            chunk_count: 1_000,
            gap: Duration::from_millis(150),
        },
    )]);
    let client = CcRelay::new();
    let start = Instant::now();
    // 1s wall-clock budget. The per-read window is the SAME 1s, but no single
    // 150ms gap trips it — only the total does.
    let result = client.fetch_reply(relay.port, "m", 1_000);
    let elapsed = start.elapsed();
    assert!(
        matches!(result, Err(RelayError::Timeout)),
        "slow-drip exceeding the wall clock must be Timeout, got {result:?}"
    );
    // It really did stop at the budget, not run the full drip (~150s) and not
    // short-circuit instantly (it waited out the ~1s budget).
    assert!(
        elapsed >= Duration::from_millis(900),
        "fetch returned too fast ({elapsed:?}) — wall clock not honored?"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "fetch took {elapsed:?} — the wall-clock deadline must bound total elapsed"
    );
}
