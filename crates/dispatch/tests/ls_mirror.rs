//! qd–qf W7 — `qd ls` FLEET MIRROR reads (READ-ONLY) bin-level integration.
//!
//! Drives the REAL `qd` binary against a JAILED, empty HOME (L9a) with seeded
//! `remote/<host>/ls.json` mirror fixtures. Covers:
//!   - `qd ls --host <h>` prints the peer's rows + a staleness annotation
//!     (human header + `--json` `mirror_age_ms`), computed from `witnessed_at`.
//!   - ABSENT mirror ⇒ `refused{no-fleet-state}` exit 12 (the `qd send --host`
//!     single-machine contract).
//!   - Malformed / `v != 1` mirror ⇒ a NAMED refusal (`refused{torn-mirror}`),
//!     never a panic.
//!   - `qd ls --all` unions local rows + every peer's mirror with per-host
//!     staleness; and WITHOUT `remote/` is byte-identical to today's `--all`.
//!   - `--host` + `--all` is rejected (clap conflict).
//!   - `--json` mirror rows carry `host` + `mirror_age_ms` and parse as JSON.
//!
//! The mirror files here stand in for the (out-of-scope) mover's output; qd only
//! READS them (never writes its own `ls.json`).

mod common;

use common::assert_not_real_home;
use std::path::{Path, PathBuf};
use std::process::Command;

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// A jailed HOME with an empty session registry. `remote/<host>/ls.json` mirrors
/// live under `<home>/.quorum/dispatch/remote/<host>/` (QD_HOME unset ⇒ the
/// default layout, matching the bin under `env_remove("QD_HOME")`). Live-row
/// children are tracked + DETERMINISTICALLY reaped on `Drop` (WP-D: a live row
/// must be backed by a real alive pid; the same guard punch_b5_ls_live uses).
struct Jail {
    home: PathBuf,
    zmx: PathBuf,
    children: std::cell::RefCell<Vec<std::process::Child>>,
}

fn jail(dir: &Path) -> Jail {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    assert_not_real_home(&home);
    Jail {
        home,
        zmx,
        children: std::cell::RefCell::new(Vec::new()),
    }
}

