//! `provider/acp/` — the ACP spine (A2 §A3-ACP) + the live Claude Code adapter.
//!
//! The delivery layer's preferred final-hop: a shared headless ACP JSON-RPC client and an
//! [`Provider`](crate::provider::Provider) that drives a harness over the Agent Client
//! Protocol (ACP v1). Scope for **scoped-ACP-CC** is the ACP spine + the **Claude Code**
//! adapter ONLY (codex/pi/opencode native adapters are deferred, behind the same protocol).
//!
//! Module layout mirrors `provider/codex/`:
//! - [`rpc`] — the `AcpClient` transport CONTRACT (ADD-5) + wire types (`StopReason`, the
//!   SC-2a `TerminalReason`/`Confidence`, the demux `AcpEvent`).
//! - [`queue`] — the **SC-1** bounded per-session outbound queue (net-new) + the SC-1a wedge
//!   surface (`cancel_and_drain`/`teardown`).
//! - `client` (next increment) — the headless `AcpHost`: spawns the `claude-code-acp` bridge,
//!   owns the SC-5 single-reader demux, hosts the SC-1 queue, tees `session/update` to Pete's
//!   transcript view.
//! - `completion` (next increment) — `AcpTurnCompletion: TurnCompletion` (SC-2 RE-ANCHORED:
//!   `Visible` + the SC-2a reason on the correlated terminal `StopReason`).
//!
//! STEP-0 (live, 2026-06-25): the official `@zed-industries/claude-code-acp@0.16.2` bridge is
//! confirmed faithful on this box (`~/work/acp-cc-coord/STEP0-FINDING.md`). Two load-bearing
//! findings drive the build: (1) the bridge must be spawned with `CLAUDECODE`/
//! `CLAUDE_CODE_SESSION_ID` UNSET (nesting guard) or `session/new` fails; (2) the bridge never
//! emits `StopReason::refusal` (R8) — refusal handling is not gated on it.

pub mod client;
pub mod completion;
pub mod daemon;
pub mod ladder;
pub mod queue;
pub mod residence;
pub mod resume;
pub mod rpc;
pub mod wire;

pub use client::AcpHost;
pub use completion::{
    AcpTurnCompletion, AcpTurnObservation, AcpTurnObserver, ProgressPhase, TurnProgress,
};
pub use ladder::{classify_connect_failure, derive_tier, ConnectFailure, Tier, PTY_TRANSPORT};
pub use wire::{serve, AcpConnection, AcpResident};

pub use queue::{CompleteOutcome, DrainOutcome, OutboundQueue, QueuedInject, TeardownOutcome};
pub use rpc::{
    AcpClient, AcpError, AcpEvent, Confidence, InitializeResult, StopReason, TerminalReason,
};

use std::path::{Path, PathBuf};

use crate::boot::{BootFailure, BootPhase};
use crate::create::BootWaiter;
use crate::jsonl::{JsonlStats, TranscriptMeta};
use crate::model::SessionStatus;
use crate::provider::{
    Hosting, InjectError, LaunchPlan, LaunchRequest, Provider, ProviderFx, SessionKey,
};

/// Environment variables that MUST be stripped from the `claude-code-acp` bridge child's
/// environment at spawn (STEP-0 load-bearing finding #1, primary-verified live on this box):
/// the bridge runs the real Claude Code engine via the Agent SDK, which refuses to launch
/// "inside another Claude Code session" when `CLAUDECODE` is set — so `session/new` fails with
/// `Query closed before response received` unless these are removed. dispatch itself runs as a
/// Claude Code child, so this WILL bite in prod. `client.rs`'s spawn applies this; pinned by a test.
pub const BRIDGE_ENV_STRIP: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
];

