#!/usr/bin/env bash
# test/golden/lib/stub_claude/stub_install.sh — install the deterministic stub as
# the jail's `claude` binary and re-export CLAUDE_BIN to it.
#
# §S substrate ruling: contract rows are recorded by driving the pinned-TS qd
# against this stub, NOT a real Claude. buildClaudeCmd (utils.ts:507-513 @ pin)
# launches `command '<CLAUDE_BIN>' <flags...>`, and CLAUDE_BIN is overridable
# (utils.ts:226). The jail UNSETS CLAUDE_BIN for hermeticity (finding #2) but
# ALLOWS a jail-rooted re-export after establish (jail.sh belt). So we drop a
# `claude` shim UNDER $JAIL_ROOT and point CLAUDE_BIN at it — the belt passes
# because the value resolves under JAIL_ROOT.
#
# Sourced by stub-backed scenarios AFTER jail_establish. Exports:
#   CLAUDE_BIN=<JAIL_ROOT>/stub-bin/claude   (the shim execs python3 stub_claude.py)
#   STUB_CLAUDE_SHA256                        (hash of the stub main file, for the
#                                              row's stub_sha256 metadata — R1)
# Bash 3.2 floor.
# ---------------------------------------------------------------------------

_STUB_HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STUB_CLAUDE_MAIN="$_STUB_HERE/stub_claude.py"
STUB_CLAUDE_VERSION_FILE="$_STUB_HERE/stub_version.txt"

# sha256 of a file (portable: macOS shasum / Linux sha256sum). Echoes the hash.
_stub_sha256() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'
    else
        sha256sum "$1" 2>/dev/null | awk '{print $1}'
    fi
}

# stub_install — create the jail-rooted shim + re-export CLAUDE_BIN. Requires a
# live jail (JAIL_ROOT under qdrg-runs). Fails closed otherwise.
stub_install() {
    if [ -z "${JAIL_ROOT:-}" ]; then
        printf '[stub] REFUSED: no JAIL_ROOT (call jail_establish first).\n' >&2
        return 1
    fi
    case "$JAIL_ROOT" in
        */qdrg-runs/*) ;;
        *) printf '[stub] REFUSED: JAIL_ROOT %s is not a sandbox dir.\n' "$JAIL_ROOT" >&2; return 1 ;;
    esac
    [ -f "$STUB_CLAUDE_MAIN" ] || { printf '[stub] stub main not found: %s\n' "$STUB_CLAUDE_MAIN" >&2; return 1; }

    local bindir="$JAIL_ROOT/stub-bin"
    mkdir -p "$bindir" || { printf '[stub] cannot create %s\n' "$bindir" >&2; return 1; }
    local shim="$bindir/claude"
    local py="${PYTHON:-python3}"
    {
        printf '#!/usr/bin/env bash\n'
        printf 'exec %q %q "$@"\n' "$py" "$STUB_CLAUDE_MAIN"
    } > "$shim"
    chmod +x "$shim"

    # Re-export CLAUDE_BIN to the jail-rooted shim (passes the jail belt: under
    # JAIL_ROOT, not a production path). buildClaudeCmd will now launch the stub.
    export CLAUDE_BIN="$shim"

    # Stub identity for the row metadata (R1). The match-proof / RECORDED-FROM
    # carry this sha so a stub edit invalidates the proofs of stub-backed rows.
    STUB_CLAUDE_SHA256="$(_stub_sha256 "$STUB_CLAUDE_MAIN")"
    STUB_CLAUDE_VERSION="$(cat "$STUB_CLAUDE_VERSION_FILE" 2>/dev/null | head -1)"
    export STUB_CLAUDE_SHA256 STUB_CLAUDE_VERSION
    return 0
}
