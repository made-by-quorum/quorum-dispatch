#!/usr/bin/env bash
# passb-diag-zmx.sh — drive `zmx run` by hand inside a jail to isolate whether
# the failure is zmx-level or sb-level. DEV-TIME EVIDENCE / DRYRUN-NOT-ORACLE.
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DIAG="$HERE/dryrun/passb-diag"
mkdir -p "$DIAG"

. "$HERE/lib/jail.sh"
. "$HERE/lib/stub_claude/stub_install.sh"

jail_establish || exit 3
trap 'jail_teardown 2>/dev/null || true' EXIT INT TERM
stub_install || exit 3

echo "zmx: $(command -v zmx) version: $(zmx --version 2>&1 | head -1)"
echo "ZMX_DIR=$ZMX_DIR"
name="${JAIL_PREFIX}manual"

echo "--- zmx run (simple sleep cmd) ---"
zmx run "$name" -d bash -lc 'sleep 30' ; echo "zmx run rc=$?"
sleep 1
echo "--- zmx list after 1s ---"; zmx list 2>&1
echo "--- socket dir contents ---"; ls -la "$ZMX_DIR" 2>/dev/null
zmx kill "$name" 2>/dev/null || true

echo "--- zmx run (stub claude) ---"
name2="${JAIL_PREFIX}manual2"
zmx run "$name2" -d bash -lc "command '$CLAUDE_BIN'" ; echo "zmx run rc=$?"
sleep 2
echo "--- zmx list after 2s ---"; zmx list 2>&1
zmx kill "$name2" 2>/dev/null || true
exit 0
