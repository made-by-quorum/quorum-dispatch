#!/usr/bin/env bash
# test/golden/mutation/run_mutation.sh — the teeth. (spec §3.8, deliverable 8)
#
# Inject N known divergences into known-good SYNTHETIC captures and assert
# `golden verify` catches ALL of them. This proves the oracle BITES before any
# real corpus exists. Each mutation must produce a CAUGHT failure (verify returns
# non-zero for the right reason); a clean (un-mutated) capture must PASS. Zero
# false negatives is the gate.
#
# In Part 1 this runs against a self-test corpus (built here). The full mutation
# test against the real recorded corpus is Part 2.
#
# Bash 3.2 floor. Run directly. Exits non-zero if any tooth fails to bite.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"   # test/golden
VERIFY="$ROOT/verify.sh"
MUTATE="$HERE/mutate.sh"
. "$ROOT/lib/compare.sh"
. "$ROOT/lib/normalize.sh"

PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# ---------------------------------------------------------------------------
# Build a known-good synthetic corpus.
#
# good_help.raw   — a byte-exact-class capture (help-text + Usage line)
# good_replay.raw — a backlog-complete-class capture (GLINE 1..N monotonic)
# good_cr.raw     — a capture with CR/LF that must survive (dropped-CR target)
# good_pass.raw   — a clean passthrough capture (no-altscreen target)
GOOD_HELP="$TMP/good_help.raw"
printf 'Usage: sb <command>\r\n  ls    list sessions\r\n  new   create\r\n' > "$GOOD_HELP"
GOOD_REPLAY="$TMP/good_replay.raw"
{ i=1; while [ "$i" -le 10 ]; do printf 'GLINE %d payload\r\n' "$i"; i=$((i+1)); done; } > "$GOOD_REPLAY"
GOOD_PASS="$TMP/good_pass.raw"
printf 'normal passthrough output\r\nno alt screen here\r\n' > "$GOOD_PASS"

# ---------------------------------------------------------------------------
# Establish the EXPECTED (normalized) baselines for byte-exact comparisons.
EXP_HELP="$TMP/exp_help.norm"
normalize_all "" "" "" < "$GOOD_HELP" > "$EXP_HELP"

# A "caught" mutation = verify/comparator returns NON-ZERO. A "missed" mutation
# (false negative) = it returns ZERO (PASS) on diverged input — a phase failure.
expect_caught() {
    local name="$1" rc="$2"
    if [ "$rc" -ne 0 ]; then
        ok "$name (caught, rc=$rc)"
    else
        bad "$name (MISSED — false negative; oracle did not bite)"
    fi
}
expect_pass() {
    local name="$1" rc="$2"
    if [ "$rc" -eq 0 ]; then
        ok "$name (clean baseline passes)"
    else
        bad "$name (false positive — clean capture flagged, rc=$rc)"
    fi
}

printf '=== baselines (must PASS) ===\n'
# Clean byte-exact: normalized good_help vs itself.
"$VERIFY" --replay "$GOOD_HELP" --class byte-exact --expected "$GOOD_HELP" >/dev/null 2>&1
expect_pass "baseline/byte-exact-help" $?
# Clean no-altscreen.
"$VERIFY" --replay "$GOOD_PASS" --class no-altscreen >/dev/null 2>&1
expect_pass "baseline/no-altscreen" $?
# Clean backlog-complete (10 monotonic GLINEs).
"$VERIFY" --replay "$GOOD_REPLAY" --class backlog-complete --marker "GLINE " --count 10 >/dev/null 2>&1
expect_pass "baseline/backlog-complete" $?
# Clean exit-code (0 == 0).
"$VERIFY" --replay "$GOOD_HELP" --class exit-code --exit-actual 0 --exit-expected 0 >/dev/null 2>&1
expect_pass "baseline/exit-code" $?

printf '\n=== mutations (must be CAUGHT) ===\n'

# 1. ALTERED HELP TEXT -> byte-exact diff.
MUT="$TMP/mut_help.raw"
"$MUTATE" altered-help "$GOOD_HELP" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$GOOD_HELP" >/dev/null 2>&1
expect_caught "mutation/altered-help" $?

# 2. DROPPED CR -> CR-vs-LF byte-exact diff (CR is never normalized).
MUT="$TMP/mut_cr.raw"
"$MUTATE" dropped-cr "$GOOD_HELP" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$GOOD_HELP" >/dev/null 2>&1
expect_caught "mutation/dropped-cr" $?

# 3. REORDERED REPLAY -> backlog-complete out-of-order catch.
MUT="$TMP/mut_reorder.raw"
"$MUTATE" reordered-replay "$GOOD_REPLAY" "GLINE " > "$MUT"
"$VERIFY" --replay "$MUT" --class backlog-complete --marker "GLINE " --count 10 >/dev/null 2>&1
expect_caught "mutation/reordered-replay" $?

# 4. WRONG EXIT CODE -> exit-code catch (load-bearing, never normalized).
"$VERIFY" --replay "$GOOD_HELP" --class exit-code --exit-actual 1 --exit-expected 0 >/dev/null 2>&1
expect_caught "mutation/wrong-exit-code" $?

# 5. INJECTED ALT-SCREEN -> no-altscreen catch (passthrough invariant).
MUT="$TMP/mut_alt.raw"
"$MUTATE" inject-altscreen "$GOOD_PASS" > "$MUT"
"$VERIFY" --replay "$MUT" --class no-altscreen >/dev/null 2>&1
expect_caught "mutation/inject-altscreen" $?

# 6. DROPPED BACKLOG LINE -> backlog-complete count catch (a line lost while
#    detached, the dtach failure mode).
MUT="$TMP/mut_drop.raw"
grep -v 'GLINE 5 ' "$GOOD_REPLAY" > "$MUT"
"$VERIFY" --replay "$MUT" --class backlog-complete --marker "GLINE " --count 10 >/dev/null 2>&1
expect_caught "mutation/dropped-backlog-line" $?

printf '\n--- run_mutation: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
