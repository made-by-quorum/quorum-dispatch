# dispatch DELIVERY-RECEIPTS — how to resolve a send's outcome from the event log

**Status: normative, external, reader-facing.** This is the **resolution recipe** for
qd's dispatch-side delivery receipts: given a raw delivery event log
(`state/sessions/*.events.jsonl`) and a `send_id`, how do you resolve that send to
**landed / failed / pending / recovered-attributed** — with **zero false "landed"** and
no undisclosed false "abandoned"?

This doc is a **different altitude** from
[`EVENT-CONTRACT.md`](./EVENT-CONTRACT.md). EVENT-CONTRACT.md §1 + §1.5 is the **wire
schema** — the on-disk envelope, the payload kinds, the 3-phase delivery contract,
`TERMINAL_EVENTS`, first-terminal-wins. **Read it first for the record shapes.** This
doc does not restate the schema; it **cross-references** it and adds the one thing the
schema doc deliberately does not carry: the reader's **decision procedure** and the
honesty guarantees it upholds.

> **Note:** EVENT-CONTRACT.md §1.2/§1.3 predates R6 — it lists a 6-member
> `TERMINAL_EVENTS` missing `send-failed`, and its `pending-abandoned` reason list omits
> `recovery-unattributable`. **This doc is authoritative on the terminal set (7 members)
> and the recovery reasons;** EVENT-CONTRACT is a flagged, out-of-scope follow-up.

> **Scope: dispatch-side only.** These receipts are single-machine artifacts written by
> the `dispatch` crate into its own `state/sessions/<key>.events.jsonl`. Nothing here
> concerns frame, obligations, cross-machine fold, or a consumer's adoption machinery
> (see §7). **Paths** are relative to the dispatch crate: `events.rs` =
> `crates/dispatch/src/events.rs`; `send.rs` / `send_relay.rs` / `wait.rs` /
> `lifecycle.rs` / `recover.rs` = `crates/dispatch/src/bin/qd/verbs/*.rs`;
> `relay_server/mod.rs` = `crates/dispatch/src/relay_server/mod.rs`. Every `file:line`
> below was verified against the current source on branch `qd-hardening/delivery`.

> **Authorities.** Pete's human ruling `01KX8NA7HB` (identical-content landing =
> DELIVERED; `recovered:true` is metadata-only; NO separate not-landed category for
> content-found); the NoRecord terminus ruling `01KX8MDPDX` (the 4-state recovery
> split); the ACP post-inject ruling `01KX8MP43N` (post-inject StopReason is
> turn-outcome, not delivery evidence). Where this doc and the code disagree, **the code
> is the arbiter** — findings are called out inline.

---

## 1. Receipt vocabulary

A send moves through **phases** and ends at exactly one **terminal receipt**. Only the
terminal resolves the outcome; a phase never does.

### 1.1 Phases (NON-terminal — NEVER a receipt)

| kind | meaning | verified |
|---|---|---|
| `send-initiated` | the recovery **anchor** — emitted before the first wire activity; carries `send_id`, `content_sha256`, `content_len`, `chunk_sha256s`, and (pty only) `transcript`/`transcript_offset` | EVENT-CONTRACT.md §1.2 (1); pty `send.rs`, relay `send_relay.rs:237`, new-p `lifecycle.rs:944` |
| `chunks-delivered` | pty transport ack ("bytes went to the PTY") | EVENT-CONTRACT.md §1.2 (2) |
| `relay-delivered` | relay on-queued ack — **the relay analog of `chunks-delivered`** | `events.rs:333` (`RelayDelivered`); emitted `send_relay.rs:255` |
| `turn-accepted` | daemon-lane on-queued ack — the resident **accepted the prompt as a turn** (ACP/pi/codex) | `events.rs:349` (`TurnAccepted`) |
| `composer-cleared` | weak screen-derived corroborator, advisory | EVENT-CONTRACT.md §1.2 (7) |

**`relay-delivered` and `turn-accepted` are NOT receipts.** They are on-queued strength
signals: "the message left / the resident took a turn," never "it entered context."
Neither is in `TERMINAL_EVENTS` (`events.rs:106-123`); `is_terminal("relay-delivered")`
is asserted **false** at `events.rs:2293`, and `turn-accepted` is absent from the set by
construction. **Wiring a resolver to either is the canonical mistake** — the whole
honesty story below rests on them never gating.

