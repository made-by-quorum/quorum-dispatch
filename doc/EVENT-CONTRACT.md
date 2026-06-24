# dispatch EVENT-CONTRACT — the external event-source contract

**Status: normative, external.** This is the single source of truth for any
**outside** reader of dispatch's on-disk event sources — chiefly bond's DuckDB
read layer (SPEC-v2 §10, track #7). It restates, from the #1 dispatch ground-truth
findings (`[S §x]`) and the live source (`path:line`), exactly what dispatch
produces today, plus the one additive change W2 introduces (the agent-pid
identity token on `session-opened`). A reader can build a log reader from this
document alone.

> **dispatch paths** are relative to `~/work/quorum/dispatch/`. `events.rs` =
> `crates/dispatch/src/events.rs` (delivery family); `sbmux events.rs` =
> `crates/sbmux/src/events.rs` (mux family); `procid.rs` =
> `crates/sbmux/src/procid.rs` (the W2 token provider).

This contract is **append-only and additively-versioned**: new optional fields
and new event variants may appear; readers MUST skip what they don't know and
MUST NOT use `deny_unknown_fields` (`sbmux events.rs` evolution rule (e); dispatch
`CONVENTIONS.md:6-16`).

---

## 0. The three on-disk stores + the path-prefix discriminator

There are **two event-log families with DIFFERENT envelopes**, plus an id store.
A reader MUST branch on which it is reading — they share some field *names*
(`session`, `seq`, `content_sha256`) that **mean different things** [S §0/G8].

| # | Store | On-disk path | Written by | Envelope key |
|---|-------|-------------|-----------|--------------|
| 1 | **Delivery log** | `~/.quorum/dispatch/state/sessions/<key>.events.jsonl` | `dispatch` crate `events.rs` | `v`+`ts`(ISO)+`pid`+`seq`; file key = uuid **or** `byname-<name>` |
| 2 | **Mux/session log** | `~/.quorum/dispatch/mux/events/<name>.daemon.<epoch>.jsonl` | `sbmux` crate `events.rs` | `session`(=NAME)+`epoch`+`seq`+`ts_ms`(int) |
| 3 | **Id store** | `~/.quorum/dispatch/state/ids.jsonl` | `dispatch` crate `idstore.rs` | `mint`/`bind`/`lineage` |

**The per-family discriminator is the PATH PREFIX, never content auto-detection**
(SPEC-v2 §10 / R2-10): `state/sessions/*.events.jsonl` = **delivery**;
`mux/events/*.jsonl` = **mux**. Read functions take the family **explicitly** (a
delivery-reader and a mux-reader). `session` / `seq` / `content_sha256` are NOT
comparable across families; a cross-family union needs explicit `CAST`
(`session` infers `varchar` vs `INT128`; `CAST(ts AS VARCHAR)` for the delivery
ISO ts) [B §7/§7d].

---

## 1. Delivery family — `state/sessions/<key>.events.jsonl`

### 1.1 Envelope (`Envelope events.rs:156-171`)

Pinned key order `v, ts, pid, seq, session?, name?` (`build_record_line
events.rs:438-446`):

| field | type | notes |
|---|---|---|
| `v` | u32 | per-record schema version, **always `1`** (multi-process file, per-record, no bookend) |
| `ts` | string | **ISO-8601 UTC ms** (e.g. `"2026-06-10T14:28:33.877Z"`) |
| `pid` | u32 | writer pid; the multi-writer key (the dead-writer rule reads THIS pid) |
| `seq` | u64 | monotonic **per (pid, file)**, from **0** |
| `session` | string? | the session **uuid**, when known (omitted when not) |
| `name` | string? | the sb **name**, when known (omitted when not) |

`Option` payload fields are **OMITTED when None** (never `null`/empty —
`insert_opt_str events.rs:429-433`); bool flags `chunk_sha256s_capped`/`recovered`
omitted when `false`.

### 1.2 The 13 payloads (`Payload events.rs:197-273` + the §1.5 3-phase delivery kinds)

1. **`send-initiated`** — emitted **before** the first chunk write (the recovery
   anchor). `send_id`, `verb`(open string; pty `"send:pty"|"new-p"`, **relay
   `"send:relay"`** — §1.5), `send_path`(open string; pty `"idle"|"busy-queued"`,
   **relay `"relay"`** — §1.5), `content_sha256`(64-hex), `content_len`(u64),
   `chunks`(u32; **relay always `1`**), `chunk_sha256s`(array, len `min(chunks,48)`),
   `chunk_sha256s_capped`(bool, omit-false), `transcript`(str?), `transcript_offset`
   (u64?), **`content_preview`(str?, ≤256 B — see §5 privacy; relay OMITS it — §1.5)**.
2. **`chunks-delivered`** — **NON-terminal** transport ack. `send_id`,
   `chunks_acked`(u32), `ack_source`(`"input-sent"|"cli-exit"`).
3. **`turn-anchored`** — **TERMINAL; the "received" signal.** `send_id`,
   `content_sha256`, `anchor{transcript,start_offset,line_index}`, `recovered`
   (bool, omit-false), `attribution`(str?, only when recovered).
4. **`turn-anchored-mismatch`** — **TERMINAL; truncation** (sha OR len disagreement).
   `send_id`, `expected_sha`, `actual_sha`, `expected_len`, `actual_len`,
   `recovered`, `attribution`(str?).
5. **`anchor-timeout`** — **TERMINAL** (timeout). `send_id`, `waited_ms`(u64).
   Terminal FOR THE WATCH, **not immutable** — a recovery-read MAY append a
   `turn-anchored` after it; take the **FIRST** terminal in file order
   (`events.rs:94-96`).
6. **`pending-abandoned`** — **TERMINAL.** `send_id`, `reason`
   (`"watch-interrupted"|"session-died"|"recovery-no-candidate"`).
7. **`composer-cleared`** — **NON-terminal**, advisory. `send_id`.
8. **`priming-readiness-timeout`** — boot terminal (NOT a send; **no `send_id`**).
   `waited_ms`(u64), `phase`(`"pid-file"|"idle"`).
9. **`status-transition`** — observed status change (**no `send_id`**).
   `status`(e.g. `"busy"`/`"idle"`), `source`(v1 constant `"status-file-poll"`).
10. **`events-truncated`** — rotation marker, **envelope only**.
11. **`relay-delivered`** — **NON-terminal** relay on-queued ack (the relay analog of
    `chunks-delivered`). `send_id`, `content_sha256`. Emitted sender-side into the
    **target's** log right after the `send:relay` POST returns the server-minted
    `message_id` (`send_id == message_id`). See §1.5.
12. **`message-seen`** — **TERMINAL (success); the on-received "the recipient pulled it
    into working context" signal** for BOTH transports (relay: a recipient-side
    transcript observer; async no-wait pty: the W8 verify ungate). `send_id`,
    `content_sha256`. Deliberately a NEW kind (NOT `turn-anchored`) so a `--wait`/W8
    `turn-anchored` anchor can never trip the on-received gate. See §1.5.
