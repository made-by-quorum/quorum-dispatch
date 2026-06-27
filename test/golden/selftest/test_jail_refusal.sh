#!/usr/bin/env bash
# test/golden/selftest/test_jail_refusal.sh — prove the jail FAILS CLOSED.
#
# The single most important safety property of the phase: the harness must REFUSE
# to run against production paths and must refuse kill/gc on bare names. These
# tests point the jail at production-looking state and assert the refusal fires
# (non-zero + a refusal message). If any of these PASS silently, the harness
# could be visible to the org's real qd on brano — a phase failure.
#
# Bash 3.2 / POSIX floor. Run directly.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/../lib/jail.sh"

PASS=0
FAIL=0

# refuses <name> <cmd...> : assert cmd returns non-zero (refusal).
refuses() {
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then
        FAIL=$((FAIL + 1))
        printf 'FAIL %s — expected REFUSAL but command SUCCEEDED\n' "$name"
    else
        PASS=$((PASS + 1))
        printf 'ok   %s (refused)\n' "$name"
    fi
}

# allows <name> <cmd...> : assert cmd returns zero.
allows() {
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        printf 'ok   %s (allowed)\n' "$name"
    else
        FAIL=$((FAIL + 1))
        printf 'FAIL %s — expected SUCCESS but command was refused\n' "$name"
    fi
}

# --- 1. Unestablished jail: assert/guard/kill all refuse -------------------
unset JAIL_ROOT JAIL_RUNID JAIL_PREFIX JAIL_RELAY_PORT
refuses "unestablished/assert" jail_assert_established
refuses "unestablished/guard-name" jail_guard_name "sbrg-x-foo"
refuses "unestablished/jail_sb" jail_sb ls

# --- 2. Production-path corruption: assert_established must refuse ----------
# Capture the REAL home BEFORE jail_establish overrides HOME. The belt checks
# against this constant; corrupting a var to a real-home path must be refused.
_REAL_HOME="$HOME"
jail_establish >/dev/null 2>&1
allows "established/assert-clean" jail_assert_established
# Corrupt SB_HOME to the REAL production registry path.
_saved_sbhome="$SB_HOME"
export SB_HOME="$_REAL_HOME/.quorum/dispatch"
refuses "corrupt/sbhome-prod-path" jail_assert_established
export SB_HOME="$_saved_sbhome"
# Corrupt HOME itself back to the real home — the most dangerous case, since TS
# qd keys its registry on HOME. Must be refused.
_saved_home="$HOME"
export HOME="$_REAL_HOME"
refuses "corrupt/home-real-home" jail_assert_established
export HOME="$_saved_home"
# Corrupt ZMX_DIR to the canonical /tmp/zmx-<uid> production dir.
_saved_zmx="$ZMX_DIR"
export ZMX_DIR="/tmp/zmx-501"
refuses "corrupt/zmxdir-prod-path" jail_assert_established
export ZMX_DIR="$_saved_zmx"
# Re-assert clean to confirm restoration.
allows "established/assert-restored" jail_assert_established

# --- 2b. Binary-read env vars: jail must CLEAR inherited leaks (finding #2) -
# The Rust binary reads SB_PLUGINS_ROOT / SB_SPAWN_AGENTS_DIR / CLAUDE_BIN /
# SB_CLAUDE_FLAGS. Before this fix the jail neither set nor cleared them, so a
# value inherited from the real shell reached qd inside the jail and escaped
# isolation (the --agent escape via SB_SPAWN_AGENTS_DIR; the binary-substitution
# via CLAUDE_BIN). Two-part proof per var:
#   (i)  pre-export a real-home-looking value BEFORE jail_establish, then assert
#        jail_establish UNSET it (so the clean belt passes — the leak is gone).
#   (ii) after establish, corrupt the var to a leaked value and assert the belt
#        REFUSES (fail-closed).
jail_teardown >/dev/null 2>&1

