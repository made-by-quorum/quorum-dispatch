#!/usr/bin/env bash
# test/golden/lib/normalize.sh — Capture normalizers for the golden harness.
#
# Source this OR run it as a filter: `normalize.sh < raw > normalized`.
# Bash 3.2 floor; the heavy lifting is POSIX sed/awk so it runs identically on
# macOS and Linux CI.
#
# CONTRACT (spec §3.2 + the normalization-spec ADR): each rule collapses a
# specific class of NON-SEMANTIC noise while PRESERVING every load-bearing byte.
#
# NEVER normalized (load-bearing — the asserter compares these):
#   - exit codes
#   - alt-screen sequences (?1049h/l, ?47h/l, ?1047h/l, 2J, 3J)
#   - CR (\r) vs LF (\n) distinction
#   - cursor-move sequences (their PRESENCE/structure; only volatile numeric
#     coordinates are NOT touched here — coordinates are load-bearing for repaint
#     fidelity, so we leave them alone)
#   - backlog line CONTENT and ORDER
#   - JSON field presence/values that carry contract
#
# NORMALIZED (volatile, non-semantic):
#   - timestamps (ISO-8601, epoch-ms, hh:mm:ss.ffffff) -> <TS>
#   - PIDs -> <PID>  (but the PID-FILE APPEARANCE event is preserved structurally;
#                     see normalize_event_stream)
#   - hermetic temp paths (the jail dirs) -> <SB_HOME>/<ZMX_DIR>/<XDG_*>/<TMPDIR>
#   - run ids / session-name run prefixes / socket prefixes / relay ports -> tokens
#
# Each public function is a stdin->stdout filter so they compose and are unit
# testable in isolation.
# ---------------------------------------------------------------------------

# normalize_timestamps: collapse common timestamp shapes to <TS>.
# Handles:
#   ISO-8601:        2026-06-04T01:15:40.797564000Z  / ...+00:00 / ...Z / no-frac
#   clock w/ frac:   01:15:40.797564000   (the spike .raw DLINE shape)
#   epoch-ms (13d):  1717460140797
# Does NOT touch line content around them beyond the timestamp token itself.
#
# CONTEXT-GUARD (P6b, deepseek finding): the clock-with-frac shape HH:MM:SS.ffffff
# also matches a legitimate DURATION/elapsed VALUE (e.g. `duration=01:02:03.500000`),
# which is load-bearing and must SURVIVE — erasing it would blind the oracle to a
# timing regression that prints as an elapsed value. BSD sed has no lookbehind, so
# we PROTECT a duration-labeled clock with a sentinel BEFORE the clock rule fires,
# tokenize the remaining (genuine-timestamp) clocks, then strip the sentinel. A
# clock is treated as a duration (preserved) iff it is immediately preceded by a
# duration label: (duration|elapsed|took|dur|interval|timeout|runtime|wall)[ =:]*.
# Everything else in the clock-with-frac shape (DLINE log times, bare timestamps)
# is tokenized as before. The ISO-8601 'T' form is always a timestamp (rule 1).
normalize_timestamps() {
    # SENT is a non-printing sentinel (SOH) that cannot occur in a real capture
    # line of text. A duration-labeled clock is shielded by injecting SENT INSIDE
    # the clock (after the first `HH:` group) so the clock pattern no longer matches
    # contiguously; the sentinel is stripped afterward, restoring the literal value.
    # (A sentinel merely PRECEDING the clock would not help — the clock's own digit
    # run stays intact and the rule would still fire.)
    local SENT
    SENT="$(printf '\001')"
    sed -E \
        -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?(Z|[+-][0-9]{2}:[0-9]{2})?/<TS>/g' \
        -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}-[0-9]{2}-[0-9]{2}/<TS>/g' \
        -e "s/([Dd]uration|[Ee]lapsed|[Tt]ook|[Dd]ur|[Ii]nterval|[Tt]imeout|[Rr]untime|[Ww]all)([ =:]+[0-9]{2}:)([0-9]{2}:[0-9]{2}\.[0-9]{3,9})/\1\2${SENT}\3/g" \
        -e 's/[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3,9}/<TS>/g' \
        -e "s/${SENT}//g" \
        -e 's/(^|[^0-9])[0-9]{13}($|[^0-9])/\1<TS>\2/g'
}

