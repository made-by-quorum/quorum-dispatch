//! ADD-20 (ack3-spec §6.1) — API-key-shaped redaction for the engine
//! `send-initiated` `content_preview`.
//!
//! [`redact_for_preview`] takes the raw sent text and returns a privacy-scrubbed,
//! length-capped preview suitable for the events file. It is content_preview's
//! ONLY producer (events are otherwise sha+len). The redaction trades a small
//! false-positive rate (uuids / session ids get redacted) for the safe direction:
//! UNDER-redaction (a real secret surviving) is the failure class, over-redaction
//! is fine for a debug preview.
//!
//! Two redaction lanes, applied in ONE left-to-right pass:
//!   1. KNOWN PREFIXES (`sk-`/`sk-ant-`, the GitHub token families, the Slack
//!      families, AWS `AKIA`/`ASIA`, JWT `eyJ`) → the whole token is replaced
//!      with `[REDACTED:<prefix>…]`. The prefix rule fires at ANY body length, so
//!      a short off-charset canary like `sk-abc123` is caught by the prefix ALONE
//!      (independent of the run belt; red-team R5).
//!   2. GENERIC RUN BELT: any unbroken run of `[A-Za-z0-9_-]` of length ≥ 24 →
//!      `[REDACTED:run]`.
//!
//! ORDER is load-bearing (§6.1): REDACT FIRST, THEN truncate to `cap_bytes` on a
//! char boundary (+ `…[truncated]`). Truncating first could split a key and
//! defeat the prefix rule.
//!
//! Hand-rolled scanner (no regex): `regex` is NOT a direct workspace dependency
//! (only a transitive lockfile entry), and the prefix/run rules are trivially
//! hand-rollable — house no-new-deps posture (§6.1).

/// The generic-run-belt threshold: an unbroken `[A-Za-z0-9_-]` run of at least
/// this many chars is redacted to `[REDACTED:run]` (§6.1). The run belt fires at
/// 24, never at 23 (unit-rowed).
const RUN_MIN: usize = 24;

/// The truncation marker appended when the redacted preview is cut to `cap_bytes`.
const TRUNCATED_MARKER: &str = "…[truncated]";

/// Known key prefixes, each with the LABEL emitted in `[REDACTED:<label>…]`.
/// Listed LONGEST-FIRST per shared stem so `sk-ant-` wins over `sk-` and
/// `github_pat_` is distinct from the `gh*_` family. The body charset that the
/// token continues over is [`is_token_char`] for every family EXCEPT AWS (which
/// continues over uppercase-alnum only; see [`scan_aws`]) and JWT (which spans
/// `.`-separated base64url segments; see [`scan_jwt`]).
const PREFIXES: &[&str] = &[
    "sk-ant-",
    "sk-",
    "github_pat_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "xoxb-",
    "xoxp-",
];

/// A token char for the generic belt + the prefix-token continuation (§6.1).
fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Is `b` a JWT segment char (base64url, no padding)? JWT bodies span `.`
/// separators between such segments.
fn is_jwt_seg_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Redact + cap. The ADD-20 entry point: `text` is the raw sent message, the
/// result is the `content_preview` payload (≤ `cap_bytes` on a char boundary,
/// with a truncation marker when cut). REDACT FIRST, then truncate (§6.1).
pub fn redact_for_preview(text: &str, cap_bytes: usize) -> String {
    let redacted = redact(text);
    truncate_on_boundary(&redacted, cap_bytes)
}

