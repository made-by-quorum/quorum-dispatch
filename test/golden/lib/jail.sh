#!/usr/bin/env bash
# test/golden/lib/jail.sh — Session jail for the qd-rust golden-master harness.
#
# Source this in the asserter, the recorder wrapper, and every scenario. Do NOT
# execute directly. Ported and HARDENED from the proven qd-qa battery jail
# (~/work/qd-qa/test/qa/lib/safety.sh): POSITIVE sandbox detection, name-prefix
# kill guard, PID whitelist, production-path refusal.
#
# ABSOLUTE RULE (spec §3.6): the org's REAL TypeScript qd runs on this same
# machine (brano). This harness MUST be invisible to it. Every qd/zmx process we
# touch lives inside a per-run hermetic jail. The harness FAILS CLOSED: if the
# jail is not fully established, nothing runs.
#
# Bash 3.2 floor (macOS): no associative arrays, no ${var,,}, no mapfile.
#
# Functions exported:
#   jail_establish [<runid>]      — create a per-run hermetic env; export all the
#                                   isolation vars; verify positive sandboxing;
#                                   refuse to proceed if any var is unset / points
#                                   at a production path. Sets JAIL_ROOT, JAIL_RUNID,
#                                   JAIL_PREFIX, JAIL_RELAY_PORT.
#   jail_assert_established        — re-verify (fail-closed) that the jail is set up.
#                                   Call at the top of anything destructive.
#   jail_teardown                  — kill prefixed sessions in-jail, rm the run dir.
#   jail_guard_name <name>         — refuse any name not matching $JAIL_PREFIX.
#   jail_register_pid <pid> <name> — whitelist a PID as a prefixed-session member.
#   jail_raw_kill <pid>            — kill a PID only if registered.
#   jail_sb <args...>             — run qd-under-test inside the jail env.
#   jail_zmx <args...>            — run zmx inside the jail env.
#   jail_kill_session <name>       — kill a session by name (guarded + in-jail).
#   jail_require_destructive_ok    — Lima destructive gate (mirror of safety.sh).
#
# Production-path refusal is POSITIVE: we don't blocklist known-bad paths, we
# REQUIRE every isolation var to live under our own per-run JAIL_ROOT. Anything
# else is rejected.
#
# redteam-retro finding #2 (latent hermeticity hole, now closed): the binary reads
# four env vars the jail did NOT set OR clear — QD_PLUGINS_ROOT, QD_SPAWN_AGENTS_DIR,
# CLAUDE_BIN, QD_CLAUDE_FLAGS — so an inherited shell value would reach qd inside the
# jail and escape isolation. jail_establish now UNSETS all four (fail-closed), and the
# positive belt re-checks them: the three path-typed vars, if re-set to a jail-rooted
# override by a live capture, must live under JAIL_ROOT; QD_CLAUDE_FLAGS (a flags
# string) must stay unset.
# ---------------------------------------------------------------------------

# The qd-under-test binary/entrypoint. Defaults to the TS qd for dry-runs; Part 2
# / the SBQA swap points this at the Rust binary. Overridable, but it is only ever
# invoked through the jail env (jail_sb), never bare.
JAIL_SB_CMD="${JAIL_SB_CMD:-qd}"
JAIL_ZMX_CMD="${JAIL_ZMX_CMD:-zmx}"

# ---------------------------------------------------------------------------
# Internal: emit a refusal to stderr.
_jail_refuse() {
    printf '[jail] REFUSED: %s\n' "$1" >&2
}

# Internal: is $1 a prefix of $2? (bash 3.2 safe)
_jail_has_prefix() {
    case "$2" in
        "$1"*) return 0 ;;
        *) return 1 ;;
    esac
}

