//! VTE-based terminal screen emulator with scrollback history.
//! Processes escape sequences and maintains a grid of styled cells.

pub(crate) mod cell;
pub(crate) mod grid;
#[allow(dead_code)]
pub(crate) mod grid_mutator;
pub(crate) mod performer;
pub(crate) mod render;
pub(crate) mod style;
pub mod traits;

use std::collections::VecDeque;
use vte::Parser;

pub use cell::WrapKind;
pub use cell::{Cell, Row};
use grid::Grid;
pub use grid::{sanitize_dimensions, CursorShape, TerminalSize};
pub use grid::{ActiveCharset, Charset, MouseEncoding, MouseModes, TerminalModes};
use performer::ScreenPerformer;
use render::render_screen;
pub use render::AnsiRenderer;
pub use render::RenderCache;
pub use style::write_u16;
pub use style::{Color, Style, StyleId, UnderlineStyle};
pub use traits::{TerminalEmulator, TerminalRenderer};

/// Ordered logical-history domain requested by a Stage-1 emission consumer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogicalEmissionSurface {
    #[default]
    AttachReplay,
    GetHistory,
}

/// Exact untrimmed frozen cell carried by logical history transport planning.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogicalCell {
    pub ch: char,
    pub display_width: u8,
    pub combining: String,
    pub style: Style,
    pub wide_early_padding: bool,
}

/// One ordered physical-row chunk. `end_of_line` is true only when no
/// validated outgoing edge authorizes continuation into the next chunk.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogicalHistoryChunk {
    pub cells: Vec<LogicalCell>,
    pub end_of_line: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogicalTransportEmission {
    pub chunks: Vec<LogicalHistoryChunk>,
}

/// Full cursor state saved by DECSC (ESC 7) / CSI s / mode 1048.
#[derive(Copy, Clone)]
pub(super) struct SavedCursor {
    pub(super) x: u16,
    pub(super) y: u16,
    pub(super) style: Style,
    pub(super) g0_charset: grid::Charset,
    pub(super) g1_charset: grid::Charset,
    pub(super) active_charset: grid::ActiveCharset,
    pub(super) autowrap_mode: bool,
    pub(super) origin_mode: bool,
    /// VT220 "last column flag": deferred autowrap is pending.
    pub(super) wrap_pending: bool,
    pub(super) alt_epoch: Option<u64>,
    pub(super) saved_by_alt_1049: bool,
}

/// Maximum responses/passthrough entries buffered per process() call.
/// 1024 is a safety cap — normal output produces 0-2 responses (DA, DSR).
/// Pathological PTY output (e.g. 1000 DSR queries in one write) is truncated.
const MAX_PENDING: usize = 1024;

/// Notifications (OSC 9/777) queued for replay on reconnect. 50 prevents
/// a disconnected session from accumulating megabytes of stale notifications
/// while still preserving recent ones for the reconnecting client.
const MAX_QUEUED_NOTIFICATIONS: usize = 50;

/// Non-grid state that the performer needs mutable access to.
/// Grouped to reduce borrow count in ScreenPerformer.
pub(super) struct ScreenState {
    pub(super) current_style: Style,
    pub(super) in_alt_screen: bool,
    pub(super) saved_grid: Option<grid::SavedGrid>,
    pub(super) saved_cursor_state: Option<SavedCursor>,
    pub(super) saved_modes: Option<grid::TerminalModes>,
    /// Scroll region saved when entering alt screen; restored on exit.
    pub(super) saved_scroll_region: Option<(u16, u16)>,
    pub(super) pending_responses: Vec<Vec<u8>>,
    pub(super) pending_passthrough: Vec<Vec<u8>>,
    pub(super) queued_notifications: VecDeque<Vec<u8>>,
    pub(super) title: String,
    pub(super) title_stack: Vec<String>,
    pub(super) last_printed_char: char,
    pub(super) next_alt_epoch: Option<u64>,
}

