//! REAL `qd send:relay` backend (spec §4.3).
//!
//! Source: `qa/hardening@3dd9f1e:src/commands/send.ts` send:relay action
//! (send.ts:391-475). Flow:
//!
//! 1. Fast path — `fast_relay_lookup` (session.ts:1277-1318): match a live pid
//!    entry by name (exact, then prefix, case-insensitive), then a relay by PID
//!    ancestry ≤5 levels.
//! 2. Fallback — full session scan → `session.relay_port` (send.ts:399-405).
//! 3. No port → `Session "<name>" has no relay.` exit 1 (send.ts:406-409).
//! 4. Send via the relay contract → print `message_id`, exit 0 (send.ts:411-435).
//! 5. `--wait` → long-poll `fetch_reply` with ≤3 retries on connection drop, 2s
//!    apart (send.ts:437-474). Wording parity throughout.

use std::time::Duration;

use clap::ArgMatches;

use dispatch::adoption::{Management, SessionAccess};
use dispatch::effects::{Env, RealClock, RealEnv};
use dispatch::exec::RealExec;
use dispatch::model::Session;
use dispatch::relay::{self, PidEntryRef, RelayContract, RelayError, RelayReply};
use dispatch::relay_http::CcRelay;

use super::acp_loss;
use super::common;

/// Default `--wait` timeout in seconds (send.ts:354, `"120"`).
const DEFAULT_TIMEOUT_S: u64 = 120;
/// `--wait` connection-drop retry budget (send.ts:441, `maxRetries = 3`).
const MAX_RETRIES: u32 = 3;
/// Inter-retry sleep (send.ts:469, `setTimeout(r, 2000)`).
const RETRY_SLEEP: Duration = Duration::from_secs(2);

pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    let message = m.get_one::<String>("message").expect("required by clap");
    let wait = m.get_flag("wait");
    // `--timeout <seconds>` default "120"; TS `parseInt(opts.timeout) * 1000`.
    let timeout_s = m
        .get_one::<String>("timeout")
        .and_then(|s| parse_timeout(s))
        .unwrap_or(DEFAULT_TIMEOUT_S);

    let client = CcRelay::new();
    run_with_client(query, message, wait, timeout_s, &client)
}

/// Unified-send entry into the existing Claude relay injection path. The
/// unified selector supplies the port captured from the already-resolved
/// registry row, so this function performs no fast lookup or second target
/// resolution. A failed POST is returned as a relay failure; this function has
/// no PTY fallback branch.
pub(super) fn run_claude_relay_unified(
    session: &Session,
    message: &str,
    relay_port: u16,
) -> i32 {
    use dispatch::provider::{InjectError, ProviderFx, SessionKey};

    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    let Some(provider_impl) = dispatch::provider::provider_for("claude-code") else {
        eprintln!(
            "qd send: unknown provider \"claude-code\" — relay delivery is unavailable."
        );
        return 1;
    };
    let client = CcRelay::new();
    let from_session = derive_from_session(&RealEnv);

    // Build ProviderFx inline (same as inject_via_provider) so we can supply the
    // resolved session UUID as SessionKey.id, keeping display name separate.
    // inject_via_provider uses name as both id and name (send:relay fast path has
    // no Session row); unified send always has the full resolved Session.
    let env = RealEnv;
    let home = env.var("HOME").filter(|s| !s.is_empty());
    let paths = dispatch::paths::QdPaths::from_home(std::path::Path::new(
        home.as_deref().unwrap_or("/"),
    ));
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: paths.sessions_dir.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: Some(&client),
        relay_port: Some(relay_port),
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    // QS-2 identity: resolved UUID pins the injection identity; name is display only.
    let key = SessionKey {
        id: &session.session_id,
        name: Some(&name),
        cwd: None,
        pid: None,
    };
    let message_id = match provider_impl.inject(&fx, &key, message, &from_session) {
        Ok(id) => id,
        Err(InjectError::RelayFailed(e)) => {
            eprintln!("Failed to send message: {}", send_err_text(&e));
            return 1;
        }
        Err(InjectError::NoTransport(_)) => {
            return no_relay_exit(&name, message, None);
        }
        Err(InjectError::Precondition(reason)) => {
            eprintln!("Failed to send message: {reason}");
            return 1;
        }
    };

    // Reuse the relay carrier's existing evidence and output semantics. A POST
    // acknowledgement is represented as queued/delivered, never as a reply.
    emit_relay_send_events(&name, Some(session), message, &message_id);
    println!("{message_id}");
    invoked_send_relay(&name);
    0
}

/// The verb body, parameterized on the relay client so tests inject a fake.
fn run_with_client(
    query: &str,
    message: &str,
    wait: bool,
    timeout_s: u64,
    client: &dyn RelayContract,
) -> i32 {
    // --- resolve the relay port (fast path, then full-scan fallback) ---
    let (relay_port, session_name, provider_id, session, target_uuid, access) =
        match resolve_relay_port(query) {
        Ok(v) => v,
        Err(code) => return code,
    };

    // Self-send guard: compare resolved stable session UUIDs.
    // If the caller's UUID equals the target's UUID, reject before any delivery activity.
    // If either UUID is unresolved, make no same-or-distinct claim (no fabricated guard result).
    let caller_uuid = caller_uuid_from_env(&RealEnv);
    if is_self_send(caller_uuid.as_deref(), target_uuid.as_deref()) {
        eprintln!(
            "qd send:relay: \"{session_name}\": target is this session — send rejected."
        );
        return 1;
    }

    // A live registry row + MCP sidecar does not prove the development channel
    // is loaded. Refuse before POST unless the process snapshot positively proved
    // receivability; a bare session must never silently accept/lose a message.
    if access.management == Management::Bare {
        return bare_destination_exit(&session_name, message, session.as_ref());
    }

    // codex P2 W6 (codex-p2-spec section 7.5): a codex row is a DAEMON-hosted
    // protocol thread, not a relay session — it carries no relay port. Route it to
    // the codex SEND ladder (connect → believed-state → turn/start|turn/steer)
    // BEFORE the relay-port-None check (which would otherwise print "has no
    // relay."). The `session` is the full-scan-fallback row (the fast path is
    // claude-only by construction, so a codex row always arrives here with a
    // Session). `--wait` is a RELAY-reply concept (a peer session replies through
    // the relay HTTP endpoint); a codex turn has no such reply channel, so the
    // codex SEND path ignores it (reported).
    if provider_id == "codex" {
        let Some(session) = session else {
            // Structurally unreachable: the fast path never yields "codex" (it
            // resolves claude relays only). Defensive — degrade to the no-relay
            // wording rather than panic. §C1: emit send-failed (session unknown →
            // byname-keyed) then fail loud.
            return no_relay_exit(&session_name, message, None);
        };
        return run_codex_send(&session, message);
    }

    // scoped-ACP-CC (residence SEND): an `acp/*` row is a daemon-hosted ACP thread with
    // no relay port — route it to the ACP SEND path (reconnect to the resident adapter →
    // AcpProvider::inject) BEFORE the relay-port-None check, exactly as the codex arm does.
    if provider_id.starts_with("acp/") {
        let Some(session) = session else {
            return no_relay_exit(&session_name, message, None);
        };
        return run_acp_send(&session, message);
    }

    // WS-A.5: a pi row's SEND is a live model TURN — reconnect to the resident pi-daemon's
    // ws front and drive `PiProvider::inject` (prompt{streamingBehavior:"steer"}: starts a
    // fresh turn when idle, steers the open turn when busy). Routed here BEFORE the
    // relay-port-None check (a pi row has a ws endpoint, no relay port), exactly as the
    // codex/acp SEND arms are. A5 flips the A2 graceful-deferred arm deferred→live.
    if provider_id == "pi" {
        let Some(session) = session else {
            return no_relay_exit(&session_name, message, None);
        };
        return run_pi_send(&session, message);
    }

    // No port on the resolved session → "has no relay." exit 1 (send.ts:406-409).
    //
    // codex P1 W5 (codex-p1-spec section 7.1): the NoTransport case stays driven by
    // THIS port-None check BEFORE inject — the wording ("has no relay.") is
    // byte-identical to today and the check is simpler here than re-routing it
    // through `InjectError::NoTransport` (provider.inject is only reached with a
    // resolved port, so its NoTransport arm is structurally unreachable on this
    // path; we map it defensively below to the same wording anyway). DECISION
    // REPORTED: NoTransport is NOT re-routed through InjectError (the existing
    // pre-inject check is byte-identical and simpler).
    let Some(port) = relay_port else {
        // §C1: the resolved full-scan `session` row (when present) keys the
        // send-failed log to the target uuid; else byname. Record-then-fail.
        return no_relay_exit(&session_name, message, session.as_ref());
    };

    // --- send the message THROUGH the provider seam (send.ts:411-435) --------
    // codex P1 W5: resolve the provider (claude-code on both paths today — the
    // fast path resolves only name+port and is claude-code BY CONSTRUCTION; the
    // full-scan fallback validated the row's provider via refuse_unknown_provider
    // before returning its id here). The defensive None arm re-prints the loud
    // unknown-provider shape rather than panicking (structurally unreachable).
    let Some(provider_impl) = dispatch::provider::provider_for(&provider_id) else {
        eprintln!(
            "qd send:relay: unknown provider \"{provider_id}\" — this engine supports: claude-code."
        );
        return 1;
    };
    let from_session = derive_from_session(&RealEnv);

    let message_id = match inject_via_provider(
        provider_impl,
        client,
        port,
        &session_name,
        message,
        &from_session,
    ) {
        Ok(id) => id,
        // The helper already printed the byte-identical stderr; carry its exit.
        Err(code) => return code,
    };

    // §X.3.1/§X.3.2 (3-phase delivery) — emit relay on-sent (`send-initiated`,
    // REUSING Payload::SendInitiated with relay values) + on-queued
    // (`relay-delivered`) into the TARGET's delivery log, on BOTH the no-wait and
    // `--wait` paths. The message has already left over the relay WIRE (the POST
    // returned message_id above); this is purely the LOCAL event log — the wire
    // is byte-untouched and the one-way invariant holds (dispatch knows nothing of
    // its consumers; a consumer adopts these lines by content_sha256 + send_id). Best-effort.
    emit_relay_send_events(&session_name, session.as_ref(), message, &message_id);

    if !wait {
        // Async: print the message_id and exit 0 (send.ts:437-440).
        println!("{message_id}");
        invoked_send_relay(&session_name);
        return 0;
    }

    // --- --wait long-poll (send.ts:442-474) ---
    let code = wait_for_reply(client, port, &message_id, timeout_s);
    if code == 0 {
        // A6 §4.1: an invoked line on the SUCCESSFUL (exit-0) --wait reply too.
        invoked_send_relay(&session_name);
    }
    code
}

/// §X (3-phase delivery) — relay on-sent + on-queued emission.
///
/// Writes `send-initiated` (the EXISTING `Payload::SendInitiated` constructed
/// with relay values, §X.3.1 — NOT a bare 2-field record) and `relay-delivered`
/// (§X.3.2, non-terminal) into the **TARGET's** delivery log, keyed to the target
/// uuid when the full-scan path resolved a `Session` row, else the
/// `byname-<target>` file (a consumer merges both, §1.4 G5). `send_id = message_id`;
/// `content_sha256 = sha256(raw caller message bytes)` (§X.4 — the SAME bytes
/// the consumer hashes into its own on-sent marker). The relay `send-initiated`
/// carries NO prose (`content_preview` omitted — a privacy improvement, §X.7).
///
/// BEST-EFFORT: a write failure (or an unresolvable HOME) is logged by
/// `warn_emit` and NEVER affects the send result — the message already left and
/// the relay WIRE is untouched.
fn emit_relay_send_events(
    target_name: &str,
    target_session: Option<&Session>,
    message: &str,
    message_id: &str,
) {
    // Production uses the real process env; the logic lives in the env-injected
    // inner fn so the Tier-2 seam test can drive the REAL emit against an isolated
    // tmp HOME with no process-env race (the I1 sender-side proof).
    emit_relay_send_events_with_env(&RealEnv, target_name, target_session, message, message_id);
}

