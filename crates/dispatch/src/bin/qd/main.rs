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
//! - **Default action:** bare `qd` (no verb) runs `ls` (index.ts:202-204).
//!
//! Real-deps wiring (kept from A2's pattern): `RealExec` / `ZmxMux` / `RealEnv` /
//! `RealClock` / `QdPaths::from_home(HOME)` / `EventBootWaiter`.

mod cli;
mod config;
mod daemon;
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
    // `qd qrmux-server [--socket-dir DIR]` (NOT `qd server`, which has no verb).
    // Dispatched PRE-CLAP so it never enters the user-facing clap surface: the a3
    // help/exit-byte surface stays byte-unchanged and the daemon entry can't be
    // commander-error-mangled. Internal-only; not advertised in help.
    match rest.first().map(String::as_str) {
        Some("config") => return config::dispatch(&rest[1..]),
        Some("survey") => return survey::dispatch(&rest[1..]),
        Some("qrmux-server") => return daemon::run_qrmux_server(&rest[1..]),
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
        _ => {}
    }

    // Everything else goes through clap. try_get_matches → commander error map.
    let cmd = cli::build_cli();
    match cmd.try_get_matches_from(argv) {
        Ok(matches) => verbs::dispatch(&matches),
        Err(e) => cli::map_clap_error_with_argv(e, argv),
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
                 (Ensure the deployed `dispatch` is on PATH — see doc/DEPLOY.md.)",
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
