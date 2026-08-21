//! Punch B5 item 2 (orc-ruled C1+D) — bin-level pins for the `qd ls --live`
//! scripting surface and the default view's loud truncation trailer, driving
//! the REAL `qd` binary against a JAILED HOME (L9a / ADD-4 discipline; harness
//! mirrors info_json.rs — integration test binaries cannot import each other,
//! duplication is the sanctioned shape).
//!
//! Pinned contract:
//! - `--live` = the resolver's ONE liveness class (`is_live_status`:
//!   idle/busy/shell; cold + killed are tombstones), UNCAPPED, composing with
//!   `--json` / `--short` / `--prefix`; an explicit `-n` caps the LIVE set.
//! - `--live` + `--all` REJECTS AT PARSE (declared rule: one liveness class per
//!   query — the start render-flag precedent). Pinned at the bin boundary here;
//!   cli.rs unit-pins the clap ArgumentConflict kind.
//! - The DEFAULT view (no --all/--live/no valid -n), when its cap (20) drops
//!   eligible rows, prints `… N more (qd ls --all)` on STDERR in text modes;
//!   `--json` NEVER carries it. N = total-eligible − shown.
//! - Existing default-cap + `--all` behavior is unchanged (D is additive on the
//!   over-cap case only): at/under cap → no trailer, stdout bytes untouched.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{assert_not_real_home, set_mtime_ms};

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// Frozen base timestamp for deterministic ordering (the surfaces asserted here
/// carry relative times only; this is belt-level determinism).
const UPDATED_MS: i64 = 1_717_495_300_000;

fn registry_row(
    pid: i64,
    started_ms: i64,
    session_id: &str,
    name: &str,
    updated_ms: i64,
) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":{started_ms},"updatedAt":{updated_ms},"status":"idle","name":"{name}","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
    )
}

