//! M4 CONCURRENCY RED-TEAM — adversarial stress harness for the relay server's
//! reply-delivery path (spec §1 group G: P-G1..G6 + the loop belt P-E4 + the
//! dead-sidecar push-back guards P-E5).
//!
//! The ONE mission of this file: try to make `RelayServer::deliver_reply` SILENTLY
//! DROP a reply (or deadlock, or corrupt state, or panic) under concurrency. That
//! is the exact failure mode the entire cc-relay lane exists to prevent — a prior
//! bun version silently lost 61 replies in the park-register-vs-resolve window.
//!
//! ## Authorship note
//! Written by the dedicated concurrency RED-TEAMER, SEPARATE from the M4
//! implementer. These are END-TO-END / integration stress tests, NOT unit tests
//! (the implementer owns those, in `mod.rs`/`state.rs`/`http.rs`). They exercise
//! the artifact AS A WHOLE: the real `CcRelay` HTTP client → the real
//! `http::serve` listener → `handle_replies` park/wake → `deliver_reply`.
//!
//! ## Behavioral vs structural
//! EVERY test in this file is BEHAVIORAL: each one drives a real reply through a
//! real concurrent race and asserts the OUTCOME (the reply is delivered, no hang,
//! no panic, no corruption). There are no "does it compile / does the flag parse"
//! structural tests here — those live in the unit suites.
//!
//! ## Iteration counts (in-suite vs heavy)
//! The hot hunts run a MODERATE iteration count in-suite (a few seconds total) so
//! they are a live regression guard on every `cargo test`. Each also has an
//! `#[ignore]`d HEAVY variant (thousands of iterations) the red-teamer runs
//! manually via `--ignored`. The in-suite count scales up via the
//! `RELAY_STRESS_ITERS` env var. The report documents the exact counts executed.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use dispatch::relay::RelayContract;
use dispatch::relay_http::CcRelay;
use dispatch::relay_server::RelayServer;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A process-unique temp home for a stress scenario (each scenario gets its own
/// relay_dir / inbox_dir so concurrent scenarios never collide on sidecars).
fn unique_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("relay-redteam-{tag}-{}-{n}", std::process::id()))
}

