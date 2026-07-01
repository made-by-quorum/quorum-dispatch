//! Provider-seam conformance lane (codex-p1-spec section 6.2).
//!
//! ONE generic harness `conformance(p, fx)` runs against BOTH provider impls
//! (`ClaudeProvider`, jail-fixture-backed; `FixtureDaemonProvider`, the R3
//! daemon-shaped fixture). The harness pins, per concern: a launch plan is
//! produced; readiness is reached; status maps to the canonical enum; a
//! transcript is located by key + stats parsed; resume args are stable across a
//! resume; inject returns an id.
//!
//! Then the NEGATIVE CONTROLS (codex-p1-spec section 6.2 + 6.3), each a named
//! test with a mutation-evidence comment: the trait fits a non-claude shape
//! because making it claude-shaped reds a specific test. These are the standing
//! answer to the ADD-8 refutations (the trait only fits claude; the daemon
//! fixture is secretly claude-shaped; shared/dead parsers).
//!
//! Rule 9 / L9a: every file path here is under a hermetic `tempfile` root — no
//! real home, no real registry, no live network.

use std::path::Path;

use std::cell::RefCell;

use dispatch::boot::{BootPhase, RealSleeper};
use dispatch::effects::{FixedClock, MapEnv};
use dispatch::model::RelayHealth;
use dispatch::model::SessionStatus;
use dispatch::mux::FixtureMux;
use dispatch::paths::QdPaths;
use dispatch::provider::codex::rpc::{
    AppServerRpc, ClientInfo, InitializeResult, Notification, RpcError, SteerOutcome,
};
use dispatch::provider::acp::{self, AcpClient, AcpError, AcpEvent};
use dispatch::provider::codex::CodexProvider;
use dispatch::provider::{
    provider_for, ClaudeProvider, FixtureDaemonProvider, Hosting, InjectError, LaunchRequest,
    Provider, ProviderFx, SessionKey,
};
use dispatch::relay::{RelayContract, RelayError, RelayReply};
use tempfile::TempDir;

/// A fixture [`AppServerRpc`] for the codex conformance run (codex-p2-spec
/// section 6 — "a FIXTURE AppServerRpc impl"). Records calls + returns canned
/// ids; `&self` (interior mutability via `RefCell`) matches the W3 contract. NO
/// socket, NO network — it drives `CodexProvider::{boot_waiter, inject}` offline.
struct FixtureRpc {
    /// initialize succeeds when true; false models a daemon that never handshakes.
    init_ok: bool,
    /// The turn id `turn_start` returns (the inject conformance expects it).
    turn_id: String,
    /// Audit: every turn_start text seen (proves inject drove turn/start).
    started: RefCell<Vec<String>>,
}

impl FixtureRpc {
    fn ready(turn_id: &str) -> Self {
        Self {
            init_ok: true,
            turn_id: turn_id.to_string(),
            started: RefCell::new(Vec::new()),
        }
    }
}

impl AppServerRpc for FixtureRpc {
    fn initialize(&self, _client: &ClientInfo) -> Result<InitializeResult, RpcError> {
        if self.init_ok {
            Ok(InitializeResult::default())
        } else {
            Err(RpcError::Transport("fixture: initialize refused".into()))
        }
    }
    fn initialized(&self) -> Result<(), RpcError> {
        Ok(())
    }
    fn thread_start(&self, _cwd: &str, _ap: &str, _qd: &str) -> Result<String, RpcError> {
        Ok("fixture-thread-1".to_string())
    }
    fn thread_resume(&self, _thread_id: &str) -> Result<(), RpcError> {
        Ok(())
    }
    fn turn_start(&self, _thread_id: &str, text: &str) -> Result<String, RpcError> {
        self.started.borrow_mut().push(text.to_string());
        Ok(self.turn_id.clone())
    }
    fn turn_steer(&self, _t: &str, _e: &str, _x: &str) -> Result<SteerOutcome, RpcError> {
        Ok(SteerOutcome::Steered(self.turn_id.clone()))
    }
    fn turn_interrupt(&self, _t: &str, _turn: &str) -> Result<(), RpcError> {
        Ok(())
    }
    fn next_notification(
        &self,
        _timeout: std::time::Duration,
    ) -> Result<Option<Notification>, RpcError> {
        Ok(None)
    }
    fn close(&self) -> Result<(), RpcError> {
        Ok(())
    }
}

/// A fixture [`AcpClient`] for the ACP/Claude-Code conformance run — the in-process ACP client
/// the A3-ACP stop-condition requires (proves the queue + completion contract WITHOUT a live
/// bridge). `&self` interior-mutability like `FixtureRpc`; NO subprocess, NO stdio. `initialize`
/// succeeds (auth not required) so boot reaches readiness; `prompt` returns a canned turn id (the
/// inject conformance expects it); `next_update` is quiet.
struct FixtureAcp {
    turn_id: String,
    /// Audit: every prompt text seen (proves inject drove the ACP send path).
    sent: RefCell<Vec<String>>,
}

impl FixtureAcp {
    fn ready(turn_id: &str) -> Self {
        Self {
            turn_id: turn_id.to_string(),
            sent: RefCell::new(Vec::new()),
        }
    }
}

impl AcpClient for FixtureAcp {
    fn initialize(&self) -> Result<acp::InitializeResult, AcpError> {
        Ok(acp::InitializeResult {
            protocol_version: 1,
            agent_name: Some("@zed-industries/claude-code-acp".to_string()),
            agent_version: Some("0.16.2".to_string()),
            auth_required: false,
        })
    }
    fn new_session(&self, _cwd: &str) -> Result<String, AcpError> {
        Ok("acp-sess-conf-1".to_string())
    }
    fn prompt(&self, _session: &str, text: &str, _from: &str) -> Result<String, AcpError> {
        self.sent.borrow_mut().push(text.to_string());
        Ok(self.turn_id.clone())
    }
    fn cancel(&self, _session: &str) -> Result<(), AcpError> {
        Ok(())
    }
    fn next_update(
        &self,
        _timeout: std::time::Duration,
    ) -> Result<Option<AcpEvent>, AcpError> {
        Ok(None)
    }
}

// ===========================================================================
// Test doubles + per-impl fixture setup.
// ===========================================================================

/// A relay contract double for the claude inject path: records the send and
/// returns a canned message id (the ADD-5 contract surface — `inject` sends
/// through THIS, never a real socket).
struct FakeRelay {
    message_id: String,
}

impl RelayContract for FakeRelay {
    fn send_message(&self, _port: u16, _text: &str, _from: &str) -> Result<String, RelayError> {
        Ok(self.message_id.clone())
    }
    fn fetch_reply(&self, _p: u16, _id: &str, _t: u64) -> Result<RelayReply, RelayError> {
        unreachable!("conformance never long-polls")
    }
    fn health(&self, _p: u16, _t: u64) -> Result<RelayHealth, RelayError> {
        unreachable!("conformance never probes health")
    }
}

