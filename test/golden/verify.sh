#!/usr/bin/env bash
# test/golden/verify.sh — the asserter for the qd-rust golden-master harness.
#
# Pipeline: establish jail -> run a scenario (or replay a capture) under a
# per-case TIMEOUT BUDGET -> normalize -> compare (per the scenario's comparator
# class). Reports a DISTINCT failure taxonomy:
#
#   EXIT_OK        0   pass
#   EXIT_DIFF      1   comparison failed (bytes / invariant)
#   EXIT_DEADLINE  2   the scenario exceeded its timeout budget (LIVENESS regression)
#   EXIT_JAIL      3   jail refused to establish (fail-closed)
#   EXIT_USAGE    64   bad invocation
#
# The deadline failure is DELIBERATELY distinct from a diff failure: a liveness
# regression (a hang that would eventually produce matching bytes) must be caught
# even when the output, given infinite time, would match. (spec §3.1)
#
# Bash 3.2 floor. Portable timeout (no GNU `timeout` dependency — macOS lacks it
# by default): we background the work, poll a deadline, and SIGTERM/SIGKILL on
# overrun, mapping that to EXIT_DEADLINE.
#
# Usage:
#   verify.sh --scenario <scenarios/foo.sh>      run+assert one scenario
#   verify.sh --replay <capture.raw> --class <c> --budget-ms <N> [--expected <f>] ...
#
# A scenario script defines (as shell vars / functions, sourced by this asserter):
#   SCN_NAME, SCN_BUDGET_MS, SCN_CLASS, SCN_FIXTURE
#   scn_run            -> drives qd-under-test in the jail, writes $SCN_OUT(.exit)
#   scn_assert         -> calls comparators on $SCN_OUT; returns 0/nonzero
# See scenarios/_template.sh.
# ---------------------------------------------------------------------------
set -u

EXIT_OK=0
EXIT_DIFF=1
EXIT_DEADLINE=2
EXIT_JAIL=3
EXIT_USAGE=64

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/jail.sh
. "$HERE/lib/jail.sh"
# shellcheck source=lib/normalize.sh
. "$HERE/lib/normalize.sh"
# shellcheck source=lib/compare.sh
. "$HERE/lib/compare.sh"
# shellcheck source=lib/check_python.sh
. "$HERE/lib/check_python.sh"
# shellcheck source=lib/stub_claude/stub_install.sh
. "$HERE/lib/stub_claude/stub_install.sh"

# ---------------------------------------------------------------------------
# run_with_budget <budget_ms> <cmd...>
#
# Run cmd under a wall-clock budget. Returns:
#   the command's own exit code, OR
#   124 if the budget was exceeded (the conventional timeout exit code).
# Portable: no GNU coreutils `timeout`. Background + poll + SIGTERM->SIGKILL.
run_with_budget() {
    local budget_ms="$1"; shift
    # Run in background; capture its PID.
    "$@" &
    local cmd_pid=$!
    local waited=0
    local poll_ms=50
    while kill -0 "$cmd_pid" 2>/dev/null; do
        if [ "$waited" -ge "$budget_ms" ]; then
            # Over budget. Terminate the cmd (and only it).
            kill -TERM "$cmd_pid" 2>/dev/null
            sleep 0.2
            kill -KILL "$cmd_pid" 2>/dev/null
            wait "$cmd_pid" 2>/dev/null
            return 124
        fi
        sleep 0.05
        waited=$(( waited + poll_ms ))
    done
    wait "$cmd_pid"
    return $?
}

