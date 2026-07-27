//! attended/tests_wired.rs — the WIRED-server deterministic gates.
//!
//! These prove the *wired* paths (the fire engine + the reconcile sweep, driving
//! the pure state machine) end-to-end at the M1 quality bar, WITHOUT a live tokio
//! task or a real PTY (M5's adversarial battery owns fire-over-a-real-PTY). Two
//! things they establish:
//!
//! 1. **End-to-end byte-identity** — every terminal the wired paths PRODUCE, when
//!    serialized through the shared vocabulary's `build_record_line`
//!    (`preserve_order` ON), is BYTE-IDENTICAL to the frozen golden. This closes
//!    BUILD-DIRECTIVES 2b end-to-end: the emitter's own golden proves `build_line`;
//!    these prove the *fire*/*reconcile* code constructs golden-serializing payloads.
//! 2. **Composition** — journal → timer decision → fire resolves as one flow.

use quorum_delivery_events::{sha256_hex, Envelope};

use super::emitter::build_line;
use super::fire::{
    fire, FireConfig, FireEffects, FireOutcome, LandingProbe, LandingScan, SafeDefaultFacts,
};
use super::spool::{PendingRecord, Spool};
use super::sweep::verdict_payload;
use super::{evaluate, FireDecision, InputLock, Journal, ReconcileVerdict};
use quorum_delivery_events::Payload;
use quorum_submit_discipline::{SubmitOptions, PROMPT_GLYPH};
use std::sync::Mutex;

// The SAME fixed envelope + golden lines as
// `quorum-delivery-events/tests/golden_wire.rs` and `attended::emitter`'s golden,
// so a break here is the SAME immediate-flag signal.
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

const GOLDEN_MESSAGE_SEEN: &str = r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"message-seen","send_id":"71234-1781241549123-10","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825"}"#;
const GOLDEN_SEND_FAILED: &str = r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"send-failed","send_id":"71234-1781241549123-12","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825","reason":"verify-blocked"}"#;
const GOLDEN_SEEN_FAILED: &str = r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"seen-failed","send_id":"71234-1781241549123-11","reason":"recipient-gone"}"#;
const GOLDEN_PENDING_ABANDONED: &str = r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"pending-abandoned","send_id":"71234-1781241549123-7","reason":"unknown-inject-outcome"}"#;

