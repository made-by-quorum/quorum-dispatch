//! Driver auto-detect (WP-B-CS-1, deliverable 1) — the per-verb I/O-mode resolver.
//!
//! The organizing principle (S-B-COMMAND-SURFACE-RULINGS, Pete-directed): **I/O
//! mode follows who DRIVES.** The surface auto-detects the caller from execution
//! context — `isatty(stdin AND stdout)` (a human at a terminal) plus the agent
//! env markers a live Claude Code session exports (`SB_SESSION_ID` / `CLAUDECODE`)
//! — and an explicit flag (`--headless` / `--interactive`) always overrides. No
//! second binary, no hidden mode.
//!
//! This module is the pure decision only: the `isatty` bool and the [`Env`] seam
//! are INJECTED, so the full TTY×env×flag matrix is unit-testable WITHOUT a real
//! TTY (the same effects-seam discipline as `dispatch::bootstrap`). The production
//! wiring is the thin [`resolve_driver_real`] (it calls `crate::tty`).

use dispatch::effects::Env;

/// Who drives this invocation's I/O — the per-verb mode the surface selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    /// Human at a terminal → the interactive (native TUI) surface.
    Human,
    /// Agent / pipe → the headless stream-json surface.
    Agent,
}

/// The explicit caller override parsed from a verb's launch flags. `--interactive`
/// forces [`Driver::Human`]; `--headless` forces [`Driver::Agent`]; absent → the
/// context auto-detect runs (the `ls --color=auto` / git-paging pattern: a
/// context-derived default with an explicit escape hatch, NOT hidden state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverOverride {
    None,
    Interactive,
    Headless,
}

impl DriverOverride {
    /// Resolve the override from the two mutually-exclusive launch flags. clap
    /// guards exclusivity for verbs that register both; the defensive both-set
    /// case resolves to [`Self::Headless`] — fail toward the machine surface (a
    /// stray-flagged agent never silently drops into an interactive TUI it has no
    /// TTY to drive), never the reverse.
    pub fn from_flags(headless: bool, interactive: bool) -> Self {
        if headless {
            Self::Headless
        } else if interactive {
            Self::Interactive
        } else {
            Self::None
        }
    }
}

/// The agent env markers (S-B rulings §"Detection signals"): a `sb` invocation
/// from INSIDE a live Claude Code session carries one of these. Either present
/// (and non-empty) ⇒ the caller is an agent, even at a TTY.
const AGENT_ENV_MARKERS: [&str; 2] = ["SB_SESSION_ID", "CLAUDECODE"];

/// Is an agent env marker present + non-empty? (An exported-but-empty marker is
/// treated as absent — a blanked var is not an agent claim.)
fn agent_marker_present(env: &dyn Env) -> bool {
    AGENT_ENV_MARKERS
        .iter()
        .any(|k| env.var(k).is_some_and(|v| !v.is_empty()))
}

/// Resolve the driver from the override, the isatty fact, and the env markers.
///
/// Order (S-B-COMMAND-SURFACE-RULINGS):
///   1. an explicit override always wins (`--headless` ⇒ Agent, `--interactive`
///      ⇒ Human);
///   2. else an agent env marker present ⇒ Agent — this BEATS the TTY (a Claude
///      session that itself runs `sb` may sit at a TTY, but it is still an agent
///      caller; the marker is the signal the daemon-spawn case cannot rely on
///      isatty for);
///   3. else a real TTY (stdin AND stdout) ⇒ Human (the interactive default);
///   4. else (a pipe / redirect, no marker) ⇒ Agent — headless on pipes (the
///      same non-TTY signal `claude -p` uses to skip its trust dialog).
pub fn resolve_driver(over: DriverOverride, is_tty: bool, env: &dyn Env) -> Driver {
    match over {
        DriverOverride::Headless => Driver::Agent,
        DriverOverride::Interactive => Driver::Human,
        DriverOverride::None => {
            if agent_marker_present(env) {
                Driver::Agent
            } else if is_tty {
                Driver::Human
            } else {
                Driver::Agent
            }
        }
    }
}

/// Production entry: resolve with the real `isatty(stdin AND stdout)` probe
/// (`crate::tty`). The pure [`resolve_driver`] above is the tested core.
pub fn resolve_driver_real(over: DriverOverride, env: &dyn Env) -> Driver {
    resolve_driver(over, crate::tty::stdin_and_stdout_are_tty(), env)
}

/// The route `sb start` takes once its driver is resolved (WP-B-CS-1, D2). The
/// pure decision the verb acts on — separated from the launch so the whole
/// {driver × prompt} matrix is unit-testable without a real daemon/claude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRoute {
    /// Human driver → today's interactive native-TUI create path, byte-unchanged.
    /// (No prompt is required — the TUI is driven live.)
    Interactive,
    /// Agent driver WITH a prompt → the headless stream-json launch (the
    /// `LaunchHeadless` client helper).
    Headless,
    /// Agent driver with NO prompt → Fork B teaching-error refusal: a headless
    /// `claude -p ""` is a degenerate no-op turn, so a bare agent start with no
    /// prompt is a usage error (agents that want work always pass `-p`).
    RefuseNoPrompt,
}

