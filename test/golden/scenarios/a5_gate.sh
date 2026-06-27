#!/usr/bin/env bash
# test/golden/scenarios/a5_gate.sh — A5/M6 GATE DRIVER (qa-a5, fresh agent).
#
# ONE driver that executes the spec §7 gate matrix rows IN ORDER and prints a
# per-row PASS/FAIL line plus raw excerpts the lead pastes into the house report
# (a4-gate-report.md format). Rows already covered by an existing scenario are
# INVOKED, never reimplemented:
#   - a5_lifecycle_live.sh      G-L1..G-L6 (+ G-L6c sweep-belt bite) live-jail
#   - bootstrap_output_audit.sh G-B1/G-B3/G-B4/G-B5/G-N2
#   - a3_state_assertions.sh    A5-reconciled state assertions (G-R1)
#   - run_selftests.sh          normalizers/jail/secret-scan/record-gate/fetch-zmx (G-R1)
#   - a5rec_*.sh via verify.sh   G-REC corpus round-trip
#
# This driver ADDS the rows that have no standalone scenario:
#   G-C1 file-backend round-trip · G-C3 locked-keychain fallback shim ·
#   G-C4 env-override precedence (unit) · G-C6 N1 config-get-no-key=exit2 ·
#   G-U1/G-P1/G-S1/G-S2 unit invocations · the TWO new relay rows (b)/(d) ·
#   fmt --check (IN this script) · the G-N1 teeth roll-up (pointers; the live
#   teeth injections are run by the QA agent out-of-band and journaled).
#
# ALL cargo goes through scripts/build-lock.sh (ADD-11c; serial — single lane).
# Bash 3.2 floor (macOS): no assoc arrays, no ${var,,}, no mapfile.
#
# Usage:  bash test/golden/scenarios/a5_gate.sh
# Env:    SB_BIN (qd-under-test; default target/debug/qd)
#         A5_GATE_SKIP_LIVE=1  skip the live-jail + selftest rows (fast unit pass)
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
cd "$REPO_ROOT" || { echo "FATAL: cannot cd to repo root"; exit 1; }

SB_BIN="${SB_BIN:-$REPO_ROOT/target/debug/qd}"
BUILD_LOCK="$REPO_ROOT/scripts/build-lock.sh"
export SB_BIN

PASS=0; FAIL=0; FAILED=""
row_pass() { PASS=$((PASS+1)); printf '  [PASS] %-10s %s\n' "$1" "$2"; }
row_fail() { FAIL=$((FAIL+1)); FAILED="$FAILED $1"; printf '  [FAIL] %-10s %s\n' "$1" "$2"; }
hdr() { printf '\n========== %s ==========\n' "$1"; }

# ---------------------------------------------------------------------------
# Build once (serial via build-lock).
# ---------------------------------------------------------------------------
hdr "BUILD (build-lock cargo build -p qd --bin qd)"
"$BUILD_LOCK" cargo build -p qd --bin qd 2>&1 | tail -2
[ -x "$SB_BIN" ] || { echo "FATAL: qd binary missing: $SB_BIN"; exit 2; }

# ---------------------------------------------------------------------------
# fmt --check IN the script (spec §7 mechanical requirement).
# ---------------------------------------------------------------------------
hdr "fmt --check (IN-SCRIPT)"
if "$BUILD_LOCK" cargo fmt --check >/dev/null 2>&1; then
    row_pass "FMT" "cargo fmt --check clean (exit 0)"
else
    row_fail "FMT" "cargo fmt --check reported diffs"
fi

# ---------------------------------------------------------------------------
# Helper: run the qd-under-test with a scratch HOME/SB_HOME, no jail (these rows
# are HOME-bounded, never destructive, never session-targeting).
# ---------------------------------------------------------------------------
scratch() {
    local T; T="$(mktemp -d)"
    GATE_T="$T"
    GATE_HOME="$T"; GATE_SBH="$T/qd"
}
scratch_done() { rm -rf "$GATE_T"; }

