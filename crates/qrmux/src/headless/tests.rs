//! WP-B2a DoD tests for the headless reader/sink (§6.2/§H.5):
//!  - backpressure stress: a >64 KB turn behind a STALLED/closed sink must not
//!    deadlock the reader; the bounded queue respects its cap; `result` is never
//!    dropped (§H.5);
//!  - circuit-breaker → child-kill: an oversize never-terminating line trips the
//!    breaker and the (fake) child is killed (§H.5);
//!  - red-before/green-after: a reader gated ON the sink would deadlock — proven
//!    via a coupled reference reader (`coupled_reader_blocks_on_stalled_sink`);
//!  - concurrency k≥2: two sessions drained concurrently, events routed by
//!    session, no cross-talk;
//!  - latency p50–p99 on parse→sink;
//!  - one #[ignore]d real-isolated-claude integration turn.
//!
//! All but the integration test use a FAKE pipe (a `Read` over bytes, optionally
//! chunked/slow) + a FAKE sink + a FAKE child — no real process.

use super::*;
use crate::stream_json::StreamEvent;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// ---- fixtures -------------------------------------------------------------

fn fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/stream_json")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {p:?}: {e}"))
}

// ---- fake child -----------------------------------------------------------

/// A fake child that records whether it was killed (no real process).
#[derive(Clone)]
struct FakeChild {
    killed: Arc<AtomicBool>,
    kill_count: Arc<AtomicUsize>,
}
impl FakeChild {
    fn new() -> Self {
        Self {
            killed: Arc::new(AtomicBool::new(false)),
            kill_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl Child for FakeChild {
    fn kill(&mut self) {
        self.kill_count.fetch_add(1, Ordering::SeqCst);
        self.killed.store(true, Ordering::SeqCst);
    }
    fn was_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }
}

// ---- fake sink ------------------------------------------------------------

/// Records every delivered message. Optionally stalls on a barrier (to model a
/// slow/stalled downstream client) or panics-if-touched (closed sink).
#[derive(Clone)]
struct RecordingSink {
    msgs: Arc<Mutex<Vec<Republish>>>,
    // When set, deliver() blocks until `release` is signalled — a STALLED sink.
    gate: Arc<(Mutex<bool>, Condvar)>,
    gated: bool,
}
impl RecordingSink {
    fn new() -> Self {
        Self {
            msgs: Arc::new(Mutex::new(Vec::new())),
            gate: Arc::new((Mutex::new(false), Condvar::new())),
            gated: false,
        }
    }
    fn stalled() -> Self {
        let mut s = Self::new();
        s.gated = true;
        s
    }
    fn release(&self) {
        let (lock, cv) = &*self.gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    fn delivered(&self) -> Vec<Republish> {
        self.msgs.lock().unwrap().clone()
    }
}
impl Sink for RecordingSink {
    fn deliver(&self, msg: Republish) {
        if self.gated {
            let (lock, cv) = &*self.gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cv.wait(released).unwrap();
            }
        }
        self.msgs.lock().unwrap().push(msg);
    }
}

// ---- a SLOW fake pipe (drip bytes) ----------------------------------------

/// A `Read` that hands out the inner bytes in small chunks, optionally sleeping
/// between reads — models the kernel delivering a big turn in pipe-sized chunks.
struct SlowPipe {
    data: Vec<u8>,
    pos: usize,
    chunk: usize,
    per_read_delay: Duration,
}
impl SlowPipe {
    fn new(data: Vec<u8>, chunk: usize, per_read_delay: Duration) -> Self {
        Self {
            data,
            pos: 0,
            chunk,
            per_read_delay,
        }
    }
}
impl Read for SlowPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        if !self.per_read_delay.is_zero() {
            std::thread::sleep(self.per_read_delay);
        }
        let end = (self.pos + self.chunk)
            .min(self.data.len())
            .min(self.pos + buf.len());
        let n = end - self.pos;
        buf[..n].copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(n)
    }
}

// ===========================================================================
// Backpressure stress (§H.5): >64KB turn + STALLED sink must not deadlock the
// reader; bounded queue respects cap; `result` is never dropped.
// ===========================================================================

