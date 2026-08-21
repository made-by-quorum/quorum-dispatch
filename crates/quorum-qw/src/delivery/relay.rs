//! The `claude_relay` carrier — inject over the recorded relay.
//!
//! Unified-send entry into the Claude relay injection path. The caller supplies
//! the port captured from the already-resolved registry row, so this function
//! performs no fast lookup and no second target resolution. A failed POST is
//! returned as a relay failure; there is no PTY fallback branch here — the lane
//! chose this carrier and the choice is not re-litigated mid-delivery.
//!
//! Nothing here prints and nothing here exits; see [`super`].

use crate::delivery::{
    append_send_invoked, emit_door_failure, emit_relay_send_events, CarrierError, CarrierResult,
    Delivered, Notes, SendParams,
};
use crate::effects::{Clock, Env};
use crate::paths::QdPaths;
use crate::provider::claude::relay::RelayContract;

/// The relay carrier's injected effects. `relay` is the HTTP client and
/// `relay_port` the port the LANE resolved off the row — neither is re-derived
/// here, because re-deriving a target mid-delivery is how a send reaches the
/// wrong session.
pub struct RelaySendDeps<'a> {
    pub env: &'a dyn Env,
    /// The `.claude`-layout paths; only `sessions_dir` is read, as the fx's
    /// `socket_dir`.
    pub paths: &'a QdPaths,
    pub clock: &'a dyn Clock,
    pub relay: &'a dyn RelayContract,
    pub relay_port: u16,
}

/// The `.claude`-layout paths this carrier reads its fx `socket_dir` off.
///
/// Deliberately NOT the caller's `paths_from_home`: an unresolvable HOME is not
/// fatal to a relay inject (the port is already in hand and no registry lookup
/// follows), so it degrades to `/` rather than refusing. Both callers — the
/// `qd send:relay` wrapper and `LaneOps::deliver`'s claude arm — build it here so
/// that stays one decision.
pub fn relay_paths(env: &dyn Env) -> QdPaths {
    let home = env.var("HOME").filter(|s| !s.is_empty());
    QdPaths::from_home(std::path::Path::new(home.as_deref().unwrap_or("/")))
}

/// Why the claude relay carrier could not deliver.
///
/// DELIBERATELY NO `Display` — see [`CarrierError`]. Two of the three variants
/// carry NO `qd <verb>:` prefix and never did; they ignore the verb, which is the
/// honest encoding of "this line was never verb-attributed".
#[derive(Debug)]
pub enum RelaySendError {
    /// `provider_for("claude-code")` did not resolve — structurally impossible
    /// with the built-in table, kept for total-match safety.
    UnknownProvider,
    /// The POST failed, or the provider refused a precondition. Carries the text
    /// the pre-move body rendered through `send_err_text` (`RelayError`'s
    /// `Display`) or the precondition's own reason. NOT verb-attributed.
    SendFailed { detail: String },
    /// `InjectError::NoTransport` — defensive (the port is resolved before this
    /// carrier is chosen, so `inject` is never called without one). Kept
    /// byte-identical to the relay DOOR's wording so a future caller change
    /// cannot silently diverge, and it emits the door's `send-failed` record for
    /// the same reason. NOT verb-attributed.
    NoRelay { name: String },
}

impl CarrierError for RelaySendError {
    fn line(&self, verb: &str) -> Option<String> {
        Some(match self {
            RelaySendError::UnknownProvider => format!(
                "qd {verb}: unknown provider \"claude-code\" — relay delivery is unavailable."
            ),
            RelaySendError::SendFailed { detail } => format!("Failed to send message: {detail}"),
            RelaySendError::NoRelay { name } => format!("Session \"{name}\" has no relay."),
        })
    }

    fn exit_code(&self) -> i32 {
        1
    }
}

