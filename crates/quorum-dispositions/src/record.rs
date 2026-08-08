//! The wire-shape reflections + their enums (qd–qf transition v1, R8–R11).
//!
//! Field DECLARATION order == the byte-exact wire key order pinned by the format
//! doc (see the crate-root doc for why declaration order is load-bearing). Do
//! not reorder fields without updating the format contract and the golden test.
//!
//! # The ruled model (R8/R8a/R8b, names ruled by R9, summary shape by R10/R11)
//!
//! `dispositions.jsonl` is an **append-only log of typed witnessed events**, one
//! row per witnessed moment — NEVER a state record. "First terminal wins" is
//! dead ([`DispositionEvent`], the five [`EventKind`] types). State is a **view**
//! ([`SummaryRecord`] / [`SummaryState`]): the summary carries the coarse
//! 4-state enum UNCHANGED, plus `last_event` for detail, and is computed by the
//! projection (see `project.rs`). Idempotence keys on a `delivered` event
//! EXISTING (see [`crate::has_delivered`]), never "any terminal."
//!
//! Naming is RULED (R9): `origin` = the origin host id ({origin, authored_at} —
//! the ORIGIN timeline); `witness` = the witnessing host id ({witness,
//! witnessed_at} — the WITNESS timeline). The N10 split, expressed as field
//! naming.

use serde::{Deserialize, Serialize};

/// A `log.jsonl` row — an envelope qd ORIGINATED (format doc §1). Unchanged by
/// R8: the log is the envelope source; the event shift is §2/§3 only.
///
/// Wire key order (byte-exact): `v, correlation_id, authored_at, expires_at,
/// target, origin, body`. `body` is last (largest + most variable field;
/// keeps the head of every line cheap to scan).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Version marker — always `1`.
    pub v: u32,
    /// The envelope id: minted once at origin, travels verbatim, the
    /// idempotency + disposition join key. Never a content hash. The name is
    /// ruled (R9.1): an event log holds many rows per envelope — the field
    /// correlates rows, it identifies none.
    pub correlation_id: String,
    /// epoch-ms, stamped once at origin (N10 authored timeline).
    pub authored_at: i64,
    /// epoch-ms. Default `authored_at + 12h`; `expired` is minted from ABSENCE
    /// past this value. Policy travels with the message.
    pub expires_at: i64,
    /// The address as given by the caller (`name | stable_id | name@host`),
    /// RAW (R9.4: views split on `@` at query time; a parsed-out target_host
    /// would be derived state materialized into the log). Operational record;
    /// NOT load-bearing for the disposition join.
    pub target: String,
    /// Origin host id (this qd's host) — named for its N10 role (R9.2).
    /// Disambiguates origin when a peer's log is read from `remote/<host>/`.
    pub origin: String,
    /// The opaque prose, verbatim, delivered as one message. qd never parses it.
    pub body: String,
}

/// The five witnessed-event types recorded in `dispositions.jsonl` (format doc
/// §2, R8b full funnel). Each is past tense — a moment qd WITNESSED, never a
/// speculative/planned state. The set is OPEN: future witnessed facts arrive as
/// NEW variants here, never as new values of a shared "outcome" field (R8a).
///
/// Serde (kebab-case): `accepted` / `attempted` / `queued` / `delivered` /
/// `delivery-failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    /// Inbound envelope presented and accepted through the door (inbound mode).
    Accepted,
    /// A delivery attempt STARTED. Each retry is a fresh `attempted` event.
    Attempted,
    /// The attempt placed the message into the target's delivery queue /
    /// awaiting idle or wake — a witnessed moment, possibly minutes before the
    /// prose lands (busy session, waking session).
    Queued,
    /// The prose LANDED in the session. Existence of this event IS the
    /// irreversible delivered fact. Carries NO reason.
    Delivered,
    /// The attempt definitively did not arrive. Carries a REQUIRED `reason` (the
    /// `{class,reason}` family — `failed{wake}` = `delivery-failed` reason
    /// `"wake"`). Serializes as `"delivery-failed"`.
    DeliveryFailed,
}

