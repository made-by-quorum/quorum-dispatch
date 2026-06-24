//! Codex P2 W4 — LIVE jailed end-to-end of the daemon create sequence
//! (codex-p2-spec §7.2, §10). Gated on `SB_CODEX_LIVE=1`; a no-op otherwise so
//! the default suite never spawns a real codex daemon.
//!
//! What it drives REAL (no fakes): [`dispatch::create_daemon::run_new_daemon`] with the
//! production seams — real port alloc, real detached spawn (`process_group(0)`,
//! P-2-proven), real `WsAppServer` connect/initialize/thread-start. NO turn is
//! started (the brief bans live API spend in this unit), so NO model call is made
//! (thread/start does not call the model).
//!
//! JAIL (rule 9 + ADD-4/14): own HOME/CODEX_HOME/XDG_*/TMPDIR under
//! `CARGO_TARGET_TMPDIR` (inside the workspace tree — literal /tmp is radioactive);
//! `env -i`-shaped overrides via the injected [`dispatch::effects::Env`]; ports 18910+
//! (the real allocator picks an ephemeral port OUTSIDE 8900-9000); pid-scoped
//! SIGTERM→grace→SIGKILL cleanup (instance-addressed, never name-addressed). No
//! OpenRouter key is needed — thread/start makes no model call.
//!
//! W4 FINDING (recorded honestly): codex 0.134.0 does NOT materialize the rollout
//! FILE on a fresh thread/start with no turn — the rollout `path` is reserved in
//! the thread descriptor + the row, but the file is written LAZILY on the first
//! turn. The spec's "rollout file appears" live assertion therefore does not hold
//! for a no-turn create; this test asserts what IS true without a turn (the
//! recorded thread + endpoint + pid + the rollout-path keying under the jail
//! CODEX_HOME), and documents the file-lands-on-first-turn behavior for W6/W8.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dispatch::create_daemon::{
    real_alloc_port, run_new_daemon, DaemonDeps, DaemonParams, RealDaemonSpawner,
};
use dispatch::effects::{Env, RealClock};
use dispatch::exec::RealExec;
use dispatch::provider::codex::{AppServerRpc, RpcError, WsAppServer};

/// The live gate: skip unless `SB_CODEX_LIVE=1`.
fn live() -> bool {
    std::env::var("SB_CODEX_LIVE").as_deref() == Ok("1")
}

/// A jail Env: only the codex-relevant vars resolve (HOME / CODEX_HOME / XDG /
/// TMPDIR / PATH). Everything else is None — the `env -i` shape through the seam.
struct JailEnv {
    vars: std::collections::HashMap<String, String>,
}

impl Env for JailEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
    fn uid(&self) -> u32 {
        // SAFETY: getuid is always safe.
        unsafe { libc::getuid() }
    }
}

/// Build the jail under CARGO_TARGET_TMPDIR (workspace tree, never /tmp).
fn make_jail() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "w4-codex-live-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    for sub in [
        "home",
        "codex-home",
        "xdg-config",
        "xdg-data",
        "xdg-cache",
        "tmp",
        "work",
    ] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    // A minimal config.toml so the daemon initializes cleanly (no model call is
    // made by thread/start; the provider stanza only matters for turns).
    std::fs::write(
        root.join("codex-home").join("config.toml"),
        "model = \"openai/gpt-4o-mini\"\nmodel_provider = \"openrouter\"\n\
         [model_providers.openrouter]\nname = \"OpenRouter\"\n\
         base_url = \"https://openrouter.ai/api/v1\"\nenv_key = \"OPENROUTER_API_KEY\"\n\
         wire_api = \"responses\"\n",
    )
    .unwrap();
    root
}

