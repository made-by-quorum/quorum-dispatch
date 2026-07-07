//! Moved-stubs with exact ported stderr (commands/send.ts:42-47, 524-528).
//!
//! (A5 M5: `qd ping` is now a REAL verb — its no-arg exit-3 validation moved to
//! `verbs/ping.rs` alongside the classifier wiring; this file keeps only the
//! send/relay moved-pointer stubs, which ARE the intended behavior.)

/// `qd send` moved stub (commands/send.ts:41-47): the 5-line stderr pointer, exit 1. THE
/// STUB IS THE BEHAVIOR (spec §3 row 7) — not an A4/A5 deferral.
pub fn run_send_moved() -> i32 {
    eprintln!("\"qd send\" has been split into explicit channels:\n");
    eprintln!("  qd send:pty <session> <message>     Send via zmx PTY (types into terminal)");
    eprintln!("  qd send:relay <session> <message>   Send via relay HTTP endpoint");
    eprintln!("  qd send:http <session> <message>    Send via OpenCode HTTP API\n");
    eprintln!("Pick the channel that matches how you want to communicate.");
    1
}

/// `qd relay` moved stub (commands/send.ts:523-528): stderr pointer, exit 1.
pub fn run_relay_moved() -> i32 {
    eprintln!("\"qd relay\" has moved to \"qd send:relay\".\n");
    eprintln!("  qd send:relay <session> <message>          async (returns message ID)");
    eprintln!("  qd send:relay <session> <message> --wait   block until reply\n");
    eprintln!("The relay channel is unchanged — just the command name.");
    1
}

/// P0 W1 (qb spec-cli §11): the retired `new` stub's exact stderr line.
/// Factored so the wording is pinned by a unit test (the house pattern, see
/// `codex_already_running_line` in verbs/resume.rs).
pub fn new_retired_line() -> &'static str {
    "qd new: `new` is retired; use `qd start`"
}

/// P0 W1 (qb spec-cli §11): the retired `kill` stub's exact stderr line.
pub fn kill_retired_line() -> &'static str {
    "qd kill: `kill` is retired; use `qd stop`"
}

/// `qd new …` retired stub: helpful stderr, exit 1. Never touches any state;
/// the clap registration swallows all args (trailing var-args) so this fires
/// instead of a clap usage error.
pub fn run_new_retired() -> i32 {
    eprintln!("{}", new_retired_line());
    1
}

/// `qd kill …` retired stub: helpful stderr, exit 1. Same contract as `new`.
pub fn run_kill_retired() -> i32 {
    eprintln!("{}", kill_retired_line());
    1
}

/// P0 start-surface rework (STATE 22 ruling): the retired `attach` stub's exact
/// stderr line. `connect` already covers attach-or-resume for humans (live →
/// attach; cold → auto-revive → attach; codex → loud daemon redirect), and
/// agents drive sessions via `qd send:relay`; `attach` was already internally
/// demoted/hidden — this completes the retirement (the new/kill pattern).
pub fn attach_retired_line() -> &'static str {
    "qd attach: `attach` is retired; humans use `qd connect`, agents use `qd send:relay`"
}

/// `qd attach …` retired stub: helpful stderr, exit 1. Same contract as
/// `new`/`kill` (trailing pass-through swallows any args, never touches state).
pub fn run_attach_retired() -> i32 {
    eprintln!("{}", attach_retired_line());
    1
}

/// The retired `qd start --agent` stub's exact stderr line. Role/agent CONTENT
/// (the `<name>.md` definitions the old static-agent path resolved under
/// `~/.quorum/dispatch/plugins/core/agents/`) now lives in the work-model PLUGIN; spawning a
/// role is `frame commission <role>.md`, not the engine's native `--agent`. The
/// engine refuses the flag with this teaching error instead of resolving a
/// path it no longer owns (the new/kill/attach retirement pattern). Pinned
/// by a unit test so the wording can't drift.
pub fn start_agent_retired_line() -> &'static str {
    "qd start --agent is retired: role/agent content lives in the work-model plugin now. \
     Commission a role via `frame commission <path-to-role>.md --name … --goal …` instead."
}

/// `qd start --agent <name>` retired guard: helpful stderr, exit 1. Fires from
/// `run_new` BEFORE any preflight/claim/launch, so nothing is created. Same
/// contract as the other retired stubs.
pub fn run_start_agent_retired() -> i32 {
    eprintln!("{}", start_agent_retired_line());
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    // P0 W1: the retired-stub stderr lines + exit codes are PINNED (spec-cli
    // §11: "`kill` is retired; use `stop`" — helpful error, exit 1; STATE 22
    // added the attach retirement with the same contract).
    #[test]
    fn retired_stub_lines_are_pinned() {
        assert_eq!(
            new_retired_line(),
            "qd new: `new` is retired; use `qd start`"
        );
        assert_eq!(
            kill_retired_line(),
            "qd kill: `kill` is retired; use `qd stop`"
        );
        assert_eq!(
            attach_retired_line(),
            "qd attach: `attach` is retired; humans use `qd connect`, agents use `qd send:relay`"
        );
        assert_eq!(
            start_agent_retired_line(),
            "qd start --agent is retired: role/agent content lives in the work-model plugin now. \
             Commission a role via `frame commission <path-to-role>.md --name … --goal …` instead."
        );
    }

    #[test]
    fn retired_stubs_exit_1() {
        assert_eq!(run_new_retired(), 1);
        assert_eq!(run_kill_retired(), 1);
        assert_eq!(run_attach_retired(), 1);
        assert_eq!(run_start_agent_retired(), 1);
    }
}
