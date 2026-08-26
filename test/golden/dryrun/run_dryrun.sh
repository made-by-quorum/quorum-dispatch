#!/usr/bin/env bash
# test/golden/dryrun/run_dryrun.sh — Part-1 dry-run driver.
#
# Runs the scenario scripts against the CURRENT TS qd main (NOT a pinned commit)
# to prove each scenario drives real qd and to shape the normalizers. Every
# capture is stamped DRYRUN-NOT-ORACLE and written under dryrun/captures/ — these
# are EVIDENCE, never committed as fixtures/<corpus>/normalized/ expectations.
#
# HARD BOUNDARY (spec §0): this is Part 1. It does NOT record golden expectations.
# Captures here are throwaway evidence of scenario-drives-qd, full stop.
#
# Runs entirely inside the jail (HOME/QD_HOME/ZMX_DIR/... sandboxed). The org's
# real qd on devbox is invisible to this run.
#
# Usage: run_dryrun.sh [scenario-name ...]   (default: the dry-run-safe set)
# Bash 3.2 floor.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"   # test/golden
. "$ROOT/lib/jail.sh"
. "$ROOT/lib/normalize.sh"
. "$ROOT/lib/check_python.sh"

# Enforce the python3 floor before any recording (ADR 0002).
check_python_floor || exit 64

# TS entrypoint (read-only). Current main, NOT pinned — that is the whole point of
# DRYRUN-NOT-ORACLE. Resolved BEFORE any jail establishes (so $HOME is still real).
TS_ENTRY="${TS_ENTRY:-$HOME/work/switchboard/src/index.ts}"
[ -f "$TS_ENTRY" ] || TS_ENTRY="/home/u/work/switchboard/src/index.ts"

CAPDIR="$HERE/captures"
mkdir -p "$CAPDIR"

STAMP="DRYRUN-NOT-ORACLE"
TS_HEAD="$(cd "$(dirname "$TS_ENTRY")/.." 2>/dev/null && git rev-parse --short HEAD 2>/dev/null || echo unknown)"
# Distinguish a dry-run against the FLOATING shared checkout (current main) from one
# against a prep'd PINNED clone (.prep-verified marker present). Both are
# DRYRUN-NOT-ORACLE evidence, never expectations — but the stamp must not lie about
# which it is. (Per the Part-2 plan: dry-running against the shared checkout is no
# longer allowed; dry-running against a prep'd pinned clone IS, still as evidence.)
TS_ENTRY_ROOT="$(cd "$(dirname "$TS_ENTRY")/.." 2>/dev/null && pwd || echo "")"
if [ -n "$TS_ENTRY_ROOT" ] && [ -f "$TS_ENTRY_ROOT/.prep-verified" ]; then
    TS_SOURCE_DESC="PINNED prep clone (HEAD=$TS_HEAD, NOT current main)"
else
    TS_SOURCE_DESC="CURRENT main, NOT a pin (HEAD=$TS_HEAD)"
fi

# Always tear down the active jail on exit/interrupt so a detached jailed daemon
# (a prefixed claude/zmx a scenario started) is never left running on devbox. This
# is the cleanup-on-interrupt guarantee that keeps us invisible to the org's qd.
trap 'jail_teardown 2>/dev/null || true' EXIT INT TERM

# Dry-run-safe scenarios: the surfaces that drive cleanly against current TS main
# without needing a real claude binary (which the bare jail lacks). The session
# scenarios that require a live claude are recorded as DRY-RUN-PARTIAL.
SAFE_SET="ls_info_json zmx_dir_resolution build_claude_cmd"
PARTIAL_SET="new_session_trace send_pty_paste_burst attach_detach_reattach history relay_health"

