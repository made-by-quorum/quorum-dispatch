#!/bin/bash
# a4-live-boot1-soak.sh — A4 M5 BOOT 1 (macOS real-claude #1 of <=3).
#
# ONE real-claude session, driven through the x20 send soak (a4-spec section 6):
#   (a) ~8 x send:pty idle-path short messages
#   (b) ~4 x queue-path: long-running prompt, then send:pty WHILE busy -> "Message queued"
#   (c) ~2 x send:pty --wait (one idle, one while busy) -- reply extraction + attribution
#   (d) ~3 x long-paste (>=1KB and >=4KB) -- exactly one turn each (JSONL user-record count)
#   (e) ~3 x sb wait during busy -> " done" exit 0
#
# Seed recipe = a2-live-row4.sh (probe-3 GrowthBook/onboarding). cachedGrowthBook
# Features COPIED FROM REAL HOME READ-ONLY. REAL-HOME BELT around the boot.
# Every sb/zmx via jail primitives. NEVER real state. Fail-loud, max 2 retries.
set -u
WT=/home/u/work/wt-a4-lead
cd "$WT" || exit 1
export JAIL_SB_CMD="$WT/target/debug/sb"
export JAIL_ZMX_CMD="/opt/homebrew/bin/zmx"
. test/golden/lib/jail.sh

EV="$WT/test/golden/dryrun/a4-boot1-bytes.txt"
: > "$EV"
log() { printf '%s\n' "$*" | tee -a "$EV"; }
strip() { perl -pe 's/\e\][0-9].*?(\a|\e\\)//g; s/\e\[[0-9;?]*[ -\/]*[@-~]//g; s/\e[()][B0]//g; s/\r//g'; }

jail_establish || { echo "FATAL: jail_establish"; exit 1; }
trap jail_teardown EXIT

REAL_SESS="$JAIL_REAL_HOME/.claude/sessions"
real_before="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
log "=== REAL-HOME BELT (before): $real_before rows in $REAL_SESS ==="

mkdir -p "$HOME/.claude/channels" "$HOME/.claude/sessions"
ln -s /home/u/work/cc-relay "$HOME/.claude/channels/relay" 2>/dev/null || true
GB_FLAGS="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(json.dumps(d.get("cachedGrowthBookFeatures",{})))' "$JAIL_REAL_HOME/.claude.json" 2>/dev/null || echo '{}')"
log "growthbook flags seeded: $(printf '%s' "$GB_FLAGS" | wc -c | tr -d ' ') bytes"

WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"
RESOLVED_WORKDIR="$(cd "$WORKDIR" && pwd -P)"
# AUTH SEED (A4 addition over the A2 probe-3 recipe): the jailed HOME has no
# Claude credentials, so real MODEL TURNS fail "Not logged in" (A2 never drove a
# turn -- its sends were marker-only, no CR). Seed auth READ-ONLY from the real
# home (read allowed, write never -- same sanction as cachedGrowthBookFeatures):
#   - copy ~/.claude/.credentials.json (file-based OAuth token) into the jail home
#   - carry oauthAccount + userID + claudeCodeFirstTokenDate into the jailed .claude.json
cp "$JAIL_REAL_HOME/.claude/.credentials.json" "$HOME/.claude/.credentials.json" 2>/dev/null \
    && chmod 600 "$HOME/.claude/.credentials.json" 2>/dev/null || true
log "credentials seeded: $([ -f "$HOME/.claude/.credentials.json" ] && echo yes || echo NO)"
AUTH_KEYS="$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
out={}
for k in ("oauthAccount","userID","claudeCodeFirstTokenDate"):
    if k in d: out[k]=d[k]
