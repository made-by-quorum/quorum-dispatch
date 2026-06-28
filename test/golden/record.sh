#!/usr/bin/env bash
# test/golden/record.sh — Part-2 recorder wrapper (GATED — DO NOT RUN ON A REAL
# CORPUS until the lead lifts the Part-2 hold).
#
# Records a golden EXPECTATION fixture for a scenario. This is the ONE place
# golden expectations are minted, and it does so by CONSTRUCTION as a
# double-record (red-team M4): the scenario runs TWICE in two FRESH jails; the two
# normalized forms must be BYTE-IDENTICAL or recording FAILS with no expectation
# written. On a match it emits a MATCH-PROOF and routes the result through the
# single fixture-admission path (fixture_admit.sh, red-team M1).
#
# HARD GATES (all fail-closed, in order):
#   G1  PINNED_TS_COMMIT must be set (Part-2 entry condition). Recording against
#       unpinned/unsettled TS would bake UNFIXED behavior into the oracle.
#   G2  QD_UNDER_TEST must resolve UNDER a prep-verified clone whose .prep-verified
#       pin == PINNED_TS_COMMIT (prep_pinned_ts.sh writes it). Closes the scenario
#       bypass: a recording can never be driven against the floating shared TS
#       checkout or an arbitrary path (red-team scenario-bypass / m2).
#   G3  Host-wide build-lock (red-team M5): the HOST lock dir is captured BEFORE
#       jail_establish overrides QD_RUST_LOCK_DIR; the process-driving critical
#       section is wrapped in scripts/build-lock.sh against the HOST lock, so a
#       concurrent build/recording cannot race. The lock is held ONLY during the
#       scenario runs — NOT during normalize / compare / admit.
#
# Double-record outputs (on a successful match):
#   raw/<name>.raw           run A raw   (+ .exit sidecar)
#   raw/<name>.runA.raw      run A raw (explicit)
#   raw/<name>.runB.raw      run B raw
#   normalized/<name>        the matched normalized expectation
#   MATCH-PROOF              sha256 of both raws, both normalized form, AND of
#                            lib/normalize.sh + the scenario file (so a normalizer
#                            or scenario change INVALIDATES the proof -> forces a
#                            re-record).
#   RECORDED-FROM            pin + zmx version + host + timestamp
# All placed via fixture_admit.sh (NOT a direct cp) so the secret-scan +
# stamp/pin + pairing checks run on the way into fixtures/<corpus>/.
#
# Distinct exit codes:
#   70  Part-2 gate closed (no pin)            — G1
#   71  QD_UNDER_TEST not under a prep clone   — G2
#   72  double-record MISMATCH (runs diverged) — the teeth: NO expectation written
#   73  host build-lock unavailable / timed out — G3
#   64  usage/scenario error
#    3  jail refused
#
# Bash 3.2 floor. Usage (Part 2 only):
#   PINNED_TS_COMMIT=<sha> QD_UNDER_TEST="bun <clone>/src/index.ts" \
#     record.sh --scenario <scenarios/foo.sh>
# ---------------------------------------------------------------------------
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_TOP="$(cd "$HERE/../.." && pwd)"
. "$HERE/lib/jail.sh"
. "$HERE/lib/normalize.sh"
. "$HERE/lib/check_python.sh"
. "$HERE/lib/secret-scan.sh"   # L11 carrier: fixture-admit secret gate (fail-closed).
. "$HERE/lib/prep_verify.sh"
. "$HERE/lib/fixture_admit.sh"

EXIT_GATE=70          # Part-2 gate closed (no pin).
EXIT_NOPREP=71        # QD_UNDER_TEST not under a prep-verified clone.
EXIT_MISMATCH=72      # double-record runs diverged.
EXIT_HOSTLOCK=73      # host build-lock unavailable.

# sha256 of a file, portable (macOS shasum / Linux sha256sum). Echoes the hash.
_rec_sha256() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'
    else
        sha256sum "$1" 2>/dev/null | awk '{print $1}'
    fi
}