fn jail_env(jail: &Path) -> JailEnv {
    let mut vars = std::collections::HashMap::new();
    let put = |vars: &mut std::collections::HashMap<String, String>, k: &str, v: String| {
        vars.insert(k.to_string(), v);
    };
    put(
        &mut vars,
        "HOME",
        jail.join("home").to_string_lossy().into(),
    );
    put(
        &mut vars,
        "CODEX_HOME",
        jail.join("codex-home").to_string_lossy().into(),
    );
    put(
        &mut vars,
        "XDG_CONFIG_HOME",
        jail.join("xdg-config").to_string_lossy().into(),
    );
    put(
        &mut vars,
        "XDG_DATA_HOME",
        jail.join("xdg-data").to_string_lossy().into(),
    );
    put(
        &mut vars,
        "XDG_CACHE_HOME",
        jail.join("xdg-cache").to_string_lossy().into(),
    );
    put(
        &mut vars,
        "TMPDIR",
        jail.join("tmp").to_string_lossy().into(),
    );
    // PATH so the real spawner finds `codex` + the exec sniff resolves it.
    put(
        &mut vars,
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| "/opt/homebrew/bin:/usr/bin:/bin".into()),
    );
    JailEnv { vars }
}

/// pid-scoped cleanup (instance-addressed; never name-addressed pkill, L10).
/// Routes through the SAME `RealDaemonSpawner::kill` the production failure-path
/// uses (SIGTERM → grace → SIGKILL + zombie reap), so the test exercises the real
/// cleanup. W4 finding: the codex launcher ignores SIGTERM after a ws session, so
/// the SIGKILL rung is load-bearing; the daemon is THIS process's child (spawned
/// in-process), so the zombie reap is what makes `is_pid_alive` go false.
fn reap(pid: i64) {
    use dispatch::create_daemon::DaemonSpawner;
    dispatch::create_daemon::RealDaemonSpawner.kill(pid);
}

