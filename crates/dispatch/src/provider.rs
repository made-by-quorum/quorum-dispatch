//! The `provider` seam (codex-p1-spec section 2) — a [`Provider`] trait extracted
//! from the claude-coupled engine, mirroring the [`crate::mux::Mux`] pattern
//! (trait + real impl + fixture impls, mux.rs:44-89).
//!
//! THE MISSION (codex-p1-spec section 0): **ZERO behavior change for claude
//! sessions.** Pete dogfoods this engine daily; the existing test corpus + the
//! goldens are the regression net. W2 is PURELY ADDITIVE — it carves the trait
//! and lands two impls (claude + a daemon-shaped FIXTURE) but rewires NO
//! production call site (that is W3-W5). Every concern the trait owns DELEGATES
//! to the existing module home (codex-p1-spec section 1.8) — code is MOVED or
//! CALLED, never re-implemented:
//!
//!   - launch  → [`crate::launch`] (`claude_bin` / `claude_flags` /
//!     `build_new_extra_args` / `build_claude_cmd`)
//!   - boot    → [`crate::boot::EventBootWaiter`] behind [`crate::create::BootWaiter`]
//!   - status  → [`crate::model::SessionStatus::parse`] (join.rs:307-317 semantics)
//!   - transcripts → [`crate::jsonl`] (`find_jsonl_path` / `scan_all` / `read_stats`)
//!   - resume  → `["--resume", id]` (+ `--fork-session`), the resume.rs verb's fragment
//!   - inject  → [`crate::relay::fast_relay_lookup`] + [`crate::relay::RelayContract`]
//!     (ADD-5: the contract stays the contract)
//!
//! NAMING (codex-p1-spec section 9, Phase-D rider): nothing claude-specific in
//! NEW public names — the trait method names are provider-neutral (`launch_plan`,
//! not `claude_cmd`). `ClaudeProvider` / `FixtureDaemonProvider` impl names are
//! plan-sanctioned. Existing claude-named items that this module CALLS (e.g.
//! `claude_flags`) keep their names — rename churn is Phase-D's, not the carve's.
//!
//! L9a: NOTHING here resolves a real home or reads raw `std::env`; every effect
//! arrives through [`ProviderFx`] (the NewDeps pattern, create.rs).

use std::path::{Path, PathBuf};

use crate::create::BootWaiter;
use crate::effects::Env;
use crate::jsonl::{JsonlStats, TranscriptMeta};
use crate::model::SessionStatus;
use crate::paths::QdPaths;
use crate::relay::{RelayContract, RelayError};

pub mod acp;
pub mod codex;
pub mod fixture;
pub mod opencode;
pub mod pi;

pub use fixture::FixtureDaemonProvider;

// ===========================================================================
// 2.1 — Identity + shared types (provider-neutral by construction).
// ===========================================================================

/// How a provider's sessions are hosted (codex-p1-spec section 2.1).
///
/// GATE-R-agnostic (R4): BOTH variants are legal trait answers; the engine
/// branches only where it already branches. P1 PRODUCTION CODE CONSULTS
/// `hosting()` NOWHERE — claude is `MuxPane` and the engine keeps its existing
/// mux calls. ONLY the fixture lane reads it. This is deliberate: the seam can
/// EXPRESS both branches (the [`FixtureDaemonProvider`] proves it) without the
/// engine PICKING one (that is GATE-R's call, not P1's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hosting {
    /// Session runs as a pane under the qd-owned mux (claude today).
    MuxPane,
    /// Session is a thread of a provider-owned daemon process — no mux pane.
    Daemon,
}

impl Hosting {
    /// The on-disk token for the registry row's `hosting` field.
    pub fn as_str(&self) -> &'static str {
        match self {
            Hosting::MuxPane => "mux-pane",
            Hosting::Daemon => "daemon",
        }
    }
}

/// Resolve a directory to its canonical form for comparison, falling back to the
/// input unchanged when it cannot be resolved (a dir that has since been removed,
/// or a permission failure).
///
/// WHY THIS IS PROVIDER-NEUTRAL AND NOT A per-provider helper. Every provider that
/// attributes an on-disk session to a registry row compares two spellings of the
/// SAME directory recorded from different vantage points: the row carries what the
/// create path was GIVEN (`--cwd /tmp/foo`, stored verbatim), while the harness
/// records what ITS process RESOLVED (`/private/tmp/foo` on macOS, where /tmp is a
/// symlink). An exact string compare then never matches and the session stays
/// unattributed forever — silently, since "not yet" and "never" look identical
/// from outside. codex hit exactly this in end-to-end validation; pi encodes the
/// resolved cwd into its session DIRECTORY NAME, so it would hit it too. One
/// canonicalizer, so the next provider inherits the fix instead of rediscovering
/// the defect.
///
/// The unchanged-input fallback is what makes this safe to apply to BOTH sides:
/// two unresolvable paths still compare as the plain strings they were.
pub fn canonical_dir(dir: &str) -> String {
    std::fs::canonicalize(dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| dir.to_string())
}