# --- G1: refuse unless pinned (Part-2 entry condition). ---------------------
if [ -z "${PINNED_TS_COMMIT:-}" ]; then
    printf '[record] REFUSED (G1): PINNED_TS_COMMIT is not set.\n' >&2
    printf '[record] Recording golden expectations is PART 2, BLOCKED until a pinned\n' >&2
    printf '[record] TS commit is ratified and supplied as PINNED_TS_COMMIT.\n' >&2
    printf '[record] Recording pre-pin would bake UNFIXED behavior into the oracle.\n' >&2
    printf '[record] (Part-1 dry-runs go through test/golden/dryrun/run_dryrun.sh and\n' >&2
    printf '[record]  produce DRYRUN-NOT-ORACLE captures, never expectations.)\n' >&2
    exit $EXIT_GATE
fi

# --- HARD GATE: if a pinned TS repo is named, its HEAD MUST equal the pin. -----
# The SET-check above guarantees a pin was DECLARED; this check guarantees the TS
# engine actually being recorded IS that pin. Without it, a recorder could declare
# PINNED_TS_COMMIT=<good> while pointing QD_UNDER_TEST at a clone checked out to a
# DIFFERENT commit — baking UNFIXED behavior under a correct-looking stamp (the
# exact silent-divergence the pin rule exists to prevent). Fail-closed on mismatch.
if [ -n "${PINNED_TS_REPO:-}" ]; then
    if ! _head="$(git -C "$PINNED_TS_REPO" rev-parse HEAD 2>/dev/null)"; then
        printf '[record] REFUSED: PINNED_TS_REPO=%s is not a git repo (cannot verify pin).\n' "$PINNED_TS_REPO" >&2
        exit $EXIT_GATE
    fi
    # Compare on the common prefix length (the pin may be an abbreviated sha).
    _plen="${#PINNED_TS_COMMIT}"
    case "$_head" in
        "$PINNED_TS_COMMIT"*) : ;;   # head starts with the (possibly-abbrev) pin — OK
        *)
            printf '[record] REFUSED: TS checkout HEAD does not match the pin (fail-closed).\n' >&2
            printf '[record]   PINNED_TS_COMMIT = %s\n' "$PINNED_TS_COMMIT" >&2
            printf '[record]   %s HEAD          = %s\n' "$PINNED_TS_REPO" "$_head" >&2
            printf '[record] Recording a NON-PINNED TS checkout would bake unfixed behavior into the oracle.\n' >&2
            exit $EXIT_GATE
            ;;
    esac
    # Belt: the prefix matched but ensure the pin is a real prefix (length sanity).
    if [ "$_plen" -lt 7 ]; then
        printf '[record] REFUSED: PINNED_TS_COMMIT=%s is too short to verify safely (need >=7 hex).\n' "$PINNED_TS_COMMIT" >&2
        exit $EXIT_GATE
    fi
fi

check_python_floor || exit 64

# --- Capture the HOST build-lock dir BEFORE jail_establish overrides it (G3,
# red-team M5). jail_establish sets QD_RUST_LOCK_DIR to a jail-internal dir, which
# would defeat the host-wide mutex; we snapshot the HOST value (or its default)
# here, while $HOME is still the real home. ----------------------------------
JAIL_HOST_LOCK_DIR="${QD_RUST_LOCK_DIR:-$HOME/.quorum/dispatch-rust}"
export JAIL_HOST_LOCK_DIR
BUILD_LOCK="$REPO_TOP/scripts/build-lock.sh"

