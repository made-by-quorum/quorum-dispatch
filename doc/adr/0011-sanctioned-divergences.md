# ADR 0011: Sanctioned divergences — the §S stub substrate (R3)

> **RENUMBERED from ADR-0010** at PR #24 pre-merge (orc-4 merge-ruling fix #2):
> the number collided with `0010-keychain-fallback.md` (A5). **Citation note:**
> artifacts whose sha is PINNED IN MATCH-PROOFS (scenario `.sh` files, the stub)
> were recorded before this renumber and still cite "ADR-0010 §(a)" etc. — those
> citations refer to THIS document (sanctioned divergences), not the keychain
> ADR. They are left byte-frozen deliberately: editing a comment in a sha-pinned
> artifact would invalidate its proofs and force re-records for a cosmetic fix.
> All non-pinned citations (coverage-matrix, ADR cross-refs, gate report) say
> ADR-0011.

**Status:** Proposed (lead + orchestrator review)
**Date:** 2026-06-05
**Supersedes/extends:** the boot-readiness note in ADR 0004 + ADR 0005-dialog-free-boot.
**Phase:** 0b Part 2 (golden-master oracle). (Filename note: the Part-2 plan §S/R3
called for `0005-sanctioned-divergences.md`; 0005 was already taken by
dialog-free-boot, so this lands as 0008. Same artifact, same content. RENUMBERED 0008->0010 (cross-branch dedup, orc-assigned — A4 carries 0008-went-busy-exit-contract + 0009-idle-path-two-write, which merge ahead of this branch and collide with 0008).)

## Context

The 0b golden oracle measures the qd ENGINE, not Claude. Contract-bearing rows
(boot-readiness, send:pty queue-to-busy + JSONL `--wait`, history, attach/detach/
reattach, relay-health, buildClaudeCmd, zmx-dir resolution) are recorded by driving
the pinned-TS qd (PIN `0d0fa9ed4800efb1309eca2311345c48af2c4932`, zmx 0.6.0) against
a spec-faithful DETERMINISTIC counterpart stub (`test/golden/lib/stub_claude/`), NOT
a real Claude and NOT the dryrun fake. The orchestrator RATIFIED §S with four riders
(R1 stub-pinning, R2 stub-seam negative control, R3 this ADR, R4 recorded-exclusions
stay non-green). This ADR is R3: ONE sanctioned-divergence ADR with THREE named
sections. The A7 cross-machine divergence table cites it.

## Decision

### (a) Parity-instrument vs realism-instruments split

This oracle is the PARITY instrument: it proves RUST-QD == TS-QD on the engine's
observable CONTRACT, via a SYMMETRIC deterministic counterpart (the SAME stub drives
both TS recording now and Rust replay at later gates — R1 pins its sha into every
RECORDED-FROM + MATCH-PROOF it backs). It deliberately does NOT exercise real-Claude
realism. REALISM is owned by SEPARATE instruments (orc ruling adopted into §S R3):

| Instrument | Owns | Why separate from this oracle |
|------------|------|-------------------------------|
| **A4 real-Claude ×20 smoke** | submit discipline / `--wait` against a REAL Claude REPL, ×20 | Real paste-burst + boot timing; the stub is deterministic by design, so it cannot surface real-timing flakes. |
| **A7 cross-machine acceptance** | full QDQA battery (QDQA_SRC-swapped) + cross-machine spawn on real hardware | Real zmx/Claude on a second machine; this oracle runs on brano + Lima only. |
| **C2 dogfood** | the org running on the Rust qd for real work | Emergent real-world behavior no fixture can pre-script. |

Rationale (orc): byte/semantic parity against a symmetric counterpart is the cheap,
complete, deterministic test of the engine's contract; realism is a DIFFERENT
question answered by instruments that pay the cost of real non-determinism. Keeping
them separate is why this oracle can be deterministic (double-record byte-identical)
without laundering fake behavior into the gold — the stub implements ONLY the
surfaces qd reads, each derived from pinned-TS source.

Why not real Claude HERE: (1) API token cost ×N gate runs; (2) credentials would be
copied into the JAILED HOME — a jail-design violation (ADD-4: HOME is load-bearing
for invisibility); (3) non-determinism breaks double-recording (the teeth). Why not
the dryrun fake: it implements NONE of the contract (red-team confirmed) — fake
behavior would be laundered into golden expectations.

### (b) Per-row stub-fidelity divergences

Each stub-backed row relies on specific stub behaviors; every behavior is derived
from pinned-TS source (cited in the stub header table) and the row carries the stub
sha in its RECORDED-FROM + MATCH-PROOF (R1). Stub identity at recording:
`stub-claude-1.5.0`, main sha256
`71a8f622a3881577f1197b3f7408229eedcec6dc43dc372fae52a65093285f79`.

