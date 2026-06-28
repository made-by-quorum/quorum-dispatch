//! Shared helpers for the A1 integration tests (spec §9).
//!
//! L9a discipline: every `QdPaths` a test builds is rooted at a TEMP home; this
//! module's [`assert_not_real_home`] panics if a constructed home ever equals the
//! real `$HOME` (or its `.claude` subtree). No test ever resolves the real home.

#![allow(dead_code)]

pub mod p0bins;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use dispatch::effects::{FixtureProcessTable, FixtureRelayProbe, MapEnv, ProcInfo};
use dispatch::mux::FixtureMux;
use dispatch::paths::QdPaths;

/// The committed fixtures dir, resolved via `CARGO_MANIFEST_DIR` (NEVER the real
/// home).
pub fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// L9a guard: assert a home path is NOT the real `$HOME` (nor a parent of its
/// `.claude`). Tests call this for every `QdPaths` they build.
pub fn assert_not_real_home(home: &Path) {
    if let Ok(real) = std::env::var("HOME") {
        let real = PathBuf::from(real);
        let real_canon = fs::canonicalize(&real).unwrap_or(real.clone());
        let home_canon = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        assert_ne!(
            home_canon, real_canon,
            "L9a: a test must NEVER root QdPaths at the real $HOME ({real:?})"
        );
        // Also ensure we're not pointing at the real ~/.claude tree.
        assert_ne!(
            home_canon,
            real_canon.join(".claude"),
            "L9a: a test must NEVER target the real ~/.claude"
        );
    }
}

/// Recursively copy `src` into `dst` (dirs created as needed). `.gitkeep` files
/// are copied too (harmless).
pub fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

/// Set a file's mtime to `epoch_ms` (UTC), deterministically (libc `utimes`).
/// Used so transcript mtimes — which drive cold-row sort + stray badges — are
/// frozen regardless of checkout time.
pub fn set_mtime_ms(path: &Path, epoch_ms: i64) {
    let secs = epoch_ms.div_euclid(1000);
    let usecs = epoch_ms.rem_euclid(1000) * 1000;
    let cpath = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
    // Set both atime and mtime to the same value.
    let tv = [
        libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: usecs as libc::suseconds_t,
        },
        libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: usecs as libc::suseconds_t,
        },
    ];
    // SAFETY: cpath is a valid NUL-terminated path; tv is a 2-element array.
    let rc = unsafe { libc::utimes(cpath.as_ptr(), tv.as_ptr()) };
    assert_eq!(rc, 0, "utimes failed for {path:?}");
}

/// A hermetic test home: the fixture copied into a tempdir, with transcript
/// mtimes frozen. Holds the `TempDir` so it lives for the test's duration.
pub struct TestHome {
    pub temp: tempfile::TempDir,
    pub paths: QdPaths,
}

impl TestHome {
    /// Copy a committed fixture (e.g. `"home-basic"`) into a fresh tempdir and
    /// build `QdPaths`. Asserts L9a discipline.
    pub fn from_fixture(name: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        copy_dir(&fixtures_root().join(name), &home);
        assert_not_real_home(&home);
        let paths = QdPaths::from_home(&home);
        TestHome { temp, paths }
    }

    /// Freeze the mtimes of the `home-basic` transcripts to deterministic values
    /// (so cold-row ordering + stray badges are byte-stable). Newest → oldest.
    pub fn freeze_basic_mtimes(&self) {
        let p = &self.paths.projects_dir;
        let set = |rel: &str, ms: i64| set_mtime_ms(&p.join(rel), ms);
        // Distinct, newest-first for a meaningful cold-row sort.
        set("-work-projE/stray-eeee-0005.jsonl", 1_717_500_000_000);
        set("-work-projA/live-aaaa-0001.jsonl", 1_717_495_300_000);
        set("-work-projB/live-bbbb-0002.jsonl", 1_717_485_100_000);
        set("-work-projC/dead-cccc-0003.jsonl", 1_717_490_000_000);
        set("-work-projD/dead-dddd-0004.jsonl", 1_717_480_000_000);
    }
}

