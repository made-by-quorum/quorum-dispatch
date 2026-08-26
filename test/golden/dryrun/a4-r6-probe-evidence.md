# A4 R6 LIVE PROBE — ≥8KB single-write delivery under the merged two-write path

**Operator:** R6 LIVE PROBE operator. **Date:** 2026-06-05 (01:42–01:50 EDT).
**Ordered:** orc-2, relay-1780637708238-13 item 1. **Ledger row:** R6.
**Branch:** `probe/a4-r6-live-agent` (isolated agent worktree
`.claude/worktrees/agent-a31427c91d46a58ac`, Bash run from there only — ADD-10).
**Tip:** `c7ff21e` (origin/main, the merged A4 two-write delivery — verified
`git log -1` before booting). **Host:** devbox, arm64/Darwin. **claude (macOS
jail):** 2.1.163. **zmx:** 0.6.0. **qd binary:** built from this worktree via
`scripts/build-lock.sh` (0.1.0, debug).

Raw per-row bytes: `a4-r6-bytes.txt` (both boots accumulate there). Driver:
`a4-r6-probe.sh` (mode `sendpty` = BOOT 1; mode `create` = BOOT 2).

## Hypothesis under test

From qd-orc-3's TS-side live verification: the merged two-write delivery is NOT
sufficient at large sizes. A ≥4KB single PTY write can **WHOLESALE-DROP** — the
PTY input buffer (~4096B) overflows before the reader drains; the composer ends
**EMPTY** and the JSONL user-record delta is **0**. This is a mode DISTINCT from
the R4 stuck-composer (where the text sits at the `❯` prompt unsubmitted). TS
reproduced: 4.3KB single write → delta 0 + EMPTY composer; the same message as
5×≤1024B chunks ~150ms apart + a separate CR → delta 1. A4's boot-6 4.3KB PASS
may have been boundary luck / zmx-transport draining. **Does the DROP reproduce
on the Rust path at 8KB and 16KB?**

## Method

Ported from `a4-live-boot6.sh` + `a4-paste-investigate.sh`: M5 full seed (probe-3
GrowthBook + onboarding + auth `.credentials.json`/`oauthAccount`, all READ-ONLY
from the real home), perl-alarm timeout wrapper (NO `timeout(1)`), glob-fallback
`$JP` resolver re-resolved before every row, resolution belt before every
session-targeting row, REAL-HOME BELT before/after each boot. Payloads carry
UNIQUE markers (`R6_8K_<runid>` etc.) and scattered multibyte UTF-8
(`日本語café☕`) so a future chunking fix's boundary handling has a baseline.

**Composer-state classification** per row (delta = JSONL user-record delta;
screen = ANSI-stripped `zmx history`):
- **DELIVERED** — delta ≥ 1 (the message landed as a user record).
- **STUCK-IN-COMPOSER** — delta 0 AND the marker `_START` is on the screen near
  the `❯` prompt (the R4 mode: text sits unsubmitted).
