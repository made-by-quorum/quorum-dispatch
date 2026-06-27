#!/usr/bin/env bash
# test/golden/lib/compare.sh — Comparator classes for the golden harness.
#
# Source this. Bash 3.2 floor; sed/awk/grep are POSIX so checks run on macOS and
# Linux CI. Each comparator returns 0 on PASS, non-zero on FAIL, and prints a
# DISTINCT failure reason to stderr. The mutation test relies on each comparator
# returning a real failure for the divergence class it owns.
#
# Comparator classes (see ADR comparator-classes):
#   compare_byte_exact <expected> <actual>     — normalized byte-for-byte
#   assert_no_altscreen <capture>              — zero ?1049h/?47h/?1047h
#   assert_backlog_complete <capture> <re> <n> — every expected backlog line present, in order
#   assert_scroll_intact <pre> <post>          — pre-detach lines survive into reattach
#   assert_exit_code <actual> <expected>       — exit code matches
#   assert_boot_ready_event <pidfile> <status> — PID-file appeared AND went busy
#
# The asserter's failure taxonomy distinguishes these from a DEADLINE failure
# (enforced in verify.sh), so a liveness regression is never silently a diff.
# ---------------------------------------------------------------------------

CMP_ALTSCREEN_RE='\x1b\[\?(1049|47|1047)[hl]'

_cmp_fail() {
    printf '[compare] FAIL(%s): %s\n' "$1" "$2" >&2
}

# ---------------------------------------------------------------------------
# compare_byte_exact <expected_file> <actual_file>
# Both files are assumed ALREADY normalized. Byte-for-byte diff.
compare_byte_exact() {
    local exp="$1" act="$2"
    if [ ! -f "$exp" ]; then
        _cmp_fail byte-exact "expected file missing: $exp"
        return 3
    fi
    if [ ! -f "$act" ]; then
        _cmp_fail byte-exact "actual file missing: $act"
        return 3
    fi
    if cmp -s "$exp" "$act"; then
        return 0
    fi
    _cmp_fail byte-exact "normalized output differs: $exp vs $act"
    diff "$exp" "$act" >&2 2>/dev/null || true
    return 1
}

