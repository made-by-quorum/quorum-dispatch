//! C-LIVE — tier-(b) LIVE-TURN validation of the codex adapter (frame
//! 01KWB75). Gated on `QD_CODEX_LIVE=1`; a no-op otherwise so the default suite
//! never spawns a real codex daemon or spends API budget. The PRODUCTION
//! ChatGPT-OAuth turn carries a SECOND gate `QD_CODEX_PROD_TURN=1` so the bulk
//! OpenRouter rounds never spend Pete's limited subscription.
//!
//! WHY THIS EXISTS — the tier-a SEND path (`CodexProvider::inject` + the believed-
//! state ladder) is DONE/ACCEPTED and validated by `codex_send_wait_live.rs`. This
//! harness does NOT re-validate that ladder. It drives the RAW [`AppServerRpc`]
//! primitives (`turn_start`/`turn_steer`/`turn_interrupt`/`next_notification`) to
//! OBSERVE the adapter WIRING the inject ladder does not touch — the live content
//! stream, a LANDED steer, a live interrupt (the only exerciser of
//! `turn_interrupt`, which is present-but-unused in production), the status
//! active↔idle PUSH, and the rollout-tail Busy reading DURING an in-flight turn.
//! PRODUCTION FOOTPRINT IS FROZEN: this is test-only; a real adapter defect a live
//! turn surfaces is ESCALATED, never fixed here.
//!
//! AT-SOURCE GROUNDING (mined from the spike evidence
//! `exec/codex-spike-evidence/jail/q1c-clientA-events.jsonl`, a real two-turn
//! session): the live turn lifecycle notifications, in order, are
//! `thread/status/changed`(status.type→"active") → `turn/started` →
//! `item/started` / `item/agentMessage/delta` (THE CONTENT STREAM) / `item/completed`
//! → `thread/tokenUsage/updated` (THE USAGE FRAME — usage rides HERE, NOT inside
//! `turn/completed`; the brief's "turn/completed{usage}" is shorthand) →
//! `thread/status/changed`(status.type→"idle") → `turn/completed`
//! (`params.turn.status` == "completed"). We assert on these REAL method strings.
//!
//! NO-SKIP-MASQUERADE: every evidence artifact is emitted ONLY after the gate
//! (`if !live() { return }`). Gate off → no artifact → a skip is provably
//! distinguishable from a run. The artifact IS the captured real frames +
//! `turn/completed` + the durable rollout tail at
//! `target/cred-evidence/clive/<lane>/turn-completed.txt`. The PRODUCTION artifact
//! is DISTINCT and carries chatgpt-path provenance.
//!
//! JAIL: own HOME/CODEX_HOME/XDG_*/TMPDIR under `CARGO_TARGET_TMPDIR` (workspace
//! tree, never literal /tmp); the OpenRouter key is read from the runner's
//! `$HOME/.quorum/dispatch/config.toml` and exported into the test PROCESS env so
//! the spawned daemon inherits it (the spawner does NOT `env_clear`; it overlays
//! only the launch_plan's `CODEX_HOME` onto the inherited process env —
//! create_daemon.rs:855). GROUP-scoped (`-pgid`) SIGTERM→SIGKILL teardown + a
//! no-survivor belt.
//!
//! D2 cross-thread content-isolation: MOOT under one-daemon-per-session (each
//! session is its own daemon with its own thread + CODEX_HOME root, so there is no
//! shared app-server to cross-contaminate). SKIPPED deliberately — no lane, noted
//! here per the §5.2 work-set.

#[path = "common/live_gate.rs"]
mod live_gate;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use dispatch::create_daemon::{
    real_alloc_port, run_new_daemon, DaemonDeps, DaemonParams, RealDaemonSpawner,
};
use dispatch::effects::{Env, RealClock};
use dispatch::exec::RealExec;
use dispatch::model::SessionStatus;
use dispatch::provider::codex::{
    derive_status, open_turn_id, read_lines, AppServerRpc, ClientInfo, CodexProvider, Notification,
    RolloutLine, RpcError, SteerOutcome, WsAppServer,
};
use dispatch::provider::{Provider, ProviderFx, SessionKey};

// ===========================================================================
// Gates (no-skip-masquerade): artifacts are emitted ONLY past these.
// ===========================================================================

/// The live gate. Skip unless `QD_CODEX_LIVE=1` (NOT the stale `SB_CODEX_LIVE`).
fn live() -> bool {
    live_gate::conformance_gate("QD_CODEX_LIVE", "codex_live_turn_live")
}

/// The SECOND gate for the load-bearing production OAuth turn. Skip unless
/// `QD_CODEX_PROD_TURN=1` so the bulk OpenRouter rounds never spend Pete's
/// limited ChatGPT subscription.
fn prod_turn() -> bool {
    std::env::var("QD_CODEX_PROD_TURN").as_deref() == Ok("1")
}

// ===========================================================================
// Evidence emission — POST-GATE ONLY (no-skip-masquerade).
// ===========================================================================

/// The canonical evidence bundle dir for a lane:
/// `target/cred-evidence/clive/<lane>/`. `CARGO_TARGET_TMPDIR` is `<target>/tmp`
/// for an integration test, so its parent is the crate `target` dir.
fn evidence_dir(lane: &str) -> PathBuf {
    let target = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .expect("CARGO_TARGET_TMPDIR has a parent (the crate target dir)")
        .to_path_buf();
    let dir = target.join("cred-evidence").join("clive").join(lane);
    std::fs::create_dir_all(&dir).expect("create the clive evidence dir");
    eprintln!("CLIVE EVIDENCE [{lane}]: {}", dir.display());
    dir
}

