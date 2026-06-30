#!/bin/bash
# codex-schema-diff.sh — Codex P2 W1 schema fixture-diff harness (spec section 5).
#
# The committed fixture (crates/dispatch/tests/fixtures/codex-schema/) is the
# `codex app-server generate-json-schema` dump of the PINNED codex version
# (VERSION.pin), stored in CANONICAL form (sorted keys, 2-space indent — the
# python3 round-trip below). This script regenerates from the installed
# binary, canonicalizes the regen the SAME way, and diffs: ANY delta =
# nonzero exit with a named report. Drift is opt-in via the re-pin ceremony
# (regenerate + review + NAMED re-mint commit + VERSION.pin bump + jailed
# rollout-taxonomy spot-check) — never silent.
#
# WHY canonical, not verbatim (W1 day-one finding): generate-json-schema
# emits definitions in NON-DETERMINISTIC order across runs of the SAME
# binary (verified 0.134.0: two generates, 1/254 files raw-differing, 0
# canonical-differing). A byte-diff harness would always-fail = noise = a
# dead harness; canonical-diff fails exactly on real schema change.
#
# Modes:
#   (default)            full: version-pin check + jailed regenerate + diff
#   --regen-dir DIR      compare-only: diff fixture vs DIR (no binary needed;
#                        the always-on mutation-evidence tests drive this)
#
# Exit codes: 0 = no drift; 1 = schema drift (diff printed); 2 = usage/setup
# error; 3 = installed codex version != VERSION.pin (named, before any diff).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
FIXTURE="${QD_CODEX_SCHEMA_FIXTURE:-$ROOT/crates/dispatch/tests/fixtures/codex-schema}"
[ -d "$FIXTURE" ] || { echo "codex-schema-diff: FAIL — fixture dir missing: $FIXTURE" >&2; exit 2; }

REGEN=""
CLEANUP=""
if [ "${1:-}" = "--regen-dir" ]; then
  REGEN="${2:-}"
  [ -d "$REGEN" ] || { echo "codex-schema-diff: FAIL — --regen-dir not a dir: $REGEN" >&2; exit 2; }
else
  PIN=$(cat "$FIXTURE/VERSION.pin" 2>/dev/null)
  [ -n "$PIN" ] || { echo "codex-schema-diff: FAIL — VERSION.pin missing/empty" >&2; exit 2; }
  VER=$(codex --version 2>/dev/null | awk '{print $2}')
  if [ "$VER" != "$PIN" ]; then
    echo "codex-schema-diff: FAIL — installed codex ${VER:-<none>} != pinned $PIN." >&2
    echo "A drifted binary produces a misleading diff; run the re-pin ceremony (spec section 5) deliberately." >&2
    exit 3
  fi
  # Jail under the repo's target dir (in-workspace; literal /tmp is banned for state).
  JAILROOT="$ROOT/target/codex-schema-diff.$$"
  CLEANUP="$JAILROOT"
  REGEN="$JAILROOT/regen"
  mkdir -p "$JAILROOT"/{home,codex-home,xdg-config,xdg-data,xdg-cache,tmp} "$REGEN" || exit 2
  # Derive the dir holding `codex` from the CALLER's PATH so the jailed `env -i`
  # can still find it (the binary may live in ~/.local/bin etc., not a hardcoded
  # system path — the original macOS-only PATH failed on Linux installs).
  CODEX_DIR=$(dirname "$(command -v codex 2>/dev/null || echo /usr/bin/codex)")
  env -i PATH="$CODEX_DIR:/opt/homebrew/bin:/usr/bin:/bin" HOME="$JAILROOT/home" \
    CODEX_HOME="$JAILROOT/codex-home" XDG_CONFIG_HOME="$JAILROOT/xdg-config" \
    XDG_DATA_HOME="$JAILROOT/xdg-data" XDG_CACHE_HOME="$JAILROOT/xdg-cache" \
    TMPDIR="$JAILROOT/tmp" \
    codex app-server generate-json-schema --out "$REGEN" >/dev/null 2>&1 \
    || { echo "codex-schema-diff: FAIL — generate-json-schema failed" >&2; rm -rf "$JAILROOT"; exit 2; }
  # Canonicalize the regen exactly as the fixture is stored (see header).
  python3 - "$REGEN" <<'PYEOF' || { echo "codex-schema-diff: FAIL — canonicalize failed" >&2; rm -rf "$JAILROOT"; exit 2; }
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
for p in root.rglob('*.json'):
    obj = json.loads(p.read_text())
    p.write_text(json.dumps(obj, sort_keys=True, indent=2) + "\n")
PYEOF
fi

# VERSION.pin + manifest.txt are qd-side fixture metadata, not binary output.
DIFF=$(diff -ru -x VERSION.pin -x manifest.txt "$FIXTURE" "$REGEN" 2>&1)
RC=$?
[ -n "$CLEANUP" ] && rm -rf "$CLEANUP"
if [ "$RC" -ne 0 ]; then
  echo "codex-schema-diff: SCHEMA DRIFT detected (fixture vs regenerated):"
  echo "$DIFF"
  echo "codex-schema-diff: FAIL — drift requires the NAMED re-pin ceremony (spec section 5), never a silent re-mint."
  exit 1
fi
echo "codex-schema-diff: OK — regenerated schema is byte-identical to the committed fixture."
exit 0