fn emit_relay_send_events_with_env(
    env: &dyn Env,
    target_name: &str,
    target_session: Option<&Session>,
    message: &str,
    message_id: &str,
) {
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        // No HOME → cannot resolve the state dir; emission is best-effort.
        return;
    };
    let state_dir =
        dispatch::paths::QdPaths::from_home_env(std::path::Path::new(&home), env).state_dir;

    // Key to the TARGET (not the sender): full-scan → target uuid; fast path
    // (Session unknown) → the `byname-<target>` file (session omitted, §X.3.1).
    let writer = match target_session {
        Some(s) => dispatch::events::EventWriter::for_key(
            &state_dir,
            &s.session_id,
            Some(s.session_id.clone()),
            s.name.clone(),
        ),
        None => dispatch::events::EventWriter::for_key(
            &state_dir,
            &dispatch::events::byname_key(target_name),
            None,
            Some(target_name.to_string()),
        ),
    };

    let content_sha256 = dispatch::events::sha256_hex(message.as_bytes());
    let clock = RealClock;

    // on-sent — REUSE Payload::SendInitiated with the §X.3.1 relay values.
    dispatch::events::warn_emit(
        &writer,
        &clock,
        &dispatch::events::Payload::SendInitiated {
            send_id: message_id.to_string(),
            verb: "send:relay".to_string(),
            send_path: "relay".to_string(),
            content_sha256: content_sha256.clone(),
            content_len: message.as_bytes().len() as u64,
            chunks: 1,
            chunk_sha256s: vec![content_sha256.clone()],
            chunk_sha256s_capped: false,
            transcript: None,
            transcript_offset: None,
            content_preview: None,
        },
    );
    // on-queued — relay-delivered (§X.3.2), NON-terminal.
    dispatch::events::warn_emit(
        &writer,
        &clock,
        &dispatch::events::Payload::RelayDelivered {
            send_id: message_id.to_string(),
            content_sha256,
        },
    );
}

/// C5/C3 (3-phase delivery, DAEMON lanes) — emit the SENT + DELIVERED phases into
/// the TARGET's log on inject-SUCCESS, for the codex / `acp/*` / pi resident arms.
/// `send-initiated` (sent — the REUSED `Payload::SendInitiated` with daemon values:
/// verb `send:relay`, `send_path` the lane, `send_id` the resident-minted turn id)
/// + `turn-accepted` (delivered, NON-terminal — the resident took the prompt as a
/// turn). The success TERMINAL lands later at the OBSERVATION seam (`run_acp_wait`
/// StopReason-mapped; `run_pi_wait` content-keyed rollout observer), NEVER here —
/// so between this and observation the send reads as non-terminal DELIVERED =
/// PENDING (gate item 3), honest.
///
/// NO `transcript`/`transcript_offset` recovery keys are carried: a resident send
/// is OBSERVER-closed, and `qd delivery:recover`'s sweep is verb-gated to
/// {`send:pty`, `new-p`} (recover.rs) — so this `send-initiated` (verb `send:relay`)
/// is out of that sweep by construction and can never be mistaken for a
/// transcript-recoverable pty dangling. `content_sha256` = sha256(raw message),
/// the SAME key the observer/consumer matches on. BEST-EFFORT: a write failure is
/// logged by `warn_emit` and never affects the send result.
fn emit_daemon_send_events(
    target_name: &str,
    target_session: Option<&Session>,
    message: &str,
    turn_id: &str,
    send_path: &str,
) {
    emit_daemon_send_events_with_env(
        &RealEnv,
        target_name,
        target_session,
        message,
        turn_id,
        send_path,
    );
}

fn emit_daemon_send_events_with_env(
    env: &dyn Env,
    target_name: &str,
    target_session: Option<&Session>,
    message: &str,
    turn_id: &str,
    send_path: &str,
) {
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return;
    };
    let state_dir =
        dispatch::paths::QdPaths::from_home_env(std::path::Path::new(&home), env).state_dir;
    // Key to the TARGET, exactly as the relay + door emitters do.
    let writer = match target_session {
        Some(s) => dispatch::events::EventWriter::for_key(
            &state_dir,
            &s.session_id,
            Some(s.session_id.clone()),
            s.name.clone(),
        ),
        None => dispatch::events::EventWriter::for_key(
            &state_dir,
            &dispatch::events::byname_key(target_name),
            None,
            Some(target_name.to_string()),
        ),
    };
    let content_sha256 = dispatch::events::sha256_hex(message.as_bytes());
    let clock = RealClock;
    // sent — REUSE Payload::SendInitiated with daemon values (no recovery keys).
    dispatch::events::warn_emit(
        &writer,
        &clock,
        &dispatch::events::Payload::SendInitiated {
            send_id: turn_id.to_string(),
            verb: "send:relay".to_string(),
            send_path: send_path.to_string(),
            content_sha256: content_sha256.clone(),
            content_len: message.as_bytes().len() as u64,
            chunks: 1,
            chunk_sha256s: vec![content_sha256.clone()],
            chunk_sha256s_capped: false,
            transcript: None,
            transcript_offset: None,
            content_preview: None,
        },
    );
    // delivered — turn-accepted (NON-terminal): the resident accepted the turn.
    dispatch::events::warn_emit(
        &writer,
        &clock,
        &dispatch::events::Payload::TurnAccepted {
            send_id: turn_id.to_string(),
            content_sha256,
        },
    );
}

/// C5/C3 — emit the daemon-lane success TERMINAL `message-seen{send_id,
/// content_sha256}` (the FLOOR / record-presence reading) into the TARGET's log,
/// best-effort. Used by the pi structured FLOOR (`run_pi_floor_send`, the dead-only
/// sub-lane) once the sent bytes are confirmed present in the appended session
/// record (content-keyed). The RESIDENT lanes emit their terminal at the
/// wait/observe seam instead (never here). A reader recovers the floor-vs-strong
/// reading from the paired send-initiated's send_path + D4's table.
fn emit_daemon_seen(
    target_name: &str,
    target_session: Option<&Session>,
    send_id: &str,
    content_sha256: &str,
) {
    emit_daemon_seen_with_env(&RealEnv, target_name, target_session, send_id, content_sha256);
}

fn emit_daemon_seen_with_env(
    env: &dyn Env,
    target_name: &str,
    target_session: Option<&Session>,
    send_id: &str,
    content_sha256: &str,
) {
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return;
    };
    let state_dir =
        dispatch::paths::QdPaths::from_home_env(std::path::Path::new(&home), env).state_dir;
    let writer = match target_session {
        Some(s) => dispatch::events::EventWriter::for_key(
            &state_dir,
            &s.session_id,
            Some(s.session_id.clone()),
            s.name.clone(),
        ),
        None => dispatch::events::EventWriter::for_key(
            &state_dir,
            &dispatch::events::byname_key(target_name),
            None,
            Some(target_name.to_string()),
        ),
    };
    dispatch::events::warn_emit(
        &writer,
        &RealClock,
        &dispatch::events::Payload::MessageSeen {
            send_id: send_id.to_string(),
            content_sha256: content_sha256.to_string(),
        },
    );
}

/// Concatenate every `*.jsonl` in the pi floor's dedicated `--session-dir` (pi
/// writes ONE appended session file there under `-c`). The floor content-keys THIS
/// (the appended record) for its success terminal — not the stdout, which need not
/// carry the user prompt record.
fn floor_session_jsonl(session_dir: &std::path::Path) -> String {
    let mut out = String::new();
    let Ok(entries) = std::fs::read_dir(session_dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            if let Ok(s) = std::fs::read_to_string(&p) {
                out.push_str(&s);
                out.push('\n');
            }
        }
    }
    out
}

/// B2 item 5 — derive the `from_session` channel-header identity. Ratified
/// precedence (Q3; the from_session NAMESPACE is the claude session uuid —
/// reply routing keys on it, so step 1 RESOLVES to a uuid, never emits the
/// stable id itself):
///
///   1. ENGINE-ASSERTED: `QD_SESSION_ID` — the engine birth property,
///      explicitly set at every launch (override-never-inherit, the D1 site-4
///      lesson) — resolved through the idstore to the claude uuid.
///   2. `CLAUDE_CODE_SESSION_ID` — only when NO engine identity resolves
///      (bare-shell operator sends from inside a claude session; also the
///      pre-fix inherited-env channel, now demoted so a leaked env var from a
///      different session can no longer mis-attribute an engine session's
///      sends — the punch_b2_item5_repro pin).
///   3. `"cli"` — bare operator shell.
///
/// An QD_SESSION_ID that is malformed, unknown to the store, or still UNBOUND
/// (mint without a session uuid yet) falls through to (2) — the derivation
/// never invents an identity. Cost: one `ids.jsonl` read per send (accepted
/// at the phase-2 checkpoint; `whoami` pays the same read).
fn derive_from_session(env: &dyn Env) -> String {
    if let Some(stable) = env.var("QD_SESSION_ID").filter(|s| !s.is_empty()) {
        if let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) {
            let paths = dispatch::paths::QdPaths::from_home_env(std::path::Path::new(&home), env);
            let ids = dispatch::idstore::fold(&dispatch::idstore::ids_path(&paths.state_dir));
            // The SHARED resolution chain (S4): whoami and attribution answer
            // "what engine identity resolves?" identically by construction.
            if let Some(uuid) = dispatch::idstore::resolve_to_uuid(&ids, &stable) {
                return uuid;
            }
        }
    }
    env.var("CLAUDE_CODE_SESSION_ID")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cli".to_string())
}

/// Resolve the caller's session UUID via the QD identity chain: `QD_SESSION_ID`
/// → idstore → UUID. Returns `None` if absent, malformed, or unmapped.
/// This is the SAME resolution chain as `qd whoami`'s env path, per the
/// spec requirement (S4: resolved-identity comparison uses the shared chain).
fn caller_uuid_from_env(env: &dyn Env) -> Option<String> {
    let stable = env.var("QD_SESSION_ID").filter(|s| !s.is_empty())?;
    let home = env.var("HOME").filter(|s| !s.is_empty())?;
    let paths = dispatch::paths::QdPaths::from_home_env(std::path::Path::new(&home), env);
    let ids = dispatch::idstore::fold(&dispatch::idstore::ids_path(&paths.state_dir));
    dispatch::idstore::resolve_to_uuid(&ids, &stable)
}

fn is_self_send(caller_uuid: Option<&str>, target_uuid: Option<&str>) -> bool {
    matches!((caller_uuid, target_uuid), (Some(caller), Some(target)) if caller == target)
}

fn fallback_target_uuid(session: &Session) -> Option<String> {
    if session.session_id.is_empty() {
        None
    } else {
        Some(session.session_id.clone())
    }
}

/// Append a content-free A6 invoked line for a SUCCESSFUL `send:relay`. The fast
/// path yields only a NAME (no sessionId), so we key the invoked line by name —
/// the fold accepts either. Best-effort: a failure warns but NEVER changes the
/// verb's exit code (spec §4.1).
fn invoked_send_relay(name: &str) {
    use dispatch::effects::RealClock;
    if let Err(e) =
        dispatch::telemetry::append_invoked(&RealEnv, &RealClock, "send", None, Some(name))
    {
        eprintln!("WARNING: telemetry invoked append failed (non-fatal): {e}");
    }
}

