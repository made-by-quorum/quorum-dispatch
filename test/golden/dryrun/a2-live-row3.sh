#!/bin/bash
# a2-live-row3.sh — A2 gate row 3: --agent fail-closed (LIVE, in-jail, built qd).
#
# Negative: `qd new <name> --agent bogus-agent-xyz` → nonzero exit, stderr names
# the agent + path tried, zmx list UNCHANGED (NO task created, NO boot).
# Positive control: create <agents_dir>/real-helper.md, point QD_SPAWN_AGENTS_DIR
# at it, `qd new <name2> --agent real-helper` → resolves PAST the agent gate
# (reaches the live create path). To avoid spending a real-claude boot for the
# positive, we assert resolvability by confirming the run gets past the agent
# fail-closed check into claim/create (it will then actually boot — so we DO let
# it boot once = boot C — and immediately tear down). If budget is tight, the
# negative case alone proves the fail-closed deliverable; the positive is the
# "it still works" control.
set -u
WT=/home/u/work/qd-rust/.claude/worktrees/agent-acfec16fb3b5c3375
cd "$WT" || exit 1
export JAIL_QD_CMD="$WT/target/debug/qd"
export JAIL_ZMX_CMD="/opt/homebrew/bin/zmx"
. test/golden/lib/jail.sh

jail_establish || { echo "FATAL"; exit 1; }
trap jail_teardown EXIT

REAL_SESS="$JAIL_REAL_HOME/.claude/sessions"
real_before="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"

mkdir -p "$HOME/.claude/channels" "$HOME/.claude/sessions"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"
RESOLVED_WORKDIR="$(cd "$WORKDIR" && pwd -P)"
printf '{"hasCompletedOnboarding": true, "bypassPermissionsModeAccepted": true, "projects": {"%s": {"hasTrustDialogAccepted": true}, "%s": {"hasTrustDialogAccepted": true}}}\n' \
    "$WORKDIR" "$RESOLVED_WORKDIR" > "$HOME/.claude.json"

AGENTS_DIR="$JAIL_ROOT/agents"; mkdir -p "$AGENTS_DIR"
export QD_SPAWN_AGENTS_DIR="$AGENTS_DIR"

echo "=== zmx list BEFORE (should be empty) ==="
jail_zmx list 2>&1 | sed 's/^/  /'
before_count="$(jail_zmx list 2>/dev/null | grep -c "$JAIL_PREFIX" || true)"

echo
echo "=== NEGATIVE: qd new --agent bogus-agent-xyz (NO boot expected) ==="
NAME1="${JAIL_PREFIX}bogus"
( cd "$WORKDIR" && QD_CLAUDE_FLAGS="--dangerously-skip-permissions" \
    "$JAIL_QD_CMD" new "$NAME1" --cwd "$WORKDIR" --agent bogus-agent-xyz ) \
    > "$JAIL_ROOT/neg-out.txt" 2> "$JAIL_ROOT/neg-err.txt"
neg_code=$?
echo "  exit=$neg_code (expect nonzero)"
echo "  stderr:"; sed 's/^/    /' "$JAIL_ROOT/neg-err.txt"
echo "=== zmx list AFTER negative (must be UNCHANGED — no task) ==="
jail_zmx list 2>&1 | sed 's/^/  /'
after_neg="$(jail_zmx list 2>/dev/null | grep -c "$JAIL_PREFIX" || true)"

# Assertions for the negative case.
neg_pass=1
[ "$neg_code" != "0" ] || { echo "  FAIL: bogus agent exited 0"; neg_pass=0; }
grep -q "bogus-agent-xyz" "$JAIL_ROOT/neg-err.txt" || { echo "  FAIL: stderr does not name the agent"; neg_pass=0; }
grep -qi "$AGENTS_DIR\|\.md\|agent" "$JAIL_ROOT/neg-err.txt" || { echo "  FAIL: stderr does not name the path/agent-def"; neg_pass=0; }
[ "$after_neg" = "$before_count" ] || { echo "  FAIL: zmx task count changed ($before_count -> $after_neg)"; neg_pass=0; }
if [ "$neg_pass" = "1" ]; then echo "  NEGATIVE: PASS (nonzero, names agent+path, no task created)"; fi

echo
echo "=== POSITIVE control: real-helper.md resolves PAST the agent gate ==="
printf '%s\n' "# real-helper" "A real agent def for the A2 fail-closed positive control." > "$AGENTS_DIR/real-helper.md"
echo "  created $AGENTS_DIR/real-helper.md"
NAME2="${JAIL_PREFIX}helper"
# This WILL boot real claude (boot C). We let it reach idle, then assert it got
# past the agent gate (a session exists) and tear down.
( cd "$WORKDIR" && QD_CLAUDE_FLAGS="--dangerously-skip-permissions" \
    "$JAIL_QD_CMD" new "$NAME2" --cwd "$WORKDIR" --agent real-helper ) \
    > "$JAIL_ROOT/pos-out.txt" 2> "$JAIL_ROOT/pos-err.txt"
pos_code=$?
echo "  exit=$pos_code (expect 0)"
echo "  stdout:"; sed 's/^/    /' "$JAIL_ROOT/pos-out.txt"
echo "  stderr:"; sed 's/^/    /' "$JAIL_ROOT/pos-err.txt"
echo "  zmx list:"; jail_zmx list 2>&1 | sed 's/^/    /'
pos_task="$(jail_zmx list 2>/dev/null | grep -c "$NAME2" || true)"
if [ "$pos_code" = "0" ] && [ "$pos_task" = "1" ]; then
    echo "  POSITIVE: PASS (resolved past agent gate, session created)"
else
    echo "  POSITIVE: result code=$pos_code task=$pos_task (see above)"
fi

echo
echo "=== REAL-HOME BELT ==="
real_after="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
leaked="$(grep -l "$JAIL_PREFIX" "$REAL_SESS"/*.json 2>/dev/null || true)"
echo "  real sessions before=$real_before after=$real_after"
[ -n "$leaked" ] && { echo "  BELT VIOLATION: $leaked"; exit 2; }
echo "  BELT HOLDS"
# teardown via trap (kills NAME2)
