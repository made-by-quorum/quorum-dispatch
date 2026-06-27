//! Process-execution seam (spec §4). EVERY process spawn in the crate goes
//! through this trait (the only exceptions are the `ps`/`kill` primitives in
//! [`crate::effects`], which are the process-table seam itself).
//!
//! Why a seam at all: the create/boot path drives zmx and claude by spawning
//! them. Unit tests must run OFFLINE — no live zmx, no live claude — and must be
//! able to ASSERT on exactly which commands ran (the dialog-free boot gate
//! asserts ZERO `zmx send` keystrokes were emitted, L5). [`ScriptedExec`] gives
//! canned responses keyed by `(cmd, args-prefix)` plus an append-only audit log
//! of every invocation so a test can assert "no `zmx send` ever ran".

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// Result of a captured (`run`) process invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecResult {
    /// Exit code, or `None` if the process was killed by a signal (or we timed
    /// out and killed it — see `timed_out`).
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// True if we hit the `timeout_ms` deadline and killed the child. A wedged
    /// zmx must not hang the caller (L3): the preflight probe relies on this.
    pub timed_out: bool,
}

/// One recorded invocation in [`ScriptedExec`]'s append-only audit log.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub cmd: String,
    pub args: Vec<String>,
    pub env_overrides: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub timeout_ms: Option<u64>,
    /// Bytes fed to the child's STDIN (`run_with_stdin`), or `None` for the
    /// plain `run`/`spawn_inherit` paths. Load-bearing for the survey secret-
    /// hygiene assert: the Authorization header is fed via stdin (`curl --config
    /// -`), NEVER as an argv token, so a test inspects `args` for absence and
    /// `stdin` for presence (G-S2).
    pub stdin: Option<String>,
}