/// The redaction pass (no capping) — separated so the order (redact-then-cap) is
/// explicit and unit-testable.
fn redact(text: &str) -> String {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        let b = bytes[i];
        if is_token_char(b) {
            // A token RUN begins here (we only ever land at a run start: after a
            // non-token char or at position 0, since a matched run/token always
            // consumes to the run's end). Try the prefix lane first, then the
            // run belt; either way we advance past the whole run.
            let run_end = run_end_from(bytes, i);
            if let Some((label, token_end)) = match_prefix_token(bytes, i, run_end) {
                out.push_str(&format!("[REDACTED:{label}…]"));
                i = token_end;
                continue;
            }
            // No prefix → the generic run belt.
            let run_len = run_end - i;
            if run_len >= RUN_MIN {
                out.push_str("[REDACTED:run]");
            } else {
                // Safe: the run is pure ASCII token chars.
                out.push_str(std::str::from_utf8(&bytes[i..run_end]).unwrap_or(""));
            }
            i = run_end;
        } else {
            // Non-token byte: copy the whole UTF-8 char verbatim (it may be multi-
            // byte). Find the char's byte length via the next char boundary.
            let char_end = next_char_boundary(text, i);
            out.push_str(&text[i..char_end]);
            i = char_end;
        }
    }
    out
}

/// The end (exclusive) of the maximal `[A-Za-z0-9_-]` run starting at `start`.
fn run_end_from(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len() && is_token_char(bytes[j]) {
        j += 1;
    }
    j
}

/// If the token run starting at `start` (ending at `run_end`) is a known key
/// shape, return `(label, token_end)` — the label for `[REDACTED:<label>…]` and
/// the byte offset PAST the whole key token (which, for JWT, may extend beyond
/// `run_end` across `.` separators). Fires at ANY body length for the literal
/// prefixes (§6.1 / R5); AWS + JWT carry their own shape guards.
fn match_prefix_token(bytes: &[u8], start: usize, run_end: usize) -> Option<(String, usize)> {
    let rest = &bytes[start..];
    // JWT: `eyJ` + base64url segments joined by '.'. Spans beyond the first run.
    if rest.starts_with(b"eyJ") {
        return Some(("eyJ".to_string(), scan_jwt(bytes, start)));
    }
    // AWS: `AKIA`/`ASIA` MUST be followed by uppercase alnum (else it's a plain
    // word like "AKIApologies"); the token runs over uppercase-alnum only.
    if rest.starts_with(b"AKIA") || rest.starts_with(b"ASIA") {
        if let Some(end) = scan_aws(bytes, start) {
            let label = std::str::from_utf8(&bytes[start..start + 4]).unwrap_or("AWS");
            return Some((label.to_string(), end));
        }
        // No uppercase-alnum body → not an AWS key; fall through to the belt.
        return None;
    }
    // Literal prefixes (sk-/gh*_/xox*-/github_pat_): match at the run start, then
    // consume the WHOLE token run (run_end). Fires at any body length.
    for p in PREFIXES {
        if rest.starts_with(p.as_bytes()) {
            return Some(((*p).to_string(), run_end));
        }
    }
    None
}

/// AWS key scan: `AKIA`/`ASIA` (4 chars) + ≥1 uppercase-alnum. Returns the byte
/// offset past the uppercase-alnum body, or `None` if no uppercase-alnum follows.
fn scan_aws(bytes: &[u8], start: usize) -> Option<usize> {
    let body_start = start + 4;
    let mut j = body_start;
    while j < bytes.len() && (bytes[j].is_ascii_uppercase() || bytes[j].is_ascii_digit()) {
        j += 1;
    }
    if j > body_start {
        Some(j)
    } else {
        None
    }
}

/// JWT scan: `eyJ` then base64url segments separated by `.`. Consumes the maximal
/// `[A-Za-z0-9_-](\.[A-Za-z0-9_-]+)*` shape from `start`. A trailing `.` is NOT
/// consumed (it must be followed by another segment char).
fn scan_jwt(bytes: &[u8], start: usize) -> usize {
    let n = bytes.len();
    let mut j = start;
    // First segment (includes the `eyJ` prefix, all base64url chars).
    while j < n && is_jwt_seg_char(bytes[j]) {
        j += 1;
    }
    // Subsequent `.segment` runs.
    loop {
        if j < n && bytes[j] == b'.' && j + 1 < n && is_jwt_seg_char(bytes[j + 1]) {
            j += 1; // the '.'
            while j < n && is_jwt_seg_char(bytes[j]) {
                j += 1;
            }
        } else {
            break;
        }
    }
    j
}

