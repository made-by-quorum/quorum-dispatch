#!/bin/bash
# test/golden/scenarios/a6_routing.sh
#
# A6 ROUTING-lane live-jail rows (spec §7): F1 create-path wiring (G-A2) + --via
# backend routing (G-A3) against the REAL Rust qd binary, real zmx, and a STUB
# claude (zero real-Claude boot, ADD-10a — every session here is a JAIL session
# created with a stub claude; nothing touches real org state or the network).
#
# The stub claude dumps its received environment to a file, so we can prove the
# composed backend vars (F1 caller-capture and/or --via profile overlay) actually
# REACHED the child through the 0600 self-deleting env file — without the value
# ever appearing in argv (G-A5 hygiene, asserted here too).
#
# House pattern: copies a5_lifecycle_live.sh — own hermetic jail (rule 9 + ADD-4),
# sbrg- names only, PASS/FAIL + SUMMARY, jail_teardown on EXIT. Bash 3.2 floor.
#
# Usage:  bash test/golden/scenarios/a6_routing.sh
# Env override: SB_BIN (qd-under-test), ZMX_BIN (zmx). Defaults autodetect.
set -u

# --- locate the worktree + binaries -----------------------------------------
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="$(cd "$HERE/../../.." && pwd)"            # scenarios -> golden -> test -> repo root
cd "$WT" || { echo "FATAL: cannot cd to worktree root"; exit 1; }

SB_BIN="${SB_BIN:-$WT/target/debug/qd}"
ZMX_BIN="${ZMX_BIN:-$(command -v zmx 2>/dev/null || echo /opt/homebrew/bin/zmx)}"
[ -x "$SB_BIN" ]  || { echo "FATAL: qd binary not found/executable: $SB_BIN"; exit 1; }
[ -x "$ZMX_BIN" ] || { echo "FATAL: zmx binary not found/executable: $ZMX_BIN"; exit 1; }

export JAIL_SB_CMD="$SB_BIN"
export JAIL_ZMX_CMD="$ZMX_BIN"
. test/golden/lib/jail.sh
# Short runid so prefix `sbrg-XXXX-` (10 chars) leaves room under zmx's 20-byte
# name cap for a short suffix (e.g. `a2`,`a3a`).
SHORT_RUNID="$(printf '%s' "${RANDOM:-0}${RANDOM:-0}" | tr -cd 'a-z0-9' | cut -c1-4)"
[ -n "$SHORT_RUNID" ] || SHORT_RUNID="z$$"
SHORT_RUNID="$(printf '%s' "$SHORT_RUNID" | cut -c1-4)"
jail_establish "$SHORT_RUNID" || { echo "FATAL: jail_establish failed"; exit 1; }
trap jail_teardown EXIT

# Real-home invisibility belt (rule 9): org session count unchanged BEFORE/AFTER.
REAL_BEFORE="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"

PASS_ALL=1
note_fail() { PASS_ALL=0; echo "  [FAIL] $1"; }
ok()        { echo "  [ok] $1"; }

# --- stub claude: writes an idle registry entry, DUMPS its env, then sleeps ---
# Mirrors a5's fake-claude but ALSO dumps `env` to $ENVDUMP_DIR/<name>.env so the
# scenario can prove the backend vars arrived. The boot waiter (boot.rs
# scan_for_name) keys on the registry NAME; qd passes --name <zmxname>, and the
# stub records exactly that.
ENVDUMP_DIR="$JAIL_ROOT/tmp/envdump"; mkdir -p "$ENVDUMP_DIR"
STUB="$JAIL_ROOT/stub-claude"
cat > "$STUB" <<'EOS'
#!/bin/bash
# Stub claude. Parse --name, dump our env (so the test can inspect what the
# self-deleting env-file prefix exported into us), write an idle registry entry,
# then sleep to stay live for the boot-ready probe.
name=""
while [ $# -gt 0 ]; do
  case "$1" in
    --name) name="$2"; shift 2;;
    *) shift;;
  esac
