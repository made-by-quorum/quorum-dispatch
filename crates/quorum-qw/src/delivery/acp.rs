//! The `acp_daemon` carrier — the ACP resident adapter. BOTH acp lanes.
//!
//! scoped-ACP-CC residence SEND path (S7). The ACP analog of
//! [`super::codex::send_codex`]: re-read the row's recorded `endpoint`, verify the
//! resident adapter's IDENTITY (pid alive AND the live `/proc` cmdline carries our
//! `acp-daemon --listen <endpoint>` — S6, defeats PID reuse), derive the tier from
//! `(provider, transport-field, endpoint-alive)`.
//!
//! **Transport-loss disposition (Child D, opencode D1 — clerk-4's Arm-B
//! ratification, bond note 01KX01BY7G): `acp/claude-code` is a NAMED DIVERGENCE.
//! On ANY transport loss this carrier REFUSES and surfaces (the same
//! "not reachable (try qd resume …)" class as `codex`/`acp/opencode`, exit 1) —
//! with the session's identity first preserved in the qd-owned tombstone store
//! ([`crate::tombstone`]; [`super::acp_loss::preserve_identity`]). There is NO
//! auto-deliver path: Child B's degrade+latch+companion-drive machinery was
//! REMOVED (not gated), so unreachability is structural.** pi's auto-deliver
//! floor ([`super::pi::send_pi_floor`]) is the deliberately separate, untouched
//! realization of D1's graceful degrade.
//!
//! Three refusal-relevant lanes:
//!   - **entry-lane dead** (no live pid, dead endpoint, or a historical
//!     `transport=="pty"` latch — anything but a healthy structured tier) →
//!     preserve identity + refuse. Pre-send vs post-send history no longer
//!     changes the disposition: both refuse (post-send always refused; pre-send
//!     joined it under Arm B).
//!   - **`AcpConnection::connect` fails** → RE-PROBE liveness+identity, then
//!     split (the round-1 TOCTOU fix; ladder's `classify_connect_failure`):
//!     still alive + verified ⇒ a genuine wedge, refuse with NO tombstone
//!     (live-pid row is not janitor-reapable); now dead / identity gone ⇒ the
//!     daemon died across the probe→connect boundary ⇒ preserve identity +
//!     refuse. See the `Err(_)` arm below and ladder.rs clause 3 (including
//!     the stated wedge-dies-later residual).
//!   - **mid-flight `NoTransport`** (a live connect, then the `session/prompt`
//!     dispatch itself fails) → preserve identity + refuse. The
//!     `acp_pre_dispatch` hook may have JUST durably marked
//!     `structured_send_issued=true` before the failure (the exactly-once
//!     dispatch-timing guard, kept intact) — that record is wire-history truth,
//!     it just no longer selects a different disposition.
//!
//! `QD_ACP_PTY_FLOOR_DISABLE` is now a NO-OP: it gated only the retired floor
//! drive, and refusal is the unconditional behavior it used to select.
//!
//! **Scope of the tombstone: `acp/claude-code` only.**
//! [`super::acp_loss::preserve_identity`] self-gates on the provider, so
//! `acp/opencode` (which also routes through this carrier) keeps its
//! byte-identical plain refusal — no store write, no extra output.
//!
//! Nothing here prints and nothing here exits; see [`super`].

use std::cell::RefCell;
use std::time::Duration;

use crate::delivery::{
    append_send_invoked, emit_daemon_send_events, emit_door_failure, CarrierError, CarrierResult,
    Delivered, Notes, Refused, SendDeps, SendParams,
};

use super::acp_loss;

