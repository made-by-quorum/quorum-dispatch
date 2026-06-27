//! codex P2 W5 (codex-p2-spec sections 7.4, 9, 13) — ls/info join for codex rows
//! + the TWO NAMED rule-8 goldens (`golden/ls-codex.json`, `golden/info-codex.json`).
//!
//! Pipeline under test (the SAME shared path ls/info drive, common.rs):
//! `join::gather` (I/O against a jailed HOME + CODEX_HOME) → `join::join_with_strays`
//! → `join::assign_codes` → `render::ls_json` / `render::info_text`. The codex
//! gather step (`gather_codex`) derives live-row status CONNECTIONLESS from the
//! rollout tail (NO socket) and discovers cold codex rows under the codex root.
//!
//! ## Determinism (mirrors the parity.rs harness discipline)
//!
//! Every absolute path (the jailed HOME prefix; the CODEX_HOME prefix) is replaced
//! with a stable placeholder by [`CodexRun::normalize`] before the golden compare,
//! exactly like parity.rs's `<HOME>`/`<ZMX>` normalization. Rollout file mtimes are
//! frozen via the common `set_mtime_ms` helper so the cold-row sort + lastActive
//! are byte-stable across checkouts. The clock is fixed.
//!
//! ## The fixture shape (proving coexistence + the contract surface)
//!
//! Three rows, built programmatically (no committed fixture tree — only the two
//! golden files are added, per the R-d STOP rule):
//!   - a CLAUDE live row (`provider` absent on disk = claude-code) — proves codex
//!     rows COEXIST with claude rows in the same `ls --json`.
//!   - a CODEX live row (`provider: "codex"`, `endpoint: ws://…`) whose rollout
//!     tail has an OPEN turn → the derived status is `busy` (connectionless).
//!   - a CODEX COLD row: a foreign rollout under the codex tree with NO registry
//!     row → discovered by the rollout scan, emitted as `cold` / provider codex.
//!
//! A GARBAGE `state_5.sqlite` sits under the codex home: `index::threads` degrades
//! to empty, the cold row STILL surfaces from the rollout scan (codex-p2-spec
//! section 13 "cold-row sqlite degrade" mutation evidence).
//!
//! ## `info-codex.json` (named-golden / no info `--json` precedent)
//!
//! This engine has NO `info --json` mode — `qd info` renders human text only
//! (`render::info_text`; the existing info golden is `info-alpha.txt`). The rule-8
//! filing (codex-p2-spec section 9.1) BINDS the new file name to `info-codex.json`,
//! so we pin the codex row's human info text INSIDE a JSON object
//! (`{"info": "<info_text>"}`) — a deterministic, byte-diffable `.json` that
//! honors the bound name AND captures the human info surface. The assertions below
//! ALSO pin the load-bearing facts as exact strings: `Provider: codex`, the
//! transcript/jsonl line = the rollout path, and that the ws ENDPOINT / port
//! NEVER appear on the human info surface (codex-p2-spec sections 7.4, 9.4).

mod common;

use std::path::{Path, PathBuf};

use dispatch::effects::{FixedClock, MapEnv, ProcInfo};
use dispatch::join::{self, JoinOpts};
use dispatch::render;

use common::{assert_not_real_home, set_mtime_ms};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Read a golden file, or — when `SB_REGEN_GOLDEN=1` — write `actual` and return
/// (so the first run freezes it). Byte-equality assert. Mirrors parity.rs exactly.
fn assert_golden(name: &str, actual: &str) {
    let path = golden_dir().join(name);
    if std::env::var("SB_REGEN_GOLDEN").is_ok() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, actual).unwrap();
        eprintln!("regenerated golden {name}");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {path:?}: {e} (run SB_REGEN_GOLDEN=1)"));
    assert_eq!(actual, expected, "golden mismatch for {name}");
}

/// Frozen, deterministic values used across the fixture (epoch ms).
const CLAUDE_UPDATED_MS: i64 = 1_717_495_300_000;
const CODEX_LIVE_UPDATED_MS: i64 = 1_717_495_200_000;
const CODEX_COLD_MTIME_MS: i64 = 1_717_490_000_000;
const NOW_MS: i64 = 1_717_500_300_000;

/// A hermetic codex+claude run. Holds the tempdir for the test's lifetime.
struct CodexRun {
    _tmp: tempfile::TempDir,
    inputs: join::JoinInputs,
    home_dir: PathBuf,
    codex_home: PathBuf,
}

