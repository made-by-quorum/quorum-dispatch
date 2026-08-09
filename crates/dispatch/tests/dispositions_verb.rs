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
//!   last_attempt_at, first_delivered_at, expires_at, authored_at, origin`
//!   (witness DROPPED, R14.2). The nullable fields emit as JSON `null` (STABLE
//!   columns for the DuckDB projection, never skipped); `last_event` is null
//!   exactly when no events exist (R11.1); `origin`/`authored_at`/`expires_at`
//!   come from the JOINED envelope only, null for an orphan-event summary (R14.2).
//! - `--events` (§3b): the RAW event rows verbatim (the funnel), in file/union
//!   order — normalized `{v, correlation_id, event, created_at, [class]}`; the
//!   machine `class` present ONLY on `delivery-failed`/`refused` (R14.2).
//!
//! Scope (`--host`), `--archive`, the point query, `--window` (the summary bounds
//! `authored_at`; `--events` bounds `created_at` — R14.2 split), and the
//! broken-pipe (exit 141) contract are exercised here.

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

/// The 10 summary-record keys, in the documented §3a wire order (witness DROPPED,
/// R14.2). Every summary row must carry ALL of them — the nullable ones as JSON
/// `null`, never skipped (stable columns for the DuckDB projection).
const SUMMARY_KEYS: [&str; 10] = [
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
            // R14.2: origin comes from the JOINED envelope only — a stable column,
            // string when the envelope is in scope, JSON null for an orphan-event
            // summary. Presence (never skipped) is asserted by the SUMMARY_KEYS loop.
            assert!(
                v["origin"].is_string() || v["origin"].is_null(),
                "origin is a stable column (string or null) ({l})"
            );
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
            // R14.2 normalized event: created_at is the only common timestamp; there
            // is NO witnessed_at/witness/origin/authored_at on an event row.
            assert!(v["created_at"].is_i64(), "created_at present ({l})");
            assert!(v.get("witness").is_none(), "no witness on a normalized event ({l})");
            assert!(v.get("origin").is_none(), "no origin on a normalized event ({l})");
            assert!(v.get("authored_at").is_none(), "no authored_at on a normalized event ({l})");
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

/// A normalized EVENT row (format doc §2 key order, R14.2/R15). `kind` ∈
/// attempted|queued|delivered. attempted/queued carry no tail; `delivered`
/// carries a REQUIRED `body_digest` tail (R15). `authored` is accepted for
/// call-site compatibility but is NOT a field on a normalized event row.
fn ev_row(id: &str, kind: &str, created_at: i64, _authored: i64) -> String {
    if kind == "delivered" {
        // R15: delivered rows carry a body_digest (a fixed test token here).
        format!(
            r#"{{"v":1,"correlation_id":"{id}","event":"delivered","created_at":{created_at},"body_digest":"seeddigest"}}"#
        )
    } else {
        format!(
            r#"{{"v":1,"correlation_id":"{id}","event":"{kind}","created_at":{created_at}}}"#
        )
    }
}

/// A `delivery-failed` EVENT row — one of the two variants that carry the
/// required machine `class` (last on the wire, format doc §2 / R14.2).
fn ev_failed_row(id: &str, created_at: i64, _authored: i64, class: &str) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","event":"delivery-failed","created_at":{created_at},"class":"{class}"}}"#
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
    // R14.2: origin/authored_at come from the JOINED envelope (present here).
    assert_eq!(d["origin"], "brano", "origin from the joined envelope");
    assert_eq!(d["authored_at"], authored);

    // R11.1: no events ⇒ last_event null, and the other nullable analytics fields
    // are JSON null (stable columns). origin/authored_at still come from the
    // envelope (in scope).
    let p = by_id("PEND");
    assert_eq!(p["state"], "pending");
    assert_eq!(p["last_event"], serde_json::Value::Null);
    assert_eq!(p["origin"], "brano", "origin from the envelope even with no events");
    assert_eq!(p["last_attempt_at"], serde_json::Value::Null);
    assert_eq!(p["first_delivered_at"], serde_json::Value::Null);
    assert_eq!(p["attempts"], 0);

    let x = by_id("EXPIR");
    assert_eq!(x["state"], "expired", "no delivered event past expires_at");
    assert_eq!(x["last_event"], serde_json::Value::Null);
    assert_eq!(x["origin"], "brano", "origin from the envelope");
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

    // --events mode: the 5 raw rows verbatim, FILE ORDER, the machine `class` ONLY
    // on the delivery-failed row (omitted from the wire on the plain variants).
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
            assert_eq!(e["class"], "delivery", "class REQUIRED on delivery-failed (R14.2)");
        } else {
            assert!(
                e.get("class").is_none(),
                "class FORBIDDEN (key omitted) on the plain variant row {i}: {e}"
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
    // R14.2: origin comes from the peer's joined envelope (unioned in), not the event.
    assert_eq!(peer_rec["origin"], "brano", "origin from the peer's joined envelope");

    let (code, stdout, _) = run_dispositions(&home, &["--host", "peerbox", "--events"]);
    assert_eq!(code, 0);
    let events = parse_events(&stdout);
    assert_eq!(events.len(), 1, "--events unions the peer's raw rows ({stdout})");
    assert_eq!(events[0]["correlation_id"], "PEER");
}

/// R14a pin 2 at the INTEGRATION level: `--all` unions every `remote/<host>/`
/// replica in SORTED-HOST order, so the projection is INVARIANT under any
/// filesystem/directory-enumeration order (event rows no longer carry a
/// source/witness column, so cross-source determinism is the reader's job).
///
/// TWO peer hosts each hold ONE event for the SAME id "SHARED" at the SAME
/// created_at (the discriminating tie). hostA says delivery-failed, hostB says
/// attempted. Because the union is sorted-host (hostA before hostB), hostB's
/// `attempted` is always later-in-input and always wins the last_event tie →
/// last_event=attempted, deterministically. The `--all` summary AND `--events`
/// output are byte-identical across repeated runs (no dependence on scan order).
#[test]
fn all_scope_cross_source_projection_is_order_invariant() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    let authored = 1_700_000_000_000i64;
    let far_future = 8_000_000_000_000i64;
    let t = 1_700_000_000_500i64; // the SAME created_at on both hosts (the tie)

    // A local envelope for SHARED so origin/authored_at are populated (the join),
    // plus the two peer replicas each carrying one event at the same created_at.
    write_lines(&root.join("log.jsonl"), &[&log_row("SHARED", authored, far_future)]);
    let host_a = root.join("remote").join("hostA");
    let host_b = root.join("remote").join("hostB");
    write_lines(&host_a.join("dispositions.jsonl"), &[&ev_failed_row("SHARED", t, authored, "wake")]);
    write_lines(&host_b.join("dispositions.jsonl"), &[&ev_row("SHARED", "attempted", t, authored)]);

    // First read establishes the reference output; repeat many times — the
    // sorted-host union makes both the summary and the raw funnel byte-stable.
    let (_c, ref_summary, _) = run_dispositions(&home, &["--all"]);
    let (_c, ref_events, _) = run_dispositions(&home, &["--all", "--events"]);
    for _ in 0..10 {
        let (code, summary, stderr) = run_dispositions(&home, &["--all"]);
        assert_eq!(code, 0, "--all summary exit 0 (stderr: {stderr})");
        assert_eq!(summary, ref_summary, "--all summary is order-invariant across reads");
        let (code, events, _) = run_dispositions(&home, &["--all", "--events"]);
        assert_eq!(code, 0);
        assert_eq!(events, ref_events, "--all --events is order-invariant across reads");
    }

    // The tie resolves to the SORTED-LAST host's event (hostB's attempted),
    // proving the union order is sorted-host, not scan order.
    let recs = parse_records(&ref_summary);
    let s = recs.iter().find(|r| r["correlation_id"] == "SHARED").unwrap();
    assert_eq!(
        s["last_event"], "attempted",
        "the sorted-last host wins the equal-created_at tie, deterministically ({ref_summary})"
    );
    assert_eq!(s["state"], "pending", "attempted latest, no delivery ⇒ pending");
    // Both hosts' raw rows are present, hostA (delivery-failed) before hostB
    // (attempted) — the sorted-host concatenation the projection folds over.
    let evs = parse_events(&ref_events);
    let kinds: Vec<&str> = evs.iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["delivery-failed", "attempted"],
        "sorted-host order: hostA's row before hostB's ({ref_events})"
    );
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
fn window_filters_summary_by_authored_at_and_events_by_created_at() {
    // R14.2 SPLIT: the summary windows on the envelope's `authored_at`; `--events`
    // windows on each event's `created_at` (events no longer copy authored_at).
    // Here each event's created_at (authored + 100ms) is on the same side of the
    // window as its envelope's authored_at, so both modes keep exactly the recent
    // id — but they are now measured on DIFFERENT timestamps.
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

    // In --events mode the window bounds each row's `created_at` (R14.2 split).
    let (code, stdout, stderr) = run_dispositions(&home, &["--window", "3650d", "--events"]);
    assert_eq!(code, 0, "windowed events exit 0 (stderr: {stderr})");
    let events = parse_events(&stdout);
    assert_eq!(events.len(), 1, "events window (on created_at) keeps only the recent row ({stdout})");
    assert_eq!(events[0]["correlation_id"], "NEW");

    // No window ⇒ everything in scope, both modes.
    let (_, stdout, _) = run_dispositions(&home, &[]);
    assert_eq!(parse_records(&stdout).len(), 2, "no window ⇒ all summaries");
    let (_, stdout, _) = run_dispositions(&home, &["--events"]);
    assert_eq!(parse_events(&stdout).len(), 2, "no window ⇒ all event rows");
}

