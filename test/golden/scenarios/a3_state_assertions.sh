#!/usr/bin/env bash
# test/golden/scenarios/a3_state_assertions.sh
#
# A3 gate pass(a) §6.1 — SCRIPTED STATE ASSERTIONS for the functional matrix.
#
# These are the "STATE ASSERTION" column of test/golden/a3-matrix.md, run IN-JAIL
# (ADD-4) against the RUST binary. NOTHING here is hand-claimed: every assertion
# observes a real jail-fs effect (recursive sha of HOME, marks.jsonl line count,
# registry dir contents, zmx socket dir) before/after the verb runs.
#
# Output is a PASS/FAIL line per assertion + a SUMMARY. The matrix cites this
# script's row IDs (SA-<verb>-<n>). Bash 3.2 floor; builds via build-lock.
#
# Verbs covered (spec §6.1 + orchestrator riders 2026-06-04):
#   read-only no-mutation : ls / info / whoami / ping / live / config-path
#   mark                  : appends EXACTLY one well-formed JSON line; round-trips
#                           byte-identical; org-vocabulary payload passes through
#                           UNINTERPRETED (rider 1); failure leaves file unchanged
#   new                   : unresolvable --agent fails CLOSED (nonzero, NO registry
#                           entry, NO zmx socket) — the A2-wired fail-closed path
#   attach                : unresolvable session → exit 1, no state change
#   send/relay moved-stub : exit 1, no state change
#   STUB verbs            : honest-stub stderr + exit, NO state mutation
#
# A5 RECONCILIATION NOTE (2026-06-05, qa-a5, gate A5/M6, branch agent/a5-qa):
#   When this file was authored (A3) the verbs resume/kill/reconcile/gc/bootstrap/
#   update and `ping <session>` were honest STUBS that printed "not yet implemented
#   in the Rust engine (<phase>)" and exited 1 with NO mutation. A4 made send:*/wait
#   real; A5 (this phase) makes resume/kill/reconcile/gc/bootstrap/update/ping real.
#   The original "STUB verbs" block asserted the now-DEAD stub contract and reds
#   the gate spuriously (msg_ok=0 because the stub line is gone). Per the A5 gate
#   dispatch (reconcile a3_state_assertions to the REAL-verb equivalents preserving
#   the original INTENT), the block below replaces each stub assertion with the
#   real verb's no-mutation-on-error / idempotent equivalent — same intent (the
#   verb must not silently mutate state when it errors or finds nothing to do),
#   updated target (real exit codes + real output shapes at pin 0d0fa9e). The A3
#   header lineage above is preserved deliberately. Cross-phase: A3's pass-(b) wake
#   must be told its asserted surface moved (orc directive relay-1780664064420-12).
# ---------------------------------------------------------------------------
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
RUST_BIN="${A3_RUST_BIN:-$REPO_ROOT/target/debug/qd}"
export JAIL_SB_CMD="$RUST_BIN"

A3_REAL_HOME="$HOME"
A3_REAL_TMPDIR="${TMPDIR:-/tmp}"
export JAIL_REAL_HOME="$A3_REAL_HOME"
. "$REPO_ROOT/test/golden/lib/jail.sh"

PASS=0; FAIL=0; FAILED=""
pass() { PASS=$((PASS+1)); printf '  [PASS] %-28s %s\n' "$1" "$2"; }
fail() { FAIL=$((FAIL+1)); FAILED="$FAILED $1"; printf '  [FAIL] %-28s %s\n' "$1" "$2"; }

# Build once.
if [ "${A3_SKIP_BUILD:-0}" != "1" ]; then
    "$REPO_ROOT/scripts/build-lock.sh" cargo build -p qd --bin qd >/dev/null 2>&1 \
        || { echo "FATAL: build failed" >&2; exit 3; }
fi
[ -x "$RUST_BIN" ] || { echo "FATAL: rust binary missing: $RUST_BIN" >&2; exit 3; }