/// Write a text artifact into the bundle.
fn ev_text(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write clive evidence artifact");
}

// ===========================================================================
// Jail + env (OpenRouter lane) — reused from the codex_send_wait_live pattern.
// ===========================================================================

/// A jail Env: only the codex-relevant vars resolve. The OpenRouter key rides
/// here AND is exported into the test process (the daemon inherits it).
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

/// Read `openrouter-key` from the runner's `$HOME/.quorum/dispatch/config.toml`
/// (the only place the real key lives — never a jail file, never on disk).
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

/// Build a fresh jail tree under CARGO_TARGET_TMPDIR (workspace tree, never /tmp).
/// `with_codex_config` writes the OpenRouter `config.toml` into `codex-home` (the
/// OpenRouter lane); the production lane passes `false` (real `~/.codex` resolves).
fn make_jail(tag: &str, with_codex_config: bool) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "clive-{tag}-{}-{}",
        std::process::id(),
        clock_nanos()
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
    if with_codex_config {
        // The model_providers stanza (spike-proven, wire_api "responses"); the KEY
        // is NOT written here — env_key names the env var the daemon reads it from.
        std::fs::write(
            root.join("codex-home").join("config.toml"),
            "model = \"openai/gpt-4o-mini\"\nmodel_provider = \"openrouter\"\n\
             [model_providers.openrouter]\nname = \"OpenRouter\"\n\
             base_url = \"https://openrouter.ai/api/v1\"\nenv_key = \"OPENROUTER_API_KEY\"\n\
             wire_api = \"responses\"\n",
        )
        .unwrap();
    }
    root
}

/// A monotonic-ish nonce (process clock) — `SystemTime::now` is the only clock a
/// test binary has; used solely to make jail dirs unique.
fn clock_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// The OpenRouter jail env: HOME/CODEX_HOME/XDG_*/TMPDIR jailed + the OpenRouter
/// key + PATH.
fn or_jail_env(jail: &Path, or_key: &str) -> JailEnv {
    let mut vars = std::collections::HashMap::new();
    let mut put = |k: &str, v: String| {
        vars.insert(k.to_string(), v);
    };
    put("HOME", jail.join("home").to_string_lossy().into());
    put("CODEX_HOME", jail.join("codex-home").to_string_lossy().into());
    put(
        "XDG_CONFIG_HOME",
        jail.join("xdg-config").to_string_lossy().into(),
    );
    put("XDG_DATA_HOME", jail.join("xdg-data").to_string_lossy().into());
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

/// The PRODUCTION jail env: HOME/XDG/TMPDIR jailed, but CODEX_HOME points at the
/// REAL `~/.codex` so the ChatGPT-OAuth `auth.json` resolves (the single override
/// the jail must NOT mask). NO OpenRouter config, NO `OPENROUTER_API_KEY`, NO
/// `OPENAI_API_KEY`. The rollout this turn writes lands under the real
/// `~/.codex/sessions/` — that is EXPECTED at-source evidence (the side-effect the
/// goal flags; do NOT scrub it).
fn prod_jail_env(jail: &Path, real_codex_home: &Path) -> JailEnv {
    let mut vars = std::collections::HashMap::new();
    let mut put = |k: &str, v: String| {
        vars.insert(k.to_string(), v);
    };
    put("HOME", jail.join("home").to_string_lossy().into());
    // CODEX_HOME = the REAL ~/.codex (un-jailed) — the OAuth store resolves here.
    put("CODEX_HOME", real_codex_home.to_string_lossy().into());
    put(
        "XDG_CONFIG_HOME",
        jail.join("xdg-config").to_string_lossy().into(),
    );
    put("XDG_DATA_HOME", jail.join("xdg-data").to_string_lossy().into());
    put(
        "XDG_CACHE_HOME",
        jail.join("xdg-cache").to_string_lossy().into(),
    );
    put("TMPDIR", jail.join("tmp").to_string_lossy().into());
    put(
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| "/opt/homebrew/bin:/usr/bin:/bin".into()),
    );
    // Deliberately NO OPENROUTER_API_KEY / OPENAI_API_KEY here.
    JailEnv { vars }
}

// ===========================================================================
// Spawn + teardown (production seams; group kill + no-survivor belt).
// ===========================================================================

/// GROUP-scoped cleanup via the production `RealDaemonSpawner::kill` (the recorded
/// pgid ladder — SIGTERM → grace → SIGKILL; the W4 launcher-orphan finding makes
/// the group kill load-bearing).
fn reap(pid: i64) {
    use dispatch::create_daemon::DaemonSpawner;
    RealDaemonSpawner.kill(pid);
}

struct ReapOnDrop(i64);
impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        reap(self.0);
    }
}

