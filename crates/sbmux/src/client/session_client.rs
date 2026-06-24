//! WS-C M3a: per-session client surface (spec §3.2 client identity belt, §4.2,
//! §4.4). NEW surface ALONGSIDE the intact legacy `client/mod.rs` verbs — the
//! engine (crates/sb) does not call these until M3b flips it.
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

/// Build the client-side identity-mismatch error (§3.2 step 3). Exact string —
/// tests assert on it. `path` is the socket the client connected to, `actual`
/// is the `ServerHello.session` the daemon reported, `expected` is the name the
/// client intended to reach.
fn identity_mismatch_error(path: &Path, actual: &str, expected: &str) -> String {
    format!(
        "sbmux daemon at {} identifies as session '{}', expected '{}'",
        path.display(),
        actual,
        expected
    )
}

/// v3 client Hello handshake WITH the §3.2 identity belt (the per-session
/// handshake — the legacy shared-daemon `client_handshake` was retired in M3b).
///
/// Sends `ClientMsg::Hello { caps: vec![] }`, reads the `ServerMsg::Hello` as
/// the server's FIRST reply frame, and verifies `ServerHello.session == name`.
/// Mismatch → the named [`identity_mismatch_error`]; a framed `Error` (e.g. the
/// "retiring" session-ended refusal) is surfaced verbatim so the launcher can
/// classify it.
///
/// Uses [`FrameReader`] (NOT `read_one_message`) so any bytes a pipelining peer
/// packed behind the ServerHello are preserved as leftover (codec.rs note,
/// red-team M6). v3 servers send nothing until their next reply, so in practice
/// there is no leftover here; the caller discards it (its subsequent
/// `read_one_message` re-reads the verb reply cleanly).
async fn session_handshake(stream: &mut UnixStream, path: &Path, name: &str) -> anyhow::Result<()> {
    let hello = protocol::encode(&ClientMsg::Hello { caps: vec![] })?;
    stream.write_all(&hello).await?;
    let mut frames = FrameReader::new();
    loop {
        if !frames.fill_from(stream).await? {
            anyhow::bail!("server closed connection before sending Hello");
        }
        if let Some(msg) = frames.decode_next::<ServerMsg>()? {
            match msg {
                ServerMsg::Hello { session, .. } => {
                    // G-ISOL negative-control seam (spec §7, red-team M4): under
                    // `SBMUX_TEST_SHARED=1` ALL names collapse onto one `shared.sock`
                    // daemon whose ServerHello.session is whatever it was launched as,
                    // so the per-session identity belt is relaxed (it would otherwise
                    // reject every connect for a different name than the daemon's
                    // launch identity). Inert in production (env unset).
                    if session != name && !crate::server::socket::shared_fate_test_mode() {
                        anyhow::bail!(identity_mismatch_error(path, &session, name));
                    }
                    return Ok(());
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
    crate::server::socket::validate_session_identity(name)?;
    let path = session_socket_path_for(socket_dir, name)?;
    let mut stream = UnixStream::connect(&path).await?;
    protocol::write_preamble(&mut stream).await?;
    session_handshake(&mut stream, &path, name).await?;
    Ok(stream)
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
    let mut stream = connect_session_stream(socket_dir, name).await?;
    let msg = protocol::encode(&ClientMsg::GetHistory {
        name: name.to_string(),
    })?;
    stream.write_all(&msg).await?;
    match read_one_message(&mut stream).await? {
        ServerMsg::History(lines) => Ok(lines),
        ServerMsg::Error(e) => anyhow::bail!("{}", e),
        other => anyhow::bail!(
            "unexpected server response: {:?}",
            std::mem::discriminant(&other)
        ),
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

/// Per-session `launch_headless` (WP-B-CS-1, §4.1 LaunchHeadless verb) — the
/// CLIENT-SIDE sender the daemon side (B2b-2b) reserved but never wired. Ensures
/// the session's daemon is running (cold-start spawns one), connects with the
/// §3.2 identity belt, and sends [`ClientMsg::LaunchHeadless`] so the daemon
/// spawns a `claude -p` stream-json turn for `name` (resolved daemon-side via its
/// injected `HeadlessFactory`). `resume_session_id` continues an existing claude
/// session (`--resume`), else a fresh launch.
///
/// This is the wiring that makes the DORMANT prod trigger LIVE: `sb start` (agent
/// driver) and `sb resume` route here. Returns on the daemon's `Connected` ack;
/// a framed `Error` (a daemon with no headless support, or a launch-resolve
/// failure) is surfaced verbatim — never swallowed as success.
pub async fn launch_headless_session(
    socket_dir: Option<&Path>,
    launch: Option<&ServerLaunchSpec>,
    name: &str,
    prompt: &str,
    resume_session_id: Option<&str>,
    cwd: Option<&str>,
    claude_args: &[String],
) -> anyhow::Result<()> {
    ensure_session_server_running(socket_dir, name, launch).await?;
    let mut stream = connect_session_stream(socket_dir, name).await?;
    send_launch_headless(
        &mut stream,
        name,
        prompt,
        resume_session_id,
        cwd,
        claude_args,
    )
    .await
}

/// The wire half of [`launch_headless_session`]: encode + send the
/// `LaunchHeadless` verb on an already-handshaken `stream`, then read the single
/// reply. Split out so the send is unit-testable against a fake daemon socket
/// WITHOUT spawning a real daemon or a real `claude` (the fake-seam DoD).
async fn send_launch_headless(
    stream: &mut UnixStream,
    name: &str,
    prompt: &str,
    resume_session_id: Option<&str>,
    cwd: Option<&str>,
    claude_args: &[String],
) -> anyhow::Result<()> {
    let msg = protocol::encode(&ClientMsg::LaunchHeadless {
        name: name.to_string(),
        prompt: prompt.to_string(),
        resume_session_id: resume_session_id.map(str::to_string),
        cwd: cwd.map(str::to_string),
        claude_args: claude_args.to_vec(),
    })?;
    stream.write_all(&msg).await?;
    match read_one_message(stream).await? {
        ServerMsg::Connected { .. } => Ok(()),
        ServerMsg::Error(e) => anyhow::bail!("{}", e),
        other => anyhow::bail!(
            "unexpected server response: {:?}",
            std::mem::discriminant(&other)
        ),
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
    let mut stream = connect_session_stream(socket_dir, name).await?;

    let (cols, rows) = super::get_terminal_size();
    let msg = protocol::encode(&ClientMsg::Connect {
        name: name.to_string(),
        history,
        cols,
        rows,
        mode,
    })?;
    stream.write_all(&msg).await?;

    // Wait for Connected/Error before entering raw mode so errors display
    // correctly. (Same shape as legacy connect_for_with.)
    let mut frames = FrameReader::new();
    loop {
        if !frames.fill_from(&mut stream).await? {
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
                    break;
                }
                ServerMsg::Error(e) => anyhow::bail!("{}", e),
                _ => anyhow::bail!("unexpected response from server"),
            }
        }
    }
    let leftover = frames.into_leftover();

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

    let mut stdin_task = tokio::spawn(super::run_stdin_to_socket(sock_writer.clone()));
    let mut socket_task = tokio::spawn(super::run_socket_to_stdout(sock_reader, leftover));

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
    use crate::protocol::{read_preamble, PreambleCheck};
    use tokio::net::UnixListener;

    #[test]
    fn identity_mismatch_error_exact_shape() {
        let err = identity_mismatch_error(Path::new("/run/sbmux/beta.sock"), "alpha", "beta");
        assert_eq!(
            err,
            "sbmux daemon at /run/sbmux/beta.sock identifies as session 'alpha', expected 'beta'"
        );
    }

    /// A fake per-session daemon for ONE connection: speak the server side of the
    /// v3 handshake (read preamble + client Hello, send ServerHello identifying as
    /// `session`), then read the client's next verb frame, RECORD it, and reply
    /// with `reply`. Returns the recorded [`ClientMsg`] so a test can assert the
    /// exact wire frame the client sent. No real daemon, no real `claude`.
    async fn fake_daemon_once(
        listener: UnixListener,
        session: &str,
        reply: ServerMsg,
    ) -> ClientMsg {
        let (mut s, _) = listener.accept().await.unwrap();
        assert_eq!(read_preamble(&mut s).await.unwrap(), PreambleCheck::Ok);
        // Client Hello (connect_session_stream sends it before reading ServerHello).
        let _client_hello: ClientMsg = read_one_message(&mut s).await.unwrap();
        let server_hello = protocol::encode(&ServerMsg::Hello {
            caps: vec![],
            session: session.to_string(),
        })
        .unwrap();
        s.write_all(&server_hello).await.unwrap();
        // The verb frame under test.
        let frame: ClientMsg = read_one_message(&mut s).await.unwrap();
        let encoded = protocol::encode(&reply).unwrap();
        s.write_all(&encoded).await.unwrap();
        frame
    }

    /// GREEN: the sender writes a `LaunchHeadless` frame carrying the EXACT
    /// name/prompt/resume_session_id, and a `Connected` ack resolves it Ok.
    ///
    /// FIX-SHAPED MUTATION (red-before): drop the resume id in `send_launch_headless`
    /// (`resume_session_id: None`) or alter the prompt → the recorded frame stops
    /// equalling the expectation below → this REDs. The frame is the contract; a
    /// drifted client send is caught wire-for-wire.
    #[tokio::test]
    async fn launch_headless_sends_exact_frame_and_accepts_connected() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = session_socket_path_for(Some(tmp.path()), "alpha").unwrap();
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            fake_daemon_once(
                listener,
                "alpha",
                ServerMsg::Connected {
                    name: "alpha".into(),
                    new_session: true,
                },
            )
            .await
        });

        let mut stream = connect_session_stream(Some(tmp.path()), "alpha")
            .await
            .unwrap();
        // WP-B5-i: a POPULATED cwd + claude_args round-trip — the appended-tail
        // fields must ride the wire verbatim (the byte-stability proof's positive
        // half; the None/empty defaulted half is the `..._surfaces_daemon_error`
        // case below).
        send_launch_headless(
            &mut stream,
            "alpha",
            "do the thing",
            Some("sess-42"),
            Some("/work/proj"),
            &["--model".to_string(), "opus".to_string()],
        )
        .await
        .expect("Connected ack resolves Ok");

        let recorded = server.await.unwrap();
        assert_eq!(
            recorded,
            ClientMsg::LaunchHeadless {
                name: "alpha".into(),
                prompt: "do the thing".into(),
                resume_session_id: Some("sess-42".into()),
                cwd: Some("/work/proj".into()),
                claude_args: vec!["--model".into(), "opus".into()],
            },
            "the wire frame must carry the exact name/prompt/resume id/cwd/claude_args"
        );
    }

    /// FALSE-POSITIVE GUARD: a daemon that refuses (no headless support / resolve
    /// failure replies `Error`) must surface as `Err` carrying the message — never
    /// be swallowed as a successful launch.
    #[tokio::test]
    async fn launch_headless_surfaces_daemon_error() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = session_socket_path_for(Some(tmp.path()), "beta").unwrap();
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            fake_daemon_once(
                listener,
                "beta",
                ServerMsg::Error("headless launch not supported by this daemon".into()),
            )
            .await
        });

        let mut stream = connect_session_stream(Some(tmp.path()), "beta")
            .await
            .unwrap();
        // WP-B5-i: the DEFAULTED appended-tail round-trip — None cwd + empty
        // claude_args ride the wire as None / `[]` (no fabricated cwd/flags).
        let err = send_launch_headless(&mut stream, "beta", "p", None, None, &[])
            .await
            .expect_err("a framed Error must surface as Err, not Ok");
        assert!(
            err.to_string().contains("headless launch not supported"),
            "the daemon's refusal message is surfaced verbatim: {err}"
        );

        let recorded = server.await.unwrap();
        // A None resume id rides the wire as None (no fabricated continuation).
        assert_eq!(
            recorded,
            ClientMsg::LaunchHeadless {
                name: "beta".into(),
                prompt: "p".into(),
                resume_session_id: None,
                cwd: None,
                claude_args: vec![],
            }
        );
    }
}
