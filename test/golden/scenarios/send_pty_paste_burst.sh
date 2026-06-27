#!/usr/bin/env bash
# scenario: send:pty paste-burst + queue-to-busy + JSONL --wait — STUB-BACKED.
# PROVISIONAL (ADD-2) -> resolved at pin.
#
# Corpus entry: send:pty byte trace incl. paste-burst, recorded against the NEW
# ADD-2 semantics (Pete's rulings, CONFIRMED in pinned send.ts):
#   - queue-to-busy: a send to a BUSY session is QUEUED (decideSendPty -> send-queue,
#     utils.ts:297-299; two-write text-then-CR + content-verified CR, send.ts:154-222)
#     and drains when the current turn ends.
#   - JSONL-keyed --wait: --wait anchors on OUR message's `user` record (findUserAnchor
#     utils.ts:341-346) and completes on status==idle (decideWait 359-365; send.ts
#     224-294). Works on busy sessions (attribution by record, not busy/idle cycles).
#
# §S: drives the pinned-TS `qd send:pty` against the stub. To EXERCISE queue-to-busy
# (not just the idle path), turn 1 runs with STUB_BUSY_HOLD_MS so the stub stays busy
# while turn 2 (a >400-char PASTE-BURST, the FINDING-E1 surface) is sent --wait: qd
# observes status==busy -> send-queue; the queued message drains on idle and --wait
# anchors on its user record. Session-targeting verbs use scn_sb_target (A4 belt).
#
# B3 gate finding (multibyte input loss): markers are ASCII-only and every
# assertion keys on APPLICATION OUTPUT (the JSONL user records + the --wait
# STUB-REPLY the stub writes), NEVER on input echo — input-echo loss under
# paste-burst cannot false-fail/pass this row.
#
# Comparator class = semantic (acceptance: both messages surface as ordered user
# records in the JSONL — the queue drained; --wait returned the queued message's
# reply; no spurious empty/duplicate turn). The recorded expectation is the
# deterministic JSONL OUTCOME, not the timing-variable PTY bytes.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="send-pty-paste-burst"
SCN_BUDGET_MS=90000
SCN_CLASS="semantic-submit-discipline"   # ADD-2 resolved at pin
SCN_FIXTURE="fixtures/send-pty-paste-burst/normalized/trace"
SCN_STUB_BACKED=1
SCN_PROVISIONAL=0   # resolved: recorded against the NEW semantics at pin

# A >400-char message to trigger paste-burst detection (FINDING-E1 surface). Kept
# SUB-1KB (the named corpus bound, ADR-0010): under the cooked-mode stub a single
# canonical line larger than macOS MAX_CANON=1024 would overflow — the >=4KB chunked
# path is exercised by the SEPARATE send-pty-chunked-idle row (raw-stdin mode). 422B.
SCN_BURST_MSG="${SCN_BURST_MSG:-$(printf 'PASTE-BURST '; i=0; while [ $i -lt 60 ]; do printf 'word%d ' $i; i=$((i+1)); done)}"
SCN_TURN1_MSG="first-turn-holds-busy"

# Extract every user record's `content` from the JSONL, in order, one user_text[i]=
# line per record (red-team m4: ordered TEXTS, not a queue_order_ok boolean — a
# swapped-order impl produces a DIFFERENT fixture).
_scn_user_texts_py='
import sys, json
idx = 0
for line in open(sys.argv[1]):
    line = line.strip()
    if not line:
        continue
    try:
        o = json.loads(line)
    except Exception:
        continue
    if o.get("type") != "user":
        continue
    c = o.get("message", {}).get("content", "")
    if not isinstance(c, str):
        c = json.dumps(c)
    sys.stdout.write("user_text[%d]=%s\n" % (idx, c))
    idx += 1
'
_scn_last_user_py='
import sys, json
last_user = None
for line in open(sys.argv[1]):
    line = line.strip()
    if not line:
        continue
    try:
        o = json.loads(line)
    except Exception:
        continue
    if o.get("type") == "user":
        c = o.get("message", {}).get("content", "")
        if isinstance(c, str):
            last_user = c
sys.stdout.write(last_user if last_user is not None else "")
'