| Row (stub-backed) | Stub behaviors it relies on | Pinned-TS citation |
|-------------------|-----------------------------|--------------------|
| **new / boot-readiness** | popup render + blind-Enter dismiss (stub #2); PID file `~/.claude/sessions/<pid>.json` name-matched, status idle (stub #3/#4) | lifecycle.ts:177-235 (blind-Enter loop), :135-175 (findPidFile/readPidStatus); session.ts:64-75 (PidEntry) |
| **send:pty paste-burst (queue-to-busy + --wait)** | idle↔busy status (stub #4); busy-HOLD so qd observes `busy` → send-queue (stub #6); JSONL user+assistant pair, exact user-record text for the anchor (stub #5); JSONL-at-boot so `--wait` finds the file | utils.ts:297-299 (decideSendPty), :341-346 (findUserAnchor), :359-365 (decideWait); send.ts:128, :154-294; submit.ts:87-114 |
| **relay-health** | child relay server binds `$QRM_RELAY_PORT`; sidecar `{sessionId,port,pid,status}`; GET /health; POST /message → `{message_id}`; relay child PID under claude PID (ls-join) (stub #7/#8) | session.ts:148-212 (sidecar + /health + RelayHealth), :845-873,922 (ls join by PID-parentage); send.ts:414-426 (/message) |
| **history / attach-detach-reattach** | SBLINE backlog generator emits N numbered rows to the PTY while DETACHED → zmx retains them server-side (stub backlog) | EMPIRICAL-RESULTS.md (zmx server-VT retention, L6); send.ts:146 (zmx history) |
| **buildClaudeCmd** | stub dumps its received launch argv → the exact flags buildClaudeCmd produced | utils.ts:507-513 (buildClaudeCmd), :226-227 (CLAUDE_FLAGS const), :258-271 (--name) |
| **zmx-dir resolution (ZMX_DIR tier)** | stub boots so a real zmx session lands a socket under ZMX_DIR (the explicit-tier OUTCOME) | utils.ts:68-82 (resolveZmxDir) |

Fidelity boundary (sanctioned): the stub implements ONLY these surfaces. It does NOT
model real Claude inference, tool use, thinking blocks, ANSI repaint fidelity, or
multi-turn beyond what a row drives — none of which qd OBSERVES on the recorded rows.
A divergence in any UN-modeled surface is out of this oracle's scope by construction
(it is a realism question — section (a)).

### (c) Blind-Enter (boot-readiness keys on the EVENT, not the keystroke)

The pinned TS `qd new` STILL dismisses the dev-channels popup with a blind-Enter loop
(`lifecycle.ts:177-235`, CONFIRMED unfixed at the pin: from 2s after spawn it sends
`\r` into the session PTY at intervals until the PID file appears, rationalized
:211 as "harmless"). It is NOT harmless — the Enters are untargeted and would accept
whatever dialog is on screen (folder-trust, OAuth, future consents). The Rust engine
DELETES this loop (A2 hardening #4; ADR 0005-dialog-free-boot): readiness becomes the
EVENT contract — PID-file APPEARANCE + a status transition to ready (idle) — with zero
stock-path keystrokes; dialogs answered only by a content-matched delegated-consent
answerer.

So the boot-readiness corpus rows (new-session-trace, boot-readiness-event) key on
the EVENT, recorded as the DETERMINISTIC outcome (`pidfile_appeared=1`,
`name_matched=1`, `status_ready_idle=1`), NOT the timing-variable blind-Enter byte
trace. The stub plays the TS side faithfully (it consumes the blind-Enter then writes
the PID file), so the corpus records TS's Enters as the divergence BASELINE; the
oracle measures the engine against the EVENT. This is a sanctioned, deliberate
NON-PARITY: the Rust engine will not reproduce the blind Enters, and the
boot-readiness comparator (ADR 0004 `boot-readiness-event` / `assert_boot_ready_event`)
is the contract both sides are judged against.

## Consequences

- The oracle is deterministic (double-record byte-identical) AND honest: no fake
  behavior is laundered into gold, because every stub behavior traces to pinned-TS
  source and the stub sha is pinned into the proofs (R1).
- Realism gaps are explicitly owned elsewhere (A4/A7/C2), so this oracle's green is
  never mistaken for "works against real Claude."
- The blind-Enter divergence is recorded as the baseline; the engine's deletion of it
  is the sanctioned non-parity, judged by the EVENT comparator.
- Rows un-recordable even against the stub are `EXCLUDED-<reason>` (R4), never ticked
  (e.g. preflight: no too-old zmx at pin).

## Addendum (2026-06-05, 0b DELTA-STRENGTH W3a): scenario-INLINE recording modes

Section (b)/(c) state the general rule: **the stub's negative-control / behavioural
SEAMS never fire during recording** — a recorded golden is the stub's DEFAULT
observable behaviour, and a seam (STUB_WITHHOLD_PID, STUB_DEAD_HEALTH, …) is set only
by the mutation-real negative controls, never on the corpus. The DELTA-STRENGTH
strengthened rows introduce a NARROW, EXPLICIT EXCEPTION:

> Seams never fire during recording **EXCEPT scenario-INLINE recording modes** — a
> seam SET DELIBERATELY in the committed scenario's `scn_run` (exactly the existing
> `STUB_BUSY_HOLD_MS` precedent), so record AND replay apply it identically with NO
> new harness mechanism. The mode is defended by **scenario-sha-in-MATCH-PROOF** (any
> edit that drops the inline mode invalidates the proof → forces a re-record) and
> stamped **documentary** in `RECORDED-FROM` (`recording_mode=<SEAM>=1`; provenance,
> not load-bearing — the load-bearing defence is the scenario sha).

Named inline recording modes:

| Seam | Row | Purpose | Status |
|------|-----|---------|--------|
| `STUB_COUNT_PRE_PID_STDIN` (W2.3) | new-session-trace (W3.3, P2) | exposes the pre-PID-file stdin char count via the boot-stats sidecar; the row asserts it is `0` by stub construction (independent of TS blind-Enter timing). | **LANDED** (W3a) — set inline in `new_session_trace.sh` scn_run; documentary stamp in RECORDED-FROM. |
| `STUB_NO_QUEUE` (W2.1) | send-pty-paste-burst (W3.1, P3) | removes the stub's TTY-buffer queueing so a busy-queued message lands iff the engine delivered it. | **MUTATION-ONLY CONTROL** (orc-4 ruling 14:50 EDT) — demoted from a recording mode: it is NEVER set on the corpus; it drives the committed mutation-real control (mutation/r2-seams/no_queue_burst.trace) that MUST flip the strengthened busy row RED (the busy-window burst is discarded). |
| `STUB_RAW_STDIN` (W3.1, v1.8.1) | send-pty-chunked-idle (W3.1, P3) | at startup flips the stub PTY stdin termios to RAW (tty.setcbreak, clears ICANON/ECHO) so the cooked-mode canonical-line bound (macOS MAX_CANON=1024) does NOT cap the chunked-IDLE >=4KB write — the stub then reads like real Claude's raw-mode TUI. | **LANDED** (W3.1, orc-4 Option C ruling) — set inline in `send_pty_chunked_idle.sh` scn_run; documentary `recording_mode=STUB_RAW_STDIN=1` in RECORDED-FROM. DORMANT default (PTY stays COOKED; the W3.7/P10 termios row's cooked icanon=1/echo=1 default is byte-identical, replay-verified in .restamp-evidence-w31.txt). |
| `STUB_WITHHOLD_PID` (R2/W3.4a) | neg-boot-timeout (W3.4a, P4) | the stub renders the popup, consumes the dismiss CR, then HOLDS OPEN without ever writing the PID file; qd's readiness wait fails. Recorded qd FAILURE SHAPE: rc=1 + readiness-timeout stderr token + no PID file. | **LANDED** (W3b) — set inline in `neg_boot_timeout.sh` scn_run; documentary stamp in RECORDED-FROM. |
| `STUB_WITHHOLD_JSONL` (R2/W3.4b) | neg-wait-no-reply (W3.4b, P4) | the stub appends the user record + reaches idle but withholds the assistant reply; `send:pty --wait` completes (not a timeout) and prints `(no text response)`, rc 0. Records the R4-honest no-reply shape. | **LANDED** (W3b) — set inline in `neg_wait_no_reply.sh` scn_run; documentary stamp in RECORDED-FROM. |
| `STUB_DEAD_HEALTH` (R2/W3.4c) | neg-relay-unhealthy (W3.4c, P4) | the stub's /health answers 503 status=dead while the sidecar stays present. Records that the engine's ls-join is SIDECAR-DRIVEN + HEALTH-INDEPENDENT (no qd surface consumes /health for the relay at the pin — flagged spec-premise divergence). | **LANDED** (W3b) — set inline in `neg_relay_unhealthy.sh` scn_run; documentary stamp in RECORDED-FROM. |
| `STUB_TWO_STAGE_PID_WRITE` (W2.2/W3.4d, P11) | neg-two-stage-tolerance (W3.4d, P4) | every PID write lands DIRECT in two stages (partial prefix + ~1500ms gap + complete), bypassing the atomic rename. Records the engine's read-tolerance OUTCOME ONLY (boot reaches idle, ls never crashes, session visible after) — never the racy partial state. | **LANDED** (W3b) — set inline in `neg_two_stage_tolerance.sh` scn_run; documentary stamp in RECORDED-FROM. |

## Addendum (2026-06-05, 0b DELTA-STRENGTH W3.1 RULED): chunked-path coverage

**Resolution of the prior W3.1 BLOCKED note (orc-4 Option C ruling, relay 14:50 EDT;
disposition table at exec/0b-panel-dispositions.md, P3 row, second premise
correction).** The panel's original P3 premise — a STUB_NO_QUEUE recording mode over
an engine-side hold queue — was already corrected once (no engine hold queue exists
at the pin; queue-to-busy relies on the TTY/kernel input buffer). The first
correction recorded the chunked-path coverage as a **NEW ≥4KB IDLE-path row**
(`send-pty-chunked-idle`, the pin's actual chunked surface). A SECOND premise
correction surfaced during W3.1 implementation and is recorded here.

### The cooked-mode MAX_CANON bound (the second premise correction, C3)

The ≥4KB IDLE row could NOT record green against the **cooked-mode** stub either.
The mechanism, nailed empirically (4 controlled in-jail probes at pin 8c59ec4):

| send:pty --wait idle payload | result |
|---|---|
| 1000 B | lands (rc 0, user record present) |
| 1023 B | lands (rc 0) |
| 1100 B | DROPPED (rc 1, no record) |
| 4178 B (full burst) | DROPPED (rc 1, --wait timeout, 0 user records) |

The boundary is EXACTLY **macOS `MAX_CANON = 1024`** (`getconf MAX_CANON` confirms).
Cause: `deliverIdleTwoWrite` (submit.ts:287-334 @ 8c59ec4) delivers the WHOLE message
via `sendTextChunked` then a SEPARATE `\r` (**submit.ts:298-301**: `sendTextChunked`,
settle, `send("\r")`) — there is NO inter-chunk CR, so the engine sends ONE canonical
line. The stub's PTY runs in **COOKED mode** (ICANON — the sanctioned W3.7/P10
fidelity boundary: "the engine does not set raw mode; the stub is a line reader").
Cooked line discipline accumulates the whole ≥4KB text in one canonical line with no
terminator until the trailing `\r`, overflowing the MAX_CANON=1024 buffer and dropping
everything past byte 1024. **Real Claude runs the PTY in RAW mode** (its TUI), where
MAX_CANON does not apply — which is why the pin's own live probe ("5×1024B + 150ms +
separate `\r` → ok", submit.ts:146-147 comment) worked against real Claude.

### The ruled resolution

A NARROW, env-gated stub seam **`STUB_RAW_STDIN`** (v1.8.1; orc-4 Option C) flips the
stub PTY stdin to raw (clears ICANON) ONLY when set, so the chunked-IDLE ≥4KB write
is received byte-loss-free and the row records GREEN — genuinely exercising the
multi-chunk path IN the corpus. It is set INLINE in `send_pty_chunked_idle.sh`
(principle 3), DORMANT by default (the W3.7/P10 cooked termios row is byte-identical,
replay-verified). Its load-bearing-ness is proved from BOTH sides (C2 mutation
control + PTY selftests 12-13): seam ON a >MAX_CANON line lands, seam OFF it is
dropped cooked. The 1.8.0→1.8.1 dormancy + re-stamp evidence is
.restamp-evidence-w31.txt.

### Named corpus limitation (R2)

The **strengthened busy row (`send-pty-paste-burst`) keeps a SUB-1KB (422 B) payload**
— this is a **NAMED CORPUS LIMITATION**: the cooked-mode busy-queue path cannot host a
≥4KB single-canonical-line burst (same MAX_CANON bound), and the busy path's
raw-mode equivalent is out of scope for this oracle. The busy row therefore covers
queue-to-busy + ordered drain + anchor + reply at sub-1KB; the ≥4KB chunked delivery
is covered by the IDLE row (raw mode) and, for the REALISM dimension (real Claude, real
tty queue, real timing), by the **A4 fake-REPL harness** — the
`DROP_OVER_BYTES`/chunked coverage in **ADR-0009 (idle-path two-write)** /
`exec/a4-gate-report.md`, the named carrier of busy-path + real-timing chunked
coverage per section (a)'s parity-vs-realism split. No silent overclaim: this oracle
proves the engine's chunked-delivery CONTRACT (zero loss across chunks) deterministically;
real-timing chunked realism is A4's.

## Addendum (2026-06-15, WP-B7): the `qd ls` render-surface flip + `qd resume` always-headless + the a3-cli `ls --help` re-mint

WP-B7 reconciles three ratified golden-deltas that earlier WPs deferred. Each is a
NAMED, intentional divergence from the recorded corpus — never a silent mask.

### (B7-1) `qd ls` table→JSON render-surface auto-flip (behavioral)

`qd ls` (and the bare `qd` default action) now AUTO-DETECTS its render surface by
the WP-B-CS-1 driver doctrine (*I/O follows who drives*): an agent/pipe caller
(non-TTY, or a Claude-session env marker) gets the **JSON** machine surface; a
human at a TTY gets the **table**. `--json` and the net-new `--table` are the two
explicit overrides on that one surface axis (`--table` forces the human table even
for an agent; `conflicts_with` `--json` only). `--short` stays a CONTENT modifier,
subordinate to the surface decision (so an agent `qd ls --short` auto-flips to JSON
exactly as `--json --short` does today; `--table --short` is the agent short-text
escape hatch). This DELIBERATELY diverges from the old always-table piped default.
Authoring change: `driver::ls_render_mode` wired into `verbs/ls.rs::run_inner`
(qd-supervisor-9-endorsed, qd-supervisor-11-ratified). The cargo-floor `ls`
text-mode tests that asserted table output for agent/pipe callers were ADAPTED
(inject `--table` to preserve the text-surface assertion, or re-assert the JSON
shape) — coverage preserved, not masked (`punch_b5_ls_live.rs`, `p0_id_matrix.rs`).

### (B7-2) `qd resume` always-headless (a5-lifecycle / a5rec_resume golden delta)

`qd resume` is now ALWAYS headless (Fork A, `resume.rs:218` GUARDRAIL-2, authored at
B-CS-1 D3): it drops the OLD interactive zmx/attach happy-path and routes to a
headless stream-json relaunch (a human re-entering a live session is `qd connect`,
not `qd resume`). The a5-lifecycle goldens (`a5_lifecycle_live.sh` G-L resume rows;
`fixtures/a5-lifecycle/normalized/resume.txt`) encode the OLD zmx/attach behavior.

- The a5 **error-shape** goldens (`a5rec_resume.sh` → `resume.txt`: no-such-session,
  unsafe-zmx-name, recorded-cwd-missing) fire in resume's RESOLVE/PREFLIGHT path,
  BEFORE the headless launch — so they are behavior-stable across the flip and stay
  byte-INERT (verified non-vacuously at the merged tip: the preflight checks are
  upstream of GUARDRAIL-2, which only governs the LAUNCH route).
- The a5 **happy-path** live re-mint (`a5_lifecycle_live.sh`, Rust+Lima/zmx, macOS)
  is NOT runnable on Linux CI → recorded here as **NAMED-DIVERGENCE-PENDING-REMINT**:
  a future Lima re-mint must reconcile the G-L resume rows + `resume.txt`'s
  zmx/attach happy-path to the always-headless relaunch. (Genuinely infra-blocked.)

### (B7-3) a3-cli `08-help-ls.txt` re-mint (`qd ls --help` — `--table` + B5 drift)

`test/golden/dryrun/a3-cli/08-help-ls.txt` is RE-MINTED from the RUST `qd ls --help`
(the `2f1c841` TS pin cannot produce these bytes). Reproducer:
`test/golden/dryrun/a3-cli/remint_ls_help.sh` (double-run determinism enforced).
**Split attribution** (red-team verdict DoD):

- **WP-B7's SOLE net-new line** is `--table  Force the human table (override the
  JSON auto-default)` — the (B7-1) flip's escape hatch, made discoverable in help.
- The `--live` flag block, the over-cap `… N more (qd ls --all)` trailer paragraph,
  and the `--all and --live are uncapped` limit wording are **PRE-EXISTING B5
  punch-item-2 drift**: they were added to `help::LS` when `--live`/the trailer
  landed in B5, but the a3-cli corpus was never re-minted then. They are reconciled
  on this row now only because B7 regenerates it anyway.
- Verify the split: the re-minted stdout body's non-`--table` lines BYTE-MATCH
  `qd ls --help` @ `d53a837` (= `help::LS` @ `d53a837`, which already carried the
  `--live`/trailer surface): `git show d53a837:crates/qd/src/bin/qd/help.rs`. The
  remaining ~21 stale a3-cli rows (other verbs' help / error shapes) are a SEPARATE
  B-wide a3-cli reconciliation follow-on (NOT WP-B7's scope).

## Addendum (2026-06-16, WP-B-CS-2-LIVE): the cutover terminal-attach env-deferral (Q2b)

WP-B-CS-2-LIVE built the live `qd connect` OBSERVE run-loop + the turn-boundary
cutover EXECUTION (teardown → `revive_claude` → buffered-input-first → attach). The
qd-supervisor-12 ruling (Q2a/Q2b) required a real-claude SEED proving the path ONCE
end-to-end on a GENUINE live busy headless window, driving `revive_claude` AS FAR AS
IT PHYSICALLY RUNS and recording the env boundary EMPIRICALLY — NOT pre-conceded.

### (B-CS-2-LIVE-1) cutover native-TUI/zmx terminal attach — `ENOTTY`, a5/Lima class

The seed (`crates/qd/tests/headless_observe_real_claude_seed.rs`,
`real_claude_observe_cutover_seed`, `#[ignore]` + explicit, env -i isolated HOME with
copied creds + a PRE-TRUSTED `.claude.json` — `hasTrustDialogAccepted` +
`bypassPermissionsModeAccepted` + onboarding done, so it is NOT launch-dialog-blocked)
drove the FULL path against a genuine `claude` 2.1.178 turn and proved, first-hand:

