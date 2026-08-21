//! The seven real [`LaneOps`] implementations.
//!
//! These used to live in the `qd` binary, because they delegated to verb
//! functions a library cannot reach. They are here now, and ALL NINE methods
//! reach nothing outside this crate: stage-3 phase 3B moved the last of the five
//! delivery bodies into [`crate::delivery`], so [`LaneOps::deliver`] calls them
//! directly and the `Carriers` callback — the one edge that pointed from `qw` UP
//! into the `qd` binary — is deleted rather than narrowed. The shape is the one
//! [`LaneOps`] itself prescribes: effects live behind `&self`, resolved from the
//! injected [`Env`] and [`QdPaths`], never from the process.
//!
//! # What moved
//!
//! - **`attach` is fully qw-native.** It needed only mux construction, and
//!   `mux_selector` was already here and already pure — the binary's `real_mux`
//!   was a two-line `eprintln!` wrapper over it.
//! - **Session lookup is qw-native**, but deliberately NOT the binary's
//!   `resolve_session_uncapped`. That resolves a *human* query — fuzzy name and
//!   prefix matching, liveness-aware disambiguation, an ambiguity-refusal UI —
//!   and to do it runs a full cross-backend gather over mux panes, the process
//!   table and relay probes. A [`SessionId`] is definitionally unambiguous, so
//!   [`row_for_id`] does the cheap exact lookup instead. Fuzzy addressing stays
//!   qd's job and feeds a concrete id in.
//! - **`wake` is qw-native too, now.** Its six revives used to be verb bodies
//!   with printing interleaved through their control flow, reachable only
//!   through an injected `LaneDeps` seam. All six have since been split into a
//!   library core plus a thin printing wrapper — the shape
//!   [`crate::create::run_new`] prescribes — so `wake` calls the cores directly
//!   and the seam is gone, exactly as its doc said it would be.
//!
//! # Why `wake` builds its own effects
//!
//! Every other [`LaneOps`] method needs at most the mux; the revives need a
//! clock, a detached spawner, a port allocator, a cmdline probe, the socket dirs
//! and the ids store. Building those at construction would make `lane_ops`
//! expensive for the six methods that never touch them, so `wake` assembles what
//! its arm needs and nothing more. Every one of them is a qw or core type — that
//! is what "the seam is gone" means concretely.

use std::path::PathBuf;

use quorum_core::effects::Env;
use quorum_core::model::Session;
use quorum_core::paths::QdPaths;

use crate::contract::*;
use crate::lane::{Harness, Lane, Mode};
use crate::launch::RenderMode;
use crate::mux::Mux;

// ===========================================================================
// Injected seam
// ===========================================================================

/// Where to attach a terminal, once a revive has produced one.
///
/// Pure data — attach coordinates and nothing else. Moved here from the binary
/// because all three pane revives return it and it has no CLI content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviveHandle {
    pub socket_dir: PathBuf,
    pub zmx_name: String,
}

/// What one carrier call answers with.
///
/// Introduced in the qd binary by stage-2 phase 2 (lifting `blocked::DELIVER`'s
/// original blocker, "the existing send fns return a bare i32 exit code, not a
/// message id") and moved here when [`LaneOps::deliver`] became its second
/// caller. ONE definition, not two — the alternative was a qw twin of the
/// binary's struct converted at the seam, which is a drift bug waiting for its
/// first divergent field.
///
/// `code` is the carrier's UNCHANGED exit code — `deliver_then_stamp` still reads
/// exactly that, so the disposition ledger's bytes do not move.
///
/// **One namespace, not two.** For the PTY carrier it is
/// [`crate::events::mint_send_id`]'s `"{pid}-{epoch_ms}-{n}"`; for the relay
/// carrier it is the relay's own `message_id`. They look nothing alike, and it
/// does not matter: both are written into the SAME field of the SAME record —
/// `Payload::SendInitiated.send_id` in the recipient's
/// `<state>/sessions/<uuid>.events.jsonl` (`verbs/send.rs`'s emit and
/// `emit_relay_send_events` respectively) — and that field is the join key for
/// the whole terminal apparatus ([`crate::sendpty::watch_terminal`],
/// [`crate::events::recovery_read`], the `TERMINAL_EVENTS` first-terminal-wins
/// rule). So the id is OPAQUE and carrier-agnostic by construction, exactly as
/// [`MessageId`] requires.
///
/// `message_id` is OPTIONAL because both carriers have refusals that fire BEFORE
/// any id exists — the relay's pre-inject door failures, and the PTY path's cold
/// / no-mux-pane / attended-zmx / unresolvable-transcript refusals, all of which
/// return before `mint_send_id`. Those are exit codes with nothing to key on, and
/// saying so is more honest than minting a placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarrierOutcome {
    pub code: i32,
    pub message_id: Option<String>,
}

impl CarrierOutcome {
    /// The carrier minted an id for this delivery. Says NOTHING about the exit
    /// code: a failed delivery that got as far as minting an id is still keyed,
    /// and that is the point — a later `recover` needs an id to search for.
    pub fn keyed(code: i32, message_id: String) -> Self {
        CarrierOutcome {
            code,
            message_id: Some(message_id),
        }
    }

    /// A refusal that fired BEFORE any id was minted, so there is nothing to key
    /// on. Never an id-less success — every arm that reaches this is a nonzero
    /// exit.
    pub fn unkeyed(code: i32) -> Self {
        CarrierOutcome {
            code,
            message_id: None,
        }
    }

    // There WAS a third constructor here — `not_yet_widened`, for "a carrier whose
    // entry point has not been widened yet". Stage-2 phase 4 widened the last three
    // (`send_relay::run_{codex,acp,pi}_send`, plus pi's floor sub-lane), so nothing
    // constructs it and the distinction it drew — "there is no id" vs "the id was
    // not plumbed" — no longer has a second side. It is DELETED rather than left
    // unused: a constructor that answers `message_id: None` for a carrier that does
    // mint one is a lie waiting for a caller.
}

// There WAS a `pub trait Carriers` here — the delivery CALLBACK. It carried five
// methods and it existed because the carrier BODIES lived in the `qd` binary's
// verb functions: `LaneOps::deliver` reached UP into `qd` to actually send. That
// edge pointed the wrong way — `deliver` cannot run inside a `qw` process while
// its bodies are `qd` verb functions — and it stranded sixteen qw-owned event
// emitters inside qd's verbs, which independently blocked the ledger split.
//
// Phase 3B moved all five bodies into `crate::delivery` and `deliver` now calls
// them directly, so the trait, its `lane_ops_with_carriers` constructor and the
// `LaneImpl::carriers` field are DELETED rather than left one-method-wide. The
// last of the five, `mux_pty`, is `crate::delivery::pty::deliver_mux_pty` — cut
// at the `wait`/`raw`/`full` boundary, because ~1100 lines with 25 return sites
// and mid-line status writes could not be cut at the function boundary without
// rewriting a control flow the tests pin. See that module.
//
// ONE CARRIER, THREE LANES. The carrier is not the lane: the pane carrier serves
// claude/pane, codex/pane and pi/pane alike. Which one a lane reaches is the
// lane's own private choice — the claim `LaneOps::deliver` rests on.

// ===========================================================================
// qw-native helpers
// ===========================================================================

/// Build the backend-selected mux. The qw half of the binary's `real_mux`.
///
/// `mux_selector` was already pure and already here; the binary's version added
/// only a `HOME` lookup and two `eprintln!`s. Errors are returned, not printed.
pub fn build_real_mux(env: &dyn Env) -> Result<Box<dyn Mux>, String> {
    let home = env
        .var("HOME")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "HOME is not set — cannot resolve the session state dir.".to_string())?;
    let home = PathBuf::from(home);
    let backend = crate::mux_selector::parse_backend(env).map_err(|e| e.message)?;
    crate::mux_selector::select_mux(backend, &home, env).map_err(|e| e.message)
}

/// Exact, id-keyed session lookup — the cheap counterpart to qd's fuzzy resolver.
///
/// Finds the registry row whose `session_id` matches exactly — LIVE or TOMBSTONED,
/// live winning — then joins the two things the registry does not carry: the pane
/// coordinates (`zmx_name`, `socket_dir`) off the mux, and the transcript
/// (`jsonl_path`) off the row's own provider. All three halves are qw's own data.
///
/// The joins are not decoration. `jsonl_path` is the input to `resume_acp`'s
/// resumability gate and `zmx_name` is what tells [`LaneOps::attach`] a pane
/// exists; a field left `None` here is not a missing nicety but a refusal
/// downstream. See the bodies of both joins for the defect each one closed.
///
/// Deliberately NOT qd's `resolve_session_uncapped`. That one resolves a HUMAN
/// query — fuzzy name/prefix matching, liveness-aware disambiguation, an
/// ambiguity-refusal listing — and to do it runs a full cross-backend gather over
/// mux panes, the process table and relay probes, plus an idstore fold. A
/// [`SessionId`] is definitionally unambiguous, so none of that applies. Fuzzy
/// addressing stays qd's job and feeds a concrete id in here.
pub fn row_for_id(
    paths: &QdPaths,
    env: &dyn Env,
    mux: Option<&dyn Mux>,
    id: &SessionId,
) -> Option<Session> {
    // TOMBSTONES COUNT, and reading past them was a real refusal. `qd stop`
    // RENAMES the row to `<pid>.json.tombstoned` rather than deleting it, and BOTH
    // verbs that address a stopped session deliberately accept that state: resume's
    // daemon arms say so in as many words ("a daemon-hosted row is revivable from
    // any non-alive state, incl. a tombstoned stop"), and `qd stop` is idempotent by
    // ruling (`tests/resolve_beyond_cap.rs::stop_on_already_tombstoned_is_graceful`).
    // With `include_tombstoned = false` this lookup answered `NotFound` for exactly
    // the rows those two verbs exist to act on. `qd attach` never noticed, because
    // `reject_if_tombstoned` refuses a stopped session in the verb, before the lane
    // is ever reached.
    //
    // THE LIVE ROW WINS when both exist. A tombstone is history and a live record is
    // current truth, and the two can carry one session id at once (a revive writes a
    // fresh `<pid>.json` and the old row's tombstone stays on disk until swept). Only
    // the live one may be stamped `LiveRegistry` — hence the sort rather than a
    // second scan. `min_by_key` over `tombstoned` (`false < true`) takes it, and
    // returns the FIRST minimum, so two live rows for one id still resolve the row
    // this function has always resolved (the genuine id collision is refused by the
    // verbs' own raw-registry preflight, which is where that decision belongs).
    let scanned = crate::registry::read_entries(&paths.sessions_dir, true)
        .into_iter()
        .filter(|s| s.entry.session_id.as_deref() == Some(id.as_str()))
        .min_by_key(|s| s.tombstoned)?;
    let tombstoned = scanned.tombstoned;
    let e = scanned.entry;

    // The transcript, JOINED — through the provider seam, the same way the gather's
    // per-row loop joins it (`provider_gather`: `transcript_path` over that
    // provider's own `transcript_root`). It was `None`, unconditionally, and that
    // was not a harmless omission: [`LaneOps::wake`]'s acp arm passes `has_jsonl:
    // s.jsonl_path.is_some()` into `resume_acp`, whose resumability gate is
    // `session_id.is_empty() || (provider == "acp/claude-code" && !has_jsonl)`. A
    // constant `None` therefore made EVERY `acp/claude-code` revive through this
    // lane fail `NoResumableTranscript`, for a session whose transcript was sitting
    // on disk the whole time — latent only because no verb routes here yet, and a
    // shipped regression the moment `qd resume` does.
    //
    // Per-row provider dispatch with an unknown provider degrading to the claude
    // derivation is the gather's rule, kept verbatim: a row whose provider string qd
    // cannot place must still resolve. codex is NOT skipped here the way the gather
    // skips it — the gather skips it because `gather_codex` resolves every codex
    // rollout in one pass and doing it twice would pay for the sqlite lookup twice;
    // this is ONE exact row with no second source, so its rollout is resolved here.
    let provider_id = e.provider.as_deref().unwrap_or("claude-code");
    let prov = crate::provider::provider_for(provider_id).unwrap_or_else(|| {
        crate::provider::provider_for("claude-code").expect("claude-code is always registered")
    });
    let fx = crate::provider_gather::root_fx(env, paths);
    let jsonl_path = prov
        .transcript_path(
            &prov.transcript_root(&fx),
            &crate::provider::SessionKey {
                id: id.as_str(),
                name: e.name.as_deref(),
                cwd: e.cwd.as_deref(),
                pid: e.pid,
            },
        )
        .map(|p| p.to_string_lossy().into_owned());

    let mut session = Session {
        name: e.name.clone(),
        user_named: None,
        session_id: id.0.clone(),
        code: None,
        qd_id: None,
        pid: e.pid,
        status: e
            .status
            .as_deref()
            .and_then(quorum_core::model::SessionStatus::parse)
            .unwrap_or(quorum_core::model::SessionStatus::Idle),
        zmx_name: None,
        zmx_clients: None,
        socket_dir: None,
        relay_port: None,
        turns: 0,
        tokens: 0,
        cwd: e.cwd.clone(),
        last_active_ms: e.updated_at,
        version: e.version.clone(),
        started_at_ms: e.started_at,
        git_branch: None,
        jsonl_path,
        last_turns: None,
        provider: e
            .provider
            .clone()
            .unwrap_or_else(|| "claude-code".to_string()),
        entrypoint: e.entrypoint.clone(),
        lineage: None,
        hosting: e.hosting.clone(),
        // A REGISTRY branch is the HONEST stamp here, not a conservative placeholder
        // — and the distinction is load-bearing, because `kill.rs` reads this field
        // (and only this field) to decide [`crate::kill::PidProvenance`], which
        // decides whether the recorded pid must pass the r8 identity check before
        // any witness is derived from it, and whether a subtree sweep may be rooted
        // on it.
        //
        // WHICH registry branch is now read off the FILE the row came out of rather
        // than assumed: `<pid>.json` is `LiveRegistry`, `<pid>.json.tombstoned` is
        // `Tombstoned`. That is a new distinction only because the lookup above did
        // not use to see the second kind of file at all; it changes NOTHING about
        // provenance, since `kill.rs` maps both to `PidProvenance::Registry` — a
        // tombstoned row's pid is a persisted historical claim exactly as a live
        // row's is. Stamping a tombstone `LiveRegistry` would be a false statement
        // of fact about a record that says, on its face, that qd stopped this
        // session.
        //
        // `PaneDerived` means "the pid came from THIS invocation's pane scan"
        // (`ColdJsonl` adoption / `ZmxOnly`) — same-scan pane truth, so pid-vs-pane
        // consistency is tautological and the foreign gate is skipped. Nothing this
        // function does can produce such a pid. It reads the registry and ONLY the
        // registry: the pid above is `e.pid`, read back from a persisted record,
        // which is the definition of `Registry` provenance. The mux join below
        // takes the pane's NAME and SOCKET DIR and never its pid, so no pane-sourced
        // pid ever enters this row.
        //
        // Stamping `ColdJsonl`/`ZmxOnly` to unlock the sweep would therefore be
        // exactly the forgery the gate exists to stop: it would exempt a persisted
        // pid — the one with a real reuse window — from the identity check, so a
        // reused pid now held by a stranger would root a sweep over the STRANGER's
        // children, and every per-victim recheck would legitimately pass. Withheld
        // witnesses leak grandchildren; invented ones kill bystanders.
        //
        // The consequence is real but narrow, and it is a consequence of the
        // LOOKUP, not of this line: a row this function cannot see (a transcript
        // with no registry record — the genuine `ColdJsonl` case) is answered
        // `NotFound`, never mis-branched. There is no reachable row here whose
        // provenance is pane-derived, so there is nothing to derive differently.
        //
        // THAT ARGUMENT SURVIVES THE TOMBSTONE WIDENING, and it is worth saying why
        // rather than assuming it. The argument was always "honest GIVEN WHAT THIS
        // FUNCTION READS", and what it reads has changed — by exactly one more kind
        // of REGISTRY FILE. Both kinds carry a persisted `pid` field and neither is
        // sourced from a pane scan, so the widening adds rows to the `Registry`
        // half and none at all to the `PaneDerived` half. The `ColdJsonl`/`ZmxOnly`
        // set stays precisely the set this function still cannot see, which is what
        // makes it safe for a caller to keep treating a `NotFound` here as the
        // pane-derived case.
        which_branch: if tombstoned {
            quorum_core::model::SessionBranch::Tombstoned
        } else {
            quorum_core::model::SessionBranch::LiveRegistry
        },
    };

    // Pane coordinates live in the mux, not the registry. Match on the session
    // name, which is what the pane is named after.
    //
    // Over the dirs the SELECTED BACKEND uses, canonical FIRST. This used to be
    // `resolve_zmx_dir(&RealEnv)` unconditionally, and both halves of that were
    // wrong:
    //
    //   - The DEFAULT backend (`QD_MUX` unset ⇒ embedded) keeps its panes in the
    //     qrmux dir, not the zmx one. Listing the zmx dir found nothing, so
    //     `zmx_name` stayed `None` and [`LaneOps::attach`] answered
    //     [`LaneError::Cold`] for a session that was in fact LIVE — the default
    //     configuration, every time. The zmx lane lost its legacy dirs too (the
    //     Bug-D cross-dir scan every verb-path join does), so a session created
    //     before a `TMPDIR` change was equally invisible.
    //   - `RealEnv` was constructed INLINE, in a function whose caller already
    //     holds an injected [`Env`]. That reads the real process env out from
    //     under a jailed test and violates the discipline `contract.rs` opens
    //     with: effects live behind `&self`, resolved from the injected `Env`,
    //     never from the process.
    //
    // The dir set is `mux_selector::mux_dirs_from_env` — the SAME resolution qd's
    // `build_mux_dirs` feeds the gather, off the SAME single `QD_MUX` parse that
    // selected the `mux` passed in here. A dir set that disagreed with the mux
    // that scans it is the Bug-D class this pairing exists to prevent.
    //
    // A dir that cannot be listed is SKIPPED, not fatal: the legacy tail is
    // routinely unreadable (another uid's `/tmp` scatter), and the canonical dir
    // is what carries the answer.
    if let (Some(mux), Some(name)) = (mux, e.name.as_deref()) {
        if let Ok(dirs) = crate::mux_selector::mux_dirs_from_env(&paths.home, env) {
            for dir in dirs.ordered() {
                let Ok(panes) = mux.list(&dir) else { continue };
                if let Some(p) = panes.into_iter().find(|p| p.name == name) {
                    session.socket_dir = p
                        .socket_dir
                        .clone()
                        .or_else(|| Some(dir.to_string_lossy().into_owned()));
                    session.zmx_name = Some(p.name);
                    break;
                }
            }
        }
    }
    Some(session)
}

// ===========================================================================
// The lanes
// ===========================================================================

/// One lane implementation. The lane it serves is data, so the seven share one
/// type rather than seven near-identical ones.
pub struct LaneImpl<'a> {
    lane: Lane,
    paths: QdPaths,
    env: &'a dyn Env,
    /// Built once at construction — effects behind `&self`, per the contract.
    /// `None` when the backend could not be resolved; the error surfaces at the
    /// method that needs it rather than failing every lane lookup.
    mux: Option<Box<dyn Mux>>,
}

/// Resolve a lane to its implementation. THE ONLY CONSTRUCTOR.
///
/// There was a second one — `lane_ops_with_carriers` — because [`LaneOps::deliver`]
/// could not run without the `Carriers` callback into the `qd` binary, and every
/// other method could. Phase 3B deleted the callback, so `deliver` is available
/// from here like the other eight and the two-constructor split has nothing left
/// to express.
pub fn lane_ops<'a>(lane: Lane, env: &'a dyn Env, paths: QdPaths) -> LaneImpl<'a> {
    let mux = build_real_mux(env).ok();
    LaneImpl {
        lane,
        paths,
        env,
        mux,
    }
}

