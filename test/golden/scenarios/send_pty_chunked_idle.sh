#!/usr/bin/env bash
# scenario: send:pty CHUNKED-IDLE delivery of a >=4KB message — STUB-BACKED.
# 0b DELTA-STRENGTH W3.1 (P3, orc-4 ruling relay-1780682310824-1, 13:58 EDT).
#
# WHY THIS ROW EXISTS (P3 re-scope): the panel's "STUB_NO_QUEUE recording mode"
# premise assumed an ENGINE-side hold queue; at pin 8c59ec4 NONE exists. The
# chunked-PTY surface the pin actually added is the IDLE send path, where a >=4KB
# write is split into <=1024B chunks so it never overflows the ~4KB tty queue and
# is delivered byte-loss-free. This row drives THAT surface directly: ONE >=4KB
# ASCII message to an IDLE stub session via `send:pty --wait`, asserting at
# APPLICATION-OUTPUT level that the FULL payload survives all chunks byte-exact.
#
# R1 PARITY CITATIONS (pinned TS 8c59ec4, lead-verified file:line):
#   - send.ts:219-223 — IDLE send dispatches to deliverIdleTwoWrite(message, ...).
#   - submit.ts:287-334 deliverIdleTwoWrite — (1) chunked two-write delivery:
#       await sendTextChunked(deps, message, opts) [submit.ts:299], settle, \r alone.
#   - submit.ts:222-234 sendTextChunked — chunkBytes default 1024 (opts.chunkBytes
#       ?? 1024, :227), interChunkMs default 150 (:228); loops chunkText(message,
#       1024) sending each chunk with a 150ms inter-chunk sleep (:230-233).
#   - submit.ts:196-213 chunkText — splits the message into <=1024B chunks on
#       CODE-POINT boundaries (never splits a char). An ASCII burst can never split
#       a code point, so the reassembled JSONL text is byte-stable.
#   Result: a >=4KB ASCII message (here 4182B -> 5 chunks at <=1024B) drives the
#   multi-chunk PTY write path. The stub's _read_one_submit reassembles the chunks
#   into ONE submitted line (terminated by the separate \r), appends ONE user
#   record carrying the FULL text + ONE deterministic assistant reply. ZERO loss
#   across all chunks IS the chunked-delivery contract this row proves.
#
# B3 gate finding (multibyte input loss): the payload is ASCII-ONLY and every
# assertion keys on APPLICATION OUTPUT (the JSONL user record text + the --wait
# STUB-REPLY the stub writes), NEVER on input echo (ADD-6) — input-echo loss under
# the chunked write cannot false-fail/pass this row.
#
# Comparator class = semantic (acceptance: the >=4KB payload surfaces as exactly
# ONE user record carrying the FULL text byte-exact; --wait returns the
# deterministic STUB-REPLY of that text byte-exact; rc 0). The recorded expectation
# is the deterministic JSONL OUTCOME, not the timing-variable PTY bytes.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="send-pty-chunked-idle"
SCN_BUDGET_MS=90000
SCN_CLASS="semantic-chunked-idle"
SCN_FIXTURE="fixtures/send-pty-chunked-idle/normalized/trace"
SCN_STUB_BACKED=1
SCN_PROVISIONAL=0   # resolved: recorded against the pin's chunked-idle surface

# A >=4KB ASCII message to drive the CHUNKED PTY path (>=4 chunks at <=1024B).
# Deterministic + ASCII-only + carries NO normalizer-volatile token (no 13-digit
# run, no pid=/port= label, no :NN port shape, no "Ns ago" duration, no /tmp/ path)
# so the JSONL content + STUB-REPLY are byte-stable across the double-record. The
# word deliberately avoids any 40+ run of hex chars [0-9a-f] (mixes non-hex letters
# w/x/y/z) so the L11 secret-scan's generic-hex token rule does NOT flag it.
# 85 fixed words -> 4182 bytes -> 5 chunks at <=1024B (verified at record).
_scn_build_idle_burst() {
    local i=0
    printf 'CHUNKED-IDLE-4KB '
    while [ "$i" -lt 85 ]; do
        printf 'idle-payload-word-no-hex-run-xyzwxyzwxyzwxyzw-%02d ' "$i"
        i=$((i + 1))
    done
}
SCN_IDLE_MSG="${SCN_IDLE_MSG:-$(_scn_build_idle_burst)}"

