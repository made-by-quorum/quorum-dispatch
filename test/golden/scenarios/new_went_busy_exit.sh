#!/usr/bin/env bash
# test/golden/scenarios/new_went_busy_exit.sh
#
# A4 M4b — Level 2 went-busy EXIT-CONTRACT golden scenario (a4-spec §5 "Level 2",
# §3.5; ADR 0008; doc/PROTOCOL.md "sb new exit contract").
#
# Drives `sb new -p` through REAL zmx with the `fakerepl` binary standing in for
# claude (CLAUDE_BIN substitution, launch.rs:23-27), IN-JAIL (ADD-4), and asserts
# the three-way exit contract end-to-end on the bin layer:
#
#   ACCEPT    `sb new <n> -p "<msg>"`  (fakerepl defaults: prompt submits, goes
#             busy)                                   -> exit 0  + stdout
#             `Prompt delivered to "<n>"`.  Run TWICE (determinism).
#   STALL     same, SB_FAKEREPL_ABSORB_ALL_CRS=1 (every CR absorbed; the
#             remediation CR can never submit)        -> exit 10 + stderr WARNING
#             block; the session STILL EXISTS (turn-start unconfirmed, not a
#             create failure).  Run TWICE (determinism).
#   NO-PROMPT `sb new <n>` (no -p)                    -> exit 0 (10 is unreachable
#             without -p; delivery never runs).
#   REFUSAL   the fakerepl binary run directly out-of-jail (clean env, AND a
#             partial-spoof env: HOME jail-shaped but SB_HOME elsewhere) -> exit
#             13.  Re-proves the jail belt (a4-spec §5 R3 coherence check) at the
#             scenario layer.
#
# HOW THE JAIL ENV REACHES fakerepl (the mechanism, verified empirically):
# `jail_establish` EXPORTS HOME/SB_HOME/ZMX_DIR/TMPDIR (jail.sh:139-146). `sb new`
# spawns `zmx run <n> -d bash -lc "command '<CLAUDE_BIN>' <flags...> --name <n>"`
# via RealExec (exec.rs:109-113) which INHERITS the parent env, overriding only
# ZMX_DIR (to the canonical socket dir, which under the jail IS $JAIL_ROOT/zmx).
# zmx + `bash -lc` inherit the rest, so fakerepl sees the coherent exported
# isolation set and its belt (a4-spec §5) passes. zmx does NOT scrub the set —
# confirmed by booting through it (no refusal at boot). CLAUDE_BIN is set AFTER
# jail_establish to a COPY of fakerepl placed UNDER $JAIL_ROOT, so it satisfies
# the jail's own CLAUDE_BIN belt (jail.sh:270-288: a set CLAUDE_BIN must resolve
# under JAIL_ROOT). The default claude flags are harmless: fakerepl reads only
# --name and silently ignores the rest (main.rs Config::from_env).
#
# OUTPUT: a PASS/FAIL line per row + a SUMMARY line (PASS=n FAIL=n), matching the
# sibling scenarios (a3_state_assertions.sh). Bash 3.2 floor (no associative
# arrays, no ${var,,}, no mapfile). Builds via scripts/build-lock.sh (B2).
#
# Hermetic: everything inside one per-run jail; sessions asserted/torn down via
# the jail's own zmx primitives (NEVER `sb ls`, which scans the legacy /tmp + XDG
# dirs and would surface the host's REAL org sessions — a noisy, non-hermetic
# read; the jailed `zmx list` pinned to ZMX_DIR sees ONLY our sessions). Teardown
# ALWAYS via an EXIT/INT/TERM trap; jail_teardown is idempotent.
# ---------------------------------------------------------------------------
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
RUST_BIN="${A4_RUST_BIN:-$REPO_ROOT/target/debug/sb}"
FAKEREPL_BIN="${A4_FAKEREPL_BIN:-$REPO_ROOT/target/debug/fakerepl}"
ZMX_CMD="${JAIL_ZMX_CMD:-zmx}"

