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
/// FOUR of them, and the fourth used to be five. `AcpClaudeCode` was a harness
/// here, with provider id `acp/claude-code`, and it was never an agent program:
/// it named a TRANSPORT in front of the claude-code program that
/// [`Harness::ClaudeCode`] already names. The bridge runs the real claude
/// engine, writes claude-shaped JSONL into claude's own store, and shares
/// claude's session-id space so completely that `join.rs` had to widen its dedup
/// key to keep an ACP row and a plain row with ONE sessionId apart. That is one
/// harness in two topologies, which is what a [`Mode`] is for — so ACP is
/// [`Mode::Acp`] now, and the lanes are `claude-code/acp` and `opencode/acp`.
///
/// The count is what makes the case: nine lanes before, nine lanes after. No
/// lane was created or destroyed, only re-coordinated onto the axis it belonged
/// on. `setup::HarnessId` had already been enumerating four for as long as it has
/// existed, and `lifecycle::harness_for_detected` was the adapter between the two
/// views — its own doc comment admitting that two of its four arms were traps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Harness {
    ClaudeCode,
    Codex,
    Pi,
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
    /// A headless resident reached over the **Agent Client Protocol** — a bridge
    /// process (`claude-code-acp`, `opencode acp`) that qd speaks ACP to and that
    /// drives the real agent behind it.
    ///
    /// # Why this is a mode and not two harnesses
    ///
    /// It was two harnesses — `acp/claude-code` and `acp/opencode` — and that was
    /// a category error the rest of the codebase kept having to work around. ACP
    /// is a TRANSPORT. Behind `claude-code/acp` is the same claude-code program
    /// `claude-code/mux-pane` runs: same engine, same `~/.claude/projects` store,
    /// same claude-shaped JSONL, same `--session-id` identity space. The proof it
    /// is the same space is that `join.rs` had to widen its dedup key to
    /// `(String, bool)` because an ACP row and a plain claude row can legitimately
    /// carry ONE sessionId, and `resolve::acp_floor_original` exists for the same
    /// collision.
    ///
    /// Measured against [`Mode::Extension`]'s own bar for why IT is not a third
    /// harness — "same binary, same store, same sessions, same `Provider` impl,
    /// same `--session-id` identity" — the ACP claude lane scored four of five.
    /// The miss was the `Provider` impl, and that is a consequence of the
    /// modelling rather than a fact about the world: `provider::provider_for`
    /// keys on a provider-id STRING, so the only way to give the bridge its own
    /// impl was to give it its own id.
    ///
    /// That registry is STILL string-keyed, and both legacy ids still resolve
    /// through it — replacing it with a `(harness, mode)` lookup is real work
    /// this change did not do. What the remodel did do is take the DECISIONS off
    /// the string: which bridge to spawn is `acp::acp_provider_for_harness`, and
    /// every carrier and topology question asks the lane.
    ///
    /// # This is a daemon lane
    ///
    /// [`Lane::is_daemon`] answers `true`, and every daemon-branch path must keep
    /// taking the daemon branch: no terminal of its own, kill is a pid-group
    /// reap, the receive path is the resident adapter's endpoint, health is
    /// process liveness. It has no viewer exception either: `attach_target`
    /// hands a terminal to `codex/app-server` alone ([`Lane::is_app_server`]),
    /// and an ACP bridge is refused there exactly as it was as a harness.
    ///
    /// # What still differs per harness
    ///
    /// The bridge program and its argv, and where the transcript lands. The claude
    /// bridge writes into claude's store (so `claude-code/acp` has NO cold store
    /// of its own — claude's pane lane owns it); opencode's bridge persists to
    /// opencode's own `opencode.db`. Both are harness facts, keyed on the harness,
    /// which is exactly the shape this remodel makes available.
    Acp,
}

impl Harness {
    /// Every harness, in a stable order.
    pub const ALL: [Harness; 4] = [
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::Pi,
        Harness::Opencode,
    ];

