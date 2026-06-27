#!/usr/bin/env bash
# test/golden/selftest/test_scan_secrets.sh — prove the secret-scan gate.
#
# Two halves:
#   (1) DETECTION: each pattern in the broadened set, planted in a scratch
#       capture, is DETECTED (scan returns non-zero).
#   (2) FALSE-POSITIVE: tokenized fixture content (<TS>/<PID>/<RELAY_PORT>/…) and
#       the existing committed dryrun captures scan CLEAN (return zero).
#
# Bash 3.2 floor. Run directly. Uses a scratch dir under TMPDIR; cleans up.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"   # test/golden
# A7: scan_secrets.sh consolidated into secret-scan.sh; the sourced API names
# (scan_secrets_text / scan_secrets_path) are preserved verbatim.
. "$ROOT/lib/secret-scan.sh"

PASS=0
FAIL=0
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/scan-selftest.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT INT TERM

# detects <name> <content> : plant content in a scratch file, expect a HIT.
detects() {
    local name="$1" content="$2"
    local f="$SCRATCH/plant-$name"
    printf '%s\n' "$content" > "$f"
    if scan_secrets_text "$f"; then
        # return 0 = clean = MISSED the secret
        FAIL=$((FAIL + 1)); printf 'FAIL detect/%s — secret NOT detected\n' "$name"
    else
        PASS=$((PASS + 1)); printf 'ok   detect/%s (caught)\n' "$name"
    fi
}

# clean <name> <content> : plant benign content, expect CLEAN (no hit).
clean() {
    local name="$1" content="$2"
    local f="$SCRATCH/clean-$name"
    printf '%s\n' "$content" > "$f"
    if scan_secrets_text "$f"; then
        PASS=$((PASS + 1)); printf 'ok   clean/%s (no false positive)\n' "$name"
    else
        FAIL=$((FAIL + 1)); printf 'FAIL clean/%s — FALSE POSITIVE (benign flagged)\n' "$name"
    fi
}

# --- 1. DETECTION: each broadened pattern planted ----------------------------
# Realistic-shape FAKE keys (random-looking; NOT real credentials).
detects "openrouter" "OPENROUTER_API_KEY=sk-or-v1-0a1B2c3D4e5F6g7H8i9J0kLmNoPqRsTuVwXyZ012345"
detects "anthropic"  "key: sk-ant-api03-Ab12Cd34Ef56Gh78Ij90Kl12Mn34Op56Qr78St90"
detects "github-pat" "token ghp_Ab1Cd2Ef3Gh4Ij5Kl6Mn7Op8Qr9St0Uv1Wx2Yz3A"
detects "github-oauth" "gho_0A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0U"
detects "aws-akia"   "aws_access_key_id = AKIAIOSFODNN7EXAMPLE"
detects "jwt"        "Authorization: Bearer eyJhbGciOiJIUzI1NiI.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36"
detects "high-entropy" "blob=Zk9Qm2Xa7Bd4Lp1Rn8Vc3Tg6Wj5Yh0Fs2Du4Ie7Ko9Mq1"

# --- 2. FALSE-POSITIVE: tokenized fixture content ----------------------------
clean "tokens" "session sbrg-<RUNID>-sess pid=<PID> port <RELAY_PORT> at <TS> under <SB_HOME>/x"
clean "json-empty" '{"sessions":[],"version":1}'
clean "help-text" "Usage: qd ls [--json] [--short]   list sessions in the registry"
clean "hex-sha"   "commit 0d0fa9ed4800efb1309eca2311345c48af2c4932 (lowercase hex, no mix)"
clean "path"      "/home/u/work/qd-rust/.claude/worktrees/agent-a4a08f94818f5f3fb/test"
clean "doc-prefix" "We scan for sk-or- and sk-ant- prefixes plus AKIA AWS keys and ghp_ tokens."

# --- 3. FALSE-POSITIVE: the existing committed dryrun captures ----------------
# These are REAL captured (tokenized) fixture content — the gate must pass them,
# else it would block every legitimate fixture from being admitted.
CAPDIR="$ROOT/dryrun/captures"
if [ -d "$CAPDIR" ]; then
    if scan_secrets_path "$CAPDIR"; then
        PASS=$((PASS + 1)); printf 'ok   clean/dryrun-captures (existing captures scan clean)\n'
    else
        FAIL=$((FAIL + 1)); printf 'FAIL clean/dryrun-captures — a committed capture was flagged\n'
    fi
else
    printf 'skip clean/dryrun-captures — %s not present\n' "$CAPDIR"
fi

# --- 4. DIR-mode detection: a planted secret in a dir tree is caught ----------
mkdir -p "$SCRATCH/tree/sub"
printf 'clean line\n' > "$SCRATCH/tree/ok.txt"
printf 'leak sk-or-v1-DEADBEEFdeadbeef0123456789ABCDEF\n' > "$SCRATCH/tree/sub/bad.txt"
if scan_secrets_path "$SCRATCH/tree"; then
    FAIL=$((FAIL + 1)); printf 'FAIL detect/dir-mode — planted secret in tree NOT caught\n'
else
    PASS=$((PASS + 1)); printf 'ok   detect/dir-mode (planted secret in tree caught)\n'
fi

printf '\n--- test_scan_secrets: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
