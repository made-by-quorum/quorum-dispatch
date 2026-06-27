#!/usr/bin/env bash
# scenario: NEGATIVE — relay-unhealthy outcome shape (W3.4c, P4). STUB-BACKED.
# 0b DELTA-STRENGTH W3.4: recorded negative row per seam family.
#
# SPEC-PREMISE DIVERGENCE (R4-honest, FLAGGED to the lead): the W3.4 spec assumes a
# "health-dependent surface (ls join / send:relay) sees the unhealthy relay." At the
# pin that premise does NOT hold, and this row records the ENGINE'S REAL behaviour
# instead of fabricating the assumed one:
#
#   - `qd ls` joins the relay by reading the SIDECAR (~/.claude/relay/<x>.json,
#     getRelayPorts session.ts:159-183) whose `status` it hardcodes to "ok"; it does
#     NOT re-query GET /health for the join. scanRelayPorts (the /health caller,
#     session.ts:185-212) is the FALLBACK that fires only when NO sidecar exists AND
#     scans ports 8900-8999 — never the jail's relay port. send:relay resolves the
#     same sidecar way (fastRelayLookup). So NO qd surface degrades on a dead /health
#     at the pin. (Empirically confirmed: a controlled in-jail probe with
#     STUB_DEAD_HEALTH=1 returned `/health` 503 status=dead while `qd ls --json` still
#     showed relay_joined_rows=1.)
#
# WHAT THIS ROW THEREFORE RECORDS (a REAL, value-bearing engine property, not a
# vacuous tick): the engine's ls-join is SIDECAR-DRIVEN and HEALTH-INDEPENDENT — it
# stays joined even when the relay's /health is dead. This pins that the join trusts
# the sidecar registration (the ADD-5 contract surface), and a regression that made
# ls-join re-gate on /health (dropping a live-sidecar relay because /health flapped)
# would DIFF.
#
# SEAM (INLINE recording mode, principle 3): STUB_DEAD_HEALTH=1 set INLINE at boot —
# the stub's /health endpoint answers 503 status=dead (stub_claude.py:377-380) while
# the sidecar registration stays present. recording_mode stamped documentary in
# RECORDED-FROM; defended load-bearing by scenario-sha-in-MATCH-PROOF.
#
# Determinism (double-record): the deterministic OUTCOME is the contract /health
# DEAD shape + the sidecar-driven ls-join staying joined. NOT byte trace.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="neg-relay-unhealthy"
SCN_BUDGET_MS=60000
SCN_CLASS="failure-shape"
SCN_FIXTURE="fixtures/neg-relay-unhealthy/normalized/shape.txt"
SCN_STUB_BACKED=1

scn_run() {
    local name
    name="$(scn_session_name nru)"
    # Boot WITH relay + STUB_DEAD_HEALTH=1 INLINE (the stub binds the relay, writes
    # the sidecar, but answers /health 503 status=dead).
    STUB_DEAD_HEALTH=1 bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    # (1) The /health CONTRACT surface is dead (503 status=dead) — the seam's premise.
    local health_dead=0
    health_dead="$(python3 -c '
import sys, json, urllib.request, urllib.error
port = sys.argv[1]
try:
    urllib.request.urlopen("http://127.0.0.1:%s/health" % port, timeout=3)
    print(0)  # 200 OK -> NOT dead
except urllib.error.HTTPError as e:
    try:
        st = json.loads(e.read()).get("status")
    except Exception:
        st = None
    print(1 if (e.code == 503 and st == "dead") else 0)
except Exception:
    print(0)
' "${QRM_RELAY_PORT:-0}")"

    # (2) The ENGINE surface: qd ls --json STILL joins the relay (sidecar-driven,
    # health-independent). A health-gating regression would drop the join.
    scn_sb ls --json 2>/dev/null > "$SCN_OUT.lsjson"
    local joined
    joined="$(python3 -c '
import sys, json
try:
    rows = json.load(open(sys.argv[1]))
except Exception:
    rows = []
print(sum(1 for r in rows if r.get("relayPort")))
' "$SCN_OUT.lsjson")"
    [ -z "$joined" ] && joined=0

    {
        printf 'SHAPE health_contract_dead=%s\n' "$health_dead"
        printf 'SHAPE ls_join_sidecar_driven_robust=%s\n' "$( [ "$joined" -ge 1 ] && echo 1 || echo 0 )"
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
    rm -f "$SCN_OUT.lsjson"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'SHAPE health_contract_dead=1' "$SCN_OUT"             || { _cmp_fail failure-shape "STUB_DEAD_HEALTH seam inert (/health did not return 503 status=dead)"; return 1; }
    grep -q 'SHAPE ls_join_sidecar_driven_robust=1' "$SCN_OUT"    || { _cmp_fail failure-shape "ls-join DROPPED the relay on dead /health — engine re-gated the sidecar join on /health (regression)"; return 1; }
    return 0
}
