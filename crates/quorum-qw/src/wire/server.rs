//! The `qw` side of the wire: decode a call, run it against a real lane, answer.
//!
//! This is what the `qw` binary's `serve` verb runs. It is deliberately tiny —
//! every decision it could make is one the boundary already made:
//!
//! - **Which lane** is named in the frame. `qd` resolves the harness and hosting
//!   mode from the registry row it already holds, and sends a [`Lane`]; qw does
//!   not re-derive it. That keeps `lane_for`'s precedence in exactly one place.
//! - **What effects to use** is not a choice either: qw rebuilds them from its own
//!   environment. `Env` is the process environ, `QdPaths` comes off `HOME`/`QD_HOME`,
//!   and the mux is `build_real_mux`, which reads `QD_MUX`. All three are
//!   env-derived, which is why the wire carries none of them — see
//!   [`crate::lanes::LaneImpl`]'s fields.
//!
//! The one thing this file must get right is that **stdout is the protocol**.
//! Anything a lane prints to stdout would be read by qd as a frame. Lanes print to
//! stderr, which is inherited; the module docs on [`super`] record why.

use std::io::{BufRead, Write};

use crate::contract::{LaneError, LaneOps};
use crate::effects::Env;
use crate::paths::QdPaths;

use super::{decode, encode, Call, Outcome, Reply, Request, WIRE_VERSION};

/// Run one call against a real lane and produce its answer, already serialized.
///
/// The `match` is exhaustive over [`Call`], which is the point: a method added to
/// [`LaneOps`] and to [`Call`] but not wired here fails to compile. A string-keyed
/// dispatcher would have answered "unknown method" at runtime instead, on a user's
/// machine, after the release.
///
/// [`Call::Hello`] is handled by the read loop rather than here — it is a
/// connection-level frame with no lane, and giving it a lane-shaped arm would mean
/// inventing one.
pub fn dispatch(env: &dyn Env, paths: &QdPaths, call: Call) -> Outcome {
    /// Serialize a method's `Result` into an [`Outcome`].
    ///
    /// A serialization failure becomes `Transport` rather than a panic: qw's job
    /// on the far side of a pipe is to always answer, because a peer that writes
    /// nothing leaves qd blocked on a read with no way to tell a slow call from a
    /// dead one.
    fn out<T: serde::Serialize>(r: Result<T, LaneError>) -> Outcome {
        match r {
            Ok(v) => match serde_json::to_value(v) {
                Ok(v) => Outcome::Ok(v),
                Err(e) => Outcome::Err(LaneError::Transport {
                    detail: format!("qw could not serialize its answer: {e}"),
                }),
            },
            Err(e) => Outcome::Err(e),
        }
    }

    match call {
        // Handled by the read loop; an arm exists so the match stays exhaustive
        // and a future reader does not conclude the frame is unhandled.
        Call::Hello { .. } => Outcome::Ok(serde_json::json!({ "v": WIRE_VERSION })),

        Call::Start { lane, req } => {
            out(crate::lanes::lane_ops(lane, env, paths.clone()).start(&req))
        }
        Call::Wake {
            lane,
            session,
            render,
            cwd_override,
        } => out(crate::lanes::lane_ops(lane, env, paths.clone()).wake(&session, render, cwd_override)),
        Call::Kill { lane, session } => out(crate::lanes::lane_ops(lane, env, paths.clone()).kill(&session)),
        Call::List { lane } => out(crate::lanes::lane_ops(lane, env, paths.clone()).list()),
        Call::Health { lane, session } => {
            out(crate::lanes::lane_ops(lane, env, paths.clone()).health(&session))
        }
        Call::ReceivePath { lane, session } => {
            out(crate::lanes::lane_ops(lane, env, paths.clone()).receive_path(&session))
        }
        Call::Deliver {
            lane,
            session,
            msg,
            policy,
        } => out(crate::lanes::lane_ops(lane, env, paths.clone()).deliver(&session, &msg, &policy)),
        Call::AwaitTerminal {
            lane,
            session,
            message_id,
            budget_ms,
        } => out(
            crate::lanes::lane_ops(lane, env, paths.clone()).await_terminal(
                &session,
                &message_id,
                budget_ms,
            ),
        ),
        Call::Recover {
            lane,
            at,
            message_id,
        } => out(crate::lanes::lane_ops(lane, env, paths.clone()).recover(&at, &message_id)),
        Call::AwaitIdle {
            lane,
            session,
            budget_ms,
        } => out(crate::lanes::lane_ops(lane, env, paths.clone()).await_idle(&session, budget_ms)),
        Call::Resolved {
            lane,
            at,
            message_id,
        } => out(crate::lanes::lane_ops(lane, env, paths.clone()).resolved(&at, &message_id)),
        // Answers unit on success: qd needs only "may I hand off", and returning
        // the pane coordinates would hand qd a session detail it has no business
        // holding — the same reason `SessionSummary.provider` is a String.
        Call::AttachPrecheck { lane, session } => out(crate::lanes::lane_ops(lane, env, paths.clone())
            .attach_target(&session)
            .map(|_| ())),
    }
}

