//! W1 Phase B2: independent A1--A16 event oracle, G8 fixtures, and corpus.

#[path = "winching/oracle.rs"]
mod oracle;

use oracle::{
    parse_fixture, CandidateCell, CandidateHistoryChunk, CandidateTransportEmission, CellStyle,
    Emission, EmissionSurface, FailClosedGeneration, FixtureExpectation, Op, Oracle, Workload,
    WrapKind,
};
use qrmux::screen::{Screen, TerminalEmulator};

#[derive(Debug)]
struct CurrentComparison {
    name: String,
    oracle_live: usize,
    oracle_severed: usize,
    current_false_breaks: usize,
    legacy_unexpected_advances: usize,
    expected_lines: Vec<String>,
    actual_physical_lines: Vec<String>,
}

impl CurrentComparison {
    fn diverges(&self) -> bool {
        self.current_false_breaks > 0
            || self.legacy_unexpected_advances > 0
            || self.expected_lines != self.actual_physical_lines
    }
}

fn g8_workloads() -> Vec<Workload> {
    [
        include_str!("fixtures/winching/g8_1_unchanged_alt.w1"),
        include_str!("fixtures/winching/g8_2_horizontal_alt_resize.w1"),
        include_str!("fixtures/winching/g8_3_vertical_trim.w1"),
        include_str!("fixtures/winching/g8_4_history_saved_boundary.w1"),
    ]
    .into_iter()
    .flat_map(parse_fixture)
    .collect()
}

fn r1_workloads() -> Vec<Workload> {
    [
        include_str!("fixtures/winching/r1_counterexamples.w1"),
        include_str!("fixtures/winching/r1_named_surfaces.w1"),
    ]
    .into_iter()
    .flat_map(parse_fixture)
    .collect()
}

fn r2_workloads() -> Vec<Workload> {
    parse_fixture(include_str!(
        "fixtures/winching/r2_findings_and_completeness.w1"
    ))
}

fn r6_workloads() -> Vec<Workload> {
    parse_fixture(include_str!("fixtures/winching/r6_oracle_fidelity.w1"))
}

fn r7_workloads() -> Vec<Workload> {
    parse_fixture(include_str!("fixtures/winching/r7_c59_fidelity.w1"))
}

fn r8_workloads() -> Vec<Workload> {
    parse_fixture(include_str!("fixtures/winching/r8_alt_epoch_rep_wide.w1"))
}

fn r9_workloads() -> Vec<Workload> {
    parse_fixture(include_str!(
        "fixtures/winching/r9_alt_exit_and_zero_resize.w1"
    ))
}

fn r11_workloads() -> Vec<Workload> {
    parse_fixture(include_str!("fixtures/winching/r11_tab_stops_and_ris.w1"))
}

fn r12_workloads() -> Vec<Workload> {
    parse_fixture(include_str!(
        "fixtures/winching/r12_fail_closed_and_resize.w1"
    ))
}

fn r14_workloads() -> Vec<Workload> {
    parse_fixture(include_str!(
        "fixtures/winching/r14_render_fidelity.w1"
    ))
}

fn r15_workloads() -> Vec<Workload> {
    parse_fixture(include_str!(
        "fixtures/winching/r15_combining_shrink_chop.w1"
    ))
}

fn r16_workloads() -> Vec<Workload> {
    parse_fixture(include_str!(
        "fixtures/winching/r16_dch_ich_bce_repair.w1"
    ))
}

fn r17_workloads() -> Vec<Workload> {
    parse_fixture(include_str!(
        "fixtures/winching/r17_preservation_fidelity.w1"
    ))
}

fn r18_workloads() -> Vec<Workload> {
    parse_fixture(include_str!(
        "fixtures/winching/r18_geometry_raw_invalidation.w1"
    ))
}

fn expected(live: usize, severed: usize, lines: &[&str]) -> FixtureExpectation {
    FixtureExpectation {
        live,
        severed,
        logical_lines: lines.iter().map(|line| (*line).to_string()).collect(),
        outgoing_marks: None,
    }
}

fn workload(
    name: &str,
    cols: u16,
    rows: u16,
    scrollback_limit: usize,
    emit_width: u16,
    ops: Vec<Op>,
    expectation: FixtureExpectation,
) -> Workload {
    Workload {
        name: name.to_string(),
        cols,
        rows,
        scrollback_limit,
        emit_width,
        emission_surface: EmissionSurface::AttachReplay,
        ops,
        expected: Some(expectation),
    }
}

fn corpus_workloads() -> Vec<Workload> {
    let mut workloads = g8_workloads();
    workloads.extend(r1_workloads());
    workloads.extend(r2_workloads());
    workloads.extend(r6_workloads());
    workloads.extend(r7_workloads());
    workloads.extend(r8_workloads());
    workloads.extend(r9_workloads());
    workloads.extend(r11_workloads());
    workloads.extend(r12_workloads());
    workloads.extend(r14_workloads());
    workloads.extend(r15_workloads());
    workloads.extend(r16_workloads());
    workloads.extend(r17_workloads());
    workloads.extend(r18_workloads());
    workloads.extend([
        workload(
            "litmus_width5_emit10",
            5,
            2,
            100,
            10,
            vec![Op::Print("ABCDEFGHIJ".into()), Op::Lf, Op::Lf],
            expected(1, 0, &["ABCDEFGHIJ"]),
        ),
        workload(
            "phase_a_width_change",
            5,
            3,
            100,
            10,
            vec![
                Op::Print("ABCDEFGHIJ".into()),
                Op::EnterAlt1049,
                Op::Resize { cols: 3, rows: 3 },
                Op::ExitAlt1049,
                Op::Print("Z".into()),
            ],
            expected(0, 1, &["ABC", "FGZ"]),
        ),
        workload(
            "phase_a_bottom_trim",
            5,
            4,
            100,
            10,
            vec![
                Op::Print("ABCDEFGHIJ".into()),
                Op::EnterAlt1049,
                Op::Resize { cols: 5, rows: 1 },
                Op::ExitAlt1049,
                Op::Print("Z".into()),
            ],
            expected(0, 1, &["ABCDZ"]),
        ),
        workload(
            "phase_a_width_change_alt_epoch_exhausted",
            5,
            3,
            100,
            10,
            vec![
                Op::Print("ABCDEFGHIJ".into()),
                Op::EnterAlt1049,
                Op::Resize { cols: 3, rows: 3 },
                Op::ExitAlt1049,
                Op::Print("Z".into()),
            ],
            expected(0, 1, &["ABC", "FGZ"]),
        ),
        workload(
            "phase_a_bottom_trim_alt_epoch_exhausted",
            5,
            4,
            100,
            10,
            vec![
                Op::Print("ABCDEFGHIJ".into()),
                Op::EnterAlt1049,
                Op::Resize { cols: 5, rows: 1 },
                Op::ExitAlt1049,
                Op::Print("Z".into()),
            ],
            expected(0, 1, &["ABCDZ"]),
        ),
        workload(
            "tui_vim_exit_unresized",
            8,
            3,
            100,
            16,
            vec![
                Op::Print("long-main-line".into()),
                Op::EnterAlt1049,
                Op::Print("VIM".into()),
                Op::ExitAlt1049,
            ],
            expected(1, 0, &["long-main-line"]),
        ),
        workload(
            "tui_vim_exit_horizontal_resize",
            8,
            3,
            100,
            16,
            vec![
                Op::Print("long-main-line".into()),
                Op::EnterAlt1049,
                Op::Print("VIM".into()),
                Op::Resize { cols: 5, rows: 3 },
                Op::ExitAlt1049,
            ],
            expected(0, 1, &["long-", "n-lin"]),
        ),
        workload(
            "scrollback_push_then_reattach_new_width",
            5,
            2,
            100,
            10,
            vec![
                Op::Print("ABCDEFGHIJ".into()),
                Op::Lf,
                Op::Lf,
                Op::Resize { cols: 10, rows: 2 },
            ],
            expected(1, 0, &["ABCDEFGHIJ"]),
        ),
        workload(
            "tui_ed3_during_alt",
            5,
            2,
            100,
            10,
            vec![
                Op::Print("ABCDEF".into()),
                Op::Lf,
                Op::EnterAlt1049,
                Op::Ed3,
                Op::ExitAlt1049,
            ],
            expected(0, 1, &["F"]),
        ),
        workload(
            "tui_ris_during_alt",
            5,
            2,
            100,
            10,
            vec![Op::Print("ABCDEF".into()), Op::EnterAlt1049, Op::Ris],
            expected(0, 1, &[]),
        ),
        workload(
            "partial_region_co_move",
            5,
            4,
            100,
            10,
            vec![
                Op::Cup { row: 3, col: 1 },
                Op::Print("ABCDEF".into()),
                Op::SetScrollRegion { top: 2, bottom: 4 },
                Op::ScrollUp(1),
            ],
            expected(1, 0, &["", "ABCDEF"]),
        ),
        workload(
            "partial_region_cut",
            5,
            4,
            100,
            10,
            vec![
                Op::Cup { row: 2, col: 1 },
                Op::Print("ABCDEF".into()),
                Op::SetScrollRegion { top: 2, bottom: 4 },
                Op::ScrollUp(1),
            ],
            expected(0, 1, &["", "F"]),
        ),
        workload(
            "construction_cr_rewrite",
            5,
            3,
            100,
            10,
            vec![Op::Print("ABCDEF".into()), Op::Cr, Op::Print("Z".into())],
            expected(0, 1, &["ABCDE", "Z"]),
        ),
        workload(
            "decawm_off_never_wraps",
            5,
            2,
            100,
            10,
            vec![Op::Decawm(false), Op::Print("ABCDEF".into())],
            expected(0, 0, &["ABCDF"]),
        ),
        workload(
            "decaln_severs_touched_edges",
            5,
            3,
            100,
            10,
            vec![Op::Print("ABCDEF".into()), Op::Decaln],
            expected(0, 1, &["EEEEE", "EEEEE", "EEEEE"]),
        ),
    ]);
    workloads
}

