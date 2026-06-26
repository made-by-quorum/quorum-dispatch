//! Daemon-side headless launch factory (WP-B2b-2b, design §D-2b) — the sb-side
//! object injected into `sbmux`'s `DaemonCtx` so the embedded daemon
//! ([`crate::bin::dispatch::daemon`] → `run_server`) can launch a `claude -p`
//! stream-json turn and write the registry status row in real time.
//!
//! sbmux cannot reference [`crate::daemon_status::RegistryStatusSink`] (sb depends
//! on sbmux, never the reverse — the governing constraint). So the COMPOSITE sink
//! is assembled sb-side: this factory resolves the launch posture (the SAME
//! `claude_bin`/`claude_flags`/env resolvers the interactive path uses — seam #1)
//! AND builds the `RegistryStatusSink`; sbmux's `SessionManager::create_headless`
//! fans that out with its own `SocketFanoutSink` so ONE reader/pump drives both.

use crate::daemon_status::{MintIdentity, RegistryStatusSink};
use crate::observe::HEADLESS_ENTRYPOINT;
use sbmux::headless::{HeadlessLaunch, Sink};
use sbmux::headless_session::{HeadlessFactory, HeadlessLaunchPlan};
use std::path::PathBuf;

/// The concrete [`HeadlessFactory`] the embedded daemon injects. Every field is
/// resolved ONCE at daemon start (the launch posture is daemon-wide); a
/// `LaunchHeadless` per-request supplies only `name`/`prompt`/`resume_session_id`.
pub struct DaemonHeadlessFactory {
    /// The `claude` binary (`launch::claude_bin`).
    pub claude_bin: String,
    /// The spawn-time flags (`launch::claude_flags`) — permission posture + relay
    /// channel injection, resolved by the daemon and passed flags-FIRST.
    pub flags: Vec<String>,
    /// Env injected directly by the daemon (seam #1). Empty + `clear_env=false`
    /// (production) means the child inherits the daemon's already-correct
    /// environment; the integration test sets `clear_env=true` + an isolated HOME.
    /// WP-B5-i ALSO appends `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1` at
    /// `resolve` time (see [`Self::resolve`]) — this `env` is the BASE.
    pub env: Vec<(String, String)>,
    /// Start from an empty environment, passing ONLY `env` (the `env -i`
    /// isolation contract — never inherit the live `~/.claude*`).
    pub clear_env: bool,
    /// Working directory for the child (`None` = inherit the daemon's cwd).
    pub cwd: Option<String>,
    /// The sessions dir holding `<pid>.json` (the registry row this daemon mints).
    pub sessions_dir: PathBuf,
    /// The idstore path (`<state_dir>/ids.jsonl`) — the SAME store `sb ls` folds.
    /// WP-B5-ii-b PROOF 3: on a headless RESUME the factory resolves the child's
    /// OWN stable sbId from its recorded `session_id` here (`mint_or_get`) and
    /// injects it as `SB_SESSION_ID` (internal self-identity, parity with the
    /// interactive path). Because it is the same store keyed by the same UUID, the
    /// injected `SB_SESSION_ID` MATCHES the sbId `sb ls` surfaces for the
    /// daemon-minted child-pid row (the row↔env consistency invariant).
    pub ids_path: PathBuf,
    /// R3b-Step-0: the per-daemon shared **signal-B** progress producer. Threaded
    /// into every per-session [`RegistryStatusSink`] so each session's OUTPUT
    /// Republishes advance `last_output_ms` on the live path (the daemon-mint sink
    /// is the production tap). One recorder per daemon, shared across its sessions;
    /// the recovery ladder (R3c) and WS-A (A3) read it via
    /// [`crate::progress::ProgressProducer`].
    pub progress: std::sync::Arc<crate::progress::ProgressRecorder>,
    /// R3c item-2: the per-daemon shared **signal-A** turn-start producer. Threaded
    /// into every per-session [`RegistryStatusSink`] so each session's turn
    /// boundaries record/clear the turn-start anchor on the live path. One recorder
    /// per daemon, shared across its sessions; the recovery ladder reads a REAL
    /// `since_turn_start_ms` via [`crate::progress::TurnStartProducer`].
    pub turn_clock: std::sync::Arc<crate::progress::TurnStartRecorder>,
}

