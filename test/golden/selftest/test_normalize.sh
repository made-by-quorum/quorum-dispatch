#!/usr/bin/env bash
# test/golden/selftest/test_normalize.sh — unit tests for the normalizers.
#
# Each normalization rule gets TWO assertions per the ADR contract:
#   (a) it COLLAPSES the noise it targets, AND
#   (b) it PRESERVES load-bearing bytes (exit codes, alt-screen, CR vs LF,
#       backlog content/order, coordinates, JSON contract).
#
# Bash 3.2 / POSIX-tool floor. Run directly: test/golden/selftest/test_normalize.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../lib/normalize.sh
. "$HERE/../lib/normalize.sh"

PASS=0
FAIL=0

# eq <name> <expected> <actual>
eq() {
    local name="$1" exp="$2" act="$3"
    if [ "$exp" = "$act" ]; then
        PASS=$((PASS + 1))
        printf 'ok   %s\n' "$name"
    else
        FAIL=$((FAIL + 1))
        printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$name" "$exp" "$act"
    fi
}

# Helper: run a filter, return cat -v of result (so escapes are visible/comparable).
visible() { cat -v; }

# --- timestamps -------------------------------------------------------------
# (a) collapses ISO-8601, clock-with-frac, and epoch-ms
eq "ts/iso-collapse" \
    "<TS>" \
    "$(printf '2026-06-04T01:15:40.797564000Z\n' | normalize_timestamps | tr -d '\n')"
eq "ts/clock-collapse" \
    "DLINE 19 <TS>" \
    "$(printf 'DLINE 19 01:15:40.797564000\n' | normalize_timestamps | tr -d '\n')"
eq "ts/epoch-collapse" \
    "ts=<TS>" \
    "$(printf 'ts=1717460140797\n' | normalize_timestamps | tr -d '\n')"
# (b) preserves backlog index, coordinates, longer/shorter numbers
eq "ts/preserve-coords" \
    "rows=24 cols=80 idx=7" \
    "$(printf 'rows=24 cols=80 idx=7\n' | normalize_timestamps | tr -d '\n')"
eq "ts/preserve-14digit" \
    "not12345678901234" \
    "$(printf 'not12345678901234\n' | normalize_timestamps | tr -d '\n')"

# --- pids -------------------------------------------------------------------
# (a) collapses labeled pids and <pid>.json registry files
eq "pid/label-collapse" \
    "pid=<PID> (pid <PID>)" \
    "$(printf 'pid=4242 (pid 99)\n' | normalize_pids | tr -d '\n')"
eq "pid/json-collapse" \
    "/var/qd/<PID>.json" \
    "$(printf '/var/qd/12345.json\n' | normalize_pids | tr -d '\n')"
# (b) preserves bare numbers (coordinates, counts) — NOT every int is a pid
eq "pid/preserve-bare" \
    "rows 24 cols 80 lines 3" \
    "$(printf 'rows 24 cols 80 lines 3\n' | normalize_pids | tr -d '\n')"

# --- paths ------------------------------------------------------------------
JR="/tmp/sbrg-runs/abc123"
# (a) collapses each jail subdir to its token
eq "path/zmx-collapse" \
    "<ZMX_DIR>/sock" \
    "$(printf '%s/zmx/sock\n' "$JR" | normalize_paths "$JR" | tr -d '\n')"
eq "path/sbhome-collapse" \
    "<SB_HOME>/x" \
    "$(printf '%s/sb_home/x\n' "$JR" | normalize_paths "$JR" | tr -d '\n')"
eq "path/xdg-runtime-collapse" \
    "<XDG_RUNTIME>/r" \
    "$(printf '%s/xdg_runtime/r\n' "$JR" | normalize_paths "$JR" | tr -d '\n')"
# (b) preserves the resolution-order suffix (e.g. zmx-<uid>) — structure intact
eq "path/preserve-suffix" \
    "<ZMX_DIR>/zmx-501/sess" \
    "$(printf '%s/zmx/zmx-501/sess\n' "$JR" | normalize_paths "$JR" | tr -d '\n')"
# (b2) a non-jail path is untouched
eq "path/preserve-foreign" \
    "/usr/local/bin/zmx" \
    "$(printf '/usr/local/bin/zmx\n' | normalize_paths "$JR" | tr -d '\n')"

