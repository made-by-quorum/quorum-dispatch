//! M1: trait Mux + MuxSession + FixtureMux (Stage-2 seam), TS src/session.ts:221-291.
//!
//! `MuxSession` mirrors the TS `ZmxSession` (session.ts:44-62). The `Mux` trait is
//! the Stage-2 seam moved forward from A2: `list` is the FILTERED view (drift never
//! surfaces as a healthy detached session), `list_raw` is the unfiltered reap input
//! reconcile needs. The drift filter is factored as a pure fn so the fixture impl
//! here and the future real `zmx list` impl in A2 share exactly one decider.

use crate::exec::ExecResult;
use crate::zmx_list::parse_zmx_output;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// Mirrors TS `ZmxSession` (src/session.ts:44-62). Field names are snake_case on
/// the Rust side; the TS camelCase (`startDir`, `socketDir`, `exitCode`,
/// `zmxStatus`) is a serialization concern owned by render.rs, not this struct.
#[derive(Debug, Clone, PartialEq)]
pub struct MuxSession {
    pub name: String,
    pub pid: i32,
    pub clients: u32,
    pub created: i64,
    pub start_dir: String,
    pub cmd: String,
    pub current: bool,
    /// Socket dir this session was FOUND in (Bug D: reads span multiple dirs,
    /// session.ts:52). Tagged post-parse by the scan, not present in the raw line.
    pub socket_dir: Option<String>,
    /// Unix ts when the task ended, if it has (zmx `ended=` field, session.ts:54).
    pub ended: Option<i64>,
    /// Exit code if the task ended (session.ts:56).
    pub exit_code: Option<i32>,
    /// zmx-reported status; "unreachable" means a dying/dead socket (session.ts:58).
    pub zmx_status: Option<String>,
    /// Present when zmx couldn't reach the session (dying/dead, session.ts:60).
    pub err: Option<String>,
}

/// Stage-2 mux seam (spec §3), extended in A2 with the drive verbs
/// (run/send/kill/history/wait/attach). EVERY method is pinned to a `socket_dir`
/// (Bug D / L1: reads and writes must agree on the dir).
pub trait Mux {
    /// `zmx list` for one socket dir, FILTERED: ended/unreachable/err tasks dropped
    /// (session.ts:221-240 — drift never surfaces as a healthy detached session,
    /// Bug D / I2).
    fn list(&self, socket_dir: &Path) -> io::Result<Vec<MuxSession>>;
    /// RAW list including ended/unreachable — reconcile's reap input
    /// (session.ts:242-260, Bug C / I3).
    fn list_raw(&self, socket_dir: &Path) -> io::Result<Vec<MuxSession>>;

    /// Detached run: `zmx run <name> -d bash -lc <shell_cmd>` pinned to
    /// `socket_dir`, in `cwd` (the zmx-exec core of TS `startDetached`,
    /// utils.ts:348-375). The session lands in the SAME dir reads/kills use
    /// (Bug D keystone). Returns the captured run result; the CALLER decides
    /// what a nonzero exit means (create.rs maps it to the missing/failed
    /// guidance — M2).
    fn run_detached(
        &self,
        socket_dir: &Path,
        name: &str,
        shell_cmd: &str,
        cwd: &Path,
    ) -> io::Result<ExecResult>;

    /// Raw PTY write: `zmx send <name> <text>` pinned to `socket_dir`. This is
    /// the fire-and-forget primitive ONLY (no CR appended, no acceptance check —
    /// submit discipline / queue-to-busy is A4's `deliverPrompt`, L4). Returns
    /// the captured result.
    fn send(&self, socket_dir: &Path, name: &str, text: &str) -> io::Result<ExecResult>;

    /// `zmx kill <name>` pinned to `socket_dir` (port of `zmxKill`,
    /// utils.ts:430-438). Returns the exit code.
    fn kill(&self, socket_dir: &Path, name: &str) -> io::Result<i32>;

    /// `zmx history <name>` pinned to `socket_dir` — the raw scrollback (used by
    /// the boot answerer's content-match, §8). Returns stdout verbatim.
    fn history(&self, socket_dir: &Path, name: &str) -> io::Result<String>;

    /// `zmx wait <name>...` pinned to `socket_dir` — block until the named
    /// detached task(s) complete. Raw passthrough (A2 scope §3); returns the exit
    /// code.
    fn wait(&self, socket_dir: &Path, names: &[String]) -> io::Result<i32>;

    /// `zmx attach <name>` pinned to `socket_dir` — an INTERACTIVE, stdio-inherit
    /// handoff (terminal takeover for `qd attach`). Returns the exit code.
    fn attach(&self, socket_dir: &Path, name: &str) -> io::Result<i32>;
}

