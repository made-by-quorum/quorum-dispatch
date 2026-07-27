//! W1 Phase A repro: stale deferred-wrap state restored across alt-screen resize.
//!
//! This intentionally asserts the behavior of the unmodified c59efe03 product
//! code.  A later fix should turn these assertions red and replace them with the
//! desired no-false-join behavior.

use qrmux::screen::{Screen, TerminalEmulator};

fn row_texts(screen: &Screen) -> Vec<String> {
    screen
        .visible_cells_snapshot()
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| if cell.width == 0 { '\0' } else { cell.c })
                .collect::<String>()
                .trim_end_matches(' ')
                .to_string()
        })
        .collect()
}

fn history_texts(screen: &Screen) -> Vec<String> {
    screen
        .get_history()
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

fn content_texts(screen: &Screen) -> Vec<String> {
    screen
        .get_content_history()
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

#[test]
fn width_change_restores_stale_deferred_wrap_and_false_joins_next_print() {
    let mut screen = Screen::new(5, 3, 100);

    // E fills row 0; F fires autowrap into row 1. J then fills row 1 and leaves
    // deferred wrap pending at (4, 1). Mode 1049 saves that pending bit.
    screen.process(b"ABCDEFGHIJ");
    assert_eq!(row_texts(&screen), vec!["ABCDE", "FGHIJ", ""]);
    screen.process(b"\x1b[?1049h");

    // Grid::resize clears the live wrap bit and changes every live alt row to
    // width 3. The saved main rows remain width 5 until alt exit, where they are
    // truncated; restore_cursor then resurrects the save-time pending bit.
    screen.resize(3, 3);
    screen.process(b"\x1b[?1049l");
    let restored = row_texts(&screen);
    let restored_cursor = screen.cursor_position();
    assert_eq!(restored, vec!["ABC", "FGH", ""]);
    assert_eq!(restored_cursor, (2, 1));

    // Z is a logically separate post-restore print. With resize having severed
    // the old continuation, it would overwrite H at the clamped cursor. Instead
    // the stale bit fires autowrap and places Z at the next row's column zero.
    screen.process(b"Z");
    let after = row_texts(&screen);
    let history = history_texts(&screen);

    // Control: CUP to the already-restored cursor is visually neutral but, like
    // all explicit cursor positioning, clears deferred wrap. It proves Z would
    // overwrite H if the geometry change had invalidated the stale bit.
    let mut cleared = Screen::new(5, 3, 100);
    cleared.process(b"ABCDEFGHIJ\x1b[?1049h");
    cleared.resize(3, 3);
    cleared.process(b"\x1b[?1049l\x1b[2;3HZ");
    let cleared_after = row_texts(&cleared);
    assert_eq!(cleared_after, vec!["ABC", "FGZ", ""]);

    eprintln!(
        "W1_WIDTH_FALSE_JOIN input=ABCDEFGHIJ,<1049h>,resize(3,3),<1049l>,Z \
         restored={restored:?} cursor={restored_cursor:?} after={after:?} history={history:?} \
         rendered_content={:?} explicit_CUP_control={cleared_after:?} false_join=FGH+Z",
        content_texts(&screen),
    );

    assert_eq!(after, vec!["ABC", "FGH", "Z"]);
    assert_eq!(history, Vec::<String>::new());
    assert_eq!(
        after[1], "FGH",
        "stale wrap kept H instead of overwriting it"
    );
    assert_eq!(after[2], "Z", "stale wrap false-joined Z onto the next row");
    assert!(
        screen
            .visible_cells_snapshot()
            .iter()
            .flatten()
            .all(|cell| cell.width == 1),
        "the reproduced continuation is deferred autowrap, not a width-0 wide-cell half"
    );
}

#[test]
fn bottom_trim_retargets_stale_deferred_wrap_to_survivor_and_scrolls_it() {
    let mut screen = Screen::new(5, 4, 100);

    // As above, F creates a real autowrap row boundary and J leaves another
    // deferred wrap pending on saved row 1.
    screen.process(b"ABCDEFGHIJ");
    assert_eq!(row_texts(&screen), vec!["ABCDE", "FGHIJ", "", ""]);
    screen.process(b"\x1b[?1049h");

    // Height 1 makes adjust_visible_to_fit pop saved rows 3, 2, and 1 from the
    // bottom. Saved cursor row 1 is then clamped onto surviving row 0, but its
    // pending-wrap bit is restored unchanged.
    screen.resize(5, 1);
    screen.process(b"\x1b[?1049l");
    let restored = row_texts(&screen);
    let restored_cursor = screen.cursor_position();
    assert_eq!(restored, vec!["ABCDE"]);
    assert_eq!(restored_cursor, (4, 0));
    assert_eq!(screen.scrollback_len(), 0);

    // Z should overwrite E on the survivor. The dangling pending bit instead
    // wraps at the bottom, scrolls ABCDE into history, and writes Z into a new
    // replacement row: the removed endpoint's state has been retargeted.
    screen.process(b"Z");
    let after = row_texts(&screen);
    let history = history_texts(&screen);

    // Same control at the clamped cursor: clearing deferred wrap makes Z replace
    // E in place, with no scrollback row created.
    let mut cleared = Screen::new(5, 4, 100);
    cleared.process(b"ABCDEFGHIJ\x1b[?1049h");
    cleared.resize(5, 1);
    cleared.process(b"\x1b[?1049l\x1b[1;5HZ");
    let cleared_after = row_texts(&cleared);
    let cleared_history = history_texts(&cleared);
    assert_eq!(cleared_after, vec!["ABCDZ"]);
    assert_eq!(cleared_history, Vec::<String>::new());

    eprintln!(
        "W1_TRIM_FALSE_JOIN input=ABCDEFGHIJ,<1049h>,resize(5,1),<1049l>,Z \
         restored={restored:?} cursor={restored_cursor:?} after={after:?} history={history:?} \
         rendered_content={:?} explicit_CUP_control={cleared_after:?} control_history={cleared_history:?} \
         false_join=ABCDE+Z",
        content_texts(&screen),
    );

    assert_eq!(after, vec!["Z"]);
    assert_eq!(history, vec!["ABCDE"]);
    assert_eq!(screen.scrollback_len(), 1);
    assert!(
        screen
            .visible_cells_snapshot()
            .iter()
            .flatten()
            .all(|cell| cell.width == 1),
        "the reproduced continuation is deferred autowrap, not a width-0 wide-cell half"
    );
}
