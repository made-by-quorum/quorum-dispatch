# ADR 0015: Declarative `connect` verb; `attach` demoted, `resume` is the agent verb

**Status:** Accepted
**Date:** 2026-06-07

## Context

The CLI exposed two overlapping imperative session-entry verbs: `attach` (take over a
live session's terminal via the mux) and `resume` (relaunch a cold session). A human had
to know which one applied to a session's current state, and the failure wording leaked
mux internals — e.g. `sb attach <cold-session>` printed `Session "<name>" is not in zmx.`,
which both exposes the `zmx` mux detail and states the wrong reason (the session is cold,
not "missing from the mux"). Separately, codex sessions are daemon-hosted with no terminal
pane, yet `attach` refused them with a misleading `unknown provider "codex"` (codex IS
supported, just not attachable).

## Decision

Collapse session-entry into one declarative human verb and clarify the audiences:

- **`connect <session>`** — the HUMAN verb ("get me into this session"). It dispatches on
  the row's provider *hosting* first, then liveness:
  - live claude (`Hosting::MuxPane`) → attach the pane;
  - cold claude → **auto-revive then attach** (revive-to-drivable + a TTY tail);
  - codex (`Hosting::Daemon`) → a loud redirect (`sb send:relay` to drive, `sb resume` to
    revive) — there is no pane to attach, so `connect` never attaches a daemon;
  - opencode → parked; unknown provider → loud refusal.
- **`resume <session>`** — kept first-class but documented as the AGENT verb:
  revive-to-DRIVABLE with no interactive attach tail (non-TTY safe).
- **`attach`** — **demoted** off the user-facing surface (hidden in the top-level help,
  `--help` points at `connect`) but still registered + dispatchable for scripts. It shares
  the same resolution/dispatch mechanic (`lifecycle::attach_resolved`) as `connect`.
- The cold/unreachable human error names the human verb: `session '<name>' is cold (not
  running) — revive and attach with: sb connect <name>`. It never leaks `zmx`/mux internals.
- A shared live-id-collision preflight (`common::refuse_id_collision`) runs in the shared
  mechanic, so `connect` and the demoted `attach` both refuse a genuine ≥2-alive id
  collision loudly (a single live row still attaches normally).

The shared mechanic returns the cold case to the caller (`AttachOutcome::Cold`) rather than
branching on the verb name, so `connect` revives while `attach` cold-errors. `connect`'s
cold revive reuses a `revive_claude` seam factored from `resume`'s detached (`--no-attach`)
path; `resume`'s own zmx-attach default path is unchanged.

## Consequences

- Humans use one verb (`connect`) regardless of session state; agents/non-TTY callers use
  `resume`. Aligns with the declarative-primitives direction (collapse imperative
  mechanics into intents).
- No `--json` byte-shapes or exit-code contracts changed; `attach`'s behavior is unchanged
  where it still dispatches (only its surface visibility + cold/codex wording changed).
- The codex daemon case stays pointed at `send:relay`/`resume` (a paneless daemon has no
  human-attach; making `connect` revive a cold codex daemon without attaching is a parked
  future refinement).
- The full cold→revive+attach path for `connect` is only exercised by a live/slow test
  (`#[ignore]`d — the boot waiter has no injectable timeout); the dispatch, redirect,
  cold-error, demotion, and collision paths are covered by jailed binary-driven tests.
- Follow-up (verb-message pass, not this change): some `connect` revive-failure infra
  errors are still generic or name `zmx` in genuine missing-binary/launch diagnostics
  (a rare path, distinct from the misleading status leak this ADR removed); and `resume`'s
  "already alive — use sb attach" message is off-model for agents.
