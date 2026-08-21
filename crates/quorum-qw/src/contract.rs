//! The `qw` interface, expressed as a Rust trait (qd/qw split, stage 1 step 2).
//!
//! # The one discipline
//!
//! **Every method signature must be serializable.** Arguments and returns are
//! plain owned data; effects live behind `&self`, injected at construction.
//!
//! This is the property `dispatch::provider::Provider` lacks and the reason it
//! could never become a process boundary: [`ProviderFx`] carries `&dyn Env`,
//! `&dyn Mux`, `&dyn Clock`, `&dyn RelayContract`, `&dyn AcpClient` — none of
//! which can cross a process — and 12 of its 14 fields are consumed by exactly
//! one provider.
//!
//! ```ignore
//! // WRONG — the shape Provider uses today. Can never be a wire call.
//! fn deliver(&self, fx: &LaneFx, id: &SessionId, msg: &str) -> Result<String, Error>;
//!
//! // RIGHT — effects are in &self, everything else is data.
//! fn deliver(&self, id: &SessionId, msg: &Message, policy: &DeliverPolicy)
//!     -> Result<Receipt, LaneError>;
//! ```
//!
//! One method is deliberately outside this rule: [`LaneOps::attach`] performs a
//! terminal handoff rather than returning data, and crosses a process boundary by
//! exec-with-inherited-stdio rather than by serialization. Its signature is still
//! plain data; only its invocation mechanism differs. See its docs for why the
//! earlier data-returning design could not work.
//!
//! Two guards enforce it mechanically, both in `super::conformance`:
//! a serde round-trip over every DTO, and a source scan of THIS file asserting
//! the trait body contains no `&dyn`, no `impl Trait` and no closure parameters.
//! If a method cannot satisfy the rule, that is the signal the boundary is drawn
//! wrong — which is what stage 1 is for.
//!
//! [`ProviderFx`]: crate::provider::ProviderFx

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::launch::RenderMode;

/// Session status, as **qw** reports it.
///
/// Deliberately qw's own type rather than a borrow of
/// `dispatch::model::SessionStatus`. `qd` depends on `qw`, never the reverse, so
/// reaching back across the boundary for this would invert the dependency. The
/// wire tokens are identical to the ones every other surface already uses, so the
/// mapping on qd's side is total and lossless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Idle,
    Busy,
    Shell,
    Cold,
    Killed,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Idle => "idle",
            SessionStatus::Busy => "busy",
            SessionStatus::Shell => "shell",
            SessionStatus::Cold => "cold",
            SessionStatus::Killed => "killed",
        }
    }

    /// Permissive parse. An unknown token is `None` — never a guess.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(SessionStatus::Idle),
            "busy" => Some(SessionStatus::Busy),
            "shell" => Some(SessionStatus::Shell),
            "cold" => Some(SessionStatus::Cold),
            "killed" => Some(SessionStatus::Killed),
            _ => None,
        }
    }
}

// ===========================================================================
// Identity
// ===========================================================================

/// A session's stable id (the provider session uuid / thread id).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub String);

/// The id minted for one delivered message.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MessageId(pub String);

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl MessageId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where in the delivery ledger a send's records live.
///
/// # Why this is the `(session?, name?)` pair and not a union
///
/// [`LaneOps::recover`] used to be addressed by [`SessionId`], and that is what
/// kept `verbs/recover.rs` calling `events::emit_recovery_verdict` directly: its
/// sweep walks LEDGER rows, and a ledger row is keyed by the session uuid when one
/// had resolved by write time and by `events::byname_key` (`byname-<name>`) when
/// one had not — a send made before a session id existed. No `SessionId` can name
/// the second kind, so a `SessionId`-addressed method could serve only part of its
/// own caller. Seven production sites mint a byname key, and
/// `delivery_recover_verb.rs` pins the behaviour.
///
/// An enum (`Session(id) | Name(n)`) was the obvious repair and is the wrong one:
/// the two halves are not exclusive. `events::reader_paths` takes BOTH and MERGES
/// the two files, because a session that acquired its id mid-life has records under
/// each key and the terminal for a byname-keyed send can land in the uuid-keyed
/// file. Collapsing to one would silently narrow that read. So the address carries
/// what the reader already takes, and [`writer_key`](LedgerAddress::writer_key)
/// states separately which single file a NEW record lands in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LedgerAddress {
    /// The session's id, when one had resolved by the time the records were
    /// written.
    pub session: Option<SessionId>,
    /// The target name, which keys the `byname-<name>` half of the ledger.
    pub name: Option<String>,
}

impl LedgerAddress {
    /// The common case: a send whose session id was known throughout.
    pub fn session(id: SessionId) -> Self {
        LedgerAddress {
            session: Some(id),
            name: None,
        }
    }

    /// A send that only ever had a name — `qd start -p` before its registry row,
    /// or a boot that failed before minting an id.
    pub fn byname(name: impl Into<String>) -> Self {
        LedgerAddress {
            session: None,
            name: Some(name.into()),
        }
    }

    /// Borrowed `(session_id, name)`, the shape every reader helper wants.
    pub fn parts(&self) -> (Option<&str>, Option<&str>) {
        (
            self.session.as_ref().map(|s| s.as_str()),
            self.name.as_deref(),
        )
    }

    /// The ledger key a WRITER binds to: the session id when there is one, else
    /// the `byname-` key. Mirrors which of the two files a new record lands in.
    pub fn writer_key(&self) -> Option<String> {
        match (&self.session, &self.name) {
            (Some(s), _) => Some(s.0.clone()),
            (None, Some(n)) => Some(crate::events::byname_key(n)),
            (None, None) => None,
        }
    }
}

// ===========================================================================
// Lifecycle
// ===========================================================================

/// What one create needs, in full.
///
/// # The five fields this type started with could not express a real create
///
/// Stage-2 phase 3 probed it against all seven lanes before implementing
/// [`LaneOps::start`], the same way phase 2 probed [`DeliverPolicy`] against
/// `deliver`. Five gaps came back, and each is a fact a lane needs that the
/// request had nowhere to put — the failure mode a ban-list guard cannot see (a
/// signature that is perfectly serializable and simply too narrow). All five are
/// repaired here; the sixth was [`SessionHandle::id`], repaired on that type.
///
/// 1. **[`StartRequest::prompt`]** — three lanes deliver the first turn IN-CORE
///    (`create_daemon::run_new_daemon` step 6 → `DaemonOutcome::first_turn_id`,
///    and `provider::acp::daemon::create_acp_daemon`'s `drive_create_prompt` for
///    both `acp/*` lanes). With no slot, `start` could only pass `None`, which is
///    a behaviour change rather than a missing feature. The four lanes that
///    refuse `-p` at create today keep refusing it — as
///    [`LaneError::NotSupported`], the honest form of the refusal.
/// 2. **[`StartRequest::await_relay`]** — `qd start`'s `--no-await-relay` opts
///    out of a DEFAULT-ON behaviour that feeds `ProviderFx::await_relay`. Without
///    the field the claude boot waiter falls back to its legacy env opt-in, which
///    is a real behaviour change and one `tests/start_surface_a.rs`
///    `a3_no_await_relay_flag_beats_env_optin` pins.
/// 3. **[`StartRequest::env`] / [`StartRequest::env_unset`]** — a `--via`
///    profile's credentials are env pairs that **by contract never touch argv**
///    (see [`crate::create::NewParams::backend_env`]), so they could not ride
///    `passthrough`. This is the same qd-resolves/qw-receives shape as
///    [`DeliverPolicy::render`]: qd materialises the profile, qw is handed the
///    result and never calls the resolver.
/// 4. **[`StartRequest::render`]** — the three pane lanes build a pane, and a
///    pane is inline or alt-screen. Already on the contract for `wake`/`deliver`
///    as of phase 2; `start` reuses it rather than growing a second spelling.
/// 5. **[`StartRequest::name`]** — was `Option<String>` with no defined meaning
///    for `None`. See the field.
///
/// # The second probe found nothing further HERE
///
/// Stage-2's probe was against the seven lane arms. The next one was against the
/// real caller — `verbs/lifecycle.rs`'s `run_new`, wired through
/// [`LaneOps::start`] for the first time — and this type came through it
/// **unchanged**: every fact `qd start` has to hand a create already has a field.
/// The three gaps that probe DID find were all on the way back out, which is the
/// half a request type cannot cover: [`SessionHandle::socket_dir`],
/// [`SessionHandle::notes`], and [`LaneError::StartFailed`]'s exit code and boot
/// phase. Recorded because "we probed and the answer was no change" is a result,
/// and the next person should not have to re-derive it.
///
/// One field is deliberately NOT filled by every lane, and it is a caller
/// decision rather than a gap: `resume` and `passthrough` reach only the claude
/// lane, because `--fork` seeds a claude transcript and `claudeArgs` is claude's
/// launch argv. The other four cores either refuse those inputs upstream or have
/// never had a field for them, so widening the request to carry them everywhere
/// would newly forward a `--` tail into codex's app-server and a claude fork uuid
/// into `pi --load-session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    pub cwd: PathBuf,
    /// REQUIRED, and the decision is recorded rather than left implicit.
    ///
    /// Every create core underneath takes a plain `String`
    /// ([`crate::create::NewParams`], `DaemonParams`, `CodexTuiParams`,
    /// `PiTuiParams`, `PiCreateParams`, `AcpCreateParams`) and clap makes the
    /// positional required, so `Option` had no producer and no consumer — a
    /// `None` reaching any lane could only become `""`, which the S2 name
    /// whitelist then rejects at the create boundary. That is a refusal spelled
    /// as an empty string.
    ///
    /// The alternative — `None` means "auto-mint a name" — was rejected, not
    /// forgotten. A name here is not a label: it keys the O_EXCL claim, names the
    /// mux pane, and is what `qd send <name>` addresses. Minting one inside qw
    /// would put the naming policy on the wrong side of a boundary whose whole
    /// point is that qd resolves human-facing identity and hands qw the result.
    pub name: String,
    pub model: Option<String>,
    /// Resume an existing session id rather than minting a new one.
    pub resume: Option<SessionId>,
    /// Everything after `--`.
    pub passthrough: Vec<String>,
    /// The first turn, delivered by the LANE where the lane's own create
    /// delivers it. See the type docs, gap 1.
    ///
    /// A lane that cannot honestly drive a create-time turn answers
    /// [`LaneError::NotSupported`] BEFORE anything is claimed or spawned, rather
    /// than accepting the prompt and dropping it. `None` is always accepted.
    pub prompt: Option<String>,
    /// Wait for the session's relay sidecar before calling the boot ready.
    ///
    /// DEFAULT ON (see the [`Default`] impl), because that is `qd start`'s
    /// contract: `--no-await-relay` is the opt-out, and the flag beats the
    /// `QD_BOOT_AWAIT_RELAY` env opt-in. Carried as a resolved `bool` for the
    /// [`DeliverPolicy::render`] reason — qd owns the flag and the env
    /// precedence, qw receives the answer. Consumed by the claude boot waiter
    /// through `ProviderFx::await_relay`; the six lanes with no relay sidecar
    /// ignore it, which is an answer and not a gap.
    pub await_relay: bool,
    /// `--via` composed backend-env pairs, materialised by qd.
    ///
    /// The VALUE of a credential lives only here in memory and in the 0600
    /// self-deleting env file the create writes — **never in argv**, which is why
    /// these could not be folded into [`StartRequest::passthrough`]. EMPTY (the
    /// ordinary case) means no env file and no prefix: byte-zero change.
    pub env: Vec<(String, String)>,
    /// Keys the env file must `unset -v` BEFORE the exports, so a `--via`
    /// session's whitelisted env is EXACTLY the composed set. EMPTY for every
    /// non-`--via` session. Mirrors [`crate::create::NewParams::backend_env_unset`].
    pub env_unset: Vec<String>,
    /// How to build the pane, for the three lanes that build one. PLAIN DATA,
    /// resolved by the caller — see [`DeliverPolicy::render`] for the full reason
    /// qw must not read the `render-default` config itself. The four daemon lanes
    /// ignore it: a resident has no pane.
    pub render: RenderMode,
}

