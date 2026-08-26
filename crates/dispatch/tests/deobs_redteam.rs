//! DE-observed idempotency RED-TEAM battery (loop component 2, adversarial seat).
//!
//! Mandate: try HARD to break write-time idempotency and the claim+append
//! atomic-or-recoverable property of `dispatch::telemetry::record_observed_in`.
//! Look SPECIFICALLY for reader-side dedup masquerading as write-time idempotency.
//!
//! Every probe is hermetic (per-test `tempfile` dir, injected `FixedClock`, no
//! ambient `QD_*`) and prints its RAW resulting stream + marker state so a
//! `--nocapture` run yields the primary-source transcript the oracle re-runs.
//!
//! In-process probes are ALWAYS-ON. The real-process death/recovery probes and
//! the ~10s lock-timeout probe are gated behind the `deobs` feature (they spawn
//! the `deobs_observe_target` victim bin, gated the same way). Run the full
//! battery with:
//!   cargo test -p quorum-dispatch --features deobs --test deobs_redteam \
//!     -- --nocapture --test-threads=1

use std::path::Path;

use dispatch::effects::FixedClock;
use dispatch::telemetry::{build_observed_line, record_observed_in, RecordHooks};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Shared RAW-evidence helpers
// ---------------------------------------------------------------------------

/// Count PHYSICAL `observed` lines matching the key exactly (host+harness+sid) —
/// the write-time duplicate detector clause (1) binds. Torn-trailing tolerant
/// (same rule the reader uses): an unterminated trailing partial contributes
/// nothing, so a corrupted append shows up as a DROP in this count.
fn count_observed_exact(marks: &Path, host: &str, harness: &str, sid: &str) -> usize {
    let Ok(text) = std::fs::read_to_string(marks) else {
        return 0;
    };
    let mut lines: Vec<&str> = text.split('\n').collect();
    if !text.ends_with('\n') {
        lines.pop();
    }
    lines
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(|v| {
            v.get("event").and_then(Value::as_str) == Some("observed")
                && v.get("host").and_then(Value::as_str) == Some(host)
                && v.get("harness").and_then(Value::as_str) == Some(harness)
                && v.get("sessionId").and_then(Value::as_str) == Some(sid)
        })
        .count()
}

fn marker_path(state_dir: &Path, host: &str, harness: &str, sid: &str) -> std::path::PathBuf {
    // Mirrors telemetry::encode_observed_key for the spec-fixed simple triple
    // (all chars in [A-Za-z0-9._-], so no percent-escaping): `host~harness~sid`.
    state_dir
        .join("observed-claims")
        .join(format!("{host}~{harness}~{sid}"))
}

/// Dump the RAW physical stream + marker dir to stdout (the oracle's transcript).
fn dump_state(tag: &str, state_dir: &Path) {
    let marks = state_dir.join("marks.jsonl");
    println!("---- RAW[{tag}] marks.jsonl @ {} ----", marks.display());
    match std::fs::read(&marks) {
        Ok(bytes) => {
            println!("bytes={} ends_with_newline={}", bytes.len(), bytes.last() == Some(&b'\n'));
            println!("{}", String::from_utf8_lossy(&bytes));
        }
        Err(e) => println!("(no marks.jsonl: {e})"),
    }
    let claims = state_dir.join("observed-claims");
    print!("---- RAW[{tag}] observed-claims/: ");
    match std::fs::read_dir(&claims) {
        Ok(rd) => {
            let names: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            println!("{names:?}");
        }
        Err(_) => println!("(none)"),
    }
}

fn clock() -> FixedClock {
    FixedClock(1_752_573_600_000)
}

// ===========================================================================
// PROBE 1 — many REAL threads, same key: exactly one physical line.
// (Strengthens the 2-racer conformance test to heavy contention.)
// ===========================================================================
#[test]
fn probe1_many_threads_same_key_exactly_one_line() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");

    const N: usize = 16;
    let barrier = Arc::new(Barrier::new(N));
    let mut handles = Vec::new();
    for _ in 0..N {
        let state = state.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait(); // release all racers at once — maximize contention
            let c = clock();
            record_observed_in(&state, &c, "obsbox", "claude", "sid-thr", None, &RecordHooks::default())
        }));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    dump_state("probe1", &state);

    let wins = results.iter().filter(|r| matches!(r, Ok(true))).count();
    let noops = results.iter().filter(|r| matches!(r, Ok(false))).count();
    let errs = results.iter().filter(|r| r.is_err()).count();
    println!("probe1: wins={wins} noops={noops} errs={errs} results={results:?}");

    assert_eq!(errs, 0, "no racer should error under normal contention");
    assert_eq!(wins, 1, "exactly one racer wins the first sighting");
    assert_eq!(noops, N - 1, "every other racer no-ops");
    assert_eq!(
        count_observed_exact(&marks, "obsbox", "claude", "sid-thr"),
        1,
        "PHYSICAL stream holds exactly one line (write-time, not reader dedup)"
    );
}