/// A stored `dispositions.jsonl` row — one TYPED WITNESSED EVENT (format doc §2,
/// R8/R8a/R8b). Never a state record: state is a view ([`SummaryRecord`]).
///
/// The `reason` invariant is per-event-type (tighter than a shared enum): it is
/// REQUIRED on [`EventKind::DeliveryFailed`] and FORBIDDEN on every other type.
/// The constructors ([`DispositionEvent::accepted`] etc.) enforce this at the
/// authoring seam; the parser ([`crate::parse_dispositions`]) enforces it on
/// read.
///
/// Event rows deliberately carry NO `expires_at` (ruled): the door refuses
/// past-expiry presentations; an orphan's expiry status stays a documented
/// degenerate analytics case, not a schema driver.
///
/// Wire key order (byte-exact): `v, correlation_id, event, witnessed_at,
/// witness, origin, authored_at, reason` (`reason` omitted entirely when
/// absent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispositionEvent {
    /// Version marker — always `1`.
    pub v: u32,
    /// The envelope's origin-minted id — the join key to the [`Envelope`] and
    /// the correlation key across an event's whole funnel.
    pub correlation_id: String,
    /// Which witnessed moment this row records.
    pub event: EventKind,
    /// epoch-ms, stamped by THIS witness at the moment of witnessing. Effects
    /// order (and the projection's latest-event pick) key on this.
    pub witnessed_at: i64,
    /// The witnessing host id (this qd) — the WITNESS timeline half of the N10
    /// split (R9.3). In a `--host`/`--all` union, disambiguates which host
    /// witnessed.
    pub witness: String,
    /// The envelope's origin host id, copied from the envelope at witness time
    /// (R11): self-containment when the envelope lives in an un-unioned mirror;
    /// all witnesses copy the same envelope field, so unions agree.
    pub origin: String,
    /// epoch-ms, the envelope's origin timeline, copied at witness time so this
    /// row is self-contained when the envelope lives in a mirror (N10).
    pub authored_at: i64,
    /// REQUIRED on `delivery-failed` (the failure class, e.g. `"wake"`);
    /// FORBIDDEN on every other type. Omitted entirely from the wire when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The coarse published summary state (format doc §3). UNCHANGED by R8b (guard
/// 2): the 4-state enum stays `pending | delivered | failed | expired` so
/// frame's simple views are stable; the events are the fine grain underneath.
/// Serialized lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SummaryState {
    /// No delivered event yet, pre-expiry, latest event is not a failure.
    Pending,
    /// A `delivered` event exists (irreversible).
    Delivered,
    /// Latest event is `delivery-failed`, no delivered event, not expired.
    Failed,
    /// No delivered event and now past the envelope's `expires_at`.
    Expired,
}

