//! Item-2: the pi-event → [`Republish`] mapping — the normalized event surface
//! the pi resident produces from its stdio-RPC stream.
//!
//! **Moved base, OPTION B (P4DB `d44e869`).** This mapping originally fed the
//! `daemon_status::RegistryStatusSink` (a per-turn status PUSH). The P4DB burn
//! DELETED that sink, and main now derives session status ON-READ (pi's own R3:
//! `pid is NEVER read`; see [`super`] `parse_status`). So under the ruling the
//! resident does NOT push this stream anywhere — the mapper + [`Republish`] enum
//! (now pi-local in [`super::republish`], over the surviving `qrmux::stream_json`
//! types) are RETAINED as the tier-a-tested normalized contract that A2 wires to
//! a connect/status poll to recover live signal-B (status-during-turn). A1's
//! resident derives status on-read and pushes nothing.
//!
//! **The contract we map onto** (pi rpc events → [`Republish`] / qrmux::stream_json):
//!   `agent_start`            → `Republish::Ready { session_id }`   (→ sink: busy/mint)
//!   `message_update`(delta)  → `Republish::Event(StreamEvent::Assistant{..})` (sink: note_output only — no status move; signal-B)
//!   `agent_end`(!willRetry)  → `Republish::TurnEnd(ResultEvent)`   (→ sink: idle)
//!   daemon breaker/kill      → `Republish::Breaker{..}`            (→ sink: offline)
//!   stdout EOF on pi exit     → `Republish::Eof(TurnOutcome)`       (→ sink: idle if Completed, else offline)
//!
//! **Impedance mismatch handled here.** pi streams per-token (`text_delta`) and
//! carries no token accounting / `stop_reason` at the rpc layer, while qrmux's
//! `StreamEvent::Assistant` is message-level (a `content: String`) and
//! `ResultEvent` wants a `Usage`. So a delta maps to an `Assistant` carrying the
//! delta chunk (the sink only notes liveness off it), and `agent_end` maps to a
//! `ResultEvent` with `Usage::default()` + `stop_reason: None`. PA9 holds by
//! construction: a refused `prompt` fires no `agent_*`, so this mapper emits
//! neither `Ready` nor `TurnEnd` → the sink stays idle.
//!
//! `willRetry:true` on `agent_end` is an auto-retry boundary, NOT a terminal —
//! the turn is still open, so we emit nothing and keep `busy` until the real
//! (`willRetry:false`) end.

use super::republish::Republish;
use qrmux::stream_json::{ResultEvent, StreamEvent, TurnOutcome, Usage};

use crate::provider::pi::stdio::rpc::{AssistantEvent, PiEvent};

/// Maps a pi event stream (for ONE adapter incarnation) into [`Republish`]
/// messages for the registry status sink. Holds the bound birth-id and the
/// minimal turn-state needed to classify the EOF outcome. Single-threaded:
/// the daemon's reader drives it in order.
#[derive(Debug, Clone)]
pub struct PiStatusMapper {
    /// The bound session birth-id (from the boot `get_state`), stamped on every
    /// emitted message.
    session_id: String,
    /// A turn has started (`agent_start`) and not yet ended — used to classify
    /// an EOF as `Aborted` (mid-turn) vs `Completed`.
    turn_open: bool,
    /// Any event was seen at all — distinguishes `Completed` from `NoTurn` at EOF.
    saw_event: bool,
    /// The in-flight turn carried an error/abort (`assistantMessageEvent` error)
    /// — surfaced on the next `TurnEnd` as `is_error`.
    saw_error: bool,
}

