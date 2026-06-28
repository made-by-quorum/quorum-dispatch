#!/usr/bin/env bash
# test/golden/scenarios/_a5_lifecycle_lib.sh — shared helpers for the A5 G-REC
# lifecycle recording scenarios (kill / gc / reconcile / resume / config / ping).
#
# These scenarios SELF-RECORD the pinned TS engine's output shapes (pin 0d0fa9e)
# into the golden oracle. record.sh establishes the jail + sets SCN_OUT before
# sourcing the scenario; each scenario forges its pre-state INSIDE the jail (so
# every byte is hermetic) and drives the TS qd via scn_qd (QD_UNDER_TEST points
# at `bun <pinned-clone>/src/index.ts`). Bash 3.2 floor.
#
# RECORDING RULES honored here (binding):
#   - rule 9 + ADD-4: every TS invocation runs under the jail's hermetic
#     HOME/QD_HOME/ZMX_DIR/XDG_*/TMPDIR (record.sh::jail_establish exports them).
#   - qdrg- prefixed session names only (JAIL_PREFIX).
#   - QD_SECRET_BACKEND=file for config rows (NO keychain — daytime-deferred).
#   - NEVER a real secret: the fake OpenRouter placeholder is BELOW the real-key
#     length anchor so the L11 secret-scan admit gate passes.
#   - ADD-12: destructive `qd reconcile` (no --dry-run) is OFF on macOS; the
#     reconcile rows here are --dry-run ONLY (read-only: the verb's dry-run guard
#     blocks every kill/tombstone). The non-dry `Repaired` shape is LIMA-DEFERRED.
#   - kill-live: jail_assert_resolves_in_jail pre-asserts before the destructive run.
# ---------------------------------------------------------------------------

. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

# The OBVIOUSLY-FAKE OpenRouter placeholder. 9 chars after `sk-or-` — BELOW the
# 20-char real-key anchor the secret-scan gate (and qd-qa safety.sh) enforce, so
# recording `config get --reveal` of this value does NOT trip the admit gate.
A5_FAKE_OPENROUTER_KEY='sk-or-FAKE-0000'

# a5_zmx — the real zmx binary under the jail env (ZMX_DIR is jailed).
A5_ZMX="${ZMX_BIN:-$(command -v zmx 2>/dev/null || echo /opt/homebrew/bin/zmx)}"

# a5_make_fake_claude — write a fake-claude into the jail that records a PID-keyed
# registry entry then sleeps. The registry NAME is what kill/ping/resume key on.
# Echoes the fake binary path.
a5_make_fake_claude() {
    local fake="$JAIL_ROOT/fake-claude"
    cat > "$fake" <<'EOS'
#!/bin/bash
name=""
while [ $# -gt 0 ]; do case "$1" in --name) name="$2"; shift 2;; *) shift;; esac; done
[ -n "${QDRG_FAKE_NAME:-}" ] && name="$QDRG_FAKE_NAME"
mkdir -p "$HOME/.claude/sessions"
printf '{"pid":%d,"name":"%s","status":"idle","sessionId":"fake-%d","cwd":"%s","version":"fake","kind":"interactive","entrypoint":"cli","startedAt":1700000000000,"updatedAt":1700000000000}\n' \
  "$$" "$name" "$$" "$PWD" > "$HOME/.claude/sessions/$$.json"
exec sleep 120
EOS
    chmod +x "$fake"
    printf '%s' "$fake"
}

# a5_spawn_fake <name> — spawn a live jailed qdrg- session via the REAL zmx + the
# fake-claude (NOT `qd new` — ADD-10a banned). Polls until the registry entry +
# zmx task land (≤8s). Returns 0 on success.
a5_spawn_fake() {
    local name="$1" fake="$JAIL_ROOT/fake-claude" work="$JAIL_ROOT/tmp/work" i=0
    [ -f "$fake" ] || fake="$(a5_make_fake_claude)"
    mkdir -p "$work"
    ( cd "$work" && HOME="$HOME" ZMX_DIR="$ZMX_DIR" TMPDIR="$TMPDIR" \
        "$A5_ZMX" run "$name" -d bash -lc "QDRG_FAKE_NAME='$name' '$fake' --name '$name'" ) >/dev/null 2>&1
    while [ "$i" -lt 40 ]; do
        if [ -n "$(a5_fake_pid_for "$name")" ] \
           && [ "$(HOME="$HOME" ZMX_DIR="$ZMX_DIR" TMPDIR="$TMPDIR" "$A5_ZMX" list 2>/dev/null | grep -c "$name" || true)" -ge 1 ]; then
            return 0
        fi
        i=$((i+1)); sleep 0.2
    done
    return 1
}

# a5_fake_pid_for <name> — the recorded claude(=sleep) PID for a session name.
a5_fake_pid_for() {
    local name="$1" f pid
    for f in "$HOME/.claude/sessions"/*.json; do
        [ -f "$f" ] || continue
        case "$(cat "$f" 2>/dev/null)" in
            *"\"name\":\"$name\""*)
                pid="$(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$f")"
                printf '%s' "$pid"; return 0;;
        esac
    done
    printf ''
}

# a5_forge_registry <name> <status> <turns> <pid> [age_s] — write a forged registry
# entry with chosen status/turns and timestamps anchored <age_s> seconds ago
# (default 60). Used to record ping classifications deterministically.
a5_forge_registry() {
    local name="$1" status="$2" turns="$3" pid="$4" age="${5:-60}"
    local now_ms started_ms
    now_ms=$(( $(date +%s) * 1000 ))
    started_ms=$(( now_ms - age * 1000 ))
    mkdir -p "$HOME/.claude/sessions"
    printf '{"pid":%d,"name":"%s","status":"%s","sessionId":"forge-%s","cwd":"%s","version":"fake","kind":"interactive","entrypoint":"cli","turns":%d,"startedAt":%d,"updatedAt":%d}\n' \
        "$pid" "$name" "$status" "$name" "$JAIL_ROOT/tmp" "$turns" "$started_ms" "$now_ms" \
        > "$HOME/.claude/sessions/$pid.json"
}
