//! `qd` CLI — the full ENGINE verb surface (A3, spec §2/§3). Replaces A2's
//! minimal hand-rolled new/attach binary (`bin/qd.rs:1-9` said A3 owns the real
//! surface).
//!
//! Architecture (spec §2, BINDING):
//! - **clap v4 builder API** (not derive) — fine-grained help-text + error-path
//!   control is the whole game. We register all 24 verbs + the default action.
//! - **`try_parse` + custom error mapping** (`cli::map_clap_error`): clap's
//!   default error exit is **2**; commander's is **1**. Every clap parse error is
//!   caught and re-rendered in commander's phrasing with exit 1. `-h/--help` and
//!   `-V/--version` still exit 0.
//! - **Hand-parsed verbs stay hand-parsed:** `config` and `survey` bypass
//!   commander in TS (allowUnknownOption + allowExcessArguments + helpOption(false)).
//!   We register them as raw-arg commands (`trailing_var_arg`) and hand the argv
//!   tail to ported `config::run_config_logic` / `survey::parse_survey_args` so
//!   clap never reshapes their errors / exit conventions (survey parse error → 2,
//!   config bare/--help → 0, unknown subcommand → 2).
//! - **Default action:** bare `qd` (no verb) prints the top-level help. It ran
//!   `ls` from the TS days through R24 (index.ts:202-204); see `verbs::dispatch`
//!   for why that moved and what it cost.
//!
//! Real-deps wiring (kept from A2's pattern): `RealExec` / `ZmxMux` / `RealEnv` /
//! `RealClock` / `QdPaths::from_home(HOME)` / `EventBootWaiter`.

mod cli;
mod config;
mod driver;
mod help;
mod survey;
mod tty;
mod verbs;

use std::process::exit;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    exit(run(&argv));
}

