//! WP-B1 parser tests: red-before/green-after framing, property/fuzz, the
//! table-driven corpus (real captures + synthetic >64 KB), the §H.5
//! bounded-buffer/circuit-breaker proof, the embedded-newline verification, and
//! a latency micro-bench.
//!
//! Fixtures (real captures + one synthetic >64 KB) live under
//! `crates/qrmux/tests/fixtures/stream_json/`. They are read at runtime via
//! `CARGO_MANIFEST_DIR` so the lib-test binary can reach them.

use super::*;
use std::path::PathBuf;

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/stream_json")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {p:?}: {e}"))
}

/// Feed a whole byte stream to a fresh parser in one push, then finish.
fn parse_whole(bytes: &[u8]) -> (Vec<StreamEvent>, TurnOutcome) {
    let mut p = StreamParser::new();
    let evs = p.push(bytes);
    let outcome = p.finish();
    (evs, outcome)
}

/// Feed a byte stream one chunk of `chunk_size` bytes at a time.
fn parse_chunked(bytes: &[u8], chunk_size: usize, cap: usize) -> (Vec<StreamEvent>, TurnOutcome) {
    let mut p = StreamParser::with_cap(cap);
    let mut evs = Vec::new();
    for ch in bytes.chunks(chunk_size.max(1)) {
        evs.extend(p.push(ch));
    }
    let outcome = p.finish();
    (evs, outcome)
}

/// Feed a byte stream one byte at a time — the strongest reassembly stress.
fn parse_byte_at_a_time(bytes: &[u8]) -> (Vec<StreamEvent>, TurnOutcome) {
    let mut p = StreamParser::new();
    let mut evs = Vec::new();
    for &b in bytes {
        evs.extend(p.push(&[b]));
    }
    let outcome = p.finish();
    (evs, outcome)
}

// ---------------------------------------------------------------------------
// Red-before / green-after: a NAIVE line-splitter fails these; ours passes.
// ---------------------------------------------------------------------------

/// A deliberately-naive reference splitter: split the chunk on `\n` and parse
/// every piece, with NO cross-chunk reassembly. This is the bug the WP-B1
/// state machine fixes; the next two tests assert our parser does NOT behave
/// like it.
fn naive_split_one_chunk(chunk: &[u8]) -> Vec<StreamEvent> {
    chunk
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| parse_line(l))
        .collect()
}

/// RED-BEFORE evidence: a torn mid-line chunk makes the naive splitter emit a
/// broken (parse_error) event for the half-line. Our state machine buffers the
/// partial and emits the single correct event once the `\n` arrives.
#[test]
fn red_before_naive_splits_a_torn_midline_chunk() {
    let line = br#"{"type":"system","subtype":"init","session_id":"abc"}"#;
    let (head, tail) = line.split_at(20); // tear mid-JSON

    // The naive splitter, fed the torn head alone, produces a parse_error event
    // (the bug we must not reproduce).
    let naive = naive_split_one_chunk(head);
    assert_eq!(naive.len(), 1);
    assert!(
        matches!(
            &naive[0],
            StreamEvent::Other {
                parse_error: true,
                ..
            }
        ),
        "naive splitter emits a broken event for a torn head — got {:?}",
        naive[0]
    );

    // GREEN-AFTER: our state machine buffers the head, emits nothing yet, and
    // produces exactly one correct SystemInit once the tail + `\n` arrive.
    let mut p = StreamParser::new();
    let after_head = p.push(head);
    assert!(
        after_head.is_empty(),
        "partial line must not emit; buffered"
    );
    assert_eq!(p.buffered_len(), head.len());
    let mut after_tail = p.push(tail);
    after_tail.extend(p.push(b"\n"));
    assert_eq!(
        after_tail,
        vec![StreamEvent::SystemInit {
            session_id: "abc".into()
        }]
    );
}

