# ADR 0008: `qd new -p` went-busy EXIT CONTRACT — a three-way exit divergence from TS

**Status:** Accepted (A4; plan §A4 HARDENING #3 mandate; spec-red-team R1 ratified)
**Date:** 2026-06-05

## Context

`qd new -p <prompt>` boots a session, then DELIVERS the prompt via the
acceptance-keyed verify-then-CR submit discipline (ADR-class L4; `submit.rs`).
Delivery has three distinguishable end states:

1. the session went **busy** — the turn started, the prompt is running;
2. the session NEVER went busy after full bounded remediation — the prompt may
   be sitting **unsubmitted** in the composer (the paste-burst absorption class,
   FINDING-E1);
3. the session's **PID file vanished** after boot — a registry/infra failure,
   not a submit stall.

TS collapses all three into a SUCCESS exit. `qd new -p` exits 0 whether the
prompt was accepted OR stalled — it only prints a `WARNING` to stderr on a stall
(`qa/hardening@3dd9f1e:src/commands/lifecycle.ts:921-931`), and a vanished PID
file lands in the same `accepted=false` bool as a stall
(`lifecycle.ts:309-311`). That is fine for an interactive human reading the
warning, but **useless to an external composer** — `qb spawn` shells out to
`qd new -p` and must branch on the exit code alone. "Created and running" and
"created but the prompt may be stuck" are operationally different outcomes; a
single exit 0 cannot express the difference.

## Decision

`qd new` with `-p` defines a THREE-WAY exit contract (a Class-4-style deliberate
divergence, ADR 0007 — the hardening IS the deliverable). It is documented in
THREE places that move together: this ADR, `doc/PROTOCOL.md`, and the `qd new
--help` epilogue.

- **0** — session created + ready, and (with `-p`) the prompt was ACCEPTED:
  `DeliverOutcome::Accepted` (the session went busy). Stdout: `Prompt delivered
  to "<name>"`.
- **10** — session created + ready, prompt delivered, PID file READABLE, but the
  session NEVER went busy after full bounded remediation:
  `DeliverOutcome::Stalled`. The session EXISTS — stderr says so + gives attach
  guidance; only the turn-start is unconfirmed. 10 is the new code.
- **1** — everything that already exits 1 (create/boot/I6/Bug-D failures),
  unchanged — **INCLUDING `DeliverOutcome::PidFileMissing`**.

**Why PidFileMissing routes to 1, not 10 (spec-red-team R1):** a vanished PID
file is an INFRA failure — the registry row that `qd ls` / `qd attach` key on is
gone, so the session is not reliably addressable. Routing it to 10 ("created,
prompt maybe unsubmitted, go attach it") would tell `qb spawn` to attach a
session whose registry row no longer exists — a lie. It stays in the generic
failure bucket, with a stderr that distinguishes it from a true stall.

**Collision table (re-derived from the actual bin, R2; attribution corrected per
in-phase red-team #3):** live exit codes today are 0 (success), 1 (errors incl.
clap parse errors re-mapped to commander's exit 1 by `cli::map_clap_error`), 2
(the HAND-PARSED `config`/`survey` path — `survey` parse error and unknown
subcommand exit 2, plus `config` usage errors; these bypass clap's re-mapping, so
clap's default exit-2 convention survives — `bin/qd/main.rs:16-17` documents this,
`bin/qd/config.rs` asserts the config-usage 2s), 3 (`ping` no-arg validation,
`stubs.rs`; ping reserves 0-4). **10 is clear of all of them.** Without `-p`, 10
is unreachable (delivery never runs). `--model`'s fire-and-forget send does not
participate in the exit code.

The three-way split lives in the library as `submit::DeliverOutcome`
(`Accepted | Stalled | PidFileMissing`); the bin's `map_deliver_outcome`
(`verbs/lifecycle.rs`) is the single mapping point, unit-tested all three ways.

## Consequences

- External composition (`qb spawn`, scripts) can branch on the exit code alone:
  `0` = go, `10` = made-but-verify, `1` = failed. Documented in
  `doc/PROTOCOL.md` so the contract is discoverable without reading the ADR.
- A divergence from TS's always-0: carried as a NAMED row in the A4 matrix and
  exercised by the `new_went_busy_exit.sh` golden scenario (M4 Level 2: accepted
  config → 0, stalled config → 10, deterministic, two runs each).
- If a future verb wants a distinct non-zero code, it must avoid 10 (and the
  0-4 ping band) or extend this table explicitly.

## Wart-wave note (2026-06-05, ADR-0012 pointer)

The 0/10/1 codes are unchanged, but a CHUNKED `-p`/`send:pty` submit that went
busy can now exit **1** with the named `payload truncated in delivery` error
when the post-submit read-back finds positive truncation evidence — fired AFTER
acceptance, in the existing exit-1 failure class (no new code). Sanctioned:
ADD-15 W8 / M11. Mechanism + policy: ADR-0012. The `new_went_busy_exit.sh`
W8-TRUNC/W8-ACCEPT-CHUNKED legs pin it.
