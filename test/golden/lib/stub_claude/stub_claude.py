#!/usr/bin/env python3
"""stub_claude.py — a DETERMINISTIC spec-faithful counterpart for the sb golden oracle.

This process plays Claude Code's role for the contract-bearing corpus rows. sb is
the system under test (the oracle measures the sb ENGINE), so this stub implements
EXACTLY the surfaces the pinned-TS sb reads / elicits — and NOTHING that sb does
not observe. Every behaviour is DERIVED FROM PINNED TS SOURCE and CITED below.

PIN: 0d0fa9ed4800efb1309eca2311345c48af2c4932  (zmx 0.6.0)
STUB_VERSION lives in stub_version.txt (read at startup; joins the match-proof so a
stub edit invalidates the proofs of the rows it backs — rider R1).

R2 NEGATIVE-CONTROL SEAMS (v1.5.1): STUB_WITHHOLD_PID / STUB_PID_DELAY_MS /
STUB_WITHHOLD_JSONL / STUB_DEAD_HEALTH inject the rider-R2 misbehaviours
(delayed/withheld PID file, missing JSONL reply, dead relay /health). They are
DORMANT by default (active ONLY when their env var is set), so the stub's
default observable behaviour is BYTE-IDENTICAL to v1.5.0. The recorded rows'
proofs stamp the stub sha AS RECORDED (71a8f622, the v1.5.0 sha) and stay
valid — no recorded fixture carries the stub version, and the seams never fire
during recording. Rows are NOT re-recorded for this dormant-seam edit; the
R1 "edit ⇒ re-record" rule targets behaviour-changing edits to recorded
surfaces, and these add-only seams change none of them.

0b DELTA-STRENGTH SEAMS (v1.8.0): four W2 additions, ALL dormant by the same
discipline as the R2 seams (env-gated or prompt-gated; the default observable
behaviour is BYTE-IDENTICAL to v1.7.0, replay-verified on the two most
stub-sensitive rows). They are SET DELIBERATELY by W3/W4 strengthened rows
(seam set INLINE in a scenario's scn_run, exactly the STUB_BUSY_HOLD_MS
precedent) and by the mutation-real negative controls; they NEVER fire on the
existing recorded corpus.
- STUB_NO_QUEUE (W2.1): while status=busy, actively read-and-DISCARD stdin
  rather than letting the TTY buffer queue it. With it on, a message reaches the
  JSONL IFF the ENGINE held it until idle (decideSendPty send-queue,
  utils.ts:297-299). Dormant default => the busy-hold sleeps and the TTY buffers
  the queued send (drained next iteration), unchanged.
- STUB_TWO_STAGE_PID_WRITE (+STUB_TWO_STAGE_GAP_MS, default 1500) (W2.2): every
  PID-file write() lands DIRECT on the final path in two stages — partial-JSON
  prefix + flush, gap, complete rewrite — bypassing the atomic tmp+rename. It
  exercises the ENGINE's tolerance of a mid-write partial PID file (the A1 PR #20
  per-field-permissive read; readPidStatus / getPidEntries,
  lifecycle.ts:167-175, session.ts:327-355). Dormant default => atomic
  tmp+rename, unchanged.
- STTY prompt reporter (W2.3, prompt-gated): a submitted line `STTY` prints a
  DETERMINISTIC termios report (python stdlib termios; flag bitmasks +
  raw-mode-defining booleans only — no speeds/cc/ids/timing) for the P10
  raw-mode row. Platform-split expected. Collision-checked: no existing scenario
  submits an `STTY` line. Dormant default => `STTY` is not submitted by any
  recorded row.
- STUB_COUNT_PRE_PID_STDIN (W2.3): count stdin chars read between popup render
  and PID-file write, EXCLUDING the single dismiss CR, exposed via the sidecar
  ~/.claude/stub-boot-stats.json. The value is 0 BY STUB CONSTRUCTION (one CR
  then the PID write; independent of TS's blind-Enter interval,
  lifecycle.ts:177-235). Dormant default => no sidecar written, dismiss read
  unchanged.

0b DELTA-STRENGTH SEAM (v1.8.1): ONE W3.1 addition, dormant by the same
discipline.
- STUB_RAW_STDIN (W3.1): at startup, flip the PTY stdin termios to RAW (clear
  ICANON/ECHO via tty.setcbreak) so the line discipline no longer caps a single
  submitted line at the cooked-mode canonical-line bound (macOS MAX_CANON=1024).
  Real Claude's TUI runs the PTY in raw mode; the pin's chunked-IDLE delivery
  (deliverIdleTwoWrite/sendTextChunked, submit.ts:287-334/222-234 @ 8c59ec4)
  sends a >=4KB message as ONE canonical line with NO inter-chunk CR
  (submit.ts:298-301), so a COOKED stub drops everything past byte 1024. With the
  seam SET INLINE in the send-pty-chunked-idle scenario's scn_run, the stub
  receives the full chunked >=4KB write byte-loss-free, proving the engine's
  chunked-delivery contract. Dormant default => the PTY stays COOKED; the default
  observable behaviour (incl. the W3.7/P10 termios row, which asserts the cooked
  icanon=1/echo=1 default) is BYTE-IDENTICAL to v1.8.0. _read_one_submit is
  UNCHANGED (already char-by-char read(1) until CR/LF).

================================ BEHAVIOUR / CITATION TABLE (rider R1) =============
Every row below names a stub behaviour and the pinned-TS file:line it is derived
from. Paths are relative to the pinned clone src/.

| # | Stub behaviour                              | sb surface it serves            | Pinned-TS citation (file:line @ pin) |
|---|---------------------------------------------|---------------------------------|--------------------------------------|
| 1 | Launched as the `claude` process by         | startDetached `zmx run <name>   | commands/lifecycle.ts:780-807 (start |
|   | `command 'claude' <flags> '--name' <name>`; |  -d bash -lc '<claudeCmd>'`;    | Detached), utils.ts:507-513 (build   |
|   | parses `--name` from argv.                  | buildClaudeCmd; --name arg.     | ClaudeCmd), utils.ts:258-271 (extra) |
| 2 | Renders a dev-channels "popup" line to the  | waitForSessionReady Phase 1+2:  | commands/lifecycle.ts:177-235 (the   |
|   | PTY, then BLOCKS reading stdin until it      | the popup appears BEFORE the    | blind-Enter loop; CONFIRMED unfixed  |
|   | receives a CR (the blind-Enter dismiss).    | PID file; Enter dismisses it.   | at pin), :211 ("harmless" note).     |
| 3 | AFTER the dismiss CR, writes the PID file    | findPidFile matches by          | commands/lifecycle.ts:135-161        |
|   | ~/.claude/sessions/<pid>.json with          | data.name===sessionName;        | (findPidFile), session.ts:64-75      |
|   | {pid,sessionId,cwd,startedAt,updatedAt,      | getPidEntries reads the file;   | (PidEntry shape), session.ts:327-355 |
|   | status:"idle",name,version,kind,entrypoint}.| status field is load-bearing.   | (getPidEntries).                     |
| 4 | status transitions idle->busy->idle on a     | readPidStatus reads data.status;| commands/lifecycle.ts:167-175 (read  |
|   | submitted line; the file's status field is   | submit/--wait key on busy/idle. | PidStatus), submit.ts:87-114 (verify |
|   | the single signal.                          |                                 | AcceptedThenCR), utils.ts:359-365.   |
| 5 | Appends a JSONL user record then an assistant| send:pty --wait JSONL anchor:   | commands/send.ts:224-346 (--wait     |
|   | record to ~/.claude/projects/<proj>/         | findUserAnchor on a `user`      | loop + extraction), utils.ts:307-346 |
|   | <sessionId>.jsonl. user={type:"user",        | record; assistant text block    | (JsonlRecord, userRecordText, find   |
|   | message:{content:<text>}}; assistant=        | with stop_reason==end_turn.     | UserAnchor), session.ts:437-461,     |
|   | {type:"assistant",message:{content:[{type:   |                                 | 433-435 (findJsonlPath, cwdToProject |
|   | "text",text:<reply>}],stop_reason:"end_turn"}}.|                               | Path).                               |
| 6 | Queue-to-busy: a line submitted WHILE busy is| decideSendPty busy->send-queue; | utils.ts:297-299 (decideSendPty),    |
|   | QUEUED and drained (its own user+assistant   | --wait anchors on the queued    | commands/send.ts:154-222 (send-queue |
|   | JSONL pair) when the current turn ends.      | message's user record.          | two-write), send.ts:224-294 (anchor).|
| 7 | server:relay flag -> spawn a CHILD relay      | getRelayPorts reads sidecar     | session.ts:148-183 (RELAY_DIR,       |
|   | server binding $SB_RELAY_PORT; write sidecar | {sessionId,port,pid,status};    | getRelayPorts), session.ts:185-212   |
|   | ~/.claude/relay/<sessionId>.json {sessionId, | ls-join maps relay child PID    | (/health RelayHealth), session.ts:   |
|   | port,pid,status:"ok"}; the sidecar `pid` is   | up to the claude PID; send:relay| 845-873,922 (ls relay join by PID    |
|   | the relay CHILD pid (PID-parentage join).    | POSTs /message -> {message_id}. | parentage), commands/send.ts:414-426.|
| 8 | Relay server answers GET /health ->          | scanRelayPorts/--wait reply;    | session.ts:185-212 (/health),        |
|   | {sessionId,port,pid,status:"ok"}, POST        | the relay CONTRACT surface per  | commands/send.ts:414-475 (/message + |
|   | /message -> {message_id:"<deterministic>"},   | ADD-5 (engine = contract only). | /replies client), ADD-5 (contract in |
|   | GET /replies/<id> -> {text:"<reply>"}.        |                                 | engine, driver external).            |
| 9 | SEAM STUB_NO_QUEUE (W2.1, dormant): while     | decideSendPty busy->send-queue; | utils.ts:297-299 (decideSendPty),    |
|   | busy, read-and-DISCARD stdin so a message     | only the ENGINE's hold-until-   | commands/send.ts:154-222 (send-queue |
|   | lands in JSONL iff the ENGINE queued it.       | idle queue can deliver it.      | two-write), send.ts:224-294 (anchor).|
|10 | SEAM STUB_TWO_STAGE_PID_WRITE (W2.2, dormant):| readPidStatus / getPidEntries   | lifecycle.ts:167-175 (readPidStatus),|
|   | PID write lands DIRECT in two stages (partial | tolerate a mid-write partial    | session.ts:327-355 (getPidEntries),  |
|   | + gap + complete), atomic rename bypassed.    | file (A1 PR#20 permissive read).| session.ts:64-75 (PidEntry shape).   |
|11 | SEAM STTY prompt (W2.3, prompt-gated): an     | the raw-mode/termios config the | (engine-behaviour row; the report is |
|   | `STTY` line prints a deterministic termios    | engine establishes on the       | the cheap portion per ADR-0010 §(a); |
|   | report (flags only) for the P10 raw-mode row. | session PTY.                    | full repaint realism stays A4/C2).   |
|12 | SEAM STUB_COUNT_PRE_PID_STDIN (W2.3, dormant):| the blind-Enter dismiss-then-   | lifecycle.ts:177-235 (blind-Enter    |
|   | count stdin chars before the PID write (excl. | PID-write ordering; counter is  | loop @ pin), :211 ("harmless" note); |
|   | dismiss CR) -> stub-boot-stats.json sidecar.  | 0 by stub construction (P2).    | session.ts:135-161 (findPidFile).    |

NON-GOALS (sb never observes these on the recorded rows, so the stub omits them):
opencode/http surfaces, real model inference, tool use, thinking blocks, multi-turn
beyond what a row drives, ANSI repaint fidelity (boot-readiness keys on the EVENT,
not bytes — ADR 0004 / 0005-dialog-free-boot).

DETERMINISM (rider: double-record must match byte-for-byte after normalize):
- sessionId is a FIXED constant (STUB_SESSION_ID), never time/random derived.
- All emitted timestamps are FIXED strings the normalizer collapses to <TS>.
- All PID-bearing values are normalized to <PID>; the relay port to <RELAY_PORT>.
- PTY output is a fixed ordered string; no wall-clock text.
- message_id is derived deterministically from the message text (sha1, stable).

Bash 3.2 / python3>=3.6 floors apply (stdlib only).
"""
import os
import sys
import json
import time
import errno
import select
import hashlib
import http.server
import socketserver