- **EMPTY-DROPPED** — delta 0 AND the marker is ABSENT from the screen (the
  hypothesis's wholesale-drop mode: nothing ever reached the composer).

## Pre-verification (NO real claude, 0 boots)

Before either boot: `mkp` payload builder verified offline (8.2KB/16.4KB/12.3KB,
both markers present, UTF-8 present, valid UTF-8 at every target); perl-alarm
wrapper verified (fires→124, propagates 0, propagates 7, exec-fail→127 — NOT an
rc=127 source); jail establish/seed/teardown smoke clean (auth keys present,
REAL-HOME BELT held). All green → boots proceeded.

## Per-row results

| # | path | bytes | JSONL delta | composer class | went busy | verb exit |
|---|------|-------|-------------|----------------|-----------|-----------|
| 1 | send:pty idle (BOOT 1) | 8223 | **1** | **DELIVERED** | yes | 0 |
| 2 | send:pty idle (BOOT 1) | 16416 | **0** | **EMPTY-DROPPED** | no | 0 (+WARNING) |
| 3 | send:pty idle (BOOT 2, recovered bisect) | 12320 | **0** | **EMPTY-DROPPED** | no | 0 (+WARNING) |
| 4 | `qd new -p` create (BOOT 2) | 16421 | **1** | **DELIVERED** | yes (status=busy) | **0** ("Prompt delivered") |

### Row 1 — 8KB send:pty: DELIVERED

`verb exit=0`, no stderr WARNING, session **WENT BUSY**, DELTA=1. claude read the
whole payload and replied: *"it starts with `R6_8K…_START` and ends with
`R6_8K…_END`, with repeated filler text (日本語café☕ / lorem ipsum) in between"*
— confirming the full 8KB INCLUDING the multibyte UTF-8 landed intact as one user
record. The two-write path handles 8KB cleanly.

### Row 2 — 16KB send:pty: EMPTY-DROPPED (the DROP reproduces)

`verb exit=0` but stderr fired the contract WARNING:
`WARNING: Message sent to "…-r6" but session did not go busy — it may be stuck
unsubmitted in the composer.` Session did NOT go busy in 12s; DELTA=0; status
stayed idle. The zmx history tail still showed the PRIOR (8KB) turn at the `❯`
prompt with an EMPTY composer — the 16KB marker `R6_16K…_START` is ABSENT from
the screen → classified **EMPTY-DROPPED**, NOT stuck-in-composer. The 16KB text
never reached the composer at all. **This is the hypothesis's wholesale-drop mode,
reproduced on the Rust two-write path.**

### Row 3 — 12KB send:pty (recovered bisect): EMPTY-DROPPED

Same wholesale-drop signature as 16KB: `verb exit=0` + WARNING, did not go busy,
DELTA=0, marker ABSENT (screen showed only the AUTHOK warm-up turn). So the drop
threshold on the send:pty idle path is **below 12KB** — the 8KB-DELIVERED /
12KB-DROPPED interval is the live boundary. (Driver-bug note below: this row was
meant to run inside BOOT 1; it was recovered on BOOT 2's already-warm session at
ZERO extra boot cost.)

### Row 4 — 16KB `qd new -p` create path: DELIVERED (exit 0, NOT a drop)

**The create path does NOT drop the same 16KB the send:pty path drops.**
`qd new -p` returned **exit 0**, stdout `Started detached session …` +
**`Prompt delivered to …`**, no stderr. The create session went to status=busy,
DELTA=1, and its zmx history showed the FULL payload in the composer —
`R6_CREATE16…_START`, the `日本語café☕` UTF-8, and `…_END` all present — with
claude already `Computing…`. Classified **DELIVERED**.

**Exit-code fact for the ADR amendment:** the create-path went-busy contract
produced **exit 0 (Accepted)**, NOT the `Stalled → 10` the hypothesis predicted
for a wholesale drop. The reason it did not stall: the create path's
`deliver_prompt` runs a BOUNDED-RETRY loop (1 + up to 3 content-verified
remediation CRs; `submit.rs` `deliver_prompt` / `lifecycle.rs` exit map
Accepted→0 / Stalled→10 / PidFileMissing→1), whereas the send:pty idle path runs
a SINGLE `deliver_idle_two_write` (one two-write + at most one content-verified
remediation CR). On this boot the create path's extra rounds (and/or the
fresh-boot drain timing) got the 16KB accepted. So the live create-path drop at
16KB did **not** reproduce — exit 10 was **not** observed on this row.

## VERDICT

- **The wholesale-drop REPRODUCES on the Rust `send:pty` idle two-write path** at
  **12KB and 16KB** — mode = **EMPTY-DROPPED** (delta 0, did-not-go-busy WARNING,
  marker absent from the composer; the text never reaches the composer). This is
  the DISTINCT buffer-overflow mode the hypothesis names, NOT the R4
  stuck-in-composer mode.
- **8KB DELIVERS clean** on the same path (delta 1, went busy, full UTF-8 intact).
  The live drop boundary is in the **8KB–12KB** interval (≤12KB drops, 8KB holds).
- **The `qd new -p` create path DELIVERED 16KB** (exit 0 / Accepted / delta 1 /
  full payload in composer). The drop did **not** reproduce there on this boot;
  the create path's bounded-retry remediation (and/or fresh-boot timing)
  succeeded. **Exit 10 (Stalled) was NOT produced** — the predicted
  wholesale-drop create-path exit code was not observed.
- Net: the merged two-write delivery is **sufficient at 8KB but NOT at ≥12KB on
  the send:pty idle path**; the create path is more robust (multi-round) and held
  at 16KB on this single boot.

## Transport observations (zmx write behavior)

- The send:pty drop fires AFTER the verb's own two-write (text alone → 200ms →
  separate `\r`) — i.e. delivering the text as ONE `zmx send <name> <16KB>` write
  still overflows the PTY input buffer before claude's reader drains it. The
  separate-CR keystroke and the content-verified remediation CR (which only fires
  while the composer holds the text) do not help here because **the composer never
  holds the text** — it was dropped before reaching the composer, so
  `composer_holds_message` is false and the remediation CR is (correctly)
  suppressed. The drop is a TRANSPORT-LAYER overflow on the single large text
  write, upstream of the submit discipline.
