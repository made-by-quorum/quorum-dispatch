#!/usr/bin/env python3
"""harness-mesh.py — the mesh test, driven ONLY through qd's process interface.

This is a second take on `harness-mesh-live.py`. It asks the same question —
can an agent sitting inside a harness drive `qd` itself? — under two rules the
older script does not keep:

  1. THE ONLY INTERFACE IS THE `qd` (and `qw`) PROCESS. Sessions are created,
     addressed, observed and torn down by running the binary and reading its
     stdout/stderr and exit code. Nothing in this file opens a path under
     `~/.quorum`, a provider transcript, or any other file qd happens to write.
     If a fact is not reachable through a verb, this script does not know it.
  2. NO PROMPT ENGINEERING. The hub prompt names the binary, names the sessions
     it should create, and stops. It does not render argv for the agent, does
     not explain lanes, does not pre-chew exit codes. What the agent does with
     `qd` is the measurement; teaching it the answer would erase the result.

Both rules cost coverage, and that is accepted: a step that cannot be seen
through a verb is reported as unobservable, not worked around, and an agent
that cannot drive `qd` from a plain instruction FAILS rather than being coached
until it succeeds. Those outcomes are findings about qd, which is the point.

EVERY HARNESS TAKES A TURN AS THE HUB, AND THE TURNS RUN AT THE SAME TIME.
One harness driving the mesh well says nothing about the other three, so the
suite is run once per harness, each with the other harnesses as its spokes, and
the runs go in parallel because they are independent: separate sessions,
separate name prefixes, separate working directories, and no shared state
except the qd registry itself — which every session is entitled to use, and
which the mesh is meant to exercise concurrently anyway. `--hub` narrows the
set when you only want one or two.

Because several hubs report at once, every progress line is stamped with the
hub it belongs to, and the per-hub result lands in one summary block at the end.

STEP 1 (this revision) — bring up each hub and have IT start its spokes:

    qd setup --json      which harnesses this machine actually has
    qd start <hub> -p …  one hub session per harness, prompted with spoke names
    qd ls --prefix …     watch the spokes appear, as qd sees them
    qd messages <hub>    what was addressed to the hub, per qd
    qd stop <session>    the sweep

Usage:
    dispatch/scripts/harness-mesh.py --dry-run      # print every plan, start nothing
    dispatch/scripts/harness-mesh.py                # every harness as hub, in parallel
    dispatch/scripts/harness-mesh.py --hub codex,pi # only those two as hubs
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import signal
import subprocess
import sys
import threading
import time

# --- what qd calls things ---------------------------------------------------

# `qd setup --json` .harnesses[].id -> the name `qd start --provider` accepts.
# The one mismatched row is claude: setup probes for the BINARY (`claude`),
# `--provider` names the PROGRAM (`claude-code`).
PROVIDER_OF = {
    "claude": "claude-code",
    "codex": "codex",
    "pi": "pi",
    "opencode": "opencode",
}

# The unattended posture. Not prompt engineering — it is environment, and
# without it the run cannot happen at all: since R22 qd is safe-by-default on
# both harnesses that have a bypass, so a claude session parks at its first
# approval prompt and a codex session is sandboxed out of the directory holding
# the `qd` it is being asked to run. Neither stall reports itself as a
# permissions problem; both look like "nothing ever happened".
UNATTENDED_ENV = {
    "QD_CLAUDE_FLAGS": "--dangerously-skip-permissions "
                       "--dangerously-load-development-channels server:relay",
    "QD_CODEX_DANGER_FULL_ACCESS": "1",
}

REPO_QD = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "target", "release", "qd")


# --- progress ---------------------------------------------------------------
#
# Several hubs write here at once, so every line carries the hub it came from
# and goes out under one lock. Nothing is buffered per-hub and replayed at the
# end: a parallel run that only prints when it finishes is a run you cannot
# watch, and watching is most of what this script is for.

_T0 = time.time()          # script start, for the wall-clock total in `summarize`
_PRINT_LOCK = threading.Lock()

# The four marks, and the SGR code each is painted in when the terminal can
# take it. `··` a step this script is about to take, `OK` a thing that happened
# as asked, `!!` a thing that did not, and the blank mark for the detail lines
# hanging off whichever of those came last — those are dimmed whole, because
# they are subordinate to the row above rather than events in their own right.
STEP, OK, BAD, NOTE = "··", "OK", "!!", "  "
MARK_SGR = {STEP: "36", OK: "32", BAD: "31"}    # cyan, green, red
DIM, RESET = "\033[2m", "\033[0m"

# Whether to paint at all. Set once by `resolve_color` before any hub starts, so
# no lock is needed around the read.
_COLOR = False


def resolve_color(choice: str) -> bool:
    """`--color auto|always|never`, with the conventions a terminal user expects.

    `auto` paints only a real terminal, and stands down for NO_COLOR (any value,
    per no-color.org) and for TERM=dumb. Piping this script into a file or a
    grep is the common case — the run is long enough that people tee it — and
    escape codes in that file help nobody.
    """
    if choice == "always":
        return True
    if choice == "never":
        return False
    if os.environ.get("NO_COLOR") is not None:
        return False
    if os.environ.get("TERM", "") == "dumb":
        return False
    return sys.stdout.isatty()


def emit(who: str, mark: str, msg: str):
    line = f"{who:<12} {mark} {msg}"
    if _COLOR:
        sgr = MARK_SGR.get(mark)
        line = (f"{who:<12} \033[{sgr}m{mark}\033[0m {msg}" if sgr
                else f"{DIM}{line}{RESET}")
    with _PRINT_LOCK:
        print(line, flush=True)


class Log:
    """The progress channel for one hub (or for the driver itself)."""

    def __init__(self, who: str):
        self.who = who

    def step(self, msg):
        emit(self.who, STEP, msg)

    def ok(self, msg):
        emit(self.who, OK, msg)

    def bad(self, msg):
        emit(self.who, BAD, msg)

    def note(self, msg):
        emit(self.who, NOTE, msg)


# --- the process interface --------------------------------------------------


def run(argv, timeout=120.0):
    """Run one command with pipes on both ends.

    Pipes are deliberate: `resolve_driver` answers Agent for a piped caller, so
    every verb below is exercised on the same surface a real agent gets, and
    the `--json` auto-default applies without being asked for.
    """
    env = dict(os.environ)
    # This process is its own caller, not a session's tool call. An inherited
    # session marker would make qd treat these calls as coming from inside a
    # session (and refuse a send to that session as a self-send).
    env.pop("QD_SESSION_ID", None)
    env.pop("CLAUDECODE", None)
    env["QD_CODEX_UNPINNED"] = "1"   # silence the version-pin nag
    env.update(UNATTENDED_ENV)
    proc = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            start_new_session=True)
    try:
        out, err = proc.communicate(b"", timeout=timeout)
        rc = proc.returncode
    except subprocess.TimeoutExpired:
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(proc.pid, sig)
            except OSError:
                pass
            time.sleep(0.3)
        out, err = b"", b""
        rc = None
    return rc, out.decode("utf-8", "replace"), err.decode("utf-8", "replace")


class Qd:
    """Every call this script makes into quorum goes through here.

    One instance is shared by every hub thread. It holds no mutable state —
    each call is its own process — so concurrent use is safe by construction.
    """

    def __init__(self, path: str, timeout: float):
        self.path = path
        self.timeout = timeout

    def __call__(self, *argv, timeout=None):
        return run([self.path, *argv], timeout=timeout or self.timeout)

    def json(self, *argv, timeout=None):
        """A verb's `--json`, parsed. `None` when the verb failed or did not
        emit JSON — never a guess, and never a fallback to reading a file."""
        rc, out, err = self(*argv, timeout=timeout)
        if rc != 0:
            return None, (err or out).strip()
        try:
            return json.loads(out), ""
        except json.JSONDecodeError as exc:
            return None, f"not JSON: {exc}"

    def jsonl(self, *argv, timeout=None):
        rc, out, err = self(*argv, timeout=timeout)
        if rc != 0:
            return None, (err or out).strip()
        rows = []
        for line in out.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue
        return rows, ""


# --- the prompt -------------------------------------------------------------
#
# The whole prompt. It names the binary, names the sessions, and asks for them
# to exist. It does NOT render the `qd start` argv, name a lane or a provider
# flag, or explain what an exit code means: how to make a session on a given
# harness with `qd` is precisely the thing under test, and an agent that has to
# read `qd start --help` to find out is doing exactly what a person would.
#
# The trailing newline is absent because the string ends at the last line, not
# because it was stripped: `qd start -p` has a known off-by-one that reports a
# payload ending in "\n" as truncated by one byte. This script does not add one
# and does not remove one.

HUB_PROMPT = """\
You are the hub of a test of the `qd` command-line tool. The build of qd under
test is the binary at this absolute path, and it is the only one you should use:

    {qd}

