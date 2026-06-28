//! scoped-ACP-CC pillar 3 — the fallback-ladder integration lane (A2 §4 / charge §3).
//!
//! Drives the REAL `AcpProvider` (resolved via `provider_for("acp/claude-code")`) with
//! ACP forced UNAVAILABLE (`ProviderFx.acp_client = None` — the production "no connected
//! bridge" condition) and proves the §4 ladder converts that real `InjectError::NoTransport`
//! into a PTY/Mode-B-floor DROP + the persisted `transport=pty` latch + an observable log,
//! re-derives the degraded row to the floor (one-way latch), and REFUSES auto-PTY once a
//! structured send was issued (the duplicate-delivery guard). Deterministic — no live bridge.

use dispatch::effects::MapEnv;
use dispatch::paths::QdPaths;
use dispatch::provider::acp::{degrade_to_pty, derive_tier, on_inject_error, LadderOutcome, Tier};
use dispatch::provider::{provider_for, InjectError, ProviderFx, SessionKey};
use tempfile::TempDir;

/// Build a minimal `ProviderFx` with `acp_client` set as given (None = ACP unavailable).
fn fx_with_acp<'a>(
    env: &'a MapEnv,
    paths: &'a QdPaths,
    socket_dir: std::path::PathBuf,
    acp_client: Option<&'a dyn dispatch::provider::acp::AcpClient>,
) -> ProviderFx<'a> {
    ProviderFx {
        env,
        paths,
        socket_dir,
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client,
    }
}

#[test]
fn acp_unavailable_inject_drops_to_pty_floor_and_latches() {
    let tmp = TempDir::new().unwrap();
    let env = MapEnv::default();
    let paths = QdPaths::from_home(tmp.path());
    // ACP forced unavailable: no connected bridge client on the fx.
    let fx = fx_with_acp(&env, &paths, tmp.path().to_path_buf(), None);

    let provider = provider_for("acp/claude-code").expect("acp/claude-code registered");
    let key = SessionKey {
        id: "sess-floor-1",
        name: Some("acp-floor"),
        cwd: Some("/work/proj"),
        pid: None,
    };

    // The REAL provider inject signals ACP-unavailable as NoTransport (mod.rs maps a
    // missing acp_client → InjectError::NoTransport). This is the ladder's trigger.
    let err = provider
        .inject(&fx, &key, "deliver me", "fallback-test")
        .expect_err("ACP unavailable → inject must error, not hang");
    assert!(
        matches!(err, InjectError::NoTransport(_)),
        "ACP-unavailable surfaces as NoTransport, got {err:?}"
    );

    // No structured send was issued (the prompt never reached a wire) → the ladder
    // says DROP TO THE FLOOR and DELIVER (the charge §3 negative control: fall to PTY,
    // NOT fail/hang).
    let outcome = on_inject_error(&err, false);
    assert_eq!(outcome, LadderOutcome::DropToPtyFloor);

    // The verb latches the degradation on the row's transport field (one persisted bit)
    // and the drop is observable, not silent.
    let mut transport: Option<String> = None; // a fresh structured row
    degrade_to_pty(&mut transport);
    assert_eq!(transport.as_deref(), Some("pty"), "the degradation latch is written");

    // Re-deriving the tier on the now-degraded row resolves to the floor (one-way
    // latch — even though, hypothetically, an endpoint might later probe alive).
    assert_eq!(
        derive_tier("acp/claude-code", transport.as_deref(), true),
        Tier::PtyFloor,
        "a pty-latched row stays on the floor (no silent re-upgrade)"
    );
}

#[test]
fn auto_pty_refused_after_a_structured_send_was_issued() {
    // Same NoTransport, but a structured send WAS already issued for/on this session →
    // auto-PTY would risk duplicate delivery, so the floor is human-recovery only.
    let err = InjectError::NoTransport("sess-2".to_string());
    assert_eq!(
        on_inject_error(&err, true),
        LadderOutcome::HumanRecoveryOnly,
        "the verb must REFUSE an automatic PTY retry after a structured send"
    );
}

#[test]
fn healthy_acp_session_prefers_the_acp_tier() {
    // The tier-2-preferred invariant: a structured acp row with a live endpoint and no
    // degradation latch drives ACP (Mode-A), NOT the floor. (Forcing PtyFloor here is
    // the mutation that reds — the wrong-tier negative control.)
    assert_eq!(derive_tier("acp/claude-code", None, true), Tier::Acp);
}
