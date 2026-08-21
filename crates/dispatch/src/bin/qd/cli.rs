//! The clap command tree + the centralized commander-error mapping (spec §2).
//!
//! ## Error mapping (the load-bearing parity surface)
//!
//! clap exits **2** on a parse error and prints its own phrasing; commander
//! exits **1** with `error: <thing>`. [`map_clap_error`] catches every clap
//! `Error` and re-renders it. The exact strings are centralized in ONE match
//! (`commander_message`) so the jail-captured corpus (M3) can refine them with a
//! single edit — never scattered across verbs. `-h/--help` and `-V/--version`
//! are clap "errors" that carry the rendered text and exit **0**.

use clap::error::ErrorKind;
use clap::{Arg, ArgAction, Command};

use crate::help;

/// Version string (index.ts:32 — `program.version("0.1.0")`).
pub const VERSION: &str = "0.1.0";

/// The commit `qd` was built from, resolved by `build.rs` (live `git rev-parse`,
/// else the `.build-sha` file the `build-sha` workflow keeps current, else
/// `unknown`). 12 hex chars, or the literal `"unknown"` — never empty.
pub const BUILD_SHA: &str = env!("QD_BUILD_SHA");

/// What `qd --version` prints: `0.1.0 (92acbe35bc60)`.
///
/// The sha is APPENDED, never substituted. `VERSION` stays the bare `0.1.0`
/// that every other surface renders (`qd doctor`'s `qd: 0.1.0`, the dry-run
/// goldens' `binary: … (0.1.0)`), so this change is confined to the one line a
/// human reads when asking which build they are running. When the sha could not
/// be resolved the line degrades to exactly the old bytes — the TS-parity output
/// (index.ts:32) is the floor, not a special case.
pub fn version_line() -> String {
    if BUILD_SHA == "unknown" {
        VERSION.to_string()
    } else {
        format!("{VERSION} ({BUILD_SHA})")
    }
}

/// Build the full clap command tree. Builder API per spec §2 (NOT derive).
///
/// Layout choices: `disable_version_flag(false)` keeps `-V/--version`;
/// `disable_help_subcommand(true)` so `qd help` is an unknown command (commander
/// has no `help` subcommand). We map errors ourselves, so we suppress clap's own
/// error/exit by using `try_get_matches_from` at the call site.
pub fn build_cli() -> Command {
    let cmd = Command::new("qd")
        .about("Run coding-agent sessions across providers, and message the agents in them")
        .version(VERSION)
        // We render version/help text but map exits ourselves.
        .disable_help_subcommand(true)
        .subcommand_required(false)
        .arg_required_else_help(false)
        .allow_external_subcommands(false)
        .subcommands(subcommands());
    // FTUE punch R4: the top-level help is GENERATED from the tree we just
    // built, not hand-written. It keeps the commander layout (H1, spec §2:
    // `Usage: qd [options] [command]`, two-space table, `ls|list` alias style)
    // — only the SOURCE of the bytes changed. The predecessor was a
    // `help::TOP` const, and it had drifted: three live unhidden verbs
    // (`dispositions`, `mark`, `delivery:recover`) were missing from it.
    // `subcommands()` below is now the one place a verb is declared, so that
    // drift cannot recur. See `help::render_top`.
    // `false` — NOT a claim that setup is finished, but the statement that this
    // string is built on every invocation and therefore may not touch the disk.
    // The surfaces that actually print the top-level help (bare `qd`, `qd
    // --help`, `qd --help-all`) probe and pass the real answer; see
    // `help::render_top`.
    let top = help::render_top(&cmd, false, false);
    cmd.override_help(top)
}

/// Every verb registration + aliases (spec §3 + qb spec-cli §11). The default
/// action (bare `qd` → ls) is handled in `verbs::dispatch` when no subcommand
/// matched.
///
/// FTUE punch R14 — the ONE hide site. The human CLI is the verbs named in
/// `help::HUMAN_VERBS` (the four session verbs plus `setup`);
/// everything else is `.hide(true)` HERE, in one pass over the list, rather
/// than sprinkled across thirty builder functions where it would be one more
/// thing to forget. `.hide(true)` is a HELP-ONLY property in clap: it drops the
/// row from the rendered table and changes nothing about parsing, aliases,
/// conflicts, or dispatch. So the hidden verbs are hidden-but-WORKING (the C1
/// resolution): `qd send:relay …` behaves exactly as before, and
/// `qd --help-all` prints the whole surface.
fn subcommands() -> Vec<Command> {
    let human_facing = |name: &str| help::HUMAN_VERBS.contains(&name);
    let registrations = vec![
        cmd_ls(),
        cmd_attach(),
        cmd_connect(),
        cmd_resume(),
        cmd_wrap(),
        cmd_adopt(),
        cmd_start(),
        cmd_stop(),
        cmd_kill(),
        cmd_new(),
        cmd_reconcile(),
        cmd_send(),
        cmd_send_pty(),
        cmd_send_relay(),
        cmd_send_http(),
        cmd_relay(),
        cmd_whoami(),
        cmd_dispositions(),
        cmd_wait(),
        cmd_live(),
        cmd_info(),
        cmd_gc(),
        cmd_init(),
        cmd_setup(),
        cmd_bootstrap(),
        cmd_update(),
        cmd_ping(),
        cmd_mark(),
        cmd_delivery_recover(),
        cmd_config(),
        cmd_survey(),
    ];
    registrations
        .into_iter()
        .map(|c| {
            let hide = !human_facing(c.get_name());
            c.hide(hide)
        })
        .collect()
}

// --- 1. ls (alias list), index.ts:36-44 ---
fn cmd_ls() -> Command {
    Command::new("ls")
        .visible_alias("list")
        .about("List all sessions, on every provider (use --json for scripting)")
        .override_help(help::LS)
        .arg(flag(
            "all",
            'a',
            "Everything: all local sessions (uncapped, incl. killed) + every peer host's mirror, each with its staleness (fleet)",
        ))
        // B5 item 2: liveness-scoped scripting surface. Conflicts with --all at
        // parse — one liveness class per query (the start --alt-screen/--inline
        // render-flag precedent).
        .arg(
            long_flag("live", "Live sessions only (idle/busy/shell), uncapped")
                .conflicts_with("all"),
        )
        // qd–qf W7 — FLEET MIRROR read (READ-ONLY). `--host <h>` reads exactly
        // one peer's `remote/<h>/ls.json` snapshot, ALWAYS annotated with the
        // mirror's staleness (`now − witnessed_at`); an absent mirror ⇒
        // refused{no-fleet-state} exit 12 (consistent with `qd send --host`).
        // Conflicts with `--all` (a single host vs the whole fleet — one host
        // scope per query, the same render-flag precedent as --live/--all); the
        // verb ALSO checks the conflict so a programmatic caller cannot bypass it.
        .arg(
            long_val(
                "host",
                "host",
                "Read one peer host's session mirror (fleet)",
            )
            .conflicts_with("all"),
        )
        .arg(long_flag(
            "json",
            "Output as JSON (best for scripting/piping)",
        ))
        // WP-B7 PIECE 1 — the render-surface SELECTOR pair. `qd ls` auto-detects
        // its surface (agent/pipe ⇒ JSON, human/TTY ⇒ table; the I/O-follows-who-
        // drives doctrine). `--json` and `--table` are the two explicit overrides
        // on that one axis, so they CONFLICT. `--table` is the symmetric complement
        // of `--json` (force the human table even for an agent caller), backed by
        // `DriverOverride::Interactive`. It deliberately does NOT conflict with
        // `--short`: `--table --short` is the agent escape hatch to short TEXT
        // (surface=Table, content=short ⇒ a short table), `--short` being a CONTENT
        // modifier, not a surface selector (qd-supervisor-11-ratified).
        .arg(
            long_flag(
                "table",
                "Force the human table (override the JSON auto-default)",
            )
            .conflicts_with("json"),
        )
        .arg(long_flag("short", "Names only, one per line"))
        .arg(long_val(
            "prefix",
            "prefix",
            "Filter sessions by name prefix",
        ))
        .arg(
            Arg::new("limit")
                .short('n')
                .long("limit")
                .value_name("count")
                .help("Max sessions to show (default: 20; --all is uncapped unless -n is given)")
                .action(ArgAction::Set),
        )
}

// --- 2. attach <session> — the human "get me into this session" verb.
// Dispatches on provider hosting then liveness (verbs/attach.rs).
fn cmd_attach() -> Command {
    Command::new("attach")
        .about("Connect to a running agent session's TUI (a cold session is revived first)")
        .override_help(help::ATTACH)
        .arg(positional("session"))
        // Revive a cold session into a PERSISTENT, relay-serving daemon and return
        // 0 WITHOUT attaching a TTY — the headless entry a systemd ExecStart calls.
        .arg(long_flag(
            "no-attach",
            "Revive to a persistent daemon without attaching a TTY (headless)",
        ))
        .args(render_mode_flags())
}

// --- 2b. connect — hidden backward-compat alias for attach. Remains registered
// and hidden; invocations route transparently to attach::run so existing shell
// wrappers that call `qd connect <session>` keep working.
fn cmd_connect() -> Command {
    Command::new("connect")
        .hide(true)
        .about("(renamed — use qd attach)")
        .override_help(help::CONNECT)
        .arg(positional("session"))
        .arg(long_flag(
            "no-attach",
            "Revive to a persistent daemon without attaching a TTY (headless)",
        ))
        .args(render_mode_flags())
}

// --- 3. resume <session>, commands/lifecycle.ts:400-404 ---
fn cmd_resume() -> Command {
    Command::new("resume")
        .about("Resume a cold session to a drivable state (agent-facing)")
        .override_help(help::RESUME)
        .arg(positional("session"))
        // FTUE punch R1 (zmx retirement): `--no-zmx` and `--zmx-name` are GONE.
        // They were parked flags — registered so TS-era scripted callers would
        // not break, read by NOTHING (`verbs/resume.rs` revives through the
        // shared `revive_claude` seam, which derives its own mux name), and
        // documented in `qd resume --help` as if they worked. A flag that is
        // advertised and inert is worse than an absent one, so removing them
        // makes `qd resume --no-zmx` an honest `error: unknown option`.
        // `--no-attach` stays: commander registers it as a negatable boolean and
        // we model it as a plain long flag (the parse surface the corpus checks).
        .arg(long_flag("no-attach", "Start detached (background)"))
        // F3 cwd override (commands/lifecycle.ts:404): `--cwd <dir>` lets a resume
        // relocate when the recorded project dir is gone (the reality-check escape).
        .arg(long_val(
            "cwd",
            "dir",
            "Override the session's recorded working directory",
        ))
        .args(render_mode_flags())
}

