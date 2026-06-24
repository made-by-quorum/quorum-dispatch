//! sbx 0.1 punch-list B2, item 3 — REPRO + MECHANISM PINS for the
//! `sb send:pty --wait` intermittent EMPTY capture while a reply exists.
//! Spec: ~/work/ws/switchboard/sbx/punch/b2-relay-spec.md.
//!
//! ## The diagnosed mechanism family (phase 1): COMPLETION OUTRUNS THE TRANSCRIPT
//! The --wait loop decides Complete from TWO UNSYNCHRONIZED sources read
//! SEQUENTIALLY each poll: (1) the JSONL transcript slice (`read_lines`,
//! verbs/send.rs), then (2) the pid-status file (`read_status`). The transcript
//! is written by Claude Code; the status file by the registry writer. NOTHING
//! orders "final assistant row flushed" before "status reads idle" — and the
//! pre-fix loop returned the lines snapshot taken BEFORE the status read.
//! Three shapes of the same race:
//!
//!   (a) the reply rows land in the file AFTER read_lines but BEFORE
//!       read_status observes idle → Complete carried a pre-reply snapshot;
//!   (b) the reply row is PARTIALLY flushed at the final read → it parses to
//!       Null (parse_jsonl_slice permissive L8) and extraction skips it;
//!   (c) the reply text rows are present but the end_turn row (the only rows
//!       Default-mode extraction keeps) hasn't landed when status reads idle.
//!
//! In every shape the pre-fix bin printed "(no text response)" — or, in
//! --full mode, the EMPTY STRING — and exited 0: an empty capture while the
//! reply demonstrably existed in the file (the field symptom).
//!
//! ## Phase 2 — the rows now pin the FIX (ratified invariant)
//! (1) POSTDATED SNAPSHOT: on Complete, `run_wait_loop` re-reads the slice
//!     until QUIESCENT (two consecutive reads one settle-delay apart with
//!     identical content; bounded by MAX_SETTLE_READS). The returned snapshot
//!     POSTDATES the completion observation, so shapes (a) and (b, when the
//!     flush finishes within the settle window) are CAPTURED IN FULL.
//! (2) LOUD EMPTY: `capture_or_defect` turns any still-empty extraction into
//!     `Err(observed)`; the bin exits 1 naming the observation — NEVER
//!     empty-as-success (covers shape (c) and anything outlasting the settle
//!     bound). Sanctioned divergence from the TS exit-0 surface.
//!
//! ## Harness
//! `run_wait_loop` is PURE over `WaitDeps` (the production seam). The deps here
//! run over REAL files and replicate `RealWaitDeps` (verbs/send.rs) line-for-
//! line for read_lines/read_status; the external-writer race is staged by
//! performing the writer's appends inside the exact window each shape occupies
//! (between read_lines and read_status, or during a settle sleep).

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;

use dispatch::sendpty::{
    capture_or_defect, extract_response, parse_jsonl_slice, run_wait_loop, ExtractMode, ParsedLine,
    WaitDeps, WaitOutcome,
};

/// JSONL rows (the live Claude Code record shapes).
fn user_row(text: &str) -> String {
    serde_json::json!({"type":"user","message":{"content":text}}).to_string()
}
fn assistant_row(text: &str, stop_reason: Option<&str>) -> String {
    serde_json::json!({
        "type":"assistant",
        "message":{
            "stop_reason": stop_reason,
            "content":[{"type":"text","text":text}]
        }
    })
    .to_string()
}

/// Real-file [`WaitDeps`]: read_lines / read_status replicate
/// `RealWaitDeps` (verbs/send.rs) over a real transcript + status file.
/// `on_status_read` is a staged EXTERNAL WRITER landing at the top of
/// read_status — i.e. inside the window between the loop's two reads.
struct FileDeps<'a> {
    jsonl: PathBuf,
    status_file: PathBuf,
    start_offset: u64,
    polls: Cell<u32>,
    t: Cell<i64>,
    on_status_read: Box<dyn Fn(u32) + 'a>,
}

