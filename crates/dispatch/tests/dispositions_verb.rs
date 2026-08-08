//! Integration tests for the `qd dispositions` READ verb (qd–qf transition W5,
//! reworked to the R8 disposition-event-log model), driving the REAL `qd`
//! binary against a JAILED, empty HOME (L9a — never the real home; HOME points
//! into a per-test tempdir).
//!
//! The verb is a thin CLI over `dispatch::dispositions::{query_summary,
//! read_events}` (unit-tested in that module); these pin the bin wiring
//! end-to-end. Two output modes, both JSONL on stdout (format doc §3):
//!
//! - DEFAULT (§3a): one per-id SUMMARY row folded over `log.jsonl` ∪
//!   `dispositions.jsonl` — `v, correlation_id, state, attempts, last_event,
//!   last_attempt_at, first_delivered_at, expires_at, authored_at, origin,
//!   witness`. The nullable fields emit as JSON `null` (STABLE columns for the
//!   DuckDB projection, never skipped); `{last_event, witness}` are null
//!   together, exactly when no events exist (R11.1 paired-null).
//! - `--events` (§3b): the RAW witnessed-event rows verbatim (the funnel), in
//!   file/union order — `reason` present ONLY on `delivery-failed`.
//!
//! Scope (`--host`), `--archive`, the point query, `--window` (bounds
//! `authored_at` in BOTH modes), and the broken-pipe (exit 141) contract are
//! exercised here.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// The qd data root under a jailed HOME: `<home>/.quorum/dispatch` (QD_HOME
/// unset). Transport files (`log.jsonl`, `dispositions.jsonl`, `remote/<h>/…`)
/// live DIRECTLY under it (format doc: not under `state/`).
fn dispatch_root(home: &Path) -> PathBuf {
    home.join(".quorum").join("dispatch")
}

/// Prepare a jailed HOME dir under `dir` and return it. The dispatch root is
/// created so tests can drop transport files straight in.
fn jail_home(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    std::fs::create_dir_all(dispatch_root(&home)).unwrap();
    // Never the real home (the L9a guard the other verb suites assert).
    let real = std::env::var("HOME").unwrap_or_default();
    assert_ne!(
        home.to_string_lossy(),
        real,
        "jailed HOME must not be the real HOME"
    );
    home
}

/// Write raw JSONL lines (already-serialized wire rows) to a file.
fn write_lines(path: &Path, lines: &[&str]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(path, body).unwrap();
}

/// Run `qd dispositions <args...>` with HOME jailed into `home`, QD_HOME unset
/// (so the data root is `<home>/.quorum/dispatch`). Returns (exit, stdout,
/// stderr).
fn run_dispositions(home: &Path, args: &[&str]) -> (i32, String, String) {
    let mut full = vec!["dispositions"];
    full.extend_from_slice(args);
    let out = Command::new(qd_bin())
        .args(&full)
        .env("HOME", home)
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The 11 summary-record keys, in the documented §3a wire order. Every summary
/// row must carry ALL of them — the nullable ones as JSON `null`, never skipped
/// (stable columns for the DuckDB projection).
const SUMMARY_KEYS: [&str; 11] = [
    "v",
    "correlation_id",
    "state",
    "attempts",
    "last_event",
    "last_attempt_at",
    "first_delivered_at",
    "expires_at",
    "authored_at",
    "origin",
    "witness",
];

/// Parse every non-empty stdout line as a §3a SUMMARY record, asserting each
/// carries the full stable-column key set, and return them.
fn parse_records(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("line is not JSON: {l:?} ({e})"));
            assert_eq!(v["v"], 1, "every row carries v:1 ({l})");
            assert!(v["correlation_id"].is_string(), "correlation_id present ({l})");
            assert!(v["state"].is_string(), "state present ({l})");
            assert!(v["origin"].is_string(), "origin present (REQUIRED, R11) ({l})");
            for key in SUMMARY_KEYS {
                assert!(
                    v.get(key).is_some(),
                    "summary stable column {key:?} present (as null when absent) ({l})"
                );
            }
            v
        })
        .collect()
}

