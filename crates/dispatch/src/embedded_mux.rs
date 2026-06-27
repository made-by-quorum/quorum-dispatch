//! C1 M4 (D2) + WS-C M3b: the EmbeddedMux adapter — the 8-verb [`Mux`] trait over
//! the PER-SESSION qrmux client surface (`qrmux::client::session_client::*` +
//! `ensure_session_server_running` + `discovery::scan_sessions`). The shared-daemon
//! mode is RETIRED (spec §1, §9): there is now ONE daemon per session, each binding
//! `<dir>/<name>.sock` in the SAME resolved dir.
//!
//! ## list/list_raw via dir-scan + probe (WS-C §4.3)
//!
//! `list`/`list_raw` no longer ask a single shared daemon to enumerate; they call
//! [`scan_sessions`], which reads the resolved dir, filters `*.sock` leaves
//! (excluding the legacy `qrmux.sock`), probes each per-session daemon, and merges
//! its 0-or-1 rows. An unclaimed daemon (socket present, 0 sessions) is invisible.
//! D-LISTRAW is preserved by construction: a daemon unlinks its socket on session
//! end, so ended sessions never surface and `list == list_raw`.
//!
//! ## create/run + send/history/kill/attach via per-session client (WS-C §4.1/§4.2)
//!
//! `run_detached` calls [`ensure_session_server_running`] (cold-starts the session's
//! own daemon via the embedder launch spec) then [`create_detached_session`]. The
//! `send`/`history`/`kill`/`attach` paths use the per-session `session_client`
//! variants, each deriving the socket from the session NAME (`<dir>/<name>.sock`)
//! and enforcing the §3.2 client identity belt (ServerHello.session == name).
//!
//! ## Engine-side pre-validation (WS-C §2)
//!
//! Before any launch the create/attach paths run [`validate_session_identity`] +
//! [`session_socket_path_for`] (the dynamic sun_path budget) so a bad name or an
//! over-budget leaf surfaces the verbatim remedy-naming error BEFORE a spawn
//! attempt — not as an opaque cold-start failure.
//!

//! ## Async bridge (D2/R11)
//!
//! The `Mux` trait is SYNC; the qrmux client ops are async. The adapter owns a
//! lazily-init single-thread (current-thread) tokio runtime and `block_on`s each
//! op. A `tokio::time::timeout` wraps EVERY op EXCEPT `attach` — attach is an
//! interactive, unbounded-by-design handoff, so only its connect-phase timeout
//! (inside `attach_session`/`ensure_session_server_running`) applies. All errors
//! map to `io::Error` with context.
//!
//! ## Socket-dir agreement (the Bug-D analog, D2/R26)
//!
//! Each call passes the trait's `socket_dir` param straight into the client op as
//! `Some(dir)`. The per-session launcher (`ensure_session_server_running`, invoked
//! by `create_detached_session`/`attach_session`/`send_input_session`) propagates
//! the SAME dir to the daemon via `qrmux-server --socket-dir <dir> --session
//! <name>`, so the engine-resolved dir == the daemon-bound socket dir AND the leaf
//! == `<name>.sock` (the generalized G-CRUD keystone, §4.4). The engine computes
//! that dir with [`crate::qrmux_dir::resolve_qrmux_dir`]; gather/kill/attach call
//! sites feed it in via the `MuxDirs` lane (single source of truth).
//!
//! ## list/list_raw synthesis (D2/R8, divergence row D-LISTRAW)
//!
//! qrmux `SessionInfo { name, pid, cols, rows, created }` lacks most `MuxSession`
//! fields. The adapter SYNTHESIZES the rest (named here + in ADR 0013):
//!
//! - `clients` → 0 (qrmux does not expose an attach count in `SessionInfo`).
//! - `ended` → `None` ALWAYS (qrmux sessions VANISH on end — they never list as
//!   ended; this is the D-LISTRAW divergence: embedded `list_raw` can never surface
//!   an ended session, so reconcile's reap input differs by construction.
//!   Works-well: embedded sessions end clean; the ended-but-listed concept is a
//!   zmx-ism).
//! - `exit_code` → `None` (no ended → no exit code).
//! - `zmx_status` → an attachable-class string (a listed qrmux session is, by
//!   construction, live + reachable). We use `"attachable"`, which
//!   [`crate::mux::is_attachable`] treats as attachable (it only drops the literal
//!   `"unreachable"`).
//! - `err` → `None` (a listed session is reachable).
//! - `created` → from `SessionInfo.created` (Unix epoch SECONDS, the same unit zmx
//!   uses; `None` → 0).
//! - `socket_dir` → the resolved dir (so cross-dir merge + kill targeting work).
//! - `start_dir`/`cmd` → empty (not in `SessionInfo`); `current` → false.
//!
//! Because no listed session is ever ended/unreachable, `list` (filtered) and
//! `list_raw` (unfiltered) return the SAME rows for the embedded backend — the
//! filter is a no-op here, which is exactly the D-LISTRAW point.
//!
//! ## wait (D2/R10)
//!
//! PRODUCTION-DEAD: user `qd wait` polls pid-file status, backend-independent;
//! `Mux::wait` is unit-test-only. We provide a minimal trait-fill (bounded poll of
//! `list` until the named sessions are absent) for trait-completeness ONLY.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::exec::ExecResult;
use crate::mux::{is_attachable, Mux, MuxSession};
use crate::mux_selector::EmbeddedEnv;
use crate::qrmux_dir::resolve_qrmux_dir;