impl Drop for Jail {
    fn drop(&mut self) {
        for mut c in self.children.borrow_mut().drain(..) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Jail {
    fn remote_dir(&self) -> PathBuf {
        self.home.join(".quorum").join("dispatch").join("remote")
    }

    /// Seed `remote/<host>/ls.json` with the given raw bytes (a torn-fixture escape
    /// hatch — the callers below use `seed_mirror` for well-formed ones).
    fn seed_mirror_raw(&self, host: &str, bytes: &str) {
        let dir = self.remote_dir().join(host);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ls.json"), bytes).unwrap();
    }

    /// Seed a well-formed `remote/<host>/ls.json` with one named session row and a
    /// given `witnessed_at`.
    fn seed_mirror(&self, host: &str, witnessed_at: i64, name: &str, session_id: &str) {
        let bytes = format!(
            r#"{{"v":1,"host":"{host}","witnessed_at":{witnessed_at},"sessions":[{{"name":"{name}","userNamed":true,"sessionId":"{session_id}","status":"idle","turns":2,"tokens":100,"provider":"claude-code"}}]}}"#
        );
        self.seed_mirror_raw(host, &bytes);
    }

    /// A live NAMED local registry row backed by a real alive child (the WP-D
    /// liveness gate downgrades a dead-pid row to cold). The child is tracked and
    /// reaped on `Jail::drop` — no process leak.
    fn write_live_local_row(&self, session_id: &str, name: &str) {
        let child = Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn child");
        let pid = child.id() as i64;
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let row = format!(
            r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":{start},"updatedAt":{start},"status":"idle","name":"{name}","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
        );
        std::fs::write(
            self.home
                .join(".claude")
                .join("sessions")
                .join(format!("{pid}.json")),
            row,
        )
        .unwrap();
        self.children.borrow_mut().push(child);
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(qd_bin())
            .args(args)
            .env("HOME", &self.home)
            // Jail the bare-proc gather so a real bare codex/opencode/pi on the
            // host can't leak extra rows into these assertions (test-lane only).
            .env("QD_TEST_NO_BARE_PROCS", "1")
            .env("ZMX_DIR", &self.zmx)
            .env_remove("QD_HOME")
            .env_remove("QD_MUX")
            .output()
            .expect("spawn qd");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

fn rows(stdout: &str) -> Vec<serde_json::Value> {
    serde_json::from_str::<Vec<serde_json::Value>>(stdout)
        .unwrap_or_else(|e| panic!("ls --json is a JSON array; got err {e} for:\n{stdout}"))
}

// ===========================================================================
// --host <h> — the single-host mirror read
// ===========================================================================

/// `qd ls --host peerbox --json` prints the peer's rows, each carrying `host` +
/// `mirror_witnessed_at` + `mirror_age_ms` computed from `witnessed_at`.
#[test]
fn host_json_prints_peer_rows_with_staleness_columns() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    // A witnessed_at safely in the past so age_ms is comfortably positive.
    let witnessed = 1_000_000_000_000i64; // 2001-09-09, long ago
    j.seed_mirror("peerbox", witnessed, "remote-wk", "sess-remote-1");

    let (code, out, err) = j.run(&["ls", "--host", "peerbox", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let r = rows(&out);
    assert_eq!(r.len(), 1, "one peer row: {out}");
    let row = &r[0];
    assert_eq!(row["name"], "remote-wk", "peer row content preserved");
    assert_eq!(row["host"], "peerbox", "row annotated with host");
    assert_eq!(
        row["mirror_witnessed_at"], witnessed,
        "row carries the mirror's witnessed_at"
    );
    let age = row["mirror_age_ms"]
        .as_i64()
        .expect("mirror_age_ms is an integer");
    assert!(
        age > 0,
        "age = now − witnessed_at is positive for a long-past witness: {age}"
    );
    // The peer's own row shape is preserved (a superset with the mirror columns).
    assert_eq!(row["provider"], "claude-code");
    assert_eq!(row["status"], "idle");
}

/// `qd ls --host peerbox` (human) prints a staleness header naming the host + an
/// ISO witnessed instant, then the peer's row.
#[test]
fn host_human_prints_staleness_header_and_rows() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    // 2024-06-04T10:00:00.000Z.
    j.seed_mirror("peerbox", 1_717_495_200_000, "remote-wk", "sess-remote-1");

    // Force the human table (agent-auto would flip to JSON under a pipe).
    let (code, out, err) = j.run(&["ls", "--host", "peerbox", "--table"]);
    assert_eq!(code, 0, "stderr: {err}");
    let plain = strip_ansi(&out);
    assert!(
        plain.contains("host peerbox — mirror age"),
        "staleness header names the host + age: {plain}"
    );
    assert!(
        plain.contains("witnessed 2024-06-04T10:00:00.000Z"),
        "header carries the ISO witnessed instant: {plain}"
    );
    assert!(
        plain.contains("remote-wk"),
        "the peer's row is shown: {plain}"
    );
}

/// An ABSENT mirror for the host ⇒ `refused{no-fleet-state}` exit 12 (the
/// single-machine contract, consistent with `qd send --host`).
#[test]
fn host_absent_mirror_is_refused_no_fleet_state_exit_12() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path()); // no remote/ at all
    let (code, out, err) = j.run(&["ls", "--host", "ghostbox", "--json"]);
    assert_eq!(
        code, 12,
        "absent mirror → exit 12 (stdout: {out}, stderr: {err})"
    );
    assert!(
        err.starts_with("qd send: refused{no-fleet-state}:"),
        "the shared Refusal render + no-fleet-state class, got: {err}"
    );
    assert!(
        err.contains("ghostbox") && err.contains("no fleet state"),
        "names the host + the absent-fleet reason, got: {err}"
    );
    assert_eq!(out, "", "no rows on stdout when refused");
}

/// A malformed / `v != 1` mirror ⇒ a NAMED refusal (`refused{torn-mirror}`),
/// exit 12, never a panic.
#[test]
fn host_torn_mirror_is_named_refusal_not_panic() {
    let t = tempfile::tempdir().unwrap();

    // (a) invalid JSON.
    let j = jail(t.path());
    j.seed_mirror_raw("peerbox", "{not valid json");
    let (code, _out, err) = j.run(&["ls", "--host", "peerbox", "--json"]);
    assert_eq!(code, 12, "torn mirror → exit 12, got stderr: {err}");
    assert!(
        err.starts_with("qd send: refused{torn-mirror}:"),
        "torn JSON → refused{{torn-mirror}}, got: {err}"
    );
    assert!(!err.contains("panic"), "no panic backtrace: {err}");

    // (b) wrong version.
    let t2 = tempfile::tempdir().unwrap();
    let j2 = jail(t2.path());
    j2.seed_mirror_raw(
        "peerbox",
        r#"{"v":2,"host":"peerbox","witnessed_at":1,"sessions":[]}"#,
    );
    let (code2, _o2, err2) = j2.run(&["ls", "--host", "peerbox", "--json"]);
    assert_eq!(code2, 12, "v!=1 → exit 12, got stderr: {err2}");
    assert!(
        err2.starts_with("qd send: refused{torn-mirror}:") && err2.contains("version"),
        "v!=1 → refused{{torn-mirror}} naming the version, got: {err2}"
    );
}

/// `--host` + `--all` is rejected AT PARSE (clap conflict). Same convention the
/// existing `--live --all` conflict follows: commander-mapped exit 1, `error: …`
/// on stderr, no listing.
#[test]
fn host_and_all_conflict_is_rejected() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    let (code, out, err) = j.run(&["ls", "--host", "peerbox", "--all"]);
    assert_eq!(code, 1, "parse conflict exits 1; stderr: {err}");
    assert_eq!(out, "", "no listing on the conflict path");
    assert!(err.starts_with("error: "), "commander-style error: {err}");
    assert!(
        err.contains("cannot be used with"),
        "clap names the conflict, got: {err}"
    );
}

// ===========================================================================
// --all — the fleet union (additive over the unchanged local dump)
// ===========================================================================

/// `qd ls --all --json` with TWO peer mirrors unions local rows + both peers'
/// rows, each peer's rows annotated with its host + staleness. Local rows carry
/// NO mirror columns (a superset — existing consumers unbroken).
#[test]
fn all_json_unions_local_and_every_peer_mirror() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    j.write_live_local_row("sess-local-1", "local-wk");
    j.seed_mirror("alpha", 1_000_000_000_000, "alpha-wk", "sess-alpha-1");
    j.seed_mirror("beta", 1_200_000_000_000, "beta-wk", "sess-beta-1");

