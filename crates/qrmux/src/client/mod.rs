//! Client-side logic for connecting to the retach server, managing sessions, and relaying I/O.

pub mod discovery;
pub mod raw_mode;
pub mod server_launcher;
pub mod session_client;

use crate::protocol::{self, ClientMsg, FrameReader, ServerMsg};
use crate::screen::{LogicalCell, LogicalHistoryChunk, Style};
use std::io::{self, BufWriter, Read, Write};
use tokio::io::AsyncWriteExt;

/// Detach key: Ctrl+\ (0x1c).
const DETACH_KEY: u8 = 0x1c;
/// attended-UX M2 deliver-now chord: Ctrl+] (0x1d, GS — telnet-escape lineage,
/// pairs with detach's Ctrl+\ and is rarely used by TUIs). Fires a held send
/// immediately (M1's `ClientMsg::DeliverNow`), no countdown reset / journal entry.
/// The exact key is a **digest taste-residual** (gate item 6), changeable freely.
const DELIVER_NOW_KEY: u8 = 0x1d;
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

/// Reassemble cell chunks into logical lines and render each complete line as
/// one ANSI byte sequence. The bool records whether the transport supplied an
/// `end_of_line` terminator; only terminated lines receive CRLF on attach.
pub(crate) fn render_logical_history(chunks: &[LogicalHistoryChunk]) -> Vec<(Vec<u8>, bool)> {
    let mut lines = Vec::new();
    let mut cells = Vec::new();
    for chunk in chunks {
        cells.extend(chunk.cells.iter().cloned());
        if chunk.end_of_line {
            lines.push((render_logical_line(&cells), true));
            cells.clear();
        }
    }
    if !cells.is_empty() {
        lines.push((render_logical_line(&cells), false));
    }
    lines
}