### 1.2 Terminal receipts (`TERMINAL_EVENTS`, currently **7** — `events.rs:106-123`)

```
TERMINAL_EVENTS = [ turn-anchored, turn-anchored-mismatch, anchor-timeout,
                    pending-abandoned, message-seen, seen-failed, send-failed ]
```

| terminal | class | one-line meaning |
|---|---|---|
| `turn-anchored` | **LANDED** | the sent bytes appear as a consumed turn in the recipient transcript (pty/new-p; or a recovered anchor) |
| `turn-anchored-mismatch` | **LANDED (truncated)** | a chunk-prefix-truncated version of the send landed |
| `message-seen` | **LANDED** | **the message ENTERED THE SESSION'S CONTEXT — the real receipt** (relay / async-pty / daemon lanes) |
| `send-failed` | **FAILED** | a **pre-wire door failure** — provably never reached the wire (the ONLY foreclosing "didn't land") |
| `pending-abandoned` | **PENDING (disclosed)** | the recovery best-effort closer — see §2; NEVER a hard "failed" |
| `anchor-timeout` | (see §2/§7) | the pty watch's own timeout; **not immutable** — a later recovery-read may append a `turn-anchored` after it |
| `seen-failed` | **FAILED** | recipient-gone on the **relay** lane only (§3); **ACP never mints this** (§3) |

**`message-seen` is the load-bearing "landed" signal.** It is a deliberately distinct
kind from `turn-anchored` (EVENT-CONTRACT.md §1.5) so that a `--wait` anchor can never
trip an on-received gate. When you see `message-seen{send_id}`, the message provably
entered that recipient's working context.

**`send-failed` is the only terminal that says "provably did NOT land," and it says so
ONLY pre-wire.** It is emitted at a send **door** — before the message reaches the wire —
by the one funnel `emit_door_failure` (`send_relay.rs:1397-1445`, construction at
`:1440`), which serves the relay door and all daemon door arms (codex / acp / pi). Its
`send_id` is **omitted** (`:1441`): a pre-wire door has no server- or resident-minted id
yet. Because it fires before wire activity, "didn't land" is a *fact*, not an inference.
There is no post-wire "it didn't land" terminal on any carrier — post-wire, absence of
evidence is never proof of non-delivery (§2, §5).

**First-terminal-wins, per `send_id`** (`first_terminal_for`, EVENT-CONTRACT.md §1.3): a
recovery-read may append a late `turn-anchored` after an `anchor-timeout`, so the reader
takes the **first** terminal in file-read order as the verdict. `send-initiated` +
exactly one terminal per `send_id` is the invariant.

---

## 2. THE RESOLUTION RECIPE

Given a `send_id`, merge both key files (uuid + `byname-<name>`, EVENT-CONTRACT.md §1.4),
order by `(ts, pid, seq)`, and take the **first terminal** for that `send_id`. Resolve it
with this table. **The outcome column has exactly four values: LANDED, FAILED, PENDING,
RECOVERED-ATTRIBUTED — and no send may resolve to a false LANDED.**

| first terminal for `send_id` | outcome | reader statement |
|---|---|---|
| `turn-anchored` (any) | **LANDED** | delivered. If `recovered:true`: delivered, recovered late via the recovery-read path — *metadata only, still delivered* |
| `turn-anchored-mismatch` (any) | **LANDED (truncated)** | delivered, but a chunk-prefix-truncated form; `expected_len`/`actual_len` disclose the truncation |
| `message-seen` | **LANDED** | delivered — entered context |
| `send-failed` | **FAILED** | provably did not land (pre-wire door: `reason` token) |
| `pending-abandoned{reason:"recovery-no-candidate", recovered:true, attribution}` | **RECOVERED-ATTRIBUTED (disclosed PENDING)** | **"could not confirm delivery — NOT proof it didn't land."** The recovery searched the recipient transcript past the send's anchor (`attribution` = `offset`\|`time-window`) and found no matching candidate. NEVER read "landed", never a hard "failed" |
| `pending-abandoned{reason:"recovery-unattributable"}` (no `recovered`/`attribution`) | **PENDING (bare)** | the send carries no recovery keys (no `content_sha256`); no search is possible. Undetermined |
| `pending-abandoned{reason:"watch-interrupted"}` or `{reason:"session-died"}` | **PENDING** | the watch ended without a verdict (process/panic residual); the send is recoverable — run `qd delivery:recover` |
| `anchor-timeout` (and no later `turn-anchored`) | **PENDING** | the pty watch timed out; the send is recoverable — see §7 (C6-deferred budget) and run `qd delivery:recover` |
| **NO terminal** for the `send_id` | **PENDING** | honest open state — the outcome is genuinely undetermined; see §2.2 |

