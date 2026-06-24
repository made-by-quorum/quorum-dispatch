//! B3 gate rows — VT scrollback + replay STRUCTURE (M3a ownership).
//!
//! Spec: `exec/b3-spec.md` REV 3. Rows implemented here: R5 (hybrid-replay
//! structure), R1 (reattach-during-altscreen, REDUCED to the new claim), R4
//! (scrollback-boundary content, integration half; unit half is in
//! `src/screen/grid.rs::tests::r4_scrollback_boundary_eviction_content`), and
//! the R7 teeth (a)(b)(c)(d)(e)(e2).
//!
//! All rows drive a REAL jailed daemon (jail.rs self-jails; no production
//! state is ever touched — ADD-10) and write evidence under
//! `target/test-evidence/<runid>/b3/<row>/` (SBMUX_GATE_RUNID, default "dev").
//!
//! KEY DISTINCTION from `integration_tests.rs` (G6 etc.): those use the
//! COLLAPSING capture (`capture_session`/`Captured`) which flattens History
//! frames and is correct only for content. The structure rows below use the
//! NON-COLLAPSING recorder (`record_attach`/`Recorded`) so frame order,
//! boundaries, and the settling ScreenUpdate are inspectable. Content adoption
//! (G6) is the lead's bookkeeping — NOT duplicated here.

#[path = "lib/mod.rs"]
#[allow(dead_code, unused_imports)] // shared harness; this binary uses a subset
mod libmod;
use libmod::*;
// M3a additions are kept off the glob (see lib/mod.rs) — import by path here.
use libmod::assertions::{
    assert_backlog_completeness, assert_backlog_ordered, check_boundary_content,
};
use libmod::client::{record_attach, strip_ansi, RecordedFrame};

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ============================================================================
// Shared helpers (evidence dir, marker generation, polling)
// ============================================================================

/// Evidence directory: target/test-evidence/<runid>/b3/<row>/.
/// Mirrors integration_tests.rs:28-29 but scoped under a `b3/` namespace so the
/// B3 gate run's artifacts never collide with the adopted B2 rows.
fn evidence_dir(row: &str) -> PathBuf {
    let runid = std::env::var("SBMUX_GATE_RUNID").unwrap_or_else(|_| "dev".to_string());
    let dir = PathBuf::from("target/test-evidence")
        .join(runid)
        .join("b3")
        .join(row);
    fs::create_dir_all(&dir).expect("create evidence dir");
    dir
}