fn wait_dead(pid: i64) {
    for _ in 0..40 {
        if !dispatch::effects::is_pid_alive(pid as i32) {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Is any `codex app-server` for THIS jail's CODEX_HOME alive? Instance-addressed
/// via `ps eww` env match (never a name-addressed pkill). ONLY safe when CODEX_HOME
/// is a UNIQUE jail dir (the OpenRouter lane) — NOT for the production lane whose
/// CODEX_HOME is the real `~/.codex` (would match Pete's unrelated codex procs).
fn jail_codex_daemon_alive(codex_home: &Path) -> bool {
    let needle = format!("CODEX_HOME={}", codex_home.display());
    let Ok(out) = std::process::Command::new("pgrep")
        .args(["-f", "codex app-server"])
        .output()
    else {
        return false;
    };
    for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if let Ok(ps) = std::process::Command::new("ps")
            .args(["eww", "-p", pid])
            .output()
        {
            if String::from_utf8_lossy(&ps.stdout).contains(&needle) {
                return true;
            }
        }
    }
    false
}

/// A spawned jailed daemon + the bits the lanes need to drive + reap it. (The
/// daemon's own boot thread id is not stored — the stream-capturing lanes drive a
/// thread they OWN on the same app-server; see `own_thread`.)
struct Spawned {
    pid: i64,
    endpoint: String,
    codex_home: PathBuf,
    cwd: PathBuf,
}

/// Spawn a jailed `codex app-server` via the production create seams. The daemon
/// env = the test PROCESS env ⊕ the launch_plan's `CODEX_HOME` (the spawner does
/// not `env_clear`), so callers MUST have arranged the process env (OpenRouter key
/// present for the OR lane; both provider keys ABSENT for the prod lane).
fn spawn_jailed(jail: &Path, env: &JailEnv, name: &str) -> Spawned {
    let sessions_dir = jail.join("home").join(".claude").join("sessions");
    let claims_dir = jail.join("home").join(".claude").join("claims");
    let log_dir = jail.join("home").join(".quorum").join("dispatch").join("log");
    let cwd = jail.join("work");

    let exec = RealExec;
    let clock = RealClock;
    let spawner = RealDaemonSpawner;
    let connect = |url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
        WsAppServer::connect(url, Duration::from_secs(5)).map(|c| {
            let b: Box<dyn AppServerRpc> = Box::new(c);
            b
        })
    };
    let alloc = real_alloc_port;

    let deps = DaemonDeps {
        provider: &CodexProvider,
        env,
        exec: &exec,
        clock: &clock,
        sessions_dir: sessions_dir.clone(),
        claims_dir,
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
        name: name.to_string(),
        cwd: cwd.clone(),
        agent: None,
        passthrough: vec![],
        prompt: None,
            hosting: None,
    };
    let out = run_new_daemon(&deps, &params).expect("live daemon create succeeds");
    let codex_home = PathBuf::from(
        env.var("CODEX_HOME")
            .expect("CODEX_HOME resolves in the jail env"),
    );
    let _ = out.thread_id; // daemon boot thread; the lanes drive their own (own_thread).
    Spawned {
        pid: out.pid,
        endpoint: out.endpoint,
        codex_home,
        cwd,
    }
}

/// A fresh ws client, handshaken, with a generous request deadline for a real turn.
fn connect_initialized(endpoint: &str) -> WsAppServer {
    let rpc = WsAppServer::connect(endpoint, Duration::from_secs(5)).expect("connect app-server");
    let client = ClientInfo {
        name: "qd-manager".to_string(),
        title: None,
        version: "0".to_string(),
    };
    rpc.initialize(&client).expect("initialize handshake");
    let _ = rpc.initialized();
    rpc.set_request_timeout(Duration::from_secs(90));
    rpc
}

/// Create + OWN a thread on this connection (RUN-1 FINDING, at source): the codex
/// app-server delivers the per-thread `item/*` + `turn/completed` + tokenUsage
/// stream ONLY to a connection that is subscribed to the thread — and a connection
/// that CREATED the thread is subscribed by construction (proven by the spike's
/// single-connection appserver-probe.py → q1c-clientA full stream). Driving the
/// daemon-boot thread from a stranger connection gets only the broadcast
/// `thread/status/changed`, NOT the content stream. So the stream-capturing lanes
/// own their own thread on the SAME real codex app-server. (The production SEND
/// path needs only status/changed, which is why it never subscribes — RUN-1
/// showed status flowing fine while item/* did not.)
fn own_thread(rpc: &WsAppServer, cwd_str: &str) -> String {
    rpc.thread_start(cwd_str, "never", "read-only")
        .expect("thread/start — own a subscribed thread for the live content stream")
}

/// Resolve the live rollout path off the jail env for `thread_id` (re-resolve each
/// call — the W4 lazy-materialize fact: the file appears as the FIRST turn runs, so
/// an early resolution can legitimately return None). codex's `transcript_path`
/// keys on the uuid (date-walk), so `thread_id` is the load-bearing field.
fn resolve_rollout(env: &JailEnv, sp: &Spawned, thread_id: &str) -> Option<PathBuf> {
    let placeholder = dispatch::paths::QdPaths::from_home(Path::new(""));
    let root = codex_root(env, &placeholder);
    let cwd_str = sp.cwd.to_string_lossy().into_owned();
    let key = SessionKey {
        id: thread_id,
        name: Some("cdx-live"),
        cwd: Some(cwd_str.as_str()),
        pid: Some(sp.pid),
    };
    CodexProvider.transcript_path(&root, &key)
}

/// Resolve codex's transcript_root off the jail env (reads `fx.env`
/// $CODEX_HOME/$HOME — never paths; the placeholder QdPaths satisfies the borrow).
fn codex_root(env: &JailEnv, paths: &dispatch::paths::QdPaths) -> PathBuf {
    let fx = ProviderFx {
        await_relay: None,
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
        pi_rpc: None,
            acp_pre_dispatch: None,
    };
    CodexProvider.transcript_root(&fx)
}

// ===========================================================================
// Notification capture (the live evidence).
// ===========================================================================

#[derive(Default)]
struct Captured {
    frames: Vec<String>,
    /// `item/*` notifications — the content stream (`item/agentMessage/delta`).
    item_deltas: usize,
    saw_status_active: bool,
    saw_status_idle: bool,
    /// `thread/tokenUsage/updated` — the USAGE frame (usage rides here, not in
    /// turn/completed; mined at source from the spike evidence).
    saw_token_usage: bool,
    /// `params` of the `turn/completed` notification (the terminal frame).
    turn_completed: Option<Value>,
    /// `params.turn.status` of `turn/completed` ("completed" on a clean turn;
    /// something else on an interrupted one — captured for the oracle).
    completed_status: Option<String>,
    /// `params.thread.modelProvider` off the `thread/started` frame — AFFIRMATIVE
    /// provenance ("openrouter" on the jailed lane; the chatgpt provider on the
    /// production OAuth turn).
    model_provider: Option<String>,
}

fn record(cap: &mut Captured, n: &Notification, elapsed_ms: u128) {
    cap.frames
        .push(format!("[{elapsed_ms:>7}ms] {}  {}", n.method, n.params));
    if n.method.starts_with("item/") {
        cap.item_deltas += 1;
    }
    if n.method == "thread/status/changed" {
        if let Some(t) = n
            .params
            .get("status")
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
        {
            if t == "active" {
                cap.saw_status_active = true;
            }
            if t == "idle" {
                cap.saw_status_idle = true;
            }
        }
    }
    if n.method == "thread/tokenUsage/updated" {
        cap.saw_token_usage = true;
    }
    if n.method == "thread/started" {
        if let Some(mp) = n
            .params
            .get("thread")
            .and_then(|t| t.get("modelProvider"))
            .and_then(Value::as_str)
        {
            cap.model_provider = Some(mp.to_string());
        }
    }
    if n.method == "turn/completed" {
        cap.completed_status = n
            .params
            .get("turn")
            .and_then(|t| t.get("status"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        cap.turn_completed = Some(n.params.clone());
    }
}

fn render(cap: &Captured, header: &str) -> String {
    let mut s = String::new();
    s.push_str(header);
    s.push('\n');
    s.push_str(&format!(
        "item_deltas={}\nstatus_active={} status_idle={}\ntoken_usage(thread/tokenUsage/updated)={}\nmodel_provider={:?}\ncompleted_status={:?}\nturn_completed={}\n\n=== FRAMES (method  params) ===\n",
        cap.item_deltas,
        cap.saw_status_active,
        cap.saw_status_idle,
        cap.saw_token_usage,
        cap.model_provider,
        cap.completed_status,
        cap.turn_completed
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "NONE".into()),
    ));
    for f in &cap.frames {
        s.push_str(f);
        s.push('\n');
    }
    s
}

/// Append the rollout tail (raw lines) to an evidence body — the durable at-source
/// evidence the artifact must carry.
fn rollout_tail_blurb(rollout: Option<&PathBuf>) -> String {
    match rollout {
        Some(p) if p.exists() => {
            let body = std::fs::read_to_string(p).unwrap_or_default();
            format!(
                "\n=== ROLLOUT TAIL ({}) ===\n{}\n",
                p.display(),
                body.lines()
                    .rev()
                    .take(12)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
        Some(p) => format!("\n=== ROLLOUT ({}) NOT MATERIALIZED ===\n", p.display()),
        None => "\n=== ROLLOUT PATH UNRESOLVED ===\n".to_string(),
    }
}

// ===========================================================================
// LANE 1 — stream + status PUSH + rollout-Busy DURING, in one economical turn.
// ===========================================================================

#[test]
fn clive_l1_stream_status_rollout_during_openrouter() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping clive L1 (stream/status/rollout-during)");
        return;
    }
    let or_key = openrouter_key();
    // SAFETY: single-threaded test setup before any spawn; the daemon inherits
    // this var (the spawner does not env_clear).
    unsafe { std::env::set_var("OPENROUTER_API_KEY", &or_key) };

    let jail = make_jail("l1", true);
    let env = or_jail_env(&jail, &or_key);
    let sp = spawn_jailed(&jail, &env, "cdx-l1");
    let reaper = ReapOnDrop(sp.pid);

    let mut cap = Captured::default();
    let rpc = connect_initialized(&sp.endpoint);
    let cwd_str = sp.cwd.to_string_lossy().into_owned();
    let tid = own_thread(&rpc, &cwd_str); // OWN the thread → the content stream flows here.
    // A multi-second turn so task_started is observable BEFORE task_complete (a
    // one-liner can complete before the rollout-Busy window opens). Still cheap.
    let t0 = Instant::now();
    let _turn_id = rpc
        .turn_start(
            &tid,
            "Count from 1 to 20, one number per line, then write the word DONE.",
        )
        .expect("L1 turn_start");

    // Single loop: poll the rollout for Busy (tolerating the None→present lazy race)
    // WHILE draining notifications, until turn/completed.
    let mut saw_busy_during = false;
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline && cap.turn_completed.is_none() {
        if !saw_busy_during {
            if let Some(p) = resolve_rollout(&env, &sp, &tid) {
                if derive_status(&read_lines(&p)) == Some(SessionStatus::Busy) {
                    saw_busy_during = true;
                }
            }
        }
        match rpc.next_notification(Duration::from_millis(300)) {
            Ok(Some(n)) => record(&mut cap, &n, t0.elapsed().as_millis()),
            Ok(None) => {}
            Err(_) => break,
        }
    }
    let _ = rpc.close();

    // Post-completion rollout tail: balanced → Idle, anchors present.
    let rollout = resolve_rollout(&env, &sp, &tid);
    let post_lines = rollout.as_ref().map(|p| read_lines(p)).unwrap_or_default();
    let post_idle = derive_status(&post_lines) == Some(SessionStatus::Idle);
    let has_started = post_lines
        .iter()
        .any(|r| matches!(r.line, RolloutLine::TaskStarted { .. }));
    let has_complete = post_lines
        .iter()
        .any(|r| matches!(r.line, RolloutLine::TaskComplete { .. }));
    let no_open = open_turn_id(&post_lines).is_none();

    // POST-GATE artifact (no-skip-masquerade): the captured real turn + rollout tail.
    let dir = evidence_dir("openrouter-l1-stream-status-rollout");
    let mut body = render(&cap, "# C-LIVE L1 — stream + status PUSH + rollout-Busy-during");
    body.push_str(&format!(
        "\nsaw_busy_DURING={saw_busy_during}\npost_idle={post_idle} has_task_started={has_started} has_task_complete={has_complete} no_open_turn={no_open}\n"
    ));
    body.push_str(&rollout_tail_blurb(rollout.as_ref()));
    ev_text(&dir, "turn-completed.txt", &body);

    // Assertions — every §5.2-1 clause.
    assert!(cap.item_deltas >= 1, "captured item/* content-stream deltas");
    assert!(
        cap.turn_completed.is_some(),
        "captured the turn/completed terminal frame"
    );
    assert_eq!(
        cap.completed_status.as_deref(),
        Some("completed"),
        "turn/completed.turn.status == completed"
    );
    assert!(
        cap.saw_token_usage,
        "captured the usage frame (thread/tokenUsage/updated)"
    );
    assert!(
        cap.saw_status_active && cap.saw_status_idle,
        "observed the status active↔idle PUSH bracket"
    );
    assert!(
        saw_busy_during,
        "rollout tail derived Busy PROMPTLY DURING the in-flight turn"
    );
    assert!(post_idle, "rollout tail derives Idle AFTER completion");
    assert!(
        has_started && has_complete && no_open,
        "rollout has balanced task_started + task_complete anchors"
    );

    drop(reaper);
    wait_dead(sp.pid);
    assert!(
        !dispatch::effects::is_pid_alive(sp.pid as i32),
        "L1 daemon reaped (pid {})",
        sp.pid
    );
    assert!(
        !jail_codex_daemon_alive(&sp.codex_home),
        "no codex app-server survives for the L1 jail CODEX_HOME"
    );
    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// LANE 2 — LANDED steer happy-path (beyond C-RED's negative stale-fence).
// ===========================================================================

#[test]
fn clive_l2_steer_landed_openrouter() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping clive L2 (steer landed)");
        return;
    }
    let or_key = openrouter_key();
    unsafe { std::env::set_var("OPENROUTER_API_KEY", &or_key) };

    let jail = make_jail("l2", true);
    let env = or_jail_env(&jail, &or_key);
    let sp = spawn_jailed(&jail, &env, "cdx-l2");
    let reaper = ReapOnDrop(sp.pid);

    let mut cap = Captured::default();
    let rpc = connect_initialized(&sp.endpoint);
    let cwd_str = sp.cwd.to_string_lossy().into_owned();
    let tid = own_thread(&rpc, &cwd_str);
    // A LONG in-flight turn so the steer lands while the original is still active
    // (gpt-4o-mini is fast — RUN-1's count-to-60 finished before the steer; a
    // 400-line reflective count keeps the turn in-flight for many seconds).
    let t0 = Instant::now();
    let turn_id = rpc
        .turn_start(
            &tid,
            "Count from 1 to 400, one number per line, and after each number write \
             one full sentence reflecting on that number.",
        )
        .expect("L2 turn_start");

    // Steer the in-flight turn IMMEDIATELY (turn_start returns at the inProgress
    // ack — non-blocking, confirmed via CodexProvider::inject — so the turn is
    // active right now).
    let outcome = rpc
        .turn_steer(
            &tid,
            &turn_id,
            "Stop counting. Instead reply with exactly the word STEERED.",
        )
        .expect("L2 turn_steer rpc");

    // Drain to completion for the durable evidence.
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline && cap.turn_completed.is_none() {
        match rpc.next_notification(Duration::from_millis(300)) {
            Ok(Some(n)) => record(&mut cap, &n, t0.elapsed().as_millis()),
            Ok(None) => {}
            Err(_) => break,
        }
    }
    let _ = rpc.close();
    let rollout = resolve_rollout(&env, &sp, &tid);

    let dir = evidence_dir("openrouter-l2-steer-landed");
    let mut body = render(&cap, "# C-LIVE L2 — LANDED steer happy-path");
    body.push_str(&format!("\nsteer_outcome={outcome:?}\noriginal_turn_id={turn_id}\n"));
    body.push_str(&rollout_tail_blurb(rollout.as_ref()));
    ev_text(&dir, "turn-completed.txt", &body);

    match &outcome {
        SteerOutcome::Steered(id) => {
            assert!(!id.is_empty(), "steer landed with a turn id");
        }
        SteerOutcome::StaleTurn(e) => panic!(
            "L2 steer did NOT land — stale fence (the original turn completed too \
             fast; lengthen the prompt). server error: {e:?}"
        ),
    }

    drop(reaper);
    wait_dead(sp.pid);
    assert!(
        !dispatch::effects::is_pid_alive(sp.pid as i32),
        "L2 daemon reaped (pid {})",
        sp.pid
    );
    assert!(
        !jail_codex_daemon_alive(&sp.codex_home),
        "no codex app-server survives for the L2 jail CODEX_HOME"
    );
    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// LANE 3 — live turn_interrupt cancels an in-flight turn (SOLE exerciser of the
// present-but-unused production wiring). A real misfire here is ESCALATED.
// ===========================================================================

#[test]
fn clive_l3_interrupt_openrouter() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping clive L3 (interrupt)");
        return;
    }
    let or_key = openrouter_key();
    unsafe { std::env::set_var("OPENROUTER_API_KEY", &or_key) };

    let jail = make_jail("l3", true);
    let env = or_jail_env(&jail, &or_key);
    let sp = spawn_jailed(&jail, &env, "cdx-l3");
    let reaper = ReapOnDrop(sp.pid);

    let mut cap = Captured::default();
    let rpc = connect_initialized(&sp.endpoint);
    let cwd_str = sp.cwd.to_string_lossy().into_owned();
    let tid = own_thread(&rpc, &cwd_str);
    // A VERY long turn: natural completion would take far longer than our post-
    // interrupt idle window, so reaching idle quickly is evidence the interrupt cut
    // it short (not a natural finish).
    let t0 = Instant::now();
    let turn_id = rpc
        .turn_start(
            &tid,
            "Count from 1 to 500, one number per line, and after each number write \
             one full sentence reflecting on that number.",
        )
        .expect("L3 turn_start");

    // Confirm the turn is genuinely in-flight before interrupting (with the thread
    // owned, turn/started + item/* arrive within ~1s).
    let mut in_flight = false;
    let started_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < started_deadline && !in_flight {
        match rpc.next_notification(Duration::from_millis(300)) {
            Ok(Some(n)) => {
                let started = n.method == "turn/started" || n.method.starts_with("item/");
                record(&mut cap, &n, t0.elapsed().as_millis());
                if started {
                    in_flight = true;
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    assert!(
        in_flight,
        "L3 observed the turn in-flight (turn/started or an item delta) before interrupt"
    );

    let interrupt_ok = rpc.turn_interrupt(&tid, &turn_id);

    // The turn should TERMINATE promptly (idle / turn/completed) — far faster than
    // a count-to-500 would naturally finish.
    let mut terminated = false;
    let term_deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < term_deadline && !terminated {
        match rpc.next_notification(Duration::from_millis(300)) {
            Ok(Some(n)) => record(&mut cap, &n, t0.elapsed().as_millis()),
            Ok(None) => {}
            Err(_) => break,
        }
        if cap.turn_completed.is_some() {
            terminated = true;
        } else if let Some(p) = resolve_rollout(&env, &sp, &tid) {
            if derive_status(&read_lines(&p)) == Some(SessionStatus::Idle) {
                terminated = true;
            }
        }
    }
    let _ = rpc.close();
    let rollout = resolve_rollout(&env, &sp, &tid);

    let dir = evidence_dir("openrouter-l3-interrupt");
    let mut body = render(&cap, "# C-LIVE L3 — live turn_interrupt cancels in-flight");
    body.push_str(&format!(
        "\ninterrupt_rpc_ok={}\nterminated_promptly={terminated}\ninterrupted_turn_id={turn_id}\n",
        interrupt_ok.is_ok()
    ));
    body.push_str(&rollout_tail_blurb(rollout.as_ref()));
    ev_text(&dir, "turn-completed.txt", &body);

    // ESCALATION SEAM: if the interrupt RPC itself errors, that is a real wiring
    // gap — fail loudly so the run is bubbled to the codex owner (NOT fixed here).
    interrupt_ok.expect(
        "L3 turn_interrupt RPC returned an error — a REAL adapter wiring gap; \
         ESCALATE to the codex owner (do NOT fix production)",
    );
    assert!(
        terminated,
        "L3 interrupt cancelled the in-flight turn — it reached idle/terminal far \
         sooner than a count-to-300 would naturally finish (completed_status={:?})",
        cap.completed_status
    );

    drop(reaper);
    wait_dead(sp.pid);
    assert!(
        !dispatch::effects::is_pid_alive(sp.pid as i32),
        "L3 daemon reaped (pid {})",
        sp.pid
    );
    assert!(
        !jail_codex_daemon_alive(&sp.codex_home),
        "no codex app-server survives for the L3 jail CODEX_HOME"
    );
    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// LANE 4 — resume history-recovery LIVE re-confirm (light; e2e/resume_kill is the
// primary coverage).
// ===========================================================================

#[test]
fn clive_l4_resume_history_recovery_openrouter() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping clive L4 (resume history-recovery)");
        return;
    }
    let or_key = openrouter_key();
    unsafe { std::env::set_var("OPENROUTER_API_KEY", &or_key) };

    let jail = make_jail("l4", true);
    let env = or_jail_env(&jail, &or_key);
    let sp = spawn_jailed(&jail, &env, "cdx-l4");
    let reaper = ReapOnDrop(sp.pid);

    // One quick turn (on an owned thread) so an on-disk rollout (history) exists.
    let cwd_str = sp.cwd.to_string_lossy().into_owned();
    let mut cap = Captured::default();
    let tid = {
        let rpc = connect_initialized(&sp.endpoint);
        let tid = own_thread(&rpc, &cwd_str);
        let t0 = Instant::now();
        let _ = rpc
            .turn_start(&tid, "Reply with exactly the text OK.")
            .expect("L4 turn_start");
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline && cap.turn_completed.is_none() {
            match rpc.next_notification(Duration::from_millis(300)) {
                Ok(Some(n)) => record(&mut cap, &n, t0.elapsed().as_millis()),
                Ok(None) => {}
                Err(_) => break,
            }
        }
        let _ = rpc.close();
        tid
    };

    // Fresh connection → thread/resume hydrates the existing rollout (history
    // recovery across a new connection). A non-existent thread would surface
    // "no rollout found".
    let resume_result = {
        let rpc = connect_initialized(&sp.endpoint);
        let r = rpc.thread_resume(&tid);
        let _ = rpc.close();
        r
    };

    let rollout = resolve_rollout(&env, &sp, &tid);
    let lines = rollout.as_ref().map(|p| read_lines(p)).unwrap_or_default();
    let has_history = lines
        .iter()
        .any(|r| matches!(r.line, RolloutLine::TaskStarted { .. }))
        && lines
            .iter()
            .any(|r| matches!(r.line, RolloutLine::TaskComplete { .. }));

    let dir = evidence_dir("openrouter-l4-resume-history-recovery");
    let mut body = render(&cap, "# C-LIVE L4 — resume history-recovery (light re-confirm)");
    body.push_str(&format!(
        "\nthread_resume_ok={}\nrollout_has_history(task_started+task_complete)={has_history}\n",
        resume_result.is_ok()
    ));
    body.push_str(&rollout_tail_blurb(rollout.as_ref()));
    ev_text(&dir, "turn-completed.txt", &body);

    resume_result.expect("L4 thread/resume hydrated the existing rollout (history recovery)");
    assert!(
        has_history,
        "L4 rollout carries the recovered history (task_started + task_complete)"
    );

    drop(reaper);
    wait_dead(sp.pid);
    assert!(
        !dispatch::effects::is_pid_alive(sp.pid as i32),
        "L4 daemon reaped (pid {})",
        sp.pid
    );
    assert!(
        !jail_codex_daemon_alive(&sp.codex_home),
        "no codex app-server survives for the L4 jail CODEX_HOME"
    );
    let _ = std::fs::remove_dir_all(&jail);
}

// ===========================================================================
// PRODUCTION — the LOAD-BEARING ChatGPT-OAuth turn (>=1 total). Double-gated.
// Run this in its OWN cargo invocation (filter to this test) so no OpenRouter lane
// sets OPENROUTER_API_KEY in-process — the daemon inherits the process env.
// ===========================================================================

#[test]
fn clive_prod_chatgpt_oauth_turn() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping clive PROD (chatgpt-oauth)");
        return;
    }
    if !prod_turn() {
        eprintln!(
            "QD_CODEX_PROD_TURN != 1 — skipping the LOAD-BEARING production OAuth turn \
             (it spends Pete's limited ChatGPT subscription)"
        );
        return;
    }

    // NEGATIVE PROVENANCE (process-level): the daemon inherits the process env, so
    // scrub BOTH provider keys and assert their absence — a leaked OpenRouter key
    // would let the "production" turn secretly run OpenRouter (the masquerade).
    // SAFETY: single-threaded prod-turn setup; run this test in isolation.
    unsafe {
        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");
    }
    assert!(
        std::env::var("OPENROUTER_API_KEY").is_err(),
        "no OPENROUTER_API_KEY in the prod turn's process env"
    );
    assert!(
        std::env::var("OPENAI_API_KEY").is_err(),
        "no OPENAI_API_KEY in the prod turn's process env (codex self-auths from OAuth)"
    );

    let real_home = std::env::var("HOME").expect("real HOME for the OAuth store");
    let real_codex_home = PathBuf::from(&real_home).join(".codex");
    let auth = real_codex_home.join("auth.json");
    assert!(
        auth.exists() && std::fs::metadata(&auth).map(|m| m.len() > 100).unwrap_or(false),
        "real ~/.codex/auth.json present (ChatGPT OAuth store): {}",
        auth.display()
    );

    let jail = make_jail("prod", false); // NO openrouter config.toml
    let env = prod_jail_env(&jail, &real_codex_home);
    // NEGATIVE PROVENANCE (env-block): the jail env carries no provider keys.
    assert!(env.var("OPENROUTER_API_KEY").is_none());
    assert!(env.var("OPENAI_API_KEY").is_none());

    let sp = spawn_jailed(&jail, &env, "cdx-prod");
    let reaper = ReapOnDrop(sp.pid);

    let mut cap = Captured::default();
    let rpc = connect_initialized(&sp.endpoint);
    let cwd_str = sp.cwd.to_string_lossy().into_owned();
    let tid = own_thread(&rpc, &cwd_str);
    let t0 = Instant::now();
    let _turn_id = rpc
        .turn_start(&tid, "Reply with exactly OK")
        .expect("PROD turn_start (the one OAuth turn)");
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline && cap.turn_completed.is_none() {
        match rpc.next_notification(Duration::from_millis(300)) {
            Ok(Some(n)) => record(&mut cap, &n, t0.elapsed().as_millis()),
            Ok(None) => {}
            Err(_) => break,
        }
    }
    let _ = rpc.close();

    // POSITIVE PROVENANCE: the rollout landed under the REAL ~/.codex/sessions
    // (only reachable via the OAuth CODEX_HOME) and did NOT run openrouter
    // gpt-4o-mini. The real-store rollout is the EXPECTED side-effect — do NOT scrub.
    let rollout = resolve_rollout(&env, &sp, &tid);
    let rollout_under_real = rollout
        .as_ref()
        .map(|p| p.starts_with(&real_codex_home))
        .unwrap_or(false);
    let rollout_body = rollout
        .as_ref()
        .filter(|p| p.exists())
        .map(|p| std::fs::read_to_string(p).unwrap_or_default())
        .unwrap_or_default();
    let no_gpt4o_mini = !rollout_body.contains("gpt-4o-mini");
    // AFFIRMATIVE model-id from the rollout (turn_context/session_meta payload.model)
    // — the chatgpt-path model, captured for at-source verification.
    let rollout_model = rollout_body.lines().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        v.get("payload")
            .and_then(|p| p.get("model"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    // AFFIRMATIVE provenance at the OAuth STORE: auth_mode == chatgpt and NO
    // OPENAI_API_KEY in auth.json. (Parse only these fields — NEVER write the
    // tokens into the artifact.)
    let auth_json: Value =
        serde_json::from_str(&std::fs::read_to_string(&auth).unwrap_or_default())
            .unwrap_or(Value::Null);
    let auth_mode = auth_json
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let auth_has_openai_key = auth_json
        .get("OPENAI_API_KEY")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    let model_provider = cap.model_provider.clone();

    let dir = evidence_dir("production-chatgpt-oauth");
    let mut body = render(&cap, "# C-LIVE PROD — ChatGPT-OAuth load-bearing turn");
    body.push_str(&format!(
        "\nPROVENANCE (two-sided)\n  AFFIRMATIVE (chatgpt path):\n    \
         auth_mode={auth_mode:?}\n    model_provider(thread/started)={model_provider:?}\n    \
         rollout_model={rollout_model:?}\n    rollout_under_real_codex_home={rollout_under_real}\n    \
         rollout_path={:?}\n  NEGATIVE (not openrouter/api-key):\n    \
         no_openrouter_gpt-4o-mini_in_rollout={no_gpt4o_mini}\n    \
         auth.json_has_OPENAI_API_KEY={auth_has_openai_key}\n    \
         no_OPENROUTER_API_KEY_in_env=true\n    no_OPENAI_API_KEY_in_env=true\n  \
         real_codex_home={}\n",
        rollout.as_ref().map(|p| p.display().to_string()),
        real_codex_home.display(),
    ));
    body.push_str(&rollout_tail_blurb(rollout.as_ref()));
    ev_text(&dir, "turn-completed.txt", &body);

    // Assertions — the turn happened AND the provenance is chatgpt (two-sided:
    // affirmative chatgpt + negative not-openrouter).
    assert!(
        cap.turn_completed.is_some(),
        "PROD captured a real turn/completed from the OAuth path"
    );
    assert_eq!(
        cap.completed_status.as_deref(),
        Some("completed"),
        "PROD turn/completed.turn.status == completed"
    );
    assert!(
        rollout_under_real,
        "PROD rollout landed under the REAL ~/.codex/sessions (OAuth store), not a jail: {:?}",
        rollout.as_ref().map(|p| p.display().to_string())
    );
    // AFFIRMATIVE: the OAuth store is in chatgpt mode (not an API-key auth).
    assert_eq!(
        auth_mode.as_deref(),
        Some("chatgpt"),
        "PROD ~/.codex/auth.json auth_mode == chatgpt (affirmative OAuth provenance)"
    );
    // AFFIRMATIVE: the turn's model provider is NOT openrouter (the jailed lane's).
    assert!(
        model_provider.as_deref() != Some("openrouter"),
        "PROD thread modelProvider is the chatgpt path, NOT openrouter: {model_provider:?}"
    );
    // NEGATIVE: no api-key auth anywhere, no openrouter model.
    assert!(
        !auth_has_openai_key,
        "PROD auth.json carries no OPENAI_API_KEY (codex self-auths via OAuth)"
    );
    assert!(
        no_gpt4o_mini,
        "PROD turn did NOT run openrouter gpt-4o-mini (negative provenance)"
    );

    drop(reaper);
    wait_dead(sp.pid);
    assert!(
        !dispatch::effects::is_pid_alive(sp.pid as i32),
        "PROD daemon reaped (pid {})",
        sp.pid
    );
    // NOTE: no CODEX_HOME-env no-survivor belt here — CODEX_HOME is the REAL
    // ~/.codex and would match Pete's unrelated codex procs. Pid-death is the belt.
    // Remove only the JAIL tree; the rollout under real ~/.codex/sessions STAYS
    // (expected evidence — do NOT scrub ~/.codex).
    let _ = std::fs::remove_dir_all(&jail);
}