try:
    import termios  # POSIX-only; used by _emit_stty_report + the STUB_RAW_STDIN seam.
    import tty
except ImportError:        # non-POSIX (never our recording host) — seam is a no-op.
    termios = None
    tty = None

# --- deterministic constants -------------------------------------------------
STUB_SESSION_ID = "stub0000-0000-0000-0000-000000000000"
FIXED_TS = "2026-01-01T00:00:00.000Z"          # normalizer -> <TS> (JSONL only)
# FIDELITY (A4 pass-(b) F3, orc-3 sanction-extension ruling 2026-06-05): the PID
# registry file's startedAt/updatedAt are epoch-ms NUMBERS — TS PidEntry declares
# `number` and TS writes Date.now() (session.ts:68, :743). The previous ISO-string
# value was readable by TS's dynamic JS read but is NOT what TS/claude actually
# write; the Rust typed registry read (i64, faithful to the declared shape)
# rejects the whole row, hiding the session from ls/resolve. JSONL record
# timestamps stay ISO strings (FIXED_TS) — that IS the real transcript shape.
# The value is an ARBITRARY deterministic constant (2026-01-01T00:00:00Z in ms);
# double-record byte-identity depends on it never being clock-derived.
FIXED_TS_MS = 1767225600000
STUB_VERSION = "unknown"

