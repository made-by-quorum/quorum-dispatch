#!/usr/bin/env bash
# scenario: TERMIOS raw-mode report (W3.7, P10, macOS side). STUB-BACKED. PLATFORM-SPLIT.
# 0b DELTA-STRENGTH W3.7: the cheap engine-behaviour portion of the P10 row (new).
#
# WHAT: boot a stub session, submit the `STTY` prompt (the 1.8.0 prompt-gated termios
# reporter, stub_claude.py:212-248), and capture the STTY-REPORT line — the termios
# config of the session PTY the ENGINE established. Comparator class = SEMANTIC on the
# report FIELDS (icanon/echo/isig) — NOT byte-exact over the whole flag bitmasks
# (those are platform-specific; this is the macOS side).
#
# FIDELITY BOUNDARY (RECORDED FACT, R4-honest — FLAGGED to the lead): the spec/ADR
# framed this as "the raw-mode config the engine establishes." EMPIRICALLY, the
# engine (qd) does NOT itself put the PTY in raw mode — RAW MODE is established by the
# interactive TUI app (real Claude Code), and the deterministic STUB is NOT a
# raw-mode TUI (it is a line reader). So the termios the stub observes on the
# engine-established session PTY is the DEFAULT zmx-run COOKED mode: icanon=1, echo=1,
# isig=1 (recorded macOS). This row therefore records the DETERMINISTIC termios the
# ENGINE+stub PTY presents (the cheap engine-behaviour portion per ADR-0010 §(a)) and
# asserts its EXACT semantic fields; raw-mode TUI realism stays A4/C2 (a stub that set
# raw mode would be a fidelity edit = a 2nd stub bump, out of scope). The MUTANT is a
# raw-mode substitution (icanon=0/echo=0) — the INVERSE of the spec's cooked-mode
# example, because the recorded reality is cooked — and it must flip red.
#
# REALISM BOUNDARY (ADR-0010 §(a)): this is the CHEAP engine-behaviour portion only.
# Full repaint / SIGWINCH / real raw-TUI realism stays A4/C2.
#
# PLATFORM-SPLIT (like zmx-dir-resolution): the flag BITMASKS differ macOS vs Linux,
# so the fixture carries per-platform provenance siblings (.macos here; .linux is W4
# in-VM). The SEMANTIC assertion (icanon=0 echo=0) is the platform-INDEPENDENT
# raw-mode contract; the recorded report.txt is the macOS capture.
#
# COLLISION CHECK (W2.3): no existing scenario submits a line starting `STTY` (the
# prompt namespace already holds EMIT) — verified at the stub doctrine header.
#
# §S: drives the pinned-TS `qd new` + `qd send:pty STTY` against the stub; the report
# is the stub reading tcgetattr on its stdin (the session PTY the ENGINE set up), so
# the row measures the PTY mode the ENGINE established, not the stub.
#
# Determinism (double-record): the SEMANTIC report fields (icanon/echo/isig) are the
# recorded expectation. The raw bitmasks are captured forensically but NOT the
# comparison target (they are platform-specific; the semantic booleans are stable).
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="termios-report"
SCN_BUDGET_MS=60000
SCN_CLASS="semantic-termios"
SCN_FIXTURE="fixtures/termios-report/normalized/report.txt"
SCN_STUB_BACKED=1

scn_run() {
    local name
    name="$(scn_session_name tm)"
    bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    # Submit the STTY prompt; the stub prints the deterministic termios report to the
    # session PTY (zmx's server-side VT). Read it back via zmx history.
    scn_qd_target send:pty "$name" "STTY" >/dev/null 2>&1
    sleep 2

    ZMX_DIR="$ZMX_DIR" zmx history "$name" 2>/dev/null > "$SCN_OUT.fullvt" || true
    # Extract the STTY-REPORT line (the last one, in case the prompt echoes too).
    local report
    report="$(cat -v "$SCN_OUT.fullvt" 2>/dev/null | sed 's/\^M$//' | grep -E '^STTY-REPORT ' | tail -1)"

    # Parse the SEMANTIC fields (the raw-mode booleans). These are platform-
    # independent; the flag bitmasks are not (kept forensically only).
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
    } > "$SCN_OUT"
    # Keep the raw report line as a forensic sibling (platform-specific bitmasks).
    printf '%s\n' "$report" > "$SCN_OUT.rawreport"
    printf '0\n' > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'TERMIOS report_present=1' "$SCN_OUT" || { _cmp_fail semantic-termios "no STTY-REPORT line captured (reporter not reached)"; return 1; }
    # The DETERMINISTIC termios the engine+stub session PTY presents (macOS). The
    # session PTY is in the default zmx-run COOKED mode (the stub is a line reader,
    # not a raw-mode TUI — fidelity boundary above). These exact fields are the
    # recorded semantic contract; the MUTANT flips any one (e.g. cooked->raw
    # icanon=1->0) and must diff.
    grep -q 'TERMIOS icanon=1' "$SCN_OUT" || { _cmp_fail semantic-termios "icanon != recorded macOS value (1) — termios mode of the session PTY changed"; return 1; }
    grep -q 'TERMIOS echo=1' "$SCN_OUT"   || { _cmp_fail semantic-termios "echo != recorded macOS value (1) — termios mode of the session PTY changed"; return 1; }
    grep -q 'TERMIOS isig=1' "$SCN_OUT"   || { _cmp_fail semantic-termios "isig != recorded macOS value (1) — termios mode of the session PTY changed"; return 1; }
    return 0
}
