//! Byte-exact wire GOLDEN for the disposition-record schema (qd–qf transition
//! v1, R8–R11 — the typed-event log + folded summary, ruled names
//! origin/witness).
//!
//! These strings are pinned against `dispatch/doc/formats/dispatch-transport-
//! formats.md` §§1–3: key ORDER, `reason` present ONLY on `delivery-failed`,
//! the nullable summary columns (`last_event`, `last_attempt_at`,
//! `first_delivered_at`, `expires_at`, `witness`) emitted as `null` when
//! absent. If a line here changes, the format contract changed — update the doc
//! in lockstep. Byte-exactness is by construction here (fixed-shape
//! serde-derive structs, fields in declaration order); no serde_json
//! `preserve_order` feature.

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

// ---- the five event types (reason ONLY on delivery-failed) ----
// Inbound story values: envelope originated on mira, witnessed on brano —
// witness ≠ origin so a swapped-field emission cannot pass the golden.

#[test]
fn event_accepted_wire_golden() {
    let ev = DispositionEvent::accepted(
        "01ABC".into(),
        1_781_241_500_000,
        "brano".into(),
        "mira".into(),
        1_781_241_499_000,
    );
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","event":"accepted","witnessed_at":1781241500000,"witness":"brano","origin":"mira","authored_at":1781241499000}"#
    );
}

#[test]
fn event_attempted_wire_golden() {
    let ev = DispositionEvent::attempted(
        "01ABC".into(),
        1_781_241_500_000,
        "brano".into(),
        "mira".into(),
        1_781_241_499_000,
    );
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","event":"attempted","witnessed_at":1781241500000,"witness":"brano","origin":"mira","authored_at":1781241499000}"#
    );
}

#[test]
fn event_queued_wire_golden() {
    let ev = DispositionEvent::queued(
        "01ABC".into(),
        1_781_241_500_000,
        "brano".into(),
        "mira".into(),
        1_781_241_499_000,
    );
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","event":"queued","witnessed_at":1781241500000,"witness":"brano","origin":"mira","authored_at":1781241499000}"#
    );
}

#[test]
fn event_delivered_wire_golden() {
    // delivered → NO reason key.
    let ev = DispositionEvent::delivered(
        "01ABC".into(),
        1_781_241_500_500,
        "brano".into(),
        "mira".into(),
        1_781_241_499_000,
    );
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","event":"delivered","witnessed_at":1781241500500,"witness":"brano","origin":"mira","authored_at":1781241499000}"#
    );
}

#[test]
fn event_delivery_failed_wire_golden() {
    // delivery-failed → reason present, LAST; event key is kebab-case.
    let ev = DispositionEvent::delivery_failed(
        "01DEF".into(),
        1_781_241_600_000,
        "brano".into(),
        "mira".into(),
        1_781_241_499_000,
        "wake".into(),
    );
    assert_eq!(
        ev.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01DEF","event":"delivery-failed","witnessed_at":1781241600000,"witness":"brano","origin":"mira","authored_at":1781241499000,"reason":"wake"}"#
    );
}

// ---- the emitted summary record ----

#[test]
fn summary_delivered_wire_golden() {
    // A folded fail→retry→succeed summary. Key order pinned by R11:
    // attempts BEFORE last_event; origin then witness last.
    let s = SummaryRecord {
        v: 1,
        correlation_id: "01ABC".to_string(),
        state: SummaryState::Delivered,
        attempts: 2,
        last_event: Some(EventKind::Delivered),
        last_attempt_at: Some(1_781_241_500_200),
        first_delivered_at: Some(1_781_241_500_500),
        expires_at: Some(1_781_284_700_000),
        authored_at: 1_781_241_499_000,
        origin: "brano".to_string(),
        witness: Some("mira".to_string()),
    };
    assert_eq!(
        s.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","state":"delivered","attempts":2,"last_event":"delivered","last_attempt_at":1781241500200,"first_delivered_at":1781241500500,"expires_at":1781284700000,"authored_at":1781241499000,"origin":"brano","witness":"mira"}"#
    );
}