fn assert_expected(workload: &Workload) -> (Oracle, Emission) {
    let mut oracle = Oracle::run(workload);
    let emission = oracle.emission_for(workload.emission_surface, workload.emit_width);
    let expected = workload.expected.as_ref().expect("workload expectation");
    assert_eq!(
        oracle.live_count(),
        expected.live,
        "{} live-edge expectation",
        workload.name
    );
    assert_eq!(
        oracle.severed_count(),
        expected.severed,
        "{} severed-edge expectation; records={:?}",
        workload.name,
        oracle.edge_records()
    );
    assert_eq!(
        emission.logical_lines(),
        expected.logical_lines,
        "{} logical emission",
        workload.name
    );
    if let Some(expected_marks) = expected.outgoing_marks {
        assert_eq!(
            oracle.outgoing_mark_count(),
            expected_marks,
            "{} Row.outgoing mark expectation",
            workload.name
        );
    }
    (oracle, emission)
}

fn apply_screen(screen: &mut Screen, op: &Op) {
    match op {
        Op::Print(text) => screen.process(text.as_bytes()),
        Op::RepeatLast(Some(count)) => screen.process(format!("\x1b[{count}b").as_bytes()),
        Op::RepeatLast(None) => screen.process(b"\x1b[b"),
        Op::SetStyle(style) if *style == CellStyle::default() => screen.process(b"\x1b[0m"),
        Op::SetStyle(style) if *style == CellStyle::red() => screen.process(b"\x1b[31m"),
        Op::SetStyle(style) if *style == CellStyle::blue_background() => {
            screen.process(b"\x1b[44m")
        }
        Op::SetStyle(_) => panic!("legacy corroboration does not encode arbitrary test styles"),
        Op::CombiningMark(text) => screen.process(text.as_bytes()),
        Op::Cr => screen.process(b"\r"),
        Op::Lf => screen.process(b"\n"),
        Op::Index => screen.process(b"\x1bD"),
        Op::ReverseIndex => screen.process(b"\x1bM"),
        Op::NextLine => screen.process(b"\x1bE"),
        Op::Decsc => screen.process(b"\x1b7"),
        Op::Decrc => screen.process(b"\x1b8"),
        Op::CsiSaveCursor => screen.process(b"\x1b[s"),
        Op::CsiRestoreCursor => screen.process(b"\x1b[u"),
        Op::Mode1048Save => screen.process(b"\x1b[?1048h"),
        Op::Mode1048Restore => screen.process(b"\x1b[?1048l"),
        Op::Backspace => screen.process(b"\x08"),
        Op::HorizontalTab => screen.process(b"\t"),
        Op::HorizontalTabSet => screen.process(b"\x1bH"),
        Op::TabClear(mode) => screen.process(format!("\x1b[{mode}g").as_bytes()),
        Op::CursorUp(count) => screen.process(format!("\x1b[{count}A").as_bytes()),
        Op::CursorDown(count) => screen.process(format!("\x1b[{count}B").as_bytes()),
        Op::CursorForward(count) => screen.process(format!("\x1b[{count}C").as_bytes()),
        Op::CursorBack(count) => screen.process(format!("\x1b[{count}D").as_bytes()),
        Op::CursorNextLine(count) => screen.process(format!("\x1b[{count}E").as_bytes()),
        Op::CursorPrevLine(count) => screen.process(format!("\x1b[{count}F").as_bytes()),
        Op::CursorHorizontalAbsolute(col) => screen.process(format!("\x1b[{col}G").as_bytes()),
        Op::CursorVerticalAbsolute(row) => screen.process(format!("\x1b[{row}d").as_bytes()),
        Op::OriginMode(enabled) => screen.process(if *enabled { b"\x1b[?6h" } else { b"\x1b[?6l" }),
        Op::Cup { row, col } => screen.process(format!("\x1b[{row};{col}H").as_bytes()),
        Op::EraseDisplay(mode) => screen.process(format!("\x1b[{mode}J").as_bytes()),
        Op::EraseLine(mode) => screen.process(format!("\x1b[{mode}K").as_bytes()),
        Op::EraseChars(count) => screen.process(format!("\x1b[{count}X").as_bytes()),
        Op::DeleteChars(count) => screen.process(format!("\x1b[{count}P").as_bytes()),
        Op::InsertChars(count) => screen.process(format!("\x1b[{count}@").as_bytes()),
        Op::Resize { cols, rows } => screen.resize(*cols, *rows),
        Op::EnterAlt1049 => screen.process(b"\x1b[?1049h"),
        Op::ExitAlt1049 => screen.process(b"\x1b[?1049l"),
        Op::EnterAlt47 => screen.process(b"\x1b[?47h"),
        Op::ExitAlt47 => screen.process(b"\x1b[?47l"),
        Op::EnterAlt1047 => screen.process(b"\x1b[?1047h"),
        Op::ExitAlt1047 => screen.process(b"\x1b[?1047l"),
        Op::Ed3 => screen.process(b"\x1b[3J"),
        // Screen has no separate public byte/API entry point for the underlying
        // bulk helper. ED3 drives Grid::clear_scrollback for before-evidence;
        // the oracle still models ClearScrollback as its own A11 event.
        Op::ClearScrollback => screen.process(b"\x1b[3J"),
        Op::Ris => screen.process(b"\x1bc"),
        Op::SetScrollRegion { top, bottom } => {
            screen.process(format!("\x1b[{top};{bottom}r").as_bytes())
        }
        Op::ScrollUp(count) => screen.process(format!("\x1b[{count}S").as_bytes()),
        Op::ScrollDown(count) => screen.process(format!("\x1b[{count}T").as_bytes()),
        Op::InsertLines(count) => screen.process(format!("\x1b[{count}L").as_bytes()),
        Op::DeleteLines(count) => screen.process(format!("\x1b[{count}M").as_bytes()),
        // Direct row helpers are private below Screen. IL/DL are their public
        // byte-level callers and provide corroborating c59 behavior only.
        Op::InsertRow { row } => screen.process(format!("\x1b[{row};1H\x1b[1L").as_bytes()),
        Op::RemoveRow { row } => screen.process(format!("\x1b[{row};1H\x1b[1M").as_bytes()),
        Op::Decaln => screen.process(b"\x1b#8"),
        Op::Decawm(enabled) => screen.process(if *enabled { b"\x1b[?7h" } else { b"\x1b[?7l" }),
    }
}

