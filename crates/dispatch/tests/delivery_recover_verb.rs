//! D1 (delivery contract §C2) — integration proofs for the `qd delivery:recover`
//! verb. These run the ACTUAL COMPILED `qd` binary (never a bare library call), per
//! the charge: the verb's real behavior — including the `is_dead_dangling` LIVENESS
//! FENCE — is the thing under test.
//!
//! The two required proofs:
//!  1. `live_writer_send_is_refused` — a `send-initiated` whose writer pid is ALIVE
//!     (foreign, evaluated via the RF-6 start_ms arm) is NOT closed: the verb writes
//!     NO terminal. This is THE fence proof.
//!  2. `dead_writer_orphan_*` — a real orphaned initiation (writer pid DEAD) is
//!     resolved through the R6 recovery-terminus lattice (seam ruling):
//!     `turn-anchored{recovered}` when the transcript matches; (c) the DISCLOSED
//!     `pending-abandoned{recovery-no-candidate, recovered:true, attribution}` when a
//!     NON-matching record sits past the anchor (searched, exhausted best-effort); and
//!     NO terminal (left dead-dangling-recoverable) for the two UNDETERMINED states —
//!     (a) transcript unreadable/unresolvable (source-unavailable) and (b) empty window
//!     (read OK, nothing past the anchor). A landed-but-undetermined send is never
//!     foreclosed.
//!
//! Plus attack_h (both build_window arms flip), the (b) empty-window flip, the (d)
//! `recovery-unattributable` closer, target-selection, and the relay-scoping defense.

use std::path::{Path, PathBuf};
use std::process::Command;

// The normative terminal set (mirrors events::TERMINAL_EVENTS; a terminal here
// means "the send is resolved"). send-initiated / relay-delivered are NOT terminal.
const TERMINALS: &[&str] = &[
    "turn-anchored",
    "turn-anchored-mismatch",
    "anchor-timeout",
    "pending-abandoned",
    "message-seen",
    "seen-failed",
    "send-failed",
];

/// An old ISO ts (years before now) so the §7 age gate (>30s) always passes — the
/// discriminator: with age satisfied, ONLY the liveness check can refuse recovery.
const OLD_TS: &str = "2020-01-01T00:00:00.000Z";

struct Jail {
    _root: tempfile::TempDir,
    home: PathBuf,
    qd_home: PathBuf,
    /// **qw's** delivery log dir — every terminal this verb produces lands here,
    /// and every assertion below reads it.
    sessions_dir: PathBuf,
    /// **qd's** intent log dir — what the sweep enumerates.
    intent_dir: PathBuf,
}

fn jail() -> Jail {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let qd_home = root.path().join("qd");
    // The ledger is TWO files (`09-ledger-split.md`): qd's intent record lives at
    // <QD_HOME>/state/intent/<key>.events.jsonl and qw's delivery/terminal records
    // at <QD_HOME>/state/sessions/<key>.events.jsonl. The sweep reads the first;
    // `recover` reads and writes the second. A fixture that planted only one of
    // them would be testing a state production cannot reach.
    let sessions_dir = qd_home.join("state").join("sessions");
    let intent_dir = qd_home.join("state").join("intent");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::create_dir_all(&intent_dir).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    Jail {
        _root: root,
        home,
        qd_home,
        sessions_dir,
        intent_dir,
    }
}

/// Append one raw JSON line to `<dir>/<key>.events.jsonl`.
fn append_line(dir: &Path, key: &str, line: &str) {
    use std::io::Write;
    let path = dir.join(format!("{key}.events.jsonl"));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

/// Plant ONE send across BOTH halves of the ledger, exactly as a real send leaves
/// it — raw JSON so we control pid/ts/start_ms (an `EventWriter` would stamp the
/// CURRENT pid/ts, which is exactly what these proofs must NOT do).
///
/// **qd's intent record** (`intent/<key>`) carries the envelope the dead-writer
/// fence reads — `pid`, `start_ms`, `ts` — and NO recovery keys, because qd
/// resolves no transcripts. **qw's delivery record** (`sessions/<key>`) carries
/// the recovery keys `recover` searches from: `content_sha256`, `transcript`,
/// `transcript_offset`. Same `send_id` in both; that correlation is the whole
/// point of qd minting the id before the wire.
///
/// Splitting the fixture this way is not cosmetic — it is what proves the split:
/// the fence is evaluated on a record with no transcript in it, and the search is
/// run from a record whose pid the fence never saw.
#[allow(clippy::too_many_arguments)]
fn write_send_initiated(
    j: &Jail,
    key: &str,
    sid: &str,
    send_id: &str,
    verb: &str,
    pid: u32,
    ts: &str,
    start_ms: Option<i64>,
    content: &str,
    transcript: Option<&str>,
) {
    let sha = dispatch::events::sha256_hex(content.as_bytes());
    let envelope = |send_path: &str| {
        let mut o = serde_json::Map::new();
        o.insert("v".into(), serde_json::json!(1));
        o.insert("ts".into(), serde_json::json!(ts));
        o.insert("pid".into(), serde_json::json!(pid));
        o.insert("seq".into(), serde_json::json!(0));
        o.insert("session".into(), serde_json::json!(sid));
        if let Some(s) = start_ms {
            o.insert("start_ms".into(), serde_json::json!(s));
        }
        o.insert("event".into(), serde_json::json!("send-initiated"));
        o.insert("send_id".into(), serde_json::json!(send_id));
        o.insert("verb".into(), serde_json::json!(verb));
        o.insert("send_path".into(), serde_json::json!(send_path));
        o.insert("content_sha256".into(), serde_json::json!(sha));
        o.insert("content_len".into(), serde_json::json!(content.len()));
        o.insert("chunks".into(), serde_json::json!(1));
        o.insert("chunk_sha256s".into(), serde_json::json!([sha]));
        o
    };

    // qd's half: the fence's inputs, no recovery keys.
    append_line(
        &j.intent_dir,
        key,
        &serde_json::Value::Object(envelope("intent")).to_string(),
    );

    // qw's half: the recovery keys.
    let mut o = envelope("idle");
    if let Some(t) = transcript {
        o.insert("transcript".into(), serde_json::json!(t));
        o.insert("transcript_offset".into(), serde_json::json!(0));
    }
    append_line(
        &j.sessions_dir,
        key,
        &serde_json::Value::Object(o).to_string(),
    );
}

/// Run the compiled `qd delivery:recover` (optionally `--send-id`), jailed.
fn run_recover(j: &Jail, send_id: Option<&str>) -> (bool, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qd"));
    cmd.arg("delivery:recover");
    if let Some(id) = send_id {
        cmd.arg("--send-id").arg(id);
    }
    cmd.env("HOME", &j.home)
        .env("QD_HOME", &j.qd_home)
        .env_remove("QD_BOOT_AWAIT_RELAY");
    let out = cmd.output().expect("run qd delivery:recover");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    (out.status.success(), stdout)
}

/// The event kinds present in a session's events file, in file order.
fn events_in(sessions_dir: &Path, key: &str) -> Vec<String> {
    let path = sessions_dir.join(format!("{key}.events.jsonl"));
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            v.get("event")?.as_str().map(str::to_string)
        })
        .collect()
}

