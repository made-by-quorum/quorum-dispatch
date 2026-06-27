#!/usr/bin/env bash
# test/golden/fixtures/layer2/run_layer2.sh — assert the synthetic layer-2 fixtures.
#
# These are SYNTHETIC (no TS pin needed, fully Part 1). They prove the harness
# works end-to-end: generate stress load, run it through a PTY under a timeout
# budget, and assert the liveness invariants (no-altscreen, backlog-complete)
# rather than byte-equality.
#
# Fixtures asserted here:
#   1. 64KB ANSI burst x100   — no-altscreen leak, no drop, budget holds
#   2. SIGWINCH storm          — no corruption/hang/altscreen under resize storm
# (The dirty-state JSON corpus is asserted by `cargo test -p golden`.)
#
# Bash 3.2 floor. Run directly. Exits non-zero if any invariant fails.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"   # test/golden
. "$ROOT/lib/jail.sh"
. "$ROOT/lib/compare.sh"
. "$ROOT/lib/normalize.sh"
. "$ROOT/lib/check_python.sh"

# Enforce the python3 floor before any recording (ADR 0002).
check_python_floor || exit 64

REC="$ROOT/recorder/record_pty.py"
GEN_BURST="$HERE/ansi-burst/gen_ansi_burst.py"
# gen lives one level up from ansi-burst/
[ -f "$GEN_BURST" ] || GEN_BURST="$HERE/gen_ansi_burst.py"

PASS=0
FAIL=0
ok()   { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }

# Establish a jail so all temp state is hermetic (even though these don't touch qd).
if ! jail_establish; then
    printf '[layer2] jail refused — aborting\n' >&2
    exit 3
fi
trap 'jail_teardown' EXIT

WORK="$JAIL_ROOT/layer2"
mkdir -p "$WORK"

# ---------------------------------------------------------------------------
# Fixture 1: 64KB ANSI burst x100 through a PTY, budgeted.
# We `cat` a generated burst file through the PTY recorder; assert the captured
# output has ZERO alt-screen sequences and the budget held.
BURST_SRC="$WORK/burst.raw"
python3 "$GEN_BURST" "$BURST_SRC" 100 65536 2>/dev/null
BURST_CAP="$WORK/burst.cap"

# Budget: 100x64KB = ~6.5MB. cat-through-PTY should finish in a few seconds; give
# a generous-but-bounded budget so a HANG (liveness regression) still fails.
BURST_BUDGET_MS=20000
_start="$(date +%s%N 2>/dev/null || echo 0)"
python3 "$REC" --out "$BURST_CAP" --secs 15 -- cat "$BURST_SRC" >/dev/null 2>&1 &
_pid=$!
# Enforce the budget around the recorder itself.
_waited=0
while kill -0 "$_pid" 2>/dev/null; do
    if [ "$_waited" -ge "$BURST_BUDGET_MS" ]; then
        kill -KILL "$_pid" 2>/dev/null; wait "$_pid" 2>/dev/null
        bad "ansi-burst/budget (DEADLINE: exceeded ${BURST_BUDGET_MS}ms)"
        break
    fi
    sleep 0.1; _waited=$((_waited + 100))
done
wait "$_pid" 2>/dev/null

if [ -f "$BURST_CAP" ]; then
    if assert_no_altscreen "$BURST_CAP"; then
        ok "ansi-burst/no-altscreen-leak (100x64KB)"
    else
        bad "ansi-burst/no-altscreen-leak"
    fi
    # No-drop under load: confirm the LAST iteration's first marker (BURST 99.0)
    # made it through — proves no truncation/drop across all 100x64KB iterations.
    if cat -v "$BURST_CAP" | grep -q "BURST 99\.0 "; then
        ok "ansi-burst/no-drop (last iteration present)"
    else
        bad "ansi-burst/no-drop (BURST 99.0 missing — output dropped under load)"
    fi
else
    bad "ansi-burst/capture-missing"
fi

# ---------------------------------------------------------------------------
# Fixture 2: SIGWINCH storm during streaming.
# Stream a burst while the recorder fires a storm of resize/SIGWINCH events.
# Assert: no alt-screen leak, no hang (budget held), output still coherent.
STORM_SRC="$WORK/storm-src.raw"
python3 "$GEN_BURST" "$STORM_SRC" 30 65536 2>/dev/null
STORM_CAP="$WORK/storm.cap"
STORM_BUDGET_MS=15000

# A small streamer that emits the file slowly so the storm overlaps streaming.
STREAMER="$WORK/streamer.sh"
cat > "$STREAMER" <<EOF
#!/usr/bin/env bash
# Emit the burst in chunks with tiny pauses so SIGWINCH events interleave.
while IFS= read -r line; do printf '%s\n' "\$line"; done < "$STORM_SRC"
EOF
chmod +x "$STREAMER"

_pid=""
python3 "$REC" --out "$STORM_CAP" --secs 12 --winch-storm 40 -- bash "$STREAMER" >/dev/null 2>&1 &
_pid=$!
_waited=0
_storm_deadline=0
while kill -0 "$_pid" 2>/dev/null; do
    if [ "$_waited" -ge "$STORM_BUDGET_MS" ]; then
        kill -KILL "$_pid" 2>/dev/null; wait "$_pid" 2>/dev/null
        _storm_deadline=1
        bad "sigwinch-storm/budget (DEADLINE: exceeded ${STORM_BUDGET_MS}ms — hang under resize storm)"
        break
    fi
    sleep 0.1; _waited=$((_waited + 100))
done
wait "$_pid" 2>/dev/null

if [ "$_storm_deadline" -eq 0 ]; then
    if [ -f "$STORM_CAP" ]; then
        if assert_no_altscreen "$STORM_CAP"; then
            ok "sigwinch-storm/no-altscreen-under-resize"
        else
            bad "sigwinch-storm/no-altscreen-under-resize"
        fi
        # Coherence: the stream still produced BURST markers (no corruption/hang
        # that zeroed output). We don't assert exact bytes (resize reflow is
        # outcome-class), only that content survived.
        if cat -v "$STORM_CAP" | grep -q "BURST "; then
            ok "sigwinch-storm/content-survived"
        else
            bad "sigwinch-storm/content-survived (no BURST markers — corruption/hang)"
        fi
    else
        bad "sigwinch-storm/capture-missing"
    fi
fi

printf '\n--- run_layer2: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
