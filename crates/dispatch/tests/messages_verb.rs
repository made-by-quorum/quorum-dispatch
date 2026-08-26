//! Integration tests for the `qd messages <session>` READ verb, driving the
//! REAL `qd` binary against a JAILED, empty HOME (L9a — never the real home;
//! HOME points into a per-test tempdir).
//!
//! The verb is the per-SESSION half of the same store `qd dispositions` reads by
//! correlation_id: the envelope ⟕ summary join
//! (`dispatch::dispositions::query_joined`), filtered to the rows with that
//! session on EITHER end — `target` (received) or `sender` (sent) — and sorted by
//! `authored_at`. These tests pin the bin wiring end-to-end: both end filters and
//! the `direction` they produce, the four derived states, the ordering, the
//! scope/window flags, and both human surfaces.
//!
//! Two environment facts shape every test here:
//!
//! - **The surface auto-detects its driver.** An agent env marker
//!   (`QD_SESSION_ID`/`CLAUDECODE`) or a non-TTY stdout ⇒ JSONL. Tests capture
//!   stdout through a pipe, so JSONL is the default they get; `--table` forces
//!   the human surface. The runner removes both markers and sets `NO_COLOR=1` so
//!   the table renders plain and byte-stable whatever env the suite inherits.
//! - **A jailed HOME has no sessions**, so every query is UNRESOLVED. That is
//!   the DESIGNED path for a stopped-and-collected session, not a degenerate
//!   one: unresolved + rows ⇒ exit 0 and the rows print (the log outlives the
//!   session it was addressed to); unresolved + NO rows ⇒ the familiar
//!   `No session matching "x"` on stderr, exit 1. The alias that works without a
//!   live session is the literal query string, so every fixture targets the
//!   string the test queries with.
//!
//! NOT covered here (out of scope — needs a resolvable session in the gather):
//! the name/stable-id/id-prefix alias tiers of `resolve_addresses`, and the
//! `No messages logged for "x".` human line, which is reachable only when a
//! session RESOLVES but has no rows.

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

/// Run `qd <verb> <args...>` with HOME jailed into `home` and the data root at
/// `<home>/.quorum/dispatch` (QD_HOME unset).
///
/// The agent markers are REMOVED and `NO_COLOR` set: `messages` picks its output
/// surface from the driver (marker or pipe ⇒ JSON), so a suite inheriting a live
/// `QD_SESSION_ID` from the shell that launched `cargo test` would otherwise be
/// testing a different default than a developer's terminal sees. Stdout here is
/// a pipe either way, so JSONL is the default and `--table` is the override.
fn run_qd(home: &Path, verb: &str, args: &[&str]) -> (i32, String, String) {
    let mut full = vec![verb];
    full.extend_from_slice(args);
    let out = Command::new(qd_bin())
        .args(&full)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .env_remove("QD_SESSION_ID")
        .env_remove("CLAUDECODE")
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run `qd messages <args...>`. Returns (exit, stdout, stderr).
fn run_messages(home: &Path, args: &[&str]) -> (i32, String, String) {
    run_qd(home, "messages", args)
}

/// The 14 message-row keys, in the verb's declared wire order: the ENVELOPE's own
/// fields (both ends — `target` and `sender` — adjacent), the computed
/// `direction`, then the JOINED disposition, then the body last. Every row
/// carries all of them — the nullable ones as JSON `null`, never skipped.
const MESSAGE_KEYS: [&str; 14] = [
    "v",
    "correlation_id",
    "authored_at",
    "expires_at",
    "target",
    "origin",
    "sender",
    "direction",
    "state",
    "attempts",
    "last_event",
    "last_attempt_at",
    "first_delivered_at",
    "body",
];

/// Parse every non-empty stdout line as a message row, asserting each carries
/// EXACTLY the stable key set (no missing column, no stray one), and return them.
fn parse_rows(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("line is not JSON: {l:?} ({e})"));
            let obj = v.as_object().unwrap_or_else(|| panic!("row is not an object: {l}"));
            let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
            got.sort_unstable();
            let mut want: Vec<&str> = MESSAGE_KEYS.to_vec();
            want.sort_unstable();
            assert_eq!(got, want, "exact stable column set on every row ({l})");
            assert_eq!(v["v"], 1, "every row carries v:1 ({l})");
            v
        })
        .collect()
}

/// The `correlation_id`s of a JSONL payload, in emission order.
fn ids(stdout: &str) -> Vec<String> {
    parse_rows(stdout)
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap().to_string())
        .collect()
}

// --- fixtures: raw JSONL rows in the documented wire shape --------------------