/// GREEN: the >64 KB fixture, fed through a slow fake pipe, drained with the
/// sink STALLED the entire time. The reader must run to EOF (parse the result)
/// without blocking, even though the pump is frozen on the stalled sink. We
/// observe the reader finished (queue closed) by joining after releasing.
#[test]
fn backpressure_stalled_sink_does_not_stall_reader() {
    let bytes = fixture("big_result_over_64k.ndjson"); // ~90 KB single result line
    assert!(
        bytes.len() > 64 * 1024,
        "fixture must exceed one pipe (>64KB)"
    );

    let sink = RecordingSink::stalled();
    let child = FakeChild::new();
    // Small chunks (4 KB) so the >64KB line spans many reads (the torn case).
    let pipe = SlowPipe::new(bytes, 4096, Duration::ZERO);

    let reader = HeadlessReader::start(
        pipe,
        sink.clone(),
        child.clone(),
        DEFAULT_QUEUE_CAP,
        DEFAULT_MAX_LINE_BYTES,
    );

    // The reader thread must reach EOF WITHOUT the sink ever draining. We prove
    // "did not block" by waiting (bounded) for the reader to enqueue everything
    // and close — the queue holds the result while the pump is frozen.
    let deadline = Instant::now() + Duration::from_secs(5);
    while reader.queue_len() == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    // The result is queued (reader drained the pipe past the >64KB line) even
    // though the stalled sink has delivered NOTHING yet.
    assert!(
        reader.queue_len() > 0,
        "reader must enqueue without the sink draining"
    );
    assert!(
        sink.delivered().is_empty(),
        "stalled sink must not have delivered yet"
    );

    // Release the sink and join — everything drains, no deadlock.
    sink.release();
    let outcome = reader.join();
    assert_eq!(outcome, TurnOutcome::Completed);

    let delivered = sink.delivered();
    // The `result` (TurnEnd) MUST be present — never dropped.
    let turn_ends = delivered
        .iter()
        .filter(|m| matches!(m, Republish::TurnEnd(_)))
        .count();
    assert_eq!(turn_ends, 1, "exactly one TurnEnd (result) must survive");
}

