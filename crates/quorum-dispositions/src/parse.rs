//! Torn-tail-tolerant JSONL parsing (format doc "common framing").
//!
//! The framing rule for every `*.jsonl` file here:
//! - one JSON object per line, UTF-8, `\n`-terminated;
//! - **torn-tail rule**: a final unterminated (partial) line is IGNORED — a
//!   concurrent appender may have written a prefix of the next record; that is
//!   NOT corruption;
//! - an unparseable **interior** line IS corruption (counted, never silently
//!   treated as absence-of-record);
//! - **version marker**: a row whose `v` != 1 is refused (counted corrupt) — a
//!   reader never guesses an unknown version;
//! - **per-event-type validation** (dispositions only, R8a): a `delivery-failed`
//!   row WITHOUT `reason`, or ANY OTHER event type WITH `reason`, is corrupt —
//!   the schema-per-event-type invariant is checked on read, tighter than a
//!   shared enum could express.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::record::{DispositionEvent, Envelope, EventKind};

/// The outcome of reading a JSONL file: the records that parsed, plus a count of
/// unparseable/version-rejected INTERIOR lines. A torn tail is NOT counted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadResult<T> {
    /// The successfully parsed rows, in file order.
    pub records: Vec<T>,
    /// Count of interior lines that were unparseable OR carried an unknown `v`
    /// OR violated a per-event-type invariant. The torn tail (final unterminated
    /// line) is deliberately excluded.
    pub corrupt_interior: u64,
}

/// Split `bytes` into the `\n`-TERMINATED lines only. The trailing segment after
/// the last `\n` (whether empty — a clean terminator — or a partial torn tail)
/// is dropped: it is never a complete record. This is the torn-tail rule.
fn terminated_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    // `split(b'\n')` yields N+1 pieces for N newlines; the LAST piece is the
    // bytes after the final `\n` (empty iff the input ends with `\n`). Dropping
    // exactly that last piece keeps only the terminated lines and discards the
    // torn/empty tail uniformly.
    let mut pieces: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    pieces.pop(); // drop the trailing unterminated segment (empty or partial)
    pieces.into_iter()
}

/// Parse `line` into a `serde_json::Value` and enforce the `v == 1` marker,
/// returning the parsed `Value` on success. `None` (⇒ count corrupt) on:
/// non-UTF-8, non-JSON, or missing/non-1 `v`. Peeking `v` on the raw `Value`
/// first rejects a v!=1 row BEFORE we try to deserialize into a versioned shape.
fn parse_versioned_value(line: &[u8]) -> Option<Value> {
    let value: Value = serde_json::from_slice(line).ok()?;
    match value.get("v").and_then(Value::as_u64) {
        Some(1) => Some(value),
        _ => None, // missing, non-integer, or unknown version → refuse
    }
}

/// Parse one terminated line into `T`, enforcing the `v == 1` marker. Returns
/// `None` (⇒ count corrupt) on non-UTF-8, non-JSON, missing/non-1 `v`, or
/// shape-mismatch against `T`.
fn parse_row<T: DeserializeOwned>(line: &[u8]) -> Option<T> {
    let value = parse_versioned_value(line)?;
    serde_json::from_value(value).ok()
}

fn parse_jsonl<T: DeserializeOwned>(bytes: &[u8]) -> ReadResult<T> {
    let mut records = Vec::new();
    let mut corrupt_interior = 0u64;
    for line in terminated_lines(bytes) {
        match parse_row::<T>(line) {
            Some(rec) => records.push(rec),
            None => corrupt_interior += 1,
        }
    }
    ReadResult {
        records,
        corrupt_interior,
    }
}

/// The per-event-type `reason` invariant (R8a): REQUIRED on `delivery-failed`,
/// FORBIDDEN on every other type. A row that violates it is corrupt.
fn reason_invariant_ok(ev: &DispositionEvent) -> bool {
    match ev.event {
        EventKind::DeliveryFailed => ev.reason.is_some(),
        _ => ev.reason.is_none(),
    }
}

