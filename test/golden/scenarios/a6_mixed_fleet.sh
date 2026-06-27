#!/usr/bin/env bash
# test/golden/scenarios/a6_mixed_fleet.sh — A6 G-A4: mixed-fleet interop.
#
# The PINNED TS qd (8c59ec4, staged by prep_pinned_ts.sh, bun-driven) must still
# ls / resolve / gc a RUST-created session whose registry entry + marks stream
# carry the NEW A6 fields (backend / spawnedBy). Runs fully IN-JAIL: both engines
# see the SAME jailed HOME (TS keys its registry off homedir() — ADD-4), so the
# org's real state is never touched.
#
# Rows:
#   r1  Rust `qd new` (stub claude writes its entry WITH backend+spawnedBy —
#       the A1-field tolerance surface) → marks.jsonl gains create+usage lines.
#   r2  TS `ls` sees the session; exit 0; output mentions the name.
#   r3  TS `info <name>` resolves it; exit 0 (field TOLERANCE: no crash/parse-drop).
#   r4  TS `gc` runs clean on the live session (exit 0, session survives).
#   r5  kill the stub; TS `gc` reaps; the tombstone (TS-side rename) PRESERVES the
#       new fields byte-wise (rename never rewrites — tolerance proof is r2-r4;
#       this row proves the fields survive the TS lifecycle end-state).
#
# Env: SB_BIN (Rust binary), TS_DIR (pinned clone; default /tmp/a6-ts-pin).
# Bash 3.2. ADDITIVE-NOT-PARITY evidence.
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
cd "$REPO_ROOT" || exit 1

WT="$REPO_ROOT"
SB_BIN="${SB_BIN:-$WT/target/debug/qd}"
TS_DIR="${TS_DIR:-/tmp/a6-ts-pin}"
BUN_BIN="${BUN_BIN:-$(command -v bun || echo "$HOME/.bun/bin/bun")}"
[ -x "$SB_BIN" ] || { echo "FATAL: qd binary missing: $SB_BIN"; exit 1; }
[ -f "$TS_DIR/src/index.ts" ] || { echo "FATAL: pinned TS clone missing: $TS_DIR (run prep_pinned_ts.sh)"; exit 1; }
[ -x "$BUN_BIN" ] || { echo "FATAL: bun not found"; exit 1; }
# Anti-drift belt: the clone must BE at the pin.
TS_HEAD="$(git -C "$TS_DIR" rev-parse HEAD 2>/dev/null)"
[ "$TS_HEAD" = "8c59ec456fe82780fd75d8afb5fe48dc72e10bc8" ] \
    || { echo "FATAL: TS clone not at pin (HEAD=$TS_HEAD)"; exit 1; }

export JAIL_SB_CMD="$SB_BIN"
export JAIL_ZMX_CMD="${ZMX_BIN:-$(command -v zmx 2>/dev/null || echo /opt/homebrew/bin/zmx)}"
. test/golden/lib/jail.sh
SHORT_RUNID="$(printf '%s' "${RANDOM:-0}${RANDOM:-0}" | tr -cd 'a-z0-9' | cut -c1-4)"
[ -n "$SHORT_RUNID" ] || SHORT_RUNID="z$$"
jail_establish "$SHORT_RUNID" || { echo "FATAL: jail_establish failed"; exit 1; }
trap 'jail_teardown' EXIT
REAL_BEFORE="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"

PASS_ALL=1
note_fail() { PASS_ALL=0; echo "  [FAIL] $1"; }
ok()        { echo "  [ok] $1"; }

ts_sb() { ( cd "$TS_DIR" && "$BUN_BIN" src/index.ts "$@" ); }

# Stub claude: registry entry WITH the A6 fields (backend/spawnedBy) — the
# tolerance surface TS must survive (TS PidEntry at pin has neither field).
STUB="$JAIL_ROOT/stub-claude"
cat > "$STUB" <<'EOS'
#!/bin/bash
name=""
while [ $# -gt 0 ]; do
  case "$1" in
    --name) name="$2"; shift 2;;
    *) shift;;
  esac
done
[ -n "${SBRG_STUB_NAME:-}" ] && name="$SBRG_STUB_NAME"
mkdir -p "$HOME/.claude/sessions"
printf '{"pid":%d,"name":"%s","status":"idle","sessionId":"mf-sid-1","cwd":"%s","version":"stub","kind":"interactive","entrypoint":"cli","startedAt":1700000000000,"updatedAt":1700000000000,"backend":"ccr-test","spawnedBy":"sbr-pa6-lead"}\n' \
  "$$" "$name" "$PWD" > "$HOME/.claude/sessions/$$.json"
exec sleep 120
EOS
chmod +x "$STUB"
export CLAUDE_BIN="$STUB"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"
NAME="${JAIL_PREFIX}mf"

echo "--- r1: Rust new (entry carries backend+spawnedBy; marks stamped) ---"
( cd "$WORKDIR" && env SBRG_STUB_NAME="$NAME" "$SB_BIN" new "$NAME" --cwd "$WORKDIR" ) \
    > "$JAIL_ROOT/r1.out" 2> "$JAIL_ROOT/r1.err"
