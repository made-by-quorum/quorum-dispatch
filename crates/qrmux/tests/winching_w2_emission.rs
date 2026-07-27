//! Stage-1 product logical-emission validation against the frozen W1 referee.

#[allow(dead_code)]
#[path = "winching/oracle.rs"]
mod oracle;

use oracle::{
    parse_fixture, CandidateCell, CandidateHistoryChunk, CandidateTransportEmission, CellColor,
    CellStyle, EmissionSurface, FixtureExpectation, Op, Oracle, Workload,
};
use qrmux::screen::{Color, LogicalCell, LogicalEmissionSurface, Screen, Style, UnderlineStyle};

fn fixture_workloads() -> Vec<Workload> {
    [
        include_str!("fixtures/winching/g8_1_unchanged_alt.w1"),
        include_str!("fixtures/winching/g8_2_horizontal_alt_resize.w1"),
        include_str!("fixtures/winching/g8_3_vertical_trim.w1"),
        include_str!("fixtures/winching/g8_4_history_saved_boundary.w1"),
        include_str!("fixtures/winching/r1_counterexamples.w1"),
        include_str!("fixtures/winching/r1_named_surfaces.w1"),
        include_str!("fixtures/winching/r2_findings_and_completeness.w1"),
        include_str!("fixtures/winching/r6_oracle_fidelity.w1"),
        include_str!("fixtures/winching/r7_c59_fidelity.w1"),
        include_str!("fixtures/winching/r8_alt_epoch_rep_wide.w1"),
        include_str!("fixtures/winching/r9_alt_exit_and_zero_resize.w1"),
        include_str!("fixtures/winching/r11_tab_stops_and_ris.w1"),
        include_str!("fixtures/winching/r12_fail_closed_and_resize.w1"),
        include_str!("fixtures/winching/r14_render_fidelity.w1"),
        include_str!("fixtures/winching/r15_combining_shrink_chop.w1"),
        include_str!("fixtures/winching/r16_dch_ich_bce_repair.w1"),
        include_str!("fixtures/winching/r17_preservation_fidelity.w1"),
        include_str!("fixtures/winching/r18_geometry_raw_invalidation.w1"),
    ]
    .into_iter()
    .flat_map(parse_fixture)
    // The frozen oracle has four private direct-row events with no public
    // terminal-byte equivalent. Their product harness is a cfg(test) unit
    // test so no shadow mutator leaks into qrmux's production API.
    .filter(|workload| {
        !workload
            .ops
            .iter()
            .any(|op| matches!(op, Op::InsertRow { .. } | Op::RemoveRow { .. }))
    })
    .collect()
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
        name: name.into(),
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
    let mut workloads = fixture_workloads();
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
        Op::SetStyle(_) => panic!("unsupported arbitrary fixture style"),
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
        Op::CursorUp(n) => screen.process(format!("\x1b[{n}A").as_bytes()),
        Op::CursorDown(n) => screen.process(format!("\x1b[{n}B").as_bytes()),
        Op::CursorForward(n) => screen.process(format!("\x1b[{n}C").as_bytes()),
        Op::CursorBack(n) => screen.process(format!("\x1b[{n}D").as_bytes()),
        Op::CursorNextLine(n) => screen.process(format!("\x1b[{n}E").as_bytes()),
        Op::CursorPrevLine(n) => screen.process(format!("\x1b[{n}F").as_bytes()),
        Op::CursorHorizontalAbsolute(col) => screen.process(format!("\x1b[{col}G").as_bytes()),
        Op::CursorVerticalAbsolute(row) => screen.process(format!("\x1b[{row}d").as_bytes()),
        Op::OriginMode(on) => screen.process(if *on { b"\x1b[?6h" } else { b"\x1b[?6l" }),
        Op::Cup { row, col } => screen.process(format!("\x1b[{row};{col}H").as_bytes()),
        Op::EraseDisplay(n) => screen.process(format!("\x1b[{n}J").as_bytes()),
        Op::EraseLine(n) => screen.process(format!("\x1b[{n}K").as_bytes()),
        Op::EraseChars(n) => screen.process(format!("\x1b[{n}X").as_bytes()),
        Op::DeleteChars(n) => screen.process(format!("\x1b[{n}P").as_bytes()),
        Op::InsertChars(n) => screen.process(format!("\x1b[{n}@").as_bytes()),
        Op::Resize { cols, rows } => screen.resize(*cols, *rows),
        Op::EnterAlt1049 => screen.process(b"\x1b[?1049h"),
        Op::ExitAlt1049 => screen.process(b"\x1b[?1049l"),
        Op::EnterAlt47 => screen.process(b"\x1b[?47h"),
        Op::ExitAlt47 => screen.process(b"\x1b[?47l"),
        Op::EnterAlt1047 => screen.process(b"\x1b[?1047h"),
        Op::ExitAlt1047 => screen.process(b"\x1b[?1047l"),
        Op::Ed3 | Op::ClearScrollback => screen.process(b"\x1b[3J"),
        Op::Ris => screen.process(b"\x1bc"),
        Op::SetScrollRegion { top, bottom } => {
            screen.process(format!("\x1b[{top};{bottom}r").as_bytes())
        }
        Op::ScrollUp(n) => screen.process(format!("\x1b[{n}S").as_bytes()),
        Op::ScrollDown(n) => screen.process(format!("\x1b[{n}T").as_bytes()),
        Op::InsertLines(n) => screen.process(format!("\x1b[{n}L").as_bytes()),
        Op::DeleteLines(n) => screen.process(format!("\x1b[{n}M").as_bytes()),
        Op::InsertRow { .. } | Op::RemoveRow { .. } => {
            panic!("private direct-row events belong to the cfg(test) unit harness")
        }
        Op::Decaln => screen.process(b"\x1b#8"),
        Op::Decawm(on) => screen.process(if *on { b"\x1b[?7h" } else { b"\x1b[?7l" }),
    }
}

