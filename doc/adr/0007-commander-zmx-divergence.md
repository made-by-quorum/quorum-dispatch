# ADR 0007: Named divergence classes — CLI (commander.js) and zmx-adapter layers

**Status:** Accepted (A2; brief-named ADR class)
**Date:** 2026-06-04

## Context

The TS qd's outermost layers are commander.js (CLI parsing) and ad-hoc Bun.spawn
calls (zmx driving). Neither is a parity surface worth replicating bug-for-bug,
but "not parity" must be NAMED and bounded, or drift hides in it. This ADR
enumerates the sanctioned divergences for both layers; anything outside these
classes is a parity bug.

## Decision

**Parity surface (unchanged):** for every command, the SUCCESS-path stdout
format, the documented option set and semantics, exit codes (0/1/2 classes), and
the actionable error strings ported from TS (guidance text, Bug-D errors, I6
messages) — these are golden/corpus rows.

**Class 1 — commander.js parsing prose (diverges):** exact `--help` layout,
unknown-flag/missing-arg error phrasing, and commander quirks
(`allowUnknownOption`, excess-argument tolerance) follow the Rust
implementation, not commander.js. Corpus rows MUST NOT assert commander's
usage/error prose. Behavioral contract kept: unknown flags still FAIL (exit 2
class), missing required args still fail, `--` pass-through still reaches
claudeArgs.

**Class 2 — process-exit discipline (diverges, deliberately):** TS calls
`process.exit(1)` deep inside library functions (`assertZmxCapable`,
`startDetached`, `resolveOrDie`). Rust library code returns `Result`; ONLY the
bin maps errors to stderr + exit codes. Same observable exits, different
internal shape — enables the decider/effects testing the whole port relies on.

**Class 3 — zmx process mechanics (diverges):** TS spawns via Bun with a cloned
env (`zmxEnvForDir` merges full `process.env`); Rust's Exec seam passes an
explicit `ZMX_DIR` override onto the inherited env. TS salvages stderr via
exception fields; Rust captures both streams uniformly. Timeouts are explicit
(`zmx --help` 5s). The CONTRACT is identical: every zmx invocation pinned to one
socket dir (L1), missing-dir list → `[]`, kill returns the exit code, ENOENT →
the missing-zmx guidance (never a raw trace).

**Class 4 — create-path hardenings (diverge, they ARE the deliverable):**
`qd new --agent <unresolvable>` fails closed (TS boots a generic session;
prior art spawn.ts:213-230); name uniqueness is enforced by an O_EXCL claim at
create (TS has a check-then-create race); boot is dialog-free per ADR 0005.
`qd new <name>` REJECTS a name containing `/`, `\`, `..`, or NUL at the create
boundary before the claim (A4 §3.6, redteam-retro #4) — TS passes such names
through to zmx; we narrow accepted inputs so distinct raw names can never
collide on a sanitized claim stem and a crafted name can never escape claims_dir.

## Consequences

- 0b corpus recording skips Class-1 prose rows; comparator normalization does
  not try to map usage text.
- Anything not listed here that differs from TS at the CLI or adapter layer is
  a BUG, not an extension of this ADR — extend the ADR explicitly or fix the
  code.
