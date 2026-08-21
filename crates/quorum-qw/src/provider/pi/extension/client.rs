//! The `quorum-lane` control-channel client — `qw`'s half of the socket the pi
//! extension serves.
//!
//! # The wire
//!
//! Newline-delimited JSON over a unix stream socket, one request per line, one
//! response per line — the same framing `qw serve` itself speaks. A request is
//! `{"id":<n>,"m":"<verb>",…}`; its response is `{"id":<n>,"ok":{…}}` or
//! `{"id":<n>,"err":{"code":…,"detail":…}}`. The full verb table lives beside
//! the server, in `assets/pi-extension/quorum-lane.ts`.
//!
//! # Why synchronous, in a crate that has tokio
//!
//! Every `LaneOps` body that reaches this module is synchronous, and the socket
//! is a loopback filesystem object with a live process on the other end. An
//! async client would need a runtime to be driven from those bodies, and would
//! buy nothing: there is exactly one connection, one outstanding request, and a
//! deadline enforced by `SO_RCVTIMEO`. `std::os::unix::net` is the whole
//! dependency.
//!
//! # Deadlines are mandatory, never optional
//!
//! Every connection sets both a read and a write timeout before the first byte
//! moves. The peer is a TUI that may be mid-render, blocked on a model call, or
//! stopped at a modal dialog the human has not answered — none of which it is
//! obliged to tell us about. A `deliver` with no deadline would hang a verb
//! body indefinitely against a session that is, from the user's point of view,
//! working perfectly.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How long to wait for a reply to an ordinary request.
///
/// Generous by the standards of a loopback socket because the server is a
/// single-threaded node event loop shared with a rendering TUI: it answers in
/// microseconds when idle and can be tens of milliseconds late mid-stream.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the socket to appear after a launch.
///
/// Measured, not guessed: with a warm jiti cache the socket appears ~0.8s after
/// spawn (pi 0.84.1, this machine). A COLD cache is the case this budget exists
/// for — jiti transpiles the extension's TypeScript on first load after an
/// install or an upgrade, which was observed to exceed 30s. A create that timed
/// out there would report a broken lane for what is really a one-time compile.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(90);

/// What went wrong talking to the extension.
#[derive(Debug)]
pub enum ClientError {
    /// No socket at that path, or nothing listening on it. The usual cause is a
    /// session whose pi process is gone — i.e. a cold row.
    NotListening { path: String, detail: String },
    /// Connected, but the exchange failed (timeout, EOF mid-frame, unparseable
    /// response).
    Transport { detail: String },
    /// The extension answered, and its answer was an error frame.
    Refused { code: String, detail: String },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NotListening { path, detail } => {
                write!(f, "no quorum-lane control channel at {path}: {detail}")
            }
            ClientError::Transport { detail } => write!(f, "control channel: {detail}"),
            ClientError::Refused { code, detail } => {
                write!(f, "control channel refused ({code}): {detail}")
            }
        }
    }
}

