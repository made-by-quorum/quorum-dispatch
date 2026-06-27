//! Codex P2 W8 — the AGENT-LIFECYCLE CAPSTONE + the supervised-dogfood REHEARSAL.
//!
//! ONE jailed end-to-end test that COMPOSES the whole codex agent lifecycle in a
//! single jail, proving the per-verb live lanes (W4 create / W5 ls-info / W6
//! send-wait / W7 resume-kill) COMPOSE into the chain a Pete-visible
//! `qd new --provider codex` toy task will exercise. The per-verb lanes prove each
//! verb works in isolation; THIS lane proves they work together against one
//! daemon + one rollout + one registry row, in order, with the row carried
//! correctly across every transition.
//!
//! ZERO ATTACH (ADD-26: codex human-attach is SEVERED to the declarative-UX wave;
//! agents have NO TTY — the agent lifecycle never touches a TUI). resume here is
//! the AGENT-facing thread/resume revive-to-DRIVABLE, never an interactive attach.
//!
//! The 8-step chain (codex-p2-spec sections 7.2, 7.4, 7.5, 7.6, 10, 12):
//!
//! 1. CREATE (W4) — run_new_daemon → assert the registry row (provider codex, real
//!    thread uuid sessionId, ws endpoint, pid alive).
//! 2. LS/INFO (W5) — gather→join→render against the LIVE jail: the codex row
//!    surfaces, provider codex, status Idle (fresh = no rollout yet, connectionless
//!    — NO socket opened), and the endpoint does NOT appear on the --json surface
//!    (the section 9.4 contract).
//! 3. SEND (W6) — the production inject ladder over a real WsAppServer → a real turn
//!    id (the ONE live model turn — the budget belt).
//! 4. WAIT (W6) — run_codex_wait_loop → Done; the rollout FILE now exists (W4
//!    lazy-materialization: it lands on the first turn) with task_started +
//!    task_complete anchors.
//! 5. LS again (W5) — status derived from the now-existing rollout tail = Idle after
//!    completion (balanced tail).
//! 6. KILL (W7) — kill_codex (the GROUP-pgid ladder) → daemon dead + tombstoned
//!    (provider + endpoint carried) + NO survivor.
//! 7. RESUME (W7) — resume_codex on the killed session → respawn + thread/resume →
//!    row updated (new pid/endpoint, PRESERVED thread id = m2) + drivable. NO turn
//!    (thread/resume hydrates from disk).
//! 8. FINAL KILL + the no-survivor belt (group-addressed by the jail CODEX_HOME).
//!
//! TURN BUDGET (the brief's API-spend cap): exactly ONE live model turn (step 3 —
//! a one-line "Reply with exactly OK" prompt). The resume (step 7) sends no turn.
//!
//! JAIL (rule 9 + ADD-4/14): own HOME/CODEX_HOME/XDG_*/TMPDIR under
//! `CARGO_TARGET_TMPDIR` (workspace tree, never /tmp); ports via the real allocator
//! (ephemeral, OUTSIDE 8900-9000); the OpenRouter key read from the RUNNER's
//! `$HOME/.quorum/dispatch/config.toml`, exported into the jail env ONLY (never written to a jail
//! file, never on disk). GROUP-scoped SIGTERM→grace→SIGKILL cleanup (instance-
//! addressed by the recorded pgid — the W4 launcher-orphan finding) + a no-survivor
//! belt after both the kill and the final cleanup.
//!
//! Gated on `QD_CODEX_LIVE=1`; a no-op (compiled + ignored) otherwise so the default
//! suite never spawns a real codex daemon or spends API budget.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dispatch::boot::RealSleeper;
use dispatch::create_daemon::{
    real_alloc_port, real_cmdline_probe, run_new_daemon, DaemonDeps, DaemonParams,
    RealDaemonSpawner,
};
use dispatch::effects::{
    Env, FixedClock, FixtureProcessTable, FixtureRelayProbe, MapEnv, RealClock,
};
use dispatch::exec::RealExec;
use dispatch::join::{self, JoinOpts};
use dispatch::model::SessionStatus;
use dispatch::provider::codex::{
    open_turn_id, read_lines, AppServerRpc, ClientInfo, CodexProvider, RolloutLine, RpcError,
    WsAppServer,
};
use dispatch::provider::{Provider, ProviderFx, SessionKey};
use dispatch::registry::RegistryEntry;
use dispatch::render;
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
        "w8-codex-e2e-{}-{}",
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
        "zmx-dir",
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

