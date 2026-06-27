//! WP-B-CS-2 — the HUMAN / observe command surface (`sb connect` against a
//! headless agent-driven session). The PURE, seam-testable core: it consumes the
//! daemon's `ServerMsg::Republish*` CONTROL FACTS and never the agent's assistant
//! text, models the read-only dashboard STATE, the 1/2/3 action menu, the
//! turn-boundary cutover PRIMITIVE (+ its input buffer), and the connect dispatch
//! discriminant. No I/O, no `claude`, no daemon — every decision is a pure fold so
//! the whole surface is unit-tested deterministically (the same effects-seam
//! discipline as `driver`/`liveness`).
//!
//! ## §2a — THE HARD LINE (observe-only, CONTROL FACTS ONLY)
//! The dashboard shows STATE — ready / working / idle / waiting / stuck, in-turn
//! vs idle, turn count, last result ok/error, last status. It NEVER shows the
//! agent's assistant text / output scrollback. The republish protocol carries no
//! content by design ([`qrmux::protocol::ServerMsg`] `Republish*` are the only
//! control facts); rendering content is the Claude-Code-lookalike trap and is NOT
//! built here. [`DashboardState::apply`] therefore has NO arm for the
//! content-bearing `ScreenUpdate`/`History`/`Passthrough` frames — they are
//! consumed by NOTHING, stored by NOTHING, rendered by NOTHING. Re-introducing a
//! content arm reds [`tests::no_assistant_text_path`].
//!
//! ## B5 boundary
//! A LIVE headless `sb start` writes no addressable registry row yet (B5, Fork C
//! ratified — `lifecycle.rs` "launched-but-not-yet-addressable until B5"). So the
//! connect dispatch ([`connect_dispatch`]) keys on a [`TargetMode`] DISCRIMINANT,
//! proven here with SYNTHETIC values; B5 populates the discriminant from the real
//! row. The renderer + cutover primitive are standalone, fed by synthetic frames.

use qrmux::protocol::ServerMsg;

// ===========================================================================
// Dashboard renderer — CONTROL FACTS ONLY (§2a)
// ===========================================================================

/// The last completed turn's result, mapped from `RepublishTurnEnd.is_error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnResult {
    Ok,
    Error,
}

impl TurnResult {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnResult::Ok => "ok",
            TurnResult::Error => "error",
        }
    }
}

/// The observe dashboard's STATE, folded from the `Republish*` control-fact
/// stream. CONTROL FACTS ONLY — there is deliberately no field that could hold
/// assistant text (§2a). `Default` is the pre-subscription state (nothing seen
/// yet), so a fresh dashboard renders an honest "starting" with no false signal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DashboardState {
    /// `RepublishReady` seen — the producer is up and has bound its identity.
    pub ready: bool,
    /// A turn is in flight (set by Ready / `status:busy`; cleared by TurnEnd /
    /// `status:idle` / End). The in-turn-vs-idle control fact.
    pub in_turn: bool,
    /// Completed turns observed (each `RepublishTurnEnd`).
    pub turn_count: u64,
    /// The last completed turn's result (ok/error), `None` until the first
    /// TurnEnd.
    pub last_result: Option<TurnResult>,
    /// The last completed turn's `stop_reason` (control metadata, e.g.
    /// `"end_turn"`), `None` until a TurnEnd carries one.
    pub last_stop_reason: Option<String>,
    /// The last coalescible `RepublishStatus` control status string (e.g.
    /// `"busy"`/`"idle"`), verbatim.
    pub last_status: Option<String>,
    /// Set by the terminal `RepublishEnd` — the stream's stop signal + outcome.
    pub ended: Option<String>,
    /// The producer's bound session id (from Ready / TurnEnd), for the header.
    pub session_id: Option<String>,
}

impl DashboardState {
    /// Fold ONE server frame into the dashboard state.
    ///
    /// Only the four `Republish*` control facts move state. EVERY other
    /// `ServerMsg` variant — including the content-bearing `ScreenUpdate`,
    /// `History`, and `Passthrough` — falls through the `_` arm and changes
    /// NOTHING: the §2a hard line. Do NOT add a content-rendering arm here.
    pub fn apply(&mut self, msg: &ServerMsg) {
        match msg {
            ServerMsg::RepublishReady { session_id } => {
                self.ready = true;
                // Ready ⇒ a turn is underway (sb wait's SubState presumes Busy on
                // subscribe — the producer republishes Ready at turn start).
                self.in_turn = true;
                self.session_id = Some(session_id.clone());
            }
            ServerMsg::RepublishTurnEnd {
                session_id,
                is_error,
                stop_reason,
            } => {
                self.in_turn = false;
                self.turn_count += 1;
                self.last_result = Some(if *is_error {
                    TurnResult::Error
                } else {
                    TurnResult::Ok
                });
                self.last_stop_reason = stop_reason.clone();
                self.session_id.get_or_insert_with(|| session_id.clone());
            }
            ServerMsg::RepublishStatus { status } => {
                self.last_status = Some(status.clone());
                match status.as_str() {
                    "busy" => self.in_turn = true,
                    "idle" => self.in_turn = false,
                    // Any other control status is recorded verbatim above but does
                    // not assert an in-turn transition (fail-closed: don't guess).
                    _ => {}
                }
            }
            ServerMsg::RepublishEnd { outcome } => {
                self.ended = Some(outcome.clone());
                self.in_turn = false;
            }
            // §2a HARD LINE: no content path. ScreenUpdate/History/Passthrough and
            // every other frame are NOT control facts — consumed by nothing.
            _ => {}
        }
    }

