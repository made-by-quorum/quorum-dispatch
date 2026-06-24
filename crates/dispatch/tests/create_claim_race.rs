//! HARDENING #2 concurrency gate (spec §6 / §11.4 / §11.10): race N≥4 SEPARATE
//! PROCESSES through the `sb new` CREATE-PATH claim wiring against ONE shared
//! claims dir — exactly one wins, the rest lose at the claim.
//!
//! This is the multi-PROCESS analogue of A1's in-process threaded claim race
//! (registry.rs `claim_concurrency_exactly_one_winner_same_name`). The thread
//! test proves the O_EXCL primitive; THIS test proves the create-path WIRING
//! around it — `run_new` → `NewDeps::claims_dir()` derivation → `claim_name` →
//! `NameClaimed` for losers — across real process boundaries (no shared-memory
//! atomics can paper over a race).
//!
//! Re-exec trick: the test binary re-execs ITSELF with a marker env var set;
//! the re-exec'd child runs ONE `run_new` against the shared home/claims dir and
//! prints `WIN` / `LOSE` on stdout. The parent spawns N children pointed at the
//! same temp home, waits, and asserts exactly one `WIN`.
//!
//! Offline: the child wires a `FixtureMux` (winner's session lists as attachable
//! so it reaches `Ok`) + a SLOW `OkBootWaiter` that holds the claim across the
//! race window, so concurrent children genuinely collide on the O_EXCL open.
//! No live zmx / claude (ScriptedExec/FixtureMux only).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use dispatch::create::{run_new, NewDeps, NewParams};
use dispatch::effects::{FixedClock, MapEnv};
use dispatch::exec::{ExecResult, ScriptedExec};
use dispatch::mux::{Mux, MuxSession};
use dispatch::paths::SbPaths;

mod common;

/// Env marker: when set, the process IS a re-exec'd child and runs `child_body`.
const CHILD_ENV: &str = "SB_CREATE_RACE_CHILD";
/// The shared temp home the children + parent agree on.
const HOME_ENV: &str = "SB_CREATE_RACE_HOME";
/// The canonical zmx dir the children list against.
const CANON_ENV: &str = "SB_CREATE_RACE_CANON";

/// A mux that lists NOTHING but SLEEPS inside `run_detached` — so the claim
/// winner holds its claim across a wide window, guaranteeing concurrently-spawned
/// children genuinely collide on the O_EXCL open (rather than serializing by luck
/// because the winner released too fast). Lists empty → the winner surfaces as
/// NotAttachable (its "got past the claim" marker), which is fine for this test.
struct SlowEmptyMux;
impl Mux for SlowEmptyMux {
    fn list(&self, _d: &Path) -> std::io::Result<Vec<MuxSession>> {
        Ok(vec![])
    }
    fn list_raw(&self, _d: &Path) -> std::io::Result<Vec<MuxSession>> {
        Ok(vec![])
    }
    fn run_detached(
        &self,
        _d: &Path,
        _n: &str,
        _c: &str,
        _w: &Path,
    ) -> std::io::Result<ExecResult> {
        // Hold the create window open so concurrent racers collide on the claim.
        std::thread::sleep(std::time::Duration::from_millis(400));
        Ok(ExecResult {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        })
    }
    fn send(&self, _d: &Path, _n: &str, _t: &str) -> std::io::Result<ExecResult> {
        unreachable!()
    }
    fn kill(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
        Ok(0)
    }
    fn history(&self, _d: &Path, _n: &str) -> std::io::Result<String> {
        Ok(String::new())
    }
    fn wait(&self, _d: &Path, _n: &[String]) -> std::io::Result<i32> {
        Ok(0)
    }
    fn attach(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
        Ok(0)
    }
}

const ZMX_HELP_OK: &str = "\
Commands:
  [s]end <name> <text...>                  Send raw input to session PTY
  [r]un <name> [-d] [command...]           Send command without attaching
";

/// Env: the file each child writes its WIN/LOSE result into (the test harness
/// CAPTURES child stdout, so we signal via a file the parent reads instead).
const RESULT_ENV: &str = "SB_CREATE_RACE_RESULT";