13. **`seen-failed`** — **TERMINAL (failure).** `send_id`, `reason` (`"recipient-gone"`
    — extend additively). Fires ONLY on a genuine recipient-gone (session-close
    bookend + transcript-absence of the `message_id`), **NEVER on latency** (an
    un-pulled-but-alive message stays PENDING). See §1.5.

### 1.3 The normative terminal set (`TERMINAL_EVENTS events.rs:97-102`)

```
TERMINAL_EVENTS = [ turn-anchored, turn-anchored-mismatch, anchor-timeout,
                    pending-abandoned, message-seen, seen-failed ]
```

- **pty on-turn `received` = `turn-anchored` SPECIFICALLY**; the **on-received**
  success signal (relay + async no-wait pty) is **`message-seen`** (§1.5).
  `turn-anchored-mismatch`/`anchor-timeout`/`pending-abandoned`/`seen-failed` are
  **failures** (a gate must STALL on them). **`relay-delivered` stays NON-terminal.**
- **Non-terminals NEVER gate:** `chunks-delivered`, `composer-cleared`,
  `status-transition` can **never** satisfy a wait (`events.rs:88-92`). Wiring a
  gate to `chunks-delivered` is the canonical mistake.
- **First-terminal-wins, per `send_id`** (`first_terminal_for events.rs:1454`):
  a recovery-read may append a late `turn-anchored` AFTER an `anchor-timeout`, so
  the reader takes the **FIRST** terminal in file order as the verdict.
