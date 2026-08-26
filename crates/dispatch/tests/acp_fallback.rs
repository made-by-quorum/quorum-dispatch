//! scoped-ACP-CC pillar 3 — the transport-loss disposition integration lane.
//!
//! Child D (opencode D1 — the Arm-B ratification):
//! `acp/claude-code` is a NAMED DIVERGENCE — transport loss REFUSES and
//! surfaces, with identity preserved in the qd-owned tombstone store
//! (`dispatch::tombstone`). This lane drives the REAL `AcpProvider` (resolved
//! via `provider_for("acp/claude-code")`) with ACP forced UNAVAILABLE
//! (`ProviderFx.acp_client = None` — the production "no connected bridge"
//! condition) and proves the pieces the verbs compose: the loss surfaces as a
//! real `InjectError::NoTransport` (never a hang), the tier derivation lands
//! every unavailable shape in the refusal lane (no auto-deliver tier exists),
//! the identity record round-trips the qd-owned store, and the exactly-once
//! dispatch marker hook is preserved intact. Deterministic — no live bridge.

use dispatch::effects::MapEnv;
use dispatch::paths::QdPaths;
use dispatch::provider::acp::{derive_tier, Tier};
use dispatch::provider::{provider_for, InjectError, ProviderFx, SessionKey};
use dispatch::tombstone::{self, IdentityTombstone};
use quorum_qw::lane::Mode;
use tempfile::TempDir;

/// Build a minimal `ProviderFx` with `acp_client` set as given (None = ACP unavailable).
fn fx_with_acp<'a>(
    env: &'a MapEnv,
    paths: &'a QdPaths,
    socket_dir: std::path::PathBuf,
    acp_client: Option<&'a dyn dispatch::provider::acp::AcpClient>,
) -> ProviderFx<'a> {
    fx_with_acp_and_pre_dispatch(env, paths, socket_dir, acp_client, None)
}

/// Same as [`fx_with_acp`] but with an explicit `acp_pre_dispatch` hook (Child B,
/// opencode D1's exactly-once dispatch-timing guard — kept intact under Child D).
fn fx_with_acp_and_pre_dispatch<'a>(
    env: &'a MapEnv,
    paths: &'a QdPaths,
    socket_dir: std::path::PathBuf,
    acp_client: Option<&'a dyn dispatch::provider::acp::AcpClient>,
    acp_pre_dispatch: Option<&'a dyn Fn()>,
) -> ProviderFx<'a> {
    ProviderFx {
        await_relay: None,
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
        pi_rpc: None,
        acp_pre_dispatch,
    }
}

#[test]
fn acp_unavailable_inject_errors_no_transport_and_never_hangs() {
    let tmp = TempDir::new().unwrap();
    let env = MapEnv::default();
    let paths = QdPaths::from_home(tmp.path());
    // ACP forced unavailable: no connected bridge client on the fx.
    let fx = fx_with_acp(&env, &paths, tmp.path().to_path_buf(), None);

    let provider = provider_for("acp/claude-code").expect("acp/claude-code registered");
    let key = SessionKey {
        id: "sess-loss-1",
        name: Some("acp-loss"),
        cwd: Some("/work/proj"),
        pid: None,
    };

    // The REAL provider inject signals ACP-unavailable as NoTransport (mod.rs maps
    // a missing acp_client → InjectError::NoTransport). This is the loss signal
    // the verb layer converts into refuse-with-identity-preserved — never a
    // silent hang, and (Child D) never an auto-deliver.
    let err = provider
        .inject(&fx, &key, "deliver me", "fallback-test")
        .expect_err("ACP unavailable → inject must error, not hang");
    assert!(
        matches!(err, InjectError::NoTransport(_)),
        "ACP-unavailable surfaces as NoTransport, got {err:?}"
    );
}

