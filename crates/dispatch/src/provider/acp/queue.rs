//! `provider/acp/queue.rs` — the **SC-1 per-session OUTBOUND queue** (net-new; A3-ACP scope).
//!
//! ACP `session/prompt` is JSON-RPC request/response: NO protocol-level queueing, NO
//! type-ahead, NO busy-error (SC-1). codex got serialization free from the server-side turn
//! fence + single-stdio; PTY got it free from the tty buffer; **ACP gets none** — so dispatch
//! MUST own a per-session, turn-aware outbound queue that releases the next item only on the
//! in-flight turn's REAL terminal `StopReason` (SC-1), never on a timer (SC-1a) and never by
//! interrupting a running turn (intra-turn autonomy, §5).
//!
//! This is a single-threaded state machine (the engine is a short-lived sync CLI; the queue
//! lives in the long-lived per-session ACP host — `client.rs` — that owns the SC-5 single
//! reader, NOT behind a borrowed per-verb `fx.acp_client`; SC-1a ownership). The host drives
//! it: `enqueue` on `inject`, `try_release` when the session is turn-idle (→ issue
//! `session/prompt`), `complete` when the correlated terminal arrives off the demux bus.
//!
//! Distinctions this type encodes precisely (so an implementer never conflates them):
//! - **bounded-overflow → `Precondition("queue full")`** is FLOW-CONTROL backpressure (the one
//!   legitimate rejection), DISTINCT from the SC-3 refuse-on-busy ban (a 2nd `inject` mid-turn
//!   QUEUES, it does not error — that is the whole point of the queue).
//! - **`cancel_and_drain`** (WS-R wedge handoff) DISCARDS the backlog and surfaces the count;
//!   `session/cancel` is best-effort, so **`teardown`** is the GUARANTEED unblock (SC-1a).
//! - a `StopReason` for the WRONG turn id (an old turn during reconnect; SC-2b) does NOT
//!   release the slot — correlation is by turn id, owned by the SC-5 demux.

use std::collections::VecDeque;

use crate::provider::acp::rpc::{SessionId, TurnId};
use crate::provider::InjectError;

/// One queued `inject`, waiting to become the session's in-flight turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedInject {
    /// The attributable turn id assigned at ENQUEUE (LB#5) — returned from `inject`, and the
    /// key the terminal `StopReason` releases by.
    pub turn: TurnId,
    pub message: String,
    pub from: String,
}

/// The outcome of [`OutboundQueue::complete`] — what a terminal `StopReason` (or error) did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteOutcome {
    /// The terminal matched the in-flight turn → slot freed; the next `try_release` may proceed.
    Released,
    /// A terminal for a turn that is NOT in flight (an old turn during reconnect, an
    /// out-of-order frame; SC-2b) → IGNORED; the in-flight turn is untouched, no release.
    WrongTurn,
    /// There was no in-flight turn at all → nothing to release (defensive; SC-2b empty case).
    NotInFlight,
}

/// The result of the WS-R wedge handoff [`OutboundQueue::cancel_and_drain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainOutcome {
    /// The in-flight turn the caller should now `session/cancel` (`None` if the session was
    /// idle — nothing in flight to cancel).
    pub cancel_turn: Option<TurnId>,
    /// How many QUEUED (not-yet-released) injects were discarded — surfaced to WS-R/operator.
    pub discarded: usize,
}

/// The result of the guaranteed-unblock [`OutboundQueue::teardown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownOutcome {
    /// The in-flight turn that was abandoned (`None` if idle) — the caller now kills the
    /// connection (`kill.rs`), the unconditional unblock when `session/cancel` was unresponsive.
    pub abandoned_turn: Option<TurnId>,
    /// Queued injects discarded by the teardown.
    pub discarded: usize,
}

/// The SC-1 bounded, turn-aware, per-session outbound queue.
#[derive(Debug)]
pub struct OutboundQueue {
    session: SessionId,
    /// Max DEPTH of the waiting backlog (the `pending` deque). The in-flight turn is NOT
    /// counted against it. Overflow → `Precondition("queue full")` (SC-1a, configurable N).
    capacity: usize,
    /// Monotonic per-session counter for attributable turn ids.
    next_seq: u64,
    /// The currently in-flight turn (released on its REAL terminal). `None` = turn-idle.
    in_flight: Option<TurnId>,
    /// The waiting backlog, FIFO.
    pending: VecDeque<QueuedInject>,
    /// Lifetime count of discarded injects (drain + teardown) — operator visibility.
    discarded_total: u64,
}

impl OutboundQueue {
    /// Create a queue for `session` bounded to `capacity` waiting items (`capacity` must be
    /// ≥ 1; a 0 bound would reject every inject). The default N is chosen by the host.
    pub fn new(session: impl Into<SessionId>, capacity: usize) -> OutboundQueue {
        OutboundQueue {
            session: session.into(),
            capacity: capacity.max(1),
            next_seq: 0,
            in_flight: None,
            pending: VecDeque::new(),
            discarded_total: 0,
        }
    }

