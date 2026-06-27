//! A1 golden-parity integration tests (spec §9, gate items 1-2, 5-6).
//!
//! Pipeline under test: `join::gather` (I/O against the frozen fixture home +
//! `FixtureMux`/`FixtureProcessTable`) → `join::join_with_strays` (pure decider)
//! → `join::assign_codes` → `render::ls_json` / `render::info_text`.
//!
//! ## Golden verification (gate item 2 — how the frozen files were validated)
//!
//! The `golden/*.json|*.txt` files were generated ONCE by this pipeline
//! (`QD_REGEN_GOLDEN=1`), then HAND-VERIFIED field-by-field against the TS
//! semantics before freezing:
//!   - Per-branch key SET + ORDER vs session.ts:913-933 (live), 960-977 (cold —
//!     jsonlPath BEFORE gitBranch), 981-995 (zmx-only — no userNamed, sessionId
//!     ""), 1022-1038 (tombstone). `code` is LAST in every object (index.ts:55).
//!   - Date strings are `Date.toJSON` ISO-8601 ms UTC (verified vs bun).
//!   - `lastActive` for live rows = `updatedAt`; for cold rows = the JSONL
//!     `lastTimestamp` (deterministic, not mtime).
//!   - The cold row for `dead-cccc-0003` AND its killed tombstone row BOTH appear
//!     in `--all`: the TS cold loop never inserts into `seenSessionIds`
//!     (session.ts:949-978), so a tombstoned-but-not-live sessionId surfaces in
//!     both the cold and the killed branch. Ported honestly; frozen as-is.
//!   - The `alpha-worker` row carries `relayPort: 8901` (sidecar pid 2001 →
//!     claude 1001 via the ppid_map) and zmx `alpha-zmx` (pid 1500, an ancestor
//!     of 1001) with `socketDir` = the canonical dir; the legacy dup pid 1599 is
//!     dropped by canonical-wins.
//!   - Strays (`epsilon-stray`) are appended after the TS rows as
//!     `status: "unmanaged"` objects (PROVISIONAL shape, render.rs).
//!
//! Re-freeze by running with `QD_REGEN_GOLDEN=1` (writes the files) and
//! re-verifying by hand — expected at pass (b) when the fix-wave lands.

mod common;

use std::path::{Path, PathBuf};

use dispatch::join::{self, JoinOpts};
use dispatch::render;

use common::{basic_mux, basic_process_table, empty_probe, env_with_zmx_dir, TestHome};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Read a golden file, or — when `QD_REGEN_GOLDEN=1` — write `actual` to it and
/// return it (so the first run freezes the file). Asserts byte-equality.
fn assert_golden(name: &str, actual: &str) {
    let path = golden_dir().join(name);
    if std::env::var("QD_REGEN_GOLDEN").is_ok() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, actual).unwrap();
        eprintln!("regenerated golden {name}");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {path:?}: {e} (run QD_REGEN_GOLDEN=1)"));
    assert_eq!(actual, expected, "golden mismatch for {name}");
}

/// Build a hermetic `home-basic` run: copy fixture, freeze mtimes, set up the
/// canonical + legacy zmx dirs, and run gather+join.
struct BasicRun {
    _home: TestHome,
    _tmp_root: tempfile::TempDir,
    inputs: join::JoinInputs,
    /// The volatile tempdir prefixes that [`Self::normalize`] replaces so the
    /// golden is path-stable across runs (NORMALIZATION-class, like the 0b
    /// comparator's locale lines).
    home_dir: PathBuf,
    zmx_canonical: PathBuf,
}

impl BasicRun {
    /// Replace the run's volatile absolute tempdir prefixes with stable
    /// placeholders (`<HOME>`, `<ZMX>`). `jsonlPath` carries the temp home;
    /// `socketDir` carries the temp zmx dir. Everything else is already stable.
    fn normalize(&self, text: &str) -> String {
        text.replace(&self.home_dir.to_string_lossy().into_owned(), "<HOME>")
            .replace(&self.zmx_canonical.to_string_lossy().into_owned(), "<ZMX>")
    }
}