# Recursive content+name sha of a dir (order-stable). Empty/missing => fixed token.
dir_sha() {
    local d="$1"
    if [ ! -d "$d" ]; then echo "MISSING-DIR"; return; fi
    ( cd "$d" && find . -type f -print0 2>/dev/null | LC_ALL=C sort -z \
        | while IFS= read -r -d '' f; do
            printf '%s:' "$f"; shasum "$f" 2>/dev/null | awk '{print $1}'
          done | shasum | awk '{print $1}' )
}

# Seed the 3 forged sessions into the current jailed HOME.
seed_sessions() {
    local sd="$HOME/.claude/sessions"; mkdir -p "$sd"
    printf '%s\n' '{"pid":99001,"sessionId":"aaaaaaaa-1111-2222-3333-444444444444","cwd":"/Users/jailuser/proj/alpha","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"alpha-worker","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}' > "$sd/99001.json"
    printf '%s\n' '{"pid":99002,"sessionId":"bbbbbbbb-1111-2222-3333-555555555555","cwd":"/Users/jailuser/proj/beta","startedAt":1717000100000,"updatedAt":1717003700000,"status":"busy","name":"beta-builder","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}' > "$sd/99002.json"
    printf '%s\n' '{"pid":99003,"sessionId":"cccccccc-1111-2222-3333-666666666666","cwd":"/Users/jailuser/proj/gamma","startedAt":1717000200000,"updatedAt":1717003800000,"status":"shell","name":"gamma-shell","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}' > "$sd/99003.json"
}

# fresh_jail — restore real HOME/TMPDIR then establish a new jail + seed.
fresh_jail() {
    export HOME="$A3_REAL_HOME"; export TMPDIR="$A3_REAL_TMPDIR"
    jail_establish "a3sa${RANDOM}" || { echo "FATAL: jail failed" >&2; exit 3; }
    seed_sessions
}

# run_sb <args...> : run the Rust binary under an 8s alarm, stdin from /dev/null.
# Sets RC / OUT_FILE / ERR_FILE.
run_sb() {
    OUT_FILE="$JAIL_ROOT/tmp/o"; ERR_FILE="$JAIL_ROOT/tmp/e"
    jail_assert_established || { echo "FATAL: jail lost" >&2; exit 3; }
    perl -e 'alarm shift; exec @ARGV' 8 "$RUST_BIN" "$@" \
        < /dev/null > "$OUT_FILE" 2> "$ERR_FILE"
    RC=$?
}

# ============================================================================
echo "========= A3 state assertions (Rust, in-jail) ========="
echo "  RUST_BIN = $RUST_BIN"
echo

# ---- READ-ONLY no-mutation (ls/info/whoami/ping/live/config-path) -----------
# For each read-only verb: sha HOME before, run, sha HOME after, assert EQUAL.
ro_no_mutation() {
    local id="$1"; shift
    fresh_jail
    local before after
    before="$(dir_sha "$HOME")"
    run_sb "$@"
    after="$(dir_sha "$HOME")"
    if [ "$before" = "$after" ]; then
        pass "$id" "exit=$RC HOME-sha unchanged ($before)"
    else
        fail "$id" "exit=$RC HOME MUTATED ($before -> $after)"
    fi
    jail_teardown
}
ro_no_mutation SA-ls-1          ls
ro_no_mutation SA-ls-json-1     ls --json
ro_no_mutation SA-info-1        info alpha-worker
ro_no_mutation SA-whoami-1      whoami
ro_no_mutation SA-ping-1        ping             # exit 3, no mutation
ro_no_mutation SA-ping-prefix-1 ping --prefix al # stub exit 1, no mutation
ro_no_mutation SA-config-path-1 config path
# live: non-TTY exits fast; assert no mutation too.
ro_no_mutation SA-live-1        live

