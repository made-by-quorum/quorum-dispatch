# ADR 0009: idle-path chunked two-write + content-verified-CR delivery — sanctioned divergence + parity port on two TS-identical large-write loss bugs

**Status:** Accepted (A4; orc-2 RULED fix-in-phase, ruling relay-1780631655040-9
item 2; **deliver_prompt extension RATIFIED at the merge ruling**, orc-2
relay-1780634623878-11; W1 slash-command exception ruled at the merge ruling,
relay-1780635056005-12). **AMENDED (A4 post-merge follow-up, orc-2
relay-1780637708238-13 item 2 + relay-1780638045316-14):** adds the
tty-queue-overflow failure mode (a) + the **chunked text phase** (≤1024B
code-point-safe, ~150ms inter-chunk), a PARITY PORT of the new TS reference
`8c59ec4:src/commands/submit.ts`. The W1 single-write exception is now **moot by
construction**.
**Date:** 2026-06-05 (amended 2026-06-05)

## Context

A4's live evidence caught **TWO distinct, size-gated large-write loss modes** on
the PTY delivery paths. Both are now defended; the gate does not model either by
default (which is WHY it stayed green while the live findings surfaced).

### Mode (b) — paste-burst `\r` ABSORPTION (the original R4 finding)

On REAL claude 2.1.163, a **≥~4KB single PTY write of `message + "\r"`** on the
IDLE `send:pty` path is treated as a PASTE BURST — its trailing `\r` is absorbed
as a literal newline, so the text sits **unsubmitted** in the composer and the
session never goes busy. In a **2-boot reproduction** the discipline's one
remediation CR did **NOT** recover it live (the follow-up CR was itself absorbed;
the composer stayed loaded, the JSONL user-record delta stayed 0).

### Mode (a) — tty-queue OVERFLOW / wholesale DROP (the R6 probe finding)

The R6 live probe (`test/golden/dryrun/a4-r6-probe-evidence.md`, ordered orc-2
relay-1780637708238-13) found a SECOND, deeper mode: a single large `zmx send`
overflows the **~4096B canonical tty input queue** BEFORE claude's reader drains
it, so the write is **DROPPED WHOLESALE** — the text **never reaches the composer
at all** (composer EMPTY, JSONL delta 0, did-not-go-busy WARNING). This is
**distinct** from mode (b): in (b) the text sits stuck at the `❯` prompt; in (a)
the marker is **ABSENT** from the composer (EMPTY-DROPPED). The merged two-write
discipline operates ABOVE this transport hole and **cannot remediate it** — the
content-verified CR correctly fires nothing because the composer genuinely holds
nothing.

**OBSERVED boundaries are EVIDENCE, not constants.** On brano (arm64/Darwin) the
send:pty idle path: **8KB DELIVERED, 12KB and 16KB EMPTY-DROPPED**; the create
path's multi-round remediation got 16KB through on one boot (it shares the same
exposed transport). The TS reference observed the drop at **~4KB**. These
boundaries are **MACHINE/LOAD-DEPENDENT** — no code path and no test asserts that
any particular size passes unchunked. The **INVARIANT is the chunk size** (≤1024B,
well under any realistic queue bound).

Evidence ladder (mode a): brano 8KB clean / 12KB+16KB EMPTY-DROPPED
(`a4-r6-probe-evidence.md`); TS-side ~4KB single-write drop (verified before the
port). The fix is the same on both: chunk the text into ≤1024B code-point-safe
pieces ~150ms apart so the reader drains the queue between writes. **TS landed the
reference (`8c59ec4:src/commands/submit.ts` `chunkText` + `sendTextChunked`); the
Rust change is a PARITY PORT of it, NOT a divergence.**

Evidence (all in-repo):
- `test/golden/dryrun/a4-live-evidence.md` §FINDING — the per-size table (1.1KB →
  SUBMITTED; 4.2KB / 4.5KB → NOT submitted, "did not go busy" WARNING; recovery
  probe: "still stuck after manual CR"), reproduced in boots #4 and #5.
