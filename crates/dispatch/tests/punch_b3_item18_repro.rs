//! punch item 18 (b3-kill-spec) LIVE PIN — the zmx pty-reuse cwd hijack,
//! FLIPPED, against REAL zmx in a scratch socket dir.
//!
//! THE HIJACK (B1 r2, re-validated in the b3 probe against zmx 0.6.0):
//! `zmx run <name>` where a SAME-NAME row exists in ENDED state does not
//! create a fresh pane — it types the new command into the OLD pty and
//! IGNORES the cwd argument, so the "new" session runs in the dead session's
//! directory (and the list row reports the REQUESTED start_dir while the
//! shell sits elsewhere — the metadata lies). Both create-side name gates
//! read the FILTERED list, where ended rows are invisible.
//!
//! THE FIX (create.rs step 3.5): pre-run ended-row reap — an exact-name row
//! whose own row says ended (identity-positive death evidence) is killed at
//! the canonical dir, the engine waits (bounded) for the row to clear (the
//! dying-socket window swallows immediate re-runs — observed 3/3 in the b3
//! probe), and only then launches.
//!
//! THE PIN: `start --dir` into a name whose previous session ENDED lands in
//! the REQUESTED cwd. Driven at the `run_new` lib level with the REAL
//! `ZmxMux<RealExec>` and a scratch socket dir (jail dirs only — never the
//! live fleet); the "claude" is a script that records its `pwd`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dispatch::create::{run_new, NewDeps, NewParams, OkBootWaiter};
use dispatch::effects::{MapEnv, RealClock};
use dispatch::exec::RealExec;
use dispatch::launch::RenderMode;
use dispatch::mux::Mux;
use dispatch::paths::QdPaths;
use dispatch::zmx_mux::ZmxMux;

/// Bounded poll helper.
fn wait_for(budget: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + budget;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

#[test]
fn ended_same_name_start_lands_in_requested_cwd() {
    // zmx must be drivable (the local-gate boxes carry it; fail LOUD, never a
    // silent vacuous pass — the require_bins posture).
    let zmx_ok = std::process::Command::new("zmx")
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(zmx_ok, "zmx not drivable on PATH — this pin needs real zmx");

    // Scratch jail under a SHORT literal-/tmp base (zmx socket sun_path
    // budget). NEVER the live fleet's dirs.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = PathBuf::from(format!("/tmp/qd-b3i18/{nanos}"));
    let zmx_dir = root.join("zmx");
    let home = root.join("home");
    let cwd_a = root.join("cwd-a");
    let cwd_b = root.join("cwd-b");
    for d in [&zmx_dir, &home, &cwd_a, &cwd_b] {
        std::fs::create_dir_all(d).unwrap();
    }

    // The fake "claude": records its working directory, then holds the pane
    // alive briefly so the I6 verify sees a live row.
    let pwd_out = root.join("pwd.out");
    let fake = root.join("fake-claude.sh");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\npwd > '{}'\nsleep 30\n", pwd_out.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let exec = RealExec;
    let mux = ZmxMux::new(RealExec);

    // Stage the trap: a session that ENDS, started in cwd A.
    let r = mux
        .run_detached(&zmx_dir, "hij", "true", &cwd_a)
        .expect("stage run");
    assert_eq!(r.status, Some(0), "stage zmx run failed: {}", r.stderr);
    let ended = wait_for(Duration::from_secs(10), || {
        mux.list_raw(&zmx_dir)
            .unwrap_or_default()
            .iter()
            .any(|z| z.name == "hij" && z.ended.is_some())
    });
    assert!(ended, "staged session never reached ended state");

    // Drive the REAL create pipeline with the requested cwd = B.
    let env = MapEnv {
        vars: [(
            "CLAUDE_BIN".to_string(),
            fake.to_string_lossy().into_owned(),
        )]
        .into(),
        uid: 501,
    };
    let clock = RealClock;
    let paths = QdPaths::from_home(&home);
    let deps = NewDeps {
        mux: &mux,
        exec: &exec,
        env: &env,
        clock: &clock,
        paths: &paths,
        canonical_dir: zmx_dir.clone(),
        legacy_dirs: vec![],
        boot_waiter: &OkBootWaiter,
        provider: &dispatch::provider::ClaudeProvider,
        backend: dispatch::mux_selector::Backend::Zmx,
    };
    let params = NewParams {
        name: "hij".to_string(),
        agent: None,
        resume: None,
        fork: false,
        claude_args: vec![],
        model: None,
        cwd: cwd_b.clone(),
        backend_env: vec![],
        backend_env_unset: vec![],
        qd_session_id: None,
        render: RenderMode::Inline,
        interactive: false,
        control_socket: None,
    };
    let out = run_new(&deps, &params).expect("run_new must succeed after the ended-row reap");
    assert_eq!(out.name, "hij");

    // THE PIN: the pane's shell runs in the REQUESTED cwd (B), not the dead
    // session's (A). Compare canonicalized (macOS /tmp → /private/tmp).
    let landed = wait_for(Duration::from_secs(10), || pwd_out.exists());
    assert!(landed, "fake claude never recorded its pwd");
    let recorded = std::fs::read_to_string(&pwd_out).unwrap();
    let recorded = Path::new(recorded.trim());
    let want = std::fs::canonicalize(&cwd_b).unwrap();
    assert_eq!(
        std::fs::canonicalize(recorded).unwrap_or_else(|_| recorded.to_path_buf()),
        want,
        "HIJACK: the session landed in the wrong cwd (pty reuse)"
    );

    // Teardown: kill our scratch pane, then remove the jail.
    let _ = mux.kill(&zmx_dir, "hij");
    let _ = wait_for(Duration::from_secs(5), || {
        mux.list_raw(&zmx_dir).unwrap_or_default().is_empty()
    });
    let _ = std::fs::remove_dir_all(&root);
}
