#!/usr/bin/env bash
# test/golden/lib/secret-scan.sh — the CONSOLIDATED secret-scan gate (A7).
#
# History: until A7 the corpus carried TWO secret scanners that landed on
# independent branches —
#   * secret-scan.sh   (A5/L11 carrier): exit-based CLI + sourced secret_scan_*,
#       broad pattern set incl. a flat-hex catch; wired into record.sh's
#       capture-content belt (raw/+normalized only) with the exit-70 wrong-pin
#       gate upstream.
#   * scan_secrets.sh  (A2-close/0b lineage): return-based sourced scan_secrets_*,
#       corpus-SAFE pattern set deliberately tuned NOT to flag tokenized fixtures
#       or 40-hex pin SHAs; wired into fixture_admit.sh's whole-staging scan.
# A7 converges them into THIS one module (the red-team CI-fixture-scan carry).
#
# A recorded oracle is a CHECKED-IN artifact: it must never embed a real
# credential. This gate scans a fixture tree (or a single file / stdin) for
# secret-shaped strings and FAILS CLOSED on any hit, so no fixture commits
# unscanned.
#
# DUAL API (both legacy families preserved verbatim so no caller had to change
# its call SHAPE — only sourcing lines were re-pointed at this file):
#   sourced, RETURN-based (never exits the caller — fixture_admit composes it):
#     scan_secrets_text  <file|->     scan ONE file (or stdin "-"); return 1 on hit.
#     scan_secrets_path  <file|dir|-> scan a file, a dir (recursive), or stdin.
#   sourced, RETURN-based, A5 names (record.sh belt + its selftest call these):
#     secret_scan_path   <path|->     alias of scan_secrets_path.
#     secret_scan_selftest            negative-control selftest (plant bites, clean passes).
#   executed, EXIT-based CLI (BASH_SOURCE dual mode):
#     secret-scan.sh <path|->         run the gate; exit 0 clean / 1 dirty.
#     secret-scan.sh --strict <path>  add the strict tier (see below).
#     secret-scan.sh --selftest       run the negative-control selftest.
#
# ---------------------------------------------------------------------------
# PATTERN UNION + the ONE semantics conflict (A7, documented — not silently picked)
# ---------------------------------------------------------------------------
# The union of the two scanners' anchored key-shape patterns is the DEFAULT
# (corpus-safe) set below. Where the two differed, the BROADER survivor was kept:
#   * GitHub: gh[posur]_  {p,o,u,s,r} == gh[pousr]_  (same 5-char class, reordered);
#            threshold 30 (scan_secrets) kept over 36 (secret-scan) — broader catch.
#   * AWS:    (AKIA|ASIA) (secret-scan) kept over AKIA-only (scan_secrets) — ASIA added.
#   * JWT eyJ<seg>.<seg>.<seg>  — only scan_secrets had it; carried into the union.
#   * sk-generic, Slack xox, Google AIza, Bearer header, PEM block — only
#            secret-scan had these; carried into the union.
#   * High-entropy awk rule (upper AND lower AND digit, >=40) — scan_secrets';
#            kept as the corpus-safe fallback (does NOT fire on flat hex).
#
# *** CONFLICT (the only one): the flat-hex pattern [0-9a-fA-F]{40,} ***
#   secret-scan.sh FLAGGED any 40+ hex run; scan_secrets.sh deliberately PASSED
#   lowercase 40-hex (its clean/hex-sha selftest asserts a commit SHA passes, and
#   53 committed fixture metadata files — RECORDED-FROM / MATCH-PROOF — legitimately
#   carry 40-hex pin SHAs). A blind union (flat-hex always on) would (a) break
#   test_scan_secrets clean/hex-sha, (b) make fixture_admit refuse every real
#   recorded set, and (c) make the new CI fixtures-scan fail closed on benign pins.
#   RESOLUTION: flat-hex is a STRICT-TIER pattern, OFF by default, opt-in via
#   SECRET_SCAN_STRICT=1 (env) or --strict (CLI). It is SAFE on capture content
#   (raw/+normalized hold ZERO 40-hex runs — verified A7) so record.sh's belt opts
#   IN to preserve the A5 aggressive intent; the whole-tree and CI fixtures scans
#   use the default corpus-safe set. NO pattern was dropped — flat-hex is gated, not lost.
# ---------------------------------------------------------------------------
#
# Bash 3.2 floor (macOS CI): no associative arrays, no ${var,,}, no mapfile. grep -E
# only (BSD + GNU portable; no \b, no PCRE).
# ---------------------------------------------------------------------------

