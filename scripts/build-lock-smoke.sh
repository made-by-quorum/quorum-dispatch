#!/usr/bin/env bash
#
# build-lock-smoke.sh — prove build-lock.sh serializes concurrent invocations.
#
# Strategy: launch two build-lock.sh invocations concurrently against a hermetic
# temp lock dir. Each runs a small command that appends "ENTER <id>" / "EXIT <id>"
# to a shared log with a sleep in between. If the lock works, the log shows fully
# nested-free, non-interleaved sections: one invocation's ENTER..EXIT completes
# before the other's ENTER. Interleaving (ENTER A, ENTER B) means the lock failed.
#
# Hermetic: uses its own SB_RUST_LOCK_DIR under a mktemp dir; never touches
# a real ~/.sb-rust.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOCK_SH="$SCRIPT_DIR/build-lock.sh"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/sb-rust-locksmoke.XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

export SB_RUST_LOCK_DIR="$WORK/lockbase"
export SB_RUST_LOCK_TIMEOUT=60
LOG="$WORK/events.log"
: >"$LOG"

# Critical-section payload, inlined into the command the lock runs (no exported
# functions — keeps us portable to the macOS system bash 3.2). Marks enter, holds
# briefly, marks exit, appending to the shared log.
payload='echo "ENTER $0" >>"$1"; sleep 0.5; echo "EXIT $0" >>"$1"'

# Launch two contenders concurrently.
"$LOCK_SH" bash -c "$payload" A "$LOG" &
p1=$!
"$LOCK_SH" bash -c "$payload" B "$LOG" &
p2=$!

wait "$p1"; r1=$?
wait "$p2"; r2=$?

if [ "$r1" -ne 0 ] || [ "$r2" -ne 0 ]; then
  echo "FAIL: a contender exited non-zero (r1=$r1 r2=$r2)" >&2
  cat "$LOG" >&2
  exit 1
fi

echo "--- event log ---"
cat "$LOG"
echo "-----------------"

# Validate non-interleaving: the sequence of events must be one complete
# ENTER/EXIT pair followed by the other. I.e. lines 1&2 share an id, 3&4 share
# the other id, and the two ids differ.
# Read lines portably (no mapfile — macOS system bash is 3.2).
lines=()
while IFS= read -r _line; do
  lines+=("$_line")
done <"$LOG"
if [ "${#lines[@]}" -ne 4 ]; then
  echo "FAIL: expected 4 events, got ${#lines[@]}" >&2
  exit 1
fi

first_id="${lines[0]##* }"
second_id="${lines[2]##* }"

if [ "${lines[0]}" = "ENTER $first_id" ] \
   && [ "${lines[1]}" = "EXIT $first_id" ] \
   && [ "${lines[2]}" = "ENTER $second_id" ] \
   && [ "${lines[3]}" = "EXIT $second_id" ] \
   && [ "$first_id" != "$second_id" ]; then
  echo "PASS: invocations serialized (no interleave): $first_id then $second_id"
  exit 0
else
  echo "FAIL: events interleaved -> lock did not serialize" >&2
  exit 1
fi
