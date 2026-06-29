# Deploying `qd` to the live binary

**Merging to `main` does NOT ship anything.** The `qd` users run is a plain copy of a release
build, placed by hand on `PATH`. There is no CI/post-merge deploy step. After the 2026-06-06
cutover, a week of merged fixes sat unshipped because nobody rebuilt the binary — the narrow `ls`,
cursor fix, faster `qd new`, token counts, all on `main` but invisible to users. If you merged a
fix and the user "still sees the old behavior," check the deployed binary's age against the merge
first.

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

```sh
# 1. Build release from the merged main.
cd <repo> && git checkout main && git merge --ff-only origin/main
./scripts/build-lock.sh cargo build --release -p quorum-dispatch     # -> target/release/qd

# 2. Resolve the live path and sanity-check the fresh binary against the LIVE fleet.
LIVE="$(command -v qd)"
target/release/qd ls                    # expect the new behavior + your real sessions

# 3. Back up the current live binary (rollback).
cp -p "$LIVE" "$LIVE.bak-$(date +%Y%m%d)-<what>"

# 4. Stage to a FRESH (unmapped) path beside the live one, then (macOS only) re-sign.
cp target/release/qd "$LIVE.new"
case "$(uname)" in Darwin) codesign --force --sign - "$LIVE.new" ;; esac
"$LIVE.new" ls >/dev/null && echo OK

# 5. ATOMIC rename into place (NOT cp — see below). Refresh any canonical rust copy too.
mv -f "$LIVE.new" "$LIVE"
# macOS legacy only: keep ~/.local/bin/qdr in sync as the rollback-source rust binary.
case "$(uname)" in Darwin) cp target/release/qd ~/.local/bin/qdr && codesign --force --sign - ~/.local/bin/qdr ;; esac

# 6. Verify.
qd --version && sha256sum "$LIVE" 2>/dev/null | cut -c1-16 || shasum -a 256 "$LIVE" | cut -c1-16
```

## Why NOT `cp` straight over the live binary

`cp` writes **in place** into the live, possibly memory-mapped inode. On macOS the OS then SIGKILLs
every new invocation ("Killed: 9", exit 137; `qd ls` silently prints nothing); on Linux an
overwrite of a running binary's inode can likewise corrupt in-flight execs. An atomic `mv` swaps
the inode instead: running processes keep the old image, new invocations get the new one, neither
is corrupted. On macOS, `codesign --force --sign -` gives the copied arm64 image a valid ad-hoc
signature (and changes its sha — expected); Linux needs no signing step.

## Rollback

`mv` a kept backup (e.g. `"$LIVE.bak-YYYYMMDD-*"`) back over the live path. The deeper cutover
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
