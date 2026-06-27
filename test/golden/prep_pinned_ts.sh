#!/usr/bin/env bash
# test/golden/prep_pinned_ts.sh — prepare a PINNED TS clone for recording.
#
# RED-TEAM m2 + scenario-bypass close: golden recordings must drive the qd at a
# RATIFIED pinned commit, never the org's shared ~/work/switchboard checkout
# (whose HEAD floats and whose branches a lead must never switch — ADD-7). This
# script makes a per-run, throwaway, PIN-VERIFIED clone that record.sh requires.
#
# What it does (all OUTSIDE both the org checkout and the qd-rust repo tree):
#   1. CLONE ~/work/switchboard (LOCAL git objects only — no network fetch) into a
#      caller-supplied dir that defaults to a build area NOT under either repo and
#      NOT committed. The source checkout's branches/HEAD are NEVER touched (clone
#      reads objects; it does not switch the source).
#   2. git checkout <pin> in the CLONE, then `git rev-parse HEAD` MUST equal the
#      supplied pin or the script REFUSES (deletes the clone, non-zero exit). This
#      is the anti-drift gate: a wrong/absent pin can never become a recording base.
#   3. bun install in the clone (network allowed HERE — prep runs OUTSIDE the jail).
#      postinstall.ts at the pin is GLOBAL-install-guarded: a local `bun install`
#      (no --global) detects `npm_config_global != "true"` and SKIPS bootstrap
#      entirely (verified read-only at pin: scripts/postinstall.ts isGlobalInstall).
#      We ALSO export SB_SKIP_BOOTSTRAP=1 belt-and-suspenders. Offline fallback:
#      if the bun cache is warm, `bun install --offline` succeeds with no network;
#      pass PREP_BUN_OFFLINE=1 to force it. A bun-install failure is REFUSED.
#   4. Write a MARKER file (.prep-verified) in the clone root containing the
#      verified pin. record.sh requires SB_UNDER_TEST to resolve UNDER a clone whose
#      marker pin matches — closing the red-team scenario-bypass hole (a scenario
#      can no longer be pointed at the floating shared checkout or an arbitrary path).
#
# NEVER touches ~/work/switchboard's checkout/branches/HEAD (clone is read-only on
# the source; we operate only inside the fresh clone).
#
# Usage:
#   prep_pinned_ts.sh --pin <sha> [--dest <dir>] [--src <ts-repo>]
#     --pin   REQUIRED ratified PINNED_TS_COMMIT.
#     --dest  clone dir (default: $PREP_BUILD_DIR or ${TMPDIR:-/tmp}/qd-rust-ts-prep/<pin>).
#             MUST NOT be under the org checkout or the qd-rust repo (refused otherwise).
#     --src   the TS repo to clone from (default: ~/work/switchboard).
#
# On success prints the clone dir + the verified entrypoint path to stdout.
# Bash 3.2 floor.
# ---------------------------------------------------------------------------
set -u
PREP_HERE="$(cd "$(dirname "$0")" && pwd)"

_prep_die() { printf '[prep] REFUSED: %s\n' "$1" >&2; exit "${2:-1}"; }

PIN=""
DEST=""
SRC="${PREP_TS_SRC:-$HOME/work/switchboard}"
[ -d "$SRC" ] || SRC="/home/u/work/switchboard"

while [ "$#" -gt 0 ]; do
    case "$1" in
        --pin)  PIN="${2:-}"; shift 2 ;;
        --dest) DEST="${2:-}"; shift 2 ;;
        --src)  SRC="${2:-}"; shift 2 ;;
        *) _prep_die "unknown arg: $1" 64 ;;
    esac
done

[ -n "$PIN" ] || _prep_die "--pin <sha> is required" 64
[ -d "$SRC/.git" ] || _prep_die "TS source repo not found (no .git): $SRC" 64

# Default dest: a build area NOT under either repo, keyed by pin.
if [ -z "$DEST" ]; then
    DEST="${PREP_BUILD_DIR:-${TMPDIR:-/tmp}/qd-rust-ts-prep}/$PIN"
fi

