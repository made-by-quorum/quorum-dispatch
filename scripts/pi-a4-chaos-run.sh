#!/usr/bin/env bash
#
# pi-a4-chaos-run.sh — layer (c) of the A4 chaos containment (A4-ISOLATION-PLAN.md).
#
# ⚠ PHASE 2 ONLY. This launches the LIVE pi C-CHAOS round, which spawns real pi
# residents and issues real group-SIGKILLs. It must run ONLY after the chaos
# coordinator relays the explicit chaos-run GATE. Phase-1 PREP does NOT invoke this
# (it only dry-checks that systemd-run is available); see the A4 exec charge.
#
# WHAT LAYER (c) BUYS: the chaos coordinator + its executors run in cgroup
# system.slice/fleet.service alongside the durable supervisor + live coordinators.
# safe_group_kill(pgid) = kill(-pgid) is GLOBAL by pgid, not cgroup-scoped. Running
# the chaos TEST-RUN in a SEPARATE transient scope OUTSIDE fleet.service means the
# test process AND every resident it spawns (residents inherit the launcher's cgroup —
# process_group(0) is a new pgid, NOT a new cgroup) live in a non-fleet cgroup. So an
# accidental executor-tree kill physically cannot touch a fleet sibling. TIGHTENING
# #2: chaos.rs::capture_proc_cgroup writes each resident's /proc/<pid>/cgroup — the
# artifact proving the scope reached the RESIDENTS, not just the launcher.
#
# THE BUILD STAYS IN THE NORMAL SERIALIZED SLOT (build-lock.sh, coordinated with the
# build coordinator); only the TEST-RUN goes in the transient scope.
#
# USAGE: pi-a4-chaos-run.sh <round-N> [evidence-root]
#   e.g. pi-a4-chaos-run.sh 1 "$PWD/target/cred-evidence/cchaos/round1"
#
# ENV (override as needed):
#   QD_PI_BIN   pinned pi binary (default ~/.npm-pi-global/bin/pi)
#   XDG_RUNTIME_DIR_SCOPE  short non-fleet runtime dir for the qrmux sun_path budget
#                          (default /tmp/xrd-a4)
set -euo pipefail

ROUND="${1:?usage: pi-a4-chaos-run.sh <round-N> [evidence-root]}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DISPATCH="$(cd "$HERE/.." && pwd)"           # the dispatch/ tree (this script lives in dispatch/scripts/)
# The cargo WORKSPACE ROOT is not at a fixed depth: it is `dispatch/` itself when
# dispatch/ is the repo root, and dispatch/'s parent in the monorepo. Probe upward
# for the `[workspace]` marker rather than hopping a hard-coded `..`.
REPO="$DISPATCH"
while [ ! -f "$REPO/Cargo.toml" ] || ! grep -q '^\[workspace\]' "$REPO/Cargo.toml"; do
  parent="$(dirname "$REPO")"
  if [ "$parent" = "$REPO" ]; then
    echo "FATAL: no Cargo.toml with a [workspace] table at or above $DISPATCH" >&2
    exit 2
  fi
  REPO="$parent"
done
CRATE_DIR="$DISPATCH/crates/dispatch"
EVIDENCE_ROOT="${2:-$DISPATCH/target/cred-evidence/cchaos/round${ROUND}}"
UNIT="pi-a4-chaos-r${ROUND}"
QD_PI_BIN="${QD_PI_BIN:-$HOME/.npm-pi-global/bin/pi}"
XRD="${XDG_RUNTIME_DIR_SCOPE:-/tmp/xrd-a4}"

# systemd-run --user needs the REAL user runtime dir to reach the user bus. On this
# box the ambient XDG_RUNTIME_DIR is /run/fleet (no user bus there), so systemd-run
# fails "Failed to connect to bus" unless we point it at /run/user/<uid>. The INNER
# test still gets the short $XRD (qrmux 104-byte sun_path budget) via `run_env`, so
# these two runtime dirs do not conflict: one is for the launcher's bus, one for the
# test's sockets.
USER_XRD="/run/user/$(id -u)"
systemd_env=(
  XDG_RUNTIME_DIR="$USER_XRD"
  DBUS_SESSION_BUS_ADDRESS="unix:path=$USER_XRD/bus"
)