/// The verb every line this carrier produces is attributed to.
///
/// [`super::acp_loss::preserve_identity`]'s note is `qd <verb>:`-attributed like
/// the refusal it precedes, but a NOTE is a `String` in [`Notes`] rather than a
/// [`CarrierError`], so it cannot be stamped at [`super::render`] time the way
/// [`AcpSendError::line`] is — the verb has to be supplied here.
///
/// It is a constant rather than a parameter ONLY because `send_acp`'s two
/// callers both pass the literal `"send:relay"` to [`super::render`] already
/// (`lanes.rs`'s `deliver` — which states that hard-coding as a REPORTED finding
/// of its own — and `bin/qd/verbs/send_relay.rs`). So this names the same one
/// verb they do, in one place, instead of a third and fourth loose literal. The
/// honest fix is to thread `verb` into `send_acp` from those two call sites;
/// that edit lands in `lanes.rs` and is reported, not made here.
const SEND_VERB: &str = "send:relay";

/// Why the ACP carrier could not deliver. DELIBERATELY NO `Display` — see
/// [`CarrierError`]. Both variants are `qd <verb>:`-attributed.
#[derive(Debug)]
pub enum AcpSendError {
    /// Every transport-loss and wedge surface: no live pid, a dead tier, a failed
    /// connect, or a mid-flight `NoTransport`. ONE human-recovery wording, because
    /// the recovery action is the same in all four.
    NotReachable { name: String },
    /// A refused/precondition inject (queue full, etc.) — the wire was live and
    /// the resident said no.
    InjectFailed { name: String, detail: String },
}

impl CarrierError for AcpSendError {
    fn line(&self, verb: &str) -> Option<String> {
        Some(match self {
            AcpSendError::NotReachable { name } => format!(
                "qd {verb}: \"{name}\": acp session daemon not reachable (try qd resume {name})"
            ),
            AcpSendError::InjectFailed { name, detail } => {
                format!("qd {verb}: \"{name}\": send failed ({detail}).")
            }
        })
    }

    fn exit_code(&self) -> i32 {
        1
    }
}