impl ScreenState {
    /// Push a PTY response (DA, DSR) with bounded growth.
    pub fn push_response(&mut self, data: Vec<u8>) {
        if self.pending_responses.len() < MAX_PENDING {
            self.pending_responses.push(data);
        } else {
            tracing::debug!("pending_responses full, dropping response");
        }
    }

    /// Push a passthrough sequence (bell, OSC, etc.) with bounded growth.
    pub fn push_passthrough(&mut self, data: Vec<u8>) {
        if self.pending_passthrough.len() < MAX_PENDING {
            self.pending_passthrough.push(data);
        }
    }

    /// Queue a text notification (OSC 9/777/99) for delivery or replay.
    /// Always enqueues; the consumer (relay or reconnect handler) drains.
    /// Oldest notifications are dropped when the queue is full.
    pub fn push_notification(&mut self, data: Vec<u8>) {
        if self.queued_notifications.len() >= MAX_QUEUED_NOTIFICATIONS {
            self.queued_notifications.pop_front();
        }
        self.queued_notifications.push_back(data);
    }
}

impl Default for ScreenState {
    fn default() -> Self {
        Self {
            current_style: Style::default(),
            in_alt_screen: false,
            saved_grid: None,
            saved_cursor_state: None,
            saved_modes: None,
            saved_scroll_region: None,
            pending_responses: Vec::new(),
            pending_passthrough: Vec::new(),
            queued_notifications: VecDeque::new(),
            title: String::new(),
            title_stack: Vec::new(),
            last_printed_char: ' ',
            next_alt_epoch: Some(0),
        }
    }
}

/// Terminal screen emulator that processes VTE escape sequences into a cell grid.
pub struct Screen {
    pub(super) grid: Grid,
    pub(super) state: ScreenState,
    parser: Parser,
    logical_grid: Grid,
    logical_state: ScreenState,
    logical_parser: Parser,
}