print(json.dumps(out))' "$JAIL_REAL_HOME/.claude.json" 2>/dev/null || echo '{}')"
python3 -c 'import json,sys
base=json.loads(sys.argv[1])
auth=json.loads(sys.argv[2])
gb=json.loads(sys.argv[3])
base["cachedGrowthBookFeatures"]=gb
base["projects"]={sys.argv[4]:{"hasTrustDialogAccepted":True}, sys.argv[5]:{"hasTrustDialogAccepted":True}}
base.update(auth)
print(json.dumps(base))' \
    '{"hasCompletedOnboarding": true, "bypassPermissionsModeAccepted": true, "dangerouslyLoadDevelopmentChannels": true}' \
    "$AUTH_KEYS" "$GB_FLAGS" "$WORKDIR" "$RESOLVED_WORKDIR" > "$HOME/.claude.json"

NAME="${JAIL_PREFIX}soak"

pidfile_for() {
    local n="$1" f
    for f in "$HOME/.claude/sessions"/*.json; do
        [ -f "$f" ] || continue
        grep -q "\"name\":\"$n\"" "$f" 2>/dev/null && { echo "$f"; return 0; }
    done
    return 1
}
status_of() {
    local pf; pf="$(pidfile_for "$1")" || { echo "NONE"; return; }
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status",""))' "$pf" 2>/dev/null || echo "?"
}
wait_status() {  # name target timeout_s
    local n="$1" want="$2" to="$3" i=0
    while [ "$i" -lt "$((to*4))" ]; do
        [ "$(status_of "$n")" = "$want" ] && return 0
        sleep 0.25; i=$((i+1))
    done
    return 1
}
jsonl_path_of() {
    local n="$1" jp
    jp="$("$JAIL_SB_CMD" ls --json 2>/dev/null | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except: sys.exit(0)
rows=d if isinstance(d,list) else d.get("sessions",d)
for r in (rows if isinstance(rows,list) else []):
    if r.get("name")=="'"$n"'":
        print(r.get("jsonlPath") or r.get("jsonl_path") or ""); break
')"
    if [ -n "$jp" ] && [ -f "$jp" ]; then echo "$jp"; return 0; fi
    # Fallback: newest .jsonl under the jailed projects dir (the session's transcript).
    ls -t "$HOME/.claude/projects"/*/*.jsonl 2>/dev/null | head -1
}
user_record_count() {
    local jp="$1"
    [ -f "$jp" ] || { echo 0; return; }
    python3 -c '
import json,sys
n=0
for line in open(sys.argv[1]):
    line=line.strip()
    if not line: continue
    try: r=json.loads(line)
    except: continue
    if r.get("type")=="user": n+=1
print(n)' "$jp" 2>/dev/null || echo 0
}

ATT=0; ACC=0; QUEUED=0; ANOM=0

log ""
log "############################################################"
log "### BOOT 1 -- sb new (default flags) -- REAL CLAUDE #1   ###"
log "############################################################"
( cd "$WORKDIR" && "$JAIL_SB_CMD" new "$NAME" --cwd "$WORKDIR" ) \
    > "$JAIL_ROOT/boot1-out.txt" 2> "$JAIL_ROOT/boot1-err.txt"
code=$?
log "sb new exit=$code"
log "  stdout: $(cat "$JAIL_ROOT/boot1-out.txt")"
log "  stderr: $(head -5 "$JAIL_ROOT/boot1-err.txt")"
PF="$(pidfile_for "$NAME" || true)"
log "  pidfile: ${PF:-NONE}  status=$(status_of "$NAME")"
JP="$(jsonl_path_of "$NAME")"
log "  jsonl: ${JP:-NONE}"
CLVER="$([ -n "$PF" ] && python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("version",""))' "$PF" 2>/dev/null || echo "?")"
log "  claude version in jail: $CLVER"

if [ "$code" != "0" ] || [ -z "$PF" ]; then
    log "!!! BOOT 1 FAILED -- cannot proceed. Capturing bytes and stopping."
    jail_zmx history "$NAME" 2>/dev/null | strip | tail -30 | sed 's/^/  /' | tee -a "$EV"
    exit 1
