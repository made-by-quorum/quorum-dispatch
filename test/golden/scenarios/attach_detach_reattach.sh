#!/usr/bin/env bash
# scenario: attach / detach / reattach — semantic (backlog-completeness + no-altscreen
# + content-integrity). STUB-BACKED. 0b DELTA-STRENGTH W3.8 (P12).
#
# Corpus entry: attach/detach/reattach — THE killer zmx property (EMPIRICAL-
# RESULTS.md, L6): everything produced while DETACHED (clients=0) is retained
# server-side (zmx's ghostty_vt scrollback model) and replayed on reattach; dtach
# FAILS this. Plus L7: a (re)attach emits ZERO alt-screen. Comparator class =
# semantic: backlog-completeness + no-altscreen (scroll-intact is a property of the
# same retained backlog). B3 gate finding (attach re-resize): this row observes the
# retained backlog via `zmx history` (the reattach-replay source), NOT a live
# `zmx attach` capture, so no attach-time PTY resize event enters the expectation;
# the semantic class is resize-tolerant by construction.
#
# W3.8 STRENGTHENING (content integrity beyond ordering): in addition to the ordering
# check + no-altscreen, assert a SENTINEL backlog line byte-exact (whole line) and an
# EXACTLY-ONCE MULTISET over the expected whole lines (assert_backlog_multiset_exact)
# on a WHOLE-LINE, CR-stripped extraction — catching DUPLICATES and BANNER-PREFIX
# corruption around present lines (which the -o substring ordering view cannot see).
#
# §S: boot a stub-backed session DETACHED (qd new -d, clients=0), drive the stub's
# deterministic backlog generator ("EMIT N" -> SBLINE 1..N) while detached, then read
# the server-side VT (zmx history — the exact source reattach replays from) and assert
# every detached-produced line survived in order AND content-intact AND the replay
# carries no alt-screen takeover. The recorded expectation is the deterministic
# retained backlog, not timing-variable PTY bytes.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="attach-detach-reattach"
SCN_BUDGET_MS=60000
SCN_CLASS="semantic-backlog-scroll"
SCN_FIXTURE="fixtures/attach-detach-reattach/normalized/reattach.trace"
SCN_STUB_BACKED=1
SCN_ADR_N=20

scn_run() {
    local name
    name="$(scn_session_name adr)"
    bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    # Produce backlog WHILE DETACHED (clients=0): the lines must survive server-side.
    scn_qd_target send:pty "$name" "EMIT $SCN_ADR_N" >/dev/null 2>&1
    sleep 2

    # The reattach-replay source: zmx's serialized server-side VT. dtach would have
    # lost the detached lines; zmx retains them.
    #   SCN_OUT          = the -o substring view (one "SBLINE i" token/line) — the
    #                      recorded ORDERING expectation (assert_backlog_complete).
    #   SCN_OUT.fullvt   = the FULL VT — the no-altscreen check target (altscreen would
    #                      appear here; the filtered SBLINE view cannot show it).
    #   SCN_OUT.wholelines = WHOLE-line, CR-stripped lines equalling "SBLINE i" — the
    #                      CONTENT-INTEGRITY view (sentinel byte-exact + exactly-once
    #                      multiset; catches duplicates + banner-prefix corruption).
    ZMX_DIR="$ZMX_DIR" zmx history "$name" > "$SCN_OUT.fullvt" 2>/dev/null || true
    cat -v "$SCN_OUT.fullvt" 2>/dev/null | grep -Eo 'SBLINE [0-9]+' > "$SCN_OUT" || true
    cat -v "$SCN_OUT.fullvt" 2>/dev/null | sed 's/\^M$//' | grep 'SBLINE' > "$SCN_OUT.wholelines" || true
    printf '0\n' > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    # L7: no alt-screen takeover in the reattach replay (checked on the FULL VT,
    # which is where altscreen would appear — the filtered SBLINE view cannot show
    # it, so checking it there would be vacuous).
    assert_no_altscreen "$SCN_OUT.fullvt" || return 1
    # L6: every detached-produced line survived, in order (backlog-completeness +
    # scroll-intact — the retained backlog IS the preserved scrollback).
    assert_backlog_complete "$SCN_OUT" "SBLINE " "$SCN_ADR_N" || return 1
    # content-integrity (W3.8): sentinel byte-exact + exactly-once multiset over the
    # WHOLE-line view (catches duplicates AND banner-prefix corruption).
    assert_backlog_multiset_exact "$SCN_OUT.wholelines" "SBLINE " "$SCN_ADR_N"
}