/// RED-BEFORE evidence: an event larger than one chunk. The naive per-chunk
/// splitter, fed only the first 64-byte slice, emits a parse_error; the state
/// machine reassembles across all chunks into one event.
#[test]
fn red_before_event_larger_than_one_chunk() {
    let bytes = fixture("big_result_over_64k.ndjson");
    // The result line is ~90 KB; a 64-byte chunk is far smaller than one event.
    let (evs, outcome) = parse_chunked(&bytes, 64, DEFAULT_MAX_LINE_BYTES);
    assert_eq!(outcome, TurnOutcome::Completed);
    // Exactly one SystemInit and one Result, reassembled — no parse_errors.
    let inits = evs
        .iter()
        .filter(|e| matches!(e, StreamEvent::SystemInit { .. }))
        .count();
    let results = evs
        .iter()
        .filter(|e| matches!(e, StreamEvent::Result(_)))
        .count();
    let errors = evs
        .iter()
        .filter(|e| {
            matches!(
                e,
                StreamEvent::Other {
                    parse_error: true,
                    ..
                }
            )
        })
        .count();
    assert_eq!((inits, results, errors), (1, 1, 0), "events: {evs:?}");

    // A naive splitter fed only the first 64-byte slice would emit a broken
    // event — proving the reassembly is load-bearing.
    let naive = naive_split_one_chunk(&bytes[..64]);
    assert!(
        naive.iter().any(|e| matches!(
            e,
            StreamEvent::Other {
                parse_error: true,
                ..
            }
        )),
        "naive per-chunk split of a sub-event slice must break"
    );
}

// ---------------------------------------------------------------------------
// Core framing properties.
// ---------------------------------------------------------------------------

/// The strong reassembly property: feeding a stream byte-at-a-time yields the
/// IDENTICAL event sequence to feeding it whole. Run across every fixture.
#[test]
fn property_byte_at_a_time_equals_whole() {
    for name in [
        "multiturn_keepalive.ndjson",
        "large_events.ndjson",
        "killed_init_only.ndjson",
        "big_result_over_64k.ndjson",
    ] {
        let bytes = fixture(name);
        let (whole, whole_out) = parse_whole(&bytes);
        let (per_byte, per_byte_out) = parse_byte_at_a_time(&bytes);
        assert_eq!(whole, per_byte, "byte-at-a-time mismatch for {name}");
        assert_eq!(whole_out, per_byte_out, "outcome mismatch for {name}");
    }
}

/// Reassembly is invariant under EVERY chunk size — interleaved partial reads
/// at every boundary must produce the same sequence.
#[test]
fn property_every_chunk_size_equals_whole() {
    for name in ["multiturn_keepalive.ndjson", "large_events.ndjson"] {
        let bytes = fixture(name);
        let (whole, whole_out) = parse_whole(&bytes);
        for cs in 1..=257usize {
            let (chunked, out) = parse_chunked(&bytes, cs, DEFAULT_MAX_LINE_BYTES);
            assert_eq!(chunked, whole, "{name}: chunk_size {cs} mismatch");
            assert_eq!(out, whole_out, "{name}: chunk_size {cs} outcome mismatch");
        }
    }
}

/// No-false-events: a partial trailing line (no `\n`) never emits, and an
/// unterminated tail is discarded at finish (never parsed).
#[test]
fn no_partial_line_emitted_and_tail_discarded() {
    let mut p = StreamParser::new();
    // Two complete events then a partial third (no trailing newline).
    let s = concat!(
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
        "\n",
        r#"{"type":"assistant","session_id":"s1","message":{"content":[{"type":"text","text":"hi"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","session_id":"s1","#, // partial, no \n
    );
    let evs = p.push(s.as_bytes());
    // Only the two terminated events surface; the partial result stays buffered.
    assert_eq!(evs.len(), 2, "got {evs:?}");
    assert!(matches!(evs[0], StreamEvent::SystemInit { .. }));
    assert!(matches!(evs[1], StreamEvent::Assistant { .. }));
    assert!(p.buffered_len() > 0, "partial result must be buffered");
    // EOF: the buffered partial is discarded, never parsed; no `result` was
    // completed → the turn is Aborted (a killed turn).
    let outcome = p.finish();
    assert_eq!(outcome, TurnOutcome::Aborted);
    assert_eq!(p.buffered_len(), 0, "tail discarded");
}

