#!/usr/bin/env bash
# passb-diag-boot.sh — manual jailed boot of the Rust sb to capture the boot
# error the paste-burst scenario discards. DEV-TIME EVIDENCE / DRYRUN-NOT-ORACLE.
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DIAG="$HERE/dryrun/passb-diag"
mkdir -p "$DIAG"
SUT="${SUT:-/home/u/work/wt-a4-passb/target/debug/sb}"

. "$HERE/lib/jail.sh"
. "$HERE/lib/stub_claude/stub_install.sh"

jail_establish || { echo "JAIL REFUSED" >&2; exit 3; }
trap 'jail_teardown 2>/dev/null || true' EXIT INT TERM
stub_install || { echo "STUB INSTALL FAILED" >&2; exit 3; }

name="${JAIL_PREFIX}diagboot"
echo "=== env relevant bits ==="
echo "HOME=$HOME"; echo "SB_HOME=${SB_HOME:-}"; echo "ZMX_DIR=${ZMX_DIR:-}"
echo "CLAUDE_BIN=${CLAUDE_BIN:-<unset>}"; command -v claude || echo "no claude on PATH"
echo "=== sb new (full stderr/stdout) ==="
STUB_BUSY_HOLD_MS=6000 "$SUT" new "$name" > "$DIAG/boot-stdout.txt" 2> "$DIAG/boot-stderr.txt" &
bootpid=$!
sleep 8
echo "--- stdout ---"; cat "$DIAG/boot-stdout.txt"
echo "--- stderr ---"; cat "$DIAG/boot-stderr.txt"
echo "--- session records ---"; ls "$HOME/.claude/sessions/" 2>/dev/null || echo none
echo "--- zmx list (jailed) ---"; zmx list 2>&1 || true
echo "--- jailed sb ls ---"; "$SUT" ls 2>&1 | head -10
kill "$bootpid" 2>/dev/null || true
exit 0
