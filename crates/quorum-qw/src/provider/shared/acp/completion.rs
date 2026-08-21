//! `provider/acp/completion.rs` — `AcpTurnCompletion` (the SC-2 RE-ANCHORED completion contract
//! for the ACP tier) + the started/streaming progress classifier (SC-2c).
//!
//! SC-2 has exactly two halves for ACP:
//! - **turn-COMPLETE = the REAL terminal** — the correlated `session/prompt` response carrying a
//!   `StopReason` (SC-2a). `AcpTurnCompletion: TurnCompletion` (`wait.rs:60`) returns `Visible` +
//!   an SC-2a [`TerminalReason`]/[`Confidence`] on that terminal, `Pending` while in flight, and
//!   `Degraded` if the transport was lost mid-turn (source integrity gone). The codex
//!   `ChannelTurnCompletion` is the worked reference; this is NOT an `events.rs` `Payload` emitter,
//!   and the SC-2a reason is carried HERE, never folded into the shared `TurnCompletionProbe`
//!   (keeps codex / `wait.rs` byte-identical — F2.1).
//! - **turn-STARTED/STREAMING = the WS-R progress producer's feed (CONSUMED, SC-2c)** — for ACP the
//!   producer's feed is the `session/update` notification stream. dispatch does NOT invent a parallel
//!   started/streaming source (the F2.1 trap); it consumes the producer. **CROSS-WS DEPENDENCY:** the
//!   real producer is WS-R's R3b module; where R3b is not yet landed, a phase wires a FIXTURE producer
//!   (named, not papered over). [`TurnProgress`] is the consumer-side classifier: first `Update` for a
//!   turn = `Started`, subsequent = `Streaming`.

use crate::provider::acp::rpc::{Confidence, StopReason, TerminalReason};
use crate::wait::{TurnCompletion, TurnCompletionProbe};

/// What the SC-5 single-reader demux knows about the EXPECTED turn, for the completion probe.
/// The host computes this by correlating events off its per-session bus to the in-flight turn id
/// (SC-2b: a terminal for the WRONG turn never satisfies this).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpTurnObservation {
    /// The correlated terminal `StopReason` response arrived for the expected turn (SC-2a).
    Terminal(StopReason),
    /// A JSON-RPC ERROR response arrived for the expected turn (R8: authRequired / internalError) —
    /// turn-terminal `Failed`, NOT a `StopReason`. Carries the error text.
    Failed(String),
    /// Not terminal yet — the turn is in flight / streaming (`Pending`; the idle-without-evidence rule).
    Pending,
    /// The transport/connection was lost before any terminal (SC-2a `TransportLost`) — source
    /// integrity gone, so the probe `Degraded`s (cannot claim done OR not-done).
    TransportLost(String),
}

/// The source the completion probe reads (SC-5 bus). Seamed so CONF-EVENT drives it without a live
/// bridge (a fixture observer), exactly as `ChannelTurnCompletion` reads a `ChannelTurnObservation`.
pub trait AcpTurnObserver {
    fn observe(&self) -> AcpTurnObservation;
}

/// `TurnCompletion` for the ACP tier. `poll_completion` is a PURE observation (no waiting — the
/// `--wait` loop owns cadence/deadline). The SC-2a reason is exposed via [`Self::terminal_reason`]
/// (CONF-EVENT asserts it), NOT smuggled into the shared `TurnCompletionProbe`.
pub struct AcpTurnCompletion<'a> {
    observer: &'a dyn AcpTurnObserver,
}

impl<'a> AcpTurnCompletion<'a> {
    pub fn new(observer: &'a dyn AcpTurnObserver) -> Self {
        Self { observer }
    }

    /// The SC-2a (reason, confidence) for the CURRENT observation, or `None` while not terminal.
    /// Every ACP terminal is `ProtocolConfirmed` (the `StopReason`/JSON-RPC response is the bridge's
    /// authoritative signal — never a screen-scrape). A `Failed` (JSON-RPC error) is `Failed`, NOT
    /// `Completed` (the CONF-EVENT negative control). A lost transport is `TransportLost`.
    pub fn terminal_reason(&self) -> Option<(TerminalReason, Confidence)> {
        match self.observer.observe() {
            AcpTurnObservation::Terminal(s) => {
                Some((s.to_terminal_reason(), Confidence::ProtocolConfirmed))
            }
            AcpTurnObservation::Failed(_) => {
                Some((TerminalReason::Failed, Confidence::ProtocolConfirmed))
            }
            AcpTurnObservation::TransportLost(_) => {
                Some((TerminalReason::TransportLost, Confidence::ProtocolConfirmed))
            }
            AcpTurnObservation::Pending => None,
        }
    }
}

impl TurnCompletion for AcpTurnCompletion<'_> {
    fn poll_completion(&self) -> TurnCompletionProbe {
        match self.observer.observe() {
            // A real terminal (StopReason) OR a definitive Failed both mean the turn is DONE — the
            // wait loop latches. The REASON (Completed vs Failed vs Cancelled vs …) is the SC-2a
            // classification, read separately via `terminal_reason` (CONF-EVENT asserts it).
            AcpTurnObservation::Terminal(_) | AcpTurnObservation::Failed(_) => {
                TurnCompletionProbe::Visible
            }
            AcpTurnObservation::Pending => TurnCompletionProbe::Pending,
            // Transport lost mid-turn → source integrity gone → degrade (never a false done).
            AcpTurnObservation::TransportLost(why) => TurnCompletionProbe::Degraded(why),
        }
    }
}