fn terminals_in(sessions_dir: &Path, key: &str) -> Vec<String> {
    events_in(sessions_dir, key)
        .into_iter()
        .filter(|e| TERMINALS.contains(&e.as_str()))
        .collect()
}

fn write_transcript(dir: &Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    let line = serde_json::json!({ "type": "user", "message": { "content": content } });
    std::fs::write(&path, format!("{line}\n")).unwrap();
    path.display().to_string()
}

/// chmod a file to `mode` (unix). Used to make a transcript UNREADABLE (0o000) at
/// verb time then READABLE (0o644) for the recovery flip (attack_h, offset-present
/// read-failure arm).
fn chmod(path: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Create a READABLE but EMPTY transcript file (zero user records) — the (b)
/// empty-window shape: the read succeeds, no candidate sits past the anchor.
fn write_empty_transcript(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, "").unwrap();
    path.display().to_string()
}

/// The (c) disclosed abandoned terminal's JSON for a key — so a proof can assert its
/// `reason` / `recovered` / `attribution` disclosure flags.
fn pending_abandoned_record(sessions_dir: &Path, key: &str) -> serde_json::Value {
    let path = sessions_dir.join(format!("{key}.events.jsonl"));
    let text = std::fs::read_to_string(&path).unwrap();
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("event").and_then(|e| e.as_str()) == Some("pending-abandoned"))
        .expect("pending-abandoned present")
}

/// Append a raw `send-initiated` line that LACKS `content_sha256` — the (d)
/// unattributable shape (a legacy/foreign record with no recovery key). Mirrors
/// `write_send_initiated` but omits the sha (and the chunk shas).
fn write_send_initiated_no_sha(j: &Jail, key: &str, sid: &str, send_id: &str, pid: u32) {
    let line = |send_path: &str| {
        let mut o = serde_json::Map::new();
        o.insert("v".into(), serde_json::json!(1));
        o.insert("ts".into(), serde_json::json!(OLD_TS));
        o.insert("pid".into(), serde_json::json!(pid));
        o.insert("seq".into(), serde_json::json!(0));
        o.insert("session".into(), serde_json::json!(sid));
        o.insert("event".into(), serde_json::json!("send-initiated"));
        o.insert("send_id".into(), serde_json::json!(send_id));
        o.insert("verb".into(), serde_json::json!("send:pty"));
        o.insert("send_path".into(), serde_json::json!(send_path));
        // NO content_sha256 / chunk_sha256s — the legacy/foreign record.
        serde_json::Value::Object(o).to_string()
    };
    append_line(&j.intent_dir, key, &line("intent"));
    append_line(&j.sessions_dir, key, &line("idle"));
}

