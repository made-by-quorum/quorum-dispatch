#!/usr/bin/env python3
"""harness-mesh.py — the mesh test, driven ONLY through qd's process interface.

This asks the question — can an agent sitting inside a harness drive `qd` 
itself? — to start and message other sessions.

  1. THE ONLY INTERFACE IS THE `qd` (and `qw`) PROCESS. Sessions are created,
     addressed, observed and torn down by running the binary and reading its
     stdout/stderr and exit code. Nothing in this file opens a path under
     `~/.quorum`, a provider transcript, or any other file qd happens to write.
     If a fact is not reachable through a verb, this script will not use it.
  2. MINIMAL PROMPT ENGINEERING. The hub prompt names the binary, names the sessions
     it should create, says to send each of them a message and what to ask for
     back, and stops. It does not render argv for the agent, does not explain
     lanes or carriers, does not pre-chew exit codes. What the agent does with
     `qd` is the measurement; teaching it the answer would mean business logic lives
     within this script rather than qd's self hosted documentation.

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

WHAT ONE HUB'S TURN IS — bring the hub up, have IT start its spokes, and have
it exchange a message with each of them:

    qd setup --json      which harnesses this machine actually has
    qd start <hub> -p …  one hub session per harness, prompted with spoke names
    qd ls --prefix …     watch the spokes appear, as qd sees them
    qd messages <every>  monitor the hub's log AND every spoke's, on a poll
    qd stop <session>    the sweep

The exchange is the point of the second half: starting a session proves qd can
CREATE, and says nothing about whether the thing it created can be ADDRESSED.
So the hub is asked to `qd send` each spoke a message asking for one back, and
the pass condition is that `qd messages` shows both legs — the hub's ask going
out and the spoke's answer coming in. A session that starts and then cannot be
reached is a failure here, where under the old step it was a pass.

THE MESSAGES ARE THE LOG. Every message the monitor finds prints TWICE, once
under the session that sent it and once under the session it was addressed to:

    pi-hub           >> pi-claude        delivered  Please use the qd command-…
    pi-claude        << pi-hub           delivered  Please use the qd command-…

so the `who` column stays what it is on every other line — the session the line
is about — and a message reads as two lines that meet in the middle. Every
session's log is swept, not just the hub's, because `qd messages` reports a
message to a session on EITHER end: a message between two spokes is on neither
of the hub's ends, and a monitor that only asked the hub would never see it.
Reading both ends is also what lets the run tell an envelope that never moved
from one that moved without its attribution — an end whose own log cannot show
its own message is reported as its own finding, not as a silence.

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

# The marks, and the SGR code each is painted in when the terminal can take it.
# `··` a step this script is about to take, `OK` a thing that happened as asked,
# `!!` a thing that did not, and the blank mark for the detail lines hanging off
# whichever of those came last — those are dimmed whole, because they are
# subordinate to the row above rather than events in their own right.
STEP, OK, BAD, NOTE = "··", "OK", "!!", "  "

# The message marks: `>>` a message leaving the session named on the line, `<<`
# that same message arriving at the one it was addressed to. Every message
# prints both, so it reads as two lines that meet in the middle and the traffic
# can be followed down the `who` column — rather than being reconstructed from
# one end's report of it. One colour for the pair, because the glyph already
# carries the direction and what the colour is for is telling message traffic
# apart from the steps and results around it.
SENT, RECV = ">>", "<<"
MARK_SGR = {STEP: "36", OK: "32", BAD: "31",    # cyan, green, red
            SENT: "35", RECV: "35"}             # magenta
DIM, RESET = "\033[2m", "\033[0m"

# How wide the `who` column is: wide enough for the longest label a hub's turn
# can produce (`{hub}-{spoke}`, e.g. `opencode-claude`). A label that overflows
# it pushes its own mark rightward and breaks the column alignment, which is the
# thing that makes four hubs reporting at once readable at all.
WHO_W = 16

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
    line = f"{who:<{WHO_W}} {mark} {msg}"
    if _COLOR:
        sgr = MARK_SGR.get(mark)
        line = (f"{who:<{WHO_W}} \033[{sgr}m{mark}\033[0m {msg}" if sgr
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


# --- reading a `qd messages` row --------------------------------------------
#
# `qd messages <session> --json` emits one envelope per line carrying both of
# its ends (`sender`, `target`), the `direction` that selected it, the folded
# delivery `state`, and the `body`. These three functions are the whole of what
# this script knows how to do with such a row; nothing else here parses one.


def session_addresses(row: dict) -> set[str]:
    """Every spelling of one session that qd itself published, lowercased.

    Read out of the session's own `qd ls` row rather than composed here, so the
    set is whatever qd says it is. `qdIdPrefix` is deliberately LEFT OUT: it is
    two characters, and matching an envelope end on two characters would let a
    collision credit the wrong session — the same reason qd's own read verb
    runs no id-prefix tier over `sender`.
    """
    return {str(row[key]).lower() for key in ("name", "qdId", "sessionId")
            if row.get(key)}


def target_of(row: dict) -> str:
    """The envelope's target AS THE SENDER TYPED IT, minus any `@host`.

    R9.4 keeps the raw string, so what comes back is the hub's own spelling and
    not a resolved id. Only the host qualifier is dropped: it addresses a
    machine rather than a session, and every session in this run is local.
    """
    return str(row.get("target") or "").lower().split("@", 1)[0]


def sender_of(row: dict) -> str:
    """The envelope's sender id, lowercased, or "" when qd recorded none.

    An id qd stamped itself rather than an address anyone typed, so it is
    compared exactly and with no prefix tier: two characters of a `sender` would
    let a collision credit another session's authorship, the one error an
    attribution column must not make.
    """
    return str(row.get("sender") or "").lower()


def end_member(value: str, members: dict) -> str | None:
    """Which session of this hub's cast an envelope end names — None if none.

    An outsider's address resolves to nothing here, and is left to print as qd
    recorded it. Guessing a member for an end that does not match one would put
    a name on traffic this run did not author.
    """
    if not value:
        return None
    for name, member in members.items():
        if value in member["addrs"]:
            return name
    return None


def ordered_messages(seen: dict) -> list[dict]:
    """The message union in message order: `authored_at`, then the id.

    The order qd's own report uses, for the reason it uses it: `authored_at` is
    the origin timeline, and the correlation id is a ULID, so two messages
    written in the same millisecond still land in mint order and not an
    arbitrary one.
    """
    return sorted(seen.values(),
                  key=lambda r: (r.get("authored_at") or 0,
                                 str(r.get("correlation_id") or "")))


def body_line(row: dict, width: int = 72) -> str:
    """One message body on one line, for a progress note."""
    body = " ".join(str(row.get("body") or "").split())
    return body if len(body) <= width else body[:width - 1] + "…"


# --- the prompt -------------------------------------------------------------
#
# The whole prompt. It names the binary, names the sessions, and asks for two
# things to happen: for the sessions to exist, and for a message to make the
# round trip. It does NOT render the `qd start` or `qd send` argv, name a lane,
# a provider flag or a carrier, or explain what an exit code means: how to make
# and address a session with `qd` is precisely the thing under test, and an
# agent that has to read `qd start --help` to find out is doing exactly what a
# person would.
#
# The one thing the second ask does dictate is the TEXT of the reply — each
# spoke is to send back its own name. That is the observable, not a hint: the
# `sender` column attributes a reply only when the replying session carried a
# `QD_SESSION_ID`, and a body that names its author stays attributable when it
# did not. Nothing about how to send is given away by asking what to say.
#
# The trailing newline is absent because the string ends at the last line, not
# because it was stripped: `qd start -p` has a known off-by-one that reports a
# payload ending in "\n" as truncated by one byte. This script does not add one
# and does not remove one.

HUB_PROMPT = """\
You are the hub of a test of the `qd` command-line tool. The build of qd under
test is the binary at this absolute path, and it is the only one you should use:

    {qd}

