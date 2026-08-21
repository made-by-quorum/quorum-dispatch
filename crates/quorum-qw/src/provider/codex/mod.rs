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
//!   - [`tui`]     — codex-interactive: identity discovery + the boot waiter for
//!     a codex TUI hosted in a mux pane (`--interactive`), which speaks no
//!     protocol to qd, so its thread id is OBSERVED from the rollout it opens.
//!
//! Wire truth is the LAW: every envelope mirrors a cited line of the q1c spike
//! evidence (exec/codex-spike-evidence/jail/), NOT textbook JSON-RPC — the
//! frames carry no `jsonrpc` field (see [`rpc`]).
//!
//! Physical layout (harness-first reorg, PROVIDER-REORG-SPEC.md): the eight
//! submodules above are grouped by transport into three directories —
//! [`app_server`] (the ws JSON-RPC contract: `rpc`/`ws`/`version`/`resume`),
//! [`pty`] (the mux-pane TUI lane: `pane`/`tui`), and [`store`] (read-only
//! rollout/sqlite readers: `rollout`/`index`). The flat paths named above and
//! used throughout `dispatch` are preserved unchanged via `pub use` re-exports
//! immediately below — see the comment there.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::jsonl::{JsonlStats, TranscriptMeta};
use crate::model::SessionStatus;
use crate::provider::shared::attribution;
use crate::provider::{
    Hosting, InjectError, LaunchPlan, LaunchRequest, Provider, ProviderFx, SessionKey,
};

pub mod app_server;
pub mod pty;
pub mod store;

// ===========================================================================
// Compatibility re-exports — PRESERVE THE PRE-REORG FLAT PATHS.
//
// Before the harness-first reorg (PROVIDER-REORG-SPEC.md), `rpc`/`ws`/
// `version`/`resume`/`pane`/`tui`/`rollout`/`index` were direct submodules of
// `provider::codex`. They now live under `app_server/`, `pty/`, and `store/`,
// but `dispatch` has ~200 call sites written against the OLD flat paths
// (`crate::provider::codex::rpc::X`, `::pane::X`, `::rollout::X`, …). These
// re-exports make every old path resolve exactly as before — DO NOT remove
// them without updating every such call site first.
// ===========================================================================
pub use self::app_server::{resume, rpc, version, ws};
pub use self::pty::{pane, tui};
pub use self::store::{index, rollout};

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
const CODEX_BIN_ENV: &str = "QD_CODEX_BIN";
/// The `sessions/` subdir of `$CODEX_HOME` — the rollout tree root.
const SESSIONS_SUBDIR: &str = "sessions";

// NOTE (W4): the qd-launched thread/start approval posture (codex-p2-spec
// section 3.3) is a CREATE-PATH concern — it is passed to
// `AppServerRpc::thread_start(cwd, approval_policy, sandbox)` by the W4 create
// sequence, NOT by this W3 launch_plan (which only builds the daemon argv). The
// constants live with that caller, not here.
//
// R22 (2026-08-20): that posture is now SAFE BY DEFAULT (`on-request` +
// `workspace-write`), following claude's `DEFAULT_FLAGS` off
// `--dangerously-skip-permissions`. The old full-bypass pair (`never` +
// `danger-full-access`) is opt-in behind `QD_CODEX_DANGER_FULL_ACCESS=1`. The
// rationale — and the warning not to "restore parity" by reverting it — lives at
// the constants in `app_server/create.rs`.

