//! Unified `qd send <target> <message>` selection and dispatch.
//!
//! Target resolution and carrier selection are deliberately separate. The
//! resolver produces one concrete session identity; the pure selector consumes
//! only that row's observable state; the dispatcher receives the same row and
//! never resolves a name, prefix, or PID on its own.

use clap::ArgMatches;

use dispatch::effects::{Env, RealEnv};
use dispatch::idstore::IdMap;
use dispatch::model::{Session, SessionStatus};

use super::{common, send, send_relay};

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
    Cold,
    Stopped,
    NoLiveReceivePath,
    UnknownProvider(String),
}

/// Pure pre-attempt selector. There is intentionally no probing or discovery
/// here: relay presence, mux linkage, provider, and lifecycle state all come
/// from the one resolved registry/join snapshot.
fn select_carrier(session: &Session) -> Result<UnifiedCarrier, SendRefusal> {
    if session.session_id.is_empty() {
        return Err(SendRefusal::Bare);
    }
    match session.status {
        SessionStatus::Cold => return Err(SendRefusal::Cold),
        SessionStatus::Killed => return Err(SendRefusal::Stopped),
        SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Shell => {}
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
        SendRefusal::Cold => eprintln!(
            "qd send: found \"{label}\", but it is cold and not receivable — resume it first."
        ),
        SendRefusal::Stopped => eprintln!(
            "qd send: found \"{label}\", but it is stopped and not receivable — resume it first."
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
    let query = m.get_one::<String>("session").expect("required by clap");
    let message = m.get_one::<String>("message").expect("required by clap");

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

    if let Err(code) = common::reject_if_tombstoned(query, &target) {
        return code;
    }
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
    // message. Selection below uses this current observable snapshot.
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
    if let Err(code) = common::reject_if_tombstoned(query, &current) {
        return code;
    }

    let carrier = match select_carrier(&current) {
        Ok(carrier) => carrier,
        Err(refusal) => return report_refusal(query, &current, refusal),
    };

    // qd–qf W3 part A: WRITE-THEN-DELIVER. Mint the envelope, append it to
    // `log.jsonl` BEFORE delivery (HARD FAIL if that append errors — we never
    // deliver without the durable envelope), deliver via the existing unified
    // carrier, then stamp the witnessed terminal into `dispositions.jsonl`
    // (best-effort — a lost disposition row must never flip the send's own exit).
    deliver_with_durability(&env, &paths, &RealUnifiedBackend, carrier, &current, query, message, expires_ms)
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
    use dispatch::dispositions::{self, StoredState};
    use dispatch::effects::{Clock, RealClock};
    use dispatch::origin_send::{build_disposition, build_envelope, mint_correlation_id};

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

    // Deliver via the existing unified carrier (unchanged behavior/exit).
    let code = dispatch_selected(backend, carrier, session, message);

    // Stamp the witnessed terminal AFTER the attempt. exit 0 ⇒ delivered; a
    // definitive failure ⇒ failed{delivery}. (There is no unwakeable-target path
    // here yet — resume-and-deliver / failed{wake} is W3 part B, deferred.)
    let (state, reason) = if code == 0 {
        (StoredState::Delivered, None)
    } else {
        (StoredState::Failed, Some("delivery".to_string()))
    };
    let disp = build_disposition(
        correlation_id,
        state,
        authored_at,
        clock.now_ms(),
        authority,
        reason,
    );
    if let Err(e) = dispositions::append_disposition(&tpaths, &disp) {
        // BEST-EFFORT: the delivery already happened; a lost disposition row must
        // NOT change the send's exit. Warn only (events.rs telemetry posture).
        eprintln!("WARNING: could not record the delivery disposition (non-fatal): {e}");
    }
    code
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

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
    fn pane_hosted_codex_still_obeys_the_lifecycle_refusals() {
        // Hosting selects the CARRIER; it does not exempt a row from the
        // cold/stopped gates that run before carrier selection.
        for (status, expected) in [
            (SessionStatus::Cold, SendRefusal::Cold),
            (SessionStatus::Killed, SendRefusal::Stopped),
        ] {
            let mut s = session("codex");
            s.hosting = Some("mux-pane".into());
            s.status = status;
            assert_eq!(select_carrier(&s), Err(expected));
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
    fn bare_cold_stopped_unknown_and_unavailable_are_refused() {
        let mut target = session("claude-code");
        target.session_id.clear();
        assert_eq!(select_carrier(&target), Err(SendRefusal::Bare));

        target = session("claude-code");
        target.status = SessionStatus::Cold;
        assert_eq!(select_carrier(&target), Err(SendRefusal::Cold));

        target.status = SessionStatus::Killed;
        assert_eq!(select_carrier(&target), Err(SendRefusal::Stopped));

        target = session("mystery");
        assert_eq!(
            select_carrier(&target),
            Err(SendRefusal::UnknownProvider("mystery".into()))
        );

        target = session("claude-code");
        target.zmx_name = None;
        target.socket_dir = None;
        assert_eq!(
            select_carrier(&target),
            Err(SendRefusal::NoLiveReceivePath)
        );
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