/// An origin log envelope row (format doc §1 key order:
/// `v, correlation_id, authored_at, expires_at, target, origin, sender, body`)
/// with NO recorded sender — the unattributed case, which is also every row
/// written before the field existed. `target` and `body` are serialized through
/// serde so a body with newlines or quotes is escaped exactly as `qd send` would
/// have written it.
fn log_row(id: &str, target: &str, authored: i64, expires: i64, body: &str) -> String {
    log_row_from(id, "null", target, authored, expires, body)
}

/// [`log_row`] with a recorded `sender` — an envelope some agent session
/// AUTHORED. `sender` is passed as a raw JSON scalar so a fixture can write
/// either `"a1b2c3d4"` or `null`.
fn log_row_from(
    id: &str,
    sender: &str,
    target: &str,
    authored: i64,
    expires: i64,
    body: &str,
) -> String {
    let target = serde_json::to_string(target).unwrap();
    let body = serde_json::to_string(body).unwrap();
    format!(
        r#"{{"v":1,"correlation_id":"{id}","authored_at":{authored},"expires_at":{expires},"target":{target},"origin":"devbox","sender":{sender},"body":{body}}}"#
    )
}

/// A `sender` value for [`log_row_from`], quoted as the wire wants it.
fn sender(id: &str) -> String {
    serde_json::to_string(id).unwrap()
}

/// A normalized EVENT row (format doc §2 key order, R14.2/R15). `kind` ∈
/// attempted|queued|delivered; `delivered` carries the REQUIRED `body_digest`.
fn ev_row(id: &str, kind: &str, created_at: i64) -> String {
    if kind == "delivered" {
        format!(
            r#"{{"v":1,"correlation_id":"{id}","event":"delivered","created_at":{created_at},"body_digest":"seeddigest"}}"#
        )
    } else {
        format!(r#"{{"v":1,"correlation_id":"{id}","event":"{kind}","created_at":{created_at}}}"#)
    }
}

/// A `delivery-failed` EVENT row — carries the required machine `class`.
fn ev_failed_row(id: &str, created_at: i64, class: &str) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","event":"delivery-failed","created_at":{created_at},"class":"{class}"}}"#
    )
}

const FAR_FUTURE: i64 = 8_000_000_000_000; // ~year 2223 — always > now
const LONG_AGO: i64 = 1_000; // ~1970 — always <= now
const AUTHORED: i64 = 1_700_000_000_000; // ~2023-11

// ===========================================================================
// Tests
// ===========================================================================

/// THE verb's whole job: rows are selected by the envelope's `target`, and a
/// message addressed to a DIFFERENT session never appears in this session's
/// report.
#[test]
fn filters_by_target_session_and_never_leaks_another() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("A1", "alpha", AUTHORED, FAR_FUTURE, "first to alpha"),
            &log_row("B1", "beta", AUTHORED + 10, FAR_FUTURE, "to beta, not alpha"),
            &log_row("A2", "alpha", AUTHORED + 20, FAR_FUTURE, "second to alpha"),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0, "rows exist ⇒ exit 0 (stderr: {stderr})");
    assert_eq!(ids(&stdout), vec!["A1", "A2"], "exactly alpha's rows ({stdout})");
    assert!(
        !stdout.contains("beta") && !stdout.contains("B1"),
        "beta's message must never appear in alpha's report ({stdout})"
    );

    // And the mirror query returns beta's one row, proving the filter is the
    // target and not an ordering accident.
    let (code, stdout, _) = run_messages(&home, &["beta"]);
    assert_eq!(code, 0);
    assert_eq!(ids(&stdout), vec!["B1"], "exactly beta's row ({stdout})");
}

/// The full row shape: the envelope's own columns come from `log.jsonl`, the
/// disposition columns are FOLDED from `dispositions.jsonl`, and the body is
/// carried verbatim.
#[test]
fn row_carries_the_envelope_fields_joined_to_the_folded_disposition() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("DELIV", "alpha", AUTHORED, FAR_FUTURE, "the prose, verbatim")],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row("DELIV", "attempted", AUTHORED + 400),
            &ev_row("DELIV", "delivered", AUTHORED + 500),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0, "exit 0 (stderr: {stderr})");
    let rows = parse_rows(&stdout);
    assert_eq!(rows.len(), 1, "one row ({stdout})");
    let r = &rows[0];

    // From the ENVELOPE.
    assert_eq!(r["v"], 1);
    assert_eq!(r["correlation_id"], "DELIV");
    assert_eq!(r["authored_at"], AUTHORED);
    assert_eq!(r["expires_at"], FAR_FUTURE);
    assert_eq!(r["target"], "alpha");
    assert_eq!(r["origin"], "devbox", "origin is the origin HOST, from the envelope");
    assert_eq!(r["body"], "the prose, verbatim");
    // From the FOLD over the events.
    assert_eq!(r["state"], "delivered");
    assert_eq!(r["attempts"], 1);
    assert_eq!(r["last_event"], "delivered");
    assert_eq!(r["last_attempt_at"], AUTHORED + 400);
    assert_eq!(r["first_delivered_at"], AUTHORED + 500);
}