impl Default for StartRequest {
    /// Matches today's `qd start` with no flags: relay-await ON, no prompt, the
    /// BUILT-IN render floor.
    ///
    /// `await_relay: true` is the field this impl exists for. `#[derive(Default)]`
    /// would answer `false` — silently the `--no-await-relay` behaviour — for
    /// every caller that reached for a default, which is exactly the
    /// too-narrow-signature failure this revision repaired. Same reasoning as
    /// [`DeliverPolicy::default`], and [`RenderMode::default()`] is inline there
    /// for the same reason: "no flag, no config", never "ignore the config".
    ///
    /// `name` defaults to EMPTY, which no lane accepts (the S2 whitelist rejects
    /// it at the create boundary). That is deliberate — a default-constructed
    /// request is a test scaffold, and a scaffold that silently created a session
    /// under an invented name would be worse than one that refuses.
    fn default() -> Self {
        StartRequest {
            cwd: PathBuf::new(),
            name: String::new(),
            model: None,
            resume: None,
            passthrough: Vec::new(),
            prompt: None,
            await_relay: true,
            env: Vec::new(),
            env_unset: Vec::new(),
            render: RenderMode::Inline,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    /// The provider session id — **OPTIONAL, because two of the seven lanes
    /// cannot fill it at create and never could.**
    ///
    /// `claude-code/mux-pane` mints UNBOUND at create: the provider uuid does not
    /// exist until claude writes its first registry row, which is what qd's bind
    /// phase waits for. `codex/mux-pane` is the same by DESIGN rather than by
    /// timing — `CodexTuiOutcome::thread_id` is `None` on a fresh start and the
    /// thread is identified later by the rollout backfill, pinned by
    /// `tests/codex_interactive_lane.rs`
    /// `interactive_codex_starts_unidentified_then_binds_its_thread`.
    ///
    /// The precedent is the field directly below: `pid` is already `Option`
    /// because a daemon-hosted session may never have one, and **nothing in this
    /// trait may REQUIRE an identity some lane structurally lacks**. A required
    /// `id` could only have been satisfied by minting a placeholder — an identity
    /// that was never observed, handed out as if it had been.
    ///
    /// `wake` always fills it: a revive keeps the id it was given.
    pub id: Option<SessionId>,
    /// The STABLE qd id (`qdId`), which — unlike [`SessionHandle::id`] — every
    /// lane does have at create.
    ///
    /// All seven mint it before the launch, because it must exist at env-bake
    /// time so the session's own `qd` invocations can self-identify
    /// (`QD_SESSION_ID`). Carrying it here is what lets `qd start --json` emit
    /// its `qdId` field from this call's RETURN VALUE; before this field the
    /// only way to learn it was for the caller to have minted it, which put the
    /// mint on the wrong side of the boundary for the four lanes whose cores
    /// mint internally.
    ///
    /// `None` on [`LaneOps::wake`], where no mint happens: a revive carries the
    /// id the row already has, and re-deriving it would be this type answering a
    /// question it was not asked.
    pub qd_id: Option<String>,
    /// OPTIONAL by design: a daemon-hosted session has thread-id identity and may
    /// never have a pid. Nothing in this trait may REQUIRE one.
    pub pid: Option<i64>,
    pub started_at_ms: Option<i64>,
    /// **The canonical socket dir the create actually landed in** — for the three
    /// PANE lanes. `None` for the four daemon lanes: a resident has no pane and no
    /// mux dir.
    ///
    /// This is a fact the create DECIDED, not one a caller may re-derive, and the
    /// difference is a named bug class. `qd start -p`'s priming send types into the
    /// pane through this dir, and its own dep doc says why it must be carried:
    /// *"the CANONICAL socket dir the session was just born in — never a
    /// re-resolved `ZMX_DIR`, which may point elsewhere (ADR 0009, Bug D)"*. Before
    /// this field the only way a caller could learn it was to have resolved the
    /// dirs itself and passed them IN — which is precisely what moving the create
    /// behind [`LaneOps::start`] takes away.
    pub socket_dir: Option<PathBuf>,
    /// Non-fatal notices the create produced, VERBATIM. EMPTY is the ordinary
    /// answer.
    ///
    /// The [`ReapObservations::notes`] precedent, and it closes the same hole on
    /// this method: the two `acp/*` lanes' create takes a `warn` callback, `qd
    /// start` routed it to stderr, and the lane arm — having no stdout of its own —
    /// **dropped it on the floor**. A lane may not print, so a notice it cannot
    /// return is a notice that ceases to exist. These are CLAUSES, not lines: the
    /// caller decides where they go and what wraps them.
    pub notes: Vec<String>,
}

/// What a wake DID — the distinction a [`SessionHandle`] alone cannot carry.
///
/// A handle answers "what is this session now", which is the same sentence
/// whether the lane revived something or found it already serving. The four
/// daemon revives DECIDE between those two at a specific instant, gated on
/// `pid_alive && endpoint_recorded && cmdline_is_ours`, and a caller cannot
/// reconstruct the verdict afterwards: comparing pids is invented logic, and
/// re-probing both reimplements the gate and races it. Reporting `Revived` for a
/// session that was never revived is a false statement of fact, so the verdict
/// is a value.
///
/// The three pane lanes always answer [`WakeState::Revived`] — a pane revive
/// relaunches unconditionally; it has no already-running arm to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WakeState {
    /// The lane looked, found the session ALREADY serving, and did nothing. A
    /// success no-op: no second process, no row mutation.
    AlreadyRunning,
    /// The lane brought the session back. Whatever it produced is in
    /// [`WakeOutcome::resident`] / [`WakeOutcome::pane`].
    Revived,
}

/// The resident a daemon wake left serving.
///
/// Both fields are the revive's OWN answer, not a re-read: a fresh resident
/// writes a new row, and re-resolving it by id afterwards asks the registry a
/// question the revive already answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resident {
    pub pid: i64,
    /// The ws endpoint the resident is serving on.
    pub endpoint: String,
}

/// The pane a pane wake built — where to attach a terminal to it.
///
/// `zmx_name` is the pane the revive actually CREATED, which is not the row's
/// name: it is derived inside the revive plan, and it is what the caller's
/// success line and its `qd attach <…>` pointer must both name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneHandle {
    pub zmx_name: String,
    pub socket_dir: String,
}