# ===========================================================================
# G-C1 — headless config-set round-trip, file backend (set→get masked→reveal→
#        unset; chmod 600).
# ===========================================================================
hdr "G-C1 config file-backend round-trip + chmod 600"
scratch
HOME="$GATE_HOME" SB_HOME="$GATE_SBH" SB_SECRET_BACKEND=file "$SB_BIN" \
    config set openrouter-key sk-gate-ABCD1234 >/dev/null 2>&1
set_rc=$?
masked="$(HOME="$GATE_HOME" SB_HOME="$GATE_SBH" SB_SECRET_BACKEND=file "$SB_BIN" config get openrouter-key 2>&1)"
reveal="$(HOME="$GATE_HOME" SB_HOME="$GATE_SBH" SB_SECRET_BACKEND=file "$SB_BIN" config get openrouter-key --reveal 2>&1)"
perms="$(ls -l "$GATE_SBH/config.toml" 2>/dev/null | cut -c1-10)"
HOME="$GATE_HOME" SB_HOME="$GATE_SBH" SB_SECRET_BACKEND=file "$SB_BIN" config unset openrouter-key >/dev/null 2>&1
afterunset="$(HOME="$GATE_HOME" SB_HOME="$GATE_SBH" SB_SECRET_BACKEND=file "$SB_BIN" config get openrouter-key 2>&1)"
printf '    set rc=%s | masked=[%s] | reveal=[%s] | perms=[%s] | after-unset=[%s]\n' \
    "$set_rc" "$masked" "$reveal" "$perms" "$afterunset"
if [ "$set_rc" = "0" ] \
   && [ "$masked" = "openrouter-key: ••••1234" ] \
   && [ "$reveal" = "openrouter-key: sk-gate-ABCD1234" ] \
   && [ "$perms" = "-rw-------" ] \
   && [ "$afterunset" = "openrouter-key: not set." ]; then
    row_pass "G-C1" "round-trip + masking + chmod 600 + unset all byte-correct"
else
    row_fail "G-C1" "see excerpt above"
fi
scratch_done

# ===========================================================================
# G-C3 — locked-keychain fallback LIVE-shape: PATH-shim fake `security` emitting
#        the no-interaction signature (+ exit 36). Selected→fallback+notice+
#        truthful (backend: file); env-forced keychain→loud exit 1, no file.
# ===========================================================================
hdr "G-C3 locked-keychain fallback (PATH-shim security)"
scratch
SHIM="$GATE_T/bin"; mkdir -p "$SHIM"
printf '#!/usr/bin/env bash\necho "User interaction is not allowed." >&2\nexit 36\n' > "$SHIM/security"
chmod +x "$SHIM/security"
# (a) keychain SELECTED (NOT env-forced) + locked → fallback to file.
a_out="$(PATH="$SHIM:$PATH" HOME="$GATE_HOME" SB_HOME="$GATE_SBH" "$SB_BIN" config set openrouter-key sk-fb-9999 2>&1)"
a_perms="$(ls -l "$GATE_SBH/config.toml" 2>/dev/null | cut -c1-10)"
a_get="$(PATH="$SHIM:$PATH" HOME="$GATE_HOME" SB_HOME="$GATE_SBH" "$SB_BIN" config get openrouter-key --reveal 2>&1 | tail -1)"
printf '    (a) selected+locked set-out:\n%s\n' "$(printf '%s' "$a_out" | sed 's/^/        /')"
printf '    (a) perms=[%s] get-reveal-tail=[%s]\n' "$a_perms" "$a_get"
if printf '%s' "$a_out" | grep -q "keychain locked (headless?)" \
   && printf '%s' "$a_out" | grep -q "(backend: file)" \
   && [ "$a_perms" = "-rw-------" ] \
   && [ "$a_get" = "openrouter-key: sk-fb-9999" ]; then
    row_pass "G-C3a" "selected+locked → fallback notice + (backend: file) + chmod 600 + value round-trips"
else
    row_fail "G-C3a" "see excerpt above"