/// The byte offset of the next char boundary strictly after `i` (i is assumed a
/// boundary). Used to copy a non-token UTF-8 char whole.
fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

/// Truncate `s` to at most `cap_bytes` on a char boundary; if cut, append
/// [`TRUNCATED_MARKER`]. The marker is ADDED past the cap (the cap bounds the
/// CONTENT body, not the marker) — the events §4.2 shrink belt accounts for the
/// field's full serialized width separately.
fn truncate_on_boundary(s: &str, cap_bytes: usize) -> String {
    if s.len() <= cap_bytes {
        return s.to_string();
    }
    // Walk back from cap_bytes to the nearest char boundary.
    let mut cut = cap_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{TRUNCATED_MARKER}", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Prefix classes: each known family is redacted (§6.1 / leak-control). ----

    #[test]
    fn redacts_each_known_prefix_class() {
        let cases = [
            ("sk-abc123def456ghi789jkl012", "sk-"),
            ("sk-ant-api03-AAAABBBBCCCCDDDD", "sk-ant-"),
            ("ghp_AAAABBBBCCCCDDDDEEEEFFFF1234", "ghp_"),
            ("gho_AAAABBBBCCCCDDDDEEEE", "gho_"),
            ("ghu_AAAABBBBCCCCDDDDEEEE", "ghu_"),
            ("ghs_AAAABBBBCCCCDDDDEEEE", "ghs_"),
            ("ghr_AAAABBBBCCCCDDDDEEEE", "ghr_"),
            ("github_pat_11ABCDEFG0abcdefghij", "github_pat_"),
            ("xoxb-12345-67890-abcdefABCDEF", "xoxb-"),
            ("xoxp-12345-67890-abcdefABCDEF", "xoxp-"),
            ("AKIAIOSFODNN7EXAMPLE", "AKIA"),
            ("ASIAIOSFODNN7EXAMPLE", "ASIA"),
            ("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.dQw4w9WgXcQ", "eyJ"),
        ];
        for (input, label) in cases {
            let out = redact(input);
            let expect = format!("[REDACTED:{label}…]");
            assert_eq!(out, expect, "input {input:?} should redact to {expect:?}");
            // The raw secret body never survives.
            assert!(
                !out.contains(&input[label.len()..label.len() + 4.min(input.len() - label.len())]),
                "no raw body leaks for {input:?}: {out:?}"
            );
        }
    }

    #[test]
    fn sk_ant_wins_over_sk_longest_prefix_first() {
        // sk-ant- is the more specific family — its label must win.
        assert_eq!(redact("sk-ant-XXXXYYYYZZZZ"), "[REDACTED:sk-ant-…]");
    }

    // -- The sub-24 prefix-keyed canary: redacted by the PREFIX rule ALONE. ------

    #[test]
    fn sub_24_prefix_keyed_canary_redacted_by_prefix_alone() {
        // `sk-abc123` is only 9 chars — far below the run belt's 24. The prefix
        // rule fires at ANY body length (R5: the prefix lane is independent teeth,
        // not a run-rule shadow).
        let input = "sk-abc123";
        assert!(input.len() < RUN_MIN);
        assert_eq!(redact(input), "[REDACTED:sk-…]");
    }

    #[test]
    fn aws_prefix_needs_uppercase_alnum_body_else_plain() {
        // AKIA + uppercase-alnum → redacted.
        assert_eq!(redact("AKIAABCD"), "[REDACTED:AKIA…]");
        // "AKIApologies" — lowercase right after AKIA → NOT an AWS key; the run
        // belt then applies (12 < 24 → plain).
        assert_eq!(redact("AKIApologies"), "AKIApologies");
    }

    // -- The generic run belt: fires at 24, not 23. ------------------------------

    #[test]
    fn run_belt_fires_at_24_not_23() {
        let r23 = "a".repeat(23);
        let r24 = "a".repeat(24);
        assert_eq!(redact(&r23), r23, "a 23-char run is below the belt → plain");
        assert_eq!(redact(&r24), "[REDACTED:run]", "a 24-char run is redacted");
    }

    #[test]
    fn run_belt_counts_token_chars_incl_dash_underscore() {
        // `-` and `_` are token chars; a 24-char mix is a single run → redacted.
        let s = "abc-def_ghi-jkl_mno-pqr0"; // 24 chars
        assert_eq!(s.len(), 24);
        assert_eq!(redact(s), "[REDACTED:run]");
    }

    // -- Mixed text: plain words survive; only the secret is scrubbed. -----------

    #[test]
    fn mixed_text_keeps_plain_words() {
        let input = "please run sk-abc123 then ping me ok";
        // Only the key token is scrubbed; the surrounding prose is verbatim.
        assert_eq!(redact(input), "please run [REDACTED:sk-…] then ping me ok");
    }

    #[test]
    fn short_words_and_punctuation_unchanged() {
        let input = "hello, world! this is fine.";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn prefix_match_only_at_token_start_not_midword() {
        // `task-foo` contains "sk-" mid-word but the run starts at `task` — the
        // prefix lane only fires at a run start, so no spurious redaction.
        let input = "task-foo bar";
        assert_eq!(redact(input), "task-foo bar");
    }

    // -- ORDER: redact FIRST, then truncate (load-bearing, §6.1). ----------------

    #[test]
    fn redact_then_truncate_order_a_split_key_would_leak() {
        // A key positioned so that a TRUNCATE-FIRST order would cut it mid-body
        // (defeating the prefix rule) — under the correct order the whole key is
        // redacted FIRST, so the cap never sees raw key bytes.
        let key = format!("sk-{}", "Z".repeat(60)); // 63-byte key
        let input = format!("{key} trailing words here that push past the cap aaaaaaa");
        // Cap small enough that truncate-first would land inside the key body.
        let out = redact_for_preview(&input, 20);
        // The raw key body is GONE (redacted first). No run of Z survives.
        assert!(
            !out.contains("ZZZ"),
            "redact-first scrubbed the key: {out:?}"
        );
        assert!(out.starts_with("[REDACTED:sk-…]"));
    }

    #[test]
    fn truncate_appends_marker_only_when_cut() {
        let short = "tiny";
        assert_eq!(redact_for_preview(short, 256), "tiny");
        let long = "x".repeat(300); // 300 'x' → a 300-char run → redacted to a short token, no cut
        assert_eq!(redact_for_preview(&long, 256), "[REDACTED:run]");
        // A long body of plain SHORT words exceeds the cap → cut + marker.
        let words = "ab ".repeat(200); // 600 bytes of short words, none redacted
        let out = redact_for_preview(&words, 256);
        assert!(
            out.ends_with(TRUNCATED_MARKER),
            "cut output carries the marker"
        );
        // The body before the marker is ≤ cap and on a char boundary.
        let body = out.strip_suffix(TRUNCATED_MARKER).unwrap();
        assert!(body.len() <= 256);
    }

    #[test]
    fn truncate_respects_char_boundary() {
        // A multi-byte char straddling the cap is not split.
        let input = "é".repeat(200); // each 'é' is 2 bytes → 400 bytes; none redacted
        let out = redact_for_preview(&input, 51); // odd cap lands mid-char
        let body = out.strip_suffix(TRUNCATED_MARKER).unwrap_or(&out);
        assert!(body.is_char_boundary(body.len()));
        // Round-trips as valid UTF-8 (no split char).
        assert!(std::str::from_utf8(body.as_bytes()).is_ok());
    }
}
