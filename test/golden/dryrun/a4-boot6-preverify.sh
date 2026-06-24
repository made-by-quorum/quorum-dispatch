#!/bin/bash
# a4-boot6-preverify.sh — A4 BOOT-#6 Phase A DRIVER PRE-VERIFICATION (NO real claude).
#
# Mandate (BOOT-#6 brief Phase A): a second rc=127-class failure burns the boot,
# so pre-verify everything that CAN be pre-verified against fakerepl + the wrapper
# BEFORE the one sanctioned real-claude boot:
#   1. perl-alarm timeout wrapper mechanics: fires on a sleeping command (exit 124)
#      AND propagates a real exit code on a fast command (no timeout). Replaces the
#      missing macOS timeout(1) — there is NO timeout(1) anywhere in the driver.
#   2. sb wait rows — STATUS-KEYED, so pre-verifiable: fakerepl writes pid-file
#      status transitions (idle->busy->idle on submit). Drive: sb wait on a busy
#      session -> " done" exit 0; sb wait with --timeout against a kept-busy session
#      -> " timeout" exit 1; sb wait on idle -> "is idle" exit 0.
#   3. the send path — fakerepl accepts send:pty (real PTY) -> "Message sent".
#   4. the --wait DOCUMENTED FAILURE MODE — fakerepl writes NO conversation JSONL,
#      so `sb send:pty <fake> "msg" --wait` must print "Cannot find conversation
#      JSONL file." and exit 1 (CLEAN), NOT rc=127. This proves the --wait verb is
#      wired and fails in the documented way, not in a wrapper/shell-127 way.
#
# Everything in-jail. CLAUDE_BIN points at the JAIL-COPIED fakerepl (M4b idiom).
# NO real-claude boot here. REAL-HOME BELT before/after.
set -u
WT="$(cd "$(dirname "$0")/../../.." && pwd -P)"   # worktree root, NOT hardcoded
cd "$WT" || exit 1
export JAIL_SB_CMD="$WT/target/debug/sb"
export JAIL_ZMX_CMD="/opt/homebrew/bin/zmx"
FAKEREPL_SRC="$WT/target/debug/fakerepl"
. test/golden/lib/jail.sh

EV="$WT/test/golden/dryrun/a4-boot6-preverify-bytes.txt"; : > "$EV"
log(){ printf '%s\n' "$*" | tee -a "$EV"; }
strip(){ perl -pe 's/\e\][0-9].*?(\a|\e\\)//g; s/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\e[()][B0]//g; s/\r//g'; }

# perl-alarm timeout wrapper (macOS has NO timeout(1)). EXACT wrapper carried from
# a4-paste-investigate.sh: fork a child, exec the command; on ALRM kill -9 the
# child and exit 124; otherwise propagate the child's exit code ($?>>8).
tmo(){ local s="$1"; shift; perl -e 'my $t=shift; my $pid=fork; if($pid==0){exec @ARGV or exit 127} local $SIG{ALRM}=sub{kill 9,$pid; exit 124}; alarm $t; waitpid($pid,0); exit($?>>8)' "$s" "$@"; }

log "=== A4 BOOT-#6 PHASE A PRE-VERIFICATION (no real claude) ==="
log "  date: $(date '+%Y-%m-%d %H:%M:%S %Z')"
log "  worktree: $WT"
log "  sb: $("$JAIL_SB_CMD" --version 2>&1 | head -1)"

# Prove the driver has NO timeout(1) anywhere (brief: grep it to prove).
log ""
log "--- GREP PROOF: no timeout(1) in this driver ---"
if grep -nE '(^|[^_[:alnum:]])timeout[[:space:]]' "$0" | grep -v 'SB_RUST_LOCK_TIMEOUT\|--timeout\|timeout_s\|timeout wrapper\|missing macOS timeout\|NO timeout\|no timeout\|class\|burns'; then
    log "  !!! timeout(1) token found above — INVESTIGATE"
else
    log "  OK: no bare timeout(1) invocation in the driver (only --timeout flag / wrapper prose)"
fi

jail_establish || { echo FATAL; exit 1; }
trap jail_teardown EXIT
REAL_SESS="$JAIL_REAL_HOME/.claude/sessions"
rb="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
log ""
log "=== REAL-HOME BELT before: $rb ==="