/// Empty input → NoTurn, no events, no panic.
#[test]
fn empty_input_no_turn() {
    let (evs, outcome) = parse_whole(b"");
    assert!(evs.is_empty());
    assert_eq!(outcome, TurnOutcome::NoTurn);
    // And a stream of only newlines (empty lines) → each empty line is a
    // parse_error Other, but still no turn semantics break.
    let (evs2, _out2) = parse_whole(b"\n\n\n");
    assert_eq!(evs2.len(), 3);
    assert!(evs2.iter().all(|e| matches!(
        e,
        StreamEvent::Other {
            parse_error: true,
            ..
        }
    )));
}

/// Input with NO trailing newline: the final complete-looking line is still
/// buffered (not yet terminated) and discarded at EOF — never emitted.
#[test]
fn no_trailing_newline_last_line_not_emitted() {
    let line = r#"{"type":"system","subtype":"init","session_id":"s1"}"#;
    let mut p = StreamParser::new();
    let evs = p.push(line.as_bytes()); // no `\n`
    assert!(evs.is_empty(), "unterminated line must not emit: {evs:?}");
    assert_eq!(p.finish(), TurnOutcome::NoTurn, "no event ever completed");
}

/// Multiple events in a single read (multi-event single chunk).
#[test]
fn multi_event_single_read() {
    let bytes = fixture("multiturn_keepalive.ndjson");
    let (evs, outcome) = parse_whole(&bytes);
    // 3 turns: each system/init, assistant, result; rate_limit on turn 1 only.
    let inits = evs
        .iter()
        .filter(|e| matches!(e, StreamEvent::SystemInit { .. }))
        .count();
    let assistants = evs
        .iter()
        .filter(|e| matches!(e, StreamEvent::Assistant { .. }))
        .count();
    let results = evs
        .iter()
        .filter(|e| matches!(e, StreamEvent::Result(_)))
        .count();
    let rate = evs
        .iter()
        .filter(|e| matches!(e, StreamEvent::RateLimit { .. }))
        .count();
    assert_eq!((inits, assistants, results), (3, 3, 3), "events: {evs:?}");
    assert_eq!(rate, 1, "rate_limit_event appears on turn 1 only");
    assert_eq!(outcome, TurnOutcome::Completed);
}

// ---------------------------------------------------------------------------
// Extraction correctness.
// ---------------------------------------------------------------------------

/// system/init carries the identity binding (session_id) on the FIRST event.
#[test]
fn extracts_session_id_from_first_event() {
    let bytes = fixture("multiturn_keepalive.ndjson");
    let (evs, _) = parse_whole(&bytes);
    match &evs[0] {
        StreamEvent::SystemInit { session_id } => {
            assert_eq!(session_id, "6d385905-cb54-46c6-970f-e9c3a73a5ba8");
        }
        other => panic!("first event must be SystemInit, got {other:?}"),
    }
}