/// The drift filter (session.ts:231-235), factored pure so the fixture and the
/// future real `zmx list` impl share ONE decider.
///
/// Drop tasks that have ended OR that zmx can't reach (a dying/dead socket,
/// surfaced briefly after `zmx kill` as `status=unreachable err=...`). These are
/// not attachable, so surfacing them would re-introduce the drift bug (Bug D/I2):
/// a "live process, dead/unattachable zmx" must never look like a healthy
/// detached session.
pub fn is_attachable(z: &MuxSession) -> bool {
    // !z.ended && z.zmxStatus !== "unreachable" && !z.err
    z.ended.is_none() && z.zmx_status.as_deref() != Some("unreachable") && z.err.is_none()
}

/// Cross-dir scan dedupe (session.ts:273-291): list across the canonical socket
/// dir AND any legacy dirs, deduped by name with the FIRST dir (canonical) winning.
/// This closes the keystone read/write socket-dir split (Bug D): a session created
/// in one TMPDIR context is visible+killable from another, and a legacy dup never
/// shadows the canonical task.
///
/// Pure over the already-scanned `(dir, sessions)` pairs; the I/O (which dirs, in
/// what order) is the caller's. Input order IS the precedence order — pass
/// `[canonical, ..legacy]`.
pub fn merge_canonical_wins(scans: Vec<(PathBuf, Vec<MuxSession>)>) -> Vec<MuxSession> {
    let mut by_name: Vec<MuxSession> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (_dir, sessions) in scans {
        for z in sessions {
            // First dir (canonical) wins; don't let a legacy dup shadow it.
            if seen.insert(z.name.clone()) {
                by_name.push(z);
            }
        }
    }
    by_name
}

/// One recorded drive-verb call against a [`FixtureMux`] (the offline analogue
/// of [`crate::exec::Invocation`]): which verb, which dir, which session, payload.
#[derive(Debug, Clone, PartialEq)]
pub struct MuxCall {
    pub verb: &'static str,
    pub socket_dir: PathBuf,
    pub name: String,
    /// `send` text / `wait` names joined; empty otherwise.
    pub payload: String,
}

/// Fixture mux: maps a socket dir → canned raw `zmx list` text. The list methods
/// run [`parse_zmx_output`] then tag each session's `socket_dir` (session.ts:236);
/// `list` additionally applies [`is_attachable`]. A missing dir yields `Ok(vec![])`
/// (TS `catch → []`, session.ts:237-239) — never an error.
///
/// The drive verbs (run/send/kill/history/wait/attach) record into an append-only
/// `calls` log + return canned values, so A2 create-path units run OFFLINE and a
/// test can assert e.g. "no send was ever issued" (the zero-keystroke contract).
#[derive(Default)]
pub struct FixtureMux {
    pub dirs: HashMap<PathBuf, String>,
    /// Canned `zmx history <name>` per (dir-string, name); empty otherwise.
    history: HashMap<(String, String), String>,
    /// Append-only drive-verb log.
    calls: RefCell<Vec<MuxCall>>,
    /// Exit code returned by `kill`/`wait`/`attach` (default 0).
    verb_exit: i32,
    /// When set, `list`/`list_raw` FAIL with this errno instead of serving the
    /// canned dirs — the refused mux read a sandboxed host produces. `None`
    /// (the default) leaves every existing fixture byte-identical.
    list_errno: Option<i32>,
}

impl FixtureMux {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register canned `zmx list` output for a socket dir.
    pub fn with_dir(mut self, dir: impl Into<PathBuf>, raw_list: impl Into<String>) -> Self {
        self.dirs.insert(dir.into(), raw_list.into());
        self
    }

    /// Register canned `zmx history <name>` output for a (dir, name).
    pub fn with_history(
        mut self,
        dir: impl Into<PathBuf>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        let dir: PathBuf = dir.into();
        self.history.insert(
            (dir.to_string_lossy().into_owned(), name.into()),
            text.into(),
        );
        self
    }

    /// Set the exit code the kill/wait/attach verbs return.
    pub fn with_verb_exit(mut self, code: i32) -> Self {
        self.verb_exit = code;
        self
    }

    /// Make `list`/`list_raw` fail with `EPERM`, as under a sandbox that denies
    /// the mux socket read. Distinct from an EMPTY list, which is the fixture's
    /// way of saying "read fine, no panes".
    pub fn with_denied_list(mut self) -> Self {
        self.list_errno = Some(libc::EPERM);
        self
    }