# --- runids + port ----------------------------------------------------------
# (a) collapses session prefix, bare runid, and relay port
eq "runid/prefix-collapse" \
    "sbrg-<RUNID>-mysess" \
    "$(printf 'sbrg-abc123-mysess\n' | normalize_runids abc123 34567 | tr -d '\n')"
eq "runid/port-collapse" \
    "port <RELAY_PORT> end" \
    "$(printf 'port 34567 end\n' | normalize_runids abc123 34567 | tr -d '\n')"
# (b) preserves a longer number that merely contains the port digits
eq "runid/preserve-superstring" \
    "keep345670" \
    "$(printf 'keep345670\n' | normalize_runids abc123 34567 | tr -d '\n')"

# --- ansi chunks (the dangerous one) ----------------------------------------
ESC=$(printf '\033')
# (a) collapses a run of redundant SGR resets to one
eq "ansi/collapse-resets" \
    "^[[0mTEXT" \
    "$(printf '%s[0m%s[0m%s[0mTEXT\n' "$ESC" "$ESC" "$ESC" | normalize_ansi_chunks | visible | tr -d '\n')"
# (b1) PRESERVES alt-screen sequences (must NEVER be collapsed/erased)
eq "ansi/preserve-altscreen" \
    "^[[?1049h^[[0m" \
    "$(printf '%s[?1049h%s[0m\n' "$ESC" "$ESC" | normalize_ansi_chunks | visible | tr -d '\n')"
# (b2) PRESERVES a single reset (no over-collapse)
eq "ansi/preserve-single-reset" \
    "A^[[0mB" \
    "$(printf 'A%s[0mB\n' "$ESC" | normalize_ansi_chunks | visible | tr -d '\n')"
# (b3) PRESERVES cursor-move sequences with coordinates
eq "ansi/preserve-cursor" \
    "^[[12;34H" \
    "$(printf '%s[12;34H\n' "$ESC" | normalize_ansi_chunks | visible | tr -d '\n')"

# --- CR vs LF (load-bearing distinction must survive the FULL pipeline) ------
# A CRLF line and an LF line must remain distinguishable after normalize_all.
cr_lf_out="$(printf 'a\r\nb\n' | normalize_all "$JR" abc123 34567 | cat -v | tr '\n' '|')"
eq "crlf/preserved-through-pipeline" "a^M|b|" "$cr_lf_out"

# --- JSON contract fields are never normalized ------------------------------
# A contract JSON line with a status field passes through unchanged (no pid/ts/path
# tokens present), proving normalize_all does not eat contract values.
eq "json/contract-preserved" \
    '{"name":"work","status":"busy","clients":0}' \
    "$(printf '{\"name\":\"work\",\"status\":\"busy\",\"clients\":0}\n' | normalize_all "$JR" abc123 34567 | tr -d '\n')"

# --- P6a: port CONTEXT-GUARD (gemini false-green) ---------------------------
# (a) collapses the port ONLY in port-bearing contexts: JSON, URL, labeled.
eq "p6a/port-json-collapse" \
    '{"port": <RELAY_PORT>}' \
    "$(printf '{\"port\": 34567}\n' | normalize_runids '' 34567 | tr -d '\n')"
eq "p6a/port-json-key-suffix-collapse" \
    '{"relayPort":<RELAY_PORT>}' \
    "$(printf '{\"relayPort\":34567}\n' | normalize_runids '' 34567 | tr -d '\n')"
eq "p6a/port-url-collapse" \
    "http://127.0.0.1:<RELAY_PORT>/health" \
    "$(printf 'http://127.0.0.1:34567/health\n' | normalize_runids '' 34567 | tr -d '\n')"
eq "p6a/port-host-collapse" \
    "localhost:<RELAY_PORT>" \
    "$(printf 'localhost:34567\n' | normalize_runids '' 34567 | tr -d '\n')"
eq "p6a/port-labeled-collapse" \
    "port=<RELAY_PORT> up" \
    "$(printf 'port=34567 up\n' | normalize_runids '' 34567 | tr -d '\n')"
# (b) a BARE integer coincidentally equal to the port SURVIVES (the false-green:
#     a buggy count/index emitting the port value must NOT be scrubbed to green).
eq "p6a/preserve-bare-count" \
    "count=34567 items" \
    "$(printf 'count=34567 items\n' | normalize_runids '' 34567 | tr -d '\n')"