Your job is to start one qd session per harness in this list, using qd:

{spokes}

Give each session the working directory {cwd}.

Do not stop any session, do not write any file, and do not summarize anything —
the harness reads the result from qd itself. When every start has returned,
print DONE."""


def render_prompt(qd_path: str, cwd: str, spokes: list[dict]) -> str:
    width = max(len(s["name"]) for s in spokes)
    lines = [f"    {s['name']:<{width}}   — harness: {s['harness']}" for s in spokes]
    return HUB_PROMPT.format(qd=qd_path, cwd=cwd, spokes="\n".join(lines))


# --- discovery, once for the whole run --------------------------------------


def discover(qd: Qd, log: Log) -> list[str]:
    """Which harnesses exist, according to qd and nothing else."""
    log.step("qd --version")
    rc, out, err = qd("--version", timeout=30)
    if rc != 0:
        log.bad(f"qd --version exited {rc}: {(err or out).strip()}")
        return []
    log.ok(f"qd {(out or err).strip()} at {qd.path}")

    log.step("qd setup --json  (which harnesses does this machine have?)")
    data, why = qd.json("setup", "--json", timeout=90)
    if data is None:
        log.bad(f"qd setup --json gave nothing usable: {why}")
        return []

    found = []
    for row in data.get("harnesses", []):
        hid = str(row.get("id"))
        present = bool(row.get("found"))
        log.note(f"{hid:<9} {'present' if present else 'absent':<8} "
                 f"wired={str(row.get('wired')):<5} {(row.get('version') or '').strip()}")
        if present and hid in PROVIDER_OF:
            found.append(hid)
    log.ok(f"harnesses qd reports here: {', '.join(found) or '(none)'}")
    return found


# --- one hub's run ----------------------------------------------------------


class Mesh:
    """One harness acting as hub, with every other harness as a spoke.

    Everything a Mesh touches is namespaced by its own tag, so N of these run
    concurrently without arranging anything between them: the session names,
    the `qd ls --prefix` watch key and the working directory all carry the hub
    harness id.
    """

    def __init__(self, qd: Qd, args, hub: str, harnesses: list[str]):
        self.qd = qd
        self.args = args
        self.hub_harness = hub
        self.log = Log(f"{hub}-hub")
        self.tag = f"{args.prefix}-{args.runid}-{hub}"
        self.hub_name = f"{self.tag}-hub"
        self.cwd = os.path.join(args.cwd, hub)
        self.spokes = [{"harness": h, "name": f"{self.tag}-spoke-{h}"}
                       for h in harnesses if h != hub]
        self.created: list[str] = []
        self.failures: list[str] = []
        self.started: dict[str, dict] = {}
        # Wall clock for THIS hub's turn, teardown excluded — the sweep is
        # bookkeeping, not part of what is being measured, and including it
        # would make a hub look slower for having created more sessions.
        self.elapsed = 0.0

    # -- the hub -------------------------------------------------------------

    def hub_argv(self, prompt: str) -> list[str]:
        provider = PROVIDER_OF[self.hub_harness]
        argv = ["start", self.hub_name, "--provider", provider]
        if provider == "claude-code":
            # The one lane whose start consults the driver: an agent-driven
            # (piped) start on claude's pane lane is REFUSED without this. Not
            # added anywhere else — on codex and pi it would select a different
            # topology.
            argv.append("--interactive")
        argv += ["--cwd", self.cwd, "-p", prompt]
        return argv

    def start_hub(self, prompt: str):
        argv = self.hub_argv(prompt)
        self.log.step("qd " + " ".join(shlex.quote(a) for a in argv[:-2]) + " -p <prompt>")
        # Registered before the call: a start that fails can still have left a
        # session running and only said so on stderr.
        self.created.append(self.hub_name)
        rc, out, err = self.qd(*argv, timeout=self.args.start_timeout)
        for line in (err or out).strip().splitlines()[-6:]:
            self.log.note(line)
        if rc == 0:
            self.log.ok("hub started, exit 0")
            return
        self.log.bad(f"hub start exited {rc} — see the lines above")
        self.failures.append(f"hub start exit {rc}")
        # Not fatal on its own: qd's own contract says a nonzero start may still
        # have created the session, and `qd ls` is the arbiter of that.

    def hub_is_live(self) -> bool:
        rows, why = self.qd.json("ls", "--prefix", self.tag, "--json")
        if rows is None:
            self.log.bad(f"qd ls --prefix {self.tag} --json: {why}")
            self.failures.append("qd ls failed")
            return False
        row = next((r for r in rows if r.get("name") == self.hub_name), None)
        if not row:
            self.log.bad(f"the hub is not in `qd ls --prefix {self.tag}` at all")
            self.failures.append("hub absent from qd ls")
            return False
        self.log.ok(f"qd ls sees the hub: status={row.get('status')} lane={row.get('lane')}")
        return True

    # -- watching the hub work ----------------------------------------------

    def watch_for_spokes(self):
        """Poll `qd ls` until every spoke exists or the budget runs out.

        `qd ls --prefix` is the only discovery key that works on every lane:
        `qd start --json` prints an identity object on the claude pane lane
        alone, and `spawnedBy` is stamped by that one create path too, so the
        NAME is what a cross-lane watcher has. The prefix carries the hub id,
        so a hub only ever sees the spokes it was asked for — never another
        hub's, even though all of them are being created at the same moment.
        """
        want = {s["name"]: s["harness"] for s in self.spokes}
        deadline = time.time() + self.args.spoke_budget
        self.log.step(f"watching `qd ls --prefix {self.tag}` for {len(want)} spoke(s), "
                      f"up to {self.args.spoke_budget:.0f}s")
        while time.time() < deadline:
            rows, why = self.qd.json("ls", "--prefix", self.tag, "--all", "--json")
            if rows is None:
                self.log.note(f"qd ls failed this tick: {why}")
            else:
                for r in rows:
                    name = str(r.get("name") or "")
                    if name in want and name not in self.started:
                        self.started[name] = r
                        self.created.append(name)
                        self.log.ok(f"spoke up: {want[name]} as {name} "
                                    f"lane={r.get('lane')} status={r.get('status')}")
            if len(self.started) == len(want):
                break
            time.sleep(self.args.poll)
        for name, harness in want.items():
            if name not in self.started:
                self.log.bad(f"spoke never appeared: {harness} ({name})")
                self.failures.append(f"no spoke on {harness}")

    def hub_messages(self):
        """What qd says was addressed to the hub.

        The per-session read verb, used here for what it can actually answer:
        every envelope sent TO the hub through `qd send`. It cannot show what
        the hub SENT (the log records the origin HOST, with no sender-session
        field) and it cannot show relay replies. Those are unobservable through
        the process interface, and this script says so rather than reaching
        past qd for them.
        """
        self.log.step(f"qd messages {self.hub_name} --json")
        rows, why = self.qd.jsonl("messages", self.hub_name, "--json")
        if rows is None:
            self.log.bad(f"qd messages: {why}")
            return
        self.log.ok(f"{len(rows)} message(s) addressed to the hub")
        for r in rows:
            body = str(r.get("body") or "").replace("\n", " ")
            self.log.note(f"{r.get('state') or r.get('disposition') or '?'}: {body[:100]}")

    # -- teardown ------------------------------------------------------------

    def sweep(self):
        if self.args.keep:
            self.log.note(f"--keep: leaving {len(self.created)} session(s); "
                          f"list them with `qd ls --prefix {self.tag}`")
            return
        self.log.step(f"sweeping {len(self.created)} session(s) with `qd stop`")
        for name in reversed(self.created):
            rc, out, err = self.qd("stop", name, timeout=60)
            first = ((err or out).strip().splitlines() or [""])[0]
            self.log.note(f"qd stop {name}: exit {rc}" + (f" — {first}" if rc else ""))

    # -- drive ---------------------------------------------------------------

    def plan_text(self) -> str:
        prompt = render_prompt(self.qd.path, self.cwd, self.spokes)
        argv = [self.qd.path, *self.hub_argv("<prompt>")]
        return ("\n--- hub: " + self.hub_harness + " " + "-" * (54 - len(self.hub_harness))
                + "\n" + " ".join(shlex.quote(a) for a in argv)
                + "\n\n" + prompt + "\n")

    def run(self):
        """One hub's whole turn. Never raises — a hub that dies is a result."""
        began = time.time()
        try:
            os.makedirs(self.cwd, exist_ok=True)
            self.start_hub(render_prompt(self.qd.path, self.cwd, self.spokes))
            if self.hub_is_live():
                self.watch_for_spokes()
                self.hub_messages()
        except Exception as exc:                      # noqa: BLE001 — see docstring
            self.log.bad(f"hub run raised {type(exc).__name__}: {exc}")
            self.failures.append(f"{type(exc).__name__}: {exc}")
        finally:
            self.elapsed = time.time() - began
            try:
                self.sweep()
            except Exception as exc:                  # noqa: BLE001
                self.log.bad(f"sweep raised {type(exc).__name__}: {exc}")