# (i) Pre-export the --agent escape vector + the binary-substitution vector to
# real-home-looking paths, plus the flags-string leak, then establish.
export SB_PLUGINS_ROOT="$_REAL_HOME/.quorum/dispatch/plugins"
export SB_SPAWN_AGENTS_DIR="$_REAL_HOME/.quorum/dispatch/plugins/core/agents"
export CLAUDE_BIN="$_REAL_HOME/.local/bin/claude"
export SB_CLAUDE_FLAGS="--dangerously-skip-permissions"
jail_establish >/dev/null 2>&1
allows "established/clears-inherited-envvars" jail_assert_established
# The clean belt passing above already implies the vars were cleared; assert the
# unset directly too so the row names the property.
refuses "cleared/qd-plugins-root-unset"   sh -c '[ -n "${SB_PLUGINS_ROOT:-}" ]'
refuses "cleared/qd-spawn-agents-dir-unset" sh -c '[ -n "${SB_SPAWN_AGENTS_DIR:-}" ]'
refuses "cleared/claude-bin-unset"        sh -c '[ -n "${CLAUDE_BIN:-}" ]'
refuses "cleared/qd-claude-flags-unset"   sh -c '[ -n "${SB_CLAUDE_FLAGS:-}" ]'

# (ii) Belt refuses each leaked value (fail-closed). The --agent escape vector:
export SB_SPAWN_AGENTS_DIR="$_REAL_HOME/.quorum/dispatch/plugins/core/agents"
refuses "leak/qd-spawn-agents-dir-real-home" jail_assert_established
unset SB_SPAWN_AGENTS_DIR
# The binary-substitution vector:
export CLAUDE_BIN="$_REAL_HOME/.local/bin/claude"
refuses "leak/claude-bin-real-home" jail_assert_established
unset CLAUDE_BIN
# The (NOT-Rust-read but defense-in-depth) plugins root:
export SB_PLUGINS_ROOT="$_REAL_HOME/.quorum/dispatch/plugins"
refuses "leak/qd-plugins-root-real-home" jail_assert_established
unset SB_PLUGINS_ROOT
# The flags-string leak (path rule can't apply — must be unset):
export SB_CLAUDE_FLAGS="--dangerously-skip-permissions"
refuses "leak/qd-claude-flags-set" jail_assert_established
unset SB_CLAUDE_FLAGS
# A jail-ROOTED override IS allowed (the live A2 captures' contract): re-export
# CLAUDE_BIN + SB_SPAWN_AGENTS_DIR under JAIL_ROOT and assert the belt passes.
export CLAUDE_BIN="$JAIL_ROOT/fake-claude"
export SB_SPAWN_AGENTS_DIR="$JAIL_ROOT/agents"
allows "rooted/jail-rooted-override-ok" jail_assert_established
unset CLAUDE_BIN SB_SPAWN_AGENTS_DIR
jail_teardown >/dev/null 2>&1

# --- 3. Name-prefix kill guard: refuse bare/foreign names ------------------
# Re-establish a clean jail for the remaining sections (2b tore down).
jail_establish >/dev/null 2>&1
refuses "guard/bare-name" jail_guard_name "work"
refuses "guard/foreign-prefix" jail_guard_name "sbqa-thing"
refuses "guard/almost-prefix" jail_guard_name "sbrg-OTHERRUN-x"
allows  "guard/correct-prefix" jail_guard_name "${JAIL_PREFIX}sess"

# --- 4. PID whitelist: raw-kill refuses unregistered PIDs ------------------
refuses "pid/unregistered-raw-kill" jail_raw_kill 999999
# Register a fake PID under a correct name, then raw_kill should be ALLOWED to
# proceed (kill of a nonexistent pid is a no-op that returns 0 by design).
jail_register_pid 999998 "${JAIL_PREFIX}sess" >/dev/null 2>&1
allows "pid/registered-raw-kill" jail_raw_kill 999998
# Registering under a bad name must be refused.
refuses "pid/register-bad-name" jail_register_pid 12345 "bare-name"

# --- 5. Lima destructive gate: must fail closed on brano -------------------
# On brano (the production machine) this must ALWAYS refuse, regardless of env.
SB_RUST_DESTRUCTIVE_OK=1 refuses "lima/brano-fail-closed" jail_require_destructive_ok

# --- 6. kill_session refuses a bare name even when jail established ---------
refuses "killsession/bare-name" jail_kill_session "work"

