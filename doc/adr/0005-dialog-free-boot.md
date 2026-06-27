# ADR 0005: Dialog-free boot — blind-Enter deletion is a sanctioned non-parity

**Status:** Accepted (A2; orchestrator-ratified gate class — brief deliverable 5)
**Date:** 2026-06-04

## Context

TS `qd new` cannot gate its popup-dismiss Enter on the PID registry file, because
the dev-channels consent popup appears BEFORE Claude Code writes that file. Its
workaround (`lifecycle.ts:184-248`) is a blind-Enter loop: from 2s after spawn,
send `\r` every ~2s into the session PTY until the PID file appears, rationalized
as "sending Enter to an idle CC with an empty composer is harmless."

It is not harmless. The Enters are UNTARGETED — they answer whatever is on
screen. Empirically (A2 probe boot 1, jailed): in a fresh HOME the first thing on
screen is the FOLDER TRUST dialog ("Is this a project you created or one you
trust?", default `1. Yes, I trust this folder`) — TS's loop silently accepts a
security consent it never read. Any future dialog claude adds (OAuth re-auth,
new consents, migration prompts) gets the same blind `\r`. This is the L5 lesson:
readiness is an EVENT, and keystrokes aimed at no specific content are a defect
class, not a boot strategy.

## Decision

The Rust boot path (A2 hardening #4) DELETES the blind-Enter loop. It is a
sanctioned, deliberate NON-PARITY with TS:

1. **Readiness = event contract (L5):** PID-file appearance (name-matched row in
   `<home>/.claude/sessions/`) + the went-busy/idle status transition. ZERO
   keystrokes on the stock path — gate-asserted via the Exec audit log (and
   mutation-tested: an injected Enter fails the gate).
2. **Dialogs are answered only by the delegated-consent answerer** (boot.rs, M3):
   opt-in per NAMED dialog, CONTENT-MATCHED against the ANSI-stripped zmx-screen
   tail, `\r` sent ONCE, re-read verify, ≤1 retry, then FAIL LOUD with
   `qd attach <name>` guidance. An unmatched dialog is NEVER answered.
3. **Known dialogs are pre-accepted as state, not keystrokes**, where claude
   supports it: `.claude.json` `hasCompletedOnboarding`,
   `bypassPermissionsModeAccepted`, per-project `hasTrustDialogAccepted`
   (probe-verified 2026-06-04: with these seeded, a stock boot reaches
   status=idle in ~1s with zero keystrokes).

## Consequences

- 0b comparator: boot-trace rows are judged by the boot-readiness EVENT class
  (ADR 0004), not keystroke-sequence parity; the corpus records TS's Enters as
  the divergence baseline.
- A boot gated by a dialog qd does not know BLOCKS LOUDLY instead of being
  silently clicked through. This is a deliberate behavior change: worse
  "it boots anyway" ergonomics, strictly better consent integrity.
- A4 (submit/wait) inherits the event-keyed readiness; the full exit contract
  lands there.

## Addendum (eng-lane item 2, 2026-06-12) — folder-trust is now a VETTED named entry

**Status:** Accepted (Pete-ruled 2026-06-12, in qd-supervisor's session; board
STATE 132). REVISES the §2 disposition of the folder-trust dialog ONLY.

The original ADR used the folder-trust dialog as the cautionary example of TS's
blind-accept defect, and §2 left it to "BLOCK LOUDLY." Two facts changed the
calculus:

1. **It now hard-blocks every spawn.** claude 2.1.x writes its session
   registration row ONLY after the folder-trust dialog is answered, so on any
   fresh/untrusted dir the engine's boot waiter polls a row that never appears →
   the dialog is no longer a rare jailed-probe curiosity but a universal spawn
   blocker (observed live, STATE 132: pane sat on the dialog, waiter timed out).
2. **Answering it is not BLIND.** The defect §Context named was an UNTARGETED
   `\r` that accepts "whatever is on screen." The answerer does the opposite:
   two-factor CONTENT recognition (the shared `Enter to confirm` marker AND the
   distinctive `Quick safety check` title), assert-before-send, a single `\r`
   selecting the default `1. Yes, I trust this folder`, re-read verify, ≤1 retry,
   then fail loud. The fleet operator controls the dirs the engine spawns into,
   so default-Yes is the fleet's own intent, not a consent read on a stranger's
   behalf.

**What does NOT change:** the "never answer an UNLISTED dialog" guarantee (§2)
stands verbatim. folder-trust is added to the `named_dialogs()` registry as ONE
vetted entry alongside dev-channels — a marker-bearing dialog with no named match
is still Unmatched and still fails loud with zero keystrokes. The wrong-victim
class ("answer a dialog that isn't the trust dialog") is closed structurally: a
send requires BOTH the marker AND the trust title in the same boot-window tail,
and the answerer only runs before the PID file appears (no real session work has
rendered yet). Carriers: `boot.rs` `named_dialogs()` + the real 2.1.175 capture
fixture (`tests/fixtures/boot/trust-dialog-2.1.175.txt`) + the differential and
wrong-victim tests (`folder_trust_real_capture_differential_old_vs_new_registry`,
`detect_dialog_trust_title_without_marker_is_nodialog`,
`detect_dialog_unmatched_unknown_dialog_with_marker`,
`folder_trust_dialog_persists_two_sends_then_fail`).

Note §3 (pre-accept known dialogs as STATE) remains the preferred path where
claude supports it — a seeded per-project `hasTrustDialogAccepted` avoids the
dialog entirely. The answerer is the belt for the dirs/sessions that reach boot
without that state seeded.
