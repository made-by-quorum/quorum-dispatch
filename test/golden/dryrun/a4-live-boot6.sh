#!/bin/bash
# a4-live-boot6.sh — A4 BOOT-#6 (the ONE sanctioned real-claude boot).
#
# Sanction: orc-2 ruling relay-1780631655040-9 item 3 (ONE boot, dual purpose,
# ledgered R5). Closes R3 (the ×5 unattested send:pty --wait + qd wait rows) and
# confirms R4 fixed LIVE (the ≥4.2KB idle-path paste that was RED in M5).
#
# Assembled from the M5 pieces (a4-live-boot1-soak.sh + a4-paste-investigate.sh):
#   - seed recipe = M5 full recipe (probe-3 GrowthBook + onboarding + auth
#     .credentials.json/oauthAccount, all READ-ONLY from the real home)
#   - PERL-ALARM wrapper replaces the missing macOS timeout(1) (NO timeout(1) in
#     this driver — grep-proven by the Phase A pre-verify)
#   - glob-fallback $JP resolver (re-resolved AFTER the warm-up turn) — the M5 R2 fix
#   - resolution belt before every session-targeting row
#   - REAL-HOME BELT before/after
#
# Rows (brief Phase B step 5):
#   a. warm-up short send:pty (turns flowing + resolves $JP via glob-fallback)
#   b. send:pty --wait #1 (idle): short msg, --timeout 60 -> reply printed, exit 0
#   c. send:pty --wait #2 (busy): long turn, then --wait while busy -> queued-then-
#      answered attribution (reply == OUR message), exit 0
#   d. qd wait x3: busy->' done' exit 0; idle-at-entry->'is idle' exit 0;
#      --timeout 5 kept-busy->' timeout' exit 1
#   e. R4 LIVE CONFIRM: idle-path >=4.2KB single message -> user-record DELTA==1
#      (the paste LANDS) + session went busy. RED in M5; must be GREEN now.
set -u
WT="$(cd "$(dirname "$0")/../../.." && pwd -P)"
cd "$WT" || exit 1
export JAIL_SB_CMD="$WT/target/debug/qd"
export JAIL_ZMX_CMD="/opt/homebrew/bin/zmx"
. test/golden/lib/jail.sh

EV="$WT/test/golden/dryrun/a4-boot6-bytes.txt"; : > "$EV"
log(){ printf '%s\n' "$*" | tee -a "$EV"; }
strip(){ perl -pe 's/\e\][0-9].*?(\a|\e\\)//g; s/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\e[()][B0]//g; s/\r//g'; }
# perl-alarm timeout wrapper (macOS has NO timeout(1)). EXACT M5 wrapper.
tmo(){ local s="$1"; shift; perl -e 'my $t=shift; my $pid=fork; if($pid==0){exec @ARGV or exit 127} local $SIG{ALRM}=sub{kill 9,$pid; exit 124}; alarm $t; waitpid($pid,0); exit($?>>8)' "$s" "$@"; }

log "=== A4 BOOT-#6 (one sanctioned real-claude boot) ==="
log "  date: $(date '+%Y-%m-%d %H:%M:%S %Z')"
log "  worktree: $WT  | tip: $(git -C "$WT" log -1 --format='%h %s' 2>/dev/null)"

jail_establish || { echo FATAL; exit 1; }
trap jail_teardown EXIT
REAL_SESS="$JAIL_REAL_HOME/.claude/sessions"
rb="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
log "=== REAL-HOME BELT before: $rb ==="

# --- SEED (M5 full recipe; all READ-ONLY from real home) -------------------
mkdir -p "$HOME/.claude/channels" "$HOME/.claude/sessions"
ln -s /home/u/work/cc-relay "$HOME/.claude/channels/relay" 2>/dev/null || true
GB="$(python3 -c 'import json,sys;print(json.dumps(json.load(open(sys.argv[1])).get("cachedGrowthBookFeatures",{})))' "$JAIL_REAL_HOME/.claude.json" 2>/dev/null||echo '{}')"
cp "$JAIL_REAL_HOME/.claude/.credentials.json" "$HOME/.claude/.credentials.json" 2>/dev/null && chmod 600 "$HOME/.claude/.credentials.json"||true
log "  credentials seeded: $([ -f "$HOME/.claude/.credentials.json" ]&&echo yes||echo NO)"
AUTH="$(python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));print(json.dumps({k:d[k] for k in ("oauthAccount","userID","claudeCodeFirstTokenDate") if k in d}))' "$JAIL_REAL_HOME/.claude.json" 2>/dev/null||echo '{}')"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"; RW="$(cd "$WORKDIR"&&pwd -P)"
python3 -c 'import json,sys
b={"hasCompletedOnboarding":True,"bypassPermissionsModeAccepted":True,"dangerouslyLoadDevelopmentChannels":True}
b["cachedGrowthBookFeatures"]=json.loads(sys.argv[1]); b["projects"]={sys.argv[3]:{"hasTrustDialogAccepted":True},sys.argv[4]:{"hasTrustDialogAccepted":True}}
b.update(json.loads(sys.argv[2])); print(json.dumps(b))' "$GB" "$AUTH" "$WORKDIR" "$RW" > "$HOME/.claude.json"

