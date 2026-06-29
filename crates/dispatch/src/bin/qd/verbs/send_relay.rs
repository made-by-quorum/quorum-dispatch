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

use dispatch::effects::{Env, RealClock, RealEnv, RealProcessTable};
use dispatch::exec::RealExec;
use dispatch::model::Session;
use dispatch::relay::{self, FastRelayMatch, PidEntryRef, RelayContract, RelayError, RelayReply};
use dispatch::relay_http::CcRelay;

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

/// The verb body, parameterized on the relay client so tests inject a fake.
fn run_with_client(
    query: &str,
    message: &str,
    wait: bool,
    timeout_s: u64,
    client: &dyn RelayContract,
) -> i32 {
    // --- resolve the relay port (fast path, then full-scan fallback) ---
    let (relay_port, session_name, provider_id, session) = match resolve_relay_port(query) {
        Ok(v) => v,
        Err(code) => return code,
    };

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
            // wording rather than panic.
            return no_relay_exit(&session_name);
        };
        return run_codex_send(&session, message);
    }

    // scoped-ACP-CC (residence SEND): an `acp/*` row is a daemon-hosted ACP thread with
    // no relay port — route it to the ACP SEND path (reconnect to the resident adapter →
    // AcpProvider::inject) BEFORE the relay-port-None check, exactly as the codex arm does.
    if provider_id.starts_with("acp/") {
        let Some(session) = session else {
            return no_relay_exit(&session_name);
        };
        return run_acp_send(&session, message);
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
        return no_relay_exit(&session_name);
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
    // bond; bond adopts these lines by content_sha256 + send_id). Best-effort.
    emit_relay_send_events(&session_name, session.as_ref(), message, &message_id);

    if !wait {
        // Async: print the message_id and exit 0 (send.ts:437-440).
        println!("{message_id}");
        usage_send_relay(&session_name);
        return 0;
    }

    // --- --wait long-poll (send.ts:442-474) ---
    let code = wait_for_reply(client, port, &message_id, timeout_s);
    if code == 0 {
        // A6 §4.1: a usage line on the SUCCESSFUL (exit-0) --wait reply too.
        usage_send_relay(&session_name);
    }
    code
}

/// §X (3-phase delivery) — relay on-sent + on-queued emission.
///
/// Writes `send-initiated` (the EXISTING `Payload::SendInitiated` constructed
/// with relay values, §X.3.1 — NOT a bare 2-field record) and `relay-delivered`
/// (§X.3.2, non-terminal) into the **TARGET's** delivery log, keyed to the target
/// uuid when the full-scan path resolved a `Session` row, else the
/// `byname-<target>` file (bond merges both, §1.4 G5). `send_id = message_id`;
/// `content_sha256 = sha256(raw caller message bytes)` (§X.4 — the SAME bytes
/// bond hashes into its on-sent Attempting marker). The relay `send-initiated`
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

/// Append a content-free A6 usage line for a SUCCESSFUL `send:relay`. The fast
/// path yields only a NAME (no sessionId), so we key the usage line by name —
/// the fold accepts either. Best-effort: a failure warns but NEVER changes the
/// verb's exit code (spec §4.1).
fn usage_send_relay(name: &str) {
    use dispatch::effects::RealClock;
    if let Err(e) =
        dispatch::telemetry::append_usage(&RealEnv, &RealClock, "send", None, Some(name))
    {
        eprintln!("WARNING: telemetry usage append failed (non-fatal): {e}");
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
        acp_client: None,    };
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
            // pre-inject port-None check (send.ts:406-409).
            Err(no_relay_exit(session_name))
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
/// usage line is appended. EVERY user-facing string here is SEND-vocabulary — NO
/// `turn/start`, `turn/steer`, or `expectedTurnId` ever appears (W2 enforces it in
/// the rpc layer; the verb keeps it too).
fn run_codex_send(session: &Session, message: &str) -> i32 {
    use dispatch::provider::codex::{
        open_turn_id, read_lines, AppServerRpc, ClientInfo, WsAppServer,
    };
    use dispatch::provider::{InjectError, Provider, ProviderFx, SessionKey};

    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());

    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // The thread id (m2 — the REAL uuid the daemon assigned) is the row's sessionId.
    let thread_id = session.session_id.clone();
    if thread_id.is_empty() {
        eprintln!("qd send:relay: \"{name}\": this codex session has no thread id (cold/dead).");
        return 1;
    }

    // The endpoint is the registry row's recorded `endpoint` (NOT on the Session /
    // --json surface — re-read the row by pid). A dead/cold row (no live pid) has
    // no daemon to reach.
    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        eprintln!("qd send:relay: \"{name}\": session daemon not reachable (try qd resume {name})");
        return 1;
    };
    let endpoint = match dispatch::registry::read_entry(&paths.sessions_dir, pid)
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty())
    {
        Some(ep) => ep,
        None => {
            eprintln!(
                "qd send:relay: \"{name}\": session daemon not reachable (try qd resume {name})"
            );
            return 1;
        }
    };

    // Connect a fresh short-lived ws client → initialize handshake (readiness).
    let rpc = match WsAppServer::connect(&endpoint, std::time::Duration::from_secs(5)) {
        Ok(c) => c,
        Err(_e) => {
            // Daemon unreachable (connect failed): the same SEND-vocabulary surface
            // as a missing endpoint (§7.5).
            eprintln!(
                "qd send:relay: \"{name}\": session daemon not reachable (try qd resume {name})"
            );
            return 1;
        }
    };
    {
        let client = ClientInfo {
            name: "qd-manager".to_string(),
            title: None,
            version: "0".to_string(),
        };
        if rpc.initialize(&client).is_err() {
            eprintln!(
                "qd send:relay: \"{name}\": session daemon not reachable (try qd resume {name})"
            );
            return 1;
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
        acp_client: None,    };
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
            // The async-send analog: print the id (turn id here), append usage.
            println!("{turn_id}");
            usage_send_relay(&name);
            0
        }
        Err(InjectError::NoTransport(_)) => {
            // Structurally unreachable (we set app_server Some) — defensive.
            eprintln!(
                "qd send:relay: \"{name}\": session daemon not reachable (try qd resume {name})"
            );
            1
        }
        Err(e) => {
            // A protocol/precondition failure. SEND-vocabulary only (InjectError's
            // Display carries no start/steer tokens; the rpc-layer error text is
            // W2-sanitized).
            eprintln!("qd send:relay: \"{name}\": send failed ({e}).");
            1
        }
    }
}