    let (code, out, err) = j.run(&["ls", "--all", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let r = rows(&out);
    let by_name = |n: &str| r.iter().find(|row| row["name"] == n);

    let local = by_name("local-wk").expect("local row present");
    assert!(
        local.get("host").is_none() && local.get("mirror_age_ms").is_none(),
        "local rows carry NO mirror columns (superset): {local}"
    );

    let alpha = by_name("alpha-wk").expect("alpha peer row present");
    assert_eq!(alpha["host"], "alpha");
    assert!(alpha["mirror_age_ms"].as_i64().unwrap() > 0);

    let beta = by_name("beta-wk").expect("beta peer row present");
    assert_eq!(beta["host"], "beta");
    assert!(beta["mirror_age_ms"].as_i64().unwrap() > 0);
    // The two peers carry DISTINCT witnessed_at (per-host staleness, not shared).
    assert_ne!(
        alpha["mirror_witnessed_at"], beta["mirror_witnessed_at"],
        "each peer keeps its own witnessed_at"
    );
}

/// `qd ls --all` (human) with a peer mirror shows the local table THEN a per-host
/// staleness header + that peer's rows.
#[test]
fn all_human_groups_peers_with_staleness() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    j.write_live_local_row("sess-local-1", "local-wk");
    j.seed_mirror("alpha", 1_717_495_200_000, "alpha-wk", "sess-alpha-1");

    let (code, out, err) = j.run(&["ls", "--all", "--table"]);
    assert_eq!(code, 0, "stderr: {err}");
    let plain = strip_ansi(&out);
    assert!(plain.contains("local-wk"), "local row present: {plain}");
    assert!(
        plain.contains("host alpha — mirror age"),
        "per-host staleness header present: {plain}"
    );
    assert!(plain.contains("alpha-wk"), "peer row present: {plain}");
}

/// The load-bearing reconciliation: WITHOUT any `remote/`, `qd ls --all` is
/// BYTE-IDENTICAL to today's `--all`. We assert it against the SAME jail run
/// twice (no remote seeded) AND that no mirror columns/headers ever appear.
#[test]
fn all_without_remote_is_byte_identical_to_today() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    j.write_live_local_row("sess-local-1", "local-wk");
    // No remote/ directory exists.
    assert!(!j.remote_dir().exists(), "precondition: no remote/");