// ===========================================================================
// PROBE 3 — torn trailing line, SAME key: no false 'already recorded', no dup,
// AND the marker⟹readable-line invariant must survive the append.
//
// A torn (unterminated) partial `observed` line for K sits at the tail (a state
// the readers explicitly tolerate and the module admits can arise from >PIPE_BUF
// mark interleaving). We then record K normally. The mechanism claims: the torn
// winner is NOT mistaken for a committed line (so we append), no duplicate, and
// (load-bearing) `marker present ⟹ a readable line for K exists`.
// ===========================================================================
#[test]
fn probe3_torn_trailing_line_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    std::fs::create_dir_all(&state).unwrap();

    // Inject a torn PARTIAL observed line for K with NO trailing newline.
    let full = build_observed_line("2026-07-15T00:00:00.000Z", "obsbox", "claude", "sid-torn", None);
    let torn = &full[..full.len() - 10]; // chop the tail → unterminated partial
    std::fs::write(&marks, torn.as_bytes()).unwrap();
    println!("probe3: injected torn tail (no newline): {torn:?}");

    let c = clock();
    let r = record_observed_in(&state, &c, "obsbox", "claude", "sid-torn", None, &RecordHooks::default());
    dump_state("probe3", &state);
    println!("probe3: record result = {r:?}");

    let readable = count_observed_exact(&marks, "obsbox", "claude", "sid-torn");
    let marker = marker_path(&state, "obsbox", "claude", "sid-torn");
    println!("probe3: readable_lines={readable} marker_exists={}", marker.exists());

    // (a) No false 'already recorded' from the torn winner: it must have appended.
    assert_eq!(r, Ok(true), "torn partial must NOT be read as a committed line");
    // (b) No duplicate: never two readable lines for the key.
    assert!(readable <= 1, "never a duplicate readable line (got {readable})");
    // (c) LOAD-BEARING invariant: if the marker was committed, a readable line
    //     for K MUST exist (marker present ⟹ line committed AND readable).
    if marker.exists() {
        assert_eq!(
            readable, 1,
            "marker present but NO readable observed line for K — \
             marker⟹line invariant BROKEN (torn-tail concatenation corrupted the append)"
        );
    }
}

// ===========================================================================
// PROBE 3b — torn trailing line for a DIFFERENT key, then a legitimate FIRST
// sighting of K2. The K2 append must not be silently corrupted (lost) by an
// unrelated torn tail left by another writer.
// ===========================================================================
#[test]
fn probe3b_torn_trailing_line_corrupts_unrelated_append() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    std::fs::create_dir_all(&state).unwrap();

    // A torn partial for K1 (some other writer's crash), no trailing newline.
    let full_k1 = build_observed_line("2026-07-15T00:00:00.000Z", "obsbox", "claude", "sid-K1", None);
    let torn_k1 = &full_k1[..full_k1.len() - 8];
    std::fs::write(&marks, torn_k1.as_bytes()).unwrap();

    // A legitimate, brand-new first sighting of K2.
    let c = clock();
    let r = record_observed_in(&state, &c, "obsbox", "claude", "sid-K2", None, &RecordHooks::default());
    dump_state("probe3b", &state);
    println!("probe3b: record K2 result = {r:?}");

    let readable_k2 = count_observed_exact(&marks, "obsbox", "claude", "sid-K2");
    let marker_k2 = marker_path(&state, "obsbox", "claude", "sid-K2");
    println!(
        "probe3b: K2 readable_lines={readable_k2} marker_exists={}",
        marker_k2.exists()
    );

    // A legitimate first sighting reported success…
    assert_eq!(r, Ok(true), "K2 is a fresh first sighting → Ok(true)");
    // …so its line MUST be readable (not silently glued onto K1's torn tail).
    assert_eq!(
        readable_k2, 1,
        "K2's legitimate append was silently corrupted by an unrelated torn tail \
         (append_line has no leading-newline guard) — first sighting LOST"
    );
}