/// One connection to one pi session's control channel.
///
/// Short-lived by design: a verb body opens it, asks, and drops it. Nothing in
/// the lane holds one across calls, so a session that restarts between two
/// verbs is picked up by the next `connect` rather than leaving a stale handle
/// to detect. The one exception is [`Client::await_idle`], which must hold the
/// connection open for the duration of a turn because that is what it is
/// waiting on.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    /// Connect, with deadlines set before any byte moves.
    pub fn connect(path: &Path) -> Result<Client, ClientError> {
        Client::connect_with(path, CALL_TIMEOUT)
    }

    pub fn connect_with(path: &Path, timeout: Duration) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(path).map_err(|e| ClientError::NotListening {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        // Both directions. A write deadline matters as much as a read one: the
        // peer's receive buffer can fill if its event loop is wedged, and a
        // blocking `write_all` would hang exactly as a blocking read would.
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .map_err(|e| ClientError::Transport {
                detail: format!("cannot set socket deadline: {e}"),
            })?;
        let reader = BufReader::new(stream.try_clone().map_err(|e| ClientError::Transport {
            detail: format!("cannot clone socket: {e}"),
        })?);
        Ok(Client {
            stream,
            reader,
            next_id: 1,
        })
    }

    /// Wait for the socket to exist AND answer a `hello`, then hand back the
    /// connected client.
    ///
    /// Both halves are required. A socket file exists from the instant
    /// `listen(2)` returns, which is BEFORE the extension has captured its pi
    /// context — so "the path exists" is not "the session can be driven". The
    /// handshake is what makes the readiness real, and it is why this returns
    /// the live client rather than making the caller reconnect to a channel it
    /// just proved was up.
    pub fn wait_ready(path: &Path, budget: Duration) -> Result<(Client, Value), ClientError> {
        let deadline = Instant::now() + budget;
        let mut last = String::from("never attempted");
        while Instant::now() < deadline {
            match Client::connect(path) {
                Ok(mut c) => match c.hello() {
                    Ok(v) => return Ok((c, v)),
                    Err(e) => last = e.to_string(),
                },
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(ClientError::NotListening {
            path: path.display().to_string(),
            detail: format!("not ready within {}s: {last}", budget.as_secs()),
        })
    }

    /// Send one request, read one response, unwrap `ok`/`err`.
    pub fn call(&mut self, verb: &str, extra: Value) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        let mut req = json!({ "id": id, "m": verb });
        if let Value::Object(fields) = extra {
            let obj = req.as_object_mut().expect("req is an object");
            for (k, v) in fields {
                obj.insert(k, v);
            }
        }

        let mut line = serde_json::to_string(&req).map_err(|e| ClientError::Transport {
            detail: format!("cannot encode request: {e}"),
        })?;
        line.push('\n');
        self.stream
            .write_all(line.as_bytes())
            .and_then(|()| self.stream.flush())
            .map_err(|e| ClientError::Transport {
                detail: format!("write failed: {e}"),
            })?;

        // Read until the frame carrying OUR id. A connection that never sent
        // `subscribe` receives nothing else, so in practice this reads exactly
        // one line — but skipping rather than assuming is what keeps a caller
        // that DID subscribe from decoding a status event as its answer. That
        // exact bug is live elsewhere in this repo (`wire/client.rs` reads one
        // line and decodes it), and it is cheap not to repeat here.
        loop {
            let v = self.read_frame()?;
            if v.get("ev").is_some() && v.get("id").is_none() {
                continue; // an unsolicited status frame; not our answer
            }
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("err") {
                return Err(ClientError::Refused {
                    code: err
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    detail: err
                        .get("detail")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                });
            }
            return Ok(v.get("ok").cloned().unwrap_or(Value::Null));
        }
    }

    fn read_frame(&mut self) -> Result<Value, ClientError> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .map_err(|e| ClientError::Transport {
                detail: format!("read failed: {e}"),
            })?;
        if n == 0 {
            return Err(ClientError::Transport {
                detail: "control channel closed mid-exchange".to_string(),
            });
        }
        serde_json::from_str(line.trim()).map_err(|e| ClientError::Transport {
            detail: format!("unparseable frame {:?}: {e}", line.trim()),
        })
    }

    /// Handshake. Answers the extension's wire version and the session identity
    /// it believes it is serving.
    pub fn hello(&mut self) -> Result<Value, ClientError> {
        self.call("hello", json!({}))
    }

    /// Live status straight from pi's own `isIdle()`.
    pub fn health(&mut self) -> Result<Health, ClientError> {
        let v = self.call("health", json!({}))?;
        Ok(Health {
            busy: v.get("status").and_then(Value::as_str) == Some("busy"),
            turns: v.get("turns").and_then(Value::as_u64).unwrap_or(0),
            pending: v
                .get("pending")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// Deliver a user message — a REAL user turn, indistinguishable from one the
    /// human typed, because it is one: the extension calls pi's own
    /// `sendUserMessage`.
    ///
    /// `deliver_as` is left to the server unless the caller insists. The server
    /// picks `steer` while a turn is streaming and immediate delivery when idle,
    /// which is the choice that always works; pi REQUIRES a mode while streaming
    /// and rejects one when idle, so deciding it here — a round trip earlier
    /// than the status it depends on — would be deciding it on stale
    /// information.
    pub fn deliver(&mut self, text: &str, deliver_as: Option<&str>) -> Result<(), ClientError> {
        let mut extra = json!({ "text": text });
        if let Some(mode) = deliver_as {
            extra["deliver_as"] = json!(mode);
        }
        self.call("deliver", extra)?;
        Ok(())
    }

    /// Stop the current run. Answers whether there was one to stop.
    pub fn interrupt(&mut self) -> Result<bool, ClientError> {
        let v = self.call("interrupt", json!({}))?;
        Ok(v.get("aborted").and_then(Value::as_bool).unwrap_or(false))
    }

    /// Block until pi reports itself settled, or the budget expires.
    ///
    /// Subscribes and waits for the pushed `idle` frame rather than polling
    /// `health`, so the answer arrives at the instant pi settles instead of up
    /// to one poll interval later.
    ///
    /// **`agent_settled`, not `agent_end`** — the extension broadcasts `idle`
    /// from the former. pi may auto-retry, auto-compact and retry, or continue
    /// with queued follow-ups after `agent_end`, so a lane that returned there
    /// would hand back control mid-turn. That choice lives in the server; it is
    /// restated here because this is the function whose correctness depends on
    /// it.
    ///
    /// The subscribe response carries the CURRENT status, which closes the race
    /// where a turn settles between the caller's decision to wait and this
    /// subscription landing: an already-idle session returns immediately rather
    /// than waiting for an edge that has already passed.
    pub fn await_idle(path: &Path, budget: Duration) -> Result<AwaitOutcome, ClientError> {
        // Its own connection, with the read deadline set to the WHOLE budget:
        // this call is meant to block for as long as a turn takes, which is
        // categorically longer than `CALL_TIMEOUT`.
        let mut c = Client::connect_with(path, budget)?;
        let sub = c.call("subscribe", json!({}))?;
        if sub.get("status").and_then(Value::as_str) == Some("idle") {
            return Ok(AwaitOutcome::AlreadyIdle);
        }

        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            let v = match c.read_frame() {
                Ok(v) => v,
                // The peer going away mid-wait is the session ending, which is
                // a legitimate terminal state for a wait — not a transport bug.
                Err(ClientError::Transport { detail }) if detail.contains("closed") => {
                    return Ok(AwaitOutcome::Vanished)
                }
                Err(e) => return Err(e),
            };
            if v.get("ev").and_then(Value::as_str) == Some("idle") {
                return Ok(AwaitOutcome::WentIdle);
            }
        }
        Ok(AwaitOutcome::TimedOut)
    }
}

/// What the control channel says about right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    pub busy: bool,
    /// Turns this pi PROCESS has seen — not the session's lifetime total. The
    /// extension counts `turn_end` in memory, so a revived session restarts at
    /// zero. Reported as an observation, never as the session's turn count.
    pub turns: u64,
    pub pending: bool,
}

/// How an [`Client::await_idle`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwaitOutcome {
    /// Already settled when we subscribed — the race this lane closes on purpose.
    AlreadyIdle,
    /// Observed the transition.
    WentIdle,
    /// The budget expired with pi still working.
    TimedOut,
    /// The session went away while we waited.
    Vanished,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    /// A scripted stand-in for the extension: reads request lines and replies
    /// with whatever the script says, so the CLIENT's framing is what is under
    /// test rather than node's.
    fn serve(replies: Vec<String>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let l = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            for reply in replies {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let _ = w.write_all((reply + "\n").as_bytes());
                let _ = w.flush();
            }
        });
        (dir, path)
    }

    #[test]
    fn call_unwraps_ok() {
        let (_d, p) = serve(vec![r#"{"id":1,"ok":{"status":"idle","turns":3}}"#.into()]);
        let mut c = Client::connect(&p).unwrap();
        let h = c.health().unwrap();
        assert!(!h.busy);
        assert_eq!(h.turns, 3);
    }

    #[test]
    fn call_surfaces_err_frames_as_refused() {
        let (_d, p) = serve(vec![
            r#"{"id":1,"err":{"code":"bad-request","detail":"no text"}}"#.into(),
        ]);
        let mut c = Client::connect(&p).unwrap();
        match c.deliver("", None) {
            Err(ClientError::Refused { code, .. }) => assert_eq!(code, "bad-request"),
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    /// THE regression this client exists to not have. A subscribed connection
    /// interleaves unsolicited `ev` frames with replies; reading exactly one
    /// line would decode the event as the answer and report a transport failure
    /// for a call that succeeded.
    #[test]
    fn status_frames_do_not_masquerade_as_replies() {
        let (_d, p) = serve(vec![format!(
            "{}\n{}\n{}",
            r#"{"ev":"busy","turns":0}"#,
            r#"{"ev":"idle","turns":1}"#,
            r#"{"id":1,"ok":{"status":"idle","turns":1}}"#
        )]);
        let mut c = Client::connect(&p).unwrap();
        let h = c.health().unwrap();
        assert_eq!(h.turns, 1, "must skip the two event frames");
    }

    #[test]
    fn absent_socket_is_not_listening_not_transport() {
        let dir = tempfile::tempdir().unwrap();
        match Client::connect(&dir.path().join("nope.sock")) {
            Err(ClientError::NotListening { .. }) => {}
            other => panic!("expected NotListening, got {other:?}"),
        }
    }

    #[test]
    fn wait_ready_gives_up_with_the_last_reason() {
        let dir = tempfile::tempdir().unwrap();
        let e = Client::wait_ready(&dir.path().join("nope.sock"), Duration::from_millis(250))
            .unwrap_err();
        assert!(
            matches!(e, ClientError::NotListening { .. }),
            "got {e:?}"
        );
    }
}
