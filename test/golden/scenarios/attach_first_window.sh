#!/usr/bin/env bash
# scenario: live-attach FIRST-WINDOW — SEMANTIC/PRESENCE (W3.5, P5). STUB-BACKED.
# 0b DELTA-STRENGTH W3.5: the attach-INITIATING capture (new row).
#
# WHAT (red-team M1, binding): boot a stub session, drive a deterministic backlog,
# then initiate a REAL `qd attach` UNDER A PTY (scn_capture_pty), capture the FIRST
# ~1KB of attach output, and DETACH CLEANLY (ctrl-\, zmx's per-client detach — never
# a kill). This is the attach-INITIATING capture; B3 ruled only attach-SPANNING out
# (a full attach session spanning resize/repaint), not the first window.
#
# COMPARATOR CLASS = SEMANTIC/PRESENCE (red-team M1, binding — NOT byte-exact over
# the KB; the attach-init region is reflow/timing-variable, which is exactly why the
# existing attach row avoids a live attach). Two PRESENCE assertions over the
# captured first window:
#   (i)  ?1049h (alt-screen enter) is ABSENT — the NEVER-normalized byte class
#        (ADR-0003 / L7: a (re)attach emits zero alt-screen). A regression that
#        takes over the alt-screen on attach DIFFS.
#   (ii) a DETERMINISTIC backlog SENTINEL substring is PRESENT — we EMIT a known
#        "SBLINE k" into the server-side VT BEFORE attaching, so zmx's reattach
#        replay carries it into the first window. The sentinel substring proves the
#        backlog actually replayed (attach delivered content), not an empty/garbled
#        window.
#
# If even PRESENCE proves double-record unstable -> EXCLUDED-<reason> per R4 (never a
# vacuous tick) + report. (This recording is the stability check.)
#
# Determinism (double-record): PRESENCE booleans over the first KB are the recorded
# expectation, NOT the byte trace. The window's EXACT bytes vary run-to-run (reflow/
# timing); the two booleans (altscreen-absent, sentinel-present) are stable.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="attach-first-window"
SCN_BUDGET_MS=60000
SCN_CLASS="semantic-presence"
SCN_FIXTURE="fixtures/attach-first-window/normalized/presence.txt"
SCN_STUB_BACKED=1
SCN_SENTINEL_K=7   # the deterministic backlog sentinel "SBLINE 7"

scn_run() {
    local name
    name="$(scn_session_name afw)"
    bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    # Drive a DETERMINISTIC backlog into the server-side VT BEFORE attaching, so the
    # reattach replay carries the sentinel "SBLINE 7" into the first window.
    scn_sb_target send:pty "$name" "EMIT $SCN_SENTINEL_K" >/dev/null 2>&1
    sleep 2

    # Initiate a REAL `qd attach` under a PTY; capture the first window; DETACH
    # CLEANLY via ctrl-\ (0x1c = base64 HA==) injected ~3s in (zmx's per-client
    # detach — leaves the session ALIVE). Capture ~6s of output.
    #
    # ENV HAZARD (recorded fact): `zmx attach` run from INSIDE a zmx session reads
    # $ZMX_SESSION and redirects to the CURRENT (host) session — the attach then
    # errors `session "<host>" does not exist` and captures nothing. The harness jail
    # does not clear $ZMX_SESSION (it is not a path-typed isolation var), so we
    # explicitly UNSET it in the attach child via `env -u ZMX_SESSION` so the attach
    # targets OUR jailed session, not the recorder's own zmx wrapper.
    local attach_raw="$SCN_OUT.attachcap"
    scn_capture_pty "$attach_raw" 6 --inject-b64 "HA==" --inject-delay 3 -- \
        env -u ZMX_SESSION QD_UNDER_TEST="$QD_UNDER_TEST" CLAUDE_BIN="${CLAUDE_BIN:-}" \
        sh -c "exec $QD_UNDER_TEST attach $name" >/dev/null 2>&1

    # FIRST WINDOW = the first 1024 bytes of the attach capture.
    local window="$SCN_OUT.window"
    head -c 1024 "$attach_raw" 2>/dev/null > "$window" || : > "$window"

    # (i) ?1049h (alt-screen enter) ABSENT over the window. cat -v renders ESC as ^[.
    local altscreen_hits
    altscreen_hits="$(cat -v "$window" 2>/dev/null | grep -Eo '\^\[\[\?1049h' 2>/dev/null | grep -c . 2>/dev/null)"
    altscreen_hits="$(printf '%s' "$altscreen_hits" | tr -d '[:space:]')"; [ -z "$altscreen_hits" ] && altscreen_hits=0

    # (ii) deterministic backlog SENTINEL present in the window.
    local sentinel_present=0
    cat -v "$window" 2>/dev/null | grep -q "SBLINE $SCN_SENTINEL_K" && sentinel_present=1

    {
        printf 'PRESENCE altscreen_1049h_absent=%s\n' "$( [ "$altscreen_hits" -eq 0 ] && echo 1 || echo 0 )"
        printf 'PRESENCE backlog_sentinel_present=%s\n' "$sentinel_present"
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
    rm -f "$attach_raw" "$attach_raw.exit" "$window"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'PRESENCE altscreen_1049h_absent=1' "$SCN_OUT"   || { _cmp_fail semantic-presence "alt-screen ?1049h PRESENT in the attach first window (attach took over the alt-screen)"; return 1; }
    grep -q 'PRESENCE backlog_sentinel_present=1' "$SCN_OUT"  || { _cmp_fail semantic-presence "deterministic backlog sentinel ABSENT from the attach first window (backlog did not replay)"; return 1; }
    return 0
}