fn actual_lines(screen: &Screen) -> Vec<String> {
    screen
        .get_content_history()
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

fn compare_current(workload: &Workload) -> CurrentComparison {
    // Corroborating c59 "before" evidence only. The legacy Screen has no W2
    // logical-emission surface to submit to the primary A13/A14 referee above.
    let mut oracle = Oracle::new(workload.cols, workload.rows, workload.scrollback_limit);
    let mut screen = Screen::new(workload.cols, workload.rows, workload.scrollback_limit);
    let mut unauthorized_autowraps = 0;

    for op in &workload.ops {
        if let Op::Print(text) = op {
            for ch in text.chars() {
                let before = screen.cursor_position();
                let before_history = screen.scrollback_len();
                let effect = oracle.apply(&Op::Print(ch.to_string()));
                let mut encoded = [0; 4];
                screen.process(ch.encode_utf8(&mut encoded).as_bytes());
                let after = screen.cursor_position();
                let after_history = screen.scrollback_len();
                let actual_advanced_row = after.1 != before.1 || after_history > before_history;
                if actual_advanced_row && effect.created_edges.is_empty() {
                    unauthorized_autowraps += 1;
                }
            }
        } else {
            oracle.apply(op);
            apply_screen(&mut screen, op);
        }
    }

    let emission = oracle.emission_for(workload.emission_surface, workload.emit_width);
    let live = oracle.live_count();
    CurrentComparison {
        name: workload.name.clone(),
        oracle_live: live,
        oracle_severed: oracle.severed_count(),
        // c59efe03 get_content_history/get_history emits one independently
        // rendered Vec per physical row.  It has no edge reader, so every live
        // contract edge is a hard break on this inspected emission surface.
        current_false_breaks: live,
        legacy_unexpected_advances: unauthorized_autowraps,
        expected_lines: emission.logical_lines(),
        actual_physical_lines: actual_lines(&screen),
    }
}

#[test]
fn oracle_self_test_a1_a16_g8_and_phase_a() {
    let fixtures = g8_workloads();
    assert_eq!(
        fixtures.len(),
        13,
        "four alt G8 cells plus the eviction cell must cover both halves"
    );
    for workload in &fixtures {
        assert_expected(workload);
    }

    let phase_a: Vec<Workload> = corpus_workloads()
        .into_iter()
        .filter(|workload| workload.name.starts_with("phase_a_"))
        .collect();
    assert_eq!(phase_a.len(), 4);
    for workload in &phase_a {
        let (mut oracle, emission) = assert_expected(workload);
        assert_eq!(oracle.history_len(), 0, "{} must not create stale history", workload.name);
        assert_eq!(oracle.live_count(), 0, "{} must remain hard-broken", workload.name);
        if workload.name.contains("width_change") {
            assert_eq!(emission.actual.chunks.len(), 2);
            let false_line = merged(
                emission.actual.chunks[0].clone(),
                &emission.actual.chunks[1],
            );
            let verdict = oracle.referee_transport_emission(&candidate(vec![false_line]));
            assert_eq!(verdict.false_joins.len(), 1, "Phase-A gate must stay closed");
            assert!(verdict.corruptions.is_empty());
        }
        let comparison = compare_current(workload);
        assert_eq!(
            comparison.legacy_unexpected_advances, 1,
            "{} must catch c59 bug",
            workload.name
        );
    }

    // Frozen-width storage / emit-width separation: the oracle retains two
    // width-5 physical rows but exposes one logical line laid out at width 10.
    let litmus = corpus_workloads()
        .into_iter()
        .find(|workload| workload.name == "litmus_width5_emit10")
        .expect("litmus workload");
    let (_, emission) = assert_expected(&litmus);
    assert_eq!(emission.logical_lines(), vec!["ABCDEFGHIJ"]);
    assert_eq!(emission.laid_out_lines, vec!["ABCDEFGHIJ"]);
}

fn candidate(chunks: Vec<CandidateHistoryChunk>) -> CandidateTransportEmission {
    CandidateTransportEmission { chunks }
}

fn merged(
    mut first: CandidateHistoryChunk,
    second: &CandidateHistoryChunk,
) -> CandidateHistoryChunk {
    first.cells.extend(second.cells.iter().cloned());
    first.end_of_line = true;
    first
}

#[test]
fn ordered_full_cells_make_physical_row_composition_unique() {
    let mut oracle = Oracle::new(5, 3, 100);
    oracle.apply(&Op::Print("A".into()));
    let correct = oracle.emission(5).actual;
    assert_eq!(correct.chunks.len(), 1);
    assert_eq!(correct.chunks[0].cells.len(), 5);
    assert_eq!(correct.chunks[0].plain_text(), "A");
    let verdict = oracle.referee_transport_emission(&correct);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());

    // Finding 1: consuming r0+r1(blank) is a different ten-cell input, not the
    // same trimmed "A". The deterministic full-row walk must cross r0->r1.
    let mut consumed_blank = correct.chunks[0].clone();
    consumed_blank
        .cells
        .extend(std::iter::repeat_with(CandidateCell::blank).take(5));
    let verdict = oracle.referee_transport_emission(&candidate(vec![consumed_blank]));
    assert_eq!(verdict.checked_joins, 1);
    assert_eq!(verdict.false_joins.len(), 1);

    // An additional empty logical line is also concrete full cells. Canonical
    // tail omission rejects it instead of assigning it one of several blanks.
    let extra_empty = CandidateHistoryChunk {
        cells: std::iter::repeat_with(CandidateCell::blank)
            .take(5)
            .collect(),
        end_of_line: true,
    };
    let verdict = oracle.referee_transport_emission(&candidate(vec![
        correct.chunks[0].clone(),
        extra_empty.clone(),
    ]));
    assert_eq!(verdict.corruptions.len(), 1);

    let mut blank_oracle = Oracle::new(5, 3, 100);
    assert!(blank_oracle.emission(5).actual.chunks.is_empty());
    let verdict = blank_oracle.referee_transport_emission(&candidate(vec![extra_empty]));
    assert_eq!(verdict.corruptions.len(), 1);
}

#[test]
fn styled_cells_are_refereed_before_continuous_ansi_rendering() {
    let red = CellStyle::red();
    let mut joined = Oracle::new(5, 2, 100);
    joined.apply(&Op::SetStyle(red));
    joined.apply(&Op::Print("ABCDEF".into()));
    let correct = joined.emission(5).actual;
    assert_eq!(correct.chunks.len(), 1);
    assert_eq!(correct.chunks[0].render_ansi(), b"\x1b[0;31mABCDEF\x1b[0m");
    let verdict = joined.referee_transport_emission(&correct);
    assert_eq!(verdict.checked_joins, 1);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());

    // Finding 2: the same styled single-pass line over never-joined rows is
    // consumed as styled cells and its forced boundary is rejected. ANSI is
    // generated afterward from those same cells; nothing is stripped.
    let mut unjoined = Oracle::new(5, 2, 100);
    unjoined.apply(&Op::SetStyle(red));
    unjoined.apply(&Op::Print("ABCDE".into()));
    unjoined.apply(&Op::Cr);
    unjoined.apply(&Op::Lf);
    unjoined.apply(&Op::Print("F".into()));
    let hard_break = unjoined.emission(5).actual;
    assert_eq!(hard_break.chunks.len(), 2);
    let false_line = merged(hard_break.chunks[0].clone(), &hard_break.chunks[1]);
    assert_eq!(false_line.render_ansi(), b"\x1b[0;31mABCDEF\x1b[0m");
    let verdict = unjoined.referee_transport_emission(&candidate(vec![false_line]));
    assert_eq!(verdict.checked_joins, 1);
    assert_eq!(verdict.false_joins.len(), 1);
    assert!(verdict.corruptions.is_empty());

    let mut style_lie = correct.clone();
    style_lie.chunks[0].cells[0].style = CellStyle::default();
    let verdict = joined.referee_transport_emission(&style_lie);
    assert_eq!(verdict.corruptions.len(), 1);
}

#[test]
fn hostile_padded_over_under_and_reordered_cell_streams_fail_closed() {
    let litmus = corpus_workloads()
        .into_iter()
        .find(|workload| workload.name == "litmus_width5_emit10")
        .expect("litmus workload");
    let (mut oracle, emission) = assert_expected(&litmus);
    let own_verdict = oracle.referee_transport_emission(&emission.actual);
    assert_eq!(own_verdict.checked_joins, 1);
    assert!(own_verdict.false_joins.is_empty());
    assert!(own_verdict.corruptions.is_empty());

    let no_edge = r1_workloads()
        .into_iter()
        .find(|workload| workload.name == "reviewer_no_edge_cr_lf_rows")
        .expect("reviewer workload");
    let (mut oracle, correct) = assert_expected(&no_edge);
    let correct_verdict = oracle.referee_transport_emission(&correct.actual);
    assert!(correct_verdict.false_joins.is_empty());
    assert!(correct_verdict.corruptions.is_empty());

    // Hostile under/over/reordered ordered-cell outputs for ABC;CR;LF;DEF.
    let under_reported =
        oracle.referee_transport_emission(&candidate(vec![correct.actual.chunks[0].clone()]));
    assert_eq!(under_reported.corruptions.len(), 1);
    assert!(under_reported.false_joins.is_empty());

    let over_reported = oracle.referee_transport_emission(&candidate(vec![
        correct.actual.chunks[0].clone(),
        correct.actual.chunks[1].clone(),
        correct.actual.chunks[1].clone(),
    ]));
    assert_eq!(over_reported.corruptions.len(), 1);
    assert!(over_reported.false_joins.is_empty());

    let false_line = merged(correct.actual.chunks[0].clone(), &correct.actual.chunks[1]);
    let merged_verdict = oracle.referee_transport_emission(&candidate(vec![false_line]));
    assert_eq!(merged_verdict.checked_joins, 1);
    assert_eq!(merged_verdict.false_joins.len(), 1);
    assert!(merged_verdict.corruptions.is_empty());
    assert_eq!(
        merged_verdict.false_joins[0].reason,
        "no live validated A1 edge authorizes this transport-framed cell join"
    );

    let reordered = oracle.referee_transport_emission(&candidate(vec![
        correct.actual.chunks[1].clone(),
        correct.actual.chunks[0].clone(),
    ]));
    assert_eq!(reordered.corruptions.len(), 1);

    // WideEarly padding stays in the pre-render stream with explicit identity;
    // dropping its marker is corruption even though rendered text is unchanged.
    let wide = r1_workloads()
        .into_iter()
        .find(|workload| workload.name == "reviewer_wide_early_abcd界")
        .expect("wide workload");
    let (mut wide_oracle, wide_emission) = assert_expected(&wide);
    assert!(wide_emission.actual.chunks[0]
        .cells
        .iter()
        .any(|cell| cell.wide_early_padding));
    let clean = wide_oracle.referee_transport_emission(&wide_emission.actual);
    assert!(clean.false_joins.is_empty() && clean.corruptions.is_empty());
    let mut padding_lie = wide_emission.actual.clone();
    let padding = padding_lie.chunks[0]
        .cells
        .iter_mut()
        .find(|cell| cell.wide_early_padding)
        .expect("padding cell");
    padding.wide_early_padding = false;
    let verdict = wide_oracle.referee_transport_emission(&padding_lie);
    assert_eq!(verdict.corruptions.len(), 1);
}