# ---------------------------------------------------------------------------
# jail_establish [<runid>]
#
# Build a per-run hermetic environment. POSITIVE sandbox detection: every
# isolation var is forced to live under a fresh per-run temp dir we create. If we
# cannot create that dir, or any var would resolve to a real/production path, we
# fail closed.
jail_establish() {
    local runid="${1:-}"
    if [ -z "$runid" ]; then
        # Portable unique-ish id: pid + epoch + RANDOM. No bash 4 features.
        runid="$$$(date +%s 2>/dev/null || echo 0)${RANDOM:-0}"
    fi
    # Strip anything that isn't [A-Za-z0-9] so the prefix is shell/zmx safe.
    runid="$(printf '%s' "$runid" | tr -cd 'A-Za-z0-9')"
    if [ -z "$runid" ]; then
        _jail_refuse "could not derive a usable run id"
        return 1
    fi

    JAIL_RUNID="$runid"
    JAIL_PREFIX="sbrg-${runid}-"

    # Create the per-run root under the system temp dir, in a NEW subtree we own.
    # We deliberately do NOT reuse $TMPDIR for state — we mint a fresh dir and
    # override TMPDIR to point inside it.
    local sys_tmp="${TMPDIR:-/tmp}"
    local base="${sys_tmp%/}/sbrg-runs"
    if ! mkdir -p "$base" 2>/dev/null; then
        _jail_refuse "cannot create jail base dir: $base"
        return 1
    fi
    JAIL_ROOT="${base}/${runid}"
    # A7 M10: every failure path past this assignment CLEARS JAIL_ROOT before
    # returning. A failed establish that leaves JAIL_ROOT set primes the A6
    # set-u incident mechanism: an EXIT-trap jail_teardown keys on JAIL_ROOT,
    # passes its early-return guard, and runs `qd ls`/`zmx list` under the
    # PREVAILING (real) env — read-only org-registry exposure, reproduced
    # 2026-06-05 (A7 journal). Teardown carries its own belt too.
    if [ -e "$JAIL_ROOT" ]; then
        _jail_refuse "jail root already exists (run id collision): $JAIL_ROOT"
        JAIL_ROOT=""
        return 1
    fi
    if ! mkdir -p "$JAIL_ROOT" 2>/dev/null; then
        _jail_refuse "cannot create jail root: $JAIL_ROOT"
        JAIL_ROOT=""
        return 1
    fi
    chmod 700 "$JAIL_ROOT" 2>/dev/null || true

    # Capture the REAL home BEFORE we override HOME. The production-path belt in
    # jail_assert_established checks against this constant — NOT the (jailed) $HOME,
    # which would otherwise become a sandbox path and defeat the belt.
    JAIL_REAL_HOME="${JAIL_REAL_HOME:-$HOME}"

    mkdir -p \
        "$JAIL_ROOT/home" \
        "$JAIL_ROOT/sb_home" \
        "$JAIL_ROOT/zmx" \
        "$JAIL_ROOT/xdg_config" \
        "$JAIL_ROOT/xdg_data" \
        "$JAIL_ROOT/xdg_state" \
        "$JAIL_ROOT/xdg_runtime" \
        "$JAIL_ROOT/tmp" \
        "$JAIL_ROOT/lock" \
        2>/dev/null || {
            _jail_refuse "cannot create jail subtree under $JAIL_ROOT"
            JAIL_ROOT=""
            return 1
        }
    chmod 700 "$JAIL_ROOT/xdg_runtime" 2>/dev/null || true

    # Export the hermetic env. Every qd/zmx invocation inherits these via jail_sb /
    # jail_zmx. Exported here so scenarios run under them.
    #
    # HOME is LOAD-BEARING: the TS qd derives its registry (~/.claude/sessions),
    # relay dir (~/.claude/relay), and projects from homedir(), NOT from QD_HOME.
    # Without overriding HOME the harness would READ AND MODIFY the org's real
    # session registry on brano — exactly the invisibility violation rule 9 bans.
    # (Empirically confirmed 2026-06-04: `qd ls --json` returned real org sessions
    # until HOME was jailed.) QD_HOME is kept too in case the Rust port honors it.
    export HOME="$JAIL_ROOT/home"
    export QD_HOME="$JAIL_ROOT/sb_home"
    export ZMX_DIR="$JAIL_ROOT/zmx"
    export XDG_CONFIG_HOME="$JAIL_ROOT/xdg_config"
    export XDG_DATA_HOME="$JAIL_ROOT/xdg_data"
    export XDG_STATE_HOME="$JAIL_ROOT/xdg_state"
    export XDG_RUNTIME_DIR="$JAIL_ROOT/xdg_runtime"
    export TMPDIR="$JAIL_ROOT/tmp"
    # Build lock stays inside the jail too (spec: QD_RUST_LOCK_DIR override).
    export QD_RUST_LOCK_DIR="$JAIL_ROOT/lock"

    # A unique relay port + socket prefix per run.
    JAIL_RELAY_PORT="$(jail__derive_port "$runid")"
    export QRM_RELAY_PORT="$JAIL_RELAY_PORT"
    export QRM_RELAY_SOCKET_PREFIX="$JAIL_ROOT/relay-${runid}"

    # Clear the four env vars the binary-under-test reads but the jail does NOT
    # itself need to set (redteam-retro finding #2 — latent hermeticity hole). The
    # jail formerly neither set nor unset these, so a value inherited from the real
    # brano shell would reach qd inside the jail and escape isolation — e.g. an
    # inherited QD_SPAWN_AGENTS_DIR makes `--agent` resolve agent defs from a REAL,
    # out-of-jail dir (create.rs resolve_agents_dir), and an inherited CLAUDE_BIN
    # substitutes a real out-of-jail binary (launch.rs claude_bin). We FAIL CLOSED
    # by unsetting all four; the positive belt re-checks them. Live A2 captures that
    # legitimately need a jail-rooted CLAUDE_BIN / QD_SPAWN_AGENTS_DIR re-export them
    # AFTER jail_establish, pointing UNDER JAIL_ROOT — the belt allows that. They
    # pass QD_CLAUDE_FLAGS as a per-command prefix (a flags string, not a path), so
    # it is never left exported in the asserted env.
    #   QD_PLUGINS_ROOT     — path; NOT read by the Rust binary (PR #6 removed the
    #                         plugins-root tier) but cleared for defense in depth
    #                         (TS qd + live-TS captures still honor it).
    #   QD_SPAWN_AGENTS_DIR — path; read by create.rs (the --agent escape vector).
    #   CLAUDE_BIN          — path; read by launch.rs (the binary-substitution vector).
    #   QD_CLAUDE_FLAGS     — flags STRING (not a path); read by launch.rs.
    unset QD_PLUGINS_ROOT
    unset QD_SPAWN_AGENTS_DIR
    unset CLAUDE_BIN
    unset QD_CLAUDE_FLAGS

    # PID whitelist registry lives inside the jail.
    _JAIL_PID_REGISTRY="$JAIL_ROOT/pid-registry"
    : > "$_JAIL_PID_REGISTRY"

    # VERIFY positive sandboxing. Fail closed on any violation.
    if ! jail_assert_established; then
        # NOTE: JAIL_ROOT deliberately LEFT SET here — by this point HOME and the
        # rest of the env are already jail-rooted, so teardown's env-dependent
        # steps are safe AND the partial jail dir should still be removed. The
        # _JAIL_ESTABLISHED flag below stays unset, so teardown's belt will skip
        # steps 1-2 anyway (nothing can have been started in a jail that failed
        # its own assert).
        return 1
    fi
    # A7 M10: positive completion marker. jail_teardown refuses its env-dependent
    # steps (SUT ls / zmx list / kills) unless this is set AND HOME is jail-rooted.
    _JAIL_ESTABLISHED=1
    return 0
}

