//! `provider::pi` — the pi non-PTY harness seam (WS-A.2). pi is a first-class
//! NON-PTY provider in Quorum Dispatch: a stdio JSONL-RPC transport, daemon-
//! hosted, registered alongside `claude-code` and `codex`. Built the CODEX WAY —
//! this module mirrors `provider::codex`; the ACP `ladder.rs` is UNTOUCHED
//! (there is no literal ladder to change — codex and pi both dispatch via the
//! `provider_for` registry + a `ProviderFx` transport member + `Hosting::Daemon`).
//!
//! Submodules:
//!   - [`rpc`]     — [`rpc::PiRpc`] contract + the stdio-JSONL envelope/event
//!     types (the `AppServerRpc` analog).
//!   - [`session`] — permissive session-JSONL reading + path math, tolerant of
//!     pi's lazy-write window ("no file" ≠ "no session").
//!
//! TODO(tier-a integration, needs a build slot — held per the cargo handshake):
//!   1. `pub mod pi;` + `pub use` + a `"pi"` arm in `provider.rs::provider_for`.
//!   2. add `pub pi_rpc: Option<&'a dyn pi::rpc::PiRpc>` to `ProviderFx` and the
//!      `pi_rpc: None,` line at the 10 construction sites (this file's methods
//!      already consume `fx.pi_rpc`).
//!   3. the per-session adapter daemon (item 1) + pgid teardown (item 3) live in
//!      the create/resume-daemon paths, not here. NOTE (moved base, OPTION B): the
//!      resident no longer PUSHES a `Republish` stream into a `RegistryStatusSink`
//!      — the P4DB burn (`d44e869`) deleted that sink and main derives status
//!      ON-READ. The [`status::PiStatusMapper`] → [`republish::Republish`] contract
//!      is retained (pi-local, tier-a-tested) as the normalized event surface A2
//!      wires to a connect/status poll; A1's resident derives status on-read.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::jsonl::{JsonlStats, TranscriptMeta};
use crate::model::SessionStatus;
use crate::provider::{
    Hosting, InjectError, LaunchPlan, LaunchRequest, Provider, ProviderFx, SessionKey,
};

pub mod chaos;
pub mod conformance;
pub mod daemon;
pub mod floor;
pub mod pin;
pub mod redteam;
pub mod remote;
pub mod republish;
pub mod residence;
pub mod rpc;
pub mod session;
pub mod status;
pub mod stdio;
pub mod tui;

pub use remote::{PiObservation, PiRemote};
pub use rpc::{PiEvent, PiRpc, PiRpcError, RpcSessionState, StreamingBehavior};
pub use status::PiStatusMapper;
pub use stdio::PiStdio;

// ===========================================================================
// PiProvider — the daemon-hosted Provider impl.
// ===========================================================================

/// The pi `--mode rpc` provider. A unit struct — pi's durable truth is the
/// adapter daemon + the session JSONL file, so the provider holds no state; the
/// live transport arrives per-call via [`ProviderFx::pi_rpc`] (the codex
/// `app_server` precedent).
pub struct PiProvider;

/// The ONE registered pi provider (a `'static` singleton so `provider_for` hands
/// out `&'static dyn Provider` without allocation — the `CODEX_PROVIDER`
/// precedent).
pub static PI_PROVIDER: PiProvider = PiProvider;

/// Override the pi binary on the launch argv (else `"pi"`). pi is NOT on PATH in
/// quorum boxes (`~/.npm-pi-global/bin/pi`), so the create path is expected to
/// set this to the absolute pinned-0.80.2 binary.
const PI_BIN_ENV: &str = "QD_PI_BIN";
/// pi's own session-storage override (`--session-dir` / this env). The
/// `CODEX_HOME` analog: when set, sessions are read/written under it. Absent ⇒
/// `$HOME/.pi/agent/sessions`.
const PI_SESSION_DIR_ENV: &str = "PI_CODING_AGENT_SESSION_DIR";