- `test/golden/dryrun/a4-paste-bytes.txt` — the captured stuck-composer bytes
  (the message visible at the `❯` prompt after the turn ended, delta=0).
- soak-ledger **R4 row** (`exec/log/2026-06-05-a4.md`) — root-cause + disposition.

The IDLE `send:pty` path (`crates/qd/src/bin/qd/verbs/send.rs`,
`SendPtyAction::SendVerify`) and the `qd new -p` create path
(`crates/qd/src/submit.rs::deliver_prompt`) BOTH delivered via a single
`message + "\r"` write — a faithful port of the TS reference idle/create paths
(`0d0fa9e:src/commands/send.ts:204` `sendRaw(message + "\r")`;
`0d0fa9e:src/commands/lifecycle.ts` `deliverPrompt` `zmx send` `message + "\r"`).
**The mechanism is TS-IDENTICAL**, so TS production loses such pastes today.

The **queue path** (busy-session send) already delivers correctly with a
different mechanism (`0d0fa9e:src/commands/send.ts:154-180`): TWO writes — text
alone, ~200ms settle, `\r` alone (mimicking a human's separate Enter keystroke a
paste burst does not collapse) — followed by a **content-verified** remediation
CR (`composerHoldsMessage`: CR only while the composer provably still holds OUR
exact text, never blind). A4 boot-4 corroborated it live: the queue rows
(QPROBE_1..4) landed as JSONL user records.

## Decision

Apply the queue path's proven mechanism to the IDLE delivery paths, **with a
chunked text phase UNDERNEATH it** (mode a).

**Delivery is a CHUNKED TWO-WRITE** on every claude PTY text path:

1. **CHUNKED TEXT PHASE (mode a, the amendment):** split the text into **≤1024B
   code-point-safe chunks** (`submit::chunk_text` — never splits a UTF-8 code
   point; a chunk may run a few bytes short), send **each chunk as its own
   `Mux::send`** with a **~150ms inter-chunk settle** (`CHUNK_SETTLE_MS`; applied
   BETWEEN chunks only — not before the first, not after the last). This lets
   claude's reader drain the tty queue between writes so a large payload does not
   overflow it and drop wholesale. The chunk size (1024B) and settle (150ms) are
   the TS-cited values (`8c59ec4:src/commands/submit.ts`), injectable via
   `ChunkSendOptions` so the gate runs fast. **A ≤1024B send = exactly ONE chunk =
   a byte-identical single `Mux::send` with no inter-chunk sleep** — zero behavior
   change for small sends.

2. **TWO-WRITE `\r` (mode b, unchanged):** then ~200ms settle
   (`TWO_WRITE_SETTLE_MS`) and `send("\r")` ALONE — never a single
   `message + "\r"` write — so the `\r` lands as its own non-paste keystroke.

3. **CONTENT-VERIFIED remediation CR (unchanged):** read the composer screen (`zmx
   history`, pinned to the same op dir — Bug D) and emit a CR ONLY while
   `composer_holds_message` is true (anchored after the LAST `❯` glyph so a
   scrollback echo can never false-positive). Every CR is conditional on our text
   being visibly present and unsubmitted — never blind. (On a mode-(a) overflow the
   composer is empty, so this correctly fires zero CRs.)

**The chunking lands in the SHARED write layer** (`crates/qd/src/submit.rs`:
`chunk_text` + `send_text_chunked`, driven by `deliver_idle_two_write` /
`deliver_idle_two_write_with` and `RealDeliverDeps::send_message`), so **ALL** PTY
text paths get it: (a) `deliver_idle_two_write`'s text phase (idle `send:pty`); (b)
`deliver_prompt`'s text phase (`qd new -p` create); (c) the busy-queue text phase in
`verbs/send.rs` (was one `mux.send(text)`); (d) `--model`'s `/model <m>` — routed
through the same helper. **(d) is SUPERSEDED 2026-06-11 — see the note below.**