/// All four derived states report for the SAME target — delivered (a delivered
/// event exists), pending (no events, unexpired), expired (no events, past its
/// own `expires_at`), failed (latest event is delivery-failed).
#[test]
fn reports_delivered_pending_expired_and_failed_for_one_session() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("DELIV", "alpha", AUTHORED, FAR_FUTURE, "landed"),
            &log_row("PEND", "alpha", AUTHORED + 1, FAR_FUTURE, "waiting"),
            &log_row("EXPIR", "alpha", AUTHORED + 2, LONG_AGO, "too late"),
            &log_row("FAIL", "alpha", AUTHORED + 3, FAR_FUTURE, "never arrived"),
        ],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row("DELIV", "attempted", AUTHORED + 400),
            &ev_row("DELIV", "delivered", AUTHORED + 500),
            &ev_row("FAIL", "attempted", AUTHORED + 600),
            &ev_failed_row("FAIL", AUTHORED + 700, "wake"),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0, "exit 0 (stderr: {stderr})");
    let rows = parse_rows(&stdout);
    assert_eq!(rows.len(), 4, "every message reports, whatever its state ({stdout})");
    let by = |id: &str| rows.iter().find(|r| r["correlation_id"] == id).cloned().unwrap();

    assert_eq!(by("DELIV")["state"], "delivered");
    let p = by("PEND");
    assert_eq!(p["state"], "pending", "no events, unexpired ⇒ pending");
    assert_eq!(p["last_event"], serde_json::Value::Null, "no events ⇒ last_event null");
    assert_eq!(p["attempts"], 0);
    assert_eq!(p["last_attempt_at"], serde_json::Value::Null);
    assert_eq!(p["first_delivered_at"], serde_json::Value::Null);
    assert_eq!(by("EXPIR")["state"], "expired", "no delivery past expires_at ⇒ expired");
    let f = by("FAIL");
    assert_eq!(f["state"], "failed", "latest event delivery-failed ⇒ failed");
    assert_eq!(f["last_event"], "delivery-failed");
    assert_eq!(f["attempts"], 1);
}

/// A report is a timeline: rows come back in ascending `authored_at` order no
/// matter what order they sit in the file (a peer's replica interleaves by WHEN
/// it was written, not by which file it was read from).
#[test]
fn rows_are_sorted_by_authored_at_not_file_order() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    // Deliberately scrambled on disk.
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("THIRD", "alpha", AUTHORED + 300, FAR_FUTURE, "c"),
            &log_row("FIRST", "alpha", AUTHORED + 100, FAR_FUTURE, "a"),
            &log_row("FOURTH", "alpha", AUTHORED + 400, FAR_FUTURE, "d"),
            &log_row("SECOND", "alpha", AUTHORED + 200, FAR_FUTURE, "b"),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0, "exit 0 (stderr: {stderr})");
    assert_eq!(
        ids(&stdout),
        vec!["FIRST", "SECOND", "THIRD", "FOURTH"],
        "oldest first, by authored_at ({stdout})"
    );
}

/// An ORPHAN event — one whose envelope is not in scope — has no `target` (R14.2
/// normalized it away), so it cannot be attributed to any session and is dropped
/// here. Nothing is LOST by that: `qd dispositions` still shows the same id,
/// because it is keyed by correlation_id and owes no target.
#[test]
fn orphan_events_are_dropped_here_but_still_visible_in_dispositions() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("HASENV", "alpha", AUTHORED, FAR_FUTURE, "has an envelope")],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row("HASENV", "delivered", AUTHORED + 500),
            // No envelope anywhere for ORPH.
            &ev_row("ORPH", "delivered", AUTHORED + 600),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0, "exit 0 (stderr: {stderr})");
    assert_eq!(ids(&stdout), vec!["HASENV"], "the orphan is not attributable ({stdout})");
    assert!(!stdout.contains("ORPH"), "no orphan row in a per-session report ({stdout})");

    // The same id IS in the id-keyed surface — re-keyed, not withheld.
    let (code, disp, stderr) = run_qd(&home, "dispositions", &[]);
    assert_eq!(code, 0, "dispositions exit 0 (stderr: {stderr})");
    assert!(
        disp.contains("ORPH"),
        "the orphan is still reported by `qd dispositions` ({disp})"
    );
}

