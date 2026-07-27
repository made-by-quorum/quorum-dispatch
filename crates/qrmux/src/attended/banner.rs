//! attended/banner.rs — M2 presentation state for the polite-delivery status line.
//!
//! This is **presentation only**. The timer task (the SOLE WRITER, driver.rs) publishes a
//! [`BannerSnapshot`] into a `watch` channel each time its already-computed phase/deadline changes
//! (and on a terminal outcome); the mux status-line surface (the `screen`/`server` render path) READS
//! it, lock-free / bounded-staleness, and renders it as an overlay row. No timer/fire/journal behavior
//! lives here — the banner reflects M1's state, it never drives it.
//!
//! # What the human sees (P5)
//! - a **countdown** while a draft is protected (`⏳ … {secs}s … ^] send now`),
//! - a transient **delivered** toast on a `message-seen` terminal (`✓ delivered`),
//! - a **recovery** notice on any failure terminal — the human-facing half of P4: the fire re-shows
//!   the preserved draft into the composer, and this toast explains *why* it came back.

/// The live banner state the timer task publishes and the render path reads. `Default` is idle
/// (nothing to show) — the initial `watch` value and the steady state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BannerSnapshot {
    /// A draft is protected: the earliest held send's countdown. `None` when nothing is held.
    pub countdown: Option<CountdownView>,
    /// The last terminal outcome, shown transiently. `None` when there is nothing recent.
    pub toast: Option<ToastView>,
}

/// The countdown the banner displays: the (earliest) live next-deadline + whether it is a priority
/// send. Read from the scheduler's already-computed decision; keystroke-reset moves `deadline_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CountdownView {
    pub deadline_ms: i64,
    pub priority: bool,
}

/// A transient toast for the most recent terminal outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastView {
    pub kind: ToastKind,
    /// Epoch ms the toast was raised (its display window is measured from here).
    pub shown_at_ms: i64,
}

/// Delivered (a `message-seen` terminal) vs a failure (every other terminal kind — the recovery
/// notice). `reason` is the shared vocabulary's terminal event-name (stranger-test wording), never a
/// locally-minted string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Delivered,
    Failed { reason: String },
}

/// Display window for the delivered toast (ms). Taste-flagged (gate item 6).
pub const DELIVERED_TOAST_MS: i64 = 2_500;
/// Display window for the failure/recovery notice (ms) — longer, and it also clears on the human's
/// next keystroke (they are now editing the restored draft). Taste-flagged.
pub const FAILED_TOAST_MS: i64 = 10_000;

impl ToastKind {
    fn window_ms(&self) -> i64 {
        match self {
            ToastKind::Delivered => DELIVERED_TOAST_MS,
            ToastKind::Failed { .. } => FAILED_TOAST_MS,
        }
    }
}

impl BannerSnapshot {
    /// Is there anything worth repainting right now (a live countdown or an unexpired toast)? The
    /// relay uses this to decide whether a periodic tick should repaint (countdown decrement / toast
    /// expiry) — when `false`, the tick does nothing, so an idle session never churns.
    pub fn is_active(&self, now_ms: i64) -> bool {
        self.countdown.is_some()
            || self
                .toast
                .as_ref()
                .is_some_and(|t| now_ms - t.shown_at_ms < t.kind.window_ms())
    }
}

/// The banner ROW content to overlay for this snapshot at `now_ms`, fit to `cols`, or `None` when
/// there is nothing to show (idle / an expired toast). Precedence: an active countdown wins over a
/// toast. Pure — the render layer supplies now/cols and draws the returned string.
pub fn banner_text(snap: &BannerSnapshot, now_ms: i64, cols: u16) -> Option<String> {
    if let Some(cd) = &snap.countdown {
        let remaining_ms = (cd.deadline_ms - now_ms).max(0);
        // Ceil to whole seconds, floored at 1: a countdown is only ever shown while the send is
        // still held, so it must never read "0s" — a tick that lands after the deadline crossed but
        // before the driver republishes (sub-second) would otherwise flash "0s" (red-team r1 N2).
        let secs = ((remaining_ms + 999) / 1000).max(1);
        let tag = if cd.priority { " (priority)" } else { "" };
        return Some(fit(
            format!("⏳ holding your message{tag} · {secs}s · ^] send now"),
            cols,
        ));
    }
    if let Some(t) = &snap.toast {
        if now_ms - t.shown_at_ms < t.kind.window_ms() {
            let s = match &t.kind {
                ToastKind::Delivered => "✓ delivered".to_string(),
                ToastKind::Failed { reason } => {
                    format!("⚠ couldn't deliver ({reason}) — your message was restored")
                }
            };
            return Some(fit(s, cols));
        }
    }
    None
}