/// Serve the protocol over one reader/writer pair until the peer closes.
///
/// # The handshake is enforced here, on the FIRST frame
///
/// A peer whose first frame is not [`Call::Hello`], or whose version differs,
/// is refused and the loop ends. Refusing on frame 1 rather than lazily is what
/// makes the refusal legible: a mismatched pair fails immediately with a message
/// naming both versions, instead of failing later inside whichever method first
/// hit a changed field, where the error would describe a decode failure rather
/// than the real cause.
///
/// Serving more than one call per connection costs nothing and is not premature:
/// the loop is the same either way, and it is what lets `qw` become resident later
/// without the protocol changing — the sequencing doc's "if per-call spawn later
/// costs too much, qw can become resident behind the same interface."
pub fn serve(
    env: &dyn Env,
    paths: &QdPaths,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> std::io::Result<()> {
    // From here on stdout is the PROTOCOL (the invariant in this module's docs).
    // A carrier's user-facing stdout line would otherwise be read by qd as a
    // frame; this routes it to inherited stderr instead. See
    // [`crate::delivery::stdout_is_protocol`].
    crate::delivery::stdout_is_protocol();
    let mut greeted = false;

    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(()); // peer closed — a normal end, not an error
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        let req: Request = match decode(trimmed) {
            Ok(r) => r,
            Err(e) => {
                // No id to correlate with, so answer on id 0 and stop: a peer
                // emitting unparseable frames is not one we can keep serving.
                let reply = Reply {
                    id: 0,
                    outcome: Outcome::Err(e),
                };
                write_frame(output, &reply)?;
                return Ok(());
            }
        };

        let outcome = match (&req.call, greeted) {
            (Call::Hello { v }, _) => {
                if *v != WIRE_VERSION {
                    let reply = Reply {
                        id: req.id,
                        outcome: Outcome::Err(LaneError::Refused {
                            detail: super::version_mismatch(*v, WIRE_VERSION),
                        }),
                    };
                    write_frame(output, &reply)?;
                    return Ok(());
                }
                greeted = true;
                Outcome::Ok(serde_json::json!({ "v": WIRE_VERSION }))
            }
            (_, false) => {
                // A call before the handshake. Refuse rather than serve it: the
                // handshake is what proves the two sides agree on what the frame
                // MEANS, and serving one frame without that agreement is the
                // silent-skew failure the version lock exists to prevent.
                let reply = Reply {
                    id: req.id,
                    outcome: Outcome::Err(LaneError::Refused {
                        detail: "qw: first frame must be `hello` — refusing an un-negotiated call"
                            .to_string(),
                    }),
                };
                write_frame(output, &reply)?;
                return Ok(());
            }
            (_, true) => dispatch(env, paths, req.call),
        };

        write_frame(
            output,
            &Reply {
                id: req.id,
                outcome,
            },
        )?;
    }
}

