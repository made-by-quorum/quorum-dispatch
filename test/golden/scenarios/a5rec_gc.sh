#!/usr/bin/env bash
# scenario: a5rec gc — TS qd `gc` lifecycle: dry-run(empty), dry-run(forged aged
# candidate), real prune→trash, list-trash, recover, recover-collision refusal,
# purge(>30d). Records as ONE byte-exact transcript fixture (jail paths/ages
# normalized). Pin 0d0fa9e. tooling: record.sh@388ccd9 normalize.sh@b581f75.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_a5_lifecycle_lib.sh"

SCN_NAME="a5rec-gc"
SCN_BUDGET_MS=25000
SCN_CLASS="byte-exact"
SCN_FIXTURE="fixtures/a5-lifecycle/normalized/gc.txt"

scn_run() {
    local PROJ="$HOME/.claude/projects/jail-proj"; mkdir -p "$PROJ"
    local DEADSID="deadsession-rec" DJ
    DJ="$PROJ/$DEADSID.jsonl"
    {
        echo "# RECORDED-FROM pin=0d0fa9e verb=gc (forged file mtimes >7d / prunedAt >30d)"
        echo "\$ qd gc --dry-run (empty)"
        scn_sb gc --dry-run 2>&1
        echo "\$ qd gc --list-trash (empty)"
        scn_sb gc --list-trash 2>&1
    } > "$SCN_OUT"
    # Forge an aged dead jsonl candidate (>7d), no live PID claims its sid.
    printf '{"type":"summary"}\n' > "$DJ"
    touch -t "$(date -v-8d +%Y%m%d%H%M 2>/dev/null || date -d '8 days ago' +%Y%m%d%H%M)" "$DJ" 2>/dev/null
    {
        echo "\$ qd gc --dry-run (forged aged candidate)"
        scn_sb gc --dry-run 2>&1
        echo "\$ qd gc (real prune to trash)"
        scn_sb gc 2>&1
        echo "\$ qd gc --list-trash (after prune)"
        scn_sb gc --list-trash 2>&1
        echo "\$ qd gc --recover deadsession-rec"
        scn_sb gc --recover "$DEADSID" 2>&1
    } >> "$SCN_OUT"
    # Re-trash the recovered file (re-age it so gc sees it as a candidate), then
    # drop a COLLIDING original back BEFORE recover so recover refuses.
    touch -t "$(date -v-8d +%Y%m%d%H%M 2>/dev/null || date -d '8 days ago' +%Y%m%d%H%M)" "$DJ" 2>/dev/null
    scn_sb gc >/dev/null 2>&1
    printf '{"type":"summary"}\n' > "$DJ"
    {
        echo "\$ qd gc --recover deadsession-rec (collision: original exists → refuse)"
        scn_sb gc --recover "$DEADSID" 2>&1
        echo "exit=$?"
    } >> "$SCN_OUT"
    # Age the trash meta prunedAt >30d, then purge.
    local TRASH="$HOME/.claude/trash" meta OLD_ISO
    meta="$(ls "$TRASH"/*"$DEADSID".jsonl_meta.json 2>/dev/null | head -1)"
    if [ -n "$meta" ]; then
        OLD_ISO="$(date -u -v-31d +%Y-%m-%dT%H:%M:%S.000Z 2>/dev/null || date -u -d '31 days ago' +%Y-%m-%dT%H:%M:%S.000Z)"
        sed "s/\"prunedAt\":[^,]*/\"prunedAt\": \"$OLD_ISO\"/" "$meta" > "$meta.tmp" && mv "$meta.tmp" "$meta"
    fi
    {
        echo "\$ qd gc --purge (>30d trash)"
        scn_sb gc --purge 2>&1
    } >> "$SCN_OUT"
    printf '0\n' > "$SCN_OUT.exit"
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q "No GC candidates found. Everything is clean." "$SCN_OUT" || return 1
    grep -q "Trash is empty." "$SCN_OUT" || return 1
    grep -q "deadsession-rec" "$SCN_OUT" || return 1
    grep -q "dry run — no changes made" "$SCN_OUT" || return 1
    grep -q "Recovered" "$SCN_OUT" || return 1
    grep -q "Cannot recover:" "$SCN_OUT" || return 1
    grep -q "already exists." "$SCN_OUT" || return 1
}
