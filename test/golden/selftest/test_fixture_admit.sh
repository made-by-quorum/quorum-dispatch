#!/usr/bin/env bash
# test/golden/selftest/test_fixture_admit.sh — prove the single admission path.
#
# fixture_admit is the ONLY way a fixture file enters the tree. It must REFUSE:
#   - a missing RECORDED-FROM stamp,
#   - a pin MISMATCH (stamp pinned_ts_commit != supplied pin),
#   - a planted secret anywhere in the staging dir,
#   - a broken raw/normalized pairing,
#   - an empty staging set;
# and ADMIT a valid, paired, scanned, pin-matching set.
#
# EVERYTHING happens in SCRATCH dirs — a scratch staging AND a scratch fixtures
# root — so this NEVER writes real fixtures/. Cleans up on exit.
#
# Bash 3.2 floor. Run directly.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"   # test/golden
. "$ROOT/lib/fixture_admit.sh"

PASS=0
FAIL=0
PIN="0d0fa9ed4800efb1309eca2311345c48af2c4932"
WRONGPIN="ffffffffffffffffffffffffffffffffffffffff"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/admit-selftest.XXXXXX")"
FXROOT="$SCRATCH/fixtures"          # scratch fixtures root — NEVER the real tree
trap 'rm -rf "$SCRATCH"' EXIT INT TERM

# Build a VALID staging dir at $1 with corpus content + a stamp claiming pin $2.
build_staging() {
    local dir="$1" stamp_pin="$2"
    rm -rf "$dir"
    mkdir -p "$dir/raw" "$dir/normalized"
    printf 'raw bytes for foo\n' > "$dir/raw/foo.txt.raw"
    printf '0\n' > "$dir/raw/foo.txt.raw.exit"
    printf 'normalized foo <TS>\n' > "$dir/normalized/foo.txt"
    {
        printf 'RECORDED-FROM\n'
        printf 'pinned_ts_commit=%s\n' "$stamp_pin"
        printf 'zmx_version=0.6.0\n'
    } > "$dir/RECORDED-FROM"
}

refused() {
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then
        FAIL=$((FAIL + 1)); printf 'FAIL %s — expected REFUSAL, but admission SUCCEEDED\n' "$name"
    else
        PASS=$((PASS + 1)); printf 'ok   %s (refused)\n' "$name"
    fi
}
admitted() {
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then
        PASS=$((PASS + 1)); printf 'ok   %s (admitted)\n' "$name"
    else
        FAIL=$((FAIL + 1)); printf 'FAIL %s — expected ADMISSION, but it was refused\n' "$name"
    fi
}

# --- 1. Missing stamp -> refused ---------------------------------------------
S="$SCRATCH/s_nostamp"
build_staging "$S" "$PIN"
rm -f "$S/RECORDED-FROM"
refused "missing-stamp" fixture_admit "$S" "c_nostamp" "$PIN" "$FXROOT"

# --- 2. Pin mismatch -> refused ----------------------------------------------
S="$SCRATCH/s_wrongpin"
build_staging "$S" "$WRONGPIN"   # stamp claims the wrong pin
refused "pin-mismatch" fixture_admit "$S" "c_wrongpin" "$PIN" "$FXROOT"

# --- 3. Planted secret -> refused --------------------------------------------
S="$SCRATCH/s_secret"
build_staging "$S" "$PIN"
printf 'leaked sk-or-v1-0a1B2c3D4e5F6g7H8i9J0kLmNoPqRsTuVwX\n' >> "$S/raw/foo.txt.raw"
refused "planted-secret" fixture_admit "$S" "c_secret" "$PIN" "$FXROOT"

# --- 4. Broken pairing -> refused --------------------------------------------
S="$SCRATCH/s_unpaired"
build_staging "$S" "$PIN"
rm -f "$S/raw/foo.txt.raw"   # normalized expectation with no raw source
refused "unpaired-normalized" fixture_admit "$S" "c_unpaired" "$PIN" "$FXROOT"

# --- 5. Empty staging -> refused ---------------------------------------------
S="$SCRATCH/s_empty"
mkdir -p "$S/raw" "$S/normalized"
{ printf 'RECORDED-FROM\n'; printf 'pinned_ts_commit=%s\n' "$PIN"; } > "$S/RECORDED-FROM"
refused "empty-staging" fixture_admit "$S" "c_empty" "$PIN" "$FXROOT"