- **connect→OBSERVE rendering CONTROL FACTS ONLY** through the REAL socket — the model
  is instructed to emit a tripwire token (real assistant text on the live stream); it
  NEVER appears in the read-only observe output (§2a hard line, end-to-end).
- the **busy→idle** transition (seeded busy from the daemon-written row status, then
  the real `RepublishTurnEnd`): `in-turn: yes` → `in-turn: no`, with the GENUINE claude
  `session_id` rendered at the boundary.
- the **cutover DEFERS** mid-turn (`Cutover queued`) and **FIRES at the real turn
  boundary** (`Turn boundary reached`) — never mid-turn.
- **teardown → `revive_claude` ran FOR REAL and SUCCEEDED through the boot ready-wait**
  (ADR-0005 EVENT waiter): the revived native session reattached
  (`[retach: reattached to 'hlseed' (detach: Ctrl+\)]`).

The path then stops at the FINAL terminal handoff:

```
qd connect: embedded mux: attach: ENOTTY: Not a typewriter   (exit 1)
```

**This ONE step — `mux.attach` handing the live native TUI to a controlling
terminal — is a NAMED ENV-DEFERRAL** (the a5/Lima terminal-attach class, B7-2's
sibling). `ENOTTY` is inherent to an automated `#[ignore]` harness: it has piped
stdio and NO controlling TTY, so the terminal can never be handed over. A real human
running `qd connect` in a real terminal HAS a TTY and the attach succeeds — the
limitation is the test substrate, not the product. Everything UP TO AND INCLUDING
teardown + `revive_claude` + the attach ATTEMPT runs for real in the seed; only the
literal TTY handoff is env-blocked. The cutover-execution LOGIC (teardown ordering,
never-mid-turn gate, buffered-input-first, headless teardown) is additionally proven
DETERMINISTICALLY by the unit/fixture path (`observe.rs` cutover-exec tests +
`headless_observe_cutover.rs` (A)/(B)), so NOTHING LOGICAL is deferred — only the
physical terminal attach (ruling Q2c satisfied). A future PTY-allocating harness (or a
real-terminal run) can exercise the final attach; recorded here as
**NAMED-DIVERGENCE-PENDING-PTY-HARNESS**. (Genuinely env-blocked, same class as a5.)

