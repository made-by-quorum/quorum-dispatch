//! Unified `qd send <target> <message>` selection and dispatch.
//!
//! Target resolution and carrier selection are deliberately separate. The
//! resolver produces one concrete session identity; the pure selector consumes
//! only that row's observable state; the dispatcher receives the same row and
//! never resolves a name, prefix, or PID on its own.

use clap::ArgMatches;

use dispatch::discovery::DiscoveryHealth;
use dispatch::effects::{Env, RealEnv};
use dispatch::idstore::IdMap;
use dispatch::launch::RenderMode;
use dispatch::model::{Session, SessionStatus};
use dispatch::origin_send::Refusal;
use quorum_qw::contract::{
    Confirmation, DeliverPolicy, LaneError, LaneOps, Message, MessageId, Receipt, ReceivePath,
    SessionId,
};

use super::carrier;
use super::common;
use super::intent;

// ===========================================================================
// WHAT USED TO BE HERE, and where it went
// ===========================================================================
//
// `UnifiedCarrier`, `select_carrier`, `dispatch_selected`, `trait Waker` and
// `RealWaker` are RETIRED. All five were qd's own copy of routing that
// `quorum_qw::lanes::LaneImpl` now owns:
//
//   - `select_carrier` keyed on `session.provider.as_str()` plus a
//     `row_hosting` re-derivation. `LaneOps::deliver` keys on the LANE — an
//     exhaustive `match (harness, mode)` — which is what put the old `"codex"`
//     and `"pi"` arms one guard away from routing a pane row into a daemon
//     carrier with no endpoint to reach.
//   - `dispatch_selected` was the one-call table between the selection and the
//     five carriers. The lane calls the SAME five functions directly, as
//     `quorum_qw::delivery` module functions — one body per carrier, shared with
//     the `qd send:relay` / `qd send:pty` verbs, rather than two a reader had to
//     compare by eye.
//   - `RealWaker` routed provider+hosting to a revive. `LaneOps::wake` routes
//     `(harness, mode)` to the same revives, and it HAS the `pi`/mux-pane arm
//     `RealWaker` never had — which is BUG 2, closed by this deletion rather
//     than by a note (see `quorum_qw::conformance`).
//
// What did NOT move, and must not: the disposition ledger (envelope append,
// `attempted`/`queued`/`delivered`/`delivery-failed` stamping, the claim lock,
// body-digest idempotency), fleet/remote-peer routing, target resolution, and
// the refusal RENDERING below — `report_refusal` still owns qd's wording and
// still downgrades an absence to `refused{receive-path-undetermined}` from THIS
// gather's `DiscoveryHealth`, which the lane cannot speak for.

/// What one carrier call answers with.
///
/// **The type MOVED to `quorum_qw::lanes` and is re-exported here**, unchanged in
/// every field, constructor and doc word. It was defined in this file when qd's
/// carriers were its only caller; `LaneOps::deliver` is its second, and a qw twin
/// converted at the seam would be a drift bug waiting for its first divergent
/// field. Every `super::send_unified::CarrierOutcome` path in `send.rs` /
/// `send_relay.rs` keeps resolving — a relocation, not an API change, the same
/// move `dispatch::lib`'s re-exports make for two dozen other modules.
///
/// `code` is still the carrier's UNCHANGED exit code, and [`deliver_then_stamp`]
/// still reads exactly that, so the disposition ledger's bytes do not move.
pub(super) use quorum_qw::lanes::CarrierOutcome;

#[derive(Debug, Clone, PartialEq, Eq)]
enum SendRefusal {
    Bare,
    NoLiveReceivePath,
    UnknownProvider(String),
}

/// A target's lifecycle liveness, from its one resolved registry/join snapshot.
/// A NOT-live target (`Cold`/`Killed`) is no longer a send refusal (qd–qf W3b:
/// "stopped is not a refusal class") — it is a WAKE trigger, and this is the ONE
/// place `qd send` decides which of the two paths a row takes.
///
/// **This reads the STATUS ENUM ALONE, and `LaneOps::health` does not.** Health
/// reads status PLUS `(pid, start_time)` through the `qd ls` liveness gate, so a
/// row with `status: "idle"` over a pid that is gone reads LIVE here and COLD
/// there. Both answers are deliberate — see `LaneOps::deliver`'s docs, which
/// carry the whole argument — and the disagreement is why the LIVE path passes
/// `wake_if_cold: false`: the lane must ATTEMPT such a row and let the carrier
/// report, never revive it off a projection this function does not share.
/// Reconciling the two readings is a separate, user-visible commit.
fn is_live(session: &Session) -> bool {
    matches!(
        session.status,
        SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Shell
    )
}

// THIS VERB HAS NO CARRIER, AND THAT IS THE POINT OF PHASE 3B.
//
// There WAS a local `trait UnifiedBackend` here with five signatures, and a
// `RealUnifiedBackend` implementing BOTH it and `quorum_qw::Carriers` — the
// callback `LaneOps::deliver` reached UP through to run a delivery whose body was
// a `qd` verb function. The twin trait went first; then phase 3B moved all five
// BODIES into `quorum_qw::delivery`, so `Carriers`, `RealUnifiedBackend` and
// `lane_ops_with_carriers` are all deleted and `lane_ops` is the only
// constructor. What this file kept is what was always qd's: the disposition
// ledger, the envelope append, the claim lock, the body-digest idempotency and
// the fleet/remote routing.

// ===========================================================================
// THE ATTEMPT — one delivery, already bound to its target
// ===========================================================================

/// ONE delivery attempt, with its target, its wake policy and its body already
/// bound. The durability wrapper below owns the LEDGER and knows nothing else;
/// this is the whole of what it calls.
///
/// It is a seam for two reasons, and neither is testing alone:
///
///  1. **A row with no lane still has a funnel.** An unknown-provider COLD row
///     (`provider: "mystery"`, the shape `tests/acceptance.rs` and
///     `tests/inbound_mode.rs` both drive) is not addressable by any lane, and
///     `RealWaker` used to answer it with `failed{wake}` from its fallthrough
///     arm. That row must keep its `attempted, queued, delivery-failed{wake}`
///     funnel and its exit 12, so [`Unwakeable`] answers exactly that and rides
///     the SAME ledger code as a real lane instead of a second copy of it.
///  2. The funnel shape stays provable without a live carrier or a live revive,
///     which is what the `deliver_with_durability` tests below have always done.
trait Attempt {
    fn run(&self) -> Result<Receipt, LaneError>;
}

/// The production attempt: `LaneOps::deliver`, which chooses the carrier itself
/// and — when the policy says so — performs the wake INSIDE the call.
struct LaneAttempt<'a> {
    ops: &'a dyn LaneOps,
    id: SessionId,
    policy: DeliverPolicy,
    message: String,
    /// The send id qd minted — and recorded in its intent log — BEFORE this
    /// attempt was built. See [`super::intent`]: the record has to be durable
    /// before the message crosses, so the id cannot come back on the receipt.
    send_id: String,
}

impl Attempt for LaneAttempt<'_> {
    fn run(&self) -> Result<Receipt, LaneError> {
        self.ops.deliver(
            &self.id,
            &Message {
                id: MessageId(self.send_id.clone()),
                // `from` is the SENDING session, and `qd send` has never carried
                // one into a carrier: all five take `(session, message)`. Passing
                // `None` is the honest answer, not a stub.
                text: self.message.clone(),
                from: None,
            },
            &self.policy,
        )
    }
}

/// A provider no lane can address, on a row that is NOT live.
///
/// `quorum_qw::lane_for` refuses an unknown provider outright, so there is no
/// `LaneOps` to ask — and the row still has to reach the ledger the way it does
/// today: envelope, `attempted`, `queued`, `delivery-failed{wake}`, exit 12. The
/// message is `RealWaker`'s fallthrough arm, carried across verbatim.
///
/// A LIVE row with an unknown provider never gets here: it is a sync
/// `refused{...}` with no envelope, exactly as `select_carrier`'s
/// `UnknownProvider` arm was.
struct Unwakeable {
    provider: String,
}

impl Attempt for Unwakeable {
    fn run(&self) -> Result<Receipt, LaneError> {
        Err(LaneError::WakeFailed {
            detail: format!(
                "provider \"{}\" cannot be woken headlessly",
                self.provider
            ),
            // qd's `failed{wake}` door code. Nothing produced this from a revive
            // core — there is no core — so it is spelled here rather than
            // carried, and the ledger tail below routes it through
            // `Refusal::failed`, which is where the 12 actually comes from.
            exit_code: 12,
            self_attributed: false,
        })
    }
}

fn is_self_send(env_id: Option<&str>, ids: &IdMap, target_session_id: &str) -> bool {
    env_id
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| dispatch::idstore::resolve_to_uuid(ids, raw))
        .is_some_and(|resolved| resolved == target_session_id)
}

fn resolve_self_session_id(env: &dyn Env) -> Result<Option<String>, i32> {
    let Some(raw) = env.var("QD_SESSION_ID").filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let ids_path = common::ids_store_path(env)?;
    let ids = dispatch::idstore::fold(&ids_path);
    Ok(dispatch::idstore::resolve_to_uuid(&ids, &raw))
}

/// Report a refusal.
///
/// `health` does NOT participate in ROUTING — routing is `LaneOps::deliver`'s,
/// keyed on the lane, and this function never sees it. What health changes is
/// whether an absence may be ASSERTED. `relay_port: None` produced
/// by a refused `ps` is not evidence of no relay — it is the absence of
/// evidence — so that case is reported as a distinct refusal CLASS
/// (`refused{receive-path-undetermined}`) carrying the underlying OS error,
/// instead of the flat "has no live receive path" claim.
///
/// Exit codes follow the contract (`origin_send` §6): the pre-existing
/// transport-shape refusals keep their generic `1` (pinned by `verbs_a4`), and
/// the NEW undetermined class rides the shared [`Refusal`] door code
/// [`EXIT_REFUSED`]. So a caller can separate the two cases on `$?` alone —
/// `1` means retrying will not help, `12` means retry with the access the
/// denied read needed — and on the machine-readable class token either way.
fn report_refusal(
    query: &str,
    session: &Session,
    refusal: SendRefusal,
    health: &DiscoveryHealth,
) -> i32 {
    let label = session.name.as_deref().unwrap_or(query);

    // The one case health is allowed to change: an absence we never observed.
    // Checked before the match so the codex-interactive Bare arm below keeps
    // its specific wording (a bare codex pane is undetermined for its OWN
    // reason, which a denied `ps` neither causes nor explains).
    if refusal == SendRefusal::NoLiveReceivePath && health.receive_path_undetermined() {
        let code = Refusal::refused(
            "receive-path-undetermined",
            format!(
                "the discovery read that would have found a receive path for \"{label}\" was \
                 refused, so relay and mux state are UNKNOWN — this is not the same as having \
                 no receive path"
            ),
        )
        .emit();
        report_degradation(health);
        return code;
    }

    match refusal {
        // codex-interactive: an interactive codex pane is Bare for a SPECIFIC and
        // temporary reason — codex does not open its rollout (and so discloses no
        // thread id) until someone types into the TUI. The generic wording is true
        // but reads like a broken session; say what it actually is and what clears
        // it, since the fix is one keystroke away and the session is perfectly fine.
        SendRefusal::Bare
            if session.provider == "codex"
                && dispatch::provider::row_hosting(&session.provider, session.hosting.as_deref())
                    == Some(dispatch::provider::Hosting::MuxPane) =>
        {
            eprintln!(
                "qd send: \"{label}\" has not been used yet, so codex has not opened a thread \
                 for it and qd has no id to send to. Type once in the session \
                 (\"qd attach {label}\") — the thread id binds on the next \"qd ls\"."
            )
        }
        SendRefusal::Bare => eprintln!(
            "qd send: found \"{label}\", but it has no bound session identity and is not receivable."
        ),
        SendRefusal::NoLiveReceivePath => eprintln!(
            "qd send: found \"{label}\", but it has no live receive path — not sendable."
        ),
        SendRefusal::UnknownProvider(provider) => eprintln!(
            "qd send: unknown provider \"{provider}\" for \"{label}\" — not sendable."
        ),
    }
    1
}

/// Print the evidence for a degraded gather, and the remedy when one applies.
/// Evidence is the raw OS error, preserved — `qd` reports what it observed and
/// lets the reader conclude what denied it.
fn report_degradation(health: &DiscoveryHealth) {
    eprintln!("qd send: reason: {}", health.evidence());
    if let Some(hint) = health.hint() {
        eprintln!("qd send: hint: {hint}");
    }
}

