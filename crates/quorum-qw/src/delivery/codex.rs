//! The `codex_daemon` carrier — the codex app-server turn ladder.
//!
//! codex P2 W6 (codex-p2-spec section 7.5). A codex row is a daemon-hosted
//! protocol thread; delivering into it means:
//!
//!   1. resolve endpoint (the row's recorded registry `endpoint`, re-read by pid —
//!      it is NOT on the human/agent `Session`/`--json` surface, §9.4), thread id
//!      (the row's sessionId, m2), and the rollout path (the row's `jsonl_path`,
//!      else `CodexProvider::transcript_path` under `transcript_root`);
//!   2. connect a `WsAppServer` to the endpoint, `initialize` + `initialized`
//!      (readiness — the same handshake the create path drives);
//!   3. read the rollout tail → `open_turn_id` → BELIEVED state (Some(T) = believed
//!      BUSY, steer T; None = believed IDLE, start fresh);
//!   4. hand the connected rpc + believed turn id to `CodexProvider::inject`, which
//!      drives turn/start | turn/steer{+stale-fence fallback} (the envelopes are
//!      PROVIDER-INTERNAL — this carrier only speaks SEND).
//!
//! EVERY user-facing string here is SEND-vocabulary — NO `turn/start`,
//! `turn/steer`, or `expectedTurnId` ever appears (W2 enforces it in the rpc
//! layer; the carrier keeps it too).
//!
//! Nothing here prints and nothing here exits; see [`super`].

use crate::delivery::{
    append_send_invoked, emit_daemon_send_events, emit_door_failure, CarrierError, CarrierResult,
    Delivered, Notes, SendDeps, SendParams,
};

/// Why the codex carrier could not deliver. DELIBERATELY NO `Display` — see
/// [`CarrierError`]. Every variant is `qd <verb>:`-attributed.
#[derive(Debug)]
pub enum CodexSendError {
    /// No live pid, no recorded endpoint, a failed connect, or a failed
    /// `initialize` — one surface for every "there is no daemon to reach".
    NotReachable { name: String },
    /// A cold/dead row: no thread id to address a turn to. Distinct wording, so
    /// distinct variant.
    NoThreadId { name: String },
    /// The inject itself failed (protocol or precondition). `detail` is
    /// `InjectError`'s Display, which carries no start/steer tokens (the rpc-layer
    /// error text is W2-sanitized).
    InjectFailed { name: String, detail: String },
}

impl CarrierError for CodexSendError {
    fn line(&self, verb: &str) -> Option<String> {
        Some(match self {
            CodexSendError::NotReachable { name } => format!(
                "qd {verb}: \"{name}\": session daemon not reachable (try qd resume {name})"
            ),
            CodexSendError::NoThreadId { name } => {
                format!("qd {verb}: \"{name}\": this codex session has no thread id (cold/dead).")
            }
            CodexSendError::InjectFailed { name, detail } => {
                format!("qd {verb}: \"{name}\": send failed ({detail}).")
            }
        })
    }

    fn exit_code(&self) -> i32 {
        1
    }
}

