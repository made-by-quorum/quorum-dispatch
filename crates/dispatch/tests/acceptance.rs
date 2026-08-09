//! The qd–qf transition ACCEPTANCE suite (TRANSITION §6, amended to the
//! R8/R8a/R8b disposition-event-log model), assembled as ONE committed
//! integration file. Drives the REAL `qd` binary against a JAILED, empty HOME
//! (L9a — never the real home; HOME points into a per-test tempdir, QD_HOME
//! removed so the transport files land under `<HOME>/.quorum/dispatch`).
//!
//! **This file IS the v1 conformance cell for the disposition transport
//! surface** (format doc "Conformance (v1)"): the log→events→`qd dispositions`→
//! DuckDB round-trip (via `read_ndjson_auto`), inbound-mode idempotence, and
//! the named door refusals. The per-provider conformance battery covers the
//! older per-session `events.jsonl` transport (`send_id`/`content_sha256`), not
//! this `correlation_id`-keyed surface.
//!
//! §6 acceptance bar, under R8 — three demonstrations, each in its own section:
//!   #1 LOCAL SEND ROUND-TRIP + DuckDB JOIN (the centerpiece): envelope
//!      (log.jsonl) → witnessed EVENT rows (dispositions.jsonl) →
//!      `qd dispositions` SUMMARY stdout → piped into DuckDB
//!      (`read_ndjson_auto('/dev/stdin')`) → the join yields the `delivered`
//!      summary (delivered-event-exists absorbs, even past expiry) AND a
//!      zero-events `pending` summary whose nullable columns surface as SQL
//!      NULLs (stable columns) — plus an `--events` DuckDB read counting the
//!      funnel by event type (the fine grain frame's analytics views project
//!      over).
//!      §6 PERMANENT DISCRIMINATING SCENARIO (kills "first terminal wins"):
//!      the REAL binary's failed leg (attempted + queued +
//!      delivery-failed{wake} rows land through the actual stamp points), then
//!      the retry's success appended as byte-exact `attempted` + `delivered`
//!      events for the SAME correlation_id → the summary reads `delivered`
//!      WHILE the `delivery-failed` row still EXISTS in `--events`, and
//!      `--events` shows the whole funnel in order.
//!   #2 INBOUND IDEMPOTENCE keyed on a `delivered` EVENT EXISTING: the first
//!      presentation stamps the admission funnel (attempted → queued →
//!      delivery-failed{wake} on an unwakeable target — `accepted` is retired,
//!      R14.3); after the retry's success is recorded, replaying the SAME payload
//!      is a no-op success ("already delivered — no-op", exit 0, NO new rows);
//!      log.jsonl empty throughout (inbound never appends).
//!   #3 DOOR REFUSALS with named classes: malformed → refused{malformed}
//!      (stderr-only, NO row — no trustworthy id); past-expiry →
//!      expired{past-expiry} stderr WITH a `refused{past-expiry}` funnel row;
//!      ambiguous target → refused{ambiguous} stderr WITH a `refused{ambiguous}`
//!      funnel row (R14.3: a parse-valid inbound refusal rides IN the funnel).
//!
//! ── WRITE-HALF of #1 (documented choice) ──────────────────────────────────
//! The `delivered` arm of the round-trip is seeded DETERMINISTICALLY: byte-
//! exact `log.jsonl` envelope + `dispositions.jsonl` EVENT rows in the
//! documented wire shape (format doc §§1–2, the same lines the leaf crate's
//! golden tests pin). WHY not a live `qd send`: a real `delivered` event
//! requires a LIVE receive carrier (a relay port, a joined zmx mux pane, or a
//! codex/acp/pi daemon — see `send_unified::select_carrier`); a bare PTY
//! session in a jailed, empty-ZMX test has none and hits `NoLiveReceivePath`.
//! A live-carrier delivered leg is DEFERRED TO CUTOVER per ruling R7 — the
//! seam-level twin test in `src/bin/qd/verbs/send_unified.rs` proves the full
//! funnel through the real stamp points. The REAL binary write path is
//! nonetheless exercised end-to-end here by the §6 discriminating scenario
//! below, which drives the ACTUAL `qd send` to a genuine failed{wake} funnel
//! (deterministic + hermetic — an unwakeable unknown-provider row) before the
//! seeded retry-success completes the fail→retry→delivered story. Between the
//! arms, every link of the §6 chain is asserted, and the failed leg is proven
//! on the real binary write path.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// The absolute path to the DuckDB CLI on this box (Homebrew). The round-trip
/// tests GATE on its presence: absent ⇒ skip with a loud `eprintln!` (so a CI
/// host without DuckDB does not red), present ⇒ run the join for real.
const DUCKDB: &str = "/opt/homebrew/bin/duckdb";

