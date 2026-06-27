#!/usr/bin/env bash
# scenario: a5rec ping — TS qd `ping` liveness classifications (exit 0=done
# 1=stuck 2=active 3=error 4=ambiguous). Forges registry status/turns/timestamps
# to drive done/active/ambiguous deterministically, plus the no-target error and
# the --prefix sweep + --json shapes. Volatile age=/uptime= counters are
# collapsed to <DUR> by normalize_durations. Pin 0d0fa9e.
# tooling: record.sh@388ccd9 normalize.sh@b581f75.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_a5_lifecycle_lib.sh"

SCN_NAME="a5rec-ping"
SCN_BUDGET_MS=20000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/a5-lifecycle/normalized/ping.txt"

scn_run() {
    # Each forged entry needs its OWN live PID (the ping liveness derivation reads
    # the registry's pid and checks it is alive — idle/busy require a LIVE pid, and
    # the registry file is keyed on the pid, so distinct pids also avoid file
    # collisions). Spawn three throwaway sleeps inside the jail and reap them after.
    sleep 30 & local P_DONE=$!
    sleep 30 & local P_ACTV=$!
    sleep 30 & local P_AMBG=$!
    {
        echo "# RECORDED-FROM pin=0d0fa9e verb=ping (forged status/turns; age=/uptime= → <DUR>)"
        echo "\$ qd ping (no target → error exit 3)"
        scn_sb ping 2>&1; echo "exit=$?"
        echo "\$ qd ping --prefix sbrg- (empty registry)"
        scn_sb ping --prefix sbrg- 2>&1; echo "exit=$?"
        echo "\$ qd ping --prefix sbrg- --json (empty)"
        scn_sb ping --prefix sbrg- --json 2>&1; echo "exit=$?"
    } > "$SCN_OUT"
    # DONE: idle, recent (uptime < 300) → exit 0.
    a5_forge_registry "${JAIL_PREFIX}done" idle 5 "$P_DONE" 60
    # ACTIVE: busy, recent (uptime < 600) → exit 2.
    a5_forge_registry "${JAIL_PREFIX}actv" busy 4 "$P_ACTV" 60
    # AMBIGUOUS: idle, 0 turns, uptime > 300 → exit 4.
    a5_forge_registry "${JAIL_PREFIX}ambg" idle 0 "$P_AMBG" 600
    {
        echo "\$ qd ping sbrg-done (idle recent → done exit 0)"
        # Isolate each target: ping by exact name reads only that entry.
        scn_sb ping "${JAIL_PREFIX}done" 2>&1; echo "exit=$?"
        echo "\$ qd ping sbrg-actv (busy recent → active exit 2)"
        scn_sb ping "${JAIL_PREFIX}actv" 2>&1; echo "exit=$?"
        echo "\$ qd ping sbrg-ambg (idle 0-turns aged → ambiguous exit 4)"
        scn_sb ping "${JAIL_PREFIX}ambg" 2>&1; echo "exit=$?"
    } >> "$SCN_OUT"
    kill "$P_DONE" "$P_ACTV" "$P_AMBG" 2>/dev/null
    printf '0\n' > "$SCN_OUT.exit"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q "qd ping: provide a <session> or --prefix" "$SCN_OUT" || return 1
    grep -q "No sessions matching 'sbrg-'" "$SCN_OUT" || return 1
    grep -q "${JAIL_PREFIX}done: status=idle" "$SCN_OUT" || return 1
    grep -q "${JAIL_PREFIX}actv: status=busy" "$SCN_OUT" || return 1
    grep -q "AMBIGUOUS" "$SCN_OUT" || return 1
}