/// Permissive parse of a registry row's `hosting` token. An unknown/garbage
/// string maps to `None` — the caller then falls back to the provider's
/// structural hosting, so a corrupt field degrades to today's behavior instead of
/// inventing a topology (L8: never a panic, never a guess).
pub fn parse_hosting(s: &str) -> Option<Hosting> {
    match s {
        "mux-pane" => Some(Hosting::MuxPane),
        "daemon" => Some(Hosting::Daemon),
        _ => None,
    }
}

/// THE hosting question every verb should ask: how is THIS ROW hosted?
///
/// codex-interactive: `Provider::hosting()` answers per PROVIDER, which was the
/// whole truth while each provider had exactly one topology. codex now has two —
/// the `app-server` daemon lane and the `--interactive` mux-pane lane — so
/// attach/kill/send/resume must key on the row, not the provider id.
///
/// Resolution order:
///   1. the row's recorded `hosting` field, when present AND parseable;
///   2. else `provider_for(provider_id).hosting()` — the structural default;
///   3. else (an UNKNOWN provider id) `None`, so the caller keeps its own
///      unknown-provider refusal rather than being handed a made-up topology.
///
/// Step 2 is what keeps every pre-existing row byte-stable: nothing but the
/// codex-interactive create path writes the field, so every claude/acp/pi/codex-
/// daemon row answers exactly as it did before this seam existed.
pub fn row_hosting(provider_id: &str, hosting_field: Option<&str>) -> Option<Hosting> {
    if let Some(h) = hosting_field.and_then(parse_hosting) {
        return Some(h);
    }
    provider_for(provider_id).map(|p| p.hosting())
}

/// Provider-neutral session identity (codex-p1-spec section 2.1).
///
/// `pid` is OPTIONAL BY DESIGN (R3 tooth): a daemon-hosted session has thread-id
/// identity and may never have a pid. NOTHING in the trait may REQUIRE pid — the
/// conformance suite drives every daemon-lane method with `pid: None` and a
/// pid-requiring impl reds it (provider_seam.rs `daemon_pid_none_flows_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionKey<'a> {
    /// Session/thread id — a claude session uuid OR a daemon thread id.
    pub id: &'a str,
    pub name: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub pid: Option<i64>,
}

/// What to run for a new session (codex-p1-spec section 2.1).
///
/// `argv` + `env` pairs. For `MuxPane` hosting the engine shell-assembles and
/// runs it under the mux exactly as today (W3 will feed `argv`/`env` from this);
/// for `Daemon` hosting the fixture lane consumes it directly (no engine branch
/// in P1 — R4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// What the caller wants launched (codex-p1-spec section 2.1; NewOpts-shaped).
///
/// Provider-neutral: a 1:1 carry of the create path's [`crate::create::NewParams`]
/// launch-relevant fields + [`crate::launch::NewOpts`], so W3's rewire is
/// MECHANICAL (the claude impl reconstructs `NewOpts` + the args from these).
/// Nothing in it may be claude-specific.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchRequest {
    /// The session name (claude `--name`; the daemon ignores it for argv).
    pub name: String,
    /// Working dir for the session (carried for parity with NewParams; the
    /// claude argv does not embed it — the mux runs the cmd IN this cwd).
    pub cwd: Option<String>,
    /// `--resume <id>` (None = a fresh session).
    pub resume: Option<String>,
    /// `--fork-session` (only meaningful with `resume` for claude).
    pub fork: bool,
    /// `--agent <name>`, if given.
    pub agent: Option<String>,
    /// `--model <m>`, if given — emitted as a LAUNCH FLAG (warranty #2,
    /// 2026-06-11). Model is a BIRTH PROPERTY of the session (same principle as
    /// QD_SESSION_ID's explicit-set-at-every-launch), NOT a post-boot mutation:
    /// the create path used to deliver it as a `/model <m>` slash command, which
    /// in current Claude Code PERSISTS as the shared global default ("saved as
    /// your default for new sessions") — so every `--model` commission polluted
    /// the default a later plain session would inherit, and `--model` combined
    /// with `-p` dropped the prompt. As a launch flag it is per-session and
    /// needs no post-boot delivery.
    pub model: Option<String>,
    /// Pass-through args (everything after `--`).
    pub passthrough: Vec<String>,
    /// codex-interactive: does the caller want a HUMAN-DRIVABLE launch (a TUI in
    /// a mux pane) rather than the provider's machine-driven default?
    ///
    /// Provider-neutral by construction, and deliberately a REQUEST not a
    /// topology: it says what the caller wants, and each impl answers for itself.
    /// claude/acp/pi ignore it — their only launch shape is already the one the
    /// caller would get. codex is the one provider where it changes the argv:
    /// `false` (the default, so every existing construction site is unchanged)
    /// keeps `codex app-server`; `true` launches the bare `codex` TUI.
    pub interactive: bool,
}

