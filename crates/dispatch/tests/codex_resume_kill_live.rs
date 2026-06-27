//! Codex P2 W7 — LIVE jailed end-to-end of the RESUME (revive) + KILL paths
//! (codex-p2-spec §7.6; ADD-26(2): resume is a first-class AGENT verb =
//! thread/resume revive-to-DRIVABLE with NO interactive-attach tail; attach is
//! SEVERED). Gated on `QD_CODEX_LIVE=1`; a no-op otherwise so the default suite
//! never spawns a real codex daemon or spends API budget.
//!
//! What it drives REAL (no fakes), the full lifecycle in ONE jail:
//!   1. CREATE a jailed `codex app-server` daemon via the production
//!      [`dispatch::create_daemon::run_new_daemon`] seams.
//!   2. SEND one tiny one-line turn (the production inject ladder over a real
//!      `WsAppServer`) so a ROLLOUT FILE materializes (W4 lazy-materialization
//!      fact: the file appears on the FIRST turn — a resumable history must exist
//!      before revive can hydrate it). Then WAIT for idle so the turn completes.
//!   3. KILL the daemon via the production [`dispatch::resume_daemon::kill_codex`] (the
//!      GROUP-pgid ladder reap + tombstone). Assert: daemon dead, the row
//!      tombstoned, NO survivor for this jail's CODEX_HOME.
//!   4. REVIVE via the production [`dispatch::resume_daemon::resume_codex`] against the
//!      now-dead row + the materialized rollout. Assert: a NEW daemon pid, the row
//!      updated (new pid/endpoint, PRESERVED thread id = m2), and the thread is
//!      DRIVABLE (thread/resume succeeded → the new daemon hydrated the rollout).
//!   5. GROUP-kill cleanup of the REVIVED daemon + a no-survivor belt at the end.
//!
//! TURN BUDGET (the brief's API-spend cap): exactly ONE live turn (a one-line
//! "Reply with exactly OK" prompt) at step 2. The revive (step 4) sends NO turn —
//! thread/resume hydrates from disk with no model call.
//!
//! JAIL (rule 9 + ADD-4/14): own HOME/CODEX_HOME/XDG_*/TMPDIR under
//! `CARGO_TARGET_TMPDIR` (workspace tree, never /tmp); ports via the real allocator
//! (ephemeral, OUTSIDE 8900-9000); the OpenRouter key read from the RUNNER's
//! `$HOME/.quorum/dispatch/config.toml`, exported into the jail env ONLY (never written to a jail
//! file, never on disk). GROUP-scoped SIGTERM→grace→SIGKILL cleanup (instance-
//! addressed by the recorded pgid — the W4 launcher-orphan finding) + no-survivor
//! belts after BOTH the kill and the revive cleanup.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dispatch::boot::RealSleeper;
use dispatch::create_daemon::{
    real_alloc_port, real_cmdline_probe, run_new_daemon, DaemonDeps, DaemonParams,
    RealDaemonSpawner,
};
use dispatch::effects::{Env, RealClock};
use dispatch::exec::RealExec;
use dispatch::provider::codex::{
    open_turn_id, read_lines, AppServerRpc, ClientInfo, CodexProvider, RolloutLine, RpcError,
    WsAppServer,
};
use dispatch::provider::{Provider, ProviderFx, SessionKey};
use dispatch::registry::RegistryEntry;
use dispatch::resume_daemon::{kill_codex, resume_codex, ResumeOutcome, ResumeParams, ReviveDeps};
use dispatch::wait::{run_codex_wait_loop, RealCodexWaitDeps, WaitStatusOutcome};

/// The live gate: skip unless `QD_CODEX_LIVE=1`.
fn live() -> bool {
    std::env::var("QD_CODEX_LIVE").as_deref() == Ok("1")
}

/// A jail Env: only the codex-relevant vars resolve. `env -i` shape via the seam.
/// The OpenRouter key rides here (exported into the spawned daemon's env), NEVER
/// written into a jail file.
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