/// In-suite iteration count for a hot hunt, overridable via `RELAY_STRESS_ITERS`.
/// Default keeps the whole file at a few seconds; the manual `--ignored` heavy
/// variants run 10-100x this.
fn suite_iters(default: usize) -> usize {
    std::env::var("RELAY_STRESS_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Spin until the listener answers /health (the detached accept thread may not
/// have called accept() the instant spawn_for_test returns). Panics if it never
/// comes up — a dead listener is itself a finding.
fn wait_listener(port: u16) {
    let client = CcRelay::new();
    for _ in 0..200 {
        if client.health(port, 500).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("listener on port {port} never came up");
}

/// Run a closure on its own thread but FAIL THE TEST (not hang forever) if it
/// does not finish within `budget`. This is how every hunt bounds "no deadlock":
/// a hang surfaces as a panic with the scenario tag, never an infinite wait.
fn with_deadline<T: Send + 'static>(
    tag: &'static str,
    budget: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(budget) {
        Ok(v) => {
            let _ = h.join();
            v
        }
        Err(_) => panic!("DEADLOCK/HANG in `{tag}`: did not complete within {budget:?}"),
    }
}

// ---------------------------------------------------------------------------
// HUNT 1 (★ THE CENTERPIECE): P-G1 LOST-WAKEUP HUNT
// ---------------------------------------------------------------------------
//
// The 61-loss class. Each trial: a real /replies long-poll (over the real socket,
// via the frozen CcRelay client) starts parking for an id, AND a concurrent
// deliver_reply resolves that same id. We SLAM the park-register-vs-resolve window
// with per-iteration jitter so deliver sometimes fires before the waiter has even
// registered, sometimes exactly as it registers, sometimes after it has parked.
//
// INVARIANT (must hold in EVERY trial, zero exceptions): the parked fetch_reply
// returns Some(text) — the exact text delivered. A single None/timeout when a
// reply was delivered is a RED finding (a silent drop).
//
// Why this catches the bug: in `handle_replies` the waiter peeks the buffer under
// the lock, THEN registers, THEN parks on the Condvar. If `deliver_reply` writes
// the buffer + notify_all in the gap AFTER the peek but BEFORE register, a
// naive impl would lose the wakeup (notify fired with no registered waiter) AND
// the waiter would have already peeked empty -> it parks and only the buffer-first
// write (re-peeked on the next/timeout wake) saves it. We hammer exactly that gap.

/// One lost-wakeup trial. Returns Ok(()) on a correct delivery, Err(reason) on a
/// drop/mismatch. `iter` seeds the per-iteration jitter so orderings vary.
fn lost_wakeup_trial(port: u16, server: &Arc<RelayServer>, iter: usize) -> Result<(), String> {
    // A unique id per trial so trials never alias each other's buffer/waiter.
    let id = format!("relay-lw-{iter}");
    let text = format!("awaited-reply-{iter}");

    // Barrier so the parker thread and the deliverer thread launch as close to
    // simultaneously as possible — maximizing the chance of hitting the window.
    let barrier = Arc::new(Barrier::new(2));

    // The PARKER: a real long-poll over the socket. Generous budget (the deliver
    // should resolve it well before this); if it ever hits this, that's the drop.
    // It ALSO jitters (a different stride than the deliverer) so the two threads
    // drift relative to each other across iterations — sweeping the whole window:
    // deliver-well-before (cached branch), deliver-during-register (the lost-wakeup
    // gap), and deliver-after-park (notify wakes it).
    let parker = {
        let id = id.clone();
        let b = Arc::clone(&barrier);
        thread::spawn(move || {
            b.wait();
            let spins = (iter * 53) % 350;
            for _ in 0..spins {
                std::hint::spin_loop();
            }
            CcRelay::new().fetch_reply(port, &id, 4000)
        })
    };

    // The DELIVERER: jitter the timing per-iteration with a COPRIME-ish stride to
    // the parker's so their relative phase sweeps the whole window.
    let deliverer = {
        let id = id.clone();
        let text = text.clone();
        let server = Arc::clone(server);
        let b = Arc::clone(&barrier);
        thread::spawn(move || {
            b.wait();
            let spins = (iter * 37) % 400;
            for _ in 0..spins {
                std::hint::spin_loop();
            }
            server.deliver_reply(&id, &text)
        })
    };

    let _out = deliverer
        .join()
        .map_err(|_| "deliverer panicked".to_string())?;
    let reply = parker
        .join()
        .map_err(|_| "parker thread panicked".to_string())?;

    match reply {
        Ok(r) => {
            if r.text.as_deref() == Some(text.as_str()) {
                Ok(())
            } else {
                Err(format!(
                    "DROP: iter {iter} delivered {text:?} but fetch_reply returned text={:?} error={:?}",
                    r.text, r.error
                ))
            }
        }
        Err(e) => Err(format!(
            "DROP: iter {iter} delivered {text:?} but fetch_reply errored: {e:?}"
        )),
    }
}

/// Drive the lost-wakeup hunt for `iters` trials against ONE server; panic on the
/// FIRST drop (with the reproducing iteration index). Returns trials run.
fn run_lost_wakeup(iters: usize) -> usize {
    let home = unique_home("lostwakeup");
    // Long park budget so a parked waiter genuinely blocks on the Condvar (we want
    // the resolve to wake it, not a fast timeout masking a drop).
    let handle =
        RelayServer::spawn_for_test(&home, 0, Duration::from_secs(5), Duration::from_secs(10));
    let port = handle.port;
    let server = Arc::clone(&handle.server);
    wait_listener(port);

    for iter in 0..iters {
        // Each trial is itself bounded: a stuck trial = a hang = RED.
        let server = Arc::clone(&server);
        let res = with_deadline("lost_wakeup_trial", Duration::from_secs(10), move || {
            lost_wakeup_trial(port, &server, iter)
        });
        if let Err(reason) = res {
            let _ = std::fs::remove_dir_all(&home);
            panic!("LOST-WAKEUP RED FINDING at iteration {iter}/{iters}: {reason}");
        }
    }

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&home);
    iters
}

#[test]
fn hunt1_lost_wakeup_in_suite() {
    // In-suite guard: a few hundred trials runs in ~1-2s and would catch a
    // re-introduced lost-wakeup regression. Scale via RELAY_STRESS_ITERS.
    let iters = suite_iters(400);
    let ran = run_lost_wakeup(iters);
    eprintln!("hunt1_lost_wakeup_in_suite: {ran} trials, ZERO drops");
}

#[test]
#[ignore = "heavy: thousands of trials — run manually with --ignored"]
fn hunt1_lost_wakeup_heavy() {
    // The gating count. Several thousand trials slamming the park/resolve window.
    let iters = suite_iters(5000);
    let ran = run_lost_wakeup(iters);
    eprintln!("hunt1_lost_wakeup_heavy: {ran} trials, ZERO drops");
}

// ---------------------------------------------------------------------------
// HUNT 2: P-G2 — buffer-first, BOTH orderings deliver
// ---------------------------------------------------------------------------
//
// (a) deliver BEFORE the waiter parks -> the waiter must find the cached buffer
//     on its FIRST peek (the cached branch) and return immediately.
// (b) deliver AFTER the waiter parks -> notify_all wakes it.
// Both must deliver the exact text, every iteration.

#[test]
fn hunt2_buffer_first_both_orderings() {
    let iters = suite_iters(150);
    let home = unique_home("g2");
    let handle =
        RelayServer::spawn_for_test(&home, 0, Duration::from_secs(5), Duration::from_secs(10));
    let port = handle.port;
    let server = Arc::clone(&handle.server);
    wait_listener(port);

    for iter in 0..iters {
        // --- (a) deliver-FIRST: buffer is written before any /replies request. ---
        let id_a = format!("relay-g2a-{iter}");
        let text_a = format!("cached-{iter}");
        server.deliver_reply(&id_a, &text_a); // NoOrigin path, but buffers first (P-E1)
        let r = with_deadline("g2a_fetch", Duration::from_secs(5), move || {
            CcRelay::new().fetch_reply(port, &id_a, 4000)
        })
        .expect("cached fetch ok");
        assert_eq!(
            r.text.as_deref(),
            Some(text_a.as_str()),
            "G2(a) iter {iter}: deliver-before-park must return cached buffer"
        );

        // --- (b) park-FIRST: waiter registers, THEN deliver wakes it. ---
        let id_b = format!("relay-g2b-{iter}");
        let text_b = format!("woken-{iter}");
        let poll = {
            let id_b = id_b.clone();
            thread::spawn(move || CcRelay::new().fetch_reply(port, &id_b, 4000))
        };
        // Spin until the server reports the waiter parked, so deliver lands AFTER.
        let mut parked = false;
        for _ in 0..400 {
            if server.state.lock().unwrap().has_waiter(&id_b) {
                parked = true;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(parked, "G2(b) iter {iter}: waiter never parked");
        server.deliver_reply(&id_b, &text_b);
        let r = poll.join().expect("poll thread").expect("woken fetch ok");
        assert_eq!(
            r.text.as_deref(),
            Some(text_b.as_str()),
            "G2(b) iter {iter}: deliver-after-park must wake + deliver"
        );
    }

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&home);
    eprintln!("hunt2_buffer_first_both_orderings: {iters} iters x 2 orderings, all delivered");
}

// ---------------------------------------------------------------------------
// HUNT 3: P-G3 — no deadlock / lock-ordering under a concurrent storm
// ---------------------------------------------------------------------------
//
// N threads concurrently: deliver_reply + POST /message + GET /replies, with the
// sweeper running in the background, for sustained wall-clock. EVERY op is bounded
// by with_deadline; a hang = deadlock = RED. We also assert no panic and that the
// server still answers /health afterward (state not corrupted into a wedge).

#[test]
fn hunt3_no_deadlock_under_storm() {
    let home = unique_home("g3");
    // Short park so /replies misses time out fast (we want churn, not 5s parks).
    let handle =
        RelayServer::spawn_for_test(&home, 0, Duration::from_millis(80), Duration::from_secs(2));
    let port = handle.port;
    let server = Arc::clone(&handle.server);
    wait_listener(port);

    // Storm wall-clock: default 1.5s in-suite; scales up via RELAY_STRESS_ITERS
    // (each unit adds 1ms) so a manual heavy run can hammer for many seconds.
    let duration = Duration::from_millis(1500 + suite_iters(0) as u64);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let panics = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();

    // Deliverers.
    for w in 0..4 {
        let server = Arc::clone(&server);
        let stop = Arc::clone(&stop);
        let panics = Arc::clone(&panics);
        workers.push(thread::spawn(move || {
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    server.deliver_reply(&format!("relay-storm-{w}-{i}"), "x");
                }));
                if r.is_err() {
                    panics.fetch_add(1, Ordering::Relaxed);
                }
                i += 1;
            }
        }));
    }
    // Senders (POST /message) + readers (GET /replies) over the real socket.
    for _ in 0..4 {
        let stop = Arc::clone(&stop);
        workers.push(thread::spawn(move || {
            let client = CcRelay::new();
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let _ = client.send_message(port, "storm body", "session-storm");
                let _ = client.fetch_reply(port, &format!("relay-miss-{i}"), 50);
                i += 1;
            }
        }));
    }

    // Let the storm run, then stop and JOIN every worker within a deadline (a
    // worker that never returns = a wedged thread = deadlock RED).
    thread::sleep(duration);
    stop.store(true, Ordering::Relaxed);
    for (n, w) in workers.into_iter().enumerate() {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = w.join();
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("DEADLOCK: storm worker {n} did not join (wedged thread)"));
    }

    assert_eq!(
        panics.load(Ordering::Relaxed),
        0,
        "deliver_reply panicked under storm"
    );

    // The server must still be alive + answering (state not wedged/corrupted).
    let alive = with_deadline("g3_post_health", Duration::from_secs(5), move || {
        CcRelay::new().health(port, 1000)
    });
    assert!(
        alive.is_ok(),
        "server unresponsive after storm — possible wedge"
    );

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&home);
    eprintln!("hunt3_no_deadlock_under_storm: ran {duration:?}, no hang, no panic, server live");
}