/// assistant content is taken in-band from the stdout event.
#[test]
fn extracts_assistant_content_in_band() {
    let bytes = fixture("multiturn_keepalive.ndjson");
    let (evs, _) = parse_whole(&bytes);
    let contents: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Assistant { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(contents, vec!["ALPHA", "BRAVO", "CHARLIE"]);
}

/// result extraction: turn-end signal + is_error + stop_reason + usage + cost +
/// session_id, including the cache-tier (§H.3) fields.
#[test]
fn extracts_result_fields_and_cache_tier() {
    let bytes = fixture("multiturn_keepalive.ndjson");
    let (evs, _) = parse_whole(&bytes);
    let first_result = evs
        .iter()
        .find_map(|e| match e {
            StreamEvent::Result(r) => Some(r),
            _ => None,
        })
        .expect("a result event");
    assert!(!first_result.is_error);
    assert_eq!(first_result.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(
        first_result.session_id,
        "6d385905-cb54-46c6-970f-e9c3a73a5ba8"
    );
    assert_eq!(first_result.total_cost_usd, Some(0.046249));
    // The real q1mt-1 turn-1 usage (verified against the capture).
    assert_eq!(first_result.usage.input_tokens, 1808);
    assert_eq!(first_result.usage.output_tokens, 7);
    assert_eq!(first_result.usage.cache_read_input_tokens, 14428);
    assert_eq!(first_result.usage.cache_creation_input_tokens, 2982);
    // §H.3 tier: 1h positive, 5m zero (the cost-claim smoking gun).
    assert_eq!(first_result.usage.ephemeral_1h_input_tokens, 2982);
    assert_eq!(first_result.usage.ephemeral_5m_input_tokens, 0);
}

/// A result with subtype "error" (or is_error true) is flagged.
#[test]
fn result_error_flagged() {
    let line = r#"{"type":"result","subtype":"error","is_error":true,"session_id":"s","stop_reason":"max_tokens"}"#;
    match parse_line(line.as_bytes()) {
        StreamEvent::Result(r) => {
            assert!(r.is_error);
            assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
        }
        other => panic!("expected Result, got {other:?}"),
    }
    // subtype success but is_error true → still error (defensive OR).
    let line2 = r#"{"type":"result","subtype":"success","is_error":true,"session_id":"s"}"#;
    assert!(matches!(parse_line(line2.as_bytes()), StreamEvent::Result(r) if r.is_error));
}

/// assistant with non-text blocks (tool_use / thinking) contributes no text.
#[test]
fn assistant_non_text_blocks_yield_empty_content() {
    let line = r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]}}"#;
    match parse_line(line.as_bytes()) {
        StreamEvent::Assistant { content, .. } => assert_eq!(content, ""),
        other => panic!("expected Assistant, got {other:?}"),
    }
    // Mixed: text + thinking → only the text is kept.
    let line2 = r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"answer"}]}}"#;
    assert!(
        matches!(parse_line(line2.as_bytes()), StreamEvent::Assistant { content, .. } if content == "answer")
    );
}

// ---------------------------------------------------------------------------
// Turn-aborted (killed turn) modeling.
// ---------------------------------------------------------------------------

/// A killed turn (real capture: system/init only, newline-terminated, no
/// result) → events present, finish() = Aborted.
#[test]
fn killed_turn_is_aborted() {
    let bytes = fixture("killed_init_only.ndjson");
    let (evs, outcome) = parse_whole(&bytes);
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], StreamEvent::SystemInit { .. }));
    assert_eq!(
        outcome,
        TurnOutcome::Aborted,
        "init-only kill = turn-aborted"
    );
}

