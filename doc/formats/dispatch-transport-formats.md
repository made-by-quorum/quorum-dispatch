# Dispatch transport formats — the qd–qf transition (v1)

Status: **draft 1**, 2026-08-08. Author: build lead (session on brano), from
`ws/quorum/qd-qf/TRANSITION.md` §§1–3 + `provider-contract.md` §4 + Annex A.
Design authority: session `qfd-qbt`.

"**Formats are contracts**" (TRANSITION §1): each file gets a JSON-Schema-first
contract — framing, field semantics, ordering guarantee, torn-tail rule, version
marker. The Rust structs (crate `quorum-dispositions`) are reflections of these
schemas, not the other way round. The **emitted disposition record** (§3) is the
one data shape frame projects over; it is **versioned with the provider
contract**.

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
dispositions.jsonl          §2  witnessed terminal facts THIS qd authored
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
  "required": ["v", "correlation_id", "authored_at", "expires_at", "target", "body", "authority"],
  "additionalProperties": false,
  "properties": {
    "v":              { "const": 1 },
    "correlation_id": { "type": "string", "minLength": 1,
                        "description": "The envelope id — minted EXACTLY ONCE at origin by the originating program's append: frame's ledger event id when frame originates (riding as correlation_id), else qd's own log ULID for bare sends. Traveling verbatim; the idempotency + disposition key. Never a content hash." },
    "authored_at":    { "type": "integer",
                        "description": "epoch-ms, stamped once at origin (derived from the ULID clock when qd mints; otherwise the origin-append time). N10 authored timeline." },
    "expires_at":     { "type": "integer",
                        "description": "epoch-ms. Default authored_at + 12h; overridable per send (--expires). `expired` is minted from ABSENCE past this value. Policy travels with the message; there is no global rolling cutoff." },
    "target":         { "type": "string", "minLength": 1,
                        "description": "The address as given by the caller (name | stable_id | name@host). Operational record; resolution happens at delivery. Not load-bearing for the disposition join (that keys on correlation_id)." },
    "body":           { "type": "string",
                        "description": "The opaque prose, verbatim, delivered as one message. qd never parses it. Persisted so write-then-deliver / resume-and-deliver can redeliver." },
    "authority":      { "type": "string", "minLength": 1,
                        "description": "Origin host id (this qd's host). On a single machine this is the local host id; disambiguates origin when a peer's log is read from remote/<host>/." }
  }
}
```

Key order on the wire (byte-exact, `preserve_order`): `v, correlation_id,
authored_at, expires_at, target, authority, body`. (`body` last: it is the
largest and most variable field; keeping it last keeps the head of every line
cheap to scan.)

---

## 2 · `dispositions.jsonl` — witnessed terminal facts (stored)

Witnessed facts about deliveries **this qd** made. **Only terminal, witnessed
states are ever stored here**: `delivered` and `failed`. It is the physical
"witnessed facts" file — witnessed order == append order.

**Never stored here** (these are *derived*, §3):
- `pending` — the *absence* of a terminal record (before `expires_at`). "Silence
  is pending, never success." Never a row.
- `expired` — minted from *absence* past the envelope's own `expires_at`.
  View-computed at the query surface, never authored into the file (keeps
  clock/policy out of the durable log; matches the witnessed-facts-only joint
  position with qfd-qbt).

### Row schema (stored)

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "qd dispositions.jsonl row (witnessed terminal, v1)",
  "type": "object",
  "required": ["v", "correlation_id", "state", "authored_at", "witnessed_at", "authority"],
  "additionalProperties": false,
  "properties": {
    "v":              { "const": 1 },
    "correlation_id": { "type": "string", "minLength": 1,
                        "description": "The envelope's origin-minted id. The idempotency key: a terminal row present for this id ⇒ inbound-mode no-op success. First terminal wins." },
    "state":          { "enum": ["delivered", "failed"],
                        "description": "Witnessed terminal only. `delivered` = the prose landed. `failed` = attempted and definitively did not arrive (carries a reason)." },
    "authored_at":    { "type": "integer",
                        "description": "epoch-ms, copied from the envelope at witness time so this file is self-contained for terminal states (the envelope may live in a remote mirror). N10 authored timeline." },
    "witnessed_at":   { "type": "integer",
                        "description": "epoch-ms, stamped by THIS authority at the moment of witnessing = the moment qd accepted it (N10 / Amendment 1). Effects at this host order solely by this." },
    "authority":      { "type": "string", "minLength": 1,
                        "description": "The witnessing host id (this qd). In a --host/--all union, disambiguates which host witnessed." },
    "reason":         { "type": "string",
                        "description": "OPTIONAL. For `failed`, the failure class (e.g. \"wake\" for failed{wake}). Absent for `delivered`. Part of the {class,reason} failure family (contract §6)." }
  }
}
```

