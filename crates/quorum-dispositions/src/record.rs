//! The wire-shape reflections + their enums (qd–qf transition v1, R8–R14).
//!
//! Field DECLARATION / emission order == the byte-exact wire key order pinned by
//! the format doc (see the crate-root doc for why order is load-bearing). Do not
//! reorder without updating the format contract and the golden test.
//!
//! # The ruled model (R8/R8a/R8b events; R14 normalization)
//!
//! `dispositions.jsonl` is an **append-only log of typed events**, one row per
//! recorded moment in an envelope's life at this host — NEVER a state record.
//! "First terminal wins" is dead ([`DispositionEvent`], the five [`EventKind`]
//! types). State is a **view** ([`SummaryRecord`] / [`SummaryState`]): the
//! summary carries the coarse 4-state enum UNCHANGED, plus `last_event` for
//! detail, and is computed by the projection (see `project.rs`). Idempotence
//! keys on a `delivered` event EXISTING (see [`crate::has_delivered`]), never
//! "any terminal."
//!
//! # R14 — FULLY NORMALIZED event rows (N13); R15 — the delivered body binding
//!
//! Event rows are `{v, correlation_id, event, created_at}` + a single per-variant
//! tail: a required machine-readable `class` on `delivery-failed` AND `refused`
//! (identical payload shape today — fine, R14.5; they may diverge later), and a
//! required `body_digest` on `delivered` (hex sha-256 of the parsed body — R15,
//! Contract Amendment 6; the integrity binding of what content landed). The two
//! plain variants (`attempted`/`queued`) carry no tail. There
//! is NO `witness`, NO copied `origin`, NO copied `authored_at` on event rows —
//! those were denormalization (R11.3 superseded by R14.2). Provenance is the
//! CONTAINER the row lives in (a local file ⇒ this host, a mirror ⇒ the path's
//! host); an emitter MAY attach a computed `source` column at union/emission (a
//! view concern, never storage). The envelope ([`Envelope`], §1) is the single
//! normalized HOME of `origin`/`authored_at`; events JOIN to it by
//! `correlation_id`. `created_at` = when THIS host recorded the event (R14.1) —
//! for outcome events that is OBSERVATION time (no retro-dating).
//!
//! `class` is the machine-readable failure/refusal discriminator (R14a pin 1 —
//! `failed{wake}` = `delivery-failed` class `"wake"`). A human-detail field
//! named `reason` is RESERVED (optional, any variant) but UNUSED in v1 — not a
//! Rust field here (YAGNI); documented reserved in the format doc.

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A `log.jsonl` row — an envelope qd ORIGINATED (format doc §1). UNCHANGED by
/// R14: it is the single normalized HOME of `origin`/`authored_at` (events join
/// for them), and it travels to peers as an `--inbound-envelope` payload where
/// no container derives `origin` — so it carries its own fields (a traveling
/// record, not denormalization).
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
    /// ruled (R9.1/R14.4): an event log holds many rows per envelope — the field
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

/// The five event types recorded in `dispositions.jsonl` (format doc §2, R8b
/// full funnel; R14.3 retires `accepted`, adds `refused`). Each is past tense —
/// a moment qd recorded, never a speculative/planned state. The set is OPEN:
/// future recorded facts arrive as NEW variants, never as new values of a shared
/// "outcome" field (R8a).
///
/// This enum is the DISCRIMINANT only — the discriminated-union payloads live on
/// [`DispositionEvent`] (R14.5). It is what the projection folds over.
///
/// Serde (kebab-case): `attempted` / `queued` / `delivered` / `delivery-failed`
/// / `refused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    /// A delivery attempt was ADMITTED AND STARTED. Each retry is a fresh
    /// `attempted` event (R14.3: `attempted` marks admission-and-start; the old
    /// `accepted` type is retired).
    Attempted,
    /// The attempt placed the message into the target's delivery queue /
    /// awaiting idle or wake — a recorded moment, possibly minutes before the
    /// prose lands (busy session, waking session).
    Queued,
    /// The prose LANDED in the session. Existence of this event IS the
    /// irreversible delivered fact. Carries a REQUIRED `body_digest` (hex sha-256
    /// of the parsed body — R15, the integrity binding of the delivery act).
    /// Serializes as `"delivered"`.
    Delivered,
    /// The attempt definitively did not arrive. Carries a REQUIRED `class` (the
    /// machine-readable failure class; `failed{wake}` = `delivery-failed` class
    /// `"wake"`). Serializes as `"delivery-failed"`.
    DeliveryFailed,
    /// A parse-valid inbound door / pre-flight refusal (mis-addressed,
    /// past-expiry, ambiguous, no-live-receive-path). Carries a REQUIRED `class`
    /// (the refusal class). PENDING-class in the fold — refused = never left ≠
    /// failed (R14.3).
    Refused,
}