// ===========================================================================
// PROBE 3c — CLASS-CLOSURE fuzz for F-DEOBS-1. A DETERMINISTIC sweep over many
// dirty/torn trailing-tail shapes (no RNG — fully reproducible for the oracle).
// For each shape: seed marks.jsonl with that tail, then do a legitimate FIRST
// sighting of a DISTINCT key and assert the invariant survives — count_readable
// (freshkey) == 1 AND (marker committed ⇒ a readable line exists). Any shape that
// corrupts the fresh append is a class member. Proves F-DEOBS-1 is a CLASS, not
// two instances, so the fix must close the CLASS (this probe is its accept gate).
// ===========================================================================
#[test]
fn probe3c_class_closure_dirty_tail_fuzz() {
    // A representative complete `observed` line we chop to make torn tails.
    let obs = build_observed_line("2026-07-15T00:00:00.000Z", "obsbox", "claude", "sid-K1", Some("/x"));
    let create = r#"{"ts":"2026-07-15T00:00:00.000Z","event":"create","name":"n","backend":"b"}"#;
    let mark = r#"{"ts":"2026-07-15T00:00:00.000Z","sessionId":"s","payload":{"k":"v"}}"#;

    // Build the deterministic shape matrix (each is the RAW tail bytes to seed).
    let mut shapes: Vec<(String, String)> = Vec::new();
    // (1) the observed line chopped at every 5th byte, UNTERMINATED (torn).
    let mut n = 5;
    while n < obs.len() {
        shapes.push((format!("obs_chop@{n}"), obs[..n].to_string()));
        n += 5;
    }
    // (2) a COMPLETE observed line but with NO trailing newline (unterminated).
    shapes.push(("obs_complete_no_nl".into(), obs.clone()));
    // (3) torn create / torn mark tails (kind-independence of the defect).
    shapes.push(("create_chop".into(), create[..create.len() / 2].to_string()));
    shapes.push(("mark_chop".into(), mark[..mark.len() / 2].to_string()));
    // (4) a valid terminated line FOLLOWED by a torn tail (mid-stream realism).
    shapes.push(("valid_then_torn".into(), format!("{create}\n{}", &obs[..obs.len() / 2])));
    // (5) non-JSON junk tail without newline.
    shapes.push(("junk_no_nl".into(), "not-json-garbage-tail".into()));

    let mut corrupted: Vec<String> = Vec::new();
    for (label, tail) in &shapes {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let marks = state.join("marks.jsonl");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(&marks, tail.as_bytes()).unwrap();

        // Legitimate first sighting of a DISTINCT key over the dirty tail.
        let sid = format!("fresh-{label}");
        let sid = sid.replace(['@', '/'], "_"); // keep it a clean single field
        let c = clock();
        let r = record_observed_in(&state, &c, "obsbox", "claude", &sid, None, &RecordHooks::default());
        let readable = count_observed_exact(&marks, "obsbox", "claude", &sid);
        let marker = marker_path(&state, "obsbox", "claude", &sid);
        let marker_exists = marker.exists();

        // A first sighting that reports Ok(true) (or commits a marker) but yields
        // no readable line is a class member (silent loss + marker⟹line break).
        let claimed = matches!(r, Ok(true)) || marker_exists;
        if claimed && readable != 1 {
            corrupted.push(format!(
                "{label}: r={r:?} readable={readable} marker={marker_exists} tail_ends_nl={}",
                tail.ends_with('\n')
            ));
        }
    }

    println!(
        "probe3c: dirty-tail shapes tested={} corrupted={}",
        shapes.len(),
        corrupted.len()
    );
    for c in &corrupted {
        println!("probe3c: CORRUPTED {c}");
    }
    assert!(
        corrupted.is_empty(),
        "F-DEOBS-1 CLASS: {}/{} dirty-tail shapes corrupted a legitimate fresh first \
         sighting (marker⟹line broken / silent loss). append_line needs a record-\
         boundary guard. Members: {:?}",
        corrupted.len(),
        shapes.len(),
        corrupted
    );
}

// ===========================================================================
// PROBE 4a — F-fwd-1 (oracle-elevated MUST-PROBE): stream loss/rotation that
// LEAVES a surviving observed-claims/ marker. The fast path short-circuits on the
// marker BEFORE the authoritative locked scan, so re-observation is refused while
// the line is gone (QS-8 hazard). Capture the outcome VERBATIM.
// ===========================================================================
#[test]
fn probe4a_rotation_surviving_marker_blocks_reobservation() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    let c = clock();

    // First sighting commits a line + marker.
    assert_eq!(
        record_observed_in(&state, &c, "obsbox", "claude", "sid-rot", None, &RecordHooks::default()),
        Ok(true)
    );
    let marker = marker_path(&state, "obsbox", "claude", "sid-rot");
    assert!(marker.exists());
    assert_eq!(count_observed_exact(&marks, "obsbox", "claude", "sid-rot"), 1);

    // ROTATION that clears the stream but NOT the marker (the exact QS-8 mis-op).
    std::fs::remove_file(&marks).unwrap();
    println!("probe4a: removed marks.jsonl; marker survives = {}", marker.exists());

    // Re-observe the SAME identity after stream loss.
    let r = record_observed_in(&state, &c, "obsbox", "claude", "sid-rot", None, &RecordHooks::default());
    dump_state("probe4a", &state);
    println!("probe4a: re-observe after stream-loss-with-surviving-marker = {r:?}");
    let readable = count_observed_exact(&marks, "obsbox", "claude", "sid-rot");
    println!("probe4a: readable_lines_after_reobserve = {readable}");

    // DOCUMENTED QS-8 hazard captured: the surviving marker fast-paths to a NO-OP
    // and re-observation SILENTLY does not happen. This is the exact masquerade at
    // the stream-lifetime boundary — BUBBLED to the de-observed owner for the acceptance
    // oracle to scope against QS-8's "rotation must clear markers" documented
    // coupling. (Assertion pins the observed behavior as raw evidence, NOT approval.)
    assert_eq!(r, Ok(false), "QS-8 HAZARD: surviving marker refuses re-observation");
    assert_eq!(readable, 0, "QS-8 HAZARD: no line re-appended — sighting silently lost");
}

