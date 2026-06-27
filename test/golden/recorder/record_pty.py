#!/usr/bin/env python3
"""record_pty.py — PTY recorder for the qd-rust golden harness.

Ported from the proven spike harness (spike/empirical/pty_capture.py +
pty_drive.py). Forks a PTY, runs a command, captures EVERY raw byte it emits to
<outfile>, optionally injects bytes after a delay, then terminates the CLIENT
process (NOT any detached daemon — same discipline as the spike: SIGTERM, brief
grace, SIGKILL).

This is the recorder half of the Option-B split (see ADR golden-harness-language):
proven PTY mechanics stay in Python; the jail, normalization, comparison, and
timeout-budget layers are first-class in the bash asserter.

Usage:
  record_pty.py --out <file> --secs <N> [--cols C] [--rows R]
                [--inject-b64 <b64> --inject-delay <S>]
                [--winch-after <S> --winch-cols C2 --winch-rows R2]
                -- <cmd> [args...]

Exit code is the recorder's own status (0 = captured ok, 2 = usage error). The
child's exit status is written to <outfile>.exit so the asserter can compare exit
codes (a load-bearing, never-normalized value).
"""
import os, sys, pty, select, time, signal, fcntl, termios, struct, base64


def _set_winsize(fd, rows, cols):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def parse_args(argv):
    opts = {
        "out": None, "secs": None, "cols": 80, "rows": 24,
        "inject_b64": "", "inject_delay": 0.0,
        "winch_after": None, "winch_cols": 80, "winch_rows": 24,
        "winch_storm": 0,  # if >0, fire N SIGWINCH-via-resize events spread over the run
    }
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--":
            return opts, argv[i + 1:]
        key = a[2:].replace("-", "_") if a.startswith("--") else None
        if key is None or key not in opts and key not in (
            "out", "secs", "cols", "rows", "inject_b64", "inject_delay",
            "winch_after", "winch_cols", "winch_rows", "winch_storm",
        ):
            sys.stderr.write("record_pty: unknown arg %r\n" % a)
            sys.exit(2)
        val = argv[i + 1]
        if key in ("cols", "rows", "winch_cols", "winch_rows", "winch_storm"):
            opts[key] = int(val)
        elif key in ("secs", "inject_delay", "winch_after"):
            opts[key] = float(val)
        else:
            opts[key] = val
        i += 2
    sys.stderr.write("record_pty: missing -- before command\n")
    sys.exit(2)


def main():
    opts, cmd = parse_args(sys.argv[1:])
    if not opts["out"] or opts["secs"] is None or not cmd:
        sys.stderr.write(__doc__)
        sys.exit(2)

    out = opts["out"]
    secs = opts["secs"]
    inject = base64.b64decode(opts["inject_b64"]) if opts["inject_b64"] else b""

    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(cmd[0], cmd)
        os._exit(127)

    _set_winsize(fd, opts["rows"], opts["cols"])

    start = time.time()
    injected = not inject
    winched = opts["winch_after"] is None
    # SIGWINCH storm: schedule N resize events across the run if requested.
    storm = opts["winch_storm"]
    storm_done = 0
    storm_interval = (secs / storm) if storm > 0 else None

    child_status = None
    with open(out, "wb") as f:
        while time.time() - start < secs:
            now = time.time() - start
            if not injected and now >= opts["inject_delay"]:
                try:
                    os.write(fd, inject)
                except OSError:
                    pass
                injected = True
            if not winched and now >= opts["winch_after"]:
                _set_winsize(fd, opts["winch_rows"], opts["winch_cols"])
                try:
                    os.kill(pid, signal.SIGWINCH)
                except ProcessLookupError:
                    pass
                winched = True
            if storm_interval is not None and storm_done < storm:
                if now >= (storm_done + 1) * storm_interval - storm_interval:
                    # Alternate between two sizes to force real reflow each tick.
                    r = opts["rows"] + (storm_done % 2)
                    c = opts["cols"] + (storm_done % 3)
                    _set_winsize(fd, r, c)
                    try:
                        os.kill(pid, signal.SIGWINCH)
                    except ProcessLookupError:
                        pass
                    storm_done += 1
            r, _, _ = select.select([fd], [], [], 0.05)
            if fd in r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                f.write(data)
                f.flush()

    # Terminate the CLIENT (never a detached daemon). Same discipline as spike.
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.kill(pid, sig)
            time.sleep(0.2)
        except ProcessLookupError:
            break
    try:
        _, child_status = os.waitpid(pid, 0)
    except ChildProcessError:
        child_status = None

    # Record the child exit code (load-bearing; never normalized).
    exit_code = ""
    if child_status is not None:
        if os.WIFEXITED(child_status):
            exit_code = str(os.WEXITSTATUS(child_status))
        elif os.WIFSIGNALED(child_status):
            exit_code = "signal-%d" % os.WTERMSIG(child_status)
    with open(out + ".exit", "w") as ef:
        ef.write(exit_code + "\n")

    sys.exit(0)


if __name__ == "__main__":
    main()