// =========================================================================
// PROOF 1 — the LIVENESS FENCE: a live-writer send is REFUSED (THE required proof)
// =========================================================================
#[test]
fn live_writer_send_is_refused() {
    let j = jail();
    // A LIVE, FOREIGN writer: this test process itself (alive throughout its own
    // run; its pid differs from the spawned verb's, so the verb evaluates it via the
    // RF-6 start_ms arm, NOT the own-pid short-circuit). start_ms = our real process
    // start, so the fence's start_ms arm sees a MATCHING incarnation → writer alive.
    let live_pid = std::process::id();
    let start_ms = dispatch::effects::proc_start_ms(live_pid as i32);
    assert!(
        start_ms.is_some(),
        "proc_start_ms must resolve so the start_ms fence arm is the one under test"
    );

    // OLD ts → the §7 age gate passes; the ONLY thing that can refuse recovery is the
    // liveness check. A recent ts would give a false-pass via the age gate.
    write_send_initiated(
        &j,
        "sid-live",
        "sid-live",
        "live-1",
        "send:pty",
        live_pid,
        OLD_TS,
        start_ms,
        "please do not recover me — my writer is alive",
        None,
    );

    let before = events_in(&j.sessions_dir, "sid-live");
    assert_eq!(
        before,
        vec!["send-initiated"],
        "precondition: only the initiation"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");

    // THE ASSERTION: no terminal was appended — the send stays dangling-but-live.
    let terms = terminals_in(&j.sessions_dir, "sid-live");
    assert!(
        terms.is_empty(),
        "FENCE VIOLATED: a live-writer send got a terminal {terms:?} — premature close (QS-1). stdout: {stdout}"
    );
    // And the verb classified it as live (not merely absent): the fence held.
    assert!(
        stdout.contains("left 1 live-writer") && stdout.contains("fence held"),
        "verb should report the fence held for 1 send; stdout: {stdout}"
    );
    // The initiation is untouched.
    assert_eq!(
        events_in(&j.sessions_dir, "sid-live"),
        vec!["send-initiated"]
    );
}

// =========================================================================
// PROOF 2a (finding H — CHANGED by the narrow fix) — a DEAD-writer orphan whose
// transcript is UNRESOLVABLE is NOT foreclosed: NO terminal, left dead-dangling.
//
// This test formerly (`dead_writer_orphan_closes_via_abandoned`) planted NO
// transcript and asserted `pending-abandoned{recovery-no-candidate}`. But "no
// transcript resolvable" is the offset-ABSENT resolve-FAILURE arm of build_window —
// a SourceUnavailable scenario (undeterminable), NOT the NoRecord case. Under the H
// fix (seam ruling) recovery must NOT foreclose an undeterminable send:
// it emits no terminal and leaves it dead-dangling-recoverable for a later run. The
// genuine "no candidate → abandoned" (NoRecord) case moves to the dedicated readable
// control below. (H-VERDICT documents this flip loudly.)
// =========================================================================
#[test]
fn dead_writer_orphan_unresolvable_transcript_left_dangling() {
    let j = jail();
    let dead_pid = known_dead_pid();

    // DEAD writer + OLD ts + NO transcript (and no resolvable jsonl in the jail's
    // projects dir) → dead-dangling, and build_window's offset-absent resolve fails →
    // SourceUnavailable. The orphan is LEFT UNRESOLVED (no terminal), not abandoned.
    write_send_initiated(
        &j,
        "sid-dead-a",
        "sid-dead-a",
        "orphan-a",
        "send:pty",
        dead_pid,
        OLD_TS,
        None, // no start_ms → pid-alive check: dead pid → writer gone
        "orphaned by a mid-send kill",
        None,
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");

    // THE ASSERTION (H): NO terminal — an undeterminable send is never foreclosed.
    let terms = terminals_in(&j.sessions_dir, "sid-dead-a");
    assert!(
        terms.is_empty(),
        "FORECLOSURE (finding H): an unresolvable-transcript send got a terminal {terms:?} — \
         it must be left dead-dangling-recoverable. stdout: {stdout}"
    );
    assert!(
        stdout.contains("source-unavailable 1"),
        "verb should report one source-unavailable (transcript unresolvable) send; stdout: {stdout}"
    );
    assert!(
        stdout.contains("recovered 0") && stdout.contains("abandoned 0"),
        "nothing was recovered or abandoned (no terminal minted); stdout: {stdout}"
    );
    // The initiation is untouched — still dead-dangling for a later run.
    assert_eq!(
        events_in(&j.sessions_dir, "sid-dead-a"),
        vec!["send-initiated"]
    );
}

// =========================================================================
// PROOF 2a-control (R6 (c) — the SEARCHED-no-match disclosed closer) — a DEAD-writer
// orphan whose transcript is READABLE and holds a NON-MATCHING candidate past the
// anchor CLOSES via the DISCLOSED pending-abandoned{recovery-no-candidate,
// recovered:true, attribution}. The ONLY legitimate foreclosing recovery terminal —
// proving the lattice split is surgical: a searched, non-empty window that yields no
// match is exhausted best-effort, disclosed (never a hard "failed").
// =========================================================================
#[test]
fn dead_writer_orphan_readable_no_candidate_recovers_abandoned() {
    let j = jail();
    let dead_pid = known_dead_pid();
    let msg = "the sent message that never landed";
    // A READABLE transcript that EXISTS and holds a DIFFERENT (non-matching) user
    // record past the anchor → build_window SUCCEEDS, candidates non-empty, no match →
    // (c) SEARCHED-no-match → the disclosed pending-abandoned closer.
    let transcript = write_transcript(
        j._root.path(),
        "nr.jsonl",
        "an entirely different and much longer user turn that is not the sent message at all",
    );

    write_send_initiated(
        &j,
        "sid-nr",
        "sid-nr",
        "nr-1",
        "send:pty",
        dead_pid,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");

    let terms = terminals_in(&j.sessions_dir, "sid-nr");
    assert_eq!(
        terms,
        vec!["pending-abandoned"],
        "a READABLE searched-no-match transcript closes via the disclosed recovery verdict (c); stdout: {stdout}"
    );
    // The DISCLOSED closer (R6 (c)): reason recovery-no-candidate + recovered:true +
    // attribution — a landed-but-abandoned send reads through D4's "recovered
    // (attributed)" category, never a hard "failed". Distinct from a foreclosing
    // SourceUnavailable (which mints NO terminal).
    let pa = pending_abandoned_record(&j.sessions_dir, "sid-nr");
    assert_eq!(
        pa.get("reason").and_then(|r| r.as_str()),
        Some("recovery-no-candidate"),
        "the searched-no-match case keeps recovery-no-candidate"
    );
    assert_eq!(
        pa.get("recovered").and_then(|b| b.as_bool()),
        Some(true),
        "the disclosed closer stamps recovered:true (R6 (c) — no UNDISCLOSED false-abandoned)"
    );
    assert_eq!(
        pa.get("attribution").and_then(|a| a.as_str()),
        Some("offset"),
        "the disclosed closer carries the search attribution (offset here)"
    );
    assert!(
        stdout.contains("abandoned 1") && stdout.contains("no-candidate 1"),
        "verb should report one no-candidate abandoned closer; stdout: {stdout}"
    );
    assert!(
        stdout.contains("source-unavailable 0") && stdout.contains("window-empty 0"),
        "a readable, searched transcript is neither source-unavailable nor window-empty; stdout: {stdout}"
    );
}

// =========================================================================
// PROOF 2a-H (attack_h) — the finding-H FLIP, BOTH build_window arms. A dead-dangling
// send whose content IS genuinely in the (initially unreadable) transcript is NOT
// foreclosed while the transcript can't be read; once the transcript becomes
// readable, a re-run RESOLVES it to turn-anchored{recovered}. Mirrors the G per-arm
// proofs. Arm 1 = offset-present read-failure (chmod 000); arm 2 = offset-absent
// resolve-failure (no jsonl → planted jsonl).
// =========================================================================
#[test]
fn attack_h_offset_present_unreadable_left_dangling_then_recovers() {
    let j = jail();
    let dead_pid = known_dead_pid();
    let msg = "recover me once my transcript is readable again";
    // The content DID land: it is genuinely in the transcript. But the recorded path
    // is UNREADABLE at verb time (chmod 000) → build_window's offset-present
    // read_transcript fails → SourceUnavailable → NO terminal (must NOT foreclose).
    let transcript = write_transcript(j._root.path(), "attack_h_present.jsonl", msg);
    chmod(&transcript, 0o000);

    write_send_initiated(
        &j,
        "sid-h1",
        "sid-h1",
        "h1-1",
        "send:pty",
        dead_pid,
        OLD_TS,
        None,
        msg,
        Some(&transcript), // offset-present arm (transcript + offset recorded)
    );

    // Run 1 — transcript unreadable → NO terminal (H: not foreclosed).
    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    assert!(
        terminals_in(&j.sessions_dir, "sid-h1").is_empty(),
        "FORECLOSURE (H, offset-present): an unreadable transcript that DOES hold the content \
         got a terminal — must be left dead-dangling. stdout: {stdout}"
    );
    assert!(
        stdout.contains("source-unavailable 1"),
        "run 1 should report the send left recoverable (source-unavailable); stdout: {stdout}"
    );

    // Flip — make the transcript READABLE, re-run → the send RESOLVES to
    // turn-anchored{recovered} (the content was there all along).
    chmod(&transcript, 0o644);
    let (ok2, stdout2) = run_recover(&j, None);
    assert!(ok2, "verb exits 0 on the re-run; stdout: {stdout2}");
    let kinds = events_in(&j.sessions_dir, "sid-h1");
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "once readable, the send recovers to turn-anchored{{recovered}}; got {kinds:?}; stdout: {stdout2}"
    );
    assert!(
        !kinds.iter().any(|e| e == "pending-abandoned"),
        "the send must NOT have been permanently abandoned by run 1; got {kinds:?}"
    );
    assert!(
        stdout2.contains("anchored 1"),
        "the re-run should report anchored 1; stdout: {stdout2}"
    );
}

#[test]
fn attack_h_offset_absent_unresolvable_left_dangling_then_recovers() {
    let j = jail();
    let dead_pid = known_dead_pid();
    let msg = "recover me once my session transcript resolves";
    let sid = "sid-h2";

    // Offset-absent arm: NO transcript recorded on the send-initiated, and NO jsonl in
    // the jail's projects dir → build_window's resolve_transcript fails →
    // SourceUnavailable → NO terminal (must NOT foreclose).
    write_send_initiated(
        &j,
        sid,
        sid,
        "h2-1",
        "send:pty",
        dead_pid,
        OLD_TS,
        None,
        msg,
        None, // offset-absent arm (no transcript/offset recorded)
    );

    // Run 1 — transcript unresolvable → NO terminal (H: not foreclosed).
    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    assert!(
        terminals_in(&j.sessions_dir, sid).is_empty(),
        "FORECLOSURE (H, offset-absent): an unresolvable transcript got a terminal — \
         must be left dead-dangling. stdout: {stdout}"
    );
    assert!(
        stdout.contains("source-unavailable 1"),
        "run 1 should report the send left recoverable (source-unavailable); stdout: {stdout}"
    );

    // Flip — plant the recipient jsonl where find_jsonl_path resolves it (scan tier:
    // <projects_dir>/<any-project>/<session_id>.jsonl), containing the sent content.
    // projects_dir = <HOME>/.claude/projects (QdPaths::from_home_env).
    let proj = j.home.join(".claude").join("projects").join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    write_transcript(&proj, &format!("{sid}.jsonl"), msg);

    let (ok2, stdout2) = run_recover(&j, None);
    assert!(ok2, "verb exits 0 on the re-run; stdout: {stdout2}");
    let kinds = events_in(&j.sessions_dir, sid);
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "once the session transcript resolves, the send recovers to turn-anchored{{recovered}}; \
         got {kinds:?}; stdout: {stdout2}"
    );
    assert!(
        !kinds.iter().any(|e| e == "pending-abandoned"),
        "the send must NOT have been permanently abandoned by run 1; got {kinds:?}"
    );
    assert!(
        stdout2.contains("anchored 1"),
        "the re-run should report anchored 1 (time-window attribution); stdout: {stdout2}"
    );
}

// =========================================================================
// PROOF 2b-EMPTY (R6 (b)) — a dead-dangling send whose recipient transcript is
// READABLE but has NO record past the anchor yet (busy-turn flush lag /
// rotation-in-place) is NOT foreclosed: NO terminal (window-empty); once the window
// GROWS (the turn flushes), a re-run resolves it. The crux of R6: absence is evidence
// only relative to a SEARCHED, NON-EMPTY window.
// =========================================================================
#[test]
fn empty_window_left_dangling_then_recovers_when_window_grows() {
    let j = jail();
    let dead_pid = known_dead_pid();
    let msg = "recover me once the recipient turn flushes";
    // A READABLE but EMPTY transcript at the recorded path (offset 0) → read succeeds,
    // ZERO candidates past the anchor → (b) EmptyWindow → NO terminal.
    let transcript = write_empty_transcript(j._root.path(), "empty_window.jsonl");
    write_send_initiated(
        &j,
        "sid-ew",
        "sid-ew",
        "ew-1",
        "send:pty",
        dead_pid,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );

    // Run 1 — empty window → NO terminal (still growable).
    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    assert!(
        terminals_in(&j.sessions_dir, "sid-ew").is_empty(),
        "EMPTY window must NOT foreclose (still growable) — got a terminal. stdout: {stdout}"
    );
    assert!(
        stdout.contains("window-empty 1"),
        "run 1 should report the send left recoverable (window-empty); stdout: {stdout}"
    );

    // Flip — the window GROWS: the recipient turn flushed the sent content. Re-run →
    // the send resolves to turn-anchored{recovered}.
    write_transcript(j._root.path(), "empty_window.jsonl", msg);
    let (ok2, stdout2) = run_recover(&j, None);
    assert!(ok2, "verb exits 0 on the re-run; stdout: {stdout2}");
    let kinds = events_in(&j.sessions_dir, "sid-ew");
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "once the window grows, the send recovers to turn-anchored; got {kinds:?}; stdout: {stdout2}"
    );
    assert!(
        !kinds.iter().any(|e| e == "pending-abandoned"),
        "the send must NOT have been abandoned by run 1; got {kinds:?}"
    );
    assert!(
        stdout2.contains("anchored 1"),
        "the re-run should report anchored 1; stdout: {stdout2}"
    );
}

