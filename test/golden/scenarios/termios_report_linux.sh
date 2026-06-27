#!/usr/bin/env bash
# scenario: TERMIOS raw-mode report (W4.2, P10, LINUX side). STUB-BACKED. PLATFORM-SPLIT.
# 0b DELTA-STRENGTH W4.2: the Linux sibling of the W3.7 termios row, recorded in-VM
# (Lima `sbtest`, aarch64). NEW recording — NOT a re-record of an existing fixture.
#
# WHAT: boot a stub session, submit the `STTY` prompt (the 1.8.0 prompt-gated termios
# reporter, stub_claude.py), capture the STTY-REPORT line — the termios config of the
# session PTY the ENGINE established on Linux. Comparator class = SEMANTIC on the
# report FIELDS (icanon/echo/isig) — NOT byte-exact over the flag bitmasks (those are
# platform-specific; this is the Linux side, sibling to report.txt's macOS capture).
#
# FIDELITY BOUNDARY (RECORDED FACT, R4-honest — same as the macOS row): the engine (qd)
# does NOT itself put the PTY in raw mode — raw mode is established by the interactive
# TUI app, and the deterministic STUB is a LINE READER, not a raw-mode TUI. So the
# termios the stub observes on the engine-established session PTY is the DEFAULT
# zmx-run COOKED mode. The SEMANTIC booleans (icanon/echo/isig) are the platform-
# INDEPENDENT cooked-mode contract; the raw bitmasks differ Linux vs macOS (why this
# is platform-split). Record HONESTLY whatever Linux cooked-mode presents.
#
# PLATFORM-SPLIT: writes the Linux fixture report-linux.txt with per-platform
# provenance siblings (.linux). The macOS sibling (report.txt) is W3.7.
#
# §S: drives the pinned-TS `qd new` + `qd send:pty STTY` against the stub; the report
# is the stub reading tcgetattr on its stdin (the session PTY the ENGINE set up), so
# the row measures the PTY mode the ENGINE established on Linux, not the stub.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="termios-report-linux"
SCN_BUDGET_MS=60000
SCN_CLASS="semantic-termios"
SCN_FIXTURE="fixtures/termios-report/normalized/report-linux.txt"
SCN_STUB_BACKED=1

scn_run() {
    local name
    name="$(scn_session_name tml)"
    bash -c "exec $SB_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    # Submit the STTY prompt; the stub prints the deterministic termios report to the
    # session PTY (zmx's server-side VT). Read it back via zmx history.
    scn_sb_target send:pty "$name" "STTY" >/dev/null 2>&1
    sleep 2

    ZMX_DIR="$ZMX_DIR" zmx history "$name" 2>/dev/null > "$SCN_OUT.fullvt" || true
    local report
    report="$(cat -v "$SCN_OUT.fullvt" 2>/dev/null | sed 's/\^M$//' | grep -E '^STTY-REPORT ' | tail -1)"

    local icanon echo_v isig present
    present=0
    [ -n "$report" ] && present=1
    icanon="$(printf '%s' "$report" | sed -nE 's/.* icanon=([0-9]+).*/\1/p')"
    echo_v="$(printf '%s' "$report" | sed -nE 's/.* echo=([0-9]+).*/\1/p')"
    isig="$(printf '%s' "$report" | sed -nE 's/.* isig=([0-9]+).*/\1/p')"
    [ -z "$icanon" ] && icanon="MISSING"
    [ -z "$echo_v" ] && echo_v="MISSING"
    [ -z "$isig" ] && isig="MISSING"

    {
        printf 'TERMIOS report_present=%s\n' "$present"
        printf 'TERMIOS icanon=%s\n' "$icanon"
        printf 'TERMIOS echo=%s\n' "$echo_v"
        printf 'TERMIOS isig=%s\n' "$isig"
        printf 'TERMIOS platform=linux\n'
    } > "$SCN_OUT"
    printf '%s\n' "$report" > "$SCN_OUT.rawreport"
    printf '0\n' > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'TERMIOS report_present=1' "$SCN_OUT" || { _cmp_fail semantic-termios "no STTY-REPORT line captured (reporter not reached)"; return 1; }
    # The DETERMINISTIC termios the engine+stub session PTY presents on Linux. Default
    # zmx-run COOKED mode (the stub is a line reader, not a raw-mode TUI). These exact
    # fields are the recorded semantic contract; the MUTANT flips any one (cooked->raw
    # icanon=1->0) and must diff. Linux cooked-mode booleans match macOS (icanon=1
    # echo=1 isig=1) — the platform-INDEPENDENT cooked contract; only the raw bitmasks
    # (forensic, not asserted) differ.
    grep -q 'TERMIOS icanon=1' "$SCN_OUT" || { _cmp_fail semantic-termios "icanon != recorded Linux value (1) — termios mode of the session PTY changed"; return 1; }
    grep -q 'TERMIOS echo=1' "$SCN_OUT"   || { _cmp_fail semantic-termios "echo != recorded Linux value (1) — termios mode of the session PTY changed"; return 1; }
    grep -q 'TERMIOS isig=1' "$SCN_OUT"   || { _cmp_fail semantic-termios "isig != recorded Linux value (1) — termios mode of the session PTY changed"; return 1; }
    return 0
}