impl LaneImpl<'_> {
    pub fn lane(&self) -> Lane {
        self.lane
    }

    /// The PANE carrier — claude/pane's relay-less arm, codex/pane and pi/pane.
    ///
    /// A thin binding of [`crate::delivery::pty::deliver_mux_pty`] to this lane's
    /// injected [`Env`] plus real time. The clock and sleeper are built here
    /// rather than held on `LaneImpl` because no other method needs them, and a
    /// field that exists for one arm is a field every constructor pays for — the
    /// mistake the deleted `carriers` field made.
    fn deliver_mux_pty(&self, session: &Session, text: &str, send_id: &str) -> CarrierOutcome {
        crate::delivery::pty::deliver_mux_pty(
            &crate::delivery::pty::PtyDeps {
                env: self.env,
                clock: &crate::effects::RealClock,
                sleeper: &crate::boot::RealSleeper,
            },
            session,
            text,
            send_id,
        )
    }

    fn row(&self, id: &SessionId) -> Result<Session, LaneError> {
        row_for_id(&self.paths, self.env, self.mux.as_deref(), id)
            .ok_or_else(|| LaneError::NotFound { id: id.clone() })
    }

    /// Re-resolve by STABLE id after a revive reported success (new pid/endpoint,
    /// same id). A row that vanished after a "successful" revive is itself a
    /// wake failure.
    fn refreshed(&self, id: &SessionId) -> Result<SessionHandle, LaneError> {
        let s = row_for_id(&self.paths, self.env, self.mux.as_deref(), id).ok_or_else(|| {
            LaneError::WakeFailed {
                detail: format!("revived {:?} but its session row vanished before use", id.0),
                // No revive core produced this one — it is the lane's own
                // post-condition — so it carries the verb precedent (exit 1)
                // rather than a core's mapping, and the caller stamps its verb
                // on the body the way it does for every other revive failure.
                exit_code: 1,
                self_attributed: false,
            }
        })?;
        Ok(SessionHandle {
            // A revive KEEPS its id — that is what makes it a revive. The
            // Option exists for `start`, where two lanes have no provider id
            // yet; see [`SessionHandle::id`].
            id: Some(id.clone()),
            // No mint happens on a revive.
            qd_id: None,
            pid: s.pid,
            started_at_ms: s.started_at_ms,
            // Both are `start`'s to fill. A revive reports what it produced on
            // [`WakeOutcome::pane`] / [`WakeOutcome::resident`] instead, which is
            // the richer answer for a pane that was just rebuilt.
            socket_dir: None,
            notes: Vec::new(),
        })
    }

    /// A revive reported failure — the CORE's own message, carried out
    /// UNCHANGED. This is why the revives had to gain typed errors before the
    /// lane could exist: the pre-split lane could only report the wrapper's exit
    /// code ("exit 1"), which told a caller nothing about what went wrong.
    ///
    /// **It adds no attribution and no framing of its own**, and that is the
    /// whole content of this function. It used to answer `could not revive
    /// {what} session {id}: {why}` with `{why}` itself stamped `qd wake:` — a
    /// command no user can type, wrapped in a sentence no user asked for. Both
    /// halves belonged to a verb, and the two flags below are how a verb gets
    /// them back: see [`LaneError::WakeFailed`].
    ///
    /// `exit_code` and `self_attributed` are parameters rather than derived
    /// because they must be read off the TYPED error before its message is
    /// formatted (formatting consumes it), and because only the arm knows which
    /// core it called.
    fn wake_failed(&self, detail: String, exit_code: i32, self_attributed: bool) -> LaneError {
        LaneError::WakeFailed {
            detail,
            exit_code,
            self_attributed,
        }
    }

    /// A PANE wake succeeded: the handle, plus the pane the revive actually
    /// built. The pane name is the revive's, not the row's — see
    /// [`PaneHandle::zmx_name`].
    fn woke_pane(&self, id: &SessionId, pane: ReviveHandle) -> Result<WakeOutcome, LaneError> {
        Ok(WakeOutcome {
            // A pane revive relaunches unconditionally — there is no
            // already-running arm for it to take.
            state: WakeState::Revived,
            handle: self.refreshed(id)?,
            resident: None,
            pane: Some(PaneHandle {
                zmx_name: pane.zmx_name,
                socket_dir: pane.socket_dir.to_string_lossy().into_owned(),
            }),
        })
    }

    /// A DAEMON wake succeeded. `state` and `resident` are the revive core's OWN
    /// answers — the verdict it reached at the instant it made the decision, and
    /// the pid/endpoint it produced. Neither is re-read off the row: see
    /// [`WakeState`] and [`Resident`].
    fn woke_daemon(
        &self,
        id: &SessionId,
        state: WakeState,
        resident: Option<Resident>,
    ) -> Result<WakeOutcome, LaneError> {
        Ok(WakeOutcome {
            state,
            handle: self.refreshed(id)?,
            resident,
            pane: None,
        })
    }

    /// The ids store, resolved the way every verb resolves it: `QD_HOME`-honoring
    /// off the injected home (L9a — never the real homedir directly).
    fn ids_path(&self) -> PathBuf {
        let paths = QdPaths::from_home_env(&self.paths.home, self.env);
        crate::idstore::ids_path(&paths.state_dir)
    }

    /// The mux backend + its canonical and legacy socket dirs — the create/revive
    /// lane's dir geometry (Bug D keystone, L1: canonical is where a session is
    /// created AND reaped; legacy dirs are scanned so a socket-dir split is
    /// detectable). Embedded resolves a single dir with legacy EMPTY.
    fn socket_dirs(&self) -> Result<(crate::mux_selector::Backend, PathBuf, Vec<PathBuf>), String> {
        self.socket_dirs_selected().map_err(|e| e.message)
    }

    /// [`LaneImpl::socket_dirs`], keeping the selector's own EXIT CODE.
    ///
    /// The create arms need it and the revive arms do not, which is why there are
    /// two spellings rather than one. `QD_MUX` naming a backend that does not
    /// exist is `mux_selector::QD_MUX_INVALID_EXIT` — **2**, deliberately distinct
    /// from every other create failure's 1 — and `qd start` has answered 2 for it
    /// since the selector existed. Collapsing it into the `String` above is how a
    /// user-visible exit code goes missing when a call site moves behind a trait.
    fn socket_dirs_selected(
        &self,
    ) -> Result<
        (crate::mux_selector::Backend, PathBuf, Vec<PathBuf>),
        crate::mux_selector::SelectorError,
    > {
        let backend = crate::mux_selector::parse_backend(self.env)?;
        // ONE resolution, shared with the gather and with [`row_for_id`]'s mux
        // join. This arm-for-arm computation used to be spelled out here as a
        // third copy of it; a create/revive lane that resolved these differently
        // from the lane that SCANS them is the Bug-D class (L1: canonical is where
        // a session is created AND reaped).
        let dirs = crate::mux_selector::resolve_mux_dirs(backend, &self.paths.home, self.env)
            .map_err(|message| crate::mux_selector::SelectorError {
                message,
                // Not a selector REFUSAL — the value parsed and the dir could not
                // be resolved — so it keeps the ordinary create exit code.
                exit_code: 1,
            })?;
        Ok((backend, dirs.canonical().to_path_buf(), dirs.legacy()))
    }

    /// The shared mux-pane lane effects, for the codex and pi TUI revives.
    fn pane_deps<'d>(
        &'d self,
        exec: &'d dyn crate::exec::Exec,
        clock: &'d dyn crate::effects::Clock,
        backend: crate::mux_selector::Backend,
        canonical_dir: PathBuf,
        legacy_dirs: Vec<PathBuf>,
    ) -> Result<crate::provider::pane::PaneDeps<'d>, LaneError> {
        let mux = self.mux.as_deref().ok_or_else(|| LaneError::Transport {
            detail: "could not resolve the mux backend".to_string(),
        })?;
        Ok(crate::provider::pane::PaneDeps {
            env: self.env,
            exec,
            clock,
            paths: &self.paths,
            mux,
            backend,
            canonical_dir,
            legacy_dirs,
            ids_path: self.ids_path(),
        })
    }

    /// The row's CURRENT endpoint, re-read by pid. It is not on the [`Session`]
    /// surface, and all three daemon revives need it as their alive-check input.
    fn current_endpoint(&self, pid: Option<i64>) -> Option<String> {
        pid.filter(|&p| p != 0)
            .and_then(|p| crate::registry::read_entry(&self.paths.sessions_dir, p))
            .and_then(|e| e.endpoint)
            .filter(|s| !s.is_empty())
    }

    /// The daemon pid a kill can address. `None` for a row with no pid (or `0`) —
    /// a daemon-hosted row whose resident pid is gone from the record has nothing
    /// to reap AND nothing to key a tombstone by, which is why the verb's three
    /// daemon arms all refuse it up front with "Nothing to kill."
    fn daemon_pid(&self, s: &Session) -> Option<i64> {
        s.pid.filter(|&p| p > 0)
    }

    /// The row's relay port, or an honest refusal to say.
    ///
    /// **[`row_for_id`] cannot supply this, and the reason matters.** No registry
    /// row records a relay port — the field does not exist on `RegistryEntry` —
    /// so `row_for_id` leaves `relay_port: None` unconditionally. The port is
    /// DISCOVERED: read the sidecars in `<.claude>/relay` (else probe), then walk
    /// the process ancestry up from each relay's pid and take the mapping onto
    /// this row's pid. That is `join.rs`'s own derivation, and both halves of it
    /// ([`crate::relay::get_relay_ports`], [`crate::relay::match_by_ancestry`])
    /// are already here.
    ///
    /// Delivering on the row's `None` instead would be a SILENT CARRIER
    /// DOWNGRADE: relay precedence is structural — a recorded port selects relay
    /// before mux state is considered — so a claude session with a live relay AND
    /// a joined pane would quietly be typed at through the PTY instead.
    ///
    /// Done HERE rather than in [`row_for_id`] because it costs a `ps`, and
    /// `wake`/`kill`/`attach` have no use for it. Delivery is the one caller that
    /// needs the answer, so delivery is where the cost is paid.
    ///
    /// **A refused `ps` is [`LaneError::Transport`], never `None`.** An empty
    /// ancestry map matches no relay, so treating a failed read as "no relay"
    /// asserts an absence that was never observed — `dispatch::discovery`'s whole
    /// war story, and qd's `refused{receive-path-undetermined}` refusal says the
    /// same thing at its own layer: this is not the same as having no receive
    /// path.
    fn relay_port_for(&self, s: &Session) -> Result<Option<u16>, LaneError> {
        let Some(pid) = s.pid.and_then(|p| i32::try_from(p).ok()) else {
            return Ok(None);
        };
        let probe = crate::provider::claude::relay_http::HttpRelayProbe::default();
        let relays = crate::relay::get_relay_ports(&self.paths.relay_dir, &probe);
        if relays.is_empty() {
            // No relay anywhere: a positive observation, and one that needs no
            // process table to establish. Skipping the `ps` here is not an
            // optimisation — it keeps a denied read from manufacturing a refusal
            // on a host that genuinely runs no relay.
            return Ok(None);
        }
        let pt = quorum_core::effects::RealProcessTable::new(crate::exec::RealExec);
        let ppid_map = quorum_core::effects::ProcessTable::ppid_map(&pt).map_err(|e| {
            LaneError::Transport {
                detail: format!(
                    "the process read that would have found a relay for {:?} was refused ({e}), \
                     so relay presence is UNKNOWN — this is not the same as having no relay",
                    s.session_id
                ),
            }
        })?;
        Ok(crate::relay::match_by_ancestry(&relays, &ppid_map)
            .get(&pid)
            .copied())
    }

    /// The daemon log root, resolved off the injected home so a jailed HOME puts
    /// the log inside the jail (L9a).
    fn log_dir(&self) -> PathBuf {
        self.paths.home.join(".quorum").join("dispatch").join("log")
    }

    /// The O_EXCL name-claim dir: `<.claude>/claims`, alongside `sessions/`.
    /// Derived from `sessions_dir`'s parent so the claim shares the registry's
    /// state root under a jailed HOME.
    fn claims_dir(&self) -> PathBuf {
        self.paths
            .sessions_dir
            .parent()
            .map(|p| p.join("claims"))
            .unwrap_or_else(|| self.paths.home.join(".claude").join("claims"))
    }

    // --- the terminal half's shared coordinates --------------------------

    /// The recipient's row, read OPPORTUNISTICALLY for the two fields the
    /// terminal half wants: its display `name` (the ledger's second key — a
    /// send made before a session id existed is filed under
    /// [`crate::events::byname_key`], and [`crate::events::ReaderCtx`] merges
    /// both files) and its recorded `cwd` (claude's transcript resolver's cheap
    /// tier).
    ///
    /// **A missing row is not an error here, and that is the whole point.**
    /// [`LaneOps::recover`] exists for sends whose writer is GONE; by then the
    /// janitor may well have reaped the registry row, and refusing to search
    /// because the row is missing would fail exactly the case the method was
    /// built for. The events ledger is keyed by the session id the caller passed,
    /// which is all either method structurally needs. The mux is not consulted —
    /// pane coordinates have no bearing on a ledger read, and asking for them
    /// would cost a backend resolution per call.
    fn ledger_row(&self, id: &SessionId) -> (Option<String>, Option<String>) {
        match row_for_id(&self.paths, self.env, None, id) {
            Some(s) => (s.name, s.cwd),
            None => (None, None),
        }
    }

    /// Everything [`LaneOps::attach`] decides BEFORE the handoff: whether this
    /// lane can attach at all, and where its pane is.
    ///
    /// # Why this is split out
    ///
    /// After the binary cut, `attach` is an **exec** — `qd` runs `qw attach` with
    /// inherited stdio, because the controlling terminal cannot be serialized (see
    /// [`LaneOps::attach`]). But an exec has only an exit code to come back with,
    /// and two of `attach`'s refusals are **data qd acts on rather than prints**:
    ///
    /// - [`LaneError::NotSupported`] — `qd attach` renders a redirect telling the
    ///   user to drive a daemon-hosted session with `qd send:relay` instead.
    /// - [`LaneError::Cold`] — `qd attach` runs the revive machinery and retries.
    ///
    /// Collapsing those into "qw printed something and exited 1" loses both
    /// behaviours, which is exactly what `tests/attach_verb.rs`'s
    /// `attach_codex_without_endpoint_is_daemon_redirect` and
    /// `attach_resolves_auto_named_cold_session` caught when the exec was wired
    /// without them.
    ///
    /// So the REFUSAL crosses as data and only the HANDOFF is an exec. The wire
    /// calls this first; on `Ok` it execs, and on `Err` it returns the typed error
    /// to the verb, which behaves exactly as it did in-process.
    ///
    /// The check-then-exec gap is real but harmless here: a session that goes cold
    /// in between makes the exec fail and `qw` report it, and unlike `deliver`
    /// there is no ledger entry to get wrong. `deliver` may NOT be split this way —
    /// see [`LaneOps::deliver`] on why its wake/steer decision has to stay atomic.
    pub fn attach_target(&self, id: &SessionId) -> Result<(String, PathBuf), LaneError> {
        // `codex/app-server` is the one daemon lane with an answer here, and it
        // is a DIFFERENT answer: not "this session's terminal" (it has none) but
        // "the pane a viewer on it would live in". Side-effect-free, like every
        // other precheck — the pane is created by `attach` itself, not here, so a
        // wire `attach_precheck` never spawns a TUI.
        if self.lane.is_app_server() {
            return self.viewer_target(id);
        }
        if self.lane.is_daemon() {
            return Err(LaneError::NotSupported {
                op: "attach".to_string(),
                reason: "a daemon-hosted session has no terminal of its own; drive it with send"
                    .to_string(),
            });
        }
        let s = self.row(id)?;
        let zmx_name = s
            .zmx_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| LaneError::Cold { id: id.clone() })?
            .to_string();
        let dir = s
            .socket_dir
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| quorum_core::zmx_dir::resolve_zmx_dir(self.env));
        Ok((zmx_name, dir))
    }

    /// Where a viewer on this `codex/app-server` session lives — `(pane name,
    /// socket dir)`. PURE: it reads the row and resolves the mux dirs, and
    /// creates nothing.
    ///
    /// The three refusals are the three facts a viewer needs, and each is a
    /// [`LaneError::Cold`] rather than a `NotSupported`, because none of them is
    /// structural — they are all "not yet" on a row that could acquire them.
    fn viewer_target(&self, id: &SessionId) -> Result<(String, PathBuf), LaneError> {
        let s = self.row(id)?;

        // ── WHICH REFUSAL, AND WHY IT IS NOT A COSMETIC CHOICE ──────────────
        //
        // `LaneError::Cold` is not merely "this did not work" — it is a PROTOCOL
        // SIGNAL to qd meaning **a wake would fix this**. `verbs/attach.rs` maps
        // it to `wake_then_attach`: revive the session, then retry the handoff.
        //
        // So `Cold` is correct for exactly one of the conditions below (a dead
        // app server — reviving respawns it and the retry succeeds) and WRONG for
        // the others, where no wake can help. Getting this backwards is not a
        // wording bug; it was observed as:
        //
        //   $ qd attach 5cfpdqzw
        //   Revived "test-37347"; attaching...
        //   qd attach: session 01a01638-… is not live
        //
        // — a pointless revive of a session that was already running, followed by
        // "is not live" about a live session, because the retry hit the same
        // refusal. The remedy the user actually needed (`qd send`) was never
        // mentioned. `Refused` carries its own sentence and qd prints it verbatim.
        let refused = |detail: String| LaneError::Refused { detail };

        // A viewer's pane is named after the SESSION, so a nameless row has
        // nowhere to put one. A wake does not mint a name.
        let name = s.name.as_deref().filter(|n| !n.is_empty()).ok_or_else(|| {
            refused(format!(
                "session {} has no name, and a viewer pane is named after its session",
                id.0
            ))
        })?;
        // No thread id ⇒ nothing for `codex resume <id>` to select. A wake cannot
        // invent one either — codex's own revive answers `NoThreadId` here.
        if s.session_id.is_empty() {
            return Err(refused(format!(
                "session {name:?} has no codex thread id yet, so there is nothing for \
                 a viewer to resume"
            )));
        }
        // THE TURN-ZERO REFUSAL, and it is not a formality — it was observed.
        //
        // The viewer's argv ends `resume <thread-id>`, and codex's `thread/resume`
        // needs a ROLLOUT to resume FROM. A thread created but never driven has
        // none, and the TUI does not degrade: it boots, fails, and leaves a dead
        // pane on screen —
        //
        //   Error: Failed to resume session from …/rollout-…jsonl:
        //   thread/resume failed during TUI bootstrap: no rollout found for
        //   thread id … (code -32600)
        //
        // Refusing HERE means `attach` and `attach_precheck` both answer before
        // anything is spawned, so the failure is a sentence instead of a corpse.
        // The same edge `ResumeError::NoRollout` names on the revive path,
        // reached from the other direction.
        //
        // **`Refused`, not `Cold`.** A wake is precisely the wrong remedy: the
        // daemon is already up, and reviving it produces no rollout. One turn
        // does, so the sentence names that instead. Checked BEFORE the endpoint
        // so a fresh session gets the actionable answer rather than being sent
        // round the revive loop.
        if self.codex_rollout_path(&s).is_none() {
            return Err(refused(format!(
                "session {name:?} has not taken a turn yet, so codex has no rollout \
                 for a viewer to resume — send it a message first (qd send {name} …), \
                 then attach"
            )));
        }
        // No LIVE app server to point `--remote` at. **This one IS `Cold`**, and
        // deliberately: the viewer is a second client on a RUNNING server, a dead
        // server is exactly what a wake repairs, and qd's revive-and-retry then
        // lands the attach on the respawned endpoint.
        //
        // It is a LIVENESS check, not an "is the field populated" check, and that
        // distinction was found the hard way. `current_endpoint` reads the string
        // off the registry row, and nothing rewrites that row when the daemon
        // dies — so a killed app server still answers `Some(ws://…)` and the
        // viewer would spawn against a dead socket, fail with `Error: failed to
        // connect to remote app server`, and leave exactly the dead pane the
        // turn-zero guard above exists to prevent.
        //
        // So this asks the same identity-checked question `kill_codex` and the
        // resume path's AlreadyRunning gate ask: pid alive AND its live cmdline is
        // OUR codex daemon on THIS endpoint. The cmdline arm is what stops a
        // reused pid — live, but some unrelated process — from reading as a
        // healthy server.
        if !self.app_server_is_live(&s) {
            return Err(LaneError::Cold { id: id.clone() });
        }
        let (_, canonical, _) = self
            .socket_dirs()
            .map_err(|detail| LaneError::Transport { detail })?;
        Ok((
            crate::provider::codex::pane::viewer_pane_name(name),
            canonical,
        ))
    }

    /// Is THIS row's codex app server actually running right now?
    ///
    /// Identity-checked liveness, the same question `kill_codex` asks before it
    /// signals a group: the recorded pid is alive AND its live command line is our
    /// codex daemon carrying the recorded `--listen <endpoint>`. The cmdline arm
    /// is not belt-and-braces — under exact-pid-reuse the recorded pid can belong
    /// to an unrelated process, and treating that as a healthy app server would
    /// point a viewer at a socket nobody is serving.
    fn app_server_is_live(&self, s: &Session) -> bool {
        let Some(endpoint) = self.current_endpoint(s.pid) else {
            return false;
        };
        let Some(pid) = s.pid.filter(|&p| p > 0) else {
            return false;
        };
        quorum_core::effects::is_pid_alive(pid as i32)
            && crate::create_daemon::cmdline_is_our_daemon(
                crate::create_daemon::real_cmdline_probe(pid).as_deref(),
                Some(endpoint.as_str()),
            )
    }

    /// This codex row's rollout file, if one exists yet.
    ///
    /// The SAME resolution `crate::lane_read`'s `codex_status` uses — sqlite
    /// `rollout_path` first, date-walk second — because a viewer and a health
    /// read must agree about whether this thread has history. `None` means the
    /// thread has taken no turn.
    fn codex_rollout_path(&self, s: &Session) -> Option<PathBuf> {
        use crate::provider::codex::CodexProvider;
        use crate::provider::{Provider, SessionKey};

        if s.session_id.is_empty() {
            return None;
        }
        let fx = crate::provider_gather::root_fx(self.env, &self.paths);
        let root = CodexProvider.transcript_root(&fx);
        CodexProvider.transcript_path(
            &root,
            &SessionKey {
                id: &s.session_id,
                name: s.name.as_deref(),
                cwd: s.cwd.as_deref(),
                pid: s.pid,
            },
        )
    }

    /// Attach a human TUI to a LIVE `codex/app-server` session, without stopping
    /// or converting it.
    ///
    /// THE MECHANISM (verified live against codex-cli 0.147.0). The codex TUI is
    /// itself an app-server client — `codex --remote <ws-url>` points it at an
    /// EXISTING app server instead of bootstrapping its own. This lane already
    /// spawns exactly such a server per session and records its address in the
    /// row's `endpoint` (`ws://127.0.0.1:<port>`, the form `--remote` accepts). So
    /// the human's TUI and the agent's RPC client become two clients of ONE app
    /// server, driving ONE thread.
    ///
    /// WHY THIS BEATS STOP-AND-CONVERT. The obvious way to give a human a terminal
    /// on an agent's session is to stop the daemon and relaunch the thread as a
    /// TUI pane. That costs the agent its session and permanently changes the
    /// row's topology — a debugging action with side effects on the thing being
    /// debugged. Here nothing stops, nothing converts, and the agent keeps driving
    /// throughout.
    ///
    /// THE VIEWER IS NOT A SESSION. It gets a mux pane (so it can be detached and
    /// re-attached, over SSH or from a phone, like any other qrmux pane) but NO
    /// registry row: it owns no thread, has no identity, and its death means
    /// nothing. A second attach reuses a live viewer rather than stacking another.
    ///
    /// # Provenance
    ///
    /// Moved verbatim from the qd binary's `verbs::lifecycle::attach_codex_viewer`,
    /// which reached it through a codex-shaped special case in `qd attach` because
    /// `LaneOps::attach` refused every daemon lane (ruling J). Giving the topology
    /// its own lane is what let the special case become an ordinary arm. Its
    /// sibling half, [`crate::provider::codex::resume::reap_viewer_pane`], made
    /// this trip one stage earlier — a viewer is a codex affordance and both
    /// halves belong on the same side of the boundary.
    fn attach_codex_viewer(&self, id: &SessionId) -> Result<i32, LaneError> {
        let s = self.row(id)?;
        let (pane, dir) = self.viewer_target(id)?;
        // Unwraps are discharged by `viewer_target`, which refused all three of
        // these above.
        let endpoint = self
            .current_endpoint(s.pid)
            .ok_or_else(|| LaneError::Cold { id: id.clone() })?;
        let mux = self.mux.as_deref().ok_or_else(|| LaneError::Transport {
            detail: "could not resolve the mux backend".to_string(),
        })?;

        // Reuse a live viewer: attaching twice should land in the same window,
        // not stack a second TUI on the same thread.
        //
        // Identity is by NAME, with a guard — and it has to be, because the
        // embedded mux cannot tell us more. qrmux's `SessionInfo` carries no
        // command line, so `MuxSession::cmd` is synthesized EMPTY under the
        // default backend; a "does this pane run our argv?" check would be dead
        // code that silently never matched (and it did: it re-created every time,
        // then failed on the taken name).
        //
        // THE GUARD closes the case a name check alone would get wrong. Nothing
        // but this function creates a `<name>.view` pane, but a user COULD have
        // started a real session literally called that — and handing them its
        // terminal when they asked for a viewer on `<name>` would be a silent
        // wrong-window. A real session has a live REGISTRY ROW; a viewer never
        // does. So: pane present + no row claiming that name ⇒ ours.
        let pane_present = mux
            .list(&dir)
            .unwrap_or_default()
            .into_iter()
            .any(|z| z.name == pane);
        let claimed_by_a_real_session =
            crate::registry::read_entries(&self.paths.sessions_dir, false)
                .into_iter()
                .any(|e| {
                    !e.tombstoned
                        && e.entry.name.as_deref() == Some(pane.as_str())
                        && e.entry
                            .pid
                            .is_some_and(|p| p != 0 && quorum_core::effects::is_pid_alive(p as i32))
                });
        if pane_present && claimed_by_a_real_session {
            return Err(LaneError::Refused {
                detail: format!(
                    "cannot open a viewer on {:?} — a live session is already named \
                     {pane:?}, which is the name a viewer needs. Rename or stop it, \
                     or attach to {pane:?} directly if that is what you meant.",
                    s.name.as_deref().unwrap_or_default()
                ),
            });
        }

        if !pane_present {
            // argv = `codex --remote <ws endpoint> resume <thread-id>`.
            // `--remote` binds the TUI to OUR app server; `resume <id>` selects
            // the agent's thread on it (an explicit UUID bypasses codex's session
            // picker, which by default HIDES non-interactive sessions — exactly
            // the kind an agent creates).
            let argv = vec![
                crate::provider::codex::codex_bin(self.env),
                "--remote".to_string(),
                endpoint,
                "resume".to_string(),
                s.session_id.clone(),
            ];
            let cmd = crate::launch::build_claude_cmd_from_argv(&argv);
            let cwd = s
                .cwd
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            match mux.run_detached(&dir, &pane, &cmd, &cwd) {
                Ok(res) if res.status == Some(0) => {}
                Ok(res) => {
                    return Err(LaneError::Transport {
                        detail: format!(
                            "could not open a viewer on {pane:?} (exit {:?}): {}",
                            res.status,
                            res.stderr.trim()
                        ),
                    })
                }
                Err(e) => {
                    return Err(LaneError::Transport {
                        detail: format!("could not open a viewer on {pane:?}: {e}"),
                    })
                }
            }
        }

        // The handoff. `attach` is reached only through `qw attach`'s argv entry
        // (`bin/qw.rs`), where stdout is a terminal rather than the protocol — so
        // unlike everything else in this file, a line here would be safe. There
        // still is not one: the pane about to take the screen says more than any
        // sentence could.
        mux.attach(&dir, &pane).map_err(|e| LaneError::Transport {
            detail: format!("attach to viewer pane {pane:?} failed: {e}"),
        })
    }

    /// This lane's recovery/await effects. See [`LaneRecoveryDeps`] for why
    /// `resolve_transcript` being per-lane is the fix rather than a detail.
    fn recovery_deps<'d>(
        &'d self,
        cwd: Option<String>,
        clock: &dyn crate::effects::Clock,
    ) -> LaneRecoveryDeps<'d> {
        LaneRecoveryDeps {
            lane: self.lane,
            env: self.env,
            paths: &self.paths,
            cwd,
            now_ms: clock.now_ms(),
        }
    }

    // --- the seven create arms ------------------------------------------
    //
    // One method per lane, so [`LaneOps::start`]'s body is nothing but the
    // routing table. Six of the seven are pure DELEGATION to a core that was
    // already in this crate; only the claude one assembles effects, and only
    // because its effect assembly is what was inline in the verb.
    //
    // What NONE of them do, stated once because the omissions are deliberate and
    // each has an owner on qd's side: the clap parse and the `claudeArgs`
    // forbidden-flag chokepoint; the parked `--port`/`--attach` refusals;
    // `--fork` target resolution and the transcript seed (they need qd's fuzzy
    // resolver); the driver auto-detect and its `StartRoute::Headless` refusal
    // (`crate::driver` reads WHO IS DRIVING — a qd-binary fact, never a lane
    // concern); all `--json` emission; the bind phase (`dispatch::bindphase`);
    // the relay-presence warning; telemetry; and the claude lane's post-boot `-p`
    // delivery with its `map_deliver_outcome` exit mapping.

    /// `claude-code/mux-pane`. The ONE arm with real work, and the work is the
    /// EFFECT ASSEMBLY rather than the create: [`crate::create::run_new`] has
    /// been in this crate all along, and what was inline in the verb was the
    /// backend/dirs resolution, the mux, the id mint, and the [`ProviderFx`] the
    /// boot waiter is built from.
    ///
    /// [`ProviderFx`]: crate::provider::ProviderFx
    fn start_claude_pane(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        refuse_create_prompt(req, CLAUDE_PROMPT_IS_QDS)?;

        let clock = crate::effects::RealClock;
        let sleeper = crate::boot::RealSleeper;
        let exec = crate::exec::RealExec;
        let (backend, canonical, legacy) = self.socket_dirs_selected().map_err(selector_failed)?;
        let mux = self
            .mux
            .as_deref()
            .ok_or_else(|| start_failed("qd start: could not resolve the mux backend".to_string()))?;

        // The id must exist at env-bake time, so it is minted BEFORE the launch
        // and fail-closed: never boot a session whose env would silently miss its
        // identity. A `resume` binds it to the id it was handed (the row and the
        // env agree from the first instant); a fresh start mints UNBOUND, because
        // the provider uuid does not exist yet — which is the same fact
        // [`SessionHandle::id`] reports back as `None`.
        let ids_path = self.ids_path();
        let minted = match req.resume.as_ref() {
            Some(id) => {
                crate::idstore::mint_or_get(&ids_path, id.as_str(), Some(&req.name), &clock)
            }
            None => crate::idstore::mint_unbound(&ids_path, Some(&req.name), &clock),
        };
        let qd_session_id = minted.map_err(|e| {
            start_failed(format!(
                "qd start: could not mint a stable session id: {e}. No session was created."
            ))
        })?;

        let provider_impl = crate::provider::provider_for(Harness::ClaudeCode.provider_id())
            .ok_or_else(|| {
                start_failed("qd start: the claude-code provider is not registered".to_string())
            })?;
        // The boot waiter comes THROUGH the provider seam, exactly as the verb
        // obtained it. `await_relay` is `Some(...)` unconditionally now: the
        // request always carries the caller's resolved decision, so the claude
        // waiter never falls back to its legacy `QD_BOOT_AWAIT_RELAY` opt-in —
        // which is the behaviour change the missing field would have caused.
        let boot_fx = crate::provider::ProviderFx {
            await_relay: Some(req.await_relay),
            env: self.env,
            paths: &self.paths,
            socket_dir: canonical.clone(),
            mux: Some(mux),
            clock: Some(&clock),
            sleeper: Some(&sleeper),
            relay: None,
            relay_port: None,
            app_server: None,
            codex_expected_turn_id: None,
            acp_client: None,
            pi_rpc: None,
            acp_pre_dispatch: None,
        };
        let boot_waiter = provider_impl.boot_waiter(&boot_fx);

        let deps = crate::create::NewDeps {
            mux,
            exec: &exec,
            env: self.env,
            clock: &clock,
            paths: &self.paths,
            canonical_dir: canonical.clone(),
            legacy_dirs: legacy,
            boot_waiter: boot_waiter.as_ref(),
            provider: provider_impl,
            backend,
        };
        let params = crate::create::NewParams {
            name: req.name.clone(),
            // `qd start --agent` is RETIRED (it refuses before preflight), so the
            // request carries no agent and there is nothing to pass. Absent from
            // the contract rather than present-and-always-None.
            agent: None,
            // `--fork` is qd's: resolving the target needs its fuzzy resolver, and
            // the seeded transcript is then resumed by a PLAIN `--resume` at the
            // pre-minted fork uuid. So a fork reaches this lane as `resume`, and
            // `fork: false` is what the verb passes for both cases.
            fork: false,
            resume: req.resume.as_ref().map(|id| id.0.clone()),
            claude_args: req.passthrough.clone(),
            model: req.model.clone(),
            cwd: req.cwd.clone(),
            backend_env: req.env.clone(),
            backend_env_unset: req.env_unset.clone(),
            qd_session_id: Some(qd_session_id.clone()),
            render: req.render,
            // The claude native-TUI create path is the only shape claude has, and
            // `ClaudeProvider::launch_plan` does not read this field — the
            // assembled cmd is byte-identical either way. It matters for codex,
            // whose TUI and app-server lanes are different argv.
            interactive: true,
            // claude's pane launch has no control channel.
            control_socket: None,
        };
        let out = crate::create::run_new(&deps, &params).map_err(|e| {
            // A BOOT TIMEOUT leaves the session RUNNING (its own `Display` says
            // so), so the pre-minted id gets a best-effort bind before we report
            // the failure — otherwise a late-booting session's env id would never
            // match what `qd ls` surfaces.
            //
            // It runs HERE rather than in the verb because it is the MINTER's
            // repair: this arm owns `ids_path` and `qd_session_id`, and neither
            // exists on the error a caller receives. It is silent by contract
            // (every failure is swallowed) — the loud boot error owns stderr and
            // stays byte-stable — which is why a lane may do it at all.
            if matches!(e, crate::create::NewError::BootTimeout { .. }) {
                self.bind_minted_id_best_effort(&ids_path, &qd_session_id, &req.name, &clock);
            }
            new_error(e)
        })?;

        // Claude's uuid still does not exist at the instant the create returns —
        // that part of the old comment here was right, and a placeholder would
        // still be an identity nobody observed. What was wrong was stopping there:
        // the row lands a poll later, and reading it back is what makes the handle
        // addressable at all. See [`Self::bind_and_resolve_after_create`].
        let (id, pid) = match self.bind_and_resolve_after_create(
            &ids_path,
            &qd_session_id,
            &req.name,
            &clock,
            &sleeper,
        ) {
            Some((sid, pid)) => (Some(sid), pid),
            // Unresolved within the budget: the session IS up, so this stays a
            // success and degrades to the pre-existing answer. qd's bind phase
            // owns the loud report.
            None => (None, None),
        };

        Ok(SessionHandle {
            id,
            qd_id: Some(qd_session_id),
            pid,
            started_at_ms: None,
            // The dir the create LANDED in, not a re-resolution of it — see the
            // field. `qd start -p` types the priming turn into this pane.
            socket_dir: Some(out.socket_dir),
            notes: Vec::new(),
        })
    }

    /// Best-effort bind of a pre-minted unbound id after a boot timeout.
    ///
    /// Moved here verbatim from `verbs/lifecycle.rs`, where it was the only piece
    /// of the claude create's failure handling that needed facts the verb no
    /// longer has. One non-blocking registry read by name through the SAME
    /// liveness-filtered pick the boot-confirm site uses; no-row and ambiguous
    /// both mean "don't bind", silently, by this path's contract.
    fn bind_minted_id_best_effort(
        &self,
        ids_path: &std::path::Path,
        qd_session_id: &str,
        name: &str,
        clock: &dyn crate::effects::Clock,
    ) {
        let rows = crate::registry::read_entries(&self.paths.sessions_dir, false);
        let alive = |pid: i64| crate::effects::is_pid_alive(pid as i32);
        if let crate::registry::LiveNamePick::One { session_id: sid } =
            crate::registry::pick_live_named_row(&rows, name, &alive)
        {
            let _ = crate::idstore::bind(ids_path, qd_session_id, &sid, clock);
        }
    }

    /// Complete the identity after a SUCCESSFUL create: bind the pre-minted id to
    /// the row claude writes, and hand back the uuid (and pid) that bind observed.
    ///
    /// The sibling of [`Self::bind_minted_id_best_effort`], which is the SINGLE-PASS
    /// repair on the boot-timeout path. This one polls, because the two paths race
    /// different things: on a timeout the row either exists or the session is
    /// wedged, while on a success claude is mid-write — the boot confirm observes
    /// the pid file, and `<pid>.json` lands a beat later.
    ///
    /// WHY it has to happen here at all: `row_for_id` keys strictly on the registry
    /// `sessionId`, so the provider uuid is the ONLY key that addresses a session.
    /// Returning `id: None` (the shape this replaces) left a `qw`-only caller with
    /// a `qd_id` and a name, neither of which is a key, and therefore no way to
    /// `deliver` to the session it had just created. `qd` did not see it because
    /// `dispatch::bindphase` fills the same gap one layer up — which is exactly why
    /// the lane could not keep relying on it.
    ///
    /// BEST-EFFORT by contract, like its sibling: `start` has already SUCCEEDED
    /// when this runs, so a miss degrades to the previous `id: None` answer rather
    /// than failing a session that is up. The loud four-arm ruling on an unbindable
    /// row (ambiguous, diverged, budget-exhausted) stays `dispatch::bindphase`'s to
    /// report; a bind made here reaches it as `BindOutcome::AlreadyBoundSameId`,
    /// which that phase already treats as success, so qd's exit codes and its
    /// `--json` identity object do not move.
    fn bind_and_resolve_after_create(
        &self,
        ids_path: &std::path::Path,
        qd_session_id: &str,
        name: &str,
        clock: &dyn crate::effects::Clock,
        sleeper: &dyn crate::boot::Sleeper,
    ) -> Option<(SessionId, Option<i64>)> {
        let poll_ms = crate::boot::BootTimeouts::default().poll_ms;
        let deadline = clock.now_ms() + BIND_AFTER_CREATE_BUDGET_MS;
        let alive = |pid: i64| crate::effects::is_pid_alive(pid as i32);
        loop {
            // Check-before-sleep, so a row already on disk costs no wait — the
            // ordinary case once the boot waiter has confirmed idle.
            let rows = crate::registry::read_entries(&self.paths.sessions_dir, false);
            if let crate::registry::LiveNamePick::One { session_id: sid } =
                crate::registry::pick_live_named_row(&rows, name, &alive)
            {
                // The bind is what makes the qd id resolvable LATER; the uuid is
                // what makes this handle addressable NOW. They fail independently,
                // so a bind that cannot be made still yields the identity we
                // observed rather than discarding it.
                let _ = crate::idstore::bind(ids_path, qd_session_id, &sid, clock);
                let pid = rows
                    .iter()
                    .find(|s| {
                        !s.tombstoned && s.entry.session_id.as_deref() == Some(sid.as_str())
                    })
                    .and_then(|s| s.entry.pid);
                return Some((SessionId(sid), pid));
            }
            if clock.now_ms() + poll_ms as i64 > deadline {
                return None;
            }
            sleeper.sleep_ms(poll_ms);
        }
    }

    /// `codex/mux-pane` — straight delegation to the shared pane choreography.
    fn start_codex_pane(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        use crate::provider::codex::pane::{create_codex_tui, CodexTuiParams};
        refuse_create_prompt(req, CODEX_TUI_HAS_NO_VERIFIABLE_SUBMIT)?;

        let exec = crate::exec::RealExec;
        let clock = crate::effects::RealClock;
        let (backend, canonical, legacy) = self.socket_dirs_selected().map_err(selector_failed)?;
        let deps = self.pane_deps(&exec, &clock, backend, canonical, legacy)?;
        let out = create_codex_tui(
            &deps,
            &CodexTuiParams {
                name: req.name.clone(),
                cwd: req.cwd.clone(),
                render: req.render,
                resume_thread: req.resume.as_ref().map(|id| id.0.clone()),
            },
        )
        .map_err(codex_tui_error)?;
        Ok(SessionHandle {
            // `None` on a FRESH start, by design rather than by timing: a bare
            // `codex` opens its thread when it feels like it, and the rollout
            // backfill identifies it afterwards. Pinned by
            // `tests/codex_interactive_lane.rs`
            // `interactive_codex_starts_unidentified_then_binds_its_thread`.
            id: out.thread_id.map(SessionId),
            qd_id: Some(out.qd_session_id),
            pid: None,
            started_at_ms: None,
            socket_dir: Some(out.socket_dir),
            notes: Vec::new(),
        })
    }

    /// `pi/mux-pane` — the TWO-PHASE delegation, and the order is the point.
    ///
    /// `plan_pi_tui` runs FIRST, before any dep resolution, because its refusals
    /// — the `pi --session-id` capability preflight above all — must be what the
    /// caller hears about even when `QD_MUX` is also wrong. That interleave is
    /// only expressible at a call site, which is why the core exposes two phases
    /// instead of one call. [`LaneOps::wake`]'s pi arm holds the same order.
    fn start_pi_pane(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        use crate::provider::pi::pane::{create_pi_tui, plan_pi_tui, PiTuiParams};
        refuse_create_prompt(req, PI_TUI_WRITES_NOTHING_UNTIL_ITS_FIRST_REPLY)?;

        let plan = plan_pi_tui(
            self.env,
            &PiTuiParams {
                name: req.name.clone(),
                cwd: req.cwd.clone(),
                render: req.render,
                session_id: req.resume.as_ref().map(|id| id.0.clone()),
            },
        )
        .map_err(pi_tui_error)?;

        let exec = crate::exec::RealExec;
        let clock = crate::effects::RealClock;
        let (backend, canonical, legacy) = self.socket_dirs_selected().map_err(selector_failed)?;
        let deps = self.pane_deps(&exec, &clock, backend, canonical, legacy)?;
        let out = create_pi_tui(&deps, &plan).map_err(pi_tui_error)?;
        Ok(SessionHandle {
            // ALWAYS present — a pi row is identified from birth on both the
            // fresh and the revive lane, which is exactly what the codex outcome
            // above cannot claim.
            id: Some(SessionId(out.session_id)),
            qd_id: Some(out.qd_session_id),
            pid: None,
            started_at_ms: None,
            socket_dir: Some(out.socket_dir),
            notes: Vec::new(),
        })
    }

    /// `pi/extension` — the pi TUI pane, launched with the `quorum-lane`
    /// control channel bound.
    ///
    /// Delegates to [`crate::provider::pi::extension::create_extension_session`],
    /// which is [`create_pi_tui`](crate::provider::pi::pane::create_pi_tui) plus
    /// a socket flag and two row fields. The two-phase order is the pi-pane
    /// order and for the pi-pane reason: `plan_extension_launch` carries the
    /// `--session-id` capability refusal (and now the extension install), and
    /// those must be what the caller hears about even when `QD_MUX` is also
    /// wrong.
    ///
    /// # This is the one pane lane that honours a create-time prompt
    ///
    /// `pi/mux-pane` refuses one, and the reason is real:
    /// [`PI_TUI_WRITES_NOTHING_UNTIL_ITS_FIRST_REPLY`] — a fresh pi session
    /// writes nothing to disk until its first assistant reply, so a PTY-typed
    /// first turn cannot be confirmed to have landed, and the lane refuses
    /// rather than accept a prompt it might drop.
    ///
    /// That reasoning is about the TRANSCRIPT, and this lane does not use the
    /// transcript. `deliver` here is an acknowledged request to pi's own
    /// `sendUserMessage`: it either returns `accepted` or it returns an error
    /// frame. The lazy-write window is irrelevant to a channel that never reads
    /// the file. So the refusal is lifted for this lane alone, and
    /// [`create_prompt_refusal`] records that as data.
    ///
    /// **A failed first turn does not fail the create**, matching the daemon
    /// lanes' in-core contract: the session exists, is addressable, and is
    /// attachable, and destroying it because one message did not land would be a
    /// worse answer than reporting the miss as a note.
    fn start_pi_extension(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        use crate::provider::pi::extension::{
            create_extension_session, plan_extension_launch, Client,
        };
        use crate::provider::pi::pane::PiTuiParams;

        let launch = plan_extension_launch(
            self.env,
            &PiTuiParams {
                name: req.name.clone(),
                cwd: req.cwd.clone(),
                render: req.render,
                session_id: req.resume.as_ref().map(|id| id.0.clone()),
            },
        )
        .map_err(pi_tui_error)?;

        let exec = crate::exec::RealExec;
        let clock = crate::effects::RealClock;
        let (backend, canonical, legacy) = self.socket_dirs_selected().map_err(selector_failed)?;
        let deps = self.pane_deps(&exec, &clock, backend, canonical, legacy)?;
        let out = create_extension_session(&deps, &launch).map_err(pi_tui_error)?;

        // Readiness is the HANDSHAKE, not the pane. `create_extension_session`
        // returns once the pane is up and verified attachable, which says
        // nothing about whether pi finished booting and the extension bound its
        // socket — on a cold jiti cache that can take tens of seconds. A caller
        // that got a handle here and immediately delivered would race the
        // channel it was handed.
        let mut notes = Vec::new();
        match Client::wait_ready(&launch.socket, crate::provider::pi::extension::client::BOOT_TIMEOUT)
        {
            Ok((mut client, _hello)) => {
                if let Some(prompt) = req.prompt.as_deref().filter(|p| !p.is_empty()) {
                    if let Err(e) = client.deliver(prompt, None) {
                        notes.push(format!("the create-time prompt was not delivered: {e}"));
                    }
                }
            }
            Err(e) => {
                // NOT fatal. The pane is real and a human can attach to it and
                // type; what is missing is the agent's half. Reporting that as a
                // note beats destroying a working session, and beats claiming a
                // channel that is not there.
                notes.push(format!(
                    "the session is up but its control channel did not answer: {e}"
                ));
                if req.prompt.as_deref().is_some_and(|p| !p.is_empty()) {
                    notes.push(
                        "the create-time prompt was not delivered — type it after `qd attach`"
                            .to_string(),
                    );
                }
            }
        }

        Ok(SessionHandle {
            id: Some(SessionId(out.session_id)),
            qd_id: Some(out.qd_session_id),
            pid: None,
            started_at_ms: None,
            socket_dir: Some(out.socket_dir),
            notes,
        })
    }

    /// `codex/daemon` — the app-server residence. Straight delegation to
    /// [`crate::create_daemon::run_new_daemon`], which owns the whole
    /// choreography (version sniff → name-claim → alloc/spawn/connect ladder →
    /// initialize → thread/start → row → optional first turn).
    ///
    /// This is one of the three lanes that deliver the create-time prompt
    /// IN-CORE, so [`StartRequest::prompt`] is passed through rather than
    /// refused. Its failure is NON-fatal inside the core, by that core's contract.
    ///
    /// `hosting` is the token stamped on the row: `Some("daemon")` for this
    /// lane, `Some("app-server")` for `codex/app-server`. It is the ONLY
    /// difference between the two — see
    /// [`crate::create_daemon::DaemonParams::hosting`].
    ///
    /// **Both callers stamp; neither passes `None`.** `codex/daemon` used to,
    /// relying on `Harness::row_default_mode` to answer `Daemon` for a row with
    /// no `hosting` field — true then, true now, and a dependency on a default
    /// living in a file this one never mentions. Stamping makes the row's lane
    /// DATA rather than an inference, which is what stops a future change to any
    /// default from silently relabelling sessions that already exist.
    fn start_codex_daemon(
        &self,
        req: &StartRequest,
        hosting: Option<&str>,
    ) -> Result<SessionHandle, LaneError> {
        use crate::create_daemon::{
            real_alloc_port, run_new_daemon, DaemonDeps, DaemonParams, RealDaemonSpawner,
        };
        use crate::provider::codex::{AppServerRpc, RpcError, WsAppServer};

        let exec = crate::exec::RealExec;
        let clock = crate::effects::RealClock;
        let spawner = RealDaemonSpawner;
        // The connector: a real ws client to the recorded endpoint, boxed as the
        // injected seam so the core drives it without holding a transport type.
        // The connect timeout floor matches the core's connect-retry granularity.
        let connect = |url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
            WsAppServer::connect(url, std::time::Duration::from_secs(5)).map(|c| {
                let boxed: Box<dyn AppServerRpc> = Box::new(c);
                boxed
            })
        };
        let alloc = real_alloc_port;
        let provider_impl = crate::provider::provider_for(Harness::Codex.provider_id())
            .ok_or_else(|| {
                start_failed("qd start: the codex provider is not registered".to_string())
            })?;

        let deps = DaemonDeps {
            provider: provider_impl,
            env: self.env,
            exec: &exec,
            clock: &clock,
            sessions_dir: self.paths.sessions_dir.clone(),
            claims_dir: self.claims_dir(),
            log_dir: self.log_dir(),
            spawner: &spawner,
            connect: &connect,
            alloc_port: &alloc,
            ids_path: self.ids_path(),
        };
        let params = DaemonParams {
            name: req.name.clone(),
            cwd: req.cwd.clone(),
            // `--agent` is retired; see the claude arm.
            agent: None,
            passthrough: req.passthrough.clone(),
            prompt: req.prompt.clone(),
            hosting: hosting.map(str::to_string),
        };
        let out = run_new_daemon(&deps, &params).map_err(daemon_error)?;
        Ok(SessionHandle {
            id: Some(SessionId(out.thread_id)),
            qd_id: Some(out.qd_session_id),
            pid: Some(out.pid),
            started_at_ms: None,
            // A resident has no pane, so no socket dir. Not a gap — an answer.
            socket_dir: None,
            notes: Vec::new(),
        })
    }

    /// `codex/app-server` — the SAME residence as `codex/daemon`, stamped so it
    /// comes back as its own lane.
    ///
    /// # Why this arm is three lines and not a second pipeline
    ///
    /// The two lanes spawn the identical process (`codex app-server --listen
    /// ws://127.0.0.1:<port>`), run the identical choreography, and produce the
    /// identical thread. They are not two topologies — they are one topology
    /// asked two different questions, and the second question is "may a human
    /// open a terminal onto this?". A separate create pipeline would be a copy of
    /// [`Self::start_codex_daemon`] free to drift from it, which is the shape
    /// `01-coupling-census.md` was written about.
    ///
    /// What the stamp buys is [`LaneImpl::attach_target`]: a row carrying
    /// `hosting: "app-server"` re-derives to THIS lane, and this lane is the one
    /// whose attach opens a `codex --remote <endpoint>` viewer instead of
    /// answering `NotSupported`.
    fn start_codex_app_server(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        self.start_codex_daemon(req, Some(crate::lane::Mode::AppServer.hosting_token()))
    }

    /// `pi/daemon` — the dispatch-OWNED stdio resident. pi has no `--listen`, so
    /// its residence is `<exe> pi-daemon`, not the codex app-server; that is why
    /// it has its own core rather than sharing the arm above.
    fn start_pi_daemon(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        use crate::provider::pi::daemon::{create_pi_session, PiCreateDeps, PiCreateParams};
        refuse_create_prompt(req, PI_DAEMON_CREATE_IS_TURN_FREE)?;

        let exe = create_exe("pi")?;
        let clock = crate::effects::RealClock;
        let now_ms = || crate::effects::Clock::now_ms(&clock);
        let spawner = crate::create_daemon::RealDaemonSpawner;

        // Identity parity with the acp/codex arms, and it is a correctness fix
        // rather than a nicety: mint an UNBOUND stable id BEFORE the spawn so the
        // resident carries THIS session's `QD_SESSION_ID` instead of inheriting
        // the commissioner's through the detached spawn's env subtree. pi's own
        // birth-id is not known until after readiness, so `bind()` happens inside
        // the core. Fail-closed: nothing spawns if the mint fails.
        let ids_path = self.ids_path();
        let qd_session_id = crate::idstore::mint_unbound(&ids_path, Some(&req.name), &clock)
            .map_err(|e| {
                start_failed(format!(
                    "qd start: could not mint stable id for pi session: {e}"
                ))
            })?;

        let deps = PiCreateDeps {
            exe,
            // The pinned pi binary (NOT on PATH) + pi's own session storage, off
            // the env SEAM (L9a).
            pi_bin: self.env.var("QD_PI_BIN").filter(|s| !s.is_empty()),
            session_dir: self
                .env
                .var("PI_CODING_AGENT_SESSION_DIR")
                .filter(|s| !s.is_empty()),
            sessions_dir: self.paths.sessions_dir.clone(),
            claims_dir: self.claims_dir(),
            log_dir: self.log_dir(),
            spawner: &spawner,
            now_ms: &now_ms,
            ids_path,
        };
        let params = PiCreateParams {
            name: req.name.clone(),
            cwd: req.cwd.clone(),
            load_session: req.resume.as_ref().map(|id| id.0.clone()),
            qd_session_id: Some(qd_session_id.clone()),
        };
        let out = create_pi_session(&deps, &params).map_err(pi_create_error)?;
        Ok(SessionHandle {
            id: Some(SessionId(out.session_id)),
            qd_id: Some(qd_session_id),
            pid: Some(out.pid),
            started_at_ms: None,
            socket_dir: None,
            notes: Vec::new(),
        })
    }

    /// Both `acp/*` daemon lanes. ONE core serves them, distinguished by the
    /// `provider_id` it is handed — which is a LANE fact here
    /// ([`Harness::provider_id`]), never a string prefix test.
    ///
    /// The id is persisted on the row, so every other verb (kill/wait/resume/
    /// send:relay) re-derives the same bridge from it.
    fn start_acp_daemon(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        use crate::provider::acp::daemon::{
            create_acp_daemon, AcpCreateParams, AcpDaemonDeps, AcpWarning,
        };

        let exe = create_exe("acp")?;
        let clock = crate::effects::RealClock;
        let spawner = crate::create_daemon::RealDaemonSpawner;
        let alloc = crate::create_daemon::real_alloc_port;
        // The create lane never consults the probe (there is no already-alive gate
        // on a create), but the deps struct is shared with the resume lane.
        let probe = crate::create_daemon::real_cmdline_probe;
        // The lane still has no stdout — it COLLECTS the notices instead of
        // printing them, and hands them back on [`SessionHandle::notes`].
        //
        // Dropping them, which is what this closure used to do, was a real loss:
        // `qd start`'s acp arm routes every `AcpWarning` to stderr, and a lane
        // that silently swallowed them would have made those lines disappear the
        // moment the verb started calling `start`. A lane must not print; it may
        // report, and reporting is what [`ReapObservations::notes`] already
        // established as the shape.
        let notes: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let warn = |w: &AcpWarning| notes.borrow_mut().push(w.to_string());

        let deps = AcpDaemonDeps {
            exe,
            home: self.paths.home.clone(),
            paths: &self.paths,
            clock: &clock,
            spawner: &spawner,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            warn: &warn,
        };
        let params = AcpCreateParams {
            name: req.name.clone(),
            provider_id: self.lane.harness.provider_id().to_string(),
            cwd: req.cwd.clone(),
            prompt: req.prompt.clone(),
        };
        let out = create_acp_daemon(&deps, &params).map_err(acp_create_error)?;
        Ok(SessionHandle {
            id: Some(SessionId(out.session_id)),
            qd_id: Some(out.qd_session_id),
            pid: Some(out.pid),
            started_at_ms: None,
            socket_dir: None,
            notes: notes.into_inner(),
        })
    }
}

