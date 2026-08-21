//! The **qd↔qw wire** — [`LaneOps`] serialized as line-delimited JSON over a
//! child's stdio (qd/qw split, stage 4).
//!
//! # What this is
//!
//! `qd` holds a [`client::WireLane`], which implements [`LaneOps`] by spawning
//! `qw` and speaking this protocol to it. `qw`'s `main` runs [`server::serve`],
//! which decodes a call, dispatches it to a real [`crate::lanes::LaneImpl`], and
//! writes the answer back. Neither side reads the other's files and neither side
//! holds the other's types in memory: the only thing that crosses is bytes on a
//! pipe.
//!
//! # Protocol
//!
//! One JSON object per `\n`-terminated line, in both directions. Stdout carries
//! protocol frames and **nothing else**; **stderr is inherited** by the child, so
//! a lane's diagnostics reach the user's terminal directly and can never corrupt
//! the frame stream. This is the `pi --mode rpc` transport
//! (`provider/pi/stdio/stdio.rs`) and the reason it puts `stderr(inherit())` in
//! its spawn.
//!
//! ```text
//! request : {"id":N,"m":<method>, …args}
//! ok      : {"id":N,"ok":<result>}
//! err     : {"id":N,"e":<LaneError>}
//! ```
//!
//! The request/ok shape is the house envelope, already documented as a protocol
//! rather than an accident in `provider/shared/acp/wire.rs:27-35`.
//!
//! # The one deliberate deviation: `e` carries a `LaneError`, not `{k,m}`
//!
//! The ACP envelope's error is a `(kind, message)` pair because an ACP error IS
//! one. [`LaneError`] is not: [`LaneError::WakeFailed`] carries `exit_code` and
//! `self_attributed`, and that second flag is the difference between qd printing
//! `qd resume: ERROR: …` and printing `ERROR: …` — it records whether the core
//! already attributed the line. Flattening the enum to `(kind, message)` would
//! drop it, and the boundary would start double-attributing errors.
//!
//! So the error payload is the serialized enum. It costs nothing — [`LaneError`]
//! already derives serde because the trait's serializability guard requires it —
//! and it makes the wire lossless by construction rather than by a mapping table
//! somebody has to keep in sync.
//!
//! # Version handshake
//!
//! Frame 1 is always `hello`, and a mismatch is a **loud refusal**, not a
//! negotiation. The precedent is `qrmux/src/protocol/handshake.rs:10-16`, whose
//! preamble shape is "FROZEN FOREVER" with a refusal contract on mismatch; this
//! is the same discipline in JSON. `02-qw-split.md` names the alternative — a
//! compatibility window — and its cost: a maintained compatibility surface on
//! every user's machine, where today the seam is a Rust trait the compiler checks
//! in one commit.
//!
//! Version-lock, refuse loudly, and let the installer's one-pin rule keep the two
//! binaries in step.
//!
//! # What does NOT cross
//!
//! [`LaneOps::attach`] is not a call. It is a terminal handoff, performed by exec
//! with inherited stdio — see that method's docs for why an earlier
//! data-returning design could not work (the default embedded mux execs nothing
//! at all, so no argv could describe it). [`client::WireLane::attach`] spawns
//! `qw attach` with the parent's own stdio and returns its exit code.

use serde::{Deserialize, Serialize};

use crate::contract::{
    DeliverPolicy, LaneError, LedgerAddress, Message, MessageId, SessionId, StartRequest,
};
use crate::lane::Lane;
use crate::launch::RenderMode;

pub mod client;
pub mod server;

/// The wire version. Bumped whenever a frame's shape changes in a way an older
/// peer would misread; a mismatch refuses rather than negotiates.
pub const WIRE_VERSION: u32 = 1;

/// The argv verb the `qw` binary answers this protocol on.
pub const SERVE_VERB: &str = "serve";

/// The argv verb the `qw` binary performs a terminal handoff on.
pub const ATTACH_VERB: &str = "attach";