# --- the DEFAULT (corpus-safe) anchored key-shape set ------------------------
# One grep -E alternation for a single pass. Each branch is length/charset
# anchored so a bare prefix used as documentation (e.g. "sk-or-" in a comment,
# or "AKIA" in prose) is NOT flagged — only real-key-shaped matches trip.
_SS_RE_OPENROUTER='sk-or-[A-Za-z0-9_-]{20,}'
_SS_RE_ANTHROPIC='sk-ant-[A-Za-z0-9_-]{20,}'
# Generic OpenAI-style sk- + 32+ base62 (NOT sk-or-/sk-ant-, anchored above).
_SS_RE_SK_GENERIC='sk-[A-Za-z0-9]{32,}'
# GitHub tokens: gh[posur]_ + 30+ base62 (union: broader 30 threshold).
_SS_RE_GITHUB='gh[posur]_[A-Za-z0-9]{30,}'
# AWS access key id: AKIA/ASIA + 16 uppercase/digits (union: ASIA carried).
_SS_RE_AWS='(AKIA|ASIA)[A-Z0-9]{16}'
# Slack tokens: xox[baprs]- + token body.
_SS_RE_SLACK='xox[baprs]-[A-Za-z0-9-]{10,}'
# Google API key: AIza + 35 url-safe chars.
_SS_RE_GOOGLE='AIza[A-Za-z0-9_-]{35}'
# JWT: eyJ<b64url>.<b64url>.<b64url> (3 dotted segments).
_SS_RE_JWT='eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}'
# Authorization: Bearer <token> headers (any token of 16+ chars).
_SS_RE_BEARER='[Aa]uthorization:[[:space:]]*[Bb]earer[[:space:]]+[A-Za-z0-9._-]{16,}'
# PEM private-key blocks.
_SS_RE_PEM='-----BEGIN [A-Z ]*PRIVATE KEY-----'

# Default alternation (corpus-safe: NO flat-hex).
_SS_RE_DEFAULT="${_SS_RE_OPENROUTER}|${_SS_RE_ANTHROPIC}|${_SS_RE_SK_GENERIC}|${_SS_RE_GITHUB}|${_SS_RE_AWS}|${_SS_RE_SLACK}|${_SS_RE_GOOGLE}|${_SS_RE_JWT}|${_SS_RE_BEARER}|${_SS_RE_PEM}"

# --- STRICT tier (opt-in) ----------------------------------------------------
# Flat-hex: a 40+ hex run. CONFLICTS with corpus pin-SHA metadata, so OFF by
# default; opt in with SECRET_SCAN_STRICT=1 or the CLI --strict flag.
_SS_RE_HEX='[0-9a-fA-F]{40,}'

# _ss_active_re — echo the alternation in force (default, +flat-hex if strict).
_ss_active_re() {
    if [ "${SECRET_SCAN_STRICT:-0}" = "1" ]; then
        printf '%s|%s' "$_SS_RE_DEFAULT" "$_SS_RE_HEX"
    else
        printf '%s' "$_SS_RE_DEFAULT"
    fi
}

