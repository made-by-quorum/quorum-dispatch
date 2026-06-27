#!/usr/bin/env bash
# scenario: zmx-dir resolution — semantic (resolution-OUTCOME), ADD-9a reclass.
#
# Corpus entry: zmx-dir resolution (Bug-D keystone, TS src/utils.ts resolveZmxDir
# 68-82). Comparator class WAS byte-exact; ADD-9a Part-2 Step-1 reclassed it to
# semantic (resolution-outcome) — see ADR-0004 DIV-9a-1 (ZMX_DIR tier) and
# DIV-9a-2 (TMPDIR fallback + collapse). WHY: qd exposes no "print my resolved zmx
# dir" surface, so a byte-exact row could only compare a FABRICATED line; the real,
# load-bearing contract is the OUTCOME — a session created under an explicit
# ZMX_DIR has its socket land in THAT dir (reachable/killable there).
#
# STUB-BACKED (§S): drives the pinned-TS `qd new` against the deterministic stub
# (CLAUDE_BIN=jail-rooted shim) so a REAL zmx session is created, then OBSERVES the
# socket dir it landed in and asserts it equals the jail's ZMX_DIR (the explicit
# tier winning outright). The TMPDIR-collapse tier is Linux-leaning at runtime and
# the XDG tier is Linux-ONLY (Step 3, Lima); on macOS the explicit ZMX_DIR outcome
# is the recordable contract.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="zmx-dir-resolution"
SCN_BUDGET_MS=45000
SCN_CLASS="semantic-resolution-outcome"   # ADD-9a reclass (DIV-9a-1/2)
SCN_FIXTURE="fixtures/zmx-dir-resolution/normalized/resolution.txt"
SCN_STUB_BACKED=1

scn_run() {
    local name
    name="$(scn_session_name zd)"
    # Boot a stub-backed session via the pinned-TS qd. startDetached pins the write
    # side to canonicalZmxDir() = ZMX_DIR (explicit tier, utils.ts:70). Capture the
    # boot trace forensically; the OUTCOME is read from where the socket landed.
    scn_capture_pty "$SCN_OUT" 40 -- \
        env SB_UNDER_TEST="$SB_UNDER_TEST" CLAUDE_BIN="${CLAUDE_BIN:-}" \
        sh -c "exec $SB_UNDER_TEST new $name" >/dev/null 2>&1
    printf '%s\n' "$?" > "$SCN_OUT.exit"

    # OUTCOME observation: the session's zmx socket must live under the jail's
    # ZMX_DIR (explicit tier wins). AUTHORITATIVE source = `zmx list` pinned to
    # ZMX_DIR (the same dir resolveZmxDir returns for the explicit tier). Poll
    # briefly so the observation never races boot. resolved = ZMX_DIR iff zmx finds
    # the session there.
    SCN_RESOLVED_DIR=""
    local j=0
    while [ "$j" -lt 15 ]; do
        if ZMX_DIR="$ZMX_DIR" zmx list 2>/dev/null | grep -q "name=$name"; then
            SCN_RESOLVED_DIR="$ZMX_DIR"; break
        fi
        sleep 1; j=$((j + 1))
    done
    {
        printf 'case=ZMX_DIR_explicit expected=%s resolved=%s\n' "$ZMX_DIR" "$SCN_RESOLVED_DIR"
        printf 'case=TMPDIR_fallback note=collapse-outcome-Linux-Step3\n'
        printf 'case=XDG_RUNTIME_DIR note=Linux-only-Step3\n'
    } >> "$SCN_OUT"

    jail_kill_session "$name" >/dev/null 2>&1 || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    # semantic resolution-outcome: the explicit ZMX_DIR tier won — the session's
    # socket landed under the jail's ZMX_DIR (DIV-9a-1). Read the OUTCOME from
    # $SCN_OUT (a FILE) so it is robust to verify.sh running scn_run in a background
    # subshell (shell-var assignments would be lost there).
    local line resolved expected
    line="$(grep '^case=ZMX_DIR_explicit ' "$SCN_OUT" | head -1)"
    expected="$(printf '%s' "$line" | sed -E 's/^case=ZMX_DIR_explicit expected=([^ ]*) resolved=.*/\1/')"
    resolved="$(printf '%s' "$line" | sed -E 's/^.* resolved=//')"
    assert_resolution_outcome "$resolved" "$expected"
}