# ---------------------------------------------------------------------------
# _verify_stub_provenance — wire the platform-appropriate RECORDED-FROM sibling
# (red-team #6/#7). For a stub-backed scenario, resolve its corpus dir from
# SCN_FIXTURE, pick the platform stamp by host (Linux -> RECORDED-FROM.linux,
# Darwin -> RECORDED-FROM.macos; fall back to the bare RECORDED-FROM), and assert
# the INSTALLED stub's sha (STUB_CLAUDE_SHA256, exported by stub_install) matches
# the stub_sha256 the recording STAMPED. A mismatch means replay is driving a
# DIFFERENT stub than the one the golden was recorded against (R1) — fail-closed.
# This makes the .macos/.linux siblings LOAD-BEARING at replay (not documentary).
# Returns 0 on match/advisory-skip, non-zero on a real provenance mismatch.
_verify_stub_provenance() {
    local fixture_rel="${SCN_FIXTURE:-}"
    [ -n "$fixture_rel" ] || return 0
    # corpus dir = fixtures/<corpus>/ (two dirs up from normalized/<name>).
    local corpus_dir
    corpus_dir="$HERE/$(dirname "$(dirname "$fixture_rel")")"
    [ -d "$corpus_dir" ] || return 0
    # Pick the platform stamp.
    local host plat stamp
    host="$(uname -s 2>/dev/null || echo unknown)"
    case "$host" in
        Linux)  plat="linux" ;;
        Darwin) plat="macos" ;;
        *)      plat="" ;;
    esac
    stamp=""
    if [ -n "$plat" ] && [ -f "$corpus_dir/RECORDED-FROM.$plat" ]; then
        stamp="$corpus_dir/RECORDED-FROM.$plat"
    elif [ -f "$corpus_dir/RECORDED-FROM" ]; then
        stamp="$corpus_dir/RECORDED-FROM"
    fi
    [ -n "$stamp" ] || return 0   # no stamp to check against (advisory skip).
    local stamped_sha
    stamped_sha="$(sed -n 's/^stub_sha256=//p' "$stamp" 2>/dev/null | head -1)"
    [ -n "$stamped_sha" ] || return 0   # pure (non-stub) row carries no sha — skip.
    if [ -n "${STUB_CLAUDE_SHA256:-}" ] && [ "$STUB_CLAUDE_SHA256" != "$stamped_sha" ]; then
        printf '[verify] STUB PROVENANCE MISMATCH (%s): installed stub sha %s != %s stamped %s\n' \
            "$(basename "$stamp")" "$STUB_CLAUDE_SHA256" "$(basename "$stamp")" "$stamped_sha" >&2
        printf '[verify] replay would drive a DIFFERENT stub than the golden was recorded against (R1).\n' >&2
        return 1
    fi
    printf '[verify] stub provenance OK (%s: stub_sha256 matches installed stub)\n' "$(basename "$stamp")" >&2
    return 0
}

verify_scenario() {
    local scn="$1"
    if [ ! -f "$scn" ]; then
        printf '[verify] usage: scenario not found: %s\n' "$scn" >&2
        return $EXIT_USAGE
    fi

    # Enforce the python3 floor before any recording (Option B recorder, ADR 0002).
    if ! check_python_floor; then
        return $EXIT_USAGE
    fi

    # Establish the jail FIRST. Fail closed. Honor an optional caller-supplied
    # SHORT run id (RECORD_RUNID): zmx caps session names at 20 bytes, and the
    # default jail runid (~20 chars) yields a 26-char `sbrg-<runid>-` prefix with
    # no room for a session suffix — so live scenarios that spawn sbrg- sessions
    # (kill/ping) cannot fit a name. Mirrors the same seam in record.sh.
    if ! jail_establish "${RECORD_RUNID:-}"; then
        printf '[verify] JAIL refused — not running scenario %s\n' "$scn" >&2
        return $EXIT_JAIL
    fi
    # Always tear down, even on operator Ctrl-C / SIGTERM mid-scenario — a
    # hard-killed verify must not leak jailed daemons (same pattern as record.sh
    # and run_dryrun.sh). jail_teardown is idempotent, so the explicit calls on
    # the normal paths below are unaffected.
    trap 'jail_teardown 2>/dev/null || true' EXIT INT TERM

    # Per-scenario output area inside the jail.
    SCN_OUT="$JAIL_ROOT/scn-out.raw"
    SCN_NORM="$JAIL_ROOT/scn-out.norm"

    # Source the scenario (it sees the jail + comparator + normalizer functions).
    SCN_NAME=""; SCN_BUDGET_MS=""; SCN_CLASS=""; SCN_FIXTURE=""; SCN_STUB_BACKED=""
    # shellcheck source=/dev/null
    . "$scn"

    if [ -z "${SCN_BUDGET_MS:-}" ]; then
        printf '[verify] scenario %s declares no SCN_BUDGET_MS\n' "$scn" >&2
        jail_teardown
        return $EXIT_USAGE
    fi

    printf '[verify] scenario=%s class=%s budget=%sms\n' \
        "${SCN_NAME:-$scn}" "${SCN_CLASS:-?}" "$SCN_BUDGET_MS" >&2
    # §S substrate: stub-backed scenarios install the deterministic stub as the
    # jail's `claude` binary (jail-rooted CLAUDE_BIN re-export) so the replay drives
    # the SAME counterpart the recording did. Without this a stub-backed scenario
    # would launch a real claude (or none) and hang to its boot timeout.
    if [ "${SCN_STUB_BACKED:-}" = "1" ]; then
        if ! stub_install; then
            printf '[verify] stub install failed for %s\n' "${SCN_NAME:-$scn}" >&2
            jail_teardown
            return $EXIT_JAIL
        fi
        # Wire the platform-appropriate RECORDED-FROM sibling: assert the installed
        # stub matches the stub the golden was recorded against (R1). Fail-closed.
        if ! _verify_stub_provenance; then
            jail_teardown
            return $EXIT_JAIL
        fi
    fi


    # Run the scenario body under the budget.
    run_with_budget "$SCN_BUDGET_MS" scn_run
    local rc=$?
    if [ "$rc" -eq 124 ]; then
        printf '[verify] DEADLINE: scenario %s exceeded %sms budget (liveness regression)\n' \
            "${SCN_NAME:-$scn}" "$SCN_BUDGET_MS" >&2
        jail_teardown
        return $EXIT_DEADLINE
    fi

    # Assert (comparator class). scn_assert returns 0 pass / nonzero diff.
    if scn_assert; then
        printf '[verify] PASS: %s\n' "${SCN_NAME:-$scn}" >&2
        jail_teardown
        return $EXIT_OK
    else
        printf '[verify] DIFF: %s failed its comparator (%s)\n' \
            "${SCN_NAME:-$scn}" "${SCN_CLASS:-?}" >&2
        jail_teardown
        return $EXIT_DIFF
    fi
}

