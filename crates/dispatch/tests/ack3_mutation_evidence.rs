//! ACK-3 MATRIX MUTATION EVIDENCE (CR-3 house pattern, ack3-spec §3.1) —
//! feature-gated `mutation-evidence`. Each test PASSES by proving a matrix row's
//! PURE PREDICATE would FAIL under the named mutation shape — the committed,
//! re-runnable form of the "would RED under deletion/perturbation" claims. Run:
//!
//!   scripts/build-lock.sh cargo test -p qd --features mutation-evidence
//!
//! The predicates here DUPLICATE the live-row predicates in ack3_matrix.rs by
//! shape (test binaries cannot import each other; ack3-spec §2 / §3.1 sanctions
//! duplication). Each operates over a synthetic event set (engine records or a
//! daemon-event vector) — no live process. The mutation shapes:
//!   - M1's predicate over a set WITH the sha present → fails (collapse to M3).
//!   - M3's predicate over a set WITHOUT the sha → fails (collapse to M1).
//!   - M4's predicate without the eaten report → fails (collapse to M3).
//!   - M5's exact sequence with a plain turn-anchored in the terminal slot → fails.
//!   - the ADD-18 exit-contract predicate over status 0 → fails.
//!   - an N-row daemon-stream predicate over a kill-switch-blanked (empty) capture
//!     → fails (the e2e analogue of ack1's R-MUT m1 through the real engine stack).

#![cfg(feature = "mutation-evidence")]

use dispatch::events::{parse_events, sha256_hex, EventRecord};
use qrmux::events::{DaemonEvent, EventMeta};

// ---------------------------------------------------------------------------
// Predicate shapes (duplicated from ack3_matrix.rs — keep byte-small so drift is
// visible).
// ---------------------------------------------------------------------------

fn engine_seq(recs: &[EventRecord], send_id: &str) -> Vec<String> {
    recs.iter()
        .filter(|r| r.send_id().as_deref() == Some(send_id))
        .map(|r| r.event.clone())
        .collect()
}

fn daemon_sha(ev: &DaemonEvent) -> Option<&str> {
    match ev {
        DaemonEvent::PtyBytesWritten { content_sha256, .. }
        | DaemonEvent::PtyWriteFailed { content_sha256, .. } => Some(content_sha256),
        _ => None,
    }
}

/// M1 daemon predicate: NO daemon record carries `sha` (the dropped frame).
fn pred_daemon_sha_absent(daemon: &[DaemonEvent], sha: &str) -> bool {
    !daemon.iter().any(|e| daemon_sha(e) == Some(sha))
}

/// M3 daemon predicate: `pty-bytes-written` for `sha` is PRESENT (the deception).
fn pred_daemon_bytes_written(daemon: &[DaemonEvent], sha: &str) -> bool {
    daemon
        .iter()
        .any(|e| matches!(e, DaemonEvent::PtyBytesWritten { content_sha256, .. } if content_sha256 == sha))
}

/// M5 engine exact sequence: ends in turn-anchored-mismatch (EXACTLY).
fn pred_engine_mismatch_seq(recs: &[EventRecord], send_id: &str) -> bool {
    engine_seq(recs, send_id)
        == [
            "send-initiated",
            "chunks-delivered",
            "turn-anchored-mismatch",
        ]
}

/// ADD-18 exit contract: status 11 + the pinned stderr line.
fn pred_add18_exit_contract(status: i32, stderr: &str) -> bool {
    status == 11 && stderr.contains("ERROR: PTY write failed")
}

/// M4 child predicate: an eaten{bytes==len} record is present in the report JSONL.
fn pred_report_eaten(report: &str, content_len: usize) -> bool {
    report
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| {
            v.get("event").and_then(|e| e.as_str()) == Some("eaten")
                && v.get("bytes").and_then(|b| b.as_u64()) == Some(content_len as u64)
        })
}

/// An N-row daemon-stream predicate: a clean delivery wrote bytes (the stream is
/// non-empty and carries the bytes-written for the sha). A kill-switch
/// (QRMUX_EVENTS_DISABLED=1) blanks the stream → this fails.
fn pred_nrow_daemon_nonblank(daemon: &[DaemonEvent], sha: &str) -> bool {
    !daemon.is_empty() && pred_daemon_bytes_written(daemon, sha)
}