/// Resolve the `sb start` route from the driver and whether a prompt was given
/// (S-B-COMMAND-SURFACE-RULINGS + the D2/D3 SCOPE-RULING, Fork B). Human → the
/// interactive create path (prompt optional). Agent → headless when a prompt is
/// present, else [`StartRoute::RefuseNoPrompt`].
pub fn start_route(driver: Driver, has_prompt: bool) -> StartRoute {
    match driver {
        Driver::Human => StartRoute::Interactive,
        Driver::Agent if has_prompt => StartRoute::Headless,
        Driver::Agent => StartRoute::RefuseNoPrompt,
    }
}

/// The render surface `sb ls` selects (WP-B-CS-2, S-B rulings §"sb ls"): a TTY
/// human gets the **table**, a pipe/agent gets **JSON**, and an explicit `--json`
/// always overrides to JSON (the same context-default + explicit-escape pattern as
/// the start/resume driver). The pure decision, separated from the verb so the
/// {flag × driver} matrix is unit-testable without a real TTY.
///
/// WIRED into `ls::run_inner` (WP-B7 PIECE 1): the live piped default flipped
/// table→JSON (agent/pipe ⇒ JSON, human/TTY ⇒ table), a Pete-directed behavior
/// change carried with a recorded test-delta (the `sb resume` GUARDRAIL-2 golden-
/// delta pattern). `--json`/`--table` are the explicit overrides on this surface
/// axis (`--short` is a CONTENT modifier, not a selector — it never reaches here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LsRender {
    /// The human table (`render_table_human`).
    Table,
    /// The machine JSON surface (`render::ls_json*`).
    Json,
}

/// Resolve the `sb ls` render surface from the explicit `--json` flag and the
/// auto-detected driver (S-B rulings: auto table for a TTY, JSON for a pipe;
/// `--json` override wins). `--json` ⇒ JSON unconditionally; else Agent ⇒ JSON,
/// Human ⇒ Table. (Wired into `ls::run_inner`, WP-B7 PIECE 1 — see [`LsRender`].)
pub fn ls_render_mode(json_flag: bool, driver: Driver) -> LsRender {
    if json_flag {
        return LsRender::Json; // explicit override always wins
    }
    match driver {
        Driver::Agent => LsRender::Json,  // pipe / agent → machine surface
        Driver::Human => LsRender::Table, // TTY human → table
    }
}