/// A `name@host` address names THAT host's session — a different session from
/// the local one of the same name — so it counts only when the caller unioned
/// that host in. The local host id is `local` when QD_HOST is unset, so
/// `alpha@local` IS the local alpha and reports by default.
#[test]
fn host_qualified_targets_respect_the_read_scope() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("BARE", "alpha", AUTHORED, FAR_FUTURE, "unqualified"),
            &log_row("LOCALQ", "alpha@local", AUTHORED + 1, FAR_FUTURE, "explicitly this host"),
            &log_row("PEERQ", "alpha@peerbox", AUTHORED + 2, FAR_FUTURE, "another host's alpha"),
        ],
    );

    // Default (local) scope: the peer-qualified row is a different session.
    let (code, stdout, stderr) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0, "exit 0 (stderr: {stderr})");
    assert_eq!(
        ids(&stdout),
        vec!["BARE", "LOCALQ"],
        "local scope: bare + @local, never @peerbox ({stdout})"
    );

    // --host peerbox unions that namespace in.
    let (code, stdout, stderr) = run_messages(&home, &["alpha", "--host", "peerbox"]);
    assert_eq!(code, 0, "--host exit 0 (stderr: {stderr})");
    assert_eq!(
        ids(&stdout),
        vec!["BARE", "LOCALQ", "PEERQ"],
        "--host peerbox admits alpha@peerbox ({stdout})"
    );

    // --all admits every namespace.
    let (code, stdout, stderr) = run_messages(&home, &["alpha", "--all"]);
    assert_eq!(code, 0, "--all exit 0 (stderr: {stderr})");
    assert_eq!(
        ids(&stdout),
        vec!["BARE", "LOCALQ", "PEERQ"],
        "--all admits every host's alpha ({stdout})"
    );

    // A different peer is still excluded under --host peerbox (the union is that
    // ONE host, not "any qualifier").
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("BARE", "alpha", AUTHORED, FAR_FUTURE, "unqualified"),
            &log_row("OTHERQ", "alpha@otherbox", AUTHORED + 3, FAR_FUTURE, "a third host"),
        ],
    );
    let (code, stdout, _) = run_messages(&home, &["alpha", "--host", "peerbox"]);
    assert_eq!(code, 0);
    assert_eq!(
        ids(&stdout),
        vec!["BARE"],
        "--host peerbox does not admit alpha@otherbox ({stdout})"
    );
}

/// `--host <h>` also unions the peer's REPLICATED log, so a message that peer
/// recorded for this session shows up in the report.
#[test]
fn host_flag_unions_the_peer_replica_log() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("LOCAL", "alpha", AUTHORED, FAR_FUTURE, "from here")],
    );
    let peer = root.join("remote").join("peerbox");
    write_lines(
        &peer.join("log.jsonl"),
        &[&log_row("PEER", "alpha", AUTHORED + 100, FAR_FUTURE, "from the peer")],
    );
    write_lines(
        &peer.join("dispositions.jsonl"),
        &[&ev_row("PEER", "delivered", AUTHORED + 900)],
    );

    let (code, stdout, _) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0);
    assert_eq!(ids(&stdout), vec!["LOCAL"], "local scope excludes the replica");

    let (code, stdout, stderr) = run_messages(&home, &["alpha", "--host", "peerbox"]);
    assert_eq!(code, 0, "--host exit 0 (stderr: {stderr})");
    let rows = parse_rows(&stdout);
    assert_eq!(
        rows.iter().map(|r| r["correlation_id"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["LOCAL", "PEER"],
        "the replica's row interleaves by authored_at ({stdout})"
    );
    assert_eq!(rows[1]["state"], "delivered", "the peer's fold projects too");
}

#[test]
fn archive_flag_unions_the_local_archive_tier() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("HOT", "alpha", AUTHORED + 100, FAR_FUTURE, "recent")],
    );
    write_lines(
        &root.join("log.archive.jsonl"),
        &[&log_row("ARCH", "alpha", AUTHORED, FAR_FUTURE, "archived")],
    );
    write_lines(
        &root.join("dispositions.archive.jsonl"),
        &[&ev_row("ARCH", "delivered", AUTHORED + 700)],
    );

    let (code, stdout, _) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0);
    assert_eq!(ids(&stdout), vec!["HOT"], "archive tier excluded by default ({stdout})");

    let (code, stdout, stderr) = run_messages(&home, &["alpha", "--archive"]);
    assert_eq!(code, 0, "--archive exit 0 (stderr: {stderr})");
    let rows = parse_rows(&stdout);
    assert_eq!(
        rows.iter().map(|r| r["correlation_id"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["ARCH", "HOT"],
        "--archive unions the archive tier, in authored order ({stdout})"
    );
    assert_eq!(rows[0]["state"], "delivered", "the archived event folds in too");
}

#[test]
fn window_bounds_the_report_by_authored_at() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    // NEW authored ~2025-10 (inside a 3650d window from any 2026+ now); OLD at
    // epoch 1970 (outside it).
    let new_authored = 1_760_000_000_000i64;
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("NEW", "alpha", new_authored, FAR_FUTURE, "recent"),
            &log_row("OLD", "alpha", LONG_AGO, FAR_FUTURE, "ancient"),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha", "--window", "3650d"]);
    assert_eq!(code, 0, "windowed exit 0 (stderr: {stderr})");
    assert_eq!(ids(&stdout), vec!["NEW"], "window keeps recent, drops ancient ({stdout})");

    let (code, stdout, _) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0);
    assert_eq!(ids(&stdout), vec!["OLD", "NEW"], "no window ⇒ every row ({stdout})");
}

