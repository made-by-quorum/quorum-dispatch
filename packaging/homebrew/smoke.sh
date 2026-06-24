#!/usr/bin/env bash
# packaging/homebrew/smoke.sh — local brew install-smoke for the dispatch formula.
#
# The repo is PRIVATE (no public release asset until phase D), and brew scrubs
# the environment at formula-parse time, so the smoke path is: generate a
# CONCRETE formula from dispatch.rb with a file:// URL + real sha256 of a local
# `git archive` tarball, install it, `brew test` it, then UNINSTALL (the
# installed binary must never linger where it could shadow the org's TS sb on
# someone's PATH — rule 9: Rust dispatch never points at real state until C2).
#
# Usage: bash packaging/homebrew/smoke.sh   (from the repo root)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

TAR=/tmp/sb-rust-brew-smoke.tar.gz
git archive --prefix=sb-rust/ -o "$TAR" HEAD
SHA="$(shasum -a 256 "$TAR" | awk '{print $1}')"
echo "[smoke] tarball $TAR sha256=$SHA"

WORK="$(mktemp -d /tmp/dispatch-brew-smoke.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
# Bake the concrete URL + sha into a copy of the canonical formula.
sed -e "s|url ENV.fetch(.*)|url \"file://$TAR\"|" \
    -e "s|sha256 ENV\[.*|sha256 \"$SHA\"|" \
    -e "s|version \"0.0.0-a7\"|version \"0.0.0a7\"|" \
    packaging/homebrew/dispatch.rb > "$WORK/dispatch.rb"

# Modern brew refuses formulae outside a tap ("requires formulae to be in a
# tap") — create a THROWAWAY local tap, install from it, then remove it.
TAP="sbrust-smoke/local"
brew untap "$TAP" >/dev/null 2>&1 || true
brew tap-new --no-git "$TAP"
TAPDIR="$(brew --repository)/Library/Taps/sbrust-smoke/homebrew-local"
cp "$WORK/dispatch.rb" "$TAPDIR/Formula/dispatch.rb"

echo "[smoke] brew install --build-from-source $TAP/dispatch (rebuilds dispatch in brew's sandbox)"
brew install --build-from-source "$TAP/dispatch"
INSTALLED_OK=$?
echo "[smoke] installed: $(command -v dispatch || true) ($("$(brew --prefix)/bin/dispatch" --version 2>/dev/null))"
brew test "$TAP/dispatch" && echo "[smoke] brew test PASS"
ls "$(brew --prefix)/share/dispatch/zmx/" && echo "[smoke] pinned-zmx staging present"
echo "[smoke] UNINSTALLING + untapping (rule 9: no lingering Rust dispatch on PATH)"
brew uninstall dispatch
brew untap "$TAP"
! [ -x "$(brew --prefix)/bin/dispatch" ] && echo "[smoke] uninstalled clean"
echo "[smoke] DONE rc=$INSTALLED_OK"