## Addendum (2026-07-08, opencode D1 Child D): acp/claude-code transport-loss = refuse-and-surface, identity preserved (named divergence)

> **Spelling note, 2026-08-24 — the ruling is unamended; only the id it names has been
> re-spelled.** `acp/claude-code` and `acp/opencode` were `Harness` variants when this
> addendum was written. ACP is `Mode::Acp` now and the lanes are **`claude-code/acp`** and
> **`opencode/acp`** (`doc/tbd/provider-architecture/17-acp-is-a-lane.md`). Read every
> `acp/claude-code` below as `claude-code/acp` and every `acp/opencode` as `opencode/acp`.
> Nothing about the divergence moves with the spelling: the scope is still exactly one
> lane, the refusal is still the unconditional disposition, and the identity tombstone is
> still written before it. The old provider strings continue to parse permanently — the
> tombstones this addendum introduced are themselves one of the reasons they must.

**Ruling:** clerk-4's Arm-B ratification (bond note `01KX01BY7G`, 2026-07-08, under
Pete's overnight full-executive grant; resolves the conditional ruling `01KWZWT83B`
on Child C's live probe evidence). Same class as the codex/opencode divergence
ruling `01KWZ80MXS`.

**The divergence.** D1's target — auto-deliver graceful degrade on transport loss —
realizes on **pi only** (pi's own ratified, latch-free floor regime,
`provider/pi/floor.rs`, untouched). `acp/claude-code` joins `codex` and
`acp/opencode` as a NAMED divergence: on transport loss (pre-send, post-send, or
mid-wait) `qd send`/`qd wait` **REFUSE and surface** the human-recovery line
("… not reachable (try qd resume …)", exit 1) — with one addition the other
divergences don't need: **the session's identity is preserved** in a qd-owned
store before the refusal (below). No auto-deliver path exists for this provider:
the Child-B degrade+latch+companion-drive machinery was **removed, not gated**
(structural unreachability — `send_relay.rs::run_acp_send`, `wait.rs::run_acp_wait`,
`provider/acp/ladder.rs`; the former `verbs/acp_floor.rs` is deleted).

