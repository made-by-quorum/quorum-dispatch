//! Shared support for the codex LIVE/binary-driving test targets (C-CONF, C-RED,
//! C-CHAOS). Drives the `qd` BINARY by verb name in a jail under
//! CARGO_TARGET_TMPDIR, reads/asserts at SOURCE (registry / rollout / proc-tree /
//! ws endpoint), reaps instance-addressed (never a name-addressed pkill), and
//! captures a primary-evidence bundle. Extracted from codex_conformance_live.rs so
//! the red-team / fault-injection targets reuse it rather than copy-paste.
//!
//! A submodule of `common` (sibling of `p0bins`) so the binary-driving helpers stay
//! namespaced (`common::live::*`) and the fixture-based `common` helpers are
//! untouched. `#![allow(dead_code)]`: shared across several targets, each using only
//! a subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use dispatch::provider::codex::{AppServerRpc, ClientInfo, WsAppServer};
use dispatch::registry::RegistryEntry;

/// The codex live gate — a thin delegate through the single-home keyed gate
/// [`super::live_gate::conformance_gate`] (C-4 (a)). The env READ lives only in
/// `common/live_gate.rs`; this must not read `QD_CODEX_LIVE` directly or the
/// drift lint reds it.
pub fn live() -> bool {
    super::live_gate::conformance_gate("QD_CODEX_LIVE", "codex-live (common)")
}

/// The `qd` binary under test (Cargo sets this for the dev binary).
pub fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// Build the jail tree under CARGO_TARGET_TMPDIR (workspace tree, never /tmp).
pub fn make_jail(tag: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "cdxlive-{tag}-{}-{}",
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
    // A minimal config.toml so the daemon boots cleanly. NO model call is made by
    // thread/start (tier-a never starts a turn), but the daemon reads this at boot,
    // so it must parse. The KEY is absent — env_key names a var we never set.
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

/// A SHORT XDG_RUNTIME_DIR for the jail, kept directly under CARGO_TARGET_TMPDIR (NOT
/// under the long jail subdir) so `<XDG_RUNTIME_DIR>/qrmux/<name>.sock` fits the
/// 104-byte `sun_path` budget `resolve_qrmux_dir` guards (the gather pipeline ls/info/
/// stop run resolves the qrmux dir even for a codex-only registry). Derived
/// deterministically from the jail's unique suffix so it is stable across the many
/// `qd` invocations in one test, and distinct per jail.
pub fn runtime_dir(jail: &Path) -> PathBuf {
    let fname = jail.file_name().and_then(|s| s.to_str()).unwrap_or("j");
    let tail: String = fname.chars().rev().take(12).collect();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("qr{tail}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The jail env block applied to every `qd` Command (env_clear + envs). `env -i`
/// shape: only the codex-relevant vars resolve; HOME drives the registry, CODEX_HOME
/// jails the rollout/auth store, XDG_RUNTIME_DIR keeps the qrmux socket path short,
/// PATH lets the real `codex` binary resolve.
pub fn jail_vars(jail: &Path) -> Vec<(String, String)> {
    let s = |p: PathBuf| p.to_string_lossy().into_owned();
    vec![
        ("HOME".into(), s(jail.join("home"))),
        ("CODEX_HOME".into(), s(jail.join("codex-home"))),
        ("XDG_CONFIG_HOME".into(), s(jail.join("xdg-config"))),
        ("XDG_DATA_HOME".into(), s(jail.join("xdg-data"))),
        ("XDG_CACHE_HOME".into(), s(jail.join("xdg-cache"))),
        ("XDG_RUNTIME_DIR".into(), s(runtime_dir(jail))),
        ("TMPDIR".into(), s(jail.join("tmp"))),
        (
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        ),
    ]
}

/// Run a `qd` verb BY NAME in the jail. Returns the raw Output for the evidence
/// bundle — but conformance is asserted AT SOURCE, never off this return.
pub fn run_qd(jail: &Path, args: &[&str]) -> Output {
    Command::new(qd_bin())
        .args(args)
        .env_clear()
        .envs(jail_vars(jail))
        .current_dir(jail.join("work"))
        .output()
        .expect("qd binary runs")
}

/// The jailed registry dir (HOME/.claude/sessions — paths_from_home/QdPaths).
pub fn sessions_dir(jail: &Path) -> PathBuf {
    jail.join("home").join(".claude").join("sessions")
}

/// The jailed qd data state dir (HOME/.quorum/dispatch/state — QD_HOME unset default).
pub fn state_dir(jail: &Path) -> PathBuf {
    jail.join("home")
        .join(".quorum")
        .join("dispatch")
        .join("state")
}

/// The jailed daemon log dir (HOME/.quorum/dispatch/log).
pub fn log_dir(jail: &Path) -> PathBuf {
    jail.join("home").join(".quorum").join("dispatch").join("log")
}

/// A PRIMARY-EVIDENCE bundle dir the acceptance oracle rules on. Root is
/// `$CCONF_EVIDENCE_DIR` (pinned + reported at run time) or, absent that, a stable
/// subdir of `CARGO_TARGET_TMPDIR`. Per-test subdir so concurrent tests don't
/// collide. Capture at-source artifacts (rows / tombstones / logs / ids / proc-tree
/// / endpoints) BEFORE teardown so the jail can be reclaimed without losing evidence.
pub fn evidence_dir(tag: &str) -> PathBuf {
    let root = std::env::var("CCONF_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cconf-evidence"));
    let dir = root.join(format!(
        "{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    eprintln!("CDXLIVE EVIDENCE [{tag}]: {}", dir.display());
    dir
}

/// Write a text artifact into the bundle.
pub fn ev_text(bundle: &Path, name: &str, body: &str) {
    let _ = std::fs::write(bundle.join(name), body);
}

/// Copy an at-source file (row / tombstone / log / ids.jsonl) into the bundle
/// verbatim — primary evidence, not a transcription.
pub fn ev_copy(bundle: &Path, src: &Path, name: &str) {
    if src.exists() {
        let _ = std::fs::copy(src, bundle.join(name));
    } else {
        ev_text(
            bundle,
            &format!("{name}.MISSING"),
            &format!("{} did not exist", src.display()),
        );
    }
}

/// Snapshot the live `codex app-server` proc tree for THIS jail's CODEX_HOME
/// (pgrep ∩ ps-eww env match) into the bundle — the process-tree primary evidence.
pub fn ev_proctree(bundle: &Path, name: &str, codex_home: &Path) {
    let needle = format!("CODEX_HOME={}", codex_home.display());
    let mut report = format!("# codex app-server procs matching {needle}\n");
    if let Ok(out) = Command::new("pgrep").args(["-f", "codex app-server"]).output() {
        for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if let Ok(ps) = Command::new("ps").args(["eww", "-p", pid]).output() {
                let text = String::from_utf8_lossy(&ps.stdout).into_owned();
                if text.contains(&needle) {
                    report.push_str(&format!("\n## pid {pid}\n{text}\n"));
                }
            }
        }
    }
    ev_text(bundle, name, &report);
}

/// Read EVERY codex registry row at source by scanning the jailed sessions dir for
/// `<pid>.json` files and parsing each via the production `registry::read_entry`
/// (the pid is the filename stem). NEVER parses qd stdout.
pub fn codex_rows(jail: &Path) -> Vec<RegistryEntry> {
    let dir = sessions_dir(jail);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        // Only live rows (`<pid>.json`), not `.tombstoned`.
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(pid) = stem.parse::<i64>() else { continue };
        if let Some(row) = dispatch::registry::read_entry(&dir, pid) {
            if row.provider.as_deref() == Some("codex") {
                out.push(row);
            }
        }
    }
    out
}

/// Parse the ws endpoint port (the real allocator picks ephemeral, OUTSIDE 8900-9000).
pub fn endpoint_port(endpoint: &str) -> u16 {
    endpoint
        .rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("endpoint carries a port: {endpoint}"))
}

/// Is any `codex app-server` for THIS jail's CODEX_HOME alive? Instance-addressed
/// via `ps eww` env match (never a name-addressed pkill, L10).
pub fn jail_codex_daemon_alive(codex_home: &Path) -> bool {
    let needle = format!("CODEX_HOME={}", codex_home.display());
    let Ok(out) = Command::new("pgrep").args(["-f", "codex app-server"]).output() else {
        return false;
    };
    for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if let Ok(ps) = Command::new("ps").args(["eww", "-p", pid]).output() {
            if String::from_utf8_lossy(&ps.stdout).contains(&needle) {
                return true;
            }
        }
    }
    false
}

