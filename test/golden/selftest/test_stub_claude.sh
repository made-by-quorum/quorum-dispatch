#!/usr/bin/env bash
# test/golden/selftest/test_stub_claude.sh — selftests for the deterministic
# stub_claude counterpart (§S substrate; rider R1).
#
# Drives the stub DIRECTLY (no jail, no real sb, no real fixtures/), each in a
# throwaway HOME, and proves the five behaviours the contract rows rely on:
#   (1) PID file appears under <HOME>/.claude/sessions after the dismiss Enter,
#       matched by name (lifecycle.ts:135-161), with status idle.
#   (2) relay /health answers the RelayHealth shape (session.ts:185-212).
#   (3) /message round-trips -> {message_id} and /replies returns the reply
#       (send.ts:414-475).
#   (4) a JSONL user+assistant pair lands (send.ts:314-339).
#   (5) DETERMINISM: two independent boots produce byte-identical PID-file +
#       JSONL after PID/timestamp normalization (the double-record premise).
#
# All HTTP is to 127.0.0.1 on an EPHEMERAL high port in a throwaway HOME — no jail
# needed because the stub touches only its own $HOME and binds only $SB_RELAY_PORT.
# Bash 3.2 floor. python3>=3.6.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"          # test/golden
STUB="$ROOT/lib/stub_claude/stub_claude.py"
PY="${PYTHON:-python3}"

PASS=0; FAIL=0; SKIP=0
ok()   { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
skip() { SKIP=$((SKIP+1)); printf '  SKIP %s\n' "$1"; }
# A7 (CI wiring exposed): two rows assert macOS COOKED-MODE behavior keyed on
# MAX_CANON=1024 (ADR-0011 4-probe table). Linux's N_TTY canonical bound differs
# (≈4096), so a >1024B line does NOT drop cooked on Linux, and the STTY reporter
# path over a cooked PTY isn't reached the same way (same root as the named
# termios_report_linux residual). These rows are platform-gated to macOS; the
# Linux cooked-mode story is its own named residual, NOT a regression.
IS_MACOS=0; [ "$(uname -s)" = "Darwin" ] && IS_MACOS=1

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/stub-selftest.XXXXXX")"
trap 'rm -rf "$SCRATCH" 2>/dev/null || true' EXIT INT TERM

# A python driver: boot the stub under a pipe, feed an Enter + optional prompt +
# optional relay round-trip, then report a JSON summary on stdout. Kept in python
# so the PTY-free drive + HTTP client stays portable (the recorder owns real PTY
# capture; here we only need stdin-pipe semantics, which the stub supports).
DRIVER="$SCRATCH/drive.py"
cat > "$DRIVER" <<'PYEOF'
import os, sys, json, time, subprocess, urllib.request
stub, home, name, port, prompt = sys.argv[1:6]
os.environ["HOME"] = home
if port != "0":
    os.environ["SB_RELAY_PORT"] = port
args = [sys.executable, stub, "--name", name]
if port != "0":
    args.append("server:relay")
p = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.PIPE)
p.stdin.write(b"\r"); p.stdin.flush(); time.sleep(0.6)   # dismiss Enter
result = {"pidfile": None, "health": None, "message_id": None, "reply": None,
          "jsonl": None}
sd = os.path.join(home, ".claude", "sessions")
if os.path.isdir(sd):
    for f in os.listdir(sd):
        if f.endswith(".json"):
            result["pidfile"] = json.load(open(os.path.join(sd, f)))
if port != "0":
    try:
        result["health"] = json.loads(
            urllib.request.urlopen("http://127.0.0.1:%s/health" % port, timeout=3).read())
        req = urllib.request.Request("http://127.0.0.1:%s/message" % port,
            data=json.dumps({"text": "hi there", "from_session": "cli"}).encode(),
            headers={"Content-Type": "application/json"}, method="POST")
        mid = json.loads(urllib.request.urlopen(req, timeout=3).read())["message_id"]
        result["message_id"] = mid
        result["reply"] = json.loads(
            urllib.request.urlopen("http://127.0.0.1:%s/replies/%s" % (port, mid),
                                   timeout=3).read())["text"]
    except Exception as e:
        result["health_error"] = str(e)
