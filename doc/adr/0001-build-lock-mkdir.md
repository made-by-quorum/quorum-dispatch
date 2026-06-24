# ADR 0001: build-lock uses an atomic mkdir mutex, not flock

**Status:** Accepted
**Date:** 2026-06-04

## Context

The build mutex (`scripts/build-lock.sh`) must serialize concurrent cargo
build/test invocations and recover from dead lock holders. The original Phase 0a
brief specified `flock -x -w 300 ~/.sb-rust/build.lock`.

`flock(1)` does **not** exist on macOS, and the project's CI and primary dev host
both run on macOS arm64 (CI matrix: `macos-latest` arm64 + `ubuntu-latest` x86_64).
A single locking implementation must therefore work identically on both platforms.

Options considered:
- `flock` — not portable to macOS. Rejected.
- macOS `lockf(1)` / `shlock(1)` — exist on macOS but differ from / are absent on
  Linux; would require platform branching. Rejected.
- `mkdir`-based lock — `mkdir` is atomic on every POSIX filesystem, available on
  both platforms, and makes dead-holder detection trivial (read PID from a metadata
  file inside the lock dir, `kill -0` it).

## Decision

Implement the lock as an atomic `mkdir` of a lock **directory**
(`$SB_RUST_LOCK_DIR/build.lock`, default base `~/.sb-rust`). Inside it, write an
`owner` metadata file with `owner`, `pid`, `timestamp`. Acquisition loops on
`mkdir`; on contention it reads the holder PID and, if that PID is dead, reclaims
the stale lock; otherwise it waits up to a 300s timeout (`SB_RUST_LOCK_TIMEOUT`).
Release is a `trap`-driven `rm -rf` guarded so only the owner deletes.

The semantics the brief required are preserved: mutual exclusion, bounded 300s
wait, owner+PID+timestamp metadata, and stale-lock recovery for dead holders. The
lock base is overridable via `SB_RUST_LOCK_DIR` so tests are hermetic and never
touch a real `~/.sb-rust`.

## Consequences

- Portable: one script, no platform branching, runs on macOS arm64 and Linux x86_64.
- Stale recovery is robust: a crashed holder is detected via `kill -0` and reclaimed,
  so an overnight run cannot wedge on a dead PID.
- Tradeoff: PID-liveness reclaim can theoretically race if PIDs are reused extremely
  fast, but the window is negligible for build serialization and acceptable here.
- Divergence from the literal brief text (`flock`) is intentional and documented here.
