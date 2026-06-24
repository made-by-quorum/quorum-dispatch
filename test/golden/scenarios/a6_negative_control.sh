#!/usr/bin/env bash
# test/golden/scenarios/a6_negative_control.sh — A6 G-A1: ported surfaces are
# byte-UNAFFECTED by the A6 code paths (spec §7 G-A1; ADDITIVE-NOT-PARITY).
#
# Two modes:
#   leg:  SB_BIN=<binary> CAPTURE_DIR=<dir> bash $0 leg
#         → run the fixed ported-surface row set against SB_BIN in a FRESH jail,
#           capture stdout/stderr/exit per row into CAPTURE_DIR (raw).
#   diff: bash $0 diff <dirBASE> <dirA6>
#         → normalize both captures with the EXISTING recorded normalizer set
#           (lib/normalize.sh normalize_all — timestamps/PIDs/jail-paths/run-ids
#           ONLY; nothing else, laundering guard per spec §7 G-A1) and byte-diff.
#
# Row set (ported surfaces, NO --via, NO ANTHROPIC_* set):
#   r1 new        — sb new <name> (stub CLAUDE_BIN), stdout+stderr+exit
#   r2 ls         — sb ls
#   r3 ls-short   — sb ls --all --short
#   r4 ls-json    — sb ls --json
#   r5 info       — sb info <name>
#
# Determinism: fixed session name, fixed stub sessionId/startedAt; the remaining
# volatile bytes (pid, jail paths, timestamps) are exactly the recorded
# normalizer classes. Bash 3.2.
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
MODE="${1:-}"

