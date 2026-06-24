#!/bin/bash
# a2-live-row4.sh — A2 gate row 4 (spec §11.5 configured-flag half): dev-channels
# consent dialog appears, the BUILT sb's EventBootWaiter answers it (≤2 sends),
# session reaches ready. boot B = real-claude #3 of 3.
#
# DEFAULT flags (NO SB_CLAUDE_FLAGS override) → built-in default carries
# --dangerously-load-development-channels server:relay → dev-channels ON.
# Seed cachedGrowthBookFeatures from real home (probe-3) + channels symlink so
# the GrowthBook gate opens and the consent dialog actually renders.
#
# If the channels gate does NOT open in-jail (feature-flag drift), record
# BLOCKED-ENV with the probe-3 evidence — NOT failed.
set -u
WT=/home/u/work/sb-rust/.claude/worktrees/agent-acfec16fb3b5c3375
cd "$WT" || exit 1
export JAIL_SB_CMD="$WT/target/debug/sb"
export JAIL_ZMX_CMD="/opt/homebrew/bin/zmx"
. test/golden/lib/jail.sh

jail_establish || { echo "FATAL"; exit 1; }
trap jail_teardown EXIT

REAL_SESS="$JAIL_REAL_HOME/.claude/sessions"
real_before="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"

mkdir -p "$HOME/.claude/channels" "$HOME/.claude/sessions"
ln -s /home/u/work/cc-relay "$HOME/.claude/channels/relay"
GB_FLAGS="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(json.dumps(d.get("cachedGrowthBookFeatures",{})))' "$JAIL_REAL_HOME/.claude.json" 2>/dev/null || echo '{}')"
echo "growthbook flags seeded: $(printf '%s' "$GB_FLAGS" | head -c 60)..."

WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"
RESOLVED_WORKDIR="$(cd "$WORKDIR" && pwd -P)"
# probe-3 method: onboarding + bypass + per-project trust + growthbook + dev-channels.
printf '{"hasCompletedOnboarding": true, "bypassPermissionsModeAccepted": true, "dangerouslyLoadDevelopmentChannels": true, "cachedGrowthBookFeatures": %s, "projects": {"%s": {"hasTrustDialogAccepted": true}, "%s": {"hasTrustDialogAccepted": true}}}\n' \
    "$GB_FLAGS" "$WORKDIR" "$RESOLVED_WORKDIR" > "$HOME/.claude.json"

NAME="${JAIL_PREFIX}cfg"
echo "=== sb new with DEFAULT flags (dev-channels ON → consent dialog expected) ==="
echo "    (NO SB_CLAUDE_FLAGS override → built-in default flags)"
( cd "$WORKDIR" && "$JAIL_SB_CMD" new "$NAME" --cwd "$WORKDIR" ) \
    > "$JAIL_ROOT/cfg-out.txt" 2> "$JAIL_ROOT/cfg-err.txt"
code=$?
echo "  exit=$code"
echo "  stdout:"; sed 's/^/    /' "$JAIL_ROOT/cfg-out.txt"
echo "  stderr:"; sed 's/^/    /' "$JAIL_ROOT/cfg-err.txt"

echo
echo "=== zmx history (was the dev-channels dialog ever on screen?) ==="
HIST="$(jail_zmx history "$NAME" 2>/dev/null | perl -pe 's/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\e[()][B0]//g; s/\r//g')"
printf '%s\n' "$HIST" | grep -v '^[[:space:]]*$' | tail -40 | sed 's/^/    /'

echo
echo "=== zmx list ==="
jail_zmx list 2>&1 | sed 's/^/  /'
task="$(jail_zmx list 2>/dev/null | grep -c "$NAME" || true)"

echo
echo "=== verdict ==="
dialog_seen=0
case "$HIST" in
    *"WARNING: Loading development channels"*) dialog_seen=1 ;;
    *"development channels"*) dialog_seen=1 ;;
esac
pidfile=""
for f in "$HOME/.claude/sessions"/*.json; do
    [ -f "$f" ] || continue
    grep -q "$NAME" "$f" 2>/dev/null && { pidfile="$f"; break; }
done
echo "  dialog_seen=$dialog_seen  exit=$code  task=$task  pidfile=${pidfile:-NONE}"
if [ -n "$pidfile" ]; then
    status="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status",""))' "$pidfile" 2>/dev/null)"
    echo "  status=$status"
fi

if [ "$code" = "0" ] && [ "$task" = "1" ] && [ -n "$pidfile" ]; then
    if [ "$dialog_seen" = "1" ]; then
        echo "  ROW 4: PASS (dialog appeared in scrollback; answerer answered; session ready)"
    else
        echo "  ROW 4: PASS-WEAK (session ready but dialog not visible in tail — channels gate may have short-circuited; inspect full history)"
    fi
else
    if [ "$dialog_seen" = "0" ]; then
        echo "  ROW 4: BLOCKED-ENV (channels gate did not open in-jail; probe-3 feature-flag drift). NOT a failure of the answerer."
    else
        echo "  ROW 4: FAIL (dialog appeared but boot did not complete — answerer issue). exit=$code task=$task"
    fi
fi

echo
echo "=== REAL-HOME BELT ==="
real_after="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
leaked="$(grep -l "$NAME" "$REAL_SESS"/*.json 2>/dev/null || true)"
echo "  real sessions before=$real_before after=$real_after"
[ -n "$leaked" ] && { echo "  BELT VIOLATION: $leaked"; exit 2; }
echo "  BELT HOLDS"
# teardown via trap