eq "p6a/preserve-bare-lines" \
    "lines 34567" \
    "$(printf 'lines 34567\n' | normalize_runids '' 34567 | tr -d '\n')"

# --- P6b: clock-with-frac CONTEXT-GUARD (deepseek finding) ------------------
# (a) a genuine timestamp clock still collapses (DLINE log line / line-leading).
eq "p6b/clock-timestamp-collapse" \
    "DLINE 19 <TS>" \
    "$(printf 'DLINE 19 01:15:40.797564000\n' | normalize_timestamps | tr -d '\n')"
# (b) a DURATION/elapsed VALUE matching the same shape SURVIVES (load-bearing —
#     erasing it would blind the oracle to a timing regression printed as a value).
eq "p6b/preserve-duration" \
    "duration=01:02:03.500000 elapsed" \
    "$(printf 'duration=01:02:03.500000 elapsed\n' | normalize_timestamps | tr -d '\n')"
eq "p6b/preserve-elapsed-label" \
    "elapsed: 00:00:12.345678" \
    "$(printf 'elapsed: 00:00:12.345678\n' | normalize_timestamps | tr -d '\n')"
eq "p6b/preserve-took-label" \
    "took 01:23:45.678901 done" \
    "$(printf 'took 01:23:45.678901 done\n' | normalize_timestamps | tr -d '\n')"

# --- P6c: HOST_TMP distinctness (gpt jail-escape finding) -------------------
# (a) an UNJAILED /tmp path -> <HOST_TMP> (DISTINCT from the jailed <TMPDIR>), so a
#     jail-escape can NEVER normalize into the same token a hermetic golden expects.
eq "p6c/unjailed-tmp-host-token" \
    "wrote <HOST_TMP>/escape/sock" \
    "$(printf 'wrote /tmp/escape/sock\n' | normalize_paths '' | tr -d '\n')"
# (a2) jailed tmp -> <TMPDIR>, an unjailed /tmp on the SAME line -> <HOST_TMP>:
#      the two are kept DISTINCT (escape stays visible next to the hermetic path).
eq "p6c/jailed-vs-host-distinct" \
    "j=<TMPDIR>/x h=<HOST_TMP>/y" \
    "$(printf 'j=%s/tmp/x h=/tmp/y\n' "$JR" | normalize_paths "$JR" | tr -d '\n')"
# (b) a word merely CONTAINING 'tmp' (foo_tmp, /xtmp) is NOT a host /tmp root.
eq "p6c/preserve-tmp-substring" \
    "name=foo_tmp v=/xtmp/z" \
    "$(printf 'name=foo_tmp v=/xtmp/z\n' | normalize_paths '' | tr -d '\n')"

# --- P7: <ZMX_UID> token preserving uid-correctness (grok + uid-501 NIT) -----
# (a) the LIVE test uid in a zmx-<uid> path component (and the uid= field) -> the
#     <ZMX_UID> token, so the row is host-portable.
eq "p7/live-uid-zmx-collapse" \
    "<TMPDIR>/dup/zmx-<ZMX_UID> uid=<ZMX_UID>" \
    "$(printf '<TMPDIR>/dup/zmx-501 uid=501\n' | normalize_zmx_uid 501 | tr -d '\n')"
# (b) a WRONG uid (hard-coded zmx-0 / zmx-999999) SURVIVES and therefore DIFFS
#     against a <ZMX_UID>-expecting golden — the false-green the NIT names is closed.
eq "p7/wrong-uid-zero-survives" \
    "zmx-0 uid=0" \
    "$(printf 'zmx-0 uid=0\n' | normalize_zmx_uid 501 | tr -d '\n')"
eq "p7/wrong-uid-big-survives" \
    "zmx-999999 uid=999999" \
    "$(printf 'zmx-999999 uid=999999\n' | normalize_zmx_uid 501 | tr -d '\n')"
# (b2) a uid that merely SHARES a digit-prefix with the live uid SURVIVES (the
#      explicit non-digit boundary: live=501 must not partial-match zmx-5010).
eq "p7/digit-prefix-share-survives" \
    "zmx-5010 uid=5010" \
    "$(printf 'zmx-5010 uid=5010\n' | normalize_zmx_uid 501 | tr -d '\n')"

printf '\n--- test_normalize: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