/// A stored `dispositions.jsonl` row — one TYPED EVENT, as a DISCRIMINATED UNION
/// (format doc §2, R8/R8a/R8b + R14.5 + R15). Never a state record: state is a
/// view ([`SummaryRecord`]).
///
/// FULLY NORMALIZED (R14.2, invariant N13). Every variant carries the common
/// fields (`v`, `correlation_id`, `created_at`) plus its own tail:
/// [`Self::DeliveryFailed`] and [`Self::Refused`] each carry a required machine
/// `class`; [`Self::Delivered`] carries a required `body_digest` (R15, the
/// integrity binding of the delivery act); [`Self::Attempted`] and
/// [`Self::Queued`] carry only the common fields. Each per-variant tail exists
/// ONLY on its variant BY CONSTRUCTION — the type system enforces what the old
/// runtime forbidden-field check did (R14.5). Delivery-failed and refused have
/// IDENTICAL payload shapes today; that is fine and they may diverge later.
///
/// DROPPED vs the pre-R14 shape: `witness`, the copied `origin`, and the copied
/// `authored_at` — those were denormalization (R11.3 superseded). Provenance is
/// the container; `origin`/`authored_at` live on the [`Envelope`] and join by
/// `correlation_id`. Event rows carry NO `expires_at` (ruled).
///
/// Wire key order (byte-exact): `v, correlation_id, event, created_at` then the
/// per-variant tail (`body_digest` on delivered, `class` on delivery-failed /
/// refused), omitted on attempted/queued. See the manual
/// [`Serialize`]/[`Deserialize`] impls below —
/// serde's internally tagged enum would put the tag FIRST (wrong: we need `v`
/// first, `event` third), so the impls emit the exact order by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispositionEvent {
    /// A delivery attempt started.
    Attempted { correlation_id: String, created_at: i64 },
    /// Placed in the target's queue / awaiting idle or wake.
    Queued { correlation_id: String, created_at: i64 },
    /// The prose landed. Carries a REQUIRED `body_digest` (hex sha-256 of the
    /// envelope's PARSED `body` string — R15, Contract Amendment 6): the
    /// integrity binding of the delivered ACT (what content landed here). The
    /// leaf STORES the hex string only; the dispatch side computes it (the leaf
    /// stays pure — no crypto dep). It is NOT a join-avoidance copy: its purpose
    /// is exactly the case where the join target (mirror envelope) is absent or
    /// untrusted, so the door can refuse a same-id/different-body presentation.
    Delivered { correlation_id: String, created_at: i64, body_digest: String },
    /// The attempt definitively did not arrive; machine `class` REQUIRED.
    DeliveryFailed { correlation_id: String, created_at: i64, class: String },
    /// A parse-valid inbound / pre-flight refusal; refusal `class` REQUIRED.
    Refused { correlation_id: String, created_at: i64, class: String },
}

/// The coarse published summary state (format doc §3). UNCHANGED by R8b (guard
/// 2) and R14: the 4-state enum stays `pending | delivered | failed | expired`
/// so frame's simple views are stable; the events are the fine grain underneath.
/// Serialized lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SummaryState {
    /// No delivered event yet, pre-expiry, latest event is not a failure
    /// (includes `attempted`/`queued`/`refused`/none — refused is pending-class).
    Pending,
    /// A `delivered` event exists (irreversible).
    Delivered,
    /// Latest event is `delivery-failed`, no delivered event, not expired.
    Failed,
    /// No delivered event and now past the envelope's `expires_at`.
    Expired,
}