    /// The single STATE word derived from the stream facts (NOT the agent's text).
    /// `ended` wins (terminal); then in-turn ⇒ working; a ready idle producer ⇒
    /// idle; else starting (pre-Ready). The liveness/ledger overlay (waiting/stuck)
    /// is layered by [`render`] when supplied — the stream alone cannot see a
    /// `--wait` or the stuck timeout.
    pub fn stream_state(&self) -> &'static str {
        if self.ended.is_some() {
            "ended"
        } else if self.in_turn {
            "working"
        } else if self.ready {
            "idle"
        } else {
            "starting"
        }
    }

    /// Render the read-only dashboard — CONTROL FACTS ONLY (§2a). `overlay` is the
    /// optional liveness/ledger readiness word (e.g. `"ready"`/`"waiting"`/
    /// `"stuck"`) the dashboard layers over the stream state; `None` shows the
    /// stream state alone. The output contains ONLY state tokens — never any
    /// assistant text (there is no field that could carry it).
    pub fn render(&self, overlay: Option<&str>) -> String {
        let sid = self.session_id.as_deref().unwrap_or("-");
        let state = match overlay {
            Some(o) => format!("{} ({o})", self.stream_state()),
            None => self.stream_state().to_string(),
        };
        let in_turn = if self.in_turn { "yes" } else { "no" };
        let last_result = self.last_result.map(|r| r.as_str()).unwrap_or("-");
        let last_status = self.last_status.as_deref().unwrap_or("-");
        format!(
            "session: {sid}\n\
             state: {state}\n\
             in-turn: {in_turn}\n\
             turns: {}\n\
             last result: {last_result}\n\
             last status: {last_status}",
            self.turn_count
        )
    }
}

// ===========================================================================
// The 1 / 2 / 3 action menu (S-B rulings — the headless-target menu)
// ===========================================================================

/// The well-labelled action menu offered when `sb connect` lands on a headless
/// (agent-driven) target (S-B-COMMAND-SURFACE-RULINGS §"sb connect").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    /// 1 — send a message via relay (async, non-disruptive; the agent stays
    /// headless and keeps working).
    RelayMessage,
    /// 2 — stop & resume → cut over to DRIVE, with the option to BUFFER input
    /// (delivered the instant the session flips interactive). The cutover is
    /// TIMING-gated to a turn boundary, never role-gated.
    CutOverToDrive,
    /// 3 — just watch (stay read-only).
    JustWatch,
}

/// Parse a menu selection. Accepts the bare digit (`"1"`/`"2"`/`"3"`), trimming
/// surrounding whitespace; anything else is `None` (the caller re-prompts).
pub fn parse_menu_choice(input: &str) -> Option<MenuChoice> {
    match input.trim() {
        "1" => Some(MenuChoice::RelayMessage),
        "2" => Some(MenuChoice::CutOverToDrive),
        "3" => Some(MenuChoice::JustWatch),
        _ => None,
    }
}

/// The concrete action a menu selection dispatches to — what the live observe
/// driver acts on. Pure (no I/O), so the selection→action mapping is unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserveAction {
    /// 1 — send a relay message (async; the agent stays headless + keeps working).
    SendRelay,
    /// 2 — cut over to DRIVE: carries the cutover gate's timing decision (fire now
    /// at a boundary vs deferred to the next `RepublishTurnEnd`).
    CutOver(CutoverDecision),
    /// 3 — stay read-only.
    Watch,
}

/// Map a 1/2/3 menu selection to its action. Choice 2 (cut over) consults the
/// turn-boundary [`CutoverGate`] — the cut-over is TIMING-gated, so its action
/// carries whether it fires now or is deferred; `buffered_input` is queued for
/// delivery-first when it fires. Choices 1 and 3 are immediate. Pure decision —
/// the live driver performs the relay send / native-TUI attach.
pub fn menu_action(
    choice: MenuChoice,
    gate: &mut CutoverGate,
    buffered_input: Option<String>,
) -> ObserveAction {
    match choice {
        MenuChoice::RelayMessage => ObserveAction::SendRelay,
        MenuChoice::CutOverToDrive => ObserveAction::CutOver(gate.request_cutover(buffered_input)),
        MenuChoice::JustWatch => ObserveAction::Watch,
    }
}

/// The menu text rendered under the dashboard for a headless target.
pub fn menu_text() -> &'static str {
    "Choose:\n  \
     1) Send a message via relay (async — the agent keeps working)\n  \
     2) Stop & resume → cut over to drive (option to buffer input)\n  \
     3) Just watch (read-only)"
}

// ===========================================================================
// Turn-boundary cutover PRIMITIVE (+ input buffer)
// ===========================================================================

/// The cutover request's disposition — the ENFORCED INVARIANT is NON-DISRUPTIVE
/// TIMING (pause only at a turn boundary, the `result` point), NOT a role guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutoverDecision {
    /// The request landed mid-turn → it is held until the next turn boundary
    /// (`RepublishTurnEnd`). It does NOT fire now.
    DeferredToBoundary,
    /// The request landed at a boundary / idle producer → it fires now.
    FireNow,
}

/// What a fired cutover carries: the buffered input to deliver FIRST (the instant
/// the session flips interactive), then the native-TUI attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutoverFire {
    /// The optional buffered input, delivered BEFORE the human starts driving.
    pub buffered_input: Option<String>,
}

