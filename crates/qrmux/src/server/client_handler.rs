use crate::protocol::{self, ClientMsg, FrameReader, ServerMsg};
use crate::session::SessionManager;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::info;

use super::session_bridge::handle_session;
use super::session_setup::ConnectRequest;

/// Timeout for reading the initial client message (Connect/List/Kill).
/// 30s is generous for network latency while preventing leaked connections
/// from clients that connect but never send anything.
const INITIAL_MSG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default)]
struct NegotiatedCaps {
    logical_history: bool,
    logical_history_stream: bool,
    initial_size_confirm: bool,
}

/// Framed-error string the SINGLE-SESSION daemon returns once it has begun
/// exit-on-session-end teardown (§4.1 step 4): a deterministic refusal, never a
/// hang. The launcher reads this as the "retiring" fourth state (§4.2). Exact
/// string — tests assert on it.
pub const ERR_SESSION_ENDED: &str = "session ended (this daemon is shutting down)";

/// §4.1 lost-reply fix: a shared in-flight counter for one-shot
/// session-addressed request/reply work. The lifecycle exit (exit-on-end,
/// claim-timeout, SIGTERM) WAITS for this to reach zero (bounded) instead of
/// sleeping a fixed grace, so an owed reply is never dropped by the daemon's own
/// teardown. RAII: [`InFlightGuard`] increments on construction and decrements +
/// `notify_waiters()` on drop.
#[derive(Clone, Default)]
pub struct InFlight {
    inner: Arc<(std::sync::atomic::AtomicUsize, tokio::sync::Notify)>,
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enter a guarded one-shot arm: bumps the count, returns the RAII guard.
    pub fn enter(&self) -> InFlightGuard {
        self.inner.0.fetch_add(1, Ordering::AcqRel);
        InFlightGuard {
            inner: self.inner.clone(),
        }
    }

    /// Current in-flight count (for the named fail-closed log line).
    pub fn count(&self) -> usize {
        self.inner.0.load(Ordering::Acquire)
    }

    /// §4.1 [F1 — NORMATIVE, the lost-wakeup trap]: wait until the in-flight
    /// count reaches zero, bounded by `budget`. Returns `true` if it drained,
    /// `false` on timeout (caller logs the named fail-closed line and exits
    /// anyway). The enable-before-check pattern is MANDATORY: the `notified()`
    /// future is created, pinned, and `enable()`d BEFORE the zero-check, so a
    /// `notify_waiters()` that lands between a failed check and registration
    /// cannot be lost (a bare check-then-`notified().await` loop would hang the
    /// full budget on a clean daemon and log a spurious fail-closed line).
    pub async fn wait_for_zero(&self, budget: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let fut = self.inner.1.notified();
            tokio::pin!(fut);
            fut.as_mut().enable(); // register BEFORE the check
            if self.inner.0.load(Ordering::Acquire) == 0 {
                return true;
            }
            // Cannot miss a post-enable notify_waiters(); bounded by the deadline.
            if tokio::time::timeout_at(deadline, fut).await.is_err() {
                // Deadline hit. Re-check once: the count may have reached zero in
                // the same instant the timer fired.
                return self.inner.0.load(Ordering::Acquire) == 0;
            }
        }
    }
}

/// §4.1 lost-reply fix: RAII guard for one guarded one-shot arm. Decrements the
/// in-flight count and broadcasts `notify_waiters()` (NEVER `notify_one`: the
/// waiter side is a broadcast-condition wait) on drop. Held PER DECODED FRAME at
/// the top of a guarded dispatch arm (drops at the arm's `return`/`continue`),
/// and — for `Connect` — across `handle_session` THROUGH `send_initial_state`
/// then dropped BEFORE the long-lived relay select ([F4]). NEVER held across
/// `frames.fill_from` (the inter-frame blocking read).
pub struct InFlightGuard {
    inner: Arc<(std::sync::atomic::AtomicUsize, tokio::sync::Notify)>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.inner.0.fetch_sub(1, Ordering::AcqRel);
        self.inner.1.notify_waiters();
    }
}

/// WS-C M2 (§4.1) + M3b: per-connection daemon context.
///
/// In SINGLE-SESSION mode (the ONLY production mode after the M3b engine flip)
/// `session` is the daemon's `--session` identity; capacity-1 enforcement, the
/// claim-timer reset pulse, and the session-ended refusal all engage.
///
/// The legacy shared-daemon MODE is RETIRED (spec §1, §9): production `run_server`
/// always runs single-session. The `None`-everywhere permissive shape survives
/// ONLY as a TEST fixture ([`DaemonCtx::legacy`], `#[cfg(test)]`) for the handler
/// verb-dispatch unit tests, which are split-agnostic and don't need an identity
/// gate; the capacity-1/identity/claim semantics have their own arms.
#[derive(Clone)]
pub struct DaemonCtx {
    /// The session identity this daemon serves. `None` only in the test-fixture
    /// permissive shape (see [`DaemonCtx::legacy`]); always `Some` in production.
    pub session: Option<Arc<str>>,
    /// Pulsed on every SESSION-ADDRESSED verb to reset the claim timer (§4.1,
    /// red-team B1: Hello-only connections must NOT reset it).
    pub claim_reset: Option<Arc<tokio::sync::Notify>>,
    /// Set true once exit-on-session-end teardown begins (§4.1 step 4): new
    /// connects past the Hello get the named session-ended refusal.
    pub ended: Option<Arc<AtomicBool>>,
    /// §4.1 lost-reply fix: the in-flight counter the lifecycle exit waits on.
    /// `None` only in the test-fixture permissive shape ([`DaemonCtx::legacy`]);
    /// always `Some` in production. `None` means the guard is a no-op (the
    /// handler unit tests don't drive the lifecycle wait).
    pub in_flight: Option<InFlight>,
}

impl DaemonCtx {
    /// TEST-ONLY permissive context: no identity, no claim timer, no end-refusal —
    /// accepts any session name. The legacy shared-daemon production MODE that this
    /// once backed is RETIRED (M3b, spec §9); this constructor survives only as a
    /// fixture for the split-agnostic handler verb-dispatch unit tests.
    #[cfg(test)]
    pub fn legacy() -> Self {
        Self {
            session: None,
            claim_reset: None,
            ended: None,
            in_flight: None,
        }
    }