#[test]
fn transport_framing_is_the_authoritative_false_join_surface() {
    let mut oracle = Oracle::new(5, 2, 100);
    oracle.apply(&Op::Print("ABC".into()));
    oracle.apply(&Op::Cr);
    oracle.apply(&Op::Lf);
    oracle.apply(&Op::Print("DEF".into()));

    let correct = oracle.emission(5).actual;
    assert_eq!(correct.chunks.len(), 2);
    assert!(correct.chunks.iter().all(|chunk| chunk.end_of_line));
    let correct_verdict = oracle.referee_transport_emission(&correct);
    assert!(correct_verdict.false_joins.is_empty());
    assert!(correct_verdict.corruptions.is_empty());

    assert_eq!(correct.render_ansi(), b"ABC\r\nDEF\r\n");

    // Round-5 counterexample: the cells remain byte-for-byte correct, but the
    // Missing first terminator makes the two row-tail spaces interior to the
    // one client-framed logical line, yielding ABC  DEF CRLF.
    let mut missing_terminator = correct.clone();
    missing_terminator.chunks[0].end_of_line = false;
    assert_eq!(missing_terminator.render_ansi(), b"ABC  DEF\r\n");
    let verdict = oracle.referee_transport_emission(&missing_terminator);
    assert_eq!(verdict.checked_joins, 1);
    assert_eq!(verdict.false_joins.len(), 1);
    assert!(verdict.corruptions.is_empty());

    // An extra terminator after ABC cuts the first full frozen row in half.
    let first = &correct.chunks[0];
    let extra_mid_row = candidate(vec![
        CandidateHistoryChunk {
            cells: first.cells[..3].to_vec(),
            end_of_line: true,
        },
        CandidateHistoryChunk {
            cells: first.cells[3..].to_vec(),
            end_of_line: true,
        },
        correct.chunks[1].clone(),
    ]);
    let verdict = oracle.referee_transport_emission(&extra_mid_row);
    assert!(!verdict.corruptions.is_empty());

    // Reordering correctly terminated transport chunks changes the actual
    // ordered cell stream and fails exact frozen-row matching.
    let reordered = candidate(vec![correct.chunks[1].clone(), correct.chunks[0].clone()]);
    let verdict = oracle.referee_transport_emission(&reordered);
    assert!(!verdict.corruptions.is_empty());
}

#[test]
fn continuity_and_content_generations_exhaust_fail_closed_without_resurrection() {
    let mut cursor_continuity = FailClosedGeneration::at(u64::MAX);
    let saved_cursor_token = cursor_continuity.token().expect("pre-exhaustion token");
    assert!(!cursor_continuity.advance());
    assert!(cursor_continuity.is_exhausted());
    assert!(!cursor_continuity.matches(saved_cursor_token));
    assert!(!cursor_continuity.advance());
    assert!(!cursor_continuity.matches(saved_cursor_token));

    let mut content_revision = FailClosedGeneration::at(u64::MAX);
    let saved_revision_token = content_revision.token().expect("pre-exhaustion token");
    assert!(!content_revision.advance());
    assert!(content_revision.is_exhausted());
    assert!(!content_revision.matches(saved_revision_token));
    assert!(!content_revision.advance());
    assert!(!content_revision.matches(saved_revision_token));
}

#[test]
fn row_and_edge_id_allocators_are_checked_nonwrapping_and_sticky() {
    for label in ["RowId", "EdgeId"] {
        let mut allocator = FailClosedGeneration::at(u64::MAX);
        assert_eq!(
            allocator.take_next(),
            Some(u64::MAX),
            "{label} issues MAX once"
        );
        assert!(allocator.is_exhausted(), "{label} poisons after MAX");
        assert_eq!(allocator.take_next(), None, "{label} never wraps to zero");
        assert_eq!(allocator.take_next(), None, "{label} exhaustion is sticky");
        assert!(!allocator.matches(u64::MAX), "{label} cannot reuse MAX");
        assert!(!allocator.matches(0), "{label} cannot reuse zero");
    }

    let mut exhausted = FailClosedGeneration::at(u64::MAX);
    assert!(!exhausted.advance());

    // RowId exhaustion still allocates the physical blank demanded by scroll,
    // but it has no stable ID and therefore forces a hard boundary even when
    // later printing physically wraps from it to a stable row.
    let mut row_exhausted = Oracle::new(5, 2, 100);
    row_exhausted.set_identity_allocators_for_test(exhausted, FailClosedGeneration::at(1));
    row_exhausted.apply(&Op::ScrollDown(1));
    assert_eq!(row_exhausted.untracked_row_count_for_test(), 1);
    row_exhausted.apply(&Op::Cup { row: 1, col: 1 });
    row_exhausted.apply(&Op::Print("ABCDEF".into()));
    assert_eq!(row_exhausted.live_count(), 0);
    assert!(row_exhausted.edge_records().is_empty());
    assert_eq!(
        row_exhausted.emission(5).logical_lines(),
        vec!["ABCDE", "F"]
    );

    // EdgeId exhaustion preserves the terminal's physical wrap/write while
    // refusing both the ledger record and sequential authorization.
    let mut edge_exhausted = Oracle::new(5, 2, 100);
    edge_exhausted.set_identity_allocators_for_test(FailClosedGeneration::at(3), exhausted);
    edge_exhausted.apply(&Op::Print("ABCDEF".into()));
    assert_eq!(edge_exhausted.live_count(), 0);
    assert!(edge_exhausted.edge_records().is_empty());
    assert_eq!(
        edge_exhausted.emission(5).logical_lines(),
        vec!["ABCDE", "F"]
    );
}

#[test]
fn exhausted_alt_epoch_matched_1049_geometry_clears_raw_motion() {
    let workloads = corpus_workloads();
    for name in [
        "phase_a_width_change_alt_epoch_exhausted",
        "phase_a_bottom_trim_alt_epoch_exhausted",
    ] {
        let workload = workloads
            .iter()
            .find(|workload| workload.name == name)
            .unwrap_or_else(|| panic!("missing exhausted-alt-epoch workload {name}"));
        let mut oracle = Oracle::new(workload.cols, workload.rows, workload.scrollback_limit);
        let mut exhausted = FailClosedGeneration::at(u64::MAX);
        assert!(!exhausted.advance());
        oracle.set_alt_epoch_allocator_for_test(exhausted);
        for op in &workload.ops {
            oracle.apply(op);
        }
        let emission = oracle.emission(workload.emit_width);
        let expected = workload.expected.as_ref().expect("fixture expectation");
        assert_eq!(oracle.live_count(), expected.live, "{name}: live");
        assert_eq!(oracle.severed_count(), expected.severed, "{name}: severed");
        assert_eq!(emission.logical_lines(), expected.logical_lines, "{name}: lines");
        assert_eq!(oracle.history_len(), 0, "{name}: stale wrap must not scroll");
        let verdict = oracle.referee_transport_emission(&emission.actual);
        assert!(verdict.false_joins.is_empty(), "{name}: {verdict:?}");
        assert!(verdict.corruptions.is_empty(), "{name}: {verdict:?}");
    }
}

#[test]
fn round_2_event_counterexamples_and_named_cursor_surfaces_pass() {
    let workloads = r2_workloads();
    for name in [
        "decawm_off_preserves_existing_edge",
        "il_inside_region_cuts_edge",
        "il_outside_region_is_noop",
        "dl_inside_region_cuts_edge",
        "dl_outside_region_is_noop",
        "decsc_decrc_saved_pending_cannot_rearm",
        "csi_s_u_saved_pending_cannot_rearm",
        "mode_1048_saved_pending_cannot_rearm",
        "backspace_target_overwrite_severs",
        "horizontal_tab_target_overwrite_severs",
    ] {
        let workload = workloads
            .iter()
            .find(|workload| workload.name == name)
            .unwrap_or_else(|| panic!("missing round-2 workload {name}"));
        let (mut oracle, emission) = assert_expected(workload);
        assert!(
            {
                let verdict = oracle.referee_transport_emission(&emission.actual);
                verdict.false_joins.is_empty() && verdict.corruptions.is_empty()
            },
            "{name} oracle emission must pass the ordered-cell referee"
        );
    }
}