> **SUPERSEDED (warranty #2, 2026-06-11): `--model` is no longer delivered via a
> post-boot `/model <m>` slash command on the create path.** Model is a BIRTH
> PROPERTY of the session (the same principle as SB_SESSION_ID's
> explicit-set-at-every-launch), now emitted as a `--model <m>` LAUNCH FLAG in
> `build_new_extra_args`. The post-boot `/model` delivery was withdrawn because,
> in current Claude Code, the `/model` slash command PERSISTS the choice as the
> shared global default ("Set model to X and saved as your default for new
> sessions" → writes `~/.claude/settings.json`): every `--model` commission
> polluted the default a later PLAIN session would inherit, and `--model`
> combined with `-p` dropped the prompt (the `/model` submit + 2s settle left the
> composer such that the `-p` body never landed → exit 10). As a launch flag the
> model is per-session, touches no shared state, and the `-p` path runs
> unencumbered. Verified by outcome: born model == requested; `-p` delivered;
> global default UNCHANGED after a `--model` commission; and stop→resume
> PRESERVES the session's model (`claude --resume <uuid>` restores it from the
> session's own state — no revert to the default, so resume needs no `--model`).
> The `/model` chunked-delivery machinery below remains accurate for the
> `send:pty` / `-p` paths it still describes; only the `--model` create-path
> application is withdrawn.

**The W1 slash-command exception is now MOOT BY CONSTRUCTION — and this does NOT
diverge from the TS pin.** `--model` sends the COMBINED `/model <m>\r` (text AND the
CR, exactly as TS's `sendViaZmx` does: `8c59ec4:src/commands/lifecycle.ts:374-385`
`zmx send <name> message + "\r"`, "No splitting") routed through `send_text_chunked`.
Being ~tens of bytes (≤1024B) it yields exactly ONE chunk, so the helper emits a
SINGLE `mux.send("/model <m>\r")` write that is **byte-identical to TS's single
combined write** — NOT a two-write split. `/model` is a slash COMMAND, not a prompt,
so the mode-(b) two-write \r-rule does not apply to it (matching TS `sendViaZmx`,
which combines text+CR); routing it through the shared helper retires the W1
size-class carve-out without changing the bytes on the wire. (Call-site comment
updated at `verbs/lifecycle.rs`.)

Two paths land this:

1. **idle `send:pty` (the RULING):** the two-write delivery REPLACES the single
   write; the acceptance-keyed `verify_accepted_then_cr` is **PRESERVED** (an idle
   session must go busy), but its remediation CR is now content-verified. All
   load-bearing invariants stay: never-CR-busy, ≤1 remediation CR, keyed on
   ACCEPTANCE (busy) not COMPLETION. Stdout/stderr wording + exit codes are
   byte-unchanged (only the delivery mechanism changes).

2. **`deliver_prompt` / `qd new -p` (a LEAD EXTENSION of the ruling):** the same
   two-write + content-verified-CR shape. The ruling NAMES the idle `send:pty`
   path; the lead extends to `deliver_prompt` on the **same-mechanism** grounds —
   it had the identical single `message + "\r"` write, and external `qb spawn`
   priming prompts are the **LIKELIEST ≥4KB case**. The bounded-retry rounds are
   kept; each round's CR becomes content-verified. All ADR-0008 exit-contract
   semantics (Accepted/Stalled/PidFileMissing → 0/10/1) are unchanged. **This
   extension is flagged for ratification at the merge ruling.**

The delivery mechanism is **factored into one library function**,
`submit::deliver_idle_two_write` (real binding `deliver_idle_two_write_real`),
which the idle bin path AND the fakerepl R4 gate row BOTH drive — the gate
exercises production code, not a reimplementation.

This is a **SANCTIONED DIVERGENCE from TS under ADD-9a** (never reproduce a TS
bug). An upstream bug report (mechanism, 2-boot repro, captured bytes, the mirror
fix) is filed to the TS repo at the A4 merge ruling.

**The fakerepl Level-1 gate remains the delivery oracle.** Its burst model
diverges from real-claude's live large-write modes by default (which is WHY the gate
stayed green while the live findings surfaced), so the gate cannot itself catch the
live bugs — but it CAN, and now does, prove BOTH mechanisms are load-bearing:

- **mode (b):** a ≥4KB IDLE row drives the real two-write helper to exactly one
  turn, and a single-write negative control under `SB_FAKEREPL_ABSORB_ALL_CRS=1`
  (modeling the observed "CR doesn't recover it" behavior) MUST fail to land a turn.
- **mode (a):** a jumbo 16KB row through the real shared chunked helper lands
  exactly one turn with the FULL payload (report bytes == sent bytes), incl. a
  multibyte-straddle payload (構築日本語café☕ across many chunk boundaries) delivered
  intact. The negative-control PAIRING uses `SB_FAKEREPL_DROP_OVER_BYTES=4096` (which
  drops any single burst >4096B wholesale — modeling THE CLASS, not the live
  boundary): the UNCHUNKED mutation (`chunk_bytes = usize::MAX`) is dropped → ZERO
  turns (asserted RED), while the CHUNKED delivery under the SAME env passes → ONE
  turn (asserted GREEN). The red→green flip on chunk size alone proves chunking is
  load-bearing. (The fakerepl also now treats a `\r` on an EMPTY composer as a no-op
  — claude does not start a turn for an empty prompt — so a dropped write does not
  manufacture a zero-byte turn.)

## Consequences

- The idle `send:pty`, `qd new -p`, busy-queue, and `--model` paths now survive a
  large write that overflows the tty queue (mode a) AND a ≥4KB paste whose `\r` is
  absorbed (mode b). The ≤1024B common case is unaffected (one chunk, byte-identical
  single write; it submitted before and submits now).
- **Mode (b) is a SANCTIONED DIVERGENCE from TS (ADD-9a); mode (a) is a PARITY
  PORT.** The chunked text phase mirrors the new TS reference
  `8c59ec4:src/commands/submit.ts` (`chunkText` + `sendTextChunked`) — TS landed it
  first, so the Rust change converges with TS rather than diverging. The mode-(b)
  upstream report still mirrors this ADR.
- **Pin move:** the TS reference pin advances **`0d0fa9e` → `8c59ec4`** at the 0b
  delta (the pass-(b) target) — `8c59ec4` is the commit carrying the chunked-delivery
  reference (`chunkText`/`sendTextChunked` + its call sites in `send.ts` /
  `lifecycle.ts`) + the TS test vectors (`8c59ec4:src/submit.test.ts`).
- The observed drop boundaries (brano 8KB clean / ≥12KB dropped; TS ~4KB) are
  recorded as **EVIDENCE ROWS only** — machine/load-dependent, never a constant in
  code or a test assertion. The chunk size (≤1024B) is the invariant.
- Timing: the 200ms settle means the text burst closes BEFORE the CR arrives —
  which is EXACTLY the fakerepl keystroke-CR model (a lone `\r` is a non-paste
  burst that submits). The existing gate rows' timing assumptions hold under the
  new mechanism (the soak still passes).
- `LESSONS.md` L4 (RESOLVED @ pin 0d0fa9e) + L13 (two-write delivery) carry this
  invariant; the ported `send.ts` war-story comment in `verbs/send.rs` and the
  R4-fix comments in `submit.rs` cite the live evidence + the ruling.
- The `deliver_prompt` extension is the one item here BEYOND the literal ruling;
  if the merge ruling declines it, reverting `deliver_prompt` to a single write is
  a localized change (the idle `send:pty` fix and the factored helper stand
  independently).

## Wart-wave note (2026-06-05, ADR-0012 pointer)

The reader-stall silent mid-truncation RESIDUAL this ADR named (the narrow
window chunking could not close) is now CLOSED by verify-after-submit —
ADR-0012 (ADD-15 W8 / M11 sanction). The chunked delivery mechanics here are
unchanged; the read-back is a post-delivery belt on top.
