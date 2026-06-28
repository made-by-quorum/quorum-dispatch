#!/usr/bin/env bash
# scenario: relay-health — CONTRACT SURFACE (ADD-5), STUB-BACKED. PROVISIONAL->resolved.
# 0b DELTA-STRENGTH W3.2 (P1): VALUE-BEARING cross-checks on RAW PRE-NORMALIZATION values.
#
# Corpus entry: relay-health. Per ADD-5 the relay SERVER is the external cc-relay
# DRIVER; the qd ENGINE owns only the messaging CONTRACT, so this row records ONLY
# the contract surface (survives a transport-driver swap):
#   (a) registration sidecar SHAPE   — ~/.claude/relay/<x>.json {sessionId,port,pid,status}
#                                       (session.ts getRelayPorts 159-183)
#   (b) GET /health                  — RelayHealth {sessionId,port,pid,status}
#                                       (session.ts 185-212)
#   (c) POST /message -> {message_id}— the send:relay client contract (send.ts 414-426)
#   (d) ls join                      — relayPort surfaces in `qd ls` by PID-parentage
#                                       (session.ts 845-873, 922)
#
# W3.2 STRENGTHENING (value-bearing cross-checks, red-team B2 PRE-NORMALIZATION rule):
# the four shape lines above are shape-only; a swap-the-pids / wrong-sessionId /
# foreign-parent relay would pass them. So we ALSO compute cross-field JOINS on the
# RAW (un-normalized) values and emit DERIVED equality/boolean lines:
#   - sidecar_pid_eq_health_pid=1     : sidecar.pid == /health.pid (same relay child)
#   - sessionid_chain_eq=1            : sidecar.sessionId == /health.sessionId ==
#                                       the qd-ls-join row's sessionId (one identity)
#   - message_id=mid-4fab57b3a2a5     : the deterministic stub token for the FIXED
#                                       probe text "contract-probe" (mid-<sha1[:12]>),
#                                       asserted LITERALLY byte-exact in the fixture
#   - relay_pid_is_child_of_claude=1  : ps parentage — relay child's PPID == claude pid
#   - relay_pid_ne_claude_pid=1       : the relay child is a SEPARATE process
# PRE-NORMALIZATION is BINDING (red-team B2): if these joins were computed on the
# NORMALIZED capture every pid collapses to <PID>, so <PID>==<PID> is vacuously true
# and a swapped/foreign pid would pass. We compute them HERE on the raw values and
# emit the DERIVED booleans (the existing has_message_id=1 pattern), which then
# normalize trivially (a 1 stays a 1; the literal mid- token carries no pid/ts/path).
#
# §S: drives the pinned-TS qd against the stub's LIVE in-jail relay endpoint (the
# stub binds $QRM_RELAY_PORT and writes the sidecar). Comparator class = byte-exact on
# the normalized CONTRACT shape + the derived boolean/equality/token lines.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="relay-health"
SCN_BUDGET_MS=60000
SCN_CLASS="byte-exact"   # contract surface per ADD-5
SCN_FIXTURE="fixtures/relay-health/normalized/contract.txt"
SCN_STUB_BACKED=1

# FIXED probe text -> deterministic stub message_id mid-<sha1(text)[:12]>.
SCN_PROBE_TEXT="contract-probe"

