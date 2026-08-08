//! Integration tests for `qd send --inbound-envelope <path|->` (qd–qf transition
//! W4, "THE ONE DOOR", reworked to the R8 disposition-event-log model), driving
//! the REAL `qd` binary against a JAILED, empty HOME (L9a — never the real home;
//! HOME points into a per-test tempdir).
//!
//! Inbound mode admits a peer's ALREADY-minted envelope at the door: qd validates
//! it (malformed / past-expiry / mis-addressed / ambiguous refusals), is IDEMPOTENT
//! on a `delivered` EVENT EXISTING for the envelope's `correlation_id` (R8 — a
//! replayed already-delivered envelope no-ops with NO new rows, not even
//! `accepted`; a `delivery-failed` row does NOT block a retry), and
//! (resume-and-)delivers WITHOUT ever appending to its own `log.jsonl` (the
//! own-origin log is for envelopes qd ORIGINATED; a peer's envelope lives in the
//! mirror). These pin the bin wiring end-to-end:
//!   - acceptance #2 IDEMPOTENCE: a delivered event present ⇒ replay is a no-op
//!     success ("already delivered — no-op", exit 0, dispositions byte-unchanged);
//!   - THE R8 DISCRIMINATOR: a prior `delivery-failed` row does NOT no-op the
//!     door — the envelope is re-admitted (a fresh `accepted` row lands);
//!   - the inbound not-live FUNNEL: accepted → attempted → queued →
//!     delivery-failed{wake} rows, in file order, `log.jsonl` empty throughout;
//!   - acceptance #3 DOOR REFUSALS: malformed / v:2 / past-expiry / unknown /
//!     ambiguous, each with the exact `qd send: <family>{<class>}:` stderr + exit 12
//!     (refusals stamp NOTHING — only an ADMITTED envelope stamps `accepted`);
//!   - stdin (`-`) source; and origin-mode preservation (the two modes are
//!     mutually exclusive; a mixed invocation is a sync arg refusal).
//!
//! The funnel probes ride an UNWAKEABLE (unknown-provider) cold target: the
//! inbound door accepts, attempts, queues, then the wake fails → a
//! `delivery-failed{wake}` EVENT — hermetic and fast, no live carrier. Under R8
//! that failure is HISTORY, not a verdict: only a `delivered` event no-ops a
//! replay.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// The qd data root under a jailed HOME: `<home>/.quorum/dispatch` (QD_HOME
/// unset). Transport files (`log.jsonl`, `dispositions.jsonl`) live directly under
/// it (format doc: not under `state/`).
fn dispatch_root(home: &Path) -> PathBuf {
    home.join(".quorum").join("dispatch")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A jailed HOME under `dir` with the `.claude/sessions` registry dir + an empty
/// zmx dir created. Never the real home (the L9a guard the other suites assert).
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
    let real = std::env::var("HOME").unwrap_or_default();
    assert_ne!(
        home.to_string_lossy(),
        real,
        "jailed HOME must not be the real HOME"
    );
    Jail { home, sessions, zmx }
}

/// A valid v1 inbound envelope JSON (byte-exact wire key order per the format
/// doc §1: `v, correlation_id, authored_at, expires_at, target, origin, body`).
/// `expires_at` is set well in the future so the past-expiry door is not hit
/// unless a test deliberately wants it. Returns (json, authored_at) so a test
/// can seed event rows sharing the envelope's origin timeline.
fn envelope_json_at(correlation_id: &str, target: &str, body: &str, expires_at: i64) -> (String, i64) {
    let authored = now_ms();
    (
        format!(
            r#"{{"v":1,"correlation_id":"{correlation_id}","authored_at":{authored},"expires_at":{expires_at},"target":"{target}","origin":"peerhost","body":"{body}"}}"#
        ),
        authored,
    )
}

fn envelope_json(correlation_id: &str, target: &str, body: &str, expires_at: i64) -> String {
    envelope_json_at(correlation_id, target, body, expires_at).0
}