- `await_received` is **library-only** (`GATE-ACK3.md:203-207`); a reader barred
  from linking dispatch (SPEC-v2) **re-implements** first-terminal-wins over the
  tailed log.

### 1.4 Split-file merge, robustness, joins

- **Split delivery files (G5):** records for one logical session split across the
  **uuid** file and the **`byname-<name>`** file (pre-registry / failed-boot land
  in byname). A reader keyed `(session_id?, name?)` MUST **merge BOTH**
  (`reader_paths events.rs:481-494`; DuckDB `union_by_name=true`), first-terminal-
  wins over the union ordered `(ts, pid, seq)`.
- **`send_id`** (`mint_send_id events.rs:64-70`) = `"{pid}-{epoch_ms}-{n}"`,
  **OPAQUE — equality only, nobody parses it.** The join from a bond event to its
  delivery span. Absent from the mux family; **now emitted by `send:relay`** as
  `send_id == message_id` (§1.5 — reverses the prior §4/G1 relay event-silence).
- **Per-record cap + shrink-to-fit-NEVER-skip:** `MAX_RECORD_BYTES=4096`
  (`events.rs:130`); overflow drops `content_preview` FIRST, then trailing
  `chunk_sha256s` (sets `chunk_sha256s_capped:true`) — `content_sha256` is **never**
  dropped; a record is **never** dropped (`fit_line events.rs:642-715`). So optional
  fields may be absent even when "expected."
- **Rotation:** file cap `EVENTS_MAX_BYTES=5 MiB` with a once-per-file
  `events-truncated` marker; **`seq` gaps after that marker are intentional**
  (suppressed non-terminal records still consume seq) — never assume contiguous seq.
- **Torn-tail tolerance:** the LAST line, if unparseable or missing its `\n`, is a
  torn tail → **skipped silently** (a concurrent ≤4 KB append is normal). DuckDB:
  always `read_json_auto(path, ignore_errors=true)` [S §2.7; B §7a].
- **Dead-writer rule:** a `send-initiated` with no terminal whose envelope `pid` is
  dead and older than `T_ANCHOR_IDLE_MS=30_000` is "dead-dangling"
  (`is_dead_dangling events.rs:968-992`). Named imperfection: **pid reuse** — the
  hole the W2 token closes for the *session* span (§2.4).

### 1.5 The 3-phase delivery contract (relay + async-pty on-received)

Ratified, why-holder-approved (`ec00a38`). A uniform **on-sent → on-queued →
on-received** sequence for both transports, keyed entirely by **`send_id`**. The
**one-way invariant is preserved**: dispatch knows nothing of bond; every record goes
ONLY into dispatch's own `state/sessions/<key>.events.jsonl`, and the relay WIRE
(`POST /message`, `/replies`, `/health`, sidecars, the `CcRelay` client) is
**byte-identical** — these emits happen purely in the local log, after the message
has already left.

**The three phases (per `send_id`):**

| phase | relay | async no-wait pty | terminal? |
|---|---|---|---|
| on-sent | `send-initiated` (the §1.2 payload REUSED with relay values: `verb="send:relay"`, `send_path="relay"`, `chunks=1`, `chunk_sha256s=[content_sha256]`, preview/transcript omitted) | `send-initiated` (pty values) | no |
| on-queued | `relay-delivered` | `chunks-delivered` | no |
| on-received | `message-seen` | `message-seen` | **yes (success)** |
| on-received failure | `seen-failed{recipient-gone}` | — | **yes (failure)** |

- **Correlation (§X.4):** `send_id` is mandatory on every record; relay
  `send_id == message_id` (the server-minted id), recovered **verbatim**
  recipient-side. The recipient-side relay `content_sha256` (over the extracted inner
  message body) is **ADVISORY** — correlation succeeds on `send_id` alone; the hash
  never gates.
- **Invariant:** exactly one `send-initiated` + **exactly one terminal** per
  `send_id`. `first_terminal_for` (§1.3) picks the first terminal in file order.