mkdir -p "$EVIDENCE_ROOT" "$XRD"
chmod 700 "$XRD" || true

# The 5-var session scrub (the preregistered hermetic scrub) + the live chaos env.
run_env=(
  env
  -u QD_HOME -u QD_SESSION_ID -u SB_SESSION_ID -u QD_BOOT_AWAIT_RELAY -u CLAUDE_CODE_SESSION_ID
  QD_PI_LIVE=1
  QD_PI_BIN="$QD_PI_BIN"
  QD_CHAOS_ROUND="$ROUND"
  QD_CHAOS_EVIDENCE_DIR="$EVIDENCE_ROOT"
  XDG_RUNTIME_DIR="$XRD"
)

echo "== A4 layer (c): build deps in the serialized slot (NOT in the scope) =="
# BUILD in the normal serialized build slot; do NOT rebuild inside the scope. qrmux is
# a `qd` subcommand (qd qrmux-server), not a separate bin.
"$HERE/build-lock.sh" cargo build -p quorum-dispatch --bin qd
"$HERE/build-lock.sh" cargo test -p quorum-dispatch --features faultinj --test pi_chaos --no-run

# Resolve the freshly-built test binary and run IT directly under the scope — NOT
# `cargo test` (which re-links inside the scope and is slow). The binary has qd's path
# compiled in via CARGO_BIN_EXE_qd, so it needs no cargo at run time. `|| true` guards
# the head-closes-pipe SIGPIPE against `set -o pipefail`.
TARGET_DEPS="$REPO/target/debug/deps"
TEST_BIN="$(ls -t "$TARGET_DEPS"/pi_chaos-* 2>/dev/null | grep -v '\.d$' | head -1 || true)"
if [ -z "$TEST_BIN" ] || [ ! -x "$TEST_BIN" ]; then
  echo "FATAL: could not resolve the built pi_chaos test binary under target/debug/deps/" >&2
  exit 70
fi
echo "test binary: $TEST_BIN"
test_cmd=("$TEST_BIN" --ignored --exact pi_cchaos_round --nocapture)

echo "== A4 layer (c): run the LIVE chaos round in a transient NON-fleet scope =="
cd "$CRATE_DIR"
if env "${systemd_env[@]}" systemd-run --user --scope --unit="${UNIT}-probe" true >/dev/null 2>&1; then
  echo "systemd-run --user --scope available; running under unit=$UNIT (outside fleet.service)"
  {
    echo "unit=$UNIT"
    echo "mechanism=systemd-run --user --scope (transient cgroup outside system.slice/fleet.service)"
    echo "launcher XDG_RUNTIME_DIR=$USER_XRD (user bus); test XDG_RUNTIME_DIR=$XRD (qrmux sun_path budget)"
  } > "$EVIDENCE_ROOT/layer-c-scope.txt"
  env "${systemd_env[@]}" systemd-run --user --scope --unit="$UNIT" \
    "${run_env[@]}" "${test_cmd[@]}"
else
  echo "WARN: systemd-run --user --scope unavailable — FALLBACK: non-fleet XDG_RUNTIME_DIR + setsid-detached run" >&2
  echo "mechanism=FALLBACK setsid + non-fleet XDG_RUNTIME_DIR=$XRD (systemd-run --scope unavailable)" > "$EVIDENCE_ROOT/layer-c-scope.txt"
  setsid "${run_env[@]}" "${test_cmd[@]}"
fi

echo "== A4 layer (c): done. Evidence: $EVIDENCE_ROOT =="
echo "   fleet before/after snapshot: $EVIDENCE_ROOT/fleet-snapshot.txt"
echo "   resident cgroup proofs:      $EVIDENCE_ROOT/**/‹class›-*-proc-cgroup.txt"
echo "   round report:                $EVIDENCE_ROOT/chaos-report.json"