/// §H.5: bounded queue respects its cap under overflow, and the `result` is
/// NEVER dropped. We push a flood of coalescible Assistant events through a tiny
/// cap with the sink stalled, then a `result`. The queue length stays <= cap+ε
/// and the result survives.
#[test]
fn bounded_queue_respects_cap_and_never_drops_result() {
    // Synthesize init, then the `result` EARLY, then a long flood of coalescible
    // assistant events AFTER it. With the sink stalled the whole turn buffers in
    // the queue; the post-result flood forces overflow eviction. The result must
    // SURVIVE — proving it is never evicted to make room (not merely "it happened
    // to be last"). A mutation that lets the result be evicted goes red here.
    let mut ndjson = Vec::new();
    let init = br#"{"type":"system","subtype":"init","session_id":"cap-test"}"#;
    ndjson.extend_from_slice(init);
    ndjson.push(b'\n');
    let result = br#"{"type":"result","subtype":"success","session_id":"cap-test","stop_reason":"end_turn"}"#;
    ndjson.extend_from_slice(result);
    ndjson.push(b'\n');
    for i in 0..500 {
        let line = format!(
            r#"{{"type":"assistant","session_id":"cap-test","message":{{"content":[{{"type":"text","text":"chunk-{i}"}}]}}}}"#
        );
        ndjson.extend_from_slice(line.as_bytes());
        ndjson.push(b'\n');
    }

    let cap = 16usize;
    let sink = RecordingSink::stalled();
    let child = FakeChild::new();
    let pipe = SlowPipe::new(ndjson, 256, Duration::ZERO);

    let reader = HeadlessReader::start(
        pipe,
        sink.clone(),
        child.clone(),
        cap,
        DEFAULT_MAX_LINE_BYTES,
    );

    // Wait (bounded) for the reader to finish enqueuing (queue closed).
    let deadline = Instant::now() + Duration::from_secs(5);
    // The reader closes the queue at EOF; once closed, len stops growing.
    // We can't read `closed` directly, so spin until len stabilizes near cap.
    let mut last = 0;
    let mut stable = 0;
    while Instant::now() < deadline {
        let l = reader.queue_len();
        // The cap is honored for coalescibles; must-keeps (Ready+TurnEnd+Eof)
        // can overshoot slightly. Allow a small must-keep headroom.
        assert!(
            l <= cap + 4,
            "queue len {l} must respect cap {cap} (+must-keep headroom)"
        );
        if l == last {
            stable += 1;
        } else {
            stable = 0;
            last = l;
        }
        if stable > 1000 {
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        reader.dropped() > 0,
        "overflow must have dropped coalescible status"
    );

    sink.release();
    let outcome = reader.join();
    assert_eq!(outcome, TurnOutcome::Completed);

    let delivered = sink.delivered();
    let turn_ends = delivered
        .iter()
        .filter(|m| matches!(m, Republish::TurnEnd(_)))
        .count();
    assert_eq!(
        turn_ends, 1,
        "the result must NEVER be dropped despite overflow"
    );
}

/// A CLOSED sink (the consumer is gone) must never stall the reader thread: the
/// reader keeps draining and reaches EOF. We model "closed" as a sink that
/// never returns (stalled forever) and assert the reader still finished by
/// observing the queue was fully enqueued. (We do not join — joining would wait
/// on the frozen pump; we drop instead, which closes the queue and frees it.)
#[test]
fn closed_sink_never_stalls_reader() {
    let bytes = fixture("large_events.ndjson");
    let sink = RecordingSink::stalled(); // never released == permanently closed
    let child = FakeChild::new();
    let pipe = SlowPipe::new(bytes, 8192, Duration::ZERO);

    let reader = HeadlessReader::start(
        pipe,
        sink.clone(),
        child.clone(),
        DEFAULT_QUEUE_CAP,
        DEFAULT_MAX_LINE_BYTES,
    );

    // The reader must reach EOF and enqueue (incl. the Eof marker) without the
    // sink ever delivering. Bounded wait for the queue to hold the Eof sentinel
    // by waiting for queue growth then no-more-growth.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut prev = 0;
    let mut settled = false;
    while Instant::now() < deadline {
        let l = reader.queue_len();
        if l > 0 && l == prev {
            settled = true;
            break;
        }
        prev = l;
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        settled,
        "reader must enqueue to EOF without the closed sink draining"
    );
    assert!(
        sink.delivered().is_empty(),
        "closed sink delivered nothing, yet reader finished"
    );
    sink.release(); // let the pump unwind cleanly on drop
}

/// RED-BEFORE reference: a COUPLED reader (sink on the read hot path) WOULD
/// block when the sink stalls — proving the decoupling is load-bearing. We model
/// the coupled design inline and assert it deadlocks (times out), whereas the
/// real `HeadlessReader` (above) does not.
#[test]
fn coupled_reader_blocks_on_stalled_sink_red_reference() {
    let bytes = fixture("big_result_over_64k.ndjson");
    let sink = RecordingSink::stalled();
    let mut pipe = SlowPipe::new(bytes, 4096, Duration::ZERO);

    let finished = Arc::new(AtomicBool::new(false));
    let finished2 = finished.clone();
    let sink2 = sink.clone();
    // The COUPLED (wrong) design: deliver straight from the read loop.
    let h = std::thread::spawn(move || {
        let mut parser = StreamParser::new();
        let mut rb = [0u8; 64 * 1024];
        loop {
            match pipe.read(&mut rb) {
                Ok(0) => break,
                Ok(n) => {
                    for ev in parser.push(&rb[..n]) {
                        // BLOCKS here on the stalled sink (no decoupling).
                        sink2.deliver(Republish::Event(ev));
                    }
                }
                Err(_) => break,
            }
        }
        finished2.store(true, Ordering::SeqCst);
    });

    // Within a bounded window the coupled reader must NOT finish (it is blocked).
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !finished.load(Ordering::SeqCst),
        "coupled reader should be BLOCKED on the stalled sink (red reference)"
    );
    // Cleanup: release + join.
    sink.release();
    let _ = h.join();
}

