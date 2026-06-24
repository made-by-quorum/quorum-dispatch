#!/usr/bin/env bash
# test/golden/lib/check_python.sh — enforce the python3 floor for the recorder.
#
# Option B (ADR 0002) keeps the proven PTY mechanics in Python. The recorder uses
# only stdlib (os/pty/select/signal/fcntl/termios/struct/base64) + f-strings, so
# the floor is Python 3.6. This check fails CLOSED with a clear message below the
# floor, BEFORE any recording runs. Bash 3.2 compatible (no associative arrays,
# integer-only arithmetic; no `python -c` version-tuple gymnastics in shell).
#
# Source it and call `check_python_floor` (returns non-zero + message if below
# floor or python3 missing), or run this file directly as a self-contained gate.
# ---------------------------------------------------------------------------

# The pinned floor. Keep in sync with ADR 0002.
PY_FLOOR_MAJOR=3
PY_FLOOR_MINOR=6

check_python_floor() {
    local py="${PYTHON:-python3}"
    if ! command -v "$py" >/dev/null 2>&1; then
        printf '[python-floor] REFUSED: %s not found on PATH. The golden recorder requires python3 >= %d.%d (ADR 0002).\n' \
            "$py" "$PY_FLOOR_MAJOR" "$PY_FLOOR_MINOR" >&2
        return 1
    fi
    # Ask python for its major/minor as two integers on one line. This is the only
    # robust way to compare versions; the shell side stays integer-only.
    local ver
    ver="$("$py" -c 'import sys; print("%d %d" % (sys.version_info[0], sys.version_info[1]))' 2>/dev/null)"
    if [ -z "$ver" ]; then
        printf '[python-floor] REFUSED: could not determine %s version.\n' "$py" >&2
        return 1
    fi
    local maj min
    maj="${ver%% *}"
    min="${ver##* }"
    # Compare (major, minor) >= (floor major, floor minor) with integer math only.
    if [ "$maj" -lt "$PY_FLOOR_MAJOR" ] 2>/dev/null; then
        :
    elif [ "$maj" -gt "$PY_FLOOR_MAJOR" ] 2>/dev/null; then
        return 0
    elif [ "$min" -ge "$PY_FLOOR_MINOR" ] 2>/dev/null; then
        return 0
    fi
    printf '[python-floor] REFUSED: %s is %s.%s; the golden recorder requires >= %d.%d (ADR 0002).\n' \
        "$py" "$maj" "$min" "$PY_FLOOR_MAJOR" "$PY_FLOOR_MINOR" >&2
    return 1
}

if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    check_python_floor
    exit $?
fi
