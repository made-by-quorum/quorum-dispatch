# pi interactive lane — what was measured, and against what

The `--interactive` pi lane (`qd start <name> --provider pi --interactive`) rests
on four facts about pi's CLI and its on-disk store. Two of them contradicted what
this repo previously recorded, so this file states each one, where it was read,
and how it was checked — the `codex-repin-log.md` posture.

**Binary under test:** `@earendil-works/pi-coding-agent@0.80.2` — the version
[`provider::pi::pin`] pins. Source references are to its published `dist/`.

**How the live checks were driven.** pi needs a model endpoint, not credentials,
so the runs used a local OpenAI-compatible mock registered through pi's own
`models.json` (`api: "openai-completions"`), with `PI_CODING_AGENT_DIR` pointed at
a scratch agent dir. That exercises the real binary, the real TUI, the real
session manager and the real on-disk writes; only the model is substituted.

---

## 1. `--session-id` lets the LAUNCHER name the session

> `--session-id <id>    Use exact project session ID, creating it if missing`

**Source** (`dist/main.js`, `createSessionManager`): with `--session-id`, pi calls
`findLocalSessionByExactId(id, cwd, sessionDir)`; a hit is `SessionManager.open`ed,
a miss becomes `SessionManager.create(cwd, sessionDir, { id })`. Valid ids are
`/^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$/` (`assertValidSessionId`), and the
flag refuses to combine with `--session` / `--continue` / `--resume` /
`--no-session` (`validateSessionIdFlags`).

**Live:** launching with a chosen UUID produced
`<store>/<ts>_<that-exact-uuid>.jsonl` whose header carries that id.

**Why it matters.** This is the whole reason the pi lane is simpler than codex's.
codex discloses no thread id until a human types, so `provider::codex::tui` has to
DISCOVER identity afterwards, with a unique-or-nothing attribution matrix to avoid
adopting a stranger's conversation. pi lets us DICTATE it, so a pi row is
identified from its first instant: no unidentified window, no gather-step backfill,
and no attribution hazard — except the one below.

**The one hazard that survives.** `--session-id` OPENS an existing session rather
than failing, so a colliding id would silently adopt someone else's conversation.
Closed twice: the id is a v4 UUID (collision-free by construction), and the create
path additionally refuses when `tui::session_id_is_taken` sees that id already in
this project dir.

---

## 2. The persist law — NOT "append-on-exit"

**Previously recorded** (`qrmux::attended::driver`, from the M5 observation): pi's
session transcript is *append-on-exit*, so a live landing is unobservable. That
claim is why pi stayed gated when codex un-gated PTY delivery.

**It is wrong.** `dist/core/session-manager.js` `_persist(entry)` runs on EVERY
appended entry (`_appendEntry` → `_persist`) and branches only on whether the
buffer already holds an assistant message:

```
hasAssistant = fileEntries.some(e => e.type === "message" && e.message.role === "assistant")
  !hasAssistant && !flushed  → buffer in memory; NO FILE ON DISK
  !hasAssistant &&  flushed  → appendFileSync(entry)
   hasAssistant && !flushed  → openSync(file,"wx"); write ALL buffered entries; flushed = true
   hasAssistant &&  flushed  → appendFileSync(entry)
```

So it is append-per-entry, **deferred only until the first assistant reply**. A
session reopened from an existing file (`setSessionFile` → `flushed = true`) appends
from its very first entry.

**Consequence.** A live user message IS observable on disk, which is what lets pi
carry `AcceptanceSignal::Landing` — the same landing-as-acceptance proof codex uses.
pi's user records are `{"type":"message","message":{"role":"user",…}}`, the shape
`TranscriptLandingProbe` was already broadened to read, so pi needed no probe of
its own.

**The residual, unchanged by any of this:** a FRESH session that has not yet had an
assistant reply has no file at all, so a landing cannot be confirmed in that window.

---

## 3. TWO on-disk layouts, and qd was reading only one

pi picks its layout from **who chose the session dir**:

| session dir chosen by | layout |
|---|---|
| pi itself (default `~/.pi/agent/sessions`) | `<root>/--<enc-cwd>--/<ts>_<id>.jsonl` |
| the caller (`--session-dir` **or** `PI_CODING_AGENT_SESSION_DIR`) | `<root>/<ts>_<id>.jsonl` — **FLAT** |

**Source:** `getDefaultSessionDirPath(cwd)` encodes the resolved cwd into a bucket
under `<agentDir>/sessions/`; but `main.js` passes an explicitly-given dir straight
through as `sessionDir`, and `SessionManager` joins the filename onto it with no
bucket (`usesDefaultSessionDir()` exists to tell the two apart).

**Live:** both were reproduced — the env-var run wrote
`<root>/2026-08-07T17-50-22-160Z_envtestid.jsonl`, the default run wrote
`<agentDir>/sessions/--private-tmp-…-work--/…_defaultlayoutid.jsonl`.