// ===========================================================================
// Circuit-breaker → child-kill (§H.5).
// ===========================================================================

/// A never-terminating oversize line (no `\n`, exceeds the cap) trips the
/// breaker → the fake child is killed exactly once, breaker_fired is set, and a
/// Breaker republish is emitted.
#[test]
fn circuit_breaker_kills_child() {
    let cap = 4096usize;
    // A line with no newline, larger than the cap.
    let blob = vec![b'x'; cap * 4];
    let sink = RecordingSink::new(); // not stalled — let it drain
    let child = FakeChild::new();
    let pipe = SlowPipe::new(blob, 1024, Duration::ZERO);

    let reader = HeadlessReader::start(pipe, sink.clone(), child.clone(), DEFAULT_QUEUE_CAP, cap);
    let outcome = reader.join();

    assert_eq!(
        outcome,
        TurnOutcome::Aborted,
        "a tripped stream aborts the turn"
    );
    assert!(
        child.was_killed(),
        "the child MUST be killed on the breaker (§H.5)"
    );
    assert_eq!(
        child.kill_count.load(Ordering::SeqCst),
        1,
        "kill exactly once"
    );

    let delivered = sink.delivered();
    assert!(
        delivered
            .iter()
            .any(|m| matches!(m, Republish::Breaker { .. })),
        "a Breaker republish must be emitted"
    );
}

/// Without an oversize line, the child is NEVER killed (no false breaker).
#[test]
fn no_breaker_no_kill_on_normal_turn() {
    let bytes = fixture("multiturn_keepalive.ndjson");
    let sink = RecordingSink::new();
    let child = FakeChild::new();
    let pipe = SlowPipe::new(bytes, 4096, Duration::ZERO);
    let reader = HeadlessReader::start(
        pipe,
        sink.clone(),
        child.clone(),
        DEFAULT_QUEUE_CAP,
        DEFAULT_MAX_LINE_BYTES,
    );
    let outcome = reader.join();
    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(!child.was_killed(), "no breaker → no kill");
}

// ===========================================================================
// Readiness fact + event routing basics.
// ===========================================================================

/// The first event (`system/init`) republishes a Ready fact carrying the
/// session_id, before turn-end.
#[test]
fn first_event_publishes_ready() {
    let bytes = fixture("multiturn_keepalive.ndjson");
    let sink = RecordingSink::new();
    let child = FakeChild::new();
    let pipe = SlowPipe::new(bytes, 4096, Duration::ZERO);
    let reader = HeadlessReader::start(
        pipe,
        sink.clone(),
        child,
        DEFAULT_QUEUE_CAP,
        DEFAULT_MAX_LINE_BYTES,
    );
    let _ = reader.join();
    let delivered = sink.delivered();
    let first_ready = delivered.iter().find_map(|m| match m {
        Republish::Ready { session_id } => Some(session_id.clone()),
        _ => None,
    });
    assert_eq!(
        first_ready.as_deref(),
        Some("6d385905-cb54-46c6-970f-e9c3a73a5ba8")
    );
}

// ===========================================================================
// Concurrency k>=2: two sessions drained concurrently, routed by session_id,
// no cross-talk.
// ===========================================================================