impl PiStatusMapper {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            turn_open: false,
            saw_event: false,
            saw_error: false,
        }
    }

    /// Map one inbound pi event → an optional [`Republish`] for the sink. `None`
    /// means "no status-relevant message" (e.g. a non-text inner event, or a
    /// `willRetry` retry boundary) — the daemon simply doesn't deliver.
    pub fn on_event(&mut self, event: &PiEvent) -> Option<Republish> {
        self.saw_event = true;
        match event {
            // Turn begins → Ready (the sink mints/marks busy off this).
            PiEvent::AgentStart => {
                self.turn_open = true;
                self.saw_error = false;
                Some(Republish::Ready {
                    session_id: self.session_id.clone(),
                })
            }

            // Assistant content / inner errors. A text delta becomes a
            // passthrough Event (liveness only); an inner abort/error is recorded
            // for the eventual TurnEnd's is_error but emits no status move itself.
            PiEvent::MessageUpdate { assistant_event } => match assistant_event {
                Some(AssistantEvent::TextDelta { delta }) => {
                    Some(Republish::Event(StreamEvent::Assistant {
                        session_id: self.session_id.clone(),
                        content: delta.clone(),
                    }))
                }
                Some(AssistantEvent::Error { reason }) => {
                    if reason == "aborted" || reason == "error" {
                        self.saw_error = true;
                    }
                    None
                }
                _ => None,
            },

            // Turn end. willRetry=true is an auto-retry boundary, not terminal —
            // stay busy. willRetry=false → TurnEnd (the sink goes idle).
            PiEvent::AgentEnd { will_retry } => {
                if *will_retry {
                    None
                } else {
                    self.turn_open = false;
                    Some(Republish::TurnEnd(ResultEvent {
                        session_id: self.session_id.clone(),
                        is_error: self.saw_error,
                        // pi's agent_end carries no stop_reason at the rpc layer.
                        stop_reason: None,
                        // pi gives no token accounting here; the sink doesn't
                        // require it for the status transition.
                        usage: Usage::default(),
                        total_cost_usd: None,
                    }))
                }
            }

            // Other top-level event kinds (turn_start/message_start/tool_*/…) are
            // not status-relevant to the registry sink.
            PiEvent::Other(_) => None,
        }
    }

    /// The daemon's circuit-breaker (byte cap exceeded / forced kill) → offline.
    pub fn on_breaker(&self, cap_bytes: usize, observed_bytes: usize) -> Republish {
        Republish::Breaker {
            cap_bytes,
            observed_bytes,
        }
    }

    /// stdout EOF on pi exit → the in-flight turn outcome. `turn_open` ⇒
    /// `Aborted` (EOF before the turn ended, resumable); else a turn that ended
    /// cleanly ⇒ `Completed`; no events ever ⇒ `NoTurn`.
    pub fn on_eof(&self) -> Republish {
        let outcome = if self.turn_open {
            TurnOutcome::Aborted
        } else if self.saw_event {
            TurnOutcome::Completed
        } else {
            TurnOutcome::NoTurn
        };
        Republish::Eof(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper() -> PiStatusMapper {
        PiStatusMapper::new("sid-1".to_string())
    }

    #[test]
    fn agent_start_maps_to_ready() {
        let mut m = mapper();
        assert_eq!(
            m.on_event(&PiEvent::AgentStart),
            Some(Republish::Ready {
                session_id: "sid-1".to_string()
            })
        );
    }

    #[test]
    fn text_delta_maps_to_an_assistant_passthrough() {
        let mut m = mapper();
        let r = m.on_event(&PiEvent::MessageUpdate {
            assistant_event: Some(AssistantEvent::TextDelta {
                delta: "hi".to_string(),
            }),
        });
        assert_eq!(
            r,
            Some(Republish::Event(StreamEvent::Assistant {
                session_id: "sid-1".to_string(),
                content: "hi".to_string()
            }))
        );
    }

    #[test]
    fn agent_end_maps_to_turn_end_idle() {
        let mut m = mapper();
        m.on_event(&PiEvent::AgentStart);
        let r = m.on_event(&PiEvent::AgentEnd { will_retry: false });
        match r {
            Some(Republish::TurnEnd(re)) => {
                assert_eq!(re.session_id, "sid-1");
                assert!(!re.is_error);
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn will_retry_end_is_not_a_terminal() {
        let mut m = mapper();
        m.on_event(&PiEvent::AgentStart);
        assert_eq!(m.on_event(&PiEvent::AgentEnd { will_retry: true }), None);
        // still open → an EOF now is Aborted, not Completed.
        assert_eq!(m.on_eof(), Republish::Eof(TurnOutcome::Aborted));
    }

    #[test]
    fn inner_abort_surfaces_as_is_error_on_turn_end() {
        let mut m = mapper();
        m.on_event(&PiEvent::AgentStart);
        // an aborted inner event records the error but emits nothing itself.
        assert_eq!(
            m.on_event(&PiEvent::MessageUpdate {
                assistant_event: Some(AssistantEvent::Error {
                    reason: "aborted".to_string()
                })
            }),
            None
        );
        match m.on_event(&PiEvent::AgentEnd { will_retry: false }) {
            Some(Republish::TurnEnd(re)) => assert!(re.is_error),
            other => panic!("expected an errored TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn eof_outcomes_completed_aborted_noturn() {
        // NoTurn: nothing ever happened.
        assert_eq!(mapper().on_eof(), Republish::Eof(TurnOutcome::NoTurn));

        // Completed: a turn started and ended cleanly.
        let mut done = mapper();
        done.on_event(&PiEvent::AgentStart);
        done.on_event(&PiEvent::AgentEnd { will_retry: false });
        assert_eq!(done.on_eof(), Republish::Eof(TurnOutcome::Completed));

        // Aborted: a turn started but EOF arrived before it ended.
        let mut mid = mapper();
        mid.on_event(&PiEvent::AgentStart);
        assert_eq!(mid.on_eof(), Republish::Eof(TurnOutcome::Aborted));
    }

    #[test]
    fn breaker_maps_through() {
        assert_eq!(
            mapper().on_breaker(128, 256),
            Republish::Breaker {
                cap_bytes: 128,
                observed_bytes: 256
            }
        );
    }
}