// --- 3b. wrap <session> — wrap a live bare Claude session under managed qrmux. ---
fn cmd_wrap() -> Command {
    Command::new("wrap")
        .about("Wrap a live bare Claude session into managed qrmux")
        .override_help(help::WRAP)
        .arg(positional("session"))
        .arg(flag(
            "force",
            'f',
            "Skip only the best-effort external idle heuristic",
        ))
}

// --- 3c. adopt — hidden backward-compat alias for wrap. Remains registered and
// hidden; invocations route transparently to adopt::run (the wrap handler) so
// existing callers of `qd adopt <session>` keep working — the same pattern
// `connect` follows for `attach` above. ---
fn cmd_adopt() -> Command {
    Command::new("adopt")
        .hide(true)
        .about("(renamed — use qd wrap)")
        .override_help(help::ADOPT)
        .arg(positional("session"))
        .arg(flag(
            "force",
            'f',
            "Skip only the best-effort external idle heuristic",
        ))
}

// --- 4. stop <session> (P0 W1, qb spec-cli §11) — today's `kill`, renamed.
// Same backend (verbs/kill.rs dual-reap + verify-gone + tombstone); the clap
// spec is a clone of the old cmd_kill.
fn cmd_stop() -> Command {
    Command::new("stop")
        .about("Stop a session")
        .override_help(help::STOP)
        .arg(positional("session"))
        // W3 (ADD-15): the verb never prompts; the flag stays PARSE-ACCEPTED so
        // existing scripted callers (`qd stop --force ...`) don't break on the
        // most destructive verb. User-visible help is help::STOP above; this
        // desc is kept in sync for consistency. See verbs/kill.rs W3 note.
        .arg(flag("force", 'f', "Deprecated no-op (stop never prompts)"))
        .arg(long_flag("server", "Also kill the OpenCode server process"))
}

// --- 4b. kill — RETIRED erroring stub (P0 W1, qb spec-cli §11). Stays
// REGISTERED + visible so `qd kill ...` reaches the stub's helpful error
// instead of a clap usage error; trailing passthrough swallows any args.
fn cmd_kill() -> Command {
    Command::new("kill")
        .about("(retired — use qd stop)")
        .override_help(help::KILL)
        .arg(trailing_passthrough())
}

// --- 5. start <name> [claudeArgs...] (P0 W1, qb spec-cli §11) — today's
// `new`, renamed. Same backend (lifecycle::run_new); the clap spec is a clone
// of the old cmd_new, including the claudeArgs trailing-var-arg semantics.
fn cmd_start() -> Command {
    Command::new("start")
        .about(help::start_about())
        .override_help(help::START)
        .arg(positional("name"))
        // claudeArgs...: variadic trailing positional. `trailing_var_arg(true)`
        // collects extra NON-flag positionals AND everything after `--` (TS
        // commander `[claudeArgs...]`). Crucially we do NOT set
        // `allow_hyphen_values(true)`: an unregistered `-`/`--`-prefixed token
        // BEFORE `--` then surfaces as UnknownArgument → `error: unknown option
        // '<--x>'` exit 1 (H3 fail-fast, corpus row 39), instead of being
        // swallowed as a claude arg and triggering a real boot/hang. After `--`,
        // clap routes raw values here regardless (the `--model opus` pass-through).
        .arg(
            Arg::new("claudeArgs")
                .value_name("claudeArgs")
                .num_args(0..)
                .trailing_var_arg(true)
                .action(ArgAction::Append),
        )
        .arg(long_val("cwd", "dir", "Working directory for the session"))
        // P0 start-surface rework (STATE 21 ruling): `--resume` is REMOVED from
        // start (it was TS-parity residue, redundant with the resume verb) and
        // `--fork` is now VALUED — `start <name> --fork <session>` starts a NEW
        // participant from an existing session's transcript. A bare `--fork`
        // is a clap missing-value error (commander-mapped exit 1).
        .arg(long_val(
            "fork",
            "session",
            "Fork an existing session's transcript into this new participant",
        ))
        // WP-B5-iii (FORK-IDENTITY-SPEC §4): `--turn N` is REWIND-ONLY — fork at a
        // PAST conversational-turn boundary (1-based). Omitted ⇒ fork at the
        // latest SAFE boundary ("clone me as I am now"). Only meaningful with
        // `--fork`; ignored otherwise (a bare `--turn` is a missing-value error).
        .arg(long_val(
            "turn",
            "ordinal",
            "With --fork: rewind the fork to a past conversational-turn boundary (default: latest safe)",
        ))
        // FTUE punch R19: `--attach` is REPLACED by `--no-attach`, because the
        // default flipped. A human who runs `qd start wk` is looking at the
        // session they just made and then had to type `qd attach wk` at it; start
        // now hands the terminal over itself, so the flag worth having is the
        // opt-OUT, not the opt-in.
        //
        // THE SPELLING IS NOT NEW. `attach` and `resume` both already register
        // `--no-attach` for exactly this meaning — "do the work, do not take over
        // my terminal" — so start joins a convention rather than inventing a third
        // word for it. (`--attach` itself was the A5-deferred flag that only ever
        // answered "not yet supported in the Rust engine", which is the other half
        // of what R19 removes: an advertised inert flag, like `--port` above.)
        //
        // It is an OPT-OUT ONLY, never an opt-in: passing nothing does not force
        // an attach either. The auto-detect decides (TTY + no agent markers +
        // an attachable lane + no `-p`) and this flag can only veto it — see
        // `crate::driver::attaches_after_start` for why an agent caller must
        // never be handed a terminal it has no way to leave.
        .arg(long_flag(
            "no-attach",
            "Start detached — do not attach after the session is created",
        ))
        .arg(long_val(
            "agent",
            "name",
            "Start with a specific agent definition",
        ))
        .arg(
            Arg::new("prompt")
                .short('p')
                .long("prompt")
                .value_name("prompt")
                .help("Send an initial prompt after the session starts")
                .action(ArgAction::Set),
        )
        .arg(long_val(
            "model",
            "model",
            "Set the model before sending the prompt",
        ))
        .arg(long_val(
            "provider",
            "provider",
            "Provider: claude-code (default), codex, pi, acp/claude-code or opencode",
        ))
        // FTUE punch R6: `--port` is GONE FROM THE HELP and STAYS IN THE PARSER,
        // and the split is the whole point. Help advertised it with a real-sounding
        // description ("Port for OpenCode server (default: auto-scan 4096-4106)")
        // for a flag `verbs/lifecycle.rs` refuses unconditionally — an advertised
        // inert flag, the same class of lie R1 removed with `resume --no-zmx`.
        //
        // R1 DELETED its flags; this one is kept, because the two are not the same
        // shape. `--no-zmx` was read by NOTHING: it parsed, was silently ignored,
        // and the session came up as if it had never been typed — so deleting it
        // turned a silent lie into `error: unknown option`, which is strictly more
        // information. `--port` is not ignored: it produces a loud, accurate,
        // exit-1 refusal that NAMES the park ("not yet supported in the Rust
        // engine (parked)"). Deleting the registration would replace that sentence
        // with clap's `error: unknown option '--port'`, which tells a TS-era
        // scripted caller strictly LESS. It is also a live park, not dead weight:
        // `--provider opencode` was un-parked and this was deliberately left
        // behind (the legacy opencode-ws port; the acp/opencode residence
        // allocates its own loopback port), so the refusal names a real deferral.
        //
        // `hide(true)` is the honesty fix and the ONLY change: it suppresses the
        // help row and nothing else — the C1 hidden-but-working property — so the
        // parse-then-refuse path is byte-identical and only the advertisement is
        // gone. The description says "parked" for the same reason: no rendering of
        // this arg, anywhere, may describe it as a working feature again.
        .arg(
            long_val(
                "port",
                "port",
                "(parked — refused by the verb; not supported by this engine)",
            )
            .hide(true),
        )
        // `--via <name>` (spec §3.2): route the new session through the named
        // backends.json profile. A6 unhides it (was a hidden no-op) and gives it
        // help text — the flag is now LIVE (resolved in verbs/lifecycle.rs run_new).
        .arg(long_val(
            "via",
            "name",
            "Route through a backends.json profile (per-session backend)",
        ))
        // WP-B-CS-1 (D2): driver-mode overrides for the I/O-follows-who-drives
        // auto-detect. Absent ⇒ the context auto-detect runs (TTY + agent env
        // markers); set ⇒ the override always wins. `--headless` forces the
        // headless stream-json launch (agent surface); `--interactive` forces the
        // native-TUI create path (human surface). Mutually exclusive at parse.
        .arg(
            long_flag(
                "headless",
                "Force a headless stream-json launch (override the driver auto-detect)",
            )
            .conflicts_with("interactive"),
        )
        // codex-interactive / pi-interactive: for `--provider codex` and
        // `--provider pi` this flag does not merely override the auto-detect
        // (which never routed either) — it selects a different TOPOLOGY: that
        // harness's PLAIN TUI in an attachable mux pane. It used to be described
        // as "instead of its daemon", which stopped being true when the defaults
        // moved: it is now instead of `codex/app-server` and instead of
        // `pi/extension`, and for pi the difference is specifically the absence
        // of the control channel — the same pane, minus the socket.
        .arg(long_flag(
            "interactive",
            "Force the interactive native-TUI launch (override the driver auto-detect). \
             With --provider codex or pi, runs that harness's plain TUI in an attachable \
             pane (no control channel)",
        ))
        // pi/extension: pi's alone. Like --interactive it runs pi's own TUI in an
        // attachable pane; unlike --interactive the pane also carries a control
        // channel, so the same session can be driven by `qd send` while a human
        // types into it. Conflicts with --interactive because the two name
        // different lanes and silently preferring one would be the create-routing
        // bug `Lane::for_create` exists to prevent.
        //
        // It now names pi's DEFAULT lane, so passing it is redundant — and it is
        // kept anyway, deliberately (16-default-lane-switch.md §5): existing
        // scripts pass it, it costs nothing, and "the default" and "this lane"
        // are two different requests even when they currently agree.
        .arg(
            long_flag(
                "extension",
                "pi only (and pi's default lane): run pi's TUI in an attachable pane WITH \
                 the quorum control channel, so `qd send` drives the same session a human \
                 is typing into",
            )
            .conflicts_with("interactive")
            .conflicts_with("headless"),
        )
        // `--daemon`: the ESCAPE HATCH, and the reason the default flip did not
        // delete two lanes (16-default-lane-switch.md, DEC-2/DEC-4). A bare
        // `qd start --provider codex|pi` used to mean "the headless resident";
        // it now means `codex/app-server` / `pi/extension`, and this flag is the
        // ONLY spelling left for `codex/daemon` and `pi/daemon` — the lanes CI,
        // a bare ssh session and any no-mux context need, because neither
        // default can be built without a mux pane (pi) or wants one.
        //
        // Conflicts with --interactive and --extension for the same reason those
        // two conflict with each other: each names a different lane, and
        // silently preferring one would be the create-routing bug
        // `Lane::for_create` exists to prevent. It does NOT conflict with
        // --headless, which is claude's driver selector and inert on the two
        // harnesses this flag is for.
        .arg(
            long_flag(
                "daemon",
                "codex/pi only: run the headless daemon rather than the default lane \
                 (codex/app-server, pi/extension) — no mux pane, no TTY, for CI and ssh",
            )
            .conflicts_with("interactive")
            .conflicts_with("extension")
            .conflicts_with("app-server"),
        )
        // `--app-server`: codex's default, nameable. It exists for the same
        // reason `--extension` survives now that it names pi's default (§5 of
        // 16-default-lane-switch.md): "the default is app-server" and
        // "app-server is requestable by name" are two different assertions, and
        // a script that means the second should not have to rely on the first.
        // The defaults just moved once; a caller that pins the lane explicitly
        // is unaffected the next time they move.
        //
        // Without it `CreateTopology::AppServer` has no producer at all —
        // `Default` resolves to app-server for codex through
        // `create_default_mode`, and the wire's `lane` field does not route
        // through `for_create` — so the variant would be a documented no-op,
        // which is exactly the dead-arm shape this enum exists to avoid.
        .arg(
            long_flag(
                "app-server",
                "codex only: run the app-server lane explicitly (the default) — a \
                 headless resident a human can still \"qd attach\" a viewer onto",
            )
            .conflicts_with("interactive")
            .conflicts_with("extension")
            .conflicts_with("daemon"),
        )
        // Lifecycle-collapse A-1 (spec D4): machine-readable identity output.
        // Exit 0 with --json guarantees the printed id is BOUND (A-2); on a
        // bind-arm failure a machine-readable error object rides stdout.
        .arg(long_flag(
            "json",
            "Emit the started session's identity as JSON on stdout \
             ({name, qdId, sessionId, status, live})",
        ))
        // Lifecycle-collapse A-3 (spec D5, Pete's ruling): the relay-sidecar
        // readiness wait is DEFAULT-ON for start; this is the opt-out for
        // callers that want raw boot speed (and for boots with no relay, e.g.
        // fakerepl-backed test lanes).
        .arg(long_flag(
            "no-await-relay",
            "Skip the default relay-readiness wait (exit 0 then means idle, \
             not relay-reachable)",
        ))
        .args(render_mode_flags())
}

