#!/usr/bin/env bash
#
# scope-audit.sh — engine scope audit (ADD-8; pulls A5's content-audit forward,
# minimal slice).
#
# WHY: plan success criterion #7 — the engine crates are CONTENT-FREE. Layout,
# org vocabulary, and product concepts live sbx-side; the engine consumes plain
# paths and opaque payloads. The redteam-retro pass found a banned layout-env
# concept had been ported into crates/sb (create.rs agents-dir tier) — this
# audit makes that class of drift a CI failure instead of a review catch.
#
# DENY (case-insensitive, crates/** source = *.rs + *.toml):
#   SB_PLUGINS_ROOT | substrate | marketplace
#
# EXPLICITLY ALLOWED (contract documentation, not pattern holes):
#   - spawned_by / spawnedBy — the ONE engine-stamped lineage field (ADD-3/3a);
#     not matched by the deny pattern, listed so nobody "fixes" it away.
#   - corpus/fixture dirs (recorded-TS class, A3 ruling): any path containing
#     /fixtures/ is exempt — recorded TS output may legitimately contain
#     anything the TS emitted.
#
# Negative-control discipline: a planted token MUST fail this script (evidence
# journaled in exec/log/2026-06-04-a2.md, ADD-8 entry).
#
# Bash 3.2 floor (macOS): no associative arrays, no ${var,,}, no mapfile.
set -euo pipefail

cd "$(dirname "$0")/.."

matches="$(grep -rinE 'SB_PLUGINS_ROOT|substrate|marketplace' crates \
    --include='*.rs' --include='*.toml' 2>/dev/null \
    | grep -v -E '/fixtures/' || true)"

if [ -n "$matches" ]; then
    echo "scope-audit: FAIL — banned engine-scope token(s) in crates/** source:" >&2
    printf '%s\n' "$matches" >&2
    echo "scope-audit: engine is content-free (success criterion #7);" >&2
    echo "scope-audit: move the concept sbx-side or record a fixture exemption." >&2
    exit 1
fi

echo "scope-audit: clean (deny: plugins-root env|substrate|marketplace; /fixtures/ exempt)"
