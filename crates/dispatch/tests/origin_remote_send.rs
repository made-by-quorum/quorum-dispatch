//! Integration tests for ORIGIN-MODE REMOTE SEND (v0-cutover delta commission —
//! the last P5 gap), driving the REAL `qd` binary against a JAILED, empty HOME
//! (never the real home). A host-qualified `name@host` with host ≠ self AND fleet
//! state present (`remote/<host>/ls.json`) now:
//!   - resolves the name inside that host's MIRRORED namespace (the strict W7
//!     read): unknown ⇒ `refused{unknown}`, ambiguous ⇒ `refused{ambiguous}`,
//!     absent mirror ⇒ `refused{no-fleet-state}` (the single-machine contract,
//!     unchanged), torn ⇒ `refused{torn-mirror}`;
//!   - on a hit, APPENDS the envelope to its own `log.jsonl` (raw target string)
//!     and STOPS — NO local delivery attempt and NO disposition stamped by origin
//!     (pending = absence, facts-only). Exit 0, prints the correlation_id.
//! The target host's apply-driver later presents the never-attempted envelope; its
//! door delivers + stamps; dispositions ride back full-mesh. The live end-to-end
//! `delivered` leg is the post-deploy brano acceptance; here we pin the wiring:
//! append-without-delivery, the refusal family, cross-store admission at the target
//! door, and origin-replay idempotency (R15).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A jailed HOME: registry under `<home>/.claude/sessions`, transport files +
/// mirrors under `<home>/.quorum/dispatch/` (QD_HOME unset ⇒ the default layout).
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
    Jail { home, sessions, zmx }
}

fn dispatch_root(home: &Path) -> PathBuf {
    home.join(".quorum").join("dispatch")
}

impl Jail {
    /// Seed `remote/<host>/ls.json` with a raw JSON body (well-formed or a torn
    /// escape hatch).
    fn seed_mirror_raw(&self, host: &str, bytes: &str) {
        let dir = dispatch_root(&self.home).join("remote").join(host);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ls.json"), bytes).unwrap();
    }

    /// Seed a well-formed one-row mirror.
    fn seed_mirror_one(&self, host: &str, name: &str, session_id: &str) {
        let body = format!(
            r#"{{"v":1,"host":"{host}","witnessed_at":{w},"sessions":[{{"name":"{name}","userNamed":true,"sessionId":"{session_id}","qdId":"ab12cdef","status":"idle","provider":"claude-code"}}]}}"#,
            w = now_ms()
        );
        self.seed_mirror_raw(host, &body);
    }

