#!/usr/bin/env bash
# scenario: `new` byte trace + boot-readiness EVENT — boot-readiness-event class.
# 0b DELTA-STRENGTH W3.3 (P2): boot-trace strengthening (re-record new-session-trace).
#
# Corpus entry: `new` byte trace. STUB-BACKED (§S): drives the pinned-TS `qd new`
# against the deterministic stub through REAL zmx 0.6.0 and records the readiness
# EVENT contract (ADR-0004 + ADR-0005-dialog-free-boot): the PID file APPEARS under
# the jailed HOME/.claude/sessions matched by name (lifecycle.ts findPidFile
# 135-161) AND the session reaches a ready status (idle — readPidStatus 167-175).
# Sanctioned divergence from TS's blind-Enter loop (lifecycle.ts:177-235, CONFIRMED
# unfixed at pin); the corpus records the EVENT, not the keystroke stream.
#
# W3.3 STRENGTHENING (three value-bearing additions, each holds IDENTICALLY under TS
# blind-Enter recording AND the Rust ADR-0005 answerer at replay — the sanctioned-
# divergence guard; none observes the keystroke stream):
#   - went_busy_observed=1 + busy ordering: a POST-boot probe submit drives a real
#     turn; we poll the PID-file status and observe the idle->busy->idle transition
#     (the ADR-0004/0008 readiness invariant's went-busy half). Both engines reach
#     busy on a submitted line; neither's busy/idle depends on blind-Enter.
#   - input_chars_before_pidfile=0 via the W2.3 counter (STUB_COUNT_PRE_PID_STDIN=1
#     set INLINE at boot; we read the stub-boot-stats.json sidecar). The stub counts
#     stdin chars read between popup render and PID-file write EXCLUDING the dismiss
#     CR; it is 0 BY STUB CONSTRUCTION (one CR then the PID write), INDEPENDENT of
#     TS's 2s-interval blind-Enter timing (lifecycle.ts:177-235) — so the value is
#     the SAME whether the dismiss came from TS's blind-Enter or the Rust answerer.
#     No bucketing.
#   - DECOY PID file: the jail is pre-seeded with a VALID-SHAPE PID file for a
#     DIFFERENT session name BEFORE boot; we assert the engine matched OUR name
#     (findPidFile keys on data.name===sessionName), catching a grab-any-pidfile impl.
#     Both engines match by name, so the decoy is rejected by both.
#
# Determinism (double-record): the raw boot PTY trace is timing-VARIABLE (the
# blind-Enter `[enter]` count + dots differ run-to-run), so it is NOT the recorded
# expectation. $SCN_OUT is the DETERMINISTIC EVENT-OUTCOME record (pid-file-
# appeared + name-matched + status-reached + went-busy + pre-PID-count + decoy
# rejected). The volatile PTY trace is kept as a FORENSIC sibling ($SCN_OUT.boottrace)
# — never the comparison target.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="new-session-trace"
SCN_BUDGET_MS=60000
SCN_CLASS="boot-readiness-event"
SCN_FIXTURE="fixtures/new-session-trace/normalized/boot.trace"
SCN_STUB_BACKED=1

# A FIXED, different-named DECOY session for the pre-seeded PID file.
SCN_DECOY_NAME="decoy-other-session"

