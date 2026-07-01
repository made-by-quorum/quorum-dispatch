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

pub mod conformance;
pub mod daemon;
pub mod pin;
pub mod redteam;
pub mod remote;
pub mod republish;
pub mod residence;
pub mod rpc;
pub mod session;
pub mod status;
pub mod stdio;

pub use remote::PiRemote;
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

/// Resolve pi's sessions root off `fx.env` ONLY (never raw `std::env`):
/// `PI_CODING_AGENT_SESSION_DIR` if set, else `$HOME/.pi/agent/sessions`. `None`
/// when no home is resolvable (degrades to a permissive miss in the readers).
fn pi_sessions_root(fx: &ProviderFx) -> Option<PathBuf> {
    if let Some(d) = fx.env.var(PI_SESSION_DIR_ENV).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let home = fx.env.var("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".pi").join("agent").join("sessions"))
}

impl Provider for PiProvider {
    fn id(&self) -> &'static str {
        "pi"
    }

    fn hosting(&self) -> Hosting {
        Hosting::Daemon
    }

    /// argv = `[<pi bin>, "--mode", "rpc"]`; bin = `fx.env` `QD_PI_BIN` override
    /// else `"pi"`. NO `--listen`/port flag: pi's transport is the child's
    /// STDIO, owned by the per-session adapter daemon (item 1) — the daemon
    /// exposes the loopback reconnect endpoint, not pi. Resume (`--session
    /// <id>`) is appended by the resume path, the same altitude as codex's
    /// create-path `--listen`. `PI_CODING_AGENT_SESSION_DIR` is passed through
    /// when set so the daemon writes sessions into the SAME root qd reads (jails
    /// own it in tests).
    fn launch_plan(&self, fx: &ProviderFx, _req: &LaunchRequest) -> LaunchPlan {
        let bin = fx
            .env
            .var(PI_BIN_ENV)
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "pi".to_string());
        let argv = vec!["--mode".to_string(), "rpc".to_string()];
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

    /// Walk `<root>/--<enc-cwd>--/<ts>_<uuid>.jsonl` → [`TranscriptMeta`],
    /// permissive (L8). session id = the uuid parsed out of the filename; the
    /// `project_dir` slot carries the `--<enc-cwd>--` DIR NAME (the shape
    /// contrast with codex's date bucket). A missing root / the lazy-write window
    /// contributes nothing (not an error).
    fn scan_transcripts(&self, state_root: &Path) -> Vec<TranscriptMeta> {
        let mut out = Vec::new();
        let Ok(dirs) = std::fs::read_dir(state_root) else {
            return out;
        };
        for dir in dirs.flatten() {
            if !dir.path().is_dir() {
                continue;
            }
            let bucket = dir.file_name().to_string_lossy().into_owned();
            let Ok(files) = std::fs::read_dir(dir.path()) else {
                continue;
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
                    project_dir: bucket.clone(),
                });
            }
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

    /// The SEND op. Drives `fx.pi_rpc.prompt(message, Some(Steer))` — the
    /// option-(i) SINGLE call: a `prompt` with `streamingBehavior:"steer"` starts
    /// a fresh turn when idle and steers the open turn when busy, so no
    /// believed-state read is needed (contrast codex's `expectedTurnId` ladder;
    /// pi has no per-turn id to fence on). Returns the minted command id (the
    /// attributable turn id — the `Provider::inject` contract). `fx.pi_rpc`
    /// REQUIRED, else `InjectError::NoTransport`. `from` is unused (pi turns have
    /// no relay-from). pid is NEVER read.
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
        rpc.prompt(message, Some(StreamingBehavior::Steer))
            .map_err(|e| match e {
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
