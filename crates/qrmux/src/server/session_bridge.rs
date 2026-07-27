use crate::protocol::{self, ServerMsg};
use crate::screen::{
    LogicalEmissionSurface, LogicalHistoryChunk, RenderCache, Screen, TerminalEmulator,
};
use crate::session::{SessionHandles, SessionManager};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::debug;

use super::session_relay::{client_to_pty, screen_to_client};
use super::session_setup::{setup_session, ConnectRequest};

// Lock ordering (to prevent deadlocks):
//
//   manager (tokio::Mutex)
//     → screen (StdMutex)
//       → master (StdMutex)
//     → pty_writer (StdMutex)  [try_lock in reader loop to avoid deadlock]
//     → dims (StdMutex)
//
// The persistent reader loop (session.rs) uses try_lock on pty_writer
// because client_to_pty may hold it during a blocking write while the
// child process waits for a DA response that the reader needs to deliver.

/// Minimum interval between consecutive screen renders to the client.
/// 16ms ≈ 60fps — fast enough for smooth animation (progress bars, htop)
/// while preventing CPU waste from rendering every PTY read (1000s/sec).
pub(super) const RENDER_THROTTLE: std::time::Duration = std::time::Duration::from_millis(16);

/// Estimated per-line bincode overhead: 8 bytes for Vec length prefix +
/// ~8 bytes for enum variant tag and alignment padding.
const BINCODE_LINE_OVERHEAD: usize = 16;

const INITIAL_SIZE_CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

enum InitialHistory {
    Legacy(Vec<Vec<u8>>),
    Logical(Vec<LogicalHistoryChunk>),
}

impl InitialHistory {
    fn is_empty(&self) -> bool {
        match self {
            Self::Legacy(lines) => lines.is_empty(),
            Self::Logical(chunks) => chunks.is_empty(),
        }
    }
}

/// Prepend passthrough escape sequences to the rendered screen data so they
/// are sent as a single `ScreenUpdate` write.  This avoids the intermediate
/// `flush()` that `Passthrough` messages trigger on the client, which can cause
/// rendering glitches in terminals like Blink (e.g. `\e[3J` clearing the
/// viewport before the new screen content arrives).
pub(super) fn prepend_passthrough(passthrough: Vec<Vec<u8>>, render_data: Vec<u8>) -> Vec<u8> {
    if passthrough.is_empty() {
        return render_data;
    }
    let total: usize = passthrough.iter().map(|c| c.len()).sum::<usize>() + render_data.len();
    let mut combined = Vec::with_capacity(total);
    for chunk in passthrough {
        combined.extend_from_slice(&chunk);
    }
    combined.extend_from_slice(&render_data);
    combined
}

/// Lock a `StdMutex` and convert poisoning into `anyhow::Error`.
pub(super) fn lock_mutex<'a, T>(
    mutex: &'a StdMutex<T>,
    label: &str,
) -> anyhow::Result<std::sync::MutexGuard<'a, T>> {
    mutex
        .lock()
        .map_err(|e| anyhow::anyhow!("{} mutex poisoned: {}", label, e))
}

/// Render the screen and send the update to the client.
pub(super) async fn render_and_send(
    screen: &Arc<StdMutex<Screen>>,
    cache: &mut RenderCache,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    full: bool,
) -> anyhow::Result<()> {
    let update = lock_mutex(screen, "screen")?.render(full, cache);
    let msg = protocol::encode(&ServerMsg::ScreenUpdate(update))?;
    writer.write_all(&msg).await?;
    Ok(())
}

async fn send_connected(
    handles: &SessionHandles,
    is_new_session: bool,
    stream: &mut tokio::net::UnixStream,
) -> anyhow::Result<()> {
    let connected = protocol::encode(&ServerMsg::Connected {
        name: handles.name.clone(),
        new_session: is_new_session,
    })?;
    stream.write_all(&connected).await?;
    Ok(())
}