#[test]
fn bad_window_is_a_sync_refusal_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    for bad in ["1.5h", "12x", "abc", "12h30m"] {
        let (code, _out, err) = run_messages(&home, &["alpha", "--window", bad]);
        assert_eq!(code, 12, "malformed --window {bad:?} → exit 12 (stderr: {err})");
        assert!(
            err.contains("refused{window}"),
            "expected refused{{window}} for {bad:?}, got: {err}"
        );
    }
}

#[test]
fn host_and_all_conflict_is_rejected_at_parse() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    // clap conflicts_with → the centralized commander mapping (nonzero exit).
    let (code, _out, err) = run_messages(&home, &["alpha", "--host", "h", "--all"]);
    assert_ne!(code, 0, "--host + --all must not succeed (stderr: {err})");
}

/// audit #4 (end-to-end): a `--host` that would traverse out of the store root is
/// refused — `refused{host}`, exit 12 — before any `remote/<host>/` path is built.
#[test]
fn host_path_traversal_is_refused_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    // Seed a matching row so a bypass would otherwise have something to emit.
    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("LOCAL", "alpha", AUTHORED, FAR_FUTURE, "local")],
    );

    for bad in ["../../etc", "/etc/passwd", "..", "foo/bar", "/"] {
        let (code, stdout, err) = run_messages(&home, &["alpha", "--host", bad]);
        assert_eq!(code, 12, "--host {bad:?} → refused exit 12 (stderr: {err})");
        assert!(
            err.starts_with("qd send: refused{host}:") && err.contains("invalid --host"),
            "--host {bad:?} → refused{{host}}, got: {err}"
        );
        assert!(
            stdout.trim().is_empty(),
            "--host {bad:?} emits nothing (no traversal read): {stdout:?}"
        );
    }
    // Control: a legit host reads normally (exit 0, the local row still shows).
    let (code, stdout, err) = run_messages(&home, &["alpha", "--host", "peerbox"]);
    assert_eq!(code, 0, "a valid host is exit 0 (stderr: {err})");
    assert_eq!(ids(&stdout), vec!["LOCAL"], "valid host reads normally ({stdout})");
}

/// An unknown session with nothing logged is the familiar miss — the same
/// message every other verb gives, so a typo reads as a typo rather than as an
/// empty report.
#[test]
fn unknown_session_with_no_rows_is_exit_1_no_session_matching() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());

    let (code, stdout, stderr) = run_messages(&home, &["nope"]);
    assert_eq!(code, 1, "unknown session, empty store ⇒ exit 1 (stderr: {stderr})");
    assert!(
        stderr.contains(r#"No session matching "nope""#),
        "the familiar miss message, got: {stderr}"
    );
    assert!(stdout.trim().is_empty(), "nothing on stdout ({stdout:?})");
}

/// The counterpart: a session that no longer EXISTS but whose messages are in
/// the log still reports. The log outlives the session it was addressed to.
#[test]
fn collected_session_still_reports_its_history() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("GONE", "long-stopped", AUTHORED, FAR_FUTURE, "said before it stopped")],
    );

    let (code, stdout, stderr) = run_messages(&home, &["long-stopped"]);
    assert_eq!(
        code, 0,
        "an unresolvable session WITH rows is not a miss (stderr: {stderr})"
    );
    assert_eq!(ids(&stdout), vec!["GONE"], "its history reports ({stdout})");
    assert!(
        !stderr.contains("No session matching"),
        "not a miss — rows exist ({stderr})"
    );
}

