//! `qd attach <session>` — the human "get me into this session" verb.
//!
//! **This verb is the first production caller of the lane layer.** It resolves the
//! target with qd's fuzzy resolver, derives the [`Lane`](quorum_qw::lane::Lane)
//! from the row's provider + hosting, and then does exactly three things through
//! [`LaneOps`]: `attach`, and on a cold session `wake` followed by `attach` again.
//! Everything else in this file is rendering, or one of the three deliberate
//! qd-side keeps listed below.
//!
//! # What the lane replaced
//!
//! Three `AttachOutcome::Cold` arms, each ELEVEN lines long, differing only in
//! which revive they called — codex-guarded, pi-guarded, claude fall-through —
//! followed by the identical "print, build the mux, attach the handle, map the
//! exit code" tail. [`LaneOps::wake`] routes all six revives on `(harness, mode)`
//! itself, so the three arms are one call, and the routing is a total match rather
//! than a guarded if-chain whose ordering was enforced by a comment.
//!
//! # What deliberately stays here
//!
//! - **The `codex/daemon` viewer.** [`LaneOps::attach`] answers `NotSupported`
//!   for every daemon lane WITHOUT a joinable residence, and `07-lane-gaps.md`
//!   ruling J says that is intentional: the viewer does not give the daemon a
//!   terminal, it opens a SECOND CLIENT on its app server, which is a different
//!   operation and must not be promoted into the contract. So the viewer is
//!   tried before the lane, and a row that cannot host one falls through to the
//!   honest redirect.
//!
//!   NARROWED TWICE, and the second time by `acp/opencode`. `codex/app-server`
//!   stopped coming through here in 2026-08 (its own lane opens the same viewer);
//!   `acp/opencode` never did, because its lane grew a viewer of its own from the
//!   start — `opencode attach <url> --session <id>` on the HTTP server inside the
//!   ACP bridge, reached through [`LaneOps::attach`] like any other lane. What is
//!   left here is `codex/daemon` alone, which has no attach of its own and would
//!   otherwise lose an affordance it has today.
//! - **`--no-attach`** — revive to a persistent daemon and DO NOT attach. `wake`
//!   covers the revive; the "already live, not attaching" report and the
//!   claude-only guard are composed here.
//! - The id-collision preflight, `reject_if_tombstoned`, the `invoked_connect`
//!   telemetry, and every message.

use clap::ArgMatches;

use dispatch::model::{Session, SessionStatus};
use quorum_qw::contract::{LaneError, LaneOps, SessionId};
use quorum_qw::launch::RenderMode;

use super::common;
use super::lifecycle;
use super::resume;

/// Append a content-free A6 invoked line, verb `connect`, for a SUCCESSFUL
/// invocation of this handler (whether reached via `qd attach` or the hidden
/// `qd connect` alias — both dispatch here, spec §3.4: no first-class `attach`
/// kind in v1, `connect` is the one telemetry-visible verb). Best-effort: a
/// failure warns but NEVER changes the verb's exit code.
fn invoked_connect(session: &Session) {
    use dispatch::effects::{RealClock, RealEnv};
    if let Err(e) = dispatch::telemetry::append_invoked(
        &RealEnv,
        &RealClock,
        "connect",
        Some(&session.session_id),
        session.name.as_deref(),
    ) {
        eprintln!("qd attach: telemetry invoked append failed (non-fatal): {e}");
    }
}

/// The row's display name — what every message in this file calls it by.
fn display_name(session: &Session) -> &str {
    session.name.as_deref().unwrap_or(&session.session_id)
}

/// Render a successful handoff's exit code, stamping telemetry on exit 0 only
/// (A6 §3.4: one content-free `invoked` line on the SUCCESSFUL path; read paths
/// emit nothing, and this verb never reads).
fn attached(session: &Session, code: i32) -> i32 {
    if code == 0 {
        invoked_connect(session);
    }
    code
}

