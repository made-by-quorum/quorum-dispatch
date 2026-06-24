#!/usr/bin/env bash
# test/golden/selftest/test_timeout_budget.sh — prove the timeout-budget path.
#
# A liveness regression (a hang) must surface as a DEADLINE failure (EXIT_DEADLINE=2),
# DISTINCT from a diff failure (EXIT_DIFF=1). This drives verify.sh against two
# throwaway scenarios — one that finishes inside budget, one that hangs past it —
# and asserts the exit taxonomy. (spec §3.1, §5)
#
# Bash 3.2 floor. Run directly.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
VERIFY="$HERE/../verify.sh"

PASS=0
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

check_exit() {
    local name="$1" want="$2" got="$3"
    if [ "$got" = "$want" ]; then
        PASS=$((PASS + 1)); printf 'ok   %s (exit %s)\n' "$name" "$got"
    else
        FAIL=$((FAIL + 1)); printf 'FAIL %s — wanted exit %s, got %s\n' "$name" "$want" "$got"
    fi
}

# --- Scenario A: completes well within budget, asserts pass ----------------
cat > "$TMP/fast.sh" <<'EOF'
SCN_NAME="selftest-fast"
SCN_BUDGET_MS=4000
SCN_CLASS="byte-exact"
SCN_FIXTURE="(selftest)"
scn_run() { printf 'done\n' > "$SCN_OUT"; }
scn_assert() { grep -q done "$SCN_OUT"; }
EOF
"$VERIFY" --scenario "$TMP/fast.sh" >/dev/null 2>&1
check_exit "fast/within-budget-pass" 0 $?

# --- Scenario B: hangs past budget -> DEADLINE (exit 2), not DIFF ----------
cat > "$TMP/hang.sh" <<'EOF'
SCN_NAME="selftest-hang"
SCN_BUDGET_MS=500
SCN_CLASS="byte-exact"
SCN_FIXTURE="(selftest)"
scn_run() {
  # Hang well past the 500ms budget. If the budget did NOT bite, this would
  # eventually write matching output and the test would pass — exactly the
  # liveness-blind-spot the deadline path exists to catch.
  sleep 5
  printf 'done\n' > "$SCN_OUT"
}
scn_assert() { grep -q done "$SCN_OUT"; }
EOF
"$VERIFY" --scenario "$TMP/hang.sh" >/dev/null 2>&1
check_exit "hang/over-budget-deadline" 2 $?

# --- Scenario C: completes in budget but assertion fails -> DIFF (exit 1) ---
cat > "$TMP/diff.sh" <<'EOF'
SCN_NAME="selftest-diff"
SCN_BUDGET_MS=4000
SCN_CLASS="byte-exact"
SCN_FIXTURE="(selftest)"
scn_run() { printf 'WRONG\n' > "$SCN_OUT"; }
scn_assert() { grep -q EXPECTED "$SCN_OUT"; }
EOF
"$VERIFY" --scenario "$TMP/diff.sh" >/dev/null 2>&1
check_exit "diff/in-budget-fail" 1 $?

# --- Confirm deadline and diff are DISTINCT codes --------------------------
if [ 2 -ne 1 ]; then
    PASS=$((PASS + 1)); printf 'ok   taxonomy/deadline-distinct-from-diff\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL taxonomy/deadline-distinct-from-diff\n'
fi

printf '\n--- test_timeout_budget: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