fn run_basic(opts: JoinOpts) -> BasicRun {
    let home = TestHome::from_fixture("home-basic");
    home.freeze_basic_mtimes();
    let home_dir = home.paths.home.clone();

    // tmp_root holds the legacy zmx dir so legacy_zmx_dirs discovers it. We make
    // `<tmp_root>/legacy-ctx/zmx-501` a real dir → a legacy candidate.
    let tmp_root = tempfile::tempdir().unwrap();
    let canonical = tmp_root.path().join("canonical").join("zmx-501");
    let legacy = tmp_root.path().join("legacy-ctx").join("zmx-501");
    std::fs::create_dir_all(&canonical).unwrap();
    std::fs::create_dir_all(&legacy).unwrap();

    let env = env_with_zmx_dir(&canonical);
    let mux = basic_mux(&canonical, &legacy);
    let pt = basic_process_table();
    let probe = empty_probe();
    let clock = dispatch::effects::FixedClock(1_717_500_300_000); // ~stray mtime + 300s

    let inputs = join::gather(
        &home.paths,
        &mux,
        &env,
        &pt,
        &probe,
        &clock,
        tmp_root.path(),
        None, // hermetic: suppress the machine-global XDG-family scan.
        opts,
    );

    BasicRun {
        _home: home,
        _tmp_root: tmp_root,
        inputs,
        home_dir,
        zmx_canonical: canonical,
    }
}

// --- Gate item 2: empty → [] (matches 0b dryrun capture) ---

#[test]
fn empty_home_renders_empty_array() {
    let home = TestHome::from_fixture("home-empty");
    let tmp_root = tempfile::tempdir().unwrap();
    let canonical = tmp_root.path().join("zmx-501");
    std::fs::create_dir_all(&canonical).unwrap();
    let env = env_with_zmx_dir(&canonical);
    let mux = dispatch::mux::FixtureMux::new(); // no dirs → empty lists
    let pt = dispatch::effects::FixtureProcessTable::default();
    let probe = empty_probe();
    let clock = dispatch::effects::FixedClock(0);
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    };
    let inputs = join::gather(
        &home.paths,
        &mux,
        &env,
        &pt,
        &probe,
        &clock,
        tmp_root.path(),
        None, // hermetic: suppress the machine-global XDG-family scan.
        opts,
    );
    let (mut sessions, strays) = join::join_with_strays(&inputs, opts);
    join::assign_codes(&mut sessions);
    let value = render::ls_json(&sessions, &strays);
    let text = render::to_pretty(&value);
    assert_eq!(
        text, "[]",
        "empty home → [] (0b dryrun ls_info_json capture)"
    );
}

// --- Gate item 2: ls --json golden parity (all + default views) ---

#[test]
fn ls_json_all_golden() {
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    };
    let run = run_basic(opts);
    let (mut sessions, strays) = join::join_with_strays(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    let text = run.normalize(&render::to_pretty(&render::ls_json(&sessions, &strays)));
    assert_golden("ls-basic.json", &text);
}

#[test]
fn ls_json_default_golden() {
    // Default view: no --all → cap 20, named non-killed only, no tombstones.
    let opts = JoinOpts {
        include_all: false,
        include_tombstoned: false,
        include_preview: true,
        limit: None,
    };
    let run = run_basic(opts);
    let (mut sessions, strays) = join::join_with_strays(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    let text = run.normalize(&render::to_pretty(&render::ls_json(&sessions, &strays)));
    assert_golden("ls-default.json", &text);
}

// --- Gate item 2: info text golden ---

#[test]
fn info_alpha_golden() {
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    };
    let run = run_basic(opts);
    let (mut sessions, _strays) = join::join_with_strays(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    let alpha = sessions
        .iter()
        .find(|s| s.session_id == "live-aaaa-0001")
        .expect("alpha row present");
    // Fixed now for the relativeTime suffix.
    let text = run.normalize(&render::info_text(alpha, 1_717_500_300_000));
    assert_golden("info-alpha.txt", &text);
}

// --- Gate item 5: stray discovery (live stray listed; dead transcript NOT) ---

#[test]
fn stray_listed_dead_transcript_not() {
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    };
    let run = run_basic(opts);
    let (_sessions, strays) = join::join_with_strays(&run.inputs, opts);

    // The live stray (epsilon, cwd /work/projE, proc 3001) IS listed.
    let stray = strays
        .iter()
        .find(|s| s.session_id == "stray-eeee-0005")
        .expect("epsilon stray detected");
    assert_eq!(stray.pid, Some(3001));

    // The dead-cold transcript (delta, projD, NO live proc) is NOT a stray.
    assert!(
        !strays.iter().any(|s| s.session_id == "dead-dddd-0004"),
        "dead transcript must NOT be a stray (stays cold)"
    );

    // And in the rendered output, the stray appears with the unmanaged badge,
    // while delta appears only as a cold row.
    let mut sessions = join::join_sessions(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    let value = render::ls_json(&sessions, &strays);
    let arr = value.as_array().unwrap();
    let unmanaged: Vec<_> = arr
        .iter()
        .filter(|v| v["status"] == serde_json::json!("unmanaged"))
        .collect();
    assert_eq!(unmanaged.len(), 1, "exactly one unmanaged stray row");
    assert_eq!(
        unmanaged[0]["sessionId"],
        serde_json::json!("stray-eeee-0005")
    );
    // delta is present as a cold row, never unmanaged.
    let delta: Vec<_> = arr
        .iter()
        .filter(|v| v["sessionId"] == serde_json::json!("dead-dddd-0004"))
        .collect();
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0]["status"], serde_json::json!("cold"));
}