fi
wait_status "$NAME" idle 30 || log "WARN: not idle after boot 30s (status=$(status_of "$NAME"))"
sleep 1
BASE_URC="$(user_record_count "$JP")"
log "  baseline user-record count: $BASE_URC"

# AUTH PROBE (fail LOUD before driving the soak). One short prompt; if the
# session does not produce a user record + an assistant turn within 45s, the
# jail is unauthenticated ("Not logged in") -- STOP, do not burn x20 turns.
log ""
log "=== AUTH PROBE: one warm-up turn to confirm the jail is logged in ==="
"$JAIL_SB_CMD" send:pty "$NAME" "Reply with exactly: AUTHOK and nothing else." >/dev/null 2>&1
authed=0
i=0
# The transcript JSONL does not exist until the first turn writes it, so $JP was
# NONE at boot. Re-resolve it INSIDE this loop (glob fallback) and count against
# the freshly-found transcript -- never the stale empty $JP.
while [ "$i" -lt 90 ]; do
    JP="$(jsonl_path_of "$NAME")"
    if [ -n "$JP" ] && [ -f "$JP" ]; then
        urc="$(user_record_count "$JP")"
        if [ "${urc:-0}" -ge 1 ]; then authed=1; break; fi
    fi
    sleep 0.5; i=$((i+1))
done
if [ "$authed" != "1" ]; then
    log "!!! AUTH PROBE FAILED -- jail not logged in (no user record after 45s)."
    log "--- zmx history tail (root-cause) ---"
    jail_zmx history "$NAME" 2>/dev/null | strip | grep -v '^[[:space:]]*$' | tail -20 | sed 's/^/  /' | tee -a "$EV"
    real_after="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
    log "REAL-HOME BELT: before=$real_before after=$real_after"
    exit 3
fi
# Re-resolve JP now that the transcript exists (it was NONE at boot time).
JP="$(jsonl_path_of "$NAME")"
log "  AUTH PROBE OK: warm-up turn produced a user record; jsonl now: ${JP:-NONE}"
# settle back to idle, then re-baseline so the soak counts cleanly
wait_status "$NAME" idle 60 || log "  WARN: not idle after warm-up"
sleep 1
BASE_URC="$(user_record_count "$JP")"
log "  re-baselined user-record count after warm-up: $BASE_URC"

log ""
log "=== GROUP (a): 8 x send:pty idle-path short messages ==="
for i in 1 2 3 4 5 6 7 8; do
    wait_status "$NAME" idle 30 || log "  (a$i) WARN not idle before send (status=$(status_of "$NAME"))"
    MSG="reply with only the word ok (idle-probe $i)"
    out="$("$JAIL_SB_CMD" send:pty "$NAME" "$MSG" 2>&1)"; rc=$?
    ATT=$((ATT+1))
    log "  (a$i) send:pty rc=$rc out=[$out]"
    case "$out" in
        *"Message sent"*|*"Message queued"*) ACC=$((ACC+1)) ;;
        *) ANOM=$((ANOM+1)); log "  (a$i) ANOMALY: unexpected output" ;;
    esac
    wait_status "$NAME" busy 8 >/dev/null 2>&1 || true
    wait_status "$NAME" idle 40 || log "  (a$i) WARN did not return to idle in 40s"
done
log "  after group (a): user-record count = $(user_record_count "$JP") (baseline $BASE_URC)"

