//! Client-side logic for connecting to the retach server, managing sessions, and relaying I/O.

pub mod discovery;
pub mod raw_mode;
pub mod server_launcher;
pub mod session_client;

use crate::protocol::{self, ClientMsg, FrameReader, ServerMsg};
use std::io::{self, BufWriter, Read, Write};
use tokio::io::AsyncWriteExt;

/// Detach key: Ctrl+\ (0x1c).
const DETACH_KEY: u8 = 0x1c;
/// Focus-in event: ESC [ I
const FOCUS_IN: u8 = b'I';
/// Focus-out event: ESC [ O
const FOCUS_OUT: u8 = b'O';

/// RAII guard that removes the custom panic hook on drop.
pub(crate) struct PanicHookGuard;

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        // Remove our custom panic hook. This discards it (including the
        // captured prev_hook) and installs Rust's default panic hook.
        // The previous hook cannot be restored because it was moved into
        // our custom hook's closure. In practice, retach's binary has no
        // custom panic hook before connect(), so this is a no-op loss.
        if !std::thread::panicking() {
            let _ = std::panic::take_hook();
        }
    }
}

/// Result of dispatching a server message to stdout.
pub(crate) enum DispatchResult {
    Continue,
    Done,
}

/// Write a single ServerMsg to stdout. Returns `Done` for terminal messages.
pub(crate) fn dispatch_server_msg(
    msg: &ServerMsg,
    stdout: &mut impl Write,
) -> io::Result<DispatchResult> {
    match msg {
        ServerMsg::ScreenUpdate(data) => {
            stdout.write_all(data)?;
        }
        ServerMsg::Passthrough(data) => {
            stdout.write_all(data)?;
            stdout.flush()?;
        }
        ServerMsg::History(lines) => {
            for line in lines {
                stdout.write_all(line)?;
                stdout.write_all(b"\r\n")?;
            }
        }
        ServerMsg::SessionEnded => {
            stdout.flush()?;
            eprintln!("[retach: session ended]");
            return Ok(DispatchResult::Done);
        }
        ServerMsg::Error(e) => {
            stdout.flush()?;
            eprintln!("[retach error: {}]", e);
            return Ok(DispatchResult::Done);
        }
        other => {
            tracing::debug!(
                "ignoring unexpected server message: {:?}",
                std::mem::discriminant(other)
            );
        }
    }
    Ok(DispatchResult::Continue)
}

pub(crate) fn get_terminal_size() -> (u16, u16) {
    if let Some(size) = terminal_size::terminal_size() {
        (size.0 .0, size.1 .0)
    } else {
        (crate::session::DEFAULT_COLS, crate::session::DEFAULT_ROWS)
    }
}

pub(crate) type SocketWriter = std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>;

// WS-C M3b: the legacy `client_handshake` (the shared-daemon Hello drain) is
// RETIRED with the legacy connect/`*_data` paths. The per-session
// `session_client::session_handshake` (with the §3.2 identity belt) is the only
// client handshake now.

/// Action produced by the input filter.
#[derive(Debug, PartialEq)]
enum FilterAction {
    /// Forward filtered bytes to the server.
    Forward(Vec<u8>),
    /// Client requested detach (Ctrl+\).
    Detach,
    /// Terminal reported focus gained — trigger screen refresh.
    FocusIn,
}

/// Filters stdin input: handles detach key, focus events, and split escape sequences.
struct InputFilter {
    carry: Vec<u8>,
}

impl InputFilter {
    fn new() -> Self {
        Self {
            carry: Vec::with_capacity(2),
        }
    }

    /// Push accumulated bytes as a Forward action if non-empty.
    fn flush_filtered(actions: &mut Vec<FilterAction>, filtered: &mut Vec<u8>) {
        if !filtered.is_empty() {
            actions.push(FilterAction::Forward(std::mem::take(filtered)));
        }
    }