// --- E2E production-mode tripwire (ADD-9b red-team BLOCKER 2) ---

#[test]
fn production_mode_gather_scans_xdg_family_end_to_end() {
    // Guards against an isolated-mode-in-production regression END TO END: gather in
    // PRODUCTION mode (Some(XdgFamily)) must discover a dir under the XDG runtime
    // root and feed it to the Mux scan, so a session living there surfaces in the
    // joined zmx set. If "roots provided = isolated" ever crept back, the XDG family
    // would be suppressed here and this session would vanish — the exact C2-Lima bug.
    //
    // Hermetic: the XDG root + run_user_dir are injected temp paths, not the host's
    // real /run/user/<uid>. Nothing touches machine-global state.
    use dispatch::effects::{FixedClock, MapEnv};
    use dispatch::mux::FixtureMux;
    use dispatch::zmx_dir::XdgFamily;

    let home = TestHome::from_fixture("home-empty");
    let scan_root = tempfile::tempdir().unwrap();

    // Canonical is pinned via ZMX_DIR to a dir distinct from the XDG family, so the
    // XDG-discovered dir is a LEGACY candidate (not filtered as canonical).
    let canonical = scan_root.path().join("canonical").join("zmx-501");
    std::fs::create_dir_all(&canonical).unwrap();

    // The injected XDG runtime root with a `zmx` child on disk (the discovered dir).
    let xdg_root = tempfile::tempdir().unwrap();
    let xdg_zmx = xdg_root.path().join("zmx");
    std::fs::create_dir_all(&xdg_zmx).unwrap();

    // Register the XDG-child dir in the Mux with one attachable session.
    let xdg_session_line =
        "name=xdg-session\tpid=4242\tclients=1\tcreated=1717400000\tstart_dir=/x\tcmd=claude\n";
    let mux = FixtureMux::new()
        .with_dir(canonical.clone(), "")
        .with_dir(xdg_zmx.clone(), xdg_session_line);

    let env = MapEnv {
        vars: [(
            "ZMX_DIR".to_string(),
            canonical.to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect(),
        uid: 501,
    };
    let pt = basic_process_table();
    let probe = empty_probe();
    let clock = FixedClock(1_717_500_300_000);
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    };

    // PRODUCTION mode: Some(XdgFamily) with the injected XDG root + run_user_dir.
    let xdg = XdgFamily {
        xdg_runtime_dir: Some(xdg_root.path().to_string_lossy().into_owned()),
        run_user_dir: scan_root.path().join("run-user-501-nonexistent"),
    };
    let inputs = join::gather(
        &home.paths,
        &mux,
        &env,
        &pt,
        &probe,
        &clock,
        scan_root.path(),
        Some(&xdg),
        opts,
    );

    let found = inputs.zmx_sessions.iter().any(|z| {
        z.name == "xdg-session" && z.socket_dir.as_deref() == Some(&*xdg_zmx.to_string_lossy())
    });
    assert!(
        found,
        "the XDG-family session must surface in production-mode gather: {:?}",
        inputs.zmx_sessions
    );
}

// --- L9a discipline: the helper's injected-home assert is wired ---

#[test]
fn injected_home_discipline_assert_is_active() {
    // Constructing a TestHome runs assert_not_real_home internally; here we also
    // directly assert the guard rejects the real HOME.
    if let Ok(real) = std::env::var("HOME") {
        let result = std::panic::catch_unwind(|| {
            common::assert_not_real_home(Path::new(&real));
        });
        assert!(result.is_err(), "L9a guard must reject the real $HOME");
    }
    // A temp home passes.
    let temp = tempfile::tempdir().unwrap();
    common::assert_not_real_home(temp.path());
}
