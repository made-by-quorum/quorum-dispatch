#!/usr/bin/env bash
# test/golden/mutation/divergent_invocation_drive.sh
# intentionally-divergent TS invocation driver (Step 4, red-team m1).
#
# PROVENANCE: this is the LIVE driver run in-VM (Lima sbtest, Linux aarch64) at
# pin 0d0fa9e to PRODUCE the committed captures under mutation/{r2-seams,divergent}/.
# It re-runs the recorded scenario(s) with the stub seam env / divergent target
# set, capturing the REAL recorder output (NOT a hand-edit). The path
# $HOME/sbrust-work is the VM staging dir; on brano the captures are replayed by
# run_mutation_real.sh, which does NOT need this driver (the captures are committed).
# Kept for reproducibility/audit. Bash 3.2 floor.
set -u
OUTDIR="$1"   # pre-jail save dir (captured before HOME is jailed)
cd "$HOME/sbrust-work/test/golden"
. lib/jail.sh; . lib/normalize.sh; . lib/check_python.sh; . lib/compare.sh
. lib/stub_claude/stub_install.sh
jail_establish; stub_install
export JAIL_SB_CMD=qd JAIL_ZMX_CMD=zmx
. scenarios/_scenario_lib.sh
name="$(scn_session_name divx)"
python3 recorder/record_pty.py --out "$JAIL_ROOT/boot.raw" --secs 25 -- \
  env QD_UNDER_TEST="$QD_UNDER_TEST" CLAUDE_BIN="$CLAUDE_BIN" \
  sh -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1
sleep 2
out="$($QD_UNDER_TEST info "$name" 2>&1)"; rc=$?
out2="$($QD_UNDER_TEST config nosuchsub 2>&1)"; rc2=$?
{
  printf 'cmd=info-missing-session expect_exit=1 got_exit=%s\n' "$rc"
  printf 'stderr_present=%s\n' "$( [ -n "$out" ] && echo 1 || echo 0 )"
  printf 'cmd=config-unknown-subcommand expect_exit=2 got_exit=%s\n' "$rc2"
  printf 'stderr_present=%s\n' "$( [ -n "$out2" ] && echo 1 || echo 0 )"
} > "$JAIL_ROOT/div.trace"
normalize_all "$JAIL_ROOT" "$JAIL_RUNID" "$JAIL_RELAY_PORT" < "$JAIL_ROOT/div.trace" > "$OUTDIR/divergent_exit.trace"
printf '=== saved divergent trace ===\n'; cat "$OUTDIR/divergent_exit.trace"
jail_kill_session "$name" >/dev/null 2>&1 || true
jail_teardown