# ---------------------------------------------------------------------------
# assert_no_altscreen <capture_file>
# Passthrough invariant (R1): zero alt-screen enter/exit sequences.
assert_no_altscreen() {
    local cap="$1"
    if [ ! -f "$cap" ]; then
        _cmp_fail no-altscreen "capture missing: $cap"
        return 3
    fi
    # Match alt-screen sequences via the cat -v visible form (^[ for ESC). We use
    # `grep -o | wc -l` (single integer, no per-line/exit-code ambiguity that
    # `grep -c` plus a `|| echo 0` fallback would produce). -a not needed since
    # cat -v already renders the bytes printable.
    local hits
    hits="$(cat -v "$cap" | grep -Eo '\^\[\[\?(1049|47|1047)[hl]' 2>/dev/null | grep -c . 2>/dev/null)"
    hits="$(printf '%s' "$hits" | tr -d '[:space:]')"
    [ -z "$hits" ] && hits=0
    if [ "$hits" -ne 0 ]; then
        _cmp_fail no-altscreen "found $hits alt-screen sequence(s) in $cap (passthrough must emit zero)"
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# assert_backlog_complete <capture_file> <line_regex> <expected_count>
# Backlog-completeness invariant: every line matching <line_regex> that was
# produced is present, AND they appear in non-decreasing order by their embedded
# index. <line_regex> must contain a capture pattern like 'LINE ' followed by a
# number. We extract the numeric index after the marker and verify monotonicity
# and count.
assert_backlog_complete() {
    local cap="$1" marker="$2" expected="$3"
    if [ ! -f "$cap" ]; then
        _cmp_fail backlog-complete "capture missing: $cap"
        return 3
    fi
    # Extract the integer index following the marker on each matching line.
    local indices
    indices="$(cat -v "$cap" \
        | grep -Eo "${marker}[0-9]+" 2>/dev/null \
        | grep -Eo '[0-9]+' 2>/dev/null)"
    local count
    count="$(printf '%s\n' "$indices" | grep -c . 2>/dev/null || echo 0)"
    if [ "${count:-0}" -ne "$expected" ]; then
        _cmp_fail backlog-complete "expected $expected '${marker}N' lines, found $count in $cap"
        return 1
    fi
    # Verify non-decreasing order (no reordering/dropping mid-stream).
    if ! printf '%s\n' "$indices" | awk '
        NR == 1 { prev = $1; next }
        { if ($1 < prev) { printf("[compare] FAIL(backlog-complete): out-of-order at %d after %d\n", $1, prev) > "/dev/stderr"; exit 1 }
          prev = $1 }
    '; then
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# assert_backlog_multiset_exact <capture_file> <marker> <expected_count>
# 0b DELTA-STRENGTH W3.8 (P12) — backlog CONTENT INTEGRITY beyond ordering:
#   (1) SENTINEL: a chosen backlog line is present BYTE-EXACT as a WHOLE line
#       "<marker><k>" (k = the middle index), so a corrupted/renamed sentinel diffs.
#   (2) EXACTLY-ONCE MULTISET: every expected "<marker>1..N" line appears EXACTLY
#       ONCE as a WHOLE captured line (the scenario pre-extracts the capture to one
#       "<marker>i" token per line). This catches BOTH:
#         - DUPLICATES (a line emitted/replayed twice -> count 2 for that index), and
#         - BANNER-PREFIX corruption (a present line like "BANNER: <marker>7" is NOT
#           the whole-line literal "<marker>7", so it is MISSING from the multiset and
#           a stray prefixed line is EXTRA). The ordering check assert_backlog_complete
#           uses -o substring extraction and CANNOT see a prefix; this whole-line
#           multiset check does.
# Input contract: <capture_file> holds the extracted backlog, ONE "<marker>i" token
# per line (cat -v already applied by the scenario). Returns 0/1.
assert_backlog_multiset_exact() {
    local cap="$1" marker="$2" expected="$3"
    if [ ! -f "$cap" ]; then
        _cmp_fail backlog-multiset "capture missing: $cap"
        return 3
    fi
    # SENTINEL: the middle index, asserted byte-exact as a WHOLE line (grep -x).
    local mid=$(( (expected + 1) / 2 ))
    if ! grep -qx "${marker}${mid}" "$cap"; then
        _cmp_fail backlog-multiset "sentinel whole-line '${marker}${mid}' absent (corrupted/prefixed sentinel)"
        return 1
    fi
    # EXACTLY-ONCE MULTISET over 1..N: each expected whole line appears exactly once.
    local i=1 c
    while [ "$i" -le "$expected" ]; do
        c="$(grep -cx "${marker}${i}" "$cap" 2>/dev/null | tr -d '[:space:]')"
        [ -z "$c" ] && c=0
        if [ "$c" -ne 1 ]; then
            _cmp_fail backlog-multiset "expected exactly ONE whole line '${marker}${i}', found $c (duplicate or prefix-corrupted/missing)"
            return 1
        fi
        i=$((i + 1))
    done
    # No EXTRA marker-bearing lines beyond 1..N (a banner-prefixed stray or an
    # out-of-range duplicate index is an extra line carrying the marker token).
    local total
    total="$(grep -c "$marker" "$cap" 2>/dev/null | tr -d '[:space:]')"
    [ -z "$total" ] && total=0
    if [ "$total" -ne "$expected" ]; then
        _cmp_fail backlog-multiset "found $total marker-bearing lines, expected exactly $expected (extra/prefixed line present)"
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# assert_scroll_intact <pre_file> <post_file>
# Scrollback-preserved invariant across detach/reattach: every non-empty
# printable line present in <pre_file> must also be present in <post_file>.
assert_scroll_intact() {
    local pre="$1" post="$2"
    if [ ! -f "$pre" ] || [ ! -f "$post" ]; then
        _cmp_fail scroll-intact "missing pre ($pre) or post ($post) capture"
        return 3
    fi
    # Strip ANSI, keep non-empty lines, check each pre-line appears in post.
    local strip='s/\x1b\[[0-9;?]*[a-zA-Z]//g'
    local missing=0 line
    # Use a temp file of stripped post lines for fast membership tests.
    local post_stripped
    post_stripped="$(mktemp)"
    cat -v "$post" | sed -E 's/\^\[\[[0-9;?]*[a-zA-Z]//g' > "$post_stripped"
    while IFS= read -r line; do
        [ -z "$(printf '%s' "$line" | tr -d '[:space:]')" ] && continue
        if ! grep -qF "$line" "$post_stripped"; then
            _cmp_fail scroll-intact "pre-detach line absent after reattach: $line"
            missing=1
            break
        fi
    done < <(cat -v "$pre" | sed -E 's/\^\[\[[0-9;?]*[a-zA-Z]//g')
    rm -f "$post_stripped"
    return $missing
}

# ---------------------------------------------------------------------------
# assert_exit_code <actual> <expected>
assert_exit_code() {
    local actual="$1" expected="$2"
    if [ "$actual" = "$expected" ]; then
        return 0
    fi
    _cmp_fail exit-code "exit code $actual != expected $expected"
    return 1
}

# ---------------------------------------------------------------------------
# assert_boot_ready_event <pidfile_path> <went_busy:0|1>
# Boot-readiness EVENT contract (spec §3.3, deliverable 5): readiness =
# PID-file appearance AND a went-busy transition. NOT a blind-Enter keystroke
# loop. This is the sanctioned divergence from TS — encoded structurally here.
assert_boot_ready_event() {
    local pidfile="$1" went_busy="$2"
    local ok=1 reasons=""
    if [ ! -f "$pidfile" ]; then
        ok=0
        reasons="${reasons} PID-file did not appear ($pidfile);"
    fi
    if [ "$went_busy" != "1" ]; then
        ok=0
        reasons="${reasons} session never went busy;"
    fi
    if [ "$ok" -ne 1 ]; then
        _cmp_fail boot-ready-event "readiness EVENT contract unmet:$reasons"
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# assert_resolution_outcome <resolved_dir> <expected_dir>
# semantic (resolution-outcome) class — ADR-0004 DIV-9a-1/2. The load-bearing
# property of zmx-dir resolution is the OUTCOME (which socket dir wins / what a
# compounded TMPDIR collapses to), NOT a byte-for-byte fabricated line. qd exposes
# no print-resolved-dir surface, so the scenario observes the outcome (where a
# session's socket actually lands / which dir resolveZmxDir targets) and this
# comparator asserts it equals the expected canonical dir.
assert_resolution_outcome() {
    local resolved="$1" expected="$2"
    if [ -z "$resolved" ]; then
        _cmp_fail resolution-outcome "no resolved dir observed"
        return 1
    fi
    if [ "$resolved" = "$expected" ]; then
        return 0
    fi
    _cmp_fail resolution-outcome "resolved dir '$resolved' != expected '$expected'"
    return 1
}

# ---------------------------------------------------------------------------
# assert_boot_readiness_event <capture_file>
# Replay comparator for the boot-readiness-event class (new_session_trace.sh). The
# recorded golden is the EVENT outcome; this asserts the SAME fields the scenario's
# scn_assert keys on, so a replay exercises the row's ACTUAL comparator (red-team
# #1): PID file appeared AND name matched AND the session reached ready/idle.
#
# W3.3 (P2): also assert the strengthened EVENT fields WHEN PRESENT in the capture
# (assert-if-present keeps older boot-trace captures that lack them passing, while a
# W3.3 golden's fields ARE checked — so the W3.3 mutants, replayed through this
# comparator, bite): went-busy observed, returned-to-idle, pre-PID stdin count == 0,
# and the decoy rejected (engine matched OUR name, not the pre-seeded decoy).
assert_boot_readiness_event() {
    local cap="$1"
    if [ ! -f "$cap" ]; then
        _cmp_fail boot-readiness-event "capture missing: $cap"
        return 3
    fi
    grep -q 'EVENT pidfile_appeared=1' "$cap" || { _cmp_fail boot-readiness-event "PID file did not appear"; return 1; }
    grep -q 'EVENT name_matched=1'      "$cap" || { _cmp_fail boot-readiness-event "name not matched"; return 1; }
    grep -q 'EVENT status_ready_idle=1' "$cap" || { _cmp_fail boot-readiness-event "session never reached ready/idle"; return 1; }
    # --- W3.3 strengthened fields (assert-if-present) ---
    if grep -q 'EVENT went_busy_observed=' "$cap"; then
        grep -q 'EVENT went_busy_observed=1' "$cap" || { _cmp_fail boot-readiness-event "probe submit never observed idle->busy"; return 1; }
    fi
    if grep -q 'EVENT returned_idle_after_busy=' "$cap"; then
        grep -q 'EVENT returned_idle_after_busy=1' "$cap" || { _cmp_fail boot-readiness-event "session never returned busy->idle"; return 1; }
    fi
    if grep -q 'EVENT input_chars_before_pidfile=' "$cap"; then
        grep -q 'EVENT input_chars_before_pidfile=0' "$cap" || { _cmp_fail boot-readiness-event "pre-PID stdin count != 0 (stub construction violated / stdin spam)"; return 1; }
    fi
    if grep -q 'EVENT decoy_rejected_matched_our_name=' "$cap"; then
        grep -q 'EVENT decoy_rejected_matched_our_name=1' "$cap" || { _cmp_fail boot-readiness-event "engine matched the DECOY pidfile (grab-any-pidfile) not our name"; return 1; }
    fi
    return 0
}

# ---------------------------------------------------------------------------
# assert_submit_discipline <capture_file>
# Replay comparator for the semantic-submit-discipline class (send_pty_paste_burst.
# sh, ADD-2 queue-to-busy + JSONL --wait). Asserts the SAME fields the scenario's
# scn_assert keys on (red-team #1): both queued messages drained, in order, --wait
# returned the queued reply, and exactly 2 user records (no spurious/duplicate turn).
assert_submit_discipline() {
    local cap="$1"
    if [ ! -f "$cap" ]; then
        _cmp_fail submit-discipline "capture missing: $cap"
        return 3
    fi
    grep -q 'queue_drained_both_users=1' "$cap" || { _cmp_fail submit-discipline "queue did not drain both messages"; return 1; }
    grep -q 'queue_order_ok=1'           "$cap" || { _cmp_fail submit-discipline "queued messages out of order"; return 1; }
    grep -q 'wait_reply_present=1'       "$cap" || { _cmp_fail submit-discipline "--wait returned no reply for the queued message"; return 1; }
    grep -q 'user_record_count=2'        "$cap" || { _cmp_fail submit-discipline "spurious/duplicate user turn (expected exactly 2)"; return 1; }
    # DELTA-STRENGTH (W3.1, red-team m4): ordered user-record TEXTS + anchor + reply
    # text byte-exact. A swapped-order / no-queue (burst discarded) / wrong-reply /
    # truncated-burst impl produces a DIFFERENT capture and FAILS here (the R3
    # STUB_NO_QUEUE control + the swapped-order/wrong-reply mutants bite via this).
    grep -q '^user_text\[0\]=first-turn-holds-busy$' "$cap" || { _cmp_fail submit-discipline "first user record text is not turn1 (order wrong / corrupted / burst discarded)"; return 1; }
    grep -q '^user_text\[1\]=PASTE-BURST '            "$cap" || { _cmp_fail submit-discipline "second user record text is not the burst (order wrong / truncated / discarded)"; return 1; }
    grep -q '^anchored_on_user_text=PASTE-BURST '     "$cap" || { _cmp_fail submit-discipline "--wait did not anchor on the burst user record"; return 1; }
    grep -q '^wait_reply_text=STUB-REPLY to: PASTE-BURST ' "$cap" || { _cmp_fail submit-discipline "--wait reply text is not the deterministic STUB-REPLY for the burst"; return 1; }
    return 0
}
