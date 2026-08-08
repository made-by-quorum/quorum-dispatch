//! The qd–qf transition ACCEPTANCE suite (TRANSITION §6), assembled as ONE
//! committed integration file. Drives the REAL `qd` binary against a JAILED,
//! empty HOME (L9a — never the real home; HOME points into a per-test tempdir,
//! QD_HOME removed so the transport files land under `<HOME>/.quorum/dispatch`).
//!
//! §6 acceptance bar (verbatim): "a local send round-trip showing **log row →
//! disposition row → `qd dispositions` output → a DuckDB join over the pipe**;
//! an inbound-mode round-trip showing **idempotence** (same payload twice → one
//! delivery, one no-op success); a **door refusal with a named reason**
//! (malformed payload, ambiguous target, past-expiry)."
//!
//! The three demonstrations, each in its own section below:
//!   #1 LOCAL SEND ROUND-TRIP + DuckDB JOIN (the centerpiece):
//!      envelope (log.jsonl) → terminal (dispositions.jsonl) → `qd dispositions`
//!      stdout → piped into DuckDB (`read_ndjson_auto('/dev/stdin')`) → the JOIN
//!      yields the `delivered` record (correlation_id + non-null witnessed_at),
//!      and a second envelope with no terminal + far-future expiry projects as
//!      `pending` (witnessed_at NULL) in the same DuckDB result.
//!   #2 INBOUND IDEMPOTENCE: `qd send --inbound-envelope <file>` TWICE → one
//!      terminal, second is a no-op success, log.jsonl empty throughout.
//!   #3 DOOR REFUSALS with named reasons: malformed → refused{malformed};
//!      past-expiry → expired{past-expiry}; ambiguous target → refused{ambiguous}.
//!
//! ── WRITE-HALF of #1 (documented choice) ──────────────────────────────────
//! The `delivered` arm of the round-trip is seeded DETERMINISTICALLY: a byte-
//! exact `log.jsonl` envelope + a `delivered` `dispositions.jsonl` terminal in
//! the documented wire shape (format doc §§1–2, matching what
//! `origin_send::build_envelope` / `build_disposition` + the
//! `dispositions::append_*` writers emit). WHY not a live `qd send`: a real
//! `delivered` terminal requires a LIVE receive carrier (a relay port, a joined
//! zmx mux pane, or a codex/acp/pi daemon — see `send_unified::select_carrier`);
//! a bare PTY session in a jailed, empty-ZMX test has none and hits
//! `NoLiveReceivePath` (exit 1, no envelope, no terminal — pinned by
//! `verbs_a4::send_live_unroutable_claude_is_unchanged_no_wake_no_envelope`).
//! Standing up a live relay/mux for a committed test is heavy + flaky (the
//! existing suites defer the live claude wake to an `#[ignore]`d test). So the
//! `delivered` chain is seeded; the REAL binary write-then-deliver path is
//! nonetheless exercised end-to-end into DuckDB by the
//! `roundtrip_real_qd_send_failed_wake_terminal_flows_into_duckdb` arm below,
//! which drives the ACTUAL `qd send` to a genuine `failed{wake}` terminal
//! (deterministic + hermetic — an unwakeable unknown-provider row) and pipes
//! THAT real terminal through `qd dispositions` → DuckDB. Between the two arms,
//! every link of the §6 chain is asserted, and at least one link is proven on
//! the real binary write path.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// The absolute path to the DuckDB CLI on this box (Homebrew). The round-trip
/// test GATES on its presence: absent ⇒ skip with a loud `eprintln!` (so a CI
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

// --- byte-exact wire fixtures (format doc §§1–2, the emitted-record wire) ----

/// An origin `log.jsonl` envelope row in the documented key order (format doc §1:
/// `v, correlation_id, authored_at, expires_at, target, authority, body`). This
/// is byte-shape-identical to what `origin_send::build_envelope` +
/// `Envelope::to_jsonl_line` write on a real origin send.
fn log_row(id: &str, authored: i64, expires: i64) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","authored_at":{authored},"expires_at":{expires},"target":"alpha@brano","authority":"brano","body":"hello over the pipe"}}"#
    )
}

/// A `delivered` terminal `dispositions.jsonl` row (format doc §2 key order:
/// `v, correlation_id, state, authored_at, witnessed_at, authority`; no reason
/// for `delivered`). Byte-shape-identical to `build_disposition(.., Delivered,
/// ..)` + `Disposition::to_jsonl_line`.
fn disp_delivered(id: &str, authored: i64, witnessed: i64) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{id}","state":"delivered","authored_at":{authored},"witnessed_at":{witnessed},"authority":"brano"}}"#
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

