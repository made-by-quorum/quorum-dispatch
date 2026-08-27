#!/usr/bin/env bash
# packaging/homebrew/bump-pin.sh — move the tap's pin to the latest public commit.
#
# The formula lives in the TAP, not in this repo (see README.md next to this
# file), and it pins a COMMIT TARBALL of the public mirror because that repo
# carries no tags. Shipping a newer main is therefore three mechanical edits to
# a file in another repo — url, sha256, and revision — which is exactly the kind
# of thing that gets done by hand once and wrong the second time.
#
# This script does them: resolves the mirror's main, downloads that tarball,
# digests it, rewrites the formula, and commits. It does NOT push unless asked.
#
# Usage (from anywhere):
#   bash packaging/homebrew/bump-pin.sh                 # stage the bump, print the diff
#   bash packaging/homebrew/bump-pin.sh --push          # ... and push it to the tap
#   bash packaging/homebrew/bump-pin.sh --version 0.2.0 # also move the hand-stated version
#   bash packaging/homebrew/bump-pin.sh --ref <sha>     # pin something other than main
#
# `version` is stated by hand in the formula — it is what `qd --version`
# reports, not the crate version (0.0.0). So when it does not move, `revision`
# must, or Homebrew offers no upgrade to anyone who already installed. That is
# this script's default: keep the version, increment the revision. Passing
# --version drops the revision instead, which is the correct pairing.
#
# Env knobs: QD_PUBLIC_REPO, QD_TAP_REMOTE, QD_TAP_DIR (reuse a clone you
# already have instead of a throwaway one).
set -euo pipefail

PUBLIC_REPO="${QD_PUBLIC_REPO:-https://github.com/made-by-quorum/quorum-dispatch}"
TAP_REMOTE="${QD_TAP_REMOTE:-git@github.com:made-by-quorum/homebrew-quorum-dispatch.git}"
FORMULA_PATH="Formula/quorum-dispatch.rb"

PUSH=0
REF="main"
NEW_VERSION=""
while [ $# -gt 0 ]; do
  case "$1" in
    --push)     PUSH=1; shift ;;
    --ref)      REF="${2:?--ref needs a branch, tag or sha}"; shift 2 ;;
    --version)  NEW_VERSION="${2:?--version needs X.Y.Z}"; shift 2 ;;
    -h|--help)  sed -n '2,26p' "$0"; exit 0 ;;
    *)          echo "[bump] unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
done

if [ -n "$NEW_VERSION" ] && ! [[ "$NEW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  # The formula's own `brew test` asserts qd --version matches this shape.
  echo "[bump] FAIL: --version must be X.Y.Z, got '$NEW_VERSION'" >&2
  exit 2
fi

WORK="$(mktemp -d /tmp/qd-brew-bump.XXXXXX)"
KEEP_WORK=0
trap '[ "$KEEP_WORK" = 1 ] || rm -rf "$WORK"' EXIT

# 1. Resolve the commit to pin. It must exist ON THE MIRROR — that is the repo
#    the tarball url resolves against, and it is not where the change was
#    authored. A sha passed with --ref is taken as-is; anything else is looked
#    up as a ref there, so a typo fails here rather than at `brew install`.
if [[ "$REF" =~ ^[0-9a-f]{40}$ ]]; then
  SHA="$REF"
else
  SHA="$(git ls-remote "$PUBLIC_REPO" "$REF" | awk 'NR==1 {print $1}')"
  [ -n "$SHA" ] || { echo "[bump] FAIL: no ref '$REF' on $PUBLIC_REPO" >&2; exit 1; }
fi
echo "[bump] mirror $PUBLIC_REPO @ $REF -> $SHA"

# 2. Get the tap. A throwaway clone by default: this script's whole job is one
#    commit, and a stale reused clone is how a bump lands on top of someone
#    else's unpushed work.
if [ -n "${QD_TAP_DIR:-}" ]; then
  TAP="$QD_TAP_DIR"
  echo "[bump] tap from \$QD_TAP_DIR=$TAP"
  [ -d "$TAP/.git" ] || { echo "[bump] FAIL: $TAP is not a git checkout" >&2; exit 1; }
  if [ -n "$(git -C "$TAP" status --porcelain)" ]; then
    echo "[bump] FAIL: $TAP has uncommitted changes — commit or stash them first" >&2
    exit 1
  fi
else
  TAP="$WORK/tap"
  echo "[bump] cloning $TAP_REMOTE"
  git clone --quiet --depth 1 "$TAP_REMOTE" "$TAP"
fi
F="$TAP/$FORMULA_PATH"
[ -f "$F" ] || { echo "[bump] FAIL: $FORMULA_PATH not in the tap checkout" >&2; exit 1; }

# 3. Read the pin as it stands. Every one of these is anchored at two-space
#    indent so it matches the formula's own top-level stanza and not something
#    inside `def install` or `test do`.
cur_url="$(sed -n 's/^  url "\(.*\)"$/\1/p' "$F" | head -1)"
cur_sha256="$(sed -n 's/^  sha256 "\(.*\)"$/\1/p' "$F" | head -1)"
cur_version="$(sed -n 's/^  version "\(.*\)"$/\1/p' "$F" | head -1)"
cur_revision="$(sed -n 's/^  revision \([0-9][0-9]*\)$/\1/p' "$F" | head -1)"
cur_sha="${cur_url##*/}"; cur_sha="${cur_sha%.tar.gz}"
if [ -z "$cur_url" ] || [ -z "$cur_sha256" ] || [ -z "$cur_version" ]; then
  echo "[bump] FAIL: could not read url/sha256/version from $FORMULA_PATH — has its shape changed?" >&2
  exit 1
fi
echo "[bump] tap pins $cur_sha (version $cur_version${cur_revision:+, revision $cur_revision})"

if [ "$cur_sha" = "$SHA" ] && [ -z "$NEW_VERSION" ]; then
  echo "[bump] already pinned to $SHA — nothing to do."
  exit 0
fi

# 4. Digest the tarball GitHub will actually serve, and prove it is buildable
#    before pinning it. The formula builds `--path crates/...` against a root
#    Cargo.toml; the private layout has neither (the engine sits under
#    dispatch/, the workspace manifest under .export-overlay/), so a mirror that
#    somehow received an unexported tree would install-fail on every machine
#    rather than here.
TARBALL_URL="$PUBLIC_REPO/archive/$SHA.tar.gz"
echo "[bump] fetching $TARBALL_URL"
curl -fsSL -o "$WORK/qd.tar.gz" "$TARBALL_URL"
NEW_SHA256="$(shasum -a 256 "$WORK/qd.tar.gz" | awk '{print $1}')"
echo "[bump] sha256=$NEW_SHA256"
if ! tar tzf "$WORK/qd.tar.gz" | grep -qE '^[^/]+/Cargo\.toml$'; then
  echo "[bump] FAIL: $SHA has no root Cargo.toml — that is not the exported layout, the formula cannot build it" >&2
  exit 1
fi
for crate in crates/dispatch crates/quorum-qw; do
  tar tzf "$WORK/qd.tar.gz" | grep -qE "^[^/]+/$crate/Cargo\.toml$" \
    || { echo "[bump] FAIL: $SHA has no $crate — the formula installs qd AND qw from it" >&2; exit 1; }
done
echo "[bump] tarball carries the export layout (root Cargo.toml + both crates)"

# 5. Decide version/revision. Homebrew keys "is this an upgrade?" off
#    version+revision, and `version` here is a hand-written string that a source
#    bump does not move — so one of the two MUST change or the bump is invisible
#    to everyone who already installed.
if [ -n "$NEW_VERSION" ]; then
  VERSION="$NEW_VERSION"
  REVISION=""            # a new version resets it; carrying one forward would be wrong
  echo "[bump] version $cur_version -> $VERSION (revision dropped)"
else
  VERSION="$cur_version"
  REVISION="$(( ${cur_revision:-0} + 1 ))"
  echo "[bump] version stays $VERSION, revision ${cur_revision:-none} -> $REVISION"
fi

# 6. Rewrite. Only the source coordinates move: install steps, caveats and the
#    test block are shipped untouched. `revision` belongs after `license` in
#    Homebrew's stanza order, which is where it is inserted when absent.
HAS_REV=0; [ -n "$cur_revision" ] && HAS_REV=1
INSERT_REV=0; [ "$HAS_REV" = 0 ] && [ -n "$REVISION" ] && INSERT_REV=1
awk -v url="$TARBALL_URL" -v sha="$NEW_SHA256" -v ver="$VERSION" \
    -v rev="$REVISION" -v ins="$INSERT_REV" '
  /^  url "/     { print "  url \"" url "\""; next }
  /^  sha256 "/  { print "  sha256 \"" sha "\""; next }
  /^  version "/ { print "  version \"" ver "\""; next }
  /^  revision / { if (rev != "") print "  revision " rev; next }
  /^  license /  { print; if (ins == "1") print "  revision " rev; next }
                 { print }