_HERE = os.path.dirname(os.path.abspath(__file__))
try:
    with open(os.path.join(_HERE, "stub_version.txt")) as _vf:
        STUB_VERSION = _vf.read().strip() or "unknown"
except OSError:
    pass


def _home():
    return os.environ.get("HOME") or os.path.expanduser("~")


def _sessions_dir():
    return os.path.join(_home(), ".claude", "sessions")


def _relay_dir():
    return os.path.join(_home(), ".claude", "relay")


def _projects_dir():
    return os.path.join(_home(), ".claude", "projects")


def _cwd_to_project_path(cwd):
    # cwdToProjectPath (session.ts:433-435): replace '/' with '-'.
    return cwd.replace("/", "-")


def _parse_name(argv):
    # buildNewExtraArgs appends ["--name", name] (utils.ts:258-271).
    for i, a in enumerate(argv):
        if a == "--name" and i + 1 < len(argv):
            return argv[i + 1]
    return "stub-session"


def _wants_relay(argv):
    # CLAUDE_FLAGS carries "server:relay" (utils.ts:227).
    return "server:relay" in argv


def _message_id(text):
    return "mid-" + hashlib.sha1(text.encode("utf-8")).hexdigest()[:12]


def _reply_for(text):
    # Deterministic canned reply (no inference). Stable per input text.
    return "STUB-REPLY to: " + text.strip()


