//! Lane identity: the `(harness, mode)` pair that answers "how do I drive THIS
//! session?" in one value.
//!
//! # Why this crate exists
//!
//! Today the answer is spread across two registry fields (`provider`, `hosting`)
//! and re-derived by hand at eleven call sites as the byte-identical expression
//! `row_hosting(&session.provider, session.hosting.as_deref())`, across six verb
//! files. `dispatch::provider` says why (`provider.rs:114-119`):
//!
//! > `Provider::hosting()` answers per PROVIDER, which was the whole truth
//! > **while each provider had exactly one topology**. codex now has two [...] so
//! > attach/kill/send/resume must key on the row, not the provider id.
//!
//! A [`Lane`] is that row-level answer, computed once.
//!
//! # Why a LEAF crate
//!
//! `dispatch` depends on `qrmux`, never the reverse. So
//! `qrmux::attended::driver::Harness` — which needs the same harness identity to
//! pick composer facts, clear chords and landing probes — cannot read the
//! registry, and re-derives it by parsing argv0 instead. On any parse it does not
//! recognise it silently downgrades to claude-shaped defaults. Both crates can
//! depend on THIS one, which is the same remedy already used by
//! `quorum-submit-discipline` and `quorum-delivery-events`.
//!
//! # Not the cartesian product
//!
//! [`Lane::ALL`] has NINE entries, not twenty. claude-code has no daemon lane;
//! the two ACP bridges have no pane lane (refused at the CLI — "an ACP bridge is
//! a protocol adapter with no terminal of its own at all"); [`Mode::Extension`]
//! is pi's alone and [`Mode::AppServer`] is codex's. Encoding that as data is
//! the point: it is currently scattered refusal code.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The agent program behind a session.
///
/// Distinct from a *provider id*: `acp/claude-code` and `acp/opencode` share one
/// transport and one `Provider` impl, but they are different harnesses driving
/// different agents, so they are different lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Harness {
    ClaudeCode,
    Codex,
    Pi,
    AcpClaudeCode,
    Opencode,
}

/// How a session is hosted. Mirrors `dispatch::provider::Hosting`, which this
/// crate cannot name (that would be a dependency cycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mode {
    /// A TUI in a mux pane a human can attach to.
    Pane,
    /// A headless resident driven over a protocol.
    Daemon,
    /// A TUI in a mux pane that ALSO carries a control channel: pi launched with
    /// the `quorum-lane` extension, which serves a unix socket that `qw`
    /// delivers over while a human types into the same composer.
    ///
    /// # Why this is a mode and not "pane with a flag"
    ///
    /// It is a pane in every respect but one: it has a real terminal, it lives
    /// in a mux pane, a human attaches to it, its revive relaunches the TUI and
    /// its kill reaps the pane. That is why [`Lane::is_pane`] answers `true` for
    /// it — every one of those paths must keep taking the pane branch.
    ///
    /// The single divergence is the CARRIER. `pi/mux-pane` delivers by typing
    /// keystrokes into the pane's PTY and infers acceptance from the transcript
    /// appearing on disk; this lane asks a socket inside pi's own process, and
    /// so reports acceptance, busy/idle and turn counts rather than deducing
    /// them. A mode is what makes that divergence a value the compiler can route
    /// on rather than a boolean somebody has to remember to check.
    ///
    /// # Why it is not a third harness
    ///
    /// Same binary, same store, same sessions, same `Provider` impl, same
    /// `--session-id` identity. What differs is topology, and topology is what
    /// `Mode` is.
    Extension,
    /// A headless resident that a human can ALSO open a terminal onto —
    /// codex's `codex app-server --listen ws://…`, which a `codex --remote
    /// <endpoint>` TUI can join as a SECOND client on the same thread.
    ///
    /// # Why this is a mode and not "daemon with a flag"
    ///
    /// It is a daemon in every respect but one: no terminal of its own, driven
    /// over RPC, receive path is an endpoint, kill is a pid-group reap. That is
    /// why [`Lane::is_daemon`] answers `true` for it — every one of those paths
    /// must keep taking the daemon branch. The single divergence is `attach`,
    /// and a mode is what makes that divergence a value the compiler can route
    /// on rather than a boolean somebody has to remember to check.
    ///
    /// # Why it is not a third harness
    ///
    /// Same binary, same store, same rollouts, same `Provider` impl. What
    /// differs is topology, and topology is what `Mode` is.
    AppServer,
}

impl Harness {
    /// Every harness, in a stable order.
    pub const ALL: [Harness; 5] = [
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::Pi,
        Harness::AcpClaudeCode,
        Harness::Opencode,
    ];

