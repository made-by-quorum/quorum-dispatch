#!/usr/bin/env bash
# test/golden/lib/prep_verify.sh — verify an entrypoint lives under a prep'd clone.
#
# Closes the red-team scenario-bypass hole: record.sh must REFUSE a QD_UNDER_TEST
# that does NOT resolve under a clone prep_pinned_ts.sh verified (a clone whose
# .prep-verified marker pin matches the ratified pin). Without this, a scenario
# could be pointed at the floating shared ~/work/switchboard checkout (un-pinned)
# or any arbitrary path, and the recording would bake un-pinned behavior.
#
# prep_verify_entrypoint <sb_under_test> <expected_pin>
#   sb_under_test  — the value scenarios will run (e.g. "bun <clone>/src/index.ts"
#                    or a path/shim). We walk UP from the first existing path token
#                    looking for a .prep-verified marker.
#   expected_pin   — the ratified pin the marker must claim.
# Returns 0 (and echoes the clone dir) if a marker is found whose pin matches;
# non-zero + a refusal message otherwise.
#
# Bash 3.2 floor.
# ---------------------------------------------------------------------------

_pv_refuse() { printf '[prep-verify] REFUSED: %s\n' "$1" >&2; }

# Extract the first token from QD_UNDER_TEST that looks like a filesystem path
# (contains a '/'). Handles "bun /path/index.ts" and a bare "/path/shim".
_pv_path_token() {
    local s="$1" tok
    for tok in $s; do
        case "$tok" in
            */*) printf '%s' "$tok"; return 0 ;;
        esac
    done
    return 1
}

prep_verify_entrypoint() {
    local sut="${1:-}" expected_pin="${2:-}"
    if [ -z "$sut" ] || [ -z "$expected_pin" ]; then
        _pv_refuse "usage: prep_verify_entrypoint <sb_under_test> <expected_pin>"
        return 64
    fi
    local p
    p="$(_pv_path_token "$sut")" || { _pv_refuse "QD_UNDER_TEST has no path token: '$sut'"; return 1; }

    # Walk up from the directory of that path token looking for .prep-verified.
    local dir
    if [ -d "$p" ]; then dir="$p"; else dir="$(dirname "$p")"; fi
    dir="$(cd "$dir" 2>/dev/null && pwd || printf '%s' "$dir")"
    local guard=0
    while [ -n "$dir" ] && [ "$dir" != "/" ] && [ "$guard" -lt 64 ]; do
        if [ -f "$dir/.prep-verified" ]; then
            local marker_pin
            marker_pin="$(sed -n 's/^pinned_ts_commit=//p' "$dir/.prep-verified" 2>/dev/null | head -1)"
            if [ "$marker_pin" = "$expected_pin" ]; then
                printf '%s\n' "$dir"
                return 0
            fi
            _pv_refuse "found a prep marker at $dir but its pin ($marker_pin) != expected ($expected_pin)"
            return 1
        fi
        dir="$(dirname "$dir")"
        guard=$((guard + 1))
    done
    _pv_refuse "QD_UNDER_TEST '$sut' does not resolve under a prep-verified clone (no .prep-verified marker with pin $expected_pin found above $p)"
    _pv_refuse "run prep_pinned_ts.sh --pin $expected_pin first, then point QD_UNDER_TEST at <clone>/src/index.ts"
    return 1
}
