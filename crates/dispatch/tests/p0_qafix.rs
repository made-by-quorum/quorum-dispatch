//! P0 QA-rulings fixes (spec-w6-qafix, orc-ruled 2026-06-10), REWORKED by the
//! P0 start-surface ruling (spec-w7-start-surface, STATE 21) — bin-level pins
//! driving the REAL `qd` binary against a JAILED HOME (L9a / ADD-4 discipline;
//! harness mirrors dupid_collision.rs for the forged-registry rows and
//! ack3_matrix.rs for the fakerepl boot jail — integration test binaries cannot
//! import each other, duplication is the sanctioned shape).
//!
//! - R1 (STATE-21 shape): `--resume` is REMOVED from start (unknown option) and
//!   `--fork <session>` is the VALUED transcript-fork — a NEW participant. The
//!   old R1 live-collision preflight died with `--resume` (forking a LIVE
//!   session is legal: new provider UUID, new participant). The R1 arms below
//!   were retargeted to the fork-shaped surface: target resolution errors
//!   (ambiguous / not-found / empty-sid) + the fork-over-live e2e SUCCESS.
//! - R2: codex start must refuse `--fork` loudly (it used to be silently
//!   dropped before `run_new_codex_daemon`). The R2 `--resume` refusal arm is
//!   structurally dead (the flag is gone) — replaced by the unknown-option pin.
//! - R3: resuming a killed, transcript-less session must state the TRUE
//!   condition (it used to claim "still alive (status: killed)") — UNCHANGED.
//!
//! Each test carries a MUTATION-EVIDENCE comment naming the mutation it kills.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

// The shared P0 jail scaffolding (spec-w9-simplify S3): the binary locators,
// the jail-belt dir scaffold, and the jailed runner live in
// tests/common/p0bins.rs (shared with p0_id_matrix.rs ONLY).
use common::p0bins::{
    establish_jail, fakerepl_bin, run_qd_jailed, qd_bin, qrmux_bin, JailScaffold,
};

/// A pid that is reliably DEAD (never a running process) — `is_pid_alive` → false.
const DEAD_PID: i64 = 2_147_483_646;

/// Spawn a real, short-lived child so we have a genuinely-ALIVE pid distinct from
/// the test runner's. Caller kills + reaps it after the assertion.
fn live_child() -> Child {
    Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep")
}

/// Forge `<pid>.json` (live) and `<pid>.json.tombstoned` rows under a freshly-
/// jailed HOME and run `qd <args...>`. CLAUDE_BIN points at a NONEXISTENT path
/// and PATH is minimal, so a refusal regression fails loudly downstream instead
/// of booting a real claude (the stderr pins then catch the wrong text).
fn run_qd_with_rows(
    dir: &Path,
    rows: &[(i64, String)],
    tombstoned: &[(i64, String)],
    args: &[&str],
) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    common::assert_not_real_home(&home);
    for (pid, json) in rows {
        std::fs::write(sessions.join(format!("{pid}.json")), json).unwrap();
    }
    for (pid, json) in tombstoned {
        std::fs::write(sessions.join(format!("{pid}.json.tombstoned")), json).unwrap();
    }
    let out = Command::new(qd_bin())
        .args(args)
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        .env("CLAUDE_BIN", "/nonexistent/claude-p0-qafix")
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn row(pid: i64, session_id: &str, name: &str, updated_at: i64) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":1717000000000,"updatedAt":{updated_at},"status":"idle","name":"{name}","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
    )
}

// ===========================================================================
// R1 (STATE-21 shape) — `start --fork <session>` target resolution (forged-
// registry arms; the boot-true arms live in the fakerepl jail test below).
//
// RETIRED WITH THE RULING (spec-w7 D3 audit trail):
//   - `start_resume_over_live_original_refuses` — the live-collision preflight
//     is DEAD CODE for start (forking a live session is legal and safe: the
//     forked boot mints a NEW provider UUID = a new participant). The resume-
//     verb-side live refusals stay pinned in dupid_collision.rs.
//   - `start_resume_refuses_a_duplicate_id_collision` — same preflight, gone.
//   - `start_resume_over_a_dead_holder_is_not_refused` — negative twin of the
//     removed guard.
//   - `codex_start_refuses_resume_flag` — `--resume` no longer parses; replaced
//     by `start_resume_is_an_unknown_option` below.
// ===========================================================================

