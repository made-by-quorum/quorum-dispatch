//! The `qd ls` SESSION MERGE POLICY guard
//! (`doc/tbd/provider-architecture/08-session-merge-policy.md`).
//!
//! `qd ls` assembles ONE list from seven independent sources. Which source wins a
//! duplicated session id was, until this file existed, decided by a
//! `seen_session_ids` set plus the order the blocks in `join_sessions_counted`
//! happen to run in — first writer wins — asserted by nothing. Reordering two
//! visually-independent `for` loops changed what a user saw and reddened no test.
//!
//! This file pins the policy. It is deliberately TWO layers:
//!
//!   1. **Named precedence assertions.** Every duplicated id in the fixture is
//!      asserted by [`SessionBranch`] with a message naming the RULE it breaks, so
//!      a future reordering fails saying "rule 1 (live > tombstoned > cold)"
//!      rather than emitting a byte diff for a human to decode.
//!   2. **A golden `ls --json`** over the whole fixture. That catches everything
//!      the named assertions do not think to look at — field composition, sort
//!      order, code assignment.
//!
//! A golden alone would NOT be sufficient: it tells you something changed, not
//! which rule was violated. The named assertions are the load-bearing half.
//!
//! ## The ruling being pinned (doc §The ruling)
//!
//!   1. Precedence: **live > tombstoned > cold** — with a NAMED DEVIATION for
//!      claude/codex, which are cold > tombstoned today. See
//!      [`cold_wins_over_tombstone_for_claude_codex_deviation_from_ruling`].
//!   2. **Whole row, no field-level merge** — the LOSING row contributes nothing.
//!   3. A row with an **empty `session_id` never participates in id-keyed dedup**
//!      in the orphaned-pane (`ZmxOnly`) branch; it keys on pane name instead.
//!      (The live and tombstone branches DO key an id-less row under `""` — the
//!      hole is pinned by [`empty_id_live_row_shadows_an_empty_id_tombstone`].)
//!   4. Cold-source ordering among providers is a non-question except for the
//!      ACP-CC bridge, which writes claude-shaped JSONL — see the `A3` pair.
//!
//! ## The fixture (the doc's own "Suggested guard", built exactly)
//!
//! > one session visible to two sources at once, one orphaned pane, one
//! > tombstone, and one cold row per provider
//!
//! | id | sources claiming it | winner | rule |
//! |---|---|---|---|
//! | `live-vs-cold-0001` | live registry + claude JSONL | LiveRegistry | 1 |
//! | `live-vs-tomb-0002` | live registry + tombstone | LiveRegistry | 1 |
//! | `acp-vs-cold-0003` | acp/* tombstone + claude JSONL shadow | Tombstoned | 1, 4 |
//! | `claude-vs-cold-0004` | claude tombstone + claude JSONL | **ColdJsonl** | DEVIATION |
//! | `cold-claude-0005` | claude JSONL only | ColdJsonl | — |
//! | `<uuidv7>` | codex rollout only | ColdJsonl (codex) | — |
//! | `pi-cold-0007` | pi transcript only | ColdJsonl (pi) | — |
//! | `ses_merge0008…` | opencode.db only | ColdJsonl (opencode) | — |
//! | `""` ×2 | two orphaned mux panes | ZmxOnly ×2 | 3 |
//!
//! ## Jailing (L9a, the `codex_ls.rs` / `stats_cache_gather.rs` discipline)
//!
//! Everything is under ONE tempdir: the claude home, `$CODEX_HOME`,
//! `$PI_CODING_AGENT_SESSION_DIR`, `$XDG_DATA_HOME` (the OpenCode store root) and
//! `$ZMX_DIR`. `assert_not_real_home` panics if the constructed home ever equals
//! the real one. The clock is fixed, every transcript mtime is frozen, and the
//! machine-global XDG-family mux scan is disabled (`xdg: None`), so the fixture
//! is hermetic and the golden is byte-stable across checkouts.

mod common;

use std::path::{Path, PathBuf};

use dispatch::effects::{FixedClock, FixtureProcessTable, FixtureRelayProbe, MapEnv};
use dispatch::join::{self, JoinOpts};
use dispatch::model::{Session, SessionBranch, SessionStatus};
use dispatch::render;

use common::{assert_not_real_home, set_mtime_ms};

// --- frozen fixture clock values (epoch ms), all DISTINCT so the final
//     lastActive-descending sort is a total order and the golden is stable. ---
const L1_UPDATED_MS: i64 = 1_717_499_000_000; // live, shadowed by a cold transcript
const L2_UPDATED_MS: i64 = 1_717_498_000_000; // live, shadowed by a tombstone
const A3_UPDATED_MS: i64 = 1_717_497_000_000; // acp tombstone, shadowed by cold JSONL
const OPENCODE_UPDATED_MS: i64 = 1_717_495_000_000;
const L1_TRANSCRIPT_MTIME_MS: i64 = 1_717_494_000_000; // loser
const A3_TRANSCRIPT_MTIME_MS: i64 = 1_717_493_000_000; // loser
const C4_TRANSCRIPT_MTIME_MS: i64 = 1_717_492_000_000; // WINNER (the deviation)
const C4_UPDATED_MS: i64 = 1_717_496_000_000; // the losing claude tombstone
const COLD_CLAUDE_MTIME_MS: i64 = 1_717_491_000_000;
const PI_COLD_MTIME_MS: i64 = 1_717_490_000_000;
const CODEX_COLD_MTIME_MS: i64 = 1_717_489_000_000;
const PANE_ALPHA_CREATED_S: i64 = 1_717_488_000; // seconds — the mux `created` unit
const PANE_BETA_CREATED_S: i64 = 1_717_487_000;
const NOW_MS: i64 = 1_717_500_300_000;