/// Deliver `message` into an ACP resident adapter's session.
///
/// Answers the turn id this send minted, which [`emit_daemon_send_events`]
/// already writes as `Payload::SendInitiated.send_id`. Every refusal above — and
/// there are four transport-loss ones — fires before any turn exists, so it
/// carries no id.
pub fn send_acp(deps: &SendDeps<'_>, params: &SendParams<'_>) -> CarrierResult<AcpSendError> {
    use crate::provider::acp::{
        classify_connect_failure, derive_tier, AcpClient, AcpConnection, ConnectFailure, Tier,
        ACP_CC_PROVIDER,
    };
    use crate::provider::{InjectError, Provider, ProviderFx, SessionKey};

    let session = params.session;
    let message = params.message;
    let name = session.name.clone().unwrap_or_default();
    // Interior mutability because `mark_dispatched` — installed into the fx and
    // driven from INSIDE `inject` — can produce a note of its own.
    let notes: RefCell<Notes> = RefCell::new(Notes::new());

    // §C1 (door-inventory B2) — record-then-fail-loud: every acp-door "not
    // reachable" refusal leaves a `send-failed` account (best-effort, keyed to the
    // TARGET session, `send_id` omitted pre-wire) BEFORE the refusal, so no
    // acp-door failure is stderr-only. `reason` names the surface. Serves BOTH
    // acp/claude-code AND acp/opencode (both route through this carrier). The emit
    // never alters the refusal; the identity-preservation tombstone is unchanged.
    let not_reachable = |reason: &str| {
        emit_door_failure(deps.env, deps.clock, &name, Some(session), message, reason);
        AcpSendError::NotReachable { name: name.clone() }
    };
    let refuse = |notes: &RefCell<Notes>, error: AcpSendError| Refused {
        notes: notes.borrow().clone(),
        error,
        // Every ACP refusal fires before a turn exists — see the doc on
        // [`Refused::message_id`].
        message_id: None,
    };

    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        // A row with no live pid is already-lost transport (the janitor may
        // have reaped the registry row entirely) — preserve identity, refuse.
        notes.borrow_mut().extend(acp_loss::preserve_identity(
            deps.env,
            session,
            "acp session has no live daemon pid at send entry",
            SEND_VERB,
        ));
        return Err(refuse(&notes, not_reachable("daemon-unreachable")));
    };
    // The endpoint + degradation latch live in the row (re-read by pid; NOT on --json).
    let entry = crate::registry::read_entry(&deps.paths.sessions_dir, pid);
    let endpoint = entry
        .as_ref()
        .and_then(|e| e.endpoint.clone())
        .filter(|s| !s.is_empty());
    let transport_field = entry.as_ref().and_then(|e| e.transport.clone());

    // S6 identity + liveness: a connect-success is liveness, NOT identity — the cmdline
    // (+ pid liveness) is the identity fence against PID reuse.
    let cmdline = crate::create_daemon::real_cmdline_probe(pid);
    let endpoint_alive = endpoint.is_some()
        && crate::effects::is_pid_alive(pid as i32)
        && crate::provider::acp::residence::cmdline_is_our_acp_daemon(
            cmdline.as_deref(),
            endpoint.as_deref(),
        );

    let tier = derive_tier(crate::lane::Mode::Acp, transport_field.as_deref(), endpoint_alive);

    if tier != Tier::Acp {
        // Entry-lane transport loss (dead endpoint, or a historical pty latch):
        // the named-divergence disposition — preserve identity (qd-owned store,
        // acp/claude-code only; a no-op for acp/opencode, whose refusal stays
        // byte-identical), then refuse to the human-recovery surface. Whether a
        // structured send was ever issued no longer branches here: pre-send and
        // post-send loss BOTH refuse (Arm B — the Child-B pre-send auto-deliver
        // was removed, not gated).
        notes.borrow_mut().extend(acp_loss::preserve_identity(
            deps.env,
            session,
            "acp endpoint not reachable at send entry",
            SEND_VERB,
        ));
        return Err(refuse(&notes, not_reachable("daemon-unreachable")));
    }
    let endpoint = endpoint.expect("Tier::Acp implies a live endpoint");

    let conn = match AcpConnection::connect(&endpoint, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(_) => {
            // F2 (red-team round 1) + the round-1 TOCTOU fix: `tier == Tier::Acp`
            // proved `endpoint_alive` BEFORE connect, but that probe cannot
            // confirm liveness ACROSS the connect boundary — a daemon can die in
            // the window and its row is then janitor-reapable. So RE-PROBE now
            // and classify (ladder's `classify_connect_failure`):
            //   - still alive + identity-verified ⇒ a genuine wedge: refuse with
            //     NO tombstone (live-pid row is not janitor-reapable; `qd resume`
            //     kills + restarts — the only safe way to clear a wedge). A
            //     wedge that dies LATER with no further qd interaction is the
            //     ACCEPTED residual — stated + defended in ladder.rs clause 3
            //     (next interaction hits the entry lane; ids.jsonl + the CC
            //     transcript carry the recovery-critical identity regardless).
            //   - now dead / identity gone ⇒ it died across the boundary: a
            //     transport LOSS — preserve identity, then the same refusal.
            // Never a floor drive either way.
            let pid_alive_now = crate::effects::is_pid_alive(pid as i32);
            let cmdline_is_ours_now = crate::provider::acp::residence::cmdline_is_our_acp_daemon(
                crate::create_daemon::real_cmdline_probe(pid).as_deref(),
                Some(endpoint.as_str()),
            );
            if classify_connect_failure(pid_alive_now, cmdline_is_ours_now)
                == ConnectFailure::TransportLost
            {
                notes.borrow_mut().extend(acp_loss::preserve_identity(
                    deps.env,
                    session,
                    "acp daemon died across the connect boundary (was live at the pre-connect probe)",
                    SEND_VERB,
                ));
            }
            return Err(refuse(&notes, not_reachable("daemon-unreachable")));
        }
    };
    let key = SessionKey {
        id: &session.session_id,
        name: session.name.as_deref(),
        cwd: session.cwd.as_deref(),
        pid: session.pid,
    };
    let from = super::derive_from_session(deps.env);

    // Scoped so the fx — and with it `mark_dispatched`'s borrow of `notes` —
    // is released before the notes are taken back out below.
    let result = {
        let conn_ref: &dyn AcpClient = &conn;
        // Child B exactly-once guard: durably mark `structured_send_issued=true` the
        // MOMENT this turn's bytes are confirmed on the wire (see
        // `AcpConnection::prompt`) — before we know whether the reply ever arrives.
        // Never gated on `inject`'s `Ok` return: a crash or socket drop right after
        // dispatch must still leave this true for the NEXT process to read.
        let sessions_dir = deps.paths.sessions_dir.clone();
        let mark_dispatched = || {
            if let Some(mut e) = crate::registry::read_entry(&sessions_dir, pid) {
                if e.structured_send_issued != Some(true) {
                    e.structured_send_issued = Some(true);
                    if let Err(err) = crate::registry::write_entry(&sessions_dir, &e) {
                        notes.borrow_mut().push(format!(
                            "qd send:relay: could not persist the structured-send marker: {err}"
                        ));
                    }
                }
            }
        };
        let fx = ProviderFx {
            await_relay: None,
            env: deps.env,
            paths: deps.paths,
            socket_dir: deps.paths.sessions_dir.clone(),
            mux: None,
            clock: None,
            sleeper: None,
            relay: None,
            relay_port: None,
            app_server: None,
            codex_expected_turn_id: None,
            acp_client: Some(conn_ref),
            pi_rpc: None,
            // F3 (red-team round 2, Child B era — the rule KEPT under Child D): this
            // hook is installed UNCONDITIONALLY. The historical bug was gating it on
            // the floor's disable flag (`floor_disabled()`, retired with the floor):
            // a send that dispatched but failed on the reply-read left
            // `structured_send_issued` unset — a false "never sent" history, which
            // in the auto-degrade era could double-deliver. The floor and its flag
            // are gone (every loss now refuses), but the rule stands: recording
            // that bytes reached the wire is history truth, never gated — the
            // resume seam consumes it, and a false record misleads any future
            // reader (registry.rs's field doc carries the current framing).
            acp_pre_dispatch: Some(&mark_dispatched),
        };
        ACP_CC_PROVIDER.inject(&fx, &key, message, &from)
    };
    match result {
        Ok(turn_id) => {
            // C5/C3: sent + delivered (turn-accepted) into the TARGET's log on the
            // inject ACK; the StopReason-mapped terminal lands later in run_acp_wait.
            emit_daemon_send_events(
                deps.env,
                deps.clock,
                &name,
                Some(session),
                message,
                &turn_id,
                &session.provider,
            );
            notes
                .borrow_mut()
                .extend(append_send_invoked(deps.env, deps.clock, &name));
            Ok(Delivered {
                stdout: Some(turn_id.clone()),
                message_id: turn_id,
                notes: notes.into_inner(),
            })
        }
        Err(InjectError::Precondition(s)) => {
            // §C1: a refused/precondition inject (queue full, etc.) — record the
            // door failure before refusing.
            emit_door_failure(
                deps.env,
                deps.clock,
                &name,
                Some(session),
                message,
                "inject-failed",
            );
            Err(refuse(
                &notes,
                AcpSendError::InjectFailed {
                    name: name.clone(),
                    detail: s,
                },
            ))
        }
        Err(err @ InjectError::NoTransport(_)) => {
            // Mid-flight transport loss (a live connect, then the dispatch
            // itself failed): the same named-divergence refusal as the
            // entry-lane arm. `mark_dispatched` may have JUST durably recorded
            // `structured_send_issued=true` (bytes confirmed on the wire before
            // the failure — the exactly-once guard, kept) — wire-history truth,
            // but no longer a disposition branch: pre-send and post-send loss
            // both refuse (Arm B).
            notes.borrow_mut().extend(acp_loss::preserve_identity(
                deps.env,
                session,
                &format!("acp transport lost mid-flight ({err})"),
                SEND_VERB,
            ));
            Err(refuse(&notes, not_reachable("transport-lost")))
        }
        Err(_) => Err(refuse(&notes, not_reachable("daemon-unreachable"))),
    }
}