#[test]
fn every_unavailable_shape_lands_in_the_refusal_lane() {
    // Child D: there is no auto-deliver tier. A dead endpoint, a historical
    // pty latch (a never-deployed Child-B dev binary's write), and a lane that
    // is not ACP at all derive Unavailable — the verbs' refusal lane.
    //
    // The first parameter is a `Mode`, and the change from the provider-id string
    // is what makes the last case below mean anything. `derive_tier` used to take
    // an id and prefix-test it for `acp/`; both production callers passed the
    // literal `"acp/claude-code"`, so the test was against a constant and could
    // not fail. Now that a real ACP row records `provider: "claude-code"`, that
    // string would answer "not ACP" for the one lane that is — a false negative
    // that costs the healthy path its structured transport. Asking the MODE is
    // asking what those callers always meant.
    assert_eq!(
        derive_tier(Mode::Acp, None, false),
        Tier::Unavailable,
        "dead endpoint → refusal lane (never hang, never auto-deliver)"
    );
    assert_eq!(
        derive_tier(Mode::Acp, Some("pty"), true),
        Tier::Unavailable,
        "a historically-latched row is never treated as structured"
    );
    assert_eq!(
        // codex's daemon lane stands for "every lane that is not ACP". The tier is
        // a property of the TOPOLOGY, not of the program: `claude-code/mux-pane`
        // has no ACP tier either, and it shares a provider id with the lane that
        // does — which is exactly why this argument stopped being a provider id.
        derive_tier(Mode::Daemon, None, true),
        Tier::Unavailable,
        "a non-ACP lane has no ACP tier, however alive its endpoint is"
    );
}

#[test]
fn healthy_acp_session_prefers_the_acp_tier() {
    // The structured-preferred invariant: a healthy acp row with a live
    // endpoint and no latch drives ACP (Mode-A) — S5's healthy-path safety.
    assert_eq!(derive_tier(Mode::Acp, None, true), Tier::Acp);
}

#[test]
fn lost_identity_round_trips_the_qd_owned_store() {
    // The identity-preservation half of the disposition: the record a verb
    // writes at loss time lands under the qd-owned state_dir (never
    // ~/.claude/sessions — the claude CLI's janitor reaps that store) and
    // reads back complete.
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let record = IdentityTombstone {
        session_id: "4eb14ae0-aaaa-bbbb-cccc-000000000001".to_string(),
        name: Some("conf-d-a1".to_string()),
        pid: Some(332267),
        cwd: Some("/work/proj".to_string()),
        provider: "acp/claude-code".to_string(),
        endpoint: Some("ws://127.0.0.1:41999".to_string()),
        transcript: Some("/home/u/.claude/projects/-work-proj/4eb14ae0.jsonl".to_string()),
        loss_reason: "acp endpoint not reachable at send entry".to_string(),
        ..IdentityTombstone::default()
    };
    let path = tombstone::record_loss(&state_dir, record, 1_700_000_000_000).unwrap();
    assert!(
        path.starts_with(&state_dir),
        "the record lives under the qd-owned state_dir: {path:?}"
    );

    let read = tombstone::read_tombstone(&state_dir, "4eb14ae0-aaaa-bbbb-cccc-000000000001")
        .expect("identity record readable after loss");
    assert_eq!(read.name.as_deref(), Some("conf-d-a1"));
    assert_eq!(read.endpoint.as_deref(), Some("ws://127.0.0.1:41999"));
    assert_eq!(read.loss_reason, "acp endpoint not reachable at send entry");
    assert_eq!(read.first_recorded_at_ms, 1_700_000_000_000);

    // A repeated loss (send then wait racing, or the next day's retry) keeps
    // the first-loss timestamp and refreshes the latest — identity is never
    // erased by re-observation.
    let again = IdentityTombstone {
        session_id: "4eb14ae0-aaaa-bbbb-cccc-000000000001".to_string(),
        provider: "acp/claude-code".to_string(),
        loss_reason: "acp session has no live daemon pid at wait entry".to_string(),
        ..IdentityTombstone::default()
    };
    tombstone::record_loss(&state_dir, again, 1_700_000_099_000).unwrap();
    let read = tombstone::read_tombstone(&state_dir, "4eb14ae0-aaaa-bbbb-cccc-000000000001").unwrap();
    assert_eq!(read.first_recorded_at_ms, 1_700_000_000_000, "first loss survives");
    assert_eq!(read.recorded_at_ms, 1_700_000_099_000, "latest loss reflected");
}