# ---------------------------------------------------------------------------
# CHECK 1: perl-alarm wrapper mechanics (the rc=127-class guard)
# ---------------------------------------------------------------------------
log ""
log "=== CHECK 1: perl-alarm wrapper mechanics ==="
# 1a. fires on a sleeping command -> exit 124 (alarm killed it)
tmo 1 sleep 5; rc=$?
log "  1a tmo 1 sleep 5 -> rc=$rc (expect 124 = alarm fired)"
[ "$rc" = "124" ] && log "    PASS: wrapper fires + reports 124" || { log "    FAIL"; FAIL1a=1; }
# 1b. propagates a clean exit on a fast command (no timeout) -> exit 0
tmo 5 true; rc=$?
log "  1b tmo 5 true -> rc=$rc (expect 0 = propagated)"
[ "$rc" = "0" ] && log "    PASS: wrapper propagates 0" || { log "    FAIL"; FAIL1b=1; }
# 1c. propagates a NON-zero exit on a fast command -> exit 7
tmo 5 sh -c 'exit 7'; rc=$?
log "  1c tmo 5 'exit 7' -> rc=$rc (expect 7 = propagated non-zero)"
[ "$rc" = "7" ] && log "    PASS: wrapper propagates non-zero exactly" || { log "    FAIL"; FAIL1c=1; }
# 1d. a missing command inside the wrapper -> 127 from the CHILD exec, NOT a shell
#     wrapper crash; proves the wrapper itself is not the rc=127 source (it surfaces
#     a real exec-failure cleanly). This is the documented child-exec path.
tmo 5 this-command-does-not-exist-xyz 2>/dev/null; rc=$?
log "  1d tmo 5 <missing-cmd> -> rc=$rc (child exec-fail surfaces as 127, wrapper intact)"

# ---------------------------------------------------------------------------
# Seed jail home for fakerepl (registry dir) + CLAUDE_BIN swap to JAIL-COPIED fakerepl
# ---------------------------------------------------------------------------
mkdir -p "$HOME/.claude/sessions"
FAKE="$JAIL_ROOT/fakerepl"
cp "$FAKEREPL_SRC" "$FAKE" && chmod +x "$FAKE"
export CLAUDE_BIN="$FAKE"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"