/// Run `qd send --inbound-envelope <arg> [extra...]` in the jail, feeding
/// `stdin_bytes` on stdin (for the `-` sentinel). Returns (exit, stdout, stderr,
/// log.jsonl body, dispositions.jsonl body).
fn run_inbound(
    j: &Jail,
    envelope_arg: &str,
    extra: &[&str],
    stdin_bytes: Option<&[u8]>,
) -> (i32, String, String, String, String) {
    let mut args = vec!["send", "--inbound-envelope", envelope_arg];
    args.extend_from_slice(extra);

    let mut cmd = Command::new(qd_bin());
    cmd.args(&args)
        .env_remove("QD_HOME")
        .env_remove("QD_HOST") // local_host is the "local" v1 placeholder
        .env("HOME", &j.home)
        .env("ZMX_DIR", &j.zmx)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_bytes.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().expect("spawn qd");
    if let Some(bytes) = stdin_bytes {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(bytes)
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait qd");

    let root = dispatch_root(&j.home);
    let log = std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default();
    let disps = std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        log,
        disps,
    )
}

/// Write an envelope file under the jail's home and return its path (as a String,
/// since the CLI arg is a path string).
fn envelope_file(j: &Jail, name: &str, contents: &str) -> String {
    let p = j.home.join(name);
    std::fs::write(&p, contents).unwrap();
    p.to_string_lossy().into_owned()
}

/// Seed raw JSONL event rows into the jail's `dispositions.jsonl` (byte-exact §2
/// wire lines — what a prior qd invocation would have appended).
fn seed_dispositions(j: &Jail, lines: &[&str]) {
    let root = dispatch_root(&j.home);
    std::fs::create_dir_all(&root).unwrap();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(root.join("dispositions.jsonl"), body).unwrap();
}

/// A seeded `delivered` EVENT row (format doc §2 key order) — witness "local"
/// (this qd's v1 placeholder host id), origin "peerhost" (the envelope's).
fn seeded_delivered(cid: &str, witnessed: i64, authored: i64) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{cid}","event":"delivered","witnessed_at":{witnessed},"witness":"local","origin":"peerhost","authored_at":{authored}}}"#
    )
}

/// A seeded `delivery-failed` EVENT row (reason REQUIRED, last on the wire).
fn seeded_failed(cid: &str, witnessed: i64, authored: i64, reason: &str) -> String {
    format!(
        r#"{{"v":1,"correlation_id":"{cid}","event":"delivery-failed","witnessed_at":{witnessed},"witness":"local","origin":"peerhost","authored_at":{authored},"reason":"{reason}"}}"#
    )
}

/// Parse a dispositions.jsonl body into (event, correlation_id, value) triples.
fn parse_event_rows(disps: &str) -> Vec<serde_json::Value> {
    disps
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad event row {l:?}: {e}")))
        .collect()
}

/// Forge one registry row `<pid>.json` for a session named `name`, provider
/// `provider`, at `session_id`, with the given `status`, using a REAL live `pid`
/// (so the resolver's pid-aware liveness sees it as alive when needed).
fn write_row(j: &Jail, pid: i64, session_id: &str, name: &str, provider: &str, status: &str) {
    let row = format!(
        r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"{status}","name":"{name}","version":"0.1.0","provider":"{provider}"}}"#
    );
    std::fs::write(j.sessions.join(format!("{pid}.json")), row).unwrap();
}

// ===========================================================================
// Acceptance #2 — IDEMPOTENCE keys on a `delivered` EVENT EXISTING (R8).
// ===========================================================================

/// A delivered event already present for the envelope's id ⇒ the door NO-OPS:
/// exit 0, "already delivered — no-op" on stderr, and the dispositions file is
/// byte-UNCHANGED — no fresh `accepted`, no attempt, nothing. Delivery is
/// irreversible; replaying the envelope must not double-deliver.
#[test]
fn inbound_already_delivered_envelope_noops_with_no_new_rows() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    // The target must RESOLVE (the door resolves before the idempotency probe);
    // an unwakeable cold row suffices — it is never woken on the no-op path.
    write_row(&j, 91001, "inbound-cold-1", "inbwk", "mystery", "cold");

    let cid = "01INBOUNDIDEMPOTENCEAAAAAA";
    let (env, authored) = envelope_json_at(cid, "inbwk", "hello from a peer", now_ms() + 3_600_000);
    let path = envelope_file(&j, "env.json", &env);

    // Seed the delivered fact (the retry's success, as a prior invocation would
    // have stamped it).
    let delivered_row = seeded_delivered(cid, now_ms(), authored);
    seed_dispositions(&j, &[&delivered_row]);
    let before = std::fs::read_to_string(dispatch_root(&j.home).join("dispositions.jsonl")).unwrap();

    // Present the SAME envelope: delivered-event-exists ⇒ NO-OP SUCCESS.
    let (code, _out, err, log, disps) = run_inbound(&j, &path, &[], None);
    assert_eq!(code, 0, "already-delivered replay → no-op SUCCESS exit 0 (stderr: {err})");
    assert!(
        err.contains(cid) && err.contains("already delivered — no-op"),
        "the no-op names the id + the already-delivered fact, got: {err}"
    );
    assert!(log.is_empty(), "INBOUND never appends to its own log.jsonl, got: {log:?}");
    assert_eq!(
        disps, before,
        "the no-op appends NOTHING — not even a fresh `accepted` (byte-unchanged)"
    );
    assert_eq!(
        parse_event_rows(&disps).len(),
        1,
        "row count unchanged across the replay"
    );
}

