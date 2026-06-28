#!/usr/bin/env bash
# test/golden/mutation/r2_seam_drive.sh
# R2 seam-control live driver (Step 4 rider R2).
#
# PROVENANCE: this is the LIVE driver run in-VM (Lima sbtest, Linux aarch64) at
# pin 0d0fa9e to PRODUCE the committed captures under mutation/{r2-seams,divergent}/.
# It re-runs the recorded scenario(s) with the stub seam env / divergent target
# set, capturing the REAL recorder output (NOT a hand-edit). The path
# $HOME/sbrust-work is the VM staging dir; on brano the captures are replayed by
# run_mutation_real.sh, which does NOT need this driver (the captures are committed).
# Kept for reproducibility/audit. Bash 3.2 floor.
set -u
OUTDIR="$1"
cd "$HOME/sbrust-work/test/golden"
_run_seam() {
  local seam_env="$1" scn="$2" out="$3" budget="${4:-30000}"
  bash -c '
    . lib/jail.sh; . lib/normalize.sh; . lib/check_python.sh; . lib/compare.sh
    . lib/stub_claude/stub_install.sh
    jail_establish; stub_install
    export JAIL_QD_CMD=qd JAIL_ZMX_CMD=zmx
    eval "export '"$seam_env"'"
    SCN_OUT=$JAIL_ROOT/out.raw
    . '"$scn"'
    SCN_BUDGET_MS='"$budget"'
    scn_run
    normalize_all $JAIL_ROOT $JAIL_RUNID $JAIL_RELAY_PORT < $SCN_OUT > "'"$out"'"
    jail_teardown
  ' 2>/dev/null
}
_run_seam "STUB_WITHHOLD_PID=1"   scenarios/new_session_trace.sh     "$OUTDIR/withhold_pid_boot.trace"   20000
_run_seam "STUB_WITHHOLD_JSONL=1" scenarios/send_pty_paste_burst.sh  "$OUTDIR/withhold_jsonl_burst.trace" 60000
_run_seam "STUB_DEAD_HEALTH=1"    scenarios/relay_health.sh          "$OUTDIR/dead_health_contract.txt"  40000
echo "=== withhold_pid_boot.trace ==="; cat "$OUTDIR/withhold_pid_boot.trace"
echo "=== withhold_jsonl_burst.trace ==="; cat "$OUTDIR/withhold_jsonl_burst.trace"
echo "=== dead_health_contract.txt ==="; cat "$OUTDIR/dead_health_contract.txt"
