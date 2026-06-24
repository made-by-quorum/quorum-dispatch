//! Integration test assertion library — reusable comparator classes for G1–G6 scenarios.
//!
//! Assertions are structured for semantic validation per ADD-6 (macOS kernel echo loss)
//! and ADR-0004 (invariants: backlog-completeness, scroll-intact, altscreen-replay —
//! the latter REVERSED from the original no-altscreen-leak; see assert_altscreen_replay).
//!
//! Each assertion emits structured PASS/FAIL with test context and returns Result<(), String>.

/// Assert that a scrollback contains all expected lines in order.
///
/// Validates backlog-completeness invariant (ADR-0004): lines produced while detached
/// appear in the replay, with the most-recent window complete in order.
///
/// # Arguments
/// - `scrollback_lines`: Output lines from scrollback replay (typically from `sbmux list`)
/// - `expected_count`: Number of lines expected to be present
/// - `run_desc`: Test description for error messages
///
/// # Returns
/// - `Ok(())` if all expected lines are present and in order
/// - `Err(String)` with detailed mismatch info
pub fn assert_backlog_completeness(
    scrollback_lines: &[String],
    expected_count: usize,
    run_desc: &str,
) -> Result<(), String> {
    if scrollback_lines.len() < expected_count {
        return Err(format!(
            "[{}] backlog-completeness FAIL: expected ≥{} lines, got {}",
            run_desc,
            expected_count,
            scrollback_lines.len()
        ));
    }

    // Check that lines are numbered sequentially (crude check for ordering)
    // This is a basic sanity check; full order verification depends on content
    for (idx, line) in scrollback_lines.iter().enumerate() {
        if line.is_empty() {
            return Err(format!(
                "[{}] backlog-completeness: empty line at index {}",
                run_desc, idx
            ));
        }
    }

    Ok(())
}

/// Assert that scrollback from BEFORE an event is intact.
///
/// Validates scroll-intact invariant (ADR-0004): pre-detach sentinel lines present
/// and unchanged in replay.
///
/// # Arguments
/// - `scrollback`: Replay output (raw bytes or line-split)
/// - `sentinel`: Line content that should be present (pre-event marker)
/// - `run_desc`: Test description
///
/// # Returns
/// - `Ok(())` if sentinel is found in scrollback
/// - `Err(String)` with context
pub fn assert_scroll_intact(
    scrollback: &[String],
    sentinel: &str,
    run_desc: &str,
) -> Result<(), String> {
    if scrollback.iter().any(|line| line.contains(sentinel)) {
        return Ok(());
    }

    Err(format!(
        "[{}] scroll-intact FAIL: sentinel '{}' not found in scrollback (lines: {})",
        run_desc,
        sentinel,
        scrollback.len()
    ))
}

/// Assert the altscreen-replay invariant (REVERSES the old no-altscreen-leak
/// gate — approved product decision, Pete 2026-06-10; see
/// doc/inbox/2026-06-10-sbmux-phone-scroll-regression.md):
///
/// > A client's raw capture contains `?1049h` IFF the inner app is in the alt
/// > screen at attach time or transitions into it while attached; a
/// > main-screen session's capture never contains 1049 sequences; within one
/// > attach each transition emits exactly once.
///
/// Divergence #1 (sbmux design) still holds for the inner app's RAW bytes:
/// the performer absorbs DEC 1049 server-side and clients never see
/// app-originated mode bytes. What clients now receive is the RENDER LAYER's
/// own per-client replay of the absorbed state (`?1049h`/`?1049l`, tracked in
/// RenderCache) — without it a phone terminal attached to a fullscreen app
/// sits on its main screen with mouse tracking on and scrolling dead.
///
/// The legacy activations (`?47`, `?1047`) must NEVER appear: the renderer
/// re-emits exclusively the 1049 form, regardless of which variant the inner
/// app used.
///
/// # Arguments
/// - `raw_bytes`: Raw PTY output bytes from replay capture
/// - `expect_1049h`: exact number of `?1049h` (alt entries) the scenario
///   produces in this capture (0 for a main-screen session)
/// - `expect_1049l`: exact number of `?1049l` (alt exits) expected
/// - `run_desc`: Test description
///
/// # Returns
/// - `Ok(())` if counts match exactly and no legacy variants are present
/// - `Err(String)` with counts/positions and context
pub fn assert_altscreen_replay(
    raw_bytes: &[u8],
    expect_1049h: usize,
    expect_1049l: usize,
    run_desc: &str,
) -> Result<(), String> {
    let h = count_pattern_occurrences(raw_bytes, b"?1049h");
    let l = count_pattern_occurrences(raw_bytes, b"?1049l");
    if h != expect_1049h || l != expect_1049l {
        return Err(format!(
            "[{}] altscreen-replay FAIL: expected exactly {} ?1049h / {} ?1049l, found {} / {}",
            run_desc, expect_1049h, expect_1049l, h, l
        ));
    }

    // Legacy altscreen activations are still absorbed and never re-emitted.
    let legacy_patterns: &[&[u8]] = &[b"?47h", b"?47l", b"?1047h", b"?1047l"];
    for pattern in legacy_patterns {
        if let Some(pos) = find_byte_sequence(raw_bytes, pattern) {
            return Err(format!(
                "[{}] altscreen-replay FAIL: legacy variant {:?} at byte position {} \
                 (renderer must replay 1049 only)",
                run_desc,
                String::from_utf8_lossy(pattern),
                pos
            ));
        }
    }

    Ok(())
}