/// STATE-21: `start --resume` is GONE — it must error as an unknown option
/// (the commander-mapped clap shape, exit 1), never reach the backend.
///
/// MUTATION EVIDENCE: re-registering a `--resume` option on cmd_start greens
/// the parse and reds this (the run would fail downstream with a different
/// stderr, or boot).
#[test]
fn start_resume_is_an_unknown_option() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_with_rows(
        t.path(),
        &[],
        &[],
        &["start", "qd-qafix-old", "--resume", "qafix-live-0001"],
    );
    assert_eq!(code, 1, "unknown option exits 1; stderr: {err}");
    assert!(
        err.contains("error: unknown option '--resume'"),
        "the commander unknown-option shape, got: {err}"
    );
}

/// `--fork <ambiguous>`: two ALIVE rows share the target NAME — the standard
/// resolver's loud ambiguity listing must fire (reuse, not reimplementation:
/// the same resolve_or_die error resume/connect print), exit 1, no boot.
///
/// MUTATION EVIDENCE: resolving the fork target with anything that picks an
/// arbitrary winner (e.g. first-match) reds this — the jail would proceed to
/// a boot attempt and fail with non-ambiguity stderr.
#[test]
fn start_fork_ambiguous_target_errors() {
    let mut c1 = live_child();
    let mut c2 = live_child();
    let p1 = c1.id() as i64;
    let p2 = c2.id() as i64;
    // Same NAME, DISTINCT session ids (no id-dedup), both pids alive.
    let rows = [
        (p1, row(p1, "qafix-amb-0001", "qd-qafix-amb", 1717000001000)),
        (p2, row(p2, "qafix-amb-0002", "qd-qafix-amb", 1717000002000)),
    ];

    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_with_rows(
        t.path(),
        &rows,
        &[],
        &["start", "qd-qafix-fk1", "--fork", "qd-qafix-amb"],
    );

    let _ = c1.kill();
    let _ = c1.wait();
    let _ = c2.kill();
    let _ = c2.wait();

    assert_eq!(code, 1, "ambiguous fork target refuses; stderr: {err}");
    assert!(
        err.contains("Ambiguous"),
        "the shared resolver ambiguity listing, got: {err}"
    );
}

/// `--fork <nope>`: no session matches — resolve_or_die's clear not-found
/// error, exit 1, no boot, no mint.
///
/// MUTATION EVIDENCE: defaulting a failed resolution to a fresh start (silent
/// drop of the flag — the old codex pathology) reds this.
#[test]
fn start_fork_not_found_errors() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_with_rows(
        t.path(),
        &[],
        &[],
        &["start", "qd-qafix-fk2", "--fork", "nope"],
    );
    assert_eq!(code, 1, "not-found fork target refuses; stderr: {err}");
    assert!(
        err.contains(r#"No session matching "nope""#),
        "the shared resolver not-found error, got: {err}"
    );
}

/// `--fork <empty-sid target>`: the target resolves but carries NO provider
/// session id (the ZmxOnly-row shape) — there is no transcript to fork. Loud
/// error, exit 1.
///
/// MUTATION EVIDENCE: dropping the empty-sid guard in run_new's fork resolution
/// reds this — the launch would carry `--resume ''` and fail downstream with
/// boot stderr, never this message.
#[test]
fn start_fork_empty_sid_target_errors() {
    let rows = [(DEAD_PID, row(DEAD_PID, "", "qd-qafix-nosid", 1717000001000))];
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_with_rows(
        t.path(),
        &rows,
        &[],
        &["start", "qd-qafix-fk3", "--fork", "qd-qafix-nosid"],
    );
    assert_eq!(code, 1, "empty-sid fork target refuses; stderr: {err}");
    assert!(
        err.contains("has no provider session id") && err.contains("nothing to fork"),
        "the loud nothing-to-fork error naming the condition, got: {err}"
    );
}

// ===========================================================================
// R2 — codex start refuses --fork loudly (the --resume arm died with the flag)
// ===========================================================================