# --- cli --------------------------------------------------------------------


def parse_args(argv=None):
    p = argparse.ArgumentParser(
        description="Mesh test driven only through the qd/qw process interfaces. "
                    "Every harness takes a turn as hub, in parallel.")
    p.add_argument("--qd", default=REPO_QD if os.path.exists(REPO_QD) else "qd",
                   help="the qd binary under test (default: this repo's target/release/qd)")
    p.add_argument("--hub", action="append", default=[], metavar="HARNESS[,HARNESS…]",
                   help="limit the run to these hubs (repeatable, or comma-separated). "
                        "Default: every harness qd reports")
    p.add_argument("--prefix", default="qmesh", help="session-name prefix (default: qmesh)")
    p.add_argument("--cwd", help="parent working directory; each hub gets a subdirectory "
                                 "(default: a fresh scratch dir — sessions run unattended)")
    p.add_argument("--dry-run", action="store_true",
                   help="print every hub's plan and prompt, start nothing")
    p.add_argument("--keep", action="store_true", help="do not stop the sessions on the way out")
    p.add_argument("--serial", action="store_true",
                   help="run the hubs one after another instead of all at once")
    p.add_argument("--timeout", type=float, default=120.0, help="per-qd-call timeout (s)")
    p.add_argument("--start-timeout", type=float, default=300.0, help="timeout for `qd start` (s)")
    p.add_argument("--spoke-budget", type=float, default=300.0,
                   help="how long to watch for a hub's spokes (s)")
    p.add_argument("--poll", type=float, default=5.0, help="seconds between `qd ls` polls")
    p.add_argument("--color", choices=("auto", "always", "never"), default="auto",
                   help="colour the OK/!!/·· marks (default: auto — a terminal only, "
                        "and never under NO_COLOR or TERM=dumb)")
    args = p.parse_args(argv)
    global _COLOR
    _COLOR = resolve_color(args.color)
    args.runid = time.strftime("%m%d%H%M%S")
    args.hubs = [h for item in args.hub for h in item.split(",") if h.strip()]
    if not args.cwd:
        args.cwd = os.path.join(
            os.environ.get("TMPDIR", "/tmp"), f"{args.prefix}-{args.runid}")
    args.cwd = os.path.abspath(args.cwd)
    return args


