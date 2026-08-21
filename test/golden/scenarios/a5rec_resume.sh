#!/usr/bin/env bash
# scenario: a5rec resume — TS qd `resume` shapes: no-such-session and
# recorded-cwd-missing clean error. Cold relaunch happy path is covered live by
# a5_lifecycle_live.sh (Rust) + Lima; here we record the TS ERROR shapes at pin.
# Pin 0d0fa9e. tooling: record.sh@388ccd9 normalize.sh@b581f75.
#
# RETIRED ROW (FTUE punch R1): this scenario used to carry a third row,
# `resume <cold-session> --zmx-name '../evil'`, recording the TS traversal
# rejection ("Session name ... contains unsafe characters"). `--zmx-name` was a
# dead parked flag naming the retired zmx multiplexer and R1 removed it from the
# CLI, so that row is no longer reproducible AT ALL — it would now record
# `error: unknown option '--zmx-name'`, which is a parse error, not the guard.
# The row and its fixture lines are deleted rather than re-pinned: the guard
# itself is untouched and still reachable through the SESSION NAME, where it is
# asserted by `quorum-qw/src/create.rs` and `tests/start_surface_a.rs:243`.
#
# DIVERGENCE NOTE (recorded, not fixed): at pin 0d0fa9e the TS resume RESOLVES
# the session FIRST, so the guard only ever fired for an EXISTING session.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_a5_lifecycle_lib.sh"

SCN_NAME="a5rec-resume"
SCN_BUDGET_MS=20000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/a5-lifecycle/normalized/resume.txt"

scn_run() {
    # Build a COLD session (JSONL transcript only — no live <pid>.json, no
    # tombstone) with an EXISTING cwd, and one with a MISSING dir for the F3 row.
    #
    # The cold session is no longer the target of a recorded row (its row was the
    # retired --zmx-name one, see the header). It is KEPT as jail state on
    # purpose: this scenario is byte-exact against a TS pin, and removing a
    # session from the jail is a change to what `resume` resolves against. Kept
    # inert rather than deleted so the two surviving rows stay pin-faithful.
    local COLDSID="coldsess-rec"
    local COLDCWD="$JAIL_ROOT/tmp/coldcwd"; mkdir -p "$COLDCWD"
    local COLDSLUG COLDPROJ
    COLDSLUG="$(printf '%s' "$COLDCWD" | sed 's,/,-,g')"
    COLDPROJ="$HOME/.claude/projects/$COLDSLUG"; mkdir -p "$COLDPROJ"
    {
        printf '{"type":"agent-name","agentName":"%scold"}\n' "$JAIL_PREFIX"
        printf '{"type":"user","cwd":"%s","timestamp":"2026-06-01T10:00:00.000Z","message":{"role":"user","content":"hi"}}\n' "$COLDCWD"
    } > "$COLDPROJ/$COLDSID.jsonl"

    local GONESID="gonecwd-rec"
    local GONEPROJ="$HOME/.claude/projects/-gone-proj"; mkdir -p "$GONEPROJ"
    {
        printf '{"type":"agent-name","agentName":"%sgone"}\n' "$JAIL_PREFIX"
        printf '{"type":"user","cwd":"%s","timestamp":"2026-06-01T10:00:00.000Z","message":{"role":"user","content":"hi"}}\n' "$JAIL_ROOT/tmp/this-dir-was-deleted"
    } > "$GONEPROJ/$GONESID.jsonl"

    {
        echo "# RECORDED-FROM pin=0d0fa9e verb=resume (error shapes; cold relaunch covered live by Rust+Lima)"
        echo "\$ qd resume qdrg-nope (no such session)"
        scn_qd resume "${JAIL_PREFIX}nope" 2>&1; echo "exit=$?"
        echo "\$ qd resume gonecwd-rec (recorded cwd missing, no --cwd)"
        scn_qd resume "$GONESID" --no-attach 2>&1; echo "exit=$?"
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q "No session matching \"${JAIL_PREFIX}nope\"" "$SCN_OUT" || return 1
    # The missing-cwd shape is recorded; its exact text is pin-faithful
    # (asserted byte-exact by the comparator at verify time).
}