// ===========================================================================
// PROBE 4b — the LEGITIMATE rotation: clearing BOTH the stream and the markers
// (the documented coupling honored) → re-observation works.
// ===========================================================================
#[test]
fn probe4b_rotation_clearing_markers_reobserves() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    let c = clock();

    assert_eq!(
        record_observed_in(&state, &c, "obsbox", "claude", "sid-rot2", None, &RecordHooks::default()),
        Ok(true)
    );
    // Proper rotation: remove BOTH the stream and the claims dir.
    std::fs::remove_file(&marks).unwrap();
    std::fs::remove_dir_all(state.join("observed-claims")).unwrap();

    let r = record_observed_in(&state, &c, "obsbox", "claude", "sid-rot2", None, &RecordHooks::default());
    dump_state("probe4b", &state);
    println!("probe4b: re-observe after PROPER rotation = {r:?}");
    assert_eq!(r, Ok(true), "proper rotation (both cleared) re-observes legitimately");
    assert_eq!(count_observed_exact(&marks, "obsbox", "claude", "sid-rot2"), 1);
}

// ===========================================================================
// PROBE 5 — QS-6 composition seam: a stream that PHYSICALLY holds two same-key
// observed lines (post-merge/post-loss history). Readers must treat them as ONE
// fact (first-wins) and record_observed_in must NOT add a third; and this must not
// weaken the write-time clause (writers in normal op never emit two).
// ===========================================================================
#[test]
fn probe5_composition_seam_duplicate_lines_first_wins() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    std::fs::create_dir_all(&state).unwrap();

    // Two physical lines for the SAME key (simulating a merged/concatenated history).
    let l1 = build_observed_line("2026-07-15T00:00:00.000Z", "obsbox", "claude", "sid-comp", Some("/a"));
    let l2 = build_observed_line("2026-07-15T00:00:01.000Z", "obsbox", "claude", "sid-comp", Some("/b"));
    std::fs::write(&marks, format!("{l1}\n{l2}\n")).unwrap();
    assert_eq!(count_observed_exact(&marks, "obsbox", "claude", "sid-comp"), 2, "seeded two lines");

    let c = clock();
    let r = record_observed_in(&state, &c, "obsbox", "claude", "sid-comp", None, &RecordHooks::default());
    dump_state("probe5", &state);
    println!("probe5: record over a 2-line history = {r:?}");

    // Reader-side handling of pre-existing duplicates: treated as present (one
    // fact), NO third line written.
    assert_eq!(r, Ok(false), "pre-existing duplicate key is seen as already-present (first-wins)");
    assert_eq!(
        count_observed_exact(&marks, "obsbox", "claude", "sid-comp"),
        2,
        "no third line added — writer did not amplify the pre-existing duplicate"
    );
    // NOTE (scope boundary, bubbled): the ONLY reader of `observed` lines in this
    // module is the presence-check inside record_observed_in (first-match wins);
    // `fold_marks` skips `observed` entirely. Whether the DOWNSTREAM consumer (DP's
    // cross-fog scan) also dedups duplicate keys is OUT OF THIS FILE'S SCOPE — I
    // cannot see it from telemetry.rs and flag that explicitly to the oracle.
}

// ===========================================================================
// PROBE 7 — marker malice. A planted/stale marker with NO line can only ever
// cause an early NO-OP (never an early APPEND that manufactures a duplicate).
// ALSO capture: because the fast path trusts the marker WITHOUT a stream cross-
// check, a marker-without-line is a PERMANENT refusal (the QS-8 root, generalized).
// ===========================================================================
#[test]
fn probe7_planted_marker_early_noop_never_early_append() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");

    // Plant a marker for K with NO stream line at all.
    let marker = marker_path(&state, "obsbox", "claude", "sid-plant");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, b"").unwrap();
    println!("probe7: planted marker with NO line: {}", marker.display());

    let c = clock();
    let r = record_observed_in(&state, &c, "obsbox", "claude", "sid-plant", None, &RecordHooks::default());
    dump_state("probe7", &state);
    println!("probe7: record with planted marker = {r:?}");
    let readable = count_observed_exact(&marks, "obsbox", "claude", "sid-plant");
    println!("probe7: readable_lines = {readable}");

    // The SAFE property that MUST hold: a marker can only cause an early NO-OP,
    // NEVER an early APPEND. So the result is Ok(false) and NO line is manufactured.
    assert_eq!(r, Ok(false), "planted marker → early no-op (never an early append)");
    assert_eq!(readable, 0, "no duplicate manufactured from a planted marker");
    // CAPTURED HAZARD (bubbled, not asserted-as-approved): this same fast-path
    // trust means a marker-without-line (from rotation/corruption/planting) is a
    // PERMANENT silent refusal to record — it does NOT self-heal via the locked
    // scan, because the scan runs only when the marker is ABSENT. Bounded in normal
    // single-host op by the marker⟹line construction invariant; the oracle scopes
    // whether that boundary is sufficient (same class as PROBE 4a / F-fwd-1).
}