#[test]
fn construction_edge_is_live_after_every_forward_target_character() {
    let workload = r2_workloads()
        .into_iter()
        .find(|workload| workload.name == "construction_forward_stream_preserves_a16")
        .expect("A16 construction workload");
    let mut oracle = Oracle::new(workload.cols, workload.rows, workload.scrollback_limit);
    for (index, op) in workload.ops.iter().enumerate() {
        oracle.apply(op);
        assert_eq!(
            oracle.live_count(),
            1,
            "A16 edge must remain inspectably live after construction op {index}: {op:?}"
        );
    }
    assert_expected(&workload);
}

#[test]
fn scrollback_cap_evicts_only_doomed_edge_without_dangling_join() {
    let workload = g8_workloads()
        .into_iter()
        .find(|workload| workload.name == "g8_scrollback_cap_evicts_oldest_preserves_later_edges")
        .expect("G8 cap workload");
    let (mut oracle, emission) = assert_expected(&workload);
    assert_eq!(emission.live_edges.len(), 2);
    assert_eq!(oracle.severed_count(), 1);
    assert_eq!(emission.logical_lines(), vec!["FGHIJKLMNOP"]);
    let verdict = oracle.referee_transport_emission(&emission.actual);
    assert_eq!(verdict.checked_joins, 2);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());
}

#[test]
fn restore_time_trim_clears_surviving_source_row_outgoing_mark() {
    let workload = g8_workloads()
        .into_iter()
        .find(|workload| workload.name == "g8_3_trimmed_target_severs_no_dangling")
        .expect("G8 trim workload");
    let mut oracle = Oracle::new(workload.cols, workload.rows, workload.scrollback_limit);
    oracle.apply(&Op::Print("ABCDEF".into()));
    let source = oracle.emission_row_ids()[0];
    assert!(
        oracle.outgoing_mark(source).is_some(),
        "precondition: live source mark"
    );
    oracle.apply(&Op::EnterAlt1049);
    oracle.apply(&Op::Resize { cols: 5, rows: 1 });
    oracle.apply(&Op::ExitAlt1049);
    assert_eq!(
        oracle.outgoing_mark(source),
        None,
        "A10 clears the surviving Row.outgoing"
    );
    assert_expected(&workload);
}

#[test]
fn unchanged_alt_restore_preserves_history_boundary_sequential_authorization() {
    let workload = g8_workloads()
        .into_iter()
        .find(|workload| workload.name == "g8_4_boundary_unchanged_sequential_continues")
        .expect("round-3 boundary sequential workload");
    let (mut oracle, emission) = assert_expected(&workload);
    assert_eq!(oracle.live_count(), 1);
    assert_eq!(emission.logical_lines(), vec!["ABCDEFG"]);
    let verdict = oracle.referee_transport_emission(&emission.actual);
    assert_eq!(verdict.checked_joins, 1);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());
}

#[test]
fn ordinary_csi_cursor_setters_cancel_a2_a7_a16_authorization() {
    let workloads = r2_workloads();
    for name in [
        "csi_a_target_stream_reposition_severs",
        "csi_b_clamped_target_reposition_severs",
        "csi_c_target_reposition_severs",
        "csi_d_target_reposition_severs",
        "csi_e_clamped_target_reposition_severs",
        "csi_f_source_reposition_severs",
        "csi_g_target_reposition_severs",
        "vpa_target_reposition_severs",
        "origin_mode_home_target_reposition_severs",
    ] {
        let workload = workloads
            .iter()
            .find(|workload| workload.name == name)
            .unwrap_or_else(|| panic!("missing cursor-setter workload {name}"));
        let (mut oracle, emission) = assert_expected(workload);
        let verdict = oracle.referee_transport_emission(&emission.actual);
        assert!(verdict.false_joins.is_empty(), "{name}: false join");
        assert!(verdict.corruptions.is_empty(), "{name}: corrupt emission");
    }
}

#[test]
fn wide_early_and_wrap_source_death_counterexamples_pass() {
    let workloads = r1_workloads();
    let wide = workloads
        .iter()
        .find(|workload| workload.name == "reviewer_wide_early_abcd界")
        .expect("wide reviewer workload");
    let (mut oracle, emission) = assert_expected(wide);
    let edge = oracle
        .edge_records()
        .into_iter()
        .find(|edge| edge.disposition == oracle::EdgeDisposition::Live)
        .expect("live WideEarly edge");
    assert_eq!(edge.kind, WrapKind::WideEarly);
    assert_eq!(edge.padding_count, 1);
    assert_eq!(emission.logical_lines(), vec!["abcd界"]);
    let verdict = oracle.referee_transport_emission(&emission.actual);
    assert_eq!(verdict.checked_joins, 1);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());

    let source_death = workloads
        .iter()
        .find(|workload| workload.name == "reviewer_one_row_alt_wrap_source_dies_fail_closed")
        .expect("one-row alt reviewer workload");
    let (oracle, _) = assert_expected(source_death);
    assert!(
        oracle.edge_records().is_empty(),
        "dead source creates no edge"
    );
}

#[test]
fn combining_at_pending_margin_refreshes_qualified_token() {
    let workloads = r6_workloads();
    let forward = workloads
        .iter()
        .find(|workload| workload.name == "combining_at_pending_margin_refreshes_token")
        .expect("round-6 forward combining workload");
    let (mut oracle, emission) = assert_expected(forward);
    assert_eq!(oracle.live_count(), 1);
    assert_eq!(emission.logical_lines(), vec!["ABCDE\u{301}F"]);
    let verdict = oracle.referee_transport_emission(&emission.actual);
    assert_eq!(verdict.checked_joins, 1);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());

    // An explicit reposition still cancels forward authorization, so the
    // pre-existing addressed source-combining control remains an A7 sever.
    let addressed = r1_workloads()
        .into_iter()
        .find(|workload| workload.name == "combining_source_severs_a7")
        .expect("addressed combining control");
    let (mut oracle, _) = assert_expected(&addressed);
    assert_eq!(oracle.live_count(), 0);
    assert_eq!(oracle.severed_count(), 1);
}

#[test]
fn styled_wide_early_uses_real_bce_padding_and_referee_accepts() {
    let workloads = r6_workloads();
    let styled = workloads
        .iter()
        .find(|workload| workload.name == "styled_wide_early_uses_bce_padding")
        .expect("round-6 styled WideEarly workload");
    let (mut oracle, emission) = assert_expected(styled);
    let edge = oracle
        .edge_records()
        .into_iter()
        .find(|edge| edge.disposition == oracle::EdgeDisposition::Live)
        .expect("live styled WideEarly edge");
    assert_eq!(edge.kind, WrapKind::WideEarly);
    assert_eq!(edge.padding_count, 1);

    let cells = &emission.actual.chunks[0].cells;
    assert!(cells[..4].iter().all(|cell| cell.style == CellStyle::red()));
    assert!(cells[4].wide_early_padding);
    assert_eq!(cells[4].style, CellStyle::default());
    assert_eq!(cells[5].ch, '界');
    assert_eq!(cells[5].style, CellStyle::red());
    assert_eq!(
        emission.actual.chunks[0].render_ansi(),
        "\x1b[0;31mabcd界\x1b[0m".as_bytes()
    );

    let verdict = oracle.referee_transport_emission(&emission.actual);
    assert_eq!(verdict.checked_joins, 1);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());

    // Padding identity is still refereed before rendering, and padding is
    // omitted mechanically regardless of style. Inventing red foreground on
    // the real default-style blank is cell corruption.
    let mut invented_full_style = emission.actual.clone();
    invented_full_style.chunks[0].cells[4].style = CellStyle::red();
    let verdict = oracle.referee_transport_emission(&invented_full_style);
    assert_eq!(verdict.corruptions.len(), 1);
}