pub fn run_send_unified(m: &ArgMatches) -> i32 {
    // qd–qf W4 — THE MODE SPLIT (before any origin-mode resolution). INBOUND mode
    // (`--inbound-envelope`) admits a peer's already-minted envelope at the door
    // and takes NO positionals; ORIGIN mode is the existing W3a/W3b path. The two
    // are mutually exclusive; a mixed invocation is a SYNC arg refusal here (clap
    // made the positionals optional so inbound can omit them — this re-imposes the
    // per-mode requiredness, keeping origin's "requires <target> <message>"
    // contract with a clear `refused{args}`).
    let inbound = m.get_one::<String>("inbound-envelope");
    let session = m.get_one::<String>("session");
    let message_opt = m.get_one::<String>("message");
    if let Some(path) = inbound {
        // INBOUND mode: forbid the origin positionals + `--expires` (an inbound
        // envelope carries its own target/body/expires_at).
        if session.is_some() || message_opt.is_some() {
            return Refusal::refused(
                "args",
                "--inbound-envelope takes the address + body from the envelope; \
                 do not also pass <target> <message>",
            )
            .emit();
        }
        if m.get_one::<String>("expires").is_some() {
            return Refusal::refused(
                "args",
                "--expires is origin-mode only; an inbound envelope carries its own expires_at",
            )
            .emit();
        }
        // qd–qf W6: `--host` is origin-mode addressing only. An inbound envelope
        // carries its own raw `target` (with any `name@host` sugar inside it), so a
        // separate `--host` here is a contradiction the door names.
        if m.get_one::<String>("host").is_some() {
            return Refusal::refused(
                "args",
                "--host is origin-mode only; an inbound envelope carries its own target address",
            )
            .emit();
        }
        // qd–qf W3c: `--correlation-id` is origin-mode only. An inbound envelope
        // already carries its own origin-minted `correlation_id`; a separate supplied
        // id here would contradict (and the door keys idempotency on the envelope's
        // id, not this flag). Same posture as `--expires` + inbound.
        if m.get_one::<String>("correlation-id").is_some() {
            return Refusal::refused(
                "args",
                "--correlation-id is origin-mode only; an inbound envelope carries its own correlation_id",
            )
            .emit();
        }
        // The carrier flags are ORIGIN-mode only, for the same reason `--expires`,
        // `--host` and `--correlation-id` are: an envelope carries its own target,
        // body, expiry and correlation id, and it carries no wire choice and no
        // wait. Naming one here is a contradiction the door says out loud rather
        // than accepting and dropping.
        if let Some(flag) = carrier::inbound_conflict(m) {
            return Refusal::refused(
                "args",
                format!(
                    "{flag} is origin-mode only; an inbound envelope is admitted and \
                     delivered through the target's own receive path, with no wire to pin \
                     and nothing to block on"
                ),
            )
            .emit();
        }
        return run_inbound(&RealEnv, path);
    }

    // ORIGIN mode: the positionals are REQUIRED (clap-optional → runtime-checked).
    let (query, message) = match (session, message_opt) {
        (Some(q), Some(msg)) => (q, msg),
        _ => {
            return Refusal::refused(
                "args",
                "origin send requires <target> <message> (or use --inbound-envelope <path> \
                 to admit a peer's envelope)",
            )
            .emit();
        }
    };

    // qd–qf W3 part C: resolve the write-then-deliver expiry window UP FRONT so a
    // malformed `--expires` is a SYNC refusal (before any resolution / side
    // effect), routed through the shared Refusal type (part D). Absent ⇒ 12h.
    let expires_ms = match m.get_one::<String>("expires") {
        Some(raw) => match dispatch::origin_send::parse_expires(raw) {
            Ok(ms) => ms,
            Err(reason) => return dispatch::origin_send::Refusal::refused("expires", reason).emit(),
        },
        None => dispatch::origin_send::DEFAULT_EXPIRES_MS,
    };

    // qd–qf W3c (provider-contract §4): the OPTIONAL caller-supplied correlation_id.
    // Present ⇒ this id becomes the envelope's `correlation_id` (frame's ledger event
    // id rides through the one door), so the log envelope AND the stamped disposition
    // key on it; absent ⇒ qd mints its own ULID (the BARE-send default). Empty is a
    // SYNC refusal here (before any resolution / side effect) — an empty id is no id.
    let supplied_correlation_id = match m.get_one::<String>("correlation-id") {
        Some(id) if id.is_empty() => {
            return dispatch::origin_send::Refusal::refused(
                "correlation-id",
                "--correlation-id is empty; pass the caller's non-empty id or omit it to mint one",
            )
            .emit();
        }
        Some(id) => Some(id.clone()),
        None => None,
    };

    let env = RealEnv;

    // qd–qf W6 — ADDRESSING. Desugar `name@host` and reconcile it with `--host`
    // (both are addressing forms; the sugar desugars to the flag). Precedence
    // (SYNC, before any resolution / side effect):
    //   - the address's @host and --host, if BOTH present, MUST agree ⇒ else a sync
    //     `refused{host}` ("address says @X but --host says Y");
    //   - the effective host = --host ∨ @host ∨ None (bare = this host / local).
    let (name, addr_host) = parse_address(query);
    let flag_host = m.get_one::<String>("host").map(String::as_str);
    let effective_host = match (addr_host, flag_host) {
        (Some(a), Some(f)) if a != f => {
            return dispatch::origin_send::Refusal::refused(
                "host",
                format!(
                    "address \"{query}\" says @{a} but --host says {f} — the host qualifiers disagree"
                ),
            )
            .emit();
        }
        // Agree, or exactly one present, or neither: --host wins where present
        // (identical to @host when both are given), else @host, else local.
        (_, Some(f)) => Some(f),
        (Some(a), None) => Some(a),
        (None, None) => None,
    };

    // ORIGIN-MODE REMOTE SEND (the last P5 gap): a host-qualified target for a host
    // that is NOT this one AND has fleet state present. Resolve the name inside that
    // host's mirrored namespace (`remote/<h>/ls.json`, strict W7 read), then
    // APPEND-ONLY — no local delivery attempt, no disposition stamped by origin
    // (pending = absence, facts-only). The TARGET host's apply-driver presents the
    // never-attempted envelope; its door stamps the outcome; dispositions ride back
    // full-mesh. An absent/torn mirror or unknown/ambiguous name refuses
    // (`resolve_remote_target`); an empty host ("name@") is left to `resolve_target`
    // below (`refused{host}`). Self-host = `QD_HOST` / the "local" placeholder.
    if let Some(h) = effective_host {
        if !h.is_empty() && h != dispatch::dispositions::local_host(&env) {
            return match resolve_remote_target(name, h, &env) {
                Ok(()) => {
                    let paths = match common::paths_from_home(&env) {
                        Ok(paths) => paths,
                        Err(code) => return code,
                    };
                    origin_remote_send(
                        &env,
                        &paths,
                        query,
                        message,
                        expires_ms,
                        supplied_correlation_id,
                    )
                }
                Err(refusal) => refusal.emit(),
            };
        }
    }

    // Resolve the caller's handle exactly once, through the SHARED W6 resolver
    // (host-aware: local resolution for bare/@local, the single-machine
    // no-fleet-state refusal for a foreign host). All later refresh/revalidation
    // uses this row's immutable provider session id, never the caller's possibly
    // ambiguous name/prefix or host qualifier. Origin now renders the resolver's
    // outcomes through the shared Refusal (refused{unknown}/refused{ambiguous},
    // exit 12) — consistent with the W4 inbound door.
    let target = match resolve_target(name, effective_host, &env) {
        Ok(session) => session,
        Err(refusal) => return refusal.emit(),
    };

    // Verb-entry self-send fence: QD_SESSION_ID is resolved through the same
    // idstore chain whoami owns. It runs before lifecycle/carrier selection, and
    // is not reported as a carrier failure. qd–qf W3 part D: the self-send sync
    // refusal renders through the shared Refusal {class,reason} type.
    let self_session_id = match resolve_self_session_id(&env) {
        Ok(value) => value,
        Err(code) => return code,
    };
    if self_session_id.as_deref() == Some(target.session_id.as_str()) {
        let label = target.name.as_deref().unwrap_or(query);
        return dispatch::origin_send::Refusal::refused(
            "self-send",
            format!("\"{label}\" — QD_SESSION_ID resolves to the target session"),
        )
        .emit();
    }

    // qd–qf W3b: a stopped/tombstoned target is NO LONGER rejected here — "stopped
    // is not a refusal class". It is a WAKE trigger, handled inside the durability
    // boundary below (log the envelope FIRST, then wake, then deliver). Only a
    // TRULY BARE session (no bound identity) stays an immediate sync refusal — a
    // bare row has nothing to wake and nothing to receive, so we refuse before
    // logging any envelope (unchanged).
    if target.session_id.is_empty() {
        // Bareness is read straight off the registry row, not from any process
        // or mux probe, so no discovery failure can manufacture or mask it —
        // a clean health is the honest thing to report against here.
        return report_refusal(query, &target, SendRefusal::Bare, &DiscoveryHealth::default());
    }

    // The join intentionally deduplicates stale rows, so inspect the raw live
    // registry before acting. Two live rows with one provider session id cannot
    // be safely bound to one carrier endpoint.
    let paths = match common::paths_from_home(&env) {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    if let Some(code) =
        common::refuse_id_collision("send", &target.session_id, &paths.sessions_dir)
    {
        return code;
    }

    // Refresh only by the resolved full session id. This closes ordinary
    // resolve-to-attempt state changes (death, relay loss/appearance, mux loss)
    // without ever allowing a replacement name/prefix match to redirect the
    // message. The wake/selection below uses this current observable snapshot.
    //
    // Carried WITH its discovery health: selection runs on THIS snapshot, so a
    // refusal must be explained by THIS gather's reads, not the resolve-time
    // ones. Without it a `relay_port` nulled by a refused `ps` is
    // indistinguishable from a session that genuinely has no relay.
    let (current, health) = match common::resolve_session_uncapped_with_health(&target.session_id) {
        Ok((session, health)) if session.session_id == target.session_id => (session, health),
        Ok(_) => {
            eprintln!("qd send: target identity changed before delivery — refusing to send.");
            return 1;
        }
        Err(_) => {
            eprintln!("qd send: target disappeared before delivery — refusing to send.");
            return 1;
        }
    };

    // THE ROW'S LANE. `lane_for` is the routing `select_carrier` used to do off a
    // provider string plus a `row_hosting` re-derivation, and `None` here means
    // exactly what `SendRefusal::UnknownProvider` meant: no lane can address this
    // provider. The two halves of that answer are NOT symmetric and never were —
    // a LIVE unknown-provider row is a sync exit-1 refusal with no envelope, and a
    // NOT-LIVE one runs the whole write-then-deliver funnel to a
    // `delivery-failed{wake}` (`tests/acceptance.rs`'s §6 scenario and
    // `tests/inbound_mode.rs`'s "mystery" rows both drive the second). So the
    // liveness split below owns both, and the no-lane case rides it through
    // [`Unwakeable`] rather than through a second copy of the ledger.
    let lane = quorum_qw::lane_for(&current.provider, current.hosting.as_deref());

    // THE WIRE, if the caller named one. `from_send_matches` answers `None`
    // unless `--carrier` or `--wait` is present, and on `None` control falls
    // straight through to the live/not-live split below — so the DEFAULT `qd
    // send` keeps `LaneOps::deliver`, its write-then-deliver envelope, its claim
    // lock and its disposition stamps byte-for-byte. These flags add a path;
    // they do not re-tune the one that exists.
    //
    // Placed HERE, after the whole front door has run (address desugaring,
    // `resolve_target`, the self-send fence, the id-collision refusal, the by-id
    // refresh) and after the row's lane is in hand: a `qd send` flag must not
    // move `qd send`'s door, and the `--wait` gate needs the lane to answer.
    // Everything past this point that the carrier arm skips is skipped
    // DELIBERATELY — a pinned wire runs the carrier's own ledger discipline
    // (`send:pty`'s intent record, `send:relay`'s relay send events), which is
    // the discipline the reply-capture machinery it reuses was built against.
    if let Some(req) = carrier::from_send_matches(m) {
        return carrier::run_from_unified(&current, lane, is_live(&current), query, message, &req);
    }

    // qd–qf W3b: the LIVE vs NOT-live split, decided by [`is_live`] — the STATUS
    // ENUM alone, unchanged. A LIVE target takes the byte-identical W3a path: ask
    // the lane whether there is anywhere to receive FIRST (a transport-shape
    // refusal is an immediate exit-1 with NO envelope logged, exactly as today),
    // then write-then-deliver with NO wake. A NOT-live target is no longer
    // refused: it is resume-and-deliver — log the envelope FIRST, then one atomic
    // `deliver` that wakes and delivers, and a wake that cannot succeed is a
    // `failed{wake}` stamped against the logged envelope, exit 12.
    if is_live(&current) {
        let Some(lane) = lane else {
            return report_refusal(
                query,
                &current,
                SendRefusal::UnknownProvider(current.provider.clone()),
                &health,
            );
        };
        // THE PRE-FLIGHT, and the reason `LaneOps::receive_path` exists. qd logs
        // the envelope BEFORE the carrier runs and treats a failed append as
        // fatal; a live target with nothing to receive through is an immediate
        // exit-1 that logs NO envelope and stamps NO disposition
        // (`verbs_a4::send_live_unroutable_claude_is_unchanged_no_wake_no_envelope`).
        // Folding that verdict inside an atomic `deliver` would put the append
        // first and move the ledger's bytes. `receive_path` is the pre-flight the
        // contract SANCTIONS for exactly this: topology only, side-effect-free,
        // and — enforced by a source scan in `quorum_qw::conformance` — never an
        // INPUT to `deliver`. Its answer is rendered here and DISCARDED; the lane
        // determines the carrier again, itself, from its own fresh read.
        let ops = dispatch::lane::open(lane, &env, paths.clone());
        let id = SessionId(current.session_id.clone());
        match ops.receive_path(&id) {
            Ok(ReceivePath::Available) => {}
            // `None` is a positive observation of absence; `Undetermined` is the
            // lane's own denied read. BOTH render through `report_refusal`,
            // because the DOWNGRADE to `refused{receive-path-undetermined}` is
            // decided by THIS gather's `DiscoveryHealth` — the reads that produced
            // the row qd is holding — and the lane cannot speak for those. An
            // `Err` (a row the registry cannot key, a transport that failed
            // answering) is the same user-visible fact: there is no live receive
            // path, exit 1, no envelope.
            Ok(ReceivePath::None { .. }) | Ok(ReceivePath::Undetermined { .. }) | Err(_) => {
                return report_refusal(query, &current, SendRefusal::NoLiveReceivePath, &health)
            }
        }
        // qd–qf W3 part A: WRITE-THEN-DELIVER. Log the envelope BEFORE delivery
        // (hard-fail if the append errors), stamp `attempted`, deliver through the
        // lane, then stamp the witnessed outcome (best-effort).
        deliver_with_durability(
            &env,
            &paths,
            &LaneAttempt {
                ops: ops.as_ref(),
                id,
                // NO WAKE on the live path — `is_live` already said this row is
                // live, and the lane's own `health` disagrees about exactly the
                // stale-live rows `verbs_a4` pins as must-not-wake. `deliver`
                // ATTEMPTS on `false` rather than refusing, which is what keeps
                // that fixture's `attempted, delivery-failed{delivery}` funnel.
                policy: DeliverPolicy {
                    wake_if_cold: false,
                    // Ignored when no wake happens, which is the whole point here.
                    render: RenderMode::default(),
                    ..DeliverPolicy::default()
                },
                message: message.clone(),
                send_id: intent::record_send_intent(
                    &env,
                    &dispatch::effects::RealClock,
                    Some(&current.session_id),
                    current.name.as_deref(),
                    intent::VERB_SEND,
                    message,
                ),
            },
            query,
            message,
            expires_ms,
            supplied_correlation_id,
        )
    } else {
        // Render mode for the wake (a not-live target is revived into a fresh
        // pane). The `send` verb has NO `--alt-screen`/`--inline` flags, so this is
        // FLAG-LESS: `render-default` config > the inline default (never
        // `m.get_flag`, which would panic on the flag-less `send` subcommand).
        // Resolved ONLY on this branch, exactly as before — the live path never
        // read the config and still does not.
        let render = dispatch::launch::resolve_render_mode(
            None,
            common::render_default_from_config(&env).as_deref(),
        );
        // No `receive_path` pre-flight here, and that is deliberate: a cold row
        // has no receive path YET, and the envelope is logged before the wake so
        // that a `delivery-failed{wake}` has an envelope to join on. A revive that
        // reports success but leaves an unroutable row is the lane's own
        // `WakeFailed` (its `no_live_receive_path` helper answers exactly that
        // when a wake happened), which lands the same `delivery-failed{wake}` +
        // exit 12 `wake_then_deliver` used to land here.
        match lane {
            Some(lane) => {
                let ops = dispatch::lane::open(lane, &env, paths.clone());
                deliver_with_durability(
                    &env,
                    &paths,
                    &LaneAttempt {
                        ops: ops.as_ref(),
                        id: SessionId(current.session_id.clone()),
                        policy: DeliverPolicy {
                            wake_if_cold: true,
                            render,
                            ..DeliverPolicy::default()
                        },
                        message: message.clone(),
                        send_id: intent::record_send_intent(
                            &env,
                            &dispatch::effects::RealClock,
                            Some(&current.session_id),
                            current.name.as_deref(),
                            intent::VERB_SEND,
                            message,
                        ),
                    },
                    query,
                    message,
                    expires_ms,
                    supplied_correlation_id,
                )
            }
            None => deliver_with_durability(
                &env,
                &paths,
                &Unwakeable {
                    provider: current.provider.clone(),
                },
                query,
                message,
                expires_ms,
                supplied_correlation_id,
            ),
        }
    }
}

// ===========================================================================
// qd–qf W4 — INBOUND MODE ("THE ONE DOOR")
// ===========================================================================

/// qd–qf W6 — split a raw address into `(name, host)` on the LAST `@`.
///
/// `name@host` is SUGAR over `--host` (TRANSITION §3 / §7 Q2 RULED): the address
/// `"alpha@devbox"` ⇒ `("alpha", Some("devbox"))`; a bare `"alpha"` (or a stable_id,
/// which never contains `@`) ⇒ `("alpha", None)`. We split on the LAST `@` because
/// neither names nor stable_ids contain `@`; an address is at most one `name@host`
/// pair, and a stray leading `@` in the name half is caught downstream as an empty
/// name. `"@host"` ⇒ `("", Some("host"))` (empty name — the caller refuses);
/// `"name@"` ⇒ `("name", Some(""))` (empty host — the caller refuses).
fn parse_address(raw: &str) -> (&str, Option<&str>) {
    match raw.rsplit_once('@') {
        Some((name, host)) => (name, Some(host)),
        None => (raw, None),
    }
}

/// qd–qf W6 — the SHARED target resolver for BOTH origin and inbound `qd send`.
/// Generalizes the former `resolve_inbound_target`: `name` is the bare handle
/// (name | stable_id | prefix | …), `host` is the OPTIONAL host qualifier (from a
/// `name@host` sugar OR the `--host` flag). Renders the resolver's outcomes
/// through [`Refusal`] (the `{class,reason}` family, exit 12) — never a first-match.
///
/// Host dispatch (TRANSITION §3 + provider-contract §2/Annex A):
///   - **host is None, OR host == [`dispatch::dispositions::local_host`]** ⇒ LOCAL resolution: the
///     uncapped gather + `resolve_session_with_liveness` with the SAME pid-aware
///     predicate the acting verbs use. `One` ⇒ Ok; `None` ⇒ `refused{unknown}`;
///     `Many` ⇒ `refused{ambiguous}` (never guess). A gather/list-build failure
///     (HOME unset etc.) surfaces as its own printed exit code, wrapped so the door
///     returns an exit not a panic.
///   - **host is Some(h), h != local** ⇒ HOST-QUALIFIED. On this single-machine box
///     (no `remote/<h>/` populated) ⇒ `refused{no-fleet-state}` (fail-closed): a
///     host-qualified address with no fleet state for that host refuses with a named
///     reason, bare/local is unaffected.
///
///     BOUNDARY (out of scope this pass): if `remote/<h>/ls.json` EXISTS, resolving
///     within that host's namespace is FLEET behavior driven by out-of-scope movers
///     — this pass does NOT build cross-host delivery, so a present-but-remote
///     target is NOT handled here. We implement only the absent-fleet-state refusal,
///     which is the single-machine contract.
fn resolve_target(name: &str, host: Option<&str>, env: &dyn Env) -> Result<Session, Refusal> {
    use dispatch::effects::is_pid_alive;
    use dispatch::join::JoinOpts;
    use dispatch::resolve::{is_live_status, resolve_session_with_liveness, Resolution};

    // Host dispatch: a Some(host) that is NOT this host is host-qualified. An empty
    // host ("name@") is a malformed address — refuse loudly rather than silently
    // treating it as local.
    if let Some(h) = host {
        if h.is_empty() {
            return Err(Refusal::refused(
                "host",
                format!("address \"{name}@\" has an empty host — drop the trailing @ for a local send"),
            ));
        }
        let local = dispatch::dispositions::local_host(env);
        if h != local {
            // Single-machine contract (provider-contract §2 Amendment / Annex A):
            // absent fleet state ⇒ a host-qualified address refuses fail-closed.
            // `remote/<h>/` being absent IS the absent-fleet-state condition; we do
            // not attempt cross-host resolution in this pass (see BOUNDARY above).
            return Err(Refusal::refused(
                "no-fleet-state",
                format!(
                    "host-qualified address for host \"{h}\" but no fleet state for it on this host"
                ),
            ));
        }
        // else: h == local ⇒ fall through to LOCAL resolution (name@local ≡ bare).
    }

    // An empty name ("@host", or a bare "") never resolves to a session — refuse
    // loudly instead of gathering and returning an opaque `unknown`.
    if name.is_empty() {
        return Err(Refusal::refused(
            "address",
            "address has an empty name — nothing to resolve".to_string(),
        ));
    }

    // The SAME uncapped gather the sealed resolver runs (include_all +
    // include_tombstoned, no preview, no cap) — a capped resolution stays
    // unexpressible here too.
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: false,
        limit: None,
    };
    let sessions = common::all_sessions(opts).map_err(|_code| {
        // `all_sessions` already printed the concrete cause (e.g. HOME unset). Give
        // the door a machine-readable refusal too; the printed cause stays visible.
        Refusal::refused(
            "unknown",
            format!("could not resolve target \"{name}\" (session store unavailable)"),
        )
    })?;

    // The pid-aware liveness predicate `resolve_or_die` uses (a dead-pid row whose
    // on-disk status still says idle/busy does not count as live), so the
    // ambiguity refinement matches the acting verbs exactly.
    let is_alive = |s: &Session| {
        is_live_status(s.status)
            && match s.pid {
                Some(p) if p != 0 => is_pid_alive(p as i32),
                _ => true,
            }
    };

    match resolve_session_with_liveness(name, &sessions, is_alive) {
        Resolution::One(s) => Ok(s.clone()),
        Resolution::None => Err(Refusal::refused(
            "unknown",
            format!("no session matching \"{name}\""),
        )),
        Resolution::Many(v) => Err(Refusal::refused(
            "ambiguous",
            format!("\"{name}\" matches {} sessions — refusing to guess", v.len()),
        )),
    }
}

