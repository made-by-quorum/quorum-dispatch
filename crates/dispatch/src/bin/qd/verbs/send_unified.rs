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

    // Resolve the caller's handle exactly once. All later refresh/revalidation
    // uses this row's immutable provider session id, never the caller's possibly
    // ambiguous name or prefix.
    let target = match common::resolve_session_uncapped(query) {
        Ok(session) => session,
        Err(code) => return code,
    };

    // Verb-entry self-send fence: QD_SESSION_ID is resolved through the same
    // idstore chain whoami owns. It runs before lifecycle/carrier selection, and
    // is not reported as a carrier failure. qd–qf W3 part D: the self-send sync
    // refusal renders through the shared Refusal {class,reason} type.
    let env = RealEnv;
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
        // (hard-fail if the append errors), deliver via the existing unified
        // carrier, then stamp the witnessed terminal (best-effort).
        deliver_with_durability(
            &env,
            &paths,
            &RealUnifiedBackend,
            carrier,
            &current,
            query,
            message,
            expires_ms,
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
        )
    }
}

// ===========================================================================
// qd–qf W4 — INBOUND MODE ("THE ONE DOOR")
// ===========================================================================

/// Resolve `target` to exactly one session for the INBOUND door, rendering the
/// resolver's outcomes through [`Refusal`] (the door's `{class,reason}` family,
/// exit 12) instead of the origin-mode `resolve_or_die` prints (which exit 1).
///
/// Same liveness-aware resolution the acting verbs use (`resolve_session_with_liveness`
/// against the FULL, uncapped session universe with the SAME pid-aware predicate
/// as `resolve_or_die`) — we do NOT first-match: `None` ⇒ `refused{unknown}`,
/// `Many` ⇒ `refused{ambiguous}`, `One` ⇒ the target. The gather / list-build
/// failure (HOME unset etc.) surfaces as its own printed exit code, wrapped so the
/// door returns an exit not a panic.
fn resolve_inbound_target(target: &str) -> Result<Session, Refusal> {
    use dispatch::effects::is_pid_alive;
    use dispatch::join::JoinOpts;
    use dispatch::resolve::{is_live_status, resolve_session_with_liveness, Resolution};

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
            format!("could not resolve inbound target \"{target}\" (session store unavailable)"),
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

    match resolve_session_with_liveness(target, &sessions, is_alive) {
        Resolution::One(s) => Ok(s.clone()),
        Resolution::None => Err(Refusal::refused(
            "unknown",
            format!("no session matching \"{target}\""),
        )),
        Resolution::Many(v) => Err(Refusal::refused(
            "ambiguous",
            format!("\"{target}\" matches {} sessions — refusing to guess", v.len()),
        )),
    }
}