# --- 7. TARGET-RESOLUTION BELT (A4 finding): second wall behind the prefix --
# The belt asserts a name resolves ONLY under JAIL_ROOT before any kill/send/wait,
# fail-closed on MISS / OUT-OF-JAIL / AMBIGUITY. We exercise it three ways using
# the resolver-override seam (_JAIL_TARGET_RESOLVER) to FORGE resolution results
# (no real colliding org session needed) PLUS a real in-jail socket file.
NAME="${JAIL_PREFIX}sess"

# (a) OUT-OF-JAIL resolution -> REFUSED. Forge a host path under the REAL home
#     (the exact A4 risk: the legacy /tmp tier resolving a real org socket). The
#     resolver is a shell function, so we set the override in-process (an env-only
#     export cannot carry a function definition into the belt's subshell call).
_forge_out_of_jail() { printf '%s\n' "$_REAL_HOME/.quorum/dispatch/sessions/$1/zmx.sock"; }
_JAIL_TARGET_RESOLVER=_forge_out_of_jail
refuses "belt/out-of-jail-resolution" jail_assert_target_resolves_in_jail "$NAME"

# (b) MISS (unresolvable name) -> REFUSED. Forge an EMPTY resolution.
_forge_empty() { return 0; }   # emits nothing
_JAIL_TARGET_RESOLVER=_forge_empty
refuses "belt/miss-unresolvable" jail_assert_target_resolves_in_jail "$NAME"

# (c) AMBIGUITY (in-jail AND out-of-jail candidates) -> REFUSED.
_forge_ambiguous() {
    printf '%s\n' "$JAIL_ROOT/zmx/$1.sock"          # in-jail
    printf '%s\n' "/tmp/zmx-501/$1.sock"            # out-of-jail (legacy tier)
}
_JAIL_TARGET_RESOLVER=_forge_ambiguous
refuses "belt/ambiguous-in-and-out" jail_assert_target_resolves_in_jail "$NAME"

# (d) LEGIT in-jail resolution -> ALLOWED. Forge an in-jail path only.
_forge_in_jail() { printf '%s\n' "$JAIL_ROOT/zmx/$1.sock"; }
_JAIL_TARGET_RESOLVER=_forge_in_jail
allows "belt/in-jail-resolution" jail_assert_target_resolves_in_jail "$NAME"

# (e) DEFAULT resolver against a REAL in-jail socket file -> ALLOWED. This
#     exercises the real jail__resolve_zmx_socket_paths tier walk (not just a
#     forged resolver): a socket under the jailed ZMX_DIR must resolve in-jail.
unset _JAIL_TARGET_RESOLVER
: > "$ZMX_DIR/$NAME.sock"
_JAIL_TARGET_RESOLVER=jail__resolve_zmx_socket_paths
allows "belt/default-resolver-real-in-jail-socket" jail_assert_target_resolves_in_jail "$NAME"
rm -f "$ZMX_DIR/$NAME.sock"

# (f) jail_kill_session is BELTED: a prefix-correct name with NO resolution
#     (MISS) must be REFUSED by kill_session (the belt fires after the prefix
#     guard passes). Forge an empty resolution so the prefix guard passes but the
#     belt does not.
_JAIL_TARGET_RESOLVER=_forge_empty
refuses "belt/killsession-miss-refused" jail_kill_session "$NAME"
# And a prefix-correct, in-jail-resolving name is ALLOWED through kill_session
# (the kill itself is a no-op against a nonexistent session, returns 0).
_JAIL_TARGET_RESOLVER=_forge_in_jail
allows "belt/killsession-in-jail-allowed" jail_kill_session "$NAME"
unset _JAIL_TARGET_RESOLVER