# normalize_pids: collapse PID tokens to <PID>.
# Matches common shapes: "pid=12345", "pid 12345", "(pid 12345)", ".../12345.json".
# Bare standalone integers are NOT collapsed (too aggressive — would eat
# coordinates and counts); only PID-LABELED numbers and <pid>.json registry files.
normalize_pids() {
    sed -E \
        -e 's/([Pp][Ii][Dd][ =:]+)[0-9]+/\1<PID>/g' \
        -e 's#/[0-9]+\.json#/<PID>.json#g'
}

# normalize_paths <jail_root>: collapse hermetic jail paths to stable tokens.
# Order matters: longest/most-specific subdir tokens first so SB_HOME etc. win
# before the JAIL_ROOT catch-all. Preserves the SUFFIX after the dir (e.g.
# /zmx-501 resolution structure) — only the volatile prefix is tokenized.
#
# HOST_TMP DISTINCTNESS (P6c, gpt finding): the jail's OWN temp dir collapses to
# <TMPDIR> (a JAILED, hermetic path). But a capture might also contain an UNJAILED
# host /tmp path — that is a JAIL-ESCAPE: the engine wrote outside the jail. Such a
# path must NOT collapse into <TMPDIR> (which would let a jail-escape normalize
# into the SAME token a hermetic golden expects and pass green). After the jail
# substitutions consume every jailed path, ANY residual bare /tmp/... is unjailed
# by construction and is tokenized to a DISTINCT <HOST_TMP> so it can never match a
# <TMPDIR>-expecting golden — the escape stays visible and DIFFS. This rule runs
# whether or not a jail root is known (an unjailed /tmp is a host path regardless).
normalize_paths() {
    local root="${1:-${JAIL_ROOT:-}}"
    # Jail substitutions first (consume every jailed path incl. the jail-root /tmp
    # prefix); when no root is known they are skipped. The direct quoted `sed`
    # invocation keeps the jail-root path space-safe (it is a single quoted arg).
    if [ -n "$root" ]; then
        # Escape sed metacharacters in the path.
        local esc
        esc="$(printf '%s' "$root" | sed -e 's/[\/&]/\\&/g')"
        sed -E \
            -e "s/${esc}\/sb_home/<SB_HOME>/g" \
            -e "s/${esc}\/zmx/<ZMX_DIR>/g" \
            -e "s/${esc}\/xdg_config/<XDG_CONFIG>/g" \
            -e "s/${esc}\/xdg_data/<XDG_DATA>/g" \
            -e "s/${esc}\/xdg_state/<XDG_STATE>/g" \
            -e "s/${esc}\/xdg_runtime/<XDG_RUNTIME>/g" \
            -e "s/${esc}\/tmp/<TMPDIR>/g" \
            -e "s/${esc}/<JAIL_ROOT>/g"
    else
        cat
    fi \
    | sed -E -e 's#(^|[^A-Za-z0-9._-])/tmp/#\1<HOST_TMP>/#g'
    # THEN the unjailed-/tmp catch -> <HOST_TMP>: any /tmp/... that SURVIVED the
    # jail substitutions is unjailed by construction (a jail-escape) and must stay
    # DISTINCT from the hermetic <TMPDIR>. The left boundary (start-of-line or a
    # non-path char) keeps /xtmp or foo_tmp from false-matching; the trailing slash
    # requires a directory path, not a bare word.
}

