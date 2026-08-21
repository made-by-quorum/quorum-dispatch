# Deploying `qd` to the live binary

**Merging to `main` does NOT ship anything.** The `qd` users run is a plain copy of a release
build, placed by hand on `PATH`. There is no CI/post-merge deploy step. After the 2026-06-06
cutover, a week of merged fixes sat unshipped because nobody rebuilt the binary — the narrow `ls`,
cursor fix, faster `qd new`, token counts, all on `main` but invisible to users. If you merged a
fix and the user "still sees the old behavior," check the deployed binary's age against the merge
first.

> **2026-07-07 postmortem — read this even if you skip nothing else.** A cutover deployed
> `target/debug/qd` as canonical (never `cargo build --release`), apparently a RAM-pressure
> shortcut under a tight free-memory box. `qd ls` went from sub-second to ~10s — dispatch's
> pure-Rust marks/ids folds are ~6-7x slower unoptimized at real ledger scale — which blew past
> frame's hardcoded 10s name-resolution timeout on every resolve and broke message delivery
> org-wide. This DEPLOY.md already said `cargo build --release` at the time; the doc mandate alone
> did not stop it. **Step 5 below now routes through `scripts/deploy-gate.sh`, which refuses to
> install a debug binary or a binary that fails a real-scale latency smoke check — this is no
> longer a step you can silently skip under pressure**, since a bare `mv` is no longer part of the
> documented procedure at all.

## Where the live binary lives (resolve it, don't assume)

The deploy target is wherever `qd` resolves on `PATH` — it differs per machine. **Resolve it; never
hardcode a path.**

```sh
LIVE="$(command -v qd)"            # e.g. ~/.local/bin/qd (Linux) or ~/.bun/bin/qd (legacy mac)
echo "deploying over: $LIVE"
```

Known live paths:
- **Linux / Lima dev box:** `~/.local/bin/qd` (canonical; the `~/.bun/bin/qd` symlink and the TS
  engine were deleted 2026-06-09 — see ADR 0016).
- **macOS (legacy):** `~/.bun/bin/qd`, with `~/.local/bin/qdr` as a canonical rust copy.

## Steps (machine-agnostic)

> **`qd` does not deploy alone.** Since the qd/qw split, `qd` spawns a sibling
> `qw` binary to do the session work, resolving it as a sibling of its OWN path
> and never via `PATH` (ADR-0020). So a deploy that replaces `qd` and not `qw`
> leaves a new `qd` next to an old `qw`, and the wire's version handshake will
> refuse — loudly, by design, which is better than the silent skew it prevents,
> but it is still a broken machine. **Deploy the pair, in the same directory,
> from the same build.** Step 5b below is not optional.

```sh
# 1. Build release from the merged main — BOTH binaries, one commit.
cd <repo> && git checkout main && git merge --ff-only origin/main
./scripts/build-lock.sh cargo build --release -p quorum-dispatch -p quorum-qw
#   -> target/release/qd  and  target/release/qw

# 2. Resolve the live path and sanity-check the fresh binary against the LIVE fleet.
LIVE="$(command -v qd)"
target/release/qd ls                    # expect the new behavior + your real sessions

# 3. Back up the current live binary (rollback).
cp -p "$LIVE" "$LIVE.bak-$(date +%Y%m%d)-<what>"

# 4. Stage to a FRESH (unmapped) path beside the live one, then (macOS only) re-sign.
cp target/release/qd "$LIVE.new"
case "$(uname)" in Darwin) codesign --force --sign - "$LIVE.new" ;; esac
"$LIVE.new" ls >/dev/null && echo OK

# 5. GATED atomic install (NOT a bare mv/cp — see "Why NOT cp" and the deploy gate below).
# Refuses to install a debug-profile binary, and refuses a binary whose `ls` takes longer
# than the budget against your real, live ledger (read-only; ls never mutates). Only on
# success does it `mv -f "$LIVE.new" "$LIVE"`.
./scripts/deploy-gate.sh "$LIVE.new" "$LIVE" --smoke-args ls --budget-secs 2

# 5b. The sibling qw, into the SAME directory (ADR-0020). Same gate, same script:
# it is generic by argument, and `qw build-profile` is both the profile check and
# the smoke verb — qw's other verbs (`serve`, `attach`) are wire endpoints that
# block on stdin, so there is nothing else to time. Half a gate, honestly labelled,
# on the binary that now runs the session work the 2026-07-07 outage was about.
QW="$(dirname "$LIVE")/qw"
cp target/release/qw "$QW.new"
case "$(uname)" in Darwin) codesign --force --sign - "$QW.new" ;; esac
./scripts/deploy-gate.sh "$QW.new" "$QW" --smoke-args build-profile --budget-secs 2

# macOS legacy only: keep ~/.local/bin/qdr in sync as the rollback-source rust binary.
case "$(uname)" in Darwin) cp target/release/qd ~/.local/bin/qdr && codesign --force --sign - ~/.local/bin/qdr ;; esac

# 6. Verify — including that the pair is a pair.
qd --version && sha256sum "$LIVE" 2>/dev/null | cut -c1-16 || shasum -a 256 "$LIVE" | cut -c1-16
ls -l "$(dirname "$LIVE")/qw"    # must exist, same mtime-ish as qd, same build
qd ls >/dev/null && echo "lane opens OK"   # this is what actually exercises qd -> qw
```