# --- 8. A7 M10: failed-establish + EXIT-trap must NOT touch the prevailing env
# Reproduces the A6 set-u incident mechanism (repro + root-cause: A7 journal
# 2026-06-05): a draft ignores a FAILED jail_establish (which used to leave
# JAIL_ROOT set with HOME still real), then aborts under set -u (1-arg
# jail_register_pid), firing an EXIT-trap jail_teardown that ran `qd ls` /
# `zmx list` against the REAL env. Fix under test: (a) establish failure paths
# clear JAIL_ROOT; (b) teardown refuses env-dependent steps unless
# _JAIL_ESTABLISHED=1 AND HOME is jail-rooted; (c) register_pid refuses bad
# arity instead of set-u-aborting. Everything runs in CHILD shells against a
# FAKE SUT that logs every invocation — the outer jail state is untouched.
_M10_DIR="$JAIL_ROOT/m10-selftest"
mkdir -p "$_M10_DIR"
_M10_LOG="$_M10_DIR/invocations.log"
: > "$_M10_LOG"
printf '#!/bin/sh\necho "INVOKED args=$* HOME=$HOME" >> "%s"\nexit 0\n' "$_M10_LOG" > "$_M10_DIR/fake-sut"
chmod +x "$_M10_DIR/fake-sut"

# (a) the full incident chain: collision-failed establish + 1-arg register_pid
# under set -u + EXIT trap. Fake SUT must record ZERO invocations and the child
# must reach its end sentinel (no set-u abort at the arity guard).
mkdir -p "$_M10_DIR/collide/sbrg-runs/m10x"
cat > "$_M10_DIR/draft-a.sh" << M10EOF
set -u
export JAIL_SB_CMD="$_M10_DIR/fake-sut" JAIL_ZMX_CMD="$_M10_DIR/fake-sut"
export TMPDIR="$_M10_DIR/collide"
. "$HERE/../lib/jail.sh"
trap 'jail_teardown' EXIT
jail_establish m10x          # FAILS (collision); rc deliberately ignored
jail_register_pid 12345      # 1-arg: must REFUSE (rc 1), not abort the shell
echo M10_SENTINEL_REACHED
M10EOF
_m10_out="$(bash "$_M10_DIR/draft-a.sh" 2>/dev/null)"
if [ ! -s "$_M10_LOG" ]; then
    PASS=$((PASS + 1)); printf 'ok   m10/failed-establish-trap-zero-invocations\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL m10/failed-establish-trap-zero-invocations — fake SUT was invoked:\n'; cat "$_M10_LOG"
fi
case "$_m10_out" in
    *M10_SENTINEL_REACHED*) PASS=$((PASS + 1)); printf 'ok   m10/register-pid-arity-refuses-not-aborts\n' ;;
    *) FAIL=$((FAIL + 1)); printf 'FAIL m10/register-pid-arity-refuses-not-aborts — set -u abort still kills the shell\n' ;;
esac

# (b) JAIL_ROOT set by hand but never established → teardown must refuse
# steps 1-2 (zero SUT invocations) while still being callable.
: > "$_M10_LOG"
( set -u
  export JAIL_SB_CMD="$_M10_DIR/fake-sut" JAIL_ZMX_CMD="$_M10_DIR/fake-sut"
  . "$HERE/../lib/jail.sh"
  JAIL_ROOT="$_M10_DIR/collide/sbrg-runs/m10x"
  jail_teardown ) >/dev/null 2>&1
if [ ! -s "$_M10_LOG" ]; then
    PASS=$((PASS + 1)); printf 'ok   m10/unestablished-teardown-refuses-env-steps\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL m10/unestablished-teardown-refuses-env-steps — fake SUT was invoked\n'
fi

# (c) POSITIVE control (belt must not make teardown vacuous): a SUCCESSFUL
# establish + teardown DOES drive the SUT ls step.
: > "$_M10_LOG"
( set -u
  export JAIL_SB_CMD="$_M10_DIR/fake-sut" JAIL_ZMX_CMD="$_M10_DIR/fake-sut"
  export TMPDIR="$_M10_DIR/positive-tmp"
  mkdir -p "$TMPDIR"
  . "$HERE/../lib/jail.sh"
  jail_establish m10pos || exit 1
  jail_teardown ) >/dev/null 2>&1
if [ -s "$_M10_LOG" ] && grep -q "args=ls" "$_M10_LOG"; then
    PASS=$((PASS + 1)); printf 'ok   m10/established-teardown-still-drives-sut (positive control)\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL m10/established-teardown-still-drives-sut — belt over-refuses (teardown vacuous)\n'
fi

jail_teardown

printf '\n--- test_jail_refusal: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
