//! The stage-2 phase-1 CROSS-CHECK for `LaneOps::health`
//! (`doc/tbd/provider-architecture/06-stage2-plan.md`).
//!
//! `qd ls` derives a live row's status in TWO places: the join picks a source per
//! provider (registry string / codex rollout tail / pi resident), and then
//! `verbs/ls.rs` applies two RENDER-path corrections — the liveness gate, and the
//! `acp_human_status` override that is the only production ACP status source
//! there is. `quorum_qw::lane_read::health_for` folds all of that behind ONE
//! per-lane call. This file computes both ways over one jailed fixture and
//! asserts they agree — **except** where the agreement would be the bug.
//!
//! ## The one intended disagreement: BUG 1
//!
//! pi/mux-pane. `gather_pi` gates every row on `endpoint.is_some()` and a pi TUI
//! row has no endpoint by construction, so today's join consults NOTHING and
//! falls through to `Idle` — a pi TUI session can never display as busy.
//! `health_for` reaches the endpoint-free `pi::session::derive_status` instead.
//! [`pi_pane_health_disagrees_with_ls_and_that_disagreement_is_bug_1`] asserts
//! the disagreement rather than papering over it: `qd ls` keeps shipping `idle`
//! (this phase changes no output), and the lane answers `busy` with
//! `HealthSource::TranscriptTail`.
//!
//! ## Why every fixture row omits `startedAt`
//!
//! The liveness gate needs BOTH a pid and a recorded start to form a
//! reuse-guarded identity, and fails OPEN without one (`liveness.rs`: "never hide
//! a row whose daemon we cannot probe"). The process table is the one thing a
//! tempdir cannot jail, so omitting `startedAt` is what makes the OTHER arms
//! deterministic. [`the_liveness_gate_is_folded_into_health`] then supplies a
//! `startedAt` deliberately, to prove the gate really is in there.

mod common;

use std::path::Path;

use dispatch::effects::{FixedClock, FixtureProcessTable, FixtureRelayProbe, MapEnv};
use dispatch::join::{self, JoinOpts};
use dispatch::model::{Session, SessionStatus};
use quorum_qw::lane::{Harness, Lane, Mode};
use quorum_qw::{HealthSource, SessionId, SessionStatus as LaneStatus};

use common::{assert_not_real_home, set_mtime_ms};

const NOW_MS: i64 = 1_717_500_300_000;
const MTIME_MS: i64 = 1_717_490_000_000;

const SID_CLAUDE: &str = "health-claude-0001";
const SID_CLAUDE_GATED: &str = "health-claude-gated-0002";
const SID_PI: &str = "health-pi-0003";
const SID_CODEX: &str = "019ea0b3-04d3-7400-8d95-f55d41e961e5";
const SID_ACP: &str = "health-acp-0005";
const SID_PI_EXT: &str = "health-pi-ext-0006";

/// A pid high enough that no process can hold it, so the gate's classifier reads
/// GONE deterministically. macOS caps `pid_max` at 99999.
const DEAD_PID: i64 = 999_999;

struct HealthRun {
    _tmp: tempfile::TempDir,
    sessions: Vec<Session>,
    paths: dispatch::paths::QdPaths,
    env: MapEnv,
}

impl HealthRun {
    fn row(&self, sid: &str) -> &Session {
        self.sessions
            .iter()
            .find(|s| s.session_id == sid)
            .unwrap_or_else(|| panic!("fixture row {sid} missing from the join"))
    }

    /// What `qd ls` RENDERS for a row: the joined status put through the same two
    /// render-path corrections `verbs/ls.rs` applies, in the same order.
    ///
    /// Spelled here rather than reached for, because those corrections live in
    /// the qd BINARY and an integration test links only the library. Keeping them
    /// as a literal transcription is also what makes the cross-check meaningful —
    /// if `ls.rs`'s gating drifts from this, the two computations stop describing
    /// the same thing and that is itself the finding.
    fn ls_renders(&self, sid: &str) -> SessionStatus {
        let s = self.row(sid);
        if s.provider.starts_with("acp/") {
            // `acp_human_status`: pid alive + `--listen` identity, never the
            // stored status. Every fixture ACP pid is dead ⇒ "stopped"/Killed.
            return SessionStatus::Killed;
        }
        if s.provider == "codex" {
            // Exempt from the pid gate: daemon-hosted, rollout-tail liveness.
            return s.status;
        }
        let src = dispatch::liveness::OsLiveness::new();
        let daemon_src = dispatch::liveness::SocketDaemonLiveness::new(None);
        dispatch::liveness::gated_ls_status_headless(
            s.status,
            s.entrypoint.as_deref(),
            s.name.as_deref(),
            s.pid,
            s.started_at_ms,
            &daemon_src,
            &src,
        )
    }

