#!/usr/bin/env bash
# test/golden/lib/fixture_admit.sh — THE single admission path for fixtures.
#
# RED-TEAM M1: the secret-scan gate was a property of "record.sh ran", which the
# Lima copy-back path bypassed. This makes admission a property of "a fixture file
# enters the git tree": EVERY route into fixtures/<corpus>/ (record.sh on brano,
# Lima limactl-cp copy-back, a manual re-stage) MUST go through fixture_admit, or
# the file does not get placed. record.sh routes through it; Step 3's copy-back
# does too.
#
# TWO MODES:
#   (1) SINGLE-PLATFORM (default): staging carries one RECORDED-FROM (+ optional
#       MATCH-PROOF). Admission checks pin + pairing + secret-scan and places the
#       single provenance set. (record.sh's per-row path.)
#   (2) PLATFORM-AWARE (FA_PLATFORMS="<p1> <p2> ..."): the corpus dir commingles
#       recordings from MULTIPLE platforms (e.g. zmx-dir-resolution = macOS
#       resolution.txt + Linux resolution-linux.txt), so a SINGLE bare RECORDED-FROM
#       cannot attest BOTH (red-team #6). Each platform P supplies its OWN
#       provenance set RECORDED-FROM.P + MATCH-PROOF.P. Admission verifies, PER
#       PLATFORM SET: pin match + HASH-PAIRING (the MATCH-PROOF.P normalized_sha256 /
#       rawA_sha256 / rawB_sha256 must match ACTUAL files in staging — this ties the
#       proof to a concrete fixture, the auditable link the single-stamp commingled
#       dir lacked) + secret-scan (whole staging). It then places the per-platform
#       siblings ALONGSIDE the canonical RECORDED-FROM/MATCH-PROOF. The bare stamp,
#       when present, is the canonical (conventionally first-platform) one and is
#       pin-checked too.
#
# Admission CHECKS (all must pass, fail-closed — nothing is placed on any failure):
#   1. SECRET-SCAN — scan_secrets_path over the WHOLE staging dir.
#   2. RECORDED-FROM stamp(s) present AND pinned_ts_commit == the supplied pin.
#      Single mode: the bare RECORDED-FROM. Platform mode: every RECORDED-FROM.P
#      (and the bare one if present).
#   3. RAW+NORMALIZED PAIRING — single mode: structural (every normalized/<name>
#      has raw/<name>.raw and vice-versa). Platform mode: ADDITIONALLY each
#      MATCH-PROOF.P's normalized/rawA/rawB sha256 must match an actual staging
#      file (hash-pairing), so each platform proof is verifiably about REAL files.
#
# Only after ALL pass does it place files into fixtures/<corpus>/.
#
# Usage:
#   fixture_admit <staging_dir> <corpus> <expected_pin> [<fixtures_root>]
#   FA_PLATFORMS="macos linux" fixture_admit <staging> <corpus> <pin> [<root>]
#     staging_dir   — dir containing raw/, normalized/, RECORDED-FROM[.P], MATCH-PROOF[.P].
#     corpus        — destination is <fixtures_root>/<corpus>/.
#     expected_pin  — the ratified PINNED_TS_COMMIT every stamp must claim.
#     fixtures_root — default: test/golden/fixtures. Selftests pass a SCRATCH root.
#     FA_PLATFORMS  — space-separated platform list -> platform-aware mode.
#
# Returns 0 on admission (files placed), non-zero + a refusal message otherwise.
# Bash 3.2 floor.
# ---------------------------------------------------------------------------
_FA_HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
# A7: scan_secrets.sh was consolidated into secret-scan.sh (one module, both APIs).
# scan_secrets_path is the corpus-safe (default, no flat-hex) whole-staging scan.
. "$_FA_HERE/secret-scan.sh"

_fa_refuse() {
    printf '[admit] REFUSED: %s\n' "$1" >&2
}

# sha256 of a file (portable: macOS shasum / Linux sha256sum). Echoes the hash.
_fa_sha256() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'
    else
        sha256sum "$1" 2>/dev/null | awk '{print $1}'
    fi
}

# Read pinned_ts_commit=<sha> from a RECORDED-FROM stamp file. Echoes the value.
_fa_stamp_pin() {
    sed -n 's/^pinned_ts_commit=//p' "$1" 2>/dev/null | head -1
}

# Read a named sha field (e.g. normalized_sha256) from a MATCH-PROOF. Echoes value.
_fa_proof_field() {
    sed -n "s/^$2=//p" "$1" 2>/dev/null | head -1
}

# _fa_check_pin <stamp_file> <expected_pin> <label> — verify a stamp's pin matches.
_fa_check_pin() {
    local stamp="$1" expected_pin="$2" label="$3"
    if [ ! -f "$stamp" ]; then
        _fa_refuse "no $label stamp in staging (unstamped fixture cannot be admitted)"
        return 1
    fi
    local p; p="$(_fa_stamp_pin "$stamp")"
    if [ -z "$p" ]; then
        _fa_refuse "$label has no pinned_ts_commit= line"
        return 1
    fi
    if [ "$p" != "$expected_pin" ]; then
        _fa_refuse "pin MISMATCH ($label): pinned_ts_commit=$p != expected $expected_pin"
        return 1
    fi
    return 0
}