    /// Process raw input bytes, returning a list of actions.
    fn process(&mut self, input: &[u8]) -> Vec<FilterAction> {
        let raw: Vec<u8> = if self.carry.is_empty() {
            input.to_vec()
        } else {
            let mut combined = std::mem::take(&mut self.carry);
            combined.extend_from_slice(input);
            combined
        };

        // Check for detach key first
        if let Some(pos) = raw.iter().position(|&b| b == DETACH_KEY) {
            let mut actions = Vec::new();
            if pos > 0 {
                actions.push(FilterAction::Forward(raw[..pos].to_vec()));
            }
            actions.push(FilterAction::Detach);
            return actions;
        }

        let mut actions = Vec::new();
        let mut filtered = Vec::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            if raw[i] != 0x1b {
                filtered.push(raw[i]);
                i += 1;
                continue;
            }

            // ESC at end of buffer — carry for next call
            let remaining = raw.len() - i;
            if remaining < 2 {
                Self::flush_filtered(&mut actions, &mut filtered);
                self.carry.extend_from_slice(&raw[i..]);
                return actions;
            }

            // ESC <non-[> — pass through both bytes as-is.
            // Single-byte advance would work by coincidence (the second byte is not
            // 0x1b so it passes through next iteration), but advancing by 2 makes
            // the intent explicit and prevents breakage if filtering rules change.
            if raw[i + 1] != b'[' {
                filtered.push(raw[i]);
                filtered.push(raw[i + 1]);
                i += 2;
                continue;
            }

            // ESC [ at end of buffer — carry for next call
            if remaining < 3 {
                Self::flush_filtered(&mut actions, &mut filtered);
                self.carry.extend_from_slice(&raw[i..]);
                return actions;
            }

            // ESC [ I — focus in
            if raw[i + 2] == FOCUS_IN {
                Self::flush_filtered(&mut actions, &mut filtered);
                actions.push(FilterAction::FocusIn);
                i += 3;
                continue;
            }

            // ESC [ O — focus out (consumed, not forwarded)
            if raw[i + 2] == FOCUS_OUT {
                i += 3;
                continue;
            }

            // ESC [ <other> — pass through
            filtered.push(raw[i]);
            i += 1;
        }
        Self::flush_filtered(&mut actions, &mut filtered);
        actions
    }

    /// Flush any remaining carry bytes as a Forward action.
    fn flush(&mut self) -> Option<FilterAction> {
        if self.carry.is_empty() {
            None
        } else {
            Some(FilterAction::Forward(std::mem::take(&mut self.carry)))
        }
    }
}