/// A fake `AcpClient` whose `prompt` invokes `on_dispatched` immediately (mirroring
/// `AcpConnection::prompt`'s real dispatch-timing contract) before returning `Ok`.
struct FakeDispatchingClient;
impl dispatch::provider::acp::AcpClient for FakeDispatchingClient {
    fn initialize(&self) -> Result<dispatch::provider::acp::InitializeResult, dispatch::provider::acp::AcpError> {
        unimplemented!()
    }
    fn new_session(&self, _cwd: &str) -> Result<String, dispatch::provider::acp::AcpError> {
        unimplemented!()
    }
    fn prompt(
        &self,
        _session: &str,
        _text: &str,
        _from: &str,
        on_dispatched: &dyn Fn(),
    ) -> Result<String, dispatch::provider::acp::AcpError> {
        on_dispatched();
        Ok("turn-1".to_string())
    }
    fn cancel(&self, _session: &str) -> Result<(), dispatch::provider::acp::AcpError> {
        unimplemented!()
    }
    fn next_update(
        &self,
        _timeout: std::time::Duration,
    ) -> Result<Option<dispatch::provider::acp::AcpEvent>, dispatch::provider::acp::AcpError> {
        unimplemented!()
    }
}

#[test]
fn inject_threads_the_pre_dispatch_hook_through_to_prompt() {
    // Child B's exactly-once guard, KEPT under Child D: `AcpProvider::inject`
    // must pass `fx.acp_pre_dispatch` through to the connected client's
    // `prompt` call — the verb layer durably persists
    // `structured_send_issued=true` at the exact moment a structured send is
    // dispatched. The disposition no longer branches on it (pre- and post-send
    // loss both refuse), but the marker stays truth about the wire history and
    // is consumed by the resume seam.
    let tmp = TempDir::new().unwrap();
    let env = MapEnv::default();
    let paths = QdPaths::from_home(tmp.path());
    let client = FakeDispatchingClient;

    let fired = std::cell::Cell::new(false);
    let hook = || fired.set(true);
    let fx = fx_with_acp_and_pre_dispatch(
        &env,
        &paths,
        tmp.path().to_path_buf(),
        Some(&client),
        Some(&hook),
    );

    let provider = provider_for("acp/claude-code").expect("acp/claude-code registered");
    let key = SessionKey {
        id: "sess-hook-1",
        name: Some("acp-hook"),
        cwd: Some("/work/proj"),
        pid: None,
    };
    let result = provider.inject(&fx, &key, "hello", "hook-test");
    assert!(result.is_ok(), "the fake client's prompt always succeeds");
    assert!(
        fired.get(),
        "inject must invoke fx.acp_pre_dispatch through to AcpClient::prompt's on_dispatched"
    );
}

#[test]
fn inject_defaults_to_a_noop_hook_when_none_supplied() {
    // Every existing caller (tests, non-ACP-send lanes) supplies `acp_pre_dispatch:
    // None` — inject must not panic or require the hook.
    let tmp = TempDir::new().unwrap();
    let env = MapEnv::default();
    let paths = QdPaths::from_home(tmp.path());
    let client = FakeDispatchingClient;
    let fx = fx_with_acp(&env, &paths, tmp.path().to_path_buf(), Some(&client));

    let provider = provider_for("acp/claude-code").expect("acp/claude-code registered");
    let key = SessionKey {
        id: "sess-hook-2",
        name: Some("acp-hook"),
        cwd: Some("/work/proj"),
        pid: None,
    };
    assert!(provider.inject(&fx, &key, "hello", "hook-test").is_ok());
}
