//! WS-C M3a: per-session client surface (spec §3.2 client identity belt, §4.2,
//! §4.4). NEW surface ALONGSIDE the intact legacy `client/mod.rs` verbs — the
//! engine (crates/qd) does not call these until M3b flips it.
//!
//! Every entry point derives the socket from the session NAME via
//! [`session_socket_path_for`] (`<dir>/<name>.sock`), connects, runs the v3
//! Hello handshake, and enforces the §3.2 client-side identity belt:
//! `ServerHello.session == name` or the named mismatch error. The per-verb
//! protocol logic (encode the verb, read the single reply) is shared with the
//! legacy path via the protocol codec — no protocol code is duplicated.
//!
//! **No `canonicalize()` anywhere (§4.4 keystone invariant):** the socket path
//! is resolved by [`session_socket_path_for`] (the same resolve-fn the daemon
//! binds with under `--socket-dir`/`--session`), and compared/used verbatim.

use std::path::Path;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::protocol::{self, read_one_message, ClientMsg, FrameReader, ServerMsg};
use crate::server::socket::session_socket_path_for;

use super::server_launcher::{ensure_session_server_running, ServerLaunchSpec};

#[derive(Clone, Copy, Debug, Default)]
struct NegotiatedServerCaps {
    logical_history_stream: bool,
    initial_size_confirm: bool,
}

/// Build the client-side identity-mismatch error (§3.2 step 3). Exact string —
/// tests assert on it. `path` is the socket the client connected to, `actual`
/// is the `ServerHello.session` the daemon reported, `expected` is the name the
/// client intended to reach.
fn identity_mismatch_error(path: &Path, actual: &str, expected: &str) -> String {
    format!(
        "qrmux daemon at {} identifies as session '{}', expected '{}'",
        path.display(),
        actual,
        expected
    )
}

