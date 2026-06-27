#!/usr/bin/env bash
# test/reliability/reliability_harness.sh
#
# The cs/qd RELIABILITY HARNESS, PORTED to drive the RUST qd binary.
#
# WHAT THIS PORTS
#   The TS reliability harness `test/reliability_harness.sh` at PIN
#   8c59ec456fe82780fd75d8afb5fe48dc72e10bc8 (switchboard repo, 417 lines). That
#   harness asserts the lifecycle invariants I1–I6 the TS repo named in its
#   diagnosis log (the I-numbers live in the harness's own comments — the
#   doc/log diagnosis file is NOT present at the pin, so the harness comments are
#   the authority, as the A7 plan directs). It drove the TS DEV build
#   (`bun <repo>/src/index.ts`). This port drives the REAL Rust `qd` binary via
#   $QD_BIN and NEVER `bun`.
#
# INVARIANT MAP  (TS step @pin  ->  Rust step  ->  delta)
#   I6  born attachable + registered
#       TS [1]: `cs new` backgrounded; poll registry for a PID; assert PID alive
#               + zmx_has(name).
#       Rust  : `qd new --cwd W <name>` with CLAUDE_BIN=<stub> (boots synchronously,
#               returns 0). Assert: registry <pid>.json exists w/ the name; the
#               claude(=stub) PID is alive; `qd info --json` shows the session; the
#               zmx task is present.
#       delta : Rust `qd new` blocks until boot-ready then returns, so no
#               background poll race; we read the registry pid directly.
#   I2  registry<->zmx resolve regardless of socket dir + a PTY WRITE LANDS
#       TS [2]+[8b]: `cs ls --json` row has zmxName==name & pid==registry pid; a
#               `cs send:pty` write is proven to LAND by the session going busy
#               (wait_busy, TS line 326 — the unfakeable signal).
#       Rust  : `qd info <name> --json` row has "zmxName":"<name>" & "pid":<pid>;
#               then `qd send:pty <name> '<line>'` and PROVE it landed by the
#               session transitioning to status:"busy" (wait_busy poll on the
#               registry). THIS busy/idle-wait is the I2-class assertion the
#               mutation negative-control targets.
#       delta : TS additionally exercised an ARBITRARY-TMPDIR cross-socket-dir
#               split (its steps 7–8). Under the jail every qd/zmx call shares one
#               hermetic ZMX_DIR/TMPDIR (jail.sh, rule 9 + ADD-4), so the cross-dir
#               split is NOT reproducible here and is a NAMED NON-PORT (see
#               "INVARIANTS NOT PORTED" below). The LANDS-proof (the load-bearing
#               I2 signal) IS ported.
#   I3  send round-trip leaves the session alive
#       TS [3]: send returns 0 AND the session is still alive after.
#       Rust  : folded into the I2 step — after the landing send we assert the
#               claude PID is still alive (a send must not kill the session).
#   I4  kill is atomic-or-loud (process dead AND zmx gone AND tombstoned)
#       TS [4]: `cs kill -f` exit 0; claude PID dead; <pid>.json.tombstoned exists;
#               <pid>.json removed; zmx session gone.
#       Rust  : `qd kill --force <name>` exit 0 + byte-exact W4 line `killed
#               <name> (zmx <name>, pid N)` (ADD-15 wart-wave re-mint; --force is
#               a deprecated no-op kept for caller compat); claude PID dead;
#               <pid>.json.tombstoned exists; the zmx task is gone.
#       delta : the live <pid>.json is REPLACED by a <pid>.json.tombstoned (the
#               Rust tombstone IS a rename of the live entry — registry.rs
#               tombstone), so "json removed" and "tombstone present" are the same
#               post-state; we assert the tombstone (which implies the live entry
#               is gone). Output string is the Rust verb's exact line.
#   I1  reconcile crash path: a dead-PID live registry entry is tombstoned
#       TS [5]: `cs new`; kill -9 the claude PID (registry left live); `cs
#               reconcile`; assert the entry is tombstoned + no longer listed.
#       Rust  : `qd new` w/ stub; kill -9 the stub PID out of band (registry left
#               live-and-DEAD); `qd reconcile`; assert <pid>.json.tombstoned
#               exists and `qd ls --json` no longer lists the name.
#   I5  reconcile is idempotent
#       TS [6]: a 2nd `cs reconcile` is a no-op ("Nothing to reconcile").
#       Rust  : a 2nd `qd reconcile` prints `Nothing to reconcile — all sources of
#               truth agree.` (the Rust verb's exact line).
#
# INVARIANTS / RED-TEAM ROWS NOT PORTED (named, not silently dropped):
#   - TS step 7/8 ARBITRARY-TMPDIR cross-socket-dir discovery + cross-dir send:
#       the jail pins ONE hermetic ZMX_DIR/TMPDIR for every call (rule 9 + ADD-4),
#       so the multi-dir split the TS rows reproduce cannot occur inside the jail.
#       The LANDS-via-busy proof those rows hinge on is ported into the I2 step.
#   - TS step 9 SIGKILL'd-claude + LIVE-WRAPPER orphan-wrapper leak (red-team #2):
#       it captures the zmx run wrapper's bash PID and asserts kill reaps the
#       wrapper from the ended task. The Rust kill verb's wrapper-reap is unit- and
#       scenario-covered (a5_lifecycle_live.sh G-L1 dual-reap + G-L3 zmx-survivor
#       advisory); reproducing the exact "live wrapper after claude -9" race here
#       would duplicate that coverage and is OUT OF SCOPE for the I1–I6 port.
#   - TS step 10 `cs ls -a` UNCAPPED-beyond-20: a paging/cap contract, not a
#       lifecycle invariant (I1–I6); covered by unit tests in the Rust ls path.
#
# JAIL COMPOSITION (belt on belt)
#   This harness runs INSIDE the repo jail (test/golden/lib/jail.sh, rule 9 +
#   ADD-4). jail_establish is called FIRST and exports the jailed HOME/QD_HOME/
#   ZMX_DIR/XDG_*/TMPDIR BEFORE any EXIT trap is installed (hard ordering req —
#   A6 set-u incident class). The jail's sbrg- prefix + PID-whitelist + production-
#   path refusal COMPOSE with the TS harness's own safety rules, which are KEPT:
#     - every session this harness creates is sbrl-<run>-<n> (the TS cstest-<run>
#       prefix, RENAMED to nest under the jail's sbrg- prefix as
#       `${JAIL_PREFIX}rl-<n>`, so a name is BOTH sbrg-jail-guarded AND sbrl-tagged);
#     - every created claude/stub PID is recorded and killed by EXACT PID only;
#     - no pkill, no name globs, no pattern kills;
#     - sessions are created ONLY via the Rust binary inside the jail (ADD-10a
#       sanctions jailed engine-under-test sessions; org sessions are NEVER touched).
#   Every destructive kill goes through the jail's two-wall guard
#   (jail_assert_resolves_in_jail) before reaching the engine.
#
# LIVE-MODE CONTRACT
#   Default: deterministic STUB claude (test/reliability/stub_claude_rl.sh) — zero
#   real-Claude boot, zero network (ADD-10a). RELIABILITY_LIVE=1 swaps in the real
#   `claude` binary (the lead runs that leg); the assertions are identical, only the
#   boot/turn timing budgets widen. This script keeps the stub leg by default.
#
# Exits NON-ZERO on any FAIL, with a per-invariant PASS/FAIL summary.
#
# TMPDIR HARD REQUIREMENT (A4 F2 lesson): the jail roots its hermetic state under
# $TMPDIR (jail.sh derives JAIL_ROOT from it). Under macOS's default
# /var/folders/... TMPDIR, the jail-rooted zmx Unix-socket path can exceed the
# 104-byte sun_path cap, and zmx then FAILS SILENTLY WITH EXIT 0 — sessions never
# come up, sentinels are absent, and the harness shows false REDs that look like
# SUT bugs. This harness NORMALIZES TMPDIR to /tmp BEFORE jail_establish (below),
# so the socket path stays short. Callers need not set it; an inherited long
# TMPDIR is overridden. (Set SBRL_KEEP_TMPDIR=1 to opt out — not recommended.)
#
# Bash 3.2 floor (macOS). No GNU timeout (deadline loops, verify.sh pattern).
# Usage:  bash test/reliability/reliability_harness.sh
# Env:    QD_BIN (qd-under-test, default $WT/target/debug/qd), ZMX_BIN,
#         RELIABILITY_LIVE=1 (real claude), CLAUDE_BIN_OVERRIDE (mutation seam:
#         point at a mutated stub — used by mutation_hang.sh).
# ---------------------------------------------------------------------------
set -u