/// Everything a per-impl conformance run needs that is NOT the provider itself:
/// the owned backing objects + the data the harness asserts against.
struct Fixture {
    _tmp: TempDir,
    paths: QdPaths,
    env: MapEnv,
    clock: FixedClock,
    sleeper: RealSleeper,
    mux: FixtureMux,
    relay: FakeRelay,
    /// The state root the transcript concern is keyed under (claude: projects
    /// dir; daemon: its date-tree root).
    transcript_root: std::path::PathBuf,
    /// The session id/thread id this fixture seeds a transcript + boot for.
    id: String,
    /// The cwd carried in the SessionKey (claude uses it for the slug; the
    /// daemon must NOT put it in the key — the negative control).
    cwd: String,
    /// Canned status raw signal this impl maps to a KNOWN status (claude: a
    /// JSON string; daemon: a notification object) + the status it maps to.
    status_raw: serde_json::Value,
    expected_status: SessionStatus,
    /// The raw signal the OTHER impl uses — fed here it must map to None
    /// (cross-feed negative control).
    foreign_status_raw: serde_json::Value,
    expected_message_id: String,
    /// The codex transport contract (None for the claude/daemon lanes — they
    /// never speak app-server). Owned here so `fx()` can borrow it as `&dyn`.
    app_server: Option<FixtureRpc>,
    /// The ACP transport contract (Some only for the acp lane — claude/daemon/codex
    /// never speak ACP). Owned here so `fx()` can borrow it as `&dyn AcpClient`.
    acp_client: Option<FixtureAcp>,
    /// What `provider.transcript_root(&fx)` MUST return (codex-p2-spec section
    /// 6.4): claude = projects_dir; daemon = its constructor-held root; codex =
    /// `$CODEX_HOME/sessions`. Pinned in the conformance harness for all three.
    expected_transcript_root: std::path::PathBuf,
}

impl Fixture {
    fn fx(&self) -> ProviderFx<'_> {
        ProviderFx {
            env: &self.env,
            paths: &self.paths,
            socket_dir: self.paths.home.join("zmx-501"),
            mux: Some(&self.mux),
            clock: Some(&self.clock),
            sleeper: Some(&self.sleeper),
            relay: Some(&self.relay),
            relay_port: Some(8901),
            app_server: self.app_server.as_ref().map(|r| r as &dyn AppServerRpc),
            // The conformance lane drives the believed-IDLE SEND path (turn/start);
            // the W6 steer/stale-fence ladder has its own dedicated tests below.
            codex_expected_turn_id: None,
            acp_client: self.acp_client.as_ref().map(|c| c as &dyn AcpClient),
            pi_rpc: None,
        }
    }
}

/// Build the claude fixture: a real PID file under sessions_dir (so the boot
/// waiter reaches idle) + a real cwd-slug transcript under projects_dir.
fn claude_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let paths = QdPaths::from_home(&home);
    let id = "abc-123-claude".to_string();
    let cwd = "/work/proj".to_string();

    // PID file → the EventBootWaiter's phase-1 scan finds it idle immediately.
    // (Fix-A's relay-sidecar phase is opt-in via QD_BOOT_AWAIT_RELAY and OFF here,
    // so conformance keeps the pid+idle readiness — no sidecar needed.)
    std::fs::create_dir_all(&paths.sessions_dir).unwrap();
    std::fs::write(
        paths.sessions_dir.join("100.json"),
        format!(r#"{{"pid":100,"name":"wk","sessionId":"{id}","status":"idle"}}"#),
    )
    .unwrap();

    // cwd-slug transcript: <projects>/<cwd-slug>/<id>.jsonl.
    let slug = cwd.replace('/', "-");
    let proj = paths.projects_dir.join(&slug);
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join(format!("{id}.jsonl")),
        // A claude-shaped JSONL: a user turn + a turn_duration system record.
        "{\"type\":\"user\",\"cwd\":\"/work/proj\",\"timestamp\":\"2026-06-06T10:00:00.000Z\",\"message\":{\"content\":\"hi\"}}\n\
         {\"type\":\"system\",\"subtype\":\"turn_duration\",\"timestamp\":\"2026-06-06T10:00:01.000Z\"}\n",
    )
    .unwrap();

    let transcript_root = paths.projects_dir.clone();
    let expected_transcript_root = transcript_root.clone();
    Fixture {
        _tmp: tmp,
        paths,
        env: MapEnv::default(),
        clock: FixedClock(0),
        sleeper: RealSleeper,
        mux: FixtureMux::new(),
        relay: FakeRelay {
            message_id: "msg-claude-1".to_string(),
        },
        transcript_root,
        id,
        cwd,
        // claude raw = the registry status STRING.
        status_raw: serde_json::json!("busy"),
        expected_status: SessionStatus::Busy,
        // The daemon's notification object fed to claude → None.
        foreign_status_raw: serde_json::json!({
            "method": "thread/status/changed",
            "params": {"status": "idle"}
        }),
        expected_message_id: "msg-claude-1".to_string(),
        app_server: None, // claude never speaks app-server.
        // claude transcript_root == fx.paths.projects_dir (== transcript_root).
        expected_transcript_root,
        acp_client: None,
    }
}

/// Build the ACP/Claude-Code fixture: a CC-shaped transcript under projects_dir (the bridge runs
/// the REAL CC engine, so transcripts land where a normal CC session's do) + an in-process
/// [`FixtureAcp`] client. **No pid file** — boot readiness is the ACP `initialize` handshake
/// (proving the ACP boot path does NOT depend on a pid, R3). status raw is ACP-shaped; a claude
/// STRING fed here → None (cross-feed). inject returns the FixtureAcp turn id.
fn acp_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let paths = QdPaths::from_home(&home);
    let id = "abc-123-acp".to_string();
    let cwd = "/work/proj".to_string();

    // CC-shaped JSONL under the projects dir (ACP-driven CC writes the standard CC transcript).
    let slug = cwd.replace('/', "-");
    let proj = paths.projects_dir.join(&slug);
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join(format!("{id}.jsonl")),
        "{\"type\":\"user\",\"cwd\":\"/work/proj\",\"timestamp\":\"2026-06-06T10:00:00.000Z\",\"message\":{\"content\":\"hi\"}}\n\
         {\"type\":\"system\",\"subtype\":\"turn_duration\",\"timestamp\":\"2026-06-06T10:00:01.000Z\"}\n",
    )
    .unwrap();

    let transcript_root = paths.projects_dir.clone();
    let expected_transcript_root = transcript_root.clone();
    Fixture {
        _tmp: tmp,
        paths,
        env: MapEnv::default(),
        clock: FixedClock(0),
        sleeper: RealSleeper,
        mux: FixtureMux::new(),
        relay: FakeRelay {
            message_id: "unused-acp".to_string(),
        },
        transcript_root,
        id,
        cwd,
        // ACP raw = the host-synthesized session-state object.
        status_raw: serde_json::json!({"acpSessionState": "busy"}),
        expected_status: SessionStatus::Busy,
        // A claude registry STRING fed to the ACP impl → None (no shared parser).
        foreign_status_raw: serde_json::json!("idle"),
        // inject returns the FixtureAcp turn id (the ACP attributable id, not a relay msg id).
        expected_message_id: "acp-turn-conf-1".to_string(),
        app_server: None, // ACP never speaks app-server.
        acp_client: Some(FixtureAcp::ready("acp-turn-conf-1")),
        // ACP transcript_root == fx.paths.projects_dir (shared CC engine, by identity).
        expected_transcript_root,
    }
}