# NB fakerepl writes PRETTY-printed pid JSON ("name": "x" with a space); the M5
# real-claude driver greps the compact form ("name":"x"). For pre-verification we
# parse the JSON name field so it is whitespace-tolerant across both shapes.
pidfile_for(){ local f n; for f in "$HOME/.claude/sessions"/*.json; do [ -f "$f" ]||continue; n="$(python3 -c 'import json,sys
try:print(json.load(open(sys.argv[1])).get("name",""))
except:pass' "$f" 2>/dev/null)"; [ "$n" = "$1" ] && { echo "$f"; return; }; done; return 1; }
status_of(){ local pf; pf="$(pidfile_for "$1")"||{ echo NONE; return; }; python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("status",""))' "$pf" 2>/dev/null||echo '?'; }
ws(){ local n="$1" w="$2" to="$3" i=0; while [ "$i" -lt "$((to*4))" ]; do [ "$(status_of "$n")" = "$w" ]&&return 0; sleep 0.25; i=$((i+1)); done; return 1; }

# ---------------------------------------------------------------------------
# CHECK 2: boot a fakerepl session; sb wait rows are status-keyed -> pre-verifiable
# ---------------------------------------------------------------------------
log ""
log "=== CHECK 2: fakerepl boot + sb wait rows (status-keyed) ==="
NAME="${JAIL_PREFIX}fr"
# Long busy hold so a kept-busy --timeout row can observe 'busy' across a 5s wait.
export SB_FAKEREPL_BUSY_MS=6000
( cd "$WORKDIR" && SB_CLAUDE_FLAGS="--dangerously-skip-permissions" "$JAIL_SB_CMD" new "$NAME" --cwd "$WORKDIR" ) >"$JAIL_ROOT/o" 2>"$JAIL_ROOT/e"
code=$?
log "  sb new exit=$code : $(cat "$JAIL_ROOT/o")"
[ -s "$JAIL_ROOT/e" ] && { log "  stderr:"; sed 's/^/    /' "$JAIL_ROOT/e" | tee -a "$EV"; }
ws "$NAME" idle 20 || log "  WARN: not idle after boot (status=$(status_of "$NAME"))"
jail_assert_resolves_in_jail "$NAME" || { log "  RESOLUTION BELT REFUSED for $NAME"; exit 4; }
log "  resolution belt: $NAME resolves uniquely in-jail (OK)"

# 2a. sb wait on an IDLE session -> exit 0, 'is idle'
out="$(tmo 10 "$JAIL_SB_CMD" wait "$NAME" 2>&1)"; rc=$?
log "  2a sb wait (idle) rc=$rc out=[$(printf '%s' "$out"|tr '\n' '|')]"
{ [ "$rc" = "0" ] && printf '%s' "$out" | grep -qi "idle"; } && log "    PASS: idle-at-entry -> 'is idle' exit 0" || { log "    FAIL"; FAIL2a=1; }

# 2b. sb wait on a BUSY session -> blocks until busy->idle then exit 0 ' done'.
#     Drive busy via a submit (send msg, fakerepl goes busy for BUSY_MS).
"$JAIL_SB_CMD" send:pty "$NAME" "drive a turn 2b" >/dev/null 2>&1
ws "$NAME" busy 5 >/dev/null 2>&1 || log "    WARN: did not observe busy for 2b"
out="$(tmo 20 "$JAIL_SB_CMD" wait "$NAME" 2>&1)"; rc=$?
log "  2b sb wait (busy->idle) rc=$rc out=[$(printf '%s' "$out"|tr '\n' '|')]"
{ [ "$rc" = "0" ] && printf '%s' "$out" | grep -qi "done"; } && log "    PASS: busy -> ' done' exit 0" || { log "    FAIL"; FAIL2b=1; }
ws "$NAME" idle 15 || true

# 2c. sb wait --timeout 5 against a KEPT-BUSY session -> ' timeout' exit 1.
#     BUSY_MS=6000 > 5s timeout, so the wait must time out while still busy.
"$JAIL_SB_CMD" send:pty "$NAME" "drive a long turn 2c" >/dev/null 2>&1
ws "$NAME" busy 5 >/dev/null 2>&1 || log "    WARN: did not observe busy for 2c"
out="$(tmo 20 "$JAIL_SB_CMD" wait "$NAME" --timeout 5 2>&1)"; rc=$?
log "  2c sb wait --timeout 5 (kept busy) rc=$rc out=[$(printf '%s' "$out"|tr '\n' '|')]"
{ [ "$rc" = "1" ] && printf '%s' "$out" | grep -qi "timeout"; } && log "    PASS: kept-busy --timeout 5 -> ' timeout' exit 1" || { log "    FAIL (rc=$rc)"; FAIL2c=1; }
ws "$NAME" idle 15 || true

# ---------------------------------------------------------------------------
# CHECK 3: the send path (send:pty -> 'Message sent')
# ---------------------------------------------------------------------------
log ""
log "=== CHECK 3: send:pty path ==="
ws "$NAME" idle 10 || true
jail_assert_resolves_in_jail "$NAME" || { log "  RESOLUTION BELT REFUSED (check3)"; exit 4; }
out="$("$JAIL_SB_CMD" send:pty "$NAME" "plain send path check" 2>&1)"; rc=$?
log "  3 send:pty rc=$rc out=[$(printf '%s' "$out"|tr '\n' '|')]"
{ printf '%s' "$out" | grep -qi "Message sent"; } && log "    PASS: send path accepted ('Message sent')" || { log "    FAIL"; FAIL3=1; }
ws "$NAME" idle 15 || true

# ---------------------------------------------------------------------------
# CHECK 4: --wait DOCUMENTED FAILURE MODE (no JSONL -> clean exit 1, NOT 127)
# ---------------------------------------------------------------------------
log ""
log "=== CHECK 4: send:pty --wait documented failure (no conversation JSONL) ==="
ws "$NAME" idle 10 || true
jail_assert_resolves_in_jail "$NAME" || { log "  RESOLUTION BELT REFUSED (check4)"; exit 4; }
# fakerepl writes NO conversation JSONL, so --wait must hit the documented
# precondition failure: "Cannot find conversation JSONL file." + exit 1.
out="$(tmo 15 "$JAIL_SB_CMD" send:pty "$NAME" "would-wait msg" --wait --timeout 5 2>&1)"; rc=$?
log "  4 send:pty --wait rc=$rc out=[$(printf '%s' "$out"|tr '\n' '|')]"
if [ "$rc" = "1" ] && printf '%s' "$out" | grep -qi "Cannot find conversation JSONL"; then
    log "    PASS: --wait fails CLEAN (documented: no JSONL -> exit 1, NOT rc=127)"
elif [ "$rc" = "127" ]; then
    log "    FAIL: rc=127 — THIS IS THE BOOT-BURNING CLASS. STOP."
    FAIL4=1
else
    log "    NOTE: rc=$rc — examine (not 127, but not the documented exit-1 path)"
    FAIL4=1
fi

# ---------------------------------------------------------------------------
# Teardown + belt
# ---------------------------------------------------------------------------
"$JAIL_SB_CMD" ls --all --short 2>/dev/null | grep -q "$NAME" && jail_kill_session "$NAME" >/dev/null 2>&1
"$JAIL_ZMX_CMD" kill "$NAME" --force >/dev/null 2>&1 || true
sleep 1
log ""
ra="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
log "=== REAL-HOME BELT after: $ra ($([ "$rb" = "$ra" ]&&echo HOLDS||echo VIOLATION)) ==="

log ""
log "=== PRE-VERIFICATION SUMMARY ==="
ANYFAIL=0
for f in FAIL1a FAIL1b FAIL1c FAIL2a FAIL2b FAIL2c FAIL3 FAIL4; do
    eval "v=\${$f:-}"
    [ -n "$v" ] && { log "  $f: FAILED"; ANYFAIL=1; }
done
if [ "$ANYFAIL" = "0" ]; then
    log "  ALL PRE-VERIFICATION CHECKS PASSED — wrapper mechanics + sb wait + send"
    log "  path + --wait clean-failure all GREEN. No rc=127-class anomaly. GO for boot."
else
    log "  !!! PRE-VERIFICATION HAD FAILURES — DO NOT BOOT. Report instead."
fi
log "=== DONE (teardown via trap) ==="
