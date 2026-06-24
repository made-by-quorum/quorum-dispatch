//! Codex P2 W2 — [`WsAppServer`] against an in-test scripted tungstenite SERVER
//! (codex-p2-spec sections 6.2, 8 W2, 10).
//!
//! OFFLINE: the "server" is a std thread binding `127.0.0.1:0` (an ephemeral OS
//! port) that we assert is NOT in 8900-9000 (sb's relay probe range — fleet
//! lesson; re-bind if it lands there). Readiness is handshaked over a channel
//! (the bound `SocketAddr` is sent once `accept` is ready) — NO sleeps-as-sync.
//! Each scenario's server runs a DETERMINISTIC script keyed on the inbound
//! request method, then exits; the client drives [`AppServerRpc`] against it.
//!
//! The frames the server emits are the WIRE LAW (no `jsonrpc` field), copied
//! from the q1c spike evidence shapes (exec/codex-spike-evidence/jail/).
//!
//! W3 mechanical edit (codex-p2-spec section 6.2): the [`AppServerRpc`] methods
//! moved from `&mut self` to `&self` (interior mutability in [`WsAppServer`]) so
//! the contract is consumable through a shared `&dyn AppServerRpc` out of
//! `ProviderFx::app_server`. The only change here is `let client` instead of
//! `let mut client` — the client bindings no longer need `mut`. The server-side
//! `let mut ws` stays (real tungstenite `WebSocket` is still `&mut self`).

use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use dispatch::provider::codex::{AppServerRpc, ClientInfo, RpcError, SteerOutcome, WsAppServer};
use serde_json::{json, Value};
use tungstenite::{accept, Message, WebSocket};

/// A server-side script: given an inbound request (`method`, `id`, `params`),
/// return the frames to write back (each a serde Value serialized as one text
/// frame). Returning an empty Vec writes nothing (e.g. for the `initialized`
/// notification, which carries no id and gets no reply).
type Script = Box<dyn Fn(&str, Option<u64>, &Value) -> Vec<Value> + Send>;

/// Bind an ephemeral 127.0.0.1 port OUTSIDE the relay-probe range (8900-9000),
/// re-binding if the OS hands us one inside it. Returns the listener + its url.
fn bind_outside_relay_range() -> (TcpListener, String) {
    // A handful of attempts is plenty — the OS rarely reuses a just-freed port.
    let mut held = Vec::new();
    let listener = loop {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = l.local_addr().unwrap().port();
        if !(8900..=9000).contains(&port) {
            break l;
        }
        // Hold the bad one so the next bind gets a different port, then drop all.
        held.push(l);
        assert!(
            held.len() < 64,
            "could not get a port outside 8900-9000 in 64 tries"
        );
    };
    drop(held);
    let port = listener.local_addr().unwrap().port();
    assert!(
        !(8900..=9000).contains(&port),
        "test server port {port} is in the forbidden relay range 8900-9000"
    );
    (listener, format!("ws://127.0.0.1:{port}"))
}

/// Spawn the scripted server on its own thread. Accepts ONE connection, then
/// serves frames: for every inbound message it runs `script` and writes the
/// returned frames. The server also writes `greeting` frames (server-initiated
/// notifications, e.g. remoteControl/status/changed) right after the ws
/// handshake, BEFORE reading anything — this models notifications that can
/// arrive before a response. Exits when the client closes or after `max_msgs`
/// inbound messages.
fn spawn_server(
    listener: TcpListener,
    greeting: Vec<Value>,
    script: Script,
    max_msgs: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut ws = accept(stream).expect("ws handshake");
        for g in &greeting {
            ws.send(Message::Text(serde_json::to_string(g).unwrap().into()))
                .expect("send greeting");
        }
        let mut seen = 0usize;
        while seen < max_msgs {
            let msg = match ws.read() {
                Ok(m) => m,
                Err(_) => break,
            };
            let text = match msg {
                Message::Text(t) => t.as_str().to_owned(),
                Message::Close(_) => break,
                _ => continue,
            };
            seen += 1;
            let frame: Value = serde_json::from_str(&text).expect("client frame is JSON");
            let method = frame
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let id = frame.get("id").and_then(Value::as_u64);
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            for out in script(&method, id, &params) {
                ws.send(Message::Text(serde_json::to_string(&out).unwrap().into()))
                    .expect("send reply");
            }
        }
        let _ = ws.close(None);
    })
}

