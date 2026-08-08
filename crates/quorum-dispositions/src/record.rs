//! The three wire-shape reflections + their enums.
//!
//! Field DECLARATION order == the byte-exact wire key order pinned by the format
//! doc (see the crate-root doc for why declaration order is load-bearing). Do
//! not reorder fields without updating the format contract and the golden test.

use serde::{Deserialize, Serialize};

/// A `log.jsonl` row — an envelope qd ORIGINATED (format doc §1).
///
/// Wire key order (byte-exact): `v, correlation_id, authored_at, expires_at,
/// target, authority, body`. `body` is last (largest + most variable field;
/// keeps the head of every line cheap to scan).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Version marker — always `1`.
    pub v: u32,
    /// The envelope id: minted once at origin, travels verbatim, the
    /// idempotency + disposition join key. Never a content hash.
    pub correlation_id: String,
    /// epoch-ms, stamped once at origin (N10 authored timeline).
    pub authored_at: i64,
    /// epoch-ms. Default `authored_at + 12h`; `expired` is minted from ABSENCE
    /// past this value. Policy travels with the message.
    pub expires_at: i64,
    /// The address as given by the caller (`name | stable_id | name@host`).
    /// Operational record; NOT load-bearing for the disposition join.
    pub target: String,
    /// Origin host id (this qd's host). Disambiguates origin when a peer's log
    /// is read from `remote/<host>/`.
    pub authority: String,
    /// The opaque prose, verbatim, delivered as one message. qd never parses it.
    pub body: String,
}

/// A stored `dispositions.jsonl` row — a witnessed TERMINAL fact (format doc §2).
///
/// Only terminal, witnessed states are ever stored: `delivered` / `failed`
/// ([`StoredState`]). `pending` and `expired` are DERIVED at the query surface
/// (§3), never authored here.
///
/// Wire key order (byte-exact): `v, correlation_id, state, authored_at,
/// witnessed_at, authority, reason` (`reason` omitted entirely when absent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disposition {
    /// Version marker — always `1`.
    pub v: u32,
    /// The envelope's origin-minted id. The idempotency key: a terminal row
    /// present for this id ⇒ inbound-mode no-op success. First terminal wins.
    pub correlation_id: String,
    /// Witnessed terminal only (`delivered` | `failed`).
    pub state: StoredState,
    /// epoch-ms, copied from the envelope at witness time so this file is
    /// self-contained for terminal states (the envelope may live in a mirror).
    pub authored_at: i64,
    /// epoch-ms, stamped by THIS authority at the moment of witnessing = the
    /// moment qd accepted it (N10 / Amendment 1). Effects order solely by this.
    pub witnessed_at: i64,
    /// The witnessing host id (this qd). In a `--host`/`--all` union,
    /// disambiguates which host witnessed.
    pub authority: String,
    /// OPTIONAL. For `failed`, the failure class (e.g. `"wake"`). Absent for
    /// `delivered`. Omitted entirely from the wire when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The stored terminal states (format doc §2). Serialized lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredState {
    /// The prose landed.
    Delivered,
    /// Attempted and definitively did not arrive (carries a `reason`).
    Failed,
}

/// The emitted 4-state record state (format doc §3). Serialized lowercase.
///
/// `pending` = absence of a terminal pre-expiry; `delivered`/`failed` =
/// witnessed terminal; `expired` = absence post-expiry (view-computed). "Silence
/// is pending, never success."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordState {
    /// Absence of a terminal record, before `expires_at`.
    Pending,
    /// Witnessed terminal: the prose landed.
    Delivered,
    /// Witnessed terminal: attempted, definitively did not arrive.
    Failed,
    /// Absence of a terminal record, at/after `expires_at` (view-computed).
    Expired,
}

/// The PUBLISHED `qd dispositions` output row (format doc §3) — the one data
/// shape frame projects over, versioned with the provider contract.
///
/// Wire key order (byte-exact): `v, correlation_id, state, authored_at,
/// witnessed_at, authority, reason`. `witnessed_at` is present as JSON `null`
/// for `pending`/`expired` (a STABLE column for the DuckDB projection — NOT
/// skipped); `reason` is omitted when absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmittedRecord {
    /// Version marker — always `1`.
    pub v: u32,
    /// The envelope's origin-minted id.
    pub correlation_id: String,
    /// One of `pending` / `delivered` / `failed` / `expired`.
    pub state: RecordState,
    /// epoch-ms, origin (N10 authored timeline).
    pub authored_at: i64,
    /// epoch-ms for `delivered`/`failed`; `null` for `pending`/`expired` (no
    /// witness). Emitted as `null` when `None` — a stable column, NOT skipped.
    pub witnessed_at: Option<i64>,
    /// The origin authority for `pending`/`expired`; the witnessing authority
    /// for `delivered`/`failed`.
    pub authority: String,
    /// OPTIONAL; present for `failed`. Omitted from the wire when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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