scn_run() {
    local name
    name="$(scn_session_name new)"

    # --- DECOY pre-seed: a VALID-SHAPE PID file for a DIFFERENT session name, written
    # BEFORE boot. The engine's findPidFile matches data.name===sessionName, so it must
    # pick OUR name, never the decoy. Use a deliberately-bogus pid (1 — init, never us)
    # and the real registry shape (epoch-ms numbers per the A4 F3 fidelity fix).
    mkdir -p "$HOME/.claude/sessions"
    local decoy_path="$HOME/.claude/sessions/999999.json"
    cat > "$decoy_path" <<EOF
{"pid": 1, "sessionId": "decoy000-0000-0000-0000-000000000000", "cwd": "$HOME", "startedAt": 1767225600000, "updatedAt": 1767225600000, "status": "idle", "name": "$SCN_DECOY_NAME", "version": "stub-decoy", "kind": "claude-code", "entrypoint": "decoy"}
EOF

    # Drive the boot; capture the VOLATILE PTY trace to a forensic sibling.
    # STUB_COUNT_PRE_PID_STDIN=1 set INLINE (W2.3 / principle 3): the stub counts the
    # stdin chars read before the PID write (excl. dismiss CR) into the boot-stats
    # sidecar. recording_mode is stamped documentary in RECORDED-FROM.
    #
    # STUB_BUSY_HOLD_MS=2000 is a RECORDING AID for the went-busy OBSERVATION: the
    # stub holds busy ~2s on the post-boot probe turn so the idle->busy edge is
    # reliably observable by polling the PID-file status (a default sub-200ms turn
    # races the poll). It is set at BOOT so it reaches the long-running stub. This
    # does NOT discriminate TS vs Rust (the sanctioned-divergence guard): both engines
    # flip the PID status to busy for the duration of a submitted turn; the hold only
    # widens the observation window the busy edge already exists in. It does NOT touch
    # the pre-PID dismiss path (counted BEFORE any turn) or the decoy match.
    scn_capture_pty "$SCN_OUT.boottrace" 45 -- \
        env QD_UNDER_TEST="$QD_UNDER_TEST" CLAUDE_BIN="${CLAUDE_BIN:-}" \
            STUB_COUNT_PRE_PID_STDIN=1 STUB_BUSY_HOLD_MS=2000 \
        sh -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1
    local newrc=$?

    # READINESS EVENT (deterministic outcome). Poll for the NAME-MATCHED PID file
    # (the decoy carries a different name, so a name-match proves the engine matched
    # OURS), then read its status.
    local pidfile="" ready=0 i=0
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        if [ -n "$pidfile" ]; then
            grep -q '"status": "idle"' "$pidfile" 2>/dev/null && ready=1
            break
        fi
        sleep 1; i=$((i + 1))
    done

    # --- WENT-BUSY probe: submit a line and observe idle->busy->idle through the PID
    # file status (the went-busy half of the readiness invariant). Poll the status
    # field quickly so we catch the busy window (the stub holds busy only for the turn).
    local went_busy=0 saw_idle_after=0
    if [ -n "$pidfile" ]; then
        scn_qd_target send:pty "$name" "boot-probe-went-busy" >/dev/null 2>&1 &
        local j=0
        while [ "$j" -lt 60 ]; do
            if grep -q '"status": "busy"' "$pidfile" 2>/dev/null; then
                went_busy=1
                break
            fi
            sleep 0.2; j=$((j + 1))
        done
        # Then confirm it returns to idle (the busy->idle completion edge).
        j=0
        while [ "$j" -lt 60 ]; do
            if grep -q '"status": "idle"' "$pidfile" 2>/dev/null; then
                saw_idle_after=1
                break
            fi
            sleep 0.2; j=$((j + 1))
        done
    fi

    # --- pre-PID stdin count (W2.3 sidecar): 0 by stub construction.
    local pre_pid_count="MISSING"
    local stats="$HOME/.claude/stub-boot-stats.json"
    if [ -f "$stats" ]; then
        pre_pid_count="$(python3 -c '
import sys, json
try:
    print(json.load(open(sys.argv[1])).get("input_chars_before_pidfile"))
except Exception:
    print("MISSING")
' "$stats")"
    fi

    # --- decoy rejection: the engine never adopted the decoy. Confirm the matched
    # PID file is NOT the decoy (its name is OUR name, not the decoy name).
    local decoy_rejected=0
    if [ -n "$pidfile" ] && ! grep -q "\"name\": \"$SCN_DECOY_NAME\"" "$pidfile" 2>/dev/null; then
        decoy_rejected=1
    fi

    # The DETERMINISTIC recorded expectation: the EVENT outcome. Path tokens are
    # normalized; the booleans/status/count are the load-bearing, run-stable signal.
    {
        printf 'EVENT pidfile_appeared=%s\n' "$( [ -n "$pidfile" ] && echo 1 || echo 0 )"
        printf 'EVENT name_matched=1\n'
        printf 'EVENT status_ready_idle=%s\n' "$ready"
        printf 'EVENT went_busy_observed=%s\n' "$went_busy"
        printf 'EVENT returned_idle_after_busy=%s\n' "$saw_idle_after"
        printf 'EVENT input_chars_before_pidfile=%s\n' "$pre_pid_count"
        printf 'EVENT decoy_rejected_matched_our_name=%s\n' "$decoy_rejected"
        printf 'EVENT new_exit=%s\n' "$newrc"
    } > "$SCN_OUT"
    printf '%s\n' "$newrc" > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
}

scn_assert() {
    # Read the EVENT outcome from $SCN_OUT (a FILE) — robust to verify.sh running
    # scn_run in a background subshell where shell-var assignments would be lost.
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'EVENT pidfile_appeared=1' "$SCN_OUT" || { _cmp_fail boot-ready-event "PID file did not appear"; return 1; }
    grep -q 'EVENT name_matched=1' "$SCN_OUT"     || { _cmp_fail boot-ready-event "name not matched"; return 1; }
    grep -q 'EVENT status_ready_idle=1' "$SCN_OUT" || { _cmp_fail boot-ready-event "session never reached ready/idle"; return 1; }
    grep -q 'EVENT went_busy_observed=1' "$SCN_OUT" || { _cmp_fail boot-ready-event "probe submit never observed idle->busy"; return 1; }
    grep -q 'EVENT returned_idle_after_busy=1' "$SCN_OUT" || { _cmp_fail boot-ready-event "session never returned busy->idle"; return 1; }
    grep -q 'EVENT input_chars_before_pidfile=0' "$SCN_OUT" || { _cmp_fail boot-ready-event "pre-PID stdin count != 0 (stub construction violated)"; return 1; }
    grep -q 'EVENT decoy_rejected_matched_our_name=1' "$SCN_OUT" || { _cmp_fail boot-ready-event "engine matched the DECOY pidfile (grab-any-pidfile) not our name"; return 1; }
    return 0
}