/// The injected effect bundle (codex-p1-spec section 2.1; the NewDeps pattern,
/// create.rs). Holds ONLY the seams the two impls actually need — kept minimal,
/// each member documented with which impl/method consumes it.
///
/// L9a: nothing here resolves a real home or reads raw `std::env`. A MINIMAL
/// `ProviderFx` (empty env, nonexistent config path, no relay inputs) is a valid
/// value — the daemon impl launches against exactly that (the negative control
/// `daemon_launch_plan_minimal_fx`), proving it consumes NO claude config
/// surface (rev B red-team (a): the config-toml path is NOT a trait param, it is
/// an impl-internal resolution off `fx`).
pub struct ProviderFx<'a> {
    /// Env for `claude_bin` + `claude_flags` precedence resolution.
    /// CONSUMED BY: `ClaudeProvider::launch_plan`. The daemon impl ignores it.
    pub env: &'a dyn Env,
    /// Home→state layout (L9a). `config_toml_path` is derived from it the same
    /// way the resume verb does (`<home>/.quorum/dispatch/config.toml`); `sessions_dir` is the
    /// boot waiter's PID-file root; `projects_dir` is the transcript root.
    /// CONSUMED BY: `ClaudeProvider::launch_plan` (config toml),
    /// `ClaudeProvider::boot_waiter` (sessions_dir).
    pub paths: &'a QdPaths,
    /// The canonical socket dir the session lives in (boot history + send
    /// target; create.rs `canonical_dir`). CONSUMED BY:
    /// `ClaudeProvider::boot_waiter` (the [`EventBootWaiter`] socket dir).
    pub socket_dir: PathBuf,
    /// The mux for the boot waiter's history reads + the single answerer `\r`.
    /// CONSUMED BY: `ClaudeProvider::boot_waiter`. None for impls/units that
    /// never drive boot (the daemon's boot readiness is a fixture record, no mux).
    pub mux: Option<&'a dyn crate::mux::Mux>,
    /// Clock + sleeper for the boot waiter's deadlines/polls.
    /// CONSUMED BY: `ClaudeProvider::boot_waiter`. None when boot is not driven.
    pub clock: Option<&'a dyn crate::effects::Clock>,
    pub sleeper: Option<&'a dyn crate::boot::Sleeper>,
    /// The ADD-5 relay CONTRACT — the transport `inject` sends through.
    /// CONSUMED BY: `ClaudeProvider::inject`. None for the daemon impl (its
    /// inject is an in-fixture enqueue, no relay).
    pub relay: Option<&'a dyn RelayContract>,
    /// Pre-resolved relay PORT for `inject` (W5 owns the full sidecar/ancestry
    /// resolution at the verb layer; the lib-side `inject` body sends through the
    /// CONTRACT given the port, never holding a transport handle in the trait
    /// signature — the banned claude-shaped contortion). CONSUMED BY:
    /// `ClaudeProvider::inject`.
    pub relay_port: Option<u16>,
    /// The codex app-server RPC transport CONTRACT (ADD-5 pattern: the contract
    /// stays the contract; `provider::codex::ws::WsAppServer` is the driver).
    /// CONSUMED BY: `CodexProvider::{boot_waiter, inject}`. None for the
    /// claude/fixture lanes (they never speak the app-server protocol), mirroring
    /// the relay members' claude-only precedent.
    ///
    /// LEAD DESIGN DECISION (codex-p2-spec section 6.2 sketched a sibling
    /// `codex_endpoint: Option<&str>` member too; it is DELIBERATELY NOT added).
    /// Endpoint resolution is a VERB-layer concern (the `relay_port` precedent
    /// above): the verb layer reads the row's recorded endpoint, CONNECTS a
    /// `WsAppServer`, and hands this trait an ALREADY-CONNECTED `&dyn AppServerRpc`
    /// here. The trait never holds a raw endpoint string or opens a socket — so a
    /// transport handle/endpoint never appears in a trait method signature (the
    /// same banned claude-shaped contortion `relay_port` avoids). For an
    /// already-connected rpc to be callable through this SHARED `&dyn`, the W3
    /// refactor moved `AppServerRpc`'s methods to `&self` (interior mutability in
    /// `WsAppServer`); the driver is single-threaded / `!Sync` by design.
    pub app_server: Option<&'a dyn crate::provider::codex::AppServerRpc>,
    /// The BELIEVED open turn id for the codex SEND ladder (codex-p2-spec section
    /// 7.5) — the `relay_port` precedent for a codex-only concern. The verb layer
    /// reads the row's rollout tail (`provider::codex::open_turn_id`) and feeds the
    /// open turn id here when the session is believed BUSY; `None` (the common
    /// case) means believed IDLE. CONSUMED BY: `CodexProvider::inject`, which on
    /// `Some(T)` steers turn T (`turn/steer{expectedTurnId:T}`) and falls back to a
    /// fresh turn on the server's stale-`expectedTurnId` fence, and on `None`
    /// starts a fresh turn. The trait never sees the start/steer vocabulary — the
    /// SEND surface stays SEND-only (the banned start/steer-in-a-user-string
    /// contortion is kept out of the verb AND the trait). None for the
    /// claude/fixture lanes.
    pub codex_expected_turn_id: Option<&'a str>,
    /// The ACP transport CONTRACT (ADD-5 pattern, the `app_server` precedent): a
    /// CONNECTED `&dyn AcpClient` the verb layer hands in. CONSUMED BY:
    /// `AcpProvider::inject` (enqueue on the long-lived host's SC-1 queue →
    /// `session/prompt`). None for the claude/codex/fixture lanes (they never speak
    /// ACP), mirroring `app_server`. The trait never holds a raw endpoint/socket —
    /// the long-lived per-session ACP host (`provider/acp/client.rs`) owns the
    /// connection + the SC-1 queue + the SC-5 single-reader; this is the borrow of it.
    pub acp_client: Option<&'a dyn crate::provider::acp::AcpClient>,
    /// Child B (opencode D1, the exactly-once dispatch-timing guard): a durable
    /// "structured send is going out" marker write, invoked by `AcpProvider::inject`
    /// (via `AcpClient::prompt`'s `on_dispatched`) the MOMENT this turn's bytes are
    /// confirmed on the wire — before the reply is read. The verb layer supplies a
    /// closure that persists the registry row's `structured_send_issued` bit; a
    /// socket drop between dispatch and reply-read then still leaves the correct
    /// history for the NEXT process to read (never gated on `inject`'s `Ok` return
    /// alone). `None` for every lane but the ACP send verb (claude/codex/pi/fixture
    /// never read it; a caller with no exactly-once concern, e.g. tests, omits it).
    pub acp_pre_dispatch: Option<&'a dyn Fn()>,
    /// The pi stdio-RPC transport CONTRACT (ADD-5 pattern, the `app_server`/
    /// `acp_client` precedent): a CONNECTED `&dyn PiRpc` the verb layer hands in
    /// (a [`crate::provider::pi::remote::PiRemote`] reaching the per-session pi
    /// resident's loopback front). CONSUMED BY: `PiProvider::{boot_waiter, inject}`.
    /// None for the claude/codex/acp/fixture lanes (they never speak pi). The trait
    /// never holds a raw endpoint/socket — the pi resident
    /// (`provider/pi/residence.rs`) owns the pi child + the stdio driver; this is
    /// the verb-layer borrow of the reach to it.
    pub pi_rpc: Option<&'a dyn crate::provider::pi::rpc::PiRpc>,
    /// Lifecycle-collapse A-3 (spec D5, Pete's ruling: relay readiness is
    /// DEFAULT-ON for `qd start`): the CALLER's relay-wait decision for the boot
    /// waiter. `Some(true)` arms the relay-sidecar phase, `Some(false)` disarms
    /// it (`--no-await-relay`), `None` = no caller decision — the legacy
    /// `QD_BOOT_AWAIT_RELAY` env opt-in governs (resume + every fx site that
    /// predates the flag keep today's behavior byte-for-byte). CONSUMED BY:
    /// `ClaudeProvider::boot_waiter`; every other impl ignores it.
    pub await_relay: Option<bool>,
}

