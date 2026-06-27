#!/usr/bin/env bash
# scenario: NEGATIVE/TOLERANCE — two-stage PID write (W3.4d, P11 consumer). STUB-BACKED.
# 0b DELTA-STRENGTH W3.4: the P11 partial-PID-write tolerance row.
#
# THE PANEL'S POINT (P11, red-team m3): the STUB_TWO_STAGE_PID_WRITE seam makes
# EVERY PID-file write land DIRECT on the final path in TWO stages — a
# syntactically-PARTIAL JSON prefix + flush, a ~1500ms gap, then the complete
# rewrite — BYPASSING the atomic tmp+rename (stub_claude.py:289-313). A reader that
# races the write can observe a mid-write partial file. This row asserts the
# ENGINE'S READ TOLERANCE OUTCOME: boot readiness STILL fires and `qd ls` never
# crashes on a mid-write partial PID file (findPidFile/readPidStatus catch the
# partial-JSON parse error and retry, lifecycle.ts:148-176).
#
# OUTCOME-ONLY (binding, red-team m3 / W2.2): we assert the DETERMINISTIC OUTCOME
# ONLY — boot reached idle, ls exits 0, the session is VISIBLE after the write
# completes. We NEVER assert the PARTIAL STATE itself (observing the partial file is
# RACY and non-deterministic — it would break double-record). The `qd ls` calls run
# DURING and AFTER the write window; the assertion is only that they do not crash
# and the session is present once the write settles.
#
# THE MUTANT (red-team m3): a partial-JSON-INTOLERANT reader (the pre-A1-PR#20
# whole-row-drop behaviour) would DROP the session on the partial read and never
# recover -> boot_reached_idle=0 / session_visible_after=0. The mutation-real tooth
# flips these fields and proves the tolerance assertion BITES.
#
# SEAM (INLINE recording mode, principle 3): STUB_TWO_STAGE_PID_WRITE=1 (+ the
# default ~1500ms STUB_TWO_STAGE_GAP_MS) set INLINE at boot. recording_mode stamped
# documentary in RECORDED-FROM; defended load-bearing by scenario-sha-in-MATCH-PROOF.
#
# BUDGET: SCN_BUDGET_MS absorbs the ~1500ms gap on the boot write AND on each
# busy/idle status transition (every write() is two-stage while the seam is set),
# plus the readiness polling (1s cadence). Sized generously.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="neg-two-stage-tolerance"
SCN_BUDGET_MS=70000
SCN_CLASS="failure-shape"
SCN_FIXTURE="fixtures/neg-two-stage-tolerance/normalized/shape.txt"
SCN_STUB_BACKED=1

scn_run() {
    local name
    name="$(scn_session_name n2s)"
    # Boot with STUB_TWO_STAGE_PID_WRITE=1 INLINE (default ~1500ms gap). Every PID
    # write is partial-then-complete; the engine must tolerate the mid-write partial.
    STUB_TWO_STAGE_PID_WRITE=1 bash -c "exec $SB_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!

    # Poll `qd ls` DURING the write window — it must never crash (rc!=0). This
    # deliberately races the two-stage write to exercise the engine reading a file
    # that may be mid-write. We assert ONLY that ls does not crash (OUTCOME, not the
    # partial state). Run a few times across the boot window.
    local ls_never_crashed=1 k=0
    while [ "$k" -lt 8 ]; do
        scn_sb ls --json >/dev/null 2>&1
        [ $? -ne 0 ] && ls_never_crashed=0
        sleep 1; k=$((k + 1))
    done

    # Boot readiness OUTCOME: the name-matched PID file appears AND the session
    # reaches idle (the readiness event still fires through the partial writes).
    local pidfile="" boot_idle=0 i=0
    while [ "$i" -lt 40 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        if [ -n "$pidfile" ]; then
            grep -q '"status": "idle"' "$pidfile" 2>/dev/null && boot_idle=1
            [ "$boot_idle" -eq 1 ] && break
        fi
        sleep 1; i=$((i + 1))
    done

    # AFTER the write settles: the session is VISIBLE in ls (ls exits 0 and the row
    # is present). This is the post-completion visibility OUTCOME.
    scn_sb ls --json 2>/dev/null > "$SCN_OUT.lsjson"
    local ls_after_rc=$?
    local visible_after=0
    visible_after="$(python3 -c '
import sys, json
try:
    rows = json.load(open(sys.argv[1]))
except Exception:
    rows = []
name = sys.argv[2]
print(1 if any((r.get("name") == name) for r in rows) else 0)
' "$SCN_OUT.lsjson" "$name")"
    [ -z "$visible_after" ] && visible_after=0

    {
        printf 'SHAPE boot_reached_idle=%s\n' "$boot_idle"
        printf 'SHAPE ls_never_crashed_during_write=%s\n' "$ls_never_crashed"
        printf 'SHAPE ls_after_exit_zero=%s\n' "$( [ "$ls_after_rc" -eq 0 ] && echo 1 || echo 0 )"
        printf 'SHAPE session_visible_after=%s\n' "$visible_after"
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
    rm -f "$SCN_OUT.lsjson"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'SHAPE boot_reached_idle=1' "$SCN_OUT"                || { _cmp_fail failure-shape "boot did NOT reach idle through the two-stage partial PID writes (intolerant reader)"; return 1; }
    grep -q 'SHAPE ls_never_crashed_during_write=1' "$SCN_OUT"    || { _cmp_fail failure-shape "qd ls CRASHED on a mid-write partial PID file (not partial-tolerant)"; return 1; }
    grep -q 'SHAPE ls_after_exit_zero=1' "$SCN_OUT"               || { _cmp_fail failure-shape "qd ls did not exit 0 after the write settled"; return 1; }
    grep -q 'SHAPE session_visible_after=1' "$SCN_OUT"            || { _cmp_fail failure-shape "session NOT visible after the write completed (whole-row dropped on a partial read — pre-PR#20 behaviour)"; return 1; }
    return 0
}
