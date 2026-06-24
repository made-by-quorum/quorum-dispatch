# ADR 0019: Agent-pid identity token on `session-opened`

**Status:** Accepted
**Date:** 2026-06-21

## Context

bond derives session liveness ("is this obligation's respondent alive?") by
checking the `session-opened.pid` recorded in dispatch's mux log. Today that check
is a bare `kill(pid, 0)` with **no process-start-time check** — the delivery-side
dead-writer rule self-documents the hole (*"pid-reuse makes a recycled pid look
alive … no process-start-time check in v1"*, `crates/dispatch/src/events.rs`).
**PID recycling makes a reused pid read false-LIVE.**

SPEC-v2 §5.A / R2-8 / P1 calls for a **process-identity token** that survives pid
reuse, carried on `session-opened` in addition to `pid`. P1 was marked
**PARTIALLY-CLOSED**: a robust, non-duplicating, non-soft-state baseline can come
**only** from dispatch recording the start-time on its own event (caching it in bond
would either duplicate a dispatch fact into bond's log — a §1 ownership violation —
or die on the supervisor restart bond's design assumes). The robust closure is
therefore a **cross-track dependency carried to the dispatch track** (#4/#6). This
ADR records building it.

The recorded `session-opened.pid` is the **PTY child = the agent process** (the
responder bond cares about), captured once at spawn (`sbmux events.rs:83-85`,
`session.rs`), **NOT** the sbmux daemon (SPEC-v2 §5.A corrected at `81becc4`, M5).
The token pins the start-time of *whatever pid is on record*; bond re-checks *that
same recorded pid*, so recycle-detection works regardless.

## Decision

Add an **OPTIONAL, additive** agent-pid identity token to the `session-opened`
mux event and **bump `DAEMON_EVENTS_SCHEMA_VERSION` 1 → 2**.

```
pid_start_ms: Option<u64>   // kernel start-time of the recorded pid (the agent/child),
                            //   epoch-MILLISECONDS, ms-floored on both platforms
boot_id:      Option<String> // per-boot-stable OPAQUE id, EXACT string-equality only
```

Both fields use `#[serde(default)]` + `skip_serializing_if = "Option::is_none"`
(additive rule (a); omit-when-None keeps a token-absent line byte-stable with v1).
The value contract (`crates/sbmux/src/procid.rs`):

- **Darwin (macOS 10.12+):** `pid_start_ms` via libproc `proc_pidinfo`
  (`pidinfo::<BSDInfo>(pid, 0)` → `pbi_start_tvsec*1000 + pbi_start_tvusec/1000`),
  dependency **`libproc = "=0.14.11"`** (exact-pinned — a compile error is the
  API-drift backstop, R8); `boot_id` via `sysctlbyname("kern.bootsessionuuid")`.
  **Never `kern.boottime`** (re-disciplines under NTP/wake-from-sleep → false
  crash-dead under string-equality). This is the **same libproc call SPEC-v2 §5.A
  names for bond's fallback** → producer (dispatch) and consumer (bond) derive
  bit-identical values by construction.
- **Linux:** `pid_start_ms` from `/proc/<pid>/stat` field 22 (`starttime` ticks) +
  `/proc/stat` `btime` + `sysconf(_SC_CLK_TCK)` (a pure parser split from IO for
  testability); `boot_id` = `/proc/sys/kernel/random/boot_id`.

**Fail-safe everywhere (per field):** `pid_start_ms` is `None` (field omitted) on
any read/parse failure, on `pid==0` (the documented benign no-child case), or on an
unsupported platform; a non-zero-pid read failure emits a `tracing::warn!` **inside
`procid`** (the cause is in scope) while still returning `Option` (no API change).
`boot_id` is **pid-independent** (a per-boot value, not derived from the pid), so it
is `None` only when its source is unreadable — **not** on `pid==0`; on `pid==0` it
may still be present, yielding a **`boot_id`-only half-token** (`pid:0`, `boot_id`
present, `pid_start_ms` omitted). This is intentional — gating `boot_id→None` on
`pid==0` would add a needless special case for zero correctness benefit. The
half-token stays fail-safe because bond treats **any absent `pid_start_ms` as
crash-dead regardless of `boot_id`** (absence / mismatch ⇒ crash-dead, never
false-LIVE).

**Same-box invariant (load-bearing):** the token is always produced and consumed on
the same machine (bond reads dispatch's local `~/.quorum/dispatch/` logs), so producer
and consumer are the same OS by construction and read the value identically.

## Consequences

- **Closes P1 robustly** (the cross-track dependency): once v2 ships, bond prefers
  the token when present and falls back to a direct libproc read when absent
  (pre-token lines / `None` fields); both paths fail-safe to crash-dead. The token
  never changes which pid is recorded, so token and fallback key the same pid.
- **Resolution is honest, ms-FLOORED on BOTH platforms (N1/D1):** ~1 ms on darwin
  (sub-ms `pbi_start_tvusec` floored), ~10 ms tick on linux. The discriminator
  defeats reuse down to that residual, **not** "any same-second reuse." A residual
  false-LIVE needs a pid recycled within the same ms/tick **and** the same `boot_id`
  **and** landing on the same pid — vanishingly unlikely, fail-safe-leaning.
- **Severity is LOW:** this is the §5.A liveness *display* fact (the `⚠`
  annotation), **not** the firing lifecycle — a residual is an advisory display
  error, never ledger corruption.
- **Back-compat:** bidirectional fold proven (a v1 line parses under the v2 reader;
  a v2 line parses under a v1-shaped reader — `deny_unknown_fields` banned). Record
  budget fine (no per-record cap; the token adds ~80 bytes to `session-opened`, the
  first record).
- **Scope held:** the only engine change is sbmux (`procid.rs` + the `session-opened`
  schema/stamp). The firing lifecycle, watcher, DuckDB, relay, `content_preview`
  redactor, the delivery schema, and every transport are **untouched**.
- **Cross-impl contract:** the linux parser's expected vector is published in
  `doc/EVENT-CONTRACT.md` §2.5 so bond's track (#7) asserts the same value.
- **Consumer side (bond, #5/#7) is out of scope here** — documented in
  `doc/EVENT-CONTRACT.md` §2.3/§2.4 for that builder to conform.
- **Naming supersedes SPEC-v2 §5.A (F5).** SPEC-v2 §5.A's illustrative names
  **`pid_start_unix`** + **`sysctl kern.boottime`** PREDATE and are **SUPERSEDED by
  this contract**: the shipped producer emits **`pid_start_ms` (epoch-MILLISECONDS)**
  + **`boot_id` from `kern.bootsessionuuid`** (`kern.boottime` explicitly rejected).
  The build is correct — it follows the accepted plan; this is upstream spec
  staleness, not a build defect. An outside reader (bond #7) **MUST build the
  consumer against `doc/EVENT-CONTRACT.md`** as the authoritative producer contract,
  **NOT §5.A's literal text** — else every token comparison mismatches and the token
  goes silently inert (fail-safe crash-dead). The §5.A spec-text sync is the
  supervisor's cross-track call; SPEC-v2.md is **not** edited by this deliverable.