/// The PUBLISHED per-id summary row — `qd dispositions` DEFAULT output (format
/// doc §3a, shape ruled by R10/R11), the coarse view frame projects over,
/// versioned with the contract. (`qd dispositions --events` emits the raw
/// [`DispositionEvent`] funnel instead.)
///
/// It carries the 4-state [`SummaryState`] UNCHANGED plus `last_event` for
/// detail (R8b guard 2), the folded analytics fields (attempts,
/// last_attempt_at, first_delivered_at), `expires_at`, and the origin/witness
/// pair (R9/R11).
///
/// PAIRED-NULL INVARIANT (R11.1): `last_event` and `witness` are null together,
/// exactly when no events exist — a summary never reports a witnessed moment
/// nobody witnessed (the old fabricated-`accepted` default is OVERRULED).
///
/// Wire key order (byte-exact): `v, correlation_id, state, attempts,
/// last_event, last_attempt_at, first_delivered_at, expires_at, authored_at,
/// origin, witness`. The nullable fields (`last_event`, `last_attempt_at`,
/// `first_delivered_at`, `expires_at`, `witness`) are emitted as JSON `null`
/// when absent (stable columns for the DuckDB projection — NOT skipped).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryRecord {
    /// Version marker — always `1`.
    pub v: u32,
    /// The envelope's origin-minted id.
    pub correlation_id: String,
    /// The coarse 4-state view.
    pub state: SummaryState,
    /// Count of `attempted` events for this id.
    pub attempts: u32,
    /// The latest event by `(witnessed_at, witness)` (R11.2) — detail beneath
    /// the coarse `state`; `null` iff no events exist (R11.1).
    pub last_event: Option<EventKind>,
    /// max `witnessed_at` over `attempted` events; `null` if none.
    pub last_attempt_at: Option<i64>,
    /// min `witnessed_at` over `delivered` events; `null` if none.
    pub first_delivered_at: Option<i64>,
    /// epoch-ms from the joined envelope; `null` when the envelope is not in
    /// scope (an orphan-event summary).
    pub expires_at: Option<i64>,
    /// epoch-ms origin timeline (from the envelope if in scope, else the event).
    pub authored_at: i64,
    /// The origin host id: from the envelope's `origin` when in scope, else
    /// copied from the (first) event's `origin` — every event carries it (R11),
    /// so this is REQUIRED (no nullable escape).
    pub origin: String,
    /// The witness of the `last_event` pick; `null` iff no events exist
    /// (paired with `last_event` — R11.1).
    pub witness: Option<String>,
}