/// R14.2 honest-null + the window's orphan carve-out: an ORPHAN-event summary
/// (an event whose envelope is NOT in scope) has `origin`/`authored_at`/
/// `expires_at` ALL null — and because its timeline is absent, a `--window` can
/// never position it, so the summary is ALWAYS KEPT (never silently dropped by a
/// bound it cannot be measured against).
#[test]
fn orphan_event_summary_is_triple_null_and_survives_any_window() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    // A delivered event with NO envelope in scope (no log.jsonl row for it).
    write_lines(
        &root.join("dispositions.jsonl"),
        &[&ev_row("ORPH", "delivered", 1_700_000_000_500, 1_700_000_000_000)],
    );

    // The summary derives `delivered` from the event alone, with the three
    // envelope-sourced columns honestly null (R14.2).
    let (code, stdout, stderr) = run_dispositions(&home, &["ORPH"]);
    assert_eq!(code, 0, "orphan summary exit 0 (stderr: {stderr})");
    let recs = parse_records(&stdout);
    assert_eq!(recs.len(), 1, "one orphan summary ({stdout})");
    let o = &recs[0];
    assert_eq!(o["state"], "delivered", "delivered event exists ⇒ delivered");
    assert_eq!(o["origin"], serde_json::Value::Null, "no envelope ⇒ origin null");
    assert_eq!(o["authored_at"], serde_json::Value::Null, "no envelope ⇒ authored_at null");
    assert_eq!(o["expires_at"], serde_json::Value::Null, "no envelope ⇒ expires_at null");

    // A tight window that would exclude any ancient timeline still KEEPS the
    // orphan summary — an absent authored_at can never fall outside the bound.
    let (code, stdout, stderr) = run_dispositions(&home, &["ORPH", "--window", "1s"]);
    assert_eq!(code, 0, "windowed orphan summary exit 0 (stderr: {stderr})");
    let recs = parse_records(&stdout);
    assert_eq!(
        recs.len(),
        1,
        "a null-timeline orphan summary is never dropped by a window ({stdout})"
    );
    assert_eq!(recs[0]["correlation_id"], "ORPH");

    // But the SAME tight window DOES drop the orphan's event in --events mode:
    // the event carries a concrete created_at (year 2023), far outside a 1s window.
    let (code, stdout, stderr) = run_dispositions(&home, &["ORPH", "--events", "--window", "1s"]);
    assert_eq!(code, 0, "windowed orphan events exit 0 (stderr: {stderr})");
    assert!(
        parse_events(&stdout).is_empty(),
        "the orphan's event (old created_at) is outside a 1s window ({stdout})"
    );
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

