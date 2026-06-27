//! The ONE place the home→state-layout mapping lives.
//!
//! TS qd derives its CLAUDE state dirs from `homedir()` (src/session.ts:6-8:
//! `const HOME = homedir(); SESSIONS_DIR = ~/.claude/sessions; PROJECTS_DIR =
//! ~/.claude/projects`; RELAY_DIR = ~/.claude/relay, src/session.ts:150) — these
//! derive from HOME ONLY. The qd DATA root is separate and SB_HOME-overridable:
//! `sbHome = env.SB_HOME || join(HOME, ".quorum", "dispatch")`, and the hot-state dir is
//! `<sbHome>/state` (bootstrap.ts:88-96 `resolveBootstrapPaths` +
//! BootstrapPaths.stateDir doc, bootstrap.ts:~54). `marks.jsonl` (A3 ADD-3)
//! lives in that state dir.
//!
//! Lesson L9a / ADD-4: **HOME is load-bearing.** A jail that overrides
//! SB_HOME/ZMX_DIR/XDG_*/TMPDIR but not HOME still sees — and could mutate —
//! the org's REAL registry (found during 0b dry-run setup; read-only exposure,
//! fixed before any kill/gc/send). In Rust the same rule holds structurally:
//! NOTHING in this crate resolves the real home OR reads SB_HOME directly;
//! everything takes an `SbPaths` built from an injected home + the injected `Env`
//! seam. Tests inject temp dirs, period.

use std::path::{Path, PathBuf};

use crate::effects::Env;

/// Resolved state-dir layout for one home.
#[derive(Debug, Clone, PartialEq)]
pub struct SbPaths {
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
    /// qd hot-state dir, `<sbHome>/state` where `sbHome = SB_HOME || <home>/.quorum/dispatch`
    /// (bootstrap.ts:88-96). Holds `marks.jsonl` (ADD-3). SB_HOME comes through
    /// the injected `Env` seam (L9a), never raw `std::env`.
    pub state_dir: PathBuf,
}

impl SbPaths {
    /// Mirror of src/session.ts:6-8 + :150 for the `.claude` dirs, plus the qd
    /// DATA `state_dir`. With NO injected env this assumes SB_HOME is unset, so
    /// `state_dir = <home>/.quorum/dispatch/state` (the default). Callers that must honor an
    /// SB_HOME override use [`SbPaths::from_home_env`].
    pub fn from_home(home: &Path) -> Self {
        Self::build(home, home.join(".quorum").join("dispatch"))
    }

    /// As [`from_home`], but resolves the qd data root via the injected `Env`
    /// seam: `sbHome = SB_HOME || <home>/.quorum/dispatch` (bootstrap.ts:88-96), so `state_dir`
    /// honors an SB_HOME override (H4). SB_HOME is read ONLY through `env` (L9a),
    /// never raw `std::env`. The `.claude` dirs are unchanged (HOME-only).
    pub fn from_home_env(home: &Path, env: &dyn Env) -> Self {
        let sb_home = env
            .var("SB_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".quorum").join("dispatch"));
        Self::build(home, sb_home)
    }

    /// Shared constructor: the `.claude` dirs derive from `home`; `state_dir`
    /// derives from the already-resolved `sb_home`.
    fn build(home: &Path, sb_home: PathBuf) -> Self {
        let claude = home.join(".claude");
        SbPaths {
            home: home.to_path_buf(),
            sessions_dir: claude.join("sessions"),
            projects_dir: claude.join("projects"),
            relay_dir: claude.join("relay"),
            // server.ts:65: join(homedir(), '.claude', 'channels', 'relay', 'inbox').
            // DISTINCT from relay_dir (which is `.claude/relay`). P-C2.
            inbox_dir: claude.join("channels").join("relay").join("inbox"),
            state_dir: sb_home.join("state"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;

    #[test]
    fn layout_mirrors_ts() {
        let p = SbPaths::from_home(Path::new("/jail/home"));
        assert_eq!(p.sessions_dir, Path::new("/jail/home/.claude/sessions"));
        assert_eq!(p.projects_dir, Path::new("/jail/home/.claude/projects"));
        assert_eq!(p.relay_dir, Path::new("/jail/home/.claude/relay"));
        // inbox_dir is DISTINCT from relay_dir (server.ts:65). P-C2.
        assert_eq!(
            p.inbox_dir,
            Path::new("/jail/home/.claude/channels/relay/inbox")
        );
        // SB_HOME unset default: <home>/.quorum/dispatch/state.
        assert_eq!(p.state_dir, Path::new("/jail/home/.quorum/dispatch/state"));
    }

    #[test]
    fn from_home_env_default_state_dir() {
        // No SB_HOME → <home>/.quorum/dispatch/state (bootstrap.ts:88-96 default).
        let env = MapEnv::default();
        let p = SbPaths::from_home_env(Path::new("/jail/home"), &env);
        assert_eq!(p.state_dir, Path::new("/jail/home/.quorum/dispatch/state"));
    }

    #[test]
    fn from_home_env_honors_sb_home_override() {
        // SB_HOME set → <SB_HOME>/state, NOT <home>/.quorum/dispatch/state.
        let mut env = MapEnv::default();
        env.vars
            .insert("SB_HOME".to_string(), "/elsewhere/sbdata".to_string());
        let p = SbPaths::from_home_env(Path::new("/jail/home"), &env);
        assert_eq!(p.state_dir, Path::new("/elsewhere/sbdata/state"));
        // The .claude dirs remain HOME-derived (NOT under SB_HOME).
        assert_eq!(p.sessions_dir, Path::new("/jail/home/.claude/sessions"));
    }

    #[test]
    fn empty_sb_home_falls_back_to_default() {
        // SB_HOME="" is falsy → default (JS `||` semantics).
        let mut env = MapEnv::default();
        env.vars.insert("SB_HOME".to_string(), String::new());
        let p = SbPaths::from_home_env(Path::new("/jail/home"), &env);
        assert_eq!(p.state_dir, Path::new("/jail/home/.quorum/dispatch/state"));
    }
}