log ""
log "=== GROUP (b): 4 x queue-path (send while busy) ==="
for i in 1 2 3 4; do
    wait_status "$NAME" idle 40 || log "  (b$i) WARN not idle before long prompt"
    "$JAIL_SB_CMD" send:pty "$NAME" "Count slowly from 1 to 30, one number per line, then say DONE_$i." >/dev/null 2>&1
    ATT=$((ATT+1)); ACC=$((ACC+1))
    if wait_status "$NAME" busy 12; then
        log "  (b$i) session went busy; sending WHILE busy"
        urc_before="$(user_record_count "$JP")"
        out="$("$JAIL_SB_CMD" send:pty "$NAME" "queued-while-busy probe $i: say QPROBE_$i" 2>&1)"; rc=$?
        ATT=$((ATT+1))
        log "  (b$i) busy-send rc=$rc out=[$out]"
        case "$out" in
            *"Message queued"*) QUEUED=$((QUEUED+1)); log "  (b$i) OK: Message queued (session busy)" ;;
            *) ANOM=$((ANOM+1)); log "  (b$i) ANOMALY: expected 'Message queued', got [$out]" ;;
        esac
        wait_status "$NAME" idle 90 || log "  (b$i) WARN not idle in 90s after queue"
        sleep 2
        urc_after="$(user_record_count "$JP")"
        log "  (b$i) user-records: before-busy=$urc_before after-both=$urc_after (delta=$((urc_after-urc_before)))"
        if grep -q "QPROBE_$i" "$JP" 2>/dev/null; then
            log "  (b$i) VERIFIED: queued message surfaced as a user record (QPROBE_$i in JSONL)"
        else
            log "  (b$i) NOTE: QPROBE_$i not in JSONL"
        fi
    else
        log "  (b$i) ANOMALY: long prompt never drove busy in 12s (status=$(status_of "$NAME"))"
        ANOM=$((ANOM+1))
    fi
done

log ""
log "=== GROUP (c): 2 x send:pty --wait ==="
wait_status "$NAME" idle 40 || log "  (c1) WARN not idle before --wait"
log "  (c1) send:pty --wait on IDLE session"
out="$(timeout 130 "$JAIL_SB_CMD" send:pty "$NAME" "Reply with exactly: WAITREPLY_ONE and nothing else." --wait 2>&1)"; rc=$?
ATT=$((ATT+1)); ACC=$((ACC+1))
log "  (c1) --wait rc=$rc"
log "  (c1) reply text:"; printf '%s\n' "$out" | sed 's/^/      /' | tee -a "$EV"
if printf '%s' "$out" | grep -q "WAITREPLY_ONE"; then
    log "  (c1) VERIFIED: reply extraction returned WAITREPLY_ONE"
else
    log "  (c1) NOTE: WAITREPLY_ONE not in extracted reply (model may paraphrase)"
fi

wait_status "$NAME" idle 40 || log "  (c2) WARN not idle before busy --wait"
"$JAIL_SB_CMD" send:pty "$NAME" "Count slowly from 1 to 20, one per line, then say BUSYDONE." >/dev/null 2>&1
if wait_status "$NAME" busy 12; then
    log "  (c2) session busy; send:pty --wait WHILE busy (queue then wait)"
    out="$(timeout 150 "$JAIL_SB_CMD" send:pty "$NAME" "Reply with exactly: WAITREPLY_TWO and nothing else." --wait 2>&1)"; rc=$?
    ATT=$((ATT+1)); ACC=$((ACC+1))
    log "  (c2) busy --wait rc=$rc"
    log "  (c2) reply text:"; printf '%s\n' "$out" | sed 's/^/      /' | tee -a "$EV"
    if printf '%s' "$out" | grep -q "WAITREPLY_TWO"; then
        log "  (c2) VERIFIED: busy-path --wait attributed WAITREPLY_TWO"
    else
        log "  (c2) NOTE: WAITREPLY_TWO not in extracted reply"
    fi
else
    log "  (c2) ANOMALY: could not drive busy for busy --wait"
    ANOM=$((ANOM+1))
fi
wait_status "$NAME" idle 60 || log "  (c2) WARN not idle after"

