//! Socket republish fan-out (WP-B2b-2a, design §C layer-2): the reusable,
//! fully-decoupled machinery that fans one [`Republish`] fact out to N socket
//! subscribers WITHOUT ever blocking the headless reader/pump.
//!
//! Shape (design §C):
//! - [`SubscriberHub`] holds a `Vec` of BOUNDED `tokio::sync::mpsc::Sender<ServerMsg>`
//!   (one per subscriber, capacity [`SUBSCRIBER_CHANNEL_CAP`]). Interior mutability
//!   (`Mutex<Vec<..>>`) so `&self` `deliver` + `subscribe` are both fine across
//!   threads.
//! - [`SocketFanoutSink`] implements [`crate::headless::Sink`]. `deliver` maps a
//!   `Republish` → `ServerMsg::Republish*` and **`try_send`s (NON-blocking)** to
//!   each subscriber. On a full channel it COALESCES (drops the one frame for that
//!   one slow subscriber) for coalescible facts, but for a MUST-KEEP
//!   (`RepublishTurnEnd`/`RepublishEnd`) it disconnects the slow subscriber
//!   (best-effort `RepublishEnd{"lagged"}`, then drop its sender). It NEVER blocks.
//! - [`relay_subscriber`] drains a subscriber's `mpsc::Receiver<ServerMsg>` onto a
//!   `tokio` socket half as length-prefixed frames (modeled on
//!   `server::session_relay::screen_to_client`). Stops on `RepublishEnd` or
//!   rx-close. This is the body WP-B2b-2b's `SubscribeRepublish` relay will call.
//!
//! The contract that makes this load-bearing: a slow/never-drained subscriber B is
//! DISCONNECTED, it does NOT stall subscriber A or the `deliver` path (the §6.2
//! backpressure keystone — the socket analogue of
//! `headless::tests::closed_sink_never_stalls_reader`). The `try_send`+drop-slow is
//! proven necessary by the red-before `BlockingFanoutSink` reference, which WEDGES
//! when B stalls.

use crate::headless::{Republish, Sink};
use crate::protocol::messages::ServerMsg;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Per-subscriber channel capacity (design §C: ~256). Coalescible status beyond
/// this is dropped for the lagging subscriber; on a must-keep overflow the
/// subscriber is disconnected (it cannot keep up with terminal facts).
pub const SUBSCRIBER_CHANNEL_CAP: usize = 256;

/// Holds the live set of socket subscribers for one headless session. Cheap to
/// `Arc`-share between the (sync) sink/pump and the (async) relay tasks.
pub struct SubscriberHub {
    subscribers: Mutex<Vec<mpsc::Sender<ServerMsg>>>,
}

impl Default for SubscriberHub {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriberHub {
    /// A hub with no subscribers.
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Register a new subscriber, returning the `Receiver` half for its relay
    /// task (`relay_subscriber`). The matching `Sender` is retained by the hub and
    /// fed by [`SocketFanoutSink::deliver`].
    pub fn subscribe(&self) -> mpsc::Receiver<ServerMsg> {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_CHANNEL_CAP);
        self.subscribers
            .lock()
            .expect("subscriber hub mutex poisoned")
            .push(tx);
        rx
    }

    /// Current subscriber count (for tests / observability).
    pub fn subscriber_count(&self) -> usize {
        self.subscribers
            .lock()
            .expect("subscriber hub mutex poisoned")
            .len()
    }