def select_hubs(requested: list[str], harnesses: list[str], log: Log) -> list[str]:
    if not requested:
        return list(harnesses)
    hubs, unknown = [], []
    for h in requested:
        (hubs if h in harnesses else unknown).append(h)
    for h in unknown:
        log.bad(f"--hub {h}: qd did not report that harness here "
                f"({', '.join(harnesses)}) — skipping it")
    return hubs


def summarize(meshes: list[Mesh], log: Log) -> int:
    """One row per hub, named the way its own progress lines are named.

    Both numbers are WALL CLOCK, never a sum: a hub's time is its own turn end
    to end, and the script's time is the whole run. Under the default parallel
    mode the hub times OVERLAP, so they add up to more than the total — adding
    them would describe a serial run that did not happen.
    """
    print()
    log.step("results, one row per hub")
    worst = 0
    for m in meshes:
        row = f"{m.hub_harness + '-hub':<12} {len(m.started)}/{len(m.spokes)} spoke(s)"
        took = f"in {m.elapsed:.1f}s"
        if m.failures:
            worst = max(worst, 1)
            log.bad(f"{row} {took} — " + "; ".join(m.failures))
        else:
            log.ok(f"{row} started {took}")
    log.step(f"total script run {time.time() - _T0:.1f}s (wall clock)")
    return worst