run_one() {
    local scn_base="$1" partial="$2"
    local scn="$ROOT/scenarios/${scn_base}.sh"
    [ -f "$scn" ] || { printf '[dryrun] missing scenario %s\n' "$scn" >&2; return 1; }

    if ! jail_establish; then
        printf '[dryrun] jail refused for %s\n' "$scn_base" >&2
        return 3
    fi
    # Point the qd-under-test at the TS entrypoint via bun. JAIL_QD_CMD must be a
    # single executable (jail_qd/jail_kill_session/teardown invoke "$JAIL_QD_CMD"
    # <args>), so wrap `bun <entry>` in a tiny shim inside the jail.
    local shim="$JAIL_ROOT/qd-shim"
    {
        printf '#!/usr/bin/env bash\n'
        printf 'exec bun %q "$@"\n' "$TS_ENTRY"
    } > "$shim"
    chmod +x "$shim"
    export QD_UNDER_TEST="$shim"
    export JAIL_QD_CMD="$shim"

    local out="$JAIL_ROOT/dryrun-out.raw"
    SCN_OUT="$out"
    # shellcheck source=/dev/null
    . "$scn"

    # Drive scn_run under a soft budget (don't let a hang wedge the dry-run).
    # On overrun we ALWAYS fall through to jail_teardown below — which now also
    # gc's the jail's own prefixed sessions (catches a detached daemon a
    # hard-killed scenario left behind).
    scn_run >/dev/null 2>&1 &
    local pid=$! waited=0 budget="${SCN_BUDGET_MS:-15000}"
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$waited" -ge "$budget" ]; then
            kill -KILL "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; break
        fi
        sleep 0.2; waited=$((waited + 200))
    done
    wait "$pid" 2>/dev/null

    # Persist a STAMPED, normalized capture under dryrun/captures/.
    local dest="$CAPDIR/${scn_base}"
    mkdir -p "$dest"
    {
        printf '# %s\n' "$STAMP"
        printf '# scenario=%s class=%s fixture=%s\n' "${SCN_NAME:-$scn_base}" "${SCN_CLASS:-?}" "${SCN_FIXTURE:-?}"
        printf '# ts-source=%s recorded=%s\n' "$TS_SOURCE_DESC" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf '# provisional=%s partial=%s\n' "${SCN_PROVISIONAL:-0}" "$partial"
        printf '# NOTE: evidence only — NOT a golden expectation. Part 2 records the real fixture.\n'
        printf '#---raw-capture-below---\n'
    } > "$dest/capture.stamped"
    if [ -f "$SCN_OUT" ]; then
        cat "$SCN_OUT" >> "$dest/capture.stamped"
        # Also a normalized view to shape normalizers. capture.normalized is a PURE
        # normalized capture (no header would corrupt it), so its DRYRUN-NOT-ORACLE
        # provenance is carried in a SIDECAR alongside it — a stranger inspecting the
        # bare normalized file finds the marker next to it and cannot mistake it for
        # a golden expectation (which live ONLY under fixtures/<corpus>/normalized/).
        normalize_all "$JAIL_ROOT" "$JAIL_RUNID" "$JAIL_RELAY_PORT" < "$SCN_OUT" > "$dest/capture.normalized"
        {
            printf '%s\n' "$STAMP"
            printf 'file=capture.normalized\n'
            printf 'scenario=%s class=%s fixture=%s\n' "${SCN_NAME:-$scn_base}" "${SCN_CLASS:-?}" "${SCN_FIXTURE:-?}"
            printf 'ts-source=%s recorded=%s\n' "$TS_SOURCE_DESC" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
            printf 'NOTE: evidence only — NOT a golden expectation. Golden expectations live ONLY under fixtures/<corpus>/normalized/ and are minted by record.sh (Part 2).\n'
        } > "$dest/capture.normalized.sidecar"
    else
        printf '(no capture produced)\n' >> "$dest/capture.stamped"
    fi
    printf '[dryrun] %s -> %s (%s)\n' "$scn_base" "$dest/capture.stamped" "$STAMP"

    jail_teardown
}

main() {
    local targets="$*"
    if [ -z "$targets" ]; then
        for s in $SAFE_SET; do run_one "$s" 0; done
        for s in $PARTIAL_SET; do run_one "$s" 1; done
    else
        for s in $targets; do
            case " $PARTIAL_SET " in *" $s "*) run_one "$s" 1 ;; *) run_one "$s" 0 ;; esac
        done
    fi
    printf '\n[dryrun] done. Captures under %s are %s — never use as expectations.\n' "$CAPDIR" "$STAMP"
}

main "$@"
