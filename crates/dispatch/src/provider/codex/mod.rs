//! `provider::codex` — the codex app-server RPC layer + the [`CodexProvider`]
//! seam impl (codex-p2-spec section 6).
//!
//! W2 (codex-p2-spec section 8): the transport CONTRACT + a real ws driver +
//! version pin/sniff + envelope types. W3: [`CodexProvider`] — the `Provider`-
//! trait impl + permissive rollout/sqlite reading + conformance extension. STILL
//! NO engine call-site rewiring (join/wait/send/create stay untouched — W4/W5/W6
//! own those). Codex-specific names are CONFINED to this module tree (codex-p2-
//! spec section 11, the Phase-D naming rider): nothing here leaks a codex-specific
//! token into a provider-neutral public name (`CodexProvider` is plan-sanctioned).
//!
//! Submodules:
//!   - [`rpc`]     — [`rpc::AppServerRpc`] contract + serde envelope types +
//!     typed [`rpc::RpcError`] (codex-p2-spec section 6.2; ADD-5 pattern).
//!   - [`ws`]      — [`ws::WsAppServer`], the tungstenite blocking driver
//!     (codex-p2-spec sections 3.1, 6.3).
//!   - [`version`] — the single-source version pin ([`version::PINNED`]),
//!     `codex --version` sniff, and the pure drift [`version::verdict`]
//!     (codex-p2-spec section 3.4).
//!   - [`rollout`] — permissive rollout-JSONL reading (taxonomy, `derive_status`,
//!     `read_stats`, filename parse) — NEVER a contract (codex-p2-spec section
//!     3.4 designed-degrade).
//!   - [`index`]   — READ-ONLY best-effort `state_5.sqlite` `threads` cache
//!     (codex-p2-spec section 6.3); every failure degrades to the rollout scan.
//!
//! Wire truth is the LAW: every envelope mirrors a cited line of the q1c spike
//! evidence (exec/codex-spike-evidence/jail/), NOT textbook JSON-RPC — the
//! frames carry no `jsonrpc` field (see [`rpc`]).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::jsonl::{JsonlStats, TranscriptMeta};
use crate::model::SessionStatus;
use crate::provider::{
    Hosting, InjectError, LaunchPlan, LaunchRequest, Provider, ProviderFx, SessionKey,
};

pub mod index;
pub mod rollout;
pub mod rpc;
pub mod version;
pub mod ws;

pub use rpc::{
    AppServerRpc, ClientInfo, InitializeResult, Notification, RpcError, ServerError, SteerOutcome,
    ThreadStartResult, TurnResult, INVALID_REQUEST_CODE,
};
pub use version::{
    parse_version_stdout, sniff, verdict, SniffOutcome, Version, VersionVerdict, PINNED,
};
pub use ws::WsAppServer;

pub use index::{threads, ThreadRow};
pub use rollout::{
    derive_status, open_turn_id, read_lines, RolloutLine, RolloutName, RolloutRecord,
};

// ===========================================================================
// CodexProvider — the daemon-hosted Provider impl (codex-p2-spec section 6).
// ===========================================================================

/// The codex `app-server` provider (codex-p2-spec section 6). A unit struct —
/// codex's durable truth is the daemon + the rollout file, so the provider holds
/// no state; the live transport arrives per-call via [`ProviderFx::app_server`].
pub struct CodexProvider;

/// The ONE registered codex provider (a `'static` singleton so `provider_for` can
/// hand out `&'static dyn Provider` without allocation — the `CLAUDE_PROVIDER`
/// precedent, provider.rs).
pub static CODEX_PROVIDER: CodexProvider = CodexProvider;

/// The env var carrying the codex state root (`$CODEX_HOME`); absent ⇒
/// `$HOME/.codex` (codex's own default).
const CODEX_HOME_ENV: &str = "CODEX_HOME";
/// The env var overriding the codex binary on the launch argv (else `"codex"`).
const CODEX_BIN_ENV: &str = "SB_CODEX_BIN";
/// The `sessions/` subdir of `$CODEX_HOME` — the rollout tree root.
const SESSIONS_SUBDIR: &str = "sessions";

// NOTE (W4): the qd-launched full-bypass thread/start posture (approval policy
// `never` + sandbox `danger-full-access`, the claude `--dangerously-…` parity
// defaults, codex-p2-spec section 3.3) is a CREATE-PATH concern — it is passed to
// `AppServerRpc::thread_start(cwd, approval_policy, sandbox)` by the W4 create
// sequence, NOT by this W3 launch_plan (which only builds the daemon argv). The
// constants live with that caller, not here.