/// Deliver `message` into a codex resident's open or fresh turn.
///
/// Answers the TURN ID this send minted, which [`emit_daemon_send_events`] writes
/// as `Payload::SendInitiated.send_id` — the ledger key `recovery_read` and the
/// terminal watch both join on. Every pre-inject refusal carries NO id, because
/// at that point no turn was minted.
pub fn send_codex(
    deps: &SendDeps<'_>,
    params: &SendParams<'_>,
) -> CarrierResult<CodexSendError> {
    use crate::provider::codex::{open_turn_id, read_lines, AppServerRpc, ClientInfo, WsAppServer};
    use crate::provider::{InjectError, Provider, ProviderFx, SessionKey};

    let session = params.session;
    let message = params.message;
    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());

    // §C1 (door-inventory B1) — record-then-fail-loud for the codex door's
    // "not reachable" refusals: emit a `send-failed` terminal (best-effort, keyed
    // to the TARGET session, `send_id` omitted pre-wire) BEFORE refusing, so no
    // codex-door failure is stderr-only. Distinct-wording refusals (no-thread-id,
    // inject error) emit inline. The emit never alters the refusal.
    let not_reachable = || {
        emit_door_failure(
            deps.env,
            deps.clock,
            &name,
            Some(session),
            message,
            "daemon-unreachable",
        );
        CodexSendError::NotReachable { name: name.clone() }
    };

    // The thread id (m2 — the REAL uuid the daemon assigned) is the row's sessionId.
    let thread_id = session.session_id.clone();
    if thread_id.is_empty() {
        // §C1: cold/dead session — no reachable daemon; record then refuse.
        emit_door_failure(
            deps.env,
            deps.clock,
            &name,
            Some(session),
            message,
            "daemon-unreachable",
        );
        return Err(CodexSendError::NoThreadId { name }.into());
    }

    // The endpoint is the registry row's recorded `endpoint` (NOT on the Session /
    // --json surface — re-read the row by pid). A dead/cold row (no live pid) has
    // no daemon to reach.
    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        return Err(not_reachable().into());
    };
    let endpoint = match crate::registry::read_entry(&deps.paths.sessions_dir, pid)
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty())
    {
        Some(ep) => ep,
        None => return Err(not_reachable().into()),
    };

    // Connect a fresh short-lived ws client → initialize handshake (readiness).
    let rpc = match WsAppServer::connect(&endpoint, std::time::Duration::from_secs(5)) {
        Ok(c) => c,
        Err(_e) => {
            // Daemon unreachable (connect failed): the same SEND-vocabulary surface
            // as a missing endpoint (§7.5).
            return Err(not_reachable().into());
        }
    };
    {
        let client = ClientInfo {
            name: "qd-manager".to_string(),
            title: None,
            version: "0".to_string(),
        };
        if rpc.initialize(&client).is_err() {
            return Err(not_reachable().into());
        }
        let _ = rpc.initialized();
    }

    // BELIEVED state from the rollout tail: the open turn id (Some ⇒ believed BUSY,
    // steer it; None ⇒ believed IDLE, start fresh). The tail is the durable truth;
    // unresolved/unreadable ⇒ None ⇒ believed IDLE (a fresh turn/start), which the
    // server's own state corrects (a start against a busy thread is the same
    // believed-idle→actually-busy case the stale-fence closes from the other side).
    let provider = crate::provider::codex::CodexProvider;
    let rollout_path = session
        .jsonl_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
        .or_else(|| {
            let key = SessionKey {
                id: &thread_id,
                name: session.name.as_deref(),
                cwd: session.cwd.as_deref(),
                pid: session.pid,
            };
            let fx = resolve_fx(deps);
            let root = provider.transcript_root(&fx);
            provider.transcript_path(&root, &key)
        });
    let expected_turn_id = rollout_path.and_then(|p| open_turn_id(&read_lines(&p)));

    // Build the fx: the connected rpc + the believed turn id (the relay_port
    // precedent — endpoint resolved by the caller's row read, an already-connected
    // rpc handed to inject; the trait never holds a transport handle / endpoint
    // string).
    let rpc_ref: &dyn AppServerRpc = &rpc;
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
        app_server: Some(rpc_ref),
        codex_expected_turn_id: expected_turn_id.as_deref(),
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    let key = SessionKey {
        id: &thread_id,
        name: session.name.as_deref(),
        cwd: session.cwd.as_deref(),
        pid: session.pid,
    };
    // B2 item 5: the same engine-asserted derivation as the relay path — one
    // attribution rule for the whole verb (declared extension; the codex
    // `from` rides the same channel-header namespace). punch R10: `inject`
    // SPENDS it — a codex turn is plain text, so the identity is rendered into
    // the message as the `<channel source="qd" …>` envelope
    // (`provider::shared::attribution`) unless it is `"cli"`. Nothing below
    // changes with it: `emit_daemon_send_events` keys sha256 of the RAW
    // `message`, and the turn id is the daemon's.
    let from = super::derive_from_session(deps.env);

    let result = provider.inject(&fx, &key, message, &from);
    // Best-effort close of our short-lived client (the daemon stays up).
    let _ = rpc.close();

    match result {
        Ok(turn_id) => {
            // C5/C3: sent + delivered (turn-accepted) into the TARGET's log on the
            // inject ACK; the success terminal lands later at the observe seam.
            emit_daemon_send_events(
                deps.env,
                deps.clock,
                &name,
                Some(session),
                message,
                &turn_id,
                &session.provider,
            );
            let mut notes = Notes::new();
            notes.extend(append_send_invoked(deps.env, deps.clock, &name));
            Ok(Delivered {
                stdout: Some(turn_id.clone()),
                message_id: turn_id,
                notes,
            })
        }
        Err(InjectError::NoTransport(_)) => {
            // Structurally unreachable (we set app_server Some) — defensive.
            Err(not_reachable().into())
        }
        Err(e) => {
            // A protocol/precondition failure. SEND-vocabulary only.
            // §C1: record the door failure before refusing.
            emit_door_failure(
                deps.env,
                deps.clock,
                &name,
                Some(session),
                message,
                "inject-failed",
            );
            Err(CodexSendError::InjectFailed {
                name,
                detail: e.to_string(),
            }
            .into())
        }
    }
}

/// A minimal `ProviderFx` for resolving the codex `transcript_root` off env only
/// (codex's `transcript_root` reads `fx.env` $CODEX_HOME/$HOME — never paths).
fn resolve_fx<'a>(deps: &SendDeps<'a>) -> crate::provider::ProviderFx<'a> {
    crate::provider::ProviderFx {
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
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    }
}