impl Envelope {
    /// The compact single-line JSON for this row (no trailing newline; the
    /// caller appends `\n`). Byte-exact by construction (see crate-root doc).
    pub fn to_jsonl_line(&self) -> String {
        // serde_json::to_string on a fixed-shape struct cannot fail (all fields
        // serialize) and emits fields in declaration order — the pinned wire
        // order. Fall back to an empty object only to keep the signature total.
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl DispositionEvent {
    /// An `accepted` event (inbound envelope through the door). `reason: None`.
    pub fn accepted(
        correlation_id: String,
        witnessed_at: i64,
        witness: String,
        origin: String,
        authored_at: i64,
    ) -> Self {
        Self::without_reason(EventKind::Accepted, correlation_id, witnessed_at, witness, origin, authored_at)
    }

    /// An `attempted` event (a delivery attempt started). `reason: None`.
    pub fn attempted(
        correlation_id: String,
        witnessed_at: i64,
        witness: String,
        origin: String,
        authored_at: i64,
    ) -> Self {
        Self::without_reason(EventKind::Attempted, correlation_id, witnessed_at, witness, origin, authored_at)
    }

    /// A `queued` event (placed in the target's queue / awaiting idle or wake).
    /// `reason: None`.
    pub fn queued(
        correlation_id: String,
        witnessed_at: i64,
        witness: String,
        origin: String,
        authored_at: i64,
    ) -> Self {
        Self::without_reason(EventKind::Queued, correlation_id, witnessed_at, witness, origin, authored_at)
    }

    /// A `delivered` event (the prose landed). `reason: None` — a delivered
    /// event never carries a reason.
    pub fn delivered(
        correlation_id: String,
        witnessed_at: i64,
        witness: String,
        origin: String,
        authored_at: i64,
    ) -> Self {
        Self::without_reason(EventKind::Delivered, correlation_id, witnessed_at, witness, origin, authored_at)
    }

    /// A `delivery-failed` event. `reason` is REQUIRED (the failure class) and is
    /// set here — the type-system counterpart to the per-event-type validation.
    pub fn delivery_failed(
        correlation_id: String,
        witnessed_at: i64,
        witness: String,
        origin: String,
        authored_at: i64,
        reason: String,
    ) -> Self {
        DispositionEvent {
            v: 1,
            correlation_id,
            event: EventKind::DeliveryFailed,
            witnessed_at,
            witness,
            origin,
            authored_at,
            reason: Some(reason),
        }
    }

    /// Shared constructor for the four reason-less event types — sets
    /// `reason: None`, enforcing the FORBIDDEN-reason half of the invariant.
    fn without_reason(
        event: EventKind,
        correlation_id: String,
        witnessed_at: i64,
        witness: String,
        origin: String,
        authored_at: i64,
    ) -> Self {
        DispositionEvent {
            v: 1,
            correlation_id,
            event,
            witnessed_at,
            witness,
            origin,
            authored_at,
            reason: None,
        }
    }

    /// The compact single-line JSON for this row (no trailing newline).
    /// Byte-exact by construction; `reason` omitted when `None`.
    pub fn to_jsonl_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl SummaryRecord {
    /// The compact single-line JSON for this row (no trailing newline).
    /// Byte-exact by construction; the nullable fields emit as `null` when
    /// `None` (stable columns, NOT skipped).
    pub fn to_jsonl_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------- byte-exact golden serialize (keys in documented order) -------

    #[test]
    fn envelope_golden_line() {
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

    #[test]
    fn accepted_event_golden_line() {
        // Inbound story: envelope originated on mira, witnessed on brano —
        // distinct witness/origin values so a field swap cannot pass.
        let ev = DispositionEvent::accepted(
            "01ABC".to_string(),
            1_781_241_500_000,
            "brano".to_string(),
            "mira".to_string(),
            1_781_241_499_000,
        );
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01ABC","event":"accepted","witnessed_at":1781241500000,"witness":"brano","origin":"mira","authored_at":1781241499000}"#
        );
    }

    #[test]
    fn attempted_event_golden_line() {
        let ev = DispositionEvent::attempted(
            "01ABC".to_string(),
            1_781_241_500_000,
            "brano".to_string(),
            "mira".to_string(),
            1_781_241_499_000,
        );
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01ABC","event":"attempted","witnessed_at":1781241500000,"witness":"brano","origin":"mira","authored_at":1781241499000}"#
        );
    }

    #[test]
    fn queued_event_golden_line() {
        let ev = DispositionEvent::queued(
            "01ABC".to_string(),
            1_781_241_500_000,
            "brano".to_string(),
            "mira".to_string(),
            1_781_241_499_000,
        );
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01ABC","event":"queued","witnessed_at":1781241500000,"witness":"brano","origin":"mira","authored_at":1781241499000}"#
        );
    }

    #[test]
    fn delivered_event_golden_line() {
        // delivered → NO reason key.
        let ev = DispositionEvent::delivered(
            "01ABC".to_string(),
            1_781_241_500_500,
            "brano".to_string(),
            "mira".to_string(),
            1_781_241_499_000,
        );
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01ABC","event":"delivered","witnessed_at":1781241500500,"witness":"brano","origin":"mira","authored_at":1781241499000}"#
        );
    }

