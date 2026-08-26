#!/usr/bin/env bash
# WS-A.2 pi-provider identity env-leak fix — evidence-grade suite runner.
# Tees FULL cargo output to a log the rtk shell hook cannot summarize away
# (the ci.sh pattern). Touched crate ONLY (quorum-dispatch); NEVER --workspace
# (duckdb breaks on this mac). The ambient session identity is scrubbed from the
# test env so it cannot pollute identity-resolution tests.
# Usage: bash run-envfix-suite.sh [logfile] [suite ...]
#   With no suite args, runs the full set. With suite args, runs only those
#   (e.g. `bash run-envfix-suite.sh /tmp/b.log build lib`).
set -uo pipefail

# cargo must run from the WORKSPACE ROOT (the dir whose Cargo.toml carries the
# `[workspace]` table) — that is what makes `-p quorum-dispatch` resolvable and
# what `git rev-parse HEAD` below is reporting on. That root is NOT at a fixed
# depth relative to this script: in the monorepo this file sits at
# `<root>/dispatch/run-envfix-suite.sh` (workspace root one level UP), and when
# `dispatch/` is itself the repo root it sits at `<root>/run-envfix-suite.sh`
# (workspace root right HERE). So probe upward for the marker instead of hopping
# a hard-coded `..`.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$HERE"
while [ ! -f "$ROOT/Cargo.toml" ] || ! grep -q '^\[workspace\]' "$ROOT/Cargo.toml"; do
  parent="$(dirname "$ROOT")"
  if [ "$parent" = "$ROOT" ]; then
    echo "FATAL: no Cargo.toml with a [workspace] table at or above $HERE" >&2
    exit 2
  fi
  ROOT="$parent"
done
cd "$ROOT" || exit 2

# Default log lands beside this script (i.e. in dispatch/), the same file the old
# cwd-relative `dispatch/envfix-suite-*.log` produced — now depth-independent.
LOG="${1:-$HERE/envfix-suite-$(date +%Y%m%d-%H%M%S).log}"
shift 2>/dev/null || true
SUITES=("$@")
if [ "${#SUITES[@]}" -eq 0 ]; then
  SUITES=(build lib binqd pi_chaos pi_redteam p0_id_matrix adopt_cli acp_chaos)
fi

# Also unset QD_HOME: several bin-qd tests resolve state via RealEnv and an
# ambient QD_HOME points them at the machine's REAL state dir (shared, non-empty)
# → a "exactly one record" hermeticity flake unrelated to any change under test.
CARGO=(env -u QD_SESSION_ID -u CLAUDE_CODE_SESSION_ID -u CLAUDECODE -u QD_HOME cargo)

declare -A RC
run() {
  local key="$1"
  shift
  echo "===== $key: cargo $* ====="
  "${CARGO[@]}" "$@" 2>&1
  RC[$key]=$?
  echo "$key exit: ${RC[$key]}"
  echo
}

{
  echo "===== ENVFIX SUITE $(date -u +%Y-%m-%dT%H:%M:%SZ) ====="
  echo "HEAD: $(git rev-parse HEAD)"
  echo "branch: $(git branch --show-current)"
  echo "suites: ${SUITES[*]}"
  echo

  for s in "${SUITES[@]}"; do
    case "$s" in
    build) run build build -p quorum-dispatch ;;
    lib) run lib test -p quorum-dispatch --lib ;;
    binqd) run binqd test -p quorum-dispatch --bin qd ;;
    pi_chaos) run pi_chaos test -p quorum-dispatch --test pi_chaos ;;
    pi_redteam) run pi_redteam test -p quorum-dispatch --test pi_redteam ;;
    p0_id_matrix) run p0_id_matrix test -p quorum-dispatch --test p0_id_matrix ;;
    adopt_cli) run adopt_cli test -p quorum-dispatch --test adopt_cli ;;
    acp_chaos) run acp_chaos test -p quorum-dispatch --test acp_chaos ;;
    *) echo "unknown suite: $s"; RC[$s]=99 ;;
    esac
  done

  echo "===== SUMMARY ====="
  allgreen=1
  for k in "${SUITES[@]}"; do
    echo "  $k = ${RC[$k]:-NA}"
    [ "${RC[$k]:-1}" -eq 0 ] || allgreen=0
  done
  if [ "$allgreen" -eq 1 ]; then echo "RESULT: ALL GREEN"; else echo "RESULT: FAILURE"; fi
} | tee "$LOG"
echo "log written: $LOG"