/// A minimal live-effects fake: fixed screen + status, writes ignored, fixed clock.
struct Fx {
    screen: String,
    status: Option<String>,
}
impl FireEffects for Fx {
    fn send_text(&self, _t: &str) {}
    fn send_cr(&self) {}
    fn write_raw(&self, _b: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn read_screen(&self) -> String {
        self.screen.clone()
    }
    fn read_status(&self) -> Option<String> {
        self.status.clone()
    }
    fn acceptance_confirmable(&self) -> bool {
        // These wired byte-identity tests drive the FULL fire (claude-shaped,
        // confirmable) to exercise the real terminal emitters — never the F1 gate.
        true
    }
    fn sleep(&self, _ms: u64) {}
    fn now_ms(&self) -> i64 {
        0
    }
}

struct FixedProbe(LandingScan);
impl LandingProbe for FixedProbe {
    fn scan(&self, _t: Option<&str>, _o: Option<u64>, _m: &str) -> LandingScan {
        self.0.clone()
    }
}

fn cfg_fast() -> FireConfig {
    FireConfig {
        verify_attempts: 1,
        verify_retry_ms: 0,
        landing_window_ms: 0,
        landing_poll_ms: 0,
        submit: SubmitOptions {
            settle_ms: 0,
            post_cr_ms: 0,
            poll_ms: 0,
        },
    }
}

fn golden_record(send_id: &str) -> PendingRecord {
    PendingRecord::accepted(
        send_id,
        sha256_hex(b"the message"),
        "the message".len() as u64,
        Some("11111111-2222-3333-4444-555555555555".into()),
        Some("alpha".into()),
        "send:pty",
        false,
        0,
    )
}

fn scratch_spool() -> (tempfile::TempDir, Spool) {
    let dir = tempfile::tempdir().unwrap();
    let spool = Spool::open(dir.path().join("pending")).unwrap();
    (dir, spool)
}

/// Assert a wired-path payload serializes BYTE-IDENTICAL to its frozen golden
/// through the shared `build_record_line` (preserve_order ON).
fn assert_golden(payload: &Payload, golden: &str) {
    let got = build_line(&golden_env(), payload);
    assert_eq!(
        got, golden,
        "WIRED-PATH BYTE-IDENTITY BREAK (immediate flag):\n got: {got}\n exp: {golden}"
    );
}

// ---- Fire path: message-seen is byte-identical end-to-end -------------------

#[test]
fn wired_fire_message_seen_is_byte_identical_to_golden() {
    let fx = Fx {
        screen: format!("{PROMPT_GLYPH} the message"),
        status: Some("busy".into()),
    };
    let (_d, spool) = scratch_spool();
    let lock = Mutex::new(InputLock::new());
    let journal = Mutex::new(Journal::new());
    let out = fire(
        &fx,
        &SafeDefaultFacts,
        &FixedProbe(LandingScan::Landed),
        &lock,
        &journal,
        &spool,
        golden_record("71234-1781241549123-10"),
        "the message",
        &cfg_fast(),
    );
    match out {
        FireOutcome::Terminal(p) => assert_golden(&p, GOLDEN_MESSAGE_SEEN),
        other => panic!("expected message-seen terminal, got {other:?}"),
    }
}

// ---- Fire path: send-failed{verify-blocked} is byte-identical ---------------

#[test]
fn wired_fire_send_failed_is_byte_identical_to_golden() {
    // No prompt glyph → SafeDefaultFacts cannot verify plain → verify-blocked.
    let fx = Fx {
        screen: "a modal, no composer glyph".into(),
        status: Some("idle".into()),
    };
    let (_d, spool) = scratch_spool();
    let lock = Mutex::new(InputLock::new());
    let journal = Mutex::new(Journal::new());
    let out = fire(
        &fx,
        &SafeDefaultFacts,
        &FixedProbe(LandingScan::Landed),
        &lock,
        &journal,
        &spool,
        golden_record("71234-1781241549123-12"),
        "the message",
        &cfg_fast(),
    );
    match out {
        FireOutcome::Terminal(p) => assert_golden(&p, GOLDEN_SEND_FAILED),
        other => panic!("expected send-failed terminal, got {other:?}"),
    }
}

// ---- Reconcile path: seen-failed / pending-abandoned are byte-identical ------

#[test]
fn wired_reconcile_seen_failed_is_byte_identical_to_golden() {
    let payload = verdict_payload(
        &ReconcileVerdict::SeenFailedRecipientGone,
        &golden_record("71234-1781241549123-11"),
    )
    .expect("seen-failed emits a terminal");
    assert_golden(&payload, GOLDEN_SEEN_FAILED);
}

#[test]
fn wired_reconcile_pending_abandoned_is_byte_identical_to_golden() {
    let payload = verdict_payload(
        &ReconcileVerdict::PendingAbandonedUnknown,
        &golden_record("71234-1781241549123-7"),
    )
    .expect("pending-abandoned emits a terminal");
    assert_golden(&payload, GOLDEN_PENDING_ABANDONED);
}

// ---- Composition: journal → timer decision → fire ---------------------------

#[test]
fn wired_journal_timer_fire_compose_end_to_end() {
    // A human is typing: the journal holds a draft, the timer decision HOLDS while
    // the keyboard is warm, then fires once quiet — and the fire lands message-seen.
    let cfg = super::AttendedConfig::default();
    let mut j = Journal::new();
    j.on_human_input(b"draft in progress", 1_000, cfg.paste_threshold);
    // Warm keyboard just after the keystroke → HOLD (countdown), never immediate.
    assert!(matches!(
        evaluate(&j, 1_000, 1_100, &cfg, false),
        FireDecision::Countdown { .. }
    ));
    // Keyboard quiet for the window → Immediate.
    assert_eq!(
        evaluate(&j, 1_000, 1_000 + cfg.quiet_window_ms, &cfg, false),
        FireDecision::Immediate
    );
    // The fire then resolves to message-seen (draft preserved+replayed inside fire).
    let fx = Fx {
        screen: format!("{PROMPT_GLYPH} the message"),
        status: Some("busy".into()),
    };
    let (_d, spool) = scratch_spool();
    let lock = Mutex::new(InputLock::new());
    let journal = Mutex::new(j);
    let out = fire(
        &fx,
        &SafeDefaultFacts,
        &FixedProbe(LandingScan::Landed),
        &lock,
        &journal,
        &spool,
        golden_record("71234-1781241549123-10"),
        "the message",
        &cfg_fast(),
    );
    assert!(matches!(
        out,
        FireOutcome::Terminal(Payload::MessageSeen { .. })
    ));
    // The spooled record was cleared through a terminal? No — fire leaves the record
    // for the caller to remove; here we just assert the terminal shape above. The
    // draft snapshot is durable (QS-2): the fire-start write preserved it.
    let rec = spool.load("71234-1781241549123-10").unwrap().unwrap();
    assert_eq!(rec.draft, b"draft in progress", "draft preserved byte-exact");
    assert!(rec.fire_started && rec.fire_completed);
}
