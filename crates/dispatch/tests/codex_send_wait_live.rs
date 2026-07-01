//! Codex P2 W6 — LIVE jailed end-to-end of the SEND + WAIT paths (codex-p2-spec
//! sections 7.5, 7.6, 10). Gated on `QD_CODEX_LIVE=1`; a no-op otherwise so the
//! default suite never spawns a real codex daemon or spends API budget.
//!
//! What it drives REAL (no fakes): spawn a jailed `codex app-server` daemon via
//! the production [`dispatch::create_daemon::run_new_daemon`] seams; SEND one tiny
//! one-line prompt through the production SEND ladder
//! ([`dispatch::provider::codex::CodexProvider::inject`] over a real `WsAppServer` with
//! the believed-state read from the rollout tail); WAIT for idle through the
//! production codex wait loop ([`dispatch::wait::run_codex_wait_loop`] +
//! [`dispatch::wait::RealCodexWaitDeps`], live notifications with a rollout-tail
//! fallback). The bin-layer verb glue (run_codex_send/run_codex_wait) is a thin
//! wrapper over exactly these lib primitives; this test exercises the lib core.
//!
//! TURN BUDGET (the brief's API-spend cap): exactly ONE live turn (a one-line
//! "Reply with exactly OK" prompt). The W4 lazy-materialization fact: the rollout
//! FILE appears on the FIRST turn — so AFTER the send+wait, an existence + tail
//! assert (task_started + task_complete) IS valid here (unlike W4's no-turn
//! create).
//!
//! JAIL (rule 9 + ADD-4/14): own HOME/CODEX_HOME/XDG_*/TMPDIR under
//! `CARGO_TARGET_TMPDIR` (inside the workspace tree — literal /tmp is radioactive);
//! ports via the real allocator (ephemeral, OUTSIDE 8900-9000); the OpenRouter key
//! is read from the RUNNER's `$HOME/.quorum/dispatch/config.toml` and exported into the jail env
//! ONLY (never written to a jail file, never on disk). GROUP-scoped
//! SIGTERM→grace→SIGKILL cleanup (instance-addressed by the recorded pgid, the W4
//! launcher-orphan finding) + a no-survivor belt.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dispatch::boot::RealSleeper;
use dispatch::create_daemon::{
    real_alloc_port, run_new_daemon, DaemonDeps, DaemonParams, RealDaemonSpawner,
};
use dispatch::effects::{Env, RealClock};
use dispatch::exec::RealExec;
use dispatch::provider::codex::{
    open_turn_id, read_lines, AppServerRpc, ClientInfo, CodexProvider, RolloutLine, RpcError,
    WsAppServer,
};
use dispatch::provider::{Provider, ProviderFx, SessionKey};
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
/// the real key lives — never an env var on disk, never a jail file). Returns the
/// raw key string. Panics if absent (the live lane needs a real turn).
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
            // `openrouter-key = "sk-or-..."`
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
        "w6-codex-sendwait-{}-{}",
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
    // The OpenRouter key — env-only, into the daemon's process env (the daemon's
    // launch_plan does not pass it; the spawner layers the daemon env from the
    // launch plan, but the daemon also inherits the spawning process env). We set
    // it on the JailEnv so the version sniff/launch see a consistent env; the real
    // spawner inherits the parent process env for unset vars, so we ALSO export it
    // into the test process below.
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

struct ReapOnDrop(Arc<i64>);
impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        reap(*self.0);
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

