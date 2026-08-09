//! Unified `qd send <target> <message>` selection and dispatch.
//!
//! Target resolution and carrier selection are deliberately separate. The
//! resolver produces one concrete session identity; the pure selector consumes
//! only that row's observable state; the dispatcher receives the same row and
//! never resolves a name, prefix, or PID on its own.

use clap::ArgMatches;

use dispatch::effects::{Env, RealEnv};
use dispatch::idstore::IdMap;
use dispatch::launch::RenderMode;
use dispatch::model::{Session, SessionStatus};
use dispatch::origin_send::Refusal;

use super::{common, lifecycle, resume, send, send_relay};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnifiedCarrier {
    ClaudeRelay { port: u16 },
    MuxPty,
    CodexDaemon,
    AcpDaemon,
    PiDaemon,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SendRefusal {
    Bare,
    NoLiveReceivePath,
    UnknownProvider(String),
}

/// A target's lifecycle liveness, from its one resolved registry/join snapshot.
/// A NOT-live target (`Cold`/`Killed`) is no longer a send refusal (qd–qf W3b:
/// "stopped is not a refusal class") — it is a WAKE trigger: the unified send
/// path revives it via [`wake_to_deliverable`] and delivers into the refreshed
/// row. Only a LIVE target reaches [`select_carrier`].
fn is_live(session: &Session) -> bool {
    matches!(
        session.status,
        SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Shell
    )
}

/// Pure pre-attempt selector. There is intentionally no probing or discovery
/// here: relay presence, mux linkage, and provider all come from the one resolved
/// registry/join snapshot.
///
/// qd–qf W3b: lifecycle liveness is NO LONGER gated here — the Cold/Stopped
/// refusal arms are RETIRED. The unified send path wakes a not-live target
/// ([`wake_to_deliverable`]) BEFORE it calls this, so `select_carrier` only ever
/// runs on a live (or freshly-revived) row. The remaining `NoLiveReceivePath`
/// arms are the transport-shape refusals (a live claude with neither relay nor a
/// joined mux pane; a pane-hosted codex with no live pane) — a genuinely-bare
/// receive surface, distinct from a stopped session. As a defense-in-depth floor
/// (a caller that reaches this on a not-live row) a non-live status also yields
/// `NoLiveReceivePath` rather than routing into a carrier that cannot receive.
fn select_carrier(session: &Session) -> Result<UnifiedCarrier, SendRefusal> {
    if session.session_id.is_empty() {
        return Err(SendRefusal::Bare);
    }
    if !is_live(session) {
        // Floor only — the unified path wakes a not-live target before selecting.
        return Err(SendRefusal::NoLiveReceivePath);
    }

    match session.provider.as_str() {
        // codex-interactive: a codex row is only an app-server row when it is
        // DAEMON-hosted. The `--interactive` lane has no ws endpoint to reconnect
        // to — its receive path is the pane's PTY, the same carrier a
        // relay-less claude pane uses. Routing it to `CodexDaemon` would fail on a
        // missing endpoint and blame the transport for a session that never had
        // one.
        //
        // What the PTY carrier does for codex today is deliberately conservative:
        // the attended-send machinery has landed codex composer facts
        // (`qrmux::attended::fire::CodexFacts`) but codex still exposes no pollable
        // busy/idle signal, so acceptance is not confirmable and the fire gates
        // itself OFF before touching the composer — an honest non-delivery rather
        // than an unverifiable claim. That is the correct answer to give here, and
        // it improves on its own the day codex grows a confirmable signal.
        "codex"
            if dispatch::provider::row_hosting(&session.provider, session.hosting.as_deref())
                == Some(dispatch::provider::Hosting::MuxPane) =>
        {
            if session.zmx_name.is_some() && session.socket_dir.is_some() {
                Ok(UnifiedCarrier::MuxPty)
            } else {
                Err(SendRefusal::NoLiveReceivePath)
            }
        }
        "codex" => Ok(UnifiedCarrier::CodexDaemon),
        provider if provider.starts_with("acp/") => Ok(UnifiedCarrier::AcpDaemon),
        "pi" => Ok(UnifiedCarrier::PiDaemon),
        // Relay precedence is structural: a recorded port selects relay before
        // mux state is considered. PTY can only be selected from a positive
        // relay_port=None observation plus a live joined mux pane.
        "claude-code" => match session.relay_port {
            Some(port) => Ok(UnifiedCarrier::ClaudeRelay { port }),
            None if session.zmx_name.is_some() && session.socket_dir.is_some() => {
                Ok(UnifiedCarrier::MuxPty)
            }
            None => Err(SendRefusal::NoLiveReceivePath),
        },
        other => Err(SendRefusal::UnknownProvider(other.to_string())),
    }
}

/// qd–qf W3b — the WAKE seam. A NOT-live target is revived into a deliverable
/// (live) row; on success the refreshed [`Session`] (new pid/endpoint, SAME
/// session id) is handed back so the caller re-runs carrier selection + delivery
/// against it. A wake that cannot succeed is a [`Refusal::failed`]`("wake", …)` —
/// the contract's `failed{wake}` (exit 12). Seamed as a trait so the
/// [`wake_then_deliver`] durability wiring is unit-testable with a mock that
/// returns Ok(refreshed) / Err(failed{wake}) without standing up a live revive.
trait Waker {
    fn wake(&self, session: &Session, render: RenderMode) -> Result<Session, Refusal>;
}

/// The production [`Waker`]: dispatch to the matching REUSED revive machinery by
/// provider + hosting. Nothing here re-implements a revive — it calls the SAME
/// fns `qd resume` / `qd attach` run:
///   - claude-code MuxPane  → [`resume::revive_claude`] (`fresh=false`, detached),
///   - codex MuxPane        → [`lifecycle::revive_codex_tui`] (verb `"send"`),
///   - codex / acp/* / pi daemon → the matching `run_*_resume` (they print their
///     own "resumed …" line and return an exit code; a nonzero is a wake failure).
///
/// On success the row is re-resolved by its STABLE `session_id` (never a name /
/// prefix — the revive rewrote the registry row under the same id). A revive that
/// cannot succeed, an unknown/​un-wakeable provider, or a row that vanishes after a
/// "successful" revive all map to `failed{wake}`.
struct RealWaker;

impl Waker for RealWaker {
    fn wake(&self, session: &Session, render: RenderMode) -> Result<Session, Refusal> {
        use dispatch::provider::{row_hosting, Hosting};

        let label = session
            .name
            .clone()
            .unwrap_or_else(|| session.session_id.clone());
        let provider = session.provider.as_str();
        let hosting = row_hosting(provider, session.hosting.as_deref());

        // Re-resolve the refreshed row by the STABLE session id after a revive that
        // reported success (new pid/endpoint, same id). A vanished row is itself a
        // wake failure — the revive claimed success but left nothing to deliver to.
        let refreshed = |session_id: &str| -> Result<Session, Refusal> {
            common::resolve_session_uncapped(session_id).map_err(|_| {
                Refusal::failed(
                    "wake",
                    format!("revived \"{label}\" but its session row vanished before delivery"),
                )
            })
        };

        match (provider, hosting) {
            // claude-code pane — the shared cold→drivable claude revive (fresh=false
            // ⇒ resume the EXISTING session id, not a new one). Detached +
            // ready-gated; errors-only print. Ok(_) ⇒ re-resolve; Err ⇒ failed{wake}.
            ("claude-code", _) => match resume::revive_claude(session, None, render, false) {
                Ok(_) => refreshed(&session.session_id),
                Err(_) => Err(Refusal::failed(
                    "wake",
                    format!("could not revive claude session \"{label}\""),
                )),
            },
            // codex, pane-hosted (--interactive) — the codex twin of revive_claude.
            ("codex", Some(Hosting::MuxPane)) => {
                match lifecycle::revive_codex_tui(session, render, "send") {
                    Ok(_) => refreshed(&session.session_id),
                    Err(_) => Err(Refusal::failed(
                        "wake",
                        format!("could not revive codex session \"{label}\""),
                    )),
                }
            }
            // codex daemon — the app-server revive (`thread/resume`). It prints its
            // own success line; exit 0 ⇒ re-resolve, nonzero ⇒ failed{wake}.
            ("codex", _) => match resume::run_codex_resume(session) {
                0 => refreshed(&session.session_id),
                _ => Err(Refusal::failed(
                    "wake",
                    format!("could not revive codex daemon session \"{label}\""),
                )),
            },
            // acp/* daemon — the resident adapter revive (`session/load`).
            (p, _) if p.starts_with("acp/") => match resume::run_acp_resume(session) {
                0 => refreshed(&session.session_id),
                _ => Err(Refusal::failed(
                    "wake",
                    format!("could not revive acp session \"{label}\""),
                )),
            },
            // pi daemon — the resident revive (`--load-session <id>`).
            ("pi", _) => match resume::run_pi_resume(session) {
                0 => refreshed(&session.session_id),
                _ => Err(Refusal::failed(
                    "wake",
                    format!("could not revive pi session \"{label}\""),
                )),
            },
            // No headless wake route for this provider/hosting.
            _ => Err(Refusal::failed(
                "wake",
                format!("provider \"{provider}\" cannot be woken headlessly"),
            )),
        }
    }
}

trait UnifiedBackend {
    fn claude_relay(&self, session: &Session, message: &str, port: u16) -> i32;
    fn mux_pty(&self, session: &Session, message: &str) -> i32;
    fn codex_daemon(&self, session: &Session, message: &str) -> i32;
    fn acp_daemon(&self, session: &Session, message: &str) -> i32;
    fn pi_daemon(&self, session: &Session, message: &str) -> i32;
}

struct RealUnifiedBackend;

impl UnifiedBackend for RealUnifiedBackend {
    fn claude_relay(&self, session: &Session, message: &str, port: u16) -> i32 {
        send_relay::run_claude_relay_unified(session, message, port)
    }

    fn mux_pty(&self, session: &Session, message: &str) -> i32 {
        send::run_send_pty_unified(session, message)
    }

    fn codex_daemon(&self, session: &Session, message: &str) -> i32 {
        send_relay::run_codex_send(session, message)
    }

    fn acp_daemon(&self, session: &Session, message: &str) -> i32 {
        send_relay::run_acp_send(session, message)
    }

    fn pi_daemon(&self, session: &Session, message: &str) -> i32 {
        send_relay::run_pi_send(session, message)
    }
}

fn dispatch_selected(
    backend: &dyn UnifiedBackend,
    carrier: UnifiedCarrier,
    session: &Session,
    message: &str,
) -> i32 {
    // Unified-send decision table (selection is complete before this match):
    //
    //   codex, daemon-hosted          -> codex daemon lane
    //   codex, pane-hosted (--interactive) -> PTY (no ws endpoint exists)
    //   acp/*                         -> ACP daemon lane
    //   pi                            -> pi daemon lane
    //   claude-code + relay_port      -> relay (wins even with a live mux pane)
    //   claude-code + no relay + mux  -> PTY spare tire
    //   anything else                 -> refused before dispatch
    //
    // Every arm makes exactly one carrier call and returns its result. There is
    // no cross-carrier fallback after any carrier's acceptance boundary.
    match carrier {
        UnifiedCarrier::ClaudeRelay { port } => {
            backend.claude_relay(session, message, port)
        }
        UnifiedCarrier::MuxPty => backend.mux_pty(session, message),
        UnifiedCarrier::CodexDaemon => backend.codex_daemon(session, message),
        UnifiedCarrier::AcpDaemon => backend.acp_daemon(session, message),
        UnifiedCarrier::PiDaemon => backend.pi_daemon(session, message),
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

fn report_refusal(query: &str, session: &Session, refusal: SendRefusal) -> i32 {
    let label = session.name.as_deref().unwrap_or(query);
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
        return run_inbound(&RealEnv, &RealUnifiedBackend, &RealWaker, path);
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
        return report_refusal(query, &target, SendRefusal::Bare);
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
    let current = match common::resolve_session_uncapped(&target.session_id) {
        Ok(session) if session.session_id == target.session_id => session,
        Ok(_) => {
            eprintln!("qd send: target identity changed before delivery — refusing to send.");
            return 1;
        }
        Err(_) => {
            eprintln!("qd send: target disappeared before delivery — refusing to send.");
            return 1;
        }
    };

    // qd–qf W3b: the LIVE vs NOT-live split. A LIVE target takes the byte-identical
    // W3a path — select the carrier FIRST (a transport-shape refusal is an
    // immediate exit-1 with NO envelope logged, exactly as today), then
    // write-then-deliver. A NOT-live target is no longer refused: it is
    // resume-and-deliver — log the envelope FIRST, WAKE it, then select + deliver
    // into the refreshed row (a wake that cannot succeed is a `failed{wake}`
    // stamped against the logged envelope, exit 12).
    if is_live(&current) {
        let carrier = match select_carrier(&current) {
            Ok(carrier) => carrier,
            Err(refusal) => return report_refusal(query, &current, refusal),
        };
        // qd–qf W3 part A: WRITE-THEN-DELIVER. Log the envelope BEFORE delivery
        // (hard-fail if the append errors), stamp `attempted`, deliver via the
        // existing unified carrier, then stamp the witnessed outcome (best-effort).
        deliver_with_durability(
            &env,
            &paths,
            &RealUnifiedBackend,
            carrier,
            &current,
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
        let render = dispatch::launch::resolve_render_mode(
            None,
            dispatch::launch::render_default_from_config(&env).as_deref(),
        );
        wake_then_deliver(
            &env,
            &paths,
            &RealUnifiedBackend,
            &RealWaker,
            render,
            &current,
            query,
            message,
            expires_ms,
            supplied_correlation_id,
        )
    }
}

// ===========================================================================
// qd–qf W4 — INBOUND MODE ("THE ONE DOOR")
// ===========================================================================

/// qd–qf W6 — split a raw address into `(name, host)` on the LAST `@`.
///
/// `name@host` is SUGAR over `--host` (TRANSITION §3 / §7 Q2 RULED): the address
/// `"alpha@brano"` ⇒ `("alpha", Some("brano"))`; a bare `"alpha"` (or a stable_id,
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
///      and is WOKEN (reuse the W3b [`Waker`] wake path) before delivery; a wake
///      that cannot succeed stamps `delivery-failed{class}` (exit 12). A live but
///      carrierless select_carrier refusal stamps `refused{no-live-receive-path}`
///      (NO `attempted` — the R12 family split, now a refused row) + refusal exit.
///      NO envelope log append (contract §4).
///   7. STAMP the outcome (`delivered` / `delivery-failed{delivery}`) via the
///      SHARED [`deliver_then_stamp`] tail — best-effort append.
///
/// Seamed (deps injected — `env`/`backend`/`waker`) so the whole door is proven
/// with mocks + a jailed store, no live carrier/revive.
fn run_inbound(
    env: &dyn Env,
    backend: &dyn UnifiedBackend,
    waker: &dyn Waker,
    envelope_arg: &str,
) -> i32 {
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

    // A not-live target is WOKEN first (reuse the W3b wake seam), then delivered;
    // a live target is delivered directly. NO envelope log append either way.
    if is_live(&target) {
        let carrier = match select_carrier(&target) {
            Ok(c) => c,
            // R14.3: a live-but-carrierless target is a parse-valid inbound refusal
            // — stamp `refused{no-live-receive-path}` IN the funnel (NO `attempted`,
            // the message never admitted) + the refusal exit. The old `report_refusal`
            // (a row-less exit-1) is replaced here for the inbound door.
            Err(_refusal) => {
                let label = target.name.as_deref().unwrap_or(&envelope.target);
                return stamp_refused(
                    Refusal::refused(
                        "no-live-receive-path",
                        format!("\"{label}\" has no live receive path — not sendable"),
                    ),
                    "no-live-receive-path",
                );
            }
        };
        stamp_attempted();
        deliver_then_stamp(
            &tpaths,
            backend,
            carrier,
            &target,
            &envelope.body,
            &envelope.correlation_id,
            &clock,
        )
    } else {
        // Flag-less render (the `send` verb has no --alt-screen/--inline): config
        // render-default > the inline default (exactly the origin not-live path).
        let render = dispatch::launch::resolve_render_mode(
            None,
            dispatch::launch::render_default_from_config(env).as_deref(),
        );
        // On a wake failure, stamp `delivery-failed{class}` against the ENVELOPE
        // (created_at = now; the peer's origin/authored_at join from the mirror) +
        // exit 12 — the SAME contract as W3b, but with NO envelope log append.
        let stamp_failed_wake = |refusal: Refusal| -> i32 {
            stamp_event(
                &tpaths,
                &dispatch::dispositions::DispositionEvent::delivery_failed(
                    envelope.correlation_id.clone(),
                    clock.now_ms(),
                    refusal.class.clone(),
                ),
            );
            refusal.emit()
        };
        // The attempt starts, and it placed the message durably awaiting the
        // target's WAKE: `attempted` then `queued`, BEFORE the wake is tried.
        stamp_attempted();
        stamp_event(
            &tpaths,
            &dispatch::dispositions::DispositionEvent::queued(
                envelope.correlation_id.clone(),
                clock.now_ms(),
            ),
        );
        let (refreshed, carrier) = match waker.wake(&target, render) {
            Ok(refreshed) => match select_carrier(&refreshed) {
                Ok(carrier) => (refreshed, carrier),
                Err(_) => {
                    let label = refreshed
                        .name
                        .clone()
                        .unwrap_or_else(|| refreshed.session_id.clone());
                    return stamp_failed_wake(Refusal::failed(
                        "wake",
                        format!("revived \"{label}\" but it has no live receive path"),
                    ));
                }
            },
            Err(refusal) => return stamp_failed_wake(refusal),
        };
        deliver_then_stamp(
            &tpaths,
            backend,
            carrier,
            &refreshed,
            &envelope.body,
            &envelope.correlation_id,
            &clock,
        )
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

/// qd–qf W3 part A — the write-then-deliver + event-stamp wrapper around the
/// existing unified carrier dispatch. Kept as a seamed helper (deps injected)
/// so the log-append / event-stamp shape is exercised without standing up a
/// full live carrier: the `backend` is any [`UnifiedBackend`], `env`/`paths` are
/// the resolved seams.
///
/// Ordering (format doc §1/§2): LOG the envelope, stamp `attempted`, THEN
/// deliver, THEN stamp the outcome. The envelope append is fatal-on-error (no
/// durable record ⇒ do not deliver); the event appends are best-effort (a lost
/// event row never changes the exit). A synchronous local attempt that
/// completes is `delivered` (exit 0) or `delivery-failed{delivery}` (nonzero);
/// `pending`/`expired` are DERIVED (absence) and never stamped here.
#[allow(clippy::too_many_arguments)]
fn deliver_with_durability(
    env: &dyn Env,
    paths: &dispatch::paths::QdPaths,
    backend: &dyn UnifiedBackend,
    carrier: UnifiedCarrier,
    session: &Session,
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

    // R15 CLAIM LOCK — held across check→(log)→deliver→stamp (the `_claim` guard
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
    //     redeliver into the outcome tail.
    // Absent from the log ⇒ the normal write-then-deliver path (append below).
    let presented_digest = dispatch::origin_send::body_digest(message);
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
            Ok(None) => {
                // Legit caller retry: do NOT append the envelope again; stamp a
                // fresh `attempted` and redeliver via the shared outcome tail.
                stamp_event(
                    &tpaths,
                    &dispatch::dispositions::DispositionEvent::attempted(
                        correlation_id.clone(),
                        clock.now_ms(),
                    ),
                );
                return deliver_then_stamp(
                    &tpaths,
                    backend,
                    carrier,
                    session,
                    message,
                    &correlation_id,
                    &clock,
                );
            }
            Err(e) => {
                eprintln!("qd send: could not read the disposition ledger for {correlation_id} ({e}) — not sent.");
                return 1;
            }
        }
    }

    // Mint + LOG FIRST (write-then-deliver). `target` is the RAW address the
    // caller gave (operational record); `body` is the message verbatim.
    let envelope = build_envelope(
        correlation_id.clone(),
        authored_at,
        expires_ms,
        raw_target.to_string(),
        origin.clone(),
        message.to_string(),
    );
    if let Err(e) = dispositions::append_envelope(&tpaths, &envelope) {
        // HARD FAIL: no durable envelope ⇒ we must not proceed to deliver. Nothing
        // was sent; the caller gets a clear error + a nonzero exit (generic class).
        eprintln!(
            "qd send: could not durably record the message before delivery ({e}) — not sent."
        );
        return 1;
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

    // Deliver via the existing unified carrier + stamp the outcome. (This is the
    // LIVE path — a not-live target's resume-and-deliver / failed{wake} lives in
    // `wake_then_deliver`.) The deliver + outcome-stamp tail is the SHARED
    // `deliver_then_stamp` core (identical to the not-live and W4 inbound tails):
    // exit 0 ⇒ `delivered`; a definitive failure ⇒ `delivery-failed{delivery}`.
    deliver_then_stamp(
        &tpaths,
        backend,
        carrier,
        session,
        message,
        &correlation_id,
        &clock,
    )
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

/// qd–qf W3/W4 — the SHARED deliver → stamp-OUTCOME tail (NO log append, and NO
/// `attempted` emission — CALLERS own the attempt-start event). The envelope is
/// ALREADY durable (origin logged it; inbound never logs its own). One carrier
/// call, then a best-effort outcome [`dispatch::dispositions::DispositionEvent`]:
///   - exit 0             ⇒ `delivered`,
///   - definitive nonzero ⇒ `delivery-failed{delivery}`.
///
/// R14.2: event rows are FULLY NORMALIZED — the outcome row carries ONLY
/// `{v, correlation_id, event, created_at}` (+ `class` on the failed variant,
/// `body_digest` on delivered — R15). `created_at` = when THIS host recorded the
/// outcome (observation time, R14.1); there is NO `witness`/`origin`/`authored_at`
/// on the row (they live on the envelope and join by `correlation_id`).
///
/// R15: on success the `delivered` row binds `body_digest(message)` — the hex
/// sha-256 of the body that ACTUALLY landed (the exact `message` string handed to
/// the carrier). This is the integrity binding the door reads back to refuse a
/// later same-id/different-body presentation. Used by the origin live path, the
/// origin resume-and-deliver path, AND W4 inbound — so the three cannot drift.
fn deliver_then_stamp(
    tpaths: &dispatch::paths::QdPaths,
    backend: &dyn UnifiedBackend,
    carrier: UnifiedCarrier,
    session: &Session,
    message: &str,
    correlation_id: &str,
    clock: &dyn dispatch::effects::Clock,
) -> i32 {
    use dispatch::dispositions::DispositionEvent;

    let code = dispatch_selected(backend, carrier, session, message);

    let event = if code == 0 {
        DispositionEvent::delivered(
            correlation_id.to_string(),
            clock.now_ms(),
            dispatch::origin_send::body_digest(message),
        )
    } else {
        DispositionEvent::delivery_failed(
            correlation_id.to_string(),
            clock.now_ms(),
            "delivery".to_string(),
        )
    };
    stamp_event(tpaths, &event);
    code
}

/// qd–qf W3b — the RESUME-AND-DELIVER path for a NOT-live target. "Stopped is not
/// a refusal class": the envelope is LOGGED FIRST (write-then-deliver — hard-fail
/// if the append errors), THEN the target is WOKEN, THEN delivered into the
/// refreshed row.
///
/// Ordering (contract §4, format doc §1/§2), all inside the durability boundary:
///   1. LOG the envelope (fatal-on-error — no durable record ⇒ do not proceed),
///      then stamp `attempted` and `queued` — the attempt placed the message
///      durably awaiting the target's WAKE, a witnessed moment stamped BEFORE
///      the wake is tried;
///   2. WAKE via the [`Waker`] seam. On `Err(failed{wake})` the wake could not
///      succeed → stamp a `delivery-failed{wake}` event against the logged
///      envelope, print the refusal, and return [`EXIT_REFUSED`] (12). Nothing
///      was delivered (and a later retry is NOT blocked — idempotence keys on
///      `delivered` existing).
///   3. On `Ok(refreshed)` re-select the carrier for the refreshed (now live) row.
///      A revive that reported success but left an unroutable row is itself a wake
///      failure → the SAME `delivery-failed{wake}` stamp + exit 12 (never a
///      silent no-op).
///   4. DELIVER via the carrier, then STAMP `delivered`/`delivery-failed{delivery}`
///      — the identical outcome wiring the live path uses.
///
/// Seamed (deps injected — `backend`/`waker`/`env`/`paths`) so the log → stamp →
/// wake → select → deliver → stamp shape is proven with mocks (no live
/// carrier/revive).
#[allow(clippy::too_many_arguments)]
fn wake_then_deliver(
    env: &dyn Env,
    paths: &dispatch::paths::QdPaths,
    backend: &dyn UnifiedBackend,
    waker: &dyn Waker,
    render: RenderMode,
    session: &Session,
    raw_target: &str,
    message: &str,
    expires_ms: i64,
    supplied_correlation_id: Option<String>,
) -> i32 {
    use dispatch::dispositions::{self, DispositionEvent};
    use dispatch::effects::{Clock, RealClock};
    use dispatch::origin_send::{build_envelope, mint_correlation_id};

    // Same transport-file resolution + minting as the live path (from_home_env
    // honors QD_HOME, matching the store + the W5 reader).
    let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, env);
    let clock = RealClock;
    let authored_at = clock.now_ms();
    // qd–qf W3c: the supplied id (frame's origin event id) also threads the
    // resume-and-deliver path — it shares the SAME origin envelope, so the logged
    // envelope AND every stamped event key on it. Absent ⇒ mint (the BARE-send
    // default). Empty was already refused at the verb entry.
    let correlation_id = supplied_correlation_id.unwrap_or_else(|| mint_correlation_id(&clock));
    // This qd ORIGINATES here: local_host is the envelope's `origin` (the single
    // normalized HOME of origin, R14.2). Event rows no longer carry origin/witness.
    let origin = dispositions::local_host(env);

    // R15 CLAIM LOCK — held across check→(log)→wake→deliver→stamp. Serializes
    // concurrent same-id submits + the body-consistency check. Fail CLOSED.
    let _claim = match dispositions::acquire_claim(&tpaths, &correlation_id) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("qd send: could not acquire the delivery claim for {correlation_id} ({e}) — not sent.");
            return 1;
        }
    };

    // R15 ORIGIN no-double-append (same rule as the live path): if this id is
    // ALREADY in my log, the caller is re-submitting the SAME authored act.
    //   - DIFFERENT body ⇒ sync `refused{body-mismatch}`, ROW-LESS (R14a pin 3).
    //   - SAME body + delivered exists ⇒ no-op success.
    //   - SAME body + not delivered ⇒ legit caller retry: do NOT re-append the
    //     envelope; fall through with `skip_append` and re-run the wake+deliver.
    let presented_digest = dispatch::origin_send::body_digest(message);
    let mut skip_append = false;
    if let Some(prior) = dispositions::logged_envelope(&tpaths, &correlation_id) {
        if dispatch::origin_send::body_digest(&prior.body) != presented_digest {
            return dispatch::origin_send::Refusal::refused(
                "body-mismatch",
                format!(
                    "{correlation_id} is already in the log with a different body — refusing to re-submit a conflicting body under the same id"
                ),
            )
            .emit();
        }
        match dispositions::recorded_delivered_digest(&tpaths, &correlation_id) {
            Ok(Some(_)) => {
                eprintln!("qd send: {correlation_id} already delivered — no-op");
                return 0;
            }
            // Same body, not delivered: a legit retry ⇒ redeliver, no re-append.
            Ok(None) => skip_append = true,
            Err(e) => {
                eprintln!("qd send: could not read the disposition ledger for {correlation_id} ({e}) — not sent.");
                return 1;
            }
        }
    }

    // (1) LOG FIRST — even a wake that later fails leaves the durable envelope, so
    // a `delivery-failed{wake}` event has an envelope to join on
    // (write-then-deliver). A caller-retry (`skip_append`) reuses the envelope
    // already in the log — never a double-append.
    let envelope = build_envelope(
        correlation_id.clone(),
        authored_at,
        expires_ms,
        raw_target.to_string(),
        origin.clone(),
        message.to_string(),
    );
    if !skip_append {
        if let Err(e) = dispositions::append_envelope(&tpaths, &envelope) {
            eprintln!(
                "qd send: could not durably record the message before delivery ({e}) — not sent."
            );
            return 1;
        }
    }

    // The delivery attempt STARTS here: stamp `attempted`, then `queued` — the
    // attempt placed the message durably awaiting the target's WAKE (a recorded
    // moment, stamped BEFORE the wake is tried; the prose may land minutes from
    // now). Normalized rows (R14.2): `{v, correlation_id, event, created_at}` only.
    stamp_event(
        &tpaths,
        &DispositionEvent::attempted(correlation_id.clone(), clock.now_ms()),
    );
    stamp_event(
        &tpaths,
        &DispositionEvent::queued(correlation_id.clone(), clock.now_ms()),
    );

    // A wake that cannot succeed: stamp a `delivery-failed{class}` event against
    // the logged envelope (best-effort — a lost event must not change the exit),
    // print the refusal, exit 12. NOT a verdict on the id — a later retry
    // re-attempts (idempotence keys on `delivered` EXISTING, never on a failure).
    let stamp_failed_wake = |refusal: Refusal| -> i32 {
        stamp_event(
            &tpaths,
            &DispositionEvent::delivery_failed(
                correlation_id.clone(),
                clock.now_ms(),
                refusal.class.clone(),
            ),
        );
        refusal.emit()
    };

    // (2) WAKE. (3) On success, re-select the carrier for the refreshed row — an
    // unroutable refreshed row is a wake that did not produce a deliverable target.
    let (refreshed, carrier) = match waker.wake(session, render) {
        Ok(refreshed) => match select_carrier(&refreshed) {
            Ok(carrier) => (refreshed, carrier),
            Err(_) => {
                let label = refreshed
                    .name
                    .clone()
                    .unwrap_or_else(|| refreshed.session_id.clone());
                return stamp_failed_wake(Refusal::failed(
                    "wake",
                    format!("revived \"{label}\" but it has no live receive path"),
                ));
            }
        },
        Err(refusal) => return stamp_failed_wake(refusal),
    };

    // (4) DELIVER into the refreshed row + STAMP the outcome (the SHARED
    // `deliver_then_stamp` tail — identical to the live + inbound paths).
    deliver_then_stamp(
        &tpaths,
        backend,
        carrier,
        &refreshed,
        message,
        &correlation_id,
        &clock,
    )
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
        assert_eq!(parse_address("alpha@brano"), ("alpha", Some("brano")));
        // Split on the LAST '@' (defensive — real names/ids carry no '@', but the
        // rule is well-defined if one somehow appears).
        assert_eq!(parse_address("a@b@brano"), ("a@b", Some("brano")));
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
        let env = env_host("brano");
        let r = resolve_target("name", Some(""), &env).unwrap_err();
        assert_eq!(r.family, dispatch::origin_send::Family::Refused);
        assert_eq!(r.class, "host");
    }

    #[test]
    fn resolve_target_empty_name_is_refused_address() {
        // "@host" ⇒ empty name ⇒ refused{address} (nothing to resolve). Here the
        // host equals local so we pass the host gate and hit the empty-name gate.
        let env = env_host("brano");
        let r = resolve_target("", Some("brano"), &env).unwrap_err();
        assert_eq!(r.family, dispatch::origin_send::Family::Refused);
        assert_eq!(r.class, "address");
    }

    #[test]
    fn resolve_target_foreign_host_is_refused_no_fleet_state() {
        // A host-qualified address for a host that is NOT this host, on a
        // single-machine box (no remote/<h>/) ⇒ fail-closed refused{no-fleet-state}.
        // local_host = "brano" (QD_HOST), target host "elsewhere" ≠ local.
        let env = env_host("brano");
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
    fn resolve_target_default_local_host_is_local() {
        // With QD_HOST unset, local_host == "local", so `@local` is treated as
        // this host — it must NOT hit the no-fleet-state refusal (it falls through
        // to local resolution). We can't drive the real gather here, so assert the
        // COMPLEMENT: `@local` does not produce a host-class refusal. A DIFFERENT
        // host on the same default env DOES refuse (control).
        let env = dispatch::effects::MapEnv::default(); // QD_HOST unset ⇒ "local"
        // Foreign host still refuses (proves the gate is active under the default).
        let foreign = resolve_target("alpha", Some("brano"), &env).unwrap_err();
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

    // === codex-interactive: a codex row's carrier follows its HOSTING ===
    //
    // The two codex topologies have disjoint receive paths — the daemon has a ws
    // endpoint and no pane, the interactive lane has a pane and no endpoint — so
    // selecting on the provider id alone necessarily gets one of them wrong.

    #[test]
    fn pane_hosted_codex_selects_the_pty_carrier_not_the_daemon() {
        let mut s = session("codex");
        s.hosting = Some("mux-pane".into());
        assert_eq!(
            select_carrier(&s),
            Ok(UnifiedCarrier::MuxPty),
            "an --interactive codex row has no ws endpoint; its receive path is the pane"
        );
    }

    #[test]
    fn daemon_hosted_codex_still_selects_the_daemon_carrier() {
        // Both the explicit token and the absent field (every pre-existing codex
        // row) must keep the app-server lane — this is the regression guard for
        // the whole codex daemon fleet.
        let mut explicit = session("codex");
        explicit.hosting = Some("daemon".into());
        assert_eq!(select_carrier(&explicit), Ok(UnifiedCarrier::CodexDaemon));

        let absent = session("codex");
        assert_eq!(absent.hosting, None);
        assert_eq!(select_carrier(&absent), Ok(UnifiedCarrier::CodexDaemon));
    }

    #[test]
    fn unidentified_pane_hosted_codex_refuses_as_bare_not_as_a_daemon() {
        // The window between starting an interactive codex session and typing into
        // it: the row exists, the pane is live, but codex has disclosed no thread
        // id. It must refuse as Bare (no identity) — NOT get routed to the
        // app-server lane, and NOT be reported as having no receive path.
        let mut s = session("codex");
        s.hosting = Some("mux-pane".into());
        s.session_id = String::new();
        assert_eq!(select_carrier(&s), Err(SendRefusal::Bare));
    }

    #[test]
    fn pane_hosted_codex_without_a_live_pane_refuses_instead_of_lying() {
        // No pane and no endpoint means nothing can receive. Refuse honestly
        // rather than dispatch into a carrier that cannot deliver.
        let mut s = session("codex");
        s.hosting = Some("mux-pane".into());
        s.zmx_name = None;
        assert_eq!(select_carrier(&s), Err(SendRefusal::NoLiveReceivePath));

        let mut s2 = session("codex");
        s2.hosting = Some("mux-pane".into());
        s2.socket_dir = None;
        assert_eq!(select_carrier(&s2), Err(SendRefusal::NoLiveReceivePath));
    }

    #[test]
    fn pane_hosted_codex_not_live_is_a_wake_trigger_not_a_carrier_refusal() {
        // qd–qf W3b: Cold/Killed are NO LONGER lifecycle REFUSALS — they are WAKE
        // triggers (`is_live` == false), so the unified path revives the row before
        // selecting a carrier. `select_carrier` is only ever reached on a live row;
        // its non-live floor is the generic `NoLiveReceivePath` (never a Cold /
        // Stopped-specific refusal — those variants are retired).
        for status in [SessionStatus::Cold, SessionStatus::Killed] {
            let mut s = session("codex");
            s.hosting = Some("mux-pane".into());
            s.status = status;
            assert!(!is_live(&s), "{status:?} is a wake trigger, not live");
            assert_eq!(
                select_carrier(&s),
                Err(SendRefusal::NoLiveReceivePath),
                "select_carrier's non-live floor is NoLiveReceivePath, not a Cold/Stopped refusal"
            );
        }
    }

    #[test]
    fn selection_table_is_deterministic_and_relay_precedes_pty() {
        let mut claude = session("claude-code");
        claude.relay_port = Some(4312);
        assert_eq!(
            select_carrier(&claude),
            Ok(UnifiedCarrier::ClaudeRelay { port: 4312 })
        );
        assert_eq!(select_carrier(&claude), select_carrier(&claude));

        claude.relay_port = None;
        assert_eq!(select_carrier(&claude), Ok(UnifiedCarrier::MuxPty));
        claude.zmx_name = None;
        assert_eq!(
            select_carrier(&claude),
            Err(SendRefusal::NoLiveReceivePath)
        );
    }

    #[test]
    fn daemon_providers_route_to_their_one_lane_even_with_relay_state() {
        for (provider, expected) in [
            ("codex", UnifiedCarrier::CodexDaemon),
            ("acp/claude-code", UnifiedCarrier::AcpDaemon),
            ("acp/opencode", UnifiedCarrier::AcpDaemon),
            ("acp/future", UnifiedCarrier::AcpDaemon),
            ("pi", UnifiedCarrier::PiDaemon),
        ] {
            let mut target = session(provider);
            target.relay_port = Some(9999);
            assert_eq!(select_carrier(&target), Ok(expected), "{provider}");
        }
    }

    #[test]
    fn bare_unknown_and_unavailable_are_refused_but_not_live_is_a_wake_trigger() {
        // Bare (no bound identity) stays an immediate refusal.
        let mut target = session("claude-code");
        target.session_id.clear();
        assert_eq!(select_carrier(&target), Err(SendRefusal::Bare));

        // qd–qf W3b: Cold/Killed are NO LONGER carrier refusals — `is_live` is
        // false (a wake trigger), and `select_carrier`'s non-live floor is the
        // generic NoLiveReceivePath (the Cold/Stopped variants are retired).
        target = session("claude-code");
        target.status = SessionStatus::Cold;
        assert!(!is_live(&target));
        assert_eq!(select_carrier(&target), Err(SendRefusal::NoLiveReceivePath));

        target.status = SessionStatus::Killed;
        assert!(!is_live(&target));
        assert_eq!(select_carrier(&target), Err(SendRefusal::NoLiveReceivePath));

        // An unknown provider on a LIVE row is still an unknown-provider refusal.
        target = session("mystery");
        assert!(is_live(&target));
        assert_eq!(
            select_carrier(&target),
            Err(SendRefusal::UnknownProvider("mystery".into()))
        );

        // A LIVE claude with neither relay nor a joined mux pane has a genuinely
        // bare receive surface — NoLiveReceivePath (a transport-shape refusal,
        // distinct from a stopped session).
        target = session("claude-code");
        target.zmx_name = None;
        target.socket_dir = None;
        assert!(is_live(&target));
        assert_eq!(select_carrier(&target), Err(SendRefusal::NoLiveReceivePath));
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

    #[derive(Default)]
    struct ProbeBackend {
        calls: RefCell<Vec<(&'static str, String, String, Option<u16>)>>,
        result: i32,
    }

    impl ProbeBackend {
        fn record(&self, lane: &'static str, session: &Session, message: &str, port: Option<u16>) {
            self.calls.borrow_mut().push((
                lane,
                session.session_id.clone(),
                message.to_string(),
                port,
            ));
        }
    }

    impl UnifiedBackend for ProbeBackend {
        fn claude_relay(&self, session: &Session, message: &str, port: u16) -> i32 {
            self.record("relay", session, message, Some(port));
            self.result
        }
        fn mux_pty(&self, session: &Session, message: &str) -> i32 {
            self.record("pty", session, message, None);
            self.result
        }
        fn codex_daemon(&self, session: &Session, message: &str) -> i32 {
            self.record("codex", session, message, None);
            self.result
        }
        fn acp_daemon(&self, session: &Session, message: &str) -> i32 {
            self.record("acp", session, message, None);
            self.result
        }
        fn pi_daemon(&self, session: &Session, message: &str) -> i32 {
            self.record("pi", session, message, None);
            self.result
        }
    }

    #[test]
    fn dispatch_makes_exactly_one_call_and_never_falls_back_on_failure() {
        let backend = ProbeBackend {
            result: 9,
            ..Default::default()
        };
        let target = session("claude-code");
        let code = dispatch_selected(
            &backend,
            UnifiedCarrier::ClaudeRelay { port: 7070 },
            &target,
            "hello",
        );
        assert_eq!(code, 9);
        assert_eq!(backend.calls.borrow().as_slice(), &[(
            "relay",
            "session-uuid".into(),
            "hello".into(),
            Some(7070),
        )]);
    }

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
            let backend = ProbeBackend::default();
            let target = session("claude-code");
            assert_eq!(
                dispatch_selected(&backend, UnifiedCarrier::MuxPty, &target, message),
                0
            );
            let calls = backend.calls.borrow();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "pty");
            assert_eq!(calls[0].1, target.session_id);
            assert_eq!(calls[0].2.as_bytes(), message.as_bytes());
        }
    }

    // === qd–qf W3 part A: write-then-deliver + disposition stamping =========
    //
    // These exercise the `deliver_with_durability` seam directly with a jailed
    // QdPaths + a ProbeBackend, so the log-append / event-stamp wiring is
    // proven without standing up a full live carrier. The store readers
    // (dispatch::dispositions) parse the actual files the seam wrote.

    use dispatch::effects::MapEnv;

    /// A MapEnv whose HOME points into `home` (QD_HOME unset ⇒ transport files
    /// land under `home/.quorum/dispatch`, exactly where the seam writes them).
    fn jail_env(home: &std::path::Path) -> MapEnv {
        let mut e = MapEnv::default();
        e.vars.insert("HOME".into(), home.to_string_lossy().into_owned());
        // QD_HOST unset ⇒ local_host = "local" (the v1 envelope-origin placeholder).
        e
    }

    #[test]
    fn durability_logs_envelope_before_delivery_then_stamps_delivered() {
        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let backend = ProbeBackend::default(); // returns 0 ⇒ delivered
        let target = session("claude-code");

        let code = deliver_with_durability(
            &env,
            &paths,
            &backend,
            UnifiedCarrier::MuxPty,
            &target,
            "worker@brano", // the RAW caller address
            "hello body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None, // no caller-supplied id ⇒ qd mints a ULID
        );
        assert_eq!(code, 0, "delivered ⇒ exit 0 (backend's result)");

        // The carrier was actually called (delivery happened).
        assert_eq!(backend.calls.borrow().len(), 1);

        // The transport files honor QD_HOME resolution; read them back.
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1, "exactly one envelope logged");
        let env_row = &log.records[0];
        assert_eq!(env_row.target, "worker@brano", "raw address recorded");
        assert_eq!(env_row.body, "hello body", "body verbatim");
        assert_eq!(env_row.origin, "local", "v1 origin-host placeholder");
        assert_eq!(
            env_row.expires_at,
            env_row.authored_at + dispatch::origin_send::DEFAULT_EXPIRES_MS
        );

        // The funnel for a live-origin success: attempted, delivered. Rows are
        // fully normalized (R14.2) — no witness/origin/authored_at fields; every
        // row joins the envelope by correlation_id and carries only created_at.
        let events = dispatch::dispositions::read_local_events(&tpaths);
        let kinds: Vec<dispatch::dispositions::EventKind> =
            events.records.iter().map(|e| e.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                dispatch::dispositions::EventKind::Attempted,
                dispatch::dispositions::EventKind::Delivered
            ],
            "live origin path stamps attempted then delivered"
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
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let backend = ProbeBackend { result: 1, ..Default::default() }; // definitive fail
        let target = session("claude-code");

        let code = deliver_with_durability(
            &env,
            &paths,
            &backend,
            UnifiedCarrier::MuxPty,
            &target,
            "worker",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None,
        );
        assert_eq!(code, 1, "carrier failure exit is preserved");

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
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let backend = ProbeBackend::default();
        let target = session("claude-code");

        // 30m in ms (what parse_expires("30m") yields).
        let thirty_min_ms = 30 * 60_000;
        deliver_with_durability(
            &env,
            &paths,
            &backend,
            UnifiedCarrier::MuxPty,
            &target,
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
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let backend = ProbeBackend::default(); // 0 ⇒ delivered
        let target = session("claude-code");

        let code = deliver_with_durability(
            &env,
            &paths,
            &backend,
            UnifiedCarrier::MuxPty,
            &target,
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
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let backend = ProbeBackend::default();
        let target = session("claude-code");

        deliver_with_durability(
            &env,
            &paths,
            &backend,
            UnifiedCarrier::MuxPty,
            &target,
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
        let backend = ProbeBackend::default();
        let refreshed = live_refreshed();
        let waker = MockWaker::ok(refreshed);

        let mut cold = session("claude-code");
        cold.status = SessionStatus::Cold;

        let code = wake_then_deliver(
            &env,
            &paths,
            &backend,
            &waker,
            RenderMode::Inline,
            &cold,
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
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());

        // Invocation 1 — LIVE origin path; the backend returns nonzero (a
        // definitive delivery failure): attempted + delivery-failed{delivery}.
        let failing = ProbeBackend {
            result: 7,
            ..Default::default()
        };
        let target = session("claude-code");
        let code = deliver_with_durability(
            &env,
            &paths,
            &failing,
            UnifiedCarrier::MuxPty,
            &target,
            "worker",
            "hello body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            Some("Q6FUNNEL".to_string()),
        );
        assert_ne!(code, 0, "invocation 1 is a definitive delivery failure");

        // Invocation 2 — the RETRY, SAME supplied id, through the wake path: the
        // waker succeeds (refreshed live row) and the backend succeeds:
        // attempted + queued + delivered.
        let ok_backend = ProbeBackend::default(); // 0 ⇒ delivered
        let waker = MockWaker::ok(live_refreshed());
        let mut cold = session("claude-code");
        cold.status = SessionStatus::Cold;
        let code = wake_then_deliver(
            &env,
            &paths,
            &ok_backend,
            &waker,
            RenderMode::Inline,
            &cold,
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
    // These exercise the `wake_then_deliver` seam with a MOCK Waker (no live
    // revive): a NOT-live target logs the envelope FIRST, then wakes, then either
    // delivers into the refreshed row (delivered stamp) or, on an unwakeable
    // target, stamps `failed{wake}` and exits 12. The store readers parse the
    // actual files the seam wrote. Separately, `RealWaker::wake`'s provider→route
    // mapping is pinned (the unknown-provider `failed{wake}` is unit-checkable
    // without a revive; the revive-backed arms are covered by the live bin test).

    /// A live (Idle) claude row a successful wake "refreshes" into — carries a mux
    /// pane so `select_carrier` routes it to the PTY carrier.
    fn live_refreshed() -> Session {
        let mut s = session("claude-code");
        s.status = SessionStatus::Idle; // live ⇒ select_carrier succeeds
        s
    }

    /// A mock [`Waker`]: returns a pre-set outcome and records that it was asked to
    /// wake (so a test can assert the wake was actually attempted).
    struct MockWaker {
        outcome: RefCell<Option<Result<Session, Refusal>>>,
        woke: Cell<bool>,
    }
    impl MockWaker {
        fn ok(refreshed: Session) -> Self {
            MockWaker {
                outcome: RefCell::new(Some(Ok(refreshed))),
                woke: Cell::new(false),
            }
        }
        fn err(refusal: Refusal) -> Self {
            MockWaker {
                outcome: RefCell::new(Some(Err(refusal))),
                woke: Cell::new(false),
            }
        }
    }
    impl Waker for MockWaker {
        fn wake(&self, _session: &Session, _render: RenderMode) -> Result<Session, Refusal> {
            self.woke.set(true);
            self.outcome.borrow_mut().take().expect("wake called once")
        }
    }

    #[test]
    fn wake_then_deliver_logs_envelope_wakes_then_delivers_into_refreshed_row() {
        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let backend = ProbeBackend::default(); // 0 ⇒ delivered
        let refreshed = live_refreshed();
        let waker = MockWaker::ok(refreshed.clone());

        let mut cold = session("claude-code");
        cold.status = SessionStatus::Cold; // the NOT-live target that triggers a wake

        let code = wake_then_deliver(
            &env,
            &paths,
            &backend,
            &waker,
            RenderMode::Inline,
            &cold,
            "worker@brano",
            "hello body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None, // no caller-supplied id ⇒ qd mints a ULID
        );
        assert_eq!(code, 0, "woken + delivered ⇒ exit 0");
        assert!(waker.woke.get(), "a not-live target must actually be woken");
        // The carrier was called ONCE against the REFRESHED row's identity.
        let calls = backend.calls.borrow();
        assert_eq!(calls.len(), 1, "delivered into the refreshed row exactly once");
        assert_eq!(calls[0].1, refreshed.session_id);

        // Envelope logged FIRST (write-then-deliver); the wake-path funnel is
        // attempted, queued (durably awaiting the wake), then delivered.
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1, "envelope logged before the wake");
        assert_eq!(log.records[0].target, "worker@brano");
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

    #[test]
    fn wake_then_deliver_unwakeable_target_stamps_failed_wake_exit_12() {
        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let backend = ProbeBackend::default();
        let waker = MockWaker::err(Refusal::failed("wake", "could not revive claude session \"wk\""));

        let mut killed = session("claude-code");
        killed.status = SessionStatus::Killed; // a tombstoned target

        let code = wake_then_deliver(
            &env,
            &paths,
            &backend,
            &waker,
            RenderMode::Inline,
            &killed,
            "wk",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None,
        );
        assert_eq!(code, dispatch::origin_send::EXIT_REFUSED, "failed{{wake}} ⇒ exit 12");
        assert!(waker.woke.get(), "the wake was attempted");
        // The carrier was NEVER called — nothing was delivered.
        assert_eq!(backend.calls.borrow().len(), 0, "no delivery on a wake failure");

        // The envelope was still logged FIRST; the funnel reads attempted, queued
        // (the attempt placed the message durably awaiting the wake), then
        // delivery-failed{wake} — so an operator can read back the outcome.
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

    #[test]
    fn wake_then_deliver_refreshed_but_unroutable_is_also_failed_wake() {
        // A revive that reports success but yields a row with no live receive path
        // (live but relay-less and mux-less) is a wake that did not produce a
        // deliverable target → failed{wake}, not a silent no-op / delivered.
        let tmp = tempfile::tempdir().unwrap();
        let env = jail_env(tmp.path());
        let paths = dispatch::paths::QdPaths::from_home(tmp.path());
        let backend = ProbeBackend::default();
        let mut unroutable = live_refreshed();
        unroutable.zmx_name = None;
        unroutable.socket_dir = None; // live claude, no relay, no mux ⇒ NoLiveReceivePath
        let waker = MockWaker::ok(unroutable);

        let mut cold = session("claude-code");
        cold.status = SessionStatus::Cold;

        let code = wake_then_deliver(
            &env,
            &paths,
            &backend,
            &waker,
            RenderMode::Inline,
            &cold,
            "wk",
            "body",
            dispatch::origin_send::DEFAULT_EXPIRES_MS,
            None,
        );
        assert_eq!(code, dispatch::origin_send::EXIT_REFUSED);
        assert_eq!(backend.calls.borrow().len(), 0, "unroutable ⇒ no delivery");
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let events = dispatch::dispositions::read_local_events(&tpaths);
        let last = events.records.last().expect("a delivery-failed event was stamped");
        assert_eq!(event_row(last), (dispatch::dispositions::EventKind::DeliveryFailed, Some("wake".to_string())));
    }

    #[test]
    fn real_waker_unknown_provider_is_failed_wake() {
        // The provider→route table's default arm: a provider with no headless wake
        // route yields failed{wake} (no revive attempted). This is the one RealWaker
        // arm reachable off the default floor — the revive-backed arms need a live
        // registry/mux and are covered by the live bin test.
        let mut mystery = session("mystery");
        mystery.status = SessionStatus::Cold;
        let r = RealWaker.wake(&mystery, RenderMode::Inline).unwrap_err();
        assert_eq!(r.family, dispatch::origin_send::Family::Failed);
        assert_eq!(r.class, "wake");
        assert!(
            r.reason.contains("cannot be woken headlessly"),
            "unknown provider ⇒ headless-wake refusal, got: {}",
            r.reason
        );
    }

    // QS-2 structural guard: `run_claude_relay_unified` in send_relay.rs MUST
    // inject using the resolved session UUID (session.session_id) as the
    // SessionKey.id — NOT a display name. The prior bug delegated to
    // inject_via_provider which set id=display_name. The fix inlines ProviderFx
    // construction. This test pins the fix so that future refactors cannot silently
    // revert to name-based injection.
    // MUTATION EVIDENCE: restoring the inject_via_provider call reds the first
    // assert; removing the session_id reference reds the second.
    #[test]
    fn relay_unified_uses_session_uuid_not_display_name_as_injection_identity() {
        let src = include_str!("send_relay.rs");
        let fn_start = src
            .find("pub(super) fn run_claude_relay_unified(")
            .expect("run_claude_relay_unified must exist in send_relay.rs");
        let after_start = &src[fn_start..];
        // Scope to the function body: ends at the next pub(super)/pub/fn boundary.
        // Scope to the function body: run_with_client immediately follows.
        let fn_end = after_start
            .find("\nfn run_with_client(")
            .expect("run_with_client must immediately follow run_claude_relay_unified");
        let body = &after_start[..fn_end];

        // Must NOT delegate to inject_via_provider (which would set id=display_name).
        assert!(
            !body.contains("inject_via_provider("),
            "run_claude_relay_unified must NOT call inject_via_provider — it must \
             inline ProviderFx using id: &session.session_id (QS-2). Body:\n{body}"
        );
        // Must reference the resolved UUID as the injection identity.
        assert!(
            body.contains("session.session_id"),
            "run_claude_relay_unified must reference session.session_id as the \
             injection identity (QS-2). Body:\n{body}"
        );
    }
}