fn color(color: Color) -> CellColor {
    match color {
        Color::Indexed(n) => CellColor::Indexed(n),
        Color::Rgb(r, g, b) => CellColor::Rgb(r, g, b),
    }
}

fn style(style: Style) -> CellStyle {
    CellStyle {
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
        underline: match style.underline {
            UnderlineStyle::None => oracle::CellUnderline::None,
            UnderlineStyle::Single => oracle::CellUnderline::Single,
            UnderlineStyle::Double => oracle::CellUnderline::Double,
            UnderlineStyle::Curly => oracle::CellUnderline::Curly,
            UnderlineStyle::Dotted => oracle::CellUnderline::Dotted,
            UnderlineStyle::Dashed => oracle::CellUnderline::Dashed,
        },
        blink: style.blink,
        inverse: style.inverse,
        strikethrough: style.strikethrough,
        hidden: style.hidden,
        fg: style.fg.map(color),
        bg: style.bg.map(color),
        underline_color: style.underline_color.map(color),
    }
}

fn candidate_cell(cell: LogicalCell) -> CandidateCell {
    CandidateCell {
        ch: cell.ch,
        display_width: cell.display_width,
        combining: cell.combining,
        style: style(cell.style),
        wide_early_padding: cell.wide_early_padding,
    }
}

fn product_candidate(screen: &Screen, surface: EmissionSurface) -> CandidateTransportEmission {
    let surface = match surface {
        EmissionSurface::AttachReplay => LogicalEmissionSurface::AttachReplay,
        EmissionSurface::GetHistory => LogicalEmissionSurface::GetHistory,
    };
    let product = screen.logical_emission(surface);
    CandidateTransportEmission {
        chunks: product
            .chunks
            .into_iter()
            .map(|chunk| CandidateHistoryChunk {
                cells: chunk.cells.into_iter().map(candidate_cell).collect(),
                end_of_line: chunk.end_of_line,
            })
            .collect(),
    }
}

fn referee_product(workload: &Workload) -> (CandidateTransportEmission, oracle::RefereeVerdict) {
    let mut screen = Screen::new(workload.cols, workload.rows, workload.scrollback_limit);
    for op in &workload.ops {
        apply_screen(&mut screen, op);
    }
    let candidate = product_candidate(&screen, workload.emission_surface);
    let mut oracle = Oracle::run(workload);
    let verdict = oracle.referee_transport_emission_for(workload.emission_surface, &candidate);
    (candidate, verdict)
}

fn candidate_logical_lines(candidate: &CandidateTransportEmission) -> Vec<String> {
    candidate
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

#[test]
fn blocking_false_break_regressions_match_single_line_frozen_oracle() {
    let regressions = [
        workload(
            "ris_during_alt_then_main_scroll_preserves_edges",
            5,
            2,
            100,
            10,
            vec![Op::EnterAlt1049, Op::Ris, Op::Print("GHIJKLMNOPQ".into())],
            expected(2, 0, &["GHIJKLMNOPQ"]),
        ),
        workload(
            "same_size_resize_then_forward_print_preserves_edge",
            5,
            3,
            100,
            10,
            vec![
                Op::Print("ABCDEF".into()),
                Op::Resize { cols: 5, rows: 3 },
                Op::Print("Z".into()),
            ],
            expected(1, 0, &["ABCDEFZ"]),
        ),
        workload(
            "vertical_grow_then_forward_print_preserves_edge",
            5,
            3,
            100,
            10,
            vec![
                Op::Print("ABCDEF".into()),
                Op::Resize { cols: 5, rows: 4 },
                Op::Print("Z".into()),
            ],
            expected(1, 0, &["ABCDEFZ"]),
        ),
    ];

    let mut false_breaks = Vec::new();
    for workload in regressions {
        let (candidate, verdict) = referee_product(&workload);
        assert!(
            verdict.false_joins.is_empty(),
            "{} false join: {verdict:?}",
            workload.name
        );
        assert!(
            verdict.corruptions.is_empty(),
            "{} corruption: {verdict:?}",
            workload.name
        );
        let actual = candidate_logical_lines(&candidate);
        let expected = &workload.expected.as_ref().unwrap().logical_lines;
        if &actual != expected {
            false_breaks.push((workload.name.clone(), actual, expected.clone()));
        }
    }
    assert!(false_breaks.is_empty(), "false breaks: {false_breaks:#?}");
}

#[test]
fn product_logical_emission_passes_frozen_referee_for_all_146_byte_workloads() {
    let workloads = corpus_workloads();
    assert_eq!(
        workloads.len(),
        146,
        "corpus cardinality is evidence-bearing"
    );
    for workload in workloads {
        let (candidate, verdict) = referee_product(&workload);
        assert!(
            verdict.false_joins.is_empty(),
            "{} false join: {verdict:?}",
            workload.name
        );
        assert!(
            verdict.corruptions.is_empty(),
            "{} corruption: {verdict:?}",
            workload.name
        );
        let logical_lines = candidate_logical_lines(&candidate);
        assert_eq!(
            logical_lines,
            workload.expected.as_ref().unwrap().logical_lines,
            "{} contract logical lines",
            workload.name
        );
    }
}