/// punch item 7: the shared `--alt-screen` / `--inline` render-mode flag pair,
/// attached to EVERY launch verb (start / resume / attach). Inline is the
/// fleet default (sessions render in the scrollback so phone/SSH attach can
/// scroll); `--alt-screen` restores fullscreen rendering for THIS session.
/// `--inline` is the mirror (forces inline when `render-default = alt-screen`
/// flipped the machine default). Mutually exclusive at parse.
fn render_mode_flags() -> [Arg; 2] {
    [
        long_flag(
            "alt-screen",
            "Fullscreen (alt-screen) rendering for this session (default: inline, \
             so phone/SSH attach can scroll)",
        )
        .conflicts_with("inline"),
        long_flag(
            "inline",
            "Force inline (scrollback) rendering for this session (overrides \
             `render-default = alt-screen`)",
        ),
    ]
}

// --- 5b. new — RETIRED erroring stub (P0 W1, qb spec-cli §11). Stays
// REGISTERED + visible so `qd new ...` reaches the stub's helpful error
// instead of a clap usage error; trailing passthrough swallows any args.
fn cmd_new() -> Command {
    Command::new("new")
        .about("(retired — use qd start)")
        .override_help(help::NEW)
        .arg(trailing_passthrough())
}

// --- 6. reconcile, commands/lifecycle.ts:813-816 ---
fn cmd_reconcile() -> Command {
    Command::new("reconcile")
        .about("Detect and repair drift across registry / mux / process (idempotent)")
        .override_help(help::RECONCILE)
        .arg(long_flag(
            "dry-run",
            "Show what would be repaired without changing anything",
        ))
}

// --- 7. send <session> <message> — unified primary send surface ---
fn cmd_send() -> Command {
    Command::new("send")
        .about("Send a message to a session (delivery path selected automatically)")
        .override_help(help::SEND)
        // qd–qf W4: `<session>`/`<message>` are ORIGIN-mode positionals. They are
        // clap-OPTIONAL (not `required(true)`) because the INBOUND mode
        // (`--inbound-envelope`) carries the address + body inside the envelope and
        // takes NO positionals. The two modes are validated at RUNTIME
        // (`run_send_unified`): origin requires both positionals + forbids
        // `--inbound-envelope`; inbound requires `--inbound-envelope` + forbids
        // positionals. Origin-mode parsing is byte-identical to W3b when
        // `--inbound-envelope` is absent — the runtime check re-imposes the
        // "requires `<target> <message>`" contract with the SAME missing-arg error.
        .arg(Arg::new("session").value_name("session"))
        // qd–qf W3: the write-then-deliver expiry policy travels with the message
        // (format doc §1 `expires_at`). Absent ⇒ the 12h default; a value is
        // `<int>` (bare = seconds) or `<int>{s|m|h|d}`. A bad form is a SYNC arg
        // refusal (see origin_send::parse_expires). Declared BEFORE the message
        // positional so `--expires` binds as an option, not swallowed as payload.
        // ORIGIN mode only (an inbound envelope carries its own `expires_at`).
        .arg(long_val(
            "expires",
            "dur",
            "How long this send stays deliverable before it expires (e.g. 12h, 30m, 45s, 1d; bare integer = seconds; default 12h)",
        ))
        // qd–qf W6 — ADDRESSING: `--host <host>` is the flag form of the `name@host`
        // sugar (TRANSITION §3 / §7 Q2 RULED — the sugar desugars to this flag; both
        // exist). The effective host = --host ∨ the address's @host ∨ None(local);
        // if BOTH are present and DIFFER it is a sync `refused{host}`. Bare = this
        // host. A host-qualified address for a host with no fleet state on this box
        // is a named refusal (single-machine contract). ORIGIN mode only.
        .arg(long_val(
            "host",
            "host",
            "Host qualifier for the target (the flag form of name@host; the effective host is --host or the address's @host, which must agree). Bare = this host.",
        ))
        // qd–qf W4 — INBOUND mode ("THE ONE DOOR"): admit a peer's already-minted
        // envelope (JSON) at the door. `<path>` is a file, or `-` for stdin. When
        // present, the `<session>`/`<message>` positionals are NOT used (the
        // envelope carries target + body + its own correlation_id/authored_at/
        // expires_at/authority). qd validates → idempotency-checks → delivers →
        // stamps the disposition (it does NOT append to its own log.jsonl).
        .arg(long_val(
            "inbound-envelope",
            "path",
            "INBOUND mode: admit a peer's already-minted envelope (JSON) from <path>, or `-` for stdin. Mutually exclusive with <target> <message>.",
        ))
        // qd–qf W3c (provider-contract §4): the caller-supplied correlation_id. When
        // frame ORIGINATES a send its ledger event id rides through this flag as the
        // envelope's `correlation_id` (the frame↔qd origin seam); the log envelope
        // AND the stamped disposition then key on that same id. Absent ⇒ qd mints its
        // own ULID (the BARE-send default, unchanged). ORIGIN mode only — an inbound
        // envelope already carries its own origin-minted id, so `--correlation-id` +
        // `--inbound-envelope` is a sync refused{args} (like `--expires` + inbound).
        // Declared BEFORE the message positional so it binds as an option, not
        // swallowed as payload.
        .arg(long_val(
            "correlation-id",
            "id",
            "ORIGIN mode: use this caller-supplied id as the envelope's correlation_id (frame passes its ledger event id here). Default: qd mints a ULID.",
        ))
        // A caller's message is opaque payload, including values such as
        // `--literal`; unified send has no transport options to reinterpret it.
        .arg(Arg::new("message").value_name("message").allow_hyphen_values(true))
}

// --- 8. send:pty <session> <message>, commands/send.ts:52-58 ---
fn cmd_send_pty() -> Command {
    Command::new("send:pty")
        .about("(compatibility/debug) Force a PTY send (types into the session pane)")
        .override_help(help::SEND_PTY)
        .arg(positional("session"))
        .arg(positional("message"))
        .arg(long_val_default(
            "timeout",
            "seconds",
            "Max wait time",
            "120",
        ))
        .arg(long_flag(
            "full",
            "Include all blocks (thinking, tool calls)",
        ))
        .arg(long_flag("raw", "Print raw JSONL lines"))
        .arg(long_flag("wait", "Block and wait for the response"))
}

// --- 9. send:relay <session> <message>, commands/send.ts:237-241 ---
fn cmd_send_relay() -> Command {
    Command::new("send:relay")
        .about("(compatibility/debug) Force relay/daemon send routing")
        .override_help(help::SEND_RELAY)
        .arg(positional("session"))
        .arg(positional("message"))
        .arg(long_val_default(
            "timeout",
            "seconds",
            "Max wait for reply",
            "120",
        ))
        .arg(long_flag(
            "wait",
            "Block and wait for the reply instead of returning immediately",
        ))
}

// --- 10. send:http <session> <message>, commands/send.ts:366-370 ---
fn cmd_send_http() -> Command {
    Command::new("send:http")
        .about("(compatibility/debug) Force the OpenCode HTTP path")
        .override_help(help::SEND_HTTP)
        .arg(positional("session"))
        .arg(positional("message"))
        .arg(long_val_default(
            "mode",
            "mode",
            "Message envelope: report, execute, or raw",
            "report",
        ))
        .arg(long_val_default(
            "timeout",
            "seconds",
            "Max wait time",
            "300",
        ))
}

// --- 11. relay (moved stub), commands/send.ts:519-522 ---
fn cmd_relay() -> Command {
    Command::new("relay")
        .about("(moved) Use send:relay instead")
        .override_help(help::RELAY)
        .arg(trailing_passthrough())
}

// --- 12. whoami (alias name), commands/status.ts:190-194 ---
fn cmd_whoami() -> Command {
    Command::new("whoami")
        .visible_alias("name")
        .about("Print the current session's name and ID")
        .override_help(help::WHOAMI)
        .arg(long_flag("json", "Output as JSON"))
}