    fn mint_turn_id(&mut self) -> TurnId {
        let seq = self.next_seq;
        self.next_seq += 1;
        format!("{}#t{}", self.session, seq)
    }

    /// Enqueue an `inject`. Returns the attributable turn id (LB#5). A 2nd `inject` while a
    /// turn is in flight QUEUES — it does NOT error (SC-3); the ONLY rejection is the bounded
    /// backlog overflow → `Precondition("queue full")` (SC-1a flow-control, distinct from SC-3).
    pub fn enqueue(&mut self, message: &str, from: &str) -> Result<TurnId, InjectError> {
        if self.pending.len() >= self.capacity {
            return Err(InjectError::Precondition("queue full".to_string()));
        }
        let turn = self.mint_turn_id();
        self.pending.push_back(QueuedInject {
            turn: turn.clone(),
            message: message.to_string(),
            from: from.to_string(),
        });
        Ok(turn)
    }

    /// If the session is turn-idle and an item waits, make the head item the in-flight turn and
    /// hand it back for the host to issue as `session/prompt`. Returns `None` if a turn is
    /// already in flight (SC-1 never interrupts a running turn) or the backlog is empty.
    pub fn try_release(&mut self) -> Option<QueuedInject> {
        if self.in_flight.is_some() {
            return None;
        }
        let item = self.pending.pop_front()?;
        self.in_flight = Some(item.turn.clone());
        Some(item)
    }

    /// Apply a terminal (the correlated `StopReason` response, or a `TerminalError`) for `turn`.
    /// Releases the slot ONLY when `turn` is the in-flight turn (SC-2b correlation); a terminal
    /// for the wrong turn is ignored ([`CompleteOutcome::WrongTurn`]) and cannot release the
    /// wrong slot. The host calls this AFTER the SC-5 reader has drained the turn's trailing
    /// `session/update`s (release timing, SC-2b).
    pub fn complete(&mut self, turn: &str) -> CompleteOutcome {
        match &self.in_flight {
            Some(t) if t == turn => {
                self.in_flight = None;
                CompleteOutcome::Released
            }
            Some(_) => CompleteOutcome::WrongTurn,
            None => CompleteOutcome::NotInFlight,
        }
    }

    /// WS-R wedge handoff (SC-1a): the detector declared the in-flight turn wedged and is
    /// initiating recovery. DISCARD the backlog (a wedge makes the conversation context
    /// suspect — silently force-feeding it into a just-recovered session is wrong) and return
    /// the in-flight turn for the caller to `session/cancel` (best-effort) plus the discarded
    /// count. Does NOT clear the in-flight turn: it clears on the `cancelled` terminal via
    /// [`Self::complete`], or unconditionally via [`Self::teardown`] if cancel is unresponsive.
    pub fn cancel_and_drain(&mut self) -> DrainOutcome {
        let discarded = self.pending.len();
        self.discarded_total += discarded as u64;
        self.pending.clear();
        DrainOutcome {
            cancel_turn: self.in_flight.clone(),
            discarded,
        }
    }

    /// The GUARANTEED unblock (SC-1a): unconditionally abandon the in-flight turn AND discard
    /// the backlog. The caller then tears the connection down (`kill.rs`) — this is the path
    /// WS-R falls to when `session/cancel` did not produce a `cancelled` terminal within its grace.
    pub fn teardown(&mut self) -> TeardownOutcome {
        let discarded = self.pending.len();
        self.discarded_total += discarded as u64;
        self.pending.clear();
        let abandoned_turn = self.in_flight.take();
        TeardownOutcome {
            abandoned_turn,
            discarded,
        }
    }

    /// Turn-idle (no in-flight turn) — the host may `try_release` the next item.
    pub fn is_idle(&self) -> bool {
        self.in_flight.is_none()
    }

    /// The in-flight turn id, if a turn is running.
    pub fn in_flight(&self) -> Option<&str> {
        self.in_flight.as_deref()
    }