// =========================================================================
// PROOF 2d (R6 (d)) — a legacy/foreign send-initiated with NO content_sha256 can
// never be searched → closes as pending-abandoned{recovery-unattributable} (NOT
// "no-candidate": no search ran), with no disclosure flags.
// =========================================================================
#[test]
fn missing_content_sha_recovers_unattributable() {
    let j = jail();
    let dead_pid = known_dead_pid();
    // A dead-dangling send-initiated lacking content_sha256 (legacy/foreign record).
    write_send_initiated_no_sha(&j, "sid-un", "sid-un", "un-1", dead_pid);

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    let terms = terminals_in(&j.sessions_dir, "sid-un");
    assert_eq!(
        terms,
        vec!["pending-abandoned"],
        "an unattributable send closes with exactly one terminal; stdout: {stdout}"
    );
    let pa = pending_abandoned_record(&j.sessions_dir, "sid-un");
    assert_eq!(
        pa.get("reason").and_then(|r| r.as_str()),
        Some("recovery-unattributable"),
        "a no-key record closes as unattributable, never no-candidate (no search ran)"
    );
    assert!(
        pa.get("recovered").is_none(),
        "unattributable carries NO recovered flag (no search ran)"
    );
    assert!(
        pa.get("attribution").is_none(),
        "unattributable carries NO attribution"
    );
    assert!(
        stdout.contains("unattributable 1") && stdout.contains("abandoned 1"),
        "verb reports one unattributable abandoned closer; stdout: {stdout}"
    );
}