#[test]
fn codex_send_wait_live_jailed_e2e() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping the live codex send/wait test");
        return;
    }

    let or_key = openrouter_key();
    // The real spawner inherits the parent process env for vars not in the launch
    // plan; the daemon reads OPENROUTER_API_KEY from its env. Export it on the test
    // process so the inherited env carries it (the jail config's env_key names it).
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

    let deps = DaemonDeps {
        provider: &CodexProvider,
        env: &env,
        exec: &exec,
        clock: &clock,
        sessions_dir: sessions_dir.clone(),
        claims_dir: claims_dir.clone(),
        log_dir,
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
        name: "cdx-sw-live".to_string(),
        cwd: cwd.clone(),
        agent: None,
        passthrough: vec![],
        // NO prompt at create — the SEND path drives the one live turn below.
        prompt: None,
    };

    let out = run_new_daemon(&deps, &params).expect("live daemon create succeeds");
    let pid = out.pid;
    let reaper = ReapOnDrop(Arc::new(pid));
    let thread_id = out.thread_id.clone();
    let endpoint = out.endpoint.clone();
    let cwd_str = cwd.to_string_lossy().into_owned();

    // === SEND: drive the production inject ladder over a real ws client ========
    // believed state from the rollout tail (no file yet at create → believed IDLE
    // → turn/start). One tiny prompt = one model call (the budget).
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
        // Raise the request deadline so a real turn-start response is not starved.
        rpc.set_request_timeout(std::time::Duration::from_secs(60));

        // believed state: read the rollout tail (none yet → None → believed IDLE).
        let placeholder = dispatch::paths::QdPaths::from_home(Path::new(""));
        let root = codex_root(&env, &placeholder);
        let key = SessionKey {
            id: &thread_id,
            name: Some("cdx-sw-live"),
            cwd: Some(cwd_str.as_str()),
            pid: Some(pid),
        };
        let rollout = CodexProvider.transcript_path(&root, &key);
        let expected = rollout
            .as_ref()
            .map(|p| open_turn_id(&read_lines(p)))
            .unwrap_or(None);

        let rpc_ref: &dyn AppServerRpc = &rpc;
        let fx = ProviderFx {
            env: &env,
            paths: &dispatch::paths::QdPaths::from_home(&jail.join("home")),
            socket_dir: sessions_dir.clone(),
            mux: None,
            clock: None,
            sleeper: None,
            relay: None,
            relay_port: None,
            app_server: Some(rpc_ref),
            codex_expected_turn_id: expected.as_deref(),
            acp_client: None,
        pi_rpc: None,        };
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

    // === WAIT: block until idle via the production codex wait loop =============
    {
        // Resolve the rollout path for the wait FALLBACK channel. W4/W6 FINDING:
        // at wait-ENTRY the rollout file may not be materialized yet (the daemon
        // writes it lazily as the first turn runs), so transcript_path can return
        // None here — and that is FINE: the codex wait's primary channel is the
        // LIVE thread/status/changed notification; the rollout tail is only the
        // daemon-unreachable fallback. The file IS present by the time the turn
        // completes (re-resolved below for the post-completion assertions).
        let placeholder = dispatch::paths::QdPaths::from_home(Path::new(""));
        let root = codex_root(&env, &placeholder);
        let key = SessionKey {
            id: &thread_id,
            name: Some("cdx-sw-live"),
            cwd: Some(cwd_str.as_str()),
            pid: Some(pid),
        };
        let rollout_path = CodexProvider.transcript_path(&root, &key);

        // Connect the live channel; on failure the deps fall back to the rollout
        // tail (the daemon-unreachable path). A fresh ws client for wait.
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
            rollout_path.clone(),
            std::time::Duration::from_millis(500),
            &real_clock,
            &sleeper,
        );
        // A generous timeout for one real turn (the budget belt keeps it small).
        let outcome = run_codex_wait_loop(&wdeps, 90_000, 500);
        if let Some(c) = connected {
            let _ = c.close();
        }
        assert_eq!(
            outcome,
            WaitStatusOutcome::Done,
            "WAIT resolves to Done (the turn completed → idle)"
        );

        // === The rollout now has task_started + task_complete. RE-RESOLVE the path
        // AFTER completion (W4 lazy-materialize fact: the file appears as the FIRST
        // turn runs, so a wait-entry resolution can be None — but it is present now).
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
        // The balanced tail derives IDLE (the connectionless channel agrees).
        assert_eq!(open_turn_id(&lines), None, "no open turn after completion");
    }

    // === GROUP-kill cleanup + no-survivor belt =================================
    drop(reaper);
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
    assert!(
        !jail_codex_daemon_alive(&codex_home),
        "no codex app-server for this jail's CODEX_HOME may survive"
    );

    let _ = std::fs::remove_dir_all(&jail);
}

/// Resolve the codex transcript_root off the jail env (codex's transcript_root
/// reads `fx.env` $CODEX_HOME/$HOME — never paths; the placeholder QdPaths is just
/// to satisfy the borrow). Owned `paths` is passed in so the borrow outlives the fx.
fn codex_root(env: &JailEnv, paths: &dispatch::paths::QdPaths) -> PathBuf {
    let fx = ProviderFx {
        env,
        paths,
        socket_dir: PathBuf::new(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    CodexProvider.transcript_root(&fx)
}