/// codex P1 W5 (codex-p1-spec section 7.1): send `message` THROUGH the resolved
/// provider's `inject`, mapping the typed `InjectError` back to the EXACT current
/// send:relay surface. Returns the message id on success (the caller prints it),
/// or `Err(exit)` after printing the byte-identical stderr.
///
/// Result mapping (each leg byte-identical to the pre-rewire `client.send_message`
/// match, send.ts:411-435):
///   - `Ok(id)`                       → `Ok(id)` (caller prints `message_id`).
///   - `Err(RelayFailed(e))`          → `Failed to send message: <send_err_text(e)>`
///     exit 1 — the SAME wording/exit the old `Err(e)` arm produced, recovered
///     from the structured `RelayError` the trait preserved (W2's
///     `claude_inject_preserves_relay_error_class`).
///   - `Err(NoTransport(_))`          → the "has no relay." wording (defensive; the
///     pre-inject port-None check in the caller already owns this case, so this arm
///     is structurally unreachable — provider.inject is called with a resolved
///     port). Kept byte-identical so a future caller change cannot silently
///     diverge.
///   - `Err(Precondition(_))`         → unreachable for claude (daemon-only steer);
///     mapped to the send-failure surface for total-match safety.
fn inject_via_provider(
    provider: &dyn dispatch::provider::Provider,
    client: &dyn RelayContract,
    port: u16,
    session_name: &str,
    message: &str,
    from: &str,
) -> Result<String, i32> {
    use dispatch::provider::{InjectError, ProviderFx, SessionKey};
    // A minimal ProviderFx: the claude inject body consumes ONLY relay + relay_port
    // (it sends through the CONTRACT given the port, never holding a transport
    // handle in the trait signature — the banned claude-shaped contortion). The
    // env/paths are required struct members but UNREAD by inject. We derive paths
    // from HOME-or-"/" (never erroring): the pre-rewire `client.send_message` send
    // seam read NO HOME, so introducing a HOME-unset failure here would be a
    // behavior change — the fallback keeps the send seam HOME-independent exactly
    // as before.
    let env = RealEnv;
    let home = env.var("HOME").filter(|s| !s.is_empty());
    let paths =
        dispatch::paths::QdPaths::from_home(std::path::Path::new(home.as_deref().unwrap_or("/")));
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: paths.sessions_dir.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: Some(client),
        relay_port: Some(port),
        // codex-only transport; the claude send:relay path uses relay+port.
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    // The send:relay verb keys on NAME+port (the fast path never builds a Session);
    // SessionKey carries the name as the id here (claude inject reads neither for
    // the send — it sends through relay+port). pid is None (not on this surface).
    let key = SessionKey {
        id: session_name,
        name: Some(session_name),
        cwd: None,
        pid: None,
    };
    match provider.inject(&fx, &key, message, from) {
        Ok(id) => Ok(id),
        Err(InjectError::RelayFailed(e)) => {
            // BYTE-IDENTICAL to the pre-rewire `Err(e)` arm (send.ts:432-434).
            eprintln!("Failed to send message: {}", send_err_text(&e));
            Err(1)
        }
        Err(InjectError::NoTransport(_)) => {
            // Defensive (unreachable: port is Some here). Same wording as the
            // pre-inject port-None check (send.ts:406-409). §C1: the door emits
            // send-failed (byname-keyed — no Session row in scope here) via
            // no_relay_exit before failing loud, like every relay-door exit.
            Err(no_relay_exit(session_name, message, None))
        }
        Err(InjectError::Precondition(reason)) => {
            // Unreachable for claude (daemon-only). Total-match safety.
            eprintln!("Failed to send message: {reason}");
            Err(1)
        }
    }
}

/// codex P2 W6 (codex-p2-spec section 7.5) — the codex SEND ladder at the verb
/// layer. A codex row is a daemon-hosted protocol thread; `qd send:relay` for it:
///
///   1. resolve endpoint (the row's recorded registry `endpoint`, re-read by pid —
///      it is NOT on the human/agent `Session`/`--json` surface, §9.4), thread id
///      (the row's sessionId, m2), and the rollout path (the row's `jsonl_path`,
///      else `CodexProvider::transcript_path` under `transcript_root`);
///   2. connect a `WsAppServer` to the endpoint, `initialize` + `initialized`
///      (readiness — the same handshake the create path drives);
///   3. read the rollout tail → `open_turn_id` → BELIEVED state (Some(T) = believed
///      BUSY, steer T; None = believed IDLE, start fresh);
///   4. hand the connected rpc + believed turn id to `CodexProvider::inject`, which
///      drives turn/start | turn/steer{+stale-fence fallback} (the envelopes are
///      PROVIDER-INTERNAL — this verb only speaks SEND).
///
/// Returns the process exit code. On success the turn id is printed (the async
/// `send:relay` prints the message id; the codex analog prints the turn id) and a
/// invoked line is appended. EVERY user-facing string here is SEND-vocabulary — NO
/// `turn/start`, `turn/steer`, or `expectedTurnId` ever appears (W2 enforces it in
/// the rpc layer; the verb keeps it too).
pub(super) fn run_codex_send(session: &Session, message: &str) -> i32 {
    use dispatch::provider::codex::{
        open_turn_id, read_lines, AppServerRpc, ClientInfo, WsAppServer,
    };
    use dispatch::provider::{InjectError, Provider, ProviderFx, SessionKey};

    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());

    // §C1 (door-inventory B1) — record-then-fail-loud for the codex door's
    // "not reachable" exits: emit a `send-failed` terminal (best-effort, keyed to
    // the TARGET session, `send_id` omitted pre-wire) BEFORE the loud exit, so no
    // codex-door failure is stderr-only. Distinct-wording exits (no-thread-id,
    // inject error) emit inline. The emit never alters the exit path.
    let not_reachable = || {
        emit_door_failure(&name, Some(session), message, "daemon-unreachable");
        eprintln!(
            "qd send:relay: \"{name}\": session daemon not reachable (try qd resume {name})"
        );
        1
    };

    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // The thread id (m2 — the REAL uuid the daemon assigned) is the row's sessionId.
    let thread_id = session.session_id.clone();
    if thread_id.is_empty() {
        // §C1: cold/dead session — no reachable daemon; record then fail loud.
        emit_door_failure(&name, Some(session), message, "daemon-unreachable");
        eprintln!("qd send:relay: \"{name}\": this codex session has no thread id (cold/dead).");
        return 1;
    }

    // The endpoint is the registry row's recorded `endpoint` (NOT on the Session /
    // --json surface — re-read the row by pid). A dead/cold row (no live pid) has
    // no daemon to reach.
    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        return not_reachable();
    };
    let endpoint = match dispatch::registry::read_entry(&paths.sessions_dir, pid)
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty())
    {
        Some(ep) => ep,
        None => {
            return not_reachable();
        }
    };

    // Connect a fresh short-lived ws client → initialize handshake (readiness).
    let rpc = match WsAppServer::connect(&endpoint, std::time::Duration::from_secs(5)) {
        Ok(c) => c,
        Err(_e) => {
            // Daemon unreachable (connect failed): the same SEND-vocabulary surface
            // as a missing endpoint (§7.5).
            return not_reachable();
        }
    };
    {
        let client = ClientInfo {
            name: "qd-manager".to_string(),
            title: None,
            version: "0".to_string(),
        };
        if rpc.initialize(&client).is_err() {
            return not_reachable();
        }
        let _ = rpc.initialized();
    }

    // BELIEVED state from the rollout tail: the open turn id (Some ⇒ believed BUSY,
    // steer it; None ⇒ believed IDLE, start fresh). The tail is the durable truth;
    // unresolved/unreadable ⇒ None ⇒ believed IDLE (a fresh turn/start), which the
    // server's own state corrects (a start against a busy thread is the same
    // believed-idle→actually-busy case the stale-fence closes from the other side).
    let provider = dispatch::provider::codex::CodexProvider;
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
            let fx = codex_resolve_fx(&env, &paths);
            let root = provider.transcript_root(&fx);
            provider.transcript_path(&root, &key)
        });
    let expected_turn_id = rollout_path.and_then(|p| open_turn_id(&read_lines(&p)));

    // Build the fx: the connected rpc + the believed turn id (the relay_port
    // precedent — endpoint resolved at the verb, an already-connected rpc handed
    // to inject; the trait never holds a transport handle / endpoint string).
    let rpc_ref: &dyn AppServerRpc = &rpc;
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: paths.sessions_dir.clone(),
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
    // `from` rides the same channel-header namespace).
    let from = derive_from_session(&RealEnv);

    let result = provider.inject(&fx, &key, message, &from);
    // Best-effort close of our short-lived client (the daemon stays up).
    let _ = rpc.close();

    match result {
        Ok(turn_id) => {
            // C5/C3: sent + delivered (turn-accepted) into the TARGET's log on the
            // inject ACK; the success terminal lands later at the observe seam.
            emit_daemon_send_events(&name, Some(session), message, &turn_id, &session.provider);
            // The async-send analog: print the id (turn id here), append invoked.
            println!("{turn_id}");
            invoked_send_relay(&name);
            0
        }
        Err(InjectError::NoTransport(_)) => {
            // Structurally unreachable (we set app_server Some) — defensive.
            not_reachable()
        }
        Err(e) => {
            // A protocol/precondition failure. SEND-vocabulary only (InjectError's
            // Display carries no start/steer tokens; the rpc-layer error text is
            // W2-sanitized). §C1: record the door failure before the loud exit.
            emit_door_failure(&name, Some(session), message, "inject-failed");
            eprintln!("qd send:relay: \"{name}\": send failed ({e}).");
            1
        }
    }
}