#[cfg(test)]
mod tests {
    /// `send_acp`'s own body, from its signature to the test module — the scope
    /// every structural guard below reads. Deliberately NOT the whole file: a
    /// whole-file `contains` check would be VACUOUSLY true, since these tests'
    /// own assertion strings quote the exact markers they look for.
    fn send_acp_body() -> &'static str {
        let src = include_str!("acp.rs");
        let fn_start = src
            .find("pub fn send_acp(deps: &SendDeps<'_>, params: &SendParams<'_>)")
            .expect("send_acp must still exist verbatim");
        let after_start = &src[fn_start..];
        let fn_end = after_start
            .find("\n#[cfg(test)]")
            .expect("the test module must follow send_acp");
        &after_start[..fn_end]
    }

    // F2 regression guard (red-team round 1) + the round-1 TOCTOU-fix pin:
    // `send_acp`'s `AcpConnection::connect` failure arm is reached ONLY when
    // `tier == Tier::Acp` proved `endpoint_alive` BEFORE connect — a reading
    // that cannot confirm liveness ACROSS the connect boundary. The arm must
    // therefore (a) never floor-drive (the historical F2 hunt: auto-flooring a
    // possibly-live daemon risks a second live writer on one transcript), and
    // (b) RE-PROBE liveness+identity and preserve identity when the daemon died
    // in the window (`classify_connect_failure` ⇒ `TransportLost` ⇒
    // `preserve_identity`), refusing either way. This is a structural
    // (source-text) guard rather than a behavioral one because exercising the
    // real branch requires a pid that is BOTH genuinely alive AND
    // identity-verified via a real `/proc/<pid>/cmdline` read
    // (`cmdline_is_our_acp_daemon`). The classification SEMANTICS are
    // unit-pinned in ladder.rs (`connect_failure_*` tests).
    // MUTATION EVIDENCE: reintroducing a floor drive in the arm reds the bans;
    // dropping the re-probe or the loss-path preserve reds the positive
    // controls.
    #[test]
    fn acp_connect_failure_never_consults_the_ladder_or_drives_the_floor() {
        let body = send_acp_body();
        let start_marker = "let conn = match AcpConnection::connect(&endpoint, Duration::from_secs(5)) {";
        let start = body
            .find(start_marker)
            .expect("send_acp's connect-failure match must still exist verbatim");
        let match_block = &body[start..];
        // The match's Err(_) arm body runs from "Err(_) => {" to the matching "};"
        // that closes the whole `let conn = match ... };` statement — find that by
        // the next line consisting of only `    };` after the marker.
        let close = match_block
            .find("\n    };\n")
            .expect("the connect match must close with `    };` on its own line");
        let match_block = &match_block[..close];
        let err_arm_start = match_block
            .find("Err(_) => {")
            .expect("the connect match must still have an Err(_) arm");
        let err_arm = &match_block[err_arm_start..];

        for banned in [
            "on_inject_error",
            "DropToPtyFloor",
            "degrade_and_persist",
            "drive_acp_floor_send",
        ] {
            assert!(
                !err_arm.contains(banned),
                "send_acp's AcpConnection::connect failure arm must NEVER reach \
                 `{banned}` — the acp daemon is confirmed alive at this point (tier==Acp \
                 already proved endpoint_alive), so auto-flooring here risks a second live \
                 writer on the same session_id. Arm body:\n{err_arm}"
            );
        }
        // Positive control 1: it DOES refuse through not_reachable (an empty or
        // renamed arm would vacuously pass the bans above without refusing).
        assert!(
            err_arm.contains("not_reachable("),
            "the arm must still call not_reachable — a silently-removed refusal would also \
             vacuously pass the bans above. Arm body:\n{err_arm}"
        );
        // Positive controls 2+3 (round-1 TOCTOU fix): the arm must classify
        // from a FRESH post-failure re-probe, and preserve identity on the
        // loss classification — a connect failure is never blanket-treated as
        // "confirmed still alive" from the stale pre-connect reading.
        assert!(
            err_arm.contains("classify_connect_failure(pid_alive_now, cmdline_is_ours_now)"),
            "the connect-Err arm must re-probe and classify wedge-vs-loss. Arm body:\n{err_arm}"
        );
        assert!(
            err_arm.contains("acp_loss::preserve_identity("),
            "the connect-Err arm's TransportLost classification must preserve identity \
             before refusing. Arm body:\n{err_arm}"
        );
    }

    // F3 regression guard (red-team round 2, confirmed real): `send_acp`'s
    // `ProviderFx.acp_pre_dispatch` — the exactly-once history marker's install
    // point — must be installed UNCONDITIONALLY. Recording that a structured
    // send genuinely reached the wire is truth about the session's history and
    // must never be gated (originally: on the retired floor's disable flag).
    // The disposition no longer branches on it (Child D: pre-send and post-send
    // loss both refuse), but the durable record still guards any future reader
    // — e.g. `lifecycle.rs`'s resume seam consumes it — and losing it silently
    // was exactly the F3 bug. This is a structural (source-text) guard because
    // exercising the real branch needs a live, identity-verified acp connection
    // this module's test style deliberately avoids. MUTATION EVIDENCE: gating the
    // hook (`acp_pre_dispatch: some_condition.then(...)`) reds this test.
    #[test]
    fn acp_pre_dispatch_hook_is_installed_unconditionally() {
        let body = send_acp_body();
        let unconditional = "acp_pre_dispatch: Some(&mark_dispatched),";
        assert!(
            body.contains(unconditional),
            "send_acp's ProviderFx must install acp_pre_dispatch UNCONDITIONALLY as \
             `{unconditional}`. Function body:\n{body}"
        );
    }

    // Child D structural guard (opencode D1 — clerk-4's Arm-B ratification, bond
    // note 01KX01BY7G): `send_acp` must have NO reachable auto-deliver path
    // on transport loss. Child B's degrade+latch+companion-drive machinery was
    // REMOVED, not gated — so this guard is structural: the identifiers cannot
    // appear in the fn body at all. It also pins the replacement disposition:
    // identity preservation (`acp_loss::preserve_identity`) at all four
    // transport-loss sites — the no-live-pid entry, the dead-tier entry lane,
    // the connect-boundary death (round-1 TOCTOU fix: the re-probe's
    // TransportLost classification), and the mid-flight NoTransport arm — each
    // followed by the existing human-recovery refusal. MUTATION EVIDENCE:
    // reintroducing a floor drive (any banned identifier) reds this even though
    // it compiles; deleting a preserve_identity call reds the positive control.
    #[test]
    fn acp_send_transport_loss_refuses_with_identity_preserved_and_never_auto_delivers() {
        let body = send_acp_body();
        for banned in [
            "drive_acp_floor_send",
            "ensure_floor_pane",
            "degrade_and_persist",
            "degrade_to_pty",
            "on_inject_error",
            "DropToPtyFloor",
        ] {
            assert!(
                !body.contains(banned),
                "send_acp must have NO auto-deliver machinery — found `{banned}`. \
                 acp/claude-code is a NAMED DIVERGENCE (refuse-and-surface, identity \
                 preserved); pi's floor is the only auto-deliver realization. Body:\n{body}"
            );
        }
        let occurrences = body.matches("acp_loss::preserve_identity(").count();
        assert_eq!(
            occurrences, 4,
            "send_acp must preserve identity at exactly its four transport-loss \
             refusals (no-live-pid entry, dead-tier entry, connect-boundary death \
             [round-1 TOCTOU fix], mid-flight NoTransport) — found {occurrences}. \
             Body:\n{body}"
        );
    }
}
