#!/usr/bin/env bash
# scenario: `history` — semantic (backlog-completeness + content-integrity). STUB-BACKED.
# 0b DELTA-STRENGTH W3.8 (P12): backlog CONTENT INTEGRITY.
#
# Corpus entry: `history` — zmx serializes its server-side VT scrollback (the
# machinery that makes reattach-replay work; EMPIRICAL-RESULTS.md, L6). There is no
# `qd history` engine verb at the pin (zmx history is used internally, send.ts:146),
# so this row records the zmx-VT BACKLOG-COMPLETENESS property the engine relies on:
# lines a DETACHED session produced are retained server-side and surface in
# `zmx history`. Comparator class = semantic (backlog-completeness): every produced
# line present, in order.
#
# W3.8 STRENGTHENING (content integrity beyond ordering): in addition to the ordering
# check (assert_backlog_complete, -o substring extraction), assert:
#   - a SENTINEL backlog line byte-exact as a WHOLE line, and
#   - an EXACTLY-ONCE MULTISET over the expected whole lines (assert_backlog_multiset_
#     exact). The multiset check runs on a WHOLE-LINE, CR-stripped extraction (NOT the
#     -o substring view), so it catches DUPLICATES and BANNER-PREFIX corruption around
#     present lines (e.g. "BANNER:SBLINE 7") that the -o ordering view cannot see.
#
# §S: boot a stub-backed session (detached, clients=0) via the pinned-TS qd, drive
# the stub's deterministic backlog generator ("EMIT N" -> SBLINE 1..N to the PTY),
# then read `zmx history` and assert all N lines survived in order + content-intact.
# The recorded expectation is the deterministic backlog OUTCOME, not timing-variable
# bytes.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="history"
SCN_BUDGET_MS=60000
SCN_CLASS="semantic-backlog"
SCN_FIXTURE="fixtures/history/normalized/history.trace"
SCN_STUB_BACKED=1
SCN_HISTORY_N=12

scn_run() {
    local name
    name="$(scn_session_name hi)"
    bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    # Drive the backlog generator while DETACHED (qd new -d leaves clients=0). The
    # SBLINE rows enter zmx's server-side VT — the retention property under test.
    scn_sb_target send:pty "$name" "EMIT $SCN_HISTORY_N" >/dev/null 2>&1
    sleep 2

    # Read the serialized server-side VT scrollback (zmx history, pinned to the
    # jail's ZMX_DIR). This is what reattach-replay draws from. The full VT dump
    # also contains the (reflow-variable) shell prompt + launch-command echo — that
    # is ENVIRONMENTAL noise, not the backlog under test.
    #   SCN_OUT          = the -o substring view (one "SBLINE i" token/line) — the
    #                      ORDERING expectation (assert_backlog_complete).
    #   SCN_OUT.wholelines = WHOLE-line, CR-stripped lines that EQUAL "SBLINE i" — the
    #                      CONTENT-INTEGRITY view (sentinel byte-exact + exactly-once
    #                      multiset). A banner-prefixed "BANNER:SBLINE 7" is NOT a whole
    #                      "SBLINE 7" line, so it is caught here (the -o view cannot).
    ZMX_DIR="$ZMX_DIR" zmx history "$name" 2>/dev/null > "$SCN_OUT.fullvt" || true
    cat -v "$SCN_OUT.fullvt" 2>/dev/null | grep -Eo 'SBLINE [0-9]+' > "$SCN_OUT" || true
    # Whole-line view: strip the trailing CR (cat -v renders \r as ^M), keep ONLY
    # lines that contain the SBLINE marker (drops shell-prompt/launch noise), so a
    # clean "SBLINE 7\r" -> "SBLINE 7" and a corrupted "BANNER:SBLINE 7\r" ->
    # "BANNER:SBLINE 7" (which fails the whole-line literal check).
    cat -v "$SCN_OUT.fullvt" 2>/dev/null | sed 's/\^M$//' | grep 'SBLINE' > "$SCN_OUT.wholelines" || true
    printf '0\n' > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    # backlog-completeness: all N detached-produced SBLINE rows present, in order.
    assert_backlog_complete "$SCN_OUT" "SBLINE " "$SCN_HISTORY_N" || return 1
    # content-integrity (W3.8): sentinel byte-exact + exactly-once multiset over the
    # WHOLE-line view (catches duplicates AND banner-prefix corruption).
    assert_backlog_multiset_exact "$SCN_OUT.wholelines" "SBLINE " "$SCN_HISTORY_N"
}
