//! Pi-local republish contract — the `Republish` event enum + `Sink` trait the
//! pi adapter maps its stdio-RPC turn events onto and delivers through.
//!
//! **Provenance / why this lives here (A1, moved base):** these two types were
//! originally borrowed from `qrmux::headless` (the claude `-p` stream-json drive).
//! On current main that module was DELETED by the P4DB burn
//! (`d44e869` "burn the one-off claude -p stream-json drive") — claude `-p` no
//! longer republishes, so qrmux dropped the machinery. The pi adapter is now the
//! sole consumer of this small contract, so it OWNS it here rather than
//! resurrecting the burned qrmux module. This is an exact relocation of the
//! `Republish` enum + `Sink` trait subset pi used (pi never used `Fanout` /
//! `HeadlessReader` / the child-drive parts, which stay burned). The wrapped
//! payload types (`StreamEvent`, `ResultEvent`, `TurnOutcome`) still live in
//! `qrmux::stream_json` and are imported from there unchanged.

use qrmux::stream_json::{ResultEvent, StreamEvent, TurnOutcome};

/// The facts the republish sink carries. A consumer (the pi resident's ws
/// fan-out, the registry-status sink) reacts to each variant.
#[derive(Debug, Clone, PartialEq)]
pub enum Republish {
    /// Readiness: the first event of a turn (`system/init`) — the producer is up
    /// and has bound its identity (`session_id`). Known before turn completion.
    Ready { session_id: String },
    /// Raw event passthrough (assistant content, rate-limit, unknown types) — the
    /// coalescible status stream. A consumer may log/forward these.
    Event(StreamEvent),
    /// Turn-end: the `result` event. NEVER dropped on overflow.
    TurnEnd(ResultEvent),
    /// Circuit-breaker fired: the child was (or is being) killed. Terminal.
    Breaker {
        cap_bytes: usize,
        observed_bytes: usize,
    },
    /// EOF: the child's stdout closed. Carries the in-flight turn's outcome.
    Eof(TurnOutcome),
    /// Daemon-driven wake: a parked session was nudged out-of-band; rides the same
    /// pump → sink path as a coalescible passthrough. Never a must-keep.
    Wake,
}

/// The republish sink. A clean trait so the delivery target (the ws fan-out front,
/// the registry-status writer) can vary without touching the reader/pump.
///
/// `deliver` MAY block (a slow socket client) — that is exactly why it lives
/// behind the decoupled pump thread, never on the reader's hot path.
pub trait Sink: Send {
    fn deliver(&self, msg: Republish);
}

/// Boxed sinks ARE sinks: a `Box<dyn Sink>` injected qd-side must compose like any
/// other sink.
impl Sink for Box<dyn Sink> {
    fn deliver(&self, msg: Republish) {
        (**self).deliver(msg);
    }
}