/// Apply the client's post-Connected size sample before taking the initial
/// snapshot. A disagreement takes the ordinary resize path, after which the
/// initial state is always a full repaint at the confirmed geometry.
async fn confirm_initial_size(
    stream: &mut tokio::net::UnixStream,
    handles: &SessionHandles,
    leftover: Vec<u8>,
) -> anyhow::Result<Vec<u8>> {
    let mut frames = crate::protocol::FrameReader::with_leftover(leftover);
    let confirmation = async {
        loop {
            if let Some(msg) = frames.decode_next::<crate::protocol::ClientMsg>()? {
                match msg {
                    crate::protocol::ClientMsg::ConfirmSize { cols, rows } => {
                        return Ok::<_, anyhow::Error>(Ok((cols, rows)));
                    }
                    other => return Ok(Err(other)),
                }
            }
            if !frames.fill_from(stream).await? {
                anyhow::bail!("client closed before confirming its current terminal size");
            }
        }
    };
    let confirmation = tokio::time::timeout(INITIAL_SIZE_CONFIRM_TIMEOUT, confirmation).await;
    let (cols, rows) = match confirmation {
        Ok(Ok(Ok(confirmed))) => confirmed,
        Ok(Ok(Err(unexpected))) => {
            tracing::warn!(
                session = %handles.name,
                message = ?std::mem::discriminant(&unexpected),
                "expected ConfirmSize after Connected; using Connect geometry"
            );
            // The frame is valid relay input, so preserve it in order ahead of
            // every still-buffered frame instead of dropping the client's first
            // action while degrading the confirmation handshake.
            let mut leftover = protocol::encode(&unexpected)?;
            leftover.extend(frames.into_leftover());
            return Ok(leftover);
        }
        Ok(Err(error)) => {
            tracing::warn!(
                session = %handles.name,
                error = %error,
                "terminal-size confirmation failed; using Connect geometry"
            );
            let leftover = frames.into_leftover();
            // A complete but malformed frame cannot be decoded by the relay
            // either. Discard just that framed message when its boundary is
            // recoverable, retaining any later coalesced frames in order.
            return Ok(match protocol::codec::decode_frame(&leftover) {
                Ok(Some((_data, consumed))) => leftover[consumed..].to_vec(),
                _ => Vec::new(),
            });
        }
        Err(_) => {
            tracing::warn!(
                session = %handles.name,
                timeout_secs = INITIAL_SIZE_CONFIRM_TIMEOUT.as_secs(),
                "terminal-size confirmation timed out; using Connect geometry"
            );
            return Ok(frames.into_leftover());
        }
    };
    let confirmed = crate::screen::sanitize_dimensions(cols, rows);
    let current = *lock_mutex(&handles.dims, "dims")?;
    if confirmed != current {
        super::session_setup::resize_or_sigwinch(
            &handles.master,
            &handles.screen,
            &handles.dims,
            confirmed.cols,
            confirmed.rows,
            current,
            &handles.name,
        )
        .await?;
    }
    Ok(frames.into_leftover())
}

/// Split one transport chunk only when it cannot fit by itself. Cell order is
/// retained and only the final fragment inherits the original terminator.
fn split_logical_chunk_to_fit(
    chunk: LogicalHistoryChunk,
) -> anyhow::Result<Vec<LogicalHistoryChunk>> {
    if protocol::encode(&ServerMsg::HistoryLogical(vec![chunk.clone()])).is_ok() {
        return Ok(vec![chunk]);
    }
    if chunk.cells.is_empty() {
        anyhow::bail!("logical-history chunk exceeds the frame limit");
    }

    let mut fragments = Vec::new();
    let mut start = 0;
    while start < chunk.cells.len() {
        let mut low = 1usize;
        let mut high = chunk.cells.len() - start;
        let mut best = 0usize;
        while low <= high {
            let mid = low + (high - low) / 2;
            let candidate = LogicalHistoryChunk {
                cells: chunk.cells[start..start + mid].to_vec(),
                end_of_line: false,
            };
            if protocol::encode(&ServerMsg::HistoryLogical(vec![candidate])).is_ok() {
                best = mid;
                low = mid + 1;
            } else {
                high = mid.saturating_sub(1);
            }
        }
        if best == 0 {
            anyhow::bail!(
                "one logical-history cell exceeds the {} byte frame limit",
                protocol::codec::MAX_FRAME_SIZE
            );
        }
        let end = start + best;
        fragments.push(LogicalHistoryChunk {
            cells: chunk.cells[start..end].to_vec(),
            end_of_line: end == chunk.cells.len() && chunk.end_of_line,
        });
        start = end;
    }
    Ok(fragments)
}

fn encode_logical_history_frames(chunks: Vec<LogicalHistoryChunk>) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut encoded_frames = Vec::new();
    let mut current = Vec::new();
    for chunk in chunks {
        for fragment in split_logical_chunk_to_fit(chunk)? {
            current.push(fragment);
            if protocol::encode(&ServerMsg::HistoryLogical(current.clone())).is_err() {
                let last = current.pop().expect("just pushed logical fragment");
                if current.is_empty() {
                    anyhow::bail!("logical-history fragment exceeds the frame limit");
                }
                encoded_frames.push(protocol::encode(&ServerMsg::HistoryLogical(
                    std::mem::take(&mut current),
                ))?);
                current.push(last);
            }
        }
    }
    if !current.is_empty() {
        encoded_frames.push(protocol::encode(&ServerMsg::HistoryLogical(current))?);
    }
    Ok(encoded_frames)
}