/// The codex binary to launch: the `QD_CODEX_BIN` override, else `"codex"` off
/// PATH. Shared so the launch plan and the human viewer
/// (`verbs::lifecycle::attach_codex_viewer`) can never disagree about which
/// binary is "codex" on this box.
pub fn codex_bin(env: &dyn crate::effects::Env) -> String {
    env.var(CODEX_BIN_ENV)
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

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

    /// argv = `[<codex bin>, "app-server"]` where bin = `fx.env` `QD_CODEX_BIN`
    /// override else `"codex"`. NO `--listen` here: the W4 create path appends
    /// `--listen ws://127.0.0.1:<port>` because PORT ALLOCATION is a create-path
    /// transport concern (the same altitude as W5's `relay_port` — a verb-layer
    /// resolution, not a launch-plan input). env carries `CODEX_HOME` passthrough
    /// when `fx.env` has it. Consumes NO claude config surface (the minimal-fx
    /// negative control: empty env still yields bin `"codex"`).
    ///
    /// codex-interactive: `req.interactive` selects the OTHER codex launch shape
    /// — the bare `codex` TUI a human drives in a mux pane, or `codex resume
    /// <thread-id>` when re-entering an existing thread (the SAME fragment
    /// [`Provider::resume_args`] already documents as "the codex CLI fragment,
    /// TUI-facing"). The `--listen` port append stays a daemon-lane concern and
    /// never applies here: an interactive pane has no ws transport at all, which
    /// is exactly why its row records `hosting: "mux-pane"` and carries no
    /// `endpoint`.
    ///
    /// `req.interactive = false` is the DEFAULT and reproduces the pre-existing
    /// argv byte-for-byte, so the daemon lane is untouched.
    fn launch_plan(&self, fx: &ProviderFx, req: &LaunchRequest) -> LaunchPlan {
        let bin = codex_bin(fx.env);
        let argv = if req.interactive {
            let mut argv = vec![bin];
            // A fresh interactive start is a bare `codex`; a revive re-enters the
            // SAME thread. `fork` is meaningless for codex (a thread appends to
            // one rollout — no transcript branching), so resume_args ignores it
            // and the start verb refuses `--fork` for codex upstream.
            if let Some(id) = req.resume.as_deref().filter(|s| !s.is_empty()) {
                argv.extend(self.resume_args(
                    &SessionKey {
                        id,
                        name: None,
                        cwd: None,
                        pid: None,
                    },
                    false,
                ));
            }
            argv
        } else {
            vec![bin, "app-server".to_string()]
        };
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
    /// claude's missing-relay class). pid is NEVER read. NO user-facing start/steer
    /// vocabulary is produced anywhere — the verb prints SEND-only surfaces.
    ///
    /// `from` is SPENT, not dropped (punch R10). A codex turn is a plain string —
    /// there is no header field to carry a sender — so the attribution rides in
    /// the TEXT: [`attribution::attribute`] puts the relay-shaped
    /// `<channel source="qd" from_session="…">…</channel>` envelope plus its
    /// one-line reply path in front of the message whenever `from` names a real
    /// peer, and passes the message through BYTE-IDENTICAL when it is `"cli"` (an
    /// operator shell is not a peer to answer). Before that this impl took the
    /// parameter as `_from` and a delegated task arrived here unattributed, with
    /// no hint that `qd send:relay` could answer it. The envelope changes only the
    /// TEXT: the ladder below, the turn id returned, and every ledger record
    /// (which keys sha256 of the RAW message) are untouched.
    fn inject(
        &self,
        fx: &ProviderFx,
        key: &SessionKey,
        message: &str,
        from: &str,
    ) -> Result<String, InjectError> {
        let rpc = fx
            .app_server
            .ok_or_else(|| InjectError::NoTransport(key.id.to_string()))?;
        // The WIRE text: the sender envelope + the message, or the message itself
        // when there is no sender to name. Every `turn/start`/`turn/steer` below
        // sends THIS, so a steer and its fallback-start carry the same bytes.
        let wire = attribution::attribute(message, from);
        let message: &str = &wire;
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
        // believed IDLE → start a fresh turn — but the belief comes from the
        // ROLLOUT TAIL, which LAGS: a turn that started microseconds ago has not
        // flushed its `task_started`, so a genuinely BUSY thread reads as idle and
        // this `turn/start` fires into an open turn. That is the mirror of the
        // stale fence above, and it was previously unhandled — the believed-busy
        // direction self-corrected, the believed-idle direction did not.
        //
        // A refusal carrying the invalid-request code is read as "the belief was
        // wrong, the thread is busy": re-read the tail (by now the server has told
        // us a turn is active, so the record it lagged on is far likelier to be
        // present) and STEER the turn we should have steered in the first place.
        // Classification is on the CODE, never the message text — the same churn
        // posture as `INVALID_REQUEST_CODE`'s own doc, and equally
        // self-correcting: a non-busy -32600 fails the steer and the ORIGINAL
        // server error is what surfaces.
        match rpc.turn_start(key.id, message) {
            Ok(turn_id) => Ok(turn_id),
            Err(RpcError::Protocol(e)) if e.code == INVALID_REQUEST_CODE => {
                let refused = InjectError::Precondition(format!(
                    "send failed: protocol error {}: {}",
                    e.code, e.message
                ));
                let Some(open) = self.reread_open_turn_id(fx, key) else {
                    // No open turn recoverable from the tail — nothing to steer;
                    // surface the server's own refusal unchanged.
                    return Err(refused);
                };
                match rpc.turn_steer(key.id, &open, message) {
                    Ok(SteerOutcome::Steered(turn_id)) => Ok(turn_id),
                    // The re-read turn ALSO moved on. Two refusals in a row is a
                    // racing thread, not a recoverable belief — refuse loudly with
                    // the original error rather than looping the ladder.
                    Ok(SteerOutcome::StaleTurn(_)) => Err(refused),
                    Err(e2) => Err(InjectError::Precondition(format!("send failed: {e2}"))),
                }
            }
            Err(e) => Err(InjectError::Precondition(format!("send failed: {e}"))),
        }
    }
}

impl CodexProvider {
    /// Re-resolve the thread's CURRENT open turn id from its rollout tail, for the
    /// believed-idle→actually-busy arm of the send ladder. Reuses the verb layer's
    /// own resolution (`transcript_root` + `transcript_path` + [`open_turn_id`]) so
    /// the two channels cannot disagree. Permissive: an unresolvable/absent/torn
    /// rollout is `None`, never an error.
    fn reread_open_turn_id(&self, fx: &ProviderFx, key: &SessionKey) -> Option<String> {
        let root = self.transcript_root(fx);
        let path = self.transcript_path(&root, key)?;
        open_turn_id(&read_lines(&path))
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

// ===========================================================================
// The believed-idle→actually-busy arm of the send ladder (the qd-drops-messages-
// to-busy-sessions fix). The believed-BUSY direction was always self-correcting
// via the stale fence; this pins the mirror.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use std::cell::RefCell;

    const THREAD: &str = "019fdc88-c071-7431-a7c1-bec41028b3f8";
    const OPEN_TURN: &str = "T-9";

    /// An AppServerRpc whose `turn/start` returns a programmed error and whose
    /// `turn/steer` returns a programmed outcome, recording every call.
    struct LadderRpc {
        start: RefCell<Option<Result<String, RpcError>>>,
        steer: RefCell<Option<Result<SteerOutcome, RpcError>>>,
        calls: RefCell<Vec<String>>,
        /// The turn TEXT each rung was handed — the R10 envelope's witness.
        prompts: RefCell<Vec<String>>,
    }

    impl LadderRpc {
        fn new(start: Result<String, RpcError>, steer: Result<SteerOutcome, RpcError>) -> Self {
            Self {
                start: RefCell::new(Some(start)),
                steer: RefCell::new(Some(steer)),
                calls: RefCell::new(Vec::new()),
                prompts: RefCell::new(Vec::new()),
            }
        }
    }

    impl AppServerRpc for LadderRpc {
        fn initialize(&self, _c: &ClientInfo) -> Result<InitializeResult, RpcError> {
            Err(RpcError::Closed)
        }
        fn initialized(&self) -> Result<(), RpcError> {
            Ok(())
        }
        fn thread_start(&self, _c: &str, _a: &str, _s: &str) -> Result<String, RpcError> {
            Err(RpcError::Closed)
        }
        fn thread_resume(&self, _t: &str) -> Result<(), RpcError> {
            Err(RpcError::Closed)
        }
        fn turn_start(&self, _t: &str, x: &str) -> Result<String, RpcError> {
            self.calls.borrow_mut().push("turn/start".into());
            self.prompts.borrow_mut().push(x.to_string());
            self.start
                .borrow_mut()
                .take()
                .unwrap_or(Err(RpcError::Closed))
        }
        fn turn_steer(&self, _t: &str, expected: &str, x: &str) -> Result<SteerOutcome, RpcError> {
            self.calls
                .borrow_mut()
                .push(format!("turn/steer[{expected}]"));
            self.prompts.borrow_mut().push(x.to_string());
            self.steer
                .borrow_mut()
                .take()
                .unwrap_or(Err(RpcError::Closed))
        }
        fn turn_interrupt(&self, _t: &str, _u: &str) -> Result<(), RpcError> {
            Ok(())
        }
        fn next_notification(
            &self,
            _timeout: std::time::Duration,
        ) -> Result<Option<Notification>, RpcError> {
            Ok(None)
        }
        fn close(&self) -> Result<(), RpcError> {
            Ok(())
        }
    }

    fn busy_refusal() -> RpcError {
        RpcError::Protocol(ServerError {
            code: INVALID_REQUEST_CODE,
            message: "a turn is already active for this thread".to_string(),
        })
    }

    /// A CODEX_HOME whose rollout for THREAD carries an OPEN turn (`task_started`
    /// with no matching `task_complete`) — the record the lagging tail had not yet
    /// flushed at send time, and which the re-read finds.
    fn codex_home_with_open_turn(open: bool) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp
            .path()
            .join("sessions")
            .join("2026")
            .join("08")
            .join("10");
        std::fs::create_dir_all(&day).unwrap();
        let mut jsonl = String::from(
            r#"{"type":"session_meta","payload":{"id":"019fdc88-c071-7431-a7c1-bec41028b3f8"}}
{"type":"event_msg","payload":{"type":"task_started","turn_id":"T-9"}}
"#,
        );
        if !open {
            jsonl.push_str(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"T-9\"}}\n",
            );
        }
        std::fs::write(
            day.join(format!("rollout-2026-08-10T10-00-00-{THREAD}.jsonl")),
            jsonl,
        )
        .unwrap();
        tmp
    }

    fn inject_believed_idle(
        rpc: &LadderRpc,
        home: &tempfile::TempDir,
    ) -> Result<String, InjectError> {
        inject_believed_idle_from(rpc, home, "cli")
    }

    /// The ladder drive with an explicit SENDER — the R10 envelope tests need the
    /// attributed lane; every ladder test above is about routing, so it drives the
    /// `"cli"` (no-envelope) lane where the wire text is the message.
    fn inject_believed_idle_from(
        rpc: &LadderRpc,
        home: &tempfile::TempDir,
        from: &str,
    ) -> Result<String, InjectError> {
        let mut env = MapEnv::default();
        env.vars.insert(
            CODEX_HOME_ENV.to_string(),
            home.path().to_string_lossy().into_owned(),
        );
        let paths = crate::paths::QdPaths::from_home(home.path());
        let rpc_ref: &dyn AppServerRpc = rpc;
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
            app_server: Some(rpc_ref),
            // BELIEVED IDLE — the lagging tail showed no open turn.
            codex_expected_turn_id: None,
            acp_client: None,
            pi_rpc: None,
            acp_pre_dispatch: None,
        };
        let key = SessionKey {
            id: THREAD,
            name: Some("codex-1"),
            cwd: None,
            pid: Some(4242),
        };
        CodexProvider.inject(&fx, &key, "the message", from)
    }

    /// punch R10 — an attributed send reaches the codex daemon WRAPPED: the turn
    /// text carries the sender and the one-line reply path, and the body is the
    /// operator's message byte-for-byte. This is the exact string a codex session
    /// reads, so it is pinned verbatim.
    #[test]
    fn an_attributed_send_wraps_the_codex_turn_text() {
        let home = codex_home_with_open_turn(false);
        let rpc = LadderRpc::new(Ok("T-NEW".to_string()), Err(RpcError::Closed));
        assert_eq!(
            inject_believed_idle_from(&rpc, &home, "sess-A").unwrap(),
            "T-NEW",
            "the turn id is the daemon's — the envelope changes TEXT only"
        );
        assert_eq!(
            *rpc.prompts.borrow(),
            vec!["<channel source=\"qd\" from_session=\"sess-A\">\n\
                  the message\n\
                  </channel>\n\
                  [reply: qd send:relay sess-A \"<your reply>\" | peers: qd ls]"
                .to_string()]
        );
    }

    /// …and an operator at a terminal (`derive_from_session` → `"cli"`) is NOT a
    /// peer: no envelope, the message reaches the daemon byte-identical.
    #[test]
    fn a_cli_send_reaches_the_daemon_unwrapped() {
        let home = codex_home_with_open_turn(false);
        let rpc = LadderRpc::new(Ok("T-NEW".to_string()), Err(RpcError::Closed));
        inject_believed_idle_from(&rpc, &home, "cli").unwrap();
        assert_eq!(*rpc.prompts.borrow(), vec!["the message".to_string()]);
    }

    /// The wire text is decided ONCE, before the ladder branches: a steer and the
    /// stale-fence `turn/start` it falls back to carry the SAME bytes. A reader who
    /// saw the envelope on one and not the other would be reading two messages.
    #[test]
    fn every_rung_of_the_ladder_carries_the_same_wire_text() {
        let home = codex_home_with_open_turn(true);
        let rpc = LadderRpc::new(
            Err(busy_refusal()),
            Ok(SteerOutcome::Steered("T-9".to_string())),
        );
        inject_believed_idle_from(&rpc, &home, "sess-A").unwrap();
        let prompts = rpc.prompts.borrow().clone();
        assert_eq!(prompts.len(), 2, "start (refused) then steer");
        assert_eq!(prompts[0], prompts[1]);
        assert!(prompts[0].starts_with("<channel source=\"qd\" from_session=\"sess-A\">"));
    }

    /// THE FIX: believed idle, `turn/start` refused with the invalid-request code,
    /// the re-read tail now shows the open turn → STEER it. The send lands instead
    /// of failing.
    #[test]
    fn believed_idle_busy_refusal_rereads_and_steers() {
        let home = codex_home_with_open_turn(true);
        let rpc = LadderRpc::new(
            Err(busy_refusal()),
            Ok(SteerOutcome::Steered("T-9".to_string())),
        );
        assert_eq!(inject_believed_idle(&rpc, &home).unwrap(), "T-9");
        assert_eq!(
            *rpc.calls.borrow(),
            vec!["turn/start".to_string(), format!("turn/steer[{OPEN_TURN}]")],
            "the ladder re-reads the tail and steers the turn it should have steered"
        );
    }

    /// A clean `turn/start` is untouched by the new arm — no re-read, no steer.
    #[test]
    fn believed_idle_clean_start_does_not_reread() {
        let home = codex_home_with_open_turn(false);
        let rpc = LadderRpc::new(Ok("T-NEW".to_string()), Err(RpcError::Closed));
        assert_eq!(inject_believed_idle(&rpc, &home).unwrap(), "T-NEW");
        assert_eq!(*rpc.calls.borrow(), vec!["turn/start".to_string()]);
    }

    /// A refusal with NO recoverable open turn in the tail surfaces the server's
    /// own error — the arm never invents a turn to steer.
    #[test]
    fn busy_refusal_without_a_recoverable_turn_surfaces_the_refusal() {
        let home = codex_home_with_open_turn(false); // balanced tail → no open turn
        let rpc = LadderRpc::new(Err(busy_refusal()), Err(RpcError::Closed));
        let err = inject_believed_idle(&rpc, &home).unwrap_err();
        match err {
            InjectError::Precondition(s) => assert!(
                s.contains("already active"),
                "the ORIGINAL server refusal is what surfaces: {s}"
            ),
            other => panic!("expected Precondition, got {other:?}"),
        }
        assert_eq!(
            *rpc.calls.borrow(),
            vec!["turn/start".to_string()],
            "no steer attempted"
        );
    }

    /// The re-read turn ALSO moved on: two refusals in a row is a racing thread,
    /// not a recoverable belief. Refuse with the ORIGINAL error rather than
    /// looping the ladder.
    #[test]
    fn double_stale_refuses_with_the_original_error() {
        let home = codex_home_with_open_turn(true);
        let rpc = LadderRpc::new(
            Err(busy_refusal()),
            Ok(SteerOutcome::StaleTurn(ServerError {
                code: INVALID_REQUEST_CODE,
                message: "expected active turn id T-9 but found T-10".to_string(),
            })),
        );
        match inject_believed_idle(&rpc, &home).unwrap_err() {
            InjectError::Precondition(s) => assert!(s.contains("already active"), "{s}"),
            other => panic!("expected Precondition, got {other:?}"),
        }
        assert_eq!(
            rpc.calls.borrow().len(),
            2,
            "exactly one retry, never a loop"
        );
    }

    /// A NON-fence error from `turn/start` (transport, close, timeout) is NOT read
    /// as busy — it fails straight through, unchanged.
    #[test]
    fn non_fence_start_error_is_not_treated_as_busy() {
        let home = codex_home_with_open_turn(true);
        let rpc = LadderRpc::new(Err(RpcError::Timeout), Err(RpcError::Closed));
        assert!(inject_believed_idle(&rpc, &home).is_err());
        assert_eq!(
            *rpc.calls.borrow(),
            vec!["turn/start".to_string()],
            "no re-read on a non-fence error"
        );
    }
}