impl DaemonHeadlessFactory {
    /// Resolve the production launch posture from the daemon's environment + the
    /// sb paths. Env-inheriting (`clear_env=false`, `env=[]`): the embedded daemon
    /// was itself launched with the correct HOME/PATH, so the headless child
    /// inherits it.
    ///
    /// WP-B5-i (identity option B): there is NO daemon-pid / boot-row read here. The
    /// rejected option A keyed status on the daemon's own `std::process::id()`; B
    /// keys identity AND status on the claude CHILD pid (known only post-spawn), so
    /// the sink is built per-launch in [`Self::resolve`] via a deferred factory.
    pub fn from_daemon_env(env: &impl crate::effects::Env, paths: &crate::paths::SbPaths) -> Self {
        let config_toml = paths
            .home
            .join(".quorum")
            .join("dispatch")
            .join("config.toml");
        let claude_bin = crate::launch::claude_bin(env);
        let flags = crate::launch::claude_flags(env, &config_toml);
        Self {
            claude_bin,
            flags,
            env: Vec::new(),
            clear_env: false,
            cwd: None,
            sessions_dir: paths.sessions_dir.clone(),
            ids_path: crate::idstore::ids_path(&paths.state_dir),
            progress: std::sync::Arc::new(crate::progress::ProgressRecorder::new()),
            turn_clock: std::sync::Arc::new(crate::progress::TurnStartRecorder::new()),
        }
    }
}

