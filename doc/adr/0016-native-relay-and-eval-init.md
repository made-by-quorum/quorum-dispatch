# ADR 0016: Native relay registration + eval-init shell integration

**Status:** Accepted
**Date:** 2026-06-09

## Context

Two pieces of `sb bootstrap` were still shaped around the retired bun/TS
implementation, and both broke in the field on 2026-06-09:

1. **The relay step offered a dead external installer.** The ADD-5 design
   treated the relay transport as a standalone external driver with its own
   installer (`cc-relay install`, overridable via `SB_RELAY_DRIVER_INSTALL`).
   The native Rust relay (`sb relay:serve`) made that obsolete: the only thing
   "installing the relay" still means is writing the `relay` entry in the
   user-scope `~/.claude/.mcp.json`. On a box without the bun toolchain the
   bootstrap offer shelled out to a nonexistent `cc-relay` and reported
   `driver installer exited 1`. Compounding it, `relay:repoint` HARDCODED the
   target command as `<home>/.bun/bin/sb` — on a machine where that symlink
   still pointed at the dead TS entrypoint, repoint "fixed" the config to a
   binary that does not implement `relay:serve`. And `apply_repoint` ERRORED
   on an absent `.mcp.json` (with a message telling the user to run the dead
   installer), making fresh-machine registration impossible.

2. **The shell wrapper was a fossil by design.** The TS bootstrap WROTE a
   `claude()` wrapper function into `~/.bashrc` between markers. The A5 port
   ruled shell-profile patching out of the engine entirely (§9 item 4), so the
   baked block survived unmanaged — and when the Rust `sb new` changed its
   argument contract (name required, `--attach` for interactive), every baked
   wrapper silently broke (`error: missing required argument 'name'` on a bare
   `claude`).

## Decision

**Relay: bootstrap registers the native relay itself, with consent.**
The external-driver concept is retired: `DEFAULT_RELAY_DRIVER_INSTALL`,
`SB_RELAY_DRIVER_INSTALL`, and the installer exec seam are gone. The bootstrap
relay step now keys off the CONFIG state of `~/.claude/.mcp.json` (driver
classification + does-the-configured-binary-exist), not runtime health (which
remains an FYI line): anything but healthy-native gets a TTY-only, default-No
offer to register, which runs the same merge-preserving `apply_repoint` the
`relay:repoint` verb uses. Two repoint fixes land with it:

- `relay:repoint` (and the bootstrap registration) target
  **`std::env::current_exe()`** — the binary being invoked is by definition
  the deployed one. No more hardcoded `.bun/bin/sb`.
- `apply_repoint` **creates `~/.claude/.mcp.json` when absent** (fresh-machine
  registration); `MigrateError::McpConfigAbsent` is removed.

**Shell: the eval-init pattern, via a new `sb init <shell>` verb.**
`sb init bash|zsh|fish` PRINTS the integration (the `claude` wrapper +
ZMX_DIR pin); the rc file carries one stable line
(`eval "$(sb init bash)"` / zsh variant / `sb init fish | source` in
`~/.config/fish/conf.d/sb.fish`). The wrapper body ships in the binary
(`sb::shell_init`), so it can never drift from the engine's verb contracts —
the starship/zoxide/direnv precedent. Bootstrap's shell step offers (TTY-only,
default No) to append that ONE line to the detected shell's rc file, reports
when it is already present, and DETECTS the retired baked block
(`>>> sb bootstrap >>>` markers) with a remove-it pointer — it never edits
existing rc content.

Wrapper flag seams are deliberately split: the emitted wrapper injects
`SB_CLAUDE_WRAPPER_FLAGS` on passthrough REAL launches only (headless /
non-TTY / inside-zmx); sb-routed launches keep taking flags from the engine
launcher (`SB_CLAUDE_FLAGS` / config / defaults, launch.rs). Reusing
`SB_CLAUDE_FLAGS` for the wrapper would have silently overridden the
launcher's defaults.

**Supersession.** This reverses the A5 §9-item-4 ruling that shell-profile
patching is not engine bootstrap's job: with the TS/sbx deploy layer deleted
(2026-06-09), the wrapper's only owner is the engine, and the wrapper's
content is engine behavior (`sb new` routing). The G-B5 forbidden-token set
drops `profile-patch|profile patch` accordingly; the sbx-vocabulary tokens
(substrate/marketplace/plugins-root/spawn/sbx) stay banned as wording
discipline. The bun/TS implementation (repo, global package, `.bun/bin/sb`
symlink) was deleted the same day; the recorded-TS differential fixtures in
this repo are unaffected (they skip loudly when bun is absent).

## Consequences

- Fresh-machine setup is one consented `sb bootstrap`: state dirs, relay
  registration (writes a valid `.mcp.json` pointing at the running binary),
  and the shell line. Re-runs are no-op reports.
- A stale deploy (config naming a deleted binary) is now DETECTED
  (`ConfiguredDangling`) and re-offerable, instead of silently breaking MCP
  spawn in new sessions.
- `sb relay:repoint` run from a dev build registers THAT dev build
  (current_exe semantics). This is intentional — the report prints the
  command path; deploy discipline (doc/DEPLOY.md) is unchanged.
- The golden gate `bootstrap_output_audit.sh` got STRONGER on the accept arm:
  it asserts the actual registration write (jailed `.mcp.json` contents +
  command path = binary-under-test) and the rc-file line (added once,
  idempotent), instead of a stub-installer sentinel.
- Wrapper bugfixes now ship with the binary: users re-`eval` on new shells
  automatically; nobody re-runs bootstrap to re-bake a block.
- `relay:rollback` semantics are unchanged (bun-backup restore); on a
  fresh-registration there is no backup, and rollback truthfully reports
  nothing to roll back to.