/// What [`LaneOps::wake`] answers.
///
/// Same split as [`KillReport`]: [`WakeOutcome::handle`] is what the lane can
/// honestly CLAIM about the session, and the rest is what the wake OBSERVED. The
/// signature returned the handle alone until this revision, and the four daemon
/// lanes lost their `AlreadyRunning`/`Revived` verdict and their endpoint on the
/// way out — facts the caller then had to invent or re-probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeOutcome {
    /// Revived, or already running? See [`WakeState`] for why this cannot be
    /// re-derived by the caller.
    pub state: WakeState,
    /// The session, as it stands after the wake. A revive KEEPS its id.
    pub handle: SessionHandle,
    /// The resident, for the four DAEMON lanes. `None` for a pane lane (which
    /// has none) and on [`WakeState::AlreadyRunning`] (nothing was produced —
    /// the row's recorded endpoint is unchanged and was never re-read).
    pub resident: Option<Resident>,
    /// The pane, for the three PANE lanes. `None` for a daemon lane: a resident
    /// has no pane.
    pub pane: Option<PaneHandle>,
}

/// A three-state answer, because "I could not tell" is a real outcome and must
/// not be flattened into `false`.
///
/// Introduced for [`KillOutcome`], where the flattening would actively mislead:
/// pi's `teardown_pi_daemon` returns `()` and silently declines to signal when
/// the pid's identity does not match, so "not reaped" and "cannot say whether it
/// was reaped" are genuinely different answers and a `bool` can only tell one of
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Confirmation {
    Yes,
    No,
    /// The lane performed the operation but cannot confirm this part of it.
    /// An honest non-answer, never a stand-in for `No`.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillOutcome {
    /// Was the process (or process group) actually signalled and gone?
    pub reaped: Confirmation,
    /// Was a tombstone written for the row?
    pub tombstoned: Confirmation,
}

/// Everything ONE reap saw, beside what it can honestly claim.
///
/// # What forced this type
///
/// [`KillOutcome`]'s two [`Confirmation`]s are **what a lane can honestly
/// claim** — the right answer to "did the kill work?", and deliberately the same
/// two words for all seven lanes. They are not, and cannot be, **what the
/// operation observed**, which is what a VERB needs in order to render. The
/// claim summary is a lossy projection of the observation, and the loss is not
/// recoverable downstream: `reaped: No` is the single answer for three
/// user-visible exit-1 messages (a failure clause, nothing to kill, a survivor),
/// and `reaped: Unknown` is the answer for a foreign pid whose NOTE ("belongs to
/// a different process … not signaled") exists nowhere else in the process.
///
/// So the observations ride ALONGSIDE the claim rather than being folded into
/// it. Nothing here is derived: every field is recorded at the instant the reap
/// made the decision it describes, over state the reap then destroys.
///
/// Rendering-neutral by construction — no field is a message, an exit code or a
/// format decision. `notes` and `failures` are the one exception and they are
/// VERBATIM CLAUSES the reap already produced, not lines: the caller decides
/// where they go and what wraps them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReapObservations {
    /// Non-fatal notes, VERBATIM. Exactly one today: the r7 foreign-pid strand
    /// note, which must be said out loud because the silent path would read as a
    /// kill. EMPTY is the ordinary answer.
    pub notes: Vec<String>,
    /// The "could not reap" clauses, VERBATIM. Non-empty means the caller's
    /// `Failed to fully reap …` line and exit 1.
    pub failures: Vec<String>,
    /// Neither a confirmed pane nor a pid — nothing was signalled, scanned or
    /// tombstoned. Distinct from a failure and from a survivor, all three of
    /// which are `reaped: No`.
    pub nothing_to_kill: bool,
    /// The post-verify scan found the targeted pane STILL PRESENT — the socket
    /// dir it was found in.
    pub survivor_dir: Option<String>,
    /// The RESOLVED pane name — the resolver's pane truth, NOT the row's name.
    /// It comes out of a three-fallback resolution performed INSIDE the reap
    /// over state the reap then destroys, so a caller cannot re-derive it.
    pub zmx_name: Option<String>,
    /// The pid the reap addressed (`0` = none).
    pub pid: i64,
    /// The socket dir was never confirmed, so the caller's success line carries
    /// its honesty marker.
    pub zmx_dir_unconfirmed: bool,
    /// **The daemon arms only** — was the resident alive AT THE INSTANT the
    /// signal decision was made? `None` for the three pane lanes, which have no
    /// such moment.
    ///
    /// It is NOT `is_pid_alive(pid)`, and the difference is the whole reason it
    /// is reported rather than re-probed: for codex and acp it is `pid > 0 &&
    /// is_pid_alive(pid) && cmdline_is_our_daemon(probe(pid), endpoint)`,
    /// computed inside the kill just before it signals. Recomputing it in the
    /// caller would be both a reimplementation of the identity guard and a race
    /// against the kill it describes.
    pub was_alive: Option<bool>,
}

/// What [`LaneOps::kill`] answers: the honest claim, and the observation.
///
/// See [`ReapObservations`] for why one type could not be both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillReport {
    /// What the lane can honestly CLAIM about the reap.
    pub outcome: KillOutcome,
    /// What the reap OBSERVED, for the caller that has to render it.
    pub observed: ReapObservations,
}

// ===========================================================================
// Observation
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    /// An OPAQUE display label (`"codex"`, `"acp/opencode"`), carried so `qd ls`
    /// can render a badge.
    ///
    /// Deliberately a `String` and not a `Lane`. A `Lane` here would hand qd a
    /// structured, exhaustively-matchable value and make
    /// `match summary.lane.harness { … }` the ergonomic thing to write — which is
    /// precisely the coupling this boundary exists to remove. Lane identity is
    /// qw's, and it does not cross.
    pub provider: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub status: SessionStatus,
    pub turns: u64,
    pub tokens: u64,
    pub last_active_ms: Option<i64>,
    pub git_branch: Option<String>,
}

/// Why a lane's enumeration is INCOMPLETE — one store it could not read.
///
/// Added in stage-2 phase 2's trait revision, because [`LaneOps::list`] had no
/// channel for "I looked and could not read it". The rejected alternative was
/// `Err(LaneError::Transport)`, and it is worth naming why it is wrong: an error
/// DISCARDS the rows the lane did read, turning a partial answer into no answer
/// — which is `quorum_core::discovery`'s rule ("never convert `EPERM` into
/// not-found") inverted, applied one layer up. A refused sqlite index and an
/// absent one must not produce the same empty list, and they must not produce a
/// missing list either. So degradation is a FIELD.
///
/// The shape is `quorum_core::discovery::AcquireFailure`'s, deliberately: `source`
/// names the concrete thing in the user's terms (`"~/.claude/projects"`), and
/// `detail` preserves the OS error verbatim. It is qw's own type rather than a
/// borrow of qd's — qd depends on qw, never the reverse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Degradation {
    /// The store that could not be read, named the way a user would name it.
    pub source: String,
    /// The underlying error, preserved (`"Permission denied (os error 13)"`).
    pub detail: String,
}

/// What [`LaneOps::list`] answers: the rows it read, AND what it could not read.
///
/// `degraded` EMPTY is the ordinary answer and means every store this lane
/// consults was read successfully — including a store that is simply not there,
/// which is a real answer and not a degradation (a user who does not run codex
/// has no codex root, and saying so is correct).
///
/// A non-empty `degraded` with a non-empty `sessions` is the case the whole type
/// exists for: "here are 40 rows, and the sqlite index was refused." Neither half
/// may be dropped.
///
/// # What still cannot be reported, stated rather than implied
///
/// Every per-provider scan below this type is L8-permissive down in its own body
/// — `scan_transcripts` and `opencode::sessions` degrade a malformed row, an
/// unreadable FILE, or a failed query to "contributed nothing" and return a
/// `Vec` with no error channel. Widening those is not this pass. What
/// [`crate::lane_read::list_for`] can honestly report today is exactly one thing:
/// the store ROOT was present and could not be opened. So a refused `opencode.db`
/// inside a readable store dir still reads as absent, and this type does not
/// pretend otherwise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub sessions: Vec<SessionSummary>,
    /// EMPTY unless a store this lane owns could not be read. See the type docs
    /// for the reads that still cannot tell unreadable from absent.
    pub degraded: Vec<Degradation>,
}

/// Where a health answer came from.
///
/// Carried so a caller can tell a live observation from a stale projection, and
/// so the conformance suite can assert a lane is not silently defaulting. The
/// pi/pane gap (BUG 1) is exactly a lane that returns `Idle` with no source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HealthSource {
    /// The registry row's own status string.
    RegistryStatus,
    /// Derived from the harness's transcript/rollout tail — needs no endpoint.
    TranscriptTail,
    /// A live round-trip to the resident.
    LiveRpc,
    /// The PROCESS was observed — pid liveness, a `(pid, starttime)` reuse
    /// check, an adapter's recorded `--listen` identity, a per-session daemon
    /// socket probe. No status the harness published was read at all.
    ///
    /// Added in stage-2 phase 1, because two of the seven lanes have no other
    /// source: `acp/*` health is entirely `acp_status_classify` (adapter pid +
    /// identity), and the `qd ls` liveness gate that downgrades a stale-live row
    /// to `cold` is a process observation overruling
    /// [`HealthSource::RegistryStatus`]. Folding either into `Synthesized` would
    /// have been the lane misattributing its own source — the exact
    /// silent-defaulting this enum exists to expose.
    ProcessLiveness,
    /// Computed by qd from its own queue state; the harness publishes nothing.
    Synthesized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub status: SessionStatus,
    pub source: HealthSource,
    pub observed_at_ms: Option<i64>,
}