/// THE R8 DISCRIMINATOR ("first terminal wins" is DEAD): a prior
/// `delivery-failed` row for the id does NOT no-op the door. Presenting the
/// envelope again PROCEEDS — a fresh `accepted` row proves admission, the
/// attempt re-runs (attempted → queued → delivery-failed{wake} on this
/// unwakeable target), and the pre-existing failure row is untouched history.
#[test]
fn inbound_prior_delivery_failed_event_does_not_block_readmission() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    write_row(&j, 91002, "inbound-cold-2", "inbwk2", "mystery", "cold");

    let cid = "01R8DISCRIMINATORAAAAAAAAA";
    let (env, authored) = envelope_json_at(cid, "inbwk2", "retry me", now_ms() + 3_600_000);
    let path = envelope_file(&j, "env-retry.json", &env);

    // Seed ONLY a delivery-failed row for the id (the failed first attempt).
    let failed_row = seeded_failed(cid, now_ms() - 1_000, authored, "wake");
    seed_dispositions(&j, &[&failed_row]);

    // Present the envelope: the door must PROCEED (no no-op). With the target
    // unwakeable the fresh funnel drives to delivery-failed{wake} again — the
    // point is ADMISSION, not the outcome.
    let (code, _out, err, log, disps) = run_inbound(&j, &path, &[], None);
    assert!(
        !err.contains("already delivered"),
        "a delivery-failed row must NOT trigger the no-op, got: {err}"
    );
    assert_eq!(
        code, 12,
        "the re-admitted attempt drove to failed{{wake}} (proceeded, not no-opped) (stderr: {err})"
    );
    assert!(err.contains("failed{wake}"), "the fresh attempt's outcome, got: {err}");
    assert!(log.is_empty(), "inbound still never logs, got: {log:?}");

    // The pre-existing failure row is intact AND a fresh funnel landed after it:
    // [seeded delivery-failed] + accepted, attempted, queued, delivery-failed.
    let rows = parse_event_rows(&disps);
    assert!(rows.iter().all(|r| r["correlation_id"] == cid));
    let kinds: Vec<&str> = rows.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["delivery-failed", "accepted", "attempted", "queued", "delivery-failed"],
        "fresh admission funnel appended AFTER the seeded failure, got: {disps:?}"
    );
    assert_eq!(
        rows[1]["event"], "accepted",
        "a FRESH accepted row proves the door admitted the replay"
    );
}

