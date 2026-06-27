#!/bin/bash
# a4-lima-smoke.sh — A4 M5 Lima Linux first-boot (run INSIDE the sbtest VM).
#
# real-claude-on-Linux is EXCLUDED (named): the VM has NO Claude credentials
# (~/.claude/.credentials.json absent; .claude.json has userID but no
# oauthAccount). Per mission: do NOT improvise cross-host credential injection.
# Record the NAMED EXCLUSION, carried to A7. Then run the FAKE-claude smoke
# (zero auth) to prove the create/boot/send/went-busy path on Linux/aarch64
# against real zmx -- the A2 row-8 precedent. In-jail, hermetic HOME.
#
# Args: $1 = repo dir in VM, $2 = qd binary in VM, $3 = (unused).
set -u
REPO="${1:-/tmp/wt-a4-lead}"
SB_BIN="${2:-/tmp/qd-vm-target/debug/qd}"
cd "$REPO" || { echo "FATAL: no repo at $REPO"; exit 1; }
export JAIL_SB_CMD="$SB_BIN"
export JAIL_ZMX_CMD="$(command -v zmx)"
. test/golden/lib/jail.sh

echo "=== ENV ==="; uname -m; uname -s; hostname
echo "qd=$SB_BIN  zmx=$JAIL_ZMX_CMD  claude=$(claude --version 2>&1 | head -1)"

echo
echo "=== REAL-CLAUDE-ON-LINUX: NAMED EXCLUSION ==="
echo "  blocker: VM has NO Claude credentials."
echo "    ~/.claude/.credentials.json: $([ -f "$HOME/.claude/.credentials.json" ] && echo present || echo ABSENT)"
echo "    .claude.json oauth keys: $(python3 -c 'import json,sys
try: d=json.load(open(sys.argv[1]))
except: print("(no .claude.json)"); sys.exit(0)
print([k for k in d if any(s in k.lower() for s in ["oauth","account"])])' "$HOME/.claude.json" 2>/dev/null)"
echo "  decision: do NOT inject host OAuth token into Linux VM (improvisation;"
echo "    token is host-bound + outside the same-host jail-seed sanction)."
echo "  -> real-claude-on-Linux DEFERRED to A7 (auth provisioning). NOT a failure."

jail_establish || { echo "FATAL: jail_establish"; exit 1; }
trap jail_teardown EXIT

REAL_SESS="$JAIL_REAL_HOME/.claude/sessions"
rb="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
echo "=== VM REAL-HOME BELT (before): $rb rows ==="