// ===========================================================================
// Delivery
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// The send id, **minted by the caller** before the message crosses.
    ///
    /// # Why the id arrives instead of coming back
    ///
    /// It used to be minted inside the carrier (`events::mint_send_id`, called
    /// from `delivery::pty` and `delivery::pi`) and returned on
    /// [`Receipt::message_id`]. That made it impossible for qd to write its
    /// **intent** record — the qd half of the two-log ledger
    /// (`doc/tbd/provider-architecture/09-ledger-split.md`) — before the wire:
    /// qd had nothing to correlate on until `deliver` had already returned.
    ///
    /// Writing the intent record *after* `deliver` returns is not the same
    /// discipline. A send whose delivery hangs and whose writer is then killed
    /// would leave **no trace at all**, and that is precisely the case
    /// `qd delivery:recover` exists to close: its sweep enumerates qd's own
    /// `send-initiated` records and closes the ones whose writer incarnation is
    /// gone. No record, no sweep. So the id is minted first, the intent record is
    /// durable before the call, and the carrier uses the id it was handed.
    ///
    /// [`Receipt::message_id`] echoes this value back for the carriers that key
    /// their ledger on it. The three provider-id carriers (relay, codex/acp/pi
    /// residents) still answer with the id their RESIDENT minted, because that id
    /// is what their own ledger records and their stdout receipt carry — see
    /// [`Receipt::message_id`].
    pub id: MessageId,
    pub text: String,
    /// The sending session, when there is one.
    pub from: Option<String>,
}

/// Caller policy for one delivery.
///
/// `wake_if_cold` is EXPLICIT rather than defaulted (stage-1 decision 1): today's
/// `qd send` wakes a cold target before delivering, and passing `true` preserves
/// that contract — including its `failed{wake}` exit 12 — byte for byte. Making
/// it a field keeps wake visible in the request without splitting `deliver` into
/// two calls, which would reintroduce the race described on [`LaneOps::deliver`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverPolicy {
    pub wake_if_cold: bool,
    pub budget_ms: u64,
    /// How to build the pane if `wake_if_cold` actually fires. PLAIN DATA,
    /// resolved by the CALLER (stage-2 phase 2's second trait repair).
    ///
    /// A revive builds a FRESH pane, so it has to know whether to build it inline
    /// or alt-screen, and the answer is a user setting: `qd send` resolves
    /// `resolve_render_mode(None, render_default_from_config(env))` today and
    /// threads it into its wake. Before this field there was nowhere on the
    /// request to put it and qw could not go and read it — the config lives
    /// behind `dispatch::secrets`, moved OUT of this crate on purpose (see
    /// [`crate::launch`], above `resolve_render_mode`) so that the module
    /// `provider.rs` depends on would not drag the whole credential surface in.
    /// The concrete cost was a user with `render-default = "alt-screen"` getting
    /// an INLINE pane back from a wake through this boundary.
    ///
    /// Carrying the resolved VALUE is the pattern this codebase already
    /// established: [`crate::create::NewParams`] carries `render` and
    /// `interactive` as values because the clap parsing happened in qd. qd
    /// resolves, qw receives. Giving qw a config reader instead would invert the
    /// dependency the whole split exists to enforce.
    ///
    /// Ignored when no wake happens, which is the ordinary case.
    pub render: RenderMode,
}

impl Default for DeliverPolicy {
    /// Matches today's `qd send`: wake a cold target, 120s budget, and the
    /// BUILT-IN render floor.
    ///
    /// [`RenderMode::default()`] is inline, which is `resolve_render_mode`'s own
    /// last tier — so this default is "no flag, no config", not "ignore the
    /// config". A caller that HAS a config must resolve it and set the field; a
    /// caller that reaches for `Default` is saying it has no user setting to
    /// honour, which is true of every test and of the fixture.
    fn default() -> Self {
        DeliverPolicy {
            wake_if_cold: true,
            budget_ms: 120_000,
            render: RenderMode::Inline,
        }
    }
}

/// Whether this session has a carrier that could take a message — and if not,
/// why not.
///
/// TOPOLOGY, not turn state. It answers "is there a receive surface", never "is
/// the session mid-turn" and never "would a delivery steer or queue". See
/// [`LaneOps::receive_path`] for why that distinction is what makes the call safe
/// to compose when [`LaneOps::deliver`]'s own atomicity rule forbids composing
/// `health`.
///
/// Three states, and the third is the one the type exists for. Flattening
/// [`ReceivePath::Undetermined`] into [`ReceivePath::None`] would assert an
/// absence that was never observed — `quorum_core::discovery`'s war story, and the
/// same rule [`Terminal::Undetermined`] and [`Confirmation::Unknown`] already
/// carry one layer down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReceivePath {
    /// There is a carrier. WHICH one is deliberately not said — that is
    /// [`LaneOps::deliver`]'s private choice, and naming it here would hand the
    /// caller something to pass back in.
    Available,
    /// The lane LOOKED and there is nothing to receive through. A positive
    /// observation of absence, never an un-read.
    None { reason: String },
    /// The read that would have found a receive path was refused, so presence is
    /// UNKNOWN. Not the same as having none, and qd renders it as its own refusal
    /// class (`refused{receive-path-undetermined}`) rather than the flat "has no
    /// live receive path" claim.
    Undetermined { reason: String },
}

/// Whether a terminal is ever coming for this send.
///
/// The one thing that genuinely has to cross the boundary. qd owns
/// `send --wait` and `qd wait`, so it must be able to tell "the terminal is
/// pending" from "this send can never be confirmed" — otherwise `--wait` blocks
/// to timeout on a message that was in fact delivered.
///
/// The motivating case is real: a pi session whose first turn has not happened
/// has written NOTHING to disk (`provider/pi/tui.rs:33-64`), so its landing
/// cannot be confirmed at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalExpectation {
    /// A terminal is coming; [`LaneOps::await_terminal`] will resolve it.
    Pending,
    /// This send can never be confirmed. Not a failure — an honest limit.
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub message_id: MessageId,
    /// The lane took responsibility for the message. NOT a claim that it landed.
    pub accepted: bool,
    pub terminal: TerminalExpectation,
    /// Did this delivery have to REVIVE the target first?
    ///
    /// Stage-2 phase 2's first trait repair, and it exists to serve one caller:
    /// qd's disposition ledger. `send_unified::deliver_with_durability` stamps a
    /// `queued` row when the lane reports a wake and the live path stamps none, so the PRESENCE
    /// of `queued` is how the ledger records that a wake happened. Fold the wake
    /// inside an atomic [`LaneOps::deliver`] and qd can no longer know whether to
    /// stamp it — the only way to find out would be to ask [`LaneOps::health`]
    /// first, which is exactly the composition `deliver`'s atomicity rule forbids.
    /// Reporting it on the receipt is what lets the rule and the ledger both hold.
    ///
    /// **The stamping rule qd follows**, in full, because half of it is not on
    /// this type: stamp `queued` when the lane says a wake HAPPENED — that is
    /// `woke` being [`Confirmation::Yes`] or [`Confirmation::Unknown`] here, OR a
    /// returned [`LaneError::WakeFailed`], which is only ever produced by a wake
    /// that was attempted. Both pinned funnels survive: `attempted, queued,
    /// delivered` and `attempted, queued, delivery-failed{wake}`.
    ///
    /// **The cost, paid knowingly.** `queued` is now stamped AFTER the wake
    /// resolves rather than before it is tried, so its `created_at` is no longer
    /// the moment the message was placed durably awaiting the wake — it is the
    /// moment qd learned that a wake had been attempted, which is a whole revive
    /// later. R14.1 keeps holding as written (`created_at` is when THIS host
    /// RECORDED the event, and there is still no retro-dating); what stops
    /// holding is the transport-format schema's parenthetical that for the
    /// qd-driven events "record time and happen time coincide to within ms". That
    /// deviation is written down where a reader of the ledger will meet it:
    /// `doc/formats/dispatch-transport-formats.md` (the `created_at` field
    /// description and the R14.1 row), `quorum_dispositions::DispositionEvent::queued`,
    /// and the stamp site itself in `send_unified::deliver_then_stamp`.
    ///
    /// [`Confirmation::Unknown`] is a real answer here and not a stand-in for
    /// `No`: a lane that revived something but cannot confirm the revive must say
    /// so, and qd stamps `queued` on it — an unconfirmed wake is still a wake
    /// attempt, and the ledger's question is whether one happened.
    pub woke: Confirmation,
}