/// Which carrier a live `claude-code/mux-pane` row takes.
///
/// The PURE half of [`LaneOps::deliver`]'s internal choice, split out because the
/// choice is the whole claim of stage-2 phase 2 and a decider this load-bearing
/// should be testable without a relay, a mux or a session. Conventions: deciders
/// are pure; effects gather the inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeCarrier {
    Relay { port: u16 },
    MuxPty,
    /// Live, but with nothing to receive through — a genuinely bare receive
    /// surface, which is NOT the same as a stopped session.
    NoLiveReceivePath,
}

/// `send_unified::select_carrier`'s `"claude-code"` arm, carried across verbatim
/// when that function still existed. It does not: qd's copy of the routing is
/// retired and this is now the only one.
///
/// **Relay precedence is structural**: a recorded port selects relay BEFORE mux
/// state is considered. PTY can only be selected from a POSITIVE
/// `relay_port: None` observation plus a live joined mux pane — which is why
/// [`LaneImpl::relay_port_for`] refuses rather than answering `None` when the
/// process read that would have found a relay was denied. A `None` that is
/// merely un-observed must never reach this function.
pub fn claude_carrier(relay_port: Option<u16>, joined_pane: bool) -> ClaudeCarrier {
    match (relay_port, joined_pane) {
        (Some(port), _) => ClaudeCarrier::Relay { port },
        (None, true) => ClaudeCarrier::MuxPty,
        (None, false) => ClaudeCarrier::NoLiveReceivePath,
    }
}

/// Whether a pane row has a pane to type into. The pane lanes' half of
/// `send_unified::select_carrier`, which asked exactly this of a codex or pi
/// `--interactive` row before choosing the PTY carrier. That function is retired;
/// this is where the question is asked now.
///
/// Deliberately NOT applied to the three daemon lanes: a resident's receive path
/// is its recorded ws endpoint, and the endpoint is not on [`Session`] at all
/// (`registry::read_entry` by pid is where it lives). Each daemon carrier re-reads
/// it and answers its own "not reachable" refusal, which is why this lane hands
/// them the row unexamined rather than inventing a shallower gate here.
fn joined_pane(s: &Session) -> bool {
    s.zmx_name.is_some() && s.socket_dir.is_some()
}

/// A live row with nothing to receive through — and WHICH error that is depends
/// on whether this delivery revived it.
///
/// A row we just REVIVED that came back bare is a wake that did not produce a
/// deliverable target: qd's retired `wake_then_deliver` called that `failed{wake}`
/// (exit 12) rather than a delivery failure, and so does this — qd's send path now
/// reads the verdict off this error instead of re-running a selector. A row that
/// was bare all along never claimed to be revived, so it is the transport refusal
/// `select_carrier` used to answer with.
fn no_live_receive_path(id: &SessionId, woke: &Confirmation, detail: String) -> LaneError {
    if *woke != Confirmation::No {
        LaneError::WakeFailed {
            detail: format!("revived {:?} but it has no live receive path", id.0),
            // qd's `failed{wake}` is exit 12, and this is that condition
            // reported from the lane. No revive core produced it — the revive
            // SUCCEEDED and the row came back bare — so the code is this
            // function's own, spelled where the sentence above names it.
            exit_code: 12,
            self_attributed: false,
        }
    } else {
        LaneError::Transport { detail }
    }
}

/// Can a session in this state receive a message as it stands?
///
/// The same three statuses `send_unified::is_live` accepts, and the same reading:
/// a NOT-live target is not a refusal class ("stopped is not a refusal class") —
/// it is a WAKE trigger. `Shell` counts as live because a pane at a shell prompt
/// still takes keystrokes.
fn is_deliverable(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Shell
    )
}

// ===========================================================================
// The terminal half: await_terminal and recover
// ===========================================================================

/// The poll cadence [`LaneOps::await_terminal`] hands the ledger watch.
///
/// [`crate::events::AwaitBudget`] is expressed as `(poll_ms, max_polls)` while the
/// contract's budget is a wall-clock millisecond count, so one of the two has to be
/// chosen here. 500ms is [`crate::events::AwaitBudget::default`]'s own cadence —
/// the §8 number the whole apparatus already runs at — so a `budget_ms` of 30_000
/// gives exactly the 60 polls that default describes rather than a new timing
/// regime invented at this boundary.
const AWAIT_POLL_MS: u64 = 500;

/// Budget for the post-create identity resolution
/// ([`LaneImpl::bind_and_resolve_after_create`]).
///
/// SMALL on purpose, and it is not a boot timeout in miniature. The boot waiter
/// has already confirmed the pid file and idle by the time this runs, so the only
/// thing still outstanding is claude's own `<pid>.json` write — observed at well
/// under a second. What the budget bounds is the anomaly (a row that never lands,
/// or one that lands ambiguous), and there the right answer is to stop quickly and
/// let the session be reported as up-but-unbound: `dispatch::bindphase` re-polls
/// the same registry against the real boot budget and owns the loud verdict, so
/// spending that budget twice would only slow every `qd start` down to say the
/// same thing.
const BIND_AFTER_CREATE_BUDGET_MS: i64 = 3_000;

/// The per-lane recovery/await effects: transcript reads, transcript RESOLUTION,
/// the clock, and the poll sleep.
///
/// **`resolve_transcript` is the per-lane half, and closing its defect is why this
/// type exists.** qd's `RealRecoveryDeps` (`verbs/recover.rs`, deleted in stage-3
/// phase 3A) resolved EVERY transcript through `jsonl::find_jsonl_path` off
/// `<home>/.claude/projects` — the claude layout, unconditionally. A codex rollout
/// lives under `$CODEX_HOME/sessions` and a pi session under
/// `PI_CODING_AGENT_SESSION_DIR`; neither is under that root and neither ever will
/// be, so the resolve answered `None`, the window build failed, and the send was
/// reported `SourceUnavailable` — "I looked and could not read it" — when the truth
/// was that nobody had looked anywhere it could be. It is
/// the same bug `provider_gather` already fixed one layer up ("a pi row was being
/// asked to find its session file under claude's tree, where it can never be"), and
/// it is fixed the same way: route through the LANE's provider (`transcript_root`
/// then `transcript_path`) instead of a hard-coded root.
///
/// claude is unmoved by the routing — `ClaudeProvider::transcript_root` IS
/// `paths.projects_dir` and its `transcript_path` delegates to
/// `jsonl::find_jsonl_path` — so the one lane that worked keeps working byte for
/// byte, which is what makes this a fix rather than a swap.
struct LaneRecoveryDeps<'a> {
    lane: Lane,
    env: &'a dyn Env,
    paths: &'a QdPaths,
    /// The row's recorded cwd, when there IS a row. claude's resolver uses it as
    /// its cheap tier before falling back to the registry scan; the others ignore
    /// it. `None` is honest — a recovery can run long after the row was reaped.
    cwd: Option<String>,
    /// Captured ONCE so every send judged in one pass shares a clock, exactly as
    /// `verbs/recover.rs` captures its fence clock once at verb start.
    now_ms: i64,
}

impl crate::events::RecoveryDeps for LaneRecoveryDeps<'_> {
    fn read_transcript(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn resolve_transcript(&self, session_id: Option<&str>, name: Option<&str>) -> Option<String> {
        let sid = session_id?;
        let prov = crate::provider::provider_for(self.lane.harness.provider_id())?;
        let fx = crate::provider_gather::root_fx(self.env, self.paths);
        let key = crate::provider::SessionKey {
            id: sid,
            name,
            cwd: self.cwd.as_deref(),
            pid: None,
        };
        prov.transcript_path(&prov.transcript_root(&fx), &key)
            .map(|p| p.display().to_string())
    }

    fn now_ms(&self) -> i64 {
        self.now_ms
    }
}

