# Dispatch transport formats — the qd–qf transition (v1)

Status: **draft 2** (R8/R8a/R8b event model + R9/R10/R11 naming and summary
fold), 2026-08-08. Author: build lead (session on brano), from
`ws/quorum/qd-qf/TRANSITION.md` §§1–3 + `provider-contract.md` §4 + Annex A.
Design authority: session `qdqf-why` (succeeded `qfd-qbt`; rulings in
`ws/quorum/qd-qf/RULINGS.md`).

"**Formats are contracts**" (TRANSITION §1): each file gets a JSON-Schema-first
contract — framing, field semantics, ordering guarantee, torn-tail rule, version
marker. The Rust structs (crate `quorum-dispositions`) are reflections of these
schemas, not the other way round. Under R8/R8a/R8b, `dispositions.jsonl` is an
**append-only log of typed witnessed events** (§2); state is a **view** over it,
published as the **emitted summary record** (§3a) — the one data shape frame's
simple views project over — with the raw event funnel available via `--events`
(§3b). Both emitted shapes are **versioned with the provider contract**.

Common framing for every `*.jsonl` file here:
- **JSONL**: one JSON object per line, UTF-8, `\n`-terminated (LF, never CRLF).
- **Append-only**, single writer = qd (Annex A / Amendment 1). qd is the sole
  writer of `log.jsonl` and `dispositions.jsonl` forever. `remote/<host>/*` are
  pipeline-written replicas (out of scope here; read-only to qd).
- **Ordering guarantee**: file order == this host's *witnessed* order (the order
  qd accepted things — N10). No other ordering is promised or load-bearing;
  cross-host order is never inferred from timestamps.
- **Torn-tail rule**: a reader tolerates a final unterminated (partial) line by
  ignoring it. An unparseable *interior* line is corruption (counted, never
  silently treated as absence-of-record).
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
dispositions.jsonl          §2  typed witnessed-event log THIS qd authored
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
  "required": ["v", "correlation_id", "authored_at", "expires_at", "target", "body", "origin"],
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
                        "description": "Origin host id (this qd's host) — named for its N10 role (R9.2). On a single machine this is the local host id; disambiguates origin when a peer's log is read from remote/<host>/." }
  }
}
```

Key order on the wire (byte-exact, `preserve_order`): `v, correlation_id,
authored_at, expires_at, target, origin, body`. (`body` last: it is the
largest and most variable field; keeping it last keeps the head of every line
cheap to scan.)

---

## 2 · `dispositions.jsonl` — the typed witnessed-event log (R8/R8a/R8b)

An **append-only log of typed, witnessed EVENTS** — one row per witnessed
moment in an envelope's life at this host. **Never a state record.** Witnessed
order == append order. State is not stored; it is a **view** computed over this
log (§3). "First terminal wins" is dead: a `delivery-failed` row no longer
resolves an id to FAILED forever, and idempotence keys on a `delivered` **event
existing**, never on "any terminal" (R8 — the bug fix; a failed@t1 → delivered@t3
retry now correctly resolves *delivered*).

**The five event types** (each past tense — a moment qd WITNESSED; each its own
row schema, not a value of a shared `outcome` field — R8a). The set is **open**:
future witnessed facts arrive as NEW types, never as new outcome values.

| `event`            | witnessed moment | `reason` |
|---|---|---|
| `accepted`         | inbound envelope presented and accepted through the door (inbound mode) | forbidden |
| `attempted`        | a delivery attempt STARTED (each retry = a fresh `attempted`) | forbidden |
| `queued`           | the attempt placed the message into the target's delivery queue / awaiting idle or wake (a witnessed moment, possibly minutes before landing — busy or waking session) | forbidden |
| `delivered`        | the prose LANDED in the session — existence of this row IS the irreversible delivered fact | forbidden |
| `delivery-failed`  | the attempt definitively did not arrive | **required** |

**Guard — only witnessed moments get events.** qd emits an event when it
witnesses the thing HAPPEN, never a speculative/planned state. "Will retry at X"
stays view-computed (`next_attempt` is policy, §3 / R8b guard 1); there is no
`pending`/`expired` row here (both are *absence*-derived at the query surface,
§3), and no `outcome` field anywhere.

**Per-event-type `reason` invariant** (schema-per-event-type, tighter than any
shared enum — R8a): `reason` is REQUIRED on `delivery-failed` (the failure
class, e.g. `"wake"` — the `{class,reason}` family; `failed{wake}` =
`delivery-failed` reason `"wake"`) and FORBIDDEN on every other type. A
`delivery-failed` row without `reason`, or any other type carrying `reason`, is
**corrupt** (counted, never returned — the reader enforces this per type).

### Row schema (one witnessed event)

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "qd dispositions.jsonl row (witnessed event, v1)",
  "type": "object",
  "required": ["v", "correlation_id", "event", "witnessed_at", "witness", "origin", "authored_at"],
  "additionalProperties": false,
  "properties": {
    "v":              { "const": 1 },
    "correlation_id": { "type": "string", "minLength": 1,
                        "description": "The envelope's origin-minted id — the join key to the log envelope and the correlation key across an event's whole funnel. (Name RULED — R9.1, see §1.)" },
    "event":          { "enum": ["accepted", "attempted", "queued", "delivered", "delivery-failed"],
                        "description": "Which witnessed moment this row records. Past tense; the type set is open (new witnessed facts are new types, never new values of an outcome field)." },
    "witnessed_at":   { "type": "integer",
                        "description": "epoch-ms, stamped by THIS witness at the moment of witnessing. Effects order — and the view's latest-event pick — key on this (N10 / Amendment 1)." },
    "witness":        { "type": "string", "minLength": 1,
                        "description": "The witnessing host id (this qd) — R9.3. In a --host/--all union, disambiguates which host witnessed." },
    "origin":         { "type": "string", "minLength": 1,
                        "description": "The envelope's origin host id, copied from the envelope at witness time (R11): self-containment when the envelope lives in an un-unioned mirror; all witnesses copy the same envelope field, so unions agree." },
    "authored_at":    { "type": "integer",
                        "description": "epoch-ms, the envelope's origin timeline, copied at witness time so this row is self-contained when the envelope lives in a mirror. N10 authored timeline." },
    "reason":         { "type": "string",
                        "description": "REQUIRED on `delivery-failed` (the failure class, e.g. \"wake\"); FORBIDDEN on every other event type. Part of the {class,reason} failure family (contract §6). Omitted from the wire when absent." }
  }
}
```