export JAIL_SB_CMD="$RUST_BIN"
export JAIL_ZMX_CMD="$ZMX_CMD"
# Capture the REAL home BEFORE the jail rewrites it (the jail's production-path
# belt keys on this; sibling a3_state_assertions.sh:32-34 does the same).
export JAIL_REAL_HOME="$HOME"

. "$REPO_ROOT/test/golden/lib/jail.sh"

PASS=0; FAIL=0; FAILED=""
pass() { PASS=$((PASS+1)); printf '  [PASS] %-22s %s\n' "$1" "$2"; }
fail() { FAIL=$((FAIL+1)); FAILED="$FAILED $1"; printf '  [FAIL] %-22s %s\n' "$1" "$2"; }

# A jail-prefixed, unique session name: <JAIL_PREFIX><tag><n>.
scn_name() { printf '%s%s%s' "${JAIL_PREFIX:?jail not established}" "$1" "$2"; }

# --- Build the two binaries ONCE, through the build lock (B2). ---------------
if [ "${A4_SKIP_BUILD:-0}" != "1" ]; then
    "$REPO_ROOT/scripts/build-lock.sh" cargo build -p fakerepl -p sb >/dev/null 2>&1 \
        || { echo "FATAL: build failed (cargo build -p fakerepl -p sb)" >&2; exit 3; }
fi
[ -x "$RUST_BIN" ]     || { echo "FATAL: sb binary missing: $RUST_BIN" >&2; exit 3; }
[ -x "$FAKEREPL_BIN" ] || { echo "FATAL: fakerepl binary missing: $FAKEREPL_BIN" >&2; exit 3; }
command -v "$JAIL_ZMX_CMD" >/dev/null 2>&1 \
    || { echo "FATAL: zmx not found on PATH (JAIL_ZMX_CMD=$JAIL_ZMX_CMD)" >&2; exit 3; }

# --- Establish ONE jail for the whole scenario. Fail closed. -----------------
# SHORT explicit runid (wart-wave M5 finding): the default pid+epoch+RANDOM
# runid (~20 chars) under a macOS /var/folders TMPDIR pushes the zmx SOCKET
# path past the unix-socket limit — zmx refuses every session name >19 bytes
# ("session name is too long") and EVERY live row reds with NotAttachable
# (Bug-D shape, but really path length). The A4-era workaround was TMPDIR=/tmp
# (a4 journal lesson); this wave's ADD-14 restatement bans literal-/tmp test
# state, so the compliant fix is a short runid: the jail keeps its hermetic
# TMPDIR and the socket paths fit. `wb` + 5-digit pid ≤ 7 chars → longest name
# here (sbrg-<runid>-w8okchunk1) ≈ 23 bytes, comfortably under the cap.
jail_establish "wb$$" || { echo "FATAL: jail refused to establish" >&2; exit 3; }
# Teardown ALWAYS — even on Ctrl-C / SIGTERM mid-scenario. Idempotent.
trap 'jail_teardown 2>/dev/null || true' EXIT INT TERM

# CLAUDE_BIN must live UNDER JAIL_ROOT to pass the jail's own belt
# (jail.sh:270-288). Copy fakerepl in and point CLAUDE_BIN at the copy.
JAILED_FAKEREPL="$JAIL_ROOT/fakerepl"
cp "$FAKEREPL_BIN" "$JAILED_FAKEREPL" || { echo "FATAL: cannot stage fakerepl in jail" >&2; exit 3; }
chmod +x "$JAILED_FAKEREPL"
export CLAUDE_BIN="$JAILED_FAKEREPL"

WORKDIR="$JAIL_ROOT/tmp/work"
mkdir -p "$WORKDIR"

# How many jailed zmx sessions currently match a name (ZMX_DIR-pinned so it sees
# ONLY our jail's sessions — never the host's real org sessions).
jail_zmx_count() {
    local name="$1"
    ZMX_DIR="$ZMX_DIR" "$JAIL_ZMX_CMD" list 2>/dev/null | grep -c "name=${name}" || true
}

# Kill a jailed session by name (guarded + ZMX_DIR-pinned). Best-effort.
jail_kill() {
    local name="$1"
    jail_guard_name "$name" >/dev/null 2>&1 || return 0
    ZMX_DIR="$ZMX_DIR" "$JAIL_ZMX_CMD" kill "$name" --force >/dev/null 2>&1 \
        || ZMX_DIR="$ZMX_DIR" "$JAIL_ZMX_CMD" kill "$name" >/dev/null 2>&1 || true
}