use qrmux::client::discovery::scan_sessions;
use qrmux::client::server_launcher::{ensure_session_server_running, ServerLaunchSpec};
use qrmux::client::session_client::{
    attach_session, create_detached_session, get_history_session, kill_session_session,
    launch_headless_session, send_input_session,
};
use qrmux::protocol::{ConnectMode, SessionInfo};
use qrmux::server::socket::{session_socket_path_for, validate_session_identity};

/// Test-lane override for the embedded daemon program (C1 M4fix mutation knob).
/// When set, the launch spec re-execs THIS program instead of `current_exe()`,
/// so a test can deliberately sever the wiring (point at a nonexistent binary)
/// and prove the cold-start path REDs. UNSET in production → `current_exe()` (the
/// real `qd` binary). Read ONLY here; never consulted on a non-test path by value.
const DAEMON_PROGRAM_ENV: &str = "SB_EMBEDDED_DAEMON_PROGRAM";

/// The argv-prefix the qd embedder uses for its hidden daemon entry: the qd binary
/// IS the daemon via `qd qrmux-server` (main.rs pre-clap dispatch).
const DAEMON_ARGS_PREFIX: &str = "qrmux-server";

/// Build the embedder's [`ServerLaunchSpec`]: re-exec the `qd` binary
/// (`current_exe()`) with `["qrmux-server"]`, so the daemon cold-start runs
/// `qd qrmux-server --socket-dir <dir>` — NOT `current_exe() server` (the bug:
/// `qd` has no bare `server` verb). The program is overridable by
/// [`DAEMON_PROGRAM_ENV`] for the mutation-control test ONLY.
fn embedder_launch_spec() -> io::Result<ServerLaunchSpec> {
    let program = match std::env::var(DAEMON_PROGRAM_ENV) {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => std::env::current_exe().map_err(|e| {
            io::Error::other(format!("embedded mux: cannot resolve current_exe: {e}"))
        })?,
    };
    Ok(ServerLaunchSpec {
        program,
        args_prefix: vec![DAEMON_ARGS_PREFIX.to_string()],
    })
}

/// Per-op timeout for the bounded (non-attach) verbs. Generous enough for a
/// busy host's daemon round-trip; the point is to never wedge `qd ls`/`qd kill`
/// on a hung daemon, not to be tight.
const OP_TIMEOUT: Duration = Duration::from_secs(15);

