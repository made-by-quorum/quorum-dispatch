# fakerepl — deterministic fake claude REPL (A4 submit-discipline gate harness)

A workspace-internal test binary (`publish = false`, **never shipped**) that
emulates claude's externally-observable boot/submit contract closely enough to
exercise the M1 submit discipline (`verify_accepted_then_cr` / `deliver_prompt`)
over a **real PTY** with **real timing** — but with **zero RNG** and no
wall-clock-keyed decisions beyond a single 50ms burst-gap constant. Load
variation comes from the *harness* varying env knobs per iteration, never from
anything random inside this binary.

It is driven by the Level-1 gate in `crates/qd/tests/fakerepl_gate.rs`.

## Contract surface

- **Registry row.** After the jail belt passes, writes
  `$HOME/.claude/sessions/<pid>.json` with the shape `boot::read_pid_status`
  parses: `{"pid": <pid>, "status": "idle", "name": "<name>"}`. Status
  transitions rewrite the file atomically (tmp + rename). The row is removed on
  clean exit (SIGTERM handler or stdin EOF).
- **Application output** (the gate's ONLY turn-count oracle — ADD-6, never echo):
  on submit prints `[turn <n>] accepted bytes=<composer_len> composer_crs=<k>`;
  after the busy hold prints `[turn <n>] done`. Flushed per line.
- **Report JSONL** to `$QD_FAKEREPL_REPORT` (cross-check only): one object per
  `burst` / `cr` / `transition` / `turn` event. Echo-independent facts only.

## Env knobs (set by the harness, no RNG inside)

| var | default | meaning |
|-----|---------|---------|
| `--name <n>` / `QD_FAKEREPL_NAME` | `fakerepl` | registry-row name (find_pid_file key) |
| `QD_FAKEREPL_PASTE_THRESHOLD` | `8` | burst ≥ this many bytes ⇒ PASTE |
| `QD_FAKEREPL_BUSY_MS` | `500` | busy hold after a submit |
| `QD_FAKEREPL_ABSORB_ALL_CRS` | unset | `=1` ⇒ EVERY CR absorbed (never submits) |
| `QD_FAKEREPL_DROP_OVER_BYTES` | unset | tty-queue OVERFLOW model: a single burst **>** N bytes is DROPPED WHOLESALE (no composer content, a `drop` report event, no turn). Models the live ~4096B canonical-tty-queue overflow class (ADR 0009 mode (a)); the negative-control pairing uses `=4096`. The live drop boundary is machine/load-dependent — this knob models THE CLASS, it does NOT assert a specific size. |
| `QD_FAKEREPL_STALL_AFTER_BYTES` | unset | W8 reader-stall seam (D16 window): once cumulative INPUT bytes first reach N, the reader PAUSES (one-shot). Arms the seam. |
| `QD_FAKEREPL_STALL_MS` | `0` | W8: how long (ms) the reader pause lasts after it triggers. |
| `QD_FAKEREPL_STALL_QUEUE_CAP` | `0` | W8: bytes admitted to the composer DURING the pause (counted from the stall trigger). Bytes beyond the cap that arrive while paused are DROPPED (saturation → a `stall_drop` report event). |
| `QD_FAKEREPL_CONVO_JSONL` | unset | W8: path for the conversation transcript. When set, every SUBMIT appends a claude-shaped user record `{"type":"user","message":{"content":"<composer>"}}` and every turn-done appends a stub `{"type":"assistant","message":{"stop_reason":"end_turn",...}}`. serde_json escapes; flushed per line (the SUT's verify step polls this file). |
| `QD_FAKEREPL_SESSION_ID` | unset | W8 end-to-end leg: the registry row gains `"sessionId": "<id>"` so the SUT's registry→sessionId→find_jsonl_path verify resolution chain works against the fakerepl (the scenario places `QD_FAKEREPL_CONVO_JSONL` at the projects-dir path this id resolves to). Unset → row byte-identical to before. |
| `QD_FAKEREPL_REPORT` | unset | path for the JSONL report |

## Burst model (a4-spec §5, deterministic)

stdin chunks arriving **< `GAP_MS` (50ms)** apart coalesce into ONE burst. A
burst is a PASTE iff its total length ≥ `QD_FAKEREPL_PASTE_THRESHOLD`. CR
dispositions:

- **busy** → recorded `cr_while_busy`, absorbed as `\n`, **no turn** (queued
  input, like claude). The busy hold is a TIMED STATE, not a blocking sleep, so
  the loop keeps reading and CRs arriving while busy are observed.
- **`QD_FAKEREPL_ABSORB_ALL_CRS=1`** → every CR absorbed as `\n`.
- **inside a paste burst** → absorbed as `\n`.
- **empty composer** → `empty_noop`: submits nothing (no turn). claude does not
  start a turn for an empty prompt; load-bearing for the overflow model (a dropped
  write leaves the composer empty, so the trailing `\r` must NOT fake a turn).
- **lone non-paste keystroke CR on a NON-empty composer** → SUBMIT the composer.

Independently of the burst model, **`QD_FAKEREPL_DROP_OVER_BYTES=N`** drops any
single burst longer than N bytes wholesale (the tty-queue overflow class). The SUT's
≤1024B chunking keeps every write under any realistic bound, so a chunked payload
passes; an unchunked single ≥N-byte write is dropped — the negative-control pairing.

The slave PTY is put into **raw mode** (`cfmakeraw`) at startup — load-bearing:
in canonical mode a `read()` blocks for a line terminator, so a paste with no
trailing `\n` would never arrive, and `ICRNL` would mangle CRs.

## W8 reader-stall / saturation model (D16 window)

`QD_FAKEREPL_STALL_AFTER_BYTES=K` + `QD_FAKEREPL_STALL_MS=T` +
`QD_FAKEREPL_STALL_QUEUE_CAP=Q` model the D16 silent-mid-truncation window: under
a sustained reader stall mid-chunked-delivery the tty queue saturates and
mid-payload bytes drop silently. The fakerepl reproduces this **deterministically
at the model boundary** (the spec's sanctioned simplification — a strictly
kernel-queue-accurate model is not portably deterministic):

- the reader stall is **one-shot**, arming the first time cumulative INPUT bytes
  reach `K`; it lasts `T` ms of wall clock (`stall_until = now + T`);
- **while paused**, content bytes are admitted to the composer only up to `Q`
  bytes (counted from the trigger); bytes beyond `Q` arriving during the pause are
  **DROPPED** (saturation) — a `stall_drop` report event records the admitted cap;
- once the `T`-ms pause elapses, normal admission resumes, so a payload whose
  middle arrived during the pause lands as **leading + Q + trailing** — SHORTER
  than sent but sharing its leading bytes (the truncation signature the verify
  step keys on).

**Why this is deterministic given the SUT contract:** the number of bytes lost is
`(bytes arriving during the T-ms pause) − Q`, and the arrival rate is fixed by the
SUT's chunk pacing (≤1024B chunks, 150ms production / 80ms gate inter-chunk
settle), not by anything random. The gate asserts `recorded < expected` (not an
exact byte count), so the row is robust to small timing jitter while the
truncation itself is guaranteed whenever `T` is long enough to outrun the cap at
the SUT's pacing.

**Named simplification (explicit):** the byte loss is injected at the
**model/admission boundary** (the reader simply does not push the over-cap bytes
to the composer during the pause) rather than by faithfully overflowing a real
kernel PTY input queue. This keeps the repro deterministic and platform-portable;
the observable effect (a shorter user record sharing the leading bytes) is
identical to the live silent-loss mode.

Unset knobs = **byte-identical** behavior to before W8 (the pre-W8 gate rows are
untouched): with `STALL_AFTER_BYTES` unset the stall window never arms and every
byte is admitted; with `CONVO_JSONL` unset no transcript is written.

## Jail refusal belt (a4-spec §5, redesigned per spec-red-team R3)

Refuses (stderr naming the failed check + **exit 13**) unless ALL hold:

- (a) `HOME` matches `*/qdrg-runs/*/home` (component-based, not substring);
- (b) with `root := dirname(HOME)`: `QD_HOME == root/qd_home`,
  `ZMX_DIR == root/zmx`, `TMPDIR == root/tmp`.

Derived ENTIRELY from the EXPORTED isolation set (`test/golden/lib/jail.sh`
:139-146). It does **not** depend on `JAIL_ROOT`/`JAIL_RUNID`/`JAIL_PREFIX` —
those are shell-local in jail.sh (NO `export`), so a child across the zmx
boundary never sees them. (b) is the COHERENCE check, so a partial spoof (HOME
jail-shaped but QD_HOME elsewhere) is refused.

## Locating the binary from the gate test

`fakerepl` is a *different* crate, so `CARGO_BIN_EXE_fakerepl` is unavailable to
`qd`'s tests. `cargo test --workspace` builds all workspace binaries first, so
`<target>/<profile>/fakerepl` exists. The gate derives `<target>/<profile>` from
the running test exe (`.../target/<profile>/deps/<testbin>`) — robust to
debug/release and a custom `CARGO_TARGET_DIR`. If absent (e.g. `cargo test -p
qd` without a prior workspace build) it shells out to `cargo build -p fakerepl`
once.

## MEASURED constants (W7 — measured, not assumed)

### 50ms burst-gap vs portable-pty coalescing

The `coalescing_note_measures_pty_burst_boundaries` test writes two 100-byte
halves to the PTY master with a **120ms stall** between them (well over the 50ms
gap) and reads back the burst sizes the fakerepl recorded.

**Observed on this machine (macOS arm64, portable-pty 0.9):**

```
split 100B + [120ms gap] + 100B  ⇒  observed burst sizes = [100, 100]
```

i.e. a >50ms stall reliably produces a burst boundary (two separate bursts of
100B each), and no single burst swallows both halves. A 4096-byte single write
(`diag_raw_4k_write_burst_shape`) is delivered as ONE 4097-byte paste burst
(trailing `\r` absorbed) followed by the lone remediation CR as a 1-byte
non-paste burst — confirming the gap constant cleanly separates a paste from its
remediation keystroke. The 50ms constant therefore sits comfortably between
portable-pty's intra-write coalescing (sub-millisecond) and a deliberate
inter-keystroke stall.

**Fragmented-paste row scoping (in-phase red-team #5).** The original
`l1_fragmented_paste_across_stall_lands_exactly_one_turn` row PRE-LOADS the
composer with two raw sub-burst writes and only THEN runs the discipline — it
proves the remediation handles a fragmented composer, but it bypasses the delivery
CADENCE, so the oracle (fakerepl burst-gap) and the SUT (the two-write delivery)
do not exercise their timing independence on the real path. The R4 row
`r4_fragmented_paste_inside_delivery_lands_exactly_one_turn` closes that seam: it
fragments the text write INSIDE the `deliver_idle_two_write` helper (two sub-bursts
straddling a >50ms stall, then the separate `\r`), so the >50ms gap is crossed on
the production delivery path, not a pre-loaded composer.

### busy-window vs the SUT's status-poll interval (HARNESS CONSTRAINT)

`deliver_prompt` polls the registry status every **250ms**
(`SubmitOptions::default().poll_ms` — a fixed property of the SUT we drive, not a
knob). A busy window shorter than `poll_ms (250) + the fakerepl's ≤50ms
burst-close latency` is **fundamentally unobservable**: the discipline reads
idle across the whole window and (correctly) judges the prompt un-accepted,
producing FALSE drops that are a harness artifact, not a discipline failure.

Real claude busy windows are **seconds** (boot-to-accept ~1.5s, responses ~15s),
so a sub-250ms acceptance window is not a realistic signal. The gate therefore
floors `QD_FAKEREPL_BUSY_MS` at **700ms** in every `deliver_prompt`-driven row
(and the soak varies 700–1500ms), instead of the spec's nominal 100–1500ms
range. This deviation is surfaced to the phase lead and documented in
`soak_knobs()` in the gate.