/// scoped-ACP-CC residence SEND path (S7). The ACP analog of [`run_codex_send`]:
/// re-read the row's recorded `endpoint`, verify the resident adapter's IDENTITY
/// (pid alive AND the live `/proc` cmdline carries our `acp-daemon --listen
/// <endpoint>` — S6, defeats PID reuse), derive the tier from `(provider,
/// transport-field, endpoint-alive)`.
///
/// **Transport-loss disposition (Child D, opencode D1 — clerk-4's Arm-B
/// ratification, bond note 01KX01BY7G): `acp/claude-code` is a NAMED DIVERGENCE.
/// On ANY transport loss this verb REFUSES and surfaces (the same
/// "not reachable (try qd resume …)" class as `codex`/`acp/opencode`, exit 1) —
/// with the session's identity first preserved in the qd-owned tombstone store
/// ([`dispatch::tombstone`]; `acp_loss::preserve_identity`). There is NO
/// auto-deliver path: Child B's degrade+latch+companion-drive machinery was
/// REMOVED (not gated), so unreachability is structural.** pi's auto-deliver
/// floor (`provider/pi/floor.rs`, [`run_pi_send`]) is the deliberately separate,
/// untouched realization of D1's graceful degrade.
///
/// Three refusal-relevant lanes:
///   - **entry-lane dead** (no live pid, dead endpoint, or a historical
///     `transport=="pty"` latch — anything but a healthy structured tier) →
///     preserve identity + refuse. Pre-send vs post-send history no longer
///     changes the disposition: both refuse (post-send always refused; pre-send
///     joined it under Arm B).
///   - **`AcpConnection::connect` fails** → RE-PROBE liveness+identity, then
///     split (the round-1 TOCTOU fix; ladder's `classify_connect_failure`):
///     still alive + verified ⇒ a genuine wedge, refuse with NO tombstone
///     (live-pid row is not janitor-reapable); now dead / identity gone ⇒ the
///     daemon died across the probe→connect boundary ⇒ preserve identity +
///     refuse. See the `Err(_)` arm below and ladder.rs clause 3 (including
///     the stated wedge-dies-later residual).
///   - **mid-flight `NoTransport`** (a live connect, then the `session/prompt`
///     dispatch itself fails) → preserve identity + refuse. The
///     `acp_pre_dispatch` hook may have JUST durably marked
///     `structured_send_issued=true` before the failure (the exactly-once
///     dispatch-timing guard, kept intact) — that record is wire-history truth,
///     it just no longer selects a different disposition.
///
/// `QD_ACP_PTY_FLOOR_DISABLE` is now a NO-OP: it gated only the retired floor
/// drive, and refusal is the unconditional behavior it used to select.
///
/// **Scope of the tombstone: `acp/claude-code` only.** `acp_loss::preserve_identity`
/// self-gates on the provider, so `acp/opencode` (which also routes through this
/// fn) keeps its byte-identical plain refusal — no store write, no extra output.
pub(super) fn run_acp_send(session: &Session, message: &str) -> i32 {
    use dispatch::provider::acp::{
        classify_connect_failure, derive_tier, AcpClient, AcpConnection, ConnectFailure, Tier,
        ACP_CC_PROVIDER,
    };
    use dispatch::provider::{InjectError, Provider, ProviderFx, SessionKey};

    let name = session.name.clone().unwrap_or_default();
    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    // §C1 (door-inventory B2) — record-then-fail-loud: every acp-door "not
    // reachable" exit leaves a `send-failed` account (best-effort, keyed to the
    // TARGET session, `send_id` omitted pre-wire) BEFORE the loud exit, so no
    // acp-door failure is stderr-only. `reason` names the surface. Serves BOTH
    // acp/claude-code AND acp/opencode (both route through this arm). The emit
    // never alters the exit path; the identity-preservation tombstone is unchanged.
    let not_reachable = |reason: &str| {
        emit_door_failure(&name, Some(session), message, reason);
        eprintln!(
            "qd send:relay: \"{name}\": acp session daemon not reachable (try qd resume {name})"
        );
        1
    };

    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        // A row with no live pid is already-lost transport (the janitor may
        // have reaped the registry row entirely) — preserve identity, refuse.
        acp_loss::preserve_identity(session, "acp session has no live daemon pid at send entry");
        return not_reachable("daemon-unreachable");
    };
    // The endpoint + degradation latch live in the row (re-read by pid; NOT on --json).
    let entry = dispatch::registry::read_entry(&paths.sessions_dir, pid);
    let endpoint = entry
        .as_ref()
        .and_then(|e| e.endpoint.clone())
        .filter(|s| !s.is_empty());
    let transport_field = entry.as_ref().and_then(|e| e.transport.clone());

    // S6 identity + liveness: a connect-success is liveness, NOT identity — the cmdline
    // (+ pid liveness) is the identity fence against PID reuse.
    let cmdline = dispatch::create_daemon::real_cmdline_probe(pid);
    let endpoint_alive = endpoint.is_some()
        && dispatch::effects::is_pid_alive(pid as i32)
        && dispatch::acp_residence::cmdline_is_our_acp_daemon(cmdline.as_deref(), endpoint.as_deref());

    let tier = derive_tier("acp/claude-code", transport_field.as_deref(), endpoint_alive);

    if tier != Tier::Acp {
        // Entry-lane transport loss (dead endpoint, or a historical pty latch):
        // the named-divergence disposition — preserve identity (qd-owned store,
        // acp/claude-code only; a no-op for acp/opencode, whose refusal stays
        // byte-identical), then refuse to the human-recovery surface. Whether a
        // structured send was ever issued no longer branches here: pre-send and
        // post-send loss BOTH refuse (Arm B — the Child-B pre-send auto-deliver
        // was removed, not gated).
        acp_loss::preserve_identity(session, "acp endpoint not reachable at send entry");
        return not_reachable("daemon-unreachable");
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
            let pid_alive_now = dispatch::effects::is_pid_alive(pid as i32);
            let cmdline_is_ours_now = dispatch::acp_residence::cmdline_is_our_acp_daemon(
                dispatch::create_daemon::real_cmdline_probe(pid).as_deref(),
                Some(endpoint.as_str()),
            );
            if classify_connect_failure(pid_alive_now, cmdline_is_ours_now)
                == ConnectFailure::TransportLost
            {
                acp_loss::preserve_identity(
                    session,
                    "acp daemon died across the connect boundary (was live at the pre-connect probe)",
                );
            }
            return not_reachable("daemon-unreachable");
        }
    };
    let conn_ref: &dyn AcpClient = &conn;
    // Child B exactly-once guard: durably mark `structured_send_issued=true` the
    // MOMENT this turn's bytes are confirmed on the wire (see
    // `AcpConnection::prompt`) — before we know whether the reply ever arrives.
    // Never gated on `inject`'s `Ok` return: a crash or socket drop right after
    // dispatch must still leave this true for the NEXT process to read.
    let sessions_dir = paths.sessions_dir.clone();
    let mark_dispatched = || {
        if let Some(mut e) = dispatch::registry::read_entry(&sessions_dir, pid) {
            if e.structured_send_issued != Some(true) {
                e.structured_send_issued = Some(true);
                if let Err(err) = dispatch::registry::write_entry(&sessions_dir, &e) {
                    eprintln!(
                        "qd send:relay: could not persist the structured-send marker: {err}"
                    );
                }
            }
        }
    };
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: paths.sessions_dir.clone(),
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
    let key = SessionKey {
        id: &session.session_id,
        name: session.name.as_deref(),
        cwd: session.cwd.as_deref(),
        pid: session.pid,
    };
    let from = derive_from_session(&RealEnv);
    match ACP_CC_PROVIDER.inject(&fx, &key, message, &from) {
        Ok(turn_id) => {
            // C5/C3: sent + delivered (turn-accepted) into the TARGET's log on the
            // inject ACK; the StopReason-mapped terminal lands later in run_acp_wait.
            emit_daemon_send_events(&name, Some(session), message, &turn_id, &session.provider);
            println!("{turn_id}");
            invoked_send_relay(&name);
            0
        }
        Err(InjectError::Precondition(s)) => {
            // §C1: a refused/precondition inject (queue full, etc.) — record the
            // door failure before the loud exit.
            emit_door_failure(&name, Some(session), message, "inject-failed");
            eprintln!("qd send:relay: \"{name}\": send failed ({s}).");
            1
        }
        Err(err @ InjectError::NoTransport(_)) => {
            // Mid-flight transport loss (a live connect, then the dispatch
            // itself failed): the same named-divergence refusal as the
            // entry-lane arm. `mark_dispatched` may have JUST durably recorded
            // `structured_send_issued=true` (bytes confirmed on the wire before
            // the failure — the exactly-once guard, kept) — wire-history truth,
            // but no longer a disposition branch: pre-send and post-send loss
            // both refuse (Arm B).
            acp_loss::preserve_identity(
                session,
                &format!("acp transport lost mid-flight ({err})"),
            );
            not_reachable("transport-lost")
        }
        Err(_) => not_reachable("daemon-unreachable"),
    }
}

/// WS-A.5 pi residence SEND path. The pi analog of [`run_acp_send`]: re-read the row's
/// recorded `endpoint`, verify the resident pi-daemon's IDENTITY (pid alive AND the live
/// `/proc` cmdline carries our `pi-daemon --listen <endpoint>` marker — defeats PID reuse:
/// a connect-success is liveness, the cmdline + recorded endpoint is identity), connect a
/// fresh short-lived [`PiRemote`] to the resident ws front, and drive `PiProvider::inject`.
///
/// `inject` mints a live turn via `prompt{streamingBehavior:"steer"}` — a single call that
/// starts a fresh turn when the resident is idle and steers the open turn when busy (no
/// per-turn believed-state read; contrast the codex `expectedTurnId` ladder). On success the
/// minted turn id is printed (the async-send analog — the codex/acp arms print the turn id
/// too). Events do NOT return through this client (`PiRemote::next_event` is `Ok(None)` by
/// design — the resident routes pi's stream into the registry sink); a caller wanting the
/// turn OUTCOME reads the registry/transcript, not the send reply. A dead/wrong-identity
/// endpoint or a failed connect DEGRADES to the SEND "not reachable" surface — never a hang
/// on a dead endpoint, never a fake.
pub(super) fn run_pi_send(session: &Session, message: &str) -> i32 {
    use dispatch::provider::pi::residence::cmdline_is_our_pi_daemon;
    use dispatch::provider::pi::{PiProvider, PiRemote, PiRpc};
    use dispatch::provider::{InjectError, Provider, ProviderFx, SessionKey};

    let name = session.name.clone().unwrap_or_default();
    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    // §C1 (door-inventory B3, resident door) — record-then-fail-loud: every
    // pi-door "not reachable" exit leaves a `send-failed` account (best-effort,
    // keyed to the target session, `send_id` omitted pre-wire) BEFORE the loud
    // exit. `reason` names the surface; the emit never alters the exit path.
    let not_reachable = |reason: &str| {
        emit_door_failure(&name, Some(session), message, reason);
        eprintln!(
            "qd send:relay: \"{name}\": pi session daemon not reachable (try qd resume {name})"
        );
        1
    };

    // Identity + liveness fence (residence S6): a connect-success is liveness only — the
    // live cmdline carrying OUR `pi-daemon --listen <endpoint>` marker (+ pid alive + a
    // recorded endpoint) is the identity guard against PID reuse.
    let pid = session.pid.filter(|&p| p != 0);
    // The endpoint lives in the registry row (re-read by pid; NOT on the --json surface).
    let endpoint = pid
        .and_then(|pid| dispatch::registry::read_entry(&paths.sessions_dir, pid))
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty());
    let (pid_alive, cmdline_is_ours) = match pid {
        Some(pid) => {
            let cmdline = dispatch::create_daemon::real_cmdline_probe(pid);
            (
                dispatch::effects::is_pid_alive(pid as i32),
                cmdline_is_our_pi_daemon(cmdline.as_deref(), endpoint.as_deref()),
            )
        }
        None => (false, false),
    };

    // A6.1 DEAD-ONLY floor (super-22 acceptance condition 8): when the native rpc
    // resident is PROVABLY DEAD/GONE (no pid / pid dead / wrong-identity cmdline /
    // missing endpoint — the S6 "not reachable" branch) DROP to the structured
    // `-p --mode json` floor instead of erroring. A LIVE identity-verified resident
    // NEVER floors — even the failed/slow ws connect below stays "not reachable" (the
    // rpc retry/steer surface), because a one-shot `-c --session-dir` concurrent with
    // the resident's OPEN session JSONL would race/corrupt it (single-writer safety).
    // dead ⇒ floor + reuse-dir; alive ⇒ never floor.
    if dispatch::provider::pi::floor::floor_may_fire(
        pid.is_some(),
        pid_alive,
        cmdline_is_ours,
        endpoint.is_some(),
    ) {
        return run_pi_floor_send(session, message, &env, &paths);
    }
    let endpoint = endpoint.expect("live identity-verified resident implies a recorded endpoint");

    // Connect a fresh short-lived remote to the resident ws front (the AcpConnection
    // fail-fast discipline — a contended connect fails fast rather than hanging).
    let remote = match PiRemote::connect(&endpoint, Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => return not_reachable("daemon-unreachable"),
    };
    let rpc_ref: &dyn PiRpc = &remote;
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: paths.sessions_dir.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: Some(rpc_ref),
            acp_pre_dispatch: None,
    };
    let key = SessionKey {
        id: &session.session_id,
        name: session.name.as_deref(),
        cwd: session.cwd.as_deref(),
        pid: session.pid,
    };
    let from = derive_from_session(&RealEnv);
    let result = PiProvider.inject(&fx, &key, message, &from);
    // Best-effort close of our short-lived client (the resident daemon stays up).
    let _ = remote.close();
    match result {
        Ok(turn_id) => {
            // C5/C3: sent + delivered (turn-accepted) into the TARGET's log on the
            // inject ACK; the content-keyed rollout terminal lands later at turn
            // close (run_pi_wait observer). PENDING in the meantime — honest.
            emit_daemon_send_events(&name, Some(session), message, &turn_id, &session.provider);
            // The async-send analog: print the minted turn id, append invoked.
            println!("{turn_id}");
            invoked_send_relay(&name);
            0
        }
        Err(InjectError::Precondition(s)) => {
            // A refused/timed-out prompt (PA9: the sink stays idle) — SEND-vocabulary only.
            // §C1: record the door failure before the loud exit.
            emit_door_failure(&name, Some(session), message, "inject-failed");
            eprintln!("qd send:relay: \"{name}\": send failed ({s}).");
            1
        }
        Err(_) => not_reachable("daemon-unreachable"),
    }
}

