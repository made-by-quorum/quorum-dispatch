#!/usr/bin/env bash
# test/golden/scenarios/bootstrap_output_audit.sh — gate rows G-B5 / G-B3 /
# G-B4 / G-N2 (RUNTIME content audit + relay/shell consent behavior, in a
# scratch-HOME jail).
#
# Runs the REAL Rust `qd bootstrap` binary inside a per-run hermetic jail and
# asserts the ENGINE invariants that no unit test can prove (they exercise the
# live binary end-to-end, not the pure deciders):
#
#   G-B5  RUNTIME content-audit: bootstrap output contains NO forbidden tokens
#         (the scope-audit deny set + substrate/marketplace + spawn/qb).
#   G-B1  idempotence: bootstrap runs TWICE in the same scratch HOME → exit 0
#         both times, identical state-dir layout (~/.quorum/dispatch + ~/.quorum/dispatch/state present).
#   G-B3  consent default-No / non-TTY: bootstrap run NON-INTERACTIVELY (stdin
#         not a TTY) → relay registration and the shell-integration line are
#         NEVER offered and NOTHING is written (relay not registered, rc file
#         untouched), no hang, exit 0, register-later / add-later pointers shown.
#   G-B4  consent ACCEPTED (PTY + explicit `y`): the relay is REGISTERED with
#         Claude Code (the stub `claude mcp add` runs with `-s user relay -- <the
#         binary under test> relay:serve`) and the init line lands in the jailed
#         rc file. A configured re-run SKIPS both offers (no duplicate rc line).
#   G-B6  claude-missing precondition: with `claude` NOT on PATH, the relay step
#         is a notice ("cannot configure — `claude` is not on PATH"), never a
#         prompt or a failure; bootstrap still exits 0.
#   G-N2  `server:channels` non-reintroduction: bootstrap never writes/echoes
#         `server:channels` into any config (A2 carry).
#
# 2026-06-10 (ADR 0017): the relay is registered through Claude Code's own
# `claude mcp` CLI (CC owns its config location/format). This gate STUBS
# `claude` in-jail (a script emulating `mcp get|add|remove -s user relay`
# against a jailed state file) so the arms are hermetic + fast and never touch a
# real Claude Code config — the accept arm asserts the stub recorded the add
# with the binary-under-test as the command.
#
# Bash 3.2 / POSIX floor (macOS /bin/bash): no associative arrays, no ${var,,},
# no mapfile. Run directly: `bash test/golden/scenarios/bootstrap_output_audit.sh`.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
. "$REPO_ROOT/test/golden/lib/jail.sh"

# The Rust binary under test. Default to the workspace debug build; overridable.
QD_BIN="${QD_BIN:-$REPO_ROOT/target/debug/qd}"
if [ ! -x "$QD_BIN" ]; then
    echo "bootstrap-audit: building qd (no binary at $QD_BIN)..." >&2
    ( cd "$REPO_ROOT" && ./scripts/build-lock.sh cargo build -p qd --bin qd >/dev/null 2>&1 ) \
        || { echo "FAIL: could not build qd" >&2; exit 2; }
fi
export JAIL_QD_CMD="$QD_BIN"
# NOTE: the relay registration now uses the BARE `qd` command (resolved via PATH,
# never goes stale on a binary move — relay-path hardening v2), so the recorded
# `mcp add` argv carries `qd`, NOT this binary's absolute path. The G-B4 assertion
# below greps for the literal bare command rather than a resolved path.

PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }

# The forbidden-token set (G-B5): the scope-audit deny set + the qb-era
# additions. Case-insensitive. A match in bootstrap's stdout/stderr is a
# content leak (qb-side VOCABULARY stays banned even though the shell + relay
# steps are sanctioned engine behavior).
FORBIDDEN='substrate|marketplace|QD_PLUGINS_ROOT|plugins-root|plugins_root|spawn|qb'

# --- jail up ---------------------------------------------------------------
jail_establish >/dev/null 2>&1 || { echo "FAIL: jail_establish" >&2; exit 2; }
trap 'jail_teardown >/dev/null 2>&1 || true' EXIT INT TERM

# Pin the shell so the wrapper step's rc target is deterministic in-jail.
export SHELL=/bin/bash
JAIL_BASHRC="$HOME/.bashrc"

# Mute the host-wide localhost relay port-scan so the relay health FYI is
# deterministic on a shared host (brano runs a real relay on 8900-9000).
export QRM_RELAY_DISABLE_SCAN=1