- **Producers (§X.5, scope-guarded):** relay on-sent/on-queued = the relay **sender**
  (`send_relay.rs emit_relay_send_events`, into the **target's** log); relay
  on-received = a long-lived **recipient-side** transcript observer hosted in
  `relay:serve` (`relay_server/mod.rs run_received_observer`) that emits `message-seen`
  when a relay `message_id` lands as a `<channel … message_id="…">` **wrapper
  attribute** in the recipient's own transcript (a body-mentioned id never fires — the
  one-terminal-per-`send_id` / no-wrong-fire guard), scoped to ids this recipient
  genuinely received; `seen-failed` at the recipient session-close bookend for a
  tracked-but-unpulled id (with a final transcript-scan race-guard so a `seen-failed`
  and a `message-seen` can never both land). The async no-wait pty `message-seen` is
  the W8 verify-success **ungated** to the `!wait` path only (`send.rs`); the
  `--wait`/`new -p` paths KEEP `turn-anchored` (untouched). `recovery_event` and
  `emit_new_p_anchored` stay `turn-anchored` (out of scope).
- **Latency ≠ failure (§X.6):** on-received latency is **unbounded**; an un-pulled
  message keeps the promise **PENDING** — never a `seen-failed`, never a timeout.
- **No prose (§X.7):** relay carries no `content_preview`; the new on-received kinds
  carry only `send_id` + `content_sha256`/`reason`.
- **Version compatibility:** additive + skip-unknown (string-keyed read path), so a
  version mismatch can only ever leave a gate **PENDING — never a wrong fire**: old
  bond + new dispatch skips the new kinds; new bond + old dispatch finds no events and stays
  PENDING. (The live dual-binary cutover + real-dispatch E2E are owned by the cutover
  composite, not this contract.)

---

## 2. Mux/session family — `mux/events/<name>.daemon.<epoch>.jsonl`

### 2.1 Envelope (`EventMeta sbmux events.rs:55-73`, `#[serde(flatten)]`)

| field | type | notes |
|---|---|---|
| `event` | string | kebab tag (`#[serde(tag="event", rename_all="kebab-case")]`) |
| `session` | string | the **NAME** (NOT a uuid) |
| `epoch` | u64 | per-daemon-incarnation, ≥1; also in the filename |
| `seq` | u64 | per-file, from **1** (counts accepted events) |
| `ts_ms` | u64 | **integer** epoch-millis; **FORENSIC-ONLY, never load-bearing** |

**No `v`, no `send_id`** in this envelope; `pid` + `schema_version` appear ONLY on
`session-opened`. Epoch must be parsed from the **filename** via a last-`.daemon.`
anchor (`scan_max_epoch sbmux events.rs:537-562`) — names may legally contain `.`.

### 2.2 Events (`DaemonEvent sbmux events.rs:76-144`)

1. **`session-opened`** — the **started** bookend, always `seq==1`. Adds `pid`(u32),
   `schema_version`(u32, carried ONLY here), **and after W2 the OPTIONAL agent-pid
   identity token `pid_start_ms`(u64?) + `boot_id`(str?)** (§2.3).
2. **`pty-bytes-written`** — **advisory, NOT a receipt** (kernel may drop after a
   successful PTY write). `bytes`(u64), `content_sha256`(str), `content_len`(u64).
3. **`session-closed`** — the **stopped** bookend. `reason`(`CloseReason`).
4. **`pty-write-failed`** — `errno`(i32?), `error`(str), `content_sha256`, `content_len`.
5. **`events-truncated`** — rotation marker; `cap_bytes`(u64).
6. **`heartbeat`** — envelope only; **DEFAULT OFF** (`SBMUX_EVENTS_HEARTBEAT_MS`).

**`CloseReason`** (`sbmux events.rs:146-158`, kebab): `killed` | `child-exited`
(no exit code in the event) | `daemon-shutdown` (graceful) | `dropped` (a finding).
**A hard daemon SIGKILL emits NO `session-closed`** — crash is signalled by
*absence* + a later epoch, not a reason (G7).

### 2.3 The agent-pid identity token (W2 — additive; `schema_version` 1 → 2)