/// Build the daemon fixture: a date-keyed rollout-shaped transcript under its
/// own root. Boot readiness is the handshake record (no pid file).
fn daemon_fixture() -> (Fixture, FixtureDaemonProvider) {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let paths = QdPaths::from_home(&home);
    let id = "thread-seed-9".to_string();
    let cwd = "/work/proj".to_string();
    // The daemon keys <root>/2026/06/06/<thread-id>.jsonl. Seed the exact path
    // transcript_path will return for this id. The provider's constructor-held
    // transcript_root is pinned to this same seeded tree (codex-p2-spec section
    // 6.4: the daemon root is provider-OWNED state, not from fx).
    let transcript_root = tmp.path().join("rollouts");
    let provider = FixtureDaemonProvider::with_root(transcript_root.clone());
    let key = SessionKey {
        id: &id,
        name: None,
        cwd: Some(&cwd),
        pid: None,
    };
    let path = provider.transcript_path(&transcript_root, &key).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        // rollout-SHAPED: session-meta first line + turn events.
        "{\"type\":\"session-meta\",\"title\":\"My Thread\"}\n\
         {\"type\":\"turn\",\"role\":\"user\",\"text\":\"hi\",\"timestamp\":\"2026-06-06T10:00:00.000Z\"}\n\
         {\"type\":\"turn\",\"role\":\"assistant\",\"text\":\"yo\",\"timestamp\":\"2026-06-06T10:00:01.000Z\"}\n",
    )
    .unwrap();

    let expected_transcript_root = transcript_root.clone();
    let fixture = Fixture {
        _tmp: tmp,
        paths,
        env: MapEnv::default(),
        clock: FixedClock(0),
        sleeper: RealSleeper,
        mux: FixtureMux::new(),
        relay: FakeRelay {
            message_id: "unused-daemon".to_string(),
        },
        transcript_root,
        id,
        cwd,
        // daemon raw = a notification OBJECT.
        status_raw: serde_json::json!({
            "method": "thread/status/changed",
            "params": {"status": {"active": {"activeFlags": ["waitingOnApproval"]}}}
        }),
        expected_status: SessionStatus::Busy,
        // A claude registry STRING fed to the daemon → None.
        foreign_status_raw: serde_json::json!("idle"),
        // The daemon inject returns a TURN id, not a relay message id; the first
        // enqueue mints turn-00000001.
        expected_message_id: "turn-00000001".to_string(),
        app_server: None, // the fixture-daemon uses its OWN internal turn queue.
        // The daemon's constructor-held root (pinned to the seeded tree).
        expected_transcript_root,
        acp_client: None,
    };
    (fixture, provider)
}

// ===========================================================================
// The generic conformance harness (run against BOTH impls).
// ===========================================================================

/// Pins every shared concern of the [`Provider`] trait against one impl + its
/// fixture (codex-p1-spec section 6.2). Run from both `conformance_claude` and
/// `conformance_daemon`.
fn conformance(p: &dyn Provider, fix: &Fixture) {
    let fx = fix.fx();
    let key = SessionKey {
        id: &fix.id,
        name: Some("wk"),
        cwd: Some(&fix.cwd),
        pid: None, // pid is OPTIONAL — nothing in the trait may require it (R3).
    };

    // id() is a non-empty stable string.
    assert!(!p.id().is_empty(), "provider id must be non-empty");

    // launch_plan produces a non-empty argv.
    let req = LaunchRequest {
        name: "wk".to_string(),
        cwd: Some(fix.cwd.clone()),
        resume: None,
        fork: false,
        agent: None,
        model: None,
        passthrough: vec![],
    };
    let plan = p.launch_plan(&fx, &req);
    assert!(
        !plan.argv.is_empty(),
        "launch_plan must produce a non-empty argv"
    );

    // transcript_root (codex-p2-spec section 6.4 — the ONE recorded signature
    // addition) is the impl's OWN root: claude = projects_dir; daemon =
    // constructor-held; codex = $CODEX_HOME/sessions. All three pinned here.
    assert_eq!(
        p.transcript_root(&fx),
        fix.expected_transcript_root,
        "transcript_root must be this impl's own root"
    );

    // readiness reached (claude: pid file idle; daemon: handshake record).
    let waiter = p.boot_waiter(&fx);
    assert!(
        waiter.wait_ready("wk").is_ok(),
        "boot waiter must reach readiness"
    );

    // status mapped to the canonical enum from this impl's native raw signal.
    assert_eq!(
        p.parse_status(&fix.status_raw),
        Some(fix.expected_status),
        "native raw signal must map to the canonical status"
    );
    // ...and the OTHER impl's raw shape fed here maps to None (cross-feed: each
    // impl owns a DISTINCT raw interpretation — no shared/secret parser).
    assert_eq!(
        p.parse_status(&fix.foreign_status_raw),
        None,
        "the foreign impl's raw shape must not parse here (no shared parser)"
    );

    // transcript located by key + stats parsed (turns counted).
    let path = p
        .transcript_path(&fix.transcript_root, &key)
        .expect("transcript_path must locate the seeded transcript");
    assert!(
        path.exists(),
        "located transcript path must exist: {path:?}"
    );
    let stats = p.transcript_stats(&path, true);
    assert!(stats.turns >= 1, "transcript_stats must count >=1 turn");

    // scan_transcripts finds the seeded transcript by id.
    let metas = p.scan_transcripts(&fix.transcript_root);
    assert!(
        metas.iter().any(|m| m.session_id == fix.id),
        "scan_transcripts must surface the seeded transcript id"
    );

    // resume args stable across resume (same args twice; id preserved).
    let r1 = p.resume_args(&key, false);
    let r2 = p.resume_args(&key, false);
    assert_eq!(r1, r2, "resume args must be stable across calls");
    assert!(
        r1.iter().any(|a| a == &fix.id),
        "resume args must carry the session/thread id"
    );

    // inject returns an id.
    let injected = p
        .inject(&fx, &key, "hello", "cli")
        .expect("inject must return an id");
    assert_eq!(
        injected, fix.expected_message_id,
        "inject must return the expected id"
    );
}