    /// Current waiting-backlog depth (excludes the in-flight turn).
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Lifetime count of injects discarded by drain/teardown.
    pub fn discarded_total(&self) -> u64 {
        self.discarded_total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> OutboundQueue {
        OutboundQueue::new("sess-A", 4)
    }

    // SC-1 happy path: send → terminal → release ORDERING.
    #[test]
    fn send_terminal_release_ordering() {
        let mut q = q();
        let t1 = q.enqueue("first", "peer").unwrap();
        // released as the in-flight turn (session was idle).
        let r1 = q.try_release().expect("first releases");
        assert_eq!(r1.turn, t1);
        assert_eq!(q.in_flight(), Some(t1.as_str()));
        // a 2nd inject mid-turn QUEUES (SC-3) and does NOT release while t1 is in flight.
        let t2 = q.enqueue("second", "peer").unwrap();
        assert!(q.try_release().is_none(), "no release while a turn is in flight (SC-1)");
        assert_eq!(q.pending_len(), 1);
        // t1 terminal → release → t2 becomes releasable.
        assert_eq!(q.complete(&t1), CompleteOutcome::Released);
        assert!(q.is_idle());
        let r2 = q.try_release().expect("second releases after first terminal");
        assert_eq!(r2.turn, t2);
    }

    // SC-3: a 2nd inject while busy QUEUES, never errors.
    #[test]
    fn second_inject_midturn_queues_not_errors() {
        let mut q = q();
        let _t1 = q.enqueue("a", "p").unwrap();
        q.try_release().unwrap();
        let r = q.enqueue("b", "p");
        assert!(r.is_ok(), "mid-turn inject must QUEUE, not error (SC-3)");
        assert_eq!(q.pending_len(), 1);
    }

    // SC-1a: wrong-turn-id terminal does NOT release.
    #[test]
    fn wrong_turn_id_terminal_does_not_release() {
        let mut q = q();
        let t1 = q.enqueue("a", "p").unwrap();
        q.try_release().unwrap();
        assert_eq!(q.complete("sess-A#t999"), CompleteOutcome::WrongTurn);
        assert_eq!(q.in_flight(), Some(t1.as_str()), "in-flight untouched by a wrong-turn terminal");
        assert!(!q.is_idle());
        // the real terminal still releases.
        assert_eq!(q.complete(&t1), CompleteOutcome::Released);
    }

    #[test]
    fn terminal_with_no_in_flight_is_not_in_flight() {
        let mut q = q();
        assert_eq!(q.complete("anything"), CompleteOutcome::NotInFlight);
    }

    // SC-1a: bounded overflow → Precondition("queue full") (flow control, NOT SC-3).
    #[test]
    fn bounded_overflow_is_precondition_queue_full() {
        let mut q = OutboundQueue::new("s", 2);
        let _t1 = q.enqueue("1", "p").unwrap();
        q.try_release().unwrap(); // t1 in flight; backlog now empty, capacity=2
        let _t2 = q.enqueue("2", "p").unwrap(); // pending=1
        let _t3 = q.enqueue("3", "p").unwrap(); // pending=2 (== capacity)
        let overflow = q.enqueue("4", "p");
        assert_eq!(
            overflow,
            Err(InjectError::Precondition("queue full".to_string())),
            "overflow is flow-control backpressure, not an SC-3 busy-refusal"
        );
        assert_eq!(q.pending_len(), 2);
    }

    // SC-1a: cancel_and_drain DISCARDS the backlog + surfaces the count + names the cancel turn.
    #[test]
    fn cancel_and_drain_discards_and_counts() {
        let mut q = q();
        let t1 = q.enqueue("a", "p").unwrap();
        q.try_release().unwrap();
        q.enqueue("b", "p").unwrap();
        q.enqueue("c", "p").unwrap();
        assert_eq!(q.pending_len(), 2);
        let out = q.cancel_and_drain();
        assert_eq!(out.cancel_turn, Some(t1.clone()), "names the in-flight turn to cancel");
        assert_eq!(out.discarded, 2, "discards the suspect backlog");
        assert_eq!(q.pending_len(), 0);
        assert_eq!(q.discarded_total(), 2);
        // in-flight is NOT cleared by drain — it clears on the cancelled terminal (best-effort).
        assert_eq!(q.in_flight(), Some(t1.as_str()));
        assert_eq!(q.complete(&t1), CompleteOutcome::Released);
    }

    #[test]
    fn cancel_and_drain_idle_session_has_no_cancel_turn() {
        let mut q = q();
        q.enqueue("a", "p").unwrap(); // pending but NOT released → idle
        let out = q.cancel_and_drain();
        assert_eq!(out.cancel_turn, None, "idle session: nothing in flight to cancel");
        assert_eq!(out.discarded, 1);
    }

    // SC-1a: teardown is the GUARANTEED unblock when cancel is unresponsive.
    #[test]
    fn teardown_guaranteed_unblock() {
        let mut q = q();
        let t1 = q.enqueue("a", "p").unwrap();
        q.try_release().unwrap();
        q.enqueue("b", "p").unwrap();
        // cancel was issued but the bridge wedged — no cancelled terminal ever arrives.
        let out = q.teardown();
        assert_eq!(out.abandoned_turn, Some(t1), "teardown abandons the wedged in-flight turn");
        assert_eq!(out.discarded, 1);
        assert!(q.is_idle(), "teardown unconditionally unblocks the queue");
        assert_eq!(q.pending_len(), 0);
    }

    #[test]
    fn turn_ids_are_unique_and_attributable_to_session() {
        let mut q = OutboundQueue::new("sess-XYZ", 8);
        let a = q.enqueue("1", "p").unwrap();
        let b = q.enqueue("2", "p").unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("sess-XYZ#"), "turn id is attributable to the session");
    }
}
