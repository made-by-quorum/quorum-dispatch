#!/usr/bin/env bash
# test/golden/selftest/test_prep_pinned_ts.sh — prove the pinned-clone prep + the
# record.sh G2 refusal that closes the scenario-bypass hole (red-team m2 +
# scenario-bypass).
#
# Cases (all hermetic — a SCRATCH git repo stands in for the TS source, so this
# NEVER clones or touches ~/work/switchboard; PREP_SKIP_BUN=1 avoids the bun dep):
#   (1) prep with the CORRECT pin -> succeeds, writes .prep-verified with the pin.
#   (2) prep with a WRONG pin -> REFUSES (pin not reachable / HEAD mismatch), and
#       leaves NO clone behind.
#   (3) prep dest UNDER the qd-rust repo tree -> REFUSED (containment guard).
#   (4) record.sh G2: QD_UNDER_TEST NOT under a prep-verified clone -> REFUSED
#       (exit 71), no fixture written.
#   (5) record.sh G2: QD_UNDER_TEST under a clone whose marker pin MISMATCHES the
#       supplied pin -> REFUSED (exit 71).
#
# Bash 3.2 floor. Run directly.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"   # test/golden
REPO_TOP="$(cd "$ROOT/../.." && pwd)"
PREP="$ROOT/prep_pinned_ts.sh"
RECORD="$ROOT/record.sh"
. "$ROOT/lib/prep_verify.sh"

PASS=0
FAIL=0
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/prep-selftest.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT INT TERM

# --- Build a SCRATCH git repo standing in for the TS source ------------------
SRC="$SCRATCH/fake-ts-src"
mkdir -p "$SRC/src"
(
    cd "$SRC"
    git init -q
    git config user.email selftest@example.com
    git config user.name selftest
    printf '// entry\n' > src/index.ts
    git add -A
    git commit -q -m "fake ts commit"
) >/dev/null 2>&1
GOOD_PIN="$( cd "$SRC" && git rev-parse HEAD )"
BAD_PIN="0000000000000000000000000000000000000000"

# --- 1. prep with the CORRECT pin succeeds + writes marker -------------------
DEST1="$SCRATCH/prep/$GOOD_PIN-good"
if PREP_SKIP_BUN=1 bash "$PREP" --pin "$GOOD_PIN" --src "$SRC" --dest "$DEST1" >/dev/null 2>&1; then
    if [ -f "$DEST1/.prep-verified" ] && grep -q "pinned_ts_commit=$GOOD_PIN" "$DEST1/.prep-verified"; then
        PASS=$((PASS + 1)); printf 'ok   prep/good-pin-succeeds+marker\n'
    else
        FAIL=$((FAIL + 1)); printf 'FAIL prep/good-pin-succeeds+marker — marker missing/wrong\n'
    fi
else
    FAIL=$((FAIL + 1)); printf 'FAIL prep/good-pin-succeeds+marker — prep refused a valid pin\n'
fi

# --- 2. prep with a WRONG pin REFUSES + leaves no clone ----------------------
DEST2="$SCRATCH/prep/$BAD_PIN-bad"
if PREP_SKIP_BUN=1 bash "$PREP" --pin "$BAD_PIN" --src "$SRC" --dest "$DEST2" >/dev/null 2>&1; then
    FAIL=$((FAIL + 1)); printf 'FAIL prep/wrong-pin-refuses — prep ACCEPTED an unreachable pin\n'
else
    PASS=$((PASS + 1)); printf 'ok   prep/wrong-pin-refuses\n'
fi
if [ ! -e "$DEST2" ]; then
    PASS=$((PASS + 1)); printf 'ok   prep/wrong-pin-no-clone-left\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL prep/wrong-pin-no-clone-left — a clone was left behind\n'
fi

# --- 3. dest under the qd-rust repo tree -> REFUSED --------------------------
DEST3="$REPO_TOP/test/golden/.scratch-prep-should-refuse"
if PREP_SKIP_BUN=1 bash "$PREP" --pin "$GOOD_PIN" --src "$SRC" --dest "$DEST3" >/dev/null 2>&1; then
    FAIL=$((FAIL + 1)); printf 'FAIL prep/dest-in-repo-refused — prep cloned INTO the qd-rust repo!\n'
    rm -rf "$DEST3" 2>/dev/null
else
    PASS=$((PASS + 1)); printf 'ok   prep/dest-in-repo-refused\n'