/// An ACP-driven adapter, parameterized by which bridge program it spawns (A-OC.1).
///
/// A stateless `'static` singleton (the [`crate::provider::ClaudeProvider`] precedent): ALL live
/// state — the bridge connection, the SC-1 outbound queue, the SC-5 single reader — lives in the
/// long-lived per-session ACP host ([`client`]) the verb layer hands in via
/// [`ProviderFx::acp_client`] (ADD-5). `hosting() = Daemon` (no mux pane; a bridge subprocess).
///
/// Two registered instances share this one driver: [`ACP_CC_PROVIDER`] (`acp/claude-code`, the
/// `claude-code-acp` bridge — Pete's live default, byte-identical) and [`ACP_OPENCODE_PROVIDER`]
/// (`acp/opencode`, the `opencode acp` bridge). Only the bridge SPAWN differs (`bridge_cmd` +
/// `bridge_args`, consumed at the residence layer); the protocol, queue, correlation, and verb
/// dispatch are identical.
///
/// **Transcripts delegate to [`crate::jsonl`]** (like `ClaudeProvider`) BY DESIGN, not by
/// claude-coupling: the bridge writes CC-shaped JSONL into the projects dir. That is also the
/// answer to "how Pete keeps his live view" (A2 §5): his existing projects-dir transcript tooling
/// already sees the ACP-driven session; the host additionally tees the `session/update` feed to a
/// live transcript (SC-2c). What is DISTINCT from claude is the live drive path (ACP, not PTY) —
/// `parse_status`/`inject`/`boot_waiter` below, and the SC-1 queue — NOT the at-rest transcript.
/// A bridge program that runs a server of ITS OWN, on its own protocol, which qd
/// pins the port of and hands out but never drives.
///
/// # Why an ACP bridge would have one at all
///
/// Because not every "ACP bridge" is a stdio shim. `claude-code-acp` is: it
/// speaks ACP on stdin/stdout and nothing else, which is why its spec leaves this
/// `None`. `opencode acp` is not — it calls `Server.listen()` and adapts ACP onto
/// a full opencode HTTP server in the same process. That server is not an
/// implementation detail we are reaching into: `opencode attach <url> --session
/// <id>` is a documented entry, and it is the ONLY way to put a human terminal on
/// a session the bridge is driving.
///
/// # What qd does and does not do with it
///
/// DOES: pin its port at spawn (it defaults to `0`, an ephemeral port nothing can
/// find again) and record the resulting address on the row, so a later verb can
/// hand it to a viewer. DOES NOT: speak the protocol. Every drive path stays ACP
/// over the adapter's ws front — one wire, unchanged. This is an ADDRESS qd
/// publishes, not a second transport it maintains.
pub struct HarnessServer {
    /// The bridge flag that pins the listen port (opencode: `--port`). Passed
    /// through as `--bridge-arg`s, so the adapter needs no knowledge of it.
    pub port_flag: &'static str,
    /// The URL scheme that server answers on (opencode: `http`).
    pub scheme: &'static str,
}

impl HarnessServer {
    /// This server's loopback address at `port` — the string the row records and
    /// a viewer is handed.
    ///
    /// Loopback is not a default that could be widened later: the bridge is a
    /// child of a per-session adapter on this machine, and its `--hostname`
    /// default (`127.0.0.1`) is what we are agreeing with rather than overriding.
    /// A harness server reachable off-box would be a session anyone on the
    /// network could type into.
    pub fn endpoint(&self, port: u16) -> String {
        format!("{}://127.0.0.1:{port}", self.scheme)
    }

    /// The `--bridge-arg` pair that pins this server to `port`.
    pub fn port_args(&self, port: u16) -> [String; 2] {
        [self.port_flag.to_string(), port.to_string()]
    }
}