impl Screen {
    /// Create a screen with the given dimensions and scrollback line limit.
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        Self {
            grid: Grid::new(cols, rows, scrollback_limit),
            state: ScreenState::default(),
            parser: Parser::new(),
            logical_grid: Grid::new_logical(cols, rows, scrollback_limit),
            logical_state: ScreenState::default(),
            logical_parser: Parser::new(),
        }
    }

    /// Borrow the underlying grid (read-only).
    #[cfg(test)]
    #[allow(dead_code)] // fork-carried test convenience
    pub(crate) fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Current window title (OSC 0/2).
    #[cfg(test)]
    pub(crate) fn title(&self) -> &str {
        &self.state.title
    }

    /// Current SGR style.
    #[cfg(test)]
    pub(crate) fn current_style(&self) -> style::Style {
        self.state.current_style
    }

    /// Number of visible rows in the grid.
    pub fn rows(&self) -> u16 {
        self.grid.rows()
    }

    /// Number of columns in the grid (inherent — the M2 banner fits its row to
    /// this without needing the `TerminalEmulator` trait in scope at the relay).
    pub fn cols(&self) -> u16 {
        self.grid.cols()
    }

    /// Whether the screen is currently in alternate screen mode.
    pub fn in_alt_screen(&self) -> bool {
        self.state.in_alt_screen
    }

    /// Feed raw bytes through the VTE parser, updating the grid and state.
    pub fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.parser.advance(
                &mut ScreenPerformer {
                    grid: &mut self.grid,
                    state: &mut self.state,
                },
                byte,
            );
            self.logical_parser.advance(
                &mut ScreenPerformer {
                    grid: &mut self.logical_grid,
                    state: &mut self.logical_state,
                },
                byte,
            );
        }
    }

    /// Take pending responses that need to be written back to PTY stdin
    pub fn take_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.state.pending_responses)
    }

    /// Drain and return scrollback lines added since the last call, rendered as ANSI bytes.
    pub fn take_pending_scrollback(&mut self) -> Vec<Vec<u8>> {
        let start = self.grid.pending_start();
        let count = self.grid.pending_scrollback_count();
        self.grid.set_pending_start(self.grid.scrollback_len());
        self.grid
            .scrollback_rows()
            .skip(start)
            .take(count)
            .map(|row| render::render_line(row, self.grid.style_table()))
            .collect()
    }

    /// Return all accumulated scrollback lines as rendered ANSI bytes.
    pub fn get_history(&self) -> Vec<Vec<u8>> {
        self.grid
            .scrollback_rows()
            .map(|row| render::render_line(row, self.grid.style_table()))
            .collect()
    }

    /// Return scrollback lines followed by the CURRENT VISIBLE SCREEN rows, in
    /// order (scrollback first, then visible rows top-to-bottom), all rendered
    /// as ANSI bytes. This is the composition the v2 one-shot `GetHistory` op
    /// returns — a CONTENT-INSPECTION view, not a replay.
    ///
    /// **Trim rule:** trailing visible rows that render to an EMPTY line (no
    /// content — a blank row; `render::render_line` returns `Vec::new()` for
    /// these) are trimmed off the end, so the result ends at the last row that
    /// has content. Styled-but-spaces rows are NOT empty and are kept. Empty
    /// lines *between* content are preserved (only the trailing run is trimmed).
    ///
    /// **Altscreen:** unlike attach-replay (which drops scrollback during alt
    /// screen because re-injecting main-screen scrollback into a fullscreen app
    /// is wrong for a *replay*), GetHistory is content inspection — the boot
    /// answerer must see a dialog even when a fullscreen app is up. So we keep
    /// the same scrollback portion as `get_history` AND always include the
    /// visible (alt) screen rows. See PROTOCOL.md "GetHistory composition".
    pub fn get_content_history(&self) -> Vec<Vec<u8>> {
        let styles = self.grid.style_table();
        let mut lines: Vec<Vec<u8>> = self
            .grid
            .scrollback_rows()
            .map(|row| render::render_line(row, styles))
            .collect();

        let visible: Vec<Vec<u8>> = self
            .grid
            .visible_rows()
            .map(|row| render::render_line(row, styles))
            .collect();

        // Trim trailing all-blank (empty-rendered) visible rows.
        let keep = visible
            .iter()
            .rposition(|l| !l.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        lines.extend(visible.into_iter().take(keep));
        lines
    }

    /// Read-only logical-line view over frozen physical rows. This never
    /// resizes, rewraps, or otherwise materializes storage.
    pub fn logical_emission(&self, surface: LogicalEmissionSurface) -> LogicalTransportEmission {
        self.logical_grid
            .logical_emission(surface, self.logical_state.in_alt_screen)
    }

    /// Unit-test realization of the contract's private direct-row event.
    #[cfg(test)]
    fn logical_insert_row_for_test(&mut self, row: u16) {
        let at = row.saturating_sub(1).min(self.logical_grid.rows() - 1) as usize;
        let bottom = self.logical_grid.visible_row_count() - 1;
        self.logical_grid.remove_visible_row(bottom);
        let blank_style = Style {
            bg: self.logical_state.current_style.bg,
            ..Style::default()
        };
        let style_id = self.logical_grid.style_table_mut().intern(blank_style);
        let blank = Cell::new(' ', style_id, 1);
        let new_row = self.logical_grid.new_blank_row(blank);
        self.logical_grid.insert_visible_row(at, new_row);
    }

    #[cfg(test)]
    fn logical_remove_row_for_test(&mut self, row: u16) {
        let at = row.saturating_sub(1).min(self.logical_grid.rows() - 1) as usize;
        self.logical_grid.remove_visible_row(at);
        let blank_style = Style {
            bg: self.logical_state.current_style.bg,
            ..Style::default()
        };
        let style_id = self.logical_grid.style_table_mut().intern(blank_style);
        let blank = Cell::new(' ', style_id, 1);
        let new_row = self.logical_grid.new_blank_row(blank);
        let bottom = self.logical_grid.visible_row_count();
        self.logical_grid.insert_visible_row(bottom, new_row);
    }

    /// Render the current grid as ANSI output. Pass `full: true` for a full redraw.
    /// Threads `in_alt_screen` to the renderer so the client's terminal is
    /// switched to/from the alternate buffer in step with the inner app
    /// (performer absorbs DEC 1049; renderer re-emits it per client).
    pub fn render(&self, full: bool, cache: &mut RenderCache) -> Vec<u8> {
        render_screen(
            &self.grid,
            &self.state.title,
            full,
            self.state.in_alt_screen,
            cache,
        )
    }

    /// Render the screen with scrollback lines included in one atomic output.
    ///
    /// Scrollback lines are injected into the real terminal's native scrollback
    /// buffer (cursor positioned at the bottom so `\r\n` scrolls), followed by
    /// a full screen redraw.  Everything is inside a single synchronized-output
    /// block to prevent flicker.
    pub fn render_with_scrollback(
        &self,
        scrollback: &[Vec<u8>],
        cache: &mut RenderCache,
    ) -> Vec<u8> {
        render::render_screen_with_scrollback(
            &self.grid,
            &self.state.title,
            scrollback,
            self.state.in_alt_screen,
            cache,
        )
    }

    /// Take pending scrollback, passthrough, notifications, and render in a
    /// single lock hold.  Returns `(render_data, passthrough)`.
    /// Notifications are consumed here so they are delivered exactly once.
    pub fn take_and_render(&mut self, cache: &mut RenderCache) -> (Vec<u8>, Vec<Vec<u8>>) {
        let scrollback_lines = self.take_pending_scrollback();
        let mut passthrough = self.take_passthrough();
        // Drain notifications into passthrough — they are OSC sequences that
        // the terminal should process, delivered exactly once via the relay.
        passthrough.extend(self.state.queued_notifications.drain(..));
        let render_data = if !scrollback_lines.is_empty() {
            self.render_with_scrollback(&scrollback_lines, cache)
        } else {
            self.render(false, cache)
        };
        (render_data, passthrough)
    }

    /// attended-UX M2 — overlay the polite-delivery banner row onto an
    /// already-rendered frame `base` for this client. `banner` is the row text (or
    /// `None` to hide). Presentation only — reads the grid cursor/alt state, never
    /// mutates the screen model. See [`render::compose_banner`] for the HARD
    /// scrolling-clause behavior (scrollback-clean, alt-screen-yielding).
    pub fn compose_banner(&self, base: &mut Vec<u8>, cache: &mut RenderCache, banner: Option<&str>) {
        render::compose_banner(
            &self.grid,
            self.state.in_alt_screen,
            base,
            banner,
            cache,
        );
    }

    /// Look up the resolved style for a visible cell. Test convenience.
    #[cfg(test)]
    pub(crate) fn cell_style(&self, row: usize, col: usize) -> style::Style {
        self.grid
            .style_table()
            .get(self.grid.visible_row(row)[col].style_id)
    }

    /// Character in a visible cell. Test convenience.
    #[cfg(test)]
    pub(crate) fn cell_char(&self, row: usize, col: usize) -> char {
        self.grid.visible_row(row)[col].c
    }

    /// Display width of a visible cell. Test convenience.
    #[cfg(test)]
    #[allow(dead_code)] // fork-carried test convenience
    pub(crate) fn cell_width(&self, row: usize, col: usize) -> u8 {
        self.grid.visible_row(row)[col].width
    }

    /// Compact the style table by scanning all cells for live style IDs
    /// and reclaiming unused slots.
    #[cfg(test)]
    pub fn compact_styles(&mut self) {
        compact_styles(&mut self.grid, self.state.saved_grid.as_ref());
    }

    /// Resize the grid to new dimensions, restoring scrollback lines on vertical expand.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        Self::resize_grid(&mut self.grid, self.state.in_alt_screen, cols, rows);
        Self::resize_grid(
            &mut self.logical_grid,
            self.logical_state.in_alt_screen,
            cols,
            rows,
        );
    }

    fn resize_grid(grid: &mut Grid, in_alt_screen: bool, cols: u16, rows: u16) {
        let old_rows = grid.rows();

        // Restore scrollback lines when growing vertically (not in alt screen).
        // With unified buffer, scrollback rows are already in cells — just move the boundary.
        if !in_alt_screen && rows > old_rows {
            let grow = (rows - old_rows) as usize;
            let restore_count = grow.min(grid.scrollback_len());
            grid.restore_scrollback(restore_count);
            grid.set_cursor_y_unclamped(
                grid.cursor_y()
                    .saturating_add(u16::try_from(restore_count).unwrap_or(u16::MAX)),
            );
        }

        // Shrinking vertically: PRESERVE content (G3 storm fix — war story in
        // grid::push_top_rows_to_scrollback). Chop blank rows below the cursor
        // first; remaining excess pushes top rows into scrollback, mirroring
        // the grow path's restore. Only rows below the cursor that hold
        // content are ever discarded (rare), by grid.resize's fallback pop.
        if !in_alt_screen && rows > 0 && rows < old_rows {
            let mut needed = (old_rows - rows) as usize;
            needed -= grid.chop_blank_bottom_rows(needed);
            let push = needed.min(grid.cursor_y() as usize);
            if push > 0 {
                grid.push_top_rows_to_scrollback(push);
                grid.set_cursor_y_unclamped(grid.cursor_y() - push as u16);
            }
        }

        grid.resize(cols, rows);
    }

    /// Snapshot the visible grid as owned rows of cells, for out-of-crate
    /// integration tests that must inspect cells at the WIDTH/continuation
    /// level (B3 R6 — render-path UTF-8 is always valid by construction so
    /// byte-level checks are vacuous; cell inspection is the only honest
    /// surface). Returns one `Vec<Cell>` per visible row, top to bottom.
    ///
    /// TEST-SUPPORT ONLY (C1 M5 / carry C1b F3). This is not part of qrmux's
    /// intended public surface — it exists so `tests/b3_resize.rs` (an
    /// integration test, hence out-of-crate) can inspect cells at the cell level.
    /// `pub(crate)` would break that target (integration tests cannot see
    /// `pub(crate)`), so the honest minimum is `pub` + `#[doc(hidden)]`: it stays
    /// reachable by the test target but is hidden from rustdoc and flagged here
    /// as not-real-API. (B3 carry C-F3.)
    #[doc(hidden)]
    pub fn visible_cells_snapshot(&self) -> Vec<Vec<Cell>> {
        self.grid
            .visible_rows()
            .map(|row| row.iter().copied().collect())
            .collect()
    }
}