# run_new <name> <env-prefix> [extra-args...] -- runs `sb new` in the jail with an
# optional single env assignment prefix (e.g. SB_FAKEREPL_ABSORB_ALL_CRS=1), then
# sets globals RC / OUT_FILE / ERR_FILE for the caller to assert on.
RC=0; OUT_FILE=""; ERR_FILE=""
run_new() {
    local name="$1" envk="$2"; shift 2
    OUT_FILE="$JAIL_ROOT/${name}.out"
    ERR_FILE="$JAIL_ROOT/${name}.err"
    # The env prefix is applied to THIS command only (not exported), so the jail's
    # asserted env stays clean. CLAUDE_BIN is already exported (jail-rooted).
    if [ -n "$envk" ]; then
        ( cd "$WORKDIR" && env "$envk" "$JAIL_SB_CMD" new "$name" --cwd "$WORKDIR" "$@" ) \
            > "$OUT_FILE" 2> "$ERR_FILE"
    else
        ( cd "$WORKDIR" && "$JAIL_SB_CMD" new "$name" --cwd "$WORKDIR" "$@" ) \
            > "$OUT_FILE" 2> "$ERR_FILE"
    fi
    RC=$?
}

echo "================ A4 went-busy EXIT-CONTRACT scenario (Level 2) ================"
echo "  jail=$JAIL_ROOT"
echo "  sb=$RUST_BIN"
echo "  fakerepl(jailed)=$CLAUDE_BIN"
echo "  zmx=$JAIL_ZMX_CMD"
echo

# ---------------------------------------------------------------------------
# ROW ACCEPT (run twice). fakerepl defaults: the trailing \r is a lone non-paste
# keystroke burst -> SUBMIT -> goes busy. Floor BUSY_MS at 900ms so the busy
# window is observable above the SUT's 250ms status-poll (fakerepl README W7).
# ---------------------------------------------------------------------------
ACCEPT_MSG="hello from the went-busy accept row"
i=1
while [ "$i" -le 2 ]; do
    name="$(scn_name accept "$i")"
    run_new "$name" "SB_FAKEREPL_BUSY_MS=900" -p "$ACCEPT_MSG"
    rc=$RC
    out="$(cat "$OUT_FILE" 2>/dev/null)"
    echo "  --- ACCEPT run $i (name=$name) ---"
    echo "  exit=$rc"
    printf '  stdout: %s\n' "$out"
    [ -s "$ERR_FILE" ] && { printf '  stderr:\n'; sed 's/^/    /' "$ERR_FILE"; }
    # Assert BOTH the code AND the stdout marker (a bare exit-0 assert is vacuous).
    if [ "$rc" -eq 0 ]; then
        case "$out" in
            *"Prompt delivered to \"$name\""*)
                pass "accept-run$i" "exit 0 + 'Prompt delivered to' marker" ;;
            *)
                fail "accept-run$i" "exit 0 but stdout lacks 'Prompt delivered to \"$name\"'" ;;
        esac
    else
        fail "accept-run$i" "expected exit 0, got $rc"
    fi
    jail_kill "$name"
    i=$((i+1))
done
echo

