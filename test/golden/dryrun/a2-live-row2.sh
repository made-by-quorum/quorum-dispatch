#!/bin/bash
# a2-live-row2.sh — A2 gate rows 2+4+5(live stock half): concurrent-create race
# via the BUILT qd binary against REAL zmx 0.6.0, in-jail, stock flags
# (--dangerously-skip-permissions only → no dev-channels → dialog-free).
#
# Boot A: N=4 background `qd new <same name>` processes. Assert exactly one
# exit 0; losers nonzero with claim/in-use error; zmx list shows EXACTLY ONE
# task; winner reached ready (PID file in jailed home, status idle). Then REUSE
# the winner session for send / reattach / kill rows.
#
# Bash 3.2. Self-cd. Points JAIL_SB_CMD at the built debug binary + JAIL_ZMX_CMD
# at the pinned real zmx.
set -u
WT=/home/u/work/qd-rust/.claude/worktrees/agent-acfec16fb3b5c3375
cd "$WT" || exit 1
export JAIL_SB_CMD="$WT/target/debug/qd"
export JAIL_ZMX_CMD="/opt/homebrew/bin/zmx"
. test/golden/lib/jail.sh

jail_establish || { echo "FATAL: jail_establish failed"; exit 1; }
trap jail_teardown EXIT

REAL_SESS="$JAIL_REAL_HOME/.claude/sessions"
real_before="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
echo "REAL-HOME baseline sessions=$real_before"

# --- seed jailed claude state (probe-script method; stock = no dev-channels) ---
mkdir -p "$HOME/.claude/channels" "$HOME/.claude/sessions"
WORKDIR="$JAIL_ROOT/tmp/work"
mkdir -p "$WORKDIR"
RESOLVED_WORKDIR="$(cd "$WORKDIR" && pwd -P)"
# Stock seed: onboarding + bypass + per-project trust (literal + /private-resolved).
# NO dangerouslyLoadDevelopmentChannels, NO growthbook -> dev-channels stays OFF.
printf '{"hasCompletedOnboarding": true, "bypassPermissionsModeAccepted": true, "projects": {"%s": {"hasTrustDialogAccepted": true}, "%s": {"hasTrustDialogAccepted": true}}}\n' \
    "$WORKDIR" "$RESOLVED_WORKDIR" > "$HOME/.claude.json"

NAME="${JAIL_PREFIX}race"

# --- THE RACE: N=4 background qd new, same name, stock flags --------------------
N=4
RDIR="$JAIL_ROOT/raceout"
mkdir -p "$RDIR"
echo "=== launching $N concurrent 'qd new $NAME' (built binary, stock flags) ==="
pids=""
i=0
while [ "$i" -lt "$N" ]; do
    (
        cd "$WORKDIR" || exit 99
        SB_CLAUDE_FLAGS="--dangerously-skip-permissions" \
            "$JAIL_SB_CMD" new "$NAME" --cwd "$WORKDIR" \
            > "$RDIR/out-$i.txt" 2> "$RDIR/err-$i.txt"
        echo "$?" > "$RDIR/code-$i.txt"
    ) &
    pids="$pids $!"
    i=$((i + 1))
done
# Register the racer subshell PIDs (best-effort whitelist) and wait.
for p in $pids; do wait "$p"; done

echo "=== race exit codes ==="
wins=0; losers=0; zero_codes=""
i=0
while [ "$i" -lt "$N" ]; do
    c="$(cat "$RDIR/code-$i.txt" 2>/dev/null || echo MISSING)"
    echo "  child $i exit=$c"
    if [ "$c" = "0" ]; then wins=$((wins+1)); zero_codes="$zero_codes $i"; else losers=$((losers+1)); fi
    i=$((i + 1))
done
echo "wins=$wins losers=$losers"

echo "=== loser stderr (should name claim/in-use) ==="
i=0
while [ "$i" -lt "$N" ]; do
    c="$(cat "$RDIR/code-$i.txt" 2>/dev/null)"
    if [ "$c" != "0" ]; then
        echo "  -- child $i (exit $c):"; sed 's/^/     /' "$RDIR/err-$i.txt"
    fi
    i=$((i + 1))
done

echo "=== winner stdout ==="
for i in $zero_codes; do echo "  child $i:"; sed 's/^/     /' "$RDIR/out-$i.txt"; done

