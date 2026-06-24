# ADR 0006: CLAUDE_FLAGS as config (dangerous flags are not hardcoded)

**Status:** Accepted (A2 hardening #4b; LESSONS L9 carrier)
**Date:** 2026-06-04

## Context

TS hardcodes `CLAUDE_FLAGS = ["--dangerously-skip-permissions",
"--dangerously-load-development-channels", "server:relay"]` (`utils.ts:186`).
Every `sb new` session therefore boots with permission checks bypassed and
development channels loaded, with no way to vary or disable this per machine,
per deployment, or per security posture short of editing source. Two of these
flags are prefixed `--dangerously-` by their own vendor.

Verified 2026-06-04 (A2): `--dangerously-load-development-channels` exists in
claude 2.1.163 (hidden from `--help`), takes `<servers...>` — `server:relay` is
its ARGUMENT — and is what loads `~/.claude/channels/` (cc-relay). The relay that
A4 consumes depends on it, so it cannot simply be dropped.

## Decision

`launch::claude_flags()` resolves flags with precedence:

1. `SB_CLAUDE_FLAGS` env var (whitespace-split) — per-invocation override.
2. `claude_flags` key in `~/.sb/config.toml` (the file `sb config` owns) —
   per-machine policy.
3. Built-in default = the TS triple, verbatim — PARITY by default.

Resolution is permissive (L8): a missing file/key falls through; a config read
never fails a launch. The default stays TS-identical so golden parity holds and
the relay keeps working out of the box; the dangerous part is now a CHOICE that
an operator can narrow (e.g. `SB_CLAUDE_FLAGS="--dangerously-skip-permissions"`
for a relay-less jail boot) without forking sb.

## Consequences

- Corpus/golden rows that embed the default flags remain byte-stable.
- Boot tests can run stock (dialog-free) configurations without code changes —
  the A2 gate's stock-boot row uses exactly this seam.
- `sb config` grows documentation for the key; a write-side subcommand surface
  is NOT added in A2 (read-side only).
- A5 bootstrap's consent posture (ADD-5 default-No for the relay driver)
  composes with this: bootstrap can write the narrowed key instead of editing
  source.