impl traits::TerminalEmulator for Screen {
    fn process(&mut self, bytes: &[u8]) {
        self.process(bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.resize(cols, rows);
    }

    fn cols(&self) -> u16 {
        self.grid.cols()
    }

    fn rows(&self) -> u16 {
        self.grid.rows()
    }

    fn visible_rows(&self) -> Box<dyn Iterator<Item = &cell::Row> + '_> {
        Box::new(self.grid.visible_rows())
    }

    fn scrollback_rows(&self) -> Box<dyn Iterator<Item = &cell::Row> + '_> {
        Box::new(self.grid.scrollback_rows())
    }

    fn scrollback_len(&self) -> usize {
        self.grid.scrollback_len()
    }

    fn cursor_position(&self) -> (u16, u16) {
        self.grid.cursor_pos()
    }

    fn cursor_visible(&self) -> bool {
        self.grid.cursor_visible()
    }

    fn resolve_style(&self, id: style::StyleId) -> style::Style {
        self.grid.style_table().get(id)
    }

    fn in_alt_screen(&self) -> bool {
        self.state.in_alt_screen
    }

    fn take_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.state.pending_responses)
    }

    fn title(&self) -> &str {
        &self.state.title
    }

    fn cursor_shape(&self) -> grid::CursorShape {
        self.grid.modes().cursor_shape
    }

    fn scroll_region(&self) -> (u16, u16) {
        self.grid.scroll_region()
    }

    fn modes(&self) -> &grid::TerminalModes {
        self.grid.modes()
    }

    fn take_passthrough(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.state.pending_passthrough)
    }

    fn take_queued_notifications(&mut self) -> Vec<Vec<u8>> {
        self.state.queued_notifications.drain(..).collect()
    }
}