/// The inbound not-live FUNNEL on a clean store: one presentation stamps
/// accepted → attempted → queued → delivery-failed{wake}, in file order, each
/// row carrying witness "local" (this host) + origin "peerhost" (the envelope's
/// — the R9/N10 split), reason ONLY on the delivery-failed row. log.jsonl stays
/// empty (inbound never appends to its own origin log).
#[test]
fn inbound_first_presentation_stamps_the_full_funnel_and_never_logs() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    write_row(&j, 91003, "inbound-cold-3", "inbwk3", "mystery", "cold");

    let cid = "01INBOUNDFUNNELAAAAAAAAAAA";
    let env = envelope_json(cid, "inbwk3", "hello funnel", now_ms() + 3_600_000);
    let path = envelope_file(&j, "env-funnel.json", &env);

    let (code, _out, err, log, disps) = run_inbound(&j, &path, &[], None);
    assert_eq!(code, 12, "unwakeable inbound → failed{{wake}} exit 12 (stderr: {err})");
    assert!(err.contains("failed{wake}"), "stderr: {err}");
    assert!(log.is_empty(), "INBOUND never appends to its own log.jsonl, got: {log:?}");

    let rows = parse_event_rows(&disps);
    let kinds: Vec<&str> = rows.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["accepted", "attempted", "queued", "delivery-failed"],
        "the inbound not-live funnel, in file order, got: {disps:?}"
    );
    for r in &rows {
        assert_eq!(r["correlation_id"], cid, "every row keys on the envelope's id");
        assert_eq!(r["witness"], "local", "witness = THIS host (v1 placeholder)");
        assert_eq!(r["origin"], "peerhost", "origin = the ENVELOPE's origin (the peer)");
        if r["event"] == "delivery-failed" {
            assert_eq!(r["reason"], "wake", "reason REQUIRED on delivery-failed");
        } else {
            assert!(r.get("reason").is_none(), "reason FORBIDDEN elsewhere: {r}");
        }
    }
}

// ===========================================================================
// Acceptance #3 — DOOR REFUSALS (exact `qd send: <family>{<class>}:` + exit 12).
// A refusal stamps NOTHING — only an ADMITTED envelope stamps `accepted`.
// ===========================================================================

/// Malformed bytes (not JSON) ⇒ refused{malformed} exit 12. Refused at the door,
/// BEFORE resolve — needs no session.
#[test]
fn inbound_malformed_bytes_are_refused_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let path = envelope_file(&j, "bad.json", "this is not json at all {");
    let (code, _out, err, log, disps) = run_inbound(&j, &path, &[], None);
    assert_eq!(code, 12, "malformed bytes → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{malformed}:"),
        "expected the refused{{malformed}} render, got: {err}"
    );
    assert!(log.is_empty() && disps.is_empty(), "a door refusal stamps/logs nothing");
}

/// An OLD-shape envelope (pre-R9 `authority` key instead of `origin`) is now
/// MALFORMED: serde requires `origin`, so the door refuses rather than guessing
/// the rename. Pins the fixture-migration edge.
#[test]
fn inbound_old_authority_shape_is_refused_malformed() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let env = format!(
        r#"{{"v":1,"correlation_id":"01OLDSHAPEAAAAAAAAAAAAAAAA","authored_at":{a},"expires_at":{e},"target":"inbwk","authority":"peerhost","body":"hi"}}"#,
        a = now_ms(),
        e = now_ms() + 3_600_000,
    );
    let path = envelope_file(&j, "oldshape.json", &env);
    let (code, _out, err, log, disps) = run_inbound(&j, &path, &[], None);
    assert_eq!(code, 12, "old authority-shape → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{malformed}:"),
        "the pre-rename shape (missing `origin`) is malformed, got: {err}"
    );
    assert!(log.is_empty() && disps.is_empty(), "a door refusal stamps/logs nothing");
}

/// A structurally-valid envelope that DECLARES `v:2` ⇒ refused{malformed} (never
/// guess a version). This is the "unsupported version" limb of the malformed door.
#[test]
fn inbound_v2_envelope_is_refused_malformed_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    // Every required field present, only `v` is 2.
    let env = format!(
        r#"{{"v":2,"correlation_id":"01V2AAAAAAAAAAAAAAAAAAAAAA","authored_at":{a},"expires_at":{e},"target":"inbwk","origin":"peerhost","body":"hi"}}"#,
        a = now_ms(),
        e = now_ms() + 3_600_000,
    );
    let path = envelope_file(&j, "v2.json", &env);
    let (code, _out, err, _log, _disps) = run_inbound(&j, &path, &[], None);
    assert_eq!(code, 12, "v:2 → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{malformed}:"),
        "a v:2 envelope is malformed (never guess a version), got: {err}"
    );
    assert!(
        err.contains("version 2") || err.contains("v1"),
        "the refusal names the unsupported version, got: {err}"
    );
}