    /// The canonical provider id `qd start --provider <id>` resolves — the same
    /// string `dispatch::provider::provider_for` keys on.
    ///
    /// EVERY ONE OF THESE IS NOW A BARE PROGRAM NAME, with no `/` in it. That is
    /// not tidiness: `Lane::id()` joins a provider id and a hosting token with a
    /// `/`, and while a provider id could itself contain one, the lane id was
    /// three segments for `acp/*` and two for everything else, and
    /// [`Lane::from_id`] had to split on the LAST `/` to survive it. A lane id is
    /// now always exactly `<program>/<topology>`.
    pub fn provider_id(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Opencode => "opencode",
        }
    }

    /// Parse a provider id into its harness, discarding any lane the spelling
    /// pins. An unknown id is `None` — never a guess.
    ///
    /// **Most callers want [`harness_and_pinned_mode`] instead.** This answers
    /// "which program", and the legacy `acp/*` spellings answer more than that —
    /// they name a lane outright. A caller that asks only for the harness and then
    /// reaches for a default mode will place every ACP row in its harness's
    /// DEFAULT lane, which for `acp/claude-code` means the mux pane: a session
    /// with no pane, addressed by every verb as though it had one.
    pub fn from_provider_id(id: &str) -> Option<Harness> {
        harness_and_pinned_mode(id).map(|(h, _)| h)
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
            // opencode has exactly one lane and this is it: everything live goes
            // over ACP (`provider/opencode/mod.rs` — "There is no opencode-only
            // PROTOCOL code to hold here"). There is nothing here to move it to.
            Harness::Opencode => Mode::Acp,
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
            Harness::Codex | Harness::Pi => Mode::Daemon,
            // AMENDED 2026-08-24, and the ONLY amendment DEC-3 has ever taken.
            // Was `Mode::Daemon`, with every other harness.
            //
            // Read the freeze's own rule and it REQUIRES this move rather than
            // forbidding it: an unstamped row means the lane it was written
            // under. Every opencode row ever written was written under the ACP
            // bridge — opencode has no other live path, and never had one — so
            // `Mode::Daemon` was not the lane those rows were written under. It
            // was the token the bridge happened to stamp while ACP was spelled as
            // a harness rather than a topology, and `Harness::Opencode` no longer
            // SUPPORTS `Mode::Daemon`, so leaving it here would re-derive every
            // hosting-less opencode row into a lane that does not exist.
            //
            // What the freeze forbids is re-pointing this at
            // `create_default_mode` so that a moved default silently relabels
            // rows on disk. That is untouched: the two functions still disagree
            // for codex and pi, deliberately, and this arm agrees with its
            // `create_default_mode` twin for the same reason `ClaudeCode` does —
            // because the harness has one lane, not because anything was
            // resynchronised.
            Harness::Opencode => Mode::Acp,
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
            // `claude-code/mux-pane` owns claude's store, and `claude-code/acp`
            // therefore enumerates ZERO — exactly as `acp/claude-code/daemon` did
            // before it was folded in. This is the arm that would double-count if
            // it were wrong: the ACP bridge writes into `~/.claude/projects`, so
            // the two claude lanes read the SAME directory, and two claimants
            // means every cold claude row appears twice in `qd ls`.
            Harness::ClaudeCode => Mode::Pane,
            Harness::Codex | Harness::Pi => Mode::Daemon,
            // Tracks the ROW default, as this function must — see its docs.
            Harness::Opencode => Mode::Acp,
        }
    }

    /// Whether this harness can be hosted in `mode` at all.
    ///
    /// This is a STRUCTURAL fact, not an unimplemented feature: claude-code has
    /// no daemon lane, and opencode has no terminal of its own to put in a pane.
    pub fn supports(self, mode: Mode) -> bool {
        // Written as the NEGATIVE set on purpose: the impossible combinations
        // are the interesting content, and listing them keeps this readable as
        // "what does not exist" rather than "what does".
        !matches!(
            (self, mode),
            // claude-code is a TUI in a mux pane, or the same engine reached
            // through an ACP bridge. There is no headless claude to host, and no
            // pi-style extension loader or codex-style app-server residence.
            (Harness::ClaudeCode, Mode::Daemon | Mode::Extension | Mode::AppServer)
                // opencode has exactly ONE lane. `opencode acp` is how qd drives
                // it and the only way qd drives it; the sibling `store` reader is
                // a cold read of `opencode.db`, not a lane. There is no opencode
                // TUI in a qd pane, no headless opencode resident, no extension.
                | (Harness::Opencode, Mode::Pane | Mode::Daemon | Mode::Extension | Mode::AppServer)
                // `extension` is pi's alone, and structurally so: it names a
                // SPECIFIC affordance — pi's `--extension <path>` loader plus an
                // extension API with `sendUserMessage`, `isIdle` and `abort`
                // (verified against pi 0.84.1) — that no other harness in this
                // repo has. codex exposes no in-process extension surface at
                // all, and claude-code is already answered by the arm above.
                | (Harness::Codex, Mode::Extension)
                // `app-server` is codex's alone, and structurally so: it names a
                // SPECIFIC residence (`codex app-server --listen ws://…`) that a
                // human TUI can be pointed at (`codex --remote <endpoint>`). No
                // other harness in this repo has either half — pi's residence is
                // `<exe> pi-daemon`, and claude-code is answered above.
                | (Harness::Pi, Mode::AppServer)
                // --- Mode::Acp's negatives, and why they are a DIFFERENT KIND --
                //
                // Every other entry in this table means "does not exist". These
                // two mean **"not built here yet"**, and that distinction is
                // worth stating rather than hiding, because it is the first time
                // this function has held one.
                //
                // ACP is not an affordance of a particular program the way
                // `--extension` and `app-server` are. It is a general protocol
                // with adapters written for many agents, and
                // `doc/tbd/acp-everywhere-report.md` argues precisely that qd
                // could drive codex and pi through it;
                // `doc/tbd/pi-acp-exploration/` carries a skeleton for pi.
                // Neither adapter is wired into this repo, so neither lane can be
                // built today, and `supports` must answer for what qd can
                // actually host — but a reader who finds these two arms and
                // assumes the usual "structurally impossible" reading would be
                // wrong, and would conclude the wrong thing about how much work
                // `codex/acp` is.
                //
                // The rule this table keeps is therefore unchanged in substance:
                // an arm here means qd cannot host that lane. What is new is that
                // for these two the reason is a missing adapter rather than a
                // missing affordance, and when one lands the arm is deleted and
                // `Lane::ALL` grows.
                | (Harness::Codex | Harness::Pi, Mode::Acp)
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
            Mode::Acp => "acp",
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
            "acp" => Some(Mode::Acp),
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

/// claude-code's mux-pane lane, by name.
///
/// Exists because several guards are about THAT LANE and not about the
/// claude-code harness, and once the harness has two lanes the difference stops
/// being academic. The relay fast-path filter is the worked example: the relay is
/// this lane's carrier, `claude-code/acp` has none, and both answer
/// `provider_id() == "claude-code"` — so any guard spelled as a provider-name
/// test admits the wrong one. Naming the lane makes that unwriteable.
pub const CLAUDE_PANE: Lane = Lane {
    harness: Harness::ClaudeCode,
    mode: Mode::Pane,
};

impl Lane {
    /// Every VALID lane — nine, not the twenty of the cartesian product. See
    /// the module docs for why the missing combinations are structural.
    ///
    /// It was nine before ACP became a mode and it is nine after. Nothing was
    /// added or removed; `acp/claude-code/daemon` and `acp/opencode/daemon` are
    /// `claude-code/acp` and `opencode/acp`, on the axis they belonged on.
    pub const ALL: [Lane; 9] = [
        Lane {
            harness: Harness::ClaudeCode,
            mode: Mode::Pane,
        },
        Lane {
            harness: Harness::ClaudeCode,
            mode: Mode::Acp,
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
            harness: Harness::Opencode,
            mode: Mode::Acp,
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
        let (harness, pinned) = parse_provider_arg(provider_id)?;
        let requested = match topology {
            CreateTopology::Default => {
                // A spelling that NAMES a lane is its own default — `--provider
                // codex/daemon` creates that lane and not codex's, and `--provider
                // acp/claude-code` creates the ACP lane and not claude's pane.
                return Lane::new(harness, pinned.unwrap_or_else(|| harness.create_default_mode()));
            }
            CreateTopology::Interactive => Mode::Pane,
            CreateTopology::Extension => Mode::Extension,
            CreateTopology::AppServer => Mode::AppServer,
            CreateTopology::Daemon => Mode::Daemon,
            CreateTopology::Acp => Mode::Acp,
        };
        // A spelling that pins a lane and a flag that names a different one are a
        // REFUSAL, not a preference. `--provider codex/daemon --interactive` asks
        // for two lanes at once, and so does `--provider acp/claude-code
        // --interactive`; neither gets a silent winner.
        //
        // The older engine caught the second with a string test on what the user
        // typed, which is why `--provider opencode --interactive` — the alias for
        // the same bridge — slipped past it and got a daemon at exit 0. There is
        // no string to test now and no alias to slip: the two requests disagree,
        // so there is no lane, and the verb renders that.
        if pinned.is_some_and(|p| p != requested) {
            return None;
        }
        Lane::new(harness, requested)
    }

    /// The stable wire id: `<provider-id>/<hosting-token>`, e.g.
    /// `"claude-code/acp"`, `"codex/mux-pane"`. Always exactly two segments —
    /// no provider id contains a `/` of its own any more.
    pub fn id(self) -> String {
        format!(
            "{}/{}",
            self.harness.provider_id(),
            self.mode.hosting_token()
        )
    }

    /// Parse a stable wire id.
    ///
    /// Splits on the LAST `/`, which no longer matters for ids this code EMITS —
    /// every provider id is a bare program name now, so every emitted lane id is
    /// exactly two segments. It still matters for ids already written down:
    /// `acp/claude-code/daemon` is in `ls --json` goldens, on `qw attach`'s argv
    /// and in `13-calling-qw-directly.md`, and it must keep parsing to the lane it
    /// has always meant.
    ///
    /// A legacy compound provider pins its mode and the trailing token is
    /// IGNORED, deliberately: `acp/claude-code/daemon` says `daemon` because that
    /// is the token the bridge stamped while ACP was a harness, and the provider
    /// half is the more specific fact. Honouring the token instead would ask for
    /// `claude-code/daemon`, which is not a lane at all.
    pub fn from_id(s: &str) -> Option<Lane> {
        let (provider, hosting) = s.rsplit_once('/')?;
        let (harness, pinned) = harness_and_pinned_mode(provider)?;
        if let Some(mode) = pinned {
            return Lane::new(harness, mode);
        }
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
    ///
    /// **`Mode::Acp` answers `true` for the same reason**, and this arm is why
    /// ACP could become a mode without touching a single daemon-branch path. As
    /// `acp/claude-code` and `acp/opencode` the ACP lanes WERE `Mode::Daemon`, so
    /// every `is_daemon()` call site already routed them correctly; keeping that
    /// answer is what makes the remodel a re-coordination rather than a rewrite.
    /// If this omitted `Mode::Acp`, kill would stop reaping the pid group, the
    /// receive path would look for a pane, health would read a transcript tail
    /// and `attach` would try to hand over a terminal that does not exist — all
    /// silently, all at once.
    pub fn is_daemon(self) -> bool {
        matches!(self.mode, Mode::Daemon | Mode::AppServer | Mode::Acp)
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
            //
            // `Mode::Acp`, not `Mode::Daemon`. This lane was `acp/opencode/daemon`
            // when the viewer landed; it is `opencode/acp` now, and `Opencode`
            // does not support `Mode::Daemon` at all — so a `Daemon` arm here
            // would answer `false` for every opencode session there is, and
            // `attach` would report that the residence is not joinable on the one
            // lane this predicate was written for.
            | (Harness::Opencode, Mode::Acp)
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

    /// Can a caller that blocks on a send get the **reply body** back?
    ///
    /// This is `qd send --wait`'s gate, and it is a LANE property for the same
    /// reason [`Lane::has_viewer`] is: the alternative is a provider-name list at
    /// the call site, which is the shape that has already been walked past three
    /// times in this file's history (`acp/` prefix tests, the `opencode` alias,
    /// `claude-code/acp`). A lane cannot be walked past.
    ///
    /// # It is NOT "does a send block", and it is not [`LaneOps::await_terminal`]
    ///
    /// Every lane can answer *did this message reach a terminal*
    /// (`LaneOps::await_terminal`, implemented for all nine), and `qd wait` can
    /// block any session busy→idle. Neither produces the assistant's TEXT. This
    /// predicate answers the third, narrower question — *is there a channel that
    /// hands the reply body back to the sender* — and only one lane has one, by
    /// two independent routes that are both its own:
    ///
    ///   - the **relay wire**: the recipient calls the relay's `reply` MCP tool
    ///     with the message id and the relay server buffers the body for a
    ///     long-poll to collect (`qd send --wait --carrier relay`);
    ///   - the **pane wire**: the sender anchors on the message's own user record
    ///     in claude's JSONL transcript and reads the assistant blocks that follow
    ///     it (`qd send --wait --carrier pty`).
    ///
    /// The daemon lanes have neither. A codex turn, an ACP prompt and a pi
    /// resident turn all report ACCEPTANCE and mint a turn id; none of them
    /// carries the completed text back to whoever sent it, which is why the
    /// codex / ACP / pi send arms have always ignored a `--wait` rather than
    /// implementing one. Answering `true` for them would make `--wait` a silent
    /// no-op — the failure mode this predicate exists to turn into a refusal.
    ///
    /// # And the OTHER two pane lanes answer `false`, which is the sharp edge
    ///
    /// `codex/mux-pane` and `pi/mux-pane` DO reach the pane carrier — the PTY
    /// body is provider-generic and `crate::delivery::pty` resolves their
    /// transcripts through the provider rather than under claude's tree. What
    /// they do not reach is a reply. The extractor on the far side of that wait
    /// is [`crate::sendpty::extract_response`], and it reads ONE schema: records
    /// with `type: "assistant"`, a `message.content` array, and
    /// `stop_reason == "end_turn"`, with `thinking` and `tool_use` block types
    /// for `--full`. That is claude's transcript, not a codex rollout and not a
    /// pi transcript, so the capture comes back empty and
    /// [`crate::sendpty::capture_or_defect`] turns it into a loud non-zero
    /// "capture EMPTY" — a wait that costs the full timeout and answers nothing.
    ///
    /// Which is why this is not [`Lane::is_pane`] with an exception bolted on.
    /// The property is "there is a channel that hands the reply body back", and
    /// for the pane wire that channel is a transcript SHAPE. Widening this
    /// predicate is a real change with a real prerequisite: teach the extractor
    /// the other harness's records first, then add the lane here.
    ///
    /// [`LaneOps::await_terminal`]: crate::contract::LaneOps::await_terminal
    pub fn captures_reply(self) -> bool {
        self == CLAUDE_PANE
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
    /// It is what [`CreateTopology::Default`] resolves to for codex, so the plain
    /// `qd start --provider codex` reaches it — and it exists separately from
    /// `Default` so that pinning "the default is app-server" and "app-server is
    /// requestable BY NAME" stay two different assertions. Three spellings ask
    /// for it explicitly now: `--app-server`, `--provider codex/app-server`, and
    /// `qw`'s wire (`{"m":"start","lane":"codex/app-server"}`).
    ///
    /// This paragraph read "**has no `qd start` flag**, and does not need one"
    /// until `--app-server` landed, and the sentence outlived the fact by two
    /// changes. It is worth knowing why the claim was safe to make and then
    /// stopped being: a variant with no producer is a documented no-op, which is
    /// the dead-arm shape this enum exists to avoid, so "no flag" was always a
    /// statement with a short shelf life.
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
    /// For `claude-code` and `opencode` it is a REFUSAL, because neither
    /// supports `Mode::Daemon`: there is no headless claude to host, and
    /// opencode's only residence is its ACP bridge. `Lane::new` answers `None`
    /// and the caller renders that rather than quietly handing back another lane.
    Daemon,
    /// `--acp`: the Agent Client Protocol bridge lane.
    ///
    /// This variant is what keeps `claude-code/acp` REACHABLE. While ACP was a
    /// harness, naming the bridge and naming the program were the same act
    /// (`--provider acp/claude-code`); now that it is a topology, `--provider
    /// claude-code` names claude's default lane — the mux pane — and something
    /// has to spell the other one. Without this variant the remodel would delete
    /// a working lane from the CLI, which is exactly what `CreateTopology` exists
    /// to make visible: `start_routing_is_total_over_every_real_input` asserts
    /// that every one of the nine lanes is reachable from `qd start`.
    ///
    /// For `opencode` it is a no-op that names the truth — its only lane is this
    /// one. For `codex` and `pi` it is a refusal today, and the reason is a
    /// missing adapter rather than a missing affordance; see `Harness::supports`.
    Acp,
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
    let (harness, pinned) = harness_and_pinned_mode(provider_id)?;
    // A legacy `acp/*` provider id NAMES the lane, and beats the row's own
    // hosting stamp. See `harness_and_pinned_mode` for why that precedence is the
    // whole point rather than a detail.
    if let Some(mode) = pinned {
        return Lane::new(harness, mode);
    }
    let recorded = hosting_field
        .and_then(Mode::from_hosting_token)
        .filter(|&m| harness.supports(m));
    Some(Lane {
        harness,
        mode: recorded.unwrap_or_else(|| harness.row_default_mode()),
    })
}

/// Parse a provider spelling into its harness and, for the spellings that name
/// one, the lane they pin.
///
/// # The legacy `acp/*` ids, and why this returns a PAIR
///
/// `acp/claude-code` and `acp/opencode` are how every ACP session already on a
/// user's disk records its provider, and they are still accepted on `--provider`.
/// They are not aliases for a harness — they are aliases for a LANE, and treating
/// them as the former is the one way this remodel could have corrupted live
/// sessions.
///
/// Concretely, the shape that does not work. Map `acp/claude-code` to
/// `Harness::ClaudeCode` alone and hand the row's own `hosting: "daemon"` stamp
/// to the usual derivation, and: `ClaudeCode.supports(Daemon)` is `false`, so the
/// recorded token is dropped; the fallback is `row_default_mode(ClaudeCode)`,
/// which is `Mode::Pane`; and every ACP session on disk silently becomes
/// `claude-code/mux-pane`. Nothing errors. `kill` then reaps a pane that does not
/// exist, `attach` hands over a terminal that was never opened, and `deliver`
/// types into a PTY nothing is listening to. It is exactly the failure
/// `row_default_mode`'s freeze warns about, arriving through a door the freeze
/// does not watch — a ROW rewrite rather than a moved default.
///
/// So the pinned mode wins over the recorded token, deliberately. A legacy row
/// says `daemon` because that is what the bridge stamped while ACP was spelled as
/// a harness; the provider half of the same row is the more specific fact, and it
/// is the half that survived the rename with its meaning intact.
///
/// The pin is also a REFUSAL surface at create time — see [`Lane::for_create`],
/// where a pinned spelling that disagrees with an explicit topology flag yields
/// no lane at all.
/// Is THIS ROW an ACP bridge session?
///
/// The question a dozen call sites were asking as `provider.starts_with("acp/")`,
/// and the reason that spelling had to go: a new ACP row's provider is the
/// PROGRAM (`claude-code`, `opencode`), so the prefix stopped matching the moment
/// ACP became a lane, and every one of those guards would silently have answered
/// `false` for exactly the sessions it exists to catch.
///
/// Takes both fields because that is what a row carries and what places it.
/// `lane_for` owns the legacy-spelling rule and the absent-means-default rule,
/// and neither belongs re-implemented at a call site.
pub fn row_is_acp(provider_id: &str, hosting_field: Option<&str>) -> bool {
    lane_for(provider_id, hosting_field).is_some_and(|l| l.mode == Mode::Acp)
}

/// Parse a `--provider` argument, which may name a PROGRAM or a LANE.
///
/// `--provider codex` says which agent to run and leaves the topology to the
/// default and the flags. `--provider codex/daemon` says both at once. Both are
/// first-class, and the second is not sugar for the first plus a flag — it is
/// the same act of naming a lane that `Lane::id()` performs in the other
/// direction, and it is the spelling `qd ls --json` already hands back.
///
/// # Why `a/b` is not ambiguous
///
/// A lane id and a legacy compound provider id are both `a/b`, and they mean
/// opposite things: in `claude-code/acp` the FIRST segment is the program, and in
/// `acp/claude-code` the SECOND one is. Nothing about their SHAPE separates them.
///
/// What separates them is that the two readings are DISJOINT — no string is both.
/// `Lane::from_id` rsplits and asks for the head, and for every id the
/// whole-string lookup accepts that head is either absent or `acp`, which stopped
/// being a provider id when ACP became a lane. So the order of the two branches
/// below is a readability choice and not a correctness one, and saying otherwise
/// would be a comfortable lie: an order that merely PREFERS one reading leaves
/// the other reachable by a future edit, and this cannot be reordered into a
/// different answer at all. Pinned by
/// `the_provider_reading_and_the_lane_reading_are_disjoint`.
///
/// Disjointness is what the ACP remodel bought, and it is stronger than a
/// tiebreak. `acp/*` is now a frozen set of two legacy spellings rather than a
/// growing family of `acp/<program>` harnesses — both
/// `doc/tbd/acp-everywhere-report.md` and `doc/tbd/pi-acp-exploration/` were
/// arguing for more of those — so the head of a compound id can never again be a
/// program name.
///
/// This is what made `provider/lane` unimplementable while ACP was a harness.
/// `--provider acp/claude-code` was then the ADVERTISED way to name that agent,
/// and `acp` was the head of an OPEN family: a rule stating which segment is the
/// program would have been a membership test against a table that was still
/// growing and that no user could see. ACP-as-a-lane closed the family, which is
/// what left room for this.
///
/// A lane id that names a real shape but not a real lane (`codex/acp`,
/// `claude-code/daemon`) is `None`, exactly as `Lane::from_id` answers it — the
/// caller refuses rather than being handed a lane with nothing behind it.
pub fn parse_provider_arg(arg: &str) -> Option<(Harness, Option<Mode>)> {
    if let Some(hit) = harness_and_pinned_mode(arg) {
        return Some(hit);
    }
    Lane::from_id(arg).map(|l| (l.harness, Some(l.mode)))
}

pub fn harness_and_pinned_mode(id: &str) -> Option<(Harness, Option<Mode>)> {
    match id {
        "claude-code" => Some((Harness::ClaudeCode, None)),
        "codex" => Some((Harness::Codex, None)),
        "pi" => Some((Harness::Pi, None)),
        "opencode" => Some((Harness::Opencode, None)),
        // LEGACY, and permanent. These are on disk in registry rows, in identity
        // tombstones, in `ls --json` goldens and in scripted `--provider`
        // arguments. `16-default-lane-switch.md` dropped its backfill rather than
        // deferring it, on the rule that a row means the lane it was written
        // under; by that same rule these two strings mean the ACP lane forever,
        // and there is no release at which dropping them becomes safe.
        "acp/claude-code" => Some((Harness::ClaudeCode, Some(Mode::Acp))),
        "acp/opencode" => Some((Harness::Opencode, Some(Mode::Acp))),
        _ => None,
    }
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
            for m in [
                Mode::Pane,
                Mode::Daemon,
                Mode::Extension,
                Mode::AppServer,
                Mode::Acp,
            ] {
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
    fn the_missing_combinations_are_structural() {
        assert!(
            !Harness::ClaudeCode.supports(Mode::Daemon),
            "claude has no headless lane: it is a TUI in a pane, or the same \
             engine behind an ACP bridge"
        );
        assert!(
            !Harness::Opencode.supports(Mode::Pane),
            "qd hosts no opencode TUI in a pane"
        );
        assert!(
            !Harness::Opencode.supports(Mode::Daemon),
            "opencode's only residence IS its ACP bridge — `opencode/daemon` \
             would be a second spelling for the same thing, and a lane with no \
             arm behind it"
        );
        assert_eq!(Lane::new(Harness::ClaudeCode, Mode::Daemon), None);
        // `app-server` is codex's alone.
        for h in [Harness::ClaudeCode, Harness::Pi, Harness::Opencode] {
            assert!(
                !h.supports(Mode::AppServer),
                "{h:?} must not claim an app-server residence"
            );
        }
        assert!(Harness::Codex.supports(Mode::AppServer));
    }

    /// **The two ACP lanes, and the two that are absent for a DIFFERENT REASON.**
    ///
    /// Kept apart from the structural test above on purpose. Every refusal there
    /// means "does not exist": there is no headless claude, no opencode TUI, no
    /// second app-server. The two refusals here mean **"not built here yet"** —
    /// ACP is a general protocol and adapters for codex and pi are conceivable
    /// (`doc/tbd/acp-everywhere-report.md` argues for them,
    /// `doc/tbd/pi-acp-exploration/` carries a skeleton), they are simply not
    /// wired into this repo.
    ///
    /// If one lands, THIS is the test that should go red — not the structural
    /// one — and the fix is to delete an arm from `Harness::supports` rather than
    /// to argue with a claim about what cannot exist.
    #[test]
    fn acp_is_a_lane_of_the_program_behind_it() {
        assert!(
            Harness::ClaudeCode.supports(Mode::Acp),
            "the bridge runs the real claude engine into claude's own store"
        );
        assert!(
            Harness::Opencode.supports(Mode::Acp),
            "everything live for opencode goes over ACP"
        );
        assert_eq!(
            Lane::new(Harness::ClaudeCode, Mode::Acp).map(|l| l.id()).as_deref(),
            Some("claude-code/acp")
        );
        assert_eq!(
            Lane::new(Harness::Opencode, Mode::Acp).map(|l| l.id()).as_deref(),
            Some("opencode/acp")
        );
        // NOT YET BUILT, which is not the same claim as the arms above.
        for h in [Harness::Codex, Harness::Pi] {
            assert!(
                !h.supports(Mode::Acp),
                "{h:?} has no ACP adapter wired up in qd YET — if one lands, \
                 delete its arm from `supports` and update this test"
            );
        }
    }

    /// Both claude lanes are the same PROGRAM, and the ACP one is a daemon.
    ///
    /// The three assertions the remodel turns on. If `is_daemon` ever stopped
    /// answering `true` for `Mode::Acp`, kill would stop reaping the pid group,
    /// the receive path would look for a pane, and attach would try to hand over
    /// a terminal that does not exist — all silently.
    #[test]
    fn the_two_claude_lanes_share_a_program_and_split_on_topology() {
        let pane = Lane::new(Harness::ClaudeCode, Mode::Pane).unwrap();
        let acp = Lane::new(Harness::ClaudeCode, Mode::Acp).unwrap();
        assert_eq!(pane.harness, acp.harness, "one program, two topologies");
        assert_eq!(
            pane.harness.provider_id(),
            acp.harness.provider_id(),
            "…and therefore ONE provider id — which is exactly why no guard may \
             route on the provider string alone"
        );
        assert!(pane.is_pane() && !pane.is_daemon());
        assert!(acp.is_daemon() && !acp.is_pane());
        // The ACP lane is not attachable: `attach_target` hands a terminal to
        // `codex/app-server` alone.
        assert!(!acp.is_app_server());
    }

    #[test]
    fn ids_round_trip() {
        for lane in Lane::ALL {
            assert_eq!(
                Lane::from_id(&lane.id()),
                Some(lane),
                "{lane} must round-trip"
            );
        }
        // Every EMITTED id is exactly two segments now — no provider id carries a
        // `/` of its own any more.
        for lane in Lane::ALL {
            assert_eq!(
                lane.id().matches('/').count(),
                1,
                "{lane} must be <program>/<topology>, one slash"
            );
        }
    }

    /// **`row_is_acp` sees an ACP row however it is spelled — and the prefix it
    /// replaced sees neither.**
    ///
    /// Seven guards asked `provider.starts_with("acp/")` to decide a row's class:
    /// the join's cross-class dedup key and its tombstone-sid set,
    /// `acp_floor_original`, `qd ls`'s liveness-gate exemption and its status
    /// override, and `send:relay`'s ACP routing arm and unknown-provider gate.
    ///
    /// Every one of them broke the moment an ACP row started carrying the PROGRAM
    /// as its provider. Not loudly: the prefix simply answers `false`, so the
    /// dedup key collapses an ACP row into its plain claude twin and shadows it
    /// out of every join-derived surface, and `qd send` walks past the ACP path
    /// to report "has no relay". The assertion that matters is the last one here.
    #[test]
    fn row_is_acp_sees_both_spellings_and_the_prefix_sees_neither() {
        // The current spelling, as a create stamps it.
        assert!(row_is_acp("claude-code", Some("acp")));
        assert!(row_is_acp("opencode", Some("acp")));
        // …and opencode's hosting-less rows, whose only lane is the bridge.
        assert!(row_is_acp("opencode", None));
        // The legacy spelling, as rows written before the remodel carry it.
        assert!(row_is_acp("acp/claude-code", Some("daemon")));
        assert!(row_is_acp("acp/opencode", Some("daemon")));

        // Not ACP: the other claude lane, which shares the provider id. This is
        // the pair no provider-string test can separate.
        assert!(!row_is_acp("claude-code", Some("mux-pane")));
        assert!(!row_is_acp("claude-code", None));
        assert!(!row_is_acp("codex", Some("daemon")));
        assert!(!row_is_acp("nonsense", None));

        // MUTATION EVIDENCE: the prefix these guards used cannot see a
        // current-spelling ACP row at all. If this ever passes, `row_is_acp` has
        // been quietly replaced by the thing it was written to remove.
        assert!(
            !"claude-code".starts_with("acp/"),
            "an ACP row's provider is the PROGRAM — the prefix test is blind to it"
        );
    }

    /// **A NEWLY WRITTEN row must round-trip back to the lane that wrote it.**
    ///
    /// The create path stamps two fields, and after the remodel they come from
    /// different places: `provider` from `Harness::provider_id()` and `hosting`
    /// from the lane's `Mode`. That split is where a real bug lived for the
    /// length of one commit. The provider stamp moved first — `claude-code`
    /// instead of `acp/claude-code` — while the ACP create still wrote
    /// `hosting: "daemon"`, and `lane_for("claude-code", Some("daemon"))` finds
    /// that claude supports no daemon lane, falls back to `row_default_mode`,
    /// and answers `claude-code/mux-pane`. Every ACP session created after that
    /// point would have been addressed as a pane that was never opened: kill
    /// reaping nothing, attach handing over a terminal that does not exist,
    /// deliver typing into a PTY with no reader.
    ///
    /// Nothing would have failed at the moment of the create. This asserts the
    /// property that was violated — a round trip through the two fields a row
    /// actually carries — for every lane, so no future change to either stamp
    /// can separate them again in silence.
    #[test]
    fn every_lane_round_trips_through_the_two_fields_a_row_carries() {
        for lane in Lane::ALL {
            let provider = lane.harness.provider_id();
            let hosting = lane.mode.hosting_token();
            assert_eq!(
                lane_for(provider, Some(hosting)),
                Some(lane),
                "a row stamped provider={provider:?} hosting={hosting:?} must read back \
                 as {lane}, not as some other lane of the same harness"
            );
        }
    }

    /// **`--provider <program>/<lane>` names a lane, and cannot be confused with
    /// the legacy compound provider ids.**
    ///
    /// The two shapes are both `a/b` and they mean opposite things — in
    /// `claude-code/acp` the first segment is the program, in `acp/claude-code`
    /// the second one is. This is the collision that made the syntax
    /// unimplementable while ACP was a harness, because `acp/claude-code` was then
    /// the ADVERTISED way to name that agent and a first-segment-is-the-program
    /// rule read it as a program that does not exist.
    ///
    /// It resolves by asking the closed question first: is the whole string a
    /// provider id? The legacy set is frozen at two, so no lane id can ever fall
    /// into it.
    #[test]
    fn a_lane_id_may_be_named_as_the_provider_argument() {
        for (arg, expected) in [
            ("codex/daemon", Some("codex/daemon")),
            ("codex/mux-pane", Some("codex/mux-pane")),
            ("codex/app-server", Some("codex/app-server")),
            ("claude-code/mux-pane", Some("claude-code/mux-pane")),
            ("claude-code/acp", Some("claude-code/acp")),
            ("pi/extension", Some("pi/extension")),
            ("opencode/acp", Some("opencode/acp")),
            // A bare program still means its default lane, unchanged.
            ("codex", Some("codex/app-server")),
            ("claude-code", Some("claude-code/mux-pane")),
            // The legacy compound ids keep meaning the ACP lane — read as a
            // provider id, never split as `acp` + a lane.
            ("acp/claude-code", Some("claude-code/acp")),
            ("acp/opencode", Some("opencode/acp")),
            // Shapes that parse but name no lane.
            ("claude-code/daemon", None),
            ("codex/acp", None),
            ("pi/acp", None),
            ("opencode/mux-pane", None),
            ("codex/sideways", None),
            ("weird/daemon", None),
            ("acp/pi", None),
            ("", None),
        ] {
            assert_eq!(
                Lane::for_create(arg, CreateTopology::Default)
                    .map(|l| l.id())
                    .as_deref(),
                expected,
                "--provider {arg}"
            );
        }
    }

    /// **The two readings of `a/b` never both match — which is why the grammar is
    /// unambiguous rather than merely disambiguated.**
    ///
    /// `parse_provider_arg` tries the whole string as a provider id and then reads
    /// it as a lane, and it is tempting to describe that order as load-bearing. It
    /// is not, and the difference matters: an order that merely PREFERS one
    /// reading leaves the other reachable by a future edit, while disjoint
    /// branches cannot be reordered into a different answer at all.
    ///
    /// They are disjoint because `Lane::from_id` rsplits and then asks
    /// `harness_and_pinned_mode` for the FIRST part. For the six provider ids
    /// that whole-string lookup accepts, that first part is either absent (no
    /// slash) or `acp` — which stopped being a provider id when ACP became a
    /// lane. So every string one branch accepts, the other refuses.
    ///
    /// This asserts that over both closed sets, rather than asserting an order.
    /// It is the property `18-provider-names-a-lane.md` §3 argues the ACP remodel
    /// bought: not "we picked a winner" but "there is nothing to pick between".
    #[test]
    fn the_provider_reading_and_the_lane_reading_are_disjoint() {
        // Every spelling the whole-string branch accepts is refused by the lane
        // branch…
        for id in [
            "claude-code",
            "codex",
            "pi",
            "opencode",
            "acp/claude-code",
            "acp/opencode",
        ] {
            assert!(harness_and_pinned_mode(id).is_some(), "{id} is a provider id");
            assert_eq!(
                Lane::from_id(id),
                None,
                "{id} must NOT also read as a lane id, or the grammar would have to \
                 choose and the choice could be edited"
            );
        }
        // …and every lane id is refused by the whole-string branch.
        for lane in Lane::ALL {
            let id = lane.id();
            assert_eq!(
                harness_and_pinned_mode(&id),
                None,
                "{id} must NOT also read as a provider id"
            );
            assert_eq!(Lane::from_id(&id), Some(lane));
        }
    }

    /// The legacy spellings resolve, in both the two- and three-segment forms.
    ///
    /// Two segments (`acp/claude-code`) reach the lane only through the
    /// whole-string branch; three (`acp/claude-code/daemon`) only through the lane
    /// branch, whose rsplit leaves the compound provider id intact as the head.
    /// Both forms are written down in places that outlive a release — registry
    /// rows, scripted `--provider` arguments, saved `ls --json` output — so both
    /// are asserted rather than assumed.
    #[test]
    fn both_legacy_forms_resolve_to_the_acp_lanes() {
        for (arg, expected) in [
            ("acp/claude-code", "claude-code/acp"),
            ("acp/opencode", "opencode/acp"),
            ("acp/claude-code/daemon", "claude-code/acp"),
            ("acp/opencode/daemon", "opencode/acp"),
        ] {
            assert_eq!(
                Lane::for_create(arg, CreateTopology::Default)
                    .map(|l| l.id())
                    .as_deref(),
                Some(expected),
                "--provider {arg}"
            );
        }
    }

    /// Naming a lane PINS it, so a topology flag that disagrees is refused rather
    /// than silently winning — the same rule the legacy spellings get, reached
    /// through the same mechanism.
    #[test]
    fn a_named_lane_refuses_a_flag_that_contradicts_it() {
        use CreateTopology::{Acp, Daemon, Interactive};
        assert_eq!(Lane::for_create("codex/daemon", Interactive), None);
        assert_eq!(Lane::for_create("codex/mux-pane", Daemon), None);
        assert_eq!(Lane::for_create("claude-code/acp", Interactive), None);
        assert_eq!(Lane::for_create("claude-code/mux-pane", Acp), None);
        // …and a flag that AGREES is a no-op, not a conflict.
        assert_eq!(
            Lane::for_create("codex/daemon", Daemon).map(|l| l.id()).as_deref(),
            Some("codex/daemon")
        );
        assert_eq!(
            Lane::for_create("claude-code/acp", Acp).map(|l| l.id()).as_deref(),
            Some("claude-code/acp")
        );
    }

    /// Every lane `qd ls --json` can PRINT is a lane `qd start --provider` will
    /// TAKE. The round trip is the promise the syntax makes: copy the `lane` field
    /// out of a listing, paste it after `--provider`, get that lane.
    #[test]
    fn every_printed_lane_id_is_accepted_as_a_provider_argument() {
        for lane in Lane::ALL {
            assert_eq!(
                Lane::for_create(&lane.id(), CreateTopology::Default),
                Some(lane),
                "{lane} is printed by `ls --json`; `--provider {}` must create it",
                lane.id()
            );
        }
    }

    /// **The legacy lane ids still parse, and to the lane they have always
    /// meant.**
    ///
    /// These strings are on disk and on wires that predate the remodel: in
    /// registry rows, in identity tombstones, in `ls --json` goldens, on `qw
    /// attach`'s argv and in `13-calling-qw-directly.md`. The trailing token says
    /// `daemon` because that is what the bridge stamped while ACP was spelled as
    /// a harness; the provider half is the more specific fact and wins.
    #[test]
    fn the_legacy_acp_ids_still_parse_to_the_acp_lanes() {
        assert_eq!(
            Lane::from_id("acp/claude-code/daemon").map(|l| l.id()).as_deref(),
            Some("claude-code/acp")
        );
        assert_eq!(
            Lane::from_id("acp/opencode/daemon").map(|l| l.id()).as_deref(),
            Some("opencode/acp")
        );
        // The legacy PROVIDER half alone also places a row, which is what
        // `lane_for` needs for every ACP session already on disk.
        assert_eq!(
            lane_for("acp/claude-code", Some("daemon")).map(|l| l.id()).as_deref(),
            Some("claude-code/acp"),
            "an ACP row on disk must NOT re-derive to claude's pane lane"
        );
        assert_eq!(
            lane_for("acp/opencode", Some("daemon")).map(|l| l.id()).as_deref(),
            Some("opencode/acp")
        );
        // …and it wins over whatever the row happens to say, including nothing.
        for hosting in [None, Some("daemon"), Some("mux-pane"), Some("garbage")] {
            assert_eq!(
                lane_for("acp/claude-code", hosting).map(|l| l.id()).as_deref(),
                Some("claude-code/acp"),
                "the legacy provider id pins the lane; hosting {hosting:?} cannot move it"
            );
        }
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
        assert_eq!(Lane::from_id("opencode/daemon"), None);
        // Not built here yet — parses as a shape, refused as a lane.
        assert_eq!(Lane::from_id("codex/acp"), None);
        assert_eq!(Lane::from_id("pi/acp"), None);
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
            Acp, AppServer as App, Daemon as Dae, Default as Def, Extension as Ext,
            Interactive as Inter,
        };
        // (provider id as typed, the topology asked for, the lane created)
        let table: [(&str, CreateTopology, Option<&str>); 42] = [
            // claude has TWO lanes now, and `--acp` is what spells the second
            // one. --daemon is still refused: there is no headless claude to
            // host, and the ACP resident is not one — it is the same engine
            // behind a bridge, which is what `--acp` asks for.
            ("claude-code", Def, Some("claude-code/mux-pane")),
            ("claude-code", Inter, Some("claude-code/mux-pane")),
            ("claude-code", Acp, Some("claude-code/acp")),
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
            // Not built here yet, and refused the same way an impossible lane is
            // — `qd start` cannot create what qd cannot host.
            ("codex", Acp, None),
            // The pi block, the same shape. `--extension` survives as an
            // explicit flag even though it now names the default — existing
            // scripts pass it, and it costs nothing.
            ("pi", Def, Some("pi/extension")),
            ("pi", Inter, Some("pi/mux-pane")),
            ("pi", Ext, Some("pi/extension")),
            ("pi", Dae, Some("pi/daemon")),
            ("pi", App, None),
            ("pi", Acp, None),
            // opencode has exactly one lane and it is the bridge. `--acp` names
            // it; every other flag asks for something opencode does not have.
            ("opencode", Def, Some("opencode/acp")),
            ("opencode", Acp, Some("opencode/acp")),
            ("opencode", Inter, None),
            ("opencode", Dae, None),
            ("opencode", Ext, None),
            ("opencode", App, None),
            // --- the LEGACY spellings, which PIN a lane --------------------
            //
            // `acp/claude-code` names the ACP lane outright, so it is its own
            // default and `--acp` is a no-op that agrees with it. Every flag
            // naming a DIFFERENT lane is refused — including `--interactive`,
            // which is the bug this pinning fixes: the old engine tested the
            // string the user typed, so `--provider acp/claude-code
            // --interactive` was caught while `--provider opencode --interactive`
            // (the alias for the same harness) slipped past and silently got a
            // daemon. There is no string to test now and no alias to slip.
            ("acp/claude-code", Def, Some("claude-code/acp")),
            ("acp/claude-code", Acp, Some("claude-code/acp")),
            ("acp/claude-code", Inter, None),
            ("acp/claude-code", Dae, None),
            ("acp/claude-code", Ext, None),
            ("acp/claude-code", App, None),
            ("acp/opencode", Def, Some("opencode/acp")),
            ("acp/opencode", Acp, Some("opencode/acp")),
            ("acp/opencode", Inter, None),
            ("acp/opencode", Dae, None),
            ("acp/opencode", Ext, None),
            ("acp/opencode", App, None),
            // Fail-closed on garbage, in every mode — never a claude
            // fall-through, which is what the pre-refusal engine did.
            ("nope", Def, None),
            ("nope", Inter, None),
            ("nope", Ext, None),
            ("nope", Dae, None),
            ("nope", App, None),
            ("nope", Acp, None),
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
            ("acp/claude-code", "claude-code/acp"),
            ("acp/opencode", "opencode/acp"),
            // The CLI ergonomic alias, which must default to the same lane as
            // the internal id rather than acquiring a default of its own.
            ("opencode", "opencode/acp"),
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
    fn the_frozen_defaults_are_frozen() {
        for h in Harness::ALL {
            let expected = match h {
                Harness::ClaudeCode => Mode::Pane,
                Harness::Codex | Harness::Pi => Mode::Daemon,
                // AMENDED 2026-08-24 — the one amendment DEC-3 has taken, and
                // the freeze's own rule is what REQUIRED it. Every opencode row
                // ever written was written under the ACP bridge (opencode has no
                // other live path), so `Mode::Daemon` was never the lane those
                // rows meant; it was the token stamped while ACP was spelled as
                // a harness. `Harness::Opencode` no longer supports
                // `Mode::Daemon`, so leaving it would re-derive every
                // hosting-less opencode row into a lane that does not exist.
                //
                // What DEC-3 forbids — re-pointing this at `create_default_mode`
                // so a moved default relabels rows — is untouched: codex and pi
                // still disagree between the two, deliberately.
                Harness::Opencode => Mode::Acp,
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
        // The legacy ACP spelling pins its lane rather than taking a default —
        // see `the_legacy_acp_ids_still_parse_to_the_acp_lanes`.
        assert_eq!(
            lane_for("acp/claude-code", None),
            Lane::new(Harness::ClaudeCode, Mode::Acp)
        );
        assert_eq!(
            lane_for("opencode", None),
            Lane::new(Harness::Opencode, Mode::Acp)
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
            Lane::new(Harness::Opencode, Mode::Acp)
        );
    }

    #[test]
    fn lane_for_an_unknown_provider_is_none() {
        assert_eq!(lane_for("nope", None), None);
        assert_eq!(lane_for("nope", Some("daemon")), None);
        assert_eq!(lane_for("", None), None);
    }

    #[test]
    /// **opencode has ONE spelling now, and the alias is retired into history.**
    ///
    /// It used to have three: `opencode` (what a user typed), `acp/opencode`
    /// (what qd stored), and a bare `opencode` label on cold rows read from
    /// `opencode.db`. The split was a direct consequence of ACP being modelled as
    /// a harness — the transport had to be in the id, so the id could not be the
    /// program name — and it cost real correctness: `send_relay`'s F4 guard had
    /// to name both spellings, and missed one for a while.
    ///
    /// `acp/opencode` still PARSES, because it is on disk. It is no longer what
    /// anything emits.
    #[test]
    fn opencode_has_one_spelling_and_the_legacy_id_still_parses() {
        assert_eq!(Harness::Opencode.provider_id(), "opencode");
        assert_eq!(
            Harness::from_provider_id("opencode"),
            Some(Harness::Opencode)
        );
        assert_eq!(
            Harness::from_provider_id("acp/opencode"),
            Some(Harness::Opencode)
        );
        assert_eq!(
            lane_for("opencode", None).map(|l| l.id()).as_deref(),
            Some("opencode/acp")
        );
        // The legacy spelling lands on the SAME lane — one lane, two spellings,
        // rather than two lanes.
        assert_eq!(
            lane_for("acp/opencode", None),
            lane_for("opencode", None)
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