/// Bounded-poll budget for the production-dead `wait` trait-fill.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const WAIT_POLL_MAX: usize = 150; // ~30s ceiling; trait-fill only.

/// History scrollback depth requested on read/create (lines). Matches the
/// default the standalone CLI uses for a reattach.
const HISTORY_LINES: usize = 10_000;

/// The embedded mux: bridges the sync `Mux` trait to the async qrmux client over
/// a lazily-init current-thread tokio runtime. Carries an [`EmbeddedEnv`] snapshot
/// plus the injected home so it can resolve its socket dir (it cannot hold a
/// borrowed `&dyn Env`).
pub struct EmbeddedMux {
    home: PathBuf,
    env: EmbeddedEnv,
    runtime: std::sync::OnceLock<tokio::runtime::Runtime>,
}

impl EmbeddedMux {
    pub fn new(home: PathBuf, env: EmbeddedEnv) -> Self {
        Self {
            home,
            env,
            runtime: std::sync::OnceLock::new(),
        }
    }

    /// The socket dir this adapter would bind, resolved from its captured
    /// home + env snapshot via [`crate::qrmux_dir::resolve_qrmux_dir`].
    ///
    /// The trait ops are dir-pinned per-call (the call sites pass the dir from the
    /// `MuxDirs` lane — the single source of truth), so the adapter does not consult
    /// this internally. It is exposed so the keystone test (and any caller that has
    /// only the adapter) can ask the adapter itself which dir it agrees on — the
    /// SAME tier logic the gather/kill/attach sites use.
    pub fn resolved_dir(&self) -> io::Result<PathBuf> {
        embedded_socket_dir(&self.home, &self.env)
    }

    /// Lazily-init the single-thread runtime, then run `fut` to completion on it.
    /// The runtime is built on first use and shared for the adapter's lifetime.
    fn block_on<F: std::future::Future>(&self, fut: F) -> io::Result<F::Output> {
        let rt = match self.runtime.get() {
            Some(rt) => rt,
            None => {
                let built = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| {
                        io::Error::other(format!("embedded mux: failed to build runtime: {e}"))
                    })?;
                // OnceLock::get_or_init can't return a Result; we built it above
                // and set it (ignoring the race-loser case — get() then returns
                // the winner).
                let _ = self.runtime.set(built);
                self.runtime.get().expect("just set")
            }
        };
        Ok(rt.block_on(fut))
    }

    /// Run a bounded (timeout-wrapped) async op on the runtime, mapping both the
    /// timeout and the inner anyhow error to an io::Error with `ctx`.
    fn run_bounded<T, F>(&self, ctx: &str, fut: F) -> io::Result<T>
    where
        F: std::future::Future<Output = anyhow::Result<T>>,
    {
        let out = self.block_on(async move { tokio::time::timeout(OP_TIMEOUT, fut).await })?;
        match out {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(io::Error::other(format!("embedded mux: {ctx}: {e}"))),
            Err(_elapsed) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("embedded mux: {ctx}: timed out after {:?}", OP_TIMEOUT),
            )),
        }
    }

    /// Map one qrmux `SessionInfo` to a `MuxSession`, applying the NAMED synthesis
    /// (see the module doc / D-LISTRAW).
    fn to_mux_session(info: &SessionInfo, dir: &Path) -> MuxSession {
        MuxSession {
            name: info.name.clone(),
            pid: info.pid as i32,
            clients: 0,                                // not exposed in SessionInfo.
            created: info.created.unwrap_or(0) as i64, // epoch seconds, None → 0.
            start_dir: String::new(),                  // not in SessionInfo.
            cmd: String::new(),                        // not in SessionInfo.
            current: false,
            socket_dir: Some(dir.to_string_lossy().into_owned()),
            ended: None, // qrmux sessions vanish on end (D-LISTRAW).
            exit_code: None,
            zmx_status: Some("attachable".to_string()), // listed ⇒ live+reachable.
            err: None,
        }
    }

    /// Fetch + synthesize the session list pinned to `socket_dir` via the
    /// per-session dir-scan + probe (WS-C §4.3).
    fn list_synth(&self, socket_dir: &Path) -> io::Result<Vec<MuxSession>> {
        let dir = socket_dir.to_path_buf();
        let infos: Vec<SessionInfo> =
            self.run_bounded("scan_sessions", scan_sessions(Some(&dir)))?;
        Ok(infos
            .iter()
            .map(|i| Self::to_mux_session(i, &dir))
            .collect())
    }

    /// Engine-side pre-validation (WS-C §2): run the name-identity belt (charset +
    /// reserved-name tightening) and the DYNAMIC sun_path budget for THIS dir
    /// BEFORE any daemon launch, so a bad name / over-budget leaf surfaces the
    /// verbatim remedy-naming error first (not an opaque cold-start failure). The
    /// error text passes through unchanged (remedy-naming intact). Mapped to an
    /// embedded-backend-named io::Error like the other ops.
    fn pre_validate(&self, dir: &Path, name: &str) -> io::Result<()> {
        validate_session_identity(name)
            .map_err(|e| io::Error::other(format!("embedded mux: {e}")))?;
        // The budget check resolves+creates the dir and computes the per-dir max;
        // we discard the path (the client op re-derives it) — we only want the
        // error to fire here, before the spawn attempt.
        session_socket_path_for(Some(dir), name)
            .map(|_| ())
            .map_err(|e| io::Error::other(format!("embedded mux: {e}")))
    }

}