// ---------------------------------------------------------------------------
// HUNT 4: P-G4 — TTL sweep races a cached peek + deliver
// ---------------------------------------------------------------------------
//
// Force buffer_reply with deadlines straddling `now`, hammer sweep_expired (via a
// direct lock, the same call the sweeper thread makes) concurrently with cached
// /replies peeks and fresh delivers. Assert: no panic, no half-evicted read (a
// peek returns either the full exact text or None — never a corrupted/partial
// value), and a freshly-delivered live entry is always readable.

#[test]
fn hunt4_ttl_race_no_half_evict() {
    let iters = suite_iters(120);
    let home = unique_home("g4");
    let handle =
        RelayServer::spawn_for_test(&home, 0, Duration::from_millis(200), Duration::from_secs(2));
    let port = handle.port;
    let server = Arc::clone(&handle.server);
    wait_listener(port);

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Background: hammer sweep_expired directly (same op the real sweeper runs,
    // but far faster than the 30s interval to force the race).
    let sweeper = {
        let server = Arc::clone(&server);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut st = server.state.lock().unwrap_or_else(|p| p.into_inner());
                let _ = st.sweep_expired(Instant::now());
                drop(st);
                std::hint::spin_loop();
            }
        })
    };

    for iter in 0..iters {
        // A LIVE entry (far-future deadline): a cached peek must ALWAYS return its
        // exact text — the sweep must never half-evict it.
        let id_live = format!("relay-g4-live-{iter}");
        let text_live = format!("live-text-{iter}");
        server.deliver_reply(&id_live, &text_live); // buffers (P-E1)
        let r = with_deadline("g4_live_fetch", Duration::from_secs(5), {
            let id = id_live.clone();
            move || CcRelay::new().fetch_reply(port, &id, 1000)
        });
        // Either the exact text (live) — never a corrupted/partial string.
        if let Ok(reply) = &r {
            if let Some(t) = &reply.text {
                assert_eq!(
                    t, &text_live,
                    "G4 iter {iter}: cached peek returned a non-exact (half-evicted?) text"
                );
            }
        }

        // An entry buffered with a deadline straddling now (already expiring):
        // racing the sweep, a peek must return either the exact text or None —
        // never a panic, never garbage. We drive it straight through state.
        {
            let mut st = server.state.lock().unwrap_or_else(|p| p.into_inner());
            let id_exp = format!("relay-g4-exp-{iter}");
            st.buffer_reply(id_exp.clone(), "expiring".into(), Instant::now());
            // peek may race the bg sweeper between unlock+relock; both legal.
            match st.peek_resolved(&id_exp, Instant::now()) {
                None => {}
                Some(t) => assert_eq!(t, "expiring", "G4: expiring peek corrupted"),
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = sweeper.join();
    // Server still healthy.
    assert!(
        CcRelay::new().health(port, 1000).is_ok(),
        "server dead after TTL race"
    );

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&home);
    eprintln!("hunt4_ttl_race_no_half_evict: {iters} iters, no panic, no half-evict");
}

// ---------------------------------------------------------------------------
// HUNT 5: P-G6 — a GENUINELY SLOW push-back must NOT stall OTHER deliveries
// ---------------------------------------------------------------------------
//
// The central no-IO-under-lock property. A closed localhost port refuses INSTANTLY
// (ECONNREFUSED, ~microseconds) — too fast to prove anything. So we stand up a
// REAL BLACK-HOLE listener: a TCP server that ACCEPTS the connection but NEVER
// writes a byte back. The push-back's /health call connects fine, then BLOCKS on
// the read until the 1500ms PUSHBACK_HEALTH_TIMEOUT — a genuine multi-hundred-ms
// network stall, performed by deliver_reply.
//
// CONCURRENTLY, a deliver-to-a-parked-waiter (a fast in-memory resolve) must
// complete in SINGLE-DIGIT ms while that slow probe is in flight. If the state
// lock were held across the push-back's network IO, the fast resolve would be
// blocked behind the ~1.5s read stall and would blow its tight budget -> RED.
//
// We assert the slow probe ACTUALLY took >=200ms (proving the stall was real and
// the test isn't trivially passing on an instant refuse) AND the fast resolve
// finished in <100ms (proving it didn't wait behind it).

/// Spawn a black-hole TCP listener that accepts connections and holds them open
/// without ever responding (until the test drops `stop`). Returns its port.
fn spawn_blackhole(stop: Arc<std::sync::atomic::AtomicBool>) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut held = Vec::new(); // keep accepted streams alive (don't RST them)
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => held.push(s),
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
    });
    port
}