# --- 6. Valid set -> ADMITTED + placed ---------------------------------------
S="$SCRATCH/s_valid"
build_staging "$S" "$PIN"
admitted "valid-set" fixture_admit "$S" "c_valid" "$PIN" "$FXROOT"
# Confirm the files actually landed in the SCRATCH fixtures root.
if [ -f "$FXROOT/c_valid/normalized/foo.txt" ] && [ -f "$FXROOT/c_valid/raw/foo.txt.raw" ] && [ -f "$FXROOT/c_valid/RECORDED-FROM" ]; then
    PASS=$((PASS + 1)); printf 'ok   valid-set/files-placed\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL valid-set/files-placed — admitted but files not present in scratch fixtures\n'
fi

# --- 7. Negative-of-negative: a REFUSAL must NOT place files ------------------
# After the refused cases above, none of those corpora may exist in the scratch root.
LEAKED=0
for c in c_nostamp c_wrongpin c_secret c_unpaired c_empty; do
    [ -e "$FXROOT/$c" ] && LEAKED=1
done
if [ "$LEAKED" -eq 0 ]; then
    PASS=$((PASS + 1)); printf 'ok   refusals/placed-nothing (fail-closed)\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL refusals/placed-nothing — a refused corpus leaked into fixtures\n'
fi

# --- 8. Real fixtures/ untouched ---------------------------------------------
# The whole test used a scratch FXROOT; assert the real tree got no new corpora.
if [ ! -e "$ROOT/fixtures/c_valid" ]; then
    PASS=$((PASS + 1)); printf 'ok   safety/real-fixtures-untouched\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL safety/real-fixtures-untouched — leaked into the REAL fixtures tree!\n'
fi