/// scoped-ACP-CC residence SEND path (S7). The ACP analog of [`run_codex_send`]: re-read
/// the row's recorded `endpoint`, verify the resident adapter's IDENTITY (pid alive AND
/// the live `/proc` cmdline carries our `acp-daemon --listen <endpoint>` — S6, defeats PID
/// reuse), derive the ladder tier from `(provider, transport-field, endpoint-alive)`, and
/// only on `Tier::Acp` connect a fresh [`AcpConnection`] and drive `AcpProvider::inject`.
/// A dead/wrong-identity endpoint or a latched-pty row DEGRADES to the SEND "not reachable"
/// surface — never a hang on a dead endpoint, never a fake.
fn run_acp_send(session: &Session, message: &str) -> i32 {
    use dispatch::provider::acp::{derive_tier, AcpClient, AcpConnection, Tier, ACP_CC_PROVIDER};
    use dispatch::provider::{InjectError, Provider, ProviderFx, SessionKey};

    let name = session.name.clone().unwrap_or_default();
    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let not_reachable = |name: &str| {
        eprintln!(
            "qd send:relay: \"{name}\": acp session daemon not reachable (try qd resume {name})"
        );
        1
    };

    let Some(pid) = session.pid.filter(|&p| p != 0) else {
        return not_reachable(&name);
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

    // S7 ladder: drive ONLY on Tier::Acp; else degrade (no drive-on-dead, no hang).
    let tier = derive_tier("acp/claude-code", transport_field.as_deref(), endpoint_alive);
    if tier != Tier::Acp {
        return not_reachable(&name);
    }
    let endpoint = endpoint.expect("Tier::Acp implies a live endpoint");

    let conn = match AcpConnection::connect(&endpoint, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(_) => return not_reachable(&name),
    };
    let conn_ref: &dyn AcpClient = &conn;
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
            println!("{turn_id}");
            usage_send_relay(&name);
            0
        }
        Err(InjectError::Precondition(s)) => {
            eprintln!("qd send:relay: \"{name}\": send failed ({s}).");
            1
        }
        Err(_) => not_reachable(&name),
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
        acp_client: None,    }
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
fn resolve_relay_port(query: &str) -> Result<(Option<u16>, String, String, Option<Session>), i32> {
    // Fast path: pid entries + relays + ppid map. Provider is claude-code by
    // construction (no Session row exists; the relay scan only knows claude relays).
    if let Some(fast) = fast_lookup(query) {
        return Ok((Some(fast.port), fast.name, "claude-code".to_string(), None));
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
    if session.provider != "codex" && !session.provider.starts_with("acp/") {
        if let Some(code) = common::refuse_unknown_provider("send:relay", session) {
            return Err(code);
        }
    }
    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    Ok((
        session.relay_port,
        name,
        session.provider.clone(),
        Some(session.clone()),
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

fn fast_lookup(query: &str) -> Option<FastRelayMatch> {
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
    let relays = relay::get_relay_ports(&paths.relay_dir, &probe);

    // ppid map for the ancestry walk.
    let pt = RealProcessTable::new(RealExec);
    let ppid_map = {
        use dispatch::effects::ProcessTable;
        pt.ppid_map().unwrap_or_default()
    };

    relay::fast_relay_lookup(query, &pid_entries, &relays, &ppid_map)
}

/// `Session "<name>" has no relay.` exit 1 (send.ts:407-408).
fn no_relay_exit(name: &str) -> i32 {
    eprintln!("Session \"{name}\" has no relay.");
    1
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
        vars.insert(
            "HOME".to_string(),
            home.path().to_string_lossy().to_string(),
        );
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
        let message = "prime the bond — please ack receipt";
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
}