/// The PUBLISHED per-id summary row — `qd dispositions` DEFAULT output (format
/// doc §3a, shape ruled by R10/R11/R14), the coarse view frame projects over,
/// versioned with the contract. (`qd dispositions --events` emits the raw
/// [`DispositionEvent`] funnel instead.)
///
/// It carries the 4-state [`SummaryState`] UNCHANGED plus `last_event` for
/// detail (R8b guard 2), the folded analytics fields (attempts,
/// last_attempt_at, first_delivered_at), and — from the JOINED envelope only
/// (R14.2) — `expires_at`, `authored_at`, `origin`.
///
/// R14.2 honest-null: `origin` and `authored_at` come ONLY from the joined
/// envelope (events no longer carry them). An orphan-event summary (no envelope
/// in scope) therefore has `origin`, `authored_at`, AND `expires_at` all `null`
/// — no more copy-from-first-event. `last_event` is `null` iff no events exist
/// (R11.1 core survives).
///
/// Wire key order (byte-exact): `v, correlation_id, state, attempts, last_event,
/// last_attempt_at, first_delivered_at, expires_at, authored_at, origin`. The
/// nullable fields (`last_event`, `last_attempt_at`, `first_delivered_at`,
/// `expires_at`, `authored_at`, `origin`) are emitted as JSON `null` when absent
/// (stable columns for the DuckDB projection — NOT skipped).
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
    /// The latest event by `created_at` (later-in-input wins full ties, R14.2) —
    /// detail beneath the coarse `state`; `null` iff no events exist (R11.1).
    pub last_event: Option<EventKind>,
    /// max `created_at` over `attempted` events; `null` if none.
    pub last_attempt_at: Option<i64>,
    /// min `created_at` over `delivered` events; `null` if none.
    pub first_delivered_at: Option<i64>,
    /// epoch-ms from the joined envelope; `null` when the envelope is not in
    /// scope (an orphan-event summary — R14.2 honest null).
    pub expires_at: Option<i64>,
    /// epoch-ms origin timeline, from the joined envelope ONLY; `null` for an
    /// orphan-event summary (R14.2 — events no longer carry `authored_at`).
    pub authored_at: Option<i64>,
    /// The origin host id, from the joined envelope's `origin` ONLY; `null` for
    /// an orphan-event summary (R14.2 — events no longer carry `origin`, the
    /// copy was denormalization).
    pub origin: Option<String>,
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
    /// An `attempted` event (a delivery attempt admitted and started).
    pub fn attempted(correlation_id: String, created_at: i64) -> Self {
        DispositionEvent::Attempted { correlation_id, created_at }
    }

    /// A `queued` event (placed in the target's queue / awaiting idle or wake).
    pub fn queued(correlation_id: String, created_at: i64) -> Self {
        DispositionEvent::Queued { correlation_id, created_at }
    }

    /// A `delivered` event (the prose landed). `body_digest` (the hex sha-256 of
    /// the envelope's PARSED body string, computed by the dispatch side) is
    /// REQUIRED and set here — the R15 integrity binding (Contract Amendment 6).
    pub fn delivered(correlation_id: String, created_at: i64, body_digest: String) -> Self {
        DispositionEvent::Delivered { correlation_id, created_at, body_digest }
    }

    /// A `delivery-failed` event. `class` (the machine failure class, e.g.
    /// `"wake"`) is REQUIRED and set here — the type-system counterpart to the
    /// per-event-type validation (R14.5 / R14a).
    pub fn delivery_failed(correlation_id: String, created_at: i64, class: String) -> Self {
        DispositionEvent::DeliveryFailed { correlation_id, created_at, class }
    }

    /// A `refused` event (a parse-valid inbound / pre-flight refusal). `class`
    /// (the refusal class, e.g. `"no-live-receive-path"`) is REQUIRED and set
    /// here. PENDING-class in the fold (R14.3).
    pub fn refused(correlation_id: String, created_at: i64, class: String) -> Self {
        DispositionEvent::Refused { correlation_id, created_at, class }
    }

    /// The event-kind discriminant (what the projection folds over). Callers can
    /// no longer read a `.event` struct field directly — the union is per-variant.
    pub fn kind(&self) -> EventKind {
        match self {
            DispositionEvent::Attempted { .. } => EventKind::Attempted,
            DispositionEvent::Queued { .. } => EventKind::Queued,
            DispositionEvent::Delivered { .. } => EventKind::Delivered,
            DispositionEvent::DeliveryFailed { .. } => EventKind::DeliveryFailed,
            DispositionEvent::Refused { .. } => EventKind::Refused,
        }
    }

    /// The correlation id (the join key), common to every variant.
    pub fn correlation_id(&self) -> &str {
        match self {
            DispositionEvent::Attempted { correlation_id, .. }
            | DispositionEvent::Queued { correlation_id, .. }
            | DispositionEvent::Delivered { correlation_id, .. }
            | DispositionEvent::DeliveryFailed { correlation_id, .. }
            | DispositionEvent::Refused { correlation_id, .. } => correlation_id,
        }
    }

    /// When THIS host recorded the event (R14.1), common to every variant.
    pub fn created_at(&self) -> i64 {
        match self {
            DispositionEvent::Attempted { created_at, .. }
            | DispositionEvent::Queued { created_at, .. }
            | DispositionEvent::Delivered { created_at, .. }
            | DispositionEvent::DeliveryFailed { created_at, .. }
            | DispositionEvent::Refused { created_at, .. } => *created_at,
        }
    }

    /// The `body_digest` on a [`Self::Delivered`] event (R15 integrity binding),
    /// `None` on every other variant. The door reads this back to compare a
    /// presented body against the one already bound to the id.
    pub fn body_digest(&self) -> Option<&str> {
        match self {
            DispositionEvent::Delivered { body_digest, .. } => Some(body_digest),
            _ => None,
        }
    }

    /// The compact single-line JSON for this row (no trailing newline).
    /// Byte-exact by construction; the per-variant tail (`body_digest` on
    /// delivered, `class` on delivery-failed / refused) is emitted last, omitted
    /// on attempted/queued.
    pub fn to_jsonl_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// ---- Manual (de)serialization: pins `{v, correlation_id, event, created_at,