/// The outcome of resolving a bare handle within a peer mirror's session rows.
enum MirrorResolve {
    One,
    None,
    Many,
}

/// Resolve `name` within a peer mirror's opaque `sessions` rows (`qd ls --json`
/// shape). A row matches on an EXACT `name`/`sessionId`/`qdId`, else (only when no
/// exact match exists) on a `name`/`qdId` PREFIX (git-short-ref style). One ⇒
/// resolvable; None ⇒ `unknown`; Many ⇒ `ambiguous` (never guess). Pure over the
/// rows so the resolution semantics are unit-testable without the filesystem.
fn resolve_name_in_mirror(sessions: &[serde_json::Value], name: &str) -> MirrorResolve {
    let field = |row: &serde_json::Value, key: &str| {
        row.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let exact = sessions
        .iter()
        .filter(|r| {
            field(r, "name").as_deref() == Some(name)
                || field(r, "sessionId").as_deref() == Some(name)
                || field(r, "qdId").as_deref() == Some(name)
        })
        .count();
    if exact == 1 {
        return MirrorResolve::One;
    }
    if exact > 1 {
        return MirrorResolve::Many;
    }
    // No exact match — fall back to a prefix match (name or the short qd id).
    let prefix = sessions
        .iter()
        .filter(|r| {
            field(r, "name").is_some_and(|n| n.starts_with(name))
                || field(r, "qdId").is_some_and(|q| q.starts_with(name))
        })
        .count();
    match prefix {
        0 => MirrorResolve::None,
        1 => MirrorResolve::One,
        _ => MirrorResolve::Many,
    }
}

/// Resolve a host-qualified ORIGIN target within a foreign host's mirror
/// (`remote/<host>/ls.json`, the strict W7 read). `Ok(())` ⇒ the name identifies
/// exactly one session there; the caller appends the envelope (raw target string)
/// and lets THAT host's apply-driver deliver. Refusals mirror the local resolver's
/// family: empty name ⇒ `address`; absent mirror ⇒ `no-fleet-state` (the
/// single-machine contract, unchanged); torn/`v!=1` ⇒ `torn-mirror`; no match ⇒
/// `unknown`; many ⇒ `ambiguous`. A stale-but-readable mirror STILL resolves —
/// staleness is surfaced at `qd ls`; a dead letter dies by its own `expires_at`.
fn resolve_remote_target(name: &str, host: &str, env: &dyn Env) -> Result<(), Refusal> {
    use super::mirror::{read_mirror, MirrorRead};
    if name.is_empty() {
        return Err(Refusal::refused(
            "address",
            "address has an empty name — nothing to resolve".to_string(),
        ));
    }
    // The mirror honors QD_HOME (from_home_env) — the SAME resolution `qd ls --host`
    // and the transport writers use, not the `.claude`-layout registry root.
    let paths = match common::paths_from_home(env) {
        Ok(paths) => paths,
        Err(_code) => {
            return Err(Refusal::refused(
                "no-fleet-state",
                format!("host-qualified address for host \"{host}\" but the session store is unavailable"),
            ))
        }
    };
    let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, env);
    match read_mirror(&tpaths, host) {
        MirrorRead::Absent { host } => Err(Refusal::refused(
            "no-fleet-state",
            format!("host-qualified address for host \"{host}\" but no fleet state for it on this host"),
        )),
        MirrorRead::Torn { host, why } => Err(Refusal::refused(
            "torn-mirror",
            format!("mirror for host \"{host}\" is unreadable: {why}"),
        )),
        MirrorRead::Ok(mirror) => match resolve_name_in_mirror(&mirror.sessions, name) {
            MirrorResolve::One => Ok(()),
            MirrorResolve::None => Err(Refusal::refused(
                "unknown",
                format!("no session matching \"{name}\" on host \"{host}\""),
            )),
            MirrorResolve::Many => Err(Refusal::refused(
                "ambiguous",
                format!("\"{name}\" matches more than one session on host \"{host}\" — refusing to guess"),
            )),
        },
    }
}

/// Origin-mode REMOTE send: APPEND the envelope to our own `log.jsonl` (the write
/// half) and STOP — no local delivery attempt, no disposition stamped by origin
/// (facts-only: pending = absence). The target host's apply-driver sees a
/// never-attempted envelope (⇒ due), delivers through its own door, and the
/// `delivered` disposition rides back full-mesh. Exit 0 + print the correlation_id.
/// R15 idempotency is preserved: same id + same body + already delivered ⇒ no-op;
/// body mismatch ⇒ `refused{body-mismatch}`; same id + same body not-yet-delivered
/// ⇒ idempotent no-op success (already durable + pending — never re-append, never
/// stamp).
fn origin_remote_send(
    env: &dyn Env,
    paths: &dispatch::paths::QdPaths,
    raw_target: &str,
    message: &str,
    expires_ms: i64,
    supplied_correlation_id: Option<String>,
) -> i32 {
    use dispatch::dispositions;
    use dispatch::effects::{Clock, RealClock};
    use dispatch::origin_send::{build_envelope, mint_correlation_id};

    let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, env);
    let clock = RealClock;
    let authored_at = clock.now_ms();
    let correlation_id = supplied_correlation_id.unwrap_or_else(|| mint_correlation_id(&clock));
    let origin = dispositions::local_host(env);

    // R15 CLAIM LOCK — serialize concurrent same-id origin submits + the body check.
    let _claim = match dispositions::acquire_claim(&tpaths, &correlation_id) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("qd send: could not acquire the delivery claim for {correlation_id} ({e}) — not sent.");
            return 1;
        }
    };

    let presented_digest = dispatch::origin_send::body_digest(message);
    if let Some(prior) = dispositions::logged_envelope(&tpaths, &correlation_id) {
        if dispatch::origin_send::body_digest(&prior.body) != presented_digest {
            return Refusal::refused(
                "body-mismatch",
                format!("{correlation_id} is already in the log with a different body — refusing to re-submit a conflicting body under the same id"),
            )
            .emit();
        }
        // Same body already logged. A `delivered` event ⇒ no-op success; otherwise
        // the envelope is still durable and pending (the target's apply-driver has
        // it) — an idempotent no-op success, NEVER a re-append, NEVER an origin stamp.
        match dispositions::recorded_delivered_digest(&tpaths, &correlation_id) {
            Ok(Some(_)) => {
                eprintln!("qd send: {correlation_id} already delivered — no-op");
                return 0;
            }
            Ok(None) => {
                println!("{correlation_id}");
                return 0;
            }
            Err(e) => {
                eprintln!("qd send: could not read the disposition ledger for {correlation_id} ({e}) — not sent.");
                return 1;
            }
        }
    }

    // Write half: `target` is the RAW caller address (R9.4), `body` verbatim,
    // correlation_id minted/carried exactly as a local send, default 12h expiry. No
    // attempted/queued row — origin never attempts a remote delivery.
    let envelope = build_envelope(
        correlation_id.clone(),
        authored_at,
        expires_ms,
        raw_target.to_string(),
        origin,
        dispatch::origin_send::caller_session_id(env),
        message.to_string(),
    );
    if let Err(e) = dispositions::append_envelope(&tpaths, &envelope) {
        eprintln!("qd send: could not durably record the message ({e}) — not sent.");
        return 1;
    }
    println!("{correlation_id}");
    0
}

