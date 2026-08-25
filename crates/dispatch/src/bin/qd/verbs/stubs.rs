//! Moved-stubs: the ported stderr pointers (commands/send.ts:524-528).
//!
//! (A5 M5: `qd ping` is now a REAL verb — its no-arg exit-3 validation moved to
//! `verbs/ping.rs` alongside the classifier wiring; this file keeps only the
//! relay moved-pointer stub, which IS the intended behavior.)
//!
//! The `qd send` moved stub that used to sit here — the "split into explicit
//! channels" menu naming send:pty/send:relay/send:http — is GONE. `send` routes
//! to `send_unified` (verbs/mod.rs), so the stub had no callers, and bare
//! `qd send` with `--carrier`/`--wait` is now the surface those three verbs are
//! being deprecated in favour of: dead code teaching the reverse of the truth.

/// `qd relay` moved stub (commands/send.ts:523-528): stderr pointer, exit 1.
pub fn run_relay_moved() -> i32 {
    eprintln!("\"qd relay\" has moved to \"qd send\".\n");
    eprintln!("  qd send <session> <message>          async (returns message ID)");
    eprintln!("  qd send <session> <message> --wait   block until reply\n");
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

/// The retired `qd start --agent` stub's exact stderr line. Role/agent CONTENT
/// (the `<name>.md` definitions the old static-agent path resolved under
/// `~/.quorum/dispatch/plugins/core/agents/`) now lives in the work-model PLUGIN; spawning a
/// role is start-and-call (`qd start` + `qf call` — the prime skill's recipe;
/// lifecycle-collapse spec A-4: `frame commission` is retired), not the
/// engine's native `--agent`. The engine refuses the flag with this teaching
/// error instead of resolving a path it no longer owns (the new/kill/attach
/// retirement pattern). Pinned by a unit test so the wording can't drift.
pub fn start_agent_retired_line() -> &'static str {
    "qd start --agent is retired: role/agent content lives in the work-model plugin now. \
     Spawn a role with `qd start --interactive <name>` and deliver its goal with \
     `qf call <name> --body-from <goal.md>` (see the prime skill)."
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
    // added the command-rename retirement with the same contract).
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
            start_agent_retired_line(),
            "qd start --agent is retired: role/agent content lives in the work-model plugin now. \
             Spawn a role with `qd start --interactive <name>` and deliver its goal with \
             `qf call <name> --body-from <goal.md>` (see the prime skill)."
        );
    }

    #[test]
    fn retired_stubs_exit_1() {
        assert_eq!(run_new_retired(), 1);
        assert_eq!(run_kill_retired(), 1);
        assert_eq!(run_start_agent_retired(), 1);
    }
}
