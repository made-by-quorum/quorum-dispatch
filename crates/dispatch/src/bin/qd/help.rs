//! Verbatim per-verb `--help` strings (H1: commander layout parity, spec §2).
//!
//! Each const is the EXACT stdout body of the corresponding TS dryrun corpus
//! capture (`test/golden/dryrun/a3-cli/NN-help-*.txt`) MINUS its single trailing
//! newline — clap's `override_help` re-adds exactly one `\n` when it renders
//! (verified), so the emitted bytes match the capture byte-for-byte. Commander
//! layout (`Usage: qd <verb>|<alias> [options] <args>`, `display help for
//! command`, `ls|list` alias style, no `Arguments:` section) is reproduced by
//! hand-writing the string rather than fighting clap's template (spec §2:
//! "where templates can't reproduce a byte, hand-write the help string
//! verbatim from the corpus"). config/survey help live in their own hand-parsed
//! modules (already byte-GREEN) and are not duplicated here.
//!
//! The TOP-LEVEL help is the exception, and deliberately so: [`render_top`] at
//! the bottom of this file GENERATES it by walking the clap tree (FTUE punch
//! R4). Per-verb strings are hand-written because they carry prose a builder
//! cannot reproduce; the top-level TABLE carries no prose at all — it is the
//! verb list — so hand-writing it bought nothing and cost drift.

// `setup` has no TS-era corpus capture — it is net-new (FTUE punch R15 + C2),
// the verb a human reaches straight after `brew install`. It is hand-written to
// the same commander layout as the rest of this file precisely because it is
// one of the few verbs `qd --help` shows: a verb on that path rendering clap's
// default layout is the one place the CLI would look unfinished.
pub const SETUP: &str = r####"Usage: qd setup [options]

Integrate your agent harnesses with quorum.

Run this when you first install quorum-dispatch, whenever you install a new
agent harness, and any time an existing integration needs repairing. It checks
everything qd needs and prints a verdict per check — that `qd` is on PATH, and
the `~/.quorum` layout, where quorum keeps its configuration. It then reports
which agent harnesses you have — Claude Code, Codex, Pi, OpenCode — with their
versions, and wires up the ones that are present.

By default it changes NOTHING: it reports what is wrong and, under each failing
check, the exact thing that would fix it. On a terminal it offers to apply them;
run non-interactively it just reports. Safe to re-run — every step is
idempotent, and a second run on a wired machine is a no-op.

Options:
  -h, --help  display help for command
"####;

// B5 item 2 (additive, orc-ruled C1+D): `--live` + the trailer note extend the
// TS-era corpus capture (the same sanctioned shape as info's `--json` line).
pub const LS: &str = r####"Usage: qd ls|list [options]

List all sessions, on every provider (use --json for scripting)

Options:
  -a, --all            Everything: all local sessions (uncapped, incl. killed)
                       PLUS every peer host's mirror, each stamped with its
                       staleness (now − witnessed_at). No fleet? = local only.
  --host <host>        One peer host's session mirror (remote/<host>/ls.json),
                       stamped with the mirror's staleness; conflicts with --all.
                       No mirror for that host ⇒ refused{no-fleet-state}.
  --live               Live sessions only (idle/busy/shell — excludes cold and
                       killed; includes UNNAMED live rows the default view
                       hides), uncapped; conflicts with --all
  --json               Output as JSON (best for scripting/piping)
  --table              Force the human table (override the JSON auto-default)
  --short              Names only, one per line
  --prefix <prefix>    Filter sessions by name prefix
  -n, --limit <count>  Max sessions to show (default: 20; --all and --live are
                       uncapped unless -n is given)
  -h, --help           display help for command

When the default view's cap (20) hides sessions, a trailer on stderr says how
many: "… N more (qd ls --all)" (the count is total dropped, not narrowed by
--prefix). Scripting tip: `qd ls --live --json` is the uncapped live-session
surface (--json never carries the trailer). Live Claude rows include an honest
bare/managed classification (`management` in JSON; `Mode` in the human table).
"####;

/// The AGENT / canonical view of `qd attach --help` — [`ATTACH_HUMAN`] is the
/// short human one.
///
/// REWRITTEN, not reformatted: the string this replaces was FALSE. It named
/// PROVIDERS ("a codex session is daemon-hosted — there is no TUI to connect
/// to"), and attach has not dispatched on a provider since the lane split.
/// `verbs/attach.rs:260-267` derives the row's `Lane` from its provider PLUS
/// its hosting and hands the whole question to `LaneOps::attach`, so one
/// harness answers up to three different ways depending on how it was started
/// — and codex in particular moved its create default to `Mode::AppServer`
/// (`quorum-qw/src/lane.rs:184-196`), the one daemon lane that DOES open a
/// terminal, a viewer on the session's app server. The old sentence outlived
/// the code it described by two lanes.
///
/// So the rows below are keyed on LANE ids and each carries its own prose,
/// because each lane's answer is genuinely a different answer. They are
/// LITERAL rather than derived from `Lane::ALL` — unlike [`provider_list`],
/// which can be generated because the accept-set is all it says. A generated
/// lane list could name the nine and say nothing about any of them, and what
/// they do is the entire content of this page.
pub const ATTACH: &str = r####"Usage: qd attach [options] <session>

Connect your terminal to a running agent session.

Attach dispatches on the session's LANE, not on its provider, so one harness
answers differently depending on how it was started:

  claude-code/mux-pane    the session's own TUI. Cold ⇒ revived, then attached
  pi/extension            the session's own pi TUI (pi's default lane). Cold ⇒
                          revived, then attached
  pi/mux-pane             the session's own pi TUI. Cold ⇒ revived, then
                          attached
  codex/mux-pane          the session's own codex TUI. Cold ⇒ revived, then
                          attached
  codex/app-server        codex's DEFAULT lane, and NOT the session's own
                          terminal: a viewer, i.e. a second client on the app
                          server, opened in a separate <session>.view pane. A
                          session that has not taken a turn yet is refused
                          (exit 1) — it has no rollout for a viewer to resume,
                          so send it a message first. Cold ⇒ the app server is
                          respawned, then the viewer opens
  codex/daemon            a viewer, but only while the row still carries a live
                          endpoint. Without one, refused (exit 1). Never revived
  pi/daemon               nothing to attach — refused (exit 1). Never revived
  claude-code/acp         nothing to attach, in ANY state: an ACP bridge is
  opencode/acp            headless by construction and has no terminal of its
                          own. Refused (exit 1), and never revived — drive it
                          with `qd send`, revive it with `qd resume`

