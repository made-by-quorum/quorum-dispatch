//! Client relay loops: screen-to-client rendering and client-to-PTY input forwarding.

use crate::protocol::{self, ClientMsg, FrameReader, ServerMsg};
use crate::session::SessionHandles;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::debug;

use super::session_bridge::{lock_mutex, prepend_passthrough, render_and_send, RENDER_THROTTLE};
use super::session_setup::resize_pty;

/// attended-UX M2: render a frame and overlay the polite-delivery banner (when
/// this session is attended), then send it. `full` forces a full redraw (the
/// refresh path); otherwise the incremental `take_and_render` path (scrollback +
/// passthrough). A frame that renders nothing AND composes no banner is skipped
/// (no-op preserved — a non-attended or idle-attended session is byte-identical to
/// the pre-M2 behavior). The banner is appended LAST, so it never enters native
/// scrollback and yields under alt-screen (see `screen::compose_banner`).
async fn render_compose_send(
    screen: &crate::session::SharedScreen,
    render_cache: &mut crate::screen::RenderCache,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    banner_rx: &Option<tokio::sync::watch::Receiver<crate::attended::banner::BannerSnapshot>>,
    full: bool,
) -> anyhow::Result<()> {
    let update = {
        let mut screen = lock_mutex(screen, "screen")?;
        let mut update = if full {
            screen.render(true, render_cache)
        } else {
            let (render_data, passthrough) = screen.take_and_render(render_cache);
            prepend_passthrough(passthrough, render_data)
        };
        if let Some(rx) = banner_rx {
            let cols = screen.cols();
            let now = {
                use crate::attended::Clock as _;
                crate::attended::SystemClock.now_ms()
            };
            let text = crate::attended::banner::banner_text(&rx.borrow(), now, cols);
            screen.compose_banner(&mut update, render_cache, text.as_deref());
        }
        update
    };
    if !update.is_empty() {
        let msg = protocol::encode(&ServerMsg::ScreenUpdate(update))?;
        writer.write_all(&msg).await?;
    }
    Ok(())
}

/// attended-UX M2 (red-team r1 F1): should the periodic banner tick render this
/// turn? Yes while a banner is live (`active` — countdown decrement / unexpired
/// toast), AND yes for exactly one more turn while a banner is still painted but
/// has gone inactive (`has_banner`) — that final render calls `compose_banner(None)`
/// and ERASES the expired toast in an otherwise-quiet session. When neither holds
/// (idle, nothing painted) the tick is a no-op, so an idle session never churns.
fn tick_should_repaint(active: bool, has_banner: bool) -> bool {
    active || has_banner
}