// ===========================================================================
// F-fwd-2a — lock-acquisition TIMEOUT path (in-process, ~10s; gated `deobs`
// so the default test run stays fast). A caller that times out acquiring
// observed.lock must leave the key RECORDABLE: non-fatal Err only, NO orphaned
// marker, NO partial line.
// ===========================================================================
#[cfg(feature = "deobs")]
#[test]
fn ffwd2a_lock_timeout_leaves_key_recordable() {
    use std::os::unix::io::AsRawFd;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    std::fs::create_dir_all(&state).unwrap();
    let lock_path = state.join("observed.lock");

    // Hold observed.lock via a SEPARATE open description (conflicts even in-proc).
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    let rc = unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(rc, 0, "test harness must hold observed.lock");
    println!("ffwd2a: holding observed.lock; a racer will now contend to timeout (~10s)");

    // A racer that will block on acquire and time out at the bounded deadline.
    let (tx, rx) = mpsc::channel();
    let state_r = state.clone();
    let racer = thread::spawn(move || {
        let c = clock();
        let r = record_observed_in(&state_r, &c, "obsbox", "claude", "sid-to", None, &RecordHooks::default());
        let _ = tx.send(r);
    });

    // Bound the wait generously above the 10s internal deadline; bubble on hang.
    let r = rx
        .recv_timeout(Duration::from_secs(25))
        .expect("BUBBLE: record_observed_in never returned — lock-timeout path may spin (hang)");
    racer.join().unwrap();
    println!("ffwd2a: timed-out racer result = {r:?}");

    assert!(r.is_err(), "a caller that can't acquire the lock returns a non-fatal Err");
    let marker = marker_path(&state, "obsbox", "claude", "sid-to");
    assert!(!marker.exists(), "timed-out caller left NO orphaned marker");
    assert_eq!(
        count_observed_exact(&marks, "obsbox", "claude", "sid-to"),
        0,
        "timed-out caller wrote NO partial line"
    );

    // Release the lock; the key must still be recordable.
    let rc = unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_UN) };
    assert_eq!(rc, 0);
    drop(holder);
    let c = clock();
    let r2 = record_observed_in(&state, &c, "obsbox", "claude", "sid-to", None, &RecordHooks::default());
    dump_state("ffwd2a", &state);
    println!("ffwd2a: record after lock released = {r2:?}");
    assert_eq!(r2, Ok(true), "key stays recordable after a lock timeout (no poison)");
    assert_eq!(count_observed_exact(&marks, "obsbox", "claude", "sid-to"), 1);
}

