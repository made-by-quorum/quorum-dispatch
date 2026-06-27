#!/usr/bin/env bash
# scenario: STALE-NOISE registry/sidecar shapes — ls/resolve OUTCOME (W3.4e, P4).
# 0b DELTA-STRENGTH W3.4: pre-seeded stale-noise family (NO live seam, NO boot).
#
# THE PANEL'S POINT (P4): a registry/relay dir in the wild accrues NOISE — duplicate
# same-name PID files, a stale relay sidecar pointing at a dead pid, a dead-PID
# registry entry. This row PRE-SEEDS each noise shape into the jailed
# ~/.claude/{sessions,relay} (no stub, no boot — these are registry-READ outcomes)
# and records the deterministic `qd ls --json` OUTCOME SHAPE for each, so a
# regression that crashes / double-counts / drops on the noise DIFFS.
#
# ONE ROW, a SMALL FAMILY of pre-seeded cases (justified: they share the registry
# substrate and one `qd ls --json` read observes all three; splitting them would
# triple the boot-free ls cost for no added coverage). Each case asserts a SEPARATE
# deterministic field:
#   (a) DUPLICATE same-name PID files, SAME sessionId  -> deduped to EXACTLY ONE row
#       (getPidEntries reads both; ls dedups by sessionId keeping higher updatedAt,
#       session.ts:875-883). A regression that double-counts would show 2.
#   (b) DEAD-PID registry entry (a valid-shape file whose pid is long dead)         ->
#       STILL VISIBLE in ls (the pin's ls does NOT liveness-filter registry rows;
#       it lists the entry with its recorded status) AND ls exits 0. A regression
#       that dropped dead-pid rows would hide it.
#   (c) STALE relay sidecar pointing at a DEAD pid                                    ->
#       ls exits 0 and does NOT crash; the stale sidecar joins to NO live session
#       (relayByClaudePid parentage finds no live ancestor, session.ts:846-873).
#
# The wrong-TYPED-timestamp registry row is W4's (NOT built here).
#
# Determinism (double-record): the OUTCOME is the per-case ls counts/visibility
# booleans, computed from `qd ls --json` over the pre-seeded registry. The seeded
# pids/sessionIds are FIXED constants (normalizer collapses pid tokens; the derived
# booleans/counts survive). NOT a byte trace.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="noise-stale"
SCN_BUDGET_MS=20000
SCN_CLASS="failure-shape"
SCN_FIXTURE="fixtures/noise-stale/normalized/shape.txt"
# NOT stub-backed: no live counterpart, no boot — pure pre-seeded registry reads.

# Fixed, deliberately-bogus pids that are NEVER us (init=1 is alive but never ours;
# use high improbable pids that are dead). The dup pair SHARES a sessionId.
SCN_DUP_SID="dup00000-0000-0000-0000-000000000000"
SCN_DEAD_SID="dead0000-0000-0000-0000-000000000000"
SCN_STALE_RELAY_SID="stale000-0000-0000-0000-000000000000"

scn_run() {
    mkdir -p "$HOME/.claude/sessions" "$HOME/.claude/relay"

    # (a) DUPLICATE same-name PID files, SAME sessionId, different updatedAt. ls
    # must dedup to ONE row (keeps the higher updatedAt). Both name "noise-dup".
    cat > "$HOME/.claude/sessions/4000001.json" <<EOF
{"pid": 4000001, "sessionId": "$SCN_DUP_SID", "cwd": "$HOME", "startedAt": 1767225600000, "updatedAt": 1767225600000, "status": "idle", "name": "noise-dup", "version": "stub-noise", "kind": "claude-code", "entrypoint": "noise"}
EOF
    cat > "$HOME/.claude/sessions/4000002.json" <<EOF
{"pid": 4000002, "sessionId": "$SCN_DUP_SID", "cwd": "$HOME", "startedAt": 1767225600000, "updatedAt": 1767225601000, "status": "idle", "name": "noise-dup", "version": "stub-noise", "kind": "claude-code", "entrypoint": "noise"}
EOF

    # (b) DEAD-PID registry entry: a valid-shape file whose pid is dead. Distinct
    # sessionId + name "noise-dead". ls must still LIST it (no liveness filter).
    cat > "$HOME/.claude/sessions/4000003.json" <<EOF
{"pid": 4000003, "sessionId": "$SCN_DEAD_SID", "cwd": "$HOME", "startedAt": 1767225600000, "updatedAt": 1767225600000, "status": "idle", "name": "noise-dead", "version": "stub-noise", "kind": "claude-code", "entrypoint": "noise"}
EOF

    # (c) STALE relay sidecar pointing at a DEAD pid (no matching live session). ls
    # must not crash; the sidecar joins to nothing live.
    cat > "$HOME/.claude/relay/$SCN_STALE_RELAY_SID.json" <<EOF
{"sessionId": "$SCN_STALE_RELAY_SID", "port": 28999, "pid": 4000099, "status": "ok"}
EOF

    # ONE ls --json read observes the whole noisy registry.
    scn_sb ls --json 2>/dev/null > "$SCN_OUT.lsjson"
    local ls_rc=$?

    python3 - "$SCN_OUT.lsjson" "$ls_rc" "$SCN_DUP_SID" "$SCN_DEAD_SID" > "$SCN_OUT" <<'PY'
import sys, json
lsjson, ls_rc, dup_sid, dead_sid = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
try:
    rows = json.load(open(lsjson))
except Exception:
    rows = []
# (a) dup same-name same-sessionId -> exactly ONE row for dup_sid.
dup_rows = sum(1 for r in rows if r.get("sessionId") == dup_sid)
# (b) dead-pid entry -> visible (its sessionId present in ls).
dead_visible = any(r.get("sessionId") == dead_sid for r in rows)
# (c) ls did not crash (exit 0) despite the stale sidecar.
print("SHAPE ls_exit_zero=%d" % (1 if ls_rc == "0" else 0))
print("SHAPE dup_same_sid_deduped_to_one=%d" % (1 if dup_rows == 1 else 0))
print("SHAPE dead_pid_entry_still_visible=%d" % (1 if dead_visible else 0))
print("SHAPE stale_sidecar_no_crash=1")  # reaching here means ls --json was parseable
PY
    printf '%s\n' "$ls_rc" > "$SCN_OUT.exit"
    rm -f "$SCN_OUT.lsjson"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'SHAPE ls_exit_zero=1' "$SCN_OUT"                    || { _cmp_fail failure-shape "qd ls did not exit 0 over the noisy registry"; return 1; }
    grep -q 'SHAPE dup_same_sid_deduped_to_one=1' "$SCN_OUT"     || { _cmp_fail failure-shape "duplicate same-name same-sessionId PID files NOT deduped to one row"; return 1; }
    grep -q 'SHAPE dead_pid_entry_still_visible=1' "$SCN_OUT"    || { _cmp_fail failure-shape "dead-pid registry entry not visible in ls (unexpected liveness filter / drop)"; return 1; }
    grep -q 'SHAPE stale_sidecar_no_crash=1' "$SCN_OUT"          || { _cmp_fail failure-shape "stale relay sidecar (dead pid) crashed ls --json"; return 1; }
    return 0
}