impl Disposition {
    /// The compact single-line JSON for this row (no trailing newline).
    /// Byte-exact by construction; `reason` omitted when `None`.
    pub fn to_jsonl_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl EmittedRecord {
    /// The compact single-line JSON for this row (no trailing newline).
    /// Byte-exact by construction; `witnessed_at` emitted as `null` when `None`,
    /// `reason` omitted when `None`.
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
            authority: "brano".to_string(),
            body: "hello world".to_string(),
        };
        assert_eq!(
            e.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01ABC","authored_at":1781241500000,"expires_at":1781284700000,"target":"alpha@brano","authority":"brano","body":"hello world"}"#
        );
    }

    #[test]
    fn disposition_delivered_no_reason_golden_line() {
        let d = Disposition {
            v: 1,
            correlation_id: "01ABC".to_string(),
            state: StoredState::Delivered,
            authored_at: 1_781_241_500_000,
            witnessed_at: 1_781_241_500_500,
            authority: "brano".to_string(),
            reason: None,
        };
        // reason omitted entirely.
        assert_eq!(
            d.to_jsonl_line(),
            r#"{"v":1,"correlation_id":"01ABC","state":"delivered","authored_at":1781241500000,"witnessed_at":1781241500500,"authority":"brano"}"#
        );
    }

    #[test]
    fn disposition_failed_with_reason_golden_line() {
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
    fn emitted_pending_golden_line() {
        // witnessed_at present as null; reason omitted.
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
    fn emitted_delivered_golden_line() {
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
    fn emitted_failed_golden_line() {
        // reason present for failed; witnessed_at present (a number).
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
    fn emitted_expired_golden_line() {
        // witnessed_at present as null; reason omitted.
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

    // ------- round-trip: serialize → parse → equal -------

    #[test]
    fn envelope_round_trip() {
        let e = Envelope {
            v: 1,
            correlation_id: "rt".to_string(),
            authored_at: 10,
            expires_at: 20,
            target: "t".to_string(),
            authority: "a".to_string(),
            body: "b".to_string(),
        };
        let back: Envelope = serde_json::from_str(&e.to_jsonl_line()).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn disposition_round_trip_both_variants() {
        for d in [
            Disposition {
                v: 1,
                correlation_id: "d1".to_string(),
                state: StoredState::Delivered,
                authored_at: 1,
                witnessed_at: 2,
                authority: "a".to_string(),
                reason: None,
            },
            Disposition {
                v: 1,
                correlation_id: "d2".to_string(),
                state: StoredState::Failed,
                authored_at: 3,
                witnessed_at: 4,
                authority: "a".to_string(),
                reason: Some("wake".to_string()),
            },
        ] {
            let back: Disposition = serde_json::from_str(&d.to_jsonl_line()).unwrap();
            assert_eq!(d, back);
        }
    }

    #[test]
    fn emitted_round_trip_all_states() {
        for r in [
            EmittedRecord {
                v: 1,
                correlation_id: "p".to_string(),
                state: RecordState::Pending,
                authored_at: 1,
                witnessed_at: None,
                authority: "a".to_string(),
                reason: None,
            },
            EmittedRecord {
                v: 1,
                correlation_id: "d".to_string(),
                state: RecordState::Delivered,
                authored_at: 1,
                witnessed_at: Some(2),
                authority: "a".to_string(),
                reason: None,
            },
            EmittedRecord {
                v: 1,
                correlation_id: "f".to_string(),
                state: RecordState::Failed,
                authored_at: 1,
                witnessed_at: Some(2),
                authority: "a".to_string(),
                reason: Some("wake".to_string()),
            },
            EmittedRecord {
                v: 1,
                correlation_id: "e".to_string(),
                state: RecordState::Expired,
                authored_at: 1,
                witnessed_at: None,
                authority: "a".to_string(),
                reason: None,
            },
        ] {
            let back: EmittedRecord = serde_json::from_str(&r.to_jsonl_line()).unwrap();
            assert_eq!(r, back);
        }
    }

    #[test]
    fn state_enums_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&StoredState::Delivered).unwrap(),
            "\"delivered\""
        );
        assert_eq!(
            serde_json::to_string(&StoredState::Failed).unwrap(),
            "\"failed\""
        );
        assert_eq!(
            serde_json::to_string(&RecordState::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&RecordState::Expired).unwrap(),
            "\"expired\""
        );
    }
}