def _maybe_emit_backlog(text):
    """A "EMIT <N>" prompt prints N numbered SBLINE rows to the PTY — a
    deterministic backlog generator for the zmx-VT retention rows (L6/L7). No
    wall-clock content; fixed strings in fixed order."""
    t = text.strip()
    if not t.startswith("EMIT "):
        return
    try:
        n = int(t.split()[1])
    except (IndexError, ValueError):
        return
    for i in range(1, n + 1):
        sys.stdout.write("SBLINE %d\r\n" % i)
    sys.stdout.flush()


def _emit_stty_report():
    """SEAM (W2.3, prompt-gated): a submitted line `STTY` makes the stub print a
    DETERMINISTIC termios report for stdin to the PTY (the P10 raw-mode row reads
    the raw-mode/termios config the ENGINE established on the session PTY). FLAGS
    ONLY — no speeds, no control-char values, no ids/timing — so the report is
    deterministic given a mode (cooked vs raw); a platform split is expected and
    handled by the P10 fixture (macOS/Linux). Collision-checked: no existing
    scenario submits a line starting `STTY` (the prompt namespace already holds
    EMIT). On a platform without termios, emit a stable UNAVAILABLE marker."""
    try:
        import termios
    except Exception:
        sys.stdout.write("STTY-REPORT termios=unavailable\r\n")
        sys.stdout.flush()
        return
    try:
        attrs = termios.tcgetattr(sys.stdin.fileno())
    except Exception:
        sys.stdout.write("STTY-REPORT termios=error\r\n")
        sys.stdout.flush()
        return
    iflag, oflag, cflag, lflag = attrs[0], attrs[1], attrs[2], attrs[3]
    # Report only the four mode bitmasks (hex), flag-derived booleans for the
    # raw-mode-defining bits, and nothing clock/id-derived. tcgetattr returns the
    # CURRENT termios — under cooked mode ICANON/ECHO are set; under raw they are
    # cleared. The fixed format + fixed field order keeps double-record stable.
    sys.stdout.write(
        "STTY-REPORT iflag=0x%x oflag=0x%x cflag=0x%x lflag=0x%x "
        "icanon=%d echo=%d isig=%d\r\n"
        % (
            iflag, oflag, cflag, lflag,
            1 if (lflag & termios.ICANON) else 0,
            1 if (lflag & termios.ECHO) else 0,
            1 if (lflag & termios.ISIG) else 0,
        )
    )
    sys.stdout.flush()