/// The qd data root under a jailed HOME: `<home>/.quorum/dispatch` (QD_HOME
/// unset). Transport files (`log.jsonl`, `dispositions.jsonl`) live DIRECTLY
/// under it (format doc: not under `state/`).
fn dispatch_root(home: &Path) -> PathBuf {
    home.join(".quorum").join("dispatch")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A jailed HOME under `dir` with `.claude/sessions` + an empty zmx dir. Never
/// the real home (the L9a guard every sibling suite asserts).
struct Jail {
    home: PathBuf,
    sessions: PathBuf,
    zmx: PathBuf,
}

fn jail(dir: &Path) -> Jail {
    let home = dir.join("home");
    let sessions = home.join(".claude").join("sessions");
    let zmx = dir.join("zmx");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    std::fs::create_dir_all(dispatch_root(&home)).unwrap();
    let real = std::env::var("HOME").unwrap_or_default();
    assert_ne!(
        home.to_string_lossy(),
        real,
        "jailed HOME must not be the real HOME"
    );
    Jail { home, sessions, zmx }
}

// --- byte-exact wire fixtures (format doc §§1–2, matching the leaf goldens) --

/// An origin `log.jsonl` envelope row in the documented key order (format doc
/// §1: `v, correlation_id, authored_at, expires_at, target, origin, body`).
/// Byte-shape-identical to what `origin_send::build_envelope` +
/// `Envelope::to_jsonl_line` write on a real origin send.
fn log_row(id: &str, authored: i64, expires: i64) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","authored_at":{authored},"expires_at":{expires},"target":"alpha@brano","origin":"brano","body":"hello over the pipe"}}"#
    )
}

/// A normalized EVENT row (format doc §2 key order, R14.2/R15). attempted/queued
/// carry no tail; `delivered` carries a REQUIRED `body_digest` (R15). The
/// `witness`/`origin`/`authored` params are accepted for call-site compatibility
/// but are NOT fields on a normalized event row (they live on the envelope now;
/// events join by correlation_id).
fn ev_row(id: &str, kind: &str, created_at: i64, _witness: &str, _origin: &str, _authored: i64) -> String {
    if kind == "delivered" {
        format!(
            r#"{{"v":1,"correlation_id":"{id}","event":"delivered","created_at":{created_at},"body_digest":"seeddigest"}}"#
        )
    } else {
        format!(
            r#"{{"v":1,"correlation_id":"{id}","event":"{kind}","created_at":{created_at}}}"#
        )
    }
}

/// A `delivered` EVENT row whose `body_digest` is the REAL R15 digest of `body`
/// (the hex sha-256 of the parsed body string, via `origin_send::body_digest`).
/// Idempotency tests that seed a prior delivery MUST bind the delivered event to
/// the SAME digest the door computes for the replayed envelope's body, or the
/// door would (correctly) refuse the replay as `body-mismatch` instead of no-op.
fn ev_delivered_for_body(id: &str, created_at: i64, body: &str) -> String {
    let digest = dispatch::origin_send::body_digest(body);
    format!(
        r#"{{"v":1,"correlation_id":"{id}","event":"delivered","created_at":{created_at},"body_digest":"{digest}"}}"#
    )
}

/// A `delivery-failed` EVENT row — one of the two variants carrying the required
/// machine `class` (last on the wire, format doc §2 / R14.2).
fn ev_failed_row(id: &str, created_at: i64, _witness: &str, _origin: &str, _authored: i64, class: &str) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","event":"delivery-failed","created_at":{created_at},"class":"{class}"}}"#
    )
}

/// Write JSONL lines to `path` (LF-terminated, one record per line).
fn write_lines(path: &Path, lines: &[&str]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(path, body).unwrap();
}

/// Append raw JSONL lines to an existing transport file (test-side seeding of a
/// "later invocation's" rows — e.g. the retry's success events).
fn append_lines(path: &Path, lines: &[&str]) {
    let mut body = std::fs::read_to_string(path).unwrap_or_default();
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    std::fs::write(path, body).unwrap();
}

