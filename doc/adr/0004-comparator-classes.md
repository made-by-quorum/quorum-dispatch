# ADR 0004: Comparator classes

**Status:** Accepted (orchestrator-reviewed 2026-06-04)
**Date:** 2026-06-04

## Context

Not every golden corpus entry can or should be compared byte-for-byte. Some
outputs (`--json` payloads, help text, exit codes) ARE deterministic after
normalization and demand byte-exactness. Others (reattach replay, passthrough
under load, scrollback across detach) are about an OUTCOME — a property that must
hold — where byte-equality is both too strict (reflow/timing) and beside the
point. We need a taxonomy so every corpus row declares HOW it is judged, and the
semantic rows enumerate their invariants explicitly.

Implemented in `test/golden/lib/compare.sh`; each class is a function returning a
DISTINCT failure reason. The asserter (`verify.sh`) keeps DEADLINE failures
(liveness/budget) distinct from these DIFF failures.

## Decision

### Class: byte-exact

Normalized output must match the normalized expectation byte-for-byte. Used for
deterministic surfaces: `ls/info --json`, help text, fixed command output, and
exit codes. Comparator: `compare_byte_exact` (+ `assert_exit_code` for the
`.exit` sidecar).

### Class: semantic / outcome-class

Assert explicit invariants, not bytes. The enumerated invariants:

| Invariant | Meaning | Comparator | Source |
|-----------|---------|------------|--------|
| **backlog-completeness** | Every line produced while detached is present on reattach, in non-decreasing index order. dtach FAILS this; zmx's server-VT model passes it. | `assert_backlog_complete <cap> <marker> <count>` | EMPIRICAL-RESULTS.md |
| **no-altscreen** | Passthrough emits ZERO `?1049h/?47h/?1047h`. | `assert_no_altscreen <cap>` | R1, spike |
| **scroll-intact** | Scrollback preserved across detach/reattach (every pre-detach printable line survives). | `assert_scroll_intact <pre> <post>` | EMPIRICAL-RESULTS.md |
| **boot-readiness EVENT** | Readiness = PID-file APPEARANCE + a went-busy transition. NOT a blind-Enter keystroke loop. | `assert_boot_ready_event <pidfile> <went_busy>` | spec §3.3, deliverable 5 |

### Per-corpus-entry assignment

Each row in `test/golden/coverage-matrix.md` declares its comparator class. The
mapping is part of the matrix scaffold (Part 1); the tick (real recording) is
Part 2.

### relay-health = CONTRACT surface, not transport (ADD-5)

Per ADD-5, cc-relay becomes a standalone external transport DRIVER; the engine
keeps only the messaging CONTRACT. The relay-health corpus rows therefore record
and assert ONLY the contract surface — (a) registration sidecar shape, (b)
`/message` endpoint, (c) `/health` endpoint, (d) the `ls` join — and NOTHING
transport-internal, so the fixtures survive a future transport-driver swap. The
comparator class is **byte-exact on the normalized contract shape** (port /
socket-prefix / pid / timestamps tokenized). The row is PROVISIONAL (relay
fix-wave + ADD-2 `--wait`). Scenario: `scenarios/relay_health.sh`.

### Sanctioned divergence: boot-readiness

TS qd determines readiness with a blind-Enter keystroke loop (it sends Enter at
intervals to dismiss the dev-channels popup, because the PID file does not yet
exist — see `~/work/switchboard/src/commands/lifecycle.ts:170-203`). The Rust
oracle deliberately diverges: readiness is the **EVENT contract** (PID-file
appears AND the session goes busy), which is the actual signal of readiness, not
a proxy. This is encoded in `assert_boot_ready_event`. Per spec §2, the *formal*
sanctioned-divergence ADR (if TS is still unfixed at record time) is a Part-2
decision; Part 1 encodes the EVENT contract in the comparator and notes the
divergence here.

## Consequences

- Every corpus entry is judged the right way: deterministic surfaces stay strict,
  liveness/replay surfaces assert the property that actually matters.
- Semantic invariants are NAMED and individually testable, so the mutation test
  can prove each one bites (dropped-backlog-line → backlog-completeness; injected
  alt-screen → no-altscreen; reordered replay → backlog ordering).
- The boot-readiness divergence is explicit and carried in code, not implicit.
- A DEADLINE (budget) failure is never conflated with a DIFF failure — a hang that
  would *eventually* match is still caught (verify.sh exit 2 vs 1).

---

## ADD-9a reclass addenda (Part-2 Step 1, 2026-06-05)

