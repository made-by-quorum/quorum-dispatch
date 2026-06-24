#!/usr/bin/env bash
# test/golden/scenarios/a6_telemetry.sh — A6 D3 telemetry GATE driver (impl-B).
#
# Drives the spec §7 G-A3 (fault-model) + G-A6 (opaque-payload / usage / dirty-
# state) rows fully JAILED, against the sb-under-test binary. Every sb invocation
# runs through the jail env (jail_sb) and touches ONLY the jail HOME/SB_HOME — the
# `sb mark`/`ls`/`info` engine invocations with stub state are sanctioned (spec:
# they touch only jail HOME/SB_HOME). No real sessions are created.
#
# Per-row PASS/FAIL + a SUMMARY footer. Bash 3.2 floor (macOS): no assoc arrays,
# no ${var,,}, no mapfile. jail_establish/teardown with an EXIT trap.
#
# Usage:  bash test/golden/scenarios/a6_telemetry.sh
# Env:    SB_BIN (sb-under-test; default target/debug/sb)
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
cd "$REPO_ROOT" || { echo "FATAL: cannot cd to repo root"; exit 1; }

SB_BIN="${SB_BIN:-$REPO_ROOT/target/debug/sb}"
[ -x "$SB_BIN" ] || { echo "FATAL: sb binary missing: $SB_BIN (build it first)"; exit 2; }

# Point the jail's sb-under-test at our binary BEFORE sourcing jail.sh.
export JAIL_SB_CMD="$SB_BIN"

# shellcheck source=test/golden/lib/jail.sh
. "$REPO_ROOT/test/golden/lib/jail.sh"

PASS=0; FAIL=0; FAILED=""
row_pass() { PASS=$((PASS+1)); printf '  [PASS] %-8s %s\n' "$1" "$2"; }
row_fail() { FAIL=$((FAIL+1)); FAILED="$FAILED $1"; printf '  [FAIL] %-8s %s\n' "$1" "$2"; }
hdr() { printf '\n========== %s ==========\n' "$1"; }

# countf <pattern> <file>   — number of matching lines, always a single integer
# (grep -c exits nonzero on zero matches; we normalize via a clean recount).
countf() { grep -c -- "$1" "$2" 2>/dev/null; true; }
# counts <pattern> from stdin string — same, over a here-string.
counts() { printf '%s' "$1" | grep -c -- "$2" 2>/dev/null; true; }

# Establish the jail; tear down on ANY exit.
RUNID="a6tel-$$"
jail_establish "$RUNID" || { echo "FATAL: jail_establish failed"; exit 3; }
trap 'jail_teardown' EXIT

# The marks file the engine writes to (SB_HOME-honored, same as `sb mark`).
MARKS="$SB_HOME/state/marks.jsonl"
SESSIONS_DIR="$HOME/.claude/sessions"
mkdir -p "$SB_HOME/state" "$SESSIONS_DIR"

# ---------------------------------------------------------------------------
# Helper: seed a live-shape registry entry so `info`/`ls --json` can resolve a
# name. The pid need not be alive — the fold join is by sessionId/name. We use a
# high, almost-certainly-dead pid; the engine classifies it cold, which still
# resolves by name for info/ls.
seed_registry() {
    local pid="$1" name="$2" sid="$3"
    printf '{"pid":%s,"sessionId":"%s","status":"idle","name":"%s","cwd":"%s"}\n' \
        "$pid" "$sid" "$name" "$HOME/proj" > "$SESSIONS_DIR/$pid.json"
}