main() {
    if [ "${1:-}" != "--scenario" ] || [ -z "${2:-}" ]; then
        printf 'usage: PINNED_TS_COMMIT=<sha> QD_UNDER_TEST="bun <clone>/src/index.ts" record.sh --scenario <scenarios/foo.sh>\n' >&2
        exit 64
    fi
    local scn="$2"
    [ -f "$scn" ] || { printf '[record] scenario not found: %s\n' "$scn" >&2; exit 64; }

    # --- G2: QD_UNDER_TEST must resolve under a prep-verified clone. ----------
    # (Resolved BEFORE any jail establishes, while paths are still real.)
    local sut="${QD_UNDER_TEST:-}"
    if [ -z "$sut" ]; then
        printf '[record] REFUSED (G2): QD_UNDER_TEST is unset.\n' >&2
        printf '[record] Run prep_pinned_ts.sh --pin %s first, then export\n' "$PINNED_TS_COMMIT" >&2
        printf '[record] QD_UNDER_TEST="bun <clone>/src/index.ts".\n' >&2
        exit $EXIT_NOPREP
    fi
    if ! prep_verify_entrypoint "$sut" "$PINNED_TS_COMMIT" >/dev/null; then
        exit $EXIT_NOPREP
    fi

    # Resolve scenario metadata (source it once with SCN_OUT pointed at /dev/null).
    SCN_OUT="/dev/null"; SCN_NAME=""; SCN_CLASS=""; SCN_FIXTURE=""; SCN_BUDGET_MS=""; SCN_STUB_BACKED=""
    # shellcheck source=/dev/null
    . "$scn"
    local fixture_rel="$SCN_FIXTURE"
    local corpus
    corpus="$(basename "$(dirname "$(dirname "$fixture_rel")")")"   # fixtures/<corpus>/...
    local base
    base="$(basename "$fixture_rel")"

    # --- R1: stub_sha256 for stub-backed rows. A stub edit changes this hash,
    # which lands in BOTH the MATCH-PROOF and RECORDED-FROM, invalidating the
    # proofs of every row the stub backs (forces a re-record). Computed from the
    # CANONICAL stub main file (not the jailed child env) so it cannot be forged.
    local STUB_MAIN="$HERE/lib/stub_claude/stub_claude.py"
    local STUB_SHA=""
    if [ "${SCN_STUB_BACKED:-}" = "1" ]; then
        STUB_SHA="$(_rec_sha256 "$STUB_MAIN")"
        if [ -z "$STUB_SHA" ]; then
            printf '[record] REFUSED: stub-backed row but stub main not hashable: %s\n' "$STUB_MAIN" >&2
            exit 64
        fi
        STUB_CLAUDE_VERSION="$(head -1 "$HERE/lib/stub_claude/stub_version.txt" 2>/dev/null)"
    fi

    # Staging dir for the double-record (OUTSIDE fixtures/ — fixture_admit places).
    local staging
    staging="$(mktemp -d "${TMPDIR:-/tmp}/qdrec-stage.XXXXXX")"
    mkdir -p "$staging/raw" "$staging/normalized"
    # Use a SHELL-GLOBAL for the trap (the EXIT trap fires after main() returns,
    # when a `local staging` would be unbound under set -u). _REC_STAGING is global.
    _REC_STAGING="$staging"
    trap 'rm -rf "${_REC_STAGING:-}" 2>/dev/null || true; jail_teardown 2>/dev/null || true' EXIT INT TERM

    local rawA="$staging/raw/${base}.runA.raw"
    local rawB="$staging/raw/${base}.runB.raw"
    local normA="$staging/normalized/.${base}.runA"
    local normB="$staging/normalized/.${base}.runB"

    # --- one_run <tag> <raw_out> <norm_out>: fresh jail, host-locked scenario --
    # SCOPE-MINIMIZED LOCK (red-team M5): the host build-lock wraps ONLY the live
    # process-driving (jail_establish + scn_run + copy the raw out). The child also
    # emits the run's JAIL_ROOT/RUNID/RELAY_PORT token values to a meta file. The
    # child is invoked THROUGH scripts/build-lock.sh with QD_RUST_LOCK_DIR forced to
    # the HOST lock dir captured pre-jail. NORMALIZATION happens in the PARENT,
    # AFTER the lock releases (the child exits + teardown runs) — normalize needs
    # only the token STRINGS, not the now-removed jail dirs, so the lock is held
    # strictly for the process-driving critical section, never for normalize/
    # compare/admit.
    one_run() {
        local tag="$1" raw_out="$2" norm_out="$3"
        local locked_script="$staging/run-$tag.sh"
        local meta="$staging/meta-$tag"
        {
            printf '#!/usr/bin/env bash\n'
            printf 'set -u\n'
            printf 'HERE=%q\n' "$HERE"
            printf '. "$HERE/lib/jail.sh"\n'
            printf '. "$HERE/lib/check_python.sh"\n'
            # A5 RECORD_RUNID re-thread (merge of the pre-double-record feature):
            # zmx caps session names at 20 bytes; the default jail runid yields a
            # `qdrg-<runid>-` prefix too long for live rows that spawn qdrg-
            # sessions (kill/ping). A caller passes RECORD_RUNID=<short> (e.g. 4
            # chars) to keep names under the cap; the jail sanitizes it.
            printf 'jail_establish %q || { printf "[record] jail refused\\n" >&2; exit 3; }\n' "${RECORD_RUNID:-}"
            printf 'trap "jail_teardown 2>/dev/null || true" EXIT INT TERM\n'
            # §S substrate: stub-backed rows install the deterministic stub as the
            # jail's `claude` binary (re-exports a jail-rooted CLAUDE_BIN) so the
            # pinned-TS qd boots/drives the stub, not a real Claude. Runs AFTER
            # jail_establish (needs JAIL_ROOT) and BEFORE scn_run.
            if [ "${SCN_STUB_BACKED:-}" = "1" ]; then
                printf '. "$HERE/lib/stub_claude/stub_install.sh"\n'
                printf 'stub_install || { printf "[record] stub install failed\\n" >&2; exit 3; }\n'
            fi
            printf 'export QD_UNDER_TEST=%q\n' "$sut"
            printf 'export JAIL_QD_CMD=%q\n' "${JAIL_QD_CMD:-qd}"
            printf 'SCN_OUT="$JAIL_ROOT/rec.raw"\n'
            printf '. %q\n' "$scn"
            printf 'scn_run\n'
            printf 'cp "$SCN_OUT" %q 2>/dev/null || true\n' "$raw_out"
            printf '[ -f "$SCN_OUT.exit" ] && cp "$SCN_OUT.exit" %q 2>/dev/null || true\n' "$raw_out.exit"
            # Emit the token values the PARENT needs to normalize after unlock.
            printf '{ printf "JAIL_ROOT=%%s\\n" "$JAIL_ROOT"; printf "JAIL_RUNID=%%s\\n" "$JAIL_RUNID"; printf "JAIL_RELAY_PORT=%%s\\n" "$JAIL_RELAY_PORT"; } > %q\n' "$meta"
        } > "$locked_script"
        chmod +x "$locked_script"

        # === LOCK HELD: process-driving only ===
        QD_RUST_LOCK_DIR="$JAIL_HOST_LOCK_DIR" \
            "$BUILD_LOCK" bash "$locked_script"
        local rc=$?
        # === LOCK RELEASED (child exited) ===
        if [ "$rc" -eq 75 ]; then
            printf '[record] REFUSED (G3): host build-lock unavailable/timed out (rc=75).\n' >&2
            exit $EXIT_HOSTLOCK
        fi
        [ "$rc" -eq 0 ] || return "$rc"

        # Normalize in the PARENT, OUTSIDE the lock, using the emitted tokens.
        local m_root m_runid m_port
        m_root="$(sed -n 's/^JAIL_ROOT=//p' "$meta" 2>/dev/null | head -1)"
        m_runid="$(sed -n 's/^JAIL_RUNID=//p' "$meta" 2>/dev/null | head -1)"
        m_port="$(sed -n 's/^JAIL_RELAY_PORT=//p' "$meta" 2>/dev/null | head -1)"
        normalize_all "$m_root" "$m_runid" "$m_port" < "$raw_out" > "$norm_out"
        return 0
    }

    one_run A "$rawA" "$normA" || { printf '[record] run A failed\n' >&2; exit 3; }
    one_run B "$rawB" "$normB" || { printf '[record] run B failed\n' >&2; exit 3; }

    # --- DOUBLE-RECORD ENFORCEMENT: normalized A and B must be byte-identical. -
    if ! cmp -s "$normA" "$normB"; then
        printf '[record] FAILED (exit %s): double-record MISMATCH — run A and run B\n' "$EXIT_MISMATCH" >&2
        printf '[record] produced DIFFERENT normalized output. NO expectation written.\n' >&2
        printf '[record] This scenario is not deterministic under the current normalizer;\n' >&2
        printf '[record] fix the scenario or the normalizer before recording.\n' >&2
        diff "$normA" "$normB" >&2 2>/dev/null | head -40 || true
        exit $EXIT_MISMATCH
    fi

    # MATCH. The matched normalized form becomes the expectation; both raws kept.
    cp "$normA" "$staging/normalized/${base}"
    cp "$rawA" "$staging/raw/${base}.raw"
    [ -f "$rawA.exit" ] && cp "$rawA.exit" "$staging/raw/${base}.raw.exit"
    rm -f "$normA" "$normB"   # drop the per-run hidden normalized scratch.

    # --- MATCH-PROOF: hashes of both raws, the matched normalized form, the
    # normalizer, and the scenario file. A change to normalize.sh or the scenario
    # changes the proof, which invalidates it -> the row's re-record is forced.
    {
        printf 'MATCH-PROOF\n'
        printf 'scenario=%s class=%s\n' "${SCN_NAME:-$scn}" "${SCN_CLASS:-?}"
        printf 'pinned_ts_commit=%s\n' "$PINNED_TS_COMMIT"
        printf 'rawA_sha256=%s\n'       "$(_rec_sha256 "$rawA")"
        printf 'rawB_sha256=%s\n'       "$(_rec_sha256 "$rawB")"
        printf 'normalized_sha256=%s\n' "$(_rec_sha256 "$staging/normalized/${base}")"
        printf 'normalizer_sha256=%s\n' "$(_rec_sha256 "$HERE/lib/normalize.sh")"
        printf 'scenario_sha256=%s\n'   "$(_rec_sha256 "$scn")"
        [ -n "$STUB_SHA" ] && printf 'stub_sha256=%s\n' "$STUB_SHA"
        printf 'proof_generated=%s\n'   "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    } > "$staging/MATCH-PROOF"

    # --- RECORDED-FROM stamp. ------------------------------------------------
    {
        printf 'RECORDED-FROM\n'
        printf 'pinned_ts_commit=%s\n' "$PINNED_TS_COMMIT"
        printf 'zmx_version=%s\n' "${ZMX_VERSION:-unknown}"
        [ -n "$STUB_SHA" ] && printf 'stub_sha256=%s\n' "$STUB_SHA"
        [ -n "$STUB_SHA" ] && printf 'stub_version=%s\n' "${STUB_CLAUDE_VERSION:-unknown}"
        printf 'scenario=%s class=%s\n' "${SCN_NAME:-$scn}" "${SCN_CLASS:-?}"
        printf 'recorded=%s host=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(uname -sm 2>/dev/null || echo unknown)"
    } > "$staging/RECORDED-FROM"

    # L11 BELT (A5 carrier, kept at the merge): scan the staged CAPTURE CONTENT
    # (raw/ + normalized/) with the A5 secret-scan in ADDITION to fixture_admit's
    # own scan below — two independent scanners, fail-closed, before anything is
    # placed. (The pre-merge A5 gate scanned the corpus dir POST-write — red-team
    # MINOR V3c; main's staging+admit architecture closes that gap.) Capture
    # content ONLY: the tooling metadata (MATCH-PROOF/RECORDED-FROM) legitimately
    # carries 40-hex pin SHAs that the scanner's token pattern would (correctly,
    # for capture content) refuse — those files are tooling-generated, never
    # engine-recorded, and fixture_admit's scan owns the full-set policy.
    # A7: secret-scan.sh consolidated both scanners. The belt runs STRICT
    # (SECRET_SCAN_STRICT=1 adds the flat-hex catch) to preserve the A5 aggressive
    # intent on CAPTURE content — safe because raw/+normalized hold no 40-hex runs
    # (verified A7); the corpus pin-SHA metadata is owned by fixture_admit's
    # default-mode whole-staging scan, which must PASS 40-hex pins.
    if ! SECRET_SCAN_STRICT=1 secret_scan_path "$staging/raw" \
       || ! SECRET_SCAN_STRICT=1 secret_scan_path "$staging/normalized"; then
        printf '[record] ADMIT REFUSED (L11 belt): secret-shaped string in staged capture — NOTHING placed.\n' >&2
        exit 1
    fi
    printf '[record] L11 secret-scan belt: staged capture clean.\n' >&2

    # --- ADMIT through the single admission path (scan + stamp + pairing). ----
    local fixtures_root="${RECORD_FIXTURES_ROOT:-$HERE/fixtures}"
    if ! fixture_admit "$staging" "$corpus" "$PINNED_TS_COMMIT" "$fixtures_root"; then
        printf '[record] FAILED: fixture_admit refused the recorded set (see above). NOTHING placed.\n' >&2
        exit 1
    fi
    printf '[record] double-recorded + admitted: %s (corpus %s)\n' "$base" "$corpus"
}

main "$@"