/// Wait for the next M2 banner-state change, or (no attended state) never resolve.
/// The SOLE place that mutably borrows `banner_rx` — every value read happens in a
/// select handler after this future is dropped.
async fn recv_banner_change(
    banner_rx: &mut Option<tokio::sync::watch::Receiver<crate::attended::banner::BannerSnapshot>>,
) {
    match banner_rx {
        Some(rx) => {
            let _ = rx.changed().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Screen -> client relay loop: waits for the persistent reader to signal new
/// data, then renders and sends updates to the client.
pub(super) async fn screen_to_client(
    h: SessionHandles,
    mut render_cache: crate::screen::RenderCache,
    refresh_notify: Arc<tokio::sync::Notify>,
    mut evict_rx: tokio::sync::watch::Receiver<bool>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
) -> anyhow::Result<()> {
    use std::pin::pin;
    use std::time::Duration;
    use tokio::time::Instant;

    // If the reader is already dead (child exited before we connected),
    // send final state and SessionEnded immediately.
    if !h.reader_alive.load(Ordering::Acquire) {
        render_and_send(&h.screen, &mut render_cache, &mut writer, true).await?;
        let msg = protocol::encode(&ServerMsg::SessionEnded)?;
        writer.write_all(&msg).await?;
        return Ok(());
    }

    // attended-UX M2: the banner state to overlay (None for a non-attended
    // session — the banner logic is then wholly inert). The 1s tick drives the
    // countdown decrement / toast expiry; `banner_rx.changed()` drives prompt
    // repaints on any state move (accept / keystroke-reset / fire / toast).
    let mut banner_rx = h.attended.as_ref().map(|a| a.banner_rx());
    let banner_present = banner_rx.is_some();
    let mut banner_tick = tokio::time::interval(Duration::from_secs(1));
    banner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut throttle_sleep = pin!(tokio::time::sleep(Duration::ZERO));
    let mut pending_render = false;

    loop {
        tokio::select! {
            _ = h.screen_notify.notified() => {
                if !h.reader_alive.load(Ordering::Acquire) {
                    // Reader exited (PTY EOF). Do a final render + send SessionEnded.
                    let (render_data, passthrough) = lock_mutex(&h.screen, "screen")?
                        .take_and_render(&mut render_cache);
                    let update = prepend_passthrough(passthrough, render_data);
                    let msg = protocol::encode(&ServerMsg::ScreenUpdate(update))?;
                    writer.write_all(&msg).await?;
                    let msg = protocol::encode(&ServerMsg::SessionEnded)?;
                    writer.write_all(&msg).await?;
                    break;
                }
                pending_render = true;
                throttle_sleep.as_mut().reset(Instant::now() + RENDER_THROTTLE);
            }
            _ = &mut throttle_sleep, if pending_render => {
                // Prepend passthrough sequences (e.g. \e[3J) so the terminal
                // processes them in a single write, then overlay the banner. Skip
                // sending empty updates (no-op frame) to avoid flicker on
                // terminals without DEC 2026 support (e.g. xterm.js).
                render_compose_send(&h.screen, &mut render_cache, &mut writer, &banner_rx, false).await?;
                pending_render = false;
            }
            _ = refresh_notify.notified() => {
                render_compose_send(&h.screen, &mut render_cache, &mut writer, &banner_rx, true).await?;
            }
            _ = recv_banner_change(&mut banner_rx) => {
                // A banner state change (accept / keystroke-reset / fire / toast):
                // repaint promptly so the countdown is never stale.
                render_compose_send(&h.screen, &mut render_cache, &mut writer, &banner_rx, false).await?;
            }
            _ = banner_tick.tick(), if banner_present => {
                // Periodic repaint while something is live (countdown decrement /
                // toast expiry). The active→inactive transition ALSO renders once
                // (`has_banner`): the tick that finds a just-expired toast is the
                // one frame that must call `compose_banner(None)` to ERASE it in an
                // otherwise-quiet session (red-team r1 F1). Once erased, `has_banner`
                // is false and an idle session never churns again.
                let active = banner_rx.as_ref().is_some_and(|rx| {
                    use crate::attended::Clock as _;
                    rx.borrow().is_active(crate::attended::SystemClock.now_ms())
                });
                if tick_should_repaint(active, render_cache.has_banner()) {
                    render_compose_send(&h.screen, &mut render_cache, &mut writer, &banner_rx, false).await?;
                }
            }
            result = evict_rx.changed() => {
                match result {
                    Ok(()) => {
                        debug!(session = %h.name, "client evicted by new connection");
                        let msg = protocol::encode(&ServerMsg::Error("evicted by new client".into()))?;
                        if let Err(e) = writer.write_all(&msg).await {
                            debug!(session = %h.name, error = %e, "failed to send eviction notice to client");
                        }
                    }
                    Err(_) => {
                        // Sender dropped — session was killed via KillSession
                        debug!(session = %h.name, "session killed while client connected");
                        let msg = protocol::encode(&ServerMsg::SessionEnded)?;
                        if let Err(e) = writer.write_all(&msg).await {
                            debug!(session = %h.name, error = %e, "failed to send session-ended to killed client");
                        }
                    }
                }
                break;
            }
        }
    }
    // has_client cleanup is handled by ClientGuard in handle_session
    Ok(())
}

/// Client -> PTY relay loop: reads client messages and dispatches them.
pub(super) async fn client_to_pty(
    h: SessionHandles,
    mut sock_reader: tokio::net::unix::OwnedReadHalf,
    refresh_notify: Arc<tokio::sync::Notify>,
    leftover: Vec<u8>,
) -> anyhow::Result<()> {
    let mut frames = FrameReader::with_leftover(leftover);

    loop {
        // Drain every complete frame already buffered by the handshake before
        // waiting for more socket input. In particular, ConfirmSize and the
        // first relay message may have arrived in one write.
        while let Some(msg) = frames.decode_next::<ClientMsg>()? {
            match msg {
                ClientMsg::Input(input) => {
                    // attended-UX M1: attach-scoped HUMAN input. Feed it to the
                    // journal (the authoritative draft) + route through the input
                    // lock + wake the timer (keystroke-reset). Injected/replayed
                    // bytes take the SendInput / fire path and never reach this arm,
                    // so they never enter the journal.
                    match &h.attended {
                        Some(att) => {
                            // attended-UX M5 (adv-r1 F1, QS-1): journal + admit + the
                            // passthrough PTY write happen ATOMICALLY under the input
                            // lock, on a blocking thread. Holding the input lock
                            // across the passthrough write makes it happen-before any
                            // fire's arm+clear (`fire`'s `lock_and_snapshot` takes the
                            // same lock), so a `Passthrough` keystroke is GUARANTEED
                            // written pre-clear — never duplicated / entering the PTY
                            // mid-fire. A `Buffered` admit (a fire is in progress)
                            // writes NOTHING now; the bytes flush in order on unlock.
                            // Was: `on_human_input` returned an `Admit` and the relay
                            // wrote the `Passthrough` bytes LATER on this separate,
                            // unsequenced `spawn_blocking` — the race the fix closes.
                            let att = att.clone();
                            let pw = h.pty_writer.clone();
                            let now = {
                                use crate::attended::Clock as _;
                                crate::attended::SystemClock.now_ms()
                            };
                            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                                att.on_human_input_passthrough(&input, now, &pw)?;
                                Ok(())
                            })
                            .await??;
                        }
                        None => {
                            // Non-attended: no journal/lock — write straight to the
                            // PTY as today.
                            let pw = h.pty_writer.clone();
                            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                                let mut w = lock_mutex(&pw, "pty_writer")?;
                                w.write_all(&input)?;
                                w.flush()?;
                                Ok(())
                            })
                            .await??;
                        }
                    }
                }
                ClientMsg::Resize { cols, rows } => {
                    let master_clone = h.master.clone();
                    let screen_clone = h.screen.clone();
                    let dims_clone = h.dims.clone();
                    let name_clone = h.name.clone();
                    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                        resize_pty(&master_clone, &screen_clone, cols, rows)?;
                        match dims_clone.lock() {
                            Ok(mut d) => *d = crate::screen::sanitize_dimensions(cols, rows),
                            Err(e) => tracing::warn!(session = %name_clone, error = %e, "dims mutex poisoned during client resize"),
                        }
                        Ok(())
                    }).await??;
                }
                ClientMsg::RefreshScreen => {
                    refresh_notify.notify_one();
                }
                ClientMsg::DeliverNow { send_id, .. } => {
                    // attended-UX M2: the deliver-now chord on the ATTACH path. The
                    // attachment IS the session, so use `h.attended` directly (the
                    // message `name` is advisory here) and fire M1's existing
                    // control — no countdown reset, no journal entry. A non-attended
                    // session ignores it. Binds M1's `ClientMsg::DeliverNow`; there
                    // is no second delivery path.
                    if let Some(att) = &h.attended {
                        att.deliver_now(send_id);
                    } else {
                        tracing::debug!(session = %h.name, "deliver-now on a non-attended session — ignored");
                    }
                }
                ClientMsg::Detach => {
                    debug!(session = %h.name, "client detached");
                    return Ok(());
                }
                // These are all one-shot / pre-session messages handled in
                // client_handler before the session bridge loop — never valid
                // mid-session, so ignore if one somehow arrives here.
                // Hello is a first-frame-only handshake (v3, §3.2) — never
                // valid mid-session; ignore if one somehow arrives here.
                ClientMsg::Connect { .. }
                | ClientMsg::ListSessions
                | ClientMsg::KillSession { .. }
                | ClientMsg::SendInput { .. }
                | ClientMsg::GetHistory { .. }
                | ClientMsg::CreateDetached { .. }
                | ClientMsg::SubscribeRepublish { .. }
                | ClientMsg::ConfirmSize { .. }
                | ClientMsg::PendingDelivery { .. }
                | ClientMsg::Hello { .. } => {
                    // `PendingDelivery` is handled on the no-attach connection in
                    // client_handler, not on the attach-scoped relay (`DeliverNow`
                    // now HAS an attach arm above — M2). Ignore if one somehow
                    // arrives here.
                    tracing::debug!("ignoring unexpected client message in session relay");
                }
            }
        }
        if !frames.fill_from(&mut sock_reader).await? {
            debug!(session = %h.name, "client socket closed");
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::tick_should_repaint;

    // attended-UX M2 (red-team r1 F1): the tick-gating decision that closes the
    // "expired toast lingers in a quiet session" defect. BEFORE the fix the gate
    // was `active` alone, so the active→inactive transition (a just-expired toast
    // still painted) SKIPPED the render that erases it — this asserts it no longer
    // does.
    #[test]
    fn tick_repaints_to_erase_a_just_expired_toast_then_stops() {
        // Live banner (countdown ticking / toast unexpired): repaint.
        assert!(tick_should_repaint(true, false));
        assert!(tick_should_repaint(true, true));
        // THE F1 CASE: toast window elapsed (inactive) but still painted → repaint
        // ONCE to erase it. (Old gate `active` alone was false here → never erased.)
        assert!(tick_should_repaint(false, true));
        // After the erase (nothing painted, nothing active) → no-op: an idle
        // session never churns (QS-7).
        assert!(!tick_should_repaint(false, false));
    }
}