/// Inject `message` into the resolved claude session over its relay.
///
/// Returns the relay's own message id — the SAME id
/// [`emit_relay_send_events`] writes as `Payload::SendInitiated.send_id`, so it
/// is the ledger key `recovery_read` and the terminal watch both join on. Every
/// pre-inject refusal carries NO id, because at that point no message was minted.
pub fn send_claude_relay(
    deps: &RelaySendDeps<'_>,
    params: &SendParams<'_>,
) -> CarrierResult<RelaySendError> {
    use crate::provider::{InjectError, ProviderFx, SessionKey};

    let session = params.session;
    let message = params.message;
    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    let Some(provider_impl) = crate::provider::provider_for("claude-code") else {
        return Err(RelaySendError::UnknownProvider.into());
    };
    let from_session = super::derive_from_session(deps.env);

    // Build ProviderFx inline (rather than through the verb's shared
    // `inject_via_provider`) so the resolved session UUID is the `SessionKey.id`,
    // keeping the display name separate. QS-2 identity: the resolved UUID pins
    // the injection identity; `name` is display only. `inject_via_provider` uses
    // the name as BOTH id and name because the `send:relay` fast path has no
    // `Session` row; unified send always has the full resolved `Session`.
    let fx = ProviderFx {
        await_relay: None,
        env: deps.env,
        paths: deps.paths,
        socket_dir: deps.paths.sessions_dir.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: Some(deps.relay),
        relay_port: Some(deps.relay_port),
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    let key = SessionKey {
        id: &session.session_id,
        name: Some(&name),
        cwd: None,
        pid: None,
    };
    let message_id = match provider_impl.inject(&fx, &key, message, &from_session) {
        Ok(id) => id,
        Err(InjectError::RelayFailed(e)) => {
            return Err(RelaySendError::SendFailed {
                detail: e.to_string(),
            }
            .into())
        }
        Err(InjectError::NoTransport(_)) => {
            // §C1: the relay door emits `send-failed` (byname-keyed — the
            // pre-move body passed `None` here even with a row in scope, and the
            // keying is part of the recorded contract) BEFORE failing loud, like
            // every relay-door exit.
            emit_door_failure(deps.env, deps.clock, &name, None, message, "no-relay");
            return Err(RelaySendError::NoRelay { name }.into());
        }
        Err(InjectError::Precondition(reason)) => {
            // Unreachable for claude (daemon-only steer). Total-match safety.
            return Err(RelaySendError::SendFailed { detail: reason }.into());
        }
    };

    // Reuse the relay carrier's existing evidence semantics. A POST
    // acknowledgement is represented as queued/delivered, never as a reply.
    emit_relay_send_events(
        deps.env,
        deps.clock,
        &name,
        Some(session),
        message,
        &message_id,
    );
    let mut notes = Notes::new();
    notes.extend(append_send_invoked(deps.env, deps.clock, &name));
    Ok(Delivered {
        stdout: Some(message_id.clone()),
        message_id,
        notes,
    })
}

#[cfg(test)]
mod tests {
    // QS-2 structural guard: [`send_claude_relay`] MUST inject using the resolved
    // session UUID (`session.session_id`) as the `SessionKey.id` — NOT a display
    // name. The prior bug delegated to the verb's `inject_via_provider`, which set
    // `id = display_name`. The fix inlines the `ProviderFx` construction. This
    // test pins the fix so that future refactors cannot silently revert to
    // name-based injection.
    //
    // It followed the function here from `send_unified.rs`, where it read
    // `send_relay.rs`'s source; the subject moved, so the guard moved with it.
    // MUTATION EVIDENCE: restoring an `inject_via_provider` call reds the first
    // assert; removing the `session_id` reference reds the second.
    #[test]
    fn relay_unified_uses_session_uuid_not_display_name_as_injection_identity() {
        let src = include_str!("relay.rs");
        let fn_start = src
            .find("pub fn send_claude_relay(")
            .expect("send_claude_relay must exist in relay.rs");
        let after_start = &src[fn_start..];
        // Scope to the function body: the test module immediately follows.
        let fn_end = after_start
            .find("\n#[cfg(test)]")
            .expect("the test module must follow send_claude_relay");
        let body = &after_start[..fn_end];

        // Must NOT delegate to inject_via_provider (which would set id=display_name).
        assert!(
            !body.contains("inject_via_provider("),
            "send_claude_relay must NOT call inject_via_provider — it must \
             inline ProviderFx using id: &session.session_id (QS-2). Body:\n{body}"
        );
        // Must reference the resolved UUID as the injection identity.
        assert!(
            body.contains("session.session_id"),
            "send_claude_relay must reference session.session_id as the \
             injection identity (QS-2). Body:\n{body}"
        );
    }
}