/// What one **session** was doing, as an idle-watcher observed it.
///
/// # Its subject is the SESSION, not a message
///
/// That is the whole reason this is not [`Terminal`]. `qd wait`'s four
/// per-provider bodies answer *has this session gone idle*; [`Terminal`] answers
/// *did THIS message reach a terminal*. The two questions have different
/// subjects, and mapping `qd wait`'s exit 0 onto [`Terminal::Seen`] would claim a
/// message was seen on evidence that never mentioned it — see
/// [`LaneOps::await_idle`] and ruling D2.
///
/// # The entry/loop split is load-bearing, and it is a RENDERING fact
///
/// [`TurnState::IdleAtEntry`] and [`TurnState::WentIdle`] are both "the session is
/// idle and the caller may proceed". They are separate variants because the caller
/// renders them differently and cannot recover the difference: a watcher that
/// found the session already idle never opened a progress line, so its answer is a
/// whole line (`<label> is idle`), while a watcher that waited has a line open and
/// answers with the word that closes it (` done`). Collapsing them would make the
/// verb print either a stray progress line or an orphaned suffix.
///
/// # Nothing here is written to the ledger
///
/// Ruling D8: **a budget belongs to the caller; the ledger belongs to the
/// session.** [`TurnState::BudgetElapsed`] is a fact about the observer, so no
/// variant of this type corresponds to a record. The ACP and pi observers DO emit
/// `message-seen` while producing these answers, and that is not a contradiction:
/// they emit because they OBSERVED a send land in the recipient's own transcript
/// or rollout, which is evidence about the send. What is forbidden — and what
/// `await_terminal` was doing before 3A — is emitting *because the budget elapsed*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TurnState {
    /// The session was ALREADY idle when the watcher arrived; no turn was waited
    /// on and no progress was reported.
    IdleAtEntry,
    /// The session was busy and went idle while this watcher observed it.
    WentIdle,
    /// The budget elapsed with the session still busy.
    ///
    /// Says nothing about the session beyond "not idle yet", exactly as
    /// [`Terminal::TimedOut`] says nothing about delivery — and, per D8, **nothing
    /// was written** on the way to answering it.
    BudgetElapsed,
    /// The session's own process is gone (the claude pid-file's status says so),
    /// so no turn will ever complete.
    SessionExited,
    /// The observation transport ended before the session went idle.
    ///
    /// Distinct from [`TurnState::SessionExited`]: the session may well be alive
    /// and busy — what died is the watcher's view of it. Never a false "went
    /// idle".
    ChannelClosed,
    /// The turn ended in a correlated protocol ERROR rather than a completion.
    ///
    /// ACP's `TerminalError` is the only producer today. It is a terminal — the
    /// turn is over — but an operator scripting `qd wait X && next` must not
    /// proceed on it, so it is its own variant rather than a flavour of
    /// [`TurnState::WentIdle`].
    TurnFailed { detail: String },
    /// The watcher looked and **could not tell**.
    ///
    /// The fourth boundary to force this shape, after [`Confirmation::Unknown`],
    /// [`Terminal::Undetermined`] and [`ReceivePath::Undetermined`] — designed in
    /// here rather than discovered later. A caller may not read it as either
    /// answer: "I could not tell" is not "still busy" and is emphatically not
    /// "idle".
    Undetermined { reason: String },
}

/// The resolved end state of one delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Terminal {
    /// Confirmed present in the recipient's own transcript/context.
    Seen,
    /// Confirmed NOT delivered. Honest non-delivery, never a silent success.
    NotDelivered { reason: String },
    /// Something landed but not what was sent (truncation).
    Mismatch,
    /// The budget elapsed with no terminal. Says nothing about delivery.
    TimedOut,
    /// qw looked and **could not tell** — ask again later.
    ///
    /// Distinct from [`Terminal::NotDelivered`], and the distinction is
    /// load-bearing. `recovery_read`'s own contract is that `Abandoned`
    /// ("searched, no candidate matched") is the ONLY verdict that may foreclose a
    /// send. Two of its outcomes are explicitly not foreclosing — the transcript
    /// was unreadable, or the recipient has not yet progressed past the send and
    /// the search window is still growable. Collapsing either into `NotDelivered`
    /// would close out a send that may still resolve.
    ///
    /// Same shape as [`Confirmation::Unknown`]: "I could not tell" is an answer,
    /// and flattening it into a negative is a correctness bug, not a
    /// simplification.
    Undetermined { reason: String },
}

// ===========================================================================
// Interactive handoff
// ===========================================================================

// (Attach carries no DTO: see `LaneOps::attach`, which performs the handoff
// rather than describing it.)

// ===========================================================================
// Errors
// ===========================================================================

/// Why a lane operation did not succeed.
///
/// [`LaneError::NotSupported`] is what makes a uniform interface HONEST: a lane
/// that cannot do something says so in the type system instead of silently
/// no-opping. That is the failure mode the current `Provider` trait has with
/// `resume_args`'s `fork` (meaningful for 2 of 5 impls, ignored by 3) and
/// `inject`'s `from` (meaningful for 2 of 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaneError {
    /// This lane structurally cannot do this. Not a bug, not a TODO — the
    /// capability does not exist and never will (an ACP bridge has no terminal
    /// to attach). Answering this is a CORRECT outcome.
    NotSupported { op: String, reason: String },
    /// This lane should do this, and does not yet.
    ///
    /// Deliberately distinct from [`LaneError::NotSupported`]: that one is an
    /// answer, this one is a debt. Every occurrence is a decision waiting to be
    /// made — implement it, or delete the method from the contract because
    /// nothing actually needs it. `detail` carries the specific blocker so that
    /// decision can be made from the error text alone.
    ///
    /// The inventory of these is `doc/tbd/provider-architecture/07-lane-gaps.md`.
    NotImplemented { op: String, detail: String },
    /// No session with that id in this lane.
    NotFound { id: SessionId },
    /// The session exists but is not live, and the caller did not ask for a wake.
    Cold { id: SessionId },
    /// The lane's transport failed. Never dressed up as a successful delivery.
    Transport { detail: String },
    /// A revive was attempted and could not succeed (`failed{wake}`, exit 12).
    ///
    /// `exit_code` is the REVIVE CORE's own `exit_code()`, carried rather than
    /// re-invented. Every revive core underneath has one, they are not all the
    /// same number, and flattening them here would have made every wake failure
    /// exit 1 the moment a verb started routing through this trait — a silent
    /// narrowing of a user-visible contract. A caller with no exit code of its
    /// own to keep may ignore it (`qd attach`'s cold arm does, deliberately: its
    /// handoff has always answered 1).
    ///
    /// **`detail` is the core's own message, and `self_attributed` says whether
    /// it is a COMPLETE line.** `false` means the caller must stamp `qd <verb>: `
    /// on the front; `true` means the core already wrote the whole line (an
    /// `ERROR: …` refusal, or a resident's `qd resume: …` Display) and stamping
    /// anything would double-attribute it.
    ///
    /// This pair exists because **a lane has no verb and must not invent one.**
    /// `resume`, `attach` and `send` all reach the same revives, and every one of
    /// those lines names the command the user typed. The lane once stamped the
    /// literal `wake` — a command nobody can type — and wrapped the result in
    /// `could not revive <lane> session <id>:`, so a `qd resume` failure read
    /// `qd resume: could not revive claude session "…": qd wake: …`. Carrying the
    /// body plus this flag is what lets the verb print exactly the line it
    /// printed before any of this existed.
    WakeFailed {
        detail: String,
        exit_code: i32,
        self_attributed: bool,
    },
    /// The lane declined for a policy reason (bad argv shape, unsupported flag).
    Refused { detail: String },
    /// **A create did not happen** — the [`LaneOps::start`] twin of
    /// [`LaneError::WakeFailed`], and it exists for the same reasons.
    ///
    /// - **`exit_code` is the create core's OWN.** Every create core underneath
    ///   has an `exit_code()`; `NewError`, `CodexTuiError`, `PiTuiError`,
    ///   `DaemonError`, `PiCreateError` and `AcpCreateError` all answer `1`, but
    ///   `mux_selector`'s `QD_MUX_INVALID_EXIT` is **2** and reaches `start`
    ///   through the pane arms' dir resolution. Flattening every create failure to
    ///   `1` would have silently narrowed a user-visible contract the moment a
    ///   verb started routing through this trait — the exact defect
    ///   [`LaneError::WakeFailed`]'s doc records for revives.
    /// - **`detail` is a COMPLETE line.** Unlike a revive, a create has exactly
    ///   ONE verb — `qd start` — so the core stamping its own `qd start: `
    ///   attribution is not a lane inventing a verb, and `create::NewError`'s
    ///   `Display` has always done it. The arms stamp the two error types that are
    ///   SHARED with the revive path (`CodexTuiError`/`PiTuiError`, whose `Display`
    ///   is body-only because `qd resume`/`qd attach` reach them too), so the
    ///   caller prints this verbatim and never has to decide.
    /// - **`boot_phase` is the boot waiter's own verdict**, `Some` only for the
    ///   boot-timeout class. `qd start -p` files a `priming-readiness-timeout`
    ///   record whose `phase` field is exactly this, and `crate::boot::BootPhase`
    ///   exists precisely so that phase is read TYPED rather than string-matched
    ///   out of `detail` (the m-4 coupling, ack3-spec §8). A boundary that dropped
    ///   it would restore the coupling with no compiler complaint.
    ///
    /// **Nothing was created.** Every create core's own error contract already
    /// says so — each one either creates nothing or reaps what it made, except
    /// `NewError::BootTimeout`, which leaves a session RUNNING and says so in its
    /// own `Display`.
    StartFailed {
        detail: String,
        exit_code: i32,
        boot_phase: Option<crate::boot::BootPhase>,
    },
}

