#!/usr/bin/env python3
"""fresh-install-smoke.py — post-install functional smoke for a shipped `qd`.

`packaging/homebrew/smoke.sh` proves the PACKAGE installs (formula builds, `qd`
and `qw` land in `bin`, `brew test` passes). It never opens a session, so a
fresh install can pass it and still be unable to do the one thing it exists for.
This script is the other half: against an ALREADY-INSTALLED `qd`, it drives the
real verbs against every harness the machine actually has.

It checks the two DRIVERS separately, because qd's surface genuinely differs
between them (`bin/qd/driver.rs::resolve_driver`):

  human  — a real TTY, no agent env marker. `qd ls` renders the TABLE, `qd start`
           hands the terminal over, `qd attach` is the way in. Driven here over a
           real pty (`pty.fork`), because a pipe would silently exercise the
           other driver and prove nothing about the human path.
  agent  — a pipe (or `QD_SESSION_ID` in the env). `qd ls` auto-defaults to
           JSON, nothing ever attaches, and `qd send` is how work arrives.

Per the ruling in `bin/qd/verbs/lifecycle.rs`, an agent-driven `qd start` on the
CLAUDE-CODE lane is refused unless `--interactive` is passed (a bare agent start
is a no-op turn; an agent start with `-p` is a one-off print run dispatch does
not spawn). The agent lane below passes `--interactive` for claude-code and
nothing else — if that refusal ever moves, this script is where it shows up.

Usage:
    python3 dispatch/scripts/fresh-install-smoke.py                # all detected providers
    python3 dispatch/scripts/fresh-install-smoke.py --providers codex,pi
    python3 dispatch/scripts/fresh-install-smoke.py --lane agent --verbose
    python3 dispatch/scripts/fresh-install-smoke.py --setup-fix    # run `qd setup --fix` first

Exit: 0 = every check passed, 1 = at least one FAIL, 2 = could not even start
(no `qd` on PATH, unusable `qd setup --json`).

Every session it creates is named `<prefix>-*` (default prefix `qdsmoke`) and is
stopped on the way out, including on Ctrl-C — `--keep` leaves them for
inspection.
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time

# --- provider table ---------------------------------------------------------

# harness id (`qd setup --json` .harnesses[].id) -> `--provider` spelling.
# The two mismatched rows are why this is a table and not `id`: setup probes for
# the BINARY (`claude`), while the provider is the LANE (`claude-code`); and
# opencode's provider id is `acp/opencode`. Mirrors
# `verbs/lifecycle.rs::provider_id_for_harness`.
HARNESS_TO_PROVIDER = {
    "claude": "claude-code",
    "codex": "codex",
    "pi": "pi",
    "opencode": "acp/opencode",
}

# `acp/claude-code` has no harness row of its own — it is the claude binary
# spoken to over ACP — so it is derived from the claude harness rather than
# detected. Daemon-only; opt out with --no-acp-claude.
ACP_CLAUDE = "acp/claude-code"

# The mux client's detach key, Ctrl+\ (`qrmux/src/client/mod.rs::DETACH_KEY`).
DETACH_KEY = b"\x1c"

ANSI_RE = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[@-Z\\-_]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")

PASS, FAIL, SKIP, WARN = "PASS", "FAIL", "SKIP", "WARN"


# --- result recording -------------------------------------------------------


class Report:
    def __init__(self, verbose: bool):
        self.rows: list[dict] = []
        self.verbose = verbose

    def record(self, lane, provider, check, status, detail="", cmd=None, output=""):
        row = {
            "lane": lane,
            "provider": provider,
            "check": check,
            "status": status,
            "detail": detail,
            "cmd": " ".join(cmd) if cmd else None,
        }
        self.rows.append(row)
        tag = {PASS: "ok  ", FAIL: "FAIL", SKIP: "skip", WARN: "warn"}[status]
        where = f"{lane}/{provider}" if provider else lane
        line = f"  [{tag}] {where}: {check}"
        if detail:
            line += f" — {detail}"
        print(line, flush=True)
        # A failure's evidence is the thing you actually need, so it prints even
        # without --verbose; a passing check's output is noise until asked for.
        if output and (status == FAIL or self.verbose):
            for out_line in output.rstrip().splitlines()[-40:]:
                print(f"         | {out_line}", flush=True)

    def failed(self) -> int:
        return sum(1 for r in self.rows if r["status"] == FAIL)

    def counts(self) -> dict:
        out = {PASS: 0, FAIL: 0, SKIP: 0, WARN: 0}
        for r in self.rows:
            out[r["status"]] += 1
        return out


# --- process runners --------------------------------------------------------


def strip_ansi(data: bytes) -> str:
    return ANSI_RE.sub(b"", data).decode("utf-8", "replace")


def table_lists(rendered: str, name: str) -> bool:
    """Is `name` one of the rows of a rendered human `qd ls` table?

    Not a substring test: the table's Name column is fixed-width and elides a
    long name to `<prefix>\u2026`, so `name in rendered` is false for exactly the
    sessions this script creates (prefix + lane + provider is comfortably over
    the column width). Accept the elided spelling too.
    """
    if name in rendered:
        return True
    return any(name[:cut] + "\u2026" in rendered for cut in range(len(name) - 1, 3, -1))


def run_pipe(argv, env=None, timeout=60.0, stdin_data=b""):
    """AGENT driver: stdin/stdout are pipes, so `resolve_driver` answers Agent.

    Returns (rc, stdout, stderr, timed_out). rc is None on timeout.
    `start_new_session=True` + killpg means a harness the verb spawned cannot
    outlive the timeout holding the pipe open.
    """
    proc = subprocess.Popen(
        argv,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        out, err = proc.communicate(input=stdin_data, timeout=timeout)
        return proc.returncode, out.decode("utf-8", "replace"), err.decode("utf-8", "replace"), False
    except subprocess.TimeoutExpired:
        _kill_group(proc.pid)
        try:
            out, err = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            out, err = b"", b""
        return None, out.decode("utf-8", "replace"), err.decode("utf-8", "replace"), True


def run_tty(argv, env=None, timeout=60.0, feed=None, feed_delay=2.0, feed_every=None):
    """HUMAN driver: run argv on a real pty, so isatty(stdin) && isatty(stdout).

    This is the whole reason the human lane is not just `run_pipe`: on a pipe qd
    resolves the AGENT driver and takes a different path through every verb
    below. `pty.fork` gives the child a controlling terminal AND its own session
    (so killpg on timeout reaches whatever it spawned).

    `feed` is written to the master after `feed_delay` seconds, repeating every
    `feed_every` — how the attach check sends the detach key.

    Returns (rc, output, timed_out). rc is None on timeout.
    """
    pid, master = pty.fork()
    if pid == 0:  # child
        try:
            os.execvpe(argv[0], argv, env if env is not None else os.environ)
        except Exception:  # noqa: BLE001 — the child has nowhere to report to
            pass
        os._exit(127)

    # A TUI with no window size renders into an 0x0 terminal and can wedge.
    try:
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    except OSError:
        pass

    chunks: list[bytes] = []
    deadline = time.time() + timeout
    next_feed = (time.time() + feed_delay) if feed else None
    timed_out = False
    while True:
        now = time.time()
        if now >= deadline:
            timed_out = True
            break
        try:
            readable, _, _ = select.select([master], [], [], min(0.2, deadline - now))
        except (OSError, ValueError):
            break
        if readable:
            try:
                data = os.read(master, 65536)
            except OSError:
                data = b""  # EIO on the master IS eof when the slave closes
            if not data:
                break
            chunks.append(data)
        if next_feed is not None and time.time() >= next_feed:
            try:
                os.write(master, feed)
            except OSError:
                pass
            next_feed = (time.time() + feed_every) if feed_every else None

    if timed_out:
        _kill_group(pid)
    try:
        os.close(master)
    except OSError:
        pass
    rc = _reap(pid)
    return (None if timed_out else rc), strip_ansi(b"".join(chunks)), timed_out


def _kill_group(pid: int):
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pid, sig)
        except OSError:
            try:
                os.kill(pid, sig)
            except OSError:
                return
        time.sleep(0.4)


def _reap(pid: int):
    for _ in range(50):
        try:
            done, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return None
        if done == pid:
            if os.WIFEXITED(status):
                return os.WEXITSTATUS(status)
            if os.WIFSIGNALED(status):
                return -os.WTERMSIG(status)
            return None
        time.sleep(0.1)
    return None


# --- the smoke --------------------------------------------------------------


class Smoke:
    def __init__(self, args):
        self.args = args
        self.report = Report(args.verbose)
        self.qd = args.qd
        self.created: list[str] = []
        self.workdir = args.cwd
        # Filled by preflight — see `probe_start_detach`.
        self.detach_argv: list[str] = []

    # -- helpers -------------------------------------------------------------

    def qd_pipe(self, *argv, timeout=None, agent_marker=False):
        env = dict(os.environ)
        env.pop("QD_SESSION_ID", None)
        env.pop("CLAUDECODE", None)
        if agent_marker:
            # driver.rs rule 2: the marker beats the TTY. Exercised on request so
            # the agent lane can be proven under the signal a real in-session
            # agent carries, not only under "stdout is a pipe".
            env["QD_SESSION_ID"] = "qdsmoke0"
        return run_pipe([self.qd, *argv], env=env, timeout=timeout or self.args.timeout)

    def qd_tty(self, *argv, timeout=None, feed=None, feed_delay=2.0, feed_every=None):
        env = dict(os.environ)
        # A human is a human: an inherited agent marker would route these to the
        # agent driver and quietly turn the human lane into a second agent lane.
        env.pop("QD_SESSION_ID", None)
        env.pop("CLAUDECODE", None)
        return run_tty(
            [self.qd, *argv],
            env=env,
            timeout=timeout or self.args.timeout,
            feed=feed,
            feed_delay=feed_delay,
            feed_every=feed_every,
        )

    def session_name(self, lane: str, provider: str) -> str:
        slug = provider.replace("/", "-")
        return f"{self.args.prefix}-{lane[0]}-{slug}"

    def ls_rows(self, *extra) -> list[dict]:
        """Live JSON view of the registry — the ONE place session state is read."""
        rc, out, _err, _to = self.qd_pipe("ls", "--json", *extra)
        if rc != 0:
            return []
        try:
            rows = json.loads(out)
        except json.JSONDecodeError:
            return []
        return rows if isinstance(rows, list) else []

    def find_row(self, name: str, *extra):
        return next((r for r in self.ls_rows(*extra) if r.get("name") == name), None)

    # -- preflight -----------------------------------------------------------

    def preflight(self) -> dict | None:
        """Prove the install is coherent, then report what harnesses it has.

        Returns the parsed `qd setup --json` document, or None if the smoke
        cannot proceed at all.
        """
        print("\n== preflight ==", flush=True)

        rc, out, err, to = self.qd_pipe("--version", timeout=30)
        version = (out or err).strip().splitlines()[0] if (out or err).strip() else ""
        self.report.record(
            "preflight", "", "qd --version",
            PASS if rc == 0 and version else FAIL,
            version or ("timed out" if to else f"exit {rc}"),
            output=err,
        )

        # ADR-0020: qd resolves qw as a SIBLING of its own executable and never
        # via PATH, so this checks the file beside qd — not that `qw` is a
        # command. A qd without its qw cannot open a lane at all, which would
        # make every check below fail for one reason.
        qw = os.path.join(os.path.dirname(os.path.realpath(self.qd)), "qw")
        if os.access(qw, os.X_OK):
            rc, out, err, _to = run_pipe([qw, "build-profile"], timeout=30)
            self.report.record(
                "preflight", "", "qw beside qd",
                PASS if rc == 0 else FAIL,
                f"{qw} ({(out or err).strip().splitlines()[0] if (out or err).strip() else f'exit {rc}'})",
            )
        else:
            self.report.record(
                "preflight", "", "qw beside qd", FAIL,
                f"missing or not executable: {qw} — an installed qd cannot open a lane",
            )
            return None

        if self.args.setup_fix:
            rc, out, err, to = self.qd_pipe("setup", "--fix", timeout=180)
            self.report.record(
                "preflight", "", "qd setup --fix",
                PASS if rc == 0 else FAIL,
                "timed out" if to else f"exit {rc}",
                output=out + err,
            )

        # setup --json is REPORT-ONLY and never prompts, so it is safe on any
        # driver. Exit 1 means "something is still missing", which is a finding
        # to surface per-check below, not a reason to stop.
        rc, out, err, to = self.qd_pipe("setup", "--json", timeout=120)
        if to or rc not in (0, 1):
            self.report.record(
                "preflight", "", "qd setup --json", FAIL,
                "timed out" if to else f"exit {rc}", output=out + err,
            )
            return None
        try:
            facts = json.loads(out)
        except json.JSONDecodeError as e:
            self.report.record("preflight", "", "qd setup --json", FAIL,
                               f"unparseable: {e}", output=out + err)
            return None

        self.report.record(
            "preflight", "", "qd setup --json",
            PASS if facts.get("ok") else WARN,
            "install is fully set up" if facts.get("ok")
            else "setup reports work outstanding (see checks below)",
        )
        for check in facts.get("checks", []):
            if check.get("status") in ("ok", "info"):
                continue
            self.report.record(
                "preflight", "", f"setup check: {check.get('id')}", WARN,
                f"{check.get('status')} — {check.get('detail', '').strip()}",
            )

        # The driver auto-detect itself, on the cheapest verb that has one. If
        # this pair disagrees, every human/agent distinction below is fiction.
        rc, out, _err, to = self.qd_pipe("ls", timeout=60)
        piped_is_json = out.lstrip().startswith("[")
        self.report.record(
            "preflight", "", "driver auto-detect: piped `qd ls` is JSON",
            PASS if rc == 0 and piped_is_json else FAIL,
            "timed out" if to else ("JSON" if piped_is_json else f"exit {rc}, not a JSON array"),
            output=out[:2000],
        )
        rc, out, to = self.qd_tty("ls", timeout=60)
        tty_is_table = rc == 0 and not out.lstrip().startswith("[")
        self.report.record(
            "preflight", "", "driver auto-detect: TTY `qd ls` is the table",
            PASS if tty_is_table else FAIL,
            "timed out" if to else ("human table" if tty_is_table else f"exit {rc}, looks like JSON"),
            output=out[:2000],
        )

        # R14 shipping shape: the four session verbs plus `setup` are what a
        # human sees. A fresh install whose --help lost one of them is broken in
        # the only place a new user looks.
        rc, out, to = self.qd_tty("--help", timeout=30)
        missing = [v for v in ("ls", "start", "stop", "attach", "setup")
                   if not re.search(rf"^\s+{re.escape(v)}\b", out, re.M)]
        self.report.record(
            "preflight", "", "qd --help lists the session verbs",
            PASS if rc == 0 and not missing else FAIL,
            "timed out" if to else (f"missing: {', '.join(missing)}" if missing else "ls/start/stop/attach/setup"),
            output=out,
        )
        self.probe_start_detach()
        return facts

    def providers_from(self, facts: dict) -> list[str]:
        if self.args.providers:
            return [p.strip() for p in self.args.providers.split(",") if p.strip()]
        found = []
        for h in facts.get("harnesses", []):
            if not h.get("found"):
                self.report.record(
                    "preflight", HARNESS_TO_PROVIDER.get(h.get("id"), h.get("id")),
                    "harness detected", SKIP, "not installed on this machine",
                )
                continue
            provider = HARNESS_TO_PROVIDER.get(h.get("id"))
            if not provider:
                continue
            found.append(provider)
            self.report.record(
                "preflight", provider, "harness detected", PASS,
                f"{h.get('version') or 'version unknown'} at {h.get('path') or 'unknown path'}",
            )
        # acp/claude-code rides the claude binary rather than a harness row of
        # its own, so it is derived, never detected.
        if "claude-code" in found and not self.args.no_acp_claude:
            found.append(ACP_CLAUDE)
        return found

    def probe_start_detach(self):
        """Ask `qd start --help` how THIS build spells "do not take my terminal".

        The human lane wants a detached start, so that start and attach stay two
        separately-attributable checks. Which argv gets one has moved:

          * `--attach` — start defaults to DETACHED and attaching is the opt-IN,
            so the detached start is the bare command and there is no flag to pass;
          * `--no-attach` — start at a terminal hands the session over by default
            (FTUE punch R19 flipped it), so the opt-OUT is what we must pass.

        Reading the flag off `--help` rather than hard-coding one spelling means
        this script does not become a build-dated artifact the first time that
        default moves again — and a build that registers NEITHER is reported,
        not silently guessed at.
        """
        rc, out, err, _to = self.qd_pipe("start", "--help", timeout=30)
        text = out + err
        if "--no-attach" in text:
            self.detach_argv = ["--no-attach"]
            detail = "`--no-attach` (a TTY start attaches by default)"
        elif "--attach" in text:
            self.detach_argv = []
            detail = "no flag needed (`--attach` is the opt-in; start is detached by default)"
        else:
            self.detach_argv = []
            detail = "neither --attach nor --no-attach is registered — starting bare"
        self.report.record(
            "preflight", "", "start's detach spelling",
            PASS if rc == 0 else FAIL, detail if rc == 0 else f"qd start --help exit {rc}",
        )

    # -- human lane ----------------------------------------------------------

    def human_lane(self, provider: str):
        name = self.session_name("human", provider)
        lane = "human"
        print(f"\n== human lane: {provider} ==", flush=True)

        # Registered for cleanup BEFORE the call, not after: `qd start`'s exit
        # contract says a nonzero exit can still leave the session RUNNING (a bind
        # failure "leaves the session RUNNING and says so on stderr"), so a FAILED
        # start is exactly the case that must not leak a live session past cleanup.
        self.created.append(name)
        # A DETACHED start, however this build spells it (see probe_start_detach),
        # so that start and attach stay two separately-attributable checks — a
        # start that attached would fold any attach failure into its exit code.
        rc, out, to = self.qd_tty(
            "start", name, "--provider", provider, "--cwd", self.workdir, *self.detach_argv,
            timeout=self.args.start_timeout,
        )
        self.report.record(
            lane, provider, f"qd start (TTY{', ' + ' '.join(self.detach_argv) if self.detach_argv else ''})",
            PASS if rc == 0 else FAIL,
            "timed out" if to else f"exit {rc}",
            output=out,
        )
        if rc != 0:
            self.report.record(lane, provider, "qd ls / attach / stop", SKIP, "no session to act on")
            return

        rc, out, to = self.qd_tty("ls", timeout=60)
        listed = table_lists(out, name)
        self.report.record(
            lane, provider, "qd ls (TTY table lists the session)",
            PASS if rc == 0 and listed and not out.lstrip().startswith("[") else FAIL,
            "timed out" if to else ("listed" if listed else f"{name} absent from the table"),
            output=out,
        )

        self.human_attach(provider, name)

        rc, out, to = self.qd_tty("stop", name, timeout=self.args.timeout)
        stopped = rc == 0
        if stopped and name in self.created:
            self.created.remove(name)
        self.report.record(
            lane, provider, "qd stop (TTY)",
            PASS if stopped else FAIL,
            "timed out" if to else f"exit {rc}",
            output=out,
        )
        if stopped:
            gone = self.find_row(name, "--live") is None
            self.report.record(
                lane, provider, "session is gone from `qd ls --live`",
                PASS if gone else FAIL,
                "no live row" if gone else "still listed as live after stop",
            )

    def human_attach(self, provider: str, name: str):
        """`qd attach` — the one verb that only exists for a human.

        A working attach has TWO legitimate shapes, and which one you get is not
        a property of the provider alone:

          * it attaches, and Ctrl+\\ gets you back out (exit 0) — a mux-pane
            lane, and the only shape where "did the session survive?" is a
            question worth asking;
          * it DECLINES and says why — a daemon-hosted lane has no terminal to
            give, and a codex session that has not taken a turn yet has no
            rollout for a viewer to resume ("send it a message first, then
            attach"). Both are qd working correctly.

        So the assertion is NOT on the exit code. It is: attach TERMINATES, and
        if it declined it said something about this session. A hang is the real
        failure — a human at a terminal with no way back to their shell — and a
        silent nonzero is the other one.
        """
        rc, out, to = self.qd_tty(
            "attach", name,
            timeout=self.args.attach_timeout,
            feed=DETACH_KEY,
            feed_delay=self.args.attach_settle,
            feed_every=2.0,
        )
        if to:
            self.report.record(
                "human", provider, "qd attach", FAIL,
                f"no exit within {self.args.attach_timeout}s, even after the detach key",
                output=out,
            )
            return
        if rc == 0:
            self.report.record(
                "human", provider, "qd attach → detach (Ctrl+\\)", PASS,
                "attached; the detach key returned the shell", output=out,
            )
            # Detaching must not take the session with it — the whole contract of
            # a mux-hosted session, and only askable when we really attached.
            alive = self.find_row(name, "--live") is not None
            self.report.record(
                "human", provider, "session survives the detach",
                PASS if alive else FAIL,
                "still live" if alive else "session died with the attach client",
            )
            return
        # Declined. Correct for a daemon-hosted lane and for a not-yet-warm codex
        # session — but only if it EXPLAINED itself, naming the session it is
        # talking about. A bare nonzero exit teaches nothing, and is a failure.
        explained = name in out or bool(out.strip() and "qd attach" in out)
        first = out.strip().splitlines()[0] if out.strip() else ""
        self.report.record(
            "human", provider, "qd attach declines with guidance",
            PASS if explained else FAIL,
            f"exit {rc}: {first[:110]}" if explained else f"exit {rc}, no explanation printed",
            output=out,
        )

    # -- agent lane ----------------------------------------------------------

    def agent_lane(self, provider: str):
        name = self.session_name("agent", provider)
        lane = "agent"
        marker = self.args.agent_marker
        print(f"\n== agent lane: {provider} ==", flush=True)

        rc, out, err, to = self.qd_pipe("ls", agent_marker=marker, timeout=60)
        try:
            parsed = json.loads(out)
            is_rows = isinstance(parsed, list)
        except json.JSONDecodeError:
            is_rows = False
        self.report.record(
            lane, provider, "qd ls (auto-JSON, no --json)",
            PASS if rc == 0 and is_rows else FAIL,
            "timed out" if to else ("JSON array" if is_rows else f"exit {rc}, not a JSON array"),
            output=out[:2000] + err,
        )

        # lifecycle.rs: on the CLAUDE-CODE lane an agent-driven start is refused
        # unless it names --interactive (a bare agent start is a no-op turn; an
        # agent start with -p is a one-off print run dispatch will not spawn).
        # Every other provider's default lane never consults the driver at all,
        # and --interactive would select a DIFFERENT topology there, so it is
        # passed for exactly one provider.
        extra = ["--interactive"] if provider == "claude-code" else []
        argv = ["start", name, "--provider", provider, "--cwd", self.workdir, "--json", *extra]
        self.created.append(name)  # see the human lane: a failed start can still leave one live
        rc, out, err, to = self.qd_pipe(*argv, agent_marker=marker, timeout=self.args.start_timeout)
        # The EXIT CODE is the cross-lane contract (help::START: 0 ready, 10 created
        # but the prompt was not confirmed submitted, 1 anything else). Whether
        # stdout is JSON is a separate question, asked separately below.
        created = rc in (0, 10)
        self.report.record(
            lane, provider, "qd start (pipe, never attaches)",
            PASS if created else FAIL,
            "timed out" if to else f"exit {rc}",
            cmd=[self.qd, *argv],
            output=out + err,
        )
        if not created:
            self.report.record(lane, provider, "qd send / qd stop", SKIP, "no session to act on")
            return

        # `--json` emits `{name, qdId, sessionId, status, live}` on the CLAUDE lane,
        # which is the only lane with a bind phase after the create — the other five
        # render one human line and return (lifecycle.rs). So a missing JSON identity
        # is a REGRESSION for claude-code and a known lane gap everywhere else; the
        # split is here so the claude case can never quietly degrade into the other.
        try:
            started = json.loads(out)
        except json.JSONDecodeError:
            started = None
        has_identity = isinstance(started, dict) and bool(started.get("sessionId"))
        self.report.record(
            lane, provider, "qd start --json emits the session identity",
            PASS if has_identity else (FAIL if provider == "claude-code" else WARN),
            f"sessionId {str(started.get('sessionId'))[:12]}…" if has_identity
            else "stdout is prose, not JSON — the --json identity is the claude lane's today",
            output="" if has_identity else out,
        )

        row = self.find_row(name)
        self.report.record(
            lane, provider, "qd ls --json lists the new session",
            PASS if row else FAIL,
            f"status {row.get('status')}" if row else f"{name} absent from ls --json",
        )

        rc, out, err, to = self.qd_pipe(
            "send", name, self.args.message, agent_marker=marker, timeout=self.args.send_timeout,
        )
        # `qd send` exit contract (help::SEND): 0 delivered, 1 no receive path,
        # 12 refused{class}. Only 0 is a working install.
        self.report.record(
            lane, provider, "qd send (delivered)",
            PASS if rc == 0 else FAIL,
            "timed out" if to else f"exit {rc}" + ("" if rc == 0 else " — see stderr"),
            output=out + err,
        )

        if self.args.wait_turn and rc == 0:
            rc, out, err, to = self.qd_pipe(
                "wait", name, "--timeout", str(self.args.wait_turn),
                agent_marker=marker, timeout=self.args.wait_turn + 30,
            )
            self.report.record(
                lane, provider, "qd wait (turn completes)",
                PASS if rc == 0 else FAIL,
                "timed out" if to else f"exit {rc}",
                output=out + err,
            )

        rc, out, err, to = self.qd_pipe("stop", name, agent_marker=marker, timeout=self.args.timeout)
        stopped = rc == 0
        if stopped and name in self.created:
            self.created.remove(name)
        self.report.record(
            lane, provider, "qd stop",
            PASS if stopped else FAIL,
            "timed out" if to else f"exit {rc}",
            output=out + err,
        )
        if stopped:
            gone = self.find_row(name, "--live") is None
            self.report.record(
                lane, provider, "session is gone from `qd ls --live`",
                PASS if gone else FAIL,
                "no live row" if gone else "still listed as live after stop",
            )

    # -- teardown ------------------------------------------------------------

    def cleanup(self):
        if not self.created:
            return
        if self.args.keep:
            print(f"\n--keep: leaving {len(self.created)} session(s): {', '.join(self.created)}", flush=True)
            return
        print(f"\n== cleanup: stopping {len(self.created)} session(s) ==", flush=True)
        for name in list(self.created):
            rc, _out, _err, _to = self.qd_pipe("stop", name, timeout=60)
            # Nonzero here is usually "no such session" — a start that never got
            # far enough to make one. Best-effort by design; never a check.
            print(f"  qd stop {name} → exit {rc}", flush=True)
            self.created.remove(name)

    # -- entry ---------------------------------------------------------------

    def run(self) -> int:
        facts = self.preflight()
        if facts is None:
            self.summarize()
            return 2
        providers = self.providers_from(facts)
        if not providers:
            print("\nNo agent harnesses detected — nothing to smoke. "
                  "Install one (claude, codex, pi, opencode) and re-run.", flush=True)
            self.summarize()
            return 2
        print(f"\nproviders under test: {', '.join(providers)}", flush=True)

        for provider in providers:
            if self.args.lane in ("human", "both"):
                self.human_lane(provider)
            if self.args.lane in ("agent", "both"):
                self.agent_lane(provider)

        self.cleanup()
        self.summarize()
        return 1 if self.report.failed() else 0

    def summarize(self):
        counts = self.report.counts()
        print("\n== summary ==", flush=True)
        for row in self.report.rows:
            if row["status"] == FAIL:
                where = f"{row['lane']}/{row['provider']}" if row["provider"] else row["lane"]
                print(f"  FAIL  {where}: {row['check']} — {row['detail']}", flush=True)
        print(
            f"  {counts[PASS]} passed, {counts[FAIL]} failed, "
            f"{counts[WARN]} warnings, {counts[SKIP]} skipped",
            flush=True,
        )
        print(f"  RESULT: {'FAILURE' if counts[FAIL] else 'ALL GREEN'}", flush=True)
        if self.args.json_report:
            with open(self.args.json_report, "w") as fh:
                json.dump({"counts": counts, "checks": self.report.rows}, fh, indent=2)
            print(f"  json report: {self.args.json_report}", flush=True)


def parse_args(argv=None):
    p = argparse.ArgumentParser(
        description="Post-install functional smoke for an installed qd: human verbs "
                    "on every detected provider, then the agent verbs (ls/start/stop/send).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--qd", default=os.environ.get("QD_BIN") or shutil.which("qd"),
                   help="qd to test (default: QD_BIN, else the qd on PATH — i.e. the installed one)")
    p.add_argument("--providers",
                   help="comma-separated provider ids to test instead of auto-detecting "
                        "(claude-code, codex, pi, acp/opencode, acp/claude-code)")
    p.add_argument("--lane", choices=("human", "agent", "both"), default="both",
                   help="which driver to exercise (default: both)")
    p.add_argument("--prefix", default="qdsmoke", help="session name prefix (default: qdsmoke)")
    p.add_argument("--cwd", default="/tmp", help="working directory for created sessions (default: /tmp)")
    p.add_argument("--message", default="qd smoke test: no action needed, this is an automated probe.",
                   help="body for the `qd send` check")
    p.add_argument("--timeout", type=float, default=60.0, help="default per-command timeout (s)")
    p.add_argument("--start-timeout", type=float, default=180.0,
                   help="timeout for `qd start` — it boots a harness and awaits relay readiness (s)")
    p.add_argument("--send-timeout", type=float, default=120.0, help="timeout for `qd send` (s)")
    p.add_argument("--attach-timeout", type=float, default=45.0, help="timeout for `qd attach` (s)")
    p.add_argument("--attach-settle", type=float, default=4.0,
                   help="seconds to let an attach paint before sending the detach key")
    p.add_argument("--wait-turn", type=int, default=0, metavar="SECONDS",
                   help="after `qd send`, also `qd wait` this long for the turn to complete "
                        "(off by default — it costs a real model turn per provider)")
    p.add_argument("--setup-fix", action="store_true",
                   help="run `qd setup --fix` before testing (the fresh-install first step)")
    p.add_argument("--agent-marker", action="store_true",
                   help="also export QD_SESSION_ID for the agent lane, the signal a real "
                        "in-session agent carries (driver.rs: the marker beats the TTY)")
    p.add_argument("--no-acp-claude", action="store_true",
                   help="skip the derived acp/claude-code provider")
    p.add_argument("--keep", action="store_true", help="do not stop sessions left behind by failures")
    p.add_argument("--json-report", metavar="PATH", help="also write the checks as JSON")
    p.add_argument("--verbose", "-v", action="store_true", help="print command output for passing checks too")
    return p.parse_args(argv)


def main(argv=None) -> int:
    args = parse_args(argv)
    if not args.qd:
        print("fresh-install-smoke: no `qd` found — pass --qd /path/to/qd or put the "
              "installed qd on PATH.", file=sys.stderr)
        return 2
    if not os.access(args.qd, os.X_OK):
        print(f"fresh-install-smoke: not executable: {args.qd}", file=sys.stderr)
        return 2
    os.makedirs(args.cwd, exist_ok=True)

    print(f"fresh-install-smoke: qd={args.qd} lane={args.lane} prefix={args.prefix}", flush=True)
    smoke = Smoke(args)
    try:
        return smoke.run()
    except KeyboardInterrupt:
        # Interrupting a smoke run must not leave live sessions behind.
        print("\ninterrupted — cleaning up", flush=True)
        smoke.cleanup()
        return 130


if __name__ == "__main__":
    sys.exit(main())
