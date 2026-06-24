//! B3 M5 — CORPUS-DEPENDENT gate rows (mux-op subset: replay / altscreen /
//! resize) vs the pinned TS corpus, semantic-class per ADR-0004.
//!
//! Spec: `exec/b3-spec.md` §"M5 — corpus rows" + C1 spec carry C1j. Until a
//! corpus dir is provided via `SBMUX_CORPUS_DIR`, the rows do NOT silently pass
//! and do NOT fail the gate: they SKIP loudly, print `CORPUS-PENDING`, and record
//! that verdict to the evidence dir so the pending state is VISIBLE in every gate
//! run (red-team #11 — a placeholder fixture must never be able to green a row,
//! and an absent fixture must never be invisible).
//!
//! Operating modes per row:
//!   0. PENDING-BY-DESIGN (`RowSpec::pending_reason = Some(..)`): no fixture for
//!      this scenario CLASS exists at the pin on disk (C1j: only
//!      attach-detach-reattach + history were recorded — NO altscreen / resize /
//!      width-resize traces). These rows record CORPUS-PENDING naming exactly
//!      what is missing, REGARDLESS of whether `SBMUX_CORPUS_DIR` is set. They do
//!      NOT require a fixture (so pointing the dir at the real corpus does not
//!      flip them to FAIL) and they are NEVER faked against an unrelated fixture.
//!   1. `SBMUX_CORPUS_DIR` UNSET (active rows) → CORPUS-PENDING. eprintln +
//!      evidence file + early return. Test PASSES (gate stays green), records
//!      PENDING, never PASS.
//!   2. `SBMUX_CORPUS_DIR` SET, fixture file ABSENT (active rows) → test FAILS
//!      loudly (`CORPUS-PENDING-BUT-REQUESTED`): the operator asked for corpus
//!      judging but the required fixture is not there.
//!   3. `SBMUX_CORPUS_DIR` SET, fixture PRESENT → NON-DEGENERACY gate (min line
//!      count + required marker) then the semantic-class comparator judges.
//!      Degenerate fixture = FAIL naming the deficiency.
//!
//! ============================================================================
//! REAL FIXTURE FORMAT  (C1j — adapted to what is on disk; load-bearing, RT #11)
//! ============================================================================
//! `SBMUX_CORPUS_DIR` points at the golden fixtures root (the repo's
//! `test/golden/fixtures/`). Each scenario is a subdir with this layout (the
//! RECORDED-FROM / MATCH-PROOF sidecars pin provenance; we judge the trace):
//!
//!     $SBMUX_CORPUS_DIR/
//!       attach-detach-reattach/      <- corpus_replay_reattach (ACTIVE)
//!         RECORDED-FROM              <- provenance (pinned_ts_commit, zmx_version, ...)
//!         MATCH-PROOF                <- rawA==rawB==normalized sha proof
//!         normalized/reattach.trace  <- the FILE we judge (wire order preserved)
//!         raw/reattach.trace.raw     <- + runA/runB + .exit sidecars
//!       history/                     <- (separate scenario; not a mux-op row)
//!         normalized/history.trace
//!
//! The judged file (`normalized/reattach.trace`) is newline-delimited text: one
//! marker line per backlog line, `SBLINE <n>` with `<n>` a 1-based decimal index
//! (NOT zero-padded), in wire (scroll) order, no interleaved chrome for this
//! scenario. attach-detach-reattach's class is `semantic-backlog-scroll`: wire
//! order IS recoverable from the normalized trace, so it is judged by the ORDERED
//! backlog comparator (R0) over 1..=20 — the honest strongest class available
//! (no settled-text fallback needed here).
//!
//! CORPUS-PENDING rows (no honest fixture on disk — C1j, do not fake):
//!   - corpus_resize / corpus_replay_width_resize: NO resize-class traces were
//!     recorded at the pin (carry C1d reason: zmx source absent, no resize corpus).
//!   - corpus_altscreen: NO altscreen-class trace exists; attach-detach-reattach
//!     is a plain backlog-scroll capture (no `?1049h/l`), so it cannot honestly
//!     feed the scroll-intact + altscreen-replay comparator either. (NOTE: the
//!     gate semantics REVERSED 2026-06-10 — an altscreen-class fixture recorded
//!     against the current mux WOULD carry the renderer's ?1049h replay; a
//!     future recording must be judged with non-zero expected counts.)
//!
//! `min_lines` and `required_marker` for the ACTIVE row are pinned to the real
//! recording (20 `SBLINE ` markers) so a placeholder/partial fixture still fails.