/// pid-scoped cleanup through the production SIGTERM→grace→SIGKILL + reap ladder.
pub fn reap(pid: i64) {
    use dispatch::create_daemon::DaemonSpawner;
    dispatch::create_daemon::RealDaemonSpawner.kill(pid);
}

/// Wait for a pid to go dead (kernel settle after SIGKILL+reap).
pub fn wait_dead(pid: i64) {
    for _ in 0..20 {
        if !dispatch::effects::is_pid_alive(pid as i32) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// RAII belt: reap any still-recorded pids on drop so a panic never leaks a daemon.
pub struct ReapAll(pub Arc<std::sync::Mutex<Vec<i64>>>);
impl Drop for ReapAll {
    fn drop(&mut self) {
        for pid in self.0.lock().unwrap().iter().copied() {
            if pid > 0 {
                reap(pid);
            }
        }
    }
}

/// A live ws `initialize` handshake against an endpoint — proves the app-server/ws
/// transport is actually live, not just a recorded string.
pub fn ws_initialize_ok(endpoint: &str) -> bool {
    let Ok(rpc) = WsAppServer::connect(endpoint, std::time::Duration::from_secs(5)) else {
        return false;
    };
    let client = ClientInfo {
        name: "cdxlive".into(),
        title: None,
        version: "0".into(),
    };
    let ok = rpc.initialize(&client).is_ok();
    let _ = rpc.initialized();
    let _ = rpc.close();
    ok
}