/// Connect a [`WsAppServer`] to the running server with a generous timeout.
fn connect(url: &str) -> WsAppServer {
    WsAppServer::connect(url, Duration::from_secs(5)).expect("client connect")
}

// === Correlation: a response with the matching id resolves the call ===

#[test]
fn initialize_correlates_response_by_id() {
    let (listener, url) = bind_outside_relay_range();
    let script: Script = Box::new(|method, id, _params| {
        if method == "initialize" {
            // q1c-clientA-events.jsonl line 1 result shape (no jsonrpc field).
            vec![json!({
                "id": id.unwrap(),
                "result": {
                    "userAgent": "scripted/0.134.0",
                    "codexHome": "/jail/codex-home",
                    "platformFamily": "unix",
                    "platformOs": "macos"
                }
            })]
        } else {
            vec![]
        }
    });
    let server = spawn_server(listener, vec![], script, 1);

    let client = connect(&url);
    let res = client
        .initialize(&ClientInfo {
            name: "sb-test".into(),
            title: None,
            version: "0".into(),
        })
        .expect("initialize ok");
    assert_eq!(res.codex_home, "/jail/codex-home");
    let _ = client.close();
    server.join().unwrap();
}

#[test]
fn thread_start_then_turn_start_return_ids() {
    let (listener, url) = bind_outside_relay_range();
    let script: Script = Box::new(|method, id, _params| match method {
        "thread/start" => vec![json!({
            "id": id.unwrap(),
            // q1c-clientA-events.jsonl line 3 (nested thread).
            "result": { "thread": {
                "id": "019e9f4b-adb9-7ec1-b4ed-08247847426a",
                "path": "/jail/codex-home/sessions/2026/06/06/rollout-x.jsonl"
            }}
        })],
        "turn/start" => vec![json!({
            "id": id.unwrap(),
            // q1c-clientA-events.jsonl line 5 (nested turn shape).
            "result": { "turn": { "id": "019e9f4b-ae50-7ee3-a7eb-80c366198453" }}
        })],
        _ => vec![],
    });
    let server = spawn_server(listener, vec![], script, 2);

    let client = connect(&url);
    let tid = client
        .thread_start("/jail/work", "never", "danger-full-access")
        .expect("thread_start ok");
    assert_eq!(tid, "019e9f4b-adb9-7ec1-b4ed-08247847426a");
    let turn = client.turn_start(&tid, "hello").expect("turn_start ok");
    assert_eq!(turn, "019e9f4b-ae50-7ee3-a7eb-80c366198453");
    let _ = client.close();
    server.join().unwrap();
}

#[test]
fn turn_start_accepts_flat_turnid_shape() {
    // q1c-clientB-events.jsonl line 11: the co-injection result is FLAT
    // `{"turnId":..}`, not nested `{"turn":{"id":..}}`. The driver normalizes.
    let (listener, url) = bind_outside_relay_range();
    let script: Script = Box::new(|method, id, _params| {
        if method == "turn/start" {
            vec![
                json!({ "id": id.unwrap(), "result": { "turnId": "019e9f4c-8e65-7fa3-ae9d-9978019abd17" }}),
            ]
        } else {
            vec![]
        }
    });
    let server = spawn_server(listener, vec![], script, 1);

    let client = connect(&url);
    let turn = client.turn_start("t", "go").expect("turn_start ok");
    assert_eq!(turn, "019e9f4c-8e65-7fa3-ae9d-9978019abd17");
    let _ = client.close();
    server.join().unwrap();
}

// === Notification buffering: notifications arriving BEFORE the response are
// queued and served in order by next_notification ===