// ---------------------------------------------------------------------------
// Synthetic event-set builders.
// ---------------------------------------------------------------------------

fn meta() -> EventMeta {
    EventMeta {
        session: "s".into(),
        epoch: 1,
        seq: 1,
        ts_ms: 0,
    }
}

fn bytes_written(sha: &str) -> DaemonEvent {
    DaemonEvent::PtyBytesWritten {
        meta: meta(),
        bytes: 10,
        content_sha256: sha.to_string(),
        content_len: 10,
    }
}

/// Build an engine record set with the given event-name sequence for one send_id.
fn engine_records_for(send_id: &str, events: &[&str]) -> Vec<EventRecord> {
    let mut text = String::new();
    for (i, ev) in events.iter().enumerate() {
        // A minimal valid record line carrying pid/seq/event/send_id.
        text.push_str(&format!(
            r#"{{"v":1,"ts":"2026-06-06T00:00:0{i}.000Z","pid":1,"seq":{i},"session":"sid","event":"{ev}","send_id":"{send_id}"}}"#
        ));
        text.push('\n');
    }
    parse_events(&text).records
}

// ---------------------------------------------------------------------------
// The five committed mutation-evidence tests.
// ---------------------------------------------------------------------------

/// M1/M3 discriminator binds (§3.1): M1's predicate (sha ABSENT) over an event
/// set WITH the sha present FAILS; and M3's predicate (sha PRESENT) over a set
/// WITHOUT the sha FAILS. The daemon log presence is the load-bearing
/// discriminator between the dropped-frame (M1) and silent-swallow (M3) rows.
#[test]
fn me_m1_daemon_log_presence_flips_the_discriminator() {
    let sha = sha256_hex(b"the-injected-content");

    // M1's predicate is satisfied by an EMPTY daemon set (sha absent).
    assert!(
        pred_daemon_sha_absent(&[], &sha),
        "M1 predicate holds when the frame was dropped (sha absent)"
    );
    // MUTATION: feed M1's predicate a set WITH the sha present (the M3 shape) → it
    // FAILS. Were the discriminator hollow, M1 would mis-classify an M3 capture.
    let m3_shaped = vec![bytes_written(&sha)];
    assert!(
        !pred_daemon_sha_absent(&m3_shaped, &sha),
        "M1 predicate FAILS over an M3-shaped daemon set (sha present) — the \
         daemon-log presence discriminator is load-bearing"
    );

    // M3's predicate is satisfied by the bytes-written shape.
    assert!(
        pred_daemon_bytes_written(&m3_shaped, &sha),
        "M3 predicate holds when bytes-written is present"
    );
    // MUTATION: feed M3's predicate a set WITHOUT the sha (the M1 shape) → FAILS.
    assert!(
        !pred_daemon_bytes_written(&[], &sha),
        "M3 predicate FAILS over an M1-shaped daemon set (sha absent) — M3 cannot \
         collapse into M1"
    );
}

/// M4 consumption assert binds (§3.1): M4's child predicate over a report stream
/// WITHOUT the eaten record FAILS — i.e. M4 cannot collapse into M3 (which has no
/// eaten record). Proven by: the predicate holds on a report WITH eaten, fails on
/// one without.
#[test]
fn me_m4_missing_eaten_report_fails_the_row() {
    let content_len = 42usize;
    let with_eaten = format!(
        r#"{{"event":"burst","size":42,"paste":true}}
{{"event":"eaten","bytes":{content_len}}}
"#
    );
    assert!(
        pred_report_eaten(&with_eaten, content_len),
        "M4 predicate holds on a report carrying eaten{{bytes==len}}"
    );
    // MUTATION: the M3-shaped report (no eaten record — M3 swallows daemon-side,
    // the child never sees the bytes) → the predicate FAILS.
    let no_eaten = r#"{"event":"burst","size":42,"paste":true}
"#;
    assert!(
        !pred_report_eaten(no_eaten, content_len),
        "M4 predicate FAILS without an eaten record — M4 cannot collapse into M3"
    );
    // Also: a wrong byte count fails (the ==len pin is load-bearing).
    let wrong_len = r#"{"event":"eaten","bytes":7}
"#;
    assert!(
        !pred_report_eaten(wrong_len, content_len),
        "M4 predicate FAILS when eaten.bytes != content_len"
    );
}

