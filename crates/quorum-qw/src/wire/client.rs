//! The `qd` side of the wire: a [`LaneOps`] that is a subprocess.
//!
//! [`WireLane`] is the type that makes the split real. It satisfies the same trait
//! `qd`'s verbs already call, so migrating a call site is a constructor change and
//! nothing else — `lane_ops(lane, env, paths)` becomes `WireLane::new(lane)`, and
//! the verb body does not move.
//!
//! # Per-call subprocess, on purpose
//!
//! Each call spawns `qw`, shakes hands, sends one frame, reads one reply, and
//! drops the child. No daemon, no socket, no lifecycle to manage, nothing to leak
//! — the sequencing doc chose this deliberately, and left the door open: because
//! the seam is an interface rather than a format, `qw` can become resident later
//! without `qd` changing. [`super::server::serve`] already loops, so the resident
//! version needs no protocol change.
//!
//! The cost is one process spawn per lane call. That is the same order as the
//! `pi --mode rpc` and `acp-daemon` spawns this codebase already does on the send
//! path, and it buys a boundary that cannot be crossed by accident.
//!
//! # What the child inherits, and why each one matters
//!
//! - **The environment.** `qw` rebuilds `Env`, `QdPaths` and the mux from it —
//!   `HOME`, `QD_HOME`, `QD_MUX` and the twelve other keys `quorum-qw` reads. Not
//!   filtering it is a decision: a filtered environ would mean maintaining a
//!   parallel list of every key any lane might ever read, and a lane that read one
//!   we forgot would fail in a way no type could catch.
//! - **The cwd.** [`crate::lanes::LaneImpl`] falls back to `current_dir()` when a
//!   row records none, so a `qw` started elsewhere would revive a pane into the
//!   wrong project.
//! - **stderr.** A lane's diagnostics are meant for the user's terminal, and
//!   inheriting is also what keeps them out of the frame stream on stdout.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use crate::contract::{
    DeliverPolicy, Health, KillReport, LaneError, LaneOps, LedgerAddress, Listing, Message,
    MessageId, Receipt, ReceivePath, SessionHandle, SessionId, StartRequest, Terminal, TurnState, WakeOutcome,
};
use crate::lane::Lane;
use crate::launch::RenderMode;

use super::{decode, encode, Call, Outcome, Reply, Request, ATTACH_VERB, SERVE_VERB, WIRE_VERSION};

/// The env var that overrides which `qw` binary to run.
///
/// Exists for tests and for a developer running a locally-built `qw` against an
/// installed `qd`. Production resolves the sibling next to the running `qd`, which
/// is the behaviour that keeps a one-pin install honest — see [`resolve_qw`].
pub const QW_BIN_ENV: &str = "QD_QW_BIN";

/// A lane that lives in another process.
pub struct WireLane {
    lane: Lane,
    program: PathBuf,
}

impl WireLane {
    /// Address `lane` in a `qw` resolved by [`resolve_qw`].
    pub fn new(lane: Lane) -> Result<Self, LaneError> {
        Ok(WireLane {
            lane,
            program: resolve_qw()?,
        })
    }

    /// Address `lane` in a specific `qw` binary.
    pub fn with_program(lane: Lane, program: PathBuf) -> Self {
        WireLane { lane, program }
    }

    /// Spawn `qw serve`, complete the handshake, exchange one frame, and drop the
    /// child.
    ///
    /// Errors are [`LaneError::Transport`] except the ones qw itself reported,
    /// which cross verbatim — that is the whole reason the error half of the
    /// envelope carries the enum rather than a string pair.
    fn call<T: for<'de> serde::Deserialize<'de>>(&self, call: Call) -> Result<T, LaneError> {
        let mut child = self.spawn()?;
        // Bound the child to this call: whatever happens below — a decode failure,
        // an early return, a panic unwinding through — the child is reaped by
        // `Reaper`'s Drop rather than left behind holding a pipe.
        let child = Reaper(&mut child);

        let mut stdin = child.0.stdin.take().ok_or_else(|| LaneError::Transport {
            detail: "qw: could not open the child's stdin".to_string(),
        })?;
        let stdout = child.0.stdout.take().ok_or_else(|| LaneError::Transport {
            detail: "qw: could not open the child's stdout".to_string(),
        })?;
        let mut reader = BufReader::new(stdout);

        // Frame 1 — the version lock. A mismatched pair fails HERE, naming both
        // versions, rather than later inside whichever method first met a changed
        // field.
        write_frame(
            &mut stdin,
            &Request {
                id: 0,
                call: Call::Hello { v: WIRE_VERSION },
            },
        )?;
        let hello: Reply = read_frame(&mut reader)?;
        if let Outcome::Err(e) = hello.outcome {
            return Err(e);
        }

        write_frame(&mut stdin, &Request { id: 1, call })?;
        let reply: Reply = read_frame(&mut reader)?;
        match reply.outcome {
            Outcome::Ok(v) => serde_json::from_value(v).map_err(|e| LaneError::Transport {
                detail: format!("qw answered a shape qd could not read: {e}"),
            }),
            Outcome::Err(e) => Err(e),
        }
    }