/// A6.1 pi structured-floor SEND — the DEAD-ONLY fallback lane. Reached from
/// [`run_pi_send`] ONLY when the native rpc resident is provably dead (the S6
/// identity+liveness fence failed; [`dispatch::provider::pi::floor::floor_may_fire`]).
/// Delivers the turn via a ONE-SHOT `pi -p --mode json -c --session-dir <dedicated>`
/// child and captures the outcome SCRAPE-FREE from the `turn_end`/`agent_end` ndjson
/// (see [`dispatch::provider::pi::floor`]). Continuity = `-c` + one dedicated
/// per-qd-session `--session-dir` (turn-2 resumes turn-1's single appended session
/// file). The drop is OBSERVABLE (a stderr drop-log line — a degradation is never
/// silent; pi's own floor doctrine, `provider/pi/floor.rs`). pi's OWN settings cred (openai-codex, `~/.pi/agent`) is inherited —
/// NEVER a `--provider`/credential override or swap.
fn run_pi_floor_send(
    session: &Session,
    message: &str,
    env: &RealEnv,
    paths: &dispatch::paths::QdPaths,
) -> i32 {
    use dispatch::provider::pi::{floor, PiProvider};
    use dispatch::provider::Provider;

    let name = session.name.clone().unwrap_or_default();

    // pi binary: the QD_PI_BIN override (pi is NOT on PATH in quorum boxes) else "pi".
    let pi_bin = env
        .var("QD_PI_BIN")
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "pi".to_string());

    // pi's sessions root off env ONLY (PI_CODING_AGENT_SESSION_DIR else
    // $HOME/.pi/agent/sessions) — reuse the provider's public resolver via a minimal
    // env-only fx (transcript_root reads fx.env, never paths).
    let fx = pi_resolve_fx(env, paths);
    let sessions_root = PiProvider.transcript_root(&fx);
    // One dedicated floor dir per qd-session, isolated from the rpc resident's
    // sessions so `-c`'s most-recent pick is unambiguous + turn-2 resumes turn-1.
    let session_dir = floor::floor_session_dir(&sessions_root, &session.session_id);
    if let Err(e) = std::fs::create_dir_all(&session_dir) {
        // §C1 (door-inventory B4, dead-only floor door) — record then fail loud.
        emit_door_failure(&name, Some(session), message, "floor-setup-failed");
        eprintln!("qd send:relay: \"{name}\": pi floor could not create session dir: {e}");
        return 1;
    }
    let continue_session = floor::dir_has_session(&session_dir);
    let cwd = session
        .cwd
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });

    // Observable degrade (never silent — pi's floor doctrine; the Child-B ACP
    // ladder's analogous drop-log was retired with that floor in Child D).
    eprintln!(
        "{}",
        floor::floor_drop_log_line(&name, "native rpc resident not identity-verified")
    );

    let turn = floor::FloorTurn {
        pi_bin: &pi_bin,
        session_dir: &session_dir,
        cwd: &cwd,
        prompt: message,
        continue_session,
    };
    match floor::run_floor_turn(&turn, floor::DEFAULT_FLOOR_TIMEOUT) {
        Ok(outcome) => {
            // C5/C3 (obligation (c), the DEAD-ONLY floor sub-lane): emit the three
            // phases into the target's log. The floor is a SYNCHRONOUS one-shot, so
            // sent + delivered + the success TERMINAL all resolve here — but the
            // terminal is CONTENT-KEYED against the APPENDED session record (the
            // floor's own observation seam; its `stopReason` readback is NOT the
            // resident observer). A minted send_id anchors all three (the floor has
            // no resident turn id). Best-effort; never alters the exit.
            let content_sha256 = dispatch::events::sha256_hex(message.as_bytes());
            let send_id = dispatch::events::mint_send_id(&RealClock);
            emit_daemon_send_events(&name, Some(session), message, &send_id, "pi");
            if dispatch::provider::pi::floor::rollout_landed(
                &floor_session_jsonl(&session_dir),
                &content_sha256,
            ) {
                emit_daemon_seen(&name, Some(session), &send_id, &content_sha256);
            }
            // Best-effort structured delivery: the turn landed and was captured
            // scrape-free. The outcome persists in the appended session file (the
            // transcript-read analog of the async rpc send). Confirm delivery on
            // stderr; SEND-vocabulary only.
            eprintln!(
                "qd send:relay: \"{name}\": delivered via pi structured floor (stopReason={}).",
                outcome.stop_reason.as_deref().unwrap_or("?")
            );
            invoked_send_relay(&name);
            0
        }
        Err(e) => {
            // §C1: the dead-only floor turn failed to deliver — record then fail loud.
            emit_door_failure(&name, Some(session), message, "floor-failed");
            eprintln!("qd send:relay: \"{name}\": pi floor delivery failed ({e}).");
            1
        }
    }
}

/// A minimal `ProviderFx` for resolving pi's env-only `transcript_root`
/// (`PI_CODING_AGENT_SESSION_DIR`/$HOME — never paths). Borrow lifetimes are
/// bounded by the caller's `env`/`paths`.
fn pi_resolve_fx<'a>(
    env: &'a RealEnv,
    paths: &'a dispatch::paths::QdPaths,
) -> dispatch::provider::ProviderFx<'a> {
    dispatch::provider::ProviderFx {
        env,
        paths,
        socket_dir: paths.sessions_dir.clone(),
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

/// A minimal `ProviderFx` for resolving the codex `transcript_root` off env only
/// (codex's `transcript_root` reads `fx.env` $CODEX_HOME/$HOME — never paths). The
/// borrow lifetimes are bounded by the caller's `env`/`paths`.
fn codex_resolve_fx<'a>(
    env: &'a RealEnv,
    paths: &'a dispatch::paths::QdPaths,
) -> dispatch::provider::ProviderFx<'a> {
    dispatch::provider::ProviderFx {
        env,
        paths,
        socket_dir: paths.sessions_dir.clone(),
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

/// Resolve `(relay_port, session_name, provider_id)` for `query`. The port is
/// `None` when the resolved session has no relay (the caller maps that to "has no
/// relay." exit 1). `Err(code)` carries an already-printed resolution failure.
///
/// Fast path first (send.ts:393-398), then the full-scan fallback
/// (send.ts:399-405): `resolveOrDie` → `session.relayPort`.
///
/// codex P1 W5 (codex-p1-spec section 7.1): the third tuple element is the
/// provider id the SEND seam dispatches through. The FAST PATH resolves only a
/// name+port from the registry/relay scan and never constructs a `Session`, so its
/// provider is "claude-code" BY CONSTRUCTION today (a daemon-provider row carries
/// no relay port — P2's rows take the fallback). The FALLBACK carries the
/// validated row provider (refuse_unknown_provider above already guaranteed it is
/// claude-code/opencode; opencode never reaches here with a port).
#[allow(clippy::type_complexity)]
fn resolve_relay_port(
    query: &str,
) -> Result<
    (
        Option<u16>,
        String,
        String,
        Option<Session>,
        Option<String>,
        SessionAccess,
    ),
    i32,
> {
    // Fast path: pid entries + relays + ppid map. Provider is claude-code by
    // construction (no Session row exists; the relay scan only knows claude relays).
    if let Some(fast) = fast_lookup(query) {
        let target_uuid = if fast.session_id.is_empty() {
            None
        } else {
            Some(fast.session_id.clone())
        };
        return Ok((
            Some(fast.port),
            fast.name,
            "claude-code".to_string(),
            None,
            target_uuid,
            fast.access,
        ));
    }

    // Fallback: cold / out-of-cap / tombstoned targets carry no live relay port, so
    // they always reach here. Resolve through the sealed uncapped entry. This is the
    // INTENTIONAL behavior change super13 named: it routes send:relay's resolution
    // through common::resolve_or_die's PID-AWARE live-over-stale dedup, REVERSING the
    // file-local status-only path this verb used (the old send_relay-local resolver
    // wrapping the pure resolve.rs matcher). Noted in the commit body. D-2 reject-set:
    // a stopped session can't receive a relay send → reject post-resolve.
    let session = common::resolve_session_uncapped(query)?;
    common::reject_if_tombstoned(query, &session)?;
    let session = &session;
    // codex P1, R1 (codex-p1-spec section 2.3): refuse an unknown provider LOUDLY.
    // NOTE (premise correction): the send:relay FAST PATH (above) resolves only a
    // name+port from the registry/relay scan and never constructs a `Session`, so
    // the refusal can only be armed on this FULL-SCAN fallback. That is sound for
    // P1 — the refusal is structurally unreachable today (join defaults absent to
    // claude-code), and P2's daemon-provider rows do not carry a relay port, so
    // they take this fallback, not the fast path.
    //
    // codex P2 W6 (codex-p2-spec section 7.5): `send:relay` is THE agent-facing
    // SEND channel, and `send` is the user-level op for a codex row too (ADD-23(4))
    // — so codex is a HANDLED provider HERE (routed to the codex SEND ladder in
    // run_with_client), distinct from the W7 verbs (attach/resume/kill) and
    // send:pty, which keep refusing codex via the shared helper until their own
    // wave wires them. We therefore pass a codex row through WITHOUT the refusal,
    // and refuse only a genuinely-unknown provider.
    // WS-A.2: pass a pi row through too (like codex/acp) — pi is a KNOWN provider, not an
    // unknown one. run_with_client gives it an HONEST "send is a tier-b turn, deferred"
    // message rather than this loud unknown-provider refusal.
    if session.provider != "codex"
        && !session.provider.starts_with("acp/")
        && session.provider != "pi"
    {
        if let Some(code) = common::refuse_unknown_provider("send:relay", session) {
            return Err(code);
        }
    }
    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    let target_uuid = fallback_target_uuid(session);
    let (access, relay_port) = real_session_access(session);
    Ok((
        relay_port.or(session.relay_port),
        name,
        session.provider.clone(),
        Some(session.clone()),
        target_uuid,
        access,
    ))
}

/// Run the engine fast-relay lookup with the REAL gathered inputs (registry pid
/// entries + relay ports + ppid map). Mirror of the I/O the TS
/// `fastRelayLookup` does inline (session.ts:1281, 1289, 1293-1298).
/// Whether a registry row's provider participates in the relay FAST PATH (F4 guard).
/// Daemon-hosted providers (`codex` / `acp/*`) do NOT: they carry no relay port and must
/// take the full-scan fallback so `send:relay` routes to their own SEND ladder. Without
/// this exclusion the fast path matches such a row BY NAME and resolves a relay via PID
/// ANCESTRY (a claude relay up the spawn chain) — mis-delivering the send (caught live:
/// the first acp send returned a relay id, not a turn id). Pinned by
/// `daemon_providers_excluded_from_relay_fast_path` so the fix cannot silently regress.
fn provider_uses_relay_fast_path(provider: Option<&str>) -> bool {
    // An absent provider reads as claude-code at the boundary (relay-capable).
    let p = provider.unwrap_or("claude-code");
    p != "codex" && !p.starts_with("acp/")
}

struct FastLookup {
    port: u16,
    session_id: String,
    name: String,
    access: SessionAccess,
}

fn fast_lookup(query: &str) -> Option<FastLookup> {
    let env = RealEnv;
    let paths = common::paths_from_home(&env).ok()?;

    // Live pid entries (TS getPidEntries — live only, no tombstones). EXCLUDE
    // daemon-hosted rows (codex / acp/*): they are NOT relay sessions, so they must
    // take the full-scan fallback and route to their own SEND ladder. Without this
    // filter the relay fast path matches such a row by name and resolves a relay via
    // PID ANCESTRY (a claude relay up the spawning chain) — mis-delivering a codex/acp
    // send to the wrong session. The codex-p2 comment ("daemon rows take the fallback")
    // was an unenforced premise; this enforces it.
    let pid_entries: Vec<PidEntryRef> =
        dispatch::registry::read_entries(&paths.sessions_dir, false)
            .into_iter()
            .filter(|s| provider_uses_relay_fast_path(s.entry.provider.as_deref()))
            .map(|s| PidEntryRef {
                name: s.entry.name,
                session_id: s.entry.session_id,
                pid: s.entry.pid.unwrap_or(0) as i32,
            })
            .collect();

    // Relay ports: sidecars, then the real HTTP probe fallback.
    let probe = dispatch::relay_http::HttpRelayProbe::new();
    let relay_candidates = relay::get_relay_ports(&paths.relay_dir, &probe);

    // ONE process snapshot drives both ancestry resolution and the channel-argv
    // proof, so a target cannot flip classifications between two `ps` reads.
    let rows = dispatch::effects::process_rows(&RealExec).ok()?;
    let relays = dispatch::adoption::verify_live_relays(
        &relay_candidates,
        &CcRelay::new(),
        &dispatch::effects::is_pid_alive,
    );
    let ppid_map = rows.iter().map(|(pid, row)| (*pid, row.ppid)).collect();
    let matched = relay::fast_relay_lookup(query, &pid_entries, &relays, &ppid_map)?;
    let access = dispatch::adoption::classify_live_claude(
        &matched.session_id,
        matched.pid,
        &relays,
        &rows,
        &dispatch::effects::is_pid_alive,
    );
    Some(FastLookup {
        port: matched.port,
        session_id: matched.session_id,
        name: matched.name,
        access,
    })
}

fn real_session_access(session: &Session) -> (SessionAccess, Option<u16>) {
    let env = RealEnv;
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return (
            SessionAccess {
                management: Management::Bare,
                relay_port: None,
            },
            None,
        );
    };
    let paths = dispatch::paths::QdPaths::from_home(std::path::Path::new(&home));
    let rows = dispatch::effects::process_rows(&RealExec).unwrap_or_default();
    let relay_candidates = relay::get_relay_ports(
        &paths.relay_dir,
        &dispatch::relay_http::HttpRelayProbe::new(),
    );
    let relays = dispatch::adoption::verify_live_relays(
        &relay_candidates,
        &CcRelay::new(),
        &dispatch::effects::is_pid_alive,
    );
    let access = dispatch::adoption::classify_session(
        session,
        &relays,
        &rows,
        &dispatch::effects::is_pid_alive,
    );
    (access, access.relay_port)
}

