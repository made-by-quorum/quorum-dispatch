//! Driver auto-detect (WP-B-CS-1, deliverable 1) — the per-verb I/O-mode resolver.
//!
//! The organizing principle (S-B-COMMAND-SURFACE-RULINGS, Pete-directed): **I/O
//! mode follows who DRIVES.** The surface auto-detects the caller from execution
//! context — `isatty(stdin AND stdout)` (a human at a terminal) plus the agent
//! env markers a live Claude Code session exports (`QD_SESSION_ID` / `CLAUDECODE`)
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

/// The agent env markers (S-B rulings §"Detection signals"): a `qd` invocation
/// from INSIDE a live Claude Code session carries one of these. Either present
/// (and non-empty) ⇒ the caller is an agent, even at a TTY.
const AGENT_ENV_MARKERS: [&str; 2] = ["QD_SESSION_ID", "CLAUDECODE"];

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
///      session that itself runs `qd` may sit at a TTY, but it is still an agent
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

/// The route `qd start` takes once its driver is resolved (WP-B-CS-1, D2). The
/// pure decision the verb acts on — separated from the launch so the whole
/// {override × driver × prompt} matrix is unit-testable without a real
/// daemon/claude.
///
/// Only the claude PANE lane (`claude-code/mux-pane`) consults this at all; every
/// other lane returns above the route in `lifecycle.rs::run_start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRoute {
    /// The interactive native-TUI create path — the ONLY route on this lane that
    /// actually makes a session. Taken by a human, and (since 2026-08-26) by an
    /// AUTO-DETECTED agent caller too: see [`start_route`] for why. A prompt is
    /// optional either way — `-p` is delivered post-create by `deliver_prompt`
    /// under the 0/10/1 went-busy exit contract, not by a different launch.
    Interactive,
    /// EXPLICIT `--headless` WITH a prompt → P4DB drive-burn: the one-off `claude
    /// -p … --output-format stream-json` launch this route once took is REMOVED.
    /// The route survives as the answer `--headless` gets, and `run_start` answers
    /// it with a routing-level teaching refusal (dispatch does not spawn one-off
    /// `-p` stream-json runs). Nothing is spawned; exit 1.
    Headless,
    /// EXPLICIT `--headless` with NO prompt → Fork B teaching-error refusal: the
    /// headless surface's whole payload was a `claude -p` turn, so asking for it
    /// without a prompt names a degenerate no-op turn. Nothing is spawned; exit 1.
    /// Unreachable without the flag — a bare agent start creates now.
    RefuseNoPrompt,
}

/// Resolve the `qd start` route from the caller's explicit override, the resolved
/// driver, and whether a prompt was given (S-B-COMMAND-SURFACE-RULINGS + the
/// D2/D3 SCOPE-RULING, Fork B — as amended by ADR-0011's 2026-08-26 addendum).
///
/// # Why the OVERRIDE is an input and the driver alone is not
///
/// [`Driver::Agent`] arrives here two ways that used to be interchangeable and no
/// longer are: **detected** (an agent env marker, or a pipe) and **demanded**
/// (`--headless`). The resolved driver cannot tell them apart — that is the whole
/// reason this function takes `over` as well.
///
/// # Why a DETECTED agent now creates
///
/// Both agent arms below are pure refusals: the P4DB drive-burn removed the
/// one-off `claude -p` stream-json launch, so `Headless` spawns nothing and
/// `RefuseNoPrompt` never did. Once that drive was gone the auto-detect was not
/// choosing between two LANES any more — it was choosing which of two errors an
/// agent read for asking to start a session, on the one lane in the fleet that
/// answered that request with an error at all. Every other pane lane (codex's,
/// pi's, both ACP residents) takes a bare agent start and creates. So the claude
/// pane lane now does what its siblings do, and the refusals stay attached to the
/// flag that actually asks for the burned surface.
///
/// This does NOT hand an agent a terminal. The post-create handoff is a separate
/// decision — [`attaches_after_start`], resolved at the call site with
/// [`DriverOverride::None`] precisely so no flag can force it — and it still
/// answers `false` for every [`Driver::Agent`] caller. An auto-detected agent
/// gets a created, tracked, attachable session and its exit code back; the mux
/// pane stays where it was.
///
/// # The matrix
///
/// | `over` | `driver` | `has_prompt` | route |
/// |---|---|---|---|
/// | any | Human | either | [`StartRoute::Interactive`] |
/// | [`DriverOverride::Headless`] | Agent | yes | [`StartRoute::Headless`] |
/// | [`DriverOverride::Headless`] | Agent | no | [`StartRoute::RefuseNoPrompt`] |
/// | [`DriverOverride::None`] | Agent | either | [`StartRoute::Interactive`] |
///
/// `(DriverOverride::Interactive, Driver::Agent)` is structurally unreachable —
/// that override resolves to [`Driver::Human`] in [`resolve_driver`] — and falls
/// into the same create arm a detected agent takes, which is where it would want
/// to land anyway.
pub fn start_route(over: DriverOverride, driver: Driver, has_prompt: bool) -> StartRoute {
    match driver {
        Driver::Human => StartRoute::Interactive,
        // DEMANDED: the caller typed `--headless`, so it is asking for the surface
        // the drive-burn removed. Both answers are refusals, and the prompt only
        // picks which one explains it.
        Driver::Agent if over == DriverOverride::Headless => {
            if has_prompt {
                StartRoute::Headless
            } else {
                StartRoute::RefuseNoPrompt
            }
        }
        // DETECTED: an env marker or a pipe. Create, like every other pane lane.
        Driver::Agent => StartRoute::Interactive,
    }
}