/// The turn-boundary cutover gate — a PURE state machine. It tracks in-turn-ness
/// from the `Republish*` stream and gates a cutover request to the next turn
/// boundary. There is DELIBERATELY no role/identity input: the cutover is
/// available for ANY session (incl. a mid-build implementer); only the TIMING is
/// enforced (v2 ruling — "enforce the timing, not the role"). Adding a role guard
/// would require a parameter this type does not have — [`tests::cutover_is_not_role_gated`]
/// pins its absence.
#[derive(Debug, Clone, Default)]
pub struct CutoverGate {
    /// True while a turn is in flight (no cutover may fire).
    in_turn: bool,
    /// `Some(buffered_input)` once a cutover has been requested and not yet fired.
    pending: Option<Option<String>>,
    /// True once the stream is terminal (`RepublishEnd`) — a pending request can
    /// fire (the turn is definitively over).
    ended: bool,
}

impl CutoverGate {
    /// A gate that starts mid-turn (the common case: `sb connect` lands on a
    /// session whose turn is already in flight). Use [`Self::default`] for an
    /// idle-start gate.
    pub fn in_turn() -> Self {
        Self {
            in_turn: true,
            pending: None,
            ended: false,
        }
    }

    /// Fold the SAME `Republish*` control facts that drive the dashboard, to track
    /// the turn boundary. Ready / `status:busy` ⇒ in-turn; TurnEnd / `status:idle`
    /// ⇒ boundary reached; End ⇒ terminal boundary.
    pub fn observe(&mut self, msg: &ServerMsg) {
        match msg {
            ServerMsg::RepublishReady { .. } => self.in_turn = true,
            ServerMsg::RepublishTurnEnd { .. } => self.in_turn = false,
            ServerMsg::RepublishStatus { status } => match status.as_str() {
                "busy" => self.in_turn = true,
                "idle" => self.in_turn = false,
                _ => {}
            },
            ServerMsg::RepublishEnd { .. } => {
                self.in_turn = false;
                self.ended = true;
            }
            _ => {}
        }
    }

    /// Request a cutover, optionally buffering `input` for delivery-first. Records
    /// the request and reports whether it fires now (at a boundary / terminal) or
    /// is deferred to the next boundary (mid-turn). No role is consulted — TIMING
    /// only.
    pub fn request_cutover(&mut self, input: Option<String>) -> CutoverDecision {
        self.pending = Some(input);
        if self.at_boundary() {
            CutoverDecision::FireNow
        } else {
            CutoverDecision::DeferredToBoundary
        }
    }

    /// Poll for a fire-able cutover. Returns `Some(CutoverFire)` exactly once, when
    /// a request is pending AND the producer is at a boundary (`!in_turn`) — the
    /// boundary gate. While in-turn it returns `None` (the request stays deferred).
    ///
    /// THE BOUNDARY GATE (`self.at_boundary()`) is load-bearing: dropping it so a
    /// pending request fires regardless of `in_turn` is the fix-shaped mutation
    /// that makes a mid-turn cutover fire — it reds
    /// [`tests::mid_turn_cutover_is_deferred_then_fires_at_boundary`].
    pub fn poll_fire(&mut self) -> Option<CutoverFire> {
        if self.pending.is_some() && self.at_boundary() {
            let buffered_input = self.pending.take().flatten();
            Some(CutoverFire { buffered_input })
        } else {
            None
        }
    }

    /// Is the producer at a safe cutover point — a turn boundary (idle) or the
    /// terminal end? This is the ONLY thing that gates a fire (no role).
    pub fn at_boundary(&self) -> bool {
        !self.in_turn || self.ended
    }

    /// Is a cutover request outstanding (requested, not yet fired)?
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

// ===========================================================================
// Connect dispatch discriminant (B5-deferred live row population)
// ===========================================================================

/// How a resolved `sb connect` target is hosted / driven — the discriminant the
/// dispatch branches on (S-B rulings §"sb connect": mode auto-follows the target).
/// B5 populates this from the real registry row (live headless addressability);
/// today it is a SYNTHETIC value, since a headless `sb start` writes no addressable
/// row yet (lifecycle.rs defers identity to B5). The renderer + cutover primitive
/// are proven against synthetic frames regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMode {
    /// Human-driven interactive/TUI session (a live mux pane) → attach to DRIVE
    /// directly (today's connect-to-live path).
    InteractiveTui,
    /// Agent-driven HEADLESS (stream-json) session → OBSERVE mode (the read-only
    /// dashboard + 1/2/3 menu). The B5 surface: no row resolves to this yet.
    HeadlessAgent,
    /// No live pane / producer (cold) → auto-revive then attach (today's
    /// connect-cold path).
    Cold,
}

/// The action `sb connect` takes for a resolved target — the pure decision keyed
/// on [`TargetMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectAction {
    /// Attach the live native TUI and DRIVE (today's live-pane behavior).
    AttachDrive,
    /// Enter the read-only OBSERVE dashboard + menu (the B-CS-2 surface).
    Observe,
    /// Auto-revive the cold session, then attach (today's cold behavior).
    ReviveThenAttach,
}

