#!/usr/bin/env bash
# packaging/homebrew/smoke.sh — local brew install-smoke for the quorum-dispatch formula.
#
# The formula lives in the TAP (made-by-quorum/quorum-dispatch), not in this repo — see
# README.md next to this file. This script fetches that canonical formula and
# points it at a tarball of LOCAL source instead of its pinned commit, so a
# formula change or a source change can be proven end-to-end without first
# exporting to the public mirror and bumping the pin.
#
# It installs, runs `brew test`, then UNINSTALLS and untaps: the smoke build must
# never linger where it could shadow the qd someone actually uses.
#
# Usage (from the repo root):
#   bash packaging/homebrew/smoke.sh
#   QD_FORMULA=/path/to/quorum-dispatch.rb bash packaging/homebrew/smoke.sh
#
# NOTE: the tarball is built with `git archive HEAD` — COMMITTED source only.
# Commit (or stash-and-commit) the change you mean to smoke first.
set -euo pipefail

TAP_FORMULA_URL="https://raw.githubusercontent.com/made-by-quorum/homebrew-quorum-dispatch/main/Formula/quorum-dispatch.rb"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# The smoke installs a formula NAMED quorum-dispatch, and uninstalls it on the
# way out. If a real one is already installed, that teardown would take it with
# it — so refuse rather than eat someone's working install.
if brew list --formula quorum-dispatch >/dev/null 2>&1; then
  cat >&2 <<'EOF'
[smoke] REFUSING: quorum-dispatch is already installed.
        This smoke installs a formula of the same name and uninstalls it at the
        end, which would remove yours too. Take it off first, run the smoke,
        then put it back:
            brew uninstall quorum-dispatch
            bash packaging/homebrew/smoke.sh
            brew install made-by-quorum/quorum-dispatch/quorum-dispatch
EOF
  exit 1
fi

WORK="$(mktemp -d /tmp/qd-brew-smoke.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

# 1. Stage source in the PUBLIC layout (workspace root at the top, crates/ under
#    it) — that is the shape the formula's `--path crates/...` expects.
#
#    In the private monorepo the engine sits under dispatch/ and has NO root
#    Cargo.toml: the workspace manifest, lock and LICENSE live in
#    dispatch/.export-overlay/ and copybara lifts them to the root on export.
#    Reproduce that lift here, or the staged tree cannot build. On the public
#    mirror the tree is already root-shaped and the overlay is gone, so the same
#    script works there with no branch of its own.
STAGE="$WORK/qd-rust"
mkdir -p "$STAGE"
if git cat-file -e HEAD:dispatch 2>/dev/null; then
  echo "[smoke] private monorepo layout — staging HEAD:dispatch + .export-overlay"
  git archive "HEAD:dispatch" | tar x -C "$STAGE"
else
  echo "[smoke] public mirror layout — staging HEAD"
  git archive HEAD | tar x -C "$STAGE"
fi
if [ -d "$STAGE/.export-overlay" ]; then
  cp -R "$STAGE/.export-overlay/." "$STAGE/"
  rm -rf "$STAGE/.export-overlay"
fi
[ -f "$STAGE/Cargo.toml" ] || { echo "[smoke] FAIL: staged tree has no root Cargo.toml" >&2; exit 1; }

TAR="$WORK/qd-rust.tar.gz"
tar czf "$TAR" -C "$WORK" qd-rust
SHA="$(shasum -a 256 "$TAR" | awk '{print $1}')"
echo "[smoke] tarball $TAR sha256=$SHA"

# 2. Take the canonical formula and swap only its source coordinates. Everything
#    else — install steps, test block, caveats — is exercised exactly as shipped.
if [ -n "${QD_FORMULA:-}" ]; then
  echo "[smoke] formula from \$QD_FORMULA=$QD_FORMULA"
  cp "$QD_FORMULA" "$WORK/canonical.rb"
else
  echo "[smoke] formula from the tap ($TAP_FORMULA_URL)"
  curl -fsSL -o "$WORK/canonical.rb" "$TAP_FORMULA_URL"
fi
sed -e "s|^  url \".*\"|  url \"file://$TAR\"|" \
    -e "s|^  sha256 \".*\"|  sha256 \"$SHA\"|" \
    -e "/^  head \"/d" \
    "$WORK/canonical.rb" > "$WORK/quorum-dispatch.rb"
grep -qF "file://$TAR" "$WORK/quorum-dispatch.rb" \
  || { echo "[smoke] FAIL: could not rewrite the formula's url — has its shape changed?" >&2; exit 1; }

# Modern brew refuses formulae outside a tap ("requires formulae to be in a
# tap") — create a THROWAWAY local tap, install from it, then remove it.
TAP="qdrust-smoke/local"
brew untap "$TAP" >/dev/null 2>&1 || true
brew tap-new --no-git "$TAP"
TAPDIR="$(brew --repository)/Library/Taps/qdrust-smoke/homebrew-local"
cp "$WORK/quorum-dispatch.rb" "$TAPDIR/Formula/quorum-dispatch.rb"

echo "[smoke] brew install --build-from-source $TAP/quorum-dispatch (builds qd + qw in brew's sandbox)"
brew install --build-from-source "$TAP/quorum-dispatch"
BREWBIN="$(brew --prefix)/bin"
echo "[smoke] installed: $(command -v qd || true) ($("$BREWBIN/qd" --version 2>/dev/null))"
# ADR-0020: qw must be BESIDE qd — qd resolves it as a sibling of its own
# executable and never searches PATH, so this checks the file next to qd, NOT
# that `qw` resolves as a command. `brew test` asserts the same thing; this line
# makes the failure legible here too, before the (longer) test run.
[ -x "$BREWBIN/qw" ] \
  && echo "[smoke] qw is beside qd at $BREWBIN ($("$BREWBIN/qw" build-profile 2>/dev/null))" \
  || { echo "[smoke] FAIL: $BREWBIN/qw missing — an installed qd cannot open a lane" >&2; exit 1; }
brew test "$TAP/quorum-dispatch" && echo "[smoke] brew test PASS"
# The formula stages NO third-party multiplexer any more (FTUE R1) — the check
# that its pinned tarball landed under share/ went with it. `qd` + `qw` in bin
# ARE the package; nothing else is expected to be installed.
echo "[smoke] UNINSTALLING + untapping (no lingering smoke build on PATH)"
brew uninstall quorum-dispatch
brew untap "$TAP"
if [ -x "$BREWBIN/qd" ] || [ -x "$BREWBIN/qw" ]; then
  echo "[smoke] FAIL: qd/qw still in $BREWBIN after uninstall" >&2
  exit 1
fi
echo "[smoke] uninstalled clean (qd + qw)"
echo "[smoke] DONE"