# --- in-jail stubs: `claude` (the MCP registrar) + `zmx` (capable) ---------
# The `claude` stub emulates exactly the three subcommands register.rs drives:
#   claude mcp get    relay         → exit 0 if registered (state file present), else 1
#   claude mcp add -s user relay -- <exe> relay:serve  → record argv, mark registered
#   claude mcp remove -s user relay → clear registered
# State = a jailed file holding the last `add` argv (so we can assert the
# registered command path is the binary under test). Hermetic: never a real CC.
STUB_DIR="$JAIL_ROOT/stubbin"
mkdir -p "$STUB_DIR"
STUB_STATE="$JAIL_ROOT/claude-mcp-relay.state"
cat > "$STUB_DIR/claude" <<EOF
#!/usr/bin/env bash
state="$STUB_STATE"
if [ "\$1" = "mcp" ]; then
  case "\$2" in
    get)    [ -f "\$state" ] && exit 0 || exit 1 ;;
    add)    printf '%s ' "\$@" > "\$state"; echo "Added stdio MCP server relay"; exit 0 ;;
    remove) rm -f "\$state"; echo "Removed MCP server relay"; exit 0 ;;
  esac
fi
exit 0
EOF
chmod +x "$STUB_DIR/claude"
# Capable zmx shim (Usage: + `send <target>`) so the macOS+brew zmx-install
# prompt NEVER fires under the PTY — this gate exercises ONLY the relay + shell
# offers (the 2026-06-05 CI macOS leg ground ~52min on a real `brew install`).
cat > "$STUB_DIR/zmx" <<'EOF'
#!/usr/bin/env bash
echo "Usage: zmx <command>"
echo "Commands:"
echo "  run <name>     run a session"
echo "  send <target>  send keys"
exit 0
EOF
chmod +x "$STUB_DIR/zmx"

# Most arms run with the stubs on PATH (claude PRESENT). PATH is inherited by
# the qd child through jail_qd. Save the original for the claude-missing arm.
ORIG_PATH="$PATH"
export PATH="$STUB_DIR:$PATH"

# ===========================================================================
# G-B5 + G-B1 + G-B3 + G-N2: NON-INTERACTIVE bootstrap, run TWICE.
# stdin from /dev/null → not a TTY → nothing offered, nothing registered.
# ===========================================================================
OUT1="$JAIL_ROOT/run1.out"
OUT2="$JAIL_ROOT/run2.out"

jail_qd bootstrap </dev/null >"$OUT1" 2>&1
RC1=$?
jail_qd bootstrap </dev/null >"$OUT2" 2>&1
RC2=$?

# G-B1: exit 0 both runs.
[ "$RC1" = "0" ] && ok "G-B1/exit-0-first-run" || bad "G-B1/exit-0-first-run (rc=$RC1)"
[ "$RC2" = "0" ] && ok "G-B1/exit-0-second-run" || bad "G-B1/exit-0-second-run (rc=$RC2)"

# G-B1: state-dir layout present.
if [ -d "$QD_HOME" ] && [ -d "$QD_HOME/state" ]; then
    ok "G-B1/state-dirs-present (~/.quorum/dispatch + ~/.quorum/dispatch/state)"
else
    bad "G-B1/state-dirs-present — missing $QD_HOME or $QD_HOME/state"
fi

# G-B5: NO forbidden token in EITHER run.
if grep -iE "$FORBIDDEN" "$OUT1" >/dev/null 2>&1; then
    bad "G-B5/no-forbidden-token-run1"
    echo "    leaked:" >&2; grep -iEn "$FORBIDDEN" "$OUT1" | sed 's/^/    /' >&2
else
    ok "G-B5/no-forbidden-token-run1"
fi
if grep -iE "$FORBIDDEN" "$OUT2" >/dev/null 2>&1; then
    bad "G-B5/no-forbidden-token-run2"
else
    ok "G-B5/no-forbidden-token-run2"
fi

# G-B5 sanity: output IS `[bootstrap]`-prefixed (we audited real output).
if grep -q '^\[bootstrap\]' "$OUT1"; then
    ok "G-B5/output-is-bootstrap-prefixed"
else
    bad "G-B5/output-is-bootstrap-prefixed — no [bootstrap] lines in run1"
fi

# G-B3: non-TTY → relay NOT offered, NOT registered, register-later pointer.
if grep -qi 'relay: not configured' "$OUT1"; then
    ok "G-B3/relay-not-configured-reported"
else
    bad "G-B3/relay-not-configured-reported — expected 'relay: not configured'"