/// The canned `zmx list` text for the `home-basic` canonical dir: a healthy task
/// (`alpha-zmx`, pid 1500 — an ancestor of claude pid 1001 via the test ppid_map)
/// plus an ENDED task (filtered out by `is_attachable`).
pub const BASIC_ZMX_CANONICAL: &str = "\
name=alpha-zmx\tpid=1500\tclients=1\tcreated=1717495000\tstart_dir=/work/projA\tcmd=claude
name=ended-task\tpid=1600\tclients=0\tcreated=1717400000\tstart_dir=/work/old\tcmd=claude\tended=1717400500\texit_code=0
";

/// The canned `zmx list` text for a LEGACY dir: repeats `alpha-zmx` with a
/// DIFFERENT pid (1599) so canonical-wins dedupe keeps the canonical 1500.
pub const BASIC_ZMX_LEGACY: &str = "\
name=alpha-zmx\tpid=1599\tclients=0\tcreated=1717400000\tstart_dir=/legacy\tcmd=claude
";

/// Build the `FixtureMux` for `home-basic`: the canonical zmx dir resolved from
/// the given env + the legacy `/tmp` candidate dir. The `env`/`tmp_root` must be
/// the SAME ones passed to `gather` so the dir keys line up.
pub fn basic_mux(canonical: &Path, legacy: &Path) -> FixtureMux {
    FixtureMux::new()
        .with_dir(canonical.to_path_buf(), BASIC_ZMX_CANONICAL)
        .with_dir(legacy.to_path_buf(), BASIC_ZMX_LEGACY)
}

/// The `home-basic` ppid_map:
///   - claude 1001 → 1500 (zmx-tracked shell): live row matches `alpha-zmx`.
///   - relay 2001 → 1001: the relay's port maps onto claude 1001.
///   - stray claude 3001 → 1 (init): a live unmanaged proc.
pub fn basic_ppids() -> HashMap<i32, i32> {
    [(1001, 1500), (1500, 1), (2001, 1001), (1002, 1), (3001, 1)]
        .into_iter()
        .collect()
}

/// The `home-basic` process table: live claude pids 1001/1002 (registry-owned)
/// + the stray claude 3001 with cwd `/work/projE`.
pub fn basic_process_table() -> FixtureProcessTable {
    FixtureProcessTable {
        ppids: basic_ppids(),
        alive: [1001, 1002, 1500, 2001, 3001]
            .into_iter()
            .collect::<HashSet<_>>(),
        claude: vec![
            ProcInfo {
                pid: 1001,
                ppid: 1500,
                cmd: "claude".into(),
                cwd: Some("/work/projA".into()),
                started_ms: None,
            },
            ProcInfo {
                pid: 1002,
                ppid: 1,
                cmd: "claude".into(),
                cwd: Some("/work/projB".into()),
                started_ms: None,
            },
            // The stray: live claude, cwd maps to -work-projE, NOT registry-owned.
            ProcInfo {
                pid: 3001,
                ppid: 1,
                cmd: "claude".into(),
                cwd: Some("/work/projE".into()),
                started_ms: None,
            },
        ],
    }
}

/// An env whose `ZMX_DIR` pins the canonical socket dir to `canonical` (so the
/// resolved dir is deterministic and independent of the host's TMPDIR/XDG).
pub fn env_with_zmx_dir(canonical: &Path) -> MapEnv {
    MapEnv {
        vars: [(
            "ZMX_DIR".to_string(),
            canonical.to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect(),
        uid: 501,
    }
}

/// The relay probe is unused for `home-basic` (the sidecar is present); an empty
/// probe asserts the sidecar path is taken.
pub fn empty_probe() -> FixtureRelayProbe {
    FixtureRelayProbe(Vec::new())
}
