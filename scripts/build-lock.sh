#!/usr/bin/env bash
#
# build-lock.sh — serialize all cargo build/test (and, later, golden-recording /
# live-mux) invocations through a single host-wide mutex.
#
# WHY A LOCK AT ALL: overnight runs may launch multiple build/test invocations
# concurrently; cargo's target dir is not safe to share across concurrent builds,
# and live/golden phases must not race. This script forces them to take turns.
#
# WHY STALE-LOCK RECOVERY (comments-carry rationale — do not delete): a dead lock
# holder must NOT wedge an overnight run. If the process that took the lock has
# died (crash, kill, OOM) without releasing, the next caller detects the dead PID
# and reclaims the lock instead of blocking forever. Without this, a single crash
# would stall every subsequent build until a human intervened.
#
# WHY mkdir, NOT flock: this must run identically on macOS arm64 (CI + dev host)
# and Linux x86_64 (CI). `flock(1)` does not exist on macOS. `mkdir` is atomic on
# every POSIX filesystem, so an atomic-create-directory mutex is portable and makes
# the dead-holder check trivial. See doc/adr/0001-build-lock-mkdir.md.
#
# USAGE: scripts/build-lock.sh <command> [args...]
#   e.g. scripts/build-lock.sh cargo test --workspace
#
# ENV:
#   QD_RUST_LOCK_DIR  Base dir for the lock (default: $HOME/.quorum/dispatch-rust). Override for
#                     hermetic tests so we never touch a real ~/.quorum/dispatch-rust.
#   QD_RUST_LOCK_TIMEOUT  Max seconds to wait for the lock (default: 300).
#   QD_RUST_LOCK_POLL     Poll interval seconds while waiting (default: 0.2).
#
set -euo pipefail

LOCK_BASE="${QD_RUST_LOCK_DIR:-$HOME/.quorum/dispatch-rust}"
LOCK_DIR="$LOCK_BASE/build.lock"
META_FILE="$LOCK_DIR/owner"
TIMEOUT="${QD_RUST_LOCK_TIMEOUT:-300}"
POLL="${QD_RUST_LOCK_POLL:-0.2}"

if [ "$#" -lt 1 ]; then
  echo "build-lock.sh: usage: build-lock.sh <command> [args...]" >&2
  exit 64
fi

mkdir -p "$LOCK_BASE"

# LEGACY GC: aside-dirs from the short-lived mv-aside reclaim revision (v1 of the
# flake-pass fix) that may persist on hosts that ran it. The marker-CAS reclaim
# below creates NO aside dirs, so this loop only ever sees historical strays.
# DEAD-owner only; nested asides (v1's macOS mv-into-existing-dir edge, orc-13
# M-1/m-3) live INSIDE a lock instance and are removed with it by owner cleanup.
for d in "$LOCK_DIR".stale.*; do
  [ -d "$d" ] || continue
  gpid="$(sed -n 's/^pid=//p' "$d/owner" 2>/dev/null || true)"
  if [ -n "$gpid" ] && ! kill -0 "$gpid" 2>/dev/null; then
    rm -rf "$d"
  fi
done

# Whether *this* invocation currently owns the lock; controls cleanup.
OWNED=0

cleanup() {
  if [ "$OWNED" = "1" ]; then
    # BELT (flake-rootcause pass): only remove the lock if it is still OURS.
    # A blind rm -rf here on a no-longer-ours name would cascade theft to the
    # NEXT holder. Verify the owner pid is ours before removing.
    if [ "$(holder_pid)" = "$$" ]; then
      rm -rf "$LOCK_DIR"
    fi
  fi
}
trap cleanup EXIT INT TERM