def main(argv=None) -> int:
    args = parse_args(argv)
    qd = Qd(args.qd, args.timeout)
    log = Log("script")

    harnesses = discover(qd, log)
    if not harnesses:
        return 2
    hubs = select_hubs(args.hubs, harnesses, log)
    if not hubs:
        log.bad("no hub to run")
        return 2
    if len(harnesses) < 2:
        log.bad(f"only one harness ({harnesses[0]}) on this machine — nothing to mesh with")
        return 2

    meshes = [Mesh(qd, args, hub, harnesses) for hub in hubs]
    log.ok(f"{len(meshes)} hub(s): " + ", ".join(
        f"{m.hub_harness} -> {len(m.spokes)} spoke(s)" for m in meshes))

    if args.dry_run:
        for m in meshes:
            print(m.plan_text())
        return 0

    try:
        if args.serial:
            for m in meshes:
                m.run()
        else:
            threads = [threading.Thread(target=m.run, name=m.hub_harness, daemon=False)
                       for m in meshes]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
    except KeyboardInterrupt:
        # The hub threads are not daemons, so they are still running their own
        # `finally: sweep()`. Nothing to do here but say so and let them land.
        print()
        log.bad("interrupted — waiting for each hub to sweep its own sessions")
        for t in threading.enumerate():
            if t is not threading.current_thread() and not t.daemon:
                t.join()
        return 130

    return summarize(meshes, log)


if __name__ == "__main__":
    sys.exit(main())