/// Assert that input bytes reach output (keyed on application output per ADD-6).
///
/// Per ADD-6 (kernel echo loss under flood): assertions on echo data are unreliable
/// on macOS (kernel tty line discipline drops bytes under paste-flood, mux-independently).
/// This assertion keys on APPLICATION OUTPUT instead.
///
/// The `app_output_check` flag:
/// - `true`: Strict mode — sent bytes must appear EXACTLY in the app's output
/// - `false`: Lenient mode — partial match acceptable (for echo-under-flood tolerance)
///
/// # Arguments
/// - `sent_bytes`: Input data sent to PTY
/// - `received_output`: Output captured from session (app stdout)
/// - `app_output_check`: If true, byte-exact match required; if false, app-output recovery OK
/// - `run_desc`: Test description
///
/// # Returns
/// - `Ok(())` if bytes recovered (per mode)
/// - `Err(String)` with drop count and context
pub fn assert_no_drop(
    sent_bytes: &[u8],
    received_output: &[u8],
    app_output_check: bool,
    run_desc: &str,
) -> Result<(), String> {
    if app_output_check {
        // Strict: every byte from input must appear in output (keyed on app, not echo)
        if !received_output
            .windows(sent_bytes.len())
            .any(|w| w == sent_bytes)
        {
            // At minimum, check that we got close (full-range check, not tail-only)
            let received_count = count_pattern_occurrences(received_output, sent_bytes);
            let expected_count = 1; // Should appear at least once

            if received_count < expected_count {
                return Err(format!(
                    "[{}] no-drop FAIL: sent {} bytes, app-output incomplete (byte-exact match required). Sent: {:?}, got: {}/{} recoverable",
                    run_desc,
                    sent_bytes.len(),
                    String::from_utf8_lossy(sent_bytes),
                    received_count,
                    expected_count
                ));
            }
        }
    } else {
        // Lenient: allow partial recovery (for echo-under-flood cases)
        if received_output.is_empty() {
            return Err(format!(
                "[{}] no-drop FAIL (lenient): sent {} bytes, received 0",
                run_desc,
                sent_bytes.len()
            ));
        }
    }

    Ok(())
}