/// Abrupt EOF mid-line on kill: a valid prefix ending mid-record. The partial
/// is discarded; the completed prefix's events stand; no `result` → Aborted.
#[test]
fn abrupt_eof_midline_discards_tail() {
    let mut s = String::new();
    s.push_str(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    s.push('\n');
    s.push_str(
        r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"text","text":"par"#,
    ); // torn mid-string, no \n
    let (evs, outcome) = parse_whole(s.as_bytes());
    assert_eq!(evs.len(), 1, "only the terminated init survives: {evs:?}");
    assert!(matches!(evs[0], StreamEvent::SystemInit { .. }));
    assert_eq!(outcome, TurnOutcome::Aborted);
}

// ---------------------------------------------------------------------------
// §H.5 — bounded buffer + hard memory cap + circuit-breaker.
// ---------------------------------------------------------------------------

/// A single oversize (never-terminating) line trips the circuit-breaker; the
/// buffer NEVER exceeds the cap.
#[test]
fn circuit_breaker_on_oversize_line() {
    let cap = 1024usize;
    let mut p = StreamParser::with_cap(cap);
    // Feed 4 KB with no newline, 256 bytes at a time; assert buffer stays <= cap
    // and a CircuitBreaker fires before the buffer could exceed it.
    let blob = vec![b'x'; 4096];
    let mut got_cb = false;
    for ch in blob.chunks(256) {
        let evs = p.push(ch);
        assert!(
            p.buffered_len() <= cap,
            "buffer {} exceeded cap {cap}",
            p.buffered_len()
        );
        if evs
            .iter()
            .any(|e| matches!(e, StreamEvent::CircuitBreaker { .. }))
        {
            got_cb = true;
            break;
        }
    }
    assert!(got_cb, "circuit-breaker must fire on the oversize line");
    assert!(p.is_tripped());
    // After tripping, the parser is poisoned: no further events, buffer empty.
    assert_eq!(p.buffered_len(), 0);
    assert!(
        p.push(b"anything\n").is_empty(),
        "poisoned parser emits nothing"
    );
}

/// The circuit-breaker also fires when a COMPLETED line (with its `\n`) exceeds
/// the cap — not only the never-terminating case.
#[test]
fn circuit_breaker_on_oversize_completed_line() {
    let cap = 1000usize;
    let mut p = StreamParser::with_cap(cap);
    let mut blob = vec![b'x'; 2000];
    blob.push(b'\n'); // a completed but oversize line
    let evs = p.push(&blob);
    assert!(
        matches!(evs.last(), Some(StreamEvent::CircuitBreaker { cap_bytes, observed_bytes })
            if *cap_bytes == cap && *observed_bytes >= 2000),
        "expected circuit-breaker with cap/observed, got {evs:?}"
    );
    assert!(p.is_tripped());
}

/// A line exactly AT the cap is fine (boundary): cap is the max admissible, not
/// the first-rejected.
#[test]
fn line_exactly_at_cap_is_admitted() {
    // Build a minimal valid JSON line and pick a cap == its length.
    let line = r#"{"type":"rate_limit_event","session_id":"s"}"#;
    let cap = line.len();
    let mut p = StreamParser::with_cap(cap);
    let mut data = line.as_bytes().to_vec();
    data.push(b'\n');
    let evs = p.push(&data);
    assert_eq!(
        evs,
        vec![StreamEvent::RateLimit {
            session_id: "s".into()
        }]
    );
    assert!(!p.is_tripped(), "a line exactly at cap must NOT trip");
}

/// The >64 KB single event is handled WITHIN the default cap (16 MiB) — the
/// "design for hundreds of KB" requirement — and the buffer is bounded.
#[test]
fn big_event_within_default_cap() {
    let bytes = fixture("big_result_over_64k.ndjson");
    let mut p = StreamParser::new();
    let mut max_buf = 0usize;
    for ch in bytes.chunks(4096) {
        p.push(ch);
        max_buf = max_buf.max(p.buffered_len());
        assert!(p.buffered_len() <= DEFAULT_MAX_LINE_BYTES);
    }
    assert_eq!(p.finish(), TurnOutcome::Completed);
    assert!(max_buf > 64 * 1024, "the >64KB line was actually buffered");
    assert!(!p.is_tripped());
}

// ---------------------------------------------------------------------------
// Fuzz: malformed / truncated / oversized / embedded-newline / random bytes.
// ---------------------------------------------------------------------------

/// Malformed JSON lines never crash; they surface as parse_error Others. Valid
/// framing (the `\n`) still works around them.
#[test]
fn fuzz_malformed_json_never_crashes() {
    let cases: &[&str] = &[
        "{torn",
        "not json at all",
        "{\"type\":}",
        "[1,2,3]",           // valid JSON, not an object → type_ None → Other
        "\"a bare string\"", // valid JSON scalar → Other
        "123",
        "null",
        "{\"type\":123}", // type is not a string
        "{}",             // empty object → Other(kind=None)
    ];
    for c in cases {
        let line = format!("{c}\n");
        let mut p = StreamParser::new();
        let evs = p.push(line.as_bytes());
        assert_eq!(evs.len(), 1, "case {c:?} -> {evs:?}");
        assert!(
            matches!(evs[0], StreamEvent::Other { .. }),
            "case {c:?} must be Other, got {:?}",
            evs[0]
        );
    }
}

/// Deterministic pseudo-random byte fuzz at every chunk boundary: the parser
/// never panics and never exceeds its cap, for arbitrary bytes (incl. embedded
/// raw newlines), across many seeds.
#[test]
fn fuzz_random_bytes_never_panic_never_overflow() {
    let cap = 4096usize;
    // A tiny deterministic LCG — no external dep, reproducible.
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };
    for _ in 0..200 {
        let len = (next() % 20000) as usize;
        let mut blob = Vec::with_capacity(len);
        for _ in 0..len {
            // Bias toward newlines and JSON punctuation so we hit framing paths.
            let b = match next() % 8 {
                0 | 1 => b'\n',
                2 => b'{',
                3 => b'}',
                4 => b'"',
                _ => (next() & 0xff) as u8,
            };
            blob.push(b);
        }
        let chunk = ((next() % 64) + 1) as usize;
        let mut p = StreamParser::with_cap(cap);
        for ch in blob.chunks(chunk) {
            let _ = p.push(ch); // must not panic
            assert!(p.buffered_len() <= cap, "cap breached during fuzz");
        }
        let _ = p.finish(); // must not panic
    }
}

