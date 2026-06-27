#!/bin/bash
# a4-paste-investigate.sh — A4 M5 controlled rerun (macOS real-claude BOOT 3).
#
# Isolates the boot-1 soak d2/d3 FINDING: 4KB single-write pastes on the IDLE
# send:pty path reported "did not go busy" + DELTA=0 (vs 1KB d1 = DELTA=1).
# This boot: clean session, auth warm-up, then ONLY the paste rows -- 1KB
# (control, expect 1 turn), 4KB, 4.5KB -- with full scrollback capture after
# each, plus a manual recovery probe (does a follow-up CR submit the stuck
# composer?). Provides a no-timeout-binary wrapper (macOS lacks `timeout`).
set -u
WT=/home/u/work/wt-a4-lead
cd "$WT" || exit 1
export JAIL_SB_CMD="$WT/target/debug/qd"
export JAIL_ZMX_CMD="/opt/homebrew/bin/zmx"
. test/golden/lib/jail.sh
EV="$WT/test/golden/dryrun/a4-paste-bytes.txt"; : > "$EV"
log(){ printf '%s\n' "$*" | tee -a "$EV"; }
strip(){ perl -pe 's/\e\][0-9].*?(\a|\e\\)//g; s/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\e[()][B0]//g; s/\r//g'; }
# perl-alarm timeout wrapper (macOS has no `timeout`)
tmo(){ local s="$1"; shift; perl -e 'my $t=shift; my $pid=fork; if($pid==0){exec @ARGV or exit 127} local $SIG{ALRM}=sub{kill 9,$pid; exit 124}; alarm $t; waitpid($pid,0); exit($?>>8)' "$s" "$@"; }

jail_establish || { echo FATAL; exit 1; }
trap jail_teardown EXIT
REAL_SESS="$JAIL_REAL_HOME/.claude/sessions"
rb="$(ls "$REAL_SESS" 2>/dev/null|wc -l|tr -d ' ')"
log "=== REAL-HOME BELT before: $rb ==="

mkdir -p "$HOME/.claude/channels" "$HOME/.claude/sessions"
ln -s /home/u/work/cc-relay "$HOME/.claude/channels/relay" 2>/dev/null || true
GB="$(python3 -c 'import json,sys;print(json.dumps(json.load(open(sys.argv[1])).get("cachedGrowthBookFeatures",{})))' "$JAIL_REAL_HOME/.claude.json" 2>/dev/null||echo '{}')"
cp "$JAIL_REAL_HOME/.claude/.credentials.json" "$HOME/.claude/.credentials.json" 2>/dev/null && chmod 600 "$HOME/.claude/.credentials.json"||true
AUTH="$(python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));print(json.dumps({k:d[k] for k in ("oauthAccount","userID","claudeCodeFirstTokenDate") if k in d}))' "$JAIL_REAL_HOME/.claude.json" 2>/dev/null||echo '{}')"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"; RW="$(cd "$WORKDIR"&&pwd -P)"
python3 -c 'import json,sys
b={"hasCompletedOnboarding":True,"bypassPermissionsModeAccepted":True,"dangerouslyLoadDevelopmentChannels":True}
b["cachedGrowthBookFeatures"]=json.loads(sys.argv[1]); b["projects"]={sys.argv[3]:{"hasTrustDialogAccepted":True},sys.argv[4]:{"hasTrustDialogAccepted":True}}
b.update(json.loads(sys.argv[2])); print(json.dumps(b))' "$GB" "$AUTH" "$WORKDIR" "$RW" > "$HOME/.claude.json"