/// Build the codex fixture (codex-p2-spec section 6 conformance extension): a
/// TempDir `$CODEX_HOME` with a real date-keyed rollout tree + a FIXTURE
/// AppServerRpc. `fx.env` carries `CODEX_HOME` so `transcript_root` resolves to
/// `$CODEX_HOME/sessions`; the rollout filename's uuid IS `fix.id` so
/// transcript_path/scan locate it. NO sqlite db is minted — transcript_path
/// degrades to the date-walk tier (index tier 1 misses, tier 2 hits).
fn codex_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let paths = QdPaths::from_home(&home);
    // A real uuidv7 (cited from the rss-jail rollout) — the thread id / SessionKey
    // id; the rollout filename embeds it.
    let id = "019ea0b3-04d3-7400-8d95-f55d41e961e4".to_string();
    let cwd = "/work/proj".to_string();

    // $CODEX_HOME = <tmp>/codex-home; the rollout tree is $CODEX_HOME/sessions.
    let codex_home = tmp.path().join("codex-home");
    let sessions = codex_home.join("sessions");
    let day = sessions.join("2026").join("06").join("07");
    std::fs::create_dir_all(&day).unwrap();
    // The on-disk rollout filename: rollout-<ISO-ts>-<uuid>.jsonl.
    let fname = format!("rollout-2026-06-07T02-09-07-{id}.jsonl");
    std::fs::write(
        day.join(&fname),
        // The REAL rollout taxonomy (event_msg payload.type discriminator): a
        // session_meta, a task_started + matching task_complete (one turn), an
        // agent_message (preview source).
        format!(
            "{{\"timestamp\":\"2026-06-07T06:09:26.889Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/work/proj\"}}}}\n\
             {{\"timestamp\":\"2026-06-07T06:09:26.899Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"t1\"}}}}\n\
             {{\"timestamp\":\"2026-06-07T06:09:37.100Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"agent_message\",\"message\":\"RSS-PROBE\"}}}}\n\
             {{\"timestamp\":\"2026-06-07T06:09:37.105Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t1\",\"last_agent_message\":\"RSS-PROBE\"}}}}\n"
        ),
    )
    .unwrap();

    // env carries CODEX_HOME → transcript_root resolves to $CODEX_HOME/sessions.
    let mut env = MapEnv::default();
    env.vars.insert(
        "CODEX_HOME".to_string(),
        codex_home.to_string_lossy().into_owned(),
    );

    let transcript_root = sessions;
    let expected_transcript_root = transcript_root.clone();
    Fixture {
        _tmp: tmp,
        paths,
        env,
        clock: FixedClock(0),
        sleeper: RealSleeper,
        mux: FixtureMux::new(),
        relay: FakeRelay {
            message_id: "unused-codex".to_string(),
        },
        transcript_root,
        id,
        cwd,
        // codex raw = the REAL thread/status/changed notification OBJECT
        // (mined from q1c-clientA-events.jsonl): status.type discriminator.
        status_raw: serde_json::json!({
            "method": "thread/status/changed",
            "params": {"threadId": "t", "status": {"type": "active", "activeFlags": []}}
        }),
        expected_status: SessionStatus::Busy, // active → Busy.
        // A claude registry STRING fed to codex → None (cross-feed control).
        foreign_status_raw: serde_json::json!("idle"),
        // codex inject = turn/start via the fixture rpc → the canned turn id.
        expected_message_id: "codex-turn-77".to_string(),
        app_server: Some(FixtureRpc::ready("codex-turn-77")),
        expected_transcript_root,
        acp_client: None,
    }
}

#[test]
fn conformance_claude() {
    let fix = claude_fixture();
    conformance(&ClaudeProvider, &fix);
}

#[test]
fn conformance_daemon() {
    let (fix, provider) = daemon_fixture();
    conformance(&provider, &fix);
}

/// The generic conformance harness runs against `CodexProvider` backed by a
/// FIXTURE AppServerRpc + a TempDir rollout tree (codex-p2-spec section 6).
#[test]
fn conformance_codex() {
    let fix = codex_fixture();
    conformance(&CodexProvider, &fix);
}

/// scoped-ACP-CC (A3-ACP stop-condition (a)): the generic conformance harness runs against
/// `AcpProvider` backed by an in-process FIXTURE AcpClient — proving the queue + completion
/// contract (id/launch/transcript/status/resume/inject + ACP boot via `initialize`) WITHOUT a
/// live bridge. The negative controls below (`parse_status_cross_feed_returns_none`,
/// `daemon_pid_none_flows_through_every_method`) cover ACP too via the same shape.
#[test]
fn conformance_acp() {
    let fix = acp_fixture();
    conformance(&acp::AcpProvider, &fix);
}

// ===========================================================================
// Negative controls (codex-p1-spec section 6.2 + 6.3) — each a standing answer
// to an ADD-8 refutation, each with a mutation-evidence comment.
// ===========================================================================

/// `pid: None` flows through EVERY daemon-lane method (no unwrap-on-pid anywhere).
///
/// MUTATION EVIDENCE: making the fixture (or the trait) require a pid — e.g.
/// `key.pid.unwrap()` in any daemon method, or a `pid: i64` (non-Option) field on
/// SessionKey — reds this test (it would panic / fail to compile against a None
/// pid). This is the R3 tooth: nothing in the trait may require pid.
#[test]
fn daemon_pid_none_flows_through_every_method() {
    let (fix, provider) = daemon_fixture();
    let fx = fix.fx();
    let key = SessionKey {
        id: &fix.id,
        name: None,
        cwd: Some(&fix.cwd),
        pid: None, // the whole point.
    };
    // Every daemon-lane method, driven with pid: None — none may unwrap pid.
    let _ = provider.launch_plan(&fx, &LaunchRequest::default());
    let _ = provider.boot_waiter(&fx).wait_ready("wk");
    let _ = provider.parse_status(&fix.status_raw);
    let path = provider
        .transcript_path(&fix.transcript_root, &key)
        .unwrap();
    let _ = provider.transcript_stats(&path, false);
    let _ = provider.scan_transcripts(&fix.transcript_root);
    let _ = provider.resume_args(&key, false);
    let injected = provider.inject(&fx, &key, "m", "cli").unwrap();
    assert_eq!(injected, "turn-00000001");
}

/// The daemon transcript path contains NO cwd-derived component.
///
/// MUTATION EVIDENCE: keying the daemon transcript path by cwd-slug (the claude
/// shape) would make the asserted path contain the cwd segments — this test reds
/// it. The daemon keys by date + thread-id ONLY.
#[test]
fn daemon_transcript_path_has_no_cwd_component() {
    let provider = FixtureDaemonProvider::ready();
    let root = Path::new("/state/rollouts");
    // A cwd that, slugified, would appear in a claude-shaped key.
    let cwd = "/home/u/secret-project";
    let key = SessionKey {
        id: "thread-xyz",
        name: None,
        cwd: Some(cwd),
        pid: None,
    };
    let path = provider.transcript_path(root, &key).unwrap();
    let s = path.to_string_lossy();
    // NO part of the cwd (raw or slugified) may appear in the key.
    assert!(
        !s.contains("secret-project"),
        "daemon path must not contain a cwd component: {s}"
    );
    assert!(
        !s.contains("Users-eric"),
        "daemon path must not contain a cwd slug: {s}"
    );
    // It IS date+id keyed.
    assert!(s.contains("2026"), "daemon path is date-keyed: {s}");
    assert!(
        s.ends_with(".jsonl"),
        "daemon path ends in the thread-id jsonl: {s}"
    );
}

/// parse_status cross-feed: claude-shaped raw → daemon impl None; daemon
/// notification object → claude impl None.
///
/// MUTATION EVIDENCE: sharing ONE parser between the two impls (or swapping them)
/// reds this — a shared parser would accept the foreign shape and return Some.
/// The two impls own DISTINCT interpretations of "raw status".
#[test]
fn parse_status_cross_feed_returns_none() {
    let daemon = FixtureDaemonProvider::ready();
    let claude = ClaudeProvider;

    // A claude registry status STRING fed to the daemon → None.
    let claude_raw = serde_json::json!("idle");
    assert_eq!(
        daemon.parse_status(&claude_raw),
        None,
        "daemon must reject a claude registry status string"
    );

    // A daemon notification OBJECT fed to claude → None.
    let daemon_raw = serde_json::json!({
        "method": "thread/status/changed",
        "params": {"status": "idle"}
    });
    assert_eq!(
        claude.parse_status(&daemon_raw),
        None,
        "claude must reject a daemon notification object"
    );

    // Sanity: each accepts its OWN shape (so the None above is discrimination,
    // not a parser that rejects everything).
    assert_eq!(claude.parse_status(&claude_raw), Some(SessionStatus::Idle));
    assert_eq!(daemon.parse_status(&daemon_raw), Some(SessionStatus::Idle));
}