# ---- mark: appends EXACTLY one well-formed line; round-trips identical -------
# NB: jail.sh exports QD_HOME, and mark honors it (H4) — so under the jail the
# marks file lives at <QD_HOME>/state, NOT <HOME>/.quorum/dispatch/state. Derive the path the
# same way the engine does (sbHome = QD_HOME || <HOME>/.quorum/dispatch).
fresh_jail
QD_DATA="${QD_HOME:-$HOME/.quorum/dispatch}"
MARKS="$QD_DATA/state/marks.jsonl"
[ -f "$MARKS" ] && fail SA-mark-pre "marks.jsonl exists before mark" || pass SA-mark-pre "no marks.jsonl before first mark"
PAYLOAD='{"k1":"v1","nested":{"a":[1,2,3]},"u":"café ☕"}'
run_sb mark alpha-worker "$PAYLOAD"
if [ "$RC" = "0" ] && [ -f "$MARKS" ]; then
    # A7 ratchet fix (2026-06-05): A6 telemetry (merge-ruled 13:20) appends a
    # USAGE event line at every mark verb — `qd mark` now writes 2 lines (the
    # mark line FIRST, then {event,verb:"mark",...}). Pre-A6 this expected 1.
    nlines="$(wc -l < "$MARKS" | tr -d ' ')"
    if [ "$nlines" = "2" ]; then
        pass SA-mark-1 "exit=0, mark line + A6 usage line appended (2 lines)"
    else
        fail SA-mark-1 "exit=0 but $nlines lines (expected 2: mark + A6 usage)"
    fi
    usage_ok="$(sed -n '2p' "$MARKS" | python3 -c '
import sys,json
o=json.load(sys.stdin)
assert "event" in o and o.get("verb")=="mark", "line 2 not a mark-verb usage event"
print("OK")' 2>/dev/null)"
    [ "$usage_ok" = "OK" ] \
        && pass SA-mark-usage "line 2 is the A6 usage event (verb=mark)" \
        || fail SA-mark-usage "line 2 is not the A6 mark usage event"
    # Line parses as JSON with ts + sessionId + payload; payload round-trips byte-identical.
    line="$(head -1 "$MARKS")"
    parsed_ok="$(printf '%s' "$line" | python3 -c '
import sys,json
o=json.load(sys.stdin)
assert "ts" in o and "sessionId" in o and "payload" in o, "missing keys"
exp=json.loads('"'"''"$PAYLOAD"''"'"')
assert o["payload"]==exp, "payload not equal"
# byte-identical round-trip: re-dump payload with sorted keys both sides
import json as j
assert j.dumps(o["payload"],sort_keys=True)==j.dumps(exp,sort_keys=True)
print("OK")
' 2>/dev/null)"
    [ "$parsed_ok" = "OK" ] && pass SA-mark-2 "JSON parses; ts+sessionId+payload; payload round-trips identical" \
                            || fail SA-mark-2 "payload/structure mismatch (line=$line)"
    # sessionId resolved to alpha-worker's id.
    sid="$(printf '%s' "$line" | python3 -c 'import sys,json;print(json.load(sys.stdin)["sessionId"])' 2>/dev/null)"
    [ "$sid" = "aaaaaaaa-1111-2222-3333-444444444444" ] \
        && pass SA-mark-3 "sessionId resolved to alpha-worker id" \
        || fail SA-mark-3 "sessionId wrong: $sid"
else
    fail SA-mark-1 "exit=$RC, marks.jsonl present=$( [ -f "$MARKS" ] && echo yes || echo no )"
fi

# rider 1: org-vocabulary payload passes through UNINTERPRETED (no key behavior).
ORG_PAYLOAD='{"on_behalf_of":"x","role_claimed":"y","reports_to":"z","succeeds":"w"}'
run_sb mark alpha-worker "$ORG_PAYLOAD"
# A7 ratchet fix: 2 marks × (mark line + A6 usage line) = 4 lines; the second
# mark's PAYLOAD line is line 3 (tail -1 would grab its usage event).
nlines2="$(wc -l < "$MARKS" | tr -d ' ')"
if [ "$RC" = "0" ] && [ "$nlines2" = "4" ]; then
    line2="$(sed -n '3p' "$MARKS")"
    org_ok="$(printf '%s' "$line2" | python3 -c '
import sys,json
o=json.load(sys.stdin)
exp=json.loads('"'"''"$ORG_PAYLOAD"''"'"')
# Payload byte-identical: ALL org keys present verbatim, NOTHING added/removed.
assert o["payload"]==exp, "org payload altered"
assert set(o["payload"].keys())=={"on_behalf_of","role_claimed","reports_to","succeeds"}, "keys changed"
print("OK")
' 2>/dev/null)"
    [ "$org_ok" = "OK" ] \
        && pass SA-mark-org "org-vocab payload appended UNINTERPRETED (rider 1): byte-identical, no key behavior" \
        || fail SA-mark-org "org payload was interpreted/altered (line=$line2)"