impl<'a> ProviderFx<'a> {
    /// The config-toml path for `claude_flags`, derived from the injected home
    /// the SAME way the resume verb does (`<home>/.quorum/dispatch/config.toml`,
    /// resume.rs:127). NOT a trait param (rev B red-team (a)): config resolution
    /// is impl-internal, off `fx`.
    fn config_toml_path(&self) -> PathBuf {
        self.paths
            .home
            .join(".quorum")
            .join("dispatch")
            .join("config.toml")
    }
}

/// Provider-neutral inject failure (codex-p1-spec section 2.1).
///
/// The claude impl maps [`RelayError`] into it WITHOUT losing the variant
/// distinctions send:relay's wording needs later (W5 must reproduce the exact
/// verb wording — the `RelayFailed` variant carries the structured `RelayError`,
/// not a flattened string). The daemon impl uses the non-relay variants only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectError {
    /// No relay port resolved for the target session (claude:
    /// send.ts:406-409 "has no relay."). Carries the session id/name context.
    NoTransport(String),
    /// The relay transport reported a failure — the structured [`RelayError`] is
    /// preserved so W5 can map each class to its exact send:relay wording.
    RelayFailed(RelayError),
    /// A precondition on the inject failed (daemon: a steer with a stale
    /// expected-turn-id). Carries the human-facing reason.
    Precondition(String),
}

impl std::fmt::Display for InjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InjectError::NoTransport(s) => write!(f, "no transport for \"{s}\""),
            InjectError::RelayFailed(e) => write!(f, "relay failed: {e}"),
            InjectError::Precondition(s) => write!(f, "precondition failed: {s}"),
        }
    }
}

impl std::error::Error for InjectError {}

// ===========================================================================
// 2.2 — The trait.
// ===========================================================================

/// The provider seam (codex-p1-spec section 2.2). The caller owns
/// transport/source; the provider owns INTERPRETATION + ASSEMBLY.
///
/// Signature note (recorded per codex-p1-spec section 2.2): `boot_waiter` takes
/// `&ProviderFx<'a>` and returns `Box<dyn BootWaiter + 'a>` — the waiter borrows
/// the mux/clock/sleeper out of `fx`, so the returned box is bounded by `fx`'s
/// lifetime. `inject` carries the message/from like the send:relay verb body;
/// it does NOT carry a port/transport handle in its SIGNATURE (the port arrives
/// via `fx.relay_port`, W5's resolution feeds it) — that keeps the banned
/// claude-shaped contortion out of the trait.
pub trait Provider {
    /// Stable provider id — the registry/dispatch key AND the `--json` value.
    fn id(&self) -> &'static str;