/// FTUE punch **R19** — does `qd start` hand the terminal over once the session
/// is up?
///
/// # Why this is a decision and not `if is_tty`
///
/// `qd start wk` used to create a session and return, leaving the human staring
/// at a prompt and typing `qd attach wk` at a session they were already looking
/// at. Attaching is now the default — but "default" here has to mean *for a
/// human at a terminal*, and nothing else, because every other caller of `start`
/// is something an attach would break:
///
/// - an **agent** (`QD_SESSION_ID` / `CLAUDECODE` in its env) has no terminal to
///   give and no keystroke to leave one with; handing it a mux pane wedges the
///   turn that spawned the session;
/// - a **pipe** (`qd start … | tee`) is the same case without the marker;
/// - a **`-p` start** is by construction a programmatic first turn — the prompt
///   IS the interaction, and the exit code (0 / 10 / 1, spec §3.5) is the answer
///   the caller is waiting for. Attaching would replace that answer with a TUI.
///
/// So the driver, which already answers "who is driving this invocation", is the
/// input — and it is resolved with [`DriverOverride::None`] at the call site,
/// NOT from `--interactive`. That is load-bearing: the fleet's commissioning
/// recipe still spells its seats `qd start <name> --interactive` (it no longer
/// HAS to — see [`start_route`] — but it does, and every existing script keeps
/// working), so letting that flag force an attach would attach *the whole fleet*.
/// It is equally load-bearing the other way now that a bare agent start CREATES:
/// this predicate is the only thing standing between an auto-detected agent and
/// a mux pane it cannot leave, and it answers `false` for [`Driver::Agent`]
/// unconditionally.
///
/// `lane_attachable` is the fourth input because three of the nine lanes have no
/// terminal at all (`codex/daemon`, `pi/daemon`, `acp/*`): for them the create is
/// the whole story, and an attach attempt would only reach the daemon-redirect
/// error. `opted_out` is `--no-attach` (and `--headless`, which says the same
/// thing in the driver's vocabulary).
pub fn attaches_after_start(
    driver: Driver,
    opted_out: bool,
    has_prompt: bool,
    lane_attachable: bool,
) -> bool {
    !opted_out && !has_prompt && lane_attachable && driver == Driver::Human
}

/// The render surface `qd ls` selects (WP-B-CS-2, S-B rulings §"qd ls"): a TTY
/// human gets the **table**, a pipe/agent gets **JSON**, and an explicit `--json`
/// always overrides to JSON (the same context-default + explicit-escape pattern as
/// the start/resume driver). The pure decision, separated from the verb so the
/// {flag × driver} matrix is unit-testable without a real TTY.
///
/// WIRED into `ls::run_inner` (WP-B7 PIECE 1): the live piped default flipped
/// table→JSON (agent/pipe ⇒ JSON, human/TTY ⇒ table), a Pete-directed behavior
/// change carried with a recorded test-delta (the `qd resume` GUARDRAIL-2 golden-
/// delta pattern). `--json`/`--table` are the explicit overrides on this surface
/// axis (`--short` is a CONTENT modifier, not a selector — it never reaches here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LsRender {
    /// The human table (`render_table_human`).
    Table,
    /// The machine JSON surface (`render::ls_json*`).
    Json,
}