/// `qd attach <session>` — resolve the row, then hand to the lane. No `--json`
/// (interactive verb).
pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    // --no-attach (headless revive): revive a COLD session into a PERSISTENT,
    // relay-serving daemon and return 0 WITHOUT attaching a TTY — the entry a
    // systemd ExecStart calls. UNSET ⇒ the block below is skipped entirely and the
    // interactive attach path stays byte-identical to today.
    let no_attach = m.get_flag("no-attach");
    // punch item 7: per-session render mode for the auto-revive path (flag >
    // render-default config > inline). A LIVE attach is unaffected — the
    // property is launch-time only.
    //
    // THIS is what `LaneOps::wake` takes as its `render` argument, and passing
    // `RenderMode::default()` there instead would silently discard the user's
    // `--alt-screen` / `--inline` / `render-default` — the exact defect the
    // contract revision that added the parameter existed to fix.
    let render = common::resolve_render_mode(m, &dispatch::effects::RealEnv);

    // attach is the human "attach OR resume" verb, so it must be able to RESOLVE
    // anything resume can — including a COLD, AUTO-named session (user_named=false).
    // `JoinOpts::default()` (include_all=false) runs the list cap's named-only
    // filter (join.rs apply_list_cap), which drops auto-named rows → attach would
    // die "No session matching <q>" on exactly the sessions it is supposed to
    // revive. include_all=true lifts that filter. Tombstoned rows stay EXCLUDED
    // (the verb's pre-existing posture — Pete: don't widen it to killed
    // sessions; resume's include_tombstoned is resume's own call).
    // D-2: resolve against the FULL universe through the sealed uncapped entry, so
    // attach targets anything resume can — incl. a COLD / auto-named session far
    // outside the `ls` display cap. Tombstones resolve too; attach then REJECTS a
    // stopped session post-resolve with the clear "resume it first" message. (This
    // REVERSES the verb's pre-existing "tombstones excluded" posture — D-2 makes it
    // resolve-then-reject so the error teaches, never a phantom `No session matching`.)
    let session = match common::resolve_session_uncapped(query) {
        Ok(s) => s,
        Err(code) => return code,
    };
    if let Err(code) = common::reject_if_tombstoned(query, &session) {
        return code;
    }
    // The sealed entry returns an OWNED Session; the attach/revive pipeline below is
    // borrow-shaped (it took `&Session` from the old slice-borrowing resolver), so
    // re-borrow here — the mechanical owned→ref tweak the plan flagged.
    let session = &session;

    // --no-attach: revive-to-persistent-daemon (or report already-live) and return
    // 0 WITHOUT ever attaching a TTY. We branch BEFORE the lane because the lane's
    // job is precisely the handoff this flag refuses — honoring "do not attach"
    // means never calling `attach` for a live row. A COLD row (status Cold; Killed
    // rows were rejected above) routes to the SAME `revive_claude` seam the
    // interactive path's `wake` reaches, then we SKIP the handoff.
    if no_attach {
        // PROVIDER GUARD: the --no-attach revive/report logic is claude-code-SHAPED
        // — `revive_claude` builds a `claude … server:relay --resume <sid>` argv, and
        // the already-live branch below assumes a mux pane serving the relay. A
        // codex / acp/* / opencode / daemon-hosted (non-claude-code) row would
        // MIS-ROUTE: a Cold row fed into `revive_claude` (wrong argv shape), a live
        // one falsely reported "already live". Every claude-code row carries the
        // literal provider "claude-code" (model.rs: the join defaults absent-on-disk
        // to it); codex ("codex"), acp ("acp/*"), opencode, and unknown are all a
        // DIFFERENT provider/hosting (see the lane derivation below / kill.rs
        // dispatch). Refuse LOUDLY and return non-zero BEFORE the revive/report
        // logic — never proceed for a non-claude-code session.
        if session.provider != "claude-code" {
            eprintln!(
                "qd attach --no-attach: \"{}\" is a {} session; --no-attach supports only claude-code sessions.",
                display_name(session),
                session.provider
            );
            return 1;
        }
        // Already-live claude pane already serves the relay — report and stop; do
        // NOT attach (idle/busy/shell are the live states; cold falls through).
        if matches!(
            session.status,
            SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Shell
        ) {
            println!("\"{}\" is already live (persistent); not attaching.", display_name(session));
            invoked_connect(session);
            return 0;
        }
        return match resume::revive_claude(session, None, render, false, "attach") {
            Ok(handle) => {
                println!("Revived \"{}\" (persistent, no attach)", handle.zmx_name);
                invoked_connect(session);
                0
            }
            // revive_claude already printed its own loud `qd attach:`-prefixed error.
            Err(code) => code,
        };
    }

    attach_resolved(session, render)
}