    /// How this provider's sessions are hosted (data, not a branch — R4).
    fn hosting(&self) -> Hosting;

    /// Launch command/flags/env resolution. The impl resolves its OWN config
    /// surfaces from `fx` (claude: `claude_bin` + `claude_flags` precedence
    /// env > config.toml-at-QdPaths > DEFAULT_FLAGS + `build_new_extra_args`,
    /// byte-identical assembly to today).
    fn launch_plan(&self, fx: &ProviderFx, req: &LaunchRequest) -> LaunchPlan;

    /// The boot-readiness waiter the create path drives (claude: the existing
    /// [`EventBootWaiter`] machinery behind [`BootWaiter`]; daemon fixture:
    /// handshake/notification-shaped readiness).
    fn boot_waiter<'a>(&self, fx: &'a ProviderFx<'a>) -> Box<dyn BootWaiter + 'a>;

    /// Status derivation from the provider's NATIVE raw signal. claude: raw =
    /// the registry row's status STRING (join.rs:307-317 semantics; non-string
    /// or unknown → None and the caller picks the fallback). Daemon: raw = a
    /// `thread/status/changed`-shaped notification object.
    fn parse_status(&self, raw: &serde_json::Value) -> Option<SessionStatus>;

    /// The provider's transcript ROOT, resolved off `fx` (codex-p2-spec section
    /// 6.4 — the ONE recorded signature addition). claude returns
    /// `fx.paths.projects_dir` (the call sites that pass `projects_dir` AT the
    /// call site switch to `provider.transcript_path(&provider.transcript_root(fx),
    /// &key)` mechanically — the root VALUE is byte-identical, so claude behavior
    /// is unchanged; W5/W6 own that switch). codex resolves `$CODEX_HOME/sessions`
    /// off `fx.env` ONLY (L9a — never raw `std::env`). The conformance suite pins
    /// all three impls' roots.
    fn transcript_root(&self, fx: &ProviderFx) -> PathBuf;

    /// Transcript location + keying (claude: `projects_dir` cwd-slug +
    /// `<session-id>.jsonl`; daemon: date/thread-id keyed, NO cwd in the key).
    /// `state_root` is the provider's transcript root, injected.
    fn transcript_path(&self, state_root: &Path, key: &SessionKey) -> Option<PathBuf>;

    /// Scan all transcripts under `state_root` (claude: `jsonl::scan_all`).
    fn scan_transcripts(&self, state_root: &Path) -> Vec<TranscriptMeta>;

    /// Parse one transcript's stats (claude: `jsonl::read_stats`; daemon: a
    /// rollout-SHAPED parser over fixture lines — shape only, no codex code).
    fn transcript_stats(&self, path: &Path, include_preview: bool) -> JsonlStats;

    /// Resume/fork argv fragment (claude: `["--resume", id]` (+ `--fork-session`
    /// exactly as the bin resume verb assembles today)).
    fn resume_args(&self, key: &SessionKey, fork: bool) -> Vec<String>;

    /// The ADD-5 driver verb. claude: resolve the relay port (W5 owns the full
    /// sidecar+ancestry resolution; the port arrives via `fx.relay_port`) then
    /// `RelayContract::send_message`. RelayContract STAYS the contract and
    /// becomes this impl's transport. Returns the provider-side message/turn id.
    fn inject(
        &self,
        fx: &ProviderFx,
        key: &SessionKey,
        message: &str,
        from: &str,
    ) -> Result<String, InjectError>;
}

// ===========================================================================
// 2.3 — Dispatch (the read-back key, R1).
// ===========================================================================

/// The ONE registered claude provider (a `'static` singleton so `provider_for`
/// can hand out `&'static dyn Provider` without allocation).
static CLAUDE_PROVIDER: ClaudeProvider = ClaudeProvider;