fi
if grep -qi 'qd relay:register' "$OUT1"; then
    ok "G-B3/relay-register-later-pointer"
else
    bad "G-B3/relay-register-later-pointer — no 'qd relay:register' pointer"
fi
if [ -e "$STUB_STATE" ]; then
    bad "G-B3/no-register-on-non-tty — relay was registered without a TTY"
else
    ok "G-B3/no-register-on-non-tty"
fi

# G-B3 (shell step): non-TTY → init line NOT offered, rc NOT modified.
if grep -qi 'qd init bash' "$OUT1"; then
    ok "G-B3/shell-add-later-pointer"
else
    bad "G-B3/shell-add-later-pointer — no 'qd init bash' pointer"
fi
if [ -e "$JAIL_BASHRC" ] && grep -q 'qd init' "$JAIL_BASHRC" 2>/dev/null; then
    bad "G-B3/no-rc-write-on-non-tty — jailed .bashrc gained the init line"
else
    ok "G-B3/no-rc-write-on-non-tty"
fi

# G-N2: `server:channels` nowhere in output nor jailed config tree.
if grep -rIE 'server:channels' "$OUT1" "$OUT2" >/dev/null 2>&1; then
    bad "G-N2/server-channels-absent-from-output"
else
    ok "G-N2/server-channels-absent-from-output"
fi
if grep -rIE 'server:channels' "$HOME" "$QD_HOME" >/dev/null 2>&1; then
    bad "G-N2/server-channels-absent-from-config-tree"
else
    ok "G-N2/server-channels-absent-from-config-tree"
fi

# ===========================================================================
# G-B6: claude-missing precondition — PATH WITHOUT the stub (and without real
# claude). The relay step must be a NOTICE, never a prompt/failure.
# ===========================================================================
OUTM="$JAIL_ROOT/runmissing.out"
PATH="/usr/bin:/bin" jail_qd bootstrap </dev/null >"$OUTM" 2>&1
RCM=$?
[ "$RCM" = "0" ] && ok "G-B6/claude-missing-exit-0" || bad "G-B6/claude-missing-exit-0 (rc=$RCM)"
if grep -qi 'cannot configure' "$OUTM" && grep -qi '`claude` is not on PATH' "$OUTM"; then
    ok "G-B6/claude-missing-notice"
else
    bad "G-B6/claude-missing-notice — expected the 'cannot configure / not on PATH' line"
fi
if [ -e "$STUB_STATE" ]; then
    bad "G-B6/claude-missing-no-register — something registered with claude absent"
else
    ok "G-B6/claude-missing-no-register"
fi

# ===========================================================================
# G-B4: consent ACCEPTED → registration runs in-jail (TTY + `y`). Driven under
# a PTY when python3 is available; else SKIPPED-with-ledger (unit tests cover
# accept). The stub `claude` is on PATH (restored above).
# ===========================================================================
B4_DONE=0
if command -v python3 >/dev/null 2>&1; then
    OUT4="$JAIL_ROOT/run4.out"
    # PATH already has the stub dir prepended (ORIG_PATH restore happened only
    # for the G-B6 sub-shell). Drive bootstrap under a PTY (isatty → offers fire).
    python3 - "$JAIL_QD_CMD" "$OUT4" <<'PYEOF'
import os, pty, sys, time, signal
qd, outpath = sys.argv[1], sys.argv[2]
out = open(outpath, "wb")
def read(fd):
    try:
        return os.read(fd, 1024)
    except OSError:
        return b""
# Feed "y" to the relay prompt and the shell prompt (zmx is shimmed-capable).
fed = [b"y\n", b"y\n", b"y\n"]
pid, fd = pty.fork()
if pid == 0:
    os.execv(qd, [qd, "bootstrap"])
else:
    import select
    deadline = time.time() + 120
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 5)
        if not r:
            break
        data = read(fd)
        out.write(data)
        if not data:
            break
        if fed:
            try:
                os.write(fd, fed.pop(0))
            except OSError:
                break
    out.close()
    reap_deadline = time.time() + 60
    code = None
    while time.time() < reap_deadline:
        wpid, status = os.waitpid(pid, os.WNOHANG)
        if wpid == pid:
            code = os.WEXITSTATUS(status) if os.WIFEXITED(status) else 1
            break
        time.sleep(0.2)
    if code is None:
        sys.stderr.write("G-B4 pty driver: child did not exit by deadline — SIGKILL\n")
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
        os.waitpid(pid, 0)
        sys.exit(1)
    sys.exit(0 if code == 0 else 1)
