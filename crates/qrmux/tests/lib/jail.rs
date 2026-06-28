//! Hermetic test jail — per-run isolation for daemon/test processes.
//!
//! Ported from `test/golden/lib/jail.sh` (tag phase-0b-part1) and adapted for qrmux
//! integration tests. Per ADD-4 and spec ground rule 3:
//!
//! Every daemon/test runs under a per-run hermetic environment: own HOME (load-bearing
//! for qd's registry), QD_HOME, ZMX_DIR, XDG_CONFIG/DATA/STATE/RUNTIME_DIR, TMPDIR,
//! relay port, socket prefix. Daemon sockets live ONLY at $XDG_RUNTIME_DIR/qrmux/.
//!
//! Jail setup:
//! 1. Create per-run temp dirs under a sandbox base (qrmux-<runid>-* prefix for safety)
//! 2. Export hermetic env vars (HOME, XDG_*, ZMX_DIR, TMPDIR, QD_RUST_LOCK_DIR)
//! 3. Verify positive sandboxing (fail-closed: refuse production paths)
//! 4. Capture real HOME for production-path refusal belt
//!
//! Jail teardown:
//! 1. Kill any jailed daemon sessions by socket sweep
//! 2. Verify no live sockets remain under $XDG_RUNTIME_DIR/qrmux/
//! 3. Remove jail_root
//! 4. Idempotent: safe to call multiple times

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use uuid::Uuid;

/// Per-run jail environment and cleanup handles.
#[derive(Debug, Clone)]
pub struct Jail {
    /// Unique run identifier (uuid or test name + timestamp)
    #[allow(dead_code)] // identity for debugging; embedded in jail_root path
    pub runid: String,

    /// Jail root directory (sandbox base: /tmp/qrmux-runs/<runid>/)
    pub jail_root: PathBuf,

    /// Session name prefix for safety: qrmux-<runid>-
    pub session_prefix: String,

    /// Paths within the jail
    pub home: PathBuf,
    pub qd_home: PathBuf,
    pub zmx_dir: PathBuf,
    pub xdg_config: PathBuf,
    pub xdg_data: PathBuf,
    pub xdg_state: PathBuf,
    pub xdg_runtime: PathBuf,
    pub tmpdir: PathBuf,
    pub lock_dir: PathBuf,

    /// Socket directory for qrmux daemons: $XDG_RUNTIME_DIR/qrmux/
    pub socket_dir: PathBuf,

    /// Captured REAL HOME before override (for production-path refusal)
    pub real_home: PathBuf,

    /// Relay port (from runid, reserved range 20000-59999)
    pub relay_port: u16,
}

impl Jail {
    /// Create a new jail with the given runid (or generate one).
    /// Fails closed: if any dir cannot be created or a production path is detected,
    /// returns an error.
    pub fn establish(runid: Option<String>) -> Result<Self, Box<dyn std::error::Error>> {
        // Generate runid if not provided, or append UUID to provided name for uniqueness
        let runid = match runid {
            Some(name) => {
                // Append a short UUID to ensure uniqueness across test runs
                let short_uuid = Uuid::new_v4().to_string()[..8].to_string();
                format!("{}-{}", name, short_uuid)
            }
            None => Uuid::new_v4().to_string(),
        };
        // Sanitize: only alphanumeric, underscore, hyphen
        let runid_clean: String = runid
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();

        if runid_clean.is_empty() {
            return Err("could not derive a usable run id".into());
        }

        // Capture the REAL home BEFORE we override HOME
        let real_home = env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default());

        // Jail root under LITERAL /tmp, never $TMPDIR. War story (macOS CI):
        // GitHub runners set TMPDIR=/var/folders/<hash>/T/ (~48 chars); the
        // daemon socket at <jail>/xdg_runtime/qrmux/qrmux.sock then exceeds
        // macOS's 104-byte sun_path limit and bind() fails — every daemon
        // test died with "socket not created within 5s" on macOS CI while
        // Ubuntu (/tmp) passed. Unix-socket paths must stay short.
        let base = Path::new("/tmp").join("qrmux-runs");
        fs::create_dir_all(&base)
            .map_err(|e| format!("cannot create jail base dir {}: {}", base.display(), e))?;