#[test]
fn round_7_c59_fidelity_counterexamples_and_emission_surfaces_pass() {
    let workloads = r7_workloads();
    assert_eq!(workloads.len(), 26);
    for workload in &workloads {
        assert_expected(workload);
    }

    let combining = workloads
        .iter()
        .find(|workload| workload.name == "combining_mark_17_is_noop_and_preserves_edge")
        .expect("combining-cap fixture");
    let (mut oracle, emission) = assert_expected(combining);
    assert_eq!(oracle.live_count(), 1);
    assert_eq!(emission.logical_lines()[0].matches('\u{301}').count(), 16);

    let wide = workloads
        .iter()
        .find(|workload| workload.name == "severed_wide_early_padding_is_real_styled_content")
        .expect("severed WideEarly fixture");
    let (mut oracle, emission) = assert_expected(wide);
    assert_eq!(emission.actual.chunks.len(), 2);
    let former_padding = &emission.actual.chunks[0].cells[4];
    assert!(!former_padding.wide_early_padding);
    assert_eq!(former_padding.style, CellStyle::blue_background());
    let verdict = oracle.referee_transport_emission(&emission.actual);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());
    let mut invented_marker = emission.actual.clone();
    invented_marker.chunks[0].cells[4].wide_early_padding = true;
    assert!(!oracle
        .referee_transport_emission(&invented_marker)
        .corruptions
        .is_empty());

    let get_history = workloads
        .iter()
        .find(|workload| workload.name == "get_history_during_alt_has_hard_main_alt_boundary")
        .expect("GetHistory-during-alt fixture");
    let mut oracle = Oracle::run(get_history);
    assert_eq!(oracle.emission(10).logical_lines(), vec!["DEF"]);
    let emission = oracle.emission_for(EmissionSurface::GetHistory, 10);
    assert_eq!(emission.logical_lines(), vec!["ABC", "DEF"]);
    let verdict =
        oracle.referee_transport_emission_for(EmissionSurface::GetHistory, &emission.actual);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());
    let mut false_join = emission.actual.clone();
    false_join.chunks[0].end_of_line = false;
    let verdict = oracle.referee_transport_emission_for(EmissionSurface::GetHistory, &false_join);
    assert_eq!(verdict.false_joins.len(), 1);

    let mut alt_restore = Oracle::new(5, 2, 100);
    alt_restore.apply(&Op::SetStyle(CellStyle::blue_background()));
    alt_restore.apply(&Op::EnterAlt1049);
    alt_restore.apply(&Op::SetStyle(CellStyle::default()));
    alt_restore.apply(&Op::ExitAlt1049);
    alt_restore.apply(&Op::Print("A".into()));
    assert_eq!(
        alt_restore.emission(5).actual.chunks[0].cells[0].style,
        CellStyle::blue_background(),
        "mode 1049 restores the saved main cursor style"
    );

    let mut blank_history = Oracle::new(5, 1, 100);
    blank_history.apply(&Op::Lf);
    blank_history.apply(&Op::EnterAlt1049);
    let emission = blank_history.emission_for(EmissionSurface::GetHistory, 5);
    assert_eq!(emission.logical_lines(), vec![""]);
    assert_eq!(emission.actual.chunks.len(), 1);
    let verdict =
        blank_history.referee_transport_emission_for(EmissionSurface::GetHistory, &emission.actual);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());
}

#[test]
fn round_7_counterexamples_match_real_c59_observations() {
    let mut screen = Screen::new(5, 1, 100);
    screen.process(b"ABCDEF\x1b[1;1rG");
    assert_eq!(actual_lines(&screen), vec!["ABCDE", "G"]);

    let mut screen = Screen::new(5, 1, 100);
    screen.process(b"ABCDEF\x1b[2;1rG");
    assert_eq!(actual_lines(&screen), vec!["ABCDE", "FG"]);

    let mark = '\u{301}';
    let mut screen = Screen::new(5, 2, 100);
    screen.process(b"ABCD");
    for _ in 0..16 {
        screen.process(mark.to_string().as_bytes());
    }
    screen.process(b"EF\x1b[1;5H");
    screen.process(mark.to_string().as_bytes());
    assert_eq!(actual_lines(&screen)[0].matches(mark).count(), 16);

    let mut screen = Screen::new(5, 1, 100);
    screen.process(b"ABC\n\x1b[?1049hDEF");
    assert_eq!(actual_lines(&screen), vec!["ABC", "DEF"]);
}

#[test]
fn round_8_alt_epoch_rep_and_wide_orphan_match_real_c59() {
    let workloads = r8_workloads();
    assert_eq!(workloads.len(), 6);
    for workload in &workloads {
        let (mut oracle, emission) = assert_expected(workload);
        let verdict = oracle.referee_transport_emission(&emission.actual);
        assert!(
            verdict.false_joins.is_empty(),
            "{} false join",
            workload.name
        );
        assert!(
            verdict.corruptions.is_empty(),
            "{} corruption",
            workload.name
        );
    }

    // A 47/1047 entry does not overwrite the shared DECSC snapshot. A 1049
    // exit does load that old cursor in c59, but the oracle's entry epoch keeps
    // its pending token from being reclassified as suspended-main provenance.
    let mixed = workloads
        .iter()
        .find(|workload| workload.name == "mixed_47_entry_1049_exit_rejects_old_decsc_token")
        .expect("mixed-alt workload");
    let mut oracle = Oracle::run(mixed);
    assert_eq!(oracle.live_count(), 0);
    assert_eq!(oracle.severed_count(), 1);
    assert_eq!(oracle.emission(10).logical_lines(), vec!["ABCDE", "Z"]);

    let mut screen = Screen::new(5, 2, 100);
    screen.process(b"ABCDE\x1b7F\x1b[?47h\x1b[?1049lZ");
    assert_eq!(actual_lines(&screen), vec!["ABCDE", "Z"]);
    let mut matched = Screen::new(5, 2, 100);
    matched.process(b"ABCDE\x1b7F\x1b[?1049h\x1b[?1049lZ");
    assert_eq!(actual_lines(&matched), vec!["ABCDE", "FZ"]);

    // Both live Grid::resize and the restore loop repair the retained wide
    // base before truncating away its continuation.
    let mut live_oracle = Oracle::new(5, 2, 100);
    live_oracle.apply(&Op::Print("abc界".into()));
    live_oracle.apply(&Op::Resize { cols: 4, rows: 2 });
    assert_eq!(live_oracle.emission(4).logical_lines(), vec!["abc"]);
    let mut live_screen = Screen::new(5, 2, 100);
    live_screen.process("abc界".as_bytes());
    live_screen.resize(4, 2);
    assert_eq!(actual_lines(&live_screen), vec!["abc"]);

    let wide_restore = workloads
        .iter()
        .find(|workload| workload.name == "alt_restore_horizontal_shrink_repairs_wide_orphan")
        .expect("wide restore workload");
    let comparison = compare_current(wide_restore);
    assert_eq!(comparison.expected_lines, vec!["abc", "F"]);
    assert_eq!(comparison.actual_physical_lines, vec!["abc", "F"]);

    // REP's omitted/zero/default/count dispatcher semantics all use print.
    let mut rep = Screen::new(5, 2, 100);
    rep.process(b"ABCDE\x1b[b");
    assert_eq!(actual_lines(&rep), vec!["ABCDE", "E"]);
    let mut rep_zero = Screen::new(5, 2, 100);
    rep_zero.process(b"ABC\x1b[0b\x1b[b");
    assert_eq!(actual_lines(&rep_zero), vec!["ABCCC"]);
    let mut rep_count = Screen::new(5, 2, 100);
    rep_count.process(b"ABC\x1b[2b");
    assert_eq!(actual_lines(&rep_count), vec!["ABCCC"]);
}

#[test]
fn round_9_alt_exit_matrix_and_zero_height_resize_match_real_c59() {
    let workloads = r9_workloads();
    assert_eq!(
        workloads.len(),
        10,
        "nine alt combinations plus zero resize"
    );
    for workload in &workloads {
        let (mut oracle, emission) = assert_expected(workload);
        let verdict = oracle.referee_transport_emission(&emission.actual);
        assert!(
            verdict.false_joins.is_empty(),
            "{} false join",
            workload.name
        );
        assert!(
            verdict.corruptions.is_empty(),
            "{} corruption",
            workload.name
        );

        let mut screen = Screen::new(workload.cols, workload.rows, workload.scrollback_limit);
        for op in &workload.ops {
            apply_screen(&mut screen, op);
        }
        let physical = actual_lines(&screen);
        if workload.name == "alt_1049h_1049l_restores_saved_main_cursor" {
            assert_eq!(physical, vec!["ABCDE", "Z"]);
        } else {
            assert_eq!(physical, workload.expected.as_ref().unwrap().logical_lines);
        }
    }

    let matched_47 = workloads
        .iter()
        .find(|workload| workload.name == "alt_47h_47l_keeps_active_cursor")
        .expect("matched-47 workload");
    let mut oracle = Oracle::run(matched_47);
    let glyph = |ch| CandidateCell {
        ch,
        display_width: 1,
        combining: String::new(),
        style: CellStyle::default(),
        wide_early_padding: false,
    };
    let mut false_joined_cells: Vec<CandidateCell> = "ABCDE".chars().map(glyph).collect();
    false_joined_cells.push(glyph('Z'));
    false_joined_cells.extend(std::iter::repeat_with(CandidateCell::blank).take(4));
    let false_joined = candidate(vec![CandidateHistoryChunk {
        cells: false_joined_cells,
        end_of_line: true,
    }]);
    let verdict = oracle.referee_transport_emission(&false_joined);
    assert!(
        !verdict.false_joins.is_empty() || !verdict.corruptions.is_empty(),
        "the erroneous ABCDEZ transport must be rejected"
    );
}

