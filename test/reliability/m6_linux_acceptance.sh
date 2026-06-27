#!/usr/bin/env bash
# M6 Fix A: 3 DISTINCT registry-layer sessions on Linux via the unique-ID rl stub.
set -u
export TMPDIR=/tmp
SB="$HOME/a7-tree/target/debug/qd"
RLSTUB="$HOME/a7-tree/test/reliability/stub_claude_rl.sh"
cd "$HOME/a7-tree/test/golden"; . lib/jail.sh
jail_establish m6v3 || exit 1
trap 'jail_teardown' EXIT
export CLAUDE_BIN="$RLSTUB"
echo "=== M6v3 Linux ($(uname -m)): unique-ID rl stub, REGISTRY-LAYER proof ==="
PASS=0; FAIL=0
ck(){ if eval "$2"; then echo "  PASS $1"; PASS=$((PASS+1)); else echo "  FAIL $1"; FAIL=$((FAIL+1)); fi; }
declare -a NAMES
for i in 1 2 3; do
  n="${JAIL_PREFIX}s$i"; NAMES+=("$n")
  "$SB" new "$n" >/dev/null 2>&1; echo "  new $n rc=$?"
done
echo "--- registry entries (distinct sessionIds): ---"
ls "$HOME/.claude/sessions/"*.json 2>/dev/null | while read f; do python3 -c "import json,sys; d=json.load(open('$f')); print('  ', d['name'], d['sessionId'][:16])"; done
NDISTINCT=$(for f in "$HOME"/.claude/sessions/*.json; do python3 -c "import json;print(json.load(open('$f'))['sessionId'])" 2>/dev/null; done | sort -u | wc -l | tr -d ' ')
ck "3 DISTINCT registry sessionIds" "[ ${NDISTINCT:-0} -eq 3 ]"
LSN=$("$SB" ls --short 2>/dev/null | grep -c "${JAIL_PREFIX}s")
echo "--- qd ls (registry): $LSN sessions ---"; "$SB" ls 2>&1 | tail -5
ck "ls count = 3 (registry layer)" "[ ${LSN:-0} -eq 3 ]"
for i in 0 1 2; do ck "info resolves ${NAMES[$i]}" "\"$SB\" info \"${NAMES[$i]}\" >/dev/null 2>&1"; done
echo "--- kill s3 by name; s1+s2 survive (registry resolution among many) ---"
"$SB" kill --force "${NAMES[2]}" >/dev/null 2>&1; sleep 1
ck "s3 gone from registry" "! \"$SB\" ls --short 2>/dev/null | grep -q \"${JAIL_PREFIX}s3\""
ck "s1 survives in registry" "\"$SB\" ls --short 2>/dev/null | grep -q \"${JAIL_PREFIX}s1\""
ck "s2 survives in registry" "\"$SB\" ls --short 2>/dev/null | grep -q \"${JAIL_PREFIX}s2\""
for n in "${NAMES[0]}" "${NAMES[1]}"; do "$SB" kill --force "$n" >/dev/null 2>&1; done; sleep 1
REM=$("$SB" ls --short 2>/dev/null | grep -c "${JAIL_PREFIX}s" || true); REM=$(printf %s "$REM" | tr -dc 0-9)
ck "all killed (0 remain)" "[ ${REM:-0} -eq 0 ]"
echo "=== M6v3 RESULT: PASS=$PASS FAIL=$FAIL ==="
jail_teardown
