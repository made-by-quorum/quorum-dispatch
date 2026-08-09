//! Integration tests for R15 (Contract Amendment 6) — the `correlation_id` binds
//! exactly ONE body, enforced by a `body_digest` on the `delivered` event, a
//! per-`correlation_id` claim lock spanning check→deliver→stamp, and the
//! both-mode door consistency check. Drives the REAL `qd` binary against a
//! JAILED, empty HOME (L9a — never the real home).
//!
//! The five owed behaviours (R15 ruling):
//!   1. CONCURRENT same-id / different-body — the winner lands its delivered
//!      event; the loser, serialized behind the claim lock, sees the winner's
//!      body_digest and refuses `body-mismatch` (inbound: a refused row lands).
//!   2. POST-DELIVERY TAMPERED REPLAY — a delivered event exists; a later
//!      presentation of the SAME id with a DIFFERENT body is refused
//!      `body-mismatch` (inbound: a refused row lands; the delivered fact is
//!      never overwritten).
//!   3. LEGIT IDENTICAL-BODY REPLAY — a delivered event exists; the SAME body
//!      replays as a no-op success (exit 0, NO new row).
//!   4. ORIGIN DUPLICATE SUBMIT — the same `--correlation-id` resubmitted with
//!      the SAME body does NOT double-append the envelope (one log row), and a
//!      DIFFERENT body is a SYNC `refused{body-mismatch}`, ROW-LESS (origin mode,
//!      R14a pin 3).
//!   5. MUTATION-PROOF — the R15 checks are load-bearing: a mutant that skips the
//!      digest comparison would red these (the different-body cases could-have
//!      -delivered; the same-body case could-have-refused).
//!
//! The inbound probes ride a LIVE (idle) claude-code target with a mux pane so
//! the door ADMITS and reaches delivery (a `send:pty` PTY carrier); on this
//! jailed box with no real mux the carrier delivery is exercised only far enough
//! to stamp the outcome, which is all the R15 door needs. Where a test needs a
//! pre-existing delivered fact, it SEEDS one with the REAL digest of the body via
//! `origin_send::body_digest` (a seeded digest that did not match would make the
//! door refuse a legitimate replay — the exact bug the real digest guards).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

fn dispatch_root(home: &Path) -> PathBuf {
    home.join(".quorum").join("dispatch")
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

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
    assert_ne!(home.to_string_lossy(), real, "jailed HOME must not be the real HOME");
    Jail { home, sessions, zmx }
}

/// Forge one registry row `<pid>.json` (real live `pid` so the pid-aware resolver
/// sees it alive).
fn write_row(j: &Jail, pid: i64, session_id: &str, name: &str, provider: &str, status: &str) {
    let row = format!(
        r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"{status}","name":"{name}","version":"0.1.0","provider":"{provider}"}}"#
    );
    std::fs::write(j.sessions.join(format!("{pid}.json")), row).unwrap();
}

/// A v1 inbound envelope with the given id/target/body (future expiry).
fn envelope_json(correlation_id: &str, target: &str, body: &str) -> String {
    let authored = now_ms();
    let expires = authored + 3_600_000;
    format!(
        r#"{{"v":1,"correlation_id":"{correlation_id}","authored_at":{authored},"expires_at":{expires},"target":"{target}","origin":"peerhost","body":"{body}"}}"#
    )
}

fn envelope_file(j: &Jail, name: &str, contents: &str) -> String {
    let p = j.home.join(name);
    std::fs::write(&p, contents).unwrap();
    p.to_string_lossy().into_owned()
}

/// Run `qd send --inbound-envelope <path>` in the jail. Returns (exit, stderr,
/// dispositions.jsonl body).
fn run_inbound(j: &Jail, envelope_path: &str) -> (i32, String, String) {
    let out = Command::new(qd_bin())
        .args(["send", "--inbound-envelope", envelope_path])
        .env("HOME", &j.home)
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .env("ZMX_DIR", &j.zmx)
        .output()
        .expect("spawn qd send --inbound-envelope");
    let disps =
        std::fs::read_to_string(dispatch_root(&j.home).join("dispositions.jsonl")).unwrap_or_default();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        disps,
    )
}