impl std::fmt::Display for LaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaneError::NotSupported { op, reason } => {
                write!(f, "{op} is not supported by this lane: {reason}")
            }
            LaneError::NotImplemented { op, detail } => {
                write!(f, "{op} is not implemented for this lane yet: {detail}")
            }
            LaneError::NotFound { id } => write!(f, "no such session: {}", id.0),
            LaneError::Cold { id } => write!(f, "session {} is not live", id.0),
            LaneError::Transport { detail } => write!(f, "transport failed: {detail}"),
            // Deliberately does NOT consult `self_attributed`: this is the
            // fallback rendering for a caller that has no verb to stamp, and it
            // stamps nothing of its own. A verb prints the two-arm form instead
            // — see the variant.
            LaneError::WakeFailed { detail, .. } => write!(f, "could not revive: {detail}"),
            LaneError::Refused { detail } => write!(f, "refused: {detail}"),
            // Deliberately bare: `detail` is already the complete `qd start: …`
            // line, so anything this added would double-attribute it. Same posture
            // as `WakeFailed` above, one step further — a create has one verb, so
            // there is nothing left for a fallback rendering to say.
            LaneError::StartFailed { detail, .. } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for LaneError {}

// ===========================================================================
// The trait
// ===========================================================================

/// What every `(harness, mode)` lane can do.
///
/// There is deliberately **no `lane()` accessor**. The only way to obtain a
/// `&dyn LaneOps` is to ask the registry for one *by lane*, so such a method
/// could only ever report back something the caller just said — and it would hand
/// out a [`Lane`](crate::lane::Lane), the one type this boundary keeps on qw's side. Implementations
/// carry an inherent `LANE` constant for their own self-checks; that is not part
/// of this interface.
///
/// Twelve methods. Ten, plus [`LaneOps::receive_path`] (which repaired the one
/// finding phase 4 left open — see its docs) and [`LaneOps::await_idle`] (ruling
/// D2: `qd wait`'s question has a different SUBJECT from
/// [`LaneOps::await_terminal`]'s, so it got a method rather than a widening).
/// Implementations DELEGATE to the existing per-provider code — nothing here is
/// reimplemented. Effects are injected at construction, so the signatures stay
/// serializable.
pub trait LaneOps {
    // --- lifecycle -------------------------------------------------------

    /// Create a session in this lane.
    ///
    /// Takes the WHOLE request, because five of its fields are facts a real
    /// create needs and the earlier five-field version could not carry — see
    /// [`StartRequest`] for each one and why passing `None` in its place was a
    /// behaviour change rather than a missing feature.
    ///
    /// **Implemented for all seven lanes** (stage-2 phase 3), as an exhaustive
    /// `match (harness, mode)` — the shape [`LaneOps::kill`] and [`LaneOps::wake`]
    /// already use, and for the same reason: it replaced a five-arm ordered
    /// `if`-chain whose ordering was enforced only by a comment, and two of whose
    /// mis-swaps were SILENT (`--interactive` ignored, a headless resident handed
    /// back, exit 0). The routing itself is [`Lane::for_create`](crate::lane::Lane::for_create).
    ///
    /// A returned handle may have **no [`SessionHandle::id`]**: two lanes are not
    /// identified at create and never could be. It always has a
    /// [`SessionHandle::qd_id`].
    ///
    /// Failures answer [`LaneError::StartFailed`], which carries the create core's
    /// own exit code and — for the boot-timeout class — the typed boot phase. The
    /// impossible `(harness, mode)` pairs answer [`LaneError::NotSupported`]; those
    /// are the only two variants a caller has to render.
    fn start(&self, req: &StartRequest) -> Result<SessionHandle, LaneError>;

    /// Revive a cold session into a drivable one, keeping its id.
    ///
    /// `render` is the CALLER's resolved render mode, carried as plain data for
    /// the reason spelled out on [`DeliverPolicy::render`]: a revive builds a
    /// fresh pane, the pane must be inline or alt-screen, and the answer is a
    /// user setting qw must not read for itself. A caller with no setting to
    /// honour passes [`RenderMode::default()`].
    ///
    /// `cwd_override` is `qd resume --cwd <dir>`'s value — the F3 reality-check
    /// escape for a project directory that moved. ONE lane consumes it (claude's
    /// revive threads it all the way to the detached launch); the other six pass
    /// what they already pass, which is an answer and not a gap. It is here
    /// because the alternative was to validate the override in the verb and then
    /// silently discard it at the lane — the same defect class `render` was added
    /// to fix, on the same method.
    ///
    /// Answers a [`WakeOutcome`], not a bare handle: see that type for the two
    /// facts a handle could not carry (the `AlreadyRunning`/`Revived` verdict,
    /// and what the revive produced).
    fn wake(&self, id: &SessionId, render: RenderMode, cwd_override: Option<String>) -> Result<WakeOutcome, LaneError>;

    /// Reap the session.
    ///
    /// Answers a [`KillReport`] rather than a bare [`KillOutcome`]: the two
    /// [`Confirmation`]s are what a lane can honestly CLAIM, and a verb needs
    /// what the reap OBSERVED. See [`ReapObservations`] for the six facts the
    /// claim summary cannot carry, and why none of them is re-derivable after
    /// the reap has run.
    fn kill(&self, id: &SessionId) -> Result<KillReport, LaneError>;

    // --- observation -----------------------------------------------------

    /// Every session this lane owns, plus whatever it could not read.
    ///
    /// Kept on the lane rather than on a global join (stage-1 decision 2): the
    /// registry fans out across lanes, which is the shape a future out-of-process
    /// qw needs.
    ///
    /// Returns a [`Listing`] rather than a bare `Vec` (stage-2 phase 2's third
    /// trait repair) so a lane that read 40 rows and was REFUSED one store can
    /// report both halves. See [`Listing`] for why an error was the wrong repair,
    /// and for what still cannot tell unreadable from absent.
    fn list(&self) -> Result<Listing, LaneError>;

    fn health(&self, id: &SessionId) -> Result<Health, LaneError>;

    // --- delivery --------------------------------------------------------

    /// Is there a carrier that could take a message for this session — and if
    /// not, why not? SIDE-EFFECT-FREE.
    ///
    /// It answers that ONE question. It does not deliver, does not wake, does not
    /// say which carrier, and does not report turn state.
    ///
    /// # Why this does not reopen the race [`LaneOps::deliver`]'s atomicity rule guards
    ///
    /// Read that rule first. It protects exactly one thing: *the lane reads live
    /// **turn state** internally to choose between **steering and queuing**, and
    /// that choice must not be composable by the caller.* Receive-path is not turn
    /// state — it is TOPOLOGY, the shape of the session's wiring — and the two
    /// staleness windows differ in KIND, not merely in size:
    ///
    /// - A stale **turn-state** answer produces a silently wrong delivery MODE:
    ///   queued when it should have steered, with no error anywhere. Nothing in
    ///   the system ever learns it was wrong.
    /// - A stale **receive-path** answer is SELF-CORRECTING. If the path dies
    ///   between this call and `deliver`, `deliver` returns
    ///   [`LaneError::Transport`] and qd stamps `delivery-failed{delivery}` —
    ///   exactly what already happens today when a carrier dies mid-send. No new
    ///   failure mode is introduced; the existing one absorbs it.
    ///
    /// # The discipline that makes it safe, and it is enforced MECHANICALLY
    ///
    /// **The answer may never be passed into [`LaneOps::deliver`].** `deliver`
    /// still determines the receive path itself, internally, from its own fresh
    /// read. This method exists solely so qd can render a refusal *before* it
    /// commits an intent record — qd's delivery is write-then-deliver (the
    /// envelope is appended BEFORE the carrier and a failed append is fatal),
    /// while `deliver` is atomic, so without this call a refusal that logs nothing
    /// today would have to log an envelope first.
    ///
    /// Because the caller structurally CANNOT hand the answer back, there is no
    /// time-of-check dependency for `deliver` to be wrong about. That is the whole
    /// argument, and a prose rule with no guard is not a rule: the source scan in
    /// `super::conformance` asserts `deliver`'s signature never grows a
    /// [`ReceivePath`] parameter.
    ///
    /// # Why [`ReceivePath::Undetermined`] is a variant rather than an error
    ///
    /// It is what gives qd's `refused{receive-path-undetermined}` a real channel.
    /// [`LaneError`] has no slot for "the read was denied, so presence is
    /// unknown" — the lane's honest version of that rule
    /// (`crate::lanes::LaneImpl::relay_port_for`, which refuses rather than
    /// answering `None` on a denied `ps`) collapses into
    /// [`LaneError::Transport`], and recovering a refusal CLASS by string-matching
    /// a `detail` would be worse than the duplication being removed.
    ///
    /// # What each lane answers
    ///
    /// claude/pane is the interesting one and runs the same pure decider
    /// `deliver` uses internally (`crate::lanes::claude_carrier`): a recorded
    /// relay port or a joined mux pane is [`ReceivePath::Available`], a denied
    /// process read is [`ReceivePath::Undetermined`], and neither-in-hand is
    /// [`ReceivePath::None`]. The two other pane lanes ask only for the joined
    /// pane. The three daemon lanes answer [`ReceivePath::Available`]
    /// unconditionally — their reachability refusal belongs to the carrier that
    /// re-reads the endpoint, and hoisting it up here would move ledger bytes.
    fn receive_path(&self, id: &SessionId) -> Result<ReceivePath, LaneError>;