if [ "$MODE" = "diff" ]; then
    A="${2:?dirBASE}"; B="${3:?dirA6}"
    # shellcheck source=../lib/normalize.sh
    . "$HERE/../lib/normalize.sh"
    # Per-leg jail params recorded by the leg (JAILMETA: root runid port uid) so
    # normalize_all tokenizes each side's OWN volatile run identity.
    read -r AROOT ARUNID APORT AUID < "$A/.jailmeta"
    read -r BROOT BRUNID BPORT BUID < "$B/.jailmeta"
    APID="$(sed -n '2p' "$A/.jailmeta")"
    BPID="$(sed -n '2p' "$B/.jailmeta")"
    fail=0
    for f in "$A"/*; do
        base="$(basename "$f")"
        case "$base" in .jailmeta) continue;; esac
        if [ ! -f "$B/$base" ]; then
            echo "  [FAIL] $base missing from A6 capture"; fail=1; continue
        fi
        # Exact-value PID substitution (BSD sed has no \b; the exact 5-6 digit
        # value is unambiguous post-normalization — timestamps already tokenized).
        na="$(normalize_all "$AROOT" "$ARUNID" "$APORT" "$AUID" < "$f" \
              | sed "s/${APID:-NOPID}/<PID>/g")"
        nb="$(normalize_all "$BROOT" "$BRUNID" "$BPORT" "$BUID" < "$B/$base" \
              | sed "s/${BPID:-NOPID}/<PID>/g")"
        if [ "$na" = "$nb" ]; then
            echo "  [ok] $base byte-identical (normalized: existing set only)"
        else
            echo "  [FAIL] $base DIFFERS:"
            diff <(printf '%s\n' "$na") <(printf '%s\n' "$nb") | head -20
            fail=1
        fi
    done
    # Also require the same file SET both sides.
    for f in "$B"/*; do
        base="$(basename "$f")"
        case "$base" in .jailmeta) continue;; esac
        [ -f "$A/$base" ] || { echo "  [FAIL] $base only in A6 capture"; fail=1; }
    done
    if [ "$fail" = 0 ]; then
        echo "A6-NEGCONTROL-DIFF: PASS"
        exit 0
    fi
    echo "A6-NEGCONTROL-DIFF: FAIL"
    exit 1
fi

if [ "$MODE" != "leg" ]; then
    echo "usage: SB_BIN=<bin> CAPTURE_DIR=<dir> $0 leg | $0 diff <dirBASE> <dirA6>"
    exit 2
fi

: "${SB_BIN:?leg mode needs SB_BIN}"
: "${CAPTURE_DIR:?leg mode needs CAPTURE_DIR}"
mkdir -p "$CAPTURE_DIR"

cd "$REPO_ROOT" || exit 1
# shellcheck source=../lib/jail.sh
. "$HERE/../lib/jail.sh"
# SHORT runid (a6_routing.sh pattern): zmx caps names at 20 bytes; the default
# long runid pushes `sbrg-<runid>-negctl` over the cap → the Bug-D error path
# (the A4 F2 lesson class). 4 chars keeps the success path live.
SHORT_RUNID="$(printf '%s' "${RANDOM:-0}${RANDOM:-0}" | tr -cd 'a-z0-9' | cut -c1-4)"
[ -n "$SHORT_RUNID" ] || SHORT_RUNID="z$$"
jail_establish "$SHORT_RUNID" || { echo "FATAL: jail_establish failed"; exit 1; }
trap 'jail_teardown' EXIT
# Record this leg's volatile run identity for the diff-mode normalizers.
printf '%s %s %s %s\n' "$JAIL_ROOT" "$JAIL_RUNID" "${JAIL_RELAY_PORT:-}" "$(id -u)" \
    > "$CAPTURE_DIR/.jailmeta"

# Stub claude: registry entry with FIXED sessionId/startedAt (pid is the one
# unavoidable volatile, an existing normalizer class), then sleep.
STUB="$JAIL_ROOT/stub-claude"
cat > "$STUB" <<'EOS'
#!/bin/bash
name=""
while [ $# -gt 0 ]; do
  case "$1" in
    --name) name="$2"; shift 2;;
    *) shift;;
  esac
done
[ -n "${SBRG_STUB_NAME:-}" ] && name="$SBRG_STUB_NAME"
mkdir -p "$HOME/.claude/sessions"
printf '{"pid":%d,"name":"%s","status":"idle","sessionId":"negctl-fixed-sid","cwd":"%s","version":"stub","kind":"interactive","entrypoint":"cli","startedAt":1700000000000,"updatedAt":1700000000000}\n' \
  "$$" "$name" "$PWD" > "$HOME/.claude/sessions/$$.json"
exec sleep 120
EOS
chmod +x "$STUB"
export CLAUDE_BIN="$STUB"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"
NAME="${JAIL_PREFIX}negctl"

run_row() {
    local row="$1"; shift
    ( cd "$WORKDIR" && env SBRG_STUB_NAME="$NAME" "$SB_BIN" "$@" ) \
        > "$CAPTURE_DIR/$row.out" 2> "$CAPTURE_DIR/$row.err"
    echo "exit=$?" > "$CAPTURE_DIR/$row.exit"
}

# Belt: NO ANTHROPIC_* may leak into the rows from the runner shell.
for v in ANTHROPIC_BASE_URL ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_MODEL; do
    unset "$v" 2>/dev/null || true
done

run_row r1-new  new "$NAME" --cwd "$WORKDIR"

# Record the stub's PID (the one per-leg volatile the stock normalize_pids
# misses in bare-cell/JSON positions). EXACT-value substitution in diff mode —
# no pattern widening (laundering guard).
STUB_PID="$(ls "$HOME/.claude/sessions/" 2>/dev/null | sed -n 's/^\([0-9][0-9]*\)\.json$/\1/p' | head -1)"
printf '%s\n' "${STUB_PID:-none}" >> "$CAPTURE_DIR/.jailmeta"

run_row r2-ls   ls
run_row r3-lsas ls --all --short
run_row r4-lsj  ls --json
run_row r5-info info "$NAME"

echo "A6-NEGCONTROL-LEG: captured 5 rows to $CAPTURE_DIR (SB_BIN=$SB_BIN)"