/// The human table: a header, one line per message however many newlines the
/// body has, a `Dir` glyph per row, and the footer naming the total and the
/// per-side split that produced it.
#[test]
fn table_surface_is_one_line_per_message_with_the_honest_footer() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("ONE", "alpha", AUTHORED, FAR_FUTURE, "line one\nline two\nline three"),
            &log_row("TWO", "alpha", AUTHORED + 10, FAR_FUTURE, "plain body"),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha", "--table"]);
    assert_eq!(code, 0, "--table exit 0 (stderr: {stderr})");

    let lines: Vec<&str> = stdout.lines().collect();
    // header + 2 message rows + the footer's blank separator + the footer line.
    assert_eq!(lines.len(), 5, "one line per message, nothing wrapped ({stdout:?})");
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        vec!["When", "Dir", "State", "Id", "Message"],
        "the header columns ({stdout})"
    );
    assert!(lines[1].contains("line one line two line three"), "newlines collapsed to one line ({stdout})");
    assert!(lines[1].contains("pending"), "the state column ({stdout})");
    assert!(lines[1].contains("ONE"), "the short correlation id ({stdout})");
    assert!(lines[1].contains('←'), "addressed TO alpha ({stdout})");
    assert!(lines[2].contains("plain body"), "the second message ({stdout})");
    assert!(lines[3].is_empty(), "a blank line before the footer ({stdout:?})");
    assert_eq!(
        lines[4],
        r#"2 messages to/from "alpha" — 2 received (←)."#,
        "the footer states the total AND the per-side split ({stdout})"
    );

    // The count is singular for one message.
    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("ONE", "alpha", AUTHORED, FAR_FUTURE, "just one")],
    );
    let (code, stdout, _) = run_messages(&home, &["alpha", "--table"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains(r#"1 message to/from "alpha""#),
        "singular for one message ({stdout})"
    );
}

/// THE TWO-SIDED REPORT. A session's transcript is what it heard AND what it
/// said, interleaved on the origin timeline — matched on `target` one way and on
/// `sender` the other, each row carrying which way it went.
#[test]
fn both_ends_are_reported_and_interleaved_in_time() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    // alpha is queried by its literal id (a jailed HOME resolves no sessions, so
    // the literal query IS the alias — see the module doc). Rows, in authored
    // order: alpha is asked something, alpha answers, a third party talks to
    // someone else entirely.
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row_from("IN1", &sender("b0b0b0b0"), "a1b2c3d4", AUTHORED, FAR_FUTURE, "check the lane routing?"),
            &log_row_from("OUT1", &sender("a1b2c3d4"), "beta", AUTHORED + 10, FAR_FUTURE, "looked — it is the bridge"),
            &log_row_from("OTHER", &sender("b0b0b0b0"), "gamma", AUTHORED + 20, FAR_FUTURE, "unrelated"),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["a1b2c3d4"]);
    assert_eq!(code, 0, "exit 0 (stderr: {stderr})");
    let rows = parse_rows(&stdout);
    assert_eq!(
        ids(&stdout),
        vec!["IN1", "OUT1"],
        "both ends, in authored order, and NOTHING from the unrelated pair ({stdout})"
    );

    assert_eq!(rows[0]["direction"], "received", "matched on target ({stdout})");
    assert_eq!(rows[0]["sender"], "b0b0b0b0", "who authored it ({stdout})");
    assert_eq!(rows[0]["target"], "a1b2c3d4");

    assert_eq!(rows[1]["direction"], "sent", "matched on sender ({stdout})");
    assert_eq!(rows[1]["sender"], "a1b2c3d4");
    assert_eq!(rows[1]["target"], "beta", "the counterparty, not us ({stdout})");
}

/// An UNATTRIBUTED row (no `sender` — a human in a shell, or any row predating
/// the field) is reported on the received side only. It must never be swept onto
/// somebody's sent side: absence of attribution is not attribution.
#[test]
fn an_unattributed_row_is_never_claimed_as_a_send() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[
            // Addressed TO alpha, author unrecorded.
            &log_row("TOALPHA", "alpha", AUTHORED, FAR_FUTURE, "from a shell"),
            // Addressed to someone else, author unrecorded — alpha owns neither
            // end, so it must not appear at all.
            &log_row("NEITHER", "beta", AUTHORED + 10, FAR_FUTURE, "not ours"),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0, "exit 0 (stderr: {stderr})");
    let rows = parse_rows(&stdout);
    assert_eq!(ids(&stdout), vec!["TOALPHA"], "only the addressed row ({stdout})");
    assert_eq!(rows[0]["direction"], "received");
    assert_eq!(
        rows[0]["sender"],
        serde_json::Value::Null,
        "the unattributed author stays null, never inferred ({stdout})"
    );
}

