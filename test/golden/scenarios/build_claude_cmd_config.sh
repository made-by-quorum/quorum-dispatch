#!/usr/bin/env bash
# scenario: buildClaudeCmd with a NON-DEFAULT config flag — byte-exact.
# *** EXPECTATION-ONLY / RUST-TARGET — UNticked, NOT recorded. ***  STUB-BACKED.
#
# Corpus entry: `buildClaudeCmd` (config-source variant). The sibling
# `build_claude_cmd.sh` row asserts the DEFAULT flag triple. The panel review
# (0b-panel-dispositions P8, opus M5 + gpt) showed that a byte-exact assertion on
# the DEFAULT flags passes a BROKEN config-loader that simply hard-codes the same
# three flags and ignores config entirely: at the pin `CLAUDE_FLAGS` IS a hard
# constant (utils.ts:226-227), so the default-flags row cannot distinguish a real
# config-loader from a constant. This row closes that gap by asserting that a
# NON-DEFAULT flag set, supplied via the config seam, ACTUALLY reaches the launch
# argv — i.e. the loader READS config, it does not echo a constant.
#
# WHY EXPECTATION-ONLY (no TS counterpart; sanctioned by ADR-0010 §(a)): at the pin
# the TS engine has NO config seam for CLAUDE_FLAGS — the triple is a compile-time
# constant, so this scenario would FAIL against pinned TS BY CONSTRUCTION (TS emits
# the constant regardless of any override). It is therefore a RUST-TARGET row: the
# expectation is what the Rust `launch::claude_flags()` precedence (ADR 0006:
# QD_CLAUDE_FLAGS env > config.toml `claude_flags` > default triple) MUST produce.
# There is no symmetric counterpart to record now (recording the TS side would
# launder the constant-echo bug into gold), so the row is UNticked and carries NO
# fixture/MATCH-PROOF until the Rust engine exists to DRIVE it (W?+). It is NOT
# wired into the green verify suite — adding the matrix row UNticked keeps it out of
# the gate by construction (scenarios are invoked per-row, never auto-globbed).
#
# The config seam used here is the env override `QD_CLAUDE_FLAGS` (ADR 0006 tier 1),
# the cheapest per-invocation seam; a non-default single flag is supplied and the
# stub (which dumps its received launch argv verbatim) reports what buildClaudeCmd
# actually emitted. The assertion is byte-exact on the normalized flag sequence.
#
# *** DO NOT record this row against TS. *** It exists to PIN the Rust expectation
# and to be the driver the Rust engine is judged against at its gate.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="build-claude-cmd-config"
SCN_BUDGET_MS=60000
SCN_CLASS="byte-exact"
# NO SCN_FIXTURE: expectation-only/Rust-target — no recorded golden until the Rust
# engine drives it. The EXPECTED argv is encoded inline in scn_assert below.
SCN_FIXTURE=""
SCN_STUB_BACKED=1
SCN_EXPECTATION_ONLY=1   # marker: NEVER auto-recorded / never a vacuous tick.

# The non-default flag set this row supplies through the config seam. A SINGLE,
# clearly-non-default flag so the assertion is unambiguous: a constant-echo loader
# (the bug this row catches) would emit the default triple instead and DIFF.
SCN_CONFIG_FLAGS="--dangerously-skip-permissions"

scn_run() {
    local name
    name="$(scn_session_name bcc)"
    # Drive qd-under-test with the config-seam override in the environment. The
    # Rust engine MUST honor it (ADR 0006); pinned TS ignores it (constant) — which
    # is exactly why this is expectation-only.
    QD_CLAUDE_FLAGS="$SCN_CONFIG_FLAGS" \
        bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0
    while [ "$i" -lt 30 ]; do
        [ -f "$HOME/.claude/stub-launch-argv.txt" ] && break
        sleep 1; i=$((i + 1))
    done
    sleep 1

    if [ -f "$HOME/.claude/stub-launch-argv.txt" ]; then
        sed "s/${name}/<NAME>/g" "$HOME/.claude/stub-launch-argv.txt" > "$SCN_OUT"
    else
        : > "$SCN_OUT"
    fi
    printf '0\n' > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    # The argv (one flag per line) after the jail-rooted binary path MUST be the
    # NON-DEFAULT flag(s) the config seam supplied, + the --name arg — NOT the
    # default triple. A constant-echo loader emits the default triple here and FAILS.
    local got
    got="$(grep -v '/stub-bin/claude$' "$SCN_OUT" | tr '\n' ' ' | sed 's/ *$//')"
    [ "$got" = "--dangerously-skip-permissions --name <NAME>" ]
}