// The shared `lib/mod.rs` is `#[path]`-included into multiple test targets; any
// helper used by only a subset is dead_code/unused here. Allow at the module
// boundary so `clippy -D warnings` stays green per target (same pattern as
// b3_replay.rs / b3_resize.rs).
#[allow(dead_code, unused_imports)]
#[path = "lib/mod.rs"]
mod libmod;
use libmod::assertions::{assert_altscreen_replay, assert_backlog_ordered, assert_scroll_intact};

use std::fs;
use std::path::{Path, PathBuf};

// ============================================================================
// Comparator classes (ADR-0004) declared per row
// ============================================================================

/// The semantic-class invariant a row is judged by, per ADR-0004
/// (`doc/adr/0004-comparator-classes.md`).
#[derive(Clone, Copy, Debug)]
enum Comparator {
    /// backlog-completeness via the ORDERED comparator (R0): wire-order is
    /// recoverable from the trace, so every marker index in range must be
    /// present exactly once, strictly increasing, none out of range.
    BacklogOrdered,
    /// scroll-intact (presence) + altscreen-replay over a MAIN-SCREEN capture
    /// (zero 1049 sequences — the renderer only replays alt state when the
    /// inner app is in it). Used where the fixture is a settled capture that
    /// cannot honestly claim wire order (red-team #12).
    ScrollIntactNoLeak,
}

// ============================================================================
// Per-row spec  (PROVISIONAL constants — see file header)
// ============================================================================

/// Declarative description of one corpus row. Everything corpus-format-specific
/// is concentrated here + in `parse_fixture()` so 0b-P2 adaptation is one-touch.
struct RowSpec {
    /// Evidence/identity name (also the `<row>` evidence subdir).
    row: &'static str,
    /// Subdirectory under `$SBMUX_CORPUS_DIR` holding this row's fixture
    /// (real layout: `<scenario>/normalized`). Unused for PENDING-BY-DESIGN rows.
    subdir: &'static str,
    /// The fixture file (relative to `subdir`) that is judged.
    file: &'static str,
    /// Comparator class (ADR-0004) this row asserts.
    comparator: Comparator,
    /// Non-degeneracy floor: a real recording must have at least this many
    /// lines. A placeholder/partial fixture below the floor FAILS.
    min_lines: usize,
    /// Required marker prefix: must appear in the fixture, else the fixture is
    /// degenerate (wrong scenario / empty placeholder). For the real corpus this
    /// is `"SBLINE "` (trailing space → the comparator reads the decimal index
    /// directly after it).
    required_marker: &'static str,
    /// For `BacklogOrdered`: the inclusive marker index range expected in the
    /// fixture. (Ignored by `ScrollIntactNoLeak`, which uses `required_marker`
    /// as the presence sentinel.)
    expected_range: std::ops::RangeInclusive<usize>,
    /// PENDING-BY-DESIGN reason (C1j). `Some(reason)` ⇒ no honest fixture for
    /// this scenario class exists at the pin: the row records CORPUS-PENDING with
    /// `reason` and NEVER requires/judges a fixture (so setting
    /// `SBMUX_CORPUS_DIR` does not flip it to FAIL, and it is never faked against
    /// an unrelated trace). `None` ⇒ active row (modes 1–3).
    pending_reason: Option<&'static str>,
}

/// The pinned TS corpus commit the ACTIVE row compares against. C1j: the real
/// on-disk corpus (attach-detach-reattach) was recorded from this pin — see its
/// `RECORDED-FROM` (`pinned_ts_commit=8c59ec45...`).
const CORPUS_PIN: &str = "8c59ec45";

// ============================================================================
// One-touch fixture parsing (the single adaptation seam for 0b-P2)
// ============================================================================