# Derive a stable high port (20000-59999) from a run id. No bash 4 features.
jail__derive_port() {
    local s="$1" sum=0 i=0 c
    while [ "$i" -lt "${#s}" ]; do
        c="${s:$i:1}"
        sum=$(( (sum * 31 + $(printf '%d' "'$c")) % 40000 ))
        i=$(( i + 1 ))
    done
    echo $(( 20000 + sum ))
}

# ---------------------------------------------------------------------------
# jail_assert_established — POSITIVE sandbox detection, fail-closed.
jail_assert_established() {
    local ok=1 reasons=""

    if [ -z "${JAIL_ROOT:-}" ] || [ ! -d "${JAIL_ROOT:-/nonexistent}" ]; then
        _jail_refuse "JAIL_ROOT unset or missing — call jail_establish first"
        return 1
    fi
    if [ -z "${JAIL_RUNID:-}" ] || [ -z "${JAIL_PREFIX:-}" ]; then
        _jail_refuse "JAIL_RUNID / JAIL_PREFIX unset"
        return 1
    fi

    # JAIL_ROOT must sit under a recognizable sandbox base (POSITIVE marker).
    case "$JAIL_ROOT" in
        */sbrg-runs/*) ;;
        *)
            ok=0
            reasons="${reasons}  JAIL_ROOT '$JAIL_ROOT' is not under an sbrg-runs/ sandbox base\n"
            ;;
    esac

    # Each isolation var must be set AND live under JAIL_ROOT (production refusal).
    # HOME is included: it is load-bearing for invisibility (TS qd keys its
    # registry on homedir()).
    local v name val
    local real_home="${JAIL_REAL_HOME:-$HOME}"
    for v in \
        "HOME=$HOME" \
        "QD_HOME=$QD_HOME" \
        "ZMX_DIR=$ZMX_DIR" \
        "XDG_CONFIG_HOME=$XDG_CONFIG_HOME" \
        "XDG_DATA_HOME=$XDG_DATA_HOME" \
        "XDG_STATE_HOME=$XDG_STATE_HOME" \
        "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR" \
        "TMPDIR=$TMPDIR" \
        "QD_RUST_LOCK_DIR=$QD_RUST_LOCK_DIR" \
        "QRM_RELAY_SOCKET_PREFIX=$QRM_RELAY_SOCKET_PREFIX"
    do
        name="${v%%=*}"
        val="${v#*=}"
        if [ -z "$val" ]; then
            ok=0
            reasons="${reasons}  $name is unset\n"
            continue
        fi
        if ! _jail_has_prefix "$JAIL_ROOT" "$val"; then
            ok=0
            reasons="${reasons}  $name='$val' does not resolve under JAIL_ROOT ($JAIL_ROOT)\n"
        fi
        # Belt: explicitly refuse anything matching a real-path pattern. Uses the
        # REAL home (captured before HOME was jailed) — NOT the live $HOME, which
        # is now a sandbox path and would make these patterns vacuous.
        case "$val" in
            "$real_home"|"$real_home"/.quorum/dispatch|"$real_home"/.quorum/dispatch/*|"$real_home"/.claude|"$real_home"/.claude/*|"$real_home"/.config|"$real_home"/.config/*|"$real_home"/.local/*|/tmp/zmx-*)
                ok=0
                reasons="${reasons}  $name='$val' matches a PRODUCTION path pattern\n"
                ;;
        esac
    done

    # Belt the four binary-read env vars the jail clears (redteam-retro finding #2).
    # Unlike the load-bearing isolation vars above, these are OPTIONAL: jail_establish
    # unsets them, so UNSET is the expected, passing state. They only become set when
    # a live A2 capture deliberately re-exports a jail-rooted override after establish.
    # Fail-closed rule for the three PATH-typed vars: if set, the value MUST resolve
    # under JAIL_ROOT (a leaked real-shell value would not) AND must not match a
    # production-path pattern. An inherited QD_SPAWN_AGENTS_DIR or CLAUDE_BIN is the
    # exact hermeticity escape this finding closes.
    for v in \
        "QD_PLUGINS_ROOT=${QD_PLUGINS_ROOT:-}" \
        "QD_SPAWN_AGENTS_DIR=${QD_SPAWN_AGENTS_DIR:-}" \
        "CLAUDE_BIN=${CLAUDE_BIN:-}"
    do
        name="${v%%=*}"
        val="${v#*=}"
        [ -z "$val" ] && continue   # unset = OK (the jail cleared it).
        if ! _jail_has_prefix "$JAIL_ROOT" "$val"; then
            ok=0
            reasons="${reasons}  $name='$val' is set but does not resolve under JAIL_ROOT ($JAIL_ROOT)\n"
        fi
        case "$val" in
            "$real_home"|"$real_home"/.quorum/dispatch|"$real_home"/.quorum/dispatch/*|"$real_home"/.claude|"$real_home"/.claude/*|"$real_home"/.config|"$real_home"/.config/*|"$real_home"/.local/*|/tmp/zmx-*)
                ok=0
                reasons="${reasons}  $name='$val' matches a PRODUCTION path pattern\n"
                ;;
        esac
    done
    # QD_CLAUDE_FLAGS is a flags STRING, not a path, so the under-JAIL_ROOT rule
    # cannot apply. The jail never leaves it exported (captures pass it per-command);
    # any value visible here is an inherited shell leak. Fail closed: it must be unset.
    if [ -n "${QD_CLAUDE_FLAGS:-}" ]; then
        ok=0
        reasons="${reasons}  QD_CLAUDE_FLAGS='${QD_CLAUDE_FLAGS}' is set (inherited leak — the jail clears it; pass flags per-command)\n"
    fi

    # Relay port must be in our reserved range.
    if [ -z "${JAIL_RELAY_PORT:-}" ] || [ "${JAIL_RELAY_PORT:-0}" -lt 20000 ] 2>/dev/null || [ "${JAIL_RELAY_PORT:-0}" -gt 59999 ] 2>/dev/null; then
        ok=0
        reasons="${reasons}  JAIL_RELAY_PORT='${JAIL_RELAY_PORT:-}' outside reserved range 20000-59999\n"
    fi

    if [ "$ok" -ne 1 ]; then
        printf '[jail] REFUSED: jail is not properly established (fail-closed).\n' >&2
        printf '[jail] Violations:\n' >&2
        printf '%b' "$reasons" >&2
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# jail_guard_name <name> — refuse any name not matching the run prefix.
jail_guard_name() {
    local name="$1"
    if [ -z "${JAIL_PREFIX:-}" ]; then
        _jail_refuse "jail not established; cannot guard name '$name'"
        return 1
    fi
    if _jail_has_prefix "$JAIL_PREFIX" "$name"; then
        return 0
    fi
    _jail_refuse "kill/gc target '$name' does not match required prefix '$JAIL_PREFIX'"
    printf '[jail] Never call qd kill / zmx kill / kill on a bare name.\n' >&2
    return 1
}

# ---------------------------------------------------------------------------
# jail_assert_target_resolves_in_jail <name> — the TARGET-RESOLUTION BELT.
#
# WHY (A4 finding, orchestrator-ruled): the prefix guard (jail_guard_name) checks
# the NAME's SHAPE — but the jailed TS qd-under-test RESOLVES a name through the
# engine's production tiers, including the literal-/tmp legacy zmx scan (Bug-D
# feature; pin-reconciled in TS PR #10). So even a prefix-correct name could, on a
# collision, resolve to a REAL org session's zmx socket / registry path OUTSIDE the
# jail. Read-only exposure in `ls`, but a kill/send/wait against a colliding name
# could touch a real org session. The engine stays unchanged (parity); the HARNESS
# adds this SECOND WALL: before any destructive / session-targeting op, PRE-ASSERT
# that the name resolves ONLY to path(s) under $JAIL_ROOT. Fail-closed on:
#   - MISS       — no resolution for a name we are about to act on, OR
#   - OUT-OF-JAIL — any resolved candidate path is not under $JAIL_ROOT, OR
#   - AMBIGUITY  — both an in-jail AND an out-of-jail candidate resolve.
#
# Resolution is queried the SAME way the engine resolves: (1) the jailed qd's own
# session lookup (`qd info <name> --json`, registry under the JAILED home) and
# (2) the zmx socket path for the name across the jail's resolution tiers
# (ZMX_DIR, XDG_RUNTIME_DIR, and the literal-/tmp legacy tier the finding is about).
# Every path-shaped token in those results must carry the $JAIL_ROOT prefix.
#
# TESTABILITY SEAM: the candidate-gathering is delegated to a resolver whose name
# is held in _JAIL_TARGET_RESOLVER (default: jail__resolve_target_paths). Selftests
# override it to FORGE a resolution result (e.g. an out-of-jail host path, or an
# empty/miss) without needing a real colliding org session. The belt LOGIC
# (miss/out-of-jail/ambiguity -> refuse) is identical regardless of resolver.
_JAIL_TARGET_RESOLVER="${_JAIL_TARGET_RESOLVER:-jail__resolve_target_paths}"

# jail__resolve_target_paths <name> — DEFAULT resolver. Emits, one per line, every
# resolved filesystem path the engine would key on for <name>. Empty output = the
# name resolves to nothing (a MISS). This mirrors the engine's resolution sources;
# it must NOT itself act on the session, only OBSERVE where it would resolve.
jail__resolve_target_paths() {
    local name="$1"
    # (1) Jailed registry lookup: ask the jailed qd for the session as JSON and
    # extract any absolute-path-shaped values (socket paths, dirs, pid files). The
    # jailed HOME means this reads OUR registry; a colliding name that the engine
    # resolves via the legacy /tmp tier would surface a /tmp path here.
    local json
    json="$("$JAIL_SB_CMD" info "$name" --json 2>/dev/null)"
    if [ -n "$json" ]; then
        # Pull "/...":  quoted absolute paths out of the JSON (portable sed; no jq).
        printf '%s\n' "$json" \
            | grep -oE '"/[^"]+"' 2>/dev/null \
            | sed -e 's/^"//' -e 's/"$//'
    fi
    # (2) zmx socket candidates across the resolution tiers. For each tier dir that
    # exists, a socket named after <name> (or a <name>.sock) is a resolved path.
    local tier d
    for d in "${ZMX_DIR:-}" "${XDG_RUNTIME_DIR:-}" "/tmp/zmx-$(id -u 2>/dev/null || echo 0)" "/tmp"; do
        [ -n "$d" ] || continue
        for tier in "$d/$name" "$d/$name.sock" "$d/${name}.socket"; do
            [ -e "$tier" ] && printf '%s\n' "$tier"
        done
    done
}

# jail__resolve_zmx_socket_paths <name> — a ZMX-SOCKET-ONLY resolver (no registry
# query). Used by jail_teardown step 2 for mid-boot daemons that have no registry
# entry yet (the registry-based default resolver would MISS them and skip cleanup).
# Emits every existing zmx socket path for <name> across the resolution tiers; an
# in-jail socket resolves (cleanup proceeds), an out-of-jail one is refused.
jail__resolve_zmx_socket_paths() {
    local name="$1" d tier
    for d in "${ZMX_DIR:-}" "${XDG_RUNTIME_DIR:-}" "/tmp/zmx-$(id -u 2>/dev/null || echo 0)" "/tmp"; do
        [ -n "$d" ] || continue
        for tier in "$d/$name" "$d/$name.sock" "$d/${name}.socket"; do
            [ -e "$tier" ] && printf '%s\n' "$tier"
        done
    done
}

jail_assert_target_resolves_in_jail() {
    local name="$1"
    if [ -z "${JAIL_ROOT:-}" ] || [ -z "${JAIL_PREFIX:-}" ]; then
        _jail_refuse "target-resolution belt: jail not established for '$name'"
        return 1
    fi
    # Gather candidate resolved paths from the (overridable) resolver.
    local paths
    paths="$("$_JAIL_TARGET_RESOLVER" "$name" 2>/dev/null)"

    local in_jail=0 out_jail=0 p
    # Iterate line by line (paths may contain spaces; use a here-string-free read).
    local oldifs="$IFS"
    IFS='
'
    for p in $paths; do
        [ -n "$p" ] || continue
        if _jail_has_prefix "$JAIL_ROOT" "$p"; then
            in_jail=$((in_jail + 1))
        else
            out_jail=$((out_jail + 1))
            _jail_refuse "target '$name' resolves to an OUT-OF-JAIL path: $p"
        fi
    done
    IFS="$oldifs"

    # AMBIGUITY: any out-of-jail candidate is fatal, even alongside in-jail ones.
    if [ "$out_jail" -gt 0 ]; then
        _jail_refuse "target-resolution belt REFUSED '$name' (out-of-jail=$out_jail, in-jail=$in_jail) — could touch a REAL org session. Never act on a name that resolves outside JAIL_ROOT."
        return 1
    fi
    # MISS: nothing resolved for a session we are about to act on. Fail-closed: if
    # the engine cannot show us an in-jail resolution, we will NOT kill/send blind
    # (a later resolution by the engine could land out-of-jail).
    if [ "$in_jail" -eq 0 ]; then
        _jail_refuse "target-resolution belt REFUSED '$name' — no in-jail resolution (MISS). Refusing to act on an unresolved name."
        return 1
    fi
    return 0
}

# --- A4's parallel belt (kept on merge: same orc carry, independent impl; its
# callers live in dryrun/a4-*.sh + A4 scenarios; ours is wired into
# jail_kill_session/teardown/scn_sb_target). Both fail closed. ---
# jail_assert_resolves_in_jail <name> — RESOLUTION BELT (A4 finding, orc-2 ruling
# 2026-06-05: relay-1780630993819-7 item 3b).
#
# WHY: qd's session discovery scans the LITERAL /tmp legacy tier + XDG family by
# design (Bug-D cross-dir discovery — production semantics, preserved per the
# A-track ruling). Inside a jail that means `qd ls`/resolve can SEE — and a
# kill/send could in principle RESOLVE — the host's real org sessions. The
# sbrg- prefix discipline (jail_guard_name) is the first wall; this belt is the
# SECOND: before any destructive or session-targeting live row (kill/send/wait)
# acts on <name>, assert the name resolves to a zmx session whose socket dir
# lives under JAIL_ROOT. Fail CLOSED on miss, ambiguity, or an out-of-jail dir.
jail_assert_resolves_in_jail() {
    local name="$1"
    jail_assert_established || return 1
    jail_guard_name "$name" || return 1
    # The jailed zmx list is ZMX_DIR-pinned to $JAIL_ROOT/zmx; require the name
    # to appear there EXACTLY ONCE (--short prints bare names — the teardown
    # idiom). A name visible only via the host's legacy /tmp tier will NOT
    # appear in the jailed list -> fail closed.
    local zn hits=0
    for zn in $(jail_zmx list --short 2>/dev/null || jail_zmx ls --short 2>/dev/null || true); do
        if [ "$zn" = "$name" ]; then
            hits=$((hits + 1))
        fi
    done
    if [ "$hits" = "1" ]; then
        return 0
    fi
    _jail_refuse "resolution belt: '$name' resolves to $hits sessions in the jailed zmx dir (need exactly 1) — refusing to target it"
    return 1
}

# ---------------------------------------------------------------------------
# jail_sweep_belt_ok — SWEEP BELT for destructive sweep verbs (orc-3 ruling on the
# A5 reconcile hermeticity finding, 2026-06-05). REQUIRED before any destructive
# `qd reconcile` (and any future destructive `qd gc` zmx-reap) live row.
#
# WHY a SECOND belt: jail_assert_resolves_in_jail guards a SINGLE named target. A
# SWEEP verb (reconcile) enumerates its OWN targets across the literal-/tmp Bug-D
# legacy tier (TS-pin-faithful: utils.ts:113 scanRoots default ["/tmp"]) — which
# is UNJAILABLE (a hardcoded "/tmp" + the real uid reaches host org sessions). The
# per-target belt cannot cover targets the verb discovers itself. This belt runs
# the verb in --dry-run (READ-ONLY: the verb's `if !dry_run` guards every
# kill/tombstone), parses the planned reap targets, and REFUSES (returns 1) unless
# EVERY planned zmx-reap target is BOTH sbrg-prefixed AND resolves inside the
# jailed zmx dir. A single out-of-jail planned target fails the whole belt closed.
#
# STANDING CONSTRAINT (orc-3): destructive `qd reconcile` on macOS is OFF
# PERMANENTLY — the destructive live row lives ONLY in the Lima lane (G-X1), where
# this belt ALSO runs (defense in depth). On brano this belt protects future
# phases from re-adding a macOS live sweep.
#
# Args: $1 = the qd-under-test verb invocation that supports --dry-run, default
#       "reconcile". The caller's jail env (jail_sb) is used.
jail_sweep_belt_ok() {
    jail_assert_established || return 1
    local verb="${1:-reconcile}"
    local plan line name bad=0 saw_reap=0
    plan="$("$JAIL_SB_CMD" "$verb" --dry-run 2>/dev/null)" || return 1
    # Each `  reap-wrapper: zmx "<name>" (...)` line names a zmx target to reap.
    # (I1 `tombstone:` targets are pid-keyed registry files under the jailed HOME —
    # HOME-bounded, never the /tmp tier — so the residual exposure is the I3 reap.)
    while IFS= read -r line; do
        case "$line" in
            *reap-wrapper:*zmx\ \"*)
                saw_reap=1
                name="${line#*zmx \"}"; name="${name%%\"*}"
                if ! _jail_has_prefix "$JAIL_PREFIX" "$name"; then
                    _jail_refuse "sweep belt: planned reap target '$name' is NOT jail-prefixed ($JAIL_PREFIX) — refusing destructive $verb"
                    bad=1
                elif ! jail_assert_resolves_in_jail "$name" >/dev/null 2>&1; then
                    _jail_refuse "sweep belt: planned reap target '$name' does not resolve in the jailed zmx dir — refusing destructive $verb"
                    bad=1
                fi
                ;;
        esac
    done <<EOF
$plan
EOF
    if [ "$bad" != "0" ]; then
        printf '[jail] sweep belt FAILED — at least one planned reap target is out-of-jail. Destructive %s REFUSED (fail-closed).\n' "$verb" >&2
        return 1
    fi
    # saw_reap==0 means the plan had no zmx reaps (only I1 tombstones, or nothing) —
    # vacuously safe for the zmx-reap surface.
    return 0
}

# ---------------------------------------------------------------------------
# jail_register_pid <pid> <name>
jail_register_pid() {
    # A7 M10: arity guard. Under `set -u` a 1-arg call used to abort the WHOLE
    # shell at the `$2` expansion (fatal unset-parameter error), firing the
    # caller's EXIT trap mid-setup — the A6 incident trigger. Refuse loudly
    # instead; a refusal composes with set -u, an abort does not.
    if [ "$#" -ne 2 ] || [ -z "${1:-}" ] || [ -z "${2:-}" ]; then
        _jail_refuse "jail_register_pid: usage <pid> <name> (got $# args)"
        return 1
    fi
    local pid="$1" name="$2"
    if ! jail_guard_name "$name"; then
        return 1
    fi
    printf '%s=%s\n' "$pid" "$name" >> "$_JAIL_PID_REGISTRY"
}

_jail_pid_is_registered() {
    [ -f "${_JAIL_PID_REGISTRY:-/nonexistent}" ] || return 1
    grep -q "^${1}=" "$_JAIL_PID_REGISTRY"
}

# ---------------------------------------------------------------------------
# jail_raw_kill <pid> — kill only a registered PID.
jail_raw_kill() {
    local pid="$1"
    if ! _jail_pid_is_registered "$pid"; then
        _jail_refuse "jail_raw_kill: PID $pid not in the jail registry"
        return 1
    fi
    kill "$pid" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# jail_sb / jail_zmx — invoke qd / zmx under the hermetic env. Always re-asserts.
jail_sb() {
    jail_assert_established || return 1
    "$JAIL_SB_CMD" "$@"
}

jail_zmx() {
    jail_assert_established || return 1
    "$JAIL_ZMX_CMD" "$@"
}

# ---------------------------------------------------------------------------
# jail_kill_session <name> — guarded, in-jail session kill.
# TWO WALLS: (1) jail_guard_name checks the name SHAPE (prefix); (2) the
# target-resolution belt checks where the name actually RESOLVES — fail-closed if
# it resolves outside JAIL_ROOT, ambiguously, or not at all. Both must pass before
# the kill reaches the engine.
jail_kill_session() {
    local name="$1"
    jail_assert_established || return 1
    jail_guard_name "$name" || return 1
    jail_assert_target_resolves_in_jail "$name" || return 1
    "$JAIL_SB_CMD" kill "$name" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# jail_teardown — kill prefixed sessions and remove the run dir. In-jail only.
jail_teardown() {
    if [ -z "${JAIL_ROOT:-}" ]; then
        return 0
    fi
    # A7 M10 BELT: steps 1-2 below invoke the SUT / zmx under the PREVAILING env.
    # If establish never completed (or HOME is not jail-rooted), those invocations
    # would read the REAL org registry / real zmx tier — the A6 set-u incident
    # mechanism (reproduced + root-caused 2026-06-05, A7 journal). Refuse the
    # env-dependent steps; step 3 (exact registered PIDs) and the sbrg-guarded rm
    # remain available either way.
    local _jail_env_ok=1
    if [ "${_JAIL_ESTABLISHED:-0}" != "1" ]; then
        _jail_refuse "jail_teardown: jail never fully established — skipping env-dependent steps 1-2"
        _jail_env_ok=0
    else
        case "${HOME:-}" in
            "$JAIL_ROOT"/*) ;;
            *)
                _jail_refuse "jail_teardown: HOME '${HOME:-}' not under JAIL_ROOT — skipping env-dependent steps 1-2"
                _jail_env_ok=0
                ;;
        esac
    fi
    if [ "$_jail_env_ok" = "1" ]; then
    # 1. Ask the jailed qd to list ITS sessions (its registry is in the jailed
    #    HOME) and kill each PREFIXED one via the guarded killer. This catches
    #    detached zmx daemons a scenario started but did not register (e.g. when a
    #    scenario was hard-killed mid-flight before its own cleanup ran). Every
    #    name here is necessarily under JAIL_PREFIX because the jail's registry is
    #    hermetic, but jail_kill_session re-guards the prefix anyway (fail-safe).
    if command -v "$JAIL_SB_CMD" >/dev/null 2>&1 || [ -n "${JAIL_SB_CMD:-}" ]; then
        local names n
        names="$("$JAIL_SB_CMD" ls --all --short 2>/dev/null || true)"
        for n in $names; do
            case "$n" in
                "$JAIL_PREFIX"*) jail_kill_session "$n" >/dev/null 2>&1 || true ;;
            esac
        done
    fi
    # 2. Suspenders: a DETACHED zmx daemon a scenario started may not have a
    #    registry entry yet (boot mid-flight when the scenario was hard-killed), so
    #    qd ls in step 1 misses it. Ask zmx directly — its sockets live in the
    #    jail's ZMX_DIR — and kill each PREFIXED session by name (guarded). zmx
    #    runs under the jailed env (ZMX_DIR/TMPDIR/HOME), so it only sees OUR
    #    sessions. Every name is re-guarded against JAIL_PREFIX.
    if command -v "$JAIL_ZMX_CMD" >/dev/null 2>&1; then
        local zn zlist
        zlist="$("$JAIL_ZMX_CMD" list --short 2>/dev/null || "$JAIL_ZMX_CMD" ls --short 2>/dev/null || true)"
        for zn in $zlist; do
            case "$zn" in
                "$JAIL_PREFIX"*)
                    # Belt both (A4 finding): prefix-guard AND a target-resolution
                    # check. This is the mid-boot-daemon path (no registry entry, so
                    # the registry-based default resolver would MISS), so we belt it
                    # with a ZMX-SOCKET resolver: zmx listed $zn from its OWN jailed
                    # socket dir, so the resolved socket(s) must live under JAIL_ROOT.
                    # A socket resolving OUT of jail (legacy-/tmp collision) is
                    # refused; an in-jail socket is allowed (so cleanup still works).
                    if jail_guard_name "$zn" >/dev/null 2>&1 \
                        && _JAIL_TARGET_RESOLVER=jail__resolve_zmx_socket_paths \
                           jail_assert_target_resolves_in_jail "$zn" >/dev/null 2>&1; then
                        "$JAIL_ZMX_CMD" kill "$zn" --force >/dev/null 2>&1 \
                            || "$JAIL_ZMX_CMD" kill "$zn" >/dev/null 2>&1 || true
                    fi
                    ;;
            esac
        done
    fi
    fi # _jail_env_ok (A7 M10 belt around steps 1-2)
    # 3. Belt: also kill any registered (prefixed) PIDs. Never pkill/patterns.
    if [ -f "${_JAIL_PID_REGISTRY:-/nonexistent}" ]; then
        local line pid
        while IFS= read -r line; do
            pid="${line%%=*}"
            [ -n "$pid" ] && jail_raw_kill "$pid"
        done < "$_JAIL_PID_REGISTRY"
    fi
    # Only remove a dir that is clearly our sandbox.
    case "$JAIL_ROOT" in
        */sbrg-runs/*) rm -rf "$JAIL_ROOT" 2>/dev/null || true ;;
        *) _jail_refuse "jail_teardown: refusing to rm non-sandbox JAIL_ROOT '$JAIL_ROOT'" ;;
    esac
    # Restore the real HOME so the calling shell is not left pointing at a deleted
    # sandbox dir. (jail_establish jailed HOME for invisibility.)
    if [ -n "${JAIL_REAL_HOME:-}" ]; then
        export HOME="$JAIL_REAL_HOME"
    fi
    # Make teardown IDEMPOTENT: a second call (e.g. an EXIT trap firing after an
    # explicit teardown) is a no-op instead of re-running kills against a
    # torn-down jail. The early-return guard at the top keys on JAIL_ROOT.
    JAIL_ROOT=""
    _JAIL_ESTABLISHED=""
}

# ---------------------------------------------------------------------------
# jail_require_destructive_ok — mirror of safety.sh require_destructive_ok.
#
# A destructive op may run ONLY if ALL THREE hold (POSITIVE "I am the disposable
# Lima sandbox"): (a) sentinel /etc/qd-rust-lima, (b) hostname!=brano,
# (c) QD_RUST_DESTRUCTIVE_OK=1. On brano this ALWAYS fails closed.
jail_require_destructive_ok() {
    local ok=1 reasons=""
    if [ ! -f /etc/qd-rust-lima ]; then
        ok=0
        reasons="${reasons}  (a) /etc/qd-rust-lima sentinel not found\n"
    fi
    local hn
    hn="$(hostname 2>/dev/null || printf '')"
    case "$hn" in
        *brano*)
            ok=0
            reasons="${reasons}  (b) hostname '${hn}' contains 'brano' — production machine\n"
            ;;
    esac
    if [ "${QD_RUST_DESTRUCTIVE_OK:-}" != "1" ]; then
        ok=0
        reasons="${reasons}  (c) QD_RUST_DESTRUCTIVE_OK is not '1'\n"
    fi
    if [ "$ok" -ne 1 ]; then
        printf '[jail] REFUSED: destructive op cannot run here (fail-closed).\n' >&2
        printf '[jail] Requires ALL of: (a) /etc/qd-rust-lima, (b) hostname!=brano, (c) QD_RUST_DESTRUCTIVE_OK=1\n' >&2
        printf '[jail] Unmet:\n' >&2
        printf '%b' "$reasons" >&2
        return 1
    fi
    return 0
}