    /// Forge a registry row so the inbound door RESOLVES `name` locally (unknown
    /// provider ⇒ admit + attempt + fail{wake} — an unwakeable target, hermetic).
    fn write_row(&self, pid: i64, session_id: &str, name: &str, provider: &str, status: &str) {
        let row = format!(
            r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"{status}","name":"{name}","version":"0.1.0","provider":"{provider}"}}"#
        );
        std::fs::write(self.sessions.join(format!("{pid}.json")), row).unwrap();
    }

    fn log_path(&self) -> PathBuf {
        dispatch_root(&self.home).join("log.jsonl")
    }
    fn dispositions_path(&self) -> PathBuf {
        dispatch_root(&self.home).join("dispositions.jsonl")
    }

    /// Non-blank rows of a transport file (self-delimiting `\n{}\n` framing).
    fn rows(&self, path: &Path) -> Vec<String> {
        match std::fs::read_to_string(path) {
            Ok(s) => s.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// `qd send <addr> <msg>` in origin mode with QD_HOST = `self_host`.
    fn origin_send(&self, self_host: &str, addr: &str, msg: &str) -> (i32, String, String) {
        let out = Command::new(qd_bin())
            .args(["send", addr, msg])
            .env("HOME", &self.home)
            .env("QD_HOST", self_host)
            .env("QD_TEST_NO_BARE_PROCS", "1")
            .env("ZMX_DIR", &self.zmx)
            .env_remove("QD_HOME")
            .env_remove("QD_MUX")
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// `qd send --inbound-envelope -` feeding `envelope` on stdin, QD_HOST=`self_host`.
    fn inbound(&self, self_host: &str, envelope: &str) -> (i32, String, String) {
        let mut child = Command::new(qd_bin())
            .args(["send", "--inbound-envelope", "-"])
            .env("HOME", &self.home)
            .env("QD_HOST", self_host)
            .env("QD_TEST_NO_BARE_PROCS", "1")
            .env("ZMX_DIR", &self.zmx)
            .env_remove("QD_HOME")
            .env_remove("QD_MUX")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(envelope.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

// ── 1. the core new behavior: resolve in the mirror, APPEND, no delivery ───────

#[test]
fn origin_remote_appends_without_delivery() {
    let tmp = tempfile::tempdir().unwrap();
    let j = jail(tmp.path());
    j.seed_mirror_one("els", "cut-els", "uuid-els-1");

    let (code, stdout, stderr) = j.origin_send("brano", "cut-els@els", "hello remote");
    assert_eq!(code, 0, "origin-remote send exits 0 (stderr: {stderr})");
    let cid = stdout.trim().to_string();
    assert!(!cid.is_empty(), "prints the correlation_id on stdout");

    // The envelope is in our OWN log with the RAW target + verbatim body.
    let log_rows = j.rows(&j.log_path());
    assert_eq!(log_rows.len(), 1, "exactly one envelope appended, got: {log_rows:?}");
    let env: serde_json::Value = serde_json::from_str(&log_rows[0]).unwrap();
    assert_eq!(env["correlation_id"].as_str(), Some(cid.as_str()));
    assert_eq!(env["target"].as_str(), Some("cut-els@els"), "raw target string (R9.4)");
    assert_eq!(env["body"].as_str(), Some("hello remote"));

    // NO disposition stamped by origin (pending = absence): no row for this cid.
    let disp = j.rows(&j.dispositions_path());
    assert!(
        disp.iter().all(|r| !r.contains(&cid)),
        "origin stamps NO disposition for a remote send, found: {disp:?}"
    );
}

// ── 2. the refusal family ──────────────────────────────────────────────────────

#[test]
fn unknown_at_remote_is_refused_unknown_and_appends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let j = jail(tmp.path());
    j.seed_mirror_one("els", "cut-els", "uuid-els-1");

    let (code, _out, err) = j.origin_send("brano", "ghost@els", "x");
    assert_eq!(code, 12, "unknown@remote ⇒ exit 12 (stderr: {err})");
    assert!(err.contains("unknown"), "names the unknown class: {err}");
    assert!(j.rows(&j.log_path()).is_empty(), "a refusal appends NO envelope");
}

#[test]
fn absent_mirror_is_refused_no_fleet_state() {
    let tmp = tempfile::tempdir().unwrap();
    let j = jail(tmp.path());
    // No remote/qrmoh mirror seeded.
    let (code, _out, err) = j.origin_send("brano", "whoever@qrmoh", "x");
    assert_eq!(code, 12, "absent mirror ⇒ exit 12 (stderr: {err})");
    assert!(err.contains("no fleet state"), "names no-fleet-state: {err}");
    assert!(j.rows(&j.log_path()).is_empty());
}

#[test]
fn ambiguous_at_remote_is_refused_ambiguous() {
    let tmp = tempfile::tempdir().unwrap();
    let j = jail(tmp.path());
    let w = now_ms();
    j.seed_mirror_raw(
        "els",
        &format!(
            r#"{{"v":1,"host":"els","witnessed_at":{w},"sessions":[{{"name":"dup","sessionId":"a"}},{{"name":"dup","sessionId":"b"}}]}}"#
        ),
    );
    let (code, _out, err) = j.origin_send("brano", "dup@els", "x");
    assert_eq!(code, 12, "ambiguous@remote ⇒ exit 12 (stderr: {err})");
    assert!(err.contains("ambiguous") || err.contains("more than one"), "names ambiguity: {err}");
    assert!(j.rows(&j.log_path()).is_empty());
}

#[test]
fn torn_mirror_is_refused_torn_mirror() {
    let tmp = tempfile::tempdir().unwrap();
    let j = jail(tmp.path());
    j.seed_mirror_raw("els", "{ this is not valid json");
    let (code, _out, err) = j.origin_send("brano", "cut-els@els", "x");
    assert_eq!(code, 12, "torn mirror ⇒ exit 12 (stderr: {err})");
    assert!(err.contains("unreadable") || err.contains("torn"), "names torn-mirror: {err}");
}

// ── 3. cross-store: A's appended envelope is ADMITTED at the target door ────────

#[test]
fn origin_remote_envelope_is_admitted_by_the_target_inbound_door() {
    let tmp_a = tempfile::tempdir().unwrap();
    let tmp_b = tempfile::tempdir().unwrap();
    let a = jail(tmp_a.path());
    let b = jail(tmp_b.path());

    // Store A (self=brano) originates a send to cut-els@els.
    a.seed_mirror_one("els", "cut-els", "uuid-els-1");
    let (code, out, err) = a.origin_send("brano", "cut-els@els", "roundtrip body");
    assert_eq!(code, 0, "A originates (stderr: {err})");
    let cid = out.trim().to_string();
    let envelope = a.rows(&a.log_path()).remove(0); // the raw envelope JSON

    // Store B (self=els) is the TARGET host. Give it a locally-resolvable but
    // NOT-live (stopped) + unwakeable (unknown-provider) `cut-els` row so the door
    // takes resume-and-deliver: stamp `attempted`, wake, fail{wake} — hermetic, no
    // live carrier. The `attempted` row is the ADMISSION proof.
    let live_pid = std::process::id() as i64; // a real, alive pid
    b.write_row(live_pid, "uuid-els-1", "cut-els", "mystery", "cold");

    let (bcode, _bout, berr) = b.inbound("els", &envelope);
    // The point is ADMISSION: B routed `cut-els@els` LOCALLY (host==self on els) and
    // drove it into its funnel — a fresh `attempted` row for this cid lands on B
    // (proving the cross-store handoff), NOT a resolution refusal.
    assert!(
        !berr.contains("no-fleet-state") && !berr.contains("refused{unknown}"),
        "B routed cut-els@els locally (host==self), did not refuse resolution: {berr}"
    );
    let bdisp = b.rows(&b.dispositions_path());
    assert!(
        bdisp.iter().any(|r| r.contains(&cid) && r.contains("attempted")),
        "the target door ADMITTED + attempted the envelope, dispositions: {bdisp:?} (code {bcode})"
    );
    // B never appends a peer's envelope to its OWN log (inbound never logs).
    assert!(b.rows(&b.log_path()).is_empty(), "inbound door does not append to own log");
}

// ── 4. origin-replay idempotency (R15): no double-append ───────────────────────

#[test]
fn origin_remote_replay_same_id_body_does_not_double_append() {
    let tmp = tempfile::tempdir().unwrap();
    let j = jail(tmp.path());
    j.seed_mirror_one("els", "cut-els", "uuid-els-1");

    // First send appends one envelope; capture its cid.
    let (c1, out1, _e1) = j.origin_send("brano", "cut-els@els", "same body");
    assert_eq!(c1, 0);
    let cid = out1.trim().to_string();
    assert_eq!(j.rows(&j.log_path()).len(), 1);

    // Re-send the SAME address + body: qd mints a NEW cid (content is not identity),
    // so this legitimately appends a SECOND distinct envelope — assert it is a new
    // cid, not a duplicate of the first.
    let (c2, out2, _e2) = j.origin_send("brano", "cut-els@els", "same body");
    assert_eq!(c2, 0);
    let cid2 = out2.trim().to_string();
    assert_ne!(cid, cid2, "a fresh send mints a fresh id (same body twice = two messages)");
    assert_eq!(j.rows(&j.log_path()).len(), 2, "two distinct sends ⇒ two envelopes");

    // But a caller-supplied SAME id + SAME body is the idempotent retry: no double
    // append. Drive it with --correlation-id.
    let out = Command::new(qd_bin())
        .args(["send", "--correlation-id", &cid, "cut-els@els", "same body"])
        .env("HOME", &j.home)
        .env("QD_HOST", "brano")
        .env("QD_TEST_NO_BARE_PROCS", "1")
        .env("ZMX_DIR", &j.zmx)
        .env_remove("QD_HOME")
        .env_remove("QD_MUX")
        .output()
        .unwrap();
    assert_eq!(out.status.code().unwrap_or(-1), 0, "same-id same-body retry is a no-op success");
    assert_eq!(
        j.rows(&j.log_path()).len(),
        2,
        "R15: same id + same body does NOT append a third envelope"
    );
}