/// Parse every non-empty stdout line as a §3b raw EVENT row, asserting the §2
/// key set, and return them.
fn parse_events(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("line is not JSON: {l:?} ({e})"));
            assert_eq!(v["v"], 1, "every event row carries v:1 ({l})");
            assert!(v["correlation_id"].is_string(), "correlation_id present ({l})");
            assert!(v["event"].is_string(), "event present ({l})");
            assert!(v["witnessed_at"].is_i64(), "witnessed_at present ({l})");
            assert!(v["witness"].is_string(), "witness present ({l})");
            assert!(v["origin"].is_string(), "origin present ({l})");
            assert!(v["authored_at"].is_i64(), "authored_at present ({l})");
            v
        })
        .collect()
}

// --- fixtures: raw JSONL rows in the documented wire shape --------------------

/// An origin log envelope row (format doc §1 key order:
/// `v, correlation_id, authored_at, expires_at, target, origin, body`).
fn log_row(id: &str, authored: i64, expires: i64) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","authored_at":{authored},"expires_at":{expires},"target":"alpha@brano","origin":"brano","body":"hi"}}"#
    )
}

/// A reason-less witnessed EVENT row (format doc §2 key order:
/// `v, correlation_id, event, witnessed_at, witness, origin, authored_at`).
/// `kind` ∈ accepted|attempted|queued|delivered.
fn ev_row(id: &str, kind: &str, witnessed: i64, authored: i64) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","event":"{kind}","witnessed_at":{witnessed},"witness":"brano","origin":"brano","authored_at":{authored}}}"#
    )
}

/// A `delivery-failed` EVENT row — the ONE type that carries `reason` (last on
/// the wire, format doc §2).
fn ev_failed_row(id: &str, witnessed: i64, authored: i64, reason: &str) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","event":"delivery-failed","witnessed_at":{witnessed},"witness":"brano","origin":"brano","authored_at":{authored},"reason":"{reason}"}}"#
    )
}

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn all_local_projects_derived_states() {
    // Three envelopes: one delivered (a delivered EVENT exists), one pending
    // (no events, far-future expiry), one expired (no events, past expiry).
    // now is real wall-clock; pick expiries so the derived state is unambiguous
    // regardless of when the test runs.
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    let far_future = 8_000_000_000_000i64; // ~year 2223 — always > now
    let long_ago = 1_000i64; // ~1970 — always <= now
    let authored = 1_700_000_000_000i64;
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("DELIV", authored, far_future),
            &log_row("PEND", authored, far_future),
            &log_row("EXPIR", authored, long_ago),
        ],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row("DELIV", "attempted", 1_700_000_000_400, authored),
            &ev_row("DELIV", "delivered", 1_700_000_000_500, authored),
        ],
    );

    let (code, stdout, stderr) = run_dispositions(&home, &[]);
    assert_eq!(code, 0, "all-local exit 0 (stderr: {stderr})");
    let recs = parse_records(&stdout);
    assert_eq!(recs.len(), 3, "one summary row per correlation ({stdout})");

    let by_id = |id: &str| recs.iter().find(|r| r["correlation_id"] == id).cloned().unwrap();
    let d = by_id("DELIV");
    assert_eq!(d["state"], "delivered");
    assert_eq!(d["attempts"], 1);
    assert_eq!(d["last_event"], "delivered");
    assert_eq!(d["last_attempt_at"], 1_700_000_000_400i64);
    assert_eq!(d["first_delivered_at"], 1_700_000_000_500i64);
    assert_eq!(d["witness"], "brano");

    // R11.1 paired-null: no events ⇒ last_event AND witness null TOGETHER, and
    // the other nullable analytics fields are JSON null (stable columns).
    let p = by_id("PEND");
    assert_eq!(p["state"], "pending");
    assert_eq!(p["last_event"], serde_json::Value::Null);
    assert_eq!(p["witness"], serde_json::Value::Null);
    assert_eq!(p["last_attempt_at"], serde_json::Value::Null);
    assert_eq!(p["first_delivered_at"], serde_json::Value::Null);
    assert_eq!(p["attempts"], 0);

    let x = by_id("EXPIR");
    assert_eq!(x["state"], "expired", "no delivered event past expires_at");
    assert_eq!(x["last_event"], serde_json::Value::Null);
    assert_eq!(x["witness"], serde_json::Value::Null);
}