impl WaitDeps for FileDeps<'_> {
    fn read_lines(&self) -> Result<Vec<ParsedLine>, String> {
        // Line-for-line replica of RealWaitDeps::read_lines (verbs/send.rs):
        // whole-file read, shrink/boundary guards, slice past the pre-send
        // offset, parse once.
        let content = fs::read_to_string(&self.jsonl).unwrap_or_default();
        let off = self.start_offset as usize;
        if content.len() < off {
            return Err(format!(
                "conversation JSONL shrank below the start offset ({} < {off} bytes)",
                content.len()
            ));
        }
        let slice = content
            .get(off..)
            .ok_or_else(|| format!("start offset {off} no longer falls on a char boundary"))?;
        Ok(parse_jsonl_slice(slice))
    }
    fn read_status(&self) -> Option<String> {
        // THE STAGED RACE WINDOW: the external writer (Claude Code + the
        // registry status writer) lands here — after this poll's read_lines,
        // before its read_status.
        (self.on_status_read)(self.polls.get());
        let bytes = fs::read(&self.status_file).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        v.get("status").and_then(|s| s.as_str()).map(str::to_string)
    }
    fn sleep(&self, ms: u64) {
        self.t.set(self.t.get() + ms as i64);
        self.polls.set(self.polls.get() + 1);
    }
    fn now_ms(&self) -> i64 {
        self.t.get()
    }
    fn progress(&self, _glyph: char) {}
}

/// As [`FileDeps`] but with a SLEEP hook: the staged writer can land during a
/// poll sleep or a settle sleep (`on_sleep` receives the sleep ordinal).
struct SleepHookDeps<'a> {
    inner: FileDeps<'a>,
    on_sleep: Box<dyn Fn(u32) + 'a>,
}

impl WaitDeps for SleepHookDeps<'_> {
    fn read_lines(&self) -> Result<Vec<ParsedLine>, String> {
        self.inner.read_lines()
    }
    fn read_status(&self) -> Option<String> {
        self.inner.read_status()
    }
    fn sleep(&self, ms: u64) {
        (self.on_sleep)(self.inner.polls.get());
        self.inner.sleep(ms);
    }
    fn now_ms(&self) -> i64 {
        self.inner.now_ms()
    }
    fn progress(&self, _glyph: char) {}
}

/// Stage a tempdir with a transcript holding exactly the anchor row (our sent
/// message, past offset 0) and a status file. Returns (dir, jsonl, status).
fn stage(message: &str, status: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let jsonl = dir.path().join("conv.jsonl");
    let status_file = dir.path().join("pid.json");
    fs::write(&jsonl, format!("{}\n", user_row(message))).unwrap();
    fs::write(
        &status_file,
        serde_json::json!({ "status": status }).to_string(),
    )
    .unwrap();
    (dir, jsonl, status_file)
}

const MSG: &str = "please summarize the build status";
const ANSWER: &str = "ANSWER-BODY: build green, two warnings";

