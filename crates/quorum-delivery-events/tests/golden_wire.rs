//! Byte-identical wire GOLDEN (vocab-extract core proof).
//!
//! These golden strings were captured from the PRE-MOVE (frozen `e800d8d5`)
//! `dispatch::events::build_record_line` via a `#[cfg(test)]` harness that called
//! the (then-private) fn with a FIXED envelope (start_ms present) and a FIXED
//! sha_cap over one representative `Payload` per variant. This test reproduces
//! every one BYTE-FOR-BYTE through the extracted leaf-crate serializer, proving
//! the move preserved the wire format exactly.
//!
//! Coverage includes: all 19 variants; Option-None-omitted (`send-initiated`
//! without transcript/offset/preview); bool-false-omitted (`turn-anchored` with
//! recovered=false emits no `recovered` key); the Anchor-bearing `turn-anchored`;
//! and the `send-failed` send_id?-omitted vs present cases.
//!
//! Byte-exactness depends on serde_json `preserve_order` (default feature).

use quorum_delivery_events::{build_record_line, Anchor, Envelope, Payload, CHUNK_SHA_CAP};

fn golden_env() -> Envelope {
    Envelope {
        v: 1,
        ts: "2026-06-06T06:09:00.123Z".to_string(),
        pid: 71234,
        seq: 7,
        session: Some("11111111-2222-3333-4444-555555555555".to_string()),
        name: Some("alpha".to_string()),
        start_ms: Some(1_781_241_500_000),
    }
}

fn sha(bytes: &[u8]) -> String {
    quorum_delivery_events::sha256_hex(bytes)
}