/// THE R8 read-surface pair: a full seeded funnel folds (DEFAULT) to ONE
/// summary row `state=delivered, attempts=2` — a `delivery-failed` row present
/// does NOT resolve the id failed-forever — while `--events` replays the SAME
/// five raw rows verbatim, in file order, `reason` only on the delivery-failed
/// row.
#[test]
fn funnel_folds_to_delivered_summary_and_events_mode_replays_raw_rows() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    let far_future = 8_000_000_000_000i64;
    let authored = 1_700_000_000_000i64;
    write_lines(&root.join("log.jsonl"), &[&log_row("FNL", authored, far_future)]);
    // The funnel: attempt 1 fails, attempt 2 queues then lands.
    write_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row("FNL", "attempted", 1_700_000_000_100, authored),
            &ev_failed_row("FNL", 1_700_000_000_150, authored, "delivery"),
            &ev_row("FNL", "attempted", 1_700_000_000_200, authored),
            &ev_row("FNL", "queued", 1_700_000_000_250, authored),
            &ev_row("FNL", "delivered", 1_700_000_000_300, authored),
        ],
    );

    // DEFAULT mode: ONE folded summary row — delivered absorbs the earlier
    // failure ("first terminal wins" is DEAD).
    let (code, stdout, stderr) = run_dispositions(&home, &[]);
    assert_eq!(code, 0, "summary exit 0 (stderr: {stderr})");
    let recs = parse_records(&stdout);
    assert_eq!(recs.len(), 1, "ONE summary row for the whole funnel ({stdout})");
    assert_eq!(recs[0]["correlation_id"], "FNL");
    assert_eq!(recs[0]["state"], "delivered", "delivered event exists ⇒ delivered");
    assert_eq!(recs[0]["attempts"], 2, "two attempted events across the retry");
    assert_eq!(recs[0]["last_event"], "delivered");
    assert_eq!(recs[0]["last_attempt_at"], 1_700_000_000_200i64);
    assert_eq!(recs[0]["first_delivered_at"], 1_700_000_000_300i64);

    // --events mode: the 5 raw rows verbatim, FILE ORDER, reason ONLY on the
    // delivery-failed row (omitted from the wire everywhere else).
    let (code, stdout, stderr) = run_dispositions(&home, &["--events"]);
    assert_eq!(code, 0, "--events exit 0 (stderr: {stderr})");
    let events = parse_events(&stdout);
    let kinds: Vec<&str> = events.iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["attempted", "delivery-failed", "attempted", "queued", "delivered"],
        "raw funnel rows in file order ({stdout})"
    );
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e["correlation_id"], "FNL");
        if e["event"] == "delivery-failed" {
            assert_eq!(e["reason"], "delivery", "reason REQUIRED on delivery-failed");
        } else {
            assert!(
                e.get("reason").is_none(),
                "reason FORBIDDEN (key omitted) on row {i}: {e}"
            );
        }
    }
}

#[test]
fn point_query_returns_just_that_record_in_both_modes() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    let far_future = 8_000_000_000_000i64;
    let authored = 1_700_000_000_000i64;
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("WANT", authored, far_future),
            &log_row("OTHER", authored, far_future),
        ],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row("WANT", "attempted", 1_700_000_000_100, authored),
            &ev_row("OTHER", "attempted", 1_700_000_000_200, authored),
        ],
    );

    let (code, stdout, stderr) = run_dispositions(&home, &["WANT"]);
    assert_eq!(code, 0, "point query exit 0 (stderr: {stderr})");
    let recs = parse_records(&stdout);
    assert_eq!(recs.len(), 1, "exactly the one queried summary ({stdout})");
    assert_eq!(recs[0]["correlation_id"], "WANT");
    assert_eq!(recs[0]["state"], "pending");
    assert_eq!(recs[0]["last_event"], "attempted");

    // --events point query: only WANT's rows, still raw.
    let (code, stdout, _) = run_dispositions(&home, &["WANT", "--events"]);
    assert_eq!(code, 0);
    let events = parse_events(&stdout);
    assert_eq!(events.len(), 1, "only the queried id's event rows ({stdout})");
    assert_eq!(events[0]["correlation_id"], "WANT");

    // A miss is empty output + exit 0 (not an error) — both modes.
    let (code, stdout, _) = run_dispositions(&home, &["NOPE"]);
    assert_eq!(code, 0);
    assert!(stdout.trim().is_empty(), "a summary miss emits nothing ({stdout:?})");
    let (code, stdout, _) = run_dispositions(&home, &["NOPE", "--events"]);
    assert_eq!(code, 0);
    assert!(stdout.trim().is_empty(), "an events miss emits nothing ({stdout:?})");
}