    fn spawn(&self) -> Result<Child, LaneError> {
        Command::new(&self.program)
            .arg(SERVE_VERB)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // INHERITED, never piped — see the module docs. A piped stderr would
            // swallow every diagnostic a lane writes.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| LaneError::Transport {
                detail: format!("qw: could not start {}: {e}", self.program.display()),
            })
    }
}

/// Kills and reaps the child on drop, on every path out of a call.
///
/// Without this a decode failure would leak a `qw` holding an open pipe. The
/// `pi --mode rpc` driver does the same thing in its own `Drop`
/// (`provider/pi/stdio/stdio.rs`), and for the same reason.
struct Reaper<'a>(&'a mut Child);

impl Drop for Reaper<'_> {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write_frame(w: &mut dyn Write, req: &Request) -> Result<(), LaneError> {
    let line = encode(req)?;
    w.write_all(line.as_bytes())
        .and_then(|()| w.flush())
        .map_err(|e| LaneError::Transport {
            detail: format!("qw: could not write a frame: {e}"),
        })
}

fn read_frame(r: &mut dyn BufRead) -> Result<Reply, LaneError> {
    let mut line = String::new();
    match r.read_line(&mut line) {
        Ok(0) => Err(LaneError::Transport {
            detail: "qw exited without answering — check its stderr".to_string(),
        }),
        Ok(_) => decode(line.trim_end()),
        Err(e) => Err(LaneError::Transport {
            detail: format!("qw: could not read a frame: {e}"),
        }),
    }
}