# ===========================================================================
# G-A6a — `sb mark` appends a payload line AND a usage line (spec §4.1). The
# opaque payload carries an INNER "event" key + org vocabulary; it must round-
# trip verbatim and NOT collide with engine event lines.
# ===========================================================================
hdr "G-A6a sb mark: payload line + usage line, opaque inner event key"
seed_registry 999001 "${JAIL_PREFIX}alpha" "sid-alpha"
: > "$MARKS"  # start clean
jail_sb mark "${JAIL_PREFIX}alpha" '{"event":"create","on_behalf_of":"lead","backend":"SPOOF"}' >/dev/null 2>&1
mark_rc=$?
# Two lines: one mark (top-level "payload"), one usage (top-level "event":"usage").
line_count="$(wc -l < "$MARKS" | tr -d ' ')"
mark_line="$(countf '"payload"' "$MARKS")"
usage_line="$(countf '"event":"usage"' "$MARKS")"
usage_verb="$(grep '"event":"usage"' "$MARKS" 2>/dev/null | grep -c '"verb":"mark"' 2>/dev/null; true)"
# The inner "event":"create" + "backend":"SPOOF" stayed INSIDE the payload object
# (no TOP-LEVEL create event line was emitted by `sb mark`). A line-level grep
# can't tell a NESTED "event":"create" from a top-level one, so parse each line's
# TOP-LEVEL "event" key with python and count those equal to "create".
spoof_top="$(python3 - "$MARKS" <<'PY'
import json, sys
n = 0
for ln in open(sys.argv[1]):
    ln = ln.strip()
    if not ln:
        continue
    try:
        o = json.loads(ln)
    except Exception:
        continue
    if isinstance(o, dict) and o.get("event") == "create":
        n += 1
print(n)
PY
)"
printf '    rc=%s lines=%s mark=%s usage=%s usage-verb-mark=%s top-create=%s\n' \
    "$mark_rc" "$line_count" "$mark_line" "$usage_line" "$usage_verb" "$spoof_top"
if [ "$mark_rc" = "0" ] && [ "$line_count" = "2" ] && [ "$mark_line" = "1" ] \
   && [ "$usage_line" = "1" ] && [ "$usage_verb" = "1" ] && [ "$spoof_top" = "0" ]; then
    row_pass "G-A6a" "mark payload + usage line; inner event key did not promote"
else
    row_fail "G-A6a" "see excerpt above; marks=$(cat "$MARKS")"
fi

# ===========================================================================
# G-A3a — fold determinism over a CREATE event: `info`/`ls --json` surface
# backend + spawnedBy when a create event exists for the session.
# ===========================================================================
hdr "G-A3a fold surfacing: info + ls --json show backend/spawnedBy"
seed_registry 999002 "${JAIL_PREFIX}beta" "sid-beta"
: > "$MARKS"
# A hand-written create event line (the lead's create.rs stamp emits this shape).
printf '{"ts":"2026-06-05T10:00:00.000Z","event":"create","name":"%sbeta","sessionId":"sid-beta","spawnedBy":"orchestrator","backend":"ccr-3456"}\n' \
    "$JAIL_PREFIX" >> "$MARKS"
info_out="$(jail_sb info "${JAIL_PREFIX}beta" 2>/dev/null)"
ls_out="$(jail_sb ls --json 2>/dev/null)"
info_has_backend="$(counts "$info_out" 'Backend:.*ccr-3456')"
info_has_spawn="$(counts "$info_out" 'Spawned by:.*orchestrator')"
ls_has_backend="$(counts "$ls_out" '"backend": "ccr-3456"')"
ls_has_spawn="$(counts "$ls_out" '"spawnedBy": "orchestrator"')"
printf '    info-backend=%s info-spawn=%s ls-backend=%s ls-spawn=%s\n' \
    "$info_has_backend" "$info_has_spawn" "$ls_has_backend" "$ls_has_spawn"
if [ "$info_has_backend" = "1" ] && [ "$info_has_spawn" = "1" ] \
   && [ "$ls_has_backend" = "1" ] && [ "$ls_has_spawn" = "1" ]; then
    row_pass "G-A3a" "info + ls --json surface backend/spawnedBy from the fold"
else
    row_fail "G-A3a" "info=[$info_out] ls=[$ls_out]"
fi