else
    fail SA-mark-org "exit=$RC, lines=$nlines2 (expected exit0/4 lines: 2 marks + 2 A6 usage)"
fi

# mark failure cases leave the file UNCHANGED.
mark_sha_before="$(dir_sha "$QD_DATA")"
run_sb mark alpha-worker '[1,2,3]'           # non-object payload
rc_nonobj=$RC
run_sb mark nosuch-session-xyz '{"k":1}'      # unresolvable session
rc_nosess=$RC
mark_sha_after="$(dir_sha "$QD_DATA")"
if [ "$rc_nonobj" = "1" ] && [ "$rc_nosess" = "1" ] && [ "$mark_sha_before" = "$mark_sha_after" ]; then
    pass SA-mark-fail "non-object(exit1) + unresolvable(exit1) leave marks.jsonl UNCHANGED"
else
    fail SA-mark-fail "rc_nonobj=$rc_nonobj rc_nosess=$rc_nosess sha($mark_sha_before vs $mark_sha_after)"
fi
jail_teardown

# ---- H4: mark HONORS QD_HOME for the state dir (commands/bootstrap.ts:88-96) --
# Contract: marks land at <sbHome>/state/marks.jsonl where sbHome = QD_HOME ||
# <HOME>/.quorum/dispatch. (a) QD_HOME set → under <QD_HOME>/state, NOT <HOME>/.quorum/dispatch/state.
# (b) QD_HOME unset → under <HOME>/.quorum/dispatch/state. QD_HOME flows ONLY through the
# injected Env seam (L9a).

# (a) QD_HOME set to a dir OUTSIDE the default ~/.quorum/dispatch.
fresh_jail
SBH="$JAIL_ROOT/tmp/sbdata_override"
mkdir -p "$SBH"
# run_sb with an QD_HOME env override (custom, mirrors run_sb's alarm+stdin).
OUT_FILE="$JAIL_ROOT/tmp/o"; ERR_FILE="$JAIL_ROOT/tmp/e"
jail_assert_established || { echo "FATAL: jail lost" >&2; exit 3; }
QD_HOME="$SBH" perl -e 'alarm shift; exec @ARGV' 8 "$RUST_BIN" mark alpha-worker '{"k":"v"}' \
    < /dev/null > "$OUT_FILE" 2> "$ERR_FILE"
RC=$?
sbhome_marks="$SBH/state/marks.jsonl"
default_marks="$HOME/.quorum/dispatch/state/marks.jsonl"
if [ "$RC" = "0" ] && [ -f "$sbhome_marks" ] && [ ! -f "$default_marks" ]; then
    nlsb="$(wc -l < "$sbhome_marks" | tr -d ' ')"
    # A7 ratchet fix: mark line + A6 usage line (see SA-mark-1).
    [ "$nlsb" = "2" ] \
        && pass SA-mark-sbhome-set "QD_HOME set → marks under <QD_HOME>/state (mark + usage), NOT <HOME>/.quorum/dispatch/state" \
        || fail SA-mark-sbhome-set "QD_HOME marks present but $nlsb lines (expected 2: mark + A6 usage)"
else
    fail SA-mark-sbhome-set "exit=$RC sbhome_marks=$( [ -f "$sbhome_marks" ] && echo yes || echo no ) default_marks=$( [ -f "$default_marks" ] && echo yes || echo no )"
fi
jail_teardown

# (b) QD_HOME UNSET → default <HOME>/.quorum/dispatch/state/marks.jsonl. NB: jail.sh exports
# QD_HOME, so we must explicitly UNSET it for this run to exercise the default.
fresh_jail
OUT_FILE="$JAIL_ROOT/tmp/o"; ERR_FILE="$JAIL_ROOT/tmp/e"
jail_assert_established || { echo "FATAL: jail lost" >&2; exit 3; }
env -u QD_HOME perl -e 'alarm shift; exec @ARGV' 8 "$RUST_BIN" mark alpha-worker '{"k":"v"}' \
    < /dev/null > "$OUT_FILE" 2> "$ERR_FILE"