# normalize_runids <runid>: collapse the per-run id, session-name prefix, socket
# prefix and relay port to stable tokens so a re-run does not diff.
#
# CONTEXT-GUARD on the port (P6a, gemini false-green): the OLD rule tokenized the
# per-run port at ANY non-digit boundary, so a BARE INTEGER that coincidentally
# equals the port (e.g. a buggy count/index that happens to print 34567) was
# scrubbed to <RELAY_PORT> and a numeric regression went green. The port is now
# tokenized ONLY in PORT-BEARING contexts:
#   - JSON       "port": N / "port":N   (any "<key>": form whose key ends in
#                                          'port' / 'Port', e.g. relayPort)
#   - URL/host   host:N    where host ends in a non-':' char and N is followed by
#                          a path '/', a non-digit, or line end (localhost:34567,
#                          http://127.0.0.1:34567/...)
#   - labeled    port=N / port N / port:N   (case-insensitive 'port' label)
# A bare integer equal to the port in ANY OTHER position SURVIVES (load-bearing:
# counts/coordinates/indices are not ports). Right boundary is an explicit
# non-digit / line-edge so 34567 matches but 345670 does not (BSD sed has no \b).
normalize_runids() {
    local runid="${1:-${JAIL_RUNID:-}}"
    local port="${2:-${JAIL_RELAY_PORT:-}}"
    local out_cmd="cat"
    if [ -n "$runid" ]; then
        local esc
        esc="$(printf '%s' "$runid" | sed -e 's/[][\/.^$*]/\\&/g')"
        # Session prefix sbrg-<runid>- -> sbrg-<RUNID>- ; bare runid -> <RUNID>.
        out_cmd="sed -E -e s/sbrg-${esc}-/sbrg-<RUNID>-/g -e s/${esc}/<RUNID>/g"
    fi
    if [ -n "$port" ]; then
        # Apply runid sed (if any), then the CONTEXT-GUARDED port seds. Each rule
        # is anchored to a port-bearing context AND a non-digit/line-edge right
        # boundary; a bare coincidental integer hits NONE of them and survives.
        $out_cmd | sed -E \
            -e "s/(\"[A-Za-z_]*[Pp]ort\"[ ]*:[ ]*)${port}(\$|[^0-9])/\\1<RELAY_PORT>\\2/g" \
            -e "s/(:\/\/[^[:space:]\/:]+:)${port}(\$|[^0-9])/\\1<RELAY_PORT>\\2/g" \
            -e "s/([A-Za-z0-9._-]:)${port}(\$|[\/]|[^0-9])/\\1<RELAY_PORT>\\2/g" \
            -e "s/([Pp][Oo][Rr][Tt][ =:]+)${port}(\$|[^0-9])/\\1<RELAY_PORT>\\2/g"
    else
        $out_cmd
    fi
}

# normalize_ansi_chunks: coalesce NON-SEMANTIC chunk-boundary noise.
#
# The PTY capture is timing-sensitive: the same logical output can arrive split
# across os.read() boundaries differently between runs. The ONLY thing this rule
# touches is a run of bare SGR-reset sequences with no intervening printable
# content (`\x1b[0m\x1b[0m...` -> a single `\x1b[0m`) and trailing whitespace
# BEFORE a CR/LF. It must NOT:
#   - merge or drop alt-screen sequences
#   - drop or reorder cursor moves
#   - alter CR vs LF
#   - touch printable line content
#
# We deliberately keep this conservative: chunk-boundary coalescing is the most
# dangerous normalizer (it can erase a real repaint), so the default collapses
# only provably-idempotent redundant resets. Coordinate-bearing sequences and
# alt-screen toggles pass through untouched.
normalize_ansi_chunks() {
    # Collapse 2+ consecutive ESC[0m into one. ESC is \x1b. We do a LITERAL-string
    # replace (index/substr), NOT gsub — gsub treats the needle as a regex and the
    # '[0m' would be parsed as a character class. Literal replace is byte-safe and
    # cannot accidentally touch alt-screen / cursor sequences.
    awk '
        BEGIN { ESC = sprintf("%c", 27); RESET = ESC "[0m"; DBL = RESET RESET; L = length(DBL) }
        {
            line = $0
            p = index(line, DBL)
            while (p > 0) {
                line = substr(line, 1, p - 1) RESET substr(line, p + L)
                p = index(line, DBL)
            }
            print line
        }
    '
}