/// One call across the boundary.
///
/// # The session field is `session`, not `id`
///
/// [`Request`] flattens this enum beside its own correlation `id`, so a variant
/// carrying a field ALSO called `id` produces a duplicate key: the frame
/// serializes without complaint and fails to decode with "duplicate field `id`".
/// Caught by `every_call_round_trips_and_tags_on_m`, which exists because
/// flatten-over-internally-tagged is exactly the combination that misbehaves
/// quietly — every individual DTO round-trips fine, and the frame still does not.
///
/// Internally tagged on `m`, so a frame reads exactly as the protocol block above
/// describes it. **Every [`LaneOps`] method except `attach` has a variant**, and
/// the exhaustive `match` in [`server::dispatch`] is what makes that total: adding
/// a method to the trait without adding it here fails to compile, which is the
/// property a hand-written string dispatcher would not have.
///
/// [`LaneOps`]: crate::contract::LaneOps
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "m", rename_all = "snake_case")]
pub enum Call {
    /// Frame 1. Carries the caller's [`WIRE_VERSION`].
    Hello { v: u32 },
    Start {
        lane: Lane,
        req: StartRequest,
    },
    Wake {
        lane: Lane,
        session: SessionId,
        render: RenderMode,
        cwd_override: Option<String>,
    },
    Kill {
        lane: Lane,
        session: SessionId,
    },
    List {
        lane: Lane,
    },
    Health {
        lane: Lane,
        session: SessionId,
    },
    ReceivePath {
        lane: Lane,
        session: SessionId,
    },
    Deliver {
        lane: Lane,
        session: SessionId,
        msg: Message,
        policy: DeliverPolicy,
    },
    AwaitTerminal {
        lane: Lane,
        session: SessionId,
        message_id: MessageId,
        budget_ms: u64,
    },
    Recover {
        lane: Lane,
        at: LedgerAddress,
        message_id: MessageId,
    },
    /// Has this SESSION gone idle? Ruling D2 — a different SUBJECT from
    /// [`Call::AwaitTerminal`], which is why it is a frame of its own rather than
    /// a flag on that one. See
    /// [`LaneOps::await_idle`](crate::contract::LaneOps::await_idle).
    AwaitIdle {
        lane: Lane,
        session: SessionId,
        budget_ms: u64,
    },
    /// Clause (a) of the dead-writer rule — a pure poll of qw's log. See
    /// [`LaneOps::resolved`](crate::contract::LaneOps::resolved).
    Resolved {
        lane: Lane,
        at: LedgerAddress,
        message_id: MessageId,
    },
    /// Everything `attach` decides BEFORE the handoff.
    ///
    /// `attach` itself is an exec, not a call — but two of its refusals are data
    /// qd ACTS on rather than prints (a daemon redirect, and a cold session it
    /// must revive and retry), and an exec has only an exit code to come back
    /// with. So the refusal crosses here and only the handoff is an exec. See
    /// `LaneImpl::attach_target`.
    AttachPrecheck {
        lane: Lane,
        session: SessionId,
    },
}

/// A request frame: a correlation id plus the call, flattened so the call's `m`
/// tag sits beside `id` rather than nested under it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub call: Call,
}

/// A reply frame.
///
/// `ok` holds the method's return value, already serialized — the server does not
/// know which method it is answering by the time it writes, and it does not need
/// to: the client decodes against the type its own call promised. That asymmetry
/// is why this is a `serde_json::Value` rather than a second exhaustive enum
/// mirroring [`Call`]; a mirror would have to be kept in step by hand, and getting
/// it wrong would deserialize one method's answer as another's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// The two shapes a reply can take.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    #[serde(rename = "ok")]
    Ok(serde_json::Value),
    /// The serialized [`LaneError`] — see the module docs for why it is the whole
    /// enum and not a `(kind, message)` pair.
    #[serde(rename = "e")]
    Err(LaneError),
}

/// The refusal a version mismatch produces, on both sides, worded once.
///
/// Deliberately names both numbers and says who is refusing, on the
/// `qrmux/src/protocol/codec.rs:726` model ("protocol version mismatch: client
/// v2, server v3 — refusing connection"). A mismatch is an installation fault —
/// two binaries from different builds — and the message has to be enough for a
/// user to recognise that without reading source.
pub fn version_mismatch(client: u32, server: u32) -> String {
    format!(
        "qw protocol version mismatch: qd speaks v{client}, qw speaks v{server} — \
         refusing. These two binaries are from different builds; reinstall so both \
         come from the same one."
    )
}

/// Encode one frame as a protocol LINE (trailing `\n` included).
///
/// Serialization cannot fail for these types — every field is plain owned data —
/// but a panic here would be a wedged pipe rather than an error, so it degrades to
/// a transport error the peer can report.
pub fn encode<T: Serialize>(frame: &T) -> Result<String, LaneError> {
    serde_json::to_string(frame)
        .map(|mut s| {
            s.push('\n');
            s
        })
        .map_err(|e| LaneError::Transport {
            detail: format!("could not encode a qw wire frame: {e}"),
        })
}

