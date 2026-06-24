#!/usr/bin/env bash
# passb-diag-argv.sh — interpose a logging shim in front of the stub to capture
# the argv/env the Rust sb hands to `claude`, and watch zmx list during boot.
# DEV-TIME EVIDENCE / DRYRUN-NOT-ORACLE. (sbr-pa4-lead2 pass-b diagnosis)
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DIAG="$HERE/dryrun/passb-diag"
mkdir -p "$DIAG"
SUT="${SUT:-/home/u/work/wt-a4-passb/target/debug/sb}"

. "$HERE/lib/jail.sh"
. "$HERE/lib/stub_claude/stub_install.sh"

jail_establish || exit 3
trap 'jail_teardown 2>/dev/null || true' EXIT INT TERM
stub_install || exit 3
STUB_SHIM="$CLAUDE_BIN"

# Logging interposer (jail-rooted, passes the belt).
mkdir -p "$JAIL_ROOT/diag-bin"
LOG="$DIAG/stub-argv.txt"; : > "$LOG"
cat > "$JAIL_ROOT/diag-bin/claude" <<EOF
#!/usr/bin/env bash
{ echo "ARGV: \$0 \$*"; echo "PWD: \$(pwd)"; echo "STUB_BUSY_HOLD_MS=\${STUB_BUSY_HOLD_MS:-}"; } >> "$LOG"
exec "$STUB_SHIM" "\$@"
rc=\$?
echo "stub exited rc=\$rc" >> "$LOG"
exit \$rc
EOF
chmod +x "$JAIL_ROOT/diag-bin/claude"
export CLAUDE_BIN="$JAIL_ROOT/diag-bin/claude"

name="${JAIL_PREFIX}argv"
# Watch zmx list at 100ms while booting.
( i=0; while [ $i -lt 100 ]; do
    out="$(zmx list 2>/dev/null | grep -c "$name")"
    [ "$out" != "0" ] && { echo "t=${i}00ms session VISIBLE"; }
    i=$((i+1)); sleep 0.1
  done ) > "$DIAG/zmx-watch.txt" 2>&1 &
watchpid=$!

STUB_BUSY_HOLD_MS=6000 "$SUT" new "$name" > "$DIAG/argv-boot-stdout.txt" 2> "$DIAG/argv-boot-stderr.txt"
echo "sb new rc=$?"
sleep 2
kill "$watchpid" 2>/dev/null || true
echo "--- stub argv log ---"; cat "$LOG" 2>/dev/null || echo "(stub never invoked)"
echo "--- zmx watch ---"; sort -u "$DIAG/zmx-watch.txt" | head -5
echo "--- sb new stderr ---"; cat "$DIAG/argv-boot-stderr.txt"
echo "--- manual stub run (same argv, 3s) ---"
# If we captured argv, re-run the stub by hand to see if it survives.
exit 0
