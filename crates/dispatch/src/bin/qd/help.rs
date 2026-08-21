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
// the first-run entry a human reaches straight after `brew install`. It is
// hand-written to the same commander layout as the rest of this file precisely
// because it is one of the few verbs `qd --help` shows: a verb on the first-run
// path that renders clap's default layout is the one place the CLI would look
// unfinished.
pub const SETUP: &str = r####"Usage: qd setup [options]

First run: set up qd's install layout and wire up your agent harnesses.

Run this once after installing qd. It checks everything qd needs and prints a
verdict per check — the `~/.quorum` layout, that `qw` sits beside `qd` (qd
resolves it as a sibling and never via PATH, so a missing one cannot open a
session at all), that `qd` is on PATH, and the relay pin in `~/.claude.json`
that lets Claude Code launch qd's agent-messaging server. It then reports which
agent harnesses you have — Claude Code, codex, pi, opencode — with their
versions, and wires up the ones that are present.

Without --fix it changes NOTHING: it reports what is wrong and, under each
failing check, the exact thing that would fix it. On a terminal it offers to
apply them; run non-interactively it just reports. Safe to re-run — every step
is idempotent, and a second run on a wired machine is a no-op.

Options:
  --fix       Apply every fix it can, without asking
  --json      Report the detected state as JSON and exit (never writes anything)
  -y, --yes   Assume yes for every prompt (same effect as --fix)
  -h, --help  display help for command

Exit code: 0 when everything needed is in place (or was fixed this run), 1 when
something required is still missing — including what --fix cannot do for you,
like an incomplete Homebrew install or a `~/.claude.json` that is not valid JSON
(setup will not rewrite a file it cannot parse).
"####;

// B5 item 2 (additive, orc-ruled C1+D): `--live` + the trailer note extend the
// TS-era corpus capture (the same sanctioned shape as info's `--json` line).
pub const LS: &str = r####"Usage: qd ls|list [options]

List Claude Code sessions (use --json for scripting)

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

// `attach` is the human "get me into this session" verb. The help
// text describes the provider/liveness matrix in human terms (no mux
// internals leak to the user — the daemon/cold wording matches what the verb
// actually prints).
pub const ATTACH: &str = r####"Usage: qd attach [options] <session>

Get into a session — the one verb for "take me there".

For a live Claude session this opens an interactive terminal. A cold Claude
session is revived and then opened. A codex session is
daemon-hosted (no terminal): it points you at `qd send:relay` / `qd resume`.

Options:
  --no-attach   Revive a cold session to a persistent daemon and return 0
                WITHOUT attaching a TTY (headless — e.g. a systemd ExecStart)
  --alt-screen  Fullscreen (alt-screen) rendering if this attach revives the
                session (default: inline, so phone/SSH attach can scroll)
  --inline      Force inline rendering (overrides `render-default = alt-screen`)
  -h, --help    display help for command
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
pub const START: &str = r####"Usage: qd start [options] <name> [claudeArgs...]

Create a new session (claude-code by default; also codex, pi, opencode)

start = new participant (fresh or forked) · resume = same participant wakes ·
attach = enter live or cold session.