    /// The canonical provider id `qd start --provider <id>` resolves — the same
    /// string `dispatch::provider::provider_for` keys on. `Opencode` carries the
    /// INTERNAL id `acp/opencode`; the bare `opencode` CLI ergonomic is an alias
    /// handled by [`Harness::from_provider_id`].
    pub fn provider_id(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::AcpClaudeCode => "acp/claude-code",
            Harness::Opencode => "acp/opencode",
        }
    }

    /// Parse a provider id. Accepts the `opencode` CLI alias for `acp/opencode`
    /// (the same aliasing `provider_for` does). An unknown id is `None` — never a
    /// guess.
    pub fn from_provider_id(id: &str) -> Option<Harness> {
        match id {
            "claude-code" => Some(Harness::ClaudeCode),
            "codex" => Some(Harness::Codex),
            "pi" => Some(Harness::Pi),
            "acp/claude-code" => Some(Harness::AcpClaudeCode),
            "opencode" | "acp/opencode" => Some(Harness::Opencode),
            _ => None,
        }
    }

    // --- The three defaults -------------------------------------------------
    //
    // There used to be ONE function here — `default_mode()` — read from three
    // call sites that ask three genuinely different questions. They agree today,
    // and that agreement is a COINCIDENCE of history rather than a fact anyone
    // decided: every lane a `qd start` creates happens to be the lane an
    // unstamped row re-derives to, which happens to be the lane that owns the
    // harness's cold store.
    //
    // The coincidence is load-bearing in exactly one direction: only the CREATE
    // default may ever move. Changing the other two would silently relabel rows
    // and transcripts that are already on disk — a `codex/daemon` session
    // becoming a `codex/app-server` session because someone edited a default in
    // a file it never heard of. When the create default does move, one of these
    // three functions changes and the other two visibly do not, which is the
    // whole reason they are three.
    //
    // See `doc/tbd/provider-architecture/16-default-lane-switch.md` §0 (DEC-3)
    // and §3 for the rulings these encode.

    /// What `qd start --provider <id>` creates when no topology flag is given.
    ///
    /// **This is the only one of the three defaults that may ever move.** It is
    /// a statement about the FUTURE — the lane a session that does not exist yet
    /// will be born into — so changing it affects nothing already on disk.
    ///
    /// It is NOT [`Harness::row_default_mode`]: that answers for rows that
    /// already exist and were never stamped, and it is frozen. It is NOT
    /// [`Harness::cold_store_owner_mode`]: that answers which lane enumerates a
    /// harness's transcripts, and a transcript records no hosting at all.
    ///
    /// Read from [`Lane::for_create`] and nowhere else.
    pub fn create_default_mode(self) -> Mode {
        match self {
            // DEC-1: the claude default does NOT move. There is no second claude
            // lane to move it to — `Mode::Daemon` is unsupported by construction
            // — and "default to relay" describes what the pane lane already
            // does, since relay wins every send and `qd start` already gates on
            // relay readiness. See `16-default-lane-switch.md` §2d.
            Harness::ClaudeCode => Mode::Pane,
            // MOVED 2026-08-19 (workstream A, B7). Was `Mode::Daemon` for both.
            //
            // codex: `codex/app-server` is the SAME process as `codex/daemon`
            // (`start_codex_app_server` is `start_codex_daemon(req,
            // Some("app-server"))`) — identical spawn, delivery, kill and wake.
            // The stamp buys exactly one thing, and it is the thing a user
            // wants by default: `qd attach` can open a real terminal on the
            // resident.
            //
            // pi: `pi/extension` is a genuine topology change — a pi TUI in a
            // mux pane with a unix-socket control channel, instead of a headless
            // `<exe> pi-daemon`. It is what makes a pi session both drivable and
            // WATCHABLE. The headlessness it trades away is not deleted: it is
            // `--daemon` ([`CreateTopology::Daemon`], DEC-2), which is why that
            // variant had to exist before this line could change.
            Harness::Codex => Mode::AppServer,
            Harness::Pi => Mode::Extension,
            // The ACP bridges have exactly one lane each; there is nothing here
            // to move them to.
            Harness::AcpClaudeCode | Harness::Opencode => Mode::Daemon,
        }
    }

    /// The mode a registry row re-derives to when its `hosting` field is absent
    /// or unparseable — the structural answer `Provider::hosting()` gives.
    ///
    /// # This is FROZEN. Permanently. (DEC-3)
    ///
    /// `Pane` for claude-code, `Daemon` for everything else, forever. Not "until
    /// a migration lands" — there is no migration, and there will not be one:
    /// **ruling DEC-3 drops the backfill rather than deferring it.**
    ///
    /// The reason is that an unstamped row is not a row with a missing field; it
    /// is a row written by a version of this code that had exactly one lane per
    /// harness, and it MEANS that lane. Re-deriving it through anything else
    /// rewrites history. Concretely, if this tracked
    /// [`Harness::create_default_mode`] and that moved to `Mode::Extension` for
    /// pi, every pi row on disk would become `pi/extension` — a PANE lane — so
    /// `kill` and `attach` would take the pane branch on a session that has no
    /// pane and `deliver` would dial a control socket that was never bound.
    /// Nothing would error at the point of the change; every verb would fail
    /// later, in its own way, on sessions the user did not touch.
    ///
    /// And this is not only about rows written in the past. `claude-code` rows
    /// are STILL written unstamped: nothing in this repo creates them — the
    /// claude session writes its own row through a hook whose source lives
    /// elsewhere (`07-lane-gaps.md` gap C) — so `qd start --provider claude-code`
    /// stamps nothing, today, and this function is what places every claude
    /// session there is. It is a live derivation, not a compatibility shim.
    ///
    /// So: if you are here because this function and `create_default_mode`
    /// disagree and that looked like drift — it is not drift, it is the design.
    /// Do not resynchronise them. The divergence is the feature, and the drift
    /// test in `dispatch/src/lane/mod.rs`
    /// (`lane_for_agrees_with_row_hosting_except_where_it_is_deliberately_stricter`,
    /// divergence count 22) exists to keep THIS function agreeing with
    /// `provider::row_hosting`, which is the other reader of rows on disk.
    ///
    /// Read from [`lane_for`] and nowhere else.
    pub fn row_default_mode(self) -> Mode {
        match self {
            Harness::ClaudeCode => Mode::Pane,
            Harness::Codex | Harness::Pi | Harness::AcpClaudeCode | Harness::Opencode => {
                Mode::Daemon
            }
        }
    }

    /// Which of this harness's lanes enumerates its COLD store.
    ///
    /// A cold store is per-HARNESS, not per-lane: a transcript on disk records
    /// no hosting, so nothing in a codex rollout says whether the thread was
    /// driven from a mux pane or from a resident. The store therefore has to be
    /// assigned to exactly one lane and every sibling must enumerate zero — two
    /// claimants would DOUBLE-COUNT every cold row in `qd ls`.
    ///
    /// # Why this is frozen too, for a different reason than the row default
    ///
    /// [`Harness::row_default_mode`] is frozen because rows on disk mean the
    /// lane they were written under. This is frozen because a cold row is
    /// EMITTED with `hosting: None` and then read back through that same
    /// derivation — so the owning lane must be the lane a hosting-less row
    /// re-derives to, or `qd ls` would list a cold codex session under
    /// `codex/app-server` while every acting verb addressed it as
    /// `codex/daemon`. It tracks the ROW default, not the create default.
    ///
    /// Pinned by `lane_read::tests::exactly_one_lane_per_harness_owns_the_cold_store`
    /// against the `list_for` / `store_degradations` match arms, and by
    /// `session_merge_policy::each_cold_store_is_enumerated_by_exactly_one_lane`
    /// against the whole nine-lane partition.
    pub fn cold_store_owner_mode(self) -> Mode {
        match self {
            Harness::ClaudeCode => Mode::Pane,
            Harness::Codex | Harness::Pi | Harness::AcpClaudeCode | Harness::Opencode => {
                Mode::Daemon
            }
        }
    }

    /// Whether this harness can be hosted in `mode` at all.
    ///
    /// This is a STRUCTURAL fact, not an unimplemented feature: claude-code has
    /// no daemon lane, and an ACP bridge has no terminal to put in a pane.
    pub fn supports(self, mode: Mode) -> bool {
        // Written as the NEGATIVE set on purpose: the impossible combinations
        // are the interesting content, and listing them keeps this readable as
        // "what does not exist" rather than "what does".
        !matches!(
            (self, mode),
            (Harness::ClaudeCode, Mode::Daemon)
                | (Harness::AcpClaudeCode | Harness::Opencode, Mode::Pane)
                // `extension` is pi's alone, and structurally so: it names a
                // SPECIFIC affordance — pi's `--extension <path>` loader plus an
                // extension API with `sendUserMessage`, `isIdle` and `abort`
                // (verified against pi 0.84.1) — that no other harness in this
                // repo has. claude-code and codex expose no in-process
                // extension surface at all, and an ACP bridge has no TUI to put
                // one in. These four are "does not exist", never "not yet
                // built".
                | (
                    Harness::ClaudeCode
                        | Harness::Codex
                        | Harness::AcpClaudeCode
                        | Harness::Opencode,
                    Mode::Extension
                )
                // `app-server` is codex's alone, and structurally so: it names a
                // SPECIFIC residence (`codex app-server --listen ws://…`) that a
                // human TUI can be pointed at (`codex --remote <endpoint>`). No
                // other harness in this repo has either half — pi's residence is
                // `<exe> pi-daemon` and an ACP bridge has no TUI at all — so
                // these four are "does not exist", never "not yet built".
                | (
                    Harness::ClaudeCode
                        | Harness::Pi
                        | Harness::AcpClaudeCode
                        | Harness::Opencode,
                    Mode::AppServer
                )
        )
    }
}