/// Poll a fresh COLLAPSING capture until `pred(text)` or timeout (used only to
/// WAIT for production to land before taking the authoritative non-collapsing
/// recording — never as the assertion surface for structure rows).
fn wait_for_text(
    socket: &std::path::Path,
    session: &str,
    timeout: Duration,
    pred: impl Fn(&str) -> bool,
) -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    loop {
        let cap = capture_session(socket, session, 150)?;
        if pred(&cap.text()) {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err("timed out waiting for production marker".into());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

// ============================================================================
// Checkers — the falsifiable comparators for R5/R1. Defined here (M3a owns
// them) so the SAME checker runs in both the positive (real recording) and the
// negative (R7 mutated-input) arms. R3(c)/R6(b) checkers live in
// tests/lib/b3_checkers.rs (M3b). R0 lives in tests/lib/assertions.rs.
// ============================================================================

/// R5(a)/R1 frame-order checker over an ordered (non-collapsed) frame list.
/// Asserts the EXACT replay grammar with no interleaving:
///   Connected , History* , ScreenUpdate
/// i.e. (1) first frame is Connected; (2) exactly one ScreenUpdate and it is
/// LAST; (3) no History (or Connected) frame appears AFTER the ScreenUpdate;
/// (4) no second Connected. Passthrough frames are tolerated only BEFORE the
/// ScreenUpdate (notifications), never after. Teeth: R7(a).
fn check_replay_frame_order(frames: &[RecordedFrame], desc: &str) -> Result<(), String> {
    if frames.is_empty() {
        return Err(format!("[{}] frame-order FAIL: empty frame list", desc));
    }
    if !matches!(frames[0], RecordedFrame::Connected { .. }) {
        return Err(format!(
            "[{}] frame-order FAIL: first frame is {:?}, expected Connected",
            desc, frames[0]
        ));
    }
    // Locate the ScreenUpdate — must be exactly one and it must be last.
    let su_positions: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| matches!(f, RecordedFrame::ScreenUpdate(_)))
        .map(|(i, _)| i)
        .collect();
    match su_positions.as_slice() {
        [] => {
            return Err(format!(
                "[{}] frame-order FAIL: no ScreenUpdate frame in replay",
                desc
            ))
        }
        [pos] => {
            if *pos != frames.len() - 1 {
                return Err(format!(
                    "[{}] frame-order FAIL: ScreenUpdate at index {} is not the last frame ({} frames; {:?} follow it)",
                    desc,
                    pos,
                    frames.len(),
                    &frames[pos + 1..]
                ));
            }
        }
        many => {
            return Err(format!(
                "[{}] frame-order FAIL: {} ScreenUpdate frames (want exactly 1) at indices {:?}",
                desc,
                many.len(),
                many
            ))
        }
    }
    // Middle frames (between Connected and ScreenUpdate) must be History/Passthrough only.
    for (i, f) in frames[1..frames.len() - 1].iter().enumerate() {
        match f {
            RecordedFrame::History(_) | RecordedFrame::Passthrough(_) => {}
            RecordedFrame::Connected { .. } => {
                return Err(format!(
                    "[{}] frame-order FAIL: second Connected at index {}",
                    desc,
                    i + 1
                ))
            }
            RecordedFrame::ScreenUpdate(_) => unreachable!("handled above"),
        }
    }
    Ok(())
}

/// R1(a) zero-History checker: NO History frame may precede the ScreenUpdate.
/// This is the mid-altscreen claim — a reattach DURING an altscreen app must
/// not replay primary-screen scrollback (session_bridge.rs:94-98 skips history
/// in alt screen). Teeth: R7(c) (a recording WITH a History frame must fail).
fn check_zero_history(frames: &[RecordedFrame], desc: &str) -> Result<(), String> {
    let n = frames
        .iter()
        .filter(|f| matches!(f, RecordedFrame::History(_)))
        .count();
    if n != 0 {
        return Err(format!(
            "[{}] zero-history FAIL: {} History frame(s) present before ScreenUpdate (mid-altscreen must replay none)",
            desc, n
        ));
    }
    Ok(())
}

/// R5(b) whole-line-framing checker: each History frame decodes to WHOLE marker
/// lines, with no marker line split across a frame boundary.
///
/// Falsifiable model (markers are emitted zero-padded to a fixed `width`): every
/// History line that contains `marker` MUST be followed by EXACTLY `width`
/// ASCII digits (a complete index) — a split leaves a SHORT digit run on the
/// tail line of one frame; AND no History line may be a bare leading digit-run
/// fragment with no marker (the orphaned suffix that lands as the head line of
/// the next frame). Either half of a split therefore fails. Teeth: R7(e).
fn check_whole_line_framing(
    frames: &[RecordedFrame],
    marker: &str,
    width: usize,
    desc: &str,
) -> Result<(), String> {
    for (fi, f) in frames.iter().enumerate() {
        let RecordedFrame::History(lines) = f else {
            continue;
        };
        for (li, raw) in lines.iter().enumerate() {
            let text = String::from_utf8_lossy(raw);
            if let Some(pos) = text.find(marker) {
                let after = &text[pos + marker.len()..];
                let digits = after.chars().take_while(|c| c.is_ascii_digit()).count();
                // 0 digits = CHROME (the echoed `seq -f '...%04.0f'` command —
                // marker text followed by `%`). 1..width = SPLIT marker head.
                // Exactly width = whole, ok.
                if digits != 0 && digits != width {
                    return Err(format!(
                        "[{}] whole-line FAIL: marker '{}' at frame {} line {} has {} trailing digits, expected {} (marker split across frame boundary?): {:?}",
                        desc, marker, fi, li, digits, width, text
                    ));
                }
            } else {
                // No marker on this line. A line that is *only* a leading digit
                // run is the orphaned suffix of a split marker.
                let trimmed = text.trim_end_matches(['\r', '\n']);
                if !trimmed.is_empty()
                    && trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
                    && trimmed.chars().all(|c| c.is_ascii_digit())
                {
                    return Err(format!(
                        "[{}] whole-line FAIL: orphaned digit-only fragment {:?} at frame {} line {} (split marker suffix)",
                        desc, trimmed, fi, li
                    ));
                }
            }
        }
    }
    Ok(())
}

/// R5(c) final-CUP checker: the LAST cursor-position escape `\x1b[<row>;<col>H`
/// in the ScreenUpdate payload MUST equal the screen's reported cursor coords
/// (1-based). Byte-presence of *a* CUP is unfalsifiable (the renderer ALWAYS
/// emits one — render.rs:317-325); pinning the FINAL CUP to the known cursor is
/// the falsifiable form. Teeth: R7(e2).
///
/// `expected_row`/`expected_col` are 1-based (render emits `cursor_y+1`,
/// `cursor_x+1`).
fn check_final_cup(
    screen_update: &[u8],
    expected_row: u16,
    expected_col: u16,
    desc: &str,
) -> Result<(), String> {
    let last = last_cup(screen_update).ok_or_else(|| {
        format!(
            "[{}] final-CUP FAIL: no CUP (\\x1b[r;cH) found in ScreenUpdate payload",
            desc
        )
    })?;
    if last != (expected_row, expected_col) {
        return Err(format!(
            "[{}] final-CUP FAIL: last CUP is row={} col={}, expected row={} col={}",
            desc, last.0, last.1, expected_row, expected_col
        ));
    }
    Ok(())
}

/// Parse the LAST `\x1b[<row>;<col>H` (CUP, full form with both params) from a
/// byte buffer. Returns (row, col), 1-based, or None if none present.
fn last_cup(buf: &[u8]) -> Option<(u16, u16)> {
    let mut found = None;
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' {
            // Scan to the CSI final byte.
            let mut j = i + 2;
            while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                j += 1;
            }
            if j < buf.len() && buf[j] == b'H' {
                // Params between i+2 and j: expect "<row>;<col>".
                let params = &buf[i + 2..j];
                if let Some((r, c)) = parse_two_params(params) {
                    found = Some((r, c));
                }
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    found
}

/// Parse "<row>;<col>" (both required) from CSI param bytes. Returns None if not
/// exactly two numeric params (the row-paint CUPs use ";1H" which IS two params;
/// the cursor CUP also uses two — both are accepted, the LAST one wins in caller).
fn parse_two_params(params: &[u8]) -> Option<(u16, u16)> {
    let s = std::str::from_utf8(params).ok()?;
    let mut parts = s.split(';');
    let row: u16 = parts.next()?.parse().ok()?;
    let col: u16 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None; // more than two params — not a plain CUP
    }
    Some((row, col))
}