/// A row whose BOTH ends are this session reports as `self` — not collapsed onto
/// a side. `qd send`'s fence refuses this at the door when `QD_SESSION_ID`
/// resolves, so it arrives only from an unresolvable id; the report says what the
/// row says rather than picking.
#[test]
fn a_row_with_both_ends_this_session_is_self() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[&log_row_from("LOOP", &sender("a1b2c3d4"), "a1b2c3d4", AUTHORED, FAR_FUTURE, "to myself")],
    );

    let (code, stdout, stderr) = run_messages(&home, &["a1b2c3d4"]);
    assert_eq!(code, 0, "exit 0 (stderr: {stderr})");
    assert_eq!(parse_rows(&stdout)[0]["direction"], "self", "{stdout}");

    let (_, table, _) = run_messages(&home, &["a1b2c3d4", "--table"]);
    assert!(table.contains('↺'), "the self glyph ({table})");
    assert!(
        table.contains(r#"1 message to/from "a1b2c3d4" — 1 self (↺)."#),
        "the footer names the side that occurred ({table})"
    );
}

/// The sent side is an EXACT id match — no id-prefix tier, though the received
/// side has one. A prefix on `sender` would let a collision claim another
/// session's authorship, which is the one error an attribution column must not
/// make.
#[test]
fn a_sender_prefix_is_not_a_send_though_a_target_prefix_is_a_receive() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    write_lines(
        &root.join("log.jsonl"),
        &[
            // Queried as "a1b2c3d4": this row's SENDER is only a prefix of it.
            &log_row_from("PREFIX", &sender("a1b2"), "beta", AUTHORED, FAR_FUTURE, "not ours to claim"),
            // …while the same string as a TARGET is a legitimate received row,
            // because a person types prefixes copied out of `qd ls`.
            &log_row("EXACT", "a1b2c3d4", AUTHORED + 10, FAR_FUTURE, "addressed to us"),
        ],
    );

    let (code, stdout, stderr) = run_messages(&home, &["a1b2c3d4"]);
    assert_eq!(code, 0, "exit 0 (stderr: {stderr})");
    assert_eq!(
        ids(&stdout),
        vec!["EXACT"],
        "a prefix in `sender` is not authorship ({stdout})"
    );
}

/// `--full` prints the body verbatim — newlines intact — under a header line
/// carrying the id, state, age and the RAW address the message was sent to.
///
/// `--table` is passed alongside only to keep this test about the RENDERER: stdout
/// here is a pipe, and the surface question — a bare `--full` implies the human
/// view — is pinned separately by `full_flag_alone_selects_the_human_surface`.
#[test]
fn full_surface_prints_untruncated_multiline_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    let body = "REPORT-FROM codex\n\nline two of the report\nline three";
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row_from("FULL", &sender("b0b0b0b0"), "alpha", AUTHORED, FAR_FUTURE, body),
            // …and the unattributed twin, whose header must show the absence
            // rather than omit the end or invent an author.
            &log_row("ANON", "alpha", AUTHORED + 10, FAR_FUTURE, "from a shell"),
        ],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[&ev_row("FULL", "delivered", AUTHORED + 500)],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha", "--table", "--full"]);
    assert_eq!(code, 0, "--full exit 0 (stderr: {stderr})");
    assert!(stdout.contains(body), "the body appears VERBATIM, newlines intact ({stdout:?})");
    assert!(
        stdout.contains("── FULL · delivered ·"),
        "the block header carries the full id and the state ({stdout})"
    );
    assert!(
        stdout.contains("b0b0b0b0 → alpha"),
        "the block header carries BOTH raw ends, sender → target ({stdout})"
    );
    assert!(
        stdout.contains("— → alpha"),
        "an unattributed sender prints as the em dash, not as a missing end ({stdout})"
    );
    // No elision anywhere in --full.
    assert!(!stdout.contains('…'), "--full never elides ({stdout:?})");
}

/// A bare `--full` selects the human surface by itself.
///
/// It did not, at first: the surface was chosen from `--json`/`--table` + the
/// driver, and `--full` was read only inside the human branch — so the natural
/// `qd messages alpha --full | less` came back as JSONL with the flag silently
/// inert. The house precedent for an inert content modifier (`ls --short` under
/// `--json`) does not carry: `--short` narrows a document JSON can also produce,
/// while `--full` has no meaning under JSON at all, which never elided a body.
/// A flag that exists solely to change how the human view prints is a request for
/// the human view. `resolve_emit_json` now takes it as an `Interactive` driver
/// override, exactly as `--table` is; an explicit `--json --full` still yields
/// JSON, because an explicit selector beats an implied one.
#[test]
fn full_flag_alone_selects_the_human_surface() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    let body = "REPORT-FROM codex\n\nline two of the report";
    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("FULL", "alpha", AUTHORED, FAR_FUTURE, body)],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha", "--full"]);
    assert_eq!(code, 0, "--full exit 0 (stderr: {stderr})");
    assert!(
        stdout.contains(body),
        "a bare --full must print the body, not a JSON document ({stdout:?})"
    );
}