# --- locate worktree + binaries ----------------------------------------------
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="$(cd "$HERE/../.." && pwd)"               # reliability -> test -> repo root
cd "$WT" || { echo "FATAL: cannot cd to worktree root"; exit 1; }

QD_BIN="${QD_BIN:-$WT/target/debug/qd}"
ZMX_BIN="${ZMX_BIN:-$(command -v zmx 2>/dev/null || echo /opt/homebrew/bin/zmx)}"
[ -x "$QD_BIN" ]  || { echo "FATAL: qd binary not found/executable: $QD_BIN"; exit 1; }
[ -x "$ZMX_BIN" ] || { echo "FATAL: zmx binary not found/executable: $ZMX_BIN"; exit 1; }

# --- normalize TMPDIR to /tmp (A4 F2: long /var/folders socket path => zmx exits
# 0 SILENTLY and sessions never come up). Must happen BEFORE jail_establish, which
# roots JAIL_ROOT under $TMPDIR. Opt out only via SBRL_KEEP_TMPDIR=1.
if [ "${SBRL_KEEP_TMPDIR:-}" != "1" ]; then
  export TMPDIR=/tmp
fi

# --- establish the jail FIRST (before any trap; ordering is load-bearing) ----
export JAIL_SB_CMD="$QD_BIN"
export JAIL_ZMX_CMD="$ZMX_BIN"
. test/golden/lib/jail.sh
# Short runid so the prefix `sbrg-XXXX-` (10 chars) leaves room under zmx's
# 20-byte name cap for our `rlN` suffix.
SHORT_RUNID="$(printf '%s' "${RANDOM:-0}${RANDOM:-0}" | tr -cd 'a-z0-9' | cut -c1-4)"
[ -n "$SHORT_RUNID" ] || SHORT_RUNID="z$$"
SHORT_RUNID="$(printf '%s' "$SHORT_RUNID" | cut -c1-4)"
jail_establish "$SHORT_RUNID" || { echo "FATAL: jail_establish failed"; exit 1; }
# Jailed HOME/ZMX_DIR/TMPDIR are now exported. ONLY NOW install the EXIT trap.
trap harness_cleanup EXIT