RC=$?
default_marks="$HOME/.quorum/dispatch/state/marks.jsonl"
if [ "$RC" = "0" ] && [ -f "$default_marks" ]; then
    pass SA-mark-sbhome-default "QD_HOME unset → marks default to <HOME>/.quorum/dispatch/state"
else
    fail SA-mark-sbhome-default "exit=$RC default_marks=$( [ -f "$default_marks" ] && echo yes || echo no )"
fi
jail_teardown

# ---- new: unresolvable --agent fails CLOSED (no registry entry, no zmx socket)
# CONTRACT (brief / A2 fail-closed): nonzero exit, NO registry entry created, NO
# zmx SOCKET. NB: a recursive sha of ZMX_DIR is NOT the contract — the zmx CLI
# writes a benign $ZMX_DIR/logs/zmx.log during boot-prep even when the agent check
# rejects; that log is NOT a session claim. We assert the real contract (registry
# entry count + socket count) and SEPARATELY report the log observation.
fresh_jail
SESS_DIR="$HOME/.claude/sessions"
reg_before="$(dir_sha "$SESS_DIR")"
# Use a jail-prefixed name (L10 discipline) + an agent that cannot resolve.
NEWNAME="${JAIL_PREFIX}newtest"
run_sb new "$NEWNAME" --agent nonexistent-agent-xyz
rc_new=$RC
reg_after="$(dir_sha "$SESS_DIR")"
# NEW registry json files mentioning our name (a claimed session would appear here).
new_entries="$(grep -rl "$NEWNAME" "$SESS_DIR" 2>/dev/null | wc -l | tr -d ' ')"
zmx_sockets="$(find "$ZMX_DIR" -type s 2>/dev/null | wc -l | tr -d ' ')"
if [ "$rc_new" != "0" ] && [ "$reg_before" = "$reg_after" ] && [ "$new_entries" = "0" ] && [ "$zmx_sockets" = "0" ]; then
    pass SA-new-failclosed "exit=$rc_new (nonzero); registry UNCHANGED; 0 new entries; 0 zmx sockets — fail-closed (A2-wired)"
else
    fail SA-new-failclosed "exit=$rc_new reg($reg_before vs $reg_after) new_entries=$new_entries zmx_sockets=$zmx_sockets"
fi
# Observation (not a contract violation): does the rejected boot leave a stray
# zmx artifact? Reported so the lead can decide if fail-closed should be cleaner.
zmx_logs="$(find "$ZMX_DIR" -type f 2>/dev/null | wc -l | tr -d ' ')"
echo "      OBSERVE SA-new-failclosed: zmx non-socket files left after rejected boot = $zmx_logs (benign zmx.log; not a session claim)."
echo "      NOTE: full live-boot state assertion (successful new creates registry+zmx) was gated in A2's live-jail gate and CARRIES — no live claude in this jail."
jail_teardown

# ---- attach: unresolvable session → exit 1, no state change -----------------
fresh_jail
asha_before="$(dir_sha "$HOME")"
run_sb attach nosuch-session-xyz
rc_attach=$RC
asha_after="$(dir_sha "$HOME")"
if [ "$rc_attach" = "1" ] && [ "$asha_before" = "$asha_after" ]; then
    pass SA-attach-fail "unresolvable session exit=1, HOME unchanged"
else
    fail SA-attach-fail "exit=$rc_attach sha($asha_before vs $asha_after)"
fi
jail_teardown

# ---- send / relay moved-stubs: exit 1, no state change ----------------------
fresh_jail
ssha_before="$(dir_sha "$HOME")"
run_sb send; rc_send=$RC
run_sb relay; rc_relay=$RC
ssha_after="$(dir_sha "$HOME")"
if [ "$rc_send" = "1" ] && [ "$rc_relay" = "1" ] && [ "$ssha_before" = "$ssha_after" ]; then
    pass SA-moved-stubs "send(exit1)+relay(exit1) moved-stubs, HOME unchanged"
else
    fail SA-moved-stubs "rc_send=$rc_send rc_relay=$rc_relay sha($ssha_before vs $ssha_after)"
fi
jail_teardown