Options:
  --cwd <dir>            Working directory for the session
  --fork <session>       Fork an existing session's transcript into this new
                         participant (session = name, id, or unique prefix)
  --turn <ordinal>       With --fork: rewind the fork to a past conversational-turn
                         boundary (default: latest safe)
  --attach               Attach interactively instead of starting detached
  --interactive          Force the interactive native-TUI launch (agent-marked
                         callers must pass it: QD_SESSION_ID in the caller's env
                         routes the auto-detect headless otherwise).
                         With --provider codex or pi this selects a different
                         TOPOLOGY: that harness's plain TUI in an attachable
                         pane (`qd attach <name>`) — for codex instead of the
                         app server, for pi instead of the extension-carrying
                         pane, i.e. the same pane WITHOUT a control channel.
                         Not supported for --provider acp/* (a protocol adapter,
                         no terminal to attach)
  --extension            pi only, and pi's DEFAULT lane: run pi's TUI in an
                         attachable pane WITH the quorum control channel, so
                         `qd send` drives the same session a human is typing
                         into. Redundant since the default moved, and kept so
                         existing scripts keep working and the lane can still be
                         named explicitly
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
                         --provider claude-code
                         (it has no daemon lane); a no-op for acp/*, whose only
                         lane is already a daemon
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
  --provider <provider>  Provider: claude-code (default), codex, pi,
                         opencode (= acp/opencode), or acp/claude-code.
                         Default lanes: claude-code runs its TUI in an
                         attachable pane; codex runs an app server you can also
                         open a terminal on (`qd attach`); pi runs its TUI in an
                         attachable pane carrying the quorum control channel;
                         acp/* is daemon-hosted only (drive it with `qd send`,
                         not `qd attach`). Use --interactive for a plain TUI
                         pane (codex, pi) or --daemon for the headless resident
                         (codex, pi)
  --port <port>          Port for OpenCode server (default: auto-scan 4096-4106)
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

/// The four SESSION verbs — the ENTIRE human-facing `qd` surface (FTUE punch
/// R14, ruled in `doc/ftue/punch-list.md`, "Shipping shape").
///
/// RULE: a verb appears in `qd --help` iff it is named here (or in
/// [`FIRST_RUN_VERBS`]) **and** registered unhidden in `cli::subcommands`.
/// Every other verb stays FULLY REGISTERED and FULLY WORKING — clap's
/// `.hide(true)` suppresses the help row and NOTHING else, so parsing and
/// dispatch are untouched. That is the C1 "hidden-but-working" resolution:
/// humans get four verbs, agents and power users keep the whole surface and
/// find it with `qd --help-all`.
pub const SESSION_VERBS: [&str; 4] = ["ls", "start", "stop", "attach"];

/// The first-run entry — R14's one exception to the four-verb rule. `setup` is
/// how a human gets from `brew install` to a working install, so it stays
/// visible, but in its OWN section: it is a thing you run once, not a fifth
/// session verb, and grouping it with the four would say otherwise.
pub const FIRST_RUN_VERBS: [&str; 1] = ["setup"];

/// Section header for the hidden surface, printed only by `qd --help-all`.
const HIDDEN_HEADING: &str = "Hidden from `qd --help` (agent-facing, machinery, compat — all still working):";

/// The `--help` trailer that makes the hidden surface discoverable (R4).
const HELP_ALL_POINTER: &str = "\
Only the session verbs and `setup` are listed here. Every other verb is still
registered and working — `qd --help-all` prints the full surface.";

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
/// [command]`, the `ls|list` alias style, the two-space table, `-h, --help
/// display help for command`) — R4 changes where the bytes come from, not what
/// they look like.
///
/// `include_hidden` is the `qd --help-all` surface: the same table with one
/// extra section listing the verbs `--help` suppresses.
pub fn render_top(cmd: &clap::Command, include_hidden: bool) -> String {
    let row = |sub: &clap::Command| (signature(sub), about_line(sub));
    let find = |name: &str| cmd.get_subcommands().find(|s| s.get_name() == name);
    let classified = |name: &str| SESSION_VERBS.contains(&name) || FIRST_RUN_VERBS.contains(&name);

    // The Options rows are clap builtins (`-V/--version`, `-h/--help`), not verb
    // registrations, so they are the one hand-written pair in this function.
    let options: Vec<(String, String)> = vec![
        ("-V, --version".into(), "output the version number".into()),
        ("-h, --help".into(), "display help for command".into()),
    ];

    let mut sections: Vec<(&str, Vec<(String, String)>)> = Vec::new();

    // The four session verbs, in the RULED order (ls/start/stop/attach) rather
    // than registration order — the punch item names that sequence, and it reads
    // as the lifecycle it is.
    let session: Vec<_> = SESSION_VERBS
        .iter()
        .filter_map(|n| find(n))
        .filter(|s| !s.is_hide_set())
        .map(row)
        .collect();
    if !session.is_empty() {
        sections.push(("Commands:", session));
    }

    let first_run: Vec<_> = FIRST_RUN_VERBS
        .iter()
        .filter_map(|n| find(n))
        .filter(|s| !s.is_hide_set())
        .map(row)
        .collect();
    if !first_run.is_empty() {
        sections.push(("First run:", first_run));
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
    out.push_str("Claude Sessions — manage Claude Code sessions\n\n");
    // The STATE-21 model line (spec-w7-start-surface D1): the one piece of
    // orientation the table itself cannot carry.
    out.push_str(
        "start = new participant (fresh or forked) · resume = same participant wakes ·\n\
         attach = enter live or cold session.\n",
    );
    push_section(&mut out, "Options:", &options, width);
    for (heading, rows) in &sections {
        push_section(&mut out, heading, rows, width);
    }
    if !include_hidden {
        out.push('\n');
        out.push_str(HELP_ALL_POINTER);
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