    /// Snapshot of the append-only drive-verb log.
    pub fn calls(&self) -> Vec<MuxCall> {
        self.calls.borrow().clone()
    }

    fn scan(&self, socket_dir: &Path) -> Vec<MuxSession> {
        let Some(text) = self.dirs.get(socket_dir) else {
            // Missing dir → [] (TS catch path).
            return Vec::new();
        };
        let dir_str = socket_dir.to_string_lossy().into_owned();
        parse_zmx_output(text)
            .into_iter()
            .map(|mut z| {
                z.socket_dir = Some(dir_str.clone());
                z
            })
            .collect()
    }

    fn record(&self, verb: &'static str, socket_dir: &Path, name: &str, payload: String) {
        self.calls.borrow_mut().push(MuxCall {
            verb,
            socket_dir: socket_dir.to_path_buf(),
            name: name.to_string(),
            payload,
        });
    }
}

impl Mux for FixtureMux {
    fn list(&self, socket_dir: &Path) -> io::Result<Vec<MuxSession>> {
        if let Some(e) = self.list_errno {
            return Err(io::Error::from_raw_os_error(e));
        }
        Ok(self
            .scan(socket_dir)
            .into_iter()
            .filter(is_attachable)
            .collect())
    }

    fn list_raw(&self, socket_dir: &Path) -> io::Result<Vec<MuxSession>> {
        if let Some(e) = self.list_errno {
            return Err(io::Error::from_raw_os_error(e));
        }
        Ok(self.scan(socket_dir))
    }

    fn run_detached(
        &self,
        socket_dir: &Path,
        name: &str,
        shell_cmd: &str,
        _cwd: &Path,
    ) -> io::Result<ExecResult> {
        self.record("run_detached", socket_dir, name, shell_cmd.to_string());
        Ok(ExecResult {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        })
    }

    fn send(&self, socket_dir: &Path, name: &str, text: &str) -> io::Result<ExecResult> {
        self.record("send", socket_dir, name, text.to_string());
        Ok(ExecResult {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        })
    }

    fn kill(&self, socket_dir: &Path, name: &str) -> io::Result<i32> {
        self.record("kill", socket_dir, name, String::new());
        Ok(self.verb_exit)
    }

    fn history(&self, socket_dir: &Path, name: &str) -> io::Result<String> {
        self.record("history", socket_dir, name, String::new());
        let key = (socket_dir.to_string_lossy().into_owned(), name.to_string());
        Ok(self.history.get(&key).cloned().unwrap_or_default())
    }

    fn wait(&self, socket_dir: &Path, names: &[String]) -> io::Result<i32> {
        let first = names.first().cloned().unwrap_or_default();
        self.record("wait", socket_dir, &first, names.join(" "));
        Ok(self.verb_exit)
    }