# _fa_structural_pairing <staging> — every normalized/<name> has raw/<name>.raw and
# vice-versa (runA/runB forensic siblings pair against <name>). Returns 0/1.
_fa_structural_pairing() {
    local staging="$1"
    local rawdir="$staging/raw" normdir="$staging/normalized"
    if [ ! -d "$rawdir" ] || [ ! -d "$normdir" ]; then
        _fa_refuse "staging must contain both raw/ and normalized/ (raw=$rawdir norm=$normdir)"
        return 1
    fi
    local n_norm
    n_norm="$(find "$normdir" -type f 2>/dev/null | grep -c . 2>/dev/null)"
    n_norm="$(printf '%s' "$n_norm" | tr -d '[:space:]')"; [ -z "$n_norm" ] && n_norm=0
    if [ "$n_norm" -eq 0 ]; then
        _fa_refuse "no normalized expectation files in $normdir (empty admission refused)"
        return 1
    fi
    local nf base
    find "$normdir" -type f 2>/dev/null | while IFS= read -r nf; do
        base="$(basename "$nf")"
        [ -f "$rawdir/${base}.raw" ] || printf 'unpaired-norm %s\n' "$base"
    done > "$staging/.fa_pair_check" 2>/dev/null
    local rf rbase
    find "$rawdir" -type f -name '*.raw' 2>/dev/null | while IFS= read -r rf; do
        rbase="$(basename "$rf")"; rbase="${rbase%.raw}"
        case "$rbase" in
            *.runA) rbase="${rbase%.runA}" ;;
            *.runB) rbase="${rbase%.runB}" ;;
        esac
        [ -f "$normdir/$rbase" ] || printf 'unpaired-raw %s\n' "$rbase"
    done >> "$staging/.fa_pair_check" 2>/dev/null
    if [ -s "$staging/.fa_pair_check" ]; then
        _fa_refuse "raw/normalized pairing broken:"
        sed 's/^/[admit]   /' "$staging/.fa_pair_check" >&2
        rm -f "$staging/.fa_pair_check"
        return 1
    fi
    rm -f "$staging/.fa_pair_check"
    return 0
}

# _fa_hash_pairing <staging> <platform> — verify MATCH-PROOF.<platform>'s
# normalized/rawA/rawB sha256 each match an ACTUAL file in staging. This is the
# auditable link tying a platform's proof to concrete files (red-team #6). Returns
# 0 if every hash in the proof is found on a real staging file.
_fa_hash_pairing() {
    local staging="$1" plat="$2"
    local proof="$staging/MATCH-PROOF.$plat"
    if [ ! -f "$proof" ]; then
        _fa_refuse "platform '$plat': MATCH-PROOF.$plat missing (hash-pairing impossible)"
        return 1
    fi
    local want_norm want_a want_b
    want_norm="$(_fa_proof_field "$proof" normalized_sha256)"
    want_a="$(_fa_proof_field "$proof" rawA_sha256)"
    want_b="$(_fa_proof_field "$proof" rawB_sha256)"
    if [ -z "$want_norm" ] || [ -z "$want_a" ] || [ -z "$want_b" ]; then
        _fa_refuse "platform '$plat': MATCH-PROOF.$plat missing a sha256 field (norm/rawA/rawB)"
        return 1
    fi
    # Search staging for a file matching each claimed hash.
    local found_norm="" found_a="" found_b="" f h
    for f in $(find "$staging/normalized" -type f 2>/dev/null); do
        h="$(_fa_sha256 "$f")"
        [ "$h" = "$want_norm" ] && found_norm="$f"
    done
    for f in $(find "$staging/raw" -type f -name '*.raw' 2>/dev/null); do
        h="$(_fa_sha256 "$f")"
        [ "$h" = "$want_a" ] && found_a="$f"
        [ "$h" = "$want_b" ] && found_b="$f"
    done
    if [ -z "$found_norm" ]; then
        _fa_refuse "platform '$plat': no normalized file in staging matches MATCH-PROOF.$plat normalized_sha256=$want_norm"
        return 1
    fi
    if [ -z "$found_a" ] || [ -z "$found_b" ]; then
        _fa_refuse "platform '$plat': raw runA/runB files in staging do not match MATCH-PROOF.$plat (rawA=$want_a rawB=$want_b)"
        return 1
    fi
    printf '[admit]   platform %s: hash-pairing OK (normalized=%s, rawA+rawB matched)\n' \
        "$plat" "$(basename "$found_norm")"
    return 0
}