    let (code, out, err) = j.run(&["ls", "--all", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let r = rows(&out);
    let local = r
        .iter()
        .find(|row| row["name"] == "local-wk")
        .expect("local row");
    // No mirror columns anywhere — the union added nothing.
    for row in &r {
        assert!(
            row.get("host").is_none()
                && row.get("mirror_age_ms").is_none()
                && row.get("mirror_witnessed_at").is_none(),
            "no-remote --all must add no mirror columns: {row}"
        );
    }
    let _ = local;

    // Human --all with no remote/ carries no staleness header.
    let (hc, hout, herr) = j.run(&["ls", "--all", "--table"]);
    assert_eq!(hc, 0, "stderr: {herr}");
    let plain = strip_ansi(&hout);
    assert!(
        !plain.contains("mirror age") && !plain.contains("host "),
        "no-remote --all human output carries no mirror header: {plain}"
    );
}

/// A torn/absent per-host mirror under `--all` is SKIPPED (best-effort across the
/// fleet) with a stderr warning — it never fails the whole dump. The GOOD peer
/// still surfaces; qd exits 0.
#[test]
fn all_skips_a_torn_peer_but_keeps_the_good_one() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    j.seed_mirror("good", 1_000_000_000_000, "good-wk", "sess-good-1");
    j.seed_mirror_raw("badpeer", "{corrupt"); // torn

    let (code, out, err) = j.run(&["ls", "--all", "--json"]);
    assert_eq!(code, 0, "one torn peer must NOT fail --all: stderr: {err}");
    let r = rows(&out);
    assert!(
        r.iter().any(|row| row["name"] == "good-wk"),
        "the good peer still surfaces: {out}"
    );
    assert!(
        err.contains("skipping") && err.contains("badpeer"),
        "the torn peer is warned about on stderr, got: {err}"
    );
}

// ===========================================================================
// Regression — no flags / plain --all behavior with mirrors unchanged locally
// ===========================================================================

/// `qd ls` with NO flags never reads mirrors: even with a peer mirror present,
/// the default (non-`--all`, non-`--host`) view is local-only.
#[test]
fn plain_ls_ignores_mirrors() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    j.write_live_local_row("sess-local-1", "local-wk");
    j.seed_mirror("alpha", 1_000_000_000_000, "alpha-wk", "sess-alpha-1");

    let (code, out, err) = j.run(&["ls", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let r = rows(&out);
    assert!(
        r.iter().any(|row| row["name"] == "local-wk"),
        "local row present: {out}"
    );
    assert!(
        !r.iter().any(|row| row["name"] == "alpha-wk"),
        "plain ls must NOT union mirrors: {out}"
    );
    // And no mirror columns leak onto the local rows.
    for row in &r {
        assert!(
            row.get("host").is_none(),
            "no mirror columns on plain ls: {row}"
        );
    }
}

/// Strip SGR sequences for plain-text assertions.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
