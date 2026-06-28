#!/usr/bin/env bash
# test/reliability/mutation_hang.sh
#
# THE TEETH PROOF for reliability_harness.sh — a mutation / negative-control that
# injects a KNOWN HANG and proves the harness CATCHES it at the right assertion.
#
# THE MUTATION (red-team-hardened): a wedged claude stub that REGISTERS a session
# (so I6 boot-readiness PASSES and the harness proceeds) and STAYS ALIVE (so the
# harness does not mistake a crash for the injected fault), but NEVER goes busy on
# a submitted line. The wedge is the stub's STUB_RL_NEVER_BUSY=1 seam: it reads-
# and-discards each submitted line and the registry status never leaves "idle".
#
# Therefore the harness MUST fail at its I2-class busy/idle-wait assertion (the
# port of the TS `wait_busy`, TS line 326): "send:pty LANDED (session went busy)".
# The wedge survives PAST the earlier I6 boot assertions and the I2 resolve
# assertions (the session registers and is discoverable) and dies exactly AT the
# busy-wait — which is the design requirement (an earlier-step failure would be a
# BUG in the mutation, not a teeth proof).
#
# This script asserts:
#   (a) the MUTATED run exits NON-ZERO;
#   (b) the failure NAMES the busy/idle invariant (the I2 wait_busy assertion) AND
#       the step trace shows the harness REACHED that named assertion (evidence:
#       not merely a non-zero exit);
#   (c) a CLEAN CONTROL run (same harness, UNMUTATED stub) PASSES.
#
# Runs jailed (the harness it invokes establishes its own jail). ADD-10a: every
# session is a jailed engine-under-test session via the Rust binary. Bash 3.2.
#
# Usage:  bash test/reliability/mutation_hang.sh
# Env:    QD_BIN, ZMX_BIN (passed through to the harness).
# ---------------------------------------------------------------------------
set -u

# Normalize TMPDIR to /tmp (A4 F2 socket-length lesson) — the harness this script
# invokes self-normalizes too, but keep our own mktemp paths short + consistent.
[ "${QDRL_KEEP_TMPDIR:-}" = "1" ] || export TMPDIR=/tmp

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="$(cd "$HERE/../.." && pwd)"
HARNESS="$HERE/reliability_harness.sh"
STUB="$HERE/stub_claude_rl.sh"
[ -x "$STUB" ] || chmod +x "$STUB" 2>/dev/null || true

PASS=0; FAIL=0
ok()   { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

# The mutated stub: a tiny wrapper that forces STUB_RL_NEVER_BUSY=1 on the real
# stub. Written under a temp dir we own (NOT under test/golden, NOT the pinned
# golden stub). It must be a single executable the harness can use as CLAUDE_BIN.
MUT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/qdrl-mut.XXXXXX")" || { echo "FATAL: mktemp"; exit 1; }
trap 'rm -rf "$MUT_DIR"' EXIT
MUT_STUB="$MUT_DIR/stub_mutated.sh"
cat > "$MUT_STUB" <<EOS
#!/usr/bin/env bash
# MUTATED stub: forces the never-busy wedge on the reliability stub.
export STUB_RL_NEVER_BUSY=1
exec "$STUB" "\$@"
EOS
chmod +x "$MUT_STUB"

# ---------------------------------------------------------------------------
# (a)+(b): the MUTATED run must fail AT the busy/idle (I2 wait_busy) assertion.
# ---------------------------------------------------------------------------
echo "=== mutation_hang: MUTATED run (never-busy wedge) ==="
MUT_LOG="$MUT_DIR/mutated.log"
CLAUDE_BIN_OVERRIDE="$MUT_STUB" QD_BIN="${QD_BIN:-$WT/target/debug/qd}" \
  ZMX_BIN="${ZMX_BIN:-}" bash "$HARNESS" >"$MUT_LOG" 2>&1
MUT_RC=$?
echo "--- mutated harness output (tail) ---"
tail -n 40 "$MUT_LOG" | sed 's/^/  | /'
echo "--- (mutated exit=$MUT_RC) ---"

# (a) non-zero exit.
if [ "$MUT_RC" -ne 0 ]; then
  ok "(a) mutated run exited non-zero ($MUT_RC)"
else
  bad "(a) mutated run exited 0 — the harness DID NOT catch the wedge"
fi

# (b1) the failure NAMES the busy/idle invariant: the I2 wait_busy FAIL line.
if grep -q 'FAIL \[I2\]: .*send:pty LANDED (session went busy' "$MUT_LOG"; then
  ok "(b) failure names the busy/idle invariant (I2 wait_busy FAIL present)"
else
  bad "(b) no I2 busy-wait FAIL line found — mutation hit the wrong assertion"
fi

# (b2) EVIDENCE the harness REACHED that assertion (step trace), not an early exit.
if grep -q '\[STEP\] I2: send:pty LANDS' "$MUT_LOG"; then
  ok "(b) step trace shows the harness REACHED the busy/idle-wait assertion"
else
  bad "(b) step trace missing — harness did not reach the busy-wait (early-fail BUG)"
fi

# (b3) GUARD against an EARLIER-step failure masquerading: I6 boot + the I2 resolve
# rows must have PASSED (the wedge survives to the busy-wait by design).
if grep -q 'PASS \[I6\]: .*registered a live claude PID' "$MUT_LOG" \
   && grep -q 'PASS \[I2\]: .*ls row has zmxName==name' "$MUT_LOG"; then
  ok "(b) wedge survived earlier steps (I6 boot + I2 resolve PASSED) — failed only at busy-wait"
else
  bad "(b) the mutation failed EARLIER than the busy-wait — fix the injection (it must register + resolve)"
fi

# ---------------------------------------------------------------------------
# (c): the CLEAN CONTROL run (unmutated stub) must PASS.
# ---------------------------------------------------------------------------
echo
echo "=== mutation_hang: CONTROL run (unmutated stub) ==="
CTL_LOG="$MUT_DIR/control.log"
QD_BIN="${QD_BIN:-$WT/target/debug/qd}" ZMX_BIN="${ZMX_BIN:-}" \
  bash "$HARNESS" >"$CTL_LOG" 2>&1
CTL_RC=$?
echo "--- control harness output (tail) ---"
tail -n 16 "$CTL_LOG" | sed 's/^/  | /'
echo "--- (control exit=$CTL_RC) ---"

if [ "$CTL_RC" -eq 0 ]; then
  ok "(c) clean control run PASSED (exit 0)"
else
  bad "(c) clean control run FAILED (exit=$CTL_RC) — the harness is broken, not just toothed"
fi
# Belt: the control must specifically PASS the busy-wait the mutation broke.
if grep -q 'PASS \[I2\]: .*send:pty LANDED (session went busy' "$CTL_LOG"; then
  ok "(c) control PASSED the busy/idle-wait the mutation broke (differential proof)"
else
  bad "(c) control did not pass the busy-wait — differential is not clean"
fi

echo
echo "=================================================="
echo "  mutation_hang RESULT: $PASS passed, $FAIL failed"
echo "=================================================="
[ "$FAIL" -eq 0 ]