// --- the session ids under test. Readable rather than uuid-shaped for claude
//     (the `home-basic` fixture precedent: the claude transcript scan derives the
//     id from the filename stem and validates no shape). codex REQUIRES a real
//     uuidv7 tail (`rollout::parse_filename`), and OpenCode ids are `ses_…` by
//     schema — which is rule 4's evidence, so both are spelled faithfully. ---
const SID_L1: &str = "live-vs-cold-0001";
const SID_L2: &str = "live-vs-tomb-0002";
const SID_A3: &str = "acp-vs-cold-0003";
const SID_C4: &str = "claude-vs-cold-0004";
const SID_COLD_CLAUDE: &str = "cold-claude-0005";
const SID_CODEX: &str = "019ea0b3-04d3-7400-8d95-f55d41e961e4";
const SID_PI: &str = "pi-cold-0007";
const SID_OPENCODE: &str = "ses_merge00080000000000000000";

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Read a golden file, or — when `QD_REGEN_GOLDEN=1` — write `actual` and return
/// (so the first run freezes it). Byte-equality assert. Mirrors `codex_ls.rs`.
fn assert_golden(name: &str, actual: &str) {
    let path = golden_dir().join(name);
    if std::env::var("QD_REGEN_GOLDEN").is_ok() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, actual).unwrap();
        eprintln!("regenerated golden {name}");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {path:?}: {e} (run QD_REGEN_GOLDEN=1)"));
    assert_eq!(actual, expected, "golden mismatch for {name}");
}

/// A hermetic merge-policy run. Holds the tempdir for the test's lifetime.
struct MergeRun {
    _tmp: tempfile::TempDir,
    inputs: join::JoinInputs,
    home: PathBuf,
    codex_home: PathBuf,
    pi_root: PathBuf,
    zmx_dir: PathBuf,
    /// The SAME jailed paths + env the gather ran against, kept so the lane
    /// cross-check can ask each lane to enumerate the identical fixture without
    /// building a second one. See `the_lane_lists_agree_with_the_cold_rows_qd_ships`.
    paths: dispatch::paths::QdPaths,
    env: MapEnv,
}

impl MergeRun {
    /// Replace the volatile absolute prefixes so the golden is path-stable
    /// (NORMALIZATION-class, the `codex_ls.rs` lineage). Longest/most-specific
    /// roots first — they are siblings here, but order-independence is not worth
    /// relying on.
    fn normalize(&self, text: &str) -> String {
        text.replace(&self.codex_home.to_string_lossy().into_owned(), "<CODEX_HOME>")
            .replace(&self.pi_root.to_string_lossy().into_owned(), "<PI_ROOT>")
            .replace(&self.zmx_dir.to_string_lossy().into_owned(), "<ZMX_DIR>")
            .replace(&self.home.to_string_lossy().into_owned(), "<HOME>")
    }
}

/// `cwd` → claude project-dir slug (`/work/x` → `-work-x`), the encoding
/// `jsonl::cwd_to_project_path` uses. Spelled here so the fixture writes the same
/// tree the scanner reads without widening that module's API.
fn slug(cwd: &str) -> String {
    cwd.replace(['/', '.'], "-")
}