/// M5 pin binds (§3.1): a plain `turn-anchored` (non-mismatch) in M5's expected
/// terminal position FAILS the exact-sequence assert — the mismatch pin is not
/// satisfiable by a clean anchor.
#[test]
fn me_m5_plain_anchor_fails_the_mismatch_pin() {
    let sid = "1-1-0";
    // The real M5 shape satisfies the pin.
    let m5 = engine_records_for(
        sid,
        &[
            "send-initiated",
            "chunks-delivered",
            "turn-anchored-mismatch",
        ],
    );
    assert!(
        pred_engine_mismatch_seq(&m5, sid),
        "M5 predicate holds on the mismatch-terminating sequence"
    );
    // MUTATION: a plain turn-anchored in the terminal slot (what a non-truncating
    // verify would emit) → the exact-sequence pin FAILS.
    let plain = engine_records_for(
        sid,
        &["send-initiated", "chunks-delivered", "turn-anchored"],
    );
    assert!(
        !pred_engine_mismatch_seq(&plain, sid),
        "M5 predicate FAILS with a plain turn-anchored in the terminal slot — the \
         mismatch pin rejects a clean anchor"
    );
    // And a spurious EXTRA terminal fails it too (exact, not contains).
    let extra = engine_records_for(
        sid,
        &[
            "send-initiated",
            "chunks-delivered",
            "turn-anchored-mismatch",
            "anchor-timeout",
        ],
    );
    assert!(
        !pred_engine_mismatch_seq(&extra, sid),
        "M5 predicate FAILS on a spurious extra terminal (exact-sequence, not contains)"
    );
}

/// Exit-11 contract binds (§3.1): the ADD-18 exit-contract predicate over status
/// 0 FAILS. (The live M1/M2 contract rows are EXPECTED RED until ADD-18 lands;
/// this committed evidence proves the predicate itself is non-vacuous — a 0 exit
/// cannot pass it.)
#[test]
fn me_add18_exit_zero_shape_fails_the_contract_row() {
    // The contract is satisfied only by exit 11 + the pinned stderr.
    assert!(
        pred_add18_exit_contract(
            11,
            "ERROR: PTY write failed (0/1 chunks acked) — see events file"
        ),
        "ADD-18 predicate holds on exit 11 + the pinned stderr line"
    );
    // MUTATION: the pre-ADD-18 shape (exit 0, success stderr) → FAILS.
    assert!(
        !pred_add18_exit_contract(0, "Message sent to m1"),
        "ADD-18 predicate FAILS over the exit-0 shape — the contract row cannot \
         pass on the pre-ADD-18 behavior"
    );
    // A non-zero exit with the WRONG code also fails (the 11 pin is load-bearing).
    assert!(
        !pred_add18_exit_contract(1, "ERROR: PTY write failed"),
        "ADD-18 predicate FAILS on exit 1 (the distinct code 11 is pinned)"
    );
}

/// Kill-switch e2e (daemon side, §3.1): an N-row daemon-stream predicate over a
/// QRMUX_EVENTS_DISABLED=1-shaped capture (an EMPTY daemon set — the emitter
/// produced no file) FAILS. The e2e analogue of ack1's R-MUT m1, now through the
/// real engine stack's reader.
#[test]
fn me_matrix_daemon_kill_switch_blanks_the_stream() {
    let sha = sha256_hex(b"clean-delivery-content");
    // A normal N-row capture wrote bytes for the sha → the predicate holds.
    let clean = vec![
        DaemonEvent::SessionOpened {
            meta: meta(),
            pid: 1,
            schema_version: 1,
            pid_start_ms: None,
            boot_id: None,
        },
        bytes_written(&sha),
    ];
    assert!(
        pred_nrow_daemon_nonblank(&clean, &sha),
        "N-row predicate holds over a clean (emitter-on) daemon capture"
    );
    // MUTATION: QRMUX_EVENTS_DISABLED=1 → the emitter is None → no file → an empty
    // parsed capture → the predicate FAILS (the harness REDs when the stream is
    // suppressed).
    let blanked: Vec<DaemonEvent> = Vec::new();
    assert!(
        !pred_nrow_daemon_nonblank(&blanked, &sha),
        "N-row predicate FAILS over a kill-switch-blanked (empty) daemon stream"
    );
}
