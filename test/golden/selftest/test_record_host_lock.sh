#!/usr/bin/env bash
# test/golden/selftest/test_record_host_lock.sh — prove recording is gated by the
# HOST-wide build-lock captured BEFORE the jail overrides QD_RUST_LOCK_DIR
# (red-team M5).
#
# The hole: jail_establish sets QD_RUST_LOCK_DIR to a jail-internal dir, so a
# naive build-lock around recording would lock against the JAIL's dir and never
# contend with the host's real build mutex — defeating it. record.sh captures the
# HOST lock dir pre-jail (JAIL_HOST_LOCK_DIR) and wraps the scenario run in
# build-lock.sh against THAT dir.
#
# Test: a dummy holder takes the HOST lock and holds it (live PID, so it is NOT
# treated as a stale lock). A recording attempt against the same host lock dir,
# with a SHORT bounded timeout, must FAIL with the host-lock exit (73) rather than
# proceed to write a fixture — and must NOT hang the suite.
#
# Fully hermetic: a SCRATCH host-lock dir, a FAKE prep clone, a SCRATCH fixtures
# root. Cleans up (kills the dummy holder).
#
# Bash 3.2 floor. Run directly.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"   # test/golden
REPO_TOP="$(cd "$ROOT/../.." && pwd)"
RECORD="$ROOT/record.sh"
BUILD_LOCK="$REPO_TOP/scripts/build-lock.sh"

PASS=0
FAIL=0
FAKEPIN="deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/hostlock-selftest.XXXXXX")"
HOLDER_PID=""
cleanup() {
    [ -n "$HOLDER_PID" ] && kill "$HOLDER_PID" 2>/dev/null
    rm -rf "$SCRATCH" 2>/dev/null
}
trap cleanup EXIT INT TERM

# --- FAKE prep clone + scratch fixtures + a deterministic scenario -----------
CLONE="$SCRATCH/fake-clone"; mkdir -p "$CLONE/src"
printf '// fake\n' > "$CLONE/src/index.ts"
{ printf 'PREP-VERIFIED\n'; printf 'pinned_ts_commit=%s\n' "$FAKEPIN"; } > "$CLONE/.prep-verified"
SUT="bun $CLONE/src/index.ts"
FXROOT="$SCRATCH/fixtures"

DET="$SCRATCH/det_scenario.sh"
cat > "$DET" <<'EOS'
SCN_NAME="det"
SCN_BUDGET_MS=4000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/hostlock-corpus/normalized/out.txt"
scn_run() { printf 'deterministic\n' > "$SCN_OUT"; printf '0\n' > "$SCN_OUT.exit"; }
EOS

# --- The HOST lock dir record.sh will use. We force QD_RUST_LOCK_DIR to this so
# record.sh's pre-jail snapshot (JAIL_HOST_LOCK_DIR) captures THIS dir. ---------
HOSTLOCK="$SCRATCH/hostlock"
mkdir -p "$HOSTLOCK"

# --- Dummy holder: takes the host lock and holds it for a while (live PID). ---
# We launch build-lock.sh against the SAME host lock dir, running a long sleep, so
# the lock is genuinely HELD by a live process during the recording attempt.
QD_RUST_LOCK_DIR="$HOSTLOCK" "$BUILD_LOCK" sleep 30 >/dev/null 2>&1 &
HOLDER_PID=$!

# Wait (bounded) until the holder has actually acquired the lock dir.
acquired=0
i=0
while [ "$i" -lt 50 ]; do
    if [ -d "$HOSTLOCK/build.lock" ]; then acquired=1; break; fi
    sleep 0.1; i=$((i + 1))
done
if [ "$acquired" -ne 1 ]; then
    FAIL=$((FAIL + 1)); printf 'FAIL setup/holder-acquired — dummy holder never took the host lock\n'
    printf '\n--- test_record_host_lock: %d passed, %d failed ---\n' "$PASS" "$FAIL"
    exit 1
fi
PASS=$((PASS + 1)); printf 'ok   setup/holder-acquired (host lock held by live dummy)\n'

# --- Recording attempt against the held host lock, SHORT bounded timeout. -----
# QD_RUST_LOCK_DIR=$HOSTLOCK makes record.sh snapshot it as JAIL_HOST_LOCK_DIR
# BEFORE the jail overrides it. QD_RUST_LOCK_TIMEOUT keeps the wait bounded so the
# suite never hangs. Expect exit 73 (host build-lock unavailable), NOT 0.
start=$(date +%s)
PINNED_TS_COMMIT="$FAKEPIN" \
QD_UNDER_TEST="$SUT" \
RECORD_FIXTURES_ROOT="$FXROOT" \
JAIL_QD_CMD="/bin/true" \
QD_RUST_LOCK_DIR="$HOSTLOCK" \
QD_RUST_LOCK_TIMEOUT=2 \
QD_RUST_LOCK_POLL=0.2 \
    bash "$RECORD" --scenario "$DET" >/dev/null 2>&1
rc=$?
end=$(date +%s)
elapsed=$((end - start))

if [ "$rc" -eq 73 ]; then
    PASS=$((PASS + 1)); printf 'ok   recording/blocked-by-host-lock (exit 73)\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL recording/blocked-by-host-lock — wanted exit 73, got %s\n' "$rc"
fi
# Did NOT proceed to write a fixture.
if [ ! -e "$FXROOT/hostlock-corpus" ]; then
    PASS=$((PASS + 1)); printf 'ok   recording/no-fixture-written-while-locked\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL recording/no-fixture-written-while-locked — recorded despite held lock!\n'
fi
# Bounded: must not have hung far beyond the 2s timeout (generous ceiling 20s).
if [ "$elapsed" -lt 20 ]; then
    PASS=$((PASS + 1)); printf 'ok   recording/bounded-wait (%ss < 20s)\n' "$elapsed"
else
    FAIL=$((FAIL + 1)); printf 'FAIL recording/bounded-wait — took %ss (suite-hang risk)\n' "$elapsed"
fi

printf '\n--- test_record_host_lock: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