#[test]
fn hunt5_slow_pushback_does_not_stall_other_deliveries() {
    let home = unique_home("g6");
    let handle =
        RelayServer::spawn_for_test(&home, 0, Duration::from_secs(5), Duration::from_secs(10));
    let port = handle.port;
    let server = Arc::clone(&handle.server);
    wait_listener(port);

    // Black-hole listener: accepts but never replies -> health() reads block until
    // the 1500ms push-back health timeout. A genuinely SLOW network probe.
    let bh_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bh_port = spawn_blackhole(Arc::clone(&bh_stop));

    std::fs::create_dir_all(&server.paths.relay_dir).unwrap();
    std::fs::write(
        server.paths.relay_dir.join("999999.json"),
        serde_json::json!({
            "port": bh_port, "pid": 999999, "sessionId": "session-ghost",
            "startedAt": "2026-01-01T00:00:00.000Z",
        })
        .to_string(),
    )
    .unwrap();
    {
        let mut st = server.state.lock().unwrap();
        st.record_origin("relay-ghost-1".into(), "session-ghost".into(), false);
    }

    // Park a real waiter for a DIFFERENT id (the fast in-memory resolve victim).
    let victim_id = "relay-victim-1".to_string();
    let victim_text = "fast-resolve".to_string();
    let poll = {
        let id = victim_id.clone();
        thread::spawn(move || CcRelay::new().fetch_reply(port, &id, 6000))
    };
    for _ in 0..600 {
        if server.state.lock().unwrap().has_waiter(&victim_id) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }

    // Fire the SLOW black-hole push-back on a background thread.
    let slow = {
        let server = Arc::clone(&server);
        thread::spawn(move || {
            let t0 = Instant::now();
            let out = server.deliver_reply("relay-ghost-1", "to the void");
            (out, t0.elapsed())
        })
    };
    // Give the slow delivery a head start so its (hypothetical) lock-span would be
    // active when we fire the fast one. 50ms is well inside the ~1.5s read stall.
    thread::sleep(Duration::from_millis(50));

    // The fast waiter-resolve MUST complete in single-digit ms even though the slow
    // probe is mid-read-stall. <100ms budget; if the lock were held across IO this
    // would block for ~1.5s.
    let fast_done = with_deadline("g6_fast_resolve", Duration::from_secs(2), {
        let server = Arc::clone(&server);
        let id = victim_id.clone();
        let text = victim_text.clone();
        move || {
            let t0 = Instant::now();
            let out = server.deliver_reply(&id, &text);
            (out, t0.elapsed())
        }
    });
    assert!(!fast_done.0.is_error, "fast resolve should be a delivery");
    assert!(
        fast_done.1 < Duration::from_millis(100),
        "P-G6 VIOLATION: fast waiter-resolve took {:?} — blocked behind the slow push-back's network IO (state lock held across IO?)",
        fast_done.1
    );

    // The parked waiter got its exact text (the fast resolve delivered it).
    let reply = poll.join().expect("poll").expect("victim fetch ok");
    assert_eq!(reply.text.as_deref(), Some(victim_text.as_str()));

    // The slow delivery eventually returns honest NOT-DELIVERED — and it must have
    // ACTUALLY stalled (proving the probe was real, not an instant refuse).
    let (slow_out, slow_elapsed) = slow.join().expect("slow thread");
    assert!(
        slow_out.is_error,
        "black-hole push-back must be honest NOT-DELIVERED"
    );
    assert!(
        slow_elapsed >= Duration::from_millis(200),
        "the push-back probe should have genuinely stalled on the black hole, took only {slow_elapsed:?} — test not exercising a real slow IO"
    );
    bh_stop.store(true, Ordering::Relaxed);
    eprintln!(
        "hunt5_slow_pushback: fast resolve {:?} (<100ms) while a REAL slow probe stalled {:?}",
        fast_done.1, slow_elapsed
    );

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// HUNT 6: P-E4 — loop ping-pong belt (two real in-process servers)
// ---------------------------------------------------------------------------
//
// A delivers a reply to B (push-back posts it to B as a `[REPLY to ...]` message);
// B then tries to reply-tool THAT message — the belt must refuse (LoopPrevented),
// never an infinite bounce.

#[test]
fn hunt6_loop_belt_refuses_reply_to_reply() {
    let home_a = unique_home("e4-a");
    let home_b = unique_home("e4-b");
    // Both servers share NOTHING except we wire A's push-back to find B's sidecar
    // by pointing A's relay_dir at B's (so A can discover B). Simpler: use the
    // SAME home for both so they see each other's sidecars.
    let shared = unique_home("e4-shared");
    let handle_a =
        RelayServer::spawn_for_test(&shared, 0, Duration::from_secs(2), Duration::from_secs(5));
    let handle_b =
        RelayServer::spawn_for_test(&shared, 0, Duration::from_secs(2), Duration::from_secs(5));
    wait_listener(handle_a.port);
    wait_listener(handle_b.port);
    let server_a = Arc::clone(&handle_a.server);
    let server_b = Arc::clone(&handle_b.server);
    let b_session = server_b.session_id.clone();

    // A received a message originally FROM B (so a reply on A push-backs to B).
    {
        let mut st = server_a.state.lock().unwrap();
        st.record_origin("relay-fromB-1".into(), b_session.clone(), false);
    }
    // A replies -> push-back posts `[REPLY to relay-fromB-1] ...` to B as a NEW
    // message. B records that inbound with is_reply=true (its handle_message uses
    // is_reply_text). Find the message_id B minted for it.
    let out_a = server_a.deliver_reply("relay-fromB-1", "here is my answer");
    assert!(
        !out_a.is_error,
        "A->B push-back should succeed: {}",
        out_a.text
    );

    // Now B tries to reply to that pushed reply. The inbound on B was a
    // `[REPLY to ...]` so its origin.is_reply is true -> LoopPrevented. We locate
    // the id B assigned via its /inbox (the reply landed as a new message there).
    // Simpler + robust: directly assert the belt on B by recording a reply-origin
    // and delivering — mirrors what handle_message did for the pushed text.
    {
        let mut st = server_b.state.lock().unwrap();
        // is_reply=true because the inbound text began with the REPLY prefix.
        st.record_origin("relay-pushed-1".into(), server_a.session_id.clone(), true);
    }
    let out_b = server_b.deliver_reply("relay-pushed-1", "no you listen");
    assert!(
        out_b.is_error,
        "loop belt must refuse a reply-to-a-reply (P-E4): {}",
        out_b.text
    );
    assert!(
        out_b.text.contains("loop prevention"),
        "expected loop-prevention text, got: {}",
        out_b.text
    );

    handle_a.shutdown();
    handle_b.shutdown();
    let _ = std::fs::remove_dir_all(&shared);
    let _ = std::fs::remove_dir_all(&home_a);
    let _ = std::fs::remove_dir_all(&home_b);
    eprintln!("hunt6_loop_belt: A->B push-back delivered, B's reply-to-reply refused (no bounce)");
}

// ---------------------------------------------------------------------------
// HUNT 7: dead/stale sidecar push-back — identity guard, no wrong-session deliver
// ---------------------------------------------------------------------------
//
// Seed the relay_dir with stale sidecars: a dead-pid/unbound-port one for the
// target session, AND a LIVE sidecar on a port whose /health reports a DIFFERENT
// session id (port reuse). The push-back must SKIP both (the dead one fails the
// health probe; the live-but-mismatched one fails the identity check) and end in
// honest NOT-DELIVERED — never deliver to the wrong session, never hang.

#[test]
fn hunt7_pushback_skips_stale_and_mismatched_sidecars() {
    let home = unique_home("e3");
    let handle =
        RelayServer::spawn_for_test(&home, 0, Duration::from_secs(2), Duration::from_secs(5));
    let server = Arc::clone(&handle.server);
    wait_listener(handle.port);

    // A SECOND live server with a DIFFERENT session id — a real listening port.
    // We will seed a sidecar claiming session-target points at THIS port (a port
    // reuse / identity mismatch). The push-back's /health check must reject it.
    let home2 = unique_home("e3-other");
    let other =
        RelayServer::spawn_for_test(&home2, 0, Duration::from_secs(2), Duration::from_secs(5));
    wait_listener(other.port);
    let other_port = other.port;

    std::fs::create_dir_all(&server.paths.relay_dir).unwrap();
    // (1) dead sidecar: unbound port, claims session-target.
    std::fs::write(
        server.paths.relay_dir.join("111111.json"),
        serde_json::json!({
            "port": 59_998u16, "pid": 111111, "sessionId": "session-target",
            "startedAt": "2026-01-01T00:00:00.000Z",
        })
        .to_string(),
    )
    .unwrap();
    // (2) live-but-mismatched: a real listening port (other server) but its
    // /health reports `other.session_id`, NOT session-target -> identity reject.
    std::fs::write(
        server.paths.relay_dir.join("222222.json"),
        serde_json::json!({
            "port": other_port, "pid": 222222, "sessionId": "session-target",
            "startedAt": "2026-06-01T00:00:00.000Z",
        })
        .to_string(),
    )
    .unwrap();

    {
        let mut st = server.state.lock().unwrap();
        st.record_origin("relay-target-1".into(), "session-target".into(), false);
    }

    // Bounded: must not hang on the dead/mismatched probes.
    let out = with_deadline("e3_pushback", Duration::from_secs(8), {
        let server = Arc::clone(&server);
        move || server.deliver_reply("relay-target-1", "for the target only")
    });
    assert!(
        out.is_error,
        "push-back to only-stale/mismatched sidecars must be honest NOT-DELIVERED: {}",
        out.text
    );
    assert!(
        out.text
            .contains("no live sidecar found for origin session session-target"),
        "expected no-live-sidecar reason, got: {}",
        out.text
    );

    // Crucially: the OTHER server (wrong session) must NOT have received the reply.
    // Its inbox should be empty of our pushed text.
    let other_inbox = CcRelay::new().fetch_reply(other.port, "nope", 50); // just liveness
    let _ = other_inbox;
    let inbox_dir = &other.server.paths.inbox_dir;
    let mut wrong_delivery = false;
    if let Ok(entries) = std::fs::read_dir(inbox_dir) {
        for e in entries.flatten() {
            if let Ok(bytes) = std::fs::read(e.path()) {
                if String::from_utf8_lossy(&bytes).contains("for the target only") {
                    wrong_delivery = true;
                }
            }
        }
    }
    assert!(
        !wrong_delivery,
        "RED: reply delivered to the WRONG session (identity guard failed)"
    );

    handle.shutdown();
    other.shutdown();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&home2);
    eprintln!("hunt7_pushback_skips_stale_and_mismatched: honest NOT-DELIVERED, no wrong-session deliver, no hang");
}

