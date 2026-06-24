#!/usr/bin/env bash
# test/golden/selftest/test_secret_scan.sh — prove the L11 secret-scan carrier.
#
# Asserts: (1) the carrier's own selftest passes (planted real-shaped key BITES,
# clean tree PASSES); (2) the short placeholder `sk-or-FAKE-0000` PASSES (the
# corpus relies on it); (3) a directory holding a planted real-shaped key is
# REFUSED; (4) the record.sh wrong-pin gate FIRES (PINNED_TS_REPO HEAD != pin).
#
# Bash 3.2 floor. Run directly.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCAN="$HERE/../lib/secret-scan.sh"
RECORD="$HERE/../record.sh"

PASS=0; FAIL=0
_ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
_no()   { FAIL=$((FAIL+1)); printf 'FAIL %s\n' "$1"; }

# (1) carrier selftest (negative control bite + clean pass).
if bash "$SCAN" --selftest >/dev/null 2>&1; then
    _ok "secret-scan/selftest (planted real-shaped key BIT; clean PASSED)"
else
    _no "secret-scan/selftest"
fi

# (2) the documented short placeholder must PASS (not a false positive).
TMP_OK="$(mktemp)"; printf 'openrouter-key: sk-or-FAKE-0000\n' > "$TMP_OK"
if bash "$SCAN" "$TMP_OK" >/dev/null 2>&1; then
    _ok "secret-scan/placeholder-passes (sk-or-FAKE-0000 below anchor)"
else
    _no "secret-scan/placeholder-passes"
fi
rm -f "$TMP_OK"

# (3) a planted REAL-shaped key in a dir must be REFUSED (exit 1).
TMP_DIR="$(mktemp -d)"
printf 'leak: sk-or-v1-planted0123456789abcdef0123456789\n' > "$TMP_DIR/leak.txt"
if bash "$SCAN" "$TMP_DIR" >/dev/null 2>&1; then
    _no "secret-scan/dir-bites (did NOT refuse a planted key)"
else
    _ok "secret-scan/dir-bites (refused planted real-shaped key)"
fi
rm -rf "$TMP_DIR"

# (4) record.sh wrong-pin gate: PINNED_TS_REPO HEAD must equal PINNED_TS_COMMIT.
# Build a throwaway git repo with a known HEAD, then declare a DIFFERENT pin.
TMP_REPO="$(mktemp -d)"
(
    cd "$TMP_REPO" || exit 1
    git init -q
    git config user.email t@t; git config user.name t
    printf 'x\n' > f; git add f; git commit -qm init
) >/dev/null 2>&1
out="$(PINNED_TS_COMMIT=deadbeefdeadbeefdeadbeefdeadbeefdeadbeef \
       PINNED_TS_REPO="$TMP_REPO" \
       bash "$RECORD" --scenario /dev/null 2>&1)"
rc=$?
if [ "$rc" -eq 70 ] && printf '%s' "$out" | grep -q "does not match the pin"; then
    _ok "secret-scan/wrong-pin-gate (record.sh refused mismatched TS HEAD, exit 70)"
else
    _no "secret-scan/wrong-pin-gate — wanted exit 70 + mismatch message, got rc=$rc"
fi
rm -rf "$TMP_REPO"

printf '\n--- test_secret_scan: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
