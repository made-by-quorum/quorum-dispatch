#!/usr/bin/env bash
# scenario: zmx-dir resolution — LINUX tiers (TMPDIR-collapse + XDG_RUNTIME_DIR).
# semantic (resolution-OUTCOME) class, ADD-9a reclass (ADR-0004 DIV-9a-2 + the
# Bug-D XDG tier). Linux-ONLY by construction: recorded on Lima `sbtest` (aarch64),
# NEVER on macOS — the XDG_RUNTIME_DIR tier is a Linux systemd construct
# (/run/user/<uid>) that has no macOS analogue, and the TMPDIR-collapse OUTCOME is
# the Linux-leaning Claude-spawns-Claude compounding path. See coverage-matrix.md
# exclusions ("XDG_RUNTIME_DIR tier on macOS").
#
# WHY two tiers in one scenario: both are lower tiers of the SAME resolveZmxDir
# precedence ladder (utils.ts:68-82 @ pin): ZMX_DIR > XDG_RUNTIME_DIR/zmx >
# collapse(TMPDIR)/zmx-<uid>. The macOS row records the EXPLICIT ZMX_DIR tier; this
# row records the two tiers BELOW it that only resolve meaningfully on Linux. The
# load-bearing property is the OUTCOME (which dir the session's socket lands in),
# not a fabricated print-line — qd exposes no "print resolved zmx dir" surface.
#
# STUB-BACKED (§S): drives the pinned-TS `qd new` against the deterministic stub
# (CLAUDE_BIN=jail-rooted shim) through REAL zmx 0.6.0 so a real zmx session is
# created, then OBSERVES where its socket landed on the filesystem and asserts it
# equals the dir resolveZmxDir's rule selects for that tier.
#
# HERMETICITY: the recorder runs this with TMPDIR=/run/user/501 so the WHOLE jail
# roots under the real Linux runtime tmpfs (/run/user/501/sbrg-runs/<runid>). Then
# the jail's own XDG_RUNTIME_DIR/TMPDIR are genuinely on /run/user/501 — exercising
# the real Bug-D Linux path — while every resolved socket dir is still UNDER
# JAIL_ROOT (sbrg-prefixed), so the kill belt passes and teardown leaves zero
# leftovers. We do NOT write into the shared /run/user/501 root directly.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="zmx-dir-resolution-linux"
SCN_BUDGET_MS=90000
SCN_CLASS="semantic-resolution-outcome"   # ADD-9a reclass (DIV-9a-2 + XDG tier)
SCN_FIXTURE="fixtures/zmx-dir-resolution/normalized/resolution-linux.txt"
SCN_STUB_BACKED=1

# _zd_uid — the numeric uid resolveZmxDir keys the TMPDIR tier on (zmx-<uid>).
_zd_uid() { id -u 2>/dev/null || echo 0; }