// ---------------------------------------------------------------------------
// Shape (a), FIXED — the reply lands between the final read_lines and the
// read_status that observes idle. The postdated settle re-read now captures
// it in full (this was the phase-1 defect pin: empty capture, reply in file).
// ---------------------------------------------------------------------------
#[test]
fn shape_a_reply_flushed_in_status_window_is_captured_via_postdated_snapshot() {
    let (_dir, jsonl, status_file) = stage(MSG, "busy");

    let jsonl2 = jsonl.clone();
    let status2 = status_file.clone();
    let deps = FileDeps {
        jsonl: jsonl.clone(),
        status_file: status_file.clone(),
        start_offset: 0,
        polls: Cell::new(0),
        t: Cell::new(0),
        on_status_read: Box::new(move |poll| {
            if poll == 0 {
                // The target session finishes its turn IN THE WINDOW: the
                // assistant rows hit the transcript and the status flips idle
                // — both before this poll's status read, both after its
                // lines read.
                let mut content = fs::read_to_string(&jsonl2).unwrap();
                content.push_str(&format!(
                    "{}\n{}\n",
                    assistant_row("working on it", Some("tool_use")),
                    assistant_row(ANSWER, Some("end_turn")),
                ));
                fs::write(&jsonl2, content).unwrap();
                fs::write(&status2, r#"{"status":"idle"}"#).unwrap();
            }
        }),
    };

    let outcome = run_wait_loop(&deps, MSG, 60_000, 500);
    let WaitOutcome::Complete { lines, anchor } = outcome else {
        panic!("expected Complete, got {outcome:?}");
    };
    assert_eq!(anchor, Some(0), "anchored on our user record");

    // ---- FIXED SHAPE (was the phase-1 defect pin) ----
    // The snapshot postdates the idle observation: the reply is IN it, and
    // the capture is the full text — not "(no text response)".
    assert_eq!(
        capture_or_defect(&lines, anchor, ExtractMode::Default),
        Ok(ANSWER.to_string()),
        "the postdated settle snapshot captures the reply in full"
    );
}

// ---------------------------------------------------------------------------
// Shape (b), FIXED — the reply row is PARTIALLY FLUSHED when status reads
// idle, and finishes during the settle window. The quiescence re-read (two
// identical reads one settle apart) picks up the completed row (this was the
// phase-1 defect pin: torn row silently skipped).
// ---------------------------------------------------------------------------
#[test]
fn shape_b_partial_row_completing_during_settle_is_captured() {
    let (_dir, jsonl, status_file) = stage(MSG, "idle");

    // The reply row is mid-flush: only its first 30 bytes are on disk.
    let full_row = assistant_row(ANSWER, Some("end_turn"));
    let base = fs::read_to_string(&jsonl).unwrap();
    fs::write(&jsonl, format!("{base}{}", &full_row[..30])).unwrap();

    let jsonl2 = jsonl.clone();
    let completed = format!("{base}{full_row}\n");
    let deps = SleepHookDeps {
        inner: FileDeps {
            jsonl: jsonl.clone(),
            status_file,
            start_offset: 0,
            polls: Cell::new(0),
            t: Cell::new(0),
            on_status_read: Box::new(|_| {}),
        },
        on_sleep: Box::new(move |sleep_ordinal| {
            // The flush finishes during the FIRST settle sleep (the loop
            // completed on poll 0, so sleep ordinal 0 IS a settle sleep).
            if sleep_ordinal == 0 {
                fs::write(&jsonl2, &completed).unwrap();
            }
        }),
    };

    let outcome = run_wait_loop(&deps, MSG, 60_000, 500);
    let WaitOutcome::Complete { lines, anchor } = outcome else {
        panic!("expected Complete, got {outcome:?}");
    };

    // ---- FIXED SHAPE (was the phase-1 defect pin) ----
    assert_eq!(
        capture_or_defect(&lines, anchor, ExtractMode::Default),
        Ok(ANSWER.to_string()),
        "a row that finishes flushing within the settle window is captured"
    );
}

// ---------------------------------------------------------------------------
// Shape (b) outlasting the settle bound — the torn row NEVER completes. The
// snapshot stays empty-of-content; the binding invariant then demands a LOUD
// defect, never empty-as-success: capture_or_defect → Err (the bin exits 1).
// ---------------------------------------------------------------------------
#[test]
fn shape_b_partial_row_never_completing_is_a_loud_capture_defect() {
    let (_dir, jsonl, status_file) = stage(MSG, "idle");
    let full_row = assistant_row(ANSWER, Some("end_turn"));
    let base = fs::read_to_string(&jsonl).unwrap();
    fs::write(&jsonl, format!("{base}{}", &full_row[..30])).unwrap();

    let deps = FileDeps {
        jsonl,
        status_file,
        start_offset: 0,
        polls: Cell::new(0),
        t: Cell::new(0),
        on_status_read: Box::new(|_| {}),
    };

    let outcome = run_wait_loop(&deps, MSG, 60_000, 500);
    let WaitOutcome::Complete { lines, anchor } = outcome else {
        panic!("expected Complete, got {outcome:?}");
    };

    let err = capture_or_defect(&lines, anchor, ExtractMode::Default)
        .expect_err("an empty extraction must be a loud defect, never success");
    assert!(
        err.contains("no response content"),
        "the defect names the observation, got: {err}"
    );
    assert!(
        err.contains("after the anchor"),
        "the defect names where it looked, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Shape (c), FIXED-LOUD — the answer text is in the slice but no end_turn row
// ever lands (status writer ran ahead of the transcript's final row). Default-
// mode extraction stays empty; the invariant demands the LOUD path.
// ---------------------------------------------------------------------------
#[test]
fn shape_c_idle_before_end_turn_is_a_loud_capture_defect() {
    let (_dir, jsonl, status_file) = stage(MSG, "idle");
    let mut content = fs::read_to_string(&jsonl).unwrap();
    // The streamed assistant text row is flushed with stop_reason null; the
    // final end_turn row never lands — but the status file already says idle
    // (independent writers).
    content.push_str(&format!("{}\n", assistant_row(ANSWER, None)));
    fs::write(&jsonl, &content).unwrap();

    let deps = FileDeps {
        jsonl,
        status_file,
        start_offset: 0,
        polls: Cell::new(0),
        t: Cell::new(0),
        on_status_read: Box::new(|_| {}),
    };

    let outcome = run_wait_loop(&deps, MSG, 60_000, 500);
    let WaitOutcome::Complete { lines, anchor } = outcome else {
        panic!("expected Complete, got {outcome:?}");
    };

    // The text IS in the snapshot (this is shape c, not a)...
    assert!(
        lines.iter().any(|l| l.raw.contains(ANSWER)),
        "the answer text is in the completed snapshot itself"
    );
    // ...and the capture is a LOUD defect naming the missing end_turn —
    // never "(no text response)" with exit 0.
    let err = capture_or_defect(&lines, anchor, ExtractMode::Default)
        .expect_err("no end_turn text = loud defect, never empty-as-success");
    assert!(
        err.contains("assistant end_turn text"),
        "the defect names what was missing, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// FIXED-LOUD — `--full` mode's literal empty-string capture (the bin used to
// print it and exit 0) is now a loud defect through the same helper.
// ---------------------------------------------------------------------------
#[test]
fn full_mode_empty_extraction_is_a_loud_capture_defect() {
    let lines = parse_jsonl_slice(&format!("{}\n", user_row(MSG)));
    // The raw extractor still yields "" (its contract is unchanged)...
    assert_eq!(extract_response(&lines, Some(0), ExtractMode::Full), "");
    // ...but the capture layer refuses to hand it out as success.
    let err = capture_or_defect(&lines, Some(0), ExtractMode::Full)
        .expect_err("--full empty extraction must be a loud defect");
    assert!(
        err.contains("0 transcript line(s) after the anchor"),
        "{err}"
    );

    // Raw mode likewise (the old silent-exit-0 special case is dead).
    capture_or_defect(&lines, Some(0), ExtractMode::Raw)
        .expect_err("--raw empty extraction must be a loud defect");
}

// ---------------------------------------------------------------------------
// Control (differential): when the writer lands BETWEEN polls — the benign
// timing — the same loop + capture return the full text. Identical machinery
// to the shape rows; only the landing window differs.
// ---------------------------------------------------------------------------
#[test]
fn control_reply_landing_between_polls_extracts_fully() {
    let (_dir, jsonl, status_file) = stage(MSG, "busy");

    let jsonl2 = jsonl.clone();
    let status2 = status_file.clone();
    let deps = SleepHookDeps {
        inner: FileDeps {
            jsonl,
            status_file,
            start_offset: 0,
            polls: Cell::new(0),
            t: Cell::new(0),
            on_status_read: Box::new(|_| {}),
        },
        on_sleep: Box::new(move |sleep_ordinal| {
            // The writer lands during poll 0's (busy) sleep: poll 1 then reads
            // the complete reply AND observes idle.
            if sleep_ordinal == 0 {
                let mut content = fs::read_to_string(&jsonl2).unwrap();
                content.push_str(&format!("{}\n", assistant_row(ANSWER, Some("end_turn"))));
                fs::write(&jsonl2, content).unwrap();
                fs::write(&status2, r#"{"status":"idle"}"#).unwrap();
            }
        }),
    };

    let outcome = run_wait_loop(&deps, MSG, 60_000, 500);
    let WaitOutcome::Complete { lines, anchor } = outcome else {
        panic!("expected Complete, got {outcome:?}");
    };
    assert_eq!(
        capture_or_defect(&lines, anchor, ExtractMode::Default),
        Ok(ANSWER.to_string()),
        "a reply landing between polls is captured in full"
    );
}
