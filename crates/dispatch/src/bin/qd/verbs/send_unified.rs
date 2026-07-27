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
    ClaudePty,
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
        "codex" => Ok(UnifiedCarrier::CodexDaemon),
        provider if provider.starts_with("acp/") => Ok(UnifiedCarrier::AcpDaemon),
        "pi" => Ok(UnifiedCarrier::PiDaemon),
        // Relay precedence is structural: a recorded port selects relay before
        // mux state is considered. PTY can only be selected from a positive
        // relay_port=None observation plus a live joined mux pane.
        "claude-code" => match session.relay_port {
            Some(port) => Ok(UnifiedCarrier::ClaudeRelay { port }),
            None if session.zmx_name.is_some() && session.socket_dir.is_some() => {
                Ok(UnifiedCarrier::ClaudePty)
            }
            None => Err(SendRefusal::NoLiveReceivePath),
        },
        other => Err(SendRefusal::UnknownProvider(other.to_string())),
    }
}

trait UnifiedBackend {
    fn claude_relay(&self, session: &Session, message: &str, port: u16) -> i32;
    fn claude_pty(&self, session: &Session, message: &str) -> i32;
    fn codex_daemon(&self, session: &Session, message: &str) -> i32;
    fn acp_daemon(&self, session: &Session, message: &str) -> i32;
    fn pi_daemon(&self, session: &Session, message: &str) -> i32;
}

struct RealUnifiedBackend;

impl UnifiedBackend for RealUnifiedBackend {
    fn claude_relay(&self, session: &Session, message: &str, port: u16) -> i32 {
        send_relay::run_claude_relay_unified(session, message, port)
    }

    fn claude_pty(&self, session: &Session, message: &str) -> i32 {
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
    //   codex                         -> codex daemon lane
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
        UnifiedCarrier::ClaudePty => backend.claude_pty(session, message),
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

    // Resolve the caller's handle exactly once. All later refresh/revalidation
    // uses this row's immutable provider session id, never the caller's possibly
    // ambiguous name or prefix.
    let target = match common::resolve_session_uncapped(query) {
        Ok(session) => session,
        Err(code) => return code,
    };

    // Verb-entry self-send fence: QD_SESSION_ID is resolved through the same
    // idstore chain whoami owns. It runs before lifecycle/carrier selection, and
    // is not reported as a carrier failure.
    let env = RealEnv;
    let self_session_id = match resolve_self_session_id(&env) {
        Ok(value) => value,
        Err(code) => return code,
    };
    if self_session_id.as_deref() == Some(target.session_id.as_str()) {
        let label = target.name.as_deref().unwrap_or(query);
        eprintln!(
            "qd send: refusing self-send to \"{label}\" — QD_SESSION_ID resolves to the target session."
        );
        return 1;
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
    dispatch_selected(&RealUnifiedBackend, carrier, &current, message)
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
            which_branch: SessionBranch::LiveRegistry,
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
        assert_eq!(select_carrier(&claude), Ok(UnifiedCarrier::ClaudePty));
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
        fn claude_pty(&self, session: &Session, message: &str) -> i32 {
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
                dispatch_selected(&backend, UnifiedCarrier::ClaudePty, &target, message),
                0
            );
            let calls = backend.calls.borrow();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "pty");
            assert_eq!(calls[0].1, target.session_id);
            assert_eq!(calls[0].2.as_bytes(), message.as_bytes());
        }
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
