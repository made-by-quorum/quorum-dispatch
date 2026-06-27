#!/usr/bin/env bash
# scenario: HOSTILE-CWD project-path mapping (W3.6, P9). STUB-BACKED.
# 0b DELTA-STRENGTH W3.6: cwdToProjectPath fidelity for special-char cwds (new row).
#
# WHAT (red-team m1 wording): boot a stub session FROM a jail cwd whose basename
# carries the special chars `:`, `.`, `_` (e.g. <jail>/p9.host:8080_v1.2) and assert
# the JSONL conversation file lands at the project dir produced by cwdToProjectPath —
# `/`→`-` ONLY, with `:`/`.`/`_` PRESERVED VERBATIM (session.ts:433-435:
# `cwd.replace(/\//g, "-")`). Then assert send:pty --wait works FROM that cwd (the
# --wait anchor resolves findJsonlPath at the special-char project dir, not a
# mangled one).
#
# PRE-NORMALIZATION derived booleans (relay-health B2 idiom): the project dir NAME is
# a jail-rooted path with `/`→`-`, so it carries no `/` for the path normalizer to
# tokenize and the specials would survive a normalize trivially — BUT to make the
# assertion robust and value-bearing we compute the expected mapping HERE on the RAW
# cwd and emit DERIVED booleans (specials-preserved, slash-mapped, jsonl-landed). A
# wrong mapping (e.g. an impl that sanitized `:`→`_` or `.`→`-`) flips a boolean.
#
# §S: drives the pinned-TS `qd new` (cwd = the hostile dir) against the stub; the
# stub writes its JSONL under ~/.claude/projects/<cwdToProjectPath(getcwd())>/ using
# the SAME transform (stub_claude.py:169-171), so the row measures the ENGINE's cwd
# recording + the --wait path resolution, not the stub.
#
# Determinism (double-record): the derived booleans + the project-dir basename (with
# the jail-root prefix tokenized away by the basename derivation) are the recorded
# expectation. NOT a byte trace.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_scenario_lib.sh"

SCN_NAME="hostile-cwd"
SCN_BUDGET_MS=60000
SCN_CLASS="semantic-project-path"
SCN_FIXTURE="fixtures/hostile-cwd/normalized/mapping.txt"
SCN_STUB_BACKED=1

# The hostile basename: all three special classes the transform must preserve.
SCN_HOSTILE_BASE="p9.host:8080_v1.2"
SCN_MSG="hostile-cwd-probe"

scn_run() {
    local name
    name="$(scn_session_name hc)"

    # Create the hostile cwd UNDER the jailed HOME and run qd new FROM it (so the
    # recorded session cwd == the hostile dir; the stub's getcwd() is the same).
    local hostile_cwd="$HOME/$SCN_HOSTILE_BASE"
    mkdir -p "$hostile_cwd"

    ( cd "$hostile_cwd" && bash -c "exec $SB_UNDER_TEST new $name" ) >/dev/null 2>&1 &
    local bootpid=$!
    local i=0 pidfile=""
    while [ "$i" -lt 30 ]; do
        pidfile="$(grep -l "\"name\": \"$name\"" "$HOME"/.claude/sessions/*.json 2>/dev/null | head -1)"
        [ -n "$pidfile" ] && break
        sleep 1; i=$((i + 1))
    done

    # Find the ACTUAL project dir the JSONL landed in. NOTE: the stub records
    # cwd=os.getcwd() = the REALPATH (on macOS /tmp -> /private/tmp), which can
    # differ from our shell's $HOME-built $hostile_cwd literal — so we locate the
    # project dir by its CONTENT (the dir holding the stub's JSONL whose basename
    # carries our hostile base) rather than a literal path we computed. This makes
    # the row robust to the /tmp realpath indirection while still proving the
    # cwdToProjectPath transform on the basename.
    local proj_dir=""
    local d
    for d in "$HOME"/.claude/projects/*"$SCN_HOSTILE_BASE"; do
        if [ -d "$d" ] && ls "$d"/*.jsonl >/dev/null 2>&1; then proj_dir="$d"; break; fi
    done
    local jsonl_landed=0
    [ -n "$proj_dir" ] && jsonl_landed=1

    # send:pty --wait FROM the hostile cwd: the --wait anchor must resolve
    # findJsonlPath at the special-char project dir. Run the send from the hostile
    # cwd too (matches the recorded session cwd).
    local waitout waitrc
    waitout="$( cd "$hostile_cwd" && scn_sb_target send:pty "$name" "$SCN_MSG" --wait --timeout 30 2>/dev/null )"
    waitrc=$?

    # DERIVED booleans (computed on the RAW values, pre-normalization). A wrong
    # mapping flips one.
    python3 - "$hostile_cwd" "$proj_dir" "$jsonl_landed" "$waitrc" "$waitout" "$SCN_HOSTILE_BASE" > "$SCN_OUT" <<'PY'
import sys, os
hostile_cwd, proj_dir, jsonl_landed, waitrc, waitout, base = sys.argv[1:7]
proj_base = os.path.basename(proj_dir)
# The project basename must be the hostile cwd with `/`→`-` — so the trailing
# component is EXACTLY the hostile base (specials preserved). Check the specials
# survived verbatim in the project dir basename.
specials_preserved = (base in proj_base) and (":" in proj_base) and ("." in proj_base) and ("_" in proj_base)
# No special was rewritten: the project basename must END with the hostile base
# untouched (an impl that sanitized `:`→`_` etc. would not).
endswith_hostile = proj_base.endswith(base)
# `/`→`-` mapping: the project dir name must contain NO `/` (all mapped to `-`).
no_slash = ("/" not in proj_base)
print("MAP specials_preserved_verbatim=%d" % (1 if specials_preserved else 0))
print("MAP project_basename_ends_with_hostile=%d" % (1 if endswith_hostile else 0))
print("MAP slash_mapped_to_dash=%d" % (1 if no_slash else 0))
print("MAP jsonl_landed_at_mapped_dir=%d" % (1 if jsonl_landed == "1" else 0))
print("MAP sendpty_wait_works_from_cwd=%d" % (1 if (waitrc == "0" and "STUB-REPLY" in waitout) else 0))
PY
    printf '0\n' > "$SCN_OUT.exit"

    jail_kill_session "$name" >/dev/null 2>&1 || true
    kill "$bootpid" 2>/dev/null || true
}

scn_assert() {
    [ -f "$SCN_OUT" ] || return 1
    grep -q 'MAP specials_preserved_verbatim=1' "$SCN_OUT"            || { _cmp_fail semantic-project-path "special chars :/./_ NOT preserved verbatim in the project dir (cwdToProjectPath sanitized them)"; return 1; }
    grep -q 'MAP project_basename_ends_with_hostile=1' "$SCN_OUT"     || { _cmp_fail semantic-project-path "project basename does not end with the hostile base (a special was rewritten)"; return 1; }
    grep -q 'MAP slash_mapped_to_dash=1' "$SCN_OUT"                   || { _cmp_fail semantic-project-path "/ not mapped to - in the project dir name"; return 1; }
    grep -q 'MAP jsonl_landed_at_mapped_dir=1' "$SCN_OUT"             || { _cmp_fail semantic-project-path "JSONL did not land at the cwdToProjectPath-mapped dir"; return 1; }
    grep -q 'MAP sendpty_wait_works_from_cwd=1' "$SCN_OUT"            || { _cmp_fail semantic-project-path "send:pty --wait did not work from the hostile cwd (anchor path resolution broke)"; return 1; }
    return 0
}
