#!/usr/bin/env bash
#
# validate-pins.sh — MANUAL pre-tag gate for extensions.toml (plan 0001 child C).
#
# WHY: `include_str!` only bakes the pin STRING into the `qd` binary; it cannot
# prove a remote ref exists or builds. This script is the honest check: it
# clones/fetches each pinned ref and confirms (a) the commit exists on the named
# repo and (b) it builds. Run it BEFORE tagging an `qd` release. There is NO CI
# wiring (out of scope, plan §"Out of scope"); this is a human-run gate.
#
# It lives OUTSIDE crates/ on purpose: it must name the deploy concepts the
# engine is forbidden to (scope-audit.sh bans them under crates/**).
#
# Usage:
#   bash scripts/validate-pins.sh            # validate both pins
#   SKIP_BUILD=1 bash scripts/validate-pins.sh   # existence-only (faster)
#
# Requires: git + SSH access to the private repos, and cargo (unless SKIP_BUILD).
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$repo_root/extensions.toml"

if [ ! -f "$manifest" ]; then
    echo "validate-pins: FAIL — no manifest at $manifest" >&2
    exit 1
fi

# Tiny reader for the flat `[section]` + `key = "value"` subset (matches the
# engine-side parser in crates/qd/src/extensions.rs). Bash 3.2 floor: no
# associative arrays.
read_pin() {
    # $1 = section, $2 = key
    awk -v section="$1" -v key="$2" '
        /^\[/ { in_section = ($0 == "[" section "]"); next }
        in_section && $1 == key {
            # strip up to the first =, trim spaces and quotes
            sub(/^[^=]*=[ \t]*/, "")
            gsub(/^"|"$/, "")
            print
            exit
        }
    ' "$manifest"
}

sbx_repo="$(read_pin qb repo)"
sbx_rev="$(read_pin qb rev)"
plugins_repo="$(read_pin plugins repo)"
plugins_rev="$(read_pin plugins rev)"

fail=0

check_ref_exists() {
    local label="$1" url="$2" rev="$3"
    if [ -z "$url" ] || [ -z "$rev" ]; then
        echo "validate-pins: FAIL — $label: missing repo/rev in manifest" >&2
        fail=1
        return
    fi
    echo "validate-pins: $label — checking $rev exists on $url"
    # `git ls-remote` can't query an arbitrary sha directly; fetch the sha into a
    # throwaway clone (the authoritative existence proof).
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    if ! git clone --quiet --no-checkout "$url" "$tmp/repo" 2>/dev/null; then
        echo "validate-pins: FAIL — $label: cannot clone $url (auth/network?)" >&2
        fail=1
        return
    fi
    if ! git -C "$tmp/repo" cat-file -e "${rev}^{commit}" 2>/dev/null; then
        echo "validate-pins: FAIL — $label: commit $rev not found on $url" >&2
        fail=1
        return
    fi
    echo "validate-pins: OK   — $label: $rev present"
    if [ "${SKIP_BUILD:-0}" = "1" ]; then
        return
    fi
    git -C "$tmp/repo" checkout --quiet "$rev"
    echo "validate-pins: $label — building $rev (cargo build --release)"
    if [ "$label" = "qb" ]; then
        # qb is a single-bin crate (NOT a workspace) — plain `cargo build` works.
        if ! ( cd "$tmp/repo" && cargo build --release --quiet ); then
            echo "validate-pins: FAIL — $label: $rev does not build" >&2
            fail=1
            return
        fi
        echo "validate-pins: OK   — $label: $rev builds"
    fi
}

check_ref_exists "qb" "$sbx_repo" "$sbx_rev"

# The plugins repo is consumed RAW (no build step — plugins/core is documentation
# + roles + skills, no Cargo crate). Validate existence + the marketplace shape.
check_plugins() {
    local url="$plugins_repo" rev="$plugins_rev"
    if [ -z "$url" ] || [ -z "$rev" ]; then
        echo "validate-pins: FAIL — plugins: missing repo/rev in manifest" >&2
        fail=1
        return
    fi
    echo "validate-pins: plugins — checking $rev exists on $url"
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    if ! git clone --quiet "$url" "$tmp/repo" 2>/dev/null; then
        echo "validate-pins: FAIL — plugins: cannot clone $url (auth/network?)" >&2
        fail=1
        return
    fi
    if ! git -C "$tmp/repo" cat-file -e "${rev}^{commit}" 2>/dev/null; then
        echo "validate-pins: FAIL — plugins: commit $rev not found on $url" >&2
        fail=1
        return
    fi
    git -C "$tmp/repo" checkout --quiet "$rev"
    # The marketplace manifest must exist (it is what `claude plugin marketplace
    # add` consumes) and the pinned plugin dir must be present.
    local market_name plugin_name plugin_version
    market_name="$(read_pin plugins marketplace)"
    plugin_name="$(read_pin plugins plugin)"
    plugin_version="$(read_pin plugins version)"
    if [ ! -f "$tmp/repo/.claude-plugin/marketplace.json" ]; then
        echo "validate-pins: FAIL — plugins: no .claude-plugin/marketplace.json at $rev" >&2
        fail=1
        return
    fi
    if [ ! -f "$tmp/repo/${plugin_name}/.claude-plugin/plugin.json" ]; then
        echo "validate-pins: FAIL — plugins: no ${plugin_name}/.claude-plugin/plugin.json at $rev" >&2
        fail=1
        return
    fi
    echo "validate-pins: OK   — plugins: $rev present; ${market_name}/${plugin_name}@${plugin_version} shape verified"
}
check_plugins

if [ "$fail" -ne 0 ]; then
    echo "validate-pins: FAIL — one or more pins are invalid; do NOT tag." >&2
    exit 1
fi
echo "validate-pins: all pins valid (existence$([ "${SKIP_BUILD:-0}" = "1" ] && echo "" || echo " + build")). Safe to tag."
