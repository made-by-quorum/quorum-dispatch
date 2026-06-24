# ADR 0012 — Verify-after-submit: chunked-payload read-back closes the silent mid-truncation residual

- **Status:** accepted (wart-wave; Pete-sanctioned via ADD-15 W8, 2026-06-05 19:47 EDT)
- **Date:** 2026-06-05
- **Deciders:** Pete (design sanction), sb-rust-orc-6 (wave GO), sbr-wart-lead
- **Design source:** the M11 joint-sanction proposal
  (`exec/a7-evidence/a4-r1-truncation-closure-proposal.md` in the phase workspace),
  post wave-spec red-team R1/R3/R5/R8 revisions.

## Context

After the A4/0b-strength chunked-delivery work, large-paste WHOLESALE drop is dead,
but a narrow residual survived in BOTH engines (divergence table D16): under a
sustained (~600ms+) PTY reader stall mid-chunked-submit, the writer's 150ms chunk
pacing outruns the stalled reader, the tty queue saturates, and mid-payload bytes
drop SILENTLY — submit reports success because acceptance keys on went-busy, not
payload-content arrival. A4 carried it as the R1 residual ("data-loss residual"
label on D16); A7 routed the closure proposal to Pete; ADD-15 W8 sanctioned it.

## Decision

A bounded **post-delivery payload read-back**, applied EXACTLY when the delivery
was chunked — the trigger is the PRODUCTION splitter's verdict
(`chunk_text(message, CHUNK_BYTES).len() > 1`, `submit.rs::payload_needs_verify`),
never a byte-count re-derivation (1024/1025-byte and multibyte seams are pinned by
unit rows). Single-chunk submits keep today's behavior byte-for-byte.

Mechanism (`submit.rs::verify_chunked_payload`, pure over `VerifyDeps`): poll the
session transcript past the PRE-DELIVERY offset every 500ms for up to 10s
(`VERIFY_POLL_MS`/`VERIFY_TIMEOUT_S`) for the submitted user record.

**Outcome policy — loud-fail ONLY on positive truncation evidence** (the design
enemy is the false fail):

| Outcome | Evidence | Bin behavior |
|---|---|---|
| `Verified` | a user record == message byte-exact (the --wait anchor contract) | unchanged success |
| `Truncated{expected,recorded}` | a record SHORTER than the message sharing its leading `min(64,len)` bytes (covers prefix AND mid-loss shapes; longest candidate) | **loud `payload truncated in delivery: expected N bytes, recorded M` + exit 1** — the EXISTING delivery-failure class, fired AFTER went-busy as a distinct named error. **NO auto-retry** (the truncated turn already reached the model; a blind resend double-submits — the loud failure IS the safe contract) |
| `Unattributable` | records exist, none matches, none carries the signature | one stderr WARN, success unchanged |
| `NoRecord` | reads succeeded, zero records in budget | path-split below |
| `SourceUnavailable` | every read errored | one stderr WARN, success unchanged |

**Path-split** (red-team R1 — the flagship `sb new -p` path must not false-fail):

- **`send:pty` idle path** (transcript resolved + offset snapshotted BEFORE the
  send): STRICT — `NoRecord` is also loud exit-1 (`could not verify payload
  arrival ... within 10s`). Verification runs BEFORE the `--wait` anchor loop
  (a truncated payload fails in ≤10s instead of anchor-timing-out at 120s).
- **`sb new -p`** (session_id is a best-effort registry read at delivery time;
  the transcript may not exist yet): BEST-EFFORT — resolution is re-attempted
  every poll (registry → sessionId → `find_jsonl_path`); `NoRecord`/
  `SourceUnavailable` DEGRADE to one warn. `--resume` sessions snapshot the
  existing transcript length pre-delivery so HISTORY is excluded from the
  evidence window (an old shorter record must not fake a truncation).
- **`send:pty` busy-QUEUE path without `--wait`:** NOT verified — the queue
  drains at an unbounded future point; a bounded read-back cannot attribute.
  WITH `--wait`, the existing JSONL anchor already content-keys it (a truncated
  payload never anchors → loud timeout). This sliver is the NAMED remainder on
  the D16 row (narrow: busy target + multi-chunk payload + no --wait + reader
  stall).

## Exit-code contract impact

ADR-0008's 0/10/1 codes are UNTOUCHED in kind — no new code. The ONE behavior
delta: a chunked submit that went busy can now exit 1 on truncation evidence
where it previously exited 0 silently-with-loss. M11 §4 sanctions exactly this
("reuse the existing delivery-failure class; fires AFTER went-busy as a distinct
named error"). ADR-0008 carries a pointer note.

## Teeth (committed)

- Unit: 12 verify-policy rows in submit.rs (10 `verify_*` policy-matrix rows
  incl. mid-loss, + 2 `needs_verify_*` trigger-seam rows; QA R2 count precision).
- Gate (fakerepl): `w8_red_differential_*` (the silent-loss window is REAL +
  the helper catches it), `w8_negctl_slow_but_complete_*` (no false positive),
  `w8_negctl_foreign_record_*` (degrade, not truncation), `w8_mutation_*`
  (unverified entry stays silent — the read-back is the belt),
  `w8_single_chunk_scope_guard_*` (zero reads under the seam). Harness seams:
  fakerepl `SB_FAKEREPL_STALL_AFTER_BYTES/_MS/_QUEUE_CAP` (saturation injected
  at the model-admission boundary — named simplification, fakerepl README) +
  `SB_FAKEREPL_CONVO_JSONL` + `SB_FAKEREPL_SESSION_ID`.
- End-to-end (the CALL-SITE wiring teeth, QA-lane live row — A4_RUN_LIVE gate,
  like all live rows): `new_went_busy_exit.sh` W8-TRUNC (exit 1 + named error
  after went-busy) + W8-ACCEPT-CHUNKED (intact chunked delivery NOT punished).
  RED-without-wiring differential committed:
  `test/golden/mutation/EVIDENCE-wart-wave-m5.txt` (same legs vs the
  lib-only f18fd69 binary → exactly w8-trunc FAILs, silent exit-0).

## TS coordination

Rust-side-first with a named divergence row (D16 flip + D21). The TS-side
closure proposal sits in Pete's TS inbox (A7 M11 routing); the gate report
records that state. If/when TS adopts, the row reclassifies to parity
(D5/D6 precedent).