// ============================================================================
// R5 — hybrid-replay structure
// ============================================================================

/// R5 — frame-level structure of a fresh-attach replay (NON-COLLAPSING record):
///   (a) exact order Connected , History* , ScreenUpdate (no interleaving);
///   (b) each History frame decodes to whole marker lines (no split);
///   (c) the ScreenUpdate's FINAL CUP equals the session's reported cursor.
///
/// (c) determinism: we drive the cursor to a KNOWN coordinate by writing
/// `printf '\033[12;5HsbB3CURSOR'` then `sleep 600` — the printf positions the
/// cursor at 1-based (row 12, col 5) and writes the 10-char literal, so the
/// cursor settles at (row 12, col 15); the sleep keeps the shell from
/// re-prompting and moving it. The render's final CUP must therefore be
/// `\x1b[12;15H`. The expected column is computed from the literal length below
/// so the assertion can never drift from the driver.
#[test]
fn r5_hybrid_replay_structure() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("b3_r5")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "b3r5")?;
    let session = "b3r5";
    create_session(&socket, session)?;
    let ev = evidence_dir("R5");

    // Produce marker lines, then FILLER (> 24 visible rows) so EVERY marker
    // line scrolls into HISTORY (the visible grid holds only the recent filler
    // tail). Without the filler the last ~23 markers would sit on the visible
    // screen — in the ScreenUpdate, NOT in History — and R0 over the full marker
    // range would (correctly) report them missing from history_lines().
    // marker width 4; 60 markers; 40 filler lines (> 24 rows).
    let marker = "b3r5-mark-";
    let width = 4;
    send_to_session(&socket, &env, session, "seq -f 'b3r5-mark-%04.0f' 1 60\n")?;
    send_to_session(&socket, &env, session, "seq -f 'b3r5-fill-%03.0f' 1 40\n")?;
    wait_for_text(&socket, session, Duration::from_secs(30), |t| {
        t.contains("b3r5-fill-040")
    })?;

    // Drive the cursor to a known coordinate and PIN it there with a long sleep
    // (no re-prompt). printf places the cursor at 1-based (12,5), writes the
    // literal, then leaves the cursor after it.
    let literal = "sbB3CURSOR";
    let expected_row = 12u16;
    let expected_col = 5u16 + literal.len() as u16; // 1-based col after the writes
    send_to_session(
        &socket,
        &env,
        session,
        &format!("printf '\\033[12;5H{}'; sleep 600\n", literal),
    )?;
    // Let the printf land (the cursor escape is app output, deterministic).
    std::thread::sleep(Duration::from_millis(800));

    // Authoritative NON-COLLAPSING recording.
    let rec = record_attach(&socket, session)?;

    // (a) frame order.
    check_replay_frame_order(&rec.frames, "R5(a)").map_err(to_err)?;
    // (b) whole-line framing across every History frame.
    check_whole_line_framing(&rec.frames, marker, width, "R5(b)").map_err(to_err)?;
    // ... and R0 over the recorded History lines (wire order), confirming the
    // produced markers are present, ordered, in-range (151..=200 not used here;
    // R5 just needs the framing to be whole + ordered over what was produced).
    assert_backlog_ordered(&rec.history_lines(), marker, 1..=60, "R5(R0)").map_err(to_err)?;
    // (c) final CUP equals the pinned cursor.
    let su = rec
        .screen_update()
        .ok_or("R5(c): no ScreenUpdate in recording")?;
    check_final_cup(su, expected_row, expected_col, "R5(c)").map_err(to_err)?;

    let hist_frames = rec.history_frame_count();
    fs::write(ev.join("R5_screenupdate.bin"), su)?;
    fs::write(
        ev.join("R5_result.txt"),
        format!(
            "R5 PASS\n(a) frame order Connected,History*,ScreenUpdate — {} History frame(s), 1 ScreenUpdate last\n\
             (b) whole-line framing: all History marker lines have exactly {} trailing digits\n\
             (R0) recorded History lines: markers 1..=60 present, ordered, in-range\n\
             (c) final CUP = row {} col {} (pinned via printf+sleep)\n\
             screenupdate_sha256={}\n",
            hist_frames, width, expected_row, expected_col, sha256(su)
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R1 — reattach-during-altscreen (REDUCED to the new claim, red-team #1)
// ============================================================================

/// R1 — the NEW claim only (G2-equivalent surface checks are the lead's adoption
/// `adopted:g2_altscreen_stress`, NOT re-done here):
///   (a) a fresh attach taken WHILE an altscreen app (`less`) runs decodes to
///       ZERO History frames before the ScreenUpdate, and
///   (b) the ScreenUpdate renders the app's marker.
///   (c) a fresh attach taken AFTER the app exits has History frames PRESENT
///       whose decoded lines satisfy R0 over the pre-app sentinel range.
///
/// Pre-app sentinel count = 50 lines (≥2× the 24 visible rows, red-team #14) so
/// non-empty post-exit History is STRUCTURALLY required, not luck. `less` is
/// driven exactly as G2 (integration_tests.rs:196) — technique mirrored, that
/// test NOT called.
#[test]
fn r1_reattach_during_altscreen() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("b3_r1")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "b3r1")?;
    let session = "b3r1";
    create_session(&socket, session)?;
    let ev = evidence_dir("R1");

    // Pre-app sentinels: 50 numbered lines (> 2× 24 visible rows, red-team #14),
    // then FILLER (> 24 rows) so ALL 50 sentinels are in HISTORY before less
    // launches and remain there after it exits (the restored primary screen
    // shows only the recent filler tail). marker width 4.
    let marker = "b3r1-pre-";
    send_to_session(&socket, &env, session, "seq -f 'b3r1-pre-%04.0f' 1 50\n")?;
    send_to_session(&socket, &env, session, "seq -f 'b3r1-fill-%03.0f' 1 40\n")?;
    wait_for_text(&socket, session, Duration::from_secs(30), |t| {
        t.contains("b3r1-fill-040")
    })?;

    // File for less with a marker that exists NOWHERE else.
    let file = jail.tmpdir.join("b3r1_file.txt");
    let content: String = (1..=120)
        .map(|i| format!("B3R1-FILE-{:03}-altmarker\n", i))
        .collect();
    fs::write(&file, &content)?;

    // Run less (altscreen app) — same technique as G2.
    send_to_session(
        &socket,
        &env,
        session,
        &format!("less {}\n", file.display()),
    )?;
    // Wait until the app marker is on screen (app is up in alt screen).
    wait_for_text(&socket, session, Duration::from_secs(30), |t| {
        t.contains("altmarker")
    })?;

    // (a)/(b) MID-APP recording: zero History, ScreenUpdate renders the marker.
    let rec_during = record_attach(&socket, session)?;
    check_replay_frame_order(&rec_during.frames, "R1-during(order)").map_err(to_err)?;
    check_zero_history(&rec_during.frames, "R1(a)").map_err(to_err)?;
    let su_during = rec_during
        .screen_update()
        .ok_or("R1(b): no ScreenUpdate mid-app")?;
    let su_during_text = strip_ansi(&String::from_utf8_lossy(su_during));
    if !su_during_text.contains("altmarker") {
        return Err(format!(
            "R1(b) FAIL: mid-app ScreenUpdate does not render app marker (text head: {:?})",
            &su_during_text.chars().take(200).collect::<String>()
        )
        .into());
    }
    fs::write(ev.join("R1_during_screenupdate.bin"), su_during)?;

    // Quit less.
    send_to_session(&socket, &env, session, "q")?;
    // Wait for restore (pre-app sentinel back, app marker gone from primary).
    wait_for_text(&socket, session, Duration::from_secs(30), |t| {
        t.contains("b3r1-pre-0050") && !t.contains("altmarker")
    })?;

    // (c) POST-EXIT recording: History present + R0 over the sentinel range.
    let rec_after = record_attach(&socket, session)?;
    check_replay_frame_order(&rec_after.frames, "R1-after(order)").map_err(to_err)?;
    if rec_after.history_frame_count() == 0 {
        return Err(
            "R1(c) FAIL: post-exit recording has ZERO History frames (sentinels must replay)"
                .into(),
        );
    }
    assert_backlog_ordered(&rec_after.history_lines(), marker, 1..=50, "R1(c)").map_err(to_err)?;
    fs::write(
        ev.join("R1_after_screenupdate.bin"),
        rec_after.screen_update().unwrap_or(&[]),
    )?;

    fs::write(
        ev.join("R1_result.txt"),
        format!(
            "R1 PASS (new claim only; G2 surface checks = adopted:g2_altscreen_stress, not here)\n\
             pre-app sentinels: 50 (>= 2x 24 visible rows)\n\
             (a) mid-app recording: {} History frames before ScreenUpdate (must be 0)\n\
             (b) mid-app ScreenUpdate renders 'altmarker'\n\
             (c) post-exit recording: {} History frame(s); R0 over b3r1-pre- 1..=50 holds\n",
            rec_during.history_frame_count(),
            rec_after.history_frame_count(),
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R4 — scrollback-boundary content (integration half)
// ============================================================================

/// R4 (integration half; unit half is the grid test) — history ring = 50, but
/// the daemon's session history is fixed at 10000 by the Connect path, so we
/// instead exercise eviction at the SESSION level is not directly tunable from
/// the test. Per spec REV 3 red-team #9 the integration row uses a SMALL ring:
/// we create the session, produce 200 marker lines, and assert the frame-decoded
/// History satisfies R0 over the SURVIVING window with the evicted prefix
/// absent. Because the protocol Connect fixes history=10000 (client.rs), all 200
/// survive; the eviction-boundary semantics are proven by the UNIT half against
/// a genuinely-capped grid. Here we assert the FULL ordered window (1..=200) is
/// present, in order, in-range — the ordered-replay invariant over a large
/// production run — and explicitly that marker-0 (never produced) is absent.
///
/// NOTE TO LEAD/QA: the spec's "history=50 → R0 over 151..=200" assumes a
/// 50-line session ring. The daemon's Connect path hard-codes history=10000
/// (crates/sbmux/tests/lib/client.rs connect_session / record_attach), and the
/// protocol exposes no per-session ring override to a test. Rather than silently
/// weaken, this is REPORTED: the eviction-content boundary is fully proven by
/// the UNIT half (cap=10, lines 1-5 evicted); the integration half proves the
/// ORDERED frame-decoded replay over a 200-line run. See report.
#[test]
fn r4_scrollback_boundary_integration() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("b3_r4")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "b3r4")?;
    let session = "b3r4";
    create_session(&socket, session)?;
    let ev = evidence_dir("R4");

    let marker = "b3r4-mark-";
    send_to_session(&socket, &env, session, "seq -f 'b3r4-mark-%04.0f' 1 200\n")?;
    // FILLER (> 24 rows) so all 200 markers land in HISTORY (recent markers
    // would otherwise sit on the visible grid / ScreenUpdate, not History).
    send_to_session(&socket, &env, session, "seq -f 'b3r4-fill-%03.0f' 1 40\n")?;
    wait_for_text(&socket, session, Duration::from_secs(60), |t| {
        t.contains("b3r4-fill-040")
    })?;

    // Settle: poll the collapsing capture until two consecutive captures agree
    // (production quiesced), then take the authoritative non-collapsing record.
    let mut last_len = 0usize;
    for _ in 0..40 {
        let c = capture_session(&socket, session, 150)?;
        let cur = c.text().len();
        if cur == last_len && c.text().contains("b3r4-fill-040") {
            break;
        }
        last_len = cur;
        std::thread::sleep(Duration::from_millis(250));
    }
    let rec = record_attach(&socket, session)?;
    check_replay_frame_order(&rec.frames, "R4(order)").map_err(to_err)?;

    // The last 200 produced markers replay as whole lines, ordered, in range.
    // (All 200 survive the 10000 ring; surviving window == full window here.)
    let hist = rec.history_lines();
    // R0 over the FULL produced range; clause (c) guarantees no out-of-range
    // index — i.e. marker-0 (never produced) cannot appear.
    assert_backlog_ordered(&hist, marker, 1..=200, "R4-int").map_err(to_err)?;

    // Explicit marker-1-absent style check from the spec, adapted: marker 0000
    // was never produced and MUST be absent from the frame-decoded History.
    let has_zero = hist.iter().any(|l| {
        let t = String::from_utf8_lossy(l);
        t.contains("b3r4-mark-0000")
    });
    if has_zero {
        return Err("R4-int FAIL: phantom marker b3r4-mark-0000 present in History".into());
    }

    fs::write(
        ev.join("R4_result.txt"),
        format!(
            "R4 PASS (integration half)\n\
             produced 200 marker lines; frame-decoded History satisfies R0 over 1..=200\n\
             (ordered, each-once, no out-of-range) — {} History frame(s)\n\
             phantom marker b3r4-mark-0000 absent\n\
             NOTE: spec's 50-ring/151..=200 needs a per-session ring override the\n\
             protocol does not expose (Connect history=10000). Eviction-content\n\
             boundary is proven by the UNIT half (grid cap=10, 1-5 evicted).\n",
            rec.history_frame_count()
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// R7 — negative controls (teeth). Each feeds MUTATED/synthetic input to the
// SAME checker used by the positive arm and REQUIRES Err. (a)(b)(c)(d)(e)(e2).
// ============================================================================

/// R7(a): frame-order checker fed a History frame AFTER the ScreenUpdate MUST fail.
#[test]
fn r7a_frame_order_history_after_screenupdate_fails() {
    let frames = vec![
        RecordedFrame::Connected {
            name: "x".into(),
            new_session: false,
        },
        RecordedFrame::History(vec![b"a".to_vec()]),
        RecordedFrame::ScreenUpdate(b"\x1b[1;1H".to_vec()),
        RecordedFrame::History(vec![b"b".to_vec()]), // illegal: after ScreenUpdate
    ];
    assert!(
        check_replay_frame_order(&frames, "R7a").is_err(),
        "R7(a): History-after-ScreenUpdate must fail the frame-order checker"
    );
}

/// R7(b): the R4 boundary-content checker fed mutated eviction inputs MUST fail.
/// This is a REAL tooth: it bites `check_boundary_content` — the SAME shared
/// checker the R4 unit test (`r4_scrollback_boundary_eviction_content`) runs on
/// the live grid's scrollback read-back — not a literal tautology. Two mutants:
///   (i)  OFF-BY-ONE: 11 indices kept at cap 10 (5..=15) → length violation;
///   (ii) WRONG WINDOW: 10 indices (correct length) but the wrong window
///        (5..=14 when 6..=15 is expected) → a len-only check would miss this,
///        the window-equality catches it.
/// Plus a positive arm so the tooth bites the MUTATION, not an always-Err fn.
#[test]
fn r7b_boundary_off_by_one_fails() {
    // (i) off-by-one (cap+1 kept).
    let off_by_one: Vec<usize> = (5..=15).collect();
    assert!(
        check_boundary_content(&off_by_one, 10, 15).is_err(),
        "R7(b)(i): 11 kept at cap 10 must fail the boundary-content checker"
    );
    // (ii) wrong window of the correct length.
    let wrong_window: Vec<usize> = (5..=14).collect();
    assert!(
        check_boundary_content(&wrong_window, 10, 15).is_err(),
        "R7(b)(ii): wrong window 5..=14 (when 6..=15 expected) must fail"
    );
    // positive arm: the true recent window passes (checker not vacuously Err).
    let correct: Vec<usize> = (6..=15).collect();
    assert!(
        check_boundary_content(&correct, 10, 15).is_ok(),
        "R7(b): the correct most-recent window must pass"
    );
}

/// R7(c): zero-History checker fed a mid-altscreen replay WITH a History frame
/// MUST fail.
#[test]
fn r7c_zero_history_with_history_fails() {
    let frames = vec![
        RecordedFrame::Connected {
            name: "x".into(),
            new_session: false,
        },
        RecordedFrame::History(vec![b"leaked-scrollback".to_vec()]), // illegal mid-alt
        RecordedFrame::ScreenUpdate(b"\x1b[1;1H".to_vec()),
    ];
    assert!(
        check_zero_history(&frames, "R7c").is_err(),
        "R7(c): a mid-altscreen replay WITH a History frame must fail zero-history"
    );
}

/// R7(d): R0 fed a complete-but-REORDERED sequence MUST fail; and fed an
/// out-of-range extra index MUST fail.
#[test]
fn r7d_r0_reordered_and_out_of_range_fail() {
    let m = "b3-";
    // Reordered: 1,3,2 — clause (b) strictly-increasing must fail.
    let reordered: Vec<Vec<u8>> = [1usize, 3, 2]
        .iter()
        .map(|i| format!("{}{:04}", m, i).into_bytes())
        .collect();
    assert!(
        assert_backlog_ordered(&reordered, m, 1..=3, "R7d-reorder").is_err(),
        "R7(d): reordered sequence must fail R0 clause (b)"
    );

    // Out-of-range extra: range 1..=3 but a 4 is present — clause (c) must fail.
    let extra: Vec<Vec<u8>> = [1usize, 2, 3, 4]
        .iter()
        .map(|i| format!("{}{:04}", m, i).into_bytes())
        .collect();
    assert!(
        assert_backlog_ordered(&extra, m, 1..=3, "R7d-extra").is_err(),
        "R7(d): out-of-range index must fail R0 clause (c)"
    );
}

/// R7(e): R5(b) whole-line checker fed a mid-line-split frame PAIR MUST fail.
/// Frame A's tail line carries a SHORT (split) marker; frame B's head line is
/// the orphaned digit suffix. Either half failing satisfies the tooth; we
/// assert the checker rejects the pair.
#[test]
fn r7e_whole_line_split_pair_fails() {
    let marker = "b3rep-";
    let width = 4;
    let frames = vec![
        RecordedFrame::Connected {
            name: "x".into(),
            new_session: false,
        },
        // Frame A: last line is a SPLIT marker — only 2 of 4 digits.
        RecordedFrame::History(vec![b"b3rep-0040".to_vec(), b"b3rep-00".to_vec()]),
        // Frame B: head line is the orphaned digit suffix.
        RecordedFrame::History(vec![b"41".to_vec(), b"b3rep-0042".to_vec()]),
        RecordedFrame::ScreenUpdate(b"\x1b[1;1H".to_vec()),
    ];
    assert!(
        check_whole_line_framing(&frames, marker, width, "R7e").is_err(),
        "R7(e): a mid-line-split frame pair must fail whole-line framing"
    );
}

/// R7(e2): R5(c) final-CUP checker fed a repaint whose final CUP DISAGREES with
/// the screen's cursor coords MUST fail.
#[test]
fn r7e2_final_cup_mismatch_fails() {
    // Payload whose LAST CUP is (5,9) but we claim the cursor is (12,14).
    let payload = b"\x1b[1;1Hrow\x1b[5;9H".to_vec();
    assert!(
        check_final_cup(&payload, 12, 14, "R7e2").is_err(),
        "R7(e2): final CUP disagreeing with reported cursor must fail"
    );
    // Sanity (positive arm): the same payload with the matching coords passes,
    // proving the tooth bites the MISMATCH, not the checker being always-Err.
    assert!(
        check_final_cup(&payload, 5, 9, "R7e2-pos").is_ok(),
        "R7(e2): matching final CUP must pass (checker not vacuously failing)"
    );
}

// Positive self-tests for the structure checkers (prove they are not
// vacuously-Err — the falsifiability counterpart to the teeth above).
#[test]
fn checkers_positive_self_tests() {
    let frames = vec![
        RecordedFrame::Connected {
            name: "x".into(),
            new_session: true,
        },
        RecordedFrame::History(vec![b"b3-0001".to_vec(), b"b3-0002".to_vec()]),
        RecordedFrame::ScreenUpdate(b"\x1b[0;0H\x1b[12;14H".to_vec()),
    ];
    assert!(check_replay_frame_order(&frames, "pos").is_ok());
    assert!(check_whole_line_framing(&frames, "b3-", 4, "pos").is_ok());
    assert!(check_final_cup(frames[2..][0].screen_bytes().unwrap(), 12, 14, "pos").is_ok());
    // zero-history must FAIL here (this recording HAS History) — confirms the
    // checker discriminates.
    assert!(check_zero_history(&frames, "pos").is_err());
}

// ============================================================================
// R0 RIDER (orchestrator) — blindness proof + ordered re-verification of the
// adopted scenario SHAPES. Per the rider's CRITICAL ROUTING RULE: if any
// adopted shape passes the OLD order-blind comparator but FAILS the ordered
// comparator, that is a B2-gate regression — the test fails LOUDLY with evidence
// and must route to the orchestrator UNMODIFIED (do not "fix" it here).
// ============================================================================

/// RIDER 2a — BLINDNESS PROOF. The same complete-but-REORDERED marker sequence
/// (i) PASSES the old order-blind `assert_backlog_completeness` (len + non-empty
/// only) yet (ii) FAILS the new wire-order `assert_backlog_ordered` (clause (b)
/// strictly-increasing). This demonstrates the order-blindness we claimed in the
/// gap table (assertions.rs:21-47 is order-blind) was real and that R0 closes it.
#[test]
fn r0_rider_blindness_proof_old_comparator_passes_reordered() {
    let marker = "b3-mark-";
    // A COMPLETE set {1,2,3,4,5} but in scrambled wire order.
    let order = [1usize, 5, 2, 4, 3];
    let as_strings: Vec<String> = order
        .iter()
        .map(|i| format!("{}{:04}", marker, i))
        .collect();
    let as_bytes: Vec<Vec<u8>> = as_strings.iter().map(|s| s.clone().into_bytes()).collect();

    // (i) OLD order-blind comparator PASSES (5 non-empty lines, count met).
    assert!(
        assert_backlog_completeness(&as_strings, 5, "rider-blind").is_ok(),
        "RIDER 2a: old comparator must be order-BLIND — it should PASS the reordered set"
    );

    // (ii) NEW ordered comparator FAILS (clause (b): 5 follows 1, not increasing).
    let ordered = assert_backlog_ordered(&as_bytes, marker, 1..=5, "rider-blind");
    assert!(
        ordered.is_err(),
        "RIDER 2a: new ordered comparator must FAIL the reordered set (order-blindness closed)"
    );
    // The error must name clause (b) — proves it failed on ORDER, not presence.
    let msg = ordered.unwrap_err();
    assert!(
        msg.contains("FAIL (b)"),
        "RIDER 2a: ordered failure must be on clause (b) [order], got: {}",
        msg
    );
}

/// Order-blind PRESENCE baseline: every index in `range` appears AT LEAST ONCE
/// somewhere in `lines` (substring `marker<zero-padded>`), ORDER IGNORED. This
/// is the semantic property the old order-blind comparator embodied
/// (completeness without order) applied to the marker view — unlike
/// `assert_backlog_completeness`, it does not choke on the blank/chrome lines a
/// real frame-decoded History carries, so it is the honest "blind passes"
/// precondition for the routing rule. `width` is the zero-pad width.
fn order_blind_presence(
    lines: &[Vec<u8>],
    marker: &str,
    range: std::ops::RangeInclusive<usize>,
    width: usize,
) -> Result<(), String> {
    let hay: Vec<String> = lines
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect();
    for want in range.clone() {
        let needle = format!("{}{:0w$}", marker, want, w = width);
        if !hay.iter().any(|l| l.contains(&needle)) {
            return Err(format!(
                "order-blind presence: index {} (needle {:?}) absent",
                want, needle
            ));
        }
    }
    Ok(())
}

/// Apply BOTH comparators over `ordered_range` of `marker` to a frame-decoded
/// recording: the order-blind PRESENCE baseline (see `order_blind_presence`) and
/// the new wire-order `assert_backlog_ordered`. `width` = zero-pad width.
///
/// CRITICAL ROUTING RULE:
///   - ordered PASSES                         → Ok(()) (adopted shape holds under R0);
///   - blind PASSES but ordered FAILS         → Err(REGRESSION …): a B2-gate
///     regression — stop, report to orchestrator UNMODIFIED, do NOT fix;
///   - blind ALSO fails                       → Err(setup defect): not the
///     routing case (range mis-sized / data absent), surfaced for the harness.
fn ordered_reverify(
    rec: &libmod::client::Recorded,
    marker: &str,
    ordered_range: std::ops::RangeInclusive<usize>,
    width: usize,
    shape: &str,
) -> Result<(), Box<dyn Error>> {
    let hist_bytes = rec.history_lines();
    let blind = order_blind_presence(&hist_bytes, marker, ordered_range.clone(), width);
    let ordered = assert_backlog_ordered(&hist_bytes, marker, ordered_range.clone(), shape);

    match (blind.is_ok(), ordered.is_ok()) {
        (_, true) => Ok(()), // ordered holds — adopted shape passes under R0
        (true, false) => {
            // ROUTING RULE: order-blind passed but ordered failed → regression.
            Err(format!(
                "REGRESSION (route to orchestrator UNMODIFIED): adopted shape '{}' PASSES the \
                 order-blind PRESENCE baseline but FAILS the ordered comparator over {}..={}. \
                 Ordered error: {}. Frame-decoded History had {} lines.",
                shape,
                ordered_range.start(),
                ordered_range.end(),
                ordered.unwrap_err(),
                hist_bytes.len()
            )
            .into())
        }
        (false, false) => {
            // Both failed — not the routing case; surface both so a mis-sized
            // range / absent data is visible (harness defect, not a regression).
            Err(format!(
                "RIDER setup defect (NOT a regression — order-blind ALSO failed): shape '{}' \
                 blind_err={:?} ordered_err={:?} (history lines: {})",
                shape,
                blind.err(),
                ordered.err(),
                hist_bytes.len()
            )
            .into())
        }
    }
}

/// RIDER 2b(i) — G6-SHAPE under the ordered comparator (`adopted:g6_reattach_replay`
/// re-driven as a SCENARIO SHAPE, frame-decoded). N pre-detach + M
/// detached-production numbered lines, detach (zero clients), produce the
/// detached batch, fresh frame-decoded attach, R0 over a range that is FULLY
/// scrolled into HISTORY (the visible tail is in the ScreenUpdate, not History —
/// my own M3a finding; ranges are sized below to stay clear of the visible 24
/// rows). Routing rule applies via `ordered_reverify`.
///
/// Adoption-mapping note: G2's ordered coverage is already R1(c)
/// (`r1_reattach_during_altscreen`) — not re-done here. Unit-only adoptions
/// (`adopted:altscreen-1049-unit`, `adopted:cell-orphan-units`) are N/A for R0:
/// R0's input contract is the frame-decoded wire History vector, which a
/// grid/cell unit test never produces — nothing to re-verify under R0.
#[test]
fn r0_rider_g6_shape_ordered() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("b3_r0g6")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "b3r0g6")?;
    let session = "b3r0g6";
    create_session(&socket, session)?;
    let ev = evidence_dir("R0-rider-g6");

    let marker = "g6shape-";
    // Pre-detach: 60 numbered lines (attached client, then detach).
    let attached = AttachedClient::attach(&socket, session)?;
    send_to_session(&socket, &env, session, "seq -f 'g6shape-%04.0f' 1 60\n")?;
    {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !attached.captured_text().contains("g6shape-0060") {
            if Instant::now() > deadline {
                return Err(
                    "R0-rider-g6: pre-detach sentinel never reached attached client".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    attached.close();
    std::thread::sleep(Duration::from_millis(300));

    // Detached production (zero clients): 60 more lines, then FILLER (> 24 rows)
    // so the WHOLE 1..=120 numbered block is pushed into history.
    send_to_session(&socket, &env, session, "seq -f 'g6shape-%04.0f' 61 120\n")?;
    send_to_session(&socket, &env, session, "seq -f 'g6fill-%03.0f' 1 40\n")?;
    wait_for_text(&socket, session, Duration::from_secs(60), |t| {
        t.contains("g6fill-040")
    })?;
    // Settle (two equal-length captures).
    let mut last = 0usize;
    for _ in 0..40 {
        let c = capture_session(&socket, session, 150)?;
        let n = c.text().len();
        if n == last && c.text().contains("g6fill-040") {
            break;
        }
        last = n;
        std::thread::sleep(Duration::from_millis(250));
    }

    // Frame-decoded record + ordered re-verify over the full numbered range
    // (all 120 are in history now; filler is chrome under `marker`).
    let rec = record_attach(&socket, session)?;
    check_replay_frame_order(&rec.frames, "R0-rider-g6(order)").map_err(to_err)?;
    let verdict = ordered_reverify(&rec, marker, 1..=120, 4, "g6_reattach_replay");
    let outcome = match &verdict {
        Ok(()) => "PASS (ordered holds)".to_string(),
        Err(e) => format!("FAIL: {}", e),
    };
    fs::write(
        ev.join("R0_rider_g6_result.txt"),
        format!(
            "R0 RIDER 2b(i) — adopted:g6_reattach_replay shape under ordered comparator\n\
             pre-detach 1..=60 (attached, then detached) + detached 61..=120 + 40 filler\n\
             frame-decoded History lines: {}\n\
             R0 over g6shape- 1..=120: {}\n\
             routing rule: order-blind-pass + ordered-fail => REGRESSION to orchestrator\n",
            rec.history_lines().len(),
            outcome
        ),
    )?;
    verdict?; // propagate; a regression here fails the test LOUDLY with evidence

    teardown_jail(&jail)?;
    Ok(())
}

/// RIDER 2b(ii) — NEGCTL-HEALTHY-SHAPE under the ordered comparator
/// (`adopted:negctl-breaker` healthy arm re-driven as a SCENARIO SHAPE,
/// frame-decoded). A 3000-line healthy run, frame-decoded, R0 over the
/// in-history range (3000 produced + filler → assert over 1..=2900, comfortably
/// clear of the visible 24-row tail). Routing rule applies via `ordered_reverify`.
#[test]
fn r0_rider_negctl_healthy_shape_ordered() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("b3_r0nc")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "b3r0nc")?;
    let session = "b3r0nc";
    create_session(&socket, session)?;
    let ev = evidence_dir("R0-rider-negctl");

    let marker = "ncshape-";
    // 3000-line healthy run + filler so the asserted range is fully in history.
    send_to_session(&socket, &env, session, "seq -f 'ncshape-%05.0f' 1 3000\n")?;
    send_to_session(&socket, &env, session, "seq -f 'ncfill-%03.0f' 1 40\n")?;
    wait_for_text(&socket, session, Duration::from_secs(120), |t| {
        t.contains("ncfill-040")
    })?;
    // Settle.
    let mut last = 0usize;
    for _ in 0..60 {
        let c = capture_session(&socket, session, 150)?;
        let n = c.text().len();
        if n == last && c.text().contains("ncfill-040") {
            break;
        }
        last = n;
        std::thread::sleep(Duration::from_millis(250));
    }

    let rec = record_attach(&socket, session)?;
    check_replay_frame_order(&rec.frames, "R0-rider-negctl(order)").map_err(to_err)?;
    // Assert over the FULL 1..=3000 range: the 40-line filler (> 24 visible rows)
    // pushes EVERY numbered line into history; only the filler tail sits on the
    // ScreenUpdate. (An earlier 1..=2900 attempt mis-judged the boundary and
    // tripped clause (c) on index 2901 — which IS in history. width 5.)
    let verdict = ordered_reverify(&rec, marker, 1..=3000, 5, "negctl_healthy");
    let outcome = match &verdict {
        Ok(()) => "PASS (ordered holds)".to_string(),
        Err(e) => format!("FAIL: {}", e),
    };
    fs::write(
        ev.join("R0_rider_negctl_result.txt"),
        format!(
            "R0 RIDER 2b(ii) — adopted:negctl-breaker (healthy arm) shape under ordered comparator\n\
             3000-line healthy run + 40 filler; frame-decoded\n\
             frame-decoded History lines: {}\n\
             R0 over ncshape- 1..=3000 (all in history via filler): {}\n\
             routing rule: order-blind-pass + ordered-fail => REGRESSION to orchestrator\n",
            rec.history_lines().len(),
            outcome
        ),
    )?;
    verdict?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// Local helpers
// ============================================================================

/// Convert a checker's `String` error into a boxed test error.
fn to_err(s: String) -> Box<dyn Error> {
    s.into()
}

/// Small accessor used only by the positive self-test.
trait ScreenBytes {
    fn screen_bytes(&self) -> Option<&[u8]>;
}
impl ScreenBytes for RecordedFrame {
    fn screen_bytes(&self) -> Option<&[u8]> {
        match self {
            RecordedFrame::ScreenUpdate(b) => Some(b),
            _ => None,
        }
    }
}