Per ADD-9a (parity philosophy: byte-exact is a TOOL for cheap testing on STABLE
contract surfaces — `--json`, exit codes, fixed command strings — not a religion;
where byte-parity costs real effort on a cosmetic/imperfect/fabricated surface,
DEFAULT to semantic/outcome-class + a NAMED divergence). The Part-2 comparator
re-evaluation walked all 14 required matrix rows. Rows already semantic
(boot-readiness-event, semantic-submit-discipline, semantic-backlog[-scroll],
no-altscreen) are unchanged; the byte-exact contract surfaces that ARE stable
(`ls/info --json`, `buildClaudeCmd`, exit codes, relay-health contract shape per
ADD-5) stay byte-exact. The following are the ONLY byte→semantic reclasses; each
is a dated NAMED divergence with its replacing invariant. (Backstop:
count(reclasses) == count(named-divergence lines below) == 2.)

- **DIV-9a-1 (2026-06-05): zmx-dir resolution, ZMX_DIR tier — byte-exact →
  semantic (resolution-outcome).** WHY: qd exposes no "print my resolved zmx dir"
  surface, so a byte-exact row could only compare a FABRICATED scenario line, not a
  real qd output — byte-parity on a fabricated string tests nothing real. REPLACING
  INVARIANT (resolution-outcome): a session created with an explicit `ZMX_DIR=X`
  has its zmx socket created under `X` and is reachable/killable there (resolveZmxDir
  returns the explicit dir outright — pinned `src/utils.ts:68-82`, ZMX_DIR branch).
  The outcome (socket lands in the explicit dir; `qd`/`zmx` find it there) is the
  load-bearing contract, asserted semantically against the jail's `ZMX_DIR`.

- **DIV-9a-2 (2026-06-05): zmx-dir resolution, TMPDIR fallback + collapse —
  byte-exact → semantic (resolution-outcome).** WHY: same — no print surface; the
  contract is the COLLAPSE OUTCOME (a compounded `TMPDIR=/tmp/x/x/.../` pegs to ONE
  canonical `<collapsed>/zmx-<uid>`), not a fabricated byte line. REPLACING
  INVARIANT (resolution-outcome): `resolveZmxDir` with no `ZMX_DIR`/`XDG_RUNTIME_DIR`
  returns `join(collapseRepeatedSegments(TMPDIR), "zmx-<uid>")` (pinned
  `src/utils.ts:79-81`), so two re-nested TMPDIRs that collapse to the same root
  resolve to the SAME socket dir — a session created under the compounded TMPDIR is
  reachable via the collapsed canonical dir. Asserted semantically (the collapsed
  canonical dir is the resolution target), not byte-for-byte on a fabricated line.

NOTE (NOT a reclass — matrix annotation): the **Bug-D `XDG_RUNTIME_DIR` tier** row
inherits the same resolution-outcome reframing, but it is **Linux-only** (Step 3 on
Lima `sbtest`, aarch64) — its recording + final class assignment land in Step 3,
not Step 2, so it is NOT counted among the two macOS reclasses above.

NOTE (NOT a reclass — matrix annotation): **`buildClaudeCmd` CLAUDE_FLAGS source.**
The matrix row reads "CLAUDE_FLAGS from config", but at the pin `CLAUDE_FLAGS` is a
hard-coded CONSTANT (`src/utils.ts:226-227`: `["--dangerously-skip-permissions",
"--dangerously-load-development-channels", "server:relay"]`), NOT config-sourced
(ADR 0006 tracks the Rust-side config-as-source decision). The row stays byte-exact
on the fixed `command '<bin>' '<flags...>'` string + load-bearing flag ORDER; the
"from config" wording describes the Rust target, not the pinned-TS source. No class
change.

NOTE (W1 P8 — expectation-only/Rust-target row, NOT a reclass): **`buildClaudeCmd`
with a NON-DEFAULT config flag.** The default-flags row above is byte-exact on the
CONSTANT triple — which a BROKEN config-loader that simply hard-codes the same three
flags ALSO satisfies (panel finding P8, opus M5 + gpt): at the pin the byte-exact
default row cannot distinguish a real config-loader from a constant-echo. The new
`scenarios/build_claude_cmd_config.sh` row closes that gap: it supplies a NON-DEFAULT
flag set through the ADR-0006 config seam (`SB_CLAUDE_FLAGS`, tier 1) and asserts the
launch argv reflects the OVERRIDE (byte-exact), not the default triple — proving the
loader READS config. **This row is EXPECTATION-ONLY / Rust-target (sanctioned by
ADR-0011 §(a)):** at the pin TS has no config seam for `CLAUDE_FLAGS` (the triple is a
compile-time constant), so the scenario would FAIL against pinned TS BY CONSTRUCTION,
and recording the TS side would launder the constant-echo bug into gold. It therefore
carries NO fixture / MATCH-PROOF and stays UNticked in the coverage matrix — NOT wired
into the green verify suite — until the Rust engine exists to DRIVE it (its `scn_assert`
encodes the expected argv inline). The comparator class is byte-exact (a stable flag
sequence); no class change to the existing row.