/// Scan all cells in the grid (scrollback + visible) and saved_grid,
/// then reclaim style table slots not referenced by any cell.
pub(crate) fn compact_styles(grid: &mut Grid, saved_grid: Option<&grid::SavedGrid>) {
    let cap = grid.style_table().capacity();
    if cap <= 1 {
        return;
    }

    let mut live = vec![false; cap];
    live[0] = true; // default style is always live

    for row in grid.scrollback_rows().chain(grid.visible_rows()) {
        for cell in row.iter() {
            let id = cell.style_id.index();
            if id < cap {
                live[id] = true;
            }
        }
    }

    if let Some(saved) = saved_grid {
        for row in saved.visible_rows() {
            for cell in row.iter() {
                let id = cell.style_id.index();
                if id < cap {
                    live[id] = true;
                }
            }
        }
    }

    grid.style_table_mut().reclaim(&live);
}

/// Render the visible grid to plain text (one line per visible row, trailing
/// whitespace trimmed, rows joined by `\n`). PRODUCTION render for the attended
/// fire's plain-composer verify + the content-verified-CR read-back
/// (`attended::fire`). The `#[cfg(test)]` `test_helpers::screen_lines` is the same
/// logic; this is its production twin (the attended path needs plain text, not the
/// ANSI `render_screen` bytes the client relay emits).
pub fn screen_text(screen: &Screen) -> String {
    screen
        .grid
        .visible_rows()
        .map(|row| {
            let s: String = row.iter().map(|c| c.c).collect();
            s.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests_traits {
    use super::traits::TerminalEmulator;
    use super::*;

    #[test]
    fn screen_implements_terminal_emulator() {
        let mut screen = Screen::new(80, 24, 100);

        // Test process + visible_rows
        TerminalEmulator::process(&mut screen, b"Hello");
        let rows: Vec<&cell::Row> = TerminalEmulator::visible_rows(&screen).collect();
        assert_eq!(rows.len(), 24);
        assert_eq!(rows[0][0].c, 'H');
        assert_eq!(rows[0][4].c, 'o');

        // Test dimensions
        assert_eq!(TerminalEmulator::cols(&screen), 80);
        assert_eq!(TerminalEmulator::rows(&screen), 24);

        // Test cursor
        assert_eq!(TerminalEmulator::cursor_position(&screen), (5, 0));
        assert!(TerminalEmulator::cursor_visible(&screen));

        // Test resolve_style
        let style = TerminalEmulator::resolve_style(&screen, rows[0][0].style_id);
        assert!(style.is_default());

        // Test alt screen
        assert!(!TerminalEmulator::in_alt_screen(&screen));

        // Test title
        assert_eq!(TerminalEmulator::title(&screen), "");

        // Test scrollback
        assert_eq!(TerminalEmulator::scrollback_len(&screen), 0);
        assert_eq!(TerminalEmulator::scrollback_rows(&screen).count(), 0);

        // Test take_responses
        assert!(TerminalEmulator::take_responses(&mut screen).is_empty());
    }

    #[test]
    fn screen_as_dyn_terminal_emulator() {
        let mut screen = Screen::new(40, 10, 50);
        let emu: &mut dyn TerminalEmulator = &mut screen;
        emu.process(b"test");
        assert_eq!(emu.cols(), 40);
        assert_eq!(emu.rows(), 10);
        let rows: Vec<_> = emu.visible_rows().collect();
        assert_eq!(rows[0][0].c, 't');
    }

    #[test]
    fn ansi_renderer_implements_terminal_renderer() {
        use super::render::AnsiRenderer;
        use super::traits::TerminalRenderer;

        let mut screen = Screen::new(10, 3, 0);
        screen.process(b"Hi");

        let mut renderer = AnsiRenderer::new();
        let output = renderer.render(&screen, true);
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("Hi"),
            "render output should contain 'Hi', got: {text}"
        );
    }

    #[test]
    fn ansi_renderer_clears_title_when_empty() {
        // Bug 3: AnsiRenderer should emit a title-clearing OSC when the
        // title was previously set and is now empty.
        use super::render::AnsiRenderer;
        use super::traits::TerminalRenderer;

        let mut screen = Screen::new(10, 3, 0);
        // Set a title
        screen.process(b"\x1b]2;Hello\x07");
        assert_eq!(screen.title(), "Hello");

        let mut renderer = AnsiRenderer::new();
        // First render — should contain the title
        let output = renderer.render(&screen, true);
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("\x1b]2;Hello\x07"),
            "first render should contain title OSC"
        );

        // Clear the title
        screen.process(b"\x1b]2;\x07");
        assert_eq!(screen.title(), "");

        // Second render — should emit an empty-title OSC to clear it
        let output = renderer.render(&screen, true);
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("\x1b]2;\x07"),
            "render should emit title-clearing OSC when title becomes empty, \
             got: {text}"
        );
    }
}

