#!/usr/bin/env bash
# passb-diag-paste-burst.sh — DIAGNOSTIC re-run of the send-pty-paste-burst row
# against the Rust binary (pass-b closure, sbr-pa4-lead2). DEV-TIME EVIDENCE,
# NOT a re-batch: the first red's bytes were lost to jail teardown; this run
# snapshots the jail's scn-out + JSONL + session records continuously so the
# red can be characterized. DRYRUN-NOT-ORACLE.
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DIAG="$HERE/dryrun/passb-diag"
mkdir -p "$DIAG"
SUT="${SUT:-/home/u/work/wt-a4-passb/target/debug/qd}"

# Launch the real harness run in the background.
( cd "$HERE" && QD_UNDER_TEST="$SUT" ./verify.sh --scenario scenarios/send_pty_paste_burst.sh \
    > "$DIAG/verify.log" 2>&1; echo "$?" > "$DIAG/verify.rc" ) &
vpid=$!

base="${TMPDIR:-/tmp}"; base="${base%/}/qdrg-runs"
# Poll-copy jail artifacts until verify exits (teardown removes the jail root).
while kill -0 "$vpid" 2>/dev/null; do
    for d in "$base"/*/; do
        [ -d "$d" ] || continue
        cp "$d/scn-out.raw" "$DIAG/scn-out.raw" 2>/dev/null || true
        for j in "$d"/home/.claude/projects/*/*.jsonl; do
            [ -f "$j" ] && cp "$j" "$DIAG/transcript.jsonl" 2>/dev/null
        done
        for s in "$d"/home/.claude/sessions/*.json; do
            [ -f "$s" ] && cp "$s" "$DIAG/session-record.json" 2>/dev/null
        done
        # zmx list of the jailed dir, for boot diagnosis
        ls "$d" > "$DIAG/jailroot-ls.txt" 2>/dev/null || true
    done
    sleep 1
done
wait "$vpid" 2>/dev/null
echo "verify rc=$(cat "$DIAG/verify.rc" 2>/dev/null)"
echo "--- captured artifacts ---"; ls -la "$DIAG"
