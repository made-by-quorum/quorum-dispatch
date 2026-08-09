//! Byte-exact wire GOLDEN for the disposition-record schema (qd–qf transition
//! v1, R8–R15 — the typed-event discriminated-union log + folded summary,
//! FULLY NORMALIZED per R14, with the R15 delivered `body_digest` binding).
//!
//! These strings are pinned against `dispatch/doc/formats/dispatch-transport-
//! formats.md` §§1–3: key ORDER, the per-variant tail (`class` ONLY on
//! `delivery-failed`/`refused`, `body_digest` ONLY on `delivered` — R15), the
//! nullable summary columns (`last_event`, `last_attempt_at`,
//! `first_delivered_at`, `expires_at`, `authored_at`, `origin`) emitted as
//! `null` when absent. If a line here changes, the format contract changed —
//! update the doc in lockstep. Byte-exactness is by construction: the fixed-shape
//! structs ride serde-derive declaration order; the [`DispositionEvent`]
//! discriminated union rides a hand-written `Serialize` that pins
//! `{v, correlation_id, event, created_at, [class | body_digest]}`.

use quorum_dispositions::{
    has_delivered, parse_dispositions, parse_log, project_summary, DispositionEvent, Envelope,
    EventKind, SummaryRecord, SummaryState,
};

// ------------------------------- goldens -------------------------------

#[test]
fn envelope_wire_golden() {
    let e = Envelope {
        v: 1,
        correlation_id: "01ABC".to_string(),
        authored_at: 1_781_241_500_000,
        expires_at: 1_781_284_700_000,
        target: "alpha@brano".to_string(),
        origin: "brano".to_string(),
        body: "hello world".to_string(),
    };
    assert_eq!(
        e.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","authored_at":1781241500000,"expires_at":1781284700000,"target":"alpha@brano","origin":"brano","body":"hello world"}"#
    );
}

// ---- the five event types (class ONLY on delivery-failed + refused) ----
// FULLY NORMALIZED (R14.2): event rows carry NO witness / origin / authored_at.

#[test]
fn event_attempted_wire_golden() {
    let ev = DispositionEvent::attempted("01ABC".into(), 1_781_241_500_000);
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","event":"attempted","created_at":1781241500000}"#
    );
}

#[test]
fn event_queued_wire_golden() {
    let ev = DispositionEvent::queued("01ABC".into(), 1_781_241_500_000);
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","event":"queued","created_at":1781241500000}"#
    );
}

#[test]
fn event_delivered_wire_golden() {
    // delivered → body_digest tail (R15), last on the wire.
    let ev = DispositionEvent::delivered(
        "01ABC".into(),
        1_781_241_500_500,
        "a591a6d40bf420404a011733cfb7b190d62c65bf0bcda32b57b277d9ad9f146e".into(),
    );
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","event":"delivered","created_at":1781241500500,"body_digest":"a591a6d40bf420404a011733cfb7b190d62c65bf0bcda32b57b277d9ad9f146e"}"#
    );
}

#[test]
fn event_delivery_failed_wire_golden() {
    // delivery-failed → class present, LAST; event key is kebab-case.
    let ev = DispositionEvent::delivery_failed("01DEF".into(), 1_781_241_600_000, "wake".into());
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01DEF","event":"delivery-failed","created_at":1781241600000,"class":"wake"}"#
    );
}

#[test]
fn event_refused_wire_golden() {
    // refused → class present, LAST (same payload shape as delivery-failed today).
    let ev = DispositionEvent::refused("01GHI".into(), 1_781_241_700_000, "no-live-receive-path".into());
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01GHI","event":"refused","created_at":1781241700000,"class":"no-live-receive-path"}"#
    );
}

// ---- the emitted summary record ----

#[test]
fn summary_delivered_wire_golden() {
    // A folded fail→retry→succeed summary. Key order pinned by R14:
    // attempts BEFORE last_event; expires_at, authored_at, origin last (from the
    // joined envelope). No witness field (R14.2 — normalized away).
    let s = SummaryRecord {
        v: 1,
        correlation_id: "01ABC".to_string(),
        state: SummaryState::Delivered,
        attempts: 2,
        last_event: Some(EventKind::Delivered),
        last_attempt_at: Some(1_781_241_500_200),
        first_delivered_at: Some(1_781_241_500_500),
        expires_at: Some(1_781_284_700_000),
        authored_at: Some(1_781_241_499_000),
        origin: Some("brano".to_string()),
    };
    assert_eq!(
        s.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","state":"delivered","attempts":2,"last_event":"delivered","last_attempt_at":1781241500200,"first_delivered_at":1781241500500,"expires_at":1781284700000,"authored_at":1781241499000,"origin":"brano"}"#
    );
}