struct Jail {
    home: PathBuf,
    zmx: PathBuf,
    // WP-D (engsol S-1): `qd ls` now liveness-GATES each live row on the WP-A
    // starttime classifier (a dead/zombie/gone/reused pid is downgraded to
    // `cold`). So a LIVE-status fixture row must be backed by a REAL, alive pid
    // or the gate would correctly drop it. Each live row spawns a guarded child
    // (its real `(pid,starttime)` written into the row) tracked here and
    // DETERMINISTICALLY reaped on `Drop` (no process leak, no flake). The cap/
    // trailer/ordering/unnamed CONTRACTS asserted below are unchanged — only the
    // backing pid moved from synthetic-but-"gone" to real-and-alive.
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
        // Deterministic reap of every guarded live-row child (WP-D).
        for mut c in self.children.borrow_mut().drain(..) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Jail {
    /// Spawn a guarded, long-lived child and return its `(pid, started_at_ms)`.
    /// WP-D: a live registry row must be backed by a real alive pid (with a
    /// starttime within the classifier's 120s reuse-slack of the real one) or
    /// `qd ls` gates it to `cold`. The recorded start is **wall-clock now** — the
    /// exact PRODUCTION shape (the registry `startedAt` is the registration
    /// timestamp ≈ process start), and within 120s of the `proc_start_ms` the gate
    /// reads at `ls` time. (Reading `proc_start_ms` of the just-forked child here
    /// would be unreliable — `ps -o etime=` of a sub-second-old process can
    /// misparse — which is a registration-time read the real fleet never does.)
    /// The child is reaped on `Jail::drop`.
    fn spawn_live(&self) -> (i64, i64) {
        let child = Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn live-row child");
        let pid = child.id() as i64;
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        self.children.borrow_mut().push(child);
        (pid, start)
    }

    /// A live (idle) NAMED registry row. `_seq` is the caller's row index (kept for
    /// call-site stability); the real pid+starttime come from a freshly spawned
    /// guarded child (WP-D).
    fn write_row(&self, _seq: i64, session_id: &str, name: &str, updated_ms: i64) {
        let (pid, start) = self.spawn_live();
        let sessions = self.home.join(".claude").join("sessions");
        std::fs::write(
            sessions.join(format!("{pid}.json")),
            registry_row(pid, start, session_id, name, updated_ms),
        )
        .unwrap();
    }

    /// A live (idle) registry row with NO `name` field → joins as user_named
    /// false (the default view's named-only filter excludes it; --live does
    /// not). No matching transcript, so no name is derived. Backed by a real
    /// guarded child (WP-D); `_seq` kept for call-site stability.
    fn write_unnamed_row(&self, _seq: i64, session_id: &str, updated_ms: i64) {
        let (pid, start) = self.spawn_live();
        let sessions = self.home.join(".claude").join("sessions");
        let row = format!(
            r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":{start},"updatedAt":{updated_ms},"status":"idle","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
        );
        std::fs::write(sessions.join(format!("{pid}.json")), row).unwrap();
    }

    /// A NAMED registry row with `status:idle` on disk but a DEAD pid (R19c pin):
    /// spawn a child, capture its pid + wall-clock start, then KILL+REAP it so the
    /// pid is no longer alive. The row on disk still says `idle` — exactly the
    /// stale-status/dead-pid shape the WP-D gate must demote at the JSON emit
    /// surface. Returns the (dead) pid. (Even in the rare event the OS reuses the
    /// pid before `ls` runs, the reused process has a DIFFERENT start-time than the
    /// recorded one → the classifier's recycled-pid protection still demotes it —
    /// so the assertion is robust either way.)
    fn write_dead_pid_live_row(&self, session_id: &str, name: &str, updated_ms: i64) -> i64 {
        let mut child = Command::new("sleep").arg("600").spawn().expect("spawn");
        let pid = child.id() as i64;
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        // Kill + reap NOW → the recorded pid is dead by the time `ls` classifies it.
        child.kill().unwrap();
        child.wait().unwrap();
        let sessions = self.home.join(".claude").join("sessions");
        std::fs::write(
            sessions.join(format!("{pid}.json")),
            registry_row(pid, start, session_id, name, updated_ms),
        )
        .unwrap();
        pid
    }

    /// A tombstoned (killed) registry row — `<pid>.json.tombstoned`. A tombstone is
    /// a DEAD session (joins as `Killed`, never live) so the liveness gate does not
    /// touch it — it keeps a synthetic pid + frozen start (no child needed).
    fn write_tombstoned_row(&self, pid: i64, session_id: &str, name: &str, updated_ms: i64) {
        let sessions = self.home.join(".claude").join("sessions");
        std::fs::write(
            sessions.join(format!("{pid}.json.tombstoned")),
            registry_row(pid, 1_717_000_000_000, session_id, name, updated_ms),
        )
        .unwrap();
    }

    /// Seed the transcript a REAL claude session would have left behind
    /// (info_json.rs shape): agent-name record + one user record. Frozen mtime
    /// so the cold row's lastActive (and so the sort position) is controlled.
    fn write_cold_transcript(&self, uuid: &str, name: &str, mtime_ms: i64) {
        let proj = self.home.join(".claude").join("projects").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join(format!("{uuid}.jsonl"));
        let body = format!(
            "{{\"type\":\"agent-name\",\"agentName\":\"{name}\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":\"hello\"}},\
             \"cwd\":\"/w\",\"sessionId\":\"{uuid}\"}}\n"
        );
        std::fs::write(&path, body).unwrap();
        set_mtime_ms(&path, mtime_ms);
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(qd_bin())
            .args(args)
            .env("HOME", &self.home)
            // lsview A4 (CF-F1): jail the ls bare-proc gather against host
            // `ps`/`lsof` so a real bare codex/opencode/pi on the host cannot
            // leak extra rows into these exact `qd ls` assertions. Test-lane only.
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

/// A synthetic v4-shaped uuid for cold transcript `n`.
fn cold_uuid(n: usize) -> String {
    format!("c01d{n:04x}-aaaa-4aaa-8aaa-{n:012x}")
}

/// The over-cap mixed jail: `n_live` named live (idle registry) rows and
/// `n_cold` named cold transcripts whose mtimes sort ABOVE every live row —
/// the exact survey trap (cold history displacing live rows behind the default
/// cap). Live rows are named `lv00..`, cold rows `cold0..`.
fn mixed_jail(dir: &Path, n_live: usize, n_cold: usize) -> Jail {
    let j = jail(dir);
    for i in 0..n_live {
        j.write_row(
            60_000 + i as i64,
            &format!("aaaa{i:04x}-aaaa-4aaa-8aaa-{i:012x}"),
            &format!("lv{i:02}"),
            UPDATED_MS - (i as i64) * 1_000,
        );
    }
    for i in 0..n_cold {
        // Cold mtimes NEWER than every live updatedAt → cold sorts first.
        j.write_cold_transcript(
            &cold_uuid(i),
            &format!("cold{i}"),
            UPDATED_MS + 60_000 + i as i64,
        );
    }
    j
}

/// Parse `ls --json` stdout (a top-level array) into serde values.
fn rows(stdout: &str) -> Vec<serde_json::Value> {
    serde_json::from_str::<Vec<serde_json::Value>>(stdout).expect("ls --json is a JSON array")
}

fn names(rows: &[serde_json::Value]) -> Vec<String> {
    rows.iter()
        .filter_map(|r| r.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

/// `--live --json`: UNCAPPED (all 25 live rows surface — more than the default
/// cap), cold rows ABSENT, and the machine surface carries NO trailer even
/// though the sibling default view truncates.
#[test]
fn live_json_is_uncapped_and_excludes_cold() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 25, 3);

    let (code, out, err) = j.run(&["ls", "--live", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(err, "", "--live --json: machine surfaces carry no trailer");
    let r = rows(&out);
    assert_eq!(
        r.len(),
        25,
        "UNCAPPED: every live row, names: {:?}",
        names(&r)
    );
    for row in &r {
        let status = row["status"].as_str().unwrap();
        assert!(
            matches!(status, "idle" | "busy" | "shell"),
            "live class only (is_live_status), got: {status}"
        );
    }
    assert!(
        !names(&r).iter().any(|n| n.starts_with("cold")),
        "cold rows excluded: {:?}",
        names(&r)
    );

    // Fixture validity: the DEFAULT view really is the trap — capped at 20 with
    // the recent cold rows present (displacing live rows).
    let (code, out, _err) = j.run(&["ls", "--json"]);
    assert_eq!(code, 0);
    let r = rows(&out);
    assert_eq!(r.len(), 20, "default view caps the mixture at 20");
    assert!(
        names(&r).iter().any(|n| n.starts_with("cold")),
        "default view mixes cold in: {:?}",
        names(&r)
    );
}

/// `--live` excludes KILLED (tombstoned) rows too — liveness is the resolver
/// class (not-cold AND not-killed), not merely "not cold".
#[test]
fn live_excludes_killed_rows() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 2, 0);
    j.write_tombstoned_row(
        61_000,
        "dead0000-aaaa-4aaa-8aaa-000000000000",
        "deadrow",
        UPDATED_MS,
    );

    // Fixture validity: --all surfaces the killed row.
    let (code, out, err) = j.run(&["ls", "--all", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        names(&rows(&out)).contains(&"deadrow".to_string()),
        "--all shows the tombstoned row: {out}"
    );

    let (code, out, err) = j.run(&["ls", "--live", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        !names(&rows(&out)).contains(&"deadrow".to_string()),
        "--live excludes killed: {out}"
    );
}

/// S3 declare-don't-hide pin: an UNNAMED live row (user_named false) appears
/// under `--live` but is ABSENT from the default view (whose named-only filter
/// hides it). The deliberate consequence of --live lifting the named-only
/// filter — a scripting consumer wanting live sessions sees the unnamed ones.
#[test]
fn live_includes_unnamed_row_default_view_hides_it() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 1, 0); // one named live row "lv00"
    j.write_unnamed_row(
        63_000,
        "eeee0000-eeee-4eee-8eee-000000000000",
        UPDATED_MS + 5,
    );
    let unnamed_sid = "eeee0000-eeee-4eee-8eee-000000000000";

    // Default view: the unnamed row is filtered out (named-only); lv00 stays.
    let (code, out, err) = j.run(&["ls", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let default_rows = rows(&out);
    assert!(
        default_rows
            .iter()
            .any(|r| r["sessionId"] == "aaaa0000-aaaa-4aaa-8aaa-000000000000"),
        "default view keeps the NAMED live row: {out}"
    );
    assert!(
        !default_rows.iter().any(|r| r["sessionId"] == unnamed_sid),
        "default view HIDES the unnamed live row (named-only filter): {out}"
    );

    // --live: the unnamed row appears (named-only filter does NOT apply).
    let (code, out, err) = j.run(&["ls", "--live", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let live_rows = rows(&out);
    assert!(
        live_rows.iter().any(|r| r["sessionId"] == unnamed_sid),
        "--live SHOWS the unnamed live row (declare-don't-hide): {out}"
    );
    assert_eq!(
        live_rows[live_rows
            .iter()
            .position(|r| r["sessionId"] == unnamed_sid)
            .unwrap()]["status"],
        "idle",
        "the unnamed row is live-class"
    );
}

/// `--live` text mode: uncapped table (all 25 live names), no cold names, no
/// trailer (the live view is uncapped — nothing was dropped).
#[test]
fn live_text_is_uncapped_no_trailer() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 25, 3);
    // WP-B7 PIECE 1 adapt: this pins the --live TABLE surface (uncapped, all live
    // names, no cold, no trailer). Under the table→JSON auto-flip an agent caller
    // auto-detects to JSON, so inject `--table` to keep exercising the text table.
    let (code, out, err) = j.run(&["ls", "--table", "--live"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(err, "", "--live is uncapped → no trailer");
    for i in 0..25 {
        assert!(out.contains(&format!("lv{i:02}")), "row lv{i:02} in: {out}");
    }
    assert!(!out.contains("cold"), "no cold rows: {out}");
}

/// `-n` composes with `--live`: an explicit limit caps the LIVE set (5 live
/// rows — never "5 of the mixture, then filter", which could return fewer).
#[test]
fn live_composes_with_explicit_limit() {
    let t = tempfile::tempdir().unwrap();
    // 3 recent cold rows sort FIRST: a pre-filter -n 5 would keep only 2 live.
    let j = mixed_jail(t.path(), 10, 3);
    let (code, out, err) = j.run(&["ls", "--live", "-n", "5", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let r = rows(&out);
    assert_eq!(r.len(), 5, "-n caps the LIVE set: {:?}", names(&r));
    assert!(
        names(&r).iter().all(|n| n.starts_with("lv")),
        "all 5 are live rows: {:?}",
        names(&r)
    );
}

/// `--prefix` + `--short` compose with `--live` (the survey's scripting
/// combinators all keep working on the live view).
#[test]
fn live_composes_with_prefix_and_short() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 3, 1);
    j.write_row(
        62_000,
        "bbbb0000-bbbb-4bbb-8bbb-000000000000",
        "other",
        UPDATED_MS,
    );
    // WP-B7 PIECE 1 adapt: the short TEXT surface (one name per line) is the point
    // here. Inject `--table` so `--table --short` (the ratified agent short-text
    // escape hatch) keeps the short surface under the auto-flip; the line-count +
    // prefix/cold-exclusion coverage is preserved.
    let (code, out, err) = j.run(&["ls", "--table", "--live", "--short", "--prefix", "lv"]);
    assert_eq!(code, 0, "stderr: {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "3 lv-prefixed live rows: {out}");
    assert!(!out.contains("other"), "prefix filter applies: {out}");
    assert!(!out.contains("cold"), "cold excluded: {out}");
}

/// DECLARED RULE pinned at the bin boundary: `--live --all` is a parse-time
/// conflict (commander-mapped: exit 1, `error: ...` on stderr, no listing).
#[test]
fn live_with_all_rejects_at_parse() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 1, 0);
    let (code, out, err) = j.run(&["ls", "--live", "--all"]);
    assert_eq!(code, 1, "parse conflict exits 1; stderr: {err}");
    assert_eq!(out, "", "no listing on the conflict path");
    assert!(err.starts_with("error: "), "commander-style error: {err}");
}

/// D trailer: the over-cap DEFAULT view announces what the cap dropped —
/// `… N more (qd ls --all)` on STDERR (stdout pipelines stay clean), with
/// N = total-eligible − shown (25 live + 3 cold eligible = 28 − 20 = 8).
#[test]
fn default_over_cap_prints_trailer_on_stderr() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 25, 3);
    // WP-B7 PIECE 1 adapt: the `… N more` trailer is a HUMAN TABLE-surface
    // affordance — the JSON machine surface never carries it (json_never_carries_
    // trailer pins that). Post-flip an agent's bare `ls` is JSON, so to exercise
    // the table-surface trailer we force the table with `--table` (the surface a
    // human reaches via auto-detect at a TTY).
    let (code, out, err) = j.run(&["ls", "--table"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(
        err, "… 8 more (qd ls --all)\n",
        "exact trailer line, count = eligible − shown"
    );
    assert!(
        !out.contains("more (qd ls --all)"),
        "trailer is stderr-only; stdout bytes unchanged: {out}"
    );
}

/// The trailer NEVER rides the json surface: same over-cap jail, `--json` →
/// stderr is byte-empty (wire-integrity: machine surfaces carry JSON only).
#[test]
fn json_never_carries_trailer() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 25, 3);
    let (code, out, err) = j.run(&["ls", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(err, "", "--json: NO trailer, nothing on stderr");
    assert_eq!(rows(&out).len(), 20, "default cap itself unchanged");
}

/// Existing behavior BYTE-UNCHANGED where the cap doesn't truncate, and on the
/// flagged views: at-cap default, `--all`, and explicit `-n` print no trailer.
#[test]
fn no_trailer_at_cap_or_on_flagged_views() {
    let t = tempfile::tempdir().unwrap();
    // Exactly 20 eligible rows: the boundary case — capped view, nothing dropped.
    // WP-B7 PIECE 1 adapt: force the table surface (`--table`) so this exercises
    // the TEXT-mode trailer logic (the JSON surface never carries a trailer at all,
    // so a bare agent `ls` would pass these no-trailer asserts vacuously).
    let j = mixed_jail(t.path(), 20, 0);
    let (code, _out, err) = j.run(&["ls", "--table"]);
    assert_eq!(code, 0);
    assert_eq!(err, "", "at-cap default: no trailer");

    let t2 = tempfile::tempdir().unwrap();
    let j2 = mixed_jail(t2.path(), 25, 3);
    let (code, _out, err) = j2.run(&["ls", "--table", "--all"]);
    assert_eq!(code, 0);
    assert_eq!(err, "", "--all is uncapped: no trailer");
    let (code, _out, err) = j2.run(&["ls", "--table", "-n", "5"]);
    assert_eq!(code, 0);
    assert_eq!(err, "", "explicit -n is the user's own cap: no trailer");
}

/// Bare `qd` is the HELP, not a session list.
///
/// This test used to assert the opposite: that the default action was `ls` and
/// therefore participated in the WP-B7 PIECE 1 surface auto-flip (agent/pipe ⇒
/// JSON). The default moved. `ls` answered a question nobody asked — a new
/// machine's first sentence was `No sessions found.` — and an install pointing
/// people at bare `qd` needs it to say what qd IS.
///
/// The auto-flip itself is untouched and still covered here, on the verb that
/// still has it: `json_never_carries_trailer` and
/// `ls_surface_auto_flips_agent_to_json_table_overrides` both drive `qd ls`
/// through a pipe. What is pinned here is only that bare `qd` no longer emits a
/// session surface at all — this harness pipes stdout, so under the old default
/// it would have produced the 20-row JSON array, and that is exactly what must
/// not happen now.
#[test]
fn bare_qd_prints_the_help_not_a_session_list() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 25, 3);
    let (code, out, err) = j.run(&[]);

    assert_eq!(code, 0, "bare `qd` exits 0: {err}");
    assert!(
        out.starts_with("Usage: qd [options] [command]"),
        "bare `qd` is the help: {out}"
    );
    assert!(
        out.contains("qd setup --fix"),
        "…and the help says what `setup` does, which is the whole point of \
         landing here on a fresh machine: {out}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&out).is_err(),
        "bare `qd` must NOT emit the machine session surface, even piped: {out}"
    );
}

/// S5 pin: `--prefix` + over-cap reports the TOTAL dropped, NOT the prefix-
/// scoped count — `capped_out` is counted PRE-prefix. Documented as internally
/// consistent (the suggested `--all` remedy is also prefix-less). Here only 2
/// of 28 eligible rows match "lv00"|"lv01", yet the trailer still says 8.
#[test]
fn prefix_over_cap_trailer_counts_total_not_prefix_scoped() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 25, 3); // 28 eligible, default cap drops 8
                                         // WP-B7 PIECE 1 adapt: `--table` to exercise the table-surface trailer (the
                                         // pre-prefix total-drop count) under the auto-flip.
    let (code, out, err) = j.run(&["ls", "--table", "--prefix", "lv0"]);
    assert_eq!(code, 0, "stderr: {err}");
    // stdout is prefix-scoped (only lv0x rows that survived the pre-prefix cap).
    assert!(!out.contains("cold"), "prefix filters the listing: {out}");
    // ...but the trailer count is the TOTAL pre-prefix drop, not the lv-scoped one.
    assert_eq!(
        err, "… 8 more (qd ls --all)\n",
        "trailer reports total dropped (pre-prefix), the documented behavior"
    );
}

/// WP-B7 PIECE 1 — the table→JSON render-surface AUTO-FLIP, pinned at the bin
/// boundary. This harness PIPES stdout (no TTY), so `qd ls` auto-detects the AGENT
/// surface. PROVES, all in one place:
///   (1) the agent/pipe DEFAULT is now JSON (the flip) — a bare `ls` parses as a
///       JSON array, with NO human-table header;
///   (2) `--table` forces the human TABLE even for the agent caller (the false-
///       positive guard: a human / explicit-table caller still gets a table, never
///       JSON);
///   (3) explicit `--json` is unchanged (the override still wins);
///   (4) `--table --short` is the agent short-text ESCAPE HATCH — a short TABLE
///       (one name per line), NOT JSON (`--short` is a content modifier that
///       composes with the `--table` surface; qd-supervisor-11-ratified).
/// MUTATION EVIDENCE: reverting the `run_inner` wiring to the raw `--json` flag
/// (pre-flip) reds assertion (1) — a bare `ls` would emit the human table, not a
/// JSON array (captured red-before in the WP-B7 build record).
#[test]
fn ls_surface_auto_flips_agent_to_json_table_overrides() {
    let t = tempfile::tempdir().unwrap();
    let j = mixed_jail(t.path(), 2, 0); // 2 named live rows: lv00, lv01

    // (1) agent/pipe default → JSON machine surface (THE FLIP).
    let (code, out, err) = j.run(&["ls"]);
    assert_eq!(code, 0, "stderr: {err}");
    let r = rows(&out); // panics unless `out` is a top-level JSON array
    assert!(
        names(&r).iter().any(|n| n == "lv00"),
        "bare `ls` auto-flips to JSON rows: {out}"
    );
    assert!(
        !out.contains("Last active"),
        "the JSON default carries no human-table header: {out}"
    );

    // (2) `--table` forces the human table even for the agent caller (false-pos guard).
    let (code, out, err) = j.run(&["ls", "--table"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("Name") && out.contains("Status") && out.contains("Last active"),
        "`--table` → the human table header: {out}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(out.trim()).is_err(),
        "`--table` output is the table, NOT JSON: {out}"
    );

    // (3) explicit `--json` override is unchanged.
    let (code, out, _e) = j.run(&["ls", "--json"]);
    assert_eq!(code, 0);
    assert!(names(&rows(&out)).iter().any(|n| n == "lv00"));

    // (4) `--table --short` → short TEXT (the agent escape hatch), NOT JSON and NOT
    // the wide table — surface=Table + content=short ⇒ one name per line.
    let (code, out, err) = j.run(&["ls", "--table", "--short"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("lv00") && out.contains("lv01"),
        "short surface lists the names: {out}"
    );
    assert!(
        !out.contains("Last active"),
        "`--table --short` is the short list, not the full table: {out}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(out.trim()).is_err(),
        "`--table --short` is text, NOT JSON: {out}"
    );
}

/// R19c PIN — the `qd ls --json` EMIT SURFACE demotes a dead-pid local row.
///
/// A registry row whose on-disk `status` is `idle` but whose recorded pid is DEAD
/// must surface as NOT-LIVE (`cold`) in the `--json` output — the WP-D liveness
/// gate (`gated_ls_status_headless`, applied in `run_inner` over the sessions Vec
/// BEFORE the emit) reaches the JSON surface, not just the human table.
///
/// This is the contract FRAME's new liveness fold depends on (qd–qf scope-1: frame
/// consumes `qd ls --json` status instead of the retired mux/session-liveness log,
/// and does NO pid probe of its own — a local kill(pid,0) is fleet-unsound over
/// mirror rows, so the owning host must publish the truth). Existing coverage pins
/// only the gate FUNCTION (unit) and the `--live` FILTER (cold-by-fixture); NONE
/// asserted the `--json` emit demotes a dead-pid-but-idle-status row. A future
/// `ls` refactor moving/removing that gate would silently break frame's crash
/// detection with no red — this closes that gap.
#[test]
fn ls_json_emit_demotes_a_dead_pid_local_row_to_not_live() {
    let t = tempfile::tempdir().unwrap();
    let j = jail(t.path());
    // A dead-pid row whose on-disk status is `idle`, plus a genuinely-live row as a
    // control (proves the gate demotes the dead one WITHOUT demoting the live one).
    let dead_pid = j.write_dead_pid_live_row("dead0000-aaaa-4aaa-8aaa-000000000001", "crashed", UPDATED_MS);
    j.write_row(1, "live0000-aaaa-4aaa-8aaa-000000000002", "alive", UPDATED_MS);

    // The on-disk row genuinely says `idle` — the demotion is the GATE, not the fixture.
    let disk = std::fs::read_to_string(
        j.home.join(".claude").join("sessions").join(format!("{dead_pid}.json")),
    )
    .unwrap();
    assert!(disk.contains(r#""status":"idle""#), "the on-disk status is idle: {disk}");

    // `--json` is the surface frame reads (engine::ls_rows shells `ls --all --json`).
    // `--all` so a demoted-to-cold row is still LISTED (the default view would hide it).
    let (code, out, err) = j.run(&["ls", "--all", "--json"]);
    assert_eq!(code, 0, "ls --all --json exit 0 (stderr: {err})");
    let r = rows(&out);
    let by_name = |n: &str| r.iter().find(|row| row["name"].as_str() == Some(n)).cloned();

    let crashed = by_name("crashed").unwrap_or_else(|| panic!("the crashed row is listed: {out}"));
    let crashed_status = crashed["status"].as_str().unwrap_or("");
    assert!(
        !matches!(crashed_status, "idle" | "busy" | "shell"),
        "R19c: a dead-pid local row must NOT emit a live status in --json — got {crashed_status:?} ({out})"
    );
    assert_eq!(
        crashed_status, "cold",
        "the dead-pid row is demoted to `cold` at the JSON emit surface: {out}"
    );

    // Control: the genuinely-alive row is untouched (still idle) — the gate is
    // targeted, not a blanket cold-everything.
    let alive = by_name("alive").unwrap_or_else(|| panic!("the alive row is listed: {out}"));
    assert_eq!(
        alive["status"].as_str().unwrap_or(""),
        "idle",
        "a genuinely-alive row keeps its live status: {out}"
    );
}