fi
# (b) env-forced keychain + locked → loud exit 1, NO file written.
scratch
SHIM="$GATE_T/bin"; mkdir -p "$SHIM"
printf '#!/usr/bin/env bash\necho "User interaction is not allowed." >&2\nexit 36\n' > "$SHIM/security"
chmod +x "$SHIM/security"
b_out="$(PATH="$SHIM:$PATH" HOME="$GATE_HOME" SB_HOME="$GATE_SBH" SB_SECRET_BACKEND=keychain "$SB_BIN" config set openrouter-key sk-nope 2>&1)"
b_rc=$?
printf '    (b) env-forced+locked set-out:\n%s\n' "$(printf '%s' "$b_out" | sed 's/^/        /')"
printf '    (b) rc=%s config.toml-exists=%s\n' "$b_rc" "$( [ -f "$GATE_SBH/config.toml" ] && echo YES || echo no )"
if [ "$b_rc" = "1" ] \
   && printf '%s' "$b_out" | grep -q "forbids file fallback" \
   && [ ! -f "$GATE_SBH/config.toml" ]; then
    row_pass "G-C3b" "env-forced keychain + locked → loud exit 1, NO fallback file"
else
    row_fail "G-C3b" "see excerpt above"
fi
scratch_done

# ===========================================================================
# G-C6 — N1 resolution: `config get` (no key) → exit 2 byte-stderr (A3's exit-0
#        observation is STALE at pin 0d0fa9e).
# ===========================================================================
hdr "G-C6 N1: config get (no key) = exit 2"
scratch
c6_err="$(HOME="$GATE_HOME" SB_HOME="$GATE_SBH" "$SB_BIN" config get 2>&1 1>/dev/null)"
HOME="$GATE_HOME" SB_HOME="$GATE_SBH" "$SB_BIN" config get >/dev/null 2>&1; c6_rc=$?
printf '    stderr=[%s] rc=%s\n' "$c6_err" "$c6_rc"
if [ "$c6_rc" = "2" ] && [ "$c6_err" = "qd config get: a key is required." ]; then
    row_pass "G-C6" "config get no-key → stderr byte-exact + exit 2 (N1 closed; A3 exit-0 stale)"
else
    row_fail "G-C6" "see excerpt above"
fi
scratch_done

# ===========================================================================
# Unit-driven rows (build-lock cargo test, per-module): G-C2, G-C4, G-U1, G-P1,
# G-S1, G-S2 + the secrets/reconcile/gc/kill/resume/bootstrap suites.
# ===========================================================================
hdr "Unit suites (build-lock cargo test)"
unit_row() {
    local id="$1" filt="$2" min="$3"
    local out; out="$("$BUILD_LOCK" cargo test -p qd "$filt" 2>&1 | grep -E 'test result: ok\.' | head -1)"
    local n; n="$(printf '%s' "$out" | sed -n 's/.*ok\. \([0-9]*\) passed.*/\1/p')"
    printf '    %-8s %s\n' "$id" "$out"
    if printf '%s' "$out" | grep -q '0 failed' && [ "${n:-0}" -ge "$min" ]; then
        row_pass "$id" "$n passed, 0 failed (>= $min expected)"
    else
        row_fail "$id" "unit suite short or red: $out"
    fi
}
unit_row "G-C2/4"  "secrets::"   30    # TOML round-trip + env-override precedence + backend select + masking
unit_row "G-U1"    "update::"     8    # channel detection every branch + argv
unit_row "G-P1u"   "ping::"      30    # classifier branches + boundaries + sweep aggregate
unit_row "G-S1/2"  "survey::"    20    # fan-out allSettled + format + key-missing + curl argv hygiene
unit_row "RECON"   "reconcile::"  9    # I1/I3/I5 decider + I5 negative control
unit_row "GCu"     "gc::"        12    # candidate scan + trash + recover + purge deciders

# G-S2 teeth: the negative-control test that MUST fail if the secret were an argv
# token. We assert the named test exists and passes (it encodes the would-fail
# assertion). The active teeth (mutate to argv) is journaled by the QA agent.
hdr "G-S2 teeth (argv-hygiene negative control present + green)"
s2="$("$BUILD_LOCK" cargo test -p qd 'survey::tests::negative_control_argv_token_variant_would_fail_the_hygiene_assert' 2>&1 | grep -E 'test result: ok\.' | head -1)"
printf '    %s\n' "$s2"
if printf '%s' "$s2" | grep -q '1 passed; 0 failed'; then
    row_pass "G-S2" "argv-token negative control present + green (would-red if secret tokenized)"