done
[ -n "${SBRG_STUB_NAME:-}" ] && name="$SBRG_STUB_NAME"
# Dump the env vars of interest (NOT the whole env — keep it focused + readable).
{
  printf 'ANTHROPIC_BASE_URL=%s\n' "${ANTHROPIC_BASE_URL:-<unset>}"
  printf 'ANTHROPIC_MODEL=%s\n' "${ANTHROPIC_MODEL:-<unset>}"
  printf 'ANTHROPIC_AUTH_TOKEN=%s\n' "${ANTHROPIC_AUTH_TOKEN:-<unset>}"
  printf 'ANTHROPIC_API_KEY=%s\n' "${ANTHROPIC_API_KEY:-<unset>}"
} > "$ENVDUMP_DIR/$name.env"
mkdir -p "$HOME/.claude/sessions"
sid="stub-$$"
printf '{"pid":%d,"name":"%s","status":"idle","sessionId":"%s","cwd":"%s","version":"stub","kind":"interactive","entrypoint":"cli","startedAt":1700000000000,"updatedAt":1700000000000}\n' \
  "$$" "$name" "$sid" "$PWD" > "$HOME/.claude/sessions/$$.json"
exec sleep 120
EOS
chmod +x "$STUB"
export CLAUDE_BIN="$STUB"
export ENVDUMP_DIR
mkdir -p "$HOME/.claude/sessions"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"

# new_session <name> [--via NAME] [ANTHROPIC_VAR=val ...] — drive `qd new` in the
# jail with the stub claude. Caller-captured ANTHROPIC_* vars are passed inline
# (export-before-run, the F1 capture surface). Returns qd's exit code; stdout/err
# land in $JAIL_ROOT/<name>.{out,err}.
new_session() {
  local name="$1"; shift
  local via=""
  local exports=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --via) via="$2"; shift 2;;
      *=*) exports="$exports $1"; shift;;
      *) shift;;
    esac
  done
  local args="new $name --cwd $WORKDIR"
  [ -n "$via" ] && args="$args --via $via"
  # The stub must report the same name qd passes as --name (== the session name).
  ( cd "$WORKDIR" \
    && env SBRG_STUB_NAME="$name" $exports "$SB_BIN" $args ) \
    > "$JAIL_ROOT/$name.out" 2> "$JAIL_ROOT/$name.err"
}

# cleanup_session <name> — belt + force kill, then reap the stub pid.
cleanup_session() {
  local name="$1"
  if jail_assert_resolves_in_jail "$name" 2>/dev/null; then
    jail_sb kill --force "$name" >/dev/null 2>&1
  else
    "$ZMX_BIN" kill "$name" --force >/dev/null 2>&1 || true
  fi
}

echo "=== A6 routing live rows (jail=$JAIL_PREFIX root=$JAIL_ROOT) ==="

# ===========================================================================
# G-A1 negative control (unit leg lives in create.rs f1_empty_*): a plain
# `qd new` with NO ANTHROPIC_* set and NO --via writes NO env file and the
# session boots normally. We assert: no session-env file was created.
# ===========================================================================
echo "--- G-A1: plain new (no backend env) writes NO env file ---"
GA1="${JAIL_PREFIX}a1"
new_session "$GA1"; code=$?
echo "    new exit=$code"
[ -s "$JAIL_ROOT/$GA1.err" ] && sed 's/^/    err: /' "$JAIL_ROOT/$GA1.err"
[ "$code" = "0" ] && ok "G-A1 plain new exit 0" || note_fail "G-A1 plain new exit=$code"
if [ -f "$HOME/.quorum/dispatch/session-env/$GA1.env" ]; then
  note_fail "G-A1 env file present for an empty capture (must be absent)"
else
  ok "G-A1 no env file for empty capture (byte-zero change)"
fi
cleanup_session "$GA1"

