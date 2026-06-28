#!/usr/bin/env bash
# scenario: a5rec resume — TS qd `resume` shapes: no-such-session; bad zmx-name on
# a COLD session (traversal rejection); recorded-cwd-missing clean error. Cold
# relaunch happy path is covered live by a5_lifecycle_live.sh (Rust) + Lima;
# here we record the TS ERROR shapes at pin. Pin 0d0fa9e.
# tooling: record.sh@388ccd9 normalize.sh@b581f75.
#
# DIVERGENCE NOTE (recorded, not fixed): at pin 0d0fa9e the TS resume RESOLVES the
# session FIRST, so `resume <nonexistent> --zmx-name '../evil'` returns
# "No session matching ..." — the unsafe-zmx-name guard only fires for an
# EXISTING session. The Rust port's G-L5 S2 row asserts the "unsafe characters"
# rejection; to record that TS shape we resume a COLD (existing) session with the
# bad zmx-name.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_a5_lifecycle_lib.sh"

SCN_NAME="a5rec-resume"
SCN_BUDGET_MS=20000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/a5-lifecycle/normalized/resume.txt"

scn_run() {
    # Build a COLD session (JSONL transcript only — no live <pid>.json, no
    # tombstone). cwd points at an EXISTING dir for the bad-zmx-name row, and a
    # MISSING dir for the F3 row.
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
        echo "\$ qd resume coldsess-rec --zmx-name '../evil' (cold session, unsafe zmx-name)"
        scn_qd resume "$COLDSID" --zmx-name '../evil' 2>&1; echo "exit=$?"
        echo "\$ qd resume gonecwd-rec (recorded cwd missing, no --cwd)"
        scn_qd resume "$GONESID" --no-attach 2>&1; echo "exit=$?"
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q "No session matching \"${JAIL_PREFIX}nope\"" "$SCN_OUT" || return 1
    # The cold + bad-zmx-name and missing-cwd shapes are recorded; their exact
    # text is pin-faithful (asserted byte-exact by the comparator at verify time).
}