# ---------------------------------------------------------------------------
# ROW STALL (run twice). SB_FAKEREPL_ABSORB_ALL_CRS=1: EVERY CR is absorbed as a
# literal newline -> the composer never submits -> deliver_prompt exhausts its
# bounded remediation (1 + 3 rounds, ~52s, deterministic) and returns Stalled.
# Assert exit 10 AND the WARNING block AND that the session STILL EXISTS (the
# divergence's whole point: made-but-unconfirmed, not a create failure).
# ---------------------------------------------------------------------------
STALL_MSG="this prompt will never submit"
i=1
while [ "$i" -le 2 ]; do
    name="$(scn_name stall "$i")"
    t0=$(date +%s 2>/dev/null || echo 0)
    run_new "$name" "SB_FAKEREPL_ABSORB_ALL_CRS=1" -p "$STALL_MSG"
    rc=$RC
    t1=$(date +%s 2>/dev/null || echo 0)
    out="$(cat "$OUT_FILE" 2>/dev/null)"
    err="$(cat "$ERR_FILE" 2>/dev/null)"
    alive="$(jail_zmx_count "$name")"
    echo "  --- STALL run $i (name=$name, ~$((t1-t0))s) ---"
    echo "  exit=$rc  session-alive=$alive (expect 1)"
    printf '  stdout: %s\n' "$out"
    printf '  stderr:\n'; sed 's/^/    /' "$ERR_FILE" 2>/dev/null
    # Assert exit 10 AND the WARNING text AND the session survived.
    ok=1; why=""
    [ "$rc" -eq 10 ] || { ok=0; why="${why} exit=$rc(want 10);"; }
    case "$err" in
        *"WARNING: Prompt sent to \"$name\" but session did not go busy."*) ;;
        *) ok=0; why="${why} missing WARNING block;" ;;
    esac
    [ "$alive" -ge 1 ] || { ok=0; why="${why} session not alive after stall;"; }
    if [ "$ok" -eq 1 ]; then
        pass "stall-run$i" "exit 10 + WARNING block + session survives"
    else
        fail "stall-run$i" "$why"
    fi
    jail_kill "$name"
    i=$((i+1))
done
echo

# ---------------------------------------------------------------------------
# ROW NO-PROMPT. `sb new` without -p: delivery never runs, so 10 is unreachable.
# Just the create path -> exit 0.
# ---------------------------------------------------------------------------
name="$(scn_name noprompt 1)"
run_new "$name" ""
rc=$RC
out="$(cat "$OUT_FILE" 2>/dev/null)"
echo "  --- NO-PROMPT (name=$name) ---"
echo "  exit=$rc"
printf '  stdout: %s\n' "$out"
[ -s "$ERR_FILE" ] && { printf '  stderr:\n'; sed 's/^/    /' "$ERR_FILE"; }
if [ "$rc" -eq 0 ]; then
    case "$out" in
        *"Started detached session \"$name\""*)
            pass "no-prompt" "exit 0 (10 unreachable without -p)" ;;
        *)
            fail "no-prompt" "exit 0 but stdout lacks 'Started detached session'" ;;
    esac
else
    fail "no-prompt" "expected exit 0, got $rc"
fi
jail_kill "$name"
echo

# ---------------------------------------------------------------------------
# ROW REFUSAL (out-of-jail). Re-prove the fakerepl jail belt at the scenario
# layer (a4-spec §5 R3): the binary refuses (exit 13) when run directly with
# (a) a CLEAN env, and (b) a PARTIAL-SPOOF env (HOME jail-shaped but SB_HOME
# pointing elsewhere). (b) proves the belt checks coherence, not just HOME.
# `env -i` gives a clean slate; we then add exactly the spoof vars.
# ---------------------------------------------------------------------------
r1err="$JAIL_ROOT/refuse-clean.err"
env -i "$FAKEREPL_BIN" </dev/null >/dev/null 2>"$r1err"
rc1=$?
echo "  --- REFUSAL clean env ---"
echo "  exit=$rc1 (expect 13)"; sed 's/^/    /' "$r1err" 2>/dev/null
if [ "$rc1" -eq 13 ]; then
    pass "refuse-clean" "exit 13 (belt refuses a clean env)"
else
    fail "refuse-clean" "expected exit 13, got $rc1"
fi

r2err="$JAIL_ROOT/refuse-spoof.err"
# HOME jail-shaped (passes marker (a)) but SB_HOME elsewhere (fails coherence (b)).
env -i \
    HOME="$JAIL_ROOT/home" \
    SB_HOME="/tmp/not-the-jail-sb-home" \
    ZMX_DIR="$JAIL_ROOT/zmx" \
    TMPDIR="$JAIL_ROOT/tmp" \
    "$FAKEREPL_BIN" </dev/null >/dev/null 2>"$r2err"