/// Top-level dispatch. `argv[0]` is the program name; `argv[1..]` is the verb +
/// args. We special-case `config` and `survey` BEFORE clap (TS dispatches them
/// pre-commander, index.ts pattern) so their hand-parsed exit conventions survive.
fn run(argv: &[String]) -> i32 {
    let rest = &argv[1..];

    // config / survey: hand-parsed, pre-clap (spec §2). The argv tail after the
    // verb is handed verbatim to the ported pure logic.
    //
    // qrmux-server: the HIDDEN embedded-daemon entry (C1 M4fix). The qd binary IS
    // the embedded qrmux daemon — it links qrmux — so the client launcher re-execs
    // `<exe> qrmux-server [--socket-dir DIR]` (NOT `<exe> server`, which has no
    // verb). Dispatched PRE-CLAP so it never enters the user-facing clap surface:
    // the a3 help/exit-byte surface stays byte-unchanged and the daemon entry can't
    // be commander-error-mangled. Internal-only; not advertised in help.
    //
    // The BODY lives in `quorum_qw::qrmux_server` and `qw` dispatches it too (ruling
    // D6): the launcher re-execs `current_exe()`, and inside the qw subprocess that
    // is `qw`. qd keeps the verb because daemons already running in the wild were
    // spawned as `qd qrmux-server`.
    match rest.first().map(String::as_str) {
        Some("config") => return config::dispatch(&rest[1..]),
        Some("survey") => return survey::dispatch(&rest[1..]),
        Some("qrmux-server") => {
            return quorum_qw::qrmux_server::run_qrmux_server("qd qrmux-server", &rest[1..])
        }
        // relay:serve: machine-spawned hidden operational verb (MCP config spawns
        // it). Dispatched pre-clap so it never enters the user-facing clap surface
        // and cannot be commander-error-mangled. Not advertised in help. §2.
        Some("relay:serve") => return dispatch::relay_server::run(),
        // acp-daemon: the HIDDEN resident ACP adapter entry (S1). The create path
        // spawns `<exe> acp-daemon --listen ws://… --cwd …` DETACHED; that process
        // owns the claude-code-acp bridge + serves the ws front, outliving the create
        // verb (cross-process residence). Dispatched pre-clap so it never enters the
        // user-facing surface and cannot be commander-error-mangled. Internal-only.
        Some("acp-daemon") => return dispatch::acp_residence::run_adapter(&rest[1..]),
        // pi-daemon: the HIDDEN resident pi adapter entry (WS-A.2, item 1). The
        // create path spawns `<exe> pi-daemon --listen ws://… --cwd …` DETACHED;
        // that process owns the `pi --mode rpc` child + serves the loopback front,
        // outliving the create verb (cross-process residence — the acp-daemon twin).
        // Dispatched pre-clap so it never enters the user-facing surface. Internal-only.
        Some("pi-daemon") => return dispatch::provider::pi::residence::run_pi_adapter(&rest[1..]),
        // relay:register (alias relay:repoint) / relay:rollback: hidden verbs.
        // Dispatched pre-clap so they are never advertised in help and cannot be
        // commander-error-mangled. They drive Claude Code's own `claude mcp` CLI
        // (ADR 0017) — CC owns its MCP config location/format; we register
        // `qd relay:serve` at user scope as the BARE command (resolved via PATH).
        Some("relay:register") | Some("relay:repoint") => {
            return run_relay_register();
        }
        Some("relay:rollback") => {
            return run_relay_rollback();
        }
        // adoption:stop: the per-session Claude Stop hook target. It appends a
        // durable boundary event under qd's state dir and emits no stdout (hook
        // stdout is protocol-significant to Claude Code). Hidden pre-clap so a
        // hook cannot be reshaped by the user-facing command parser.
        Some("adoption:stop") => {
            return run_adoption_stop(&rest[1..]);
        }
        // adoption:relaunch: hidden programmatic relaunch verb used by
        // `qd adopt`'s external-adoption flow. Bypasses registry resolution so
        // the relaunch succeeds even when the original session's registry row was
        // deleted on exit (zero-turn bare sessions leave no tombstone or JSONL).
        // Takes: --session-id <uuid> --name <name> [--cwd <cwd>]
        // Dispatched pre-clap so it never enters the user-facing surface.
        Some("adoption:relaunch") => {
            return run_adoption_relaunch(&rest[1..]);
        }
        // build-profile: HIDDEN deploy-gate probe (perf-regression postmortem,
        // 2026-07-07). Prints "release" or "debug" via `cfg!(debug_assertions)`
        // and exits 0 — a debug build must never answer "release". Dispatched
        // pre-clap (like the other hidden verbs above) so it can't be shadowed
        // by a real verb or mangled by commander error mapping. Read by
        // `scripts/deploy-gate.sh` before any binary is moved into a canonical
        // bin dir; see dispatch/doc/DEPLOY.md.
        Some("build-profile") => {
            println!("{}", if cfg!(debug_assertions) { "debug" } else { "release" });
            return 0;
        }
        // --help-all: the FULL verb surface (FTUE punch R4). `qd --help` lists
        // the four session verbs plus `setup` (R14) and hides the rest — hidden,
        // not removed: every one of them still parses and dispatches. Agents and
        // power users need a way to SEE that surface, so this flag prints the
        // same generated table with the hidden rows included, exit 0.
        //
        // Dispatched PRE-CLAP for the same reason `build-profile` and
        // `relay:serve` are: clap's top-level parse would have to be taught a
        // flag whose only job is to print a different help, and any mistake in
        // that teaching comes back as a commander-mangled parse error instead of
        // help. A `--help`-shaped token can never be a session name, so there is
        // nothing here to shadow.
        Some("--help-all") => {
            let incomplete = verbs::setup::install_is_incomplete();
            print!("{}", help::render_top(&cli::build_cli(), true, incomplete));
            return 0;
        }
        // `qd --help` / `qd -h`: dispatched PRE-CLAP for one reason — the help
        // clap would print is the string baked into `cli::build_cli`, and the
        // setup-state line can only be true at PRINT time. Building it in
        // `build_cli` would instead charge every `qd send:relay` a filesystem
        // probe for a line it never prints. Only the bare form is taken here;
        // `qd start --help` is clap's, unchanged.
        Some("--help") | Some("-h") if rest.len() == 1 => {
            let incomplete = verbs::setup::install_is_incomplete();
            print!("{}", help::render_top(&cli::build_cli(), false, incomplete));
            return 0;
        }
        _ => {}
    }

    // Everything else goes through clap. try_get_matches → commander error map.
    let cmd = cli::build_cli();
    match cmd.try_get_matches_from(argv) {
        Ok(matches) => verbs::dispatch(&matches),
        Err(e) => cli::map_clap_error_with_argv(e, argv),
    }
}

