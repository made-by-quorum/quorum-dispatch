# Dispatch transport formats — the qd–qf transition (v1)

Status: **draft 3** (R14 normalization: `created_at`, fully-normalized event
rows, `refused` replaces `accepted`, the discriminated-union event schema — on
top of the R8/R8a/R8b event model + R9/R10/R11 naming and summary fold),
2026-08-08. Author: build lead (session on brano), from
`ws/quorum/qd-qf/TRANSITION.md` §§1–3 + `provider-contract.md` §4 + Annex A.
Design authority: session `qdqf-why` (succeeded `qfd-qbt`; rulings in
`ws/quorum/qd-qf/RULINGS.md`).

"**Formats are contracts**" (TRANSITION §1): each file gets a JSON-Schema-first
contract — framing, field semantics, ordering guarantee, torn-tail rule, version
marker. The Rust structs (crate `quorum-dispositions`) are reflections of these
schemas, not the other way round. Under R8/R8a/R8b + R14, `dispositions.jsonl`
is an **append-only log of typed events** (§2, fully normalized per N13); state
is a **view** over it,
published as the **emitted summary record** (§3a) — the one data shape frame's
simple views project over — with the raw event funnel available via `--events`
(§3b). Both emitted shapes are **versioned with the provider contract**.

Common framing for every `*.jsonl` file here:
- **JSONL**: one JSON object per line, UTF-8, `\n`-terminated (LF, never CRLF).
- **Append-only**, single writer = qd (Annex A / Amendment 1). qd is the sole
  writer of `log.jsonl` and `dispositions.jsonl` forever. `remote/<host>/*` are
  pipeline-written replicas (out of scope here; read-only to qd).
- **Ordering guarantee**: file order == this host's *recorded* order (the order
  qd wrote things — N10). No other ordering is promised or load-bearing;
  cross-host order is never inferred from timestamps.
- **Fully normalized (N13, R14.0)**: we never denormalize, and never put a view
  inside the data model. No row copies a field for "self-containment," no row
  carries a provenance column — **provenance is the container the row lives in**
  (a local file ⇒ this host; a mirror at `remote/<host>/` ⇒ that host). An
  emitter MAY attach a computed `source` column at union/emission (a view
  concern, never storage). Honest `null` over copied presence, always. The data
  model is normalized; you build views over it.
- **Torn-tail rule**: a reader tolerates a final unterminated (partial) line by
  ignoring it. An unparseable *interior* line is corruption (counted, never
  silently treated as absence-of-record). A **blank** (all-whitespace) interior
  line is NOT corruption — it is skipped (it carries no record to lose); this is
  the reader half of the self-delimiting append framing below.