/// Resolve `$CODEX_HOME` off `fx.env` ONLY (L9a — NEVER raw `std::env`): the
/// `CODEX_HOME` var, else `$HOME/.codex` (HOME also read from `fx.env`). Returns
/// `None` if neither is set (a caller with no home cannot resolve a root).
fn codex_home(fx: &ProviderFx) -> Option<PathBuf> {
    if let Some(h) = fx.env.var(CODEX_HOME_ENV) {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    let home = fx.env.var("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".codex"))
}

impl Provider for CodexProvider {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn hosting(&self) -> Hosting {
        Hosting::Daemon
    }

    /// argv = `[<codex bin>, "app-server"]` where bin = `fx.env` `SB_CODEX_BIN`
    /// override else `"codex"`. NO `--listen` here: the W4 create path appends
    /// `--listen ws://127.0.0.1:<port>` because PORT ALLOCATION is a create-path
    /// transport concern (the same altitude as W5's `relay_port` — a verb-layer
    /// resolution, not a launch-plan input). env carries `CODEX_HOME` passthrough
    /// when `fx.env` has it. Consumes NO claude config surface (the minimal-fx
    /// negative control: empty env still yields bin `"codex"`).
    fn launch_plan(&self, fx: &ProviderFx, _req: &LaunchRequest) -> LaunchPlan {
        let bin = fx
            .env
            .var(CODEX_BIN_ENV)
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "codex".to_string());
        let argv = vec![bin, "app-server".to_string()];
        // CODEX_HOME passthrough so the spawned daemon writes rollouts into the
        // SAME root qd reads (jails own it in tests); absent ⇒ codex's default.
        let env = match fx.env.var(CODEX_HOME_ENV).filter(|h| !h.is_empty()) {
            Some(h) => vec![(CODEX_HOME_ENV.to_string(), h)],
            None => vec![],
        };
        LaunchPlan { argv, env }
    }

    /// Readiness = the `initialize` handshake OK via `fx.app_server`. Panics with
    /// a clear message if `app_server` is absent (mirroring `ClaudeProvider`'s
    /// `expect` posture for a missing boot effect — a caller driving codex boot
    /// MUST supply the connected rpc; W4 connects it in the create sequence).
    fn boot_waiter<'a>(&self, fx: &'a ProviderFx<'a>) -> Box<dyn crate::create::BootWaiter + 'a> {
        let rpc = fx.app_server.expect(
            "CodexProvider::boot_waiter requires fx.app_server (the connected app-server rpc)",
        );
        Box::new(InitializeWaiter { rpc })
    }

    /// codex's NATIVE protocol signal = a `thread/status/changed` notification
    /// OBJECT (codex-p2-spec section 3.3). The REAL wire shape (mined from
    /// `q1c-clientA-events.jsonl`):
    /// `{"method":"thread/status/changed","params":{"threadId":..,"status":{"type":
    /// "idle"|"active"|"notLoaded"|"systemError","activeFlags":[..]}}}`. Map on
    /// `params.status.type`: `idle`→Idle; `active`→Busy (REGARDLESS of
    /// activeFlags); `notLoaded`→None; `systemError`→None (caller fallback). A
    /// claude registry STRING → None (the cross-feed negative control).
    fn parse_status(&self, raw: &Value) -> Option<SessionStatus> {
        let obj = raw.as_object()?;
        // A claude registry status STRING is not an object → None already.
        if obj.get("method")?.as_str()? != "thread/status/changed" {
            return None;
        }
        let status_type = obj.get("params")?.get("status")?.get("type")?.as_str()?;
        match status_type {
            "idle" => Some(SessionStatus::Idle),
            // active → Busy regardless of activeFlags (the approvals-as-data feed
            // is noted, not surfaced — codex-p2-spec section 3.3).
            "active" => Some(SessionStatus::Busy),
            // notLoaded / systemError → None (caller fallback; no enum variant
            // fits systemError — the named residual, delivery-acks will carry it).
            _ => None,
        }
    }

    /// `$CODEX_HOME/sessions` resolved off `fx.env` ONLY (codex-p2-spec section
    /// 6.4 / L9a). `$CODEX_HOME` else `$HOME/.codex`; an unresolvable home yields
    /// `.codex/sessions` under an empty base only as a last-resort placeholder
    /// (a real run always has HOME) — but `transcript_*` degrade on a missing
    /// dir, so a wrong root is a permissive miss, never a crash.
    fn transcript_root(&self, fx: &ProviderFx) -> PathBuf {
        codex_home(fx)
            .unwrap_or_else(|| PathBuf::from(".codex"))
            .join(SESSIONS_SUBDIR)
    }

    /// Resolve a thread's rollout path (codex-p2-spec section 6.4): the sqlite
    /// `rollout_path` (best-effort, via [`index`] against `state_root`'s parent
    /// codex-home) when resolvable, else a date-walk filename match on `key.id`
    /// under `state_root`. NO cwd component is EVER derived from `key.cwd` (the
    /// negative control asserts a cwd in the key never appears in the path).
    /// `state_root` is the `sessions/` dir (this provider's `transcript_root`).
    fn transcript_path(&self, state_root: &Path, key: &SessionKey) -> Option<PathBuf> {
        // Tier 1: the sqlite index, keyed on the parent codex-home (sessions/'s
        // parent IS $CODEX_HOME, which holds state_5.sqlite). Best-effort.
        if let Some(codex_home) = state_root.parent() {
            if let Some(p) = index::rollout_path_for(codex_home, key.id) {
                let pb = PathBuf::from(p);
                if pb.exists() {
                    return Some(pb);
                }
            }
        }
        // Tier 2: date-walk the rollout tree for a filename whose parsed uuid ==
        // key.id. NO cwd-slug ANYWHERE (date+uuid keyed — rename-safe).
        date_walk_for_id(state_root, key.id)
    }

    /// Rollout-dir walk → [`TranscriptMeta`] (codex-p2-spec section 6.4),
    /// permissive (L8). Walks `<root>/YYYY/MM/DD/rollout-*.jsonl`; each file's
    /// session id = the uuid parsed out of the filename (NOT a cwd slug). A
    /// missing/garbage tree contributes nothing.
    fn scan_transcripts(&self, state_root: &Path) -> Vec<TranscriptMeta> {
        let mut out = Vec::new();
        let Ok(years) = std::fs::read_dir(state_root) else {
            return out;
        };
        for y in years.flatten() {
            let Ok(months) = std::fs::read_dir(y.path()) else {
                continue;
            };
            for mo in months.flatten() {
                let Ok(days) = std::fs::read_dir(mo.path()) else {
                    continue;
                };
                for d in days.flatten() {
                    let bucket = format!(
                        "{}/{}/{}",
                        y.file_name().to_string_lossy(),
                        mo.file_name().to_string_lossy(),
                        d.file_name().to_string_lossy()
                    );
                    let Ok(files) = std::fs::read_dir(d.path()) else {
                        continue;
                    };
                    for f in files.flatten() {
                        let fname = f.file_name().to_string_lossy().into_owned();
                        // Parse the rollout filename → (ts, uuid). A non-rollout
                        // file degrades to a skip (permissive).
                        let Some(parsed) = rollout::parse_filename(&fname) else {
                            continue;
                        };
                        let mtime_ms = f
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|dd| dd.as_millis() as i64)
                            .unwrap_or(0);
                        out.push(TranscriptMeta {
                            // session id = the thread uuid (NOT a cwd slug).
                            session_id: parsed.id,
                            path: f.path(),
                            mtime_ms,
                            // The "project dir" slot carries the DATE bucket
                            // (date-keyed), never a cwd slug — the shape contrast.
                            project_dir: bucket.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    /// Rollout taxonomy stats (codex-p2-spec section 6.4) — delegates to
    /// [`rollout::read_stats`] (lines, last ts, preview from the last
    /// agent_message). Permissive / compressed-rollout-degrade aware.
    fn transcript_stats(&self, path: &Path, include_preview: bool) -> JsonlStats {
        rollout::read_stats(path, include_preview)
    }

    /// `["resume", <thread-id>]` — the codex CLI fragment (TUI-facing). The
    /// `fork` arg is IGNORED (a daemon thread appends to the same rollout, no
    /// fork-session concept). DAEMON-path resume is the `thread/resume` RPC at the
    /// verb layer (W6), NOT this argv. pid is NEVER read.
    fn resume_args(&self, key: &SessionKey, _fork: bool) -> Vec<String> {
        vec!["resume".to_string(), key.id.to_string()]
    }

    /// The SEND ladder (codex-p2-spec section 7.5; ADD-23(4): the user op is SEND,
    /// the start/steer envelopes are PROVIDER-INTERNAL). Drives, via the connected
    /// `fx.app_server`, the believed-state ladder:
    ///   - believed BUSY — `fx.codex_expected_turn_id = Some(T)` (the verb layer
    ///     read the rollout tail's open turn id) → `turn/steer{expectedTurnId:T}`.
    ///     On a landed steer → that turn's id. On the server's stale-`expectedTurnId`
    ///     fence ([`SteerOutcome::StaleTurn`] — the turn moved on between the tail
    ///     read and the steer) → FALL BACK to `turn/start` (race-safe by
    ///     construction: the fence is server-side, so a swallowed prompt is
    ///     impossible — the fallback always reaches the now-current state).
    ///   - believed IDLE — `fx.codex_expected_turn_id = None` → `turn/start`.
    ///
    /// Returns the turn id (the `Provider::inject` contract). `fx.app_server`
    /// REQUIRED — else `InjectError::NoTransport(key id)` (the codex equivalent of
    /// claude's missing-relay class). `from` is unused (codex turns have no
    /// relay-from). pid is NEVER read. NO user-facing start/steer vocabulary is
    /// produced anywhere — the verb prints SEND-only surfaces.
    fn inject(
        &self,
        fx: &ProviderFx,
        key: &SessionKey,
        message: &str,
        _from: &str,
    ) -> Result<String, InjectError> {
        let rpc = fx
            .app_server
            .ok_or_else(|| InjectError::NoTransport(key.id.to_string()))?;
        // believed BUSY → steer the open turn; on the stale fence, fall back to a
        // fresh turn (the prompt-swallow class is closed at the protocol level).
        if let Some(expected) = fx.codex_expected_turn_id {
            match rpc.turn_steer(key.id, expected, message) {
                Ok(SteerOutcome::Steered(turn_id)) => return Ok(turn_id),
                Ok(SteerOutcome::StaleTurn(_)) => {
                    // The fence fired: the believed-open turn moved on. Start a
                    // fresh turn against the now-current state.
                    return rpc
                        .turn_start(key.id, message)
                        .map_err(|e| InjectError::Precondition(format!("send failed: {e}")));
                }
                Err(e) => return Err(InjectError::Precondition(format!("send failed: {e}"))),
            }
        }
        // believed IDLE → start a fresh turn.
        rpc.turn_start(key.id, message)
            .map_err(|e| InjectError::Precondition(format!("send failed: {e}")))
    }
}

/// Date-walk `<root>/YYYY/MM/DD/rollout-*.jsonl` for the file whose parsed uuid
/// matches `id` (codex-p2-spec section 6.4 transcript_path tier 2). Permissive:
/// a missing/garbage tree → `None`. NO cwd component is consulted.
fn date_walk_for_id(state_root: &Path, id: &str) -> Option<PathBuf> {
    let years = std::fs::read_dir(state_root).ok()?;
    for y in years.flatten() {
        let Ok(months) = std::fs::read_dir(y.path()) else {
            continue;
        };
        for mo in months.flatten() {
            let Ok(days) = std::fs::read_dir(mo.path()) else {
                continue;
            };
            for d in days.flatten() {
                let Ok(files) = std::fs::read_dir(d.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    let fname = f.file_name().to_string_lossy().into_owned();
                    if let Some(parsed) = rollout::parse_filename(&fname) {
                        if parsed.id == id {
                            return Some(f.path());
                        }
                    }
                }
            }
        }
    }
    None
}

/// The codex boot waiter: readiness is the `initialize` handshake landing OK over
/// the connected `fx.app_server` (codex-p2-spec section 6.4). Holds the borrowed
/// `&dyn AppServerRpc` out of `fx` (the waiter's lifetime is bounded by `fx`).
struct InitializeWaiter<'a> {
    rpc: &'a dyn AppServerRpc,
}

impl crate::create::BootWaiter for InitializeWaiter<'_> {
    fn wait_ready(&self, name: &str) -> Result<(), crate::boot::BootFailure> {
        let client = ClientInfo {
            name: "qd-manager".to_string(),
            title: None,
            version: "0".to_string(),
        };
        match self.rpc.initialize(&client) {
            Ok(_) => {
                // The spike follows initialize with a bare `initialized`
                // notification; best-effort (a failure here does not un-ready us).
                let _ = self.rpc.initialized();
                Ok(())
            }
            Err(e) => Err(crate::boot::BootFailure {
                // PidFile is the closest existing phase; the detail names the
                // handshake (codex has no pid-file boot — readiness is the rpc).
                phase: crate::boot::BootPhase::PidFile,
                detail: format!("codex session \"{name}\" initialize handshake failed: {e}"),
            }),
        }
    }
}