impl HeadlessFactory for DaemonHeadlessFactory {
    fn resolve(
        &self,
        name: &str,
        prompt: &str,
        resume_session_id: Option<&str>,
        cwd: Option<&str>,
        claude_args: &[String],
    ) -> anyhow::Result<HeadlessLaunchPlan> {
        // WP-B5-i: the per-session cwd threaded from `sb start` WINS; fall back to
        // the factory's own `cwd` (None in production) when the request omits it.
        // This is the value BOTH the child spawns in (`launch.cwd`) AND the minted
        // row records (`MintIdentity.cwd`) — they must agree.
        let cwd = cwd.map(str::to_string).or_else(|| self.cwd.clone());
        // WP-B5-i: append the per-session `claude_args` AFTER the daemon's resolved
        // posture flags — posture FIRST, then passthrough (the flags-first
        // byte-stability the B2a addendum established; `HeadlessLaunch::argv` keeps
        // these ahead of `-p PROMPT`).
        let mut flags = self.flags.clone();
        flags.extend(claude_args.iter().cloned());
        // WP-B5-ii-b PROOF 3 (headless SB_SESSION_ID parity, sb-supervisor-10
        // ruling): a headless agent must self-identify via `$SB_SESSION_ID` exactly
        // as the interactive path does (whoami.rs PREFERS it; relay self-registration
        // + self-directed `sb` need it). The interactive/zmux path injects it via
        // `launch_env_pairs` (`resume.rs` → `prepare_claude_resume_env`); the D3
        // headless path DIVERGED and never did — that divergence is the bug this
        // closes. Resolve the child's OWN stable sbId from its RECORDED `session_id`
        // via the SAME idstore `sb ls` folds (so the injected `SB_SESSION_ID`
        // MATCHES the sbId of the daemon-minted child-pid row — one identity, two
        // surfaces, the row↔env consistency invariant). A future fork resumes its
        // OWN session_id → mints its OWN sbId (never the parent's) — forward-
        // compatible with B5-iii. On START (`resume_session_id == None` — the UUID is
        // only known at `system/init`) there is nothing to resolve pre-spawn; the row
        // carries identity externally and the env stays byte-stable with pre-B5-ii-b
        // start (FORCE only).
        let sb_session_id = resume_session_id.and_then(|uuid| {
            match crate::idstore::mint_or_get(
                &self.ids_path,
                uuid,
                Some(name),
                &crate::effects::RealClock,
            ) {
                Ok(id) => Some(id),
                Err(e) => {
                    // Self-id is best-effort — never fail the launch on an idstore
                    // hiccup; the external child-pid row identity still lands.
                    eprintln!("sb[daemon_headless]: SB_SESSION_ID resolve skipped: {e}");
                    None
                }
            }
        });
        // Reuse the ONE shared env-assembly (`launch_env_pairs`): backend pairs
        // FIRST, then `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1` (rides every launch —
        // the nested-spawn registration fix, board STATE 130), then `SB_SESSION_ID`
        // LAST (never in the capture whitelist — engine-asserted, never inherited).
        // `RenderMode::AltScreen` injects NO screen var (a headless agent has no TUI),
        // so START's env is byte-identical to the prior hand-rolled FORCE-only env.
        let env = crate::launch::launch_env_pairs(
            self.env.clone(),
            sb_session_id,
            crate::launch::RenderMode::AltScreen,
        );
        let launch = HeadlessLaunch {
            claude_bin: self.claude_bin.clone(),
            prompt: prompt.to_string(),
            resume_session_id: resume_session_id.map(str::to_string),
            cwd: cwd.clone(),
            env,
            clear_env: self.clear_env,
            flags,
        };
        // WP-B5-i: the registry-write half of the composite, DEFERRED until the
        // claude child pid is known (post-spawn). The factory mints the
        // CHILD-PID-keyed row on the first `Republish::Ready` (the daemon-mint
        // fallback — claude writes no row headless), carrying the sb `name`, the
        // per-session `cwd`, the `headless` discriminant, and NO provider field.
        let sessions_dir = self.sessions_dir.clone();
        let identity = MintIdentity {
            name: Some(name.to_string()),
            cwd: cwd.clone(),
            entrypoint: Some(HEADLESS_ENTRYPOINT.to_string()),
            // WP-B5-i (lead ruling, D round-trip): a claude row carries NO
            // `provider` field — the EXISTING interactive invariant (common.rs:
            // "writes no provider field for claude rows"; the join defaults absent
            // → "claude-code", model.rs). Writing an explicit "claude" here (the
            // initial banked mistake) is REJECTED by provider_for/
            // refuse_unknown_provider → `sb connect` refuses the row "unknown
            // provider" BEFORE the observe resolver runs. `None` mirrors the
            // interactive `<pid>.json` model exactly (the point of option-B
            // identity) so connect/ls/resolve dispatch on the row uniformly.
            provider: None,
        };
        let progress = self.progress.clone();
        let turn_clock = self.turn_clock.clone();
        let status_sink_factory: Box<dyn FnOnce(i64) -> Box<dyn Sink> + Send> =
            Box::new(move |child_pid: i64| {
                // R3b-Step-0: attach the per-daemon signal-B producer so this
                // session's OUTPUT Republishes advance `last_output_ms` on the live
                // path (the production tap that makes signal-B non-test-only).
                // R3c item-2: attach the per-daemon signal-A producer so this
                // session's turn boundaries record/clear the turn-start anchor — the
                // production tap that makes signal-A's `since_turn_start_ms`
                // non-test-only (the rev0 gap the live-chain gate fed synthetically).
                Box::new(
                    RegistryStatusSink::with_mint_system_clock(sessions_dir, child_pid, identity)
                        .with_progress(progress)
                        .with_turn_clock(turn_clock),
                ) as Box<dyn Sink>
            });
        Ok(HeadlessLaunchPlan {
            launch,
            status_sink_factory: Some(status_sink_factory),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sbmux::headless_session::HeadlessFactory;

    fn factory(daemon_cwd: Option<&str>, flags: &[&str]) -> DaemonHeadlessFactory {
        DaemonHeadlessFactory {
            claude_bin: "claude".to_string(),
            flags: flags.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            clear_env: false,
            cwd: daemon_cwd.map(str::to_string),
            sessions_dir: PathBuf::from("/tmp/sessions"),
            ids_path: PathBuf::from("/tmp/ids.jsonl"),
            progress: std::sync::Arc::new(crate::progress::ProgressRecorder::new()),
            turn_clock: std::sync::Arc::new(crate::progress::TurnStartRecorder::new()),
        }
    }

    /// WP-B5-i (C): the per-session `cwd` threaded from `sb start` WINS over the
    /// factory's own cwd, and `claude_args` are APPENDED to the daemon's resolved
    /// posture flags (posture FIRST). The resolved argv keeps the appended args
    /// ahead of `-p PROMPT` (flags-first byte-stability).
    ///
    /// FIX-SHAPED MUTATION (red-before): in `resolve`, replace the per-request cwd
    /// `cwd.map(..).or_else(|| self.cwd.clone())` with bare `self.cwd.clone()` →
    /// the child falls back to the DAEMON cwd (first assert reds); drop the
    /// `flags.extend(claude_args..)` → the passthrough args vanish from argv (the
    /// flags + argv asserts red).
    #[test]
    fn resolve_threads_per_session_cwd_and_appends_claude_args() {
        let f = factory(Some("/daemon/cwd"), &["--posture"]);
        let args = vec!["--model".to_string(), "opus".to_string()];
        let plan = f
            .resolve("sess", "the prompt", None, Some("/session/cwd"), &args)
            .unwrap();

        // (1) the threaded per-session cwd WINS over the factory's daemon cwd.
        assert_eq!(
            plan.launch.cwd.as_deref(),
            Some("/session/cwd"),
            "the per-session cwd must win over the daemon cwd"
        );

        // (2) claude_args appended AFTER the posture flags (posture first).
        assert_eq!(
            plan.launch.flags,
            vec!["--posture", "--model", "opus"],
            "claude_args append after the resolved posture flags"
        );

        // (3) the resolved argv keeps posture + passthrough ahead of `-p PROMPT`.
        let argv = plan.launch.argv();
        let posture_at = argv.iter().position(|a| a == "--posture").unwrap();
        let model_at = argv.iter().position(|a| a == "--model").unwrap();
        let dash_p_at = argv.iter().position(|a| a == "-p").unwrap();
        assert!(
            posture_at < model_at && model_at < dash_p_at,
            "flags-first ordering: posture < claude_args < -p; got {argv:?}"
        );
    }

    /// WP-B5-ii-b PROOF 2 (cheap default-floor mirror of the `#[ignore]`
    /// `headless_rekey_survives_death` integration seed): the factory threads the
    /// claude CHILD pid handed to its status-sink factory into the MINT — the row is
    /// keyed on the child, NOT the daemon's `std::process::id()`. That keying CHOICE
    /// is exactly why identity SURVIVES a daemon kill+respawn: the child outlives the
    /// daemon, so the child-pid-keyed row stays valid while the daemon's pid changes.
    ///
    /// FIX-SHAPED MUTATION (red-before): in `resolve`, pass `std::process::id() as
    /// i64` to `with_mint_system_clock` instead of the `child_pid` the factory is
    /// handed → the row is keyed on the DAEMON pid → `row.pid` reds here (and, in the
    /// integration proof, the surviving row points at the now-dead daemon).
    #[test]
    fn factory_mints_row_keyed_on_child_pid_not_daemon_pid() {
        use sbmux::headless::{Republish, Sink};
        let dir = tempfile::tempdir().unwrap();
        let f = DaemonHeadlessFactory {
            claude_bin: "claude".to_string(),
            flags: Vec::new(),
            env: Vec::new(),
            clear_env: false,
            cwd: None,
            sessions_dir: dir.path().to_path_buf(),
            ids_path: dir.path().join("ids.jsonl"),
            progress: std::sync::Arc::new(crate::progress::ProgressRecorder::new()),
            turn_clock: std::sync::Arc::new(crate::progress::TurnStartRecorder::new()),
        };
        let plan = f.resolve("sess", "p", None, None, &[]).unwrap();
        let make = plan
            .status_sink_factory
            .expect("a headless mint factory is always returned");
        // A child pid DISTINCT from this process's pid (the daemon pid the mutation
        // would substitute) — so the assert genuinely distinguishes the two.
        let child_pid: i64 = 4242;
        assert_ne!(
            child_pid,
            std::process::id() as i64,
            "child pid != daemon pid"
        );
        make(child_pid).deliver(Republish::Ready {
            session_id: "claude-uuid".to_string(),
        });
        let row = crate::registry::read_entry(dir.path(), child_pid)
            .expect("the mint is keyed on the CHILD pid handed to the factory");
        assert_eq!(
            row.pid,
            Some(child_pid),
            "row keyed on the child pid (survives daemon death), not the daemon pid"
        );
        assert_eq!(row.session_id.as_deref(), Some("claude-uuid"));
        assert!(
            crate::registry::read_entry(dir.path(), std::process::id() as i64).is_none(),
            "no row is keyed on the daemon's own pid (a daemon-pid row dies with the daemon)"
        );
    }

    /// WP-B5-ii-b PROOF 3 (cheap default-floor mirror of the a1/a4 end-to-end
    /// seeds): a headless RESUME injects `SB_SESSION_ID` LAST, resolved from the
    /// child's OWN recorded `session_id` via the SAME idstore — so the env self-id
    /// MATCHES the sbId `sb ls` surfaces for the daemon-minted row (the row↔env
    /// consistency invariant). A START launch (`resume_session_id == None`) injects
    /// NONE (byte-stable with pre-B5-ii-b start: FORCE only, no `SB_SESSION_ID`).
    ///
    /// FIX-SHAPED MUTATION (red-before): drop the `resume_session_id.and_then(...)`
    /// resolution (force `sb_session_id = None`, the pre-fix D3 divergence) → the
    /// resume env carries NO `SB_SESSION_ID` → the "resume injects the recorded sbId"
    /// assert reds (`a1` got `''`).
    #[test]
    fn resolve_injects_sb_session_id_on_resume_matching_idstore() {
        let dir = tempfile::tempdir().unwrap();
        let ids_path = dir.path().join("ids.jsonl");
        let f = DaemonHeadlessFactory {
            claude_bin: "claude".to_string(),
            flags: Vec::new(),
            env: Vec::new(),
            clear_env: false,
            cwd: None,
            sessions_dir: dir.path().to_path_buf(),
            ids_path: ids_path.clone(),
            progress: std::sync::Arc::new(crate::progress::ProgressRecorder::new()),
            turn_clock: std::sync::Arc::new(crate::progress::TurnStartRecorder::new()),
        };
        let uuid = "11111111-2222-3333-4444-555555555555";

        // RESUME: the injected SB_SESSION_ID is the sbId the SAME idstore resolves for
        // this session_id (the row↔env match — `sb ls` folds the same store/UUID).
        let plan = f.resolve("wk", "p", Some(uuid), None, &[]).unwrap();
        let want =
            crate::idstore::mint_or_get(&ids_path, uuid, Some("wk"), &crate::effects::RealClock)
                .unwrap();
        let sid = plan
            .launch
            .env
            .iter()
            .find(|(k, _)| k == "SB_SESSION_ID")
            .map(|(_, v)| v.clone());
        assert_eq!(
            sid.as_deref(),
            Some(want.as_str()),
            "resume injects the child's OWN recorded sbId (matches `sb ls` for the row)"
        );
        // It lands LAST (the launch_env_pairs discipline).
        assert_eq!(
            plan.launch.env.last().map(|(k, _)| k.as_str()),
            Some("SB_SESSION_ID"),
            "SB_SESSION_ID lands LAST in the env"
        );

        // START (no resume_session_id): no SB_SESSION_ID — byte-stable with pre-fix.
        let start = f.resolve("wk", "p", None, None, &[]).unwrap();
        assert!(
            !start.launch.env.iter().any(|(k, _)| k == "SB_SESSION_ID"),
            "start injects no SB_SESSION_ID (the UUID is unknown pre-system/init)"
        );
    }

    /// WP-B5-i (C): when `LaunchHeadless` omits the per-session cwd (`None` — e.g.
    /// the resume path), the resolver falls back to the factory's own `cwd`
    /// (today's daemon-inherits behaviour), and an empty `claude_args` adds nothing.
    #[test]
    fn resolve_falls_back_to_factory_cwd_when_request_omits() {
        let f = factory(Some("/daemon/cwd"), &["--posture"]);
        let plan = f.resolve("sess", "p", None, None, &[]).unwrap();
        assert_eq!(
            plan.launch.cwd.as_deref(),
            Some("/daemon/cwd"),
            "a None per-session cwd falls back to the factory cwd"
        );
        assert_eq!(
            plan.launch.flags,
            vec!["--posture"],
            "empty claude_args append nothing"
        );
    }
}