/// Stdin → socket relay: reads stdin, handles detach key and focus events,
/// and forwards input to the server.
pub(crate) async fn run_stdin_to_socket(sw: SocketWriter) -> anyhow::Result<()> {
    let mut filter = InputFilter::new();

    'stdin: loop {
        let result = tokio::task::spawn_blocking(|| {
            // 1 KiB stdin buffer — matches typical terminal input rates. Even fast
            // paste operations are chunked by the OS at ~4 KiB pipe buffer boundaries,
            // so 1 KiB reads keep latency low without excessive syscalls.
            let mut buf = [0u8; 1024];
            let n = io::stdin().read(&mut buf)?;
            Ok::<_, io::Error>((buf, n))
        })
        .await;

        match result {
            Ok(Ok((_buf, 0))) => {
                if let Some(FilterAction::Forward(data)) = filter.flush() {
                    let msg = protocol::encode(&ClientMsg::Input(data))?;
                    let mut w = sw.lock().await;
                    w.write_all(&msg).await?;
                }
                break;
            }
            Ok(Ok((buf, n))) => {
                for action in filter.process(&buf[..n]) {
                    match action {
                        FilterAction::Forward(data) => {
                            let msg = protocol::encode(&ClientMsg::Input(data))?;
                            let mut w = sw.lock().await;
                            w.write_all(&msg).await?;
                        }
                        FilterAction::Detach => {
                            let mut w = sw.lock().await;
                            if let Ok(msg) = protocol::encode(&ClientMsg::Detach) {
                                w.write_all(&msg).await?;
                            }
                            drop(w);
                            return Ok(());
                        }
                        FilterAction::FocusIn => {
                            if let Ok(msg) = protocol::encode(&ClientMsg::RefreshScreen) {
                                let mut w = sw.lock().await;
                                if let Err(e) = w.write_all(&msg).await {
                                    tracing::debug!(error = %e, "failed to send focus-in refresh");
                                    break 'stdin;
                                }
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => return Err(anyhow::Error::from(e)),
            Err(e) => return Err(anyhow::Error::from(e)),
        }
    }
    Ok(())
}

/// Spawn SIGWINCH handler that sends Resize + RefreshScreen on terminal size change.
pub(crate) fn spawn_sigwinch_handler(
    sock_writer: SocketWriter,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let sw = sock_writer;
    Ok(tokio::spawn(async move {
        while sigwinch.recv().await.is_some() {
            let (cols, rows) = get_terminal_size();
            let mut w = sw.lock().await;
            if let Ok(msg) = protocol::encode(&ClientMsg::Resize { cols, rows }) {
                if let Err(e) = w.write_all(&msg).await {
                    tracing::debug!(error = %e, "failed to send resize");
                    break;
                }
            }
            if let Ok(msg) = protocol::encode(&ClientMsg::RefreshScreen) {
                if let Err(e) = w.write_all(&msg).await {
                    tracing::debug!(error = %e, "failed to send refresh after resize");
                    break;
                }
            }
        }
    }))
}

/// Socket → stdout relay: reads server messages, dispatches them to stdout.
pub(crate) async fn run_socket_to_stdout(
    mut sock_reader: tokio::net::unix::OwnedReadHalf,
    leftover: Vec<u8>,
) -> anyhow::Result<()> {
    let mut frames = FrameReader::with_leftover(leftover);
    let mut stdout = BufWriter::new(io::stdout());

    // Process any complete frames already in the leftover buffer
    while let Some(msg) = frames.decode_next::<ServerMsg>()? {
        if matches!(
            dispatch_server_msg(&msg, &mut stdout)?,
            DispatchResult::Done
        ) {
            return Ok(());
        }
    }
    stdout.flush()?;

    loop {
        if !frames.fill_from(&mut sock_reader).await? {
            eprintln!("[retach: detached]");
            break;
        }
        while let Some(msg) = frames.decode_next::<ServerMsg>()? {
            if matches!(
                dispatch_server_msg(&msg, &mut stdout)?,
                DispatchResult::Done
            ) {
                return Ok(());
            }
        }
        stdout.flush()?;
    }
    Ok(())
}

/// The exact byte sequence `cleanup_terminal` writes. Extracted as a const so
/// tests can pin its contents (the `?1049l` below is load-bearing — see
/// doc/inbox/2026-06-10-qrmux-phone-scroll-regression.md).
pub(crate) const CLEANUP_SEQUENCE: &str = concat!(
    // Exit the alternate screen FIRST: the render layer replays the inner
    // app's `?1049h` to this client (screen/render.rs alt-screen replay), so
    // detaching from a fullscreen session would otherwise strand the user's
    // shell in the alt buffer. Harmless no-op when already on the main screen.
    "\x1b[?1049l",
    "\x1b[r",      // reset scroll region to full screen (also moves cursor to home)
    "\x1b[2J",     // clear screen
    "\x1b[H",      // cursor to home
    "\x1b[?25h",   // show cursor
    "\x1b[?7h",    // re-enable auto-wrap
    "\x1b[?1l",    // normal cursor keys (DECCKM reset)
    "\x1b[?2004l", // disable bracketed paste
    "\x1b[?1000l", // disable mouse click tracking
    "\x1b[?1002l", // disable mouse button tracking
    "\x1b[?1003l", // disable mouse any-event tracking
    "\x1b[?1005l", // disable UTF-8 mouse encoding
    "\x1b[?1006l", // disable SGR mouse encoding
    "\x1b[?1004l", // disable focus reporting
    "\x1b[?2026l", // disable synchronized output
    "\x1b>",       // normal keypad mode (DECKPNM)
    "\x1b[0 q",    // default cursor shape
    "\x1b[0m",     // reset all SGR attributes
    // After clearing the screen the user would otherwise face a blank
    // void with no indication the session has ended. Print a terse hint
    // at the (now home) cursor so they know to press enter to get their
    // shell prompt back. Lives here so BOTH teardown paths
    // (normal exit + panic hook) show it exactly once.
    "[press enter]",
);

/// Reset terminal modes after detach/disconnect so the user's shell isn't left
/// with hidden cursor, mouse capture, bracketed paste, etc.
pub(crate) fn cleanup_terminal() {
    let mut stdout = io::stdout();
    // Best-effort terminal cleanup — errors are expected if stdout is broken
    let _ = stdout.write_all(CLEANUP_SEQUENCE.as_bytes());
    // Best-effort terminal cleanup — errors are expected if stdout is broken
    let _ = stdout.flush();
}

// ===========================================================================
// WS-C M3b: standalone-CLI printers over the PER-SESSION client surface.
//
// The legacy shared-daemon `*_data` library variants (which bound `qrmux.sock`)
// are RETIRED (spec §1, §9): the engine uses `session_client::*` + `scan_sessions`
// directly; the standalone CLI's List/Kill/Send printers below use the same
// per-session world (D-SOCKDIR mirror). Nothing binds or expects `qrmux.sock`
// anymore — the §5.3 legacy-visibility probe was dropped as part of the cat-(iii)
// clean break (`qrmux.sock` is still excluded from per-session scanning via the
// LEGACY_LEAF guard in `discovery::scan_sessions`).
// ===========================================================================

/// List active sessions (standalone CLI `List`) via the per-session dir-scan +
/// probe (`scan_sessions`, spec §4.3) and print them to stdout. Empty when no
/// per-session daemon is live.
pub async fn list_sessions() -> anyhow::Result<()> {
    let sessions = super::client::discovery::scan_sessions(None).await?;
    if sessions.is_empty() {
        println!("No active sessions");
    } else {
        for s in sessions {
            println!("{} ({}x{})", s.name, s.cols, s.rows);
        }
    }
    Ok(())
}

/// Kill the named session (standalone CLI `Kill`) by addressing its own daemon
/// (`<dir>/<name>.sock`, spec §4.1). Read-side: no cold-start — a kill of an
/// absent session surfaces the connect error.
pub async fn kill_session(name: &str) -> anyhow::Result<()> {
    super::client::session_client::kill_session_session(None, name).await?;
    println!("killed session '{}'", name);
    Ok(())
}

/// Out-of-band one-shot send to the named session's PTY (standalone CLI `Send`),
/// addressing its own daemon and cold-starting it if needed (spec §4.1). Prints
/// the acked byte count.
pub async fn send_input(name: &str, data: Vec<u8>) -> anyhow::Result<()> {
    let bytes = super::client::session_client::send_input_session(None, None, name, data).await?;
    eprintln!("[retach: sent {} bytes to '{}']", bytes, name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Detach cleanup must exit the alternate screen, and must do so FIRST:
    /// the render layer replays the inner app's `?1049h` to clients, so a
    /// detach from a fullscreen session (Claude Code, vim) would otherwise
    /// strand the user's shell in the alt buffer — and the `\x1b[2J` clear
    /// must hit the MAIN buffer, after the switch back.
    #[test]
    fn cleanup_sequence_exits_alt_screen_first() {
        assert!(
            CLEANUP_SEQUENCE.starts_with("\x1b[?1049l"),
            "cleanup must exit the alt screen before any other reset, got: {:?}",
            &CLEANUP_SEQUENCE[..16.min(CLEANUP_SEQUENCE.len())]
        );
    }

    #[test]
    fn dispatch_server_msg_error_returns_done() {
        let msg = ServerMsg::Error("test error".into());
        let mut buf = Vec::new();
        let result = dispatch_server_msg(&msg, &mut buf).unwrap();
        assert!(matches!(result, DispatchResult::Done));
    }

    #[test]
    fn dispatch_server_msg_session_ended_returns_done() {
        let msg = ServerMsg::SessionEnded;
        let mut buf = Vec::new();
        let result = dispatch_server_msg(&msg, &mut buf).unwrap();
        assert!(matches!(result, DispatchResult::Done));
    }

    #[test]
    fn dispatch_server_msg_screen_update_continues() {
        let msg = ServerMsg::ScreenUpdate(b"hello".to_vec());
        let mut buf = Vec::new();
        let result = dispatch_server_msg(&msg, &mut buf).unwrap();
        assert!(matches!(result, DispatchResult::Continue));
        assert_eq!(buf, b"hello");
    }

    #[test]
    fn input_filter_passthrough() {
        let mut filter = InputFilter::new();
        let result = filter.process(b"hello");
        assert_eq!(result, vec![FilterAction::Forward(b"hello".to_vec())]);
    }

    #[test]
    fn input_filter_detach() {
        let mut filter = InputFilter::new();
        let result = filter.process(b"abc\x1cdef");
        assert_eq!(
            result,
            vec![FilterAction::Forward(b"abc".to_vec()), FilterAction::Detach,]
        );
    }

    #[test]
    fn input_filter_detach_at_start() {
        let mut filter = InputFilter::new();
        let result = filter.process(b"\x1c");
        assert_eq!(result, vec![FilterAction::Detach]);
    }

    #[test]
    fn input_filter_focus_in() {
        let mut filter = InputFilter::new();
        let result = filter.process(b"\x1b[I");
        assert_eq!(result, vec![FilterAction::FocusIn]);
    }

    #[test]
    fn input_filter_focus_out_dropped() {
        let mut filter = InputFilter::new();
        let result = filter.process(b"\x1b[O");
        assert!(result.is_empty());
    }

    #[test]
    fn input_filter_carry_lone_esc() {
        let mut filter = InputFilter::new();
        let result = filter.process(b"abc\x1b");
        assert_eq!(result, vec![FilterAction::Forward(b"abc".to_vec())]);
        // Complete the sequence
        let result = filter.process(b"[Amore");
        assert_eq!(result, vec![FilterAction::Forward(b"\x1b[Amore".to_vec())]);
    }

    #[test]
    fn input_filter_carry_esc_bracket() {
        let mut filter = InputFilter::new();
        let result = filter.process(b"\x1b[");
        assert!(result.is_empty());
        let result = filter.process(b"Irest");
        assert_eq!(
            result,
            vec![
                FilterAction::FocusIn,
                FilterAction::Forward(b"rest".to_vec()),
            ]
        );
    }

    #[test]
    fn input_filter_flush() {
        let mut filter = InputFilter::new();
        let _ = filter.process(b"\x1b");
        let flushed = filter.flush();
        assert_eq!(flushed, Some(FilterAction::Forward(vec![0x1b])));
        // Double flush is empty
        assert_eq!(filter.flush(), None);
    }

    #[test]
    fn input_filter_esc_non_bracket_passthrough() {
        let mut filter = InputFilter::new();
        let result = filter.process(b"\x1bOA"); // ESC O A (arrow key in app mode)
        assert_eq!(result, vec![FilterAction::Forward(b"\x1bOA".to_vec())]);
    }

    #[test]
    fn input_filter_mixed() {
        let mut filter = InputFilter::new();
        let result = filter.process(b"text\x1b[Imore");
        assert_eq!(
            result,
            vec![
                FilterAction::Forward(b"text".to_vec()),
                FilterAction::FocusIn,
                FilterAction::Forward(b"more".to_vec()),
            ]
        );
    }

    #[test]
    fn input_filter_esc_bracket_at_boundary() {
        let mut f = InputFilter::new();
        // ESC [ split across two calls
        let a1 = f.process(b"\x1b");
        assert!(a1.is_empty(), "lone ESC should be carried");
        let a2 = f.process(b"[I");
        assert_eq!(a2, vec![FilterAction::FocusIn]);
    }

    #[test]
    fn input_filter_multiple_focus_events() {
        let mut f = InputFilter::new();
        let actions = f.process(b"a\x1b[Ib\x1b[Oc");
        assert_eq!(
            actions,
            vec![
                FilterAction::Forward(b"a".to_vec()),
                FilterAction::FocusIn,
                FilterAction::Forward(b"bc".to_vec()),
            ]
        );
    }
}