scn_run() {
    local name
    name="$(scn_session_name rl)"
    # Boot a stub-backed session WITH relay (the stub's server:relay flag makes it
    # bind $QRM_RELAY_PORT + write the sidecar). Boot via qd so zmx + PID-file +
    # relay all come up the real way.
    bash -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1 &
    local bootpid=$!
    # Wait for the name-matched PID file (boot complete).
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    {
        # (a) sidecar SHAPE — read the real sidecar the stub wrote; emit the CONTRACT
        # keys only (tokenize port/pid; sessionId is the stub's fixed constant).
        printf 'CONTRACT sidecar:'
        local sc
        sc="$(ls "$HOME"/.claude/relay/*.json 2>/dev/null | head -1)"
        if [ -n "$sc" ]; then
            python3 -c '
import sys, json
d = json.load(open(sys.argv[1]))
keys = [k for k in ("sessionId","port","pid","status") if k in d]
print(" present keys=" + ",".join(sorted(keys)) + " status=" + str(d.get("status")))
' "$sc"
        else
            printf ' MISSING\n'
        fi

        # (b) GET /health — query the LIVE stub relay endpoint.
        printf 'CONTRACT GET /health ->'
        python3 -c '
import sys, json, urllib.request
port = sys.argv[1]
try:
    d = json.loads(urllib.request.urlopen("http://127.0.0.1:%s/health" % port, timeout=3).read())
    keys = [k for k in ("sessionId","port","pid","status") if k in d]
    print(" keys=" + ",".join(sorted(keys)) + " status=" + str(d.get("status")))
except Exception as e:
    print(" ERROR")
' "${QRM_RELAY_PORT:-0}"

        # (c) POST /message -> {message_id} — the send:relay client contract, driven
        # against the live stub endpoint (the real round-trip, not a fabricated line).
        printf 'CONTRACT POST /message ->'
        python3 -c '
import sys, json, urllib.request
port = sys.argv[1]
try:
    req = urllib.request.Request("http://127.0.0.1:%s/message" % port,
        data=json.dumps({"text":sys.argv[2],"from_session":"cli"}).encode(),
        headers={"Content-Type":"application/json"}, method="POST")
    d = json.loads(urllib.request.urlopen(req, timeout=3).read())
    print(" has_message_id=" + ("1" if "message_id" in d else "0"))
except Exception:
    print(" ERROR")
' "${QRM_RELAY_PORT:-0}" "$SCN_PROBE_TEXT"

        # (d) ls join — relayPort surfaces in `qd ls --json` (PID-parentage join).
        printf 'CONTRACT ls-join:'
        scn_qd ls --json 2>/dev/null > "$SCN_OUT.lsjson"
        python3 -c '
import sys, json
try:
    rows = json.load(open(sys.argv[1]))
except Exception:
    rows = []
joined = 0
for r in rows:
    if r.get("relayPort"):
        joined += 1
print(" relay_joined_rows=%d has_relayPort=%s" % (joined, "1" if joined else "0"))
' "$SCN_OUT.lsjson"

        # === W3.2 DERIVED CROSS-CHECKS (computed on RAW pre-normalization values) ===
        # All five derived lines are computed HERE from the raw sidecar/health/ls/ps
        # values and emitted as 0/1 booleans or a literal token, so a swapped/foreign
        # pid or wrong sessionId or empty message_id FLIPS a derived line (B2). The
        # normalizer then collapses these trivially (1 stays 1; the mid- token has no
        # pid/ts/path byte).
        python3 - "$sc" "${QRM_RELAY_PORT:-0}" "$SCN_OUT.lsjson" "$SCN_PROBE_TEXT" "$pidfile" <<'PY'
import sys, json, hashlib, urllib.request, subprocess

sidecar_path, port, lsjson_path, probe_text, pidfile = sys.argv[1:6]

def load(p):
    try:
        return json.load(open(p))
    except Exception:
        return {}

sidecar = load(sidecar_path) if sidecar_path else {}
ls_rows = load(lsjson_path) if lsjson_path else []
pidfile_data = load(pidfile) if pidfile else {}

# /health (raw)
health = {}
try:
    health = json.loads(urllib.request.urlopen("http://127.0.0.1:%s/health" % port, timeout=3).read())
except Exception:
    health = {}

sidecar_pid = sidecar.get("pid")
health_pid = health.get("pid")
sidecar_sid = sidecar.get("sessionId")
health_sid = health.get("sessionId")

# the qd-ls relay-join row's sessionId (the row that carries relayPort).
ls_relay_sid = None
for r in ls_rows:
    if r.get("relayPort"):
        ls_relay_sid = r.get("sessionId")
        break

# (1) sidecar.pid == /health.pid (same relay child process). NOTE the field name
# must NOT end in "pid=<n>" — the normalizer's normalize_pids rule eats any
# "...pid[ =:]+<digits>", which would corrupt a "...pid=1" boolean into "...pid=<PID>"
# and make a swapped-pid mutant pass. "..._same=1" ends in a safe word.
print("relay_sidecar_health_pid_same=%d" %
      (1 if (sidecar_pid is not None and sidecar_pid == health_pid) else 0))

# (2) sessionId chain: sidecar == /health == ls-join row (one identity).
sid_chain = (sidecar_sid is not None
             and sidecar_sid == health_sid
             and (ls_relay_sid is None or ls_relay_sid == sidecar_sid)
             and ls_relay_sid is not None)
print("sessionid_chain_eq=%d" % (1 if sid_chain else 0))

# (3) message_id: deterministic stub token for the FIXED probe text. Asserted
# LITERALLY in the fixture (byte-exact), so an empty/wrong token diffs.
mid = "mid-" + hashlib.sha1(probe_text.encode("utf-8")).hexdigest()[:12]
got_mid = ""
try:
    req = urllib.request.Request("http://127.0.0.1:%s/message" % port,
        data=json.dumps({"text": probe_text, "from_session": "cli"}).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    got_mid = json.loads(urllib.request.urlopen(req, timeout=3).read()).get("message_id", "")
except Exception:
    got_mid = ""
print("message_id=%s" % (got_mid if got_mid == mid else ("MISMATCH:" + str(got_mid))))

# (4)+(5) parentage: the relay child's PPID == the claude (session) pid, and the
# relay child pid != the claude pid (a separate process). claude pid = the PID-file
# pid (the session's main process). Walk ps for the relay child's parent.
claude_pid = pidfile_data.get("pid")
relay_pid = sidecar_pid
ppid = None
if relay_pid is not None:
    try:
        out = subprocess.check_output(["ps", "-o", "ppid=", "-p", str(relay_pid)],
                                      stderr=subprocess.DEVNULL).decode().strip()
        ppid = int(out) if out else None
    except Exception:
        ppid = None
print("relay_pid_is_child_of_claude=%d" %
      (1 if (claude_pid is not None and ppid is not None and ppid == claude_pid) else 0))
# "..._distinct=1" ends in a safe word (NOT "pid=<n>") so the normalizer leaves it.
print("relay_claude_pid_distinct=%d" %
      (1 if (relay_pid is not None and claude_pid is not None and relay_pid != claude_pid) else 0))
PY
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
    rm -f "$SCN_OUT.lsjson"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    # All four contract elements present + healthy against the live stub relay.
    grep -q 'CONTRACT sidecar: present keys=pid,port,sessionId,status status=ok' "$SCN_OUT" || return 1
    grep -q 'CONTRACT GET /health -> keys=pid,port,sessionId,status status=ok' "$SCN_OUT" || return 1
    grep -q 'CONTRACT POST /message -> has_message_id=1' "$SCN_OUT" || return 1
    grep -q 'CONTRACT ls-join: relay_joined_rows=1 has_relayPort=1' "$SCN_OUT" || return 1
    # W3.2 derived cross-checks (PRE-normalization joins).
    grep -q '^relay_sidecar_health_pid_same=1$' "$SCN_OUT" || { _cmp_fail relay-xcheck "sidecar.pid != /health.pid"; return 1; }
    grep -q '^sessionid_chain_eq=1$' "$SCN_OUT"         || { _cmp_fail relay-xcheck "sessionId chain (sidecar==health==ls) broken"; return 1; }
    grep -q '^message_id=mid-4fab57b3a2a5$' "$SCN_OUT"  || { _cmp_fail relay-xcheck "message_id not the deterministic token for the fixed probe"; return 1; }
    grep -q '^relay_pid_is_child_of_claude=1$' "$SCN_OUT" || { _cmp_fail relay-xcheck "relay pid is not a child of the claude pid"; return 1; }
    grep -q '^relay_claude_pid_distinct=1$' "$SCN_OUT"      || { _cmp_fail relay-xcheck "relay pid == claude pid (not a separate process)"; return 1; }
    return 0
}
