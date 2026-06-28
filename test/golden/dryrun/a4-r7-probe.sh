#!/bin/bash
# a4-r7-probe.sh — A4 R7 LIVE CONFIRM: do the R6-failure rows (12KB, 16KB on the
# send:pty idle path) now DELIVER on the MERGED chunked-delivery binary?
# (orc-3 ruling, relay-1780658366989-17 ask 2.)
#
# R6 found (probe/a4-r6-live, commit 8ca5987, on the pre-fix two-write binary):
#   12KB send:pty idle -> delta 0, EMPTY-DROPPED, did-not-go-busy WARNING, exit 0
#   16KB send:pty idle -> delta 0, EMPTY-DROPPED, did-not-go-busy WARNING, exit 0
# The merged fix (60fe8a7, in PR #13 / merge 37881b1): chunk_text + send_text_chunked
# in the SHARED write layer — PTY text split into <=1024B code-point-safe chunks, 150ms
# inter-chunk settle, so a large write no longer overflows the ~4096B canonical tty
# queue. This probe RE-RUNS the two R6-failure sizes on the FIXED binary.
#
# EXPECT per row: delta=1, DELIVERED, went busy, verb exit 0 — the fix working.
# If EITHER row still drops: capture everything, classify, do NOT improvise fixes;
# report the red verbatim.
#
# Method: ported EXACTLY from a4-r6-probe.sh (sendpty mode):
#   - seed = M5 full recipe (probe-3 GrowthBook + onboarding + auth, READ-ONLY)
#   - PERL-ALARM wrapper (NO timeout(1) on macOS)
#   - glob-fallback $JP resolver (re-resolved after warm-up + before each row)
#   - resolution belt before every session-targeting row
#   - REAL-HOME BELT before/after
#   - composer-state classification per row: DELIVERED / STUCK-IN-COMPOSER / EMPTY-DROPPED
#   - UNIQUE markers (R7_12K_<rand> etc.) + scattered multibyte UTF-8
#   - ONE real-claude boot: warm-up + 12KB + 16KB (the R6 EMPTY-DROPPED sizes).
set -u
WT="$(cd "$(dirname "$0")/../../.." && pwd -P)"
cd "$WT" || exit 1
export JAIL_QD_CMD="$WT/target/debug/qd"
export JAIL_ZMX_CMD="/opt/homebrew/bin/zmx"
. test/golden/lib/jail.sh

EV="$WT/test/golden/dryrun/a4-r7-bytes.txt"
log(){ printf '%s\n' "$*" | tee -a "$EV"; }
strip(){ perl -pe 's/\e\][0-9].*?(\a|\e\\)//g; s/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\e[()][B0]//g; s/\r//g'; }
tmo(){ local s="$1"; shift; perl -e 'my $t=shift; my $pid=fork; if($pid==0){exec @ARGV or exit 127} local $SIG{ALRM}=sub{kill 9,$pid; exit 124}; alarm $t; waitpid($pid,0); exit($?>>8)' "$s" "$@"; }

log ""
log "=== A4 R7 LIVE CONFIRM (12KB + 16KB send:pty idle on the FIXED binary) ==="
log "  date: $(date '+%Y-%m-%d %H:%M:%S %Z')"
log "  worktree: $WT  | tip: $(git -C "$WT" log -1 --format='%h %s' 2>/dev/null)"
log "  binary: $JAIL_QD_CMD ($($JAIL_QD_CMD --version 2>/dev/null))"

jail_establish || { echo FATAL; exit 1; }
trap jail_teardown EXIT
REAL_SESS="$JAIL_REAL_HOME/.claude/sessions"
rb="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
log "=== REAL-HOME BELT before: $rb ==="

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