impl Mode {
    /// The registry `hosting` token this mode is written as.
    pub fn hosting_token(self) -> &'static str {
        match self {
            Mode::Pane => "mux-pane",
            Mode::Daemon => "daemon",
            Mode::Extension => "extension",
            Mode::AppServer => "app-server",
        }
    }

    /// Permissive parse of a row's `hosting` token, matching
    /// `dispatch::provider::parse_hosting`: an unknown/garbage string is `None`
    /// so the caller falls back to the harness default rather than inventing a
    /// topology.
    pub fn from_hosting_token(s: &str) -> Option<Mode> {
        match s {
            "mux-pane" => Some(Mode::Pane),
            "daemon" => Some(Mode::Daemon),
            "extension" => Some(Mode::Extension),
            "app-server" => Some(Mode::AppServer),
            _ => None,
        }
    }
}

/// One lane: a harness plus how it is hosted. THE dispatch key.
///
/// Serializes as its stable string id (`"codex/daemon"`), never as a struct, so
/// the wire shape survives adding a harness or reordering variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Lane {
    pub harness: Harness,
    pub mode: Mode,
}

impl Lane {
    /// Every VALID lane — nine, not the twenty of the cartesian product. See
    /// the module docs for why the seven missing combinations are structural.
    pub const ALL: [Lane; 9] = [
        Lane {
            harness: Harness::ClaudeCode,
            mode: Mode::Pane,
        },
        Lane {
            harness: Harness::Codex,
            mode: Mode::Pane,
        },
        Lane {
            harness: Harness::Codex,
            mode: Mode::Daemon,
        },
        Lane {
            harness: Harness::Codex,
            mode: Mode::AppServer,
        },
        Lane {
            harness: Harness::Pi,
            mode: Mode::Pane,
        },
        Lane {
            harness: Harness::Pi,
            mode: Mode::Daemon,
        },
        Lane {
            harness: Harness::Pi,
            mode: Mode::Extension,
        },
        Lane {
            harness: Harness::AcpClaudeCode,
            mode: Mode::Daemon,
        },
        Lane {
            harness: Harness::Opencode,
            mode: Mode::Daemon,
        },
    ];

    /// Construct a lane, refusing a structurally impossible combination.
    pub fn new(harness: Harness, mode: Mode) -> Option<Lane> {
        harness.supports(mode).then_some(Lane { harness, mode })
    }

