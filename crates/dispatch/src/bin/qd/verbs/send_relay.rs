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
use quorum_qw::delivery;

use super::common;
use super::send_unified::CarrierOutcome;

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

// -- THE `claude_relay` CARRIER IS NOT HERE ANY MORE -----------------------
//
// `run_claude_relay_unified` lived here, as the unified send's entry into the
// Claude relay injection path, and `LaneOps::deliver` called back UP into it
// through `quorum_qw::Carriers`. Phase 3B moved the body to
// `quorum_qw::delivery::relay::send_claude_relay` and the lane now calls it
// directly, so the wrapper had exactly one caller left -- the lane -- and a
// wrapper whose only caller is the thing it was written to avoid is not a
// wrapper. It is deleted rather than left `#[allow(dead_code)]`.
//
// The explicit `qd send:relay` verb below never used it: it injects through
// `inject_via_provider`, which is a different identity contract (name-as-id,
// because the fast path has no `Session` row) and is pinned as such.
//
// The three daemon carriers' wrappers DO survive, further down -- `qd send:relay`
// routes to them directly for a codex / `acp/*` / pi row -- and each is
// `quorum_qw::delivery::render` over the core, the same one-line printing layer
// `LaneOps::deliver` uses.

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
        // `.code` — the three daemon carriers answer a `CarrierOutcome` since
        // stage-2 phase 4 so `LaneOps::deliver` can key a `Receipt` on the turn
        // id they mint. `send:relay` is a verb, not a lane: it wants the exit
        // code and nothing else, and the code is UNCHANGED by the widening. Same
        // at the acp and pi arms below.
        return run_codex_send(&session, message).code;
    }

    // scoped-ACP-CC (residence SEND): an `acp/*` row is a daemon-hosted ACP thread with
    // no relay port — route it to the ACP SEND path (reconnect to the resident adapter →
    // AcpProvider::inject) BEFORE the relay-port-None check, exactly as the codex arm does.
    if provider_id.starts_with("acp/") {
        let Some(session) = session else {
            return no_relay_exit(&session_name, message, None);
        };
        return run_acp_send(&session, message).code;
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
        return run_pi_send(&session, message).code;
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
    let from_session = delivery::derive_from_session(&RealEnv);

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
    delivery::emit_relay_send_events(
        &RealEnv,
        &RealClock,
        &session_name,
        session.as_ref(),
        message,
        &message_id,
    );

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
///
/// The append itself is [`quorum_qw::delivery::append_send_invoked`] (telemetry is
/// qw's); this is the printing half. The four moved carriers append it inside
/// their cores and return the warning as a note — only `send:relay`'s OWN paths
/// still reach it through here.
fn invoked_send_relay(name: &str) {
    if let Some(note) = delivery::append_send_invoked(&RealEnv, &RealClock, name) {
        eprintln!("{note}");
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
        await_relay: None,
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

/// The `codex_daemon` carrier's qd half — see
/// [`quorum_qw::delivery::codex::send_codex`] for the turn ladder itself.
///
/// VERB-LAYER ADAPTER ONLY: resolve HOME (whose refusal line
/// `common::paths_from_home` owns and has always printed unattributed as
/// `qd: HOME is not set …`), then `map_err` + `eprintln!`.
///
/// TWO CALLERS, ONE VERB STRING, and it is not obviously the right one. This
/// wrapper is `qd send:relay <codex-row>`; the unified `qd send` reaches the
/// same core through `LaneOps::deliver`'s codex/daemon arm, and BOTH stamp
/// `send:relay`. That is what the pre-move body hard-coded, so it is preserved —
/// a user who typed `qd send` still reads a line naming `send:relay`. Reported
/// as a finding rather than fixed: correcting it moves bytes that a dozen pinned
/// tests read, and it is the same class of bug `ReviveClaudeError` documents.
pub(super) fn run_codex_send(session: &Session, message: &str) -> CarrierOutcome {
    let env = RealEnv;
    let clock = RealClock;
    // The caller mints the send id now that `SendParams` carries one (see
    // `quorum_qw::contract::Message::id`). NO intent record is written for it:
    // a resident carrier keys its `send-initiated` on the TURN id its resident
    // answered with, so a qd record under this id would correlate with nothing,
    // and `send:relay` is outside `qd delivery:recover`'s verb gate by
    // construction anyway. The field is filled honestly rather than left blank
    // so no carrier can be handed an id that is not one.
    let send_id = dispatch::events::mint_send_id(&clock);
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return CarrierOutcome::unkeyed(code),
    };
    let deps = delivery::SendDeps {
        env: &env,
        paths: &paths,
        clock: &clock,
    };
    delivery::render(
        delivery::codex::send_codex(&deps, &delivery::SendParams {
            session,
            message,
            send_id: &send_id,
        }),
        "send:relay",
    )
}

/// The `acp_daemon` carrier's qd half — BOTH acp lanes. See
/// [`quorum_qw::delivery::acp::send_acp`] for the residence send path, the
/// transport-loss disposition and the four identity-preservation sites.
///
/// VERB-LAYER ADAPTER ONLY: `map_err` + `eprintln!`. Note that the
/// identity-preservation line is a NOTE rather than an error — it is printed
/// BEFORE the refusal it precedes, exactly where the pre-move body wrote it.
pub(super) fn run_acp_send(session: &Session, message: &str) -> CarrierOutcome {
    let env = RealEnv;
    let clock = RealClock;
    // The caller mints the send id now that `SendParams` carries one (see
    // `quorum_qw::contract::Message::id`). NO intent record is written for it:
    // a resident carrier keys its `send-initiated` on the TURN id its resident
    // answered with, so a qd record under this id would correlate with nothing,
    // and `send:relay` is outside `qd delivery:recover`'s verb gate by
    // construction anyway. The field is filled honestly rather than left blank
    // so no carrier can be handed an id that is not one.
    let send_id = dispatch::events::mint_send_id(&clock);
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return CarrierOutcome::unkeyed(code),
    };
    let deps = delivery::SendDeps {
        env: &env,
        paths: &paths,
        clock: &clock,
    };
    delivery::render(
        delivery::acp::send_acp(&deps, &delivery::SendParams {
            session,
            message,
            send_id: &send_id,
        }),
        "send:relay",
    )
}

/// The `pi_daemon` carrier's qd half — the resident, floor sub-lane included. See
/// [`quorum_qw::delivery::pi::send_pi`].
///
/// VERB-LAYER ADAPTER ONLY: resolve HOME and the process cwd (the floor's
/// one-shot child inherits it when the row records none — a library reading
/// `current_dir()` would be a hidden input), then `map_err` + `eprintln!`.
///
/// The floor sub-lane answers `echo_id: false`: it is a synchronous one-shot that
/// reports on stderr and has never printed an id, so
/// [`quorum_qw::delivery::render`] prints none.
pub(super) fn run_pi_send(session: &Session, message: &str) -> CarrierOutcome {
    let env = RealEnv;
    let clock = RealClock;
    // The caller mints the send id now that `SendParams` carries one (see
    // `quorum_qw::contract::Message::id`). NO intent record is written for it:
    // a resident carrier keys its `send-initiated` on the TURN id its resident
    // answered with, so a qd record under this id would correlate with nothing,
    // and `send:relay` is outside `qd delivery:recover`'s verb gate by
    // construction anyway. The field is filled honestly rather than left blank
    // so no carrier can be handed an id that is not one.
    let send_id = dispatch::events::mint_send_id(&clock);
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return CarrierOutcome::unkeyed(code),
    };
    let fallback_cwd =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let deps = delivery::pi::PiSendDeps {
        env: &env,
        paths: &paths,
        clock: &clock,
        fallback_cwd,
    };
    delivery::render(
        delivery::pi::send_pi(&deps, &delivery::SendParams {
            session,
            message,
            send_id: &send_id,
        }),
        "send:relay",
    )
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
/// Non-relay providers do NOT: they carry no relay port and must
/// take the full-scan fallback so `send:relay` routes to their own SEND ladder. Without
/// this exclusion the fast path matches such a row BY NAME and resolves a relay via PID
/// ANCESTRY (a claude relay up the spawn chain) — mis-delivering the send (caught live:
/// the first acp send returned a relay id, not a turn id). Pinned by
/// `daemon_providers_excluded_from_relay_fast_path` so the fix cannot silently regress.
///
/// **This is NOT `qd send`'s carrier selection and it did not go with it.** The
/// duplicated routing (`send_unified::select_carrier` and friends) is retired in
/// favour of `LaneOps::deliver`; this gates a DIFFERENT verb's registry
/// fast-LOOKUP filter (`qd send:relay`), which runs before any session is
/// resolved and therefore has no lane to ask. Deleting it re-opens F4.
///
/// **DRIFT CORRECTED (this change), because it was real.** The exclusion list was
/// `codex` and `acp/*` and nothing else, which left TWO providers that carry no
/// relay port inside the fast path:
///
///   - `pi` — both of its lanes. A pi resident's receive path is its ws endpoint
///     and a pi TUI's is its pane; neither is a relay, so a pi row matched here by
///     name could only ever resolve someone else's relay up the spawn chain. That
///     is the F4 shape exactly, one provider over.
///   - `opencode` — the CLI alias for `acp/opencode` (`Harness::from_provider_id`
///     accepts both, and `join.rs` emits rows carrying the bare token). The
///     `acp/` prefix test does not match it, so the alias walked straight past a
///     guard written for the thing it is an alias OF.
///
/// The rule is now stated as what it means — the relay is claude-code's carrier,
/// and only claude-code's — with every non-relay provider named.
fn provider_uses_relay_fast_path(provider: Option<&str>) -> bool {
    // An absent provider reads as claude-code at the boundary (relay-capable).
    let p = provider.unwrap_or("claude-code");
    p != "codex" && p != "pi" && p != "opencode" && !p.starts_with("acp/")
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
    delivery::emit_door_failure(&RealEnv, &RealClock, name, target_session, message, "no-relay");
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
    delivery::emit_door_failure(
        &RealEnv,
        &RealClock,
        name,
        target_session,
        message,
        "non-receivable-bare",
    );
    eprintln!("{}", bare_destination_message(name));
    1
}

/// §C1 — emit a single `send-failed` terminal at a send DOOR (best-effort).
///
/// The RELAY door's half of the emitter phase 3B moved into qw: the four daemon
/// doors call [`quorum_qw::delivery::emit_door_failure`] from inside their cores,
/// and `no_relay_exit` / `bare_destination_exit` call it from here, so every send
/// door — relay or resident — writes the SAME record (door-inventory §A/§B).

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
            hosting: None,
            which_branch: dispatch::model::SessionBranch::ZmxOnly,
        };

        let target_uuid = fallback_target_uuid(&session);
        assert_eq!(target_uuid, None, "an empty fallback id is unresolved");
        assert!(
            !is_self_send(Some("caller-uuid"), target_uuid.as_deref()),
            "an unresolved target must not trigger self-send rejection"
        );
    }

    // F4 regression guard: the relay fast-path daemon-exclusion (the live-surfaced
    // mis-route fix). Reverting `provider_uses_relay_fast_path` to allow codex/acp (or
    // dropping the `acp/` check) reds this — a non-vacuous guard for the no-regression
    // condition that previously had only the live turn-id-vs-relay-id discriminator.
    #[test]
    fn daemon_providers_excluded_from_relay_fast_path() {
        // The relay is claude-code's carrier and only claude-code's, so it is the
        // only row shape that participates.
        assert!(provider_uses_relay_fast_path(None), "absent → claude-code (relay)");
        assert!(provider_uses_relay_fast_path(Some("claude-code")));
        // Non-relay rows are EXCLUDED → they take the full-scan fallback.
        assert!(!provider_uses_relay_fast_path(Some("codex")), "codex is daemon-hosted");
        assert!(
            !provider_uses_relay_fast_path(Some("acp/claude-code")),
            "acp/* is daemon-hosted"
        );
        assert!(!provider_uses_relay_fast_path(Some("acp/anything")));
        // DRIFT CORRECTED. `pi` carries no relay port in EITHER lane (a resident's
        // ws endpoint; a TUI's pane), so a pi row inside the fast path could only
        // resolve someone ELSE's relay by ancestry — F4, one provider over.
        assert!(!provider_uses_relay_fast_path(Some("pi")), "pi has no relay in either lane");
        // …and `opencode` is the CLI ALIAS for `acp/opencode` (`join.rs` emits rows
        // carrying the bare token). It asserted TRUE here until this change,
        // walking past a guard written for the very thing it is an alias of.
        assert!(
            !provider_uses_relay_fast_path(Some("opencode")),
            "the bare `opencode` token is acp/opencode, and an ACP bridge has no relay"
        );
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

    use dispatch::effects::MapEnv;

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
    // REAL `quorum_qw::delivery::emit_relay_send_events` writes `send-initiated` (the reused
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
    /// `delivery::emit_relay_send_events` then writes exactly one `send-initiated`
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
            hosting: None,
            which_branch: dispatch::model::SessionBranch::LiveRegistry,
        };
        delivery::emit_relay_send_events(
            &env,
            &RealClock,
            "target-b",
            Some(&target),
            message,
            &message_id,
        );

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

    // === The SEND-seam landing check, ACROSS the two seams ====================
    //
    // The landing check's OWN units moved into `quorum_qw::delivery::pi` with the
    // function. This one stays: its subject is the pair — qw's send-seam
    // `confirm_landing` and `wait.rs`'s own copy of the observer — and it is what
    // proves they agree while the two exist. When `qd wait`'s pi observer moves
    // (the other half of phase 3B), it moves with them.

    /// Build a pi target whose rollout is `<home>/rollout.jsonl`, with `rollout`
    /// as its contents, and a `send-initiated` already in its delivery log for
    /// `msg` (what `emit_daemon_send_events` writes on the inject ACK).
    fn pi_landing_fixture(
        home: &std::path::Path,
        msg: &str,
        rollout: &str,
    ) -> (MapEnv, dispatch::paths::QdPaths, Session) {
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.to_string_lossy().to_string());
        let env = MapEnv { vars, uid: 501 };
        let paths = dispatch::paths::QdPaths::from_home_env(home, &env);
        let rollout_path = home.join("rollout.jsonl");
        std::fs::write(&rollout_path, rollout).unwrap();
        let target = Session {
            name: Some("pi-resident-1".to_string()),
            user_named: None,
            session_id: "cdcdcdcd-0101-2323-4545-676767676767".to_string(),
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
            jsonl_path: Some(rollout_path.to_string_lossy().to_string()),
            last_turns: None,
            provider: "pi".to_string(),
            hosting: None,
            entrypoint: None,
            lineage: None,
            which_branch: dispatch::model::SessionBranch::LiveRegistry,
        };
        // The inject-ACK records the observer joins against.
        delivery::emit_daemon_send_events(
            &env,
            &RealClock,
            "pi-resident-1",
            Some(&target),
            msg,
            "turn-1",
            "pi",
        );
        (env, paths, target)
    }

    fn pi_user_record(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::to_string(&serde_json::json!({
                "type": "message",
                "message": {"role": "user", "content": [{"type": "text", "text": text}]}
            }))
            .unwrap()
        )
    }

    /// `delivery::pi::confirm_landing` with a ZERO window — this exercises the
    /// content key and the emission, never the polling clock.
    fn landed(env: &MapEnv, paths: &dispatch::paths::QdPaths, target: &Session, msg: &str) -> bool {
        delivery::pi::confirm_landing(
            env,
            &RealClock,
            paths,
            target,
            msg,
            Duration::from_millis(0),
        )
    }

    fn terminals_in_log(paths: &dispatch::paths::QdPaths, uuid: &str) -> Vec<String> {
        let raw = std::fs::read_to_string(dispatch::events::events_path(&paths.state_dir, uuid))
            .unwrap_or_default();
        raw.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v["event"].as_str().map(str::to_owned))
            .filter(|e| dispatch::events::is_terminal(e))
            .collect()
    }

    /// Idempotent across the two seams: the send seam emits, and a later wait-seam
    /// sweep over the same landed send adds NOTHING (first-terminal-wins).
    ///
    /// Its subject SURVIVED the convergence and narrowed to the thing that was
    /// always load-bearing. It used to drive two IMPLEMENTATIONS — this one and
    /// `qd wait`'s field-for-field copy — and prove they agreed. Ruling D2's
    /// `await_idle` moved the wait arm into `quorum_qw::idle`, which now calls
    /// `delivery::pi::observe_landed_sends` directly, so the copy is gone and
    /// "they agree" is true by construction. What is NOT true by construction, and
    /// is what this still proves, is that the two SEAMS firing over one landed
    /// send produce exactly ONE terminal: that rests on first-terminal-wins in the
    /// ledger, which no amount of sharing an implementation would give you.
    #[test]
    fn send_and_wait_seams_never_double_emit() {
        let home = tempfile::tempdir().unwrap();
        let msg = "landed exactly once";
        let (env, paths, target) = pi_landing_fixture(home.path(), msg, &pi_user_record(msg));
        assert!(landed(&env, &paths, &target, msg));
        // The WAIT seam runs over the same rollout afterwards — the very call
        // `idle::await_idle_pi` makes at every release.
        delivery::pi::observe_landed_sends(&env, &RealClock, &paths, &target);
        assert_eq!(
            terminals_in_log(&paths, &target.session_id),
            vec!["message-seen".to_string()],
            "exactly one terminal, whichever seam gets there first"
        );
    }
}