/// Run `qd dispositions <args...>` in the jail (QD_HOME removed) and return its
/// raw stdout (the JSONL projection to be piped into DuckDB).
fn qd_dispositions_stdout(home: &Path, args: &[&str]) -> (i32, String, String) {
    let mut full = vec!["dispositions"];
    full.extend_from_slice(args);
    let out = Command::new(qd_bin())
        .args(&full)
        .env("HOME", home)
        .env_remove("QD_HOME")
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

// ===========================================================================
// DEMONSTRATION #1 — LOCAL SEND ROUND-TRIP + DuckDB JOIN (the centerpiece).
//
// The whole §6 chain, asserted link by link:
//   log.jsonl envelope  →  dispositions.jsonl `delivered` terminal (same
//   correlation_id)  →  `qd dispositions` stdout  →  DuckDB join over the pipe.
// Plus a derived-state arm: a second envelope with NO terminal + far-future
// expiry projects as `pending` (witnessed_at NULL) in the SAME DuckDB result.
// ===========================================================================

#[test]
fn roundtrip_log_to_disposition_to_dispositions_to_duckdb_join() {
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
    let authored = 1_700_000_000_000i64;
    let witnessed = 1_700_000_000_500i64;
    let delivered_id = "01DELIVEREDROUNDTRIPAAAAAA";
    let pending_id = "01PENDINGNOTERMINALAAAAAAA";

    // WRITE HALF (seeded, byte-exact — see the module doc for why not a live
    // send): the DELIVERED envelope + its matching `delivered` terminal, and a
    // second PENDING envelope with a far-future expiry and NO terminal.
    write_lines(
        &root.join("log.jsonl"),
        &[
            &log_row(delivered_id, authored, far_future),
            &log_row(pending_id, authored, far_future),
        ],
    );
    write_lines(
        &root.join("dispositions.jsonl"),
        &[&disp_delivered(delivered_id, authored, witnessed)],
    );

    // LINK 1 — the on-disk transport carries the envelope + the matching terminal
    // (same correlation_id). This is the "log row → disposition row" leg.
    let log_body = std::fs::read_to_string(root.join("log.jsonl")).unwrap();
    let disp_body = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap();
    assert!(
        log_body.contains(delivered_id),
        "log.jsonl carries the delivered envelope, got: {log_body:?}"
    );
    assert!(
        disp_body.contains(delivered_id) && disp_body.contains("\"state\":\"delivered\""),
        "dispositions.jsonl carries the matching `delivered` terminal for the SAME id, got: {disp_body:?}"
    );

    // LINK 2 — `qd dispositions` PROJECTS both into the emitted-record JSONL: the
    // delivered terminal (witnessed_at set) + the derived `pending` (no terminal,
    // far-future expiry ⇒ witnessed_at null). This is the "→ `qd dispositions`
    // output" leg.
    let (code, stdout, stderr) = qd_dispositions_stdout(&j.home, &[]);
    assert_eq!(code, 0, "qd dispositions exit 0 (stderr: {stderr})");
    assert!(
        stdout.lines().any(|l| l.contains(delivered_id) && l.contains("\"delivered\"")),
        "qd dispositions emits the delivered record, got: {stdout}"
    );
    assert!(
        stdout.lines().any(|l| l.contains(pending_id) && l.contains("\"pending\"")),
        "qd dispositions derives the pending record (silence pre-expiry), got: {stdout}"
    );

    // LINK 3 — the DuckDB JOIN over the pipe: qd's stdout → DuckDB
    // `read_ndjson_auto('/dev/stdin')`. Assert the DELIVERED record with the right
    // correlation_id + a NON-NULL witnessed_at.
    let delivered_json = duckdb_over_pipe(
        &j.home,
        &[],
        "SELECT correlation_id, state, witnessed_at \
         FROM read_ndjson_auto('/dev/stdin') \
         WHERE state = 'delivered' AND witnessed_at IS NOT NULL",
    );
    let delivered: serde_json::Value =
        serde_json::from_str(&delivered_json).expect("DuckDB -json emits a JSON array");
    let delivered_rows = delivered.as_array().expect("DuckDB result is an array");
    assert_eq!(
        delivered_rows.len(),
        1,
        "exactly one delivered record joins over the pipe, got: {delivered_json}"
    );
    assert_eq!(
        delivered_rows[0]["correlation_id"], delivered_id,
        "the joined delivered record carries the right correlation_id: {delivered_json}"
    );
    assert_eq!(
        delivered_rows[0]["state"], "delivered",
        "state column is delivered: {delivered_json}"
    );
    assert_eq!(
        delivered_rows[0]["witnessed_at"], witnessed,
        "witnessed_at is the stamped terminal time (non-null): {delivered_json}"
    );

    // DERIVED-STATE arm — the SAME pipeline surfaces the `pending` record with a
    // NULL witnessed_at (silence-pre-expiry, view-computed, never a stored row).
    let pending_json = duckdb_over_pipe(
        &j.home,
        &[],
        "SELECT correlation_id, state, witnessed_at \
         FROM read_ndjson_auto('/dev/stdin') \
         WHERE correlation_id = '01PENDINGNOTERMINALAAAAAAA'",
    );
    let pending: serde_json::Value =
        serde_json::from_str(&pending_json).expect("DuckDB -json emits a JSON array");
    let pending_rows = pending.as_array().expect("array");
    assert_eq!(pending_rows.len(), 1, "the pending record is present: {pending_json}");
    assert_eq!(pending_rows[0]["state"], "pending", "derived pending: {pending_json}");
    assert_eq!(
        pending_rows[0]["witnessed_at"],
        serde_json::Value::Null,
        "pending has NO witness → witnessed_at is SQL NULL in the DuckDB result: {pending_json}"
    );
}

/// The write-half proven on the REAL binary, into DuckDB. Drives the ACTUAL
/// `qd send` origin path to a GENUINE terminal (a `failed{wake}` — an unwakeable
/// unknown-provider row: deterministic + hermetic, the same fast path
/// `verbs_a4::send_cold_target_wakes_and_is_not_refused_as_stopped` uses), then
/// pipes that real, binary-written terminal through `qd dispositions` → DuckDB.
/// This closes the "seeded delivered" gap: at least one arm of the centerpiece
/// exercises the genuine write-then-deliver code path (envelope appended to
/// log.jsonl BEFORE the attempt + a terminal appended after) end-to-end into the
/// DuckDB join.
#[test]
fn roundtrip_real_qd_send_failed_wake_terminal_flows_into_duckdb() {
    if !duckdb_present() {
        eprintln!("SKIP roundtrip_real_qd_send_...duckdb: DuckDB CLI absent at {DUCKDB}.");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());

    // An UNWAKEABLE cold target: a live-registry row with an unknown provider.
    // `qd send` ACCEPTS it (resume-and-deliver), attempts the wake, the wake
    // fails immediately (no headless revive for an unknown provider) → a REAL
    // failed{wake} terminal is written, WITHOUT a live carrier. The envelope is
    // logged FIRST (write-then-deliver).
    let row = r#"{"pid":90099,"sessionId":"mystery-cold-acc","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"cold","name":"accwk","version":"0.1.0","provider":"mystery"}"#;
    std::fs::write(j.sessions.join("90099.json"), row).unwrap();

    // Drive the REAL `qd send` (origin mode) — the actual write-then-deliver path.
    let out = Command::new(qd_bin())
        .args(["send", "accwk", "please ack"])
        .env("HOME", &j.home)
        .env_remove("QD_HOME")
        .env("ZMX_DIR", &j.zmx)
        .output()
        .expect("spawn qd send");
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(code, 12, "unwakeable cold target → real failed{{wake}} exit 12 (stderr: {stderr})");
    assert!(stderr.contains("failed{wake}"), "the real send stamped failed{{wake}}: {stderr}");

    // The REAL binary wrote an envelope into log.jsonl (write-then-deliver) + a
    // `failed` terminal (reason wake) into dispositions.jsonl.
    let root = dispatch_root(&j.home);
    let log_body = std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default();
    let disp_body = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default();
    assert!(
        log_body.contains("mystery-cold-acc") || log_body.contains("accwk"),
        "the real send logged the envelope BEFORE the wake, got log.jsonl: {log_body:?}"
    );
    assert!(
        disp_body.contains("\"state\":\"failed\"") && disp_body.contains("\"reason\":\"wake\""),
        "the real send wrote a failed{{wake}} terminal, got dispositions.jsonl: {disp_body:?}"
    );

    // Pull the real correlation_id out of the envelope the binary minted (a 26-char
    // ULID) so the DuckDB assertion keys on the ACTUAL id qd wrote, not a fixture.
    let env_val: serde_json::Value = serde_json::from_str(log_body.lines().next().unwrap())
        .expect("the logged envelope is valid JSON");
    let real_id = env_val["correlation_id"].as_str().expect("correlation_id string").to_string();

    // The §6 round-trip on the REAL terminal: `qd dispositions` → DuckDB join.
    // Assert the failed record (state=failed, reason=wake, non-null witnessed_at)
    // for the id qd actually minted.
    let failed_json = duckdb_over_pipe(
        &j.home,
        &[],
        &format!(
            "SELECT correlation_id, state, reason, witnessed_at \
             FROM read_ndjson_auto('/dev/stdin') \
             WHERE correlation_id = '{real_id}'"
        ),
    );
    let failed: serde_json::Value = serde_json::from_str(&failed_json).expect("json array");
    let rows = failed.as_array().expect("array");
    assert_eq!(rows.len(), 1, "the real terminal joins over the pipe: {failed_json}");
    assert_eq!(rows[0]["state"], "failed", "the real terminal is failed: {failed_json}");
    assert_eq!(rows[0]["reason"], "wake", "carrying reason wake: {failed_json}");
    assert!(
        rows[0]["witnessed_at"].is_i64(),
        "a witnessed terminal has a non-null witnessed_at: {failed_json}"
    );
}

/// The non-DuckDB links of the #1 chain, always run (no DuckDB gate) so the
/// log→disposition→`qd dispositions` legs are covered even on a host without
/// DuckDB. (The DuckDB leg itself is `roundtrip_..._duckdb_join` above.)
#[test]
fn roundtrip_chain_links_without_duckdb() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let root = dispatch_root(&j.home);
    let far_future = 8_000_000_000_000i64;
    let id = "01CHAINLINKSNODUCKDBAAAAAA";
    write_lines(&root.join("log.jsonl"), &[&log_row(id, 1_700_000_000_000, far_future)]);
    write_lines(
        &root.join("dispositions.jsonl"),
        &[&disp_delivered(id, 1_700_000_000_000, 1_700_000_000_500)],
    );

    let (code, stdout, stderr) = qd_dispositions_stdout(&j.home, &[id]);
    assert_eq!(code, 0, "point query exit 0 (stderr: {stderr})");
    let rec: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("the point-query record is one JSON line");
    assert_eq!(rec["correlation_id"], id);
    assert_eq!(rec["state"], "delivered", "log⟕disposition projects delivered");
    assert_eq!(rec["witnessed_at"], 1_700_000_000_500i64, "witnessed terminal time carried through");
}