    /// THE create question, asked once: which lane does
    /// `qd start --provider <id> [--interactive]` create?
    ///
    /// This is the entire content of what `lifecycle::run_new` computed with a
    /// five-arm ordered `if`-chain whose ordering was enforced **only by a
    /// comment**, and whose arms are each recoverable from the three lines below:
    ///
    ///   - [`Harness::create_default_mode`] gives the harness's default lane —
    ///     the chain's arms 3, 5 and its fall-through. (It gave `Pane` for
    ///     claude and `Daemon` for everything else when the chain was removed;
    ///     codex and pi have since moved, which is precisely the edit this
    ///     function exists to make one-line and visible.);
    ///   - an explicit `--interactive` is `Mode::Pane` — arms 2 and 4;
    ///   - [`Lane::new`] answers `None` for exactly the three impossible
    ///     combinations, which IS the `--interactive` refusal for `acp/*`. It is
    ///     UNREPRESENTABLE here rather than checked.
    ///
    /// **Why it was worth removing rather than documenting.** Two of the five
    /// swaps were silent: exchange the two pi arms, or the two codex arms, and
    /// `--interactive` is ignored — the caller asks for an attachable pane and
    /// gets a headless resident, exit 0. Nothing errors, so nothing tells anyone.
    /// A function with a total table over its real inputs cannot hold that bug.
    ///
    /// The *wording* and *exit code* of the unknown-provider and acp-interactive
    /// refusals stay on qd's side, where the user-facing text belongs. What is
    /// here is the SET: `None` means "no such lane", and qd chooses how to say it.
    ///
    /// **Why the second parameter is an enum and not a `bool`.** It used to be
    /// `interactive: bool`, which was total while a create had exactly two
    /// outcomes. `pi/extension` makes three, and the obvious patch —
    /// `for_create(id, interactive, extension)` — reintroduces the precise bug
    /// shape the doc above says this function exists to make unrepresentable:
    /// two adjacent silently-swappable bools, where exchanging them is accepted
    /// by the compiler, ignored at runtime, and reported as exit 0. A
    /// [`CreateTopology`] cannot be passed in the wrong order because there is
    /// only one of it.
    ///
    /// **This table is now TOTAL over the nine lanes.** [`Mode::AppServer`] used
    /// to be the one lane no input here could produce — reachable only by naming
    /// it on `qw`'s wire (`{"m":"start","lane":"codex/app-server", …}`) — and the
    /// note that stood in this paragraph said wiring it up would cost "one more
    /// [`CreateTopology`] variant plus the clap flag". It cost two variants: the
    /// app-server one, and an explicit [`CreateTopology::Daemon`], because moving
    /// the codex and pi defaults off `Mode::Daemon`
    /// (`16-default-lane-switch.md` DEC-2/DEC-4) takes `codex/daemon` and
    /// `pi/daemon` OFF the CLI unless something puts them back. `--daemon` is
    /// that something, and the two changes had to land together or the flip would
    /// have deleted two working lanes.
    pub fn for_create(provider_id: &str, topology: CreateTopology) -> Option<Lane> {
        let harness = Harness::from_provider_id(provider_id)?;
        let mode = match topology {
            CreateTopology::Default => harness.create_default_mode(),
            CreateTopology::Interactive => Mode::Pane,
            CreateTopology::Extension => Mode::Extension,
            CreateTopology::AppServer => Mode::AppServer,
            CreateTopology::Daemon => Mode::Daemon,
        };
        Lane::new(harness, mode)
    }

    /// The stable wire id: `<provider-id>/<hosting-token>`, e.g.
    /// `"acp/claude-code/daemon"`, `"codex/mux-pane"`.
    pub fn id(self) -> String {
        format!(
            "{}/{}",
            self.harness.provider_id(),
            self.mode.hosting_token()
        )
    }

    /// Parse a stable wire id. Splits on the LAST `/` because a provider id may
    /// itself contain one (`acp/claude-code`).
    pub fn from_id(s: &str) -> Option<Lane> {
        let (provider, hosting) = s.rsplit_once('/')?;
        let harness = Harness::from_provider_id(provider)?;
        let mode = Mode::from_hosting_token(hosting)?;
        Lane::new(harness, mode)
    }

    /// Does this lane have a terminal of its own, in a mux pane?
    ///
    /// TRUE for [`Mode::Extension`]. See its docs for why: the extension lane is
    /// a pane in every respect — real TUI, real pane, attachable, revived by
    /// relaunching, killed by reaping the pane — and diverges only in what
    /// carries a `deliver`. If this answered `false`, every one of those paths
    /// would silently take the daemon branch and the lane would be wrong
    /// everywhere at once.
    pub fn is_pane(self) -> bool {
        matches!(self.mode, Mode::Pane | Mode::Extension)
    }

    /// Is this a headless resident?
    ///
    /// **`Mode::AppServer` answers `true`**, and that is the load-bearing part.
    /// It is a daemon in every respect except `attach`: no terminal of its own,
    /// driven over RPC, receive path is an endpoint, kill is a pid-group reap.
    /// If it answered `false` here, every one of those paths would silently take
    /// the PANE branch and the lane would be wrong everywhere at once — so the
    /// attach exception is spelled at `attach` (keyed on [`Lane::mode`]), where
    /// it is one visible arm, rather than hidden in this predicate.
    pub fn is_daemon(self) -> bool {
        matches!(self.mode, Mode::Daemon | Mode::AppServer)
    }

    /// Is this lane's residence the `codex app-server` specifically?
    ///
    /// Spelled as a predicate rather than compared inline so a call site reads as
    /// a named property instead of a bare enum test. Ask [`Lane::has_viewer`]
    /// instead for the "can a human get a terminal on this" question — that is
    /// no longer the same question, and conflating them is what this pair exists
    /// to prevent.
    pub fn is_app_server(self) -> bool {
        self.mode == Mode::AppServer
    }

    /// Can a human be given a terminal on this DAEMON lane's session — not by
    /// giving the session a terminal it does not have, but by opening a second
    /// CLIENT on the server its residence already is?
    ///
    /// # Why this is a lane property and not a mode test
    ///
    /// It used to be [`Lane::is_app_server`], and reading it that way was right
    /// while codex was the only harness whose residence a second client could
    /// join. It is now wrong in a specific and checkable way: `opencode acp` is
    /// not a stdio bridge — it runs a full opencode HTTP server in-process, qd
    /// pins its port at spawn, and `opencode attach <url> --session <id>` is a
    /// documented second client of exactly that server. So `acp/opencode` has the
    /// property while being nothing like an app-server lane in any other respect.
    ///
    /// Keeping the mode test would have forced the choice between two worse
    /// things: giving opencode `Mode::AppServer` (a lie about its topology that
    /// `Harness::supports` correctly refuses — app-server names a codex-specific
    /// residence *and* a `codex --remote` TUI), or a second attach path bolted on
    /// beside the first. Naming the actual property costs one predicate.
    ///
    /// Answering `true` here does NOT make a lane attachable in the ordinary
    /// sense: [`Lane::is_daemon`] still answers `true`, there is still no
    /// terminal to hand over, and everything else — kill, deliver, receive path,
    /// health — still takes the daemon branch. The exception is spelled at
    /// `attach` and nowhere else.
    pub fn has_viewer(self) -> bool {
        matches!(
            (self.harness, self.mode),
            // codex's app server, joined by `codex --remote <ws> resume <id>`.
            (Harness::Codex, Mode::AppServer)
            // opencode's HTTP server inside the ACP bridge, joined by
            // `opencode attach <http> --session <id>`.
            | (Harness::Opencode, Mode::Daemon)
        )
    }