/// The attach itself, from an ALREADY-RESOLVED row: the collision preflight, the
/// codex viewer, the lane, and every message. Split out of [`run`] for exactly
/// one second caller — `qd start`'s post-create handoff (FTUE punch R19).
///
/// # Why start calls this and not `run`
///
/// R19 makes `qd start <name>` end where `qd attach <name>` would have: the human
/// is looking at the session they just made, so making them type the second
/// command was the whole complaint. "Ends where attach would" has to mean the
/// SAME code, or the two drift — start would get a hand-rolled `ops.attach(&id)`
/// that quietly lacks the codex viewer, the cold-wake, the daemon redirect and
/// the `NotFound` revive, and the first bug report would be that attaching by
/// hand works and attaching from start does not.
///
/// What start does NOT reuse is everything ABOVE this line in [`run`]: `--no-attach`
/// is start's own flag with its own meaning (do not attach at all, rather than
/// revive-without-attaching), and the tombstone rejection is nonsense for a row
/// that was created microseconds ago. The resolution start does for itself, so
/// that a failure to find the session it just started is not fatal — see
/// [`attach_after_create`].
pub fn attach_resolved(session: &Session, render: RenderMode) -> i32 {
    // ADD-8 residual fix — live-id-collision PREFLIGHT over the RAW registry,
    // SHARED with resume (Pete feedback #6). The deduped join collapses two same-id
    // LIVE rows to one, so a bare `qd attach` would silently attach to the deduped
    // survivor. We refuse a genuine ≥2-alive collision LOUDLY here (before the lane
    // is even derived) so attach inherits the guard. A 1-alive session is NOT
    // refused (refuse_id_collision returns None for the single-alive case → normal
    // attach proceeds; we do NOT use alive_pid_for_id, which would block the
    // legitimate single-live attach).
    //
    // It runs FIRST, exactly where the retired `attach_resolved` MECHANIC (the
    // deleted lifecycle.rs helper, not this function) ran it: a collision is a
    // refusal to address the session at all, so it precedes every routing question.
    let env = dispatch::effects::RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    if let Some(code) = common::refuse_id_collision("attach", &session.session_id, &paths.sessions_dir)
    {
        return code;
    }

    // codex-interactive, use case 2: a LIVE daemon-hosted codex session CAN be
    // attached after all — not by giving the daemon a terminal, but by opening a
    // second client on it. `attach_codex_viewer` launches the codex TUI bound to
    // this session's own app server (`codex --remote <endpoint> resume
    // <thread-id>`), so a human can watch and type in the very thread an agent is
    // driving, with nothing stopped and nothing converted.
    //
    // KEPT IN QD ON PURPOSE (ruling J): `LaneOps::attach` answers `NotSupported`
    // for every daemon lane and must keep doing so — the viewer is a second client
    // on a running server, not a terminal for a session that has none, and folding
    // it into `attach` would make the contract claim daemon lanes are attachable.
    //
    // `None` means this row cannot host one (no endpoint, no thread id, or a
    // provider with no such affordance) — those fall through to the lane, which
    // answers `NotSupported`, and qd renders the honest redirect.
    //
    // NARROWED (2026-08-17): a `codex/app-server` row does NOT come through here.
    // That lane's `LaneOps::attach` opens the same viewer, so letting this special
    // case fire first would shadow the lane with a second implementation of its
    // own defining feature — the drift bug the split exists to prevent. The case
    // survives ONLY for `codex/daemon`, which has no attach of its own and would
    // otherwise lose an affordance it has today. When the two are unified, this is
    // the half to delete.
    //
    // `acp/opencode` is the worked example of doing it the right way round: it
    // has a viewer too (`opencode attach <url> --session <id>` on the bridge's own
    // HTTP server) and NO special case here at all — the lane owns it, the
    // provider guard below excludes it, and this block never sees it.
    //
    // The guard asks `has_viewer()` rather than `is_app_server()` so it keeps
    // meaning "the lane already does this" as more lanes acquire one. For a
    // `codex` row the two answers coincide today; if they ever stop, the wrong
    // one would shadow a lane's own viewer with this copy.
    let lane_opens_its_own_viewer = quorum_qw::lane_for(&session.provider, session.hosting.as_deref())
        .is_some_and(|l| l.has_viewer());
    if session.provider == "codex"
        && !lane_opens_its_own_viewer
        && matches!(session.status, SessionStatus::Idle | SessionStatus::Busy)
    {
        if let Some(code) = lifecycle::attach_codex_viewer(session) {
            return attached(session, code);
        }
    }

    // The lane, from the ROW's provider + hosting. `lane_for` is the drop-in for
    // the `row_hosting(&session.provider, session.hosting.as_deref())` this verb
    // used to spell out three times, and is deliberately STRICTER in two places
    // (an unknown provider is refused outright; a harness claiming a topology it
    // cannot have degrades to its structural default) — both pinned by
    // `dispatch::lane`'s agreement test.
    //
    // `None` ⇒ genuinely unknown provider ⇒ the pre-existing loud refusal. It
    // cannot return `None` for a provider `refuse_unknown_provider` waves through
    // (`claude-code` / `opencode` both resolve a harness), so the fallback below is
    // unreachable; it is spelled out rather than `unwrap`ed because an unreachable
    // arm that panics is a worse answer than an unreachable arm that exits 1.
    let Some(lane) = quorum_qw::lane_for(&session.provider, session.hosting.as_deref()) else {
        return common::refuse_unknown_provider("attach", session).unwrap_or(1);
    };

    let ops = dispatch::lane::open(lane, &env, paths);
    let id = SessionId(session.session_id.clone());

    match ops.attach(&id) {
        Ok(code) => attached(session, code),

        // A daemon lane has no terminal of its own. The viewer above already had
        // its chance, so this is the honest redirect, from the SAME helper as
        // before — now handed the `lane` resolved above so it can NAME the row it
        // is refusing. Three lanes land here and the message used to call every one
        // of them "codex"; see [`common::daemon_redirect`].
        Err(LaneError::NotSupported { .. }) => {
            common::daemon_redirect(lane, display_name(session))
        }

        // The one interesting arm, and the whole point of the rewrite: a cold
        // pane-hosted session auto-revives and we hand over the terminal. `wake`
        // routes to the right revive for the lane — claude / codex TUI / pi TUI —
        // so the three provider-guarded arms this used to need are one call.
        Err(LaneError::Cold { .. }) => wake_then_attach(ops.as_ref(), &id, session, render),

        // A row qd RESOLVED that the lane cannot ADDRESS. `row_for_id` is an exact,
        // REGISTRY-keyed lookup, while the join also emits `ColdJsonl` rows — a
        // claude transcript with no registry record behind it (a session run
        // outside qd, or one whose row was removed). Those show up in `qd ls` and
        // attach has always revived them, so reporting "no such session" for one
        // would be a capability regression, not a cleanup.
        //
        // Claude-only BY CONSTRUCTION, and guarded anyway: every other provider's
        // registry-less cold rows are daemon-hosted (`codex_cold` / `pi_cold` /
        // the opencode gather carry no `hosting` token, so `lane_for` gives them
        // their harness default) and are answered `NotSupported` above, before
        // this arm is reachable.
        Err(LaneError::NotFound { .. }) if session.provider == "claude-code" => {
            revive_outside_the_registry(session, render)
        }

        // Same shape as the arm above for a NON-claude row: `revive_claude` builds
        // a `claude … --resume <sid>` argv, so feeding it a foreign session id
        // would launch the wrong harness against an id it cannot read.
        Err(LaneError::NotFound { .. }) => {
            eprintln!(
                "qd attach: \"{}\" is a stopped {} session and qd cannot revive it in \
                 place. Start a fresh one with \"qd start <name> --provider {}\".",
                display_name(session),
                session.provider,
                session.provider
            );
            1
        }

        // Transport / Refused / the rest: the lane's own words, exit 1.
        Err(e) => {
            eprintln!("qd attach: {e}");
            1
        }
    }
}