#[test]
fn notifications_before_response_are_buffered_in_order() {
    let (listener, url) = bind_outside_relay_range();
    // The server greets with TWO notifications, then for thread/start it emits a
    // THIRD notification AHEAD of the response — all must be buffered FIFO and
    // the response must still correlate.
    let greeting = vec![
        // q1c line 2 shape.
        json!({"method":"remoteControl/status/changed","params":{"status":"disabled"}}),
        json!({"method":"warning","params":{"message":"first"}}),
    ];
    let script: Script = Box::new(|method, id, _params| {
        if method == "thread/start" {
            vec![
                // notification AHEAD of the response (q1c line 4 thread/started shape).
                json!({"method":"thread/started","params":{"thread":{"id":"T"}}}),
                json!({
                    "id": id.unwrap(),
                    "result": { "thread": { "id": "T", "path": "/r.jsonl" }}
                }),
            ]
        } else {
            vec![]
        }
    });
    let server = spawn_server(listener, greeting, script, 1);

    let client = connect(&url);
    let tid = client
        .thread_start("/w", "never", "read-only")
        .expect("thread_start ok despite interleaved notifications");
    assert_eq!(tid, "T");

    // The three notifications were buffered FIFO: two greetings + the one ahead
    // of the response, served in arrival order.
    let n1 = client
        .next_notification(Duration::from_secs(2))
        .unwrap()
        .expect("buffered #1");
    assert_eq!(n1.method, "remoteControl/status/changed");
    let n2 = client
        .next_notification(Duration::from_secs(2))
        .unwrap()
        .expect("buffered #2");
    assert_eq!(n2.method, "warning");
    let n3 = client
        .next_notification(Duration::from_secs(2))
        .unwrap()
        .expect("buffered #3");
    assert_eq!(n3.method, "thread/started");
    assert_eq!(n3.params["thread"]["id"], json!("T"));

    let _ = client.close();
    server.join().unwrap();
}

// === Typed stale-steer error surfaces from turn_steer ===

#[test]
fn turn_steer_stale_id_surfaces_typed_stale_turn() {
    let (listener, url) = bind_outside_relay_range();
    let script: Script = Box::new(|method, id, _params| {
        if method == "turn/steer" {
            // q1c-clientB-events.jsonl line 12 — the structured stale-steer ERROR.
            vec![json!({
                "error": {
                    "code": -32600,
                    "message": "expected active turn id `019e0000-0000-7000-8000-000000000000` but found `019e9f4c-8e65-7fa3-ae9d-9978019abd17`"
                },
                "id": id.unwrap()
            })]
        } else {
            vec![]
        }
    });
    let server = spawn_server(listener, vec![], script, 1);

    let client = connect(&url);
    let outcome = client
        .turn_steer("t", "019e0000-0000-7000-8000-000000000000", "steer text")
        .expect("steer returns Ok(StaleTurn), not Err");
    match outcome {
        SteerOutcome::StaleTurn(e) => {
            assert_eq!(e.code, -32600);
            assert!(e.message.contains("expected active turn id"));
        }
        SteerOutcome::Steered(id) => panic!("expected StaleTurn, got Steered({id})"),
    }
    let _ = client.close();
    server.join().unwrap();
}

#[test]
fn turn_steer_match_returns_steered() {
    let (listener, url) = bind_outside_relay_range();
    let script: Script = Box::new(|method, id, _params| {
        if method == "turn/steer" {
            // TurnSteerResponse schema shape: {"turnId":..}.
            vec![json!({ "id": id.unwrap(), "result": { "turnId": "TURN-1" }})]
        } else {
            vec![]
        }
    });
    let server = spawn_server(listener, vec![], script, 1);

    let client = connect(&url);
    let outcome = client.turn_steer("t", "TURN-1", "more").expect("steer ok");
    assert_eq!(outcome, SteerOutcome::Steered("TURN-1".to_string()));
    let _ = client.close();
    server.join().unwrap();
}

// === Clean close ===

