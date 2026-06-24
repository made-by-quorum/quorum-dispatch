#!/usr/bin/env bash
# test/golden/mutation/mutate.sh — divergence injectors for the mutation test.
#
# Each function reads a known-good capture from stdin (or a file arg) and emits a
# DIVERGED copy to stdout. The mutation test (run_mutation.sh) feeds these to
# `golden verify` and asserts each divergence is CAUGHT. The teeth must bite:
# every mutation -> a caught failure, zero false negatives.
#
# Divergence set (spec §3.8, min): wrong exit code, altered help text, dropped CR,
# reordered replay. Each maps to a comparator class so it hits a REAL check.
#
# Bash 3.2 / POSIX floor.
set -u

# mut_altered_help <file>: change a word in help/output text -> byte-exact diff.
mut_altered_help() {
    # Replace the first occurrence of "Usage" with "Usagex" (a plausible help
    # drift). Keeps everything else identical so only the byte-exact check fires.
    sed 's/Usage/Usagex/' "$1"
}

# mut_dropped_cr <file>: strip carriage returns -> CR-vs-LF byte-exact diff.
# CR vs LF is load-bearing and never normalized, so dropping CR must be caught.
mut_dropped_cr() {
    tr -d '\r' < "$1"
}

# mut_reordered_replay <file> <marker>: swap two backlog lines so the index order
# breaks -> backlog-complete (out-of-order) catch. <marker> e.g. "GLINE ".
mut_reordered_replay() {
    local f="$1"
    # Reverse the order of all lines: a monotonic GLINE 1..N becomes N..1, which
    # the backlog-complete ordering check must reject.
    awk '{ a[NR]=$0 } END { for (i=NR;i>=1;i--) print a[i] }' "$f"
}

# mut_inject_altscreen <file>: prepend an alt-screen enter -> no-altscreen catch.
mut_inject_altscreen() {
    printf '\033[?1049h'
    cat "$1"
}

# mut_wrong_exit_code <code>: emit a wrong exit code value (the .exit sidecar).
# Used by the exit-code comparator path.
mut_wrong_exit_code() {
    printf '%s\n' "${1:-1}"
}

# Dispatch when run directly: mutate.sh <kind> <file> [arg]
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    kind="${1:-}"; file="${2:-}"; arg="${3:-}"
    case "$kind" in
        altered-help)     mut_altered_help "$file" ;;
        dropped-cr)       mut_dropped_cr "$file" ;;
        reordered-replay) mut_reordered_replay "$file" "$arg" ;;
        inject-altscreen) mut_inject_altscreen "$file" ;;
        wrong-exit-code)  mut_wrong_exit_code "$arg" ;;
        *) printf 'mutate.sh: unknown kind %s\n' "$kind" >&2; exit 64 ;;
    esac
fi