fn run_adoption_stop(args: &[String]) -> i32 {
    use dispatch::effects::Env;

    let [session_id] = args else {
        eprintln!("adoption:stop: expected exactly one session id");
        return 1;
    };
    if session_id.is_empty() {
        eprintln!("adoption:stop: session id is empty");
        return 1;
    }
    let env = dispatch::effects::RealEnv;
    let Some(home) = env.var("HOME").filter(|value| !value.is_empty()) else {
        eprintln!("adoption:stop: HOME is not set");
        return 1;
    };
    let home = std::path::PathBuf::from(home);
    let paths = dispatch::paths::QdPaths::from_home_env(&home, &env);
    let observed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    match dispatch::adoption::record_stop_hook_event(
        &paths.state_dir,
        session_id,
        observed_at_ms,
        std::process::id(),
    ) {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("adoption:stop: {error}");
            1
        }
    }
}

/// `qd adoption:relaunch`: hidden verb used by the external-adoption flow to
/// relaunch a session's Claude process without depending on a live registry row.
///
/// When `qd wrap` SIGTERMs a bare session and the process exits, Claude may
/// delete its own registry row on exit (zero-turn sessions that never wrote a
/// JSONL file leave no tombstone at all). `qd resume <name>` would fail with
/// "No session matching" because the row is gone. This verb bypasses that
/// resolution by building the Session struct from the known UUID + name + cwd
/// (all captured in the AdoptRecord before SIGTERM was sent) and calling
/// `revive_claude` directly.
///
/// Args: `--session-id <uuid> --name <name> [--cwd <cwd>]`
fn run_adoption_relaunch(args: &[String]) -> i32 {
    let mut session_id: Option<String> = None;
    let mut name: Option<String> = None;
    let mut cwd: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session-id" => {
                i += 1;
                session_id = args.get(i).cloned();
            }
            "--name" => {
                i += 1;
                name = args.get(i).cloned();
            }
            "--cwd" => {
                i += 1;
                cwd = args.get(i).cloned();
            }
            other => {
                eprintln!("adoption:relaunch: unknown argument {other:?}");
                return 1;
            }
        }
        i += 1;
    }

    let session_id = match session_id.filter(|s| !s.is_empty()) {
        Some(id) => id,
        None => {
            eprintln!("adoption:relaunch: --session-id is required and must be non-empty");
            return 1;
        }
    };
    let name = match name.filter(|s| !s.is_empty()) {
        Some(n) => n,
        None => {
            eprintln!("adoption:relaunch: --name is required and must be non-empty");
            return 1;
        }
    };

    // Build a minimal Session struct from the known adoption facts. Status is
    // set to Cold so revive_claude's "no session id" guard is the only check
    // that can gate (and we've already verified session_id is non-empty). The
    // cwd from the AdoptRecord is used directly; revive_claude will reality-check
    // it and error if it has vanished (no silent fallback).
    use dispatch::model::{Session, SessionBranch, SessionStatus};
    let session = Session {
        name: Some(name),
        user_named: Some(true),
        session_id,
        code: None,
        qd_id: None,
        pid: None,
        status: SessionStatus::Cold,
        zmx_name: None,
        zmx_clients: None,
        socket_dir: None,
        relay_port: None,
        turns: 0,
        tokens: 0,
        cwd,
        last_active_ms: None,
        version: None,
        started_at_ms: None,
        git_branch: None,
        jsonl_path: None,
        last_turns: None,
        provider: "claude-code".to_string(),
        entrypoint: None,
        lineage: None,
        hosting: None,
        which_branch: SessionBranch::LiveRegistry,
    };

    // Zero-turn bare sessions never write a JSONL transcript; claude's --resume
    // fails with "No conversation found" when no JSONL exists. Branch on JSONL
    // presence: use --session-id (fresh start under the same UUID) when there's
    // no transcript to preserve, --resume (restore conversation) when one exists.
    let fresh = {
        use dispatch::effects::Env;
        let env = dispatch::effects::RealEnv;
        match env.var("HOME").filter(|s| !s.is_empty()) {
            Some(home) => {
                let home = std::path::PathBuf::from(home);
                let paths = dispatch::paths::QdPaths::from_home(&home);
                dispatch::jsonl::find_jsonl_path(
                    &paths.projects_dir,
                    &session.session_id,
                    session.cwd.as_deref(),
                )
                .is_none()
            }
            // HOME unset: can't locate JSONL; assume zero-turn (safer default).
            None => true,
        }
    };

    use dispatch::launch::RenderMode;
    // The verb the user actually invoked. This is the hidden `adoption:relaunch`
    // entry point, so that is what its failure lines name — not `attach`, which is
    // what the pre-split revive hard-coded from every caller.
    match verbs::resume::revive_claude(&session, None, RenderMode::Inline, fresh, "adoption:relaunch")
    {
        Ok(handle) => {
            println!(
                "Resumed session \"{}\" from {} (detached); attach with \"qd attach {}\".",
                handle.zmx_name,
                dispatch::fmt::truncate_id_default(&session.session_id),
                handle.zmx_name
            );
            0
        }
        Err(code) => code,
    }
}