/// Write logical history with an explicit completion marker when the streaming
/// capability was negotiated. Legacy and Stage-2a responses are unchanged.
pub(super) async fn write_history_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response: ServerMsg,
    logical_stream: bool,
) -> anyhow::Result<()> {
    if logical_stream {
        if let ServerMsg::HistoryLogical(chunks) = response {
            for frame in encode_logical_history_frames(chunks)? {
                writer.write_all(&frame).await?;
            }
            writer
                .write_all(&protocol::encode(&ServerMsg::HistoryLogical(Vec::new()))?)
                .await?;
            return Ok(());
        }
    }
    writer.write_all(&protocol::encode(&response)?).await?;
    Ok(())
}

/// Send scrollback history and initial screen state.
/// Returns the render_cache for subsequent incremental renders.
async fn send_initial_state(
    handles: &SessionHandles,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    logical_history: bool,
    logical_history_stream: bool,
) -> anyhow::Result<RenderCache> {
    let mut render_cache = RenderCache::new();
    let (history, screen_msg) = {
        let mut screen = lock_mutex(&handles.screen, "screen")?;
        // Skip history injection when in alt screen (e.g. htop, vim).
        // The scrollback is from the main screen and not relevant while the
        // alt screen app is running.  Re-injecting it on every reconnect
        // would accumulate duplicate lines in the outer terminal's scrollback.
        // (The render below replays `?1049h` to the client when in alt screen
        // — screen/render.rs alt-screen replay — so the attach lands in the
        // client's ALT buffer, matching the inner app.)
        let history = if logical_history {
            InitialHistory::Logical(
                screen
                    .logical_emission(LogicalEmissionSurface::AttachReplay)
                    .chunks,
            )
        } else {
            InitialHistory::Legacy(if screen.in_alt_screen() {
                Vec::new()
            } else {
                screen.get_history()
            })
        };
        let notifications = screen.take_queued_notifications();
        let mut render_data = Vec::new();
        // Prepend queued notifications so the terminal processes them on reconnect
        for notif in notifications {
            render_data.extend_from_slice(&notif);
        }
        // After the client writes history lines with \r\n, up to `rows - 1`
        // lines remain on the visible screen (the final \r\n already scrolled
        // one line off, leaving the cursor on a blank bottom row).  Prepend
        // newlines to flush them into the real terminal's scrollback buffer
        // before the screen clear erases them.
        if !history.is_empty() {
            // Logical cells are width-agnostic. On the capable path, the
            // width-committed full repaint below is reached only after the
            // post-Connected ConfirmSize has been applied.
            // Position cursor at the bottom row first so that each \n
            // reliably triggers one scroll, regardless of initial cursor position.
            use crate::screen::write_u16;
            render_data.extend_from_slice(b"\x1b[");
            write_u16(&mut render_data, screen.rows());
            render_data.extend_from_slice(b";1H");
            // 1-row terminal: 0 newlines — nothing to flush.
            render_data.extend(std::iter::repeat_n(
                b'\n',
                screen.rows().saturating_sub(1) as usize,
            ));
        }
        render_data.extend_from_slice(&screen.render(true, &mut render_cache));
        let screen_msg = protocol::encode(&ServerMsg::ScreenUpdate(render_data))?;
        (history, screen_msg)
    };

    // B3 R8 site (a): delay AFTER the history/screen snapshot is taken (lock
    // released above) and BEFORE the frames are written. Inert unless
    // QRMUX_TEST_SEAM_DELAY_MS=a:<ms>. See SeamDelay docs in session.rs.
    // Async seam → tokio::time::sleep via maybe_sleep_async (C1 M5 / carry C1b F4).
    crate::session::SeamDelay::maybe_sleep_async(crate::session::SeamDelay::from_env(), b'a').await;

    match history {
        InitialHistory::Legacy(hist_chunks) if !hist_chunks.is_empty() => {
            let mut chunk = Vec::new();
            let mut chunk_size = 0;
            // Leave headroom for bincode framing (length prefix, enum tags)
            let size_limit = protocol::codec::MAX_FRAME_SIZE / 2;

            for line in hist_chunks {
                let line_size = line.len() + BINCODE_LINE_OVERHEAD;
                if chunk_size + line_size > size_limit && !chunk.is_empty() {
                    let msg = protocol::encode(&ServerMsg::History(std::mem::take(&mut chunk)))?;
                    writer.write_all(&msg).await?;
                    chunk_size = 0;
                }
                chunk_size += line_size;
                chunk.push(line);
            }
            if !chunk.is_empty() {
                let msg = protocol::encode(&ServerMsg::History(chunk))?;
                writer.write_all(&msg).await?;
            }
        }
        InitialHistory::Logical(chunks) if !chunks.is_empty() => {
            write_history_response(
                writer,
                ServerMsg::HistoryLogical(chunks),
                logical_history_stream,
            )
            .await?;
        }
        InitialHistory::Logical(_) if logical_history_stream => {
            write_history_response(writer, ServerMsg::HistoryLogical(Vec::new()), true).await?;
        }
        _ => {}
    }
    writer.write_all(&screen_msg).await?;

    // Drain stale pending scrollback so the screen→client loop starts clean.
    {
        let mut screen = lock_mutex(&handles.screen, "screen")?;
        screen.take_pending_scrollback();
        screen.take_passthrough();
    }

    Ok(render_cache)
}