fixture_admit() {
    local staging="${1:-}"
    local corpus="${2:-}"
    local expected_pin="${3:-}"
    local fixtures_root="${4:-$_FA_HERE/../fixtures}"

    if [ -z "$staging" ] || [ -z "$corpus" ] || [ -z "$expected_pin" ]; then
        _fa_refuse "usage: fixture_admit <staging_dir> <corpus> <expected_pin> [<fixtures_root>]"
        return 64
    fi
    if [ ! -d "$staging" ]; then
        _fa_refuse "staging dir not found: $staging"
        return 64
    fi

    # --- CHECK 1: secret-scan the entire staging dir -------------------------
    if ! scan_secrets_path "$staging"; then
        _fa_refuse "secret-scan tripped on staging dir $staging — NOTHING admitted"
        return 1
    fi

    local platforms="${FA_PLATFORMS:-}"
    local plat

    # --- CHECK 2: pin on every provenance stamp ------------------------------
    # The bare RECORDED-FROM is the canonical stamp; required in single mode, and
    # in platform mode it is the canonical (conventionally first-platform) one. If
    # platform mode is used WITHOUT a bare stamp, the first platform's stamp is the
    # canonical one (copied to RECORDED-FROM at placement).
    if [ -n "$platforms" ]; then
        # Every RECORDED-FROM.P must exist + pin-match.
        for plat in $platforms; do
            _fa_check_pin "$staging/RECORDED-FROM.$plat" "$expected_pin" "RECORDED-FROM.$plat" || return 1
        done
        # A bare stamp, if present, must also pin-match.
        if [ -f "$staging/RECORDED-FROM" ]; then
            _fa_check_pin "$staging/RECORDED-FROM" "$expected_pin" "RECORDED-FROM" || return 1
        fi
    else
        _fa_check_pin "$staging/RECORDED-FROM" "$expected_pin" "RECORDED-FROM" || return 1
    fi

    # --- CHECK 3: pairing ----------------------------------------------------
    # Structural pairing always (whole staging). Platform mode ADDS hash-pairing per
    # platform set so each platform proof is verifiably about real staging files.
    _fa_structural_pairing "$staging" || return 1
    if [ -n "$platforms" ]; then
        for plat in $platforms; do
            _fa_hash_pairing "$staging" "$plat" || return 1
        done
    fi

    # --- ALL CHECKS PASSED: place files into fixtures/<corpus>/ --------------
    local dest="$fixtures_root/$corpus"
    if ! mkdir -p "$dest/raw" "$dest/normalized" 2>/dev/null; then
        _fa_refuse "cannot create destination $dest"
        return 1
    fi
    cp -R "$staging/raw/." "$dest/raw/" 2>/dev/null || { _fa_refuse "raw copy failed"; return 1; }
    cp -R "$staging/normalized/." "$dest/normalized/" 2>/dev/null || { _fa_refuse "normalized copy failed"; return 1; }

    # Canonical bare stamp: the staging's bare RECORDED-FROM, or (platform mode w/o
    # a bare stamp) the FIRST platform's stamp.
    local canon_stamp="$staging/RECORDED-FROM"
    local canon_proof="$staging/MATCH-PROOF"
    if [ ! -f "$canon_stamp" ] && [ -n "$platforms" ]; then
        local first_plat; first_plat="$(printf '%s\n' $platforms | head -1)"
        canon_stamp="$staging/RECORDED-FROM.$first_plat"
        canon_proof="$staging/MATCH-PROOF.$first_plat"
    fi
    [ -f "$canon_stamp" ] && { cp "$canon_stamp" "$dest/RECORDED-FROM" 2>/dev/null || { _fa_refuse "stamp copy failed"; return 1; }; }
    [ -f "$canon_proof" ] && cp "$canon_proof" "$dest/MATCH-PROOF" 2>/dev/null || true

    # Place every per-platform sibling.
    local n_norm
    n_norm="$(find "$staging/normalized" -type f 2>/dev/null | grep -c . 2>/dev/null)"
    n_norm="$(printf '%s' "$n_norm" | tr -d '[:space:]')"; [ -z "$n_norm" ] && n_norm=0
    if [ -n "$platforms" ]; then
        for plat in $platforms; do
            cp "$staging/RECORDED-FROM.$plat" "$dest/RECORDED-FROM.$plat" 2>/dev/null || { _fa_refuse "RECORDED-FROM.$plat copy failed"; return 1; }
            [ -f "$staging/MATCH-PROOF.$plat" ] && cp "$staging/MATCH-PROOF.$plat" "$dest/MATCH-PROOF.$plat" 2>/dev/null || true
        done
        printf '[admit] ADMITTED %s -> %s (pin %s, %s normalized expectation(s), platforms: %s)\n' \
            "$corpus" "$dest" "$expected_pin" "$n_norm" "$platforms"
    else
        printf '[admit] ADMITTED %s -> %s (pin %s, %s normalized expectation(s))\n' \
            "$corpus" "$dest" "$expected_pin" "$n_norm"
    fi
    return 0
}

# CLI: [FA_PLATFORMS="..."] fixture_admit.sh <staging> <corpus> <pin> [<fixtures_root>]
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    fixture_admit "$@"
    exit $?
fi