## B3 gate findings — attach re-resize + multibyte input loss (Part-2 Step 2, 2026-06-05)

Two B3 gate findings (orchestrator-forwarded) shaped the Step-2 scenario design;
both are recorded here as class/scenario constraints (no new reclass):

- **B3 gate finding (attach re-resize):** the merged mux re-resizes the PTY on
  every fresh single-client attach, emitting a resize event that zmx may not emit
  identically — so a capture that SPANS a live attach is correctly semantic, never
  byte-exact. The `attach-detach-reattach` row is `semantic-backlog-scroll` (not
  byte-exact), AND it observes the retained backlog via `zmx history` (the
  server-side VT serialization that reattach replays from), NOT a live `zmx attach`
  capture — so no attach-time resize event enters the recorded expectation. The
  semantic assertions (backlog-completeness on the SBLINE rows + no-altscreen on the
  full VT) are resize-tolerant by construction. No attach-spanning row is byte-exact.

- **B3 gate finding (multibyte input loss):** CJK/multibyte PTY INPUT is lossy under
  paste-burst. Two binding constraints, both honored: (a) every marker/sentinel a
  scenario greps keys on APPLICATION OUTPUT (the JSONL records the stub writes, the
  SBLINE rows the stub emits, the stub's STUB-REPLY), NEVER on input echo; (b) NO
  multibyte sentinels in paste-burst rows — `send-pty-paste-burst` uses ASCII-only
  markers (`PASTE-BURST word0…`, `first-turn-holds-busy`) and asserts on the JSONL
  user records + the --wait reply (application output), so input-echo loss cannot
  false-fail or false-pass the row.

## Alt-screen replay reversal — scope split of the no-altscreen invariant (2026-06-10, PR #51)

The **no-altscreen** invariant above was written when clients were never supposed
to see DEC 1049: the embedded mux absorbed the inner app's alternate-screen state
server-side (performer consumes 1049, swaps grids) and rendered everything into
the client's MAIN screen. PR #51 deliberately REVERSES that for the client-attach
surface: Claude Code 2.1.x runs its whole TUI in the alt screen with mouse
tracking on, and a client kept on the main screen has dead scrolling on phone
terminals (Termius swipes become mouse events; the local buffer is empty because
alt-screen output never reaches mux scrollback). zmx replays `?1049h` on
reattach, which is why the pre-flip engine never exposed the problem. Full
evidence chain: `doc/inbox/2026-06-10-qrmux-phone-scroll-regression.md`.

The invariant is not retired — it is SPLIT by surface:

- **History/backlog VT captures (`zmx history`, GetHistory, scrollback
  injection): no-altscreen UNCHANGED.** Retained backlog is main-screen content
  by construction; `?1049h/?47h/?1047h` in a history capture is still a leak.
  The corpus rows above (e.g. `attach-detach-reattach`, judged on the history
  serialization) keep `assert_no_altscreen` as written.

- **Live client attach/transition captures (embedded qrmux): no-altscreen-leak
  REPLACED by alt-screen REPLAY.** New invariant: a client's raw capture
  contains `?1049h` IFF the inner app is in the alt screen at attach time or
  transitions into it while attached, exactly once per attach; a main-screen
  session's capture stays byte-identical to the pre-replay behavior; legacy
  `?47h/?1047h` are STILL never emitted (the renderer replays only 1049).
  Comparator: `assert_altscreen_replay <cap> <expect_1049h> <expect_1049l>`
  (replaces `assert_no_altscreen_leak` in `crates/qrmux/tests/lib/assertions.rs`;
  c1_gate G-ALT asserts the 1049h/1049l ride-through, G-WINCH and fresh-reattach
  rows assert zero-on-main-screen).

Mutation-test note (Consequences bullet above): "injected alt-screen →
no-altscreen" still bites on the history surface; on the client surface the
biting mutation is now its dual (suppressed replay → assert_altscreen_replay
fails its exact-count).
