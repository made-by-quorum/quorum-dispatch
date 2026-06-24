#!/usr/bin/env bash
# scenario: NEGATIVE — boot-timeout failure shape (W3.4a, P4). STUB-BACKED.
# 0b DELTA-STRENGTH W3.4: recorded negative row per seam family.
#
# THE PANEL'S POINT (P4 / rider R2): the R2 seams previously had NO RECORDED
# failure-shape fixtures — a "swallow-failure-as-success" engine (one that exits 0
# / prints nothing on a misbehaving counterpart) would be UNDETECTABLE by the
# oracle. This row RECORDS sb's FAILURE SHAPE (rc + normalized stderr token) when
# the counterpart withholds the PID file, so a regression that swallows the boot
# failure DIFFS.
#
# SEAM (INLINE recording mode, principle 3): STUB_WITHHOLD_PID=1 set INLINE in
# scn_run (the STUB_BUSY_HOLD_MS precedent) — the stub renders the popup, consumes
# the dismiss CR, then HOLDS OPEN without ever writing the PID file. sb's
# waitForSessionReady (lifecycle.ts:194-262) polls for the name-matched PID file;
# it never appears, so after the PID-phase deadline sb FAILS the readiness wait,
# best-effort-reaps the stray wrapper, prints the readiness-timeout error to
# stderr, and exits 1 (lifecycle.ts:929-945). recording_mode stamped documentary in
# RECORDED-FROM; defended load-bearing by scenario-sha-in-MATCH-PROOF.
#
# Determinism (double-record): the EXACT readiness byte trace (the ` [enter]....`
# dots) is timing-variable, so it is NOT the recorded expectation. $SCN_OUT is the
# DETERMINISTIC FAILURE-SHAPE record: new_exit (the load-bearing rc, never
# normalized) + the normalized stderr TOKEN class (did_reach_idle / a stable
# substring of the readiness-timeout message). The volatile boot trace is kept as a
# forensic sibling, never the comparison target.
#
# BUDGET: sb's PID-not-found phase caps at ~40s (lifecycle.ts:215
# pidPhaseDeadline = min(deadline, now+40000)). The driven timeout is therefore
# bounded by the ENGINE, not the scenario; SCN_BUDGET_MS absorbs it (×1 run; record
# does two serially).
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="neg-boot-timeout"
SCN_BUDGET_MS=70000
SCN_CLASS="failure-shape"
SCN_FIXTURE="fixtures/neg-boot-timeout/normalized/shape.txt"
SCN_STUB_BACKED=1

scn_run() {
    local name
    name="$(scn_session_name nbt)"

    # Drive the boot with STUB_WITHHOLD_PID=1 INLINE (recording mode): the stub
    # never writes the PID file, so the readiness wait must FAIL. Capture stderr
    # (the failure-shape carrier) to a sibling; the volatile readiness byte trace
    # to another. new's exit code is the load-bearing rc.
    STUB_WITHHOLD_PID=1 \
        bash -c "exec $SB_UNDER_TEST new $name" \
        > "$SCN_OUT.stdout" 2> "$SCN_OUT.stderr"
    local newrc=$?

    # FAILURE-SHAPE record (deterministic outcome). The stderr token class is a
    # STABLE substring of the readiness-timeout message (lifecycle.ts:935: 'did not
    # reach idle state within timeout') — a normalized token, not the byte trace.
    local saw_timeout_token=0
    grep -q 'did not reach idle state within timeout' "$SCN_OUT.stderr" 2>/dev/null && saw_timeout_token=1
    # No name-matched PID file was ever written (the seam's premise — the engine did
    # not fabricate or grab one).
    local pidfile_appeared=0
    grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json >/dev/null 2>&1 && pidfile_appeared=1

    {
        printf 'SHAPE new_exit=%s\n' "$newrc"
        printf 'SHAPE failed_nonzero=%s\n' "$( [ "$newrc" -ne 0 ] && echo 1 || echo 0 )"
        printf 'SHAPE readiness_timeout_token=%s\n' "$saw_timeout_token"
        printf 'SHAPE pidfile_appeared=%s\n' "$pidfile_appeared"
    } > "$SCN_OUT"
    printf '%s\n' "$newrc" > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'SHAPE failed_nonzero=1' "$SCN_OUT"          || { _cmp_fail failure-shape "boot did NOT fail (swallow-failure-as-success: rc 0 on a withheld PID file)"; return 1; }
    grep -q 'SHAPE readiness_timeout_token=1' "$SCN_OUT" || { _cmp_fail failure-shape "readiness-timeout stderr token absent (failure shape not surfaced)"; return 1; }
    grep -q 'SHAPE pidfile_appeared=0' "$SCN_OUT"        || { _cmp_fail failure-shape "a PID file appeared despite STUB_WITHHOLD_PID (engine fabricated/grabbed one)"; return 1; }
    return 0
}
