#!/usr/bin/env bash
# passb-diag-row.sh <scenario-basename> — run one scenario the way verify.sh
# does (jail + stub + scn_run + scn_assert) but snapshot the FULL jail state
# before teardown, for red-row byte capture. DEV-TIME EVIDENCE / DRYRUN-NOT-ORACLE.
# (sbr-pa4-lead2 pass-b closure diagnosis; no fixture edits, no harness edits.)
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
SCN_BASE="${1:?usage: passb-diag-row.sh <scenario-basename>}"
DIAG="$HERE/dryrun/passb-diag/$SCN_BASE"
mkdir -p "$DIAG"
SUT="${SUT:-/home/u/work/wt-a4-passb/target/debug/qd}"
export QD_UNDER_TEST="$SUT"

. "$HERE/lib/jail.sh"
. "$HERE/lib/normalize.sh"
. "$HERE/lib/compare.sh"
. "$HERE/lib/check_python.sh"
. "$HERE/lib/stub_claude/stub_install.sh"

jail_establish || exit 3
trap 'jail_teardown 2>/dev/null || true' EXIT INT TERM

SCN_OUT="$JAIL_ROOT/scn-out.raw"
SCN_NORM="$JAIL_ROOT/scn-out.norm"
SCN_NAME=""; SCN_BUDGET_MS=""; SCN_CLASS=""; SCN_FIXTURE=""; SCN_STUB_BACKED=""
. "$HERE/scenarios/$SCN_BASE.sh"

if [ "${SCN_STUB_BACKED:-}" = "1" ]; then
    stub_install || exit 3
fi

scn_run
run_rc=$?
echo "scn_run rc=$run_rc"

# Snapshot the jail BEFORE assert/teardown.
cp "$SCN_OUT" "$DIAG/scn-out.raw" 2>/dev/null || echo "(no scn-out.raw)"
cp "$SCN_OUT.exit" "$DIAG/scn-out.exit" 2>/dev/null || true
mkdir -p "$DIAG/home-claude"
cp -R "$HOME/.claude/sessions" "$DIAG/home-claude/sessions" 2>/dev/null || echo "(no sessions dir)"
cp -R "$HOME/.claude/projects" "$DIAG/home-claude/projects" 2>/dev/null || echo "(no projects dir)"
zmx list > "$DIAG/zmx-list.txt" 2>&1 || true
ls -laR "$JAIL_ROOT" > "$DIAG/jailroot-find.txt" 2>&1 || true

if scn_assert; then echo "ASSERT: PASS"; else echo "ASSERT: DIFF"; fi
echo "--- scn-out.raw ---"; cat "$DIAG/scn-out.raw" 2>/dev/null
echo "--- sessions dir ---"; ls -la "$DIAG/home-claude/sessions" 2>/dev/null
exit 0