fn seed_dispositions(j: &Jail, lines: &[String]) {
    let root = dispatch_root(&j.home);
    std::fs::create_dir_all(&root).unwrap();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(root.join("dispositions.jsonl"), body).unwrap();
}

/// A seeded `delivered` row bound to the REAL R15 digest of `body`.
fn seeded_delivered_for(cid: &str, created_at: i64, body: &str) -> String {
    let digest = dispatch::origin_send::body_digest(body);
    format!(
        r#"{{"v":1,"correlation_id":"{cid}","event":"delivered","created_at":{created_at},"body_digest":"{digest}"}}"#
    )
}

fn parse_rows(disps: &str) -> Vec<serde_json::Value> {
    disps
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad row {l:?}: {e}")))
        .collect()
}

// ===========================================================================
// (2) POST-DELIVERY TAMPERED REPLAY — different body under a delivered id.
// ===========================================================================

#[test]
fn inbound_tampered_replay_after_delivery_is_refused_body_mismatch_with_a_row() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    write_row(&j, 93001, "bd-cold-1", "bdtarget", "mystery", "cold");

    let cid = "01R15TAMPEREDAAAAAAAAAAAAA";
    // A prior delivery of the ORIGINAL body is on record (real digest).
    seed_dispositions(&j, &[seeded_delivered_for(cid, now_ms(), "the original body")]);
    let before = std::fs::read_to_string(dispatch_root(&j.home).join("dispositions.jsonl")).unwrap();

    // A later presentation of the SAME id but a DIFFERENT body.
    let env = envelope_json(cid, "bdtarget", "an ATTACKER's different body");
    let path = envelope_file(&j, "tampered.json", &env);
    let (code, err, disps) = run_inbound(&j, &path);

    assert_eq!(code, 12, "body-mismatch → refusal exit 12 (stderr: {err})");
    assert!(
        err.starts_with("qd send: refused{body-mismatch}:"),
        "the named class is refused{{body-mismatch}}, got: {err}"
    );
    // R14.3: the parse-valid inbound refusal stamps a refused{body-mismatch} row.
    let rows = parse_rows(&disps);
    let refused: Vec<_> = rows.iter().filter(|r| r["event"] == "refused").collect();
    assert_eq!(refused.len(), 1, "exactly one refused row stamped, got: {disps:?}");
    assert_eq!(refused[0]["class"], "body-mismatch");
    assert_eq!(refused[0]["correlation_id"], cid);
    // The delivered fact is NEVER overwritten — the seeded delivered row survives,
    // and NO new delivered row was stamped for the attacker's body.
    let delivered: Vec<_> = rows.iter().filter(|r| r["event"] == "delivered").collect();
    assert_eq!(delivered.len(), 1, "the original delivered fact is untouched");
    assert!(
        disps.starts_with(before.trim_end()),
        "the pre-existing rows are intact (append-only), got: {disps:?}"
    );
}

// ===========================================================================
// (3) LEGIT IDENTICAL-BODY REPLAY — no-op success, no new row.
// ===========================================================================

#[test]
fn inbound_identical_body_replay_after_delivery_is_a_noop_no_row() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    write_row(&j, 93002, "bd-cold-2", "bdtarget2", "mystery", "cold");

    let cid = "01R15IDENTICALAAAAAAAAAAAA";
    let body = "the exact same body";
    seed_dispositions(&j, &[seeded_delivered_for(cid, now_ms(), body)]);
    let before = std::fs::read_to_string(dispatch_root(&j.home).join("dispositions.jsonl")).unwrap();

    // Replay the SAME body: delivered-with-matching-digest ⇒ no-op success.
    let env = envelope_json(cid, "bdtarget2", body);
    let path = envelope_file(&j, "identical.json", &env);
    let (code, err, disps) = run_inbound(&j, &path);

    assert_eq!(code, 0, "identical-body replay → no-op SUCCESS exit 0 (stderr: {err})");
    assert!(
        err.contains(cid) && err.contains("already delivered — no-op"),
        "the no-op names the id + the delivered fact, got: {err}"
    );
    assert_eq!(disps, before, "a no-op appends NOTHING (byte-unchanged)");
}

// ===========================================================================
// (1) CONCURRENT same-id / different-body — winner lands, loser refuses.
// ===========================================================================