#[test]
fn summary_zero_events_pending_nulls_wire_golden() {
    // Zero events → {last_event, witness} null TOGETHER (R11.1 paired-null; no
    // fabricated `accepted`), plus the null analytics columns — all STABLE
    // columns, never skipped.
    let s = SummaryRecord {
        v: 1,
        correlation_id: "01GHI".to_string(),
        state: SummaryState::Pending,
        attempts: 0,
        last_event: None,
        last_attempt_at: None,
        first_delivered_at: None,
        expires_at: Some(1_781_284_700_000),
        authored_at: 1_781_241_499_000,
        origin: "brano".to_string(),
        witness: None,
    };
    assert_eq!(
        s.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01GHI","state":"pending","attempts":0,"last_event":null,"last_attempt_at":null,"first_delivered_at":null,"expires_at":1781284700000,"authored_at":1781241499000,"origin":"brano","witness":null}"#
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
        authored_at: 1_781_241_499_000,
        origin: "brano".to_string(),
        witness: Some("brano".to_string()),
    };
    assert_eq!(
        s.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01DEF","state":"failed","attempts":1,"last_event":"delivery-failed","last_attempt_at":1781241600000,"first_delivered_at":null,"expires_at":1781284700000,"authored_at":1781241499000,"origin":"brano","witness":"brano"}"#
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
    let ev = DispositionEvent::delivery_failed("rt".into(), 15, "w".into(), "o".into(), 10, "wake".into());
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
fn event_row_missing_origin_is_corrupt_on_parse() {
    // R11: `origin` is REQUIRED on every event row (serde required field) — a
    // pre-R11 row without it no longer parses.
    let no_origin =
        r#"{"v":1,"correlation_id":"x","event":"queued","witnessed_at":5,"witness":"h","authored_at":0}"#;
    let good = DispositionEvent::queued("x".into(), 6, "h".into(), "o".into(), 0).to_jsonl_line();
    let buf = format!("{}\n{}\n", no_origin, good);
    let r = parse_dispositions(buf.as_bytes());
    assert_eq!(r.records.len(), 1, "only the origin-carrying row survives");
    assert_eq!(r.corrupt_interior, 1, "missing `origin` is corrupt");
}

// -------- the fail→retry→succeed fold, end-to-end through project_summary -----

#[test]
fn fail_then_retry_then_succeed_folds_to_delivered() {
    let (t1, t2, t3) = (100, 200, 300);
    let events = vec![
        DispositionEvent::attempted("a".into(), t1, "h".into(), "origin".into(), 10),
        DispositionEvent::delivery_failed("a".into(), t1, "h".into(), "origin".into(), 10, "wake".into()),
        DispositionEvent::attempted("a".into(), t2, "h".into(), "origin".into(), 10),
        DispositionEvent::queued("a".into(), t2, "h".into(), "origin".into(), 10),
        DispositionEvent::delivered("a".into(), t3, "h".into(), "origin".into(), 10),
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
    assert_eq!(out[0].witness.as_deref(), Some("h"));
    assert_eq!(out[0].first_delivered_at, Some(t3));
    assert_eq!(out[0].last_attempt_at, Some(t2));
    assert!(has_delivered(&events, "a"));

    // and the emitted summary shows the null discipline for a zero-events
    // sibling: {last_event, witness} null TOGETHER (R11.1) + null analytics.
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
    assert!(p[0].to_jsonl_line().contains(r#""witness":null"#));
    assert!(p[0].to_jsonl_line().contains(r#""last_attempt_at":null"#));
    assert!(p[0].to_jsonl_line().contains(r#""first_delivered_at":null"#));
    assert!(!out[0].to_jsonl_line().contains("null"), "the delivered summary has no nulls");
}

// -------- R11.2 tie-break, end-to-end through project_summary -----

#[test]
fn same_instant_funnel_wire_shows_delivered_last_event() {
    // The §6 funnel compressed into ONE instant, one witness: the file-last
    // row (delivered) is the last_event pick — the tie-break's own
    // discriminating scenario (a strict-`>` fold would emit "attempted").
    let t = 1_781_241_500_000;
    let events = vec![
        DispositionEvent::attempted("z".into(), t, "h".into(), "origin".into(), 10),
        DispositionEvent::delivery_failed("z".into(), t, "h".into(), "origin".into(), 10, "wake".into()),
        DispositionEvent::attempted("z".into(), t, "h".into(), "origin".into(), 10),
        DispositionEvent::queued("z".into(), t, "h".into(), "origin".into(), 10),
        DispositionEvent::delivered("z".into(), t, "h".into(), "origin".into(), 10),
    ];
    let out = project_summary(&[], &events, t + 1);
    assert_eq!(out[0].state, SummaryState::Delivered);
    assert_eq!(out[0].last_event, Some(EventKind::Delivered));
    let line = out[0].to_jsonl_line();
    assert!(line.contains(r#""state":"delivered""#));
    assert!(line.contains(r#""last_event":"delivered""#));
}