/// qd–qf W4 — INBOUND MODE. Admit a peer's ALREADY-minted envelope at the door,
/// validate it, be idempotent on its id, and (resume-and-)deliver it — WITHOUT
/// ever appending to this host's own `log.jsonl` (my log = envelopes I
/// ORIGINATED; the peer's envelope lives in the mirror). Under R14.2 event rows
/// are FULLY NORMALIZED: they carry ONLY `{v, correlation_id, event, created_at}`
/// (+ `class` on refused/delivery-failed) — NO `witness`, NO copied `origin`, NO
/// copied `authored_at`. `created_at` = when THIS host recorded the moment; the
/// peer's `origin`/`authored_at` live on the envelope in the mirror and JOIN by
/// `correlation_id`.
///
/// Door order (validate cheap→expensive, side-effect-free until delivery):
///   1. READ the envelope bytes (`<path>`, or stdin for `-`). IO error ⇒ error.
///   2. PARSE into [`Envelope`] via serde; a parse failure / `v != 1` / a missing
///      field ⇒ stderr-only `refused{malformed}` — NO row (no trustworthy id to
///      key one on; R14.3 malformed carve-out).
///   3. PAST-EXPIRY: `expires_at < now` ⇒ stamp `refused{past-expiry}` THEN the
///      stderr refusal + exit 12 (R14.3: a parse-valid inbound refusal rides IN
///      the funnel; `expired` stays a DERIVED view state, never authored).
///   4. RESOLVE the envelope's `target` (desugar `name@host`, then [`resolve_target`]):
///      unknown ⇒ `refused{unknown}`, ambiguous ⇒ `refused{ambiguous}` (never
///      first-match); a host-qualified address ⇒ the single-machine no-fleet-state
///      refusal — each stamps a `refused{class}` row (parse-valid id) then emits.
///   5. IDEMPOTENCY: a `delivered` event already present for `correlation_id`
///      ([`dispatch::dispositions::has_delivered_event`]) ⇒ NO-OP SUCCESS
///      (deliver nothing, stamp nothing, exit 0). Delivery is irreversible;
///      idempotence keys on the delivered event EXISTING (R8) — a
///      `delivery-failed` row does NOT block the retry.
///   6. ADMIT + (resume-and-)DELIVER: `accepted` is RETIRED (R14.3) — admission
///      is marked by `attempted`; a not-live target additionally emits `queued`
///      and is WOKEN inside `LaneOps::deliver` (`wake_if_cold`) before delivery;
///      a wake that cannot succeed stamps `delivery-failed{class}` (exit 12). A
///      live target with no receive path — asked of [`LaneOps::receive_path`],
///      which is topology-only and never an input to `deliver` — stamps
///      `refused{no-live-receive-path}` (NO `attempted`; the R12 family split,
///      now a refused row) + refusal exit. NO envelope log append (contract §4).
///   7. STAMP the outcome (`delivered` / `delivery-failed{delivery}`) via the
///      SHARED [`deliver_then_stamp`] tail — best-effort append.
///
/// Seamed (`env` injected) so the whole door is proven with a jailed store. It
/// never picks a carrier — the lane does, from its own fresh read of the row.
fn run_inbound(env: &dyn Env, envelope_arg: &str) -> i32 {
    use dispatch::dispositions;
    use dispatch::effects::{Clock, RealClock};

    // (1) READ the envelope bytes: `-` ⇒ stdin, else the path.
    let bytes = match read_envelope_bytes(envelope_arg) {
        Ok(b) => b,
        Err(e) => {
            // An unreadable source is not a DOOR refusal (nothing to validate yet)
            // — it is a plain IO error with a clear message + generic exit.
            eprintln!("qd send: could not read inbound envelope from {envelope_arg} ({e}) — not admitted.");
            return 1;
        }
    };

    // (2) PARSE into the leaf Envelope. serde rejects a missing REQUIRED field or a
    // type mismatch; we reject any `v != 1` EXPLICITLY (never guess a version).
    let envelope: dispatch::dispositions::Envelope = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(e) => {
            return Refusal::refused(
                "malformed",
                format!("inbound envelope is not a valid v1 envelope: {e}"),
            )
            .emit();
        }
    };
    if envelope.v != 1 {
        return Refusal::refused(
            "malformed",
            format!("unsupported envelope version {} (this qd speaks v1)", envelope.v),
        )
        .emit();
    }

    let clock = RealClock;
    let now = clock.now_ms();

    // The transport files honor QD_HOME (from_home_env), matching the store + the
    // W5 reader. Resolve them UP FRONT so every parse-valid door refusal below can
    // stamp its `refused{class}` row IN the funnel (R14.3). A HOME-unset failure
    // here is a plain IO error (surface it) — the same failure `resolve_target`'s
    // gather would raise.
    let paths = match common::paths_from_home(env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, env);

    // Stamp a `refused{class}` row for a parse-valid inbound door refusal (R14.3 —
    // the refusal class rides IN the funnel), then emit the shared stderr refusal
    // and return its exit code. `created_at` = now (this host recorded it). NO
    // `attempted` precedes a refusal that never admitted the message.
    let stamp_refused = |refusal: Refusal, class: &str| -> i32 {
        stamp_event(
            &tpaths,
            &dispatch::dispositions::DispositionEvent::refused(
                envelope.correlation_id.clone(),
                clock.now_ms(),
                class.to_string(),
            ),
        );
        refusal.emit()
    };

    // (3) PAST-EXPIRY door — a past-expiry inbound envelope is REFUSED at the door;
    // R14.3 stamps a `refused{past-expiry}` row (the refusal rides IN the funnel),
    // NOT an `expired` row (that is a DERIVED view state, never authored).
    if envelope.expires_at < now {
        return stamp_refused(
            Refusal::expired(
                "past-expiry",
                format!(
                    "envelope {} expired at {} (now {})",
                    envelope.correlation_id, envelope.expires_at, now
                ),
            ),
            "past-expiry",
        );
    }

    // (4) RESOLVE the ENVELOPE's target (mis-addressed / ambiguous ⇒ named refusal).
    // qd–qf W6: desugar the envelope's `target` `name@host` too — an inbound
    // envelope carries a raw address string, so a host qualifier in it routes the
    // SAME shared resolver (host-qualified ⇒ the single-machine no-fleet-state
    // refusal; `@local`/bare ⇒ local resolution). Each parse-valid resolution
    // refusal stamps a `refused{class}` row before emitting (R14.3).
    let (in_name, in_host) = parse_address(&envelope.target);
    let target = match resolve_target(in_name, in_host, env) {
        Ok(s) => s,
        Err(refusal) => {
            let class = refusal.class.clone();
            return stamp_refused(refusal, &class);
        }
    };

    // (5) CLAIM LOCK + IDEMPOTENCY/INTEGRITY (R15). Acquire the per-correlation_id
    // claim lock and HOLD it across check→deliver→stamp (the `_claim` guard lives
    // to the end of this fn): concurrent presentations of the SAME id SERIALIZE,
    // so the winner delivers+stamps and the loser then re-reads and resolves
    // against the winner's fact (closes the check-then-act double-delivery race,
    // security audit #1). A lock error fails CLOSED (never proceed unserialized).
    let _claim = match dispositions::acquire_claim(&tpaths, &envelope.correlation_id) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("qd send: could not acquire the delivery claim for {} ({e}) — not admitted.", envelope.correlation_id);
            return 1;
        }
    };

    // The R15 integrity digest of the PRESENTED body (hex sha-256 of the parsed
    // body string — trailing-newline-trim safe).
    let presented_digest = dispatch::origin_send::body_digest(&envelope.body);

    // Under the lock: read the digest bound to this id by an existing `delivered`
    // event. A `delivered` event present ⇒ the prose already landed (irreversible,
    // R8 idempotence — a `delivery-failed` row does NOT block a retry). R15 then
    // compares bodies: SAME body ⇒ no-op success (idempotent apply); DIFFERENT
    // body ⇒ `refused{body-mismatch}` (the id binds exactly one body — a
    // different-body presentation is by construction a violation; stamp the
    // refused row per R14.3 + refuse). A read error is NOT treated as "absent"
    // (that would risk a double delivery) — surface it.
    match dispositions::recorded_delivered_digest(&tpaths, &envelope.correlation_id) {
        Ok(Some(recorded)) if recorded == presented_digest => {
            eprintln!(
                "qd send: {} already delivered — no-op",
                envelope.correlation_id
            );
            return 0;
        }
        Ok(Some(_)) => {
            // Same id, DIFFERENT body — someone else's content landed under this
            // id (buggy origin, corrupt mover, or attacker). Refuse loudly.
            return stamp_refused(
                Refusal::refused(
                    "body-mismatch",
                    format!(
                        "{} was already delivered with a different body — refusing to deliver a conflicting body under the same id",
                        envelope.correlation_id
                    ),
                ),
                "body-mismatch",
            );
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!(
                "qd send: could not read the disposition ledger for idempotency ({e}) — not admitted."
            );
            return 1;
        }
    }

    // (6) ADMIT + (resume-and-)DELIVER. `accepted` is RETIRED (R14.3) — admission
    // is marked by `attempted`. Event rows are fully normalized (R14.2): they
    // carry ONLY `{v, correlation_id, event, created_at}` (+ `class` on
    // refused/delivery-failed); the peer's `origin`/`authored_at` live on the
    // envelope in the mirror and JOIN by `correlation_id`.

    // The attempt-start stamp every inbound delivery route shares (each route
    // below is one attempt; a retry is a fresh inbound presentation). Stamped
    // AFTER the idempotency short-circuit — a replayed already-delivered envelope
    // no-ops with no fresh row.
    let stamp_attempted = || {
        stamp_event(
            &tpaths,
            &dispatch::dispositions::DispositionEvent::attempted(
                envelope.correlation_id.clone(),
                clock.now_ms(),
            ),
        );
    };

    // The target's LANE (the routing `select_carrier` used to do off a provider
    // string). `None` is a provider no lane can address — the same two-sided
    // answer the origin path makes: refused at the door when the row is LIVE,
    // and `failed{wake}` through the funnel when it is not.
    let lane = quorum_qw::lane_for(&target.provider, target.hosting.as_deref());

    // A not-live target is WOKEN inside `deliver` (`wake_if_cold`), then
    // delivered; a live target is delivered directly, with no wake. NO envelope
    // log append either way.
    if is_live(&target) {
        // R14.3: a live-but-carrierless target is a parse-valid inbound refusal —
        // stamp `refused{no-live-receive-path}` IN the funnel (NO `attempted`, the
        // message never admitted) + the refusal exit. The origin path's row-less
        // exit-1 `report_refusal` is replaced by this for the inbound door.
        //
        // The pre-flight is `LaneOps::receive_path` — topology only, and never an
        // input to `deliver` (see the origin path, and the source scan in
        // `quorum_qw::conformance`). An unknown provider takes the same door: it
        // was `select_carrier`'s `UnknownProvider`, and this arm never
        // distinguished which refusal it caught.
        let refuse_no_receive_path = || {
            let label = target.name.as_deref().unwrap_or(&envelope.target);
            stamp_refused(
                Refusal::refused(
                    "no-live-receive-path",
                    format!("\"{label}\" has no live receive path — not sendable"),
                ),
                "no-live-receive-path",
            )
        };
        let Some(lane) = lane else {
            return refuse_no_receive_path();
        };
        let paths_for_lane = paths.clone();
        let ops = dispatch::lane::open(lane, env, paths_for_lane);
        let id = SessionId(target.session_id.clone());
        match ops.receive_path(&id) {
            Ok(ReceivePath::Available) => {}
            Ok(ReceivePath::None { .. }) | Ok(ReceivePath::Undetermined { .. }) | Err(_) => {
                return refuse_no_receive_path()
            }
        }
        stamp_attempted();
        deliver_then_stamp(
            &tpaths,
            &LaneAttempt {
                ops: ops.as_ref(),
                id,
                // No wake: `is_live` said this row is live. `deliver` ATTEMPTS on
                // `wake_if_cold: false` rather than refusing off its own `health`,
                // which is what keeps a stale-live row's funnel where it is.
                policy: DeliverPolicy {
                    wake_if_cold: false,
                    render: RenderMode::default(),
                    ..DeliverPolicy::default()
                },
                message: envelope.body.clone(),
                send_id: intent::record_send_intent(
                    env,
                    &clock,
                    Some(&target.session_id),
                    target.name.as_deref(),
                    intent::VERB_SEND,
                    &envelope.body,
                ),
            },
            &envelope.body,
            &envelope.correlation_id,
            &clock,
        )
    } else {
        // Flag-less render (the `send` verb has no --alt-screen/--inline): config
        // render-default > the inline default (exactly the origin not-live path).
        let render = dispatch::launch::resolve_render_mode(
            None,
            common::render_default_from_config(env).as_deref(),
        );
        // The attempt starts. `queued` is stamped by the SHARED tail once the lane
        // reports whether a wake happened (`Receipt::woke`, or a returned
        // `WakeFailed`) — the file ORDER is unchanged (`attempted`, `queued`,
        // outcome) while `created_at` is no longer the moment before the wake was
        // tried. See `deliver_then_stamp` for why that is now unavoidable and what
        // it costs.
        stamp_attempted();
        let paths_for_lane = paths.clone();
        match lane {
            Some(lane) => {
                let ops = dispatch::lane::open(lane, env, paths_for_lane);
                deliver_then_stamp(
                    &tpaths,
                    &LaneAttempt {
                        ops: ops.as_ref(),
                        id: SessionId(target.session_id.clone()),
                        policy: DeliverPolicy {
                            wake_if_cold: true,
                            render,
                            ..DeliverPolicy::default()
                        },
                        message: envelope.body.clone(),
                        send_id: intent::record_send_intent(
                            env,
                            &clock,
                            Some(&target.session_id),
                            target.name.as_deref(),
                            intent::VERB_SEND,
                            &envelope.body,
                        ),
                    },
                    &envelope.body,
                    &envelope.correlation_id,
                    &clock,
                )
            }
            None => deliver_then_stamp(
                &tpaths,
                &Unwakeable {
                    provider: target.provider.clone(),
                },
                &envelope.body,
                &envelope.correlation_id,
                &clock,
            ),
        }
    }
}

/// Read the inbound envelope bytes: from STDIN when `arg == "-"`, else from the
/// file at `arg`. A read error propagates (the caller renders it).
fn read_envelope_bytes(arg: &str) -> std::io::Result<Vec<u8>> {
    if arg == "-" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        std::fs::read(arg)
    }
}

/// qd–qf W3 part A — the write-then-deliver + event-stamp wrapper around ONE
/// [`Attempt`].
///
/// **This is now the WHOLE durability boundary.** It used to have a twin,
/// `wake_then_deliver`, which existed only because qd performed the wake ITSELF:
/// it had to log, stamp, wake, re-select a carrier for the refreshed row and only
/// then deliver. `LaneOps::deliver` is ATOMIC — the wake happens inside the call
/// when `policy.wake_if_cold` is set, and the lane re-reads the row afterwards —
/// so there is nothing left for a second function to sequence. The live and cold
/// paths now differ in exactly ONE value, the policy's `wake_if_cold`, and the
/// receipt reports back whether a wake happened so the ledger can still stamp
/// `queued`.
///
/// Ordering (format doc §1/§2): LOG the envelope, stamp `attempted`, THEN run the
/// attempt, THEN stamp the outcome. The envelope append is fatal-on-error (no
/// durable record ⇒ do not deliver) — and it is what makes a `failed{wake}` have
/// an envelope to join on. The event appends are best-effort (a lost event row
/// never changes the exit). A synchronous local attempt that completes is
/// `delivered` (exit 0) or `delivery-failed{delivery}` (nonzero);
/// `pending`/`expired` are DERIVED (absence) and never stamped here.
///
/// Kept as a seamed helper (deps injected) so the log-append / event-stamp shape
/// is exercised without standing up a live carrier OR a live revive: the
/// `attempt` is any [`Attempt`], `env`/`paths` are the resolved seams.
fn deliver_with_durability(
    env: &dyn Env,
    paths: &dispatch::paths::QdPaths,
    attempt: &dyn Attempt,
    raw_target: &str,
    message: &str,
    expires_ms: i64,
    supplied_correlation_id: Option<String>,
) -> i32 {
    use dispatch::dispositions;
    use dispatch::effects::{Clock, RealClock};
    use dispatch::origin_send::{build_envelope, mint_correlation_id};

    // The transport files honor QD_HOME (from_home_env), matching the store's own
    // resolution + the W5 reader — NOT the plain from_home `paths` (which is the
    // `.claude`-layout registry root). Both derive from the same resolved home.
    let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, env);

    let clock = RealClock;
    let authored_at = clock.now_ms();
    // qd–qf W3c (provider-contract §4): frame's ledger event id rides as the
    // correlation_id when it originates; qd mints its own ULID only for BARE sends.
    // The SAME id flows into BOTH the log envelope (below) and the stamped
    // disposition events (via deliver_then_stamp) — they must key on one id. The
    // empty-id sync refusal was already applied at the verb entry.
    let correlation_id = supplied_correlation_id.unwrap_or_else(|| mint_correlation_id(&clock));
    // This qd ORIGINATES here, so local_host is the envelope's `origin` (the single
    // normalized HOME of origin, R14.2). Event rows no longer carry origin/witness.
    let origin = dispositions::local_host(env);

    // R15 CLAIM LOCK — held across check→(log)→attempt→stamp (the `_claim` guard
    // lives to the end of this fn). Serializes concurrent same-id origin submits
    // and the body-consistency check. Fail CLOSED on a lock error.
    let _claim = match dispositions::acquire_claim(&tpaths, &correlation_id) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("qd send: could not acquire the delivery claim for {correlation_id} ({e}) — not sent.");
            return 1;
        }
    };

    // R15 ORIGIN no-double-append: if this id is ALREADY in my own log, the caller
    // is re-submitting the SAME authored act. Compare bodies (hex sha-256 of the
    // parsed body):
    //   - DIFFERENT body ⇒ sync `refused{body-mismatch}`, ROW-LESS (R14a pin 3 —
    //     origin-mode sync refusals stamp no funnel row); the id binds one body.
    //   - SAME body + a `delivered` event exists ⇒ no-op success (idempotent).
    //   - SAME body + NOT delivered ⇒ a legit caller retry: NO fresh envelope
    //     append (do not double-append the log), a fresh `attempted`, then
    //     re-run the attempt into the outcome tail.
    // Absent from the log ⇒ the normal write-then-deliver path (append below).
    let presented_digest = dispatch::origin_send::body_digest(message);
    let mut skip_append = false;
    if let Some(prior) = dispositions::logged_envelope(&tpaths, &correlation_id) {
        if dispatch::origin_send::body_digest(&prior.body) != presented_digest {
            // Sync refusal, ROW-LESS (origin mode, R14a pin 3): the id already
            // binds a different body in my own log.
            return dispatch::origin_send::Refusal::refused(
                "body-mismatch",
                format!(
                    "{correlation_id} is already in the log with a different body — refusing to re-submit a conflicting body under the same id"
                ),
            )
            .emit();
        }
        // Same body already logged: a delivered event ⇒ no-op; else caller retry.
        match dispositions::recorded_delivered_digest(&tpaths, &correlation_id) {
            Ok(Some(_)) => {
                eprintln!("qd send: {correlation_id} already delivered — no-op");
                return 0;
            }
            // Legit caller retry: do NOT append the envelope again; stamp a fresh
            // `attempted` and re-attempt through the shared outcome tail.
            Ok(None) => skip_append = true,
            Err(e) => {
                eprintln!("qd send: could not read the disposition ledger for {correlation_id} ({e}) — not sent.");
                return 1;
            }
        }
    }

    // Mint + LOG FIRST (write-then-deliver). `target` is the RAW address the
    // caller gave (operational record); `body` is the message verbatim. Even a
    // wake that later fails leaves the durable envelope, so a
    // `delivery-failed{wake}` has an envelope to join on. A caller-retry
    // (`skip_append`) reuses the envelope already in the log — never a
    // double-append.
    if !skip_append {
        let envelope = build_envelope(
            correlation_id.clone(),
            authored_at,
            expires_ms,
            raw_target.to_string(),
            origin.clone(),
            // The invoking agent session, RAW from QD_SESSION_ID. Read through
            // the SAME `env` seam the self-send fence uses, so a test's MapEnv
            // drives attribution too — and read HERE rather than threaded in,
            // because a caller-retry (`skip_append`) must NOT rewrite the
            // sender: the envelope already in the log holds the session that
            // actually authored the act.
            dispatch::origin_send::caller_session_id(env),
            message.to_string(),
        );
        if let Err(e) = dispositions::append_envelope(&tpaths, &envelope) {
            // HARD FAIL: no durable envelope ⇒ we must not proceed to deliver.
            // Nothing was sent; the caller gets a clear error + a nonzero exit
            // (generic class).
            eprintln!(
                "qd send: could not durably record the message before delivery ({e}) — not sent."
            );
            return 1;
        }
    }

    // The delivery attempt STARTS here (the envelope is durable): stamp
    // `attempted` — each retry invocation is a fresh attempted event (R8b).
    // Normalized (R14.2): the row carries only `{v, correlation_id, event,
    // created_at}`; `created_at = now` (this host recorded it).
    stamp_event(
        &tpaths,
        &dispatch::dispositions::DispositionEvent::attempted(
            correlation_id.clone(),
            clock.now_ms(),
        ),
    );

    // Run the attempt + stamp the outcome. The tail is SHARED with W4 inbound, so
    // the two cannot drift.
    deliver_then_stamp(&tpaths, attempt, message, &correlation_id, &clock)
}