/// Find the `qw` binary.
///
/// Order: [`QW_BIN_ENV`], then the sibling of the running executable. **Never
/// `PATH`** — a `qw` found on `PATH` could be from a different install than the
/// `qd` that is running, which is precisely the version skew the handshake exists
/// to catch. Resolving as a sibling makes the common case correct by construction
/// rather than by a runtime check: one directory, one install, one pin.
pub fn resolve_qw() -> Result<PathBuf, LaneError> {
    if let Some(p) = std::env::var_os(QW_BIN_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().map_err(|e| LaneError::Transport {
        detail: format!("qw: could not resolve qd's own path to find its sibling: {e}"),
    })?;
    let dir = exe.parent().ok_or_else(|| LaneError::Transport {
        detail: "qw: qd's own path has no parent directory".to_string(),
    })?;
    Ok(dir.join("qw"))
}

impl LaneOps for WireLane {
    fn start(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        self.call(Call::Start {
            lane: self.lane,
            req: req.clone(),
        })
    }

    fn wake(
        &self,
        id: &SessionId,
        render: RenderMode,
        cwd_override: Option<String>,
    ) -> Result<WakeOutcome, LaneError> {
        self.call(Call::Wake {
            lane: self.lane,
            session: id.clone(),
            render,
            cwd_override,
        })
    }

    fn kill(&self, id: &SessionId) -> Result<KillReport, LaneError> {
        self.call(Call::Kill {
            lane: self.lane,
            session: id.clone(),
        })
    }

    fn list(&self) -> Result<Listing, LaneError> {
        self.call(Call::List { lane: self.lane })
    }

    fn health(&self, id: &SessionId) -> Result<Health, LaneError> {
        self.call(Call::Health {
            lane: self.lane,
            session: id.clone(),
        })
    }

    fn receive_path(&self, id: &SessionId) -> Result<ReceivePath, LaneError> {
        self.call(Call::ReceivePath {
            lane: self.lane,
            session: id.clone(),
        })
    }

    fn deliver(
        &self,
        id: &SessionId,
        msg: &Message,
        policy: &DeliverPolicy,
    ) -> Result<Receipt, LaneError> {
        self.call(Call::Deliver {
            lane: self.lane,
            session: id.clone(),
            msg: msg.clone(),
            policy: policy.clone(),
        })
    }

    /// The one call whose budget outlives the frame.
    ///
    /// `budget_ms` bounds how long **qw** polls, and qd blocks on the read for as
    /// long as that takes. The child must therefore be allowed to live past the
    /// budget rather than being bounded by a shorter client timeout — which is why
    /// there is no per-call read deadline here. A deadline shorter than the budget
    /// would kill a child mid-poll and report a transport failure for a send that
    /// was still resolving.
    fn await_terminal(
        &self,
        id: &SessionId,
        message_id: &MessageId,
        budget_ms: u64,
    ) -> Result<Terminal, LaneError> {
        self.call(Call::AwaitTerminal {
            lane: self.lane,
            session: id.clone(),
            message_id: message_id.clone(),
            budget_ms,
        })
    }

    fn recover(&self, at: &LedgerAddress, message_id: &MessageId) -> Result<Terminal, LaneError> {
        self.call(Call::Recover {
            lane: self.lane,
            at: at.clone(),
            message_id: message_id.clone(),
        })
    }

    /// The OTHER call whose budget outlives the frame — and the one that also
    /// PRINTS while it runs.
    ///
    /// `qw`'s stderr is inherited (see the module docs), so the lane's
    /// `Waiting for <label>...` prefix and any mid-wait diagnostic reach the
    /// user's terminal in real time, interleaved with nothing of qd's. qd closes
    /// the line once this returns. Same no-read-deadline reasoning as
    /// [`WireLane::await_terminal`]: a deadline shorter than the budget would kill
    /// the child mid-watch and report a transport failure for a session that was
    /// still working.
    fn await_idle(&self, id: &SessionId, budget_ms: u64) -> Result<TurnState, LaneError> {
        self.call(Call::AwaitIdle {
            lane: self.lane,
            session: id.clone(),
            budget_ms,
        })
    }

    fn resolved(
        &self,
        at: &LedgerAddress,
        message_id: &MessageId,
    ) -> Result<Option<Terminal>, LaneError> {
        self.call(Call::Resolved {
            lane: self.lane,
            at: at.clone(),
            message_id: message_id.clone(),
        })
    }

    /// **Not a wire call — an exec.**
    ///
    /// `qw attach` inherits this process's stdin, stdout and stderr, so the
    /// controlling terminal passes through and the embedded mux can put the
    /// *calling* process's stdio into raw mode, which is the thing no serialized
    /// plan could describe. See [`LaneOps::attach`] for the design history.
    ///
    /// We wait rather than `exec`ing over ourselves: qd stamps its own ledger
    /// after an attach returns, and a true `exec` would replace the process before
    /// it could. The controlling terminal is inherited either way — that is the
    /// property that matters, and it is what ADR-0011's `ENOTTY` was NOT about.
    ///
    /// # The refusal crosses as data; only the handoff is an exec
    ///
    /// An exec has one exit code to come back with, and two of `attach`'s
    /// refusals are things qd **acts on** rather than prints:
    /// [`LaneError::NotSupported`] makes `qd attach` render a redirect to
    /// `qd send:relay`, and [`LaneError::Cold`] makes it revive the session and
    /// retry. Wiring the exec without them silently deleted both behaviours —
    /// caught by `tests/attach_verb.rs`'s
    /// `attach_codex_without_endpoint_is_daemon_redirect` and
    /// `attach_resolves_auto_named_cold_session`.
    ///
    /// So [`Call::AttachPrecheck`] asks first and the typed error comes back over
    /// the wire; the exec happens only once qw has said it can hand off. The
    /// check-then-exec gap is harmless here — a session going cold in between
    /// makes the exec fail and qw report it, and unlike `deliver` there is no
    /// ledger entry to get wrong.
    fn attach(&self, id: &SessionId) -> Result<i32, LaneError> {
        // The refusal half. `()` on success — qd is told it may hand off, not
        // where the pane is.
        let () = self.call(Call::AttachPrecheck {
            lane: self.lane,
            session: id.clone(),
        })?;
        let status = Command::new(&self.program)
            .arg(ATTACH_VERB)
            .arg(self.lane.id())
            .arg(&id.0)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| LaneError::Transport {
                detail: format!("qw attach: could not start {}: {e}", self.program.display()),
            })?;
        // A signalled child has no code. Report the shell convention (128+signal)
        // rather than inventing a success: an attach killed by SIGHUP did not
        // succeed, and `None` mapped to 0 would say it did.
        Ok(status.code().unwrap_or(129))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env override wins, so a test or a developer can point at a specific
    /// build without a `PATH` game.
    #[test]
    fn the_env_override_selects_the_binary() {
        // SAFETY: single-threaded test process; restored immediately after.
        let prev = std::env::var_os(QW_BIN_ENV);
        unsafe { std::env::set_var(QW_BIN_ENV, "/nonexistent/qw-under-test") };
        let got = resolve_qw().expect("override resolves");
        assert_eq!(got, PathBuf::from("/nonexistent/qw-under-test"));
        match prev {
            Some(v) => unsafe { std::env::set_var(QW_BIN_ENV, v) },
            None => unsafe { std::env::remove_var(QW_BIN_ENV) },
        }
    }

    /// A missing `qw` is a Transport error naming the path, not a panic and not a
    /// silent lane refusal. The message has to name the path because the whole
    /// failure mode is "the sibling is not where qd expected."
    #[test]
    fn a_missing_binary_reports_transport_with_the_path() {
        let lane = Lane::from_id("claude-code/mux-pane").unwrap();
        let w = WireLane::with_program(lane, PathBuf::from("/nonexistent/definitely-not-qw"));
        match w.list() {
            Err(LaneError::Transport { detail }) => {
                assert!(detail.contains("definitely-not-qw"), "{detail}")
            }
            other => panic!("expected a transport error, got {other:?}"),
        }
    }

    /// A child that exits without answering must not hang the caller.
    ///
    /// `true` accepts stdin and closes stdout immediately, which is exactly the
    /// shape of a `qw` that died during startup.
    #[test]
    fn a_child_that_answers_nothing_reports_rather_than_hangs() {
        let lane = Lane::from_id("claude-code/mux-pane").unwrap();
        let w = WireLane::with_program(lane, PathBuf::from("/usr/bin/true"));
        match w.list() {
            Err(LaneError::Transport { detail }) => {
                assert!(detail.contains("without answering"), "{detail}")
            }
            other => panic!("expected a transport error, got {other:?}"),
        }
    }
}
