//! The ONE place the home→state-layout mapping lives.
//!
//! TS qd derives its CLAUDE state dirs from `homedir()` (src/session.ts:6-8:
//! `const HOME = homedir(); SESSIONS_DIR = ~/.claude/sessions; PROJECTS_DIR =
//! ~/.claude/projects`; RELAY_DIR = ~/.claude/relay, src/session.ts:150) — these
//! derive from HOME ONLY. The qd DATA root is separate and QD_HOME-overridable:
//! `qdHome = env.QD_HOME || join(HOME, ".quorum", "dispatch")`, and the hot-state dir is
//! `<qdHome>/state` (bootstrap.ts:88-96 `resolveBootstrapPaths` +
//! BootstrapPaths.stateDir doc, bootstrap.ts:~54). `marks.jsonl` (A3 ADD-3)
//! lives in that state dir.
//!
//! Lesson L9a / ADD-4: **HOME is load-bearing.** A jail that overrides
//! QD_HOME/ZMX_DIR/XDG_*/TMPDIR but not HOME still sees — and could mutate —
//! the org's REAL registry (found during 0b dry-run setup; read-only exposure,
//! fixed before any kill/gc/send). In Rust the same rule holds structurally:
//! NOTHING in this crate resolves the real home OR reads QD_HOME directly;
//! everything takes an `QdPaths` built from an injected home + the injected `Env`
//! seam. Tests inject temp dirs, period.

use std::path::{Path, PathBuf};

use crate::effects::Env;

/// Resolved state-dir layout for one home.
#[derive(Debug, Clone, PartialEq)]
pub struct QdPaths {
    pub home: PathBuf,
    /// PID registry: `<home>/.claude/sessions/<pid>.json` (+ `.tombstoned`).
    pub sessions_dir: PathBuf,
    /// Transcripts: `<home>/.claude/projects/<cwd-slug>/<uuid>.jsonl`.
    pub projects_dir: PathBuf,
    /// Relay sidecars: `<home>/.claude/relay/<x>.json`.
    pub relay_dir: PathBuf,
    /// Relay message inbox: `<home>/.claude/channels/relay/inbox`.
    ///
    /// Persisted incoming messages for the relay server (server.ts:65
    /// `join(homedir(), '.claude', 'channels', 'relay', 'inbox')`). Distinct
    /// from `relay_dir` which holds sidecar files. P-C2.
    pub inbox_dir: PathBuf,
    /// qd hot-state dir, `<qdHome>/state` where `qdHome = QD_HOME || <home>/.quorum/dispatch`
    /// (bootstrap.ts:88-96). Holds `marks.jsonl` (ADD-3). QD_HOME comes through
    /// the injected `Env` seam (L9a), never raw `std::env`.
    pub state_dir: PathBuf,
    /// The resolved qd data root = `qdHome` (the PARENT of `state_dir`; today
    /// `state_dir = <dispatch_root>/state`). This is the home of the qd–qf
    /// transport files (`log.jsonl` / `dispositions.jsonl` / `ls.json` /
    /// `remote/<host>/*`) which live directly under `qd_home`, NOT under
    /// `state/` (dispatch-transport-formats.md "common framing"). Same QD_HOME
    /// resolution as `state_dir`, through the injected `Env` seam (L9a).
    pub dispatch_root: PathBuf,
}

impl QdPaths {
    /// Mirror of src/session.ts:6-8 + :150 for the `.claude` dirs, plus the qd
    /// DATA `state_dir`. With NO injected env this assumes QD_HOME is unset, so
    /// `state_dir = <home>/.quorum/dispatch/state` (the default). Callers that must honor an
    /// QD_HOME override use [`QdPaths::from_home_env`].
    pub fn from_home(home: &Path) -> Self {
        Self::build(home, home.join(".quorum").join("dispatch"))
    }