impl Mux for EmbeddedMux {
    fn list(&self, socket_dir: &Path) -> io::Result<Vec<MuxSession>> {
        // Filtered view. For the embedded backend no listed session is ever
        // ended/unreachable, so the filter is a no-op (D-LISTRAW) — applied
        // anyway for trait-uniformity.
        Ok(self
            .list_synth(socket_dir)?
            .into_iter()
            .filter(is_attachable)
            .collect())
    }

    fn list_raw(&self, socket_dir: &Path) -> io::Result<Vec<MuxSession>> {
        // RAW view. DIVERGENCE D-LISTRAW: embedded `list_raw` NEVER surfaces ended
        // sessions (qrmux sessions vanish on end), so reconcile's reap input
        // differs from the zmx lane by construction. Documented in ADR 0013.
        self.list_synth(socket_dir)
    }

    fn run_detached(
        &self,
        socket_dir: &Path,
        name: &str,
        shell_cmd: &str,
        cwd: &Path,
    ) -> io::Result<ExecResult> {
        // WS-C M3b: create_detached_session cold-starts the session's OWN daemon
        // (`ensure_session_server_running` → `<qd> qrmux-server --socket-dir <dir>
        // --session <name>`, the embedder launch spec, C1 M4fix) binding
        // `<dir>/<name>.sock`, then creates the detached session. Engine-side
        // pre-validation (§2) runs FIRST so a bad name / over-budget leaf surfaces
        // the verbatim remedy-naming error BEFORE any spawn attempt.
        let dir = socket_dir.to_path_buf();
        let name_s = name.to_string();
        let cmd_s = shell_cmd.to_string();
        let cwd_buf = cwd.to_path_buf();
        let spec = embedder_launch_spec()?;
        self.pre_validate(&dir, &name_s)?;
        let acked = self.run_bounded(
            "create_detached",
            create_detached_session(
                Some(&dir),
                Some(&spec),
                &name_s,
                &cmd_s,
                cwd_buf,
                HISTORY_LINES,
            ),
        )?;
        // Caller decides what success/failure means; the embedded daemon either
        // acks (Ok) or errors (mapped to io::Error above). Mirror ZmxMux's
        // ExecResult shape: status 0 + the acked name on stdout (informational).
        Ok(ExecResult {
            status: Some(0),
            stdout: acked,
            stderr: String::new(),
            timed_out: false,
        })
    }