# TS harness safety rule, KEPT + composed with the jail: our own session tag.
# A created name is `${JAIL_PREFIX}rl-<n>` — sbrg-jail-guarded AND sbrl-tagged.
RL_TAG="rl"

# Real-home invisibility belt (rule 9, house pattern): snapshot the ORG's real
# session count BEFORE; assert it UNCHANGED at the end. Any drift = the harness
# touched production state (the exact invisibility violation the jail prevents).
REAL_BEFORE="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"

# --- TS-harness PID safety (KEPT): exact-PID cleanup, no globs ----------------
CREATED_PIDS=""        # space-separated list of recorded claude/stub PIDs
CREATED_NAMES=""       # space-separated list of recorded session names

record_pid()  { CREATED_PIDS="$CREATED_PIDS $1"; }
record_name() { CREATED_NAMES="$CREATED_NAMES $1"; }

harness_cleanup() {
  echo
  echo "--- cleanup (jail teardown + exact recorded PIDs) ---"
  # Kill recorded stub PIDs by EXACT pid only (TS rule: never pkill/glob).
  local pid
  for pid in $CREATED_PIDS; do
    [ -n "$pid" ] || continue
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null && echo "  killed pid $pid"
      sleep 1
      kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
    fi
  done
  # Jail teardown reaps prefixed zmx sessions in-jail and rms the run dir.
  jail_teardown || true
  echo "--- cleanup done ---"
}