#[test]
fn host_flag_unions_the_peer_replica() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    let far_future = 8_000_000_000_000i64;
    let authored = 1_700_000_000_000i64;

    // Local has one envelope.
    write_lines(&root.join("log.jsonl"), &[&log_row("LOCAL", authored, far_future)]);
    // A peer replica under remote/peerbox/ carries another, with a delivered EVENT.
    let peer = root.join("remote").join("peerbox");
    write_lines(&peer.join("log.jsonl"), &[&log_row("PEER", authored, far_future)]);
    write_lines(
        &peer.join("dispositions.jsonl"),
        &[&ev_row("PEER", "delivered", 1_700_000_000_900, authored)],
    );

    // Local scope: peer NOT included.
    let (code, stdout, _) = run_dispositions(&home, &[]);
    assert_eq!(code, 0);
    let local_ids: Vec<String> = parse_records(&stdout)
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(local_ids, vec!["LOCAL"], "Local scope excludes the peer");

    // --host peerbox: local UNION the peer (both modes).
    let (code, stdout, stderr) = run_dispositions(&home, &["--host", "peerbox"]);
    assert_eq!(code, 0, "--host exit 0 (stderr: {stderr})");
    let recs = parse_records(&stdout);
    let mut ids: Vec<String> = recs
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["LOCAL", "PEER"], "--host unions in the peer");
    let peer_rec = recs.iter().find(|r| r["correlation_id"] == "PEER").unwrap();
    assert_eq!(peer_rec["state"], "delivered", "peer's delivered event projected");
    assert_eq!(peer_rec["witness"], "brano", "witness carried from the peer's event");

    let (code, stdout, _) = run_dispositions(&home, &["--host", "peerbox", "--events"]);
    assert_eq!(code, 0);
    let events = parse_events(&stdout);
    assert_eq!(events.len(), 1, "--events unions the peer's raw rows ({stdout})");
    assert_eq!(events[0]["correlation_id"], "PEER");
}

#[test]
fn archive_flag_unions_the_local_archive_tier() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    let far_future = 8_000_000_000_000i64;
    let authored = 1_700_000_000_000i64;

    write_lines(&root.join("log.jsonl"), &[&log_row("HOT", authored, far_future)]);
    write_lines(&root.join("log.archive.jsonl"), &[&log_row("ARCH", authored, far_future)]);
    write_lines(
        &root.join("dispositions.archive.jsonl"),
        &[&ev_row("ARCH", "delivered", 1_700_000_000_700, authored)],
    );

    // Without --archive: the archive tier is NOT read.
    let (code, stdout, _) = run_dispositions(&home, &[]);
    assert_eq!(code, 0);
    let ids: Vec<String> = parse_records(&stdout)
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["HOT"], "archive tier excluded by default");

    // With --archive: unioned in (summary + events).
    let (code, stdout, stderr) = run_dispositions(&home, &["--archive"]);
    assert_eq!(code, 0, "--archive exit 0 (stderr: {stderr})");
    let recs = parse_records(&stdout);
    let mut ids: Vec<String> = recs
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["ARCH", "HOT"], "--archive unions the archive tier");
    let arch = recs.iter().find(|r| r["correlation_id"] == "ARCH").unwrap();
    assert_eq!(arch["state"], "delivered");

    let (code, stdout, _) = run_dispositions(&home, &["--archive", "--events"]);
    assert_eq!(code, 0);
    let events = parse_events(&stdout);
    assert_eq!(events.len(), 1, "archived event row surfaces under --events");
    assert_eq!(events[0]["correlation_id"], "ARCH");
}