# ===========================================================================
# G-A8 telemetry FAILURE-LEG on a ported surface (merge-ruling MAJOR-1): the
# create-stamp appends are warn-not-fail by design (spec §4.1). Inject an append
# failure (marks.jsonl as a DIRECTORY → open fails) and assert the ported `qd new`
# surface is UNCHANGED: exit 0, the normal stdout, and stderr carrying EXACTLY the
# two designed warn lines (create event + create-usage — one per append), nothing
# else. This is a FAILURE-MODE-ONLY divergence vs TS at pin (TS emits nothing —
# named in exec/divergence-table.md), never a silent drop.
# ===========================================================================
echo "--- G-A8: telemetry append-failure leg (marks.jsonl as dir) ---"
GA8="${JAIL_PREFIX}a8"
MARKS_PATH="$SB_HOME/state/marks.jsonl"
rm -f "$MARKS_PATH"; mkdir -p "$MARKS_PATH"   # a DIRECTORY at the file path
new_session "$GA8"; code=$?
echo "    new exit=$code"
[ "$code" = "0" ] && ok "G-A8 exit code UNCHANGED (0) under append failure" \
  || note_fail "G-A8 exit=$code (telemetry failure changed a ported exit code)"
grep -q "Started detached session \"$GA8\"" "$JAIL_ROOT/$GA8.out" \
  && ok "G-A8 stdout UNCHANGED (started line present)" \
  || note_fail "G-A8 stdout missing/changed: $(cat "$JAIL_ROOT/$GA8.out")"
WARNS="$(grep -c "WARNING: telemetry .* append failed (non-fatal)" "$JAIL_ROOT/$GA8.err" 2>/dev/null)"
OTHERS="$(grep -vc "WARNING: telemetry .* append failed (non-fatal)" "$JAIL_ROOT/$GA8.err" 2>/dev/null)"
if [ "$WARNS" = "2" ] && [ "$OTHERS" = "0" ]; then
  ok "G-A8 stderr = EXACTLY the two designed warn lines (create-event + usage)"
else
  note_fail "G-A8 stderr unexpected (warns=$WARNS others=$OTHERS): $(cat "$JAIL_ROOT/$GA8.err")"
fi
rm -rf "$MARKS_PATH"   # restore: the dir is jail-local
cleanup_session "$GA8"

# ===========================================================================
# G-A2 F1-wiring positive (PORT row): `qd new` with ANTHROPIC_BASE_URL set in the
# caller env → the stub claude's env dump shows the var arrived, AND the env file
# is GONE after boot (D1 self-delete).
# ===========================================================================
echo "--- G-A2: F1 caller-capture reaches the child + self-delete ---"
GA2="${JAIL_PREFIX}a2"
new_session "$GA2" "ANTHROPIC_BASE_URL=http://127.0.0.1:9911"; code=$?
echo "    new exit=$code"
[ -s "$JAIL_ROOT/$GA2.err" ] && sed 's/^/    err: /' "$JAIL_ROOT/$GA2.err"
[ "$code" = "0" ] && ok "G-A2 new exit 0" || note_fail "G-A2 new exit=$code"
DUMP="$ENVDUMP_DIR/$GA2.env"
if [ -f "$DUMP" ] && grep -q "ANTHROPIC_BASE_URL=http://127.0.0.1:9911" "$DUMP"; then
  ok "G-A2 ANTHROPIC_BASE_URL arrived in the child (env-file dot-source worked)"
else
  note_fail "G-A2 base url did not reach the child (dump: $(cat "$DUMP" 2>/dev/null))"
fi
# D1 self-delete: the per-session env file is gone after boot.
if [ -f "$HOME/.quorum/dispatch/session-env/$GA2.env" ]; then
  note_fail "G-A2 env file survived boot (D1 self-delete failed)"
else
  ok "G-A2 env file self-deleted after boot (D1)"
fi
cleanup_session "$GA2"

