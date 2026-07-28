//! lsview A1: end-to-end proof that the stats cache is WIRED INTO the real
//! `join::gather` seam (not just correct as a module).
//!
//! The module-level unit tests (`src/stats_cache.rs`) prove the cache logic
//! hermetically via an injected counting reader. These tests prove the OTHER
//! half: that `gather` actually persists and consults the cache, and that a warm
//! gather serves cached stats WITHOUT re-reading transcript content — using the
//! real gather → join → render pipeline against the frozen `home-basic` fixture.

mod common;

use std::path::Path;

use dispatch::join::{self, JoinOpts};
use dispatch::render;

use common::{basic_mux, basic_process_table, empty_probe, env_with_zmx_dir, TestHome};

const CACHE_FILE: &str = "ls-stats-cache.json";

/// Everything a `join::gather` call needs, bound to one hermetic `home-basic`.
struct Harness {
    home: TestHome,
    _tmp_root: tempfile::TempDir,
    canonical: std::path::PathBuf,
    legacy: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let home = TestHome::from_fixture("home-basic");
        home.freeze_basic_mtimes();
        let tmp_root = tempfile::tempdir().unwrap();
        let canonical = tmp_root.path().join("canonical").join("zmx-501");
        let legacy = tmp_root.path().join("legacy-ctx").join("zmx-501");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        Harness {
            home,
            _tmp_root: tmp_root,
            canonical,
            legacy,
        }
    }

    /// Run the FULL pipeline (`gather` → `join_with_strays` → `assign_codes` →
    /// `ls_json`) and return the rendered `ls --json` text. Each call is an
    /// independent `qd ls`: a fresh gather that loads, consults, and persists the
    /// cache exactly as production does.
    fn render_ls(&self, opts: JoinOpts) -> String {
        let env = env_with_zmx_dir(&self.canonical);
        let mux = basic_mux(&self.canonical, &self.legacy);
        let pt = basic_process_table();
        let probe = empty_probe();
        let clock = dispatch::effects::FixedClock(1_717_500_300_000);
        let inputs = join::gather(
            &self.home.paths,
            &mux,
            &env,
            &pt,
            &probe,
            &clock,
            self._tmp_root.path(),
            None,
            opts,
        );
        let (mut sessions, strays) = join::join_with_strays(&inputs, opts);
        join::assign_codes(&mut sessions);
        render::to_pretty(&render::ls_json(&sessions, &strays))
    }

    fn cache_path(&self) -> std::path::PathBuf {
        self.home.paths.state_dir.join(CACHE_FILE)
    }

    fn transcript(&self, rel: &str) -> std::path::PathBuf {
        self.home.paths.projects_dir.join(rel)
    }
}

fn opts() -> JoinOpts {
    JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    }
}

#[test]
fn gather_persists_the_cache_and_renders_identically_warm() {
    let h = Harness::new();
    assert!(!h.cache_path().exists(), "no cache before the first gather");

    // COLD gather: reads every transcript, persists the snapshot.
    let cold = h.render_ls(opts());
    assert!(
        h.cache_path().exists(),
        "the wired gather persisted the stats cache to the state dir"
    );

    // WARM gather over the unchanged home: byte-identical rendered output.
    let warm = h.render_ls(opts());
    assert_eq!(cold, warm, "warm gather renders byte-identically to cold");
}

#[test]
fn warm_gather_serves_cached_stats_without_reading_content() {
    // The seam-level analog of the module's zero-reads test: after a cold gather
    // caches a rich live row, we swap that transcript's CONTENT for same-size
    // garbage and RESTORE its mtime, leaving (path, mtime, size) unchanged. A warm
    // gather keyed on that triple MUST hit the cache and serve the ORIGINAL stats;
    // if it re-read the file it would parse the garbage to zeroed defaults and the
    // rendered row (turns=2, tokens=12500, name, previews) would collapse. An
    // identical render proves the wired gather skipped the content read.
    let h = Harness::new();
    let victim = h.transcript("-work-projA/live-aaaa-0001.jsonl");

    // Establish the probe bites: the original transcript is richly non-default.
    let orig_stats = dispatch::jsonl::read_stats(&victim, true);
    assert_ne!(
        orig_stats,
        dispatch::jsonl::JsonlStats::default(),
        "victim transcript is non-default (turns/tokens/previews present)"
    );

    let cold = h.render_ls(opts()); // caches the victim's real stats
    assert!(h.cache_path().exists());

    // Same-size garbage + restored mtime → the (path, mtime, size) key is unchanged.
    let original_bytes = std::fs::read(&victim).unwrap();
    let orig_mtime = std::fs::metadata(&victim).unwrap().modified().unwrap();
    let garbage = vec![b'x'; original_bytes.len()];
    std::fs::write(&victim, &garbage).unwrap();
    restore_mtime(&victim, orig_mtime);

    // Sanity: a DIRECT read of the swapped file now yields collapsed stats (the
    // render-relevant fields go to zero/empty) — so a re-reading gather WOULD
    // change the output. The cache is the only thing that can keep it stable.
    let swapped = dispatch::jsonl::read_stats(&victim, true);
    assert_eq!(swapped.turns, 0, "garbage → turns collapse");
    assert_eq!(swapped.tokens, 0, "garbage → tokens collapse");
    assert_eq!(swapped.name, None, "garbage → name collapse");
    assert!(
        orig_stats.turns != 0 || orig_stats.tokens != 0,
        "the probe must bite: original had non-zero turns/tokens"
    );

    let after_swap = h.render_ls(opts());
    assert_eq!(
        cold, after_swap,
        "warm gather served the CACHED stats, never re-read the transcript content"
    );
}