/// THE pi binary this engine launches: `QD_PI_BIN` if set and non-empty, else
/// `"pi"` off PATH.
///
/// PUBLIC and shared (the `codex::codex_bin` precedent) so the interactive create
/// path's capability preflight ([`tui::supports_session_id`]) inspects the SAME
/// binary `launch_plan` will run. Resolving it twice by two rules is precisely how
/// a preflight comes to bless one binary while the pane launches another.
pub fn pi_bin(env: &dyn crate::effects::Env) -> String {
    env.var(PI_BIN_ENV)
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "pi".to_string())
}

/// Resolve pi's sessions root off an injected [`crate::effects::Env`] ONLY (never
/// raw `std::env`): `PI_CODING_AGENT_SESSION_DIR` if set, else
/// `$HOME/.pi/agent/sessions`. `None` when no home is resolvable (degrades to a
/// permissive miss in the readers).
///
/// PUBLIC because the interactive create path needs this root BEFORE it has any
/// `ProviderFx` to hand — it checks [`tui::session_id_is_taken`] against the
/// store before launching a pane. Taking `&dyn Env` rather than a `ProviderFx`
/// keeps that caller from having to fabricate an fx of `None`s just to read two
/// environment variables.
pub fn sessions_root(env: &dyn crate::effects::Env) -> Option<PathBuf> {
    if let Some(d) = env.var(PI_SESSION_DIR_ENV).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let home = env.var("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".pi").join("agent").join("sessions"))
}

/// [`sessions_root`] off a [`ProviderFx`] — the `Provider`-trait call sites.
fn pi_sessions_root(fx: &ProviderFx) -> Option<PathBuf> {
    sessions_root(fx.env)
}

impl Provider for PiProvider {
    fn id(&self) -> &'static str {
        "pi"
    }

    fn hosting(&self) -> Hosting {
        Hosting::Daemon
    }

    /// TWO LANES, selected by `req.interactive`.
    ///
    /// **Daemon (the default, `interactive: false`).** argv = `[<pi bin>,
    /// "--mode", "rpc"]`. NO `--listen`/port flag: pi's transport is the child's
    /// STDIO, owned by the per-session adapter daemon (item 1) — the daemon
    /// exposes the loopback reconnect endpoint, not pi. Resume (`--session
    /// <id>`) is appended by the resume path, the same altitude as codex's
    /// create-path `--listen`. Byte-identical to the pre-interactive argv, so
    /// every existing pi caller is unchanged.
    ///
    /// **Interactive (`qd start --provider pi --interactive`).** argv = `[<pi
    /// bin>, "--session-id", <id>]` — the real pi TUI, in a mux pane a human
    /// attaches to.
    ///
    /// WHY `req.resume` CARRIES THE ID ON BOTH A FRESH START AND A REVIVE, and
    /// why there is only one flag here. pi's `--session-id` is documented as "use
    /// exact project session ID, **creating it if missing**", and at source
    /// (`dist/main.js` `createSessionManager`) it opens an existing local session
    /// of that id or creates one carrying it. So the two lanes that need separate
    /// argv for codex (`codex` vs `codex resume <id>`) are ONE argv for pi, and
    /// the create path pre-mints the id rather than discovering it afterwards —
    /// see [`tui`] for why that removes codex's whole attribution apparatus.
    ///
    /// `--session-id` is REQUIRED here, never optional: a bare `pi` would let pi
    /// choose the id, putting us straight back in codex's world of a session we
    /// cannot address until it writes something. The create path always supplies
    /// one, and an interactive request that somehow arrives without it falls back
    /// to the bare TUI rather than emitting a `--session-id` with no value (an
    /// argv pi would reject at startup, killing the pane).
    ///
    /// NOT combined with `--session`/`--continue`/`--resume`/`--no-session`: pi
    /// exits 1 on `--session-id` plus any of those (`validateSessionIdFlags`), so
    /// this lane emits `--session-id` alone. `fork` has no interactive analog and
    /// is refused upstream.
    ///
    /// `PI_CODING_AGENT_SESSION_DIR` is passed through on BOTH lanes when set, so
    /// pi writes sessions into the SAME root qd reads (jails own it in tests).
    fn launch_plan(&self, fx: &ProviderFx, req: &LaunchRequest) -> LaunchPlan {
        let bin = pi_bin(fx.env);
        let argv = if req.interactive {
            match req.resume.as_deref().filter(|s| !s.is_empty()) {
                Some(id) => vec!["--session-id".to_string(), id.to_string()],
                None => vec![],
            }
        } else {
            vec!["--mode".to_string(), "rpc".to_string()]
        };
        let argv = std::iter::once(bin).chain(argv).collect();
        let env = match fx.env.var(PI_SESSION_DIR_ENV).filter(|d| !d.is_empty()) {
            Some(d) => vec![(PI_SESSION_DIR_ENV.to_string(), d)],
            None => vec![],
        };
        LaunchPlan { argv, env }
    }

    /// Readiness = a `get_state` round-trip OK via `fx.pi_rpc` (the codex
    /// `InitializeWaiter` analog). The same response binds the birth-id
    /// (`state.session_id`) at the create site. ~2s timeout lives in the driver
    /// (PA1: max observed 788ms, live sample 595ms — ample margin). Panics with
    /// a clear message if `pi_rpc` is absent (the `expect` posture a caller
    /// driving pi boot MUST satisfy — the daemon connects it first).
    fn boot_waiter<'a>(&self, fx: &'a ProviderFx<'a>) -> Box<dyn crate::create::BootWaiter + 'a> {
        let rpc = fx
            .pi_rpc
            .expect("PiProvider::boot_waiter requires fx.pi_rpc (the connected pi stdio rpc)");
        Box::new(GetStateWaiter { rpc })
    }

    /// pi's NATIVE status signals. Two shapes are accepted permissively:
    ///   - a `get_state` `data` object → `isStreaming:true`→Busy, `false`→Idle.
    ///   - a streaming event object → `type:"agent_start"`→Busy,
    ///     `type:"agent_end"`→Idle.
    /// Anything else (incl. a claude status STRING — the cross-feed negative
    /// control) → `None` (caller fallback). **OPTION B (moved base):** this
    /// ON-READ point read IS the status path — the former event→sink push was
    /// burned by P4DB (`d44e869`); pid is NEVER read (R3).
    fn parse_status(&self, raw: &Value) -> Option<SessionStatus> {
        let obj = raw.as_object()?;
        if let Some(streaming) = obj.get("isStreaming").and_then(Value::as_bool) {
            return Some(if streaming {
                SessionStatus::Busy
            } else {
                SessionStatus::Idle
            });
        }
        match obj.get("type").and_then(Value::as_str) {
            Some("agent_start") => Some(SessionStatus::Busy),
            Some("agent_end") => Some(SessionStatus::Idle),
            _ => None,
        }
    }

    /// `$PI_CODING_AGENT_SESSION_DIR` else `$HOME/.pi/agent/sessions`, off
    /// `fx.env` only. A wrong/missing root is a permissive miss in the readers,
    /// never a crash (the `.pi/agent/sessions` placeholder only bites a no-HOME
    /// caller, which a real run never is).
    fn transcript_root(&self, fx: &ProviderFx) -> PathBuf {
        pi_sessions_root(fx).unwrap_or_else(|| PathBuf::from(".pi/agent/sessions"))
    }

    /// Resolve a session's JSONL path by `key.id` under `state_root` (the
    /// sessions root), **lazy-write tolerant**: a `None` here means the file is
    /// not yet flushed (pre-first-assistant-message), NOT that the session is
    /// absent — `scan_transcripts`/the daemon must not treat it as "no session".
    /// When `key.cwd` is known we look only in that `--<enc-cwd>--` dir, else we
    /// scan all. NO pid is read.
    fn transcript_path(&self, state_root: &Path, key: &SessionKey) -> Option<PathBuf> {
        session::find_session_file(state_root, key.id, key.cwd)
    }

    /// Walk pi's session store → [`TranscriptMeta`], permissive (L8), covering
    /// BOTH layouts pi writes (see [`session::find_session_file`] for the full
    /// contract and the live evidence):
    ///
    ///   - **BUCKETED** — `<root>/--<enc-cwd>--/<ts>_<id>.jsonl`, the default
    ///     `$HOME/.pi/agent/sessions` shape. `project_dir` carries the
    ///     `--<enc-cwd>--` DIR NAME (the shape contrast with codex's date bucket).
    ///   - **FLAT** — `<root>/<ts>_<id>.jsonl`, what pi writes whenever the
    ///     session dir is handed to it explicitly (`--session-dir` or
    ///     `PI_CODING_AGENT_SESSION_DIR`). `project_dir` is the ROOT's own
    ///     directory name: there is no per-cwd bucket to name, and inventing one
    ///     would claim a cwd the file does not record.
    ///
    /// Scanning only the bucketed layout meant a store qd itself had pinned via
    /// the env var contributed ZERO cold rows — silently, since an empty scan is
    /// also what a genuinely empty store returns.
    ///
    /// session id = the uuid parsed out of the filename. A missing root / the
    /// lazy-write window contributes nothing (not an error).
    fn scan_transcripts(&self, state_root: &Path) -> Vec<TranscriptMeta> {
        fn collect(dir: &Path, bucket: &str, out: &mut Vec<TranscriptMeta>) {
            let Ok(files) = std::fs::read_dir(dir) else {
                return;
            };
            for f in files.flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                let Some(parsed) = session::parse_filename(&fname) else {
                    continue;
                };
                let mtime_ms = f
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                out.push(TranscriptMeta {
                    session_id: parsed.session_id,
                    path: f.path(),
                    mtime_ms,
                    project_dir: bucket.to_string(),
                });
            }
        }

        let mut out = Vec::new();
        // FLAT: session files sitting directly in the root.
        let root_bucket = state_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        collect(state_root, &root_bucket, &mut out);
        // BUCKETED: one `--<enc-cwd>--` dir per project.
        let Ok(dirs) = std::fs::read_dir(state_root) else {
            return out;
        };
        for dir in dirs.flatten() {
            if !dir.path().is_dir() {
                continue;
            }
            let bucket = dir.file_name().to_string_lossy().into_owned();
            collect(&dir.path(), &bucket, &mut out);
        }
        out
    }

    /// Session-JSONL stats. TODO(tier-a): pi's line taxonomy differs from the
    /// claude transcript the generic reader assumes (header + `message` entries
    /// vs claude turns), so turns/tokens need a pi-specific pass. For the
    /// skeleton this delegates to the generic reader, which already degrades a
    /// missing/torn file safely — correct for line/preview, approximate for
    /// turns until the pi pass lands.
    fn transcript_stats(&self, path: &Path, include_preview: bool) -> JsonlStats {
        crate::jsonl::read_stats(path, include_preview)
    }

    /// The pi CLI resume fragment: `["--fork", <id>]` when forking, else
    /// `["--session", <id>]` (verified against `pi --help` 0.80.2). The DAEMON-
    /// path resume is the `switch_session` RPC at the verb layer, NOT this argv.
    /// pid is NEVER read.
    fn resume_args(&self, key: &SessionKey, fork: bool) -> Vec<String> {
        if fork {
            vec!["--fork".to_string(), key.id.to_string()]
        } else {
            vec!["--session".to_string(), key.id.to_string()]
        }
    }

    /// The SEND op. Drives `fx.pi_rpc.prompt` with a `streamingBehavior` chosen
    /// from the resident's OWN live turn state: `get_state().is_streaming` true
    /// (BUSY) routes `FollowUp` — pi QUEUES the message and runs it when the open
    /// turn closes; false (IDLE) routes `Steer`, which on an idle resident is
    /// simply a fresh turn.
    ///
    /// **Why not always `Steer` (the retired option-(i) single call).** `Steer`
    /// INTERRUPTS the open turn (`rpc.rs` `StreamingBehavior` doc) — it does not
    /// queue. A steer that arrives while the resident is mid-tool-call can be
    /// discarded by pi with a `success:true` reply, so the send is ACKed and lost:
    /// qd writes its non-terminal `turn-accepted` and nothing ever contradicts it.
    /// `FollowUp` puts the queue in the RESIDENT, where it survives this
    /// short-lived CLI process (a queue held in qd would die with the terminal it
    /// was typed into).
    ///
    /// The probe is read HERE, through the rpc `inject` already holds, rather than
    /// plumbed in through `fx` like codex's `codex_expected_turn_id`. That
    /// contrast is deliberate: codex's believed state comes from a lagging
    /// on-disk rollout tail the trait cannot read, whereas pi's is server truth
    /// one round-trip away — reading it at the last possible moment is what keeps
    /// the busy window closed. A FAILED probe degrades to `Steer` (the historical
    /// behavior): a dead transport fails the `prompt` on the next line anyway.
    ///
    /// Returns the minted command id (the attributable turn id — the
    /// `Provider::inject` contract). `fx.pi_rpc` REQUIRED, else
    /// `InjectError::NoTransport`. `from` is unused (pi turns have no relay-from).
    /// pid is NEVER read.
    fn inject(
        &self,
        fx: &ProviderFx,
        key: &SessionKey,
        message: &str,
        _from: &str,
    ) -> Result<String, InjectError> {
        let rpc = fx
            .pi_rpc
            .ok_or_else(|| InjectError::NoTransport(key.id.to_string()))?;
        // Server-truth busy read; an unreadable state degrades to Steer.
        let behavior = match rpc.get_state() {
            Ok(state) if state.is_streaming => StreamingBehavior::FollowUp,
            Ok(_) | Err(_) => StreamingBehavior::Steer,
        };
        rpc.prompt(message, Some(behavior)).map_err(|e| match e {
            // A dead stdin/closed pi = no transport (the daemon is gone).
            PiRpcError::Transport(_) | PiRpcError::Closed => {
                InjectError::NoTransport(key.id.to_string())
            }
            // A `success:false` / timeout = a precondition failure (PA9: a
            // refused prompt fires no agent_* so the sink stays idle).
            other => InjectError::Precondition(format!("send failed: {other}")),
        })
    }
}