    /// Does this lane deliver over the `quorum-lane` extension's control socket
    /// rather than by typing into a PTY?
    ///
    /// The one question [`Lane::is_pane`] deliberately does not answer. Kept as
    /// a named predicate because "which carrier" is the entire content of this
    /// mode, and a bare `== Mode::Extension` at a call site says less.
    pub fn has_control_channel(self) -> bool {
        self.mode == Mode::Extension
    }
}

/// What topology a create is asking for — the input to [`Lane::for_create`].
///
/// Exists so that the outcomes of `qd start` are ONE value rather than a row of
/// booleans; see [`Lane::for_create`] for why that distinction earned its own
/// type. There are five of them now and the argument only gets stronger with
/// each one: five bools would be thirty-two call sites' worth of orderings, of
/// which twenty-seven are nonsense the compiler would accept.
///
/// Every variant except [`CreateTopology::Default`] names a topology the caller
/// asked for BY NAME, so a harness that has no such lane is a refusal
/// ([`Lane::new`] answering `None`), never a silent downgrade to the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CreateTopology {
    /// No topology flag: the harness's structural default,
    /// [`Harness::create_default_mode`]. The only variant whose answer moves
    /// when a default is flipped.
    Default,
    /// `--interactive`: a TUI in a mux pane.
    Interactive,
    /// `--extension`: a TUI in a mux pane carrying the `quorum-lane` control
    /// socket. pi only — [`Lane::new`] answers `None` for every other harness.
    Extension,
    /// The `codex app-server` residence, joinable by a second `codex --remote`
    /// TUI. codex only — [`Lane::new`] answers `None` for every other harness.
    ///
    /// **Has no `qd start` flag**, and does not need one: it is what
    /// [`CreateTopology::Default`] resolves to for codex, so the plain
    /// `qd start --provider codex` reaches it. This variant is the way a caller
    /// names the lane EXPLICITLY — `qw`'s wire
    /// (`{"m":"start","lane":"codex/app-server"}`) and any future flag — and it
    /// exists separately from `Default` so that pinning "the default is
    /// app-server" and "app-server is requestable" stay two different
    /// assertions.
    AppServer,
    /// `--daemon`: the headless resident, explicitly. No mux pane, no TTY, no
    /// terminal to attach — the CI / over-ssh / no-mux escape hatch.
    ///
    /// # Why an explicit variant rather than "just don't pass a flag"
    ///
    /// Until the defaults moved, "no flag" and "daemon" were the same request
    /// for codex and pi, so `--daemon` would have been a no-op. They are not the
    /// same request any more (`16-default-lane-switch.md` DEC-2/DEC-4): the
    /// default is `codex/app-server` / `pi/extension`, and this variant is the
    /// ONLY way `qd start` reaches `codex/daemon` or `pi/daemon`. Folding it
    /// into `Default` would delete two lanes from the CLI.
    ///
    /// For `acp/*` it is a no-op that names the truth — those harnesses have one
    /// lane and it is this one. For `claude-code` it is a REFUSAL, because
    /// `Harness::ClaudeCode.supports(Mode::Daemon)` is `false`: there is no
    /// headless claude to host, so `Lane::new` answers `None` and the caller
    /// renders that rather than quietly handing back the pane lane.
    Daemon,
}

/// THE lane question, asked once: how is this row hosted?
///
/// Replaces the eleven duplicated
/// `row_hosting(&session.provider, session.hosting.as_deref())` expressions.
/// Resolution order is byte-identical to `dispatch::provider::row_hosting`:
///
///   1. the row's recorded `hosting` token, when present AND parseable;
///   2. else the harness's structural default;
///   3. else (unknown provider id) `None`, so the caller keeps its own
///      unknown-provider refusal rather than being handed a made-up topology.
///
/// A row whose recorded token names a combination the harness cannot support
/// (a corrupt row claiming a pane-hosted ACP bridge) falls back to the harness
/// default rather than returning a lane that cannot exist.
pub fn lane_for(provider_id: &str, hosting_field: Option<&str>) -> Option<Lane> {
    let harness = Harness::from_provider_id(provider_id)?;
    let recorded = hosting_field
        .and_then(Mode::from_hosting_token)
        .filter(|&m| harness.supports(m));
    Some(Lane {
        harness,
        mode: recorded.unwrap_or_else(|| harness.row_default_mode()),
    })
}

impl fmt::Display for Lane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id())
    }
}

impl From<Lane> for String {
    fn from(l: Lane) -> String {
        l.id()
    }
}

impl TryFrom<String> for Lane {
    type Error = UnknownLane;
    fn try_from(s: String) -> Result<Lane, UnknownLane> {
        Lane::from_id(&s).ok_or(UnknownLane(s))
    }
}

/// A lane id that does not name a valid lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownLane(pub String);

impl fmt::Display for UnknownLane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown lane id: {:?}", self.0)
    }
}

impl std::error::Error for UnknownLane {}

// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lanes_are_valid_and_unique() {
        let mut ids: Vec<String> = Lane::ALL.iter().map(|l| l.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), Lane::ALL.len(), "lane ids must be unique");
        for lane in Lane::ALL {
            assert!(
                lane.harness.supports(lane.mode),
                "{lane} is in ALL but its harness does not support that mode"
            );
        }
    }

    #[test]
    fn all_is_exactly_the_supported_combinations() {
        // ALL must be the supported subset of the cartesian product — not a
        // hand-maintained list that can drift from `supports`.
        let mut expected: Vec<Lane> = Vec::new();
        for h in Harness::ALL {
            for m in [Mode::Pane, Mode::Daemon, Mode::Extension, Mode::AppServer] {
                if let Some(l) = Lane::new(h, m) {
                    expected.push(l);
                }
            }
        }
        let mut actual = Lane::ALL.to_vec();
        expected.sort();
        actual.sort();
        assert_eq!(actual, expected);
        assert_eq!(
            actual.len(),
            9,
            "nine lanes, not the twenty of the product"
        );
    }

    #[test]
    fn the_three_missing_combinations_are_structural() {
        assert!(
            !Harness::ClaudeCode.supports(Mode::Daemon),
            "claude has no daemon lane"
        );
        assert!(
            !Harness::AcpClaudeCode.supports(Mode::Pane),
            "an ACP bridge has no terminal"
        );
        assert!(
            !Harness::Opencode.supports(Mode::Pane),
            "an ACP bridge has no terminal"
        );
        assert_eq!(Lane::new(Harness::ClaudeCode, Mode::Daemon), None);
        // `app-server` is codex's alone.
        for h in [
            Harness::ClaudeCode,
            Harness::Pi,
            Harness::AcpClaudeCode,
            Harness::Opencode,
        ] {
            assert!(
                !h.supports(Mode::AppServer),
                "{h:?} must not claim an app-server residence"
            );
        }
        assert!(Harness::Codex.supports(Mode::AppServer));
    }

    #[test]
    fn ids_round_trip_including_the_slash_carrying_provider() {
        for lane in Lane::ALL {
            assert_eq!(
                Lane::from_id(&lane.id()),
                Some(lane),
                "{lane} must round-trip"
            );
        }
        // The acp ids contain a `/` of their own — the parse must split on the LAST one.
        let acp = Lane {
            harness: Harness::AcpClaudeCode,
            mode: Mode::Daemon,
        };
        assert_eq!(acp.id(), "acp/claude-code/daemon");
        assert_eq!(Lane::from_id("acp/claude-code/daemon"), Some(acp));
    }

    #[test]
    fn unknown_ids_are_none_not_a_guess() {
        assert_eq!(Lane::from_id("nope/daemon"), None);
        assert_eq!(Lane::from_id("codex/sideways"), None);
        // The app-server lane parses; nobody else's app-server does.
        assert_eq!(
            Lane::from_id("codex/app-server"),
            Lane::new(Harness::Codex, Mode::AppServer)
        );
        assert_eq!(Lane::from_id("pi/app-server"), None);
        assert_eq!(Lane::from_id("codex"), None);
        assert_eq!(Lane::from_id(""), None);
        // Structurally impossible, even though both halves parse.
        assert_eq!(Lane::from_id("claude-code/daemon"), None);
    }

    // --- for_create: the routing table that replaced the if-chain ----------

    /// **The test that replaces the comment.**
    ///
    /// A TOTAL table over every real `(--provider, topology)` a user can type,
    /// including the bare `opencode` CLI alias and the absent-flag default, plus
    /// `None` for the combinations that cannot exist. Pure: no process, no mux,
    /// no registry.
    ///
    /// The silent mis-swaps this pins are the codex block and the pi block — in
    /// both, exchanging two rows means the topology flag is ignored and the
    /// caller gets a lane it did not ask for with exit 0. Since the defaults
    /// moved (`16-default-lane-switch.md` A2/B7) those blocks are four rows each
    /// rather than two, and the `Default` row of each no longer duplicates the
    /// `Daemon` row — which is exactly why `--daemon` had to become a real
    /// topology instead of a synonym for "no flag".
    #[test]
    fn start_routing_is_total_over_every_real_input() {
        use CreateTopology::{
            AppServer as App, Daemon as Dae, Default as Def, Extension as Ext, Interactive as Inter,
        };
        // (provider id as typed, the topology asked for, the lane created)
        let table: [(&str, CreateTopology, Option<&str>); 35] = [
            // claude has one lane; --interactive is its only shape, so the flag
            // changes nothing rather than being refused. --daemon IS refused:
            // there is no headless claude to host, so this is a lane that does
            // not exist rather than a flag qd declines to honour.
            ("claude-code", Def, Some("claude-code/mux-pane")),
            ("claude-code", Inter, Some("claude-code/mux-pane")),
            ("claude-code", Ext, None),
            ("claude-code", Dae, None),
            ("claude-code", App, None),
            // The codex block. Its Default row and its Daemon row now name
            // DIFFERENT lanes, and both must stay reachable (DEC-4): the app
            // server is the default because it is the same process plus an
            // attachable terminal, and `codex/daemon` survives behind --daemon.
            ("codex", Def, Some("codex/app-server")),
            ("codex", Inter, Some("codex/mux-pane")),
            ("codex", Dae, Some("codex/daemon")),
            ("codex", App, Some("codex/app-server")),
            ("codex", Ext, None),
            // The pi block, the same shape. `--extension` survives as an
            // explicit flag even though it now names the default — existing
            // scripts pass it, and it costs nothing.
            ("pi", Def, Some("pi/extension")),
            ("pi", Inter, Some("pi/mux-pane")),
            ("pi", Ext, Some("pi/extension")),
            ("pi", Dae, Some("pi/daemon")),
            ("pi", App, None),
            // An ACP bridge is a protocol adapter with no terminal of its own, so
            // the interactive row is not a refusal to implement — it is a lane
            // that does not exist. --daemon is a no-op that names the truth: the
            // daemon lane is the only lane it has.
            ("acp/claude-code", Def, Some("acp/claude-code/daemon")),
            ("acp/claude-code", Inter, None),
            ("acp/claude-code", Dae, Some("acp/claude-code/daemon")),
            ("acp/claude-code", Ext, None),
            ("acp/claude-code", App, None),
            ("acp/opencode", Def, Some("acp/opencode/daemon")),
            ("acp/opencode", Inter, None),
            ("acp/opencode", Dae, Some("acp/opencode/daemon")),
            ("acp/opencode", Ext, None),
            ("acp/opencode", App, None),
            // The CLI ergonomic alias resolves to the SAME lane as the internal
            // id, under every topology — including the new ones, so the alias
            // cannot acquire a routing of its own.
            ("opencode", Def, Some("acp/opencode/daemon")),
            ("opencode", Inter, None),
            ("opencode", Dae, Some("acp/opencode/daemon")),
            ("opencode", Ext, None),
            ("opencode", App, None),
            // Fail-closed on garbage, in every mode — never a claude
            // fall-through, which is what the pre-refusal engine did.
            ("nope", Def, None),
            ("nope", Inter, None),
            ("nope", Ext, None),
            ("nope", Dae, None),
            ("nope", App, None),
        ];
        for (provider, topology, expected) in table {
            assert_eq!(
                Lane::for_create(provider, topology).map(|l| l.id()).as_deref(),
                expected,
                "start routing for --provider {provider} with {topology:?}"
            );
        }

        // TOTAL, and asserted as such: every lane `qd start` can make is reachable
        // from some real input. A lane that no input creates would be a lane
        // `qd start` cannot make, and the table above would still pass.
        //
        // THE EXCLUSION IS GONE. This assertion used to carry one — `qd start`
        // had no flag for `codex/app-server`, so it was subtracted from `ALL`
        // here by name. It is now the codex DEFAULT, and the lane the exclusion
        // was protecting (`codex/daemon`) is reachable through `--daemon`. All
        // nine, no carve-outs: if a tenth lane is ever added without a way to
        // create it, this is what says so.
        let reachable: Vec<String> = {
            let mut v: Vec<String> = table
                .iter()
                .filter_map(|(_, _, e)| e.map(|s| s.to_string()))
                .collect();
            v.sort();
            v.dedup();
            v
        };
        let mut all: Vec<String> = Lane::ALL.iter().map(|l| l.id()).collect();
        all.sort();
        assert_eq!(reachable, all, "every lane must be reachable from `qd start`");

        // The two lanes the default flip took off the no-flag path. If `--daemon`
        // ever stops routing, these do not become "the wrong default" — they
        // become UNREACHABLE, and a headless CI `qd start --provider pi` has no
        // spelling at all.
        for (provider, expected) in [("codex", "codex/daemon"), ("pi", "pi/daemon")] {
            assert_eq!(
                Lane::for_create(provider, CreateTopology::Daemon)
                    .map(|l| l.id())
                    .as_deref(),
                Some(expected),
                "--daemon is the ONLY route to {expected} now that the default moved"
            );
        }
    }

    /// **The create defaults, pinned on their own — the test that is SUPPOSED to
    /// change.**
    ///
    /// Every provider id a user can type, with no topology flag, and the lane
    /// `qd start` makes for it. The table above already covers these rows as part
    /// of a total routing check; this one exists separately because it is the
    /// only place where a changed EXPECTATION is the correct outcome rather than
    /// a regression. When `codex` is flipped to `codex/app-server` or `pi` to
    /// `pi/extension` (`16-default-lane-switch.md` workstreams A and B), the
    /// change is one line in [`Harness::create_default_mode`] and the failing
    /// rows are right here, named, in a test whose whole subject is defaults.
    ///
    /// Its counterpart is `lane_for_falls_back_to_the_structural_default`, which
    /// pins the SAME shape for rows on disk and must NOT change when this one
    /// does. The two tests are the split made visible: a create default is a
    /// statement about sessions that do not exist yet, and a row default is a
    /// statement about sessions that already do.
    ///
    /// **It changed, on 2026-08-19, and this is the record.** codex moved to
    /// `codex/app-server` and pi to `pi/extension`. claude-code did NOT move
    /// (DEC-1) and neither ACP bridge did — each has exactly one lane. Every row
    /// below that still reads `daemon` is a harness whose default was never in
    /// question. If you are reading this because a row went red: check WHICH
    /// default moved. This function is `create_default_mode`, and it is the only
    /// one of the three that may. If `the_frozen_defaults_are_frozen_…` went red
    /// alongside it, the wrong one moved.
    #[test]
    fn the_create_default_lane_is_pinned_for_every_provider_id() {
        // (provider id as typed, the lane a flagless `qd start` creates)
        let defaults: [(&str, &str); 6] = [
            // DEC-1: unmoved, and there is nowhere for it to move to.
            ("claude-code", "claude-code/mux-pane"),
            // Moved (A2). Same process as `codex/daemon`, plus an attach.
            ("codex", "codex/app-server"),
            // Moved (B7). A pi TUI in a pane with a control channel, in place of
            // a headless resident; `--daemon` is the way back.
            ("pi", "pi/extension"),
            ("acp/claude-code", "acp/claude-code/daemon"),
            ("acp/opencode", "acp/opencode/daemon"),
            // The CLI ergonomic alias, which must default to the same lane as
            // the internal id rather than acquiring a default of its own.
            ("opencode", "acp/opencode/daemon"),
        ];
        for (provider, expected) in defaults {
            assert_eq!(
                Lane::for_create(provider, CreateTopology::Default)
                    .map(|l| l.id())
                    .as_deref(),
                Some(expected),
                "a flagless `qd start --provider {provider}` must create {expected}"
            );
        }

        // Every harness has a create default, and it is a lane that exists. Held
        // as a property rather than a sixth row so that adding a harness cannot
        // ship a default pointing at a combination `Lane::new` refuses — which
        // would make `qd start` answer "no such lane" for a provider it accepts.
        for h in Harness::ALL {
            assert!(
                Lane::new(h, h.create_default_mode()).is_some(),
                "{h:?}: its create default names a lane that cannot be built"
            );
        }
    }

    /// **The three defaults are three questions, and only one of them may move.**
    ///
    /// They answer identically today, which is exactly the coincidence that made
    /// a single `default_mode()` look correct for as long as it did. This test
    /// does not assert that they agree — asserting agreement would re-tie the
    /// knot the split undid, and would go red for the RIGHT change. It asserts
    /// the two frozen ones against literals, so that a flip of the create default
    /// leaves them untouched and provably so.
    ///
    /// `row_default_mode` is frozen by DEC-3: an unstamped row on disk means the
    /// lane it was written under, and no backfill is coming.
    /// `cold_store_owner_mode` is frozen because it must agree with the row
    /// default — a cold row is emitted with no hosting and read back through
    /// exactly that derivation, so an owner that disagreed would list a session
    /// under one lane while every acting verb addressed it as another.
    #[test]
    fn the_frozen_defaults_are_frozen_at_pane_for_claude_and_daemon_for_everything_else() {
        for h in Harness::ALL {
            let expected = match h {
                Harness::ClaudeCode => Mode::Pane,
                Harness::Codex | Harness::Pi | Harness::AcpClaudeCode | Harness::Opencode => {
                    Mode::Daemon
                }
            };
            assert_eq!(
                h.row_default_mode(),
                expected,
                "{h:?}: row derivation is FROZEN (DEC-3). If this went red because the \
                 create default moved and someone resynchronised the two, that is the \
                 exact change the split exists to prevent: it silently relabels every \
                 unstamped row already on disk."
            );
            assert_eq!(
                h.cold_store_owner_mode(),
                h.row_default_mode(),
                "{h:?}: the cold-store owner must track the ROW default, because a cold \
                 row is emitted with no hosting and re-read through that derivation"
            );
        }
    }

    /// The absent `--provider` default is claude-code, and it is the same
    /// resolution — not a separate path. (qd substitutes the default before
    /// asking; this pins that the substituted value routes.)
    #[test]
    fn the_default_provider_creates_the_claude_pane_lane() {
        assert_eq!(
            Lane::for_create("claude-code", CreateTopology::Default),
            Lane::new(Harness::ClaudeCode, Mode::Pane)
        );
    }

    // --- lane_for: the row_hosting semantics, pinned -----------------------

    #[test]
    fn lane_for_prefers_the_rows_recorded_token() {
        // The codex-interactive case row_hosting exists to serve.
        assert_eq!(
            lane_for("codex", Some("mux-pane")),
            Lane::new(Harness::Codex, Mode::Pane)
        );
        assert_eq!(
            lane_for("pi", Some("mux-pane")),
            Lane::new(Harness::Pi, Mode::Pane)
        );
    }

    #[test]
    fn lane_for_falls_back_to_the_structural_default() {
        // Absent field → the provider's structural answer, so every pre-existing
        // row keeps answering exactly as it did before this crate existed.
        assert_eq!(
            lane_for("codex", None),
            Lane::new(Harness::Codex, Mode::Daemon)
        );
        assert_eq!(lane_for("pi", None), Lane::new(Harness::Pi, Mode::Daemon));
        assert_eq!(
            lane_for("claude-code", None),
            Lane::new(Harness::ClaudeCode, Mode::Pane)
        );
        assert_eq!(
            lane_for("acp/claude-code", None),
            Lane::new(Harness::AcpClaudeCode, Mode::Daemon)
        );
    }

    #[test]
    fn lane_for_degrades_a_garbage_token_to_the_default() {
        assert_eq!(
            lane_for("codex", Some("")),
            Lane::new(Harness::Codex, Mode::Daemon)
        );
        assert_eq!(
            lane_for("codex", Some("sideways")),
            Lane::new(Harness::Codex, Mode::Daemon)
        );
    }

    #[test]
    fn lane_for_refuses_an_impossible_recorded_token() {
        // A corrupt row claiming a pane-hosted ACP bridge must NOT yield a lane
        // that cannot exist — it degrades to the harness default.
        assert_eq!(
            lane_for("acp/opencode", Some("mux-pane")),
            Lane::new(Harness::Opencode, Mode::Daemon)
        );
    }

    #[test]
    fn lane_for_an_unknown_provider_is_none() {
        assert_eq!(lane_for("nope", None), None);
        assert_eq!(lane_for("nope", Some("daemon")), None);
        assert_eq!(lane_for("", None), None);
    }

    #[test]
    fn opencode_cli_alias_resolves_to_the_internal_id() {
        assert_eq!(
            Harness::from_provider_id("opencode"),
            Some(Harness::Opencode)
        );
        assert_eq!(
            Harness::from_provider_id("acp/opencode"),
            Some(Harness::Opencode)
        );
        // ...and always SERIALIZES as the internal id.
        assert_eq!(Harness::Opencode.provider_id(), "acp/opencode");
        assert_eq!(
            lane_for("opencode", None).map(|l| l.id()).as_deref(),
            Some("acp/opencode/daemon")
        );
    }

    #[test]
    fn lane_serializes_as_its_stable_string_id() {
        let lane = Lane {
            harness: Harness::Codex,
            mode: Mode::Daemon,
        };
        let json = serde_json::to_string(&lane).unwrap();
        assert_eq!(
            json, r#""codex/daemon""#,
            "lane must be a string on the wire"
        );
        let back: Lane = serde_json::from_str(&json).unwrap();
        assert_eq!(back, lane);
    }

    #[test]
    fn every_lane_survives_a_serde_round_trip() {
        for lane in Lane::ALL {
            let json = serde_json::to_string(&lane).unwrap();
            let back: Lane = serde_json::from_str(&json).unwrap();
            assert_eq!(back, lane, "{lane} must survive serde");
        }
    }

    #[test]
    fn deserializing_an_impossible_lane_errors_rather_than_guessing() {
        let err = serde_json::from_str::<Lane>(r#""claude-code/daemon""#);
        assert!(err.is_err(), "an impossible lane must not deserialize");
    }
}
