#!/usr/bin/env bash
#
# fetch-zmx.sh — retrieve the pinned zmx 0.6.0 source tarball into a destination
# directory and sha256-VERIFY it before letting anyone use it.
#
# WHY THIS EXISTS: the pin must be enforced, not merely declared. qd is built and
# tested against exactly one zmx source tree; a wrong, truncated, or tampered
# tarball must be REFUSED loudly rather than silently accepted. The mirror in
# vendor/zmx/ is the source of truth; this script is the only sanctioned way to
# materialize it elsewhere, and it never leaves an unverified blob at the dest.
#
# WHY VERIFY-AFTER-COPY (comments-carry rationale — do not delete): a copy can be
# truncated by a full disk, a download can be corrupted in flight, and a mirror
# can be tampered with. We compute sha256 against vendor/zmx/SHA256SUMS AFTER the
# bytes land at dest, and on ANY mismatch we DELETE the dest copy and exit 1. The
# invariant a caller may rely on: if this script exits 0, the tarball at dest
# matches the pin; if it exits non-zero, there is no tarball at dest.
#
# WHY shasum -c FROM INSIDE THE DEST DIR: SHA256SUMS records the BARE filename
# (`<hash>  zmx-0.6.0.tar.gz`), so `shasum -a 256 -c` must run with that file as a
# relative path in the cwd. We cd into dest and feed it the checksum line.
#
# USAGE: scripts/fetch-zmx.sh <dest-dir>
#   Copies vendor/zmx/zmx-0.6.0.tar.gz into <dest-dir>/ and verifies it.
#
# ENV:
#   QD_ZMX_MIRROR_URL  If set, download the tarball from this URL (curl -fsSL)
#                      instead of copying the in-repo mirror. The SAME sha256
#                      verification then applies — a remote mirror is trusted no
#                      more than a local file.
#
# Bash 3.2 floor (macOS /bin/bash): no associative arrays, no ${var,,}, no mapfile.
set -euo pipefail

TARBALL="zmx-0.6.0.tar.gz"

# Resolve this script's own dir so the mirror path is independent of the caller's
# cwd (the repo may be checked out anywhere; tests run from temp dirs).
HERE="$(cd "$(dirname "$0")" && pwd)"
MIRROR_DIR="$HERE/../vendor/zmx"
SHASUMS="$MIRROR_DIR/SHA256SUMS"

if [ "$#" -lt 1 ]; then
  echo "fetch-zmx.sh: usage: fetch-zmx.sh <dest-dir>" >&2
  exit 64  # EX_USAGE
fi

DEST_DIR="$1"

if [ ! -f "$SHASUMS" ]; then
  echo "fetch-zmx.sh: missing checksum file: $SHASUMS" >&2
  exit 1
fi

mkdir -p "$DEST_DIR"
DEST_TARBALL="$DEST_DIR/$TARBALL"

# Extract just the expected hash for $TARBALL from SHA256SUMS (first field of the
# matching line). Used only for the loud refusal message; shasum -c does the real
# gate below.
expected_hash="$(awk -v f="$TARBALL" '$2 == f { print $1 }' "$SHASUMS")"
if [ -z "$expected_hash" ]; then
  echo "fetch-zmx.sh: no checksum recorded for $TARBALL in $SHASUMS" >&2
  exit 1
fi

# --- materialize the tarball at dest -----------------------------------------
if [ -n "${QD_ZMX_MIRROR_URL:-}" ]; then
  echo "fetch-zmx.sh: downloading $TARBALL from \$QD_ZMX_MIRROR_URL" >&2
  if ! curl -fsSL -o "$DEST_TARBALL" "$QD_ZMX_MIRROR_URL"; then
    echo "fetch-zmx.sh: download failed from $QD_ZMX_MIRROR_URL" >&2
    rm -f "$DEST_TARBALL"
    exit 1
  fi
else
  SRC_TARBALL="$MIRROR_DIR/$TARBALL"
  if [ ! -f "$SRC_TARBALL" ]; then
    echo "fetch-zmx.sh: missing in-repo mirror: $SRC_TARBALL" >&2
    exit 1
  fi
  cp "$SRC_TARBALL" "$DEST_TARBALL"
fi

# --- verify (the gate) -------------------------------------------------------
# Compute the actual hash for the refusal message, then let shasum -c be the
# authoritative pass/fail (it re-reads the file and compares against the pin).
actual_hash="$(shasum -a 256 "$DEST_TARBALL" | awk '{ print $1 }')"

# Run the canonical check from inside dest so the bare filename in SHA256SUMS
# resolves. Feed shasum -c only the line for our tarball.
checkline="$expected_hash  $TARBALL"
if ( cd "$DEST_DIR" && printf '%s\n' "$checkline" | shasum -a 256 -c - >/dev/null 2>&1 ); then
  echo "fetch-zmx.sh: verified $TARBALL (sha256 $actual_hash) at $DEST_TARBALL" >&2
  exit 0
fi

# Mismatch: REFUSE loudly, name expected vs actual, and leave nothing behind.
echo "fetch-zmx.sh: REFUSING — sha256 mismatch for $TARBALL" >&2
echo "fetch-zmx.sh:   expected $expected_hash" >&2
echo "fetch-zmx.sh:   actual   $actual_hash" >&2
echo "fetch-zmx.sh:   the fetched tarball does NOT match the pin in $SHASUMS." >&2
echo "fetch-zmx.sh:   deleting the unverified copy at $DEST_TARBALL and aborting." >&2
rm -f "$DEST_TARBALL"
exit 1