/// Truncate to at most `cols` DISPLAY CELLS (the overlay is exactly one row). Width-based, not
/// char-based: a wide glyph (e.g. ⏳ = 2 cells) must count as its true cell width, or a char-count
/// truncation could still overflow the physical row and wrap to row 2 on a narrow autowrap terminal
/// (a phone in portrait — the actual attended target), corrupting real row-1 content (red-team r1
/// F2). Stops before the first char that would push the total past `cols`. `cols == 0` ⇒ empty.
fn fit(mut s: String, cols: u16) -> String {
    use unicode_width::UnicodeWidthChar;
    let max = cols as usize;
    if max == 0 {
        return String::new();
    }
    let mut width = 0usize;
    let mut end = s.len();
    for (i, ch) in s.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max {
            end = i;
            break;
        }
        width += w;
    }
    s.truncate(end);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_snapshot_shows_nothing() {
        let snap = BannerSnapshot::default();
        assert_eq!(banner_text(&snap, 0, 80), None);
        assert!(!snap.is_active(0));
    }

    #[test]
    fn countdown_shows_ceil_seconds_and_deliver_now_hint() {
        let snap = BannerSnapshot {
            countdown: Some(CountdownView {
                deadline_ms: 5_400,
                priority: false,
            }),
            toast: None,
        };
        // 5400 - 1000 = 4400ms remaining → ceil = 5s.
        let text = banner_text(&snap, 1_000, 80).unwrap();
        assert!(text.contains("5s"), "got: {text}");
        assert!(text.contains("send now"), "deliver-now hint present: {text}");
        assert!(snap.is_active(1_000));
    }

    #[test]
    fn countdown_never_reads_zero_while_still_held() {
        // 200ms left → ceil = 1s (never "0s" while a positive remainder is held).
        let snap = BannerSnapshot {
            countdown: Some(CountdownView {
                deadline_ms: 1_200,
                priority: false,
            }),
            toast: None,
        };
        assert!(banner_text(&snap, 1_000, 80).unwrap().contains("1s"));
        // N2: at/after the deadline crossing (stale snapshot, sub-second before the
        // driver fires+republishes) the count must NOT flash "0s" — floored at 1s.
        let text_at_zero = banner_text(&snap, 1_200, 80).unwrap();
        assert!(text_at_zero.contains("1s"), "boundary shows 1s not 0s: {text_at_zero}");
        assert!(!text_at_zero.contains("0s"), "never 0s while held: {text_at_zero}");
        let text_past = banner_text(&snap, 5_000, 80).unwrap();
        assert!(!text_past.contains("0s") && text_past.contains("1s"));
    }

    #[test]
    fn countdown_takes_precedence_over_a_toast() {
        let snap = BannerSnapshot {
            countdown: Some(CountdownView {
                deadline_ms: 4_000,
                priority: false,
            }),
            toast: Some(ToastView {
                kind: ToastKind::Delivered,
                shown_at_ms: 0,
            }),
        };
        assert!(banner_text(&snap, 0, 80).unwrap().contains("holding"));
    }

    #[test]
    fn delivered_toast_shows_then_expires() {
        let snap = BannerSnapshot {
            countdown: None,
            toast: Some(ToastView {
                kind: ToastKind::Delivered,
                shown_at_ms: 0,
            }),
        };
        assert_eq!(banner_text(&snap, 100, 80).as_deref(), Some("✓ delivered"));
        assert!(snap.is_active(100));
        // After the window: gone.
        assert_eq!(banner_text(&snap, DELIVERED_TOAST_MS + 1, 80), None);
        assert!(!snap.is_active(DELIVERED_TOAST_MS + 1));
    }

    #[test]
    fn failed_toast_is_the_recovery_notice_and_lingers_longer() {
        let snap = BannerSnapshot {
            countdown: None,
            toast: Some(ToastView {
                kind: ToastKind::Failed {
                    reason: "recipient-gone".into(),
                },
                shown_at_ms: 0,
            }),
        };
        let text = banner_text(&snap, 100, 120).unwrap();
        assert!(text.contains("couldn't deliver"), "got: {text}");
        assert!(text.contains("recipient-gone"), "got: {text}");
        assert!(text.contains("restored"), "recovery wording: {text}");
        // Still visible past the delivered window (recovery lingers).
        assert!(snap.is_active(DELIVERED_TOAST_MS + 1));
        assert!(banner_text(&snap, DELIVERED_TOAST_MS + 1, 120).is_some());
        // Gone after its own longer window.
        assert_eq!(banner_text(&snap, FAILED_TOAST_MS + 1, 120), None);
    }

    #[test]
    fn fit_truncates_by_display_width_not_char_count() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(fit("hello world".into(), 5), "hello");
        assert_eq!(fit("hi".into(), 80), "hi");
        assert_eq!(fit("anything".into(), 0), "");
        // A wide glyph is 2 cells: fitting "⏳⏳⏳⏳" (8 cells) to cols=2 yields ONE
        // glyph (2 cells), NOT two (which would be 4 cells and overflow) — the F2 fix.
        let s = fit("⏳⏳⏳⏳".into(), 2);
        assert_eq!(s.chars().count(), 1, "one wide glyph fills cols=2");
        assert!(s.width() <= 2, "display width must not exceed cols");
        // A wide glyph that would straddle the boundary is dropped whole (no half-cell).
        assert_eq!(fit("a⏳".into(), 2), "a", "no room for the 2-cell glyph after 'a'");
    }

    #[test]
    fn f2_wide_glyph_countdown_never_exceeds_a_narrow_row() {
        // red-team r1 F2: the real countdown string on a narrow (phone-portrait) row.
        // The composed text's DISPLAY WIDTH must be <= cols so it never wraps to row 2.
        use unicode_width::UnicodeWidthStr;
        let snap = BannerSnapshot {
            countdown: Some(CountdownView { deadline_ms: 5_000, priority: false }),
            toast: None,
        };
        for cols in [10u16, 20, 24, 35, 40] {
            let text = banner_text(&snap, 0, cols).unwrap();
            assert!(
                text.width() <= cols as usize,
                "cols={cols}: display width {} > cols (would wrap): {text:?}",
                text.width()
            );
        }
    }
}
