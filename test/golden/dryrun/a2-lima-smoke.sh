#!/bin/bash
# a2-lima-smoke.sh — Row 8 Linux live smoke (Lima linuxvm, aarch64).
#
# Exercises the FULL create path + real zmx + boot waiter with a FAKE claude
# (zero auth/cost): a tiny sh that writes a valid registry row into
# $HOME/.claude/sessions/$$.json then sleeps. In-VM jail (jail.sh). Records
# uname/arch + RSS of qd + zmx daemon.
#
# real-claude-on-Linux DEFERRED (auth/backend env = A4 scope) — recorded exclusion.
# Bash. Run INSIDE the VM. Args: $1 = repo dir, $2 = qd binary, $3 = target dir.
set -u
REPO="${1:-/home/u/work/qd-rust/.claude/worktrees/agent-acfec16fb3b5c3375}"
QD_BIN="${2:-/tmp/qd-vm-target/debug/qd}"
cd "$REPO" || exit 1

export JAIL_QD_CMD="$QD_BIN"
export JAIL_ZMX_CMD="$(command -v zmx)"
. test/golden/lib/jail.sh

echo "=== ENV ==="; uname -m; uname -s; hostname
echo "qd=$QD_BIN  zmx=$JAIL_ZMX_CMD"

jail_establish || { echo "FATAL: jail_establish"; exit 1; }
trap jail_teardown EXIT

# --- fake claude: write a valid registry row, then sleep -----------------------
FAKE="$JAIL_ROOT/fake-claude"
cat > "$FAKE" <<'EOS'
#!/bin/bash
# fake claude — parse --name, write a registry row to $HOME/.claude/sessions/$$.json
name=""
while [ $# -gt 0 ]; do
    case "$1" in
        --name) name="$2"; shift 2 ;;
        *) shift ;;
    esac
done
mkdir -p "$HOME/.claude/sessions"
printf '{"pid":%d,"name":"%s","status":"idle","sessionId":"fake-%d","cwd":"%s","version":"fake-0.0","kind":"interactive","entrypoint":"cli","startedAt":1700000000000,"updatedAt":1700000000000}\n' \
    "$$" "$name" "$$" "$PWD" > "$HOME/.claude/sessions/$$.json"
# Stay alive so zmx keeps the task and the wrapper PID is killable.
exec sleep 600
EOS
chmod +x "$FAKE"
export CLAUDE_BIN="$FAKE"

mkdir -p "$HOME/.claude/sessions"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"
NAME="${JAIL_PREFIX}lima"

echo
echo "=== CREATE: qd new (fake claude via CLAUDE_BIN) ==="
( cd "$WORKDIR" && QD_CLAUDE_FLAGS="--dangerously-skip-permissions" \
    "$JAIL_QD_CMD" new "$NAME" --cwd "$WORKDIR" ) \
    > "$JAIL_ROOT/lima-out.txt" 2> "$JAIL_ROOT/lima-err.txt"
code=$?
echo "  exit=$code"
echo "  stdout:"; sed 's/^/    /' "$JAIL_ROOT/lima-out.txt"
echo "  stderr:"; sed 's/^/    /' "$JAIL_ROOT/lima-err.txt"

echo "=== zmx list ==="; jail_zmx list 2>&1 | sed 's/^/  /'
task="$(jail_zmx list 2>/dev/null | grep -c "$NAME" || true)"
echo "=== PID file (jailed home) ==="
pf=""
for f in "$HOME/.claude/sessions"/*.json; do [ -f "$f" ] || continue; grep -q "$NAME" "$f" && { pf="$f"; break; }; done
[ -n "$pf" ] && { echo "  $pf:"; cat "$pf" | sed 's/^/    /'; echo; }

echo
echo "=== SEND (raw inject, no CR) ==="
jail_zmx send "$NAME" "LIMA_SMOKE_MARKER"
sleep 1
jail_zmx history "$NAME" 2>/dev/null | perl -pe 's/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\r//g' 2>/dev/null | grep -i "LIMA_SMOKE_MARKER" | head -3 | sed 's/^/    /' \
    || jail_zmx history "$NAME" 2>/dev/null | tail -5 | sed 's/^/    /'

echo
echo "=== RSS (qd + zmx daemon) ==="
echo "  process RSS (KB):"
ps -eo pid,rss,comm 2>/dev/null | grep -iE "zmx|qd|fake-claude|sleep" | grep -v grep | sed 's/^/    /' | head -10

echo
echo "=== KILL (in-jail, real zmx) ==="
wrapper_pid="$(jail_zmx list 2>/dev/null | grep "$NAME" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
echo "  wrapper pid before kill: ${wrapper_pid:-unknown}"
"$JAIL_ZMX_CMD" kill "$NAME" --force >/dev/null 2>&1 || "$JAIL_ZMX_CMD" kill "$NAME" >/dev/null 2>&1 || true
sleep 1
after="$(jail_zmx list 2>/dev/null | grep -c "$NAME" || true)"
echo "  zmx list after kill:"; jail_zmx list 2>&1 | sed 's/^/    /'
if [ -n "$wrapper_pid" ]; then
    if kill -0 "$wrapper_pid" 2>/dev/null; then echo "  wrapper PID $wrapper_pid STILL ALIVE"; else echo "  wrapper PID $wrapper_pid is DEAD"; fi
fi

echo
echo "=== VERDICT ==="
echo "  create_exit=$code task=$task pidfile=${pf:+present} kill_after=$after"
if [ "$code" = "0" ] && [ "$task" = "1" ] && [ -n "$pf" ] && [ "$after" = "0" ]; then
    echo "  ROW 8 (Lima fake-claude smoke): PASS"
else
    echo "  ROW 8: CHECK (see above)"
fi
echo "  NOTE: real-claude-on-Linux DEFERRED (auth/backend env = A4 scope) — recorded exclusion."
# teardown via trap