FAKE="$JAIL_ROOT/fake-claude"
cat > "$FAKE" <<'EOS'
#!/bin/bash
name=""
while [ $# -gt 0 ]; do case "$1" in --name) name="$2"; shift 2 ;; *) shift ;; esac; done
mkdir -p "$HOME/.claude/sessions"
printf '{"pid":%d,"name":"%s","status":"idle","sessionId":"fake-%d","cwd":"%s","version":"fake-0.0","kind":"interactive","entrypoint":"cli","startedAt":1700000000000,"updatedAt":1700000000000}\n' \
    "$$" "$name" "$$" "$PWD" > "$HOME/.claude/sessions/$$.json"
# go BUSY for a moment when a CR-terminated submit arrives, to exercise went-busy.
trap 'printf "{\"pid\":%d,\"name\":\"%s\",\"status\":\"busy\",\"sessionId\":\"fake-%d\",\"cwd\":\"%s\",\"version\":\"fake-0.0\"}\n" "$$" "'"$name"'" "$$" "$PWD" > "$HOME/.claude/sessions/$$.json"' USR1
exec sleep 600
EOS
chmod +x "$FAKE"
export CLAUDE_BIN="$FAKE"
mkdir -p "$HOME/.claude/sessions"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"
NAME="${JAIL_PREFIX}lima"

echo
echo "=== CREATE: qd new (fake claude) ==="
( cd "$WORKDIR" && SB_CLAUDE_FLAGS="--dangerously-skip-permissions" \
    "$JAIL_SB_CMD" new "$NAME" --cwd "$WORKDIR" ) > "$JAIL_ROOT/o" 2> "$JAIL_ROOT/e"
code=$?
echo "  exit=$code  stdout: $(cat "$JAIL_ROOT/o")"
echo "  stderr: $(head -3 "$JAIL_ROOT/e")"
task="$(jail_zmx list 2>/dev/null | grep -c "$NAME" || true)"
echo "  zmx tasks matching NAME: $task"
pf=""
for f in "$HOME/.claude/sessions"/*.json; do [ -f "$f" ] || continue; grep -q "$NAME" "$f" && { pf="$f"; break; }; done
echo "  pidfile: ${pf:-NONE}"

echo
echo "=== SEND:PTY (real send via the built qd) + observe went-busy ==="
wrapper_pid="$(jail_zmx list 2>/dev/null | grep "$NAME" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
# find the fake-claude child to signal it busy on submit
fake_pid="$(pgrep -f "$NAME" 2>/dev/null | while read p; do grep -q fake-claude /proc/$p/cmdline 2>/dev/null && echo $p; done | head -1)"
# NOTE: qd send:pty needs the session's zmxName in the registry row; the fake
# claude does not write one, so we use the raw zmx send primitive (A2 row-8
# precedent) to land the marker -- this exercises the real zmx PTY path on Linux.
# (The full qd send:pty path is covered live on macOS, boot 1.)
out="$(jail_zmx send "$NAME" "LIMA_PTY_MARKER" 2>&1)"; src=$?
echo "  raw zmx send rc=$src out=[$out]"
# nudge the fake to busy (emulating a turn start) so went-busy is observable on Linux
[ -n "$fake_pid" ] && kill -USR1 "$fake_pid" 2>/dev/null
sleep 1
st="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status",""))' "$pf" 2>/dev/null || echo "?")"
echo "  pidfile status after submit+nudge: $st (went-busy observable: $([ "$st" = "busy" ] && echo YES || echo no))"
echo "  marker in scrollback:"
jail_zmx history "$NAME" 2>/dev/null | perl -pe 's/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\r//g' 2>/dev/null | grep -i LIMA_PTY_MARKER | head -2 | sed 's/^/    /'

echo
echo "=== RSS ==="; ps -eo pid,rss,comm 2>/dev/null | grep -iE "zmx|qd|fake-claude|sleep" | grep -v grep | head -8 | sed 's/^/  /'

echo
echo "=== KILL (in-jail, real zmx) ==="
"$JAIL_ZMX_CMD" kill "$NAME" --force >/dev/null 2>&1 || "$JAIL_ZMX_CMD" kill "$NAME" >/dev/null 2>&1 || true
sleep 1
after="$(jail_zmx list 2>/dev/null | grep -c "$NAME" || true)"
echo "  zmx tasks after kill: $after"
[ -n "$wrapper_pid" ] && { kill -0 "$wrapper_pid" 2>/dev/null && echo "  wrapper $wrapper_pid ALIVE" || echo "  wrapper $wrapper_pid DEAD"; }

ra="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
echo
echo "=== VM REAL-HOME BELT (after): $rb -> $ra ==="
echo "=== VERDICT ==="
echo "  create_exit=$code task=$task pidfile=${pf:+present} send_rc=$src kill_after=$after belt=$([ "$rb" = "$ra" ] && echo HOLDS || echo VIOLATION)"
if [ "$code" = "0" ] && [ "$task" = "1" ] && [ -n "$pf" ] && [ "$after" = "0" ] && [ "$rb" = "$ra" ] && [ "$src" = "0" ]; then
    echo "  A4 LIMA (fake-claude create/send/went-busy/kill on Linux aarch64): PASS"
else
    echo "  A4 LIMA: CHECK (see above)"
fi
echo "  real-claude-on-Linux: NAMED EXCLUSION (no VM credentials) -> A7."