PYEOF
    RC4=$?
    B4_DONE=1
    [ "$RC4" = "0" ] && ok "G-B4/accept-run-exit-0" || bad "G-B4/accept-run-exit-0 (rc=$RC4)"
    # The relay was registered: the stub recorded a `mcp add` argv.
    if [ -f "$STUB_STATE" ] && grep -q 'mcp add -s user relay --' "$STUB_STATE"; then
        ok "G-B4/relay-registered-via-claude-mcp-add"
    else
        bad "G-B4/relay-registered-via-claude-mcp-add — stub recorded no add: $(cat "$STUB_STATE" 2>/dev/null)"
    fi
    # The registered command is the BARE `qd` (resolved via PATH, never goes
    # stale on a binary move — relay-path hardening v2), NOT an absolute path.
    # The exact tail is `... relay -- qd relay:serve`.
    if grep -q 'relay -- qd relay:serve' "$STUB_STATE" 2>/dev/null; then
        ok "G-B4/registered-command-is-bare-qd"
    else
        bad "G-B4/registered-command-is-bare-qd — recorded: $(cat "$STUB_STATE" 2>/dev/null)"
    fi
    # The shell-integration line landed in the JAILED .bashrc.
    if [ -f "$JAIL_BASHRC" ] && grep -q 'qd init bash' "$JAIL_BASHRC"; then
        ok "G-B4/init-line-added-to-jailed-bashrc"
    else
        bad "G-B4/init-line-added-to-jailed-bashrc — missing init line in $JAIL_BASHRC"
    fi
    # G-B5 holds on the interactive run too.
    if grep -iE "$FORBIDDEN" "$OUT4" >/dev/null 2>&1; then
        bad "G-B5/no-forbidden-token-accept-run"
    else
        ok "G-B5/no-forbidden-token-accept-run"
    fi

    # G-B4 (configured skips offers): re-run NON-interactively. The stub now
    # reports the relay registered (state present) and the init line is present.
    OUT5="$JAIL_ROOT/run5.out"
    jail_qd bootstrap </dev/null >"$OUT5" 2>&1
    RC5=$?
    [ "$RC5" = "0" ] && ok "G-B4/configured-rerun-exit-0" || bad "G-B4/configured-rerun-exit-0 (rc=$RC5)"
    if grep -qi 'relay: configured' "$OUT5"; then
        ok "G-B4/configured-reported"
    else
        bad "G-B4/configured-reported — expected 'relay: configured' in rerun"
    fi
    if grep -qi 'shell: integration configured' "$OUT5"; then
        ok "G-B4/shell-configured-reported"
    else
        bad "G-B4/shell-configured-reported — expected 'shell: integration configured' in rerun"
    fi
    INIT_COUNT="$(grep -c 'qd init bash' "$JAIL_BASHRC" 2>/dev/null || echo 0)"
    if [ "$INIT_COUNT" = "1" ]; then
        ok "G-B4/init-line-not-duplicated"
    else
        bad "G-B4/init-line-not-duplicated — found $INIT_COUNT copies"
    fi
fi
if [ "$B4_DONE" = "0" ]; then
    echo "ledger: G-B4 PTY arm SKIPPED (no python3 for a pseudo-TTY); unit tests" >&2
    echo "ledger:   bootstrap::tests::check_relay_tty_accepted_registers +" >&2
    echo "ledger:   bootstrap::tests::check_wrapper_tty_accepted_adds_line cover accept." >&2
fi

# ===========================================================================
# Health FYI: seed a relay sidecar so discovery finds a healthy server.
# ===========================================================================
RELAY_DIR="$HOME/.claude/relay"
mkdir -p "$RELAY_DIR"
cat > "$RELAY_DIR/seeded.json" <<EOF
{"port": $JAIL_RELAY_PORT, "sessionId": "${JAIL_PREFIX}seed", "pid": 0, "status": "ok"}
EOF
OUT6="$JAIL_ROOT/run6.out"
jail_qd bootstrap </dev/null >"$OUT6" 2>&1
RC6=$?
[ "$RC6" = "0" ] && ok "health/seeded-run-exit-0" || bad "health/seeded-run-exit-0 (rc=$RC6)"
if grep -qi 'server is up' "$OUT6"; then
    ok "health/fyi-line-reported"
else
    bad "health/fyi-line-reported — expected a 'server is up' FYI line"
fi

# Quiet the unused-var linter for ORIG_PATH on shells that warn (kept for clarity).
: "$ORIG_PATH"

printf '\n--- bootstrap_output_audit: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