/// launch_plan against a MINIMAL ProviderFx (empty env, nonexistent config path,
/// no relay inputs) succeeds for the daemon impl — it consumes NO claude config
/// surface.
///
/// MUTATION EVIDENCE (rev B red-team (a) residue): if the daemon launch_plan read
/// a claude config surface (claude_flags off a config toml, claude_bin off env),
/// a minimal fx with a nonexistent config + empty env would still PRODUCE a plan
/// (claude_flags falls through to DEFAULT_FLAGS) — but it would carry claude
/// flags. This test asserts the daemon plan is its OWN shape (`fixtured`), proving
/// it never touched the claude surface.
#[test]
fn daemon_launch_plan_minimal_fx_consumes_no_claude_config() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("nonexistent-home");
    let paths = QdPaths::from_home(&home); // config toml path does not exist.
    let env = MapEnv::default(); // empty env.
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: home.join("zmx-501"),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    let provider = FixtureDaemonProvider::ready();
    let plan = provider.launch_plan(&fx, &LaunchRequest::default());
    assert_eq!(
        plan.argv,
        vec!["fixtured".to_string(), "app-server".to_string()],
        "daemon launch plan is its own direct argv, no claude flags"
    );
    // No claude DEFAULT_FLAGS leaked in.
    assert!(
        !plan.argv.iter().any(|a| a.contains("dangerously")),
        "daemon plan must not carry claude flags"
    );
}

