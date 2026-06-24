#!/usr/bin/env bash
#
# install-extensions.sh — the EXTERNAL install actions for `sb bootstrap`'s
# extension cascade (plan 0001 child D-install; ADR 0018).
#
# WHY EXTERNAL: the engine is content-free (scope-audit.sh bans the deploy
# vocabulary under crates/**). The actual install actions name those concepts
# (`claude plugin marketplace add`, etc.), so they live here and `sb bootstrap`
# shells out to this script by path. The engine owns ONLY consent + the
# invocation; this script owns the deploy mechanics.
#
# It reads the SAME committed pin the binary bakes (extensions.toml), so the
# script and binary agree on what "the pinned combo" is.
#
# Subcommands (one extension per call so bootstrap can consent-gate each):
#   install-extensions.sh sbx       # cargo install the pinned sbx binary
#   install-extensions.sh plugin    # add the marketplace + install the pinned plugin
#
# Discipline (plan §D standard):
#   - Idempotent: re-run = refresh/no-op (cargo install --force; plugin re-add).
#   - Partial-safe: each subcommand is independent; a failure in one doesn't
#     wedge the other.
#   - Loud on missing toolchain/auth: actionable message + non-zero exit, never
#     an opaque trace. The engine reports the exit status.
#   - Installs over private SSH (the repos are private; SSH URLs in the manifest).
#
# Honors $SB_EXTENSIONS_MANIFEST to override the manifest path (test seam);
# defaults to the repo-root extensions.toml next to this script's parent.
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
manifest="${SB_EXTENSIONS_MANIFEST:-$repo_root/extensions.toml}"

if [ ! -f "$manifest" ]; then
    echo "install-extensions: FAIL — no pin manifest at $manifest" >&2
    exit 1
fi

read_pin() {
    awk -v section="$1" -v key="$2" '
        /^\[/ { in_section = ($0 == "[" section "]"); next }
        in_section && $1 == key {
            sub(/^[^=]*=[ \t]*/, "")
            gsub(/^"|"$/, "")
            print
            exit
        }
    ' "$manifest"
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "install-extensions: FAIL — \`$1\` is not on PATH. $2" >&2
        exit 127
    fi
}

install_sbx() {
    local repo rev
    repo="$(read_pin sbx repo)"
    rev="$(read_pin sbx rev)"
    if [ -z "$repo" ] || [ -z "$rev" ]; then
        echo "install-extensions: FAIL — sbx pin missing repo/rev in manifest" >&2
        exit 1
    fi
    require_cmd cargo "Install a Rust toolchain (https://rustup.rs), then re-run."
    require_cmd git "Install git, then re-run."
    echo "install-extensions: sbx — cargo install $repo @ $rev (--force = idempotent refresh)"
    # sbx is a SINGLE-BIN crate (not a workspace) — `cargo install --git <url>`
    # resolves the lone package without a selector. --rev pins the exact commit;
    # --force makes a re-run an idempotent refresh (overwrites an older install).
    # --bin sbx is explicit (harmless, future-proofs against a second bin).
    # CARGO_NET_GIT_FETCH_WITH_CLI: the repos are PRIVATE; cargo's libgit2 fetch
    # doesn't use the ssh-agent, so it must shell out to the git CLI to auth.
    # (The manifest URL must also be `ssh://git@github.com/…`, not scp-style
    # `git@github.com:…`, which cargo's URL parser rejects.)
    CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git "$repo" --rev "$rev" --bin sbx --force
    echo "install-extensions: sbx — installed pinned $rev to $(command -v sbx 2>/dev/null || echo '~/.cargo/bin/sbx')"
}

install_plugin() {
    local repo rev market plugin version
    repo="$(read_pin plugins repo)"
    rev="$(read_pin plugins rev)"
    market="$(read_pin plugins marketplace)"
    plugin="$(read_pin plugins plugin)"
    version="$(read_pin plugins version)"
    if [ -z "$repo" ] || [ -z "$rev" ] || [ -z "$market" ] || [ -z "$plugin" ]; then
        echo "install-extensions: FAIL — plugins pin missing fields in manifest" >&2
        exit 1
    fi
    require_cmd git "Install git, then re-run."
    require_cmd claude "Install Claude Code first, then re-run."

    # Clone the pinned plugins ref into a stable cache dir, then point Claude
    # Code's marketplace at the local checkout (the repo is PRIVATE — a git-URL
    # marketplace would need Claude Code to carry SSH auth; a local checkout we
    # control sidesteps that). plugins/core is consumed RAW (no build step).
    local cache_dir="${SB_HOME:-$HOME/.sb}/extensions/plugins"
    if [ -d "$cache_dir/.git" ]; then
        echo "install-extensions: plugin — refreshing checkout at $cache_dir"
        git -C "$cache_dir" fetch --quiet origin
    else
        echo "install-extensions: plugin — cloning $repo → $cache_dir"
        rm -rf "$cache_dir"
        mkdir -p "$(dirname "$cache_dir")"
        git clone --quiet "$repo" "$cache_dir"
    fi
    git -C "$cache_dir" checkout --quiet "$rev"
    echo "install-extensions: plugin — checkout at pinned $rev"

    # Add the marketplace (idempotent: re-adding refreshes; tolerate "already
    # added"), then install the pinned plugin. KEEP marketplace=$market,
    # plugin=$plugin, version=$version stable — the commission cache path
    # (~/.claude/plugins/cache/<market>/<plugin>/<version>/) depends on them.
    echo "install-extensions: plugin — registering marketplace '$market' from $cache_dir"
    claude plugin marketplace add "$cache_dir" 2>&1 || \
        claude plugin marketplace update "$market" 2>&1 || true
    echo "install-extensions: plugin — installing ${plugin}@${market}"
    claude plugin install "${plugin}@${market}"
    echo "install-extensions: plugin — installed ${market}/${plugin}@${version}"
}

case "${1:-}" in
    sbx) install_sbx ;;
    plugin) install_plugin ;;
    *)
        echo "usage: install-extensions.sh {sbx|plugin}" >&2
        exit 2
        ;;
esac