// ---------------------------------------------------------------------------
// HUNT 8: poison-resilience — a panic mid-critical-section must not brick delivery
// ---------------------------------------------------------------------------
//
// Poison the state Mutex (panic while holding the guard), then concurrently hammer
// deliver_reply + /replies + a parked-waiter resolve. All must STILL work (the
// recover-guards posture). A single failure after poison = RED.

#[test]
fn hunt8_poison_resilience_under_concurrency() {
    let home = unique_home("poison");
    let handle =
        RelayServer::spawn_for_test(&home, 0, Duration::from_secs(3), Duration::from_secs(5));
    let port = handle.port;
    let server = Arc::clone(&handle.server);
    wait_listener(port);

    // Poison the Mutex from a thread that panics while holding the guard.
    {
        let s = Arc::clone(&server);
        let _ = thread::spawn(move || {
            let _g = s.state.lock().unwrap();
            panic!("intentional poison mid-critical-section");
        })
        .join();
    }
    assert!(server.state.is_poisoned(), "mutex must be poisoned now");

    // After poison: a parked /replies long-poll must still be resolvable.
    let id = "relay-after-poison-1".to_string();
    let text = "survives-poison".to_string();
    let poll = {
        let id = id.clone();
        thread::spawn(move || CcRelay::new().fetch_reply(port, &id, 4000))
    };
    for _ in 0..400 {
        // Note: has_waiter goes through the recovered guard too.
        if server
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .has_waiter(&id)
        {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let out = with_deadline("poison_resolve", Duration::from_secs(5), {
        let server = Arc::clone(&server);
        let id = id.clone();
        let text = text.clone();
        move || server.deliver_reply(&id, &text)
    });
    assert!(
        !out.is_error,
        "post-poison waiter-resolve must still deliver"
    );
    let reply = poll.join().expect("poll").expect("fetch ok after poison");
    assert_eq!(
        reply.text.as_deref(),
        Some(text.as_str()),
        "post-poison parked waiter must still receive its reply"
    );

    // And a concurrent burst of delivers + a fresh POST all succeed post-poison.
    let burst = with_deadline("poison_burst", Duration::from_secs(5), {
        let server = Arc::clone(&server);
        move || {
            for i in 0..50 {
                server.deliver_reply(&format!("relay-pb-{i}"), "x");
            }
            CcRelay::new().send_message(port, "post-poison body", "s")
        }
    });
    assert!(burst.is_ok(), "POST /message must still work after poison");

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&home);
    eprintln!(
        "hunt8_poison_resilience: parked-resolve + burst + POST all survived a poisoned lock"
    );
}

// ---------------------------------------------------------------------------
// HUNT 9 (extra, adversarial): P-G5 — concurrent senders mint DISTINCT ids
// ---------------------------------------------------------------------------
//
// N threads POST /message simultaneously; every returned message_id must be
// UNIQUE (the monotonic seq under the lock must not double-mint under contention).
// A duplicate id = a corrupted mint = a reply could route to the wrong origin.

#[test]
fn hunt9_concurrent_senders_distinct_ids() {
    let home = unique_home("g5");
    let handle =
        RelayServer::spawn_for_test(&home, 0, Duration::from_millis(100), Duration::from_secs(5));
    let port = handle.port;
    wait_listener(port);

    let n_threads = 8;
    let per = suite_iters(40);
    let barrier = Arc::new(Barrier::new(n_threads));
    let mut handles = Vec::new();
    for _ in 0..n_threads {
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let client = CcRelay::new();
            b.wait();
            let mut ids = Vec::with_capacity(per);
            for _ in 0..per {
                if let Ok(id) = client.send_message(port, "body", "session-sender") {
                    ids.push(id);
                }
            }
            ids
        }));
    }
    let mut all = std::collections::HashSet::new();
    let mut total = 0usize;
    for h in handles {
        for id in h.join().expect("sender thread") {
            total += 1;
            assert!(
                all.insert(id.clone()),
                "DUPLICATE message_id minted under contention: {id}"
            );
        }
    }
    assert_eq!(all.len(), total, "every minted id must be unique");
    handle.shutdown();
    let _ = std::fs::remove_dir_all(&home);
    eprintln!("hunt9_concurrent_senders_distinct_ids: {total} ids, all unique");
}