fi
[ -e "$DEST3" ] && { FAIL=$((FAIL + 1)); printf 'FAIL prep/dest-in-repo-no-leak — left a dir in the repo\n'; rm -rf "$DEST3"; } || { PASS=$((PASS + 1)); printf 'ok   prep/dest-in-repo-no-leak\n'; }

# --- 3b. dest under the ORG TS checkout -> REFUSED ---------------------------
# (Uses the scratch SRC as the org-checkout stand-in so we never touch the real
# ~/work/switchboard.) The org checkout's branches/HEAD must never be at risk.
DEST3B="$SRC/inside-org-checkout-should-refuse"
if PREP_SKIP_BUN=1 bash "$PREP" --pin "$GOOD_PIN" --src "$SRC" --dest "$DEST3B" >/dev/null 2>&1; then
    FAIL=$((FAIL + 1)); printf 'FAIL prep/dest-in-org-refused — prep cloned INTO the org TS checkout!\n'
    rm -rf "$DEST3B" 2>/dev/null
else
    PASS=$((PASS + 1)); printf 'ok   prep/dest-in-org-refused\n'
fi
[ -e "$DEST3B" ] && { FAIL=$((FAIL + 1)); printf 'FAIL prep/dest-in-org-no-leak\n'; rm -rf "$DEST3B"; } || { PASS=$((PASS + 1)); printf 'ok   prep/dest-in-org-no-leak\n'; }

# --- 4. record.sh G2: QD_UNDER_TEST NOT under a prep clone -> exit 71 --------
FXROOT="$SCRATCH/fixtures"
DET="$SCRATCH/det.sh"
cat > "$DET" <<'EOS'
SCN_NAME="det"; SCN_BUDGET_MS=4000; SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/g2-corpus/normalized/out.txt"
scn_run() { printf 'x\n' > "$SCN_OUT"; printf '0\n' > "$SCN_OUT.exit"; }
EOS
# Point QD_UNDER_TEST at a bare path with NO .prep-verified above it.
PINNED_TS_COMMIT="$GOOD_PIN" \
QD_UNDER_TEST="bun $SCRATCH/not-a-clone/index.ts" \
RECORD_FIXTURES_ROOT="$FXROOT" \
JAIL_SB_CMD="/bin/true" \
    bash "$RECORD" --scenario "$DET" >/dev/null 2>&1
rc=$?
if [ "$rc" -eq 71 ]; then
    PASS=$((PASS + 1)); printf 'ok   record-g2/no-prep-clone-refused (exit 71)\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL record-g2/no-prep-clone-refused — wanted 71, got %s\n' "$rc"
fi
[ ! -e "$FXROOT/g2-corpus" ] && { PASS=$((PASS + 1)); printf 'ok   record-g2/no-fixture-on-refusal\n'; } \
    || { FAIL=$((FAIL + 1)); printf 'FAIL record-g2/no-fixture-on-refusal — wrote a fixture\n'; }

# --- 5. record.sh G2: clone marker pin MISMATCHES supplied pin -> exit 71 ----
# Reuse DEST1 (a valid prep clone with marker pin=$GOOD_PIN) but supply a
# DIFFERENT pin to record.sh. prep_verify must refuse on the pin mismatch.
OTHER_PIN="abcabcabcabcabcabcabcabcabcabcabcabcabca"
PINNED_TS_COMMIT="$OTHER_PIN" \
QD_UNDER_TEST="bun $DEST1/src/index.ts" \
RECORD_FIXTURES_ROOT="$FXROOT" \
JAIL_SB_CMD="/bin/true" \
    bash "$RECORD" --scenario "$DET" >/dev/null 2>&1
rc=$?
if [ "$rc" -eq 71 ]; then
    PASS=$((PASS + 1)); printf 'ok   record-g2/marker-pin-mismatch-refused (exit 71)\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL record-g2/marker-pin-mismatch-refused — wanted 71, got %s\n' "$rc"
fi

# --- 6. prep_verify_entrypoint unit: good clone resolves ---------------------
if prep_verify_entrypoint "bun $DEST1/src/index.ts" "$GOOD_PIN" >/dev/null 2>&1; then
    PASS=$((PASS + 1)); printf 'ok   prep-verify/good-clone-resolves\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL prep-verify/good-clone-resolves — valid clone not recognized\n'
fi

printf '\n--- test_prep_pinned_ts: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
