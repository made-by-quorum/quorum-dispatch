#!/usr/bin/env bash
# test/golden/remint_kill_wave.sh — wart-wave re-mint driver for the
# a5-lifecycle kill fixture (ADD-15 W3+W4). COMMITTED as the mint record /
# reproducer: re-running it against the same binary re-derives the fixture
# (double-run determinism enforced below).
#
# WHY NOT record.sh: the kill surface is now SANCTIONED-DIVERGENT from the TS
# pin (W3 removed the confirm prompt TS still has; W4 changed the success line)
# — the pinned TS CANNOT produce these bytes (it would refuse/prompt). Precedent:
# the D3 NAMED-DIVERGENCE CONTRACT fixture (52-live-nontty.txt). This driver
# keeps record.sh's HONESTY MECHANICS: hermetic jail per run, DOUBLE-RUN with a
# byte-diff of the normalized outputs (determinism proof), raw + normalized both
# written, provenance stamped. SUT = the RUST binary (the contract being pinned).
#
# Usage: QD_BIN=<abs qd> bash test/golden/remint_kill_wave.sh
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
QD_BIN="${QD_BIN:?QD_BIN required (abs path to the Rust qd)}"
[ -x "$QD_BIN" ] || { echo "FATAL: QD_BIN not executable: $QD_BIN"; exit 64; }

STAGE="$HERE/.remint-stage.$$"
mkdir -p "$STAGE"
trap 'rm -rf "$STAGE" 2>/dev/null || true' EXIT INT TERM

one_run() {
    local tag="$1"
    # Subshell: each run gets a fresh jail + fresh scenario sourcing; teardown
    # inside so run B starts clean. QD_UNDER_TEST/JAIL_QD_CMD -> Rust binary.
    (
        set -u
        . "$HERE/lib/jail.sh"
        . "$HERE/lib/normalize.sh"
        export QD_UNDER_TEST="$QD_BIN" JAIL_QD_CMD="$QD_BIN"
        jail_establish "rmk${tag}$$" || exit 3
        SCN_OUT="$JAIL_ROOT/scn-out.raw"
        # shellcheck source=/dev/null
        . "$HERE/scenarios/a5rec_kill.sh"
        scn_run
        rc=$?
        cp "$SCN_OUT" "$STAGE/kill.run$tag.raw" 2>/dev/null
        normalize_all "$JAIL_ROOT" "$JAIL_RUNID" "${JAIL_RELAY_PORT:-}" \
            < "$SCN_OUT" > "$STAGE/kill.run$tag.norm"
        jail_teardown
        exit "$rc"
    )
}

echo "[remint] run A"
one_run A || { echo "FATAL: run A failed"; exit 1; }
echo "[remint] run B"
one_run B || { echo "FATAL: run B failed"; exit 1; }

echo "[remint] double-run determinism diff (normalized):"
if ! diff "$STAGE/kill.runA.norm" "$STAGE/kill.runB.norm"; then
    echo "FATAL: double-run normalized outputs differ — NOT minting"; exit 1
fi
echo "[remint] identical. Installing fixture."

FIXDIR="$HERE/fixtures/a5-lifecycle"
cp "$STAGE/kill.runA.norm" "$FIXDIR/normalized/kill.txt"
cp "$STAGE/kill.runA.raw"  "$FIXDIR/raw/kill.txt.raw"
printf '0\n' > "$FIXDIR/raw/kill.txt.raw.exit"

echo "[remint] minted:"
cat "$FIXDIR/normalized/kill.txt"