On `session-opened`, after `schema_version`, two **OPTIONAL** fields (SPEC-v2
§5.A / R2-8 / P1):

| field | type | meaning |
|---|---|---|
| `pid_start_ms` | u64? | kernel start-time of the recorded `pid` (the PTY child / **agent**), epoch-**MILLISECONDS**, **ms-floored on both platforms** |
| `boot_id` | str? | a per-boot-stable **opaque** id, compared by **EXACT string equality only** |

- **What `pid` is:** the **PTY child = the agent process** (the responder bond
  cares about), **NOT** the sbmux daemon — captured ONCE at spawn
  (`sbmux events.rs:83-85`, `session.rs`), written on `session-opened`. The token
  pins the start-time of *that same recorded pid*; bond re-checks *that same pid*,
  so recycle-detection works regardless [SPEC-v2 §5.A, M5; #4/dispatch-gt-ac1].
- **Why:** today liveness rests on `kill(pid,0)` with no start-time check
  (`is_pid_alive crates/dispatch/src/effects.rs:520-527`), so a recycled pid reads
  **false-LIVE**. `pid_start_ms` pins the incarnation **within** a boot; `boot_id`
  disambiguates **across** reboots.
- **Resolution is ms-FLOORED on BOTH (N1/D1):** darwin's source is sub-ms
  (`pbi_start_tvusec`) but floored to ms; linux is clock-TICK-bounded (~10 ms at
  `CLK_TCK=100`). Effective discriminator: **~1 ms darwin, ~10 ms linux** — this
  bounds same-ms/tick pid-reuse, **not** "same-second."
- **Fail-safe (per field):** `pid_start_ms` is `None` (omitted) on any read
  failure, on `pid==0` (the benign no-child case), or on an unsupported platform.
  `boot_id` is **pid-independent** (a per-boot value, not derived from the pid), so
  it is `None` only when its source is unreadable — **not** on `pid==0`. Hence on
  `pid==0` the expected shape is a **`boot_id`-only half-token** (`pid:0`, `boot_id`
  present, `pid_start_ms` omitted). The half-token is still fail-safe: a consumer
  treats **any absent `pid_start_ms` as crash-dead regardless of `boot_id`**
  (absence / mismatch ⇒ crash-dead, never false-LIVE).
- **Producer method (the value contract — `procid.rs`):**
  - **Darwin (macOS 10.12+):** `pid_start_ms` via libproc `proc_pidinfo`
    (`pidinfo::<BSDInfo>(pid,0)` → `pbi_start_tvsec*1000 + pbi_start_tvusec/1000`);
    `boot_id` via `sysctlbyname("kern.bootsessionuuid")`. **Never `kern.boottime`**
    (re-disciplines under NTP/sleep → false crash-dead under string-equality). This
    is the SAME libproc call SPEC-v2 §5.A names for bond's fallback → producer and
    consumer derive **bit-identical** values by construction (same-box invariant).
  - **Linux:** `pid_start_ms` from `/proc/<pid>/stat` field 22 (`starttime` ticks) +
    `/proc/stat` `btime` + `sysconf(_SC_CLK_TCK)`; `boot_id` =
    `/proc/sys/kernel/random/boot_id`.

> **⚠ Naming supersedes SPEC-v2 §5.A (authoritative-contract note, F5).** SPEC-v2
> §5.A's illustrative names **`pid_start_unix`** + **`sysctl kern.boottime`**
> **PREDATE and are SUPERSEDED by this contract.** The shipped producer emits
> **`pid_start_ms` (epoch-MILLISECONDS)** + **`boot_id` from `kern.bootsessionuuid`**
> (`kern.boottime` is explicitly *rejected* — it re-disciplines under NTP/sleep). An
> outside reader (bond #7) **MUST build the consumer against THIS document** as the
> authoritative producer contract — `pid_start_ms` (epoch-ms) + `boot_id` from
> `kern.bootsessionuuid` — **NOT §5.A's literal text**, else every token comparison
> mismatches and the token goes **silently inert** (fail-safe crash-dead). The §5.A
> spec-text sync is a cross-track (supervisor) call; SPEC-v2.md is not a #6 file.

### 2.4 The wire-format example (the EXACT serialized line a reader parses)

Field order = tag first, flattened `EventMeta`, then `pid`, `schema_version`, then
the appended token (**omitted-when-None**). **v2, token PRESENT:**

```json
{"event":"session-opened","session":"g1","epoch":1,"seq":1,"ts_ms":1780722000123,"pid":4242,"schema_version":2,"pid_start_ms":1780721999123,"boot_id":"550e8400-e29b-41d4-a716-446655440000"}
```

**v2, token ABSENT** (still valid; fields omitted — byte-stable with a v1 line
except `schema_version`):

```json
{"event":"session-opened","session":"g1","epoch":1,"seq":1,"ts_ms":1780722000123,"pid":4242,"schema_version":2}
```

A **pre-token v1** line (`schema_version:1`, no token) remains valid and parses
under the v2 reader (`#[serde(default)]` → fields `None`). The version bump signals
"a token *may* be present." The additive-evolution **rule is declared** at
`sbmux events.rs:39-40` — warn (never fail) on a newer-than-known version — but
dispatch has **no mux-stream consumer**, so its **enforcement is the consumer's**
(bond #7): a v2 reader skips a newer `schema_version` / unknown fields, never
errors. The *never-fail* half is structural here (`deny_unknown_fields` is banned),
asserted by the forward-compat test (a `schema_version:3` / unknown-field
`session-opened` line parses under the v2 reader, ignored).

### 2.5 Cross-impl symmetry — the PUBLISHED linux parser fixture vector

The linux `/proc/<pid>/stat` parser is value-contract-critical (producer dispatch and
consumer bond are separate codebases). bond's track (#7) MUST assert the **same**
vector against its own implementation:

| input | value |
|---|---|
| `/proc/<pid>/stat` | `1234 (my )( proc) S 1 1 1 0 -1 4194560 100 0 0 0 10 5 0 0 20 0 1 0 22200 0 0` |
| `/proc/stat` | `…\nbtime 1700000000\n…` |
| `CLK_TCK` | `100` |
| **expected `pid_start_ms`** | **`1700000222000`** |

Derivation: `btime*1000 + starttime_ticks*1000/CLK_TCK` (integer floor) =
`1700000000*1000 + 22200*1000/100 = 1700000000000 + 222000`. The `comm` field
contains `) (` and spaces — the parser keys on the **LAST `)`**, then field 22
(`starttime`) is index 19 after the comm. (Asserted in `procid.rs`
`parse_start_ms_published_fixture_vector`, which runs on every host since the
parser is pure.)

---

## 3. Cross-family rules an external reader MUST honor

1. **Path-prefix discriminator** (§0): `state/sessions/*` = delivery, `mux/events/*`
   = mux; never auto-detect (SPEC-v2 §10/R2-10).
2. **Same field NAMES mean different things** across families: `session` (uuid vs
   NAME), `seq` (from 0 vs from 1), `content_sha256` (delivery payload vs mux PTY
   bytes); `v` exists only on delivery, `send_id` only on delivery [G8].
3. **Additive-only evolution** (`sbmux events.rs:33-41`; `CONVENTIONS.md:6-16`): new
   optional fields land with `#[serde(default)]`; new event variants must be
   **skipped** by readers, never error; nothing is renamed/retyped/removed;
   `deny_unknown_fields` is **banned**; any additive change bumps the version. The
   "warn (not fail) on a newer version" rule is **declared** at `events.rs:39-40`;
   its **enforcement is the consumer's** (bond #7) — dispatch has no mux-stream reader.
4. **Torn-tail tolerance**: read with `ignore_errors`; an unparseable line →
   skipped (`parse_line → None`, `sbmux events.rs:163-165`; delivery
   `events.rs:839-882`).
5. **Cross-log joins:**
   - bond event ↔ delivery span: **`send_id`** (delivery only).
   - delivery span ↔ session span: **`name`** only (`delivery.name == mux.session`);
     the mux log has no uuid/`send_id`. Name is **reusable over time** (kill+restart
     yields multiple epochs sharing `session=NAME`), disambiguated by epoch +
     (best-effort) pid correlation / temporal overlap [G4]. **No per-send join across
     the two logs exists.**
   - uuid ↔ short-id: `ids.jsonl` `by_session`/`by_id` (`idstore.rs:90,93`). **No
     `by_name` index** (the `name` on a mint line is fold-discarded).

---

## 4. Scope — the deliberate NON-changes (W4)

So a reader (and a future builder) is not misled into expecting changes dispatch did
**not** make. These are resolved **bond-side** in SPEC-v2, not in dispatch:

- **Relay event-silence (G1) — REVERSED by the 3-phase delivery contract (§1.5).**
  The prior contract held that `send:relay` emitted **no event and no `send_id`**,
  with correlation resolved bond-side (SPEC-v2 §4.D, arm-time refusal of relay on a
  watched link). The ratified 3-phase delivery contract (§1.5, `ec00a38`) **takes the
  alternative the prior version declined**: `send:relay` now emits `send-initiated`
  (relay values) + `relay-delivered` sender-side, and `message-seen`/`seen-failed`
  recipient-side, all keyed `send_id == message_id`. The **one-way invariant still
  holds** — events go ONLY into dispatch's own log; the relay WIRE is byte-identical.
- **`content_preview` privacy (G6):** SPEC-v2 §4.C handles this **bond-side** (a
  read-allowlist that EXCLUDES `content_preview`). **dispatch does NOT change
  `content_preview`** — the redactor (`redact.rs`) is untouched. (W1.3 only adds a
  reader-facing reconcile note at the misleading invariant; see §5.)
- **`send:http` unusable for engine sessions (G10):** always exits 1 for engine
  sessions (`send.rs:1014-1043`). **Documented, no change.**
- **W3 (optional `send_id`-to-stderr) — DROPPED.** SPEC-v2 §4.A.3 C1-sub marks it
  OPTIONAL and **NOT load-bearing** (bond's direct-spawn pid-match + the
  issued-but-unsent reconcile floor cover correlation). It was assessed for #6 and
  **dropped**: the `send:pty` success paths in `send.rs` are interleaved with the
  `--wait` fall-through, so emitting *exactly one* `send-id:` line cleanly would
  require refactoring a delicate hot-path function — not "cheap & clean," and zero
  SPEC-v2 conformance cost. If later added, the pinned contract is: exactly one
  line matching `^send-id: \S+$` on **stderr** (never stdout — the exit-code
  contract says composers branch on the exit code alone, and the `--wait` reply body
  on stdout must stay byte-identical).

---

## 5. Privacy reconciliation (W1.3 / SPEC-v2 §4.C, G6)

dispatch's module invariant (`crates/dispatch/src/events.rs` privacy section, marked
"VERBATIM — do not weaken") states records carry `content_sha256` + `content_len`
**ONLY — never raw message text**. This is **contradicted by shipped behavior**:
`send-initiated` also carries **`content_preview`** (`events.rs` field +
`redact.rs`), which scrubs only known key-prefixes (`sk-`, `ghp_`, `xoxb-`,
`AKIA`, JWT `eyJ`, …) and generic ≥24-char tokens, then truncates to 256 B — *"only
the key token is scrubbed; the surrounding prose is verbatim"*. So **readable
message prose** (secrets-scrubbed, ≤256 B) **is** written to the delivery log.

An external reader must treat the delivery log as carrying readable prose. The fix
is **consumer-side**: bond's §4.C read-allowlist (envelope `v,ts,pid,seq,session,
name`; `send_id`; `event`; `content_sha256`,`content_len`; `anchor`; terminal-
detail) that **EXCLUDES `content_preview`** and any content-prose field. The
redactor itself is **unchanged** (W4). A one-line correction note is also recorded
at the dispatch invariant so the in-source comment no longer misleads.

---

## 6. Provenance

- Substrate: #1 dispatch ground-truth (`exec/findings/dispatch-ground-truth.md`, `[S §x]`)
  + live dispatch source `path:line`.
- Conformance: SPEC-v2 §5.A (token), §6.A (terminal set), §10 (DuckDB-readable),
  §4.C/§4.D (bond-side non-changes).
- W2 decision + value contract: ADR `doc/adr/0019-agent-pid-identity-token.md`.