/// Run `qd dispositions <args...>` in the jail (QD_HOME removed) and return its
/// raw stdout (the JSONL projection to be piped into DuckDB).
fn qd_dispositions_stdout(home: &Path, args: &[&str]) -> (i32, String, String) {
    let mut full = vec!["dispositions"];
    full.extend_from_slice(args);
    let out = Command::new(qd_bin())
        .args(&full)
        .env("HOME", home)
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .output()
        .expect("spawn qd dispositions");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The CANONICAL round-trip: pipe `qd dispositions`' stdout straight into DuckDB
/// over `/dev/stdin` and run `sql`, returning DuckDB's `-json` result string (a
/// JSON array). This is THE §6 "DuckDB join over the pipe" — qd's stdout is the
/// pipe DuckDB reads; we build the pipeline under `bash -c` so the shell wires
/// qd's stdout to DuckDB's stdin exactly as an operator would.
///
/// ⚠ `read_ndjson_auto('/dev/stdin')` (NOT bare `read_json_auto`, which collapses
/// the stream into a single `json` column) — the verified canonical invocation.
fn duckdb_over_pipe(home: &Path, disp_args: &[&str], sql: &str) -> String {
    let pipeline = format!(
        "'{qd}' dispositions {args} | '{duck}' -json -c \"{sql}\"",
        qd = qd_bin(),
        args = disp_args.join(" "),
        duck = DUCKDB,
        sql = sql,
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(&pipeline)
        .env("HOME", home)
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn bash pipeline (qd dispositions | duckdb)");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "the `qd dispositions | duckdb` pipeline must succeed.\n  pipeline: {pipeline}\n  stdout: {stdout}\n  stderr: {stderr}"
    );
    stdout
}

/// Skip-gate: is the DuckDB CLI present? Absent ⇒ the round-trip test prints a
/// loud skip and returns green (a CI host without DuckDB must not red). On brano
/// it IS present, so the join runs for real.
fn duckdb_present() -> bool {
    Path::new(DUCKDB).exists()
}

/// Parse a dispositions.jsonl body / `--events` stdout into raw event values.
fn parse_event_rows(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad event row {l:?}: {e}")))
        .collect()
}

// ===========================================================================
// DEMONSTRATION #1 — LOCAL SEND ROUND-TRIP + DuckDB JOIN (the centerpiece).
//
// The whole §6 chain, asserted link by link:
//   log.jsonl envelope  →  dispositions.jsonl EVENT funnel (same
//   correlation_id)  →  `qd dispositions` summary stdout  →  DuckDB join over
//   the pipe (+ an `--events` DuckDB read over the raw funnel).
// ===========================================================================

/// The reference DELIVERED summary line (byte-exact — the leaf crate's
/// `summary_record_golden_line` golden): the fail→retry→succeed fold. The seeds
/// below reproduce it through the REAL binary's read path. NOTE the golden's
/// `expires_at` (2026-06) is already in the PAST at any run date ≥ 2026-08 —
/// the summary still reads `delivered` because a delivered event EXISTING is
/// the only absorbing state (R10 precedence: delivered > expired).
const GOLDEN_DELIVERED_SUMMARY: &str = r#"{"v":1,"correlation_id":"01ABC","state":"delivered","attempts":2,"last_event":"delivered","last_attempt_at":1781241500200,"first_delivered_at":1781241500500,"expires_at":1781284700000,"authored_at":1781241499000,"origin":"brano"}"#;

/// The reference `delivery-failed` EVENT line (byte-exact — the leaf crate's
/// `delivery_failed_event_golden_line` golden), seeded verbatim as an
/// orphan-event id (01DEF) so the `--events` read carries a classed failure.
/// Normalized (R14.2): `{v, correlation_id, event, created_at, class}`.
const GOLDEN_FAILED_EVENT: &str = r#"{"v":1,"correlation_id":"01DEF","event":"delivery-failed","created_at":1781241600000,"class":"wake"}"#;

#[test]
fn roundtrip_log_to_events_to_summary_to_duckdb_join() {
    if !duckdb_present() {
        eprintln!(
            "SKIP roundtrip_...duckdb_join: DuckDB CLI absent at {DUCKDB} — \
             the JSONL-over-pipe join cannot run on this host (present on brano). \
             The non-DuckDB links of the chain are still asserted by \
             roundtrip_chain_links_without_duckdb."
        );
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let root = dispatch_root(&j.home);

    let far_future = 8_000_000_000_000i64; // ~year 2223 — always > now.
    let authored = 1_781_241_499_000i64; // the golden's origin timeline.
    let delivered_id = "01ABC";
    let pending_id = "01PENDINGNOEVENTSAAAAAAAAA";

    // WRITE HALF (seeded, byte-exact — see the module doc for why not a live
    // send): the DELIVERED envelope + its fail→retry→succeed EVENT funnel
    // (witnessed by "mira", origin "brano" — matching the golden summary), a
    // PENDING envelope with far-future expiry and NO events, and the reference
    // orphan delivery-failed event (01DEF) verbatim.
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row(delivered_id, authored, 1_781_284_700_000),
            &log_row(pending_id, authored, far_future),
        ],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row(delivered_id, "attempted", 1_781_241_500_100, "mira", "brano", authored),
            &ev_failed_row(delivered_id, 1_781_241_500_150, "mira", "brano", authored, "delivery"),
            &ev_row(delivered_id, "attempted", 1_781_241_500_200, "mira", "brano", authored),
            &ev_row(delivered_id, "delivered", 1_781_241_500_500, "mira", "brano", authored),
            GOLDEN_FAILED_EVENT,
        ],
    );

    // LINK 1 — the on-disk transport carries the envelope + the matching EVENT
    // funnel (same correlation_id). This is the "log row → event rows" leg.
    let log_body = std::fs::read_to_string(root.join("log.jsonl")).unwrap();
    let disp_body = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap();
    assert!(
        log_body.contains(delivered_id),
        "log.jsonl carries the delivered envelope, got: {log_body:?}"
    );
    assert!(
        disp_body.contains(delivered_id) && disp_body.contains("\"event\":\"delivered\""),
        "dispositions.jsonl carries the delivered EVENT for the SAME id, got: {disp_body:?}"
    );

    // LINK 2 — `qd dispositions` FOLDS the funnel into the emitted summary: the
    // delivered id's line is BYTE-EXACTLY the published golden (delivered
    // absorbs both the earlier failure AND the passed expiry), and the
    // zero-events envelope derives `pending` with `last_event` null (R11.1 —
    // stable columns as JSON null, never skipped; witness DROPPED in R14.2).
    let (code, stdout, stderr) = qd_dispositions_stdout(&j.home, &[]);
    assert_eq!(code, 0, "qd dispositions exit 0 (stderr: {stderr})");
    let delivered_line = stdout
        .lines()
        .find(|l| l.contains("\"correlation_id\":\"01ABC\""))
        .unwrap_or_else(|| panic!("delivered summary emitted: {stdout}"));
    assert_eq!(
        delivered_line, GOLDEN_DELIVERED_SUMMARY,
        "the emitted summary is byte-exactly the published golden"
    );
    let pending_line = stdout
        .lines()
        .find(|l| l.contains(pending_id))
        .unwrap_or_else(|| panic!("pending summary emitted: {stdout}"));
    assert!(
        pending_line.contains("\"state\":\"pending\"")
            && pending_line.contains("\"last_event\":null")
            && !pending_line.contains("\"witness\""),
        "zero-events pending summary: last_event null, no witness column (R14.2), got: {pending_line}"
    );

    // LINK 3 — the DuckDB JOIN over the pipe (summary mode): qd's stdout →
    // DuckDB `read_ndjson_auto('/dev/stdin')`. The DELIVERED summary row with
    // the folded analytics; the PENDING row with SQL NULLs in the stable
    // nullable columns.
    let delivered_json = duckdb_over_pipe(
        &j.home,
        &[],
        "SELECT correlation_id, state, attempts, last_event, first_delivered_at, origin \
         FROM read_ndjson_auto('/dev/stdin') \
         WHERE state = 'delivered'",
    );
    let delivered: serde_json::Value =
        serde_json::from_str(&delivered_json).expect("DuckDB -json emits a JSON array");
    let delivered_rows = delivered.as_array().expect("DuckDB result is an array");
    assert_eq!(
        delivered_rows.len(),
        1,
        "exactly one delivered summary joins over the pipe, got: {delivered_json}"
    );
    assert_eq!(delivered_rows[0]["correlation_id"], "01ABC", "{delivered_json}");
    assert_eq!(delivered_rows[0]["attempts"], 2, "two attempts folded: {delivered_json}");
    assert_eq!(delivered_rows[0]["last_event"], "delivered", "{delivered_json}");
    assert_eq!(
        delivered_rows[0]["first_delivered_at"], 1_781_241_500_500i64,
        "first_delivered_at carried through: {delivered_json}"
    );
    // R14.2: origin comes from the JOINED envelope (the seeded log row).
    assert_eq!(delivered_rows[0]["origin"], "brano", "{delivered_json}");

    // The zero-events pending summary: last_event/last_attempt_at/
    // first_delivered_at surface as SQL NULLs (stable DuckDB columns). `origin`
    // comes from the envelope (in scope) so it is NOT null here.
    let pending_json = duckdb_over_pipe(
        &j.home,
        &[],
        &format!(
            "SELECT correlation_id, state, attempts, last_event, last_attempt_at, \
                    first_delivered_at, origin \
             FROM read_ndjson_auto('/dev/stdin') \
             WHERE correlation_id = '{pending_id}'"
        ),
    );
    let pending: serde_json::Value =
        serde_json::from_str(&pending_json).expect("DuckDB -json emits a JSON array");
    let pending_rows = pending.as_array().expect("array");
    assert_eq!(pending_rows.len(), 1, "the pending record is present: {pending_json}");
    assert_eq!(pending_rows[0]["state"], "pending", "derived pending: {pending_json}");
    assert_eq!(pending_rows[0]["attempts"], 0, "{pending_json}");
    for null_col in ["last_event", "last_attempt_at", "first_delivered_at"] {
        assert_eq!(
            pending_rows[0][null_col],
            serde_json::Value::Null,
            "zero-events ⇒ {null_col} is SQL NULL in the DuckDB result: {pending_json}"
        );
    }
    assert_eq!(
        pending_rows[0]["origin"], "brano",
        "origin from the joined envelope (in scope), not null: {pending_json}"
    );

    // LINK 4 — the `--events` DuckDB read: the raw funnel is the fine grain
    // frame's analytics views project over. Count-by-event-type over the piped
    // funnel, and the machine `class` non-null EXACTLY on the delivery-failed rows.
    let counts_json = duckdb_over_pipe(
        &j.home,
        &["--events"],
        "SELECT event, count(*)::INT AS n \
         FROM read_ndjson_auto('/dev/stdin') \
         GROUP BY event ORDER BY event",
    );
    let counts: serde_json::Value = serde_json::from_str(&counts_json).expect("json array");
    assert_eq!(
        counts,
        serde_json::json!([
            {"event": "attempted", "n": 2},
            {"event": "delivered", "n": 1},
            {"event": "delivery-failed", "n": 2}
        ]),
        "count-by-event-type over the piped funnel: {counts_json}"
    );
    let classes_json = duckdb_over_pipe(
        &j.home,
        &["--events"],
        "SELECT count(*)::INT AS classed, \
                (count(*) FILTER (WHERE event = 'delivery-failed'))::INT AS failed \
         FROM read_ndjson_auto('/dev/stdin') WHERE class IS NOT NULL",
    );
    let classes: serde_json::Value = serde_json::from_str(&classes_json).expect("json array");
    assert_eq!(
        classes[0]["classed"], 2,
        "class present exactly on the delivery-failed rows: {classes_json}"
    );
    assert_eq!(
        classes[0]["classed"], classes[0]["failed"],
        "every classed row IS a delivery-failed row: {classes_json}"
    );
}