scn_run() {
    local name
    name="$(scn_session_name sp)"
    # Boot the stub-backed session (idle).
    # STUB_BUSY_HOLD_MS is set at BOOT so it reaches the long-running stub process
    # (a send-time env would NOT reach the already-running stub). Every turn then
    # holds busy ~6s — long enough for turn 2 to observe BUSY and queue.
    STUB_BUSY_HOLD_MS=6000 bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    # Turn 1: send a message that HOLDS the stub busy (STUB_BUSY_HOLD_MS), so the
    # next send observes a BUSY session. Fire-and-forget; do NOT wait.
    scn_sb_target send:pty "$name" "$SCN_TURN1_MSG" >/dev/null 2>&1 &
    # Give turn 1 a moment to flip the session to busy.
    sleep 2

    # Turn 2: the PASTE-BURST, sent --wait WHILE BUSY -> queue-to-busy. --wait
    # anchors on the JSONL user record (the queue draining to us) and completes on
    # idle. Capture the --wait reply text (the stub's deterministic STUB-REPLY).
    SCN_WAIT_OUT="$(scn_sb_target send:pty "$name" "$SCN_BURST_MSG" --wait --timeout 60 2>/dev/null)"
    SCN_WAIT_RC=$?

    # Observe the JSONL OUTCOME: both user records present, in queue order.
    local jsonl
    jsonl="$(ls "$HOME"/.claude/projects/*/*.jsonl 2>/dev/null | head -1)"
    {
        printf 'queue_drained_both_users=%s\n' "$(
            if [ -n "$jsonl" ] \
               && grep -Fq "$SCN_TURN1_MSG" "$jsonl" \
               && grep -Fq 'PASTE-BURST' "$jsonl"; then echo 1; else echo 0; fi)"
        # order: turn1's user record precedes the paste-burst user record.
        printf 'queue_order_ok=%s\n' "$(
            if [ -n "$jsonl" ]; then
                t1="$(grep -nF "$SCN_TURN1_MSG" "$jsonl" | head -1 | cut -d: -f1)"
                tb="$(grep -nF 'PASTE-BURST' "$jsonl" | head -1 | cut -d: -f1)"
                if [ -n "$t1" ] && [ -n "$tb" ] && [ "$t1" -lt "$tb" ]; then echo 1; else echo 0; fi
            else echo 0; fi)"
        # ORDERED user-record TEXTS (red-team m4): one user_text[i]= per record in
        # JSONL order. turn1 must be user_text[0], the burst user_text[1] — a
        # swapped-order impl produces a DIFFERENT fixture (vs the old boolean).
        if [ -n "$jsonl" ]; then
            python3 -c "$_scn_user_texts_py" "$jsonl"
        fi
        # ANCHOR: --wait anchored on OUR burst user record (findUserAnchor matches the
        # sent text byte-for-byte). Stored as the LAST user record's text == the burst.
        printf 'anchored_on_user_text=%s\n' "$(
            if [ -n "$jsonl" ]; then python3 -c "$_scn_last_user_py" "$jsonl"; fi)"
        # --wait returned the queued message's reply (attribution by record).
        printf 'wait_reply_present=%s\n' "$( printf '%s' "$SCN_WAIT_OUT" | grep -q 'STUB-REPLY' && echo 1 || echo 0 )"
        # --wait REPLY TEXT byte-exact: the deterministic STUB-REPLY for the burst
        # ("STUB-REPLY to: " + text.strip(), stub_claude.py:191-193).
        printf 'wait_reply_text=%s\n' "$(printf '%s' "$SCN_WAIT_OUT" | grep -m1 'STUB-REPLY to:')"
        printf 'wait_rc=%s\n' "$SCN_WAIT_RC"
        # no spurious EXTRA user turn: exactly TWO user records (turn1 + burst).
        printf 'user_record_count=%s\n' "$(
            if [ -n "$jsonl" ]; then grep -c '"type": "user"' "$jsonl" 2>/dev/null | tr -d '[:space:]'; else echo 0; fi)"
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'queue_drained_both_users=1' "$SCN_OUT" || { _cmp_fail submit-discipline "queue did not drain both messages"; return 1; }
    grep -q 'queue_order_ok=1' "$SCN_OUT"          || { _cmp_fail submit-discipline "queued messages out of order"; return 1; }
    grep -q 'wait_reply_present=1' "$SCN_OUT"       || { _cmp_fail submit-discipline "--wait returned no reply for the queued message"; return 1; }
    grep -q 'user_record_count=2' "$SCN_OUT"        || { _cmp_fail submit-discipline "spurious/duplicate user turn (expected exactly 2)"; return 1; }
    # ORDERED texts (m4): turn1 is user_text[0], the burst is user_text[1].
    grep -q "^user_text\[0\]=${SCN_TURN1_MSG}\$" "$SCN_OUT" || { _cmp_fail submit-discipline "first user record text is not turn1 (order wrong / corrupted)"; return 1; }
    grep -q '^user_text\[1\]=PASTE-BURST ' "$SCN_OUT"      || { _cmp_fail submit-discipline "second user record text is not the burst (order wrong / truncated)"; return 1; }
    # ANCHOR: --wait anchored on the burst user record (its text is the burst).
    grep -q '^anchored_on_user_text=PASTE-BURST ' "$SCN_OUT" || { _cmp_fail submit-discipline "--wait did not anchor on the burst user record"; return 1; }
    # REPLY TEXT byte-exact: the deterministic STUB-REPLY for the burst.
    grep -q '^wait_reply_text=STUB-REPLY to: PASTE-BURST ' "$SCN_OUT" || { _cmp_fail submit-discipline "--wait reply text is not the deterministic STUB-REPLY for the burst"; return 1; }
    return 0
}