- **zmx does NOT visibly chunk the large write** in this path — a single
  `mux.send(dir, name, <12–16KB>)` is handed to `zmx send` as one argument, and
  the live result (EMPTY-DROPPED) is consistent with the buffer overflowing on
  that one write. No zmx-side fragmentation was observed that would have rescued
  it. (This matches the TS-side finding that the FIX is to chunk into ≤1024B
  pieces ~150ms apart — i.e. the chunking must happen ABOVE the single `zmx send`.)
- The 8KB write landing intact while 12KB/16KB drop is consistent with a
  ~4096-byte PTY input buffer plus some drain headroom: 8KB fits within
  one-buffer-plus-drain on this host's timing; ≥12KB does not.

## Boots / belt accounting

- **Boots spent (macOS real-claude): 2** — BOOT 1 (sendpty: warm-up + 8KB + 16KB)
  and BOOT 2 (create: warm-up + 12KB recovered bisect + 16KB `qd new -p`). Within
  the ≤2 budget. Zero pre-boot failures (pre-verification caught nothing).
- **REAL-HOME BELT: 731 → 731 HOLDS on BOTH boots**, zero leaked prefixed rows.
  Clean trap-protected teardown via jail primitives; post-run sweep confirmed zero
  orphaned claude/zmx of mine and both jail roots (`qdrg-9445…`, `qdrg-4512…`)
  removed.

## Driver-bug note (accounting only — NOT a data defect)

The BOOT-1 driver had a command-substitution bug: `R8="$(sendpty_row …)"`
captured the function's tee'd `log` lines into `$R8`, so the bisect comparison
`[ "$R8" = DELIVERED ]` failed and the 8KB-vs-16KB summary mark/counters ran in
subshells (BOOT-1 summary printed `GREEN 0 / RED 0`). **The per-row DATA in
`a4-r6-bytes.txt` is fully correct and unaffected** (delta / class / busy / exit
all captured before the buggy echo). The bug only mis-fired the in-driver bisect
branch, so the 12KB row did not run on BOOT 1. FIX: publish the class via a file
(`$JAIL_ROOT/last_class`) instead of stdout; the 12KB bisect was then recovered
on BOOT 2's already-warm send:pty session at ZERO extra boot cost (row 3). The
fixed driver's BOOT-2 summary is correct (`GREEN 1 / RED 1`).

## FACTS-ONLY disposition

The drop reproduces on the send:pty idle path at ≥12KB as EMPTY-DROPPED; 8KB
holds; the create path held 16KB (exit 0, no stall) on one boot. The lead decides
the fix (the TS-side remedy is sub-buffer chunking above the single `zmx send`).
The UTF-8 multibyte payloads are a captured baseline: at 8KB and at the
create-path 16KB the full `日本語café☕` round-tripped intact through whatever DID
deliver, so a future chunking fix must preserve UTF-8 boundaries (do not split a
multibyte sequence across chunks).

---

# A4 R7 LIVE CONFIRM — the R6-failure rows (12KB, 16KB send:pty idle) on the MERGED chunked-delivery binary

**Operator:** R7 live-confirm operator. **Date:** 2026-06-05 (07:24–07:30 EDT).
**Ordered:** orc-3 ruling, relay-1780658366989-17 ask 2. **Ledger row:** R7.
**Branch:** `phase/a4-submit` (isolated worktree `~/work/wt-a4-lead`,
`git fetch origin && git reset --hard origin/main`; Bash run from there only —
ADD-10). **Tip:** `37881b1` — **PR #13 chunked delivery merged** (verified
`git log -1` before booting; the chunking fix is commit `60fe8a7`). **Host:**
devbox, arm64/Darwin. **claude (macOS jail):** 2.1.165. **qd binary:** built from
this worktree via `scripts/build-lock.sh cargo build -p quorum-dispatch` (0.1.0, debug).

Raw per-row bytes: `a4-r7-bytes.txt`. Driver: `a4-r7-probe.sh` (ported EXACTLY
from `a4-r6-probe.sh` sendpty mode; hardcoded to the two R6-failure sizes 12KB +
16KB, ONE boot).

## What changed vs R6