/// `sb connect`'s mode dispatch (S-B rulings): the human verb's I/O follows the
/// target. A pure discriminant decision — coherent without B5's row shape, so it
/// is built + tested now (synthetic `TargetMode`), and B5 only has to feed the
/// real discriminant in.
pub fn connect_dispatch(mode: TargetMode) -> ConnectAction {
    match mode {
        TargetMode::InteractiveTui => ConnectAction::AttachDrive,
        TargetMode::HeadlessAgent => ConnectAction::Observe,
        TargetMode::Cold => ConnectAction::ReviveThenAttach,
    }
}

/// The `entrypoint` discriminant a headless `sb start` stamps on its registry row
/// (WP-B5-i). This is the row field [`resolve_target_mode`] keys on to route a
/// live agent-driven session into observe; the daemon-mint side
/// (`daemon_status::MintIdentity`) writes it. Interactive rows leave `entrypoint`
/// absent. Canonical home is HERE (with the dispatch it feeds), so the minter and
/// the resolver cannot drift.
pub const HEADLESS_ENTRYPOINT: &str = "headless";

/// WP-B5-i — the LIVE `sb connect` resolver (the deferred B-CS-2 wiring): map a
/// resolved registry row to its [`TargetMode`] from the row's hosting/mode
/// discriminant + liveness, so `connect` routes a target by what it actually is.
///
/// - a LIVE headless agent (the [`HEADLESS_ENTRYPOINT`] marker + an alive pid) →
///   [`TargetMode::HeadlessAgent`] (→ [`ConnectAction::Observe`]). Headless
///   sessions have no mux pane, so the pre-B5 `attach_resolved` misread them as
///   Cold (revive); this arm is what makes them observe instead.
/// - a live interactive pane (no headless marker, a live pane) →
///   [`TargetMode::InteractiveTui`] (→ attach-drive).
/// - anything not live → [`TargetMode::Cold`] (→ revive-then-attach), INCLUDING a
///   headless row whose pid is dead (revive owns the cold path; a dead headless
///   row is not observable).
///
/// Fix-shaped mutation: drop the `entrypoint == Some(HEADLESS_ENTRYPOINT)` guard
/// and a live headless row falls through to `Cold` — `connect` would try to revive
/// a session that is already live, the exact regression the test pins.
pub fn resolve_target_mode(
    entrypoint: Option<&str>,
    pid_alive: bool,
    has_live_pane: bool,
) -> TargetMode {
    if entrypoint == Some(HEADLESS_ENTRYPOINT) && pid_alive {
        TargetMode::HeadlessAgent
    } else if has_live_pane {
        TargetMode::InteractiveTui
    } else {
        TargetMode::Cold
    }
}

// ===========================================================================
// The live observe run-loop core — turn-boundary cutover EXECUTION (WP-B-CS-2-LIVE)
// ===========================================================================

/// One observe tick over the cutover gate: fold the next `Republish*` control
/// fact into the gate's turn-boundary tracking, then check whether a pending
/// cutover is now releasable. Returns `Some(CutoverFire)` exactly when a request
/// is pending AND the producer has reached a boundary (`!in_turn || ended`) — i.e.
/// the SAME boundary gate the banked [`CutoverGate::poll_fire`] enforces. The live
/// run-loop calls this for EVERY frame it drains off the subscriber, so a cutover
/// can fire ONLY at a turn boundary, NEVER mid-turn.
///
/// This is the shared per-frame primitive the run-loop and its deterministic
/// fixture both drive, so the test exercises the engine's real control flow rather
/// than a re-implementation. §2a-safe by construction: it only consults the gate's
/// turn-boundary state, which folds control facts only (content frames are inert in
/// [`CutoverGate::observe`]).
pub fn observe_tick(gate: &mut CutoverGate, frame: &ServerMsg) -> Option<CutoverFire> {
    gate.observe(frame);
    gate.poll_fire()
}

/// The effects a fired cutover performs, factored behind a seam so the EXECUTION
/// LOGIC (ordering, buffered-input-first) is proven DETERMINISTICALLY with a fake
/// impl while the real impl drives the actual teardown + `revive_claude`, delivery,
/// and attach. Every method is fallible; [`execute_cutover`] short-circuits on the
/// first error (a half-done cutover is reported, never silently continued).
pub trait CutoverExec {
    /// The drivable session handle [`Self::revive_to_drive`] produces (the real
    /// impl's revive handle; the fake's a marker). Opaque to the orchestrator.
    type Handle;
    /// The failure type a step reports (the real impl's process exit code).
    type Err;

    /// STEP 1 — tear down the live HEADLESS session: reap the agent-driven claude
    /// child (and its per-session daemon) so the SAME claude session can be revived
    /// into a native TUI without two producers racing the one transcript.
    fn teardown_headless(&mut self) -> Result<(), Self::Err>;

    /// STEP 2 — revive the session to a DRIVABLE native TUI (resume the SAME claude
    /// session id the headless child wrote), detached + ready to attach.
    fn revive_to_drive(&mut self) -> Result<Self::Handle, Self::Err>;

    /// STEP 3 — deliver the buffered operator input, if any, BEFORE the human starts
    /// driving (the buffered-input-FIRST guarantee). A `None` buffer is a no-op.
    fn deliver_buffered_input(
        &mut self,
        handle: &Self::Handle,
        input: Option<&str>,
    ) -> Result<(), Self::Err>;

    /// STEP 4 — attach the now-live pane and hand the human the wheel. Consumes the
    /// handle and returns the attach process's exit code.
    fn attach(&mut self, handle: Self::Handle) -> Result<i32, Self::Err>;
}