    fn attach(&self, socket_dir: &Path, name: &str) -> io::Result<i32> {
        self.record("attach", socket_dir, name, String::new());
        Ok(self.verb_exit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(name: &str) -> MuxSession {
        MuxSession {
            name: name.to_string(),
            pid: 100,
            clients: 0,
            created: 0,
            start_dir: String::new(),
            cmd: String::new(),
            current: false,
            socket_dir: None,
            ended: None,
            exit_code: None,
            zmx_status: None,
            err: None,
        }
    }

    // A realistic two-session canned listing: one healthy, one ended.
    const CANONICAL_LIST: &str = "\
name=alpha\tpid=111\tclients=1\tcreated=1700000000\tstart_dir=/home/x\tcmd=claude
name=beta\tpid=222\tclients=0\tcreated=1700000100\tstart_dir=/home/y\tcmd=claude\tended=1700000200\texit_code=0
name=gamma\tpid=333\tclients=0\tcreated=1700000300\tstart_dir=/home/z\tcmd=claude\tstatus=unreachable\terr=socket gone
";

    #[test]
    fn is_attachable_drops_ended_unreachable_err() {
        let mut ended = mk("e");
        ended.ended = Some(123);
        assert!(!is_attachable(&ended));

        let mut unreachable = mk("u");
        unreachable.zmx_status = Some("unreachable".to_string());
        assert!(!is_attachable(&unreachable));

        let mut errd = mk("x");
        errd.err = Some("boom".to_string());
        assert!(!is_attachable(&errd));

        // A healthy detached task (clients=0 but no ended/err/unreachable) stays.
        let healthy = mk("h");
        assert!(is_attachable(&healthy));
    }

    #[test]
    fn fixture_list_filters_raw_does_not() {
        let dir = PathBuf::from("/tmp/zmx-501");
        let mux = FixtureMux::new().with_dir(dir.clone(), CANONICAL_LIST);

        // list (filtered): only the healthy "alpha" survives.
        let filtered = mux.list(&dir).unwrap();
        assert_eq!(
            filtered.iter().map(|z| z.name.as_str()).collect::<Vec<_>>(),
            ["alpha"]
        );
        // socket_dir tagging (session.ts:236).
        assert_eq!(filtered[0].socket_dir.as_deref(), Some("/tmp/zmx-501"));

        // list_raw (unfiltered): all three, INCLUDING ended/unreachable.
        let raw = mux.list_raw(&dir).unwrap();
        assert_eq!(
            raw.iter().map(|z| z.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta", "gamma"]
        );
        assert_eq!(raw[2].socket_dir.as_deref(), Some("/tmp/zmx-501"));
    }

    #[test]
    fn fixture_missing_dir_is_empty_not_error() {
        let mux = FixtureMux::new();
        // TS: catch → [] (session.ts:237-239). Not an Err.
        assert_eq!(mux.list(Path::new("/tmp/nope")).unwrap(), Vec::new());
        assert_eq!(mux.list_raw(Path::new("/tmp/nope")).unwrap(), Vec::new());
    }

    #[test]
    fn canonical_wins_dedupe() {
        let canonical = PathBuf::from("/tmp/claude-501/zmx-501");
        let legacy = PathBuf::from("/tmp/zmx-501");

        // Both dirs have a "dup" task; canonical's pid 111 must win over legacy 999.
        let mut canon_dup = mk("dup");
        canon_dup.pid = 111;
        canon_dup.socket_dir = Some(canonical.to_string_lossy().into_owned());
        let canon_only = {
            let mut s = mk("canon-only");
            s.socket_dir = Some(canonical.to_string_lossy().into_owned());
            s
        };

        let mut legacy_dup = mk("dup");
        legacy_dup.pid = 999;
        legacy_dup.socket_dir = Some(legacy.to_string_lossy().into_owned());
        let legacy_only = {
            let mut s = mk("legacy-only");
            s.socket_dir = Some(legacy.to_string_lossy().into_owned());
            s
        };

        // Order is precedence: canonical FIRST.
        let merged = merge_canonical_wins(vec![
            (canonical, vec![canon_dup, canon_only]),
            (legacy, vec![legacy_dup, legacy_only]),
        ]);

        let dup = merged.iter().find(|z| z.name == "dup").unwrap();
        assert_eq!(dup.pid, 111, "canonical task wins the name");
        // Both unique tasks present; legacy dup dropped.
        let names: Vec<_> = merged.iter().map(|z| z.name.as_str()).collect();
        assert_eq!(names, ["dup", "canon-only", "legacy-only"]);
    }

    #[test]
    fn fixture_drive_verbs_log_and_return_canned() {
        let dir = PathBuf::from("/tmp/zmx-501");
        let mux = FixtureMux::new()
            .with_history(dir.clone(), "s", "scrollback here")
            .with_verb_exit(0);

        mux.run_detached(&dir, "s", "command 'claude'", Path::new("/cwd"))
            .unwrap();
        mux.send(&dir, "s", "\r").unwrap();
        assert_eq!(mux.history(&dir, "s").unwrap(), "scrollback here");
        assert_eq!(mux.kill(&dir, "s").unwrap(), 0);
        assert_eq!(mux.wait(&dir, &["s".to_string()]).unwrap(), 0);
        assert_eq!(mux.attach(&dir, "s").unwrap(), 0);

        let calls = mux.calls();
        let verbs: Vec<&str> = calls.iter().map(|c| c.verb).collect();
        assert_eq!(
            verbs,
            ["run_detached", "send", "history", "kill", "wait", "attach"]
        );
        // send payload recorded so a test can assert WHAT keystroke went out.
        let send = calls.iter().find(|c| c.verb == "send").unwrap();
        assert_eq!(send.payload, "\r");
        assert_eq!(send.socket_dir, dir);
    }

    #[test]
    fn fixture_zero_keystroke_assert() {
        // The load-bearing contract: with no send issued, the calls log shows no
        // "send" verb (M3/M4 dialog-free boot asserts this against a real boot).
        let dir = PathBuf::from("/tmp/zmx-501");
        let mux = FixtureMux::new();
        mux.run_detached(&dir, "s", "cmd", Path::new("/cwd"))
            .unwrap();
        assert!(!mux.calls().iter().any(|c| c.verb == "send"));
    }

    #[test]
    fn fixture_history_missing_is_empty() {
        let dir = PathBuf::from("/tmp/zmx-501");
        let mux = FixtureMux::new();
        assert_eq!(mux.history(&dir, "absent").unwrap(), "");
    }
}