/// A long body is elided in the table (a fixed 72-char budget, so the column
/// renders identically with or without a terminal) but complete in `--json`.
#[test]
fn long_bodies_are_elided_in_the_table_and_complete_in_json() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    // 100 chars — comfortably past the 72-char preview budget.
    let long_body = "x".repeat(100);
    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("LONG", "alpha", AUTHORED, FAR_FUTURE, &long_body)],
    );

    let (code, stdout, stderr) = run_messages(&home, &["alpha", "--table"]);
    assert_eq!(code, 0, "--table exit 0 (stderr: {stderr})");
    assert!(stdout.contains('…'), "an over-long preview is elided ({stdout})");
    assert!(
        !stdout.contains(&long_body),
        "the whole body is NOT in the table ({stdout})"
    );
    let row_line = stdout.lines().nth(1).unwrap();
    let xs = row_line.chars().filter(|c| *c == 'x').count();
    assert_eq!(xs, 72, "exactly the 72-char budget, then the ellipsis ({row_line:?})");

    // --json carries it whole.
    let (code, stdout, _) = run_messages(&home, &["alpha", "--json"]);
    assert_eq!(code, 0);
    let rows = parse_rows(&stdout);
    assert_eq!(rows[0]["body"], long_body, "JSON carries the untruncated body");
}

/// `--json` forces the machine surface even when the human one would have been
/// chosen, and `--table` forces the human surface even for a piped/agent caller.
#[test]
fn json_and_table_flags_override_the_auto_detected_surface() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    write_lines(
        &root.join("log.jsonl"),
        &[&log_row("ID1", "alpha", AUTHORED, FAR_FUTURE, "hi")],
    );

    // Piped stdout ⇒ JSONL by default (the auto-detect).
    let (code, stdout, _) = run_messages(&home, &["alpha"]);
    assert_eq!(code, 0);
    assert_eq!(ids(&stdout), vec!["ID1"], "a pipe gets the machine surface ({stdout})");

    // --json is the same, explicitly.
    let (code, stdout, _) = run_messages(&home, &["alpha", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(ids(&stdout), vec!["ID1"]);

    // --table forces the human surface through the very same pipe.
    let (code, stdout, _) = run_messages(&home, &["alpha", "--table"]);
    assert_eq!(code, 0);
    assert!(
        stdout.starts_with("When"),
        "--table forces the table even on a pipe ({stdout})"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout.lines().next().unwrap()).is_err(),
        "the table is not JSON ({stdout})"
    );
}

#[test]
fn broken_pipe_does_not_panic() {
    // `qd messages alpha | head -0`: head reads nothing and closes the pipe
    // immediately, so qd's stdout write gets EPIPE. The verb must exit cleanly
    // (141 on SIGPIPE, or 0 if it finished writing first) — NEVER 101 (a panic).
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    let rows: Vec<String> = (0..2000)
        .map(|i| log_row(&format!("ID{i:05}"), "alpha", AUTHORED + i, FAR_FUTURE, "a body"))
        .collect();
    let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    write_lines(&root.join("log.jsonl"), &refs);

    let piped = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "'{}' messages alpha | head -0 >/dev/null; echo qd_exit=${{PIPESTATUS[0]}}",
            qd_bin()
        ))
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .env_remove("QD_SESSION_ID")
        .env_remove("CLAUDECODE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn bash pipeline");
    let stdout = String::from_utf8_lossy(&piped.stdout);
    let stderr = String::from_utf8_lossy(&piped.stderr);

    assert!(
        !stderr.contains("panicked") && !stdout.contains("panicked"),
        "qd must not panic on a broken pipe (stdout: {stdout}, stderr: {stderr})"
    );
    let qd_exit: Option<i32> = stdout
        .lines()
        .find_map(|l| l.strip_prefix("qd_exit="))
        .and_then(|n| n.trim().parse().ok());
    assert!(
        matches!(qd_exit, Some(141) | Some(0)),
        "qd exit on broken pipe must be 141 or 0, got {qd_exit:?} (stdout: {stdout}, stderr: {stderr})"
    );
}