    /// Broadcast one already-mapped [`ServerMsg`] to every subscriber, NON-blocking.
    /// This is the load-bearing fan-out: `try_send` only.
    ///
    /// - `must_keep`: when the channel is `Full`, a must-keep frame disconnects the
    ///   slow subscriber (best-effort `RepublishEnd{"lagged"}`, then drop its
    ///   sender) — a lagging subscriber must NOT cause a terminal/turn-end fact to
    ///   be silently lost; we tell it it lagged and cut it loose. A coalescible
    ///   frame just drops for that one subscriber.
    /// - A closed receiver (`SendError`) prunes that sender.
    ///
    /// NEVER blocks. NEVER awaits. A stalled subscriber B cannot stall A or this
    /// call.
    fn broadcast(&self, msg: ServerMsg, must_keep: bool) {
        let mut subs = self
            .subscribers
            .lock()
            .expect("subscriber hub mutex poisoned");
        // Retain only the subscribers that are still healthy after this send.
        subs.retain(|tx| match tx.try_send(msg.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                if must_keep {
                    // The slow one cannot keep up with a terminal fact: tell it it
                    // lagged (best-effort — its channel is full, so this may itself
                    // fail) and disconnect it. Dropping the sender closes its rx →
                    // its relay task ends.
                    let _ = tx.try_send(ServerMsg::RepublishEnd {
                        outcome: "lagged".into(),
                    });
                    false
                } else {
                    // Coalescible: drop just this frame for this subscriber, keep it.
                    true
                }
            }
            // Receiver gone (relay task ended / socket closed): prune.
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }
}

/// A [`Sink`] that republishes [`Republish`] facts to the socket subscribers of a
/// shared [`SubscriberHub`]. Cheap to clone-share (it is just an `Arc`).
pub struct SocketFanoutSink {
    hub: Arc<SubscriberHub>,
}

impl SocketFanoutSink {
    /// Wrap a shared hub.
    pub fn new(hub: Arc<SubscriberHub>) -> Self {
        Self { hub }
    }
}

impl Sink for SocketFanoutSink {
    fn deliver(&self, msg: Republish) {
        match msg {
            Republish::Ready { session_id } => {
                self.hub
                    .broadcast(ServerMsg::RepublishReady { session_id }, false);
            }
            Republish::TurnEnd(r) => {
                // MUST-KEEP: the turn-end signal is never coalesced away.
                self.hub.broadcast(
                    ServerMsg::RepublishTurnEnd {
                        session_id: r.session_id,
                        is_error: r.is_error,
                        stop_reason: r.stop_reason,
                    },
                    true,
                );
            }
            Republish::Breaker { .. } => {
                // MUST-KEEP terminal.
                self.hub.broadcast(
                    ServerMsg::RepublishEnd {
                        outcome: "breaker".into(),
                    },
                    true,
                );
            }
            Republish::Eof(outcome) => {
                // MUST-KEEP terminal. Debug-string the outcome (Completed/Aborted/
                // NoTurn) — the relay's stop signal.
                self.hub.broadcast(
                    ServerMsg::RepublishEnd {
                        outcome: format!("{outcome:?}"),
                    },
                    true,
                );
            }
            // Coalescible status passthrough: skip raw assistant/system/etc. events
            // to avoid amplification (matching the daemon-status sink's choice —
            // design §C / spec §3). The liveness facts (Ready/TurnEnd/End) carry the
            // control signal; raw `Event`s are not republished on the socket.
            Republish::Event(_ev) => {}
        }
    }
}

/// Drain a subscriber's `mpsc::Receiver<ServerMsg>` onto a `tokio` socket write
/// half as length-prefixed [`crate::protocol::encode`]d frames. Modeled on
/// `server::session_relay::screen_to_client`. Returns when:
/// - a `RepublishEnd` frame has been written (terminal — the relay's stop signal), or
/// - the rx is closed (all senders dropped, e.g. the session ended / subscriber
///   pruned), or
/// - the socket write errors (the client went away).
///
/// Exported `pub` so WP-B2b-2b's `SubscribeRepublish` dispatch arm can spawn it as
/// the long-lived relay task.
pub async fn relay_subscriber<W>(
    mut rx: mpsc::Receiver<ServerMsg>,
    mut stream: W,
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    while let Some(msg) = rx.recv().await {
        let terminal = matches!(msg, ServerMsg::RepublishEnd { .. });
        let frame = crate::protocol::encode(&msg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        stream.write_all(&frame).await?;
        stream.flush().await?;
        if terminal {
            break;
        }
    }
    Ok(())
}

// ===========================================================================
// §6.2 DoD tests (tokio; UnixStream pairs + synthetic Republish + FrameReader).
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{encode, FrameReader};
    use crate::stream_json::{ResultEvent, TurnOutcome, Usage};
    use std::time::{Duration, Instant};
    use tokio::net::UnixStream;

    fn result_event(sid: &str) -> ResultEvent {
        ResultEvent {
            session_id: sid.into(),
            is_error: false,
            stop_reason: Some("end_turn".into()),
            usage: Usage::default(),
            total_cost_usd: None,
        }
    }

    /// Read exactly `n` `ServerMsg` frames off a `UnixStream` read half (bounded by
    /// a deadline so a test can never hang the suite).
    async fn read_n(stream: &mut UnixStream, n: usize) -> Vec<ServerMsg> {
        let mut frames = FrameReader::new();
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while out.len() < n {
            if Instant::now() > deadline {
                panic!("timed out waiting for {n} frames (got {})", out.len());
            }
            tokio::select! {
                r = frames.fill_from(stream) => {
                    if !r.expect("read frames") {
                        break; // EOF
                    }
                    while let Some(m) = frames.decode_next::<ServerMsg>().expect("decode") {
                        out.push(m);
                        if out.len() == n {
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        out
    }

    // -- 1. delivery order (green-after) ------------------------------------

    /// hub + 1 subscriber; spawn `relay_subscriber`; deliver Ready, TurnEnd, then
    /// Eof(Completed) through `SocketFanoutSink`; the client half decodes
    /// `RepublishReady`, `RepublishTurnEnd`, `RepublishEnd` in order.
    #[tokio::test]
    async fn delivery_order_green_after() {
        let hub = Arc::new(SubscriberHub::new());
        let (server_half, mut client_half) = UnixStream::pair().expect("socketpair");
        let rx = hub.subscribe();
        let relay = tokio::spawn(relay_subscriber(rx, server_half));

        let sink = SocketFanoutSink::new(hub.clone());
        sink.deliver(Republish::Ready {
            session_id: "s1".into(),
        });
        sink.deliver(Republish::TurnEnd(result_event("s1")));
        sink.deliver(Republish::Eof(TurnOutcome::Completed));

        let got = read_n(&mut client_half, 3).await;
        assert!(
            matches!(got[0], ServerMsg::RepublishReady { ref session_id } if session_id == "s1"),
            "first frame Ready, got {:?}",
            got[0]
        );
        assert!(
            matches!(got[1], ServerMsg::RepublishTurnEnd { ref session_id, is_error: false, .. } if session_id == "s1"),
            "second frame TurnEnd, got {:?}",
            got[1]
        );
        assert!(
            matches!(got[2], ServerMsg::RepublishEnd { ref outcome } if outcome == "Completed"),
            "third frame End(Completed), got {:?}",
            got[2]
        );
        relay.await.expect("relay join").expect("relay ok");
    }

    // -- 2. concurrency k>=2, no cross-talk ---------------------------------

    /// Two hubs (two sessions), 2 subscribers each; interleave deliveries with
    /// distinct session_ids; each subscriber sees only its session's facts.
    #[tokio::test]
    async fn concurrency_two_sessions_no_cross_talk() {
        let hub_a = Arc::new(SubscriberHub::new());
        let hub_b = Arc::new(SubscriberHub::new());
        let sink_a = SocketFanoutSink::new(hub_a.clone());
        let sink_b = SocketFanoutSink::new(hub_b.clone());

        // 2 subscribers per session.
        let (sa1, mut ca1) = UnixStream::pair().unwrap();
        let (sa2, mut ca2) = UnixStream::pair().unwrap();
        let (sb1, mut cb1) = UnixStream::pair().unwrap();
        let (sb2, mut cb2) = UnixStream::pair().unwrap();
        let r = vec![
            tokio::spawn(relay_subscriber(hub_a.subscribe(), sa1)),
            tokio::spawn(relay_subscriber(hub_a.subscribe(), sa2)),
            tokio::spawn(relay_subscriber(hub_b.subscribe(), sb1)),
            tokio::spawn(relay_subscriber(hub_b.subscribe(), sb2)),
        ];

        // Interleave deliveries across the two sessions.
        sink_a.deliver(Republish::Ready {
            session_id: "AAAA".into(),
        });
        sink_b.deliver(Republish::Ready {
            session_id: "BBBB".into(),
        });
        sink_a.deliver(Republish::TurnEnd(result_event("AAAA")));
        sink_b.deliver(Republish::TurnEnd(result_event("BBBB")));
        sink_a.deliver(Republish::Eof(TurnOutcome::Completed));
        sink_b.deliver(Republish::Eof(TurnOutcome::Completed));

        fn sid_of(m: &ServerMsg) -> Option<&str> {
            match m {
                ServerMsg::RepublishReady { session_id } => Some(session_id),
                ServerMsg::RepublishTurnEnd { session_id, .. } => Some(session_id),
                _ => None,
            }
        }
        for c in [&mut ca1, &mut ca2] {
            let got = read_n(c, 3).await;
            for m in &got {
                if let Some(s) = sid_of(m) {
                    assert_eq!(s, "AAAA", "session-A subscriber saw cross-talk: {m:?}");
                }
            }
        }
        for c in [&mut cb1, &mut cb2] {
            let got = read_n(c, 3).await;
            for m in &got {
                if let Some(s) = sid_of(m) {
                    assert_eq!(s, "BBBB", "session-B subscriber saw cross-talk: {m:?}");
                }
            }
        }
        for h in r {
            h.await.unwrap().unwrap();
        }
    }

    // -- 3. backpressure KEYSTONE (the load-bearing DoD) --------------------

    /// Subscriber A drained normally; subscriber B's rx NEVER polled (its
    /// 256-slot channel fills). Flood `deliver` with many coalescible Ready + a
    /// final TurnEnd. Assert: (a) every `deliver` returns within a tight bound
    /// (< 50ms) — the sink never blocks on stalled B; (b) A receives the must-keep
    /// `RepublishTurnEnd`; (c) B is pruned (count drops). Socket analogue of
    /// `closed_sink_never_stalls_reader`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backpressure_keystone_slow_subscriber_never_stalls_a_or_sink() {
        const PER_DELIVER_BOUND: Duration = Duration::from_millis(50);

        let hub = Arc::new(SubscriberHub::new());
        let sink = SocketFanoutSink::new(hub.clone());

        // A: drained normally — a real relay task writes A's frames to the socket,
        // and a concurrent reader task drains the CLIENT side so A's pipeline keeps
        // flowing throughout the flood (A must never back up and lose its must-keep).
        let (server_a, mut client_a) = UnixStream::pair().unwrap();
        let relay_a = tokio::spawn(relay_subscriber(hub.subscribe(), server_a));
        let reader_a = tokio::spawn(async move {
            let mut frames = FrameReader::new();
            let mut saw_turn_end = false;
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && !saw_turn_end {
                if !frames.fill_from(&mut client_a).await.unwrap() {
                    break; // EOF
                }
                while let Some(m) = frames.decode_next::<ServerMsg>().unwrap() {
                    if let ServerMsg::RepublishTurnEnd { session_id, .. } = &m {
                        assert_eq!(session_id, "final");
                        saw_turn_end = true;
                        break;
                    }
                }
            }
            saw_turn_end
        });

        // B: subscribe but NEVER poll its rx — keep the Receiver alive so its
        // channel fills and stays full (a stalled slow subscriber).
        let _rx_b = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 2);

        // Flood far past B's 256-slot channel with coalescible Ready frames, each
        // timed: every deliver MUST return within the tight bound (proves the sink
        // never blocks on the stalled B). Yield periodically so A's relay/reader
        // tasks make progress (A drains; only B stays wedged).
        let flood = SUBSCRIBER_CHANNEL_CAP * 4;
        let mut worst = Duration::ZERO;
        for i in 0..flood {
            let t0 = Instant::now();
            sink.deliver(Republish::Ready {
                session_id: format!("s{i}"),
            });
            let dt = t0.elapsed();
            worst = worst.max(dt);
            assert!(
                dt < PER_DELIVER_BOUND,
                "deliver #{i} took {dt:?} (> {PER_DELIVER_BOUND:?}) — sink BLOCKED on stalled B"
            );
            if i % 32 == 0 {
                tokio::task::yield_now().await; // let A's relay/reader drain
            }
        }
        // Let A's pipeline fully settle so its 256-slot channel has drained (A is a
        // healthy subscriber — only B is wedged). This isolates the keystone claim:
        // the must-keep below reaches the NON-stalled A, while the never-drained B is
        // disconnected. (Coalescible Ready frames A couldn't hold were simply dropped
        // for A — that is the coalesce path, exercised above.)
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Final must-keep TurnEnd: also non-blocking, and it disconnects B.
        let t0 = Instant::now();
        sink.deliver(Republish::TurnEnd(result_event("final")));
        let dt = t0.elapsed();
        worst = worst.max(dt);
        assert!(
            dt < PER_DELIVER_BOUND,
            "must-keep TurnEnd deliver took {dt:?} — sink BLOCKED on stalled B"
        );
        println!(
            "BACKPRESSURE keystone: worst per-deliver = {worst:?} (bound {PER_DELIVER_BOUND:?}), \
             flood={flood} coalescible + 1 must-keep"
        );

        // (c) B was disconnected on the must-keep overflow → hub pruned it. A may or
        // may not still be present depending on its drain pace, but B must be gone:
        // the count is < 2 (B pruned) and A's must-keep still arrives (asserted next).
        assert!(
            hub.subscriber_count() < 2,
            "stalled subscriber B must be pruned on must-keep overflow (count {})",
            hub.subscriber_count()
        );

        // (b) A — drained normally — received the must-keep RepublishTurnEnd.
        let saw_turn_end = reader_a.await.expect("reader_a join");
        assert!(
            saw_turn_end,
            "subscriber A must receive the must-keep RepublishTurnEnd"
        );
        relay_a.abort();
    }

    // -- 4. red-before reference --------------------------------------------

    /// A `blocking_send().await` reference variant WEDGES when B's channel is full
    /// and never drained — proving `try_send`+drop-slow is load-bearing. We assert
    /// the blocking broadcast EXCEEDS a timeout (it cannot complete), whereas the
    /// real `SocketFanoutSink` (test #3) returns within 50ms.
    #[tokio::test]
    async fn blocking_fanout_red_reference_wedges_on_stalled_b() {
        // A blocking analogue of `broadcast`: awaits `send` on each subscriber.
        async fn blocking_broadcast(senders: &[mpsc::Sender<ServerMsg>], msg: ServerMsg) {
            for tx in senders {
                // The wrong design: this AWAITS until the channel has room — on a
                // never-drained full channel it never returns.
                let _ = tx.send(msg.clone()).await;
            }
        }

        // One stalled subscriber: full 256-slot channel, rx never polled.
        let (tx, _rx_never_polled) = mpsc::channel::<ServerMsg>(SUBSCRIBER_CHANNEL_CAP);
        // Fill it to capacity so the next send blocks.
        for i in 0..SUBSCRIBER_CHANNEL_CAP {
            tx.try_send(ServerMsg::RepublishReady {
                session_id: format!("fill{i}"),
            })
            .expect("prefill must fit");
        }
        let senders = vec![tx];

        // The blocking broadcast on a full channel must NOT complete within the
        // window (it is wedged). `timeout` returns Err on elapse.
        let res = tokio::time::timeout(
            Duration::from_millis(300),
            blocking_broadcast(
                &senders,
                ServerMsg::RepublishTurnEnd {
                    session_id: "x".into(),
                    is_error: false,
                    stop_reason: None,
                },
            ),
        )
        .await;
        assert!(
            res.is_err(),
            "RED-BEFORE: blocking_send().await reference must WEDGE (time out) on a \
             stalled subscriber — proving try_send+drop-slow is load-bearing"
        );
        println!("RED-BEFORE: blocking fan-out reference timed out at 300ms (wedged) as expected");
    }

    // -- 5. latency p50-p99 --------------------------------------------------

    /// Stamp `deliver` → subscriber-receive over N iters; assert p99 is small.
    #[tokio::test]
    async fn latency_deliver_to_receive_p50_p99() {
        let hub = Arc::new(SubscriberHub::new());
        let sink = SocketFanoutSink::new(hub.clone());
        let mut rx = hub.subscribe();

        let n = 500usize;
        let mut deltas: Vec<u128> = Vec::with_capacity(n);
        for i in 0..n {
            let t0 = Instant::now();
            sink.deliver(Republish::Ready {
                session_id: format!("s{i}"),
            });
            // The mapped frame lands in the in-memory mpsc immediately; receive it.
            let _m = rx.recv().await.expect("frame");
            deltas.push(t0.elapsed().as_micros());
        }
        deltas.sort_unstable();
        let pct = |p: f64| deltas[((deltas.len() as f64 * p) as usize).min(deltas.len() - 1)];
        let p50 = pct(0.50);
        let p99 = pct(0.99);
        println!("LATENCY deliver→receive (us): p50={p50} p99={p99} n={n}");
        assert!(
            p99 < 50_000,
            "p99 deliver→receive latency {p99}us unexpectedly high"
        );
    }

    // -- 6. protocol round-trip + FROZEN-INDEX regression -------------------

    /// Each new `ServerMsg::Republish*` round-trips through `encode`/`decode`; AND
    /// `ServerMsg::Error("x")` STILL decodes as `Error("x")` after the tail
    /// additions (guards the append-only discipline — a frozen-index regression).
    #[test]
    fn protocol_round_trip_and_frozen_error_index() {
        use crate::protocol::codec::{decode, decode_frame};

        let republish_msgs = vec![
            ServerMsg::RepublishReady {
                session_id: "s1".into(),
            },
            ServerMsg::RepublishTurnEnd {
                session_id: "s1".into(),
                is_error: true,
                stop_reason: Some("end_turn".into()),
            },
            ServerMsg::RepublishStatus {
                status: "idle".into(),
            },
            ServerMsg::RepublishEnd {
                outcome: "Completed".into(),
            },
        ];
        for msg in &republish_msgs {
            let encoded = encode(msg).unwrap();
            let (data, _) = decode_frame(&encoded).unwrap().unwrap();
            let decoded: ServerMsg = decode(data).unwrap();
            assert_eq!(&decoded, msg, "Republish* variant must round-trip");
        }

        // FROZEN-INDEX regression: Error("x") still decodes as Error("x") AFTER the
        // tail additions. This pins that we APPENDED (did not insert), keeping the
        // frozen `Error` index-4 contract intact.
        let err = ServerMsg::Error("x".into());
        let encoded = encode(&err).unwrap();
        let (data, _) = decode_frame(&encoded).unwrap().unwrap();
        let decoded: ServerMsg = decode(data).unwrap();
        match decoded {
            ServerMsg::Error(e) => {
                assert_eq!(e, "x", "frozen Error payload must survive tail additions")
            }
            other => panic!("Error must still decode as Error after tail additions, got {other:?}"),
        }

        // Also pin the hand-rolled index-4 wire (bincode fixint: u32 index, u64 len,
        // bytes) still decodes to Error after the additions.
        let mut wire = Vec::new();
        wire.extend_from_slice(&4u32.to_le_bytes());
        wire.extend_from_slice(&1u64.to_le_bytes());
        wire.extend_from_slice(b"x");
        let decoded: ServerMsg = decode(&wire).expect("frozen index-4 must decode");
        assert!(matches!(decoded, ServerMsg::Error(ref e) if e == "x"));
    }

    /// Sanity: `relay_subscriber` stops on `RepublishEnd` (terminal) even with more
    /// frames queued behind it — the `RepublishStatus` after the End is NOT written.
    #[tokio::test]
    async fn relay_stops_on_republish_end() {
        let (server_half, mut client_half) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel::<ServerMsg>(SUBSCRIBER_CHANNEL_CAP);
        tx.try_send(ServerMsg::RepublishReady {
            session_id: "s".into(),
        })
        .unwrap();
        tx.try_send(ServerMsg::RepublishEnd {
            outcome: "Completed".into(),
        })
        .unwrap();
        tx.try_send(ServerMsg::RepublishStatus {
            status: "after-end".into(),
        })
        .unwrap();
        drop(tx);
        let relay = tokio::spawn(relay_subscriber(rx, server_half));

        // Exactly two frames reach the client: Ready then End. The status after End
        // is dropped because the relay stopped.
        let got = read_n(&mut client_half, 2).await;
        assert!(
            matches!(got[0], ServerMsg::RepublishReady { .. }),
            "got {:?}",
            got[0]
        );
        assert!(
            matches!(got[1], ServerMsg::RepublishEnd { .. }),
            "got {:?}",
            got[1]
        );
        relay.await.unwrap().unwrap();

        // The relay has stopped; no third frame is ever delivered. Confirm the read
        // side sees EOF (server half dropped) rather than a third frame.
        let mut frames = FrameReader::new();
        let more = frames.fill_from(&mut client_half).await.unwrap();
        if more {
            let next = frames.decode_next::<ServerMsg>().unwrap();
            assert!(next.is_none(), "no frame after RepublishEnd, got {next:?}");
        }
    }
}