    /// As [`from_home`], but resolves the qd data root via the injected `Env`
    /// seam: `qdHome = QD_HOME || <home>/.quorum/dispatch` (bootstrap.ts:88-96), so `state_dir`
    /// honors an QD_HOME override (H4). QD_HOME is read ONLY through `env` (L9a),
    /// never raw `std::env`. The `.claude` dirs are unchanged (HOME-only).
    pub fn from_home_env(home: &Path, env: &dyn Env) -> Self {
        let qd_home = env
            .var("QD_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".quorum").join("dispatch"));
        Self::build(home, qd_home)
    }

    /// Shared constructor: the `.claude` dirs derive from `home`; `state_dir`
    /// and `dispatch_root` derive from the already-resolved `qd_home`.
    fn build(home: &Path, qd_home: PathBuf) -> Self {
        let claude = home.join(".claude");
        QdPaths {
            home: home.to_path_buf(),
            sessions_dir: claude.join("sessions"),
            projects_dir: claude.join("projects"),
            relay_dir: claude.join("relay"),
            // server.ts:65: join(homedir(), '.claude', 'channels', 'relay', 'inbox').
            // DISTINCT from relay_dir (which is `.claude/relay`). P-C2.
            inbox_dir: claude.join("channels").join("relay").join("inbox"),
            state_dir: qd_home.join("state"),
            // The qd data root itself (parent of state_dir): the qd–qf transport
            // files live directly here, not under state/.
            dispatch_root: qd_home,
        }
    }

    // ---- qd–qf transport paths (directly under `dispatch_root` = qd_home,
    // NOT under state/; see dispatch-transport-formats.md "common framing"). ----

    /// `<root>/log.jsonl` — qd's event source (envelopes qd originated, §1).
    pub fn log_path(&self) -> PathBuf {
        self.dispatch_root.join("log.jsonl")
    }

    /// `<root>/dispositions.jsonl` — witnessed terminal facts, stored (§2).
    pub fn dispositions_path(&self) -> PathBuf {
        self.dispatch_root.join("dispositions.jsonl")
    }

    /// `<root>/log.archive.jsonl` — the log archive tier (v2 tiering; born
    /// absent, additive).
    pub fn log_archive_path(&self) -> PathBuf {
        self.dispatch_root.join("log.archive.jsonl")
    }

    /// `<root>/dispositions.archive.jsonl` — the dispositions archive tier (v2).
    pub fn dispositions_archive_path(&self) -> PathBuf {
        self.dispatch_root.join("dispositions.archive.jsonl")
    }

    /// `<root>/ls.json` — own session snapshot, published for peers.
    pub fn ls_path(&self) -> PathBuf {
        self.dispatch_root.join("ls.json")
    }

    /// `<root>/remote` — the directory of peers' mover-written replicas.
    pub fn remote_dir(&self) -> PathBuf {
        self.dispatch_root.join("remote")
    }

    /// `<root>/remote/<host>/log.jsonl` — a peer's replicated log (mover-written).
    pub fn remote_log_path(&self, host: &str) -> PathBuf {
        self.remote_dir().join(host).join("log.jsonl")
    }

    /// `<root>/remote/<host>/dispositions.jsonl` — a peer's replicated
    /// dispositions (mover-written).
    pub fn remote_dispositions_path(&self, host: &str) -> PathBuf {
        self.remote_dir().join(host).join("dispositions.jsonl")
    }

    /// `<root>/remote/<host>/ls.json` — a peer's session snapshot (mover-written).
    pub fn remote_ls_path(&self, host: &str) -> PathBuf {
        self.remote_dir().join(host).join("ls.json")
    }
}

/// Whether `host` is a safe bare hostname to interpolate into a `remote/<host>/`
/// path (audit #4 — path-traversal defense). A caller-supplied `--host` value
/// flows into [`QdPaths::remote_log_path`] / `remote_dispositions_path` /
/// `remote_ls_path`, which plain-`join` it under `remote/`. Without this check a
/// hostile value escapes the store root:
///   - `..` (or a segment containing it) walks UP out of `remote/`;
///   - an ABSOLUTE value (`/etc`, or a leading `/`) makes `Path::join` DISCARD the
///     base entirely and read that absolute dir;
///   - an embedded `/` addresses a nested/sibling path, not one host dir;
///   - empty is not a host.
/// A real host id is a single opaque segment (the v1 placeholder is `"local"`;
/// future host-identity ids are still single segments). We therefore require a
/// NON-EMPTY value with NO path separators and NO `..` component — the minimal
/// rule that confines every `remote/<host>/` read to one direct child of
/// `remote/`. Callers reject an invalid host with a named refusal; the store also
/// re-checks at the read seam (defense in depth) so no path is ever joined from
/// an unvalidated host.
pub fn is_valid_hostname(host: &str) -> bool {
    !host.is_empty()
        // No path separators (forward slash on unix; also reject backslash
        // defensively) — a host is ONE segment, never a nested path.
        && !host.contains('/')
        && !host.contains('\\')
        // No `..` component (walks up out of remote/). Reject the bare `..` and
        // any `.`-only segment; a legitimate host id never needs them.
        && host != ".."
        && host != "."
        // NUL is never valid in a path component.
        && !host.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;

    #[test]
    fn layout_mirrors_ts() {
        let p = QdPaths::from_home(Path::new("/jail/home"));
        assert_eq!(p.sessions_dir, Path::new("/jail/home/.claude/sessions"));
        assert_eq!(p.projects_dir, Path::new("/jail/home/.claude/projects"));
        assert_eq!(p.relay_dir, Path::new("/jail/home/.claude/relay"));
        // inbox_dir is DISTINCT from relay_dir (server.ts:65). P-C2.
        assert_eq!(
            p.inbox_dir,
            Path::new("/jail/home/.claude/channels/relay/inbox")
        );
        // QD_HOME unset default: <home>/.quorum/dispatch/state.
        assert_eq!(p.state_dir, Path::new("/jail/home/.quorum/dispatch/state"));
        // dispatch_root = qd_home = parent of state_dir (default).
        assert_eq!(p.dispatch_root, Path::new("/jail/home/.quorum/dispatch"));
        // qd–qf transport files live directly under dispatch_root, NOT state/.
        assert_eq!(
            p.log_path(),
            Path::new("/jail/home/.quorum/dispatch/log.jsonl")
        );
        assert_eq!(
            p.dispositions_path(),
            Path::new("/jail/home/.quorum/dispatch/dispositions.jsonl")
        );
        assert_eq!(
            p.log_archive_path(),
            Path::new("/jail/home/.quorum/dispatch/log.archive.jsonl")
        );
        assert_eq!(
            p.dispositions_archive_path(),
            Path::new("/jail/home/.quorum/dispatch/dispositions.archive.jsonl")
        );
        assert_eq!(
            p.ls_path(),
            Path::new("/jail/home/.quorum/dispatch/ls.json")
        );
        assert_eq!(
            p.remote_dir(),
            Path::new("/jail/home/.quorum/dispatch/remote")
        );
        assert_eq!(
            p.remote_log_path("peerbox"),
            Path::new("/jail/home/.quorum/dispatch/remote/peerbox/log.jsonl")
        );
        assert_eq!(
            p.remote_dispositions_path("peerbox"),
            Path::new("/jail/home/.quorum/dispatch/remote/peerbox/dispositions.jsonl")
        );
        assert_eq!(
            p.remote_ls_path("peerbox"),
            Path::new("/jail/home/.quorum/dispatch/remote/peerbox/ls.json")
        );
    }

    #[test]
    fn from_home_env_default_state_dir() {
        // No QD_HOME → <home>/.quorum/dispatch/state (bootstrap.ts:88-96 default).
        let env = MapEnv::default();
        let p = QdPaths::from_home_env(Path::new("/jail/home"), &env);
        assert_eq!(p.state_dir, Path::new("/jail/home/.quorum/dispatch/state"));
    }

    #[test]
    fn from_home_env_honors_qd_home_override() {
        // QD_HOME set → <QD_HOME>/state, NOT <home>/.quorum/dispatch/state.
        let mut env = MapEnv::default();
        env.vars
            .insert("QD_HOME".to_string(), "/elsewhere/qddata".to_string());
        let p = QdPaths::from_home_env(Path::new("/jail/home"), &env);
        assert_eq!(p.state_dir, Path::new("/elsewhere/qddata/state"));
        // The .claude dirs remain HOME-derived (NOT under QD_HOME).
        assert_eq!(p.sessions_dir, Path::new("/jail/home/.claude/sessions"));
        // dispatch_root + ALL transport files root at <QD_HOME>, NOT
        // <home>/.quorum/dispatch. This is the L9a discipline: the override
        // fully relocates the qd data root.
        assert_eq!(p.dispatch_root, Path::new("/elsewhere/qddata"));
        assert_eq!(p.log_path(), Path::new("/elsewhere/qddata/log.jsonl"));
        assert_eq!(
            p.dispositions_path(),
            Path::new("/elsewhere/qddata/dispositions.jsonl")
        );
        assert_eq!(
            p.log_archive_path(),
            Path::new("/elsewhere/qddata/log.archive.jsonl")
        );
        assert_eq!(
            p.dispositions_archive_path(),
            Path::new("/elsewhere/qddata/dispositions.archive.jsonl")
        );
        assert_eq!(p.ls_path(), Path::new("/elsewhere/qddata/ls.json"));
        assert_eq!(p.remote_dir(), Path::new("/elsewhere/qddata/remote"));
        assert_eq!(
            p.remote_log_path("peerbox"),
            Path::new("/elsewhere/qddata/remote/peerbox/log.jsonl")
        );
        assert_eq!(
            p.remote_dispositions_path("peerbox"),
            Path::new("/elsewhere/qddata/remote/peerbox/dispositions.jsonl")
        );
        assert_eq!(
            p.remote_ls_path("peerbox"),
            Path::new("/elsewhere/qddata/remote/peerbox/ls.json")
        );
    }

    #[test]
    fn empty_qd_home_falls_back_to_default() {
        // QD_HOME="" is falsy → default (JS `||` semantics).
        let mut env = MapEnv::default();
        env.vars.insert("QD_HOME".to_string(), String::new());
        let p = QdPaths::from_home_env(Path::new("/jail/home"), &env);
        assert_eq!(p.state_dir, Path::new("/jail/home/.quorum/dispatch/state"));
    }

    // ---- audit #4: --host path-traversal validation -------------------------

    #[test]
    fn is_valid_hostname_accepts_real_host_ids() {
        // A real host id is a single opaque segment.
        for ok in [
            "local", "peerbox", "brano", "host-1", "HOST_2", "a.b.c", "01ABCXYZ",
        ] {
            assert!(is_valid_hostname(ok), "{ok:?} is a valid bare hostname");
        }
    }

    #[test]
    fn is_valid_hostname_rejects_traversal_and_absolute() {
        // The audit #4 attack values — each escapes `remote/<host>/` if joined.
        for bad in [
            "",          // not a host
            "..",        // walks up out of remote/
            ".",         // degenerate self-segment
            "../../etc", // classic traversal
            "a/../../b", // traversal via an embedded segment
            "/etc",      // ABSOLUTE → Path::join discards the base
            "/",         // absolute root
            "foo/bar",   // nested/sibling path, not one host dir
            "peer/..",   // trailing traversal
            "a\\b",      // backslash separator (defensive)
            "has\0nul",  // NUL in a path component
        ] {
            assert!(
                !is_valid_hostname(bad),
                "{bad:?} must be rejected (traversal/absolute)"
            );
        }
    }

    #[test]
    fn a_valid_host_stays_confined_under_remote() {
        // Belt-and-suspenders: a VALID host joins to a direct child of remote/.
        let p = QdPaths::from_home(Path::new("/jail/home"));
        let remote = p.remote_dir();
        for h in ["local", "peerbox"] {
            assert!(is_valid_hostname(h));
            assert_eq!(p.remote_log_path(h).parent().unwrap(), remote.join(h));
            assert!(
                p.remote_log_path(h).starts_with(&remote),
                "confined under remote/"
            );
        }
    }
}