- **Self-delimiting torn-safe append framing** (audit follow-up #3a): qd writes
  each record as a SINGLE `O_APPEND` `write_all` of `\n{line}\n` — a LEADING
  newline as well as the trailing one. The leading `\n` unconditionally closes
  any torn prefix a prior interrupted write may have left (a crash MID the write
  of a `>PIPE_BUF` body, ENOSPC, EIO), so a new complete record can never FUSE
  onto a torn tail: loss is bounded to the torn record ALONE, never the following
  complete record (the delivered-fact-loss guarantee). On a clean/empty tail the
  leading `\n` yields a harmless blank line, which the reader skips (per the
  torn-tail rule above). Precedent: telemetry.rs `append_observed_self_delimited`
  (F-DEOBS-1).
- **Version marker**: every row carries `"v": 1`. A reader rejecting an unknown
  `v` refuses with a named reason; it never guesses.
- **Timestamps** are epoch-milliseconds **integers** (`i64`), UTC. Integers, not
  ISO-8601 strings: unambiguous, TZ-free, and directly range-comparable in the
  DuckDB projection frame runs over the emitted records (§3). (Precedent: the
  delivery-events envelope's `start_ms`.)

Files (all directly under `~/.quorum/dispatch/` = `qd_home`, **not** under
`state/`):

```
log.jsonl                   §1  every envelope qd ORIGINATED (write-then-deliver)
log.archive.jsonl           §1  archive tier (v2 tiering; born absent, additive)
dispositions.jsonl          §2  typed event log THIS qd authored (normalized)
dispositions.archive.jsonl  §2  archive tier (v2)
remote/<host>/log.jsonl         peer's replicated log        (mover-written)
remote/<host>/dispositions.jsonl peer's replicated dispositions (mover-written)
remote/<host>/ls.json            peer's session snapshot      (mover-written)
ls.json                         own session snapshot, published for peers
```

Migration: `log.jsonl` and `dispositions.jsonl` are **born empty at upgrade** —
no backfill (TRANSITION §6). The archive siblings are born absent; tiering is
v2 and purely additive (never built in this pass — the only v1 obligation is
"never delete anything," which needs no code).

---

## 1 · `log.jsonl` — qd's event source (envelopes qd originated)

The operational record of what qd **MOVED** (never what was *said* — that is
frame's ledger). One row per envelope qd **originated**, in **origin mode**
(`qd send <target> <body>`). **Inbound mode does NOT append here** — a peer's
envelope lives in the mirror (`remote/<host>/log.jsonl`), never in my own log
(TRANSITION §3, Annex A "THE ONE DOOR").

Write-then-deliver: the row is appended **before** the delivery attempt, so the
durable envelope backs resume-and-deliver's active `pending` (Amendment 2).

### Row schema

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "qd log.jsonl row (envelope, v1)",
  "type": "object",
  "required": ["v", "correlation_id", "authored_at", "expires_at", "target", "body", "origin", "sender"],
  "additionalProperties": false,
  "properties": {
    "v":              { "const": 1 },
    "correlation_id": { "type": "string", "minLength": 1,
                        "description": "The envelope id — minted EXACTLY ONCE at origin by the originating program's append: frame's ledger event id when frame originates (riding as correlation_id), else qd's own log ULID for bare sends. Traveling verbatim; the idempotency + disposition key. Never a content hash. The name is RULED (R9.1): an event log holds many rows per envelope — the field correlates rows, it identifies none; contract §4 names it." },
    "authored_at":    { "type": "integer",
                        "description": "epoch-ms, stamped once at origin (derived from the ULID clock when qd mints; otherwise the origin-append time). N10 authored timeline." },
    "expires_at":     { "type": "integer",
                        "description": "epoch-ms. Default authored_at + 12h; overridable per send (--expires). `expired` is minted from ABSENCE past this value. Policy travels with the message; there is no global rolling cutoff." },
    "target":         { "type": "string", "minLength": 1,
                        "description": "The address as given by the caller (name | stable_id | name@host), RAW (R9.4): a parsed-out target_host at write time is derived state materialized into the log; views split on `@` at query time. Operational record; resolution happens at delivery. Not load-bearing for the disposition join (that keys on correlation_id)." },
    "body":           { "type": "string",
                        "description": "The opaque prose, verbatim, delivered as one message. qd never parses it. Persisted so write-then-deliver / resume-and-deliver can redeliver." },
    "origin":         { "type": "string", "minLength": 1,
                        "description": "Origin host id (this qd's host) — named for its N10 role (R9.2). On a single machine this is the local host id; disambiguates origin when a peer's log is read from remote/<host>/." },
    "sender":         { "type": ["string", "null"], "minLength": 1,
                        "description": "The AGENT SESSION that invoked qd, as its QD_SESSION_ID, RAW — or null when the caller carried none (a human in a shell, a cron). `origin` says which HOST authored; `sender` says which session on it. RAW on the same R9.4 ground as `target`: a folded idstore lookup at write time would materialize derived state into the log, and a fold that fails to resolve would anonymize a send that WAS attributable — a log records what the caller carried and lets views resolve. Unlike a human-typed `target` the id has exactly one spelling (injected verbatim at session create), so raw IS the stable id. Never inferred: absence is recorded as null, not guessed from adjacency." }
  }
}
```

Key order on the wire (byte-exact, `preserve_order`): `v, correlation_id,
authored_at, expires_at, target, origin, sender, body`. (`body` last: it is the
largest and most variable field; keeping it last keeps the head of every line
cheap to scan.)

`sender` is **born nullable, never backfilled**, and is the one field a v1 reader
must tolerate MISSING: rows written before it existed — and envelopes arriving
from a peer running an older qd — carry no `sender` key at all, and are read as
`null` (unattributed), NOT as corruption. That absence-tolerance is deliberate
and narrow: every other §1 key stays strictly required, and an old reader meeting
a new row simply ignores the key it does not know. Because the field is emitted
even when null, the row's key set is identical either way.

---

## 2 · `dispositions.jsonl` — the typed event log (R8/R8a/R8b, R14-normalized)

An **append-only log of typed EVENTS** — one row per recorded moment in an
envelope's life at this host. **Never a state record.** File order == this host's
recorded order. State is not stored; it is a **view** computed over this log
(§3). "First terminal wins" is dead: a `delivery-failed` row no longer resolves
an id to FAILED forever, and idempotence keys on a `delivered` **event
existing**, never on "any terminal" (R8 — the bug fix; a failed@t1 → delivered@t3
retry now correctly resolves *delivered*).

Event rows are **fully normalized (N13, R14.2)**: each row is
`{v, correlation_id, event, created_at}` plus, on the two non-success variants
only, a required machine `class`. There is **no `witness`, no copied `origin`, no
copied `authored_at`** on an event row — the pre-R14 copies were denormalization
(R11.3 superseded). Provenance is the container (§ framing / N13); `origin` and
`authored_at` live once, on the §1 envelope, and events **join** to it by
`correlation_id`. This is why the envelope is a standalone traveling record but
event rows are not.

**The five event types** (each past tense — a moment qd recorded; each its own
row schema — a DISCRIMINATED UNION, R14.5 — not a value of a shared `outcome`
field, R8a). Identical payload shapes today are fine and may diverge later. The
set is **open**: future recorded facts arrive as NEW types, never as new outcome
values.

| `event`            | recorded moment | `class` |
|---|---|---|
| `attempted`        | a delivery attempt was ADMITTED and STARTED (each retry = a fresh `attempted`; `attempted` marks admission-and-start — R14.3 retires the old `accepted`) | — |
| `queued`           | the attempt placed the message into the target's delivery queue / awaiting idle or wake (a recorded moment, possibly minutes before landing — busy or waking session) | — |
| `delivered`        | the prose LANDED in the session — existence of this row IS the irreversible delivered fact; carries a **required `body_digest`** (R15 — the integrity binding of what content landed) | — (has `body_digest`) |
| `delivery-failed`  | the attempt definitively did not arrive | **required** |
| `refused`          | a parse-valid **inbound** door / pre-flight refusal — mis-addressed, past-expiry, ambiguous, no-live-receive-path, body-mismatch (R15); the refusal class rides IN the funnel now (not stderr-only). **PENDING-class in the fold, never `failed`** (refused = never left ≠ failed) | **required** |

**Guard — only recorded moments get events.** qd emits an event when it records
the thing HAPPEN, never a speculative/planned state. "Will retry at X" stays
view-computed (`next_attempt` is policy, §3 / R8b guard 1); there is no
`pending`/`expired` row here (both are *absence*-derived at the query surface,
§3), and no `outcome` field anywhere.

**Pre-flight refusals stamp a `refused` event (R14.3, supersedes R12's lone
`accepted`).** Every parse-valid **inbound** door / pre-flight refusal — a
mis-addressed / past-expiry / ambiguous envelope, or an admitted envelope whose
carrier selection then finds no live receive path — stamps an explicit
`refused{class}` row. No `attempted` is fabricated for an attempt that never
started, and no `delivery-failed` (the contract's families hold: refused = never
left, pre-flight sync; failed = attempted and definitively did not arrive). The
"a disposition is owed" obligation is met by the refused → pending → expired
ABSENCE path (refused folds pending-class, §3a). Origin-mode SYNC refusals stay
row-less — the interactive caller is present and the refusal rides its stderr,
as does a MALFORMED envelope with no trustworthy `correlation_id` (nothing to
key a row on); widening origin-mode refusals to rows is a future ruling.

**Per-variant tail invariant** (the discriminated union, tighter than any shared
enum — R14.5, extended R15): `class` is REQUIRED on `delivery-failed` AND
`refused` (the machine-readable failure/refusal class, e.g. `"wake"` /
`"no-live-receive-path"` / `"body-mismatch"` — the contract §6 `{class}` family;
`failed{wake}` = `delivery-failed` class `"wake"`) and FORBIDDEN elsewhere;
`body_digest` is REQUIRED on `delivered` (R15) and FORBIDDEN elsewhere. So each
type carries exactly one tail (`delivered`→`body_digest`,
`delivery-failed`/`refused`→`class`, `attempted`/`queued`→none). A row missing
its required tail, carrying a foreign tail (a plain type with `class`, a
non-delivered type with `body_digest`, …), or carrying **any** field foreign to
its variant, is **corrupt** (counted, never returned — the reader enforces the
union shape per type). A human detail field named `reason` is **RESERVED**
(optional, any variant) but UNUSED in v1 — not emitted, and a row carrying it
today is corrupt.

**R15 — the id binds ONE body (Contract Amendment 6).** `correlation_id` is minted
once at origin by one append, so one id IS one act of authorship with one body; no
legitimate flow produces same-id/different-body (a retry re-presents the IDENTICAL
envelope; movers re-read the same row). The `delivered` event therefore binds a
`body_digest` (hex sha-256 of the parsed body), and the door — under a
per-`correlation_id` claim lock spanning check→deliver→stamp — refuses a
conflicting presentation: **inbound** a same-id/different-body presentation stamps
`refused{body-mismatch}` (funnel row, exit 12); **origin** a duplicate submit with
the same body does NOT double-append the envelope (a caller retry redelivers), and
a different body is a SYNC row-less `refused{body-mismatch}`. An identical-body
replay is a no-op success either way. This is DETECTION, not prevention (an
attacker who presents first still lands their body; the legit presentation then
refuses `body-mismatch` — which IS the alarm); prevention needs envelope
authenticity (signing / the host-identity tree), out of this pass.

### Row schema (one event — a discriminated union keyed on `event`)

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "qd dispositions.jsonl row (event, v1)",
  "type": "object",
  "required": ["v", "correlation_id", "event", "created_at"],
  "additionalProperties": false,
  "properties": {
    "v":              { "const": 1 },
    "correlation_id": { "type": "string", "minLength": 1,
                        "description": "The envelope's origin-minted id — the join key to the log envelope and the correlation key across an event's whole funnel. Named `correlation_id` (RULED R9.1/R14.4) because an event log holds MANY rows per envelope: the value correlates rows across systems (frame ledger ↔ qd log ↔ event funnel ↔ replicas), it identifies no single row. This is the distributed-tracing/messaging correlation-id convention, used to spec — `id` would falsely claim row-identity; `ulid` would over-promise a format the contract does not require of frame-origin ids." },
    "event":          { "enum": ["attempted", "queued", "delivered", "delivery-failed", "refused"],
                        "description": "Which recorded moment this row records — the union discriminant. Past tense; the type set is open (new recorded facts are new types, never new values of an outcome field)." },
    "created_at":     { "type": "integer",
                        "description": "epoch-ms, when THIS host recorded the event (R14.1). Effects order — and the view's latest-event pick — key on this (N10 / Amendment 1). For OUTCOME events this is OBSERVATION time: a delivery that truly landed at 12:01 but was recorded at 12:02 is a `delivered` event `created_at` 12:02 — there is no retro-dating, and `created_at` is honest about what qd knew and when. (For qd-driven events — attempted/queued — record time and happen time coincide to within ms, with ONE stated exception: `queued` on the origin resume-and-deliver path is stamped AFTER the wake resolves, not before it is tried, so its `created_at` trails the moment the message was placed awaiting the wake by the whole duration of the revive. The funnel ORDER is unchanged. Cause: `LaneOps::deliver` is atomic over the wake, so qd learns a wake happened only from the lane's answer — see `quorum_qw::contract::Receipt::woke` and `send_unified::wake_then_deliver`.)" },
    "class":          { "type": "string", "minLength": 1,
                        "description": "The machine-readable failure/refusal class. REQUIRED on `delivery-failed` (e.g. \"wake\") and `refused` (e.g. \"no-live-receive-path\", \"body-mismatch\"); FORBIDDEN on the other three types. Part of the {class} failure/refusal family (contract §6). Omitted from the wire when absent." },
    "body_digest":    { "type": "string", "minLength": 1,
                        "description": "R15 (Contract Amendment 6): the lowercase-hex SHA-256 of the envelope's PARSED `body` string (its UTF-8 bytes — NOT the file line bytes, so transport trailing-newline trimming can never fabricate a mismatch on a legit retry). REQUIRED on `delivered`, FORBIDDEN on the other four types. The integrity binding of the delivery act — what content landed at this host; the door refuses a same-id/different-body presentation as `refused{body-mismatch}`. Omitted from the wire when absent." }
  },
  "oneOf": [
    { "properties": { "event": { "const": "attempted" } },       "required": ["v", "correlation_id", "event", "created_at"],                 "not": { "anyOf": [ { "required": ["class"] }, { "required": ["body_digest"] } ] } },
    { "properties": { "event": { "const": "queued" } },          "required": ["v", "correlation_id", "event", "created_at"],                 "not": { "anyOf": [ { "required": ["class"] }, { "required": ["body_digest"] } ] } },
    { "properties": { "event": { "const": "delivered" } },       "required": ["v", "correlation_id", "event", "created_at", "body_digest"], "not": { "required": ["class"] } },
    { "properties": { "event": { "const": "delivery-failed" } }, "required": ["v", "correlation_id", "event", "created_at", "class"],       "not": { "required": ["body_digest"] } },
    { "properties": { "event": { "const": "refused" } },         "required": ["v", "correlation_id", "event", "created_at", "class"],       "not": { "required": ["body_digest"] } }
  ]
}
```

Key order on the wire: `v, correlation_id, event, created_at` then the
per-variant tail (`class` on `delivery-failed` / `refused`; `body_digest` on
`delivered` — R15; omitted entirely on `attempted` / `queued`). `v` is first
(version-marker convention); `event` is
third (matching the §1 envelope).

**Normalization note (N13, R14.2).** An event row carries ONLY its own facts —
`{v, correlation_id, event, created_at}` (+ `class`). `witness`,
`origin`, and `authored_at` are **gone** from event rows: they were copied for
"self-containment," which is denormalization. Provenance is the CONTAINER the row
lives in (a local file ⇒ this host; a mirror at `remote/<host>/` ⇒ that host);
an emitter MAY attach a computed `source` column at union/emission (a view
concern — the `(created_at, source)` union comparator lives there, §3a note 2),
never storage. The `origin`/`authored_at` timeline lives once on the §1 envelope;
a view **joins** by `correlation_id` to attach it. Event rows deliberately carry
**no `expires_at`** either (ruled): the door refuses past-expiry presentations;
an orphan's expiry status stays a documented degenerate analytics case (§3a note
3), not a schema driver. `created_at` is the sole timeline the row owns —
observation time for outcome events (above), record time for qd-driven ones.

---

## 3 · The emitted disposition output — `qd dispositions` (published, versioned with the contract)

`qd dispositions` publishes **two** shapes, both JSONL on stdout, both versioned
with the provider contract. Both are **stateless, caller-windowed projections**:
the caller brings the window; qd stores no read-state, no cursors, ever (N2).
Both are computed fresh at query time over `log.jsonl` ∪ `dispositions.jsonl`
(and their `remote/<host>/` replicas under `--host`/`--all`, `*.archive.jsonl`
under `--archive`).

- **DEFAULT — the summary record** (§3a): one row per `correlation_id`, carrying
  the coarse 4-state view + folded analytics. **This is the one data shape
  frame's simple views project over** — the 4-state enum is UNCHANGED (R8b guard
  2), so those views stay stable.