/// Write a claude transcript at `<projects>/<slug(cwd)>/<sid>.jsonl` and freeze its
/// mtime. Records carry NO `timestamp`, so `lastTimestamp` stays absent and the
/// cold row's `lastActive` falls back to the frozen mtime — deterministic without
/// having to keep ISO strings and epoch constants in sync.
fn write_claude_transcript(projects: &Path, cwd: &str, sid: &str, name: &str, mtime: i64) -> PathBuf {
    let dir = projects.join(slug(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{sid}.jsonl"));
    let lines = [
        format!(r#"{{"type":"agent-name","agentName":"{name}"}}"#),
        format!(r#"{{"type":"user","cwd":"{cwd}","gitBranch":"transcript-branch","message":{{"content":"hello"}}}}"#),
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#.to_string(),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    set_mtime_ms(&path, mtime);
    path
}

/// Build the jailed fixture and run the REAL `join::gather` (the same call `qd ls`
/// and `qd info` drive).
fn run_merge(opts: JoinOpts) -> MergeRun {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let codex_home = tmp.path().join("codex-home");
    let pi_root = tmp.path().join("pi-sessions");
    let xdg_data = tmp.path().join("xdg-data");
    let zmx_dir = tmp.path().join("zmx-501");
    let sessions = home.join(".claude").join("sessions");
    let projects = home.join(".claude").join("projects");
    for d in [&sessions, &projects, &codex_home, &pi_root, &zmx_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    assert_not_real_home(&home);

    // --- (1) LIVE vs COLD-claude: the live registry row and a claude transcript
    //     both claim SID_L1. Rule 1 says live wins. The transcript deliberately
    //     records a DIFFERENT cwd (`/work/l1-transcript-cwd`) from the registry
    //     row (`/work/l1-live`) — rule 2's rationale made observable: a cold
    //     transcript's cwd says where the transcript was WRITTEN, not where the
    //     session is now, so a field-level merge would put a stale path on a live
    //     row. ---
    std::fs::write(
        sessions.join("3101.json"),
        format!(
            r#"{{"pid":3101,"sessionId":"{SID_L1}","cwd":"/work/l1-live","startedAt":{s},"updatedAt":{u},"status":"idle","name":"live-vs-cold","version":"1.0.0"}}"#,
            s = L1_UPDATED_MS - 1000,
            u = L1_UPDATED_MS,
        ),
    )
    .unwrap();
    write_claude_transcript(
        &projects,
        "/work/l1-transcript-cwd",
        SID_L1,
        "l1-transcript-name",
        L1_TRANSCRIPT_MTIME_MS,
    );

    // --- (2) LIVE vs TOMBSTONE: a live row and a tombstone (a DIFFERENT pid file)
    //     both claim SID_L2. Rule 1 says live wins — "it is running" outranks
    //     "qd killed it". ---
    std::fs::write(
        sessions.join("3102.json"),
        format!(
            r#"{{"pid":3102,"sessionId":"{SID_L2}","cwd":"/work/l2-live","startedAt":{s},"updatedAt":{u},"status":"busy","name":"live-vs-tomb","version":"1.0.0"}}"#,
            s = L2_UPDATED_MS - 1000,
            u = L2_UPDATED_MS,
        ),
    )
    .unwrap();
    std::fs::write(
        sessions.join("3902.json.tombstoned"),
        format!(
            r#"{{"pid":3902,"sessionId":"{SID_L2}","cwd":"/work/l2-tombstone","startedAt":{s},"updatedAt":{u},"status":"idle","name":"l2-tombstone-name","version":"1.0.0"}}"#,
            s = L2_UPDATED_MS - 9000,
            u = L2_UPDATED_MS - 8000,
        ),
    )
    .unwrap();

    // --- (3) ACP TOMBSTONE vs its COLD-JSONL SHADOW (rule 4's one real
    //     cross-provider collision). The ACP-CC bridge writes CLAUDE-shaped JSONL
    //     under ~/.claude/projects, so the claude cold scan finds a transcript
    //     carrying an acp session's id. `join.rs`'s `acp_tombstone_sids` special
    //     case makes the tombstone win — rule 1, upheld. Winner is the WHOLE
    //     tombstone row: provider `acp/claude-code`, the FRIENDLY name, status
    //     killed. None of the shadow's fields survive (rule 2). ---
    std::fs::write(
        sessions.join("3903.json.tombstoned"),
        format!(
            r#"{{"pid":3903,"sessionId":"{SID_A3}","cwd":"/work/a3-tombstone","startedAt":{s},"updatedAt":{u},"status":"idle","name":"acp-friendly-name","version":"1.0.0","provider":"acp/claude-code"}}"#,
            s = A3_UPDATED_MS - 1000,
            u = A3_UPDATED_MS,
        ),
    )
    .unwrap();
    write_claude_transcript(
        &projects,
        "/work/a3-shadow-cwd",
        SID_A3,
        "a3-shadow-name",
        A3_TRANSCRIPT_MTIME_MS,
    );

    // --- (4) THE DEVIATION: a CLAUDE tombstone with the same shadow shape loses to
    //     its cold JSONL, because `acp_tombstone_sids` is scoped to `acp/*`. This
    //     is cold > tombstoned — the REVERSE of rule 1. Deliberately preserved;
    //     see `cold_wins_over_tombstone_for_claude_codex_deviation_from_ruling`. ---
    std::fs::write(
        sessions.join("3904.json.tombstoned"),
        format!(
            r#"{{"pid":3904,"sessionId":"{SID_C4}","cwd":"/work/c4-tombstone","startedAt":{s},"updatedAt":{u},"status":"idle","name":"claude-tombstone-name","version":"1.0.0"}}"#,
            s = C4_UPDATED_MS - 1000,
            u = C4_UPDATED_MS,
        ),
    )
    .unwrap();
    write_claude_transcript(
        &projects,
        "/work/c4-shadow-cwd",
        SID_C4,
        "c4-shadow-name",
        C4_TRANSCRIPT_MTIME_MS,
    );

    // --- (5) one COLD row per provider, uncontested: claude … ---
    write_claude_transcript(
        &projects,
        "/work/cold-claude",
        SID_COLD_CLAUDE,
        "cold-claude-name",
        COLD_CLAUDE_MTIME_MS,
    );

    // --- … codex (a rollout with no registry row; the sqlite index is garbage so
    //     the row surfaces from the rollout scan — the proven degrade path) … ---
    let day = codex_home.join("sessions").join("2026/06/04");
    std::fs::create_dir_all(&day).unwrap();
    let rollout = day.join(format!("rollout-2026-06-04T08-33-20-{SID_CODEX}.jsonl"));
    std::fs::write(
        &rollout,
        format!(
            "{meta}\n{started}\n{done}\n",
            meta = format!(
                r#"{{"timestamp":"2026-06-04T08:33:20.000Z","type":"session_meta","payload":{{"id":"{SID_CODEX}","cwd":"/work/cold-codex","originator":"qd"}}}}"#
            ),
            started = r#"{"timestamp":"2026-06-04T08:33:21.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t-cold"}}"#,
            done = r#"{"timestamp":"2026-06-04T08:33:30.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t-cold","last_agent_message":"done"}}"#,
        ),
    )
    .unwrap();
    set_mtime_ms(&rollout, CODEX_COLD_MTIME_MS);
    std::fs::write(
        codex_home.join("state_5.sqlite"),
        b"not a sqlite database at all \x00\xff",
    )
    .unwrap();

    // --- … pi (FLAT layout — what pi writes when the session dir is handed to it
    //     via `PI_CODING_AGENT_SESSION_DIR`; the id is the tail after the LAST
    //     `_`) … ---
    let pi_file = pi_root.join(format!("2026-06-04T08-20-00-000Z_{SID_PI}.jsonl"));
    std::fs::write(
        &pi_file,
        concat!(
            r#"{"type":"agent-name","agentName":"cold-pi-name"}"#,
            "\n",
            r#"{"type":"user","cwd":"/work/cold-pi","message":{"content":"hello"}}"#,
            "\n",
        ),
    )
    .unwrap();
    set_mtime_ms(&pi_file, PI_COLD_MTIME_MS);

    // --- … and opencode (one row in the monolithic `opencode.db` under
    //     `$XDG_DATA_HOME/opencode`). The schema is the consumed subset of the real
    //     one; `provider/opencode` SELECTs by name, so omitted real columns are
    //     irrelevant. ---
    let store = xdg_data.join("opencode");
    std::fs::create_dir_all(&store).unwrap();
    mint_opencode_store(&store.join("opencode.db"));

    // --- (6) TWO orphaned mux panes. Both emit `session_id: ""`. Rule 3: an absent
    //     id is not an id — the `ZmxOnly` branch neither consults nor writes
    //     `seen_session_ids`, keying on PANE NAME instead, so both survive. One
    //     pane alone could not prove that; two can. Neither name matches any cold
    //     row's name, so neither is consumed by the cold-JSONL name-merge. ---
    let zmx_text = format!(
        "name=orphan-pane-alpha\tpid=7701\tclients=1\tcreated={a}\tstart_dir=/work/orphan-alpha\tcmd=claude\n\
         name=orphan-pane-beta\tpid=7702\tclients=0\tcreated={b}\tstart_dir=/work/orphan-beta\tcmd=claude\n",
        a = PANE_ALPHA_CREATED_S,
        b = PANE_BETA_CREATED_S,
    );

    let env = MapEnv {
        vars: [
            ("ZMX_DIR", zmx_dir.to_string_lossy().into_owned()),
            ("CODEX_HOME", codex_home.to_string_lossy().into_owned()),
            (
                "PI_CODING_AGENT_SESSION_DIR",
                pi_root.to_string_lossy().into_owned(),
            ),
            ("XDG_DATA_HOME", xdg_data.to_string_lossy().into_owned()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        uid: 501,
    };

    let mux = dispatch::mux::FixtureMux::new().with_dir(zmx_dir.clone(), &zmx_text);
    // The pane pids are NOT ancestors of the live pids, so neither live row claims
    // a pane — the panes stay orphaned, which is the point of the branch. No claude
    // procs → no strays (a stray row is a different surface, out of scope here).
    let pt = FixtureProcessTable {
        ppids: [(3101, 1), (3102, 1), (7701, 1), (7702, 1)]
            .into_iter()
            .collect(),
        alive: [3101, 3102, 7701, 7702].into_iter().collect(),
        claude: Vec::new(),
    };
    let probe = FixtureRelayProbe(Vec::new());
    let clock = FixedClock(NOW_MS);

    let paths = dispatch::paths::QdPaths::from_home(&home);
    let inputs = join::gather(
        &paths,
        &mux,
        &env,
        &pt,
        &probe,
        &clock,
        tmp.path(),
        None, // hermetic: no machine-global XDG-family mux scan.
        opts,
    );

    MergeRun {
        _tmp: tmp,
        inputs,
        home,
        codex_home,
        pi_root,
        zmx_dir,
        paths,
        env,
    }
}

/// Mint the OpenCode store: the consumed subset of the real `session`/`message`
/// schema (the shape mined in the opencode reader's own tests), one session.
fn mint_opencode_store(db_path: &Path) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (
            id TEXT PRIMARY KEY,
            slug TEXT NOT NULL,
            directory TEXT NOT NULL,
            title TEXT NOT NULL,
            version TEXT NOT NULL DEFAULT '1.15.5',
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            tokens_input INTEGER NOT NULL DEFAULT 0,
            tokens_cache_read INTEGER NOT NULL DEFAULT 0,
            tokens_cache_write INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL
        );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session
         (id, slug, directory, title, version, time_created, time_updated,
          tokens_input, tokens_cache_read, tokens_cache_write)
         VALUES (?1, 'nimble-nebula', '/work/cold-opencode', 'cold-opencode-name',
                 '1.15.5', ?2, ?3, 100, 20, 5)",
        rusqlite::params![
            SID_OPENCODE,
            OPENCODE_UPDATED_MS - 60_000,
            OPENCODE_UPDATED_MS
        ],
    )
    .unwrap();
    for m in ["msg_1", "msg_2"] {
        conn.execute(
            "INSERT INTO message (id, session_id) VALUES (?1, ?2)",
            rusqlite::params![m, SID_OPENCODE],
        )
        .unwrap();
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

/// Run the fixture through the pure decider and hand back the joined rows.
fn joined() -> (MergeRun, Vec<Session>) {
    let opts = all_opts();
    let run = run_merge(opts);
    let (mut sessions, _strays) = join::join_with_strays(&run.inputs, opts);
    join::assign_codes(&mut sessions);
    (run, sessions)
}

/// The one row carrying `sid`. Panics naming the rule if there is not EXACTLY one —
/// a duplicate is itself a merge-policy failure (the "Ambiguous — matches 2
/// sessions" class the `seen_session_ids` guard exists to prevent).
fn only(sessions: &[Session], sid: &str) -> Session {
    let hits: Vec<&Session> = sessions.iter().filter(|s| s.session_id == sid).collect();
    assert_eq!(
        hits.len(),
        1,
        "merge policy: exactly ONE row may carry session id {sid:?}; found {} \
         (branches: {:?}). A second row means a source escaped the seen-id guard.",
        hits.len(),
        hits.iter().map(|s| s.which_branch).collect::<Vec<_>>(),
    );
    hits[0].clone()
}

/// Assert which SOURCE supplied the row for `sid`, failing with the rule's name.
fn assert_wins(sessions: &[Session], sid: &str, want: SessionBranch, rule: &str) {
    let row = only(sessions, sid);
    assert_eq!(
        row.which_branch, want,
        "MERGE POLICY VIOLATED for {sid:?}: expected the {want:?} source to win, \
         got {got:?}.\n  Rule: {rule}\n  See doc/tbd/provider-architecture/\
         08-session-merge-policy.md — if this change is intended, change the RULE \
         there first, then this test.",
        got = row.which_branch,
    );
}

// === rule 1 — precedence: live > tombstoned > cold ===

#[test]
fn live_wins_over_a_cold_transcript_claiming_the_same_id() {
    let (_run, sessions) = joined();
    assert_wins(
        &sessions,
        SID_L1,
        SessionBranch::LiveRegistry,
        "1 — live > tombstoned > cold. A running process is the strongest claim \
         about a session's current state; a transcript is merely a file on disk.",
    );
}

#[test]
fn live_wins_over_a_tombstone_claiming_the_same_id() {
    let (_run, sessions) = joined();
    assert_wins(
        &sessions,
        SID_L2,
        SessionBranch::LiveRegistry,
        "1 — live > tombstoned > cold. \"it is running\" outranks \"qd killed it\".",
    );
}

#[test]
fn acp_tombstone_wins_over_its_claude_shaped_cold_shadow() {
    let (_run, sessions) = joined();
    assert_wins(
        &sessions,
        SID_A3,
        SessionBranch::Tombstoned,
        "1 + 4 — a tombstone is a deliberate record that qd killed this session; a \
         cold transcript is merely a file on disk. This is the ONE real \
         cross-provider cold collision: the ACP-CC bridge writes claude-shaped \
         JSONL, so the claude cold scan would otherwise shadow the acp row. \
         `join.rs`'s `acp_tombstone_sids` set is what upholds the rule here.",
    );
}

// === rule 2 — whole row, no field-level merge ===

#[test]
fn the_winning_source_supplies_every_field_no_splicing() {
    let (_run, sessions) = joined();

    // The live winner takes its cwd/status/pid/name from the REGISTRY. The losing
    // transcript recorded cwd `/work/l1-transcript-cwd` and name
    // `l1-transcript-name`; neither may appear. Splicing them in would put the
    // path where the transcript was WRITTEN onto a row describing where the
    // session is NOW.
    let l1 = only(&sessions, SID_L1);
    assert_eq!(
        l1.cwd.as_deref(),
        Some("/work/l1-live"),
        "rule 2: cwd comes from the winning live row, never from the losing transcript",
    );
    assert_eq!(l1.name.as_deref(), Some("live-vs-cold"), "rule 2: name likewise");
    assert_eq!(l1.status, SessionStatus::Idle, "rule 2: status likewise");
    assert_eq!(l1.pid, Some(3101), "rule 2: pid likewise");

    // The acp tombstone winner takes provider + name + status + cwd from the
    // TOMBSTONE. The losing cold shadow would have supplied provider
    // "claude-code", name "a3-shadow-name", status cold — the exact regression
    // `acp_tombstone_sids` exists to prevent (`qd resume <name>` post-stop loses
    // the friendly name AND misroutes to the claude resume path).
    let a3 = only(&sessions, SID_A3);
    assert_eq!(a3.provider, "acp/claude-code", "rule 2: provider from the tombstone");
    assert_eq!(a3.name.as_deref(), Some("acp-friendly-name"));
    assert_eq!(a3.status, SessionStatus::Killed);
    assert_eq!(a3.cwd.as_deref(), Some("/work/a3-tombstone"));

    // AND the precise scope of rule 2, asserted so nobody misreads it: a branch
    // may still compose its row from several of its OWN pre-gathered INPUTS. Both
    // winners above carry `gitBranch` and `jsonlPath` sourced from the transcript,
    // because the LiveRegistry and Tombstoned branches each resolve a transcript
    // for their row via `jsonl_path_for`/`stats_for` — a per-branch input channel,
    // NOT the losing row. Rule 2 forbids splicing fields off a LOSING SOURCE'S
    // ROW; it does not forbid a branch reading its own inputs. Pinned here because
    // the distinction is invisible in the rendered output.
    assert_eq!(
        l1.git_branch.as_deref(),
        Some("transcript-branch"),
        "rule 2 scope: the live branch reads its OWN transcript stats channel",
    );
    assert_eq!(a3.git_branch.as_deref(), Some("transcript-branch"));
}

// === rule 3 — an empty session_id never participates in id-keyed dedup ===

#[test]
fn two_orphaned_panes_both_survive_empty_ids_do_not_collide() {
    let (_run, sessions) = joined();
    let panes: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.which_branch == SessionBranch::ZmxOnly)
        .collect();
    assert_eq!(
        panes.len(),
        2,
        "rule 3: an absent id is not an id. BOTH orphaned panes must survive — the \
         ZmxOnly branch keys on PANE NAME, never on the empty session id. If these \
         collapse to one, something started keying `\"\"` in `seen_session_ids`.",
    );
    for p in &panes {
        assert_eq!(p.session_id, "", "an orphaned pane carries no session id");
    }
    let mut names: Vec<&str> = panes.iter().filter_map(|p| p.name.as_deref()).collect();
    names.sort_unstable();
    assert_eq!(names, ["orphan-pane-alpha", "orphan-pane-beta"]);
}

#[test]
fn empty_id_live_row_shadows_an_empty_id_tombstone() {
    // KNOWN HOLE in rule 3, pinned so it cannot widen unnoticed
    // (doc §Rule 3 — where the rule does not hold today).
    //
    // The ZmxOnly branch is genuinely exempt from id-keyed dedup, but the LIVE and
    // TOMBSTONE branches are not: both fall back to `unwrap_or_default()`, so an
    // id-less row keys under the empty string. A live registry row with no
    // sessionId therefore inserts `""` into `seen_session_ids`, and an id-less
    // tombstone is then skipped as "already seen" — two rows that share nothing
    // colliding on the absence of a key.
    //
    // Narrow in practice (claude writes its own sessionId; the daemon lanes get
    // one from `thread/start`; the codex-interactive pre-identification window is
    // the only live producer) but real. Closing it means giving the live and
    // tombstone branches the same "no id → no dedup key" treatment the registry
    // pre-collapse already gives id-less rows via `dedupe_id`.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let sessions_dir = home.join(".claude").join("sessions");
    std::fs::create_dir_all(home.join(".claude").join("projects")).unwrap();
    std::fs::create_dir_all(&sessions_dir).unwrap();
    assert_not_real_home(&home);

    // A live row with NO sessionId (the codex-interactive pre-identification shape).
    std::fs::write(
        sessions_dir.join("4101.json"),
        format!(
            r#"{{"pid":4101,"cwd":"/work/unidentified","startedAt":{s},"updatedAt":{u},"status":"idle","name":"unidentified-live"}}"#,
            s = L1_UPDATED_MS - 1000,
            u = L1_UPDATED_MS,
        ),
    )
    .unwrap();
    // A tombstone with NO sessionId. Unrelated to the row above in every way.
    std::fs::write(
        sessions_dir.join("4102.json.tombstoned"),
        format!(
            r#"{{"pid":4102,"cwd":"/work/unrelated-dead","startedAt":{s},"updatedAt":{u},"status":"idle","name":"unidentified-tombstone"}}"#,
            s = L2_UPDATED_MS - 1000,
            u = L2_UPDATED_MS,
        ),
    )
    .unwrap();

    let zmx_dir = tmp.path().join("zmx-501");
    std::fs::create_dir_all(&zmx_dir).unwrap();
    let env = MapEnv {
        vars: [("ZMX_DIR".to_string(), zmx_dir.to_string_lossy().into_owned())]
            .into_iter()
            .collect(),
        uid: 501,
    };
    let mux = dispatch::mux::FixtureMux::new().with_dir(zmx_dir.clone(), "");
    let pt = FixtureProcessTable {
        ppids: [(4101, 1)].into_iter().collect(),
        alive: [4101].into_iter().collect(),
        claude: Vec::new(),
    };
    let opts = all_opts();
    let inputs = join::gather(
        &dispatch::paths::QdPaths::from_home(&home),
        &mux,
        &env,
        &pt,
        &FixtureRelayProbe(Vec::new()),
        &FixedClock(NOW_MS),
        tmp.path(),
        None,
        opts,
    );
    let (sessions, _strays) = join::join_with_strays(&inputs, opts);

    let names: Vec<&str> = sessions.iter().filter_map(|s| s.name.as_deref()).collect();
    assert!(
        names.contains(&"unidentified-live"),
        "the id-less live row surfaces; got {names:?}",
    );
    assert!(
        !names.contains(&"unidentified-tombstone"),
        "TODAY'S BEHAVIOUR (the rule-3 hole): the id-less tombstone is swallowed by \
         the id-less live row's `\"\"` key. If this now passes — i.e. the tombstone \
         surfaces — the hole has been CLOSED; delete this test and update \
         doc/tbd/provider-architecture/08-session-merge-policy.md §Rule 3.",
    );
}

// === the NAMED DEVIATION — claude/codex are cold > tombstoned ===

#[test]
fn cold_wins_over_tombstone_for_claude_codex_deviation_from_ruling() {
    // This test encodes a KNOWN-WRONG behaviour ON PURPOSE.
    //
    // Rule 1 says live > tombstoned > cold. For `acp/*` the code implements it
    // (`acp_tombstone_sids`, join.rs). For claude and codex it does NOT: the claude
    // cold-JSONL block runs BEFORE the tombstone block and claims the id first, so a
    // killed claude session with a transcript on disk renders as a COLD claude row —
    // losing its friendly tombstone name, its `killed` status, and its recorded cwd.
    //
    // It is left alone deliberately: flipping it is a user-visible `qd ls` change
    // that would red `join.rs`'s `tombstone_seen_guard_skips_already_cold`, and it
    // does not belong inside a refactor.
    //
    // See doc/tbd/provider-architecture/08-session-merge-policy.md
    // §"The deviation — claude and codex are cold > tombstoned" for what closing it
    // requires.
    let (_run, sessions) = joined();
    let c4 = only(&sessions, SID_C4);
    assert_eq!(
        c4.which_branch,
        SessionBranch::ColdJsonl,
        "DEVIATION PIN: today the claude cold transcript beats the claude tombstone. \
         If this now reads Tombstoned the deviation has been CLOSED — that is the \
         intended end state, but it is a behaviour change: update the doc's \
         deviation section, retire this test, and expect \
         `tombstone_seen_guard_skips_already_cold` to need revisiting.",
    );
    // Whole-row, so the deviation is not a partial loss — EVERY tombstone field
    // goes, which is precisely why it is user-visible.
    assert_eq!(c4.status, SessionStatus::Cold, "the tombstone's `killed` status is lost");
    assert_eq!(
        c4.name.as_deref(),
        Some("c4-shadow-name"),
        "the tombstone's friendly name is lost",
    );
    assert_eq!(
        c4.cwd.as_deref(),
        Some("/work/c4-shadow-cwd"),
        "the tombstone's recorded cwd is lost",
    );
    assert_eq!(c4.provider, "claude-code");
}

// === rule 4 — one cold row per provider, no cross-provider id contention ===

#[test]
fn every_provider_contributes_exactly_one_uncontested_cold_row() {
    let (_run, sessions) = joined();
    for (sid, provider) in [
        (SID_COLD_CLAUDE, "claude-code"),
        (SID_CODEX, "codex"),
        (SID_PI, "pi"),
        (SID_OPENCODE, "opencode"),
    ] {
        let row = only(&sessions, sid);
        assert_eq!(
            row.which_branch,
            SessionBranch::ColdJsonl,
            "rule 4: {sid} is a cold row from the {provider} scan",
        );
        assert_eq!(row.provider, provider, "rule 4: {sid} carries its own provider");
        assert_eq!(row.status, SessionStatus::Cold);
    }
}

// === the source ORDER itself — the thing that decides every case above ===

#[test]
fn the_cold_block_order_is_pinned_live_then_claude_then_pane_then_tombstone_then_codex_pi_opencode() {
    // The policy is currently EMERGENT: which source wins is the order the blocks
    // run in inside `join_sessions_counted`. Reordering them is invisible in a
    // rendered list sorted by lastActive, so pin the order by the only artefact
    // that survives the sort — which branch owns each CONTESTED id.
    //
    // Reading the table below top to bottom IS the block order:
    //   live registry → claude cold JSONL → orphaned panes → tombstones
    //   → codex cold → pi cold → opencode cold
    // with the single `acp/*` inversion between claude-cold and tombstones.
    let (_run, sessions) = joined();
    let contested: Vec<(&str, SessionBranch)> = [SID_L1, SID_L2, SID_A3, SID_C4]
        .into_iter()
        .map(|sid| (sid, only(&sessions, sid).which_branch))
        .collect();
    assert_eq!(
        contested,
        vec![
            (SID_L1, SessionBranch::LiveRegistry), // live beats claude cold
            (SID_L2, SessionBranch::LiveRegistry), // live beats tombstone
            (SID_A3, SessionBranch::Tombstoned),   // acp tombstone beats claude cold
            (SID_C4, SessionBranch::ColdJsonl),    // claude cold beats tombstone (DEVIATION)
        ],
        "the source order changed. Every entry above is a rule in \
         doc/tbd/provider-architecture/08-session-merge-policy.md; the last one is \
         the recorded deviation. Reordering the blocks in `join_sessions_counted` \
         changes which row a user sees.",
    );
}

// === the golden — everything the named assertions did not think to check ===

#[test]
fn ls_merge_policy_golden() {
    let (run, sessions) = joined();
    let (_, strays) = join::join_with_strays(&run.inputs, all_opts());
    let text = run.normalize(&render::to_pretty(&render::ls_json(&sessions, &strays)));
    assert_golden("ls-merge-policy.json", &text);
}

// ===========================================================================
// The stage-2 phase-1 CROSS-CHECK — the lanes vs the rows qd ships
// ===========================================================================
//
// `06-stage2-plan.md` phase 1: "compute both ways, assert agreement in tests,
// keep shipping the old value. Flip once agreeing." Read paths are the only ones
// that can be cross-checked, so this is where the safety comes from — the write
// paths get delegation discipline and nothing else.
//
// The fixture above is exactly the one this needs: four cold stores populated,
// two of their rows deliberately shadowed by a stronger claim, so the check has
// both agreement AND disagreement to account for.

use quorum_qw::lane::{Harness, Lane, Mode};
use quorum_qw::{SessionStatus as LaneStatus, SessionSummary};

/// Ask every lane to enumerate the fixture, flattened and id-keyed.
fn lane_rows(run: &MergeRun) -> Vec<(Lane, SessionSummary)> {
    let mut out = Vec::new();
    for lane in Lane::ALL {
        let listed = quorum_qw::lane_read::list_for(lane, &run.paths, &run.env)
            .unwrap_or_else(|e| panic!("{lane}: list must not fail on a readable fixture: {e}"));
        // A READABLE fixture must report NO degradation — the field exists so an
        // unreadable store can say so, and a green cross-check that quietly
        // dropped rows into `degraded` would prove nothing.
        assert!(
            listed.degraded.is_empty(),
            "{lane}: the fixture store is readable, so nothing may be reported degraded: {:?}",
            listed.degraded
        );
        out.extend(listed.sessions.into_iter().map(|r| (lane, r)));
    }
    out
}

/// THE CROSS-CHECK. Every cold row `qd ls` ships must be produced, field for
/// field, by the lane that owns the store it came from.
///
/// This is the whole safety argument for migrating `join.rs`'s four cold-row
/// blocks behind `LaneOps::list`: not "the code looks equivalent" but "both
/// computations were run against one fixture and agreed".
#[test]
fn the_lane_lists_agree_with_the_cold_rows_qd_ships() {
    let (run, sessions) = joined();
    let lanes = lane_rows(&run);

    for s in sessions.iter().filter(|s| s.which_branch == SessionBranch::ColdJsonl) {
        let hits: Vec<&(Lane, SessionSummary)> = lanes
            .iter()
            .filter(|(_, r)| r.id.as_str() == s.session_id)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "CROSS-CHECK: {} is a ColdJsonl row in `qd ls` but {} lane(s) claim it. \
             Exactly one lane owns each cold store — see quorum_qw::lane_read.",
            s.session_id,
            hits.len()
        );
        let (lane, r) = hits[0];
        // Field by field, because a `SessionSummary` that agrees on the id and
        // disagrees on the cwd would silently change what a user reads.
        assert_eq!(r.provider, s.provider, "CROSS-CHECK provider for {} ({lane})", s.session_id);
        assert_eq!(r.name, s.name, "CROSS-CHECK name for {} ({lane})", s.session_id);
        assert_eq!(r.cwd, s.cwd, "CROSS-CHECK cwd for {} ({lane})", s.session_id);
        assert_eq!(
            r.status,
            LaneStatus::Cold,
            "CROSS-CHECK status for {} ({lane}) — a cold-store row is Cold by construction",
            s.session_id
        );
        assert_eq!(r.turns, s.turns, "CROSS-CHECK turns for {} ({lane})", s.session_id);
        assert_eq!(r.tokens, s.tokens, "CROSS-CHECK tokens for {} ({lane})", s.session_id);
        assert_eq!(
            r.last_active_ms, s.last_active_ms,
            "CROSS-CHECK lastActive for {} ({lane})",
            s.session_id
        );
        assert_eq!(
            r.git_branch, s.git_branch,
            "CROSS-CHECK gitBranch for {} ({lane})",
            s.session_id
        );
    }
}

/// The other half, and the harder one: a lane row that `qd ls` does NOT ship must
/// be explained by a MERGE rule, never by the lane inventing a session.
///
/// Both entries below are the merge doing its job. If this list ever grows, the
/// lane found something the join does not — which is either a new source or a
/// bug, and either way must be named here rather than absorbed.
#[test]
fn every_lane_row_qd_drops_is_dropped_by_a_named_merge_rule() {
    let (run, sessions) = joined();
    let lanes = lane_rows(&run);

    // "Dropped" means the lane's row lost the DEDUP — the id may well still be in
    // the list, sourced from a stronger claim. Keying on the winning branch is
    // what makes the distinction visible; keying on presence alone would report
    // nothing and prove nothing.
    let dropped: Vec<(String, Option<SessionBranch>)> = lanes
        .iter()
        .map(|(_, r)| r.id.0.clone())
        .filter(|id| {
            !sessions
                .iter()
                .any(|s| &s.session_id == id && s.which_branch == SessionBranch::ColdJsonl)
        })
        .map(|id| {
            let won = sessions
                .iter()
                .find(|s| s.session_id == id)
                .map(|s| s.which_branch);
            (id, won)
        })
        .collect();

    assert_eq!(
        dropped,
        vec![
            // RULE 1, live > cold: the live registry row for this id wins, so the
            // claude lane's transcript row is never emitted. The lane finding it
            // is CORRECT — the transcript really is on disk.
            (SID_L1.to_string(), Some(SessionBranch::LiveRegistry)),
            // RULES 1 + 4: the ACP-CC bridge writes claude-shaped JSONL, so the
            // claude lane legitimately finds an ACP session's transcript. Only qd
            // can resolve that — `acp_tombstone_sids` is a CROSS-lane rule, and no
            // single lane's `list()` can evaluate it.
            (SID_A3.to_string(), Some(SessionBranch::Tombstoned)),
        ],
        "a lane enumerated a session `qd ls` does not ship as a cold row, and it is \
         not one of the two the merge policy accounts for. Every entry must cite \
         its rule in doc/tbd/provider-architecture/08-session-merge-policy.md. A \
         `None` winner means the row vanished entirely, which is a LOST SESSION."
    );
}

/// The ownership claim from `quorum_qw::lane_read`, asserted against a fixture
/// that has one populated store per harness.
///
/// Four of the nine lanes read zero because their harness's store belongs to a
/// sibling lane, and a fifth — `claude-code/acp` — reads zero because the ACP
/// bridge has no store of its own at all: it writes into claude's. Nine rows in
/// this table, four non-zero counts, one per harness. That last sentence is the
/// invariant; the rest is which lane holds the count.
#[test]
fn each_cold_store_is_enumerated_by_exactly_one_lane() {
    let (run, _sessions) = joined();
    let counts: Vec<(Lane, usize)> = Lane::ALL
        .into_iter()
        .map(|l| {
            (
                l,
                quorum_qw::lane_read::list_for(l, &run.paths, &run.env)
                    .unwrap()
                    .sessions
                    .len(),
            )
        })
        .collect();

    let expect = |h: Harness, m: Mode| Lane::new(h, m).unwrap();
    assert_eq!(
        counts,
        vec![
            // 4 claude transcripts: L1, A3 (the bridge's shadow), C4, cold-claude.
            (expect(Harness::ClaudeCode, Mode::Pane), 4),
            // The ACP lane of the SAME harness, and the arm this whole test exists
            // to protect. `claude-code/acp` reads `~/.claude/projects` — the very
            // directory the line above just enumerated four rows out of — because
            // the ACP bridge runs the real claude engine and writes claude-shaped
            // JSONL into claude's own store. A cold transcript records no hosting,
            // so nothing in those four files says whether the turn came from a
            // human's pane or from the bridge; there is no split to make, and any
            // number here other than zero DOUBLE-COUNTS every cold claude session
            // in `qd ls`.
            //
            // It reads zero for the same reason `acp/claude-code/daemon` did before
            // ACP became a mode, but the risk is new and larger. The two lanes now
            // share a harness, so a `list_for` arm written as
            // `(Harness::ClaudeCode, _)` — the shape a reader reaches for once the
            // pane arm above is the "obvious" claude arm — would absorb this lane
            // silently and give claude's store two claimants. That is why the
            // wildcard is refused in `lane_read::list_for` and why this row is
            // asserted here and not left to `Harness::cold_store_owner_mode`'s own
            // unit test: this file measures the actual partition over a populated
            // fixture, so a second claimant shows up as 4 and 4 rather than as a
            // table that merely looks plausible.
            (expect(Harness::ClaudeCode, Mode::Acp), 0),
            // A rollout on disk records no hosting, so the store belongs to the
            // STRUCTURAL-default lane and its sibling reports nothing.
            (expect(Harness::Codex, Mode::Pane), 0),
            (expect(Harness::Codex, Mode::Daemon), 1),
            // Same reason, and the one that had to be got right when the
            // app-server lane landed: `codex/app-server` is a THIRD claimant on codex's one
            // store, so if it enumerated too, every cold codex session would be
            // listed twice. Its sessions are LIVE rows, which have never been any
            // lane's to enumerate.
            (expect(Harness::Codex, Mode::AppServer), 0),
            (expect(Harness::Pi, Mode::Pane), 0),
            (expect(Harness::Pi, Mode::Daemon), 1),
            // Same rule, third pi lane: a session file on disk records no
            // hosting, so `pi/extension` enumerates nothing and the one pi cold
            // row above is counted exactly once.
            (expect(Harness::Pi, Mode::Extension), 0),
            // opencode's one lane, and the one harness where the store owner is
            // an ACP lane rather than a pane or a daemon one. Everything opencode
            // does live goes over its bridge, and unlike claude's bridge it
            // persists to a store of its OWN (`opencode.db`) — so here `Mode::Acp`
            // is the claimant, not the abstainer. The two ACP entries in this
            // table reading 0 and 1 is the whole point: the mode does not decide
            // who owns a store, the harness's `cold_store_owner_mode` does.
            (expect(Harness::Opencode, Mode::Acp), 1),
        ],
        "the per-lane cold-store partition changed"
    );
}