if prompt != "-":
    p.stdin.write((prompt + "\r").encode()); p.stdin.flush(); time.sleep(0.4)
    pd = os.path.join(home, ".claude", "projects")
    for r, _, fs in os.walk(pd):
        for f in fs:
            result["jsonl"] = open(os.path.join(r, f)).read()
p.stdin.close()
try: p.wait(timeout=5)
except Exception: p.kill()
print(json.dumps(result))
PYEOF

# --- normalizer: collapse the stub's own PID + the relay port for determinism --
norm() { sed -E -e 's/"pid": ?[0-9]+/"pid":<PID>/g' -e 's/"port": ?[0-9]+/"port":<PORT>/g'; }

# ---- Test 1+4: boot writes a name-matched idle PID file; prompt -> JSONL pair --
H1="$SCRATCH/home1"; mkdir -p "$H1"
OUT1="$("$PY" "$DRIVER" "$STUB" "$H1" "selftest-boot" "0" "hello world" 2>/dev/null)"
if printf '%s' "$OUT1" | grep -q '"name": "selftest-boot"'; then ok "PID file present, name-matched"; else bad "PID file name match (got: $OUT1)"; fi
if printf '%s' "$OUT1" | grep -q '"status": "idle"'; then ok "PID file status=idle after boot"; else bad "PID file status idle"; fi
if printf '%s' "$OUT1" | grep -q 'type.*user.*hello world'; then ok "JSONL user record landed"; else bad "JSONL user record"; fi
if printf '%s' "$OUT1" | grep -q 'STUB-REPLY to: hello world'; then ok "JSONL assistant reply landed"; else bad "JSONL assistant reply"; fi

# ---- Test 2+3: relay /health + /message round-trip ----------------------------
H2="$SCRATCH/home2"; mkdir -p "$H2"
PORT2=29137
OUT2="$("$PY" "$DRIVER" "$STUB" "$H2" "selftest-relay" "$PORT2" "-" 2>/dev/null)"
if printf '%s' "$OUT2" | grep -q '"status": "ok"'; then ok "/health answers status ok"; else bad "/health (got: $OUT2)"; fi
if printf '%s' "$OUT2" | grep -q '"port": '"$PORT2"; then ok "/health reports the bound port"; else bad "/health port"; fi
if printf '%s' "$OUT2" | grep -qE '"message_id": "mid-[0-9a-f]+"'; then ok "/message returns a message_id"; else bad "/message message_id"; fi
if printf '%s' "$OUT2" | grep -q '"reply": "STUB-REPLY to: hi there"'; then ok "/replies returns the round-tripped reply"; else bad "/replies reply"; fi
# sidecar exists under the jailed HOME
if [ -f "$H2/.claude/relay/"*".json" ] 2>/dev/null || ls "$H2/.claude/relay/"*.json >/dev/null 2>&1; then ok "relay sidecar written under HOME/.claude/relay"; else bad "relay sidecar"; fi

# ---- Test 5: determinism — two boots, byte-identical after normalize ----------
HA="$SCRATCH/homeA"; HB="$SCRATCH/homeB"; mkdir -p "$HA" "$HB"
OA="$("$PY" "$DRIVER" "$STUB" "$HA" "det" "0" "same prompt" 2>/dev/null | norm)"
OB="$("$PY" "$DRIVER" "$STUB" "$HB" "det" "0" "same prompt" 2>/dev/null | norm)"
if [ "$OA" = "$OB" ]; then ok "two independent boots are byte-identical after normalize"; else
    bad "determinism: boots differ"
    printf 'A: %s\nB: %s\n' "$OA" "$OB" | head -4
fi

# =============================================================================
# 0b DELTA-STRENGTH seam coverage (v1.8.0): each new seam FIRES under its env /
# prompt, and the DEFAULT path (seams unset) is unchanged. A second python driver
# exercises the boot-level seams (two-stage write, pre-PID counter) and the REPL
# seams (STUB_NO_QUEUE, STTY) directly over the stdin pipe.
# =============================================================================
SEAMDRV="$SCRATCH/seamdrive.py"
cat > "$SEAMDRV" <<'PYEOF'
import os, sys, json, time, subprocess
# usage: seamdrive.py <stub> <home> <mode>
stub, home, mode = sys.argv[1:4]
os.environ["HOME"] = home
env = dict(os.environ)