    fn send(&self, socket_dir: &Path, name: &str, text: &str) -> io::Result<ExecResult> {
        // Raw fire-and-forget PTY write (no CR appended — submit discipline is the
        // caller's). send_input_session ensures the session's daemon is running
        // (per-session, §4.1) then writes; returns the acked byte count.
        let dir = socket_dir.to_path_buf();
        let name_s = name.to_string();
        let data = text.as_bytes().to_vec();
        let spec = embedder_launch_spec()?;
        self.pre_validate(&dir, &name_s)?;
        let bytes = self.run_bounded(
            "send_input",
            send_input_session(Some(&dir), Some(&spec), &name_s, data),
        )?;
        Ok(ExecResult {
            status: Some(0),
            stdout: bytes.to_string(),
            stderr: String::new(),
            timed_out: false,
        })
    }

    fn kill(&self, socket_dir: &Path, name: &str) -> io::Result<i32> {
        // kill_session_session is Result<()> — Ok ⇒ killed, Err ⇒ no such session /
        // daemon error. The kill.rs call site treats exit 0 as "zmx kill succeeded"
        // and any nonzero as a failure to add to its `failures` list; it ALSO does
        // its own verify-gone scan, so the exit code is advisory, not load-bearing.
        // Map Ok→0, Err→1 (mirroring TS `proc.exitCode ?? 1` / ZmxMux's
        // `r.status.unwrap_or(1)`): a clean kill is 0; any error is the nonzero
        // "could not reap via the mux" signal the verify scan then confirms.
        let dir = socket_dir.to_path_buf();
        let name_s = name.to_string();
        match self.run_bounded("kill_session", kill_session_session(Some(&dir), &name_s)) {
            Ok(()) => Ok(0),
            Err(_) => Ok(1),
        }
    }

    fn history(&self, socket_dir: &Path, name: &str) -> io::Result<String> {
        // get_history_session returns Vec<Vec<u8>> (scrollback + visible lines). The
        // trait contract is a single String (the boot answerer content-matches its
        // tail); join the lines with '\n' lossily (history is rendered ANSI bytes).
        let dir = socket_dir.to_path_buf();
        let name_s = name.to_string();
        let lines: Vec<Vec<u8>> =
            self.run_bounded("get_history", get_history_session(Some(&dir), &name_s))?;
        let joined = lines
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(joined)
    }

    fn wait(&self, socket_dir: &Path, names: &[String]) -> io::Result<i32> {
        // PRODUCTION-DEAD (D2/R10): user `qd wait` polls pid-file status; this is
        // trait-completeness ONLY. Bounded poll of `list` until none of `names`
        // are present (or the budget runs out → exit 1, "still present").
        if names.is_empty() {
            return Ok(0);
        }
        for _ in 0..WAIT_POLL_MAX {
            let present: Vec<MuxSession> = self.list_synth(socket_dir)?;
            let any = present.iter().any(|s| names.contains(&s.name));
            if !any {
                return Ok(0);
            }
            std::thread::sleep(WAIT_POLL_INTERVAL);
        }
        Ok(1)
    }

    fn attach(&self, socket_dir: &Path, name: &str) -> io::Result<i32> {
        // INTERACTIVE handoff: attach_session inherits stdio (raw-mode terminal
        // takeover). NO op-timeout — attach is unbounded by design; only the
        // connect-phase timeout inside ensure_session_server_running applies.
        // CreateOrAttach so an attach to a live session reattaches (the standalone
        // `attach` semantics); the engine has already resolved the session exists.
        let dir = socket_dir.to_path_buf();
        let name_s = name.to_string();
        let spec = embedder_launch_spec()?;
        self.pre_validate(&dir, &name_s)?;
        let res = self.block_on(attach_session(
            Some(&dir),
            Some(&spec),
            &name_s,
            HISTORY_LINES,
            ConnectMode::CreateOrAttach,
        ))?;
        match res {
            Ok(()) => Ok(0),
            Err(e) => Err(io::Error::other(format!("embedded mux: attach: {e}"))),
        }
    }
}

