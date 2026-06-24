#!/usr/bin/env bash
# W31 seam-control live driver — produces the committed captures replayed by
# run_mutation_real.sh: (R3) STUB_NO_QUEUE-on busy row, (C2) STUB_RAW_STDIN-unset
# idle row. Mirrors r2_seam_drive.sh. Run in-jail at pin 8c59ec4. Bash 3.2 floor.
set -u
OUTDIR="$1"
# REPO-relative (this file lives at test/golden/mutation/); GOLDEN_TOP = test/golden.
GOLDEN_TOP="$(cd "$(dirname "$0")/.." && pwd)"
export TMPDIR="${TMPDIR:-/tmp}"
export SB_UNDER_TEST="${SB_UNDER_TEST:-bun /tmp/sb-rust-ts-prep/8c59ec456fe82780fd75d8afb5fe48dc72e10bc8/src/index.ts}"
cd "$GOLDEN_TOP"
_run_seam() {
  local seam_env="$1" scn="$2" out="$3" budget="${4:-30000}"
  bash -c '
    . lib/jail.sh; . lib/normalize.sh; . lib/check_python.sh; . lib/compare.sh
    . lib/stub_claude/stub_install.sh
    jail_establish; stub_install
    export JAIL_SB_CMD=sb JAIL_ZMX_CMD=zmx
    eval "export '"$seam_env"'"
    SCN_OUT=$JAIL_ROOT/out.raw
    . '"$scn"'
    SCN_BUDGET_MS='"$budget"'
    scn_run
    normalize_all $JAIL_ROOT $JAIL_RUNID $JAIL_RELAY_PORT < $SCN_OUT > "'"$out"'"
    jail_teardown
  ' 2>/dev/null
}
# R3: the busy row driven with STUB_NO_QUEUE=1 -> the busy-window burst is
# read-and-DISCARDED by the stub -> it never reaches the JSONL -> the strengthened
# busy outcome CHANGES (user_text[1]/anchor/reply for the burst missing, only turn1).
_run_seam "STUB_NO_QUEUE=1" scenarios/send_pty_paste_burst.sh "$OUTDIR/no_queue_burst.trace" 60000
# C2: the idle >=4KB row with the inline STUB_RAW_STDIN seam STRIPPED -> the stub
# PTY stays COOKED -> macOS MAX_CANON=1024 drops the >4KB write -> rc1/no-payload.
# The scenario hardcodes the seam inline, so we drive a seam-stripped COPY (this is
# exactly the "silent seam-loss" the C2 control must detect — a scenario that lost
# the inline seam would produce THIS cooked-drop capture, caught against gold).
COOKED_SCN="scenarios/.w31_cooked_idle_tmp.sh"
sed 's/STUB_RAW_STDIN=1 bash -c/bash -c/' scenarios/send_pty_chunked_idle.sh > "$COOKED_SCN"
_run_seam "_W31_NOOP=1" "$COOKED_SCN" "$OUTDIR/cooked_idle.trace" 90000
rm -f "$COOKED_SCN"
echo "=== no_queue_burst.trace ==="; cat "$OUTDIR/no_queue_burst.trace"
echo "=== cooked_idle.trace (field summary) ==="; grep -vE '^user_text|^anchored_on|^wait_reply_text' "$OUTDIR/cooked_idle.trace"