### 2.1 The content-found rule (Pete's ruling `01KX8NA7HB`)

**Content-FOUND resolves to DELIVERED — always, including `recovered:true`.** When the
recovery-read anchors the sent content in the recipient transcript, the send is
DELIVERED. `recovered:true` records only *how* we learned it (via the recovery-read path,
not a contemporaneous watch); it does **not** demote the receipt. **There is no separate
"recovered (attributed) non-landed" category for content that was found** — that category
(from the superseded F1/F3 seam ruling) is eliminated. The recovery verdict's disclosure
wording still records "best-effort late attribution," but the **resolution is DELIVERED**.

Verified: `recovery_event` maps `RecoveryVerdict::Anchored → turn-anchored{recovered:true,
attribution}` (`events.rs:1767-1776`) and `Truncated → turn-anchored-mismatch{recovered:
true, attribution}` (`events.rs:1777-1790`). Both are `turn-anchored[-mismatch]` — LANDED.

### 2.2 The four epistemic states of recovery (the crux — NoRecord ruling `01KX8MDPDX`)

When a send has a `send-initiated` but no terminal and its **writer incarnation is dead**
(`is_dead_dangling`, EVENT-CONTRACT.md §1.4), a recovery-read resolves it. The single
pre-R6 foreclosing `Abandoned` is now split into **four epistemically-distinct states**
(`RecoveryVerdict`, `events.rs:1442-1476`; produced in `recovery_read`,
`events.rs:1505-1583`). A positive match self-evidences; **absence is evidence only
relative to a searched, non-empty window.**

| state | trigger (in `recovery_read`) | terminal emitted | recipe row |
|---|---|---|---|
| **(a) SourceUnavailable** | transcript could not be read/resolved (`build_window → None`, `events.rs:1525-1527`) | **NONE** (`recovery_event` returns `None`, `events.rs:1809`) | NO terminal → **PENDING** (still recoverable; a later sweep resolves it) |
| **(b) EmptyWindow** | read succeeded but **zero** candidate user-records past the anchor / in the time-window (`events.rs:1536-1537`) | **NONE** (`events.rs:1809`) | NO terminal → **PENDING** (window still growable: busy-turn flush lag, rotation-in-place) |
| **(c) Abandoned** | candidates existed past the anchor; **none matched** exact-sha or chunk-prefix (`events.rs:1582`) | `pending-abandoned{recovery-no-candidate, recovered:true, attribution}` (`events.rs:1794-1799`) | **RECOVERED-ATTRIBUTED** — "could not confirm delivery — NOT proof it didn't land" |
| **(d) Unattributable** | no `content_sha256` on the `send-initiated` — a search can never run (`events.rs:1509-1511`) | `pending-abandoned{recovery-unattributable}` (`events.rs:1802-1807`) | **PENDING (bare)** — carries no recovery keys; no search possible |

The asymmetry between (b) and (c) is the whole point of R6: an **empty** window means
nothing was searched — the recipient has not demonstrably progressed past the send — so
it stays open (PENDING). A **searched, non-empty** window with no match is the strongest
non-delivery evidence the recovery keys can yield — exhausted best-effort — so it closes,
**disclosed**, and still never claims "didn't land."

### 2.3 Closing an orphaned initiation — `qd delivery:recover`

A stranger holding a log with a dangling `send-initiated` (no terminal) can close it by
running **`qd delivery:recover`** (optionally `--send-id <id>`). The verb
(`recover.rs`):

1. Scans every `state/sessions/*.events.jsonl` for `send-initiated` records with
   `verb ∈ {send:pty, new-p}` — **transcript-anchored sends only** (`recover.rs:240-243`).