    #[test]
    fn delivery_failed_event_golden_line() {
        // delivery-failed → reason present, last; event serializes kebab-case.
        let ev = DispositionEvent::delivery_failed(
            "01DEF".to_string(),
            1_781_241_600_000,
            "brano".to_string(),
            "mira".to_string(),
            1_781_241_499_000,
            "wake".to_string(),
        );
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01DEF","event":"delivery-failed","witnessed_at":1781241600000,"witness":"brano","origin":"mira","authored_at":1781241499000,"reason":"wake"}"#
        );
    }

    #[test]
    fn summary_record_golden_line() {
        // A folded delivered summary (the fail→retry→succeed outcome).
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
    fn summary_record_nulls_golden_line() {
        // A zero-events pending summary: {last_event, witness} are null TOGETHER
        // (R11.1 paired-null — no fabricated `accepted`), and the Option<i64>
        // fields emit as null (stable columns).
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

    // ------- round-trip: serialize → parse → equal -------

    #[test]
    fn envelope_round_trip() {
        let e = Envelope {
            v: 1,
            correlation_id: "rt".to_string(),
            authored_at: 10,
            expires_at: 20,
            target: "t".to_string(),
            origin: "o".to_string(),
            body: "b".to_string(),
        };
        let back: Envelope = serde_json::from_str(&e.to_jsonl_line()).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn event_round_trip_all_kinds() {
        for ev in [
            DispositionEvent::accepted("a".to_string(), 1, "h".to_string(), "o".to_string(), 0),
            DispositionEvent::attempted("a".to_string(), 2, "h".to_string(), "o".to_string(), 0),
            DispositionEvent::queued("a".to_string(), 3, "h".to_string(), "o".to_string(), 0),
            DispositionEvent::delivered("a".to_string(), 4, "h".to_string(), "o".to_string(), 0),
            DispositionEvent::delivery_failed(
                "a".to_string(),
                5,
                "h".to_string(),
                "o".to_string(),
                0,
                "wake".to_string(),
            ),
        ] {
            let back: DispositionEvent = serde_json::from_str(&ev.to_jsonl_line()).unwrap();
            assert_eq!(ev, back);
        }
    }

    #[test]
    fn summary_round_trip() {
        let s = SummaryRecord {
            v: 1,
            correlation_id: "s".to_string(),
            state: SummaryState::Failed,
            attempts: 3,
            last_event: Some(EventKind::DeliveryFailed),
            last_attempt_at: Some(9),
            first_delivered_at: None,
            expires_at: None,
            authored_at: 1,
            origin: "o".to_string(),
            witness: Some("h".to_string()),
        };
        let back: SummaryRecord = serde_json::from_str(&s.to_jsonl_line()).unwrap();
        assert_eq!(s, back);
    }

    // ------- constructors enforce the reason invariant -------

    #[test]
    fn reasonless_constructors_set_none() {
        assert_eq!(DispositionEvent::accepted("a".into(), 1, "h".into(), "o".into(), 0).reason, None);
        assert_eq!(DispositionEvent::attempted("a".into(), 1, "h".into(), "o".into(), 0).reason, None);
        assert_eq!(DispositionEvent::queued("a".into(), 1, "h".into(), "o".into(), 0).reason, None);
        assert_eq!(DispositionEvent::delivered("a".into(), 1, "h".into(), "o".into(), 0).reason, None);
    }

    #[test]
    fn delivery_failed_constructor_sets_reason() {
        let ev = DispositionEvent::delivery_failed("a".into(), 1, "h".into(), "o".into(), 0, "wake".into());
        assert_eq!(ev.event, EventKind::DeliveryFailed);
        assert_eq!(ev.reason.as_deref(), Some("wake"));
    }

    #[test]
    fn constructors_set_witness_and_origin_distinctly() {
        // witness ≠ origin must land in the right fields (a swap cannot pass).
        let ev = DispositionEvent::accepted("a".into(), 1, "wit".into(), "org".into(), 0);
        assert_eq!(ev.witness, "wit");
        assert_eq!(ev.origin, "org");
    }

    // ------- enum serde spellings -------

    #[test]
    fn event_kind_serializes_kebab_case() {
        assert_eq!(serde_json::to_string(&EventKind::Accepted).unwrap(), "\"accepted\"");
        assert_eq!(serde_json::to_string(&EventKind::Attempted).unwrap(), "\"attempted\"");
        assert_eq!(serde_json::to_string(&EventKind::Queued).unwrap(), "\"queued\"");
        assert_eq!(serde_json::to_string(&EventKind::Delivered).unwrap(), "\"delivered\"");
        assert_eq!(
            serde_json::to_string(&EventKind::DeliveryFailed).unwrap(),
            "\"delivery-failed\""
        );
    }

    #[test]
    fn summary_state_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&SummaryState::Pending).unwrap(), "\"pending\"");
        assert_eq!(serde_json::to_string(&SummaryState::Delivered).unwrap(), "\"delivered\"");
        assert_eq!(serde_json::to_string(&SummaryState::Failed).unwrap(), "\"failed\"");
        assert_eq!(serde_json::to_string(&SummaryState::Expired).unwrap(), "\"expired\"");
    }
}