**Why (evidence-backed, Child C live probes).** The floor drive's only revival
seam is `claude --resume <session_id>`, which is structurally impossible for the
only degrade-eligible population — a zero-structured-turn session has no
conversation to resume ("No conversation found", exit 1, live-proven). A clean
Arm A (an existing path delivers) is therefore impossible, and the Arm-A drive
REDESIGN (fresh-start companion / seed materialization) was ruled a speculative
redesign on a lane with zero prior production use on this box, and DECLINED.

**Identity preservation (the arm-independent fix).** The claude CLI's own
dead-pid janitor reaps `~/.claude/sessions/<pid>.json` ~1s after daemon death
(live-proven 3/3; qd makes zero unlink/rename on the row) — so a transport-lost
session's identity was erased no matter the disposition. The fix: at every
loss-refusal, qd first writes an **identity tombstone** —
`<QD_HOME || ~/.quorum/dispatch>/state/tombstones/<session_id>.json`
(`dispatch::tombstone`; atomic tmp+rename, latest-wins with the first-loss
timestamp carried forward) — recording session_id, name, pid, cwd, provider,
endpoint, transcript path, loss reason, timestamps. The refusal stderr names the
record's path ("identity preserved at …"). Scope: `acp/claude-code` only;
codex/opencode refusals stay byte-identical and write nothing.