# --- PASS/FAIL accounting, keyed per invariant -------------------------------
PASS=0
FAIL=0
# Per-invariant verdicts (bash 3.2: no assoc arrays — parallel flat strings).
I1_V="-"; I2_V="-"; I3_V="-"; I4_V="-"; I5_V="-"; I6_V="-"

# fail_inv <I#> — mark an invariant FAILED (latching).
fail_inv() {
  case "$1" in
    I1) I1_V="FAIL" ;; I2) I2_V="FAIL" ;; I3) I3_V="FAIL" ;;
    I4) I4_V="FAIL" ;; I5) I5_V="FAIL" ;; I6) I6_V="FAIL" ;;
  esac
}
# pass_inv <I#> — mark an invariant PASSED only if not already failed.
pass_inv() {
  case "$1" in
    I1) [ "$I1_V" != "FAIL" ] && I1_V="PASS" ;;
    I2) [ "$I2_V" != "FAIL" ] && I2_V="PASS" ;;
    I3) [ "$I3_V" != "FAIL" ] && I3_V="PASS" ;;
    I4) [ "$I4_V" != "FAIL" ] && I4_V="PASS" ;;
    I5) [ "$I5_V" != "FAIL" ] && I5_V="PASS" ;;
    I6) [ "$I6_V" != "FAIL" ] && I6_V="PASS" ;;
  esac
}

# assert <I#> <desc> <cmd...> — run cmd; PASS/FAIL; attribute to the invariant.
assert() {
  local inv="$1" desc="$2"; shift 2
  if "$@"; then
    echo "  PASS [$inv]: $desc"
    PASS=$((PASS+1)); pass_inv "$inv"
  else
    echo "  FAIL [$inv]: $desc"
    FAIL=$((FAIL+1)); fail_inv "$inv"
  fi
}

# step <label> — print a step-trace line (the mutation evidence requirement: the
# run output must SHOW which named assertion the harness reached).
step() { echo "[STEP] $*"; }

# --- registry helpers (read the JAILED ~/.claude/sessions) -------------------
SESS_DIR="$HOME/.claude/sessions"