# REFUSE a dest inside the qd-rust repo tree or inside the org TS checkout — the
# clone must live OUTSIDE both (so it is never committed, never confused with the
# source checkout).
_resolve() { ( cd "$1" 2>/dev/null && pwd ) || printf '%s' "$1"; }
SBRUST_TOP="$(cd "$PREP_HERE/../.." 2>/dev/null && pwd)"   # repo root of qd-rust
SRC_TOP="$(_resolve "$SRC")"
# Resolve dest's existing-parent prefix for the containment check.
_dest_parent="$DEST"
while [ ! -d "$_dest_parent" ] && [ "$_dest_parent" != "/" ] && [ "$_dest_parent" != "." ]; do
    _dest_parent="$(dirname "$_dest_parent")"
done
_dest_real="$(_resolve "$_dest_parent")"
case "$_dest_real/" in
    "$SBRUST_TOP"/*) _prep_die "dest $DEST is under the qd-rust repo ($SBRUST_TOP) — must be outside" 64 ;;
    "$SRC_TOP"/*)    _prep_die "dest $DEST is under the org TS checkout ($SRC_TOP) — must be outside" 64 ;;
esac

# Fresh clone each run: remove a stale dest (only if it is clearly our prep dir).
if [ -e "$DEST" ]; then
    case "$DEST" in
        *qd-rust-ts-prep*|*"$PIN"*) rm -rf "$DEST" 2>/dev/null || true ;;
        *) _prep_die "dest $DEST exists and is not a recognizable prep dir — refusing to remove it" 1 ;;
    esac
fi
mkdir -p "$(dirname "$DEST")" 2>/dev/null || _prep_die "cannot create dest parent for $DEST" 1

# --- 1. CLONE (local objects, no network; source HEAD/branches untouched) ----
# --no-checkout first so we control the checked-out commit explicitly; --local
# uses hardlinks to the source object store and does NOT modify the source.
if ! git clone --local --no-checkout "$SRC" "$DEST" >/dev/null 2>&1; then
    _prep_die "git clone --local from $SRC failed" 1
fi

# --- 2. CHECKOUT the pin in the CLONE, verify HEAD == pin else REFUSE ---------
if ! ( cd "$DEST" && git checkout --quiet "$PIN" ) 2>/dev/null; then
    rm -rf "$DEST" 2>/dev/null || true
    _prep_die "pin $PIN is not reachable in the clone (checkout failed)" 1
fi
HEAD_SHA="$( cd "$DEST" && git rev-parse HEAD 2>/dev/null )"
if [ "$HEAD_SHA" != "$PIN" ]; then
    rm -rf "$DEST" 2>/dev/null || true
    _prep_die "HEAD verify FAILED: clone HEAD=$HEAD_SHA != pin $PIN" 1
fi

# --- 3. bun install in the clone (network allowed; bootstrap skipped) ---------
# Skip when explicitly requested (selftest / offline CI): PREP_SKIP_BUN=1.
if [ "${PREP_SKIP_BUN:-0}" = "1" ]; then
    printf '[prep] bun install SKIPPED (PREP_SKIP_BUN=1)\n' >&2
elif command -v bun >/dev/null 2>&1; then
    _bun_flags=""
    [ "${PREP_BUN_OFFLINE:-0}" = "1" ] && _bun_flags="--offline"
    # SB_SKIP_BOOTSTRAP=1 is belt-and-suspenders; postinstall already skips a
    # local (non-global) install. Run inside the clone.
    if ! ( cd "$DEST" && SB_SKIP_BOOTSTRAP=1 bun install $_bun_flags ) >/dev/null 2>&1; then
        rm -rf "$DEST" 2>/dev/null || true
        _prep_die "bun install in the clone failed (try PREP_BUN_OFFLINE=1 if the cache is warm)" 1
    fi
else
    rm -rf "$DEST" 2>/dev/null || true
    _prep_die "bun not found on PATH; cannot install the pinned clone deps" 1
fi

# --- 4. MARKER: record.sh requires this; contains the verified pin -----------
{
    printf 'PREP-VERIFIED\n'
    printf 'pinned_ts_commit=%s\n' "$PIN"
    printf 'src=%s\n' "$SRC_TOP"
    printf 'prepared=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$DEST/.prep-verified"

ENTRY="$DEST/src/index.ts"
printf '[prep] OK clone=%s pin=%s\n' "$DEST" "$PIN" >&2
printf '%s\n' "$DEST"
[ -f "$ENTRY" ] && printf 'entry=%s\n' "$ENTRY"
exit 0