def boot(extra_env=None, prompts=None, settle=0.6):
    e = dict(env)
    if extra_env:
        e.update(extra_env)
    p = subprocess.Popen([sys.executable, stub, "--name", "seam-" + mode],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, env=e)
    p.stdin.write(b"\r"); p.stdin.flush(); time.sleep(settle)  # dismiss Enter
    for (msg, wait) in (prompts or []):
        p.stdin.write((msg + "\r").encode()); p.stdin.flush(); time.sleep(wait)
    return p

def finish(p):
    try:
        p.stdin.close()
    except Exception:
        pass
    try:
        out, _ = p.communicate(timeout=8)
    except Exception:
        p.kill(); out = b""
    return out

def pidfile_text():
    sd = os.path.join(home, ".claude", "sessions")
    if not os.path.isdir(sd):
        return None
    for f in os.listdir(sd):
        if f.endswith(".json"):
            return open(os.path.join(sd, f)).read()
    return None

def jsonl_text():
    pd = os.path.join(home, ".claude", "projects")
    for r, _, fs in os.walk(pd):
        for f in fs:
            if f.endswith(".jsonl"):
                return open(os.path.join(r, f)).read()
    return ""

res = {}

if mode == "two_stage":
    # Two-stage write seam: after settle the final PID file must be COMPLETE,
    # valid JSON (the partial prefix was overwritten). Use a short gap.
    p = boot({"STUB_TWO_STAGE_PID_WRITE": "1", "STUB_TWO_STAGE_GAP_MS": "300"},
             settle=1.2)
    txt = pidfile_text()
    res["pidfile_present"] = txt is not None
    try:
        d = json.loads(txt) if txt else None
        res["pidfile_valid_json"] = d is not None and d.get("status") == "idle"
    except Exception:
        res["pidfile_valid_json"] = False
    finish(p)

elif mode == "count":
    # Pre-PID stdin counter: sidecar written, count == 0 by construction.
    p = boot({"STUB_COUNT_PRE_PID_STDIN": "1"})
    sc = os.path.join(home, ".claude", "stub-boot-stats.json")
    res["sidecar_present"] = os.path.exists(sc)
    if res["sidecar_present"]:
        res["count"] = json.load(open(sc)).get("input_chars_before_pidfile")
    finish(p)

elif mode == "count_dormant":
    # Default: NO sidecar when the env is unset.
    p = boot()
    sc = os.path.join(home, ".claude", "stub-boot-stats.json")
    res["sidecar_present"] = os.path.exists(sc)
    finish(p)

elif mode == "no_queue":
    # STUB_NO_QUEUE: while busy (held by STUB_BUSY_HOLD_MS) a concurrent send is
    # read-and-DISCARDED, so the SECOND message never lands in the JSONL. Send
    # msg1 (holds busy), then msg2 DURING the hold; only msg1's user record lands.
    p = boot({"STUB_NO_QUEUE": "1", "STUB_BUSY_HOLD_MS": "1500"},
             prompts=[("first-holds-busy", 0.4), ("second-during-busy", 2.0)])
    j = jsonl_text()
    res["first_present"] = "first-holds-busy" in j
    res["second_discarded"] = "second-during-busy" not in j
    finish(p)

elif mode == "queue_dormant":
    # Default (seam unset): the second message IS queued (TTY-buffered) and
    # drains -> both user records land.
    p = boot({"STUB_BUSY_HOLD_MS": "1500"},
             prompts=[("first-holds-busy", 0.4), ("second-during-busy", 2.0)])
    j = jsonl_text()
    res["first_present"] = "first-holds-busy" in j
    res["second_present"] = "second-during-busy" in j
    finish(p)

elif mode == "stty":
    # STTY prompt reporter: a submitted `STTY` line prints a deterministic termios
    # report to the PTY and takes NO turn (no JSONL user record for it).
    p = boot(prompts=[("STTY", 0.5)])
    out = finish(p)
    res["report_present"] = b"STTY-REPORT" in out
    j = jsonl_text()
    res["no_turn_for_stty"] = "STTY" not in j

print(json.dumps(res))
PYEOF

# ---- Test 6: STUB_TWO_STAGE_PID_WRITE — final PID file complete + valid -------
HT="$SCRATCH/two_stage"; mkdir -p "$HT"
OT="$("$PY" "$SEAMDRV" "$STUB" "$HT" "two_stage" 2>/dev/null)"
if printf '%s' "$OT" | grep -q '"pidfile_valid_json": true'; then ok "two-stage seam: final PID file is complete valid JSON"; else bad "two-stage seam (got: $OT)"; fi