' "$F" > "$WORK/formula.rb"

# Verify the rewrite rather than trusting the patterns — a formula reshaped
# upstream would otherwise be committed unchanged and read as a successful bump.
grep -qF "url \"$TARBALL_URL\"" "$WORK/formula.rb" || { echo "[bump] FAIL: url not rewritten" >&2; exit 1; }
grep -qF "sha256 \"$NEW_SHA256\"" "$WORK/formula.rb" || { echo "[bump] FAIL: sha256 not rewritten" >&2; exit 1; }
grep -qF "version \"$VERSION\"" "$WORK/formula.rb" || { echo "[bump] FAIL: version not rewritten" >&2; exit 1; }
if [ -n "$REVISION" ]; then
  grep -qxF "  revision $REVISION" "$WORK/formula.rb" || { echo "[bump] FAIL: revision not written" >&2; exit 1; }
else
  grep -qE '^  revision ' "$WORK/formula.rb" && { echo "[bump] FAIL: revision should have been dropped" >&2; exit 1; }
fi
cp "$WORK/formula.rb" "$F"

# 7. Commit in the tap. Subject names the pin, body says what it is, because
#    `git log` on the tap is the only record of which source a bottle-less
#    formula built from.
SHORT="${SHA:0:12}"
git -C "$TAP" add "$FORMULA_PATH"
git -C "$TAP" -c user.name="$(git config user.name)" -c user.email="$(git config user.email)" \
  commit --quiet -F - <<EOF
quorum-dispatch: pin $SHORT${REVISION:+ (revision $REVISION)}

Moves the pinned commit tarball to $SHA
on $REF of the mirror. version $VERSION is unchanged by a source
bump${REVISION:+, so revision carries the upgrade}.

$TARBALL_URL
sha256 $NEW_SHA256
EOF

echo
git -C "$TAP" --no-pager show --stat --patch HEAD
echo

if [ "$PUSH" = 1 ]; then
  echo "[bump] pushing to $TAP_REMOTE"
  git -C "$TAP" push origin HEAD:main
  echo "[bump] PUSHED — the tap now installs $SHORT."
  echo "[bump] verify on a machine with it installed: brew update && brew upgrade quorum-dispatch"
else
  KEEP_WORK=1
  echo "[bump] committed but NOT pushed. To ship it:"
  echo "         git -C $TAP push origin HEAD:main"
  echo "       or re-run this with --push."
  echo "[bump] to smoke the formula against local source first, see smoke.sh next to this script."
fi