/// `sb resume` is an AGENT verb: **ALWAYS headless** (Fork A full replacement). The
/// driver is accepted only to PROVE the decision ignores it — even a Human/TTY
/// context resumes headless (a human re-entering a session is `sb connect`
/// (WP-B-CS-2), NOT `sb resume`, so there is no `--interactive` escape on resume).
/// A regression that re-introduces a driver-keyed interactive branch flips one of
/// these rows and REDs [`tests::resume_is_always_headless_even_at_a_tty`].
pub fn resume_is_headless(_driver: Driver) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch::effects::MapEnv;
    use std::collections::HashMap;

    /// An env with NO agent markers (the human-shell case).
    fn bare_env() -> MapEnv {
        MapEnv {
            vars: HashMap::new(),
            uid: 501,
        }
    }

    /// An env carrying one named agent marker (non-empty).
    fn agent_env(key: &str) -> MapEnv {
        let mut vars = HashMap::new();
        vars.insert(key.to_string(), "ab3kx9mq".to_string());
        MapEnv { vars, uid: 501 }
    }

    // --- override-always-wins rows (1) --------------------------------------

    #[test]
    fn override_headless_wins_even_at_tty_with_no_marker() {
        // A human at a terminal who explicitly asked for --headless gets Agent.
        assert_eq!(
            resolve_driver(DriverOverride::Headless, true, &bare_env()),
            Driver::Agent
        );
    }

    #[test]
    fn override_interactive_wins_even_on_pipe_with_agent_marker() {
        // The strongest override case: NOT a TTY, an agent marker IS present, yet
        // --interactive forces Human. Override beats both context signals.
        assert_eq!(
            resolve_driver(
                DriverOverride::Interactive,
                false,
                &agent_env("SB_SESSION_ID")
            ),
            Driver::Human
        );
    }

    // --- agent-marker-beats-TTY rows (2) ------------------------------------

    #[test]
    fn agent_marker_beats_tty_sb_session_id() {
        // At a TTY, but inside a Claude session (SB_SESSION_ID) → Agent.
        assert_eq!(
            resolve_driver(DriverOverride::None, true, &agent_env("SB_SESSION_ID")),
            Driver::Agent
        );
    }

    #[test]
    fn agent_marker_beats_tty_claudecode() {
        // The other marker (CLAUDECODE) is equally sufficient, at a TTY → Agent.
        assert_eq!(
            resolve_driver(DriverOverride::None, true, &agent_env("CLAUDECODE")),
            Driver::Agent
        );
    }

    #[test]
    fn empty_marker_is_not_an_agent_claim() {
        // An exported-but-EMPTY marker is treated as absent: a blanked var is no
        // agent claim, so at a TTY this is the human default.
        let mut vars = HashMap::new();
        vars.insert("SB_SESSION_ID".to_string(), String::new());
        let env = MapEnv { vars, uid: 501 };
        assert_eq!(
            resolve_driver(DriverOverride::None, true, &env),
            Driver::Human
        );
    }

    // --- TTY-default row (3) ------------------------------------------------

    #[test]
    fn tty_no_marker_no_override_is_human() {
        assert_eq!(
            resolve_driver(DriverOverride::None, true, &bare_env()),
            Driver::Human
        );
    }

    // --- pipe-default row (4) -----------------------------------------------

    #[test]
    fn pipe_no_marker_no_override_is_agent() {
        // No TTY, no marker, no flag → headless on pipes.
        assert_eq!(
            resolve_driver(DriverOverride::None, false, &bare_env()),
            Driver::Agent
        );
    }

    // --- from_flags surface --------------------------------------------------

    #[test]
    fn from_flags_maps_each_flag_and_defaults_none() {
        assert_eq!(
            DriverOverride::from_flags(true, false),
            DriverOverride::Headless
        );
        assert_eq!(
            DriverOverride::from_flags(false, true),
            DriverOverride::Interactive
        );
        assert_eq!(
            DriverOverride::from_flags(false, false),
            DriverOverride::None
        );
        // Defensive both-set: headless wins (fail toward the machine surface).
        assert_eq!(
            DriverOverride::from_flags(true, true),
            DriverOverride::Headless
        );
    }

    // --- D2: start_route (the pure sb-start decision) -----------------------

    #[test]
    fn start_route_human_is_interactive_with_or_without_prompt() {
        // A human always takes the interactive create path; a prompt is optional
        // (the TUI is driven live), never a refusal.
        assert_eq!(start_route(Driver::Human, true), StartRoute::Interactive);
        assert_eq!(start_route(Driver::Human, false), StartRoute::Interactive);
    }

    #[test]
    fn start_route_agent_with_prompt_is_headless() {
        assert_eq!(start_route(Driver::Agent, true), StartRoute::Headless);
    }

    #[test]
    fn start_route_agent_without_prompt_refuses() {
        // Fork B: a bare agent/headless start with no -p is a usage error (a
        // headless `claude -p ""` is a degenerate no-op turn).
        assert_eq!(
            start_route(Driver::Agent, false),
            StartRoute::RefuseNoPrompt
        );
    }

    /// END-TO-END (resolver → router): an agent env marker at a TTY with no prompt
    /// routes to RefuseNoPrompt — the marker beats the TTY (Agent), then Fork B
    /// refuses. FIX-SHAPED MUTATION: if `start_route`'s no-prompt arm regressed to
    /// `Headless` (dropping Fork B) this flips to `Headless` and REDs.
    #[test]
    fn agent_marker_at_tty_no_prompt_routes_to_refuse() {
        let driver = resolve_driver(DriverOverride::None, true, &agent_env("SB_SESSION_ID"));
        assert_eq!(start_route(driver, false), StartRoute::RefuseNoPrompt);
    }

    /// END-TO-END: `--interactive` at a pipe with an agent marker present forces the
    /// interactive route (override beats both context signals) — a human escape
    /// hatch even from an agent-looking context.
    #[test]
    fn interactive_override_routes_to_interactive_even_with_agent_marker() {
        let driver = resolve_driver(DriverOverride::Interactive, false, &agent_env("CLAUDECODE"));
        assert_eq!(start_route(driver, false), StartRoute::Interactive);
    }

    // --- WP-B-CS-2: ls render-mode auto-detect (override wins) --------------

    /// `sb ls` mode matrix (S-B rulings §"sb ls"): `--json` always wins; else the
    /// driver decides (Agent/pipe → JSON, Human/TTY → table). FIX-SHAPED MUTATION:
    /// dropping the `--json` override (letting the driver decide even when the flag
    /// is set) would flip the `--json`-at-a-TTY row from JSON to Table and red the
    /// override rows.
    #[test]
    fn ls_render_mode_matrix() {
        // Override: --json wins regardless of driver.
        assert_eq!(ls_render_mode(true, Driver::Human), LsRender::Json);
        assert_eq!(ls_render_mode(true, Driver::Agent), LsRender::Json);
        // Auto-detect: human → table, agent → json.
        assert_eq!(ls_render_mode(false, Driver::Human), LsRender::Table);
        assert_eq!(ls_render_mode(false, Driver::Agent), LsRender::Json);
    }

    // --- D3: resume is ALWAYS headless (Fork A) -----------------------------

    /// Resume ignores the driver: even a Human driver at a TTY resumes headless
    /// (no `--interactive` escape — a human re-entering is `sb connect`). FIX-SHAPED
    /// MUTATION: re-introducing a driver-keyed interactive branch (e.g.
    /// `matches!(driver, Driver::Agent)`) flips the Human row to false and REDs.
    #[test]
    fn resume_is_always_headless_even_at_a_tty() {
        assert!(resume_is_headless(Driver::Human));
        assert!(resume_is_headless(Driver::Agent));
    }
}