    /// Deliver one message. ATOMIC by contract.
    ///
    /// The lane reads live turn state internally to choose between steering and
    /// queuing a follow-up. That choice must NOT be composable by the caller: if
    /// a caller could ask [`LaneOps::health`] and then `deliver`, it would
    /// reintroduce a time-of-check race this design exists to avoid. It is also
    /// already internal today — there is no user-facing steer flag, and
    /// `PiProvider::inject` makes the call from a live `is_streaming` read.
    ///
    /// # Phase-2 gate: what claude/pane proved, and what it cost
    ///
    /// Stage-2 phase 2 probed this method against the one lane that must choose
    /// between two carriers internally, then implemented it there. Three of its
    /// four claims held on first contact, and they held on real delivery code
    /// rather than by construction:
    ///
    /// - **[`Receipt`] fits.** Both carriers mint an id, and — this is the part
    ///   that was not obvious — they mint it into the SAME namespace. The relay's
    ///   id comes back from `ClaudeProvider::inject`; the PTY's comes from
    ///   `events::mint_send_id`. They share no format, and it does not matter:
    ///   both are written as `Payload::SendInitiated.send_id`, which is the key
    ///   `sendpty::watch_terminal` and [`crate::events::recovery_read`] already
    ///   join on. [`MessageId`] being opaque is what makes that work.
    /// - **[`TerminalExpectation`] fits**, and both carriers are honestly
    ///   `Pending`: the embedded mux owns the terminal after its handoff, and the
    ///   relay's arrives from the recipient's own transcript observer.
    ///   `Unavailable` is simply unused by this lane — correct, not a gap.
    /// - **Steer-vs-queue is already internal**, so that half of atomicity is
    ///   free. `decide_send_pty` runs inside the PTY body; the relay makes no such
    ///   choice at all (the relay server queues, the recipient pulls). The
    ///   *carrier* choice was qd's (`send_unified::select_carrier`, now retired), and moving it
    ///   inside is a straight improvement — this method takes a [`SessionId`], so
    ///   the lane re-reads the row itself instead of trusting a snapshot the
    ///   caller resolved earlier.
    ///
    /// **What did not fit was [`DeliverPolicy`], on two counts — and the gate's
    /// whole purpose was to fix them once rather than seven times. Both are
    /// fixed.**
    ///
    /// 1. `wake_if_cold` collided with qd's ledger.
    ///    `send_unified`'s retired `wake_then_deliver` stamped `queued` between the envelope
    ///    append and the wake — deliberately, as "a witnessed moment stamped
    ///    BEFORE the wake is tried" — and the live path stamps no such row. So the
    ///    presence of `queued` is how the ledger records that a wake happened.
    ///    Folding the wake inside an atomic `deliver` left qd unable to know
    ///    whether to stamp it: the only way to find out was to ask
    ///    [`LaneOps::health`] first, which is exactly the composition this
    ///    method's atomicity rule forbids.
    ///
    ///    **REPAIRED** by [`Receipt::woke`], with `queued` stamped
    ///    retrospectively. Both pinned funnel orderings survive verbatim
    ///    (`attempted, queued, delivered` and `attempted, queued,
    ///    delivery-failed{wake}`) because a failed wake is still a wake that
    ///    happened — see [`Receipt::woke`] for the full stamping rule and for the
    ///    real cost, which is that `queued`'s `created_at` stops being the moment
    ///    the message was placed awaiting the wake.
    ///
    /// 2. There was no slot for the render mode, so a wake through this boundary
    ///    silently ignored `render-default = "alt-screen"` and handed back an
    ///    inline pane. **REPAIRED** by [`DeliverPolicy::render`] plus the `render`
    ///    argument on [`LaneOps::wake`], both carrying qd's resolved
    ///    [`RenderMode`] as plain data. See [`DeliverPolicy::render`] for why the
    ///    alternative — letting qw read the config — was never available.
    ///
    /// A third item surfaced from the same pass, in the read half:
    /// [`LaneOps::list`] had no channel for "I looked and could not read it".
    /// **REPAIRED** by [`Listing`].
    ///
    /// # What is implemented
    ///
    /// **All seven lanes.** claude/pane came first and alone, deliberately: it is
    /// the lane that must choose between two carriers internally, which is the
    /// "carrier heterogeneity becomes private" claim the whole split rests on, and
    /// probing the trait against it is what surfaced the two `DeliverPolicy`
    /// repairs above. Phase 4 added the other six once their carriers were widened
    /// to report the turn id they had been minting all along
    /// (`send_relay::run_{codex,acp,pi}_send`, plus pi's floor sub-lane).
    ///
    /// Seven lanes reach five carriers — the PTY one serves all three pane lanes
    /// and one ACP one serves both bridges — and the lane does the choosing. The
    /// choice that used to live in `send_unified::select_carrier` lives inside
    /// [`crate::lanes::LaneImpl::deliver`] now — keyed on the LANE rather than on
    /// a provider string plus a hosting re-derivation — and qd's copy is deleted
    /// rather than kept in step.
    ///
    /// ALL FIVE CARRIERS ARE qw's OWN CODE as of stage-3 phase 3B
    /// ([`crate::delivery`]); `deliver` calls them directly. The `Carriers`
    /// callback trait that used to reach UP into the `qd` binary for their bodies
    /// is deleted, and with it `lane_ops_with_carriers` — `crate::lanes::lane_ops`
    /// is the only constructor, and no `qw` code calls into `qd`.
    ///
    /// # The finding phase 4 left open: atomicity vs. the write-then-deliver ordering
    ///
    /// **DECIDED, and the decision is [`LaneOps::receive_path`].** The shape of
    /// the problem, kept because the reasoning is what the answer has to satisfy:
    ///
    /// qd logs the envelope BEFORE the carrier runs and treats a failed append as
    /// fatal. But a transport-shape refusal — a live claude with neither relay nor
    /// pane — is an immediate exit-1 that logs NO envelope and stamps NO
    /// disposition, pinned as such. qd could have both only because it asked
    /// `send_unified::select_carrier` first and appended only then. Fold that
    /// verdict inside an atomic `deliver` and the append must come first, so a
    /// refusal that logs nothing today would log an envelope — the ledger's bytes
    /// move.
    ///
    /// The escape of asking [`LaneOps::health`] first and then delivering IS the
    /// time-of-check composition this method forbids, and the two options left
    /// were a receipt-shaped "refused before I took responsibility" or a
    /// pre-flight the contract explicitly sanctions and bounds. The second one was
    /// taken: `receive_path` is that sanctioned pre-flight, bounded to TOPOLOGY
    /// (never turn state) and forbidden — mechanically, by the source scan in
    /// `super::conformance` — from ever becoming an INPUT here. See its docs for
    /// why a stale topology answer is self-correcting where a stale turn-state
    /// answer is not.
    ///
    /// The second, smaller half rode along and is repaired by the same type: qd's
    /// refusal for this case downgrades to `refused{receive-path-undetermined}`
    /// when the read that would have found a receive path was DENIED, and
    /// [`LaneError`] has no channel for that distinction — the lane's honest
    /// version of the rule collapses into [`LaneError::Transport`].
    /// [`ReceivePath::Undetermined`] is that channel.
    ///
    /// # The LIVENESS DISAGREEMENT, and why `wake_if_cold: false` attempts
    ///
    /// Two components answer "is this session live" differently, and **both are
    /// deliberate**:
    ///
    /// - This method's implementation reads [`LaneOps::health`], which for
    ///   claude/pane runs `liveness::gated_ls_status_headless` — status PLUS
    ///   `(pid, start_time)` through the `qd ls` liveness gate, because that is
    ///   the answer `qd ls` renders.
    /// - `send_unified::is_live` reads the JOIN's **status enum alone**, ungated.
    ///
    /// A row with `status: "idle"` and a pid that is gone reads LIVE to `qd send`
    /// and COLD to `health`. `tests/verbs_a4.rs`'s
    /// `send_live_unroutable_claude_is_unchanged_no_wake_no_envelope` fixture —
    /// `{"pid":90101,"status":"idle"}`, forged dead — is exactly that row, and
    /// today `qd send` refuses it with exit 1, **no wake, no envelope, no
    /// disposition**.
    ///
    /// So the implementation reads `health` ONLY to decide whether to wake, and
    /// reads it only when `wake_if_cold` is set. `wake_if_cold: false` therefore
    /// **attempts the delivery** and lets the carrier report, rather than
    /// answering [`LaneError::Cold`] off a projection the caller never asked
    /// about. Refusing there would turn that one fixture row into either a silent
    /// wake (`attempted, delivery-failed{delivery}` becoming `attempted, queued,
    /// delivered` — the ledger's bytes moving) or a `Cold` the live path has no
    /// refusal class for. Attempting means the carrier fails naturally and qd
    /// stamps `delivery-failed{delivery}`, which is what happens today.
    ///
    /// **Reconciling the two readings is deliberately DEFERRED to its own
    /// commit.** Which reading is `qd send`'s truth decides whether a stale-live
    /// row gets revived; that is a user-visible change to `qd send` and must not
    /// arrive buried in a refactor. It is not a gap in this contract.
    ///
    /// # The caller-side retirement HAS landed
    ///
    /// This doc used to end "what has NOT happened is the caller-side
    /// retirement", listing `send_unified::select_carrier`, `RealWaker`,
    /// `dispatch_selected` and `UnifiedCarrier` as still alive. All four are
    /// deleted. `qd send`'s path is: resolve → [`LaneOps::receive_path`] → refuse
    /// with NO envelope if there is none → append the envelope → this method →
    /// stamp from the [`Receipt`]. `RealWaker` going with them is what closed BUG
    /// 2 (see `crate::conformance`): its missing `("pi", Some(MuxPane))` arm WAS
    /// the bug, and [`LaneOps::wake`] — which this method calls — has one.
    fn deliver(
        &self,
        id: &SessionId,
        msg: &Message,
        policy: &DeliverPolicy,
    ) -> Result<Receipt, LaneError>;