/// Resolve a provider id to its impl (codex-p1-spec section 2.3).
///
/// Registry: claude-code, codex, acp/claude-code, pi, and (A-OC.1) opencode —
/// where `"opencode"` and `"acp/opencode"` both resolve to the opencode-bridged
/// [`acp::ACP_OPENCODE_PROVIDER`] (CLI ergonomic → internal id `acp/opencode`). An
/// UNKNOWN id → `None`, and every ACTING caller errors LOUDLY (lifecycle.rs:39
/// pattern) and exits nonzero. The `"fixture-daemon"` id is ALSO not registered
/// here: [`FixtureDaemonProvider`] is a FIXTURE (the conformance lane constructs
/// it directly), not a production-dispatchable provider — registering it would
/// make `qd new --provider fixture-daemon` bootable, which P1 must not do.
pub fn provider_for(id: &str) -> Option<&'static dyn Provider> {
    match id {
        "claude-code" => Some(&CLAUDE_PROVIDER),
        // P2 W3: codex is now resolvable (GATE-R RULED = (A) daemon-thread). The
        // CLI fail-closed check + the supported-providers error string (which W4
        // updates to list codex) live in the bin verb layer
        // (src/bin/dispatch/verbs/lifecycle.rs:138 + :155 — NOT touched here).
        "codex" => Some(&codex::CODEX_PROVIDER),
        // scoped-ACP-CC: the Claude Code ACP adapter is CLI-bootable. This SUPERSEDES A2
        // §A3-ACP's "gated off CLI boot until a bridge exists" — Pete's directive supplies the
        // official `claude-code-acp` bridge (STEP-0 GREEN, confirmed faithful on this box), so
        // the Mode-B→Mode-A switch A2 deferred for CC is now live.
        "acp/claude-code" => Some(&acp::ACP_CC_PROVIDER),
        // A-OC.1: opencode's product verb-path over the SAME ACP driver, bridged to `opencode
        // acp`. The CLI ergonomic `--provider opencode` (Pete's phrasing — opencode is the
        // provider, acp the transport) is an ALIAS that RESOLVES to internal id `acp/opencode`,
        // so it rides the existing `acp/`-prefix verb dispatch with no per-opencode verb arms.
        "opencode" | "acp/opencode" => Some(&acp::ACP_OPENCODE_PROVIDER),
        // WS-A.2: pi is now resolvable (daemon-hosted, stdio-RPC; the resident
        // host is `provider/pi/residence.rs`, the `pi-daemon` verb in main.rs).
        "pi" => Some(&pi::PI_PROVIDER),
        _ => None,
    }
}

// ===========================================================================
// ClaudeProvider — DELEGATES to the existing module homes (section 1.8).
// ===========================================================================

/// The claude-code provider. Every method DELEGATES to the existing claude home
/// (codex-p1-spec section 1.8) — code is MOVED or CALLED, never re-implemented,
/// so claude behavior is byte-identical to today (the W3-W5 rewires keep the
/// existing create/resume/send units passing untouched).
pub struct ClaudeProvider;

