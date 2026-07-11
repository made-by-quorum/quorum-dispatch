//! ACK-2 M-1 MUTATION EVIDENCE (merge ruling, CR-3 reproducible-by-command) —
//! feature-gated `mutation-evidence`, the negative_control.rs house pattern.
//!
//! Each test PASSES by proving a gate row's assert WOULD FAIL under the named
//! mutation — the committed, re-runnable form of the gate report's "live-fired →
//! RED → reverted" claims. Run:
//!
//!   scripts/build-lock.sh cargo test -p quorum-dispatch --features mutation-evidence
//!
//! These two claims operate on EVENT-STREAM SHAPES through the public events API
//! (the row predicates from ack2_gate.rs applied to emitted-vs-deletion-shaped
//! streams); the two claims needing private schema access (G1 cap, G4 set
//! pollution) + the WatchGuard claim live in `events.rs`'s feature-gated tests.

#![cfg(feature = "mutation-evidence")]

use dispatch::effects::FixedClock;
use dispatch::events::{parse_events, sha256_hex, EventWriter, Payload};
use std::path::Path;

/// The event-name sequence for one send_id, file order — the SAME extraction
/// ack2_gate.rs's `seq_for` performs (duplicated here because integration tests
/// cannot import each other; kept byte-small so drift is visible).
fn seq_for(path: &Path, send_id: &str) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    parse_events(&text)
        .records
        .iter()
        .filter(|r| r.send_id().as_deref() == Some(send_id))
        .map(|r| r.event.clone())
        .collect()
}

/// Emit the send-initiated + chunks-delivered prefix every send path writes.
fn emit_prefix(w: &EventWriter, clock: &FixedClock, send_id: &str) {
    w.emit(
        clock,
        &Payload::SendInitiated {
            send_id: send_id.into(),
            verb: "send:pty".into(),
            send_path: "idle".into(),
            content_sha256: sha256_hex(b"m"),
            content_len: 1,
            chunks: 1,
            chunk_sha256s: vec![sha256_hex(b"m")],
            chunk_sha256s_capped: false,
            transcript: None,
            transcript_offset: None,
            content_preview: None,
        },
    )
    .unwrap();
    w.emit(
        clock,
        &Payload::ChunksDelivered {
            send_id: send_id.into(),
            chunks_acked: 1,
            ack_source: "input-sent".into(),
        },
    )
    .unwrap();
}

/// M-1 claim 3 (G3 wait-loop NON-foreclosure, INVERTED at amend rider 3): post
/// finding G the `WaitOutcome::TimedOut` arm mints NO terminal, so the G3 timeout
/// row's EXACT-sequence assert is now `[send-initiated, chunks-delivered]`. That is
/// satisfied by the real (non-foreclosing) stream and FAILED by the MUTATION-shaped
/// stream that RE-ADDS a foreclosing anchor-timeout — exactly what re-introducing the
/// arm's emission in send.rs would produce. Re-adding the emission REDs
/// `g3_seq_sendpty_wait_timeout_no_foreclosing_terminal`.
#[test]
fn me_g3_wait_timeout_readd_terminal_fails_exact_sequence() {
    let clock = FixedClock(1_781_241_549_123);
    // Post amend rider 3 the row forbids ANY terminal: the exact sequence is the
    // two-event prefix.
    let expected = vec!["send-initiated".to_string(), "chunks-delivered".to_string()];

    // The real (non-foreclosing) shape — the TimedOut arm emits no terminal.
    let dir_a = tempfile::tempdir().unwrap();
    let w_a = EventWriter::for_key(dir_a.path(), "sid-a", Some("sid-a".into()), None);
    emit_prefix(&w_a, &clock, "id-1");
    assert_eq!(
        seq_for(w_a.path(), "id-1"),
        expected,
        "the non-foreclosing stream satisfies the G3 timeout row"
    );

    // Mutation shape (a foreclosing anchor-timeout RE-ADDED at the arm).
    let dir_b = tempfile::tempdir().unwrap();
    let w_b = EventWriter::for_key(dir_b.path(), "sid-b", Some("sid-b".into()), None);
    emit_prefix(&w_b, &clock, "id-1");
    w_b.emit(
        &clock,
        &Payload::AnchorTimeout {
            send_id: "id-1".into(),
            waited_ms: 3000,
        },
    )
    .unwrap();
    assert_ne!(
        seq_for(w_b.path(), "id-1"),
        expected,
        "the mutation-shaped stream FAILS the exact-sequence assert — re-adding \
         the anchor-timeout emission REDs the G3 timeout row"
    );
}

/// M-1 claim 4 (priming-readiness-timeout deletion): the G7(c) row's predicate —
/// the byname events file exists and contains EXACTLY ONE
/// priming-readiness-timeout — is satisfied by the emitted output and FAILED by
/// the deletion-shaped output (no emission ⇒ the file is never created; the
/// live-fired RED was `byname events file written: NotFound`). Deleting the
/// `warn_emit` in `emit_priming_timeout` REDs `g7c_readiness_arm_…` and both
/// `priming_timeout_*` bin-unit rows.
#[test]
fn me_priming_deletion_shape_fails_byname_row() {
    let clock = FixedClock(1_781_241_549_123);
    let row_predicate = |state: &Path| -> bool {
        let path = dispatch::events::events_path(state, &dispatch::events::byname_key("wk"));
        match std::fs::read_to_string(&path) {
            Err(_) => false, // NotFound — the live-fired RED's exact mechanism
            Ok(text) => {
                parse_events(&text)
                    .records
                    .iter()
                    .filter(|r| r.event == "priming-readiness-timeout")
                    .count()
                    == 1
            }
        }
    };

    // Emitted output (what emit_priming_timeout writes).
    let dir_a = tempfile::tempdir().unwrap();
    let w = EventWriter::for_key(
        dir_a.path(),
        &dispatch::events::byname_key("wk"),
        None,
        Some("wk".into()),
    );
    w.emit(
        &clock,
        &Payload::PrimingReadinessTimeout {
            waited_ms: 40_000,
            phase: "pid-file".into(),
        },
    )
    .unwrap();
    assert!(
        row_predicate(dir_a.path()),
        "the emitted output satisfies the G7(c)/priming rows"
    );

    // Deletion shape: nothing emitted ⇒ no byname file ⇒ the predicate fails.
    let dir_b = tempfile::tempdir().unwrap();
    assert!(
        !row_predicate(dir_b.path()),
        "the deletion-shaped output FAILS the byname row (NotFound) — deleting \
         the priming emission REDs G7(c) and the priming_timeout_* rows"
    );
}
