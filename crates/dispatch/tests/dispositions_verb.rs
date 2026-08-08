//! Integration tests for the `qd dispositions` READ verb (qd–qf transition W5),
//! driving the REAL `qd` binary against a JAILED, empty HOME (L9a — never the
//! real home; HOME points into a per-test tempdir).
//!
//! The verb is a thin CLI over `dispatch::dispositions::query` (unit-tested in
//! that module); these pin the bin wiring end-to-end: the transport files seeded
//! under `<HOME>/.quorum/dispatch/` are read, projected into the 4-state emitted
//! record (format doc §3), and emitted as JSONL on stdout — one record per line,
//! each valid JSON carrying `v,correlation_id,state,...`. Scope (`--host`), the
//! point query, and the broken-pipe (exit 141) contract are exercised here.

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

/// Write raw JSONL lines (already-serialized `log.jsonl` rows) to a file.
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
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Parse every non-empty stdout line as JSON, asserting each carries the
/// published emitted-record keys, and return them.
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
            assert!(v["authority"].is_string(), "authority present ({l})");
            v
        })
        .collect()
}

// --- fixtures: raw JSONL rows in the documented wire shape --------------------

/// An origin log envelope row (format doc §1 key order).
fn log_row(id: &str, authored: i64, expires: i64) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","authored_at":{authored},"expires_at":{expires},"target":"alpha@brano","authority":"brano","body":"hi"}}"#
    )
}

/// A delivered terminal disposition row (format doc §2; no reason).
fn disp_delivered(id: &str, authored: i64, witnessed: i64) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","state":"delivered","authored_at":{authored},"witnessed_at":{witnessed},"authority":"brano"}}"#
    )
}

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn all_local_projects_derived_states() {
    // Three envelopes: one delivered (terminal present), one pending (no
    // terminal, far-future expiry), one expired (no terminal, past expiry).
    // now is real wall-clock; pick expiries so the derived state is unambiguous
    // regardless of when the test runs.
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);

    let far_future = 8_000_000_000_000i64; // ~year 2223 — always > now
    let long_ago = 1_000i64; // ~1970 — always <= now
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("DELIV", 1_700_000_000_000, far_future),
            &log_row("PEND", 1_700_000_000_000, far_future),
            &log_row("EXPIR", 1_700_000_000_000, long_ago),
        ],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[&disp_delivered("DELIV", 1_700_000_000_000, 1_700_000_000_500)],
    );

    let (code, stdout, stderr) = run_dispositions(&home, &[]);
    assert_eq!(code, 0, "all-local exit 0 (stderr: {stderr})");
    let recs = parse_records(&stdout);
    assert_eq!(recs.len(), 3, "one record per correlation ({stdout})");

    let by_id = |id: &str| recs.iter().find(|r| r["correlation_id"] == id).cloned().unwrap();
    assert_eq!(by_id("DELIV")["state"], "delivered");
    assert_eq!(by_id("DELIV")["witnessed_at"], 1_700_000_000_500i64);
    assert_eq!(by_id("PEND")["state"], "pending");
    assert_eq!(by_id("PEND")["witnessed_at"], serde_json::Value::Null);
    assert_eq!(by_id("EXPIR")["state"], "expired");
    assert_eq!(by_id("EXPIR")["witnessed_at"], serde_json::Value::Null);
}

#[test]
fn point_query_returns_just_that_record() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    let far_future = 8_000_000_000_000i64;
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("WANT", 1_700_000_000_000, far_future),
            &log_row("OTHER", 1_700_000_000_000, far_future),
        ],
    );

    let (code, stdout, stderr) = run_dispositions(&home, &["WANT"]);
    assert_eq!(code, 0, "point query exit 0 (stderr: {stderr})");
    let recs = parse_records(&stdout);
    assert_eq!(recs.len(), 1, "exactly the one queried record ({stdout})");
    assert_eq!(recs[0]["correlation_id"], "WANT");
    assert_eq!(recs[0]["state"], "pending");

    // A miss is empty output + exit 0 (not an error).
    let (code, stdout, _) = run_dispositions(&home, &["NOPE"]);
    assert_eq!(code, 0);
    assert!(stdout.trim().is_empty(), "a miss emits nothing ({stdout:?})");
}

#[test]
fn host_flag_unions_the_peer_replica() {
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    let far_future = 8_000_000_000_000i64;

    // Local has one envelope.
    write_lines(&root.join("log.jsonl"), &[&log_row("LOCAL", 1_700_000_000_000, far_future)]);
    // A peer replica under remote/peerbox/ carries another.
    let peer = root.join("remote").join("peerbox");
    write_lines(&peer.join("log.jsonl"), &[&log_row("PEER", 1_700_000_000_000, far_future)]);
    write_lines(
        &peer.join("dispositions.jsonl"),
        &[&disp_delivered("PEER", 1_700_000_000_000, 1_700_000_000_900)],
    );

    // Local scope: peer NOT included.
    let (code, stdout, _) = run_dispositions(&home, &[]);
    assert_eq!(code, 0);
    let local_ids: Vec<String> = parse_records(&stdout)
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(local_ids, vec!["LOCAL"], "Local scope excludes the peer");

    // --host peerbox: local UNION the peer.
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
    assert_eq!(peer_rec["state"], "delivered", "peer's terminal projected");
}

#[test]
fn window_filters_by_authored_at() {
    // Two envelopes with very different authored_at; --window keeps only the
    // recent one. Use a huge window magnitude on a recent authored timestamp:
    // simpler to assert the OLD one is dropped and the NEW one kept by picking an
    // ancient authored_at for OLD (1970) vs a near-now authored_at for NEW.
    let temp = tempfile::tempdir().unwrap();
    let home = jail_home(temp.path());
    let root = dispatch_root(&home);
    let far_future = 8_000_000_000_000i64;

    // NEW authored "now-ish" (just under a day ago in ms from a 2026 wall clock);
    // OLD authored at epoch 1970. A 1h window keeps neither if NEW is >1h old, so
    // instead author NEW at basically now by using a large value close to the
    // real clock. To stay deterministic we author NEW at i64 near a 2026 ms value
    // and use an enormous window (3650d) that always covers 2026 but the OLD 1970
    // row is 56 years back — still within 3650d? 3650d ≈ 10y, so 1970 is OUT.
    let new_authored = 1_760_000_000_000i64; // ~2025-10, within ~10y of a 2026 now
    let old_authored = 1_000i64; // 1970 — outside a 10y window
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row("NEW", new_authored, far_future),
            &log_row("OLD", old_authored, far_future),
        ],
    );

    // 3650d window (~10y): covers a 2025 authored_at from a 2026 now, excludes 1970.
    let (code, stdout, stderr) = run_dispositions(&home, &["--window", "3650d"]);
    assert_eq!(code, 0, "windowed exit 0 (stderr: {stderr})");
    let ids: Vec<String> = parse_records(&stdout)
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["NEW"], "window keeps recent, drops ancient ({stdout})");

    // No window ⇒ both present.
    let (_, stdout, _) = run_dispositions(&home, &[]);
    assert_eq!(parse_records(&stdout).len(), 2, "no window ⇒ all in scope");
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
    // Born-empty transport (no files at all) → zero records, exit 0.
    let (code, stdout, stderr) = run_dispositions(&home, &[]);
    assert_eq!(code, 0, "empty store exit 0 (stderr: {stderr})");
    assert!(stdout.trim().is_empty(), "no records ({stdout:?})");
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
