# ADR 0013: Embedded-mux state-dir compatibility + migration (SB_MUX flip)

**Status:** Accepted (C1; orc-6 checkpoint riders R-A/R-B/R-D folded; Pete gate at C1 close)
**Date:** 2026-06-05

## Context

C1 flips the engine's `Mux` backend to embedded qrmux behind `SB_MUX` (default `embedded`;
`SB_MUX=zmx` = the escape hatch, test-carried). Pre-flip sessions live in the zmx world
(sockets under zmx dirs; registry rows written by Claude Code). ADD-14 (Pete): the engine
never WRITES literal /tmp. Rule 9: Rust sb never touches real state until C2 — this ADR
RULES the migration story; C2 executes it.

## Decisions

### 1. Whole-world backend rule (no hybrid in C1)

`SB_MUX` selects the WHOLE universe: the zmx lane sees/operates all zmx sessions (incl.
every pre-flip session); the embedded lane sees/operates embedded sessions. There is NO
cross-backend listing, rendering, or proxying in C1 — under embedded, registry rows whose
sessions live in the zmx world surface exactly as cold/non-mux-live rows do today (zero
`ls --json` shape change; the `zmxClients` key name stays byte-stable). Any cross-backend
visibility feature is a C2 decision WITH Pete (a `--json` contract surface, ADD-13(4)).

### 2. Embedded state dir (ADD-14-compliant)

Engine resolution `resolve_qrmux_dir`: `$XDG_RUNTIME_DIR/qrmux` else `<sbHome>/mux` where
`sbHome = SB_HOME || $HOME/.sb` (the engine's `SbPaths::from_home_env` seam). NO /tmp tier.
sun_path-length guard at resolve with a named remedy (set XDG_RUNTIME_DIR or shorten
SB_HOME). The engine-resolved dir is the single source of truth: passed per-call into the
qrmux client ops AND propagated to the daemon via `server --socket-dir` argv — daemon binds
exactly where the engine reads (Bug-D keystone, asserted in `embedded_mux_live` +
gate row G-CRUD).

**Standalone qrmux CLI fallback (checkpoint rider R-B, ruled):** ADD-14 extends to every
shipped binary — qrmux's own `socket.rs` fallback changed from `/tmp/qrmux-{uid}` to
`$XDG_RUNTIME_DIR/qrmux` else `<sbHome>/mux`, **honoring SB_HOME** (implementer choice,
ratified here): engine and standalone agree fully; a relocated SB_HOME moves the mux dir
with it; SB_HOME-only jails stay hermetic. D-SOCKDIR is therefore a NON-divergence record.

### 3. Backend stamping deferred to C2/A6

`registry.backend` exists but stays UNWRITTEN in C1: the engine does not own the live
registry row (Claude Code writes `<pid>.json` post-boot; engine writes only kill
tombstones), so there is no honest write seam — a pre-boot stub row would race Claude's
write. Lane membership in C1 derives from which mux's world contains the session (already
backend-scoped by decision 1). **C2 carry:** stamp `backend` when the marks/lineage flow
(A6/sbx) owns the write, or at cutover migration.

### 4. Migration story (C2 executes)

At cutover: pre-existing zmx sessions remain fully operable via `SB_MUX=zmx` for the
TS-compat window; new sessions default embedded. The engine's literal-/tmp surface is
READ-ONLY from C1 on (A14-2, orc-6-ruled): legacy /tmp scan survives for zmx-lane
visibility + migration discovery, but **destructive targets are never sourced from /tmp
enumeration alone — /tmp scan output is visibility-only**; strays surfaced by scan are
REPORTED, never auto-killed (an explicit named per-target user command converts them to
the allowed class). ADD-12 belts (Lima-only live destructive sweeps; dry-run sweep belt)
stand for whatever destructive surface survives. The test-lane scan-root override
(`TEST_SCAN_ROOTS`, A14-2(c)) governs the READ scan in jails; production read default
unchanged.

### 5. Named divergences introduced by the flip (gate-report table carries the full set)

- **D-LISTRAW:** embedded `list_raw` never surfaces ended sessions (qrmux sessions vanish
  on end) — reconcile's reap input differs by construction; embedded sessions end clean,
  so the reap path is a zmx-ism. Works-well assessed in the gate report.
- **D-RESUME:** qrmux has no inline-command attach; embedded resume = `run_detached` THEN
  `attach` (vs zmx's single create-or-attach spawn). Same observable outcome; window
  between create and attach is benign (detached session runs regardless).
- **GetHistory composition:** scrollback + visible screen (content-inspection op;
  intentionally differs from attach-replay's empty-during-alt — boot answerer must see
  dialogs under fullscreen apps). Documented in PROTOCOL.md.
- **D17 (--model passthrough):** assessed KEEP (engine content-free; claude rejects
  unknown models loudly) — Pete-visible at the C1 gate, not silently lead-ruled.
- **Hidden `qrmux-server` subcommand (M4fix):** the sb binary is also the embedded daemon
  (single-binary; pre-clap dispatch so `--help`/exit surfaces are structurally untouched).
  The embedded launcher passes `ServerLaunchSpec { current_exe(), ["qrmux-server"] }`;
  standalone qrmux keeps `current_exe() server`. Added after the Lima-lane cold-start find
  (G-COLDSTART gate row + mutation control carry the anti-regression).

## Consequences

- Pete's escape hatch is a one-variable rollback (`SB_MUX=zmx`) proven by gate row G-E
  against the real zmx binary.
- No real-state migration risk in C1 (rule 9 intact); C2 inherits decisions 3+4 as named
  carries.
- Protocol v2 skew window: production has no pre-existing qrmux daemons; dev/test v1
  daemons surface a named "stale qrmux daemon at <dir>; restart it" refusal — never
  auto-killed.

## WS-C note (2026-06-06, ADR-0014)

The per-session daemon split (ADR-0014) changes the TOPOLOGY (one daemon per session,
`<dir>/<name>.sock`) but NOT this ADR's dir-resolution contract: the two-tier
XDG/sbHome resolution, SB_HOME honoring, ADD-14 no-/tmp-writes, and the whole-world
backend rule are all unmodified. The v2 skew-window note above generalizes per-session
("stale qrmux daemon for session '<name>' at <dir>; kill or restart THAT session").
