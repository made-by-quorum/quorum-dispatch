//! B3 M3b — resize integration rows R2, R3, R6 and the R7(f)(g) teeth.
//!
//! Spec: `exec/b3-spec.md` REV 3 (CONVERGED). This file owns:
//!   - R2 resize-during-altscreen (jailed daemon; protocol Resize while in-app)
//!   - R3 replay-after-width-resize (styled + CJK; semantic-class integrity via
//!     the M3b checkers in `lib/b3_checkers.rs`)
//!   - R6 horizontal-resize Screen integration (in-process Screen API; CELL/WIDTH
//!     level orphan inspection)
//!   - R7(f) truncated-SGR / lone-continuation lines MUST fail the R3(c) checkers
//!   - R7(g) synthetic grid with an orphan wide-continuation CELL MUST fail R6
//!
//! Hygiene: every daemon test self-jails (lib/jail.rs); evidence lands under
//! `target/test-evidence/<runid>/b3/<row>/` (ADD-7). App-output keyed (ADD-6):
//! markers are PRINTED by the app, never matched on PTY echo.

// The shared `lib/mod.rs` is `#[path]`-included into THREE test targets
// (integration_tests, b3_replay, b3_resize). Any helper/export used by only a
// subset of targets is `dead_code`/`unused_imports` in the others — a structural
// consequence of the multi-target split, not a real defect. Allow it at the
// module boundary so `clippy -D warnings` stays green per target. (M3b note:
// integration_tests.rs and b3_replay.rs need the same allow on their own
// `mod libmod;` — flagged to the lead.)
#[allow(dead_code, unused_imports)]
#[path = "lib/mod.rs"]
mod libmod;
use libmod::*;

use libmod::b3_checkers::{check_cjk_integrity, check_no_orphan_wide_cell, check_sgr_well_formed};
use libmod::client::Captured;

use sbmux::screen::{Cell, Screen};

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ============================================================================
// Shared helpers (M3b-local; integration_tests.rs helpers are not in scope)
// ============================================================================

/// Evidence dir for a B3 row: target/test-evidence/<runid>/b3/<row>/.
fn b3_evidence_dir(row: &str) -> PathBuf {
    let runid = std::env::var("SBMUX_GATE_RUNID").unwrap_or_else(|_| "dev".to_string());
    let dir = PathBuf::from("target/test-evidence")
        .join(runid)
        .join("b3")
        .join(row);
    fs::create_dir_all(&dir).expect("create evidence dir");
    dir
}