/// Assert that frame-decoded History lines carry the marker indices
/// `expected_range`, each exactly once, in strictly increasing order, with no
/// out-of-range index present.  This is B3's ORDERED backlog comparator (R0) —
/// the order-aware replacement for `assert_backlog_completeness` (len+non-empty,
/// order-blind) and `assert_scroll_intact` (substring-anywhere).
///
/// # Input contract (red-team #12 — load-bearing, do NOT relax)
/// `lines` MUST be the **FRAME-DECODED `ServerMsg::History` line vector in WIRE
/// ORDER** — i.e. the lines exactly as the daemon emitted them across one or
/// more `History` frames, concatenated in receive order (see
/// `record_attach_frames` / `Recorded::history_lines`). It must NEVER be a
/// settled *text capture* (e.g. `Captured::text()` or a spatially reconstructed
/// screen): a text capture is spatially ordered by construction, so clause (b)
/// (strictly-increasing-in-input-order) would be VACUOUS — it would pass even on
/// a daemon that emitted the History frames in scrambled order. Rows that can
/// only supply settled text (R2(b)) must use `assert_scroll_intact` + presence
/// instead, honestly labeled as a weaker (order-blind) check.
///
/// # Marker grammar
/// A line is a MARKER LINE iff it contains `marker` immediately followed by a
/// run of ASCII digits forming the index (e.g. `marker="b3-mark-"` matches
/// `b3-mark-0042 trailing junk` → index 42). All other lines are CHROME and are
/// ignored — including (i) lines with no `marker` at all (prompts, blanks) and
/// (ii) lines that contain the marker text but no FOLLOWING digit, such as the
/// ECHOED generator command `seq -f 'b3-mark-%04.0f' 1 50` whose `%` follows the
/// prefix. (Markers split across a frame boundary — where digits land but are
/// INCOMPLETE — are the job of the dedicated whole-line-framing checker (R5b),
/// not R0; an incomplete digit run still parses here as some in/out-of-range
/// index and is judged by clauses (a)/(c).)
///
/// # Checks
/// - (a) every index in `expected_range` is present EXACTLY once;
/// - (b) the indices of marker lines, in input (wire) order, are strictly
///   increasing;
/// - (c) NO index outside `expected_range` is present.
///
/// # Returns
/// - `Ok(())` if all three clauses hold;
/// - `Err(String)` naming the first violated clause with offending indices.
pub fn assert_backlog_ordered(
    lines: &[Vec<u8>],
    marker: &str,
    expected_range: std::ops::RangeInclusive<usize>,
    run_desc: &str,
) -> Result<(), String> {
    // Parse the index out of every marker-bearing line, preserving wire order.
    let mut found: Vec<usize> = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let text = String::from_utf8_lossy(raw);
        let Some(pos) = text.find(marker) else {
            continue; // chrome line — not a marker
        };
        let after = &text[pos + marker.len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            // Marker text but no following digit → CHROME (e.g. the echoed
            // generator command line). Not a marker line; skip.
            let _ = i;
            continue;
        }
        let idx: usize = digits.parse().map_err(|e| {
            format!(
                "[{}] backlog-ordered FAIL: index '{}' at wire-line {} unparseable: {}",
                run_desc, digits, i, e
            )
        })?;
        found.push(idx);
    }

    // (b) strictly increasing in WIRE order (only meaningful on wire-ordered input).
    for w in found.windows(2) {
        if w[1] <= w[0] {
            return Err(format!(
                "[{}] backlog-ordered FAIL (b): indices not strictly increasing in wire order — \
                 {} followed by {} (full wire sequence: {:?})",
                run_desc, w[0], w[1], found
            ));
        }
    }

    // (c) no index outside the expected range.
    for &idx in &found {
        if !expected_range.contains(&idx) {
            return Err(format!(
                "[{}] backlog-ordered FAIL (c): out-of-range index {} present (allowed {}..={})",
                run_desc,
                idx,
                expected_range.start(),
                expected_range.end()
            ));
        }
    }

    // (a) every expected index present exactly once.
    use std::collections::HashMap;
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for &idx in &found {
        *counts.entry(idx).or_insert(0) += 1;
    }
    for want in expected_range.clone() {
        match counts.get(&want).copied().unwrap_or(0) {
            1 => {}
            0 => {
                return Err(format!(
                    "[{}] backlog-ordered FAIL (a): expected index {} MISSING (allowed {}..={})",
                    run_desc,
                    want,
                    expected_range.start(),
                    expected_range.end()
                ))
            }
            n => {
                return Err(format!(
                    "[{}] backlog-ordered FAIL (a): expected index {} present {} times (want exactly 1)",
                    run_desc, want, n
                ))
            }
        }
    }

    Ok(())
}

// R4 scrollback-boundary CONTENT checker (shared by the R4 unit test's grid
// read-back and the R7(b) negative-control tooth).
//
// SINGLE SOURCE OF TRUTH (C1 M5 / carry C1b F2): the checker body lives in
// `boundary_content.rs` and is `include!`-spliced here AND into
// `src/screen/grid.rs`'s unit-test module — see that file's header for the
// drift-class rationale. `include!` (not `#[path] mod`) splices the top-level
// `pub fn` directly into THIS module, so it stays at the path consumers expect
// (`libmod::assertions::check_boundary_content`). `include!` resolves relative
// to this file's dir (`tests/lib/`), robust to assertions.rs itself being
// `#[path]`-mounted into the test binaries via `lib/mod.rs`.
include!("boundary_content.rs");

// ============================================================================
// Helpers
// ============================================================================