// =========================================================================
// PROOF 2b — a DEAD-writer orphan RECOVERS (turn-anchored{recovered}) from transcript
// =========================================================================
#[test]
fn dead_writer_orphan_recovers_anchored_from_transcript() {
    let j = jail();
    let dead_pid = known_dead_pid();
    let msg = "recover me after death";
    // A transcript containing the sent bytes → recovery-read anchors it.
    let transcript = write_transcript(j._root.path(), "t2b.jsonl", msg);

    write_send_initiated(
        &j,
        "sid-dead-b",
        "sid-dead-b",
        "orphan-b",
        "send:pty",
        dead_pid,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");

    let kinds = events_in(&j.sessions_dir, "sid-dead-b");
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "a matching transcript must yield turn-anchored{{recovered}}; got {kinds:?}; stdout: {stdout}"
    );
    // The anchored terminal must carry recovered:true (a LATE, recovered anchor).
    let path = j.sessions_dir.join("sid-dead-b.events.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let anchored = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("event").and_then(|e| e.as_str()) == Some("turn-anchored"))
        .expect("turn-anchored present");
    assert_eq!(
        anchored.get("recovered").and_then(|b| b.as_bool()),
        Some(true),
        "recovered anchor must be flagged recovered:true"
    );
    assert!(
        stdout.contains("anchored 1"),
        "stdout should report anchored 1; got: {stdout}"
    );
}

// =========================================================================
// PROOF 3 — target selection: --send-id closes only that send; others untouched
// =========================================================================
#[test]
fn targeted_send_id_only_recovers_that_send() {
    let j = jail();
    let dead = known_dead_pid();
    // Two dead-dangling sends in one session. The targeted send gets a READABLE
    // no-match transcript so recovery CLOSES it via pending-abandoned (a terminal
    // appears — the target-selection assertion). (Finding H: an absent transcript is
    // SourceUnavailable → no terminal, which could not exercise "the targeted send
    // closes".) The untargeted send is never scanned, so its transcript is immaterial.
    let want_transcript = write_transcript(
        j._root.path(),
        "target.jsonl",
        "an unrelated user turn that is not the targeted send",
    );
    write_send_initiated(
        &j,
        "sid-t",
        "sid-t",
        "want",
        "send:pty",
        dead,
        OLD_TS,
        None,
        "target me",
        Some(&want_transcript),
    );
    write_send_initiated(
        &j,
        "sid-t",
        "sid-t",
        "leave",
        "send:pty",
        dead,
        OLD_TS,
        None,
        "not me",
        None,
    );

    let (ok, stdout) = run_recover(&j, Some("want"));
    assert!(ok, "verb exits 0; stdout: {stdout}");

    // Exactly one terminal, and it resolves the targeted send only.
    let terms = terminals_in(&j.sessions_dir, "sid-t");
    assert_eq!(
        terms.len(),
        1,
        "only the targeted send closes; stdout: {stdout}"
    );
    // The untargeted send has no terminal joined to it.
    let path = j.sessions_dir.join("sid-t.events.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let closed_leave = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| {
            v.get("event").and_then(|e| e.as_str()) == Some("pending-abandoned")
                && v.get("send_id").and_then(|s| s.as_str()) == Some("leave")
        });
    assert!(!closed_leave, "the untargeted send must stay open");
}