- **`--events` — the raw event record** (§3b): the funnel itself — every
  [§2 event](#2--dispositionsjsonl--the-typed-event-log-r8r8ar8b-r14-normalized),
  emitted verbatim (published/versioned), for history/analytics views
  (attempts-before-landing, queue→delivered latency, wake-latency — R8b payoff).

**State is a view, always.** The summary's `state` is DERIVED from the event
log; nothing stores it. Idempotence and the delivered-view both key on a
`delivered` **event existing** (R8), never "any terminal."

### 3a · The summary record (DEFAULT)

Folded per `correlation_id` over the event log ∪ the envelope:

- `state` — the coarse 4-state view (below).
- `attempts` — count of `attempted` events.
- `last_event` — the event MAX by `created_at` (later-in-input wins full ties;
  note 2 below) — detail beneath `state` without widening the enum (R8b guard
  2). **`null` iff no events exist** (R11.1).
- `last_attempt_at` — max `created_at` over `attempted` (null if none).
- `first_delivered_at` — min `created_at` over `delivered` (null if none).
- `expires_at` — from the joined envelope (**`null`** when the envelope is out
  of scope — an orphan-event summary).
- `authored_at` — from the joined envelope ONLY (**`null`** for an orphan-event
  summary — R14.2 honest null; events no longer carry `authored_at`).
- `origin` — from the joined envelope's `origin` ONLY (**`null`** for an
  orphan-event summary — R14.2 honest null; events no longer carry `origin`, the
  copy was denormalization).