/// Property: re-feeding any fixture in two halves split at EVERY byte boundary
/// equals feeding it whole (a split-point sweep — the torn-anywhere property).
#[test]
fn property_split_at_every_boundary() {
    for name in ["multiturn_keepalive.ndjson", "killed_init_only.ndjson"] {
        let bytes = fixture(name);
        let (whole, whole_out) = parse_whole(&bytes);
        for split in 0..=bytes.len() {
            let mut p = StreamParser::new();
            let mut evs = p.push(&bytes[..split]);
            evs.extend(p.push(&bytes[split..]));
            let out = p.finish();
            assert_eq!(evs, whole, "{name}: split at {split} mismatch");
            assert_eq!(out, whole_out, "{name}: split at {split} outcome");
        }
    }
}

// ---------------------------------------------------------------------------
// Embedded-newline assumption — verified against the REAL captures (§DoD).
// ---------------------------------------------------------------------------

/// The framing assumption: in valid claude stream-json, a literal `\n` ONLY
/// occurs between records (newlines inside string values are escaped `\\n`).
/// Verify it directly against the real captures: splitting each non-empty line
/// on `\n` yields exactly one valid-JSON record per line. AND a hypothetical
/// RAW embedded newline is handled safely (frames early; the malformed half is
/// a parse_error Other, never a panic).
#[test]
fn embedded_newline_assumption_holds_on_real_captures() {
    for name in [
        "multiturn_keepalive.ndjson",
        "large_events.ndjson",
        "killed_init_only.ndjson",
    ] {
        let bytes = fixture(name);
        for line in bytes.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            // Every framed line is a single valid JSON value (no raw newline
            // split a record into invalid halves).
            let v: serde_json::Result<serde_json::Value> = serde_json::from_slice(line);
            assert!(
                v.is_ok(),
                "{name}: a framed line did not parse as one JSON value — \
                 the embedded-newline assumption is violated"
            );
        }
    }
}

/// Hypothetical RAW embedded newline inside a string value: the parser frames
/// at the raw `\n` (correct per the framing rule) and the two halves surface as
/// parse_error Others — NO panic, NO unbounded growth.
#[test]
fn hypothetical_raw_embedded_newline_handled_safely() {
    // A record with a RAW newline inside the "text" value (invalid JSON if not
    // escaped). Our framer splits on the raw `\n`; both halves are malformed.
    let s = "{\"type\":\"assistant\",\"text\":\"line1\nline2\"}\n";
    let (evs, _out) = parse_whole(s.as_bytes());
    assert_eq!(evs.len(), 2, "framed into two halves: {evs:?}");
    assert!(
        evs.iter().all(|e| matches!(
            e,
            StreamEvent::Other {
                parse_error: true,
                ..
            }
        )),
        "both halves are malformed-but-framed Others, no panic: {evs:?}"
    );
}

// ---------------------------------------------------------------------------
// Corpus — table-driven (the §DoD corpus list).
// ---------------------------------------------------------------------------

struct CorpusCase {
    name: &'static str,
    fixture: &'static str,
    expect_outcome: TurnOutcome,
    min_inits: usize,
    min_results: usize,
}