/// The pi boot waiter: readiness is a `get_state` round-trip landing OK over the
/// connected `fx.pi_rpc` (the codex `InitializeWaiter` analog). Holds the
/// borrowed `&dyn PiRpc` out of `fx` (lifetime bounded by `fx`).
struct GetStateWaiter<'a> {
    rpc: &'a dyn PiRpc,
}

impl crate::create::BootWaiter for GetStateWaiter<'_> {
    fn wait_ready(&self, name: &str) -> Result<(), crate::boot::BootFailure> {
        match self.rpc.get_state() {
            // The create site reads `state.session_id` off the same probe to bind
            // the birth-id; here readiness is simply the round-trip landing.
            Ok(_state) => Ok(()),
            Err(e) => Err(crate::boot::BootFailure {
                // PidFile is the closest existing phase; pi has no pid-file boot —
                // readiness IS the get_state round-trip (the detail names it).
                phase: crate::boot::BootPhase::PidFile,
                detail: format!("pi session \"{name}\" get_state readiness probe failed: {e}"),
            }),
        }
    }
}

// ===========================================================================
// The busy-routing contract (the qd-drops-messages-to-busy-sessions fix).
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use crate::provider::pi::rpc::{PiEvent, RpcSessionState};
    use crate::provider::Provider;
    use std::cell::RefCell;

    /// A PiRpc that reports a PROGRAMMED `is_streaming` (or a failing probe) and
    /// records the `streamingBehavior` `inject` routed with.
    struct RoutingRpc {
        state: Option<bool>, // None = the probe itself fails
        routed: RefCell<Vec<Option<StreamingBehavior>>>,
    }

    impl RoutingRpc {
        fn new(state: Option<bool>) -> Self {
            Self {
                state,
                routed: RefCell::new(Vec::new()),
            }
        }
        fn routed(&self) -> Option<StreamingBehavior> {
            self.routed.borrow().first().copied().flatten()
        }
    }

    impl PiRpc for RoutingRpc {
        fn get_state(&self) -> Result<RpcSessionState, PiRpcError> {
            match self.state {
                Some(is_streaming) => Ok(RpcSessionState {
                    is_streaming,
                    ..Default::default()
                }),
                None => Err(PiRpcError::Timeout),
            }
        }
        fn prompt(
            &self,
            _m: &str,
            behavior: Option<StreamingBehavior>,
        ) -> Result<String, PiRpcError> {
            self.routed.borrow_mut().push(behavior);
            Ok("c1".to_string())
        }
        fn steer(&self, _m: &str) -> Result<(), PiRpcError> {
            Err(PiRpcError::Closed)
        }
        fn follow_up(&self, _m: &str) -> Result<(), PiRpcError> {
            Err(PiRpcError::Closed)
        }
        fn abort(&self) -> Result<(), PiRpcError> {
            Err(PiRpcError::Closed)
        }
        fn switch_session(&self, _p: &str) -> Result<(), PiRpcError> {
            Err(PiRpcError::Closed)
        }
        fn new_session(&self, _p: Option<&str>) -> Result<(), PiRpcError> {
            Err(PiRpcError::Closed)
        }
        fn next_event(&self, _t: std::time::Duration) -> Result<Option<PiEvent>, PiRpcError> {
            Ok(None)
        }
        fn close(&self) -> Result<(), PiRpcError> {
            Ok(())
        }
    }

    fn route_with(state: Option<bool>) -> (Option<StreamingBehavior>, Result<String, InjectError>) {
        let rpc = RoutingRpc::new(state);
        let env = MapEnv::default();
        let paths = crate::paths::QdPaths::from_home(std::path::Path::new("/nonexistent"));
        let rpc_ref: &dyn PiRpc = &rpc;
        let fx = ProviderFx {
            await_relay: None,
            env: &env,
            paths: &paths,
            socket_dir: paths.sessions_dir.clone(),
            mux: None,
            clock: None,
            sleeper: None,
            relay: None,
            relay_port: None,
            app_server: None,
            codex_expected_turn_id: None,
            acp_client: None,
            pi_rpc: Some(rpc_ref),
            acp_pre_dispatch: None,
        };
        let key = SessionKey {
            id: "s-1",
            name: Some("pi-1"),
            cwd: None,
            pid: Some(4242),
        };
        let out = PiProvider.inject(&fx, &key, "the message", "from");
        (rpc.routed(), out)
    }

    /// The FIX: a BUSY resident routes `FollowUp` — pi queues the message and runs
    /// it when the open turn closes. The retired always-`Steer` INTERRUPTED the
    /// open turn, which pi can discard while ACKing `success:true` — an accepted,
    /// silently lost message.
    #[test]
    fn busy_resident_routes_follow_up_not_steer() {
        let (routed, out) = route_with(Some(true));
        assert_eq!(routed, Some(StreamingBehavior::FollowUp));
        assert_eq!(out.unwrap(), "c1");
    }

    /// An IDLE resident keeps `Steer`, which on an idle pi is simply a fresh turn
    /// (the historical behavior on the only lane where it was ever correct).
    #[test]
    fn idle_resident_routes_steer() {
        let (routed, _) = route_with(Some(false));
        assert_eq!(routed, Some(StreamingBehavior::Steer));
    }

    /// A FAILED probe degrades to `Steer` — never a refusal, never a silent skip:
    /// a transport too sick to answer `get_state` fails the `prompt` on the next
    /// line anyway, and that failure is the loud one worth surfacing.
    #[test]
    fn unreadable_state_degrades_to_steer() {
        let (routed, out) = route_with(None);
        assert_eq!(routed, Some(StreamingBehavior::Steer));
        assert!(
            out.is_ok(),
            "a failed probe must not, by itself, fail the send"
        );
    }

    /// The routing decision is read through the rpc `inject` holds, NOT plumbed in
    /// through `fx` — so it is server truth at the last possible moment. Pinned so
    /// a future refactor cannot quietly reintroduce a believed-state gap here.
    #[test]
    fn routing_probes_the_live_rpc_every_inject() {
        let rpc = RoutingRpc::new(Some(true));
        let env = MapEnv::default();
        let paths = crate::paths::QdPaths::from_home(std::path::Path::new("/nonexistent"));
        let rpc_ref: &dyn PiRpc = &rpc;
        let fx = ProviderFx {
            await_relay: None,
            env: &env,
            paths: &paths,
            socket_dir: paths.sessions_dir.clone(),
            mux: None,
            clock: None,
            sleeper: None,
            relay: None,
            relay_port: None,
            app_server: None,
            codex_expected_turn_id: None,
            acp_client: None,
            pi_rpc: Some(rpc_ref),
            acp_pre_dispatch: None,
        };
        let key = SessionKey {
            id: "s-1",
            name: None,
            cwd: None,
            pid: None,
        };
        let _ = PiProvider.inject(&fx, &key, "a", "f");
        let _ = PiProvider.inject(&fx, &key, "b", "f");
        assert_eq!(
            *rpc.routed.borrow(),
            vec![
                Some(StreamingBehavior::FollowUp),
                Some(StreamingBehavior::FollowUp)
            ],
            "every inject re-reads; no cached belief"
        );
    }

    // === Cold-scan layout coverage (main) ===

    /// The cold scan must see BOTH of pi's layouts (see
    /// [`session::find_session_file`] for the contract and the live evidence).
    ///
    /// The flat half is the one that was missing, and it is the one qd's own
    /// configuration produces: a store pinned through
    /// `PI_CODING_AGENT_SESSION_DIR` contributed ZERO cold rows, silently — an
    /// empty scan is also what a genuinely empty store returns.
    #[test]
    fn scan_transcripts_sees_both_the_flat_and_the_bucketed_layout() {
        let root = tempfile::tempdir().unwrap();
        let bucket = root.path().join(session::encode_cwd_dir("/w"));
        std::fs::create_dir_all(&bucket).unwrap();
        std::fs::write(bucket.join("2026-08-07T00-00-00-000Z_bucketed.jsonl"), "{}").unwrap();
        std::fs::write(root.path().join("2026-08-07T00-00-01-000Z_flat.jsonl"), "{}").unwrap();
        // A non-session file in each place must be skipped, not counted.
        std::fs::write(root.path().join("notes.txt"), "hi").unwrap();
        std::fs::write(bucket.join("notes.txt"), "hi").unwrap();

        let mut got: Vec<(String, String)> = PI_PROVIDER
            .scan_transcripts(root.path())
            .into_iter()
            .map(|m| (m.session_id, m.project_dir))
            .collect();
        got.sort();

        assert_eq!(got.len(), 2, "both layouts contribute exactly one row each");
        assert_eq!(got[0].0, "bucketed");
        assert_eq!(
            got[0].1,
            session::encode_cwd_dir("/w"),
            "a bucketed row names its cwd bucket"
        );
        assert_eq!(got[1].0, "flat");
        assert_eq!(
            got[1].1,
            root.path().file_name().unwrap().to_string_lossy(),
            "a flat row names the ROOT — there is no per-cwd bucket, and inventing \
             one would claim a cwd the file does not record"
        );
    }

    #[test]
    fn scan_transcripts_of_a_missing_root_is_empty_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        assert!(PI_PROVIDER
            .scan_transcripts(&root.path().join("nope"))
            .is_empty());
    }
}