#[test]
fn summary_orphan_triple_null_wire_golden() {
    // An orphan-event summary (no envelope in scope): origin, authored_at, AND
    // expires_at are ALL null (the R14.2 honest-null change — no copy-from-event).
    // The delivered event still drives state=delivered + last_event. This is the
    // discriminating golden for R14.2.
    let s = SummaryRecord {
        v: 1,
        correlation_id: "01ORPHAN".to_string(),
        state: SummaryState::Delivered,
        attempts: 0,
        last_event: Some(EventKind::Delivered),
        last_attempt_at: None,
        first_delivered_at: Some(1_781_241_500_500),
        expires_at: None,
        authored_at: None,
        origin: None,
    };
    assert_eq!(
        s.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ORPHAN","state":"delivered","attempts":0,"last_event":"delivered","last_attempt_at":null,"first_delivered_at":1781241500500,"expires_at":null,"authored_at":null,"origin":null}"#
    );
}

#[test]
fn summary_zero_events_pending_nulls_wire_golden() {
    // Zero events WITH an envelope in scope → last_event null (no fabricated
    // accepted), null analytics columns; origin/authored_at/expires_at present
    // from the envelope. All columns are STABLE, never skipped.
    let s = SummaryRecord {
        v: 1,
        correlation_id: "01GHI".to_string(),
        state: SummaryState::Pending,
        attempts: 0,
        last_event: None,
        last_attempt_at: None,
        first_delivered_at: None,
        expires_at: Some(1_781_284_700_000),
        authored_at: Some(1_781_241_499_000),
        origin: Some("brano".to_string()),
    };
    assert_eq!(
        s.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01GHI","state":"pending","attempts":0,"last_event":null,"last_attempt_at":null,"first_delivered_at":null,"expires_at":1781284700000,"authored_at":1781241499000,"origin":"brano"}"#
    );
}

#[test]
fn summary_failed_wire_golden() {
    let s = SummaryRecord {
        v: 1,
        correlation_id: "01DEF".to_string(),
        state: SummaryState::Failed,
        attempts: 1,
        last_event: Some(EventKind::DeliveryFailed),
        last_attempt_at: Some(1_781_241_600_000),
        first_delivered_at: None,
        expires_at: Some(1_781_284_700_000),
        authored_at: Some(1_781_241_499_000),
        origin: Some("brano".to_string()),
    };
    assert_eq!(
        s.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01DEF","state":"failed","attempts":1,"last_event":"delivery-failed","last_attempt_at":1781241600000,"first_delivered_at":null,"expires_at":1781284700000,"authored_at":1781241499000,"origin":"brano"}"#
    );
}

// --------------------------- round-trip via parse ---------------------------

#[test]
fn round_trip_through_parsers() {
    let e = Envelope {
        v: 1,
        correlation_id: "rt".to_string(),
        authored_at: 10,
        expires_at: 20,
        target: "t".to_string(),
        origin: "o".to_string(),
        body: "b".to_string(),
    };
    let ev = DispositionEvent::delivery_failed("rt".into(), 15, "wake".into());
    let log_bytes = format!("{}\n", e.to_jsonl_line());
    let disp_bytes = format!("{}\n", ev.to_jsonl_line());
    let er = parse_log(log_bytes.as_bytes());
    let dr = parse_dispositions(disp_bytes.as_bytes());
    assert_eq!(er.records, vec![e]);
    assert_eq!(dr.records, vec![ev]);
    assert_eq!(er.corrupt_interior, 0);
    assert_eq!(dr.corrupt_interior, 0);
}

#[test]
fn delivery_failed_missing_class_is_corrupt_on_parse() {
    // R14.5: `class` is REQUIRED on delivery-failed (discriminated-union tail) —
    // a row without it no longer parses.
    let no_class = r#"{"v":1,"correlation_id":"x","event":"delivery-failed","created_at":5}"#;
    let good = DispositionEvent::queued("x".into(), 6).to_jsonl_line();
    let buf = format!("{}\n{}\n", no_class, good);
    let r = parse_dispositions(buf.as_bytes());
    assert_eq!(r.records.len(), 1, "only the valid row survives");
    assert_eq!(r.corrupt_interior, 1, "missing `class` is corrupt");
}

#[test]
fn plain_event_with_foreign_class_is_corrupt_on_parse() {
    // R14.5: a plain event carrying `class` (a foreign field) → corrupt.
    let with_class = r#"{"v":1,"correlation_id":"x","event":"queued","created_at":5,"class":"nope"}"#;
    let good = DispositionEvent::queued("x".into(), 6).to_jsonl_line();
    let buf = format!("{}\n{}\n", with_class, good);
    let r = parse_dispositions(buf.as_bytes());
    assert_eq!(r.records.len(), 1, "only the valid row survives");
    assert_eq!(r.corrupt_interior, 1, "a plain event with a foreign `class` is corrupt");
}

// -------- the fail→retry→succeed fold, end-to-end through project_summary -----

