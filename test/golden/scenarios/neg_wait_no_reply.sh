#!/usr/bin/env bash
# scenario: NEGATIVE — send:pty --wait no-reply failure shape (W3.4b, P4). STUB-BACKED.
# 0b DELTA-STRENGTH W3.4: recorded negative row per seam family.
#
# THE PANEL'S POINT (P4 / rider R2): without a RECORDED failure-shape fixture for a
# withheld JSONL reply, an engine that fabricated a reply (or swallowed the
# no-reply as success-with-text) would be undetectable. This row RECORDS qd's
# HONEST OUTCOME when the counterpart appends the USER record but WITHHOLDS the
# assistant reply.
#
# SEAM (INLINE recording mode, principle 3): STUB_WITHHOLD_JSONL=1 set INLINE at
# boot (it must reach the long-running stub). With it on, a submitted turn appends
# the user record + transitions busy→idle but NEVER appends the assistant pair
# (stub_claude.py:586-594).
#
# THE RECORDED SHAPE (R4-honest — what qd ACTUALLY does, not a fabricated timeout):
# `send:pty --wait` anchors on the user record (findUserAnchor, utils.ts:341-346),
# the status reaches idle so decideWait returns "complete" (utils.ts:359-365) — the
# wait COMPLETES (it does NOT time out), and the from-anchor extraction finds NO
# assistant text, so qd prints the literal `(no text response)` to STDOUT and exits
# 0 (send.ts:316-365). This IS the failure shape: a withheld reply surfaces as the
# `(no text response)` sentinel with the JSONL carrying the user record but no
# assistant record. (Recording note: WITHHOLD_JSONL produces NO `--wait` timeout
# because the stub still reaches idle — a true timeout would need a never-idle
# counterpart, a different seam, out of scope this wave. We record the HONEST shape;
# the mutant proves it bites.)
#
# Determinism (double-record): the deterministic OUTCOME is the stdout sentinel +
# the rc + the JSONL record-class counts (1 user, 0 assistant). NOT the byte trace.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="neg-wait-no-reply"
SCN_BUDGET_MS=60000
SCN_CLASS="failure-shape"
SCN_FIXTURE="fixtures/neg-wait-no-reply/normalized/shape.txt"
SCN_STUB_BACKED=1

SCN_MSG="neg-wait-probe"

scn_run() {
    local name
    name="$(scn_session_name nwr)"
    # Boot with STUB_WITHHOLD_JSONL=1 set at BOOT (reaches the long-running stub).
    STUB_WITHHOLD_JSONL=1 bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    # Send a turn --wait with a SMALL timeout. The user record lands; the assistant
    # reply is withheld; status reaches idle so --wait completes with no text.
    local waitout
    waitout="$(scn_sb_target send:pty "$name" "$SCN_MSG" --wait --timeout 15 2>"$SCN_OUT.stderr")"
    local waitrc=$?

    # JSONL record-class counts: exactly 1 user record, 0 assistant records.
    local jsonl users assists
    jsonl="$(ls "$HOME"/.claude/projects/*/*.jsonl 2>/dev/null | head -1)"
    users=0; assists=0
    if [ -n "$jsonl" ]; then
        users="$(grep -c '"type": "user"' "$jsonl" 2>/dev/null | tr -d '[:space:]')"
        assists="$(grep -c '"type": "assistant"' "$jsonl" 2>/dev/null | tr -d '[:space:]')"
    fi
    [ -z "$users" ] && users=0
    [ -z "$assists" ] && assists=0

    {
        printf 'SHAPE wait_exit=%s\n' "$waitrc"
        printf 'SHAPE no_text_response_sentinel=%s\n' \
            "$( printf '%s' "$waitout" | grep -q '(no text response)' && echo 1 || echo 0 )"
        printf 'SHAPE user_record_present=%s\n' "$( [ "$users" -ge 1 ] && echo 1 || echo 0 )"
        printf 'SHAPE assistant_record_absent=%s\n' "$( [ "$assists" -eq 0 ] && echo 1 || echo 0 )"
    } > "$SCN_OUT"
    printf '%s\n' "$waitrc" > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'SHAPE no_text_response_sentinel=1' "$SCN_OUT" || { _cmp_fail failure-shape "withheld reply did NOT surface as '(no text response)' (engine fabricated a reply or swallowed it)"; return 1; }
    grep -q 'SHAPE user_record_present=1' "$SCN_OUT"       || { _cmp_fail failure-shape "user record absent (the anchor the --wait keys on never landed)"; return 1; }
    grep -q 'SHAPE assistant_record_absent=1' "$SCN_OUT"   || { _cmp_fail failure-shape "an assistant record appeared despite STUB_WITHHOLD_JSONL (reply not actually withheld)"; return 1; }
    return 0
}
