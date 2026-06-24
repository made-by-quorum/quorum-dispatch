#!/usr/bin/env bash
# test/golden/selftest/run_selftests.sh — run all golden-harness self-tests.
#
# Aggregates the bash self-tests (normalizers, jail refusal, timeout budget,
# record gate). The Rust dirty-state corpus runs via `cargo test -p golden` and
# the layer-2 + mutation harnesses have their own runners. Bash 3.2 floor.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"

# A7 CONSOLIDATED: the two formerly-parallel scanners (A5's lib/secret-scan.sh +
# main's lib/scan_secrets.sh) were merged into ONE module at lib/secret-scan.sh
# (the A7 CI-fixture-scan carry). BOTH selftest suites are KEPT and BOTH run —
# they are the proof the consolidation lost nothing: test_secret_scan.sh exercises
# the A5 names + the exit-70 wrong-pin gate; test_scan_secrets.sh exercises the
# return-based scan_secrets_* names + the corpus-safe pattern set. Both now source
# the single survivor.
TESTS="test_normalize.sh test_jail_refusal.sh test_timeout_budget.sh test_record_gate.sh test_secret_scan.sh test_scan_secrets.sh test_fetch_zmx.sh test_fmt_check.sh test_fixture_admit.sh test_double_record.sh test_record_host_lock.sh test_prep_pinned_ts.sh test_stub_claude.sh"
FAILED=""

for t in $TESTS; do
    printf '\n========== %s ==========\n' "$t"
    if bash "$HERE/$t"; then
        :
    else
        FAILED="$FAILED $t"
    fi
done

# A4 Level-2 went-busy exit-contract scenario (scenarios/new_went_busy_exit.sh).
# This is a LIVE scenario: it boots fakerepl through REAL zmx and exercises the
# full deliver_prompt remediation timeout (~2 min wall-clock, dominated by the two
# STALL rows). The default fast/hermetic selftest path leaves it OFF — like the
# A2 live rows (dryrun/a2-mac-fakeclaude.sh) and the A3 parity scenario, live
# boots are not in the per-run selftest budget. Opt in with A4_RUN_LIVE=1 (and a
# zmx on PATH); QA/M6 runs it standalone for the actual gate. Registered HERE so
# the canonical entry point can discover and drive it (a4-spec §5).
if [ "${A4_RUN_LIVE:-0}" = "1" ]; then
    SCN="$(cd "$HERE/../scenarios" && pwd)/new_went_busy_exit.sh"
    printf '\n========== new_went_busy_exit.sh (A4 live, A4_RUN_LIVE=1) ==========\n'
    if bash "$SCN"; then
        :
    else
        FAILED="$FAILED new_went_busy_exit.sh"
    fi
else
    printf '\n========== new_went_busy_exit.sh (A4 live) — SKIPPED ==========\n'
    printf 'Set A4_RUN_LIVE=1 to run the went-busy exit-contract scenario (~2 min, needs zmx).\n'
fi

printf '\n==================================\n'
if [ -n "$FAILED" ]; then
    printf 'SELFTESTS FAILED:%s\n' "$FAILED"
    exit 1
fi
printf 'ALL SELFTESTS PASSED\n'