        let jail_root = base.join(&runid_clean);
        if jail_root.exists() {
            return Err(format!(
                "jail root already exists (run id collision): {}",
                jail_root.display()
            )
            .into());
        }

        // Create all subdirectories
        let subdirs = [
            "home",
            "qd_home",
            "zmx",
            "xdg_config",
            "xdg_data",
            "xdg_state",
            "xdg_runtime",
            "tmp",
            "lock",
        ];

        for subdir in &subdirs {
            let path = jail_root.join(subdir);
            fs::create_dir_all(&path)
                .map_err(|e| format!("cannot create {}: {}", path.display(), e))?;
        }

        // Set permissions on sensitive dirs
        fs::set_permissions(&jail_root, fs::Permissions::from_mode(0o700)).ok();
        fs::set_permissions(
            jail_root.join("xdg_runtime"),
            fs::Permissions::from_mode(0o700),
        )
        .ok();

        let runid_for_port = runid_clean.clone();
        let jail_root_clone = jail_root.clone();

        let jail = Jail {
            runid: runid_clean,
            jail_root: jail_root_clone.clone(),
            session_prefix: format!(
                "qrmux-{}-",
                &jail_root_clone
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ),
            home: jail_root_clone.join("home"),
            qd_home: jail_root_clone.join("qd_home"),
            zmx_dir: jail_root_clone.join("zmx"),
            xdg_config: jail_root_clone.join("xdg_config"),
            xdg_data: jail_root_clone.join("xdg_data"),
            xdg_state: jail_root_clone.join("xdg_state"),
            xdg_runtime: jail_root_clone.join("xdg_runtime"),
            tmpdir: jail_root_clone.join("tmp"),
            lock_dir: jail_root_clone.join("lock"),
            socket_dir: jail_root_clone.join("xdg_runtime").join("qrmux"),
            real_home,
            relay_port: derive_port(&runid_for_port),
        };

        // Create socket dir explicitly
        fs::create_dir_all(&jail.socket_dir).map_err(|e| {
            format!(
                "cannot create socket dir {}: {}",
                jail.socket_dir.display(),
                e
            )
        })?;

        // Verify positive sandboxing (fail-closed)
        jail.assert_established()?;

        // Teardown-leak belt: FIRST stamp this run's owning test-harness pid so a
        // concurrent sibling's setup-reaper sees a live owner and never touches this
        // dir; THEN reap daemons leaked by PRIOR (dead-owner) runs of this family
        // (best-effort, never fails setup). `jail_root` already exists so it is
        // correctly excluded as the current run.
        super::daemon_reaper::stamp_owner_pid(&jail.jail_root);
        let _ = super::daemon_reaper::reap_prior_run_daemons(&jail.jail_root);