/// FTUE punch **R19** — the handoff `qd start` makes after a successful create.
///
/// Resolves the row by the name the create just used and runs the ordinary
/// [`attach_resolved`] path, so a started session lands exactly where `qd attach
/// <name>` would have landed it.
///
/// # Why a resolution failure is not a failure
///
/// The session EXISTS by the time this is called — the create returned a handle,
/// the bind phase (on the claude lane) already proved the id is bound, and the
/// verdict line is already on stdout. If the fuzzy resolver cannot find it
/// anyway — a name that now collides, a registry read that lost a race — then
/// the thing that failed is the CONVENIENCE, not the start. Reporting exit 1
/// there would tell a caller the create failed when it did not, and would
/// silently change the exit contract of a verb whose codes are ruled (§3.5). So
/// the fallback is the pointer the user would otherwise have typed, and exit 0.
///
/// A resolution that SUCCEEDS hands its exit code straight through: from that
/// point on this is an attach, and an attach that fails should say so.
pub fn attach_after_create(name: &str, render: RenderMode) -> i32 {
    // `resolve_session_uncapped` prints its own loud not-found/ambiguous line, so
    // the caller is already told WHAT went wrong; this adds only what to do next.
    let Ok(session) = common::resolve_session_uncapped(name) else {
        eprintln!(
            "qd start: \"{name}\" was created but could not be resolved to attach to. \
             The session IS RUNNING — attach with \"qd attach {name}\"."
        );
        return 0;
    };
    attach_resolved(&session, render)
}