**Reverts/levers.** `QD_ACP_PTY_FLOOR_DISABLE` (which gated only the retired
drive) is now a NO-OP — refusal is the unconditional disposition it used to
select. The `RegistryEntry::transport="pty"` latch is write-retired; a row
bearing one (never-deployed dev-binary writes only) reads conservatively as
unavailable ⇒ refuse. `structured_send_issued` (the exactly-once wire marker)
is still recorded unconditionally — history truth, no longer a disposition
branch. If Pete later wants real acp/claude-code degrade-robustness, it is a
scoped FUTURE effort behind clerk-4's gate (the lever), not a revert of this
divergence.

## Addendum (2026-08-26, qd-start-agent-default): a detected agent's `qd start` on the claude PANE lane CREATES

`qd start <name>` on `claude-code/mux-pane` no longer refuses an auto-detected
agent caller. A caller the surface detects as an agent — an agent env marker
(`QD_SESSION_ID` / `CLAUDECODE`), or a pipe — now takes the interactive native-TUI
create path, with or without `-p`, exactly as a human at a terminal does. An
EXPLICIT `--headless` keeps both of the refusals it used to share with the
auto-detect, byte-for-byte in their stable heads.

**Why this is a flip and not a bug fix.** It was neither, when it was authored.
WP-B-CS-1 (D2) routed `{Agent, has_prompt}` to a REAL second launch — a one-off
`claude -p … --output-format stream-json` run — and `{Agent, no prompt}` to Fork
B's refusal, because a headless `claude -p ""` is a degenerate no-op turn. The
auto-detect was choosing between two LANES, and refusing the empty one was right.
The P4DB drive-burn (§6) then removed the `-p` stream-json launch and replaced it
with a teaching error. From that commit forward the claude pane lane had exactly
ONE create path — Interactive — and the driver auto-detect was choosing only which
of two ERRORS an agent read for asking to start a session. Every other pane lane
in the fleet (`codex/mux-pane`, `pi/mux-pane`, `pi/extension`, and both ACP
residents) already answered that same request by creating. So the divergence being
removed here is between claude's pane lane and its own siblings.