# --- PID registry file (citation #3/#4) --------------------------------------
class PidFile(object):
    def __init__(self, name, cwd):
        self.name = name
        self.cwd = cwd
        self.path = os.path.join(_sessions_dir(), "%d.json" % os.getpid())
        self.status = "idle"

    def write(self):
        os.makedirs(_sessions_dir(), exist_ok=True)
        data = {
            "pid": os.getpid(),
            "sessionId": STUB_SESSION_ID,
            "cwd": self.cwd,
            "startedAt": FIXED_TS_MS,
            "updatedAt": FIXED_TS_MS,
            "status": self.status,
            "name": self.name,
            "version": "stub-" + STUB_VERSION,
            "kind": "claude-code",
            "entrypoint": "stub_claude",
        }
        # SEAM (W2.2): STUB_TWO_STAGE_PID_WRITE makes EVERY write() (the boot
        # write AND status transitions) land DIRECT on the final path in TWO
        # stages — a syntactically-partial JSON prefix + flush, a gap, then the
        # complete rewrite — BYPASSING the atomic tmp+rename below. It exercises
        # the ENGINE's tolerance of a mid-write partial PID file (the P11 consumer
        # asserts the deterministic OUTCOME — boot readiness still fires, ls does
        # not crash — never the partial state). DORMANT by default: unset => the
        # atomic tmp+rename path runs UNCHANGED (default output byte-identical).
        if os.environ.get("STUB_TWO_STAGE_PID_WRITE") == "1":
            self._write_two_stage(data)
            return
        tmp = self.path + ".tmp"
        with open(tmp, "w") as f:
            json.dump(data, f)
        os.rename(tmp, self.path)

    def _write_two_stage(self, data):
        # Direct-to-final two-stage write (SEAM W2.2). Stage 1: a deliberately
        # INCOMPLETE JSON prefix (truncated mid-object, no closing brace) + flush,
        # so a reader racing the write can observe a partial file. Gap
        # (STUB_TWO_STAGE_GAP_MS, default 1500). Stage 2: the complete JSON
        # rewritten over the SAME path. No tmp+rename — the atomic guard is
        # intentionally bypassed to open the partial-state window.
        gap_ms = 1500
        try:
            gap_ms = int(os.environ.get("STUB_TWO_STAGE_GAP_MS", "1500"))
        except ValueError:
            gap_ms = 1500
        full = json.dumps(data)
        # Half the bytes => guaranteed not valid JSON (no closing brace).
        prefix = full[: max(1, len(full) // 2)]
        with open(self.path, "w") as f:
            f.write(prefix)
            f.flush()
            os.fsync(f.fileno())
        if gap_ms > 0:
            time.sleep(gap_ms / 1000.0)
        with open(self.path, "w") as f:
            f.write(full)
            f.flush()
            os.fsync(f.fileno())

    def set_status(self, status):
        self.status = status
        self.write()


# --- JSONL conversation file (citation #5) -----------------------------------
class Jsonl(object):
    def __init__(self, cwd):
        proj = _cwd_to_project_path(cwd)
        self.dir = os.path.join(_projects_dir(), proj)
        self.path = os.path.join(self.dir, STUB_SESSION_ID + ".jsonl")

    def append_user(self, text):
        self._append({"type": "user", "message": {"content": text},
                      "timestamp": FIXED_TS})

    def append_assistant(self, reply):
        self._append({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": reply}],
                        "stop_reason": "end_turn"},
            "timestamp": FIXED_TS,
        })

    def ensure_exists(self):
        os.makedirs(self.dir, exist_ok=True)
        # touch — findJsonlPath (session.ts:437-461) stat()s this path; it must
        # exist before the first turn or send:pty --wait refuses (send.ts:128).
        if not os.path.exists(self.path):
            open(self.path, "a").close()

    def _append(self, obj):
        os.makedirs(self.dir, exist_ok=True)
        with open(self.path, "a") as f:
            f.write(json.dumps(obj) + "\n")


# --- relay server (citations #7/#8) ------------------------------------------
def _run_relay_child(port, sidecar_path):
    """Run in a forked CHILD so the sidecar pid is the relay child's pid (the
    PID-parentage join: session.ts:845-873). Binds $SB_RELAY_PORT, writes the
    sidecar, serves /health + /message + /replies until killed."""
    replies = {}

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass  # silence — deterministic output only

        def _json(self, code, obj):
            body = json.dumps(obj).encode("utf-8")
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/health":
                # R2 SEAM: dead relay /health (rider R2). When STUB_DEAD_HEALTH=1
                # the endpoint answers 503 with status "dead" instead of the
                # RelayHealth ok shape, so scanRelayPorts/ls-join sees an unhealthy
                # relay. Dormant by default.
                if os.environ.get("STUB_DEAD_HEALTH") == "1":
                    self._json(503, {"sessionId": STUB_SESSION_ID, "port": port,
                                     "pid": os.getpid(), "status": "dead"})
                    return
                # RelayHealth shape (session.ts:152-157, 200-202).
                self._json(200, {"sessionId": STUB_SESSION_ID, "port": port,
                                 "pid": os.getpid(), "status": "ok"})
            elif self.path.startswith("/replies/"):
                mid = self.path[len("/replies/"):]
                self._json(200, {"text": replies.get(mid, _reply_for(mid))})
            else:
                self._json(404, {"error": "not found"})

        def do_POST(self):
            if self.path == "/message":
                length = int(self.headers.get("Content-Length", "0") or "0")
                raw = self.rfile.read(length) if length else b"{}"
                try:
                    payload = json.loads(raw.decode("utf-8"))
                except Exception:
                    payload = {}
                text = payload.get("text", "")
                mid = _message_id(text)
                replies[mid] = _reply_for(text)
                # /message -> {message_id} (send.ts:414-426).
                self._json(200, {"message_id": mid})
            else:
                self._json(404, {"error": "not found"})

    os.makedirs(_relay_dir(), exist_ok=True)
    # Sidecar shape (session.ts:159-183): {sessionId,port,pid,status}.
    sidecar = {"sessionId": STUB_SESSION_ID, "port": port,
               "pid": os.getpid(), "status": "ok"}
    tmp = sidecar_path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(sidecar, f)
    os.rename(tmp, sidecar_path)

    socketserver.TCPServer.allow_reuse_address = True
    httpd = socketserver.TCPServer(("127.0.0.1", port), Handler)
    httpd.serve_forever()


def _maybe_start_relay(argv):
    if not _wants_relay(argv):
        return None
    port = os.environ.get("SB_RELAY_PORT")
    if not port:
        return None
    try:
        port = int(port)
    except ValueError:
        return None
    sidecar_path = os.path.join(_relay_dir(), STUB_SESSION_ID + ".json")
    os.makedirs(_relay_dir(), exist_ok=True)
    pid = os.fork()
    if pid == 0:
        # CHILD: become the relay server.
        try:
            _run_relay_child(port, sidecar_path)
        finally:
            os._exit(0)
    return pid  # parent: the relay child's pid (for cleanup).


# --- R2 negative-control helper ----------------------------------------------
def _stub_hold_open():
    """Keep the process alive (consuming stdin) without writing the PID file, so a
    withhold-PID negative control makes the readiness poll time out instead of the
    process exiting (which would look like a crash, not a withheld event)."""
    while True:
        line = _read_one_submit()
        if line is None:
            return


# --- SEAM (W3.1, v1.8.1): STUB_RAW_STDIN -------------------------------------
def _maybe_set_raw_stdin():
    """SEAM (W3.1, scenario-INLINE recording mode): when STUB_RAW_STDIN=1, flip the
    session PTY's stdin termios to RAW at startup (clear ICANON/ECHO/ISIG) so the
    line discipline no longer caps a single submitted line at the cooked-mode
    canonical-line bound (macOS MAX_CANON=1024). This makes the stub read like REAL
    Claude, whose TUI puts the PTY in raw mode — necessary to receive the pin's
    chunked-IDLE delivery of a >=4KB message (deliverIdleTwoWrite/sendTextChunked
    send the whole text as ONE canonical line with NO inter-chunk CR, submit.ts:
    298-301 @ 8c59ec4, so a cooked stub drops everything past byte 1024).

    DORMANCY: gated on STUB_RAW_STDIN=1; UNSET => no-op, the PTY stays COOKED and the
    default observable behaviour (incl. the W3.7/P10 termios row, which asserts the
    cooked icanon=1/echo=1 default) is BYTE-IDENTICAL to 1.8.0. _read_one_submit is
    UNCHANGED — it is already char-by-char (read(1) until CR/LF), so it reassembles
    the raw byte stream into the same submitted line a cooked reader would for a
    <1024B message; raw mode only removes the canonical-buffer cap. The line is still
    terminated by the trailing \\r the engine sends separately."""
    if os.environ.get("STUB_RAW_STDIN") != "1":
        return
    if termios is None or not sys.stdin.isatty():
        return
    try:
        # cbreak-like: clear ICANON/ECHO (no canonical line buffering, no echo) but
        # keep the rest minimal. tty.setcbreak clears ICANON+ECHO and leaves signal
        # handling intact; we additionally do not touch output processing so the
        # stub's \r\n writes render as before.
        tty.setcbreak(sys.stdin.fileno(), termios.TCSANOW)
    except (termios.error, OSError, ValueError):
        return


# --- main REPL loop ----------------------------------------------------------
def main():
    argv = sys.argv[1:]
    _maybe_set_raw_stdin()
    name = _parse_name(argv)
    cwd = os.getcwd()

    # Record the EXACT launch flags (buildClaudeCmd output the shell expanded into
    # our argv) so the build_claude_cmd row can verify flag ORDER + presence — the
    # load-bearing contract (utils.ts:507-513, 226-227). One arg per line.
    try:
        argv_path = os.path.join(_home(), ".claude", "stub-launch-argv.txt")
        os.makedirs(os.path.dirname(argv_path), exist_ok=True)
        with open(argv_path, "w") as af:
            af.write("\n".join(argv) + "\n")
    except OSError:
        pass

    pidf = PidFile(name, cwd)
    jsonl = Jsonl(cwd)

    relay_child = _maybe_start_relay(argv)

    # Citation #2: render the dev-channels popup, then BLOCK on stdin for the
    # blind-Enter dismiss BEFORE the PID file is written (waitForSessionReady's
    # premise: the popup precedes the PID file). A fixed, ordered PTY string.
    #
    # FIDELITY (A4 pass-(b) F1, orc-3 ruling 2026-06-05): the popup text is the
    # REAL captured dev-channels dialog VERBATIM (ANSI-stripped capture, A2
    # 2026-06-04 journal; pinned by sb boot.rs DEV_CHANNELS_TAIL + the
    # strip_ansi_on_captured_dev_channels_dialog test). The previous paraphrase
    # ("Development channels / Press Enter to continue") was dismissable only by
    # TS's blind-Enter loop; the Rust ADR-0005 answerer content-matches the real
    # title ("WARNING: Loading development channels") + the shared marker line
    # ("Enter to confirm") and — correctly — answers NOTHING else. Strings-only
    # change (sanction scope): busy/idle, JSONL, relay, and PID timing untouched.
    sys.stdout.write(
        "\r\nWARNING: Loading development channels\r\n"
        "--dangerously-load-development-channels is for local channel development only. "
        "Do not use this option to run channels you have downloaded off the internet.\r\n"
        "Please use --channels to run a list of approved channels.\r\n"
        "Channels: server:relay\r\n"
        "❯ 1. I am using this for local development\r\n"
        "  2. Exit\r\n"
        "Enter to confirm · Esc to cancel\r\n"
    )
    sys.stdout.flush()

    # Phase 1+2: consume the dismiss Enter (one CR/LF), THEN write the PID file.
    # The blind-Enter loop sends `\r`; either CR or LF dismisses.
    # SEAM (W2.3): when STUB_COUNT_PRE_PID_STDIN=1 the dismiss read also COUNTS the
    # stdin chars seen before the terminating CR (excluding the dismiss CR) and
    # writes them to the boot-stats sidecar. Dormant default: the plain
    # _read_one_submit() consumes identical bytes and writes no sidecar.
    if os.environ.get("STUB_COUNT_PRE_PID_STDIN") == "1":
        _, _pre_pid_count = _read_dismiss_counting()
        _write_boot_stats(_pre_pid_count)
    else:
        _read_one_submit()
    jsonl.ensure_exists()              # conversation file exists at session start
    # R2 NEGATIVE-CONTROL SEAMS (dormant by default; active ONLY under STUB_* env —
    # so the DEFAULT behaviour is byte-identical to the recorded stub and the
    # committed rows' proofs stay valid). Used by mutation/run_mutation_real.sh to
    # inject stub misbehaviours through the seams and prove the recorder/comparator
    # OUTPUT CHANGES and verify CATCHES each. NEVER set during recording.
    if os.environ.get("STUB_WITHHOLD_PID") == "1":
        # Withhold the PID file entirely: boot-readiness event NEVER fires.
        # (delayed/withheld PID file — rider R2). Keep the process alive so the
        # driver's readiness poll times out rather than the process exiting.
        _stub_hold_open()
        return 0
    _pid_delay = 0
    try:
        _pid_delay = int(os.environ.get("STUB_PID_DELAY_MS", "0"))
    except ValueError:
        _pid_delay = 0
    if _pid_delay > 0:
        time.sleep(_pid_delay / 1000.0)
    pidf.write()                       # PID file APPEARS -> readiness event part 1
    sys.stdout.write("ready\r\n> ")    # cosmetic; not asserted
    sys.stdout.flush()

    # Citation #4/#5/#6: the prompt REPL. Each submitted (non-empty) line:
    # go busy, append the user+assistant JSONL pair, go idle. Queue-to-busy is
    # naturally honoured: a line typed while we are mid-turn is read on the next
    # iteration (the kernel/TTY buffers it), so it drains after the current turn.
    while True:
        line = _read_one_submit()
        if line is None:
            break                       # EOF (client gone) -> exit
        # Record the line EXACTLY as submitted (real claude does NOT strip): a
        # trailing space etc. is load-bearing because send:pty --wait's findUserAnchor
        # (utils.ts:341-346) matches the user record's text against the sent message
        # BYTE-for-byte. Only treat a fully-empty submit (a bare remediation CR) as
        # a no-op turn.
        text = line
        if text.strip() == "":
            continue                    # bare Enter (e.g. remediation CR): no turn
        # SEAM (W2.3, prompt-gated): a submitted `STTY` line prints a deterministic
        # termios report INSTEAD of a normal turn (no busy/idle, no JSONL). Gated
        # on the exact prompt; no existing scenario submits an `STTY` line, so the
        # default REPL is unchanged.
        if text.strip() == "STTY":
            _emit_stty_report()
            continue
        pidf.set_status("busy")         # idle->busy (acceptance signal)
        jsonl.append_user(text)
        # Backlog generator (zmx-VT rows L6/L7): "EMIT <N>" prints N numbered
        # SBLINE rows to the PTY so zmx retains them server-side for reattach/
        # history. Deterministic content + order; NO timestamps.
        _maybe_emit_backlog(text)
        # Optional busy-HOLD: stay busy (NOT reading stdin) for STUB_BUSY_HOLD_MS so
        # a concurrent `send:pty` observes status=="busy" and takes decideSendPty's
        # send-queue path (utils.ts:297-299). The TTY buffers the queued send; we
        # read + drain it on the NEXT loop iteration (queue-to-busy). The JSONL
        # outcome (ordered user+assistant pairs) is deterministic; only the busy
        # window's duration varies, and timing is never the recorded expectation.
        hold_ms = 0
        try:
            hold_ms = int(os.environ.get("STUB_BUSY_HOLD_MS", "0"))
        except ValueError:
            hold_ms = 0
        if hold_ms > 0:
            # SEAM (W2.1): STUB_NO_QUEUE removes the stub's server-side queueing —
            # while busy it actively READS-AND-DISCARDS stdin instead of letting the
            # TTY buffer hold a concurrent send for the next iteration. With the seam
            # on, a message reaches the JSONL IFF the ENGINE held it until idle
            # (decideSendPty send-queue). DORMANT default: unset => we just sleep
            # (TTY buffers the queued send, drained next iteration) — byte-identical
            # to the recorded behaviour.
            if os.environ.get("STUB_NO_QUEUE") == "1":
                _drain_stdin_for(hold_ms / 1000.0)
            else:
                time.sleep(hold_ms / 1000.0)
        # R2 SEAM: withhold the assistant JSONL reply (missing JSONL reply — rider
        # R2). The user record still lands; the assistant pair never does, so
        # send:pty --wait's findUserAnchor finds the user record but no reply.
        if os.environ.get("STUB_WITHHOLD_JSONL") != "1":
            jsonl.append_assistant(_reply_for(text))
            # Echo the reply to the PTY so it enters zmx's server-side VT backlog.
            sys.stdout.write(_reply_for(text) + "\r\n> ")
            sys.stdout.flush()
        pidf.set_status("idle")         # busy->idle (completion signal)

    if relay_child:
        try:
            os.kill(relay_child, 9)
        except OSError:
            pass
    return 0


def _read_one_submit():
    """Read one submitted line from stdin (blocking). Returns the line WITHOUT
    the trailing newline, or None on EOF. A submit is a CR or LF terminator."""
    buf = []
    while True:
        try:
            ch = sys.stdin.read(1)
        except (IOError, OSError) as e:
            if e.errno == errno.EINTR:
                continue
            return None
        if ch == "":
            return None if not buf else "".join(buf)
        if ch in ("\r", "\n"):
            return "".join(buf)
        buf.append(ch)


def _drain_stdin_for(duration_s):
    """SEAM (W2.1) helper: for `duration_s` seconds, actively read-and-DISCARD any
    stdin that arrives (instead of leaving it in the TTY buffer to be drained next
    iteration). Uses select so it never blocks past the window. Only reached when
    STUB_NO_QUEUE=1 (dormant otherwise)."""
    end = time.time() + duration_s
    while True:
        remaining = end - time.time()
        if remaining <= 0:
            return
        try:
            r, _, _ = select.select([sys.stdin], [], [], remaining)
        except (IOError, OSError) as e:
            if e.errno == errno.EINTR:
                continue
            return
        if not r:
            return                      # window elapsed with no input
        try:
            ch = sys.stdin.read(1)
        except (IOError, OSError) as e:
            if e.errno == errno.EINTR:
                continue
            return
        if ch == "":
            return                      # EOF (client gone)
        # else: discard the char and keep draining until the window ends.


def _read_dismiss_counting():
    """Read the dismiss line like _read_one_submit, but COUNT the stdin chars
    read BEFORE the terminating CR/LF (SEAM W2.3, P2 pre-PID stdin counter). The
    terminating CR is the dismiss itself and is EXCLUDED (it is the terminator,
    never appended to buf). By stub construction this is 0: the stub reads exactly
    one CR then writes the PID file, independent of TS's 2s-interval blind-Enter
    timing. Returns (line, count). The bytes consumed are identical to
    _read_one_submit, so the default boot path is unchanged."""
    buf = []
    while True:
        try:
            ch = sys.stdin.read(1)
        except (IOError, OSError) as e:
            if e.errno == errno.EINTR:
                continue
            return (None if not buf else "".join(buf), len(buf))
        if ch == "":
            return (None if not buf else "".join(buf), len(buf))
        if ch in ("\r", "\n"):
            return ("".join(buf), len(buf))
        buf.append(ch)


def _write_boot_stats(pre_pid_count):
    """SEAM (W2.3): expose the pre-PID stdin char count via a sidecar
    ~/.claude/stub-boot-stats.json — written ONLY when STUB_COUNT_PRE_PID_STDIN=1
    (dormant otherwise, so the default boot writes no extra file)."""
    try:
        path = os.path.join(_home(), ".claude", "stub-boot-stats.json")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            json.dump({"input_chars_before_pidfile": pre_pid_count}, f)
    except OSError:
        pass


if __name__ == "__main__":
    sys.exit(main())