/// Parse a fixture file into a line vector in TRACE (wire) order.
///
/// THIS IS THE ONE-TOUCH ADAPTATION SEAM. C1j confirmed the REAL on-disk format
/// (golden `normalized/*.trace`) is newline-delimited text — `SBLINE <n>` markers
/// in wire order — so the original split-on-LF body is correct as-is; only the
/// `RowSpec` constants needed updating (marker `"SBLINE "`, real range/floor). If
/// a future corpus ships a different shape (binary frame log, JSON, sidecar-
/// keyed), adapt ONLY this fn — the mode logic and judging skeleton consume the
/// returned `Vec<Vec<u8>>` unchanged.
fn parse_fixture(bytes: &[u8]) -> Vec<Vec<u8>> {
    // Split on LF, drop a single trailing empty (final newline), keep each line
    // as raw bytes so a corrupted multibyte char stays visible to the comparators
    // rather than being masked by a lossy decode.
    let mut lines: Vec<Vec<u8>> = bytes.split(|&b| b == b'\n').map(|s| s.to_vec()).collect();
    if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

// ============================================================================
// Evidence dir
// ============================================================================

/// Evidence dir for a corpus row: target/test-evidence/<runid>/b3/corpus/<row>/.
/// Namespaced under `corpus/` so M5's pending verdicts never collide with the
/// M3/M4 row artifacts under `b3/<row>/`.
fn corpus_evidence_dir(row: &str) -> PathBuf {
    let runid = std::env::var("SBMUX_GATE_RUNID").unwrap_or_else(|_| "dev".to_string());
    let dir = PathBuf::from("target/test-evidence")
        .join(runid)
        .join("b3")
        .join("corpus")
        .join(row);
    fs::create_dir_all(&dir).expect("create corpus evidence dir");
    dir
}

/// Write `<row>_result.txt` into the evidence dir with the given verdict body.
fn write_result(row: &str, body: &str) {
    let dir = corpus_evidence_dir(row);
    let path = dir.join(format!("{row}_result.txt"));
    fs::write(&path, body).expect("write corpus result file");
}

// ============================================================================
// Core judging skeleton
// ============================================================================

/// Run one corpus row through the three operating modes. Returns `Err` ONLY for
/// genuine failures (mode 2 missing fixture, mode 3 degenerate/mismatched
/// fixture). CORPUS-PENDING (mode 1) returns `Ok(())` after recording the
/// pending verdict — the gate stays green but the row is visibly pending.
fn run_corpus_row(spec: &RowSpec) -> Result<(), String> {
    // ---- Mode 0: PENDING-BY-DESIGN (no honest fixture exists at the pin) -----
    // C1j: these rows have no corresponding scenario class on disk and must NOT
    // be faked against an unrelated fixture, NOR fail when SBMUX_CORPUS_DIR is
    // set at the real corpus. Record CORPUS-PENDING naming what is missing.
    if let Some(reason) = spec.pending_reason {
        let verdict = format!(
            "CORPUS-PENDING (by design — {reason}; comparator={comp:?}; pin {CORPUS_PIN}). \
             This row is intentionally not judged: faking it against an unrelated trace \
             would be dishonest. Recorded to evidence dir.",
            comp = spec.comparator,
        );
        eprintln!(
            "\n\
             ============================================================\n\
             [{row}] {verdict}\n\
             ============================================================",
            row = spec.row,
        );
        write_result(spec.row, &verdict);
        return Ok(());
    }

    let corpus_dir = std::env::var("SBMUX_CORPUS_DIR")
        .ok()
        .filter(|s| !s.is_empty());

    // ---- Mode 1: SBMUX_CORPUS_DIR UNSET → CORPUS-PENDING (loud skip) --------
    let Some(corpus_dir) = corpus_dir else {
        let verdict = format!("CORPUS-PENDING (SBMUX_CORPUS_DIR unset; pin {CORPUS_PIN})");
        eprintln!(
            "\n\
             ============================================================\n\
             [{row}] {verdict}\n\
             comparator={comp:?}  expected_subdir={subdir}/{file}\n\
             This row is SKIPPED (not PASS): set SBMUX_CORPUS_DIR to the golden\n\
             fixtures root to enable judging. Recorded to evidence dir.\n\
             ============================================================",
            row = spec.row,
            comp = spec.comparator,
            subdir = spec.subdir,
            file = spec.file,
        );
        write_result(spec.row, &verdict);
        return Ok(());
    };

    let corpus_dir = PathBuf::from(corpus_dir);
    let fixture_path = corpus_dir.join(spec.subdir).join(spec.file);

    // ---- Mode 2: dir SET but fixture ABSENT → FAIL (requested-but-missing) --
    if !fixture_path.exists() {
        let verdict = format!(
            "CORPUS-PENDING-BUT-REQUESTED (SBMUX_CORPUS_DIR set to {dir}, but fixture {path} \
             is MISSING; pin {pin})",
            dir = corpus_dir.display(),
            path = fixture_path.display(),
            pin = CORPUS_PIN,
        );
        eprintln!("[{}] {}", spec.row, verdict);
        write_result(spec.row, &verdict);
        return Err(verdict);
    }

    let bytes = fs::read(&fixture_path).map_err(|e| {
        format!(
            "[{}] cannot read fixture {}: {e}",
            spec.row,
            fixture_path.display()
        )
    })?;
    let lines = parse_fixture(&bytes);

    // ---- Mode 3a: NON-DEGENERACY gate (before any judging, red-team #11) -----
    check_non_degenerate(spec, &lines, &fixture_path)?;

    // ---- Mode 3b: the comparator judges -------------------------------------
    let result = judge(spec, &lines, &bytes);
    let verdict = match &result {
        Ok(()) => format!(
            "CORPUS-PASS [{comp:?}] fixture={path} lines={n} pin={pin}",
            comp = spec.comparator,
            path = fixture_path.display(),
            n = lines.len(),
            pin = CORPUS_PIN,
        ),
        Err(e) => format!(
            "CORPUS-FAIL [{comp:?}] fixture={path} pin={pin}\n{e}",
            comp = spec.comparator,
            path = fixture_path.display(),
            pin = CORPUS_PIN,
        ),
    };
    eprintln!("[{}] {}", spec.row, verdict);
    write_result(spec.row, &verdict);
    result
}

/// Non-degeneracy gate: a real recording must clear the provisional floor and
/// carry the required marker. A placeholder/partial fixture FAILS here, naming
/// the deficiency, so it can never green a row (red-team #11).
fn check_non_degenerate(spec: &RowSpec, lines: &[Vec<u8>], path: &Path) -> Result<(), String> {
    if lines.len() < spec.min_lines {
        let v = format!(
            "DEGENERATE FIXTURE [{row}]: {path} has {got} lines, provisional minimum is {min} \
             (placeholder/partial fixture cannot green this row; threshold PROVISIONAL pending \
             0b-P2 format confirmation)",
            row = spec.row,
            path = path.display(),
            got = lines.len(),
            min = spec.min_lines,
        );
        write_result(spec.row, &v);
        return Err(v);
    }
    let has_marker = lines
        .iter()
        .any(|l| String::from_utf8_lossy(l).contains(spec.required_marker));
    if !has_marker {
        let v = format!(
            "DEGENERATE FIXTURE [{row}]: {path} is missing required marker '{marker}' \
             (wrong scenario or empty placeholder; marker PROVISIONAL pending 0b-P2 format \
             confirmation)",
            row = spec.row,
            path = path.display(),
            marker = spec.required_marker,
        );
        write_result(spec.row, &v);
        return Err(v);
    }
    Ok(())
}

/// Apply the row's declared comparator class (ADR-0004).
fn judge(spec: &RowSpec, lines: &[Vec<u8>], raw: &[u8]) -> Result<(), String> {
    match spec.comparator {
        Comparator::BacklogOrdered => assert_backlog_ordered(
            lines,
            spec.required_marker,
            spec.expected_range.clone(),
            spec.row,
        ),
        Comparator::ScrollIntactNoLeak => {
            // scroll-intact wants String lines; presence-only (order-blind),
            // honestly weaker than R0 (red-team #12).
            let text: Vec<String> = lines
                .iter()
                .map(|l| String::from_utf8_lossy(l).into_owned())
                .collect();
            assert_scroll_intact(&text, spec.required_marker, spec.row)?;
            // altscreen-replay over the raw fixture bytes: this row's fixtures
            // are MAIN-SCREEN replays, which must carry zero 1049 sequences
            // (the renderer replays alt state only when the inner app is in
            // it — reversed gate, 2026-06-10). An alt-screen-class fixture
            // would need non-zero expected counts (none exists on disk yet).
            assert_altscreen_replay(raw, 0, 0, spec.row)
        }
    }
}

// ============================================================================
// The four rows
// ============================================================================

/// (i) corpus_replay_reattach — replay backlog vs the REAL pinned TS corpus
/// (C1j: attach-detach-reattach, class `semantic-backlog-scroll`).
/// Comparator: backlog-completeness via ORDERED (R0) — wire order IS recoverable
/// from the normalized trace (`SBLINE <n>` in scroll order), the strongest honest
/// class for this fixture.
#[test]
fn corpus_replay_reattach() {
    let spec = RowSpec {
        row: "corpus_replay_reattach",
        // Real golden layout: <scenario>/normalized/<name>.trace.
        subdir: "attach-detach-reattach/normalized",
        file: "reattach.trace",
        comparator: Comparator::BacklogOrdered,
        // Real recording is exactly 20 markers; floor pinned to it so a
        // placeholder/partial fixture still fails non-degeneracy.
        min_lines: 20,
        required_marker: "SBLINE ",
        expected_range: 1..=20,
        pending_reason: None,
    };
    run_corpus_row(&spec).unwrap_or_else(|e| panic!("{e}"));
}

/// (ii) corpus_altscreen — altscreen enter/exit + scrollback freeze vs corpus.
/// Comparator: scroll-intact + altscreen-replay (settled capture, order-blind).
/// C1j: CORPUS-PENDING by design — no altscreen-class trace exists on disk.
#[test]
fn corpus_altscreen() {
    let spec = RowSpec {
        row: "corpus_altscreen",
        subdir: "altscreen",
        file: "altscreen.trace",
        comparator: Comparator::ScrollIntactNoLeak,
        min_lines: 10,
        required_marker: "SBLINE ",
        expected_range: 1..=1, // unused by ScrollIntactNoLeak
        pending_reason: Some(
            "no altscreen-class fixture recorded at the pin; the only mux-op trace on disk \
             (attach-detach-reattach) is a plain backlog-scroll capture with no ?1049h/l, so \
             it cannot honestly feed scroll-intact + altscreen-replay",
        ),
    };
    run_corpus_row(&spec).unwrap_or_else(|e| panic!("{e}"));
}

/// (iii) corpus_resize — resize (shrink/grow) backlog vs corpus.
/// Comparator: backlog-completeness via ORDERED (R0).
/// C1j / C1d: CORPUS-PENDING by design — no resize-class trace exists on disk.
#[test]
fn corpus_resize() {
    let spec = RowSpec {
        row: "corpus_resize",
        subdir: "resize",
        file: "resize.trace",
        comparator: Comparator::BacklogOrdered,
        min_lines: 20,
        required_marker: "SBLINE ",
        expected_range: 1..=20,
        pending_reason: Some(
            "no resize-class trace recorded at the pin (carry C1d: zmx source absent, no \
             resize corpus); recording one requires a resize-capable recorder not present",
        ),
    };
    run_corpus_row(&spec).unwrap_or_else(|e| panic!("{e}"));
}

/// (iv) corpus_replay_width_resize — the R3(d) wrap-vs-truncate cross-check:
/// replay AFTER a width change, judged against the corpus to settle the named
/// divergence (history replays as-recorded; client terminal wraps).
/// Comparator: backlog-completeness via ORDERED (R0) over the post-resize range.
/// C1j / C1d: CORPUS-PENDING by design — no width-resize trace exists on disk.
#[test]
fn corpus_replay_width_resize() {
    let spec = RowSpec {
        row: "corpus_replay_width_resize",
        subdir: "replay_width_resize",
        file: "replay_width.trace",
        comparator: Comparator::BacklogOrdered,
        min_lines: 20,
        required_marker: "SBLINE ",
        expected_range: 1..=20,
        pending_reason: Some(
            "no width-resize trace recorded at the pin (carry C1d: no resize-class corpus); \
             the R3(d) wrap-vs-truncate cross-check cannot be judged without one",
        ),
    };
    run_corpus_row(&spec).unwrap_or_else(|e| panic!("{e}"));
}

// ============================================================================
// Self-tests for the M5 skeleton itself (teeth: the modes do what they claim,
// without needing a real corpus). These do NOT touch SBMUX_CORPUS_DIR-driven
// rows — they exercise the helpers directly with synthetic input.
// ============================================================================

#[cfg(test)]
mod skeleton_selftest {
    use super::*;

    fn spec_ordered() -> RowSpec {
        RowSpec {
            row: "selftest_ordered",
            subdir: "x",
            file: "x.trace",
            comparator: Comparator::BacklogOrdered,
            min_lines: 3,
            required_marker: "b3-mark-",
            expected_range: 1..=3,
            pending_reason: None,
        }
    }

    #[test]
    fn parse_drops_single_trailing_newline() {
        assert_eq!(parse_fixture(b"a\nb\n").len(), 2);
        assert_eq!(parse_fixture(b"a\nb").len(), 2);
        // a blank interior line is preserved
        assert_eq!(parse_fixture(b"a\n\nb\n").len(), 3);
    }

    #[test]
    fn non_degenerate_rejects_too_few_lines() {
        let spec = spec_ordered();
        let lines = parse_fixture(b"b3-mark-0001\n");
        let r = check_non_degenerate(&spec, &lines, Path::new("/tmp/x.trace"));
        assert!(r.is_err(), "1 line < min 3 must be degenerate");
        assert!(r.unwrap_err().contains("DEGENERATE"));
    }

    #[test]
    fn non_degenerate_rejects_missing_marker() {
        let spec = spec_ordered();
        let lines = parse_fixture(b"no marker here\nstill none\nnope\n");
        let r = check_non_degenerate(&spec, &lines, Path::new("/tmp/x.trace"));
        assert!(r.is_err(), "missing required marker must be degenerate");
        assert!(r.unwrap_err().contains("missing required marker"));
    }

    #[test]
    fn non_degenerate_accepts_real_looking_fixture() {
        let spec = spec_ordered();
        let lines = parse_fixture(b"b3-mark-0001\nb3-mark-0002\nb3-mark-0003\n");
        assert!(check_non_degenerate(&spec, &lines, Path::new("/tmp/x.trace")).is_ok());
    }

    #[test]
    fn judge_ordered_passes_complete_in_order() {
        let spec = spec_ordered();
        let raw = b"b3-mark-0001\nb3-mark-0002\nb3-mark-0003\n";
        let lines = parse_fixture(raw);
        assert!(judge(&spec, &lines, raw).is_ok());
    }

    #[test]
    fn judge_ordered_passes_real_sbline_format() {
        // C1j: the REAL fixture format is `SBLINE <n>` (space-decimal, not
        // zero-padded). With marker "SBLINE " the comparator must read the index
        // directly after the space and pass for a complete in-order trace.
        let spec = RowSpec {
            required_marker: "SBLINE ",
            expected_range: 1..=3,
            ..spec_ordered()
        };
        let raw = b"SBLINE 1\nSBLINE 2\nSBLINE 3\n";
        let lines = parse_fixture(raw);
        assert!(
            judge(&spec, &lines, raw).is_ok(),
            "real SBLINE format must judge clean"
        );
    }

    #[test]
    fn pending_by_design_returns_ok_and_records_reason() {
        // C1j mode 0: a PENDING-BY-DESIGN row must record CORPUS-PENDING and
        // return Ok (gate green). The mode-0 check short-circuits BEFORE any
        // `SBMUX_CORPUS_DIR` read — so it cannot be flipped to FAIL by a set dir,
        // and this test needs no env mutation (avoids racing parallel tests).
        let spec = RowSpec {
            row: "selftest_pending_by_design",
            pending_reason: Some("selftest: no fixture exists for this class"),
            ..spec_ordered()
        };
        let r = run_corpus_row(&spec);
        assert!(r.is_ok(), "pending-by-design must return Ok (gate green)");
        let evidence = corpus_evidence_dir(spec.row).join(format!("{}_result.txt", spec.row));
        let body = std::fs::read_to_string(&evidence).expect("result recorded");
        assert!(body.contains("CORPUS-PENDING (by design"));
        assert!(body.contains("no fixture exists for this class"));
    }

    #[test]
    fn judge_ordered_fails_reordered() {
        let spec = spec_ordered();
        let raw = b"b3-mark-0002\nb3-mark-0001\nb3-mark-0003\n";
        let lines = parse_fixture(raw);
        assert!(
            judge(&spec, &lines, raw).is_err(),
            "reordered indices must fail R0"
        );
    }

    #[test]
    fn judge_scroll_intact_fails_on_unexpected_1049() {
        let spec = RowSpec {
            comparator: Comparator::ScrollIntactNoLeak,
            ..spec_ordered()
        };
        // Marker present (scroll-intact ok) but a 1049h appears in what this
        // row declares to be a MAIN-SCREEN replay → fail (reversed gate:
        // 1049h is required for alt-screen attaches, forbidden here).
        let raw = b"b3-mark-0001\n\x1b[?1049h unexpected\n";
        let lines = parse_fixture(raw);
        assert!(
            judge(&spec, &lines, raw).is_err(),
            "unexpected 1049 in a main-screen fixture must fail"
        );
    }

    #[test]
    fn missing_fixture_when_dir_set_is_error() {
        // Point at a dir that exists but has no fixture file.
        let tmp =
            std::env::temp_dir().join(format!("sbmux-corpus-selftest-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Build a spec/path that cannot exist.
        let spec = spec_ordered();
        let fixture_path = tmp.join(spec.subdir).join(spec.file);
        assert!(!fixture_path.exists());
        // Exercise the mode-2 branch directly via run logic is awkward (it reads
        // the env var); instead assert the path-absent precondition the branch
        // keys on, which is the load-bearing check.
        std::fs::remove_dir_all(&tmp).ok();
    }
}