#[test]
fn close_then_next_notification_reports_closed() {
    let (listener, url) = bind_outside_relay_range();
    // Server replies to initialize, then the next read it does will be the
    // client's Close — it exits. The client, after a clean close on its side,
    // calling next_notification observes Closed (the socket is gone).
    let script: Script = Box::new(|method, id, _params| {
        if method == "initialize" {
            vec![json!({"id": id.unwrap(), "result": {"codexHome":"/h","userAgent":"u"}})]
        } else {
            vec![]
        }
    });
    let server = spawn_server(listener, vec![], script, 1);

    let client = connect(&url);
    client
        .initialize(&ClientInfo {
            name: "c".into(),
            title: None,
            version: "0".into(),
        })
        .expect("init ok");
    // Clean close from the client side.
    client.close().expect("clean close");
    server.join().unwrap();
}

// === Read-timeout is honored: a quiet socket yields Ok(None), and a request
// with no response within the request timeout yields Timeout ===

#[test]
fn next_notification_quiet_socket_returns_none() {
    let (listener, url) = bind_outside_relay_range();
    // Server accepts, sends nothing, and just holds the connection open by
    // blocking on a read that never completes (max_msgs=1 but client never
    // sends a request, so read blocks; the client side times out cleanly).
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut ws: WebSocket<TcpStream> = accept(stream).expect("handshake");
        // Block until the client closes; we never emit a notification.
        loop {
            match ws.read() {
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    let client = WsAppServer::connect(&url, Duration::from_millis(150)).expect("connect");
    let started = std::time::Instant::now();
    let got = client
        .next_notification(Duration::from_millis(400))
        .expect("quiet socket is not an error");
    assert!(got.is_none(), "no notification was sent → Ok(None)");
    // It honored the caller deadline (did not block forever) and returned in a
    // bounded window.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "next_notification must honor its deadline"
    );
    client.close().expect("close");
    // Use a readiness handshake instead of sleeping: closing the client ends the
    // server's read loop.
    let _ = server.join();
}

#[test]
fn request_with_no_response_times_out() {
    let (listener, url) = bind_outside_relay_range();
    // Server accepts and reads the request but NEVER replies.
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut ws: WebSocket<TcpStream> = accept(stream).expect("handshake");
        loop {
            match ws.read() {
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {} // read the request, send nothing
            }
        }
    });

    // Short request timeout so the test is fast.
    let client = WsAppServer::connect(&url, Duration::from_millis(120)).expect("connect");
    client.set_request_timeout(Duration::from_millis(300));
    let err = client
        .initialize(&ClientInfo {
            name: "c".into(),
            title: None,
            version: "0".into(),
        })
        .expect_err("no response → error");
    assert!(
        matches!(err, RpcError::Timeout),
        "expected Timeout, got {err:?}"
    );
    let _ = client.close();
    let _ = server.join();
}

// === Lead-review regression: a response slower than the 200ms poll interval
// must still arrive (read_response originally returned a false Timeout on the
// first quiet poll wakeup — the request deadline was unreachable). The server
// sleeps 600ms (3x the poll granularity) before answering; the default request
// deadline (30s) governs. MUTATION EVIDENCE: reverting the continue-on-
// WouldBlock fix in ws.rs read_response reds this test.

#[test]
fn response_slower_than_poll_interval_still_arrives() {
    let (listener, url) = bind_outside_relay_range();
    let script: Script = Box::new(|method, id, _params| {
        if method == "initialize" {
            std::thread::sleep(Duration::from_millis(600));
            vec![json!({
                "id": id.unwrap(),
                "result": { "userAgent": "slow/0.134.0", "codexHome": "/jail/codex-home" }
            })]
        } else {
            vec![]
        }
    });
    let server = spawn_server(listener, vec![], script, 1);
    let client = connect(&url);
    let result = client
        .initialize(&ClientInfo {
            name: "slowpoke".into(),
            title: None,
            version: "0".into(),
        })
        .expect("a 600ms response inside a 30s deadline must arrive, not Timeout");
    assert_eq!(result.codex_home, "/jail/codex-home");
    let _ = client.close();
    let _ = server.join();
}