2. For each, applies the **liveness fence**: it emits a terminal **only** when
   `is_dead_dangling` returns true (`recover.rs:176`) — a still-live writer is left
   untouched. This is why the verb is a separate, safe process: it never forecloses a
   send whose sender is still running.
3. For a dead-dangling send it runs `recovery_read` and appends the §2.2 verdict
   idempotently (an exclusive `flock` across the re-check→emit, `events.rs:1843-1896`,
   so two concurrent runs can't double-emit).

**PENDING is an honest answer.** States (a) and (b) leave the send with no terminal — the
verb reports it as "left recoverable," and a later run resolves it once the transcript
resolves or the window grows. **Scope fence:** the verb does **not** sweep relay or
daemon sends — those have no local transcript to recovery-read, and running the read on
them would manufacture a false `pending-abandoned` (`recover.rs:24-29`). Relay/daemon
sends resolve via their recipient-side observers (§3), or stay PENDING (§7 residual).

### 2.4 What a qd exit code means

A qd send's **exit code is not a receipt.** Exit 0 means the **wire accepted** the send
(the POST returned, the PTY write went through, the resident took the turn) — it does
**not** mean the message landed. Only a **terminal** in the log says landed. A `--wait`
relay send's exit reflects whether a *reply* arrived, not whether the message entered
context (§7). The machinery that consumes these terminals to satisfy an obligation is
**deferred** (§6, C6).

---

## 3. Per-carrier receipt semantics

Each carrier derives `message-seen` (or a `turn-anchored`) differently. The reading
splits into a **record-presence FLOOR** ("the bytes are present as a record") and a
**turn-consumption STRONG** reading ("a turn consumed the prompt").

| carrier | "delivered" terminal | reading | how derived | honest failure / pending |
|---|---|---|---|---|
| **claude-pty** (`send:pty`, `new-p`) | `turn-anchored` | **STRONG** (turn-anchored) | the sent bytes appear as a consumed user-turn at/after the anchor offset (`send.rs:951`, `:1064`; new-p `lifecycle.rs:1561`); async no-wait pty also emits `message-seen` (W8 read-back, `send.rs:1042`) | `turn-anchored-mismatch` (truncation, `send.rs:1104`); `anchor-timeout` (watch timeout, recoverable); dead-dangling → `qd delivery:recover` (§2.2) |
| **claude-relay** | `message-seen` | **FLOOR** (record-presence) | a recipient-side transcript observer (`relay_server/mod.rs run_received_observer`, emit `:994`) emits `message-seen` when the relay `message_id` lands as a `<channel … message_id="…">` **wrapper attribute** in the recipient's own transcript | `seen-failed{recipient-gone}` at the recipient's session-close bookend for a tracked-but-unpulled id (`relay_server/mod.rs:1125`); latency is **never** a failure (an un-pulled but alive message stays PENDING) |
| **ACP** (acp/claude-code, opencode) | `message-seen` | **STRONG** on clean turn; **FLOOR** on landing-check | see §3.1 — post-inject StopReason is turn-outcome; delivery = a **landing check** against `~/.claude/projects`, content-keyed on `content_sha256` (`wait.rs:545-565`, emit `:619`) | **NO terminal** on not-landed/ambiguous — the send stays recoverable (§3.1). **ACP never mints `seen-failed`** |
| **pi** | `message-seen` | **FLOOR** (record-presence) | a content-keyed rollout observer emits `message-seen` when the sent `content_sha256` appears as a user-turn record in the pi rollout (`wait.rs:1002-1069`, emit `:1066`); the dead-only structured floor sub-lane emits via `emit_daemon_seen` (`send_relay.rs:355-404`, `:400`) | door failure → `send-failed` (§1.2); otherwise stays PENDING. **pi LIVE conformance is DEFERRED** (§6) |
| **codex** | — | — | **DEFERRED — codex dies on brano** (`qd start --provider codex` unsupported; sessions die instantly). No conformance; do not rely on codex receipts | door failure → `send-failed`; otherwise PENDING |

**Floor vs strong, labeled:** relay `message-seen`, pi `message-seen`, and the ACP
landing-check are the **record-presence FLOOR** ("the bytes are present as a record").
pty `turn-anchored` and a clean ACP `Completed` are the **turn-consumption STRONG**
reading ("a turn consumed the prompt"). The floor is a true "entered context" — it is an
observation of the recipient's own record, not an inference.

### 3.1 ACP (FINAL from D2 — verified at `wait.rs`)

- **Post-inject StopReason is turn-OUTCOME, not delivery evidence** (ruling
  `01KX8MP43N`). A clean `Completed` or the `MaxTokens` limit proves the turn consumed
  the prompt → **`message-seen`** (`acp_delivery_disposition` matches `Completed |
  MaxTokens => Seen`, `wait.rs:498`; emit `:625`). The limit reason is preserved in the
  classification, never laundered into a clean completion. (These are the shipped
  `TerminalReason` names, `rpc.rs:84-92`; the ACP wire `StopReason::EndTurn` classifies to
  `Completed` and `StopReason::MaxTokens`/`MaxTurnRequests` to `MaxTokens` at
  `rpc.rs:110-111`.)
- **A post-inject FAILURE StopReason** (Cancelled / Refusal / Failed / Crashed /
  TransportLost) is *post-delivery* — the inject already succeeded. It is
  **LANDING-CHECKED** against the ACP session's native CC transcript in
  `~/.claude/projects`, content-keyed on `content_sha256` (`acp_prompt_landed`,
  `wait.rs:545-565`):
  - **found** → `message-seen` (`AcpLanded::Yes`, `wait.rs:637`) — it landed; the turn
    merely didn't complete cleanly. The turn-outcome reason is preserved, never
    laundered.
  - **not found / unresolvable** → **NO terminal** (`AcpLanded::No | Unknown => return`,
    `wait.rs:649`). The send stays recoverable.
- **ACP NEVER mints `seen-failed`.** The `No → seen-failed` arm rests on a write-ordering
  guarantee ("the bridge writes the user record before the terminal response") that is
  **unprovable on brano** (the claude-code-acp bridge is absent). At R6 the `No` arm is
  **DEGRADED to recoverable** (`wait.rs:576-579`, `:638-649`). So the ACP terminal set is
  exactly: **`message-seen` (landed) | NO terminal (not-landed/ambiguous → recoverable)**.
  There is **no `Payload::SeenFailed` construction anywhere in `wait.rs`** — the only
  `seen-failed` producer crate-wide is the relay observer at `relay_server/mod.rs:1125`.
  Re-enabling `No → seen-failed` is **DEFERRED** (re-entry needs the ACP bridge installed
  on a box to pin the write-ordering, §6).

> **Reconciliation note (code vs. the pre-fix map).** The crate-wide emission map
> (`d1-coord`, @ pre-fix `65e33c90`) lists "wait.rs:550 seen-failed — D2 to certify." The
> **current** code degraded that arm: `wait.rs` no longer constructs `SeenFailed`. The map
> is stale on this row; the shipped behavior is the degrade above. **The code is the
> arbiter.**

---

## 4. Recovery lie-shape cells (each a stranger-test)

These are the shapes a naive resolver would misread as "landed" or as a hard "failed."
Each is shown as the event-log fragment a stranger holds, and how the recipe resolves it.
**None resolve to a hard "landed"; each maps to a disclosed or recoverable category.**
(Shapes drawn from the D1 build red-team rounds 1-2; the resolution is §2 applied.)

**(1) retry-after-kill** — a sender is killed mid-send, then a second sender re-sends the
same content; the log carries a dangling `send-initiated` (dead pid) plus, later, a
`turn-anchored` for the *re-send*.
→ The dangling initiation is dead-dangling; `qd delivery:recover` runs `recovery_read`.
If the content is found past the anchor → `turn-anchored{recovered:true}` → **LANDED**
(§2.1). If a searched non-empty window has no match → `pending-abandoned{recovery-no-
candidate, recovered:true}` → **RECOVERED-ATTRIBUTED** ("could not confirm — not proof it
didn't land"). Never a false "landed" for the *killed* send; never a hard "failed."

**(2) killed-common-content** — two sends carry byte-identical content; one is killed.
→ Exact-sha matching cannot distinguish them, but the recovery-read anchors on **content
presence past the offset**, and resolves to DELIVERED for a found match. The disclosure
`recovered:true` + `attribution` records that this is best-effort attribution, not a
contemporaneous watch — so a reader knows the identical-content ambiguity is disclosed,
not hidden. Resolution: **LANDED (disclosed)** — per Pete's ruling, identical-content
landing IS delivered.

**(3) false-mismatch** — the recipient reformats/merges the turn so neither exact-sha nor
chunk-prefix matches, though the content *did* land.
→ Searched, non-empty window, no match → `pending-abandoned{recovery-no-candidate,
recovered:true, attribution}` → **RECOVERED-ATTRIBUTED**. The reader statement is "could
not confirm delivery — NOT proof it didn't land." This is an accepted, **disclosed**
residual (a structural key limit; NoRecord ruling §7) — **not** a hard "failed" and
**not** a false "landed."

**(4) partial-write-landed** — a truncated prefix of the message landed as a real turn.
→ Chunk-prefix truncation detected → `turn-anchored-mismatch{recovered:true}` →
**LANDED (truncated)**, with `expected_len`/`actual_len` disclosing the truncation. The
door-side partial-write case (a `send:pty` whose PTY write partially failed) emits **no
foreclosing terminal** — it stays recoverable, not a false "failed."

**(5) watch-interrupted** — the `--wait` watch is torn down (process exit / panic) before
a verdict.
→ `pending-abandoned{reason:"watch-interrupted"}` (WatchGuard drop residual,
`events.rs:2167`), which carries **no** `recovered`/`attribution` → **PENDING**. The send
is recoverable: `qd delivery:recover` re-reads and resolves it to the real outcome. A
watch teardown is never itself a "failed."

---

## 5. The honesty criteria — QS-1 and its converse

Two criteria bind the recipe, and both must hold:

- **QS-3 (hard zero): no false "landed."** No send may resolve to LANDED unless its
  content was positively observed in the recipient's own record (`turn-anchored` /
  `turn-anchored-mismatch` / `message-seen`). Absence of evidence never produces a
  "landed." This is a **hard zero** — a false "landed" is a silently lost message with no
  consumer remedy.

- **QS-1 converse: no UNDISCLOSED false "abandoned."** A landed send may read "abandoned"
  **only** through the disclosed attribution-limited category
  (`pending-abandoned{recovery-no-candidate, recovered:true, attribution}`) — and **never**
  through a terminal emitted while its window was still growable (EmptyWindow → no
  terminal) or unreadable (SourceUnavailable → no terminal). The disclosure stamp
  (`recovered:true` + `attribution`) is the mechanical routing: it sends a reader to the
  RECOVERED-ATTRIBUTED category, uniformly.

The asymmetry is principled: a false "landed" is unrecoverable (the message is lost
silently); a false "unconfirmed-abandoned" is consumer-recoverable (resend). So
accept-and-disclose is sufficient on the abandoned side while zero-tolerance binds the
landed side.

---

## 6. Deferred — machinery this doc does NOT imply exists

These are **explicitly deferred**; the recipe does not depend on them, and readers must
not assume they exist:

- **C6 — frame/obligation consumption of receipts.** Nothing here builds the machinery
  that adopts a terminal to satisfy a frame obligation. That is C6; the terminals are
  single-machine artifacts only.
- **QS-4 + the obligation-holder-visibility half of QS-1** — C6 properties, deferred.
- **The pending-forever residual class** — a permanently-unresolvable transcript, or a
  dead-or-idle-forever recipient with an empty window (state (b)), stays **visibly open**
  (PENDING) this cycle by design. The last-resort age-bounded closer is a C6
  `recovery_coordinator` policy decision, decided with the consumer in hand. An ACP send
  left recoverable also lands here — `qd delivery:recover` does not sweep ACP
  (`recover.rs:240-243`).
- **The relay receive-path fix** (mux-less inbound wake, `01KX7NV75W`) — HELD, out of
  scope. §7 documents only the honest *reading*, not a fix.
- **codex conformance** — deferred (dies on brano).
- **pi LIVE conformance** — deferred.
- **ACP `No → seen-failed` re-enable** — deferred (needs the ACP bridge on a box to pin
  write-ordering, §3.1).

---

## 7. Relay-lane mux-less receipt honesty (report `01KX8SRNG9`)

**The honest reading.** A mux-less (`zmx:-`) relay session emits `relay-delivered` but
never `message-seen` (no recipient-side observer runs to pull the message into context).

- **`relay-delivered` is NOT the receipt.** It is non-terminal (§1.1). A resolver that
  sees `relay-delivered` and no `message-seen` reads the send as **PENDING /
  not-delivered** — never "delivered." The ledger does **not** lie.

**Assessment (a) — is the CURRENT code ledger-honest? YES.** `relay-delivered` is
non-terminal by construction: it is absent from `TERMINAL_EVENTS` (`events.rs:106-123`)
and `is_terminal("relay-delivered")` is asserted false at `events.rs:2293`. The only
terminal success on the relay lane is `message-seen`, emitted by the recipient-side
observer (`relay_server/mod.rs:994`). With no observer (mux-less), no `message-seen` is
ever written, so a resolver correctly reads PENDING. **No false "delivered."**

**Assessment (b) — does `qd send:relay --wait` block forever? NO (bounded), but there is
a diagnosability gap.** Tracing the send path (`send_relay.rs`):
- `--wait` is a **relay-reply** concept, not a receipt wait (`send_relay.rs:69`). After
  emitting `send-initiated` + `relay-delivered` (`:155`), the `--wait` path calls
  `wait_for_reply` (`:165`), which long-polls `fetch_reply` for a **peer's reply body**
  with a default **120s timeout** (`:28`, `:1449`).
- On timeout it prints `"Timed out waiting for reply."` and **exits 1** (`:1474-1479`) —
  it does **not** block forever.
- **The gap:** `--wait` never consults `message-seen` at all. A mux-less recipient that
  never wakes produces no reply, so `--wait` times out (exit 1, "no reply"). That exit
  conflates *"delivered but the recipient chose not to reply"* with *"never delivered
  (mux-less, no observer)"*. The sender learns "no reply," not "delivery failed." So the
  `--wait` sender does **not** surface a delivery failure — but it also never falsely
  claims 'delivered' (exit 1, not 0). This is a **diagnosability** gap, not a
  blocks-forever bug and not a ledger lie.

**Recommendation: FAST-FOLLOW, not a D4 fix.** (a) is already honest — no change needed.
(b) is a real but bounded gap: `--wait` cannot distinguish a silent-but-delivered
recipient from an undelivered mux-less one. Surfacing that (e.g. a `--wait` that also
checks for `message-seen`, or a mux-less-recipient warning at send time) is a **new
report / fast-follow**, out of D4's documentation scope. The receive-path fix itself
(mux-less inbound wake) is separately HELD (`01KX7NV75W`, §6). **This doc does not fix
either — it documents the honest reading only.**

---

## 8. D0 cross-machine note (single-machine artifacts)

Verbatim from the D0 finding (`cc-delivery/D0-mover-foldin-finding.md`):

> Frame's cross-machine mover fold-in **cannot** re-interpret delivery-event
> truth-values, because it **never carries them**: the mover replicates only frame's own
> obligation events (`Message`/`Call`/`Respond`/`Note`/`Transfer`), and **no** path —
> neither frame's mover nor dispatch's own archive — replicates dispatch's delivery log
> (`state/sessions/*.events.jsonl`) cross-machine at all. The receipts are single-machine
> artifacts; there is no replicated delivery log for a fold to re-key, relabel, or
> re-interpret.

**C6 gap (DEFERRED).** A cross-machine obligation send fires its `qd send` on the **peer**
machine, so its receipt lands in the peer's un-replicated delivery log; a local-log
adoption (C6) would be blind to it unless surfaced via a replicated frame obligation
event. This is a **C6/D3** item — nothing to change here.

---

## 9. Provenance

- Code arbiter: `crates/dispatch/src/` on branch `qd-hardening/delivery` (post-D1/D2,
  R6 landed). Every `file:line` re-verified at current source.
- Wire schema: [`EVENT-CONTRACT.md`](./EVENT-CONTRACT.md) §1 (delivery family) + §1.5
  (3-phase delivery contract).
- Rulings: Pete's human ruling `01KX8NA7HB` (content-found = delivered); NoRecord
  terminus `01KX8MDPDX` (4-state recovery split); ACP post-inject `01KX8MP43N`.
- Reconciliation references (pre-fix `65e33c90`, verified against current code):
  `d1-coord/crate-wide-terminal-emission-map.md`, `scratch/qd-d1-g/door-inventory-v5.md`.