# ===========================================================================
# G-A1 (negative control) — a session with NO telemetry data has NO Backend:/
# Spawned by: lines and NO backend/spawnedBy JSON fields. Additive-only proof.
# ===========================================================================
hdr "G-A1 negative control: no telemetry → no A6 lines/fields"
seed_registry 999003 "${JAIL_PREFIX}gamma" "sid-gamma"
: > "$MARKS"  # empty marks → fold yields nothing for gamma
info_g="$(jail_sb info "${JAIL_PREFIX}gamma" 2>/dev/null)"
ls_g="$(jail_sb ls --json 2>/dev/null)"
g_info_backend="$(counts "$info_g" 'Backend:')"
g_info_spawn="$(counts "$info_g" 'Spawned by:')"
g_ls_backend="$(counts "$ls_g" '"backend"')"
g_ls_spawn="$(counts "$ls_g" '"spawnedBy"')"
printf '    info-backend=%s info-spawn=%s ls-backend=%s ls-spawn=%s\n' \
    "$g_info_backend" "$g_info_spawn" "$g_ls_backend" "$g_ls_spawn"
if [ "$g_info_backend" = "0" ] && [ "$g_info_spawn" = "0" ] \
   && [ "$g_ls_backend" = "0" ] && [ "$g_ls_spawn" = "0" ]; then
    row_pass "G-A1" "no telemetry → byte-clean (no Backend:/Spawned by:, no JSON fields)"
else
    row_fail "G-A1" "info=[$info_g] ls=[$ls_g]"
fi

# ===========================================================================
# G-A3b — RESTART/DIRTY fault model (spec §7 G-A3, red-team F8). Fixture-
# constructed faults: TORN trailing line, MISSING file, EMPTY file. The fold
# stays deterministic + identical ignoring the torn tail; `sb mark` append still
# works onto a dirty file.
# ===========================================================================
hdr "G-A3b dirty-state: torn tail / missing / empty — fold + append survive"
seed_registry 999004 "${JAIL_PREFIX}delta" "sid-delta"

# (1) Torn trailing line after a WHOLE create event.
: > "$MARKS"
printf '{"ts":"t","event":"create","name":"%sdelta","sessionId":"sid-delta","backend":"be-delta"}\n' "$JAIL_PREFIX" >> "$MARKS"
printf '{"ts":"t2","event":"crea' >> "$MARKS"   # torn, no newline
torn_info="$(jail_sb info "${JAIL_PREFIX}delta" 2>/dev/null)"
torn_rc=$?
torn_ok="$(counts "$torn_info" 'Backend:.*be-delta')"

# (2) `sb mark` appends onto the torn file → still works (non-fatal, whole lines).
jail_sb mark "${JAIL_PREFIX}delta" '{"k":1}' >/dev/null 2>&1
append_rc=$?

# (3) Missing file: remove it → info still exits 0 with no A6 line, no crash.
rm -f "$MARKS"
miss_info="$(jail_sb info "${JAIL_PREFIX}delta" 2>/dev/null)"
miss_rc=$?
miss_clean="$(counts "$miss_info" 'Backend:')"

# (4) Empty file: truncate → same.
: > "$MARKS"
empty_info="$(jail_sb info "${JAIL_PREFIX}delta" 2>/dev/null)"
empty_rc=$?
empty_clean="$(counts "$empty_info" 'Backend:')"

printf '    torn(rc=%s ok=%s) append(rc=%s) missing(rc=%s clean=%s) empty(rc=%s clean=%s)\n' \
    "$torn_rc" "$torn_ok" "$append_rc" "$miss_rc" "$miss_clean" "$empty_rc" "$empty_clean"
if [ "$torn_rc" = "0" ] && [ "$torn_ok" = "1" ] && [ "$append_rc" = "0" ] \
   && [ "$miss_rc" = "0" ] && [ "$miss_clean" = "0" ] \
   && [ "$empty_rc" = "0" ] && [ "$empty_clean" = "0" ]; then
    row_pass "G-A3b" "torn-tail folds (ignoring tail); append survives; missing/empty clean"
