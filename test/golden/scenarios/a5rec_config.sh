#!/usr/bin/env bash
# scenario: a5rec config — TS sb `config` set/get/reveal/path/unset lifecycle,
# FILE backend (SB_SECRET_BACKEND=file, NO keychain), fake placeholder key.
# Records the full config lifecycle transcript as ONE byte-exact fixture.
# Pin 0d0fa9e. tooling: record.sh@388ccd9 normalize.sh@b581f75.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_a5_lifecycle_lib.sh"

SCN_NAME="a5rec-config"
SCN_BUDGET_MS=20000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/a5-lifecycle/normalized/config.txt"

scn_run() {
    export SB_SECRET_BACKEND=file   # FILE backend only — daytime-deferred keychain row.
    local K="$A5_FAKE_OPENROUTER_KEY"
    {
        echo "# RECORDED-FROM pin=0d0fa9e verb=config backend=file (fake key, masked+reveal)"
        echo "\$ sb config path (no key set)"
        scn_sb config path 2>&1
        echo "\$ sb config set openrouter-key <FAKE>"
        scn_sb config set openrouter-key "$K" 2>&1
        echo "\$ sb config get openrouter-key (masked)"
        scn_sb config get openrouter-key 2>&1
        echo "\$ sb config get openrouter-key --reveal"
        scn_sb config get openrouter-key --reveal 2>&1
        echo "\$ sb config path (key set)"
        scn_sb config path 2>&1
        echo "\$ sb config unset openrouter-key"
        scn_sb config unset openrouter-key 2>&1
        echo "\$ sb config get openrouter-key (after unset)"
        scn_sb config get openrouter-key 2>&1
    } > "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q "Backend:     file" "$SCN_OUT" || return 1
    grep -q "Stored openrouter-key (backend: file)." "$SCN_OUT" || return 1
    grep -q "openrouter-key: ••••0000" "$SCN_OUT" || return 1
    grep -q "openrouter-key: $A5_FAKE_OPENROUTER_KEY" "$SCN_OUT" || return 1
    grep -q "Unset openrouter-key." "$SCN_OUT" || return 1
    grep -q "openrouter-key: not set." "$SCN_OUT" || return 1
}