pub struct AcpProvider {
    /// The stable provider id (registry/dispatch key + `--json` value): `acp/<bridge>`.
    id: &'static str,
    /// The resident adapter's `--bridge-cmd` override. `None` keeps the compiled [`BRIDGE_BIN`]
    /// default (`claude-code-acp`) so acp/claude-code's adapter argv + spawned bridge stay
    /// BYTE-IDENTICAL; `Some(cmd)` (opencode → `"opencode"`) selects a different bridge program at
    /// the residence layer (`run_new_acp_daemon` / `run_acp_resume`).
    bridge_cmd: Option<&'static str>,
    /// Extra `--bridge-arg`s for the bridge program (opencode: `["acp"]`; claude-code: none).
    bridge_args: &'static [&'static str],
    /// This bridge's own server, if it runs one ([`HarnessServer`]). `None` for a
    /// stdio-only bridge (claude-code), and that `None` is load-bearing: it is
    /// what makes the create path allocate ONE port rather than two, and what
    /// leaves `acp/claude-code`'s row and spawned argv byte-identical to what
    /// they were before this field existed.
    harness_server: Option<&'static HarnessServer>,
}



impl AcpProvider {
    /// Build a bridge spec. `const` because the two registered instances are
    /// `static`s, and they no longer live in this module: the harness-specific
    /// facts (WHICH bridge program, WHICH argv) belong to the harness, so
    /// `acp/claude-code`'s spec lives in [`crate::provider::claude::acp`] and
    /// `acp/opencode`'s in [`crate::provider::opencode::acp`]. This constructor
    /// is the seam that lets them do that while the fields stay private — the
    /// driver keeps sole control of how a spec is READ, the harness only says
    /// what its spec IS.
    pub const fn new(
        id: &'static str,
        bridge_cmd: Option<&'static str>,
        bridge_args: &'static [&'static str],
        harness_server: Option<&'static HarnessServer>,
    ) -> AcpProvider {
        AcpProvider {
            id,
            bridge_cmd,
            bridge_args,
            harness_server,
        }
    }

    /// The resident adapter's `--bridge-cmd` override (`None` = the [`BRIDGE_BIN`] default). Read
    /// by the residence create/resume paths (`run_new_acp_daemon`/`run_acp_resume`) to spawn the
    /// right bridge program for this provider.
    pub fn bridge_cmd(&self) -> Option<&'static str> {
        self.bridge_cmd
    }

    /// The extra `--bridge-arg`s for this provider's bridge program (opencode: `["acp"]`).
    pub fn bridge_args(&self) -> &'static [&'static str] {
        self.bridge_args
    }

    /// This bridge's own server, if it runs one. `None` ⇒ a stdio-only bridge:
    /// the residence allocates no second port and the row records no
    /// `harnessEndpoint`.
    pub fn harness_server(&self) -> Option<&'static HarnessServer> {
        self.harness_server
    }
}

// The per-harness bridge specs live under their HARNESS, not here: `bridge_cmd`
// and `bridge_args` are the only things that differ between the registered ACP
// instances, and they are facts about claude-code and opencode respectively —
// not about the protocol. Re-exported under their pre-reorg names so
// `provider::acp::{BRIDGE_BIN, CLAUDE_AGENT_ACP_BIN, ACP_CC_PROVIDER,
// ACP_OPENCODE_PROVIDER}` keeps resolving for every existing call site.
pub use crate::provider::claude::acp::{ACP_CC_PROVIDER, BRIDGE_BIN, CLAUDE_AGENT_ACP_BIN};
pub use crate::provider::opencode::acp::ACP_OPENCODE_PROVIDER;

/// Resolve a registered `acp/*` provider id to its concrete [`AcpProvider`] — the bridge spec's
/// single home. The residence verbs read `bridge_cmd`/`bridge_args` off the SAME statics
/// [`crate::provider::provider_for`] dispatches, so there is no second bridge mapping to drift.
pub fn acp_provider_for(id: &str) -> Option<&'static AcpProvider> {
    match id {
        "acp/claude-code" => Some(&ACP_CC_PROVIDER),
        "acp/opencode" => Some(&ACP_OPENCODE_PROVIDER),
        _ => None,
    }
}