        Ok(jail)
    }

    /// Verify the jail is properly established. Fail-closed: refuse any production path.
    pub fn assert_established(&self) -> Result<(), Box<dyn std::error::Error>> {
        // JAIL_ROOT must sit under a recognizable sandbox base
        let jail_root_str = self.jail_root.to_string_lossy();
        if !jail_root_str.contains("qrmux-runs/") {
            return Err(format!(
                "JAIL_ROOT '{}' not under qrmux-runs/ sandbox base",
                jail_root_str
            )
            .into());
        }

        // Each isolation var must be set AND live under JAIL_ROOT
        let vars = [
            ("HOME", &self.home),
            ("QD_HOME", &self.qd_home),
            ("ZMX_DIR", &self.zmx_dir),
            ("XDG_CONFIG_HOME", &self.xdg_config),
            ("XDG_DATA_HOME", &self.xdg_data),
            ("XDG_STATE_HOME", &self.xdg_state),
            ("XDG_RUNTIME_DIR", &self.xdg_runtime),
            ("TMPDIR", &self.tmpdir),
            ("QD_RUST_LOCK_DIR", &self.lock_dir),
        ];

        for (name, val) in &vars {
            let val_str = val.to_string_lossy();

            // Must live under JAIL_ROOT
            if !val_str.starts_with(self.jail_root.to_string_lossy().as_ref()) {
                return Err(format!(
                    "{}='{}' does not resolve under JAIL_ROOT ({})",
                    name,
                    val_str,
                    self.jail_root.display()
                )
                .into());
            }

            // Production-path refusal: explicitly forbid real paths
            if is_production_path(&val_str, &self.real_home) {
                return Err(
                    format!("{}='{}' matches a PRODUCTION path pattern", name, val_str).into(),
                );
            }
        }

        // Relay port must be in reserved range
        if self.relay_port < 20000 || self.relay_port > 59999 {
            return Err(format!(
                "RELAY_PORT {} outside reserved range 20000-59999",
                self.relay_port
            )
            .into());
        }

        Ok(())
    }

    /// Generate environment variables for daemon/test processes to run under this jail.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        vec![
            ("HOME".to_string(), self.home.to_string_lossy().to_string()),
            (
                "QD_HOME".to_string(),
                self.qd_home.to_string_lossy().to_string(),
            ),
            (
                "ZMX_DIR".to_string(),
                self.zmx_dir.to_string_lossy().to_string(),
            ),
            (
                "XDG_CONFIG_HOME".to_string(),
                self.xdg_config.to_string_lossy().to_string(),
            ),
            (
                "XDG_DATA_HOME".to_string(),
                self.xdg_data.to_string_lossy().to_string(),
            ),
            (
                "XDG_STATE_HOME".to_string(),
                self.xdg_state.to_string_lossy().to_string(),
            ),
            (
                "XDG_RUNTIME_DIR".to_string(),
                self.xdg_runtime.to_string_lossy().to_string(),
            ),
            (
                "TMPDIR".to_string(),
                self.tmpdir.to_string_lossy().to_string(),
            ),
            (
                "QD_RUST_LOCK_DIR".to_string(),
                self.lock_dir.to_string_lossy().to_string(),
            ),
            ("QRM_RELAY_PORT".to_string(), self.relay_port.to_string()),
        ]
    }

    /// Tear down the jail: kill sessions, verify socket cleanup, remove jail_root.
    /// Idempotent: safe to call multiple times.
    pub fn teardown(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.jail_root.exists() {
            return Ok(()); // Already torn down
        }

        // Sweep sockets under socket_dir and kill associated daemons
        if self.socket_dir.exists() {
            self._kill_sessions_by_socket()?;
        }

        // Verify nothing unexpected remains. The ONLY entry allowed to
        // outlive the daemon is the ACK-1 `events/` dir — the event stream is
        // a forensic record by design (ack1-spec §1; LESSONS L12) and is
        // removed with jail_root below. Everything else (live sockets, lock
        // files, stray writes) keeps the original full-strictness detection
        // (merge-ruling C-1 refinement: events-specific exclusion, not a
        // *.sock narrowing).
        if self.socket_dir.exists() {
            let leftovers = fs::read_dir(&self.socket_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| !(e.file_name() == "events" && e.path().is_dir()))
                .collect::<Vec<_>>();
            if !leftovers.is_empty() {
                return Err(format!(
                    "unexpected entries remain after teardown under {} ({:?})",
                    self.socket_dir.display(),
                    leftovers
                        .iter()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                )
                .into());
            }
        }

        // Remove jail root
        fs::remove_dir_all(&self.jail_root).map_err(|e| {
            format!(
                "cannot remove jail root {}: {}",
                self.jail_root.display(),
                e
            )
        })?;

        Ok(())
    }

    /// Internal: kill jailed sessions by socket sweep.
    fn _kill_sessions_by_socket(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.socket_dir.exists() {
            return Ok(());
        }

        // For each .sock file in socket_dir, extract session name and kill it
        for entry in fs::read_dir(&self.socket_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "sock") {
                // Try to remove the socket file directly (daemonized process won't be here)
                let _ = fs::remove_file(&path);

                if let Some(filename) = path.file_stem() {
                    let session_name = filename.to_string_lossy().to_string();
                    // Try to kill via qrmux as well (may not work, but worth trying)
                    let _ = self._kill_session_by_name(&session_name);
                }
            }
        }

        // Give processes a moment to fully exit after socket removal
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Retry removing any remaining sockets that may have been recreated
        for entry in fs::read_dir(&self.socket_dir)
            .unwrap_or_else(|_| fs::read_dir(&self.socket_dir).unwrap())
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "sock") {
                let _ = fs::remove_file(&path);
            }
        }

        Ok(())
    }

    /// Internal: kill a session by name (with safety checks).
    fn _kill_session_by_name(&self, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Guard: refuse any name not matching the session prefix
        if !name.starts_with(&self.session_prefix) {
            return Err(format!(
                "session name '{}' does not match required prefix '{}'",
                name, self.session_prefix
            )
            .into());
        }

        // Try to kill via qrmux kill (may fail if qrmux not available, that's OK)
        let _ = Command::new("qrmux")
            .arg("kill")
            .arg(name)
            .env_clear()
            .envs(self.env_vars())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();

        // Bonus: remove the socket file directly if it exists
        let socket_path = self.socket_dir.join(format!("{}.sock", name));
        fs::remove_file(&socket_path).ok();

        Ok(())
    }
}