// [class | body_digest]}` byte order. serde's internally-tagged enum puts the tag
// FIRST, so we emit the fixed head + the per-variant tail by hand (R14.5). The
// tail is `class` on delivery-failed / refused and `body_digest` on delivered
// (R15); the three keys are mutually exclusive by construction. ----

impl Serialize for DispositionEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Field count = 4 common + a single per-variant tail on delivered
        // (body_digest) / delivery-failed (class) / refused (class).
        let tail = matches!(
            self,
            DispositionEvent::Delivered { .. }
                | DispositionEvent::DeliveryFailed { .. }
                | DispositionEvent::Refused { .. }
        );
        let mut s = serializer.serialize_struct("DispositionEvent", if tail { 5 } else { 4 })?;
        // Head, in the pinned order: v, correlation_id, event, created_at.
        s.serialize_field("v", &1u32)?;
        s.serialize_field("correlation_id", self.correlation_id())?;
        s.serialize_field("event", &self.kind())?;
        s.serialize_field("created_at", &self.created_at())?;
        // Per-variant tail, last: `body_digest` on delivered, `class` on
        // delivery-failed / refused; nothing on the two plain variants.
        match self {
            DispositionEvent::Delivered { body_digest, .. } => {
                s.serialize_field("body_digest", body_digest)?;
            }
            DispositionEvent::DeliveryFailed { class, .. }
            | DispositionEvent::Refused { class, .. } => {
                s.serialize_field("class", class)?;
            }
            _ => {}
        }
        s.end()
    }
}