/// THE §6 PERMANENT DISCRIMINATING SCENARIO (TRANSITION §6 amended) — the
/// assertion that kills "first terminal wins".
///
/// The REAL binary drives the failed leg: `qd send` to an unwakeable
/// (unknown-provider cold) row lands the genuine funnel — envelope logged
/// first, then `attempted` + `queued` + `delivery-failed{wake}` EVENT rows
/// through the actual stamp points. Then the retry's success is appended as
/// byte-exact `attempted` + `delivered` events for the SAME correlation_id (a
/// live-carrier delivered leg is deferred to cutover per ruling R7 — the
/// seam-level twin in `send_unified.rs` proves the full funnel through the
/// real stamp points). Assert: the summary reads `delivered` WHILE the
/// `delivery-failed` row still EXISTS in `--events`, and `--events` replays
/// the whole funnel in order.
#[test]
fn sec6_failed_then_retry_summary_delivered_while_failure_row_persists() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let root = dispatch_root(&j.home);

    // An UNWAKEABLE cold target: a registry row with an unknown provider.
    // `qd send` ACCEPTS it (resume-and-deliver), attempts the wake, the wake
    // fails immediately (no headless revive for an unknown provider) → the REAL
    // failed leg lands, without a live carrier.
    let row = r#"{"pid":90099,"sessionId":"mystery-cold-acc","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"cold","name":"accwk","version":"0.1.0","provider":"mystery"}"#;
    std::fs::write(j.sessions.join("90099.json"), row).unwrap();

    // Drive the REAL `qd send` (origin mode) — the actual write-then-deliver path.
    let out = Command::new(qd_bin())
        .args(["send", "accwk", "please ack"])
        .env("HOME", &j.home)
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .env("ZMX_DIR", &j.zmx)
        .output()
        .expect("spawn qd send");
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(code, 12, "unwakeable cold target → real failed{{wake}} exit 12 (stderr: {stderr})");
    assert!(stderr.contains("failed{wake}"), "the real send failed{{wake}}: {stderr}");

    // The REAL binary wrote the envelope (write-then-deliver, with `origin`) +
    // the funnel EVENT rows attempted, queued, delivery-failed{wake}.
    let log_body = std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default();
    let disp_body = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default();
    // The self-delimiting `\n{line}\n` append framing (audit follow-up #3a) leaves
    // a leading blank line before each record, so skip empties to reach the first
    // real envelope row (mirrors `parse_event_rows`).
    let first_env_line = log_body
        .lines()
        .find(|l| !l.trim().is_empty())
        .expect("a logged envelope line");
    let env_val: serde_json::Value = serde_json::from_str(first_env_line)
        .expect("the logged envelope is valid JSON");
    assert!(env_val["origin"].is_string(), "the envelope carries `origin`: {log_body:?}");
    let real_id = env_val["correlation_id"].as_str().expect("correlation_id string").to_string();
    let authored = env_val["authored_at"].as_i64().expect("authored_at i64");
    let witness = env_val["origin"].as_str().unwrap().to_string(); // origin send: witness == origin host
    let failed_leg = parse_event_rows(&disp_body);
    let kinds: Vec<&str> = failed_leg.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["attempted", "queued", "delivery-failed"],
        "the REAL failed leg landed through the actual stamp points: {disp_body:?}"
    );
    assert_eq!(failed_leg[2]["class"], "wake", "delivery-failed carries class wake (R14.2): {disp_body:?}");
    assert!(failed_leg.iter().all(|r| r["correlation_id"] == real_id.as_str()));

    // The RETRY's success, appended as byte-exact events for the SAME id (the
    // deferred live-carrier leg — R7): a fresh `attempted`, then `delivered`.
    let t_retry = now_ms() + 1_000;
    let t_landed = now_ms() + 2_000;
    append_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row(&real_id, "attempted", t_retry, &witness, &witness, authored),
            &ev_row(&real_id, "delivered", t_landed, &witness, &witness, authored),
        ],
    );

    // ── THE DISCRIMINATING ASSERTION PAIR ────────────────────────────────────
    // (a) The summary VIEW resolves `delivered` (a delivered event EXISTS — the
    //     only absorbing state) with both attempts folded in…
    let (scode, sstdout, sstderr) = qd_dispositions_stdout(&j.home, &[&real_id]);
    assert_eq!(scode, 0, "summary exit 0 (stderr: {sstderr})");
    let summary: serde_json::Value = serde_json::from_str(sstdout.trim())
        .unwrap_or_else(|e| panic!("one summary row for {real_id}, got {sstdout:?} ({e})"));
    assert_eq!(
        summary["state"], "delivered",
        "summary = delivered DESPITE the earlier delivery-failed row (first-terminal-wins is DEAD): {sstdout}"
    );
    assert_eq!(summary["attempts"], 2, "both attempts folded: {sstdout}");
    assert_eq!(summary["last_event"], "delivered", "{sstdout}");
    assert_eq!(summary["last_attempt_at"], t_retry, "{sstdout}");
    assert_eq!(summary["first_delivered_at"], t_landed, "{sstdout}");
    // (b) …WHILE the delivery-failed row still EXISTS in the raw `--events`
    //     funnel — history is never rewritten, and the whole funnel reads in
    //     file order.
    let (ecode, estdout, estderr) = qd_dispositions_stdout(&j.home, &["--events", &real_id]);
    assert_eq!(ecode, 0, "--events exit 0 (stderr: {estderr})");
    let funnel = parse_event_rows(&estdout);
    let funnel_kinds: Vec<&str> = funnel.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert_eq!(
        funnel_kinds,
        vec!["attempted", "queued", "delivery-failed", "attempted", "delivered"],
        "--events shows the WHOLE funnel in order: {estdout}"
    );
    assert!(
        funnel.iter().any(|r| r["event"] == "delivery-failed" && r["class"] == "wake"),
        "the delivery-failed{{wake}} row EXISTS alongside the delivered summary: {estdout}"
    );

    // The same pair over the DuckDB pipe (the §6 join on the REAL binary's
    // write path), when DuckDB is present.
    if duckdb_present() {
        let sjson = duckdb_over_pipe(
            &j.home,
            &[],
            &format!(
                "SELECT state, attempts FROM read_ndjson_auto('/dev/stdin') \
                 WHERE correlation_id = '{real_id}'"
            ),
        );
        let s: serde_json::Value = serde_json::from_str(&sjson).expect("json array");
        assert_eq!(s[0]["state"], "delivered", "DuckDB sees the delivered summary: {sjson}");
        assert_eq!(s[0]["attempts"], 2, "{sjson}");
        let ejson = duckdb_over_pipe(
            &j.home,
            &["--events"],
            &format!(
                "SELECT count(*)::INT AS n FROM read_ndjson_auto('/dev/stdin') \
                 WHERE correlation_id = '{real_id}' AND event = 'delivery-failed'"
            ),
        );
        let e: serde_json::Value = serde_json::from_str(&ejson).expect("json array");
        assert_eq!(
            e[0]["n"], 1,
            "DuckDB sees the persisted delivery-failed row over --events: {ejson}"
        );
    } else {
        eprintln!("SKIP (DuckDB leg only) sec6_...: DuckDB CLI absent at {DUCKDB}.");
    }
}

