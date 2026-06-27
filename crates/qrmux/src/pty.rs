use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// What command a session's PTY should run.
///
/// `None` at the `Pty::spawn` / `Session::new` boundary preserves today's exact
/// behavior — the platform default login shell via
/// `CommandBuilder::new_default_prog`, inheriting the daemon's cwd. That path is
/// what `Connect` uses, so the interactive `attach`/`open` flow is byte-for-byte
/// unchanged. `Some` is used only by the v2 `CreateDetached` path, which passes
/// `["bash","-lc",<shell_cmd>]` (see [`CommandSpec::login_shell_c`]) plus an
/// explicit cwd — the daemon's own cwd is arbitrary after `setsid`, so
/// inheriting it would be wrong by construction (C1 D1/R27).
#[derive(Debug, Clone, PartialEq)]
pub struct CommandSpec {
    /// Full argv: program plus arguments. Must be non-empty.
    pub argv: Vec<String>,
}

impl CommandSpec {
    /// `["bash", "-lc", <shell_cmd>]` — a login shell running one command, the
    /// exact shape `CreateDetached` requires.
    pub fn login_shell_c(shell_cmd: &str) -> Self {
        Self {
            argv: vec!["bash".to_string(), "-lc".to_string(), shell_cmd.to_string()],
        }
    }
}

/// Shared PTY writer handle.
pub type SharedPtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;
/// Shared master PTY handle.
pub type SharedMasterPty = Arc<Mutex<Box<dyn MasterPty + Send>>>;
/// Shared child process handle.
pub type SharedChild = Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>;

/// Wrapper around a pseudo-terminal with shared access to the master, writer, and child process.
pub struct Pty {
    writer: SharedPtyWriter,
    master: SharedMasterPty,
    child: SharedChild,
}

impl Pty {
    /// Spawn a process in a PTY with the given dimensions.
    ///
    /// `command`/`cwd` of `None`/`None` preserves today's exact behavior: the
    /// default login shell (`CommandBuilder::new_default_prog`) inheriting the
    /// daemon's cwd. This is the `Connect` path — unchanged. `Some(spec)` runs
    /// that argv; `Some(cwd)` sets an EXPLICIT working directory on the
    /// `CommandBuilder` (CreateDetached, C1 D1/R27).
    pub fn spawn(
        cols: u16,
        rows: u16,
        command: Option<CommandSpec>,
        cwd: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = match command {
            None => CommandBuilder::new_default_prog(),
            Some(spec) => {
                let mut argv = spec.argv.into_iter();
                let prog = argv
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("CommandSpec.argv must not be empty"))?;
                let mut b = CommandBuilder::new(prog);
                for arg in argv {
                    b.arg(arg);
                }
                b
            }
        };
        cmd.env("TERM", "xterm-256color");
        // Explicit cwd ONLY when requested. Leaving it unset preserves the
        // None/None inherit-daemon-cwd behavior the Connect path depends on.
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        let child = pair.slave.spawn_command(cmd)?;
        let writer = pair.master.take_writer()?;

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            master: Arc::new(Mutex::new(pair.master)),
            child: Arc::new(Mutex::new(child)),
        })
    }

    /// Return a shared reference to the PTY writer.
    pub fn writer_arc(&self) -> SharedPtyWriter {
        self.writer.clone()
    }

    /// Return a shared reference to the child process.
    pub fn child_arc(&self) -> SharedChild {
        self.child.clone()
    }

    /// Return a shared reference to the master PTY (used for reading output and resizing).
    pub fn master_arc(&self) -> SharedMasterPty {
        self.master.clone()
    }

    /// Check if the child process is still alive.
    /// Uses `try_lock()` to avoid blocking Tokio workers.
    pub fn is_child_alive(&self) -> bool {
        match self.child.try_lock() {
            Ok(mut c) => c.try_wait().ok().flatten().is_none(),
            Err(std::sync::TryLockError::WouldBlock) => true,
            Err(std::sync::TryLockError::Poisoned(e)) => {
                tracing::warn!(error = %e, "child mutex poisoned in is_alive");
                false
            }
        }
    }

    /// Clone the PTY reader for use by the persistent reader thread.
    pub fn clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
        let master = self
            .master
            .lock()
            .map_err(|e| anyhow::anyhow!("master mutex poisoned: {}", e))?;
        master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("failed to clone PTY reader: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_spec_login_shell_c() {
        let spec = CommandSpec::login_shell_c("echo hi");
        assert_eq!(spec.argv, vec!["bash", "-lc", "echo hi"]);
    }

    /// CreateDetached path: a CommandSpec + explicit cwd runs the command in
    /// that directory. Asserted via the command writing its own cwd to a file
    /// (ADD-6: assert on what the program did, not on echo).
    #[test]
    fn spawn_runs_command_in_explicit_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let marker = cwd.join("cwd.out");

        // `pwd -P` resolves symlinks so it matches the canonicalized cwd.
        let spec = CommandSpec::login_shell_c("pwd -P > cwd.out");
        let pty = Pty::spawn(80, 24, Some(spec), Some(cwd.clone())).unwrap();

        // Wait (bounded) for the child to write the marker file.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "command did not run / write marker file");
        let got = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            got.trim(),
            cwd.to_string_lossy(),
            "command ran in the wrong cwd"
        );
        drop(pty);
    }

    /// None/None preserves the default-shell, inherit-cwd behavior (regression
    /// guard for the Connect path): the spawn succeeds and the child is alive.
    #[test]
    fn spawn_none_none_uses_default_shell() {
        let pty = Pty::spawn(80, 24, None, None).unwrap();
        assert!(pty.is_child_alive());
        drop(pty);
    }
}
