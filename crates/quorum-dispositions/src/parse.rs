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
//! - **discriminated-union validation** (dispositions only, R14.5): the event
//!   variant's tail is enforced by the type's own `Deserialize` — a
//!   `delivery-failed` OR `refused` row WITHOUT `class`, a plain event WITH a
//!   `class`, or ANY event carrying a field foreign to it (e.g. the reserved-
//!   but-unused `reason`) fails deserialization and is counted corrupt. This
//!   replaces the pre-R14 runtime forbidden-field check: the type system now
//!   enforces the per-variant shape.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::record::{DispositionEvent, Envelope};

/// The outcome of reading a JSONL file: the records that parsed, plus a count of
/// unparseable/version-rejected INTERIOR lines. A torn tail is NOT counted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadResult<T> {
    /// The successfully parsed rows, in file order.
    pub records: Vec<T>,
    /// Count of interior lines that were unparseable OR carried an unknown `v`
    /// OR violated the discriminated-union shape. The torn tail (final
    /// unterminated line) is deliberately excluded.
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
/// shape-mismatch against `T` (for a [`DispositionEvent`] that includes the
/// discriminated-union invariants, enforced by its own `Deserialize`).
///
/// The `v == 1` marker is peeked via a `serde_json::Value` probe
/// ([`parse_versioned_value`]) — cheap and version-only. But the FINAL typed
/// deserialize consumes the RAW LINE BYTES (`from_slice`), NOT the intermediate
/// `Value` (audit follow-up #3b). This is load-bearing: a `serde_json::Value`
/// COLLAPSES a duplicate object key (last-wins) DURING its own parse, so a
/// `from_value` deserialize would receive an already-deduped map — the type's
/// `Deserialize` (the `MapAccess` `set_once` dup-key guard in
/// [`crate::record`]) would NEVER see the duplicate and could not reject it. A
/// row with a duplicated key is CORRUPT, never silently deduped; deserializing
/// from the raw bytes runs `set_once` over the true token stream so the guard is
/// live on this deployed file-read path (not only on the direct `from_str`
/// unit-test path).
fn parse_row<T: DeserializeOwned>(line: &[u8]) -> Option<T> {
    // v-peek only (the Value probe may keep its dedup — we discard it here); the
    // TYPED deserialize is from the raw bytes so `set_once` sees duplicate keys.
    let _v = parse_versioned_value(line)?;
    serde_json::from_slice(line).ok()
}

