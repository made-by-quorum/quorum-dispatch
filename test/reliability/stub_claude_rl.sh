#!/usr/bin/env bash
# test/reliability/stub_claude_rl.sh — a tiny DETERMINISTIC claude stand-in for
# the Rust reliability harness. SEPARATE from the sha-pinned golden stub
# (test/golden/lib/stub_claude/*) on purpose: the golden stub is byte-frozen for
# the oracle proof-chain and MUST NOT be edited. This stub is owned by the
# reliability harness and may carry harness-specific behaviour (the mutation seam).
#
# Behaviour (faithful to what the Rust sb engine OBSERVES — registry + status):
#   1. Parse --name from argv (sb new appends `--name <zmxname>`; create.rs).
#   2. Render the dev-channels dismiss popup line + block on stdin for ONE CR
#      (the boot dismiss; boot.rs answerer). Then write the PID-keyed registry
#      entry ~/.claude/sessions/<pid>.json with status:"idle" (boot readiness).
#   3. REPL: each submitted non-empty line → set status "busy", (brief hold),
#      then status "idle". This idle->busy->idle transition is the SINGLE signal
#      `sb wait` (status-keyed) and the harness's wait_busy poll key on — the I2
#      "the PTY write LANDED" unfakeable proof.
#
# MUTATION SEAM (STUB_RL_NEVER_BUSY=1, DORMANT by default): when set, a submitted
# line is read-and-DISCARDED and the status NEVER leaves "idle" — the session
# registers and stays alive but never goes busy. This is the negative control
# that mutation_hang.sh injects: the harness MUST then fail at its busy/idle-wait
# assertion (I2 class). Default (unset) => normal idle->busy->idle, byte-faithful.
#
# Bash 3.2 floor. No GNU-isms.
set -u

name=""
while [ $# -gt 0 ]; do
  case "$1" in
    --name) name="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "${SBRL_STUB_NAME:-}" ] && name="$SBRL_STUB_NAME"
[ -n "$name" ] || name="stub-session"

SESS_DIR="$HOME/.claude/sessions"
mkdir -p "$SESS_DIR"
PIDFILE="$SESS_DIR/$$.json"
SID="stubrl-$$"

# Atomic status write (tmp+rename) — mirrors the engine-observed PID file shape.
write_status() {
  local st="$1" tmp="$PIDFILE.tmp"
  printf '{"pid":%d,"name":"%s","status":"%s","sessionId":"%s","cwd":"%s","version":"stub-rl","kind":"interactive","entrypoint":"cli","startedAt":1700000000000,"updatedAt":1700000000000}\n' \
    "$$" "$name" "$st" "$SID" "$PWD" > "$tmp"
  mv -f "$tmp" "$PIDFILE"
}

# Read one submitted line (CR/LF terminated) from stdin; empty string on EOF-only.
read_submit() {
  local line=""
  IFS= read -r line || return 1
  printf '%s' "$line"
  return 0
}

# Phase 1: render the dismiss popup, consume the boot dismiss CR, then register.
printf '\r\nWARNING: Loading development channels\r\nEnter to confirm\r\n'
# Consume exactly one boot dismiss line (the answerer sends a CR). If stdin is
# already closed, proceed to register anyway (the boot path still wrote nothing).
IFS= read -r _dismiss 2>/dev/null || true
write_status "idle"     # boot readiness: PID file appears, status idle
printf 'ready\r\n> '

# Phase 2: REPL. Each submitted non-empty line drives the status transition that
# the harness's busy/idle assertion observes.
while :; do
  if ! line="$(read_submit)"; then
    break          # EOF — client gone
  fi
  # Strip nothing; only a bare-empty submit is a no-op turn.
  case "$line" in
    "" ) continue ;;
  esac

  if [ "${STUB_RL_NEVER_BUSY:-}" = "1" ]; then
    # MUTATION SEAM: read-and-discard, NEVER go busy. The session stays alive and
    # idle-registered, so the harness reaches its busy/idle-wait and fails THERE.
    printf 'discarded (never-busy)\r\n> '
    continue
  fi

  write_status "busy"                 # idle -> busy: acceptance signal
  # Brief hold so a concurrent observer (sb wait / wait_busy poll) can SEE busy.
  sleep "${STUB_RL_BUSY_HOLD_S:-1}"
  printf 'STUB-REPLY: %s\r\n> ' "$line"
  write_status "idle"                 # busy -> idle: completion signal
done

exit 0