// =========================================================================
// PROOF 4 — scoping defense: a dead-dangling RELAY send is NOT swept (no false failure)
// =========================================================================
#[test]
fn relay_send_is_not_swept() {
    let j = jail();
    let dead = known_dead_pid();
    // A relay send-initiated (verb:"send:relay", no transcript) that is dead-dangling.
    // recovery-read would find no candidate and manufacture a false pending-abandoned;
    // the verb must SKIP it (relay sends resolve via their recipient observer, C5/C6).
    write_send_initiated(
        &j,
        "sid-relay",
        "sid-relay",
        "relay-1",
        "send:relay",
        dead,
        OLD_TS,
        None,
        "a relay message the observer will confirm",
        None,
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");

    let terms = terminals_in(&j.sessions_dir, "sid-relay");
    assert!(
        terms.is_empty(),
        "a relay send must NOT be swept by the recovery verb (no false failure); got {terms:?}; stdout: {stdout}"
    );
    assert!(
        stdout.contains("scanned 0"),
        "the sweep scans 0 eligible sends (relay is out of scope); stdout: {stdout}"
    );
}

// =========================================================================
// PROOF 6 (R5 seam ruling F3) — a partial write that LANDED is recovered, not false-failed
// =========================================================================
#[test]
fn partial_write_that_landed_is_recovered_not_false_failed() {
    // The ruled behavior (seam ruling): the pty partial-write door mints NO
    // terminal, so an ack-timeout-but-actually-landed send stays dead-dangling instead
    // of being permanently false-failed with pending-abandoned{partial-write}. The
    // recover verb then resolves it against the transcript ground truth. Here the
    // content DID land (it is in the transcript) → turn-anchored{recovered}, NOT a
    // false "failed".
    let j = jail();
    let dead = known_dead_pid();
    let msg = "a busy-queued send whose bytes landed despite the ack timeout";
    let transcript = write_transcript(j._root.path(), "partial.jsonl", msg);

    // Post-partial-write dead-dangling state: send-initiated (busy-queued), NO terminal
    // (the door minted none per R5), writer dead, old ts.
    write_send_initiated(
        &j,
        "sid-pw",
        "sid-pw",
        "pw-1",
        "send:pty",
        dead,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );

    // Precondition: the door left NO terminal — the send is dead-dangling, not failed.
    assert!(
        terminals_in(&j.sessions_dir, "sid-pw").is_empty(),
        "the partial-write door must mint no terminal (R5) — the send stays dead-dangling"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");

    // The verb recovers the LANDED send as turn-anchored{recovered} — the false
    // permanent "failed" the old door terminal would have minted is gone.
    let kinds = events_in(&j.sessions_dir, "sid-pw");
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "a landed partial-write send must recover to turn-anchored, not stay failed; got {kinds:?}; stdout: {stdout}"
    );
    assert!(
        !kinds.iter().any(|e| e == "pending-abandoned"),
        "a landed send must NOT be recorded as pending-abandoned/failed; got {kinds:?}"
    );
    assert!(
        stdout.contains("anchored 1"),
        "stdout should report anchored 1; got: {stdout}"
    );
}

// =========================================================================
// PROOF 7 (R5 rider 2, red-team finding B) — a WATCH-INTERRUPTED send that LANDED
// is recovered, not false-failed
// =========================================================================
#[test]
fn watch_interrupted_that_landed_is_recovered_not_false_failed() {
    // Finding B is the F3 lie-shape at the pty `--wait` watch phase (send.rs
    // SourceError arm). By the time the watch runs, the send's bytes were FULLY
    // acked (chunks-delivered emitted) and the `\r` submitted the turn; a watch
    // interruption (JSONL integrity lost) makes the turn's fate UNDETERMINABLE, not
    // abandoned. Ruled behavior (rider 2, mirroring F3 4ed923de): the watch-
    // interrupted door mints NO terminal, so an interrupted-but-actually-landed
    // send stays dead-dangling instead of being permanently false-failed with
    // pending-abandoned{watch-interrupted}. The recover verb then resolves it
    // against transcript ground truth. Here the content DID land (it is in the
    // transcript) → turn-anchored{recovered}, NOT a false "failed".
    let j = jail();
    let dead = known_dead_pid();
    let msg = "a --wait send whose turn landed despite the watch being interrupted";
    let transcript = write_transcript(j._root.path(), "watch.jsonl", msg);

    // Post-watch-interrupt dead-dangling state: send-initiated (bytes acked, turn
    // submitted), NO terminal (the door minted none per rider 2), writer dead, old
    // ts. (send_path is immaterial to recovery — it keys on transcript+offset+sha.)
    write_send_initiated(
        &j,
        "sid-wi",
        "sid-wi",
        "wi-1",
        "send:pty",
        dead,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );

    // Precondition: the door left NO terminal — the send is dead-dangling, not failed.
    assert!(
        terminals_in(&j.sessions_dir, "sid-wi").is_empty(),
        "the watch-interrupted door must mint no terminal (rider 2) — the send stays dead-dangling"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");

    // The verb recovers the LANDED send as turn-anchored{recovered} — the false
    // permanent "failed" the old watch-interrupted terminal would have minted is gone.
    let kinds = events_in(&j.sessions_dir, "sid-wi");
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "a landed watch-interrupted send must recover to turn-anchored, not stay failed; got {kinds:?}; stdout: {stdout}"
    );
    assert!(
        !kinds.iter().any(|e| e == "pending-abandoned"),
        "a landed send must NOT be recorded as pending-abandoned/failed; got {kinds:?}"
    );
    assert!(
        stdout.contains("anchored 1"),
        "stdout should report anchored 1; got: {stdout}"
    );
}

// =========================================================================
// PROOF 7b (R5 rider 2, finding B) — a WATCH-INTERRUPTED send with NO candidate
// closes via the RECOVERY verdict pending-abandoned{recovery-no-candidate} (the
// disclosed "when not in the transcript" arm), never a permanent watch-interrupted
// door false-fail
// =========================================================================
#[test]
fn watch_interrupted_with_no_candidate_recovers_abandoned() {
    // The symmetric arm: a watch-interrupted send whose content is NOT found past
    // its offset (the turn truly never committed, or the transcript is gone) closes
    // via the RECOVERY path — pending-abandoned{recovery-no-candidate}, the honest
    // possibly-false-negative disclosure — not via a door-minted permanent
    // watch-interrupted terminal. The reason field discriminates the two: recovery-
    // no-candidate (verb ran, found nothing) vs the old watch-interrupted (the door
    // foreclosed before any recovery could run).
    let j = jail();
    let dead = known_dead_pid();
    let msg = "a --wait send whose turn never committed after the watch was interrupted";

    // Dead-dangling, NO terminal (door minted none), and a READABLE transcript that
    // holds no matching record (the turn truly never committed) → recovery finds no
    // candidate (the NoRecord case). NOTE (finding H): this formerly planted NO
    // transcript, which is actually the SourceUnavailable arm (undeterminable → no
    // terminal); a genuine "no candidate → abandoned" needs a READABLE transcript
    // that simply lacks the content.
    let transcript = write_transcript(
        j._root.path(),
        "wi_nc.jsonl",
        "an unrelated user turn that is not the interrupted send at all",
    );
    write_send_initiated(
        &j,
        "sid-wi-nc",
        "sid-wi-nc",
        "wi-nc-1",
        "send:pty",
        dead,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );

    assert!(
        terminals_in(&j.sessions_dir, "sid-wi-nc").is_empty(),
        "precondition: the door minted no terminal — dead-dangling, not failed"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");

    // Exactly one terminal, and it is the RECOVERY verdict — the reason string
    // discriminates it from the foreclosing door terminal this fix removed.
    let terms = terminals_in(&j.sessions_dir, "sid-wi-nc");
    assert_eq!(
        terms,
        vec!["pending-abandoned"],
        "the orphaned watch-interrupted send closes with exactly one recovery terminal; stdout: {stdout}"
    );
    let path = j.sessions_dir.join("sid-wi-nc.events.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let abandoned = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("event").and_then(|e| e.as_str()) == Some("pending-abandoned"))
        .expect("pending-abandoned present");
    assert_eq!(
        abandoned.get("reason").and_then(|r| r.as_str()),
        Some("recovery-no-candidate"),
        "reason must be the recovery verdict (verb ran), NOT a foreclosing watch-interrupted door terminal"
    );
    // R6 (c): the disclosed closer stamps recovered:true + attribution.
    assert_eq!(
        abandoned.get("recovered").and_then(|b| b.as_bool()),
        Some(true)
    );
    assert_eq!(
        abandoned.get("attribution").and_then(|a| a.as_str()),
        Some("offset")
    );
}

// =========================================================================
// FINDING G (amend rider 3) — the COMPLETE non-foreclosure class: the four sibling
// failure arms that fire on a possibly-or-provably-LANDED send now mint NO terminal,
// and the compiled verb resolves each from the transcript. One landed proof per arm;
// one no-candidate proof per verb family (send:pty via PROOF 8b, new-p via 11b).
//
// The recover verb keys on the send-initiated record (verb ∈ {send:pty, new-p}) +
// transcript, NOT on which arm produced the dead-dangling state — so each proof
// models the exact dead-dangling ledger its arm leaves (send-initiated, NO terminal,
// dead writer, old ts) and asserts the SAME two resolution modes B proved for the
// SourceError arm.
// =========================================================================

/// PROOF 8 (finding G, send.rs `WaitOutcome::Died` :889) — a --wait send whose
/// message LANDED (anchor found by run_wait_loop) then the session died is recovered,
/// not false-failed. The Died arm emits NO pending-abandoned{session-died}; the verb
/// resolves it to turn-anchored{recovered} from the transcript.
#[test]
fn wait_died_that_landed_is_recovered_not_false_failed() {
    let j = jail();
    let dead = known_dead_pid();
    let msg = "a --wait send whose message landed before the session died";
    let transcript = write_transcript(j._root.path(), "died.jsonl", msg);
    // Dead-dangling ledger the Died arm leaves: send-initiated (bytes acked, turn
    // submitted), NO terminal, dead writer, old ts.
    write_send_initiated(
        &j,
        "sid-died",
        "sid-died",
        "died-1",
        "send:pty",
        dead,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );
    assert!(
        terminals_in(&j.sessions_dir, "sid-died").is_empty(),
        "the Died arm must mint no terminal (finding G) — the send stays dead-dangling"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    let kinds = events_in(&j.sessions_dir, "sid-died");
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "a landed Died send must recover to turn-anchored, not stay failed; got {kinds:?}; stdout: {stdout}"
    );
    assert!(
        !kinds.iter().any(|e| e == "pending-abandoned"),
        "a landed send must NOT be recorded as pending-abandoned/failed; got {kinds:?}"
    );
    assert!(
        stdout.contains("anchored 1"),
        "stdout should report anchored 1; got: {stdout}"
    );
}

/// PROOF 8b (finding G, Died arm — the send:pty no-candidate family) — a Died send
/// whose content is NOT in the transcript closes via the RECOVERY verdict
/// pending-abandoned{recovery-no-candidate} (the disclosed possibly-false-negative),
/// never a permanent Died-arm door false-fail. reason discriminates: recovery-no-
/// candidate (verb ran, found nothing) vs the old session-died (door foreclosed).
#[test]
fn wait_died_with_no_candidate_recovers_abandoned() {
    let j = jail();
    let dead = known_dead_pid();
    let msg = "a --wait send whose turn never committed before the session died";
    // Dead-dangling, NO terminal, READABLE transcript with no matching record →
    // recovery finds no candidate (NoRecord). (Finding H: a plain absent transcript
    // is the SourceUnavailable arm — undeterminable — so the genuine no-candidate case
    // uses a readable-but-non-matching transcript.)
    let transcript = write_transcript(
        j._root.path(),
        "died_nc.jsonl",
        "an unrelated user turn that is not the sent message before the session died",
    );
    write_send_initiated(
        &j,
        "sid-died-nc",
        "sid-died-nc",
        "died-nc-1",
        "send:pty",
        dead,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );
    assert!(
        terminals_in(&j.sessions_dir, "sid-died-nc").is_empty(),
        "precondition: the door minted no terminal — dead-dangling, not failed"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    let terms = terminals_in(&j.sessions_dir, "sid-died-nc");
    assert_eq!(
        terms,
        vec!["pending-abandoned"],
        "the orphaned Died send closes with exactly one recovery terminal; stdout: {stdout}"
    );
    let path = j.sessions_dir.join("sid-died-nc.events.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let abandoned = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("event").and_then(|e| e.as_str()) == Some("pending-abandoned"))
        .expect("pending-abandoned present");
    assert_eq!(
        abandoned.get("reason").and_then(|r| r.as_str()),
        Some("recovery-no-candidate"),
        "reason must be the recovery verdict (verb ran), NOT a foreclosing session-died door terminal"
    );
    // R6 (c): the disclosed closer stamps recovered:true + attribution.
    assert_eq!(
        abandoned.get("recovered").and_then(|b| b.as_bool()),
        Some(true)
    );
    assert_eq!(
        abandoned.get("attribution").and_then(|a| a.as_str()),
        Some("offset")
    );
}

/// PROOF 9 (finding G, send.rs `WaitOutcome::TimedOut{anchored}` :903) — a --wait send
/// that timed out with `anchored:true` (the message PROVABLY landed, response merely
/// slow) is recovered, not false-failed. The TimedOut arm emits NO anchor-timeout; the
/// verb resolves it to turn-anchored{recovered}. (The no-candidate mode is the same
/// send:pty family proven in PROOF 8b.)
#[test]
fn wait_timedout_that_landed_is_recovered_not_false_failed() {
    let j = jail();
    let dead = known_dead_pid();
    let msg = "a --wait send that anchored then merely timed out waiting for the reply";
    let transcript = write_transcript(j._root.path(), "timedout.jsonl", msg);
    write_send_initiated(
        &j,
        "sid-to",
        "sid-to",
        "to-1",
        "send:pty",
        dead,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );
    assert!(
        terminals_in(&j.sessions_dir, "sid-to").is_empty(),
        "the TimedOut arm must mint no terminal (finding G) — the send stays dead-dangling"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    let kinds = events_in(&j.sessions_dir, "sid-to");
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "a landed TimedOut send must recover to turn-anchored, not stay failed; got {kinds:?}; stdout: {stdout}"
    );
    assert!(
        !kinds.iter().any(|e| e == "anchor-timeout" || e == "pending-abandoned"),
        "a landed send must NOT carry a foreclosing anchor-timeout/pending-abandoned; got {kinds:?}"
    );
    assert!(
        stdout.contains("anchored 1"),
        "stdout should report anchored 1; got: {stdout}"
    );
}

/// PROOF 10 (finding G, lifecycle.rs `DeliverOutcome::Stalled` :1111) — a PRIMING
/// (`new-p`) send whose deliver stalled (bytes written + `\r` submitted, the turn may
/// yet commit) is recovered, not false-failed. The Stalled arm emits NO anchor-timeout;
/// the verb (its sweep includes verb "new-p") resolves it to turn-anchored{recovered}.
/// This exercises the door-inventory CORRECTION: priming failure arms are recoverable,
/// not "already covered".
#[test]
fn priming_stalled_that_landed_is_recovered_not_false_failed() {
    let j = jail();
    let dead = known_dead_pid();
    let msg = "a -p priming prompt whose turn landed though the deliver watch stalled";
    let transcript = write_transcript(j._root.path(), "stalled.jsonl", msg);
    // The priming ledger: send-initiated verb "new-p", NO terminal, dead writer.
    write_send_initiated(
        &j,
        "sid-stall",
        "sid-stall",
        "stall-1",
        "new-p",
        dead,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );
    assert!(
        terminals_in(&j.sessions_dir, "sid-stall").is_empty(),
        "the Stalled arm must mint no terminal (finding G) — the priming send stays dead-dangling"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    let kinds = events_in(&j.sessions_dir, "sid-stall");
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "a landed priming Stalled send must recover to turn-anchored (new-p is swept); got {kinds:?}; stdout: {stdout}"
    );
    assert!(
        !kinds
            .iter()
            .any(|e| e == "anchor-timeout" || e == "pending-abandoned"),
        "a landed priming send must NOT carry a foreclosing terminal; got {kinds:?}"
    );
    assert!(
        stdout.contains("anchored 1"),
        "stdout should report anchored 1; got: {stdout}"
    );
}

/// PROOF 11 (finding G, lifecycle.rs `DeliverOutcome::PidFileMissing` :1122) — a
/// PRIMING (`new-p`) send whose pid file vanished AFTER send_message wrote the bytes
/// (deliver_prompt: send_message precedes the find_pid_file None) is recovered, not
/// false-failed. The PidFileMissing arm emits NO pending-abandoned{session-died}; the
/// verb resolves it to turn-anchored{recovered}. This is the priming door the
/// door-inventory wrongly called "already covered" (only the Accepted arm was).
#[test]
fn priming_pidfile_missing_that_landed_is_recovered_not_false_failed() {
    let j = jail();
    let dead = known_dead_pid();
    let msg = "a -p priming prompt whose bytes landed before the pid file vanished";
    let transcript = write_transcript(j._root.path(), "pidmiss.jsonl", msg);
    write_send_initiated(
        &j,
        "sid-pidm",
        "sid-pidm",
        "pidm-1",
        "new-p",
        dead,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );
    assert!(
        terminals_in(&j.sessions_dir, "sid-pidm").is_empty(),
        "the PidFileMissing arm must mint no terminal (finding G) — the priming send stays dead-dangling"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    let kinds = events_in(&j.sessions_dir, "sid-pidm");
    assert!(
        kinds.contains(&"turn-anchored".to_string()),
        "a landed priming PidFileMissing send must recover to turn-anchored; got {kinds:?}; stdout: {stdout}"
    );
    assert!(
        !kinds.iter().any(|e| e == "pending-abandoned"),
        "a landed priming send must NOT be recorded as pending-abandoned/failed; got {kinds:?}"
    );
    assert!(
        stdout.contains("anchored 1"),
        "stdout should report anchored 1; got: {stdout}"
    );
}

/// PROOF 11b (finding G, PidFileMissing arm — the new-p no-candidate family) — a
/// priming send whose content is NOT in the transcript closes via the RECOVERY verdict
/// pending-abandoned{recovery-no-candidate}, never a permanent PidFileMissing-arm door
/// false-fail. Proves the new-p family's no-candidate resolution mode end-to-end.
#[test]
fn priming_pidfile_missing_with_no_candidate_recovers_abandoned() {
    let j = jail();
    let dead = known_dead_pid();
    let msg = "a -p priming prompt whose turn never committed before the pid file vanished";
    // READABLE transcript with no matching record → recovery finds no candidate
    // (NoRecord). (Finding H: a plain absent transcript is the SourceUnavailable arm —
    // undeterminable — so the genuine no-candidate case uses a readable-non-matching
    // transcript.)
    let transcript = write_transcript(
        j._root.path(),
        "pidm_nc.jsonl",
        "an unrelated priming turn that is not the sent prompt before the pid file vanished",
    );
    write_send_initiated(
        &j,
        "sid-pidm-nc",
        "sid-pidm-nc",
        "pidm-nc-1",
        "new-p",
        dead,
        OLD_TS,
        None,
        msg,
        Some(&transcript),
    );
    assert!(
        terminals_in(&j.sessions_dir, "sid-pidm-nc").is_empty(),
        "precondition: the door minted no terminal — dead-dangling, not failed"
    );

    let (ok, stdout) = run_recover(&j, None);
    assert!(ok, "verb exits 0; stdout: {stdout}");
    let terms = terminals_in(&j.sessions_dir, "sid-pidm-nc");
    assert_eq!(
        terms,
        vec!["pending-abandoned"],
        "the orphaned priming send closes with exactly one recovery terminal; stdout: {stdout}"
    );
    let path = j.sessions_dir.join("sid-pidm-nc.events.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let abandoned = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("event").and_then(|e| e.as_str()) == Some("pending-abandoned"))
        .expect("pending-abandoned present");
    assert_eq!(
        abandoned.get("reason").and_then(|r| r.as_str()),
        Some("recovery-no-candidate"),
        "reason must be the recovery verdict (verb ran), NOT a foreclosing session-died door terminal"
    );
    // R6 (c): the disclosed closer stamps recovered:true + attribution.
    assert_eq!(
        abandoned.get("recovered").and_then(|b| b.as_bool()),
        Some(true)
    );
    assert_eq!(
        abandoned.get("attribution").and_then(|a| a.as_str()),
        Some("offset")
    );
}

// =========================================================================
// PROOF 5 (F2 regression) — concurrent recover verbs emit EXACTLY ONE terminal (C2)
// =========================================================================
#[test]
fn concurrent_recover_emits_exactly_one_terminal() {
    // Two `qd delivery:recover` runs launched as simultaneously as possible against ONE
    // dead-dangling send. Pre-fix (lock-free read-then-append) this produced TWO
    // pending-abandoned terminals for one send_id (red-team F2, 40/40). The flock over
    // the re-check→emit critical section must yield EXACTLY ONE. Looped to stress the
    // race window.
    for iter in 0..20 {
        let j = jail();
        let dead = known_dead_pid();
        // A READABLE transcript with no matching record → both concurrent runs take the
        // NoRecord → pending-abandoned path (a terminal IS minted), so the flock's
        // exactly-one guarantee is what's under test. (Finding H: an absent transcript
        // would be SourceUnavailable → NO terminal, which cannot exercise F2's
        // exactly-one-terminal race.)
        let transcript = write_transcript(
            j._root.path(),
            "conc.jsonl",
            "an unrelated user turn that is not the concurrently-recovered send",
        );
        write_send_initiated(
            &j,
            "sid-c",
            "sid-c",
            "conc-1",
            "send:pty",
            dead,
            OLD_TS,
            None,
            "recover me once, not twice",
            Some(&transcript),
        );
        // Launch BOTH children before waiting on either (maximize overlap).
        let c1 = spawn_recover(&j);
        let c2 = spawn_recover(&j);
        let o1 = c1.wait_with_output().expect("verb 1");
        let o2 = c2.wait_with_output().expect("verb 2");
        assert!(
            o1.status.success() && o2.status.success(),
            "both verbs exit 0 (iter {iter})"
        );
        let terms = terminals_in(&j.sessions_dir, "sid-c");
        assert_eq!(
            terms.len(),
            1,
            "iter {iter}: concurrent recover emitted {} terminals for one send_id (C2 exactly-one BREAK): {terms:?}",
            terms.len()
        );
        assert_eq!(terms[0], "pending-abandoned");
    }
}

/// Spawn `qd delivery:recover` WITHOUT waiting (for the concurrency race test).
fn spawn_recover(j: &Jail) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_qd"))
        .arg("delivery:recover")
        .env("HOME", &j.home)
        .env("QD_HOME", &j.qd_home)
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn qd delivery:recover")
}

/// Spawn-and-reap a child to obtain a known-DEAD pid (mirrors the events.rs unit
/// helper): a mid-send kill leaves the writer's pid dead.
fn known_dead_pid() -> u32 {
    let child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn reapable child");
    let pid = child.id();
    let mut child = child;
    let _ = child.wait();
    pid
}
