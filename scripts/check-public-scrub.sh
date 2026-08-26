#!/usr/bin/env bash
#
# check-public-scrub.sh — EXIT GATE for the public-export scrub of the `dispatch/`
# tree (Phase 1 "upstream normalization").
#
# WHAT IT ASSERTS: nothing that actually SHIPS to the public mirror carries a
# personal identity, a private host name, or a private repo owner. It is the
# upstream half of a two-sided assertion — the downstream half is the
# `core.verify_match` tripwire in `copy.bara.sky`. Passing here and passing there
# are MEANT TO BE THE SAME ASSERTION, so:
#
#   ⚠ THE EXCLUSION LIST BELOW MUST STAY IN SYNC WITH `copy.bara.sky`. ⚠
#     - `NOT_EXPORTED` mirrors copy.bara.sky's `origin_files` EXCLUDE globs
#       (plus LESSONS.md, excluded by owner decision).
#     - `COVERED_BY_TRANSFORM` mirrors the `core.replace` content transform that
#       rewrites the pinned extension URLs at export time.
#   If you add/remove an exclude in either place, change BOTH in the same commit.
#   A file that is excluded here but exported there is a leak this gate will miss.
#
# WHY THINGS ARE EXCLUDED (two distinct reasons — do not conflate them):
#
#   1. NOT EXPORTED AT ALL. These paths are on copy.bara.sky's `origin_files`
#      exclude list, so their contents never reach the public mirror. Scrubbing
#      them would be pointless churn on internal-only material, and asserting on
#      them would make this gate red forever for no shipped risk.
#
#   2. COVERED BY A COPYBARA CONTENT TRANSFORM. `extensions.toml` and the two
#      assertions in `crates/dispatch/src/extensions.rs` that mirror it keep
#      their `ssh://git@github.com/<private-owner>/{qb,plugins}.git` pins VERBATIM
#      upstream, by explicit owner decision: the pins must stay identical to what
#      the private build resolves, and the rewrite happens at export via a
#      `core.replace` transform instead. This gate therefore filters out exactly
#      those known-covered URL LINES — NOT the whole files — so a NEW leak
#      introduced anywhere else in either file is still caught.
#
# Usage:
#   bash scripts/check-public-scrub.sh          # from anywhere in the repo
#
# It works at BOTH tree depths: in the private monorepo (the tree lives at
# `dispatch/`) and in the exported mirror (the tree IS the repo root). The depth
# is probed from a marker, never hard-coded.
#
# Exit codes:
#   0  clean — no leaks in anything that ships
#   1  leak(s) found — every hit is printed
#   2  harness error (not a git repo / cannot locate the dispatch tree)
#
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "check-public-scrub: FATAL — not inside a git repository" >&2
  exit 2
}
cd "$REPO_ROOT" || exit 2

# Locate the dispatch tree by probing for a marker (`crates/dispatch/`), so this
# gate does not care whether it is running in the monorepo or in the mirror.
if [ -d "$REPO_ROOT/dispatch/crates/dispatch" ]; then
  P="dispatch/" # private monorepo: the tree is a subdirectory
elif [ -d "$REPO_ROOT/crates/dispatch" ]; then
  P="" # exported mirror: the tree IS the repo root
else
  echo "check-public-scrub: FATAL — cannot find the dispatch tree (no crates/dispatch/ at or under $REPO_ROOT)" >&2
  exit 2
fi

# The leak pattern. Keep in step with copy.bara.sky's core.verify_match.
#
# The single-character classes (`bran[o]`, `EricWater[s]`, …) are NOT a typo and
# NOT an approximation: `[x]` is exactly `x` to the regex engine, so the pattern
# matches the same strings it always did. They exist so that THIS FILE — which
# has to spell out every needle it hunts for — does not match itself and report a
# permanent false positive. That keeps this script INSIDE the sweep, so a real
# leak accidentally pasted into it is still caught, instead of exempting it by
# path. Preserve the brackets when editing.
PATTERN='peterber[g]|/Users/eri[c]|/home/eri[c]|EricWater[s]|bran[o]|lima-sandbo[x]|tail6780a[f]'

# --- reason 1: NOT EXPORTED (mirrors copy.bara.sky `origin_files` EXCLUDE) -----
NOT_EXPORTED=(
  ":(exclude)${P}ops/cutover/**"
  ":(exclude)${P}doc/inbox/**"
  ":(exclude)${P}doc/tbd/**"
  ":(exclude)${P}HANDOFF-M3-PROTOCOL-COMPLETE.md"
  ":(exclude)${P}crates/*/GATE-*.md"
  ":(exclude)${P}**/__pycache__/**"
  # Not a copybara origin_files exclude — excluded from the export by explicit
  # owner decision. Listed here for the same reason: it does not ship.
  ":(exclude)${P}LESSONS.md"
)

# --- reason 2: COVERED BY A COPYBARA core.replace CONTENT TRANSFORM -----------
# Deliberately narrow: match the exact known-covered URL lines in the exact two
# files that carry them. Anything else in those files still trips this gate.
COVERED_BY_TRANSFORM="^(${P}extensions\.toml|${P}crates/dispatch/src/extensions\.rs):[0-9]+:.*ssh://git@github\.com/EricWater[s]/(qb|plugins)\.git"

hits="$(git grep -InE "$PATTERN" -- "${P:-.}" "${NOT_EXPORTED[@]}" || true)"
hits="$(printf '%s' "$hits" | grep -vE "$COVERED_BY_TRANSFORM" || true)"

if [ -z "$hits" ]; then
  echo "check-public-scrub: CLEAN — no identity/host/owner leaks in anything that ships."
  echo "  tree: ${P:-<repo root>}   pattern: $PATTERN"
  exit 0
fi

echo "check-public-scrub: LEAKS FOUND — these lines would ship to the public mirror:" >&2
echo >&2
printf '%s\n' "$hits" >&2
echo >&2
echo "count: $(printf '%s\n' "$hits" | wc -l | tr -d ' ')" >&2
echo "Fix them upstream, or (if the path genuinely does not ship) add it to BOTH" >&2
echo "the NOT_EXPORTED list here AND copy.bara.sky's origin_files EXCLUDE." >&2
exit 1