**Where the decision lives.** `bin/qd/driver.rs::start_route`, whose signature
grew the caller's `DriverOverride`: `start_route(over, driver, has_prompt)`. That
argument is the whole mechanism — `Driver::Agent` arrives two ways that used to be
interchangeable (DETECTED, and DEMANDED via `--headless`) and the resolved driver
alone cannot tell them apart. The call site in `verbs/lifecycle.rs::run_start`
passes the same `DriverOverride::from_flags(headless_flag, interactive_flag)` value
it already folds into `resolve_driver_real`, and stays gated behind `claude_pane`,
so no other lane is touched. It is deliberately NOT implemented by setting
`interactive_flag`: that flag also feeds `CreateTopology::Interactive`, which for
codex/pi selects a different LANE (`Lane::for_create`).

**What did NOT change: agents still never get a terminal.** `attaches_after_start`
is still resolved with `DriverOverride::None` at the call site and still answers
`false` for every `Driver::Agent`. An auto-detected agent now gets a created,
tracked, attachable session and its exit code back; the mux pane stays where it
was. That predicate carries MORE weight after this flip than before it — it is now
the only thing between a bare `qd start` inside a Claude session and a pane that
caller cannot leave — and its composed test
(`agent_marked_interactive_start_does_not_attach`) is unchanged and still green.
`-p` on the create path is likewise unchanged: it is delivered post-create by
`deliver_prompt` under the 0/10/1 went-busy exit contract, not by a second launch.

**Tests ADAPTED, not deleted** (the WP-B7 discipline: every pinned row that
encoded the old behavior is rewritten to state the new one, with the reasoning in
the doc comment):

- `bin/qd/driver.rs` unit rows. `start_route_agent_with_prompt_is_headless` and
  `start_route_agent_without_prompt_refuses` became
  `start_route_explicit_headless_{with,without}_prompt_*` — the refusals are pinned
  on the FLAG now. `agent_marker_at_tty_no_prompt_routes_to_refuse` became
  `…_routes_to_the_create_path`, and asserts `Driver::Agent` explicitly so the flip
  cannot be misread as the DETECT having gone soft. Net-new:
  `start_route_detected_agent_creates_with_or_without_prompt` (the flip itself) and
  `start_route_matrix` (all ten `{override × driver × prompt}` rows, including the
  structurally-unreachable `(Interactive, Agent)` corner).
- The **PTY-LANE PREMISE** carried by
  `interactive_override_routes_to_interactive_even_with_agent_marker` is RETIRED,
  and its doc comment says so rather than the assertions being forced. The premise
  was that an agent-marked start without `--interactive` auto-routed to a route no
  persistent fleet seat could ride, so every commissioned seat HAD to pass the
  flag. It no longer has to. The test now pins what survives: the override remains
  an honest escape hatch, so the prime recipe and every script that spells
  `--interactive` keep landing on the create lane — through the override rather
  than through the detect. Nothing in the fleet's recipes needs to change.
- `tests/acp_lane_is_not_the_claude_pane.rs`. Its CONTROL — the probe whose job is
  to prove the pane lane's refusal is live in the build under test, so the two ACP
  assertions cannot pass vacuously — was a BARE pane start. Re-based on
  `--headless`, which is what summons that sentence now. The file states honestly
  that the no-prompt test's mutation sensitivity WEAKENED (a binary with the old
  `lane.harness == ClaudeCode` predicate restored would walk the claude ACP start
  onto the pane path without printing the refusal); the byte-identical PARITY test
  carries the mutation weight now, and REDs under that same mutation.
- `src/conformance/harness.rs`. The `Outcome::blocked` arm keyed on "does not spawn
  one-off" is KEPT (defensive) with its comment narrowed: it fires only for an
  explicit `--headless` now, since the harness's own piped caller takes the create.

