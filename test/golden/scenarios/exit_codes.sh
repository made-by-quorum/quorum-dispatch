#!/usr/bin/env bash
# scenario: exit codes 1 / 2 — REAL qd error paths driven in the jail.
#
# Corpus entry: "exit codes 0 / 1 / 2" (red-team M3 — the matrix row had no
# producing scenario, so the exit-code tick would have been vacuous). This drives
# the REAL qd error surfaces and records the exit code as the load-bearing
# contract (the comparator class is `exit-code`; exit codes are NEVER normalized
# per ADR-0003).
#
#   exit 1 — operational failure: `qd info <missing-session>` against the jail's
#            EMPTY registry. qd prints `No session matching "<name>"` to stderr and
#            exits 1. (Confirmed at current TS main via the a3-cli dryrun corpus
#            44-fail-info-nosuch: exit 1; re-confirmed against the pinned clone at
#            record time.)
#   exit 2 — usage/argument error: `qd config <unknown-subcommand>`. qd's config
#            verb rejects an unknown subcommand with a usage message and exits 2.
#            (a3-cli 40-fail-config-nosuchsub: exit 2 at current main.)
#
# exit 0 (the success class) is already covered by ls_info_json.sh's .exit=0, so
# this scenario fills the 1 and 2 cells the matrix was missing. Each command's
# exit code is captured in its OWN .exit sidecar; the combined trace records the
# command + exit so the byte-of-the-trace is the structural evidence and the
# .exit sidecars are the load-bearing comparator inputs.
#
# Runs entirely in the jail (empty registry guaranteed by the sandboxed HOME).
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="exit-codes"
SCN_BUDGET_MS=8000
SCN_CLASS="exit-code"
# Primary fixture path (the combined trace). The per-class exit fixtures live
# alongside under fixtures/exit-codes/raw/*.exit (load-bearing; never normalized).
SCN_FIXTURE="fixtures/exit-codes/normalized/exit-codes.trace"

scn_run() {
    : > "$SCN_OUT"

    # --- exit 1: info on a missing session (empty jail registry). ------------
    local out1 rc1
    out1="$(scn_sb info "${JAIL_PREFIX}no-such-session" 2>&1)"
    rc1=$?
    {
        printf 'cmd=info-missing-session expect_exit=1 got_exit=%s\n' "$rc1"
        printf 'stderr_present=%s\n' "$( [ -n "$out1" ] && echo 1 || echo 0 )"
    } >> "$SCN_OUT"
    printf '%s\n' "$rc1" > "$SCN_OUT.exit1"

    # --- exit 2: usage error — unknown config subcommand. --------------------
    local out2 rc2
    out2="$(scn_sb config nosuchsub 2>&1)"
    rc2=$?
    {
        printf 'cmd=config-unknown-subcommand expect_exit=2 got_exit=%s\n' "$rc2"
        printf 'stderr_present=%s\n' "$( [ -n "$out2" ] && echo 1 || echo 0 )"
    } >> "$SCN_OUT"
    printf '%s\n' "$rc2" > "$SCN_OUT.exit2"

    # The scenario itself succeeds (it is a harness driver). The .exit sidecar the
    # recorder pairs with the primary fixture reflects driver success; the
    # load-bearing per-command codes are in .exit1 / .exit2.
    printf '0\n' > "$SCN_OUT.exit"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    # The contract: info-missing exits 1, config-unknown-sub exits 2. These are
    # the REAL qd exit codes — the asserter checks them as the load-bearing class.
    [ "$(cat "$SCN_OUT.exit1" 2>/dev/null)" = "1" ] || return 1
    [ "$(cat "$SCN_OUT.exit2" 2>/dev/null)" = "2" ] || return 1
}
