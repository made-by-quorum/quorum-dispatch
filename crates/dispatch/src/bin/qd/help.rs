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
// text describes the provider/liveness matrix in human terms (no `zmx`/mux
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
  --no-zmx           Don't wrap in a zmx session
  --no-attach        Start detached (background) — revive to drivable, no tail
  --zmx-name <name>  Custom zmx session name
  --alt-screen       Fullscreen (alt-screen) rendering for this session
                     (default: inline, so phone/SSH attach can scroll)
  --inline           Force inline rendering (overrides `render-default = alt-screen`)
  -h, --help         display help for command
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

Create a new session (Claude Code in zmx, or OpenCode server)

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
                         With --provider codex this selects a different
                         TOPOLOGY: the codex TUI in an attachable pane
                         (`qd attach <name>`) instead of the app-server daemon.
                         Not supported for --provider pi / acp/* (daemon-hosted,
                         no terminal to attach)
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
                         claude-code runs in an attachable pane; codex runs an
                         app-server daemon by default and an attachable TUI pane
                         with --interactive; the rest are daemon-hosted (drive
                         them with `qd send`, not `qd attach`)
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

Detect and repair drift across registry / zmx / process (idempotent)

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
"####;

pub const SEND_PTY: &str = r####"Usage: qd send:pty [options] <session> <message>

(Compatibility/debug control) Force a message through the zmx PTY.

Options:
  --timeout <seconds>  Max wait time (default: "120")
  --full               Include all blocks (thinking, tool calls)
  --raw                Print raw JSONL lines
  --wait               Block and wait for the response
  -h, --help           display help for command

How it works:
  Types the message into the session's zmx terminal as if a human typed it,
  then presses Enter. The session processes it like normal user input.

Behavior:
  - Fire-and-forget by default. Add --wait to block until the response.
  - --wait anchors on the JSONL: it waits for your message to surface as a user
    record (the session taking it up), then reads the assistant response that
    follows, completing when the session returns to idle.
  - Messages are sent as a single zmx send call with Enter appended.
  - Busy sessions are NOT refused: the TUI buffers input typed while busy and
    queues the submitted message, so it is queued and prints "Message queued ...
    (session busy)". The acceptance verify-then-CR is skipped on this path
    (never CR a busy session).

Requirements:
  - Session must be in zmx (has a zmx terminal).
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
pub const INIT: &str = r####"Usage: qd init [options] <shell>

Print shell integration for <shell> (bash, zsh, or fish): `claude` and `codex`
wrappers that route a bare interactive launch into a tracked qd session, plus
the ZMX_DIR pin. Evaluate it from your shell's rc file:

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
                           inside-zmx) — never on management subcommands or
                           --version/--help. qd-routed launches take their
                           flags from the engine launcher (QD_CLAUDE_FLAGS /
                           config / defaults) instead.
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

/// Top-level `qd --help` (rows 01/02). Built from corpus 01 with the spawn
/// line removed (sanctioned divergence — qb, parked) and the bootstrap
/// description replaced with the engine-only one-liner (spec §3 row 17; the
/// harness normalizes this line per the orc bootstrap-help ruling). config +
/// survey ARE listed (they dispatch pre-clap but appear in the command list).
/// P0 start-surface rework (STATE 21 ruling): the start/resume/attach model
/// line is a further sanctioned divergence (spec-w7-start-surface D1).
pub const TOP: &str = r####"Usage: qd [options] [command]

Claude Sessions — manage Claude Code sessions

start = new participant (fresh or forked) · resume = same participant wakes ·
attach = enter live or cold session.

Options:
  -V, --version                             output the version number
  -h, --help                                display help for command

Commands:
  ls|list [options]                         List Claude Code sessions (use --json for scripting)
  attach <session>                          Get into a session (live/cold Claude → terminal; codex → driving guidance)
  resume [options] <session>                Resume a dead session (wraps in zmx by default)
  wrap <session>                            Prepare this bare Claude session for a manual qrmux relaunch
  start [options] <name> [claudeArgs...]    Create a new session (Claude Code in zmx, or OpenCode server)
  stop [options] <session>                  Stop a session
  kill [options]                            (retired — use qd stop)
  new [options]                             (retired — use qd start)
  reconcile [options]                       Detect and repair drift across registry / zmx / process (idempotent)
  send <session> <message>                  Send a message (delivery path selected automatically)
  send:pty [options] <session> <message>    (compatibility/debug) Force a zmx PTY send
  send:relay [options] <session> <message>  (compatibility/debug) Force relay/daemon send routing
  send:http [options] <session> <message>   (compatibility/debug) Force the OpenCode HTTP path
  relay                                     (moved) Use send:relay instead
  whoami|name [options]                     Print the current session's name and ID
  wait [options] <session>                  Block until a session transitions from busy to idle
  live [options]                            Live-updating session list — type a 3-char code to attach
  info <session>                            Detailed view of a single session
  gc [options]                              Prune stale sessions and sidecars to recoverable trash
  init <shell>                              Print shell integration (claude + codex wrappers) — add `eval "$(qd init bash)"` to your rc file
  bootstrap                                 Set up qd's local data directory under ~/.quorum/dispatch (idempotent)
  update                                    Self-update qd via the detected install channel (Homebrew or cargo).
  ping [options] [session]                  Classify session liveness (drop-in for the legacy monitor.sh): exit 0=done 1=stuck 2=active 3=error 4=ambiguous. Use --prefix to sweep all sessions by name prefix.
  survey                                    Fan an artifact out to a panel of LLMs via OpenRouter and collect responses (the panel-review / panel-ideate mechanic). Requires OPENROUTER_API_KEY.
  config                                    Manage stored secrets (e.g. `qd config set openrouter-key`). Tiered backend: macOS Keychain when available, else a chmod-600 ~/.quorum/dispatch/config.toml. Env var overrides.
"####;
