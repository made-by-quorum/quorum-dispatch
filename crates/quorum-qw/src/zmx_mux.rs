//! Real `Mux` impl over pinned zmx 0.6.0 (spec §4, deliverable 4).
//!
//! EVERY zmx invocation is env-pinned with `ZMX_DIR=<socket_dir>` (port of
//! `zmxEnvForDir`, utils.ts:129-135). This is the WRITE side of the Bug-D / L1
//! keystone: reads (`zmx list`) and writes (`zmx run`/`send`/`kill`) must agree
//! on the socket dir, or a session created in one TMPDIR context becomes a "live
//! process, dead/unattachable zmx" no caller can reach. Pinning ZMX_DIR per call
//! forces them to agree.

use crate::exec::{Exec, ExecResult};
use crate::mux::{is_attachable, Mux, MuxSession};
use crate::zmx_list::parse_zmx_output;
use std::io;
use std::path::Path;

/// The real mux. Generic over the [`Exec`] seam so tests drive it with a
/// `ScriptedExec` (no live zmx) while production wires a `RealExec`.
pub struct ZmxMux<E: Exec> {
    exec: E,
}

impl<E: Exec> ZmxMux<E> {
    pub fn new(exec: E) -> Self {
        Self { exec }
    }

    /// Build the `ZMX_DIR=<socket_dir>` env override carried by EVERY zmx call
    /// (port of `zmxEnvForDir`, utils.ts:133-135). The single place the write-side
    /// pin is applied.
    fn env_for_dir(socket_dir: &Path) -> Vec<(String, String)> {
        vec![(
            "ZMX_DIR".to_string(),
            socket_dir.to_string_lossy().into_owned(),
        )]
    }

    /// Run `zmx <args>` pinned to `socket_dir`, capturing output.
    fn zmx(
        &self,
        socket_dir: &Path,
        args: &[String],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> io::Result<ExecResult> {
        self.exec
            .run("zmx", args, &Self::env_for_dir(socket_dir), cwd, timeout_ms)
    }

    /// Parse a `zmx list` blob into sessions tagged with `socket_dir`. Shared by
    /// `list`/`list_raw` (the filter is the only difference). A zmx error /
    /// missing dir surfaces as an empty stdout → empty vec, matching TS
    /// `catch → []` (session.ts:237-239): a nonexistent dir is NOT an error.
    fn scan(&self, socket_dir: &Path) -> Vec<MuxSession> {
        // `zmx list` against a nonexistent dir exits nonzero / prints nothing; we
        // do not propagate that as an Err. parse_zmx_output of "" → [] (TS parity).
        let out = match self.zmx(socket_dir, &["list".to_string()], None, None) {
            Ok(r) => r.stdout,
            // Even a spawn-level error degrades to [] here — list must never crash
            // `qd ls` when a legacy dir is gone (L8 clean-on-corruption posture).
            Err(_) => String::new(),
        };
        let dir_str = socket_dir.to_string_lossy().into_owned();
        parse_zmx_output(&out)
            .into_iter()
            .map(|mut z| {
                z.socket_dir = Some(dir_str.clone());
                z
            })
            .collect()
    }
}

impl<E: Exec> Mux for ZmxMux<E> {
    fn list(&self, socket_dir: &Path) -> io::Result<Vec<MuxSession>> {
        Ok(self
            .scan(socket_dir)
            .into_iter()
            .filter(is_attachable)
            .collect())
    }

    fn list_raw(&self, socket_dir: &Path) -> io::Result<Vec<MuxSession>> {
        Ok(self.scan(socket_dir))
    }

    fn run_detached(
        &self,
        socket_dir: &Path,
        name: &str,
        shell_cmd: &str,
        cwd: &Path,
    ) -> io::Result<ExecResult> {
        // `zmx run <name> -d bash -lc <shell_cmd>` (utils.ts:350). `-d` detaches;
        // `bash -lc` runs the built claude command (a single shell-quoted string).
        let args = vec![
            "run".to_string(),
            name.to_string(),
            "-d".to_string(),
            "bash".to_string(),
            "-lc".to_string(),
            shell_cmd.to_string(),
        ];
        self.zmx(socket_dir, &args, Some(cwd), None)
    }

    fn send(&self, socket_dir: &Path, name: &str, text: &str) -> io::Result<ExecResult> {
        // `zmx send <name> <text>` — raw fire-and-forget PTY write (no CR appended;
        // submit discipline is A4, L4).
        let args = vec!["send".to_string(), name.to_string(), text.to_string()];
        self.zmx(socket_dir, &args, None, None)
    }

    fn kill(&self, socket_dir: &Path, name: &str) -> io::Result<i32> {
        // Port of `zmxKill` (utils.ts:430-438): `zmx kill <name>`, dir-pinned.
        let args = vec!["kill".to_string(), name.to_string()];
        let r = self.zmx(socket_dir, &args, None, None)?;
        // TS `proc.exitCode ?? 1` (utils.ts:437).
        Ok(r.status.unwrap_or(1))
    }

    fn history(&self, socket_dir: &Path, name: &str) -> io::Result<String> {
        // `zmx history <name>` — raw scrollback (the boot answerer content-matches
        // its tail, §8).
        let args = vec!["history".to_string(), name.to_string()];
        Ok(self.zmx(socket_dir, &args, None, None)?.stdout)
    }