## The deploy gate: `scripts/deploy-gate.sh`

Step 5 does not `mv` directly — it hands the staged binary and the live path to
`scripts/deploy-gate.sh`, which performs two independent checks and only then does the atomic
install itself (so there is no code path in this doc that installs a binary without passing both):

1. **Build-profile check.** Runs `<staged-binary> build-profile` — a hidden verb (`qd
   build-profile`, dispatched pre-clap like `qrmux-server`/`relay:serve`) that prints `release` or
   `debug` via `cfg!(debug_assertions)`. Anything other than exactly `release` refuses the install.
   This is the exact 2026-07-07 cause, caught mechanically instead of by doc reminder.
2. **Startup-latency smoke check.** Times `<staged-binary> <smoke-args>` (here, `ls`) and refuses
   the install if it exceeds `--budget-secs` (default 2s; override per call). Run it against your
   real HOME (the default — `ls` is read-only) or `--fixture-home DIR` for a deterministic CI/dev
   run. **This is the general-case gate**: it would have caught the 2026-07-07 regression even if
   the cause had been something other than a debug build, because it measures the actual behavior
   at real ledger scale rather than trusting the build profile alone. A small synthetic fixture
   would not have caught this class of bug — the pre-existing small-fixture unit test suite passed
   throughout the incident; the gate must run against real (or realistically-sized) data.

Either check failing leaves `$LIVE` untouched and exits nonzero (`1` = debug binary, `2` = over
budget) — see the script's header comment for exact usage and exit codes.

## The other install paths (round 3 of this postmortem)

This gate covers hand-run `qd`/`qf` deploys via this doc. It does not cover every path a binary can
reach `~/.quorum/bin` by — an exhaustive audit found two more:

- `qrm bootstrap`/`qrm bootstrap --fresh`/`qrm update` place `qd`/`qf` through
  `qrm::verbs::place_colocated_binary`, now gated in Rust with the same two checks (ported, not
  shelled out — see `qrm/src/deploy_checks.rs`).
- `install.sh` and this repo's README "Manual build" section both do a raw shell `install -m 0755`
  that nothing in Rust can pre-gate — but both end by running `qrm doctor`, which now checks the
  LIVE installed `~/.quorum/bin/{qd,qf}` for build-profile + real-scale latency and FAILs loud on
  either. This is the universal post-install backstop for any path that can't be pre-gated in code.

`qw` (ADR-0020) rides the same three paths, with the profile half of the gate:
`qrm`'s `place_colocated_binary` places it beside `qd` and refuses a debug build;
`install.sh` builds and installs it alongside; and `qrm doctor` adds a **`sibling`**
check — `qd` installed *without* `qw` beside it is a FAIL, not a warning, because
that `qd` cannot open a lane at all. `qw` is deliberately absent from the `resolve`
checks: it is not on `PATH` and must not be.

## Why NOT `cp` straight over the live binary

`cp` writes **in place** into the live, possibly memory-mapped inode. On macOS the OS then SIGKILLs
every new invocation ("Killed: 9", exit 137; `qd ls` silently prints nothing); on Linux an
overwrite of a running binary's inode can likewise corrupt in-flight execs. An atomic `mv` swaps
the inode instead: running processes keep the old image, new invocations get the new one, neither
is corrupted. On macOS, `codesign --force --sign -` gives the copied arm64 image a valid ad-hoc
signature (and changes its sha — expected); Linux needs no signing step.

## Rollback

`mv` a kept backup (e.g. `"$LIVE.bak-YYYYMMDD-*"`) back over the live path. **Roll
back `qw` with it** — the two are version-locked at runtime, so rolling back `qd`
alone reproduces the skew the handshake refuses on. Keep a `qw.bak-*` beside the
`qd` one for exactly this. The deeper cutover
rollback (the retired TS engine) is in the workspace runbook `exec/stage2-rollback-runbook.md`.

## Relay registration is a separate, per-machine step

Deploying the binary does NOT wire up cross-session relay messaging. Claude Code loads MCP servers
from its own user-scope config (`~/.claude.json` → `mcpServers`, written by `claude mcp add -s
user`), so the relay must be registered there pointing at the deployed `qd`:

```sh
claude mcp add -s user relay -- "$(command -v qd)" relay:serve
claude mcp get relay        # expect: Status ✔ Connected
```

Verify it actually serves: a session started after registration writes a sidecar to
`~/.claude/relay/<pid>.json` and listens on `127.0.0.1:8900+`; `qd send:relay <session> <msg>`
returns a message id and the target goes busy.

> NOTE (2026-06-10): `qd bootstrap` / `qd relay:repoint` currently write the relay entry to
> `~/.claude/.mcp.json`, which **this Claude Code version (2.1.170) does not read** — the
> `~/.claude.json` user scope above is the load-bearing one. Until the engine retargets its
> registration (tracked separately), register the relay with `claude mcp add` as shown.