# ===========================================================================
# G-A2b exit-97 fail-closed (D1 source-failure contract): the env-file prefix
# sources fail-closed (exit 97) when the dot-source fails. The self-deleting prod
# file cannot be corrupted mid-`qd new`, so we exercise the REAL prefix SHAPE
# (the exact bash the binary emits) against a deliberately-broken env file and
# prove exit 97. This pins the contract end-to-end at the bash level.
# ===========================================================================
echo "--- G-A2b: env-file source fail-closed → exit 97 ---"
BROKEN="$JAIL_ROOT/tmp/broken.env"
# Invalid shell: an unterminated quote makes `. file` fail.
printf "export X='unterminated\n" > "$BROKEN"
# The prefix shape is byte-identical to launch.rs session_env_prefix():
#   { . 'FILE'; } || { echo '...' >&2; exit 97; }; rm -f -- 'FILE';
PREFIX="{ . '$BROKEN'; } || { echo 'qd: env file source failed, aborting session' >&2; exit 97; }; rm -f -- '$BROKEN'; "
bash -lc "${PREFIX}true" 2>"$JAIL_ROOT/a2b.err"; code=$?
echo "    fail-closed prefix exit=$code"
[ "$code" = "97" ] && ok "G-A2b source failure aborts with exit 97" || note_fail "G-A2b exit=$code (expect 97)"
grep -q "env file source failed" "$JAIL_ROOT/a2b.err" && ok "G-A2b fail-closed message present" || note_fail "G-A2b message missing"

# ===========================================================================
# G-A3 --via end-to-end (CCR-shaped): backends.json with two ccr profiles (ports
# differ) + a file-backend secret. `qd new --via ccr-a` → the stub env dump shows
# the profile's baseUrl/model/credential var; `--via ccr-b` routes the other
# port. Loud-failure rows: unknown name, missing baseUrl, missing secret.
# ===========================================================================
echo "--- G-A3: --via routing (two ccr profiles) + loud failures ---"
# Seed a FILE-backend secret (no keychain dependency in CI): write the secret
# under the jailed qd config via `qd config set <key> <value>` so get_secret
# resolves it. Force the file backend so the jail never touches the keychain.
# The value is passed as an ARGUMENT (non-TTY stdin is rejected by config set).
# Obviously-FAKE value (credential hard line: never a real-looking key).
export SB_SECRET_BACKEND=file
jail_sb config set openrouter-key "sk-FAKE-ccr-token" >/dev/null 2>&1 \
  || note_fail "G-A3 could not seed file-backend secret"

# backends.json under <sbHome>/state (SB_HOME-honored; the jail sets SB_HOME).
STATE_DIR="$SB_HOME/state"; mkdir -p "$STATE_DIR"
BACKENDS="$STATE_DIR/backends.json"
cat > "$BACKENDS" <<EOS
{
  "version": 1,
  "backends": {
    "ccr-a": {
      "baseUrl": "http://127.0.0.1:7001",
      "model": "openrouter,anthropic/claude-sonnet-4.6",
      "credential": { "mode": "secret", "key": "openrouter-key", "var": "ANTHROPIC_AUTH_TOKEN" }
    },
    "ccr-b": { "baseUrl": "http://127.0.0.1:7002" },
    "no-base": { "model": "m" },
    "needs-secret": {
      "baseUrl": "http://127.0.0.1:7003",
      "credential": { "mode": "secret", "key": "absent-key" }
    }
  }
}
EOS
chmod 600 "$BACKENDS"