**No `witness` column (R14.2).** The summary carries no witness field — event
rows no longer carry `witness` (normalized away, N13); provenance is the
container, and a view MAY attach a computed `source` column at emission (nothing
consumes it today — additive if earned). `last_event` is `null` iff no events
exist (R11.1 core survives).

**Honest-null rule (R14.2, supersedes R11's copy-from-event).** `origin`,
`authored_at`, and `expires_at` come ONLY from the joined envelope. An
orphan-event summary (events in scope, envelope in an un-unioned mirror) is
therefore `null` across all three — no more copy-from-first-event. The join
fills them when the mirror unions in. `last_event` `null` iff no events (a
summary never reports a moment nobody recorded — the old fabricated-`accepted`
default is dead).

**State precedence** (**RATIFIED — R10**; isolated in a single `derive_state`
fn in the crate so it stays auditable in one place):

1. `delivered` — a `delivered` event exists: the only absorbing state
   (irreversible; wins over expiry and any later failure).
2. `expired`   — no delivered event AND `now >= expires_at` — expired > failed
   is the contract transported: `delivery-failed` is not terminal under the
   retry model; expired = no delivered event by the envelope's own `expires_at`,
   failure history or none. (Only derivable where the envelope/mirror is in
   scope, so an orphan-event summary — no envelope — is **never** `expired`.)
3. `failed`    — no delivered event, not expired, and `last_event` is
   `delivery-failed` — awaiting retry.
4. `pending`   — otherwise (latest is `attempted`/`queued`/**`refused`**, or none
   — an envelope with no event yet). **`refused` folds PENDING-class** (refused =
   never left ≠ failed; R14.3). "Silence is pending, never success."

Multi-source resolution simplifies (R8): `delivered`-exists **anywhere** wins
(deterministic, no tie-break — delivery is an irreversible fact); attempt
histories from different sources UNION harmlessly into the counts.

**Consumer notes:**

1. **`failed` is NOT absorbing.** Across retries a row moves failed → pending →
   failed → …; `delivered` is the only absorbing state; `failed` → `expired` at
   `expires_at`. Frame alert views must predicate on undelivered-past-N or
   attempts ≥ K (view policy), NEVER on "reached failed once" — the old
   first-terminal-wins habit is dead. (R10 delta 2.)
2. **Determinism.** `last_event` orders by `(created_at, source)` — within one
   file, file order IS recorded order (at equal `created_at` the later row wins;
   the ms timestamp is a lossy projection of append order). Event rows carry no
   `source` (N13); across a multi-source union the READER supplies determinism —
   it concatenates per-source rows in sorted-host order (so input order encodes
   source order) or attaches a computed `source` for a true `(created_at, source,
   input-index)` comparator — before the leaf fold. That cross-source
   order-invariance is a view/emitter concern, tested in the dispatch layer; the
   pure leaf fold sees a flat, already-ordered slice and keys on `created_at` +
   input order. (R14.2 relayers R11.2.)
3. An envelope-out-of-scope summary (events in scope, envelope in an un-unioned
   mirror) has `expires_at: null` (and `origin: null`, `authored_at: null` —
   note above) ⇒ `expired` is underivable there; the door's past-expiry refusal
   bounds the exposure. (R10 note b, extended by R14.2.)

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "qd disposition summary record (emitted / published, v1)",
  "type": "object",
  "required": ["v", "correlation_id", "state", "attempts", "last_event", "last_attempt_at", "first_delivered_at", "expires_at", "authored_at", "origin"],
  "additionalProperties": false,
  "properties": {
    "v":                  { "const": 1 },
    "correlation_id":     { "type": "string", "minLength": 1 },
    "state":              { "enum": ["pending", "delivered", "failed", "expired"],
                            "description": "The coarse 4-state view (UNCHANGED, R8b guard 2). delivered = a delivered event exists; expired = no delivery past expires_at; failed = latest event is delivery-failed, pre-expiry; pending = otherwise (incl. refused — pending-class, R14.3). Silence is pending, never success. Precedence RATIFIED (R10)." },
    "attempts":           { "type": "integer", "minimum": 0,
                            "description": "Count of `attempted` events for this id." },
    "last_event":         { "enum": ["attempted", "queued", "delivered", "delivery-failed", "refused", null],
                            "description": "The event MAX by created_at (later-in-input wins full ties within a source; determinism note 2) — detail beneath `state`. null iff no events exist (R11.1). Stable column." },
    "last_attempt_at":    { "type": ["integer", "null"],
                            "description": "epoch-ms, max created_at over `attempted`; null if none. Stable column." },
    "first_delivered_at": { "type": ["integer", "null"],
                            "description": "epoch-ms, min created_at over `delivered`; null if none. Stable column." },
    "expires_at":         { "type": ["integer", "null"],
                            "description": "epoch-ms from the joined envelope; null when the envelope is out of scope (orphan-event summary — R14.2 honest null). Stable column." },
    "authored_at":        { "type": ["integer", "null"],
                            "description": "epoch-ms origin timeline, from the joined envelope ONLY; null for an orphan-event summary (R14.2 — events no longer carry authored_at). Stable column." },
    "origin":             { "type": ["string", "null"],
                            "description": "The origin host id, from the joined envelope's `origin` ONLY; null for an orphan-event summary (R14.2 — events no longer carry origin, the copy was denormalization). Stable column." }
  }
}
```

Key order on the wire: `v, correlation_id, state, attempts, last_event,
last_attempt_at, first_delivered_at, expires_at, authored_at, origin`.
The nullable columns (`last_event`, `last_attempt_at`, `first_delivered_at`,
`expires_at`, `authored_at`, `origin`) are present as `null` when absent —
**stable columns** for the DuckDB projection, never skipped.

### 3b · The raw event record (`--events`)

`qd dispositions --events` emits the §2 events verbatim — the same discriminated-
union row schema, published and versioned with this contract (key order `v,
correlation_id, event, created_at, class`; `class` present only on
`delivery-failed` and `refused`, omitted on the three plain types). This is the
funnel history/analytics views project over; the DEFAULT summary (§3a) is the
fold of exactly these rows.

### 3c · The per-session view (`qd messages <session>`)

`qd messages` publishes the SAME two files under a different key: the §1
envelope ⟕ its §3a summary, filtered to one session by the envelope's `target`,
in `authored_at` order. One JSONL row per message (`--json`, and the default for
a pipe or an agent caller): the envelope's fields — `v, correlation_id,
authored_at, expires_at, target, origin` — then the disposition's — `state,
attempts, last_event, last_attempt_at, first_delivered_at` — then `body` last.

Two consequences of the schema, load-bearing for anyone reading such a report:

- The view is **envelope-rooted**, so an orphan-event id (events in scope whose
  envelope is not) is ABSENT — R14.2 normalized `target` off the event row, so
  an orphan cannot be attributed to any session. Those ids remain published by
  `qd dispositions`, which is keyed by id and owes no target.
- It reports the **addressed** side only. `origin` is the origin HOST (R9.2);
  no field records the sending SESSION, so "what did this session send" is not
  a question these files can answer. Nor is a relay reply visible: qd is the
  sole writer here (§ single-writer law), and the relay server's reply path
  appends no envelope.

`--as-of` note (frame-side, §5.2 of TRANSITION): dispositions carry **no ledger
ord** — they are the live overlay. Frame registers this table fresh each
evaluation and must **not** apply `--as-of` time-travel to it.

**DuckDB consumer note (the `/dev/stdin` trap).** `qd dispositions` emits these
records as JSONL (one object per line). When piping them into DuckDB, read with
**`read_ndjson_auto('/dev/stdin')`** (or `read_json_auto('/dev/stdin',
format='newline_delimited')`). Do **NOT** use bare
`read_json_auto('/dev/stdin')`: on a pipe it cannot sample to infer the schema
and collapses the whole stream into a single `json` column (a
`Binder Error: … column not found`). Verified on brano (duckdb 1.5.x). This is
the join the up-projection runs to correlate the disposition rows back to the
ledger by `correlation_id`, so the consumer form is load-bearing.

---

## 4 · `ls.json` — a host's session snapshot (published for peers, mirror-read)

A **whole-document JSON** snapshot of a host's `qd ls --json` rows, wrapped with
the host id + the instant the snapshot was taken. `ls.json` is how the fleet
answers "what sessions does host *h* have?" without a live cross-host call: the
mover writes each host's snapshot into every peer's `remote/<host>/ls.json`, and
`qd ls --host <h>` / `qd ls --all` READ those replicas, **always** surfacing the
mirror's **staleness** (`now − witnessed_at`) so a dead replication pipeline is
visible at the surface you look at.

Unlike §1–§3 this is a **single JSON document**, not JSONL (it mirrors the
`qd ls --json` array shape, which is itself one pretty-printed array). It is
NOT part of the disposition-record contract frame projects over; it is an
operational fleet-visibility surface.

**Writer / reader split (READ-ONLY for `qd ls`):** `remote/<host>/ls.json` is
**mover-written** (out of scope here), exactly like `remote/<host>/log.jsonl` and
`remote/<host>/dispositions.jsonl`. The mover runs `qd ls --json`, wraps the rows
with `host` + `witnessed_at`, and writes the result to peers'
`remote/<myhost>/ls.json`. **`qd ls` does NOT write its own `ls.json`** — the
`ls.json` (own snapshot, published) row in the file table above is the mover's
output, produced by *reading* `qd ls --json`, not by `qd ls` itself. W7
implements only the DEFINE-and-READ half.

### Document schema

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "qd ls.json session snapshot (v1)",
  "type": "object",
  "required": ["v", "host", "witnessed_at", "sessions"],
  "additionalProperties": false,
  "properties": {
    "v":            { "const": 1 },
    "host":         { "type": "string", "minLength": 1,
                      "description": "The host id this snapshot describes (the peer's local_authority). Disambiguates origin when the file is read from remote/<host>/." },
    "witnessed_at": { "type": "integer",
                      "description": "epoch-ms, when the snapshot was TAKEN (the mover's `qd ls --json` run time). Staleness = now − witnessed_at; an old value is a stalled/dead replication pipeline, made visible at every read." },
    "sessions":     { "type": "array",
                      "items": { "type": "object" },
                      "description": "The host's `qd ls --json` rows, verbatim (the same per-session object shape `qd ls --json` emits: name/sessionId/qdId/status/pid/provider/…). Carried opaquely — a peer's row-schema evolution never breaks the reader." }
  }
}
```

`v` / `host` are validated on read; a missing/`v != 1` / non-object / torn file
⇒ a **named refusal**, never a panic (the whole-document sibling of the JSONL
torn-tail rule: a mirror we cannot trust is refused with a reason, never silently
treated as absence-of-rows).

### How `qd ls` READS it (staleness always surfaced)

- **`qd ls --host <h>`** reads `remote/<h>/ls.json`.
  - ABSENT ⇒ `refused{no-fleet-state}` exit 12 (the single-machine contract,
    consistent with `qd send --host`: a host-qualified read with no fleet state
    for that host refuses with a named reason; bare/local is unaffected).
  - Torn / `v != 1` ⇒ `refused{torn-mirror}` exit 12.
  - Else prints the peer's rows, **always** annotated with the mirror's staleness.
    - Human: a header — `host <h> — mirror age 5m12s (witnessed <ISO-8601>)`.
    - `--json`: each row carries `host`, `mirror_witnessed_at` (epoch-ms), and
      `mirror_age_ms` (`now − witnessed_at`), so a DuckDB view can see a dead
      pipeline.
  - `--host` conflicts with `--all` (one host scope per query).
- **`qd ls --all`** keeps its existing LOCAL meaning (uncap the row limit +
  include cold tombstones) EXACTLY, then ADDITIVELY unions every peer's
  `remote/<host>/ls.json`, each peer's rows annotated with host + staleness. On a
  single machine with **no `remote/`**, the union is a no-op ⇒ `--all` is
  **byte-identical** to today. (Across peers `--all` is best-effort: a torn/absent
  per-host mirror is skipped with a stderr warning; `--host <h>` is the strict
  single-host read that refuses on a bad mirror.)
- **`qd ls --json`** is always computed fresh and piped into DuckDB (never
  cached); the local rows keep their existing shape (a superset — `host` /
  `mirror_*` columns are absent for local rows), so existing consumers are
  unbroken.

---

## Derivation & invariant cross-reference

| Rule | Source |
|---|---|
| `dispositions.jsonl` = append-only log of typed EVENTS, never state records; file order == recorded order | §2, R8/R8a/R8b |
| FULLY NORMALIZED event rows `{v, correlation_id, event, created_at}` (+ `class`); no `witness`/`origin`/`authored_at`; provenance = the container; `source` is a view column at union/emission, never storage (N13) | §2, R14.0/R14.2 (supersedes R9.3 witness-column, R11.3) |
| `witnessed_at` → `created_at` everywhere: when THIS host recorded the event; observation time for outcome events (delivered-at-12:01-recorded-at-12:02 ⇒ `created_at` 12:02, no retro-dating). DEVIATION, stated: `queued` on the origin wake path is now recorded after the wake resolves — R14.1 holds (no retro-dating), but record time and happen time no longer coincide there | §2, R14.1 (supersedes R9.3 witnessed_at) |
| five event types (attempted/queued/delivered/delivery-failed/refused), past tense, own row schema — a DISCRIMINATED UNION keyed on `event`; type set open (new facts = new types, not outcome values) | §2, R8a/R8b/R14.5 (R14.3 retires `accepted`) |
| only recorded moments get events; no speculative/planned rows; no `outcome` field | §2, R8b guard 1 |
| `attempted` = ADMITTED and STARTED; `refused` = a parse-valid INBOUND door / pre-flight refusal (mis-addressed / past-expiry / ambiguous / no-live-receive-path), carries `class`, folds PENDING-class (never `failed`); origin-mode + malformed refusals stay row-less on stderr | §2, R14.3 (supersedes R12 accepted-definition + lone-accepted breadcrumb) |
| `class` REQUIRED on `delivery-failed` AND `refused`, FORBIDDEN on the three plain types; any variant carrying a foreign field is corrupt (discriminated-union validation, type-system-enforced); `reason` reserved-but-unused | §2, R14.5 (supersedes R8a reason-invariant) |
| state is a VIEW, always; nothing stores it (summary derives it, §3a) | §3, R8 |
| idempotence + delivered-view key on a `delivered` EVENT existing, never "any terminal"; "first terminal wins" is DEAD | §3a, R8 (the bug fix) |
| summary `state` precedence delivered→expired→failed→pending (RATIFIED, isolated `derive_state`); `refused` folds pending-class | §3a, R10/R14.3 |
| `failed` is NOT absorbing: failed → pending → failed → … across retries; `delivered` is the only absorbing state; alert views predicate on undelivered-past-N / attempts ≥ K, never "reached failed once" | §3a note 1, R10 delta 2 |
| summary carries `last_event` for detail; the 4-state enum stays UNCHANGED; no `witness` column | §3a, R8b guard 2 / R14.2 |
| zero-events summary: `last_event` is `null` — never a fabricated `accepted`; `null` iff no events | §3a, R11.1 |
| `last_event` pick = MAX by `created_at`; full tie → later-in-input row (file order IS recorded order within one source); cross-source `(created_at, source)` determinism is the union reader's job (dispatch layer) | §3a note 2, R14.2 (relayers R11.2) |
| orphan-event summary (envelope out of scope) is `null` across `origin`/`authored_at`/`expires_at` — honest null, the join fills them when the mirror unions in; event rows carry NO `expires_at` | §2/§3a, R14.2 (supersedes R11.3 copy-from-event) |
| pending = no delivery pre-expiry, view-computed; never a row | §2/§3a, contract §4 "silence is pending" |
| expired = no delivery past envelope's own expires_at; view-computed; orphan-event (no envelope) is never expired | §2/§3a, TRANSITION §2, contract §4 |
| multi-source: delivered-exists anywhere wins (no tie-break); attempt histories union harmlessly | §3a, R8 |
| `qd dispositions` DEFAULT = summary; `--events` = the raw event funnel (both published/versioned) | §3, R8b |
| `qd messages <session>` = the SAME files keyed by target: envelope ⟕ summary, envelope-rooted (orphan events absent, they have no target), addressed-side only (no sender-session field, no relay-reply envelope) | §3c |
| correlation_id minted once at origin; verbatim; idempotency key | §1, TRANSITION identity crux, contract §4 |
| inbound mode: idempotent on id (delivered-exists), no local log append | §1/§2, Annex A THE ONE DOOR |
| no `posted` state | contract Annex A (ruled 2026-08-08) |
| no second-order "delivered" event; the `delivered` event IS the single copy of truth | N12, contract §4 |
| single writer qd for log + dispositions; mirrors mover-written | Amendment 1/3 |
| emitted records = stateless caller-windowed projection, no cursors | N2, contract §4 bulk form |
| field NAMING RULED: `origin` (origin host id) lives on the §1 envelope + §3a summary (from the join) — NOT on event rows (R14.2 normalized it away); `witness` RETIRED from event rows and summary (R14.2), reserved as a future cross-host observation EVENT TYPE (R14.1); `correlation_id` keeps its name (correlation convention, R14.4); `ls.json` §4 `host`/`witnessed_at` UNCHANGED (a different, already-landed surface) | R9 (superseded on the disposition surface by R14.1/R14.2) |
| `ls.json` mover-written; `qd ls` is READ-ONLY (defines + reads mirrors) | §4, TRANSITION §3 |
| `qd ls` staleness always surfaced (`now − witnessed_at`) | §4, TRANSITION §3 "dead pipeline visible" |
| `--all` = local (uncap + tombstones) + every peer mirror; no-`remote/` = byte-identical | §4, build-lead reconciliation |
| host-qualified `ls` with no mirror ⇒ refused{no-fleet-state} exit 12 | §4, consistent w/ `qd send --host` |
| resume-and-deliver: stopped ≠ refusal class; wake in the attempt; failed{wake} on unwakeable | TRANSITION §3, contract §4 P0 ruling |
| inbound door: mis-addressed / past-expiry / ambiguous / no-live-receive-path ⇒ a `refused{class}` event (parse-valid; past-expiry refused, never stamped `expired`); malformed (no trustworthy correlation_id) stays stderr-only | §2/§3, R14.3, Annex A THE ONE DOOR |

**Conformance (v1).** The §6 acceptance bar for this transport surface is
demonstrated end-to-end by `dispatch/crates/dispatch/tests/acceptance.rs` — the
log→disposition→`qd dispositions`→DuckDB-join round-trip (via `read_ndjson_auto`),
inbound-mode idempotence, and the named door refusals. Per the W8 assessment,
`acceptance.rs` *is* the conformance cell for the disposition surface in v1 (the
existing per-provider conformance battery covers the older per-session
`events.jsonl` transport, keyed by `send_id`/`content_sha256`, not this
`correlation_id`-keyed surface). A dedicated lane-agnostic "D8 DispositionContract"
dimension on the tier grid is a v2 option.