#[test]
fn round_11_mutable_tab_stops_drive_ht_destination_and_a1() {
    let workloads = r11_workloads();
    for name in [
        "hts_custom_stop_suppresses_a1",
        "tbc_clear_all_creates_a1",
        "tbc_current_clears_only_custom_stop",
    ] {
        let workload = workloads
            .iter()
            .find(|workload| workload.name == name)
            .unwrap_or_else(|| panic!("missing round-11 tab workload {name}"));
        let (mut oracle, emission) = assert_expected(workload);
        let verdict = oracle.referee_transport_emission(&emission.actual);
        assert!(verdict.false_joins.is_empty(), "{name}: false join");
        assert!(verdict.corruptions.is_empty(), "{name}: corrupt emission");

        let comparison = compare_current(workload);
        let expected_physical = match name {
            "hts_custom_stop_suppresses_a1" => vec!["ABC ZQR".to_string()],
            "tbc_clear_all_creates_a1" => {
                vec!["ABC      Z".to_string(), "Q".to_string()]
            }
            "tbc_current_clears_only_custom_stop" => vec!["ABC     ZQ".to_string()],
            _ => unreachable!(),
        };
        assert_eq!(
            comparison.actual_physical_lines, expected_physical,
            "{name}"
        );
        assert_eq!(comparison.legacy_unexpected_advances, 0, "{name}");
    }
}

#[test]
fn raw_only_wrap_fallback_stays_no_edge_then_later_fresh_a1_is_modeled() {
    let mut oracle = Oracle::new(5, 3, 100);
    for op in [
        Op::Print("ABCDE".into()),
        Op::Decsc,
        Op::Print("F".into()),
        Op::Decrc,
        Op::Print("Z12345".into()),
    ] {
        oracle.apply(&op);
    }
    assert_eq!(oracle.live_count(), 1, "only the later fresh A1 stays live");
    assert_eq!(
        oracle.severed_count(),
        1,
        "raw fallback target write cuts old A1"
    );
    assert_eq!(oracle.emission(10).logical_lines(), vec!["ABCDE", "Z12345"]);

    let mut screen = Screen::new(5, 3, 100);
    screen.process(b"ABCDE\x1b7F\x1b8Z12345");
    assert_eq!(actual_lines(&screen), vec!["ABCDE", "Z1234", "5"]);
}

#[test]
fn round_12_false_join_repros_fail_closed_and_fidelity_gaps_match_c59() {
    let workloads = r12_workloads();
    assert_eq!(workloads.len(), 7);
    for workload in &workloads {
        assert_expected(workload);
    }

    // F1 and F2 deliberately have two hard-broken physical rows whose exact
    // cells could otherwise be framed as one line. The authoritative referee
    // must reject that candidate despite its cell-for-cell fidelity.
    for name in [
        "r12_ri_raw_pending_is_untracked",
        "r12_ech_raw_pending_is_untracked",
        "r12_decawm_off_rewrite_demotes_raw_pending",
        "r12_combining_rep_uses_true_last_char",
    ] {
        let workload = workloads
            .iter()
            .find(|workload| workload.name == name)
            .expect("round-12 false-join workload");
        let (mut oracle, emission) = assert_expected(workload);
        assert_eq!(emission.actual.chunks.len(), 2, "{name}");
        let false_line = merged(
            emission.actual.chunks[0].clone(),
            &emission.actual.chunks[1],
        );
        let verdict = oracle.referee_transport_emission(&candidate(vec![false_line]));
        assert_eq!(verdict.checked_joins, 1, "{name}: {verdict:?}");
        assert_eq!(verdict.false_joins.len(), 1, "{name}: {verdict:?}");
        assert!(verdict.corruptions.is_empty(), "{name}: {verdict:?}");
    }

    // Corroborate the two raw-pending layouts directly against c59.
    for name in [
        "r12_ri_raw_pending_is_untracked",
        "r12_ech_raw_pending_is_untracked",
        "r12_csi_s_moved_pending_is_untracked",
        "r12_decawm_off_rewrite_demotes_raw_pending",
        "r12_combining_rep_uses_true_last_char",
        "r12_restored_row_resizes_at_unchanged_global_cols",
    ] {
        let workload = workloads
            .iter()
            .find(|workload| workload.name == name)
            .expect("round-12 c59 workload");
        let mut screen = Screen::new(workload.cols, workload.rows, workload.scrollback_limit);
        for op in &workload.ops {
            apply_screen(&mut screen, op);
        }
        assert_eq!(
            actual_lines(&screen),
            workload.expected.as_ref().unwrap().logical_lines,
            "{name} c59 physical rows"
        );
    }

    // F4's preserved edge joins two physical c59 rows in the oracle, so pin
    // the real chop outcome by visible content and absence of migration.
    let styled = workloads
        .iter()
        .find(|workload| workload.name == "r12_styled_blank_bottom_is_chopped")
        .unwrap();
    let mut screen = Screen::new(styled.cols, styled.rows, styled.scrollback_limit);
    for op in &styled.ops {
        apply_screen(&mut screen, op);
    }
    assert_eq!(screen.scrollback_len(), 0);
    assert_eq!(actual_lines(&screen), vec!["ABCDE", "F"]);
}

#[test]
fn round_14_render_witness_preserves_combining_repair_and_opaque_chunks() {
    let workloads = r14_workloads();
    assert_eq!(workloads.len(), 4);
    for workload in &workloads {
        assert_expected(workload);
    }

    let on_space = workloads
        .iter()
        .find(|workload| workload.name == "r14_combining_on_default_space_renders")
        .expect("combining-on-space workload");
    let (mut oracle, emission) = assert_expected(on_space);
    let verdict = oracle.referee_transport_emission(&emission.actual);
    assert!(verdict.false_joins.is_empty() && verdict.corruptions.is_empty());
    assert_eq!(emission.actual.render_ansi(), " \u{301}\r\n".as_bytes());

    for name in [
        "r14_live_resize_wide_repair_preserves_combining",
        "r14_alt_restore_wide_repair_preserves_combining",
    ] {
        let workload = workloads
            .iter()
            .find(|workload| workload.name == name)
            .expect("wide/combining repair workload");
        let (mut oracle, emission) = assert_expected(workload);
        assert_eq!(emission.actual.chunks.len(), 2, "{name}");
        let repaired = &emission.actual.chunks[0].cells[3];
        assert_eq!(repaired.ch, ' ', "{name}");
        assert_eq!(repaired.display_width, 1, "{name}");
        assert_eq!(repaired.combining, "\u{301}", "{name}");
        let verdict = oracle.referee_transport_emission(&emission.actual);
        assert!(verdict.false_joins.is_empty(), "{name}: {verdict:?}");
        assert!(verdict.corruptions.is_empty(), "{name}: {verdict:?}");
    }

    let opaque = workloads
        .iter()
        .find(|workload| workload.name == "r14_opaque_chunk_interior_spaces")
        .expect("opaque-chunk workload");
    let (mut oracle, emission) = assert_expected(opaque);
    assert_eq!(emission.actual.chunks.len(), 1);
    let cells = &emission.actual.chunks[0].cells;
    let split = candidate(vec![
        CandidateHistoryChunk {
            cells: cells[..5].to_vec(),
            end_of_line: false,
        },
        CandidateHistoryChunk {
            cells: cells[5..].to_vec(),
            end_of_line: true,
        },
    ]);
    assert_eq!(split.client_lines(), emission.actual.client_lines());
    let verdict = oracle.referee_transport_emission(&split);
    assert_eq!(verdict.checked_joins, 1);
    assert!(verdict.false_joins.is_empty() && verdict.corruptions.is_empty());
    assert_eq!(split.render_ansi(), b"ABC  D\r\n");

    // Rendering fidelity is post-gate only. The same continuous renderer does
    // not authorize a merge of two hard-broken rows.
    let mut unjoined = Oracle::new(5, 2, 100);
    for op in [Op::Print("ABC".into()), Op::Cr, Op::Lf, Op::Print("D".into())] {
        unjoined.apply(&op);
    }
    let hard_break = unjoined.emission(5).actual;
    let hostile_merge = candidate(vec![
        CandidateHistoryChunk {
            end_of_line: false,
            ..hard_break.chunks[0].clone()
        },
        hard_break.chunks[1].clone(),
    ]);
    let verdict = unjoined.referee_transport_emission(&hostile_merge);
    assert_eq!(verdict.checked_joins, 1);
    assert_eq!(verdict.false_joins.len(), 1);
    assert!(verdict.corruptions.is_empty());
}

#[test]
fn round_15_shrink_chop_is_style_agnostic_and_combining_aware() {
    let workloads = r15_workloads();
    assert_eq!(workloads.len(), 3);
    for workload in &workloads {
        assert_expected(workload);
    }

    let combining = workloads
        .iter()
        .find(|workload| workload.name == "r15_combining_bottom_row_migrates_not_chopped")
        .expect("combining-bearing shrink workload");
    let (mut oracle, emission) = assert_expected(combining);
    assert_eq!(emission.actual.chunks.len(), 2);
    assert_eq!(emission.actual.chunks[1].cells[5].ch, ' ');
    assert_eq!(emission.actual.chunks[1].cells[5].combining, "\u{301}");
    let verdict = oracle.referee_transport_emission(&emission.actual);
    assert_eq!(verdict.checked_joins, 1);
    assert!(verdict.false_joins.is_empty() && verdict.corruptions.is_empty());
    assert_eq!(emission.actual.render_ansi(), "\r\n      \u{301}\r\n".as_bytes());
}

