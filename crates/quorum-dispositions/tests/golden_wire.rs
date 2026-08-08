//! Byte-exact wire GOLDEN for the disposition-record schema (qd–qf transition W1).
//!
//! These strings are pinned against `dispatch/doc/formats/dispatch-transport-
//! formats.md` §§1–3: key ORDER, `witnessed_at:null` present for pending/expired,
//! `reason` omitted when absent. If a line here changes, the format contract
//! changed — update the doc in lockstep. Byte-exactness is by construction here
//! (fixed-shape serde-derive structs, fields in declaration order); no
//! serde_json `preserve_order` feature is involved.

use quorum_dispositions::{
    parse_dispositions, parse_log, project, Disposition, EmittedRecord, Envelope, RecordState,
    StoredState,
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
        authority: "brano".to_string(),
        body: "hello world".to_string(),
    };
    assert_eq!(
        e.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","authored_at":1781241500000,"expires_at":1781284700000,"target":"alpha@brano","authority":"brano","body":"hello world"}"#
    );
}

#[test]
fn disposition_delivered_wire_golden() {
    // delivered, no reason → reason key omitted.
    let d = Disposition {
        v: 1,
        correlation_id: "01ABC".to_string(),
        state: StoredState::Delivered,
        authored_at: 1_781_241_500_000,
        witnessed_at: 1_781_241_500_500,
        authority: "brano".to_string(),
        reason: None,
    };
    assert_eq!(
        d.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","state":"delivered","authored_at":1781241500000,"witnessed_at":1781241500500,"authority":"brano"}"#
    );
}

#[test]
fn disposition_failed_wire_golden() {
    // failed, with reason → reason key present, last.
    let d = Disposition {
        v: 1,
        correlation_id: "01DEF".to_string(),
        state: StoredState::Failed,
        authored_at: 1_781_241_500_000,
        witnessed_at: 1_781_241_600_000,
        authority: "brano".to_string(),
        reason: Some("wake".to_string()),
    };
    assert_eq!(
        d.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01DEF","state":"failed","authored_at":1781241500000,"witnessed_at":1781241600000,"authority":"brano","reason":"wake"}"#
    );
}

#[test]
fn emitted_pending_wire_golden() {
    // pending → witnessed_at:null present (stable column), reason omitted.
    let r = EmittedRecord {
        v: 1,
        correlation_id: "01ABC".to_string(),
        state: RecordState::Pending,
        authored_at: 1_781_241_500_000,
        witnessed_at: None,
        authority: "brano".to_string(),
        reason: None,
    };
    assert_eq!(
        r.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","state":"pending","authored_at":1781241500000,"witnessed_at":null,"authority":"brano"}"#
    );
}

#[test]
fn emitted_delivered_wire_golden() {
    let r = EmittedRecord {
        v: 1,
        correlation_id: "01ABC".to_string(),
        state: RecordState::Delivered,
        authored_at: 1_781_241_500_000,
        witnessed_at: Some(1_781_241_500_500),
        authority: "brano".to_string(),
        reason: None,
    };
    assert_eq!(
        r.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01ABC","state":"delivered","authored_at":1781241500000,"witnessed_at":1781241500500,"authority":"brano"}"#
    );
}

#[test]
fn emitted_failed_wire_golden() {
    let r = EmittedRecord {
        v: 1,
        correlation_id: "01DEF".to_string(),
        state: RecordState::Failed,
        authored_at: 1_781_241_500_000,
        witnessed_at: Some(1_781_241_600_000),
        authority: "brano".to_string(),
        reason: Some("wake".to_string()),
    };
    assert_eq!(
        r.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01DEF","state":"failed","authored_at":1781241500000,"witnessed_at":1781241600000,"authority":"brano","reason":"wake"}"#
    );
}