else
    row_fail "G-S2" "negative control missing/red: $s2"
fi

# ===========================================================================
# NEW RELAY ROWS (orc-3 conditions, relay-1780662680745-11).
#   (b) DEFAULT-PATH: bootstrap in-jail WITHOUT SB_RELAY_DISABLE_SCAN → assert a
#       `[bootstrap] relay:` line with one of the three findings (tolerant shape).
#   (d) disable-scan-keeps-sidecar: DISABLE_SCAN=1 + forged healthy sidecar →
#       relay reported PRESENT (the seam mutes ONLY the port-scan).
# ===========================================================================
hdr "NEW relay rows (b) default-path + (d) disable-scan-keeps-sidecar"
export JAIL_SB_CMD="$SB_BIN"
export JAIL_REAL_HOME="$HOME"
. "$REPO_ROOT/test/golden/lib/jail.sh"
if jail_establish "a5gaterelay$$" >/dev/null 2>&1; then
    # (b) default-path — scan ENABLED. Tolerant: assert the line SHAPE, not finding.
    unset SB_RELAY_DISABLE_SCAN
    b_out="$(jail_sb bootstrap </dev/null 2>&1)"
    b_relay="$(printf '%s' "$b_out" | grep '^\[bootstrap\] relay:')"
    printf '    (b) relay line: %s\n' "$b_relay"
    if printf '%s' "$b_relay" | grep -qE '^\[bootstrap\] relay: (present \(healthy\)\.|present \(unhealthy\)\.|absent.*)'; then
        row_pass "NEW-b" "default-path relay: line present with a valid finding (tolerant shape)"
    else
        row_fail "NEW-b" "no valid [bootstrap] relay: finding line: [$b_relay]"
    fi
    # (d) DISABLE_SCAN=1 + forged healthy sidecar → still PRESENT.
    export SB_RELAY_DISABLE_SCAN=1
    RELAY_DIR="$HOME/.claude/relay"; mkdir -p "$RELAY_DIR"
    printf '{"port": %s, "sessionId": "%sseed", "pid": 0, "status": "ok"}\n' \
        "$JAIL_RELAY_PORT" "$JAIL_PREFIX" > "$RELAY_DIR/seeded.json"
    d_out="$(jail_sb bootstrap </dev/null 2>&1)"
    d_relay="$(printf '%s' "$d_out" | grep '^\[bootstrap\] relay:')"
    printf '    (d) relay line: %s\n' "$d_relay"
    if printf '%s' "$d_relay" | grep -qi 'relay: present'; then
        row_pass "NEW-d" "DISABLE_SCAN=1 + forged sidecar → PRESENT (seam mutes only the port-scan)"
    else
        row_fail "NEW-d" "expected present with forged sidecar under DISABLE_SCAN: [$d_relay]"
    fi
    jail_teardown >/dev/null 2>&1
else
    row_fail "NEW-b/d" "jail_establish failed — could not run the new relay rows"
fi

# CRITICAL: sanitize the env the in-script jail (NEW-b/d) leaked into THIS shell.
# jail_establish exported HOME/SB_HOME/ZMX_DIR/XDG_*/TMPDIR/SB_RELAY_* and the
# NEW-d arm exported SB_RELAY_DISABLE_SCAN. The invoked live scenarios each
# establish their OWN jail and MUST start from a clean shell env (a leaked
# ZMX_DIR/HOME points at the now-deleted run dir → "resolution belt FAILED";
# a leaked SB_RELAY_DISABLE_SCAN changes bootstrap's relay finding under G-REC).
# Restore HOME and drop every jail/relay export so each child scenario is hermetic.
[ -n "${JAIL_REAL_HOME:-}" ] && export HOME="$JAIL_REAL_HOME"
unset SB_HOME ZMX_DIR XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME \
      XDG_RUNTIME_DIR TMPDIR SB_RUST_LOCK_DIR SB_RELAY_PORT \
      SB_RELAY_SOCKET_PREFIX SB_RELAY_DISABLE_SCAN \
      JAIL_ROOT JAIL_RUNID JAIL_PREFIX JAIL_RELAY_PORT 2>/dev/null || true