A stopped session is refused (exit 1) whatever its lane: resume it first. So is
an id shared by two live sessions — kill the duplicate first.

Options:
  --no-attach   claude-code ONLY — exit 1 for any other provider. Revive a cold
                session to a persistent daemon and return 0 WITHOUT attaching a
                TTY (headless — e.g. a systemd ExecStart). A session that is
                already live is left alone and returns 0 without reviving
  --alt-screen  Fullscreen (alt-screen) rendering if this attach revives the
                session (default: inline, so phone/SSH attach can scroll)
  --inline      Force inline rendering (overrides `render-default = alt-screen`)
  -h, --help    display help for command

--alt-screen and --inline are consumed by the REVIVE, so they are inert on a
live attach, on every daemon lane, and on the codex viewer even when it opens a
new pane. They bite only on a cold revive of claude-code/mux-pane,
codex/mux-pane, pi/mux-pane or pi/extension.
"####;

/// The HUMAN view of `qd attach --help` — [`ATTACH`] is the agent/canonical one.
///
/// The same split, for the same reason, as [`start_human`]: by AUDIENCE, not by
/// truth. Nothing here contradicts [`ATTACH`]; it is a strict subset of it plus
/// a pointer at the rest, and the pointer is `qd attach --help | cat` because a
/// pipe is the signal that already resolves the driver to Agent.
///
/// What it subtracts is the lane TAXONOMY. A person at a prompt has one
/// question — "will this give me a terminal, and if not, what do I type
/// instead?" — so the three answers are grouped under the harness names they
/// would recognise, and the three non-default lanes are left to the full page,
/// named only by the flags that produce them. That grouping is a simplification
/// the page ADMITS to ("started it with --daemon or --interactive? that changes
/// the answer"), which is what keeps it short without making it the old lie
/// again.
///
/// A plain `const` where [`start_human`] is a `fn`, and deliberately: that one
/// must be computed because it interpolates [`provider_list`]. Nothing on this
/// page is derived — see [`ATTACH`] for why per-lane prose cannot be — so
/// there is nothing here for a `format!` to do.
pub const ATTACH_HUMAN: &str = r####"Usage: qd attach [options] <session>

Example: qd attach claude1

Connect your terminal to a running session.

What you get depends on how the session is hosted:
  claude-code, pi   its own TUI. A cold session is revived first
  codex             a viewer onto its app server, opened in a separate
                    <session>.view pane — not the session's own terminal. Send
                    it a message before attaching: a session that has not taken
                    a turn yet has no rollout for a viewer to resume
  opencode          nothing to attach to — its only lane is an ACP bridge,
                    headless by construction. Drive it with `qd send`, revive
                    it with `qd resume`

Started it with --acp, --daemon or --interactive? That changes the answer —
the full page lists every lane. A stopped session is refused whatever its
lane: resume it first.

Options:
  --alt-screen  Fullscreen rendering, but only if this attach revives the
                session (default: inline, so phone/SSH attach can scroll)
  --inline      Force inline rendering (overrides `render-default = alt-screen`)
  -h, --help    display help for command

Every option: `qd attach --help | cat` (the full agent-facing list).
"####;

// `connect` is a retired spelling, kept as a hidden backward-compat alias for attach.
pub const CONNECT: &str = r####"Usage: qd connect [options]

(renamed — use qd attach)

Options:
  -h, --help  display help for command
"####;

pub const RESUME: &str = r####"Usage: qd resume [options] <session>

Revive a cold session to a DRIVABLE state (agent-facing).

`resume` is the AGENT verb: it relaunches a cold session and brings it back to a
state you can drive with `qd send:relay <session> <text>`. It is non-TTY safe —
codex (daemon-hosted) sessions revive with NO interactive attach tail at all, and
the claude path's detached mode (`--no-attach`) leaves the session running in the
background without taking over your terminal. Humans who want to land inside a
session interactively should use `qd attach <session>` instead.

Options:
  --no-attach   Start detached (background) — revive to drivable, no tail
  --alt-screen  Fullscreen (alt-screen) rendering for this session (default:
                inline, so phone/SSH attach can scroll)
  --inline      Force inline rendering (overrides `render-default = alt-screen`)
  -h, --help    display help for command
"####;

pub const WRAP: &str = r####"Usage: qd wrap [options] <session>

Wrap a live bare Claude Code session under qrmux with the relay development
channel enabled.

When run from the target session, self-wrap prepares the existing manual
shutdown flow. For an external target, qd uses a best-effort foreground-child
idle heuristic, sends SIGTERM only after identity re-fencing, installs a
session-scoped Stop hook, resumes the same transcript under qrmux, and marks the
wrap final only after managed readiness is positively observed.

Options:
  -f, --force  Skip only the best-effort external idle heuristic
  -h, --help   display help for command
"####;

// `adopt` is the retired spelling, kept as a hidden backward-compat alias for wrap.
pub const ADOPT: &str = r####"Usage: qd adopt [options] <session>

(renamed — use qd wrap)

Options:
  -f, --force  Skip only the best-effort external idle heuristic
  -h, --help   display help for command
"####;

// P0 W1 (qb spec-cli §11): `stop` is today's `kill`, renamed — same backend
// (dual-reap + verify-gone + tombstone). W3 (ADD-15, wart-wave): the
// confirmation prompt is removed; --force stays parse-accepted as a deprecated
// no-op (see verbs/kill.rs W3 note).
pub const STOP: &str = r####"Usage: qd stop [options] <session>

Stop a session

Options:
  -f, --force  Deprecated no-op (stop never prompts)
  --server     Also kill the OpenCode server process
  -h, --help   display help for command
"####;

// P0 W1 (qb spec-cli §11): `kill` is RETIRED — erroring stub pointing at
// `qd stop` (see verbs/stubs.rs; the exact stderr line is pinned there).
pub const KILL: &str = r####"Usage: qd kill [options]

(retired — use qd stop)

Options:
  -h, --help  display help for command
"####;