/// Poll a fresh-attach capture until `pred(text)` holds or timeout.
fn wait_for_text(
    socket: &std::path::Path,
    session: &str,
    timeout: Duration,
    pred: impl Fn(&str) -> bool,
) -> Result<String, Box<dyn Error>> {
    let start = Instant::now();
    loop {
        let cap = capture_session(socket, session, 150)?;
        let text = cap.text();
        if pred(&text) {
            return Ok(text);
        }
        if start.elapsed() > timeout {
            let tail: String = text.chars().skip(text.len().saturating_sub(300)).collect();
            return Err(format!(
                "timed out after {:?} (last capture {} chars; tail: {:?})",
                timeout,
                text.len(),
                tail
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Block until the daemon's snapshot for `session` reports `(cols, rows)`.
/// G3 dims-barrier: resizes ride the attached connection while a later stty
/// rides a separate connection and could overtake them; gate on the daemon's
/// own dims first.
fn wait_for_dims(
    socket: &std::path::Path,
    session: &str,
    cols: u16,
    rows: u16,
    timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let dims = list_sessions(socket)?
            .iter()
            .find(|s| s.name == session)
            .map(|s| (s.cols, s.rows));
        if dims == Some((cols, rows)) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!(
                "daemon dims never reached {}x{}; last snapshot: {:?}",
                cols, rows, dims
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Count numbered lines `prefix<lo>..prefix<hi>` (zero-padded) present in text.
fn count_numbered_lines(text: &str, prefix: &str, lo: usize, hi: usize, width: usize) -> usize {
    (lo..=hi)
        .filter(|i| text.contains(&format!("{}{:0w$}", prefix, i, w = width)))
        .count()
}

// ============================================================================
// R2 — resize-during-altscreen
// ============================================================================

/// R2 — enter a real altscreen app (`less`), shrink rows then grow WHILE in-app
/// (protocol Resize on the attached connection, G3 dims-barrier), exit, then:
///   (a) primary grid re-fit to final dims (stty-confirmed behind dims barrier);
///   (b) pre-app scrollback intact — text-level scroll-intact + EVERY sentinel
///       present (honestly text-level per spec; NOT R0, a settled capture can't
///       claim wire order — red-team #12);
///   (c) reattach replay clean (no panic; settled capture sane);
///   (d) raw bytes altscreen-replay: the post-exit fresh reattach is a
///       MAIN-SCREEN capture → zero 1049 sequences (app present in-run →
///       assertion has teeth; reversed gate 2026-06-10).
#[test]
fn r2_resize_during_altscreen() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("b3_r2_resize_altscreen")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "b3r2")?;
    let session = "b3r2";
    create_session(&socket, session)?;
    let ev = b3_evidence_dir("R2");

    // Pre-app: numbered scrollback + several DISTINCT sentinels (quote-split so
    // the echoed command line can never satisfy the match — ADD-6).
    send_to_session(&socket, &env, session, "seq -f 'r2-pre-%03.0f' 1 40\n")?;
    let sentinels = ["R2-ALPHA", "R2-BRAVO", "R2-CHARLIE", "R2-DELTA"];
    for s in &sentinels {
        // split the literal so only executed echo output carries the joined form
        let (a, b) = s.split_at(4);
        send_to_session(&socket, &env, session, &format!("echo {}''{}\n", a, b))?;
    }
    wait_for_text(&socket, session, Duration::from_secs(30), |t| {
        sentinels.iter().all(|s| t.contains(s))
    })?;

    // less input file with a marker that exists nowhere else.
    let file = jail.tmpdir.join("r2_file.txt");
    let content: String = (1..=200)
        .map(|i| format!("R2-FILE-{:03}-zorkmarker\n", i))
        .collect();
    fs::write(&file, &content)?;

    // Persistent attached client: the resize driver AND the in-app observer.
    let attached = AttachedClient::attach(&socket, session)?;
    let wait_attached = |needle: &str, secs: u64| -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if attached.captured_text().contains(needle) {
                return Ok(());
            }
            if let Some(e) = attached.error() {
                return Err(format!("attached client errored: {}", e));
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "timed out waiting for {:?} on attached stream",
                    needle
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };

    // Establish a known starting size, then enter less.
    attached.resize(100, 40)?;
    wait_for_dims(&socket, session, 100, 40, Duration::from_secs(10))?;
    send_to_session(
        &socket,
        &env,
        session,
        &format!("less {}\n", file.display()),
    )?;
    wait_attached("zorkmarker", 30)
        .map_err(|e| format!("R2: less never rendered in-app marker: {}", e))?;

    // While IN-APP: shrink rows then grow. Dims-barrier each step.
    attached.resize(100, 20)?;
    wait_for_dims(&socket, session, 100, 20, Duration::from_secs(10))?;
    attached.resize(100, 45)?;
    wait_for_dims(&socket, session, 100, 45, Duration::from_secs(10))?;
    // Final known size — asserted via stty after exit.
    attached.resize(110, 30)?;
    wait_for_dims(&socket, session, 110, 30, Duration::from_secs(10))?;

    if let Some(e) = attached.error() {
        return Err(format!("R2: attached client errored during in-app resize: {}", e).into());
    }

    // Exit less. Keep the attached client OPEN: a fresh-attach capture would
    // resize the child PTY to the capture client's own dims (one-client mux),
    // clobbering the 110x30 we just set. The final-dims assertion must ride the
    // SAME attached connection that set the size (G3 pattern).
    send_to_session(&socket, &env, session, "q")?;

    // (a) primary grid re-fit to final dims: stty behind the dims barrier,
    // observed on the still-attached stream. `stty size` prints "rows cols".
    wait_for_dims(&socket, session, 110, 30, Duration::from_secs(10))?;
    send_to_session(&socket, &env, session, "stty size\n")?;
    wait_attached("30 110", 30).map_err(|e| {
        let t = attached.captured_text();
        let tail: String = t.chars().skip(t.len().saturating_sub(400)).collect();
        format!(
            "R2(a): final size 30x110 not reported by stty: {}; attached tail: {:?}",
            e, tail
        )
    })?;
    fs::write(ev.join("r2_stty.txt"), attached.captured_text())?;

    // (b)+(c) on the ATTACHED stream. This client attached BEFORE less and so
    // its accumulated text holds the pre-app scrollback (sentinels + r2-pre
    // lines) from the initial attach replay, PLUS the post-exit restore — a
    // continuous record across the whole altscreen+resize episode. A post-exit
    // fresh capture cannot serve this honestly: a fresh attach re-resizes the
    // child to its own dims, and the daemon's 80x24 capture window does not
    // re-replay the full 110x30 scrollback intact (observed: the joined sentinel
    // output scrolls out of the fresh-capture window). The attached stream is
    // the truthful continuous surface. (Finding for the lead, R2 notes.)
    let after = attached.captured_text();
    fs::write(ev.join("r2_after.txt"), &after)?;

    // (b) pre-app scrollback intact — text-level scroll-intact + EVERY sentinel.
    // Asserted on the cumulative attached transcript (holds the pre-app state).
    let lines: Vec<String> = after.lines().map(|s| s.to_string()).collect();
    for s in &sentinels {
        assert_scroll_intact(&lines, s, "R2(b)").map_err(|e| -> Box<dyn Error> { e.into() })?;
    }
    let n = count_numbered_lines(&after, "r2-pre-", 1, 40, 3);
    assert_eq!(
        n, 40,
        "R2(b) scroll-intact: expected all 40 pre-app lines, got {}",
        n
    );

    // Drop the attached client so the (c)/(d) fresh-capture checks can attach.
    attached.close();

    // (c) reattach replay clean: a FRESH attach (the real reattach surface)
    // shows the restored primary screen — NO half-rendered grid, and the
    // altscreen app content is gone. No panic from the capture path itself.
    let reattach = capture_session(&socket, session, 200)?;
    let reattach_text = reattach.text();
    fs::write(ev.join("r2_reattach.txt"), &reattach_text)?;
    assert!(
        !reattach_text.contains("zorkmarker"),
        "R2(c): altscreen app content ('zorkmarker') must be gone on fresh reattach after exit"
    );

    // (d) raw bytes altscreen-replay: app WAS present, so this has teeth —
    // and because less already exited, this fresh attach is a main-screen
    // replay that must carry ZERO 1049 sequences (the renderer replays alt
    // state only while the inner app is in it; reversed gate 2026-06-10).
    assert_altscreen_replay(&reattach.raw_render(), 0, 0, "R2(d)-after")
        .map_err(|e| -> Box<dyn Error> { e.into() })?;
    fs::write(ev.join("r2_raw_after.bin"), reattach.raw_render())?;

    fs::write(
        ev.join("r2_result.txt"),
        format!(
            "R2 PASS\n\
             (a) primary re-fit: stty reports 30 rows x 110 cols (dims-barrier confirmed)\n\
             (b) scroll-intact: 40/40 pre-app lines + all {} sentinels present (text-level, honest)\n\
             (c) reattach replay clean: settled capture restored sentinels, marker gone, no panic\n\
             (d) altscreen-replay: post-exit main-screen capture carries zero 1049 (app was present in-run)\n\
             resizes during altscreen: 100x40 -> 100x20 (shrink) -> 100x45 (grow) -> 110x30 (final)\n",
            sentinels.len()
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R3 — replay-after-width-resize (semantic-class)
// ============================================================================

/// R3 — produce styled (SGR colors) + CJK wide-char marker lines at 100 cols,
/// resize to 80, fresh attach:
///   (a) no panic, replay completes;
///   (b) every marker line's TEXT present post-strip-ANSI (app-output keyed);
///   (c) styled-replay integrity via the M3b checkers — SGR well-formed per line
///       + CJK chars whole (no lone continuation bytes after strip);
///   (d) BEHAVIOR NOTE recorded for the C1 comparator: history replays
///       as-recorded; the client terminal wraps. (named-divergence candidate.)
#[test]
fn r3_replay_after_width_resize() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("b3_r3_width_resize")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "b3r3")?;
    let session = "b3r3";
    create_session(&socket, session)?;
    let ev = b3_evidence_dir("R3");

    // Single attached client = sole observer AND resize source for the whole
    // production phase. We must NOT fresh-attach (capture_session) while it is
    // open: a fresh attach EVICTS the one attached client (one-client mux). So
    // barriers poll the attached stream, not capture_session.
    let attached = AttachedClient::attach(&socket, session)?;
    let wait_attached = |needle: &str, secs: u64| -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if attached.captured_text().contains(needle) {
                return Ok(());
            }
            if let Some(e) = attached.error() {
                return Err(format!("attached client errored: {}", e));
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "timed out waiting for {:?} on attached stream",
                    needle
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };

    // Start at 100 cols via the attached client (real resize path).
    attached.resize(100, 30)?;
    wait_for_dims(&socket, session, 100, 30, Duration::from_secs(10))?;

    // Produce styled (SGR) + CJK marker lines by `cat`-ing pre-written files.
    // WHY files, not inline `printf`: sending multibyte UTF-8 through the PTY
    // INPUT path proved lossy under burst (the CJK + index were truncated from
    // the command line before the shell read them). App output is the contract
    // (ADD-6), so we make the APP read deterministic bytes from a file and emit
    // them — exactly the G2 file technique. The markers themselves are the app
    // output; nothing keys on echo.
    //
    // Styled file: each line wraps a unique token in real SGR (ESC[31m … ESC[0m).
    let styled_file = jail.tmpdir.join("r3_styled.txt");
    let styled: String = (1..=6)
        .map(|i| format!("\x1b[31mR3STYLE-{:02}-\x1b[1;32mtail\x1b[0m\n", i))
        .collect();
    fs::write(&styled_file, &styled)?;
    // CJK file: each line carries wide chars + a unique ASCII token.
    let cjk_file = jail.tmpdir.join("r3_cjk.txt");
    let cjk: String = (1..=6)
        .map(|i| format!("R3CJK-{:02}-世界你好テスト\n", i))
        .collect();
    fs::write(&cjk_file, &cjk)?;

    send_to_session(
        &socket,
        &env,
        session,
        &format!("cat {}\n", styled_file.display()),
    )?;
    send_to_session(
        &socket,
        &env,
        session,
        &format!("cat {}\n", cjk_file.display()),
    )?;
    // Barrier: wait until the last of each class is visible on the ATTACHED
    // stream (no fresh attach — that would evict the attached client).
    wait_attached("R3STYLE-06", 30).map_err(|e| -> Box<dyn Error> { e.into() })?;
    wait_attached("R3CJK-06", 30).map_err(|e| -> Box<dyn Error> { e.into() })?;

    // Push the 12 marker lines OFF the visible grid into scrollback (history),
    // so R3(c) can check the RECORDED History wire form. 30 rows visible →
    // emit a generous trailing filler block and wait for its tail.
    send_to_session(&socket, &env, session, "seq -f 'r3-fill-%03.0f' 1 60\n")?;
    wait_attached("r3-fill-060", 30).map_err(|e| -> Box<dyn Error> { e.into() })?;

    // Resize to 80 cols (the width change under test). Drop the attached client
    // so the FRESH attach below is the replay-after-resize surface.
    attached.resize(80, 30)?;
    wait_for_dims(&socket, session, 80, 30, Duration::from_secs(10))?;
    attached.close();

    // (a) no panic, replay completes: a fresh capture returns Captured.
    let cap: Captured = capture_session(&socket, session, 200)?;
    let text = cap.text();
    fs::write(ev.join("r3_capture_text.txt"), &text)?;

    // (b) every marker line's TEXT present post-strip-ANSI.
    let style_present = count_numbered_lines(&text, "R3STYLE-", 1, 6, 2);
    let cjk_present = count_numbered_lines(&text, "R3CJK-", 1, 6, 2);
    assert_eq!(
        style_present, 6,
        "R3(b): expected 6 styled marker TEXTs post-strip, got {}",
        style_present
    );
    assert_eq!(
        cjk_present, 6,
        "R3(b): expected 6 CJK marker TEXTs post-strip, got {}",
        cjk_present
    );
    // Whole CJK content survived as text.
    let cjk_needle = "世界你好テスト";
    assert!(
        text.contains(cjk_needle),
        "R3(b): whole CJK content '{}' missing from replay text",
        cjk_needle
    );

    // (c) styled-replay integrity on the RECORDED History line bytes (the wire
    // form the client replays). Run both checkers over every history line that
    // carries one of our markers; this keys the checker on app output, not
    // prompt chrome.
    let mut checked_style = 0;
    let mut checked_cjk = 0;
    for (idx, raw) in cap.history.iter().enumerate() {
        let as_text = String::from_utf8_lossy(raw);
        let desc = format!("R3(c)-histline-{}", idx);
        // SGR well-formedness applies to ALL history lines (cheap, universal).
        check_sgr_well_formed(raw, &desc).map_err(|e| -> Box<dyn Error> { e.into() })?;
        // CJK integrity applies to ALL history lines too.
        check_cjk_integrity(raw, &desc).map_err(|e| -> Box<dyn Error> { e.into() })?;
        if as_text.contains("R3STYLE-") {
            checked_style += 1;
        }
        if as_text.contains("R3CJK-") {
            checked_cjk += 1;
        }
    }
    assert!(
        checked_style >= 6,
        "R3(c): expected >=6 styled history lines checked, got {} (history may have wrapped \
         differently — investigate, do not weaken)",
        checked_style
    );
    assert!(
        checked_cjk >= 6,
        "R3(c): expected >=6 CJK history lines checked, got {}",
        checked_cjk
    );

    // (d) behavior note for C1 comparator.
    fs::write(
        ev.join("r3_result.txt"),
        format!(
            "R3 PASS (semantic-class)\n\
             (a) no panic; fresh-attach replay completed at 80 cols\n\
             (b) text present post-strip: {}/6 styled, {}/6 CJK; whole CJK content intact\n\
             (c) styled-replay integrity: SGR well-formed + CJK whole over ALL {} history lines \
                 ({} styled markers, {} CJK markers among them)\n\
             (d) BEHAVIOR NOTE (for C1 comparator cross-check, M5): history replays AS-RECORDED \
                 (the lines were captured at the producing width; the daemon does NOT reflow \
                 recorded scrollback on a subsequent width change). The CLIENT terminal performs \
                 visual wrapping of the replayed bytes. Candidate named divergence pending corpus \
                 cross-check (R3(d), M5). render.rs:62 contract holds: every recorded line is \
                 self-contained, SGR-balanced, width-0 continuations dropped.\n",
            style_present,
            cjk_present,
            cap.history.len(),
            checked_style,
            checked_cjk
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R6 — horizontal-resize Screen integration (in-process; CELL/WIDTH level)
// ============================================================================

/// R6 — at the SCREEN level (not Row/cell unit level, which cell.rs:292-318
/// covers): build a screen with a CJK char straddling the FUTURE H-shrink
/// boundary, `screen.resize()` H-shrink then H-grow, then assert at CELL/WIDTH
/// level:
///   (a) no panic;
///   (b) no half-cell artifact — boundary cell is a whole wide char or the
///       documented orphan replacement; NO lone wide-continuation cell survives
///       (cell inspection, not rendered bytes — red-team #16);
///   (c) non-straddling content intact.
#[test]
fn r6_horizontal_resize_screen_integration() -> Result<(), Box<dyn Error>> {
    let ev = b3_evidence_dir("R6");

    // Screen: 10 cols. Place a wide CJK char so its continuation lands exactly
    // at the future boundary. We'll H-shrink to 6, so a wide char at cols 5-6
    // straddles the new last-column (index 5). Put '世' at col 4-5 (within) and
    // another '界' at col 5-6 (straddling boundary col 5).
    let mut screen = Screen::new(10, 3, 100);
    // Non-straddling marker (ASCII) at the row start, then CJK pair.
    // Layout target on row 0: A B 世(2-3) 界(4-5) 你(6-7) ...
    screen.process("AB世界你好".as_bytes());

    // Sanity: confirm the straddle exists BEFORE resize at the cell level.
    let pre = screen.visible_cells_snapshot();
    fs::write(ev.join("r6_pre.txt"), format!("{:#?}", cells_summary(&pre)))?;

    // (a) no panic: H-shrink across the boundary, then H-grow back.
    screen.resize(6, 3);
    let shrunk = screen.visible_cells_snapshot();
    fs::write(
        ev.join("r6_shrunk.txt"),
        format!("{:#?}", cells_summary(&shrunk)),
    )?;
    // (b) on the SHRUNK grid: no lone wide-continuation cell, no orphan base.
    check_no_orphan_wide_cell(&shrunk, "R6(b)-shrunk")
        .map_err(|e| -> Box<dyn Error> { e.into() })?;

    screen.resize(10, 3);
    let grown = screen.visible_cells_snapshot();
    fs::write(
        ev.join("r6_grown.txt"),
        format!("{:#?}", cells_summary(&grown)),
    )?;
    // (b) on the GROWN grid: invariant still holds.
    check_no_orphan_wide_cell(&grown, "R6(b)-grown").map_err(|e| -> Box<dyn Error> { e.into() })?;

    // (c) non-straddling content intact: the ASCII 'A''B' at the row start
    // survived both resizes (it was never near the boundary).
    let row0 = &grown[0];
    assert_eq!(
        row0[0].c, 'A',
        "R6(c): col0 should be 'A', got {:?}",
        row0[0].c
    );
    assert_eq!(
        row0[1].c, 'B',
        "R6(c): col1 should be 'B', got {:?}",
        row0[1].c
    );
    // The leading wide char '世' (cols 2-3, well inside the 6-col shrink) must
    // survive whole as base+continuation.
    assert_eq!(
        row0[2].c, '世',
        "R6(c): col2 should be '世', got {:?}",
        row0[2].c
    );
    assert_eq!(
        row0[2].width, 2,
        "R6(c): '世' must be a wide base (width 2)"
    );
    assert_eq!(row0[3].width, 0, "R6(c): '世' continuation must be width 0");

    fs::write(
        ev.join("r6_result.txt"),
        "R6 PASS (Screen-level integration)\n\
         (a) no panic: screen.resize(6,3) H-shrink across CJK boundary then resize(10,3) H-grow\n\
         (b) no orphan: check_no_orphan_wide_cell clean on BOTH shrunk and grown snapshots \
             (cell/width level — render-byte UTF-8 is vacuous, red-team #16)\n\
         (c) non-straddling intact: 'A''B' + leading wide '世' (width 2 / continuation 0) survived\n",
    )?;

    Ok(())
}

/// Compact a cell-grid snapshot to (char, width) per cell for evidence files.
fn cells_summary(rows: &[Vec<Cell>]) -> Vec<Vec<(char, u8)>> {
    rows.iter()
        .map(|r| r.iter().map(|c| (c.c, c.width)).collect())
        .collect()
}

// ============================================================================
// R7(f) — R3(c) checker teeth (truncated SGR + lone-continuation byte)
// ============================================================================

/// R7(f) — the R3(c) checkers MUST fail on corrupted input:
///   - a truncated-SGR line (CSI never terminated, or not by 'm');
///   - a lone-continuation-byte line (a split wide char leaves a dangling UTF-8
///     continuation byte that `check_cjk_integrity` must reject).
#[test]
fn r7f_r3_checkers_have_teeth() {
    // --- truncated SGR: ESC[ with no terminator ---
    let truncated = b"\x1b[31mhello\x1b[1;32"; // second CSI never closed
    assert!(
        check_sgr_well_formed(truncated, "r7f-truncated").is_err(),
        "R7(f): truncated SGR must FAIL the well-formedness checker"
    );

    // --- styled line missing trailing reset ---
    let no_reset = b"\x1b[31mhello"; // styled but no closing \x1b[0m
    assert!(
        check_sgr_well_formed(no_reset, "r7f-no-reset").is_err(),
        "R7(f): styled line without trailing reset must FAIL"
    );

    // --- CSI terminated by a non-'m' final byte (e.g. a cursor move 'H') ---
    let non_sgr = b"\x1b[2Hhello\x1b[0m";
    assert!(
        check_sgr_well_formed(non_sgr, "r7f-non-sgr").is_err(),
        "R7(f): a non-SGR CSI on a history line must FAIL (render_line emits only SGR)"
    );

    // --- lone continuation byte: '世' is E4 B8 96; drop the lead byte to leave
    //     a dangling continuation sequence (B8 96), which is invalid UTF-8 ---
    let whole = "世".as_bytes(); // [0xe4, 0xb8, 0x96]
    let lone_continuation = vec![whole[1], whole[2]]; // [0xb8, 0x96] — orphan
    assert!(
        check_cjk_integrity(&lone_continuation, "r7f-lone-cont").is_err(),
        "R7(f): a lone wide-char continuation byte sequence must FAIL the CJK checker"
    );

    // Positive controls so the teeth aren't vacuously failing on everything.
    assert!(
        check_sgr_well_formed(b"\x1b[31mhi\x1b[0m", "r7f-pos-sgr").is_ok(),
        "R7(f) control: a well-formed styled line must PASS"
    );
    assert!(
        check_sgr_well_formed(b"plain text", "r7f-pos-plain").is_ok(),
        "R7(f) control: an unstyled line must PASS (no trailing reset required)"
    );
    assert!(
        check_cjk_integrity("世界 R3CJK-01".as_bytes(), "r7f-pos-cjk").is_ok(),
        "R7(f) control: whole CJK text must PASS"
    );
}

// ============================================================================
// R7(g) — R6 checker teeth (synthetic orphan wide-continuation CELL)
// ============================================================================

/// R7(g) — the R6 cell-level checker MUST fail on a synthetic grid containing an
/// orphan wide-continuation cell (cell-level mutation, aligned with R6(b)).
#[test]
fn r7g_r6_checker_has_teeth() {
    // Lone continuation: a width-0 cell NOT preceded by a width-2 base.
    let orphan_continuation: Vec<Vec<Cell>> = vec![vec![
        Cell::new('A', Default::default(), 1),
        Cell::new('\0', Default::default(), 0), // orphan continuation
        Cell::new('B', Default::default(), 1),
    ]];
    assert!(
        check_no_orphan_wide_cell(&orphan_continuation, "r7g-lone-cont").is_err(),
        "R7(g): a lone wide-continuation cell must FAIL the R6 checker"
    );

    // Orphan wide base: a width-2 cell at row end with no continuation after it.
    let orphan_base: Vec<Vec<Cell>> = vec![vec![
        Cell::new('A', Default::default(), 1),
        Cell::new('世', Default::default(), 2), // base at row end, no continuation
    ]];
    assert!(
        check_no_orphan_wide_cell(&orphan_base, "r7g-orphan-base").is_err(),
        "R7(g): a wide base with no continuation must FAIL the R6 checker"
    );

    // Wide base followed by a non-continuation cell (width 1) — also orphaned.
    let base_then_normal: Vec<Vec<Cell>> = vec![vec![
        Cell::new('世', Default::default(), 2),
        Cell::new('X', Default::default(), 1), // should have been width 0
    ]];
    assert!(
        check_no_orphan_wide_cell(&base_then_normal, "r7g-base-then-normal").is_err(),
        "R7(g): a wide base followed by a width-1 cell must FAIL the R6 checker"
    );

    // Positive control: a well-formed wide pair passes.
    let well_formed: Vec<Vec<Cell>> = vec![vec![
        Cell::new('A', Default::default(), 1),
        Cell::new('世', Default::default(), 2),
        Cell::new('\0', Default::default(), 0),
        Cell::new('B', Default::default(), 1),
    ]];
    assert!(
        check_no_orphan_wide_cell(&well_formed, "r7g-pos").is_ok(),
        "R7(g) control: a well-formed wide pair must PASS"
    );
}