**The defect this exposed (pre-existing, and it affected the daemon lane too).**
`session::find_session_file` searched only the bucket, and `scan_transcripts` walked
only `<root>/*/`. qd's own root resolution PREFERS `PI_CODING_AGENT_SESSION_DIR`
when set — so against the layout qd's own configuration produces, the search read a
directory that never existed and returned `None` forever. And `None` is
indistinguishable from pi's legitimate lazy-write window, so the failure was
invisible: pi sessions simply never grew a transcript path, turn count or preview.
Both readers now cover both layouts; an id match is unambiguous either way.

---

## 4. The cwd on disk is the RESOLVED one

pi records `resolvePath(cwd)` in the session header and encodes that same resolved
spelling into the bucket name. The create path therefore canonicalizes the cwd once
and stores THAT in the registry row.

codex hit the string-compare form of this in its own end-to-end validation
(`/tmp` vs `/private/tmp`); for pi it is structural rather than cosmetic, because
the resolved path becomes a **directory name**. The canonicalizer is now shared
(`provider::canonical_dir`) so the next provider inherits the fix instead of
rediscovering the defect.

---

## Live end-to-end, through the real `qd`

A real pi 0.80.2 TUI in a real qd mux pane, driven by real `qd send`:

```
users : ['Reply with exactly: PI-FIRST-OK', 'Reply with exactly: PI-SECOND-OK', 'Reply with exactly: PI-AFTER-REVIVE']
agents: ['PI-FIRST-OK', 'PI-SECOND-OK', 'PI-AFTER-REVIVE']
```

- The session file was created under the id **qd dictated**, with the resolved cwd.
- Each message landed **exactly once** — the double-submit guarantee holding in
  production, not just in a fixture — and each was answered.
- The **second** send emitted a `message-seen` terminal: acceptance confirmed from
  the transcript, which is the un-gate working end to end.
- The **first** send emitted no terminal, exactly as the residual above predicts:
  no transcript existed at send time, so the landing was unconfirmable even though
  the bytes landed. Observed, not assumed.
- `qd stop` then `qd resume` reopened the SAME conversation — still ONE file, the
  third exchange appended to it.

### A trap worth recording for the next person

The first live attempts appeared to show delivery failing with `verify-blocked`.
The cause was the test harness, not the product: `Harness::from_command` classifies
by the launched binary's **basename**, so a stand-in wrapper named `pi-wrapper` or
`pi-diag` resolves to `Harness::Default` and gets claude's composer facts (which
look for `❯` and never match pi's screen). Renaming the wrapper to exactly `pi`
selected `PiFacts` and delivery worked immediately. Any future live pi drive must
name its stand-in `pi`.

---

## 5. `--session-id` is version-gated, and the failure without it is invisible

`--session-id` does not exist in older pi. **0.74.2** answers
`Error: Unknown option: --session-id` and exits immediately.

Without a guard that is a genuinely awful failure, because it happens INSIDE a
freshly-spawned mux pane: pi prints its error to a pane nobody is attached to and
dies, the pane dies with it, and `qd start` reports whatever its attachability
verify happened to observe — a message about panes and registries that says
nothing about the cause. Found exactly that way on a dev box where an
asdf-managed **pi 0.74.2 shadowed a correctly-installed 0.80.2 on PATH**.

So the interactive create path now PREFLIGHTS the binary
([`tui::supports_session_id`]) before it claims a name or spawns anything, and
refuses with the binary, its reported version, the pin, and the `QD_PI_BIN` fix.

**A capability probe, not a version compare.** What the lane needs is the flag, not
a number: pinning to `PINNED_VERSION` would refuse a future 0.81 that supports
`--session-id` perfectly well, and would still pass a build that reported the right
version without the flag. `--help` names its own options, so we ask it directly.
(The exact-version pin still matters — it guards the RPC wire the DAEMON lane
rides, a different contract.)

**The probe is bounded, and that is load-bearing.** The first version used a plain
`Command::output()`. The probe runs whatever `QD_PI_BIN`/PATH resolves to, and a
binary that does not recognise `--help` is free to sit there — which the test
stand-in did (`exec sleep 600`), hanging `qd start` for ten minutes. An unbounded
probe is a strictly worse failure than the dead pane it was added to prevent: at
least the dead pane came back. It now polls to a 10s deadline and kills the exact
child (never a group signal — the `floor::run_floor_turn` binding); a timeout is an
honest "could not tell", not a verdict in either direction.

---

## Not available for pi: a human viewer on an agent-hosted session

codex's second interactive use case — `qd attach` on a LIVE daemon-hosted session
opening a human TUI onto it — has **no pi analog**, and this is structural rather
than unimplemented.

That mechanism works because the codex TUI is itself an app-server client:
`codex --remote <ws-url>` binds it to the server qd already spawned, so the human's
TUI and the agent's RPC client become two clients of one server on one thread. pi
has no such affordance. It speaks stdio, and qd's resident adapter OWNS that stdio
— there is no second-client door, and no flag that opens one. Opening
`pi --session <id>` alongside would be a second process writing the same session
file, which corrupts the conversation rather than sharing it.

So a daemon-hosted pi row keeps the honest daemon redirect. A human who wants a
terminal on pi starts the session `--interactive` in the first place.