/// R2: `start --provider codex --fork <target>` must refuse loudly (exit 1),
/// BEFORE target resolution (the refusal is about the provider, not the
/// target — so a nonexistent target still gets the teaching refusal). Same
/// silent-drop pathology, same errors-that-teach contract as the original R2.
///
/// MUTATION EVIDENCE: removing the codex --fork refusal in run_new reds this
/// (the resolver would print not-found instead).
#[test]
fn codex_start_refuses_fork_flag() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_with_rows(
        t.path(),
        &[],
        &[],
        &[
            "start",
            "qd-qafix-cx2",
            "--provider",
            "codex",
            "--fork",
            "whatever",
        ],
    );
    assert_eq!(
        code, 1,
        "codex --fork must refuse with exit 1; stderr: {err}"
    );
    assert!(
        err.contains("--fork is not supported with --provider codex"),
        "stderr must name the unsupported flag + provider, got: {err}"
    );
    assert!(
        err.contains("qd resume <name>"),
        "stderr must teach the working revive path, got: {err}"
    );
    assert!(
        !err.contains("No session matching"),
        "the refusal fires BEFORE target resolution, got: {err}"
    );
}

/// Red-team r2 (lead-adjudicated, symmetric twin of the codex-START refusal):
/// the fork TARGET must be same-provider. A codex target's thread UUID handed
/// to `claude --resume <uuid> --fork-session` would fail downstream as a
/// confusing empty boot; the engine refuses at preflight instead, naming both
/// providers.
///
/// MUTATION EVIDENCE: removing the target-provider guard in run_new's fork
/// resolution reds this (the launch would proceed toward the claude boot path
/// and fail later with a different error).
#[test]
fn fork_target_must_be_same_provider() {
    let t = tempfile::tempdir().unwrap();
    let codex_row = r#"{"pid":4242,"sessionId":"cdx-thread-uuid-1","cwd":"/w","startedAt":1717000000000,"updatedAt":1717000600000,"status":"idle","name":"cxwk","provider":"codex","endpoint":"ws://127.0.0.1:18999"}"#;
    let (code, _out, err) = run_qd_with_rows(
        t.path(),
        &[(4242, codex_row.to_string())],
        &[],
        &["start", "qd-qafix-xp", "--fork", "cxwk"],
    );
    assert_eq!(
        code, 1,
        "cross-provider fork must refuse with exit 1; stderr: {err}"
    );
    assert!(
        err.contains("cannot fork \"cxwk\"")
            && err.contains("codex session")
            && err.contains("claude-code"),
        "stderr names the target, its provider, and the new session's provider: {err}"
    );
}

// ===========================================================================
// R3 — honest killed-session resume error
// ===========================================================================

/// R3: resuming a KILLED (tombstoned) session with no resumable transcript must
/// state the true condition — never "Session is still alive (status: killed)"
/// (false: the process is dead). Covers BOTH non-resumable shapes: a tombstone
/// with a session id but NO transcript on disk, and one with an EMPTY id.
///
/// MUTATION EVIDENCE: reverting the Killed/non-resumable arm in resume.rs's
/// must-be-cold gate reds this — stderr regresses to the false "still alive
/// (status: killed)" line.
#[test]
fn resume_killed_transcriptless_session_states_the_truth() {
    // Arm 1: tombstone WITH a session id, no transcript anywhere in the jail.
    // Arm 2: tombstone with an EMPTY session id.
    let arms: [(&str, &str); 2] = [
        ("qafix-ghost-0001", "qd-qafix-ghost"),
        ("", "qd-qafix-ghost2"),
    ];
    for (sid, name) in arms {
        let tombs = [(DEAD_PID, row(DEAD_PID, sid, name, 1717000001000))];

        let t = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_qd_with_rows(t.path(), &[], &tombs, &["resume", name]);

        assert_eq!(
            code, 1,
            "resume of a killed transcript-less session refuses (gate logic unchanged); \
             stderr: {err}"
        );
        assert!(
            err.contains(&format!(
                "session \"{name}\" was stopped and has no resumable transcript"
            )),
            "stderr must state the TRUE condition, got: {err}"
        );
        assert!(
            !err.contains("still alive"),
            "the false 'still alive' claim must be gone for killed sessions, got: {err}"
        );
    }
}