R6 (commit `8ca5987`, pre-fix two-write binary) found **12KB and 16KB
EMPTY-DROPPED** on the send:pty idle path: delta 0, did-not-go-busy WARNING,
marker absent from the composer (tty-queue wholesale-drop, ADR 0009 mode (a)).
The merged fix (`60fe8a7`) ports `chunk_text` + `send_text_chunked` into the
SHARED Rust write layer — PTY text is split into ≤1024B code-point-safe chunks
with a 150ms inter-chunk settle, so a large write no longer overflows the
~4096B canonical tty queue. R7 re-runs those exact two failure sizes on the
FIXED binary.

## Method

Identical to R6 sendpty mode: M5 full seed (probe-3 GrowthBook + onboarding +
auth `.credentials.json`/`oauthAccount`, READ-ONLY from the real home),
perl-alarm timeout wrapper (NO `timeout(1)`), glob-fallback `$JP` resolver
re-resolved before every row, resolution belt before every session-targeting
row, REAL-HOME BELT before/after. Payloads carry UNIQUE markers
(`R7_12K_<runid>` / `R7_16K_<runid>`) and scattered multibyte UTF-8
(`日本語café☕`). Composer-state classification per row (DELIVERED / STUCK-IN-COMPOSER
/ EMPTY-DROPPED). Pre-verification (0 boots): `mkp` payloads at 12288/16384 had
both markers, UTF-8 present, valid UTF-8 at every target (12318B / 16414B).

## Per-row results

| # | path | bytes | JSONL delta | composer class | went busy | verb exit | R6 was |
|---|------|-------|-------------|----------------|-----------|-----------|--------|
| 1 | send:pty idle (BOOT 1) | 12318 | **1** | **DELIVERED** | yes | 0 | EMPTY-DROPPED |
| 2 | send:pty idle (BOOT 1) | 16414 | **1** | **DELIVERED** | yes | 0 | EMPTY-DROPPED |

### Row 1 — 12KB send:pty: DELIVERED (R6 EMPTY-DROPPED → now delivers)

`verb exit=0`, **no stderr WARNING**, session **WENT BUSY** within 12s, DELTA=1
(user-records 1→2). The full 12KB landed as one user record; claude read it and
replied *"it appears to be a test payload (markers `R7_12K…_START` … `R7_12K…_END`
with repeated filler text, ~12K characters)"* — confirming the marker pair AND
the multibyte UTF-8 round-tripped intact. Contrast R6 row 3: same 12KB →
EMPTY-DROPPED (delta 0, did-not-go-busy WARNING, marker absent).

### Row 2 — 16KB send:pty: DELIVERED (R6 EMPTY-DROPPED → now delivers)

`verb exit=0`, **no stderr WARNING**, **WENT BUSY** within 12s, DELTA=1
(user-records 2→3). The zmx history tail shows the full payload reached the
composer/transcript — `…consectetur 日本語café☕  R7_16K…_END` visible — and
claude acknowledged *"~16K characters of the same filler pattern"*. Contrast R6
row 2: same 16KB → EMPTY-DROPPED. The chunked write (≤1024B pieces, 150ms apart)
no longer overflows the tty queue at 16KB.

## VERDICT

**CONFIRMED FIXED.** Both R6-failure rows — **12KB and 16KB on the send:pty idle
path** — now **DELIVER** on the merged chunked-delivery binary (`37881b1`,
fix `60fe8a7`): delta=1, DELIVERED, went busy, verb exit 0, no did-not-go-busy
WARNING, full multibyte UTF-8 intact in the composer. The EMPTY-DROPPED
wholesale-drop mode R6 reproduced at these sizes does NOT reproduce on the fixed
path. GREEN 2 / RED 0.

## Boots / belt accounting

- **Boots spent (macOS real-claude): 1** — ONE session (`qdrg-…-r7`): warm-up
  (AUTHOK, urc=1) + 12KB + 16KB. Within the ≤2 budget; planned for 1, spent 1.
  Zero pre-boot failures.
- **REAL-HOME BELT: 734 → 734 HOLDS**, zero leaked prefixed rows. Clean
  trap-protected teardown via jail primitives; post-run sweep confirmed zero
  orphaned claude/zmx of mine and the jail root
  (`/tmp/claude-501/qdrg-runs/209801780658671170`) removed.

## FACTS-ONLY disposition

The R6 EMPTY-DROPPED drop at 12KB/16KB on the send:pty idle path is fixed by the
merged chunked delivery: both sizes now DELIVER (delta 1, busy, exit 0, UTF-8
intact). One boot, belt held.