r1=$?
[ "$r1" = 0 ] && ok "r1 rust new exit 0" || note_fail "r1 rust new exit $r1 ($(cat "$JAIL_ROOT/r1.err"))"
ENTRY_FILE="$(ls "$HOME/.claude/sessions/" | grep '^[0-9]*\.json$' | head -1)"
grep -q '"backend":"ccr-test"' "$HOME/.claude/sessions/$ENTRY_FILE" \
    && ok "r1 entry carries backend field" || note_fail "r1 entry missing backend field"
MARKS="$SB_HOME/state/marks.jsonl"
[ -f "$MARKS" ] && grep -q '"event":"create"' "$MARKS" \
    && ok "r1 marks.jsonl has the create event" || note_fail "r1 marks.jsonl create event missing"

echo "--- r2: TS ls sees the Rust session (field tolerance) ---"
ts_sb ls --all > "$JAIL_ROOT/r2.out" 2> "$JAIL_ROOT/r2.err"; r2=$?
[ "$r2" = 0 ] && ok "r2 TS ls exit 0" || note_fail "r2 TS ls exit $r2 ($(head -2 "$JAIL_ROOT/r2.err"))"
grep -q "$NAME" "$JAIL_ROOT/r2.out" \
    && ok "r2 TS ls lists the Rust-created session" || note_fail "r2 TS ls does not list $NAME"

echo "--- r3: TS resolve/info on the Rust session ---"
ts_sb info "$NAME" > "$JAIL_ROOT/r3.out" 2> "$JAIL_ROOT/r3.err"; r3=$?
[ "$r3" = 0 ] && ok "r3 TS info exit 0 (tolerates new fields)" \
    || note_fail "r3 TS info exit $r3 ($(head -2 "$JAIL_ROOT/r3.err"))"
grep -q "mf-sid-1" "$JAIL_ROOT/r3.out" \
    && ok "r3 TS info resolved the right session" || note_fail "r3 TS info wrong/empty output"

echo "--- r4: TS gc with the session LIVE (must not eat it) ---"
ts_sb gc > "$JAIL_ROOT/r4.out" 2> "$JAIL_ROOT/r4.err"; r4=$?
[ "$r4" = 0 ] && ok "r4 TS gc exit 0" || note_fail "r4 TS gc exit $r4 ($(head -2 "$JAIL_ROOT/r4.err"))"
[ -f "$HOME/.claude/sessions/$ENTRY_FILE" ] \
    && ok "r4 live session survived TS gc" || note_fail "r4 TS gc removed a LIVE session entry"

echo "--- r5: kill stub -> TS gc traverses dead-session state w/ A6 fields ---"
# SCOPE (merge-ruling MAJOR-2 re-label): TS gc at pin does NOT transform registry
# entries — its candidates are garbage FILES (cc-jsonl / existing tombstones /
# oc-sidecars / oc-logs; gc.ts:30 candidate types, :93-97 live-set by pid, :215-221
# tombstone SCAN — consumes, never creates). The registry-entry→tombstone transform
# is qd kill/reconcile, NOT gc. So "TS gc tombstones the entry and the fields
# survive THAT transform" is not an exercisable path at pin. This row asserts what
# TS gc ACTUALLY does over dead-session state carrying A6 fields: exits 0, no
# crash, no corruption of the entry. The TS-driven entry-TRANSFORM of an
# A6-fields entry = RECORDED EXCLUSION; the remaining carrier for field survival
# through a re-serialize is the Rust-side serde round-trip
# (registry.rs ensure_tombstone synthesize-branch test + write_entry round-trip).
STUB_PID="${ENTRY_FILE%.json}"
kill -9 "$STUB_PID" 2>/dev/null; sleep 1
ts_sb gc > "$JAIL_ROOT/r5.out" 2> "$JAIL_ROOT/r5.err"; r5=$?
[ "$r5" = 0 ] && ok "r5 TS gc (post-kill) exit 0 over A6-fields dead-session state" \
    || note_fail "r5 TS gc exit $r5 ($(head -2 "$JAIL_ROOT/r5.err"))"
if [ -f "$HOME/.claude/sessions/$ENTRY_FILE" ]; then
    grep -q '"backend":"ccr-test"' "$HOME/.claude/sessions/$ENTRY_FILE" \
        && ok "r5 entry NOT corrupted by TS gc traversal (fields intact; transform = recorded exclusion)" \
        || note_fail "r5 TS gc corrupted the entry's A6 fields"
else
    note_fail "r5 TS gc unexpectedly removed the registry entry (not a pin gc behavior)"
fi

REAL_AFTER="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"
echo "--- real-home belt: $REAL_BEFORE -> $REAL_AFTER ---"
[ "$REAL_BEFORE" = "$REAL_AFTER" ] && ok "real-home untouched ($REAL_AFTER)" \
    || note_fail "real-home session count changed ($REAL_BEFORE -> $REAL_AFTER)"

echo "==========================================================="
if [ "$PASS_ALL" = 1 ]; then echo "A6-MIXED-FLEET: PASS"; exit 0; fi
echo "A6-MIXED-FLEET: FAIL"; exit 1
