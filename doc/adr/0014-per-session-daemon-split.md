# ADR-0014: Per-session daemon split (WS-C)

Date: 2026-06-06
Status: accepted (WS-A ruling exec/wsa-ruling.md, orc-7, ADD-16a authority; spec
exec/wsc-spec.md rev C, orc-8 GO + riders; implemented M1–M5 on phase/wsc-split-spec)

## Context

The as-built qrmux topology was ONE daemon per socket-dir owning ALL sessions' PTY
masters — a silent inheritance from the retach 0.8 fork base that contradicted B2's own
spec text ("one session = one daemon", b2-spec.md:101; LESSONS L22). Its single measured
liability was TOTAL blast radius: SIGKILL of the one daemon killed every session's PTY
child at once (divergence D27, G-DAEMONKILL empirical), and one-daemon RSS grows
additively, making it a preferred OOM target exactly when the fleet is busiest.
Supervision cannot fix this — PTY masters die with the process.

## Decision

One server process per SESSION (the zmx-parity shape, and the shape B2 actually
specced):

- **Topology:** `qd qrmux-server --socket-dir <dir> --session <name>` (--session
  REQUIRED; the per-dir multi-session mode is retired). Capacity-1 enforced by
  identity: every session-addressed verb checks `name == self.session`.
- **Naming (no third resolution scheme):** dir resolution is UNCHANGED (two-tier
  XDG/sbHome, ADR-0013). Leaves: `<dir>/<name>.sock` / `<name>.lock` / `<name>.log`.
  Injective name→leaf mapping (refuse-don't-escape; reserved: `qrmux`, leading `.`);
  dynamic sun_path budget with remedy-naming errors (zmx precedent).
- **Protocol v3 + capability exchange:** preamble byte 0x03; `ClientMsg::Hello {caps}` /
  `ServerMsg::Hello {caps, session}` APPENDED (Error variant index 4 stays frozen);
  Hello-first normative on every connection; ServerHello.session = client-side identity
  belt. Versioning rule amended: breaking-only bumps; additive evolution by capability.
  Breaking-bump refusals are now PER-SESSION (restart one session, not the fleet).
- **Lifecycle:** per-session flock held across spawn (launch serialization AND name
  uniqueness); liveness = preamble+Hello handshake, not socket existence; four launcher
  states (Up/Retiring/Crashed/Absent) with ECONNREFUSED-only unlink eligibility;
  claim-timeout (30s, reset by session-addressed verbs only) reaps unclaimed daemons;
  exit-on-session-end with content-first/close-last ordering and unlink-before-exit.
- **Discovery:** engine dir-scan of `*.sock` + per-socket Hello+ListSessions probe; a
  row surfaces IFF the daemon reports ≥1 session; ConnectionRefused-only stale cleanup
  (zmx busy-daemon rule); D-LISTRAW preserved by construction.
- **Mixed-state:** a live pre-split `qrmux.sock` daemon gets ONE best-effort stderr
  warning — visibility, NEVER auto-kill.

## Consequences

- D27 flips to zmx-parity: daemon death is one session's death (G-ISOL: positive
  isolation control + shared-fate negative control via the test-only
  QRMUX_TEST_SHARED seam).
- Cold-start race class multiplies → per-session G-COLDSTART-N arms (same-session
  race, cross-session burst, claim-timeout, create-vs-teardown, teardown-grace rows).
- Measured (release build, gate evidence crates/qd/tests/c1-gate-evidence/wsc-m5/):
  data plane p95 0.88–0.95× of the shared baseline under saturating load (split is
  FASTER), zero timeouts; idle rent ~2.2MB/daemon flat through N=20 (Σ44MB) — the
  accept-the-rent disposition, measurement-vindicated. macOS RSS double-counts shared
  text; PSS-honest numbers are a named Lima-lane follow-up.
- L24's premise INVERTS: per-session env now exists (each daemon its own env) — e.g.
  fault-injection can arm per-daemon.
- Test-harness exposure (LESSONS): every verb can cold-start a daemon, so
  captured-output harness calls must not block on pipe EOF; jailed daemons outlive
  SIGTERM'd runners (jail-lifecycle belts are the fix, not daemon self-watch — a
  daemon surviving its creator is correct production behavior).

## Cross-references

ADR-0013 (state-dir contract — unmodified, see its WS-C note); crates/qrmux/PROTOCOL.md
v3 sections; exec/wsc-spec.md (rev C + §14 riders); exec/wsa-ruling.md + Amendment 1;
exec/wsa-daemon-memo.md; divergence table D27/D-LISTRAW/D-SOCKDIR rows.