impl CodexRun {
    /// Replace the volatile absolute prefixes (`<HOME>`, `<CODEX_HOME>`) so the
    /// golden is path-stable across runs (NORMALIZATION-class, parity.rs lineage).
    /// CODEX_HOME first (it is NOT nested under HOME here, but order is harmless).
    fn normalize(&self, text: &str) -> String {
        text.replace(
            &self.codex_home.to_string_lossy().into_owned(),
            "<CODEX_HOME>",
        )
        .replace(&self.home_dir.to_string_lossy().into_owned(), "<HOME>")
    }
}

/// Write a rollout file at `<codex_home>/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`
/// with the given lines, then freeze its mtime. Returns the absolute path.
fn write_rollout(
    codex_home: &Path,
    date: &str,
    ts: &str,
    uuid: &str,
    lines: &[&str],
    mtime: i64,
) -> PathBuf {
    let day_dir = codex_home.join("sessions").join(date);
    std::fs::create_dir_all(&day_dir).unwrap();
    let path = day_dir.join(format!("rollout-{ts}-{uuid}.jsonl"));
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    set_mtime_ms(&path, mtime);
    path
}

/// Build the jailed fixture + run gather (the SAME `join::gather` ls/info drive).
fn run_codex(opts: JoinOpts) -> CodexRun {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let codex_home = tmp.path().join("codex-home");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();
    assert_not_real_home(&home);

    // --- registry rows: one claude (provider absent), one codex (provider+endpoint). ---
    let claude_row = format!(
        r#"{{"pid":1001,"sessionId":"claude-sid-0001","cwd":"/work/projA","startedAt":{start},"updatedAt":{up},"status":"idle","name":"claude-worker","version":"1.0.0"}}"#,
        start = CLAUDE_UPDATED_MS - 1000,
        up = CLAUDE_UPDATED_MS,
    );
    std::fs::write(sessions.join("1001.json"), claude_row).unwrap();

    // The codex live thread uuid (a real-shape uuidv7).
    let codex_uuid = "019ea0b3-04d3-7400-8d95-f55d41e961e4";
    let codex_row = format!(
        r#"{{"pid":5050,"sessionId":"{uuid}","cwd":"/work/codexA","startedAt":{start},"updatedAt":{up},"status":"idle","name":"codex-worker","version":"0.134.0","provider":"codex","endpoint":"ws://127.0.0.1:18951"}}"#,
        uuid = codex_uuid,
        start = CODEX_LIVE_UPDATED_MS - 1000,
        up = CODEX_LIVE_UPDATED_MS,
    );
    std::fs::write(sessions.join("5050.json"), codex_row).unwrap();

    // --- the codex LIVE thread rollout: an OPEN turn (task_started, NO
    //     task_complete) → derive_status = Busy (connectionless). cwd from
    //     session_meta. ---
    let live_meta = format!(
        r#"{{"timestamp":"2026-06-04T10:01:39.000Z","type":"session_meta","payload":{{"id":"{codex_uuid}","cwd":"/work/codexA","originator":"qd"}}}}"#
    );
    let live_started = r#"{"timestamp":"2026-06-04T10:01:40.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"019ea0b3-5157-7913-8a49-3308f6be7cb0"}}"#;
    let live_user = r#"{"timestamp":"2026-06-04T10:01:41.000Z","type":"event_msg","payload":{"type":"user_message","message":"do a thing"}}"#;
    // A token_count event carries per-turn `last_token_usage` (current context fill,
    // Pete #5) alongside the monotonic `total_token_usage` lifetime cumulative — we
    // take the former. 15800 here → rendered "15.8k", distinct from the 1537063
    // lifetime total to prove we do NOT use total_token_usage.
    let live_tokens = r#"{"timestamp":"2026-06-04T10:01:42.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1500000,"cached_input_tokens":1400000,"output_tokens":37063,"reasoning_output_tokens":0,"total_tokens":1537063},"last_token_usage":{"input_tokens":12000,"cached_input_tokens":3500,"output_tokens":300,"reasoning_output_tokens":0,"total_tokens":15800},"model_context_window":258400}}}"#;
    write_rollout(
        &codex_home,
        "2026/06/04",
        "2026-06-04T10-01-39",
        codex_uuid,
        &[&live_meta, live_started, live_user, live_tokens],
        CODEX_LIVE_UPDATED_MS,
    );

    // --- a COLD/foreign codex thread (NO registry row) — balanced turn → its own
    //     status is irrelevant (cold rows render `cold`); discovered by the rollout
    //     scan. cwd from session_meta. ---
    let cold_uuid = "019e9f3b-deea-7392-9861-b5d8ad376e2b";
    let cold_meta = format!(
        r#"{{"timestamp":"2026-06-04T08:33:20.000Z","type":"session_meta","payload":{{"id":"{cold_uuid}","cwd":"/work/codexCold","originator":"qd"}}}}"#
    );
    let cold_started = r#"{"timestamp":"2026-06-04T08:33:21.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t-cold"}}"#;
    let cold_complete = r#"{"timestamp":"2026-06-04T08:33:30.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t-cold","last_agent_message":"done"}}"#;
    write_rollout(
        &codex_home,
        "2026/06/04",
        "2026-06-04T08-33-20",
        cold_uuid,
        &[&cold_meta, cold_started, cold_complete],
        CODEX_COLD_MTIME_MS,
    );

    // --- a GARBAGE state_5.sqlite: index::threads degrades to empty; the cold row
    //     STILL surfaces from the rollout scan (codex-p2-spec section 13 cold-row
    //     sqlite degrade evidence). ---
    std::fs::write(
        codex_home.join("state_5.sqlite"),
        b"not a sqlite database at all \x00\xff",
    )
    .unwrap();

    // --- env: ZMX_DIR (empty canonical) + CODEX_HOME → the codex root. ---
    let canonical = tmp.path().join("zmx-501");
    std::fs::create_dir_all(&canonical).unwrap();
    let env = MapEnv {
        vars: [
            (
                "ZMX_DIR".to_string(),
                canonical.to_string_lossy().into_owned(),
            ),
            (
                "CODEX_HOME".to_string(),
                codex_home.to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        uid: 501,
    };

    // The claude live pid 1001 is alive + zmx-tracked (a normal claude row); the
    // codex pid 5050 needs no zmx (daemon-hosted). A minimal process table.
    let mux = dispatch::mux::FixtureMux::new().with_dir(canonical.clone(), "");
    let pt = dispatch::effects::FixtureProcessTable {
        ppids: [(1001, 1), (5050, 1)].into_iter().collect(),
        alive: [1001, 5050].into_iter().collect(),
        claude: vec![ProcInfo {
            pid: 1001,
            ppid: 1,
            cmd: "claude".into(),
            cwd: Some("/work/projA".into()),
            started_ms: None,
        }],
    };
    let probe = dispatch::effects::FixtureRelayProbe(Vec::new());
    let clock = FixedClock(NOW_MS);

    let paths = dispatch::paths::SbPaths::from_home(&home);
    let inputs = join::gather(
        &paths,
        &mux,
        &env,
        &pt,
        &probe,
        &clock,
        tmp.path(),
        None, // hermetic: no machine-global XDG-family scan.
        opts,
    );

    CodexRun {
        _tmp: tmp,
        inputs,
        home_dir: home,
        codex_home,
    }
}

fn all_opts() -> JoinOpts {
    JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    }
}

// --- ls --json golden: live codex + cold codex + claude coexistence ---

#[test]
fn ls_codex_golden() {
    let opts = all_opts();
    let run = run_codex(opts);
    let (mut sessions, strays) = join::join_with_strays(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    let text = run.normalize(&render::to_pretty(&render::ls_json(&sessions, &strays)));
    assert_golden("ls-codex.json", &text);
}

// --- info golden (human text wrapped in JSON, named per §9.1) ---

#[test]
fn info_codex_golden() {
    let opts = all_opts();
    let run = run_codex(opts);
    let (mut sessions, _strays) = join::join_with_strays(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    let codex = sessions
        .iter()
        .find(|s| s.session_id == "019ea0b3-04d3-7400-8d95-f55d41e961e4")
        .expect("the live codex row is present");
    let info = run.normalize(&render::info_text(codex, NOW_MS));
    // Wrap the human info text in a deterministic JSON object so the bound
    // `.json` name is honored (info has no `--json` mode in this engine).
    let value = serde_json::json!({ "info": info });
    let text = render::to_pretty(&value);
    assert_golden("info-codex.json", &text);
}

// --- the load-bearing facts, pinned as exact assertions (not just the golden) ---

#[test]
fn live_codex_row_is_busy_from_rollout_tail_no_socket() {
    // MUTATION EVIDENCE (codex-p2-spec section 13 "rollout busy/idle anchor
    // inverted" + status-path inversion): the live codex thread's rollout has an
    // OPEN task_started with NO task_complete → the connectionless derivation is
    // Busy. If the codex branch used parse_status (None for a registry string →
    // Idle) OR derive_status were inverted, this reds. NO socket is ever opened
    // (the test runs with no daemon listening at ws://127.0.0.1:18951).
    let opts = all_opts();
    let run = run_codex(opts);
    let mut sessions = join::join_sessions(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    let codex = sessions
        .iter()
        .find(|s| s.session_id == "019ea0b3-04d3-7400-8d95-f55d41e961e4")
        .expect("live codex row");
    assert_eq!(codex.status, dispatch::model::SessionStatus::Busy);
    assert_eq!(codex.provider, "codex");
    // jsonlPath = the resolved rollout path (under the codex root, NOT projects_dir).
    assert!(
        codex
            .jsonl_path
            .as_deref()
            .is_some_and(|p| p.contains("/sessions/2026/06/04/rollout-")),
        "jsonlPath = the rollout path: {:?}",
        codex.jsonl_path
    );
}

#[test]
fn cold_codex_row_surfaces_despite_garbage_sqlite() {
    // MUTATION EVIDENCE (codex-p2-spec section 13 "cold-row sqlite degrade"): the
    // state_5.sqlite is garbage → index::threads degrades to empty; the cold codex
    // thread STILL surfaces via the rollout scan. If the gather step errored on the
    // bad db (instead of degrading) OR the rollout-scan fallback were removed, the
    // cold row would vanish and this reds.
    let opts = all_opts();
    let run = run_codex(opts);
    let mut sessions = join::join_sessions(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    let cold = sessions
        .iter()
        .find(|s| s.session_id == "019e9f3b-deea-7392-9861-b5d8ad376e2b")
        .expect("the cold codex row surfaces from the rollout scan");
    assert_eq!(cold.status, dispatch::model::SessionStatus::Cold);
    assert_eq!(cold.provider, "codex");
    assert_eq!(cold.cwd.as_deref(), Some("/work/codexCold"));
    // No live registry row → no pid.
    assert_eq!(cold.pid, None);
}

#[test]
fn endpoint_never_appears_in_ls_json_or_human_info() {
    // codex-p2-spec section 9.4 + section 7.4: the recorded ws endpoint MUST NOT
    // appear in `ls --json` NOR on the human info surface. The endpoint is an
    // internal registry field (agents reach codex sessions through qd verbs).
    //
    // MUTATION EVIDENCE: if a future change leaked `endpoint` into the Session
    // model + render (the banned --json key) this assertion reds.
    let opts = all_opts();
    let run = run_codex(opts);
    let (mut sessions, strays) = join::join_with_strays(&run.inputs, opts);
    join::assign_codes(&mut sessions);

    let ls = render::to_pretty(&render::ls_json(&sessions, &strays));
    assert!(
        !ls.contains("endpoint") && !ls.contains("18951") && !ls.contains("ws://"),
        "ls --json must NOT carry endpoint / port / ws scheme: {ls}"
    );

    let codex = sessions
        .iter()
        .find(|s| s.session_id == "019ea0b3-04d3-7400-8d95-f55d41e961e4")
        .expect("live codex row");
    let info = render::info_text(codex, NOW_MS);
    assert!(
        !info.contains("endpoint") && !info.contains("18951") && !info.contains("ws://"),
        "human info must NOT carry endpoint / port / ws scheme: {info}"
    );
    // The human info DOES render Provider: codex.
    assert!(info.contains("Provider:    codex\n"), "info: {info}");
}

#[test]
fn claude_and_codex_rows_coexist_in_one_ls() {
    // The whole point: a claude row and codex rows render side by side in ONE
    // `ls --json`, each with its own provider value and shape.
    let opts = all_opts();
    let run = run_codex(opts);
    let mut sessions = join::join_sessions(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    let providers: Vec<&str> = sessions.iter().map(|s| s.provider.as_str()).collect();
    assert!(providers.contains(&"claude-code"), "{providers:?}");
    assert!(providers.contains(&"codex"), "{providers:?}");
    // The claude row keeps its claude-shaped derivation (idle string → Idle).
    let claude = sessions
        .iter()
        .find(|s| s.session_id == "claude-sid-0001")
        .expect("claude row");
    assert_eq!(claude.status, dispatch::model::SessionStatus::Idle);
    assert_eq!(claude.provider, "claude-code");
}