/// A missing REQUIRED field (serde reject) ⇒ refused{malformed}. Confirms the door
/// does not silently default a field.
#[test]
fn inbound_missing_required_field_is_refused_malformed() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    // `target` omitted entirely.
    let env = format!(
        r#"{{"v":1,"correlation_id":"01MISSINGTGTAAAAAAAAAAAAAA","authored_at":{a},"expires_at":{e},"origin":"peerhost","body":"hi"}}"#,
        a = now_ms(),
        e = now_ms() + 3_600_000,
    );
    let path = envelope_file(&j, "missing.json", &env);
    let (code, _out, err, _log, _disps) = run_inbound(&j, &path, &[], None);
    assert_eq!(code, 12, "missing field → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{malformed}:"),
        "a missing required field is malformed, got: {err}"
    );
}

/// A past-expiry envelope ⇒ expired{past-expiry} exit 12, REFUSED at the door (not
/// stamped `expired`). Checked BEFORE resolve, so it needs no session; and nothing
/// is stamped (expired is a DERIVED view state, never authored — there is no
/// expired EVENT type at all).
#[test]
fn inbound_past_expiry_is_refused_at_the_door_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    // expires_at strictly in the past.
    let env = envelope_json("01PASTEXPIRYAAAAAAAAAAAAAA", "inbwk", "stale", now_ms() - 60_000);
    let path = envelope_file(&j, "expired.json", &env);
    let (code, _out, err, log, disps) = run_inbound(&j, &path, &[], None);
    assert_eq!(code, 12, "past-expiry → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: expired{past-expiry}:"),
        "expected the expired{{past-expiry}} render, got: {err}"
    );
    assert!(
        log.is_empty() && disps.is_empty(),
        "past-expiry is a DOOR refusal — no event stamped (not even accepted), got disps: {disps:?}"
    );
}

/// An unknown target ⇒ refused{unknown} exit 12 (empty registry ⇒ no match).
#[test]
fn inbound_unknown_target_is_refused_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    // No rows written → the resolver finds nothing.
    let env = envelope_json("01UNKNOWNTGTAAAAAAAAAAAAAA", "ghost", "hi", now_ms() + 3_600_000);
    let path = envelope_file(&j, "unknown.json", &env);
    let (code, _out, err, _log, disps) = run_inbound(&j, &path, &[], None);
    assert_eq!(code, 12, "unknown target → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{unknown}:"),
        "expected the refused{{unknown}} render, got: {err}"
    );
    // Genuinely unknown (a resolver MISS), NOT a store-gather failure masquerading
    // as unknown — assert the resolver-miss reason so the door is proven to have
    // reached the resolver on an empty registry.
    assert!(
        err.contains("no session matching \"ghost\""),
        "the refusal is a genuine resolver miss (not a store-unavailable fallback), got: {err}"
    );
    assert!(disps.is_empty(), "a resolve refusal stamps nothing (not even accepted)");
}

/// An ambiguous target (two GENUINELY-LIVE sessions sharing one name) ⇒
/// refused{ambiguous} exit 12 — never first-match. Both rows carry THIS test
/// process's live pid so the resolver's pid-aware liveness sees both as alive
/// (two dead-pid rows would collapse to unknown, not ambiguous).
#[test]
fn inbound_ambiguous_target_is_refused_exit_12() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let live_pid = std::process::id() as i64; // the test runner — definitely alive.
    // Two DISTINCT sessionIds, SAME name, both idle + live-pid ⇒ Resolution::Many.
    write_row(&j, live_pid, "ambi-session-A", "twin", "mystery", "idle");
    // Second row under a different filename/sessionId but the same live pid + name.
    let row_b = format!(
        r#"{{"pid":{live_pid},"sessionId":"ambi-session-B","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"twin","version":"0.1.0","provider":"mystery"}}"#
    );
    std::fs::write(j.sessions.join("ambi-b.json"), row_b).unwrap();

    let env = envelope_json("01AMBIGUOUSAAAAAAAAAAAAAAA", "twin", "hi", now_ms() + 3_600_000);
    let path = envelope_file(&j, "ambi.json", &env);
    let (code, _out, err, _log, disps) = run_inbound(&j, &path, &[], None);
    assert_eq!(code, 12, "ambiguous target → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{ambiguous}:"),
        "expected the refused{{ambiguous}} render (never first-match), got: {err}"
    );
    assert!(
        err.contains("matches 2 sessions"),
        "the refusal names the collision (two live same-name rows), got: {err}"
    );
    assert!(disps.is_empty(), "an ambiguity refusal stamps nothing");
}