# Extract every user record's `content` from the JSONL, in order, one
# user_text[i]= line per record. Reused by both scn_run (capture) and the
# anchored_on_user_text derivation.
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
    name="$(scn_session_name ci)"
    # Boot the stub-backed session and let it reach IDLE. The chunked-IDLE path is
    # the pin's actual chunked surface (deliverIdleTwoWrite -> sendTextChunked) and
    # fires only when the session is idle at send time.
    #
    # RECORDING MODE (W3.1, principle 3): STUB_RAW_STDIN=1 is set INLINE on the boot
    # line (exactly the STUB_BUSY_HOLD_MS precedent). It flips the stub PTY stdin to
    # RAW (clear ICANON) so the cooked-mode canonical-line bound (macOS MAX_CANON=
    # 1024) does NOT cap the >=4KB chunked write — the stub then reads like REAL
    # Claude's raw-mode TUI. Without it a cooked stub drops everything past byte 1024
    # (deliverIdleTwoWrite sends the whole text as ONE canonical line, no inter-chunk
    # CR; submit.ts:298-301 @ 8c59ec4). The seam is documentary in RECORDED-FROM
    # (recording_mode=STUB_RAW_STDIN=1); the load-bearing defence is the scenario sha
    # in the MATCH-PROOF (a seam-dropping edit invalidates the proof -> re-record).
    STUB_RAW_STDIN=1 bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done
    # Settle: ensure the session is idle (boot turn, if any, complete) before send.
    sleep 2

    # THE drive: ONE >=4KB ASCII message to the IDLE session via send:pty --wait.
    # deliverIdleTwoWrite chunks it (>=5 chunks at <=1024B, ~150ms apart) and submits
    # with a separate \r; --wait anchors on OUR message's user record and completes
    # on idle. Capture the --wait reply (the stub's deterministic STUB-REPLY).
    SCN_WAIT_OUT="$(scn_qd_target send:pty "$name" "$SCN_IDLE_MSG" --wait --timeout 60 2>/dev/null)"
    SCN_WAIT_RC=$?

    local jsonl
    jsonl="$(ls "$HOME"/.claude/projects/*/*.jsonl 2>/dev/null | head -1)"
    {
        # FULL PAYLOAD byte-exact: the >=4KB text landed in the JSONL user record
        # WHOLE (zero loss across all chunks — the chunked-delivery contract).
        printf 'full_payload_present=%s\n' "$(
            if [ -n "$jsonl" ] && grep -Fq "$SCN_IDLE_MSG" "$jsonl"; then echo 1; else echo 0; fi)"
        # Exactly ONE user record (no spurious/duplicate/split turn from chunking).
        printf 'user_record_count=%s\n' "$(
            if [ -n "$jsonl" ]; then grep -c '"type": "user"' "$jsonl" 2>/dev/null | tr -d '[:space:]'; else echo 0; fi)"
        # ORDERED user-record TEXTS (m4): one user_text[i]= per record, JSONL order.
        # The FULL burst text (>=4KB) is stored verbatim so a truncated/altered burst
        # produces a DIFFERENT fixture (the truncated-burst mutant bites here).
        if [ -n "$jsonl" ]; then
            python3 -c "$_scn_user_texts_py" "$jsonl"
        fi
        # ANCHOR: --wait anchored on OUR burst user record (findUserAnchor matches the
        # sent text byte-for-byte). Stored as the LAST user record's text == the burst.
        printf 'anchored_on_user_text=%s\n' "$(
            if [ -n "$jsonl" ]; then python3 -c "$_scn_last_user_py" "$jsonl"; fi)"
        # --wait reply text byte-exact: the deterministic STUB-REPLY for the burst
        # ("STUB-REPLY to: " + text.strip(), stub_claude.py:191-193).
        printf 'wait_reply_text=%s\n' "$(printf '%s' "$SCN_WAIT_OUT" | grep -m1 'STUB-REPLY to:')"
        printf 'wait_rc=%s\n' "$SCN_WAIT_RC"
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'full_payload_present=1' "$SCN_OUT" || { _cmp_fail chunked-idle "the >=4KB payload did not land whole in the JSONL (chunk loss)"; return 1; }
    grep -q 'user_record_count=1'    "$SCN_OUT" || { _cmp_fail chunked-idle "expected exactly 1 user record (chunking split/duplicated the turn)"; return 1; }
    # ORDERED texts (m4): the ONE user record's text is the FULL burst byte-exact.
    grep -q "^user_text\[0\]=CHUNKED-IDLE-4KB " "$SCN_OUT" || { _cmp_fail chunked-idle "first user record text is not the burst (order/truncation)"; return 1; }
    # ANCHOR: --wait anchored on the burst user record.
    grep -q '^anchored_on_user_text=CHUNKED-IDLE-4KB ' "$SCN_OUT" || { _cmp_fail chunked-idle "--wait did not anchor on the burst user record"; return 1; }
    # REPLY TEXT byte-exact: the deterministic STUB-REPLY for the burst.
    grep -q '^wait_reply_text=STUB-REPLY to: CHUNKED-IDLE-4KB ' "$SCN_OUT" || { _cmp_fail chunked-idle "--wait reply text is not the deterministic STUB-REPLY for the burst"; return 1; }
    grep -q '^wait_rc=0$' "$SCN_OUT" || { _cmp_fail chunked-idle "--wait rc is not 0"; return 1; }
    return 0
}