// ===========================================================================
// PROBE 2 + F-fwd-2b — REAL crash/SIGKILL of a holder pinned IN the critical
// section (post-check, pre-append, lock held). The kernel must release the flock
// on process death and the key must stay recordable (the crash-between-claim-and-
// append case AND the wedged-holder SIGKILL case). Subprocess-driven; gated.
// ===========================================================================
#[cfg(feature = "deobs")]
#[test]
fn probe2_ffwd2b_sigkill_in_section_holder_recovers() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    std::fs::create_dir_all(&state).unwrap();
    let sentinel = state.join("in-section.sentinel");

    let bin = env!("CARGO_BIN_EXE_deobs_observe_target");
    let mut child = Command::new(bin)
        .args([
            "--hold-in-section",
            state.to_str().unwrap(),
            "obsbox",
            "claude",
            "sid-kill",
            sentinel.to_str().unwrap(),
        ])
        .spawn()
        .expect("spawn deobs_observe_target");

    // Wait (bounded) until the child is provably IN-SECTION (holding the lock,
    // post-check, pre-append) — the sentinel is created inside before_append.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sentinel.exists() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("BUBBLE: holder never reached in-section within 10s (hang, not spin)");
        }
        // Also detect an early child death (would defeat the probe).
        if let Ok(Some(status)) = child.try_wait() {
            panic!("BUBBLE: holder exited early before in-section: {status:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("probe2/ffwd2b: holder is IN-SECTION (sentinel present); no line appended yet");
    assert_eq!(
        count_observed_exact(&marks, "obsbox", "claude", "sid-kill"),
        0,
        "holder is pre-append while pinned in-section"
    );

    // CRASH it uncatchably while it holds the lock.
    child.kill().expect("SIGKILL the in-section holder");
    let status = child.wait().expect("reap the killed holder");
    println!("probe2/ffwd2b: holder killed, status = {status:?}");

    // A fresh caller for the SAME key must acquire the (OS-released) lock and record.
    let c = clock();
    let r = record_observed_in(&state, &c, "obsbox", "claude", "sid-kill", None, &RecordHooks::default());
    dump_state("probe2/ffwd2b", &state);
    println!("probe2/ffwd2b: recovery record after holder death = {r:?}");
    assert_eq!(
        r,
        Ok(true),
        "flock released on death + no durable poison ⇒ next caller records (Ok(true))"
    );
    assert_eq!(
        count_observed_exact(&marks, "obsbox", "claude", "sid-kill"),
        1,
        "exactly one line after crash-recovery (no duplicate, no permanent wedge)"
    );
}

// ===========================================================================
// PROBE 6 — reader-side-dedup masquerade DISCRIMINATOR (AUTOMATIC-FINDING hunt).
// N REAL processes race the SAME key with NO injected seam. If write-time
// idempotency is genuine, the PHYSICAL stream holds exactly ONE line (nothing for
// a reader to dedup). If the "one fact" only emerged at read time, the physical
// stream would hold >1 line → automatic finding. Subprocess-driven; gated.
// ===========================================================================
#[cfg(feature = "deobs")]
#[test]
fn probe6_multiprocess_racers_one_physical_line() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    std::fs::create_dir_all(&state).unwrap();

    let bin = env!("CARGO_BIN_EXE_deobs_observe_target");
    const N: usize = 24;

    // Spawn all N first (so they contend), then collect.
    let mut children = Vec::new();
    for _ in 0..N {
        let child = Command::new(bin)
            .args(["--race", state.to_str().unwrap(), "obsbox", "claude", "sid-mp"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn racer");
        children.push(child);
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut outputs = Vec::new();
    for mut child in children {
        // Bounded wait per child; kill+bubble on hang.
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("BUBBLE: a racer process hung (no exit within 30s)");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("BUBBLE: try_wait failed: {e}"),
            }
        }
        let out = child.wait_with_output().expect("collect racer output");
        outputs.push(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    dump_state("probe6", &state);
    println!("probe6: {N} racer outputs = {outputs:?}");

    let wins = outputs.iter().filter(|o| o.contains("ok=true")).count();
    let noops = outputs.iter().filter(|o| o.contains("ok=false")).count();
    println!("probe6: wins={wins} noops={noops}");

    let physical = count_observed_exact(&marks, "obsbox", "claude", "sid-mp");
    // THE DISCRIMINATOR: exactly one PHYSICAL line ⇒ write-time serialization, not
    // reader-side dedup over a physically-duplicated stream.
    assert_eq!(
        physical, 1,
        "MASQUERADE CHECK: {N} real processes produced {physical} physical lines — \
         write-time idempotency requires exactly 1 (else reader-side dedup masquerade)"
    );
    assert_eq!(wins, 1, "exactly one process won the first sighting across real processes");
    assert_eq!(noops, N - 1, "every other process observed 'already recorded'");
}

// ===========================================================================
// PROBE 8 — FIX-SPECIFIC (F-DEOBS-1): a TORN `qd mark` tail injected BETWEEN our
// locked scan and our append. `qd mark` does NOT take observed.lock, so a torn
// tail can appear mid-critical-section; the fix's self-delimited `\n{line}\n`
// must still land INDEPENDENTLY READABLE. Deterministic via the before_append
// hook (fires post-scan, pre-append, while WE hold the lock). Teeth: RED on the
// pre-fix bare `{line}\n` append (glued → unreadable), GREEN on the fix.
// ===========================================================================
#[test]
fn probe8_torn_mark_injected_between_scan_and_append() {
    use std::io::Write as _;

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    std::fs::create_dir_all(&state).unwrap();

    let marks_for_hook = marks.clone();
    let hook = move || {
        // A non-lock-respecting writer (qd mark) appends a TORN payload with NO
        // trailing newline — the exact dirty tail the fix defends against.
        let torn = format!(
            "{{\"ts\":\"t\",\"sessionId\":\"s\",\"payload\":\"{}",
            "M".repeat(300)
        );
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&marks_for_hook)
            .unwrap();
        f.write_all(torn.as_bytes()).unwrap(); // no newline → torn tail
        f.flush().unwrap();
    };
    let hooks = RecordHooks {
        before_append: Some(Box::new(hook)),
        fail_append: false,
    };

    let c = clock();
    let r = record_observed_in(&state, &c, "obsbox", "claude", "sid-mark", None, &hooks);
    dump_state("probe8", &state);
    println!("probe8: record with torn-mark injected mid-section = {r:?}");
    let readable = count_observed_exact(&marks, "obsbox", "claude", "sid-mark");
    let marker = marker_path(&state, "obsbox", "claude", "sid-mark");
    println!("probe8: readable={readable} marker_exists={}", marker.exists());

    assert_eq!(r, Ok(true), "first sighting appends");
    assert_eq!(
        readable, 1,
        "self-delimited record is INDEPENDENTLY READABLE despite a torn qd-mark tail \
         landing between scan and append (F-DEOBS-1 fix must hold)"
    );
    assert!(marker.exists(), "marker committed AND (given readable==1) marker⟹readable holds");
}

// ===========================================================================
// PROBE 9 — FIX-SPECIFIC: the leading-`\n` self-delimited writes accumulate blank
// lines between records; readers (fold_marks / observed_line_in_stream) must SKIP
// them and NEVER treat a blank line as a spurious record. We force the SCAN path
// (delete markers) to prove observed_line_in_stream skips the leading blanks and
// stays idempotent over a self-delimited stream.
// ===========================================================================
#[test]
fn probe9_leading_newline_blank_safety_and_scan_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    let c = clock();

    // Two distinct first sightings → `\n{K1}\n\n{K2}\n` (blank line between).
    assert_eq!(
        record_observed_in(&state, &c, "obsbox", "claude", "sid-b1", None, &RecordHooks::default()),
        Ok(true)
    );
    assert_eq!(
        record_observed_in(&state, &c, "obsbox", "claude", "sid-b2", None, &RecordHooks::default()),
        Ok(true)
    );
    dump_state("probe9", &state);

    // No spurious/duplicate records; each key readable exactly once.
    assert_eq!(count_observed_exact(&marks, "obsbox", "claude", "sid-b1"), 1);
    assert_eq!(count_observed_exact(&marks, "obsbox", "claude", "sid-b2"), 1);

    // Force the SCAN path: delete the fast-path markers so re-observe must consult
    // observed_line_in_stream over the blank-line-laced stream.
    std::fs::remove_dir_all(state.join("observed-claims")).unwrap();
    // The scan must SKIP the leading blank lines and find each key → Ok(false),
    // NO new line appended (idempotent over the self-delimited stream).
    assert_eq!(
        record_observed_in(&state, &c, "obsbox", "claude", "sid-b1", None, &RecordHooks::default()),
        Ok(false),
        "scan skips leading blanks and finds K1 → no re-append"
    );
    assert_eq!(
        record_observed_in(&state, &c, "obsbox", "claude", "sid-b2", None, &RecordHooks::default()),
        Ok(false),
        "scan skips leading blanks and finds K2 → no re-append"
    );
    assert_eq!(count_observed_exact(&marks, "obsbox", "claude", "sid-b1"), 1, "still exactly one K1");
    assert_eq!(count_observed_exact(&marks, "obsbox", "claude", "sid-b2"), 1, "still exactly one K2");
}

// ===========================================================================
// ROUND 2 — DIVERSIFIED SURFACES (fresh attack angles, not round-1 re-runs).
// ===========================================================================

// PROBE 10 — marker-encoding cross-key collisions + separator/traversal forgery.
// Distinct identity triples that try to FORGE the `~` field separator (or smuggle
// path separators / traversal / unicode) must NEVER collide to one marker (a
// collision would suppress a genuine first sighting) and must never escape the
// observed-claims/ dir. Each records INDEPENDENTLY exactly once and is idempotent.
#[test]
fn probe10_marker_encoding_cross_key_no_collision_no_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    let claims = state.join("observed-claims");
    let c = clock();

    // Triples that collide ONLY if `~` can be forged from within a field, plus
    // path-separator / traversal / unicode smuggling attempts.
    let keys: Vec<(&str, &str, &str)> = vec![
        ("a~b", "c", "d"),   // ~ in host
        ("a", "b~c", "d"),   // ~ in harness
        ("a", "b", "c~d"),   // ~ in sid
        ("../../etc", "claude", "x/y"), // traversal + path sep
        ("h", "cl/aude", "s\u{00e9}\u{1f600}"), // path sep + unicode bytes
        ("h", "claude", "s"),           // a plain control key
    ];

    for (host, harness, sid) in &keys {
        assert_eq!(
            record_observed_in(&state, &c, host, harness, sid, None, &RecordHooks::default()),
            Ok(true),
            "distinct key ({host:?},{harness:?},{sid:?}) is its own first sighting"
        );
        // Idempotent immediately.
        assert_eq!(
            record_observed_in(&state, &c, host, harness, sid, None, &RecordHooks::default()),
            Ok(false),
            "re-sight of ({host:?},{harness:?},{sid:?}) no-ops"
        );
        assert_eq!(
            count_observed_exact(&marks, host, harness, sid),
            1,
            "exactly one line for ({host:?},{harness:?},{sid:?}) — no cross-key suppression"
        );
    }
    dump_state("probe10", &state);

    // Every marker is a DIRECT child of observed-claims/ (no `/` created a subdir;
    // no traversal escaped the dir). Count == number of distinct keys.
    let mut marker_files = 0usize;
    for e in std::fs::read_dir(&claims).unwrap().flatten() {
        assert!(e.path().is_file(), "marker {:?} must be a plain file (no subdir/traversal)", e.path());
        assert_eq!(
            e.path().parent().unwrap(),
            claims.as_path(),
            "marker escaped observed-claims/ (path traversal in a key field)"
        );
        marker_files += 1;
    }
    assert_eq!(marker_files, keys.len(), "one distinct marker per distinct key (no collisions)");
}