/// The non-DuckDB links of the #1 chain, always run (no DuckDB gate) so the
/// log→events→`qd dispositions` legs are covered even on a host without
/// DuckDB. (The DuckDB leg itself is `roundtrip_..._duckdb_join` above.)
#[test]
fn roundtrip_chain_links_without_duckdb() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let root = dispatch_root(&j.home);
    let far_future = 8_000_000_000_000i64;
    let authored = 1_700_000_000_000i64;
    let id = "01CHAINLINKSNODUCKDBAAAAAA";
    write_lines(&root.join("log.jsonl"), &[&log_row(id, authored, far_future)]);
    write_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row(id, "attempted", 1_700_000_000_400, "brano", "brano", authored),
            &ev_row(id, "delivered", 1_700_000_000_500, "brano", "brano", authored),
        ],
    );

    let (code, stdout, stderr) = qd_dispositions_stdout(&j.home, &[id]);
    assert_eq!(code, 0, "point query exit 0 (stderr: {stderr})");
    let rec: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("the point-query record is one JSON line");
    assert_eq!(rec["correlation_id"], id);
    assert_eq!(rec["state"], "delivered", "log ∪ events folds to delivered");
    assert_eq!(rec["first_delivered_at"], 1_700_000_000_500i64, "delivered created_at carried");
    // R14.2: origin comes from the joined envelope (the seeded log row).
    assert_eq!(rec["origin"], "brano", "origin from the joined envelope");
    assert_eq!(rec["last_event"], "delivered");
}