/// `Session "<name>" has no relay.` exit 1 (send.ts:407-408).
///
/// §C1 (delivery contract) — the RELAY DOOR. A resolved target session with no
/// relay transport fails BEFORE any wire activity. RECORD-THEN-FAIL: emit the
/// additive `send-failed` terminal into the TARGET's delivery log (best-effort),
/// THEN fail loud byte-identical (the `eprintln!` wording + `exit 1` are
/// unchanged). Every relay-door exit funnels through here (the four pre-wire
/// call sites + the structurally-unreachable NoTransport arm), so baking the
/// emission here means no relay-door path — present or future — is ever silent
/// (door-inventory §A). `target_session` keys the log to the target uuid when the
/// full-scan row is known, else the `byname-<target>` file.
fn no_relay_exit(name: &str, message: &str, target_session: Option<&Session>) -> i32 {
    emit_door_failure(name, target_session, message, "no-relay");
    eprintln!("Session \"{name}\" has no relay.");
    1
}

fn bare_destination_message(name: &str) -> String {
    format!(
        "Destination \"{name}\" is non-receivable (bare); no message was queued. \
         Ask the human to have that Claude Code session run `qd wrap {name}`. \
         Wrapping requires a manual qrmux restart with `qd relay:serve` and \
         `--dangerously-load-development-channels server:relay`."
    )
}

fn bare_destination_exit(name: &str, message: &str, target_session: Option<&Session>) -> i32 {
    emit_door_failure(name, target_session, message, "non-receivable-bare");
    eprintln!("{}", bare_destination_message(name));
    1
}

/// §C1 — emit a single `send-failed` terminal at a send DOOR (best-effort).
/// Serves BOTH the relay door (`no_relay_exit`, D1) AND the daemon protocol arms
/// (codex/`acp/*`/pi — D2's `run_codex_send`/`run_acp_send`/`run_pi_send`/
/// `run_pi_floor_send`): every carrier's door failure funnels its `send-failed`
/// here, so no door reads as "someone else's child" (door-inventory §A/§B).
/// Mirrors [`emit_relay_send_events`]'s target-keying and content hashing but
/// emits ONE failure record instead of the success pair. `send_id` is OMITTED
/// (spec `send-failed { send_id?, reason }`): a resolved pre-wire door has no
/// server-minted / resident-minted id yet — never client-invented. `content_sha256`
/// is carried for correlation; `reason` is a short surface token (extend additively).
fn emit_door_failure(
    target_name: &str,
    target_session: Option<&Session>,
    message: &str,
    reason: &str,
) {
    emit_door_failure_with_env(&RealEnv, target_name, target_session, message, reason);
}

fn emit_door_failure_with_env(
    env: &dyn Env,
    target_name: &str,
    target_session: Option<&Session>,
    message: &str,
    reason: &str,
) {
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        // No HOME → cannot resolve the state dir; emission is best-effort.
        return;
    };
    let state_dir =
        dispatch::paths::QdPaths::from_home_env(std::path::Path::new(&home), env).state_dir;
    // Key to the TARGET (not the sender): full-scan → target uuid; else the
    // `byname-<target>` file (session omitted), exactly as the success path keys.
    let writer = match target_session {
        Some(s) => dispatch::events::EventWriter::for_key(
            &state_dir,
            &s.session_id,
            Some(s.session_id.clone()),
            s.name.clone(),
        ),
        None => dispatch::events::EventWriter::for_key(
            &state_dir,
            &dispatch::events::byname_key(target_name),
            None,
            Some(target_name.to_string()),
        ),
    };
    let content_sha256 = dispatch::events::sha256_hex(message.as_bytes());
    let clock = RealClock;
    dispatch::events::warn_emit(
        &writer,
        &clock,
        &dispatch::events::Payload::SendFailed {
            send_id: None,
            content_sha256,
            reason: reason.to_string(),
        },
    );
}

/// `--wait` long-poll loop (send.ts:442-474). Returns the process exit code.
fn wait_for_reply(client: &dyn RelayContract, port: u16, message_id: &str, timeout_s: u64) -> i32 {
    use std::io::Write;

    // TS: `process.stderr.write(\`Sent ${messageId}, waiting for reply...\`)`
    // (send.ts:439).
    eprint!("Sent {message_id}, waiting for reply...");
    let _ = std::io::stderr().flush();

    let timeout_ms = timeout_s * 1000;

    // for (attempt = 0; attempt <= maxRetries; attempt++) (send.ts:442).
    //
    // RETRY POLICY KEYS ON THE ERROR CLASS (spec §4.1/§4.2 — binding over TS,
    // which retries any non-abort error): ONLY `ConnectionFailed` drives the
    // ≤3-retry loop. `Timeout` ends the wait ("Timed out", no retry, the TS
    // AbortError branch). `BadResponse`/`ServerError` are definite, non-transient
    // failures — retrying them 3× with 2s sleeps only delays the inevitable, so
    // they fail immediately.
    for attempt in 0..=MAX_RETRIES {
        match client.fetch_reply(port, message_id, timeout_ms) {
            Ok(reply) => {
                // TS: `process.stderr.write(" done\n")` (send.ts:453).
                eprintln!(" done");
                return finish_reply(reply);
            }
            Err(RelayError::Timeout) => {
                // TS AbortError branch (send.ts:462-465): "\n" + the message,
                // exit 1. No retry on timeout.
                eprintln!();
                eprintln!("Timed out waiting for reply.");
                return 1;
            }
            Err(RelayError::ConnectionFailed) if attempt < MAX_RETRIES => {
                // Transient connection drop → retry (send.ts:466-469 generic
                // catch, narrowed to the connection class per spec §4.2).
                // TS: ` retry ${attempt + 1}/${maxRetries}...` (send.ts:467).
                eprint!(" retry {}/{MAX_RETRIES}...", attempt + 1);
                let _ = std::io::stderr().flush();
                std::thread::sleep(RETRY_SLEEP);
                continue;
            }
            Err(e) => {
                // Connection drop with retries exhausted, OR a non-transient
                // class (BadResponse/ServerError) → fail now (send.ts:470-472
                // wording: "\n" + the failure, exit 1).
                eprintln!();
                eprintln!("Failed to get reply: {}", send_err_text(&e));
                return 1;
            }
        }
    }
    // The loop always returns; this is unreachable but keeps the type checker
    // happy without an explicit panic.
    1
}

/// Print the resolved reply and return the exit code (send.ts:454-460): an
/// `error` field → `Error: <e>` exit 1; otherwise print `text` exit 0.
fn finish_reply(reply: RelayReply) -> i32 {
    if let Some(err) = reply.error {
        // TS: `console.error(\`Error: ${data.error}\`)` exit 1 (send.ts:455-457).
        eprintln!("Error: {err}");
        return 1;
    }
    // TS: `console.log(data.text)` (send.ts:459). A missing text prints an empty
    // line (JS `console.log(undefined)` → "undefined", but the relay contract
    // guarantees one of text/error; an empty body prints nothing meaningful).
    println!("{}", reply.text.unwrap_or_default());
    0
}

/// Operator-facing text for a relay error in the "Failed to ..." messages. The
/// TS interpolates `err.message`; our errors carry a class-derived string.
fn send_err_text(e: &RelayError) -> String {
    e.to_string()
}