    /// Block until this message reaches a terminal, or the budget elapses.
    ///
    /// A CALL, never a file read: qd must not read the lane's log, because a
    /// shared file is a wire protocol with none of the properties you want from
    /// one — unversioned, unenumerable, and impossible to know who depends on
    /// which field.
    ///
    /// Implemented for all seven lanes as of stage-2 phase 4, through ONE body
    /// rather than seven — every lane's [`Receipt::message_id`] is minted into the
    /// same `Payload::SendInitiated.send_id` namespace, so "has a terminal arrived
    /// for this id" has one answer and one place to read it. See
    /// [`crate::lanes::LaneImpl::await_terminal`] for what IS per-lane about it,
    /// and for why `qd wait` did NOT fold in here.
    fn await_terminal(
        &self,
        id: &SessionId,
        message_id: &MessageId,
        budget_ms: u64,
    ) -> Result<Terminal, LaneError>;

    /// Resolve a send that has no terminal, by searching the recipient's own
    /// transcript for whether the message actually landed.
    ///
    /// **Why this is qw's and not qd's.** The search reads the agent's transcript
    /// and parses it per-harness — session-artifact access qd must not have. The
    /// ledger is two logs and qd never reads qw's, so the half of the dead-writer
    /// rule qd CAN evaluate (its own record's age, and whether its own writing
    /// incarnation is gone) stays in qd; only "is there a terminal, and if not
    /// what does the transcript say" crosses, as this call.
    ///
    /// qw emits any resulting late terminal into its own log; the return value
    /// tells qd what happened. A lane that cannot search returns
    /// [`Terminal::Undetermined`] rather than a negative — see that variant.
    ///
    /// Implemented for all seven lanes as of stage-2 phase 4, and routing the verb
    /// through it fixed a real defect: qd's `RealRecoveryDeps::resolve_transcript`
    /// resolved EVERY harness's transcript through claude's `~/.claude/projects`
    /// layout, so a codex rollout or a pi session was unresolvable and silently
    /// reported `SourceUnavailable` — "I looked and could not read it" — when
    /// nobody had looked anywhere it could be. Per-lane resolution is the fix; see
    /// [`crate::lanes::LaneRecoveryDeps`].
    ///
    /// **The caller side moves with the address widening.** This method used to
    /// be addressed by [`SessionId`], and that is why `verbs/recover.rs` went on
    /// calling `events::emit_recovery_verdict` directly: the sweep also carries
    /// sends keyed by `events::byname_key`, which no `SessionId` can name, so a
    /// `SessionId`-addressed method could serve only part of its own caller.
    /// [`LedgerAddress`] is that repair — see its docs for why it is the
    /// `(session?, name?)` pair rather than a union.
    ///
    /// What does NOT cross with it is the liveness fence. `recover` is called
    /// ONLY on a send its caller has already proved dead-dangling; clauses (b)
    /// and (c) of that rule read qd's own record and qd's own process, and stay
    /// in qd (`09-ledger-split.md`). Calling this on a live-writer send would
    /// append a premature terminal.
    fn recover(&self, at: &LedgerAddress, message_id: &MessageId) -> Result<Terminal, LaneError>;

    /// Block until this **session** goes idle, or the budget elapses.
    ///
    /// # It is not `await_terminal`, and the subject is why
    ///
    /// [`LaneOps::await_terminal`] answers "did THIS message reach a terminal".
    /// This answers "has this SESSION gone idle" — the question `qd wait`'s four
    /// per-provider bodies ask. Mapping one onto the other would claim a message
    /// was seen on evidence that never mentioned it, which is why ruling D2 grew a
    /// method instead of widening one. See [`TurnState`].
    ///
    /// # D8: the budget is the caller's, the ledger is the session's
    ///
    /// An idle-watcher whose budget runs out **must not stamp the session** or
    /// mint any foreclosing record: budget exhaustion is a fact about the
    /// observer, not about the turn. [`TurnState::BudgetElapsed`] therefore
    /// corresponds to nothing written, and this method takes no writer for it.
    ///
    /// The ACP and pi observers this method carries DO emit `message-seen`, and
    /// that is the distinction D8 turns on rather than an exception to it: they
    /// emit when they OBSERVE a send land in the recipient's transcript or
    /// rollout. That is evidence about the send, in qw's own log. What is
    /// forbidden is emitting *because the budget elapsed* — the bug
    /// [`LaneOps::await_terminal`] carried until 3A.
    ///
    /// # What the lane may print
    ///
    /// A lane that decides to WAIT opens the progress line (`Waiting for
    /// <label>...`) on stderr before it blocks, on the same footing as the pane
    /// carrier's `Waiting for response` — an answer that takes two minutes to
    /// arrive has to say so while it is arriving, and only the lane knows whether
    /// it is going to wait at all. The caller closes that line from the
    /// [`TurnState`]. Nothing here writes stdout: on the wire, stdout is the
    /// protocol.
    ///
    /// # `budget_ms == 0`
    ///
    /// Means "the caller passed no bound", not "do not wait" — `qd wait`'s
    /// `--timeout` parses to 0 when absent or unparseable and the verb has always
    /// meant its 120s default by it. Preserved rather than tidied: making 0 a
    /// zero-length wait would turn every `qd wait` with a malformed `--timeout`
    /// into an instant timeout.
    fn await_idle(&self, id: &SessionId, budget_ms: u64) -> Result<TurnState, LaneError>;

    /// Clause **(a)** of the dead-writer rule, and nothing else: does qw's
    /// delivery log already carry a terminal for this send? A pure read — it
    /// writes nothing, searches no transcript and takes no lock.
    ///
    /// # Why this is its own method rather than folded into `recover`
    ///
    /// It is the third clause of the rule, and the only one that needs the far
    /// side's log (`09-ledger-split.md`'s table). Before the ledger split
    /// `verbs/recover.rs` answered it locally, because the terminal sat in the
    /// same file as the `send-initiated` it was sweeping. It no longer does.
    ///
    /// Folding it into [`LaneOps::recover`] — letting the sweep call `recover` on
    /// every record and rely on its under-flock idempotence to adopt whatever
    /// terminal is already there — is safe for the LEDGER (no second terminal is
    /// ever written) and wrong for the ANSWER. `recover`'s adopt path reports an
    /// existing terminal through [`crate::events::RecoveryVerdict`], which has no
    /// "it was already resolved" state, so a send that was delivered and seen
    /// months ago comes back as `NotDelivered{recovery-no-candidate}` and the
    /// verb's summary reports it as abandoned. Every resolved pty send in the
    /// history of a state dir would be counted that way on every run, and each
    /// would pay a transcript read to get there. A sweep whose report is a lie
    /// about the sends it did not touch is worse than no sweep.
    ///
    /// So the poll comes first and `recover` is called only on what is genuinely
    /// unresolved. `09-ledger-split.md` anticipated exactly this shape — "with
    /// `budget_ms: 0` it is a poll, which answers clause (a)" — and the only
    /// reason it is not [`LaneOps::await_terminal`] itself is the address:
    /// `await_terminal` is [`SessionId`]-addressed, and half the sweep's
    /// population is `byname`-keyed, which is why [`recover`](LaneOps::recover)
    /// already takes a [`LedgerAddress`].
    ///
    /// `None` means "no terminal for this send", never "no such send" — an
    /// unknown message id is indistinguishable from an unresolved one from
    /// here, and both mean the same thing to the caller: there is nothing
    /// recorded that closes it.
    fn resolved(
        &self,
        at: &LedgerAddress,
        message_id: &MessageId,
    ) -> Result<Option<Terminal>, LaneError>;

    // --- interactive handoff ---------------------------------------------

    /// Attach a terminal to this session, returning the attached program's exit
    /// code. **The one method that is not a wire call.**
    ///
    /// An earlier design returned an `AttachPlan { argv, env, cwd }` for the
    /// caller to exec, on the theory that describing attach as DATA let it
    /// survive a process boundary without the controlling TTY crossing the
    /// interface. That was wrong in practice: the DEFAULT mux backend
    /// (`QD_MUX` unset → embedded) execs nothing at all. `EmbeddedMux::attach`
    /// calls `attach_session` in-process, puts the CALLING process's own stdio
    /// into raw mode, and pumps bytes to `<socket_dir>/<name>.sock`. There is no
    /// argv to return, so no plan could describe it.
    ///
    /// Note that `AttachPlan` passed the serializability guard cleanly — it was a
    /// plain data struct. A type can be perfectly serializable and still be unable
    /// to REPRESENT the behaviour it stands in for; the guard cannot catch that.
    ///
    /// So attach performs the handoff instead. The boundary mechanism is
    /// **process exec with inherited stdio**, not serialization: today the lane
    /// attaches directly, and after the binary split `qd attach` shells out to
    /// `qw`, whose stdio the terminal is already connected to. The signature stays
    /// plain data so it can still be *described* across the boundary — only the
    /// invocation differs.
    ///
    /// A lane with no terminal returns [`LaneError::NotSupported`].
    fn attach(&self, id: &SessionId) -> Result<i32, LaneError>;
}