rc2=$?
echo "  --- REFUSAL partial-spoof (HOME jail-shaped, SB_HOME elsewhere) ---"
echo "  exit=$rc2 (expect 13)"; sed 's/^/    /' "$r2err" 2>/dev/null
ok=1; why=""
[ "$rc2" -eq 13 ] || { ok=0; why="${why} exit=$rc2(want 13);"; }
# It must refuse on the COHERENCE check (SB_HOME), not merely on HOME shape —
# otherwise the control would be a no-op (proves the belt checks coherence).
if ! grep -q "SB_HOME" "$r2err" 2>/dev/null; then
    ok=0; why="${why} refusal did not cite SB_HOME (coherence check not exercised);"
fi
if [ "$ok" -eq 1 ]; then
    pass "refuse-spoof" "exit 13 on SB_HOME coherence (not just HOME shape)"
else
    fail "refuse-spoof" "$why"
fi
echo

# ---------------------------------------------------------------------------
# ROW W8-TRUNC + ROW W8-ACCEPT-CHUNKED (wart-wave, ADD-15 W8 / M11 / ADR-0012).
#
# The verify-after-submit CALL-SITE wiring teeth (spec red-team R5): the pure
# helper's unit/gate rows cannot prove the BINARY calls it — these two legs do.
# Removing the lifecycle.rs verify call-site turns W8-TRUNC exit-1 into exit-0
# -> FAIL here (CI-resident anti-regression).
#
# Mechanism: fakerepl carries SB_FAKEREPL_SESSION_ID (registry row gains
# sessionId) + SB_FAKEREPL_CONVO_JSONL placed at the projects-dir path that id
# resolves to, so the SUT's registry->sessionId->find_jsonl_path verify chain
# works end-to-end. TRUNC leg arms the reader-stall saturation seam (the D16
# window model): the chunked payload mid-truncates, fakerepl records the
# truncated composer as the user record, the SUT's read-back catches it ->
# exit 1 + the named error, AFTER went-busy (ADR-0008 codes untouched: 1 is the
# existing failure class). ACCEPT-CHUNKED leg = same payload, no stall -> the
# read-back verifies -> exit 0 (the verified-success control: verification must
# not punish an intact chunked delivery).
#
# Payload timing (deterministic per fakerepl's no-RNG model): 8KB -> 8 chunks at
# 150ms pacing (~1.05s of writes). STALL_AFTER_BYTES=1024 arms the pause at the
# first chunk; STALL_MS=800 ends it (~t=800ms) BEFORE the trailing CR
# (~t=1.25s), so the submit CR is admitted and the turn STARTS (went-busy) with
# a truncated composer — exactly the D16 silent-loss shape.
# ---------------------------------------------------------------------------
_w8_build_payload() {
    local i=0
    while [ "$i" -lt 256 ]; do
        printf 'w8-payload-word-xyzw-%03d-pad8 ' "$i"   # 29 chars + space = 30B
        i=$((i+1))
    done
}
W8_MSG="$(_w8_build_payload)"   # 7680 bytes -> 8 chunks (multi-chunk => verify in scope)

# run_new with the W8 env set. $1=name $2=sid $3=convo $4=stall(0|1).
run_new_w8() {
    local name="$1" sid="$2" convo="$3" stall="$4"
    OUT_FILE="$JAIL_ROOT/${name}.out"
    ERR_FILE="$JAIL_ROOT/${name}.err"
    if [ "$stall" = "1" ]; then
        ( cd "$WORKDIR" && env \
            SB_FAKEREPL_BUSY_MS=900 \
            SB_FAKEREPL_SESSION_ID="$sid" \
            SB_FAKEREPL_CONVO_JSONL="$convo" \
            SB_FAKEREPL_STALL_AFTER_BYTES=1024 \
            SB_FAKEREPL_STALL_MS=800 \
            SB_FAKEREPL_STALL_QUEUE_CAP=1024 \
            "$JAIL_SB_CMD" new "$name" --cwd "$WORKDIR" -p "$W8_MSG" ) \
            > "$OUT_FILE" 2> "$ERR_FILE"
    else
        ( cd "$WORKDIR" && env \
            SB_FAKEREPL_BUSY_MS=900 \
            SB_FAKEREPL_SESSION_ID="$sid" \
            SB_FAKEREPL_CONVO_JSONL="$convo" \
            "$JAIL_SB_CMD" new "$name" --cwd "$WORKDIR" -p "$W8_MSG" ) \
            > "$OUT_FILE" 2> "$ERR_FILE"
    fi
    RC=$?
}