/// The execution seam.
pub trait Exec {
    /// Run a command to completion, capturing stdout/stderr.
    ///
    /// `env_overrides` are layered ON TOP of the inherited environment (this is
    /// how ZMX_DIR pinning is applied — TS `zmxEnvForDir` spreads `process.env`
    /// then sets `ZMX_DIR`, utils.ts:133-135).
    ///
    /// `timeout_ms`, when set, is a hard deadline: a wedged child is killed and
    /// `ExecResult::timed_out` is set true (L3 — the `send` capability probe must
    /// never hang on a broken zmx).
    fn run(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> io::Result<ExecResult>;

    /// Run a command to completion, feeding `stdin_bytes` to the child's STDIN,
    /// capturing stdout/stderr. This is the secret-hygiene transport for `survey`
    /// (`curl --config -`): the Authorization header is written to the child's
    /// stdin, NEVER passed as an argv token (ps-visible on a shared host, ADD-10 /
    /// red-team R3). Same timeout semantics as [`Exec::run`].
    fn run_with_stdin(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
        stdin_bytes: &[u8],
    ) -> io::Result<ExecResult>;

    /// Spawn a command with stdio INHERITED (no capture) and wait for it. Used
    /// for the interactive handoff (`qd connect` → `zmx attach`, an exec-style
    /// takeover of the terminal). Returns the child's exit code (or 1 if it was
    /// signalled / produced no code).
    fn spawn_inherit(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
    ) -> io::Result<i32>;
}

/// Blanket impl so a `&dyn Exec` (or any `&T: Exec`) itself satisfies `impl Exec`.
/// The A2 create path holds the exec as `&dyn Exec` but the preflight probe
/// (preflight.rs, M1-frozen) takes `&impl Exec` — this bridge passes the dyn ref
/// straight through with no shim.
impl<T: Exec + ?Sized> Exec for &T {
    fn run(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> io::Result<ExecResult> {
        (**self).run(cmd, args, env_overrides, cwd, timeout_ms)
    }
    fn run_with_stdin(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
        stdin_bytes: &[u8],
    ) -> io::Result<ExecResult> {
        (**self).run_with_stdin(cmd, args, env_overrides, cwd, timeout_ms, stdin_bytes)
    }
    fn spawn_inherit(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
    ) -> io::Result<i32> {
        (**self).spawn_inherit(cmd, args, env_overrides, cwd)
    }
}

/// Real process execution via [`std::process::Command`].
pub struct RealExec;

impl RealExec {
    fn base(
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
    ) -> Command {
        let mut c = Command::new(cmd);
        c.args(args);
        for (k, v) in env_overrides {
            c.env(k, v);
        }
        if let Some(dir) = cwd {
            c.current_dir(dir);
        }
        c
    }
}

impl Exec for RealExec {
    fn run(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> io::Result<ExecResult> {
        let mut command = Self::base(cmd, args, env_overrides, cwd);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // No timeout: the simple, blocking path.
        let Some(timeout_ms) = timeout_ms else {
            let out = command.output()?;
            return Ok(ExecResult {
                status: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                timed_out: false,
            });
        };

        // Timeout path: spawn, drain pipes on threads, then wait with a deadline.
        // A wedged child must not hang the caller (L3) — we kill it and report
        // `timed_out`, salvaging whatever output drained before the deadline.
        let mut child = command.spawn()?;
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let out_handle = std::thread::spawn(move || drain(stdout_pipe));
        let err_handle = std::thread::spawn(move || drain(stderr_pipe));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        let status = loop {
            match child.try_wait()? {
                Some(s) => break Some(s),
                None => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        timed_out = true;
                        break None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        };

        let stdout = out_handle.join().unwrap_or_default();
        let stderr = err_handle.join().unwrap_or_default();
        Ok(ExecResult {
            status: status.and_then(|s| s.code()),
            stdout,
            stderr,
            timed_out,
        })
    }

    fn run_with_stdin(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
        stdin_bytes: &[u8],
    ) -> io::Result<ExecResult> {
        let mut command = Self::base(cmd, args, env_overrides, cwd);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn()?;

        // Write stdin on a thread so a child that emits stdout before draining
        // its stdin cannot deadlock us (pipe-buffer stall).
        let stdin_pipe = child.stdin.take();
        let payload = stdin_bytes.to_vec();
        let in_handle = std::thread::spawn(move || {
            if let Some(mut p) = stdin_pipe {
                let _ = io::Write::write_all(&mut p, &payload);
                // Drop closes the pipe → child sees EOF (needed for `curl
                // --config -`, which reads stdin to EOF).
            }
        });

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let out_handle = std::thread::spawn(move || drain(stdout_pipe));
        let err_handle = std::thread::spawn(move || drain(stderr_pipe));

        // Wait, honoring an optional hard deadline (L3 — a wedged child must not
        // hang the caller; survey passes a 120s per-model deadline).
        let mut timed_out = false;
        let status = match timeout_ms {
            None => Some(child.wait()?),
            Some(ms) => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
                loop {
                    match child.try_wait()? {
                        Some(s) => break Some(s),
                        None => {
                            if std::time::Instant::now() >= deadline {
                                let _ = child.kill();
                                let _ = child.wait();
                                timed_out = true;
                                break None;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(25));
                        }
                    }
                }
            }
        };

        let _ = in_handle.join();
        let stdout = out_handle.join().unwrap_or_default();
        let stderr = err_handle.join().unwrap_or_default();
        Ok(ExecResult {
            status: status.and_then(|s| s.code()),
            stdout,
            stderr,
            timed_out,
        })
    }

    fn spawn_inherit(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
    ) -> io::Result<i32> {
        let mut command = Self::base(cmd, args, env_overrides, cwd);
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command.status()?;
        Ok(status.code().unwrap_or(1))
    }
}

/// Drain a child pipe to a lossy-UTF8 String (helper for the timeout path).
fn drain(pipe: Option<impl io::Read>) -> String {
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = io::Read::read_to_end(&mut p, &mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// A canned response for [`ScriptedExec`], keyed by `(cmd, args-prefix)`.
#[derive(Clone)]
struct Canned {
    cmd: String,
    /// Match when the recorded args START WITH this prefix (so a test can key on
    /// `["list"]` regardless of trailing args).
    args_prefix: Vec<String>,
    result: ExecResult,
}

/// Test double: canned `run` responses + an APPEND-ONLY audit log of every
/// invocation (load-bearing for the M3/M4 zero-keystroke boot assert — a test
/// inspects the log and asserts no `zmx send` ever ran). `spawn_inherit`
/// invocations are also logged; a canned exit code is returned.
///
/// Matching is FIRST-WINS over the registered canned responses, so register the
/// most specific prefix first. An unmatched `run` returns a benign empty success
/// (status 0, empty out/err) — tests that care assert via the audit log, not by
/// relying on a default.
#[derive(Default)]
pub struct ScriptedExec {
    canned: Vec<Canned>,
    /// Append-only; never cleared. Inspect via [`ScriptedExec::log`].
    audit: Mutex<Vec<Invocation>>,
    /// Exit code returned by every `spawn_inherit` (default 0).
    inherit_exit: i32,
}

impl ScriptedExec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a canned `run` response for `cmd` whose args start with
    /// `args_prefix`. First registered match wins.
    pub fn on(
        mut self,
        cmd: &str,
        args_prefix: &[&str],
        status: Option<i32>,
        stdout: &str,
        stderr: &str,
    ) -> Self {
        self.canned.push(Canned {
            cmd: cmd.to_string(),
            args_prefix: args_prefix.iter().map(|s| s.to_string()).collect(),
            result: ExecResult {
                status,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
                timed_out: false,
            },
        });
        self
    }

    /// Set the exit code `spawn_inherit` returns.
    pub fn with_inherit_exit(mut self, code: i32) -> Self {
        self.inherit_exit = code;
        self
    }

    /// Snapshot of the append-only audit log (every `run`/`spawn_inherit`).
    pub fn log(&self) -> Vec<Invocation> {
        self.audit.lock().unwrap().clone()
    }

    /// Convenience: did any invocation run `cmd` with these args as a prefix?
    /// (Used by the zero-keystroke assert: `ran("zmx", &["send"]) == false`.)
    pub fn ran(&self, cmd: &str, args_prefix: &[&str]) -> bool {
        self.log()
            .iter()
            .any(|inv| inv.cmd == cmd && starts_with(&inv.args, args_prefix))
    }

    fn record(&self, inv: Invocation) {
        self.audit.lock().unwrap().push(inv);
    }
}

fn starts_with(args: &[String], prefix: &[&str]) -> bool {
    args.len() >= prefix.len() && prefix.iter().zip(args).all(|(p, a)| *p == a.as_str())
}

impl Exec for ScriptedExec {
    fn run(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> io::Result<ExecResult> {
        self.record(Invocation {
            cmd: cmd.to_string(),
            args: args.to_vec(),
            env_overrides: env_overrides.to_vec(),
            cwd: cwd.map(Path::to_path_buf),
            timeout_ms,
            stdin: None,
        });
        for c in &self.canned {
            let prefix: Vec<&str> = c.args_prefix.iter().map(String::as_str).collect();
            if c.cmd == cmd && starts_with(args, &prefix) {
                return Ok(c.result.clone());
            }
        }
        // No canned match: benign empty success. Tests assert via the audit log.
        Ok(ExecResult {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        })
    }

    fn run_with_stdin(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
        stdin_bytes: &[u8],
    ) -> io::Result<ExecResult> {
        // Record with the stdin payload captured (the G-S2 hygiene assert reads
        // it back: header lives in `stdin`, never in `args`).
        self.record(Invocation {
            cmd: cmd.to_string(),
            args: args.to_vec(),
            env_overrides: env_overrides.to_vec(),
            cwd: cwd.map(Path::to_path_buf),
            timeout_ms,
            stdin: Some(String::from_utf8_lossy(stdin_bytes).into_owned()),
        });
        for c in &self.canned {
            let prefix: Vec<&str> = c.args_prefix.iter().map(String::as_str).collect();
            if c.cmd == cmd && starts_with(args, &prefix) {
                return Ok(c.result.clone());
            }
        }
        Ok(ExecResult {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        })
    }

    fn spawn_inherit(
        &self,
        cmd: &str,
        args: &[String],
        env_overrides: &[(String, String)],
        cwd: Option<&Path>,
    ) -> io::Result<i32> {
        self.record(Invocation {
            cmd: cmd.to_string(),
            args: args.to_vec(),
            env_overrides: env_overrides.to_vec(),
            cwd: cwd.map(Path::to_path_buf),
            timeout_ms: None,
            stdin: None,
        });
        Ok(self.inherit_exit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_canned_response_keyed_on_prefix() {
        let exec = ScriptedExec::new().on("zmx", &["list"], Some(0), "name=alpha", "");
        let r = exec
            .run(
                "zmx",
                &["list".to_string(), "--short".to_string()],
                &[],
                None,
                None,
            )
            .unwrap();
        assert_eq!(r.status, Some(0));
        assert_eq!(r.stdout, "name=alpha");
    }

    #[test]
    fn scripted_audit_log_is_append_only_and_inspectable() {
        let exec = ScriptedExec::new();
        exec.run("zmx", &["list".to_string()], &[], None, None)
            .unwrap();
        exec.run(
            "zmx",
            &["run".to_string(), "s".to_string()],
            &[("ZMX_DIR".to_string(), "/d".to_string())],
            None,
            None,
        )
        .unwrap();
        let log = exec.log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].args, vec!["run", "s"]);
        assert_eq!(
            log[1].env_overrides,
            vec![("ZMX_DIR".to_string(), "/d".to_string())]
        );
    }

    #[test]
    fn scripted_ran_detects_zero_keystrokes() {
        // The load-bearing assert: with no `zmx send` invocation, `ran` is false.
        let exec = ScriptedExec::new();
        exec.run(
            "zmx",
            &["run".to_string(), "s".to_string()],
            &[],
            None,
            None,
        )
        .unwrap();
        assert!(!exec.ran("zmx", &["send"]), "no send ran");
        exec.run(
            "zmx",
            &["send".to_string(), "s".to_string()],
            &[],
            None,
            None,
        )
        .unwrap();
        assert!(exec.ran("zmx", &["send"]), "send now recorded");
    }

    #[test]
    fn scripted_unmatched_run_is_benign_success() {
        let exec = ScriptedExec::new();
        let r = exec
            .run("zmx", &["history".to_string()], &[], None, None)
            .unwrap();
        assert_eq!(r.status, Some(0));
        assert!(r.stdout.is_empty());
    }

    #[test]
    fn scripted_spawn_inherit_logs_and_returns_canned_exit() {
        let exec = ScriptedExec::new().with_inherit_exit(42);
        let code = exec
            .spawn_inherit("zmx", &["attach".to_string(), "s".to_string()], &[], None)
            .unwrap();
        assert_eq!(code, 42);
        assert!(exec.ran("zmx", &["attach"]));
    }

    #[test]
    fn real_exec_runs_and_captures() {
        let exec = RealExec;
        let r = exec
            .run("/bin/echo", &["hi".to_string()], &[], None, None)
            .unwrap();
        assert_eq!(r.status, Some(0));
        assert_eq!(r.stdout.trim(), "hi");
        assert!(!r.timed_out);
    }

    #[test]
    fn real_exec_timeout_kills_wedged_child() {
        let exec = RealExec;
        // A 10s sleep with a 200ms deadline must be killed, not awaited.
        let start = std::time::Instant::now();
        let r = exec
            .run("/bin/sleep", &["10".to_string()], &[], None, Some(200))
            .unwrap();
        assert!(r.timed_out, "wedged child must report timed_out");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn real_exec_run_with_stdin_feeds_child() {
        // `cat` echoes its stdin to stdout — proves the stdin payload reaches the
        // child and the pipes drain without deadlock.
        let exec = RealExec;
        let r = exec
            .run_with_stdin("/bin/cat", &[], &[], None, None, b"hello stdin")
            .unwrap();
        assert_eq!(r.status, Some(0));
        assert_eq!(r.stdout, "hello stdin");
        assert!(!r.timed_out);
    }

    #[test]
    fn scripted_run_with_stdin_captures_payload_in_audit() {
        // The G-S2 hygiene shape: the payload is recorded in the audit log's
        // `stdin` field, NOT in `args`.
        let exec = ScriptedExec::new();
        exec.run_with_stdin(
            "curl",
            &["-sS".to_string(), "https://example".to_string()],
            &[],
            None,
            Some(120_000),
            b"header = \"Authorization: Bearer SECRET\"",
        )
        .unwrap();
        let log = exec.log();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0].stdin.as_deref(),
            Some("header = \"Authorization: Bearer SECRET\"")
        );
        // The secret never appears in argv.
        assert!(log[0].args.iter().all(|a| !a.contains("SECRET")));
    }

    #[test]
    fn real_exec_env_override_applied() {
        let exec = RealExec;
        let r = exec
            .run(
                "/bin/sh",
                &["-c".to_string(), "printf %s \"$ZMX_DIR\"".to_string()],
                &[("ZMX_DIR".to_string(), "/tmp/pinned".to_string())],
                None,
                None,
            )
            .unwrap();
        assert_eq!(r.stdout, "/tmp/pinned");
    }
}