#[test]
fn corpus_table_driven() {
    let cases = [
        // A real multi-event turn sequence: init->assistant->[rate_limit]->result.
        CorpusCase {
            name: "real multi-event turns (keep-alive)",
            fixture: "multiturn_keepalive.ndjson",
            expect_outcome: TurnOutcome::Completed,
            min_inits: 3,
            min_results: 3,
        },
        // A real large-event turn (>23 KB single events; multi assistant blocks).
        CorpusCase {
            name: "real large events (>23KB lines)",
            fixture: "large_events.ndjson",
            expect_outcome: TurnOutcome::Completed,
            min_inits: 1,
            min_results: 1,
        },
        // A synthetic >64 KB single event.
        CorpusCase {
            name: "synthetic >64KB single result",
            fixture: "big_result_over_64k.ndjson",
            expect_outcome: TurnOutcome::Completed,
            min_inits: 1,
            min_results: 1,
        },
        // A killed turn: init only, no result -> turn-aborted.
        CorpusCase {
            name: "killed turn (init only)",
            fixture: "killed_init_only.ndjson",
            expect_outcome: TurnOutcome::Aborted,
            min_inits: 1,
            min_results: 0,
        },
    ];
    for c in cases {
        let bytes = fixture(c.fixture);
        // Each corpus case is also checked under whole-feed AND a torn EOF tail
        // (append a partial unterminated record) AND a multi-event single read.
        let (evs, outcome) = parse_whole(&bytes);
        let inits = evs
            .iter()
            .filter(|e| matches!(e, StreamEvent::SystemInit { .. }))
            .count();
        let results = evs
            .iter()
            .filter(|e| matches!(e, StreamEvent::Result(_)))
            .count();
        assert_eq!(outcome, c.expect_outcome, "{}: outcome", c.name);
        assert!(inits >= c.min_inits, "{}: inits {inits}", c.name);
        assert!(results >= c.min_results, "{}: results {results}", c.name);

        // Torn EOF tail variant: append an unterminated partial record. The
        // events must be identical to the whole feed (the tail is discarded);
        // a stream that was Completed becomes Aborted only if its trailing
        // result is the torn one — here we append AFTER a full stream, so a
        // Completed stream's events are unchanged but the new in-flight (tail)
        // turn has no result.
        let mut torn = bytes.clone();
        torn.extend_from_slice(br#"{"type":"system","subtype":"init","session_id":"x"#);
        let (torn_evs, _torn_out) = parse_whole(&torn);
        assert_eq!(
            torn_evs, evs,
            "{}: torn EOF tail must not change the emitted events",
            c.name
        );
    }
}

// ---------------------------------------------------------------------------
// Latency micro-bench (timed loop; reports MB/s + ns/event). Printed under
// `--nocapture`; always asserts it completes so it runs in CI.
// ---------------------------------------------------------------------------

#[test]
fn latency_micro_bench() {
    // Concatenate the real multi-turn corpus many times into a big stream.
    let unit = fixture("multiturn_keepalive.ndjson");
    let reps = 2000usize;
    let mut stream = Vec::with_capacity(unit.len() * reps);
    for _ in 0..reps {
        stream.extend_from_slice(&unit);
    }
    let total_bytes = stream.len();

    // Warm up.
    {
        let mut p = StreamParser::new();
        let _ = p.push(&stream);
        let _ = p.finish();
    }

    // Bench: feed in 64 KiB chunks (the kernel pipe granularity), count events.
    let iters = 20usize;
    let mut event_count = 0usize;
    let start = std::time::Instant::now();
    for _ in 0..iters {
        let mut p = StreamParser::new();
        for ch in stream.chunks(65536) {
            event_count += p.push(ch).len();
        }
        let _ = p.finish();
    }
    let elapsed = start.elapsed();

    let total_processed = (total_bytes * iters) as f64;
    let secs = elapsed.as_secs_f64();
    let mb_s = total_processed / secs / (1024.0 * 1024.0);
    let ns_per_event = elapsed.as_nanos() as f64 / event_count as f64;
    println!(
        "stream_json parse bench: {:.1} MB/s, {:.1} ns/event ({} events over {} MiB in {:.3}s)",
        mb_s,
        ns_per_event,
        event_count,
        total_processed as u64 / (1024 * 1024),
        secs
    );
    assert!(event_count > 0);
    // Sanity floor (very loose — guards against an accidental O(n^2) regression
    // in reassembly): at least 5 MB/s on any plausible machine.
    assert!(
        mb_s > 5.0,
        "parse throughput {mb_s:.1} MB/s implausibly low"
    );
}