fn render_logical_line(cells: &[LogicalCell]) -> Vec<u8> {
    // Tail detection intentionally runs over the reassembled logical line,
    // not each physical-row chunk. WideEarly padding is edge-qualified
    // omission metadata and never reaches the terminal.
    let last = cells.iter().rposition(|cell| {
        !cell.wide_early_padding
            && ((cell.ch != ' ' && cell.ch != '\0')
                || !cell.combining.is_empty()
                || cell.style != Style::default())
    });
    let mut out = Vec::new();
    let mut current_style = Style::default();
    for cell in cells.iter().take(last.map_or(0, |index| index + 1)) {
        if cell.wide_early_padding || cell.display_width == 0 {
            continue;
        }
        if cell.style != current_style {
            cell.style.write_sgr_with_reset_to(&mut out);
            current_style = cell.style;
        }
        let mut buf = [0u8; 4];
        out.extend_from_slice(cell.ch.encode_utf8(&mut buf).as_bytes());
        out.extend_from_slice(cell.combining.as_bytes());
    }
    if current_style != Style::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
    out
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
        ServerMsg::HistoryLogical(chunks) => {
            for (line, end_of_line) in render_logical_history(chunks) {
                stdout.write_all(&line)?;
                if end_of_line {
                    stdout.write_all(b"\r\n")?;
                }
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
    /// attended-UX M2: client requested deliver-now (Ctrl+]).
    DeliverNow,
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

        // Check for detach key first — it ends the session, so it takes precedence.
        if let Some(pos) = raw.iter().position(|&b| b == DETACH_KEY) {
            let mut actions = Vec::new();
            // Extract any deliver-now chords in the pre-detach bytes in stream order,
            // so a deliver-now that PRECEDES a detach in one read is honored — not
            // forwarded to the PTY as a literal GS (0x1d) byte (red-team r1 N3).
            let mut pre = Vec::with_capacity(pos);
            for &b in &raw[..pos] {
                if b == DELIVER_NOW_KEY {
                    Self::flush_filtered(&mut actions, &mut pre);
                    actions.push(FilterAction::DeliverNow);
                } else {
                    pre.push(b);
                }
            }
            Self::flush_filtered(&mut actions, &mut pre);
            actions.push(FilterAction::Detach);
            return actions;
        }

        let mut actions = Vec::new();
        let mut filtered = Vec::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            // attended-UX M2: deliver-now chord (Ctrl+]). Emitted as its own action
            // in stream order; surrounding bytes still forward normally. (Detach is
            // checked first above and ends the session, so a detach in the same read
            // wins — a rare edge, and the chord byte then rides along harmlessly.)
            if raw[i] == DELIVER_NOW_KEY {
                Self::flush_filtered(&mut actions, &mut filtered);
                actions.push(FilterAction::DeliverNow);
                i += 1;
                continue;
            }
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
/// and forwards input to the server. `session_name` names the attached session
/// for the M2 deliver-now frame (the attach path uses it as advisory metadata; the
/// server resolves the target from the attachment itself).
pub(crate) async fn run_stdin_to_socket(sw: SocketWriter, session_name: String) -> anyhow::Result<()> {
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
                        FilterAction::DeliverNow => {
                            // attended-UX M2: fire the attached session's held send
                            // now (M1's control). `send_id: None` = the earliest
                            // held send ("send my message now").
                            if let Ok(msg) = protocol::encode(&ClientMsg::DeliverNow {
                                name: session_name.clone(),
                                send_id: None,
                            }) {
                                let mut w = sw.lock().await;
                                w.write_all(&msg).await?;
                            }
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
    logical_history_stream: bool,
) -> anyhow::Result<()> {
    let mut frames = FrameReader::with_leftover(leftover);
    let mut stdout = BufWriter::new(io::stdout());
    let mut logical_chunks = Vec::new();
    let mut awaiting_stream_completion = logical_history_stream;

    fn dispatch_frame(
        msg: ServerMsg,
        stdout: &mut impl Write,
        awaiting_stream_completion: &mut bool,
        logical_chunks: &mut Vec<LogicalHistoryChunk>,
    ) -> anyhow::Result<DispatchResult> {
        if *awaiting_stream_completion {
            match msg {
                ServerMsg::HistoryLogical(mut chunks) => {
                    if chunks.is_empty() {
                        *awaiting_stream_completion = false;
                        let complete = ServerMsg::HistoryLogical(std::mem::take(logical_chunks));
                        return Ok(dispatch_server_msg(&complete, stdout)?);
                    }
                    logical_chunks.append(&mut chunks);
                    return Ok(DispatchResult::Continue);
                }
                other => {
                    anyhow::bail!(
                        "logical history stream ended without its completion frame before {:?}",
                        std::mem::discriminant(&other)
                    );
                }
            }
        }
        Ok(dispatch_server_msg(&msg, stdout)?)
    }

    // Process any complete frames already in the leftover buffer
    while let Some(msg) = frames.decode_next::<ServerMsg>()? {
        if matches!(
            dispatch_frame(
                msg,
                &mut stdout,
                &mut awaiting_stream_completion,
                &mut logical_chunks,
            )?,
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
                dispatch_frame(
                    msg,
                    &mut stdout,
                    &mut awaiting_stream_completion,
                    &mut logical_chunks,
                )?,
                DispatchResult::Done
            ) {
                return Ok(());
            }
        }
        stdout.flush()?;
    }
    if awaiting_stream_completion {
        anyhow::bail!("server closed before completing logical history stream");
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
    fn dispatch_history_logical_reassembles_before_rendering() {
        fn cell(ch: char) -> LogicalCell {
            LogicalCell {
                ch,
                display_width: 1,
                combining: String::new(),
                style: Style::default(),
                wide_early_padding: false,
            }
        }

        let msg = ServerMsg::HistoryLogical(vec![
            LogicalHistoryChunk {
                cells: "ABCDE".chars().map(cell).collect(),
                end_of_line: false,
            },
            LogicalHistoryChunk {
                cells: "FGHIJ".chars().map(cell).collect(),
                end_of_line: true,
            },
        ]);
        let mut buf = Vec::new();
        let result = dispatch_server_msg(&msg, &mut buf).unwrap();
        assert!(matches!(result, DispatchResult::Continue));
        assert_eq!(buf, b"ABCDEFGHIJ\r\n");
    }

    async fn run_streamed_history_relay(messages: Vec<ServerMsg>) -> anyhow::Result<()> {
        let (client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let (reader, _client_writer) = client.into_split();
        for message in messages {
            server
                .write_all(&protocol::encode(&message).unwrap())
                .await
                .unwrap();
        }
        drop(server);
        run_socket_to_stdout(reader, Vec::new(), true).await
    }

    #[tokio::test]
    async fn streamed_history_eof_before_first_frame_is_named_error() {
        let error = run_streamed_history_relay(Vec::new()).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "server closed before completing logical history stream"
        );
    }

    #[tokio::test]
    async fn streamed_history_non_history_before_marker_is_named_error() {
        let error = run_streamed_history_relay(vec![ServerMsg::ScreenUpdate(b"early".to_vec())])
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("logical history stream ended without its completion frame before"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn streamed_empty_history_completion_marker_succeeds() {
        run_streamed_history_relay(vec![ServerMsg::HistoryLogical(Vec::new())])
            .await
            .unwrap();
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
    fn input_filter_deliver_now_alone() {
        let mut filter = InputFilter::new();
        assert_eq!(filter.process(b"\x1d"), vec![FilterAction::DeliverNow]);
    }

    #[test]
    fn input_filter_deliver_now_mid_stream_splits_surrounding_bytes() {
        // attended-UX M2: the chord fires in stream order; surrounding input still forwards.
        let mut filter = InputFilter::new();
        assert_eq!(
            filter.process(b"ab\x1dcd"),
            vec![
                FilterAction::Forward(b"ab".to_vec()),
                FilterAction::DeliverNow,
                FilterAction::Forward(b"cd".to_vec()),
            ]
        );
    }

    #[test]
    fn input_filter_detach_takes_precedence_over_deliver_now_in_one_read() {
        // Detach ends the session, so it wins if both chords arrive in one buffer.
        // A deliver-now AFTER the detach byte is moot (the session is detaching).
        let mut filter = InputFilter::new();
        let actions = filter.process(b"x\x1c\x1d");
        assert_eq!(
            actions,
            vec![FilterAction::Forward(b"x".to_vec()), FilterAction::Detach]
        );
    }

    #[test]
    fn input_filter_deliver_now_before_detach_is_honored_not_leaked() {
        // red-team r1 N3: a deliver-now chord PRECEDING a detach in one read is
        // emitted as a DeliverNow action (in stream order), never forwarded to the
        // PTY as a literal GS byte.
        let mut filter = InputFilter::new();
        assert_eq!(
            filter.process(b"\x1d\x1c"),
            vec![FilterAction::DeliverNow, FilterAction::Detach]
        );
        // With surrounding text preserved around the chord, before the detach.
        let mut filter2 = InputFilter::new();
        assert_eq!(
            filter2.process(b"ab\x1dcd\x1c"),
            vec![
                FilterAction::Forward(b"ab".to_vec()),
                FilterAction::DeliverNow,
                FilterAction::Forward(b"cd".to_vec()),
                FilterAction::Detach,
            ]
        );
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