/// The cold path: revive, then hand over the terminal.
///
/// The revive is `LaneOps::wake`, which keeps the session's id and routes on
/// `(harness, mode)`; the handoff is `LaneOps::attach` again, re-resolving by that
/// same STABLE id so it targets the pane the revive just built rather than a
/// handle captured before it existed.
fn wake_then_attach(
    ops: &dyn LaneOps,
    id: &SessionId,
    session: &Session,
    render: RenderMode,
) -> i32 {
    // `None` for --cwd: `qd attach` has no such flag. It is `qd resume`'s, and
    // passing an invented value here would relocate a session the user never
    // asked to relocate.
    if let Err(e) = ops.wake(id, render, None) {
        // The lane's revives return TYPED errors and do not print — so the loud,
        // `qd attach:`-prefixed line is emitted HERE, carrying the core's own
        // message. Nothing is appended after it: a second pointer telling the user
        // to re-run the command that just failed is circular on the human verb.
        match e {
            // ATTRIBUTION, restored. `detail` is the revive core's own message
            // and `self_attributed` says whether it is already a complete line
            // (an `ERROR: …` refusal, a resident's `qd resume: …` Display) — so
            // the line a user reads is exactly the one `revive_claude(…,
            // "attach")` printed before the lane existed. The lane used to stamp
            // `qd wake:` on the body and wrap it in "could not revive claude
            // session …", which named a command nobody can type.
            //
            // The core's `exit_code` rides on the error now; this verb keeps
            // answering 1, which is what its handoff has always answered.
            LaneError::WakeFailed {
                detail,
                self_attributed,
                ..
            } => {
                if self_attributed {
                    eprintln!("{detail}");
                } else {
                    eprintln!("qd attach: {detail}");
                }
            }
            other => eprintln!("qd attach: {other}"),
        }
        return 1;
    }
    println!("Revived \"{}\"; attaching...", display_name(session));
    match ops.attach(id) {
        Ok(code) => attached(session, code),
        Err(e) => {
            eprintln!("qd attach: {e}");
            1
        }
    }
}

/// Revive a claude row the REGISTRY does not carry — the `ColdJsonl` join branch.
///
/// The lane cannot do this and should not pretend to: `LaneOps` is addressed by
/// [`SessionId`] and resolves it against the registry, which is what makes its
/// lookup cheap and unambiguous. A transcript-only row has no registry record to
/// find, so qd revives it from the row IT already holds — the one the join built —
/// and then attaches the handle the revive returns directly, since there is still
/// no registry row for a re-resolution to find.
fn revive_outside_the_registry(session: &Session, render: RenderMode) -> i32 {
    match resume::revive_claude(session, None, render, false, "attach") {
        Ok(handle) => {
            // Attach the now-live pane with a plain mux.attach (NO fused
            // `zmx attach … bash -lc` — the session is already up).
            println!("Revived \"{}\"; attaching...", handle.zmx_name);
            let mux = match common::real_mux() {
                Ok(m) => m,
                Err(code) => return code,
            };
            match mux.attach(&handle.socket_dir, &handle.zmx_name) {
                Ok(code) => attached(session, code),
                Err(e) => {
                    eprintln!("qd attach: {e}");
                    1
                }
            }
        }
        // revive_claude already printed its own loud, `qd attach:`-prefixed error
        // explaining the failure. Return that code as-is — do NOT append a second
        // recovery pointer that merely tells the user to re-run the command that
        // just failed — circular/confusing on the human verb.
        Err(code) => code,
    }
}