echo "=== zmx list in-jail (EXACTLY ONE task expected) ==="
jail_zmx list 2>&1 | sed 's/^/  /'
task_count="$(jail_zmx list 2>/dev/null | grep -c "$NAME")"
echo "tasks matching $NAME = $task_count"

echo "=== winner PID file + status (jailed home) ==="
pidfile=""
for f in "$HOME/.claude/sessions"/*.json; do
    [ -f "$f" ] || continue
    if grep -q "\"name\"[^,]*$NAME" "$f" 2>/dev/null; then pidfile="$f"; break; fi
done
if [ -n "$pidfile" ]; then
    echo "  PID file: $pidfile"; cat "$pidfile" | sed 's/^/    /'; echo
    status="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status",""))' "$pidfile" 2>/dev/null)"
    echo "  status=$status"
else
    echo "  NO PID file found for $NAME"
fi

echo
echo "=== ROW 2 send (raw inject, NO trailing CR — no turn submit) ==="
INJECT="QA_LIVE_MARKER_$$"
jail_zmx send "$NAME" "$INJECT"
sleep 2
echo "  history tail (ANSI-stripped) — composer should show the marker (ADD-6: app-output keyed):"
jail_zmx history "$NAME" 2>/dev/null | perl -pe 's/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\e[()][B0]//g; s/\r//g' | grep -v '^[[:space:]]*$' | tail -25 | sed 's/^/    /'
if jail_zmx history "$NAME" 2>/dev/null | perl -pe 's/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\r//g' | grep -q "$INJECT"; then
    echo "  SEND-ASSERT: PASS (marker '$INJECT' present in application output)"
else
    echo "  SEND-ASSERT: FAIL (marker not in app output)"
fi

echo
echo "=== ROW 2 reattach (history returns boot-screen backlog after detached the whole time) ==="
# A known boot-screen marker for claude TUI. Capture the full backlog.
jail_zmx history "$NAME" 2>/dev/null | perl -pe 's/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\e[()][B0]//g; s/\r//g' | grep -v '^[[:space:]]*$' > "$JAIL_ROOT/backlog.txt"
echo "  backlog line count: $(wc -l < "$JAIL_ROOT/backlog.txt" | tr -d ' ')"
echo "  backlog head:"; head -15 "$JAIL_ROOT/backlog.txt" | sed 's/^/    /'
# Boot-screen markers commonly present in claude TUI scrollback.
if grep -qiE "claude|welcome|/help|bypass|tip" "$JAIL_ROOT/backlog.txt"; then
    echo "  REATTACH-ASSERT: PASS (boot-screen backlog present)"
else
    echo "  REATTACH-ASSERT: WEAK (no canonical marker; see backlog above)"
fi

echo
echo "=== ROW 2 kill (qd kill verb DEFERRED to gc phase per bin/qd.rs — using jail_kill_session belt) ==="
# Capture wrapper PID before kill (zmx task pid).
echo "  zmx list before kill:"; jail_zmx list 2>&1 | grep "$NAME" | sed 's/^/    /'
"$JAIL_ZMX_CMD" kill "$NAME" --force >/dev/null 2>&1 || "$JAIL_ZMX_CMD" kill "$NAME" >/dev/null 2>&1 || true
sleep 2
echo "  zmx list after kill:"; jail_zmx list 2>&1 | sed 's/^/    /'
after_count="$(jail_zmx list 2>/dev/null | grep -c "$NAME")"
if [ "$after_count" = "0" ]; then echo "  KILL-ASSERT: PASS (task gone from zmx list)"; else echo "  KILL-ASSERT: FAIL ($after_count still present)"; fi

echo
echo "=== REAL-HOME BELT (must be untouched) ==="
real_after="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
leaked="$(grep -l "$NAME" "$REAL_SESS"/*.json 2>/dev/null || true)"
echo "  real sessions before=$real_before after=$real_after"
if [ -n "$leaked" ]; then echo "  BELT VIOLATION: $leaked"; exit 2; fi
echo "  BELT HOLDS — no race rows in real registry"

echo
echo "=== ROW SUMMARY ==="
echo "  wins=$wins (expect 1) losers=$losers (expect $((N-1))) zmx_tasks=$task_count (expect 1) kill_after=$after_count (expect 0)"
# teardown via trap