#[cfg(test)]
pub(super) mod test_helpers {
    use super::*;

    /// Strip ANSI escape sequences, returning only printable text.
    pub fn strip_ansi(bytes: &[u8]) -> String {
        let s = String::from_utf8_lossy(bytes);
        let mut out = String::new();
        let mut in_esc = false;
        for ch in s.chars() {
            if in_esc {
                if ch.is_ascii_alphabetic() {
                    in_esc = false;
                }
                continue;
            }
            if ch == '\x1b' {
                in_esc = true;
                continue;
            }
            if ch >= ' ' {
                out.push(ch);
            }
        }
        out.trim_end().to_string()
    }

    /// Collect visible grid rows as trimmed strings.
    pub fn screen_lines(screen: &Screen) -> Vec<String> {
        screen
            .grid
            .visible_rows()
            .map(|row| {
                let s: String = row.iter().map(|c| c.c).collect();
                s.trim_end().to_string()
            })
            .collect()
    }

    /// Collect scrollback history as trimmed text strings (ANSI stripped).
    pub fn history_texts(screen: &Screen) -> Vec<String> {
        screen.get_history().iter().map(|b| strip_ansi(b)).collect()
    }
}

#[cfg(test)]
mod tests_content_history {
    use super::test_helpers::strip_ansi;
    use super::Screen;