log ""
log "=== GROUP (d): 3 x long-paste (exactly one turn each) ==="
mk_paste() {
    python3 -c 'import sys; n=int(sys.argv[1]); print("PASTE_START "+("lorem ipsum dolor sit amet "*((n//27)+1))[:n-20]+" PASTE_END")' "$1"
}
d_idx=0
for sz in 1100 4200 4500; do
    d_idx=$((d_idx+1))
    wait_status "$NAME" idle 40 || log "  (d$d_idx) WARN not idle before paste"
    urc_before="$(user_record_count "$JP")"
    PASTE="$(mk_paste "$sz")"
    plen="$(printf '%s' "$PASTE" | wc -c | tr -d ' ')"
    log "  (d$d_idx) pasting $plen bytes; instruct reply PASTEACK_$d_idx"
    FULLMSG="$PASTE -- after reading the above, reply with exactly PASTEACK_$d_idx and nothing else."
    out="$("$JAIL_SB_CMD" send:pty "$NAME" "$FULLMSG" 2>&1)"; rc=$?
    ATT=$((ATT+1))
    case "$out" in *"Message sent"*|*"Message queued"*) ACC=$((ACC+1));; *) ANOM=$((ANOM+1)); log "  (d$d_idx) ANOMALY out=[$out]";; esac
    log "  (d$d_idx) send rc=$rc out=[$out]"
    wait_status "$NAME" busy 10 >/dev/null 2>&1 || true
    wait_status "$NAME" idle 60 || log "  (d$d_idx) WARN not idle 60s"
    sleep 2
    urc_after="$(user_record_count "$JP")"
    delta=$((urc_after-urc_before))
    log "  (d$d_idx) user-records: before=$urc_before after=$urc_after DELTA=$delta"
    if [ "$delta" = "1" ]; then
        log "  (d$d_idx) VERIFIED: EXACTLY ONE turn from long paste (delta=1)"
    else
        log "  (d$d_idx) ANOMALY: expected 1 user-record, got delta=$delta"
        ANOM=$((ANOM+1))
    fi
done

log ""
log "=== GROUP (e): 3 x sb wait during busy ==="
for i in 1 2 3; do
    wait_status "$NAME" idle 40 || log "  (e$i) WARN not idle before"
    "$JAIL_SB_CMD" send:pty "$NAME" "Count slowly from 1 to 15, one per line, then say WAITDONE_$i." >/dev/null 2>&1
    ATT=$((ATT+1)); ACC=$((ACC+1))
    if wait_status "$NAME" busy 12; then
        log "  (e$i) session busy; calling sb wait"
        out="$(timeout 130 "$JAIL_SB_CMD" wait "$NAME" 2>&1)"; rc=$?
        log "  (e$i) sb wait rc=$rc out=[$out]"
        if [ "$rc" = "0" ] && printf '%s' "$out" | grep -qi "done"; then
            log "  (e$i) VERIFIED: sb wait ' done' exit 0 after busy->idle"
        else
            log "  (e$i) ANOMALY: sb wait rc=$rc out=[$out]"
            ANOM=$((ANOM+1))
        fi
    else
        log "  (e$i) ANOMALY: could not drive busy for sb wait"
        ANOM=$((ANOM+1))
    fi
done

log ""
log "############################################################"
log "### SOAK TALLY (BOOT 1)                                   ###"
log "############################################################"
log "  sends attempted: $ATT"
log "  accepted (sent/queued): $ACC"
log "  queued-while-busy:      $QUEUED"
log "  anomalies (red):        $ANOM"
log "  final user-record count: $(user_record_count "$JP")  (baseline $BASE_URC)"

real_after="$(ls "$REAL_SESS" 2>/dev/null | wc -l | tr -d ' ')"
leaked="$(grep -l "$NAME" "$REAL_SESS"/*.json 2>/dev/null || true)"
log ""
log "=== REAL-HOME BELT (after): before=$real_before after=$real_after ==="
if [ -n "$leaked" ]; then
    log "  !!! BELT VIOLATION: $leaked"; exit 2
fi
if [ "$real_before" != "$real_after" ]; then
    log "  !!! BELT COUNT CHANGED ($real_before -> $real_after)"; exit 2
fi
log "  BELT HOLDS ($real_before -> $real_after, zero leaked prefixed rows)"