NAME="${JAIL_PREFIX}b6"
# pidfile lookup: real claude writes COMPACT pid JSON; tolerate both shapes.
pidfile_for(){ local f n; for f in "$HOME/.claude/sessions"/*.json; do [ -f "$f" ]||continue; n="$(python3 -c 'import json,sys
try:print(json.load(open(sys.argv[1])).get("name",""))
except:pass' "$f" 2>/dev/null)"; [ "$n" = "$1" ] && { echo "$f"; return; }; done; return 1; }
status_of(){ local pf; pf="$(pidfile_for "$1")"||{ echo NONE; return; }; python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("status",""))' "$pf" 2>/dev/null||echo '?'; }
ws(){ local n="$1" w="$2" to="$3" i=0; while [ "$i" -lt "$((to*4))" ]; do [ "$(status_of "$n")" = "$w" ]&&return 0; sleep 0.25; i=$((i+1)); done; return 1; }
jp_of(){ ls -t "$HOME/.claude/projects"/*/*.jsonl 2>/dev/null|head -1; }
urc(){ [ -f "$1" ]||{ echo 0; return; }; python3 -c 'import json,sys
n=0
for l in open(sys.argv[1]):
 l=l.strip()
 if not l:continue
 try:r=json.loads(l)
 except:continue
 if r.get("type")=="user":n+=1
print(n)' "$1"; }

GREEN=0; RED=0
mark(){ if [ "$1" = G ]; then GREEN=$((GREEN+1)); else RED=$((RED+1)); fi; }

# --- BOOT ------------------------------------------------------------------
log ""
log "=== qd new (real claude) ==="
( cd "$WORKDIR" && "$JAIL_SB_CMD" new "$NAME" --cwd "$WORKDIR" ) >"$JAIL_ROOT/o" 2>"$JAIL_ROOT/e"
code=$?
log "  qd new exit=$code : $(cat "$JAIL_ROOT/o")"
[ -s "$JAIL_ROOT/e" ] && { log "  stderr:"; head -5 "$JAIL_ROOT/e"|sed 's/^/    /'|tee -a "$EV"; }
if [ "$code" != 0 ]; then log "!!! BOOT FAILED — capturing + stop"; jail_zmx history "$NAME" 2>/dev/null|strip|tail -20|sed 's/^/  /'|tee -a "$EV"; exit 1; fi
ws "$NAME" idle 30 || log "  WARN not idle after boot (status=$(status_of "$NAME"))"
sleep 1
PF="$(pidfile_for "$NAME"||true)"
CLVER="$([ -n "$PF" ]&&python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("version",""))' "$PF" 2>/dev/null||echo '?')"
log "  claude version in jail: $CLVER"
jail_assert_resolves_in_jail "$NAME" || { log "  RESOLUTION BELT REFUSED"; exit 4; }
log "  resolution belt: $NAME resolves uniquely in-jail (OK)"

# --- (a) WARM-UP -----------------------------------------------------------
log ""
log "=== (a) warm-up send:pty (turns flowing; resolve \$JP via glob-fallback) ==="
out="$("$JAIL_SB_CMD" send:pty "$NAME" "Reply with exactly: AUTHOK and nothing else." 2>&1)"
log "  (a) send out=[$out]"
i=0; JP=""; while [ "$i" -lt 120 ]; do JP="$(jp_of)"; [ -n "$JP" ]&&[ "$(urc "$JP")" -ge 1 ]&&break; sleep 0.5; i=$((i+1)); done
[ -z "$JP" ] && { log "  !!! AUTH/WARMUP FAILED — no user record after 60s"; jail_zmx history "$NAME" 2>/dev/null|strip|grep -v '^[[:space:]]*$'|tail -20|sed 's/^/    /'|tee -a "$EV"; exit 3; }
ws "$NAME" idle 60 || true; sleep 1
log "  (a) GREEN: turns flowing; \$JP=$JP (urc=$(urc "$JP"))"; mark G

# --- (b) send:pty --wait #1 (idle) -----------------------------------------
log ""
log "=== (b) send:pty --wait #1 (idle), --timeout 60 ==="
ws "$NAME" idle 40 || log "  (b) WARN not idle pre-wait"
jail_assert_resolves_in_jail "$NAME" || { log "  RESOLUTION BELT REFUSED (b)"; exit 4; }
out="$(tmo 90 "$JAIL_SB_CMD" send:pty "$NAME" "Reply with exactly: WAITREPLY_ONE and nothing else." --wait --timeout 60 2>&1)"; rc=$?
log "  (b) --wait rc=$rc"
log "  (b) reply text:"; printf '%s\n' "$out"|sed 's/^/      /'|tee -a "$EV"
if [ "$rc" = 0 ] && printf '%s' "$out"|grep -q "WAITREPLY_ONE"; then
    log "  (b) GREEN: idle --wait returned WAITREPLY_ONE, exit 0"; mark G
else
    log "  (b) RED: rc=$rc / WAITREPLY_ONE absent"; mark R
    jail_zmx history "$NAME" 2>/dev/null|strip|grep -v '^[[:space:]]*$'|tail -10|sed 's/^/      /'|tee -a "$EV"
fi
ws "$NAME" idle 60 || true

# --- (c) send:pty --wait #2 (busy) -----------------------------------------
log ""
log "=== (c) send:pty --wait #2 (busy): queued-then-answered attribution ==="
ws "$NAME" idle 40 || log "  (c) WARN not idle pre-busy"
"$JAIL_SB_CMD" send:pty "$NAME" "Count slowly from 1 to 20, one number per line, pausing, then say BUSYDONE." >/dev/null 2>&1
if ws "$NAME" busy 15; then
    log "  (c) session busy; issuing send:pty --wait WHILE busy"
    out="$(tmo 180 "$JAIL_SB_CMD" send:pty "$NAME" "Reply with exactly: WAITREPLY_TWO and nothing else." --wait --timeout 120 2>&1)"; rc=$?
    log "  (c) busy --wait rc=$rc"
    log "  (c) reply text:"; printf '%s\n' "$out"|sed 's/^/      /'|tee -a "$EV"
    if [ "$rc" = 0 ] && printf '%s' "$out"|grep -q "WAITREPLY_TWO"; then
        log "  (c) GREEN: queued-then-answered — reply attributed to OUR message (WAITREPLY_TWO), exit 0"; mark G
    else
        log "  (c) RED: rc=$rc / WAITREPLY_TWO absent (attribution unconfirmed)"; mark R
        jail_zmx history "$NAME" 2>/dev/null|strip|grep -v '^[[:space:]]*$'|tail -12|sed 's/^/      /'|tee -a "$EV"
    fi
else
    log "  (c) RED: long prompt never drove busy in 15s (status=$(status_of "$NAME"))"; mark R
fi
ws "$NAME" idle 90 || true

# --- (d) qd wait x3 --------------------------------------------------------
log ""
log "=== (d) qd wait x3 ==="
# d1: busy -> ' done' exit 0
ws "$NAME" idle 40 || log "  (d1) WARN not idle pre"
"$JAIL_SB_CMD" send:pty "$NAME" "Count slowly from 1 to 15, one per line, then say WAITDONE_1." >/dev/null 2>&1
if ws "$NAME" busy 15; then
    out="$(tmo 180 "$JAIL_SB_CMD" wait "$NAME" 2>&1)"; rc=$?
    log "  (d1) qd wait (busy) rc=$rc out=[$(printf '%s' "$out"|tr '\n' '|')]"
    { [ "$rc" = 0 ] && printf '%s' "$out"|grep -qi "done"; } && { log "  (d1) GREEN: busy -> ' done' exit 0"; mark G; } || { log "  (d1) RED"; mark R; }
else
    log "  (d1) RED: could not drive busy"; mark R
fi
ws "$NAME" idle 60 || true
# d2: idle-at-entry -> 'is idle' exit 0
ws "$NAME" idle 40 || log "  (d2) WARN not idle pre"
out="$(tmo 20 "$JAIL_SB_CMD" wait "$NAME" 2>&1)"; rc=$?
log "  (d2) qd wait (idle) rc=$rc out=[$(printf '%s' "$out"|tr '\n' '|')]"
{ [ "$rc" = 0 ] && printf '%s' "$out"|grep -qi "idle"; } && { log "  (d2) GREEN: idle-at-entry -> 'is idle' exit 0"; mark G; } || { log "  (d2) RED"; mark R; }
# d3: --timeout 5 kept-busy -> ' timeout' exit 1
ws "$NAME" idle 40 || log "  (d3) WARN not idle pre"
"$JAIL_SB_CMD" send:pty "$NAME" "Count VERY slowly from 1 to 60, one number per line, pause between each, then say LONGDONE." >/dev/null 2>&1
if ws "$NAME" busy 15; then
    out="$(tmo 20 "$JAIL_SB_CMD" wait "$NAME" --timeout 5 2>&1)"; rc=$?
    log "  (d3) qd wait --timeout 5 (kept busy) rc=$rc out=[$(printf '%s' "$out"|tr '\n' '|')]"
    { [ "$rc" = 1 ] && printf '%s' "$out"|grep -qi "timeout"; } && { log "  (d3) GREEN: kept-busy --timeout 5 -> ' timeout' exit 1"; mark G; } || { log "  (d3) RED (rc=$rc)"; mark R; }
else
    log "  (d3) RED: could not drive busy"; mark R
fi
ws "$NAME" idle 120 || true; sleep 2

# --- (e) R4 LIVE CONFIRM: >=4.2KB idle-path paste lands (DELTA==1) ----------
log ""
log "=== (e) R4 LIVE CONFIRM: >=4.2KB idle-path single paste -> DELTA==1 + busy ==="
mkp(){ python3 -c 'import sys;n=int(sys.argv[1]);print("PASTE_START "+("lorem ipsum dolor sit amet "*((n//27)+1))[:n-20]+" PASTE_END")' "$1"; }
ws "$NAME" idle 60 || log "  (e) WARN not idle pre-paste"
jail_assert_resolves_in_jail "$NAME" || { log "  RESOLUTION BELT REFUSED (e)"; exit 4; }
JP="$(jp_of)"   # re-resolve current transcript
before="$(urc "$JP")"
P="$(mkp 4300)"; plen="$(printf '%s' "$P"|wc -c|tr -d ' ')"
log "  (e) pasting $plen bytes (>=4.2KB); instruct reply PASTEACK_R4"
busy_seen=0
out="$("$JAIL_SB_CMD" send:pty "$NAME" "$P -- after reading the above, reply with exactly PASTEACK_R4 and nothing else." 2>&1)"; rc=$?
log "  (e) send rc=$rc out=[$(printf '%s' "$out"|tr '\n' '|')]"
if ws "$NAME" busy 12; then busy_seen=1; log "  (e) session WENT BUSY (paste accepted on the idle two-write path)"; else log "  (e) WARN: did not observe busy in 12s"; fi
ws "$NAME" idle 120 || log "  (e) WARN not idle 120s post-paste"
sleep 3
JP="$(jp_of)"
after="$(urc "$JP")"; delta=$((after-before))
log "  (e) user-records before=$before after=$after DELTA=$delta busy_seen=$busy_seen status=$(status_of "$NAME")"
# byte capture for the record
log "  (e) JSONL tail (last user/assistant records):"
python3 -c 'import json,sys
recs=[]
for l in open(sys.argv[1]):
 l=l.strip()
 if not l:continue
 try:r=json.loads(l)
 except:continue
 t=r.get("type")
 if t in ("user","assistant"):
  msg=r.get("message",{})
  c=msg.get("content","")
  if isinstance(c,list):
   c="".join(x.get("text","") if isinstance(x,dict) else str(x) for x in c)
  recs.append((t,(str(c)[:80]).replace("\n"," ")))
for t,c in recs[-4:]:
 print("      [%s] %s"%(t,c))' "$JP" 2>/dev/null | tee -a "$EV"
log "  (e) zmx history tail:"
jail_zmx history "$NAME" 2>/dev/null|strip|grep -v '^[[:space:]]*$'|tail -6|sed 's/^/      /'|tee -a "$EV"
if [ "$delta" = 1 ] && [ "$busy_seen" = 1 ]; then
    log "  (e) GREEN: R4 FIXED LIVE — >=4.2KB paste LANDS (DELTA=1) + went busy. Was RED in M5."; mark G
else
    log "  (e) RED: DELTA=$delta busy_seen=$busy_seen (expected DELTA=1 + busy)"; mark R
fi

# --- TEARDOWN + BELT -------------------------------------------------------
"$JAIL_SB_CMD" ls --all --short 2>/dev/null | grep -q "$NAME" && jail_kill_session "$NAME" >/dev/null 2>&1
"$JAIL_ZMX_CMD" kill "$NAME" --force >/dev/null 2>&1 || true
sleep 1
ra="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
leaked="$(grep -l "$NAME" "$REAL_SESS"/*.json 2>/dev/null || true)"
log ""
log "=== REAL-HOME BELT after: $ra ($([ "$rb" = "$ra" ]&&echo HOLDS||echo VIOLATION)) ==="
[ -n "$leaked" ] && log "  !!! BELT VIOLATION (leaked): $leaked"

log ""
log "=== BOOT-#6 TALLY ==="
log "  rows attempted: 6 (a warm-up, b/c --wait x2, d qd-wait x3 counted as one group of 3, e R4)"
log "  GREEN: $GREEN   RED: $RED"
log "=== DONE (teardown via trap) ==="