/// v3 client Hello handshake WITH the §3.2 identity belt (the per-session
/// handshake — the legacy shared-daemon `client_handshake` was retired in M3b).
///
/// Advertises `history-logical-v1`, reads the `ServerMsg::Hello` as the
/// server's FIRST reply frame, and verifies `ServerHello.session == name`.
/// Mismatch → the named [`identity_mismatch_error`]; a framed `Error` (e.g. the
/// "retiring" session-ended refusal) is surfaced verbatim so the launcher can
/// classify it.
///
/// Uses [`FrameReader`] (NOT `read_one_message`) so any bytes a pipelining peer
/// packed behind the ServerHello are preserved as leftover (codec.rs note,
/// red-team M6). v3 servers send nothing until their next reply, so in practice
/// there is no leftover here; the caller discards it (its subsequent
/// `read_one_message` re-reads the verb reply cleanly).
async fn session_handshake(
    stream: &mut UnixStream,
    path: &Path,
    name: &str,
) -> anyhow::Result<NegotiatedServerCaps> {
    let hello = protocol::encode(&ClientMsg::Hello {
        caps: vec![
            protocol::HISTORY_LOGICAL_V1_CAP.to_string(),
            protocol::HISTORY_LOGICAL_STREAM_V1_CAP.to_string(),
            protocol::INITIAL_SIZE_CONFIRM_V1_CAP.to_string(),
        ],
    })?;
    stream.write_all(&hello).await?;
    let mut frames = FrameReader::new();
    loop {
        if !frames.fill_from(stream).await? {
            anyhow::bail!("server closed connection before sending Hello");
        }
        if let Some(msg) = frames.decode_next::<ServerMsg>()? {
            match msg {
                ServerMsg::Hello { caps, session } => {
                    // G-ISOL negative-control seam (spec §7, red-team M4): under
                    // `QRMUX_TEST_SHARED=1` ALL names collapse onto one `shared.sock`
                    // daemon whose ServerHello.session is whatever it was launched as,
                    // so the per-session identity belt is relaxed (it would otherwise
                    // reject every connect for a different name than the daemon's
                    // launch identity). Inert in production (env unset).
                    if session != name && !crate::server::socket::shared_fate_test_mode() {
                        anyhow::bail!(identity_mismatch_error(path, &session, name));
                    }
                    let logical_history = caps
                        .iter()
                        .any(|cap| cap == protocol::HISTORY_LOGICAL_V1_CAP);
                    return Ok(NegotiatedServerCaps {
                        logical_history_stream: logical_history
                            && caps
                                .iter()
                                .any(|cap| cap == protocol::HISTORY_LOGICAL_STREAM_V1_CAP),
                        initial_size_confirm: caps
                            .iter()
                            .any(|cap| cap == protocol::INITIAL_SIZE_CONFIRM_V1_CAP),
                    });
                }
                ServerMsg::Error(e) => anyhow::bail!("{}", e),
                other => anyhow::bail!(
                    "expected server Hello, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
        }
    }
}

/// Connect to a per-session daemon's socket (`<dir>/<name>.sock`), run the v3
/// Hello handshake, and enforce the §3.2 client-side identity belt. Returns the
/// handshaken stream ready for a verb frame.
///
/// Validates the session identity (charset + reserved-name tightening, §2)
/// before deriving the socket, so a bogus name fails loudly client-side rather
/// than connecting to a path that could never be a valid leaf.
///
/// NOTE: this does NOT launch a daemon — it is the pure connect+belt. Callers
/// that must guarantee a live daemon call [`ensure_session_server_running`]
/// first (see the verb entry points below).
pub async fn connect_session_stream(
    socket_dir: Option<&Path>,
    name: &str,
) -> anyhow::Result<UnixStream> {
    Ok(connect_session_stream_with_caps(socket_dir, name).await?.0)
}

async fn connect_session_stream_with_caps(
    socket_dir: Option<&Path>,
    name: &str,
) -> anyhow::Result<(UnixStream, NegotiatedServerCaps)> {
    crate::server::socket::validate_session_identity(name)?;
    let path = session_socket_path_for(socket_dir, name)?;
    let mut stream = UnixStream::connect(&path).await?;
    protocol::write_preamble(&mut stream).await?;
    let caps = session_handshake(&mut stream, &path, name).await?;
    Ok((stream, caps))
}

/// Per-session `send_input` (§4.1 SendInput verb). Ensures the session's daemon
/// is running, then writes `data` to the session's PTY out-of-band (no attach).
/// Returns the acked byte count.
pub async fn send_input_session(
    socket_dir: Option<&Path>,
    launch: Option<&ServerLaunchSpec>,
    name: &str,
    data: Vec<u8>,
) -> anyhow::Result<usize> {
    ensure_session_server_running(socket_dir, name, launch).await?;
    let mut stream = connect_session_stream(socket_dir, name).await?;
    let msg = protocol::encode(&ClientMsg::SendInput {
        name: name.to_string(),
        data,
    })?;
    stream.write_all(&msg).await?;
    match read_one_message(&mut stream).await? {
        ServerMsg::InputSent { bytes, .. } => Ok(bytes),
        ServerMsg::Error(e) => anyhow::bail!("{}", e),
        other => anyhow::bail!(
            "unexpected server response: {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// Per-session `pending_delivery` (v5 attended-UX delivery surface). Hands ONE
/// send to the mux delivery surface and reads the single NON-terminal
/// `DeliveryQueued` receipt, then closes. From this receipt onward the MUX owns
/// the send's lifecycle and emits EXACTLY ONE terminal per send_id to the
/// authoritative ledger (the single-writer split, M3) — the caller NEVER writes a
/// terminal for a mux-held send. Mirrors [`send_input_session`]'s one-shot
/// request/reply shape; ensures the session daemon is running first. Returns the
/// server-acked send_id (the same id the caller minted + carried on `send-initiated`).
pub async fn pending_delivery_session(
    socket_dir: Option<&Path>,
    launch: Option<&ServerLaunchSpec>,
    msg: ClientMsg,
) -> anyhow::Result<String> {
    // `msg` is a fully-built ClientMsg::PendingDelivery; recover the addressed
    // name for the daemon-ensure + connect belt without re-taking it as a param.
    let name = match &msg {
        ClientMsg::PendingDelivery { name, .. } => name.clone(),
        other => anyhow::bail!(
            "pending_delivery_session: expected PendingDelivery, got {:?}",
            std::mem::discriminant(other)
        ),
    };
    ensure_session_server_running(socket_dir, &name, launch).await?;
    let mut stream = connect_session_stream(socket_dir, &name).await?;
    stream.write_all(&protocol::encode(&msg)?).await?;
    match read_one_message(&mut stream).await? {
        // NON-terminal handoff-ack: a no-`--wait` sender leaves here; the mux still
        // resolves the send to exactly one terminal after the sender is gone.
        ServerMsg::DeliveryQueued { send_id } => Ok(send_id),
        ServerMsg::Error(e) => anyhow::bail!("{}", e),
        other => anyhow::bail!(
            "unexpected server response: {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// Per-session `get_history` (§4.1 GetHistory verb). Reads the session's
/// scrollback (no attach, no eviction) as rendered ANSI lines.
///
/// Read-only: does NOT launch a daemon — a history read of a session that does
/// not exist should surface the daemon's "not found" / a connect error, not
/// silently spawn an empty daemon.
pub async fn get_history_session(
    socket_dir: Option<&Path>,
    name: &str,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let (mut stream, caps) = connect_session_stream_with_caps(socket_dir, name).await?;
    let msg = protocol::encode(&ClientMsg::GetHistory {
        name: name.to_string(),
    })?;
    stream.write_all(&msg).await?;
    if caps.logical_history_stream {
        let mut frames = FrameReader::new();
        let mut chunks = Vec::new();
        loop {
            if let Some(msg) = frames.decode_next::<ServerMsg>()? {
                match msg {
                    ServerMsg::HistoryLogical(mut frame_chunks) => {
                        if frame_chunks.is_empty() {
                            return Ok(super::render_logical_history(&chunks)
                                .into_iter()
                                .map(|(line, _)| line)
                                .collect());
                        }
                        chunks.append(&mut frame_chunks);
                    }
                    ServerMsg::Error(e) => anyhow::bail!("{}", e),
                    other => anyhow::bail!(
                        "unexpected streamed history response: {:?}",
                        std::mem::discriminant(&other)
                    ),
                }
                continue;
            }
            if !frames.fill_from(&mut stream).await? {
                anyhow::bail!("server closed before completing logical history");
            }
        }
    } else {
        match read_one_message(&mut stream).await? {
            ServerMsg::History(lines) => Ok(lines),
            ServerMsg::HistoryLogical(chunks) => Ok(super::render_logical_history(&chunks)
                .into_iter()
                .map(|(line, _)| line)
                .collect()),
            ServerMsg::Error(e) => anyhow::bail!("{}", e),
            other => anyhow::bail!(
                "unexpected server response: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }
}

/// Per-session `kill_session` (§4.1 KillSession verb). Asks the session's daemon
/// to terminate the session, which drives its exit-on-end path.
///
/// Read-side: does NOT launch a daemon. A kill of a non-running session is a
/// connect error (ENOENT/ECONNREFUSED), surfaced to the caller.
pub async fn kill_session_session(socket_dir: Option<&Path>, name: &str) -> anyhow::Result<()> {
    let mut stream = connect_session_stream(socket_dir, name).await?;
    let msg = protocol::encode(&ClientMsg::KillSession {
        name: name.to_string(),
    })?;
    stream.write_all(&msg).await?;
    match read_one_message(&mut stream).await? {
        ServerMsg::SessionKilled { .. } => Ok(()),
        ServerMsg::Error(e) => anyhow::bail!("{}", e),
        other => anyhow::bail!(
            "unexpected server response: {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// Per-session `create_detached` (§4.1 CreateDetached verb). Ensures the
/// session's daemon is running (cold-start spawns one), then creates a detached
/// session running `shell_cmd` in `cwd`. Returns the session name the daemon
/// acked.
#[allow(clippy::too_many_arguments)]
pub async fn create_detached_session(
    socket_dir: Option<&Path>,
    launch: Option<&ServerLaunchSpec>,
    name: &str,
    shell_cmd: &str,
    cwd: std::path::PathBuf,
    history: usize,
) -> anyhow::Result<String> {
    ensure_session_server_running(socket_dir, name, launch).await?;
    let mut stream = connect_session_stream(socket_dir, name).await?;
    let msg = protocol::encode(&ClientMsg::CreateDetached {
        name: name.to_string(),
        shell_cmd: shell_cmd.to_string(),
        cwd,
        history,
    })?;
    stream.write_all(&msg).await?;
    match read_one_message(&mut stream).await? {
        ServerMsg::Connected { name, .. } => Ok(name),
        ServerMsg::Error(e) => anyhow::bail!("{}", e),
        other => anyhow::bail!(
            "unexpected server response: {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

struct AttachHandshake {
    leftover: Vec<u8>,
    size_disagreed: bool,
}

async fn perform_attach_handshake(
    stream: &mut UnixStream,
    name: &str,
    history: usize,
    mode: crate::protocol::ConnectMode,
    caps: NegotiatedServerCaps,
    connect_size: (u16, u16),
    current_size: impl FnOnce() -> (u16, u16),
) -> anyhow::Result<AttachHandshake> {
    let (cols, rows) = connect_size;
    let msg = protocol::encode(&ClientMsg::Connect {
        name: name.to_string(),
        history,
        cols,
        rows,
        mode,
    })?;
    stream.write_all(&msg).await?;

    let mut current_size = Some(current_size);
    let mut frames = FrameReader::new();
    loop {
        if !frames.fill_from(stream).await? {
            anyhow::bail!("server closed connection before handshake completed");
        }
        if let Some(msg) = frames.decode_next::<ServerMsg>()? {
            match msg {
                ServerMsg::Connected {
                    name: ref session_name,
                    new_session,
                } => {
                    if new_session {
                        eprintln!("[retach: new session '{}' (detach: Ctrl+\\)]", session_name);
                    } else {
                        eprintln!(
                            "[retach: reattached to '{}' (detach: Ctrl+\\)]",
                            session_name
                        );
                    }
                    let mut size_disagreed = false;
                    if caps.initial_size_confirm {
                        let confirmed = current_size.take().expect("size sampled once")();
                        size_disagreed = confirmed != connect_size;
                        let confirm = protocol::encode(&ClientMsg::ConfirmSize {
                            cols: confirmed.0,
                            rows: confirmed.1,
                        })?;
                        stream.write_all(&confirm).await?;
                    }
                    return Ok(AttachHandshake {
                        leftover: frames.into_leftover(),
                        size_disagreed,
                    });
                }
                ServerMsg::Error(e) => anyhow::bail!("{}", e),
                _ => anyhow::bail!("unexpected response from server"),
            }
        }
    }
}

/// Per-session interactive attach (`connect_for`'s per-session twin, §4.1
/// Connect verb). Ensures the session's daemon is running, connects with the
/// identity belt, sends `Connect`, and runs the SAME interactive raw-mode I/O
/// the legacy [`super::connect_for_with`] uses (shared building blocks — no
/// duplication of the relay/sigwinch/teardown logic).
pub async fn attach_session(
    socket_dir: Option<&Path>,
    launch: Option<&ServerLaunchSpec>,
    name: &str,
    history: usize,
    mode: crate::protocol::ConnectMode,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    ensure_session_server_running(socket_dir, name, launch).await?;
    let (mut stream, caps) = connect_session_stream_with_caps(socket_dir, name).await?;

    let handshake = perform_attach_handshake(
        &mut stream,
        name,
        history,
        mode,
        caps,
        super::get_terminal_size(),
        super::get_terminal_size,
    )
    .await?;
    if handshake.size_disagreed {
        tracing::debug!(
            "terminal size changed during attach handshake; confirmed repaint requested"
        );
    }
    let leftover = handshake.leftover;

    // Install panic hook to restore terminal even if we panic while in raw mode.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        super::raw_mode::emergency_restore();
        super::cleanup_terminal();
        prev_hook(info);
    }));
    let _hook_guard = super::PanicHookGuard;

    let _raw = super::raw_mode::RawMode::enter()?;

    // Enable focus reporting so the focus-event filter is live.
    if let Err(e) = std::io::stdout().write_all(b"\x1b[?1004h") {
        tracing::debug!(error = %e, "failed to enable focus reporting");
    }
    if let Err(e) = std::io::stdout().flush() {
        tracing::debug!(error = %e, "failed to flush stdout after enabling focus reporting");
    }

    let (sock_reader, sock_writer) = stream.into_split();
    let sock_writer = std::sync::Arc::new(tokio::sync::Mutex::new(sock_writer));

    let sigwinch_handle = super::spawn_sigwinch_handler(sock_writer.clone())?;

    // Rebase union: M2 (banner) added `name` to run_stdin_to_socket; main
    // (logical-history stream) added `caps.logical_history_stream` to
    // run_socket_to_stdout. Both merged signatures are honored here.
    let mut stdin_task =
        tokio::spawn(super::run_stdin_to_socket(sock_writer.clone(), name.to_string()));
    let mut socket_task = tokio::spawn(super::run_socket_to_stdout(
        sock_reader,
        leftover,
        caps.logical_history_stream,
    ));

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    enum Completed {
        Stdin,
        Socket,
        Neither,
    }
    let completed = tokio::select! {
        r = &mut stdin_task => {
            if let Ok(Err(e)) = r {
                tracing::debug!(error = %e, "stdin task error");
            }
            Completed::Stdin
        }
        r = &mut socket_task => {
            if let Ok(Err(e)) = r {
                tracing::warn!(error = %e, "socket task error");
                eprintln!("[retach error: {}]", e);
            }
            Completed::Socket
        }
        _ = sigint.recv() => {
            tracing::debug!("received SIGINT, detaching");
            if let Ok(msg) = protocol::encode(&ClientMsg::Detach) {
                let mut w = sock_writer.lock().await;
                if let Err(e) = w.write_all(&msg).await {
                    tracing::debug!(error = %e, "failed to send detach on SIGINT");
                }
            }
            Completed::Neither
        }
        _ = sigterm.recv() => {
            tracing::debug!("received SIGTERM, detaching");
            if let Ok(msg) = protocol::encode(&ClientMsg::Detach) {
                let mut w = sock_writer.lock().await;
                if let Err(e) = w.write_all(&msg).await {
                    tracing::debug!(error = %e, "failed to send detach on SIGTERM");
                }
            }
            Completed::Neither
        }
    };

    match completed {
        Completed::Stdin => {
            socket_task.abort();
            let _ = socket_task.await;
        }
        Completed::Socket => {
            stdin_task.abort();
            let _ = stdin_task.await;
        }
        Completed::Neither => {
            stdin_task.abort();
            socket_task.abort();
            let _ = tokio::join!(stdin_task, socket_task);
        }
    }

    sigwinch_handle.abort();

    drop(_raw);
    drop(_hook_guard);

    super::cleanup_terminal();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    static TEST_SOCKET_ID: AtomicUsize = AtomicUsize::new(0);

    async fn serve_one_client(
        tag: &str,
        name: &str,
        manager: Arc<Mutex<crate::session::SessionManager>>,
    ) -> (
        std::path::PathBuf,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let id = TEST_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::path::PathBuf::from(format!("/tmp/w2s2b-{}-{}-{}", std::process::id(), tag, id));
        std::fs::create_dir_all(&dir).unwrap();
        let path = crate::server::socket::session_socket_path_for(Some(&dir), name).unwrap();
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let session_name: Arc<str> = Arc::from(name);
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            crate::server::client_handler::handle_client(
                stream,
                manager,
                Arc::new(crate::server::fault::FaultLayer::default()),
                crate::server::client_handler::DaemonCtx {
                    session: Some(session_name),
                    claim_reset: None,
                    ended: None,
                    in_flight: None,
                },
            )
            .await
        });
        (dir, handle)
    }

    async fn connect_with_advertised_caps(dir: &Path, name: &str, caps: &[&str]) -> UnixStream {
        let path = crate::server::socket::session_socket_path_for(Some(dir), name).unwrap();
        let mut stream = UnixStream::connect(path).await.unwrap();
        protocol::write_preamble(&mut stream).await.unwrap();
        stream
            .write_all(
                &protocol::encode(&ClientMsg::Hello {
                    caps: caps.iter().map(|cap| (*cap).to_string()).collect(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        match read_one_message(&mut stream).await.unwrap() {
            ServerMsg::Hello { session, .. } => assert_eq!(session, name),
            other => panic!("expected server Hello, got {other:?}"),
        }
        stream
    }

    async fn send_attach_and_read_connected(
        stream: &mut UnixStream,
        name: &str,
        cols: u16,
        rows: u16,
    ) {
        stream
            .write_all(
                &protocol::encode(&ClientMsg::Connect {
                    name: name.to_string(),
                    history: 16,
                    cols,
                    rows,
                    mode: crate::protocol::ConnectMode::AttachOnly,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            read_one_message::<ServerMsg>(stream).await.unwrap(),
            ServerMsg::Connected { .. }
        ));
    }

    async fn wait_for_screen_bytes(
        manager: &Arc<Mutex<crate::session::SessionManager>>,
        name: &str,
        needle: &[u8],
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let screen = manager.lock().await.get(name).unwrap().screen.clone();
                let found = screen
                    .lock()
                    .unwrap()
                    .get_content_history()
                    .iter()
                    .any(|line| line.windows(needle.len()).any(|window| window == needle));
                if found {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("PTY did not receive coalesced input");
    }

    async fn drop_test_session(
        manager: Arc<Mutex<crate::session::SessionManager>>,
        name: &str,
        dir: &Path,
    ) {
        let session = manager.lock().await.remove(name);
        if let Some(session) = session {
            tokio::task::spawn_blocking(move || drop(session))
                .await
                .unwrap();
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn identity_mismatch_error_exact_shape() {
        let err = identity_mismatch_error(Path::new("/run/qrmux/beta.sock"), "alpha", "beta");
        assert_eq!(
            err,
            "qrmux daemon at /run/qrmux/beta.sock identifies as session 'alpha', expected 'beta'"
        );
    }

    #[tokio::test]
    async fn initial_size_confirm_server_cap_is_independent_of_logical_history() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let server_task = tokio::spawn(async move {
            let hello = read_one_message::<ClientMsg>(&mut server).await.unwrap();
            assert!(matches!(hello, ClientMsg::Hello { .. }));
            server
                .write_all(
                    &protocol::encode(&ServerMsg::Hello {
                        caps: vec![protocol::INITIAL_SIZE_CONFIRM_V1_CAP.to_string()],
                        session: "cap-only".to_string(),
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        });
        let caps = session_handshake(&mut client, Path::new("cap-only.sock"), "cap-only")
            .await
            .unwrap();
        assert!(caps.initial_size_confirm);
        assert!(!caps.logical_history_stream);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn b2_coalesced_confirm_and_input_reaches_pty_without_later_frame() {
        let name = "b2-coalesced-input";
        let manager = Arc::new(Mutex::new(crate::session::SessionManager::new()));
        manager
            .lock()
            .await
            .create_detached(
                name.into(),
                20,
                2,
                16,
                crate::pty::CommandSpec::login_shell_c(
                    "stty -echo; IFS= read -r -n 1 c; printf 'PTY:%s' \"$c\"; sleep 60",
                ),
                std::env::current_dir().unwrap(),
            )
            .unwrap();

        let (dir, server) = serve_one_client("coalesce-input", name, manager.clone()).await;
        let mut stream =
            connect_with_advertised_caps(&dir, name, &[protocol::INITIAL_SIZE_CONFIRM_V1_CAP])
                .await;
        send_attach_and_read_connected(&mut stream, name, 20, 2).await;

        let mut coalesced =
            protocol::encode(&ClientMsg::ConfirmSize { cols: 20, rows: 2 }).unwrap();
        coalesced.extend(protocol::encode(&ClientMsg::Input(b"x".to_vec())).unwrap());
        stream.write_all(&coalesced).await.unwrap();

        // No later client frame is sent until the PTY proves the buffered Input
        // was drained from the confirmation reader and dispatched.
        wait_for_screen_bytes(&manager, name, b"PTY:x").await;
        stream
            .write_all(&protocol::encode(&ClientMsg::Detach).unwrap())
            .await
            .unwrap();
        server.await.unwrap().unwrap();
        drop_test_session(manager, name, &dir).await;
    }

    #[tokio::test]
    async fn b2_coalesced_confirm_and_detach_finishes_without_later_frame() {
        let name = "b2-coalesced-detach";
        let manager = Arc::new(Mutex::new(crate::session::SessionManager::new()));
        manager
            .lock()
            .await
            .create_detached(
                name.into(),
                20,
                2,
                16,
                crate::pty::CommandSpec::login_shell_c("sleep 60"),
                std::env::current_dir().unwrap(),
            )
            .unwrap();

        let (dir, server) = serve_one_client("coalesce-detach", name, manager.clone()).await;
        let mut stream =
            connect_with_advertised_caps(&dir, name, &[protocol::INITIAL_SIZE_CONFIRM_V1_CAP])
                .await;
        send_attach_and_read_connected(&mut stream, name, 20, 2).await;

        let mut coalesced =
            protocol::encode(&ClientMsg::ConfirmSize { cols: 20, rows: 2 }).unwrap();
        coalesced.extend(protocol::encode(&ClientMsg::Detach).unwrap());
        stream.write_all(&coalesced).await.unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("coalesced Detach remained stranded without a later frame")
            .unwrap()
            .unwrap();
        drop_test_session(manager, name, &dir).await;
    }

    #[tokio::test]
    async fn b2_missing_confirm_falls_back_to_connect_geometry_and_attach_lives() {
        let name = "b2-confirm-timeout";
        let manager = Arc::new(Mutex::new(crate::session::SessionManager::new()));
        manager
            .lock()
            .await
            .create_detached(
                name.into(),
                7,
                3,
                16,
                crate::pty::CommandSpec::login_shell_c("sleep 60"),
                std::env::current_dir().unwrap(),
            )
            .unwrap();

        let (dir, server) = serve_one_client("confirm-timeout", name, manager.clone()).await;
        let mut stream =
            connect_with_advertised_caps(&dir, name, &[protocol::INITIAL_SIZE_CONFIRM_V1_CAP])
                .await;
        send_attach_and_read_connected(&mut stream, name, 7, 3).await;

        let started = tokio::time::Instant::now();
        let initial = tokio::time::timeout(
            std::time::Duration::from_secs(7),
            read_one_message::<ServerMsg>(&mut stream),
        )
        .await
        .expect("server did not fall back within the confirmation bound")
        .unwrap();
        assert!(matches!(initial, ServerMsg::ScreenUpdate(_)));
        assert!(started.elapsed() >= std::time::Duration::from_secs(5));
        assert!(
            !server.is_finished(),
            "attach dropped after timeout fallback"
        );
        let dims = *manager.lock().await.get(name).unwrap().dims.lock().unwrap();
        assert_eq!((dims.cols, dims.rows), (7, 3));

        stream
            .write_all(&protocol::encode(&ClientMsg::Detach).unwrap())
            .await
            .unwrap();
        server.await.unwrap().unwrap();
        drop_test_session(manager, name, &dir).await;
    }

    #[tokio::test]
    async fn b2_confirm_only_capability_repaints_at_confirmed_width() {
        let name = "b2-confirm-only";
        let manager = Arc::new(Mutex::new(crate::session::SessionManager::new()));
        {
            let mut mgr = manager.lock().await;
            mgr.create_detached(
                name.into(),
                5,
                2,
                16,
                crate::pty::CommandSpec::login_shell_c("sleep 60"),
                std::env::current_dir().unwrap(),
            )
            .unwrap();
            mgr.get(name)
                .unwrap()
                .screen
                .lock()
                .unwrap()
                .process(b"\x1bcABCD");
        }

        let (dir, server) = serve_one_client("confirm-only", name, manager.clone()).await;
        let mut stream =
            connect_with_advertised_caps(&dir, name, &[protocol::INITIAL_SIZE_CONFIRM_V1_CAP])
                .await;
        send_attach_and_read_connected(&mut stream, name, 5, 2).await;
        stream
            .write_all(&protocol::encode(&ClientMsg::ConfirmSize { cols: 3, rows: 2 }).unwrap())
            .await
            .unwrap();

        let update = read_one_message::<ServerMsg>(&mut stream).await.unwrap();
        let ServerMsg::ScreenUpdate(update) = update else {
            panic!("expected initial ScreenUpdate without logical history capability");
        };
        let dims = *manager.lock().await.get(name).unwrap().dims.lock().unwrap();
        assert_eq!((dims.cols, dims.rows), (3, 2));
        assert!(update.windows(3).any(|window| window == b"ABC"));
        assert!(!update.windows(4).any(|window| window == b"ABCD"));

        stream
            .write_all(&protocol::encode(&ClientMsg::Detach).unwrap())
            .await
            .unwrap();
        server.await.unwrap().unwrap();
        drop_test_session(manager, name, &dir).await;
    }

    /// B-1 real path: actual Unix socket + Hello negotiation + public
    /// get_history_session client. One logical line is larger than the codec's
    /// per-frame cap, so the server must send multiple HistoryLogical frames
    /// and the client must wait for the empty HistoryLogical completion frame
    /// before rendering it.
    #[tokio::test]
    async fn b1_oversized_logical_line_reassembles_on_real_get_history_client() {
        let name = "b1-stream";
        let manager = Arc::new(Mutex::new(crate::session::SessionManager::new()));
        {
            let mut mgr = manager.lock().await;
            mgr.create_detached(
                name.into(),
                1,
                1,
                64,
                crate::pty::CommandSpec::login_shell_c("sleep 60"),
                std::env::current_dir().unwrap(),
            )
            .unwrap();
            let combining = "\u{301}".repeat(500_000);
            let chunks = (0..18)
                .map(|index| crate::screen::LogicalHistoryChunk {
                    cells: vec![crate::screen::LogicalCell {
                        ch: 'a',
                        display_width: 1,
                        combining: combining.clone(),
                        style: crate::screen::Style::default(),
                        wide_early_padding: false,
                    }],
                    end_of_line: index == 17,
                })
                .collect();
            mgr.get_mut(name)
                .unwrap()
                .set_logical_history_override(crate::screen::LogicalTransportEmission { chunks });
        }

        let (dir, server) = serve_one_client("b1", name, manager.clone()).await;
        let lines = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            get_history_session(Some(&dir), name),
        )
        .await
        .expect("oversized history client timed out")
        .expect("oversized history client failed");
        server.await.unwrap().unwrap();

        assert_eq!(
            lines.len(),
            1,
            "frame boundary must not invent a line break"
        );
        assert!(
            lines[0].len() > crate::protocol::codec::MAX_FRAME_SIZE,
            "fixture must exceed the per-message cap (got {} bytes, {} base cells)",
            lines[0].len(),
            lines[0].iter().filter(|&&byte| byte == b'a').count()
        );
        assert_eq!(lines[0].iter().filter(|&&byte| byte == b'a').count(), 18);
        assert!(!lines[0].windows(2).any(|w| w == b"\r\n"));

        drop_test_session(manager, name, &dir).await;
    }

    /// B-2 real path through the production attach handshake helper and actual
    /// server bridge. The terminal changes from 5 to 3 columns after Connect;
    /// ConfirmSize forces the server resize before the full ScreenUpdate.
    #[tokio::test]
    async fn b2_resize_during_handshake_repaints_at_confirmed_width() {
        let name = "b2-width";
        let manager = Arc::new(Mutex::new(crate::session::SessionManager::new()));
        {
            let mut mgr = manager.lock().await;
            mgr.create_detached(
                name.into(),
                5,
                2,
                16,
                crate::pty::CommandSpec::login_shell_c("sleep 60"),
                std::env::current_dir().unwrap(),
            )
            .unwrap();
            let screen = mgr.get(name).unwrap().screen.clone();
            drop(mgr);
            let mut screen = screen.lock().unwrap();
            screen.process(b"\x1bcABCD");
        }

        let (dir, server) = serve_one_client("b2", name, manager.clone()).await;
        let (mut stream, caps) = connect_session_stream_with_caps(Some(&dir), name)
            .await
            .unwrap();
        assert!(caps.initial_size_confirm);
        let handshake = perform_attach_handshake(
            &mut stream,
            name,
            16,
            crate::protocol::ConnectMode::AttachOnly,
            caps,
            (5, 2),
            || (3, 2),
        )
        .await
        .unwrap();
        assert!(
            handshake.size_disagreed,
            "post-Connected resample branch did not fire"
        );

        let mut frames = FrameReader::with_leftover(handshake.leftover);
        let mut logical_chunks = Vec::new();
        let screen_update = loop {
            if let Some(msg) = frames.decode_next::<ServerMsg>().unwrap() {
                match msg {
                    ServerMsg::HistoryLogical(mut chunks) => {
                        if !chunks.is_empty() {
                            logical_chunks.append(&mut chunks)
                        }
                    }
                    ServerMsg::ScreenUpdate(data) => break data,
                    other => panic!("unexpected initial attach frame: {other:?}"),
                }
            } else {
                assert!(frames.fill_from(&mut stream).await.unwrap());
            }
        };

        let confirmed_dims = *manager.lock().await.get(name).unwrap().dims.lock().unwrap();
        assert_eq!((confirmed_dims.cols, confirmed_dims.rows), (3, 2));
        assert!(
            screen_update.windows(3).any(|w| w == b"ABC"),
            "confirmed-width repaint omitted retained cells"
        );
        assert!(
            !screen_update.windows(4).any(|w| w == b"ABCD"),
            "stale 5-column snapshot leaked into the repaint"
        );

        let mut rendered = Vec::new();
        crate::client::dispatch_server_msg(
            &ServerMsg::HistoryLogical(logical_chunks),
            &mut rendered,
        )
        .unwrap();
        crate::client::dispatch_server_msg(&ServerMsg::ScreenUpdate(screen_update), &mut rendered)
            .unwrap();
        let mut outer = crate::screen::Screen::new(3, 2, 16);
        outer.process(&rendered);
        let visible = outer.get_content_history();
        assert!(visible.iter().any(|line| line == b"ABC"));
        assert!(!visible
            .iter()
            .any(|line| line.windows(4).any(|w| w == b"ABCD")));

        stream
            .write_all(&protocol::encode(&ClientMsg::Detach).unwrap())
            .await
            .unwrap();
        server.await.unwrap().unwrap();
        drop_test_session(manager, name, &dir).await;
    }
}