// ===========================================================================
// DEMONSTRATION #2 — INBOUND IDEMPOTENCE (same payload twice → one delivery,
// one no-op success; dispositions.jsonl EXACTLY ONE terminal; log.jsonl EMPTY).
//
// The canonical §6 idempotence assertion in one place. Mirrors
// inbound_mode.rs's approach: an origin-minted envelope FILE admitted at the
// inbound door twice. The target is an UNWAKEABLE cold row so the FIRST inbound
// wakes → fails → stamps exactly ONE `failed{wake}` TERMINAL without a live
// carrier (fast, hermetic); the SECOND inbound of the SAME id hits the
// idempotency key (a terminal is already present) → no-op success exit 0. That
// the terminal is `failed{wake}` not `delivered` is immaterial to idempotence:
// ANY terminal for the id wins (format doc §2 "First terminal wins"). Inbound
// NEVER appends to its own log.jsonl (a peer's envelope lives in the mirror).
// ===========================================================================

#[test]
fn inbound_same_payload_twice_is_one_delivery_one_noop_never_logs() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let root = dispatch_root(&j.home);

    // Unwakeable cold target: resolves (sole name match), wake fails → one
    // failed{wake} terminal on the FIRST inbound.
    let row = r#"{"pid":92002,"sessionId":"inbound-acc-cold","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"cold","name":"accinbwk","version":"0.1.0","provider":"mystery"}"#;
    std::fs::write(j.sessions.join("92002.json"), row).unwrap();

    let cid = "01ACCEPTIDEMPOTENCEAAAAAAA";
    let envelope = format!(
        r#"{{"v":1,"correlation_id":"{cid}","authored_at":{a},"expires_at":{e},"target":"accinbwk","authority":"peerhost","body":"idempotent payload"}}"#,
        a = now_ms(),
        e = now_ms() + 3_600_000,
    );
    let env_path = j.home.join("acc-inbound.json");
    std::fs::write(&env_path, &envelope).unwrap();

    let run_inbound = |home: &Path| -> (i32, String) {
        let out = Command::new(qd_bin())
            .args(["send", "--inbound-envelope", env_path.to_str().unwrap()])
            .env("HOME", home)
            .env_remove("QD_HOME")
            .env("ZMX_DIR", &j.zmx)
            .output()
            .expect("spawn qd send --inbound-envelope");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // FIRST inbound: wakes, fails, stamps EXACTLY ONE failed{wake} terminal.
    let (code1, err1) = run_inbound(&j.home);
    assert_eq!(code1, 12, "first inbound (unwakeable) → failed{{wake}} exit 12 (stderr: {err1})");
    assert!(err1.contains("failed{wake}"), "first inbound stamped a terminal, got: {err1}");

    let disps1 = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default();
    let log1 = std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default();
    let terminals1 = disps1.lines().filter(|l| l.contains(cid)).count();
    assert_eq!(terminals1, 1, "exactly ONE terminal after the first delivery, got: {disps1:?}");
    assert!(log1.is_empty(), "INBOUND never appends to its own log.jsonl, got: {log1:?}");

    // SECOND inbound of the SAME payload: the idempotency key (a terminal already
    // present for this id) ⇒ NO-OP SUCCESS. No second delivery, no second
    // terminal, log still empty.
    let (code2, err2) = run_inbound(&j.home);
    assert_eq!(code2, 0, "second inbound of the same id → no-op SUCCESS exit 0 (stderr: {err2})");
    assert!(
        err2.contains(cid) && (err2.contains("no-op") || err2.contains("already")),
        "the no-op prints a brief already-witnessed note, got: {err2}"
    );

    let disps2 = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default();
    let log2 = std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default();
    let terminals2 = disps2.lines().filter(|l| l.contains(cid)).count();
    assert_eq!(terminals2, 1, "STILL exactly one terminal after the no-op (no second stamp), got: {disps2:?}");
    assert_eq!(disps1, disps2, "the no-op appends NOTHING to dispositions.jsonl (byte-unchanged)");
    assert!(log2.is_empty(), "log.jsonl still empty after the no-op, got: {log2:?}");
}