# --- masking -----------------------------------------------------------------
# Mask any matched secret so the gate's own diagnostic output never leaks the
# value (keep a short scheme prefix, then ***).
_ss_mask() {
    sed -E \
        -e 's/(sk-or-)[A-Za-z0-9_-]{4}[A-Za-z0-9_-]*/\1****<REDACTED>/g' \
        -e 's/(sk-ant-)[A-Za-z0-9_-]{4}[A-Za-z0-9_-]*/\1****<REDACTED>/g' \
        -e 's/(sk-)[A-Za-z0-9]{4}[A-Za-z0-9]{12,}/\1****<REDACTED>/g' \
        -e 's/(gh[posur]_)[A-Za-z0-9]{4}[A-Za-z0-9]*/\1****<REDACTED>/g' \
        -e 's/(AKIA|ASIA)[A-Z0-9]{12,}/\1****<REDACTED>/g' \
        -e 's/(AIza)[A-Za-z0-9_-]{31,}/\1****<REDACTED>/g' \
        -e 's/(eyJ[A-Za-z0-9_-]{4})[A-Za-z0-9_.-]+/\1***/g' \
        -e 's/([Bb]earer[[:space:]]+)[A-Za-z0-9._-]{4}[A-Za-z0-9._-]*/\1****<REDACTED>/g' \
        -e 's/[0-9a-fA-F]{40,}/<REDACTED-HEX>/g'
}

# --- single-file scan --------------------------------------------------------
# _ss_scan_one <file> — print masked hits to stderr; return 1 if any, else 0.
# Pass 1: anchored key shapes (+ flat-hex when strict). Pass 2: high-entropy awk.
_ss_scan_one() {
    local f="$1"
    local hit=0
    local re
    re="$(_ss_active_re)"

    if grep -Eq "$re" "$f" 2>/dev/null; then
        printf '[secret-scan] SECRET-SHAPED STRING in: %s\n' "$f" >&2
        grep -En "$re" "$f" 2>/dev/null | _ss_mask >&2
        hit=1
    fi

    # Pass 2: conservative high-entropy catch. Tokenize on non-key chars, then
    # for each token require length >= 40 AND upper AND lower AND digit. This
    # avoids flat hex hashes (lowercase => no upper), tokenized fixtures, prose.
    if awk '
        {
            gsub(/[^A-Za-z0-9_-]/, " ")
            m = split($0, w, " ")
            for (i = 1; i <= m; i++) {
                t = w[i]
                if (length(t) < 40) continue
                hasU = (t ~ /[A-Z]/)
                hasL = (t ~ /[a-z]/)
                hasD = (t ~ /[0-9]/)
                if (hasU && hasL && hasD) {
                    pre = substr(t, 1, 6)
                    printf("[secret-scan] SECRET (high-entropy) in: %s: %s***\n", FILENAME, pre) > "/dev/stderr"
                    found = 1
                }
            }
        }
        END { exit (found ? 1 : 0) }
    ' "$f"; then
        : # awk exit 0 = no high-entropy hit
    else
        hit=1
    fi

    return "$hit"
}

# --- public: RETURN-based sourced API ----------------------------------------
# scan_secrets_text <file|-> : scan ONE file (or stdin via "-"). Return 1 on hit.
scan_secrets_text() {
    local target="$1"
    if [ "$target" = "-" ]; then
        local tmp
        tmp="$(mktemp)"
        cat > "$tmp"
        if _ss_scan_one "$tmp"; then
            rm -f "$tmp"; return 0
        fi
        rm -f "$tmp"; return 1
    fi
    if [ ! -f "$target" ]; then
        printf '[secret-scan] scan_secrets_text: not a file: %s\n' "$target" >&2
        return 2
    fi
    if _ss_scan_one "$target"; then
        return 0
    fi
    return 1
}