/// Read `openrouter-key` from the RUNNER's `$HOME/.quorum/dispatch/config.toml` (the only place
/// the real key lives — never an env var on disk, never a jail file).
fn openrouter_key() -> String {
    let home = std::env::var("HOME").expect("HOME set for the live lane");
    let cfg = std::fs::read_to_string(
        PathBuf::from(home)
            .join(".quorum")
            .join("dispatch")
            .join("config.toml"),
    )
    .expect("runner ~/.quorum/dispatch/config.toml exists for the live lane");
    for line in cfg.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("openrouter-key") {
            if let Some(eq) = rest.find('=') {
                let val = rest[eq + 1..].trim().trim_matches('"').to_string();
                if !val.is_empty() {
                    return val;
                }
            }
        }
    }
    panic!("openrouter-key not found in ~/.quorum/dispatch/config.toml — the live turn needs it");
}

/// Build the jail tree under CARGO_TARGET_TMPDIR (workspace tree, never /tmp).
fn make_jail() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "w7-codex-resumekill-{}-{}",
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
    // The model_providers stanza (spike-proven, wire_api "responses"); the KEY is
    // NOT written here — env_key names the env var the daemon reads it from.
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

fn jail_env(jail: &Path, or_key: &str) -> JailEnv {
    let mut vars = std::collections::HashMap::new();
    let mut put = |k: &str, v: String| {
        vars.insert(k.to_string(), v);
    };
    put("HOME", jail.join("home").to_string_lossy().into());
    put(
        "CODEX_HOME",
        jail.join("codex-home").to_string_lossy().into(),
    );
    put(
        "XDG_CONFIG_HOME",
        jail.join("xdg-config").to_string_lossy().into(),
    );
    put(
        "XDG_DATA_HOME",
        jail.join("xdg-data").to_string_lossy().into(),
    );
    put(
        "XDG_CACHE_HOME",
        jail.join("xdg-cache").to_string_lossy().into(),
    );
    put("TMPDIR", jail.join("tmp").to_string_lossy().into());
    put("OPENROUTER_API_KEY", or_key.to_string());
    put(
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| "/opt/homebrew/bin:/usr/bin:/bin".into()),
    );
    JailEnv { vars }
}

/// GROUP-scoped cleanup via the production RealDaemonSpawner::kill (the recorded
/// pgid ladder — SIGTERM → grace → SIGKILL + zombie reap; the W4 launcher-orphan
/// finding makes the group kill load-bearing).
fn reap(pid: i64) {
    use dispatch::create_daemon::DaemonSpawner;
    dispatch::create_daemon::RealDaemonSpawner.kill(pid);
}

struct ReapOnDrop(Arc<std::sync::Mutex<i64>>);
impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        let pid = *self.0.lock().unwrap();
        if pid > 0 {
            reap(pid);
        }
    }
}

/// Is any `codex app-server` for THIS jail's CODEX_HOME alive? Instance-addressed
/// via `ps eww` env match (never a name-addressed pkill, L10).
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

