#!/usr/bin/env bash
# passb-restamp-replay.sh — the orc-3-ruled re-stamp mechanism for the
# NON-re-recorded stub-backed rows after the stub fidelity fixes (A4 pass-(b)
# F1 dialog strings + F3 timestamp shape; stub 1.5.1 -> 1.7.0).
#
# R1-precedent form (.restamp-evidence.txt): for each row, run the row's OWN
# scenario in a FRESH jail under the COMMITTED new stub, normalize the capture
# EXACTLY as record.sh does (normalize_all <root> <runid> <port>), and
# sha256-compare against the committed normalized golden. BYTE-MATCH -> the row
# is provably untouched by the stub change and its stub_sha256/stub_version
# stamps may be updated WITHOUT re-recording (citing the orc-3 ruling).
# NON-MATCH -> the row JOINS the re-record set. No judgment calls, sha only.
#
# Replay basis = the PINNED TS (the counterpart the goldens were recorded
# against), same as the original tick proofs.
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${RESTAMP_OUT:-$HERE/dryrun/passb-restamp-evidence.txt}"
TS_ENTRY="${TS_ENTRY:-/tmp/qd-rust-ts-prep/8c59ec456fe82780fd75d8afb5fe48dc72e10bc8/src/index.ts}"
export QD_UNDER_TEST="bun $TS_ENTRY"

. "$HERE/lib/jail.sh"
. "$HERE/lib/normalize.sh"
. "$HERE/lib/compare.sh"
. "$HERE/lib/check_python.sh"
. "$HERE/lib/stub_claude/stub_install.sh"

sha() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        sha256sum "$1" | awk '{print $1}'
    fi
}

run_row() {
    local scn_base="$1" fixture_rel="$2"
    local golden="$HERE/$fixture_rel"
    [ -f "$golden" ] || { echo "$scn_base  MISSING-GOLDEN $fixture_rel"; return 1; }

    jail_establish || { echo "$scn_base  JAIL-REFUSED"; return 1; }
    # shellcheck disable=SC2064
    trap "jail_teardown 2>/dev/null || true" EXIT INT TERM
    stub_install || { echo "$scn_base  STUB-INSTALL-FAILED"; jail_teardown; return 1; }

    SCN_OUT="$JAIL_ROOT/scn-out.raw"
    SCN_NORM="$JAIL_ROOT/scn-out.norm"
    SCN_NAME=""; SCN_BUDGET_MS=""; SCN_CLASS=""; SCN_FIXTURE=""; SCN_STUB_BACKED=""
    # shellcheck source=/dev/null
    . "$HERE/scenarios/$scn_base.sh"
    scn_run

    local norm="$JAIL_ROOT/restamp.norm"
    normalize_all "$JAIL_ROOT" "$JAIL_RUNID" "$JAIL_RELAY_PORT" < "$SCN_OUT" > "$norm"
    local r g
    r="$(sha "$norm")"; g="$(sha "$golden")"
    if [ "$r" = "$g" ]; then
        echo "$scn_base  replay_sha=$r  golden_sha=$g  BYTE-MATCH"
    else
        echo "$scn_base  replay_sha=$r  golden_sha=$g  MISMATCH -> JOINS RE-RECORD SET"
    fi
    jail_teardown
    trap - EXIT INT TERM
}

if [ ! -f "$OUT" ]; then
{
    echo "# Re-stamp replay evidence — A4 pass-(b), stub 1.5.1->1.7.0 (F1 dialog strings + F3 timestamp shape)"
    echo "# Mechanism: orc-3 ruling relay-1780664987618-6 (re-stamp on byte-match, re-record on mismatch),"
    echo "# extending the R1 dormancy-replay precedent (.restamp-evidence.txt). Sha-only, no judgment calls."
    echo "# Replay basis: pinned TS 8c59ec4 (prep clone), fresh jails, TMPDIR=/tmp, $(date -u +%Y-%m-%dT%H:%M:%SZ)."
    echo "# committed stub version: $(cat "$HERE/lib/stub_claude/stub_version.txt")"
    echo "#"
} >> "$OUT"
fi

# Each invocation handles ONE row (fresh process => fresh jail, no trap reuse).
row="${1:?usage: passb-restamp-replay.sh <scenario-base> <fixture-rel>}"
fix="${2:?}"
run_row "$row" "$fix" | tee -a "$OUT"
