#!/usr/bin/env bash
# scenario: ls/info --json  — byte-exact JSON contract surface.
#
# Corpus entry: `ls/info --json`. Deterministic after normalization, so
# comparator class = byte-exact. Drives sb-under-test in the jail with an empty
# registry (and, for a non-empty case, would create a jailed session — Part 2).
#
# Parameterized on SB_UNDER_TEST so Part-2 recording is a re-run. verify.sh has
# already established the jail and set SCN_OUT before sourcing this file.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="ls-info-json"
SCN_BUDGET_MS=8000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/ls-info-json/normalized/ls-empty.json"

scn_run() {
    # Empty-registry ls --json: the simplest deterministic JSON contract. The
    # jail guarantees an empty registry (HOME is sandboxed), so output is stable.
    scn_sb ls --json > "$SCN_OUT" 2>/dev/null
    printf '%s\n' "$?" > "$SCN_OUT.exit"
}

scn_assert() {
    # Part 1: there is no recorded expectation yet (Part 2 records it). We assert
    # the SHAPE that the byte-exact comparator will later enforce: valid JSON, and
    # exit code 0. This proves the scenario drives the surface; the byte-golden
    # tick is Part 2.
    [ -f "$SCN_OUT" ] || return 1
    [ "$(cat "$SCN_OUT.exit" 2>/dev/null)" = "0" ] || return 1
    # Valid-JSON smoke (python is already a harness dep).
    python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$SCN_OUT" 2>/dev/null
}