Key order on the wire: `v, correlation_id, state, authored_at, witnessed_at,
authority, reason` (`reason` omitted entirely when absent).

---

## 3 · The emitted disposition record — `qd dispositions` output (published, versioned with the contract)

This is **the one data shape frame projects over** (contract §4 "the disposition
record schema is published and versioned with this contract"). `qd dispositions`
emits it as **JSONL on stdout**, one record per `correlation_id` in scope, for
piping into DuckDB. It is a **stateless, caller-windowed projection**: the caller
brings the window; qd stores no read-state, no cursors, ever (N2).

It is computed fresh at query time from `log.jsonl` **⟕ (left join)**
`dispositions.jsonl` (and their `remote/<host>/` replicas under `--host`/`--all`,
`*.archive.jsonl` under `--archive`):

- envelope has a `delivered` terminal        → `state = "delivered"`
- envelope has a `failed` terminal            → `state = "failed"` (+ `reason`)
- envelope, no terminal, `now < expires_at`   → `state = "pending"`  (witnessed_at = null)
- envelope, no terminal, `now >= expires_at`  → `state = "expired"`  (witnessed_at = null)

If more than one terminal exists for a `correlation_id` (normally impossible —
at most one authority witnesses a given id — but a replication-merge or race
artifact must resolve deterministically), the **earliest `witnessed_at` wins**,
ties broken by scan order. This coincides with "first terminal wins" for a
single-writer append-ordered file (§2), and generalizes it across a
multi-authority union.

Where a disposition terminal exists but its originating envelope is not in scope
(e.g. a locally-witnessed inbound delivery whose envelope is in a mirror not
being unioned), the terminal is emitted from the disposition row alone (it is
self-contained: it carries `authored_at` + `authority`). `pending`/`expired`
require the envelope (only it knows `expires_at`), so they are only derivable
where the log/mirror is in scope.

### Emitted record schema

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "qd disposition record (emitted / published, v1)",
  "type": "object",
  "required": ["v", "correlation_id", "state", "authored_at", "authority"],
  "additionalProperties": false,
  "properties": {
    "v":              { "const": 1 },
    "correlation_id": { "type": "string", "minLength": 1 },
    "state":          { "enum": ["pending", "delivered", "failed", "expired"],
                        "description": "pending = absence pre-expiry; delivered/failed = witnessed terminal; expired = absence post-expiry (view-computed). Silence is pending, never success." },
    "authored_at":    { "type": "integer", "description": "epoch-ms, origin (N10 authored timeline)." },
    "witnessed_at":   { "type": ["integer", "null"],
                        "description": "epoch-ms for delivered/failed; null for pending/expired (no witness). Effects order by this per host (N10)." },
    "authority":      { "type": "string", "minLength": 1,
                        "description": "The origin authority for pending/expired; the witnessing authority for delivered/failed. In a union, the origin/authority column disambiguates." },
    "reason":         { "type": "string", "description": "OPTIONAL; present for failed." }
  }
}
```

Key order on the wire: `v, correlation_id, state, authored_at, witnessed_at,
authority, reason`. `witnessed_at` is present as `null` for pending/expired (a
stable column for the DuckDB projection); `reason` is omitted when absent.

`--as-of` note (frame-side, §5.2 of TRANSITION): dispositions carry **no ledger
ord** — they are the live overlay. Frame registers this table fresh each
evaluation and must **not** apply `--as-of` time-travel to it.

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
| pending = absence of terminal (pre-expiry); never a stored row | §2, contract §4 "silence is pending" |
| expired = absence past envelope's own expires_at; view-computed | §2, TRANSITION §2, contract §4 |
| delivered/failed = witnessed terminal, stored, witnessed_at stamped | §2, N10, Amendment 1 |
| correlation_id minted once at origin; verbatim; idempotency key | §1, TRANSITION identity crux, contract §4 |
| inbound mode: idempotent on id, no local log append | §1/§2, Annex A THE ONE DOOR |
| no `posted` state | contract Annex A (ruled 2026-08-08) |
| no second-order "delivered" event; disposition = single copy of truth | N12, contract §4 |
| single writer qd for log + dispositions; mirrors mover-written | Amendment 1/3 |
| emitted record = stateless caller-windowed projection, no cursors | N2, contract §4 bulk form |
| `ls.json` mover-written; `qd ls` is READ-ONLY (defines + reads mirrors) | §4, TRANSITION §3 |
| `qd ls` staleness always surfaced (`now − witnessed_at`) | §4, TRANSITION §3 "dead pipeline visible" |
| `--all` = local (uncap + tombstones) + every peer mirror; no-`remote/` = byte-identical | §4, build-lead reconciliation |
| host-qualified `ls` with no mirror ⇒ refused{no-fleet-state} exit 12 | §4, consistent w/ `qd send --host` |