impl Provider for ClaudeProvider {
    fn id(&self) -> &'static str {
        // model.rs:92-98 documents this is the stable provider id / --json value.
        "claude-code"
    }

    fn hosting(&self) -> Hosting {
        Hosting::MuxPane
    }

    /// Reproduces EXACTLY what the create path builds today: `claude_bin` +
    /// `claude_flags` (env > config-toml > DEFAULT_FLAGS, launch.rs:11-57) +
    /// `build_new_extra_args`. The argv is the SAME token list `build_claude_cmd`
    /// single-quotes (build_claude_cmd is the SHELL-assembly step the mux owns;
    /// the trait yields the pre-quote argv + env, W3 feeds it to the mux). Pinned
    /// against the launch.rs functions directly by `claude_launch_plan_matches_*`.
    fn launch_plan(&self, fx: &ProviderFx, req: &LaunchRequest) -> LaunchPlan {
        // `fx.env` is `&dyn Env`; the blanket `impl Env for &T` (effects.rs) lets
        // it satisfy the `&impl Env` these M1-frozen helpers take.
        let bin = crate::launch::claude_bin(&fx.env);
        let flags = crate::launch::claude_flags(&fx.env, &fx.config_toml_path());
        let opts = crate::launch::NewOpts {
            resume: req.resume.clone(),
            fork: req.fork,
            agent: req.agent.clone(),
            model: req.model.clone(),
        };
        let extra = crate::launch::build_new_extra_args(&req.name, &opts, &req.passthrough, &flags);
        // argv = [bin, ...flags, ...extra] — the exact token order
        // build_claude_cmd quotes (launch.rs:150-157), pre-shell-assembly.
        let mut argv = Vec::with_capacity(1 + flags.len() + extra.len());
        argv.push(bin);
        argv.extend(flags);
        argv.extend(extra);
        // env: claude resolves its backend env via the F1 session-env-file
        // mechanism at the create path, NOT inline argv env — so the trait-level
        // LaunchPlan env is empty for claude (the F1 file is a create.rs concern,
        // W3 keeps it there). Documented so W3's rewire knows env stays []: today.
        LaunchPlan { argv, env: vec![] }
    }

    /// The SAME [`EventBootWaiter`] machinery the create path constructs today
    /// (create.rs wires it from boot.rs). Borrows the mux/clock/sleeper out of
    /// `fx`; panics with a clear message if a boot effect is absent (a caller
    /// driving boot MUST supply them — units that never drive boot pass None and
    /// never call this).
    fn boot_waiter<'a>(&self, fx: &'a ProviderFx<'a>) -> Box<dyn BootWaiter + 'a> {
        let mux = fx
            .mux
            .expect("ClaudeProvider::boot_waiter requires fx.mux (the boot history/send target)");
        let clock = fx
            .clock
            .expect("ClaudeProvider::boot_waiter requires fx.clock");
        let sleeper = fx
            .sleeper
            .expect("ClaudeProvider::boot_waiter requires fx.sleeper");
        // Fix-A (RESPEC-DELTA §4): the relay-sidecar readiness phase makes up-live =
        // pid + idle + the child's relay sidecar present, so the relay-default
        // priming transport is sound (§4.3). `relay_dir` is the global
        // `<home>/.claude/relay` sidecar dir.
        //
        // Lifecycle-collapse A-3 (spec D5, Pete's ruling): relay readiness is
        // DEFAULT-ON for `qd start` — the START verb passes `fx.await_relay =
        // Some(!--no-await-relay)`, so exit 0 means idle AND relay-reachable.
        // `fx.await_relay = None` (resume, tests, every pre-flag fx site) keeps
        // the legacy behavior: the phase engages only on the QD_BOOT_AWAIT_RELAY
        // env opt-in (kept as a transition alias — frame's engine::start still
        // sets it; harmless under default-on). The old BUILD-REPORT bubble
        // ("keep opt-in vs default-on") is RESOLVED by D5: default-on,
        // consumer-scoped opt-out.
        let waiter = crate::boot::EventBootWaiter::new(
            mux,
            fx.socket_dir.clone(),
            fx.paths.sessions_dir.clone(),
            clock,
            sleeper,
        );
        let await_relay = fx.await_relay.unwrap_or_else(|| {
            fx.env
                .var("QD_BOOT_AWAIT_RELAY")
                .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        });
        Box::new(if await_relay {
            waiter.with_relay_dir(fx.paths.relay_dir.clone())
        } else {
            waiter
        })
    }

    /// join.rs:307-317 semantics: trust the registry status STRING through
    /// [`SessionStatus::parse`]; a non-string or unknown value → None (the caller
    /// picks the fallback — join uses Idle). We do NOT change join.rs (W6).
    fn parse_status(&self, raw: &serde_json::Value) -> Option<SessionStatus> {
        // The claude raw signal is the registry row's status STRING. A daemon
        // notification OBJECT is `as_str() == None` → None (the cross-feed
        // negative control: a daemon notification fed here returns None).
        raw.as_str().and_then(SessionStatus::parse)
    }

    /// The claude transcript root is `fx.paths.projects_dir` — the SAME value the
    /// gather/wait call sites pass at the call site today (codex-p2-spec section
    /// 6.4). The mechanical switch (W5/W6) is byte-identical because the root
    /// VALUE is unchanged.
    fn transcript_root(&self, fx: &ProviderFx) -> PathBuf {
        fx.paths.projects_dir.clone()
    }

    /// `jsonl::find_jsonl_path` over `projects_dir` (cwd-slug keying). `state_root`
    /// IS the projects dir. The cwd-slug tier needs `key.cwd`; pid is NEVER read.
    fn transcript_path(&self, state_root: &Path, key: &SessionKey) -> Option<PathBuf> {
        crate::jsonl::find_jsonl_path(state_root, key.id, key.cwd)
    }

    fn scan_transcripts(&self, state_root: &Path) -> Vec<TranscriptMeta> {
        crate::jsonl::scan_all(state_root)
    }

    fn transcript_stats(&self, path: &Path, include_preview: bool) -> JsonlStats {
        crate::jsonl::read_stats(path, include_preview)
    }

    /// `["--resume", id]` (+ `--fork-session`) — the EXACT fragment the bin
    /// resume verb assembles (resume.rs:130 builds `["--resume", session_id]`;
    /// the fork shape is `--fork-session`, build_new_extra_args:123). pid is
    /// NEVER read.
    fn resume_args(&self, key: &SessionKey, fork: bool) -> Vec<String> {
        let mut args = vec!["--resume".to_string(), key.id.to_string()];
        if fork {
            args.push("--fork-session".to_string());
        }
        args
    }

    /// Resolve the relay transport and send through the CONTRACT (ADD-5). The
    /// port arrives via `fx.relay_port` — W5 owns the full sidecar+ancestry
    /// resolution at the verb layer (the lib-side `fast_relay_lookup` is pure but
    /// its INPUTS — pid entries / sidecars / ppid map — are gathered with
    /// bin-crate effects RealEnv/HttpRelayProbe/RealProcessTable, send_relay.rs:
    /// 146-169, which do not belong in lib). RESIDUAL FOR W5: feed `fx.relay_port`
    /// from that resolution and map `InjectError::RelayFailed(RelayError)` back to
    /// each class's exact send:relay wording (send_relay.rs `send_err_text`).
    fn inject(
        &self,
        fx: &ProviderFx,
        key: &SessionKey,
        message: &str,
        from: &str,
    ) -> Result<String, InjectError> {
        let Some(port) = fx.relay_port else {
            return Err(InjectError::NoTransport(key.id.to_string()));
        };
        let relay = fx
            .relay
            .ok_or_else(|| InjectError::NoTransport(key.id.to_string()))?;
        relay
            .send_message(port, message, from)
            .map_err(InjectError::RelayFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    // === codex-interactive: row_hosting — the per-ROW hosting question ===
    //
    // The whole point of this helper is that a provider id is no longer a
    // sufficient answer. These pin both halves: the fallback that keeps every
    // pre-existing row behaving exactly as before, and the override that makes the
    // interactive codex lane possible.

    #[test]
    fn row_hosting_falls_back_to_the_providers_structural_answer() {
        // No `hosting` field — every row written before this seam existed, and
        // every row written by a provider with only one topology.
        assert_eq!(
            row_hosting("claude-code", None),
            Some(Hosting::MuxPane),
            "claude is structurally pane-hosted"
        );
        assert_eq!(
            row_hosting("codex", None),
            Some(Hosting::Daemon),
            "a codex row that does not say is the app-server daemon — the \
             pre-codex-interactive answer, so existing rows are untouched"
        );
        assert_eq!(row_hosting("pi", None), Some(Hosting::Daemon));
        assert_eq!(row_hosting("acp/opencode", None), Some(Hosting::Daemon));
    }

    #[test]
    fn row_hosting_lets_the_row_override_the_provider() {
        // THE case this seam exists for: one provider, two topologies.
        assert_eq!(
            row_hosting("codex", Some("mux-pane")),
            Some(Hosting::MuxPane),
            "an --interactive codex row is pane-hosted despite codex's structural Daemon"
        );
        assert_eq!(row_hosting("codex", Some("daemon")), Some(Hosting::Daemon));
    }

    #[test]
    fn row_hosting_of_an_unknown_provider_is_none_not_a_guess() {
        // Callers keep their own unknown-provider refusal rather than being handed
        // a fabricated topology.
        assert_eq!(row_hosting("gemini", None), None);
    }

    #[test]
    fn row_hosting_of_a_garbage_field_degrades_to_the_provider_default() {
        // A corrupt/unknown token must not invent a topology, and must not drop
        // the row either — it degrades to exactly the pre-field behavior.
        assert_eq!(row_hosting("codex", Some("banana")), Some(Hosting::Daemon));
        assert_eq!(row_hosting("codex", Some("")), Some(Hosting::Daemon));
        assert_eq!(
            row_hosting("claude-code", Some("banana")),
            Some(Hosting::MuxPane)
        );
    }

    #[test]
    fn hosting_tokens_round_trip_through_parse() {
        // The write side (`as_str`) and the read side (`parse_hosting`) must agree
        // — a drift would make every row we write unreadable to ourselves.
        for h in [Hosting::MuxPane, Hosting::Daemon] {
            assert_eq!(parse_hosting(h.as_str()), Some(h));
        }
        assert_eq!(parse_hosting("nonsense"), None);
    }

    // --- codex P1 W7: provider-routed transcript_path IS jsonl::find_jsonl_path ---
    //
    // The gather + send/wait `find_jsonl_path` call sites route through
    // `ClaudeProvider::transcript_path` (codex-p1-spec section 7.2). That method
    // DELEGATES to `jsonl::find_jsonl_path` — this is call-site ROUTING only, the
    // resolved path must be IDENTICAL for every constructible row. These pin that
    // delegation across both `find_jsonl_path` tiers (cwd-slug tier + fallback
    // scan tier) for a representative jail layout.
    //
    // MUTATION EVIDENCE (CR-3): a keying drift in `transcript_path` — dropping
    // `key.cwd` (cwd-slug lost → the cwd tier misses, path differs) or mangling
    // `key.id` (the filename differs) — reds the equality assertions below.

    #[test]
    fn claude_transcript_path_equals_find_jsonl_path_cwd_tier() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path();
        let cwd = "/home/user/proj";
        let sid = "abc-123";
        let slug = crate::jsonl::cwd_to_project_path(cwd);
        let path = projects.join(&slug).join(format!("{sid}.jsonl"));
        write_file(&path, "{}");

        let key = SessionKey {
            id: sid,
            name: Some("w"),
            cwd: Some(cwd),
            pid: Some(42),
        };
        let via_provider = ClaudeProvider.transcript_path(projects, &key);
        let via_jsonl = crate::jsonl::find_jsonl_path(projects, sid, Some(cwd));
        assert_eq!(via_provider, via_jsonl, "cwd-tier: routing == direct");
        assert_eq!(
            via_provider,
            Some(path),
            "resolves the cwd-slug + <id>.jsonl"
        );
    }

    #[test]
    fn claude_transcript_path_equals_find_jsonl_path_scan_tier() {
        // cwd-derived dir does not contain it; the fallback scan tier finds it.
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path();
        let sid = "xyz-789";
        let path = projects
            .join("-some-other-dir")
            .join(format!("{sid}.jsonl"));
        write_file(&path, "{}");

        let key = SessionKey {
            id: sid,
            name: None,
            cwd: Some("/wrong/cwd"), // misses the cwd tier → fallback scan.
            pid: None,               // pid is NEVER read by transcript_path.
        };
        let via_provider = ClaudeProvider.transcript_path(projects, &key);
        let via_jsonl = crate::jsonl::find_jsonl_path(projects, sid, Some("/wrong/cwd"));
        assert_eq!(via_provider, via_jsonl, "scan-tier: routing == direct");
        assert_eq!(via_provider, Some(path), "resolves via the fallback scan");
    }

    #[test]
    fn claude_transcript_path_missing_is_none_like_find_jsonl_path() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp.path().join("-d").join("other.jsonl"), "{}");
        let key = SessionKey {
            id: "nope",
            name: None,
            cwd: None,
            pid: None,
        };
        let via_provider = ClaudeProvider.transcript_path(tmp.path(), &key);
        let via_jsonl = crate::jsonl::find_jsonl_path(tmp.path(), "nope", None);
        assert_eq!(via_provider, via_jsonl);
        assert_eq!(via_provider, None);
    }
}
