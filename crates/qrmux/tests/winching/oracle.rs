//! Independent event oracle for Spec W's adjacency-edge lifecycle.
//!
//! This is intentionally a test-side terminal model. It does not inspect
//! `Screen` internals or model B1's saved-main representation. Real c59's
//! active cursor/shared SavedCursor and its cursor-free SavedGrid rows are
//! modeled as separate mechanisms, while persistent joins come only from the
//! observable contract edge created when autowrap fires. See `RULE_MAP.md`
//! beside this file for the auditable A1--A16 and G8 symbol map.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use unicode_width::UnicodeWidthChar;

const MAX_COMBINING: usize = 16;

fn default_tab_stops(cols: u16) -> Vec<bool> {
    (0..cols)
        .map(|column| column > 0 && column % 8 == 0)
        .collect()
}

pub type RowId = u64;
pub type EdgeId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Print(String),
    /// REP count: `None` is omitted/default; `Some(0)` also defaults to one.
    RepeatLast(Option<u16>),
    SetStyle(CellStyle),
    CombiningMark(String),
    Cr,
    Lf,
    Index,
    ReverseIndex,
    NextLine,
    Decsc,
    Decrc,
    CsiSaveCursor,
    CsiRestoreCursor,
    Mode1048Save,
    Mode1048Restore,
    Backspace,
    HorizontalTab,
    HorizontalTabSet,
    /// TBC parameter: 0 clears the current-column stop; 3 clears all stops.
    TabClear(u16),
    CursorUp(u16),
    CursorDown(u16),
    CursorForward(u16),
    CursorBack(u16),
    CursorNextLine(u16),
    CursorPrevLine(u16),
    CursorHorizontalAbsolute(u16),
    CursorVerticalAbsolute(u16),
    OriginMode(bool),
    Cup {
        row: u16,
        col: u16,
    },
    EraseDisplay(u16),
    EraseLine(u16),
    EraseChars(u16),
    DeleteChars(u16),
    InsertChars(u16),
    Resize {
        cols: u16,
        rows: u16,
    },
    EnterAlt1049,
    ExitAlt1049,
    /// Modes 47 and 1047 reach the same c59 helpers, but remain separate
    /// operations so every entry/exit mode combination is executable.
    EnterAlt47,
    ExitAlt47,
    EnterAlt1047,
    ExitAlt1047,
    Ed3,
    ClearScrollback,
    Ris,
    SetScrollRegion {
        top: u16,
        bottom: u16,
    },
    ScrollUp(u16),
    ScrollDown(u16),
    InsertLines(u16),
    DeleteLines(u16),
    InsertRow {
        row: u16,
    },
    RemoveRow {
        row: u16,
    },
    Decaln,
    Decawm(bool),
}

#[derive(Clone, Debug)]
pub struct Workload {
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_limit: usize,
    pub emit_width: u16,
    pub emission_surface: EmissionSurface,
    pub ops: Vec<Op>,
    pub expected: Option<FixtureExpectation>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmissionSurface {
    #[default]
    AttachReplay,
    GetHistory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureExpectation {
    pub live: usize,
    pub severed: usize,
    pub logical_lines: Vec<String>,
    pub outgoing_marks: Option<usize>,
}

#[derive(Clone, Debug)]
struct Row {
    /// `None` is the fail-closed row allocated after RowId exhaustion. Such a
    /// row participates in ordered cell emission but can never be an edge
    /// endpoint or compare equal to an older row.
    id: Option<RowId>,
    width: u16,
    content_revision: FailClosedGeneration,
    cells: Vec<Cell>,
    outgoing: Option<EdgeId>,
}

/// A checked, non-wrapping generation. Exhaustion is sticky: once advancing
/// would overflow, no saved numeric token can ever compare equal again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailClosedGeneration {
    current: Option<u64>,
}

impl FailClosedGeneration {
    pub fn new() -> Self {
        Self { current: Some(0) }
    }

    pub fn at(value: u64) -> Self {
        Self {
            current: Some(value),
        }
    }

    pub fn token(self) -> Option<u64> {
        self.current
    }

    pub fn matches(self, token: u64) -> bool {
        self.current == Some(token)
    }

    pub fn advance(&mut self) -> bool {
        self.current = self.current.and_then(|value| value.checked_add(1));
        self.current.is_some()
    }

    /// Allocate the current identity once, then checked-advance. At `MAX` the
    /// value is returned exactly once and the allocator becomes sticky-poisoned.
    pub fn take_next(&mut self) -> Option<u64> {
        let value = self.current?;
        self.advance();
        Some(value)
    }

    pub fn is_exhausted(self) -> bool {
        self.current.is_none()
    }
}

#[derive(Clone, Debug)]
struct Cell {
    ch: char,
    display_width: u8,
    combining: String,
    style: CellStyle,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CellUnderline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellColor {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Semantic cell style carried across the transport-framed referee boundary.
/// This deliberately mirrors values, not product style-table IDs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: CellUnderline,
    pub blink: bool,
    pub inverse: bool,
    pub strikethrough: bool,
    pub hidden: bool,
    pub fg: Option<CellColor>,
    pub bg: Option<CellColor>,
    pub underline_color: Option<CellColor>,
}

impl CellStyle {
    pub fn red() -> Self {
        Self {
            fg: Some(CellColor::Indexed(1)),
            ..Self::default()
        }
    }

    pub fn blue_background() -> Self {
        Self {
            bg: Some(CellColor::Indexed(4)),
            ..Self::default()
        }
    }

    /// Match the performer's BCE `blank_cell`: preserve only the active
    /// background and default every other semantic style field.
    fn blank_cell(self) -> Self {
        Self {
            bg: self.bg,
            ..Self::default()
        }
    }
}

impl Cell {
    fn blank() -> Self {
        Self {
            ch: ' ',
            display_width: 1,
            combining: String::new(),
            style: CellStyle::default(),
        }
    }

    fn styled_blank(style: CellStyle) -> Self {
        Self {
            style,
            ..Self::blank()
        }
    }

    /// c59 keeps combining marks outside `Cell`, so wide-pair repair replaces
    /// the base cell without clearing marks stored at the repaired column.
    fn blank_base_preserving_combining(&mut self) {
        let combining = std::mem::take(&mut self.combining);
        *self = Self::blank();
        self.combining = combining;
    }

    /// DCH/ICH post-shift repair uses the performer's active BCE blank rather
    /// than the grid-wide default blank used by general wide-pair repair.
    fn bce_blank_base_preserving_combining(&mut self, current_style: CellStyle) {
        let combining = std::mem::take(&mut self.combining);
        *self = Self::styled_blank(current_style.blank_cell());
        self.combining = combining;
    }

    fn glyph(ch: char, display_width: u8, style: CellStyle) -> Self {
        Self {
            ch,
            display_width,
            combining: String::new(),
            style,
        }
    }

    fn continuation(style: CellStyle) -> Self {
        Self::glyph('\0', 0, style)
    }
}

impl Row {
    fn blank(id: Option<RowId>, width: u16) -> Self {
        Self {
            id,
            width,
            content_revision: FailClosedGeneration::new(),
            cells: vec![Cell::blank(); width as usize],
            outgoing: None,
        }
    }