/// qd–qf W4 — INBOUND MODE. Admit a peer's ALREADY-minted envelope at the door,
/// validate it, be idempotent on its id, and (resume-and-)deliver it — WITHOUT
/// ever appending to this host's own `log.jsonl` (my log = envelopes I
/// ORIGINATED; the peer's envelope lives in the mirror). The witnessed terminal
/// is stamped with `authority` = THIS host (the witness), `authored_at` copied
/// from the envelope, `witnessed_at` = now.
///
/// Door order (validate cheap→expensive, side-effect-free until delivery):
///   1. READ the envelope bytes (`<path>`, or stdin for `-`). IO error ⇒ error.
///   2. PARSE into [`Envelope`] via serde; a parse failure / `v != 1` / a missing
///      field ⇒ `refused{malformed}`.
///   3. PAST-EXPIRY: `expires_at < now` ⇒ `expired{past-expiry}` (REFUSED at the
///      door, never stamped `expired` — `expired` is a DERIVED view state, §2/§3).
///   4. RESOLVE the envelope's `target` (via [`resolve_inbound_target`]): unknown
///      ⇒ `refused{unknown}`, ambiguous ⇒ `refused{ambiguous}` (never first-match).
///   5. IDEMPOTENCY: a terminal already present for `correlation_id`
///      ([`has_terminal`]) ⇒ NO-OP SUCCESS (deliver nothing, stamp nothing, exit
///      0). "First terminal wins" (§2).
///   6. (Resume-and-)DELIVER: a not-live target is WOKEN first (reuse the W3b
///      [`Waker`] wake path), then delivered; a wake that cannot succeed stamps
///      `failed{wake}` (exit 12). NO envelope log append (contract §4).
///   7. STAMP the witnessed terminal (`delivered` / `failed{delivery}`) via the
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

    // (3) PAST-EXPIRY door — a past-expiry inbound envelope is REFUSED at the door,
    // NOT stamped `expired` (that is a DERIVED view state, never authored).
    if envelope.expires_at < now {
        return Refusal::expired(
            "past-expiry",
            format!(
                "envelope {} expired at {} (now {})",
                envelope.correlation_id, envelope.expires_at, now
            ),
        )
        .emit();
    }

    // (4) RESOLVE the ENVELOPE's target (mis-addressed / ambiguous ⇒ named refusal).
    let target = match resolve_inbound_target(&envelope.target) {
        Ok(s) => s,
        Err(refusal) => return refusal.emit(),
    };

    // The transport files honor QD_HOME (from_home_env), matching the store + the
    // W5 reader. HOME/paths already succeeded inside `resolve_inbound_target`'s
    // gather, so a failure here is only a fresh HOME-unset race — surface it.
    let paths = match common::paths_from_home(env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, env);

    // (5) IDEMPOTENCY — a terminal already present for this id ⇒ NO-OP SUCCESS
    // (deliver nothing, stamp nothing, exit 0). Local-only by design (this qd's
    // witnessed facts are its own authority). A read error is NOT treated as
    // "absent" (that would risk a double delivery) — surface it as a generic
    // failure rather than silently re-delivering.
    match dispositions::has_terminal(&tpaths, &envelope.correlation_id) {
        Ok(true) => {
            eprintln!(
                "qd send: {} already witnessed — no-op",
                envelope.correlation_id
            );
            return 0;
        }
        Ok(false) => {}
        Err(e) => {
            eprintln!(
                "qd send: could not read the disposition ledger for idempotency ({e}) — not admitted."
            );
            return 1;
        }
    }

    // (6) (resume-and-)DELIVER. The disposition WITNESS authority is THIS host (not
    // the envelope's origin authority); `authored_at` is copied from the envelope.
    let witness_authority = dispositions::local_authority(env);

    // A not-live target is WOKEN first (reuse the W3b wake seam), then delivered;
    // a live target is delivered directly. NO envelope log append either way.
    if is_live(&target) {
        let carrier = match select_carrier(&target) {
            Ok(c) => c,
            Err(refusal) => return report_refusal(&envelope.target, &target, refusal),
        };
        deliver_then_stamp(
            &tpaths,
            backend,
            carrier,
            &target,
            &envelope.body,
            &envelope.correlation_id,
            envelope.authored_at,
            &witness_authority,
            &clock,
        )
    } else {
        // Flag-less render (the `send` verb has no --alt-screen/--inline): config
        // render-default > the inline default (exactly the origin not-live path).
        let render = dispatch::launch::resolve_render_mode(
            None,
            dispatch::launch::render_default_from_config(env).as_deref(),
        );
        // On a wake failure, stamp `failed{wake}` against the ENVELOPE (witnessed
        // by this host, authored_at copied) + exit 12 — the SAME contract as W3b,
        // but with NO envelope log append.
        let stamp_failed_wake = |refusal: Refusal| -> i32 {
            let disp = dispatch::origin_send::build_disposition(
                envelope.correlation_id.clone(),
                dispatch::dispositions::StoredState::Failed,
                envelope.authored_at,
                clock.now_ms(),
                witness_authority.clone(),
                Some(refusal.class.clone()),
            );
            if let Err(e) = dispositions::append_disposition(&tpaths, &disp) {
                eprintln!("WARNING: could not record the wake-failure disposition (non-fatal): {e}");
            }
            refusal.emit()
        };
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
            envelope.authored_at,
            &witness_authority,
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

/// qd–qf W3 part A — the write-then-deliver + disposition-stamp wrapper around
/// the existing unified carrier dispatch. Kept as a seamed helper (deps injected)
/// so the log-append / terminal-stamp shape is exercised without standing up a
/// full live carrier: the `backend` is any [`UnifiedBackend`], `env`/`paths` are
/// the resolved seams.
///
/// Ordering (format doc §1/§2): LOG the envelope, THEN deliver, THEN stamp. The
/// envelope append is fatal-on-error (no durable record ⇒ do not deliver); the
/// disposition append is best-effort (the delivery already happened). A
/// synchronous local attempt that completes is `delivered` (exit 0) or `failed`
/// (nonzero); `pending`/`expired` are DERIVED (absence) and never stamped here.
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
    let correlation_id = mint_correlation_id(&clock);
    let authority = dispositions::local_authority(env);

    // Mint + LOG FIRST (write-then-deliver). `target` is the RAW address the
    // caller gave (operational record); `body` is the message verbatim.
    let envelope = build_envelope(
        correlation_id.clone(),
        authored_at,
        expires_ms,
        raw_target.to_string(),
        authority.clone(),
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

    // Deliver via the existing unified carrier + stamp the witnessed terminal.
    // (This is the LIVE path — a not-live target's resume-and-deliver /
    // failed{wake} lives in `wake_then_deliver`.) The deliver + terminal-stamp
    // tail is the SHARED `deliver_then_stamp` core (identical to the not-live and
    // W4 inbound tails): exit 0 ⇒ delivered; a definitive failure ⇒
    // failed{delivery}.
    deliver_then_stamp(
        &tpaths,
        backend,
        carrier,
        session,
        message,
        &correlation_id,
        authored_at,
        &authority,
        &clock,
    )
}

/// qd–qf W3/W4 — the SHARED deliver → stamp-terminal tail (NO log append). The
/// envelope is ALREADY durable (origin logged it; inbound never logs its own).
/// One carrier call, then a best-effort terminal `Disposition`:
///   - exit 0            ⇒ `delivered` (no reason),
///   - definitive nonzero ⇒ `failed{delivery}`.
///
/// `witness_authority` is stamped as the disposition's `authority` (the WITNESS —
/// this host); `authored_at` is copied verbatim (origin's mint for an origin
/// send, the ENVELOPE's for an inbound one — a self-contained terminal, §2). Used
/// by the origin live path, the origin resume-and-deliver path, AND W4 inbound —
/// so the three cannot drift.
#[allow(clippy::too_many_arguments)]
fn deliver_then_stamp(
    tpaths: &dispatch::paths::QdPaths,
    backend: &dyn UnifiedBackend,
    carrier: UnifiedCarrier,
    session: &Session,
    message: &str,
    correlation_id: &str,
    authored_at: i64,
    witness_authority: &str,
    clock: &dyn dispatch::effects::Clock,
) -> i32 {
    use dispatch::dispositions::{self, StoredState};
    use dispatch::origin_send::build_disposition;

    let code = dispatch_selected(backend, carrier, session, message);

    let (state, reason) = if code == 0 {
        (StoredState::Delivered, None)
    } else {
        (StoredState::Failed, Some("delivery".to_string()))
    };
    let disp = build_disposition(
        correlation_id.to_string(),
        state,
        authored_at,
        clock.now_ms(),
        witness_authority.to_string(),
        reason,
    );
    if let Err(e) = dispositions::append_disposition(tpaths, &disp) {
        // BEST-EFFORT: the delivery already happened; a lost disposition row must
        // NOT change the send's exit. Warn only (events.rs telemetry posture).
        eprintln!("WARNING: could not record the delivery disposition (non-fatal): {e}");
    }
    code
}

/// qd–qf W3b — the RESUME-AND-DELIVER path for a NOT-live target. "Stopped is not
/// a refusal class": the envelope is LOGGED FIRST (write-then-deliver — hard-fail
/// if the append errors), THEN the target is WOKEN, THEN delivered into the
/// refreshed row.
///
/// Ordering (contract §4, format doc §1/§2), all inside the durability boundary:
///   1. LOG the envelope (fatal-on-error — no durable record ⇒ do not proceed);
///   2. WAKE via the [`Waker`] seam. On `Err(failed{wake})` the wake could not
///      succeed → stamp a `failed{wake}` disposition against the logged envelope,
///      print the refusal, and return [`EXIT_REFUSED`] (12). Nothing was delivered.
///   3. On `Ok(refreshed)` re-select the carrier for the refreshed (now live) row.
///      A revive that reported success but left an unroutable row is itself a wake
///      failure → the SAME `failed{wake}` stamp + exit 12 (never a silent no-op).
///   4. DELIVER via the carrier, then STAMP `delivered`/`failed{delivery}` — the
///      identical terminal wiring the live path uses.
///
/// Seamed (deps injected — `backend`/`waker`/`env`/`paths`) so the log → wake →
/// select → deliver → stamp shape is proven with mocks (no live carrier/revive).
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
) -> i32 {
    use dispatch::dispositions::{self, StoredState};
    use dispatch::effects::{Clock, RealClock};
    use dispatch::origin_send::{build_disposition, build_envelope, mint_correlation_id};

    // Same transport-file resolution + minting as the live path (from_home_env
    // honors QD_HOME, matching the store + the W5 reader).
    let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, env);
    let clock = RealClock;
    let authored_at = clock.now_ms();
    let correlation_id = mint_correlation_id(&clock);
    let authority = dispositions::local_authority(env);

    // (1) LOG FIRST — even a wake that later fails leaves the durable envelope, so
    // a `failed{wake}` disposition has an envelope to join on (write-then-deliver).
    let envelope = build_envelope(
        correlation_id.clone(),
        authored_at,
        expires_ms,
        raw_target.to_string(),
        authority.clone(),
        message.to_string(),
    );
    if let Err(e) = dispositions::append_envelope(&tpaths, &envelope) {
        eprintln!(
            "qd send: could not durably record the message before delivery ({e}) — not sent."
        );
        return 1;
    }

    // A `failed{wake}` terminal: stamp it against the logged envelope (best-effort
    // — a lost disposition must not change the exit), print the refusal, exit 12.
    let stamp_failed_wake = |refusal: Refusal| -> i32 {
        let disp = build_disposition(
            correlation_id.clone(),
            StoredState::Failed,
            authored_at,
            clock.now_ms(),
            authority.clone(),
            Some(refusal.class.clone()),
        );
        if let Err(e) = dispositions::append_disposition(&tpaths, &disp) {
            eprintln!("WARNING: could not record the wake-failure disposition (non-fatal): {e}");
        }
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

    // (4) DELIVER into the refreshed row + STAMP the witnessed terminal (the
    // SHARED `deliver_then_stamp` tail — identical to the live + inbound paths).
    deliver_then_stamp(
        &tpaths,
        backend,
        carrier,
        &refreshed,
        message,
        &correlation_id,
        authored_at,
        &authority,
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
    // QdPaths + a ProbeBackend, so the log-append / terminal-stamp wiring is
    // proven without standing up a full live carrier. The store readers
    // (dispatch::dispositions) parse the actual files the seam wrote.

    use dispatch::effects::MapEnv;

    /// A MapEnv whose HOME points into `home` (QD_HOME unset ⇒ transport files
    /// land under `home/.quorum/dispatch`, exactly where the seam writes them).
    fn jail_env(home: &std::path::Path) -> MapEnv {
        let mut e = MapEnv::default();
        e.vars.insert("HOME".into(), home.to_string_lossy().into_owned());
        // QD_HOST unset ⇒ authority = "local" (the v1 placeholder).
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
        assert_eq!(env_row.authority, "local", "v1 authority placeholder");
        assert_eq!(
            env_row.expires_at,
            env_row.authored_at + dispatch::origin_send::DEFAULT_EXPIRES_MS
        );

        let disps = dispatch::dispositions::read_local_dispositions(&tpaths);
        assert_eq!(disps.records.len(), 1, "exactly one terminal stamped");
        let d = &disps.records[0];
        assert_eq!(
            d.correlation_id, env_row.correlation_id,
            "disposition joins the envelope on correlation_id"
        );
        assert_eq!(d.state, dispatch::dispositions::StoredState::Delivered);
        assert_eq!(d.reason, None, "delivered carries no reason");
        assert_eq!(d.authored_at, env_row.authored_at, "authored_at copied from envelope");
        assert!(d.witnessed_at >= d.authored_at, "witnessed at/after authored");
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
        );
        assert_eq!(code, 1, "carrier failure exit is preserved");

        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        // Envelope still logged (write-then-deliver logs BEFORE the attempt).
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1);
        let disps = dispatch::dispositions::read_local_dispositions(&tpaths);
        assert_eq!(disps.records.len(), 1);
        let d = &disps.records[0];
        assert_eq!(d.state, dispatch::dispositions::StoredState::Failed);
        assert_eq!(d.reason.as_deref(), Some("delivery"), "failed carries a class reason");
        assert_eq!(d.correlation_id, log.records[0].correlation_id);
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
        );
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        let e = &log.records[0];
        assert_eq!(e.expires_at, e.authored_at + thirty_min_ms, "--expires window honored");
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
        );
        assert_eq!(code, 0, "woken + delivered ⇒ exit 0");
        assert!(waker.woke.get(), "a not-live target must actually be woken");
        // The carrier was called ONCE against the REFRESHED row's identity.
        let calls = backend.calls.borrow();
        assert_eq!(calls.len(), 1, "delivered into the refreshed row exactly once");
        assert_eq!(calls[0].1, refreshed.session_id);

        // Envelope logged FIRST (write-then-deliver), disposition = delivered.
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1, "envelope logged before the wake");
        assert_eq!(log.records[0].target, "worker@brano");
        let disps = dispatch::dispositions::read_local_dispositions(&tpaths);
        assert_eq!(disps.records.len(), 1);
        assert_eq!(disps.records[0].state, dispatch::dispositions::StoredState::Delivered);
        assert_eq!(disps.records[0].reason, None);
        assert_eq!(disps.records[0].correlation_id, log.records[0].correlation_id);
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
        );
        assert_eq!(code, dispatch::origin_send::EXIT_REFUSED, "failed{{wake}} ⇒ exit 12");
        assert!(waker.woke.get(), "the wake was attempted");
        // The carrier was NEVER called — nothing was delivered.
        assert_eq!(backend.calls.borrow().len(), 0, "no delivery on a wake failure");

        // The envelope was still logged FIRST, and a failed{wake} disposition joins
        // it (reason = the wake class), so an operator can read back the outcome.
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let log = dispatch::dispositions::read_local_log(&tpaths);
        assert_eq!(log.records.len(), 1, "envelope logged even though the wake failed");
        let disps = dispatch::dispositions::read_local_dispositions(&tpaths);
        assert_eq!(disps.records.len(), 1, "a failed{{wake}} terminal was stamped");
        assert_eq!(disps.records[0].state, dispatch::dispositions::StoredState::Failed);
        assert_eq!(disps.records[0].reason.as_deref(), Some("wake"), "failed{{wake}} reason");
        assert_eq!(disps.records[0].correlation_id, log.records[0].correlation_id);
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
        );
        assert_eq!(code, dispatch::origin_send::EXIT_REFUSED);
        assert_eq!(backend.calls.borrow().len(), 0, "unroutable ⇒ no delivery");
        let tpaths = dispatch::paths::QdPaths::from_home_env(&paths.home, &env);
        let disps = dispatch::dispositions::read_local_dispositions(&tpaths);
        assert_eq!(disps.records.len(), 1);
        assert_eq!(disps.records[0].state, dispatch::dispositions::StoredState::Failed);
        assert_eq!(disps.records[0].reason.as_deref(), Some("wake"));
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
