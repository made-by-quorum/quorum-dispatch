#!/usr/bin/env bash
# test/golden/selftest/test_fetch_zmx.sh — prove the zmx pin FAILS CLOSED.
#
# scripts/fetch-zmx.sh enforces the vendored zmx pin by sha256. The property
# under test is the refusal: a corrupted mirror or a tampered checksum file must
# REFUSE (non-zero) AND leave no tarball at the destination, so an unverified
# blob can never reach a build. The happy path is the floor; the two negative
# controls (gate rows) are the point — if they pass silently, the pin is theater.
#
# Bash 3.2 / POSIX floor. Run directly.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
FETCH="$REPO/scripts/fetch-zmx.sh"
MIRROR="$REPO/vendor/zmx/zmx-0.6.0.tar.gz"
TARBALL="zmx-0.6.0.tar.gz"

PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }

# Per-run scratch; cleaned on exit.
WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT INT TERM

# --- 0. sanity: the fixtures we depend on exist ----------------------------
if [ ! -f "$FETCH" ]; then bad "sanity/fetch-script-present ($FETCH)"; fi
if [ ! -f "$MIRROR" ]; then bad "sanity/mirror-present ($MIRROR)"; fi

# --- 1. happy path: fetch from the in-repo mirror --------------------------
# Exit 0, tarball present at dest, hash matches the mirror.
dest1="$WORK/dest-happy"
if bash "$FETCH" "$dest1" >/dev/null 2>&1; then
    ok "happy/exit-zero"
else
    bad "happy/exit-zero — wanted exit 0"
fi
if [ -f "$dest1/$TARBALL" ]; then
    ok "happy/tarball-present"
else
    bad "happy/tarball-present — no tarball at dest"
fi
got="$(shasum -a 256 "$dest1/$TARBALL" 2>/dev/null | awk '{print $1}')"
want="$(shasum -a 256 "$MIRROR" 2>/dev/null | awk '{print $1}')"
if [ -n "$got" ] && [ "$got" = "$want" ]; then
    ok "happy/hash-matches-mirror"
else
    bad "happy/hash-matches-mirror — got '$got' want '$want'"
fi

# --- 2. NEGATIVE CONTROL: corrupt mirror via QD_ZMX_MIRROR_URL --------------
# Copy the real mirror, flip one byte, and point the script at it through the
# file:// URL seam. The script must REFUSE (non-zero) and leave NO tarball.
corrupt="$WORK/corrupt-mirror.tar.gz"
cp "$MIRROR" "$corrupt"
# Append one byte — changes the bytes (and the length), so the sha256 cannot match.
printf 'X' >> "$corrupt"
dest2="$WORK/dest-corrupt"
if QD_ZMX_MIRROR_URL="file://$corrupt" bash "$FETCH" "$dest2" >/dev/null 2>&1; then
    bad "corrupt-mirror/refuses — expected REFUSAL but exit 0"
else
    ok "corrupt-mirror/refuses (non-zero)"
fi
if [ -e "$dest2/$TARBALL" ]; then
    bad "corrupt-mirror/no-blob-left — unverified tarball remained at dest"
else
    ok "corrupt-mirror/no-blob-left"
fi

# --- 3. NEGATIVE CONTROL: tampered SHA256SUMS -------------------------------
# Build a throwaway copy of script + vendor tree with a WRONG hash in
# SHA256SUMS, but the GENUINE tarball as the mirror. The good tarball must be
# refused because it no longer matches the (tampered) pin. fetch-zmx.sh resolves
# its mirror relative to its own location, so a relocated script with a tampered
# sibling SHA256SUMS exercises exactly this path.
fakerepo="$WORK/fakerepo"
mkdir -p "$fakerepo/scripts" "$fakerepo/vendor/zmx"
cp "$FETCH" "$fakerepo/scripts/fetch-zmx.sh"
cp "$MIRROR" "$fakerepo/vendor/zmx/$TARBALL"
# A syntactically valid but WRONG checksum line (all-zero hash).
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$TARBALL" \
    > "$fakerepo/vendor/zmx/SHA256SUMS"
dest3="$WORK/dest-tampered"
if bash "$fakerepo/scripts/fetch-zmx.sh" "$dest3" >/dev/null 2>&1; then
    bad "tampered-sums/refuses — expected REFUSAL but exit 0"
else
    ok "tampered-sums/refuses (non-zero)"
fi
if [ -e "$dest3/$TARBALL" ]; then
    bad "tampered-sums/no-blob-left — unverified tarball remained at dest"
else
    ok "tampered-sums/no-blob-left"
fi

printf '\n--- test_fetch_zmx: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