#[test]
fn window_filters_by_authored_at_in_both_modes() {
    // Two envelopes with very different authored_at (each with one event row
    // carrying the SAME authored_at — the origin timeline copied onto every
    // event, §2); --window keeps only the recent one in BOTH modes.
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    let far_future = 8_000_000_000_000i64;

    // NEW authored ~2025-10 (within ~10y of a 2026+ now); OLD authored at epoch
    // 1970 (outside any 3650d window).
    let new_authored = 1_760_000_000_000i64;
    let old_authored = 1_000i64;
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("NEW", new_authored, far_future),
            &log_row("OLD", old_authored, far_future),
        ],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row("NEW", "attempted", new_authored + 100, new_authored),
            &ev_row("OLD", "attempted", old_authored + 100, old_authored),
        ],
    );

    // 3650d window (~10y): covers a 2025 authored_at from a 2026 now, excludes 1970.
    let (code, stdout, stderr) = run_dispositions(&home, &["--window", "3650d"]);
    assert_eq!(code, 0, "windowed summary exit 0 (stderr: {stderr})");
    let ids: Vec<String> = parse_records(&stdout)
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["NEW"], "summary window keeps recent, drops ancient ({stdout})");

    // The SAME --window rule bounds authored_at in --events mode.
    let (code, stdout, stderr) = run_dispositions(&home, &["--window", "3650d", "--events"]);
    assert_eq!(code, 0, "windowed events exit 0 (stderr: {stderr})");
    let events = parse_events(&stdout);
    assert_eq!(events.len(), 1, "events window keeps only the recent row ({stdout})");
    assert_eq!(events[0]["correlation_id"], "NEW");

    // No window ⇒ everything in scope, both modes.
    let (_, stdout, _) = run_dispositions(&home, &[]);
    assert_eq!(parse_records(&stdout).len(), 2, "no window ⇒ all summaries");
    let (_, stdout, _) = run_dispositions(&home, &["--events"]);
    assert_eq!(parse_events(&stdout).len(), 2, "no window ⇒ all event rows");
}

#[test]
fn bad_window_is_a_sync_refusal_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    // Empty store is fine; the refusal fires before/independent of any read.
    for bad in ["1.5h", "12x", "abc", "12h30m"] {
        let (code, _out, err) = run_dispositions(&home, &["--window", bad]);
        assert_eq!(code, 12, "malformed --window {bad:?} → exit 12 (stderr: {err})");
        assert!(
            err.contains("refused{window}"),
            "expected refused{{window}} for {bad:?}, got: {err}"
        );
    }
}

#[test]
fn host_and_all_conflict_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    // clap conflicts_with → the centralized commander mapping (exit 1).
    let (code, _out, err) = run_dispositions(&home, &["--host", "h", "--all"]);
    assert_ne!(code, 0, "--host + --all must not succeed (stderr: {err})");
}

#[test]
fn empty_store_is_empty_output_exit_0() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    // Born-empty transport (no files at all) → zero records, exit 0, both modes.
    let (code, stdout, stderr) = run_dispositions(&home, &[]);
    assert_eq!(code, 0, "empty store exit 0 (stderr: {stderr})");
    assert!(stdout.trim().is_empty(), "no summary records ({stdout:?})");
    let (code, stdout, stderr) = run_dispositions(&home, &["--events"]);
    assert_eq!(code, 0, "empty store --events exit 0 (stderr: {stderr})");
    assert!(stdout.trim().is_empty(), "no event rows ({stdout:?})");
}

#[test]
fn broken_pipe_does_not_panic() {
    // `qd dispositions | head -0`: head reads nothing and closes the pipe
    // immediately, so qd's stdout write gets EPIPE. The verb must exit cleanly
    // (141 on SIGPIPE, or 0 if it finished writing the small payload before the
    // reader closed) — NEVER 101 (a Rust panic + backtrace). Run the pipeline
    // under bash so `PIPESTATUS[0]` gives qd's own exit code.
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    // Many rows so a write is plausibly in-flight when head closes the pipe.
    let far_future = 8_000_000_000_000i64;
    let rows: Vec<String> = (0..2000)
        .map(|i| log_row(&format!("ID{i:05}"), 1_700_000_000_000, far_future))
        .collect();
    let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    write_lines(&root.join("log.jsonl"), &refs);

    let piped = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "'{}' dispositions | head -0 >/dev/null; echo qd_exit=${{PIPESTATUS[0]}}",
            qd_bin()
        ))
        .env("HOME", &home)
        .env_remove("QD_HOME")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn bash pipeline");
    let stdout = String::from_utf8_lossy(&piped.stdout);
    let stderr = String::from_utf8_lossy(&piped.stderr);

    // Never a panic (exit 101 + backtrace) on either stream.
    assert!(
        !stderr.contains("panicked") && !stdout.contains("panicked"),
        "qd must not panic on a broken pipe (stdout: {stdout}, stderr: {stderr})"
    );
    // qd's own exit code (via PIPESTATUS): 141 (SIGPIPE) or 0 (finished first).
    let qd_exit: Option<i32> = stdout
        .lines()
        .find_map(|l| l.strip_prefix("qd_exit="))
        .and_then(|n| n.trim().parse().ok());
    assert!(
        matches!(qd_exit, Some(141) | Some(0)),
        "qd exit on broken pipe must be 141 or 0, got {qd_exit:?} (stdout: {stdout}, stderr: {stderr})"
    );
}