/// (label, payload, expected_wire_line). The expected strings are the frozen
/// pre-move golden output.
fn cases() -> Vec<(&'static str, Payload, &'static str)> {
    vec![
        (
            "send-initiated",
            Payload::SendInitiated {
                send_id: "71234-1781241549123-0".to_string(),
                verb: "send:pty".to_string(),
                send_path: "idle".to_string(),
                content_sha256: sha(b"the message"),
                content_len: 11,
                chunks: 1,
                chunk_sha256s: vec![sha(b"the message")],
                chunk_sha256s_capped: false,
                transcript: Some("/path/to/transcript.jsonl".to_string()),
                transcript_offset: Some(4096),
                content_preview: Some("the message".to_string()),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"send-initiated","send_id":"71234-1781241549123-0","verb":"send:pty","send_path":"idle","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825","content_len":11,"chunks":1,"chunk_sha256s":["c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825"],"transcript":"/path/to/transcript.jsonl","transcript_offset":4096,"content_preview":"the message"}"#,
        ),
        (
            "send-initiated-omitted",
            Payload::SendInitiated {
                send_id: "71234-1781241549123-1".to_string(),
                verb: "new-p".to_string(),
                send_path: "busy-queued".to_string(),
                content_sha256: sha(b"m"),
                content_len: 1,
                chunks: 1,
                chunk_sha256s: vec![sha(b"m")],
                chunk_sha256s_capped: false,
                transcript: None,
                transcript_offset: None,
                content_preview: None,
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"send-initiated","send_id":"71234-1781241549123-1","verb":"new-p","send_path":"busy-queued","content_sha256":"62c66a7a5dd70c3146618063c344e531e6d4b59e379808443ce962b3abd63c5a","content_len":1,"chunks":1,"chunk_sha256s":["62c66a7a5dd70c3146618063c344e531e6d4b59e379808443ce962b3abd63c5a"]}"#,
        ),
        (
            "chunks-delivered",
            Payload::ChunksDelivered {
                send_id: "71234-1781241549123-2".to_string(),
                chunks_acked: 3,
                ack_source: "input-sent".to_string(),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"chunks-delivered","send_id":"71234-1781241549123-2","chunks_acked":3,"ack_source":"input-sent"}"#,
        ),
        (
            "turn-anchored",
            Payload::TurnAnchored {
                send_id: "71234-1781241549123-3".to_string(),
                content_sha256: sha(b"the message"),
                anchor: Anchor {
                    transcript: "/path/to/transcript.jsonl".to_string(),
                    start_offset: 4096,
                    line_index: 42,
                },
                recovered: false,
                attribution: None,
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"turn-anchored","send_id":"71234-1781241549123-3","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825","anchor":{"transcript":"/path/to/transcript.jsonl","start_offset":4096,"line_index":42}}"#,
        ),
        (
            "turn-anchored-recovered",
            Payload::TurnAnchored {
                send_id: "71234-1781241549123-4".to_string(),
                content_sha256: sha(b"the message"),
                anchor: Anchor {
                    transcript: "/path/to/transcript.jsonl".to_string(),
                    start_offset: 8192,
                    line_index: 43,
                },
                recovered: true,
                attribution: Some("offset".to_string()),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"turn-anchored","send_id":"71234-1781241549123-4","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825","anchor":{"transcript":"/path/to/transcript.jsonl","start_offset":8192,"line_index":43},"recovered":true,"attribution":"offset"}"#,
        ),
        (
            "turn-anchored-mismatch",
            Payload::TurnAnchoredMismatch {
                send_id: "71234-1781241549123-5".to_string(),
                expected_sha: sha(b"x"),
                actual_sha: sha(b"y"),
                expected_len: 100,
                actual_len: 90,
                recovered: false,
                attribution: None,
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"turn-anchored-mismatch","send_id":"71234-1781241549123-5","expected_sha":"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881","actual_sha":"a1fce4363854ff888cff4b8e7875d600c2682390412a8cf79b37d0b11148b0fa","expected_len":100,"actual_len":90}"#,
        ),
        (
            "anchor-timeout",
            Payload::AnchorTimeout {
                send_id: "71234-1781241549123-6".to_string(),
                waited_ms: 30000,
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"anchor-timeout","send_id":"71234-1781241549123-6","waited_ms":30000}"#,
        ),
        (
            "pending-abandoned",
            Payload::PendingAbandoned {
                send_id: "71234-1781241549123-7".to_string(),
                reason: "watch-interrupted".to_string(),
                recovered: None,
                attribution: None,
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"pending-abandoned","send_id":"71234-1781241549123-7","reason":"watch-interrupted"}"#,
        ),
        (
            "pending-abandoned-recovered",
            Payload::PendingAbandoned {
                send_id: "71234-1781241549123-8".to_string(),
                reason: "recovery-no-candidate".to_string(),
                recovered: Some(true),
                attribution: Some("time-window".to_string()),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"pending-abandoned","send_id":"71234-1781241549123-8","reason":"recovery-no-candidate","recovered":true,"attribution":"time-window"}"#,
        ),
        (
            "composer-cleared",
            Payload::ComposerCleared {
                send_id: "71234-1781241549123-9".to_string(),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"composer-cleared","send_id":"71234-1781241549123-9"}"#,
        ),
        (
            "priming-readiness-timeout",
            Payload::PrimingReadinessTimeout {
                waited_ms: 15000,
                phase: "pid-file".to_string(),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"priming-readiness-timeout","waited_ms":15000,"phase":"pid-file"}"#,
        ),
        (
            "status-transition",
            Payload::StatusTransition {
                status: "busy".to_string(),
                source: "status-file-poll".to_string(),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"status-transition","status":"busy","source":"status-file-poll"}"#,
        ),
        (
            "events-truncated",
            Payload::EventsTruncated,
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"events-truncated"}"#,
        ),
        (
            "relay-delivered",
            Payload::RelayDelivered {
                send_id: "relay-1781241549123-7".to_string(),
                content_sha256: sha(b"the message"),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"relay-delivered","send_id":"relay-1781241549123-7","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825"}"#,
        ),
        (
            "turn-accepted",
            Payload::TurnAccepted {
                send_id: "turn-42".to_string(),
                content_sha256: sha(b"the message"),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"turn-accepted","send_id":"turn-42","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825"}"#,
        ),
        (
            "message-seen",
            Payload::MessageSeen {
                send_id: "71234-1781241549123-10".to_string(),
                content_sha256: sha(b"the message"),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"message-seen","send_id":"71234-1781241549123-10","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825"}"#,
        ),
        (
            "seen-failed",
            Payload::SeenFailed {
                send_id: "71234-1781241549123-11".to_string(),
                reason: "recipient-gone".to_string(),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"seen-failed","send_id":"71234-1781241549123-11","reason":"recipient-gone"}"#,
        ),
        (
            "send-failed",
            Payload::SendFailed {
                send_id: None,
                content_sha256: sha(b"the message"),
                reason: "no-relay".to_string(),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"send-failed","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825","reason":"no-relay"}"#,
        ),
        (
            "send-failed-with-id",
            Payload::SendFailed {
                send_id: Some("relay-1781241549123-9".to_string()),
                content_sha256: sha(b"the message"),
                reason: "transport-error".to_string(),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"send-failed","send_id":"relay-1781241549123-9","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825","reason":"transport-error"}"#,
        ),
        (
            "rung-entered",
            Payload::RungEntered {
                session_id: "sess-1".to_string(),
                rung: "pidfd-signal".to_string(),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"rung-entered","session_id":"sess-1","rung":"pidfd-signal"}"#,
        ),
        (
            "rung-succeeded",
            Payload::RungSucceeded {
                session_id: "sess-1".to_string(),
                rung: "control-wake".to_string(),
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"rung-succeeded","session_id":"sess-1","rung":"control-wake"}"#,
        ),
        (
            "rung-timeout",
            Payload::RungTimeout {
                session_id: "sess-1".to_string(),
                rung: "pty-inject".to_string(),
                waited_ms: 5000,
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"rung-timeout","session_id":"sess-1","rung":"pty-inject","waited_ms":5000}"#,
        ),
        (
            "recovery-crit",
            Payload::RecoveryCrit {
                session_id: "sess-1".to_string(),
                consecutive_failures: 3,
            },
            r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"recovery-crit","session_id":"sess-1","consecutive_failures":3}"#,
        ),
    ]
}