// ===========================================================================
// DEMONSTRATION #2 — INBOUND IDEMPOTENCE keyed on a `delivered` EVENT EXISTING
// (R8). The first presentation of the payload is ADMITTED and stamps the funnel
// (attempted → queued → delivery-failed{wake} on an unwakeable target — fast,
// hermetic, real stamp points; `accepted` is retired, R14.3); after the retry's
// success is recorded, replaying the SAME payload is a NO-OP SUCCESS with NO new
// rows. Inbound NEVER appends to its own log.jsonl (a peer's envelope lives in
// the mirror). NOTE the R8 shift: a delivery-failed row alone would NOT no-op
// the replay (pinned in inbound_mode.rs
// `inbound_prior_delivery_failed_event_does_not_block_readmission`); only the
// delivered event does.
// ===========================================================================

#[test]
fn inbound_replay_after_delivered_event_is_a_noop_and_never_logs() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let root = dispatch_root(&j.home);

    // Unwakeable cold target: resolves (sole name match), wake fails → the
    // admission funnel lands on the FIRST inbound.
    let row = r#"{"pid":92002,"sessionId":"inbound-acc-cold","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"cold","name":"accinbwk","version":"0.1.0","provider":"mystery"}"#;
    std::fs::write(j.sessions.join("92002.json"), row).unwrap();

    let cid = "01ACCEPTIDEMPOTENCEAAAAAAA";
    let authored = now_ms();
    let envelope = format!(
        r#"{{"v":1,"correlation_id":"{cid}","authored_at":{authored},"expires_at":{e},"target":"accinbwk","origin":"peerhost","body":"idempotent payload"}}"#,
        e = authored + 3_600_000,
    );
    let env_path = j.home.join("acc-inbound.json");
    std::fs::write(&env_path, &envelope).unwrap();

    let run_inbound = |home: &Path| -> (i32, String) {
        let out = Command::new(qd_bin())
            .args(["send", "--inbound-envelope", env_path.to_str().unwrap()])
            .env("HOME", home)
            .env_remove("QD_HOME")
            .env_remove("QD_HOST")
            .env("ZMX_DIR", &j.zmx)
            .output()
            .expect("spawn qd send --inbound-envelope");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // FIRST inbound: ADMITTED — the funnel lands (attempted, queued,
    // delivery-failed{wake}; `accepted` retired), exit 12, log.jsonl untouched.
    let (code1, err1) = run_inbound(&j.home);
    assert_eq!(code1, 12, "first inbound (unwakeable) → failed{{wake}} exit 12 (stderr: {err1})");
    assert!(err1.contains("failed{wake}"), "first inbound outcome, got: {err1}");

    let disps1 = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default();
    let log1 = std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default();
    let rows1 = parse_event_rows(&disps1);
    let kinds1: Vec<&str> = rows1.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds1,
        vec!["attempted", "queued", "delivery-failed"],
        "the inbound admission funnel after the first delivery attempt (no accepted), got: {disps1:?}"
    );
    assert!(rows1.iter().all(|r| r["correlation_id"] == cid));
    assert!(log1.is_empty(), "INBOUND never appends to its own log.jsonl, got: {log1:?}");

    // The RETRY's success is recorded (byte-exact, the deferred live-carrier
    // leg — R7): attempted + delivered. Normalized rows (R14.2) carry only
    // {v, correlation_id, event, created_at}; created_at = when recorded.
    append_lines(
        &root.join("dispositions.jsonl"),
        &[
            &ev_row(cid, "attempted", now_ms() + 1_000, "local", "peerhost", authored),
            // R15: the delivered row binds the REAL digest of the envelope's body,
            // so the replay of the SAME body no-ops (not body-mismatch).
            &ev_delivered_for_body(cid, now_ms() + 2_000, "idempotent payload"),
        ],
    );
    let before_replay = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap();

    // REPLAY of the SAME payload: the delivered event EXISTS with a MATCHING
    // body_digest ⇒ NO-OP SUCCESS. No re-delivery, NO new rows, log still empty.
    let (code2, err2) = run_inbound(&j.home);
    assert_eq!(code2, 0, "replay after delivered → no-op SUCCESS exit 0 (stderr: {err2})");
    assert!(
        err2.contains(cid) && err2.contains("already delivered — no-op"),
        "the no-op names the id + the delivered fact, got: {err2}"
    );

    let disps2 = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default();
    let log2 = std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default();
    assert_eq!(
        disps2, before_replay,
        "the no-op appends NOTHING to dispositions.jsonl (byte-unchanged)"
    );
    assert!(log2.is_empty(), "log.jsonl still empty after the no-op, got: {log2:?}");
}