/// Resolve the `qd ls` render surface from the explicit `--json` flag and the
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
                &agent_env("QD_SESSION_ID")
            ),
            Driver::Human
        );
    }

    // --- agent-marker-beats-TTY rows (2) ------------------------------------

    #[test]
    fn agent_marker_beats_tty_qd_session_id() {
        // At a TTY, but inside a Claude session (QD_SESSION_ID) → Agent.
        assert_eq!(
            resolve_driver(DriverOverride::None, true, &agent_env("QD_SESSION_ID")),
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
        vars.insert("QD_SESSION_ID".to_string(), String::new());
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

    // --- D2: start_route (the pure qd-start decision) -----------------------

    #[test]
    fn start_route_human_is_interactive_with_or_without_prompt() {
        // A human always takes the interactive create path; a prompt is optional
        // (the TUI is driven live), never a refusal. The override is irrelevant on
        // this row — a Human driver can only have come from `--interactive` or the
        // auto-detect, and both create.
        for over in [DriverOverride::None, DriverOverride::Interactive] {
            assert_eq!(
                start_route(over, Driver::Human, true),
                StartRoute::Interactive
            );
            assert_eq!(
                start_route(over, Driver::Human, false),
                StartRoute::Interactive
            );
        }
    }

    #[test]
    fn start_route_explicit_headless_with_prompt_is_headless() {
        // `--headless -p …` asks for the surface the P4DB drive-burn removed, so it
        // gets the route whose only content is that refusal.
        assert_eq!(
            start_route(DriverOverride::Headless, Driver::Agent, true),
            StartRoute::Headless
        );
    }

    #[test]
    fn start_route_explicit_headless_without_prompt_refuses() {
        // Fork B, now scoped to the flag: `--headless` with no -p names a
        // degenerate no-op turn (a headless `claude -p ""`).
        assert_eq!(
            start_route(DriverOverride::Headless, Driver::Agent, false),
            StartRoute::RefuseNoPrompt
        );
    }

    /// THE FLIP (ADR-0011 addendum 2026-08-26). An agent caller the surface
    /// DETECTED — env marker or pipe, no flag — creates, with or without a prompt.
    ///
    /// Both agent refusals became pure teaching errors when the drive-burn removed
    /// the one-off `claude -p` stream-json launch, so the auto-detect was picking
    /// which error an agent read for asking to start a session — on the one pane
    /// lane in the fleet that answered that request with an error at all. FIX-SHAPED
    /// MUTATION: reinstating the driver-only routing (`Driver::Agent if has_prompt`
    /// ⇒ `Headless`, else `RefuseNoPrompt`) flips both rows here to refusals.
    #[test]
    fn start_route_detected_agent_creates_with_or_without_prompt() {
        assert_eq!(
            start_route(DriverOverride::None, Driver::Agent, true),
            StartRoute::Interactive
        );
        assert_eq!(
            start_route(DriverOverride::None, Driver::Agent, false),
            StartRoute::Interactive
        );
    }

    /// END-TO-END (resolver → router): an agent env marker at a TTY with no prompt
    /// routes to the CREATE path. The marker still beats the TTY (`Driver::Agent`,
    /// asserted here so the flip is not read as the detect having gone soft) — it
    /// is the ROUTE that changed, not the detection.
    #[test]
    fn agent_marker_at_tty_no_prompt_routes_to_the_create_path() {
        let driver = resolve_driver(DriverOverride::None, true, &agent_env("QD_SESSION_ID"));
        assert_eq!(driver, Driver::Agent);
        assert_eq!(
            start_route(DriverOverride::None, driver, false),
            StartRoute::Interactive
        );
        // …and the same context WITH the flag still reads both refusals, which is
        // the half of the old premise that survives.
        let demanded = resolve_driver(DriverOverride::Headless, true, &agent_env("QD_SESSION_ID"));
        assert_eq!(
            start_route(DriverOverride::Headless, demanded, false),
            StartRoute::RefuseNoPrompt
        );
        assert_eq!(
            start_route(DriverOverride::Headless, demanded, true),
            StartRoute::Headless
        );
    }

    /// END-TO-END: `--interactive` at a pipe with an agent marker present forces the
    /// interactive route (override beats both context signals) — a human escape
    /// hatch even from an agent-looking context.
    ///
    /// The PTY-LANE PREMISE this test used to carry is RETIRED (ADR-0011 addendum
    /// 2026-08-26). It said an agent-marked session that started WITHOUT
    /// `--interactive` auto-routed to `Headless` — a route no persistent fleet seat
    /// could ride — so every commissioned/relay/tracked start HAD to pass the flag.
    /// It no longer has to: a detected agent takes the same create path, asserted
    /// in the converse half below. What is still true, and is what this test now
    /// pins, is that the flag REMAINS an honest escape hatch — the prime recipe and
    /// every script that spells `--interactive` keep landing on the create lane,
    /// through the override rather than through the detect. If the override half
    /// regresses, a launch-time breakage of every primed agent is what this reports.
    #[test]
    fn interactive_override_routes_to_interactive_even_with_agent_marker() {
        let driver = resolve_driver(DriverOverride::Interactive, false, &agent_env("CLAUDECODE"));
        assert_eq!(
            start_route(DriverOverride::Interactive, driver, false),
            StartRoute::Interactive
        );
        // The converse half, restated for what it now says: the SAME agent-marked
        // context WITHOUT the override lands on the SAME route. Dropping the flag
        // from a recipe is no longer a launch-time breakage — it is a no-op.
        let bare = resolve_driver(DriverOverride::None, false, &agent_env("CLAUDECODE"));
        assert_eq!(
            start_route(DriverOverride::None, bare, true),
            StartRoute::Interactive
        );
        assert_eq!(
            start_route(DriverOverride::None, bare, false),
            StartRoute::Interactive
        );
    }

    /// The whole {override × driver × prompt} matrix in one place, including the
    /// structurally-unreachable `(Interactive, Agent)` corner — pinned so a future
    /// reader can see the arm it falls into rather than guessing.
    #[test]
    fn start_route_matrix() {
        use DriverOverride::*;
        let rows: &[(DriverOverride, Driver, bool, StartRoute)] = &[
            (None, Driver::Human, true, StartRoute::Interactive),
            (None, Driver::Human, false, StartRoute::Interactive),
            (Interactive, Driver::Human, true, StartRoute::Interactive),
            (Interactive, Driver::Human, false, StartRoute::Interactive),
            (Headless, Driver::Agent, true, StartRoute::Headless),
            (Headless, Driver::Agent, false, StartRoute::RefuseNoPrompt),
            (None, Driver::Agent, true, StartRoute::Interactive),
            (None, Driver::Agent, false, StartRoute::Interactive),
            // Unreachable in production (`--interactive` ⇒ Driver::Human); it
            // creates, which is where such a caller would want to land anyway.
            (Interactive, Driver::Agent, true, StartRoute::Interactive),
            (Interactive, Driver::Agent, false, StartRoute::Interactive),
        ];
        for (over, driver, has_prompt, want) in rows {
            assert_eq!(
                start_route(*over, *driver, *has_prompt),
                *want,
                "row ({over:?}, {driver:?}, has_prompt={has_prompt})"
            );
        }
    }

    // --- R19: attach-after-start (the post-create handoff decision) ---------

    /// The ONE row that attaches: a human, no opt-out, no `-p`, an attachable
    /// lane. Everything else in this matrix returns.
    #[test]
    fn attaches_after_start_only_for_a_bare_human_start_on_an_attachable_lane() {
        assert!(attaches_after_start(Driver::Human, false, false, true));
        // --no-attach / --headless veto.
        assert!(!attaches_after_start(Driver::Human, true, false, true));
        // -p is a programmatic first turn; its exit code is the answer.
        assert!(!attaches_after_start(Driver::Human, false, true, true));
        // A daemon lane has no terminal to hand over.
        assert!(!attaches_after_start(Driver::Human, false, false, false));
        // An agent caller NEVER attaches, however bare the invocation.
        assert!(!attaches_after_start(Driver::Agent, false, false, true));
    }

    /// FIX-SHAPED MUTATION, and the one that matters most: the fleet starts its
    /// seats with `qd start <name> --interactive` from inside an agent-marked
    /// session. If the attach decision were ever resolved through the
    /// `--interactive` override instead of [`DriverOverride::None`], every
    /// commissioned agent would be handed a mux pane it cannot leave. Compose the
    /// two halves here so the wiring, not just the predicate, is pinned.
    ///
    /// It matters MORE since the 2026-08-26 flip: a detected agent now reaches the
    /// create instead of a refusal, so this predicate is the only thing left
    /// between a bare `qd start` from inside a Claude session and that mux pane.
    #[test]
    fn agent_marked_interactive_start_does_not_attach() {
        let env = agent_env("QD_SESSION_ID");
        // What the call site actually asks: no override, so the marker decides.
        let driver = resolve_driver(DriverOverride::None, true, &env);
        assert_eq!(driver, Driver::Agent);
        assert!(!attaches_after_start(driver, false, false, true));
        // And the converse: a human at the same TTY with no marker DOES attach.
        let human = resolve_driver(DriverOverride::None, true, &bare_env());
        assert!(attaches_after_start(human, false, false, true));
    }

    // --- WP-B-CS-2: ls render-mode auto-detect (override wins) --------------

    /// `qd ls` mode matrix (S-B rulings §"qd ls"): `--json` always wins; else the
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

}