mkp(){ python3 -c '
import sys
n=int(sys.argv[1]); marker=sys.argv[2]
uni="日本語café☕"
head=marker+"_START "
tail=" "+marker+"_END"
body=[]
filler="lorem ipsum dolor sit amet consectetur "
cur=len(head.encode())+len(tail.encode())
i=0
while cur < n:
    chunk = filler if (i%3) else (uni+" ")
    b=chunk.encode()
    body.append(chunk); cur+=len(b); i+=1
s=head+"".join(body)+tail
enc=s.encode()[:n]
while True:
    try: s2=enc.decode(); break
    except UnicodeDecodeError: enc=enc[:-1]
if marker+"_END" not in s2:
    s2 = s2.rstrip()+tail
sys.stdout.write(s2)
' "$1" "$2"; }

classify(){ local name="$1" marker="$2" delta="$3"
    if [ "$delta" -ge 1 ]; then echo "DELIVERED"; return; fi
    local hist; hist="$(jail_zmx history "$name" 2>/dev/null|strip)"
    if printf '%s' "$hist" | grep -q "${marker}_START"; then
        echo "STUCK-IN-COMPOSER"
    else
        echo "EMPTY-DROPPED"
    fi
}

GREEN=0; RED=0; ROWS=""
mark(){ if [ "$1" = G ]; then GREEN=$((GREEN+1)); else RED=$((RED+1)); fi; }
addrow(){ ROWS="${ROWS}$1
"; }

NAME="${JAIL_PREFIX}r7"
log ""
log "=== qd new (real claude) ==="
( cd "$WORKDIR" && "$JAIL_QD_CMD" new "$NAME" --cwd "$WORKDIR" ) >"$JAIL_ROOT/o" 2>"$JAIL_ROOT/e"
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

log ""
log "=== warm-up send:pty (turns flowing; resolve \$JP via glob-fallback) ==="
out="$("$JAIL_QD_CMD" send:pty "$NAME" "Reply with exactly: AUTHOK and nothing else." 2>&1)"
log "  warm-up send out=[$out]"
i=0; JP=""; while [ "$i" -lt 120 ]; do JP="$(jp_of)"; [ -n "$JP" ]&&[ "$(urc "$JP")" -ge 1 ]&&break; sleep 0.5; i=$((i+1)); done
[ -z "$JP" ] && { log "  !!! AUTH/WARMUP FAILED — no user record after 60s"; jail_zmx history "$NAME" 2>/dev/null|strip|grep -v '^[[:space:]]*$'|tail -20|sed 's/^/    /'|tee -a "$EV"; exit 3; }
ws "$NAME" idle 60 || true; sleep 1
log "  warm-up OK: \$JP=$JP (urc=$(urc "$JP"))"

sendpty_row(){
    local sz="$1" marker="$2" before after delta P plen busy_seen=0 cls vrc
    log ""
    log "===== send:pty ROW: target=$sz bytes marker=$marker ====="
    ws "$NAME" idle 90 || log "  WARN not idle pre-row"
    jail_assert_resolves_in_jail "$NAME" || { log "  RESOLUTION BELT REFUSED (row $sz)"; return 2; }
    JP="$(jp_of)"; before="$(urc "$JP")"
    P="$(mkp "$sz" "$marker")"; plen="$(printf '%s' "$P"|wc -c|tr -d ' ')"
    log "  payload actual bytes=$plen (target $sz); marker=$marker"
    "$JAIL_QD_CMD" send:pty "$NAME" "$P" >"$JAIL_ROOT/row.out" 2>"$JAIL_ROOT/row.err"; vrc=$?
    log "  verb exit=$vrc"
    log "  verb stdout: $(cat "$JAIL_ROOT/row.out" 2>/dev/null|tr '\n' '|')"
    if [ -s "$JAIL_ROOT/row.err" ]; then log "  verb stderr:"; cat "$JAIL_ROOT/row.err"|sed 's/^/    /'|tee -a "$EV"; else log "  verb stderr: (none)"; fi
    if ws "$NAME" busy 12; then busy_seen=1; log "  WENT BUSY within 12s"; else log "  did NOT observe busy in 12s (status=$(status_of "$NAME"))"; fi
    ws "$NAME" idle 150 || log "  WARN not idle 150s post-row"
    sleep 3
    JP="$(jp_of)"; after="$(urc "$JP")"; delta=$((after-before))
    cls="$(classify "$NAME" "$marker" "$delta")"
    log "  user-records before=$before after=$after DELTA=$delta busy_seen=$busy_seen status=$(status_of "$NAME") CLASS=$cls"
    log "  JSONL tail (last 4 user/assistant):"
    python3 -c 'import json,sys
recs=[]
for l in open(sys.argv[1]):
 l=l.strip()
 if not l:continue
 try:r=json.loads(l)
 except:continue
 t=r.get("type")
 if t in ("user","assistant"):
  msg=r.get("message",{}); c=msg.get("content","")
  if isinstance(c,list): c="".join(x.get("text","") if isinstance(x,dict) else str(x) for x in c)
  recs.append((t,(str(c)[:90]).replace("\n"," ")))
for t,c in recs[-4:]: print("      [%s] %s"%(t,c))' "$JP" 2>/dev/null | tee -a "$EV"
    log "  zmx history tail (ANSI-stripped, last 8 non-blank):"
    jail_zmx history "$NAME" 2>/dev/null|strip|grep -v '^[[:space:]]*$'|tail -8|sed 's/^/      /'|tee -a "$EV"
    addrow "send:pty | ${plen}B | delta=$delta | $cls | busy=$busy_seen | exit=$vrc"
    if [ "$cls" = DELIVERED ]; then mark G; else mark R; fi
    printf '%s' "$cls" > "$JAIL_ROOT/last_class"
}

sendpty_row 12288 "R7_12K_${JAIL_RUNID}"; R12="$(cat "$JAIL_ROOT/last_class")"
sendpty_row 16384 "R7_16K_${JAIL_RUNID}"; R16="$(cat "$JAIL_ROOT/last_class")"
log ""
log "  >>> 12KB class=$R12   16KB class=$R16"

"$JAIL_QD_CMD" ls --all --short 2>/dev/null | grep -q "$NAME" && jail_kill_session "$NAME" >/dev/null 2>&1
"$JAIL_ZMX_CMD" kill "$NAME" --force >/dev/null 2>&1 || true
sleep 1
ra="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
leaked="$(grep -l "${JAIL_PREFIX}" "$REAL_SESS"/*.json 2>/dev/null || true)"
log ""
log "=== REAL-HOME BELT after: $ra ($([ "$rb" = "$ra" ]&&echo HOLDS||echo VIOLATION)) ==="
[ -n "$leaked" ] && log "  !!! BELT VIOLATION (leaked prefixed rows): $leaked"

log ""
log "=== R7 ROW SUMMARY ==="
printf '%s' "$ROWS" | sed 's/^/  /' | tee -a "$EV"
log "  GREEN(delivered): $GREEN   RED(drop/stuck): $RED"
log "=== DONE (teardown via trap) ==="