/// Set `path`'s mtime back to `when` (std-only; no filetime dep).
fn restore_mtime(path: &Path, when: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

// ===========================================================================
// lsview A1 F1 — the hermetic LIVE-CODEX fixture the round-1 red-team noted the
// harness lacked. A codex registry row with a live-reported pid + a rollout
// resolvable under a fixture `CODEX_HOME`, driven through the SAME gather → join
// → render pipeline. Proves the codex live-row STATUS is served from the shared
// cache: a warm gather over the unchanged rollout re-reads NOTHING, and a
// same-size/mtime content swap that would FLIP `derive_status` (Busy → Idle)
// leaves the rendered status stable — the render-stability instrument the
// change-order names, sensitive to exactly the read F1 named.
// ===========================================================================

use std::collections::HashMap;
use dispatch::effects::{FixedClock, MapEnv};

/// The codex thread uuid (8-4-4-4-12 hex — `parse_filename` validates the shape).
const CODEX_UUID: &str = "019ea0b3-04d3-7400-8d95-f55d41e961e4";
/// The live-reported codex pid (added to the fixture process table's alive set).
const CODEX_PID: i64 = 9001;

/// A BUSY rollout: one completed turn (turns=1) + a token_count (occupancy) +
/// a SECOND still-open `task_started` (no matching complete) ⇒ `derive_status`
/// = Busy. The completed turn and occupancy make the row richly non-default so
/// the stats half is exercised alongside the status half.
fn busy_rollout() -> String {
    [
        r#"{"timestamp":"2026-06-07T06:09:20.000Z","type":"session_meta","payload":{"id":"019ea0b3-04d3-7400-8d95-f55d41e961e4","cwd":"/work/codexproj"}}"#,
        r#"{"timestamp":"2026-06-07T06:09:21.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        r#"{"timestamp":"2026-06-07T06:09:25.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":4200}}}}"#,
        r#"{"timestamp":"2026-06-07T06:09:26.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
        r#"{"timestamp":"2026-06-07T06:09:30.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
        r#"{"timestamp":"2026-06-07T06:09:31.000Z","type":"event_msg","payload":{"type":"agent_message","message":"still working on it"}}"#,
    ]
    .join("\n")
        + "\n"
}

/// A live-codex harness: `home-basic` (a valid paths tree) + a codex registry
/// row + a `CODEX_HOME` holding the row's rollout, gathered exactly as `qd ls`.
struct CodexHarness {
    home: TestHome,
    _tmp_root: tempfile::TempDir,
    _codex_home: tempfile::TempDir,
    canonical: std::path::PathBuf,
    legacy: std::path::PathBuf,
    codex_home_dir: std::path::PathBuf,
    rollout: std::path::PathBuf,
}

impl CodexHarness {
    fn new() -> Self {
        let home = TestHome::from_fixture("home-basic");
        home.freeze_basic_mtimes();

        // A codex live registry row (provider="codex", a live-reported pid).
        let entry = format!(
            r#"{{"pid":{CODEX_PID},"sessionId":"{CODEX_UUID}","cwd":"/work/codexproj","startedAt":1717490000000,"updatedAt":1717495200000,"status":"busy","name":"codex-worker","provider":"codex"}}"#,
        );
        let sess_file = home.paths.sessions_dir.join(format!("{CODEX_PID}.json"));
        std::fs::create_dir_all(&home.paths.sessions_dir).unwrap();
        std::fs::write(&sess_file, entry).unwrap();

        // A CODEX_HOME with the rollout under sessions/YYYY/MM/DD (date-walk tier).
        let codex_home = tempfile::tempdir().unwrap();
        let codex_home_dir = codex_home.path().to_path_buf();
        let day = codex_home_dir.join("sessions").join("2026").join("06").join("07");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join(format!("rollout-2026-06-07T02-09-07-{CODEX_UUID}.jsonl"));
        std::fs::write(&rollout, busy_rollout()).unwrap();

        let tmp_root = tempfile::tempdir().unwrap();
        let canonical = tmp_root.path().join("canonical").join("zmx-501");
        let legacy = tmp_root.path().join("legacy-ctx").join("zmx-501");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();

        CodexHarness {
            home,
            _tmp_root: tmp_root,
            _codex_home: codex_home,
            canonical,
            legacy,
            codex_home_dir,
            rollout,
        }
    }

    /// The env: pins the canonical zmx dir AND `CODEX_HOME` (so the codex step
    /// resolves the fixture rollout tree, never the host's real `~/.codex`).
    fn env(&self) -> MapEnv {
        let vars: HashMap<String, String> = [
            ("ZMX_DIR".to_string(), self.canonical.to_string_lossy().into_owned()),
            ("CODEX_HOME".to_string(), self.codex_home_dir.to_string_lossy().into_owned()),
        ]
        .into_iter()
        .collect();
        MapEnv { vars, uid: 501 }
    }

    /// One `qd ls`: a fresh gather that loads, consults, and persists the cache.
    fn render_ls(&self) -> String {
        let env = self.env();
        let mux = basic_mux(&self.canonical, &self.legacy);
        // The basic claude process table + the codex pid reported LIVE.
        let mut pt = basic_process_table();
        pt.alive.insert(CODEX_PID as i32);
        let probe = empty_probe();
        let clock = FixedClock(1_717_500_300_000);
        let inputs = join::gather(
            &self.home.paths,
            &mux,
            &env,
            &pt,
            &probe,
            &clock,
            self._tmp_root.path(),
            None,
            opts(),
        );
        let (mut sessions, strays) = join::join_with_strays(&inputs, opts());
        join::assign_codes(&mut sessions);
        render::to_pretty(&render::ls_json(&sessions, &strays))
    }

    fn cache_path(&self) -> std::path::PathBuf {
        self.home.paths.state_dir.join(CACHE_FILE)
    }
}

/// The `"status"` string of the codex row (sessionId == `CODEX_UUID`) in a
/// rendered `ls --json` payload — `None` if the row or field is absent.
fn codex_row_status(render: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(render).ok()?;
    let rows = v.as_array()?;
    for row in rows {
        if row.get("sessionId").and_then(|s| s.as_str()) == Some(CODEX_UUID) {
            return row.get("status").and_then(|s| s.as_str()).map(str::to_owned);
        }
    }
    None
}

#[test]
fn codex_fixture_renders_a_busy_live_row() {
    // Sanity: the fixture wiring lands a live codex row whose rollout-derived
    // status is Busy — the precondition for the swap test below to mean anything.
    let h = CodexHarness::new();
    let cold = h.render_ls();
    assert_eq!(
        codex_row_status(&cold).as_deref(),
        Some("busy"),
        "the live codex row's rollout-derived status is Busy\n{cold}"
    );
}

#[test]
fn warm_gather_serves_cached_codex_status_without_reading_content() {
    // THE F1 REGRESSION TEST (red-then-green vehicle). A live codex row's status
    // derives from a FULL content read of its rollout. Pre-fix that read ran on
    // EVERY gather — warm or cold, uncached, invisible to the counter — so a warm
    // `ls` re-read every live rollout for status. Post-fix the derived status is
    // memoized in the shared cache alongside the row's stats, keyed by the same
    // (path, mtime, size).
    //
    // We cold-gather (status Busy, memoized), then swap the rollout for SAME-SIZE
    // garbage and RESTORE its mtime, leaving (path, mtime, size) UNCHANGED. A
    // re-reading gather parses garbage → `derive_status` None → the join's Idle
    // fallback → the row FLIPS to "idle". A cache HIT serves the memoized Busy →
    // the render is byte-identical. This is the status analog of
    // `warm_gather_serves_cached_stats_without_reading_content`; the flipping
    // field is STATUS — exactly the read F1 named.
    //
    // AGAINST PRE-FIX PRODUCTION this test goes RED (warm status flips Busy→idle);
    // with the fix it is GREEN. A test that passed either way would prove nothing.
    let h = CodexHarness::new();

    let cold = h.render_ls();
    assert!(h.cache_path().exists(), "the wired gather persisted the cache");
    assert_eq!(codex_row_status(&cold).as_deref(), Some("busy"), "cold: Busy");

    // Same-size garbage + restored mtime → the (path, mtime, size) key is unchanged.
    let orig_bytes = std::fs::read(&h.rollout).unwrap();
    let orig_mtime = std::fs::metadata(&h.rollout).unwrap().modified().unwrap();
    let garbage = vec![b'x'; orig_bytes.len()];
    std::fs::write(&h.rollout, &garbage).unwrap();
    restore_mtime(&h.rollout, orig_mtime);

    // The probe bites: a fresh derive over the swapped rollout is None (→ Idle) —
    // so a re-reading gather WOULD flip the status. Only the cache keeps it stable.
    assert_eq!(
        dispatch::provider::codex::derive_status(
            &dispatch::provider::codex::rollout::read_lines(&h.rollout)
        ),
        None,
        "swapped rollout derives None → a re-reading gather would show idle"
    );

    // WARM gather over the unchanged key: the cache serves the memoized Busy.
    let after_swap = h.render_ls();
    assert_eq!(
        codex_row_status(&after_swap).as_deref(),
        Some("busy"),
        "warm served the CACHED status; a re-reading gather would show idle"
    );
    assert_eq!(
        cold, after_swap,
        "warm gather served cached stats AND status — never re-read the rollout content"
    );
}