#[test]
fn concurrency_two_sessions_no_cross_talk() {
    // Build two distinct synthetic turns with different session_ids.
    fn turn(sid: &str, text: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(
            format!(r#"{{"type":"system","subtype":"init","session_id":"{sid}"}}"#).as_bytes(),
        );
        v.push(b'\n');
        v.extend_from_slice(format!(r#"{{"type":"assistant","session_id":"{sid}","message":{{"content":[{{"type":"text","text":"{text}"}}]}}}}"#).as_bytes());
        v.push(b'\n');
        v.extend_from_slice(format!(r#"{{"type":"result","subtype":"success","session_id":"{sid}","stop_reason":"end_turn"}}"#).as_bytes());
        v.push(b'\n');
        v
    }
    let a = turn("sess-AAAA", "alpha");
    let b = turn("sess-BBBB", "bravo");

    let sink_a = RecordingSink::new();
    let sink_b = RecordingSink::new();
    let ra = HeadlessReader::start(
        SlowPipe::new(a, 32, Duration::from_micros(50)),
        sink_a.clone(),
        FakeChild::new(),
        DEFAULT_QUEUE_CAP,
        DEFAULT_MAX_LINE_BYTES,
    );
    let rb = HeadlessReader::start(
        SlowPipe::new(b, 32, Duration::from_micros(50)),
        sink_b.clone(),
        FakeChild::new(),
        DEFAULT_QUEUE_CAP,
        DEFAULT_MAX_LINE_BYTES,
    );

    assert_eq!(ra.join(), TurnOutcome::Completed);
    assert_eq!(rb.join(), TurnOutcome::Completed);

    // Every message in sink_a must carry session AAAA; sink_b only BBBB.
    fn sids(msgs: &[Republish]) -> Vec<String> {
        msgs.iter()
            .filter_map(|m| match m {
                Republish::Ready { session_id } => Some(session_id.clone()),
                Republish::TurnEnd(r) => Some(r.session_id.clone()),
                Republish::Event(StreamEvent::SystemInit { session_id }) => {
                    Some(session_id.clone())
                }
                Republish::Event(StreamEvent::Assistant { session_id, .. }) => {
                    Some(session_id.clone())
                }
                _ => None,
            })
            .collect()
    }
    let da = sids(&sink_a.delivered());
    let db = sids(&sink_b.delivered());
    assert!(
        !da.is_empty() && da.iter().all(|s| s == "sess-AAAA"),
        "sink A only AAAA, got {da:?}"
    );
    assert!(
        !db.is_empty() && db.iter().all(|s| s == "sess-BBBB"),
        "sink B only BBBB, got {db:?}"
    );
}

// ===========================================================================
// Latency p50-p99 on parse -> sink.
// ===========================================================================

/// Measures per-event parse→sink latency over the multiturn fixture, replayed
/// many times. Asserts a sane upper bound (the path is in-memory; this guards a
/// pathological regression, not a hard SLA) and prints p50/p99.
#[test]
fn latency_parse_to_sink_p50_p99() {
    /// A sink that stamps delivery time per message.
    #[derive(Clone)]
    struct TimingSink {
        stamps: Arc<Mutex<Vec<Instant>>>,
    }
    impl Sink for TimingSink {
        fn deliver(&self, _msg: Republish) {
            self.stamps.lock().unwrap().push(Instant::now());
        }
    }

    let one = fixture("multiturn_keepalive.ndjson");
    // Replay the fixture many times back-to-back for a population.
    let reps = 200;
    let mut bytes = Vec::with_capacity(one.len() * reps);
    for _ in 0..reps {
        bytes.extend_from_slice(&one);
    }

    let sink = TimingSink {
        stamps: Arc::new(Mutex::new(Vec::new())),
    };
    let child = FakeChild::new();
    let start = Instant::now();
    // Feed the whole blob in one go (no artificial read delay) so we measure the
    // parse→enqueue→pump→deliver pipeline, not the fake pipe's pacing.
    let pipe = SlowPipe::new(bytes, 64 * 1024, Duration::ZERO);
    let reader = HeadlessReader::start(
        pipe,
        sink.clone(),
        child,
        DEFAULT_QUEUE_CAP,
        DEFAULT_MAX_LINE_BYTES,
    );
    let _ = reader.join();

    let stamps = sink.stamps.lock().unwrap().clone();
    assert!(
        stamps.len() > reps,
        "should deliver many events, got {}",
        stamps.len()
    );
    // Inter-delivery latencies as the proxy for the parse→sink pipeline cadence.
    let mut deltas: Vec<u128> = Vec::new();
    let mut prev = start;
    for t in &stamps {
        deltas.push(t.duration_since(prev).as_micros());
        prev = *t;
    }
    deltas.sort_unstable();
    let pct = |p: f64| deltas[((deltas.len() as f64 * p) as usize).min(deltas.len() - 1)];
    let p50 = pct(0.50);
    let p99 = pct(0.99);
    println!(
        "LATENCY parse→sink (us): p50={p50} p99={p99} n={}",
        deltas.len()
    );
    // Guard a pathological regression: even p99 should be sub-millisecond-ish on
    // an in-memory path. Generous bound to avoid CI flake.
    assert!(
        p99 < 50_000,
        "p99 inter-delivery latency {p99}us unexpectedly high"
    );
}

// ===========================================================================
// Integration: ONE real isolated claude turn (network/creds → #[ignore]).
// ===========================================================================

/// Launch ONE real headless turn in an isolated sandbox (lib.sh contract:
/// synthetic HOME + CLAUDE_CONFIG_DIR, NEVER the live ~/.claude*), drain it, and
/// assert the `result` arrives with the right session_id and "PONG" content.
///
/// `#[ignore]` because it needs network + credentials; run with:
///   cargo test -p qrmux --lib headless::tests::real_isolated_claude_pong -- --ignored --nocapture
#[test]
#[ignore = "needs network + live credentials; isolated via lib.sh-style env -i"]
fn real_isolated_claude_pong() {
    use std::path::PathBuf;

    let claude_bin = std::env::var("CLAUDE_BIN")
        .unwrap_or_else(|_| "/home/u/.local/bin/claude".to_string());
    let live_creds = std::env::var("LIVE_CREDENTIALS")
        .unwrap_or_else(|_| "/home/u/.claude/.credentials.json".to_string());

    // Build the isolated sandbox (the lib.sh contract, minimal inline form).
    let qb = tempfile::tempdir().expect("mktemp sandbox");
    let root = qb.path();
    let home = root.join("home");
    let config = root.join("config");
    let work = root.join("work");
    for d in [&home, &config, &work] {
        std::fs::create_dir_all(d).unwrap();
    }
    // Copy ONLY the oauth credential into the sandbox config (nothing else live).
    if PathBuf::from(&live_creds).exists() {
        std::fs::copy(&live_creds, config.join(".credentials.json")).expect("copy creds");
    }
    // Seed a controlled baseline claude.json (onboarding done, no MCP) + trust cwd.
    let work_str = work.to_string_lossy();
    let claude_json = format!(
        r#"{{
  "hasCompletedOnboarding": true,
  "numStartups": 0,
  "autoUpdates": false,
  "hasSeenTasksHint": true,
  "bypassPermissionsModeAccepted": true,
  "mcpServers": {{}},
  "enableAllProjectMcpServers": false,
  "projects": {{ "{work_str}": {{ "hasTrustDialogAccepted": true, "projectOnboardingSeenCount": 1, "allowedTools": [] }} }}
}}"#
    );
    std::fs::write(config.join(".claude.json"), claude_json).unwrap();

    // env -i HOME=.. CLAUDE_CONFIG_DIR=.. PATH=.. TERM=dumb (the isolation argv).
    let launch = HeadlessLaunch {
        claude_bin,
        prompt: "Reply with exactly: PONG".to_string(),
        resume_session_id: None,
        cwd: Some(work.to_string_lossy().into_owned()),
        clear_env: true,
        env: vec![
            ("HOME".into(), home.to_string_lossy().into_owned()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                config.to_string_lossy().into_owned(),
            ),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("TERM".into(), "dumb".into()),
        ],
        flags: vec![],
    };

    let (child, stdout) = launch.spawn().expect("spawn isolated claude");

    let sink = RecordingSink::new();
    let reader = HeadlessReader::start(
        stdout,
        sink.clone(),
        child,
        DEFAULT_QUEUE_CAP,
        DEFAULT_MAX_LINE_BYTES,
    );
    let outcome = reader.join();
    assert_eq!(
        outcome,
        TurnOutcome::Completed,
        "the real turn must complete with a result"
    );

    let delivered = sink.delivered();
    // The result arrives with a session_id.
    let result = delivered.iter().find_map(|m| match m {
        Republish::TurnEnd(r) => Some(r.clone()),
        _ => None,
    });
    let result = result.expect("a result event must arrive");
    assert!(!result.session_id.is_empty(), "result carries a session_id");
    assert!(!result.is_error, "the turn must succeed");

    // The Ready fact carries the SAME session_id as the result.
    let ready_sid = delivered.iter().find_map(|m| match m {
        Republish::Ready { session_id } => Some(session_id.clone()),
        _ => None,
    });
    assert_eq!(
        ready_sid.as_deref(),
        Some(result.session_id.as_str()),
        "Ready and result share session_id"
    );

    // Assistant content contains PONG (in-band content, never the transcript).
    let content: String = delivered
        .iter()
        .filter_map(|m| match m {
            Republish::Event(StreamEvent::Assistant { content, .. }) => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert!(
        content.contains("PONG"),
        "assistant content must contain PONG, got: {content:?}"
    );

    println!(
        "REAL CLAUDE: session_id={} content={:?}",
        result.session_id, content
    );
}

// ---- argv() purity: spawn-time flag injection + ordering (WP-B2a-addendum) ----
//
// Pure golden tests over `HeadlessLaunch::argv()` — NO real claude, NO spawn.
// Implements amend-v1 ruling #6: the daemon resolves the (A) launch flags
// (`--dangerously-skip-permissions` + `--dangerously-load-development-channels
// server:relay`) and carries them into `HeadlessLaunch.flags`; `argv()` emits
// them immediately after the binary, before `-p` (flags-first, matching the (A)
// `launch.rs` golden). qrmux stays pure — it never resolves flags itself.

/// The three flags the interactive (A) PTY path injects (launch.rs DEFAULT_FLAGS).
/// Carried here as already-resolved daemon data — NOT imported from crates/qd.
fn relay_flags() -> Vec<String> {
    vec![
        "--dangerously-skip-permissions".to_string(),
        "--dangerously-load-development-channels".to_string(),
        "server:relay".to_string(),
    ]
}

fn launch_with(flags: Vec<String>, resume: Option<&str>) -> HeadlessLaunch {
    HeadlessLaunch {
        claude_bin: "claude".to_string(),
        prompt: "do the thing".to_string(),
        resume_session_id: resume.map(|s| s.to_string()),
        cwd: None,
        env: vec![],
        clear_env: false,
        flags,
    }
}

/// GOLDEN (green-after): flags appear after the binary and before `-p`, in order.
/// Asserts the FULL vector — not `.contains()`.
#[test]
fn argv_golden_injects_flags_before_prompt() {
    let launch = launch_with(relay_flags(), None);
    let expected: Vec<String> = vec![
        "claude",
        "--dangerously-skip-permissions",
        "--dangerously-load-development-channels",
        "server:relay",
        "-p",
        "do the thing",
        "--output-format",
        "stream-json",
        "--verbose",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(launch.argv(), expected);
}

/// FALSE-POSITIVE GUARD: `flags: vec![]` injects NO posture — a bare `-p` launch.
/// Proves we never inject uninvited `--dangerously-*` tokens.
#[test]
fn argv_empty_flags_no_injection() {
    let launch = launch_with(vec![], None);
    let expected: Vec<String> = vec![
        "claude",
        "-p",
        "do the thing",
        "--output-format",
        "stream-json",
        "--verbose",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(launch.argv(), expected);
    assert!(
        !launch
            .argv()
            .iter()
            .any(|a| a.starts_with("--dangerously-")),
        "empty flags must inject no --dangerously-* tokens"
    );
}

/// RESUME ORDERING: `--resume <sid>` is at the TAIL; flags still come first.
/// FIX-SHAPED MUTATION: dropping `self.flags` from `argv()` turns both this and
/// `argv_golden_injects_flags_before_prompt` red (the leading `--dangerously-*`
/// tokens vanish) — demonstrated by removal/restore during build.
#[test]
fn argv_resume_keeps_flags_first_resume_at_tail() {
    let launch = launch_with(relay_flags(), Some("sess-123"));
    let expected: Vec<String> = vec![
        "claude",
        "--dangerously-skip-permissions",
        "--dangerously-load-development-channels",
        "server:relay",
        "-p",
        "do the thing",
        "--output-format",
        "stream-json",
        "--verbose",
        "--resume",
        "sess-123",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(launch.argv(), expected);
}