# ---- Test 7: STUB_COUNT_PRE_PID_STDIN — sidecar written, count 0 --------------
HC="$SCRATCH/count"; mkdir -p "$HC"
OC="$("$PY" "$SEAMDRV" "$STUB" "$HC" "count" 2>/dev/null)"
if printf '%s' "$OC" | grep -q '"sidecar_present": true'; then ok "pre-PID counter seam: sidecar written"; else bad "pre-PID counter sidecar (got: $OC)"; fi
if printf '%s' "$OC" | grep -q '"count": 0'; then ok "pre-PID counter seam: count is 0 by construction"; else bad "pre-PID counter value (got: $OC)"; fi

# ---- Test 8: counter DORMANT — no sidecar when env unset ----------------------
HCD="$SCRATCH/count_dormant"; mkdir -p "$HCD"
OCD="$("$PY" "$SEAMDRV" "$STUB" "$HCD" "count_dormant" 2>/dev/null)"
if printf '%s' "$OCD" | grep -q '"sidecar_present": false'; then ok "pre-PID counter DORMANT: no sidecar when unset"; else bad "counter dormancy (got: $OCD)"; fi

# ---- Test 9: STUB_NO_QUEUE — busy-window send discarded -----------------------
HNQ="$SCRATCH/no_queue"; mkdir -p "$HNQ"
ONQ="$("$PY" "$SEAMDRV" "$STUB" "$HNQ" "no_queue" 2>/dev/null)"
if printf '%s' "$ONQ" | grep -q '"first_present": true'; then ok "no-queue seam: first (busy-holding) message landed"; else bad "no-queue first msg (got: $ONQ)"; fi
if printf '%s' "$ONQ" | grep -q '"second_discarded": true'; then ok "no-queue seam: busy-window send discarded (not queued)"; else bad "no-queue discard (got: $ONQ)"; fi

# ---- Test 10: queue DORMANT — busy-window send IS queued (both land) ----------
HQD="$SCRATCH/queue_dormant"; mkdir -p "$HQD"
OQD="$("$PY" "$SEAMDRV" "$STUB" "$HQD" "queue_dormant" 2>/dev/null)"
if printf '%s' "$OQD" | grep -q '"second_present": true'; then ok "no-queue DORMANT: busy-window send queued + drained (both land)"; else bad "queue dormancy (got: $OQD)"; fi

# ---- Test 11: STTY prompt reporter — deterministic report, no turn ------------
HS="$SCRATCH/stty"; mkdir -p "$HS"
OS="$("$PY" "$SEAMDRV" "$STUB" "$HS" "stty" 2>/dev/null)"
if [ "$IS_MACOS" = "1" ]; then
  if printf '%s' "$OS" | grep -q '"report_present": true'; then ok "STTY reporter: deterministic termios report emitted"; else bad "STTY report (got: $OS)"; fi
else
  skip "STTY reporter report_present (macOS cooked-PTY-specific; Linux = termios_report_linux residual)"
fi
if printf '%s' "$OS" | grep -q '"no_turn_for_stty": true'; then ok "STTY reporter: takes no turn (no JSONL user record)"; else bad "STTY no-turn (got: $OS)"; fi

# =============================================================================
# 0b DELTA-STRENGTH seam coverage (v1.8.1): STUB_RAW_STDIN. Driven over a REAL PTY
# (pty.openpty) — the seam only matters on a tty (it flips stdin termios to raw to
# defeat the cooked-mode canonical-line bound MAX_CANON). Brackets MAX_CANON from
# both sides: a >MAX_CANON line WITHOUT inter-chunk CR is DROPPED cooked (seam unset)
# and LANDS WHOLE raw (seam set). A <MAX_CANON line lands either way. _read_one_submit
# is unchanged; the seam only removes the canonical-buffer cap (rider C1/C2 mechanism).
# =============================================================================
RAWDRV="$SCRATCH/rawdrive.py"
cat > "$RAWDRV" <<'PYEOF'
import os, sys, json, time, select, pty, termios
# usage: rawdrive.py <stub> <home> <seam>   (seam in {"on","off"})
stub, home, seam = sys.argv[1:4]
os.environ["HOME"] = home
env = dict(os.environ)
if seam == "on":
    env["STUB_RAW_STDIN"] = "1"
