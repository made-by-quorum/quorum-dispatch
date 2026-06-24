#!/usr/bin/env bash
# test/golden/scenarios/_scenario_lib.sh — shared helpers for scenario scripts.
#
# Sourced by every scenario AND by verify.sh (which sets SCN_OUT). Scenarios are
# parameterized on the TS entrypoint + pin so Part-2 recording is a re-run:
#
#   SB_UNDER_TEST   — how to invoke the sb-under-test. Default: the TS sb on PATH.
#                     For dry-runs this is `bun <TS-entrypoint>` or just `sb`.
#                     For Part 2 / SBQA swap it points at the Rust binary.
#   PINNED_TS_COMMIT — set ONLY in Part 2. Part 1 dry-runs run against current TS
#                      main and stamp DRYRUN-NOT-ORACLE.
#
# Every scenario runs INSIDE the jail (verify.sh calls jail_establish before
# sourcing the scenario, so SB_HOME/ZMX_DIR/XDG_*/TMPDIR/JAIL_PREFIX are set).
# Scenarios MUST use jail_sb / jail_zmx / the jail-guarded kill helpers — never a
# bare sb/zmx invocation.
# ---------------------------------------------------------------------------

# The sb-under-test command. A space-separated string so it can be `bun entry`.
SB_UNDER_TEST="${SB_UNDER_TEST:-sb}"

# scn_session_name <suffix> — a jail-prefixed, unique session name.
scn_session_name() {
    printf '%s%s' "${JAIL_PREFIX:?jail not established}" "${1:-sess}"
}

# scn_sb <args...> — run the sb-under-test under the hermetic jail env.
# Honors a multi-word SB_UNDER_TEST (e.g. "bun /path/index.ts").
#
# NOTE: scn_sb is for NON-session-targeting invocations (ls, info, config, help,
# --json contract surfaces). For any verb that TARGETS A SESSION BY NAME and could
# act on it — kill / send (send:pty/relay/http) / wait — scenarios MUST use
# scn_sb_target below, which applies the target-resolution belt first.
scn_sb() {
    jail_assert_established || return 1
    # shellcheck disable=SC2086
    $SB_UNDER_TEST "$@"
}

# scn_sb_target <verb> <name> [args...] — session-targeting invocation, BELTED.
#
# A4 finding (orchestrator-ruled): the jailed sb resolves a name through the
# engine's production tiers (incl. the literal-/tmp legacy zmx scan), so a
# name-collision could resolve a kill/send/wait onto a REAL org session. Before the
# verb reaches the engine this asserts the name resolves ONLY under JAIL_ROOT
# (fail-closed on miss / out-of-jail / ambiguity) — the SECOND WALL behind the
# prefix guard. Use this for: kill, send / send:pty / send:relay / send:http, wait.
scn_sb_target() {
    local verb="$1" name="$2"; shift 2
    jail_assert_established || return 1
    jail_guard_name "$name" || return 1
    jail_assert_target_resolves_in_jail "$name" || return 1
    # shellcheck disable=SC2086
    $SB_UNDER_TEST "$verb" "$name" "$@"
}

# scn_capture_pty <outfile> <secs> [extra record_pty args...] -- <cmd...>
# Thin wrapper over the recorder, used by byte-trace scenarios.
scn_capture_pty() {
    local out="$1" secs="$2"; shift 2
    local rec
    rec="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/recorder/record_pty.py"
    python3 "$rec" --out "$out" --secs "$secs" "$@"
}