/// A reaper that always kills the CURRENT live daemon pid (updated across the
/// create→kill→revive transitions) so a panic never leaks a daemon.
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

/// Drive the REAL W5 ls/info pipeline against the LIVE jail (the SAME `join::gather`
/// → `join_with_strays` → `assign_codes` → `render` path ls/info use), and return
/// the live codex row plus the rendered `ls --json` text.
///
/// CONNECTIONLESS: gather opens NO socket — the codex row's status derives from the
/// rollout tail (or Idle when no rollout exists yet). A hermetic gather env: only
/// CODEX_HOME (→ the codex root) + an EMPTY ZMX_DIR (so no real-host zmx dir is
/// scanned); the legacy zmx scan root is pinned INSIDE the jail. The daemon pid is
/// marked alive in the fixture process table (the join never gates row presence on
/// pid-aliveness, but a faithful live snapshot reports the daemon alive). NOW_MS is
/// fixed so the render is deterministic.
fn ls_snapshot(
    jail: &Path,
    codex_home: &Path,
    daemon_pid: i64,
) -> (Option<dispatch::model::Session>, String) {
    // A gather-only env: CODEX_HOME + an empty ZMX_DIR (hermetic — no real host).
    let genv = MapEnv {
        vars: [
            (
                "CODEX_HOME".to_string(),
                codex_home.to_string_lossy().into_owned(),
            ),
            (
                "ZMX_DIR".to_string(),
                jail.join("zmx-dir").to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        // SAFETY: getuid is always safe.
        uid: unsafe { libc::getuid() },
    };
    let mux = dispatch::mux::FixtureMux::new();
    let pt = FixtureProcessTable {
        ppids: [(daemon_pid as i32, 1)].into_iter().collect(),
        alive: [daemon_pid as i32].into_iter().collect(),
        claude: Vec::new(),
    };
    let probe = FixtureRelayProbe(Vec::new());
    // A fixed clock keeps the render deterministic; the value is irrelevant to the
    // status assertions (status derives from the rollout tail, not the clock).
    let clock = FixedClock(1_717_500_300_000);
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: false,
        include_preview: true,
        limit: None,
    };
    let paths = dispatch::paths::SbPaths::from_home(&jail.join("home"));
    let inputs = join::gather(
        &paths, &mux, &genv, &pt, &probe, &clock,
        jail, // tmp_root pinned INSIDE the jail (legacy zmx scan stays hermetic)
        None, // no machine-global XDG-family scan
        opts,
    );
    let (mut sessions, strays) = join::join_with_strays(&inputs, opts);
    join::assign_codes(&mut sessions);
    let ls_json = render::to_pretty(&render::ls_json(&sessions, &strays));
    // The codex row = the one whose provider is "codex".
    let codex = sessions.iter().find(|s| s.provider == "codex").cloned();
    (codex, ls_json)
}

#[test]
fn codex_full_lifecycle_live_jailed() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping the codex full-lifecycle capstone");
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

    // === Step 1: CREATE the codex session (W4) =================================
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
        ids_path: jail
            .join("home")
            .join(".quorum")
            .join("dispatch")
            .join("state")
            .join("ids.jsonl"),
    };
    let create_params = DaemonParams {
        name: "cdx-e2e".to_string(),
        cwd: cwd.clone(),
        agent: None,
        passthrough: vec![],
        prompt: None, // the SEND step (3) drives the one live turn.
    };
    let out = run_new_daemon(&create_deps, &create_params).expect("live daemon create succeeds");
    let thread_id = out.thread_id.clone();
    let endpoint = out.endpoint.clone();
    let create_pid = out.pid;

    let live_pid = Arc::new(std::sync::Mutex::new(create_pid));
    let _reaper = ReapOnDrop(live_pid.clone());

    // The written registry row: every codex field on the m2 thread id.
    assert!(create_pid > 0, "real daemon pid");
    assert!(
        endpoint.starts_with("ws://127.0.0.1:"),
        "endpoint: {endpoint}"
    );
    let port: u16 = endpoint
        .rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .expect("endpoint carries a port");
    assert!(
        !(8900..=9000).contains(&port),
        "endpoint port {port} OUTSIDE the relay range"
    );
    assert!(
        thread_id.contains('-') && thread_id.len() >= 32,
        "thread id looks like a real uuid: {thread_id}"
    );
    let row =
        dispatch::registry::read_entry(&sessions_dir, create_pid).expect("row written in the jail");
    assert_eq!(row.pid, Some(create_pid));
    assert_eq!(row.session_id.as_deref(), Some(thread_id.as_str()));
    assert_eq!(row.provider.as_deref(), Some("codex"));
    assert_eq!(row.endpoint.as_deref(), Some(endpoint.as_str()));
    assert_eq!(row.status.as_deref(), Some("idle"));
    assert!(
        dispatch::effects::is_pid_alive(create_pid as i32),
        "daemon alive post-create"
    );

    // === Step 2: LS/INFO on the FRESH session (W5) — connectionless, no socket ==
    {
        let (codex, ls_json) = ls_snapshot(&jail, &codex_home, create_pid);
        let codex = codex.expect("the live codex row surfaces in ls");
        assert_eq!(codex.provider, "codex");
        assert_eq!(codex.session_id, thread_id, "m2: sessionId = thread uuid");
        // Fresh session: no rollout file yet (lazy materialization) → absent from
        // codex_status_for → Idle. The status read opened NO socket.
        assert_eq!(
            codex.status,
            SessionStatus::Idle,
            "a fresh codex session is Idle (connectionless, no rollout yet)"
        );
        // The endpoint NEVER appears on the --json surface (section 9.4 contract).
        assert!(
            !ls_json.contains("endpoint")
                && !ls_json.contains("ws://")
                && !ls_json.contains(&port.to_string()),
            "ls --json must NOT carry endpoint / port / ws scheme: {ls_json}"
        );
        // The human info surface ALSO hides the endpoint and DOES render the provider.
        let info = render::info_text(&codex, 1_717_500_300_000);
        assert!(
            !info.contains("endpoint") && !info.contains("ws://"),
            "human info must NOT carry endpoint / ws scheme: {info}"
        );
        assert!(
            info.contains("Provider:    codex\n"),
            "human info renders Provider: codex: {info}"
        );
    }

    // === Step 3: SEND one tiny prompt (W6 inject ladder) — the ONE live turn ====
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

        // believed state: read the rollout tail (none yet → None → believed IDLE).
        let placeholder = dispatch::paths::SbPaths::from_home(Path::new(""));
        let root = codex_root(&env, &placeholder);
        let key = SessionKey {
            id: &thread_id,
            name: Some("cdx-e2e"),
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

    // === Step 4: WAIT for idle (W6) — the turn completes, rollout materializes ==
    {
        let placeholder = dispatch::paths::SbPaths::from_home(Path::new(""));
        let root = codex_root(&env, &placeholder);
        let key = SessionKey {
            id: &thread_id,
            name: Some("cdx-e2e"),
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
            "WAIT resolves to Done (the turn completed → idle)"
        );

        // The rollout FILE appears on the FIRST turn (W4 lazy-materialization) —
        // RE-RESOLVE the path AFTER completion (a wait-entry resolution can be None).
        let rollout_path = CodexProvider
            .transcript_path(&root, &key)
            .expect("rollout file materialized once the first turn completed");
        assert!(
            rollout_path.exists() && rollout_path.starts_with(&codex_home),
            "rollout under the jail CODEX_HOME: {rollout_path:?}"
        );
        let lines = read_lines(&rollout_path);
        assert!(
            lines
                .iter()
                .any(|r| matches!(r.line, RolloutLine::TaskStarted { .. })),
            "rollout has a task_started anchor"
        );
        assert!(
            lines
                .iter()
                .any(|r| matches!(r.line, RolloutLine::TaskComplete { .. })),
            "rollout has a task_complete anchor (the turn finished)"
        );
        assert_eq!(open_turn_id(&lines), None, "no open turn after completion");
    }

    // === Step 5: LS again (W5) — status now derives from the rollout tail = Idle =
    {
        let (codex, _ls_json) = ls_snapshot(&jail, &codex_home, create_pid);
        let codex = codex.expect("the live codex row still surfaces post-turn");
        assert_eq!(codex.provider, "codex");
        // Balanced rollout tail (task_started + task_complete) → derive_status = Idle.
        assert_eq!(
            codex.status,
            SessionStatus::Idle,
            "after completion the rollout-tail status is Idle (balanced tail)"
        );
        // jsonlPath now resolves to the materialized rollout (under the codex root).
        assert!(
            codex
                .jsonl_path
                .as_deref()
                .is_some_and(|p| p.contains("/sessions/") && p.contains("/rollout-")),
            "jsonlPath = the rollout path: {:?}",
            codex.jsonl_path
        );
    }

    // === Step 6: KILL (W7 group ladder) + tombstone + no survivor ===============
    let captured: Option<RegistryEntry> = dispatch::registry::read_entry(&sessions_dir, create_pid);
    assert!(captured.is_some(), "the create row exists pre-kill");
    // W9 FIX M-1: the production cmdline-identity probe gates the group signal on
    // the live daemon being OUR codex daemon (codex app-server + --listen endpoint).
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
    assert!(
        !jail_codex_daemon_alive(&codex_home),
        "no codex app-server survives the kill"
    );

    // === Step 7: RESUME-revive (W7) on the killed session =======================
    // thread/resume hydrates from the on-disk rollout with NO model call.
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
        ids_path: jail
            .join("home")
            .join(".quorum")
            .join("dispatch")
            .join("state")
            .join("ids.jsonl"),
    };
    let revive_params = ResumeParams {
        name: "cdx-e2e".to_string(),
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
    *live_pid.lock().unwrap() = new_pid;

    assert!(new_pid > 0 && new_pid != create_pid, "a NEW daemon pid");
    assert!(
        new_endpoint.starts_with("ws://127.0.0.1:") && new_endpoint != endpoint,
        "a new endpoint: {new_endpoint}"
    );
    assert!(
        dispatch::effects::is_pid_alive(new_pid as i32),
        "revived daemon alive (pid {new_pid})"
    );
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

    // DRIVABLE proof: connect to the revived endpoint, initialize, re-resume the
    // SAME thread (the hydrated history is drivable). NO turn (no extra API spend).
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

    // === Step 8: FINAL KILL of the revived daemon + the no-survivor belt ========
    reap(new_pid);
    *live_pid.lock().unwrap() = 0; // the reaper drop is now a no-op
    wait_dead(new_pid);
    assert!(
        !dispatch::effects::is_pid_alive(new_pid as i32),
        "revived daemon reaped (pid {new_pid})"
    );
    // The no-survivor belt: NO codex app-server for this jail's CODEX_HOME survives.
    assert!(
        !jail_codex_daemon_alive(&codex_home),
        "no codex app-server for this jail's CODEX_HOME may survive"
    );

    // Best-effort jail teardown (we passed; the chain composed end to end).
    let _ = std::fs::remove_dir_all(&jail);
}