write_meta() {
  {
    echo "owner=${USER:-unknown}@$(hostname 2>/dev/null || echo unknownhost)"
    echo "pid=$$"
    echo "timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >"$META_FILE"
}

# Read the holder PID from metadata; empty if unreadable.
holder_pid() {
  [ -f "$META_FILE" ] || return 0
  sed -n 's/^pid=//p' "$META_FILE" 2>/dev/null || true
}

# Returns success epoch-portably.
now() { date +%s; }

START="$(now)"
while true; do
  if mkdir "$LOCK_DIR" 2>/dev/null; then
    OWNED=1
    write_meta
    break
  fi

  # Could not create — someone holds it (or it's stale). Check the holder.
  hpid="$(holder_pid)"
  if [ -n "$hpid" ] && ! kill -0 "$hpid" 2>/dev/null; then
    # Holder PID is dead -> stale lock. Reclaim via an INSTANCE-PINNED marker
    # (flake-rootcause pass v2; orc-13 M-1).
    #
    # WHY NOT `rm -rf $LOCK_DIR; continue` (the original code): two waiters both
    # pass the dead-pid check; the late one's rm deletes a successor's LIVE lock
    # → two concurrent holders (storm test reds 3/3 against it).
    #
    # WHY NOT mv-aside (fix v1): mv is name-addressed, not instance-addressed.
    # A waiter whose dead-verdict is one beat stale movs WHATEVER instance now
    # bears the name — including a successor's live lock — out from under it;
    # the putback then loses races (macOS mv into an existing fresh lock NESTS
    # the victim's instance inside it). Storm test red 10/15 on this host, 2/2
    # on orc-13's (M-1 forensics: one reclaim line, two violations).
    #
    # MARKER CAS: `mkdir $LOCK_DIR/reclaim` atomically claims THE INSTANCE we
    # are examining (a successor instance is a different dir; our marker dies
    # with the instance it was made in). Exactly one claimant per instance.
    # After winning, write our pid into reclaim/by, then re-verify ON THE
    # PINNED INSTANCE: (1) the owner pid — an instance's owner file never
    # changes identity after write_meta, so dead-after-claim ⇒ this really is
    # a dead man's lock, frozen (owner can't clean it, other claimants can't
    # win its marker); (2) reclaim/by still reads OUR pid — proves the marker
    # at the name is the one WE made (not a successor's), closing the
    # marker-outlived-its-instance edge. Only then rm -rf. Any other reading
    # (owner alive, meta unwritten, by≠ours) ⇒ stale verdict on our side:
    # remove the marker ONLY if it is ours, and back off to polling.
    #
    # FAIL-CLOSED CORNER (accepted, loud): a claimant killed inside the
    # marker window (μs: between mkdir and its resolution) leaves a marker
    # whose by-pid is dead. Waiters clear dead-by markers below; a marker
    # with NO by yet gets the same dead-claimant clearing only after the by
    # write window passes (it never does for a dead claimant) — cleared via
    # the dead-by path once by exists, else it wedges reclaim until TIMEOUT
    # (exit 75, loud) rather than ever failing open. The live-owner cleanup
    # rm -rf also clears foreign markers left inside its instance.
    if mkdir "$LOCK_DIR/reclaim" 2>/dev/null; then
      echo "$$" >"$LOCK_DIR/reclaim/by" 2>/dev/null || true
      spid="$(holder_pid)"
      claimed_by="$(cat "$LOCK_DIR/reclaim/by" 2>/dev/null || true)"
      if [ -n "$spid" ] && ! kill -0 "$spid" 2>/dev/null && [ "$claimed_by" = "$$" ]; then
        echo "build-lock.sh: stale lock (dead holder pid=$spid); reclaimed" >&2
        rm -rf "$LOCK_DIR"
      elif [ "$claimed_by" = "$$" ]; then
        # Our claim, but the instance is live/unverifiable — undo OUR marker only.
        rm -rf "$LOCK_DIR/reclaim" 2>/dev/null || true
      fi
    else
      # Marker already present. If ITS claimant is dead (killed mid-window),
      # clear it so reclaim can't wedge. Racing a fresh claimant here is safe-
      # direction: the worst outcome is deleting their by-file, which makes
      # THEIR by-check fail and THEM back off — never a second holder.
      bpid="$(cat "$LOCK_DIR/reclaim/by" 2>/dev/null || true)"
      if [ -n "$bpid" ] && ! kill -0 "$bpid" 2>/dev/null; then
        rm -rf "$LOCK_DIR/reclaim" 2>/dev/null || true
      fi
    fi
    continue
  fi

  # Live holder (or metadata not yet written) -> wait, bounded by timeout.
  elapsed=$(( $(now) - START ))
  if [ "$elapsed" -ge "$TIMEOUT" ]; then
    echo "build-lock.sh: timed out after ${TIMEOUT}s waiting for $LOCK_DIR (held by pid=${hpid:-?})" >&2
    exit 75  # EX_TEMPFAIL
  fi
  sleep "$POLL"
done

# We own the lock. Run the command; preserve its exit code.
set +e
"$@"
rc=$?
set -e
exit "$rc"
