//! Registry write-path round-trip through the FULL pipeline (spec §9, gate item 3).
//!
//! Write a `RegistryEntry` carrying `backend` + `spawnedBy` into a temp home,
//! then `gather` + `join` and assert the row appears with the right fields, and
//! that re-reading the on-disk file yields the exact entry back (byte round-trip).

mod common;

use std::collections::HashMap;

use dispatch::effects::{FixedClock, FixtureProcessTable, FixtureRelayProbe, MapEnv};
use dispatch::join::{self, JoinOpts};
use dispatch::mux::FixtureMux;
use dispatch::paths::SbPaths;
use dispatch::registry::{self, RegistryEntry};

#[test]
fn registry_write_roundtrip_through_pipeline() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    common::assert_not_real_home(&home);
    let paths = SbPaths::from_home(&home);

    // Write a registry entry WITH the new schema fields.
    let entry = RegistryEntry {
        pid: Some(7777),
        session_id: Some("roundtrip-sid".into()),
        cwd: Some("/work/rt".into()),
        started_at: Some(1_717_490_000_000),
        updated_at: Some(1_717_495_000_000),
        status: Some("idle".into()),
        name: Some("rt-worker".into()),
        version: Some("2.0.0".into()),
        kind: Some("claude".into()),
        entrypoint: Some("cli".into()),
        backend: Some("zmx".into()),
        spawned_by: Some("orchestrator-7".into()),
        // codex P1, R1 (codex-p1-spec section 3.1): a populated provider field
        // round-trips through write_entry + read_entry exactly like its siblings.
        provider: Some("claude-code".into()),
        // codex P2 W4: a claude row carries NO endpoint (None) — it stays absent
        // on disk; the codex endpoint round-trip is pinned in registry.rs.
        endpoint: None,
        // scoped-ACP-CC: a healthy claude row carries no degradation latch (absent).
        transport: None,
    };
    registry::write_entry(&paths.sessions_dir, &entry).unwrap();

    // Re-read the on-disk file: byte round-trip equality (gate item 3).
    let back = registry::read_entry(&paths.sessions_dir, 7777).unwrap();
    assert_eq!(back, entry, "on-disk registry round-trips exactly");
    assert_eq!(back.backend.as_deref(), Some("zmx"));
    assert_eq!(back.spawned_by.as_deref(), Some("orchestrator-7"));

    // Now drive the FULL pipeline and confirm the row appears with the fields.
    let tmp_root = tempfile::tempdir().unwrap();
    let canonical = tmp_root.path().join("zmx-501");
    std::fs::create_dir_all(&canonical).unwrap();
    let env = MapEnv {
        vars: HashMap::from([(
            "ZMX_DIR".to_string(),
            canonical.to_string_lossy().into_owned(),
        )]),
        uid: 501,
    };
    let mux = FixtureMux::new();
    let pt = FixtureProcessTable::default();
    let probe = FixtureRelayProbe(Vec::new());
    let clock = FixedClock(1_717_500_000_000);
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    };
    let inputs = join::gather(
        &paths,
        &mux,
        &env,
        &pt,
        &probe,
        &clock,
        tmp_root.path(),
        None, // hermetic: suppress the machine-global XDG-family scan.
        opts,
    );
    let sessions = join::join_sessions(&inputs, opts);

    let row = sessions
        .iter()
        .find(|s| s.session_id == "roundtrip-sid")
        .expect("the written registry entry appears as a session row");
    assert_eq!(row.pid, Some(7777));
    assert_eq!(row.name.as_deref(), Some("rt-worker"));
    assert_eq!(row.status, dispatch::model::SessionStatus::Idle);
    assert_eq!(row.version.as_deref(), Some("2.0.0"));
    assert_eq!(row.cwd.as_deref(), Some("/work/rt"));
    assert_eq!(row.last_active_ms, Some(1_717_495_000_000));
}