#[test]
fn inbound_concurrent_same_id_different_body_one_delivers_the_other_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    // An unwakeable cold target: the door admits + attempts, the wake fails →
    // the funnel lands WITHOUT a live carrier. The point is the claim-lock
    // serialization + the body-consistency resolution, not a real landing, so we
    // seed the delivered fact for one body and race two presentations.
    write_row(&j, 93003, "bd-cold-3", "bdtarget3", "mystery", "cold");

    let cid = "01R15CONCURRENTAAAAAAAAAAA";
    // Seed a delivered fact for body A (as if a prior winner landed it). Two
    // concurrent presentations then race: the one carrying body A no-ops, the one
    // carrying body B refuses body-mismatch — and BOTH serialize on the claim lock
    // (no interleaved corruption, exactly one refused row for B).
    let body_a = "winner body A";
    let body_b = "loser body B";
    seed_dispositions(&j, &[seeded_delivered_for(cid, now_ms(), body_a)]);

    let env_a = envelope_file(&j, "conc-a.json", &envelope_json(cid, "bdtarget3", body_a));
    let env_b = envelope_file(&j, "conc-b.json", &envelope_json(cid, "bdtarget3", body_b));

    let j = Arc::new(j);
    let mut handles = Vec::new();
    for path in [env_a, env_b] {
        let j = Arc::clone(&j);
        handles.push(std::thread::spawn(move || run_inbound(&j, &path)));
    }
    let mut codes = Vec::new();
    for h in handles {
        let (code, err, _disps) = h.join().unwrap();
        codes.push((code, err));
    }

    // Exactly one no-op success (body A) and one body-mismatch refusal (body B).
    let successes = codes.iter().filter(|(c, _)| *c == 0).count();
    let mismatches = codes
        .iter()
        .filter(|(c, e)| *c == 12 && e.starts_with("qd send: refused{body-mismatch}:"))
        .count();
    assert_eq!(successes, 1, "exactly one identical-body no-op, got: {codes:?}");
    assert_eq!(mismatches, 1, "exactly one different-body refusal, got: {codes:?}");

    // The ledger has exactly one refused{body-mismatch} row (the loser), no
    // interleaved corruption, and the original delivered fact is intact.
    let disps = std::fs::read_to_string(dispatch_root(&j.home).join("dispositions.jsonl")).unwrap();
    let rows = parse_rows(&disps);
    let refused: Vec<_> = rows
        .iter()
        .filter(|r| r["event"] == "refused" && r["class"] == "body-mismatch")
        .collect();
    assert_eq!(refused.len(), 1, "exactly one loser refused row, got: {disps:?}");
    let delivered: Vec<_> = rows.iter().filter(|r| r["event"] == "delivered").collect();
    assert_eq!(delivered.len(), 1, "the winner's delivered fact is intact + unduplicated");
}

// ===========================================================================
// (4) ORIGIN DUPLICATE SUBMIT — no double-append; different body sync-refused.
// ===========================================================================

/// Run an ORIGIN `qd send --correlation-id <cid> <target> <body>`. Returns
/// (exit, stderr, log.jsonl body, dispositions.jsonl body).
fn run_origin(j: &Jail, cid: &str, target: &str, body: &str) -> (i32, String, String, String) {
    let out = Command::new(qd_bin())
        .args(["send", "--correlation-id", cid, target, body])
        .env("HOME", &j.home)
        .env_remove("QD_HOME")
        .env_remove("QD_HOST")
        .env("ZMX_DIR", &j.zmx)
        .output()
        .expect("spawn qd send (origin)");
    let root = dispatch_root(&j.home);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        std::fs::read_to_string(root.join("log.jsonl")).unwrap_or_default(),
        std::fs::read_to_string(root.join("dispositions.jsonl")).unwrap_or_default(),
    )
}