    fn health(&self, lane: Lane, sid: &str) -> quorum_qw::Health {
        quorum_qw::lane_read::health_for(lane, &self.paths, &self.env, &SessionId(sid.to_string()))
            .unwrap_or_else(|e| panic!("health({lane}, {sid}) failed: {e}"))
    }
}

fn lane(h: Harness, m: Mode) -> Lane {
    Lane::new(h, m).expect("fixture lanes are all valid")
}

/// `cwd` → claude project-dir slug, the encoding `jsonl::cwd_to_project_path` uses.
fn slug(cwd: &str) -> String {
    cwd.replace(['/', '.'], "-")
}

fn write_claude_transcript(projects: &Path, cwd: &str, sid: &str) {
    let dir = projects.join(slug(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(
        &path,
        format!("{{\"type\":\"user\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"hi\"}}}}\n"),
    )
    .unwrap();
    set_mtime_ms(&path, MTIME_MS);
}

fn run() -> HealthRun {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let codex_home = tmp.path().join("codex-home");
    let pi_root = tmp.path().join("pi-sessions");
    let zmx_dir = tmp.path().join("zmx-501");
    let sessions_dir = home.join(".claude").join("sessions");
    let projects = home.join(".claude").join("projects");
    for d in [&sessions_dir, &projects, &codex_home, &pi_root, &zmx_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    assert_not_real_home(&home);

    // --- (1) a live CLAUDE row. `startedAt` omitted ⇒ the gate fails open, so
    //     this arm reads the registry status string and nothing else. ---
    write_row(
        &sessions_dir,
        5101,
        &format!(
            r#"{{"pid":5101,"sessionId":"{SID_CLAUDE}","cwd":"/work/claude","updatedAt":{u},"status":"busy","name":"claude-live"}}"#,
            u = NOW_MS - 1000,
        ),
    );
    write_claude_transcript(&projects, "/work/claude", SID_CLAUDE);

    // --- (2) the SAME shape WITH a `startedAt` and an unholdable pid, so the
    //     liveness gate has a reuse-guarded identity to refuse. ---
    write_row(
        &sessions_dir,
        DEAD_PID,
        &format!(
            r#"{{"pid":{DEAD_PID},"sessionId":"{SID_CLAUDE_GATED}","cwd":"/work/gated","startedAt":{s},"updatedAt":{u},"status":"busy","name":"claude-gated"}}"#,
            s = NOW_MS - 60_000,
            u = NOW_MS - 1000,
        ),
    );

    // --- (3) a live PI row in a MUX PANE — no endpoint, by construction. Its
    //     transcript's last message is a `user` entry, which `derive_status`
    //     reads as Busy (the model has not answered yet). BUG 1's fixture. ---
    write_row(
        &sessions_dir,
        5103,
        &format!(
            r#"{{"pid":5103,"sessionId":"{SID_PI}","cwd":"/work/pi","updatedAt":{u},"status":"idle","name":"pi-live","provider":"pi","hosting":"mux-pane"}}"#,
            u = NOW_MS - 1000,
        ),
    );
    let pi_file = pi_root.join(format!("2026-06-04T08-20-00-000Z_{SID_PI}.jsonl"));
    std::fs::write(
        &pi_file,
        concat!(
            r#"{"type":"agent-name","agentName":"pi-live"}"#,
            "\n",
            r#"{"type":"message","message":{"role":"user","content":[{"type":"text"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    set_mtime_ms(&pi_file, MTIME_MS);

    // --- (6) a live PI/EXTENSION row whose control channel is NOT answering.
    //     The endpoint names a socket that does not exist, which is exactly the
    //     shape of a session whose pi died without unlinking, or one launched
    //     before the extension was installed.
    //
    //     The assertion this fixture exists for is the FALLBACK: the lane must
    //     report `TranscriptTail`, not `LiveRpc`. Naming `LiveRpc` for an
    //     answer no RPC gave is BUG 1's shape one lane over — a source
    //     misattributed is worse than a slow one, because a caller uses the
    //     source to decide how much to trust the status. ---
    write_row(
        &sessions_dir,
        5106,
        &format!(
            r#"{{"pid":5106,"sessionId":"{SID_PI_EXT}","cwd":"/work/pi","updatedAt":{u},"status":"idle","name":"pi-ext","provider":"pi","hosting":"extension","endpoint":"unix:///nonexistent/quorum-pi/{SID_PI_EXT}.sock"}}"#,
            u = NOW_MS - 1000,
        ),
    );
    let pi_ext_file = pi_root.join(format!("2026-06-04T08-25-00-000Z_{SID_PI_EXT}.jsonl"));
    std::fs::write(
        &pi_ext_file,
        concat!(
            r#"{"type":"agent-name","agentName":"pi-ext"}"#,
            "\n",
            r#"{"type":"message","message":{"role":"user","content":[{"type":"text"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    set_mtime_ms(&pi_ext_file, MTIME_MS);

    // --- (4) a live CODEX row whose rollout has an OPEN turn (task_started with
    //     no matching task_complete) ⇒ the connectionless derivation says Busy. ---
    write_row(
        &sessions_dir,
        5104,
        &format!(
            r#"{{"pid":5104,"sessionId":"{SID_CODEX}","cwd":"/work/codex","updatedAt":{u},"status":"idle","name":"codex-live","provider":"codex","hosting":"daemon"}}"#,
            u = NOW_MS - 1000,
        ),
    );
    let day = codex_home.join("sessions").join("2026/06/04");
    std::fs::create_dir_all(&day).unwrap();
    let rollout = day.join(format!("rollout-2026-06-04T08-33-20-{SID_CODEX}.jsonl"));
    std::fs::write(
        &rollout,
        format!(
            "{meta}\n{started}\n",
            meta = format!(
                r#"{{"timestamp":"2026-06-04T08:33:20.000Z","type":"session_meta","payload":{{"id":"{SID_CODEX}","cwd":"/work/codex","originator":"qd"}}}}"#
            ),
            started = r#"{"timestamp":"2026-06-04T08:33:21.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t-open"}}"#,
        ),
    )
    .unwrap();
    set_mtime_ms(&rollout, MTIME_MS);
    std::fs::write(codex_home.join("state_5.sqlite"), b"not a sqlite db \x00\xff").unwrap();

    // --- (5) a live ACP row with a dead adapter pid. The stored status says
    //     `busy`; the truth is that nothing is running. That gap IS the reason
    //     `acp_human_status` never reads the stored field. ---
    write_row(
        &sessions_dir,
        5105,
        &format!(
            r#"{{"pid":{DEAD_PID},"sessionId":"{SID_ACP}","cwd":"/work/acp","updatedAt":{u},"status":"busy","name":"acp-live","provider":"acp/claude-code","endpoint":"ws://127.0.0.1:59999"}}"#,
            u = NOW_MS - 2000,
        ),
    );

    let env = MapEnv {
        vars: [
            ("ZMX_DIR", zmx_dir.to_string_lossy().into_owned()),
            ("CODEX_HOME", codex_home.to_string_lossy().into_owned()),
            (
                "PI_CODING_AGENT_SESSION_DIR",
                pi_root.to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        uid: 501,
    };

    let mux = dispatch::mux::FixtureMux::new().with_dir(zmx_dir.clone(), "");
    let pt = FixtureProcessTable::default();
    let probe = FixtureRelayProbe(Vec::new());
    let clock = FixedClock(NOW_MS);
    let paths = dispatch::paths::QdPaths::from_home(&home);
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: false,
        include_preview: false,
        limit: None,
    };
    let inputs = join::gather(
        &paths, &mux, &env, &pt, &probe, &clock, tmp.path(), None, opts,
    );
    let sessions = join::join_sessions(&inputs, opts);

    HealthRun {
        _tmp: tmp,
        sessions,
        paths,
        env,
    }
}

fn write_row(sessions_dir: &Path, pid: i64, json: &str) {
    std::fs::write(sessions_dir.join(format!("{pid}.json")), json).unwrap();
}

/// Two vocabularies, one meaning. Spelled out so a drift in either enum is a
/// compile error here rather than a silent mistranslation.
fn as_lane(s: SessionStatus) -> LaneStatus {
    match s {
        SessionStatus::Idle => LaneStatus::Idle,
        SessionStatus::Busy => LaneStatus::Busy,
        SessionStatus::Shell => LaneStatus::Shell,
        SessionStatus::Cold => LaneStatus::Cold,
        SessionStatus::Killed => LaneStatus::Killed,
    }
}

// ===========================================================================

/// THE CROSS-CHECK. For every lane except the one BUG 1 lives in, the lane's
/// health must equal what `qd ls` renders — the whole point of phase 1 being that
/// the two computations are run and compared, not merely inspected.
#[test]
fn lane_health_agrees_with_what_qd_ls_renders() {
    let r = run();
    let cases: Vec<(Lane, &str)> = vec![
        (lane(Harness::ClaudeCode, Mode::Pane), SID_CLAUDE),
        (lane(Harness::ClaudeCode, Mode::Pane), SID_CLAUDE_GATED),
        (lane(Harness::Codex, Mode::Daemon), SID_CODEX),
        (lane(Harness::AcpClaudeCode, Mode::Daemon), SID_ACP),
    ];
    for (l, sid) in cases {
        assert_eq!(
            r.health(l, sid).status,
            as_lane(r.ls_renders(sid)),
            "CROSS-CHECK: lane {l} and `qd ls` disagree about {sid}. A disagreement \
             here is a FINDING — the only sanctioned one is BUG 1 (pi/mux-pane)."
        );
    }
}

/// Every lane names a REAL source. `HealthSource` exists so a caller can tell a
/// live observation from a stale projection, and so this suite can assert a lane
/// is not silently defaulting — which is precisely what BUG 1 was.
#[test]
fn every_lane_names_the_source_it_actually_read() {
    let r = run();
    let expect: Vec<(Lane, &str, HealthSource)> = vec![
        // The registry status string, unmodified — the gate fails open with no
        // `startedAt`, so the answer really is the registry's.
        (
            lane(Harness::ClaudeCode, Mode::Pane),
            SID_CLAUDE,
            HealthSource::RegistryStatus,
        ),
        // The gate overruled the string. Reporting `RegistryStatus` here would be
        // the lane misattributing an answer the registry did not give.
        (
            lane(Harness::ClaudeCode, Mode::Pane),
            SID_CLAUDE_GATED,
            HealthSource::ProcessLiveness,
        ),
        // Connectionless: the rollout tail, no socket.
        (
            lane(Harness::Codex, Mode::Daemon),
            SID_CODEX,
            HealthSource::TranscriptTail,
        ),
        // BUG 1's closure: a source where there was none.
        (
            lane(Harness::Pi, Mode::Pane),
            SID_PI,
            HealthSource::TranscriptTail,
        ),
        // Adapter pid + `--listen` identity. Not a status string, not an RPC.
        (
            lane(Harness::AcpClaudeCode, Mode::Daemon),
            SID_ACP,
            HealthSource::ProcessLiveness,
        ),
        // pi/extension with a DEAD channel. `LiveRpc` is this lane's source when
        // the socket answers; when it does not, the honest answer is the one it
        // actually read. A lane that reported `LiveRpc` here would be naming an
        // RPC that never happened.
        (
            lane(Harness::Pi, Mode::Extension),
            SID_PI_EXT,
            HealthSource::TranscriptTail,
        ),
    ];
    let actual: Vec<(Lane, &str, HealthSource)> = expect
        .iter()
        .map(|(l, sid, _)| (*l, *sid, r.health(*l, sid).source))
        .collect();
    assert_eq!(
        actual, expect,
        "a lane changed which source it reads. `Idle` with no real source IS the \
         bug — see doc/tbd/provider-architecture/07-lane-gaps.md §B/§D."
    );
}

/// **BUG 1, asserted as a disagreement rather than papered over.**
///
/// `qd ls` keeps shipping `idle` for a pi TUI row whatever it is doing, because
/// the resident point-read that would know gates on an endpoint the row cannot
/// have. The lane reads the transcript tail instead and says `busy`. This phase
/// changes no output — it establishes that the lane's answer is the better one,
/// so the flip can be made deliberately.
#[test]
fn pi_pane_health_disagrees_with_ls_and_that_disagreement_is_bug_1() {
    let r = run();
    assert_eq!(
        r.ls_renders(SID_PI),
        SessionStatus::Idle,
        "the shipped value must still be the old one — phase 1 changes no output"
    );
    let h = r.health(lane(Harness::Pi, Mode::Pane), SID_PI);
    assert_eq!(
        h.status,
        LaneStatus::Busy,
        "BUG 1: a pi TUI row whose transcript ends on a `user` message is BUSY. \
         If this reads Idle the endpoint-free derivation is not wired in."
    );
    assert_eq!(h.source, HealthSource::TranscriptTail);
}

/// The gate really is folded in — same input, same row shape, only `startedAt`
/// differs, and the answer flips from the registry's `busy` to `cold`.
#[test]
fn the_liveness_gate_is_folded_into_health() {
    let r = run();
    let l = lane(Harness::ClaudeCode, Mode::Pane);
    assert_eq!(r.health(l, SID_CLAUDE).status, LaneStatus::Busy);
    assert_eq!(
        r.health(l, SID_CLAUDE_GATED).status,
        LaneStatus::Cold,
        "a row whose reuse-guarded pid identity is gone must not render live"
    );
}

/// An id with no LIVE registry row is `NotFound`, never a silent `Idle`. A cold
/// row's `Cold` is structural — `list` carries it — so `health` does not re-scan
/// a store to re-derive a constant.
#[test]
fn health_on_an_unknown_id_is_not_found() {
    let r = run();
    let l = lane(Harness::ClaudeCode, Mode::Pane);
    let err = quorum_qw::lane_read::health_for(
        l,
        &r.paths,
        &r.env,
        &SessionId("no-such-session".to_string()),
    );
    assert!(
        matches!(err, Err(quorum_qw::LaneError::NotFound { .. })),
        "got {err:?}"
    );
}

/// The fixture's own precondition: nothing above proves anything if the join did
/// not actually produce the five rows.
#[test]
fn the_fixture_joins_six_live_rows() {
    let r = run();
    let mut ids: Vec<&str> = r.sessions.iter().map(|s| s.session_id.as_str()).collect();
    ids.sort_unstable();
    let mut want = vec![
        SID_CLAUDE,
        SID_CLAUDE_GATED,
        SID_PI,
        SID_CODEX,
        SID_ACP,
        SID_PI_EXT,
    ];
    want.sort_unstable();
    assert_eq!(ids, want);
}

/// The concrete answers, pinned.
///
/// [`lane_health_agrees_with_what_qd_ls_renders`] compares two computations, and
/// two computations that BOTH default to `Idle` would agree perfectly while
/// telling the user nothing. This is the discrimination check: each row's status
/// is asserted against the fixture that produced it, so a lane that stopped
/// reading its source reds here even though the cross-check stays green.
#[test]
fn the_cross_check_discriminates_each_source_answered() {
    let r = run();
    let cases: Vec<(Lane, &str, LaneStatus, &str)> = vec![
        (
            lane(Harness::ClaudeCode, Mode::Pane),
            SID_CLAUDE,
            LaneStatus::Busy,
            "the registry row's status string says busy",
        ),
        (
            lane(Harness::Codex, Mode::Daemon),
            SID_CODEX,
            LaneStatus::Busy,
            "the rollout has a task_started with no matching task_complete",
        ),
        (
            lane(Harness::Pi, Mode::Pane),
            SID_PI,
            LaneStatus::Busy,
            "the pi transcript's last message is a `user` entry",
        ),
        (
            lane(Harness::AcpClaudeCode, Mode::Daemon),
            SID_ACP,
            LaneStatus::Killed,
            "the adapter pid is gone, whatever the stored status claims",
        ),
    ];
    for (l, sid, want, why) in cases {
        assert_eq!(
            r.health(l, sid).status,
            want,
            "{l} / {sid}: {why}. An Idle here means the source was never read."
        );
    }
}