# The projects-dir path the SUT's find_jsonl_path fallback scans (jail HOME).
W8_PROJ="$HOME/.claude/projects/w8-scenario"
mkdir -p "$W8_PROJ"

# --- W8-TRUNC: stall seam armed -> truncated record -> loud exit 1 ----------
name="$(scn_name w8trunc 1)"
sid="w8sid-trunc-$$"
run_new_w8 "$name" "$sid" "$W8_PROJ/$sid.jsonl" 1
rc=$RC
out="$(cat "$OUT_FILE" 2>/dev/null)"
err="$(cat "$ERR_FILE" 2>/dev/null)"
echo "  --- W8-TRUNC (name=$name) ---"
echo "  exit=$rc (expect 1)"
printf '  stdout: %s\n' "$out"
printf '  stderr:\n'; sed 's/^/    /' "$ERR_FILE" 2>/dev/null
ok=1; why=""
[ "$rc" -eq 1 ] || { ok=0; why="${why} exit=$rc(want 1);"; }
case "$err" in
    *"payload truncated in delivery"*) ;;
    *) ok=0; why="${why} missing 'payload truncated in delivery' error;" ;;
esac
case "$err" in
    *"do NOT blindly resend"*) ;;
    *) ok=0; why="${why} missing the no-retry guidance;" ;;
esac
# The truncation fires AFTER went-busy — it must NOT be a Stalled (10) or a
# delivery-success masquerade.
case "$out" in
    *"Prompt delivered"*) ok=0; why="${why} stdout claims delivery despite truncation;" ;;
esac
if [ "$ok" -eq 1 ]; then
    pass "w8-trunc" "exit 1 + named truncation error after went-busy"
else
    fail "w8-trunc" "$why"
fi
jail_kill "$name"
echo

# --- W8-ACCEPT-CHUNKED: same payload, no stall -> Verified -> exit 0 --------
name="$(scn_name w8okchunk 1)"
sid="w8sid-ok-$$"
run_new_w8 "$name" "$sid" "$W8_PROJ/$sid.jsonl" 0
rc=$RC
out="$(cat "$OUT_FILE" 2>/dev/null)"
err="$(cat "$ERR_FILE" 2>/dev/null)"
echo "  --- W8-ACCEPT-CHUNKED (name=$name) ---"
echo "  exit=$rc (expect 0)"
printf '  stdout: %s\n' "$out"
[ -s "$ERR_FILE" ] && { printf '  stderr:\n'; sed 's/^/    /' "$ERR_FILE"; }
ok=1; why=""
[ "$rc" -eq 0 ] || { ok=0; why="${why} exit=$rc(want 0);"; }
case "$out" in
    *"Prompt delivered to \"$name\""*) ;;
    *) ok=0; why="${why} missing 'Prompt delivered' marker;" ;;
esac
case "$err" in
    *"payload truncated"*) ok=0; why="${why} false truncation on an intact chunked delivery;" ;;
esac
if [ "$ok" -eq 1 ]; then
    pass "w8-accept-chunked" "exit 0 verified (intact chunked delivery not punished)"
else
    fail "w8-accept-chunked" "$why"
fi
jail_kill "$name"
echo

# ---------------------------------------------------------------------------
# Teardown + leak check, then SUMMARY.
# ---------------------------------------------------------------------------
jail_teardown
# After teardown the jail's zmx dir is removed; nothing of ours can remain.
trap - EXIT INT TERM

echo "================ A4 went-busy EXIT-CONTRACT SUMMARY ================"
printf 'PASS=%d FAIL=%d\n' "$PASS" "$FAIL"
if [ "$FAIL" -ne 0 ]; then
    printf 'FAILED ROWS:%s\n' "$FAILED"
    exit 1
fi
echo "ALL EXIT-CONTRACT ROWS GREEN"
exit 0