# normalize_durations: collapse volatile elapsed-duration counters to <DUR>.
#
# The `qd ping` classifier prints `age=<N>s` and `uptime=<N>s` (and `--prefix`
# repeats them per session). These are WALL-CLOCK ELAPSED counters derived from
# now() minus a registry timestamp — volatile across runs the same way a raw
# timestamp is, but they are NOT in the timestamp shapes above (they are bare
# `<int>s` suffixed counters, e.g. `age=0s uptime=60s`). The LOAD-BEARING signal
# in a ping line is the CLASSIFICATION (status / done|active|stuck|ambiguous text)
# and the EXIT CODE — never the exact elapsed seconds. We collapse only the
# value after the `age=`/`uptime=` labels, leaving status= and turns= untouched.
normalize_durations() {
    sed -E \
        -e 's/(age=)[0-9]+s/\1<DUR>/g' \
        -e 's/(uptime=)[0-9]+s/\1<DUR>/g' \
        -e 's/(Age:[[:space:]]+)[0-9]+[smhd] ago/\1<DUR> ago/g'
}

# normalize_zmx_uid <uid>: tokenize the LIVE test uid in zmx-<uid> path components
# (and the bare `uid=<uid>` field that the resolution row records) to <ZMX_UID> —
# but ONLY when the uid equals the live test uid.
#
# WHY (P7, grok finding + redteam uid-501 NIT): resolveZmxDir's TMPDIR-collapse tier
# appends `zmx-<uid>` (utils.ts:68-82). The recorded golden therefore embeds the
# RECORDING host's uid (501). If that literal is left as-is the row is host-locked;
# if it is tokenized UNCONDITIONALLY a broken engine that hard-codes the WRONG uid
# (zmx-0, zmx-999999) would ALSO tokenize to <ZMX_UID> and pass green — the exact
# false-green the NIT names. The fix tokenizes ONLY the live uid: a WRONG uid is NOT
# the live uid, so it stays a literal and DIFFS against the <ZMX_UID> the golden
# expects. One rule serves both portability (correct uid -> stable token) and
# correctness (wrong uid -> survives + diffs). No live uid known -> no-op pass.
# Boundaries are explicit non-digit / line-edge (BSD sed has no \b) so the live uid
# matches but a uid that merely SHARES a digit-prefix (e.g. 5010 vs 501) does not.
normalize_zmx_uid() {
    local uid="${1:-${JAIL_UID:-}}"
    if [ -z "$uid" ]; then
        cat
        return 0
    fi
    # Only act on a NUMERIC uid (a non-numeric value would build a bad regex).
    case "$uid" in
        ''|*[!0-9]*) cat; return 0 ;;
    esac
    sed -E \
        -e "s/(^|[^0-9])zmx-${uid}(\$|[^0-9])/\\1zmx-<ZMX_UID>\\2/g" \
        -e "s/([Uu]id[ =:]+)${uid}(\$|[^0-9])/\\1<ZMX_UID>\\2/g"
}

# normalize_all [<jail_root> <runid> <port> <uid>]: the full pipeline the asserter
# uses. Composes the rules in a safe order. Paths/runids first (most specific), then
# the live-uid token, then timestamps and PIDs, then the conservative chunk
# coalescer LAST. <uid> defaults to JAIL_UID or the live `id -u` (the recording/
# replay host's uid == the live test uid by construction).
normalize_all() {
    local root="${1:-${JAIL_ROOT:-}}"
    local runid="${2:-${JAIL_RUNID:-}}"
    local port="${3:-${JAIL_RELAY_PORT:-}}"
    local uid="${4:-${JAIL_UID:-$(id -u 2>/dev/null || echo)}}"
    normalize_paths "$root" \
        | normalize_runids "$runid" "$port" \
        | normalize_zmx_uid "$uid" \
        | normalize_timestamps \
        | normalize_pids \
        | normalize_durations \
        | normalize_ansi_chunks
}

# If executed (not sourced), act as the full-pipeline filter using env JAIL_*.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    normalize_all "${JAIL_ROOT:-}" "${JAIL_RUNID:-}" "${JAIL_RELAY_PORT:-}" "${JAIL_UID:-}"
fi