    fn wait(&self, socket_dir: &Path, names: &[String]) -> io::Result<i32> {
        // `zmx wait <name>...` — raw passthrough (A2 scope §3).
        let mut args = vec!["wait".to_string()];
        args.extend(names.iter().cloned());
        let r = self.zmx(socket_dir, &args, None, None)?;
        Ok(r.status.unwrap_or(1))
    }

    fn attach(&self, socket_dir: &Path, name: &str) -> io::Result<i32> {
        // `zmx attach <name>` — INTERACTIVE handoff: stdio inherited so the user
        // takes over the terminal. Pinned to socket_dir (Bug D cross-dir attach).
        let args = vec!["attach".to_string(), name.to_string()];
        self.exec
            .spawn_inherit("zmx", &args, &Self::env_for_dir(socket_dir), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ScriptedExec;
    use std::path::PathBuf;

    const LIST: &str = "name=alpha\tpid=111\tclients=1\tcreated=1700000000\tstart_dir=/h\tcmd=claude
name=beta\tpid=222\tclients=0\tcreated=1700000100\tstart_dir=/h\tcmd=claude\tended=1700000200\texit_code=0
";

    fn dir() -> PathBuf {
        PathBuf::from("/tmp/claude-501/zmx-501")
    }

    /// Assert the single recorded invocation carries ZMX_DIR=<dir>.
    fn assert_pinned(exec: &ScriptedExec, idx: usize, socket_dir: &Path) {
        let inv = &exec.log()[idx];
        assert!(
            inv.env_overrides
                .iter()
                .any(|(k, v)| k == "ZMX_DIR" && v == &socket_dir.to_string_lossy()),
            "invocation {idx} must pin ZMX_DIR={}: {:?}",
            socket_dir.display(),
            inv.env_overrides
        );
    }

    #[test]
    fn list_parses_filters_and_tags_socket_dir() {
        let exec = ScriptedExec::new().on("zmx", &["list"], Some(0), LIST, "");
        let mux = ZmxMux::new(exec);
        let live = mux.list(&dir()).unwrap();
        // beta is ended → filtered; alpha survives, tagged with socket_dir.
        assert_eq!(
            live.iter().map(|z| z.name.as_str()).collect::<Vec<_>>(),
            ["alpha"]
        );
        assert_eq!(
            live[0].socket_dir.as_deref(),
            Some(dir().to_string_lossy().as_ref())
        );
        // raw keeps both.
        let raw = mux.list_raw(&dir()).unwrap();
        assert_eq!(raw.len(), 2);
    }

    #[test]
    fn list_missing_dir_is_empty_not_error() {
        // zmx errors (nonzero, empty stdout) on a nonexistent dir → Ok([]) (TS catch→[]).
        let exec = ScriptedExec::new().on("zmx", &["list"], Some(1), "", "no such dir");
        let mux = ZmxMux::new(exec);
        assert_eq!(mux.list(&dir()).unwrap(), Vec::new());
        assert_eq!(mux.list_raw(&dir()).unwrap(), Vec::new());
    }

    #[test]
    fn every_verb_pins_zmx_dir_and_uses_right_argv() {
        let exec = ScriptedExec::new();
        let d = dir();
        let mux = ZmxMux::new(exec);

        mux.run_detached(&d, "s", "command 'claude'", Path::new("/cwd"))
            .unwrap();
        mux.send(&d, "s", "\r").unwrap();
        mux.kill(&d, "s").unwrap();
        mux.history(&d, "s").unwrap();
        mux.wait(&d, &["s".to_string(), "t".to_string()]).unwrap();
        mux.list(&d).unwrap();
        mux.attach(&d, "s").unwrap();

        let exec = &mux.exec;
        let log = exec.log();
        assert_eq!(
            log[0].args,
            vec!["run", "s", "-d", "bash", "-lc", "command 'claude'"]
        );
        assert_eq!(log[0].cwd.as_deref(), Some(Path::new("/cwd")));
        assert_eq!(log[1].args, vec!["send", "s", "\r"]);
        assert_eq!(log[2].args, vec!["kill", "s"]);
        assert_eq!(log[3].args, vec!["history", "s"]);
        assert_eq!(log[4].args, vec!["wait", "s", "t"]);
        assert_eq!(log[5].args, vec!["list"]);
        assert_eq!(log[6].args, vec!["attach", "s"]);
        // L1: EVERY one carries the dir pin.
        for i in 0..log.len() {
            assert_pinned(exec, i, &d);
        }
    }

    #[test]
    fn kill_returns_exit_code() {
        let exec = ScriptedExec::new().on("zmx", &["kill"], Some(0), "", "");
        let mux = ZmxMux::new(exec);
        assert_eq!(mux.kill(&dir(), "s").unwrap(), 0);
    }

    #[test]
    fn history_returns_stdout_verbatim() {
        let exec = ScriptedExec::new().on("zmx", &["history"], Some(0), "line1\nline2\n", "");
        let mux = ZmxMux::new(exec);
        assert_eq!(mux.history(&dir(), "s").unwrap(), "line1\nline2\n");
    }
}