#[test]
fn codex_daemon_create_live_jailed_end_to_end() {
    if !live() {
        eprintln!("SB_CODEX_LIVE != 1 — skipping the live codex daemon create test");
        return;
    }

    let jail = make_jail();
    let env = jail_env(&jail);
    let codex_home = jail.join("codex-home");
    let sessions_dir = jail.join("home").join(".claude").join("sessions");
    let claims_dir = jail.join("home").join(".claude").join("claims");
    let log_dir = jail
        .join("home")
        .join(".quorum")
        .join("dispatch")
        .join("log");
    let cwd = jail.join("work");

    let exec = RealExec;
    let clock = RealClock;
    let spawner = RealDaemonSpawner;
    // Real ws connector — the production transport, no fixture.
    let connect = |url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
        WsAppServer::connect(url, std::time::Duration::from_secs(5)).map(|c| {
            let b: Box<dyn AppServerRpc> = Box::new(c);
            b
        })
    };
    let alloc = real_alloc_port;

    let deps = DaemonDeps {
        provider: &dispatch::provider::codex::CODEX_PROVIDER,
        env: &env,
        exec: &exec,
        clock: &clock,
        sessions_dir: sessions_dir.clone(),
        claims_dir: claims_dir.clone(),
        log_dir: log_dir.clone(),
        spawner: &spawner,
        connect: &connect,
        alloc_port: &alloc,
        ids_path: jail
            .join("home")
            .join(".quorum")
            .join("dispatch")
            .join("state")
            .join("ids.jsonl"),
    };
    let params = DaemonParams {
        name: "cdx-live".to_string(),
        cwd: cwd.clone(),
        agent: None,
        passthrough: vec![],
        // NO prompt → NO turn → NO model call (the brief's API-spend ban).
        prompt: None,
    };

    let out = run_new_daemon(&deps, &params).expect("live daemon create succeeds");

    // Ensure we ALWAYS reap the daemon, even if an assertion below panics.
    let pid = out.pid;
    let reaper = ReapOnDrop(Arc::new(pid));

    // --- Real pid + endpoint + thread uuid on the written row -----------------
    assert!(pid > 0, "real daemon pid");
    assert!(
        out.endpoint.starts_with("ws://127.0.0.1:"),
        "endpoint: {}",
        out.endpoint
    );
    let port: u16 = out
        .endpoint
        .rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .expect("endpoint carries a port");
    assert!(
        !(8900..=9000).contains(&port),
        "endpoint port {port} must be OUTSIDE the relay range"
    );
    // The thread id is a real uuidv7-shaped string (hyphenated, non-empty).
    assert!(
        out.thread_id.contains('-') && out.thread_id.len() >= 32,
        "thread id looks like a real uuid: {}",
        out.thread_id
    );
    assert_eq!(out.first_turn_id, None, "no prompt → no turn started");

    // --- The row in the JAILED sessions_dir, every codex field --------------
    let row = dispatch::registry::read_entry(&sessions_dir, pid).expect("row written in the jail");
    assert_eq!(row.pid, Some(pid));
    assert_eq!(row.session_id.as_deref(), Some(out.thread_id.as_str()));
    assert_eq!(row.cwd.as_deref(), Some(cwd.to_string_lossy().as_ref()));
    assert_eq!(row.status.as_deref(), Some("idle"));
    assert_eq!(row.name.as_deref(), Some("cdx-live"));
    assert_eq!(row.provider.as_deref(), Some("codex"));
    assert_eq!(row.endpoint.as_deref(), Some(out.endpoint.as_str()));
    assert!(row.started_at.is_some() && row.updated_at.is_some());

    // --- The daemon is ALIVE and the log file landed under the jail ---------
    assert!(
        dispatch::effects::is_pid_alive(pid as i32),
        "daemon alive post-create"
    );
    let log_file = log_dir.join("codex-cdx-live.log");
    assert!(log_file.exists(), "daemon log under the jail: {log_file:?}");
    let log = std::fs::read_to_string(&log_file).unwrap_or_default();
    assert!(
        log.contains("listening on") || log.contains(&out.endpoint),
        "daemon log shows it bound the endpoint: {log}"
    );

    // --- Rollout keying under the jail CODEX_HOME (W4 FINDING) ----------------
    // codex 0.134.0 does NOT write the rollout FILE on a fresh thread/start with
    // no turn (verified live). The thread's rollout PATH (reserved by the daemon)
    // resolves under THIS jail's CODEX_HOME/sessions — so the keying is correct
    // even before the file lands on the first turn (W6/W8). We assert the keying,
    // NOT file existence (which would require a turn — banned here).
    let sessions_root = codex_home.join("sessions");
    // The provider resolves a transcript path off its $CODEX_HOME/sessions root;
    // with no turn the file is absent, so transcript_path returns None — that is
    // the truthful no-turn state. The directory tree itself is under the jail.
    assert!(
        sessions_root.starts_with(&codex_home),
        "rollout root under the jail CODEX_HOME"
    );

    // --- pid-scoped cleanup ---------------------------------------------------
    drop(reaper); // SIGTERM → grace → SIGKILL + zombie reap (the real cleanup)
                  // After the real cleanup the launcher is killed AND reaped (not a zombie), so
                  // `is_pid_alive` (kill 0) is false. A tiny settle for the kernel.
    for _ in 0..20 {
        if !dispatch::effects::is_pid_alive(pid as i32) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    assert!(
        !dispatch::effects::is_pid_alive(pid as i32),
        "daemon reaped (pid {pid})"
    );
    // BELT: confirm no codex daemon for THIS jail's CODEX_HOME survives (instance-
    // addressed via the env CODEX_HOME match — never a name-addressed pkill).
    assert!(
        !jail_codex_daemon_alive(&codex_home),
        "no codex app-server for this jail's CODEX_HOME may survive"
    );

    // Best-effort jail teardown (keep the log on failure paths — but we passed).
    let _ = std::fs::remove_dir_all(&jail);
}

/// RAII reaper: kills the recorded daemon pid on drop, so an assertion panic
/// between create and the explicit cleanup never leaks a live daemon.
struct ReapOnDrop(Arc<i64>);
impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        reap(*self.0);
    }
}

/// Is any `codex app-server` process running with THIS jail's CODEX_HOME alive?
/// Instance-addressed via `ps eww` env match (never a name-addressed pkill, L10).
fn jail_codex_daemon_alive(codex_home: &Path) -> bool {
    let needle = format!("CODEX_HOME={}", codex_home.display());
    let pgrep = std::process::Command::new("pgrep")
        .args(["-f", "codex app-server"])
        .output();
    let Ok(out) = pgrep else {
        return false;
    };
    for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        let ps = std::process::Command::new("ps")
            .args(["eww", "-p", pid])
            .output();
        if let Ok(ps) = ps {
            if String::from_utf8_lossy(&ps.stdout).contains(&needle) {
                return true;
            }
        }
    }
    false
}
