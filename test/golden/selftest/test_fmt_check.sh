#!/usr/bin/env bash
# test/golden/selftest/test_fmt_check.sh — mechanical rustfmt gate.
#
# WHY: the PR #7 merge commit went CI-red on `cargo fmt --check` alone — the
# A3 fix pass committed without a final fmt, and the lead verification loop ran
# tests+clippy but not fmt. Orc rider (2026-06-04 expedited PR #8 ruling): the
# fmt check joins the MECHANICAL local gate so the local selftest run catches
# exactly what CI's rustfmt step catches, no memory involved.
#
# Serialized via build-lock (B2 rule); formatting-only check, no build artifacts.
# Bash 3.2 floor.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"

echo "=== rustfmt check (cargo fmt --all -- --check) ==="
if "$REPO_ROOT/scripts/build-lock.sh" cargo fmt --all -- --check; then
    echo "--- fmt_check: 1 passed, 0 failed ---"
    exit 0
fi
echo "fmt_check: FAIL — run 'scripts/build-lock.sh cargo fmt --all' and re-commit." >&2
echo "--- fmt_check: 0 passed, 1 failed ---"
exit 1