# scan_secrets_path <file|dir|-> : scan a file, a dir (recursively), or stdin.
# Return 1 if ANY scanned file trips the gate. Never exits the caller.
scan_secrets_path() {
    local target="$1"
    if [ "$target" = "-" ] || [ -f "$target" ]; then
        scan_secrets_text "$target"
        return $?
    fi
    if [ -d "$target" ]; then
        # find|while runs in a subshell (pipe), so collect hits via a temp file.
        local hitlog
        hitlog="$(mktemp)"
        find "$target" -type f 2>/dev/null | while IFS= read -r f; do
            if ! _ss_scan_one "$f"; then
                printf 'hit\n' >> "$hitlog"
            fi
        done
        if [ -s "$hitlog" ]; then
            rm -f "$hitlog"; return 1
        fi
        rm -f "$hitlog"; return 0
    fi
    printf '[secret-scan] scan_secrets_path: not a file, dir, or "-": %s\n' "$target" >&2
    return 2
}

# --- public: A5 names (record.sh belt + its selftest call these) -------------
# secret_scan_path is the A5 alias of scan_secrets_path. record.sh's L11 belt
# scans capture content (raw/+normalized) under STRICT (flat-hex on) to preserve
# the A5 aggressive intent — safe because capture content holds no 40-hex runs.
secret_scan_path() {
    scan_secrets_path "$@"
    return $?
}

# secret_scan_selftest — plant a real-shaped key in a temp tree, assert the scan
# BITES (negative control), then assert a clean tree (incl. the documented short
# placeholder) PASSES. Returns 0 if both hold.
secret_scan_selftest() {
    local root rc_dirty rc_clean
    root="$(mktemp -d)"
    # DIRTY tree: a length-anchored sk-or key (the exact class a leaked
    # OPENROUTER_API_KEY would have). FAKE but real-SHAPED.
    mkdir -p "$root/dirty"
    printf 'openrouter-key: sk-or-v1-planted0123456789abcdef0123456789\n' > "$root/dirty/leak.txt"
    if scan_secrets_path "$root/dirty" >/dev/null 2>&1; then
        rc_dirty=0   # scan did NOT bite — BAD
    else
        rc_dirty=1   # scan bit — GOOD
    fi
    # CLEAN tree: the documented short placeholder + ordinary fixture text. Must
    # PASS (no hit) — proves the gate is not a blunt instrument that flags the
    # very placeholder the corpus relies on.
    mkdir -p "$root/clean"
    printf 'openrouter-key: sk-or-FAKE-0000\n' > "$root/clean/masked.txt"
    printf 'killed qdrg-x-k1 (zmx qdrg-x-k1, pid 12345)\nexit 0\n' > "$root/clean/kill.txt"
    if scan_secrets_path "$root/clean" >/dev/null 2>&1; then
        rc_clean=1   # clean PASSED — GOOD
    else
        rc_clean=0   # clean tree flagged — BAD (false positive)
    fi
    rm -rf "$root"
    if [ "$rc_dirty" = "1" ] && [ "$rc_clean" = "1" ]; then
        printf '[secret-scan] SELFTEST OK: planted real-shaped key BIT; clean tree PASSED.\n' >&2
        return 0
    fi
    printf '[secret-scan] SELFTEST FAILED: dirty-bit=%s clean-pass=%s (need bit=1 pass=1).\n' \
        "$rc_dirty" "$rc_clean" >&2
    return 1
}

# --- CLI (BASH_SOURCE dual mode) ---------------------------------------------
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    case "${1:-}" in
        --selftest)
            secret_scan_selftest; exit $?
            ;;
        --strict)
            if [ "$#" -lt 2 ]; then
                printf 'usage: secret-scan.sh --strict <path|->\n' >&2
                exit 64
            fi
            if SECRET_SCAN_STRICT=1 secret_scan_path "$2"; then
                exit 0
            fi
            printf '[secret-scan] GATE FAILED (strict): secret-shaped string detected in %s\n' "$2" >&2
            exit 1
            ;;
        "")
            printf 'usage: secret-scan.sh <path|-> | --strict <path|-> | --selftest\n' >&2
            exit 64
            ;;
        *)
            if secret_scan_path "$1"; then
                exit 0
            fi
            printf '[secret-scan] GATE FAILED: secret-shaped string detected in %s\n' "$1" >&2
            exit 1
            ;;
    esac
fi