/// Bridge a connected client to a session, relaying screen updates and client input bidirectionally.
pub(super) async fn handle_session(
    mut stream: tokio::net::UnixStream,
    manager: Arc<Mutex<SessionManager>>,
    req: ConnectRequest,
    // §4.1 lost-reply fix [F4]: the per-frame in-flight guard, MOVED in from the
    // dispatch loop. Held across setup_session + send_initial_state (so a Connect
    // whose owed Connected/Error reply races the exit — e.g. its session reaped
    // mid-setup — is covered), then DROPPED before the long-lived relay select.
    // `None` only in the legacy test fixture (no lifecycle wait to gate).
    in_flight_guard: Option<super::client_handler::InFlightGuard>,
) -> anyhow::Result<()> {
    let setup = setup_session(
        &mut stream,
        &manager,
        &req.name,
        req.history,
        req.cols,
        req.rows,
        req.mode,
    )
    .await?;
    // Manager lock dropped — not held during I/O

    // ClientGuard clears has_client on drop (unless evicted).
    // Keep it alive until this function returns.
    let _client_guard = setup.client_guard;

    send_connected(&setup.handles, setup.is_new_session, &mut stream).await?;
    let leftover = if req.initial_size_confirm {
        confirm_initial_size(&mut stream, &setup.handles, req.leftover).await?
    } else {
        req.leftover
    };
    let (reader, mut writer) = stream.into_split();
    let render_cache = send_initial_state(
        &setup.handles,
        &mut writer,
        req.logical_history,
        req.logical_history_stream,
    )
    .await?;

    // §4.1 lost-reply fix [F4]: the initial-reply flush is done — drop the
    // in-flight guard HERE, before the long-lived relay select. The relay phase
    // is deliberately EXCLUDED from the exit-wait: its end-of-life signal is
    // SessionEnded/EOF (the B3 content-first/close-last replay discipline +
    // drain_all at shutdown), not a single owed reply, and guarding it would let
    // a detached-but-connected client hold the daemon open past its session's
    // death (the immortal-daemon problem the backstop exists to kill).
    drop(in_flight_guard);

    let refresh_notify = Arc::new(tokio::sync::Notify::new());

    // B3 R8 site (b): delay AFTER the replay ScreenUpdate has been written (in
    // send_initial_state, returned above) and BEFORE the screen_notify wakeup
    // (`setup.handles.screen_notify.notify_one()` just below) arms the live
    // screen_to_client relay. Widens the suspected loss window: PTY output
    // processed here should ride the first live render; if a notify/ordering flaw
    // existed it could be lost from BOTH replay and the next render. Inert unless
    // QRMUX_TEST_SEAM_DELAY_MS=b:<ms>. Async seam → tokio::time::sleep via
    // maybe_sleep_async (C1 M5 / carry C1b F4).
    crate::session::SeamDelay::maybe_sleep_async(crate::session::SeamDelay::from_env(), b'b').await;

    // Ensure the screen_to_client relay doesn't miss notifications that fired
    // between send_initial_state draining pending data and the first notified()
    // poll.  A spurious wakeup is harmless — it just triggers a no-op render.
    setup.handles.screen_notify.notify_one();

    let mut screen_to_client_task = tokio::spawn(screen_to_client(
        setup.handles.clone(),
        render_cache,
        refresh_notify.clone(),
        setup.evict_rx,
        writer,
    ));

    let mut client_to_pty_task = tokio::spawn(client_to_pty(
        setup.handles,
        reader,
        refresh_notify,
        leftover,
    ));

    tokio::select! {
        r = &mut screen_to_client_task => {
            debug!("screen_to_client finished: {:?}", r.as_ref().map(|r| r.as_ref().map(|_| "ok")));
            client_to_pty_task.abort();
            r??;
        }
        r = &mut client_to_pty_task => {
            debug!("client_to_pty finished: {:?}", r.as_ref().map(|r| r.as_ref().map(|_| "ok")));
            screen_to_client_task.abort();
            r??;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FrameReader;
    use crate::screen::{RenderCache, Screen, TerminalEmulator};

    #[test]
    fn prepend_passthrough_empty() {
        let render = b"render-data".to_vec();
        let result = prepend_passthrough(vec![], render.clone());
        assert_eq!(result, render);
    }

    #[test]
    fn prepend_passthrough_single() {
        let pt = vec![b"\x1b[3J".to_vec()];
        let render = b"\x1b[?2026hcontent\x1b[?2026l".to_vec();
        let result = prepend_passthrough(pt, render);
        assert_eq!(&result[..4], b"\x1b[3J");
        assert_eq!(&result[4..], b"\x1b[?2026hcontent\x1b[?2026l");
    }

    #[test]
    fn prepend_passthrough_multiple() {
        let pt = vec![vec![0x07], b"\x1b[3J".to_vec()];
        let render = b"screen".to_vec();
        let result = prepend_passthrough(pt, render);
        assert_eq!(result, b"\x07\x1b[3Jscreen");
    }

    /// ED mode 3 passthrough is prepended to the render buffer,
    /// ensuring the terminal processes clear + redraw atomically.
    #[test]
    fn ed3_included_in_screen_update() {
        let mut screen = Screen::new(80, 24, 100);
        screen.process(b"hello world");
        screen.process(b"\x1b[3J");

        let passthrough = screen.take_passthrough();
        assert_eq!(passthrough.len(), 1);
        assert_eq!(passthrough[0], b"\x1b[3J");

        let mut cache = RenderCache::new();
        let render_data = screen.render(true, &mut cache);

        let combined = prepend_passthrough(passthrough, render_data.clone());
        assert!(
            combined.starts_with(b"\x1b[3J"),
            "passthrough should prefix screen data"
        );
        assert_eq!(&combined[4..], &render_data[..]);
    }

    #[tokio::test]
    async fn initial_state_capability_gate_preserves_message_order() {
        async fn capture(logical_history: bool, name: &str) -> Vec<ServerMsg> {
            let mut manager = SessionManager::new();
            manager
                .create_detached(
                    name.into(),
                    5,
                    2,
                    100,
                    crate::pty::CommandSpec::login_shell_c("sleep 60"),
                    std::env::current_dir().unwrap(),
                )
                .unwrap();
            let screen = manager.get(name).unwrap().screen.clone();
            {
                let mut screen = screen.lock().unwrap();
                screen.process(b"\x1bcABCDEFGHIJ\r\nK");
            }
            let (guard, handles, _evict_rx) = manager.get_mut(name).unwrap().connect();
            let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
            let (_reader, mut writer) = server.into_split();
            writer
                .write_all(
                    &protocol::encode(&ServerMsg::Connected {
                        name: handles.name.clone(),
                        new_session: false,
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
            send_initial_state(&handles, &mut writer, logical_history, false)
                .await
                .unwrap();
            drop(writer);

            let mut frames = FrameReader::new();
            let mut messages = Vec::new();
            while frames.fill_from(&mut client).await.unwrap() {
                while let Some(msg) = frames.decode_next::<ServerMsg>().unwrap() {
                    messages.push(msg);
                }
            }

            drop(guard);
            let session = manager.remove(name).unwrap();
            crate::server::drop_blocking_with_timeout(session, "test cleanup").await;
            messages
        }

        let logical = capture(true, "initial-logical").await;
        assert!(matches!(logical[0], ServerMsg::Connected { .. }));
        assert!(matches!(logical[1], ServerMsg::HistoryLogical(_)));
        assert!(matches!(logical[2], ServerMsg::ScreenUpdate(_)));
        match &logical[1] {
            ServerMsg::HistoryLogical(chunks) => {
                let rendered = crate::client::render_logical_history(chunks);
                assert!(rendered.iter().any(|(line, _)| line == b"ABCDEFGHIJ"));
            }
            _ => unreachable!(),
        }

        let legacy = capture(false, "initial-legacy").await;
        assert!(matches!(legacy[0], ServerMsg::Connected { .. }));
        assert!(matches!(legacy[1], ServerMsg::History(_)));
        assert!(matches!(legacy[2], ServerMsg::ScreenUpdate(_)));
    }
}