// ===========================================================================
// DEMONSTRATION #3 — DOOR REFUSALS WITH NAMED REASONS (exact
// `qd send: <family>{<class>}:` stderr + exit 12). One canonical place for the
// §6 named-refusal bar: malformed payload, past-expiry, ambiguous target.
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

/// Past-expiry payload → `expired{past-expiry}` exit 12, refused at the door;
/// nothing is stamped `expired` (expired is a DERIVED view state, never authored).
#[test]
fn door_past_expiry_is_expired_past_expiry_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let envelope = format!(
        r#"{{"v":1,"correlation_id":"01ACCPASTEXPIRYAAAAAAAAAAA","authored_at":{a},"expires_at":{e},"target":"accwk","authority":"peerhost","body":"stale"}}"#,
        a = now_ms(),
        e = now_ms() - 60_000, // strictly in the past.
    );
    let (code, err, log, disps) = run_inbound_file(&j, &envelope);
    assert_eq!(code, 12, "past-expiry → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: expired{past-expiry}:"),
        "the named reason is expired{{past-expiry}}, got: {err}"
    );
    assert!(
        log.is_empty() && disps.is_empty(),
        "past-expiry is a DOOR refusal — nothing is stamped `expired`, got disps: {disps:?}"
    );
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

    let envelope = format!(
        r#"{{"v":1,"correlation_id":"01ACCAMBIGUOUSAAAAAAAAAAAA","authored_at":{a},"expires_at":{e},"target":"acctwin","authority":"peerhost","body":"hi"}}"#,
        a = now_ms(),
        e = now_ms() + 3_600_000,
    );
    let (code, err, log, disps) = run_inbound_file(&j, &envelope);
    assert_eq!(code, 12, "ambiguous target → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{ambiguous}:"),
        "the named reason is refused{{ambiguous}} (never first-match), got: {err}"
    );
    assert!(
        err.contains("matches 2 sessions"),
        "the refusal names the collision, got: {err}"
    );
    assert!(log.is_empty() && disps.is_empty(), "an ambiguity refusal touches no state");
}