# ---------------------------------------------------------------------------
# Standalone replay mode: assert an EXISTING capture against a class. Used by the
# mutation test and by layer-2 fixtures that ship a pre-built capture.
# verify.sh --replay <capture> --class <c> [--budget-ms N] [--expected f]
#           [--marker M --count N] [--pre f --post f] [--exit-actual a --exit-expected e]
verify_replay() {
    local capture="" class="" budget_ms="" expected=""
    local marker="" count="" pre="" post="" exit_actual="" exit_expected="" resolved=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --replay) capture="$2"; shift 2 ;;
            --class) class="$2"; shift 2 ;;
            --budget-ms) budget_ms="$2"; shift 2 ;;
            --expected) expected="$2"; shift 2 ;;
            --marker) marker="$2"; shift 2 ;;
            --count) count="$2"; shift 2 ;;
            --pre) pre="$2"; shift 2 ;;
            --post) post="$2"; shift 2 ;;
            --exit-actual) exit_actual="$2"; shift 2 ;;
            --exit-expected) exit_expected="$2"; shift 2 ;;
            --resolved) resolved="$2"; shift 2 ;;
            *) printf '[verify] unknown replay arg: %s\n' "$1" >&2; return $EXIT_USAGE ;;
        esac
    done
    if [ -z "$class" ]; then
        printf '[verify] --replay requires --class\n' >&2
        return $EXIT_USAGE
    fi

    case "$class" in
        byte-exact)
            # Normalize the capture, then byte-compare to the normalized expected.
            local tmp_norm tmp_exp
            tmp_norm="$(mktemp)"; tmp_exp="$(mktemp)"
            normalize_all "" "" "" < "$capture" > "$tmp_norm"
            normalize_all "" "" "" < "$expected" > "$tmp_exp"
            if compare_byte_exact "$tmp_exp" "$tmp_norm"; then
                rm -f "$tmp_norm" "$tmp_exp"; return $EXIT_OK
            fi
            rm -f "$tmp_norm" "$tmp_exp"; return $EXIT_DIFF
            ;;
        no-altscreen)
            assert_no_altscreen "$capture" && return $EXIT_OK; return $EXIT_DIFF ;;
        backlog-complete)
            assert_backlog_complete "$capture" "$marker" "$count" && return $EXIT_OK; return $EXIT_DIFF ;;
        backlog-multiset)
            assert_backlog_multiset_exact "$capture" "$marker" "$count" && return $EXIT_OK; return $EXIT_DIFF ;;
        scroll-intact)
            assert_scroll_intact "$pre" "$post" && return $EXIT_OK; return $EXIT_DIFF ;;
        exit-code)
            assert_exit_code "$exit_actual" "$exit_expected" && return $EXIT_OK; return $EXIT_DIFF ;;
        resolution-outcome)
            assert_resolution_outcome "$resolved" "$expected" && return $EXIT_OK; return $EXIT_DIFF ;;
        boot-readiness-event)
            assert_boot_readiness_event "$capture" && return $EXIT_OK; return $EXIT_DIFF ;;
        submit-discipline|semantic-submit-discipline)
            assert_submit_discipline "$capture" && return $EXIT_OK; return $EXIT_DIFF ;;
        *)
            printf '[verify] unknown comparator class: %s\n' "$class" >&2
            return $EXIT_USAGE ;;
    esac
}

# ---------------------------------------------------------------------------
main() {
    if [ "$#" -lt 1 ]; then
        printf 'usage: verify.sh --scenario <f> | --replay <cap> --class <c> ...\n' >&2
        exit $EXIT_USAGE
    fi
    case "$1" in
        --scenario)
            verify_scenario "$2"; exit $?
            ;;
        --replay)
            verify_replay "$@"; exit $?
            ;;
        *)
            printf 'usage: verify.sh --scenario <f> | --replay <cap> --class <c> ...\n' >&2
            exit $EXIT_USAGE
            ;;
    esac
}

# Only run main if executed (not sourced — the mutation test sources us).
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    main "$@"
fi