#[test]
fn round_16_dch_ich_post_shift_repair_uses_bce_blank() {
    let workloads = r16_workloads();
    assert_eq!(workloads.len(), 2);

    for workload in &workloads {
        let (mut oracle, emission) = assert_expected(workload);
        assert_eq!(emission.actual.chunks.len(), 2, "{}", workload.name);
        let cells = &emission.actual.chunks[0].cells;
        assert_eq!(cells.len(), 5, "{}", workload.name);

        match workload.name.as_str() {
            "r16_dch_post_shift_orphan_uses_blue_bce_blank" => {
                assert_eq!(cells[0].ch, ' ');
                assert_eq!(cells[0].style, CellStyle::blue_background());
                assert_eq!([cells[1].ch, cells[2].ch], ['X', 'Y']);
                assert!(cells[3..]
                    .iter()
                    .all(|cell| cell.ch == ' ' && cell.style == CellStyle::blue_background()));
            }
            "r16_ich_boundary_orphan_uses_blue_bce_blank" => {
                assert_eq!(cells[2].ch, 'A');
                assert!(cells
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != 2)
                    .all(|(_, cell)| {
                        cell.ch == ' ' && cell.style == CellStyle::blue_background()
                    }));
            }
            name => panic!("unexpected round-16 workload {name}"),
        }

        let verdict = oracle.referee_transport_emission(&emission.actual);
        assert!(verdict.false_joins.is_empty(), "{}: {verdict:?}", workload.name);
        assert!(verdict.corruptions.is_empty(), "{}: {verdict:?}", workload.name);

        let mut screen = Screen::new(workload.cols, workload.rows, workload.scrollback_limit);
        for op in &workload.ops {
            apply_screen(&mut screen, op);
        }
        let rows = screen.visible_cells_snapshot();
        let row_text: String = rows[0].iter().map(|cell| cell.c).collect();
        let blank_indices: &[usize] = match workload.name.as_str() {
            "r16_dch_post_shift_orphan_uses_blue_bce_blank" => {
                assert_eq!(row_text, " XY  ");
                &[0, 3, 4]
            }
            "r16_ich_boundary_orphan_uses_blue_bce_blank" => {
                assert_eq!(row_text, "  A  ");
                &[0, 1, 3, 4]
            }
            _ => unreachable!(),
        };
        let bce_style = screen.resolve_style(rows[0][blank_indices[0]].style_id);
        assert!(bce_style.bg.is_some(), "{} real c59 BCE background", workload.name);
        assert!(blank_indices.iter().all(|index| {
            rows[0][*index].c == ' '
                && screen.resolve_style(rows[0][*index].style_id) == bce_style
        }));
    }
}

#[test]
fn round_17_false_break_fidelity_and_non_crossing_preservation_pass() {
    let workloads = r17_workloads();
    assert_eq!(workloads.len(), 11);
    for workload in &workloads {
        let (mut oracle, emission) = assert_expected(workload);
        let verdict = oracle.referee_transport_emission(&emission.actual);
        assert!(verdict.false_joins.is_empty(), "{}: {verdict:?}", workload.name);
        assert!(verdict.corruptions.is_empty(), "{}: {verdict:?}", workload.name);
    }

    let wide = workloads
        .iter()
        .find(|workload| {
            workload.name == "wide_early_direct_bce_write_preserves_sparse_combining"
        })
        .expect("WideEarly sparse-combining workload");
    let (mut oracle, emission) = assert_expected(wide);
    let suffix = &emission.actual.chunks[0].cells[4];
    assert_eq!(suffix.ch, ' ');
    assert_eq!(suffix.display_width, 1);
    assert_eq!(suffix.combining, "\u{301}");
    assert!(suffix.wide_early_padding);
    assert_eq!(emission.logical_lines(), vec!["ABCD界"]);
    let verdict = oracle.referee_transport_emission(&emission.actual);
    assert_eq!(verdict.checked_joins, 1);
    assert!(verdict.false_joins.is_empty() && verdict.corruptions.is_empty());

    for name in [
        "direct_insert_row_below_edge_preserves",
        "direct_remove_row_below_edge_preserves",
    ] {
        let workload = workloads
            .iter()
            .find(|workload| workload.name == name)
            .unwrap_or_else(|| panic!("missing non-crossing helper workload {name}"));
        let (mut oracle, emission) = assert_expected(workload);
        assert_eq!(oracle.live_count(), 1, "{name}");
        assert_eq!(emission.logical_lines(), vec!["ABCDEFG"], "{name}");
        let verdict = oracle.referee_transport_emission(&emission.actual);
        assert_eq!(verdict.checked_joins, 1, "{name}: {verdict:?}");
        assert!(verdict.false_joins.is_empty(), "{name}: {verdict:?}");
        assert!(verdict.corruptions.is_empty(), "{name}: {verdict:?}");
    }

    for name in [
        "direct_insert_row_crossing_severs",
        "direct_remove_row_endpoint_severs",
    ] {
        let workload = r2_workloads()
            .into_iter()
            .find(|workload| workload.name == name)
            .unwrap_or_else(|| panic!("missing crossing helper control {name}"));
        let (mut oracle, _) = assert_expected(&workload);
        assert_eq!(oracle.live_count(), 0, "{name}");
        assert_eq!(oracle.severed_count(), 1, "{name}");
    }

    let same_size = r7_workloads()
        .into_iter()
        .find(|workload| workload.name == "same_size_resize_preserves_pending_construction")
        .expect("same-size resize preservation workload");
    assert_expected(&same_size);
}

#[test]
fn round_11_ris_resets_style_before_new_a1_and_referee_accepts() {
    let workload = r11_workloads()
        .into_iter()
        .find(|workload| workload.name == "ris_resets_style_before_new_a1")
        .expect("round-11 RIS workload");
    let (mut oracle, emission) = assert_expected(&workload);
    assert_eq!(oracle.live_count(), 1);
    assert_eq!(oracle.severed_count(), 1);
    assert_eq!(emission.logical_lines(), vec!["GHIJKL"]);
    assert!(emission
        .actual
        .chunks
        .iter()
        .flat_map(|chunk| &chunk.cells)
        .all(|cell| cell.style == CellStyle::default()));

    // This candidate represents c59's source-faithful post-RIS default cells.
    // Before the fix, the oracle expected red here and rejected it.
    let mut source_faithful = emission.actual.clone();
    for cell in source_faithful
        .chunks
        .iter_mut()
        .flat_map(|chunk| &mut chunk.cells)
    {
        cell.style = CellStyle::default();
    }
    let verdict = oracle.referee_transport_emission(&source_faithful);
    assert_eq!(verdict.checked_joins, 1);
    assert!(verdict.false_joins.is_empty());
    assert!(verdict.corruptions.is_empty());

    let mut screen = Screen::new(5, 2, 100);
    screen.process(b"\x1b[31mABCDEF\x1bcGHIJKL");
    assert_eq!(actual_lines(&screen), vec!["GHIJK", "L"]);
    for cell in screen.visible_cells_snapshot().into_iter().flatten() {
        if cell.c != ' ' {
            assert!(screen.resolve_style(cell.style_id).fg.is_none());
        }
    }
}

#[test]
fn corpus_against_current_c59_screen() {
    let workloads = corpus_workloads();
    assert_eq!(
        workloads.len(),
        150,
        "corpus cardinality is evidence-bearing"
    );
    let mut comparisons = Vec::new();
    for workload in &workloads {
        assert_expected(workload);
        comparisons.push(compare_current(workload));
    }

    eprintln!(
        "B2_EVIDENCE name | should_survive | should_sever | current_false_breaks | \
         legacy_unexpected_advances | wrong_edge_decisions | diverges | expected | actual_physical"
    );
    for row in &comparisons {
        eprintln!(
            "B2_EVIDENCE {} | {} | {} | {} | {} | {} | {} | {:?} | {:?}",
            row.name,
            row.oracle_live,
            row.oracle_severed,
            row.current_false_breaks,
            row.legacy_unexpected_advances,
            row.current_false_breaks + row.legacy_unexpected_advances,
            row.diverges(),
            row.expected_lines,
            row.actual_physical_lines,
        );
    }

    let legacy_advance_workloads = comparisons
        .iter()
        .filter(|row| row.legacy_unexpected_advances > 0)
        .count();
    let divergent_workloads = comparisons.iter().filter(|row| row.diverges()).count();
    eprintln!(
        "B2_SUMMARY workloads={} divergent={} legacy_unexpected_advance_workloads={}",
        comparisons.len(),
        divergent_workloads,
        legacy_advance_workloads
    );

    assert!(
        comparisons
            .iter()
            .any(|row| row.name == "litmus_width5_emit10" && row.diverges()),
        "corpus must catch the logical-line replay litmus"
    );
    assert_eq!(
        legacy_advance_workloads, 15,
        "six prior legacy cells, four source-faithful untracked raw-wrap cells, two exhausted-epoch Phase-A controls, and three round-18 raw-motion cells must advance without A1"
    );
}
