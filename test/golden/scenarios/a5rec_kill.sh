#!/usr/bin/env bash
# scenario: a5rec kill — `kill` shapes: no-such-session refusal, LIVE direct kill
# (W3: no prompt, non-TTY, no --force), LIVE kill with the deprecated no-op
# --force (flag must stay parse-accepted), and the ambiguous-prefix LOUD refusal
# (the relocated W3 safety belt). Destructive runs are pre-asserted with
# jail_assert_resolves_in_jail (the resolution belt) before they fire.
#
# WART-WAVE RE-MINT (ADD-15 W3+W4, 2026-06-05): the confirm prompt is REMOVED
# (kill executes directly; old non-TTY refusal path is gone) and the success
# line is the W4 unambiguous format `killed <registry-name> (zmx <name>, pid N)`.
# Fixture re-recorded; the old `Killed session "<label>".` bytes retire WITH this
# header as the named reason. Divergence rows: exec/divergence-table.md W3 + W4.
# MUTATION TEETH: re-adding the prompt/refusal REDs the no-force live row
# (kill would block or exit 1); removing the --force flag from clap REDs the
# no-op row (usage error, exit 2-class); reverting the W4 format REDs both
# fixture compares + scn_assert.
# tooling: record.sh@388ccd9 normalize.sh@b581f75.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_a5_lifecycle_lib.sh"

SCN_NAME="a5rec-kill"
SCN_BUDGET_MS=40000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/a5-lifecycle/normalized/kill.txt"

scn_run() {
    a5_make_fake_claude >/dev/null
    {
        echo "# RECORDED-FROM pin=8c59ec4 verb=kill (wart-wave re-mint: ADD-15 W3 direct-kill + W4 line; belt pre-asserts destructive runs)"
        echo "\$ qd kill --force qdrg-nope (no such session)"
        scn_qd kill --force "${JAIL_PREFIX}nope" 2>&1; echo "exit=$?"
        echo "\$ qd kill qdrg-nope </dev/null (no --force, non-TTY, no such session)"
        scn_qd kill "${JAIL_PREFIX}nope" </dev/null 2>&1; echo "exit=$?"
    } > "$SCN_OUT"

    # LIVE direct kill — W3 row: NO --force, stdin non-TTY (</dev/null), no
    # prompt, no refusal; W4 line + exit 0. THE mutation row for the prompt.
    local NAME="${JAIL_PREFIX}k1"
    if a5_spawn_fake "$NAME"; then
        if jail_assert_resolves_in_jail "$NAME" >/dev/null 2>&1; then
            {
                echo "\$ qd kill qdrg-k1 </dev/null (LIVE, no --force, non-TTY — W3 direct kill)"
                scn_qd kill "$NAME" </dev/null 2>&1; echo "exit=$?"
            } >> "$SCN_OUT"
        else
            echo "# BELT REFUSED kill of $NAME — not recorded (fail-closed)" >> "$SCN_OUT"
        fi
    else
        echo "# could not spawn $NAME for live-kill row" >> "$SCN_OUT"
    fi

    # LIVE kill with --force — the deprecated no-op must stay PARSE-ACCEPTED
    # (15+ scripted callers; removing the flag REDs this row with a usage error).
    local NAME2="${JAIL_PREFIX}k2"
    if a5_spawn_fake "$NAME2"; then
        if jail_assert_resolves_in_jail "$NAME2" >/dev/null 2>&1; then
            {
                echo "\$ qd kill --force qdrg-k2 (LIVE, deprecated no-op flag accepted)"
                scn_qd kill --force "$NAME2" 2>&1; echo "exit=$?"
            } >> "$SCN_OUT"
        else
            echo "# BELT REFUSED kill of $NAME2 — not recorded (fail-closed)" >> "$SCN_OUT"
        fi
    else
        echo "# could not spawn $NAME2 for no-op-force row" >> "$SCN_OUT"
    fi

    # AMBIGUOUS-PREFIX negative control (the relocated safety belt, red-team R9):
    # two live sessions sharing a prefix; killing by the prefix must refuse LOUD
    # (exit 1) and kill NEITHER. Captured as deterministic key=value tokens (the
    # listing itself carries volatile codes/ids — not fixture material).
    local AMBA="${JAIL_PREFIX}amb-a" AMBB="${JAIL_PREFIX}amb-b"
    if a5_spawn_fake "$AMBA" && a5_spawn_fake "$AMBB"; then
        local ambout ambrc
        ambout="$(scn_qd kill "${JAIL_PREFIX}amb" </dev/null 2>&1)"; ambrc=$?
        local alive
        alive="$( (jail_zmx list --short 2>/dev/null || jail_zmx ls --short 2>/dev/null) | grep -c "${JAIL_PREFIX}amb" || true)"
        {
            echo "\$ qd kill qdrg-amb (ambiguous prefix — LOUD refusal, kills neither)"
            echo "ambiguous_exit=$ambrc"
            printf 'ambiguous_loud=%s\n' "$(printf '%s' "$ambout" | grep -q 'Ambiguous' && echo 1 || echo 0)"
            echo "ambiguous_survivors=$alive"
        } >> "$SCN_OUT"
        # cleanup (belt + direct kill, per target).
        jail_assert_resolves_in_jail "$AMBA" >/dev/null 2>&1 && scn_qd kill "$AMBA" </dev/null >/dev/null 2>&1
        jail_assert_resolves_in_jail "$AMBB" >/dev/null 2>&1 && scn_qd kill "$AMBB" </dev/null >/dev/null 2>&1
    else
        echo "# could not spawn ambiguous pair — amb rows not recorded" >> "$SCN_OUT"
    fi
    printf '0\n' > "$SCN_OUT.exit"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q "No session matching \"${JAIL_PREFIX}nope\"" "$SCN_OUT" || return 1
    # W4 line, both live rows (W3 no-force + no-op --force). Anchored on the full
    # parenthetical so a partial/ambiguous revert cannot pass.
    grep -q "killed ${JAIL_PREFIX}k1 (zmx ${JAIL_PREFIX}k1, pid " "$SCN_OUT" || return 1
    grep -q "killed ${JAIL_PREFIX}k2 (zmx ${JAIL_PREFIX}k2, pid " "$SCN_OUT" || return 1
    # W3 teeth: the prompt/refusal/old-format bytes must be ABSENT.
    ! grep -q "Refusing to kill" "$SCN_OUT" || return 1
    ! grep -q "\[y/N\]" "$SCN_OUT" || return 1
    ! grep -q "Killed session" "$SCN_OUT" || return 1
    # Ambiguous-prefix belt: loud exit 1, neither session killed.
    grep -q "ambiguous_exit=1" "$SCN_OUT" || return 1
    grep -q "ambiguous_loud=1" "$SCN_OUT" || return 1
    grep -q "ambiguous_survivors=2" "$SCN_OUT" || return 1
}