# ===========================================================================
# INVOKED SCENARIOS (not reimplemented). G-B*/G-N2, A3 surface + state, selftests,
# the live-jail G-L rows, and the G-REC corpus round-trip.
#
# Each scenario runs in a CLEAN child bash with only the needed env (SB_BIN /
# JAIL_SB_CMD / SB_UNDER_TEST), so an earlier scenario's jail env never leaks
# into the next — the live G-L rows establish their own jail and fail closed on
# a stale ZMX_DIR.
# ===========================================================================
invoke() {
    local id="$1" desc="$2"; shift 2
    hdr "$id  ($desc)"
    if env -u SB_HOME -u ZMX_DIR -u TMPDIR -u XDG_RUNTIME_DIR \
           -u SB_RELAY_DISABLE_SCAN -u SB_RELAY_PORT -u SB_RELAY_SOCKET_PREFIX \
           -u JAIL_ROOT -u JAIL_PREFIX \
           HOME="${JAIL_REAL_HOME:-$HOME}" SB_BIN="$SB_BIN" \
           JAIL_SB_CMD="$SB_BIN" SB_UNDER_TEST="$SB_BIN" \
           "$@"; then
        row_pass "$id" "$desc — scenario exit 0"
    else
        row_fail "$id" "$desc — scenario exit nonzero"
    fi
}

if [ "${A5_GATE_SKIP_LIVE:-0}" != "1" ]; then
    invoke "G-B*/N2"  "bootstrap_output_audit (G-B1/B3/B4/B5/N2)" \
        bash "$HERE/bootstrap_output_audit.sh"
    invoke "G-L*"     "a5_lifecycle_live (G-L1..L6 + L6c sweep-belt bite)" \
        bash "$HERE/a5_lifecycle_live.sh"
    invoke "G-R1.sa"  "a3_state_assertions (A5-reconciled)" \
        env A3_SKIP_BUILD=1 bash "$HERE/a3_state_assertions.sh"
    invoke "G-R1.self" "run_selftests (normalize/jail/secret-scan/record-gate/fetch-zmx)" \
        bash "$REPO_ROOT/test/golden/selftest/run_selftests.sh"
    # G-REC corpus round-trip via verify.sh (each a5rec_* scenario). Clean env per
    # the invoke() rationale — verify.sh establishes its own jail.
    hdr "G-REC corpus round-trip (verify.sh --scenario a5rec_*)"
    grec_fail=0
    for s in reconcile config gc kill ping resume; do
        if env -u SB_HOME -u ZMX_DIR -u TMPDIR -u XDG_RUNTIME_DIR \
               -u SB_RELAY_DISABLE_SCAN -u SB_RELAY_PORT -u SB_RELAY_SOCKET_PREFIX \
               -u JAIL_ROOT -u JAIL_PREFIX \
               HOME="${JAIL_REAL_HOME:-$HOME}" \
               SB_UNDER_TEST="$SB_BIN" JAIL_SB_CMD="$SB_BIN" RECORD_RUNID="grec${s}$$" \
               bash "$REPO_ROOT/test/golden/verify.sh" \
               --scenario "$HERE/a5rec_${s}.sh" >/dev/null 2>&1; then
            printf '    a5rec_%-10s verify PASS\n' "$s"
        else
            printf '    a5rec_%-10s verify FAIL\n' "$s"; grec_fail=1
        fi
    done
    [ "$grec_fail" = "0" ] && row_pass "G-REC" "all 6 corpus round-trips PASS" \
                           || row_fail "G-REC" "a corpus round-trip failed"
else
    printf '\n[A5_GATE_SKIP_LIVE=1] live-jail + selftest + G-REC rows skipped.\n'
fi

# ===========================================================================
# SUMMARY
# ===========================================================================
printf '\n===========================================================\n'
printf 'A5 GATE DRIVER SUMMARY: PASS=%d  FAIL=%d\n' "$PASS" "$FAIL"
if [ "$FAIL" -ne 0 ]; then
    printf 'FAILED ROWS:%s\n' "$FAILED"
    exit 1
fi
printf 'A5 GATE DRIVER: ALL ROWS GREEN\n'
exit 0