# ---- REAL verbs: no-mutation-on-error / idempotent (A5 reconciliation) -------
# A3 LINEAGE: these rows were "STUB verbs: honest-stub stderr + NO mutation".
# resume/kill/reconcile/gc/bootstrap/update + ping are REAL as of A5 (was A5-stub);
# send:*/wait became real in A4. Original intent PRESERVED: the verb must not
# silently mutate jail state on an error or a no-op. Updated target: real exit
# codes + real output shapes at pin 0d0fa9e (no more "not yet implemented" line).

# realverb_no_mutation <id> <expected-rc> -- <verb args...> : run a real verb that
# should ERROR cleanly (unresolvable target / no install channel) and assert the
# expected nonzero exit AND that HOME is byte-for-byte unchanged (no-mutation-on-
# error — the surviving A3 intent).
realverb_no_mutation() {
    local id="$1" exp_rc="$2"; shift 2
    fresh_jail
    local before after
    before="$(dir_sha "$HOME")"
    run_sb "$@"
    after="$(dir_sha "$HOME")"
    local rc_ok=0 nomut=0
    [ "$RC" = "$exp_rc" ] && rc_ok=1
    [ "$before" = "$after" ] && nomut=1
    if [ "$rc_ok" = "1" ] && [ "$nomut" = "1" ]; then
        pass "$id" "real verb errors clean exit=$RC, NO mutation"
    else
        fail "$id" "rc_ok=$rc_ok(exp=$exp_rc got=$RC) nomut=$nomut ($(head -1 "$ERR_FILE"))"
    fi
    jail_teardown
}
# MERGE UNION (A5 merge, 2026-06-05): main's A3-pass-(b) reconciliation promoted
# send:*/wait and kept A5 verbs as stubs; A5's reconciliation promoted its own
# verbs. Union = main's STRONGER helper (asserts the resolver message too) used
# for every resolver-error path; A5's verb reality (no stub rows remain).
real_fail_no_mutation() {
    local id="$1"; shift 1
    fresh_jail
    local before after
    before="$(dir_sha "$HOME")"
    run_sb "$@"
    after="$(dir_sha "$HOME")"
    local rc_ok=0 msg_ok=0 nomut=0
    [ "$RC" = "1" ] && rc_ok=1
    grep -q 'No session matching "somesess"' "$ERR_FILE" && msg_ok=1
    [ "$before" = "$after" ] && nomut=1
    if [ "$rc_ok" = "1" ] && [ "$msg_ok" = "1" ] && [ "$nomut" = "1" ]; then
        pass "$id" "REAL verb unresolvable-session: resolver error, exit1, NO mutation"
    else
        fail "$id" "rc_ok=$rc_ok msg_ok=$msg_ok nomut=$nomut rc=$RC ($(head -1 "$ERR_FILE"))"
    fi
    jail_teardown
}
# resume/kill of an unresolvable session: resolver error, exit 1, no mutation
# (real verb, fails BEFORE any destructive sub-step — the A3 intent).
real_fail_no_mutation SA-resume-noexist  resume somesess
real_fail_no_mutation SA-kill-noexist    kill somesess
# A4-real send:*/wait against an unresolvable session (main's rows, kept).
real_fail_no_mutation SA-real-sendpty-fail   send:pty somesess hi
real_fail_no_mutation SA-real-sendrelay-fail send:relay somesess hi
real_fail_no_mutation SA-real-sendhttp-fail  send:http somesess hi
real_fail_no_mutation SA-real-wait-fail      wait somesess
# update with no determinable install channel (jail HOME has no Homebrew/cargo
# ancestry for the test bin): exit 1 guidance, no mutation (no resolver msg —
# the rc-only helper applies).
realverb_no_mutation SA-update-nochan   1 update