    fn joined(screen: &Screen) -> String {
        screen
            .get_content_history()
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A marker printed on a FRESH session (single line, never scrolled out of
    /// the visible screen) MUST appear in get_content_history — this is the
    /// boot-answerer case scrollback-only semantics were blind to.
    #[test]
    fn includes_visible_unscrolled_marker() {
        let mut screen = Screen::new(40, 10, 1000);
        screen.process(b"DIALOG_MARKER");
        let text = joined(&screen);
        assert!(
            text.contains("DIALOG_MARKER"),
            "visible (unscrolled) marker must appear, got: {text:?}"
        );
        // And it came from the visible screen, not scrollback.
        assert!(
            screen.get_history().is_empty(),
            "precondition: nothing scrolled out yet"
        );
    }

    /// Scrollback lines come FIRST, then visible rows, in order.
    #[test]
    fn scrollback_then_visible_in_order() {
        let mut screen = Screen::new(40, 3, 1000);
        // 3-row screen; emit 5 labeled lines so the first ones scroll back.
        for i in 1..=5 {
            screen.process(format!("L{i}\r\n").as_bytes());
        }
        screen.process(b"VISIBLE_TAIL");
        let _ = screen.take_pending_scrollback();
        let text = joined(&screen);
        let pos_l1 = text.find("L1").expect("L1 (scrollback) present");
        let pos_tail = text.find("VISIBLE_TAIL").expect("visible tail present");
        assert!(
            pos_l1 < pos_tail,
            "scrollback must precede visible rows: {text:?}"
        );
    }

    /// Trailing all-blank visible rows are trimmed: the result ends at the last
    /// row with content (no run of empty trailing lines).
    #[test]
    fn trailing_blank_visible_rows_trimmed() {
        let mut screen = Screen::new(40, 10, 1000);
        screen.process(b"ONLY_LINE");
        let lines = screen.get_content_history();
        assert_eq!(
            lines.len(),
            1,
            "9 trailing blank visible rows must be trimmed, got {} lines",
            lines.len()
        );
        assert_eq!(strip_ansi(&lines[0]), "ONLY_LINE");
    }

    /// Empty lines BETWEEN content are preserved (only the trailing run trims).
    #[test]
    fn interior_blank_lines_preserved() {
        let mut screen = Screen::new(40, 10, 1000);
        screen.process(b"TOP\r\n\r\nBOTTOM");
        let lines = screen.get_content_history();
        // TOP, (blank), BOTTOM = 3 lines; trailing blanks after BOTTOM trimmed.
        assert_eq!(lines.len(), 3, "interior blank kept, got: {lines:?}");
        assert_eq!(strip_ansi(&lines[0]), "TOP");
        assert_eq!(strip_ansi(&lines[1]), "");
        assert_eq!(strip_ansi(&lines[2]), "BOTTOM");
    }

    /// Altscreen: a marker shown by a fullscreen (alt-screen) app MUST appear in
    /// get_content_history — content inspection differs from attach-replay,
    /// which drops history during alt screen.
    #[test]
    fn altscreen_visible_marker_included() {
        let mut screen = Screen::new(40, 10, 1000);
        // Enter alt screen (DEC 1049h), then draw a dialog marker.
        screen.process(b"\x1b[?1049h");
        assert!(screen.in_alt_screen(), "precondition: in alt screen");
        screen.process(b"FULLSCREEN_DIALOG");
        let text = joined(&screen);
        assert!(
            text.contains("FULLSCREEN_DIALOG"),
            "alt-screen dialog must appear in content history, got: {text:?}"
        );
    }
}

#[cfg(test)]
mod logical_direct_row_tests {
    use super::*;

    fn logical_lines(screen: &Screen) -> Vec<String> {
        let mut lines = Vec::new();
        let mut current = String::new();
        for chunk in screen
            .logical_emission(LogicalEmissionSurface::AttachReplay)
            .chunks
        {
            for cell in chunk.cells {
                if !cell.wide_early_padding && cell.display_width > 0 {
                    current.push(cell.ch);
                    current.push_str(&cell.combining);
                }
            }
            if chunk.end_of_line {
                lines.push(current.trim_end_matches(' ').to_owned());
                current.clear();
            }
        }
        lines
    }

    #[test]
    fn private_direct_row_events_stay_inside_cfg_test_harness() {
        let mut insert_crossing = Screen::new(5, 4, 100);
        insert_crossing.process(b"\x1b[2;1HABCDEF");
        insert_crossing.logical_insert_row_for_test(3);
        assert_eq!(logical_lines(&insert_crossing), ["", "ABCDE", "", "F"]);

        let mut remove_endpoint = Screen::new(5, 4, 100);
        remove_endpoint.process(b"\x1b[2;1HABCDEF");
        remove_endpoint.logical_remove_row_for_test(2);
        assert_eq!(logical_lines(&remove_endpoint), ["", "F"]);

        let mut insert_below = Screen::new(5, 4, 100);
        insert_below.process(b"ABCDEF");
        insert_below.logical_insert_row_for_test(4);
        insert_below.process(b"G");
        assert_eq!(logical_lines(&insert_below), ["ABCDEFG"]);

        let mut remove_below = Screen::new(5, 4, 100);
        remove_below.process(b"ABCDEF");
        remove_below.logical_remove_row_for_test(4);
        remove_below.process(b"G");
        assert_eq!(logical_lines(&remove_below), ["ABCDEFG"]);
    }
}

#[cfg(test)]
mod history_boundary_tests;
#[cfg(test)]
mod tests_large_updates;
#[cfg(test)]
mod tests_live_scrollback;
#[cfg(test)]
mod tests_progress_bar_scrollback;
#[cfg(test)]
mod tests_reattach;
#[cfg(test)]
mod tests_reconnect_scrollback;
#[cfg(test)]
mod tests_resize;
#[cfg(test)]
mod tests_screen;
#[cfg(test)]
mod tests_banner;