/// Best-effort disposition-EVENT append (R8): a lost event row must NEVER
/// change a send's exit code, so an append error is a WARNING eprintln only —
/// the same posture the old disposition append had (events.rs telemetry
/// discipline). Every stamp point routes through this one helper.
fn stamp_event(
    tpaths: &dispatch::paths::QdPaths,
    event: &dispatch::dispositions::DispositionEvent,
) {
    if let Err(e) = dispatch::dispositions::append_event(tpaths, event) {
        eprintln!("WARNING: could not record a disposition event (non-fatal): {e}");
    }
}

/// qd–qf W3/W4 — the SHARED attempt → stamp-OUTCOME tail (NO log append, and NO
/// `attempted` emission — CALLERS own the attempt-start event). The envelope is
/// ALREADY durable (origin logged it; inbound never logs its own). One
/// [`Attempt`], then the best-effort
/// [`dispatch::dispositions::DispositionEvent`]s the receipt calls for.
///
/// # What the receipt is read for, in order
///
/// **`queued`, from `Receipt::woke`.** The LIVE path stamps no `queued`, so the
/// row's PRESENCE is how the ledger records that a wake happened at all. `deliver`
/// being ATOMIC means qd cannot know whether one did until the lane TELLS it, so
/// the rule is exactly the one `Receipt::woke` documents: stamp when the lane says
/// a wake HAPPENED — `Confirmation::Yes` or `Unknown`, OR a returned
/// [`LaneError::WakeFailed`], which is only ever produced by a wake that was
/// attempted. Both pinned funnels survive: `attempted, queued, delivered` and
/// `attempted, queued, delivery-failed{wake}`.
///
/// It is stamped RETROSPECTIVELY, and the cost is stated rather than discovered:
/// `created_at` is no longer the moment the message was placed durably awaiting
/// the wake — it is the moment qd LEARNED a wake had been attempted, a whole
/// revive later (seconds, not ms). R14.1 still holds as written (`created_at` is
/// when THIS host RECORDED the event; there is no retro-dating). What no longer
/// holds is the row schema's parenthetical that for the qd-driven events "record
/// time and happen time coincide to within ms" — said out loud in
/// `doc/formats/dispatch-transport-formats.md` and on `DispositionEvent::queued`.
///
/// **The outcome.**
///   - accepted            ⇒ `delivered` (exit 0),
///   - not accepted        ⇒ `delivery-failed{delivery}`,
///   - [`LaneError::WakeFailed`] ⇒ `delivery-failed{wake}` + the shared refusal
///     exit 12, printed as `failed{wake}` — a wake that could not succeed is NOT
///     a verdict on the id, and a later retry re-attempts (idempotence keys on
///     `delivered` EXISTING, never on a failure),
///   - any other [`LaneError`] ⇒ `delivery-failed{delivery}`, SILENTLY. The
///     carrier already printed its own loud line before answering — that is the
///     entire content of `CarrierOutcome::unkeyed`, which is what produces
///     `LaneError::Transport` here — and a second qd-authored line on top of it
///     would be new output for a case that has always been the carrier's to
///     narrate.
///
/// **The exit code narrows to 1 for a failed delivery, and that is a decision.**
/// `Receipt` carries `accepted`, not a code, on purpose: the carrier's exit code
/// is a private number and the ledger keys on the qd-minted `correlation_id`.
/// Every reachable carrier failure answers `1` today (`no_relay_exit`, every
/// `CarrierOutcome::unkeyed(1)` door, `run_send_pty_resolved`'s strict returns),
/// so nothing observable moves; what changes is that a NEW carrier cannot
/// introduce a third exit code through this path without saying so.
///
/// R14.2: event rows are FULLY NORMALIZED — the outcome row carries ONLY
/// `{v, correlation_id, event, created_at}` (+ `class` on the failed variant,
/// `body_digest` on delivered — R15). `created_at` = when THIS host recorded the
/// outcome (observation time, R14.1); there is NO `witness`/`origin`/`authored_at`
/// on the row (they live on the envelope and join by `correlation_id`).
///
/// R15: on success the `delivered` row binds `body_digest(message)` — the hex
/// sha-256 of the body that ACTUALLY landed (the exact `message` string the
/// attempt carried). This is the integrity binding the door reads back to refuse a
/// later same-id/different-body presentation. Used by the origin path AND W4
/// inbound, so the two cannot drift.
fn deliver_then_stamp(
    tpaths: &dispatch::paths::QdPaths,
    attempt: &dyn Attempt,
    message: &str,
    correlation_id: &str,
    clock: &dyn dispatch::effects::Clock,
) -> i32 {
    use dispatch::dispositions::DispositionEvent;

    let outcome = attempt.run();

    // `queued` first — see the doc above for the rule and what it costs.
    let a_wake_happened = match &outcome {
        Ok(receipt) => receipt.woke != Confirmation::No,
        Err(LaneError::WakeFailed { .. }) => true,
        Err(_) => false,
    };
    if a_wake_happened {
        stamp_event(
            tpaths,
            &DispositionEvent::queued(correlation_id.to_string(), clock.now_ms()),
        );
    }

    match outcome {
        Ok(receipt) if receipt.accepted => {
            stamp_event(
                tpaths,
                &DispositionEvent::delivered(
                    correlation_id.to_string(),
                    clock.now_ms(),
                    dispatch::origin_send::body_digest(message),
                ),
            );
            0
        }
        Ok(_) => {
            stamp_event(
                tpaths,
                &DispositionEvent::delivery_failed(
                    correlation_id.to_string(),
                    clock.now_ms(),
                    "delivery".to_string(),
                ),
            );
            1
        }
        Err(LaneError::WakeFailed { detail, .. }) => {
            stamp_event(
                tpaths,
                &DispositionEvent::delivery_failed(
                    correlation_id.to_string(),
                    clock.now_ms(),
                    "wake".to_string(),
                ),
            );
            // The CORE's own message, carried out unchanged under qd's class. The
            // lane deliberately does not stamp a verb on it (a lane has no verb —
            // see `LaneError::WakeFailed`), and `Refusal::failed` is where
            // `qd send: failed{wake}:` and the exit 12 both come from.
            Refusal::failed("wake", detail).emit()
        }
        Err(_) => {
            stamp_event(
                tpaths,
                &DispositionEvent::delivery_failed(
                    correlation_id.to_string(),
                    clock.now_ms(),
                    "delivery".to_string(),
                ),
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use dispatch::model::SessionBranch;

    use super::*;

    fn session(provider: &str) -> Session {
        Session {
            name: Some("target".into()),
            user_named: Some(true),
            session_id: "session-uuid".into(),
            code: None,
            qd_id: Some("ab3kx9mq".into()),
            pid: Some(42),
            status: SessionStatus::Idle,
            zmx_name: Some("target-pane".into()),
            zmx_clients: Some(0),
            socket_dir: Some("/mux".into()),
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: Some("/work".into()),
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: provider.into(),
            entrypoint: None,
            lineage: None,
            hosting: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    /// Reduce a normalized [`dispatch::dispositions::DispositionEvent`] to
    /// `(kind, class?)` — the class is `Some` only on the delivery-failed / refused
    /// variants (the discriminated-union tail), `None` on the three plain variants.
    /// The R14 test analogue of the old `(e.event, e.reason.as_deref())` tuple.
    fn event_row(
        e: &dispatch::dispositions::DispositionEvent,
    ) -> (dispatch::dispositions::EventKind, Option<String>) {
        use dispatch::dispositions::DispositionEvent as E;
        let class = match e {
            E::DeliveryFailed { class, .. } | E::Refused { class, .. } => Some(class.clone()),
            _ => None,
        };
        (e.kind(), class)
    }

    // === qd–qf W6 — ADDRESSING: parse_address + resolve_target host dispatch ===
    //
    // `name@host` is sugar over `--host`; `parse_address` splits on the LAST `@`.
    // `resolve_target`'s host branch is unit-checkable up to (not through) the
    // real-store gather: the empty-host, empty-name, and foreign-host arms all
    // short-circuit to a Refusal BEFORE `all_sessions()`, so no live registry is
    // needed. The LOCAL-resolution arms (bare / @local → One/None/Many) read the
    // real environment via `all_sessions`, so they are proven by the built-binary
    // integration tests (verbs_a4 / inbound_mode), not here.

    #[test]
    fn parse_address_splits_on_the_last_at() {
        // Bare handle (name | stable_id — neither contains '@') ⇒ no host.
        assert_eq!(parse_address("alpha"), ("alpha", None));
        assert_eq!(parse_address("ab3kx9mq"), ("ab3kx9mq", None));
        // name@host ⇒ (name, Some(host)).
        assert_eq!(parse_address("alpha@devbox"), ("alpha", Some("devbox")));
        // Split on the LAST '@' (defensive — real names/ids carry no '@', but the
        // rule is well-defined if one somehow appears).
        assert_eq!(parse_address("a@b@devbox"), ("a@b", Some("devbox")));
        // Degenerate forms are PARSED here (the refusal is resolve_target's job):
        assert_eq!(parse_address("@host"), ("", Some("host")), "empty name half");
        assert_eq!(parse_address("name@"), ("name", Some("")), "empty host half");
        assert_eq!(parse_address("@"), ("", Some("")), "both halves empty");
        assert_eq!(parse_address(""), ("", None), "empty input, no '@'");
    }

    /// The env whose local host id is `host` (QD_HOST override; empty/absent ⇒
    /// "local" per `dispositions::local_host`).
    fn env_host(host: &str) -> dispatch::effects::MapEnv {
        let mut e = dispatch::effects::MapEnv::default();
        e.vars.insert("QD_HOST".into(), host.into());
        e
    }

    #[test]
    fn resolve_target_empty_host_is_refused_host() {
        // "name@" ⇒ empty host qualifier ⇒ a sync refused{host} (never silently
        // treated as local). Short-circuits before any gather.
        let env = env_host("devbox");
        let r = resolve_target("name", Some(""), &env).unwrap_err();
        assert_eq!(r.family, dispatch::origin_send::Family::Refused);
        assert_eq!(r.class, "host");
    }

    #[test]
    fn resolve_target_empty_name_is_refused_address() {
        // "@host" ⇒ empty name ⇒ refused{address} (nothing to resolve). Here the
        // host equals local so we pass the host gate and hit the empty-name gate.
        let env = env_host("devbox");
        let r = resolve_target("", Some("devbox"), &env).unwrap_err();
        assert_eq!(r.family, dispatch::origin_send::Family::Refused);
        assert_eq!(r.class, "address");
    }

    #[test]
    fn resolve_target_foreign_host_is_refused_no_fleet_state() {
        // A host-qualified address for a host that is NOT this host, on a
        // single-machine box (no remote/<h>/) ⇒ fail-closed refused{no-fleet-state}.
        // local_host = "devbox" (QD_HOST), target host "elsewhere" ≠ local.
        let env = env_host("devbox");
        let r = resolve_target("alpha", Some("elsewhere"), &env).unwrap_err();
        assert_eq!(r.family, dispatch::origin_send::Family::Refused);
        assert_eq!(r.class, "no-fleet-state");
        assert!(
            r.reason.contains("elsewhere") && r.reason.contains("no fleet state"),
            "the refusal names the host + the absent-fleet-state reason, got: {}",
            r.reason
        );
    }

    #[test]
    fn mirror_resolve_one_none_many_and_prefix() {
        // The pure resolution semantics origin-remote send uses over a peer mirror's
        // opaque `qd ls --json` rows: exact name/id, unique prefix, unknown, ambiguous.
        use serde_json::json;
        let rows = vec![
            json!({"name":"cut-els","userNamed":true,"sessionId":"uuid-1","qdId":"ab12cdef","status":"idle"}),
            json!({"name":"other","sessionId":"uuid-2","qdId":"ff99aaaa","status":"idle"}),
        ];
        assert!(matches!(resolve_name_in_mirror(&rows, "cut-els"), MirrorResolve::One));
        assert!(matches!(resolve_name_in_mirror(&rows, "uuid-2"), MirrorResolve::One)); // by sessionId
        assert!(matches!(resolve_name_in_mirror(&rows, "ff99aaaa"), MirrorResolve::One)); // by qdId
        assert!(matches!(resolve_name_in_mirror(&rows, "ab12"), MirrorResolve::One)); // unique qdId prefix
        assert!(matches!(resolve_name_in_mirror(&rows, "ghost"), MirrorResolve::None));
        // ambiguous: two rows share a name.
        let dup = vec![json!({"name":"dup","sessionId":"a"}), json!({"name":"dup","sessionId":"b"})];
        assert!(matches!(resolve_name_in_mirror(&dup, "dup"), MirrorResolve::Many));
        // ambiguous prefix across two distinct names.
        let pfx = vec![json!({"name":"alpha","qdId":"x1"}), json!({"name":"alphb","qdId":"x2"})];
        assert!(matches!(resolve_name_in_mirror(&pfx, "alph"), MirrorResolve::Many));
        // an EXACT match wins even when it also prefixes another row ("al" vs "alpha").
        let exwin = vec![json!({"name":"al","qdId":"y1"}), json!({"name":"alpha","qdId":"y2"})];
        assert!(matches!(resolve_name_in_mirror(&exwin, "al"), MirrorResolve::One));
    }

    #[test]
    fn resolve_target_default_local_host_is_local() {
        // With QD_HOST unset, local_host == "local", so `@local` is treated as
        // this host — it must NOT hit the no-fleet-state refusal (it falls through
        // to local resolution). We can't drive the real gather here, so assert the
        // COMPLEMENT: `@local` does not produce a host-class refusal. A DIFFERENT
        // host on the same default env DOES refuse (control).
        let env = dispatch::effects::MapEnv::default(); // QD_HOST unset ⇒ "local"
        // Foreign host still refuses (proves the gate is active under the default).
        let foreign = resolve_target("alpha", Some("devbox"), &env).unwrap_err();
        assert_eq!(foreign.class, "no-fleet-state");
        // "@local" for an EMPTY name passes the host gate (local match) and hits the
        // empty-name gate instead of no-fleet-state — proof the local branch is
        // taken, without needing the live store.
        let local_empty = resolve_target("", Some("local"), &env).unwrap_err();
        assert_eq!(
            local_empty.class, "address",
            "@local is local: it passes the host gate (would go to the store), not no-fleet-state"
        );
    }

    // === THE ROUTING MOVED, and this is what qd still owes ===============
    //
    // Fourteen tests lived here — the `select_carrier` table, exhaustively: the
    // two codex topologies, the two pi topologies, relay-precedes-PTY, the bare
    // and unknown-provider refusals, the non-live floor. They are GONE with the
    // function, and not because coverage was traded away: the table they pinned
    // is `quorum_qw::lanes::LaneImpl::deliver`'s seven-arm `match (harness,
    // mode)`, and it is pinned THERE — `lanes::tests::deliver_is_total_for_every_lane`,
    // `relay_precedence_is_structural_and_pty_needs_an_observed_absence`, and
    // `lane::tests::start_routing_is_total_over_every_real_input`, which walks
    // every real `(provider, hosting)` input. Re-asserting it here would be a
    // second copy of the thing this change deleted.
    //
    // What is left on THIS side is the one step qd still performs: turning a
    // registry row into a LANE. That is `quorum_qw::lane_for(provider, hosting)`,
    // and the test below pins the property the fourteen were really defending.

    /// **A row's carrier follows its HOSTING, never its provider id alone.**
    ///
    /// This is the whole content of the deleted codex/pi topology tests, asked of
    /// the one function qd still calls. The two codex topologies have DISJOINT
    /// receive paths — the daemon has a ws endpoint and no pane, the
    /// `--interactive` lane has a pane and no endpoint — so a router keyed on
    /// `"codex"` alone necessarily gets one of them wrong, and `select_carrier`
    /// was one guard away from exactly that. pi is the same story one provider
    /// over, and it is the shape BUG 2 was: a pane row driven through the
    /// resident revive.
    ///
    /// MUTATION EVIDENCE: make `lane_for` ignore the hosting token and the two
    /// `Mode::Pane` rows come back `Mode::Daemon`.
    #[test]
    fn a_rows_lane_follows_its_hosting_not_its_provider_string() {
        use quorum_qw::lane::{Harness, Mode};

        let lane_of = |provider: &str, hosting: Option<&str>| {
            quorum_qw::lane_for(provider, hosting).map(|l| (l.harness, l.mode))
        };

        // The two topologies that split, in both directions.
        assert_eq!(
            lane_of("codex", Some("mux-pane")),
            Some((Harness::Codex, Mode::Pane)),
            "an --interactive codex row is a PANE lane: its receive path is the \
             pane's PTY, and it has no ws endpoint to reconnect to"
        );
        assert_eq!(lane_of("codex", Some("daemon")), Some((Harness::Codex, Mode::Daemon)));
        assert_eq!(lane_of("codex", None), Some((Harness::Codex, Mode::Daemon)));
        assert_eq!(lane_of("pi", Some("mux-pane")), Some((Harness::Pi, Mode::Pane)));
        assert_eq!(lane_of("pi", Some("daemon")), Some((Harness::Pi, Mode::Daemon)));
        assert_eq!(lane_of("pi", None), Some((Harness::Pi, Mode::Daemon)));

        // claude-code has no daemon lane, so a `daemon` token cannot invent one.
        assert_eq!(
            lane_of("claude-code", Some("daemon")),
            Some((Harness::ClaudeCode, Mode::Pane)),
            "an unsupportable hosting token falls back to the harness's structural \
             mode rather than fabricating a lane"
        );

        // Both ACP lanes, reached through the legacy spellings that still name
        // them and through opencode's single current one.
        assert_eq!(lane_of("acp/claude-code", None), Some((Harness::ClaudeCode, Mode::Acp)));
        assert_eq!(lane_of("acp/opencode", None), Some((Harness::Opencode, Mode::Acp)));
        assert_eq!(lane_of("opencode", None), Some((Harness::Opencode, Mode::Acp)));
        assert_eq!(lane_of("claude-code", Some("acp")), Some((Harness::ClaudeCode, Mode::Acp)));

        // AND THE DRIFT THIS CHANGE ACCEPTS, named rather than discovered.
        // `select_carrier` matched `acp/*` by PREFIX, so a hypothetical
        // `acp/future` routed to the ACP daemon carrier. `lane_for` knows the two
        // ACP harnesses that exist and refuses the rest — the same answer it
        // already gives `qd attach`, `qd resume` and `qd kill`, all of which
        // moved onto it first. A future bridge is added to `Harness`, which is
        // where a new lane belongs, rather than being routed by a string prefix
        // into a carrier nobody wired.
        assert_eq!(lane_of("acp/future", None), None);
        assert_eq!(lane_of("mystery", None), None);
    }

    // --- refusal reporting: absence vs. undetermined ---------------------

    fn eperm() -> std::io::Error {
        std::io::Error::from_raw_os_error(libc::EPERM)
    }

    fn denied(source: &'static str) -> dispatch::discovery::AcquireFailure {
        dispatch::discovery::AcquireFailure::new(source, &eperm())
    }

    // `discovery_health_never_participates_in_selection` lived here. It pinned
    // that a degraded gather could not change `select_carrier`'s verdict, which
    // was a property of a function in THIS file that happened not to read
    // `DiscoveryHealth`. It is retired rather than rewritten because the property
    // is now STRUCTURAL: routing is `LaneOps::deliver`'s, in `quorum-qw`, and
    // `DiscoveryHealth` is a `quorum-dispatch` type the lane cannot name — qw
    // must never depend on qd. There is nothing left for a test to defend.
    //
    // The REPORTING half of the same rule is very much still qd's, and every test
    // below pins it.

    /// A clean gather still ASSERTS the absence, and keeps the pre-existing
    /// transport-shape exit `1` that `verbs_a4` pins.
    #[test]
    fn clean_gather_reports_a_confirmed_absent_receive_path() {
        let mut claude = session("claude-code");
        claude.relay_port = None;
        claude.zmx_name = None;
        let code = report_refusal(
            "target",
            &claude,
            SendRefusal::NoLiveReceivePath,
            &DiscoveryHealth::default(),
        );
        assert_eq!(code, 1, "a confirmed absence keeps the generic transport exit");
    }

    /// THE regression this change exists for: a refused `ps` nulls `relay_port`
    /// on every claude row, and the refusal must NOT report that as a confirmed
    /// absence. It becomes its own refusal CLASS on the shared contract door
    /// code, so a caller can separate "retrying will not help" (exit 1) from
    /// "retry with the access that read needed" (EXIT_REFUSED) on `$?` alone.
    #[test]
    fn refused_process_table_downgrades_absence_to_undetermined() {
        let mut claude = session("claude-code");
        claude.relay_port = None;
        claude.zmx_name = None;
        let health = DiscoveryHealth {
            process_table: Some(denied("ps")),
            ..Default::default()
        };
        let code = report_refusal("target", &claude, SendRefusal::NoLiveReceivePath, &health);
        assert_eq!(code, dispatch::origin_send::EXIT_REFUSED);
        assert_ne!(code, 1, "undetermined must not look like a confirmed absence");
    }

    /// A refused mux list undetermines the PTY carrier the same way.
    #[test]
    fn refused_mux_list_also_downgrades_absence_to_undetermined() {
        let mut claude = session("claude-code");
        claude.relay_port = None;
        claude.zmx_name = None;
        let health = DiscoveryHealth {
            mux_list: Some(denied("mux list")),
            ..Default::default()
        };
        assert_eq!(
            report_refusal("target", &claude, SendRefusal::NoLiveReceivePath, &health),
            dispatch::origin_send::EXIT_REFUSED
        );
    }

    /// A degraded census alone leaves the receive-path facts intact, so the
    /// absence is still a real observation and keeps the confirmed exit.
    #[test]
    fn unrelated_degradation_does_not_downgrade_a_real_absence() {
        let mut claude = session("claude-code");
        claude.relay_port = None;
        claude.zmx_name = None;
        let health = DiscoveryHealth {
            claude_procs: Some(denied("ps")),
            ..Default::default()
        };
        assert!(health.is_degraded());
        assert_eq!(
            report_refusal("target", &claude, SendRefusal::NoLiveReceivePath, &health),
            1
        );
    }

    /// The undetermined class rides the shared `{class,reason}` refusal family,
    /// so it renders in the same machine-readable shape as every other door
    /// refusal rather than inventing a second vocabulary.
    #[test]
    fn undetermined_uses_the_shared_refusal_contract_shape() {
        let line = dispatch::origin_send::Refusal::refused("receive-path-undetermined", "why")
            .stderr_line();
        assert!(
            line.starts_with("qd send: refused{receive-path-undetermined}: "),
            "{line}"
        );
    }

    /// Health must never manufacture or mask a refusal for a target that is NOT
    /// carrierless — degradation explains an existing refusal, it never creates
    /// one. (Selection itself cannot see health at all; this pins the reporting
    /// half of that same rule.)
    #[test]
    fn degradation_only_speaks_for_the_receive_path_refusal() {
        let s = session("claude-code");
        let degraded = DiscoveryHealth {
            process_table: Some(denied("ps")),
            ..Default::default()
        };
        // A bare row and an unknown provider are registry-derived facts: a
        // denied process read neither causes nor explains them, so they keep
        // their ordinary exit even under a degraded gather.
        assert_eq!(report_refusal("t", &s, SendRefusal::Bare, &degraded), 1);
        assert_eq!(
            report_refusal("t", &s, SendRefusal::UnknownProvider("nope".into()), &degraded),
            1
        );
    }

    /// The two-sided answer for a provider NO LANE CAN ADDRESS, which is the one
    /// place `select_carrier`'s `UnknownProvider` had a user-visible consequence
    /// that survives its deletion.
    ///
    /// It is deliberately NOT symmetric, and both halves are pinned end-to-end
    /// elsewhere: a LIVE unknown-provider row is a sync exit-1 refusal with NO
    /// envelope and NO disposition, while a NOT-LIVE one runs the whole
    /// write-then-deliver funnel to `attempted, queued, delivery-failed{wake}`
    /// and exit 12 (`tests/acceptance.rs`'s §6 scenario and
    /// `tests/inbound_mode.rs`'s "mystery" rows both drive the second). This pins
    /// the two pieces qd owns: the refusal exit for the live half, and the
    /// [`Unwakeable`] attempt that carries the cold half.
    ///
    /// MUTATION EVIDENCE: give `Unwakeable` any other `LaneError` and the funnel
    /// below becomes `delivery-failed{delivery}` at exit 1 — the failed leg
    /// `acceptance.rs` reads would change class.
    #[test]
    fn an_unaddressable_provider_refuses_live_and_fails_the_wake_when_cold() {
        // LIVE: a sync refusal, exit 1, and the message names the provider.
        let live = session("mystery");
        assert!(is_live(&live));
        assert_eq!(
            report_refusal(
                "t",
                &live,
                SendRefusal::UnknownProvider("mystery".into()),
                &DiscoveryHealth::default()
            ),
            1
        );

        // NOT LIVE: the funnel's attempt is `Unwakeable`, whose answer is the
        // `failed{wake}` class + `RealWaker`'s fallthrough message, verbatim.
        let mut cold = session("mystery");
        cold.status = SessionStatus::Cold;
        assert!(!is_live(&cold));
        match (Unwakeable {
            provider: "mystery".into(),
        })
        .run()
        {
            Err(LaneError::WakeFailed { detail, exit_code, .. }) => {
                assert_eq!(detail, "provider \"mystery\" cannot be woken headlessly");
                assert_eq!(exit_code, 12);
            }
            other => panic!("an unaddressable provider must answer failed{{wake}}, got {other:?}"),
        }
    }

    /// Cold/Killed are NOT refusals — "stopped is not a refusal class" (qd–qf
    /// W3b). They are the WAKE TRIGGER, and [`is_live`] is the one place `qd send`
    /// decides it. `select_carrier` used to carry a defense-in-depth non-live
    /// floor; there is nothing left to floor, because the not-live branch now
    /// hands the row to `deliver` with `wake_if_cold: true` and the lane owns the
    /// rest.
    #[test]
    fn a_stopped_target_is_a_wake_trigger_not_a_refusal() {
        let mut target = session("claude-code");
        target.status = SessionStatus::Cold;
        assert!(!is_live(&target));
        target.status = SessionStatus::Killed;
        assert!(!is_live(&target));

        // …and the three that ARE live, including a pane sitting at a shell.
        for status in [SessionStatus::Idle, SessionStatus::Busy, SessionStatus::Shell] {
            target.status = status;
            assert!(is_live(&target), "{status:?} is deliverable as it stands");
        }
    }

    #[test]
    fn qd_session_id_resolves_to_uuid_and_fences_only_self() {
        let ids = dispatch::idstore::fold_str(concat!(
            r#"{"v":1,"ts":"t","event":"mint","id":"ab3kx9mq","session_id":"session-uuid","name":"target"}"#,
            "\n",
        ));
        assert!(is_self_send(Some("AB3KX9MQ"), &ids, "session-uuid"));
        assert!(!is_self_send(Some("ab3kx9mq"), &ids, "other-uuid"));
        assert!(!is_self_send(Some("zzzzzzzz"), &ids, "session-uuid"));
        assert!(!is_self_send(Some(""), &ids, "session-uuid"));
        assert!(!is_self_send(None, &ids, "session-uuid"));
    }

    /// Read the TARGET's delivery-log records out of the jail. `emit_door_failure`
    /// — the codex carrier's refusal record — writes them, so this is how the
    /// carrier is observed now that it is a real function rather than a probe.
    fn door_records(
        paths: &dispatch::paths::QdPaths,
        env: &dyn Env,
    ) -> Vec<serde_json::Value> {
        let state_dir = dispatch::paths::QdPaths::from_home_env(&paths.home, env).state_dir;
        let raw = std::fs::read_to_string(dispatch::events::events_path(&state_dir, JAILED_SID))
            .unwrap_or_default();
        raw.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .collect()
    }

    /// **Exactly ONE carrier call, and no fallback after a failure** — through
    /// the REAL lane AND the REAL carrier, which is where the decision now lives.
    ///
    /// This used to watch a `ProbeBackend` at the `Carriers` seam. Phase 3B moved
    /// the four daemon carriers into `quorum_qw::delivery`, so the codex/daemon
    /// arm now calls a real function and there is no seam to instrument — but the
    /// carrier leaves a BETTER witness than a call counter did: its `§C1`
    /// record-then-fail-loud door writes exactly one `send-failed` into the
    /// target's delivery log per call. One record is one carrier call.
    ///
    /// MUTATION EVIDENCE: a `deliver` arm that retried a second carrier on a
    /// refusal writes a second door record and reds the count.
    #[test]
    fn deliver_reaches_exactly_one_carrier_and_never_falls_back_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, env, lane) = jailed_daemon_lane(tmp.path());
        let ops = quorum_qw::lanes::lane_ops(lane, &env, paths.clone());
        let out = LaneAttempt {
            ops: &ops,
            id: SessionId(JAILED_SID.into()),
            policy: live_policy(),
            message: "hello".into(),
            send_id: "m-test".to_string(),
        }
        .run();

        // The forged row has a recorded pid and NO endpoint, so the codex carrier
        // hits its reachability door: no turn is minted, hence no keyed receipt.
        assert!(
            matches!(out, Err(LaneError::Transport { .. })),
            "an id-less carrier refusal is a Transport error, got {out:?}"
        );

        let recs = door_records(&paths, &env);
        let kinds: Vec<&str> = recs.iter().filter_map(|r| r["event"].as_str()).collect();
        assert_eq!(
            kinds,
            vec!["send-failed"],
            "exactly ONE carrier ran, and it left exactly one door record"
        );
        assert_eq!(recs[0]["reason"], "daemon-unreachable");
    }

    /// The payload reaches the carrier byte for byte, whatever is in it.
    ///
    /// Same substitution as above: the witness is the door record's
    /// `content_sha256`, which the carrier computes over the RAW message bytes it
    /// was handed. That is strictly stronger than the old call-list compare — it
    /// proves the bytes survived all the way into the carrier's own hashing, not
    /// just into a probe's argument.
    #[test]
    fn payload_is_forwarded_byte_for_byte_without_affecting_carrier() {
        for message in [
            "",
            "--option-like",
            "multiline\nsecond line",
            "multibyte: 🧭 café",
            "$(shell) `ticks` ; & | ' \" $HOME",
            &"x".repeat(8193),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (paths, env, lane) = jailed_daemon_lane(tmp.path());
            let ops = quorum_qw::lanes::lane_ops(lane, &env, paths.clone());
            let _ = LaneAttempt {
                ops: &ops,
                id: SessionId(JAILED_SID.into()),
                policy: live_policy(),
                message: message.to_string(),
                send_id: "m-test".to_string(),
            }
            .run();

            let recs = door_records(&paths, &env);
            assert_eq!(recs.len(), 1, "one carrier call for {message:?}");
            assert_eq!(
                recs[0]["content_sha256"].as_str().unwrap(),
                dispatch::events::sha256_hex(message.as_bytes()),
                "the carrier hashed the payload it was handed, byte for byte"
            );
        }
    }

    // === qd–qf W3 part A: write-then-deliver + disposition stamping =========
    //
    // These exercise the `deliver_with_durability` seam directly with a jailed
    // QdPaths, so the log-append / event-stamp wiring is proven without standing
    // up a full live carrier. The store readers (dispatch::dispositions) parse the
    // actual files the seam wrote.
    //
    // TWO kinds of double, and the split is deliberate:
    //
    //   - a REAL `LaneImpl` over a forged registry row, driving the REAL codex
    //     carrier, for every funnel that ends in a FAILURE. It exercises the whole
    //     chain qd now owns — `Attempt` → `LaneOps::deliver` → carrier — and the
    //     carrier refuses deterministically at its reachability door.
    //   - `ProbeAttempt`, an [`Attempt`] answering a pre-set `Receipt`/`LaneError`,
    //     for every funnel that ends in a DELIVERY, and for the WAKE funnel. A real
    //     lane's `wake_if_cold: true` would run a real revive (a codex
    //     `thread/resume`, a detached claude launch), which is exactly what the
    //     retired `MockWaker` existed to avoid.
    //
    // THE SUCCESS HALF USED TO USE THE FIRST DOUBLE, through `ProbeBackend` at the
    // `Carriers` seam. Phase 3B moved the four daemon carriers into
    // `quorum_qw::delivery`, so the only route left through that seam is `mux_pty`
    // — and reaching it needs a JOINED PANE, which a jail cannot forge without
    // launching a real mux. So the ledger tests that need an ACCEPTED receipt
    // moved out one level, to `ProbeAttempt`. Nothing they assert is lost: every
    // one of them measures the disposition ledger, and which carrier produced the
    // receipt was never their subject. The two tests whose subject WAS the carrier
    // stayed on the real lane and now watch the carrier's own door record.

    use dispatch::effects::MapEnv;

    /// A MapEnv whose HOME points into `home` (QD_HOME unset ⇒ transport files
    /// land under `home/.quorum/dispatch`, exactly where the seam writes them).
    fn jail_env(home: &std::path::Path) -> MapEnv {
        let mut e = MapEnv::default();
        e.vars.insert("HOME".into(), home.to_string_lossy().into_owned());
        // The mux dirs the lane's row-join lists. SHORT literals so the `sun_path`
        // budget holds on any host, and jailed so the join cannot reach the
        // developer's real panes. Nothing is ever launched into them.
        e.vars.insert("XDG_RUNTIME_DIR".into(), "/tmp/qd-su-xdg".into());
        e.vars.insert("ZMX_DIR".into(), "/tmp/qd-su-zmx".into());
        // QD_HOST unset ⇒ local_host = "local" (the v1 envelope-origin placeholder).
        e
    }

    /// The forged row's session id. Distinctive so a stray real pane cannot join
    /// onto it.
    const JAILED_SID: &str = "qd-send-unified-jailed-sid";

    /// A jailed home holding ONE live registry row, plus the lane + env that
    /// address it.
    ///
    /// **codex/daemon on purpose.** Its `deliver` arm is the codex carrier and
    /// nothing else — no relay discovery (which would port-scan 8900..9000 on the
    /// developer's machine) and no pane gate — so these tests measure the LEDGER
    /// rather than a topology probe. Which carrier a lane reaches is pinned where
    /// it lives, in `quorum_qw::lanes`.
    ///
    /// The row carries a pid and NO endpoint, so the real carrier stops at its
    /// reachability door: it reaches the wire never, and refuses deterministically
    /// on any host.
    fn jailed_daemon_lane(
        home: &std::path::Path,
    ) -> (dispatch::paths::QdPaths, MapEnv, quorum_qw::lane::Lane) {
        let paths = dispatch::paths::QdPaths::from_home(home);
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        std::fs::write(
            paths.sessions_dir.join("424242.json"),
            format!(
                concat!(
                    r#"{{"pid":424242,"sessionId":"{sid}","name":"qd-su-jailed-row","cwd":"/w","#,
                    r#""startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","#,
                    r#""version":"0.1.0","provider":"codex","hosting":"daemon"}}"#
                ),
                sid = JAILED_SID
            ),
        )
        .unwrap();
        let lane = quorum_qw::lane_for("codex", Some("daemon")).expect("codex/daemon is a lane");
        (paths, jail_env(home), lane)
    }

    /// The LIVE path's policy, verbatim: no wake, and a render mode that is
    /// ignored because no wake happens.
    fn live_policy() -> DeliverPolicy {
        DeliverPolicy {
            wake_if_cold: false,
            render: RenderMode::default(),
            ..DeliverPolicy::default()
        }
    }

    /// An [`Attempt`] that answers a pre-set outcome and counts its calls. The
    /// successor to `MockWaker` — see the section note above for why the seam
    /// moved out one level.
    struct ProbeAttempt {
        calls: Cell<u32>,
        outcome: Result<Receipt, LaneError>,
    }

    impl ProbeAttempt {
        fn new(outcome: Result<Receipt, LaneError>) -> Self {
            ProbeAttempt {
                calls: Cell::new(0),
                outcome,
            }
        }
        /// A delivery that landed with NO wake — what the LIVE path answers.
        fn delivered() -> Self {
            ProbeAttempt::new(Ok(Receipt {
                message_id: quorum_qw::contract::MessageId("probe-mid".into()),
                accepted: true,
                terminal: quorum_qw::contract::TerminalExpectation::Pending,
                woke: Confirmation::No,
            }))
        }
        /// A delivery that WOKE the target first and then landed — what
        /// `wake_if_cold: true` answers on a successful revive.
        fn woke_and_delivered() -> Self {
            ProbeAttempt::new(Ok(Receipt {
                message_id: quorum_qw::contract::MessageId("probe-mid".into()),
                accepted: true,
                terminal: quorum_qw::contract::TerminalExpectation::Pending,
                woke: Confirmation::Yes,
            }))
        }
    }

    impl Attempt for ProbeAttempt {
        fn run(&self) -> Result<Receipt, LaneError> {
            self.calls.set(self.calls.get() + 1);
            self.outcome.clone()
        }
    }

    #[test]
    fn durability_logs_envelope_before_delivery_then_stamps_delivered() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, env, _lane) = jailed_daemon_lane(tmp.path());
        let attempt = ProbeAttempt::delivered();

        let code = deliver_with_durability(
            &env,
            &paths,
            &attempt,
            "worker@devbox", // the RAW caller address
            "hello body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None, // no caller-supplied id ⇒ qd mints a ULID
        );
        assert_eq!(code, 0, "delivered ⇒ exit 0");

        // The attempt was actually run (delivery happened).
        assert_eq!(attempt.calls.get(), 1);

        // The transport files honor QD_HOME resolution; read them back.
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1, "exactly one envelope logged");
        let env_row = &log.records[0];
        assert_eq!(env_row.target, "worker@devbox", "raw address recorded");
        assert_eq!(env_row.body, "hello body", "body verbatim");
        assert_eq!(env_row.origin, "local", "v1 origin-host placeholder");
        assert_eq!(
            env_row.expires_at,
            env_row.authored_at + dispatch::origin_send::DEFAULT_EXPIRES_MS
        );

        // The funnel for a live-origin success: attempted, delivered — and NO
        // `queued`, because the receipt said no wake happened. Rows are fully
        // normalized (R14.2) — no witness/origin/authored_at fields; every row
        // joins the envelope by correlation_id and carries only created_at.
        let events = dispatch::dispositions::read_local_events(&tpaths);
        let kinds: Vec<dispatch::dispositions::EventKind> =
            events.records.iter().map(|e| e.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                dispatch::dispositions::EventKind::Attempted,
                dispatch::dispositions::EventKind::Delivered
            ],
            "live origin path stamps attempted then delivered, with no queued"
        );
        for d in &events.records {
            assert_eq!(
                d.correlation_id(),
                env_row.correlation_id,
                "every event joins the envelope on correlation_id"
            );
            // The plain variants carry NO class tail (only delivery-failed/refused
            // do — enforced by the type; assert the variant shape here).
            assert!(
                matches!(
                    d,
                    dispatch::dispositions::DispositionEvent::Attempted { .. }
                        | dispatch::dispositions::DispositionEvent::Delivered { .. }
                ),
                "attempted/delivered carry no class tail"
            );
            assert!(
                d.created_at() >= env_row.authored_at,
                "created_at recorded at/after the envelope was authored"
            );
        }
    }

    #[test]
    fn durability_stamps_failed_delivery_when_carrier_returns_nonzero() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, env, lane) = jailed_daemon_lane(tmp.path());
        // The REAL codex carrier over a row with no endpoint: a definitive
        // delivery failure at its reachability door.
        let ops = quorum_qw::lanes::lane_ops(lane, &env, paths.clone());

        let code = deliver_with_durability(
            &env,
            &paths,
            &LaneAttempt {
                ops: &ops,
                id: SessionId(JAILED_SID.into()),
                policy: live_policy(),
                message: "body".into(),
                send_id: "m-test".to_string(),
            },
            "worker",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None,
        );
        assert_eq!(code, 1, "a carrier failure is a nonzero exit");

        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        // Envelope still logged (write-then-deliver logs BEFORE the attempt).
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1);
        // The funnel for a live-origin definitive failure: attempted, then
        // delivery-failed{delivery} — the class ONLY on the failed row.
        let events = dispatch::dispositions::read_local_events(&tpaths);
        let rows: Vec<(dispatch::dispositions::EventKind, Option<String>)> =
            events.records.iter().map(event_row).collect();
        assert_eq!(
            rows,
            vec![
                (dispatch::dispositions::EventKind::Attempted, None),
                (dispatch::dispositions::EventKind::DeliveryFailed, Some("delivery".to_string())),
            ]
        );
        for d in &events.records {
            assert_eq!(d.correlation_id(), log.records[0].correlation_id);
        }
    }

    #[test]
    fn durability_custom_expires_is_reflected_in_the_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, env, _lane) = jailed_daemon_lane(tmp.path());
        let attempt = ProbeAttempt::delivered();

        // 30m in ms (what parse_expires("30m") yields).
        let thirty_min_ms = 30 * 60_000;
        deliver_with_durability(
            &env,
            &paths,
            &attempt,
            "worker",
            "body",
            thirty_min_ms,
            None,
        );
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        let e = &log.records[0];
        assert_eq!(e.expires_at, e.authored_at + thirty_min_ms, "--expires window honored");
    }

    // === qd–qf W3c: caller-supplied correlation_id (the frame↔qd origin seam) ===
    //
    // provider-contract §4: `submit(address, body, correlation_id)` is
    // CALLER-SUPPLIED — frame's ledger event id rides through as the envelope's
    // correlation_id when frame originates; qd mints its own ULID only for BARE
    // sends. These pin the supplied-vs-mint branch in `deliver_with_durability` at
    // the seam: a supplied id lands in BOTH the log envelope AND the stamped
    // disposition (they must key on the same id); absent ⇒ a fresh 26-char ULID.

    /// A caller-supplied id (frame's event id) becomes the envelope's
    /// correlation_id AND the disposition's — NOT a minted ULID. This is the
    /// round-trip proof the frame↔qd origin seam requires.
    #[test]
    fn durability_uses_the_supplied_correlation_id_in_envelope_and_disposition() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, env, _lane) = jailed_daemon_lane(tmp.path());
        let attempt = ProbeAttempt::delivered();

        let code = deliver_with_durability(
            &env,
            &paths,
            &attempt,
            "worker",
            "hello body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            Some("FRAME-EVT-123".to_string()), // frame's ledger event id
        );
        assert_eq!(code, 0);

        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1);
        assert_eq!(
            log.records[0].correlation_id, "FRAME-EVT-123",
            "the log envelope carries the caller-supplied id verbatim (no mint)"
        );
        let events = dispatch::dispositions::read_local_events(&tpaths);
        assert!(!events.records.is_empty());
        assert!(
            events.records.iter().all(|e| e.correlation_id() == "FRAME-EVT-123"),
            "every stamped event keys on the SAME supplied id as the envelope"
        );
        // Not a minted ULID (26 Crockford chars): the supplied id is 13 chars.
        assert_ne!(log.records[0].correlation_id.len(), 26);
    }

    /// Absent supplied id ⇒ qd mints its own 26-char ULID (the BARE-send default,
    /// unchanged). Envelope + disposition still share it.
    #[test]
    fn durability_mints_a_ulid_when_no_id_is_supplied() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, env, _lane) = jailed_daemon_lane(tmp.path());
        let attempt = ProbeAttempt::delivered();

        deliver_with_durability(
            &env,
            &paths,
            &attempt,
            "worker",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None, // bare send ⇒ mint
        );
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1);
        assert_eq!(
            log.records[0].correlation_id.len(),
            26,
            "no supplied id ⇒ a minted 26-char ULID"
        );
        let events = dispatch::dispositions::read_local_events(&tpaths);
        assert!(
            events
                .records
                .iter()
                .all(|e| e.correlation_id() == log.records[0].correlation_id),
            "every event joins the minted id"
        );
    }

    /// The supplied id also threads the RESUME-AND-DELIVER path (a not-live target
    /// woken then delivered) — it shares the same origin envelope, so the logged
    /// envelope AND the delivered disposition key on it.
    #[test]
    fn wake_then_deliver_uses_the_supplied_correlation_id() {
        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());

        let code = deliver_with_durability(
            &env,
            &paths,
            &ProbeAttempt::woke_and_delivered(),
            "worker",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            Some("FRAME-EVT-WAKE".to_string()),
        );
        assert_eq!(code, 0);
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records[0].correlation_id, "FRAME-EVT-WAKE");
        let events = dispatch::dispositions::read_local_events(&tpaths);
        assert!(!events.records.is_empty());
        assert!(
            events.records.iter().all(|e| e.correlation_id() == "FRAME-EVT-WAKE"),
            "every wake-path event keys on the supplied id"
        );
    }

    // === TRANSITION §6 — THE DISCRIMINATING FUNNEL SCENARIO ==================
    //
    // The permanent §6 acceptance scenario: a definitive delivery failure, then a
    // retry (same correlation id) through the wake path that succeeds. Under the
    // R8 event model the disposition log must hold the FULL witnessed funnel —
    // `attempted, delivery-failed, attempted, queued, delivered` in file order —
    // and the projection must fold it to a Delivered summary with attempts=2.
    // This is the test that would have caught the terminals-only collapse (the
    // old "first terminal wins" model either blocked the retry or summarized the
    // id as failed forever).

    #[test]
    fn fail_then_retry_then_succeed_writes_the_funnel() {
        use dispatch::dispositions::{
            project_summary, read_local_events, read_local_log, EventKind, SummaryState,
        };

        let tmp = tempfile::tempdir().unwrap();
        let (paths, env, lane) = jailed_daemon_lane(tmp.path());

        // Invocation 1 — LIVE origin path; the REAL codex carrier refuses at its
        // reachability door (a definitive delivery failure): attempted +
        // delivery-failed{delivery}.
        {
            let ops = quorum_qw::lanes::lane_ops(lane, &env, paths.clone());
            let code = deliver_with_durability(
                &env,
                &paths,
                &LaneAttempt {
                    ops: &ops,
                    id: SessionId(JAILED_SID.into()),
                    policy: live_policy(),
                    message: "hello body".into(),
                    send_id: "m-test".to_string(),
                },
                "worker",
                "hello body",
                dispatch::origin_send::DEFAULT_EXPIRES_MS,
                Some("Q6FUNNEL".to_string()),
            );
            assert_ne!(code, 0, "invocation 1 is a definitive delivery failure");
        }

        // Invocation 2 — the RETRY, SAME supplied id, through the wake path: the
        // revive succeeds and the delivery lands: attempted + queued + delivered.
        let code = deliver_with_durability(
            &env,
            &paths,
            &ProbeAttempt::woke_and_delivered(),
            "worker",
            "hello body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            Some("Q6FUNNEL".to_string()),
        );
        assert_eq!(code, 0, "invocation 2 (the retry) delivers");

        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);

        // dispositions.jsonl IN FILE ORDER: the exact funnel — event types AND
        // the reason appearing ONLY on the delivery-failed row.
        let events = read_local_events(&tpaths);
        assert_eq!(events.corrupt_interior, 0, "every event row parses");
        let rows: Vec<(EventKind, Option<String>)> = events
            .records
            .iter()
            .filter(|e| e.correlation_id() == "Q6FUNNEL")
            .map(event_row)
            .collect();
        assert_eq!(
            rows,
            vec![
                (EventKind::Attempted, None),
                (EventKind::DeliveryFailed, Some("delivery".to_string())),
                (EventKind::Attempted, None),
                (EventKind::Queued, None),
                (EventKind::Delivered, None),
            ],
            "the full funnel in file order; class only on delivery-failed"
        );

        // log.jsonl: ONE envelope for the id (R15 no-double-append). The first
        // origin invocation logs it; the retry (SAME id + SAME body, not yet
        // delivered) is a caller retry that REUSES the logged envelope rather than
        // appending a second — the R15 duplicate-submit rule.
        let log = read_local_log(&tpaths);
        let envelopes: Vec<_> = log
            .records
            .iter()
            .filter(|e| e.correlation_id == "Q6FUNNEL")
            .collect();
        assert_eq!(
            envelopes.len(),
            1,
            "R15: the retry reuses the logged envelope — no double-append"
        );

        // The projection over the files at a PRE-expiry now: the funnel folds to
        // Delivered with attempts=2 (the fix the event model exists for).
        let now = envelopes[0].authored_at + 1; // well inside the 12h window
        let summaries = project_summary(&log.records, &events.records, now);
        let s = summaries
            .iter()
            .find(|r| r.correlation_id == "Q6FUNNEL")
            .expect("one summary for the id");
        assert_eq!(s.state, SummaryState::Delivered, "delivered event exists");
        assert_eq!(s.attempts, 2, "two attempted events across the retry");
        assert_eq!(s.last_event, Some(EventKind::Delivered));
        assert!(s.first_delivered_at.is_some(), "first_delivered_at is set");
    }

    // === qd–qf W3b: resume-and-deliver + failed{wake} ========================
    //
    // A NOT-live target logs the envelope FIRST, then the lane wakes and delivers
    // inside ONE `deliver` call, and the ledger stamps from what the receipt says.
    // The `Attempt` is stubbed (see the section note above): a real lane here
    // would drive a real revive.

    #[test]
    fn wake_then_deliver_logs_envelope_wakes_then_delivers_into_refreshed_row() {
        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let attempt = ProbeAttempt::woke_and_delivered();

        let code = deliver_with_durability(
            &env,
            &paths,
            &attempt,
            "worker@devbox",
            "hello body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None, // no caller-supplied id ⇒ qd mints a ULID
        );
        assert_eq!(code, 0, "woken + delivered ⇒ exit 0");
        assert_eq!(attempt.calls.get(), 1, "exactly one attempt");

        // Envelope logged FIRST (write-then-deliver); the wake-path funnel is
        // attempted, queued (the lane reported a wake), then delivered.
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1, "envelope logged before the attempt");
        assert_eq!(log.records[0].target, "worker@devbox");
        let events = dispatch::dispositions::read_local_events(&tpaths);
        let rows: Vec<(dispatch::dispositions::EventKind, Option<String>)> =
            events.records.iter().map(event_row).collect();
        assert_eq!(
            rows,
            vec![
                (dispatch::dispositions::EventKind::Attempted, None),
                (dispatch::dispositions::EventKind::Queued, None),
                (dispatch::dispositions::EventKind::Delivered, None),
            ],
            "wake-origin funnel in file order"
        );
        for d in &events.records {
            assert_eq!(d.correlation_id(), log.records[0].correlation_id);
        }
    }

    /// `Receipt::woke` is what decides the `queued` row — nothing else does.
    ///
    /// The LIVE funnel has no `queued`, so the row's PRESENCE is how the ledger
    /// records that a wake happened at all. Since `deliver` is atomic, the ONLY
    /// evidence qd has is the receipt, and the rule (`Receipt::woke`'s own docs)
    /// is: stamp on `Yes` or `Unknown`, never on `No`. `Unknown` is a real answer
    /// — a lane that revived something it cannot re-confirm still attempted a
    /// wake, and the ledger's question is whether one happened.
    ///
    /// MUTATION EVIDENCE: collapsing `Unknown` into "no wake" reds the middle
    /// case; stamping unconditionally reds the first.
    #[test]
    fn queued_is_stamped_from_the_receipt_and_only_when_a_wake_happened() {
        for (woke, expect_queued) in [
            (Confirmation::No, false),
            (Confirmation::Yes, true),
            (Confirmation::Unknown, true),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let env = jail_env(tmp.path());
            let paths = dispatch::paths::QdPaths::from_home(tmp.path());
            let code = deliver_with_durability(
                &env,
                &paths,
                &ProbeAttempt::new(Ok(Receipt {
                    message_id: quorum_qw::contract::MessageId("probe-mid".into()),
                    accepted: true,
                    terminal: quorum_qw::contract::TerminalExpectation::Pending,
                    woke,
                })),
                "wk",
                "body",
                dispatch::origin_send::DEFAULT_EXPIRES_MS,
                None,
            );
            assert_eq!(code, 0);
            let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
            let rows: Vec<dispatch::dispositions::EventKind> =
                dispatch::dispositions::read_local_events(&tpaths)
                    .records
                    .iter()
                    .map(|e| event_row(e).0)
                    .collect();
            let expected = if expect_queued {
                vec![
                    dispatch::dispositions::EventKind::Attempted,
                    dispatch::dispositions::EventKind::Queued,
                    dispatch::dispositions::EventKind::Delivered,
                ]
            } else {
                vec![
                    dispatch::dispositions::EventKind::Attempted,
                    dispatch::dispositions::EventKind::Delivered,
                ]
            };
            assert_eq!(rows, expected, "woke = {woke:?}");
        }
    }

    /// `queued` is stamped AFTER the attempt resolves, not before it is tried.
    ///
    /// This is the stage-2 phase-2 timing change, pinned so it cannot silently
    /// revert — and so the cost stays visible. `queued`'s `created_at` is no
    /// longer the moment the message was placed awaiting the wake; it is the
    /// moment qd learned a wake had been attempted, which on a real revive is
    /// seconds later. R14.1 still holds (no retro-dating); the row schema's
    /// "record time and happen time coincide to within ms" no longer does, and
    /// both `doc/formats/dispatch-transport-formats.md` and
    /// `DispositionEvent::queued` say so.
    ///
    /// WHY it moved: `LaneOps::deliver` performs the wake INSIDE the call, so qd
    /// can only learn that one happened from the lane's answer
    /// (`Receipt::woke`, or a returned `WakeFailed`). Asking `health` first is the
    /// composition that method's atomicity rule forbids.
    ///
    /// MUTATION EVIDENCE: moving the `queued` stamp above `attempt.run()` reds the
    /// first assert; deleting it reds the second.
    #[test]
    fn queued_is_stamped_after_the_attempt_resolves_not_before_it_is_tried() {
        /// An [`Attempt`] that PHOTOGRAPHS the event log at the instant it runs —
        /// the only way to observe stamp ORDER against the wake from outside, now
        /// that the wake is inside the call.
        struct SnapshottingAttempt {
            tpaths: dispatch::paths::QdPaths,
            at_attempt: RefCell<Vec<dispatch::dispositions::EventKind>>,
        }
        impl Attempt for SnapshottingAttempt {
            fn run(&self) -> Result<Receipt, LaneError> {
                *self.at_attempt.borrow_mut() =
                    dispatch::dispositions::read_local_events(&self.tpaths)
                        .records
                        .iter()
                        .map(|e| event_row(e).0)
                        .collect();
                ProbeAttempt::woke_and_delivered().run()
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let attempt = SnapshottingAttempt {
            tpaths: dispatch::paths::QdPaths::from_home_env(&paths.home, &env),
            at_attempt: RefCell::new(Vec::new()),
        };

        let code = deliver_with_durability(
            &env,
            &paths,
            &attempt,
            "wk",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None,
        );
        assert_eq!(code, 0);
        assert_eq!(
            *attempt.at_attempt.borrow(),
            vec![dispatch::dispositions::EventKind::Attempted],
            "at the moment the attempt runs the ledger must hold `attempted` ALONE — \
             `queued` is stamped from the attempt's OUTCOME"
        );
        // And the funnel that reaches disk is unchanged, which is the whole point:
        // the ORDER survives the timing change.
        let events = dispatch::dispositions::read_local_events(&tpaths);
        let rows: Vec<dispatch::dispositions::EventKind> =
            events.records.iter().map(|e| event_row(e).0).collect();
        assert_eq!(
            rows,
            vec![
                dispatch::dispositions::EventKind::Attempted,
                dispatch::dispositions::EventKind::Queued,
                dispatch::dispositions::EventKind::Delivered,
            ]
        );
    }

    /// A wake that could not succeed: `attempted, queued, delivery-failed{wake}`,
    /// exit 12, and the CORE's own message under qd's class.
    ///
    /// `queued` is stamped even though the wake FAILED, because a wake that failed
    /// is still a wake that happened — which is what keeps this funnel and the
    /// success funnel reading exactly as they did.
    #[test]
    fn wake_then_deliver_unwakeable_target_stamps_failed_wake_exit_12() {
        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let attempt = ProbeAttempt::new(Err(LaneError::WakeFailed {
            detail: "could not revive claude session \"wk\"".into(),
            exit_code: 1,
            self_attributed: false,
        }));

        let code = deliver_with_durability(
            &env,
            &paths,
            &attempt,
            "wk",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None,
        );
        assert_eq!(
            code,
            dispatch::origin_send::EXIT_REFUSED,
            "failed{{wake}} rides the shared refusal door code (12), NOT the revive \
             core's own exit — `qd send` has one wake-failure exit and always has"
        );

        // The envelope was still logged FIRST; the funnel reads attempted, queued
        // (a wake was attempted), then delivery-failed{wake} — so an operator can
        // read back the outcome.
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1, "envelope logged even though the wake failed");
        let events = dispatch::dispositions::read_local_events(&tpaths);
        let rows: Vec<(dispatch::dispositions::EventKind, Option<String>)> =
            events.records.iter().map(event_row).collect();
        assert_eq!(
            rows,
            vec![
                (dispatch::dispositions::EventKind::Attempted, None),
                (dispatch::dispositions::EventKind::Queued, None),
                (dispatch::dispositions::EventKind::DeliveryFailed, Some("wake".to_string())),
            ],
            "wake failure funnel: attempted, queued, delivery-failed{{wake}}"
        );
        for d in &events.records {
            assert_eq!(d.correlation_id(), log.records[0].correlation_id);
        }
    }

    /// A revive that reports success but yields a row with no live receive path is
    /// a wake that did not produce a deliverable target → `failed{wake}`, never a
    /// silent no-op or a `delivered`.
    ///
    /// The verdict MOVED but did not change: `wake_then_deliver` used to re-run
    /// `select_carrier` on the refreshed row and call the miss a wake failure;
    /// `quorum_qw::lanes::no_live_receive_path` now answers `WakeFailed` for
    /// exactly the same condition, because it knows whether THIS delivery revived
    /// the row. This pins qd's half — that such an answer still lands
    /// `delivery-failed{wake}` and exit 12.
    #[test]
    fn a_revived_but_unroutable_row_is_also_failed_wake() {
        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let code = deliver_with_durability(
            &env,
            &paths,
            // Verbatim the shape `no_live_receive_path` answers when a wake
            // happened first.
            &ProbeAttempt::new(Err(LaneError::WakeFailed {
                detail: "revived \"wk\" but it has no live receive path".into(),
                exit_code: 12,
                self_attributed: false,
            })),
            "wk",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None,
        );
        assert_eq!(code, dispatch::origin_send::EXIT_REFUSED);
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let events = dispatch::dispositions::read_local_events(&tpaths);
        let last = events.records.last().expect("a delivery-failed event was stamped");
        assert_eq!(
            event_row(last),
            (dispatch::dispositions::EventKind::DeliveryFailed, Some("wake".to_string()))
        );
    }

    /// A carrier that refused BEFORE minting an id (`CarrierOutcome::unkeyed`)
    /// reaches qd as `LaneError::Transport`, and it must still be an ordinary
    /// `delivery-failed{delivery}` — NOT a `failed{wake}` (nothing was revived)
    /// and NOT a fresh qd-authored stderr line (the carrier already printed its
    /// own loud one; that is the whole content of an unkeyed refusal).
    #[test]
    fn a_carrier_refusal_with_no_minted_id_is_a_plain_delivery_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let code = deliver_with_durability(
            &env,
            &paths,
            &ProbeAttempt::new(Err(LaneError::Transport {
                detail: "the carrier refused before minting a message id (exit 1)".into(),
            })),
            "wk",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None,
        );
        assert_eq!(code, 1);
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let rows: Vec<(dispatch::dispositions::EventKind, Option<String>)> =
            dispatch::dispositions::read_local_events(&tpaths)
                .records
                .iter()
                .map(event_row)
                .collect();
        assert_eq!(
            rows,
            vec![
                (dispatch::dispositions::EventKind::Attempted, None),
                (dispatch::dispositions::EventKind::DeliveryFailed, Some("delivery".to_string())),
            ],
            "no `queued` — nothing was woken; and the class is `delivery`, not `wake`"
        );
    }

}
