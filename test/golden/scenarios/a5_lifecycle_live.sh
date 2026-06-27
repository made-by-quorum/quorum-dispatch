#!/bin/bash
# test/golden/scenarios/a5_lifecycle_live.sh
#
# A5 workstream-C live-jail rows G-L1..G-L6 (spec §7): kill / gc / resume /
# reconcile against the REAL Rust qd binary, real zmx 0.6, and a fake-claude
# (zero real-Claude boot, ADD-10a — nothing here creates a Claude/agent session).
#
# House pattern: self-contained like dryrun/a2-mac-fakeclaude.sh. Establishes its
# OWN per-run hermetic jail (rule 9 + ADD-4 + L15 belt), sbrg- names only, every
# destructive row pre-asserts jail_assert_resolves_in_jail, jailed assertions key
# on `jail_zmx list` (never `qd ls`). Bash 3.2 floor (macOS): no assoc arrays,
# no ${var,,}, no mapfile.
#
# Usage:  bash test/golden/scenarios/a5_lifecycle_live.sh
# Env override: QD_BIN (qd-under-test), ZMX_BIN (zmx). Defaults autodetect.
set -u

# --- locate the worktree + binaries (no hardcoded worktree path) --------------
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="$(cd "$HERE/../../.." && pwd)"            # scenarios -> golden -> test -> repo root
cd "$WT" || { echo "FATAL: cannot cd to worktree root"; exit 1; }

QD_BIN="${QD_BIN:-$WT/target/debug/qd}"
ZMX_BIN="${ZMX_BIN:-$(command -v zmx 2>/dev/null || echo /opt/homebrew/bin/zmx)}"
[ -x "$QD_BIN" ]  || { echo "FATAL: qd binary not found/executable: $QD_BIN"; exit 1; }
[ -x "$ZMX_BIN" ] || { echo "FATAL: zmx binary not found/executable: $ZMX_BIN"; exit 1; }

export JAIL_SB_CMD="$QD_BIN"
export JAIL_ZMX_CMD="$ZMX_BIN"
. test/golden/lib/jail.sh
# zmx caps session names at 20 bytes. The jail prefix is `sbrg-<runid>-`, so we
# pass a SHORT 4-char runid → prefix `sbrg-XXXX-` (10 chars), leaving room for a
# short suffix (e.g. `k1`,`g4`,`r5`) under the 20-byte zmx limit.
SHORT_RUNID="$(printf '%s' "${RANDOM:-0}${RANDOM:-0}" | tr -cd 'a-z0-9' | cut -c1-4)"
[ -n "$SHORT_RUNID" ] || SHORT_RUNID="z$$"
SHORT_RUNID="$(printf '%s' "$SHORT_RUNID" | cut -c1-4)"
jail_establish "$SHORT_RUNID" || { echo "FATAL: jail_establish failed"; exit 1; }
trap jail_teardown EXIT

# Real-home invisibility belt: snapshot the org's real session count BEFORE and
# require it unchanged AFTER (rule 9). Any drift = the harness touched prod.
REAL_BEFORE="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"

PASS_ALL=1
note_fail() { PASS_ALL=0; echo "  [FAIL] $1"; }
ok()        { echo "  [ok] $1"; }

# The SWEEP BELT (jail_sweep_belt_ok) lives in test/golden/lib/jail.sh — required
# + wired per the orc-3 ruling. The G-L6 destructive branch (gated OFF on macOS)
# calls it; the G-L6c negative-control row below proves it BITES on a non-sbrg
# planned target.

# --- fake-claude: writes a PID-keyed registry entry, then sleeps -------------
# Mirrors a2-mac-fakeclaude.sh. The registry file is what kill/reconcile/resume
# operate on. `exec sleep` keeps the zmx wrapper + "claude" PID alive so liveness
# probes (kill -0) see a live process — exactly a real cold-resumable session.
FAKE="$JAIL_ROOT/fake-claude"
cat > "$FAKE" <<'EOS'
#!/bin/bash
# Fake claude. Writes a PID-keyed registry entry then sleeps. The registry NAME
# is what the resume EventBootWaiter (boot.rs scan_for_name) keys on; a real
# claude knows its own name, so for the resume relaunch the scenario passes the
# expected zmx name via SBRG_FAKE_NAME (the real-claude knowledge stand-in).
name=""; sid=""
while [ $# -gt 0 ]; do
  case "$1" in
    --name) name="$2"; shift 2;;
    --resume) sid="$2"; shift 2;;
    *) shift;;
  esac
done
[ -n "${SBRG_FAKE_NAME:-}" ] && name="$SBRG_FAKE_NAME"
mkdir -p "$HOME/.claude/sessions"
[ -n "$sid" ] || sid="fake-$$"
printf '{"pid":%d,"name":"%s","status":"idle","sessionId":"%s","cwd":"%s","version":"fake","kind":"interactive","entrypoint":"cli","startedAt":1700000000000,"updatedAt":1700000000000}\n' \
  "$$" "$name" "$sid" "$PWD" > "$HOME/.claude/sessions/$$.json"
exec sleep 120
EOS
chmod +x "$FAKE"
export CLAUDE_BIN="$FAKE"
mkdir -p "$HOME/.claude/sessions"
WORKDIR="$JAIL_ROOT/tmp/work"; mkdir -p "$WORKDIR"