# _zd_collapse <path> — mirror utils.ts collapseRepeatedSegments: drop CONSECUTIVE
# duplicate path segments. Used to compute the EXPECTED collapsed TMPDIR dir
# INDEPENDENTLY of qd (so the assert is non-vacuous: a real divergence in qd's
# collapse would make resolved != expected).
_zd_collapse() {
    printf '%s' "$1" | awk '
        BEGIN { FS="/"; OFS="/" }
        {
            lead = ($0 ~ /^\//) ? "/" : ""
            n = 0
            for (i = 1; i <= NF; i++) {
                if ($i == "") continue
                if (n == 0 || out[n] != $i) { n++; out[n] = $i }
            }
            s = ""
            for (i = 1; i <= n; i++) s = s (i > 1 ? "/" : "") out[i]
            printf "%s%s", lead, s
        }'
}

# _zd_drive_and_observe <name> <expected_dir> <env-overrides...>
# Boot a stub-backed `qd new <name>` with the given env overrides (e.g. ZMX_DIR
# unset, a compounded TMPDIR), then find where the session's zmx socket landed.
# Emits the resolved dir on stdout (empty on miss). The session is killed (pinned
# to the dir it actually landed in) before return so teardown finds nothing live.
_zd_drive_and_observe() {
    local name="$1" expected="$2"; shift 2
    # Boot under the overridden env. The override is applied PER-INVOCATION via
    # env(1); the jail env (HOME/QD_HOME/etc.) is otherwise inherited so the run
    # stays hermetic. CLAUDE_BIN points at the jail-rooted stub shim.
    scn_capture_pty "$SCN_OUT.boottrace.$name" 30 -- \
        env "$@" CLAUDE_BIN="${CLAUDE_BIN:-}" \
        sh -c "exec $QD_UNDER_TEST new $name" >/dev/null 2>&1

    # Observe: poll for the session socket appearing under the EXPECTED dir. We
    # check the rule's predicted dir; if qd resolved elsewhere the socket is NOT
    # there and resolved stays empty (-> assert fails, non-vacuous).
    local resolved="" i=0
    while [ "$i" -lt 20 ]; do
        if [ -e "$expected/$name" ] || [ -e "$expected/$name.sock" ] \
            || ls "$expected"/"$name"* >/dev/null 2>&1; then
            resolved="$expected"; break
        fi
        sleep 1; i=$((i + 1))
    done
    # Kill the session pinned to where it actually lives (resolved or expected),
    # via zmx directly under the jail env (the socket is under JAIL_ROOT, so this
    # is hermetic). jail_kill_session uses qd's resolution which may differ per
    # tier, so we kill by the observed dir explicitly + belt with teardown.
    local killdir="${resolved:-$expected}"
    ZMX_DIR="$killdir" "$JAIL_ZMX_CMD" kill "$name" --force >/dev/null 2>&1 \
        || ZMX_DIR="$killdir" "$JAIL_ZMX_CMD" kill "$name" >/dev/null 2>&1 || true
    printf '%s' "$resolved"
}

scn_run() {
    local uid; uid="$(_zd_uid)"
    : > "$SCN_OUT"

    # --- TIER A: XDG_RUNTIME_DIR (Bug-D, Linux-only) -------------------------
    # ZMX_DIR UNSET -> resolveZmxDir falls to XDG_RUNTIME_DIR/zmx (utils.ts:76-78).
    # The jail set XDG_RUNTIME_DIR under JAIL_ROOT (on /run/user/501 tmpfs when the
    # recorder rooted the jail there). Expected = $XDG_RUNTIME_DIR/zmx.
    local name_xdg exp_xdg res_xdg
    name_xdg="$(scn_session_name zdx)"
    exp_xdg="$XDG_RUNTIME_DIR/zmx"
    res_xdg="$(_zd_drive_and_observe "$name_xdg" "$exp_xdg" \
        -u ZMX_DIR XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" QD_UNDER_TEST="$QD_UNDER_TEST")"

    # --- TIER B: TMPDIR collapse (Claude-spawns-Claude compounding) ----------
    # ZMX_DIR + XDG_RUNTIME_DIR UNSET, a COMPOUNDED TMPDIR (segment repeated) ->
    # resolveZmxDir collapses consecutive dup segments (collapseRepeatedSegments)
    # then appends zmx-<uid>. The compounded dir is built UNDER the jail TMPDIR so
    # it stays hermetic. Expected = collapse(compounded)/zmx-<uid> (ONE canonical
    # dir, not scattered by nesting depth — DIV-9a-2).
    local comp collapsed name_tc exp_tc res_tc
    comp="$TMPDIR/dup/dup"          # consecutive dup -> collapses to $TMPDIR/dup
    mkdir -p "$comp" 2>/dev/null || true
    collapsed="$(_zd_collapse "$comp")"
    name_tc="$(scn_session_name zdt)"
    exp_tc="$collapsed/zmx-$uid"
    res_tc="$(_zd_drive_and_observe "$name_tc" "$exp_tc" \
        -u ZMX_DIR -u XDG_RUNTIME_DIR TMPDIR="$comp" QD_UNDER_TEST="$QD_UNDER_TEST")"

    # Record the OUTCOME (path tokens normalized; the tier labels + the
    # expected==resolved equality is the load-bearing, run-stable signal).
    {
        printf 'case=XDG_RUNTIME_DIR_tier expected=%s resolved=%s\n' "$exp_xdg" "$res_xdg"
        printf 'case=TMPDIR_collapse_tier expected=%s resolved=%s\n' "$exp_tc" "$res_tc"
        printf 'platform=linux uid=%s\n' "$uid"
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    local line resolved expected
    # TIER A: XDG_RUNTIME_DIR tier won — socket landed under $XDG_RUNTIME_DIR/zmx.
    line="$(grep '^case=XDG_RUNTIME_DIR_tier ' "$SCN_OUT" | head -1)"
    expected="$(printf '%s' "$line" | sed -E 's/^case=XDG_RUNTIME_DIR_tier expected=([^ ]*) resolved=.*/\1/')"
    resolved="$(printf '%s' "$line" | sed -E 's/^.* resolved=//')"
    assert_resolution_outcome "$resolved" "$expected" || return 1
    # TIER B: TMPDIR collapse outcome — compounded TMPDIR pegged to ONE canonical dir.
    line="$(grep '^case=TMPDIR_collapse_tier ' "$SCN_OUT" | head -1)"
    expected="$(printf '%s' "$line" | sed -E 's/^case=TMPDIR_collapse_tier expected=([^ ]*) resolved=.*/\1/')"
    resolved="$(printf '%s' "$line" | sed -E 's/^.* resolved=//')"
    assert_resolution_outcome "$resolved" "$expected" || return 1
    return 0
}