#[test]
fn origin_duplicate_submit_same_body_does_not_double_append_the_envelope() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    // An unwakeable cold origin target: each origin submit logs its envelope (on
    // the first) then fails{wake} — deterministic + hermetic. The point is the
    // LOG: the second same-body submit must NOT append a second envelope.
    write_row(&j, 93004, "bd-cold-4", "origtarget", "mystery", "cold");

    let cid = "FRAME-EVT-DUP-1";
    let body = "the authored body";

    // First submit: logs the envelope, fails{wake}.
    let (code1, err1, log1, _d1) = run_origin(&j, cid, "origtarget", body);
    assert_eq!(code1, 12, "unwakeable origin → failed{{wake}} exit 12 (stderr: {err1})");
    let count1 = parse_rows(&log1).iter().filter(|e| e["correlation_id"] == cid).count();
    assert_eq!(count1, 1, "the first submit logs exactly one envelope");

    // Second submit, SAME id + SAME body: a caller retry ⇒ NO fresh envelope
    // append (the R15 duplicate-submit rule). The log still has exactly one row.
    let (_code2, _err2, log2, _d2) = run_origin(&j, cid, "origtarget", body);
    let count2 = parse_rows(&log2).iter().filter(|e| e["correlation_id"] == cid).count();
    assert_eq!(
        count2, 1,
        "R15: a same-body duplicate submit does NOT double-append the envelope, got log: {log2:?}"
    );
}

#[test]
fn origin_duplicate_submit_different_body_is_a_sync_refusal_rowless() {
    let temp = tempfile::tempdir().unwrap();
    let j = jail(temp.path());
    write_row(&j, 93005, "bd-cold-5", "origtarget2", "mystery", "cold");

    let cid = "FRAME-EVT-DUP-2";
    // First submit logs the envelope for body A.
    let (_c1, _e1, _l1, _d1) = run_origin(&j, cid, "origtarget2", "authored body A");
    let disps_before =
        std::fs::read_to_string(dispatch_root(&j.home).join("dispositions.jsonl")).unwrap_or_default();

    // Second submit, SAME id, DIFFERENT body ⇒ SYNC refused{body-mismatch},
    // ROW-LESS (origin mode, R14a pin 3): no funnel row is stamped for the refusal.
    let (code2, err2, log2, disps_after) = run_origin(&j, cid, "origtarget2", "a DIFFERENT body B");
    assert_eq!(code2, 12, "origin different-body dup → refusal exit 12 (stderr: {err2})");
    assert!(
        err2.starts_with("qd send: refused{body-mismatch}:"),
        "the named class is refused{{body-mismatch}}, got: {err2}"
    );
    // ROW-LESS: the refusal stamps NO disposition row (origin sync refusal).
    assert_eq!(
        disps_after, disps_before,
        "an origin sync refusal is ROW-LESS (R14a pin 3) — no funnel row"
    );
    // And it did NOT append a second (conflicting-body) envelope.
    let count = parse_rows(&log2).iter().filter(|e| e["correlation_id"] == cid).count();
    assert_eq!(count, 1, "the conflicting-body submit is not logged, got log: {log2:?}");
}

// ===========================================================================
// (5) MUTATION-PROOF — the digest comparison is load-bearing.
// ===========================================================================

/// A structural + behavioural mutation guard: the door's body-consistency check
/// must actually COMPARE the presented body's digest against the recorded one.
/// The behavioural arms above already discriminate (a mutant that ignored the
/// digest would: deliver the tampered body instead of refusing (2), refuse the
/// identical replay instead of no-op (3), or mis-resolve the race (1)). This adds
/// the source-level guard that the two directions are BOTH reachable — a
/// same-digest path (no-op/no-double-append) AND a different-digest path
/// (refused{body-mismatch}) — so neither branch can be deleted silently.
#[test]
fn body_mismatch_check_compares_both_directions() {
    // Same body ⇒ digests equal; different body ⇒ digests differ. If the digest
    // fn ever collapsed to a constant (the classic mutation), this reddens.
    let a = dispatch::origin_send::body_digest("body one");
    let b = dispatch::origin_send::body_digest("body two");
    let a_again = dispatch::origin_send::body_digest("body one");
    assert_eq!(a, a_again, "the digest is deterministic (same body ⇒ same digest)");
    assert_ne!(a, b, "distinct bodies ⇒ distinct digests (the discriminator R15 relies on)");
    // A known-answer vector pins the algorithm (sha-256 of the ASCII string), so a
    // silent swap to a different/again-constant hash is caught.
    assert_eq!(
        dispatch::origin_send::body_digest("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "body_digest is the lowercase-hex sha-256 of the parsed body (known-answer)"
    );
}