# spawn_fake <name> -> launches `zmx run <name> bash -lc 'fake --name <name>'`
# detached in the jail; returns once the registry entry lands (polls ≤6s). We do
# NOT use `qd new` (ADD-10a banned) — we drive zmx directly with the fake binary.
spawn_fake() {
  local name="$1" sid="${2:-}" i=0
  local cmd="'$FAKE' --name '$name'"
  [ -n "$sid" ] && cmd="$cmd --resume '$sid'"
  ( cd "$WORKDIR" && "$ZMX_BIN" run "$name" -d bash -lc "$cmd" ) >/dev/null 2>&1
  # Wait for BOTH the zmx task AND the registry entry (the fake writes its
  # <pid>.json from INSIDE zmx, which lags the task; the join/kill needs the
  # registry, so we poll the registry by name — not just the task).
  while [ "$i" -lt 40 ]; do
    if [ -n "$(fake_pid_for "$name")" ] \
       && [ "$(jail_zmx list 2>/dev/null | grep -c "$name" || true)" -ge 1 ]; then
      return 0
    fi
    i=$((i+1)); sleep 0.2
  done
  return 1
}

# fake_pid_for <name> -> the claude(=fake sleep) PID the registry recorded for a
# session whose recorded name matches. Reads the jailed registry JSON.
fake_pid_for() {
  local name="$1" f pid
  for f in "$HOME/.claude/sessions"/*.json; do
    [ -f "$f" ] || continue
    case "$(cat "$f")" in
      *"\"name\":\"$name\""*)
        pid="$(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$f")"
        printf '%s' "$pid"; return 0;;
    esac
  done
  printf ''
}

echo "=== A5 G-L live rows (jail=$JAIL_PREFIX root=$JAIL_ROOT) ==="

# ===========================================================================
# G-L1: kill live — sbrg- session, belt pre-assert, dual-reap, tombstone present,
#        post-verify clean, exit 0.
# ===========================================================================
echo "--- G-L1: kill live (dual-reap + tombstone + post-verify clean) ---"
GL1="${JAIL_PREFIX}kill1"
spawn_fake "$GL1"
tasks="$(jail_zmx list 2>/dev/null | grep -c "$GL1" || true)"
[ "$tasks" = "1" ] && ok "G-L1 spawned (zmx tasks=1)" || note_fail "G-L1 spawn (tasks=$tasks expect 1)"
pid1="$(fake_pid_for "$GL1")"
[ -n "$pid1" ] && ok "G-L1 registry entry captured (pid=$pid1)" || note_fail "G-L1 registry entry missing"
# BELT pre-assert before the destructive kill.
if jail_assert_resolves_in_jail "$GL1"; then
  ok "G-L1 resolution belt passed"
  out="$(jail_sb kill --force "$GL1" 2>"$JAIL_ROOT/gl1.err")"; code=$?
  echo "    kill exit=$code out=[$out]"
  [ -s "$JAIL_ROOT/gl1.err" ] && sed 's/^/    err: /' "$JAIL_ROOT/gl1.err"
  [ "$code" = "0" ] && ok "G-L1 exit 0" || note_fail "G-L1 exit=$code (expect 0)"
  # W4 (ADD-15 wart-wave): the unambiguous success line, byte-exact incl. pid.
  case "$out" in
    "killed $GL1 (zmx $GL1, pid $pid1)") ok "G-L1 output byte-exact (W4 line)" ;;
    *) note_fail "G-L1 output mismatch: [$out] (expect W4 line w/ pid=$pid1)" ;;
  esac
  # dual-reap: the claude PID must be dead.
  if [ -n "$pid1" ] && kill -0 "$pid1" 2>/dev/null; then
    note_fail "G-L1 claude pid $pid1 still alive (dual-reap failed)"
  else
    ok "G-L1 claude pid reaped"
  fi
  # post-verify: zmx task gone.
  after="$(jail_zmx list 2>/dev/null | grep -c "$GL1" || true)"
  [ "$after" = "0" ] && ok "G-L1 post-verify clean (zmx tasks=0)" || note_fail "G-L1 survivor (tasks=$after)"
  # tombstone present: a <pid>.json.tombstoned exists for the reaped pid.
  if [ -n "$pid1" ] && [ -f "$HOME/.claude/sessions/$pid1.json.tombstoned" ]; then
    ok "G-L1 tombstone synthesized"
  else
    note_fail "G-L1 tombstone missing for pid $pid1"
  fi
else
  note_fail "G-L1 resolution belt FAILED — refusing to kill"
fi

# ===========================================================================
# G-L2: kill non-TTY without --force → DIRECT KILL (W3, ADD-15 wart-wave).
#        REWRITTEN WHOLE from the old refusal leg (retired-with-reason: Pete
#        ruled the confirm prompt "misguided for this tool" 19:47 EDT 2026-06-05;
#        the old leg's exit-1 refusal + "session untouched" assertions INVERT —
#        kill now executes directly). stdin is the script's (non-TTY), the
#        natural state. MUTATION TEETH: re-adding the prompt (kill would block
#        on stdin or print [y/N]) or the refusal (exit 1, session alive) REDs
#        every assertion below. Divergence row: exec/divergence-table.md W3.
# ===========================================================================
echo "--- G-L2: kill non-TTY without --force → direct kill (W3) ---"
GL2="${JAIL_PREFIX}kill2"
spawn_fake "$GL2"
pid2="$(fake_pid_for "$GL2")"
if jail_assert_resolves_in_jail "$GL2"; then
  out="$(jail_sb kill "$GL2" </dev/null 2>"$JAIL_ROOT/gl2.err")"; code=$?
  echo "    kill(no --force) exit=$code out=[$out]"
  [ -s "$JAIL_ROOT/gl2.err" ] && sed 's/^/    err: /' "$JAIL_ROOT/gl2.err"
  [ "$code" = "0" ] && ok "G-L2 exit 0 (direct kill)" || note_fail "G-L2 exit=$code (expect 0)"
  # W4 line, byte-exact (the W3 row carries the W4 teeth too).
  case "$out" in
    "killed $GL2 (zmx $GL2, pid $pid2)") ok "G-L2 output byte-exact (W4 line)" ;;
    *) note_fail "G-L2 output mismatch: [$out] (expect W4 line w/ pid=$pid2)" ;;
  esac
  # No prompt/refusal bytes anywhere (stdout OR stderr).
  if printf '%s' "$out" | grep -q '\[y/N\]' || grep -q "Refusing to kill" "$JAIL_ROOT/gl2.err"; then
    note_fail "G-L2 prompt/refusal bytes present (W3 regression)"
  else
    ok "G-L2 no prompt/refusal bytes"
  fi
  # The session must be REAPED (the old 'untouched' assertion, inverted).
  alive="$(jail_zmx list 2>/dev/null | grep -c "$GL2" || true)"
  [ "$alive" = "0" ] && ok "G-L2 session reaped (zmx tasks=0)" || note_fail "G-L2 survivor (tasks=$alive)"
else
  note_fail "G-L2 resolution belt FAILED"
fi

# ===========================================================================
# G-L3: kill zmx-survivor advisory — a shim zmx that REFUSES to die. The kill
#        cleans the registry but the zmx task lingers → exit 1 + advisory.
# ===========================================================================
echo "--- G-L3: kill zmx-survivor advisory (shim zmx refuses to die) ---"
# Build a shim zmx: passes through to the real zmx for list/run, but turns
# `kill` into a NO-OP (the task survives). The kill verb's verify-gone loop must
# then time out → loud exit 1 advisory, NEVER kill the unconfirmed target.
SHIM_DIR="$JAIL_ROOT/shimbin"; mkdir -p "$SHIM_DIR"
SHIM="$SHIM_DIR/zmx"
GL3="${JAIL_PREFIX}surv3"
# zmx shim:
#   - `kill`/`k` → NO-OP exit 0 (the task "refuses to die").
#   - `list`/`ls` → pass through to real zmx, then APPEND a synthetic survivor row
#     for GL3 so the verb's verify-gone scan ALWAYS sees it alive (a real zmx
#     would clean the task once the inner claude PID dies; the shim keeps the
#     survivor visible so we exercise the advisory path deterministically).
#   - everything else passes through.
cat > "$SHIM" <<EOS
#!/bin/bash
case "\$1" in
  kill|k) exit 0 ;;
  list|ls)
    "$ZMX_BIN" "\$@"
    printf 'name=%s\tpid=999000\tclients=0\tcreated=1700000000\tstart_dir=%s\n' "$GL3" "$WORKDIR"
    exit 0 ;;
  *) exec "$ZMX_BIN" "\$@" ;;
esac
EOS
chmod +x "$SHIM"
spawn_fake "$GL3"           # spawn via the REAL zmx (task is genuinely alive)
pid3="$(fake_pid_for "$GL3")"
if jail_assert_resolves_in_jail "$GL3"; then
  ok "G-L3 resolution belt passed"
  # Run the kill with the SHIM zmx on PATH so zmx kill no-ops -> survivor.
  out="$(PATH="$SHIM_DIR:$PATH" JAIL_ZMX_CMD="$SHIM" jail_sb kill --force "$GL3" 2>"$JAIL_ROOT/gl3.err")"; code=$?
  echo "    kill(shim) exit=$code out=[$out]"
  sed 's/^/    err: /' "$JAIL_ROOT/gl3.err"
  # The shim keeps the survivor visible to the verify-gone loop, so kill takes the
  # LOUD fail-safe path: exit 1 with a "Failed to fully reap ... zmx session ..."
  # advisory (spec §5.1 verify-gone 12×250ms fail-safe). This is a survivor
  # advisory; the F2/C2 post-verify "still exists after kill" + `zmx ls` hint path
  # is the other survivor branch (unit-covered — it needs failures-empty + a
  # stale-zmxName name mismatch, not reliably forgeable with a shim here).
  [ "$code" = "1" ] && ok "G-L3 exit 1 (loud survivor advisory)" || note_fail "G-L3 exit=$code (expect 1)"
  if grep -q "Failed to fully reap session \"$GL3\"" "$JAIL_ROOT/gl3.err" \
     && grep -q "zmx session \"$GL3\"" "$JAIL_ROOT/gl3.err"; then
    ok "G-L3 survivor advisory present (verify-gone fail-safe, names the session)"
  else
    note_fail "G-L3 survivor advisory missing/mismatched"
  fi
  # Discipline belt: the verify-gone advisory must not direct the operator to run
  # `zmx kill <name>` as a REMEDIATION. (It legitimately reports the diagnostic
  # "(zmx kill exit N)" — the exit code of the kill that ran — which is NOT a
  # remediation instruction; we match the imperative hint form `zmx ls`/`ZMX_DIR=
  # ... zmx ls` only in the post-verify path, asserted in G-L3b.)
  if grep -Eq "Verify with: .*zmx kill|run .*zmx kill" "$JAIL_ROOT/gl3.err"; then
    note_fail "G-L3 advisory suggests 'zmx kill' as a remediation (innocent-target hazard)"
  else
    ok "G-L3 advisory carries no 'zmx kill' remediation instruction"
  fi
  # cleanup: now kill for real (real zmx).
  jail_assert_resolves_in_jail "$GL3" && jail_sb kill --force "$GL3" >/dev/null 2>&1
  [ -n "$pid3" ] && kill -9 "$pid3" 2>/dev/null
else
  note_fail "G-L3 resolution belt FAILED"
fi

# G-L3b: the F2/C2 POST-VERIFY survivor path specifically — failures-empty but the
# final name-scan finds a survivor → exit 1 with the "still exists after kill"
# advisory whose hint uses `zmx ls`, NEVER `zmx kill`. A STATEFUL shim reports the
# task GONE for the verify-gone poll (so failures stay empty) then PRESENT for the
# final post-verify scan.
echo "--- G-L3b: kill F2/C2 post-verify hint discipline (zmx ls, never zmx kill) ---"
# The F2/C2 post-verify survivor branch (failures-empty + a stale-zmxName same-name
# survivor in the FINAL scan) cannot be forced LIVE without resolve ambiguity (two
# same-named sources → "Ambiguous" exit 1 before the reap), so its `zmx ls` hint
# discipline is asserted STRUCTURALLY against the built verb source: the advisory
# must instruct `zmx ls` and must NOT instruct a `zmx kill` of an innocent
# same-named task. (The live survivor-advisory EXIT-1 behavior is proven by G-L3.)
KSRC="$WT/crates/qd/src/bin/qd/verbs/kill.rs"
if grep -q "still exists after kill" "$KSRC" \
   && grep -q "Verify with: ZMX_DIR=.* zmx ls" "$KSRC"; then
  ok "G-L3b post-verify advisory uses 'zmx ls' hint (source)"
else
  note_fail "G-L3b post-verify advisory missing the 'zmx ls' hint (source)"
fi
# The post-verify block must NEVER emit a `zmx kill` remediation. Scope the check
# to the kill-verify advisory region (the only place that prints a remediation).
if grep -A6 "still exists after kill" "$KSRC" | grep -q "zmx kill"; then
  note_fail "G-L3b post-verify advisory wrongly suggests 'zmx kill' (innocent-target hazard)"
else
  ok "G-L3b post-verify advisory never suggests 'zmx kill' (innocent-target safe)"
fi

# ===========================================================================
# G-L4: gc dry-run / real / recover / purge with forged file AGES. Production gc
#        uses the real Clock at the bin layer, so we forge FILE MTIMES (>7d) and
#        the trash meta prunedAt (>30d) rather than injecting a clock.
# ===========================================================================
echo "--- G-L4: gc dry-run / real / recover / purge (forged ages) ---"
PROJ="$HOME/.claude/projects/jail-proj"; mkdir -p "$PROJ"
DEADSID="deadsession-gl4"
DEADJSONL="$PROJ/$DEADSID.jsonl"
printf '{"type":"summary"}\n' > "$DEADJSONL"
# Age it 8 days into the past (>7d window) AND ensure no live PID claims its sid
# (no registry entry references DEADSID, so it is dead by construction).
touch -t "$(date -v-8d +%Y%m%d%H%M 2>/dev/null || date -d '8 days ago' +%Y%m%d%H%M)" "$DEADJSONL" 2>/dev/null
# dry-run: lists the candidate, mutates NOTHING.
out="$(jail_sb gc --dry-run 2>"$JAIL_ROOT/gl4dry.err")"; code=$?
echo "    gc --dry-run exit=$code"
echo "$out" | sed 's/^/    | /'
[ "$code" = "0" ] && ok "G-L4 dry-run exit 0" || note_fail "G-L4 dry-run exit=$code"
echo "$out" | grep -q "$DEADSID" && ok "G-L4 dry-run lists dead jsonl" || note_fail "G-L4 dry-run missed candidate"
echo "$out" | grep -q "dry run — no changes made" && ok "G-L4 dry-run banner present" || note_fail "G-L4 dry-run banner missing"
[ -f "$DEADJSONL" ] && ok "G-L4 dry-run mutated nothing (jsonl still present)" || note_fail "G-L4 dry-run DELETED a file"
# real run: moves to trash.
out="$(jail_sb gc 2>"$JAIL_ROOT/gl4real.err")"; code=$?
echo "    gc (real) exit=$code"
echo "$out" | sed 's/^/    | /'
[ "$code" = "0" ] && ok "G-L4 real exit 0" || note_fail "G-L4 real exit=$code"
[ ! -f "$DEADJSONL" ] && ok "G-L4 original removed (moved to trash)" || note_fail "G-L4 original still present after gc"
TRASH="$HOME/.claude/trash"
trashed="$(ls "$TRASH"/*"$DEADSID".jsonl 2>/dev/null | head -1)"
[ -n "$trashed" ] && ok "G-L4 trash file present" || note_fail "G-L4 trash file missing"
meta="$(ls "$TRASH"/*"$DEADSID".jsonl_meta.json 2>/dev/null | head -1)"
[ -n "$meta" ] && ok "G-L4 trash metadata present" || note_fail "G-L4 trash metadata missing"
# list-trash shows it with the Age line (byte-parity fix).
out="$(jail_sb gc --list-trash 2>/dev/null)"
echo "$out" | grep -q "Age:" && ok "G-L4 list-trash shows Age line" || note_fail "G-L4 list-trash missing Age line"
# recover: restores the original; refuses if original exists.
recname="$(basename "$trashed")"      # e.g. <stamp>_<sid>.jsonl
out="$(jail_sb gc --recover "$DEADSID" 2>"$JAIL_ROOT/gl4rec.err")"; code=$?
echo "    gc --recover exit=$code out=[$out]"
[ "$code" = "0" ] && ok "G-L4 recover exit 0" || note_fail "G-L4 recover exit=$code"
[ -f "$DEADJSONL" ] && ok "G-L4 recover restored original" || note_fail "G-L4 recover did not restore"
case "$out" in "✓ Recovered "*) ok "G-L4 recover output byte-exact" ;; *) note_fail "G-L4 recover output: [$out]" ;; esac
# recover refusal when original exists: re-trash then recover again with the file
# present → must exit 1.
jail_sb gc >/dev/null 2>&1            # re-trash the (recovered) dead jsonl
# put a colliding original back so recover must refuse.
printf '{"type":"summary"}\n' > "$DEADJSONL"
out="$(jail_sb gc --recover "$DEADSID" 2>"$JAIL_ROOT/gl4ref.err")"; code=$?
echo "    gc --recover (collision) exit=$code"
sed 's/^/    err: /' "$JAIL_ROOT/gl4ref.err"
[ "$code" = "1" ] && ok "G-L4 recover refuses when original exists (exit 1)" || note_fail "G-L4 recover-collision exit=$code"
grep -q "already exists" "$JAIL_ROOT/gl4ref.err" && ok "G-L4 recover-collision message present" || note_fail "G-L4 recover-collision message missing"
# purge: forge a trash item's meta prunedAt to >30d ago, then --purge.
meta2="$(ls "$TRASH"/*"$DEADSID".jsonl_meta.json 2>/dev/null | head -1)"
if [ -n "$meta2" ]; then
  OLD_ISO="$(date -u -v-31d +%Y-%m-%dT%H:%M:%S.000Z 2>/dev/null || date -u -d '31 days ago' +%Y-%m-%dT%H:%M:%S.000Z)"
  # rewrite the prunedAt field in the meta json (bash 3.2 sed).
  sed "s/\"prunedAt\":[^,]*/\"prunedAt\": \"$OLD_ISO\"/" "$meta2" > "$meta2.tmp" && mv "$meta2.tmp" "$meta2"
  out="$(jail_sb gc --purge 2>"$JAIL_ROOT/gl4purge.err")"; code=$?
  echo "    gc --purge exit=$code"
  echo "$out" | sed 's/^/    | /'
  [ "$code" = "0" ] && ok "G-L4 purge exit 0" || note_fail "G-L4 purge exit=$code"
  echo "$out" | grep -q "✓ Purged" && ok "G-L4 purge removed >30d item" || note_fail "G-L4 purge did not purge"
  [ ! -f "$meta2" ] && ok "G-L4 purged meta gone" || note_fail "G-L4 purged meta still present"
else
  note_fail "G-L4 purge: no meta to age"
fi

# ===========================================================================
# G-L5: resume cold relaunch (fake-claude) + F3 missing-cwd clean error + S2
#        bad zmx-name rejection.
# ===========================================================================
echo "--- G-L5: resume cold relaunch + F3 + S2 ---"
# Build a COLD session as A1 does: a JSONL transcript ONLY (no live <pid>.json,
# no tombstone — a tombstone would make it Killed, which resume refuses). The
# recorded cwd is the FIRST user-record `cwd`; the name comes from an agent-name
# record (sbrg- prefixed). A1 surfaces JSONL-only as status=cold (ColdJsonl).
COLDSID="coldsess-gl5"
# Recorded cwd that EXISTS (for the happy relaunch) — slugified project dir.
COLDCWD="$JAIL_ROOT/tmp/coldcwd"; mkdir -p "$COLDCWD"
COLDSLUG="$(printf '%s' "$COLDCWD" | sed 's,/,-,g')"
COLDPROJ="$HOME/.claude/projects/$COLDSLUG"; mkdir -p "$COLDPROJ"
{
  printf '{"type":"agent-name","agentName":"%scold5"}\n' "$JAIL_PREFIX"
  printf '{"type":"user","cwd":"%s","timestamp":"2026-06-01T10:00:00.000Z","message":{"role":"user","content":"hi"}}\n' "$COLDCWD"
} > "$COLDPROJ/$COLDSID.jsonl"

# --- S2: reject a traversal/injection zmx-name (no spawn). ---
out="$(jail_sb resume "$COLDSID" --zmx-name '../evil' 2>"$JAIL_ROOT/gl5s2.err")"; code=$?
echo "    resume --zmx-name '../evil' exit=$code"
sed 's/^/    err: /' "$JAIL_ROOT/gl5s2.err"
[ "$code" = "1" ] && ok "G-L5 S2 rejects bad zmx-name (exit 1)" || note_fail "G-L5 S2 exit=$code (expect 1)"
grep -q "unsafe characters" "$JAIL_ROOT/gl5s2.err" && ok "G-L5 S2 message present" || note_fail "G-L5 S2 message missing"

# --- F3: recorded cwd missing + no --cwd → clean error, never raw ENOENT. ---
# Cold session whose recorded `cwd` points at a dir that does NOT exist.
GONESID="gonecwd-gl5"
GONEPROJ="$HOME/.claude/projects/-gone-proj"; mkdir -p "$GONEPROJ"
{
  printf '{"type":"agent-name","agentName":"%sgone5"}\n' "$JAIL_PREFIX"
  printf '{"type":"user","cwd":"%s","timestamp":"2026-06-01T10:00:00.000Z","message":{"role":"user","content":"hi"}}\n' "$JAIL_ROOT/tmp/this-dir-was-deleted"
} > "$GONEPROJ/$GONESID.jsonl"
out="$(jail_sb resume "$GONESID" 2>"$JAIL_ROOT/gl5f3.err")"; code=$?
echo "    resume (missing cwd, no --cwd) exit=$code"
sed 's/^/    err: /' "$JAIL_ROOT/gl5f3.err"
[ "$code" = "1" ] && ok "G-L5 F3 exit 1" || note_fail "G-L5 F3 exit=$code (expect 1)"
if grep -q "recorded directory no longer exists" "$JAIL_ROOT/gl5f3.err" && grep -q -- "--cwd" "$JAIL_ROOT/gl5f3.err"; then
  ok "G-L5 F3 clean actionable error (not raw ENOENT)"
else
  note_fail "G-L5 F3 error not clean/actionable"
fi
# Belt: must NOT be a raw ENOENT / 'No such file or directory' spew.
grep -q "No such file or directory" "$JAIL_ROOT/gl5f3.err" && note_fail "G-L5 F3 leaked raw ENOENT" || ok "G-L5 F3 no raw ENOENT leak"

# --- happy relaunch (--no-attach detached + fake-claude). ---
export CLAUDE_BIN="$FAKE"
ZNAME="${JAIL_PREFIX}cold5"
# resume --no-attach launches detached via zmx, then event-waits for ready
# (boot.rs scan_for_name keys on the registry NAME == zmx name). The fake writes
# SBRG_FAKE_NAME as its registry name so the waiter finds it.
export SBRG_FAKE_NAME="$ZNAME"
out="$(jail_sb resume "$COLDSID" --no-attach --zmx-name "$ZNAME" 2>"$JAIL_ROOT/gl5ok.err")"; code=$?
unset SBRG_FAKE_NAME
echo "    resume --no-attach exit=$code out=[$out]"
sed 's/^/    err: /' "$JAIL_ROOT/gl5ok.err"
relaunched="$(jail_zmx list 2>/dev/null | grep -c "$ZNAME" || true)"
if [ "$relaunched" -ge 1 ]; then
  ok "G-L5 cold relaunch landed (zmx task present)"
else
  note_fail "G-L5 cold relaunch did not land (tasks=$relaunched)"
fi
# cleanup the relaunched session (belt + force).
if jail_assert_resolves_in_jail "$ZNAME" 2>/dev/null; then
  jail_sb kill --force "$ZNAME" >/dev/null 2>&1
else
  "$ZMX_BIN" kill "$ZNAME" --force >/dev/null 2>&1 || true
fi

# ===========================================================================
# G-L6: reconcile forged drift — I1 (dead-PID registry → tombstone) + I3 (orphan
#        wrapper, dead claude PID, no live entry → reap); I5 untouched-live row;
#        --dry-run mutates nothing; stray observed read-only.
# ===========================================================================
echo "--- G-L6: reconcile I1 + I3 + I5 + dry-run + stray ---"
# I1: a LIVE registry entry whose PID is dead (definitely-dead high PID). Robust
# (a forged registry file under jailed HOME — HOME-bounded, no /tmp tier).
DEADPID=4000001
while kill -0 "$DEADPID" 2>/dev/null; do DEADPID=$((DEADPID+1)); done
printf '{"pid":%d,"name":"%sdeadreg6","status":"idle","sessionId":"deadreg-gl6","cwd":"%s","startedAt":1700000000000,"updatedAt":1700000000000}\n' \
  "$DEADPID" "$JAIL_PREFIX" "$WORKDIR" > "$HOME/.claude/sessions/$DEADPID.json"
# I5: a LIVE session reconcile must NEVER touch. Robust (a live fake).
GL6LIVE="${JAIL_PREFIX}live6"
spawn_fake "$GL6LIVE" || note_fail "G-L6 could not spawn the live I5 session"
livepid="$(fake_pid_for "$GL6LIVE")"
# I3 orphan (BEST-EFFORT on macOS): a zmx wrapper whose claude PID is dead with no
# live registry entry. Real macOS zmx CLEANS a task once its command exits, so a
# persistent live-wrapper/dead-claude orphan is not reliably forgeable here — I3
# LIVE coverage is the Lima lane's (G-X1). We still attempt to leave an
# ended/unreachable task for the dry-run plan, but the I3 dry-run assertion below
# is INFORMATIONAL (the i3 LOGIC is hard-asserted by reconcile.rs unit tests).
GL6ORPH="${JAIL_PREFIX}orph6"
spawn_fake "$GL6ORPH"
orphpid="$(fake_pid_for "$GL6ORPH")"
# Remove ONLY the orphan's registry entry (match its exact pid file), leaving
# live6's entry intact, then kill the orphan's inner pid (never live6's).
if [ -n "$orphpid" ] && [ "$orphpid" != "$livepid" ]; then
  rm -f "$HOME/.claude/sessions/$orphpid.json"
  kill -9 "$orphpid" 2>/dev/null
fi
sleep 1

# HARD-STOP (reported + VERIFIED by sbr-pa5-lead2; orc-3 ruling pending):
# `qd reconcile` is NOT jail-hermetic. The bin verb sweeps
# legacy_zmx_dirs(env.uid(), canonical, [PathBuf::from("/tmp")], ..) — literal
# "/tmp" + the REAL uid (TS-pin-faithful: utils.ts:113 scanRoots default ["/tmp"]),
# ignoring the jailed TMPDIR/ZMX_DIR — so it scans the HOST's /tmp/claude-<uid>/
# zmx-<uid> and the REAL (non-dry) I3 path issues `zmx kill` against real host
# orphans. The binary is CORRECT per the pin + the A-track literal-/tmp Bug-D
# ruling; the gap is at the BELT level (sweep verbs cannot be per-target belted).
# Ruling: binary UNCHANGED, destructive-reconcile live coverage moves to the Lima
# lane (G-X1). On macOS we run --dry-run ONLY: dry-run SCANS but the verb's
# `if !dry_run` guards EVERY kill/tombstone, so it is read-only and cannot reach
# a real session destructively. I1/I3/I5 LOGIC is fully covered by the pure-decider
# unit tests (reconcile.rs i1/i3/i5 + the i5 negative-control harness).

# --dry-run: PLAN MY forged drift, mutate nothing. Assertions key on MY specific
# forged identities (sbrg- name / DEADPID), NOT a bare "tombstone:"/"reap-wrapper:"
# line (a real /tmp orphan in the host tier could otherwise false-pass).
out="$(jail_sb reconcile --dry-run 2>"$JAIL_ROOT/gl6dry.err")"; code=$?
echo "    reconcile --dry-run exit=$code"
echo "$out" | sed 's/^/    | /'
echo "$out" | grep -q "Would repair" && ok "G-L6 dry-run says 'Would repair'" || note_fail "G-L6 dry-run verb wrong"
# I1: MY dead-PID registry entry is planned for tombstone, byte-exact detail.
echo "$out" | grep -q "tombstone: ${JAIL_PREFIX}deadreg6 (pid $DEADPID dead)" \
  && ok "G-L6 I1 planned (my dead-PID entry, byte-exact detail)" \
  || note_fail "G-L6 I1 not in plan for pid $DEADPID"
# I3: MY orphan wrapper SHOULD be planned for reap — INFORMATIONAL on macOS (real
# zmx may have already cleaned the task; I3 LOGIC is hard-asserted by units, I3
# LIVE is the Lima lane). Never a hard fail here.
if echo "$out" | grep -q "reap-wrapper: zmx \"$GL6ORPH\""; then
  ok "G-L6 I3 planned (my orphan wrapper present in plan)"
else
  echo "  [info] G-L6 I3 orphan not in macOS dry-run plan (real zmx cleaned the task; I3 covered by units + Lima G-X1)"
fi
# dry-run mutated nothing: MY dead-PID registry entry is STILL live.
[ -f "$HOME/.claude/sessions/$DEADPID.json" ] && ok "G-L6 dry-run mutated nothing (I1 entry intact)" || note_fail "G-L6 dry-run tombstoned in dry mode"
# I5: MY live session must NOT appear anywhere in the plan.
echo "$out" | grep -q "$GL6LIVE" && note_fail "G-L6 I5 VIOLATION: plan names the live session" || ok "G-L6 I5 my live session absent from plan"
# I5: dry-run touched nothing → my live fake is still running.
if [ -n "$livepid" ] && kill -0 "$livepid" 2>/dev/null; then
  ok "G-L6 I5 my live pid untouched by dry-run"
else
  note_fail "G-L6 I5 my live pid died during dry-run"
fi
# Stray read-only discipline (carry 4 — TAKEOVER parked): NO adopt/takeover verb.
if echo "$out" | grep -Eq "adopt|takeover|seiz"; then
  note_fail "G-L6 stray: reconcile emitted an adopt/takeover verb (must be read-only)"
else
  ok "G-L6 stray discipline read-only (no adopt/takeover in output)"
fi

# G-L6c: SWEEP-BELT NEGATIVE CONTROL (orc-3 ruling — the belt MUST demonstrably
# BITE on a non-sbrg planned target). On brano the reconcile --dry-run plan reaches
# the literal /tmp tier and surfaces NON-sbrg host orphans (e.g. ended org tasks),
# so jail_sweep_belt_ok MUST refuse (return non-zero). To make the control robust
# even if the host /tmp tier happens to be empty of reapable orphans, we ALSO forge
# a non-sbrg ended task in the jail's OWN zmx dir so a non-sbrg reap target is
# guaranteed in the plan.
echo "--- G-L6c: sweep-belt negative control (must BITE on non-sbrg target) ---"
NONSBRG="intruder-not-jailed"      # deliberately NOT $JAIL_PREFIX-prefixed
# Spawn it in the jail zmx dir, then kill its inner pid so it ends → an ended task
# with a non-sbrg name appears in the RAW plan as a reap-wrapper target.
( cd "$WORKDIR" && "$ZMX_BIN" run "$NONSBRG" -d bash -lc "exec sleep 1" ) >/dev/null 2>&1
sleep 2   # let the inner `sleep 1` exit so the task is ended/unreachable
if jail_sweep_belt_ok reconcile 2>"$JAIL_ROOT/gl6c.err"; then
  note_fail "G-L6c sweep belt did NOT bite (a non-sbrg reap target should refuse)"
else
  ok "G-L6c sweep belt BIT (refused: non-sbrg reap target present)"
  grep -q "is NOT jail-prefixed\|does not resolve in the jailed zmx dir" "$JAIL_ROOT/gl6c.err" \
    && ok "G-L6c refusal names the offending target" \
    || echo "  [info] G-L6c refusal emitted (reason text varies by which host target tripped first)"
fi
# Cleanup the forged intruder (guarded raw kill is name-mismatched, so kill the
# zmx task directly in the jail dir — it is in OUR ZMX_DIR).
"$ZMX_BIN" kill "$NONSBRG" --force >/dev/null 2>&1 || true

# Real-reconcile row: OFF on macOS PERMANENTLY (orc-3 standing constraint). It runs
# ONLY in the Lima lane (G-X1), gated by ALL of: (1) jail_require_destructive_ok —
# the Lima sentinel /etc/qd-rust-lima + hostname!=brano + QD_RUST_DESTRUCTIVE_OK=1,
# which FAILS CLOSED on brano/macOS; (2) the sweep belt; (3) the explicit opt-in.
# On brano this branch is unreachable (the Lima gate alone refuses).
if jail_require_destructive_ok 2>/dev/null \
   && [ "${A5_L6_DESTRUCTIVE_OK:-0}" = "1" ] \
   && jail_sweep_belt_ok reconcile; then
  echo "    [Lima + sweep belt + opt-in] running REAL reconcile (Lima lane only)"
  out="$(jail_sb reconcile 2>"$JAIL_ROOT/gl6real.err")"; code=$?
  echo "$out" | sed 's/^/    | /'
  echo "$out" | grep -q "Repaired" && ok "G-L6 real says 'Repaired'" || note_fail "G-L6 real verb wrong"
  if [ ! -f "$HOME/.claude/sessions/$DEADPID.json" ] && [ -f "$HOME/.claude/sessions/$DEADPID.json.tombstoned" ]; then
    ok "G-L6 I1 applied (dead registry entry tombstoned)"
  else
    note_fail "G-L6 I1 not applied"
  fi
  orphafter="$(jail_zmx list 2>/dev/null | grep -c "$GL6ORPH" || true)"
  [ "$orphafter" = "0" ] && ok "G-L6 I3 applied (orphan reaped)" || note_fail "G-L6 I3 orphan survived"
  liveafter="$(jail_zmx list 2>/dev/null | grep -c "$GL6LIVE" || true)"
  if [ "$liveafter" = "1" ] && [ -n "$livepid" ] && kill -0 "$livepid" 2>/dev/null; then
    ok "G-L6 I5 honored (live untouched)"
  else
    note_fail "G-L6 I5 VIOLATION (live tasks=$liveafter)"
  fi
else
  echo "    [skipped] real reconcile — GATED (macOS /tmp-tier hermeticity gap; Lima lane owns destructive reconcile)"
fi

# cleanup the live + orphan + any stragglers (belt + force).
if jail_assert_resolves_in_jail "$GL6LIVE" 2>/dev/null; then
  jail_sb kill --force "$GL6LIVE" >/dev/null 2>&1
fi
[ -n "$livepid" ] && kill -9 "$livepid" 2>/dev/null
[ -n "$orphpid" ] && kill -9 "$orphpid" 2>/dev/null

# ===========================================================================
# Real-home invisibility belt (rule 9): org session count unchanged.
# ===========================================================================
REAL_AFTER="$(ls "$JAIL_REAL_HOME/.claude/sessions" 2>/dev/null | wc -l | tr -d ' ')"
echo "--- real-home belt: $REAL_BEFORE -> $REAL_AFTER ---"
[ "$REAL_BEFORE" = "$REAL_AFTER" ] && ok "real-home untouched ($REAL_BEFORE)" || note_fail "real-home DRIFT ($REAL_BEFORE -> $REAL_AFTER)"

echo "==========================================================="
if [ "$PASS_ALL" = "1" ]; then
  echo "A5-LIFECYCLE-LIVE: PASS"
  exit 0
else
  echo "A5-LIFECYCLE-LIVE: FAIL"
  exit 1
fi