impl<'de> Deserialize<'de> for DispositionEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EventVisitor;

        impl<'de> Visitor<'de> for EventVisitor {
            type Value = DispositionEvent;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a disposition event object")
            }

            fn visit_map<M>(self, mut map: M) -> Result<DispositionEvent, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut v: Option<u32> = None;
                let mut correlation_id: Option<String> = None;
                let mut event: Option<EventKind> = None;
                let mut created_at: Option<i64> = None;
                let mut class: Option<String> = None;
                let mut body_digest: Option<String> = None;

                // Accept keys in any order (JSON is unordered on the way in); the
                // ORDER contract is an emission property, pinned in Serialize.
                // A DUPLICATE key or an UNKNOWN key is rejected — the latter is
                // the discriminated-union "no foreign fields" rule (R14.5): a
                // variant carrying a field foreign to it is CORRUPT.
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "v" => set_once(&mut v, &mut map, "v")?,
                        "correlation_id" => set_once(&mut correlation_id, &mut map, "correlation_id")?,
                        "event" => set_once(&mut event, &mut map, "event")?,
                        "created_at" => set_once(&mut created_at, &mut map, "created_at")?,
                        "class" => set_once(&mut class, &mut map, "class")?,
                        "body_digest" => set_once(&mut body_digest, &mut map, "body_digest")?,
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &["v", "correlation_id", "event", "created_at", "class", "body_digest"],
                            ));
                        }
                    }
                }

                let _v = v.ok_or_else(|| de::Error::missing_field("v"))?;
                let correlation_id =
                    correlation_id.ok_or_else(|| de::Error::missing_field("correlation_id"))?;
                let event = event.ok_or_else(|| de::Error::missing_field("event"))?;
                let created_at = created_at.ok_or_else(|| de::Error::missing_field("created_at"))?;

                // Per-variant tail, enforced (the discriminated-union invariant):
                //   delivered       → REQUIRES `body_digest`, FORBIDS `class` (R15)
                //   delivery-failed  → REQUIRES `class`, FORBIDS `body_digest`
                //   refused          → REQUIRES `class`, FORBIDS `body_digest`
                //   attempted/queued → FORBID both.
                match event {
                    EventKind::Delivered => {
                        if class.is_some() {
                            return Err(de::Error::custom(
                                "a `delivered` event carries a foreign `class` field",
                            ));
                        }
                        let body_digest =
                            body_digest.ok_or_else(|| de::Error::missing_field("body_digest"))?;
                        Ok(DispositionEvent::Delivered { correlation_id, created_at, body_digest })
                    }
                    EventKind::DeliveryFailed => {
                        if body_digest.is_some() {
                            return Err(de::Error::custom(
                                "a `delivery-failed` event carries a foreign `body_digest` field",
                            ));
                        }
                        let class = class.ok_or_else(|| de::Error::missing_field("class"))?;
                        Ok(DispositionEvent::DeliveryFailed { correlation_id, created_at, class })
                    }
                    EventKind::Refused => {
                        if body_digest.is_some() {
                            return Err(de::Error::custom(
                                "a `refused` event carries a foreign `body_digest` field",
                            ));
                        }
                        let class = class.ok_or_else(|| de::Error::missing_field("class"))?;
                        Ok(DispositionEvent::Refused { correlation_id, created_at, class })
                    }
                    kind => {
                        // attempted / queued: neither tail field is permitted.
                        if class.is_some() {
                            return Err(de::Error::custom(
                                "a plain event (attempted/queued) carries a foreign `class` field",
                            ));
                        }
                        if body_digest.is_some() {
                            return Err(de::Error::custom(
                                "a plain event (attempted/queued) carries a foreign `body_digest` field",
                            ));
                        }
                        Ok(match kind {
                            EventKind::Attempted => {
                                DispositionEvent::Attempted { correlation_id, created_at }
                            }
                            EventKind::Queued => {
                                DispositionEvent::Queued { correlation_id, created_at }
                            }
                            // Delivered / DeliveryFailed / Refused handled above.
                            _ => unreachable!(),
                        })
                    }
                }
            }
        }

        deserializer.deserialize_map(EventVisitor)
    }
}