/// Wait for a pid to go dead (the kernel needs a tiny settle after SIGKILL+reap).
fn wait_dead(pid: i64) {
    for _ in 0..20 {
        if !dispatch::effects::is_pid_alive(pid as i32) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// Resolve the codex transcript_root off the jail env (codex's transcript_root reads
/// `fx.env` $CODEX_HOME/$HOME — never paths; the placeholder SbPaths satisfies the
/// borrow). Owned `paths` is passed in so the borrow outlives the fx.
fn codex_root(env: &JailEnv, paths: &dispatch::paths::SbPaths) -> PathBuf {
    let fx = ProviderFx {
        env,
        paths,
        socket_dir: paths.sessions_dir.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,    };
    CodexProvider.transcript_root(&fx)
}

#[test]
fn codex_resume_kill_live_jailed_e2e() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping the live codex resume/kill test");
        return;
    }

    let or_key = openrouter_key();
    // The real spawner inherits the parent process env for vars not in the launch
    // plan; the daemon reads OPENROUTER_API_KEY from its env.
    // SAFETY: single-threaded test setup before any spawn.
    unsafe { std::env::set_var("OPENROUTER_API_KEY", &or_key) };

    let jail = make_jail();
    let env = jail_env(&jail, &or_key);
    let codex_home = jail.join("codex-home");
    let sessions_dir = jail.join("home").join(".claude").join("sessions");
    let claims_dir = jail.join("home").join(".claude").join("claims");
    let log_dir = jail
        .join("home")
        .join(".quorum")
        .join("dispatch")
        .join("log");
    let ids_path = jail
        .join("home")
        .join(".quorum")
        .join("dispatch")
        .join("state")
        .join("ids.jsonl");
    let cwd = jail.join("work");
    let cwd_str = cwd.to_string_lossy().into_owned();

    let exec = RealExec;
    let clock = RealClock;
    let spawner = RealDaemonSpawner;
    let connect = |url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
        WsAppServer::connect(url, std::time::Duration::from_secs(5)).map(|c| {
            let b: Box<dyn AppServerRpc> = Box::new(c);
            b
        })
    };
    let alloc = real_alloc_port;

    // === Step 1: CREATE the daemon =============================================
    let create_deps = DaemonDeps {
        provider: &CodexProvider,
        env: &env,
        exec: &exec,
        clock: &clock,
        sessions_dir: sessions_dir.clone(),
        claims_dir: claims_dir.clone(),
        log_dir: log_dir.clone(),
        spawner: &spawner,
        connect: &connect,
        alloc_port: &alloc,
        ids_path: ids_path.clone(),
    };
    let create_params = DaemonParams {
        name: "cdx-rk-live".to_string(),
        cwd: cwd.clone(),
        agent: None,
        passthrough: vec![],
        prompt: None, // the SEND step drives the one live turn.
    };
    let out = run_new_daemon(&create_deps, &create_params).expect("live daemon create succeeds");
    let thread_id = out.thread_id.clone();
    let endpoint = out.endpoint.clone();
    let create_pid = out.pid;

    // P0 QA (spec-w4-qa A1, codex empirical pin): create minted a stable qb id
    // and BOUND it to the REAL thread uuid (mint-unbound → bind-after-thread/start).
    let ids_at_create = dispatch::idstore::fold(&ids_path);
    let sbx_id_at_create = ids_at_create
        .by_session
        .get(&thread_id)
        .cloned()
        .expect("create bound a stable id to the live thread uuid");
    assert!(
        dispatch::idstore::is_valid_id(&sbx_id_at_create),
        "well-formed qb id: {sbx_id_at_create:?}"
    );

    // A reaper that always kills the CURRENT live daemon pid (updated across the
    // create→kill→revive transitions) so a panic never leaks a daemon.
    let live_pid = Arc::new(std::sync::Mutex::new(create_pid));
    let _reaper = ReapOnDrop(live_pid.clone());

    // === Step 2: SEND one tiny turn so a ROLLOUT FILE materializes =============
    {
        let rpc = WsAppServer::connect(&endpoint, std::time::Duration::from_secs(5))
            .expect("send: connect the daemon");
        let client = ClientInfo {
            name: "qd-manager".to_string(),
            title: None,
            version: "0".to_string(),
        };
        rpc.initialize(&client).expect("send: initialize");
        let _ = rpc.initialized();
        rpc.set_request_timeout(std::time::Duration::from_secs(60));

        let placeholder = dispatch::paths::SbPaths::from_home(Path::new(""));
        let root = codex_root(&env, &placeholder);
        let key = SessionKey {
            id: &thread_id,
            name: Some("cdx-rk-live"),
            cwd: Some(cwd_str.as_str()),
            pid: Some(create_pid),
        };
        let rollout = CodexProvider.transcript_path(&root, &key);
        let expected = rollout
            .as_ref()
            .map(|p| open_turn_id(&read_lines(p)))
            .unwrap_or(None);

        let rpc_ref: &dyn AppServerRpc = &rpc;
        let fx = ProviderFx {
            env: &env,
            paths: &dispatch::paths::SbPaths::from_home(&jail.join("home")),
            socket_dir: sessions_dir.clone(),
            mux: None,
            clock: None,
            sleeper: None,
            relay: None,
            relay_port: None,
            app_server: Some(rpc_ref),
            codex_expected_turn_id: expected.as_deref(),
            acp_client: None,        };
        let turn_id = CodexProvider
            .inject(
                &fx,
                &key,
                "Reply with exactly the text OK and nothing else.",
                "cli",
            )
            .expect("SEND: the one live turn starts");
        assert!(
            turn_id.contains('-') && turn_id.len() >= 16,
            "SEND returned a real turn id: {turn_id}"
        );
        let _ = rpc.close();
    }

    // WAIT for idle so the turn completes + the rollout has a balanced tail.
    {
        let placeholder = dispatch::paths::SbPaths::from_home(Path::new(""));
        let root = codex_root(&env, &placeholder);
        let key = SessionKey {
            id: &thread_id,
            name: Some("cdx-rk-live"),
            cwd: Some(cwd_str.as_str()),
            pid: Some(create_pid),
        };
        let rollout_path = CodexProvider.transcript_path(&root, &key);
        let connected = WsAppServer::connect(&endpoint, std::time::Duration::from_secs(5)).ok();
        if let Some(c) = &connected {
            let client = ClientInfo {
                name: "qd-manager".to_string(),
                title: None,
                version: "0".to_string(),
            };
            let _ = c.initialize(&client);
            let _ = c.initialized();
        }
        let real_clock = RealClock;
        let sleeper = RealSleeper;
        let rpc_ref: Option<&dyn AppServerRpc> = connected.as_ref().map(|c| c as &dyn AppServerRpc);
        let wdeps = RealCodexWaitDeps::new(
            rpc_ref,
            rollout_path,
            std::time::Duration::from_millis(500),
            &real_clock,
            &sleeper,
        );
        let outcome = run_codex_wait_loop(&wdeps, 90_000, 500);
        if let Some(c) = connected {
            let _ = c.close();
        }
        assert_eq!(
            outcome,
            WaitStatusOutcome::Done,
            "the turn completed → idle"
        );
    }

    // Confirm the rollout FILE now exists (the resumable history revive will hydrate).
    let rollout_after_turn = {
        let placeholder = dispatch::paths::SbPaths::from_home(Path::new(""));
        let root = codex_root(&env, &placeholder);
        let key = SessionKey {
            id: &thread_id,
            name: Some("cdx-rk-live"),
            cwd: Some(cwd_str.as_str()),
            pid: Some(create_pid),
        };
        CodexProvider
            .transcript_path(&root, &key)
            .expect("rollout file materialized after the first turn")
    };
    assert!(
        rollout_after_turn.exists() && rollout_after_turn.starts_with(&codex_home),
        "rollout under the jail CODEX_HOME: {rollout_after_turn:?}"
    );

    // === Step 3: KILL the daemon (GROUP ladder) + tombstone ====================
    let captured: Option<RegistryEntry> = dispatch::registry::read_entry(&sessions_dir, create_pid);
    assert!(captured.is_some(), "the create row exists pre-kill");
    // W9 FIX M-1: the production cmdline-identity probe — the live daemon's real
    // command line matches OUR codex daemon (codex app-server + the --listen
    // endpoint), so the group ladder fires. A reused pid would fail this and skip
    // the foreign-group signal.
    let probe = real_cmdline_probe;
    let kill_outcome = kill_codex(
        &sessions_dir,
        create_pid,
        captured.as_ref(),
        &spawner,
        &probe,
    );
    assert!(kill_outcome.was_alive, "the daemon was alive at kill time");
    wait_dead(create_pid);
    assert!(
        !dispatch::effects::is_pid_alive(create_pid as i32),
        "daemon reaped by the group ladder (pid {create_pid})"
    );
    // The row is tombstoned (the live <pid>.json was renamed).
    let tomb = sessions_dir.join(format!("{create_pid}.json.tombstoned"));
    assert!(tomb.exists(), "the killed row is tombstoned");
    assert!(
        !sessions_dir.join(format!("{create_pid}.json")).exists(),
        "the live row file was consumed by the tombstone rename"
    );
    let tomb_body = std::fs::read_to_string(&tomb).unwrap();
    assert!(
        tomb_body.contains("\"provider\": \"codex\"") && tomb_body.contains(&endpoint),
        "tombstone carries provider + endpoint: {tomb_body}"
    );
    // No survivor for THIS jail's CODEX_HOME after the kill.
    assert!(
        !jail_codex_daemon_alive(&codex_home),
        "no codex app-server survives the kill"
    );

    // === Step 4: REVIVE via resume_codex (dead row + materialized rollout) =====
    // The row is now tombstoned (dead). Build the resume params from the dead-row
    // identity: the PRESERVED thread id (m2), the cwd, the dead pid + old endpoint
    // (alive-check inputs → not alive → revive). thread/resume hydrates from the
    // on-disk rollout with NO model call.
    let revive_deps = ReviveDeps {
        provider: &CodexProvider,
        env: &env,
        exec: &exec,
        clock: &clock,
        sessions_dir: sessions_dir.clone(),
        log_dir: log_dir.clone(),
        spawner: &spawner,
        connect: &connect,
        alloc_port: &alloc,
        cmdline_probe: &probe,
        ids_path: ids_path.clone(),
    };
    let revive_params = ResumeParams {
        name: "cdx-rk-live".to_string(),
        thread_id: thread_id.clone(),
        cwd: Some(cwd_str.clone()),
        current_pid: Some(create_pid), // dead now
        current_endpoint: Some(endpoint.clone()),
    };
    let revived = resume_codex(&revive_deps, &revive_params).expect("revive succeeds");
    let (new_pid, new_endpoint) = match revived {
        ResumeOutcome::Revived { pid, endpoint } => (pid, endpoint),
        other => panic!("expected Revived, got {other:?}"),
    };
    // Track the revived daemon for cleanup.
    *live_pid.lock().unwrap() = new_pid;

    assert!(new_pid > 0 && new_pid != create_pid, "a NEW daemon pid");
    assert!(
        new_endpoint.starts_with("ws://127.0.0.1:") && new_endpoint != endpoint,
        "a new endpoint: {new_endpoint}"
    );
    // The revived daemon is ALIVE + drivable.
    assert!(
        dispatch::effects::is_pid_alive(new_pid as i32),
        "revived daemon alive (pid {new_pid})"
    );
    // The row was UPDATED: keyed by the NEW pid, PRESERVED thread id (m2), new endpoint.
    let new_row =
        dispatch::registry::read_entry(&sessions_dir, new_pid).expect("revived row written");
    assert_eq!(new_row.pid, Some(new_pid));
    assert_eq!(
        new_row.session_id.as_deref(),
        Some(thread_id.as_str()),
        "m2: the thread id is PRESERVED across revive"
    );
    assert_eq!(new_row.endpoint.as_deref(), Some(new_endpoint.as_str()));
    assert_eq!(new_row.provider.as_deref(), Some("codex"));

    // P0 QA (spec-w4-qa A1, codex empirical pin): thread id preserved ⇒ SAME
    // qb id after the revive — mint_or_get keyed by the thread uuid rode
    // through; the revive minted NO second id.
    let ids_at_revive = dispatch::idstore::fold(&ids_path);
    assert_eq!(
        ids_at_revive.by_session.get(&thread_id),
        Some(&sbx_id_at_create),
        "codex resume → SAME qb id (the A1 matrix row, codex column)"
    );
    assert_eq!(
        ids_at_revive.by_id.len(),
        ids_at_create.by_id.len(),
        "the revive minted no extra ids"
    );

    // DRIVABLE proof: connect to the revived endpoint, initialize, and resume the
    // SAME thread again — a second thread/resume against the live daemon succeeds
    // (the thread is hydrated + drivable). NO turn (no extra API spend).
    {
        let rpc = WsAppServer::connect(&new_endpoint, std::time::Duration::from_secs(5))
            .expect("drivable: connect the revived daemon");
        let client = ClientInfo {
            name: "qd-manager".to_string(),
            title: None,
            version: "0".to_string(),
        };
        rpc.initialize(&client).expect("drivable: initialize");
        let _ = rpc.initialized();
        rpc.thread_resume(&thread_id)
            .expect("drivable: the revived thread re-resumes (hydrated history)");
        let _ = rpc.close();
    }

    // === Step 5: GROUP-kill cleanup of the REVIVED daemon + no-survivor belt ====
    reap(new_pid);
    *live_pid.lock().unwrap() = 0; // the reaper drop is now a no-op
    wait_dead(new_pid);
    assert!(
        !dispatch::effects::is_pid_alive(new_pid as i32),
        "revived daemon reaped (pid {new_pid})"
    );
    assert!(
        !jail_codex_daemon_alive(&codex_home),
        "no codex app-server for this jail's CODEX_HOME may survive"
    );

    // Sanity: the rollout retained its anchors (the revive did not corrupt history).
    let lines = read_lines(&rollout_after_turn);
    assert!(
        lines
            .iter()
            .any(|r| matches!(r.line, RolloutLine::TaskStarted { .. })),
        "rollout still has a task_started anchor"
    );
    assert!(
        lines
            .iter()
            .any(|r| matches!(r.line, RolloutLine::TaskComplete { .. })),
        "rollout still has a task_complete anchor"
    );

    let _ = std::fs::remove_dir_all(&jail);
}