/// Decode one protocol line.
pub fn decode<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, LaneError> {
    serde_json::from_str(line).map_err(|e| LaneError::Transport {
        detail: format!("could not decode a qw wire frame: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::TerminalExpectation;

    /// Every [`Call`] variant survives a round trip, and the tag is `m`.
    ///
    /// This is guard 1's discipline (`conformance::every_dto_round_trips_through_json`)
    /// applied to the frames themselves: the DTOs were already covered there, but a
    /// frame can be unencodable while every field in it round-trips — `#[serde(flatten)]`
    /// over an internally-tagged enum is exactly the combination that silently
    /// misbehaves, so it is pinned rather than assumed.
    #[test]
    fn every_call_round_trips_and_tags_on_m() {
        let lane = Lane::from_id("claude-code/mux-pane").expect("a real lane");
        let calls = vec![
            Call::Hello { v: WIRE_VERSION },
            Call::Start {
                lane,
                req: StartRequest {
                    name: "alpha".into(),
                    ..Default::default()
                },
            },
            Call::Wake {
                lane,
                session: SessionId("s".into()),
                render: RenderMode::default(),
                cwd_override: Some("/tmp".into()),
            },
            Call::Kill {
                lane,
                session: SessionId("s".into()),
            },
            Call::List { lane },
            Call::Health {
                lane,
                session: SessionId("s".into()),
            },
            Call::ReceivePath {
                lane,
                session: SessionId("s".into()),
            },
            Call::Deliver {
                lane,
                session: SessionId("s".into()),
                msg: Message {
                    id: MessageId("m".into()),
                    text: "hi".into(),
                    from: None,
                },
                policy: DeliverPolicy::default(),
            },
            Call::AwaitTerminal {
                lane,
                session: SessionId("s".into()),
                message_id: MessageId("m".into()),
                budget_ms: 1000,
            },
            Call::Recover {
                lane,
                at: LedgerAddress::byname("alpha"),
                message_id: MessageId("m".into()),
            },
            Call::AwaitIdle {
                lane,
                session: SessionId("s".into()),
                budget_ms: 120_000,
            },
            Call::Resolved {
                lane,
                at: LedgerAddress::byname("alpha"),
                message_id: MessageId("m".into()),
            },
            Call::AttachPrecheck {
                lane,
                session: SessionId("s".into()),
            },
        ];
        // The list above must name EVERY variant. A `Call` added without a case
        // here would be a frame nothing round-trips — which is how the duplicate
        // `id` collision would have shipped if only some variants were covered.
        assert_eq!(
            calls.len(),
            13,
            "every Call variant needs a case here; add the new one rather than \
             bumping this number without looking"
        );
        for call in calls {
            let req = Request {
                id: 7,
                call: call.clone(),
            };
            let line = encode(&req).expect("encode");
            assert!(line.ends_with('\n'), "a frame is one LINE: {line:?}");
            assert_eq!(line.matches('\n').count(), 1, "no embedded newline: {line:?}");
            let back: Request = decode(line.trim_end()).expect("decode");
            assert_eq!(back, req, "round trip changed the frame");
            // The tag rides beside `id`, not nested — the protocol block's shape.
            let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(v.get("id").and_then(|x| x.as_u64()), Some(7));
            assert!(v.get("m").is_some(), "frame must be tagged on `m`: {v}");
        }
    }

    /// The error half is the whole enum, so the attribution flag survives.
    ///
    /// This is the deviation from the house `{k,m}` envelope, and it is pinned
    /// because the flag is invisible in the common case: a `(kind, message)`
    /// flattening loses it silently and qd starts printing `qd resume: ERROR: …`
    /// over a line the core already attributed.
    #[test]
    fn a_wake_failure_keeps_its_attribution_across_the_wire() {
        let reply = Reply {
            id: 3,
            outcome: Outcome::Err(LaneError::WakeFailed {
                detail: "ERROR: the session's recorded directory no longer exists".into(),
                exit_code: 1,
                self_attributed: true,
            }),
        };
        let line = encode(&reply).expect("encode");
        let back: Reply = decode(line.trim_end()).expect("decode");
        assert_eq!(back, reply);
        match back.outcome {
            Outcome::Err(LaneError::WakeFailed {
                self_attributed, ..
            }) => assert!(self_attributed, "the attribution flag did not survive"),
            other => panic!("wrong outcome: {other:?}"),
        }
    }

    #[test]
    fn an_ok_reply_carries_the_methods_own_return_value() {
        let receipt = crate::contract::Receipt {
            message_id: MessageId("m1".into()),
            accepted: true,
            terminal: TerminalExpectation::Pending,
            woke: crate::contract::Confirmation::No,
        };
        let reply = Reply {
            id: 1,
            outcome: Outcome::Ok(serde_json::to_value(&receipt).unwrap()),
        };
        let line = encode(&reply).expect("encode");
        let back: Reply = decode(line.trim_end()).expect("decode");
        match back.outcome {
            Outcome::Ok(v) => {
                let got: crate::contract::Receipt = serde_json::from_value(v).expect("decode");
                assert_eq!(got, receipt);
            }
            other => panic!("wrong outcome: {other:?}"),
        }
    }

    /// A lane names itself on the wire as its stable string id, never as a struct.
    /// `lane.rs:151-152` rules that; the wire is the reason the rule exists, so it
    /// is asserted here too rather than only where the type is defined.
    #[test]
    fn a_lane_crosses_as_its_stable_string_id() {
        let lane = Lane::from_id("acp/claude-code/daemon").expect("a real lane");
        let line = encode(&Request {
            id: 1,
            call: Call::List { lane },
        })
        .unwrap();
        assert!(
            line.contains("\"lane\":\"acp/claude-code/daemon\""),
            "lane must cross as its id: {line}"
        );
    }

    #[test]
    fn the_mismatch_refusal_names_both_versions_and_the_cause() {
        let m = version_mismatch(1, 2);
        assert!(m.contains("v1") && m.contains("v2"), "{m}");
        assert!(m.contains("refusing"), "{m}");
        assert!(m.contains("reinstall"), "a user must learn what to DO: {m}");
    }
}