/// Parse a `log.jsonl` byte buffer into [`Envelope`] rows (torn-tail tolerant,
/// `v == 1` enforced). Empty input → empty records, `corrupt_interior == 0`.
pub fn parse_log(bytes: &[u8]) -> ReadResult<Envelope> {
    parse_jsonl(bytes)
}

/// Parse a `dispositions.jsonl` byte buffer into [`DispositionEvent`] rows
/// (torn-tail tolerant, `v == 1` enforced, PLUS the per-event-type `reason`
/// invariant). Empty input → empty records.
///
/// A well-typed `DispositionEvent` that violates the `reason` invariant
/// (`delivery-failed` sans reason, or any other type carrying a reason) is
/// counted corrupt and NOT returned — the schema-per-event-type check that a
/// shared enum could not express.
pub fn parse_dispositions(bytes: &[u8]) -> ReadResult<DispositionEvent> {
    let mut records = Vec::new();
    let mut corrupt_interior = 0u64;
    for line in terminated_lines(bytes) {
        match parse_row::<DispositionEvent>(line) {
            Some(ev) if reason_invariant_ok(&ev) => records.push(ev),
            // Parsed to a DispositionEvent but broke the reason invariant, OR
            // failed to parse / wrong version → corrupt interior.
            _ => corrupt_interior += 1,
        }
    }
    ReadResult {
        records,
        corrupt_interior,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_line(id: &str) -> String {
        Envelope {
            v: 1,
            correlation_id: id.to_string(),
            authored_at: 1,
            expires_at: 2,
            target: "t".to_string(),
            origin: "o".to_string(),
            body: "b".to_string(),
        }
        .to_jsonl_line()
    }

    #[test]
    fn empty_input_is_empty() {
        let r = parse_log(b"");
        assert!(r.records.is_empty());
        assert_eq!(r.corrupt_interior, 0);
    }

    #[test]
    fn clean_terminated_lines_all_parse() {
        let buf = format!("{}\n{}\n", env_line("a"), env_line("b"));
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.corrupt_interior, 0);
        assert_eq!(r.records[0].correlation_id, "a");
        assert_eq!(r.records[1].correlation_id, "b");
    }

    #[test]
    fn torn_tail_is_ignored_not_corrupt() {
        // Two full lines + a truncated third (no trailing \n) → 2 records, 0 corrupt.
        let buf = format!("{}\n{}\n{{\"v\":1,\"correlation_i", env_line("a"), env_line("b"));
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 2, "torn tail dropped");
        assert_eq!(r.corrupt_interior, 0, "torn tail is NOT corruption");
    }

    #[test]
    fn interior_garbage_counts_corrupt_others_survive() {
        let buf = format!("{}\nnot json at all\n{}\n", env_line("a"), env_line("b"));
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 2, "the two valid rows survive");
        assert_eq!(r.corrupt_interior, 1, "the garbage interior line is corrupt");
    }

    #[test]
    fn unknown_version_row_is_corrupt() {
        // A v:2 row is refused (counted corrupt), never guessed.
        let v2 = r#"{"v":2,"correlation_id":"x","authored_at":1,"expires_at":2,"target":"t","origin":"o","body":"b"}"#;
        let buf = format!("{}\n{}\n{}\n", env_line("a"), v2, env_line("b"));
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.corrupt_interior, 1, "v:2 row rejected");
    }

    #[test]
    fn missing_version_is_corrupt() {
        let no_v = r#"{"correlation_id":"x","authored_at":1,"expires_at":2,"target":"t","origin":"o","body":"b"}"#;
        let buf = format!("{}\n{}\n", no_v, env_line("a"));
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.corrupt_interior, 1);
    }

    #[test]
    fn blank_interior_line_is_corrupt() {
        // A stray empty interior line (\n\n) is not a record.
        let buf = format!("{}\n\n{}\n", env_line("a"), env_line("b"));
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.corrupt_interior, 1);
    }

    #[test]
    fn shape_mismatch_is_corrupt() {
        // Valid JSON, v==1, but wrong field types for an Envelope → corrupt.
        let bad = r#"{"v":1,"correlation_id":123,"authored_at":"nope"}"#;
        let buf = format!("{}\n{}\n", bad, env_line("a"));
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.corrupt_interior, 1);
    }

    // ------- dispositions: all 5 event types parse, version enforced -------

    #[test]
    fn dispositions_parse_all_event_types_and_enforce_version() {
        let buf = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            DispositionEvent::accepted("a".into(), 1, "h".into(), "o".into(), 0).to_jsonl_line(),
            DispositionEvent::attempted("a".into(), 2, "h".into(), "o".into(), 0).to_jsonl_line(),
            DispositionEvent::queued("a".into(), 3, "h".into(), "o".into(), 0).to_jsonl_line(),
            DispositionEvent::delivered("a".into(), 4, "h".into(), "o".into(), 0).to_jsonl_line(),
            DispositionEvent::delivery_failed("a".into(), 5, "h".into(), "o".into(), 0, "wake".into())
                .to_jsonl_line(),
            // a v:2 row → refused
            r#"{"v":2,"correlation_id":"y","event":"delivered","witnessed_at":9,"witness":"h","origin":"o","authored_at":0}"#,
        );
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 5, "all five v1 event types parse");
        assert_eq!(r.corrupt_interior, 1, "the v:2 row rejected");
        assert_eq!(r.records[0].event, EventKind::Accepted);
        assert_eq!(r.records[4].event, EventKind::DeliveryFailed);
        assert_eq!(r.records[4].reason.as_deref(), Some("wake"));
    }

    // ------- per-event-type reason invariant on READ (R8a) -------

    #[test]
    fn delivery_failed_without_reason_is_corrupt() {
        // Hand-rolled delivery-failed sans reason (the constructor would never
        // build this) → corrupt, NOT returned.
        let no_reason =
            r#"{"v":1,"correlation_id":"x","event":"delivery-failed","witnessed_at":5,"witness":"h","origin":"o","authored_at":0}"#;
        let good = DispositionEvent::delivered("x".into(), 4, "h".into(), "o".into(), 0).to_jsonl_line();
        let buf = format!("{}\n{}\n", no_reason, good);
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 1, "only the valid delivered row survives");
        assert_eq!(r.corrupt_interior, 1, "delivery-failed sans reason is corrupt");
        assert_eq!(r.records[0].event, EventKind::Delivered);
    }

    #[test]
    fn non_failed_with_reason_is_corrupt() {
        // Each of the four reason-less types carrying a reason → corrupt.
        for kind in ["accepted", "attempted", "queued", "delivered"] {
            let with_reason = format!(
                r#"{{"v":1,"correlation_id":"x","event":"{}","witnessed_at":5,"witness":"h","origin":"o","authored_at":0,"reason":"nope"}}"#,
                kind
            );
            let r = parse_dispositions(format!("{}\n", with_reason).as_bytes());
            assert_eq!(r.records.len(), 0, "{kind} with reason is not returned");
            assert_eq!(r.corrupt_interior, 1, "{kind} with reason is corrupt");
        }
    }

    #[test]
    fn dispositions_torn_tail_tolerated() {
        let good = DispositionEvent::delivered("a".into(), 4, "h".into(), "o".into(), 0).to_jsonl_line();
        let buf = format!("{}\n{{\"v\":1,\"correlation_i", good);
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.corrupt_interior, 0, "torn tail is not corruption");
    }

    #[test]
    fn event_row_missing_origin_is_corrupt() {
        // R11: `origin` is a REQUIRED field on every event row. A row missing it
        // fails serde deserialization → corrupt, NOT returned.
        let no_origin =
            r#"{"v":1,"correlation_id":"x","event":"delivered","witnessed_at":5,"witness":"h","authored_at":0}"#;
        let good = DispositionEvent::delivered("x".into(), 4, "h".into(), "o".into(), 0).to_jsonl_line();
        let buf = format!("{}\n{}\n", no_origin, good);
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 1, "only the origin-carrying row survives");
        assert_eq!(r.corrupt_interior, 1, "an event row missing `origin` is corrupt");
        assert_eq!(r.records[0].origin, "o");
    }
}