    /// The ServerHello `session` field: the `--session` value in single-session
    /// mode, else the empty string (the test-fixture permissive shape only).
    fn hello_session(&self) -> String {
        self.session
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    /// Pulse the claim-timer reset (single-session mode only). Called on every
    /// session-addressed verb BEFORE it runs.
    fn pulse_claim_reset(&self) {
        if let Some(n) = &self.claim_reset {
            n.notify_one();
        }
    }

    /// §4.1 lost-reply fix: enter the in-flight guard for a guarded one-shot arm,
    /// if this daemon tracks in-flight work (always in production; `None` only in
    /// the legacy test fixture, where the guard is a harmless no-op).
    fn enter_in_flight(&self) -> Option<InFlightGuard> {
        self.in_flight.as_ref().map(|f| f.enter())
    }

    /// True if teardown has begun (single-session mode); always false in legacy.
    fn session_has_ended(&self) -> bool {
        self.ended
            .as_ref()
            .map(|e| e.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// Capacity-1 identity gate (§4.1): in single-session mode a session-addressed
    /// verb naming a session OTHER than this daemon's identity is rejected with
    /// the exact "not found (this daemon serves ...)" error. In legacy mode every
    /// name is accepted (returns `Ok`).
    fn check_identity(&self, name: &str) -> Result<(), String> {
        // G-ISOL negative-control seam (spec §7, red-team M4): under
        // `QRMUX_TEST_SHARED=1` the capacity-1 gate accepts ANY session name so the
        // single daemon goes multi-session (the SessionManager HashMap machinery is
        // unchanged — it always supported >1). Inert in production (env unset); the
        // socket-leaf collapse (`socket::session_socket_path_for`) is the other half.
        if crate::server::socket::shared_fate_test_mode() {
            return Ok(());
        }
        match &self.session {
            Some(s) if name != &**s => Err(format!(
                "session '{}' not found (this daemon serves '{}')",
                name, s
            )),
            _ => Ok(()),
        }
    }
}

/// The session name a SESSION-ADDRESSED verb targets, or `None` for verbs that
/// are not session-addressed (`ListSessions`, `Hello`, and the attach-only
/// `Input`/`Resize`/`Detach`/`RefreshScreen` which only appear mid-attach).
/// Drives the §4.1 capacity-1 gate and the B1 claim-timer reset rule: only these
/// verbs prove a real claim on the daemon's session.
fn session_addressed_name(msg: &ClientMsg) -> Option<&str> {
    match msg {
        ClientMsg::Connect { name, .. }
        | ClientMsg::SendInput { name, .. }
        | ClientMsg::GetHistory { name }
        | ClientMsg::KillSession { name }
        | ClientMsg::CreateDetached { name, .. }
        | ClientMsg::SubscribeRepublish { name }
        // v5 (attended-UX M1): both delivery frames are session-addressed —
        // they prove a real claim on the daemon's session (§4.1 capacity-1 gate).
        | ClientMsg::PendingDelivery { name, .. }
        | ClientMsg::DeliverNow { name, .. } => Some(name),
        _ => None,
    }
}

/// Dispatch a single client connection by reading its first message and routing accordingly.
pub async fn handle_client(
    mut stream: tokio::net::UnixStream,
    manager: Arc<Mutex<SessionManager>>,
    fault: Arc<super::fault::FaultLayer>,
    ctx: DaemonCtx,
) -> anyhow::Result<()> {
    let mut frames = FrameReader::new();
    let deadline = tokio::time::Instant::now() + INITIAL_MSG_TIMEOUT;

    // Version handshake BEFORE any frames. War story: protocol-version skew is a
    // named failure class (the reason the version byte exists from day 1 — see
    // PROTOCOL.md and exec/b2-spec.md deliverable #3). Mismatch gets a framed
    // Error and a close: a clean refusal, never a hang or a misparse.
    use crate::protocol::{handshake, PreambleCheck};
    match tokio::time::timeout_at(deadline, handshake::read_preamble(&mut stream)).await {
        Ok(Ok(PreambleCheck::Ok)) => {}
        Ok(Ok(PreambleCheck::Eof)) => return Ok(()), // liveness probe; quiet close
        Ok(Ok(PreambleCheck::BadMagic(m))) => {
            let resp = protocol::encode(&ServerMsg::Error(format!(
                "not an qrmux client (bad magic {:02x?})",
                m
            )))?;
            stream.write_all(&resp).await?;
            return Ok(());
        }
        Ok(Ok(PreambleCheck::VersionMismatch { client })) => {
            let resp = protocol::encode(&ServerMsg::Error(format!(
                "protocol version mismatch: client v{}, server v{} — refusing connection",
                client,
                handshake::PROTOCOL_VERSION
            )))?;
            stream.write_all(&resp).await?;
            info!(client_version = client, "refused version-mismatched client");
            return Ok(());
        }
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            tracing::debug!("client timed out before completing preamble");
            return Ok(());
        }
    }

    // v3 Hello-first handshake (PROTOCOL.md §3.2). The FIRST frame after the
    // preamble MUST be ClientMsg::Hello. EOF before any frame (post-preamble
    // connect-and-drop probe) → quiet close, no error. Any other first frame →
    // framed "expected Hello" error, close. Bounds violation → framed Error,
    // close. On success, the server's FIRST reply frame is ServerMsg::Hello.
    let first = loop {
        match tokio::time::timeout_at(deadline, frames.fill_from(&mut stream)).await {
            Ok(Ok(true)) => {}
            // EOF before any frame: quiet close (liveness/readiness probes
            // connect-and-drop after the preamble). Spec §3.2 EOF rule.
            Ok(Ok(false)) => return Ok(()),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                tracing::debug!("client timed out before sending Hello");
                return Ok(());
            }
        }
        if let Some(msg) = frames.decode_next::<ClientMsg>()? {
            break msg;
        }
    };
    let negotiated = match first {
        ClientMsg::Hello { caps } => {
            if let Err(e) = crate::protocol::validate_hello_caps(&caps) {
                let resp = protocol::encode(&ServerMsg::Error(e))?;
                stream.write_all(&resp).await?;
                return Ok(());
            }
            let logical_history = caps
                .iter()
                .any(|cap| cap == crate::protocol::HISTORY_LOGICAL_V1_CAP);
            let logical_history_stream = logical_history
                && caps
                    .iter()
                    .any(|cap| cap == crate::protocol::HISTORY_LOGICAL_STREAM_V1_CAP);
            let initial_size_confirm = caps
                .iter()
                .any(|cap| cap == crate::protocol::INITIAL_SIZE_CONFIRM_V1_CAP);
            // Server's FIRST reply frame. Advertise the additive logical-history
            // surface; emission is still gated by the client's advertisement.
            let resp = protocol::encode(&ServerMsg::Hello {
                caps: vec![
                    crate::protocol::HISTORY_LOGICAL_V1_CAP.to_string(),
                    crate::protocol::HISTORY_LOGICAL_STREAM_V1_CAP.to_string(),
                    crate::protocol::INITIAL_SIZE_CONFIRM_V1_CAP.to_string(),
                ],
                session: ctx.hello_session(),
            })?;
            stream.write_all(&resp).await?;
            NegotiatedCaps {
                logical_history,
                logical_history_stream,
                initial_size_confirm,
            }
        }
        _ => {
            let resp = protocol::encode(&ServerMsg::Error(
                crate::protocol::ERR_EXPECTED_HELLO.into(),
            ))?;
            stream.write_all(&resp).await?;
            return Ok(());
        }
    };

    loop {
        // Decode any frame already buffered (a client may pipeline its first
        // request right behind the Hello — §3.2) BEFORE blocking on a read.
        let msg = match frames.decode_next::<ClientMsg>()? {
            Some(msg) => msg,
            None => match tokio::time::timeout_at(deadline, frames.fill_from(&mut stream)).await {
                Ok(Ok(true)) => continue,
                Ok(Ok(false)) => return Ok(()),
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    tracing::debug!("client timed out waiting for initial message");
                    return Ok(());
                }
            },
        };
        // §4.1 lost-reply fix [F3 — PER-DECODED-FRAME RAII]: enter the in-flight
        // guard at the TOP of the dispatch arm, gated on the owed-reply classes:
        // every session-addressed verb PLUS `ListSessions` (also a one-shot that
        // owes a single reply). The guard covers the ENTIRE arm — including the
        // ended-refusal write and the identity-gate error write below (all ten
        // owed-reply sites the red-team enumerated) — and drops at the arm's
        // `return` (or, for the SendInput fault-drop, its `continue`: iteration
        // scope releases it either way). It is NEVER held across `frames.fill_from`
        // (the inter-frame blocking read above): a connection-scoped guard would
        // let an idle-but-open client extend the daemon's exit (immortal-daemon
        // re-import). `Hello` is NOT guarded (pre-`ended` first-frame, the
        // unclaimed/probe class — guarding it would let bare probes delay exit).
        let mut in_flight_guard =
            if session_addressed_name(&msg).is_some() || matches!(msg, ClientMsg::ListSessions) {
                ctx.enter_in_flight()
            } else {
                None
            };
        // WS-C M2 (§4.1): single-session gates applied to SESSION-ADDRESSED verbs
        // (Connect/SendInput/GetHistory/KillSession/CreateDetached). ListSessions
        // and Hello are NOT session-addressed: they neither reset the claim timer
        // (red-team B1) nor get the capacity/identity gate.
        //
        // Order per §4.1: (4) once teardown has begun, refuse with the named
        // session-ended error (deterministic, never a hang); then the capacity-1
        // identity gate; then pulse the claim-timer reset.
        if let Some(name) = session_addressed_name(&msg) {
            if ctx.session_has_ended() {
                let resp = protocol::encode(&ServerMsg::Error(ERR_SESSION_ENDED.into()))?;
                stream.write_all(&resp).await?;
                return Ok(());
            }
            if let Err(e) = ctx.check_identity(name) {
                let resp = protocol::encode(&ServerMsg::Error(e))?;
                stream.write_all(&resp).await?;
                return Ok(());
            }
            ctx.pulse_claim_reset();
        }
        {
            match msg {
                ClientMsg::Connect {
                    name,
                    history,
                    cols,
                    rows,
                    mode,
                } => {
                    if let Err(e) = crate::session::validate_session_name(&name) {
                        let resp = protocol::encode(&ServerMsg::Error(format!("{}", e)))?;
                        stream.write_all(&resp).await?;
                        return Ok(());
                    }
                    // §4.1 [F4]: the guard is held THROUGH handle_session's
                    // send_initial_state (the initial Connected/Error flush) and
                    // dropped there, BEFORE the long-lived relay select — the
                    // relay phase is excluded (a detached-but-connected client
                    // must not hold the daemon open past its session's death), but
                    // the initial reply IS guarded (a Connect that passes the
                    // ended check can have its session reaped mid-setup_session,
                    // racing the exit exactly like the kill case).
                    return handle_session(
                        stream,
                        manager,
                        ConnectRequest {
                            name,
                            history,
                            cols,
                            rows,
                            leftover: frames.into_leftover(),
                            mode,
                            logical_history: negotiated.logical_history,
                            logical_history_stream: negotiated.logical_history_stream,
                            initial_size_confirm: negotiated.initial_size_confirm,
                        },
                        in_flight_guard.take(),
                    )
                    .await;
                }
                ClientMsg::ListSessions => {
                    let list = manager.lock().await.list();
                    let resp = protocol::encode(&ServerMsg::SessionList(list))?;
                    stream.write_all(&resp).await?;
                    return Ok(());
                }
                ClientMsg::KillSession { name } => {
                    if let Err(e) = crate::session::validate_session_name(&name) {
                        let resp = protocol::encode(&ServerMsg::Error(format!("{}", e)))?;
                        stream.write_all(&resp).await?;
                        return Ok(());
                    }
                    let removed = {
                        let mut mgr = manager.lock().await;
                        mgr.remove(&name)
                    };
                    if let Some(mut session) = removed {
                        // ACK-1 close bookend (spec §2 C1): the kill verb.
                        // Emitted before the drop so the Drop belt's "dropped"
                        // fallback never fires for this path.
                        if let Some(ev) = session.events_writer() {
                            ev.emit_closed(crate::events::CloseReason::Killed);
                        }
                        // Disconnect before dropping the session so the connected
                        // client's watch receiver sees RecvError ("session killed")
                        // instead of the eviction value ("evicted by new client").
                        session.disconnect();
                        super::drop_blocking_with_timeout(
                            session,
                            &format!("kill session '{}'", name),
                        )
                        .await;
                        info!(session = %name, "session killed");
                        let resp = protocol::encode(&ServerMsg::SessionKilled { name })?;
                        stream.write_all(&resp).await?;
                    } else {
                        let resp = protocol::encode(&ServerMsg::Error(format!(
                            "session '{}' not found",
                            name
                        )))?;
                        stream.write_all(&resp).await?;
                    }
                    return Ok(());
                }
                ClientMsg::SendInput { name, data } => {
                    // ACK-1: content sha — the event join key AND the fault
                    // layer's frame filter. Lowercase hex of the EXACT data
                    // bytes (spec §3).
                    let content_sha256 = {
                        use sha2::Digest as _;
                        format!("{:x}", sha2::Sha256::digest(&data))
                    };

                    // Injection 1 (spec §4.1): drop the frame at handler
                    // entry — no validate, no write, no event, NO reply — and
                    // PARK the connection open so a dropped frame looks like
                    // SILENCE (the consumer-side signature is the engine's
                    // T_write timeout, never a connection close). The park
                    // rides the existing INITIAL_MSG_TIMEOUT: the connection
                    // closes quietly at the deadline, far beyond any T_write
                    // window.
                    if fault.should_drop_frame(&name, &content_sha256) {
                        tracing::warn!(
                            session = %name,
                            sha_prefix = &content_sha256[..8.min(content_sha256.len())],
                            "FAULT: dropping SendInput frame (parking connection open)"
                        );
                        continue;
                    }

                    if let Err(e) = crate::session::validate_session_name(&name) {
                        let resp = protocol::encode(&ServerMsg::Error(format!("{}", e)))?;
                        stream.write_all(&resp).await?;
                        return Ok(());
                    }
                    // Out-of-band one-shot input: grab the target session's PTY writer
                    // and write the bytes directly. No attach, no eviction of any
                    // currently-attached client, no screen subscription.
                    // The event writer handle is cloned alongside (spec §2 site W).
                    let target = {
                        let mgr = manager.lock().await;
                        mgr.get(&name)
                            .map(|s| (s.pty_writer_arc(), s.events_writer()))
                    };
                    let resp = match target {
                        Some((pw, events)) => {
                            let n = data.len();
                            let fault = fault.clone();
                            let session_name = name.clone();
                            let sha = content_sha256;
                            let write_result =
                                tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                                    match pw.lock() {
                                        Ok(mut w) => {
                                            // Fault layer BELOW the emitter
                                            // (spec §4.1): the emitter reports
                                            // the result the daemon OBSERVED —
                                            // a swallow is recorded as a
                                            // successful write (injection 3's
                                            // deception, by design).
                                            let res = match fault
                                                .intercept_write(&session_name, &sha)
                                            {
                                                Some(faulted) => faulted,
                                                None => w.write_all(&data).and_then(|_| w.flush()),
                                            };
                                            // Emit while still holding the pty
                                            // writer lock: seq order == PTY
                                            // byte order (spec §2 site W; lock
                                            // order pty→events HERE ONLY).
                                            if let Some(ev) = &events {
                                                match &res {
                                                    Ok(()) => ev.emit_bytes_written(
                                                        n as u64,
                                                        sha.clone(),
                                                        n as u64,
                                                    ),
                                                    Err(e) => ev.emit_write_failed(
                                                        e.raw_os_error(),
                                                        e.to_string(),
                                                        sha.clone(),
                                                        n as u64,
                                                    ),
                                                }
                                            }
                                            res
                                        }
                                        Err(_) => {
                                            let e =
                                                std::io::Error::other("pty_writer mutex poisoned");
                                            // No pty lock to hold here; the
                                            // failure event still carries the
                                            // join keys (errno: None branch).
                                            if let Some(ev) = &events {
                                                ev.emit_write_failed(
                                                    None,
                                                    e.to_string(),
                                                    sha.clone(),
                                                    n as u64,
                                                );
                                            }
                                            Err(e)
                                        }
                                    }
                                })
                                .await?;
                            match write_result {
                                Ok(()) => ServerMsg::InputSent { name, bytes: n },
                                Err(e) => ServerMsg::Error(format!(
                                    "failed to write input to '{}': {}",
                                    name, e
                                )),
                            }
                        }
                        None => ServerMsg::Error(format!("session '{}' not found", name)),
                    };
                    let encoded = protocol::encode(&resp)?;
                    stream.write_all(&encoded).await?;
                    return Ok(());
                }
                ClientMsg::GetHistory { name } => {
                    if let Err(e) = crate::session::validate_session_name(&name) {
                        let resp = protocol::encode(&ServerMsg::Error(format!("{}", e)))?;
                        stream.write_all(&resp).await?;
                        return Ok(());
                    }
                    // One-shot content read: NO attach, no eviction, no screen
                    // subscription. Capable peers receive cell-exact logical
                    // chunks; legacy peers keep the unchanged History reply.
                    let result = {
                        let mgr = manager.lock().await;
                        mgr.get(&name).map(|s| {
                            if negotiated.logical_history {
                                s.logical_history(crate::screen::LogicalEmissionSurface::GetHistory)
                                    .map(|emission| ServerMsg::HistoryLogical(emission.chunks))
                            } else {
                                s.history_lines().map(ServerMsg::History)
                            }
                        })
                    };
                    let resp = match result {
                        Some(Ok(response)) => response,
                        Some(Err(e)) => ServerMsg::Error(format!(
                            "failed to read history for '{}': {}",
                            name, e
                        )),
                        None => ServerMsg::Error(format!("session '{}' not found", name)),
                    };
                    super::session_bridge::write_history_response(
                        &mut stream,
                        resp,
                        negotiated.logical_history_stream,
                    )
                    .await?;
                    return Ok(());
                }
                ClientMsg::CreateDetached {
                    name,
                    shell_cmd,
                    cwd,
                    history,
                } => {
                    if let Err(e) = crate::session::validate_session_name(&name) {
                        let resp = protocol::encode(&ServerMsg::Error(format!("{}", e)))?;
                        stream.write_all(&resp).await?;
                        return Ok(());
                    }
                    // Spawn a detached session running `["bash","-lc",<shell_cmd>]`
                    // in an EXPLICIT cwd (the daemon's own cwd is arbitrary after
                    // setsid). No attach — ack with a Connected-class response
                    // (new_session: true) so the client knows the create
                    // succeeded (protocol v2 / C1 D1/R27).
                    let command = crate::pty::CommandSpec::login_shell_c(&shell_cmd);
                    let resp = {
                        let mut mgr = manager.lock().await;
                        match mgr.create_detached(name.clone(), 0, 0, history, command, cwd) {
                            Ok(()) => {
                                info!(session = %name, "created detached session");
                                ServerMsg::Connected {
                                    name,
                                    new_session: true,
                                }
                            }
                            Err(e) => ServerMsg::Error(format!("{}", e)),
                        }
                    };
                    let encoded = protocol::encode(&resp)?;
                    stream.write_all(&encoded).await?;
                    return Ok(());
                }
                ClientMsg::SubscribeRepublish { name } => {
                    if let Err(e) = crate::session::validate_session_name(&name) {
                        let resp = protocol::encode(&ServerMsg::Error(format!("{}", e)))?;
                        stream.write_all(&resp).await?;
                        return Ok(());
                    }
                    // P4DB drive-burn: the one-off `claude -p` stream-json producer
                    // (the headless session map + its republish hub) was removed. No
                    // daemon hosts a headless republish stream anymore, so this verb
                    // always answers "no session to subscribe to". The verb is
                    // RETAINED (not dropped) because the load-bearing `qd wait`
                    // channel still sends it; on this framed refusal `qd wait` falls
                    // back to its documented disk-poll (channel-DOWN) path.
                    let resp = protocol::encode(&ServerMsg::Error(format!(
                        "no headless session '{}' to subscribe to",
                        name
                    )))?;
                    stream.write_all(&resp).await?;
                    return Ok(());
                }
                // attended-UX M1 (v5): qd hands a send to the mux polite-delivery
                // surface. Write-ahead-spool it (durable ACCEPTANCE — before the
                // receipt), arm the timer, and return the non-terminal queued
                // receipt. A no-`--wait` sender leaves here; the mux resolves the
                // send to exactly one terminal later (and writes it to the ledger).
                ClientMsg::PendingDelivery {
                    send_id,
                    data,
                    content_sha256,
                    content_len,
                    transcript,
                    transcript_offset,
                    session,
                    name,
                    priority,
                } => {
                    if let Err(e) = crate::session::validate_session_name(&name) {
                        let resp = protocol::encode(&ServerMsg::Error(format!("{}", e)))?;
                        stream.write_all(&resp).await?;
                        return Ok(());
                    }
                    let attended = {
                        let mgr = manager.lock().await;
                        mgr.get(&name).and_then(|s| s.attended_arc())
                    };
                    let resp = match attended {
                        Some(att) => {
                            use crate::attended::Clock as _;
                            let now = crate::attended::SystemClock.now_ms();
                            match att.accept(
                                send_id.clone(),
                                data,
                                content_sha256,
                                content_len,
                                transcript,
                                transcript_offset,
                                session,
                                name.clone(),
                                "send:pty",
                                priority,
                                now,
                            ) {
                                Ok(()) => ServerMsg::DeliveryQueued { send_id },
                                Err(e) => ServerMsg::Error(format!(
                                    "failed to spool pending delivery for '{}': {}",
                                    name, e
                                )),
                            }
                        }
                        None => ServerMsg::Error(format!(
                            "session '{}' not found or attended delivery unavailable",
                            name
                        )),
                    };
                    let encoded = protocol::encode(&resp)?;
                    stream.write_all(&encoded).await?;
                    return Ok(());
                }
                // attended-UX M1 (v5): the deliver-now control — fire the target
                // (or earliest) held send NOW, without a countdown reset or a
                // journal entry. Fire-and-forget; the outcome arrives via
                // `DeliveryOutcome`. Ack with `InputSent{bytes:0}` (control
                // accepted; the fire is asynchronous, no synchronous PTY write).
                ClientMsg::DeliverNow { name, send_id } => {
                    if let Err(e) = crate::session::validate_session_name(&name) {
                        let resp = protocol::encode(&ServerMsg::Error(format!("{}", e)))?;
                        stream.write_all(&resp).await?;
                        return Ok(());
                    }
                    let attended = {
                        let mgr = manager.lock().await;
                        mgr.get(&name).and_then(|s| s.attended_arc())
                    };
                    let resp = match attended {
                        Some(att) => {
                            att.deliver_now(send_id);
                            ServerMsg::InputSent { name, bytes: 0 }
                        }
                        None => ServerMsg::Error(format!(
                            "session '{}' not found or attended delivery unavailable",
                            name
                        )),
                    };
                    let encoded = protocol::encode(&resp)?;
                    stream.write_all(&encoded).await?;
                    return Ok(());
                }
                _ => {
                    let resp = protocol::encode(&ServerMsg::Error(
                        "expected Connect, ListSessions, KillSession, SendInput, GetHistory, or \
                         CreateDetached"
                            .into(),
                    ))?;
                    stream.write_all(&resp).await?;
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{self, ClientMsg, FrameReader, ServerMsg};
    use crate::session::SessionManager;
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::Mutex;

    /// Helper: an unarmed (identity) fault layer for tests that don't
    /// exercise injection.
    fn test_fault() -> Arc<super::super::fault::FaultLayer> {
        Arc::new(super::super::fault::FaultLayer::default())
    }

    /// Helper: write the v3 preamble + `ClientMsg::Hello` (the mandatory
    /// first-frame handshake, §3.2). Tests then send their request frame; the
    /// server replies ServerHello first, which [`read_response`] transparently
    /// skips.
    async fn write_preamble_and_hello(stream: &mut tokio::net::UnixStream) {
        protocol::write_preamble(stream).await.unwrap();
        let hello = protocol::encode(&ClientMsg::Hello { caps: vec![] }).unwrap();
        stream.write_all(&hello).await.unwrap();
    }

    /// Helper: read a full response from a UnixStream and deserialize it as a
    /// ServerMsg. The v3 ServerHello (the server's first reply frame, §3.2) is
    /// skipped transparently so existing per-verb assertions read the verb's
    /// actual response.
    async fn read_response(stream: &mut tokio::net::UnixStream) -> ServerMsg {
        let mut reader = FrameReader::new();
        loop {
            assert!(
                reader.fill_from(stream).await.expect("read failed"),
                "connection closed before a full response was received"
            );
            while let Some(msg) = reader.decode_next::<ServerMsg>().expect("decode error") {
                if matches!(msg, ServerMsg::Hello { .. }) {
                    continue; // skip the v3 ServerHello
                }
                return msg;
            }
        }
    }

    #[tokio::test]
    async fn list_sessions_empty() {
        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));

        // Send preamble + ListSessions from client side
        let mut client_stream = client_stream;
        write_preamble_and_hello(&mut client_stream).await;
        let msg = protocol::encode(&ClientMsg::ListSessions).unwrap();
        client_stream.write_all(&msg).await.unwrap();

        // Spawn handle_client on the server side
        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            DaemonCtx::legacy(),
        ));

        // Read response on the client side
        let response = read_response(&mut client_stream).await;
        match response {
            ServerMsg::SessionList(list) => {
                assert!(
                    list.is_empty(),
                    "expected empty session list, got {:?}",
                    list
                );
            }
            other => panic!("expected SessionList, got {:?}", other),
        }

        // handle_client should complete successfully
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn kill_nonexistent_session() {
        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));

        let mut client_stream = client_stream;
        write_preamble_and_hello(&mut client_stream).await;
        let msg = protocol::encode(&ClientMsg::KillSession {
            name: "no-such-session".into(),
        })
        .unwrap();
        client_stream.write_all(&msg).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            DaemonCtx::legacy(),
        ));

        let response = read_response(&mut client_stream).await;
        match response {
            ServerMsg::Error(err_msg) => {
                assert!(
                    err_msg.contains("no-such-session"),
                    "error message should mention session name, got: {}",
                    err_msg
                );
            }
            other => panic!("expected Error, got {:?}", other),
        }

        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unexpected_message_returns_error() {
        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));

        // Send an Input message, which is not a valid initial message
        let mut client_stream = client_stream;
        write_preamble_and_hello(&mut client_stream).await;
        let msg = protocol::encode(&ClientMsg::Input(b"hello".to_vec())).unwrap();
        client_stream.write_all(&msg).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            DaemonCtx::legacy(),
        ));

        let response = read_response(&mut client_stream).await;
        match response {
            ServerMsg::Error(err_msg) => {
                assert!(
                    err_msg.contains("expected Connect, ListSessions, KillSession, SendInput"),
                    "unexpected error message: {}",
                    err_msg
                );
            }
            other => panic!("expected Error, got {:?}", other),
        }

        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn client_disconnect_before_message() {
        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));

        // Drop the client side immediately to simulate a disconnect
        drop(client_stream);

        // handle_client should return Ok(()) when the client disconnects before sending anything
        let result = handle_client(server_stream, manager, test_fault(), DaemonCtx::legacy()).await;
        assert!(result.is_ok(), "expected Ok(()), got {:?}", result);
    }

    /// Gate pass (a) item 3: old-version client → modern daemon = framed error
    /// (clean refusal), not a hang, not a silent drop.
    #[tokio::test]
    async fn version_mismatch_refused_with_framed_error() {
        use crate::protocol::handshake::{PREAMBLE_MAGIC, PROTOCOL_VERSION};

        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));

        // Craft an old-version preamble (version 0 — predates v1).
        let mut preamble = [0u8; 5];
        preamble[..4].copy_from_slice(&PREAMBLE_MAGIC);
        preamble[4] = 0;
        client_stream.write_all(&preamble).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            DaemonCtx::legacy(),
        ));

        // Clean refusal must arrive promptly — bound it so a hang fails the test.
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_response(&mut client_stream),
        )
        .await
        .expect("server hung instead of refusing version-mismatched client");
        match response {
            ServerMsg::Error(e) => {
                assert!(
                    e.contains("version mismatch") && e.contains("client v0"),
                    "refusal should name the mismatch, got: {}",
                    e
                );
                assert!(
                    e.contains(&format!("server v{}", PROTOCOL_VERSION)),
                    "refusal should name the server version, got: {}",
                    e
                );
            }
            other => panic!("expected framed Error, got {:?}", other),
        }
        handle.await.unwrap().unwrap();
    }

    /// v2 GetHistory: one-shot read over the wire. No attach. The History frame
    /// carries the v2 composition — scrollback lines PLUS the current visible
    /// rows — so BOTH a scrolled-out line and a fresh still-visible line appear
    /// (ADD-6: assert on application output).
    #[tokio::test]
    async fn get_history_one_shot_with_backlog() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));

        // Create a session and drive enough output to push lines into
        // scrollback (3-row screen, so output beyond 3 lines scrolls back).
        // BACKLOG_ONE scrolls out; VISIBLE_TAIL stays on the visible screen.
        {
            let mut mgr = manager.lock().await;
            mgr.create("hist-sess".into(), 20, 3, 1000).unwrap();
            let writer = mgr.get("hist-sess").unwrap().pty_writer_arc();
            drop(mgr);
            tokio::task::spawn_blocking(move || {
                let mut w = writer.lock().unwrap();
                use std::io::Write as _;
                w.write_all(b"echo BACKLOG_ONE\r\necho BACKLOG_TWO\r\necho VISIBLE_TAIL\r\n")
                    .unwrap();
                w.flush().unwrap();
            })
            .await
            .unwrap();
        }

        // Give the shell+reader a moment to process and scroll output back.
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        write_preamble_and_hello(&mut client_stream).await;
        let msg = protocol::encode(&ClientMsg::GetHistory {
            name: "hist-sess".into(),
        })
        .unwrap();
        client_stream.write_all(&msg).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager.clone(),
            test_fault(),
            DaemonCtx::legacy(),
        ));

        let response = read_response(&mut client_stream).await;
        match response {
            ServerMsg::History(lines) => {
                let joined = lines
                    .iter()
                    .map(|l| String::from_utf8_lossy(l).to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    joined.contains("BACKLOG_ONE"),
                    "history should contain scrolled-back output, got: {joined:?}"
                );
                assert!(
                    joined.contains("VISIBLE_TAIL"),
                    "history must include the still-visible line (v2 composition), got: {joined:?}"
                );
            }
            other => panic!("expected History, got {:?}", other),
        }
        handle.await.unwrap().unwrap();

        // Teardown: kill the session so no child/reader leaks.
        let session = manager.lock().await.remove("hist-sess");
        if let Some(s) = session {
            crate::server::drop_blocking_with_timeout(s, "test cleanup").await;
        }
    }

    /// Stage 2a capability gate through the real Hello + GetHistory path: the
    /// capable connection receives logical chunks and reconstructs the wrapped
    /// source line, while an unadvertised connection receives legacy History.
    #[tokio::test]
    async fn history_logical_capability_gates_real_history_emission() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        let cwd = std::env::current_dir().unwrap();
        {
            let mut mgr = manager.lock().await;
            mgr.create_detached(
                "logical-gate".into(),
                5,
                2,
                100,
                crate::pty::CommandSpec::login_shell_c("sleep 60"),
                cwd,
            )
            .unwrap();
            let screen = mgr.get("logical-gate").unwrap().screen.clone();
            drop(mgr);
            let mut screen = screen.lock().unwrap();
            screen.process(b"\x1bcABCDEFGHIJ");
        }

        async fn request(
            manager: Arc<Mutex<SessionManager>>,
            caps: Vec<String>,
        ) -> (ServerMsg, ServerMsg) {
            let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
            protocol::write_preamble(&mut client_stream).await.unwrap();
            let hello = protocol::encode(&ClientMsg::Hello { caps }).unwrap();
            let get = protocol::encode(&ClientMsg::GetHistory {
                name: "logical-gate".into(),
            })
            .unwrap();
            client_stream.write_all(&hello).await.unwrap();
            client_stream.write_all(&get).await.unwrap();

            let handle = tokio::spawn(handle_client(
                server_stream,
                manager,
                test_fault(),
                DaemonCtx::legacy(),
            ));
            let mut reader = FrameReader::new();
            let mut replies = Vec::new();
            while replies.len() < 2 {
                assert!(reader.fill_from(&mut client_stream).await.unwrap());
                while let Some(msg) = reader.decode_next::<ServerMsg>().unwrap() {
                    replies.push(msg);
                }
            }
            handle.await.unwrap().unwrap();
            (replies.remove(0), replies.remove(0))
        }

        let (hello, capable) = request(
            manager.clone(),
            vec![protocol::HISTORY_LOGICAL_V1_CAP.to_string()],
        )
        .await;
        match hello {
            ServerMsg::Hello { caps, .. } => {
                assert_eq!(
                    caps,
                    [
                        protocol::HISTORY_LOGICAL_V1_CAP,
                        protocol::HISTORY_LOGICAL_STREAM_V1_CAP,
                        protocol::INITIAL_SIZE_CONFIRM_V1_CAP,
                    ]
                )
            }
            other => panic!("expected server Hello, got {:?}", other),
        }
        match capable {
            ServerMsg::HistoryLogical(chunks) => {
                assert_eq!(chunks.len(), 2);
                assert!(!chunks[0].end_of_line);
                assert!(chunks[1].end_of_line);
                let lines = crate::client::render_logical_history(&chunks);
                assert_eq!(lines, vec![(b"ABCDEFGHIJ".to_vec(), true)]);
            }
            other => panic!("capable peer expected HistoryLogical, got {:?}", other),
        }

        let (_, legacy) = request(manager.clone(), vec![]).await;
        match legacy {
            ServerMsg::History(lines) => {
                assert_eq!(lines, vec![b"ABCDE".to_vec(), b"FGHIJ".to_vec()]);
            }
            other => panic!("legacy peer expected unchanged History, got {:?}", other),
        }

        let session = manager.lock().await.remove("logical-gate");
        if let Some(session) = session {
            crate::server::drop_blocking_with_timeout(session, "test cleanup").await;
        }
    }

    /// v2 GetHistory against a missing session → framed Error naming the session.
    #[tokio::test]
    async fn get_history_missing_session_errors() {
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        write_preamble_and_hello(&mut client_stream).await;
        let msg = protocol::encode(&ClientMsg::GetHistory {
            name: "ghost".into(),
        })
        .unwrap();
        client_stream.write_all(&msg).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            DaemonCtx::legacy(),
        ));
        match read_response(&mut client_stream).await {
            ServerMsg::Error(e) => assert!(e.contains("ghost"), "got: {e}"),
            other => panic!("expected Error, got {:?}", other),
        }
        handle.await.unwrap().unwrap();
    }

    /// v2 CreateDetached: the daemon spawns a detached session that runs the
    /// command in the EXPLICIT cwd and acks Connected — without attaching.
    /// Asserted by the command writing its own resolved cwd to a file.
    #[tokio::test]
    async fn create_detached_runs_command_in_cwd() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let marker = cwd.join("cwd.out");

        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        write_preamble_and_hello(&mut client_stream).await;
        let msg = protocol::encode(&ClientMsg::CreateDetached {
            name: "det-sess".into(),
            shell_cmd: "pwd -P > cwd.out".into(),
            cwd: cwd.clone(),
            history: 1000,
        })
        .unwrap();
        client_stream.write_all(&msg).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager.clone(),
            test_fault(),
            DaemonCtx::legacy(),
        ));
        match read_response(&mut client_stream).await {
            ServerMsg::Connected { name, new_session } => {
                assert_eq!(name, "det-sess");
                assert!(new_session, "CreateDetached should report a new session");
            }
            other => panic!("expected Connected, got {:?}", other),
        }
        handle.await.unwrap().unwrap();

        // The detached command must run in the explicit cwd.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            marker.exists(),
            "detached command did not run / write marker"
        );
        let got = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(got.trim(), cwd.to_string_lossy(), "wrong cwd");

        // Teardown.
        let session = manager.lock().await.remove("det-sess");
        if let Some(s) = session {
            crate::server::drop_blocking_with_timeout(s, "test cleanup").await;
        }
    }

    /// v2 CreateDetached on a duplicate name → framed Error, no second spawn.
    #[tokio::test]
    async fn create_detached_duplicate_name_errors() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        {
            let mut mgr = manager.lock().await;
            mgr.create("dup".into(), 0, 0, 100).unwrap();
        }

        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        write_preamble_and_hello(&mut client_stream).await;
        let msg = protocol::encode(&ClientMsg::CreateDetached {
            name: "dup".into(),
            shell_cmd: "true".into(),
            cwd: std::env::temp_dir(),
            history: 100,
        })
        .unwrap();
        client_stream.write_all(&msg).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager.clone(),
            test_fault(),
            DaemonCtx::legacy(),
        ));
        match read_response(&mut client_stream).await {
            ServerMsg::Error(e) => assert!(e.contains("already exists"), "got: {e}"),
            other => panic!("expected Error, got {:?}", other),
        }
        handle.await.unwrap().unwrap();

        let session = manager.lock().await.remove("dup");
        if let Some(s) = session {
            crate::server::drop_blocking_with_timeout(s, "test cleanup").await;
        }
    }

    /// Non-qrmux bytes on the socket get a framed error, not a misparse.
    #[tokio::test]
    async fn bad_magic_refused_with_framed_error() {
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));

        client_stream.write_all(b"GET /").await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            DaemonCtx::legacy(),
        ));

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_response(&mut client_stream),
        )
        .await
        .expect("server hung instead of refusing bad-magic client");
        match response {
            ServerMsg::Error(e) => {
                assert!(e.contains("bad magic"), "got: {}", e);
            }
            other => panic!("expected framed Error, got {:?}", other),
        }
        handle.await.unwrap().unwrap();
    }

    /// Helper: raw-read the next ServerMsg WITHOUT skipping ServerHello (the
    /// negotiation-order tests assert on the Hello frame itself).
    async fn read_raw(stream: &mut tokio::net::UnixStream) -> ServerMsg {
        let mut reader = FrameReader::new();
        loop {
            assert!(
                reader.fill_from(stream).await.expect("read failed"),
                "connection closed before a full response was received"
            );
            if let Some(msg) = reader.decode_next::<ServerMsg>().expect("decode error") {
                return msg;
            }
        }
    }

    /// §3.2: the FIRST frame after the preamble MUST be Hello. A valid Hello →
    /// the server's FIRST reply frame is ServerHello (with the server's
    /// additive capabilities and transitional empty session in M1).
    #[tokio::test]
    async fn hello_first_roundtrip() {
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        protocol::write_preamble(&mut client_stream).await.unwrap();
        let hello = protocol::encode(&ClientMsg::Hello { caps: vec![] }).unwrap();
        client_stream.write_all(&hello).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            DaemonCtx::legacy(),
        ));
        match read_raw(&mut client_stream).await {
            ServerMsg::Hello { caps, session } => {
                assert_eq!(
                    caps,
                    [
                        protocol::HISTORY_LOGICAL_V1_CAP,
                        protocol::HISTORY_LOGICAL_STREAM_V1_CAP,
                        protocol::INITIAL_SIZE_CONFIRM_V1_CAP,
                    ]
                );
                assert_eq!(session, "", "M1 session identity is transitional/empty");
            }
            other => panic!("expected ServerHello, got {:?}", other),
        }
        // Server then waits for the next request; drop closes the connection.
        drop(client_stream);
        handle.await.unwrap().unwrap();
    }

    /// §3.2: ServerHello, then a pipelined ListSessions reads TWO reply frames
    /// (ServerHello then SessionList) off ONE connection — the FrameReader path
    /// the engine probe relies on (red-team M6).
    #[tokio::test]
    async fn hello_then_pipelined_list_two_reply_frames() {
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        protocol::write_preamble(&mut client_stream).await.unwrap();
        // Pipeline: Hello + ListSessions back-to-back, no wait.
        let hello = protocol::encode(&ClientMsg::Hello { caps: vec![] }).unwrap();
        let list = protocol::encode(&ClientMsg::ListSessions).unwrap();
        client_stream.write_all(&hello).await.unwrap();
        client_stream.write_all(&list).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            DaemonCtx::legacy(),
        ));

        // A SINGLE FrameReader reads BOTH reply frames — this is the red-team-M6
        // contract: pipelined reply bytes (ServerHello + SessionList) may arrive
        // in one read, so a fresh-reader-per-frame (or read_one_message) would
        // discard the second frame's bytes. The multi-frame reader preserves
        // them.
        let mut reader = FrameReader::new();
        let mut replies: Vec<ServerMsg> = Vec::new();
        while replies.len() < 2 {
            assert!(
                reader
                    .fill_from(&mut client_stream)
                    .await
                    .expect("read failed"),
                "connection closed before both reply frames arrived"
            );
            while let Some(msg) = reader.decode_next::<ServerMsg>().expect("decode error") {
                replies.push(msg);
            }
        }
        // First reply frame MUST be ServerHello.
        assert!(
            matches!(replies[0], ServerMsg::Hello { .. }),
            "first reply frame must be ServerHello, got {:?}",
            replies[0]
        );
        // Second reply frame is the SessionList response.
        match &replies[1] {
            ServerMsg::SessionList(list) => assert!(list.is_empty(), "got {:?}", list),
            other => panic!("expected SessionList, got {:?}", other),
        }
        handle.await.unwrap().unwrap();
    }

    /// §3.2: a non-Hello first frame → EXACT framed "expected Hello" error,
    /// close. Exact-equality assert (house lesson).
    #[tokio::test]
    async fn hello_first_violation_exact_error() {
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        protocol::write_preamble(&mut client_stream).await.unwrap();
        // First frame is ListSessions, NOT Hello — a protocol violation.
        let msg = protocol::encode(&ClientMsg::ListSessions).unwrap();
        client_stream.write_all(&msg).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            DaemonCtx::legacy(),
        ));
        match read_raw(&mut client_stream).await {
            ServerMsg::Error(e) => assert_eq!(
                e, "protocol error: expected Hello as first frame",
                "exact refusal string"
            ),
            other => panic!("expected framed Error, got {:?}", other),
        }
        handle.await.unwrap().unwrap();
    }

    /// §3.2: post-preamble connect-and-drop (EOF before any frame) → quiet
    /// close, NO error frame.
    #[tokio::test]
    async fn eof_after_preamble_quiet_close() {
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        protocol::write_preamble(&mut client_stream).await.unwrap();
        // Drop without sending any frame: a post-preamble liveness probe.
        drop(client_stream);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle_client(server_stream, manager, test_fault(), DaemonCtx::legacy()),
        )
        .await
        .expect("server hung instead of closing quietly on post-preamble EOF");
        assert!(result.is_ok(), "expected quiet Ok(()), got {:?}", result);
    }

    /// §3.2 defensive bounds: a Hello whose caps violate count/size/charset →
    /// framed Error, close.
    #[tokio::test]
    async fn hello_bounds_violations_framed_error() {
        // (a) too many caps.
        let too_many: Vec<String> = (0..65).map(|i| format!("c{}", i)).collect();
        // (b) cap too long.
        let too_long = vec!["a".repeat(65)];
        // (c) bad charset (uppercase).
        let bad_charset = vec!["NotKebab".to_string()];

        for caps in [too_many, too_long, bad_charset] {
            let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
            let manager = Arc::new(Mutex::new(SessionManager::new()));
            protocol::write_preamble(&mut client_stream).await.unwrap();
            let hello = protocol::encode(&ClientMsg::Hello { caps }).unwrap();
            client_stream.write_all(&hello).await.unwrap();

            let handle = tokio::spawn(handle_client(
                server_stream,
                manager,
                test_fault(),
                DaemonCtx::legacy(),
            ));
            match read_raw(&mut client_stream).await {
                ServerMsg::Error(e) => {
                    assert!(e.starts_with("protocol error:"), "got: {}", e)
                }
                other => panic!("expected framed Error, got {:?}", other),
            }
            handle.await.unwrap().unwrap();
        }
    }

    // =====================================================================
    // WS-C M2: single-session capacity-1 identity + ServerHello.session.
    // =====================================================================

    /// A single-session DaemonCtx for tests (no claim timer / no end flag —
    /// those are exercised by the wsc_m2 integration tests against run_server).
    fn single_session_ctx(name: &str) -> DaemonCtx {
        DaemonCtx {
            session: Some(Arc::from(name)),
            claim_reset: None,
            ended: None,
            in_flight: None,
        }
    }

    /// §3.2/§4.1: in single-session mode ServerHello.session == the daemon's
    /// --session identity (replaces M1's transitional empty string).
    #[tokio::test]
    async fn server_hello_carries_session_identity() {
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        protocol::write_preamble(&mut client_stream).await.unwrap();
        let hello = protocol::encode(&ClientMsg::Hello { caps: vec![] }).unwrap();
        client_stream.write_all(&hello).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            single_session_ctx("alpha"),
        ));
        match read_raw(&mut client_stream).await {
            ServerMsg::Hello { caps, session } => {
                assert_eq!(
                    caps,
                    [
                        protocol::HISTORY_LOGICAL_V1_CAP,
                        protocol::HISTORY_LOGICAL_STREAM_V1_CAP,
                        protocol::INITIAL_SIZE_CONFIRM_V1_CAP,
                    ]
                );
                assert_eq!(
                    session, "alpha",
                    "ServerHello must carry the --session identity"
                );
            }
            other => panic!("expected ServerHello, got {:?}", other),
        }
        drop(client_stream);
        handle.await.unwrap().unwrap();
    }

    /// §4.1 capacity-1: a session-addressed verb naming a DIFFERENT session →
    /// EXACT "not found (this daemon serves ...)" error. Exact-equality (house
    /// lesson). Covers GetHistory; the gate is verb-agnostic (same code path).
    #[tokio::test]
    async fn wrong_name_verb_exact_not_found_error() {
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        write_preamble_and_hello(&mut client_stream).await;
        let msg = protocol::encode(&ClientMsg::GetHistory {
            name: "intruder".into(),
        })
        .unwrap();
        client_stream.write_all(&msg).await.unwrap();

        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            single_session_ctx("alpha"),
        ));
        match read_response(&mut client_stream).await {
            ServerMsg::Error(e) => assert_eq!(
                e, "session 'intruder' not found (this daemon serves 'alpha')",
                "exact capacity-1 identity error"
            ),
            other => panic!("expected Error, got {:?}", other),
        }
        handle.await.unwrap().unwrap();
    }

    /// §4.1: the capacity-1 gate also fires for SendInput and KillSession (every
    /// session-addressed verb checks identity FIRST), each with the exact error.
    #[tokio::test]
    async fn wrong_name_send_and_kill_exact_errors() {
        // SendInput.
        {
            let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
            let manager = Arc::new(Mutex::new(SessionManager::new()));
            write_preamble_and_hello(&mut client_stream).await;
            let msg = protocol::encode(&ClientMsg::SendInput {
                name: "other".into(),
                data: b"x".to_vec(),
            })
            .unwrap();
            client_stream.write_all(&msg).await.unwrap();
            let handle = tokio::spawn(handle_client(
                server_stream,
                manager,
                test_fault(),
                single_session_ctx("alpha"),
            ));
            match read_response(&mut client_stream).await {
                ServerMsg::Error(e) => {
                    assert_eq!(e, "session 'other' not found (this daemon serves 'alpha')")
                }
                other => panic!("expected Error, got {:?}", other),
            }
            handle.await.unwrap().unwrap();
        }
        // KillSession.
        {
            let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
            let manager = Arc::new(Mutex::new(SessionManager::new()));
            write_preamble_and_hello(&mut client_stream).await;
            let msg = protocol::encode(&ClientMsg::KillSession {
                name: "other".into(),
            })
            .unwrap();
            client_stream.write_all(&msg).await.unwrap();
            let handle = tokio::spawn(handle_client(
                server_stream,
                manager,
                test_fault(),
                single_session_ctx("alpha"),
            ));
            match read_response(&mut client_stream).await {
                ServerMsg::Error(e) => {
                    assert_eq!(e, "session 'other' not found (this daemon serves 'alpha')")
                }
                other => panic!("expected Error, got {:?}", other),
            }
            handle.await.unwrap().unwrap();
        }
    }

    /// §4.1: the OWN-name create→send→history chain works under capacity-1 (the
    /// identity gate passes for the daemon's own session; the wire flow is
    /// unchanged). Proves the gate is not over-broad.
    #[tokio::test]
    async fn own_name_create_send_history_chain() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();

        // CreateDetached for the daemon's OWN name.
        {
            let (mut cs, ss) = tokio::net::UnixStream::pair().unwrap();
            write_preamble_and_hello(&mut cs).await;
            let msg = protocol::encode(&ClientMsg::CreateDetached {
                name: "alpha".into(),
                shell_cmd: "echo OWN_NAME_OK".into(),
                cwd: cwd.clone(),
                history: 1000,
            })
            .unwrap();
            cs.write_all(&msg).await.unwrap();
            let h = tokio::spawn(handle_client(
                ss,
                manager.clone(),
                test_fault(),
                single_session_ctx("alpha"),
            ));
            match read_response(&mut cs).await {
                ServerMsg::Connected { name, new_session } => {
                    assert_eq!(name, "alpha");
                    assert!(new_session);
                }
                other => panic!("expected Connected, got {:?}", other),
            }
            h.await.unwrap().unwrap();
        }

        // GetHistory for the OWN name eventually shows the output.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (mut cs, ss) = tokio::net::UnixStream::pair().unwrap();
            write_preamble_and_hello(&mut cs).await;
            let msg = protocol::encode(&ClientMsg::GetHistory {
                name: "alpha".into(),
            })
            .unwrap();
            cs.write_all(&msg).await.unwrap();
            let h = tokio::spawn(handle_client(
                ss,
                manager.clone(),
                test_fault(),
                single_session_ctx("alpha"),
            ));
            let resp = read_response(&mut cs).await;
            h.await.unwrap().unwrap();
            if let ServerMsg::History(lines) = resp {
                let joined = lines
                    .iter()
                    .map(|l| String::from_utf8_lossy(l).to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                if joined.contains("OWN_NAME_OK") {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "own-name history never showed the output"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Teardown.
        let session = manager.lock().await.remove("alpha");
        if let Some(s) = session {
            super::super::drop_blocking_with_timeout(s, "test cleanup").await;
        }
    }

    /// §4.1: ListSessions is NOT session-addressed — in single-session mode it
    /// returns this daemon's 0-or-1 rows (no identity gate). Empty here.
    #[tokio::test]
    async fn list_sessions_single_session_returns_own_rows() {
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        write_preamble_and_hello(&mut client_stream).await;
        let msg = protocol::encode(&ClientMsg::ListSessions).unwrap();
        client_stream.write_all(&msg).await.unwrap();
        let handle = tokio::spawn(handle_client(
            server_stream,
            manager,
            test_fault(),
            single_session_ctx("alpha"),
        ));
        match read_response(&mut client_stream).await {
            ServerMsg::SessionList(list) => assert!(list.is_empty(), "got {:?}", list),
            other => panic!("expected SessionList, got {:?}", other),
        }
        handle.await.unwrap().unwrap();
    }

    /// §4.1 step 4: once `ended` is set, a session-addressed verb gets the EXACT
    /// named session-ended refusal (deterministic, never a hang). ListSessions is
    /// not session-addressed so it is unaffected (the launcher's retiring probe
    /// uses a session-addressed verb).
    #[tokio::test]
    async fn ended_flag_refuses_with_named_error() {
        let ended = Arc::new(AtomicBool::new(true));
        let ctx = DaemonCtx {
            session: Some(Arc::from("alpha")),
            claim_reset: None,
            ended: Some(ended),
            in_flight: None,
        };
        let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        write_preamble_and_hello(&mut client_stream).await;
        // A session-addressed verb for the OWN name still gets the ended refusal.
        let msg = protocol::encode(&ClientMsg::GetHistory {
            name: "alpha".into(),
        })
        .unwrap();
        client_stream.write_all(&msg).await.unwrap();
        let handle = tokio::spawn(handle_client(server_stream, manager, test_fault(), ctx));
        match read_response(&mut client_stream).await {
            ServerMsg::Error(e) => {
                assert_eq!(e, ERR_SESSION_ENDED, "exact session-ended refusal")
            }
            other => panic!("expected Error, got {:?}", other),
        }
        handle.await.unwrap().unwrap();
    }

    /// §4.1 / red-team B1: a session-addressed verb pulses the claim-reset
    /// notify; a Hello-only connection does NOT. Asserted by counting pulses on
    /// a shared Notify via a watcher.
    #[tokio::test]
    async fn session_verb_pulses_claim_reset_hello_does_not() {
        let claim_reset = Arc::new(tokio::sync::Notify::new());
        let ctx = DaemonCtx {
            session: Some(Arc::from("alpha")),
            claim_reset: Some(claim_reset.clone()),
            ended: None,
            in_flight: None,
        };

        // Watcher: record whether a pulse arrives within a short window.
        async fn pulse_arrived(n: &tokio::sync::Notify) -> bool {
            tokio::time::timeout(std::time::Duration::from_millis(500), n.notified())
                .await
                .is_ok()
        }

        // (a) Hello-only connection: arm the waiter BEFORE the connection so a
        // pulse can't be missed, then confirm NO pulse.
        {
            let waiter = {
                let cr = claim_reset.clone();
                tokio::spawn(async move { pulse_arrived(&cr).await })
            };
            tokio::task::yield_now().await;
            let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
            let manager = Arc::new(Mutex::new(SessionManager::new()));
            write_preamble_and_hello(&mut client_stream).await;
            let h = tokio::spawn(handle_client(
                server_stream,
                manager,
                test_fault(),
                ctx.clone(),
            ));
            // Read the ServerHello reply (so the server's write succeeds), then
            // close WITHOUT sending any session-addressed verb — a pure probe.
            let _ = read_raw(&mut client_stream).await;
            drop(client_stream);
            h.await.unwrap().unwrap();
            assert!(
                !waiter.await.unwrap(),
                "Hello-only must NOT pulse claim-reset"
            );
        }

        // (b) A session-addressed verb that PASSES the identity gate DOES pulse.
        // Order per §4.1 (handle_client): the capacity-1 identity gate runs FIRST
        // and returns early on a wrong name, so a wrong-name verb would NOT pulse;
        // only a verb that clears the gate reaches pulse_claim_reset. This probe
        // uses the daemon's OWN name ("alpha") so it passes the gate and pulses.
        {
            let waiter = {
                let cr = claim_reset.clone();
                tokio::spawn(async move { pulse_arrived(&cr).await })
            };
            tokio::task::yield_now().await;
            let (mut client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
            let manager = Arc::new(Mutex::new(SessionManager::new()));
            write_preamble_and_hello(&mut client_stream).await;
            let msg = protocol::encode(&ClientMsg::GetHistory {
                name: "alpha".into(),
            })
            .unwrap();
            client_stream.write_all(&msg).await.unwrap();
            let h = tokio::spawn(handle_client(
                server_stream,
                manager,
                test_fault(),
                ctx.clone(),
            ));
            let _ = read_response(&mut client_stream).await;
            h.await.unwrap().unwrap();
            assert!(
                waiter.await.unwrap(),
                "session-addressed verb MUST pulse claim-reset"
            );
        }
    }
}