// --- 12b. dispositions [<correlation_id>] — qd–qf transition W5 (read verb) ---
// A stateless, caller-windowed JSONL view over log.jsonl ∪ dispositions.jsonl
// (format doc §3), for piping into DuckDB: the folded per-id summary by default
// (§3a), or the raw witnessed-event funnel with --events (§3b). New surface
// (not a TS port), so clap auto-generates its --help.
fn cmd_dispositions() -> Command {
    Command::new("dispositions")
        .about("Emit the per-correlation disposition summary as JSONL (default; --events emits the raw witnessed-event rows)")
        // Optional positional point query: just this correlation_id.
        .arg(Arg::new("correlation_id").value_name("correlation_id").required(false))
        // §3b: the raw event rows (the funnel) instead of the folded summary.
        .arg(long_flag(
            "events",
            "Emit the raw witnessed-event rows (the funnel) instead of the per-id summary",
        ))
        // Caller-windowed lower bound within the last <dur>. The summary windows
        // on the envelope's authored_at (null-timeline orphans always kept);
        // --events windows on each event's created_at (R14.2). Same grammar as
        // `qd send --expires` (bare int = seconds, else <int>{s|m|h|d}). STATELESS
        // — qd stores no read cursor (N2).
        .arg(long_val(
            "window",
            "dur",
            "Only rows within the last <dur> (summary: envelope authored_at; --events: created_at; e.g. 12h, 30m, 45s, 1d; bare integer = seconds). Stateless/caller-windowed — qd stores no cursor",
        ))
        // Scope: --host unions one peer's remote replica; --all unions every peer.
        // Mutually exclusive (one scope per query).
        .arg(
            long_val(
                "host",
                "host",
                "Union in one peer's replicated dispositions (remote/<host>/)",
            )
            .conflicts_with("all"),
        )
        .arg(long_flag(
            "all",
            "Union in every peer's replicated dispositions (remote/*/)",
        ))
        // Additionally union the local archive tier (*.archive.jsonl).
        .arg(long_flag(
            "archive",
            "Also read the local archive tier (log.archive.jsonl + dispositions.archive.jsonl)",
        ))
}

// --- 13. wait <session>, commands/status.ts:214-217 ---
fn cmd_wait() -> Command {
    Command::new("wait")
        .about("Block until a session transitions from busy to idle")
        .override_help(help::WAIT)
        .arg(positional("session"))
        .arg(long_val_default(
            "timeout",
            "seconds",
            "Max wait time",
            "120",
        ))
}

// --- 14. live, commands/status.ts:394-397 ---
fn cmd_live() -> Command {
    Command::new("live")
        .about("Live-updating session list — type a 3-char code to attach")
        .override_help(help::LIVE)
        .arg(flag("all", 'a', "Include dead sessions"))
}

// --- 15. info <session>, commands/status.ts:560-562 ---
fn cmd_info() -> Command {
    Command::new("info")
        .about("Detailed view of a single session")
        .override_help(help::INFO)
        .arg(positional("session"))
        // P0 spec-w8: one json object (the point-resolution surface an outside
        // consumer joins against). Human output without the flag stays byte-unchanged.
        .arg(long_flag("json", "Output as JSON"))
}

// --- 16. gc, commands/gc.ts:482-487 ---
fn cmd_gc() -> Command {
    Command::new("gc")
        .about("Prune stale sessions and sidecars to recoverable trash")
        .override_help(help::GC)
        .arg(long_flag(
            "dry-run",
            "Show what would be pruned without acting",
        ))
        .arg(long_flag("list-trash", "Show trash contents"))
        .arg(long_val("recover", "item", "Recover an item from trash"))
        .arg(long_flag(
            "purge",
            "Permanently delete trash items older than 30 days",
        ))
}

// --- 16b. init <shell> (NET-NEW, 2026-06-09 ruling) — print the shell
// integration (claude + codex wrappers + mux socket-dir pin) for eval'ing from the rc
// file. The eval-init pattern: the wrapper body ships in the binary so it can
// never drift from what `qd new` accepts (the retired TS bootstrap baked the
// wrapper INTO the rc file, and it fossilized).
fn cmd_init() -> Command {
    Command::new("init")
        .about("Print shell integration (claude + codex wrappers) — add `eval \"$(qd init bash)\"` to your rc file")
        .override_help(help::INIT)
        .arg(positional("shell"))
}

// --- 17. bootstrap, commands/bootstrap.ts:929-931 ---
// Description NOT ported verbatim (A3 spec §3 row 17 ruling, carried into A5 §4.1
// + named divergence §9 item 5): the TS text carries scope-banned tokens AND A5
// redefines engine bootstrap as ENGINE-ONLY — the qb-owned deploy steps are
// dropped; the engine creates ~/.quorum/dispatch + ~/.quorum/dispatch/state + the mux notice + the ADD-5
// relay offer. This one-line engine-only description still matches the now-REAL
// A5 behavior; matrix row = plan-sanctioned parity exclusion (flagged to
// orchestrator). The banned-token list lives in the spec, never in this repo.
fn cmd_setup() -> Command {
    Command::new("setup")
        .about("Integrate your agent harnesses with quorum")
        // Renamed from `--fix`, which survives as a hidden alias so the older
        // spelling in scripts and CI keeps working. Hidden from help on both
        // spellings (help::SETUP documents neither, nor --json/-y): the flag is
        // the non-interactive escape hatch, not the path a human is pointed down.
        .arg(
            long_flag(
                "fix",
                "Apply the fixes for everything detected (non-interactive)",
            )
            .long("auto-apply-changes")
            .alias("fix")
            .hide(true),
        )
        .arg(long_flag("json", "Report the detected setup state as JSON").hide(true))
        .arg(flag("yes", 'y', "Assume yes for every prompt").hide(true))
        .override_help(help::SETUP)
}

fn cmd_bootstrap() -> Command {
    // The relay env seam is an OPERATOR surface with visibility parity (orc-3
    // ruling relay-1780662680745-11 condition c): documented together here, in
    // `bootstrap --help`, and in the bootstrap.rs source.
    // (QRM_RELAY_DRIVER_INSTALL is GONE with the external bun driver, 2026-06-09
    // ruling: the relay is native — bootstrap registers `qd relay:serve` in
    // ~/.claude/.mcp.json itself, with consent.)
    Command::new("bootstrap")
        .about("Set up qd's local data directory under ~/.quorum/dispatch (idempotent)")
        .after_help(
            "Environment:\n  \
             QRM_RELAY_DISABLE_SCAN    Set to 1/true to skip the localhost relay \
             port-scan (sidecar-file discovery still runs).",
        )
}

// --- 18. update, commands/update.ts:88-92 ---
// Description NOT ported verbatim (A5 §4.3 + named divergence §9 item 3): the TS
// `bun install -g` text is FALSE for the Rust engine, which self-updates via the
// detected install channel (Homebrew or cargo). Engine-truthful one-liner;
// A3-row-17-class sanctioned exclusion.
fn cmd_update() -> Command {
    Command::new("update")
        .about("Self-update qd via the detected install channel (Homebrew or cargo).")
        .override_help(help::UPDATE)
}

// --- 19. ping [session], commands/ping.ts:279-284 ---
fn cmd_ping() -> Command {
    Command::new("ping")
        .about(
            "Classify session liveness (drop-in for the legacy monitor.sh): \
             exit 0=done 1=stuck 2=active 3=error 4=ambiguous. \
             Use --prefix to sweep all sessions by name prefix.",
        )
        .override_help(help::PING)
        // [session] is OPTIONAL (commands/ping.ts:279).
        .arg(Arg::new("session").value_name("session").required(false))
        .arg(long_val(
            "prefix",
            "prefix",
            "Sweep all sessions whose name starts with <prefix>",
        ))
        .arg(long_flag("json", "Output as JSON"))
}

// --- 22. mark <session> <payload> (NET-NEW, spec §4) ---
fn cmd_mark() -> Command {
    Command::new("mark")
        .about("Append an opaque mark to the session's mark stream")
        .arg(positional("session"))
        .arg(positional("payload"))
}

// --- 23. delivery:recover [--send-id X] (NET-NEW, delivery contract §C2, D1) ---
// One-shot, dispatch-only recovery of DEAD-DANGLING pty/new-p sends: append a
// terminal for every initiated send whose writer incarnation is gone. Enforces the
// is_dead_dangling liveness fence — a still-LIVE send is never touched. Minimal
// scope: the only option narrows the sweep to one send_id; no scheduling/residency
// (that is the deferred recovery_coordinator).
fn cmd_delivery_recover() -> Command {
    Command::new("delivery:recover")
        .about(
            "Recover dead-dangling sends: append a terminal for each initiated send \
             whose writer is gone (a still-live send is left untouched). One-shot.",
        )
        .arg(long_val(
            "send-id",
            "send_id",
            "Recover only this send_id (default: sweep all dead-dangling sends)",
        ))
}

// --- 24/25. config + survey — HAND-PARSED, dispatched pre-clap in main.rs ---
// These two bypass clap entirely (TS `allowUnknownOption` + `allowExcessArguments`
// + `helpOption(false)`; `main::run` intercepts them before `build_cli` is even
// consulted, so their exit conventions survive). They are registered here ANYWAY,
// hidden and arg-swallowing, for exactly one reason: R4 generates the command
// table by walking `subcommands()`, so a verb that is not declared here cannot
// appear in `qd --help-all`. `config` is how a human stores their OpenRouter key
// — it has to be findable. The registration is inert: nothing can reach clap
// dispatch for these names.
fn cmd_config() -> Command {
    Command::new("config")
        .about(
            "Manage stored secrets (e.g. `qd config set openrouter-key`). Tiered backend: \
             macOS Keychain when available, else a chmod-600 ~/.quorum/dispatch/config.toml. \
             Env var overrides.",
        )
        .arg(trailing_passthrough())
}

fn cmd_survey() -> Command {
    Command::new("survey")
        .about(
            "Fan an artifact out to a panel of LLMs via OpenRouter and collect responses \
             (the panel-review / panel-ideate mechanic). Requires OPENROUTER_API_KEY.",
        )
        .arg(trailing_passthrough())
}

// --- arg builder helpers ---

/// A required positional argument.
fn positional(name: &'static str) -> Arg {
    Arg::new(name).value_name(name).required(true)
}

/// A boolean long flag (`--name`).
fn long_flag(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .help(help)
        .action(ArgAction::SetTrue)
}

/// A boolean flag with a short + long form (`-x, --name`).
fn flag(name: &'static str, short: char, help: &'static str) -> Arg {
    Arg::new(name)
        .short(short)
        .long(name)
        .help(help)
        .action(ArgAction::SetTrue)
}

/// A value-taking long option (`--name <value>`).
fn long_val(name: &'static str, value_name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .value_name(value_name)
        .help(help)
        .action(ArgAction::Set)
}

/// A value-taking long option with a default (commander `.option(.., default)`).
fn long_val_default(
    name: &'static str,
    value_name: &'static str,
    help: &'static str,
    default: &'static str,
) -> Arg {
    long_val(name, value_name, help).default_value(default)
}