    fn is_blank(&self) -> bool {
        // Spec W's shrink-chop classification deliberately ignores style, but
        // combining data is content even when its base is a space/width-0 cell.
        // This intentionally exceeds c59's char-only grid.rs predicate.
        self.cells.iter().all(|cell| {
            (cell.ch == ' ' || cell.display_width == 0) && cell.combining.is_empty()
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapKind {
    Normal,
    WideEarly,
}

#[derive(Clone, Debug)]
struct PendingWrap {
    source: RowId,
    source_revision: u64,
    valid_width: u16,
    cursor_continuity: u64,
    kind: WrapKind,
    padding_count: u16,
    alt_epoch: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SequentialEdge {
    source: RowId,
    target: RowId,
    cursor_continuity: u64,
    alt_epoch: Option<u64>,
}

#[derive(Clone, Debug)]
struct SavedCursor {
    cursor_x: u16,
    cursor_y: u16,
    pending: Option<PendingWrap>,
    untracked_wrap_pending: bool,
    sequential: Option<SequentialEdge>,
    cursor_continuity: FailClosedGeneration,
    style: CellStyle,
    autowrap: bool,
    origin_mode: bool,
    /// Present only when mode 1049 performed this save for a particular alt
    /// entry. Ordinary DECSC/CSI-s/1048 saves deliberately carry no epoch.
    alt_epoch: Option<u64>,
    /// Distinguishes an exhausted-but-current 1049 snapshot from an ordinary
    /// shared-cursor save, both of which necessarily have `alt_epoch == None`.
    saved_by_alt_1049: bool,
}

#[derive(Clone, Debug)]
/// Independent model of real c59's `SavedGrid`: rows plus grid modes/region,
/// deliberately not cursor coordinates or pending/continuation state.
/// Cursor state is modeled only through the performer's shared SavedCursor.
struct SavedMainRows {
    rows: Vec<Row>,
    cols: u16,
    scroll_top: u16,
    scroll_bottom: u16,
    autowrap: bool,
    origin_mode: bool,
    alt_epoch: Option<u64>,
    /// The entry operation is still known after checked epoch exhaustion.
    /// This does not authorize provenance; it only classifies the matching
    /// 1049 snapshot so geometry-invalidated raw motion can be cleared.
    entered_with_shared_cursor_save: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeDisposition {
    Live,
    Severed(&'static str),
}

#[derive(Clone, Debug)]
pub struct EdgeRecord {
    pub id: EdgeId,
    pub source: RowId,
    pub target: RowId,
    pub valid_width: u16,
    pub kind: WrapKind,
    pub padding_count: u16,
    pub disposition: EdgeDisposition,
}

#[derive(Clone, Debug, Default)]
pub struct StepEffect {
    pub created_edges: Vec<EdgeId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Emission {
    pub live_edges: Vec<(RowId, RowId)>,
    pub actual: CandidateTransportEmission,
    pub laid_out_lines: Vec<String>,
}

impl Emission {
    pub fn logical_lines(&self) -> Vec<String> {
        self.actual
            .client_lines()
            .into_iter()
            .map(|cells| {
                CandidateHistoryChunk {
                    cells,
                    end_of_line: true,
                }
                .plain_text()
            })
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CandidateTransportEmission {
    /// The actual ordered HistoryLogical transport chunks. Client-line
    /// boundaries are derived only from `end_of_line`; no logical-line grouping
    /// or structural side channel is accepted separately.
    pub chunks: Vec<CandidateHistoryChunk>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CandidateHistoryChunk {
    pub cells: Vec<CandidateCell>,
    pub end_of_line: bool,
}

/// Candidate-facing cell value. Full frozen rows cross this boundary: ordinary
/// trailing blanks, width-0 wide continuations, combining data, style, and
/// WideEarly padding authority is never persisted in a cell or inferred from
/// blanks; it is derived from a validated live edge at read time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateCell {
    pub ch: char,
    pub display_width: u8,
    pub combining: String,
    pub style: CellStyle,
    pub wide_early_padding: bool,
}

impl CandidateCell {
    pub fn blank() -> Self {
        Self {
            ch: ' ',
            display_width: 1,
            combining: String::new(),
            style: CellStyle::default(),
            wide_early_padding: false,
        }
    }
}

impl From<&Cell> for CandidateCell {
    fn from(cell: &Cell) -> Self {
        Self {
            ch: cell.ch,
            display_width: cell.display_width,
            combining: cell.combining.clone(),
            style: cell.style,
            // Padding authority is edge-qualified by the reader. A frozen
            // blank cell never carries a persistent omission marker.
            wide_early_padding: false,
        }
    }
}

impl CandidateHistoryChunk {
    pub fn plain_text(&self) -> String {
        let last = self.cells.iter().rposition(|cell| {
            !cell.wide_early_padding
                && ((cell.ch != ' ' && cell.ch != '\0')
                    || !cell.combining.is_empty()
                    || cell.style != CellStyle::default())
        });
        let mut text = String::new();
        for cell in self.cells.iter().take(last.map_or(0, |index| index + 1)) {
            if !cell.wide_early_padding && cell.display_width > 0 {
                text.push(cell.ch);
                text.push_str(&cell.combining);
            }
        }
        text
    }

    /// Render one complete logical line. The maintained terminal-observable
    /// witness is `CandidateTransportEmission::render_ansi`, which joins opaque
    /// chunks through `end_of_line` before calling this same renderer.
    pub fn render_ansi(&self) -> Vec<u8> {
        render_ansi_chunks(std::slice::from_ref(self))
    }
}

impl CandidateTransportEmission {
    pub fn client_lines(&self) -> Vec<Vec<CandidateCell>> {
        let mut lines = Vec::new();
        let mut current = Vec::new();
        for chunk in &self.chunks {
            current.extend(chunk.cells.iter().cloned());
            if chunk.end_of_line {
                lines.push(std::mem::take(&mut current));
            }
        }
        lines
    }

    /// Terminal-observable rendering after a clean referee verdict. Chunk
    /// boundaries are opaque: tail detection and style transitions run once
    /// over each complete logical line, then its framing emits CRLF.
    pub fn render_ansi(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut line_start = 0;
        for (index, chunk) in self.chunks.iter().enumerate() {
            if chunk.end_of_line {
                out.extend(render_ansi_chunks(&self.chunks[line_start..=index]));
                out.extend_from_slice(b"\r\n");
                line_start = index + 1;
            }
        }
        if line_start < self.chunks.len() {
            out.extend(render_ansi_chunks(&self.chunks[line_start..]));
        }
        out
    }
}

fn render_ansi_chunks(chunks: &[CandidateHistoryChunk]) -> Vec<u8> {
    let cells = || chunks.iter().flat_map(|chunk| chunk.cells.iter());
    let last = cells()
        .enumerate()
        .filter(|(_, cell)| {
            !cell.wide_early_padding
                && ((cell.ch != ' ' && cell.ch != '\0')
                    || !cell.combining.is_empty()
                    || cell.style != CellStyle::default())
        })
        .map(|(index, _)| index)
        .last();
    let mut out = Vec::new();
    let mut current = CellStyle::default();
    for cell in cells().take(last.map_or(0, |index| index + 1)) {
        if cell.wide_early_padding || cell.display_width == 0 {
            continue;
        }
        if cell.style != current {
            write_style_with_reset(cell.style, &mut out);
            current = cell.style;
        }
        let mut buf = [0; 4];
        out.extend_from_slice(cell.ch.encode_utf8(&mut buf).as_bytes());
        for mark in cell.combining.chars() {
            out.extend_from_slice(mark.encode_utf8(&mut buf).as_bytes());
        }
    }
    if current != CellStyle::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefereeVerdict {
    pub checked_joins: usize,
    pub false_joins: Vec<FalseJoin>,
    pub corruptions: Vec<EmissionCorruption>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FalseJoin {
    pub source: Option<RowId>,
    pub target: Option<RowId>,
    pub logical_line: usize,
    pub boundary: usize,
    pub reason: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmissionCorruption {
    pub logical_line: Option<usize>,
    pub reason: &'static str,
}

#[derive(Clone, Debug)]
pub struct Oracle {
    cols: u16,
    rows: u16,
    scrollback_limit: usize,
    history: VecDeque<Row>,
    visible: Vec<Row>,
    saved_main: Option<SavedMainRows>,
    in_alt: bool,
    cursor_x: u16,
    cursor_y: u16,
    scroll_top: u16,
    scroll_bottom: u16,
    autowrap: bool,
    origin_mode: bool,
    current_style: CellStyle,
    /// Mutable c59 grid tab-stop table. HTS/TBC do not move the cursor, but
    /// their state controls a later HT destination and therefore later A1.
    tab_stops: Vec<bool>,
    pending: Option<PendingWrap>,
    /// Physical deferred-wrap state for an exhausted-ID source. It preserves
    /// terminal motion while remaining structurally incapable of A1 creation.
    untracked_wrap_pending: bool,
    sequential: Option<SequentialEdge>,
    cursor_continuity: FailClosedGeneration,
    saved_cursor: Option<SavedCursor>,
    next_row: FailClosedGeneration,
    next_edge: FailClosedGeneration,
    next_alt_epoch: FailClosedGeneration,
    last_printed_char: char,
    edges: BTreeMap<EdgeId, EdgeRecord>,
}

impl Oracle {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut oracle = Self {
            cols,
            rows,
            scrollback_limit,
            history: VecDeque::new(),
            visible: Vec::new(),
            saved_main: None,
            in_alt: false,
            cursor_x: 0,
            cursor_y: 0,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            autowrap: true,
            origin_mode: false,
            current_style: CellStyle::default(),
            tab_stops: default_tab_stops(cols),
            pending: None,
            untracked_wrap_pending: false,
            sequential: None,
            cursor_continuity: FailClosedGeneration::new(),
            saved_cursor: None,
            next_row: FailClosedGeneration::at(1),
            next_edge: FailClosedGeneration::at(1),
            next_alt_epoch: FailClosedGeneration::at(1),
            last_printed_char: ' ',
            edges: BTreeMap::new(),
        };
        for _ in 0..rows {
            let row = oracle.new_blank_row(cols);
            oracle.visible.push(row);
        }
        oracle
    }

    pub fn run(workload: &Workload) -> Self {
        let mut oracle = Self::new(workload.cols, workload.rows, workload.scrollback_limit);
        for op in &workload.ops {
            oracle.apply(op);
        }
        oracle
    }

    pub fn apply(&mut self, op: &Op) -> StepEffect {
        let mut effect = StepEffect::default();
        match op {
            Op::Print(text) => {
                for ch in text.chars() {
                    self.print_char(ch, &mut effect);
                }
            }
            Op::RepeatLast(count) => {
                for _ in 0..count.unwrap_or(1).max(1) {
                    self.print_char(self.last_printed_char, &mut effect);
                }
            }
            Op::SetStyle(style) => self.current_style = *style,
            Op::CombiningMark(text) => {
                for ch in text.chars() {
                    assert_eq!(UnicodeWidthChar::width(ch), Some(0));
                    // Real Perform::print stores the pre-map input for REP
                    // before routing width-0 input to the combining handler.
                    self.print_char(ch, &mut effect);
                }
            }
            Op::Cr => self.explicit_cr(),
            Op::Lf => self.explicit_lf(),
            Op::Index => self.index(),
            Op::ReverseIndex => self.reverse_index(),
            Op::NextLine => self.next_line(),
            Op::Decsc | Op::CsiSaveCursor | Op::Mode1048Save => self.save_cursor(),
            Op::Decrc | Op::CsiRestoreCursor | Op::Mode1048Restore => self.restore_cursor(),
            Op::Backspace => self.backspace(),
            Op::HorizontalTab => self.horizontal_tab(),
            Op::HorizontalTabSet => self.horizontal_tab_set(),
            Op::TabClear(mode) => self.tab_clear(*mode),
            Op::CursorUp(count) => self.cursor_up(*count),
            Op::CursorDown(count) => self.cursor_down(*count),
            Op::CursorForward(count) => self.cursor_forward(*count),
            Op::CursorBack(count) => self.cursor_back(*count),
            Op::CursorNextLine(count) => self.cursor_next_line(*count),
            Op::CursorPrevLine(count) => self.cursor_prev_line(*count),
            Op::CursorHorizontalAbsolute(col) => self.cursor_horizontal_absolute(*col),
            Op::CursorVerticalAbsolute(row) => self.cursor_vertical_absolute(*row),
            Op::OriginMode(enabled) => self.origin_mode(*enabled),
            Op::Cup { row, col } => self.cup(*row, *col),
            Op::EraseDisplay(mode) => self.erase_display(*mode),
            Op::EraseLine(mode) => self.erase_line(*mode),
            Op::EraseChars(count) => self.erase_chars(*count),
            Op::DeleteChars(count) => self.delete_chars(*count),
            Op::InsertChars(count) => self.insert_chars(*count),
            Op::Resize { cols, rows } => self.resize(*cols, *rows),
            Op::EnterAlt1049 => self.enter_alt(true),
            Op::ExitAlt1049 => self.exit_alt(true),
            Op::EnterAlt47 => self.enter_alt(false),
            Op::ExitAlt47 => self.exit_alt(false),
            Op::EnterAlt1047 => self.enter_alt(false),
            Op::ExitAlt1047 => self.exit_alt(false),
            Op::Ed3 => self.clear_history("A11: ED3 destroyed endpoint"),
            Op::ClearScrollback => {
                self.clear_history("A11: bulk clear_scrollback destroyed endpoint")
            }
            Op::Ris => self.ris(),
            Op::SetScrollRegion { top, bottom } => self.set_scroll_region(*top, *bottom),
            Op::ScrollUp(count) => {
                let raw_wrap_pending = self.raw_wrap_pending();
                for _ in 0..(*count).max(1).min(self.rows) {
                    self.scroll_up(false);
                }
                self.reconcile_raw_preserving_topology(raw_wrap_pending);
            }
            Op::ScrollDown(count) => {
                let raw_wrap_pending = self.raw_wrap_pending();
                for _ in 0..(*count).max(1).min(self.rows) {
                    self.scroll_down();
                }
                self.reconcile_raw_preserving_topology(raw_wrap_pending);
            }
            Op::InsertLines(count) => self.insert_lines(*count),
            Op::DeleteLines(count) => self.delete_lines(*count),
            Op::InsertRow { row } => self.insert_row(*row),
            Op::RemoveRow { row } => self.remove_row(*row),
            Op::Decaln => self.decaln(),
            Op::Decawm(enabled) => {
                self.autowrap = *enabled;
                // A14/c59: a mode toggle is not content, topology, geometry,
                // or cursor motion. Existing pending construction and an
                // already-created edge remain qualified; while off, printing
                // cannot consume/create an autowrap edge.
            }
        }
        effect
    }

    pub fn live_count(&mut self) -> usize {
        self.validate_all_edges();
        self.edges
            .values()
            .filter(|edge| edge.disposition == EdgeDisposition::Live)
            .count()
    }

    pub fn severed_count(&self) -> usize {
        self.edges
            .values()
            .filter(|edge| matches!(edge.disposition, EdgeDisposition::Severed(_)))
            .count()
    }

    pub fn edge_records(&self) -> Vec<EdgeRecord> {
        self.edges.values().cloned().collect()
    }

    pub fn emission(&mut self, emit_width: u16) -> Emission {
        self.emission_for(EmissionSurface::AttachReplay, emit_width)
    }

    pub fn emission_for(&mut self, surface: EmissionSurface, emit_width: u16) -> Emission {
        self.validate_all_edges();
        let rows = self.emission_order_rows(surface);
        let mut chunks: Vec<CandidateHistoryChunk> = Vec::new();
        let mut chunk_has_history = Vec::new();
        let mut live_edges = Vec::new();
        let mut current = Vec::new();
        let mut current_has_history = false;

        for (index, (row, is_history)) in rows.iter().enumerate() {
            let next = rows.get(index + 1).map(|(row, _)| *row);
            let edge = row
                .id
                .zip(next.and_then(|target| target.id))
                .and_then(|(source, target)| self.validated_outgoing(source, target));
            current_has_history |= *is_history;
            current.extend(self.candidate_cells_for_row(row, next));
            if let Some(edge) = edge {
                live_edges.push((edge.source, edge.target));
            } else {
                chunks.push(CandidateHistoryChunk {
                    cells: std::mem::take(&mut current),
                    end_of_line: true,
                });
                chunk_has_history.push(std::mem::take(&mut current_has_history));
            }
        }
        // Display-line omission is canonical and occurs only after full cells
        // have been grouped. It never changes the referee's matching unit.
        while chunks
            .last()
            .is_some_and(|line| line_is_omittable_trailing_blank(line))
            && chunk_has_history.last() == Some(&false)
        {
            chunks.pop();
            chunk_has_history.pop();
        }

        let actual = CandidateTransportEmission { chunks };
        let mut laid_out_lines = Vec::new();
        for line in actual.client_lines() {
            let line = CandidateHistoryChunk {
                cells: line,
                end_of_line: true,
            };
            laid_out_lines.extend(layout_at_display_width(
                &line.plain_text(),
                emit_width.max(1),
            ));
        }

        Emission {
            live_edges,
            actual,
            laid_out_lines,
        }
    }

    /// PRIMARY RULE A13 + A14 referee for W2's final transport surface.
    ///
    /// Candidate chunks contain the concrete ordered cells sent through
    /// HistoryLogical and the exact `end_of_line` flag that makes the client
    /// append CRLF. The referee first derives client logical lines solely from
    /// those flags, then consumes full, untrimmed frozen rows. Because every row
    /// has a positive fixed cell count and every field is matched exactly, each
    /// derived line has one physical-row composition. Every boundary within
    /// that client line is checked against the live A1 ledger. There is no
    /// pre-grouped line list, join claim, row ID, or post-gate framing step.
    pub fn referee_transport_emission(
        &mut self,
        candidate: &CandidateTransportEmission,
    ) -> RefereeVerdict {
        self.referee_transport_emission_for(EmissionSurface::AttachReplay, candidate)
    }

    pub fn referee_transport_emission_for(
        &mut self,
        surface: EmissionSurface,
        candidate: &CandidateTransportEmission,
    ) -> RefereeVerdict {
        self.validate_all_edges();
        let rows = self.emission_order_rows(surface);
        let mut row_index = 0;
        let mut checked_joins = 0;
        let mut false_joins = Vec::new();
        let mut corruptions = Vec::new();
        let mut client_lines = Vec::new();
        let mut client_line_has_history = Vec::new();
        let mut current_line = Vec::new();

        for chunk in &candidate.chunks {
            current_line.extend(chunk.cells.iter().cloned());
            if chunk.end_of_line {
                client_lines.push(std::mem::take(&mut current_line));
            }
        }
        if !current_line.is_empty()
            || candidate
                .chunks
                .last()
                .is_some_and(|chunk| !chunk.end_of_line)
        {
            corruptions.push(EmissionCorruption {
                logical_line: Some(client_lines.len()),
                reason: "transport ended without end_of_line framing",
            });
        }

        for (logical_line, cells) in client_lines.iter().enumerate() {
            if cells.is_empty() {
                corruptions.push(EmissionCorruption {
                    logical_line: Some(logical_line),
                    reason: "transport-framed line contains no full frozen row",
                });
                continue;
            }
            let start = row_index;
            let mut cell_index = 0;
            while cell_index < cells.len() {
                let Some((row, _)) = rows.get(row_index).copied() else {
                    corruptions.push(EmissionCorruption {
                        logical_line: Some(logical_line),
                        reason: "candidate contains cells beyond the frozen row domain",
                    });
                    break;
                };
                let end = cell_index + row.cells.len();
                if end > cells.len() {
                    corruptions.push(EmissionCorruption {
                        logical_line: Some(logical_line),
                        reason: "transport framing ends in the middle of a frozen row",
                    });
                    break;
                }
                let expected = self
                    .candidate_cells_for_row(row, rows.get(row_index + 1).map(|(next, _)| *next));
                if cells[cell_index..end] != expected {
                    corruptions.push(EmissionCorruption {
                        logical_line: Some(logical_line),
                        reason: "candidate cell differs from the ordered frozen row",
                    });
                    break;
                }
                cell_index = end;
                row_index += 1;
            }

            if cell_index != cells.len() {
                client_line_has_history.push(false);
                continue;
            }
            client_line_has_history.push(
                rows[start..row_index]
                    .iter()
                    .any(|(_, is_history)| *is_history),
            );
            for (boundary, pair) in rows[start..row_index].windows(2).enumerate() {
                checked_joins += 1;
                let source = pair[0].0.id;
                let target = pair[1].0.id;
                if source
                    .zip(target)
                    .and_then(|(source, target)| self.validated_outgoing(source, target))
                    .is_none()
                {
                    false_joins.push(FalseJoin {
                        source,
                        target,
                        logical_line,
                        boundary,
                        reason:
                            "no live validated A1 edge authorizes this transport-framed cell join",
                    });
                }
            }
        }

        if corruptions.is_empty() {
            let remaining = &rows[row_index..];
            if !remaining
                .iter()
                .all(|(row, is_history)| !*is_history && Self::row_is_omittable_trailing_blank(row))
            {
                corruptions.push(EmissionCorruption {
                    logical_line: None,
                    reason: "candidate omitted a nonblank frozen row",
                });
            } else if client_lines
                .last()
                .is_some_and(|cells| cells_are_omittable_trailing_blank(cells))
                && client_line_has_history.last() != Some(&true)
            {
                corruptions.push(EmissionCorruption {
                    logical_line: client_lines.len().checked_sub(1),
                    reason: "candidate emitted a noncanonical trailing blank logical line",
                });
            }
        }

        RefereeVerdict {
            checked_joins,
            false_joins,
            corruptions,
        }
    }

    fn row_is_omittable_trailing_blank(row: &Row) -> bool {
        row.cells.iter().all(|cell| {
            cell.ch == ' '
                && cell.display_width == 1
                && cell.combining.is_empty()
                && cell.style == CellStyle::default()
        })
    }

    pub fn emission_row_ids(&self) -> Vec<RowId> {
        self.emission_order_rows(EmissionSurface::AttachReplay)
            .into_iter()
            .filter_map(|(row, _)| row.id)
            .collect()
    }

    pub fn outgoing_mark(&self, row_id: RowId) -> Option<EdgeId> {
        self.row(row_id).and_then(|row| row.outgoing)
    }

    pub fn outgoing_mark_count(&self) -> usize {
        self.history
            .iter()
            .chain(self.visible.iter())
            .chain(self.saved_main.iter().flat_map(|saved| saved.rows.iter()))
            .filter(|row| row.outgoing.is_some())
            .count()
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    #[cfg(test)]
    pub fn set_identity_allocators_for_test(
        &mut self,
        next_row: FailClosedGeneration,
        next_edge: FailClosedGeneration,
    ) {
        self.next_row = next_row;
        self.next_edge = next_edge;
    }

    #[cfg(test)]
    pub fn set_alt_epoch_allocator_for_test(&mut self, next_alt_epoch: FailClosedGeneration) {
        self.next_alt_epoch = next_alt_epoch;
    }

    #[cfg(test)]
    pub fn untracked_row_count_for_test(&self) -> usize {
        self.history
            .iter()
            .chain(self.visible.iter())
            .chain(self.saved_main.iter().flat_map(|saved| saved.rows.iter()))
            .filter(|row| row.id.is_none())
            .count()
    }

    fn new_blank_row(&mut self, width: u16) -> Row {
        let id = self.next_row.take_next();
        Row::blank(id, width)
    }

    fn new_blank_row_with_style(&mut self, width: u16, blank: Cell) -> Row {
        let mut row = self.new_blank_row(width);
        row.cells.fill(blank);
        row
    }

    fn row(&self, id: RowId) -> Option<&Row> {
        self.history
            .iter()
            .chain(self.visible.iter())
            .chain(self.saved_main.iter().flat_map(|saved| saved.rows.iter()))
            .find(|row| row.id == Some(id))
    }

    fn row_mut(&mut self, id: RowId) -> Option<&mut Row> {
        if let Some(index) = self.history.iter().position(|row| row.id == Some(id)) {
            return self.history.get_mut(index);
        }
        if let Some(index) = self.visible.iter().position(|row| row.id == Some(id)) {
            return self.visible.get_mut(index);
        }
        if let Some(saved) = self.saved_main.as_mut() {
            if let Some(index) = saved.rows.iter().position(|row| row.id == Some(id)) {
                return saved.rows.get_mut(index);
            }
        }
        None
    }

    fn main_order_rows(&self) -> Vec<&Row> {
        self.history
            .iter()
            .chain(if self.in_alt {
                self.saved_main
                    .iter()
                    .flat_map(|saved| saved.rows.iter())
                    .collect::<Vec<_>>()
            } else {
                self.visible.iter().collect::<Vec<_>>()
            })
            .collect()
    }

    fn active_order_rows(&self) -> Vec<&Row> {
        self.visible.iter().collect()
    }

    fn emission_order_rows(&self, surface: EmissionSurface) -> Vec<(&Row, bool)> {
        match (surface, self.in_alt) {
            // Attach replay suppresses the saved main domain while alt is
            // active and emits only the active alt rows.
            (EmissionSurface::AttachReplay, true) => self
                .active_order_rows()
                .into_iter()
                .map(|row| (row, false))
                .collect(),
            // One-shot GetHistory is content inspection: main scrollback,
            // then active alt rows. Saved visible-main rows are not emitted.
            (EmissionSurface::GetHistory, true) => self
                .history
                .iter()
                .map(|row| (row, true))
                .chain(self.visible.iter().map(|row| (row, false)))
                .collect(),
            (_, false) => self
                .history
                .iter()
                .map(|row| (row, true))
                .chain(self.visible.iter().map(|row| (row, false)))
                .collect(),
        }
    }

    fn candidate_cells_for_row(&self, row: &Row, next: Option<&Row>) -> Vec<CandidateCell> {
        let marked = row
            .id
            .zip(next.and_then(|target| target.id))
            .and_then(|(source, target)| self.validated_outgoing(source, target))
            .filter(|edge| edge.kind == WrapKind::WideEarly)
            .map_or(0, |edge| edge.padding_count as usize);
        let marker_start = row.cells.len().saturating_sub(marked);
        row.cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let mut candidate = CandidateCell::from(cell);
                candidate.wide_early_padding = marked > 0 && index >= marker_start;
                candidate
            })
            .collect()
    }

    fn domain_adjacent(&self, source: RowId, target: RowId) -> bool {
        let adjacent = |rows: &[&Row]| {
            rows.windows(2)
                .any(|pair| pair[0].id == Some(source) && pair[1].id == Some(target))
        };
        adjacent(&self.main_order_rows()) || (self.in_alt && adjacent(&self.active_order_rows()))
    }

    fn validated_outgoing(&self, source: RowId, target: RowId) -> Option<&EdgeRecord> {
        let source_row = self.row(source)?;
        let edge = source_row
            .outgoing
            .and_then(|edge_id| self.edges.get(&edge_id))?;
        let kind_valid = match edge.kind {
            WrapKind::Normal => edge.padding_count == 0,
            WrapKind::WideEarly => {
                edge.padding_count > 0
                    && edge.padding_count <= edge.valid_width
                    && source_row.cells[source_row.cells.len() - edge.padding_count as usize..]
                        .iter()
                        // performer.rs writes the BCE base Cell directly and
                        // leaves Row's separate sparse combining entry intact.
                        .all(|cell| cell.ch == ' ' && cell.display_width == 1)
            }
        };
        (edge.disposition == EdgeDisposition::Live
            && edge.source == source
            && edge.target == target
            && source_row.width == edge.valid_width
            && self
                .row(target)
                .is_some_and(|row| row.width == edge.valid_width)
            && self.domain_adjacent(source, target)
            && kind_valid)
            .then_some(edge)
    }

    fn print_char(&mut self, ch: char, effect: &mut StepEffect) {
        // c59 records the pre-charset-map input for REP, including combining
        // input. The oracle has no charset mapping, so this is the same char.
        self.last_printed_char = ch;
        let display_width = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if display_width == 0 {
            self.write_combining_mark(ch);
            return;
        }

        if self.autowrap {
            if let Some(pending) = self.pending.take() {
                if let Some(edge) = self.perform_deferred_wrap(pending) {
                    effect.created_edges.push(edge);
                }
            } else if std::mem::take(&mut self.untracked_wrap_pending) {
                self.perform_untracked_wrap();
            }
        }

        // WideEarly is an immediate, separately qualified A1 path. The real
        // BCE blanks are stored as ordinary cells; only the live edge carries
        // their temporary padding/omission meaning.
        if display_width == 2 && self.cursor_x >= self.cols - 1 {
            if !self.autowrap || self.cols < 2 {
                return;
            }
            let source = self.visible[self.cursor_y as usize].id;
            self.before_content_write(source, true);
            let padding_count = self.cols - self.cursor_x;
            for x in self.cursor_x..self.cols {
                self.visible[self.cursor_y as usize].cells[x as usize]
                    .bce_blank_base_preserving_combining(self.current_style);
            }
            if let Some((source, source_revision, cursor_continuity)) = source.and_then(|source| {
                self.row(source)
                    .and_then(|row| row.content_revision.token())
                    .zip(self.cursor_continuity.token())
                    .map(|(source_revision, cursor_continuity)| {
                        (source, source_revision, cursor_continuity)
                    })
            }) {
                let early = PendingWrap {
                    source,
                    source_revision,
                    valid_width: self.cols,
                    cursor_continuity,
                    kind: WrapKind::WideEarly,
                    padding_count,
                    alt_epoch: None,
                };
                if let Some(edge) = self.perform_deferred_wrap(early) {
                    effect.created_edges.push(edge);
                }
            } else {
                self.perform_untracked_wrap();
            }
        }

        // With DECAWM off, c59 does not consume or clear a pre-existing raw
        // deferred-wrap bit before overwriting the margin. The write cuts the
        // old source qualification, so preserve that bit only as untracked.
        if !self.autowrap && self.raw_wrap_pending() {
            self.cut_sequential_preserving_raw_wrap();
        }

        let row_id = self.visible[self.cursor_y as usize].id;
        self.before_content_write(row_id, true);
        self.fixup_wide_char_at(self.cursor_x as usize, self.cursor_y as usize);
        self.visible[self.cursor_y as usize].cells[self.cursor_x as usize] =
            Cell::glyph(ch, display_width as u8, self.current_style);
        if display_width == 2 {
            self.fixup_wide_char_at(self.cursor_x as usize + 1, self.cursor_y as usize);
            self.visible[self.cursor_y as usize].cells[self.cursor_x as usize + 1] =
                Cell::continuation(self.current_style);
        }

        if self.cursor_x + display_width >= self.cols {
            self.cursor_x = self.cols - 1;
            if self.autowrap {
                self.pending = row_id
                    .and_then(|row_id| {
                        self.row(row_id)
                            .and_then(|row| row.content_revision.token())
                            .map(|source_revision| (row_id, source_revision))
                    })
                    .zip(self.cursor_continuity.token())
                    .map(
                        |((source, source_revision), cursor_continuity)| PendingWrap {
                            source,
                            source_revision,
                            valid_width: self.cols,
                            cursor_continuity,
                            kind: WrapKind::Normal,
                            padding_count: 0,
                            alt_epoch: None,
                        },
                    );
                self.untracked_wrap_pending = self.pending.is_none();
            }
        } else {
            self.cursor_x += display_width;
        }
    }

    fn write_combining_mark(&mut self, ch: char) {
        let x = if self.pending.is_some() || self.untracked_wrap_pending {
            self.cursor_x
        } else if self.cursor_x > 0 {
            self.cursor_x - 1
        } else {
            return;
        };
        let row_id = self.visible[self.cursor_y as usize].id;
        let mut target = x as usize;
        if self.visible[self.cursor_y as usize].cells[target].display_width == 0 && target > 0 {
            target -= 1;
        }
        // performer.rs:483-507: the 17th mark is a true no-op. Check the cap
        // before the semantic write facade so ignored input cannot sever an
        // edge or advance the row revision.
        if self.visible[self.cursor_y as usize].cells[target]
            .combining
            .chars()
            .count()
            >= MAX_COMBINING
        {
            return;
        }
        // A forward combining mark does not consume the deferred wrap in the
        // real performer. Qualify the pre-write token before its source
        // revision advances; only that current-stream token may be refreshed.
        let refresh_pending = row_id.is_some_and(|row_id| {
            self.pending.as_ref().is_some_and(|pending| {
                pending.source == row_id
                    && pending.valid_width == self.cols
                    && self.cursor_continuity.matches(pending.cursor_continuity)
                    && self
                        .row(row_id)
                        .is_some_and(|row| row.content_revision.matches(pending.source_revision))
            })
        });
        self.before_content_write(row_id, true);
        if refresh_pending {
            if let Some(source_revision) = row_id
                .and_then(|row_id| self.row(row_id))
                .and_then(|row| row.content_revision.token())
            {
                self.pending
                    .as_mut()
                    .expect("qualified pending token remains present")
                    .source_revision = source_revision;
            } else {
                // Checked generation exhaustion cannot issue a refreshed
                // numeric token and therefore fails closed.
                self.pending = None;
            }
        }
        self.visible[self.cursor_y as usize].cells[target]
            .combining
            .push(ch);
    }

    // RULE A1: the sole edge-creation seam.  No other function inserts a live
    // edge record or assigns `Row::outgoing = Some`.
    // RULE A2 + A16: creation consumes a fixed-width pending wrap, and the
    // resulting target is the sole authorized sequential continuation.
    fn perform_deferred_wrap(&mut self, pending: PendingWrap) -> Option<EdgeId> {
        if pending.valid_width != self.cols
            || pending.alt_epoch.is_some()
            || !self.cursor_continuity.matches(pending.cursor_continuity)
            || self.visible[self.cursor_y as usize].id != Some(pending.source)
            || !self
                .row(pending.source)
                .is_some_and(|row| row.content_revision.matches(pending.source_revision))
        {
            self.sequential = None;
            return None;
        }
        self.cursor_x = 0;
        if self.cursor_y == self.scroll_bottom {
            self.scroll_up(true);
        } else if self.cursor_y < self.rows - 1 {
            self.cursor_y += 1;
        }
        let Some(target) = self.visible[self.cursor_y as usize].id else {
            self.sequential = None;
            return None;
        };

        // c59 can clear pending and home x without advancing y when the cursor
        // is below a partial region at the physical bottom. That is not an
        // inter-row wrap and cannot publish a self-edge.
        if target == pending.source {
            self.sequential = None;
            return None;
        }

        // A1/A9/A13 source-survival guard: wrap-induced scrolling may destroy
        // the source (notably a one-row alt domain).  Never publish provenance
        // onto the replacement row and never panic; absence is the hard break.
        if self.row(pending.source).is_none() {
            self.sequential = None;
            return None;
        }

        let Some(id) = self.next_edge.take_next() else {
            // EdgeId exhaustion is sticky: the wrap still happened physically,
            // but no provenance is published and no older numeric ID is reused.
            self.sequential = None;
            return None;
        };
        let record = EdgeRecord {
            id,
            source: pending.source,
            target,
            valid_width: pending.valid_width,
            kind: pending.kind,
            padding_count: pending.padding_count,
            disposition: EdgeDisposition::Live,
        };
        self.edges.insert(id, record);
        self.row_mut(pending.source)
            .expect("source-survival guard")
            .outgoing = Some(id);
        self.sequential = Some(SequentialEdge {
            source: pending.source,
            target,
            cursor_continuity: pending.cursor_continuity,
            alt_epoch: pending.alt_epoch,
        });
        Some(id)
    }

    fn perform_untracked_wrap(&mut self) {
        self.pending = None;
        self.untracked_wrap_pending = false;
        self.sequential = None;
        self.cursor_x = 0;
        if self.cursor_y == self.scroll_bottom {
            self.scroll_up(true);
        } else if self.cursor_y < self.rows - 1 {
            self.cursor_y += 1;
        }
    }

    // RULE A7: source writes/erases/DECALN sever regardless of geometry; a
    // target write after explicit reposition severs its incoming edge.  Only
    // the A2 forward-print authorization preserves a target write.
    fn before_content_write(&mut self, row_id: Option<RowId>, forward_print: bool) {
        let Some(row_id) = row_id else {
            // Exhausted-ID rows are structurally ineligible for pending tokens
            // and edges. Their cell write proceeds as an unconditional break.
            self.pending = None;
            self.sequential = None;
            return;
        };
        if let Some(edge_id) = self.row(row_id).and_then(|row| row.outgoing) {
            self.sever_edge(edge_id, "A7: content mutation touched source row");
        }
        let incoming: Vec<EdgeId> = self
            .edges
            .values()
            .filter(|edge| edge.disposition == EdgeDisposition::Live && edge.target == row_id)
            .map(|edge| edge.id)
            .collect();
        for edge_id in incoming {
            let edge = self.edges.get(&edge_id).expect("incoming edge");
            let sequential = forward_print
                && self.sequential.is_some_and(|sequential| {
                    sequential.source == edge.source
                        && sequential.target == edge.target
                        && sequential.alt_epoch.is_none()
                        && self.cursor_continuity.matches(sequential.cursor_continuity)
                })
                && self.domain_adjacent(edge.source, edge.target)
                && self
                    .row(edge.source)
                    .is_some_and(|row| row.width == edge.valid_width)
                && self
                    .row(edge.target)
                    .is_some_and(|row| row.width == edge.valid_width);
            if !sequential {
                self.sever_edge(edge_id, "A7: target write after explicit reposition");
            }
        }
        if let Some(row) = self.row_mut(row_id) {
            row.content_revision.advance();
        }
    }

    fn cancel_sequential(&mut self) {
        self.pending = None;
        self.untracked_wrap_pending = false;
        self.sequential = None;
        self.cursor_continuity.advance();
    }

    /// Single geometry-invalidation seam for qualified and raw-only deferred
    /// wrap. A qualified token carries its stable source ID; raw-only c59
    /// state has deliberately discarded that provenance, so its caller must
    /// classify the saved/current cursor row before geometry mutates it.
    fn invalidate_pending_for_geometry(
        pending: &mut Option<PendingWrap>,
        untracked_wrap_pending: &mut bool,
        source_invalidated: impl Fn(RowId) -> bool,
        raw_source_invalidated: bool,
    ) {
        let qualified_source_invalidated = pending
            .as_ref()
            .is_some_and(|pending| source_invalidated(pending.source));
        if qualified_source_invalidated {
            *pending = None;
        }
        if qualified_source_invalidated || raw_source_invalidated {
            *untracked_wrap_pending = false;
        }
    }

    fn raw_wrap_pending(&self) -> bool {
        self.pending.is_some() || self.untracked_wrap_pending
    }

    /// A contract cut on a c59 path which deliberately retains its physical
    /// deferred-wrap Boolean. Motion remains source-faithful, but the raw bit
    /// can never be consumed by the sole qualified A1 creation seam.
    fn cut_sequential_preserving_raw_wrap(&mut self) {
        let raw_wrap_pending = self.raw_wrap_pending();
        self.pending = None;
        self.untracked_wrap_pending = raw_wrap_pending;
        self.sequential = None;
        self.cursor_continuity.advance();
    }

    /// CSI S/T retain c59's raw Boolean without moving the cursor. Preserve a
    /// still-valid unaffected token, but demote it if row topology made its
    /// source no longer the current unchanged row.
    fn reconcile_raw_preserving_topology(&mut self, raw_wrap_pending: bool) {
        if !raw_wrap_pending {
            return;
        }
        let qualified = self.pending.as_ref().is_some_and(|pending| {
            pending.alt_epoch.is_none()
                && pending.valid_width == self.cols
                && self.cursor_continuity.matches(pending.cursor_continuity)
                && self.visible[self.cursor_y as usize].id == Some(pending.source)
                && self.visible[self.cursor_y as usize]
                    .content_revision
                    .matches(pending.source_revision)
        });
        if !qualified {
            self.pending = None;
            self.untracked_wrap_pending = true;
            self.sequential = None;
            self.cursor_continuity.advance();
        }
    }

    /// Direct row helpers preserve A2/A9 continuation when their cut is
    /// wholly outside the edge. If topology validation severed or separated
    /// the named endpoints, authorization must fail closed with that edge.
    fn reconcile_sequential_after_topology(&mut self) {
        let valid = self.sequential.is_some_and(|sequential| {
            self.cursor_continuity.matches(sequential.cursor_continuity)
                && self
                    .validated_outgoing(sequential.source, sequential.target)
                    .is_some()
        });
        if !valid {
            self.sequential = None;
        }
    }

    fn explicit_cr(&mut self) {
        self.cursor_x = 0;
        self.cancel_sequential();
    }

    fn explicit_lf(&mut self) {
        self.cancel_sequential();
        if self.cursor_y == self.scroll_bottom {
            self.scroll_up(false);
        } else if self.cursor_y < self.rows - 1 {
            self.cursor_y += 1;
        }
    }

    fn index(&mut self) {
        self.explicit_lf();
    }

    fn next_line(&mut self) {
        self.cursor_x = 0;
        self.explicit_lf();
    }

    fn reverse_index(&mut self) {
        // performer.rs RI moves/scrolls without clearing raw wrap_pending.
        self.cut_sequential_preserving_raw_wrap();
        if self.cursor_y == self.scroll_top {
            self.scroll_down();
        } else if self.cursor_y > 0 {
            self.cursor_y -= 1;
        }
    }

    // RULE A2 + A7 + A16: all three saved-cursor syntaxes share this
    // lifecycle model, matching the product's shared save/restore path. Saving
    // copies the qualified pending token. Restoring is itself explicit cursor
    // repositioning, so continuity advances before the copied token is
    // considered and the stale token cannot re-arm.
    fn save_cursor(&mut self) {
        self.save_cursor_with_alt_epoch(None, false);
    }

    fn save_cursor_with_alt_epoch(&mut self, alt_epoch: Option<u64>, saved_by_alt_1049: bool) {
        let mut pending = self.pending.clone();
        if let (Some(pending), Some(epoch)) = (&mut pending, alt_epoch) {
            pending.alt_epoch = Some(epoch);
        }
        let mut sequential = alt_epoch.and(self.sequential);
        if let (Some(sequential), Some(epoch)) = (&mut sequential, alt_epoch) {
            sequential.alt_epoch = Some(epoch);
        }
        self.saved_cursor = Some(SavedCursor {
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
            pending,
            untracked_wrap_pending: self.untracked_wrap_pending,
            sequential,
            cursor_continuity: self.cursor_continuity,
            style: self.current_style,
            autowrap: self.autowrap,
            origin_mode: self.origin_mode,
            alt_epoch,
            saved_by_alt_1049,
        });
    }

    fn restore_cursor(&mut self) {
        let Some(saved) = self.saved_cursor.clone() else {
            return;
        };
        let raw_wrap_pending = saved.pending.is_some() || saved.untracked_wrap_pending;
        self.cursor_x = saved.cursor_x.min(self.cols - 1);
        self.cursor_y = saved.cursor_y.min(self.rows - 1);
        self.current_style = saved.style;
        self.autowrap = saved.autowrap;
        self.origin_mode = saved.origin_mode;
        self.cancel_sequential();
        self.pending = saved.pending.filter(|pending| {
            pending.alt_epoch.is_none()
                && self.cursor_continuity.matches(pending.cursor_continuity)
                && pending.valid_width == self.cols
                && self.visible[self.cursor_y as usize].id == Some(pending.source)
                && self.visible[self.cursor_y as usize]
                    .content_revision
                    .matches(pending.source_revision)
        });
        // Real c59 restores its raw Boolean even though A2/A16 reject the
        // copied provenance after this explicit same-domain reposition. Keep
        // the physical deferred motion, but make it structurally no-edge.
        self.untracked_wrap_pending = raw_wrap_pending && self.pending.is_none();
    }

    fn backspace(&mut self) {
        self.cursor_x = self.cursor_x.saturating_sub(1);
        self.cancel_sequential();
    }

    fn horizontal_tab(&mut self) {
        self.cursor_x = ((self.cursor_x + 1)..self.cols)
            .find(|column| self.tab_stops[*column as usize])
            .unwrap_or(self.cols - 1);
        self.cancel_sequential();
    }

    fn horizontal_tab_set(&mut self) {
        self.tab_stops[self.cursor_x as usize] = true;
    }

    fn tab_clear(&mut self, mode: u16) {
        match mode {
            0 => self.tab_stops[self.cursor_x as usize] = false,
            3 => self.tab_stops.fill(false),
            _ => {}
        }
    }

    // RULE A2 + A7 + A16: ordinary CSI A/B/C/D/E/F/G and VPA are explicit
    // reposition surfaces even when clamping leaves a coordinate unchanged.
    // They all cancel pending construction and sequential target authorization.
    fn cursor_up(&mut self, count: u16) {
        let top = if self.cursor_y >= self.scroll_top {
            self.scroll_top
        } else {
            0
        };
        self.cursor_y = self.cursor_y.saturating_sub(count.max(1)).max(top);
        self.cancel_sequential();
    }

    fn cursor_down(&mut self, count: u16) {
        let bottom = if self.cursor_y <= self.scroll_bottom {
            self.scroll_bottom
        } else {
            self.rows - 1
        };
        self.cursor_y = self.cursor_y.saturating_add(count.max(1)).min(bottom);
        self.cancel_sequential();
    }

    fn cursor_forward(&mut self, count: u16) {
        self.cursor_x = self
            .cursor_x
            .saturating_add(count.max(1))
            .min(self.cols - 1);
        self.cancel_sequential();
    }

    fn cursor_back(&mut self, count: u16) {
        self.cursor_x = self.cursor_x.saturating_sub(count.max(1));
        self.cancel_sequential();
    }

    fn cursor_next_line(&mut self, count: u16) {
        self.cursor_x = 0;
        let bottom = if self.cursor_y <= self.scroll_bottom {
            self.scroll_bottom
        } else {
            self.rows - 1
        };
        self.cursor_y = self.cursor_y.saturating_add(count.max(1)).min(bottom);
        self.cancel_sequential();
    }

    fn cursor_prev_line(&mut self, count: u16) {
        self.cursor_x = 0;
        let top = if self.cursor_y >= self.scroll_top {
            self.scroll_top
        } else {
            0
        };
        self.cursor_y = self.cursor_y.saturating_sub(count.max(1)).max(top);
        self.cancel_sequential();
    }

    fn cursor_horizontal_absolute(&mut self, col: u16) {
        self.cursor_x = col.saturating_sub(1).min(self.cols - 1);
        self.cancel_sequential();
    }

    fn cursor_vertical_absolute(&mut self, row: u16) {
        self.cursor_y = if self.origin_mode {
            self.scroll_top
                .saturating_add(row.saturating_sub(1))
                .min(self.scroll_bottom)
        } else {
            row.saturating_sub(1).min(self.rows - 1)
        };
        self.cancel_sequential();
    }

    fn origin_mode(&mut self, enabled: bool) {
        self.origin_mode = enabled;
        if enabled {
            self.cursor_x = 0;
            self.cursor_y = self.scroll_top;
            self.cancel_sequential();
        }
    }

    fn cup(&mut self, row: u16, col: u16) {
        self.cursor_y = if self.origin_mode {
            self.scroll_top
                .saturating_add(row.saturating_sub(1))
                .min(self.scroll_bottom)
        } else {
            row.saturating_sub(1).min(self.rows - 1)
        };
        self.cursor_x = col.saturating_sub(1).min(self.cols - 1);
        self.cancel_sequential();
    }

    fn prepare_addressed_mutation(&mut self) -> (usize, usize) {
        // ED/EL/ECH/DCH/ICH write without clearing c59's raw wrap_pending.
        // The mutation cuts qualified construction, so retain only raw motion.
        self.cut_sequential_preserving_raw_wrap();
        let y = self.cursor_y as usize;
        let x = self.cursor_x as usize;
        let row_id = self.visible[y].id;
        self.before_content_write(row_id, false);
        (x, y)
    }

    fn erase_display(&mut self, mode: u16) {
        if mode == 3 {
            self.clear_history("A11: ED3 destroyed endpoint");
            return;
        }
        if !matches!(mode, 0..=2) {
            return;
        }
        let x = self.cursor_x as usize;
        let y = self.cursor_y as usize;
        let blank = Cell::styled_blank(self.current_style.blank_cell());
        let ranges: Vec<(usize, usize, usize)> = match mode {
            0 => (y..self.rows as usize)
                .map(|row| (row, if row == y { x } else { 0 }, self.cols as usize))
                .collect(),
            1 => (0..=y)
                .map(|row| (row, 0, if row == y { x + 1 } else { self.cols as usize }))
                .collect(),
            2 => (0..self.rows as usize)
                .map(|row| (row, 0, self.cols as usize))
                .collect(),
            _ => unreachable!(),
        };
        self.cut_sequential_preserving_raw_wrap();
        for (row, start, end) in ranges {
            let row_id = self.visible[row].id;
            self.before_content_write(row_id, false);
            self.fixup_wide_char_at(start, row);
            if end < self.cols as usize {
                self.fixup_wide_char_at(end, row);
            }
            self.visible[row].cells[start..end].fill(blank.clone());
        }
    }

    fn erase_line(&mut self, mode: u16) {
        if !matches!(mode, 0..=2) {
            return;
        }
        let (x, y) = self.prepare_addressed_mutation();
        let (start, end) = match mode {
            0 => (x, self.cols as usize),
            1 => (0, x + 1),
            2 => (0, self.cols as usize),
            _ => unreachable!(),
        };
        self.fixup_wide_char_at(start, y);
        if end < self.cols as usize {
            self.fixup_wide_char_at(end, y);
        }
        self.visible[y].cells[start..end].fill(Cell::styled_blank(self.current_style.blank_cell()));
    }

    fn erase_chars(&mut self, count: u16) {
        let (x, y) = self.prepare_addressed_mutation();
        let end = (x + count.max(1) as usize).min(self.cols as usize);
        self.fixup_wide_char_at(x, y);
        if end < self.cols as usize {
            self.fixup_wide_char_at(end, y);
        }
        self.visible[y].cells[x..end].fill(Cell::styled_blank(self.current_style.blank_cell()));
    }

    fn delete_chars(&mut self, count: u16) {
        let (x, y) = self.prepare_addressed_mutation();
        let current_style = self.current_style;
        let count = (count.max(1) as usize).min(self.cols as usize - x);
        self.fixup_wide_char_at(x, y);
        self.visible[y].cells.drain(x..x + count);
        self.visible[y].cells.extend(
            std::iter::repeat(Cell::styled_blank(self.current_style.blank_cell())).take(count),
        );
        if self.visible[y].cells[x].display_width == 0 {
            self.visible[y].cells[x].bce_blank_base_preserving_combining(current_style);
            if x > 0 && self.visible[y].cells[x - 1].display_width == 2 {
                self.visible[y].cells[x - 1]
                    .bce_blank_base_preserving_combining(current_style);
            }
        }
    }

    fn insert_chars(&mut self, count: u16) {
        let (x, y) = self.prepare_addressed_mutation();
        let current_style = self.current_style;
        let count = (count.max(1) as usize).min(self.cols as usize - x);
        self.fixup_wide_char_at(x, y);
        self.visible[y].cells.truncate(self.cols as usize - count);
        self.visible[y].cells.splice(
            x..x,
            std::iter::repeat(Cell::styled_blank(self.current_style.blank_cell())).take(count),
        );
        let last = self.cols as usize - 1;
        if self.visible[y].cells[last].display_width == 2 {
            self.visible[y].cells[last]
                .bce_blank_base_preserving_combining(current_style);
        } else if self.visible[y].cells[last].display_width == 0 {
            self.visible[y].cells[last]
                .bce_blank_base_preserving_combining(current_style);
            if last > 0 && self.visible[y].cells[last - 1].display_width == 2 {
                self.visible[y].cells[last - 1]
                    .bce_blank_base_preserving_combining(current_style);
            }
        }
    }

    fn fixup_wide_char_at(&mut self, x: usize, y: usize) {
        if y >= self.rows as usize || x >= self.cols as usize {
            return;
        }
        match self.visible[y].cells[x].display_width {
            2 => {
                if x + 1 < self.cols as usize {
                    self.visible[y].cells[x + 1].blank_base_preserving_combining();
                }
                self.visible[y].cells[x].blank_base_preserving_combining();
            }
            0 if x > 0 => {
                self.visible[y].cells[x - 1].blank_base_preserving_combining();
                self.visible[y].cells[x].blank_base_preserving_combining();
            }
            _ => {}
        }
    }

    /// Match c59 Row::fix_wide_char_orphan_at_boundary before truncation.
    /// A base straddling the new boundary, or a continuation left at the last
    /// retained cell, is repaired with default (not BCE-styled) blanks.
    fn fix_wide_char_orphan_at_boundary(row: &mut Row, new_cols: usize) {
        if new_cols == 0 || row.cells.len() <= new_cols {
            return;
        }
        let last = new_cols - 1;
        if row.cells[last].display_width == 2 {
            row.cells[last].blank_base_preserving_combining();
        } else if last > 0 && row.cells[last].display_width == 0 {
            row.cells[last].blank_base_preserving_combining();
            row.cells[last - 1].blank_base_preserving_combining();
        }
    }

    // RULE A3 + A5 + A9 + A15: movement carries stable rows.  Validation after
    // every topology operation preserves whole-grid/boundary and wholly
    // co-moving pairs, but severs a removed endpoint or broken adjacency.
    fn scroll_up(&mut self, _from_autowrap: bool) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        let removed = self.visible.remove(top);
        if !self.in_alt && top == 0 && self.scrollback_limit > 0 {
            self.history.push_back(removed);
            self.enforce_history_cap();
        } else {
            self.sever_touching(&[removed.id], "A9/A12: scroll topology removed endpoint");
        }
        let blank = self.new_blank_row_with_style(
            self.cols,
            Cell::styled_blank(self.current_style.blank_cell()),
        );
        self.visible.insert(bottom, blank);
        self.validate_all_edges();
    }

    fn scroll_down(&mut self) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        let removed = self.visible.remove(bottom);
        self.sever_touching(&[removed.id], "A9: reverse scroll removed endpoint");
        let blank = self.new_blank_row_with_style(
            self.cols,
            Cell::styled_blank(self.current_style.blank_cell()),
        );
        self.visible.insert(top, blank);
        self.validate_all_edges();
    }

    fn insert_lines(&mut self, count: u16) {
        // The real DECSTBM path ignores IL when the cursor is outside the
        // scrolling region. An ignored command is not a topology cut and does
        // not invalidate construction/continuation state.
        if self.cursor_y < self.scroll_top || self.cursor_y > self.scroll_bottom {
            return;
        }
        self.cancel_sequential();
        let count = count.max(1).min(self.scroll_bottom - self.cursor_y + 1);
        for _ in 0..count {
            let bottom = self.scroll_bottom as usize;
            let removed = self.visible.remove(bottom);
            self.sever_touching(&[removed.id], "A9: IL removed endpoint");
            let blank = self.new_blank_row_with_style(
                self.cols,
                Cell::styled_blank(self.current_style.blank_cell()),
            );
            self.visible.insert(self.cursor_y as usize, blank);
        }
        self.validate_all_edges();
    }

    fn delete_lines(&mut self, count: u16) {
        if self.cursor_y < self.scroll_top || self.cursor_y > self.scroll_bottom {
            return;
        }
        self.cancel_sequential();
        let count = count.max(1).min(self.scroll_bottom - self.cursor_y + 1);
        for _ in 0..count {
            let removed = self.visible.remove(self.cursor_y as usize);
            self.sever_touching(&[removed.id], "A9: DL removed endpoint");
            let blank = self.new_blank_row_with_style(
                self.cols,
                Cell::styled_blank(self.current_style.blank_cell()),
            );
            self.visible.insert(self.scroll_bottom as usize, blank);
        }
        self.validate_all_edges();
    }

    // Direct row-helper surfaces are contract events even though c59 exposes
    // them only below Screen's public byte API. They model an insertion/removal
    // at a specific visible position and the balancing bottom-row mutation.
    fn insert_row(&mut self, row: u16) {
        let raw_wrap_pending = self.raw_wrap_pending();
        let at = row.saturating_sub(1).min(self.rows - 1) as usize;
        let removed = self.visible.pop().expect("visible grid is nonempty");
        self.sever_touching(&[removed.id], "A9: direct row insert removed endpoint");
        let blank = self.new_blank_row_with_style(
            self.cols,
            Cell::styled_blank(self.current_style.blank_cell()),
        );
        self.visible.insert(at, blank);
        self.validate_all_edges();
        self.reconcile_raw_preserving_topology(raw_wrap_pending);
        self.reconcile_sequential_after_topology();
    }

    fn remove_row(&mut self, row: u16) {
        let raw_wrap_pending = self.raw_wrap_pending();
        let at = row.saturating_sub(1).min(self.rows - 1) as usize;
        let removed = self.visible.remove(at);
        self.sever_touching(&[removed.id], "A9: direct row remove destroyed endpoint");
        let blank = self.new_blank_row_with_style(
            self.cols,
            Cell::styled_blank(self.current_style.blank_cell()),
        );
        self.visible.push(blank);
        self.validate_all_edges();
        self.reconcile_raw_preserving_topology(raw_wrap_pending);
        self.reconcile_sequential_after_topology();
    }

    fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        // Apply csi_dispatch's zero defaults before performer.rs:468-480.
        // An out-of-range top must remain invalid; clamping it would invent a
        // region, cursor home, and authorization cut that c59 never performs.
        let top = top.max(1).saturating_sub(1);
        let bottom = if bottom == 0 { self.rows } else { bottom }
            .saturating_sub(1)
            .min(self.rows - 1);
        if top <= bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
            self.cursor_x = 0;
            self.cursor_y = if self.origin_mode { top } else { 0 };
            self.cancel_sequential();
        }
    }

    // RULE A4 + A8: vertical-only migration moves rows and validates A4/A15;
    // any horizontal mutation first severs every edge touching a resized row.
    fn resize(&mut self, cols: u16, rows: u16) {
        let new_cols = cols.max(1);
        let new_rows = rows.max(1);
        let old_rows = self.rows;
        let width_changed = new_cols != self.cols;
        // Grid::resize clears c59's raw Boolean on every call. The contract
        // improves only a still-qualified A2/A16 token at unchanged width
        // whose source row survives. Classify bottom truncation against the
        // pre-resize domain so raw-only state cannot escape via lost identity.
        let bottom_truncated_ids: BTreeSet<RowId> = if (self.in_alt || rows == 0)
            && self.visible.len() > new_rows as usize
        {
            self.visible[new_rows as usize..]
                .iter()
                .filter_map(|row| row.id)
                .collect()
        } else {
            BTreeSet::new()
        };
        Self::invalidate_pending_for_geometry(
            &mut self.pending,
            &mut self.untracked_wrap_pending,
            |source| width_changed || bottom_truncated_ids.contains(&source),
            true,
        );

        if self.in_alt {
            self.resize_visible_height_bottom(new_rows, "A9: active-alt resize removed endpoint");
        } else if rows > old_rows {
            let grow = (new_rows - old_rows) as usize;
            let restore = grow.min(self.history.len());
            let split = self.history.len() - restore;
            let restored: Vec<Row> = self.history.drain(split..).collect();
            self.cursor_y = self.cursor_y.saturating_add(restore as u16);
            self.visible.splice(0..0, restored);
            while self.visible.len() < new_rows as usize {
                let blank = self.new_blank_row(new_cols);
                self.visible.push(blank);
            }
        } else if rows > 0 && rows < old_rows {
            self.resize_main_height(new_rows);
        } else if rows == 0 {
            // Screen::resize evaluates its raw-height guard before
            // Grid::resize sanitizes 0 to 1. No main-shrink migration occurs;
            // Grid::resize then removes excess visible rows from the bottom.
            self.resize_visible_height_bottom(
                new_rows,
                "A9: zero-height sanitized grid resize removed endpoint",
            );
        }

        let touched: Vec<Option<RowId>> = self
            .visible
            .iter()
            .filter(|row| row.width != new_cols)
            .map(|row| row.id)
            .collect();
        if !touched.is_empty() {
            self.sever_touching(&touched, "A8: horizontal resize touched endpoint");
            for row in &mut self.visible {
                if row.width != new_cols {
                    Self::fix_wide_char_orphan_at_boundary(row, new_cols as usize);
                    row.cells.resize(new_cols as usize, Cell::blank());
                    row.width = new_cols;
                    row.content_revision.advance();
                }
            }
            // Width changes invalidate forward authorization even when no
            // pending/raw bit was present at the helper seam.
            self.sequential = None;
            self.cursor_continuity.advance();
        }

        self.cols = new_cols;
        self.rows = new_rows;
        // Grid::resize resets the table even on a same-dimension call.
        self.tab_stops = default_tab_stops(new_cols);
        self.cursor_x = self.cursor_x.min(new_cols - 1);
        self.cursor_y = self.cursor_y.min(new_rows - 1);
        self.scroll_top = 0;
        self.scroll_bottom = new_rows - 1;
        self.validate_all_edges();
    }

    fn resize_visible_height_bottom(&mut self, new_rows: u16, reason: &'static str) {
        if self.visible.len() > new_rows as usize {
            let doomed: Vec<Option<RowId>> = self.visible[new_rows as usize..]
                .iter()
                .map(|row| row.id)
                .collect();
            self.sever_touching(&doomed, reason);
            self.visible.truncate(new_rows as usize);
        }
        while self.visible.len() < new_rows as usize {
            let blank = self.new_blank_row(self.cols);
            self.visible.push(blank);
        }
    }

    fn resize_main_height(&mut self, new_rows: u16) {
        let mut needed = self.visible.len().saturating_sub(new_rows as usize);
        while needed > 0
            && self.visible.len() - 1 > self.cursor_y as usize
            && self.visible.last().is_some_and(Row::is_blank)
        {
            let row = self.visible.pop().expect("blank bottom row");
            self.sever_touching(&[row.id], "A9: vertical resize removed blank endpoint");
            needed -= 1;
        }
        let push = needed.min(self.cursor_y as usize);
        for _ in 0..push {
            let row = self.visible.remove(0);
            self.history.push_back(row);
            self.enforce_history_cap();
            self.cursor_y -= 1;
            needed -= 1;
        }
        while needed > 0 {
            let row = self.visible.pop().expect("vertical shrink row");
            self.sever_touching(&[row.id], "A9: vertical resize removed endpoint");
            needed -= 1;
        }
    }

    fn enter_alt(&mut self, save_shared_cursor: bool) {
        if self.in_alt {
            return;
        }
        // Every accepted entry receives a one-shot identity. Mode 1049 stamps
        // its shared saved-cursor snapshot with that identity; 47/1047 do not
        // touch the shared snapshot at all.
        let alt_epoch = self.next_alt_epoch.take_next();
        if save_shared_cursor {
            self.save_cursor_with_alt_epoch(alt_epoch, true);
        }
        let saved_rows = std::mem::take(&mut self.visible);
        self.saved_main = Some(SavedMainRows {
            rows: saved_rows,
            cols: self.cols,
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
            autowrap: self.autowrap,
            origin_mode: self.origin_mode,
            alt_epoch,
            entered_with_shared_cursor_save: save_shared_cursor,
        });
        for _ in 0..self.rows {
            let blank = self.new_blank_row(self.cols);
            self.visible.push(blank);
        }
        self.in_alt = true;
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.pending = None;
        self.untracked_wrap_pending = false;
        self.sequential = None;
        self.cursor_continuity.advance();
    }

    // RULE A6 + A10 + A12: saved rows are retained, never recreated.  Before
    // restore, width mismatch severs A8 touching every saved row; bottom-trim
    // IDs are severed before pop (no dangling mark); discarded alt rows die.
    // Thus severance structurally dominates the preservation-by-doing-nothing.
    fn exit_alt(&mut self, restore_shared_cursor: bool) {
        if !self.in_alt {
            return;
        }
        let active_cursor_x = self.cursor_x;
        let active_cursor_y = self.cursor_y;
        let active_raw_wrap_pending = self.pending.is_some() || self.untracked_wrap_pending;
        let alt_ids: Vec<Option<RowId>> = self.visible.iter().map(|row| row.id).collect();
        self.sever_touching(&alt_ids, "A12: alt rows discarded at exit");
        self.visible.clear();
        self.in_alt = false;

        let Some(mut saved) = self.saved_main.take() else {
            for _ in 0..self.rows {
                let blank = self.new_blank_row(self.cols);
                self.visible.push(blank);
            }
            self.cursor_x = 0;
            self.cursor_y = 0;
            return;
        };

        let saved_ids: Vec<Option<RowId>> = saved.rows.iter().map(|row| row.id).collect();
        let restore_width_changed = saved.cols != self.cols;
        if restore_width_changed {
            self.sever_touching(&saved_ids, "A8: alt-exit width adjustment touched endpoint");
        }
        let mut restore_trimmed_ids = BTreeSet::new();
        if saved.rows.len() > self.rows as usize {
            let doomed: Vec<Option<RowId>> = saved.rows[self.rows as usize..]
                .iter()
                .map(|row| row.id)
                .collect();
            restore_trimmed_ids.extend(doomed.iter().flatten().copied());
            self.sever_touching(&doomed, "A10: restore-time bottom trim removed endpoint");
            saved.rows.truncate(self.rows as usize);
        }
        // `saved` is temporarily outside `self.saved_main`, so `sever_edge`
        // cannot reach a surviving saved source through `row_mut`. Reconcile
        // its source-owned mark against the now-severed ledger before restore.
        Self::clear_nonlive_outgoing_marks(&self.edges, &mut saved.rows);
        while saved.rows.len() < self.rows as usize {
            saved.rows.push(self.new_blank_row(self.cols));
        }
        for row in &mut saved.rows {
            if row.width != self.cols {
                row.content_revision.advance();
            }
            Self::fix_wide_char_orphan_at_boundary(row, self.cols as usize);
            row.cells.resize(self.cols as usize, Cell::blank());
            row.width = self.cols;
        }
        self.autowrap = saved.autowrap;
        self.origin_mode = saved.origin_mode;
        let restored_ids: BTreeSet<RowId> = saved.rows.iter().filter_map(|row| row.id).collect();
        let saved_alt_epoch = saved.alt_epoch;
        self.visible = saved.rows;
        // Row restore is unconditional, but cursor/pending restoration is
        // selected solely by the EXIT mode, exactly like performer.rs. Start
        // from the active alt cursor and retain only its raw physical pending;
        // its qualified token named a discarded alt row and cannot cross.
        self.cursor_x = active_cursor_x.min(self.cols - 1);
        self.cursor_y = active_cursor_y.min(self.rows - 1);
        self.pending = None;
        self.sequential = None;
        self.untracked_wrap_pending = active_raw_wrap_pending;
        if restore_shared_cursor {
            let snapshot_matches_entry = self.saved_cursor.as_ref().is_some_and(|cursor| {
                saved.entered_with_shared_cursor_save
                    && cursor.saved_by_alt_1049
                    && match saved_alt_epoch {
                        Some(epoch) => cursor.alt_epoch == Some(epoch),
                        None => cursor.alt_epoch.is_none(),
                    }
            });
            if snapshot_matches_entry {
                // This is the exact mode-1049 snapshot created by the entry
                // whose main domain was just restored. Unlike SavedMainRows,
                // it independently carries the real shared cursor state.
                let mut saved_cursor = self.saved_cursor.clone().expect("1049 snapshot match");
                let raw_source_invalidated = restore_width_changed
                    || saved_cursor.cursor_y as usize >= self.rows as usize;
                Self::invalidate_pending_for_geometry(
                    &mut saved_cursor.pending,
                    &mut saved_cursor.untracked_wrap_pending,
                    |source| {
                        restore_width_changed || restore_trimmed_ids.contains(&source)
                    },
                    raw_source_invalidated,
                );
                let raw_wrap_pending =
                    saved_cursor.pending.is_some() || saved_cursor.untracked_wrap_pending;
                self.cursor_x = saved_cursor.cursor_x.min(self.cols - 1);
                self.cursor_y = saved_cursor.cursor_y.min(self.rows - 1);
                self.current_style = saved_cursor.style;
                self.autowrap = saved_cursor.autowrap;
                self.origin_mode = saved_cursor.origin_mode;
                self.cursor_continuity = saved_cursor.cursor_continuity;
                self.pending = saved_cursor.pending.take().and_then(|mut pending| {
                    (saved_alt_epoch.is_some()
                        && pending.alt_epoch == saved_alt_epoch
                        && pending.valid_width == self.cols
                        && restored_ids.contains(&pending.source)
                        && self.cursor_continuity.matches(pending.cursor_continuity)
                        && self.row(pending.source).is_some_and(|row| {
                            row.content_revision.matches(pending.source_revision)
                        }))
                    .then(|| {
                        pending.alt_epoch = None;
                        pending
                    })
                });
                self.sequential = saved_cursor.sequential.and_then(|mut sequential| {
                    (saved_alt_epoch.is_some()
                        && sequential.alt_epoch == saved_alt_epoch
                        && self.cursor_continuity.matches(sequential.cursor_continuity)
                        && self
                            .validated_outgoing(sequential.source, sequential.target)
                            .is_some())
                    .then(|| {
                        sequential.alt_epoch = None;
                        sequential
                    })
                });
                self.untracked_wrap_pending = raw_wrap_pending && self.pending.is_none();
            } else {
                // c59 still loads the independently selected shared snapshot,
                // but the contract treats it as an explicit reposition. An
                // old DECSC/CSI-s/1048 or prior-alt token cannot be reclassified
                // as this entry's suspended-main continuation.
                if let Some(saved_cursor) = &mut self.saved_cursor {
                    let raw_source_invalidated = restore_width_changed
                        || saved_cursor.cursor_y as usize >= self.rows as usize;
                    Self::invalidate_pending_for_geometry(
                        &mut saved_cursor.pending,
                        &mut saved_cursor.untracked_wrap_pending,
                        |source| {
                            restore_width_changed || restore_trimmed_ids.contains(&source)
                        },
                        raw_source_invalidated,
                    );
                }
                self.restore_cursor();
            }
        }
        self.scroll_top = saved.scroll_top.min(self.rows - 1);
        self.scroll_bottom = saved.scroll_bottom.min(self.rows - 1).max(self.scroll_top);
        self.validate_all_edges();
    }

    fn clear_nonlive_outgoing_marks(edges: &BTreeMap<EdgeId, EdgeRecord>, rows: &mut [Row]) {
        for row in rows {
            if row.outgoing.is_some_and(|edge_id| {
                edges
                    .get(&edge_id)
                    .map_or(true, |edge| edge.disposition != EdgeDisposition::Live)
            }) {
                row.outgoing = None;
            }
        }
    }

    fn decaln(&mut self) {
        self.cancel_sequential();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.cursor_x = 0;
        self.cursor_y = 0;
        let touched: Vec<Option<RowId>> = self.visible.iter().map(|row| row.id).collect();
        self.sever_touching(&touched, "A7: DECALN content mutation touched endpoint");
        for row in &mut self.visible {
            row.content_revision.advance();
            row.cells.fill(Cell::glyph('E', 1, CellStyle::default()));
        }
    }

    // RULE A11: row destruction from ED3/cap/RIS/bulk clear severs both source
    // and target references before removal, independent of geometry.
    fn clear_history(&mut self, reason: &'static str) {
        let doomed: Vec<Option<RowId>> = self.history.iter().map(|row| row.id).collect();
        self.sever_touching(&doomed, reason);
        self.history.clear();
        self.validate_all_edges();
    }

    fn enforce_history_cap(&mut self) {
        while self.history.len() > self.scrollback_limit {
            let row = self.history.pop_front().expect("history over cap");
            self.sever_touching(&[row.id], "A11: scrollback cap evicted endpoint");
        }
    }

    // RULE A12: RIS during alt discards saved main unrestored as well as active
    // alt rows.  All rows die with their edges; fresh rows default to no edge.
    fn ris(&mut self) {
        let mut doomed: Vec<Option<RowId>> = self.history.iter().map(|row| row.id).collect();
        doomed.extend(self.visible.iter().map(|row| row.id));
        if let Some(saved) = &self.saved_main {
            doomed.extend(saved.rows.iter().map(|row| row.id));
        }
        self.sever_touching(&doomed, "A11/A12: RIS destroyed row domain");
        self.history.clear();
        self.visible.clear();
        self.saved_main = None;
        self.saved_cursor = None;
        self.in_alt = false;
        for _ in 0..self.rows {
            let blank = self.new_blank_row(self.cols);
            self.visible.push(blank);
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.autowrap = true;
        self.origin_mode = false;
        self.current_style = CellStyle::default();
        self.tab_stops = default_tab_stops(self.cols);
        self.last_printed_char = ' ';
        self.cancel_sequential();
    }

    fn sever_touching(&mut self, row_ids: &[Option<RowId>], reason: &'static str) {
        let ids: BTreeSet<RowId> = row_ids.iter().copied().flatten().collect();
        let doomed_edges: Vec<EdgeId> = self
            .edges
            .values()
            .filter(|edge| {
                edge.disposition == EdgeDisposition::Live
                    && (ids.contains(&edge.source) || ids.contains(&edge.target))
            })
            .map(|edge| edge.id)
            .collect();
        for edge_id in doomed_edges {
            self.sever_edge(edge_id, reason);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|p| ids.contains(&p.source))
        {
            self.pending = None;
        }
        if self.sequential.is_some_and(|sequential| {
            ids.contains(&sequential.source) || ids.contains(&sequential.target)
        }) {
            self.sequential = None;
        }
    }

    fn sever_edge(&mut self, edge_id: EdgeId, reason: &'static str) {
        let Some(edge) = self.edges.get_mut(&edge_id) else {
            return;
        };
        if edge.disposition != EdgeDisposition::Live {
            return;
        }
        edge.disposition = EdgeDisposition::Severed(reason);
        let source = edge.source;
        if self.row(source).and_then(|row| row.outgoing) == Some(edge_id) {
            self.row_mut(source).expect("live edge source").outgoing = None;
        }
    }

    // RULE A3/A4/A5/A6/A8/A9/A10/A15 validation backstop: preservation is
    // possible only while both stable IDs remain immediate neighbors at their
    // creation width.  Any failure monotonically severs; nothing resurrects.
    fn validate_all_edges(&mut self) {
        let live: Vec<EdgeId> = self
            .edges
            .values()
            .filter(|edge| edge.disposition == EdgeDisposition::Live)
            .map(|edge| edge.id)
            .collect();
        for edge_id in live {
            let edge = self.edges.get(&edge_id).expect("live edge").clone();
            let valid = self
                .validated_outgoing(edge.source, edge.target)
                .is_some_and(|validated| validated.id == edge.id);
            if !valid {
                self.sever_edge(
                    edge_id,
                    "A8/A9: endpoint identity, width, or adjacency invalid",
                );
            }
        }
    }
}

fn layout_at_display_width(logical: &str, width: u16) -> Vec<String> {
    if logical.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut columns = 0u16;
    for ch in logical.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if char_width > 0 && columns > 0 && columns + char_width > width {
            lines.push(std::mem::take(&mut current));
            columns = 0;
        }
        current.push(ch);
        columns += char_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn line_is_omittable_trailing_blank(line: &CandidateHistoryChunk) -> bool {
    cells_are_omittable_trailing_blank(&line.cells)
}

fn cells_are_omittable_trailing_blank(cells: &[CandidateCell]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|cell| {
            cell.ch == ' '
                && cell.display_width == 1
                && cell.combining.is_empty()
                && cell.style == CellStyle::default()
                && !cell.wide_early_padding
        })
}

fn write_style_with_reset(style: CellStyle, out: &mut Vec<u8>) {
    if style == CellStyle::default() {
        out.extend_from_slice(b"\x1b[0m");
        return;
    }
    let mut params = vec!["0".to_string()];
    if style.bold {
        params.push("1".into());
    }
    if style.dim {
        params.push("2".into());
    }
    if style.italic {
        params.push("3".into());
    }
    match style.underline {
        CellUnderline::None => {}
        CellUnderline::Single => params.push("4".into()),
        CellUnderline::Double => params.push("4:2".into()),
        CellUnderline::Curly => params.push("4:3".into()),
        CellUnderline::Dotted => params.push("4:4".into()),
        CellUnderline::Dashed => params.push("4:5".into()),
    }
    if style.blink {
        params.push("5".into());
    }
    if style.inverse {
        params.push("7".into());
    }
    if style.hidden {
        params.push("8".into());
    }
    if style.strikethrough {
        params.push("9".into());
    }
    push_color_param(&mut params, style.fg, 30, 90, 38);
    push_color_param(&mut params, style.bg, 40, 100, 48);
    push_color_param(&mut params, style.underline_color, 0, 0, 58);
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(params.join(";").as_bytes());
    out.push(b'm');
}

fn push_color_param(
    params: &mut Vec<String>,
    color: Option<CellColor>,
    base: u16,
    bright_base: u16,
    extended: u16,
) {
    match color {
        Some(CellColor::Indexed(index)) if extended != 58 && index < 8 => {
            params.push((base + u16::from(index)).to_string());
        }
        Some(CellColor::Indexed(index)) if extended != 58 && index < 16 => {
            params.push((bright_base + u16::from(index - 8)).to_string());
        }
        Some(CellColor::Indexed(index)) => {
            params.extend([extended.to_string(), "5".into(), index.to_string()]);
        }
        Some(CellColor::Rgb(r, g, b)) => {
            params.extend([
                extended.to_string(),
                "2".into(),
                r.to_string(),
                g.to_string(),
                b.to_string(),
            ]);
        }
        None => {}
    }
}

pub fn parse_fixture(input: &str) -> Vec<Workload> {
    let mut workloads = Vec::new();
    let mut current: Option<Workload> = None;

    for (line_no, raw) in input.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let command = parts.next().expect("nonempty fixture line");
        match command {
            "case" => {
                assert!(current.is_none(), "nested case at line {}", line_no + 1);
                let name = parts.next().expect("case name").to_string();
                current = Some(Workload {
                    name,
                    cols: 0,
                    rows: 0,
                    scrollback_limit: 0,
                    emit_width: 0,
                    emission_surface: EmissionSurface::AttachReplay,
                    ops: Vec::new(),
                    expected: None,
                });
            }
            "size" => {
                let workload = current.as_mut().expect("size inside case");
                workload.cols = parse_u16(parts.next(), "cols");
                workload.rows = parse_u16(parts.next(), "rows");
                workload.scrollback_limit = parts
                    .next()
                    .expect("scrollback limit")
                    .parse()
                    .expect("numeric scrollback limit");
            }
            "emit" => {
                current.as_mut().expect("emit inside case").emit_width =
                    parse_u16(parts.next(), "emit width");
            }
            "surface" => {
                current
                    .as_mut()
                    .expect("surface inside case")
                    .emission_surface = match parts.next().expect("emission surface") {
                    "attach" => EmissionSurface::AttachReplay,
                    "get-history" => EmissionSurface::GetHistory,
                    other => panic!("invalid emission surface {other}"),
                };
            }
            "print" => {
                let text = line.strip_prefix("print ").expect("print payload");
                current
                    .as_mut()
                    .expect("print inside case")
                    .ops
                    .push(Op::Print(unescape(text)));
            }
            "rep" => {
                let count = match parts.next().expect("REP count/default") {
                    "default" => None,
                    value => Some(
                        value
                            .parse()
                            .unwrap_or_else(|_| panic!("invalid REP count")),
                    ),
                };
                push_op(&mut current, Op::RepeatLast(count));
            }
            "combine" => {
                let text = line.strip_prefix("combine ").expect("combining payload");
                push_op(&mut current, Op::CombiningMark(unescape(text)));
            }
            "style" => push_op(
                &mut current,
                Op::SetStyle(match parts.next().expect("style name") {
                    "default" => CellStyle::default(),
                    "red" => CellStyle::red(),
                    "blue-bg" => CellStyle::blue_background(),
                    other => panic!("invalid style value {other}"),
                }),
            ),
            "cr" => push_op(&mut current, Op::Cr),
            "lf" => push_op(&mut current, Op::Lf),
            "ind" => push_op(&mut current, Op::Index),
            "ri" => push_op(&mut current, Op::ReverseIndex),
            "nel" => push_op(&mut current, Op::NextLine),
            "decsc" => push_op(&mut current, Op::Decsc),
            "decrc" => push_op(&mut current, Op::Decrc),
            "csi-save" => push_op(&mut current, Op::CsiSaveCursor),
            "csi-restore" => push_op(&mut current, Op::CsiRestoreCursor),
            "1048-save" => push_op(&mut current, Op::Mode1048Save),
            "1048-restore" => push_op(&mut current, Op::Mode1048Restore),
            "bs" => push_op(&mut current, Op::Backspace),
            "ht" => push_op(&mut current, Op::HorizontalTab),
            "hts" => push_op(&mut current, Op::HorizontalTabSet),
            "tbc" => push_op(
                &mut current,
                Op::TabClear(parse_u16(parts.next(), "TBC mode")),
            ),
            "csi-a" => push_op(
                &mut current,
                Op::CursorUp(parse_u16(parts.next(), "CSI A count")),
            ),
            "csi-b" => push_op(
                &mut current,
                Op::CursorDown(parse_u16(parts.next(), "CSI B count")),
            ),
            "csi-c" => push_op(
                &mut current,
                Op::CursorForward(parse_u16(parts.next(), "CSI C count")),
            ),
            "csi-d" => push_op(
                &mut current,
                Op::CursorBack(parse_u16(parts.next(), "CSI D count")),
            ),
            "csi-e" => push_op(
                &mut current,
                Op::CursorNextLine(parse_u16(parts.next(), "CSI E count")),
            ),
            "csi-f" => push_op(
                &mut current,
                Op::CursorPrevLine(parse_u16(parts.next(), "CSI F count")),
            ),
            "csi-g" => push_op(
                &mut current,
                Op::CursorHorizontalAbsolute(parse_u16(parts.next(), "CSI G column")),
            ),
            "vpa" => push_op(
                &mut current,
                Op::CursorVerticalAbsolute(parse_u16(parts.next(), "VPA row")),
            ),
            "origin" => push_op(
                &mut current,
                Op::OriginMode(match parts.next().expect("origin on/off") {
                    "on" => true,
                    "off" => false,
                    other => panic!("invalid origin value {other}"),
                }),
            ),
            "cup" => push_op(
                &mut current,
                Op::Cup {
                    row: parse_u16(parts.next(), "CUP row"),
                    col: parse_u16(parts.next(), "CUP col"),
                },
            ),
            "resize" => push_op(
                &mut current,
                Op::Resize {
                    cols: parse_u16(parts.next(), "resize cols"),
                    rows: parse_u16(parts.next(), "resize rows"),
                },
            ),
            // Historical fixture spelling is retained as an explicit 1049
            // alias; the Op set itself no longer collapses alt modes.
            "alt-enter" | "alt-1049-enter" => push_op(&mut current, Op::EnterAlt1049),
            "alt-exit" | "alt-1049-exit" => push_op(&mut current, Op::ExitAlt1049),
            "alt-47-enter" => push_op(&mut current, Op::EnterAlt47),
            "alt-47-exit" => push_op(&mut current, Op::ExitAlt47),
            "alt-1047-enter" => push_op(&mut current, Op::EnterAlt1047),
            "alt-1047-exit" => push_op(&mut current, Op::ExitAlt1047),
            "ed3" => push_op(&mut current, Op::Ed3),
            "clear-scrollback" => push_op(&mut current, Op::ClearScrollback),
            "ris" => push_op(&mut current, Op::Ris),
            "erase-display" => push_op(
                &mut current,
                Op::EraseDisplay(parse_u16(parts.next(), "ED mode")),
            ),
            "erase-line" => push_op(&mut current, Op::EraseLine(2)),
            "erase-line-mode" => push_op(
                &mut current,
                Op::EraseLine(parse_u16(parts.next(), "EL mode")),
            ),
            "ech" => push_op(
                &mut current,
                Op::EraseChars(parse_u16(parts.next(), "ECH count")),
            ),
            "dch" => push_op(
                &mut current,
                Op::DeleteChars(parse_u16(parts.next(), "DCH count")),
            ),
            "ich" => push_op(
                &mut current,
                Op::InsertChars(parse_u16(parts.next(), "ICH count")),
            ),
            "region" => push_op(
                &mut current,
                Op::SetScrollRegion {
                    top: parse_u16(parts.next(), "region top"),
                    bottom: parse_u16(parts.next(), "region bottom"),
                },
            ),
            "scroll-up" => push_op(
                &mut current,
                Op::ScrollUp(parse_u16(parts.next(), "scroll-up count")),
            ),
            "scroll-down" => push_op(
                &mut current,
                Op::ScrollDown(parse_u16(parts.next(), "scroll-down count")),
            ),
            "insert-lines" => push_op(
                &mut current,
                Op::InsertLines(parse_u16(parts.next(), "IL count")),
            ),
            "delete-lines" => push_op(
                &mut current,
                Op::DeleteLines(parse_u16(parts.next(), "DL count")),
            ),
            "insert-row" => push_op(
                &mut current,
                Op::InsertRow {
                    row: parse_u16(parts.next(), "insert-row position"),
                },
            ),
            "remove-row" => push_op(
                &mut current,
                Op::RemoveRow {
                    row: parse_u16(parts.next(), "remove-row position"),
                },
            ),
            "decaln" => push_op(&mut current, Op::Decaln),
            "decawm" => push_op(
                &mut current,
                Op::Decawm(match parts.next().expect("decawm on/off") {
                    "on" => true,
                    "off" => false,
                    other => panic!("invalid decawm value {other}"),
                }),
            ),
            "expect" => {
                let workload = current.as_mut().expect("expect inside case");
                let live: usize = parts
                    .next()
                    .expect("live count")
                    .parse()
                    .expect("live count");
                let severed: usize = parts
                    .next()
                    .expect("severed count")
                    .parse()
                    .expect("severed count");
                let marker = format!("expect {live} {severed} ");
                let lines = line
                    .strip_prefix(&marker)
                    .expect("expect logical lines")
                    .split('|')
                    .map(unescape)
                    .collect();
                workload.expected = Some(FixtureExpectation {
                    live,
                    severed,
                    logical_lines: lines,
                    outgoing_marks: None,
                });
            }
            "expect-outgoing" => {
                let count: usize = parts
                    .next()
                    .expect("outgoing mark count")
                    .parse()
                    .expect("numeric outgoing mark count");
                current
                    .as_mut()
                    .expect("expect-outgoing inside case")
                    .expected
                    .as_mut()
                    .expect("expect-outgoing follows expect")
                    .outgoing_marks = Some(count);
            }
            "end" => {
                let workload = current.take().expect("end inside case");
                assert!(workload.cols > 0 && workload.rows > 0 && workload.emit_width > 0);
                assert!(
                    workload.expected.is_some(),
                    "fixture case lacks expectation"
                );
                workloads.push(workload);
            }
            other => panic!("unknown fixture command {other:?} at line {}", line_no + 1),
        }
    }
    assert!(current.is_none(), "unterminated fixture case");
    workloads
}

fn parse_u16(value: Option<&str>, label: &str) -> u16 {
    value
        .unwrap_or_else(|| panic!("missing {label}"))
        .parse()
        .unwrap_or_else(|_| panic!("invalid {label}"))
}

fn push_op(current: &mut Option<Workload>, op: Op) {
    current
        .as_mut()
        .expect("operation inside case")
        .ops
        .push(op);
}

fn unescape(value: &str) -> String {
    value.replace("<space>", " ").replace("<pipe>", "|")
}
