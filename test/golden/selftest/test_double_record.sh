#!/usr/bin/env bash
# test/golden/selftest/test_double_record.sh — prove record.sh is double-record
# BY CONSTRUCTION (red-team M4).
#
# Two cases, both fully hermetic (NO real qd, NO real fixtures/):
#   (1) a DELIBERATELY run-varying scenario (scn_run emits $RANDOM) -> record.sh
#       MUST FAIL with the double-record-mismatch exit (72), NO expectation written.
#   (2) a DETERMINISTIC scenario -> record.sh succeeds end-to-end, writing the
#       expectation + BOTH raws + a MATCH-PROOF, admitted into a SCRATCH fixtures
#       root.
#
# Part-2 gate bypass for the selftest ONLY (per the contract): we supply a FAKE
# pin AND a FAKE prep-verified clone (a scratch dir with a .prep-verified marker
# claiming the fake pin) so G1/G2 pass, and RECORD_FIXTURES_ROOT points at a
# SCRATCH dir so we NEVER write real fixtures/. Everything is cleaned up.
#
# Bash 3.2 floor. Run directly.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"   # test/golden
RECORD="$ROOT/record.sh"

PASS=0
FAIL=0
FAKEPIN="deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/dblrec-selftest.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT INT TERM

# --- A FAKE prep-verified clone (so G2 passes) -------------------------------
CLONE="$SCRATCH/fake-clone"
mkdir -p "$CLONE/src"
printf '// fake entrypoint for selftest\n' > "$CLONE/src/index.ts"
{ printf 'PREP-VERIFIED\n'; printf 'pinned_ts_commit=%s\n' "$FAKEPIN"; } > "$CLONE/.prep-verified"
SUT="bun $CLONE/src/index.ts"

FXROOT="$SCRATCH/fixtures"   # scratch fixtures root — NEVER the real tree

# --- A DETERMINISTIC scenario ------------------------------------------------
DET="$SCRATCH/det_scenario.sh"
cat > "$DET" <<'EOS'
SCN_NAME="det"
SCN_BUDGET_MS=4000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/det-corpus/normalized/out.txt"
scn_run() {
    # Fully deterministic content — no qd, no time, no random.
    printf 'deterministic line one\ndeterministic line two\n' > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
}
EOS

# --- A run-VARYING scenario (emits $RANDOM) ----------------------------------
VAR="$SCRATCH/var_scenario.sh"
cat > "$VAR" <<'EOS'
SCN_NAME="varying"
SCN_BUDGET_MS=4000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/var-corpus/normalized/out.txt"
scn_run() {
    # Deliberately non-deterministic: a bare random integer the normalizer does
    # NOT collapse (it is not a PID/timestamp/port token). Run A != Run B.
    printf 'random=%s\n' "$RANDOM$RANDOM" > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
}
EOS

run_record() {
    local scn="$1"
    PINNED_TS_COMMIT="$FAKEPIN" \
    QD_UNDER_TEST="$SUT" \
    RECORD_FIXTURES_ROOT="$FXROOT" \
    JAIL_SB_CMD="/bin/true" \
        bash "$RECORD" --scenario "$scn"
}

# --- Case 1: varying scenario -> MISMATCH exit 72, nothing written -----------
run_record "$VAR" >/dev/null 2>&1
rc=$?
if [ "$rc" -eq 72 ]; then
    PASS=$((PASS + 1)); printf 'ok   varying/mismatch-exit-72\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL varying/mismatch-exit-72 — wanted 72, got %s\n' "$rc"
fi
if [ ! -e "$FXROOT/var-corpus" ]; then
    PASS=$((PASS + 1)); printf 'ok   varying/no-expectation-written\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL varying/no-expectation-written — a fixture was written on mismatch!\n'
fi

# --- Case 2: deterministic scenario -> success end-to-end --------------------
run_record "$DET" >/dev/null 2>&1
rc=$?
if [ "$rc" -eq 0 ]; then
    PASS=$((PASS + 1)); printf 'ok   deterministic/record-succeeds\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL deterministic/record-succeeds — wanted 0, got %s\n' "$rc"
fi
DEST="$FXROOT/det-corpus"
if [ -f "$DEST/normalized/out.txt" ]; then
    PASS=$((PASS + 1)); printf 'ok   deterministic/expectation-written\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL deterministic/expectation-written — normalized expectation missing\n'
fi
if [ -f "$DEST/raw/out.txt.runA.raw" ] && [ -f "$DEST/raw/out.txt.runB.raw" ]; then
    PASS=$((PASS + 1)); printf 'ok   deterministic/both-raws-written\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL deterministic/both-raws-written — .runA/.runB raw missing\n'
fi
if [ -f "$DEST/MATCH-PROOF" ]; then
    # Proof must reference the normalizer + scenario hashes (so a change forces re-record).
    if grep -q '^normalizer_sha256=' "$DEST/MATCH-PROOF" && grep -q '^scenario_sha256=' "$DEST/MATCH-PROOF"; then
        PASS=$((PASS + 1)); printf 'ok   deterministic/match-proof-binds-normalizer+scenario\n'
    else
        FAIL=$((FAIL + 1)); printf 'FAIL deterministic/match-proof-binds — missing normalizer/scenario hash\n'
    fi
else
    FAIL=$((FAIL + 1)); printf 'FAIL deterministic/match-proof-binds — MATCH-PROOF missing\n'
fi

# --- Safety: real fixtures/ untouched ----------------------------------------
if [ ! -e "$ROOT/fixtures/det-corpus" ] && [ ! -e "$ROOT/fixtures/var-corpus" ]; then
    PASS=$((PASS + 1)); printf 'ok   safety/real-fixtures-untouched\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL safety/real-fixtures-untouched — leaked into the REAL fixtures tree!\n'
fi

printf '\n--- test_double_record: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