**Prose reconciled** (the refusal was stated as a RULE in five places):
`driver.rs`'s `StartRoute` / `start_route` / `attaches_after_start` docs,
`lifecycle.rs`'s route block and `claude_pane` binding comment, `help.rs`'s
`--interactive` paragraph and `cli.rs`'s flag help (both stopped telling
agent-marked callers they must pass it; both keep the codex/pi half, where the flag
still names a different TOPOLOGY), and `scripts/fresh-install-smoke.py`'s module
docstring + agent-lane comment (the script still passes `--interactive` for
claude-code — redundant on that lane now, kept because it is what shipped recipes
spell).

**Known-stale, out of scope, named here so it is not lost.** Three documents
outside `dispatch/` still teach the retired premise as a hard requirement:
`frame/src/engine.rs` (the comment at the `--interactive` injection),
`qc/corpus/skills/prime/SKILL.md` and its built `qc-plugin/skills/prime/SKILL.md`
("`--interactive` is load-bearing, not a preference: an agent-marked context
starting without it is refused"). The recipes they describe still WORK unchanged —
`--interactive` is merely redundant on this lane now — so nothing is broken by
leaving them; they are simply overstating. Reconciling them belongs to a qc-corpus
pass, not to this one.

**Revert.** Restore the driver-only signature (`start_route(driver, has_prompt)`)
and its two agent arms. There is no env lever or flag guarding the flip — a
context-derived default with an explicit escape hatch is the doctrine
(S-B-COMMAND-SURFACE-RULINGS), and `--headless` IS the escape hatch.

## Addendum (2026-08-26, qd-carrier-labels): `qd ls` and `qd info` stop naming zmx

The two human-render surfaces that still printed the retired backend's name now
name the PANE instead:

| surface | was | is |
|---|---|---|
| `qd ls` wide table (`qd live`'s 12-column render) | `zmx` column header | `Pane` |
| `qd info` text | `zmx:         <name> (attached)` | `Pane:        <name> (attached)` |
| `qd info` text | `zmx dir:     <dir>` | `Pane dir:    <dir>` |

**Why the label could not keep the name.** Both lines report the state of a MUX
PANE, and both print under the embedded qrmux default as well as the `QD_MUX=zmx`
escape hatch. Labelling them for one of the two backends was wrong even before
FTUE punch R1 retired that backend from the help surface and the `resume` flags;
R1 simply left these two behind. `Pane` is the term the rest of the surface
already uses — `qd ls`'s Hosting column renders `mux-pane`, and every pane lane id
is `*/mux-pane`.

**What did NOT change.** The VALUES are untouched: the `qd ls` cells are still
`attached` / `detached` / `-` with the same colorZmx colors (the helper is renamed
`color_pane`, the TS reference kept in its doc), and `qd info` still renders the
pane's own name, its attach state, `-` for a confirmed absence, and
`unknown (mux list unavailable)` for a refused read. Label padding stays at the
13-column gutter every other row uses, so nothing reflows. The `zmxName` /
`zmxClients` JSON keys in `ls --json` and `info --json` are DELIBERATELY untouched:
that is a machine-facing contract with consumers outside this repo, and renaming it
is a breaking change that needs its own decision.

**Goldens re-minted, and it is a TS-parity divergence.** `info-alpha.txt` was
frozen hand-verified against TS `status.ts:629-634`, which spelled both labels for
the backend. TWO goldens carry the info text and both were re-minted with
`QD_REGEN_GOLDEN=1`: `info-alpha.txt` (the two label lines) and `info-codex.json`,
which pins the codex row's human info text inside a JSON wrapper and carries the
`Pane: -` row — a codex session has no pane, and `-` is the same confirmed-absence
it always rendered. Each diff was verified to be EXACTLY the label lines — no
value, order, padding or preview change. The
`parity.rs` module header records the divergence at the golden itself, so a reader
re-freezing it later does not "fix" the labels back. The wide `qd ls` table has no
golden; its header is pinned by `wide_table_shows_stable_id_not_code`, which asserts
on the `Id` column and is unaffected.

**Still naming zmx, deliberately.** `qd info --json` / `qd ls --json` keys (above);
`shell_init.rs`'s emitted `ZMX_DIR` export; `preflight.rs`'s "Failed to launch zmx
— is it installed and on PATH?" (genuinely backend-specific and correct);
`send:pty`'s refusal strings, `create.rs`'s attachability error, and
`lifecycle.rs`'s `Session "X" is not in zmx.` — that last family is also pinned in
prose by `GATE-C1.md` and belongs to a send/attach pass, not this one.

**Revert.** Restore the four string literals (`"zmx"` header, `zmx:`/`zmx dir:`
labels) and re-freeze the golden. Nothing else is load-bearing.

## Cross-references

- LESSONS **L5** (boot-readiness EVENT) + **L9a** (HOME load-bearing for the jail) —
  the jailed-HOME credential-isolation rationale in section (a) is L9a's invariant;
  the EVENT-contract in section (c) is L5's. Both are carried executable + written.
- ADR 0004 (comparator classes + ADD-9a reclass addenda + B3 findings).
- ADR 0005-dialog-free-boot (the engine-side blind-Enter deletion).
- The §S substrate ruling and its four riders (R1 stub-pinning, R2 stub-seam
  negative controls, R3 this ADR, R4 recorded-exclusions stay non-green), as
  ratified by the project orchestrator on 2026-06-05 and summarized in the
  Context section above.