impl Provider for AcpProvider {
    fn id(&self) -> &'static str {
        // Namespaced `acp/<bridge>` (A2 §A3-ACP): the registry/dispatch key + `--json` value.
        self.id
    }

    fn hosting(&self) -> Hosting {
        // Daemon-hosted: the bridge is a subprocess thread of conversation, no mux pane.
        Hosting::Daemon
    }

    /// The bridge launch command. The argv is this provider's bridge program + args (+ passthrough
    /// — claude-code: `claude-code-acp`; opencode: `opencode acp`); the
    /// host's spawn REMOVES [`BRIDGE_ENV_STRIP`] from the inherited env (the nesting guard —
    /// `LaunchPlan.env` can only ADD, so the removal is a spawn-time concern, documented here and
    /// applied in `client.rs`). Consumes NO claude config surface (no `fx.env`/config-toml read).
    ///
    /// **NOT the spawn path, and one thing it deliberately omits.** The live ACP
    /// create/resume assemble their argv at the residence layer
    /// (`daemon::bridge_for` + `residence::build_adapter_argv`), not here; this
    /// stays as the `Provider` contract's description of the command. What it
    /// cannot include is the [`HarnessServer`] port pin (`opencode acp --port
    /// <p>`), because the port is allocated per residence and a `&LaunchRequest`
    /// carries none. Read this for "which program", never for "the exact argv a
    /// running bridge was spawned with".
    fn launch_plan(&self, _fx: &ProviderFx, req: &LaunchRequest) -> LaunchPlan {
        // The bridge program: this provider's override (opencode → `opencode acp`) or the
        // compiled BRIDGE_BIN default (claude-code → `claude-code-acp`). For acp/claude-code
        // (bridge_cmd None, bridge_args empty) this is byte-identical to the pre-A-OC.1 argv.
        let mut argv = vec![self.bridge_cmd.unwrap_or(BRIDGE_BIN).to_string()];
        argv.extend(self.bridge_args.iter().map(|a| a.to_string()));
        argv.extend(req.passthrough.iter().cloned());
        LaunchPlan {
            argv,
            // The bridge takes its config over the ACP handshake, not launch env; the
            // nesting-guard STRIP (not a set) is applied at spawn (see BRIDGE_ENV_STRIP).
            env: vec![],
        }
    }

    /// Readiness = the ACP `initialize` handshake succeeding on the connected client (the
    /// `InitializeWaiter` analog). Borrows `fx.acp_client`; a missing client or a failed/auth-
    /// required handshake is a boot failure (never a pid-file timeout — there is no pid).
    fn boot_waiter<'a>(&self, fx: &'a ProviderFx<'a>) -> Box<dyn BootWaiter + 'a> {
        Box::new(AcpBootWaiter {
            client: fx.acp_client,
        })
    }

    /// ACP session-state — the host synthesizes a status object from the SC-1 queue's in-flight
    /// state (ACP v1 has no first-class status notification): `{"acpSessionState":"idle"|"busy"}`.
    /// A claude registry STRING or a codex `thread/status/changed` OBJECT → None (cross-feed
    /// negative control: each impl owns a DISTINCT raw interpretation, no shared parser).
    fn parse_status(&self, raw: &serde_json::Value) -> Option<SessionStatus> {
        let obj = raw.as_object()?;
        match obj.get("acpSessionState")?.as_str()? {
            "idle" => Some(SessionStatus::Idle),
            "busy" => Some(SessionStatus::Busy),
            _ => None,
        }
    }

    /// The CC projects dir (`fx.paths.projects_dir`) — the bridge runs the real CC engine, so an
    /// ACP-driven session's transcript lands exactly where a normal CC session's does. Shared
    /// with claude by ENGINE identity, not by trait coupling (see the type doc).
    fn transcript_root(&self, fx: &ProviderFx) -> PathBuf {
        fx.paths.projects_dir.clone()
    }

    /// `jsonl::find_jsonl_path` (cwd-slug + scan tiers) — CC-shaped JSONL, same as a normal CC
    /// session. pid is NEVER read.
    fn transcript_path(&self, state_root: &Path, key: &SessionKey) -> Option<PathBuf> {
        crate::jsonl::find_jsonl_path(state_root, key.id, key.cwd)
    }

    fn scan_transcripts(&self, state_root: &Path) -> Vec<TranscriptMeta> {
        crate::jsonl::scan_all(state_root)
    }

    fn transcript_stats(&self, path: &Path, include_preview: bool) -> JsonlStats {
        crate::jsonl::read_stats(path, include_preview)
    }

    /// ACP resume is the `session/load` protocol call (not a CLI arg); the fragment carries the
    /// session id so the host can re-establish it. Stable across calls; `fork` is not an ACP
    /// concept here (the bridge supports fork via `session/load` flags, a build follow-on).
    fn resume_args(&self, key: &SessionKey, _fork: bool) -> Vec<String> {
        vec!["resume".to_string(), key.id.to_string()]
    }

    /// ENQUEUE on the long-lived host's SC-1 queue → `session/prompt` when turn-idle, returning
    /// the attributable turn id (LB#5). The queue lives in the host behind `fx.acp_client` (SC-1a
    /// ownership), not reconstructed here. A 2nd inject mid-turn QUEUES (SC-3); a bounded-overflow
    /// maps to `Precondition("queue full")` (SC-1a flow-control). A missing/closed client →
    /// `NoTransport` — the verb layer's transport-loss signal (Child D: the verbs refuse
    /// and surface on it, identity preserved via `crate::tombstone`; see `ladder.rs`).
    fn inject(
        &self,
        fx: &ProviderFx,
        key: &SessionKey,
        message: &str,
        from: &str,
    ) -> Result<String, InjectError> {
        let client = fx
            .acp_client
            .ok_or_else(|| InjectError::NoTransport(key.id.to_string()))?;
        // Child B (opencode D1): the verb layer's durable exactly-once marker write,
        // fired the moment this turn's bytes are confirmed on the wire (see
        // `AcpClient::prompt`'s `on_dispatched` doc). No-op when the caller supplies none
        // (e.g. every existing test fixture, and any non-CC-send caller of this trait).
        let noop = || {};
        let on_dispatched = fx.acp_pre_dispatch.unwrap_or(&noop);
        client
            .prompt(key.id, message, from, on_dispatched)
            .map_err(|e| match e {
                AcpError::QueueFull => InjectError::Precondition("queue full".to_string()),
                AcpError::Closed | AcpError::Transport(_) => {
                    InjectError::NoTransport(key.id.to_string())
                }
                AcpError::Protocol(s) => InjectError::Precondition(s),
            })
    }
}

/// The ACP boot waiter: readiness is the `initialize` handshake, not a pid file.
struct AcpBootWaiter<'a> {
    client: Option<&'a dyn AcpClient>,
}

impl BootWaiter for AcpBootWaiter<'_> {
    fn wait_ready(&self, name: &str) -> Result<(), BootFailure> {
        let client = self.client.ok_or_else(|| BootFailure {
            phase: BootPhase::PidFile,
            detail: format!(
                "acp session \"{name}\" has no connected bridge client (fx.acp_client None)"
            ),
        })?;
        match client.initialize() {
            Ok(init) if !init.auth_required => Ok(()),
            Ok(_) => Err(BootFailure {
                phase: BootPhase::PidFile,
                detail: format!("acp bridge for \"{name}\" requires auth (run `claude /login`)"),
            }),
            Err(e) => Err(BootFailure {
                phase: BootPhase::PidFile,
                detail: format!("acp bridge for \"{name}\" failed initialize: {e}"),
            }),
        }
    }
}