# G-A3a: --via ccr-a → profile baseUrl + model + secret-into-AUTH_TOKEN arrive.
# A7 anti-vacuity hardening (Lima 2026-06-05 finding): export a DECOY caller
# API_KEY for this leg so the F12 drop assertion has something REAL to drop.
# Without it the "<unset>" check passes vacuously on any host that doesn't
# happen to export ANTHROPIC_API_KEY — which is exactly how the inherited-env
# leak shipped through the A6 macOS gate (a real caller key rode into a
# profile-secret child on Lima, where the VM .profile exports one). sk-FAKE
# decoy = scan-safe. The leg also asserts the decoy VALUE is absent anywhere
# in the dump (belt: not just "<unset>" in the slot line).
GA3A="${JAIL_PREFIX}a3a"
export ANTHROPIC_API_KEY="sk-FAKE-caller-decoy-must-be-dropped"
new_session "$GA3A" --via ccr-a; code=$?
unset ANTHROPIC_API_KEY
echo "    new --via ccr-a exit=$code"
[ -s "$JAIL_ROOT/$GA3A.err" ] && sed 's/^/    err: /' "$JAIL_ROOT/$GA3A.err"
[ "$code" = "0" ] && ok "G-A3a --via ccr-a exit 0" || note_fail "G-A3a exit=$code"
DUMP="$ENVDUMP_DIR/$GA3A.env"
if [ -f "$DUMP" ]; then
  grep -q "ANTHROPIC_BASE_URL=http://127.0.0.1:7001" "$DUMP" && ok "G-A3a baseUrl from profile" || note_fail "G-A3a baseUrl wrong: $(grep BASE_URL "$DUMP")"
  grep -q "ANTHROPIC_MODEL=openrouter,anthropic/claude-sonnet-4.6" "$DUMP" && ok "G-A3a model from profile" || note_fail "G-A3a model wrong: $(grep MODEL "$DUMP")"
  grep -q "ANTHROPIC_AUTH_TOKEN=sk-FAKE-ccr-token" "$DUMP" && ok "G-A3a secret resolved into AUTH_TOKEN slot" || note_fail "G-A3a credential wrong: $(grep AUTH_TOKEN "$DUMP")"
  # Credential-slot exclusivity (F12): the OTHER slot (API_KEY) must be unset —
  # even though the CALLER exported a decoy (A7 anti-vacuity, see above).
  grep -q "ANTHROPIC_API_KEY=<unset>" "$DUMP" && ok "G-A3a other credential slot dropped (F12, non-vacuous: caller decoy was exported)" || note_fail "G-A3a API_KEY slot not dropped: $(grep API_KEY "$DUMP")"
  grep -q "sk-FAKE-caller-decoy" "$DUMP" && note_fail "G-A3a decoy VALUE leaked into child env dump" || ok "G-A3a decoy value absent from dump (belt)"
else
  note_fail "G-A3a no env dump"
fi
cleanup_session "$GA3A"

# G-A3b: sibling --via ccr-b routes the OTHER port (deterministic per-name).
GA3B="${JAIL_PREFIX}a3b"
new_session "$GA3B" --via ccr-b; code=$?
echo "    new --via ccr-b exit=$code"
[ "$code" = "0" ] && ok "G-A3b --via ccr-b exit 0" || note_fail "G-A3b exit=$code"
DUMP="$ENVDUMP_DIR/$GA3B.env"
if [ -f "$DUMP" ] && grep -q "ANTHROPIC_BASE_URL=http://127.0.0.1:7002" "$DUMP"; then
  ok "G-A3b ccr-b routed the other port (7002)"
else
  note_fail "G-A3b wrong port: $(grep BASE_URL "$DUMP" 2>/dev/null)"
fi
# mode none + no caller credential → both credential slots unset (no injection).
if [ -f "$DUMP" ] && grep -q "ANTHROPIC_AUTH_TOKEN=<unset>" "$DUMP"; then
  ok "G-A3b mode-none injects no credential"
else
  note_fail "G-A3b unexpected credential under mode none"
fi
cleanup_session "$GA3B"

# G-A3c: unknown backend name → loud exit 1 listing KNOWN names only.
echo "--- G-A3c: loud failures (unknown / missing baseUrl / missing secret) ---"
GA3C="${JAIL_PREFIX}a3c"
new_session "$GA3C" --via nope; code=$?
echo "    new --via nope exit=$code"
sed 's/^/    err: /' "$JAIL_ROOT/$GA3C.err"
[ "$code" = "1" ] && ok "G-A3c unknown backend exit 1" || note_fail "G-A3c exit=$code (expect 1)"
if grep -q "unknown backend 'nope'" "$JAIL_ROOT/$GA3C.err" \
   && grep -q "ccr-a" "$JAIL_ROOT/$GA3C.err"; then
  ok "G-A3c lists known names (ccr-a present)"
else
  note_fail "G-A3c missing known-names listing"
fi
# No session created (the stub never dumped an env for this name).
[ -f "$ENVDUMP_DIR/$GA3C.env" ] && note_fail "G-A3c booted despite unknown backend" || ok "G-A3c created no session"