/// Deserialize a value into `slot`, rejecting a duplicate key.
fn set_once<'de, T, M>(slot: &mut Option<T>, map: &mut M, field: &'static str) -> Result<(), M::Error>
where
    T: Deserialize<'de>,
    M: MapAccess<'de>,
{
    if slot.is_some() {
        return Err(de::Error::duplicate_field(field));
    }
    *slot = Some(map.next_value()?);
    Ok(())
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
    fn attempted_event_golden_line() {
        let ev = DispositionEvent::attempted("01ABC".to_string(), 1_781_241_500_000);
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01ABC","event":"attempted","created_at":1781241500000}"#
        );
    }

    #[test]
    fn queued_event_golden_line() {
        let ev = DispositionEvent::queued("01ABC".to_string(), 1_781_241_500_000);
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01ABC","event":"queued","created_at":1781241500000}"#
        );
    }

    #[test]
    fn delivered_event_golden_line() {
        // delivered → body_digest tail present, last (R15). The digest is the hex
        // sha-256 of the parsed body, computed by the dispatch side; the leaf just
        // stores + emits the string.
        let ev = DispositionEvent::delivered(
            "01ABC".to_string(),
            1_781_241_500_500,
            "a591a6d40bf420404a011733cfb7b190d62c65bf0bcda32b57b277d9ad9f146e".to_string(),
        );
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01ABC","event":"delivered","created_at":1781241500500,"body_digest":"a591a6d40bf420404a011733cfb7b190d62c65bf0bcda32b57b277d9ad9f146e"}"#
        );
    }

    #[test]
    fn delivery_failed_event_golden_line() {
        // delivery-failed → class present, last; event serializes kebab-case.
        let ev =
            DispositionEvent::delivery_failed("01DEF".to_string(), 1_781_241_600_000, "wake".to_string());
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01DEF","event":"delivery-failed","created_at":1781241600000,"class":"wake"}"#
        );
    }

    #[test]
    fn refused_event_golden_line() {
        // refused → class present, last (same shape as delivery-failed today).
        let ev = DispositionEvent::refused(
            "01GHI".to_string(),
            1_781_241_700_000,
            "no-live-receive-path".to_string(),
        );
        assert_eq!(
            ev.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01GHI","event":"refused","created_at":1781241700000,"class":"no-live-receive-path"}"#
        );
    }

    #[test]
    fn summary_record_golden_line() {
        // A folded delivered summary (the fail→retry→succeed outcome), origin +
        // authored_at from the joined envelope.
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
    fn summary_orphan_triple_null_golden_line() {
        // An orphan-event summary: no envelope in scope → origin, authored_at,
        // AND expires_at are ALL null (R14.2 honest null). The delivered event
        // still drives state=delivered and last_event.
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
    fn summary_record_zero_events_with_envelope_golden_line() {
        // A zero-events pending summary WITH an envelope in scope: {last_event}
        // is null (no events), the analytics columns null, but origin/authored_at
        // /expires_at come from the envelope (present).
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
            DispositionEvent::attempted("a".to_string(), 2),
            DispositionEvent::queued("a".to_string(), 3),
            DispositionEvent::delivered("a".to_string(), 4, "deadbeef".to_string()),
            DispositionEvent::delivery_failed("a".to_string(), 5, "wake".to_string()),
            DispositionEvent::refused("a".to_string(), 6, "ambiguous".to_string()),
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
            authored_at: Some(1),
            origin: Some("o".to_string()),
        };
        let back: SummaryRecord = serde_json::from_str(&s.to_jsonl_line()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn summary_orphan_round_trip() {
        // The triple-null orphan summary round-trips (nulls → None).
        let s = SummaryRecord {
            v: 1,
            correlation_id: "orphan".to_string(),
            state: SummaryState::Delivered,
            attempts: 0,
            last_event: Some(EventKind::Delivered),
            last_attempt_at: None,
            first_delivered_at: Some(7),
            expires_at: None,
            authored_at: None,
            origin: None,
        };
        let back: SummaryRecord = serde_json::from_str(&s.to_jsonl_line()).unwrap();
        assert_eq!(s, back);
    }

    // ------- constructors + accessors -------

    #[test]
    fn constructors_and_accessors() {
        let att = DispositionEvent::attempted("a".into(), 1);
        assert_eq!(att.kind(), EventKind::Attempted);
        assert_eq!(att.correlation_id(), "a");
        assert_eq!(att.created_at(), 1);
        assert_eq!(att.body_digest(), None, "no body_digest on a plain variant");

        let dv = DispositionEvent::delivered("a".into(), 4, "abc123".into());
        assert_eq!(dv.kind(), EventKind::Delivered);
        assert_eq!(dv.body_digest(), Some("abc123"), "delivered exposes its body_digest (R15)");
        assert!(matches!(dv, DispositionEvent::Delivered { ref body_digest, .. } if body_digest == "abc123"));

        let df = DispositionEvent::delivery_failed("a".into(), 2, "wake".into());
        assert_eq!(df.kind(), EventKind::DeliveryFailed);
        assert_eq!(df.body_digest(), None, "no body_digest on delivery-failed");
        assert!(matches!(df, DispositionEvent::DeliveryFailed { ref class, .. } if class == "wake"));

        let rf = DispositionEvent::refused("a".into(), 3, "ambiguous".into());
        assert_eq!(rf.kind(), EventKind::Refused);
        assert!(matches!(rf, DispositionEvent::Refused { ref class, .. } if class == "ambiguous"));
    }

    // ------- enum serde spellings -------

    #[test]
    fn event_kind_serializes_kebab_case() {
        assert_eq!(serde_json::to_string(&EventKind::Attempted).unwrap(), "\"attempted\"");
        assert_eq!(serde_json::to_string(&EventKind::Queued).unwrap(), "\"queued\"");
        assert_eq!(serde_json::to_string(&EventKind::Delivered).unwrap(), "\"delivered\"");
        assert_eq!(
            serde_json::to_string(&EventKind::DeliveryFailed).unwrap(),
            "\"delivery-failed\""
        );
        assert_eq!(serde_json::to_string(&EventKind::Refused).unwrap(), "\"refused\"");
    }

    #[test]
    fn summary_state_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&SummaryState::Pending).unwrap(), "\"pending\"");
        assert_eq!(serde_json::to_string(&SummaryState::Delivered).unwrap(), "\"delivered\"");
        assert_eq!(serde_json::to_string(&SummaryState::Failed).unwrap(), "\"failed\"");
        assert_eq!(serde_json::to_string(&SummaryState::Expired).unwrap(), "\"expired\"");
    }

    // ------- discriminated-union deserialize invariants (mirror the parser) ---

    #[test]
    fn deserialize_rejects_foreign_class_on_non_failure_variant() {
        // attempted/queued carry NO tail; delivered carries `body_digest`, NOT
        // `class`. A `class` on any of them is a foreign field ⇒ corrupt. For
        // delivered we also give it a valid body_digest so the ONLY defect is the
        // foreign class (proving `class` alone rejects it, not the missing digest).
        for kind in ["attempted", "queued"] {
            let line = format!(
                r#"{{"v":1,"correlation_id":"x","event":"{}","created_at":1,"class":"nope"}}"#,
                kind
            );
            assert!(
                serde_json::from_str::<DispositionEvent>(&line).is_err(),
                "{kind} with a foreign class must be rejected"
            );
        }
        let delivered_with_class = r#"{"v":1,"correlation_id":"x","event":"delivered","created_at":1,"body_digest":"abc","class":"nope"}"#;
        assert!(
            serde_json::from_str::<DispositionEvent>(delivered_with_class).is_err(),
            "a delivered row carrying `class` is corrupt even with a valid body_digest"
        );
    }

    #[test]
    fn deserialize_requires_class_tail() {
        let df_no_class = r#"{"v":1,"correlation_id":"x","event":"delivery-failed","created_at":1}"#;
        assert!(serde_json::from_str::<DispositionEvent>(df_no_class).is_err());
        let refused_no_class = r#"{"v":1,"correlation_id":"x","event":"refused","created_at":1}"#;
        assert!(serde_json::from_str::<DispositionEvent>(refused_no_class).is_err());
    }

    // ------- R15 body_digest discriminated-union invariants -------

    #[test]
    fn deserialize_requires_body_digest_on_delivered() {
        // A `delivered` row WITHOUT body_digest is corrupt (R15: the tail is
        // REQUIRED). A well-formed one round-trips.
        let no_digest = r#"{"v":1,"correlation_id":"x","event":"delivered","created_at":1}"#;
        assert!(
            serde_json::from_str::<DispositionEvent>(no_digest).is_err(),
            "delivered sans body_digest is corrupt"
        );
        let good = r#"{"v":1,"correlation_id":"x","event":"delivered","created_at":1,"body_digest":"abc"}"#;
        let ev: DispositionEvent = serde_json::from_str(good).unwrap();
        assert_eq!(ev.body_digest(), Some("abc"));
    }

    #[test]
    fn deserialize_rejects_body_digest_on_non_delivered_variants() {
        // `body_digest` is foreign to every variant except delivered. A row of any
        // other kind carrying it is corrupt (even if its own required tail is
        // present, for delivery-failed/refused).
        for (kind, tail) in [
            ("attempted", ""),
            ("queued", ""),
            ("delivery-failed", r#","class":"wake""#),
            ("refused", r#","class":"ambiguous""#),
        ] {
            let line = format!(
                r#"{{"v":1,"correlation_id":"x","event":"{}","created_at":1,"body_digest":"abc"{}}}"#,
                kind, tail
            );
            assert!(
                serde_json::from_str::<DispositionEvent>(&line).is_err(),
                "{kind} carrying a foreign body_digest must be rejected"
            );
        }
    }

    #[test]
    fn deserialize_rejects_unknown_field() {
        // `reason` is RESERVED but UNUSED in v1 → an unknown field → corrupt (the
        // delivered row is otherwise well-formed with its body_digest, so the ONLY
        // defect is the foreign `reason`).
        let with_reason = r#"{"v":1,"correlation_id":"x","event":"delivered","created_at":1,"body_digest":"abc","reason":"nope"}"#;
        assert!(serde_json::from_str::<DispositionEvent>(with_reason).is_err());
    }
}