/// `qd relay:register` (alias `relay:repoint`): register `qd relay:serve` with
/// Claude Code at user scope, as the BARE `qd` command (resolved via PATH — so
/// the entry never goes stale when the binary moves/upgrades). Idempotent
/// (re-running re-points to the bare command). Requires `claude` on PATH.
fn run_relay_register() -> i32 {
    use dispatch::exec::RealExec;
    use dispatch::relay_server::register;
    let exec = RealExec;
    if !dispatch::bootstrap::real_command_exists(&exec, "claude") {
        eprintln!(
            "relay:register: `claude` is not on PATH — install Claude Code first, then re-run."
        );
        return 1;
    }
    match register::register_relay(&exec) {
        Ok(()) => {
            println!(
                "relay:register: registered the relay MCP server with Claude Code (user scope) \
                 → `{cmd} relay:serve` (bare command, resolved via PATH).\n  \
                 New sessions will load it; verify with `claude mcp get relay`.\n  \
                 (Ensure the deployed `qd` is on PATH — see doc/DEPLOY.md.)",
                cmd = register::RELAY_BARE_COMMAND
            );
            0
        }
        Err(e) => {
            eprintln!("relay:register: {e}");
            1
        }
    }
}

/// `qd relay:rollback`: remove the relay MCP server from Claude Code (user
/// scope). Idempotent. Requires `claude` on PATH.
fn run_relay_rollback() -> i32 {
    use dispatch::exec::RealExec;
    let exec = RealExec;
    if !dispatch::bootstrap::real_command_exists(&exec, "claude") {
        eprintln!("relay:rollback: `claude` is not on PATH — nothing to do via `claude mcp`.");
        return 1;
    }
    match dispatch::relay_server::register::unregister_relay(&exec) {
        Ok(()) => {
            println!("relay:rollback: removed the relay MCP server from Claude Code (user scope).");
            0
        }
        Err(e) => {
            eprintln!("relay:rollback: {e}");
            1
        }
    }
}