else:
    env.pop("STUB_RAW_STDIN", None)

# A >MAX_CANON (1024) ASCII line with NO embedded CR — exactly the cooked-mode
# overflow shape deliverIdleTwoWrite produces (whole text, then a separate CR).
BIG = "BIGLINE-" + ("z" * 1500)            # 1508 bytes > 1024
SMALL = "SMALLLINE-" + ("y" * 200)         # 210 bytes < 1024

pid, fd = pty.fork()
if pid == 0:
    os.execvpe(sys.executable, [sys.executable, stub, "--name", "raw-" + seam], env)
    os._exit(127)

def w(s):
    os.write(fd, s.encode())

def drain(secs):
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try:
                if not os.read(fd, 65536):
                    return
            except OSError:
                return

w("\r"); time.sleep(0.6); drain(0.3)        # dismiss Enter
# First a SMALL line (must always land — sanity that the reader works in this mode
# BEFORE any overflow could perturb the cooked line discipline).
w(SMALL + "\r"); drain(1.0)
# Then the BIG line WITHOUT an embedded CR (chunk it like the engine: text then CR).
for i in range(0, len(BIG), 1024):
    w(BIG[i:i+1024]); time.sleep(0.05)
w("\r"); drain(1.5)
try:
    os.close(fd)
except OSError:
    pass
try:
    os.waitpid(pid, os.WNOHANG)
except OSError:
    pass

def jsonl_text():
    pd = os.path.join(home, ".claude", "projects")
    for r, _, fs in os.walk(pd):
        for f in fs:
            if f.endswith(".jsonl"):
                return open(os.path.join(r, f)).read()
    return ""

j = jsonl_text()
print(json.dumps({"big_present": BIG in j, "small_present": SMALL in j}))
PYEOF

# ---- Test 12: STUB_RAW_STDIN ON — >MAX_CANON line lands whole over a real PTY ---
HRO="$SCRATCH/raw_on"; mkdir -p "$HRO"
ORO="$("$PY" "$RAWDRV" "$STUB" "$HRO" "on" 2>/dev/null)"
if printf '%s' "$ORO" | grep -q '"big_present": true'; then ok "raw-stdin seam: >MAX_CANON (1508B) line lands whole (raw mode defeats the cooked cap)"; else bad "raw-stdin seam big line (got: $ORO)"; fi
if printf '%s' "$ORO" | grep -q '"small_present": true'; then ok "raw-stdin seam: <MAX_CANON line still lands (reader unchanged)"; else bad "raw-stdin seam small line (got: $ORO)"; fi

# ---- Test 13: STUB_RAW_STDIN OFF (dormant) — >MAX_CANON line DROPPED cooked -----
# Brackets MAX_CANON from the other side: the cooked default drops the >1024B line
# (the empirical cooked-drop signature the C2 mutation control also asserts), while
# the <MAX_CANON line still lands -> the seam is load-bearing, not vacuous.
HRF="$SCRATCH/raw_off"; mkdir -p "$HRF"
ORF="$("$PY" "$RAWDRV" "$STUB" "$HRF" "off" 2>/dev/null)"
if [ "$IS_MACOS" = "1" ]; then
  if printf '%s' "$ORF" | grep -q '"big_present": false'; then ok "raw-stdin DORMANT: >MAX_CANON line DROPPED cooked (seam is load-bearing)"; else bad "raw-stdin dormancy big drop (got: $ORF)"; fi
else
  # Linux N_TTY MAX_CANON (≈4096) > the 1508B probe, so the cooked default does
  # NOT drop it (big_present=true on Linux) — the macOS-1024 drop signature is
  # platform-specific. The seam's load-bearing-ness IS proven on macOS + by the
  # raw-ON row above (which passes on both: raw mode lands the big line).
  skip "raw-stdin DORMANT big-drop (macOS MAX_CANON=1024-specific; Linux bound ≈4096 — line lands, not dropped)"
fi
if printf '%s' "$ORF" | grep -q '"small_present": true'; then ok "raw-stdin DORMANT: <MAX_CANON line still lands (default reader unchanged)"; else bad "raw-stdin dormancy small (got: $ORF)"; fi

printf '\n[test_stub_claude] PASS=%d FAIL=%d SKIP=%d\n' "$PASS" "$FAIL" "$SKIP"
[ "$FAIL" -eq 0 ]