/// The body each re-exec'd child runs: one `run_new` for the SHARED name against
/// the SHARED home → writes WIN (got past the claim) or LOSE (NameClaimed) to its
/// result file. Exits 0 either way (the PARENT decides pass/fail by counting).
fn child_body() -> ! {
    let home = PathBuf::from(std::env::var(HOME_ENV).unwrap());
    let canonical = PathBuf::from(std::env::var(CANON_ENV).unwrap());
    let result_file = PathBuf::from(std::env::var(RESULT_ENV).unwrap());
    common::assert_not_real_home(&home);

    let paths = SbPaths::from_home(&home);
    let exec = ScriptedExec::new().on("zmx", &["--help"], Some(0), ZMX_HELP_OK, "");
    // SlowEmptyMux: pre-check passes (nothing listed) so all children race the
    // O_EXCL claim; the winner HOLDS its claim across the 400ms run_detached
    // window so concurrently-spawned losers genuinely collide → NameClaimed.
    let mux = SlowEmptyMux;
    let env = MapEnv {
        vars: HashMap::new(),
        uid: 501,
    };
    let clock = FixedClock(1_700_000_000_000);
    let waiter = dispatch::create::OkBootWaiter;
    let deps = NewDeps {
        mux: &mux,
        exec: &exec,
        env: &env,
        clock: &clock,
        paths: &paths,
        canonical_dir: canonical,
        legacy_dirs: vec![],
        boot_waiter: &waiter,
        // codex P1 W3: NewDeps now carries the resolved provider (the launch cmd
        // builds through provider.launch_plan; the claude impl derives its config
        // off fx.paths.home/.sb/config.toml — nonexistent in this jail → DEFAULT_FLAGS,
        // identical to the dropped no-config path). This race test never inspects
        // the launch cmd; the claude provider keeps the byte-stable wiring.
        provider: &dispatch::provider::ClaudeProvider,
        backend: dispatch::mux_selector::Backend::Zmx,
    };
    let params = NewParams {
        name: "racey".to_string(),
        agent: None,
        resume: None,
        fork: false,
        claude_args: vec![],
        model: None,
        cwd: PathBuf::from("/work"),
        backend_env: vec![],
        backend_env_unset: vec![],
        sb_session_id: None,
        render: dispatch::launch::RenderMode::Inline,
    };
    let marker = match run_new(&deps, &params) {
        // The claim winner gets PAST the claim and reaches the create+I6 steps.
        // With the empty mux that surfaces as NotAttachable — the marker that
        // this process acquired the claim (it ran run_detached). That IS the win.
        Err(dispatch::create::NewError::NotAttachable { .. }) => "WIN".to_string(),
        // A loser fails AT the claim, never reaching run_detached.
        Err(dispatch::create::NewError::NameClaimed { .. }) => "LOSE".to_string(),
        // Ok would mean the I6 verify passed (shouldn't happen with the empty
        // mux), but it equally means the claim was won — count it as a win.
        Ok(_) => "WIN".to_string(),
        // Any OTHER error is a test-harness problem (not a claim outcome) — make
        // it loud so a vacuous pass can't hide here.
        Err(other) => format!("ERR:{other}"),
    };
    std::fs::write(&result_file, marker).expect("write result file");
    std::process::exit(0);
}

/// Path to THIS test binary (for re-exec). `current_exe` is the running test
/// executable.
fn self_exe() -> PathBuf {
    std::env::current_exe().expect("current_exe")
}

#[test]
fn create_path_claim_race_exactly_one_winner_across_processes() {
    // If we ARE the re-exec'd child, run the body and exit before the harness
    // tries to run other tests in this process.
    if std::env::var(CHILD_ENV).is_ok() {
        child_body();
    }

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let canonical = temp.path().join("zmx-501");
    std::fs::create_dir_all(&home).unwrap();
    common::assert_not_real_home(&home);

    const N: usize = 6;
    // Spawn N children, all pointed at the SAME home + claims dir, racing `racey`.
    // Each child writes its outcome to its own result file (the harness captures
    // child stdout, so a file is the reliable channel). Spawn them all FIRST
    // (non-blocking), THEN wait — so they overlap on the claim window.
    let result_files: Vec<PathBuf> = (0..N)
        .map(|i| temp.path().join(format!("result-{i}")))
        .collect();
    let mut spawned: Vec<_> = (0..N)
        .map(|i| {
            Command::new(self_exe())
                .arg("--exact")
                .arg("create_path_claim_race_exactly_one_winner_across_processes")
                .env(CHILD_ENV, "1")
                .env(HOME_ENV, &home)
                .env(CANON_ENV, &canonical)
                .env(RESULT_ENV, &result_files[i])
                .spawn()
                .expect("spawn child")
        })
        .collect();
    for child in &mut spawned {
        child.wait().expect("child wait");
    }

    let mut wins = 0;
    let mut loses = 0;
    let mut errs = Vec::new();
    for rf in &result_files {
        let marker = std::fs::read_to_string(rf)
            .unwrap_or_else(|e| panic!("child result file {rf:?} missing: {e}"));
        if marker == "WIN" {
            wins += 1;
        } else if marker == "LOSE" {
            loses += 1;
        } else {
            errs.push(marker);
        }
    }

    assert!(
        errs.is_empty(),
        "no child should hit a non-claim error: {errs:?}"
    );
    assert_eq!(
        wins, 1,
        "exactly ONE process must win the claim (got {wins})"
    );
    assert_eq!(
        loses,
        N - 1,
        "the other {} processes must lose at the claim (got {loses})",
        N - 1
    );

    // The claim file was released by the winner (RAII on run_new return), so the
    // create window is closed and the name is claimable again post-race.
    let claims_dir = home.join(".claude").join("claims");
    let racey_claim = claims_dir.join("racey.claim");
    assert!(
        !racey_claim.exists(),
        "the winner must release its claim on return: {racey_claim:?} still present"
    );
}

/// Sanity: the create-path claims_dir derivation matches where the race test
/// expects the file (`<home>/.claude/claims`). Guards against a silent drift in
/// `NewDeps::claims_dir()` that would make the race test point at the wrong dir.
#[test]
fn claims_dir_is_under_claude_root() {
    let home = Path::new("/jail/home");
    let paths = SbPaths::from_home(home);
    let exec = ScriptedExec::new();
    let mux = SlowEmptyMux;
    let env = MapEnv {
        vars: HashMap::new(),
        uid: 501,
    };
    let clock = FixedClock(0);
    let waiter = dispatch::create::OkBootWaiter;
    let deps = NewDeps {
        mux: &mux,
        exec: &exec,
        env: &env,
        clock: &clock,
        paths: &paths,
        canonical_dir: PathBuf::from("/tmp/zmx-501"),
        legacy_dirs: vec![],
        boot_waiter: &waiter,
        // codex P1 W3: NewDeps carries the resolved provider (see the first site).
        // This site only exercises claims_dir() derivation; the provider is inert.
        provider: &dispatch::provider::ClaudeProvider,
        backend: dispatch::mux_selector::Backend::Zmx,
    };
    assert_eq!(
        deps.claims_dir(),
        PathBuf::from("/jail/home/.claude/claims")
    );
}