Key order on the wire: `v, correlation_id, event, witnessed_at, witness,
origin, authored_at, reason` (`reason` omitted entirely when absent — present
only on `delivery-failed`).

**Witness note (the N10 split as field naming — R9.3).** Every event row
carries both timelines: **{`origin`, `authored_at`}** is the ORIGIN timeline
(who minted the envelope, when it was authored — copied from the envelope at
witness time) vs **{`witness`, `witnessed_at`}** the WITNESS timeline (who saw
this moment, when). Event rows deliberately carry **no `expires_at`** (ruled):
the door refuses past-expiry presentations; an orphan's expiry status stays a
documented degenerate analytics case (§3a note 3), not a schema driver.

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
  [§2 witnessed event](#2--dispositionsjsonl--the-typed-witnessed-event-log-r8r8ar8b),
  emitted verbatim (published/versioned), for history/analytics views
  (attempts-before-landing, queue→delivered latency, wake-latency — R8b payoff).

**State is a view, always.** The summary's `state` is DERIVED from the event
log; nothing stores it. Idempotence and the delivered-view both key on a
`delivered` **event existing** (R8), never "any terminal."

### 3a · The summary record (DEFAULT)

Folded per `correlation_id` over the event log ∪ the envelope:

- `state` — the coarse 4-state view (below).
- `attempts` — count of `attempted` events.
- `last_event` — the event MAX by `(witnessed_at, witness)` lexicographic
  (R11.2, note 2 below) — detail beneath `state` without widening the enum (R8b
  guard 2). **`null` iff no events exist** (R11.1).
- `last_attempt_at` — max `witnessed_at` over `attempted` (null if none).
- `first_delivered_at` — min `witnessed_at` over `delivered` (null if none).
- `expires_at` — from the joined envelope (null when the envelope is out of
  scope — an orphan-event summary).
- `authored_at` — from the envelope if in scope, else from the (first) event
  (self-contained).
- `origin` — from the envelope's `origin` if in scope, else copied from the
  (first) event's `origin` (every event carries it, R11 — no nullable escape).
  REQUIRED.
- `witness` — the witness of the `last_event` pick; **`null` iff no events
  exist** (paired with `last_event`, R11.1).

**Paired-null rule (R11.1):** `{last_event, witness}` are null together,
exactly when no events exist. A summary never reports a witnessed moment nobody
witnessed — the old behavior of defaulting a zero-events envelope to `accepted`
is **overruled** (a fabricated `accepted` poisons `WHERE last_event='accepted'`
views).

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
4. `pending`   — otherwise (latest is `accepted`/`attempted`/`queued`, or none —
   an envelope with no witnessed event yet). "Silence is pending, never
   success."

Multi-witness resolution simplifies (R8): `delivered`-exists **anywhere** wins
(deterministic, no tie-break — delivery is an irreversible fact); attempt
histories from different witnesses UNION harmlessly into the counts.

**Consumer notes:**

1. **`failed` is NOT absorbing.** Across retries a row moves failed → pending →
   failed → …; `delivered` is the only absorbing state; `failed` → `expired` at
   `expires_at`. Frame alert views must predicate on undelivered-past-N or
   attempts ≥ K (view policy), NEVER on "reached failed once" — the old
   first-terminal-wins habit is dead. (R10 delta 2.)
2. **Determinism.** Across a multi-witness union, `last_event` orders by
   `(witnessed_at, witness)` lexicographic; within one witness's file, file
   order IS witnessed order — at equal `witnessed_at` the later row wins (the
   ms timestamp is a lossy projection of append order). (R11.2.)
3. An envelope-out-of-scope summary (events in scope, envelope in an un-unioned
   mirror) has `expires_at: null` ⇒ `expired` is underivable there; the door's
   past-expiry refusal bounds the exposure. (R10 note b.)

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "qd disposition summary record (emitted / published, v1)",
  "type": "object",
  "required": ["v", "correlation_id", "state", "attempts", "last_event", "last_attempt_at", "first_delivered_at", "expires_at", "authored_at", "origin", "witness"],
  "additionalProperties": false,
  "properties": {
    "v":                  { "const": 1 },
    "correlation_id":     { "type": "string", "minLength": 1 },
    "state":              { "enum": ["pending", "delivered", "failed", "expired"],
                            "description": "The coarse 4-state view (UNCHANGED, R8b guard 2). delivered = a delivered event exists; expired = no delivery past expires_at; failed = latest event is delivery-failed, pre-expiry; pending = otherwise. Silence is pending, never success. Precedence RATIFIED (R10)." },
    "attempts":           { "type": "integer", "minimum": 0,
                            "description": "Count of `attempted` events for this id." },
    "last_event":         { "enum": ["accepted", "attempted", "queued", "delivered", "delivery-failed", null],
                            "description": "The event MAX by (witnessed_at, witness) lexicographic (R11.2; full tie → later-in-input row) — detail beneath `state`. null iff no events exist (R11.1 paired-null with `witness`). Stable column." },
    "last_attempt_at":    { "type": ["integer", "null"],
                            "description": "epoch-ms, max witnessed_at over `attempted`; null if none. Stable column." },
    "first_delivered_at": { "type": ["integer", "null"],
                            "description": "epoch-ms, min witnessed_at over `delivered`; null if none. Stable column." },
    "expires_at":         { "type": ["integer", "null"],
                            "description": "epoch-ms from the joined envelope; null when the envelope is out of scope (orphan-event summary). Stable column." },
    "authored_at":        { "type": "integer", "description": "epoch-ms origin timeline (envelope if in scope, else the first event). N10." },
    "origin":             { "type": "string", "minLength": 1,
                            "description": "The origin host id: the envelope's `origin` if in scope, else copied from the (first) event's `origin` (every event carries it — R11; no nullable escape). In a union, disambiguates origin." },
    "witness":            { "type": ["string", "null"],
                            "description": "The witness of the `last_event` pick; null iff no events exist (R11.1 paired-null with `last_event`). Stable column." }
  }
}
```

Key order on the wire: `v, correlation_id, state, attempts, last_event,
last_attempt_at, first_delivered_at, expires_at, authored_at, origin, witness`.
The nullable columns (`last_event`, `last_attempt_at`, `first_delivered_at`,
`expires_at`, `witness`) are present as `null` when absent — **stable columns**
for the DuckDB projection, never skipped.

### 3b · The raw event record (`--events`)

`qd dispositions --events` emits the §2 witnessed events verbatim — the same
row schema, published and versioned with this contract (key order `v,
correlation_id, event, witnessed_at, witness, origin, authored_at, reason`;
`reason` present only on `delivery-failed`). This is the funnel history/analytics views
project over; the DEFAULT summary (§3a) is the fold of exactly these rows.

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
the join the up-projection runs to correlate `witness`/`witnessed_at` back to
the ledger, so the consumer form is load-bearing.

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
| `dispositions.jsonl` = append-only log of typed witnessed EVENTS, never state records; witnessed order == append order | §2, R8/R8a/R8b |
| five event types (accepted/attempted/queued/delivered/delivery-failed), past tense, own row schema; type set open (new facts = new types, not outcome values) | §2, R8a/R8b |
| only witnessed moments get events; no speculative/planned rows; no `outcome` field | §2, R8b guard 1 |
| `reason` REQUIRED on delivery-failed, FORBIDDEN on every other type (per-event-type validation) | §2, R8a |
| state is a VIEW, always; nothing stores it (summary derives it, §3a) | §3, R8 |
| idempotence + delivered-view key on a `delivered` EVENT existing, never "any terminal"; "first terminal wins" is DEAD | §3a, R8 (the bug fix) |
| summary `state` precedence delivered→expired→failed→pending (RATIFIED, isolated `derive_state`) | §3a, R10 |
| `failed` is NOT absorbing: failed → pending → failed → … across retries; `delivered` is the only absorbing state; alert views predicate on undelivered-past-N / attempts ≥ K, never "reached failed once" | §3a note 1, R10 delta 2 |
| summary carries `last_event` for detail; the 4-state enum stays UNCHANGED | §3a, R8b guard 2 |
| zero-events summary: `last_event` (and `witness`) are `null` — never a fabricated `accepted`; {last_event, witness} paired-null iff no events | §3a, R11.1 |
| `last_event`/`witness` pick = MAX by `(witnessed_at, witness)` lexicographic; full tie → later-in-input row (file order IS witnessed order within one witness) | §3a note 2, R11.2 |
| event rows carry `origin` (copied from the envelope at witness time; all witnesses copy the same field, unions agree); event rows carry NO `expires_at` | §2, R11 |
| pending = no delivery pre-expiry, view-computed; never a row | §2/§3a, contract §4 "silence is pending" |
| expired = no delivery past envelope's own expires_at; view-computed; orphan-event (no envelope) is never expired | §2/§3a, TRANSITION §2, contract §4 |
| multi-witness: delivered-exists anywhere wins (no tie-break); attempt histories union harmlessly | §3a, R8 |
| `qd dispositions` DEFAULT = summary; `--events` = the raw event funnel (both published/versioned) | §3, R8b |
| correlation_id minted once at origin; verbatim; idempotency key | §1, TRANSITION identity crux, contract §4 |
| inbound mode: idempotent on id (delivered-exists), no local log append | §1/§2, Annex A THE ONE DOOR |
| no `posted` state | contract Annex A (ruled 2026-08-08) |
| no second-order "delivered" event; the `delivered` event IS the single copy of truth | N12, contract §4 |
| single writer qd for log + dispositions; mirrors mover-written | Amendment 1/3 |
| emitted records = stateless caller-windowed projection, no cursors | N2, contract §4 bulk form |
| field NAMING RULED: `origin` (origin host id, §1 envelope + §2 event + §3a summary) / `witness` (witnessing host id, §2/§3a); `correlation_id` keeps its name; `ls.json` §4 `host` UNCHANGED (a different, already-landed surface) | R9 |
| `ls.json` mover-written; `qd ls` is READ-ONLY (defines + reads mirrors) | §4, TRANSITION §3 |
| `qd ls` staleness always surfaced (`now − witnessed_at`) | §4, TRANSITION §3 "dead pipeline visible" |
| `--all` = local (uncap + tombstones) + every peer mirror; no-`remote/` = byte-identical | §4, build-lead reconciliation |
| host-qualified `ls` with no mirror ⇒ refused{no-fleet-state} exit 12 | §4, consistent w/ `qd send --host` |
| resume-and-deliver: stopped ≠ refusal class; wake in the attempt; failed{wake} on unwakeable | TRANSITION §3, contract §4 P0 ruling |
| inbound door: malformed / mis-addressed / past-expiry / ambiguous ⇒ named refusals (past-expiry refused, never stamped `expired`) | §3, Annex A THE ONE DOOR |

**Conformance (v1).** The §6 acceptance bar for this transport surface is
demonstrated end-to-end by `dispatch/crates/dispatch/tests/acceptance.rs` — the
log→disposition→`qd dispositions`→DuckDB-join round-trip (via `read_ndjson_auto`),
inbound-mode idempotence, and the named door refusals. Per the W8 assessment,
`acceptance.rs` *is* the conformance cell for the disposition surface in v1 (the
existing per-provider conformance battery covers the older per-session
`events.jsonl` transport, keyed by `send_id`/`content_sha256`, not this
`correlation_id`-keyed surface). A dedicated lane-agnostic "D8 DispositionContract"
dimension on the tier grid is a v2 option.