// ===========================================================================
// stdin source (`--inbound-envelope -`).
// ===========================================================================

/// `--inbound-envelope -` reads the envelope from STDIN. Proven via a past-expiry
/// envelope on stdin (a door refusal that needs no session but does require the
/// bytes to have been read + parsed off stdin).
#[test]
fn inbound_reads_envelope_from_stdin_sentinel() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let env = envelope_json("01STDINPASTEXPIRYAAAAAAAAA", "inbwk", "via stdin", now_ms() - 60_000);
    let (code, _out, err, _log, _disps) = run_inbound(&j, "-", &[], Some(env.as_bytes()));
    assert_eq!(code, 12, "stdin past-expiry → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: expired{past-expiry}:"),
        "the envelope was read + parsed off stdin, got: {err}"
    );
}

/// A malformed envelope on stdin is ALSO refused{malformed} — the `-` path routes
/// the same door as a file path.
#[test]
fn inbound_stdin_malformed_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let (code, _out, err, _log, _disps) =
        run_inbound(&j, "-", &[], Some(b"{ not an envelope"));
    assert_eq!(code, 12, "stdin malformed → exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{malformed}:"),
        "stdin malformed routes the same malformed door, got: {err}"
    );
}

// ===========================================================================
// Origin-mode preservation — the two modes are mutually exclusive.
// ===========================================================================

/// `--inbound-envelope` + origin positionals ⇒ a clear SYNC arg refusal
/// (refused{args} exit 12). The envelope carries the address + body; passing them
/// again is a contradiction the door names.
#[test]
fn inbound_with_positionals_is_an_arg_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let env = envelope_json("01ARGSAAAAAAAAAAAAAAAAAAAA", "inbwk", "hi", now_ms() + 3_600_000);
    let path = envelope_file(&j, "args.json", &env);
    // Pass BOTH the inbound envelope AND a <target> <message>.
    let (code, _out, err, log, disps) = run_inbound(&j, &path, &["extratarget", "extramsg"], None);
    assert_eq!(code, 12, "inbound + positionals → arg refusal exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{args}:"),
        "expected the refused{{args}} sync refusal, got: {err}"
    );
    assert!(
        log.is_empty() && disps.is_empty(),
        "an arg refusal touches no state, got log: {log:?} disps: {disps:?}"
    );
}

/// `--inbound-envelope` + `--expires` ⇒ arg refusal (an inbound envelope carries
/// its own expires_at; --expires is origin-mode only).
#[test]
fn inbound_with_expires_flag_is_an_arg_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let env = envelope_json("01EXPFLAGAAAAAAAAAAAAAAAAA", "inbwk", "hi", now_ms() + 3_600_000);
    let path = envelope_file(&j, "expflag.json", &env);
    let (code, _out, err, _log, _disps) = run_inbound(&j, &path, &["--expires", "30m"], None);
    // clap declares `--expires` on the send command, so it parses; the runtime mode
    // split rejects it in inbound mode.
    assert_eq!(code, 12, "inbound + --expires → arg refusal exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{args}:"),
        "expected the refused{{args}} sync refusal for --expires in inbound mode, got: {err}"
    );
}

/// Origin mode still reaches the resolver when `--inbound-envelope` is ABSENT: an
/// unknown-session origin send takes the ORIGIN path (not the inbound door). qd–qf
/// W6 ALIGNED origin's ambiguity/unknown family to the shared Refusal — an unknown
/// target is now `refused{unknown}` exit 12 (the SAME family the inbound door
/// renders), replacing the old resolve_or_die exit-1 "No session matching". This
/// stays a regression guard that the mode split routes origin to the resolver (the
/// resolver-miss reason text is preserved inside the refusal).
#[test]
fn origin_mode_unchanged_when_inbound_absent() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    let out = Command::new(qd_bin())
        .args(["send", "ghost", "body"])
        .env_remove("QD_HOME")
        .env("HOME", &j.home)
        .env("ZMX_DIR", &j.zmx)
        .output()
        .expect("spawn qd");
    let code = out.status.code().unwrap_or(-1);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(code, 12, "origin send to an unknown session → W6 refused{{unknown}} exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{unknown}:") && err.contains("no session matching \"ghost\""),
        "origin mode reaches the resolver (aligned refused{{unknown}}), got: {err}"
    );
}