/// The started→streaming half of SC-2 (SC-2c): the CONSUMER-side classifier over the WS-R progress
/// producer's ACP `session/update` feed. dispatch does NOT own the producer; this just maps the
/// per-turn event count to a phase. First `session/update` for a turn = `Started`; subsequent =
/// `Streaming`. (CROSS-WS: the producer is R3b; a fixture producer feeds this where R3b is unlanded.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressPhase {
    Started,
    Streaming,
}

/// Tracks the started/streaming phase for one turn from the count of `session/update`s observed.
#[derive(Debug, Default)]
pub struct TurnProgress {
    updates_seen: u64,
}

impl TurnProgress {
    pub fn new() -> Self {
        Self { updates_seen: 0 }
    }

    /// Record one observed `session/update` for the turn; returns the phase it represents. The
    /// FIRST is `Started`, the rest `Streaming` (the SC-2c started→streaming mapping).
    pub fn on_update(&mut self) -> ProgressPhase {
        self.updates_seen += 1;
        if self.updates_seen == 1 {
            ProgressPhase::Started
        } else {
            ProgressPhase::Streaming
        }
    }

    /// Whether any progress (a started/streaming event) has been observed for this turn.
    pub fn started(&self) -> bool {
        self.updates_seen > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fixture observer whose observation a test can set — the in-process stand-in for the SC-5
    /// bus (CONF-EVENT drives the completion contract without a live bridge).
    struct FixtureObserver {
        obs: RefCell<AcpTurnObservation>,
    }
    impl FixtureObserver {
        fn new(o: AcpTurnObservation) -> Self {
            Self {
                obs: RefCell::new(o),
            }
        }
        #[allow(dead_code)]
        fn set(&self, o: AcpTurnObservation) {
            *self.obs.borrow_mut() = o;
        }
    }
    impl AcpTurnObserver for FixtureObserver {
        fn observe(&self) -> AcpTurnObservation {
            self.obs.borrow().clone()
        }
    }

    // CONF-EVENT: Pending while in flight → Pending probe, no reason.
    #[test]
    fn pending_is_pending_no_reason() {
        let o = FixtureObserver::new(AcpTurnObservation::Pending);
        let c = AcpTurnCompletion::new(&o);
        assert_eq!(c.poll_completion(), TurnCompletionProbe::Pending);
        assert_eq!(c.terminal_reason(), None);
    }

    // CONF-EVENT: a clean end_turn terminal → Visible + reason=Completed/ProtocolConfirmed.
    #[test]
    fn end_turn_is_visible_completed() {
        let o = FixtureObserver::new(AcpTurnObservation::Terminal(StopReason::EndTurn));
        let c = AcpTurnCompletion::new(&o);
        assert_eq!(c.poll_completion(), TurnCompletionProbe::Visible);
        assert_eq!(
            c.terminal_reason(),
            Some((TerminalReason::Completed, Confidence::ProtocolConfirmed))
        );
    }

    // CONF-EVENT: a cancelled terminal → Visible + reason=Cancelled (interrupt path).
    #[test]
    fn cancelled_is_visible_cancelled() {
        let o = FixtureObserver::new(AcpTurnObservation::Terminal(StopReason::Cancelled));
        let c = AcpTurnCompletion::new(&o);
        assert_eq!(c.poll_completion(), TurnCompletionProbe::Visible);
        assert_eq!(c.terminal_reason().unwrap().0, TerminalReason::Cancelled);
    }

    // CONF-EVENT NEGATIVE CONTROL (SC-2a): a JSON-RPC ERROR terminal must read as Failed, NOT
    // Completed — a crash/error must never look like a clean turn (panel 4.5).
    #[test]
    fn jsonrpc_error_is_failed_not_completed() {
        let o = FixtureObserver::new(AcpTurnObservation::Failed("internalError".into()));
        let c = AcpTurnCompletion::new(&o);
        assert_eq!(
            c.poll_completion(),
            TurnCompletionProbe::Visible,
            "a failed turn IS done"
        );
        let (reason, conf) = c.terminal_reason().unwrap();
        assert_eq!(
            reason,
            TerminalReason::Failed,
            "error terminal must be Failed, NOT Completed"
        );
        assert_eq!(conf, Confidence::ProtocolConfirmed);
    }

    // SC-2a: transport lost mid-turn → Degraded (never a false done) + reason=TransportLost.
    #[test]
    fn transport_lost_is_degraded() {
        let o = FixtureObserver::new(AcpTurnObservation::TransportLost("eof".into()));
        let c = AcpTurnCompletion::new(&o);
        assert!(matches!(
            c.poll_completion(),
            TurnCompletionProbe::Degraded(_)
        ));
        assert_eq!(
            c.terminal_reason().unwrap().0,
            TerminalReason::TransportLost
        );
    }

    // SC-2a: max_turn_requests maps to the MaxTokens limit reason (model-limit terminal).
    #[test]
    fn max_turn_requests_is_visible_maxtokens() {
        let o = FixtureObserver::new(AcpTurnObservation::Terminal(StopReason::MaxTurnRequests));
        let c = AcpTurnCompletion::new(&o);
        assert_eq!(c.poll_completion(), TurnCompletionProbe::Visible);
        assert_eq!(c.terminal_reason().unwrap().0, TerminalReason::MaxTokens);
    }

    // SC-2c: the started→streaming half consumes the session/update feed: first update = Started,
    // subsequent = Streaming. (Fixture producer; the real producer is WS-R R3b — named dependency.)
    #[test]
    fn progress_first_started_then_streaming() {
        let mut p = TurnProgress::new();
        assert!(!p.started());
        assert_eq!(p.on_update(), ProgressPhase::Started);
        assert!(p.started());
        assert_eq!(p.on_update(), ProgressPhase::Streaming);
        assert_eq!(p.on_update(), ProgressPhase::Streaming);
    }
}