/// Resolve the embedded backend's socket dir from this adapter's snapshot — the
/// single-source-of-truth dir gather/kill/attach feed into every op. Exposed so
/// the `MuxDirs` lane (join.rs) and the keystone test can resolve the SAME dir
/// the adapter would bind.
pub fn embedded_socket_dir(home: &Path, env: &EmbeddedEnv) -> io::Result<PathBuf> {
    resolve_qrmux_dir(home, env).map_err(|msg| io::Error::other(format!("embedded mux: {msg}")))
}

/// Best-effort PER-SESSION daemon launch for a resolved dir + name (used by tests
/// that need the session's daemon up before asserting its bound socket path).
/// WS-C M3b: mirrors the auto-launch a `run_detached`/`attach` op triggers —
/// `ensure_session_server_running` with the EMBEDDER launch spec (C1 M4fix), so it
/// re-execs `qd qrmux-server --socket-dir <dir> --session <name>`, binding
/// `<dir>/<name>.sock`.
pub fn ensure_session_daemon(
    rt: &tokio::runtime::Runtime,
    dir: &Path,
    name: &str,
) -> io::Result<()> {
    let spec = embedder_launch_spec()?;
    rt.block_on(ensure_session_server_running(Some(dir), name, Some(&spec)))
        .map_err(|e| io::Error::other(format!("embedded mux: ensure_session_daemon: {e}")))
}