/// audit #4 (end-to-end): `--host` values that would traverse out of the store
/// root or read an absolute dir are refused by the REAL binary — `refused{host}`,
/// exit 12 (the shared refusal code) — before any `remote/<host>/` path is built.
/// Nothing is read from outside the store.
#[test]
fn host_path_traversal_is_refused_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    // Seed a valid local record so a bypass would otherwise have something to emit.
    let root = dispatch_root(&home);
    write_lines(&root.join("log.jsonl"), &[&log_row("LOCAL", 1_700_000_000_000, 8_000_000_000_000)]);

    for bad in ["../../etc", "/etc/passwd", "..", "foo/bar", "/"] {
        let (code, stdout, err) = run_dispositions(&home, &["--host", bad]);
        assert_eq!(code, 12, "--host {bad:?} → refused exit 12 (stderr: {err})");
        assert!(
            err.starts_with("qd send: refused{host}:") && err.contains("invalid --host"),
            "--host {bad:?} → refused{{host}}, got: {err}"
        );
        assert!(stdout.trim().is_empty(), "--host {bad:?} emits nothing (no traversal read): {stdout:?}");
    }
    // Control: a legit host is exit 0 — NOT a refusal. `--host` is local UNION the
    // peer, so the absent peerbox contributes nothing but the local record still
    // shows (proving a valid host reads normally, unlike the refused traversals).
    let (code, stdout, err) = run_dispositions(&home, &["--host", "peerbox"]);
    assert_eq!(code, 0, "a valid host is exit 0 (stderr: {err})");
    let ids: Vec<String> = parse_records(&stdout)
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["LOCAL"], "valid host reads normally (local record present, absent peer empty)");
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