Your own session is named:

    {hub}

You have two jobs, in this order.

First: start one qd session per harness in this list, using qd.

{spokes}

Give each session the working directory {cwd}.

Second: send a message with qd to each session you started, asking it to send
a message back to your own session, using qd, whose text is its own session
name — the name it is listed under above.

Do not stop any session, do not write any file, and do not summarize anything —
the harness reads the result from qd itself. When every send has returned,
print DONE."""


def render_prompt(qd_path: str, hub: str, cwd: str, spokes: list[dict]) -> str:
    width = max(len(s["name"]) for s in spokes)
    lines = [f"    {s['name']:<{width}}   — harness: {s['harness']}" for s in spokes]
    return HUB_PROMPT.format(qd=qd_path, hub=hub, cwd=cwd, spokes="\n".join(lines))


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
        # The hub's own `qd ls` row, kept from the liveness check: the monitor
        # matches envelope ends against it exactly as it does a spoke's.
        self.hub_row: dict | None = None
        # The two legs of each exchange, keyed by spoke name and holding the
        # `qd messages` row that closed them: the hub's ask on its way out, and
        # the spoke's answer on its way back.
        self.asked: dict[str, dict] = {}
        self.answered: dict[str, dict] = {}
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
        self.hub_row = row
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

    # -- the message monitor -------------------------------------------------

    def cast(self) -> dict:
        """Every session in this hub's turn, with the label its lines carry.

        The `who` column is `{hub}-{role}`: `pi-hub` for the hub itself, and
        `pi-claude` for the claude spoke of pi's turn. The hub prefix is not
        decoration — four hubs report into one stream at once, and `claude`
        alone would name a different session in each of them.

        `addrs` is the address set an envelope end is matched against, taken
        from the session's own `qd ls` row and nowhere else.
        """
        rows = {self.hub_name: (self.hub_row or {"name": self.hub_name}, "hub")}
        for spoke in self.spokes:
            if spoke["name"] in self.started:
                rows[spoke["name"]] = (self.started[spoke["name"]], spoke["harness"])
        return {name: {"label": f"{self.hub_harness}-{role}",
                       "addrs": session_addresses(row)}
                for name, (row, role) in rows.items()}

    def read_logs(self, members: dict) -> dict:
        """`qd messages` for every session in the cast, in one sweep.

        The hub's log is not the run's traffic. A message is reported to a
        session on either END of it, so anything two spokes say to each other
        is on neither of the hub's ends and would go unseen by a monitor that
        only ever asked the hub. Reading every member's log leaves the monitor
        blind to nothing qd wrote.

        A verb that fails is a failed tick, not an empty log: `None` is kept
        distinct from `[]` so a read error is never folded into "no messages".
        """
        logs = {}
        for name in members:
            rows, why = self.qd.jsonl("messages", name, "--json")
            if rows is None:
                self.log.note(f"qd messages {name} failed this tick: {why}")
            logs[name] = rows
        return logs

    def emit_new(self, logs: dict, members: dict, seen: dict, printed: set):
        """Print the messages a sweep turned up that have not been printed yet.

        TWO LINES PER MESSAGE: one under its sender, one under its recipient.
        Which is which is the mark, so the `who` column stays what it is
        everywhere else in this script — the session the line is about.

        The pair is keyed by correlation id AND state, so a message first seen
        `pending` and later `delivered` prints again. Those are two facts about
        it, and collapsing them would leave the log asserting a delivery state
        that has since changed.

        A message read from two logs is still one message: the union is keyed
        by correlation id, and `direction` is deliberately not consulted for
        it, because `direction` is relative to whichever session was asked for
        the log. The ends are not — they mean the same thing in every log they
        appear in, which is what makes one union possible at all.
        """
        for rows in logs.values():
            for row in rows or []:
                cid = str(row.get("correlation_id") or "")
                if cid:
                    seen[cid] = row
        for row in ordered_messages(seen):
            key = (row.get("correlation_id"), row.get("state"))
            if key in printed:
                continue
            printed.add(key)
            src = end_member(sender_of(row), members)
            dst = end_member(target_of(row), members)
            # An end outside the cast prints as qd recorded it and is never
            # resolved into a member it might not be: the raw target for an
            # address this run does not know, and `(no sender)` for the null
            # the store writes when the sending caller carried no session id.
            src_label = (members[src]["label"] if src
                         else (row.get("sender") or "(no sender)"))
            dst_label = (members[dst]["label"] if dst
                         else (row.get("target") or "(no target)"))
            state = str(row.get("state") or "?")
            body = body_line(row, 48)
            emit(src_label, SENT, f"{dst_label:<{WHO_W}} {state:<9} {body}")
            emit(dst_label, RECV, f"{src_label:<{WHO_W}} {state:<9} {body}")

    def one_sided(self, logs: dict) -> bool:
        """True — and said out loud — when this build cannot report a sent side.

        `direction` arrived with the `sender` field the whole monitor rests on.
        A build without it reports only the addressed side, so half of every
        exchange is missing from the store and no amount of polling will turn
        it up. Better said in the first seconds than discovered one empty sweep
        at a time across the whole budget.
        """
        for rows in logs.values():
            if rows:
                if "direction" in rows[0]:
                    return False
                self.log.bad("this qd build's `messages` rows carry no `direction`: it "
                             "predates two-sided reporting, so the sent half of every "
                             "exchange is unreadable — rebuild qd")
                self.failures.append("qd messages is one-sided")
                return True
        return False

    def roundtrip(self):
        """Watch the run's messages go by, and say whether each spoke answered.

        The monitor sweeps every session's log on a poll, prints what is new,
        and closes each spoke's two legs as they appear. It is polled rather
        than read once because the hub is still working while this runs — a hub
        that has started its spokes has not necessarily written to them yet.
        Nothing said before the monitor starts is lost: each sweep reads the
        whole log, so an early message is reported late rather than not at all.

        Note what is NOT accepted as an answer: a delivered ask. `qd send`
        exiting 0 says an envelope reached a receive path, which is the claim
        the transport can make and not the one the mesh is about. Only a second
        message, authored by the spoke, shows that something on the far end
        read the first one and could work the tool.
        """
        want = {s["name"]: s["harness"] for s in self.spokes if s["name"] in self.started}
        if not want:
            self.log.bad("no spoke ever started — there is nothing to exchange with")
            self.failures.append("no spoke to message")
            return
        never = [s["harness"] for s in self.spokes if s["name"] not in self.started]
        if never:
            self.log.note(f"exchanging with {len(want)} of {len(self.spokes)} spoke(s) — "
                          f"never started: {', '.join(never)}")

        # One more `qd ls` before the monitor starts. The row the watch cached
        # is from the instant a spoke FIRST appeared, and a session's ids can
        # still be filling in at that moment — and those ids are exactly what
        # every envelope end below is matched on.
        current, why = self.qd.json("ls", "--prefix", self.tag, "--all", "--json")
        if current is None:
            self.log.note(f"qd ls before the monitor failed ({why}); matching on the "
                          "ids the watch saw")
        for row in current or []:
            name = str(row.get("name") or "")
            if name in self.started:
                self.started[name] = row

        members = self.cast()
        seen: dict[str, dict] = {}       # correlation id -> the message, latest read
        printed: set = set()             # (correlation id, state) already on the log
        deadline = time.time() + self.args.message_budget
        self.log.step(f"monitoring `qd messages` across {len(members)} session(s) until "
                      f"{len(want)} round trip(s) close, or "
                      f"{self.args.message_budget:.0f}s pass")
        while True:
            logs = self.read_logs(members)
            if self.one_sided(logs):
                return
            self.emit_new(logs, members, seen, printed)
            self.collect(seen, members, want)
            if len(self.asked) == len(want) and len(self.answered) == len(want):
                break
            if time.time() >= deadline:
                break
            time.sleep(self.args.poll)

        self.report_gaps(seen, members, want)
        self.cross_check(members, seen, printed)

    def collect(self, seen: dict, members: dict, want: dict):
        """Close each spoke's two legs from the messages read so far.

        The legs are matched on the ENDS, never on `direction`: the ask is the
        message the hub sent to the spoke, the answer is the one the spoke sent
        back to the hub. `direction` is relative to whichever session was asked
        for the log, and the monitor reads several of them; the ends mean the
        same thing whoever reported them.

        The answer has one fallback the ask does not: a message addressed to
        the hub that carries no `sender` but whose body names the spoke. The
        prompt asked each spoke to send back its own name precisely so that an
        unattributed reply is still readable, and which of the two closed the
        leg is printed, because "matched on the body" means qd recorded no
        sender for it.

        A message already claimed by one leg is never offered to another: a
        message belongs to one exchange, and a substring collision between two
        session names must not be able to close a leg twice.
        """
        rows = ordered_messages(seen)
        hub_addrs = members[self.hub_name]["addrs"]
        claimed = {r.get("correlation_id")
                   for r in (*self.asked.values(), *self.answered.values())}
        for name, harness in want.items():
            addrs = members[name]["addrs"]
            fresh = [r for r in rows if r.get("correlation_id") not in claimed]
            if name not in self.asked:
                row = next((r for r in fresh if sender_of(r) in hub_addrs
                            and target_of(r) in addrs), None)
                if row:
                    self.asked[name] = row
                    claimed.add(row.get("correlation_id"))
                    self.log.ok(f"the ask to {harness} is in the log: {row.get('state')}")
                    fresh = [r for r in fresh if r is not row]
            if name not in self.answered:
                by_sender = next((r for r in fresh if sender_of(r) in addrs
                                  and target_of(r) in hub_addrs), None)
                by_body = next((r for r in fresh if not sender_of(r)
                                and target_of(r) in hub_addrs
                                and name.lower() in str(r.get("body") or "").lower()), None)
                row = by_sender or by_body
                if row:
                    self.answered[name] = row
                    claimed.add(row.get("correlation_id"))
                    how = "sender" if by_sender else "body only, no sender recorded"
                    self.log.ok(f"{harness} answered: {row.get('state')} ({how})")

    def report_gaps(self, seen: dict, members: dict, want: dict):
        """What never closed and — where the log can say — why not.

        A leg can fail two ways that look identical in a count and are not:
        nothing was sent, or something was sent that no log can attribute. The
        second is a message sitting in the store with a null `sender`, and
        saying so points at the attribution rather than at the session.
        """
        rows = ordered_messages(seen)
        for name, harness in want.items():
            addrs = members[name]["addrs"]
            if name not in self.asked:
                orphan = next((r for r in rows if target_of(r) in addrs
                               and not sender_of(r)), None)
                if orphan:
                    self.log.bad(f"a message to {harness} ({name}) is in the log but "
                                 "carries no `sender`, so nothing attributes it to the "
                                 "hub — the ask cannot be confirmed")
                    self.failures.append(f"ask to {harness} unattributed")
                else:
                    self.log.bad(f"the hub never asked {harness} ({name}): no message "
                                 "from the hub is addressed to it")
                    self.failures.append(f"no send to {harness}")
            if name not in self.answered:
                self.log.bad(f"nothing came back from {harness} ({name}): no message "
                             "from it is addressed to the hub")
                self.failures.append(f"no reply from {harness}")

    def cross_check(self, members: dict, seen: dict, printed: set):
        """One last sweep, then every message checked against its own two ends.

        `qd messages` reports a message to a session on EITHER end, so a
        message between two cast members has to be in both of their logs. Where
        it is not, the envelope moved but its attribution did not — a different
        result from a message that never moved, and worth naming rather than
        folding into a silence.

        The sweep is re-read rather than reused, because the loop's last read
        of one member happened at a slightly different instant from its read of
        another, and a message written between the two would look absent from
        an end that in fact has it. Whatever the sweep turns up that the
        monitor never printed is printed now, so the log ends complete.
        """
        logs = self.read_logs(members)
        self.emit_new(logs, members, seen, printed)
        where: dict[str, set] = {}
        for name, rows in logs.items():
            for row in rows or []:
                where.setdefault(str(row.get("correlation_id") or ""), set()).add(name)
        checked = found = 0
        for row in ordered_messages(seen):
            cid = str(row.get("correlation_id") or "")
            short = cid[:8] or "?"
            ends = where.get(cid, set())
            src = end_member(sender_of(row), members)
            dst = end_member(target_of(row), members)
            checked += 1
            if not sender_of(row):
                # Unattributed at the source. The body may still name its
                # author — that is what the prompt asked each spoke to send —
                # so the finding can name a session even when the store cannot.
                author = next((members[n]["label"] for n in members
                               if n.lower() in str(row.get("body") or "").lower()), None)
                whose = f"{author}'s message" if author else f"the message {short}"
                self.log.bad(f"{whose} to {members[dst]['label'] if dst else '?'} carries "
                             "no `sender`: it is on nobody's sent side, and no log can "
                             "say which session wrote it")
                self.failures.append(f"{author or short} sends unattributed")
                found += 1
                continue
            if src and src not in ends:
                self.log.bad(f"{members[src]['label']} sent {short}, but its own log "
                             "does not report it")
                self.failures.append(f"{members[src]['label']} cannot see its send")
                found += 1
            if dst and dst not in ends:
                self.log.bad(f"{short} is addressed to {members[dst]['label']}, but that "
                             "session's log does not report it")
                self.failures.append(f"{members[dst]['label']} cannot see the message")
                found += 1
        # Said even when nothing is wrong. A check that only ever speaks up to
        # complain leaves a clean run unable to show it ran at all, and "no
        # findings" and "never looked" have to be different lines.
        if not found:
            self.log.ok(f"both ends agree on every message: {checked} checked")

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
        prompt = render_prompt(self.qd.path, self.hub_name, self.cwd, self.spokes)
        argv = [self.qd.path, *self.hub_argv("<prompt>")]
        return ("\n--- hub: " + self.hub_harness + " " + "-" * (54 - len(self.hub_harness))
                + "\n" + " ".join(shlex.quote(a) for a in argv)
                + "\n\n" + prompt + "\n")

    def run(self):
        """One hub's whole turn. Never raises — a hub that dies is a result."""
        began = time.time()
        try:
            os.makedirs(self.cwd, exist_ok=True)
            self.start_hub(render_prompt(self.qd.path, self.hub_name, self.cwd, self.spokes))
            if self.hub_is_live():
                self.watch_for_spokes()
                self.roundtrip()
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
    p.add_argument("--message-budget", type=float, default=300.0,
                   help="how long to watch for each spoke's ask and answer (s). Its own "
                        "budget, not a share of --spoke-budget: the exchange only starts "
                        "once the spokes exist, so the two are spent one after the other")
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
        # Round trips are counted against the spokes ASKED FOR, not the ones
        # that came up: a hub that started two of three and exchanged with both
        # is 2/3 here, because the third spoke's silence is the failure and
        # scoring it out of two would hide it.
        row = (f"{m.hub_harness + '-hub':<12} "
               f"{len(m.started)}/{len(m.spokes)} spoke(s), "
               f"{len(m.answered)}/{len(m.spokes)} round trip(s)")
        took = f"in {m.elapsed:.1f}s"
        if m.failures:
            worst = max(worst, 1)
            log.bad(f"{row} {took} — " + "; ".join(m.failures))
        else:
            log.ok(f"{row} {took}")
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