/// WP-B-CS-1 (D2 `qd start` agent caller / D3 `qd resume`): launch (or resume) a
/// headless `claude -p` stream-json turn for `name` via the per-session qrmux
/// daemon's `LaunchHeadless` verb (the D-LH client helper). Headless is inherently
/// the qrmux-daemon path (there is no PTY/pane to render), so it resolves the
/// embedded qrmux dir directly and cold-starts the session's OWN daemon with the
/// embedder launch spec — exactly the spawn `run_detached`/`attach` trigger above,
/// re-execing `qd qrmux-server --socket-dir <dir> --session <name>`.
/// `resume_session_id = Some(id)` continues an existing claude session
/// (`--resume`); `None` is a fresh launch.
///
/// A free helper (not a `Mux` trait method) on purpose: headless is embedded-only,
/// so this stays additive and does not perturb the zmx lane or the frozen `Mux`
/// fixtures. A one-shot current-thread runtime drives the single async send (this
/// is a terminal verb act, not a long-lived adapter — no shared runtime needed).
///
/// **IDENTITY / ADDRESSABILITY DEFERRED** (Fork C escape hatch, lead +
/// supervisor-ratified → folds into B5): this LAUNCHES the headless session but
/// does NOT yet mint/bind a registry identity or guarantee a daemon-flipped status
/// row. The daemon's `RegistryStatusSink` keys status on its own pid and
/// `registry::set_status` never CREATES a row, so the addressable-identity +
/// status-row wiring (option B: claude child-pid key + `FORCE_SESSION_PERSISTENCE`)
/// is a daemon-side fork, not B-CS-1. The session is launched-but-not-yet-addressable
/// until B5 lands it.
pub fn launch_headless_embedded(
    home: &Path,
    env: &dyn crate::effects::Env,
    name: &str,
    prompt: &str,
    resume_session_id: Option<&str>,
    cwd: Option<&str>,
    claude_args: &[String],
) -> io::Result<()> {
    let dir = resolve_qrmux_dir(home, env)
        .map_err(|msg| io::Error::other(format!("embedded mux: {msg}")))?;
    // Engine-side name belt (§2) BEFORE any spawn — verbatim remedy-naming error
    // first, not an opaque cold-start failure (mirrors `pre_validate`).
    validate_session_identity(name).map_err(|e| io::Error::other(format!("embedded mux: {e}")))?;
    let spec = embedder_launch_spec()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| io::Error::other(format!("embedded mux: failed to build runtime: {e}")))?;
    rt.block_on(launch_headless_session(
        Some(&dir),
        Some(&spec),
        name,
        prompt,
        resume_session_id,
        cwd,
        claude_args,
    ))
    .map_err(|e| io::Error::other(format!("embedded mux: launch_headless: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_env() -> EmbeddedEnv {
        EmbeddedEnv {
            xdg_runtime_dir: Some("/run/user/501".to_string()),
            sb_home: None,
            uid: 501,
        }
    }

    fn info(name: &str, pid: u32, created: Option<u64>) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pid,
            cols: 80,
            rows: 24,
            created,
        }
    }

    #[test]
    fn synthesis_table_maps_session_info() {
        let dir = Path::new("/run/user/501/qrmux");
        let s = EmbeddedMux::to_mux_session(&info("alpha", 111, Some(1_700_000_000)), dir);
        assert_eq!(s.name, "alpha");
        assert_eq!(s.pid, 111);
        assert_eq!(s.clients, 0, "clients synthesized to 0");
        assert_eq!(s.created, 1_700_000_000, "created from SessionInfo seconds");
        assert_eq!(s.start_dir, "", "start_dir synthesized empty");
        assert_eq!(s.cmd, "", "cmd synthesized empty");
        assert!(!s.current);
        assert_eq!(s.socket_dir.as_deref(), Some("/run/user/501/qrmux"));
        assert_eq!(s.ended, None, "ended always None (D-LISTRAW)");
        assert_eq!(s.exit_code, None);
        assert_eq!(s.zmx_status.as_deref(), Some("attachable"));
        assert_eq!(s.err, None);
        // The synthesized row is, by construction, attachable (filter no-op).
        assert!(is_attachable(&s), "synthesized rows are always attachable");
    }

    #[test]
    fn created_none_maps_to_zero() {
        let dir = Path::new("/run/user/501/qrmux");
        let s = EmbeddedMux::to_mux_session(&info("beta", 222, None), dir);
        assert_eq!(s.created, 0, "None created → 0");
    }

    #[test]
    fn construction_does_no_io() {
        // Building the adapter must not touch the filesystem or spawn a runtime
        // (the runtime is lazily-init on first block_on). This is the "no orphan
        // daemons, no I/O at selection time" guarantee.
        let _mux = EmbeddedMux::new(PathBuf::from("/jail/home"), snap_env());
        // No daemon spawned, no dir created — just a struct.
    }

    #[test]
    fn embedded_socket_dir_resolves_via_snapshot() {
        let dir = embedded_socket_dir(Path::new("/jail/home"), &snap_env()).unwrap();
        assert_eq!(dir, Path::new("/run/user/501/qrmux"));
    }

    #[test]
    fn adapter_resolved_dir_agrees_with_free_fn() {
        // The adapter's self-resolution uses the SAME tier logic the MuxDirs lane
        // feeds into every op (keystone agreement — engine-resolved == adapter dir).
        let mux = EmbeddedMux::new(PathBuf::from("/jail/home"), snap_env());
        assert_eq!(
            mux.resolved_dir().unwrap(),
            embedded_socket_dir(Path::new("/jail/home"), &snap_env()).unwrap()
        );
    }

    #[test]
    fn wait_empty_names_is_ok() {
        // Trait-fill: no names → immediately Ok(0), no daemon contact.
        let mux = EmbeddedMux::new(PathBuf::from("/jail/home"), snap_env());
        assert_eq!(mux.wait(Path::new("/run/user/501/qrmux"), &[]).unwrap(), 0);
    }
}