/// Write one frame and flush. **Flushing is load-bearing**: qd blocks on a read
/// until the newline arrives, so a buffered reply is a hang, not a slowdown.
fn write_frame(output: &mut dyn Write, reply: &Reply) -> std::io::Result<()> {
    let line = encode(reply).map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    output.write_all(line.as_bytes())?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use std::io::Cursor;

    fn paths() -> QdPaths {
        QdPaths::from_home(std::path::Path::new("/tmp/qw-wire-test-home"))
    }

    /// Drive `serve` over canned input and return the reply lines.
    fn serve_lines(input: &str) -> Vec<Reply> {
        let env = MapEnv {
            vars: Default::default(),
            uid: 501,
        };
        let mut out: Vec<u8> = Vec::new();
        let mut cur = Cursor::new(input.as_bytes().to_vec());
        serve(&env, &paths(), &mut cur, &mut out).expect("serve");
        String::from_utf8(out)
            .expect("utf8")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| decode::<Reply>(l).expect("decode reply"))
            .collect()
    }

    #[test]
    fn the_handshake_is_answered_and_the_version_echoed() {
        let hello = encode(&Request {
            id: 1,
            call: Call::Hello { v: WIRE_VERSION },
        })
        .unwrap();
        let replies = serve_lines(&hello);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, 1);
        match &replies[0].outcome {
            Outcome::Ok(v) => assert_eq!(v.get("v").and_then(|x| x.as_u64()), Some(1)),
            other => panic!("handshake refused: {other:?}"),
        }
    }

    /// The loud refusal, and the fact that it ENDS the connection.
    ///
    /// Answering-and-continuing would leave a skewed peer able to issue calls, and
    /// the whole point of version-locking is that it cannot.
    #[test]
    fn a_version_mismatch_refuses_loudly_and_stops_serving() {
        let mut input = encode(&Request {
            id: 1,
            call: Call::Hello { v: WIRE_VERSION + 9 },
        })
        .unwrap();
        input.push_str(
            &encode(&Request {
                id: 2,
                call: Call::List {
                    lane: crate::lane::Lane::from_id("claude-code/mux-pane").unwrap(),
                },
            })
            .unwrap(),
        );
        let replies = serve_lines(&input);
        assert_eq!(replies.len(), 1, "the second call must never be served");
        match &replies[0].outcome {
            Outcome::Err(LaneError::Refused { detail }) => {
                assert!(detail.contains("version mismatch"), "{detail}");
                assert!(detail.contains("v10") && detail.contains("v1"), "{detail}");
            }
            other => panic!("expected a refusal: {other:?}"),
        }
    }

    /// A call before `hello` is refused. Without this the handshake would be
    /// advisory — a skewed peer could simply skip it.
    #[test]
    fn a_call_before_the_handshake_is_refused() {
        let input = encode(&Request {
            id: 5,
            call: Call::List {
                lane: crate::lane::Lane::from_id("claude-code/mux-pane").unwrap(),
            },
        })
        .unwrap();
        let replies = serve_lines(&input);
        assert_eq!(replies.len(), 1);
        match &replies[0].outcome {
            Outcome::Err(LaneError::Refused { detail }) => {
                assert!(detail.contains("hello"), "{detail}")
            }
            other => panic!("expected a refusal: {other:?}"),
        }
    }

    /// An unparseable frame is answered (on id 0) rather than ignored, then the
    /// connection ends. A peer that gets no answer at all blocks forever.
    #[test]
    fn garbage_is_answered_not_ignored() {
        let replies = serve_lines("{not json\n");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, 0);
        assert!(matches!(
            replies[0].outcome,
            Outcome::Err(LaneError::Transport { .. })
        ));
    }

    /// Correlation ids come back on the reply, and more than one call may ride a
    /// single connection — the property that lets qw become resident later without
    /// the protocol changing.
    #[test]
    fn ids_correlate_across_several_calls_on_one_connection() {
        let mut input = encode(&Request {
            id: 1,
            call: Call::Hello { v: WIRE_VERSION },
        })
        .unwrap();
        for id in [42u64, 43] {
            input.push_str(
                &encode(&Request {
                    id,
                    call: Call::Health {
                        lane: crate::lane::Lane::from_id("claude-code/mux-pane").unwrap(),
                        session: crate::contract::SessionId("nope".into()),
                    },
                })
                .unwrap(),
            );
        }
        let replies = serve_lines(&input);
        assert_eq!(
            replies.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 42, 43]
        );
    }

    /// A peer closing mid-stream is a normal end, not an error — qd drops the
    /// child after a per-call exchange, so this is the COMMON path, not an edge.
    #[test]
    fn a_closed_peer_ends_the_loop_cleanly() {
        let env = MapEnv {
            vars: Default::default(),
            uid: 501,
        };
        let mut out: Vec<u8> = Vec::new();
        let mut empty = Cursor::new(Vec::new());
        serve(&env, &paths(), &mut empty, &mut out).expect("a closed peer is not an error");
        assert!(out.is_empty());
    }
}