/// `parseInt(opts.timeout)` leading-integer parse (send.ts:440). A non-integer
/// → None (caller defaults to 120). Timing is wall-clock via thread::sleep + the
/// per-call read timeout, not a clock seam.
fn parse_timeout(s: &str) -> Option<u64> {
    let t = s.trim_start();
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_empty_session_id_does_not_trigger_self_send_guard() {
        let session = Session {
            name: Some("zmx-only".to_string()),
            user_named: None,
            session_id: String::new(),
            code: None,
            qd_id: None,
            pid: None,
            status: dispatch::model::SessionStatus::Idle,
            zmx_name: Some("zmx-only".to_string()),
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: dispatch::model::SessionBranch::ZmxOnly,
        };

        let target_uuid = fallback_target_uuid(&session);
        assert_eq!(target_uuid, None, "an empty fallback id is unresolved");
        assert!(
            !is_self_send(Some("caller-uuid"), target_uuid.as_deref()),
            "an unresolved target must not trigger self-send rejection"
        );
    }

    // F2 regression guard (red-team round 1) + the round-1 TOCTOU-fix pin:
    // `run_acp_send`'s `AcpConnection::connect` failure arm is reached ONLY when
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
    // (`cmdline_is_our_acp_daemon`), which every other test in this module
    // deliberately avoids — this file tests small pure/fake-backed helpers,
    // never the full verb against real process state. The classification
    // SEMANTICS are unit-pinned in ladder.rs (`connect_failure_*` tests).
    // MUTATION EVIDENCE: reintroducing a floor drive in the arm reds the bans;
    // dropping the re-probe or the loss-path preserve reds the positive
    // controls.
    #[test]
    fn acp_connect_failure_never_consults_the_ladder_or_drives_the_floor() {
        let src = include_str!("send_relay.rs");
        let start_marker = "let conn = match AcpConnection::connect(&endpoint, Duration::from_secs(5)) {";
        let start = src
            .find(start_marker)
            .expect("run_acp_send's connect-failure match must still exist verbatim");
        let after_start = &src[start..];
        // The match's Err(_) arm body runs from "Err(_) => {" to the matching "};"
        // that closes the whole `let conn = match ... };` statement — find that by
        // the next line consisting of only `    };` after the marker.
        let close = after_start
            .find("\n    };\n")
            .expect("the connect match must close with `    };` on its own line");
        let match_block = &after_start[..close];
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
                "run_acp_send's AcpConnection::connect failure arm must NEVER reach \
                 `{banned}` — the acp daemon is confirmed alive at this point (tier==Acp \
                 already proved endpoint_alive), so auto-flooring here risks a second live \
                 writer on the same session_id. Arm body:\n{err_arm}"
            );
        }
        // Positive control 1: it DOES call not_reachable (an empty/renamed
        // arm would vacuously pass the bans above without actually refusing).
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

    // F3 regression guard (red-team round 2, confirmed real): `run_acp_send`'s
    // `ProviderFx.acp_pre_dispatch` — the exactly-once history marker's install
    // point — must be installed UNCONDITIONALLY. Recording that a structured
    // send genuinely reached the wire is truth about the session's history and
    // must never be gated (originally: on the retired floor's disable flag).
    // The disposition no longer branches on it (Child D: pre-send and post-send
    // loss both refuse), but the durable record still guards any future reader
    // — e.g. `lifecycle.rs`'s resume seam consumes it — and losing it silently
    // was exactly the F3 bug. This is a structural (source-text) guard because
    // exercising the real branch needs a live, identity-verified acp connection
    // this file's test style deliberately avoids. MUTATION EVIDENCE: gating the
    // hook (`acp_pre_dispatch: some_condition.then(...)`) reds this test.
    #[test]
    fn acp_pre_dispatch_hook_is_installed_unconditionally() {
        // Scope the check to `run_acp_send`'s OWN body (fn-start to the next
        // top-level `fn`), NOT the whole file — a whole-file `contains` check
        // would be VACUOUSLY true here since this test's own assertion string
        // literal (quoting the exact marker) is itself part of `src`.
        let src = include_str!("send_relay.rs");
        let fn_start = src
            .find("fn run_acp_send(session: &Session, message: &str) -> i32 {")
            .expect("run_acp_send must still exist verbatim");
        let after_start = &src[fn_start..];
        let fn_end = after_start
            .find("fn run_pi_send(")
            .expect("run_pi_send must still follow run_acp_send");
        let body = &after_start[..fn_end];

        let unconditional = "acp_pre_dispatch: Some(&mark_dispatched),";
        assert!(
            body.contains(unconditional),
            "run_acp_send's ProviderFx must install acp_pre_dispatch UNCONDITIONALLY as \
             `{unconditional}`. Function body:\n{body}"
        );
    }

    // Child D structural guard (opencode D1 — clerk-4's Arm-B ratification, bond
    // note 01KX01BY7G): `run_acp_send` must have NO reachable auto-deliver path
    // on transport loss. Child B's degrade+latch+companion-drive machinery was
    // REMOVED, not gated — so this guard is structural: the identifiers cannot
    // appear in the fn body at all. It also pins the replacement disposition:
    // identity preservation (`acp_loss::preserve_identity`) at all four
    // transport-loss sites — the no-live-pid entry, the dead-tier entry lane,
    // the connect-boundary death (round-1 TOCTOU fix: the re-probe's
    // TransportLost classification), and the mid-flight NoTransport arm — each
    // followed by the existing human-recovery refusal. MUTATION EVIDENCE:
    // reintroducing a floor
    // drive (any banned identifier) reds this even though it compiles; deleting
    // a preserve_identity call reds the positive control.
    #[test]
    fn acp_send_transport_loss_refuses_with_identity_preserved_and_never_auto_delivers() {
        let src = include_str!("send_relay.rs");
        let fn_start = src
            .find("fn run_acp_send(session: &Session, message: &str) -> i32 {")
            .expect("run_acp_send must still exist verbatim");
        let after_start = &src[fn_start..];
        let fn_end = after_start
            .find("fn run_pi_send(")
            .expect("run_pi_send must still follow run_acp_send");
        let body = &after_start[..fn_end];

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
                "run_acp_send must have NO auto-deliver machinery — found `{banned}`. \
                 acp/claude-code is a NAMED DIVERGENCE (refuse-and-surface, identity \
                 preserved); pi's floor is the only auto-deliver realization. Body:\n{body}"
            );
        }
        let occurrences = body.matches("acp_loss::preserve_identity(").count();
        assert_eq!(
            occurrences, 4,
            "run_acp_send must preserve identity at exactly its four transport-loss \
             refusals (no-live-pid entry, dead-tier entry, connect-boundary death \
             [round-1 TOCTOU fix], mid-flight NoTransport) — found {occurrences}. \
             Body:\n{body}"
        );
    }

    // F4 regression guard: the relay fast-path daemon-exclusion (the live-surfaced
    // mis-route fix). Reverting `provider_uses_relay_fast_path` to allow codex/acp (or
    // dropping the `acp/` check) reds this — a non-vacuous guard for the no-regression
    // condition that previously had only the live turn-id-vs-relay-id discriminator.
    #[test]
    fn daemon_providers_excluded_from_relay_fast_path() {
        // Relay-capable rows participate in the fast path.
        assert!(provider_uses_relay_fast_path(None), "absent → claude-code (relay)");
        assert!(provider_uses_relay_fast_path(Some("claude-code")));
        assert!(provider_uses_relay_fast_path(Some("opencode")));
        // Daemon-hosted rows are EXCLUDED → they take the full-scan fallback.
        assert!(!provider_uses_relay_fast_path(Some("codex")), "codex is daemon-hosted");
        assert!(
            !provider_uses_relay_fast_path(Some("acp/claude-code")),
            "acp/* is daemon-hosted"
        );
        assert!(!provider_uses_relay_fast_path(Some("acp/anything")));
    }

    #[test]
    fn parse_timeout_leading_int() {
        assert_eq!(parse_timeout("120"), Some(120));
        assert_eq!(parse_timeout("30abc"), Some(30));
        assert_eq!(parse_timeout("abc"), None);
        assert_eq!(parse_timeout(""), None);
    }

    #[test]
    fn wrap_guidance_for_bare_destination_is_actionable_and_non_queuing() {
        let message = bare_destination_message("bare-one");
        assert_eq!(
            message,
            "Destination \"bare-one\" is non-receivable (bare); no message was queued. Ask the human to have that Claude Code session run `qd wrap bare-one`. Wrapping requires a manual qrmux restart with `qd relay:serve` and `--dangerously-load-development-channels server:relay`."
        );
    }

    #[test]
    fn finish_reply_text_is_exit_0() {
        let r = RelayReply {
            text: Some("hello".into()),
            error: None,
        };
        assert_eq!(finish_reply(r), 0);
    }

    #[test]
    fn finish_reply_error_is_exit_1() {
        let r = RelayReply {
            text: None,
            error: Some("boom".into()),
        };
        assert_eq!(finish_reply(r), 1);
    }

    // --- codex P1 W5 (codex-p1-spec section 7.1): inject routes through the
    // provider seam, mapped to the EXACT pre-rewire send:relay surface. ----------

    use std::cell::RefCell;

    /// A counting fake RelayContract: records every `send_message` call so the
    /// test can assert the SEND seam routes through the provider with the SAME
    /// (port, message, from) the pre-rewire `client.send_message` was handed.
    struct CountingRelay {
        message_id: String,
        sends: RefCell<Vec<(u16, String, String)>>,
        fail: Option<RelayError>,
    }
    impl RelayContract for CountingRelay {
        fn send_message(&self, port: u16, text: &str, from: &str) -> Result<String, RelayError> {
            self.sends
                .borrow_mut()
                .push((port, text.to_string(), from.to_string()));
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(self.message_id.clone()),
            }
        }
        fn fetch_reply(&self, _p: u16, _id: &str, _t: u64) -> Result<RelayReply, RelayError> {
            unreachable!("W5 inject test never long-polls")
        }
        fn health(&self, _p: u16, _t: u64) -> Result<dispatch::model::RelayHealth, RelayError> {
            unreachable!("W5 inject test never probes health")
        }
    }

    /// `inject_via_provider` (the SEND seam) routes through `ClaudeProvider::inject`
    /// → the RelayContract: EXACTLY one send, carrying the SAME port/message/from
    /// the pre-rewire `client.send_message(port, message, &from_session)` passed,
    /// and returns the relay's message id.
    ///
    /// MUTATION EVIDENCE: bypassing the provider (calling client.send_message
    /// directly with different args), or the claude inject body dropping
    /// relay/relay_port, reds this — the recorded send would be absent or carry the
    /// wrong tuple.
    #[test]
    fn inject_via_provider_routes_one_send_same_args() {
        let provider = dispatch::provider::provider_for("claude-code").unwrap();
        let relay = CountingRelay {
            message_id: "msg-w5-1".to_string(),
            sends: RefCell::new(Vec::new()),
            fail: None,
        };
        let id = inject_via_provider(provider, &relay, 8901, "wk", "hello world", "cli").unwrap();
        assert_eq!(id, "msg-w5-1", "returns the relay's message id");
        let sends = relay.sends.borrow();
        assert_eq!(sends.len(), 1, "exactly one send through the contract");
        assert_eq!(
            sends[0],
            (8901u16, "hello world".to_string(), "cli".to_string()),
            "the send carries the same port/message/from as the pre-rewire path"
        );
    }

    // --- B2 item 5: derive_from_session precedence matrix (unit level; the
    // full-stack channel-header pin is punch_b2_item5_repro.rs). -------------

    use dispatch::effects::MapEnv;

    /// Build a MapEnv + a staged ids.jsonl under a tempdir HOME. Returns the
    /// tempdir (keep alive) and the env.
    fn identity_env(
        qd_session_id: Option<&str>,
        claude_session_id: Option<&str>,
        ids_lines: &str,
    ) -> (tempfile::TempDir, MapEnv) {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join(".quorum").join("dispatch").join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("ids.jsonl"), ids_lines).unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.path().to_string_lossy().to_string());
        if let Some(v) = qd_session_id {
            vars.insert("QD_SESSION_ID".to_string(), v.to_string());
        }
        if let Some(v) = claude_session_id {
            vars.insert("CLAUDE_CODE_SESSION_ID".to_string(), v.to_string());
        }
        (home, MapEnv { vars, uid: 501 })
    }

    const MINT: &str = "{\"event\":\"mint\",\"id\":\"ab3kx9mq\",\"session_id\":\"true-uuid-1\"}\n";

    #[test]
    fn engine_identity_wins_over_inherited_env() {
        // Both planted: the idstore-resolved uuid wins over the leaked env var.
        let (_h, env) = identity_env(Some("ab3kx9mq"), Some("imposter-uuid"), MINT);
        assert_eq!(derive_from_session(&env), "true-uuid-1");
    }

    #[test]
    fn unresolvable_engine_identity_falls_back_to_claude_env() {
        // Valid-shaped but unknown id → fall through, never invent.
        let (_h, env) = identity_env(Some("zzzzzzzz"), Some("cc-uuid"), MINT);
        assert_eq!(derive_from_session(&env), "cc-uuid");
        // Malformed id → same fall-through.
        let (_h2, env2) = identity_env(Some("not-an-id!"), Some("cc-uuid"), MINT);
        assert_eq!(derive_from_session(&env2), "cc-uuid");
        // UNBOUND mint (no uuid yet) → same fall-through.
        let unbound = "{\"event\":\"mint\",\"id\":\"cd47qrst\",\"session_id\":null}\n";
        let (_h3, env3) = identity_env(Some("cd47qrst"), Some("cc-uuid"), unbound);
        assert_eq!(derive_from_session(&env3), "cc-uuid");
    }

    #[test]
    fn bare_shell_is_cli() {
        // Neither identity present → "cli" (the operator-shell attribution the
        // ruling pins as must-not-break).
        let (_h, env) = identity_env(None, None, MINT);
        assert_eq!(derive_from_session(&env), "cli");
        // QD_SESSION_ID unresolvable and no claude env → still "cli".
        let (_h2, env2) = identity_env(Some("zzzzzzzz"), None, MINT);
        assert_eq!(derive_from_session(&env2), "cli");
    }

    #[test]
    fn engine_identity_is_case_folded() {
        // Ids are case-insensitive at resolution (idstore::normalize).
        let (_h, env) = identity_env(Some("AB3KX9MQ"), Some("imposter"), MINT);
        assert_eq!(derive_from_session(&env), "true-uuid-1");
    }

    /// A `RelayFailed` (the claude inject mapping a transport `RelayError`) maps
    /// back to exit 1 AND the SAME "Failed to send message: <send_err_text(e)>"
    /// wording the pre-rewire `Err(e)` arm produced (send.ts:432-434). We pin the
    /// wording SOURCE — `send_err_text` over the preserved error class — so the
    /// stderr line is byte-identical even though the unit can't capture stderr.
    ///
    /// MUTATION EVIDENCE: flattening InjectError::RelayFailed (losing the class) or
    /// changing the exit code reds this — the helper would return a different code
    /// and `send_err_text` would no longer key on the preserved RelayError.
    #[test]
    fn inject_via_provider_relay_failed_maps_to_send_failure() {
        let provider = dispatch::provider::provider_for("claude-code").unwrap();
        let relay = CountingRelay {
            message_id: "unused".to_string(),
            sends: RefCell::new(Vec::new()),
            fail: Some(RelayError::ConnectionFailed),
        };
        let code = inject_via_provider(provider, &relay, 8901, "wk", "m", "cli").unwrap_err();
        assert_eq!(
            code, 1,
            "RelayFailed maps to exit 1 (the pre-rewire Err arm)"
        );
        // The wording SOURCE is unchanged: the old path printed
        // `Failed to send message: {send_err_text(&e)}`; the preserved class
        // round-trips through the same `send_err_text`.
        assert_eq!(
            send_err_text(&RelayError::ConnectionFailed),
            "connection failed",
            "send_err_text wording for the preserved class is unchanged"
        );
    }

    // =======================================================================
    // §X (3-phase delivery) — Tier-2 seam-integration proof of the SENDER side
    // (Group A): a true e2e relay POST mints a real server `message_id`, then the
    // REAL `emit_relay_send_events_with_env` writes `send-initiated` (the reused
    // Payload::SendInitiated with relay values) + `relay-delivered` into the
    // TARGET's real `<state>/sessions/<uuid>.events.jsonl`, tailed from disk and
    // asserted against PINNED-EVENT-CONTRACT §X.3.1/§X.3.2. No record is
    // hand-written: every line comes from the production emit fn via warn_emit.
    // (The recipient half — message-seen — is proven in the lib test module's
    // i1_recipient_* tests; both halves share send_id == message_id.)
    // =======================================================================

    use dispatch::events::{parse_events, sha256_hex};
    use dispatch::relay_http::CcRelay;
    use dispatch::relay_server::RelayServer;
    use std::time::Duration;

    fn wait_until_up(port: u16) {
        let client = CcRelay::new();
        for _ in 0..200 {
            if client.health(port, 2).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("relay server never came up on port {port}");
    }

    /// I1 (sender side, true e2e): a real `send:relay` POST to a real local
    /// `relay:serve` recipient mints a real `message_id`; the REAL
    /// `emit_relay_send_events_with_env` then writes exactly one `send-initiated`
    /// (relay values) + one `relay-delivered` (non-terminal) into the TARGET's real
    /// delivery log. Tailed from disk; §X.3.1/§X.3.2 shapes + send_id==message_id +
    /// content_sha256 over the raw message bytes are asserted.
    #[test]
    fn i1_relay_happy_sender_emits_send_initiated_and_relay_delivered() {
        // 1) A real recipient relay server (real HTTP, isolated tmp HOME).
        let recv_home = tempfile::tempdir().unwrap();
        let handle = RelayServer::spawn_for_test(
            recv_home.path(),
            0,
            Duration::from_millis(150),
            Duration::from_secs(10),
        );
        wait_until_up(handle.port);

        // 2) A real send over the WIRE → the server mints a real message_id.
        let message = "prime the session — please ack receipt";
        let client = CcRelay::new();
        let message_id = client
            .send_message(handle.port, message, "sess-sender")
            .expect("real relay POST mints a message_id");
        assert!(
            message_id.starts_with("relay-"),
            "real server message_id: {message_id}"
        );

        // 3) The REAL sender-side emit, keyed to the TARGET uuid, into an isolated
        //    sender-side HOME (the events go into the target's log under it).
        let sender_home = tempfile::tempdir().unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert(
            "HOME".to_string(),
            sender_home.path().to_string_lossy().to_string(),
        );
        let env = MapEnv { vars, uid: 501 };
        let target_uuid = "11111111-2222-3333-4444-555555555555";
        let target = Session {
            name: Some("target-b".to_string()),
            user_named: None,
            session_id: target_uuid.to_string(),
            code: None,
            qd_id: None,
            pid: None,
            status: dispatch::model::SessionStatus::Idle,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: Some(handle.port),
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: dispatch::model::SessionBranch::LiveRegistry,
        };
        emit_relay_send_events_with_env(&env, "target-b", Some(&target), message, &message_id);

        // 4) Tail the TARGET's real delivery log under the sender HOME.
        let state_dir = dispatch::paths::QdPaths::from_home_env(sender_home.path(), &env).state_dir;
        let log = state_dir
            .join("sessions")
            .join(format!("{target_uuid}.events.jsonl"));
        let raw = std::fs::read_to_string(&log)
            .unwrap_or_else(|e| panic!("target log {log:?} must exist: {e}"));
        let recs = parse_events(&raw).records;

        // 5) Assert §X.3.1/§X.3.2: exactly one send-initiated + one relay-delivered,
        //    both keyed by send_id == message_id, in that order.
        let kinds: Vec<&str> = recs.iter().map(|r| r.event.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["send-initiated", "relay-delivered"],
            "sender emits send-initiated then relay-delivered; got {kinds:?}"
        );
        for r in &recs {
            assert_eq!(
                r.send_id().as_deref(),
                Some(message_id.as_str()),
                "send_id == message_id on every sender-side record (§X.4)"
            );
        }
        let want_sha = sha256_hex(message.as_bytes());

        // send-initiated: the REUSED Payload::SendInitiated with relay values.
        let si: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("send-initiated")).unwrap())
                .unwrap();
        assert_eq!(si["event"], "send-initiated");
        assert_eq!(si["v"], 1);
        assert_eq!(si["verb"], "send:relay");
        assert_eq!(si["send_path"], "relay");
        assert_eq!(si["chunks"], 1);
        assert_eq!(si["content_len"], message.as_bytes().len());
        assert_eq!(si["content_sha256"].as_str().unwrap(), want_sha);
        assert_eq!(
            si["chunk_sha256s"].as_array().unwrap(),
            &vec![serde_json::json!(want_sha)],
            "chunk_sha256s = [content_sha256] (one chunk)"
        );
        assert!(
            si.get("content_preview").is_none(),
            "relay send-initiated carries NO prose (§X.7)"
        );
        assert!(
            si.get("transcript").is_none() && si.get("transcript_offset").is_none(),
            "relay sender-side has no recovery transcript (§X.3.1)"
        );
        assert!(
            si.get("chunk_sha256s_capped").is_none(),
            "omit-false (§X.3.1)"
        );
        assert_eq!(
            si["session"].as_str().unwrap(),
            target_uuid,
            "keyed to the TARGET uuid (§X.3.1 / Group A keying)"
        );

        // relay-delivered: NON-terminal on-queued.
        let rd: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("relay-delivered")).unwrap())
                .unwrap();
        assert_eq!(rd["event"], "relay-delivered");
        assert_eq!(rd["v"], 1);
        assert_eq!(rd["content_sha256"].as_str().unwrap(), want_sha);
        assert!(
            !dispatch::events::is_terminal("relay-delivered"),
            "relay-delivered is NON-terminal"
        );

        // Evidence dump (oracle input) when DISPATCH_PROOF_DIR is set.
        if let Ok(dir) = std::env::var("DISPATCH_PROOF_DIR") {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(
                std::path::Path::new(&dir).join("I1-relay-happy-sender.jsonl"),
                &raw,
            );
        }

        handle.shutdown();
    }

    /// D2 §C1 (door-inventory §B) — a daemon-arm DOOR FAILURE emits exactly one
    /// `send-failed` terminal into the TARGET's delivery log: keyed to the target
    /// uuid, `send_id` OMITTED (pre-wire, spec `send-failed { send_id? }`),
    /// `content_sha256` over the raw message, `reason` the surface token. This is
    /// the single emission ALL four daemon-arm doors (codex/acp/pi/floor) funnel
    /// through (`emit_door_failure` → this `_with_env`); the per-lane end-to-end
    /// door INJECTIONS live in the conformance suite. No record is hand-written.
    #[test]
    fn door_failure_emits_send_failed_terminal_keyed_to_target() {
        let home = tempfile::tempdir().unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert(
            "HOME".to_string(),
            home.path().to_string_lossy().to_string(),
        );
        let env = MapEnv { vars, uid: 501 };
        let target_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let message = "steer: please ack";
        let target = Session {
            name: Some("codex-dead-1".to_string()),
            user_named: None,
            session_id: target_uuid.to_string(),
            code: None,
            qd_id: None,
            pid: None,
            status: dispatch::model::SessionStatus::Idle,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "codex".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: dispatch::model::SessionBranch::LiveRegistry,
        };
        emit_door_failure_with_env(&env, "codex-dead-1", Some(&target), message, "daemon-unreachable");

        let state_dir = dispatch::paths::QdPaths::from_home_env(home.path(), &env).state_dir;
        let log = state_dir
            .join("sessions")
            .join(format!("{target_uuid}.events.jsonl"));
        let raw = std::fs::read_to_string(&log)
            .unwrap_or_else(|e| panic!("target log {log:?} must exist: {e}"));
        let recs = parse_events(&raw).records;

        let kinds: Vec<&str> = recs.iter().map(|r| r.event.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["send-failed"],
            "exactly one send-failed at the door; got {kinds:?}"
        );

        let sf: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("send-failed")).unwrap()).unwrap();
        assert_eq!(sf["event"], "send-failed");
        assert_eq!(sf["v"], 1);
        assert_eq!(sf["reason"], "daemon-unreachable");
        assert_eq!(
            sf["content_sha256"].as_str().unwrap(),
            sha256_hex(message.as_bytes()),
            "content_sha256 over the raw message bytes (the consumer's join key)"
        );
        assert!(
            sf.get("send_id").is_none(),
            "send_id OMITTED at a pre-wire door (spec `send-failed {{ send_id? }}`)"
        );
        assert_eq!(
            sf["session"].as_str().unwrap(),
            target_uuid,
            "keyed to the TARGET uuid"
        );
        assert!(
            dispatch::events::is_terminal("send-failed"),
            "send-failed is a terminal (§C1)"
        );
    }

    /// D2 §C5/C3 (daemon-lane phases) — a daemon inject-SUCCESS emits exactly
    /// `send-initiated` (sent — daemon values: verb `send:relay`, `send_path` the
    /// lane, `send_id` the resident turn id, NO recovery transcript keys) then
    /// `turn-accepted` (delivered, NON-terminal), keyed to the TARGET uuid, in that
    /// order. The success TERMINAL is deliberately NOT emitted here — it lands at
    /// the observe seam (run_acp_wait / run_pi_wait). The absent transcript keys +
    /// verb `send:relay` keep this send OUT of the recover verb's pty/new-p sweep.
    #[test]
    fn daemon_send_emits_send_initiated_then_turn_accepted() {
        let home = tempfile::tempdir().unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.path().to_string_lossy().to_string());
        let env = MapEnv { vars, uid: 501 };
        let target_uuid = "12121212-3434-5656-7878-909090909090";
        let message = "steer: land this mid-turn";
        let turn_id = "turn-abc";
        let target = Session {
            name: Some("acp-live-1".to_string()),
            user_named: None,
            session_id: target_uuid.to_string(),
            code: None,
            qd_id: None,
            pid: Some(4242),
            status: dispatch::model::SessionStatus::Idle,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "acp/claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: dispatch::model::SessionBranch::LiveRegistry,
        };
        emit_daemon_send_events_with_env(
            &env,
            "acp-live-1",
            Some(&target),
            message,
            turn_id,
            "acp/claude-code",
        );

        let state_dir = dispatch::paths::QdPaths::from_home_env(home.path(), &env).state_dir;
        let log = state_dir
            .join("sessions")
            .join(format!("{target_uuid}.events.jsonl"));
        let raw = std::fs::read_to_string(&log)
            .unwrap_or_else(|e| panic!("target log {log:?} must exist: {e}"));
        let recs = parse_events(&raw).records;

        let kinds: Vec<&str> = recs.iter().map(|r| r.event.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["send-initiated", "turn-accepted"],
            "daemon inject-success emits sent then delivered (no terminal here); got {kinds:?}"
        );
        for r in &recs {
            assert_eq!(
                r.send_id().as_deref(),
                Some(turn_id),
                "send_id == the resident turn id on both records"
            );
        }
        let want_sha = sha256_hex(message.as_bytes());

        let si: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("send-initiated")).unwrap())
                .unwrap();
        assert_eq!(si["verb"], "send:relay");
        assert_eq!(si["send_path"], "acp/claude-code", "send_path names the lane");
        assert_eq!(si["content_sha256"].as_str().unwrap(), want_sha);
        assert!(
            si.get("transcript").is_none() && si.get("transcript_offset").is_none(),
            "NO recovery transcript keys → out of the recover verb's pty/new-p sweep"
        );
        assert_eq!(si["session"].as_str().unwrap(), target_uuid, "keyed to TARGET uuid");

        let ta: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("turn-accepted")).unwrap())
                .unwrap();
        assert_eq!(ta["event"], "turn-accepted");
        assert_eq!(ta["content_sha256"].as_str().unwrap(), want_sha);
        assert!(
            !dispatch::events::is_terminal("turn-accepted"),
            "turn-accepted is NON-terminal delivered — only the terminal says landed"
        );
    }

    /// D2 §C5/C3 (obligation (c), the DEAD-ONLY floor sub-lane) — the floor's
    /// success terminal is CONTENT-KEYED against the APPENDED session record:
    /// `floor_session_jsonl` reads the dedicated `--session-dir`, the shared
    /// `rollout_landed` confirms the sent bytes as a user record, and
    /// `emit_daemon_seen` appends the message-seen terminal. Hermetic — the live
    /// floor drive needs pi/OAuth (deferred).
    #[test]
    fn floor_content_keyed_seen_from_appended_session_record() {
        let home = tempfile::tempdir().unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.path().to_string_lossy().to_string());
        let env = MapEnv { vars, uid: 501 };
        let uuid = "abababab-cdcd-efef-0101-232323232323";
        let msg = "floor prompt that must land";
        let target = Session {
            name: Some("pi-floor-1".to_string()),
            user_named: None,
            session_id: uuid.to_string(),
            code: None,
            qd_id: None,
            pid: None,
            status: dispatch::model::SessionStatus::Idle,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "pi".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: dispatch::model::SessionBranch::LiveRegistry,
        };

        // A dedicated floor `--session-dir` holding the appended user record.
        let session_dir = home.path().join("floordir");
        std::fs::create_dir_all(&session_dir).unwrap();
        let user = serde_json::json!({
            "type": "message",
            "message": {"role": "user", "content": [{"type": "text", "text": msg}]}
        });
        std::fs::write(
            session_dir.join("sess.jsonl"),
            format!("{}\n", serde_json::to_string(&user).unwrap()),
        )
        .unwrap();

        let jsonl = floor_session_jsonl(&session_dir);
        let sha = sha256_hex(msg.as_bytes());
        assert!(
            dispatch::provider::pi::floor::rollout_landed(&jsonl, &sha),
            "the appended user record makes the send content-keyed LANDED"
        );

        emit_daemon_seen_with_env(&env, "pi-floor-1", Some(&target), "floor-send-1", &sha);
        let state_dir = dispatch::paths::QdPaths::from_home_env(home.path(), &env).state_dir;
        let raw = std::fs::read_to_string(dispatch::events::events_path(&state_dir, uuid)).unwrap();
        let ms: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("message-seen")).unwrap()).unwrap();
        assert_eq!(ms["event"], "message-seen");
        assert_eq!(ms["send_id"], "floor-send-1");
        assert_eq!(ms["content_sha256"].as_str().unwrap(), sha);
    }
}
