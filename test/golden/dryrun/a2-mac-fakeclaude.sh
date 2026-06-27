#!/bin/bash
# macOS fake-claude bonus: real zmx + fake claude (zero real-claude boot) — an
# extra create/kill cycle hardening row 2 on macOS. NOT a real-claude boot.
set -u
WT=/home/u/work/qd-rust/.claude/worktrees/agent-acfec16fb3b5c3375
cd "$WT" || exit 1
export JAIL_SB_CMD="$WT/target/debug/qd"
export JAIL_ZMX_CMD="/opt/homebrew/bin/zmx"
. test/golden/lib/jail.sh
jail_establish || { echo FATAL; exit 1; }
trap jail_teardown EXIT
real_before="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"
FAKE="$JAIL_ROOT/fake-claude"
cat > "$FAKE" <<'EOS'
#!/bin/bash
name=""
while [ $# -gt 0 ]; do case "$1" in --name) name="$2"; shift 2;; *) shift;; esac; done
mkdir -p "$HOME/.claude/sessions"
printf '{"pid":%d,"name":"%s","status":"idle","sessionId":"fake-%d","cwd":"%s","version":"fake","kind":"interactive","entrypoint":"cli","startedAt":1700000000000,"updatedAt":1700000000000}\n' "$$" "$name" "$$" "$PWD" > "$HOME/.claude/sessions/$$.json"
exec sleep 120
EOS
chmod +x "$FAKE"
export CLAUDE_BIN="$FAKE"
mkdir -p "$HOME/.claude/sessions"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"
NAME="${JAIL_PREFIX}fake"
( cd "$WORKDIR" && SB_CLAUDE_FLAGS="--dangerously-skip-permissions" "$JAIL_SB_CMD" new "$NAME" --cwd "$WORKDIR" ) > "$JAIL_ROOT/o.txt" 2> "$JAIL_ROOT/e.txt"
code=$?
echo "create exit=$code : $(cat "$JAIL_ROOT/o.txt")"
[ -s "$JAIL_ROOT/e.txt" ] && { echo "stderr:"; sed 's/^/  /' "$JAIL_ROOT/e.txt"; }
task="$(jail_zmx list 2>/dev/null | grep -c "$NAME" || true)"
echo "zmx tasks=$task (expect 1)"
"$JAIL_ZMX_CMD" kill "$NAME" --force >/dev/null 2>&1 || "$JAIL_ZMX_CMD" kill "$NAME" >/dev/null 2>&1 || true
sleep 1
after="$(jail_zmx list 2>/dev/null | grep -c "$NAME" || true)"
echo "after kill tasks=$after (expect 0)"
real_after="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"
echo "real-home belt: $real_before -> $real_after"
[ "$code" = "0" ] && [ "$task" = "1" ] && [ "$after" = "0" ] && [ "$real_before" = "$real_after" ] && echo "MAC-FAKE-CLAUDE: PASS" || echo "MAC-FAKE-CLAUDE: CHECK"