# reconcile --dry-run: the fresh_jail seeds 3 forged sessions (PIDs 99001-99003)
# whose PIDs are DEAD, so a real reconcile classifies them as I1 drift and WOULD
# tombstone them. --dry-run PLANS the repair ("Would repair N drift item(s):")
# but mutates NOTHING — the faithful mapping of the A3 "stub never mutated" intent
# onto the real verb, while still exercising the live I1 dead-PID classifier.
# (A non-dry reconcile here genuinely repairs 3 items — verified — so it is NOT a
# no-mutation row; the destructive path is covered live/jailed in a5_lifecycle_live.)
fresh_jail
before="$(dir_sha "$HOME")"
run_sb reconcile --dry-run
after="$(dir_sha "$HOME")"
if [ "$RC" = "0" ] && grep -q "Would repair" "$OUT_FILE" && [ "$before" = "$after" ]; then
    pass SA-reconcile-dryrun "reconcile --dry-run plans I1 repair (dead-PID drift) exit=0, NO mutation"
else
    fail SA-reconcile-dryrun "rc=$RC nomut=$( [ "$before" = "$after" ] && echo 1 || echo 0 ) ($(head -1 "$OUT_FILE"))"
fi
jail_teardown

# gc --dry-run on a clean jail: prints the scan, mutates NOTHING (the dry-run
# guarantees the original no-mutation intent for the destructive verb).
fresh_jail
before="$(dir_sha "$HOME")"
run_sb gc --dry-run
after="$(dir_sha "$HOME")"
if [ "$RC" = "0" ] && grep -q "GC candidates" "$OUT_FILE" && [ "$before" = "$after" ]; then
    pass SA-gc-dryrun "gc --dry-run exit=0 scanned, NO mutation"
else
    fail SA-gc-dryrun "rc=$RC nomut=$( [ "$before" = "$after" ] && echo 1 || echo 0 ) ($(head -1 "$OUT_FILE"))"
fi
jail_teardown

# bootstrap: REAL verb creates ~/.quorum/dispatch state dirs on first run, so "no mutation" is
# NOT its contract. The A3 intent (the verb does the right, bounded thing) maps to
# IDEMPOTENCE: run twice in a fresh jail, the SECOND run's HOME-sha must equal the
# first run's (a no-op apart from re-checks), both exit 0. This is the same
# property G-B1 asserts; here it closes the reconciled stub row.
fresh_jail
run_sb bootstrap; rc_b1=$RC
sha_after1="$(dir_sha "$HOME")"
run_sb bootstrap; rc_b2=$RC
sha_after2="$(dir_sha "$HOME")"
if [ "$rc_b1" = "0" ] && [ "$rc_b2" = "0" ] && [ "$sha_after1" = "$sha_after2" ]; then
    pass SA-bootstrap-idem "bootstrap idempotent: exit 0 ×2, HOME stable across re-run"
else
    fail SA-bootstrap-idem "rc1=$rc_b1 rc2=$rc_b2 sha($sha_after1 vs $sha_after2)"
fi
jail_teardown

# ping <session>: REAL classifier (was A5 stub). The forged alpha-worker entry is
# idle/turns=0 with a far-past updatedAt → classify_health yields the ambiguous
# band (exit 4) or done (exit 0) depending on uptime; either way it is a real
# classification exit, NOT the old stub-1, and it mutates NOTHING (read-only verb).
fresh_jail
before="$(dir_sha "$HOME")"
run_sb ping alpha-worker
after="$(dir_sha "$HOME")"
# Frozen ping band: 0 (done) / 1 (stuck) / 2 (active) / 4 (ambiguous). Any of these
# is a valid REAL classification; the old stub returned 1 with the impl-stub line.
ping_real=0
case "$RC" in 0|1|2|4) ping_real=1 ;; esac
if grep -q "not yet implemented" "$ERR_FILE" 2>/dev/null; then ping_real=0; fi
if [ "$ping_real" = "1" ] && [ "$before" = "$after" ]; then
    pass SA-ping-real "ping <session> REAL classifier exit=$RC (frozen band), NO mutation"
else
    fail SA-ping-real "rc=$RC real=$ping_real nomut=$( [ "$before" = "$after" ] && echo 1 || echo 0 )"
fi
jail_teardown

echo
echo "========= STATE-ASSERTION SUMMARY ========="
printf 'PASS=%d  FAIL=%d\n' "$PASS" "$FAIL"
if [ "$FAIL" -ne 0 ]; then printf 'FAILED:%s\n' "$FAILED"; exit 1; fi
echo "ALL STATE ASSERTIONS GREEN"
exit 0