use std::os::unix::fs::PermissionsExt;

// ---- Helpers ----

/// Derive a stable port (20000-59999) from a run id.
fn derive_port(runid: &str) -> u16 {
    let mut sum: u32 = 0;
    for c in runid.chars() {
        sum = (sum.wrapping_mul(31).wrapping_add(c as u32)) % 40000;
    }
    20000 + (sum as u16)
}

/// Check if a path matches a production path pattern (fail-closed belt).
fn is_production_path(val: &str, real_home: &Path) -> bool {
    let real_home_str = real_home.to_string_lossy();

    // Explicitly forbid patterns that indicate real system paths
    let forbidden_patterns = [
        real_home_str.as_ref(),
        &format!("{}/.quorum/dispatch", real_home_str),
        &format!("{}/.quorum/dispatch", real_home_str),
        &format!("{}/.claude", real_home_str),
        &format!("{}/.config", real_home_str),
        &format!("{}/.local", real_home_str),
        "/tmp/zmx-",
    ];

    for pattern in &forbidden_patterns {
        if val == *pattern || val.starts_with(&format!("{}/", pattern)) {
            return true;
        }
    }

    false
}

// ---- Public conveniences ----

/// Setup a new jail with optional runid. Returns the jail or an error.
pub fn setup_jail(runid: &str) -> Result<Jail, Box<dyn std::error::Error>> {
    Jail::establish(Some(runid.to_string()))
}

/// Tear down a jail. Idempotent.
pub fn teardown_jail(jail: &Jail) -> Result<(), Box<dyn std::error::Error>> {
    jail.teardown()
}

/// Get environment variables for a jail.
pub fn jail_env(jail: &Jail) -> Vec<(String, String)> {
    jail.env_vars()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jail_establishment() {
        let jail = Jail::establish(Some("test1".to_string())).expect("jail setup failed");
        assert!(jail.jail_root.exists());
        assert!(jail.home.exists());
        assert!(jail.socket_dir.exists());

        // Cleanup
        jail.teardown().expect("teardown failed");
        assert!(!jail.jail_root.exists());
    }

    #[test]
    fn test_jail_production_path_refusal() {
        let jail = Jail::establish(Some("test2".to_string())).expect("jail setup failed");
        // Verify the real home was captured and differs from jailed home
        assert!(!jail
            .home
            .to_string_lossy()
            .contains(&jail.real_home.to_string_lossy().to_string()));

        jail.teardown().expect("teardown failed");
    }

    #[test]
    fn test_jail_idempotent_teardown() {
        let jail = Jail::establish(Some("test3".to_string())).expect("jail setup failed");
        jail.teardown().expect("first teardown failed");
        // Second teardown should be idempotent (no error)
        jail.teardown()
            .expect("second teardown should be idempotent");
    }

    #[test]
    fn test_session_prefix_guard() {
        let jail = Jail::establish(Some("test4".to_string())).expect("jail setup failed");
        let session_prefix = jail.session_prefix.clone();

        // Try to kill an unprefixed session (should fail)
        let result = jail._kill_session_by_name("unprefixed-session");
        assert!(result.is_err());

        // The prefix should contain the runid
        assert!(session_prefix.contains("qrmux-"));

        jail.teardown().expect("teardown failed");
    }
}
