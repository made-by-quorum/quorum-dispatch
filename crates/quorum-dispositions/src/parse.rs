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
//!   reader never guesses an unknown version.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::record::{Disposition, Envelope};

/// The outcome of reading a JSONL file: the records that parsed, plus a count of
/// unparseable/version-rejected INTERIOR lines. A torn tail is NOT counted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadResult<T> {
    /// The successfully parsed rows, in file order.
    pub records: Vec<T>,
    /// Count of interior lines that were unparseable OR carried an unknown `v`.
    /// The torn tail (final unterminated line) is deliberately excluded.
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

/// Parse one terminated line into `T`, enforcing the `v == 1` marker. Returns
/// `None` (⇒ count corrupt) on: non-UTF-8, non-JSON, missing/non-1 `v`, or
/// shape-mismatch against `T`.
fn parse_row<T: DeserializeOwned>(line: &[u8]) -> Option<T> {
    // A blank interior line (two consecutive `\n`) is not a record → corrupt.
    // First peek `v` as a raw Value so a v!=1 row is rejected BEFORE we try to
    // deserialize into T (whose shape may differ across versions — never guess).
    let value: Value = serde_json::from_slice(line).ok()?;
    match value.get("v").and_then(Value::as_u64) {
        Some(1) => {}
        _ => return None, // missing, non-integer, or unknown version → refuse
    }
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

/// Parse a `log.jsonl` byte buffer into [`Envelope`] rows (torn-tail tolerant,
/// `v == 1` enforced). Empty input → empty records, `corrupt_interior == 0`.
pub fn parse_log(bytes: &[u8]) -> ReadResult<Envelope> {
    parse_jsonl(bytes)
}

/// Parse a `dispositions.jsonl` byte buffer into [`Disposition`] rows (torn-tail
/// tolerant, `v == 1` enforced). Empty input → empty records.
pub fn parse_dispositions(bytes: &[u8]) -> ReadResult<Disposition> {
    parse_jsonl(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::StoredState;

    fn env_line(id: &str) -> String {
        Envelope {
            v: 1,
            correlation_id: id.to_string(),
            authored_at: 1,
            expires_at: 2,
            target: "t".to_string(),
            authority: "a".to_string(),
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
        let v2 = r#"{"v":2,"correlation_id":"x","authored_at":1,"expires_at":2,"target":"t","authority":"a","body":"b"}"#;
        let buf = format!("{}\n{}\n{}\n", env_line("a"), v2, env_line("b"));
        let r = parse_log(buf.as_bytes());
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.corrupt_interior, 1, "v:2 row rejected");
    }

    #[test]
    fn missing_version_is_corrupt() {
        let no_v = r#"{"correlation_id":"x","authored_at":1,"expires_at":2,"target":"t","authority":"a","body":"b"}"#;
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
    fn dispositions_parse_and_enforce_version() {
        let d = Disposition {
            v: 1,
            correlation_id: "d".to_string(),
            state: StoredState::Failed,
            authored_at: 1,
            witnessed_at: 2,
            authority: "a".to_string(),
            reason: Some("wake".to_string()),
        };
        let v2 = r#"{"v":2,"correlation_id":"y","state":"delivered","authored_at":1,"witnessed_at":2,"authority":"a"}"#;
        let buf = format!("{}\n{}\n", d.to_jsonl_line(), v2);
        let r = parse_dispositions(buf.as_bytes());
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].reason.as_deref(), Some("wake"));
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
}
