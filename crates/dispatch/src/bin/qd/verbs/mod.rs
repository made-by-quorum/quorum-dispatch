//! Verb dispatch + the REAL backends (spec §3). One module per verb group; this
//! file routes a parsed clap `ArgMatches` to the right backend, and the default
//! action (bare `qd` → ls, index.ts:202-204) when no subcommand matched.

mod acp_loss;
mod adopt;
mod bootstrap;
mod common;
mod attach;
mod gc;
mod init;
mod kill;
mod lifecycle;
mod ls;
mod mark;
mod ping;
mod reconcile;
mod recover;
pub(super) mod resume;
mod send;
mod send_relay;
mod send_unified;
mod stubs;
mod update;
mod wait;
mod whoami;

use clap::ArgMatches;

/// Route the parsed top-level matches to the verb backend. When no subcommand
/// was given, run the default action (`ls`), index.ts:202-204.
pub fn dispatch(matches: &ArgMatches) -> i32 {
    match matches.subcommand() {
        Some(("ls", m)) => ls::run(m),
        Some(("attach", m)) => attach::run(m),
        // `connect` was renamed to `attach`; kept as a hidden backward-compat alias
        // so existing shell wrappers that call `qd connect <session>` keep working.
        Some(("connect", m)) => attach::run(m),
        Some(("resume", m)) => resume::run(m),
        Some(("wrap", m)) => adopt::run(m),
        // `adopt` was renamed to `wrap`; kept as a hidden backward-compat alias
        // so existing callers of `qd adopt <session>` keep working (same pattern
        // as `connect` → `attach` above).
        Some(("adopt", m)) => adopt::run(m),
        // P0 W1 (qb spec-cli §11): start/stop are the lifecycle verbs — same
        // backends as the old new/kill (renamed, not forked). new/kill are
        // RETIRED erroring stubs (helpful error, exit 1, never touch state).
        Some(("start", m)) => lifecycle::run_new(m),
        Some(("stop", m)) => kill::run(m),
        Some(("kill", _)) => stubs::run_kill_retired(),
        Some(("new", _)) => stubs::run_new_retired(),
        Some(("reconcile", m)) => reconcile::run(m),
        Some(("send", m)) => send_unified::run_send_unified(m),
        Some(("send:pty", m)) => send::run_send_pty(m),
        Some(("send:relay", m)) => send_relay::run(m),
        Some(("send:http", m)) => send::run_send_http(m),
        Some(("relay", _)) => stubs::run_relay_moved(),
        Some(("whoami", m)) => whoami::run(m),
        Some(("wait", m)) => wait::run_wait(m),
        Some(("live", m)) => lifecycle::run_live(m),
        Some(("info", m)) => lifecycle::run_info(m),
        Some(("gc", m)) => gc::run(m),
        Some(("init", m)) => init::run(m),
        Some(("bootstrap", _)) => bootstrap::run(),
        Some(("update", _)) => update::run(),
        Some(("ping", m)) => ping::run(m),
        Some(("mark", m)) => mark::run(m),
        // §C2 (delivery contract) — the one-shot, dispatch-only recovery verb (D1).
        Some(("delivery:recover", m)) => recover::run(m),
        // Unknown subcommand never reaches here (clap rejects it pre-dispatch).
        Some((other, _)) => {
            eprintln!("error: unknown command '{other}'");
            1
        }
        // No subcommand: default action → ls (index.ts:202-204).
        None => ls::run_default(),
    }
}