impl crate::events::AwaitDeps for LaneRecoveryDeps<'_> {
    fn sleep(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// [`crate::events::Received`] → [`Terminal`]: the §8 await outcomes restated in
/// the contract's vocabulary.
///
/// The two that are NOT collapsed are the point. `AnchorTimeout` is
/// [`Terminal::TimedOut`] and says nothing about delivery; `BudgetExhausted` is
/// [`Terminal::Undetermined`], because a watch that ran out of polls without even
/// emitting its own timeout has LESS to report, not more. Neither becomes
/// [`Terminal::NotDelivered`] — foreclosing a send the ledger never foreclosed is
/// the "the ledger lies" failure this whole apparatus exists to prevent.
fn terminal_from_received(r: crate::events::Received) -> Terminal {
    use crate::events::Received;
    match r {
        Received::Anchored => Terminal::Seen,
        Received::AnchoredMismatch => Terminal::Mismatch,
        Received::AnchorTimeout => Terminal::TimedOut,
        // `pending-abandoned` — the ONE verdict the recovery contract permits to
        // foreclose a send. See [`crate::events::RecoveryVerdict::Abandoned`].
        Received::Abandoned => Terminal::NotDelivered {
            reason: "pending-abandoned".to_string(),
        },
        // §C1: a door failed loudly BEFORE the wire. A failure, never a success —
        // and never a hang, because a `send-failed` IS a terminal and satisfies
        // the await.
        Received::SendFailed { reason } => Terminal::NotDelivered {
            reason: format!("send-failed: {reason}"),
        },
        // §X.3.5 — the post-wire on-received failure. Same shape as the door
        // failure above and for the same reason (a confirmed non-delivery, never
        // an undetermined one), but it keeps its OWN ledger token in `reason`:
        // qd renders that token verbatim, so collapsing the two would print
        // `send-failed` for a send that failed after it reached the wire.
        Received::SeenFailed { reason } => Terminal::NotDelivered {
            reason: format!("seen-failed: {reason}"),
        },
        Received::BudgetExhausted { last_stage } => Terminal::Undetermined {
            reason: format!("the await budget ran out at stage {last_stage:?}"),
        },
    }
}

/// [`crate::events::RecoveryVerdict`] → [`Terminal`]: the R6 recovery-terminus
/// lattice restated in the contract's vocabulary. This mapping is qd's today, and
/// it is the half that crosses WITH the search.
///
/// The lattice's whole point survives the translation — its four epistemically
/// distinct terminus states stay four. `Abandoned` (searched, candidates existed,
/// none matched) and `Unattributable` (no recovery key, so a search can never run)
/// are the two DISCLOSED closers and become [`Terminal::NotDelivered`] carrying the
/// ledger's OWN reason token — the same string `recovery_event` writes into
/// `pending-abandoned{reason}`, not a second vocabulary invented at the boundary.
/// `SourceUnavailable` and `EmptyWindow` wrote NO terminal at all and become
/// [`Terminal::Undetermined`], which is precisely the variant that exists so a
/// still-growable window is never closed out as a non-delivery.
fn terminal_from_verdict(v: &crate::events::RecoveryVerdict) -> Terminal {
    use crate::events::RecoveryVerdict as V;
    match v {
        V::Anchored { .. } => Terminal::Seen,
        V::Truncated { .. } => Terminal::Mismatch,
        V::Abandoned { .. } => Terminal::NotDelivered {
            reason: "recovery-no-candidate".to_string(),
        },
        V::Unattributable => Terminal::NotDelivered {
            reason: "recovery-unattributable".to_string(),
        },
        V::SourceUnavailable => Terminal::Undetermined {
            reason: "source-unavailable".to_string(),
        },
        V::EmptyWindow => Terminal::Undetermined {
            reason: "window-empty".to_string(),
        },
    }
}

/// The pane revives' cwd: the row's recorded dir, or the process cwd. A revive
/// must land in the project it came from — the bridge and the transcript lookup
/// are both cwd-keyed — so a missing record falls back to where we are, never to
/// nothing.
fn revive_cwd(recorded: Option<&str>) -> PathBuf {
    recorded
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// This binary's own path — the pi and acp residents are spawned as `<exe>
/// pi-daemon`/`<exe> acp-daemon`. An unresolvable self-exe means no resident can
/// be launched at all, so it is reported as the wake failure it is.
///
/// `<exe>` is whichever binary is running this code, and since the split that is
/// normally `qw`. BOTH `qd` and `qw` dispatch both verbs pre-clap (ruling D6) —
/// they have to, because this is a re-exec of SELF and this code runs in both.
fn self_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot resolve own executable: {e}"))
}

/// A daemon-hosted row with no addressable pid: nothing was signalled and no
/// tombstone was keyed. `No` on both, and it is a real `No` — not an `Unknown` —
/// because we know exactly what did not happen.
const NOTHING_TO_REAP: KillOutcome = KillOutcome {
    reaped: Confirmation::No,
    tombstoned: Confirmation::No,
};

/// What every daemon arm can honestly claim, and no more.
///
/// `reaped: Unknown` because the pgid ladder
/// ([`crate::create_daemon::DaemonSpawner::kill`]) fires SIGTERM → grace →
/// SIGKILL at the GROUP and then returns: it waits on the group LEADER only, and
/// never re-checks after the SIGKILL, so the other group members were never
/// observed. And on the identity-guard path the delegate declines to signal at
/// all — `teardown_pi_daemon` returns `()` while doing so — which is the case
/// [`Confirmation`] exists for: "already dead" and "a reused pid we refuse to
/// touch" are indistinguishable from here, and one of them is not a reap.
/// Reporting `No` would claim knowledge we do not have; `Yes` would claim more.
///
/// `tombstoned: Unknown` because [`crate::registry::ensure_tombstone`] returns
/// `()` and is best-effort by design (a failed mkdir/write leaves no tombstone
/// rather than aborting the kill) — having CALLED it is not evidence one EXISTS.
const DAEMON_REAPED: KillOutcome = KillOutcome {
    reaped: Confirmation::Unknown,
    tombstoned: Confirmation::Unknown,
};

/// A daemon arm with no pid to address: nothing was signalled, scanned or
/// tombstoned.
///
/// `nothing_to_kill` is the OBSERVATION; [`NOTHING_TO_REAP`] is the claim. They
/// are not the same statement — `reaped: No` is also what a failure clause and a
/// surviving pane answer, and those are three different exit-1 messages.
fn nothing_to_reap() -> KillReport {
    KillReport {
        outcome: NOTHING_TO_REAP,
        observed: ReapObservations {
            nothing_to_kill: true,
            ..Default::default()
        },
    }
}

/// A daemon arm that ran. `was_alive` is the delegate's OWN answer at the
/// instant it decided whether to signal — see [`ReapObservations::was_alive`] for
/// why re-probing it in the caller is both a reimplementation and a race.
fn daemon_reaped(pid: i64, was_alive: bool) -> KillReport {
    KillReport {
        outcome: DAEMON_REAPED,
        observed: ReapObservations {
            pid,
            was_alive: Some(was_alive),
            ..Default::default()
        },
    }
}

// ===========================================================================
// start: the create-time prompt, and how a create failure is classified
// ===========================================================================

/// Why each of the four lanes that cannot drive a create-time turn says so.
///
/// These are the REASONS, not the lines a user reads. The wording qd prints today
/// is unchanged and stays qd's: its wrapper warns and creates the session anyway
/// (`-p` was not delivered; the session IS created), so it prints its own line and
/// calls this lane with `prompt: None`. What changes is that the lane can no
/// longer be handed a prompt it would silently drop — the shape
/// [`LaneError::NotSupported`] exists for.
///
/// The three lanes NOT listed here — `codex/daemon` and both `acp/*` — deliver the
/// first turn inside their own create, so they take the prompt.
/// Why the four non-codex harnesses answer `NotSupported` for
/// [`Mode::AppServer`].
///
/// Unreachable through `Lane::new` / `Lane::from_id` — both refuse the
/// combination — so these arms exist for exactly the reason the `claude/daemon`
/// and `acp/*-pane` ones do: a hand-built `Lane` gets an ANSWER rather than a
/// panic or, worse, another lane's behaviour.
const APP_SERVER_IS_CODEX_ONLY: &str =
    "only codex has an app-server residence a human terminal can join";

const CLAUDE_PROMPT_IS_QDS: &str = "the claude create returns at boot-ready and its -p turn is a \
     POST-boot delivery with its own went-busy exit contract (deliver_prompt + map_deliver_outcome), \
     which is qd start's to run — not part of creating the session";
const CODEX_TUI_HAS_NO_VERIFIABLE_SUBMIT: &str = "a create-time prompt would have to be typed into \
     the TUI's composer, and the codex TUI has no verifiable submit path yet (no pollable busy/idle \
     signal), so claiming it was delivered would be a claim we cannot check";
const PI_TUI_WRITES_NOTHING_UNTIL_ITS_FIRST_REPLY: &str = "pi writes no transcript until its first \
     assistant reply, so a create-time submit cannot be verified — there is nothing on disk to \
     verify a landing against";
const PI_DAEMON_CREATE_IS_TURN_FREE: &str = "a create-time prompt would be a model TURN, and pi \
     tier-a create is credential-free and turn-free by design; drive the turn by sending to the \
     running session";

/// Why `pi/mux-pane` refuses [`LaneOps::await_idle`] instead of waiting.
///
/// # It has no live idle source at all
///
/// A bare pi TUI in a mux pane publishes its busy/idle nowhere: there is no
/// endpoint on the row, no control socket bound, and nothing writes a status
/// string for it. `lane_read::health_for`'s `(Pi, Pane)` arm — the one place
/// that has to answer "is this session busy" for `qd ls` — falls all the way
/// through to `pi_transcript_status` and says so in as many words: the
/// transcript "is a record of what finished", so **`pi/mux-pane` reports `Busy`
/// never**, and `Idle` for a session that may be mid-turn.
///
/// # Which is exactly why the transcript tail was NOT given to `wait`
///
/// A watcher built on that source would read "not busy" at entry every single
/// time and return `IdleAtEntry` unconditionally — `qd wait` would exit 0
/// instantly on a session that is streaming, and `qd wait X && next` would run
/// `next` into a busy agent. That is a FALSE DONE, and `crate::idle` is built
/// around never producing one ("never a false done, never a hang"). A refusal
/// that names the missing capability is strictly better than a wait that always
/// answers yes: the operator learns something true and their script stops.
///
/// A quiet-period heuristic over the transcript ("no new bytes for N seconds")
/// was the other candidate and is rejected for the same reason — silence is not
/// idleness, and pi writes nothing at all until its first assistant reply, so the
/// heuristic's very first observation of a fresh session is indistinguishable
/// from a finished one.
///
/// # `NotSupported`, not `NotImplemented`
///
/// This is an answer, not a debt. The signal does not exist to be read; the lane
/// with the same pi TUI and a channel to ask over it is `pi/extension`, which is
/// what the reason names. Building an idle watcher for `pi/mux-pane` means giving
/// it a channel — at which point it is the extension lane.
///
/// Rendered by `qd wait`'s generic `Err(e)` arm as
/// `qd wait: "<name>": await_idle is not supported by this lane: …`, exit 1.
/// Deliberately not [`LaneError::Transport`]: that arm is worded "session daemon
/// not reachable (try qd resume …)", which would send a user to revive a session
/// that is running fine.
const PI_PANE_HAS_NO_IDLE_SOURCE: &str = "a pi TUI in a mux pane publishes no busy/idle signal \
     anywhere — its transcript records what FINISHED and can never report busy, so any wait built \
     on it would answer \"idle\" for a session mid-turn; start the session with --extension \
     (pi/extension) for a lane with a control channel that can be asked";

/// **Can this lane deliver the first turn at create — and if not, why not?**
///
/// The four constants above, addressed by lane. `None` means the lane's own
/// create drives the prompt in-core (`create_daemon::run_new_daemon`'s step 6 and
/// `create_acp_daemon`'s `drive_create_prompt`); `Some(reason)` is the refusal
/// [`refuse_create_prompt`] answers with.
///
/// **Public because the CALLER has to know the same thing, and there must be one
/// answer.** `qd start -p` does not simply forward the prompt and print whatever
/// comes back: for three of these four lanes it prints its own "the session is
/// created; type the prompt after `qd attach`" notice and creates the session
/// ANYWAY, and for claude it runs the whole post-boot priming send itself. So qd
/// decides, per lane, whether to pass the prompt at all — and a qd that decided
/// that from its own private list would be a second copy of this table, free to
/// drift into either a dropped prompt or a refused create.
pub fn create_prompt_refusal(lane: Lane) -> Option<&'static str> {
    match (lane.harness, lane.mode) {
        (Harness::ClaudeCode, Mode::Pane) => Some(CLAUDE_PROMPT_IS_QDS),
        (Harness::Codex, Mode::Pane) => Some(CODEX_TUI_HAS_NO_VERIFIABLE_SUBMIT),
        (Harness::Pi, Mode::Pane) => Some(PI_TUI_WRITES_NOTHING_UNTIL_ITS_FIRST_REPLY),
        (Harness::Pi, Mode::Daemon) => Some(PI_DAEMON_CREATE_IS_TURN_FREE),
        // `pi/extension` is the ONE pane lane that accepts a create-time prompt.
        // The pane lanes above refuse because they cannot confirm a typed first
        // turn landed; this lane does not type it. Its `deliver` is an
        // acknowledged call to pi's own `sendUserMessage`, so the confirmation
        // problem those refusals exist for does not arise. See
        // `start_pi_extension`.
        (Harness::Pi, Mode::Extension) => None,
        // The lanes that deliver it in-core, plus the combinations that are not
        // lanes at all (they refuse before any of this).
        // `codex/app-server` rides with `codex/daemon`: same core, same in-core
        // first turn, so the same answer.
        (Harness::Codex, Mode::Daemon | Mode::AppServer)
        | (Harness::AcpClaudeCode, Mode::Daemon)
        | (Harness::Opencode, Mode::Daemon)
        | (Harness::ClaudeCode, Mode::Daemon)
        | (Harness::AcpClaudeCode | Harness::Opencode, Mode::Pane)
        | (
            Harness::ClaudeCode | Harness::Codex | Harness::AcpClaudeCode | Harness::Opencode,
            Mode::Extension,
        )
        | (
            Harness::ClaudeCode | Harness::Pi | Harness::AcpClaudeCode | Harness::Opencode,
            Mode::AppServer,
        ) => None,
    }
}

/// The four lanes above, refusing a prompt BEFORE anything is claimed or spawned.
///
/// An EMPTY prompt is accepted as no prompt, matching the `is_some_and(|s|
/// !s.is_empty())` gate every one of those verb wrappers uses today: an empty `-p`
/// is a degenerate no-op turn, not a request the lane must refuse.
///
/// It takes the REASON, not the lane, and that is deliberate: handing every lane a
/// prompt is the side-effect-free probe
/// `a_create_time_prompt_is_refused_by_the_four_lanes_that_cannot_deliver_it` uses
/// to prove WHICH ARM ran. Looking the reason up from `self.lane` instead would
/// make that probe answer from the lane field rather than from the arm, and the
/// mis-swap it exists to catch would go quiet.
/// [`create_prompt_refusal`] is pinned against these four arms by that same test.
fn refuse_create_prompt(req: &StartRequest, reason: &str) -> Result<(), LaneError> {
    match req.prompt.as_deref() {
        Some(p) if !p.is_empty() => Err(LaneError::NotSupported {
            op: "start(prompt)".to_string(),
            reason: reason.to_string(),
        }),
        _ => Ok(()),
    }
}

// --- create failures, classified ------------------------------------------
//
// Every create failure crosses as [`LaneError::StartFailed`], and each match
// below decides the three facts that variant carries and a caller cannot
// re-derive: the EXIT CODE, whether the failure was the BOOT WAITER giving up
// (and in which phase), and whether the `detail` is already a complete line.
//
// It used to be a `Refused` / `Transport` split. That split classified honestly
// and answered a question nobody asked — no caller branched on it, and it could
// not carry an exit code or a boot phase, both of which `qd start` reads. The
// exhaustive matches SURVIVE, because the reason they exist is unchanged: a new
// failure variant has to be classified rather than swept into a catch-all, the
// same reason `start` itself is an exhaustive match on the lane.
//
// The `detail` is the core's OWN message, verbatim. These errors already carry
// the complete user-facing line — they are what `qd start` prints today — so
// re-wording them here would fork the text a user reads from the text the tests
// pin. The ONE exception is spelled out on [`start_line`].

/// Stamp `qd start: ` on a create error whose `Display` is body-only.
///
/// Only [`crate::provider::codex::pane::CodexTuiError`] and its pi twin need it,
/// and only because those two types are SHARED with the revive path: `qd resume`
/// and `qd attach` reach the same cores, so their `Display` deliberately omits the
/// verb and every caller stamps its own.
///
/// A create has exactly one verb. That is what makes stamping it here different
/// from a lane inventing one — the fact `qd start` is the caller is structural,
/// not a guess, and `create::NewError`'s own `Display` has always written it. The
/// self-attributed variants (`…::Create`, which wraps `NewError`) are passed
/// through untouched, exactly as `verbs/lifecycle.rs`'s `codex_tui_failure_line`
/// did before this moved.
fn start_line(self_attributed: bool, body: String) -> String {
    if self_attributed {
        body
    } else {
        format!("qd start: {body}")
    }
}

/// A create failure with the ordinary exit code and no boot phase — every arm's
/// common case.
fn start_failed(detail: String) -> LaneError {
    LaneError::StartFailed {
        detail,
        exit_code: 1,
        boot_phase: None,
    }
}

/// This binary's own path, for the two CREATE arms that spawn a resident from it.
///
/// Separate from [`self_exe`] because of the WORDING, not the lookup: `self_exe`'s
/// message is shared with the revive arms and names no adapter, and `qd start`'s
/// two daemon wrappers have always said which one could not be resolved. Verbatim
/// from those wrappers, raw `io::Error` and all.
fn create_exe(adapter: &str) -> Result<PathBuf, LaneError> {
    std::env::current_exe().map_err(|e| {
        start_failed(format!(
            "qd start: cannot resolve own executable for {adapter} adapter: {e}"
        ))
    })
}

/// The mux backend could not be selected. Carries the selector's OWN exit code —
/// `QD_MUX_INVALID_EXIT` is 2, and `qd start` has answered 2 for a bogus `QD_MUX`
/// since the selector existed.
fn selector_failed(e: crate::mux_selector::SelectorError) -> LaneError {
    LaneError::StartFailed {
        detail: e.message,
        exit_code: e.exit_code,
        boot_phase: None,
    }
}

/// `create::NewError` → the wire. Its `Display` is ALREADY a complete
/// `qd start: …` / `ERROR: …` line, so nothing is stamped.
///
/// `BootTimeout` is the one variant carrying a fact beyond its text: the TYPED
/// boot phase. `qd start -p` files a `priming-readiness-timeout` record whose
/// `phase` field is exactly this value, and the whole point of the typed phase
/// (m-4, ack3-spec §8) is that the old `detail.contains("did not reach idle")`
/// string-match is gone. Dropping it at the boundary would restore that coupling
/// silently, so it rides across.
fn new_error(e: crate::create::NewError) -> LaneError {
    use crate::create::NewError as E;
    let detail = e.to_string();
    match e {
        E::BootTimeout { phase, .. } => LaneError::StartFailed {
            detail,
            exit_code: 1,
            boot_phase: Some(phase),
        },
        E::NameRejected { .. }
        | E::NameUnsafeS2 { .. }
        | E::PreflightStale(_)
        | E::NameHeldLive { .. }
        | E::NameInUse(_)
        | E::NameClaimed { .. }
        | E::StaleEndedPane { .. }
        | E::ZmxMissing(_)
        | E::EmbeddedDaemonLaunchFailed(_)
        | E::ZmxRunFailed(_)
        | E::NotAttachable { .. }
        | E::SocketDirSplit { .. }
        | E::EnvFileWriteFailed { .. } => start_failed(detail),
    }
}

fn codex_tui_error(e: crate::provider::codex::pane::CodexTuiError) -> LaneError {
    use crate::provider::codex::pane::CodexTuiError as E;
    match e {
        // Nested, not flattened: a create that failed inside the shared pane
        // choreography answers with that choreography's own verdict — including
        // its boot phase, which only that path can produce.
        E::Create(inner) => new_error(inner),
        E::NoName
        | E::NeverUsed { .. }
        | E::IdMintFailed { .. }
        | E::PaneVanished { .. }
        | E::RowWriteFailed { .. } => {
            start_failed(start_line(e.is_self_attributed(), e.to_string()))
        }
    }
}

fn pi_tui_error(e: crate::provider::pi::pane::PiTuiError) -> LaneError {
    use crate::provider::pi::pane::PiTuiError as E;
    match e {
        E::Create(inner) => new_error(inner),
        // CapabilityProbeFailed refuses rather than guesses: the probe could not
        // tell, and blessing an unknown binary restores the dead-pane failure the
        // preflight exists to prevent. Every one of these is body-only `Display`
        // (the type is shared with `qd resume`/`qd attach`), so the verb is
        // stamped here — see [`start_line`].
        E::NoName
        | E::NoSessionId { .. }
        | E::SessionIdUnsupported { .. }
        | E::CapabilityProbeFailed { .. }
        | E::InvalidSessionId { .. }
        | E::SessionIdTaken { .. }
        | E::IdMintFailed { .. }
        | E::PaneVanished { .. }
        | E::RowWriteFailed { .. } => {
            start_failed(start_line(e.is_self_attributed(), e.to_string()))
        }
    }
}

/// Every `DaemonError` `Display` is already a complete `qd start: …` line, and
/// every one exits 1. `VersionBreaking` used to be in this list; it is gone
/// because a breaking version drift no longer refuses at all — see
/// [`crate::create_daemon::check_version`] for the ruling and what it traded.
fn daemon_error(e: crate::create_daemon::DaemonError) -> LaneError {
    use crate::create_daemon::DaemonError as E;
    let detail = e.to_string();
    match e {
        E::NameClaimed { .. }
        | E::VersionUnknown { .. }
        | E::PortAllocFailed { .. }
        | E::SpawnFailed { .. }
        | E::HandshakeFailed { .. }
        | E::ThreadStartFailed { .. }
        | E::RowWriteFailed { .. }
        | E::IdMintFailed { .. } => start_failed(detail),
    }
}

fn pi_create_error(e: crate::provider::pi::daemon::PiCreateError) -> LaneError {
    use crate::provider::pi::daemon::PiCreateError as E;
    let detail = e.to_string();
    match e {
        E::NameClaimed { .. }
        | E::PortAllocFailed { .. }
        | E::SpawnFailed { .. }
        | E::RowWriteFailed { .. } => start_failed(detail),
    }
}

/// Every acp create failure is machinery, and the exhaustive match is what makes
/// that a statement rather than a default: an ACP create has no policy gate to
/// fail. It takes no name-claim, sniffs no version, and runs no capability
/// preflight. All of them `Display` as a complete `qd start: …` line.
fn acp_create_error(e: crate::provider::acp::daemon::AcpCreateError) -> LaneError {
    use crate::provider::acp::daemon::AcpCreateError as E;
    let detail = e.to_string();
    match e {
        E::PortAllocFailed { .. }
        | E::ExeUnresolved { .. }
        | E::IdMintFailed { .. }
        | E::SpawnFailed { .. }
        | E::NotReady { .. }
        | E::RowWriteFailed { .. } => start_failed(detail),
    }
}

// `fn todo(op, detail) -> Err(LaneError::NotImplemented { .. })` lived here, the
// one constructor of that variant in this file. It is GONE, because as of stage-2
// phase 4 nothing constructs it: all nine methods are built for all seven lanes.
// `LaneError::NotImplemented` stays in the contract — it is how a lane says "this
// is a debt, not an answer", and the next debt should say so the same way — but a
// helper with no caller would only invite a method to be parked back on one
// quietly. `no_method_is_parked_on_a_blocker_any_more` is the pin.

/// The blockers that came down, kept as a record of what each one actually was.
///
/// Every constant here is retired; what is left is the reasoning, because a
/// blocker's real content is usually not what its one-line text said. The
/// remaining inventory is `doc/tbd/provider-architecture/07-lane-gaps.md`.
mod blocked {
    // START is IMPLEMENTED for all seven lanes (stage-2 phase 3) — see
    // `LaneOps::start` below.
    //
    // The blocker read: "create routing lives in lifecycle::run_new as a 5-step
    // ordered if-chain whose ordering is comment-enforced only; extracting it
    // per-lane is stage-2 phase 3." The if-chain is not extracted — it is GONE.
    // Its whole content was
    //
    //     Lane::new(Harness::from_provider_id(p)?,
    //               if interactive { Mode::Pane } else { harness.create_default_mode() })
    //
    // and every part of that already existed as data: `create_default_mode` gives Pane
    // for claude and Daemon for codex/pi/acp*/opencode (the chain's arms 3, 5 and
    // fall-through), explicit `--interactive` is `Mode::Pane` (arms 2 and 4), and
    // `Lane::new` answers `None` for exactly the three impossible combinations —
    // which IS the acp-interactive refusal. So the ordering does not get
    // documented; there is nothing left to order.
    //
    // Why that mattered: two of the five arms could be swapped SILENTLY. Swap the
    // pi arms or the codex arms and `--interactive` is ignored — the caller asks
    // for a pane, gets a headless resident, and the verb exits 0. The exhaustive
    // `match (harness, mode)` below cannot express that: the lane IS the key, so
    // there is no arm to forget and no order to get wrong. It deliberately does
    // NOT route on `Hosting::Daemon` or on a `starts_with("acp/")` prefix — the
    // catch-all-wearing-a-specific-name shape that made the swaps silent.
    // KILL is IMPLEMENTED (stage-2 phase 3) — see `LaneOps::kill` below.
    //
    // KILL_DAEMON read: "run_codex_kill / run_acp_kill / run_pi_kill are private
    // to the kill module and need pub(super)". They are not in that module any
    // more: `kill_codex` and `kill_acp` came over with the resume_daemon split
    // (`provider::codex::resume`, `provider::acp::resume`) and
    // `teardown_pi_daemon` was always here — so the four daemon arms are plain
    // delegations, as predicted, each answering `Confirmation::Unknown` for
    // `reaped`.
    //
    // KILL_PANE read: "the pane dual-reap is not a function — it is ~450 lines
    // inlined in kill::run(m: &ArgMatches), interleaved with printing and early
    // returns, so there is nothing to delegate to; it must be extracted first."
    // That was the whole of the work: `crate::kill` held the entire PURE decision
    // layer and NONE of the effect layer. It now holds both — the body was MOVED
    // into `kill::reap_pane_session`, ordering, verify loop, failure wording and
    // tombstone step intact, with the four printing sites turned into returned
    // fields. The verb renders those fields and keeps the dead-pid registry sweep,
    // which is housekeeping over every row rather than this session's lane.
    // LIST/HEALTH are IMPLEMENTED (stage-2 phase 1) — see `crate::lane_read`.
    // The blocker read: "gather_providers answers for every lane at once — one
    // registry slice in, one flat ProviderGather out — so there is no per-lane
    // call to delegate to". That was a real shape problem and it is now cut: the
    // four cold stores are four `*_cold_scan` functions the gather and the lanes
    // BOTH call, and the two live status sources (codex's rollout tail, pi's
    // resident point-read) are per-row entry points into the same code. What is
    // left of `gather_providers` is the part that was never per-lane — the shared
    // transcript stats/path resolution serving qd's OWN live and tombstone
    // branches — and it stays where it is, called by qd.
    // DELIVER is IMPLEMENTED for claude/mux-pane (stage-2 phase 2) — see
    // `LaneOps::deliver` below. Two blockers came down to get there, in order.
    //
    // The FIRST read: "the existing send fns return a bare i32 exit code, not a
    // message id, so a Receipt cannot be constructed without changing them."
    // Retired by widening both carriers to a `CarrierOutcome { code, message_id }`
    // (the claude relay one — now `delivery::relay::send_claude_relay` — and the
    // pane one, now `delivery::pty::deliver_mux_pty`); the
    // id is the same one in both cases — `Payload::SendInitiated.send_id` in the
    // recipient's events file, the join key the whole terminal apparatus already
    // uses. `deliver_then_stamp` reads `.code` through a shim, so the disposition
    // ledger's bytes did not move.
    //
    // The SECOND read: "what blocks the method is DeliverPolicy — an atomic
    // wake_if_cold hides the cold/live verdict inside the lane while qd's ledger
    // stamps `queued` BEFORE the wake is tried, and DeliverPolicy has no render
    // slot." Both were CONTRACT problems, which is what the phase-2 gate existed
    // to find, and both are repaired: `Receipt::woke` lets qd stamp `queued`
    // retrospectively, and `DeliverPolicy::render` carries the resolved mode in as
    // plain data. See the gate note on `LaneOps::deliver`.
    //
    // The THIRD read was the last, and it was a scope note rather than a blocker:
    // "the other six wait on phase 4 — their carriers (send_relay::run_{codex,acp,
    // pi}_send) still return a bare exit code, so CarrierOutcome::not_yet_widened
    // is all they can answer and a Receipt would have no message id to carry."
    // Phase 4 widened all three (and pi's floor sub-lane) the same way phase 2
    // widened the claude pair: every arm's exit code is UNCHANGED and the id is the
    // resident's TURN id, which `emit_daemon_send_events` was already writing as
    // `Payload::SendInitiated.send_id`. Nothing was invented — the id was on the
    // floor, unreturned. `not_yet_widened` is deleted with its last caller.
    // AWAIT_TERMINAL is IMPLEMENTED for all seven lanes (stage-2 phase 4) — see
    // `LaneOps::await_terminal` below.
    //
    // The blocker read: "no terminal source is reachable without the
    // delivery-event ledger wiring." The wiring was already here — it arrived with
    // `crate::events` — and `deliver` is what made it reachable: every lane's
    // carrier now hands back the `Payload::SendInitiated.send_id` that
    // `events::await_received` keys on, so there is a source and it is the same
    // one for all seven. What that method does NOT do is fold in `qd wait`; its
    // docs say why, and it is the one piece of phase 4 left open.

    // RECOVER is IMPLEMENTED for all seven lanes (stage-2 phase 4) — see
    // `LaneOps::recover` below.
    //
    // The blocker read: "the transcript search (events::recovery_read) is here,
    // but the caller side (verbs/recover.rs) must become a caller of this method
    // rather than a reader of the ledger." BOTH halves are now done — stage-3 phase
    // 3A. What unblocked the second half was a contract shape rather than effort:
    // `recover` was addressed by `SessionId`, while the sweep also carries sends
    // keyed by `events::byname_key` (a send made before a session id existed), which
    // no `SessionId` can name. `LedgerAddress` is that widening, and with it the
    // verb keeps only what is qd's — the sweep enumeration, the liveness fence, the
    // report — while the search crosses and stops hard-coding claude's
    // `~/.claude/projects` layout for every harness (see `LaneRecoveryDeps`).
}