# ===========================================================================
# PLATFORM-AWARE mode (FA_PLATFORMS) — red-team #6: a commingled dual-platform
# corpus dir cannot be attested by ONE bare stamp. Each platform supplies its own
# RECORDED-FROM.<p> + MATCH-PROOF.<p>; admit verifies pin + HASH-PAIRING (the
# proof's normalized/rawA/rawB sha256 must match real staging files) per platform.
# ---------------------------------------------------------------------------
_pa_sha() { if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'; else sha256sum "$1" | awk '{print $1}'; fi; }
# admit_platforms <platforms> <args...> — call fixture_admit with FA_PLATFORMS set
# (env(1) cannot invoke a sourced shell function; an inline assignment can).
admit_platforms() { local plats="$1"; shift; FA_PLATFORMS="$plats" fixture_admit "$@"; }

# build_dual_staging <dir> <pin> — a valid TWO-platform (macos+linux) staging with
# correct hash-pairing in each MATCH-PROOF.<p>.
build_dual_staging() {
    local dir="$1" pin="$2"
    rm -rf "$dir"; mkdir -p "$dir/raw" "$dir/normalized"
    # macos set
    printf 'mac raw A\n'    > "$dir/raw/res.txt.runA.raw"
    printf 'mac raw B\n'    > "$dir/raw/res.txt.runB.raw"
    printf 'mac raw A\n'    > "$dir/raw/res.txt.raw"
    printf '0\n'            > "$dir/raw/res.txt.raw.exit"
    printf 'mac normalized <TS>\n' > "$dir/normalized/res.txt"
    # linux set
    printf 'lin raw A\n'    > "$dir/raw/res-linux.txt.runA.raw"
    printf 'lin raw B\n'    > "$dir/raw/res-linux.txt.runB.raw"
    printf 'lin raw A\n'    > "$dir/raw/res-linux.txt.raw"
    printf '0\n'            > "$dir/raw/res-linux.txt.raw.exit"
    printf 'lin normalized <TS>\n' > "$dir/normalized/res-linux.txt"
    # per-platform stamps + proofs with REAL hashes
    local mn ma mb ln la lb
    mn="$(_pa_sha "$dir/normalized/res.txt")"; ma="$(_pa_sha "$dir/raw/res.txt.runA.raw")"; mb="$(_pa_sha "$dir/raw/res.txt.runB.raw")"
    ln="$(_pa_sha "$dir/normalized/res-linux.txt")"; la="$(_pa_sha "$dir/raw/res-linux.txt.runA.raw")"; lb="$(_pa_sha "$dir/raw/res-linux.txt.runB.raw")"
    { printf 'RECORDED-FROM\npinned_ts_commit=%s\nhost=Darwin arm64\n' "$pin"; } > "$dir/RECORDED-FROM.macos"
    { printf 'RECORDED-FROM\npinned_ts_commit=%s\nhost=Linux aarch64\n' "$pin"; } > "$dir/RECORDED-FROM.linux"
    { printf 'MATCH-PROOF\npinned_ts_commit=%s\nrawA_sha256=%s\nrawB_sha256=%s\nnormalized_sha256=%s\n' "$pin" "$ma" "$mb" "$mn"; } > "$dir/MATCH-PROOF.macos"
    { printf 'MATCH-PROOF\npinned_ts_commit=%s\nrawA_sha256=%s\nrawB_sha256=%s\nnormalized_sha256=%s\n' "$pin" "$la" "$lb" "$ln"; } > "$dir/MATCH-PROOF.linux"
    # canonical bare = macos
    cp "$dir/RECORDED-FROM.macos" "$dir/RECORDED-FROM"
    cp "$dir/MATCH-PROOF.macos"  "$dir/MATCH-PROOF"
}

# --- 9. Valid dual-platform -> ADMITTED, both sibling sets placed -------------
S="$SCRATCH/s_dual"; build_dual_staging "$S" "$PIN"
admitted "platform/valid-dual" admit_platforms "macos linux" "$S" "c_dual" "$PIN" "$FXROOT"
if [ -f "$FXROOT/c_dual/RECORDED-FROM.macos" ] && [ -f "$FXROOT/c_dual/RECORDED-FROM.linux" ] \
   && [ -f "$FXROOT/c_dual/MATCH-PROOF.linux" ] && [ -f "$FXROOT/c_dual/normalized/res-linux.txt" ]; then
    PASS=$((PASS + 1)); printf 'ok   platform/valid-dual/siblings-placed\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL platform/valid-dual/siblings-placed — sibling provenance not placed\n'
fi

# --- 10. Wrong pin in ONE platform stamp -> refused --------------------------
S="$SCRATCH/s_dual_wrongpin"; build_dual_staging "$S" "$PIN"
{ printf 'RECORDED-FROM\npinned_ts_commit=%s\nhost=Linux aarch64\n' "$WRONGPIN"; } > "$S/RECORDED-FROM.linux"
refused "platform/wrong-pin-in-linux-stamp" admit_platforms "macos linux" "$S" "c_dual_wp" "$PIN" "$FXROOT"

# --- 11. Broken hash-pairing (proof claims a hash no file has) -> refused -----
S="$SCRATCH/s_dual_badhash"; build_dual_staging "$S" "$PIN"
{ printf 'MATCH-PROOF\npinned_ts_commit=%s\nrawA_sha256=dead\nrawB_sha256=beef\nnormalized_sha256=0000000000000000000000000000000000000000000000000000000000000000\n' "$PIN"; } > "$S/MATCH-PROOF.linux"
refused "platform/broken-hash-pairing" admit_platforms "macos linux" "$S" "c_dual_bh" "$PIN" "$FXROOT"

# --- 12. Missing platform sibling (RECORDED-FROM.linux absent) -> refused -----
S="$SCRATCH/s_dual_missing"; build_dual_staging "$S" "$PIN"
rm -f "$S/RECORDED-FROM.linux"
refused "platform/missing-sibling-stamp" admit_platforms "macos linux" "$S" "c_dual_ms" "$PIN" "$FXROOT"

# --- 13. Platform refusals placed nothing ------------------------------------
PLEAK=0
for c in c_dual_wp c_dual_bh c_dual_ms; do [ -e "$FXROOT/$c" ] && PLEAK=1; done
if [ "$PLEAK" -eq 0 ]; then
    PASS=$((PASS + 1)); printf 'ok   platform/refusals-placed-nothing (fail-closed)\n'
else
    FAIL=$((FAIL + 1)); printf 'FAIL platform/refusals-placed-nothing — a refused platform corpus leaked\n'
fi


printf '\n--- test_fixture_admit: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