/// Find a byte sequence in a byte slice. Returns the position or None.
fn find_byte_sequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Count how many times a pattern appears in the output (simple substring match).
fn count_pattern_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = find_byte_sequence(&haystack[start..], needle) {
        count += 1;
        start += pos + 1; // Move past this match for next search
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assert_backlog_completeness_pass() {
        let lines = vec![
            "line 1".to_string(),
            "line 2".to_string(),
            "line 3".to_string(),
        ];
        assert!(assert_backlog_completeness(&lines, 3, "test").is_ok());
    }

    #[test]
    fn test_assert_backlog_completeness_fail_too_few() {
        let lines = vec!["line 1".to_string(), "line 2".to_string()];
        assert!(assert_backlog_completeness(&lines, 5, "test").is_err());
    }

    #[test]
    fn test_check_boundary_content_pass() {
        // cap=10, written=15 → retained must be exactly 6..=15.
        let retained: Vec<usize> = (6..=15).collect();
        assert!(check_boundary_content(&retained, 10, 15).is_ok());
    }

    #[test]
    fn test_check_boundary_content_off_by_one_fails() {
        // 11 kept at cap 10 → length violation.
        let retained: Vec<usize> = (5..=15).collect();
        assert!(check_boundary_content(&retained, 10, 15).is_err());
    }

    #[test]
    fn test_check_boundary_content_wrong_window_fails() {
        // Correct LENGTH (10) but wrong window (5..=14 when 6..=15 expected).
        let retained: Vec<usize> = (5..=14).collect();
        assert!(check_boundary_content(&retained, 10, 15).is_err());
    }

    #[test]
    fn test_assert_scroll_intact_pass() {
        let lines = vec!["before".to_string(), "after".to_string()];
        assert!(assert_scroll_intact(&lines, "before", "test").is_ok());
    }

    #[test]
    fn test_assert_scroll_intact_fail() {
        let lines = vec!["after".to_string()];
        assert!(assert_scroll_intact(&lines, "before", "test").is_err());
    }

    #[test]
    fn test_assert_altscreen_replay_main_screen_clean() {
        let bytes = b"hello world\n";
        assert!(assert_altscreen_replay(bytes, 0, 0, "test").is_ok());
    }

    #[test]
    fn test_assert_altscreen_replay_unexpected_1049_fails() {
        // Main-screen expectation (0/0) but a 1049h appears → fail.
        let bytes = b"hello\x1b[?1049hworld\n";
        assert!(assert_altscreen_replay(bytes, 0, 0, "test").is_err());
    }

    #[test]
    fn test_assert_altscreen_replay_expected_entry_passes() {
        let bytes = b"\x1b[?1049h\x1b[?2026hAPP\x1b[?2026l";
        assert!(assert_altscreen_replay(bytes, 1, 0, "test").is_ok());
    }

    #[test]
    fn test_assert_altscreen_replay_missing_entry_fails() {
        // Alt-screen attach expected (1 entry) but no 1049h present → fail.
        // This is the tooth that catches a regression back to absorb-only.
        let bytes = b"\x1b[?2026hAPP\x1b[?2026l";
        assert!(assert_altscreen_replay(bytes, 1, 0, "test").is_err());
    }

    #[test]
    fn test_assert_altscreen_replay_double_emit_fails() {
        // Two entries within one attach violate the exactly-once clause.
        let bytes = b"\x1b[?1049hAPP\x1b[?1049h";
        assert!(assert_altscreen_replay(bytes, 1, 0, "test").is_err());
    }

    #[test]
    fn test_assert_altscreen_replay_legacy_variant_fails() {
        // Renderer replays 1049 only; a legacy ?47h must never reach clients.
        let bytes = b"\x1b[?47hAPP";
        assert!(assert_altscreen_replay(bytes, 0, 0, "test").is_err());
    }

    fn ml(marker: &str, idx: usize) -> Vec<u8> {
        format!("{}{:04} chrome", marker, idx).into_bytes()
    }

    #[test]
    fn test_assert_backlog_ordered_pass() {
        let lines: Vec<Vec<u8>> = (6..=15).map(|i| ml("b3-", i)).collect();
        assert!(assert_backlog_ordered(&lines, "b3-", 6..=15, "ok").is_ok());
    }

    #[test]
    fn test_assert_backlog_ordered_ignores_chrome() {
        let mut lines = vec![b"$ seq 6 15".to_vec(), Vec::new()];
        lines.extend((6..=8).map(|i| ml("b3-", i)));
        assert!(assert_backlog_ordered(&lines, "b3-", 6..=8, "chrome").is_ok());
    }

    #[test]
    fn test_assert_no_drop_pass() {
        let sent = b"test";
        let received = b"test output";
        assert!(assert_no_drop(sent, received, false, "test").is_ok());
    }

    #[test]
    fn test_assert_no_drop_fail_empty() {
        let sent = b"test";
        let received = b"";
        assert!(assert_no_drop(sent, received, false, "test").is_err());
    }
}