#[test]
fn emitted_expired_wire_golden() {
    // expired → witnessed_at:null present, reason omitted.
    let r = EmittedRecord {
        v: 1,
        correlation_id: "01GHI".to_string(),
        state: RecordState::Expired,
        authored_at: 1_781_241_500_000,
        witnessed_at: None,
        authority: "brano".to_string(),
        reason: None,
    };
    assert_eq!(
        r.to_jsonl_line(),
        r#"{"v":1,"correlation_id":"01GHI","state":"expired","authored_at":1781241500000,"witnessed_at":null,"authority":"brano"}"#
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
        authority: "a".to_string(),
        body: "b".to_string(),
    };
    let d = Disposition {
        v: 1,
        correlation_id: "rt".to_string(),
        state: StoredState::Failed,
        authored_at: 10,
        witnessed_at: 15,
        authority: "a".to_string(),
        reason: Some("wake".to_string()),
    };
    let log_bytes = format!("{}\n", e.to_jsonl_line());
    let disp_bytes = format!("{}\n", d.to_jsonl_line());
    let er = parse_log(log_bytes.as_bytes());
    let dr = parse_dispositions(disp_bytes.as_bytes());
    assert_eq!(er.records, vec![e]);
    assert_eq!(dr.records, vec![d]);
    assert_eq!(er.corrupt_interior, 0);
    assert_eq!(dr.corrupt_interior, 0);
}

// ---------------------------- projection matrix ----------------------------

#[test]
fn projection_matrix_states_and_witnessed_nullness() {
    let envs = vec![
        // a: delivered
        Envelope { v: 1, correlation_id: "a".into(), authored_at: 1, expires_at: 1000, target: "t".into(), authority: "origin".into(), body: "b".into() },
        // b: failed(reason)
        Envelope { v: 1, correlation_id: "b".into(), authored_at: 2, expires_at: 1000, target: "t".into(), authority: "origin".into(), body: "b".into() },
        // c: no disp, pre-expiry → pending
        Envelope { v: 1, correlation_id: "c".into(), authored_at: 3, expires_at: 1000, target: "t".into(), authority: "origin".into(), body: "b".into() },
        // d: no disp, post-expiry → expired
        Envelope { v: 1, correlation_id: "d".into(), authored_at: 4, expires_at: 10, target: "t".into(), authority: "origin".into(), body: "b".into() },
    ];
    let disps = vec![
        Disposition { v: 1, correlation_id: "a".into(), state: StoredState::Delivered, authored_at: 1, witnessed_at: 50, authority: "wit".into(), reason: None },
        Disposition { v: 1, correlation_id: "b".into(), state: StoredState::Failed, authored_at: 2, witnessed_at: 60, authority: "wit".into(), reason: Some("wake".into()) },
        // orphan: terminal with no envelope in scope
        Disposition { v: 1, correlation_id: "orphan".into(), state: StoredState::Delivered, authored_at: 7, witnessed_at: 70, authority: "wit".into(), reason: None },
    ];
    let now = 100; // a/b terminal; c pending (100<1000); d expired (100>=10)
    let out = project(&envs, &disps, now);

    // Order: envelopes (a,b,c,d) then orphans (orphan).
    let ids: Vec<&str> = out.iter().map(|r| r.correlation_id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", "c", "d", "orphan"]);

    // a delivered, witnessed_at Some.
    assert_eq!(out[0].state, RecordState::Delivered);
    assert_eq!(out[0].witnessed_at, Some(50));
    assert_eq!(out[0].reason, None);
    // b failed with reason, witnessed_at Some.
    assert_eq!(out[1].state, RecordState::Failed);
    assert_eq!(out[1].witnessed_at, Some(60));
    assert_eq!(out[1].reason.as_deref(), Some("wake"));
    // c pending, witnessed_at null, origin authority.
    assert_eq!(out[2].state, RecordState::Pending);
    assert_eq!(out[2].witnessed_at, None);
    assert_eq!(out[2].authority, "origin");
    // d expired, witnessed_at null.
    assert_eq!(out[3].state, RecordState::Expired);
    assert_eq!(out[3].witnessed_at, None);
    // orphan → terminal emitted from disposition alone.
    assert_eq!(out[4].state, RecordState::Delivered);
    assert_eq!(out[4].witnessed_at, Some(70));
    assert_eq!(out[4].authored_at, 7, "orphan authored_at from disposition");

    // Also confirm the emitted null/omit wire shape end-to-end for pending & failed.
    assert!(out[2].to_jsonl_line().contains(r#""witnessed_at":null"#));
    assert!(!out[0].to_jsonl_line().contains("reason"));
    assert!(out[1].to_jsonl_line().contains(r#""reason":"wake""#));
}