// ===========================================================================
// R1 e2e — the boot-true matrix over the embedded qrmux daemon with fakerepl
// as Claude (jail + driver mirror ack3_matrix.rs; duplication sanctioned —
// integration test binaries cannot import each other)
// ===========================================================================

/// A fakerepl boot jail (ack3_matrix shape, reduced to what this matrix needs).
/// Rooted under /tmp for the zmx/qrmux socket-path length budget (L21).
struct BootJail {
    /// The shared jail-belt scaffold (root/home/xdg/qd_home) — p0bins.
    dirs: JailScaffold,
    projects: PathBuf,
}

impl BootJail {
    fn establish(tag: &str) -> BootJail {
        // fakerepl's jail belt (a4-spec §5) requires HOME to match
        // `*/qdrg-runs/*/home` with qd_home/zmx/tmp as root-siblings.
        let dirs = establish_jail(Path::new("/tmp/qd-p0qafix"), tag);
        let projects = dirs.home.join(".claude").join("projects").join("proj");
        BootJail { dirs, projects }
    }

    /// fakerepl env for a session adopting `uuid` as its provider session id.
    fn fakerepl_env(&self, name: &str, uuid: &str) -> Vec<(&'static str, String)> {
        vec![
            ("QD_FAKEREPL_NAME", name.to_string()),
            ("QD_FAKEREPL_SESSION_ID", uuid.to_string()),
            (
                "QD_FAKEREPL_CONVO_JSONL",
                self.projects
                    .join(format!("{uuid}.jsonl"))
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]
    }

    fn run_qd(&self, args: &[&str], extra: &[(&str, String)]) -> (i32, String, String) {
        // WP-B-CS-1 (D2): force the INTERACTIVE surface for `qd start` — this harness
        // runs qd with piped stdio (non-TTY) + a fake-claude CLAUDE_BIN (not a PTY for
        // qd), so a bare start would auto-detect the HEADLESS surface (and a no-`-p`
        // start would hit Fork B's refuse-no-prompt). These boot-matrix tests exercise
        // the interactive create + --fork path. Delta flagged in the WP-B-CS-1 response.
        let injected: Vec<String>;
        let arg_refs: Vec<&str>;
        let args: &[&str] = if args.first() == Some(&"start") {
            injected = std::iter::once("start".to_string())
                .chain(std::iter::once("--interactive".to_string()))
                .chain(args[1..].iter().map(|s| s.to_string()))
                .collect();
            arg_refs = injected.iter().map(String::as_str).collect();
            &arg_refs
        } else {
            args
        };
        run_qd_jailed(&self.dirs, &fakerepl_bin(), args, extra)
    }

    fn teardown(&self, names: &[&str]) {
        for name in names {
            let _ = self.run_qd(&["stop", "--force", name], &[]);
        }
        let _ = std::fs::remove_dir_all(&self.dirs.root);
        let _ = std::fs::remove_dir_all(&self.dirs.xdg);
    }
}

/// Fork boot-true matrix (STATE-21 shape, end-to-end over real boots).
/// RETIRED with the ruling: the old `start_resume_live_matrix_e2e` arm 1
/// (live-original refusal) and arm 3 (`start --resume` after stop) — both
/// `--resume`-shaped; the live preflight is dead code for start and the
/// resume VERB owns same-participant wake (its pins live in p0_id_matrix.rs
/// A1/A4 and dupid_collision.rs).
///
///   1. fork OVER THE LIVE original by NAME → SUCCEEDS (the ruled headline:
///      a fork is a NEW participant — new provider UUID — so live targets are
///      legal and safe; the old refusal would have blocked exactly this);
///   2. both participants visible side by side (two ls rows, two UUIDs);
///   3. `start --resume <uuid>` is an unknown option in the boot jail too.
///
/// MUTATION EVIDENCE: arm 1 reds if fork resolution refuses live holders
/// (re-adding the old preflight) or mis-resolves the name; arm 2 reds if the
/// fork CONTINUED the original participant instead of forking (one row, one
/// UUID); arm 3 reds if `--resume` parses again.
#[test]
fn start_fork_live_matrix_e2e() {
    let _ = qrmux_bin();
    let jail = BootJail::establish("r1");
    let u1 = "aaaaaaaa-1111-2222-3333-444444444444";

    // Arm 0 (setup): boot the original; it registers provider uuid u1.
    let (code, _out, err) = jail.run_qd(&["start", "orig"], &jail.fakerepl_env("orig", u1));
    assert_eq!(code, 0, "original boots; stderr: {err}");

    // punch item 11 + WP-B5-iii Mechanism S: the fork source needs a REAL
    // transcript on disk — qd reads it, copies+rekeys+truncates at a SAFE
    // `end_turn` boundary, and seeds the fork at a fresh qd-minted uuid. Real
    // claude writes one at boot; fakerepl doesn't, so plant a minimal but valid
    // one (one completed turn) keyed to u1.
    std::fs::create_dir_all(&jail.projects).unwrap();
    let parent_jsonl = [
        serde_json::json!({"type":"user","uuid":"pu1","parentUuid":null,"sessionId":u1}),
        serde_json::json!({"type":"assistant","uuid":"pa1","parentUuid":"pu1","sessionId":u1,"message":{"stop_reason":"end_turn"}}),
        serde_json::json!({"type":"ai-title","sessionId":u1,"aiTitle":"orig"}),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    std::fs::write(
        jail.projects.join(format!("{u1}.jsonl")),
        format!("{parent_jsonl}\n"),
    )
    .unwrap();

    // Arm 1: fork over the SAME live original, by NAME. Mechanism S: qd mints the
    // fork's OWN uuid PRE-spawn and seeds <fork_uuid>.jsonl; the forked fakerepl
    // adopts that uuid via `--resume <fork_uuid>` (faithful claude emulation) — the
    // test does NOT pre-inject a session id (name-only env), so the registered
    // uuid is whatever qd minted.
    let (code, _out, err) = jail.run_qd(
        &["start", "forked", "--fork", "orig"],
        &[("QD_FAKEREPL_NAME", "forked".to_string())],
    );
    assert_eq!(
        code, 0,
        "--fork over a live original succeeds (a new participant); stderr: {err}"
    );

    // Arm 2 (Mechanism S identity): qd seeded EXACTLY ONE new transcript at the
    // fork's own qd-minted uuid (≠ the parent's u1); the parent transcript is
    // byte-untouched; the seed is rekeyed to the fork uuid with NO parent-id leak.
    let parent_after = std::fs::read_to_string(jail.projects.join(format!("{u1}.jsonl"))).unwrap();
    assert_eq!(
        parent_after,
        format!("{parent_jsonl}\n"),
        "parent transcript byte-untouched"
    );
    // The seed lands under <projects>/<slug(fork-launch-cwd)>/<fork_uuid>.jsonl
    // (claude resolves --resume by the launch-cwd slug); find it by walking the
    // projects tree for the one NEW <uuid>.jsonl that is not the parent's.
    let projects_root = jail.projects.parent().unwrap().to_path_buf();
    let mut seeds: Vec<PathBuf> = vec![];
    for sub in std::fs::read_dir(&projects_root)
        .unwrap()
        .filter_map(|e| e.ok())
    {
        if sub.path().is_dir() {
            for f in std::fs::read_dir(sub.path())
                .unwrap()
                .filter_map(|e| e.ok())
            {
                let n = f.file_name().to_string_lossy().into_owned();
                if n.ends_with(".jsonl") && n != format!("{u1}.jsonl") {
                    seeds.push(f.path());
                }
            }
        }
    }
    assert_eq!(
        seeds.len(),
        1,
        "exactly one NEW seeded transcript (the fork): {seeds:?}"
    );
    let seed_path = seeds.pop().unwrap();
    let fork_uuid = seed_path
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_ne!(fork_uuid, u1, "the fork has its OWN uuid, not the parent's");
    let seed = std::fs::read_to_string(&seed_path).unwrap();
    assert!(
        seed.contains(&fork_uuid) && !seed.contains(u1),
        "seed rekeyed to the fork uuid with no parent-id leak: {seed}"
    );

    // Arm 2b: BOTH participants are alive side by side — two rows, two UUIDs.
    let (code, out, err) = jail.run_qd(&["ls", "--all", "--json"], &[]);
    assert_eq!(code, 0, "ls --json; stderr: {err}");
    assert!(
        out.contains(u1) && out.contains(&fork_uuid),
        "both the original ({u1}) and the forked participant ({fork_uuid}) surface: {out}"
    );

    // Arm 2c (WP-B5-iii obl-3): the forked claude row carries provider==None ON
    // DISK. claude rows never write a `provider` field; the join default fills
    // ABSENT->claude-code only at read-back (so `ls --json` shows "claude-code" for
    // BOTH rows — it cannot distinguish None there). We assert the raw `<pid>.json`
    // ROW, which is where the B5-i `provider` defect hid (green at the row layer,
    // `qd connect` exit-1'd live). Fork-specific: keyed to the "forked" participant.
    let sessions_dir = jail.dirs.home.join(".claude").join("sessions");
    let rows = dispatch::registry::read_entries(&sessions_dir, false);
    let fork_row = rows
        .iter()
        .find(|e| e.entry.name.as_deref() == Some("forked"))
        .unwrap_or_else(|| {
            panic!(
                "the forked participant registered a <pid>.json row; rows present: {:?}",
                rows.iter()
                    .map(|e| e.entry.name.clone())
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        fork_row.entry.provider, None,
        "obl-3: the forked claude row carries provider=None on disk (the join default \
         fills ABSENT->claude-code only at read-back; B5-i cautionary defect); got {:?}",
        fork_row.entry.provider
    );

    // Arm 3: the removed flag is an unknown option here too (boot jail, full
    // PATH/registry state — the parse rejection is environment-independent).
    let (code, _out, err) = jail.run_qd(&["start", "branch", "--resume", u1], &[]);
    assert_eq!(code, 1, "start --resume is gone; stderr: {err}");
    assert!(
        err.contains("error: unknown option '--resume'"),
        "the commander unknown-option shape, got: {err}"
    );

    jail.teardown(&["orig", "forked"]);
}

/// WP-B5-iii obl-5 (§5a staleness): forking a source whose transcript ends
/// mid-in-flight-tool must REPORT the gap ("forking at the latest SAFE
/// boundary …, mid-flight on <tool>") and fork the SAFE prefix — never silently
/// fork the unsafe tail. Exercises the report through the live `qd start --fork`
/// verb (Mechanism-S `resolve(Latest)` retreats; lifecycle.rs surfaces it).
///
/// MUTATION EVIDENCE: dropping the staleness surfacing (or seeding the in-flight
/// tail) reds the stderr assert; the guard (boundary < tail) is unit-pinned in
/// fork_seed.rs.
#[test]
fn start_fork_in_flight_source_reports_staleness() {
    let _ = qrmux_bin();
    let jail = BootJail::establish("stale");
    let u1 = "cccccccc-1111-2222-3333-444444444444";

    let (code, _out, err) = jail.run_qd(&["start", "orig"], &jail.fakerepl_env("orig", u1));
    assert_eq!(
        code, 0,
        "orig boots (registers sessionId u1); stderr: {err}"
    );

    // Plant an IN-FLIGHT-tailed transcript: one completed turn (end_turn) then an
    // unanswered Bash tool_use — the lone unsafe end-state (§5 #3).
    std::fs::create_dir_all(&jail.projects).unwrap();
    let inflight = [
        serde_json::json!({"type":"user","uuid":"iu1","parentUuid":null,"sessionId":u1}),
        serde_json::json!({"type":"assistant","uuid":"ia1","parentUuid":"iu1","sessionId":u1,"message":{"stop_reason":"end_turn"}}),
        serde_json::json!({"type":"assistant","uuid":"ia2","parentUuid":"ia1","sessionId":u1,"message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hi"}}]}}),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    std::fs::write(
        jail.projects.join(format!("{u1}.jsonl")),
        format!("{inflight}\n"),
    )
    .unwrap();

    // Fork it: succeeds (forks the SAFE turn-1 prefix) AND reports staleness.
    let (code, _out, err) = jail.run_qd(
        &["start", "forked", "--fork", "orig"],
        &[("QD_FAKEREPL_NAME", "forked".to_string())],
    );
    assert_eq!(
        code, 0,
        "fork of an in-flight source still succeeds (safe prefix); stderr: {err}"
    );
    assert!(
        err.contains("SAFE boundary") && err.contains("Bash"),
        "§5a: the verb must report forking-as-of-before the in-flight Bash, never silently stale: {err}"
    );

    jail.teardown(&["orig", "forked"]);
}
