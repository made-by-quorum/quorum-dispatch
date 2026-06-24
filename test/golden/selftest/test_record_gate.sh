#!/usr/bin/env bash
# test/golden/selftest/test_record_gate.sh — prove the Part-1/Part-2 boundary.
#
# record.sh mints golden EXPECTATIONS, which is PART 2 — BLOCKED until the
# orchestrator sets PINNED_TS_COMMIT. This test asserts record.sh REFUSES (exit 70)
# when no pin is set, so a Part-1 run can never accidentally bake an expectation.
#
# Bash 3.2 floor. Run directly.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
RECORD="$HERE/../record.sh"

PASS=0
FAIL=0

# Unset any pin in this subshell and assert refusal.
out="$(env -u PINNED_TS_COMMIT bash "$RECORD" --scenario "$HERE/../scenarios/ls_info_json.sh" 2>&1)"
rc=$?
if [ "$rc" -eq 70 ]; then
    PASS=$((PASS + 1)); printf 'ok   record-gate/refuses-without-pin (exit 70)\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL record-gate/refuses-without-pin — wanted exit 70, got %s\n' "$rc"
fi
case "$out" in
    *"PINNED_TS_COMMIT is not set"*)
        PASS=$((PASS + 1)); printf 'ok   record-gate/clear-message\n' ;;
    *)
        FAIL=$((FAIL + 1)); printf 'FAIL record-gate/clear-message — refusal message missing\n' ;;
esac

printf '\n--- test_record_gate: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