// PROBE 11 — oversized sessionId ⇒ marker-creation FAILS (filename > 255 bytes),
// but idempotency + readability must still hold via the authoritative locked scan.
// A best-effort marker that can never be created must NOT poison the key nor cause
// a duplicate: the fast path simply always misses and the scan is the authority.
#[test]
fn probe11_oversized_sessionid_marker_failure_is_nonfatal_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    let c = clock();

    // ~320-char sessionId → encoded marker basename > 255 bytes → create fails.
    let big_sid: String = std::iter::repeat('Z').take(320).collect();

    // First sighting still records the line (marker create fails, swallowed).
    let r1 = record_observed_in(&state, &c, "obsbox", "claude", &big_sid, None, &RecordHooks::default());
    println!("probe11: first sighting (oversized sid) = {r1:?}");
    assert_eq!(r1, Ok(true), "line records even though the marker can't be created");
    assert_eq!(count_observed_exact(&marks, "obsbox", "claude", &big_sid), 1);

    // The marker was NOT created (name too long) — confirm the fast path can't have it.
    let claims = state.join("observed-claims");
    let marker_present = std::fs::read_dir(&claims)
        .map(|rd| rd.flatten().count() > 0)
        .unwrap_or(false);
    println!("probe11: any marker created for oversized key = {marker_present}");

    // Repeated calls MUST stay idempotent via the locked scan (no marker to hit).
    for _ in 0..5 {
        assert_eq!(
            record_observed_in(&state, &c, "obsbox", "claude", &big_sid, None, &RecordHooks::default()),
            Ok(false),
            "scan is authoritative when the marker can never exist — no duplicate"
        );
    }
    dump_state("probe11", &state);
    assert_eq!(
        count_observed_exact(&marks, "obsbox", "claude", &big_sid),
        1,
        "still exactly one line — marker-failure is non-fatal and never duplicates"
    );
}