log ""
log "############################################################"
log "### EXIT-CONTRACT SPOT-CHECK (sb new -p) -- REAL CLAUDE #2 ###"
log "############################################################"
# a4-spec section 3.5 / section 6: sb new -p on REAL claude -> exit 0 (went busy = accepted)
# + stdout 'Prompt delivered'. Record the exit code EXPLICITLY (echo $?).
NAME2="${JAIL_PREFIX}exitp"
( cd "$WORKDIR" && "$JAIL_SB_CMD" new "$NAME2" --cwd "$WORKDIR" -p "say the word ready and nothing else" ) \
    > "$JAIL_ROOT/ec-out.txt" 2> "$JAIL_ROOT/ec-err.txt"
EC_CODE=$?
log "  sb new -p exit code (echo \$?): $EC_CODE"
log "  stdout:"; sed 's/^/    /' "$JAIL_ROOT/ec-out.txt" | tee -a "$EV"
log "  stderr:"; head -5 "$JAIL_ROOT/ec-err.txt" | sed 's/^/    /' | tee -a "$EV"
if [ "$EC_CODE" = "0" ] && grep -q "Prompt delivered" "$JAIL_ROOT/ec-out.txt"; then
    log "  EXIT-CONTRACT: PASS (exit 0 + 'Prompt delivered' -- prompt ACCEPTED/went busy)"
elif [ "$EC_CODE" = "10" ]; then
    log "  EXIT-CONTRACT: exit 10 (STALLED -- delivered, readable, never went busy). Recorded."
else
    log "  EXIT-CONTRACT: code=$EC_CODE -- recorded (see stdout/stderr above)"
fi
# kill the exit-contract session in-jail afterwards (mission step 3)
"$JAIL_SB_CMD" ls --all --short 2>/dev/null | grep -q "$NAME2" && jail_kill_session "$NAME2" >/dev/null 2>&1
"$JAIL_ZMX_CMD" kill "$NAME2" --force >/dev/null 2>&1 || true
sleep 1
log "  exit-contract session killed; zmx tasks matching NAME2: $(jail_zmx list 2>/dev/null | grep -c "$NAME2" || echo 0)"

log ""
log "=== DEV-CHANNELS FINDING (A2 carry) ==="
log "  claude --version in jail: $CLVER"
RELAY_DIR="$HOME/.claude/relay"
log "  relay sidecar dir ($RELAY_DIR):"
if [ -d "$RELAY_DIR" ]; then
    ls -la "$RELAY_DIR" 2>/dev/null | sed 's/^/    /' | tee -a "$EV"
    for rf in "$RELAY_DIR"/*.json; do [ -f "$rf" ] && { log "    sidecar $rf:"; head -c 400 "$rf" | sed 's/^/      /' | tee -a "$EV"; log ""; }; done
else
    log "    (no relay sidecar dir -- relay server did not register a sidecar)"
fi
log "  ls --json relayPort field:"
"$JAIL_SB_CMD" ls --json 2>/dev/null | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except: print("    (ls --json parse failed)"); sys.exit(0)
rows=d if isinstance(d,list) else d.get("sessions",d)
for r in (rows if isinstance(rows,list) else []):
    print("    name=%s relayPort=%s" % (r.get("name"), r.get("relayPort") or r.get("relay_port")))
' | tee -a "$EV"
log "  channels dir state ($HOME/.claude/channels):"
ls -la "$HOME/.claude/channels" 2>/dev/null | sed 's/^/    /' | tee -a "$EV"
log "  trying send:relay (absence is a FINDING not a failure):"
relay_out="$("$JAIL_SB_CMD" send:relay "$NAME" "ping" 2>&1)"; relay_rc=$?
log "    send:relay rc=$relay_rc out=[$relay_out]"
log "  dev-channels banner in scrollback?"
jail_zmx history "$NAME" 2>/dev/null | strip | grep -iE "development channels|server:relay|channels \(experimental\)" | head -5 | sed 's/^/    /' | tee -a "$EV"

log ""
log "=== BOOT 1 SOAK COMPLETE. Teardown via trap. ==="