// P0 W1 (qb spec-cli §11): `start` is today's `new`, renamed — same backend
// (lifecycle::run_new), same options + exit contract.
// P0 start-surface rework (STATE 21 ruling): `--resume` removed (redundant with
// the resume verb); `--fork <session>` is now valued — a new participant forked
// from an existing transcript. The model line below is the ruled wording.
//
// FTUE punch R6 / R19 / R20 — three edits to this string, one theme: it may no
// longer describe anything it does not do.
//   R6:  `--port` is GONE from this list. It was advertised here with a real
//        description while the verb refused it unconditionally; the flag itself
//        stays parse-accepted so the refusal survives (see `cli::cmd_start`).
//   R19: `--attach` is replaced by `--no-attach`. The old flag was A5-deferred
//        and only ever answered "not yet supported"; the default now attaches,
//        so what a human needs is the opt-out.
//   R20: the `--provider` entry says what an OMITTED provider does, which is no
//        longer one thing — a terminal is asked, everything else defaults.
pub const START: &str = r####"Usage: qd start [options] <name> [claudeArgs...]

Create a new session (claude-code, codex, pi, opencode)

start = new participant (fresh or forked) · resume = same participant wakes ·
attach = enter live or cold session.

Options:
  --cwd <dir>            Working directory for the session
  --fork <session>       Fork an existing session's transcript into this new
                         participant (session = name, id, or unique prefix)
  --turn <ordinal>       With --fork: rewind the fork to a past conversational-turn
                         boundary (default: latest safe)
  --no-attach            Start detached — do not attach after the session is
                         created. A start at a terminal hands you the session
                         it just made (the same handoff `qd attach <name>`
                         does); this opts out and returns instead. Agent and
                         piped callers never attach in the first place, so the
                         flag is for the human who wants the old behaviour
  --interactive          Force the interactive native-TUI launch (agent-marked
                         callers must pass it: QD_SESSION_ID in the caller's env
                         routes the auto-detect headless otherwise).
                         With --provider codex or pi this selects a different
                         TOPOLOGY: that harness's plain TUI in an attachable
                         pane (`qd attach <name>`) — for codex instead of the
                         app server, for pi instead of the extension-carrying
                         pane, i.e. the same pane WITHOUT a control channel.
                         Not supported for --provider opencode, or with --acp
                         (an ACP bridge is a protocol adapter with no terminal
                         to attach)
  --extension            pi only, and pi's DEFAULT lane: run pi's TUI in an
                         attachable pane WITH the quorum control channel, so
                         `qd send` drives the same session a human is typing
                         into. Redundant since the default moved, and kept so
                         existing scripts keep working and the lane can still be
                         named explicitly
  --acp                  Run the ACP bridge lane — a headless resident driven
                         over the Agent Client Protocol, with no terminal of its
                         own: drive it with `qd send`, not `qd attach`. Spells
                         claude-code/acp (the same claude engine, reached through
                         the bridge instead of a pane) and opencode/acp (which is
                         opencode's ONLY lane, so there it names the default).
                         Not available for codex or pi — no ACP adapter is wired
                         up for them in qd yet. Conflicts with --interactive,
                         --extension, --daemon and --app-server
  --app-server           codex only, and codex's DEFAULT lane: run the app
                         server (`codex app-server --listen ws://…`), a headless
                         resident a human can still open a viewer onto with
                         `qd attach <name>`. Redundant since the default moved,
                         and kept for the same reason --extension is: so a
                         script can name the lane instead of relying on which
                         way the default currently points
  --daemon               codex/pi only: run the headless daemon (codex/daemon,
                         pi/daemon) instead of the default lane — no mux pane,
                         no TTY, nothing to attach. The escape hatch for CI, ssh
                         and any no-mux context, and the only way to reach those
                         two lanes now that a bare start makes codex/app-server
                         and pi/extension. Conflicts with --interactive,
                         --extension and --app-server. Not supported for
                         --provider claude-code (it has no daemon lane) or
                         --provider opencode (its only residence is its ACP
                         bridge — use --acp, or nothing, which means the same)
  --headless             Force a headless stream-json launch (override the
                         driver auto-detect)
  --json                 Emit the started session's identity as JSON on stdout:
                         {name, qdId, sessionId, status, live}. Exit 0 guarantees
                         the id is bound; a bind failure emits {error: {class:
                         "unbound"|"ambiguous"|"diverged", ...}} and exits 1
  --no-await-relay       Skip the default relay-readiness wait (exit 0 then
                         means idle, not relay-reachable)
  -p, --prompt <prompt>  Send an initial prompt after the session starts
  --model <model>        Set the model before sending the prompt
  --provider <provider>  Which agent to run: claude-code, codex, pi or opencode.
                         A LANE may be named instead, as <provider>/<lane> —
                         `--provider codex/daemon` picks both the program and how
                         it is hosted, in one word. Every lane id `qd ls --json`
                         prints is accepted here, so a lane copied out of a
                         listing can be pasted back. Naming a lane PINS it: a
                         topology flag that asks for a different one is refused
                         rather than silently winning.
                         OMITTED AT A TERMINAL, qd ASKS — the choice is offered
                         from the harnesses `qd setup` found installed on this
                         machine, and Enter takes the default. Omitted
                         ANYWHERE ELSE (a pipe, a script, an agent session) it
                         resolves to claude-code without asking: a prompt no
                         one can answer is a hang, not a question.
                         Default lanes: claude-code runs its TUI in an
                         attachable pane; codex runs an app server you can also
                         open a terminal on (`qd attach`); pi runs its TUI in an
                         attachable pane carrying the quorum control channel;
                         opencode runs its ACP bridge, which is its only lane.
                         A provider names a PROGRAM and a lane names how it is
                         hosted, so each has a flag: --interactive for a plain
                         TUI pane (codex, pi), --daemon for the headless
                         resident (codex, pi), --acp for the Agent Client
                         Protocol bridge (claude-code, opencode). Each flag is
                         also sayable as a lane — `--provider pi --daemon` and
                         `--provider pi/daemon` are the same request.
                         The older spellings `acp/claude-code` and
                         `acp/opencode` still work and mean `claude-code/acp`
                         and `opencode/acp`
  --via <name>           Route through a backends.json profile (per-session backend)
  --alt-screen           Fullscreen (alt-screen) rendering for this session
                         (default: inline, so phone/SSH attach can scroll)
  --inline               Force inline rendering (overrides `render-default = alt-screen`)
  -h, --help             display help for command

Exit codes (with -p, for external composition — see doc/PROTOCOL.md, ADR 0008):
  0   Session created and ready — idle, stable id BOUND, and (unless
      --no-await-relay) relay-reachable; the prompt was accepted (went busy).
  10  Session created and ready, but the prompt was NOT confirmed submitted after
      bounded remediation. The session EXISTS — attach and check the composer.
  1   Any other failure (create/boot/bind error, or the PID file vanished after
      boot). Bind failures leave the session RUNNING and say so on stderr.
"####;

// P0 W1 (qb spec-cli §11): `new` is RETIRED — erroring stub pointing at
// `qd start` (see verbs/stubs.rs; the exact stderr line is pinned there).
pub const NEW: &str = r####"Usage: qd new [options]

(retired — use qd start)

Options:
  -h, --help  display help for command
"####;

pub const RECONCILE: &str = r####"Usage: qd reconcile [options]

Detect and repair drift across registry / mux / process (idempotent)

Options:
  --dry-run   Show what would be repaired without changing anything
  -h, --help  display help for command
"####;

pub const SEND: &str = r####"Usage: qd send <target> <message>

Send a message to a session. qd resolves the target and selects its registered
receive path automatically before making one delivery attempt.

Options:
  -h, --help  display help for command

The selected path is never changed after an attempt starts. A target that is
stopped, cold, ambiguous, self, or has no live receive path is refused.

Exit codes:
  0   delivered
  1   generic failure, including a target that is live but CONFIRMED to
      have no receive path (retrying will not help)
  12  refused{<class>} - a door refusal, printed as
      "qd send: refused{<class>}: <reason>". Classes include:
        address, host, ambiguous, unknown, self-send, no-live-receive-path
        receive-path-undetermined - the discovery read (e.g. `ps`) that
          would have found a receive path was DENIED, so relay/mux state
          is unknown, NOT absent. Unlike a confirmed absence, the remedy
          is to retry with the access that read needs (outside a sandbox,
          or with elevated permissions).

qd never reports a denied read as an absence. When discovery is degraded it
says so on stderr, prints the underlying OS error as its reason, and `qd info`
renders the affected fields as "unknown (<source> unavailable)" instead of "-".
"####;

pub const SEND_PTY: &str = r####"Usage: qd send:pty [options] <session> <message>

(Compatibility/debug control) Force a message through the session's PTY.

Options:
  --timeout <seconds>  Max wait time (default: "120")
  --full               Include all blocks (thinking, tool calls)
  --raw                Print raw JSONL lines
  --wait               Block and wait for the response
  -h, --help           display help for command

How it works:
  Types the message into the session's mux pane as if a human typed it,
  then presses Enter. The session processes it like normal user input.

Behavior:
  - Fire-and-forget by default. Add --wait to block until the response.
  - --wait anchors on the JSONL: it waits for your message to surface as a user
    record (the session taking it up), then reads the assistant response that
    follows, completing when the session returns to idle.
  - Messages are sent as a single mux send call with Enter appended.
  - Busy sessions are NOT refused: the TUI buffers input typed while busy and
    queues the submitted message, so it is queued and prints "Message queued ...
    (session busy)". The acceptance verify-then-CR is skipped on this path
    (never CR a busy session).

Requirements:
  - Session must be hosted in a mux pane (it has a terminal to type into).
  - --wait works on busy sessions too: it queues the message, then waits — the
    latency just includes the current turn finishing before yours runs. It
    anchors on the JSONL user record, so the response is always attributed to
    your message, never to the busy session's current task.

Pitfalls:
  - No delivery confirmation for fire-and-forget — if the message is malformed
    or the session is in an unexpected state, it fails silently. Queued
    (busy-session) sends are especially unconfirmed: verify-then-CR is skipped.
  - No retry mechanism. If the send fails, it's gone.
  - --wait response extraction depends on reading the JSONL conversation file.
    If the file path can't be found, --wait will error.
  - Not suitable for long-running tasks where the connection might drop. Use
    send:relay for those.

Best for: Multi-turn conversation, dialogue, back-and-forth exchanges where
you need to see the full response text including tool calls and thinking.
"####;

pub const SEND_RELAY: &str = r####"Usage: qd send:relay [options] <session> <message>

(Compatibility/debug control) Force relay/daemon send routing.

Options:
  --timeout <seconds>  Max wait for reply (default: "120")
  --wait               Block and wait for the reply instead of returning
                       immediately
  -h, --help           display help for command

How it works:
  HTTP POST to the session's relay MCP server, which delivers the message as
  a channel notification. The session sees it as an inbound message with
  from_session and message_id metadata.

Behavior:
  - Async by default: sends the message, prints the message_id, and exits.
  - Add --wait to long-poll for the reply (blocks until the session responds).
  - Uses fast resolution via sidecar files (~/.claude/relay/*.json) so port
    lookup is near-instant. Falls back to full session scan if needed.

Reliability:
  - --wait retries up to 3 times on connection drop with 2s delay between.
  - Relay server buffers resolved replies for 5 minutes. If your connection
    drops after the session replies, a retry will still retrieve the reply.
  - More reliable than send:pty for long-running tasks.

Requirements:
  - Session must have a running relay MCP server (started with server:relay).
  - For --wait: the target session must call the "reply" MCP tool with the
    message_id. This happens automatically when the session's relay server
    instructions tell it to, but if the session ignores the channel message,
    --wait will hang until timeout.

Pitfalls:
  - The reply comes from Claude's explicit "reply" tool call, not from JSONL.
    You get the reply text only, not thinking blocks or tool call details.
  - If the session doesn't have relay instructions loaded, it may process the
    message but never call the reply tool — --wait will timeout.
  - Async mode (no --wait) gives you no indication whether the session actually
    processed the message. Check session status separately if you need to know.

Best for: Task delegation, reporting results back, fire-and-forget dispatch,
any communication where reliability matters more than seeing full response
details.
"####;

pub const SEND_HTTP: &str = r####"Usage: qd send:http [options] <session> <message>

(Compatibility/debug control) Force the OpenCode HTTP path.

Options:
  --mode <mode>        Message envelope: report, execute, or raw (default:
                       "report")
  --timeout <seconds>  Max wait time (default: "300")
  -h, --help           display help for command

How it works:
  HTTP POST to the OpenCode server's sync endpoint, which blocks until the
  assistant completes its full turn (including any tool use) and returns the
  response.

Modes:
  --mode report    (default) Wraps message with "respond with text only"
  --mode execute   Wraps message with "complete the task and report results"
  --mode raw       Sends bare message with no envelope

Behavior:
  - Blocks until the full turn completes. Default timeout: 5 minutes.
  - Returns the assistant's text response only (no tool calls or thinking).
  - Timeout does NOT cancel the session's work — it just stops waiting.

Requirements:
  - Session must be an OpenCode session (provider: opencode).
  - OpenCode server must be running and reachable on the session's port.
  - For sessions that trigger tool use, permission prompts will block
    indefinitely. Use --dangerously-skip-permissions for worker sessions.

Best for: Sending tasks to OpenCode sessions, cross-provider messaging,
scripted interactions with OpenCode.
"####;

pub const RELAY: &str = r####"Usage: qd relay [options]

(moved) Use send:relay instead

Options:
  -h, --help  display help for command
"####;

pub const WHOAMI: &str = r####"Usage: qd whoami|name [options]

Print the current session's name and ID

Options:
  --json      Output as JSON
  -h, --help  display help for command
"####;

// W6+W7 (ADD-15, wart-wave): wait completion is now status- AND transcript-keyed
// (a turn shorter than one poll interval is caught via its JSONL record). The
// about line DIVERGES from the TS capture (22-help-wait.txt "Block until a
// session transitions from busy to idle") — sanctioned, normalized in the a3
// comparator, divergence row. send:pty --wait stays the documented per-message
// attribution path (already stated in SEND_PTY help at the pin).
pub const WAIT: &str = r####"Usage: qd wait [options] <session>

Block until the session's current turn completes (status- and transcript-keyed)

Options:
  --timeout <seconds>  Max wait time (default: "120")
  -h, --help           display help for command
"####;

pub const LIVE: &str = r####"Usage: qd live [options]

Live-updating session list — type a 3-char code to attach

Options:
  -a, --all   Include dead sessions
  -h, --help  display help for command
"####;

// B5 item 12 (doc note, accepted r2): the --json qdId absent-until-minted
// contract is surfaced here — behavior is unchanged and pinned by
// tests/info_json.rs (info_json_unmapped_qd_id_golden / mapped_live_golden).
pub const INFO: &str = r####"Usage: qd info [options] <session>

Detailed view of a single session

Options:
  --json      Output as JSON
  -h, --help  display help for command

--json: the qdId/qdIdPrefix keys are ABSENT until the session's stable id is
minted (ids are minted at `qd start` and bound at boot-confirm; `qd ls` lazily
backfills pre-existing sessions). Treat a missing qdId as "not yet minted",
not as an error — resolution stays engine-side.
"####;

pub const GC: &str = r####"Usage: qd gc [options]

Prune stale sessions and sidecars to recoverable trash

Options:
  --dry-run         Show what would be pruned without acting
  --list-trash      Show trash contents
  --recover <item>  Recover an item from trash
  --purge           Permanently delete trash items older than 30 days
  -h, --help        display help for command
"####;

// NET-NEW (2026-06-09 ruling): the eval-init shell integration. The wrapper
// body ships in the binary (see dispatch::shell_init module docs); the rc file
// carries one stable line, so the wrapper can never drift from what `qd new`
// accepts (the retired TS bootstrap baked the wrapper into rc files, and the
// baked copies fossilized when the engine's CLI moved).
//
// FTUE punch R1 (zmx retirement), boundary note: the PROSE here no longer names
// zmx, but the two `*_NO_ZMX` rows below keep it — those are the LITERAL env-var
// names `dispatch::shell_init` still emits into the wrapper bodies, so a user who
// wants the passthrough escape hatch has to type them exactly. RULE: help never
// documents a variable the code does not read, and never hides one it does.
// Renaming the variables belongs to whoever owns `shell_init.rs`; this const
// follows them, it does not lead them.
pub const INIT: &str = r####"Usage: qd init [options] <shell>

Print shell integration for <shell> (bash, zsh, or fish): `claude` and `codex`
wrappers that route a bare interactive launch into a tracked qd session, plus
the mux socket-dir pin. Evaluate it from your shell's rc file:

  bash   ~/.bashrc:                     eval "$(qd init bash)"
  zsh    ~/.zshrc:                      eval "$(qd init zsh)"
  fish   ~/.config/fish/conf.d/qd.fish: qd init fish | source

The claude wrapper passes management subcommands (config, login, mcp, ...),
headless runs (-p/--print), --version/--help, and non-TTY launches straight
through to the real claude. Escape hatch: `command claude ...`.

The codex wrapper routes to `qd start --provider codex --interactive` and is
narrower by design: it routes ONLY a bare `codex`, because that lane accepts no
launch argv — so `codex exec ...`, `codex resume`, and `codex "<prompt>"` reach
the real binary instead of losing what you typed. Escape hatch:
`command codex ...`.

Environment (read by the emitted wrappers at call time):
  QD_CLAUDE_WRAPPER_FLAGS  Extra flags (whitespace-split) injected on
                           passthrough REAL launches (headless / non-TTY /
                           already inside a mux pane) — never on management
                           subcommands or --version/--help. qd-routed launches
                           take their flags from the engine launcher
                           (QD_CLAUDE_FLAGS / config / defaults) instead.
  QD_CODEX_WRAPPER_FLAGS   The same, for the codex wrapper.
  CLAUDE_NO_ZMX            Set to disable claude routing (always passthrough).
  CODEX_NO_ZMX             Set to disable codex routing (always passthrough).

Options:
  -h, --help  display help for command
"####;

// Engine-truthful (A5 §4.3 + named divergence §9 item 3): the Rust engine ships
// via cargo or Homebrew, NOT `bun install -g`. `qd update` detects the install
// channel from the running exe path (Cellar/brew prefix → `brew upgrade quorum-dispatch`;
// ~/.cargo/bin → `cargo install --git <repo> --locked quorum-dispatch`) and re-runs it.
pub const UPDATE: &str = r####"Usage: qd update [options]

Self-update qd via the detected install channel (Homebrew or cargo). The channel
is inferred from the running executable's path; an undeterminable channel exits 1
with manual-reinstall guidance.

Options:
  -h, --help  display help for command
"####;

pub const PING: &str = r####"Usage: qd ping [options] [session]

Classify session liveness (drop-in for the legacy monitor.sh): exit 0=done
1=stuck 2=active 3=error 4=ambiguous. Use --prefix to sweep all sessions by name
prefix.

Options:
  --prefix <prefix>  Sweep all sessions whose name starts with <prefix>
  --json             Output as JSON
  -h, --help         display help for command
"####;

// ===========================================================================
// Top-level `qd --help` — GENERATED from the clap verb table (FTUE punch R4).
// ===========================================================================

/// The verbs `qd --help` lists — the ENTIRE human-facing `qd` surface (FTUE
/// punch R14, ruled in `doc/ftue/punch-list.md`, "Shipping shape").
///
/// RULE: a verb appears in `qd --help` iff it is named here **and** registered
/// unhidden in `cli::subcommands`. Every other verb stays FULLY REGISTERED and
/// FULLY WORKING — clap's `.hide(true)` suppresses the help row and NOTHING
/// else, so parsing and dispatch are untouched. That is the C1
/// "hidden-but-working" resolution: humans get this list, agents and power
/// users keep the whole surface and find it with `qd --help-all`.
///
/// `setup` used to sit in its own `First run:` section under a three-line note.
/// It is a command like the others — you run it, it does a thing — and giving
/// it a private section said the opposite: that a reader had to understand a
/// second concept before the table made sense. It is the last row now, which is
/// where a once-per-machine command belongs, and its own `--help` carries the
/// detail the note used to.
pub const HUMAN_VERBS: [&str; 5] = ["ls", "start", "stop", "attach", "setup"];

/// The one-line notice a top-level help prints when this machine's install is
/// not finished (FTUE punch: the help says so instead of announcing "first run"
/// to everyone, including the thousandth run on a wired machine).
///
/// It is the ONLY state-dependent line in the help, and it is deliberately a
/// pointer rather than a diagnosis: `qd setup` already reports check-by-check,
/// and duplicating any of that here would be a second place to keep true.
const SETUP_INCOMPLETE_NOTICE: &str =
    "This machine is not fully set up — run `qd setup` to see what is missing.";

/// Heading for the harness roster (FTUE punch **R28**).
///
/// "on this machine" earns its three words: every other section of this help is
/// a property of `qd` and reads the same everywhere, and this one is the only
/// one that describes the reader's laptop. Without the qualifier the block reads
/// as a catalogue of what qd supports, which is what the `start` row already
/// says and is exactly the wrong thing for someone trying to work out why their
/// harness will not start.
const HARNESS_HEADING: &str = "Harnesses on this machine:";

/// What running `qd setup` will actually do to a machine — the two facts that
/// decide whether a person is willing to type it.
///
/// It sits under the roster rather than in its own section because it is the
/// roster's answer: every `run \`qd setup\`` in the rows above is a suggestion
/// to run something, and a suggestion is worth less than nothing to someone who
/// does not know whether it writes to their shell profile. `setup` used to own a
/// three-line `First run:` block that said this to everyone, on every run, on
/// every machine; this says it to the people the rows just pointed at setup.
const SETUP_POSTURE: &str = "Report-only by default: `qd setup` reports what is missing and writes \
                             nothing; `qd setup --fix` applies it. Safe to re-run.";

/// Every provider `qd start --provider` actually accepts, as the help prints
/// them.
///
/// DERIVED from `Harness::ALL`, never hand-listed: the accept-set lives in
/// `Harness::from_provider_id` (which is also what `Lane::for_create` routes
/// on), so a harness added there shows up in the help on the next build and a
/// harness removed there stops being advertised.
///
/// There is no alias parenthetical any more. `opencode` used to print as
/// `opencode (= acp/opencode)` because the name a person typed and the id qd
/// stored were different strings — a split ACP-as-a-harness forced, since the
/// transport had to be in the id and so the id could not be the program name.
/// Every harness has exactly one spelling now. The legacy `acp/*` ids still
/// parse, for rows and scripts written before the remodel; they are not
/// advertised, because advertising two spellings for one thing is how the
/// split started.
pub fn provider_list() -> String {
    use quorum_qw::lane::Harness;
    Harness::ALL
        .iter()
        .map(|h| h.provider_id())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `start` row's one-liner, and the first line of [`START`] — one string so
/// the table and the verb's own `--help` can never name different providers.
pub fn start_about() -> String {
    format!("Create a new session ({})", provider_list())
}

/// Width of the option gutter in [`start_human`]: two spaces, the widest flag
/// spelling (`--provider <provider>`, 21 chars), two spaces. Descriptions start
/// here and continuations hang here.
const GUTTER: usize = 25;

/// Hard-wrap an option's description to the help's 80-column page, hanging the
/// continuation lines under `indent` (the width of the `  --flag <val>  ` gutter).
///
/// WHY this is code and not a hand-wrapped literal: the only description in
/// [`start_human`] that needs it is `--provider`, and the reason it needs it is
/// that it interpolates [`provider_list`] — a string DERIVED from `Harness::ALL`.
/// A hand-wrap is correct for exactly the harness set it was typed against and
/// silently overruns the page on the next harness added, which is the same class
/// of drift `provider_list` itself exists to prevent. Wrapping at print time
/// makes the layout a function of the content instead of a snapshot of it.
///
/// Word boundaries only — a single word longer than the available width is left
/// to overrun rather than broken mid-token, because every "word" here is a
/// provider id or a flag name and a split one would be a lie. (`start_human`'s
/// 80-column guarantee is pinned by test; provider ids are far short of 55.)
///
/// Widths are counted in CHARACTERS, not bytes: this help carries `·` and `—`,
/// and `str::len` would under-wrap every line containing them.
fn wrap_hanging(desc: &str, indent: usize) -> String {
    const PAGE: usize = 80;
    let avail = PAGE.saturating_sub(indent);
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut col = 0usize;
    for word in desc.split_whitespace() {
        let w = word.chars().count();
        if col == 0 {
            out.push_str(word);
            col = w;
        } else if col + 1 + w <= avail {
            out.push(' ');
            out.push_str(word);
            col += 1 + w;
        } else {
            out.push('\n');
            out.push_str(&pad);
            out.push_str(word);
            col = w;
        }
    }
    out
}

/// The HUMAN view of `qd start --help` — [`START`] is the agent/canonical one.
///
/// WHY there are two: [`START`] documents the whole verb, and the whole verb is
/// a composition surface — lanes (`--extension`, `--app-server`, `--daemon`),
/// topology (`--interactive`, `--alt-screen`, `--inline`), machine plumbing
/// (`--json`, `--no-await-relay`, `--via`) and the exit-code contract an
/// external composer branches on. That page is CORRECT and it is what an agent
/// needs; it is also thirty options deep, and a person who typed `qd start` and
/// got it wrong does not need a lane taxonomy — they need the four options they
/// actually choose between (where it runs, what to say, which model, which
/// harness) and one example they can retype.
///
/// So the split is by AUDIENCE, not by truth: nothing here contradicts
/// [`START`], it is a strict subset of it plus a pointer at the rest. The
/// driver decides which one prints (`cli::map_clap_error_for`), and the pointer
/// is `qd start --help | cat` because a pipe is exactly the signal that already
/// resolves the driver to Agent — the escape hatch is the same fact, not a new
/// flag.
///
/// The description line does NOT reuse [`start_about`]. That string answers
/// "which providers?", which is the table's job in a list of twenty verbs; a
/// person who opened THIS page has already chosen the verb and needs to know
/// what starting a session buys them — that it is qd-wrapped, and therefore
/// reachable from every other qd-wrapped session. The provider set is still
/// derived, one line down, where `--provider` interpolates [`provider_list`],
/// so the accept-set cannot drift here either. That derivation is also why the
/// `--provider` entry is laid out by [`wrap_hanging`] rather than typed: its
/// text grows with `Harness::ALL`, so its line breaks have to be computed from
/// the text, not frozen against one day's harness set. The other three options
/// are static and already fit, and stay literal.
pub fn start_human() -> String {
    format!(
        r####"Usage: qd start [options] <name> [claudeArgs...]

Example: qd start claude1 --provider claude-code
         qd start pi1 --provider pi

Create a new qd wrapped session that can communicate with any other
qd wrapped session

Options:
  --cwd <dir>            Working directory for the session
  -p, --prompt <prompt>  Send an initial prompt after the session starts
  --model <model>        Set the model before sending the prompt
  --provider <provider>  {provider}
  -h, --help             display help for command

Every option: `qd start --help | cat` (the full agent-facing list).
"####,
        provider = wrap_hanging(
            &format!(
                "Provider: {}. At a terminal qd asks if you omit it; anywhere \
                 else it defaults to claude-code.",
                provider_list()
            ),
            GUTTER,
        ),
    )
}

/// The SHORT, human view of one verb's `--help`, or `None` for a verb that has
/// only the one page.
///
/// This is the lookup [`crate::cli::map_clap_error_for`] asks in both places it
/// forks on the driver — the `--help` arm and the bad-option arm. It is a
/// function rather than an `invoked_verb(argv) == Some("start")` written twice
/// because the SECOND verb to earn a human view is what proved the shape: that
/// comparison was already duplicated across two arms in one file, and a third
/// verb would have meant six sites to keep in agreement. Here a new human view
/// costs one arm of one match, and every verb without one keeps today's output
/// by falling out as `None` — the fork's default is "print what clap rendered",
/// and it stays that way for the twenty-odd verbs nobody has written a short
/// page for.
///
/// `connect` answers with the ATTACH view because it IS attach: a hidden
/// backward-compat spelling that `verbs::run` routes straight to `attach::run`,
/// so a person who typed the retired name asked the attach question and is owed
/// the attach answer — not a stub that spends their `--help` telling them to
/// type it again.
///
/// But they are owed the RENAME too, and it does not survive on its own: hand
/// someone a page headed `qd attach` when they typed `qd connect` and nothing
/// on it says why. So the human `connect` view is the notice AND the answer.
/// The agent path is untouched — [`CONNECT`] still prints there verbatim,
/// because a script that reads the stub is testing for the stub.
pub fn human_view(verb: &str) -> Option<String> {
    match verb {
        "start" => Some(start_human()),
        "attach" => Some(ATTACH_HUMAN.to_string()),
        "connect" => Some(format!(
            "`qd connect` was renamed — this is `qd attach`.\n\n{ATTACH_HUMAN}"
        )),
        _ => None,
    }
}

/// Section header for the hidden surface, printed only by `qd --help-all`.
const HIDDEN_HEADING: &str = "Hidden from `qd --help` (agent-facing, machinery, compat — all still working):";

/// Render the top-level help table by WALKING the clap command (FTUE punch R4).
///
/// RULE: the verb table is never written by hand. Its predecessor — a
/// `help::TOP` string const — had already drifted: `dispositions`, `mark` and
/// `delivery:recover` were live, unhidden verbs it never listed, and `attach`
/// was listed without the options it takes. Reading every row back off the
/// registration (`get_subcommands` → `get_visible_aliases` / `get_about` /
/// `get_positionals` / `is_hide_set`) makes that class of drift structurally
/// impossible: `cli::subcommands()` becomes the ONE place a verb is declared.
///
/// The commander layout is preserved deliberately (`Usage: qd [options]
/// [command]`, the `ls|list` alias style, the two-space table) — R4 changed
/// where the bytes come from, not what they look like.
///
/// `include_hidden` is the `qd --help-all` surface: the same table with one
/// extra section listing the verbs `--help` suppresses.
///
/// `setup_incomplete` is the one fact the tree cannot answer: whether THIS
/// machine's install is finished. It is a parameter rather than a probe because
/// the help is also rendered from `cli::build_cli`, which every invocation
/// builds — a filesystem probe in there would be paid by `qd send:relay` to
/// print nothing. The print sites that can afford the probe pass it; the ones
/// that cannot pass `false`.
///
/// `harnesses` is the R28 roster, and it is a parameter for exactly the same
/// reason and on exactly the same terms: an EMPTY slice means "not probed", not
/// "you have no harnesses", and renders no block at all. `cli::build_cli` passes
/// `&[]` because it may not touch the disk; the four surfaces that actually
/// print this text — `qd --help`, `qd --help-all`, bare `qd`, and the tail of a
/// completed `qd setup` — pass the real roster.
///
/// So the help now has TWO state-dependent parts rather than one. The rule the
/// single one was defending still holds and is worth restating: neither part
/// diagnoses. `qd setup` remains the only surface that explains a harness —
/// version, pin drift, the exact export for an off-`PATH` install — and the
/// roster deliberately carries none of that, because a second place that
/// explains is a second place to keep true. What it carries is the fact you
/// cannot get from a verb table: which of these four this machine can actually
/// run right now.
pub fn render_top(
    cmd: &clap::Command,
    include_hidden: bool,
    setup_incomplete: bool,
    harnesses: &[dispatch::setup::harness::HarnessFacts],
) -> String {
    let row = |sub: &clap::Command| (signature(sub), about_line(sub));
    let find = |name: &str| cmd.get_subcommands().find(|s| s.get_name() == name);
    let classified = |name: &str| HUMAN_VERBS.contains(&name);

    // The Options rows are clap builtins (`-V/--version`, `-h/--help`), not verb
    // registrations, so they are the one hand-written pair in this function.
    let options: Vec<(String, String)> = vec![
        ("-V, --version".into(), "output the version number".into()),
        (
            "-h, --help".into(),
            "display help — append it to any command for that command's help".into(),
        ),
    ];

    let mut sections: Vec<(&str, Vec<(String, String)>)> = Vec::new();

    // The human verbs in the RULED order (the session lifecycle ls/start/stop/
    // attach, then `setup`) rather than registration order — the punch item
    // names that sequence, and it reads as the lifecycle it is.
    let commands: Vec<_> = HUMAN_VERBS
        .iter()
        .filter_map(|n| find(n))
        .filter(|s| !s.is_hide_set())
        .map(row)
        .collect();
    if !commands.is_empty() {
        sections.push(("Commands:", commands));
    }

    // Safety net, and the reason this is a walk and not a lookup: a verb that is
    // registered unhidden but named by NEITHER list still gets a row. Unhiding a
    // verb can make the help wrong-looking; it can never make the verb invisible.
    let other: Vec<_> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set() && !classified(s.get_name()))
        .map(row)
        .collect();
    if !other.is_empty() {
        sections.push(("Other commands:", other));
    }

    if include_hidden {
        let hidden: Vec<_> = cmd.get_subcommands().filter(|s| s.is_hide_set()).map(row).collect();
        if !hidden.is_empty() {
            sections.push((HIDDEN_HEADING, hidden));
        }
    }

    // Commander aligns every term — options and commands alike — to one column,
    // two spaces past the longest.
    let width = options
        .iter()
        .chain(sections.iter().flat_map(|(_, rows)| rows.iter()))
        .map(|(term, _)| term.chars().count())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    out.push_str("Usage: qd [options] [command]\n\n");
    // qd is not a Claude tool that grew. It runs sessions on every harness it
    // supports and carries messages BETWEEN them — `qd send:relay` is as much
    // the product as `qd start` — and a summary line naming one vendor told a
    // new reader the opposite.
    out.push_str("Run coding-agent sessions across providers, and message the agents in them.\n");
    push_section(&mut out, "Options:", &options, width);
    for (heading, rows) in &sections {
        push_section(&mut out, heading, rows, width);
    }
    // R28: the roster, on its OWN alignment. Sharing `width` with the verb table
    // would let a harness label move every command description in the help, and
    // the two blocks are not one table — the terms above are things you type,
    // and these are things you have installed.
    if !harnesses.is_empty() {
        let rows = dispatch::setup::harness::help_rows(harnesses);
        let rw = rows.iter().map(|(t, _)| t.chars().count()).max().unwrap_or(0);
        push_section(&mut out, HARNESS_HEADING, &rows, rw);
        out.push('\n');
        out.push_str(SETUP_POSTURE);
        out.push('\n');
    }

    // The other line that depends on the machine rather than the tree — and
    // the reason the help no longer greets everyone with "First run": a wired
    // machine says nothing, and an unwired one says the one thing that is true
    // of it.
    if setup_incomplete {
        out.push('\n');
        out.push_str(SETUP_INCOMPLETE_NOTICE);
        out.push('\n');
    }
    out
}

/// One `Heading:` block of aligned `  term  description` rows.
fn push_section(out: &mut String, heading: &str, rows: &[(String, String)], width: usize) {
    out.push('\n');
    out.push_str(heading);
    out.push('\n');
    for (term, desc) in rows {
        let pad = width.saturating_sub(term.chars().count());
        out.push_str("  ");
        out.push_str(term);
        for _ in 0..pad {
            out.push(' ');
        }
        out.push_str("  ");
        out.push_str(desc);
        out.push('\n');
    }
}

/// The commander invocation signature for one verb, derived from its clap
/// registration: `name|alias [options] <required> [optional] [variadic...]`.
/// Nothing here is hand-written, so a flag or positional added to a verb shows
/// up in the table on the next build.
fn signature(sub: &clap::Command) -> String {
    let mut sig = sub.get_name().to_string();
    for alias in sub.get_visible_aliases() {
        sig.push('|');
        sig.push_str(alias);
    }
    // `[options]` iff the verb registers a flag of its own. clap's auto
    // `help`/`version` args don't count — every verb has those, so listing them
    // would put `[options]` on every row and mean nothing.
    let has_options = sub.get_arguments().any(|a| {
        !a.is_positional() && !matches!(a.get_id().as_str(), "help" | "version")
    });
    if has_options {
        sig.push_str(" [options]");
    }
    for arg in sub.get_positionals() {
        let name = arg
            .get_value_names()
            .and_then(|names| names.first())
            .map(|n| n.to_string())
            .unwrap_or_else(|| arg.get_id().as_str().to_string());
        let variadic = arg.get_num_args().is_some_and(|r| r.max_values() > 1);
        let name = if variadic { format!("{name}...") } else { name };
        sig.push(' ');
        if arg.is_required_set() {
            sig.push_str(&format!("<{name}>"));
        } else {
            sig.push_str(&format!("[{name}]"));
        }
    }
    sig
}

/// A verb's `about` as ONE table line. Descriptions are written as Rust string
/// continuations, so they arrive with embedded newlines/indentation; the table
/// is a single-line-per-verb layout, so collapse the whitespace rather than let
/// a continuation break the column.
fn about_line(sub: &clap::Command) -> String {
    sub.get_about()
        .map(|a| a.to_string())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