fn parse_jsonl<T: DeserializeOwned>(bytes: &[u8]) -> ReadResult<T> {
    let mut records = Vec::new();
    let mut corrupt_interior = 0u64;
    for line in terminated_lines(bytes) {
        // A BLANK (all-whitespace) interior line is NOT a record and is NOT
        // corruption — it is skipped. This is the reader half of the
        // self-delimiting `\n{line}\n` append framing (dispatch dispositions.rs
        // `append_line`, audit follow-up #3a; telemetry.rs F-DEOBS-1): the LEADING
        // newline of every append leaves a harmless blank line on a clean tail, so
        // a valid file is a run of `record\n\nrecord\n\n…` (or `record\n\n…` after
        // any legacy bare-`{line}\n` rows). Counting those blanks corrupt would
        // both inflate the forensic count and trip the store's per-file
        // corrupt-interior warn on EVERY read. An empty line carries no bytes to
        // lose, so skipping it cannot hide a dropped record; a NON-blank
        // unparseable interior line is STILL corruption (counted below).
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
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

/// Parse a `log.jsonl` byte buffer into [`Envelope`] rows (torn-tail tolerant,
/// `v == 1` enforced). Empty input → empty records, `corrupt_interior == 0`.
pub fn parse_log(bytes: &[u8]) -> ReadResult<Envelope> {
    parse_jsonl(bytes)
}

/// Parse a `dispositions.jsonl` byte buffer into [`DispositionEvent`] rows
/// (torn-tail tolerant, `v == 1` enforced, PLUS the discriminated-union
/// invariants). Empty input → empty records.
///
/// The per-variant shape (R14.5) is enforced by [`DispositionEvent`]'s own
/// `Deserialize`: a `delivery-failed` or `refused` row sans `class`, a plain
/// event carrying a `class`, or any row carrying a foreign field (including the
/// reserved-but-unused `reason`) fails deserialization and is counted corrupt
/// (NOT returned) — the schema-per-event-type check the type system now owns.
pub fn parse_dispositions(bytes: &[u8]) -> ReadResult<DispositionEvent> {
    parse_jsonl(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::EventKind;

    fn env_line(id: &str) -> String {
        Envelope {
            v: 1,
            correlation_id: id.to_string(),
            authored_at: 1,
            expires_at: 2,
            target: "t".to_string(),
            origin: "o".to_string(),
            sender: None,
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
    fn blank_interior_line_is_skipped_not_corrupt() {
        // A stray empty interior line (\n\n) is NOT a record and is NOT corruption
        // — it is skipped (audit follow-up #3a). This is the reader half of the
        // self-delimiting `\n{line}\n` append framing: every append leaves a
        // leading blank line on a clean tail, so a valid file is a run of
        // `record\n\nrecord\n\n…`. Counting those blanks corrupt would inflate the
        // forensic count and trip the store's per-file corrupt-interior warn on
        // every read. (Pre-#3a this asserted `corrupt_interior == 1`; the
        // self-delimiting write flips a blank line from corrupt→skipped.)
        let buf = format!("{}\n\n{}\n", env_line("a"), env_line("b"));
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 2, "both real records parse across the blank");
        assert_eq!(r.corrupt_interior, 0, "a blank interior line is skipped, not corrupt");
    }

    #[test]
    fn self_delimited_framing_round_trips_with_leading_blank_lines() {
        // The exact on-disk shape the self-delimiting `\n{line}\n` writer produces
        // (dispatch dispositions.rs `append_line`, audit follow-up #3a): a leading
        // blank line before EVERY record, i.e. `\nA\n\nB\n\nC\n`. Every record must
        // parse and NONE of the interspersed blank lines counts corrupt — this is
        // the reader-side guarantee the writer relies on.
        let buf = format!(
            "\n{}\n\n{}\n\n{}\n",
            env_line("a"),
            env_line("b"),
            env_line("c")
        );
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 3, "all three records survive the leading blanks");
        assert_eq!(r.corrupt_interior, 0, "no blank line miscounted as corrupt");
        let ids: Vec<&str> = r.records.iter().map(|e| e.correlation_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"], "order preserved across the framing");
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
            DispositionEvent::attempted("a".into(), 2).to_jsonl_line(),
            DispositionEvent::queued("a".into(), 3).to_jsonl_line(),
            DispositionEvent::delivered("a".into(), 4, "d".into()).to_jsonl_line(),
            DispositionEvent::delivery_failed("a".into(), 5, "wake".into()).to_jsonl_line(),
            DispositionEvent::refused("a".into(), 6, "ambiguous".into()).to_jsonl_line(),
            // a v:2 row → refused
            r#"{"v":2,"correlation_id":"y","event":"delivered","created_at":9}"#,
        );
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 5, "all five v1 event types parse");
        assert_eq!(r.corrupt_interior, 1, "the v:2 row rejected");
        assert_eq!(r.records[0].kind(), EventKind::Attempted);
        assert_eq!(r.records[3].kind(), EventKind::DeliveryFailed);
        assert!(matches!(
            r.records[3],
            DispositionEvent::DeliveryFailed { ref class, .. } if class == "wake"
        ));
        assert_eq!(r.records[4].kind(), EventKind::Refused);
        assert!(matches!(
            r.records[4],
            DispositionEvent::Refused { ref class, .. } if class == "ambiguous"
        ));
    }

    // ------- discriminated-union validation on READ (R14.5) -------

    #[test]
    fn delivery_failed_without_class_is_corrupt() {
        // Hand-rolled delivery-failed sans class (the constructor would never
        // build this) → corrupt, NOT returned.
        let no_class =
            r#"{"v":1,"correlation_id":"x","event":"delivery-failed","created_at":5}"#;
        let good = DispositionEvent::delivered("x".into(), 4, "d".into()).to_jsonl_line();
        let buf = format!("{}\n{}\n", no_class, good);
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 1, "only the valid delivered row survives");
        assert_eq!(r.corrupt_interior, 1, "delivery-failed sans class is corrupt");
        assert_eq!(r.records[0].kind(), EventKind::Delivered);
    }

    #[test]
    fn refused_without_class_is_corrupt() {
        let no_class = r#"{"v":1,"correlation_id":"x","event":"refused","created_at":5}"#;
        let good = DispositionEvent::delivered("x".into(), 4, "d".into()).to_jsonl_line();
        let buf = format!("{}\n{}\n", no_class, good);
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 1, "only the valid delivered row survives");
        assert_eq!(r.corrupt_interior, 1, "refused sans class is corrupt");
    }

    #[test]
    fn plain_event_with_class_is_corrupt() {
        // Each plain type carrying a `class` (a foreign field) → corrupt.
        for kind in ["attempted", "queued", "delivered"] {
            let with_class = format!(
                r#"{{"v":1,"correlation_id":"x","event":"{}","created_at":5,"class":"nope"}}"#,
                kind
            );
            let r = parse_dispositions(format!("{}\n", with_class).as_bytes());
            assert_eq!(r.records.len(), 0, "{kind} with class is not returned");
            assert_eq!(r.corrupt_interior, 1, "{kind} with class is corrupt");
        }
    }

    #[test]
    fn any_variant_with_foreign_reason_is_corrupt() {
        // `reason` is RESERVED but UNUSED in v1 → an unknown field on ANY variant
        // → corrupt (the discriminated union rejects foreign fields).
        for (kind, tail) in [
            ("attempted", ""),
            ("delivered", ""),
            ("delivery-failed", r#","class":"wake""#),
            ("refused", r#","class":"ambiguous""#),
        ] {
            let line = format!(
                r#"{{"v":1,"correlation_id":"x","event":"{}","created_at":5,"reason":"nope"{}}}"#,
                kind, tail
            );
            let r = parse_dispositions(format!("{}\n", line).as_bytes());
            assert_eq!(r.records.len(), 0, "{kind} carrying `reason` is not returned");
            assert_eq!(r.corrupt_interior, 1, "{kind} carrying `reason` is corrupt");
        }
    }

    #[test]
    fn dispositions_torn_tail_tolerated() {
        let good = DispositionEvent::delivered("a".into(), 4, "d".into()).to_jsonl_line();
        let buf = format!("{}\n{{\"v\":1,\"correlation_i", good);
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.corrupt_interior, 0, "torn tail is not corruption");
    }

    #[test]
    fn event_row_missing_created_at_is_corrupt() {
        // `created_at` is a REQUIRED common field on every event row. A row
        // missing it fails deserialization → corrupt, NOT returned.
        let no_created =
            r#"{"v":1,"correlation_id":"x","event":"delivered"}"#;
        let good = DispositionEvent::delivered("x".into(), 4, "d".into()).to_jsonl_line();
        let buf = format!("{}\n{}\n", no_created, good);
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 1, "only the created_at-carrying row survives");
        assert_eq!(r.corrupt_interior, 1, "an event row missing `created_at` is corrupt");
    }

    // ------- audit follow-up #3b: dup-key guard on the DEPLOYED read path -------
    //
    // These drive the ACTUAL file-read entry point (`parse_dispositions` /
    // `parse_log` → `parse_row`), NOT a direct `from_str::<T>`. Before the fix,
    // `parse_row` deserialized from an intermediate `serde_json::Value`, which
    // COLLAPSES a duplicate object key (last-wins) before the type's `set_once`
    // dup-key guard ever runs — so a duplicated field was silently accepted on the
    // real read path (the guard was DEAD here; only the direct-`from_str` unit
    // tests exercised it). The fix makes the typed deserialize consume the RAW LINE
    // BYTES (`from_slice`), so `set_once` sees the duplicate token and rejects the
    // row as corrupt.
    //
    // MUTATION PROOF: revert `parse_row` to `serde_json::from_value(value)` and
    // every "records=0 / corrupt_interior=1" assertion below FLIPS to "records=1"
    // (the last-wins dedup makes the row parse) — these tests go RED. That is
    // exactly the dead-guard regression this fix closes.

    #[test]
    fn duplicate_event_key_is_corrupt_on_the_read_path() {
        // (a) A duplicated `event` key. Pre-fix: the Value collapses it to the
        // last value ("queued") and the row parses as records=1, kind=Queued.
        // Post-fix: `set_once` rejects the duplicate → records=0, corrupt=1.
        let dup = r#"{"v":1,"correlation_id":"a","event":"attempted","event":"queued","created_at":5}"#;
        let r = parse_dispositions(format!("{dup}\n").as_bytes());
        assert_eq!(r.records.len(), 0, "a duplicated `event` key is rejected (not last-wins deduped)");
        assert_eq!(r.corrupt_interior, 1, "the dup-key row is counted corrupt on the read path");
    }

    #[test]
    fn duplicate_body_digest_key_is_corrupt_on_the_read_path() {
        // (b) R21 explicitly requires a duplicated `body_digest` case. A delivered
        // row with two `body_digest` values must be corrupt — a tampered/torn row
        // must never silently resolve to the last digest (the R15 integrity
        // binding). Pre-fix the Value keeps only the last, and the row parses.
        let dup = r#"{"v":1,"correlation_id":"a","event":"delivered","created_at":5,"body_digest":"aaaa","body_digest":"bbbb"}"#;
        let r = parse_dispositions(format!("{dup}\n").as_bytes());
        assert_eq!(r.records.len(), 0, "a duplicated `body_digest` key is rejected");
        assert_eq!(r.corrupt_interior, 1, "the dup-body_digest row is corrupt on the read path");
    }

    #[test]
    fn duplicate_correlation_id_key_is_corrupt_on_the_read_path() {
        // A duplicated join key is equally corrupt (belt-and-suspenders over (a)/(b)).
        let dup = r#"{"v":1,"correlation_id":"a","correlation_id":"b","event":"attempted","created_at":5}"#;
        let r = parse_dispositions(format!("{dup}\n").as_bytes());
        assert_eq!(r.records.len(), 0, "a duplicated `correlation_id` key is rejected");
        assert_eq!(r.corrupt_interior, 1);
    }

    #[test]
    fn single_key_control_still_parses_on_the_read_path() {
        // (c) CONTROL: a valid single-key line still parses unchanged — the fix
        // rejects ONLY duplicate keys, never a well-formed row.
        let good = DispositionEvent::delivered("a".into(), 5, "digest".into()).to_jsonl_line();
        let r = parse_dispositions(format!("{good}\n").as_bytes());
        assert_eq!(r.records.len(), 1, "a valid single-key row still parses");
        assert_eq!(r.corrupt_interior, 0);
        assert_eq!(r.records[0].kind(), EventKind::Delivered);
        assert_eq!(r.records[0].body_digest(), Some("digest"));
    }

    #[test]
    fn duplicate_envelope_key_is_corrupt_on_the_log_read_path() {
        // The SINGLE fix covers `parse_log` (Envelope) too — both route through
        // `parse_row`. A duplicated `correlation_id` on an envelope row is corrupt
        // on the deployed read path (serde's derived struct Deserialize rejects the
        // dup once it runs over raw bytes; pre-fix the Value dedup hid it).
        let dup = r#"{"v":1,"correlation_id":"a","correlation_id":"b","authored_at":1,"expires_at":2,"target":"t","origin":"o","body":"x"}"#;
        let r = parse_log(format!("{dup}\n").as_bytes());
        assert_eq!(r.records.len(), 0, "a duplicated envelope key is rejected on the read path");
        assert_eq!(r.corrupt_interior, 1);
    }

    #[test]
    fn version_gate_and_valid_rows_unchanged_by_the_dup_fix() {
        // Confirm the version gate still refuses `v != 1`, and valid rows parse
        // unchanged (the peek stays; only the typed source changed to raw bytes).
        let v2 = r#"{"v":2,"correlation_id":"y","event":"delivered","created_at":9,"body_digest":"d"}"#;
        let good = DispositionEvent::attempted("z".into(), 7).to_jsonl_line();
        let r = parse_dispositions(format!("{v2}\n{good}\n").as_bytes());
        assert_eq!(r.records.len(), 1, "the valid v1 row parses unchanged");
        assert_eq!(r.corrupt_interior, 1, "the v!=1 row is still refused by the version gate");
        assert_eq!(r.records[0].kind(), EventKind::Attempted);
    }
}
