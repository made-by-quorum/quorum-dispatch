#!/usr/bin/env bash
# scenario: buildClaudeCmd with CLAUDE_FLAGS — byte-exact. STUB-BACKED.
#
# Corpus entry: `buildClaudeCmd`. buildClaudeCmd (utils.ts:507-513) emits
# `command '<CLAUDE_BIN>' '<flags...>' '<extra...>'`; the `command` builtin bypasses
# the claude() wrapper + PATH-resolves on any machine (L9), and the FLAG ORDER is
# load-bearing. At the pin CLAUDE_FLAGS is a CONSTANT (utils.ts:226-227):
# --dangerously-skip-permissions --dangerously-load-development-channels server:relay
# (NOT config-sourced — ADR-0004 ADD-9a note; "from config" is the Rust target).
#
# §S: qd has no print-cmd surface at the pin, so we observe buildClaudeCmd's REAL
# output via the STUB: the shell expands `command '<shim>' <flags> --name <name>`
# into the stub's argv, which the stub dumps to ~/.claude/stub-launch-argv.txt. The
# scenario boots a stub-backed session and reads that argv to assert the exact flag
# ORDER + the --name arg buildNewExtraArgs appends (utils.ts:263). Comparator class
# = byte-exact on the normalized flag sequence (the binary path is jail-rooted ->
# tokenized; the FLAGS are the load-bearing contract).
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="build-claude-cmd"
SCN_BUDGET_MS=60000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/build-claude-cmd/normalized/cmd.txt"
SCN_STUB_BACKED=1

scn_run() {
    local name
    name="$(scn_session_name bc)"
    bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0
    while [ "$i" -lt 30 ]; do
        [ -f "$HOME/.claude/stub-launch-argv.txt" ] && break
        sleep 1; i=$((i + 1))
    done
    sleep 1

    # Emit the exact launch flags the stub received (buildClaudeCmd output), with
    # the session NAME tokenized (run-specific) so the expectation is stable.
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
    # The CLAUDE_FLAGS constant + --name in the load-bearing order. The argv is one
    # flag per line; assert the exact sequence (after the binary path, which is the
    # jail-rooted shim) — flag presence + ORDER is the contract.
    local got
    got="$(grep -v '/stub-bin/claude$' "$SCN_OUT" | tr '\n' ' ' | sed 's/ *$//')"
    [ "$got" = "--dangerously-skip-permissions --dangerously-load-development-channels server:relay --name <NAME>" ]
}
