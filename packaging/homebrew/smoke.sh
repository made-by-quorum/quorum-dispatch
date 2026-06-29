#!/usr/bin/env bash
# packaging/homebrew/smoke.sh — local brew install-smoke for the quorum-dispatch formula.
#
# The repo is PRIVATE (no public release asset until phase D), and brew scrubs
# the environment at formula-parse time, so the smoke path is: generate a
# CONCRETE formula from quorum-dispatch.rb with a file:// URL + real sha256 of a local
# `git archive` tarball, install it, `brew test` it, then UNINSTALL (the
# installed binary must never linger where it could shadow the org's TS qd on
# someone's PATH — rule 9: Rust qd never points at real state until C2).
#
# Usage: bash packaging/homebrew/smoke.sh   (from the repo root)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

TAR=/tmp/qd-rust-brew-smoke.tar.gz
git archive --prefix=qd-rust/ -o "$TAR" HEAD
SHA="$(shasum -a 256 "$TAR" | awk '{print $1}')"
echo "[smoke] tarball $TAR sha256=$SHA"

WORK="$(mktemp -d /tmp/qd-brew-smoke.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
# Bake the concrete URL + sha into a copy of the canonical formula.
sed -e "s|url ENV.fetch(.*)|url \"file://$TAR\"|" \
    -e "s|sha256 ENV\[.*|sha256 \"$SHA\"|" \
    -e "s|version \"0.0.0-a7\"|version \"0.0.0a7\"|" \
    packaging/homebrew/quorum-dispatch.rb > "$WORK/quorum-dispatch.rb"

# Modern brew refuses formulae outside a tap ("requires formulae to be in a
# tap") — create a THROWAWAY local tap, install from it, then remove it.
TAP="qdrust-smoke/local"
brew untap "$TAP" >/dev/null 2>&1 || true
brew tap-new --no-git "$TAP"
TAPDIR="$(brew --repository)/Library/Taps/qdrust-smoke/homebrew-local"
cp "$WORK/quorum-dispatch.rb" "$TAPDIR/Formula/quorum-dispatch.rb"

echo "[smoke] brew install --build-from-source $TAP/quorum-dispatch (rebuilds qd in brew's sandbox)"
brew install --build-from-source "$TAP/quorum-dispatch"
INSTALLED_OK=$?
echo "[smoke] installed: $(command -v qd || true) ($("$(brew --prefix)/bin/qd" --version 2>/dev/null))"
brew test "$TAP/quorum-dispatch" && echo "[smoke] brew test PASS"
ls "$(brew --prefix)/share/quorum-dispatch/zmx/" && echo "[smoke] pinned-zmx staging present"
echo "[smoke] UNINSTALLING + untapping (rule 9: no lingering Rust qd on PATH)"
brew uninstall quorum-dispatch
brew untap "$TAP"
! [ -x "$(brew --prefix)/bin/qd" ] && echo "[smoke] uninstalled clean"
echo "[smoke] DONE rc=$INSTALLED_OK"