# pid_for_name <name> — the claude(=stub) PID recorded for a live registry entry
# whose name matches. Reads the jailed registry JSON.
pid_for_name() {
  local name="$1" f
  for f in "$SESS_DIR"/*.json; do
    [ -f "$f" ] || continue
    case "$(cat "$f" 2>/dev/null)" in
      *"\"name\":\"$name\""*)
        sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$f"
        return 0 ;;
    esac
  done
  return 1
}

is_alive()         { kill -0 "$1" 2>/dev/null; }
is_dead()          { ! kill -0 "$1" 2>/dev/null; }
tombstone_exists() { [ -e "$SESS_DIR/$1.json.tombstoned" ]; }
zmx_has()          { jail_zmx list 2>/dev/null | grep -q "$1"; }

# wait_for_pid <name> <secs> — poll the registry until a PID appears (boot).
wait_for_pid() {
  local name="$1" secs="$2" deadline p
  deadline=$(( $(date +%s) + secs ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    p="$(pid_for_name "$name")"
    [ -n "$p" ] && { printf '%s' "$p"; return 0; }
    sleep 1
  done
  return 1
}

# wait_busy <name> <secs> — THE I2-CLASS ASSERTION (port of TS wait_busy @line
# 326): poll the registry for status:"busy" within N seconds. PROVES a PTY write
# LANDED in the session. Returns 0 iff busy was observed. The mutation negative-
# control wedges the stub so this can NEVER succeed.
wait_busy() {
  local name="$1" secs="$2" i=0 lim f
  lim=$(( secs * 2 ))
  while [ "$i" -lt "$lim" ]; do
    for f in "$SESS_DIR"/*.json; do
      [ -f "$f" ] || continue
      case "$(cat "$f" 2>/dev/null)" in
        *"\"name\":\"$name\""*"\"status\":\"busy\""*|*"\"status\":\"busy\""*"\"name\":\"$name\""*)
          return 0 ;;
      esac
    done
    sleep 0.5; i=$((i+1))
  done
  return 1
}

# wait_dead <pid> <secs> — wait until a PID is actually reaped (TS wait_dead:
# removes the kill -9 / reconcile race without weakening any assertion).
wait_dead() {
  local pid="$1" secs="$2" i=0 lim
  lim=$(( secs * 2 ))
  while [ "$i" -lt "$lim" ]; do
    is_dead "$pid" && return 0
    sleep 0.5; i=$((i+1))
  done
  return 1
}

# scn_name <suffix> — a jail-prefixed, sbrl-tagged, zmx-safe session name:
# `${JAIL_PREFIX}rl<suffix>` (e.g. sbrg-ab12-rl1). Both jail-guarded AND sbrl-tagged.
scn_name() { printf '%s%s%s' "${JAIL_PREFIX:?jail not established}" "$RL_TAG" "$1"; }

# --- claude binary selection: stub (default) or real (RELIABILITY_LIVE) ------
# The jail's redteam-retro belt #2 REQUIRES CLAUDE_BIN to resolve UNDER JAIL_ROOT
# (an out-of-jail value would be a hermeticity escape). So we STAGE the chosen
# binary inside the jail root (a5_lifecycle_live.sh / a6_routing.sh do the same:
# they write their stub into $JAIL_ROOT). Stub: copy. Real claude: symlink (the
# real binary is large; a symlink under JAIL_ROOT satisfies the belt's
# under-JAIL_ROOT path check while still execing the real binary).
if [ "${RELIABILITY_LIVE:-}" = "1" ]; then
  _CB_SRC="${CLAUDE_BIN_OVERRIDE:-$(command -v claude 2>/dev/null)}"
  [ -n "$_CB_SRC" ] || { echo "FATAL: RELIABILITY_LIVE=1 but no real claude found"; exit 1; }
  CLAUDE_BIN_RESOLVED="$JAIL_ROOT/claude-bin"
  ln -sf "$_CB_SRC" "$CLAUDE_BIN_RESOLVED"
  BOOT_SECS=90; BUSY_SECS=30
  echo "=== RELIABILITY harness: LIVE mode (real claude via $CLAUDE_BIN_RESOLVED -> $_CB_SRC) ==="
else
  _CB_SRC="${CLAUDE_BIN_OVERRIDE:-$HERE/stub_claude_rl.sh}"
  CLAUDE_BIN_RESOLVED="$JAIL_ROOT/claude-bin"
  cp "$_CB_SRC" "$CLAUDE_BIN_RESOLVED"
  chmod +x "$CLAUDE_BIN_RESOLVED"
  BOOT_SECS=30; BUSY_SECS=15
  echo "=== RELIABILITY harness: STUB mode (jailed stub $CLAUDE_BIN_RESOLVED <- $_CB_SRC) ==="
fi
export CLAUDE_BIN="$CLAUDE_BIN_RESOLVED"

WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"
mkdir -p "$SESS_DIR"

echo "=== run jail=$JAIL_PREFIX root=$JAIL_ROOT  qd=$QD_BIN ==="
echo

# rl_new <name> — drive `qd new` in the jail with the resolved claude binary.
# (ADD-10a: a jailed engine-under-test session created via the Rust binary.)
# stdout/err -> $JAIL_ROOT/<name>.{out,err}. Returns qd's exit code.
rl_new() {
  local name="$1"
  ( cd "$WORKDIR" && env SBRL_STUB_NAME="$name" "$QD_BIN" new "$name" --cwd "$WORKDIR" ) \
    > "$JAIL_ROOT/$name.out" 2> "$JAIL_ROOT/$name.err"
}

# ---------------------------------------------------------------------------
# I6 — new: born attachable + registered
# ---------------------------------------------------------------------------
step "I6: qd new (born attachable + registered)"
S1="$(scn_name 1)"
record_name "$S1"
rl_new "$S1"; NEW_RC=$?
echo "    new exit=$NEW_RC"
[ -s "$JAIL_ROOT/$S1.err" ] && sed 's/^/    err: /' "$JAIL_ROOT/$S1.err"
assert I6 "qd new exit 0" test "$NEW_RC" -eq 0
PID1="$(wait_for_pid "$S1" "$BOOT_SECS" || true)"
if [ -z "${PID1:-}" ]; then
  echo "  FAIL [I6]: $S1 never registered a PID"; FAIL=$((FAIL+1)); fail_inv I6
else
  record_pid "$PID1"
  assert I6 "$S1 registered a live claude PID ($PID1)" is_alive "$PID1"
  assert I6 "$S1 attachable: zmx task present" zmx_has "$S1"
  LS1="$(jail_sb ls --json 2>/dev/null)"
  # `qd ls --json` is PRETTY-printed (`"name": "x"` with spaces), so all field
  # greps are whitespace-tolerant (-E with [[:space:]]*). The registry file is
  # compact JSON; only the ls-output greps need this.
  assert I6 "$S1 present in qd ls --json" \
    bash -c "printf '%s' \"\$1\" | grep -Eq '\"name\":[[:space:]]*\"$S1\"'" _ "$LS1"
fi

# ---------------------------------------------------------------------------
# I2 — registry<->zmx resolve + a PTY WRITE LANDS (the keystone; mutation target)
# I3 — send round-trip leaves the session alive (folded in)
# ---------------------------------------------------------------------------
step "I2: qd ls --json row (zmxName + pid) resolve"
if [ -n "${PID1:-}" ]; then
  # ls --json is pretty-printed (one field per line). $S1 is UNIQUE in the array,
  # so the name / zmxName / pid fields all belong to OUR row — we grep the whole
  # blob per field (whitespace-tolerant). zmxName==name is the I2 resolve proof;
  # pid==registry-pid ties the ls row to the registry entry.
  LS1="$(jail_sb ls --json 2>/dev/null)"
  echo "    ls row name/zmxName/pid:"
  printf '%s' "$LS1" | grep -E "\"(name|zmxName|pid)\":" | sed 's/^/      /'
  assert I2 "$S1 ls row has zmxName==name" \
    bash -c "printf '%s' \"\$1\" | grep -Eq '\"zmxName\":[[:space:]]*\"$S1\"'" _ "$LS1"
  assert I2 "$S1 ls row pid == registry pid ($PID1)" \
    bash -c "printf '%s' \"\$1\" | grep -Eq '\"pid\":[[:space:]]*$PID1([^0-9]|\$)'" _ "$LS1"

  step "I2: send:pty LANDS — session must go busy (wait_busy; TS line 326)"
  jail_assert_resolves_in_jail "$S1" \
    && jail_sb send:pty "$S1" 'reliability probe line' >"$JAIL_ROOT/$S1.send" 2>&1 \
    || echo "    (send:pty dispatch returned non-zero or belt refused)"
  # THE I2-CLASS ASSERTION. The mutation negative-control makes this UNREACHABLE-
  # as-PASS: a wedged stub registers + stays alive but never goes busy.
  assert I2 "$S1 send:pty LANDED (session went busy within ${BUSY_SECS}s)" \
    wait_busy "$S1" "$BUSY_SECS"

  step "I3: send leaves the session alive"
  assert I3 "$S1 still alive after send:pty (PID $PID1)" is_alive "$PID1"
fi

# ---------------------------------------------------------------------------
# I4 — kill is atomic-or-loud (process dead AND zmx gone AND tombstoned)
# ---------------------------------------------------------------------------
step "I4: qd kill --force (atomic-or-loud)"
if [ -n "${PID1:-}" ]; then
  if jail_assert_resolves_in_jail "$S1"; then
    KOUT="$(jail_sb kill --force "$S1" 2>"$JAIL_ROOT/$S1.kill")"; KRC=$?
    echo "    kill exit=$KRC out=[$KOUT]"
    [ -s "$JAIL_ROOT/$S1.kill" ] && sed 's/^/    err: /' "$JAIL_ROOT/$S1.kill"
    assert I4 "kill exit 0" test "$KRC" -eq 0
    assert I4 "kill output byte-exact" test "$KOUT" = "killed $S1 (zmx $S1, pid $PID1)"
    wait_dead "$PID1" 10 || true
    assert I4 "$S1 claude PID $PID1 dead" is_dead "$PID1"
    assert I4 "$S1 <pid>.json.tombstoned exists" tombstone_exists "$PID1"
    assert I4 "$S1 zmx task gone" bash -c "! jail_zmx list 2>/dev/null | grep -q '$S1'"
  else
    echo "  FAIL [I4]: resolution belt refused $S1 — cannot kill"; FAIL=$((FAIL+1)); fail_inv I4
  fi
fi

# ---------------------------------------------------------------------------
# I1 — reconcile crash path: a dead-PID live registry entry is tombstoned
# ---------------------------------------------------------------------------
step "I1: reconcile after crash (dead-PID registry entry -> tombstoned)"
S2="$(scn_name 2)"
record_name "$S2"
rl_new "$S2"; echo "    new($S2) exit=$?"
PID2="$(wait_for_pid "$S2" "$BOOT_SECS" || true)"
if [ -z "${PID2:-}" ]; then
  echo "  FAIL [I1]: setup — $S2 never registered"; FAIL=$((FAIL+1)); fail_inv I1
else
  record_pid "$PID2"
  echo "    simulating crash: kill -9 $PID2 (registry left live-and-dead)"
  kill -9 "$PID2" 2>/dev/null || true
  wait_dead "$PID2" 10 || echo "    WARN: $PID2 not dead after 10s"
  jail_sb reconcile >"$JAIL_ROOT/$S2.rec1" 2>&1
  sed 's/^/    rec: /' "$JAIL_ROOT/$S2.rec1"
  assert I1 "$S2 PID $PID2 dead" is_dead "$PID2"
  assert I1 "$S2 registry tombstoned after reconcile" tombstone_exists "$PID2"
  assert I1 "$S2 no longer listed live in qd ls --json" \
    bash -c "! jail_sb ls --json 2>/dev/null | grep -Eq '\"name\":[[:space:]]*\"$S2\"'"
fi

# ---------------------------------------------------------------------------
# I5 — reconcile is idempotent (2nd run is a no-op)
# ---------------------------------------------------------------------------
step "I5: reconcile idempotent (2nd run = 'Nothing to reconcile')"
jail_sb reconcile >"$JAIL_ROOT/rec2" 2>&1
sed 's/^/    rec2: /' "$JAIL_ROOT/rec2"
assert I5 "2nd reconcile reports Nothing to reconcile" \
  grep -q "Nothing to reconcile" "$JAIL_ROOT/rec2"

# ---------------------------------------------------------------------------
# Invisibility belt: the org's real session count must be UNCHANGED (rule 9).
# ---------------------------------------------------------------------------
step "invisibility: org real-home session count unchanged"
REAL_AFTER="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"
if [ "$REAL_BEFORE" = "$REAL_AFTER" ]; then
  echo "  PASS [inv]: org session count unchanged ($REAL_BEFORE)"
  PASS=$((PASS+1))
else
  echo "  FAIL [inv]: org session count CHANGED ($REAL_BEFORE -> $REAL_AFTER) — invisibility breached"
  FAIL=$((FAIL+1))
fi

# ---------------------------------------------------------------------------
# SUMMARY — per-invariant PASS/FAIL (the spec'd named summary)
# ---------------------------------------------------------------------------
echo
echo "=================================================="
echo "  PER-INVARIANT VERDICT"
echo "    I1 (reconcile crash -> tombstone) : $I1_V"
echo "    I2 (resolve + send LANDS busy)    : $I2_V"
echo "    I3 (send leaves session alive)    : $I3_V"
echo "    I4 (kill atomic-or-loud)          : $I4_V"
echo "    I5 (reconcile idempotent)         : $I5_V"
echo "    I6 (new born attachable+reg)      : $I6_V"
echo "  RESULT: $PASS passed, $FAIL failed"
echo "=================================================="
[ "$FAIL" -eq 0 ]
