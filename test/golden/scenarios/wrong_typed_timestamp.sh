#!/usr/bin/env bash
# scenario: WRONG-TYPED-TIMESTAMP registry row — ls/resolve OUTCOME (W4.3, A1 F3).
# 0b DELTA-STRENGTH W4.3 (orc-3 succession item): pre-seeded dirty-state registry
# file whose startedAt/updatedAt are ISO STRINGS — the shape stub 1.6.0 wrongly
# wrote (numbers were the declared type, session.ts:68-69 PidEntry). NO seam, NO
# boot — a pure registry-READ outcome, joining the W3.4e stale-noise family but
# recorded as its OWN row per the spec.
#
# THE A1 POINT (registry permissive-parse): TS reads the registry with a bare
# JSON.parse + `as PidEntry` cast (getPidEntries, session.ts:335-346) — NO runtime
# type validation — then renders via `new Date(pid.startedAt)`. An ISO-string
# startedAt is therefore TOLERATED: the dynamic read keeps the row, `qd ls --json`
# shows the session, exit 0. (Empirically confirmed at pin 8c59ec4: the row renders
# with startedAt/lastActive round-tripped back to the ISO strings — see RECORDED-FROM.)
# The Rust side must match this PRESENCE through the A1 PR#20 per-field-permissive
# deserializer (RegistryEntry::from_value): a wrong-typed timestamp DEGRADES to
# default, the ROW SURVIVES, visible to ls/resolve again. (Byte-parity for the
# wrong-typed field is NOT claimed — the row's PRESENCE is the contract.)
#
# Determinism (double-record): the OUTCOME is derived BOOLEANS over `qd ls --json`
# (row visible by sessionId, ls exit 0), computed PRE-normalization from a FIXED
# pre-seeded sessionId. NOT a byte trace of the (run-independent) registry. The
# seeded pid is a fixed bogus constant the pid-normalizer collapses; the booleans
# survive trivially.
#
# Mutant (run_mutation_real.sh): the pre-A1-PR#20 Rust behavior — a wrong-typed
# field drops the WHOLE ROW (session silently invisible to ls/resolve) — must flip
# wrong_typed_row_visible 1->0 and the row RED.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="wrong-typed-timestamp"
SCN_BUDGET_MS=20000
SCN_CLASS="failure-shape"
SCN_FIXTURE="fixtures/wrong-typed-timestamp/normalized/shape.txt"
# NOT stub-backed: no live counterpart, no boot — a pure pre-seeded registry read.

# Fixed, deliberately-bogus pid that is NEVER us; fixed sessionId for determinism.
SCN_WRONGTS_SID="wrongts0-0000-0000-0000-000000000000"

scn_run() {
    mkdir -p "$HOME/.claude/sessions"

    # A registry file with ISO-STRING startedAt/updatedAt (the stub-1.6.0 shape).
    # Every OTHER field is valid (pid number, sessionId, cwd, status, name) so the
    # ONLY anomaly is the wrong-typed timestamps — isolating the A1 F3 surface.
    cat > "$HOME/.claude/sessions/4100001.json" <<EOF
{"pid": 4100001, "sessionId": "$SCN_WRONGTS_SID", "cwd": "$HOME", "startedAt": "2026-06-05T12:00:00.000Z", "updatedAt": "2026-06-05T12:05:00.000Z", "status": "idle", "name": "wrong-ts", "version": "stub-noise", "kind": "claude-code", "entrypoint": "noise"}
EOF

    # ONE ls --json read observes the registry.
    scn_sb ls --json 2>/dev/null > "$SCN_OUT.lsjson"
    local ls_rc=$?

    python3 - "$SCN_OUT.lsjson" "$ls_rc" "$SCN_WRONGTS_SID" > "$SCN_OUT" <<'PY'
import sys, json
lsjson, ls_rc, wt_sid = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    rows = json.load(open(lsjson))
except Exception:
    rows = []
# (A1) the wrong-typed-timestamp row is VISIBLE (its sessionId present in ls).
wt_visible = any(r.get("sessionId") == wt_sid for r in rows)
# ls did not crash (exit 0) despite the wrong-typed timestamps.
print("SHAPE ls_exit_zero=%d" % (1 if ls_rc == "0" else 0))
print("SHAPE wrong_typed_row_visible=%d" % (1 if wt_visible else 0))
PY
    printf '%s\n' "$ls_rc" > "$SCN_OUT.exit"
    rm -f "$SCN_OUT.lsjson"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'SHAPE ls_exit_zero=1' "$SCN_OUT"            || { _cmp_fail failure-shape "qd ls did not exit 0 over the wrong-typed-timestamp registry"; return 1; }
    grep -q 'SHAPE wrong_typed_row_visible=1' "$SCN_OUT" || { _cmp_fail failure-shape "wrong-typed-timestamp row NOT visible in ls (whole-row drop — pre-A1-PR#20 behavior)"; return 1; }
    return 0
}