impl LaneOps for LaneImpl<'_> {
    /// Create a session — **the routing table, and nothing else.**
    ///
    /// This replaces a five-arm ordered `if`-chain in `lifecycle::run_new` whose
    /// ordering was enforced only by a comment, and two of whose mis-swaps were
    /// SILENT: swap the pi arms or the codex arms and `--interactive` is ignored,
    /// so the caller asks for an attachable pane and is handed a headless
    /// resident, with exit 0. Nothing fails; the answer is simply wrong.
    ///
    /// An exhaustive `match (harness, mode)` cannot express that. The lane IS the
    /// key, so there is no arm to forget and no order to get wrong — the same
    /// property [`LaneOps::kill`] and [`LaneOps::wake`] already rely on, and the
    /// reason this deliberately does NOT route on `Hosting::Daemon` or on a
    /// `starts_with("acp/")` prefix. Those are catch-alls wearing specific names,
    /// and they are what made the swaps silent.
    ///
    /// The chain's whole content survives as data and is asserted as such by
    /// `start_routing_is_total_over_every_real_input`: a lane is
    /// `Lane::new(Harness::from_provider_id(p)?, if interactive { Mode::Pane }
    /// else { harness.create_default_mode() })`, and the three combinations `Lane::new`
    /// refuses ARE the acp-interactive refusal — unrepresentable rather than
    /// checked.
    ///
    /// Six of the seven arms are pure delegation to a core already in this crate.
    /// See the arm methods for what each one does not do, and who owns it.
    fn start(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        match (self.lane.harness, self.lane.mode) {
            (Harness::ClaudeCode, Mode::Pane) => self.start_claude_pane(req),
            (Harness::Codex, Mode::Pane) => self.start_codex_pane(req),
            (Harness::Codex, Mode::Daemon) => self.start_codex_daemon(req, Some(Mode::Daemon.hosting_token())),
            (Harness::Codex, Mode::AppServer) => self.start_codex_app_server(req),
            (Harness::Pi, Mode::Pane) => self.start_pi_pane(req),
            (Harness::Pi, Mode::Daemon) => self.start_pi_daemon(req),
            (Harness::Pi, Mode::Extension) => self.start_pi_extension(req),
            // Seven lanes, seven arms. The two acp lanes share a core and are
            // still written separately, because the seven-line table IS the thing
            // that replaced the comment — collapsing two of its rows to save a
            // line would start it back down the road it came from.
            (Harness::AcpClaudeCode, Mode::Daemon) => self.start_acp_daemon(req),
            (Harness::Opencode, Mode::Daemon) => self.start_acp_daemon(req),

            // --- the three combinations that do not exist -------------------
            // Reachable only by constructing a `Lane` by hand around
            // `Lane::new`, which refuses all three. They are answers, not gaps:
            // claude has no daemon lane, and an ACP bridge is a protocol adapter
            // with no terminal of its own at all.
            (Harness::ClaudeCode, Mode::Daemon) => Err(LaneError::NotSupported {
                op: "start".to_string(),
                reason: "claude-code has no daemon lane".to_string(),
            }),
            (
                Harness::ClaudeCode | Harness::Codex | Harness::AcpClaudeCode | Harness::Opencode,
                Mode::Extension,
            ) => Err(LaneError::NotSupported {
                op: "start".to_string(),
                reason: "the extension lane is pi's alone: it rides pi's `--extension` loader \
                         and its in-process extension API, which no other harness here has"
                    .to_string(),
            }),
            (Harness::AcpClaudeCode | Harness::Opencode, Mode::Pane) => {
                Err(LaneError::NotSupported {
                    op: "start".to_string(),
                    reason: "an ACP bridge is a protocol adapter with no terminal of its own"
                        .to_string(),
                })
            }
            (
                Harness::ClaudeCode | Harness::Pi | Harness::AcpClaudeCode | Harness::Opencode,
                Mode::AppServer,
            ) => Err(LaneError::NotSupported {
                op: "start".to_string(),
                reason: APP_SERVER_IS_CODEX_ONLY.to_string(),
            }),
        }
    }

    /// Dispatches on the lane rather than on a provider string — which is what
    /// makes the missing-arm class of bug (BUG 2) unrepresentable: there is no
    /// arm to forget, because the lane IS the key.
    ///
    /// **`render` is the caller's, and it is a value.** A revive builds a fresh
    /// pane, so it has to know whether to build it inline or alt-screen, and
    /// `qd resume`/`qd send` both resolve that from the `render-default` config
    /// before revive. Until the phase-2 trait revision the signature had nowhere
    /// to put it and all three pane arms below hardcoded `RenderMode::default()`
    /// — a KNOWN DEFECT that handed a user with `render-default = "alt-screen"`
    /// an inline pane. It is a parameter now, because this crate still cannot go
    /// and READ the setting: the config lives behind `dispatch::secrets`,
    /// deliberately on qd's side (see [`crate::launch`], above
    /// `resolve_render_mode`). qd resolves, qw receives. See
    /// [`DeliverPolicy::render`].
    ///
    /// The four daemon arms take no render mode at all, and that is an answer
    /// rather than an omission: a resident has no pane to build.
    ///
    /// **`cwd_override` is the claude arm's, and only the claude arm's.** It is
    /// `qd resume --cwd <dir>` — the F3 escape for a project directory that
    /// moved — and it reaches `plan_claude_revive`, which resolves it against the
    /// recorded cwd and threads the result to the detached launch. This arm
    /// hardcoded `None` until the revision that added the parameter, so routing
    /// `qd resume` through the lane would have validated the user's override in
    /// the verb and then dropped it here, silently. The other six arms do not
    /// consult it: a resident inherits its own cwd resolution, and the two TUI
    /// revives take the row's recorded cwd through [`revive_cwd`].
    fn wake(
        &self,
        id: &SessionId,
        render: RenderMode,
        cwd_override: Option<String>,
    ) -> Result<WakeOutcome, LaneError> {
        let s = self.row(id)?;
        let clock = crate::effects::RealClock;
        match (self.lane.harness, self.lane.mode) {
            // --- the four harnesses with no app-server residence ---------------
            // Placed FIRST so it also shadows the `(Harness::ClaudeCode, _)` arm
            // below: a hand-built `claude-code/app-server` must be REFUSED, not
            // quietly run through claude's own machinery. See
            // [`APP_SERVER_IS_CODEX_ONLY`].
            (
                Harness::ClaudeCode | Harness::Pi | Harness::AcpClaudeCode | Harness::Opencode,
                Mode::AppServer,
            ) => Err(LaneError::NotSupported {
                op: "wake".to_string(),
                reason: APP_SERVER_IS_CODEX_ONLY.to_string(),
            }),
            // --- claude pane: the two-phase revive (plan → backend/dirs → launch).
            // The phase split exists so the same-name guard and the env-file write
            // land before the mux backend is resolved; see the core's module docs.
            (Harness::ClaudeCode, _) => {
                use crate::provider::claude::revive::{
                    plan_claude_revive, run_claude_revive, ClaudeLaunchDeps, ClaudePlanDeps,
                    ClaudeReviveParams,
                };
                let plan_deps = ClaudePlanDeps {
                    env: self.env,
                    home: &self.paths.home,
                    paths: &self.paths,
                    ids_path: self.ids_path(),
                    clock: &clock,
                    fallback_cwd: std::env::current_dir()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| ".".to_string()),
                };
                let plan = plan_claude_revive(
                    &plan_deps,
                    &ClaudeReviveParams {
                        session: &s,
                        cwd_override: cwd_override.as_deref(),
                        render,
                        fresh: false,
                    },
                )
                // The lane is not a user-facing verb, so it carries the error's
                // BODY out and lets the verb stamp its own name on it, rather
                // than pretending a command was typed.
                .map_err(|e| self.wake_failed(e.body(), e.exit_code(), e.is_self_attributed()))?;

                let (_, canonical, legacy) = self
                    .socket_dirs()
                    .map_err(|e| LaneError::Transport { detail: e })?;
                let mux = self.mux.as_deref().ok_or_else(|| LaneError::Transport {
                    detail: "could not resolve the mux backend".to_string(),
                })?;
                let mut scan_dirs = vec![canonical.clone()];
                scan_dirs.extend(legacy);
                let launch_deps = ClaudeLaunchDeps {
                    mux,
                    canonical_dir: canonical,
                    scan_dirs,
                    paths: &self.paths,
                };
                let pane = run_claude_revive(&launch_deps, &clock, &plan)
                    .map_err(|e| self.wake_failed(e.body(), e.exit_code(), e.is_self_attributed()))?;
                self.woke_pane(id, pane)
            }

            (Harness::Codex, Mode::Pane) => {
                use crate::provider::codex::pane::{revive_codex_tui, CodexReviveParams};
                let exec = crate::exec::RealExec;
                let (backend, canonical, legacy) = self
                    .socket_dirs()
                    .map_err(|e| LaneError::Transport { detail: e })?;
                let deps = self.pane_deps(&exec, &clock, backend, canonical, legacy)?;
                let params = CodexReviveParams {
                    // The gate runs inside the core too; this is the resolved name.
                    name: s.name.clone().unwrap_or_default(),
                    session_id: s.session_id.clone(),
                    cwd: revive_cwd(s.cwd.as_deref()),
                    render,
                    old_pid: s.pid,
                };
                let pane = revive_codex_tui(&deps, &params).map_err(|e| {
                    // `Create` errors carry their own `qd start:` / `ERROR:`
                    // attribution and must be printed verbatim — the same
                    // `is_self_attributed` split the verb's failure-line
                    // formatter has always made.
                    self.wake_failed(e.to_string(), e.exit_code(), e.is_self_attributed())
                })?;
                self.woke_pane(id, pane)
            }

            (Harness::Pi, Mode::Pane) => {
                use crate::provider::pi::pane::{plan_pi_tui, revive_pi_tui, PiTuiParams};
                // Phase 1 FIRST — its refusals (the `--session-id` capability
                // preflight above all) must land before the backend is resolved.
                let plan = plan_pi_tui(
                    self.env,
                    &PiTuiParams {
                        name: s.name.clone().unwrap_or_default(),
                        cwd: revive_cwd(s.cwd.as_deref()),
                        render,
                        session_id: Some(s.session_id.clone()),
                    },
                )
                .map_err(|e| self.wake_failed(e.to_string(), e.exit_code(), e.is_self_attributed()))?;
                let exec = crate::exec::RealExec;
                let (backend, canonical, legacy) = self
                    .socket_dirs()
                    .map_err(|e| LaneError::Transport { detail: e })?;
                let deps = self.pane_deps(&exec, &clock, backend, canonical, legacy)?;
                let pane = revive_pi_tui(&deps, &plan, s.pid).map_err(|e| {
                    self.wake_failed(e.to_string(), e.exit_code(), e.is_self_attributed())
                })?;
                self.woke_pane(id, pane)
            }

            // `pi/extension` — the pi TUI revive, plus the control channel.
            //
            // Identity is carried exactly as `pi/mux-pane` carries it (the row's
            // recorded `--session-id` reopens that conversation), so the only
            // additions are the ones the create makes: install the extension,
            // clear a stale socket, relaunch with the flag, and re-record
            // `hosting`/`endpoint`. A revive that skipped the row correction
            // would come back as `pi/mux-pane` and silently lose its channel —
            // the same trap the create path documents.
            (Harness::Pi, Mode::Extension) => {
                use crate::provider::pi::extension::{
                    plan_extension_launch, revive_extension_session,
                };
                use crate::provider::pi::pane::PiTuiParams;
                // Phase 1 FIRST — its refusals must land before the backend is
                // resolved, the same order the pi pane arm holds.
                let launch = plan_extension_launch(
                    self.env,
                    &PiTuiParams {
                        name: s.name.clone().unwrap_or_default(),
                        cwd: revive_cwd(s.cwd.as_deref()),
                        render,
                        session_id: Some(s.session_id.clone()),
                    },
                )
                .map_err(|e| self.wake_failed(e.to_string(), e.exit_code(), e.is_self_attributed()))?;
                let exec = crate::exec::RealExec;
                let (backend, canonical, legacy) = self
                    .socket_dirs()
                    .map_err(|e| LaneError::Transport { detail: e })?;
                let deps = self.pane_deps(&exec, &clock, backend, canonical, legacy)?;
                let out = revive_extension_session(&deps, &launch, s.pid).map_err(|e| {
                    self.wake_failed(e.to_string(), e.exit_code(), e.is_self_attributed())
                })?;
                self.woke_pane(
                    id,
                    ReviveHandle {
                        socket_dir: out.socket_dir,
                        zmx_name: out.zmx_name,
                    },
                )
            }

            // `codex/app-server` shares this revive verbatim: same daemon, same
            // endpoint, same identity-checked respawn. Only `attach` differs —
            // and the ONE thing that has to travel for that to stay true is the
            // row's `hosting` stamp, which this arm is the only place that knows.
            // See the `hosting` line inside the params below.
            (Harness::Codex, Mode::Daemon | Mode::AppServer) => {
                use crate::provider::codex::resume::{
                    resume_codex_real, ResumeOutcome, ResumeParams,
                };
                let params = ResumeParams {
                    name: s.name.clone().unwrap_or_else(|| s.session_id.clone()),
                    thread_id: s.session_id.clone(),
                    cwd: s.cwd.clone(),
                    current_pid: s.pid,
                    current_endpoint: self.current_endpoint(s.pid),
                    // THE LANE BEING REVIVED, restated as data. `self.lane` is
                    // the lane the caller opened and the only thing in this
                    // revive that distinguishes the two codex arms, so it is
                    // what the rewritten row must carry — NOT `s.hosting`,
                    // which would launder a row whose stamp was missing or
                    // garbage back onto disk unchanged, and NOT a literal,
                    // which would go stale the moment a third codex residence
                    // exists.
                    //
                    // Without it a revive rewrote `hosting: None` and the
                    // session came back as `codex/daemon`: an app-server row
                    // that survived a `qd resume` lost its attachability, and
                    // `qd attach` started answering "a daemon-hosted session
                    // has no terminal of its own" about a session that has one.
                    // The stamp a create writes and the stamp a revive writes
                    // are the same field read by the same derivation, so they
                    // may not disagree — see `ResumeParams::hosting`.
                    hosting: Some(self.lane.mode.hosting_token().to_string()),
                };
                let out = resume_codex_real(
                    self.paths.sessions_dir.clone(),
                    self.log_dir(),
                    self.ids_path(),
                    &params,
                )
                // The codex daemon error is the one that is neither bare nor
                // self-attributed: its Display is a body, and the line the verb
                // prints names the SESSION before it (`qd resume: "wk": …`). The
                // name is the lane's to supply — it read the row.
                .map_err(|e| {
                    self.wake_failed(format!("\"{}\": {e}", params.name), e.exit_code(), false)
                })?;
                // The verdict is the CORE's, taken at the instant it made the
                // decision (`pid_alive && endpoint_set && cmdline_is_ours`).
                // Collapsing both arms into one answer here is what used to make
                // a caller print "resumed …" for a session it never revived.
                match out {
                    ResumeOutcome::AlreadyRunning => {
                        self.woke_daemon(id, WakeState::AlreadyRunning, None)
                    }
                    ResumeOutcome::Revived { pid, endpoint } => self.woke_daemon(
                        id,
                        WakeState::Revived,
                        Some(Resident { pid, endpoint }),
                    ),
                }
            }

            (Harness::Pi, Mode::Daemon) => {
                use crate::provider::pi::resume::{
                    resume_pi, PiResumeDeps, PiResumeOutcome, PiResumeParams,
                };
                // Not a resume decision — process introspection that failed
                // before any core ran, so it carries the verb precedent (exit 1)
                // and the session name the verb's line has always led with.
                // `self_exe` writes its own "cannot resolve own executable"
                // body, so nothing is restated here.
                let exe = self_exe().map_err(|e| {
                    self.wake_failed(
                        format!(
                            "\"{}\": {e}",
                            s.name.clone().unwrap_or_else(|| s.session_id.clone())
                        ),
                        1,
                        false,
                    )
                })?;
                let spawner = crate::create_daemon::RealDaemonSpawner;
                let is_alive = |pid: i64| crate::effects::is_pid_alive(pid as i32);
                let probe = crate::create_daemon::real_cmdline_probe;
                let deps = PiResumeDeps {
                    exe,
                    pi_bin: self.env.var("QD_PI_BIN").filter(|s| !s.is_empty()),
                    session_dir: self
                        .env
                        .var("PI_CODING_AGENT_SESSION_DIR")
                        .filter(|s| !s.is_empty()),
                    sessions_dir: self.paths.sessions_dir.clone(),
                    claims_dir: self.claims_dir(),
                    log_dir: self.log_dir(),
                    spawner: &spawner,
                    clock: &clock,
                    ids_path: self.ids_path(),
                    is_pid_alive: &is_alive,
                    cmdline_probe: &probe,
                };
                let params = PiResumeParams {
                    name: s.name.clone().unwrap_or_else(|| s.session_id.clone()),
                    session_id: s.session_id.clone(),
                    cwd: s.cwd.clone(),
                    current_pid: s.pid,
                    current_endpoint: self.current_endpoint(s.pid),
                };
                // `PiResumeError`'s Display is the COMPLETE line, `qd resume:`
                // prefix included — see that type's module docs for why this
                // lane diverges from the codex one.
                let out = resume_pi(&deps, &params)
                    .map_err(|e| self.wake_failed(e.to_string(), e.exit_code(), true))?;
                // The core echoes the caller's own `name` back on both arms; the
                // lane drops it rather than carrying a fact the caller supplied.
                match out {
                    PiResumeOutcome::AlreadyRunning { .. } => {
                        self.woke_daemon(id, WakeState::AlreadyRunning, None)
                    }
                    PiResumeOutcome::Revived { pid, endpoint, .. } => self.woke_daemon(
                        id,
                        WakeState::Revived,
                        Some(Resident { pid, endpoint }),
                    ),
                }
            }

            (Harness::AcpClaudeCode | Harness::Opencode, _) => {
                use crate::provider::acp::daemon::{
                    resume_acp, AcpDaemonDeps, AcpResumeOutcome, AcpResumeParams, AcpWarning,
                };
                // Not a resume decision — see the pi arm.
                let exe = self_exe().map_err(|e| {
                    self.wake_failed(
                        format!(
                            "\"{}\": {e}",
                            s.name.clone().unwrap_or_else(|| s.session_id.clone())
                        ),
                        1,
                        false,
                    )
                })?;
                let spawner = crate::create_daemon::RealDaemonSpawner;
                let alloc = crate::create_daemon::real_alloc_port;
                let probe = crate::create_daemon::real_cmdline_probe;
                // The lane has no terminal. A non-fatal notice is dropped rather
                // than printed — the caller gets the typed outcome, and inventing
                // a stderr write here would be the lane pretending to be a verb.
                let warn = |_: &AcpWarning| {};
                let deps = AcpDaemonDeps {
                    exe,
                    home: self.paths.home.clone(),
                    paths: &self.paths,
                    clock: &clock,
                    spawner: &spawner,
                    alloc_port: &alloc,
                    cmdline_probe: &probe,
                    warn: &warn,
                };
                let params = AcpResumeParams {
                    name: s.name.clone().unwrap_or_else(|| s.session_id.clone()),
                    session_id: s.session_id.clone(),
                    provider_id: s.provider.clone(),
                    cwd: s.cwd.clone(),
                    has_jsonl: s.jsonl_path.is_some(),
                    current_pid: s.pid,
                    current_endpoint: self.current_endpoint(s.pid),
                };
                // `AcpResumeError`'s Display is the COMPLETE `qd resume: …`
                // line — this lane has exactly one verb from every caller.
                let out = resume_acp(&deps, &params)
                    .map_err(|e| self.wake_failed(e.to_string(), e.exit_code(), true))?;
                match out {
                    AcpResumeOutcome::AlreadyRunning { .. } => {
                        self.woke_daemon(id, WakeState::AlreadyRunning, None)
                    }
                    AcpResumeOutcome::Revived { pid, endpoint, .. } => self.woke_daemon(
                        id,
                        WakeState::Revived,
                        Some(Resident { pid, endpoint }),
                    ),
                }
            }

            // Not a lane. Reachable only by constructing a `Lane` by hand around
            // `Lane::new`, which refuses it — the extension lane is pi's alone.
            (Harness::Codex, Mode::Extension) => Err(LaneError::NotSupported {
                op: "wake".to_string(),
                reason: "the extension lane is pi's alone".to_string(),
            }),
        }
    }

    /// Reap the session, keyed on the LANE rather than on a provider string —
    /// which is what makes the leak `kill.rs:88-95` documents unrepresentable.
    /// The verb's daemon arms are guarded by `hosting()==Daemon` if-chains placed
    /// in a load-bearing ORDER, and getting that order wrong does not merely
    /// fail: applying the daemon group-kill to a PANE row silently LEAKS, because
    /// the cmdline guard correctly refuses to signal a pane (a TUI has no
    /// `app-server`, no `pi-daemon`), the row is tombstoned anyway, and the pane
    /// keeps running. Here the lane IS the key, so there is no ordering to get
    /// wrong and no arm to forget.
    ///
    /// Every arm DELEGATES to the function the verb calls — the three pane lanes
    /// to [`crate::kill::reap_pane_session`] (the dual-reap, extracted from
    /// `qd stop`'s body in this same change), the four daemon lanes to the
    /// group-kill each already had.
    ///
    /// **What the report carries beyond the claim.** Every arm's delegate makes
    /// observations a verb has to render and cannot re-derive — the pane reap's
    /// foreign-pid note, its failure clauses and its RESOLVED pane name (computed
    /// over state the reap then destroys), and the daemon arms' identity-checked
    /// `was_alive`, read at the instant the signal decision is made. Until this
    /// revision the return type was [`KillOutcome`] alone and all of it was
    /// dropped here, which is why `LaneOps::kill` had no production caller: the
    /// verb could not have rendered what it prints today from the two
    /// [`Confirmation`]s. See [`ReapObservations`].
    fn kill(&self, id: &SessionId) -> Result<KillReport, LaneError> {
        let s = self.row(id)?;
        match (self.lane.harness, self.lane.mode) {
            // --- the four harnesses with no app-server residence ---------------
            // Placed FIRST so it also shadows the `(Harness::ClaudeCode, _)` arm
            // below: a hand-built `claude-code/app-server` must be REFUSED, not
            // quietly run through claude's own machinery. See
            // [`APP_SERVER_IS_CODEX_ONLY`].
            (
                Harness::ClaudeCode | Harness::Pi | Harness::AcpClaudeCode | Harness::Opencode,
                Mode::AppServer,
            ) => Err(LaneError::NotSupported {
                op: "kill".to_string(),
                reason: APP_SERVER_IS_CODEX_ONLY.to_string(),
            }),
            // --- the FOUR pane lanes: one dual-reap serves all of them ---------
            // A pane-hosted codex or pi row is reaped by the SAME machinery as a
            // claude one (mux kill + pid-targeted kill + descendant sweep) — that
            // is exactly what the verb's fall-through arranges, and why the daemon
            // arms above it are scoped to `hosting()==Daemon`.
            //
            // `pi/extension` reaps identically: its pi process IS the pane
            // process, so there is no resident to group-kill and nothing about
            // the control channel changes how the process dies. What it adds is
            // unlinking the socket afterwards — the extension removes it on a
            // clean shutdown, and a reap is by definition not one.
            (_, Mode::Pane | Mode::Extension) => {
                let exec = crate::exec::RealExec;
                let clock = crate::effects::RealClock;
                let mux = self.mux.as_deref().ok_or_else(|| LaneError::Transport {
                    detail: "could not resolve the mux backend".to_string(),
                })?;
                let (_, canonical, legacy) = self
                    .socket_dirs()
                    .map_err(|e| LaneError::Transport { detail: e })?;
                let mut dirs = vec![canonical];
                dirs.extend(legacy);
                let deps = crate::kill::PaneReapDeps {
                    paths: &self.paths,
                    env: self.env,
                    exec: &exec,
                    clock: &clock,
                    mux,
                    dirs: &dirs,
                };
                // The reap's other fields are the VERB's rendering inputs — the
                // failure clauses, the foreign-pid note, the survivor advisory,
                // the three W4 namespaces. A lane has no stdout, so it never
                // PRINTS them; it reports them, which is a different thing and
                // the one this arm got wrong. `reaped`/`tombstoned` name the
                // STATES (No for a failure or a survivor, Unknown for the foreign
                // pid) but not which one, and three distinct exit-1 messages hang
                // off that difference.
                let report = crate::kill::reap_pane_session(&deps, &s).report();

                // Unlink the control socket. The extension removes it on a clean
                // `session_shutdown`, and a reap is by definition not one, so a
                // killed session would otherwise leave a socket file that
                // `connect(2)` fails on with ECONNREFUSED — which reads as "the
                // channel is broken" rather than "the session is gone". Scoped to
                // the lane that has one, and best-effort: the create clears a
                // stale path before binding anyway.
                if self.lane.has_control_channel() {
                    crate::provider::pi::extension::install::remove_socket(
                        &crate::provider::pi::extension::socket_for(
                            self.env,
                            self.current_endpoint(s.pid).as_deref(),
                            &s.session_id,
                        ),
                    );
                }
                Ok(report)
            }

            // --- codex/daemon ---------------------------------------------------
            // `codex/app-server` reaps identically — and the `reap_viewer_pane`
            // call inside this arm is what stops its attached TUI from being
            // left rendering a dead connection.
            (Harness::Codex, Mode::Daemon | Mode::AppServer) => {
                use crate::provider::codex::resume::{kill_codex, reap_viewer_pane};
                let Some(pid) = self.daemon_pid(&s) else {
                    return Ok(nothing_to_reap());
                };
                let captured = crate::registry::read_entry(&self.paths.sessions_dir, pid);
                let spawner = crate::create_daemon::RealDaemonSpawner;
                let probe = crate::create_daemon::real_cmdline_probe;
                let killed = kill_codex(
                    &self.paths.sessions_dir,
                    pid,
                    captured.as_ref(),
                    &spawner,
                    &probe,
                );
                // The viewer pane is a codex affordance, so it rides with the codex
                // arm rather than with the verb: `qd attach` on a live daemon row
                // spawns one, and left behind it renders a dead connection.
                if let Some(vname) = s.name.as_deref() {
                    reap_viewer_pane(self.env, &self.paths.home, vname);
                }
                Ok(daemon_reaped(pid, killed.was_alive))
            }

            // --- the two acp/* daemon lanes -------------------------------------
            (Harness::AcpClaudeCode | Harness::Opencode, Mode::Daemon) => {
                use crate::provider::acp::resume::kill_acp;
                let Some(pid) = self.daemon_pid(&s) else {
                    return Ok(nothing_to_reap());
                };
                let captured = crate::registry::read_entry(&self.paths.sessions_dir, pid);
                let spawner = crate::create_daemon::RealDaemonSpawner;
                let probe = crate::create_daemon::real_cmdline_probe;
                let killed = kill_acp(
                    &self.paths.sessions_dir,
                    pid,
                    captured.as_ref(),
                    &spawner,
                    &probe,
                );
                Ok(daemon_reaped(pid, killed.was_alive))
            }

            // --- pi/daemon ------------------------------------------------------
            // The one arm whose delegate returns NOTHING: `teardown_pi_daemon` is
            // `-> ()` and silently declines to signal when the pid's identity does
            // not match. That is the case `Confirmation::Unknown` was introduced
            // for — see `contract::Confirmation`.
            (Harness::Pi, Mode::Daemon) => {
                use crate::provider::pi::daemon::teardown_pi_daemon;
                let Some(pid) = self.daemon_pid(&s) else {
                    return Ok(nothing_to_reap());
                };
                let captured = crate::registry::read_entry(&self.paths.sessions_dir, pid);
                // The endpoint is the cmdline-identity discriminator that defeats
                // PID reuse; it lives on the row, not on the Session surface.
                let endpoint = captured.as_ref().and_then(|e| e.endpoint.as_deref());
                let probe = crate::create_daemon::real_cmdline_probe;
                // PLAIN liveness, read BEFORE the teardown — and deliberately NOT
                // the identity-checked form the codex/acp delegates compute for
                // themselves. `teardown_pi_daemon` returns `()`, so this is the
                // only moment the answer exists, and it is the same read (and the
                // same position) the verb has always made.
                let was_alive = crate::effects::is_pid_alive(pid as i32);
                teardown_pi_daemon(pid, endpoint, &probe);
                crate::registry::ensure_tombstone(
                    &self.paths.sessions_dir,
                    pid,
                    captured.as_ref(),
                );
                Ok(daemon_reaped(pid, was_alive))
            }

            // claude-code is pane-only; the arm above already took it.
            (Harness::ClaudeCode, Mode::Daemon) => Err(LaneError::NotSupported {
                op: "kill".to_string(),
                reason: "claude-code has no daemon lane".to_string(),
            }),
        }
    }

    /// The lane's own cold store. See [`crate::lane_read`] for why "own" excludes
    /// the live registry, orphaned panes and tombstones — none of which is any
    /// harness's session — and why three of the seven lanes correctly list
    /// nothing.
    fn list(&self) -> Result<Listing, LaneError> {
        crate::lane_read::list_for(self.lane, &self.paths, self.env)
    }

    /// The status `qd ls` renders for a LIVE row, render gates included.
    /// **Closes BUG 1**: pi/mux-pane now derives from the transcript tail instead
    /// of falling through to a sourceless `Idle`. See [`crate::lane_read`] for the
    /// per-lane source table.
    fn health(&self, id: &SessionId) -> Result<Health, LaneError> {
        crate::lane_read::health_for(self.lane, &self.paths, self.env, id)
    }

    /// Deliver one message — **all seven lanes** (claude/pane was stage-2 phase
    /// 2's gate; the other six landed in phase 4 once their carriers were widened
    /// to carry a message id).
    ///
    /// # The carrier choice is PRIVATE, and that is the whole point
    ///
    /// claude/pane is the one lane with two carriers, and the choice between them
    /// was `send_unified::select_carrier`'s `"claude-code"` arm, and that function
    /// is now retired. It is carried here verbatim, including its precedence rule — **a recorded relay
    /// port selects relay before mux state is considered**; PTY can only be
    /// selected from a positive `relay_port: None` observation PLUS a live joined
    /// pane. A caller never learns which one ran, which is the "carrier
    /// heterogeneity becomes private" claim the whole split rests on.
    ///
    /// The other six share three carriers between them — the PTY one twice over
    /// (a codex or pi `--interactive` row is typed at exactly as a relay-less
    /// claude pane is) and the ACP one twice over — which is why FIVE carriers
    /// serve seven lanes. The carrier is not the lane; which one a lane reaches is
    /// the lane's own answer.
    ///
    /// The bodies are NOT reimplemented and no longer reached by callback: all
    /// five are [`crate::delivery`] functions this method calls directly, and the
    /// `qd` verbs that used to own them now call the SAME functions. One body per
    /// carrier, shared, rather than two a reader had to compare by eye.
    ///
    /// # Atomicity, and the row this method re-reads
    ///
    /// The wake happens INSIDE the call when `policy.wake_if_cold` is set, and the
    /// row is re-read afterwards, so no caller can interleave a `health` between
    /// the verdict and the send. That is the race [`LaneOps::deliver`]'s contract
    /// forbids composing.
    ///
    /// # `wake_if_cold: false` ATTEMPTS. It does not refuse on a projection.
    ///
    /// `health` is read for ONE purpose here — deciding whether to wake — and it
    /// is therefore not read at all when no wake was asked for. There is no
    /// `LaneError::Cold` off this method. That is a DECISION, not an oversight,
    /// and the reason is that two components in this repo answer "is this session
    /// live" differently and **both answers are deliberate**:
    ///
    /// - `send_unified::is_live` reads the **status enum alone** — the string on
    ///   the JOIN's row, ungated.
    /// - [`LaneOps::health`] reads status **plus `(pid, start_time)`** through the
    ///   liveness gate, because it is the answer `qd ls` renders and `qd ls` must
    ///   not print `idle` for a pid that is gone.
    ///
    /// `tests/verbs_a4.rs`'s fixture is exactly the row that splits them —
    /// `{"pid":90101,"status":"idle"}` with a dead pid. `qd send` calls it LIVE,
    /// takes the live path, and today refuses it with exit 1, **no wake, no
    /// envelope, no disposition** (`send_live_unroutable_claude_is_unchanged_no_wake_no_envelope`).
    /// This method calls the same row COLD.
    ///
    /// Refusing off `health` in here would turn that one row into one of two
    /// wrong things. With `wake_if_cold: true` it becomes a SILENT WAKE — the
    /// funnel `attempted, delivery-failed{delivery}` becomes `attempted, queued,
    /// delivered`, which is the ledger's bytes moving. With `wake_if_cold: false`
    /// it becomes a `Cold`, a refusal class qd's live path has nowhere to put:
    /// every refusal it can render is either envelope-less and sync (and `Cold`
    /// is not one of the pinned classes) or a `delivery-failed` against an
    /// envelope it has not written yet.
    ///
    /// ATTEMPTING is what makes both of those go away, and it costs nothing: the
    /// carrier is handed a row whose pane nobody is holding, it fails the way it
    /// fails today, and qd stamps `delivery-failed{delivery}` — which is exactly
    /// what happens today. A dead-pid row has no receive path to succeed through,
    /// so "attempt and let the carrier report" and "refuse" differ only in WHO
    /// says so, and the carrier is the one that actually looked.
    ///
    /// **The gated-vs-ungated question itself is DELIBERATELY DEFERRED** to its
    /// own commit. Which of the two readings is `qd send`'s truth is a
    /// user-visible change to `qd send` — it decides whether a stale-live row gets
    /// revived — and it must not arrive buried inside a refactor whose whole claim
    /// is that no byte moved. Do not "fix" the disagreement here.
    ///
    /// # What the receipt says, exactly
    ///
    /// - exit 0 with an id ⇒ `accepted: true`, terminal [`TerminalExpectation::Pending`].
    ///   Every carrier is honestly pending: the embedded mux owns the terminal
    ///   after its handoff, the relay's arrives from the recipient's own
    ///   transcript observer, and each resident emits `turn-accepted` — which is
    ///   explicitly NON-terminal — on the inject ACK, with the terminal landing
    ///   later at its observe seam. [`TerminalExpectation::Unavailable`] is unused
    ///   by every lane, and that is an answer rather than a gap: even pi's
    ///   first-turn case, the one this crate documents as unconfirmable, reports
    ///   PENDING rather than foreclosing (`run_pi_send`'s own "not yet present in
    ///   the rollout — delivery PENDING, not confirmed").
    /// - nonzero WITH an id ⇒ `accepted: false`, terminal still `Pending`. The
    ///   carrier got as far as minting, so the send is KEYED and
    ///   [`LaneOps::recover`] can still resolve it. Calling that `Unavailable`
    ///   would foreclose a send that may yet land — the same reasoning
    ///   [`Terminal::Undetermined`] exists for.
    /// - nonzero with NO id ⇒ [`LaneError::Transport`]. A refusal that fired
    ///   before any id existed has nothing to key on, and a receipt whose
    ///   `message_id` was invented would be a lie the terminal apparatus then
    ///   joins on.
    /// Is there a carrier that could take a message? TOPOLOGY only, and
    /// side-effect-free.
    ///
    /// This is `deliver`'s carrier-shape question asked ALONE, and it deliberately
    /// answers it from the same two deciders `deliver` uses internally
    /// ([`claude_carrier`] and [`joined_pane`]) rather than from a second, shallower
    /// reading — a duplicate gate here that drifted from the one inside `deliver`
    /// would be the exact defect this method was added to remove one layer up.
    ///
    /// **It does not consult liveness, and that is deliberate.** A cold session's
    /// receive-path question is answered by a WAKE, not by a refusal: "stopped is
    /// not a refusal class". `deliver` composes its own `health` read with its own
    /// wake, and the caller reaching this method is the one that has already
    /// decided the row is live (or is about to hand the cold case to `deliver`'s
    /// `wake_if_cold`).
    ///
    /// The three daemon lanes answer [`ReceivePath::Available`] UNCONDITIONALLY.
    /// A resident's receive path is its recorded ws endpoint, which is not on the
    /// [`Session`] surface at all, and each daemon carrier re-reads it and answers
    /// its own "not reachable" refusal AFTER taking responsibility for the message
    /// — so a delivery to an unreachable resident is a `delivery-failed{delivery}`
    /// against a logged envelope today. Hoisting that refusal up here would turn it
    /// into an envelope-less `refused{...}`, which is the ledger's bytes moving.
    /// The honest answer for these lanes is "there is a carrier"; whether it
    /// connects is the carrier's to report.
    fn receive_path(&self, id: &SessionId) -> Result<ReceivePath, LaneError> {
        let s = self.row(id)?;

        match (self.lane.harness, self.lane.mode) {
            // --- the four harnesses with no app-server residence ---------------
            // Placed FIRST so it also shadows the `(Harness::ClaudeCode, _)` arm
            // below: a hand-built `claude-code/app-server` must be REFUSED, not
            // quietly run through claude's own machinery. See
            // [`APP_SERVER_IS_CODEX_ONLY`].
            (
                Harness::ClaudeCode | Harness::Pi | Harness::AcpClaudeCode | Harness::Opencode,
                Mode::AppServer,
            ) => Err(LaneError::NotSupported {
                op: "receive_path".to_string(),
                reason: APP_SERVER_IS_CODEX_ONLY.to_string(),
            }),
            // claude/pane: the one lane with a choice, and the one lane that can
            // answer `Undetermined`. `relay_port_for` REFUSES rather than answering
            // `None` when the process read that would have found a relay was denied
            // — that refusal is precisely the "absence of evidence" case, so it is
            // translated here into the variant that says so instead of being
            // propagated as a transport error that reads like a broken session.
            (Harness::ClaudeCode, _) => {
                let port = match self.relay_port_for(&s) {
                    Ok(port) => port,
                    Err(LaneError::Transport { detail }) => {
                        return Ok(ReceivePath::Undetermined { reason: detail })
                    }
                    Err(e) => return Err(e),
                };
                Ok(match claude_carrier(port, joined_pane(&s)) {
                    ClaudeCarrier::Relay { .. } | ClaudeCarrier::MuxPty => ReceivePath::Available,
                    ClaudeCarrier::NoLiveReceivePath => ReceivePath::None {
                        reason: format!(
                            "session {:?} has neither a recorded relay port nor a joined mux pane",
                            id.0
                        ),
                    },
                })
            }

            // codex/pane and pi/pane: the pane IS the receive path, so the question
            // is exactly [`joined_pane`] and nothing else. No relay to discover, so
            // no denied read to report — these two can never answer `Undetermined`.
            (Harness::Codex, Mode::Pane) | (Harness::Pi, Mode::Pane) => Ok(if joined_pane(&s) {
                ReceivePath::Available
            } else {
                ReceivePath::None {
                    reason: format!("session {:?} has no live mux pane to type into", id.0),
                }
            }),

            // pi/extension: BOTH halves must hold, and this is the one lane that
            // can be sure of either.
            //
            // The pane gate stays — a session whose pane is gone is not
            // deliverable no matter what a socket says, and a stale socket file
            // outlives the process that bound it. But the pane alone is not
            // enough here: the whole receive path is the channel, and a pi that
            // is up with the extension unloaded (a `--no-extensions` relaunch, a
            // failed jiti compile, a `/reload` that dropped it) has a perfectly
            // live pane and no way to be driven.
            //
            // So this arm HANDSHAKES. It is the only `receive_path` in the file
            // that touches the transport, and the justification is the one the
            // method docs give for NOT doing so elsewhere: there, the refusal
            // belongs to the carrier because the endpoint is not on the
            // `Session` surface and a hoisted refusal would turn a logged
            // `delivery-failed{delivery}` into an envelope-less `refused{...}`.
            // Here the endpoint IS on the row, the probe is a sub-millisecond
            // loopback `connect` + one frame, and answering `Available` for a
            // channel nothing is listening on would be a lie this lane is in a
            // position to avoid telling.
            (Harness::Pi, Mode::Extension) => {
                if !joined_pane(&s) {
                    return Ok(ReceivePath::None {
                        reason: format!("session {:?} has no live mux pane", id.0),
                    });
                }
                let sock = crate::provider::pi::extension::socket_for(
                    self.env,
                    self.current_endpoint(s.pid).as_deref(),
                    &s.session_id,
                );
                Ok(
                    match crate::provider::pi::extension::Client::connect(&sock)
                        .and_then(|mut c| c.hello())
                    {
                        Ok(_) => ReceivePath::Available,
                        Err(e) => ReceivePath::None {
                            reason: format!(
                                "session {:?} has a live pane but its control channel is not \
                                 answering: {e}",
                                id.0
                            ),
                        },
                    },
                )
            }

            // The three resident lanes. See the method docs: their reachability
            // refusal is the carrier's, and it must stay there.
            (Harness::Codex, Mode::Daemon | Mode::AppServer)
            | (Harness::Pi, Mode::Daemon)
            | (Harness::AcpClaudeCode, Mode::Daemon)
            | (Harness::Opencode, Mode::Daemon) => Ok(ReceivePath::Available),

            // The combinations that do not exist, refused in the same
            // vocabulary `start` and `deliver` refuse them.
            (Harness::AcpClaudeCode | Harness::Opencode, Mode::Pane) => {
                Err(LaneError::NotSupported {
                    op: "receive_path".to_string(),
                    reason: "an ACP bridge is a protocol adapter with no terminal of its own"
                        .to_string(),
                })
            }
            // claude is already answered by its `(ClaudeCode, _)` arm above, so
            // only these three reach here.
            (
                Harness::Codex | Harness::AcpClaudeCode | Harness::Opencode,
                Mode::Extension,
            ) => Err(LaneError::NotSupported {
                op: "receive_path".to_string(),
                reason: "the extension lane is pi's alone".to_string(),
            }),
        }
    }

    fn deliver(
        &self,
        id: &SessionId,
        msg: &Message,
        policy: &DeliverPolicy,
    ) -> Result<Receipt, LaneError> {
        let mut s = self.row(id)?;
        let mut woke = Confirmation::No;

        // The cold/live verdict and the wake are ONE step, and the row is re-read
        // from the id afterwards — a revive rewrites the registry row (new pid,
        // new pane) under the SAME session id, so the pre-wake snapshot cannot be
        // used to pick a carrier.
        //
        // The verdict is THIS LANE'S OWN `health`, not the registry string on the
        // row: health is the answer `qd ls` renders, liveness gate included, so a
        // stale-live row — status "idle", pid long gone — is seen as the cold row
        // it is and WOKEN, instead of being handed to a carrier that will fail on
        // a pane nobody is holding. The lane composing its own two reads is not
        // the composition `LaneOps::deliver` forbids; a CALLER composing them is,
        // and doing it in here is precisely what stops one from being able to.
        //
        // `wake_if_cold` IS THE FIRST CONJUNCT, and the order is the decision.
        // With no wake asked for there is nothing `health` could decide, so it is
        // not read — this method has no `LaneError::Cold` to return, and a caller
        // that passes `false` gets an ATTEMPT rather than a refusal off a
        // projection it did not ask about. The two components that answer "is this
        // live" differently, why both are right, and why reconciling them is a
        // separate commit: see the method docs above.
        // `health` RESOLVES A LIVE ROW ONLY, and this method has already resolved
        // the id including tombstones (`self.row(id)?`, above) — so by here the
        // session provably EXISTS, and the only thing a `NotFound` out of `health`
        // can mean is "no live row", which is a STOPPED session. That is precisely
        // the state `qd stop` leaves behind and precisely the state the unified
        // send calls a WAKE TRIGGER rather than a refusal (`send_unified.rs`: "a
        // stopped/tombstoned target is NO LONGER rejected here ... It is a WAKE
        // trigger"). Propagating it answered `NotFound` — "no such session" — for a
        // row the very next line is able to revive, and left `deliver` the one
        // method that disagreed with `wake` and `kill` about whether a tombstone is
        // a row. See `row_for_id`'s TOMBSTONES COUNT note for the other half.
        //
        // EVERY OTHER ERROR STILL PROPAGATES, and that asymmetry is the decision: a
        // denied or unreadable registry must not be read as "cold" and silently
        // fire a revive at a session that may well be running.
        let wake_needed = if policy.wake_if_cold {
            match self.health(id) {
                Ok(h) => !is_deliverable(h.status),
                Err(LaneError::NotFound { .. }) => true,
                Err(e) => return Err(e),
            }
        } else {
            // `wake_if_cold` IS STILL THE FIRST CONJUNCT and `health` is still not
            // read without one — the short-circuit moved into the `if`, it did not
            // go away.
            false
        };
        if wake_needed {
            // No `--cwd` here: a delivery-time revive has no CLI override to
            // honour. That is `None` as an ANSWER, not a placeholder — the flag
            // belongs to `qd resume`, and inventing one from the row's recorded
            // cwd would re-resolve a decision the revive already makes.
            self.wake(id, policy.render, None)?;
            // `wake` re-resolved the row and found it, which is an OBSERVED
            // revive — `Yes`, not `Unknown`. `Confirmation::Unknown` belongs to a
            // lane that signals a revive it cannot then confirm.
            woke = Confirmation::Yes;
            s = self.row(id)?;
        }

        // The carrier, per lane. Seven arms, and the routing is the LANE — never
        // `session.provider.as_str()` plus a `row_hosting` re-derivation, which is
        // what the retired `select_carrier` did, and what put its `"codex"` and
        // `"pi"` arms one guard away from silently routing a pane row into a
        // daemon carrier that has no endpoint to reach.
        let outcome = match (self.lane.harness, self.lane.mode) {
            // --- the four harnesses with no app-server residence ---------------
            // Placed FIRST so it also shadows the `(Harness::ClaudeCode, _)` arm
            // below: a hand-built `claude-code/app-server` must be REFUSED, not
            // quietly run through claude's own machinery. See
            // [`APP_SERVER_IS_CODEX_ONLY`].
            (
                Harness::ClaudeCode | Harness::Pi | Harness::AcpClaudeCode | Harness::Opencode,
                Mode::AppServer,
            ) => return Err(LaneError::NotSupported {
                op: "deliver".to_string(),
                reason: APP_SERVER_IS_CODEX_ONLY.to_string(),
            }),
            // claude/pane: the one lane with a CHOICE, and it is private. See
            // [`claude_carrier`] for the precedence and [`LaneImpl::relay_port_for`]
            // for why the port cannot simply be read off the row.
            (Harness::ClaudeCode, _) => {
                match claude_carrier(self.relay_port_for(&s)?, joined_pane(&s)) {
                    ClaudeCarrier::Relay { port } => {
                        let relay_paths = crate::delivery::relay::relay_paths(self.env);
                        let client = crate::provider::claude::relay::http::CcRelay::new();
                        let deps = crate::delivery::relay::RelaySendDeps {
                            env: self.env,
                            paths: &relay_paths,
                            clock: &crate::effects::RealClock,
                            relay: &client,
                            relay_port: port,
                        };
                        // `"send"` — this carrier is reached ONLY from the unified
                        // `qd send`, which is the command the user typed.
                        crate::delivery::render(
                            crate::delivery::relay::send_claude_relay(
                                &deps,
                                &crate::delivery::SendParams {
                                    session: &s,
                                    message: &msg.text,
                                    send_id: &msg.id.0,
                                },
                            ),
                            "send",
                        )
                    }
                    ClaudeCarrier::MuxPty => self.deliver_mux_pty(&s, &msg.text, &msg.id.0),
                    ClaudeCarrier::NoLiveReceivePath => {
                        return Err(no_live_receive_path(
                            id,
                            &woke,
                            format!(
                                "session {:?} has neither a recorded relay port nor a joined \
                                 mux pane",
                                id.0
                            ),
                        ))
                    }
                }
            }

            // codex/pane and pi/pane: the SAME carrier claude's relay-less pane
            // takes. A `--interactive` row has no ws endpoint to reconnect to —
            // its receive path IS the pane's PTY — and the PTY carrier genuinely
            // delivers for both: codex publishes no busy/idle signal but its
            // rollout records the submitted message, and pi's transcript is
            // append-per-entry once flushed, so acceptance is confirmed from the
            // transcript (`AcceptanceSignal::Landing`) either way rather than from
            // a status neither publishes.
            (Harness::Codex, Mode::Pane) | (Harness::Pi, Mode::Pane) => {
                if !joined_pane(&s) {
                    return Err(no_live_receive_path(
                        id,
                        &woke,
                        format!("session {:?} has no live mux pane to type into", id.0),
                    ));
                }
                self.deliver_mux_pty(&s, &msg.text, &msg.id.0)
            }

            // pi/extension: the reason the lane exists.
            //
            // The pane sibling above types the message into a PTY and then reads
            // the transcript to decide whether it landed. This asks, and is
            // answered: `pi.sendUserMessage` either accepts — a real user turn,
            // indistinguishable from a typed one because it IS one — or it
            // returns an error frame naming the refusal. There is no landing
            // probe, no lazy-write window to wait out, and no inference.
            //
            // The pane gate is kept even so. It is not redundant with the
            // socket: a stale socket file outlives the process that bound it,
            // and `connect(2)` on one fails with a transport error that reads as
            // "the channel broke" when the truth is "the session is gone".
            // Checking the pane first makes the common cold-session case say so.
            (Harness::Pi, Mode::Extension) => {
                if !joined_pane(&s) {
                    return Err(no_live_receive_path(
                        id,
                        &woke,
                        format!("session {:?} has no live mux pane", id.0),
                    ));
                }
                let sock = crate::provider::pi::extension::socket_for(
                    self.env,
                    self.current_endpoint(s.pid).as_deref(),
                    &s.session_id,
                );
                // NOTE: nothing here writes to stdout. Under `qw serve` stdout IS
                // the protocol, and a stray line corrupts the frame the caller is
                // mid-read on. Notes go to stderr, which is inherited.
                match crate::provider::pi::extension::Client::connect(&sock)
                    .and_then(|mut c| c.deliver(&msg.text, None).map(|()| c))
                {
                    Ok(_) => {
                        // THE LEDGER. Without this the send is invisible to
                        // `await_terminal` and `recover`: both join on
                        // `Payload::SendInitiated.send_id` in the RECIPIENT's
                        // log, so a carrier that delivers without writing it
                        // hands back a `MessageId` that addresses nothing and
                        // every wait on it times out.
                        //
                        // `emit_daemon_send_events` is the right shape rather
                        // than the pty emitter's: this is an ACCEPTED turn on a
                        // resident-like front, not a chunked keystroke delivery
                        // with a transcript to recover from. It writes
                        // `send-initiated` + `turn-accepted` (delivered,
                        // NON-terminal), which is honest — the turn is running,
                        // and its terminal lands at the observation seam.
                        //
                        // Its doc's recovery note holds here for the same
                        // reason it holds for the resident lanes: verb
                        // `send:relay` is outside `delivery:recover`'s
                        // {send:pty, new-p} sweep, so this can never be
                        // mistaken for a transcript-recoverable pty dangling.
                        crate::delivery::emit_daemon_send_events(
                            self.env,
                            &crate::effects::RealClock,
                            s.name.as_deref().unwrap_or(&s.session_id),
                            Some(&s),
                            &msg.text,
                            &msg.id.0,
                            "pi/extension",
                        );
                        CarrierOutcome::keyed(0, msg.id.0.clone())
                    }
                    Err(e) => {
                        eprintln!("qd send:relay: {e}");
                        // KEYED even though it failed. The id was minted before
                        // this arm ran, and a later `recover` needs something to
                        // search for — the same reason every other carrier keys
                        // its failures.
                        CarrierOutcome::keyed(1, msg.id.0.clone())
                    }
                }
            }

            // The three resident lanes. No pane gate and no relay question: each
            // carrier re-reads the row's recorded endpoint by pid and answers its
            // own "not reachable" refusal, which is the honest place for it —
            // the endpoint is not on the `Session` surface for this lane to check.
            //
            // `"send:relay"` on all four daemon lines below, from BOTH callers.
            // That is what the pre-move bodies hard-coded — `qd send:relay` and
            // the unified `qd send` alike — and it is preserved rather than
            // corrected here, because correcting it moves bytes a dozen pinned
            // tests read. It is a REPORTED finding, not an accepted one.
            // The app-server lane delivers over the SAME ws endpoint. An attached
            // human viewer changes nothing here: both are clients of one server
            // driving one thread, which is the whole premise of the lane.
            (Harness::Codex, Mode::Daemon | Mode::AppServer) => crate::delivery::render(
                crate::delivery::codex::send_codex(
                    &crate::delivery::SendDeps {
                        env: self.env,
                        paths: &self.paths,
                        clock: &crate::effects::RealClock,
                    },
                    &crate::delivery::SendParams {
                        session: &s,
                        message: &msg.text,
                        send_id: &msg.id.0,
                    },
                ),
                "send:relay",
            ),
            (Harness::Pi, Mode::Daemon) => crate::delivery::render(
                crate::delivery::pi::send_pi(
                    &crate::delivery::pi::PiSendDeps {
                        env: self.env,
                        paths: &self.paths,
                        clock: &crate::effects::RealClock,
                        // The floor sub-lane's one-shot child inherits this when
                        // the row records no cwd — the same fallback `resolve`
                        // takes at :1806.
                        fallback_cwd: std::env::current_dir()
                            .unwrap_or_else(|_| PathBuf::from(".")),
                    },
                    &crate::delivery::SendParams {
                        session: &s,
                        message: &msg.text,
                        send_id: &msg.id.0,
                    },
                ),
                "send:relay",
            ),
            // ONE carrier, two lanes: `run_acp_send` takes the provider off the
            // row, and `acp_loss::preserve_identity` self-gates on it, so
            // acp/opencode keeps its byte-identical plain refusal. Written as two
            // arms anyway — the seven-line table is the thing that replaced the
            // provider-string chain, and collapsing a row to save a line starts it
            // back down that road.
            (Harness::AcpClaudeCode, Mode::Daemon) => crate::delivery::render(
                crate::delivery::acp::send_acp(
                    &crate::delivery::SendDeps {
                        env: self.env,
                        paths: &self.paths,
                        clock: &crate::effects::RealClock,
                    },
                    &crate::delivery::SendParams {
                        session: &s,
                        message: &msg.text,
                        send_id: &msg.id.0,
                    },
                ),
                "send:relay",
            ),
            (Harness::Opencode, Mode::Daemon) => crate::delivery::render(
                crate::delivery::acp::send_acp(
                    &crate::delivery::SendDeps {
                        env: self.env,
                        paths: &self.paths,
                        clock: &crate::effects::RealClock,
                    },
                    &crate::delivery::SendParams {
                        session: &s,
                        message: &msg.text,
                        send_id: &msg.id.0,
                    },
                ),
                "send:relay",
            ),

            // --- the combinations that do not exist -------------------------
            // The same set [`LaneOps::start`] refuses, for the same reason: they
            // are unreachable except by constructing a `Lane` around `Lane::new`.
            (Harness::AcpClaudeCode | Harness::Opencode, Mode::Pane) => {
                return Err(LaneError::NotSupported {
                    op: "deliver".to_string(),
                    reason: "an ACP bridge is a protocol adapter with no terminal of its own"
                        .to_string(),
                })
            }
            (
                Harness::Codex | Harness::AcpClaudeCode | Harness::Opencode,
                Mode::Extension,
            ) => {
                return Err(LaneError::NotSupported {
                    op: "deliver".to_string(),
                    reason: "the extension lane is pi's alone".to_string(),
                })
            }
        };

        match outcome.message_id {
            Some(mid) => Ok(Receipt {
                message_id: MessageId(mid),
                accepted: outcome.code == 0,
                terminal: TerminalExpectation::Pending,
                woke,
            }),
            None => Err(LaneError::Transport {
                detail: format!(
                    "the carrier refused before minting a message id (exit {})",
                    outcome.code
                ),
            }),
        }
    }

    /// Block until this message reaches a terminal, or the budget elapses.
    ///
    /// # Why this is ONE body and not seven
    ///
    /// Every lane's `deliver` mints a [`MessageId`] into the SAME namespace —
    /// `Payload::SendInitiated.send_id` in the RECIPIENT's
    /// `<state>/sessions/<uuid>.events.jsonl` — because that is the join key the
    /// whole terminal apparatus already uses ([`crate::sendpty::watch_terminal`],
    /// [`crate::events::recovery_read`], the `TERMINAL_EVENTS` first-terminal-wins
    /// rule). See [`CarrierOutcome`], which says exactly this about the ids the
    /// five carriers hand back. So "has a terminal arrived for this id" is answered
    /// from one ledger, and [`crate::events::await_received`] is the §8 function
    /// that answers it. Writing seven near-identical watches over one file would be
    /// the reimplementation the stage-2 rule forbids, not uniformity.
    ///
    /// What IS per-lane rides in [`LaneRecoveryDeps`]: the §7 dead-dangling check
    /// inside the poll runs recovery-read inline, and recovery-read has to RESOLVE
    /// the recipient's transcript — which is a per-harness layout, and the one
    /// place a lane genuinely differs here.
    ///
    /// # What this deliberately is NOT
    ///
    /// It is not `qd wait`. `run_wait`'s four per-provider bodies answer "has this
    /// SESSION gone idle" and return an exit code; this answers "did THIS message
    /// reach a terminal" and returns which one. The two questions have different
    /// subjects, and mapping `qd wait`'s exit 0 onto [`Terminal::Seen`] would claim
    /// a message was seen on evidence that never mentioned it. `qd wait` folding in
    /// here is still open work — see `doc/tbd/provider-architecture/06-stage2-plan.md`.
    ///
    /// # It is a READ. It is deliberately NOT [`crate::events::await_received`].
    ///
    /// This body used to call `await_received`, and that was wrong in a way only
    /// wiring a caller could reveal (stage-3 phase 3A). `await_received` is a
    /// **writer**: on budget exhaustion it emits `anchor-timeout` — a member of the
    /// terminal set — and each poll runs recovery-read inline, which can emit
    /// `pending-abandoned`.
    ///
    /// Terminals resolve FIRST-WINS. So a caller passing a 75s budget against a
    /// session whose mux resolves the send at 80s would mint a foreclosing terminal
    /// at 75s and beat the real one to the ledger. The send is delivered; the
    /// ledger says it timed out. That is precisely the false verdict this whole
    /// apparatus exists to prevent, and three separate places in this codebase
    /// already say so:
    ///
    /// - [`Terminal::TimedOut`]'s own doc — "says nothing about delivery."
    /// - `crate::sendpty`'s `watch_terminal`, the function this method replaces on
    ///   the `qd send:pty --wait` path, whose header says it is "deliberately NOT
    ///   `events::await_received` — that helper is a WRITER … which would make qd
    ///   mint a terminal for a mux-held send and break the single-writer split."
    /// - `ack2_gate::g3_seq_sendpty_wait_timeout_no_foreclosing_terminal`, which
    ///   forbids exactly this sequence on the ZMX path.
    ///
    /// The general rule, worth stating because the next method to cross will meet
    /// it too: **a budget belongs to the caller; the ledger belongs to the
    /// session.** An observer giving up is a fact about the observer. Only the
    /// watch that armed the send — the writer that promised to resolve it — may
    /// close it out, and inline recovery belongs to [`LaneOps::recover`], which has
    /// the liveness fence this method does not.
    ///
    /// `await_received` keeps its emitting behaviour for its own callers and its
    /// own tests; it simply is not this method's implementation. Its
    /// `Received::BudgetExhausted` doc already anticipated a non-emitting caller
    /// ("a budget-only caller without emission would see BudgetExhausted").
    fn await_terminal(
        &self,
        id: &SessionId,
        message_id: &MessageId,
        budget_ms: u64,
    ) -> Result<Terminal, LaneError> {
        let clock = crate::effects::RealClock;
        let (name, cwd) = self.ledger_row(id);
        // Kept ONLY for its sleeper — see the read-only note below. `LaneRecoveryDeps`
        // is the poll cadence's seam, which is why tests can run this instantly.
        let deps = self.recovery_deps(cwd, &clock);
        let ctx = crate::events::ReaderCtx {
            state_dir: &self.paths.state_dir,
            session_id: Some(&id.0),
            name: name.as_deref(),
        };
        // The contract's budget is wall-clock ms; §8's is (cadence, polls). Round
        // UP so a sub-cadence budget still gets one look rather than none — a
        // caller asking for 100ms is asking to be told what the ledger says now,
        // not to be told nothing.
        let max_polls = budget_ms.div_ceil(AWAIT_POLL_MS).max(1);
        for poll in 0..max_polls {
            let merged = ctx.read();
            if let Some(term) = crate::events::first_terminal_for(&merged.records, &message_id.0) {
                return Ok(terminal_from_received(
                    crate::events::received_from_terminal(&term),
                ));
            }
            // No sleep after the last look — the budget bounds waiting, not looking.
            if poll + 1 < max_polls {
                crate::events::AwaitDeps::sleep(&deps, AWAIT_POLL_MS);
            }
        }
        Ok(Terminal::TimedOut)
    }

    /// Resolve a send that has no terminal, by searching the recipient's own
    /// transcript. The **search half** of the dead-writer rule; the FENCE half
    /// stays in qd.
    ///
    /// That split is [`LaneOps::recover`]'s own contract and it is load-bearing in
    /// both directions. qd can evaluate its own record's age and whether its own
    /// writing incarnation is gone ([`crate::events::is_dead_dangling`]) without
    /// reading any session artifact, so that stays there; it CANNOT parse a
    /// harness's transcript, so the search crosses. This method therefore does not
    /// re-check the fence — it is called ONLY on a send its caller has already
    /// proved dead-dangling, and calling it on a live-writer send would append a
    /// premature terminal, which is the QS-1 violation `verbs/recover.rs`'s own
    /// module docs open with.
    ///
    /// Idempotence and the cross-process emit lock are
    /// [`crate::events::emit_recovery_verdict`]'s, unchanged: it re-reads under an
    /// exclusive flock and ADOPTS a terminal that raced in rather than writing a
    /// second one. A verdict that mints no terminal — the two undetermined states —
    /// leaves the send dead-dangling for a later pass and is reported as
    /// [`Terminal::Undetermined`], never as a negative. See [`terminal_from_verdict`].
    ///
    /// A message id with no `send-initiated` in this session's ledger is
    /// `Undetermined` too, and for the same reason: there is nothing to search
    /// FROM, so there is nothing to conclude. An absent anchor is not an absent
    /// delivery.
    fn recover(
        &self,
        at: &LedgerAddress,
        message_id: &MessageId,
    ) -> Result<Terminal, LaneError> {
        let clock = crate::effects::RealClock;
        // A session-addressed send can join its registry row for the name and the
        // cwd; a byname-only send has no row, so both stay as the address gave
        // them. The cwd is what `resolve_transcript` needs, so its absence is
        // precisely why a byname send answers Undetermined rather than a negative
        // — see LedgerAddress.
        let (name, cwd) = match &at.session {
            Some(id) => {
                let (row_name, cwd) = self.ledger_row(id);
                (row_name.or_else(|| at.name.clone()), cwd)
            }
            None => (at.name.clone(), None),
        };
        let deps = self.recovery_deps(cwd, &clock);
        let Some(writer_key) = at.writer_key() else {
            return Ok(Terminal::Undetermined {
                reason: "ledger address names neither a session nor a target".to_string(),
            });
        };
        let writer = crate::events::EventWriter::for_key(
            &self.paths.state_dir,
            &writer_key,
            at.session.as_ref().map(|s| s.0.clone()),
            name.clone(),
        );
        let ctx = crate::events::ReaderCtx {
            state_dir: &self.paths.state_dir,
            session_id: at.session.as_ref().map(|s| s.as_str()),
            name: name.as_deref(),
        };
        let merged = ctx.read();
        let Some(si) = crate::events::send_initiated_for(&merged.records, &message_id.0) else {
            return Ok(Terminal::Undetermined {
                reason: format!(
                    "no send-initiated record for {:?} in this session's ledger",
                    message_id.0
                ),
            });
        };
        crate::events::emit_recovery_verdict(&deps, &writer, &clock, ctx, &si)
            .map(|v| terminal_from_verdict(&v))
            .map_err(|detail| LaneError::Transport { detail })
    }

    /// Clause (a), as a pure read of qw's own log.
    ///
    /// Deliberately the SAME merged read [`LaneOps::recover`] does — the
    /// `(session?, name?)` pair through `events::reader_paths`, not one file —
    /// because a send initiated before its session id resolved has its
    /// `send-initiated` under `byname-<name>` and its terminal under the uuid.
    /// Reading one key would answer "unresolved" for a send that is resolved, and
    /// the caller would then hand it to `recover`, which is the outcome this
    /// method exists to prevent.
    ///
    /// The terminal is mapped through the same `received_from_terminal` pair
    /// [`LaneOps::await_terminal`] uses, so "this send is seen" reads the same
    /// whichever method asked.
    fn resolved(
        &self,
        at: &LedgerAddress,
        message_id: &MessageId,
    ) -> Result<Option<Terminal>, LaneError> {
        let name = match &at.session {
            Some(id) => self.ledger_row(id).0.or_else(|| at.name.clone()),
            None => at.name.clone(),
        };
        let ctx = crate::events::ReaderCtx {
            state_dir: &self.paths.state_dir,
            session_id: at.session.as_ref().map(|s| s.as_str()),
            name: name.as_deref(),
        };
        let merged = ctx.read();
        Ok(
            crate::events::first_terminal_for(&merged.records, &message_id.0).map(|term| {
                terminal_from_received(crate::events::received_from_terminal(&term))
            }),
        )
    }

    /// Has this SESSION gone idle? Ruling D2's method, and the last shared-file
    /// path across the split.
    ///
    /// # Five bodies, not one — the opposite of `await_terminal`
    ///
    /// [`LaneOps::await_terminal`] is ONE body for seven lanes because every
    /// lane's terminals land in one ledger under one join key. Idleness has no
    /// such common source: claude reads a pid file plus a transcript tail, codex
    /// reads `thread/status/changed` or a rollout tail, ACP pulls `next_update`
    /// over the residence socket, pi's resident POINT-READS `is_streaming`, and
    /// pi's extension subscribes to a pushed `idle` frame. Writing one watch over
    /// five incompatible sources would be the invention, not the uniformity — so
    /// this routes, and [`crate::idle`] holds the bodies.
    ///
    /// # It routes on the LANE, and it did not use to — that was P2
    ///
    /// This match keyed on `self.lane.harness` alone, inheriting the shape of the
    /// verb it came out of: `run_wait` routed on the provider STRING
    /// (`provider == "codex"`, `provider.starts_with("acp/")`, `provider == "pi"`,
    /// else claude) and was mode-blind by construction. It was the last method in
    /// this file still routing that way while `start`, `wake`, `kill` and
    /// `deliver` had all moved to `(harness, mode)` — and it is the worked example
    /// of why they did. A harness-keyed match has no arm to forget, so a lane it
    /// cannot serve does not fail to compile; it compiles into a wrong answer:
    ///
    ///   - **`pi/extension`** — every `qd wait` on the lane died
    ///     `"pi endpoint not reachable at wait entry"`, because `await_idle_pi`'s
    ///     identity gate requires a cmdline that is our `pi-daemon` and a pi TUI's
    ///     never is. The lane had a working, unit-tested idle RPC of its own the
    ///     whole time ([`crate::provider::pi::extension::Client::await_idle`]);
    ///     there was simply no arm in which to name it.
    ///   - **`pi/mux-pane`** — the same failure, and NOT a regression introduced
    ///     by any default switch: it has been broken since this method existed.
    ///     It is now a typed refusal that says what it cannot do; see
    ///     [`PI_PANE_HAS_NO_IDLE_SOURCE`].
    ///
    /// The three non-pi harnesses keep a wildcard over mode, and each keeps it for
    /// a stated reason rather than by inheritance — see the arms. What the lane
    /// key buys even so is that adding a MODE to any of them is now a visible
    /// edit to this match instead of a silent inheritance of another lane's
    /// watcher.
    ///
    /// # The row is qw's, and where that differs from the join
    ///
    /// [`row_for_id`] resolves the row, so `name`/`pid`/`status`/`jsonl_path`
    /// come from the registry rather than from qd's cross-backend join. For a
    /// CLAUDE row — the only arm that reads `status` — the two agree: the join
    /// derives it as `ClaudeProvider::parse_status(row.status)` with an `Idle`
    /// fallback, which is what `row_for_id` computes. `name` can differ in one
    /// case: the join falls back to the TRANSCRIPT's name when the registry row
    /// carries none, and that name reaches only the progress prefix and the
    /// republish socket's name gate. A session created by `qd new` always records
    /// its name, so the divergence needs a row discovered from a transcript alone.
    fn await_idle(&self, id: &SessionId, budget_ms: u64) -> Result<TurnState, LaneError> {
        // No row is the same fact as no pid — there is no live process to wait on
        // — and it renders identically. Saying `NotFound` instead would hand the
        // verb a class it has no line for.
        let Some(session) = row_for_id(&self.paths, self.env, self.mux.as_deref(), id) else {
            return Err(LaneError::Cold { id: id.clone() });
        };
        let label = session
            .name
            .clone()
            .unwrap_or_else(|| quorum_core::fmt::truncate_id_default(&session.session_id));
        match (self.lane.harness, self.lane.mode) {
            // claude: one watcher over every claude mode, and only one claude
            // mode exists — `Harness::ClaudeCode.supports(Mode::Daemon)` is
            // `false` by construction and DEC-1 (`16-default-lane-switch.md`)
            // retired the relay lane that would have been the second. This is
            // the same `(Harness::ClaudeCode, _)` wildcard `deliver`, `wake`,
            // `kill` and `lane_read::health_for` carry, kept in step with them
            // deliberately: if a second claude mode is ever built, all five
            // split together or none of them do.
            (Harness::ClaudeCode, _) => {
                crate::idle::await_idle_claude(self.env, &self.paths, &session, &label, budget_ms)
            }

            // codex: one watcher over BOTH topologies, and it is right rather
            // than merely compatible. A codex row's busy/idle derives
            // CONNECTIONLESSLY from the rollout tail (`join.rs`'s
            // `codex_status_for`), and a `--interactive` TUI, a daemon thread and
            // an app-server thread all write that same rollout. There is no
            // socket in this path for a mode to change.
            (Harness::Codex, _) => {
                crate::idle::await_idle_codex(self.env, &self.paths, &session, &label, budget_ms)
            }

            // ACP: one watcher over both bridges. `Mode::Daemon` is the only mode
            // either ACP harness supports — `--interactive` is refused for them
            // at create — so the wildcard covers exactly one lane apiece.
            (Harness::AcpClaudeCode, _) | (Harness::Opencode, _) => {
                crate::idle::await_idle_acp(self.env, &self.paths, &session, &label, budget_ms)
            }

            // pi/daemon — the lane `await_idle_pi` was written for, and the only
            // one it can serve: its gate demands a pid whose cmdline is our
            // `pi-daemon`, which is true here and false for both siblings below.
            (Harness::Pi, Mode::Daemon) => {
                crate::idle::await_idle_pi(self.env, &self.paths, &session, &label, budget_ms)
            }

            // pi/extension — the lane's own idle RPC, over the control channel
            // its `deliver` already drives. This arm is the fix P2 named.
            (Harness::Pi, Mode::Extension) => crate::idle::await_idle_pi_extension(
                self.env,
                &self.paths,
                &session,
                &label,
                budget_ms,
            ),

            // pi/mux-pane — a REFUSAL, and the honest one. See
            // [`PI_PANE_HAS_NO_IDLE_SOURCE`] for why a transcript-tail wait was
            // the wrong answer here even though the transcript is where this
            // lane's `health` comes from.
            (Harness::Pi, Mode::Pane) => Err(LaneError::NotSupported {
                op: "await_idle".to_string(),
                reason: PI_PANE_HAS_NO_IDLE_SOURCE.to_string(),
            }),

            // pi/app-server does not exist. Same refusal `deliver` answers for the
            // three non-codex harnesses, for the same reason and in the same
            // words — see [`APP_SERVER_IS_CODEX_ONLY`]. Unconstructable through
            // `Lane::new`; present because the match is over the type, not over
            // `Lane::ALL`.
            (Harness::Pi, Mode::AppServer) => Err(LaneError::NotSupported {
                op: "await_idle".to_string(),
                reason: APP_SERVER_IS_CODEX_ONLY.to_string(),
            }),
        }
    }

    /// Fully qw-native: performs the handoff directly, for both mux backends.
    /// See [`LaneOps::attach`] for why this returns an exit code rather than a
    /// plan. A daemon-hosted session has no terminal of its own.
    fn attach(&self, id: &SessionId) -> Result<i32, LaneError> {
        // The one daemon lane that attaches. It does not hand over the session's
        // terminal — it opens a SECOND CLIENT on the session's app server and
        // hands over that. See [`LaneImpl::attach_codex_viewer`].
        if self.lane.is_app_server() {
            return self.attach_codex_viewer(id);
        }
        let (zmx_name, dir) = self.attach_target(id)?;
        let zmx_name = zmx_name.as_str();

        let mux = self.mux.as_deref().ok_or_else(|| LaneError::Transport {
            detail: "could not resolve the mux backend".to_string(),
        })?;
        mux.attach(&dir, zmx_name)
            .map_err(|e| LaneError::Transport {
                detail: format!("attach to pane {zmx_name:?} failed: {e}"),
            })
    }
}

// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    struct E;
    impl Env for E {
        fn var(&self, _: &str) -> Option<String> {
            None
        }
        fn uid(&self) -> u32 {
            0
        }
    }

    fn lane_for_test(lane: Lane) -> (QdPaths, Lane) {
        (
            QdPaths::from_home(std::path::Path::new("/nonexistent")),
            lane,
        )
    }

    #[test]
    fn every_lane_resolves_and_reports_itself() {
        for lane in Lane::ALL {
            let (p, _) = lane_for_test(lane);
            let ops = lane_ops(lane, &E, p);
            assert_eq!(ops.lane(), lane);
        }
    }

    /// Attach refusal must track attachability EXACTLY in both directions: a pane
    /// lane refusing attach would be a bug, since attach is the point of a pane.
    ///
    /// **`codex/app-server` is the deliberate exception, and this is where it is
    /// pinned.** It is `is_daemon()` — every other path must treat it as one —
    /// but it does NOT refuse attach, because its residence is a server a second
    /// client can join. The predicate is therefore `is_daemon() &&
    /// !is_app_server()`, and spelling it that way here is the point: if someone
    /// later makes `is_daemon()` false for it to "fix" attach, this test still
    /// passes while the lane silently starts taking every PANE branch in the
    /// file. The two halves are asserted separately below so that cannot happen.
    #[test]
    fn only_unattachable_daemon_lanes_refuse_attach() {
        let ghost = SessionId("no-such-session".into());
        for lane in Lane::ALL {
            let (p, _) = lane_for_test(lane);
            let ops = lane_ops(lane, &E, p);
            let refused = matches!(ops.attach(&ghost), Err(LaneError::NotSupported { .. }));
            assert_eq!(
                refused,
                lane.is_daemon() && !lane.is_app_server(),
                "{lane}"
            );
        }
    }

    /// The half the test above cannot see: `codex/app-server` must remain a
    /// DAEMON everywhere except attach.
    ///
    /// Separated on purpose. `only_unattachable_daemon_lanes_refuse_attach` is
    /// satisfied by making `is_daemon()` false for this lane — which would be the
    /// wrong fix and a silent one, since it would reroute kill, receive_path and
    /// every hosting-keyed branch in the crate onto their pane arms. This asserts
    /// the property that fix would break.
    #[test]
    fn the_app_server_lane_is_a_daemon_everywhere_but_attach() {
        let lane = Lane::new(Harness::Codex, Mode::AppServer).unwrap();
        assert!(lane.is_daemon(), "app-server must stay daemon-hosted");
        assert!(!lane.is_pane(), "app-server is not a pane lane");
        assert!(lane.is_app_server());
        // And it is the ONLY lane for which the two disagree.
        for l in Lane::ALL {
            if l != lane {
                assert!(
                    !l.is_app_server(),
                    "{l} must not claim an app-server residence"
                );
            }
        }
    }

    /// **`Cold` means "a wake would fix this" — and nothing else.**
    ///
    /// A source scan, because the property is about which VARIANT each refusal in
    /// [`LaneImpl::viewer_target`] reaches, and the alternative is standing up a
    /// registry row plus a live app server per case to observe it.
    ///
    /// This exists because the distinction was got WRONG and shipped. Every
    /// refusal was `Cold`, `verbs/attach.rs` maps `Cold` to `wake_then_attach`,
    /// and a `qd attach` on a freshly started session produced:
    ///
    /// ```text
    /// Revived "test-37347"; attaching...
    /// qd attach: session 01a01638-… is not live
    /// ```
    ///
    /// — a pointless revive of a running session, then "is not live" about a live
    /// one, because the retry hit the identical refusal. The user's actual remedy
    /// (send it a turn) appeared nowhere.
    ///
    /// So: exactly ONE `Cold` in that function, guarding the dead-endpoint case,
    /// which is the only one a revive repairs.
    #[test]
    fn viewer_target_says_cold_only_where_a_wake_would_help() {
        let src = include_str!("lanes.rs");
        let start = src
            .find("fn viewer_target(&self")
            .expect("viewer_target exists");
        // The body ends at the next `fn ` at method indentation.
        let end = start
            + src[start..]
                .find("\n    /// This codex row's rollout file")
                .expect("viewer_target is followed by codex_rollout_path");
        let body = &src[start..end];

        // The CONSTRUCTION, not the mention — the doc block above the guards
        // explains `Cold` by name and must not be counted as a use of it.
        assert_eq!(
            body.matches("LaneError::Cold { id:").count(),
            1,
            "viewer_target must reach `Cold` exactly once — qd turns it into a \
             revive-and-retry, so a `Cold` on a condition no wake repairs spins \
             the user through a pointless revive and then reports \"is not live\" \
             about a session that is live"
        );
        // And it must be the DEAD-SERVER guard that owns it.
        let cold_at = body.find("LaneError::Cold { id:").unwrap();
        let liveness_at = body
            .find("!self.app_server_is_live(&s)")
            .expect("the dead-server guard exists");
        assert!(
            cold_at > liveness_at && cold_at - liveness_at < 400,
            "the one `Cold` must belong to the dead-server guard — that is the \
             only refusal here a wake can repair"
        );
        // It must ask LIVENESS, not "is the endpoint field populated". Nothing
        // rewrites the row when the daemon dies, so a field check passes for a
        // corpse and the viewer spawns against a dead socket — the same dead-pane
        // class the turn-zero guard exists to prevent.
        assert!(
            !body.contains("self.current_endpoint(s.pid).is_none()"),
            "the dead-server guard must not be a field-presence check: a killed \
             app server still leaves its `endpoint` string on the row"
        );
        // The turn-zero refusal must name the remedy, since no wake supplies it.
        assert!(
            body.contains("has not taken a turn yet"),
            "the turn-zero refusal must say so in words"
        );
        assert!(
            body.contains("qd send"),
            "the turn-zero refusal must name the remedy that actually works"
        );
    }

    // --- Defect A: the mux join must scan the SELECTED backend's dirs ------

    /// A mux that answers panes for EXACTLY ONE socket dir and refuses every
    /// other. That refusal is what makes the test discriminating: a caller that
    /// asks the wrong dir gets an empty answer, not a lucky hit.
    struct OneDirMux {
        dir: PathBuf,
        pane: String,
    }

    impl crate::mux::Mux for OneDirMux {
        fn list(&self, socket_dir: &std::path::Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
            if socket_dir != self.dir {
                return Err(std::io::Error::other(format!(
                    "no mux here: {}",
                    socket_dir.display()
                )));
            }
            Ok(vec![crate::mux::MuxSession {
                name: self.pane.clone(),
                pid: 4242,
                clients: 0,
                created: 0,
                start_dir: "/w".to_string(),
                cmd: "claude".to_string(),
                current: false,
                // The real embedded list tags this; leaving it None also exercises
                // the fallback that fills it from the dir the pane was FOUND in.
                socket_dir: None,
                ended: None,
                exit_code: None,
                zmx_status: None,
                err: None,
            }])
        }
        fn list_raw(&self, socket_dir: &std::path::Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
            self.list(socket_dir)
        }
        fn run_detached(
            &self,
            _: &std::path::Path,
            _: &str,
            _: &str,
            _: &std::path::Path,
        ) -> std::io::Result<crate::exec::ExecResult> {
            unreachable!("attach never launches")
        }
        fn send(&self, _: &std::path::Path, _: &str, _: &str) -> std::io::Result<crate::exec::ExecResult> {
            unreachable!("attach never sends")
        }
        fn kill(&self, _: &std::path::Path, _: &str) -> std::io::Result<i32> {
            unreachable!("attach never kills")
        }
        fn history(&self, _: &std::path::Path, _: &str) -> std::io::Result<String> {
            unreachable!("attach never reads history")
        }
        fn wait(&self, _: &std::path::Path, _: &[String]) -> std::io::Result<i32> {
            unreachable!("attach never waits")
        }
        fn attach(&self, socket_dir: &std::path::Path, name: &str) -> std::io::Result<i32> {
            assert_eq!(socket_dir, self.dir, "attach targets the dir the pane was found in");
            assert_eq!(name, self.pane);
            // A distinctive code, so "attach ran" cannot be confused with a
            // default-0 success invented somewhere else.
            Ok(7)
        }
    }

    /// Forge one live registry row under a jailed home and hand back the paths +
    /// env the lane resolves from. `XDG_RUNTIME_DIR` is a SHORT literal so the
    /// qrmux dir is deterministic and passes the `sun_path` budget on any host;
    /// `ZMX_DIR` is a DIFFERENT literal, which is the whole point — the two dirs
    /// must not coincide or the defect would be unobservable.
    fn jailed_row(home: &std::path::Path, name: &str, sid: &str) -> (QdPaths, crate::effects::MapEnv) {
        let paths = QdPaths::from_home(home);
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        std::fs::write(
            paths.sessions_dir.join("4242.json"),
            format!(
                concat!(
                    r#"{{"pid":4242,"sessionId":"{sid}","name":"{name}","cwd":"/w","#,
                    r#""startedAt":1717000000000,"updatedAt":1717003600000,"#,
                    r#""status":"idle","version":"0.1.0"}}"#
                ),
                sid = sid,
                name = name
            ),
        )
        .unwrap();
        let env = crate::effects::MapEnv {
            vars: [
                ("HOME", home.to_string_lossy().into_owned()),
                ("XDG_RUNTIME_DIR", "/tmp/qw-lanes-xdg".to_string()),
                ("ZMX_DIR", "/tmp/qw-lanes-zmx".to_string()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
            uid: 501,
        };
        (paths, env)
    }

    /// **Defect A regression.** Under the DEFAULT backend — `QD_MUX` unset ⇒
    /// EMBEDDED — a live pane lives in the qrmux dir. [`row_for_id`] used to list
    /// `zmx_dir::resolve_zmx_dir` unconditionally (off an inline `RealEnv`, no
    /// less), so it found nothing there, left `zmx_name` as `None`, and
    /// [`LaneOps::attach`] answered [`LaneError::Cold`] for a session that was in
    /// fact LIVE. Not an edge case: that is the default configuration, every time.
    ///
    /// MUTATION EVIDENCE: point the join back at the zmx dir and `OneDirMux`
    /// refuses the listing, so `zmx_name` stays `None` and this reds with the
    /// EXACT symptom the defect produced — `Err(Cold)` on a live pane. Feed it the
    /// process env instead of the injected one and it reds too, because the real
    /// `XDG_RUNTIME_DIR`/`ZMX_DIR` are not the jail's.
    #[test]
    fn an_embedded_pane_is_found_so_attach_is_never_cold() {
        let t = tempfile::tempdir().unwrap();
        let (paths, env) = jailed_row(t.path(), "wk", "sid-embedded-1");
        let id = SessionId("sid-embedded-1".into());
        // Where the EMBEDDED backend puts its panes, resolved the same way the
        // production path resolves it — asserted, not assumed.
        let qrmux = crate::qrmux_dir::resolve_qrmux_dir(&paths.home, &env).unwrap();
        assert_eq!(qrmux, PathBuf::from("/tmp/qw-lanes-xdg/qrmux"));
        assert_ne!(
            qrmux,
            quorum_core::zmx_dir::resolve_zmx_dir(&env),
            "the two dirs must differ or the defect is unobservable"
        );

        let mux = OneDirMux {
            dir: qrmux.clone(),
            pane: "wk".to_string(),
        };
        let row = row_for_id(&paths, &env, Some(&mux), &id).expect("the forged row resolves");
        assert_eq!(row.zmx_name.as_deref(), Some("wk"), "the live pane is FOUND");
        assert_eq!(
            row.socket_dir.as_deref(),
            Some(qrmux.to_string_lossy().as_ref()),
            "and it is tagged with the dir it was found in"
        );

        let ops = LaneImpl {
            lane: Lane::new(Harness::ClaudeCode, Mode::Pane).unwrap(),
            paths,
            env: &env,
            mux: Some(Box::new(mux)),
        };
        match ops.attach(&id) {
            Ok(code) => assert_eq!(code, 7, "the handoff reached the mux"),
            Err(e) => panic!("a live embedded pane must not report cold: {e}"),
        }
    }

    /// **Defect A regression — a tombstoned row is still a row.**
    ///
    /// `qd stop` RENAMES `<pid>.json` to `<pid>.json.tombstoned`; it never deletes
    /// it. [`row_for_id`] read the sessions dir with `include_tombstoned = false`,
    /// so a STOPPED session answered `NotFound` through every lane method — and
    /// "no such session" is a different sentence from "it is stopped", said to a
    /// user about a session `qd resume` and `qd stop` are both required to act on
    /// (`tests/resolve_beyond_cap.rs::stop_on_already_tombstoned_is_graceful`, and
    /// resume's own "revivable from any non-alive state, incl. a tombstoned stop").
    ///
    /// MUTATION EVIDENCE: put the `false` back in the `read_entries` call and the
    /// first `expect` fires — the stopped row is not found at all. Stamp the found
    /// row `LiveRegistry` unconditionally (the pre-fix literal) and the branch
    /// assertion reds, which is the assertion `kill.rs` reads to pick
    /// [`crate::kill::PidProvenance`]. Reverse the live/tombstone preference (make
    /// it `max_by_key`) and the last block reds: the stale history would win over
    /// the current record.
    #[test]
    fn a_stopped_row_resolves_and_is_stamped_tombstoned_live_winning() {
        use quorum_core::model::SessionBranch;
        let t = tempfile::tempdir().unwrap();
        let (paths, env) = jailed_row(t.path(), "wk", "sid-stopped-1");
        // Exactly what `qd stop` does to the file — a rename, not a delete.
        std::fs::rename(
            paths.sessions_dir.join("4242.json"),
            paths.sessions_dir.join("4242.json.tombstoned"),
        )
        .unwrap();

        let id = SessionId("sid-stopped-1".into());
        let row = row_for_id(&paths, &env, None, &id)
            .expect("a stopped session is still addressable — resume and stop both act on one");
        assert_eq!(row.pid, Some(4242));
        assert_eq!(
            row.which_branch,
            SessionBranch::Tombstoned,
            "the branch is read off the FILE the row came out of; calling a tombstone \
             LiveRegistry is a false statement about a record that says qd stopped this"
        );

        // And through the lane, which is where it mattered: the refusal is now the
        // honest COLD (this pane-lane row has no pane), not NotFound. `mux: None`
        // keeps the pane join out of it — the point here is the LOOKUP.
        let ops = LaneImpl {
            lane: Lane::new(Harness::ClaudeCode, Mode::Pane).unwrap(),
            paths: paths.clone(),
            env: &env,
            mux: None,
        };
        match ops.attach(&id) {
            Err(LaneError::Cold { .. }) => {}
            other => panic!("a stopped row must be addressable, not NotFound: {other:?}"),
        }

        // LIVE WINS when both files carry the id. A revive writes a fresh
        // `<pid>.json` and the old row's tombstone sits on disk until something
        // sweeps it, so the two coexist and only one of them is current truth.
        std::fs::write(
            paths.sessions_dir.join("4243.json"),
            concat!(
                r#"{"pid":4243,"sessionId":"sid-stopped-1","name":"wk","cwd":"/w","#,
                r#""startedAt":1717000000000,"updatedAt":1717003600000,"#,
                r#""status":"idle","version":"0.1.0"}"#
            ),
        )
        .unwrap();
        let row = row_for_id(&paths, &env, None, &id).expect("still resolves");
        assert_eq!(row.pid, Some(4243), "the LIVE row is the one that answers");
        assert_eq!(row.which_branch, SessionBranch::LiveRegistry);
    }

    /// **Defect A regression, second half — `deliver` disagreed with `wake` and
    /// `kill` about whether a tombstone is a row.**
    ///
    /// [`row_for_id`] was widened to read past tombstones (its TOMBSTONES COUNT
    /// note), and the test directly above pins that through `attach`. `deliver`
    /// still answered `NotFound` for the same row, because its wake gate did not
    /// go through `row_for_id` — it read `self.health(id)?`, and `health` resolves
    /// via `lane_read::live_row`, whose `read_entries(.., false)` is the very
    /// `false` the other lookup had dropped. The two lookups differed by one
    /// boolean and the `?` fired before `wake` was ever reached.
    ///
    /// What it cost a user: `qd stop wk` then `qd send wk "hi"`. `send_unified`
    /// says in as many words that a stopped target "is NO LONGER rejected here ...
    /// It is a WAKE trigger", and passes `wake_if_cold: true` for exactly this
    /// row — which came back "no such session" for a session the user had just
    /// stopped BY NAME.
    ///
    /// MUTATION EVIDENCE: restore `!is_deliverable(self.health(id)?.status)` as the
    /// gate and this reds with `NotFound` — the revive is never attempted. The
    /// assertion is deliberately NOT `Ok`: nothing revives inside a jail with no
    /// mux, so what is pinned is that the answer became a WAKE OUTCOME (the revive
    /// ran and failed on the absent backend) rather than a LOOKUP VERDICT (the
    /// revive never ran). Narrowing it to a specific wake error would pin the
    /// jail's failure mode instead of the gate. `mux: None` is also what keeps
    /// this test from launching anything: the claude arm resolves its backend
    /// after planning and stops there.
    #[test]
    fn deliver_wakes_a_stopped_row_instead_of_calling_it_not_found() {
        let t = tempfile::tempdir().unwrap();
        let (paths, env) = jailed_row(t.path(), "wk", "sid-stopped-deliver-1");
        // Exactly what `qd stop` does to the file — a rename, not a delete.
        std::fs::rename(
            paths.sessions_dir.join("4242.json"),
            paths.sessions_dir.join("4242.json.tombstoned"),
        )
        .unwrap();

        let id = SessionId("sid-stopped-deliver-1".into());
        let ops = LaneImpl {
            lane: Lane::new(Harness::ClaudeCode, Mode::Pane).unwrap(),
            paths: paths.clone(),
            env: &env,
            mux: None,
        };

        // The two readings this defect lived in the gap between, asserted first so
        // that a change closing the gap somewhere else retires this test loudly
        // rather than leaving it green for a reason it was not written for.
        assert!(
            row_for_id(&paths, &env, None, &id).is_some(),
            "the stopped row is addressable — that is the fix the test above pins"
        );
        assert!(
            matches!(ops.health(&id), Err(LaneError::NotFound { .. })),
            "`health` still resolves LIVE rows only; if that stops holding, the \
             gate below is no longer the thing under test"
        );

        if let Err(LaneError::NotFound { .. }) = ops.deliver(
            &id,
            &a_message(),
            &DeliverPolicy {
                wake_if_cold: true,
                ..DeliverPolicy::default()
            },
        ) {
            panic!(
                "a stopped session is a WAKE TRIGGER, not \"no such session\" — \
                 `deliver` refused a row that `wake` and `kill` both act on"
            );
        }
    }

    /// A spawner that must never be reached: the test below is supposed to stop at
    /// the port allocator, one step past the gate it is about. Borrowed shape from
    /// `provider::acp::daemon`'s own order guard, which uses the same lever.
    struct NeverSpawner;
    impl crate::create_daemon::DaemonSpawner for NeverSpawner {
        fn spawn_detached(
            &self,
            _argv: &[String],
            _env: &[(String, String)],
            _cwd: &std::path::Path,
            _log: &std::path::Path,
        ) -> std::io::Result<crate::create_daemon::SpawnedDaemon> {
            unreachable!("the allocator refuses first — nothing may be launched")
        }
        fn kill(&self, _pid: i64) {
            unreachable!("nothing was spawned, so nothing can be killed")
        }
    }

    /// **Defect B regression — `jsonl_path` was `None`, and that REFUSED every acp
    /// revive.**
    ///
    /// [`LaneOps::wake`]'s acp arm passes `has_jsonl: s.jsonl_path.is_some()` into
    /// `resume_acp`, whose resumability gate is `session_id.is_empty() ||
    /// (provider == "acp/claude-code" && !has_jsonl)`. [`row_for_id`] set
    /// `jsonl_path: None` unconditionally, so that gate was FALSE-TRIPPED for every
    /// single `acp/claude-code` row — including one whose CC transcript was sitting
    /// on disk the whole time. Latent only because no verb routes here yet.
    ///
    /// The revive is stopped one step past the gate by a failing port allocator
    /// (the same lever `daemon.rs`'s order guard uses), so this observes the gate
    /// without spawning an adapter.
    ///
    /// MUTATION EVIDENCE: the second half of this test IS the pre-fix behaviour,
    /// asserted as a counterfactual — feed the gate `has_jsonl: false` and it
    /// answers `NoResumableTranscript`. So restoring `jsonl_path: None` in
    /// `row_for_id` turns the first assertion into the second one. Point the
    /// transcript join at a root the file is not under (or drop the join) and the
    /// `jsonl_path` assertion reds directly.
    #[test]
    fn an_acp_row_with_a_transcript_on_disk_is_not_refused_no_resumable_transcript() {
        use crate::provider::acp::daemon::{
            resume_acp, AcpDaemonDeps, AcpResumeError, AcpResumeParams, AcpWarning,
        };
        let t = tempfile::tempdir().unwrap();
        let paths = QdPaths::from_home(t.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let sid = "sid-acp-1";

        // A pid that is definitively DEAD (spawned, killed, reaped) — the row must
        // not look alive, or the already-alive gate answers before the one under
        // test. The same trick `daemon.rs`'s order guard uses in reverse.
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let dead_pid = child.id() as i64;
        let _ = child.kill();
        let _ = child.wait();

        std::fs::write(
            paths.sessions_dir.join(format!("{dead_pid}.json")),
            format!(
                concat!(
                    r#"{{"pid":{pid},"sessionId":"{sid}","name":"wk","cwd":"/w","#,
                    r#""provider":"acp/claude-code","hosting":"daemon","#,
                    r#""startedAt":1717000000000,"updatedAt":1717003600000,"#,
                    r#""status":"idle","version":"0.1.0"}}"#
                ),
                pid = dead_pid,
                sid = sid
            ),
        )
        .unwrap();
        // The bridge writes CLAUDE-shaped JSONL, so the acp provider's transcript
        // root IS the projects dir and its lookup IS `jsonl::find_jsonl_path`.
        let transcript = paths.projects_dir.join("-w").join(format!("{sid}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();

        let id = SessionId(sid.into());
        let row = row_for_id(&paths, &E, None, &id).expect("the acp row resolves");
        assert_eq!(
            row.jsonl_path.as_deref(),
            Some(transcript.to_string_lossy().as_ref()),
            "the transcript is JOINED, through the row's own provider"
        );

        let clock = quorum_core::effects::FixedClock(0);
        let spawner = NeverSpawner;
        let warn = |_: &AcpWarning| {};
        let alloc = || -> std::io::Result<u16> { Err(std::io::Error::other("no ports in a unit")) };
        let probe = |_pid: i64| None;
        let deps = AcpDaemonDeps {
            exe: PathBuf::from("/nonexistent/qd"),
            home: paths.home.clone(),
            paths: &paths,
            clock: &clock,
            spawner: &spawner,
            alloc_port: &alloc,
            cmdline_probe: &probe,
            warn: &warn,
        };
        // Built EXACTLY the way the lane's acp arm builds it — `has_jsonl` is
        // `s.jsonl_path.is_some()` off the row this function just returned.
        let params = |has_jsonl: bool| AcpResumeParams {
            name: "wk".to_string(),
            session_id: row.session_id.clone(),
            provider_id: row.provider.clone(),
            cwd: row.cwd.clone(),
            has_jsonl,
            current_pid: row.pid,
            current_endpoint: None,
        };

        match resume_acp(&deps, &params(row.jsonl_path.is_some())) {
            Err(AcpResumeError::PortAllocFailed { .. }) => {}
            other => panic!(
                "a transcript on disk must get PAST the resumability gate, to the revive \
                 proper: {other:?}"
            ),
        }
        // The counterfactual — and, verbatim, what this lane did before the join
        // existed. One `false` is the whole defect.
        match resume_acp(&deps, &params(false)) {
            Err(AcpResumeError::NoResumableTranscript { .. }) => {}
            other => panic!("without a transcript the gate must refuse: {other:?}"),
        }
    }

    #[test]
    fn an_unknown_id_is_not_found_never_a_silent_success() {
        let ghost = SessionId("no-such-session".into());
        for lane in Lane::ALL.iter().filter(|l| l.is_pane()) {
            let (p, _) = lane_for_test(*lane);
            let ops = lane_ops(*lane, &E, p);
            assert!(matches!(ops.wake(&ghost, RenderMode::default(), None), Err(LaneError::NotFound { .. })));
            assert!(matches!(
                ops.attach(&ghost),
                Err(LaneError::NotFound { .. })
            ));
        }
    }

    // --- start: the routing table, and that each arm is wired to its own core

    /// **The silent-swap guard, at runtime.**
    ///
    /// Four lanes refuse a create-time prompt, each for its OWN reason, and each
    /// refuses BEFORE anything is claimed or spawned — so handing every lane a
    /// prompt is a side-effect-free probe of which arm actually ran. Route
    /// `pi/mux-pane` to the daemon arm (mis-swap #2) and this goes red, because
    /// the reason that comes back is the turn-free tier-a one instead of the
    /// no-transcript-until-first-reply one.
    ///
    /// The three lanes that ACCEPT a prompt deliver the first turn in-core, so
    /// they are covered by the source-level guard below instead: probing them at
    /// runtime would spawn a resident.
    #[test]
    fn each_prompt_refusing_lane_answers_with_its_own_reason() {
        let expect: [(Lane, &str); 4] = [
            (
                Lane::new(Harness::ClaudeCode, Mode::Pane).unwrap(),
                "POST-boot delivery",
            ),
            (
                Lane::new(Harness::Codex, Mode::Pane).unwrap(),
                "no verifiable submit path",
            ),
            (
                Lane::new(Harness::Pi, Mode::Pane).unwrap(),
                "writes no transcript until its first",
            ),
            (
                Lane::new(Harness::Pi, Mode::Daemon).unwrap(),
                "turn-free by design",
            ),
        ];
        for (lane, needle) in expect {
            let (p, _) = lane_for_test(lane);
            let ops = lane_ops(lane, &E, p);
            let req = StartRequest {
                name: "s".into(),
                prompt: Some("hello".into()),
                ..Default::default()
            };
            match ops.start(&req) {
                Err(LaneError::NotSupported { op, reason }) => {
                    assert_eq!(op, "start(prompt)", "{lane}");
                    assert!(
                        reason.contains(needle),
                        "{lane}: expected THIS lane's reason (containing {needle:?}), got {reason:?}"
                    );
                }
                other => panic!("{lane}: a create-time prompt must be refused, got {other:?}"),
            }
            // The ARM refused, and this is the same sentence the public TABLE
            // gives a caller. `qd start -p` reads the table to decide whether to
            // pass the prompt at all, so a table that disagreed with the arm would
            // mean either a dropped prompt or a create refused outright — and
            // neither would show up anywhere else.
            assert_eq!(
                create_prompt_refusal(lane).map(|r| r.contains(needle)),
                Some(true),
                "{lane}: create_prompt_refusal must agree with the arm that just refused"
            );
        }

        // And the other side of it: the three lanes whose create delivers the
        // first turn IN-CORE must read as accepting one. Asserted from the table
        // rather than by probing, because probing them spawns a resident.
        for lane in [
            Lane::new(Harness::Codex, Mode::Daemon).unwrap(),
            Lane::new(Harness::AcpClaudeCode, Mode::Daemon).unwrap(),
            Lane::new(Harness::Opencode, Mode::Daemon).unwrap(),
        ] {
            assert_eq!(
                create_prompt_refusal(lane),
                None,
                "{lane} delivers the create-time prompt in-core"
            );
        }
    }

    /// An EMPTY prompt is no prompt — the same gate every verb wrapper uses today
    /// (`is_some_and(|s| !s.is_empty())`). Asserted on the refusal itself so the
    /// four arms above cannot start refusing a degenerate no-op turn.
    #[test]
    fn an_empty_prompt_is_not_a_prompt() {
        let req = StartRequest {
            name: "s".into(),
            prompt: Some(String::new()),
            ..Default::default()
        };
        assert!(refuse_create_prompt(&req, "unused").is_ok());
        assert!(refuse_create_prompt(&StartRequest::default(), "unused").is_ok());
    }

    /// **The silent-swap guard, at the type level.**
    ///
    /// Source-scans THIS file for two facts a runtime test cannot reach without
    /// spawning real processes: that the seven-arm routing table maps each lane to
    /// its own arm, and that each arm calls its own create core and NO sibling's.
    /// Same `include_str!` idiom as `conformance`'s trait-signature guard, and for
    /// the same reason — the property is about the code's shape.
    ///
    /// Swapping two arms in the table, or pasting the wrong core into an arm, is
    /// exactly the mis-swap class that used to be silent. Here it is a red test.
    #[test]
    fn every_lane_routes_to_its_own_arm_and_its_own_core() {
        let src = include_str!("lanes.rs");

        // 1. The routing table itself: eight lanes, eight arms, one mapping each.
        //
        // `codex/daemon` and `codex/app-server` share a core and are the one pair
        // that does — the ARGUMENT is the difference (the row's `hosting` stamp),
        // and that argument is spelled in the table precisely so a swap of the two
        // reds here instead of producing a session that comes back as the wrong
        // lane.
        let table: [(&str, &str); 8] = [
            ("(Harness::ClaudeCode, Mode::Pane)", "self.start_claude_pane(req)"),
            ("(Harness::Codex, Mode::Pane)", "self.start_codex_pane(req)"),
            (
                "(Harness::Codex, Mode::Daemon)",
                "self.start_codex_daemon(req, Some(Mode::Daemon.hosting_token()))",
            ),
            ("(Harness::Codex, Mode::AppServer)", "self.start_codex_app_server(req)"),
            ("(Harness::Pi, Mode::Pane)", "self.start_pi_pane(req)"),
            ("(Harness::Pi, Mode::Daemon)", "self.start_pi_daemon(req)"),
            ("(Harness::AcpClaudeCode, Mode::Daemon)", "self.start_acp_daemon(req)"),
            ("(Harness::Opencode, Mode::Daemon)", "self.start_acp_daemon(req)"),
        ];
        for (pattern, arm) in table {
            let line = format!("{pattern} => {arm},");
            assert!(
                src.contains(&line),
                "the create routing table must map {pattern} to {arm} — this is the \
                 mapping the deleted if-chain got to reorder silently.\n\
                 Expected the line: {line}"
            );
        }

        // 2. Each arm delegates to ITS core, and mentions no other arm's core.
        //    `start_claude_pane` is the one that assembles effects rather than
        //    delegating whole, so its marker is the create it drives.
        let cores: [(&str, &str); 7] = [
            ("fn start_claude_pane", "crate::create::run_new(&deps, &params)"),
            ("fn start_codex_pane", "create_codex_tui("),
            ("fn start_pi_pane", "create_pi_tui(&deps, &plan)"),
            ("fn start_codex_daemon", "run_new_daemon(&deps, &params)"),
            ("fn start_pi_daemon", "create_pi_session(&deps, &params)"),
            ("fn start_acp_daemon", "create_acp_daemon(&deps, &params)"),
            // `pi/extension` delegates to its own core, which wraps the pi pane
            // create rather than copying it. The mutual-exclusion half of this
            // check is what matters most for this pair: `start_pi_pane` must
            // never reach the channelled core (it would launch a socket nobody
            // recorded) and this arm must never reach the bare one (it would
            // launch an unchannelled pi under an `extension` row — the exact
            // silent state this lane shipped in before `control_socket` was
            // plumbed through `NewParams`).
            (
                "fn start_pi_extension",
                "create_extension_session(&deps, &launch)",
            ),
        ];
        for (arm, core) in cores {
            let body = arm_body(src, arm);
            assert!(
                body.contains(core),
                "{arm} must delegate to {core} — a lane that reimplements its create \
                 forfeits the regression net the whole stage rests on"
            );
            for (other_arm, other_core) in cores {
                if other_arm != arm {
                    assert!(
                        !body.contains(other_core),
                        "{arm} calls {other_core}, which belongs to {other_arm}. That is \
                         the silent swap: the caller asks for one lane and gets another."
                    );
                }
            }
        }

        // The pi pane lane's TWO-PHASE order is user-visible (its `--session-id`
        // capability refusal must land before the mux backend is resolved), so the
        // plan call must precede the dep resolution — the same order `wake`'s pi
        // arm holds.
        let body = arm_body(src, "fn start_pi_pane");
        let plan_at = body.find("plan_pi_tui(").expect("pi/pane must plan first");
        let deps_at = body.find("self.pane_deps(").expect("pi/pane must resolve deps");
        assert!(
            plan_at < deps_at,
            "plan_pi_tui must run BEFORE pane_deps: its refusals have to be what the \
             caller hears about even when QD_MUX is also wrong"
        );

        // `pi/extension` holds the same order, and owes it for one more reason:
        // its phase 1 also installs the extension and rejects a `$TMPDIR` too
        // deep to hold a socket. Both refusals must land before a name is
        // claimed or a pane is spawned.
        let body = arm_body(src, "fn start_pi_extension");
        let plan_at = body
            .find("plan_extension_launch(")
            .expect("pi/extension must plan first");
        let deps_at = body
            .find("self.pane_deps(")
            .expect("pi/extension must resolve deps");
        assert!(
            plan_at < deps_at,
            "plan_extension_launch must run BEFORE pane_deps: an install failure \
             or an overlong socket path has to be what the caller hears about"
        );
    }

    /// One arm method's source, from its signature to the next method's doc or
    /// signature at the same indent.
    fn arm_body<'a>(src: &'a str, sig: &str) -> &'a str {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} must exist"));
        let rest = &src[start..];
        // Stop at whichever comes first: the next 4-indented `fn`, or the doc
        // comment introducing it (so a sibling's prose is never scanned as this
        // arm's body).
        let end = ["\n    fn ", "\n    /// "]
            .iter()
            .filter_map(|m| rest[sig.len()..].find(m).map(|i| i + sig.len()))
            .min()
            .unwrap_or(rest.len());
        &rest[..end]
    }

    /// `start` is TOTAL: no lane answers `NotImplemented`, and the two
    /// combinations that are not lanes answer `NotSupported` — an answer, never a
    /// debt. Cheap because the impossible arms return before touching anything.
    #[test]
    fn start_is_total_for_every_lane() {
        for (harness, mode) in [
            (Harness::ClaudeCode, Mode::Daemon),
            (Harness::AcpClaudeCode, Mode::Pane),
            (Harness::Opencode, Mode::Pane),
        ] {
            assert_eq!(
                Lane::new(harness, mode),
                None,
                "{harness:?}/{mode:?} must not be constructible as a lane"
            );
            // Reachable only by building the struct around `Lane::new`, which is
            // why the arms exist at all. They must ANSWER, not panic and not claim
            // a debt.
            let lane = Lane { harness, mode };
            let (p, _) = lane_for_test(lane);
            let ops = lane_ops(lane, &E, p);
            assert!(
                matches!(
                    ops.start(&StartRequest::default()),
                    Err(LaneError::NotSupported { .. })
                ),
                "{lane}: a combination that is not a lane must be NotSupported"
            );
        }
    }

    /// `wake` is TOTAL — no lane falls through to `NotImplemented`.
    ///
    /// The successor to the deleted `the_adapter_covers_every_lane`, which lived
    /// in the binary and pinned that the `LaneDeps` adapter supplied all seven
    /// revives. There is no adapter now: `wake` matches on the lane itself, so
    /// the compiler enforces that every lane has an ARM — but not that the arm
    /// does anything. This pins the second half: for every lane, a ghost id
    /// reaches the row lookup and reports `NotFound`, which is only reachable if
    /// the arm is real work rather than a `todo()`.
    #[test]
    fn wake_is_total_for_every_lane() {
        let ghost = SessionId("no-such-session".into());
        for lane in Lane::ALL {
            let (p, _) = lane_for_test(lane);
            let ops = lane_ops(lane, &E, p);
            assert!(
                matches!(ops.wake(&ghost, RenderMode::default(), None), Err(LaneError::NotFound { .. })),
                "{lane}: wake must reach the row lookup, never a NotImplemented gap"
            );
        }
    }

    /// `kill` is TOTAL — no lane falls through to `NotImplemented`.
    ///
    /// The twin of [`wake_is_total_for_every_lane`], and it pins the same second
    /// half: the compiler enforces that every lane has an ARM, not that the arm
    /// does anything. A ghost id must reach the row lookup and report `NotFound`,
    /// which is only reachable once the arm is real delegation rather than a
    /// `todo()`. This is the one destructive method in the trait, so the pin
    /// matters more here than anywhere: a lane that answered `NotImplemented`
    /// would push its caller back onto the provider-string routing whose
    /// mis-ordering LEAKS (`kill.rs`'s daemon-strategy-on-a-pane war story).
    #[test]
    fn kill_is_total_for_every_lane() {
        let ghost = SessionId("no-such-session".into());
        for lane in Lane::ALL {
            let (p, _) = lane_for_test(lane);
            let ops = lane_ops(lane, &E, p);
            assert!(
                matches!(ops.kill(&ghost), Err(LaneError::NotFound { .. })),
                "{lane}: kill must reach the row lookup, never a NotImplemented gap"
            );
        }
    }

    fn a_message() -> Message {
        Message {
            id: MessageId("m-fixture".into()),
            text: "hi".into(),
            from: None,
        }
    }

    /// The carrier choice, exhaustively. This is the decision phase 2 moved
    /// inside the lane, so it is pinned where a reader of `deliver` will look for
    /// it rather than only through a live send.
    ///
    /// The first row is the one that matters: relay wins WITH a joined pane in
    /// hand. Flipping that precedence is the silent downgrade — a session with a
    /// live relay quietly typed at through its PTY.
    #[test]
    fn relay_precedence_is_structural_and_pty_needs_an_observed_absence() {
        assert_eq!(
            claude_carrier(Some(7), true),
            ClaudeCarrier::Relay { port: 7 },
            "a recorded port selects relay BEFORE mux state is considered"
        );
        assert_eq!(claude_carrier(Some(7), false), ClaudeCarrier::Relay { port: 7 });
        assert_eq!(claude_carrier(None, true), ClaudeCarrier::MuxPty);
        assert_eq!(claude_carrier(None, false), ClaudeCarrier::NoLiveReceivePath);
    }

    /// `deliver` is REAL for every lane.
    ///
    /// The same second-half pin [`wake_is_total_for_every_lane`] makes: the
    /// compiler enforces that the arm EXISTS, not that it does anything. A ghost
    /// id must reach the ROW LOOKUP on all seven and report `NotFound`, which is
    /// only reachable once the arm is real delegation rather than a `todo()` — and
    /// which is what would red if a lane were quietly parked back on a blocker.
    ///
    /// It builds through [`lane_ops`] — the ONLY constructor. Until phase 3B this
    /// line read `lane_ops_with_carriers(.., &ProbeCarriers)`, because `deliver`
    /// refused outright without the callback seam; that it now delivers from the
    /// plain constructor IS the phase's deliverable, asserted rather than
    /// described.
    #[test]
    fn deliver_is_total_for_every_lane() {
        let ghost = SessionId("no-such-session".into());
        for lane in Lane::ALL {
            let (p, _) = lane_for_test(lane);
            let ops = lane_ops(lane, &E, p);
            let out = ops.deliver(&ghost, &a_message(), &DeliverPolicy::default());
            assert!(
                matches!(out, Err(LaneError::NotFound { .. })),
                "{lane}: deliver must reach the row lookup, got {out:?}"
            );
        }
    }

    /// **`await_idle` ROUTES ON THE LANE — the three pi lanes get three
    /// different answers for ONE row.**
    ///
    /// P2 (`16-default-lane-switch.md`). This match used to key on
    /// `self.lane.harness`, so all three pi lanes landed on
    /// [`crate::idle::await_idle_pi`] — whose entry gate demands a pid whose
    /// cmdline is our `pi-daemon`. A pi TUI's never is, so `qd wait` on
    /// `pi/extension` and on `pi/mux-pane` failed `"pi endpoint not reachable at
    /// wait entry"`: a transport refusal, about a live session, naming a daemon
    /// it does not have. For `pi/extension` that happened while the lane's own
    /// idle RPC sat unused two files away.
    ///
    /// ONE row, three lanes, and the assertions are about the SHAPE of each
    /// answer rather than its wording where the wording is not the lane's:
    ///
    ///   - `pi/daemon` keeps the daemon watcher, proven by the exact sentence its
    ///     identity gate produces. This is the control: the row IS one that
    ///     `await_idle_pi` refuses this way, so the two arms below are refusing
    ///     it differently because of the ROUTING and not because of the fixture.
    ///   - `pi/extension` must never produce that sentence. It reaches its own
    ///     watcher, which refuses on the CONTROL CHANNEL — `Cold` for a pid the
    ///     process table does not have, or a transport error naming the socket.
    ///     The row is deliberately bare (no endpoint, no live pi), because what
    ///     is under test is WHERE the refusal comes from, and driving a real pi
    ///     TUI to idle belongs in a live suite (B5).
    ///   - `pi/mux-pane` refuses in the type system, and says why. See
    ///     [`PI_PANE_HAS_NO_IDLE_SOURCE`]: its only source is a transcript that
    ///     can never report busy, so a wait built on it would answer "idle" for a
    ///     session mid-turn — a false done, which this crate may not produce.
    ///
    /// MUTATION EVIDENCE: put the match back on `self.lane.harness` and the last
    /// two blocks red at once — both lanes start answering with the daemon
    /// sentence the first block pins. Route `(Pi, Pane)` to the extension watcher
    /// "for symmetry" and the third block reds on the missing refusal.
    #[test]
    fn await_idle_routes_on_the_lane_so_the_three_pi_lanes_answer_differently() {
        let t = tempfile::tempdir().unwrap();
        let (paths, env) = jailed_row(t.path(), "piwait", "sid-pi-wait-1");
        let id = SessionId("sid-pi-wait-1".into());
        let ops = |mode: Mode| LaneImpl {
            lane: Lane::new(Harness::Pi, mode).expect("pi has all three of these lanes"),
            paths: paths.clone(),
            env: &env,
            mux: None,
        };

        // The control. Deterministic whatever the process table says about the
        // fixture's pid: the row records NO endpoint, and `await_idle_pi`'s gate
        // is `endpoint.is_some() && alive && cmdline_is_ours`.
        let daemon = ops(Mode::Daemon).await_idle(&id, 10);
        assert!(
            matches!(
                &daemon,
                Err(LaneError::Transport { detail })
                    if detail == "pi endpoint not reachable at wait entry"
            ),
            "pi/daemon keeps `await_idle_pi`, and this row is one it refuses: {daemon:?}"
        );

        let extension = ops(Mode::Extension).await_idle(&id, 10);
        match &extension {
            // The fixture pid is not alive here: no live process to wait on.
            Err(LaneError::Cold { .. }) => {}
            // The fixture pid IS alive (a pid the host happens to be reusing).
            // Then the watcher got as far as the channel and refused THERE —
            // which is the point, and the socket path is in the sentence.
            Err(LaneError::Transport { detail }) => assert!(
                detail.contains("control channel"),
                "the extension watcher refuses on ITS channel, not on a daemon \
                 endpoint: {detail}"
            ),
            other => panic!("expected a channel-shaped refusal, got {other:?}"),
        }
        assert_ne!(
            format!("{extension:?}"),
            format!("{daemon:?}"),
            "pi/extension must not inherit pi/daemon's answer — that inheritance \
             IS the bug (P2)"
        );

        let pane = ops(Mode::Pane).await_idle(&id, 10);
        let Err(LaneError::NotSupported { op, reason }) = &pane else {
            panic!("pi/mux-pane has no idle source and must say so: {pane:?}");
        };
        assert_eq!(op, "await_idle");
        assert_eq!(
            reason, PI_PANE_HAS_NO_IDLE_SOURCE,
            "the refusal must carry the constant that explains itself, so the \
             reason and its reasoning cannot drift apart"
        );
    }

    /// **A STALE-LIVE ROW WITH `wake_if_cold: false` IS ATTEMPTED, NEVER REFUSED
    /// `Cold`.**
    ///
    /// The row here is the one that splits the repo's two liveness answers, and
    /// both of them are deliberate: `send_unified::is_live` reads the STATUS ENUM
    /// alone and calls this row LIVE, while [`LaneOps::health`] reads status PLUS
    /// `(pid, start_time)` through the `qd ls` gate and calls it COLD. It is the
    /// shape of `verbs_a4`'s
    /// `send_live_unroutable_claude_is_unchanged_no_wake_no_envelope` fixture —
    /// `status: "idle"` over a pid whose recorded start time no host can match —
    /// and qd's LIVE path passes exactly `wake_if_cold: false` for it.
    ///
    /// So `deliver` must NOT read `health` when no wake was asked for: it must
    /// hand the row to the carrier and let the carrier report. Refusing off the
    /// projection would give qd's live path a `Cold` it has no refusal class for,
    /// where today it stamps `delivery-failed{delivery}` off a carrier that
    /// actually looked.
    ///
    /// The relay sidecar is what keeps this deterministic rather than a mercy:
    /// with `<home>/.claude/relay` non-empty, `get_relay_ports` returns the
    /// sidecar set instead of port-scanning 8900..9000 on the developer's own
    /// machine. Its pid is `1`, whose ancestry walk terminates immediately, so no
    /// relay ever maps onto this row's pid and the PTY arm is the one taken.
    ///
    /// MUTATION EVIDENCE: restore the `if !policy.wake_if_cold { return
    /// Err(LaneError::Cold …) }` arm and this reds on the FIRST assertion — no
    /// carrier is called at all. Drop the `policy.wake_if_cold &&` conjunct so
    /// `health` is consulted unconditionally and it reds the same way.
    #[test]
    fn a_stale_live_row_with_no_wake_asked_for_is_attempted_not_refused_cold() {
        let t = tempfile::tempdir().unwrap();
        let (paths, env) = jailed_row(t.path(), "stalewk", "sid-stale-live-1");
        let id = SessionId("sid-stale-live-1".into());
        let lane = Lane::new(Harness::ClaudeCode, Mode::Pane).unwrap();

        // A sidecar relay owned by pid 1: `get_relay_ports` answers from the
        // sidecars (no port scan), and pid 1's ancestry walk ends at once, so the
        // ancestry map is empty and this row's relay port is an OBSERVED `None`.
        std::fs::create_dir_all(&paths.relay_dir).unwrap();
        std::fs::write(
            paths.relay_dir.join("r.json"),
            r#"{"port":8901,"sessionId":"someone-elses-relay","pid":1}"#,
        )
        .unwrap();

        // The two readings, asserted rather than assumed — this test is worthless
        // if the row is not actually the one that splits them.
        let qrmux = crate::qrmux_dir::resolve_qrmux_dir(&paths.home, &env).unwrap();
        let mux = OneDirMux {
            dir: qrmux,
            pane: "stalewk".to_string(),
        };
        let reader = LaneImpl {
            lane,
            paths: paths.clone(),
            env: &env,
            mux: None,
        };
        assert_eq!(
            reader.health(&id).unwrap().status,
            SessionStatus::Cold,
            "the gate must convict this pid, or the row does not split the two \
             readings and the test proves nothing"
        );
        assert_eq!(
            row_for_id(&paths, &env, None, &id).unwrap().status,
            quorum_core::model::SessionStatus::Idle,
            "…while the registry STRING still says idle — which is what \
             `send_unified::is_live` reads"
        );

        let ops = LaneImpl {
            lane,
            paths: paths.clone(),
            env: &env,
            mux: Some(Box::new(mux)),
        };
        let out = ops.deliver(
            &id,
            &a_message(),
            &DeliverPolicy {
                wake_if_cold: false,
                ..DeliverPolicy::default()
            },
        );

        // The carrier used to be a `RecordingCarriers` mock that pushed
        // `"mux_pty"` onto a list. Phase 3B deleted the seam it plugged into, so
        // the attempt is proven from the carrier's OWN FOOTPRINT instead: the pane
        // carrier mints a `send_id` and writes `send-initiated{verb: "send:pty"}`
        // into the jailed delivery log BEFORE it touches the mux, and nothing else
        // in this crate writes that record. A relay-arm delivery would have written
        // `verb: "send:relay"`; a refusal off `health` would have written nothing
        // at all, which is exactly the mutation this line catches.
        let log = QdPaths::from_home_env(&paths.home, &env)
            .state_dir
            .join("sessions")
            .join("sid-stale-live-1.events.jsonl");
        let raw = std::fs::read_to_string(&log).unwrap_or_else(|e| {
            panic!("the pane carrier must have run and written {log:?}: {e}; deliver said {out:?}")
        });
        assert!(
            raw.contains("\"event\":\"send-initiated\"") && raw.contains("\"verb\":\"send:pty\""),
            "the delivery was ATTEMPTED through the one carrier this row has \
             (send-initiated{{verb:send:pty}}); log was {raw:?}, deliver said {out:?}"
        );
        match out {
            Ok(receipt) => {
                assert!(
                    !receipt.accepted,
                    "the carrier failed, so the receipt says so — it is the CARRIER's \
                     report, not a projection's"
                );
                assert_eq!(
                    receipt.woke,
                    Confirmation::No,
                    "no wake was asked for and none happened, so qd stamps no `queued`"
                );
            }
            Err(LaneError::Cold { .. }) => panic!(
                "`wake_if_cold: false` must ATTEMPT, never refuse Cold off a `health` \
                 read the caller did not ask for"
            ),
            Err(e) => panic!("expected an attempted delivery, got {e:?}"),
        }
    }

    /// `receive_path` is REAL for every lane, and answers WITHOUT reaching a
    /// carrier.
    ///
    /// That is the property the whole method rests on: qd must be able to ask
    /// "is there anywhere to receive" and render a refusal BEFORE it commits an
    /// intent record, so the question cannot be allowed to require the machinery
    /// that takes responsibility for the message. A ghost id reaching the row
    /// lookup on all seven is what proves each arm is real rather than parked.
    #[test]
    fn receive_path_is_total_for_every_lane_without_the_carrier_seam() {
        let ghost = SessionId("no-such-session".into());
        for lane in Lane::ALL {
            let (p, _) = lane_for_test(lane);
            let ops = lane_ops(lane, &E, p);
            let out = ops.receive_path(&ghost);
            assert!(
                matches!(out, Err(LaneError::NotFound { .. })),
                "{lane}: receive_path must reach the row lookup without touching \
                 a carrier, got {out:?}"
            );
        }
    }

    // There WAS a `deliver_without_the_carrier_seam_says_which_seam_is_missing`
    // here, asserting that a lane built by `lane_ops` refused `deliver` BY NAME
    // ("use lane_ops_with_carriers") rather than failing in some other vocabulary.
    // Its subject is gone: phase 3B deleted the seam, so there is no seamless
    // construction left to refuse, and `deliver_is_total_for_every_lane` — which
    // now builds through `lane_ops` — asserts the opposite property in its place.

    #[test]
    fn no_method_is_parked_on_a_blocker_any_more() {
        let ghost = SessionId("x".into());
        for lane in Lane::ALL {
            let (p, _) = lane_for_test(lane);
            let ops = lane_ops(lane, &E, p);
            let outs = [
                // `start` is NOT in this sweep any more — it is implemented, and
                // calling it here would run seven REAL creates (a `codex
                // --version` sniff, a pi capability probe, a port bind) against a
                // nonexistent home. Its coverage is the two tests below.
                ops.kill(&ghost).err(),
                ops.list().err(),
                ops.health(&ghost).err(),
                ops.receive_path(&ghost).err(),
                ops.deliver(
                    &ghost,
                    &Message {
                        id: MessageId("m-fixture".into()),
                        text: String::new(),
                        from: None,
                    },
                    &DeliverPolicy::default(),
                )
                .err(),
                ops.await_terminal(&ghost, &MessageId(String::new()), 0)
                    .err(),
                ops.recover(
                    &LedgerAddress::session(ghost.clone()),
                    &MessageId(String::new()),
                )
                .err(),
                ops.resolved(
                    &LedgerAddress::session(ghost.clone()),
                    &MessageId(String::new()),
                )
                .err(),
            ];
            for e in outs.into_iter().flatten() {
                assert!(
                    !matches!(e, LaneError::NotImplemented { .. }),
                    "{lane}: no method may answer NotImplemented any more — every \
                     one of the nine is built. Got {e:?}"
                );
            }
        }
    }
}