// PROBE 12 — mixed-concurrency CHAOS: N distinct-key observers race a non-lock-
// respecting writer that hammers marks.jsonl with TORN (unterminated) qd-mark-like
// fragments. The self-delimited observed write must guarantee EVERY observed key
// ends up readable exactly once (marker⟹readable), regardless of torn tails
// landing around it. Bounded joins; chaos stops after observers finish.
#[test]
fn probe12_concurrency_chaos_distinct_keys_vs_torn_mark_writer() {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    let marks = state.join("marks.jsonl");
    std::fs::create_dir_all(&state).unwrap();

    const N: usize = 8;
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(N + 1));

    // Chaos thread: append TORN mark fragments (no trailing newline), no lock.
    let chaos = {
        let marks = marks.clone();
        let stop = stop.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&marks) {
                    // A torn (unterminated) mark-ish fragment — no newline.
                    let _ = f.write_all(format!("{{\"ts\":\"t\",\"payload\":\"chaos-{i}", ).as_bytes());
                    let _ = f.flush();
                }
                i += 1;
            }
        })
    };

    // Observer threads: each records a DISTINCT key concurrently.
    let mut observers = Vec::new();
    for k in 0..N {
        let state = state.clone();
        let barrier = barrier.clone();
        observers.push(thread::spawn(move || {
            barrier.wait();
            let c = clock();
            let sid = format!("sid-c{k}");
            record_observed_in(&state, &c, "obsbox", "claude", &sid, None, &RecordHooks::default())
        }));
    }

    // Bounded join of observers (bubble on hang, never spin).
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut results = Vec::new();
    for (k, h) in observers.into_iter().enumerate() {
        // thread::join has no timeout; guard via an overall deadline check after.
        results.push((k, h.join()));
        if Instant::now() >= deadline {
            // Fall through; the assert below will catch a missing result.
        }
    }
    stop.store(true, Ordering::Relaxed);
    chaos.join().unwrap();
    dump_state("probe12", &state);

    for (k, r) in &results {
        let r = r.as_ref().expect("observer thread panicked");
        assert_eq!(*r, Ok(true), "observer {k} recorded its distinct first sighting");
    }
    // Every distinct key readable EXACTLY once despite the torn-mark chaos.
    for k in 0..N {
        let sid = format!("sid-c{k}");
        let n = count_observed_exact(&marks, "obsbox", "claude", &sid);
        assert_eq!(n, 1, "key {sid} must be readable exactly once (got {n}) over a torn-chaos stream");
        let marker = marker_path(&state, "obsbox", "claude", &sid);
        assert!(marker.exists(), "marker⟹readable holds for {sid}");
    }
}