// ===========================================================================
// DEMONSTRATION #3 — DOOR REFUSALS WITH NAMED CLASSES (exact
// `qd send: <family>{<class>}:` stderr + exit 12). One canonical place for the
// §6 named-refusal bar: malformed payload, past-expiry, ambiguous target.
// R14.3: a PARSE-VALID inbound refusal (past-expiry / ambiguous) stamps a
// `refused{class}` row IN the funnel; MALFORMED stays stderr-only (no
// trustworthy correlation_id → no row).
// ===========================================================================

/// Run `qd send --inbound-envelope <path>` on a written envelope file, returning
/// (exit, stderr, log.jsonl, dispositions.jsonl).
fn run_inbound_file(j: &Jail, contents: &str) -> (i32, String, String, String) {
    let path = j.home.join("door.json");
    std::fs::write(&path, contents).unwrap();
    let out = Command::new(qd_bin())
        .args(["send", "--inbound-envelope", path.to_str().unwrap()])
        .env("HOME", &j.home)
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .env("ZMX_DIR", &j.zmx)
        .output()
        .expect("spawn qd send --inbound-envelope");
    let root = dispatch_root(&j.home);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default(),
        std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default(),
    )
}

/// Malformed payload → `refused{malformed}` exit 12, refused at the door BEFORE
/// resolve, touching no state.
#[test]
fn door_malformed_payload_is_refused_malformed_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let (code, err, log, disps) = run_inbound_file(&j, "this is not json at all {");
    assert_eq!(code, 12, "malformed payload → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{malformed}:"),
        "the named reason is refused{{malformed}}, got: {err}"
    );
    assert!(log.is_empty() && disps.is_empty(), "a door refusal stamps/logs nothing");
}