/// A trailing pass-through bucket for the moved stubs (commander
/// `allowUnknownOption`): swallow any args/options so the stub's pointer text is
/// reached regardless of what was typed.
fn trailing_passthrough() -> Arg {
    Arg::new("rest")
        // The value name is what the generated help table renders (`[args...]`),
        // so it names what a caller types, not the bucket it lands in.
        .value_name("args")
        .num_args(0..)
        .trailing_var_arg(true)
        .allow_hyphen_values(true)
        .action(ArgAction::Append)
}

// --- error mapping (spec §2, centralized) ---

/// Map a clap parse `Error` to commander phrasing + the right exit code.
///
/// - `--help`/`--version` (clap "errors" that carry rendered text) → print to
///   stdout, exit **0**.
/// - Every genuine parse error → `commander_message` to stderr, exit **1** (NOT
///   clap's default 2).
///
/// The exact strings live in ONE place ([`commander_message`]); the M3 corpus
/// refines them with a single-site edit.
///
/// Map a clap parse error, given the full argv (the unknown-verb arm names the
/// token the user actually typed, so it needs argv when clap's context is empty).
pub fn map_clap_error_with_argv(e: clap::Error, argv: &[String]) -> i32 {
    match e.kind() {
        // commander's `.version("0.1.0")` prints JUST the version string (TS
        // `qd --version` → "0.1.0"); clap would prepend the bin name ("qd 0.1.0").
        // We keep that shape — no bin-name prefix — and append the build sha
        // (`version_line`), so the answer to "which build is this?" is in the
        // one place people already look. Exit 0.
        ErrorKind::DisplayVersion => {
            println!("{}", version_line());
            0
        }
        ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            print!("{e}");
            0
        }
        ErrorKind::InvalidSubcommand => {
            // FTUE punch R7 — a mistyped verb SAYS SO. This arm used to re-render
            // the typo as commander's `error: too many arguments. Expected 0
            // arguments but got N.`, on the TS-parity reasoning that there is no
            // "unknown command" in TS qd: a bad first token was an excess operand
            // to the default `ls` action, so an argument-COUNT error was the
            // faithful port. R7 overrode that corpus phrasing deliberately —
            // faithful or not, telling someone who typed `qd lss` that they
            // passed too many arguments describes qd's internals instead of their
            // mistake, and gives them nothing to do next. Exit 1 is unchanged.
            eprintln!("error: unknown command '{}'", unknown_verb_token(&e, argv));
            eprintln!("Run `qd --help` for the list of commands.");
            1
        }
        _ => {
            eprintln!("{}", commander_message(&e));
            1
        }
    }
}

/// Render commander-style `error: <thing>` for a clap parse error. ONE match —
/// the M3 corpus tunes exact strings here. We pull the offending token out of
/// clap's structured context where available, falling back to a generic message.
fn commander_message(e: &clap::Error) -> String {
    use clap::error::ContextKind;

    let ctx = |k: ContextKind| e.get(k).map(|v| v.to_string());

    match e.kind() {
        // NB: an unknown top-level verb never reaches here — R7 owns it in
        // `map_clap_error_with_argv`, which has the argv it needs to name the
        // token.

        // Unknown option (commander: "error: unknown option '<--x>'").
        ErrorKind::UnknownArgument => {
            let arg = ctx(ContextKind::InvalidArg).unwrap_or_default();
            // clap may include a value suffix ("--x <y>"); take the option token.
            let opt = arg.split_whitespace().next().unwrap_or(&arg);
            format!("error: unknown option '{opt}'")
        }
        // Missing required argument: commander names the BARE token, e.g.
        // "error: missing required argument 'session'" (corpus 5-*).
        ErrorKind::MissingRequiredArgument => {
            let arg = first_arg_token(&ctx(ContextKind::InvalidArg));
            format!("error: missing required argument '{arg}'")
        }
        // Too many positional args. Commander phrasing carries an expected/got
        // count; the per-verb count is refined by the M3 corpus.
        ErrorKind::TooManyValues => "error: too many arguments".to_string(),
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            let arg = ctx(ContextKind::InvalidArg).unwrap_or_default();
            format!("error: invalid value for '{arg}'")
        }
        // Any other kind: fall back to a generic commander-style line (refined by
        // the M3 corpus).
        _ => {
            // Strip clap's own "error: " prefix if present to avoid doubling.
            let raw = e.to_string();
            let first = raw.lines().next().unwrap_or("").trim();
            let stripped = first.strip_prefix("error: ").unwrap_or(first);
            format!("error: {stripped}")
        }
    }
}

/// The token a user typed where a verb belonged. clap normally hands it over in
/// the error context; if it ever does not, fall back to the first non-option
/// token in argv (`argv[0]` is the program name and is skipped) — which is the
/// same token clap rejected, because an unknown SUBCOMMAND error can only be
/// raised by the first operand.
fn unknown_verb_token(e: &clap::Error, argv: &[String]) -> String {
    e.get(clap::error::ContextKind::InvalidSubcommand)
        .map(|v| v.to_string())
        .or_else(|| {
            argv.iter()
                .skip(1)
                .find(|a| !a.starts_with('-'))
                .cloned()
        })
        .unwrap_or_default()
}