# G-A3d: missing baseUrl (the `no-base` profile) → loud exit 1 naming the field.
GA3D="${JAIL_PREFIX}a3d"
new_session "$GA3D" --via no-base; code=$?
echo "    new --via no-base exit=$code"
sed 's/^/    err: /' "$JAIL_ROOT/$GA3D.err"
[ "$code" = "1" ] && ok "G-A3d missing baseUrl exit 1" || note_fail "G-A3d exit=$code (expect 1)"
grep -q "baseUrl" "$JAIL_ROOT/$GA3D.err" && ok "G-A3d names the missing field" || note_fail "G-A3d does not name baseUrl"

# G-A3e: missing secret (the `needs-secret` profile, key not set) → loud exit 1
# naming the key + config-set hint, NEVER a value.
GA3E="${JAIL_PREFIX}a3e"
new_session "$GA3E" --via needs-secret; code=$?
echo "    new --via needs-secret exit=$code"
sed 's/^/    err: /' "$JAIL_ROOT/$GA3E.err"
[ "$code" = "1" ] && ok "G-A3e missing secret exit 1" || note_fail "G-A3e exit=$code (expect 1)"
if grep -q "absent-key" "$JAIL_ROOT/$GA3E.err" && grep -q "qd config set" "$JAIL_ROOT/$GA3E.err"; then
  ok "G-A3e names the key + config-set hint"
else
  note_fail "G-A3e missing key name / hint"
fi

# ===========================================================================
# G-A5 hygiene teeth: the credential VALUE must never appear in any zmx-visible
# string. Grep the zmx command line (history/list), qd stdout/stderr captures,
# and backends.json for the FAKE secret value. (The negative control proving the
# assert BITES lives in the survey.rs G-S2 pattern + launch.rs prefix unit.)
# ===========================================================================
echo "--- G-A5: credential value never in argv / logs / fixtures ---"
SECRET_VAL="sk-FAKE-ccr-token"
LEAK=0
# qd captures (all .out/.err under JAIL_ROOT).
if grep -rq "$SECRET_VAL" "$JAIL_ROOT"/*.out "$JAIL_ROOT"/*.err 2>/dev/null; then
  note_fail "G-A5 secret value leaked into an qd stdout/stderr capture"; LEAK=1
fi
# backends.json holds key NAMES only — never the value.
if grep -q "$SECRET_VAL" "$BACKENDS" 2>/dev/null; then
  note_fail "G-A5 secret value present in backends.json (must hold NAMES only)"; LEAK=1
fi
# The composed env file self-deletes; if any survived, it must not be argv-visible
# anyway. Check the zmx command strings for any live jail session do not carry it.
if jail_zmx list 2>/dev/null | grep -q "$SECRET_VAL"; then
  note_fail "G-A5 secret value visible in a zmx command line (argv leak)"; LEAK=1
fi
[ "$LEAK" = "0" ] && ok "G-A5 no credential-value leak in argv / logs / backends.json"

# NEGATIVE CONTROL with TEETH (spec §7 G-A5; impl-redteam F-3): plant the secret
# value in a canary capture file, re-run the SAME sweep, and require it to FIRE.
# A sweep that cannot detect a planted leak proves nothing about the clean pass.
CANARY="$JAIL_ROOT/ga5-canary.out"
printf 'argv: qd new --token %s\n' "$SECRET_VAL" > "$CANARY"
if grep -rq "$SECRET_VAL" "$JAIL_ROOT"/*.out "$JAIL_ROOT"/*.err 2>/dev/null; then
  ok "G-A5 negative control: the sweep DETECTS a planted leak (assert bites)"
else
  note_fail "G-A5 negative control FAILED: planted leak NOT detected — sweep is blind"
fi
rm -f "$CANARY"

# ===========================================================================
# Real-home invisibility belt (rule 9): org session count unchanged.
# ===========================================================================
REAL_AFTER="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"
echo "--- real-home belt: $REAL_BEFORE -> $REAL_AFTER ---"
[ "$REAL_BEFORE" = "$REAL_AFTER" ] && ok "real-home untouched ($REAL_BEFORE)" || note_fail "real-home DRIFT ($REAL_BEFORE -> $REAL_AFTER)"

echo "==========================================================="
if [ "$PASS_ALL" = "1" ]; then
  echo "A6-ROUTING: PASS"
  exit 0
else
  echo "A6-ROUTING: FAIL"
  exit 1
fi