/// Past-expiry payload → `expired{past-expiry}` stderr + exit 12, refused at the
/// door. R14.3: a PARSE-VALID inbound refusal now stamps a `refused{past-expiry}`
/// EVENT row (the refusal rides IN the funnel) — but NEVER an `expired` row
/// (expired is a DERIVED view state; there is no expired event type). The stderr
/// family stays `expired` (Family::Expired); the stamped ROW is `refused`.
/// Inbound never logs, so log.jsonl stays empty.
#[test]
fn door_past_expiry_is_expired_past_expiry_exit_12() {
    let cid = "01ACCPASTEXPIRYAAAAAAAAAAA";
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let envelope = format!(
        r#"{{"v":1,"correlation_id":"{cid}","authored_at":{a},"expires_at":{e},"target":"accwk","origin":"peerhost","body":"stale"}}"#,
        a = now_ms(),
        e = now_ms() - 60_000, // strictly in the past.
    );
    let (code, err, log, disps) = run_inbound_file(&j, &envelope);
    assert_eq!(code, 12, "past-expiry → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: expired{past-expiry}:"),
        "the named class is expired{{past-expiry}}, got: {err}"
    );
    assert!(log.is_empty(), "inbound never logs an envelope, got: {log:?}");
    // R14.3: exactly one `refused{past-expiry}` row — never an `expired` row.
    let rows = parse_event_rows(&disps);
    assert_eq!(rows.len(), 1, "exactly one refused row stamped, got: {disps:?}");
    assert_eq!(rows[0]["event"], "refused", "the row is `refused`, not `expired`: {disps:?}");
    assert_eq!(rows[0]["class"], "past-expiry", "{disps:?}");
    assert_eq!(rows[0]["correlation_id"], cid, "keys on the envelope's id: {disps:?}");
}

/// Ambiguous target → `refused{ambiguous}` exit 12, never first-match. Two
/// GENUINELY-LIVE sessions share one name (both rows carry THIS test process's
/// live pid so the resolver's pid-aware liveness sees both as alive). Asserted on
/// the INBOUND door here (the origin twin is pinned by
/// `verbs_a4::send_origin_ambiguous_name_is_refused_ambiguous_exit_12`).
#[test]
fn door_ambiguous_target_is_refused_ambiguous_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let live_pid = std::process::id() as i64; // the test runner — definitely alive.
    for (fname, sid) in [("twin-a.json", "acc-ambi-A"), ("twin-b.json", "acc-ambi-B")] {
        let row = format!(
            r#"{{"pid":{live_pid},"sessionId":"{sid}","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"acctwin","version":"0.1.0","provider":"mystery"}}"#
        );
        std::fs::write(j.sessions.join(fname), row).unwrap();
    }

    let cid = "01ACCAMBIGUOUSAAAAAAAAAAAA";
    let envelope = format!(
        r#"{{"v":1,"correlation_id":"{cid}","authored_at":{a},"expires_at":{e},"target":"acctwin","origin":"peerhost","body":"hi"}}"#,
        a = now_ms(),
        e = now_ms() + 3_600_000,
    );
    let (code, err, log, disps) = run_inbound_file(&j, &envelope);
    assert_eq!(code, 12, "ambiguous target → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{ambiguous}:"),
        "the named class is refused{{ambiguous}} (never first-match), got: {err}"
    );
    assert!(
        err.contains("matches 2 sessions"),
        "the refusal names the collision, got: {err}"
    );
    assert!(log.is_empty(), "inbound never logs an envelope, got: {log:?}");
    // R14.3: a parse-valid inbound refusal rides IN the funnel — exactly one
    // `refused{ambiguous}` row keyed on the envelope's id.
    let rows = parse_event_rows(&disps);
    assert_eq!(rows.len(), 1, "exactly one refused row, got: {disps:?}");
    assert_eq!(rows[0]["event"], "refused", "{disps:?}");
    assert_eq!(rows[0]["class"], "ambiguous", "{disps:?}");
    assert_eq!(rows[0]["correlation_id"], cid, "{disps:?}");
}