/// clap renders a missing-required context value as e.g. `<session>` or a list;
/// commander names the bare token. Take the first whitespace/`<>`-stripped token.
fn first_arg_token(ctx: &Option<String>) -> String {
    let s = ctx.clone().unwrap_or_default();
    let first = s.split_whitespace().next().unwrap_or("");
    first
        .trim_matches(|c| c == '<' || c == '>' || c == '.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
        let mut argv = vec!["qd"];
        argv.extend_from_slice(args);
        build_cli().try_get_matches_from(argv)
    }

    // --- args parse + defaults ---

    #[test]
    fn ls_flags_parse() {
        let m = parse(&["ls", "-a", "--json", "--short", "--prefix", "wk", "-n", "5"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert!(sm.get_flag("all"));
        assert!(sm.get_flag("json"));
        assert!(sm.get_flag("short"));
        assert_eq!(
            sm.get_one::<String>("prefix").map(String::as_str),
            Some("wk")
        );
        assert_eq!(sm.get_one::<String>("limit").map(String::as_str), Some("5"));
    }

    /// B5 item 2: `--live` parses and composes with the scripting flags; absent
    /// → false (the default view is untouched).
    #[test]
    fn ls_live_flag_parses_and_composes() {
        let m = parse(&[
            "ls", "--live", "--json", "--short", "--prefix", "wk", "-n", "5",
        ])
        .unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert!(sm.get_flag("live"));
        assert!(sm.get_flag("json"));
        assert!(sm.get_flag("short"));
        assert!(!sm.get_flag("all"));
        let m = parse(&["ls"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert!(!sm.get_flag("live"));
    }

    /// B5 item 2 DECLARED RULE: `--live` + `--all` REJECTS AT PARSE (clap
    /// conflicts_with) — a session list is one liveness class per query,
    /// mirroring the start `--alt-screen`/`--inline` render-flag precedent.
    #[test]
    fn ls_live_conflicts_with_all_at_parse() {
        for argv in [vec!["ls", "--live", "--all"], vec!["ls", "-a", "--live"]] {
            let e = parse(&argv).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::ArgumentConflict, "{argv:?}");
        }
    }

    /// WP-B7 PIECE 1: `--table` is the explicit human-table surface selector.
    /// It CONFLICTS with `--json` (the two overrides on the one surface axis) but
    /// COMPOSES with `--short` (`--table --short` is the agent short-text escape
    /// hatch: surface=Table, content=short). Absent ⇒ false (auto-detect runs).
    #[test]
    fn ls_table_flag_parses_conflicts_json_composes_short() {
        // composes with --short (the escape hatch).
        let m = parse(&["ls", "--table", "--short"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert!(sm.get_flag("table"));
        assert!(sm.get_flag("short"));
        assert!(!sm.get_flag("json"));
        // absent ⇒ false.
        let m = parse(&["ls"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert!(!sm.get_flag("table"));
        // conflicts with --json at parse (both orderings).
        for argv in [
            vec!["ls", "--table", "--json"],
            vec!["ls", "--json", "--table"],
        ] {
            let e = parse(&argv).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::ArgumentConflict, "{argv:?}");
        }
    }

    #[test]
    fn ls_alias_list_parses() {
        let m = parse(&["list"]).unwrap();
        // visible_alias maps `list` to the `ls` subcommand.
        assert_eq!(m.subcommand_name(), Some("ls"));
    }

    #[test]
    fn whoami_alias_name_parses() {
        let m = parse(&["name", "--json"]).unwrap();
        assert_eq!(m.subcommand_name(), Some("whoami"));
        let (_, sm) = m.subcommand().unwrap();
        assert!(sm.get_flag("json"));
    }

    #[test]
    fn info_json_flag_parses() {
        // P0 spec-w8: `--json` accepted; absent → false (human path).
        let m = parse(&["info", "wk", "--json"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(
            sm.get_one::<String>("session").map(String::as_str),
            Some("wk")
        );
        assert!(sm.get_flag("json"));
        let m = parse(&["info", "wk"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert!(!sm.get_flag("json"));
    }

    #[test]
    fn unified_send_requires_target_and_preserves_opaque_message() {
        for message in [
            "",
            "--option-like",
            "multiline\nsecond line",
            "multibyte: 🧭 café",
            "$(shell) `ticks` ; & | ' \" $HOME",
        ] {
            let m = parse(&["send", "sess", message]).unwrap();
            let (_, sm) = m.subcommand().unwrap();
            assert_eq!(
                sm.get_one::<String>("session").map(String::as_str),
                Some("sess")
            );
            assert_eq!(
                sm.get_one::<String>("message").map(String::as_str),
                Some(message)
            );
            // ORIGIN mode carries no inbound envelope.
            assert_eq!(sm.get_one::<String>("inbound-envelope"), None);
        }

        // qd–qf W4: `<session>`/`<message>` are now clap-OPTIONAL so INBOUND mode
        // can omit them; the "origin requires both" contract is re-imposed at
        // RUNTIME (run_send_unified) with the SAME missing-arg refusal (bin-tested
        // in inbound_door.rs). So clap ACCEPTS the bare forms here — the mode
        // split, not clap, decides.
        for argv in [vec!["send"], vec!["send", "sess"]] {
            assert!(
                parse(&argv).is_ok(),
                "{argv:?} parses (runtime enforces origin-mode requiredness)"
            );
        }
    }

    #[test]
    fn unified_send_inbound_envelope_parses_from_path_and_stdin_sentinel() {
        // INBOUND mode: `--inbound-envelope <path>` binds as an option; `-` is the
        // stdin sentinel (a legal value). The positionals may be absent.
        for value in ["/tmp/env.json", "-"] {
            let m = parse(&["send", "--inbound-envelope", value]).unwrap();
            let (_, sm) = m.subcommand().unwrap();
            assert_eq!(
                sm.get_one::<String>("inbound-envelope").map(String::as_str),
                Some(value)
            );
            assert_eq!(sm.get_one::<String>("session"), None);
            assert_eq!(sm.get_one::<String>("message"), None);
        }
    }

    #[test]
    fn unified_help_is_transport_neutral_and_escape_hatches_are_labeled() {
        assert!(help::SEND.contains("Usage: qd send <target> <message>"));
        assert!(help::SEND.contains("selects its registered\nreceive path automatically"));
        for carrier in ["send:pty", "send:relay", "send:http"] {
            assert!(
                !help::SEND.contains(carrier),
                "primary help must not teach carrier menu item {carrier}"
            );
        }
        assert!(help::SEND_PTY.contains("Compatibility/debug control"));
        assert!(help::SEND_RELAY.contains("Compatibility/debug control"));
        assert!(help::SEND_HTTP.contains("Compatibility/debug control"));
        // The carriers are off the human table now (R14) but still described in
        // the generated full surface, with the compat/debug label intact.
        let all = help::render_top(&build_cli(), true, false);
        assert!(all.contains("send [options] [session] [message]"));
        assert!(all.contains("(compatibility/debug) Force a PTY send"));
    }

    #[test]
    fn send_pty_timeout_defaults_to_120() {
        let m = parse(&["send:pty", "sess", "hi"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(
            sm.get_one::<String>("timeout").map(String::as_str),
            Some("120")
        );
    }

    #[test]
    fn send_http_defaults_mode_report_timeout_300() {
        let m = parse(&["send:http", "sess", "hi"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(
            sm.get_one::<String>("mode").map(String::as_str),
            Some("report")
        );
        assert_eq!(
            sm.get_one::<String>("timeout").map(String::as_str),
            Some("300")
        );
    }

    #[test]
    fn wait_timeout_defaults_120() {
        let m = parse(&["wait", "sess"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(
            sm.get_one::<String>("timeout").map(String::as_str),
            Some("120")
        );
    }

    #[test]
    fn start_accepts_deferred_options_at_parse() {
        // -p/--model/--provider/--port are PARSE-accepted (the honest deferral
        // error is emitted by the backend AFTER parse, spec §3 row 5).
        //
        // R19 removed `--attach` from this row: it was the OTHER parse-then-refuse
        // flag here, and it no longer exists in either half (the default attaches,
        // `--no-attach` opts out — see `start_no_attach_replaces_attach`).
        let m = parse(&[
            "start",
            "wk",
            "-p",
            "hello",
            "--model",
            "opus",
            "--provider",
            "opencode",
            "--port",
            "4096",
        ])
        .unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(sm.get_one::<String>("name").map(String::as_str), Some("wk"));
        assert_eq!(
            sm.get_one::<String>("prompt").map(String::as_str),
            Some("hello")
        );
        assert_eq!(
            sm.get_one::<String>("model").map(String::as_str),
            Some("opus")
        );
        assert_eq!(
            sm.get_one::<String>("port").map(String::as_str),
            Some("4096")
        );
    }

    /// FTUE punch **R6**: `--port` is gone from the ADVERTISED surface and still
    /// in the PARSER, and the two halves are asserted together because either one
    /// alone is the bug. Help that lists it is the doc-drift R6 removes; a parser
    /// that drops it replaces a refusal naming the park with `error: unknown
    /// option`, which tells a TS-era scripted caller strictly less.
    #[test]
    fn start_port_is_hidden_but_still_parses() {
        // Not advertised — not in the verbatim help the verb prints, and not in
        // clap's own rendering either (the arg is `hide(true)`).
        assert!(
            !help::START.contains("--port"),
            "R6: --port must not be advertised in `qd start --help`:\n{}",
            help::START
        );
        let mut start_cmd = build_cli()
            .find_subcommand("start")
            .cloned()
            .expect("start exists");
        let rendered = start_cmd.render_long_help().to_string();
        assert!(!rendered.contains("--port"), "hidden in clap help too:\n{rendered}");
        // Still parses, so `verbs/lifecycle.rs` can print the parked refusal.
        let m = parse(&["start", "wk", "--port", "4096"]).expect("--port still parses");
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(sm.get_one::<String>("port").map(String::as_str), Some("4096"));
        // And nothing anywhere still describes it as a working feature.
        assert!(
            !rendered.contains("auto-scan"),
            "the OpenCode-server description is gone:\n{rendered}"
        );
    }

    /// FTUE punch **R19**: `--attach` is REPLACED, not merely defaulted. The old
    /// flag was advertised and inert ("not yet supported in the Rust engine");
    /// the opt-out that replaces it is spelled exactly as `attach` and `resume`
    /// already spell theirs.
    #[test]
    fn start_no_attach_replaces_attach() {
        assert!(parse(&["start", "wk", "--no-attach"]).is_ok());
        // The opt-in is an honest unknown option now.
        let e = parse(&["start", "wk", "--attach"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnknownArgument);
        // The help surface agrees with the parser.
        assert!(help::START.contains("--no-attach"), "{}", help::START);
        assert!(
            !help::START.contains("  --attach"),
            "the deferred opt-in is gone from help:\n{}",
            help::START
        );
        // THE SPELLING IS SHARED — that is the point of choosing it. All three
        // verbs that can decline to take over your terminal decline the same way.
        for argv in [
            vec!["attach", "wk", "--no-attach"],
            vec!["resume", "wk", "--no-attach"],
            vec!["start", "wk", "--no-attach"],
        ] {
            assert!(parse(&argv).is_ok(), "{argv:?}");
        }
    }

    #[test]
    fn start_via_parse_accepted() {
        // A6: --via is now a VISIBLE, live flag (was a hidden no-op). Parse still
        // binds the value; the verb resolves it against backends.json.
        let m = parse(&["start", "wk", "--via", "helper-x"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(
            sm.get_one::<String>("via").map(String::as_str),
            Some("helper-x")
        );
    }

    #[test]
    fn start_claude_args_passthrough_after_ddash() {
        let m = parse(&["start", "wk", "--", "--model", "opus"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        let extra: Vec<&String> = sm.get_many::<String>("claudeArgs").unwrap().collect();
        assert_eq!(extra, vec!["--model", "opus"]);
    }

    #[test]
    fn start_unknown_option_before_ddash_is_rejected() {
        // H3 fail-fast (corpus row 39): a bare `--nosuchopt` BEFORE `--` must NOT
        // be swallowed as a claudeArg (which would attempt a real boot → hang).
        // It is an UnknownArgument that maps to `error: unknown option '--nosuchopt'`
        // exit 1 — NOT collected into the post-`--` pass-through bucket.
        let e = parse(&["start", "somename", "--nosuchopt"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnknownArgument);
        assert_eq!(commander_message(&e), "error: unknown option '--nosuchopt'");
    }

    #[test]
    fn start_resume_is_now_an_unknown_option() {
        // P0 start-surface rework (STATE 21 ruling): `--resume` is REMOVED from
        // start (the resume verb owns same-participant wake). It now maps to the
        // existing commander unknown-option shape, exit 1.
        let e = parse(&["start", "wk", "--resume", "some-uuid"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnknownArgument);
        assert_eq!(commander_message(&e), "error: unknown option '--resume'");
    }

    #[test]
    fn start_fork_takes_a_session_value() {
        // `--fork <session>` is VALUED (STATE 21): the value binds…
        let m = parse(&["start", "wk2", "--fork", "wk"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(sm.get_one::<String>("fork").map(String::as_str), Some("wk"));
        // …and a BARE --fork is a clap error (commander-mapped exit 1; the
        // generic fallback arm renders clap's missing-value first line).
        let e = parse(&["start", "wk2", "--fork"]).unwrap_err();
        let msg = commander_message(&e);
        assert!(
            msg.starts_with("error: "),
            "bare --fork maps to a commander-shaped error, got: {msg}"
        );
    }

    #[test]
    fn start_positional_passthrough_before_ddash_still_collected() {
        // A NON-flag extra positional before `--` is still a valid claudeArg
        // (commander variadic), so `start x extra` does not error.
        let m = parse(&["start", "wk", "extra"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        let extra: Vec<&String> = sm
            .get_many::<String>("claudeArgs")
            .map(|v| v.collect())
            .unwrap_or_default();
        assert_eq!(extra, vec!["extra"]);
    }

    /// punch item 7: --alt-screen / --inline parse on EVERY launch verb
    /// (start / resume / attach), default false, and conflict at parse.
    #[test]
    fn render_mode_flags_on_all_launch_verbs() {
        for argv in [
            vec!["start", "wk", "--alt-screen"],
            vec!["resume", "wk", "--alt-screen"],
            vec!["attach", "wk", "--alt-screen"],
        ] {
            let m = parse(&argv).unwrap();
            let (_, sm) = m.subcommand().unwrap();
            assert!(sm.get_flag("alt-screen"), "{argv:?}");
            assert!(!sm.get_flag("inline"), "{argv:?}");
        }
        for argv in [
            vec!["start", "wk", "--inline"],
            vec!["resume", "wk", "--inline"],
            vec!["attach", "wk", "--inline"],
        ] {
            let m = parse(&argv).unwrap();
            let (_, sm) = m.subcommand().unwrap();
            assert!(sm.get_flag("inline"), "{argv:?}");
            assert!(!sm.get_flag("alt-screen"), "{argv:?}");
        }
        // Default: neither flag.
        let m = parse(&["start", "wk"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert!(!sm.get_flag("alt-screen"));
        assert!(!sm.get_flag("inline"));
        // The pair conflicts at parse (a session renders ONE way).
        let e = parse(&["start", "wk", "--alt-screen", "--inline"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn ping_session_optional() {
        // ping with no session parses (the no-arg validation is a runtime exit-3,
        // not a parse error).
        assert!(parse(&["ping"]).is_ok());
        let m = parse(&["ping", "--prefix", "wk"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(
            sm.get_one::<String>("prefix").map(String::as_str),
            Some("wk")
        );
    }

    #[test]
    fn mark_requires_session_and_payload() {
        assert!(parse(&["mark", "sess", "{}"]).is_ok());
        // Missing payload → MissingRequiredArgument.
        let e = parse(&["mark", "sess"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::MissingRequiredArgument);
    }

    // --- A6: --via is now VISIBLE in --help output ---

    #[test]
    fn via_present_in_start_help() {
        // A6 unhid --via: it must now appear in BOTH clap's rendered help AND the
        // override-help string (help::START) the verb actually prints.
        let mut start_cmd = build_cli()
            .find_subcommand("start")
            .cloned()
            .expect("start exists");
        let help = start_cmd.render_long_help().to_string();
        assert!(
            help.contains("--via"),
            "--via must now appear in rendered help:\n{help}"
        );
        // The verbatim override-help string carries it too (the user-facing text).
        assert!(
            help::START.contains("--via"),
            "--via must appear in the override-help START string"
        );
    }

    // --- P0 W1: retired stubs swallow ANY args at parse (the stub must fire
    // at dispatch instead of a clap usage error) ---

    #[test]
    fn retired_new_swallows_any_args_at_parse() {
        for argv in [
            vec!["new"],
            vec!["new", "wk"],
            vec!["new", "wk", "-p", "hello", "--model", "opus"],
            vec!["new", "--nosuchopt"],
        ] {
            let m = parse(&argv).unwrap_or_else(|e| {
                panic!("retired `new` must accept-and-ignore {argv:?}, got: {e}")
            });
            assert_eq!(m.subcommand_name(), Some("new"));
        }
    }

    #[test]
    fn retired_kill_swallows_any_args_at_parse() {
        for argv in [
            vec!["kill"],
            vec!["kill", "wk"],
            vec!["kill", "--force", "wk"],
            vec!["kill", "--server", "wk"],
        ] {
            let m = parse(&argv).unwrap_or_else(|e| {
                panic!("retired `kill` must accept-and-ignore {argv:?}, got: {e}")
            });
            assert_eq!(m.subcommand_name(), Some("kill"));
        }
    }

    #[test]
    fn stop_parses_like_old_kill() {
        // stop clones the old cmd_kill spec: <session> + parse-accepted
        // deprecated --force no-op + --server.
        let m = parse(&["stop", "wk", "--force", "--server"]).unwrap();
        let (_, sm) = m.subcommand().unwrap();
        assert_eq!(
            sm.get_one::<String>("session").map(String::as_str),
            Some("wk")
        );
        assert!(sm.get_flag("force"));
        assert!(sm.get_flag("server"));
        // Missing session is still a parse error (commander phrasing).
        let e = parse(&["stop"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::MissingRequiredArgument);
    }

    // --- error kinds → commander mapping ---

    #[test]
    fn missing_required_session_maps_to_commander_phrasing() {
        let e = parse(&["attach"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::MissingRequiredArgument);
        let msg = commander_message(&e);
        assert_eq!(msg, "error: missing required argument 'session'");
    }

    // --- connect is a hidden alias for attach: same parse contract ---

    #[test]
    fn connect_alias_parse_contract_matches_attach() {
        // connect accepts a session positional (alias contract).
        let m = parse(&["connect", "wk"]).expect("connect <session> must parse");
        let sm = m.subcommand_matches("connect").unwrap();
        assert_eq!(sm.get_one::<String>("session").map(String::as_str), Some("wk"));
        // connect without a session is a parse error (required positional).
        let e = parse(&["connect"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::MissingRequiredArgument);
        // connect rejects unknown options (no trailing passthrough).
        let e2 = parse(&["connect", "--nosuchopt", "wk"]).unwrap_err();
        assert_eq!(e2.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn missing_required_message_maps() {
        let e = parse(&["send:pty", "sess"]).unwrap_err();
        let msg = commander_message(&e);
        assert_eq!(msg, "error: missing required argument 'message'");
    }

    #[test]
    fn unknown_option_maps_to_commander_phrasing() {
        let e = parse(&["ls", "--nosuchopt"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnknownArgument);
        let msg = commander_message(&e);
        assert_eq!(msg, "error: unknown option '--nosuchopt'");
    }

    /// FTUE punch R7: a mistyped verb is reported as a MISTYPED VERB. The old
    /// contract rendered it as commander's "too many arguments. Expected 0
    /// arguments but got N." (the typo read as an excess operand to the default
    /// `ls` action) — an argument-count error for a spelling mistake. The
    /// operand count is gone; the token the user typed is what gets named, and
    /// the number of trailing words never changes the message.
    #[test]
    fn unknown_verb_says_unknown_command() {
        for argv in [
            vec!["qd".to_string(), "lss".to_string()],
            vec!["qd".to_string(), "lss".to_string(), "wk".to_string()],
            vec!["qd".to_string(), "lss".to_string(), "wk".to_string(), "x".to_string()],
        ] {
            let e = build_cli().try_get_matches_from(&argv).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::InvalidSubcommand, "{argv:?}");
            // The message names the typed token, not a count of what followed it.
            assert_eq!(unknown_verb_token(&e, &argv), "lss", "{argv:?}");
            // Exit 1 (commander), never clap's 2.
            assert_eq!(map_clap_error_with_argv(e, &argv), 1, "{argv:?}");
        }
    }

    #[test]
    fn help_and_version_are_display_kinds() {
        assert_eq!(
            parse(&["--help"]).unwrap_err().kind(),
            ErrorKind::DisplayHelp
        );
        assert_eq!(
            parse(&["--version"]).unwrap_err().kind(),
            ErrorKind::DisplayVersion
        );
    }

    #[test]
    fn version_string_is_0_1_0() {
        assert_eq!(VERSION, "0.1.0");
    }

    #[test]
    fn build_sha_is_twelve_hex_or_unknown() {
        assert!(
            BUILD_SHA == "unknown"
                || (BUILD_SHA.len() == 12 && BUILD_SHA.chars().all(|c| c.is_ascii_hexdigit())),
            "build.rs emitted an unusable sha: {BUILD_SHA:?}"
        );
    }

    #[test]
    fn version_line_appends_the_sha_and_degrades_to_bare_version() {
        let line = version_line();
        // The bare version is always the prefix — every consumer that scraped
        // `qd --version` for "0.1.0" still finds it at the front.
        assert!(line.starts_with(VERSION), "{line:?}");
        if BUILD_SHA == "unknown" {
            assert_eq!(line, VERSION);
        } else {
            assert_eq!(line, format!("0.1.0 ({BUILD_SHA})"));
        }
    }

    #[test]
    fn all_verbs_registered() {
        let cmd = build_cli();
        let names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
        for v in [
            "ls",
            "connect",
            "attach",
            "resume",
            "wrap",
            "adopt",
            "start",
            "stop",
            "kill",
            "new",
            "reconcile",
            "send",
            "send:pty",
            "send:relay",
            "send:http",
            "relay",
            "whoami",
            "dispositions",
            "wait",
            "live",
            "info",
            "gc",
            "init",
            "setup",
            "bootstrap",
            "update",
            "ping",
            "mark",
            // The three the hand-maintained `help::TOP` had silently dropped —
            // the drift FTUE punch R4 exists to make impossible.
            "dispositions",
            "delivery:recover",
            // Hand-parsed pre-clap (main.rs), registered here so R4's generated
            // table can list them.
            "config",
            "survey",
        ] {
            assert!(names.contains(&v), "missing verb {v}");
        }
        // 27 registrations here (`wrap` is the primary self-wrap shutdown flow;
        // renamed `adopt` stays registered+hidden as a backward-compat alias
        // routing to the same handler; `attach` is live; renamed `connect` stays
        // registered+hidden as an erroring stub; `init` added with the eval-init
        // shell integration; P0 W1 added `start`/`stop` with `new`/`kill`
        // retained as retired stubs; transcript-archive-spec.md Atomic B's
        // `backup` was retired by persist-relocation — transcript persistence now
        // lives in frame as `qf persist`, in-crate, no cross-binary hop); the
        // delivery/receipt contract (D1) added `delivery:recover`, the
        // dead-dangling recovery verb; the qd–qf transition W5 added
        // `dispositions`, the stateless JSONL read verb; config + survey are
        // dispatched pre-clap (hand-parsed) but registered here so R4's
        // generated help table can see them; the first-run `setup` verb (R15)
        // is the 29th.
        assert_eq!(names.len(), 31);
    }

    #[test]
    fn wrap_force_is_a_plain_flag_after_the_session() {
        let matches = parse(&["wrap", "bare-one", "--force"]).unwrap();
        let (name, wrap) = matches.subcommand().unwrap();
        assert_eq!(name, "wrap");
        assert_eq!(
            wrap.get_one::<String>("session").map(String::as_str),
            Some("bare-one")
        );
        assert!(wrap.get_flag("force"));

        let matches = parse(&["wrap", "bare-one"]).unwrap();
        let (_, wrap) = matches.subcommand().unwrap();
        assert!(!wrap.get_flag("force"));
    }

    // `adopt` is the hidden backward-compat alias for `wrap`; it must still parse
    // the same positional + `-f/--force` surface so existing callers keep working.
    #[test]
    fn adopt_alias_force_is_a_plain_flag_after_the_session() {
        let matches = parse(&["adopt", "bare-one", "--force"]).unwrap();
        let (name, adopt) = matches.subcommand().unwrap();
        assert_eq!(name, "adopt");
        assert_eq!(
            adopt.get_one::<String>("session").map(String::as_str),
            Some("bare-one")
        );
        assert!(adopt.get_flag("force"));

        let matches = parse(&["adopt", "bare-one"]).unwrap();
        let (_, adopt) = matches.subcommand().unwrap();
        assert!(!adopt.get_flag("force"));
    }

    // The hidden `adopt` alias carries the redirect notice, exactly as `connect`
    // does for `attach`; the primary `wrap` help teaches the real verb.
    #[test]
    fn adopt_alias_help_redirects_to_wrap() {
        assert!(help::ADOPT.contains("(renamed — use qd wrap)"));
        assert!(help::WRAP.contains("Wrap a live bare Claude Code session"));
        assert!(!help::WRAP.contains("(renamed"));
        // R14: `wrap` and its `adopt` alias are BOTH off the human table now —
        // the four session verbs plus `setup` are the whole of it. Both are still
        // registered, so both are on the `--help-all` surface.
        let cmd = build_cli();
        let top = help::render_top(&cmd, false, false);
        assert!(!top.contains("wrap ["), "wrap is off the human table: {top}");
        assert!(!top.contains("adopt ["), "adopt is off the human table: {top}");
        let all = help::render_top(&cmd, true, false);
        assert!(all.contains("wrap [options] <session>"));
        assert!(all.contains("adopt [options] <session>"));
    }

    // === FTUE punch R14 / R4 — the generated, five-verb help surface ===

    /// R14: `qd --help` shows EXACTLY the four session verbs plus `setup`, in
    /// ONE `Commands:` table. Everything else is hidden.
    ///
    /// `setup` used to sit below the table in a `First run:` section of its own,
    /// under a three-line note. Both are gone: it is a command like the others,
    /// the section said otherwise, and "first run" was a greeting the surface
    /// repeated to people on their five-hundredth run.
    ///
    /// MUTATION EVIDENCE: unhiding any other verb reds the "Other commands:"
    /// assert (the safety-net section it would land in); dropping a name from
    /// `help::HUMAN_VERBS` reds its row assert; reordering the list reds the
    /// positional row asserts.
    #[test]
    fn visible_help_table_is_the_four_session_verbs_plus_setup() {
        use std::collections::BTreeSet;
        let cmd = build_cli();
        let visible: BTreeSet<&str> = cmd
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
            .map(|c| c.get_name())
            .collect();
        let expected: BTreeSet<&str> = ["ls", "start", "stop", "attach", "setup"].into();
        assert_eq!(visible, expected, "the human surface is R14's five rows");

        let top = help::render_top(&cmd, false, false);
        // Commander layout is preserved — only the source of the bytes changed.
        assert!(top.starts_with("Usage: qd [options] [command]\n"));
        // `-h` says what it does AND that it works on every row below it — the
        // one piece of navigation the table cannot show.
        assert!(
            top.contains("  -h, --help")
                && top.contains("append it to any command for that command's help"),
            "{top}"
        );
        // The five human verbs, in the ruled order, with the `ls|list` alias style.
        let commands = top.split("\nCommands:\n").nth(1).expect("a Commands: section");
        let rows: Vec<&str> = commands.lines().take_while(|l| l.starts_with("  ")).collect();
        assert_eq!(rows.len(), 5, "the five human verbs, one table: {rows:?}");
        assert!(rows[0].starts_with("  ls|list [options]"), "{rows:?}");
        assert!(rows[1].starts_with("  start [options] <name> [claudeArgs...]"), "{rows:?}");
        assert!(rows[2].starts_with("  stop [options] <session>"), "{rows:?}");
        assert!(rows[3].starts_with("  attach [options] <session>"), "{rows:?}");
        assert!(rows[4].starts_with("  setup [options]"), "{rows:?}");
        // No `First run:` section, no note under it, and no "first run" anywhere.
        assert!(!top.contains("First run"), "{top}");
        assert!(!top.to_lowercase().contains("first run"), "{top}");
        // No hidden verb leaks a row, and the safety-net section stays empty.
        for hidden in ["send:relay", "reconcile", "dispositions", "bootstrap", "gc ", "wrap ["] {
            assert!(!top.contains(hidden), "{hidden} must not be on the human table: {top}");
        }
        assert!(!top.contains("Other commands:"), "nothing unclassified is visible: {top}");
    }

    /// The summary line describes the PRODUCT: sessions on any supported
    /// harness, plus the messaging between them. It named one vendor for as
    /// long as qd existed ("Claude Sessions — manage Claude Code sessions"),
    /// which was wrong about the sessions it lists AND silent about the relay.
    ///
    /// MUTATION EVIDENCE: putting a single-provider summary back reds this.
    #[test]
    fn top_help_summary_is_cross_provider_and_names_the_messaging() {
        let top = help::render_top(&build_cli(), false, false);
        let summary = top.lines().nth(2).expect("summary line");
        assert!(summary.contains("across providers"), "{summary:?}");
        assert!(summary.contains("message"), "{summary:?}");
        assert!(!top.contains("Claude Sessions"), "{top}");
        // ...and `ls` no longer claims to list only one harness's sessions.
        assert!(top.contains("List all sessions, on every provider"), "{top}");
    }

    /// `start`'s one-liner names the providers `Harness::from_provider_id`
    /// ACTUALLY accepts, and it is DERIVED from that set rather than retyped —
    /// the hand-written list had lost `acp/claude-code`.
    ///
    /// MUTATION EVIDENCE: adding a harness to `Harness::ALL` without touching
    /// this file changes the row (and reds the START-const twin assert below);
    /// hard-coding the row again reds the derivation assert.
    #[test]
    fn start_row_names_every_provider_the_engine_accepts() {
        let top = help::render_top(&build_cli(), false, false);
        for h in quorum_qw::lane::Harness::ALL {
            let advertised = match h {
                quorum_qw::lane::Harness::Opencode => "opencode",
                other => other.provider_id(),
            };
            assert!(
                top.contains(advertised),
                "{h:?} is startable but not advertised: {top}"
            );
            assert!(
                quorum_qw::lane::Harness::from_provider_id(advertised).is_some(),
                "{advertised} is advertised but not accepted"
            );
        }
        // The verb's own `--help` opens with the same sentence as its table row,
        // so the two surfaces cannot name different providers.
        assert_eq!(
            help::START.lines().nth(2),
            Some(help::start_about().as_str()),
            "`qd start --help` and the table row disagree about providers"
        );
    }

    /// The one state-dependent line in the help: an unfinished install says so,
    /// a finished one says nothing at all. (The state itself is probed at the
    /// print sites — see `verbs::setup::install_is_incomplete`.)
    ///
    /// MUTATION EVIDENCE: printing the notice unconditionally reds the
    /// finished-install assert; dropping it reds the other.
    #[test]
    fn setup_notice_appears_only_when_the_install_is_unfinished() {
        let cmd = build_cli();
        let finished = help::render_top(&cmd, false, false);
        assert!(!finished.to_lowercase().contains("not fully set up"), "{finished}");
        let unfinished = help::render_top(&cmd, false, true);
        assert!(unfinished.contains("not fully set up"), "{unfinished}");
        assert!(unfinished.contains("`qd setup`"), "{unfinished}");
        // It is a TRAILER: the table is intact and the notice follows it.
        assert!(
            unfinished.starts_with(finished.trim_end()),
            "the notice must only ADD to the finished help:\n{unfinished}"
        );
    }

    /// R14's load-bearing claim: `.hide(true)` is help-only. Every hidden verb
    /// still PARSES to itself, so dispatch is untouched — hidden-but-working.
    /// The table is checked against the built tree, so a newly hidden verb fails
    /// here until someone adds its parse check.
    #[test]
    fn hidden_verbs_still_parse_and_reach_their_own_dispatch_arm() {
        use std::collections::BTreeSet;
        let invocations: [(&str, &[&str]); 26] = [
            ("connect", &["connect", "wk"]),
            ("resume", &["resume", "wk"]),
            ("wrap", &["wrap", "wk"]),
            ("adopt", &["adopt", "wk"]),
            ("kill", &["kill", "wk"]),
            ("new", &["new", "wk"]),
            ("reconcile", &["reconcile"]),
            ("send", &["send", "wk", "hi"]),
            ("send:pty", &["send:pty", "wk", "hi"]),
            ("send:relay", &["send:relay", "wk", "hi"]),
            ("send:http", &["send:http", "wk", "hi"]),
            ("relay", &["relay"]),
            ("whoami", &["whoami"]),
            ("dispositions", &["dispositions"]),
            ("wait", &["wait", "wk"]),
            ("live", &["live"]),
            ("info", &["info", "wk"]),
            ("gc", &["gc"]),
            ("init", &["init", "bash"]),
            ("bootstrap", &["bootstrap"]),
            ("update", &["update"]),
            ("ping", &["ping"]),
            ("mark", &["mark", "wk", "{}"]),
            ("delivery:recover", &["delivery:recover"]),
            ("config", &["config"]),
            ("survey", &["survey"]),
        ];
        let covered: BTreeSet<&str> = invocations.iter().map(|(n, _)| *n).collect();
        let cmd = build_cli();
        let hidden: BTreeSet<&str> = cmd
            .get_subcommands()
            .filter(|c| c.is_hide_set())
            .map(|c| c.get_name())
            .collect();
        assert_eq!(
            hidden, covered,
            "every hidden verb needs a parse check here (hide must not break dispatch)"
        );
        for (name, argv) in invocations {
            let m = parse(argv).unwrap_or_else(|e| panic!("hidden verb {name} must parse: {e}"));
            assert_eq!(m.subcommand_name(), Some(name), "{argv:?}");
        }
        // Aliases survive hiding too (`qd name` still routes to whoami).
        assert_eq!(parse(&["name"]).unwrap().subcommand_name(), Some("whoami"));
    }

    /// R4: `qd --help-all` prints the SAME generated table with the hidden rows
    /// restored — so the full surface stays reachable and no registration can go
    /// undocumented. This is the assertion the hand-maintained `help::TOP` could
    /// not make: it had silently lost `dispositions`, `mark` and
    /// `delivery:recover`.
    #[test]
    fn help_all_lists_every_registered_verb() {
        let cmd = build_cli();
        let all = help::render_top(&cmd, true, false);
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            assert!(
                all.contains(&format!("\n  {name}")),
                "--help-all must carry a row for {name}:\n{all}"
            );
        }
        for lost in ["dispositions [options]", "mark <session> <payload>", "delivery:recover [options]"] {
            assert!(all.contains(lost), "the drifted-away verbs are listed: {all}");
        }
        assert!(all.contains("Hidden from `qd --help`"), "{all}");
        // The pointer is for the SHORT help only — this IS the full surface.
        assert!(!all.contains("prints the full surface"), "{all}");
    }

    /// R1: no `qd` help surface names the retired mux, and `resume`'s two dead
    /// parked flags are gone from the parser as well as from the help.
    #[test]
    fn zmx_is_gone_from_the_help_surface_and_the_resume_flags() {
        let all = help::render_top(&build_cli(), true, false);
        assert!(!all.to_lowercase().contains("zmx"), "{all}");
        for (surface, text) in [
            ("RESUME", help::RESUME),
            ("START", help::START),
            ("RECONCILE", help::RECONCILE),
            ("SEND_PTY", help::SEND_PTY),
            ("ATTACH", help::ATTACH),
        ] {
            assert!(
                !text.to_lowercase().contains("zmx"),
                "{surface} help still names zmx: {text}"
            );
        }
        // The parked flags refuse instead of pretending to work.
        for argv in [vec!["resume", "wk", "--no-zmx"], vec!["resume", "wk", "--zmx-name", "z"]] {
            let e = parse(&argv).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::UnknownArgument, "{argv:?}");
        }
        // --no-attach is NOT a zmx flag and still parses.
        assert!(parse(&["resume", "wk", "--no-attach"]).is_ok());
    }

    #[test]
    fn bootstrap_description_is_engine_only_one_liner() {
        // The bootstrap description is the engine-only one-liner (spec §3 row 17),
        // NOT the TS verbatim text (which carries scope-banned tokens). Assert the
        // exact replacement string so a regression to the banned text fails here.
        let cmd = build_cli();
        let b = cmd.find_subcommand("bootstrap").unwrap();
        let about = b.get_about().map(|s| s.to_string()).unwrap_or_default();
        assert_eq!(
            about,
            "Set up qd's local data directory under ~/.quorum/dispatch (idempotent)"
        );
    }
}