/// Drive a fired cutover through its four steps IN THE LOAD-BEARING ORDER:
/// teardown the headless session → revive to a drivable TUI → deliver the buffered
/// input FIRST → attach. The ordering is the invariant the fixture pins:
///
/// - teardown BEFORE revive (one producer on the transcript at a time);
/// - the buffered input is delivered BEFORE attach, so it lands the instant the
///   session flips interactive and ahead of anything the human types.
///
/// Fix-shaped mutations the deterministic test flips: reorder deliver after attach
/// (buffered input no longer lands first → red); drop the teardown call (revive
/// races a live headless producer → red). Pure orchestration (no I/O); the effects
/// live behind [`CutoverExec`].
pub fn execute_cutover<E: CutoverExec>(fire: CutoverFire, exec: &mut E) -> Result<i32, E::Err> {
    exec.teardown_headless()?;
    let handle = exec.revive_to_drive()?;
    exec.deliver_buffered_input(&handle, fire.buffered_input.as_deref())?;
    exec.attach(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(id: &str) -> ServerMsg {
        ServerMsg::RepublishReady {
            session_id: id.to_string(),
        }
    }
    fn turn_end(id: &str, is_error: bool) -> ServerMsg {
        ServerMsg::RepublishTurnEnd {
            session_id: id.to_string(),
            is_error,
            stop_reason: Some("end_turn".to_string()),
        }
    }
    fn status(s: &str) -> ServerMsg {
        ServerMsg::RepublishStatus {
            status: s.to_string(),
        }
    }
    fn end(outcome: &str) -> ServerMsg {
        ServerMsg::RepublishEnd {
            outcome: outcome.to_string(),
        }
    }

    // --- dashboard renderer: synthetic frames → STATE -----------------------

    /// A synthetic `Republish*` sequence folds into the expected STATE — and the
    /// rendered dashboard carries the control facts, never assistant text.
    #[test]
    fn dashboard_folds_control_facts_to_state() {
        let mut d = DashboardState::default();
        assert_eq!(d.stream_state(), "starting"); // nothing seen yet

        d.apply(&ready("sess-1"));
        assert!(d.ready && d.in_turn);
        assert_eq!(d.stream_state(), "working");
        assert_eq!(d.session_id.as_deref(), Some("sess-1"));

        d.apply(&turn_end("sess-1", false));
        assert!(!d.in_turn);
        assert_eq!(d.turn_count, 1);
        assert_eq!(d.last_result, Some(TurnResult::Ok));
        assert_eq!(d.stream_state(), "idle");

        // A second turn: busy → idle(error).
        d.apply(&status("busy"));
        assert_eq!(d.stream_state(), "working");
        d.apply(&turn_end("sess-1", true));
        assert_eq!(d.turn_count, 2);
        assert_eq!(d.last_result, Some(TurnResult::Error));

        d.apply(&end("Completed"));
        assert_eq!(d.ended.as_deref(), Some("Completed"));
        assert_eq!(d.stream_state(), "ended");

        let out = d.render(None);
        assert!(out.contains("session: sess-1"));
        assert!(out.contains("turns: 2"));
        assert!(out.contains("last result: error"));
    }

    /// The overlay (liveness/ledger readiness word) layers over the stream state
    /// without inventing a content path.
    #[test]
    fn dashboard_render_overlays_readiness_word() {
        let mut d = DashboardState::default();
        d.apply(&status("busy"));
        let out = d.render(Some("waiting"));
        assert!(out.contains("state: working (waiting)"), "{out}");
    }

    /// §2a HARD LINE — the false-positive guard. A content-bearing frame
    /// (ScreenUpdate / History / Passthrough) carrying assistant text moves NO
    /// state and is NEVER rendered as scrollback. Re-introducing a content arm in
    /// `apply` (the forbidden mutation) reds this.
    #[test]
    fn no_assistant_text_path() {
        let secret = "SECRET ASSISTANT SCROLLBACK that must never render";
        let mut d = DashboardState::default();
        d.apply(&ready("s")); // a legitimate control fact, so the dashboard is live
        let before = d.clone();

        // Feed every content-bearing frame the protocol can carry.
        d.apply(&ServerMsg::ScreenUpdate(secret.as_bytes().to_vec()));
        d.apply(&ServerMsg::History(vec![secret.as_bytes().to_vec()]));
        d.apply(&ServerMsg::Passthrough(secret.as_bytes().to_vec()));
        d.apply(&ServerMsg::Error(secret.to_string()));

        // State is unchanged by content frames, and the render never leaks the text.
        assert_eq!(d, before, "content frames must not move dashboard state");
        let out = d.render(None);
        assert!(
            !out.contains("SECRET"),
            "the dashboard must NEVER render assistant text: {out}"
        );
    }

    // --- menu ---------------------------------------------------------------

    #[test]
    fn menu_parses_1_2_3_and_rejects_others() {
        assert_eq!(parse_menu_choice("1"), Some(MenuChoice::RelayMessage));
        assert_eq!(parse_menu_choice(" 2 "), Some(MenuChoice::CutOverToDrive));
        assert_eq!(parse_menu_choice("3\n"), Some(MenuChoice::JustWatch));
        for bad in ["", "0", "4", "x", "12", "drive"] {
            assert_eq!(parse_menu_choice(bad), None, "{bad:?} must not parse");
        }
    }

    /// §6 — the 1/2/3 menu → ACTION dispatch. Choice 1 → relay; choice 3 → watch;
    /// choice 2 → cut over, carrying the cutover gate's TIMING decision (deferred
    /// mid-turn, fire-now at a boundary) + buffering the input for delivery-first.
    #[test]
    fn menu_dispatches_selection_to_action() {
        // 1 → relay (no gate interaction).
        let mut gate = CutoverGate::in_turn();
        assert_eq!(
            menu_action(MenuChoice::RelayMessage, &mut gate, None),
            ObserveAction::SendRelay
        );
        assert!(!gate.is_pending(), "relay must not arm a cutover");

        // 3 → watch.
        assert_eq!(
            menu_action(MenuChoice::JustWatch, &mut gate, None),
            ObserveAction::Watch
        );

        // 2 mid-turn → cut over, DEFERRED to the boundary, input buffered.
        let act = menu_action(MenuChoice::CutOverToDrive, &mut gate, Some("queued".into()));
        assert_eq!(
            act,
            ObserveAction::CutOver(CutoverDecision::DeferredToBoundary)
        );
        assert!(gate.is_pending());
        gate.observe(&turn_end("s", false));
        assert_eq!(
            gate.poll_fire().unwrap().buffered_input.as_deref(),
            Some("queued")
        );

        // 2 at idle → cut over, FIRE NOW.
        let mut idle = CutoverGate::default();
        assert_eq!(
            menu_action(MenuChoice::CutOverToDrive, &mut idle, None),
            ObserveAction::CutOver(CutoverDecision::FireNow)
        );
    }

    // --- turn-boundary cutover primitive ------------------------------------

    /// §6 DoD — RED-before / GREEN-after for the cutover boundary. A mid-turn
    /// request is DEFERRED and does NOT fire while in-turn; once the
    /// `RepublishTurnEnd` boundary lands it fires, delivering the buffered input
    /// first. The boundary gate (`at_boundary`) is the load-bearing invariant —
    /// dropping it (the fix-shaped mutation: fire regardless of `in_turn`) makes
    /// the mid-turn poll fire and reds this test.
    #[test]
    fn mid_turn_cutover_is_deferred_then_fires_at_boundary() {
        let mut gate = CutoverGate::in_turn();
        gate.observe(&ready("s")); // confirm in-turn

        // RED-before posture: request mid-turn → deferred, and poll does NOT fire.
        let decision = gate.request_cutover(Some("buffered hello".to_string()));
        assert_eq!(decision, CutoverDecision::DeferredToBoundary);
        assert!(
            gate.poll_fire().is_none(),
            "a mid-turn cutover must NOT fire (the boundary gate)"
        );
        assert!(gate.is_pending());

        // GREEN-after: the turn boundary lands → the cutover fires, buffer first.
        gate.observe(&turn_end("s", false));
        let fire = gate.poll_fire().expect("cutover fires at the boundary");
        assert_eq!(fire.buffered_input.as_deref(), Some("buffered hello"));
        // It fires exactly once.
        assert!(gate.poll_fire().is_none(), "cutover fires once");
        assert!(!gate.is_pending());
    }

    /// A request made when ALREADY at a boundary (idle producer) fires immediately.
    #[test]
    fn idle_cutover_fires_now() {
        let mut gate = CutoverGate::default(); // idle-start
        let decision = gate.request_cutover(None);
        assert_eq!(decision, CutoverDecision::FireNow);
        let fire = gate.poll_fire().expect("fires now");
        assert_eq!(fire.buffered_input, None);
    }

    /// A `status:busy` re-opens the turn after a boundary: a request between
    /// `busy` frames stays deferred until `idle`/TurnEnd. Pins that the gate
    /// tracks the LIVE boundary, not a one-shot.
    #[test]
    fn cutover_tracks_live_boundary_across_status_frames() {
        let mut gate = CutoverGate::default();
        gate.observe(&status("busy"));
        assert!(!gate.at_boundary());
        assert_eq!(
            gate.request_cutover(None),
            CutoverDecision::DeferredToBoundary
        );
        assert!(gate.poll_fire().is_none());
        gate.observe(&status("idle"));
        assert!(gate.poll_fire().is_some(), "idle is a boundary");
    }

    /// §6 DoD — ALL-SESSIONS-ALLOWED: the cutover is TIMING-gated, never
    /// role-gated. The gate exposes no role/identity input at all; the SAME
    /// timing behavior holds for what would be a mid-build implementer. Re-adding
    /// a supervisors-only (or any role) guard would need a parameter this type
    /// does not have — its ABSENCE is what makes this pass. (A role guard that
    /// refused a non-supervisor here would red it.)
    #[test]
    fn cutover_is_not_role_gated() {
        // "A mid-build implementer" — there is no role to pass; only timing.
        let mut gate = CutoverGate::in_turn();
        assert_eq!(
            gate.request_cutover(Some("drive me".into())),
            CutoverDecision::DeferredToBoundary,
            "deferral is by TIMING (in-turn), not role"
        );
        gate.observe(&turn_end("impl", false));
        let fire = gate
            .poll_fire()
            .expect("ANY session's cutover fires at the boundary — no role guard");
        assert_eq!(fire.buffered_input.as_deref(), Some("drive me"));
    }

    /// The terminal `RepublishEnd` is also a boundary — a pending request fires.
    #[test]
    fn cutover_fires_at_terminal_end() {
        let mut gate = CutoverGate::in_turn();
        gate.request_cutover(None);
        assert!(gate.poll_fire().is_none());
        gate.observe(&end("Completed"));
        assert!(gate.at_boundary());
        assert!(gate.poll_fire().is_some());
    }

    // --- connect dispatch (synthetic discriminant) --------------------------

    /// §6 — the connect dispatch keyed on the SYNTHETIC `TargetMode` discriminant:
    /// interactive → drive, headless → observe, cold → revive-then-attach. Proven
    /// without B5's row shape (a pure discriminant decision).
    #[test]
    fn connect_dispatch_routes_by_mode() {
        assert_eq!(
            connect_dispatch(TargetMode::InteractiveTui),
            ConnectAction::AttachDrive
        );
        assert_eq!(
            connect_dispatch(TargetMode::HeadlessAgent),
            ConnectAction::Observe
        );
        assert_eq!(
            connect_dispatch(TargetMode::Cold),
            ConnectAction::ReviveThenAttach
        );
    }

    /// WP-B5-i — the LIVE resolver end-to-end (row discriminant → mode → action),
    /// the §6 DoD line. A live headless row OBSERVES; a live interactive pane
    /// attach-drives; a dead headless row falls to revive (cold). This is the
    /// decision `attach_resolved` runs over the real registry row.
    #[test]
    fn resolve_target_mode_routes_headless_to_observe() {
        // Live headless agent → HeadlessAgent → Observe (the headline).
        let m = resolve_target_mode(Some(HEADLESS_ENTRYPOINT), true, false);
        assert_eq!(m, TargetMode::HeadlessAgent);
        assert_eq!(connect_dispatch(m), ConnectAction::Observe);

        // Live interactive pane (no headless marker) → InteractiveTui → AttachDrive.
        let m = resolve_target_mode(None, true, true);
        assert_eq!(m, TargetMode::InteractiveTui);
        assert_eq!(connect_dispatch(m), ConnectAction::AttachDrive);

        // A DEAD headless row → Cold (revive owns it; not observable).
        assert_eq!(
            resolve_target_mode(Some(HEADLESS_ENTRYPOINT), false, false),
            TargetMode::Cold
        );
        // No marker, no live pane → Cold.
        assert_eq!(resolve_target_mode(None, false, false), TargetMode::Cold);
    }

    /// Fix-shaped mutation guard: the headless arm REQUIRES the marker. A row with
    /// NO marker but a live pid+pane must NOT observe — it attach-drives. (Dropping
    /// the `entrypoint == headless` guard would flip a marker-less live row, or send
    /// a live headless row to Cold — both regressions this pins.)
    #[test]
    fn resolve_target_mode_marker_is_load_bearing() {
        // marker-less but live with a pane → interactive, NEVER observe.
        assert_ne!(
            resolve_target_mode(None, true, true),
            TargetMode::HeadlessAgent
        );
        // the headless marker is what selects observe.
        assert_eq!(
            resolve_target_mode(Some(HEADLESS_ENTRYPOINT), true, true),
            TargetMode::HeadlessAgent,
            "the marker wins over a (spurious) pane — an agent target observes"
        );
    }

    // --- concurrency / latency on the fold (DoD #5) -------------------------

    /// §6 DoD #5 — concurrency k≥2 + latency p50/p99 on the dashboard fold (the
    /// per-frame work the subscriber path runs). k independent observers each fold
    /// an identical synthetic stream; all converge to the SAME terminal state
    /// (the fold is deterministic + isolated), and per-fold latency is PRINTED.
    #[test]
    fn dashboard_fold_k2_concurrency_and_latency() {
        use std::sync::Arc;
        use std::thread;

        let frames: Arc<Vec<ServerMsg>> = Arc::new(vec![
            ready("s"),
            status("busy"),
            turn_end("s", false),
            status("idle"),
            status("busy"),
            turn_end("s", true),
            end("Completed"),
        ]);

        let k = 4; // k ≥ 2
        let handles: Vec<_> = (0..k)
            .map(|_| {
                let frames = frames.clone();
                thread::spawn(move || {
                    let mut d = DashboardState::default();
                    for f in frames.iter() {
                        d.apply(f);
                    }
                    d
                })
            })
            .collect();
        let states: Vec<DashboardState> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // All k observers agree (no shared mutable state across the fold).
        for s in &states[1..] {
            assert_eq!(*s, states[0], "k observers must converge to one state");
        }
        assert_eq!(states[0].turn_count, 2);
        assert_eq!(states[0].ended.as_deref(), Some("Completed"));

        // Latency: p50/p99 of folding the whole stream once, printed for the verifier.
        let n = 1000;
        let mut us: Vec<u128> = Vec::with_capacity(n);
        for _ in 0..n {
            let t = std::time::Instant::now();
            let mut d = DashboardState::default();
            for f in frames.iter() {
                d.apply(f);
            }
            std::hint::black_box(&d);
            us.push(t.elapsed().as_micros());
        }
        us.sort_unstable();
        let p50 = us[n / 2];
        let p99 = us[(n * 99 / 100).min(n - 1)];
        println!("dashboard fold latency (7 frames): p50={p50}us p99={p99}us (n={n})");
        assert!(
            p99 < 10_000,
            "p99 {p99}us exceeds 10ms budget for a pure fold"
        );
    }

    // --- cutover EXECUTION: ordering + never-mid-turn (WP-B-CS-2-LIVE) -------

    /// A fake [`CutoverExec`] that records each step (and its argument) in call
    /// order, so the deterministic tests can assert the load-bearing ordering
    /// (teardown→revive→deliver-FIRST→attach) without a real claude / TTY.
    #[derive(Default)]
    struct RecordingExec {
        calls: Vec<String>,
    }
    impl CutoverExec for RecordingExec {
        type Handle = ();
        type Err = ();
        fn teardown_headless(&mut self) -> Result<(), ()> {
            self.calls.push("teardown".into());
            Ok(())
        }
        fn revive_to_drive(&mut self) -> Result<(), ()> {
            self.calls.push("revive".into());
            Ok(())
        }
        fn deliver_buffered_input(&mut self, _h: &(), input: Option<&str>) -> Result<(), ()> {
            self.calls.push(format!("deliver:{}", input.unwrap_or("-")));
            Ok(())
        }
        fn attach(&mut self, _h: ()) -> Result<i32, ()> {
            self.calls.push("attach".into());
            Ok(0)
        }
    }

    /// §6 DoD — the cutover EXECUTION ordering is load-bearing: teardown BEFORE
    /// revive (one producer on the transcript), and the buffered input delivered
    /// BEFORE attach (buffered-input-FIRST). Reordering `deliver_buffered_input`
    /// after `attach` in [`execute_cutover`] — the fix-shaped mutation — flips the
    /// `deliver` index past `attach` and reds this.
    #[test]
    fn execute_cutover_runs_steps_in_load_bearing_order() {
        let mut exec = RecordingExec::default();
        let code = execute_cutover(
            CutoverFire {
                buffered_input: Some("queued line".into()),
            },
            &mut exec,
        )
        .expect("fake exec never errors");
        assert_eq!(code, 0);
        assert_eq!(
            exec.calls,
            vec![
                "teardown".to_string(),
                "revive".to_string(),
                "deliver:queued line".to_string(),
                "attach".to_string(),
            ],
            "teardown→revive→deliver-FIRST→attach is the enforced order"
        );
        let td = exec.calls.iter().position(|c| c == "teardown").unwrap();
        let rv = exec.calls.iter().position(|c| c == "revive").unwrap();
        let dl = exec
            .calls
            .iter()
            .position(|c| c.starts_with("deliver:"))
            .unwrap();
        let at = exec.calls.iter().position(|c| c == "attach").unwrap();
        assert!(td < rv, "teardown must precede revive");
        assert!(dl < at, "buffered input must be delivered BEFORE attach");
    }

    /// A `None` buffer (cut over with no queued input) still runs the full path and
    /// delivers a no-op buffer step before attach (the buffered-first slot is
    /// unconditional; only its payload is optional).
    #[test]
    fn execute_cutover_with_no_buffer_still_orders_deliver_before_attach() {
        let mut exec = RecordingExec::default();
        execute_cutover(
            CutoverFire {
                buffered_input: None,
            },
            &mut exec,
        )
        .unwrap();
        assert_eq!(
            exec.calls,
            vec!["teardown", "revive", "deliver:-", "attach"]
        );
    }

    /// §6 DoD (RED-before / GREEN-after) — the live run-loop, driven through the
    /// SHARED [`observe_tick`] primitive, fires the cutover EXECUTION ONLY at the
    /// turn boundary, NEVER mid-turn — and delivers the buffered input first when
    /// it does. RED-before posture: while in-turn, `observe_tick` returns `None`, so
    /// `execute_cutover` is never reached (the fake exec stays empty). GREEN-after:
    /// the `RepublishTurnEnd` boundary releases the pending request → the full
    /// ordered execution runs once. The fix-shaped mutation (drop `at_boundary()`
    /// from `poll_fire`) makes `observe_tick` fire mid-turn → teardown runs while
    /// the agent is still in its turn → this reds.
    #[test]
    fn live_loop_fires_cutover_only_at_boundary_buffered_first() {
        let mut gate = CutoverGate::in_turn();
        let mut exec = RecordingExec::default();
        // Operator picks "2" mid-turn, buffering input → deferred to the boundary.
        let decision = gate.request_cutover(Some("buffered hi".to_string()));
        assert_eq!(decision, CutoverDecision::DeferredToBoundary);

        // The run-loop's per-frame drive: mid-turn frames must NOT fire.
        for f in [ready("s"), status("busy")] {
            if let Some(fire) = observe_tick(&mut gate, &f) {
                execute_cutover(fire, &mut exec).unwrap();
            }
        }
        assert!(
            exec.calls.is_empty(),
            "no cutover step may run mid-turn (the boundary gate); got {:?}",
            exec.calls
        );

        // The boundary lands → the cutover fires exactly once, in order.
        if let Some(fire) = observe_tick(&mut gate, &turn_end("s", false)) {
            execute_cutover(fire, &mut exec).unwrap();
        }
        assert_eq!(
            exec.calls,
            vec!["teardown", "revive", "deliver:buffered hi", "attach"],
            "the deferred cutover runs the full ordered execution at the boundary"
        );

        // Idempotent: further frames do not re-fire.
        for f in [status("busy"), turn_end("s", false)] {
            if let Some(fire) = observe_tick(&mut gate, &f) {
                execute_cutover(fire, &mut exec).unwrap();
            }
        }
        assert_eq!(
            exec.calls.len(),
            4,
            "the cutover fires once — later boundaries do not re-execute it"
        );
    }
}