NAME="${JAIL_PREFIX}paste"
pidfile_for(){ local f; for f in "$HOME/.claude/sessions"/*.json; do grep -q "\"name\":\"$1\"" "$f" 2>/dev/null && { echo "$f"; return; }; done; return 1; }
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

log "=== BOOT 3 (paste investigation) ==="
( cd "$WORKDIR" && "$JAIL_SB_CMD" new "$NAME" --cwd "$WORKDIR" ) >"$JAIL_ROOT/o" 2>"$JAIL_ROOT/e"
log "  qd new exit=$? : $(cat "$JAIL_ROOT/o")"
ws "$NAME" idle 30||log "  WARN not idle"
# RESOLUTION BELT (orc-2 ruling): every send/kill-by-name op asserts the name
# resolves to EXACTLY ONE session in the jailed zmx dir first.
jail_assert_resolves_in_jail "$NAME" || { log "RESOLUTION BELT REFUSED for $NAME"; exit 4; }
log "  resolution belt: $NAME resolves uniquely in-jail (OK)"
# warm-up to create transcript + confirm auth
"$JAIL_SB_CMD" send:pty "$NAME" "Reply with exactly: AUTHOK" >/dev/null 2>&1
i=0; JP=""; while [ "$i" -lt 90 ]; do JP="$(jp_of)"; [ -n "$JP" ]&&[ "$(urc "$JP")" -ge 1 ]&&break; sleep 0.5; i=$((i+1)); done
[ -z "$JP" ]&&{ log "AUTH FAILED"; exit 3; }
log "  authed; JP=$JP"
ws "$NAME" idle 60||true; sleep 1

mkp(){ python3 -c 'import sys;n=int(sys.argv[1]);print("PASTE_START "+("lorem ipsum dolor sit amet "*((n//27)+1))[:n-20]+" PASTE_END")' "$1"; }
probe(){  # size
    local sz="$1" before after delta P plen
    ws "$NAME" idle 40||log "  WARN not idle pre-paste $sz"
    jail_assert_resolves_in_jail "$NAME" || { log "  RESOLUTION BELT REFUSED for $NAME (probe $sz)"; return 1; }
    before="$(urc "$JP")"
    P="$(mkp "$sz")"; plen="$(printf '%s' "$P"|wc -c|tr -d ' ')"
    log ""
    log "--- PASTE $plen bytes ---"
    out="$("$JAIL_SB_CMD" send:pty "$NAME" "$P -- then reply exactly PA_$sz" 2>&1)"
    log "  send out=[$(printf '%s' "$out"|tr '\n' '|')]"
    ws "$NAME" busy 8 >/dev/null 2>&1||true
    ws "$NAME" idle 60||log "  WARN not idle 60s post-paste"
    sleep 2
    after="$(urc "$JP")"; delta=$((after-before))
    log "  user-records before=$before after=$after DELTA=$delta status=$(status_of "$NAME")"
    if [ "$delta" -ge 1 ]; then
        log "  RESULT $sz: SUBMITTED (delta=$delta)"
    else
        log "  RESULT $sz: NOT SUBMITTED (delta=0) — composer state:"
        jail_zmx history "$NAME" 2>/dev/null|strip|grep -v '^[[:space:]]*$'|tail -8|sed 's/^/      /'|tee -a "$EV"
        log "  RECOVERY PROBE: does a manual follow-up CR submit the stuck composer?"
        jail_zmx send "$NAME" "" >/dev/null 2>&1   # bare CR via zmx
        $JAIL_ZMX_CMD send "$NAME" $'\r' >/dev/null 2>&1 || true
        ws "$NAME" busy 8 >/dev/null 2>&1||true
        ws "$NAME" idle 60||true; sleep 2
        local r2; r2="$(urc "$JP")"
        log "  after manual CR: user-records=$r2 (delta-from-before=$((r2-before)))"
        [ "$((r2-before))" -ge 1 ] && log "  -> RECOVERABLE: a follow-up CR submits it (idle-path single-CR remediation under-fired for this size)" \
                                   || log "  -> still stuck after manual CR"
    fi
}
probe 1100   # control
probe 4200
probe 4500

log ""
log "=== TALLY ==="
ra="$(ls "$REAL_SESS" 2>/dev/null|wc -l|tr -d ' ')"
log "  REAL-HOME BELT: $rb -> $ra $([ "$rb" = "$ra" ]&&echo HOLDS||echo VIOLATION)"
log "=== DONE (teardown via trap) ==="
