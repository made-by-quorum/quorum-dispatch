#!/usr/bin/env bash
# scenario: a5rec reconcile — TS sb `reconcile --dry-run` shapes (READ-ONLY).
#
# ADD-12 (binding): destructive `sb reconcile` (no --dry-run) is OFF on macOS —
# TS legacyZmxDirs() defaults scanRoots to ["/tmp"] with the REAL uid
# (utils.ts:113 at pin 0d0fa9e), so a non-dry run would sweep the HOST's /tmp
# org sessions. We record DRY-RUN ONLY: the verb's dry-run guard blocks every
# kill/tombstone, so it is read-only and cannot reap a real session. Rows:
#   (a) clean: "Nothing to reconcile — all sources of truth agree."
#   (b) forged I1 dead-PID registry → "Would repair N drift item(s):" + per-item
#       "tombstone: <name> (pid <N> dead)".
# The NON-DRY `Repaired` line is LIMA-DEFERRED (see header below) — jail.sh::
# jail_sweep_belt_ok will NOT pass on brano, so it is recorded only in the Lima
# lane (G-X1). Pin 0d0fa9e. tooling: record.sh@388ccd9 normalize.sh@b581f75.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_a5_lifecycle_lib.sh"

SCN_NAME="a5rec-reconcile"
SCN_BUDGET_MS=20000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/a5-lifecycle/normalized/reconcile-dryrun.txt"

scn_run() {
    local DEADPID=4000001
    while kill -0 "$DEADPID" 2>/dev/null; do DEADPID=$((DEADPID+1)); done
    {
        echo "# RECORDED-FROM pin=0d0fa9e verb=reconcile mode=--dry-run (ADD-12: destructive OFF on macOS)"
        echo "# NON-DRY 'Repaired' line: LIMA-DEFERRED (G-X1) — TS sweeps literal /tmp; sweep-belt refuses on brano."
        echo "\$ sb reconcile --dry-run (clean — all agree)"
        scn_sb reconcile --dry-run 2>&1
    } > "$SCN_OUT"
    # Forge I1: a LIVE registry entry whose PID is dead (HOME-bounded forged file).
    mkdir -p "$HOME/.claude/sessions"
    printf '{"pid":%d,"name":"%sdeadreg","status":"idle","sessionId":"deadreg-rec","cwd":"%s","startedAt":1700000000000,"updatedAt":1700000000000}\n' \
        "$DEADPID" "$JAIL_PREFIX" "$JAIL_ROOT/tmp" > "$HOME/.claude/sessions/$DEADPID.json"
    {
        echo "\$ sb reconcile --dry-run (forged I1 dead-PID registry → Would repair)"
        scn_sb reconcile --dry-run 2>&1
    } >> "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q "Nothing to reconcile — all sources of truth agree." "$SCN_OUT" || return 1
    grep -q "Would repair 1 drift item(s):" "$SCN_OUT" || return 1
    grep -Eq "tombstone: ${JAIL_PREFIX}deadreg \(pid [0-9]+ dead\)" "$SCN_OUT" || return 1
}