else
    row_fail "G-A3b" "see excerpt above"
fi

# ===========================================================================
# G-A6b — kill -9 SUPPORTING row (spec §7 G-A3, red-team F8). A driver loops
# `sb mark` (sub-PIPE_BUF lines); kill -9 it mid-stream → marks.jsonl contains
# only WHOLE lines plus at most one torn tail; the fold stays green (no panic).
# ===========================================================================
hdr "G-A6b kill -9 a sb-mark loop driver → whole lines + ≤1 torn tail, fold green"
seed_registry 999005 "${JAIL_PREFIX}epsilon" "sid-epsilon"
: > "$MARKS"
# Background loop firing `sb mark` (sub-PIPE_BUF lines). The subshell INHERITS the
# jailed HOME/SB_HOME exported by jail_establish, so every append lands in the
# jail MARKS file. We kill -9 the LOOP PID directly (tracked via $!) — we do NOT
# touch jail_register_pid (it requires <pid> <name>; a 1-arg call trips set -u and
# would abort the script). The kill targets only OUR loop PID.
( i=0; while [ "$i" -lt 200 ]; do
    "$SB_BIN" mark "${JAIL_PREFIX}epsilon" "{\"n\":$i}" >/dev/null 2>&1
    i=$((i+1))
  done ) &
LOOP_PID=$!
# A7 ratchet fix (Lima): `sleep 1` assumed ≥1 mark lands in a second — on a slow
# host (aarch64 VM, cold cache) ZERO had landed and the row failed with total=0.
# Poll until the stream is demonstrably mid-flight (≥5 lines) before the kill,
# bounded at ~15s; kill-mid-stream semantics unchanged. If the loop finishes
# early (fast host), the kill is a no-op and the row still asserts the
# whole-lines property over the complete stream.
_w=0
while [ "$_w" -lt 150 ]; do
    [ "$(wc -l < "$MARKS" 2>/dev/null | tr -d ' ')" -ge 5 ] 2>/dev/null && break
    kill -0 "$LOOP_PID" 2>/dev/null || break   # loop already finished
    sleep 0.1; _w=$((_w+1))
done
kill -9 "$LOOP_PID" 2>/dev/null || true
wait "$LOOP_PID" 2>/dev/null || true
# Every NON-EMPTY line that parses as JSON is whole; a single torn tail is allowed.
bad=0; total=0; torn=0
while IFS= read -r ln || [ -n "$ln" ]; do
    [ -z "$ln" ] && continue
    total=$((total+1))
    if printf '%s' "$ln" | python3 -c 'import json,sys; json.loads(sys.stdin.read())' >/dev/null 2>&1; then
        :
    else
        torn=$((torn+1))
    fi
done < "$MARKS"
# At most one torn line (the final partial write); never a torn line in the middle.
# We approximate "torn only at the tail" by: torn <= 1.
fold_info="$(jail_sb info "${JAIL_PREFIX}epsilon" 2>/dev/null)"
fold_rc=$?
printf '    total-lines=%s torn=%s fold-info-rc=%s\n' "$total" "$torn" "$fold_rc"
if [ "$total" -ge 1 ] && [ "$torn" -le 1 ] && [ "$fold_rc" = "0" ]; then
    row_pass "G-A6b" "kill -9 left whole lines + <=1 torn tail; fold stayed green"
else
    row_fail "G-A6b" "total=$total torn=$torn fold_rc=$fold_rc"
fi

# ===========================================================================
# SUMMARY
# ===========================================================================
hdr "SUMMARY"
printf '  PASS=%s FAIL=%s\n' "$PASS" "$FAIL"
if [ "$FAIL" -ne 0 ]; then
    printf '  FAILED ROWS:%s\n' "$FAILED"
    exit 1
fi
printf '  ALL A6 TELEMETRY GATE ROWS GREEN\n'
exit 0