#[test]
fn fail_then_retry_then_succeed_folds_to_delivered() {
    let (t1, t2, t3) = (100, 200, 300);
    let events = vec![
        DispositionEvent::attempted("a".into(), t1),
        DispositionEvent::delivery_failed("a".into(), t1, "wake".into()),
        DispositionEvent::attempted("a".into(), t2),
        DispositionEvent::queued("a".into(), t2),
        DispositionEvent::delivered("a".into(), t3, "d".into()),
    ];
    let envs = vec![Envelope {
        v: 1,
        correlation_id: "a".into(),
        authored_at: 10,
        expires_at: 100_000,
        target: "t".into(),
        origin: "origin".into(),
        body: "b".into(),
    }];
    let out = project_summary(&envs, &events, 400);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].state, SummaryState::Delivered);
    assert_eq!(out[0].attempts, 2);
    assert_eq!(out[0].last_event, Some(EventKind::Delivered));
    assert_eq!(out[0].first_delivered_at, Some(t3));
    assert_eq!(out[0].last_attempt_at, Some(t2));
    assert_eq!(out[0].origin, Some("origin".to_string()), "origin from the envelope");
    assert_eq!(out[0].authored_at, Some(10), "authored_at from the envelope");
    assert!(has_delivered(&events, "a"));

    // The delivered summary carries no nulls (all columns present from the
    // envelope join).
    assert!(!out[0].to_jsonl_line().contains("null"), "the delivered summary has no nulls");

    // A zero-events sibling shows the null discipline: last_event null (no
    // fabricated accepted) + null analytics; origin/authored_at present (envelope).
    let pending_env = Envelope {
        v: 1,
        correlation_id: "p".into(),
        authored_at: 5,
        expires_at: 100_000,
        target: "t".into(),
        origin: "origin".into(),
        body: "b".into(),
    };
    let p = project_summary(&[pending_env], &[], 400);
    assert!(p[0].to_jsonl_line().contains(r#""last_event":null"#));
    assert!(p[0].to_jsonl_line().contains(r#""last_attempt_at":null"#));
    assert!(p[0].to_jsonl_line().contains(r#""first_delivered_at":null"#));
    assert!(p[0].to_jsonl_line().contains(r#""origin":"origin""#));
}

// -------- the orphan-event summary shows the R14.2 triple-null, end-to-end ----

#[test]
fn orphan_event_summary_wire_shows_triple_null() {
    // A delivered event with no envelope in scope → origin, authored_at, AND
    // expires_at ALL null in the emitted line (the R14.2 honest-null change).
    let events = vec![DispositionEvent::delivered("orph".into(), 700, "d".into())];
    let out = project_summary(&[], &events, i64::MAX);
    assert_eq!(out.len(), 1);
    let line = out[0].to_jsonl_line();
    assert!(line.contains(r#""state":"delivered""#));
    assert!(line.contains(r#""expires_at":null"#), "no envelope → expires_at null");
    assert!(line.contains(r#""authored_at":null"#), "no envelope → authored_at null (R14.2)");
    assert!(line.contains(r#""origin":null"#), "no envelope → origin null (R14.2)");
}

// -------- a refused-only summary → state pending (refused is pending-class) ----

#[test]
fn refused_only_summary_is_pending_wire() {
    let events = vec![DispositionEvent::refused("r".into(), 700, "ambiguous".into())];
    let envs = vec![Envelope {
        v: 1,
        correlation_id: "r".into(),
        authored_at: 10,
        expires_at: 1_000_000,
        target: "t".into(),
        origin: "origin".into(),
        body: "b".into(),
    }];
    let out = project_summary(&envs, &events, 800);
    assert_eq!(out[0].state, SummaryState::Pending, "refused ≠ failed, pending-class (R14.3)");
    assert_eq!(out[0].last_event, Some(EventKind::Refused));
    let line = out[0].to_jsonl_line();
    assert!(line.contains(r#""state":"pending""#));
    assert!(line.contains(r#""last_event":"refused""#));
}

// -------- R14.2 tie-break, end-to-end through project_summary -----

#[test]
fn same_instant_funnel_wire_shows_delivered_last_event() {
    // The §6 funnel compressed into ONE instant: the file-last row (delivered)
    // is the last_event pick — the tie-break's own discriminating scenario (a
    // strict-`>` fold would emit "attempted"). Cross-source determinism is the
    // union reader's job one layer up (dispatch layer); this leaf folds a flat,
    // already-ordered slice.
    let t = 1_781_241_500_000;
    let events = vec![
        DispositionEvent::attempted("z".into(), t),
        DispositionEvent::delivery_failed("z".into(), t, "wake".into()),
        DispositionEvent::attempted("z".into(), t),
        DispositionEvent::queued("z".into(), t),
        DispositionEvent::delivered("z".into(), t, "d".into()),
    ];
    let out = project_summary(&[], &events, t + 1);
    assert_eq!(out[0].state, SummaryState::Delivered);
    assert_eq!(out[0].last_event, Some(EventKind::Delivered));
    let line = out[0].to_jsonl_line();
    assert!(line.contains(r#""state":"delivered""#));
    assert!(line.contains(r#""last_event":"delivered""#));
}