/// The daemon refuses a STALE steer precondition with the typed error
/// (codex-p1-spec section 6.1).
///
/// MUTATION EVIDENCE: dropping the expected-turn-id precondition (always Ok) reds
/// this — a stale id would land instead of erroring. The steer is expressible
/// WITHOUT any port/sidecar concept (the claude-shaped contortion).
#[test]
fn daemon_steer_stale_precondition_is_typed_error() {
    let provider = FixtureDaemonProvider::ready();
    let fx_tmp = TempDir::new().unwrap();
    let paths = QdPaths::from_home(&fx_tmp.path().join("home"));
    let env = MapEnv::default();
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: fx_tmp.path().to_path_buf(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    let key = SessionKey {
        id: "thread-1",
        name: None,
        cwd: None,
        pid: None,
    };
    // Enqueue a turn so there IS an active turn.
    let turn = provider.inject(&fx, &key, "start", "cli").unwrap();
    assert_eq!(turn, "turn-00000001");

    // A correct steer (matching id) lands.
    assert_eq!(provider.steer(&turn, "more").unwrap(), turn);

    // A STALE steer (wrong expected id) → typed Precondition error.
    let err = provider.steer("turn-99999999", "more").unwrap_err();
    assert!(
        matches!(err, InjectError::Precondition(_)),
        "stale steer must be a typed Precondition error, got {err:?}"
    );
}

/// `provider_for`: "claude-code" resolves; "fixture-daemon" does NOT resolve from
/// the production registry; an unknown id → None.
///
/// MUTATION EVIDENCE: registering the fixture in `provider_for` would make
/// `qd new --provider fixture-daemon` bootable (P1 must not) — this test reds
/// that. The fixture is a CONFORMANCE construct, not a dispatchable provider.
/// DOCUMENTED CHOICE (codex-p1-spec section 2.3): the fixture is constructed
/// directly by the conformance lane, never via the registry.
#[test]
fn provider_for_registry_is_claude_only() {
    assert_eq!(
        provider_for("claude-code").map(|p| p.id()),
        Some("claude-code"),
        "claude-code resolves from the registry"
    );
    assert!(
        provider_for("fixture-daemon").is_none(),
        "the fixture daemon is NOT a registered/dispatchable provider"
    );
    assert!(
        provider_for("opencode").is_none(),
        "opencode is parked (verbs keep their branches), not a Provider impl"
    );
    assert!(
        provider_for("totally-unknown").is_none(),
        "an unknown id resolves to None (callers error loudly)"
    );
}

/// The claude launch_plan reproduces EXACTLY what the launch.rs functions build
/// for the same inputs (codex-p1-spec section 4 byte-identity obligation).
///
/// MUTATION EVIDENCE: any drift in the claude argv assembly (a reordered flag, a
/// dropped dedupe, a different bin resolution) reds this — the assertion compares
/// the trait output token-for-token against the frozen launch.rs helper output.
#[test]
fn claude_launch_plan_matches_launch_rs_helpers() {
    use dispatch::launch::{build_new_extra_args, claude_bin, claude_flags, NewOpts};

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let paths = QdPaths::from_home(&home);
    let env = MapEnv::default();
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: home.join("zmx-501"),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    let req = LaunchRequest {
        name: "wk".to_string(),
        cwd: Some("/work".to_string()),
        resume: Some("sess-1".to_string()),
        fork: true,
        agent: Some("reviewer".to_string()),
        model: None,
        passthrough: vec!["--model".to_string(), "opus".to_string()],
    };
    let plan = ClaudeProvider.launch_plan(&fx, &req);

    // Reconstruct the EXACT token list the create path builds today
    // (create.rs build_claude_command → claude_bin + claude_flags +
    // build_new_extra_args), pre-shell-assembly.
    let config = home.join(".quorum").join("dispatch").join("config.toml");
    let bin = claude_bin(&env);
    let flags = claude_flags(&env, &config);
    let opts = NewOpts {
        resume: Some("sess-1".to_string()),
        fork: true,
        agent: Some("reviewer".to_string()),
        model: None,
    };
    let extra = build_new_extra_args(
        "wk",
        &opts,
        &["--model".to_string(), "opus".to_string()],
        &flags,
    );
    let mut expected = vec![bin];
    expected.extend(flags);
    expected.extend(extra);

    assert_eq!(
        plan.argv, expected,
        "claude launch_plan argv must be byte-identical to the launch.rs assembly"
    );
    // claude env stays [] at the trait level (F1 session-env file is a create.rs
    // concern W3 keeps there) — documented in launch_plan.
    assert!(plan.env.is_empty(), "claude trait-level env is empty");
}

/// The claude impl maps each RelayError class through inject WITHOUT flattening
/// (W5 needs the structured class to reproduce send:relay's exact wording).
///
/// MUTATION EVIDENCE: collapsing InjectError::RelayFailed into a String would
/// red this — the test matches on the preserved RelayError variant.
#[test]
fn claude_inject_preserves_relay_error_class() {
    struct FailingRelay(RelayError);
    impl RelayContract for FailingRelay {
        fn send_message(&self, _p: u16, _t: &str, _f: &str) -> Result<String, RelayError> {
            Err(self.0.clone())
        }
        fn fetch_reply(&self, _p: u16, _id: &str, _t: u64) -> Result<RelayReply, RelayError> {
            unreachable!()
        }
        fn health(&self, _p: u16, _t: u64) -> Result<RelayHealth, RelayError> {
            unreachable!()
        }
    }
    let tmp = TempDir::new().unwrap();
    let paths = QdPaths::from_home(&tmp.path().join("home"));
    let env = MapEnv::default();
    let relay = FailingRelay(RelayError::Timeout);
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: tmp.path().to_path_buf(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: Some(&relay),
        relay_port: Some(8901),
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    let key = SessionKey {
        id: "s",
        name: None,
        cwd: None,
        pid: None,
    };
    let err = ClaudeProvider.inject(&fx, &key, "m", "cli").unwrap_err();
    match err {
        InjectError::RelayFailed(RelayError::Timeout) => {}
        other => panic!("expected preserved RelayError::Timeout, got {other:?}"),
    }

    // No port → NoTransport (send.ts:406-409 "has no relay." class).
    let fx_no_port = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: tmp.path().to_path_buf(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: Some(&relay),
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    assert!(matches!(
        ClaudeProvider
            .inject(&fx_no_port, &key, "m", "cli")
            .unwrap_err(),
        InjectError::NoTransport(_)
    ));
}

/// The daemon resume keeps the SAME id across resumes and carries NO fork flag
/// shape (contrast with claude's `--fork-session`).
///
/// MUTATION EVIDENCE: emitting a `--fork-session` shape (claude's) or a changed
/// id on the daemon resume reds this.
#[test]
fn daemon_and_claude_resume_shapes_differ() {
    let daemon = FixtureDaemonProvider::ready();
    let key = SessionKey {
        id: "thread-7",
        name: None,
        cwd: None,
        pid: None,
    };
    // Same id across fork=false and fork=true; no fork flag ever.
    assert_eq!(daemon.resume_args(&key, false), vec!["resume", "thread-7"]);
    assert_eq!(
        daemon.resume_args(&key, true),
        vec!["resume", "thread-7"],
        "daemon ignores fork — no --fork-session shape"
    );

    // Claude: --resume <id>, and --fork-session WHEN fork.
    let claude_key = SessionKey {
        id: "uuid-1",
        name: None,
        cwd: None,
        pid: None,
    };
    assert_eq!(
        ClaudeProvider.resume_args(&claude_key, false),
        vec!["--resume", "uuid-1"]
    );
    assert_eq!(
        ClaudeProvider.resume_args(&claude_key, true),
        vec!["--resume", "uuid-1", "--fork-session"]
    );
}

/// hosting() answers data, not a branch (R4): claude is MuxPane, the daemon is
/// Daemon. The seam EXPRESSES both modes.
#[test]
fn hosting_modes_are_both_expressible() {
    assert_eq!(ClaudeProvider.hosting(), Hosting::MuxPane);
    assert_eq!(FixtureDaemonProvider::ready().hosting(), Hosting::Daemon);
}

/// The daemon boot waiter fails via the handshake record, never a pid-file
/// timeout path — a daemon that never handshaked surfaces a handshake failure.
#[test]
fn daemon_boot_unready_fails_with_handshake_detail() {
    let provider = FixtureDaemonProvider::default(); // handshake_ready = false.
    let tmp = TempDir::new().unwrap();
    let paths = QdPaths::from_home(&tmp.path().join("home"));
    let env = MapEnv::default();
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: tmp.path().to_path_buf(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    let err = provider.boot_waiter(&fx).wait_ready("wk").unwrap_err();
    assert_eq!(err.phase, BootPhase::PidFile);
    assert!(
        err.detail.contains("handshake"),
        "daemon boot failure names the handshake, not a pid file: {}",
        err.detail
    );
}

// ===========================================================================
// W3 codex-specific negative controls (codex-p2-spec section 6 + §13 ledger).
// Each is a standing answer to an ADD-8 refutation against CodexProvider.
// ===========================================================================

/// `pid: None` flows through EVERY CodexProvider method (the R3 tooth extended to
/// codex, codex-p2-spec section 13 "pid requirement creeps into codex lane").
///
/// MUTATION EVIDENCE: any `key.pid.unwrap()` (or a pid: i64 non-Option) in a codex
/// method reds this — every method is driven with pid: None.
#[test]
fn codex_pid_none_flows_through_every_method() {
    let fix = codex_fixture();
    let fx = fix.fx();
    let key = SessionKey {
        id: &fix.id,
        name: None,
        cwd: Some(&fix.cwd),
        pid: None, // the whole point.
    };
    let _ = CodexProvider.launch_plan(&fx, &LaunchRequest::default());
    let _ = CodexProvider.boot_waiter(&fx).wait_ready("wk");
    let _ = CodexProvider.parse_status(&fix.status_raw);
    let _ = CodexProvider.transcript_root(&fx);
    let path = CodexProvider
        .transcript_path(&fix.transcript_root, &key)
        .expect("transcript_path locates the seeded rollout");
    let _ = CodexProvider.transcript_stats(&path, false);
    let _ = CodexProvider.scan_transcripts(&fix.transcript_root);
    let _ = CodexProvider.resume_args(&key, false);
    let injected = CodexProvider.inject(&fx, &key, "m", "cli").unwrap();
    assert_eq!(injected, "codex-turn-77");
}

/// The codex transcript path contains NO cwd-derived component (date+uuid keyed,
/// codex-p2-spec section 6.4).
///
/// MUTATION EVIDENCE: keying the codex transcript path by cwd-slug (the claude
/// shape) would make the path contain the cwd — this reds it. A cwd in the key
/// must NEVER appear in the resolved path.
#[test]
fn codex_transcript_path_has_no_cwd_component() {
    let fix = codex_fixture();
    let cwd = "/home/u/secret-codex-project";
    let key = SessionKey {
        id: &fix.id,
        name: None,
        cwd: Some(cwd), // a cwd that, slugified, would appear in a claude key.
        pid: None,
    };
    let path = CodexProvider
        .transcript_path(&fix.transcript_root, &key)
        .expect("transcript_path locates by uuid, ignoring cwd");
    let s = path.to_string_lossy();
    assert!(
        !s.contains("secret-codex-project"),
        "codex path must not contain a cwd component: {s}"
    );
    assert!(
        !s.contains("Users-eric"),
        "codex path must not contain a cwd slug: {s}"
    );
    // It IS the date+uuid rollout file.
    assert!(s.contains(&fix.id), "codex path is uuid-keyed: {s}");
    assert!(s.ends_with(".jsonl"));
}

/// parse_status cross-feed both directions (codex-p2-spec section 13 "shared
/// parser creep"): a claude registry STRING fed to codex → None; the codex
/// thread/status/changed notification fed to claude → None.
///
/// MUTATION EVIDENCE: sharing ONE parser between claude and codex (or swapping
/// them) reds this — a shared parser would accept the foreign shape and return
/// Some. The impls own DISTINCT raw interpretations.
#[test]
fn codex_claude_parse_status_cross_feed_returns_none() {
    let codex = CodexProvider;
    let claude = ClaudeProvider;

    // The REAL codex notification (status.type discriminator, mined from q1c).
    let codex_raw = serde_json::json!({
        "method": "thread/status/changed",
        "params": {"threadId": "t", "status": {"type": "idle"}}
    });
    // A claude registry STRING.
    let claude_raw = serde_json::json!("idle");

    // Cross-feeds → None.
    assert_eq!(
        codex.parse_status(&claude_raw),
        None,
        "codex must reject a claude registry status string"
    );
    assert_eq!(
        claude.parse_status(&codex_raw),
        None,
        "claude must reject a codex notification object"
    );
    // Sanity: each accepts its OWN shape (the None above is discrimination).
    assert_eq!(codex.parse_status(&codex_raw), Some(SessionStatus::Idle));
    assert_eq!(claude.parse_status(&claude_raw), Some(SessionStatus::Idle));
}

/// The codex status map (codex-p2-spec section 3.3 truth table): idle→Idle;
/// active→Busy (regardless of activeFlags); notLoaded→None; systemError→None.
///
/// MUTATION EVIDENCE (codex-p2-spec section 13 "status map flipped"): flipping
/// active→Idle (or idle→Busy) reds this — the assertions below pin each arm to
/// its canonical status by the REAL wire `status.type` discriminator.
#[test]
fn codex_status_map_truth_table() {
    let p = CodexProvider;
    let n = |status: serde_json::Value| -> serde_json::Value {
        serde_json::json!({
            "method": "thread/status/changed",
            "params": {"threadId": "t", "status": status}
        })
    };
    // idle → Idle.
    assert_eq!(
        p.parse_status(&n(serde_json::json!({"type": "idle"}))),
        Some(SessionStatus::Idle)
    );
    // active → Busy, regardless of activeFlags (empty AND non-empty).
    assert_eq!(
        p.parse_status(&n(serde_json::json!({"type": "active", "activeFlags": []}))),
        Some(SessionStatus::Busy),
        "active → Busy (a flip to Idle would red this)"
    );
    assert_eq!(
        p.parse_status(&n(
            serde_json::json!({"type": "active", "activeFlags": ["waitingOnApproval"]})
        )),
        Some(SessionStatus::Busy),
        "active → Busy regardless of activeFlags"
    );
    // notLoaded → None (caller fallback).
    assert_eq!(
        p.parse_status(&n(serde_json::json!({"type": "notLoaded"}))),
        None
    );
    // systemError → None (named residual).
    assert_eq!(
        p.parse_status(&n(serde_json::json!({"type": "systemError"}))),
        None
    );
    // A non-status notification method → None (not thread/status/changed).
    assert_eq!(
        p.parse_status(&serde_json::json!({
            "method": "turn/completed", "params": {"turnId": "x"}
        })),
        None
    );
}

/// codex launch_plan against a MINIMAL fx (empty env) succeeds with bin "codex"
/// (codex-p2-spec section 6.4 minimal-fx negative control).
///
/// MUTATION EVIDENCE: if codex launch_plan read a claude config surface
/// (claude_bin/claude_flags off env+config) it would NOT yield the bare
/// `[codex, app-server]` argv. The QD_CODEX_BIN override is also pinned.
#[test]
fn codex_launch_plan_minimal_fx_uses_codex_bin() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("nonexistent-home");
    let paths = QdPaths::from_home(&home);
    let env = MapEnv::default(); // empty env.
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: home.join("zmx-501"),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    let plan = CodexProvider.launch_plan(&fx, &LaunchRequest::default());
    assert_eq!(
        plan.argv,
        vec!["codex".to_string(), "app-server".to_string()],
        "minimal fx → bin 'codex', no --listen (W4 appends the port), no claude flags"
    );
    assert!(
        plan.env.is_empty(),
        "no CODEX_HOME in empty env → no env passthrough"
    );
    assert!(
        !plan.argv.iter().any(|a| a.contains("dangerously")),
        "codex plan must not carry claude flags"
    );

    // QD_CODEX_BIN override + CODEX_HOME passthrough.
    let mut env2 = MapEnv::default();
    env2.vars
        .insert("QD_CODEX_BIN".to_string(), "/opt/codex".to_string());
    env2.vars
        .insert("CODEX_HOME".to_string(), "/jail/codex-home".to_string());
    let fx2 = ProviderFx {
        env: &env2,
        paths: &paths,
        socket_dir: home.join("zmx-501"),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    let plan2 = CodexProvider.launch_plan(&fx2, &LaunchRequest::default());
    assert_eq!(
        plan2.argv,
        vec!["/opt/codex".to_string(), "app-server".to_string()]
    );
    assert_eq!(
        plan2.env,
        vec![("CODEX_HOME".to_string(), "/jail/codex-home".to_string())],
        "CODEX_HOME passthrough when fx.env carries it"
    );
}

/// codex inject with NO app_server in fx → InjectError::NoTransport(key id)
/// (codex-p2-spec section 6.4). The codex equivalent of claude's missing-relay.
///
/// MUTATION EVIDENCE: dropping the app_server-required guard would either panic
/// (unwrap) or silently succeed — this asserts the typed NoTransport carrying the
/// key id.
#[test]
fn codex_inject_no_transport_when_app_server_absent() {
    let tmp = TempDir::new().unwrap();
    let paths = QdPaths::from_home(&tmp.path().join("home"));
    let env = MapEnv::default();
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: tmp.path().to_path_buf(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None, // no connected rpc.
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,    };
    let key = SessionKey {
        id: "thread-abc",
        name: None,
        cwd: None,
        pid: None,
    };
    let err = CodexProvider.inject(&fx, &key, "m", "cli").unwrap_err();
    match err {
        InjectError::NoTransport(id) => assert_eq!(id, "thread-abc"),
        other => panic!("expected NoTransport(thread-abc), got {other:?}"),
    }
}

/// `provider_for("codex")` resolves to the codex provider (codex-p2-spec section
/// 6.1 / deliverable f). The unknown-provider CLI error string (which W4 updates
/// to list codex) lives in the bin verb layer
/// (src/bin/dispatch/verbs/lifecycle.rs:138 + :155), NOT in provider_for.
#[test]
fn provider_for_resolves_codex() {
    assert_eq!(
        provider_for("codex").map(|p| p.id()),
        Some("codex"),
        "codex resolves from the registry (GATE-R ruled)"
    );
    // hosting() = Daemon (the FIRST production hosting branch is W4's, not here).
    assert_eq!(provider_for("codex").unwrap().hosting(), Hosting::Daemon);
}

/// codex resume_args = ["resume", id], fork ignored, no --fork-session shape
/// (codex-p2-spec section 6.4) — contrast with claude's --resume/--fork-session.
#[test]
fn codex_resume_args_shape() {
    let key = SessionKey {
        id: "019e-thread",
        name: None,
        cwd: None,
        pid: None,
    };
    assert_eq!(
        CodexProvider.resume_args(&key, false),
        vec!["resume", "019e-thread"]
    );
    assert_eq!(
        CodexProvider.resume_args(&key, true),
        vec!["resume", "019e-thread"],
        "codex ignores fork — daemon resume is thread/resume RPC, not argv"
    );
}

// ===========================================================================
// codex P2 W6 (codex-p2-spec section 7.5) — the SEND ladder on CodexProvider::
// inject: believed-IDLE → turn/start; believed-BUSY → turn/steer; stale fence →
// turn/start fallback. The user op is SEND; the start/steer envelopes are
// PROVIDER-INTERNAL (ADD-23(4)).
// ===========================================================================

use dispatch::provider::codex::rpc::ServerError;
use std::path::PathBuf as W6PathBuf;

/// A configurable SEND-ladder rpc: records the ORDERED method calls (with args)
/// and returns scripted outcomes for turn_start / turn_steer, so the ladder's
/// branch + the stale-fence fallback are pinned by call-order assertions.
struct LadderRpc {
    start_id: String,
    steer: std::cell::RefCell<Vec<SteerOutcome>>,
    calls: std::cell::RefCell<Vec<String>>,
}
impl LadderRpc {
    fn new(start_id: &str, steer: Vec<SteerOutcome>) -> Self {
        Self {
            start_id: start_id.to_string(),
            steer: std::cell::RefCell::new(steer),
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}
impl AppServerRpc for LadderRpc {
    fn initialize(&self, _c: &ClientInfo) -> Result<InitializeResult, RpcError> {
        Ok(InitializeResult::default())
    }
    fn initialized(&self) -> Result<(), RpcError> {
        Ok(())
    }
    fn thread_start(&self, _c: &str, _a: &str, _s: &str) -> Result<String, RpcError> {
        unreachable!("the SEND ladder never starts a thread")
    }
    fn thread_resume(&self, _id: &str) -> Result<(), RpcError> {
        unreachable!()
    }
    fn turn_start(&self, thread_id: &str, text: &str) -> Result<String, RpcError> {
        self.calls
            .borrow_mut()
            .push(format!("start({thread_id},{text})"));
        Ok(self.start_id.clone())
    }
    fn turn_steer(&self, t: &str, e: &str, x: &str) -> Result<SteerOutcome, RpcError> {
        self.calls.borrow_mut().push(format!("steer({t},{e},{x})"));
        Ok(self.steer.borrow_mut().remove(0))
    }
    fn turn_interrupt(&self, _t: &str, _u: &str) -> Result<(), RpcError> {
        unreachable!()
    }
    fn next_notification(&self, _t: std::time::Duration) -> Result<Option<Notification>, RpcError> {
        Ok(None)
    }
    fn close(&self) -> Result<(), RpcError> {
        Ok(())
    }
}

/// Build a ProviderFx carrying the connected ladder rpc + a believed open turn id.
fn ladder_fx<'a>(
    env: &'a MapEnv,
    paths: &'a QdPaths,
    rpc: &'a dyn AppServerRpc,
    expected: Option<&'a str>,
) -> ProviderFx<'a> {
    ProviderFx {
        env,
        paths,
        socket_dir: W6PathBuf::new(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: Some(rpc),
        codex_expected_turn_id: expected,
        acp_client: None,
        pi_rpc: None,    }
}

#[test]
fn send_ladder_idle_starts_a_turn_returns_id() {
    // believed IDLE (codex_expected_turn_id = None) → turn/start, returns the
    // start id. NO steer call.
    let tmp = TempDir::new().unwrap();
    let env = MapEnv::default();
    let paths = QdPaths::from_home(&tmp.path().join("home"));
    let rpc = LadderRpc::new("TURN-START-1", vec![]);
    let fx = ladder_fx(&env, &paths, &rpc, None);
    let key = SessionKey {
        id: "thread-A",
        name: None,
        cwd: None,
        pid: None,
    };
    let id = CodexProvider.inject(&fx, &key, "hello", "cli").unwrap();
    assert_eq!(id, "TURN-START-1");
    assert_eq!(rpc.calls(), vec!["start(thread-A,hello)"]);
}

#[test]
fn send_ladder_busy_steers_the_open_turn_returns_id() {
    // believed BUSY (codex_expected_turn_id = Some(T)) → turn/steer{expectedTurnId:T}
    // landed → returns the steered turn id. NO start call.
    let tmp = TempDir::new().unwrap();
    let env = MapEnv::default();
    let paths = QdPaths::from_home(&tmp.path().join("home"));
    let rpc = LadderRpc::new(
        "UNUSED-START",
        vec![SteerOutcome::Steered("TURN-STEERED-9".to_string())],
    );
    let fx = ladder_fx(&env, &paths, &rpc, Some("OPEN-TURN-7"));
    let key = SessionKey {
        id: "thread-B",
        name: None,
        cwd: None,
        pid: None,
    };
    let id = CodexProvider.inject(&fx, &key, "steer me", "cli").unwrap();
    assert_eq!(id, "TURN-STEERED-9");
    assert_eq!(
        rpc.calls(),
        vec!["steer(thread-B,OPEN-TURN-7,steer me)"],
        "believed-busy steers the open turn; no start call"
    );
}

#[test]
fn send_ladder_busy_stale_fence_falls_back_to_start_returns_id() {
    // believed BUSY → turn/steer → STALE FENCE (the open turn moved on) → FALL
    // BACK to turn/start → returns the start id. Call order proves the fallback:
    // steer THEN start.
    //
    // MUTATION EVIDENCE (codex-p2-spec section 13 "steer-stale fallback removed"):
    // if the StaleTurn arm bubbled the error instead of falling back to turn/start,
    // this inject would Err (no id) and the call list would have NO "start(...)" —
    // this test reds. The stale-fence-to-start fallback is the race-safe core of
    // the ladder (the prompt-swallow class is closed at the protocol level).
    let tmp = TempDir::new().unwrap();
    let env = MapEnv::default();
    let paths = QdPaths::from_home(&tmp.path().join("home"));
    let stale = ServerError {
        code: -32600,
        message: "expected active turn id `X` but found `Y`".to_string(),
    };
    let rpc = LadderRpc::new("TURN-FALLBACK-START", vec![SteerOutcome::StaleTurn(stale)]);
    let fx = ladder_fx(&env, &paths, &rpc, Some("STALE-TURN-3"));
    let key = SessionKey {
        id: "thread-C",
        name: None,
        cwd: None,
        pid: None,
    };
    let id = CodexProvider.inject(&fx, &key, "race me", "cli").unwrap();
    assert_eq!(
        id, "TURN-FALLBACK-START",
        "the stale fence falls back to a fresh turn/start, returning its id"
    );
    assert_eq!(
        rpc.calls(),
        vec![
            "steer(thread-C,STALE-TURN-3,race me)".to_string(),
            "start(thread-C,race me)".to_string(),
        ],
        "steer first (the believed-busy attempt), then the start fallback after the fence"
    );
}

/// codex P2 W6 (codex-p2-spec sections 7.5, 11): SEND-vocabulary ONLY. No user-
/// facing string the codex send/wait verbs produce may contain the PROVIDER-
/// INTERNAL protocol vocabulary (`turn/start`, `turn/steer`, `expectedTurnId`).
/// This greps the verb source for the user-facing string literals it emits and
/// asserts none carries a banned token. (W2 enforces sanitation in the rpc layer;
/// this pins it at the verb layer too — the ADD-23(4) / §11 banned-token rule.)
///
/// MUTATION EVIDENCE: a verb message that leaked `turn/steer` into an error string
/// (e.g. "steer failed: …") reds this.
#[test]
fn send_wait_verbs_use_send_vocabulary_only() {
    // The verb sources are the single place a codex user-facing string is minted.
    let send_src = include_str!("../src/bin/qd/verbs/send_relay.rs");
    let wait_src = include_str!("../src/bin/qd/verbs/wait.rs");
    for (name, src) in [("send_relay.rs", send_src), ("wait.rs", wait_src)] {
        for line in src.lines() {
            // Only inspect lines that PRINT to a user (eprintln!/println!), not the
            // protocol-layer doc comments that legitimately name the methods.
            let printed = line.contains("eprintln!") || line.contains("println!");
            if !printed {
                continue;
            }
            for banned in ["turn/start", "turn/steer", "expectedTurnId"] {
                assert!(
                    !line.contains(banned),
                    "{name}: a user-facing string leaks the banned token `{banned}`: {line}"
                );
            }
        }
    }
}