#[test]
fn wire_bytes_match_frozen_golden() {
    let env = golden_env();
    // Assert every event_name is exercised: 19 distinct variants.
    let mut seen_kinds = std::collections::BTreeSet::new();
    for (label, payload, expected) in cases() {
        let got = build_record_line(&env, &payload, CHUNK_SHA_CAP);
        assert_eq!(
            got, expected,
            "byte-identical wire mismatch for case `{label}`:\n  got: {got}\n  exp: {expected}"
        );
        seen_kinds.insert(payload.event_name());
    }
    assert_eq!(
        seen_kinds.len(),
        19,
        "must exercise all 19 Payload variants; saw {:?}",
        seen_kinds
    );
}

/// `is_success_terminal` is the ONE home for the "which terminal means delivered"
/// identity (F5/M2 de-dup). Exactly `message-seen`; every other terminal (failure/
/// mismatch/abandon) and every non-terminal is NOT success. A success terminal is
/// always a terminal.
#[test]
fn is_success_terminal_membership() {
    use quorum_delivery_events::{is_success_terminal, is_terminal, TERMINAL_EVENTS};

    assert!(is_success_terminal("message-seen"));
    assert!(is_terminal("message-seen"), "success ⇒ terminal");

    // Every OTHER terminal is a non-success terminal (failure/mismatch/abandon).
    for &t in TERMINAL_EVENTS.iter().filter(|&&t| t != "message-seen") {
        assert!(
            !is_success_terminal(t),
            "`{t}` is a terminal but NOT the success terminal"
        );
    }
    // Non-terminals (incl. the cheap-event trap names) are never success.
    for e in ["chunks-delivered", "composer-cleared", "status-transition", "relay-delivered", ""] {
        assert!(!is_success_terminal(e), "`{e}` is not a success terminal");
    }
    // The whole terminal set: exactly one success member.
    assert_eq!(
        TERMINAL_EVENTS.iter().filter(|t| is_success_terminal(t)).count(),
        1,
        "exactly one success terminal in the 7-set"
    );
}
