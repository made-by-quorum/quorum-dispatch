# ADR 0017: Register the relay via `claude mcp`, not by writing config files

**Status:** Accepted
**Date:** 2026-06-10
**Supersedes:** the `~/.claude/.mcp.json` registration target of ADR 0016 (the
rest of 0016 — native `relay:serve`, the retired bun driver, the eval-init
shell wrapper — stands).

## Context

ADR 0016 had `sb bootstrap` / `sb relay:repoint` register the relay by
hand-writing the relay entry into `~/.claude/.mcp.json`, per the "orc-38 E1"
ruling that Claude Code loads user-scope MCP servers from that file.

That ruling is **false for Claude Code 2.1.x.** Verified on the live box
(2.1.170): with a well-formed `~/.claude/.mcp.json` relay entry present,
`claude mcp get relay` reports "No MCP server found" and no session ever loads
the relay. Claude Code reads user-scope MCP server definitions from
`~/.claude.json`'s top-level `mcpServers` (written by `claude mcp add -s user`),
NOT from `~/.claude/.mcp.json`. So the relay had been silently non-functional,
while `sb bootstrap` cheerfully reported "relay: configured" by reading the same
file Claude Code ignores — a false green.

Settings files are not an option either: `settings.json` does not DEFINE MCP
servers (it only enables/permissions them via `enabledMcpjsonServers` etc.). MCP
server definitions live only in `.mcp.json` (project) and `~/.claude.json`
(user) — and that location/format is Claude Code's to change, as this incident
proves it does across versions.

## Decision

**Register the relay through Claude Code's own `claude mcp` CLI; never
hand-write Claude Code's config.** The engine owns the *intent* ("register the
relay, pointing at this binary, at user scope"); Claude Code owns *where and how*
that is stored. This is version-robust by construction — whatever file/format a
given Claude Code uses, `claude mcp` writes it.

- **`crate::relay_server::register`** (new; replaces `relay_server::migrate`)
  drives three operations through the `Exec` seam:
  - `relay_is_registered` → `claude mcp get relay` (exit 0 = registered),
  - `register_relay(exe)` → `claude mcp remove -s user relay` (idempotent
    no-op when absent) **then** `claude mcp add -s user relay -- <exe>
    relay:serve`. The remove-then-add guarantees the command is REPOINTED to
    `<exe>` (a bare `add` refuses to overwrite an existing entry),
  - `unregister_relay` → `claude mcp remove -s user relay` (rollback).
- **`current_exe()`** is the registered command (the binary you run is the one
  registered) — unchanged from 0016.
- **`sb relay:register`** is the primary verb name (clearer than "repoint",
  which never fit first-time setup); **`relay:repoint`** stays as a hidden
  back-compat alias. **`sb relay:rollback`** → `unregister_relay`.
- **Bootstrap** gains a `claude`-on-PATH precondition (we drive `claude`): when
  absent, the relay step is a NOTICE with a manual `sb relay:register` pointer,
  never a prompt or a failure — exactly the zmx-notice discipline. Detection is
  `relay_is_registered`; the offer (TTY-only, default No) calls `register_relay`.
  Runtime health (sidecar discovery) stays an FYI line, orthogonal to whether
  NEW sessions will load the relay (that's registration).
- **Retired:** the entire hand-rolled `migrate` module — `repoint_merge` /
  `rollback_merge` / `apply_repoint` / `apply_rollback` / `classify_driver` /
  `RelayDriver` / the `.mcp.json` / `.bun-backup` / plugin-cache / channels path
  helpers and their `RelayConfigState` driver classification in bootstrap. The
  relay no longer reasons about config-file shapes at all.

## Consequences

- The relay actually works: `claude mcp get relay` → Connected, a fresh session
  writes a sidecar and listens, `sb send:relay` delivers (verified live).
- No more false greens: bootstrap's "configured" line reflects `claude mcp get`,
  the same source of truth Claude Code uses.
- Survives Claude Code config-format/location changes for free — the failure
  mode that caused this ADR cannot recur, because we no longer encode CC's
  storage anywhere.
- New dependency: registration requires `claude` on PATH. That is always true
  where it matters (the relay is FOR Claude Code sessions); bootstrap degrades
  to a notice otherwise.
- The golden gate (`bootstrap_output_audit.sh`) STUBS `claude` in-jail (a script
  emulating `mcp get|add|remove` against a state file), so the arms stay
  hermetic and assert the engine drove `claude mcp add -s user relay -- <binary
  under test> relay:serve` — plus a new claude-missing precondition arm.
- `relay:rollback` no longer restores a `.bun-backup` (there is none); it is a
  plain `claude mcp remove`. The bun-migration path is fully gone (the bun
  driver was deleted 2026-06-09).
- Trade-off accepted: shelling out per operation (vs. one file write) costs a
  few `claude` process spawns at bootstrap/register time. Bounded by a 20s
  per-call timeout (no-hang discipline); negligible in practice.
