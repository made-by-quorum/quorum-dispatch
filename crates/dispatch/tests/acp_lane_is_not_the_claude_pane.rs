//! `claude-code/acp` is a LANE of the claude-code harness, not the claude PANE —
//! a regression pin, driven through the real binary.
//!
//! # The bug this exists to keep dead
//!
//! `qd start`'s post-routing chain gated every claude-only phase on
//! `lane.harness == Harness::ClaudeCode`. That predicate was written while
//! claude-code had exactly ONE lane and it was correct for exactly that long:
//! when ACP stopped being a harness (`acp/claude-code`) and became a MODE
//! (`claude-code/acp`), the harness grew a second lane and the test started
//! admitting it. The daemon-hosted ACP resident was then walked up the mux-pane
//! launch's path.
//!
//! The load-bearing consequence, and the one asserted below: the pane lane's
//! driver route (`crate::driver::start_route`) refuses a bare agent/headless
//! start with **"agent/headless start requires -p \<prompt\>"**, because a
//! headless `claude -p ""` is a degenerate no-op turn. That sentence describes
//! nothing an ACP resident does — starting one bare is the ordinary thing an
//! agent asks for — and `opencode/acp`, which has the IDENTICAL topology, was
//! never refused. So the same request differed by harness where it should have
//! differed by lane.
//!
//! # Why the probe is shaped like this
//!
//! The predicate lives in `bin/qd/verbs/lifecycle.rs` and the route it feeds is
//! reached only after `--provider` parsing, lane resolution and the fork
//! preflight — there is no smaller pure function that carries the regression,
//! because the regression IS which lanes reach that route. So it is asked of the
//! binary, the way `start_provider_lane.rs` asks its questions.
//!
//! `--cwd <a path that does not exist>` is what keeps the probe hermetic. It sits
//! BELOW the driver route and ABOVE anything durable: with the fix, an ACP start
//! gets past the route and dies in the adapter spawn (`posix_spawn` cannot chdir),
//! which is a libc fact rather than a machine's — it does not depend on
//! `claude-code-acp` or `opencode` being installed, and it is why the two ACP
//! lanes can be compared byte-for-byte. Nothing is claimed, and
//! `assert_nothing_created` checks that rather than assuming it.
//!
//! `Command::output()` gives the child a NULL stdin, so `resolve_driver` answers
//! `Driver::Agent` — the caller this whole regression is about. The pane control
//! below proves that is really happening rather than assumed: if the child were
//! somehow resolving `Driver::Human`, the control's expected refusal would not
//! appear and the negative assertions would all pass vacuously.
//!
//! # Mutation evidence
//!
//! Restore the old predicate — `let claude_pane = lane.harness ==
//! Harness::ClaudeCode` in `lifecycle.rs::run_start` — and the two regression
//! tests below RED while the pane control stays green, which is the split that
//! says the file is measuring the lane and not the route's existence.
//!
//! # The jail is rooted SHORT on purpose
//!
//! `/tmp/qd-acplane`, not a `tempfile::tempdir()` — `qd` resolves a qrmux socket
//! dir under the jailed HOME before it parses a verb's arguments, and a Unix
//! socket path must fit 104 bytes. Same reasoning, same remedy as
//! `start_provider_lane.rs`.

mod common;

use std::path::PathBuf;
use std::process::Command;

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// The pane lane's bare-agent refusal — the exact sentence the ACP lanes must
/// never read. Matched on its stable head rather than the whole paragraph, which
/// carries re-entry advice that is free to be reworded.
const PANE_NO_PROMPT_REFUSAL: &str = "agent/headless start requires -p <prompt>";

/// A per-test jail: a HOME with an empty session registry, plus the NAME of a
/// working dir that is deliberately never created.
struct Jail {
    root: PathBuf,
    home: PathBuf,
    missing_cwd: PathBuf,
    sessions: PathBuf,
}

impl Jail {
    fn new(tag: &str) -> Jail {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = PathBuf::from("/tmp/qd-acplane").join(format!("{tag}-{nanos}"));
        let home = root.join("home");
        let sessions = home.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        common::assert_not_real_home(&home);
        Jail {
            missing_cwd: root.join("no-such-work-dir"),
            root,
            home,
            sessions,
        }
    }

    /// `qd start probe --provider <provider> [extra…] --cwd <the missing dir>`,
    /// against this jail, with a NULL stdin. Returns (exit code, stdout, stderr).
    ///
    /// `CLAUDE_BIN` points at a path that does not exist, the same belt
    /// `start_provider_lane.rs` wears: if a probe ever stops failing early and
    /// reaches a pane launch, it dies at the exec rather than booting a real
    /// agent on the developer's machine.
    fn start(&self, provider: &str, extra: &[&str]) -> (i32, String, String) {
        let cwd = self.missing_cwd.to_string_lossy().into_owned();
        let mut args: Vec<&str> = vec!["start", "probe", "--provider", provider];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--cwd", &cwd]);
        let out = Command::new(qd_bin())
            .args(args)
            .env("HOME", &self.home)
            .env("CLAUDE_BIN", "/nonexistent/claude-acp-lane-pin")
            .output()
            .expect("spawn qd");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Nothing may be created by any probe in this file. Asserted rather than
    /// assumed: a guard that moved BELOW the claim would still print the right
    /// sentence.
    fn assert_nothing_created(&self, what: &str) {
        let rows: Vec<_> = std::fs::read_dir(&self.sessions)
            .expect("read sessions dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert!(
            rows.is_empty(),
            "{what} must not claim a session; the registry holds {rows:?}"
        );
    }
}

/// Best-effort reap on `Drop` so a PANICKING test cleans up too. Only this run's
/// stamped subdir — the shared root is left for concurrently-running siblings.
impl Drop for Jail {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// THE CONTROL, and it runs first for a reason: it proves the refusal this file
/// is about is live in this build and that these probes really are resolving
/// `Driver::Agent`. Without it every assertion below would pass on a binary that
/// had simply deleted the route.
#[test]
fn a_bare_agent_start_of_the_claude_pane_is_still_refused_for_a_missing_prompt() {
    let jail = Jail::new("pane-control");
    let (code, _out, err) = jail.start("claude-code", &[]);
    assert!(
        err.contains(PANE_NO_PROMPT_REFUSAL),
        "the claude PANE lane's bare-agent refusal must be unchanged; got: {err}"
    );
    assert_eq!(code, 1, "the pane refusal exits 1; stderr was: {err}");
    jail.assert_nothing_created("a refused pane start");
}

/// The regression. Both spellings of the claude ACP lane — the legacy provider id
/// and the `--acp` flag — must get PAST the pane lane's driver route.
#[test]
fn a_bare_agent_start_of_the_claude_acp_lane_is_not_refused_for_a_missing_prompt() {
    for (label, provider, extra) in [
        ("--provider acp/claude-code", "acp/claude-code", &[][..]),
        (
            "--provider claude-code --acp",
            "claude-code",
            &["--acp"][..],
        ),
    ] {
        let jail = Jail::new("acp-bare");
        let (_code, _out, err) = jail.start(provider, extra);
        assert!(
            !err.contains(PANE_NO_PROMPT_REFUSAL),
            "{label} names a daemon-hosted ACP resident, which an agent may start \
             bare — the claude PANE lane's no-prompt refusal must not reach it. \
             stderr: {err}"
        );
        jail.assert_nothing_created(label);
    }
}

/// The positive half: the two ACP lanes are the SAME topology, so the same bare
/// agent start must land in the same place.
///
/// Asserted as byte-identical stderr, which is the strongest form the claim has
/// and the one a harness-shaped predicate cannot satisfy: the pre-fix binary
/// answered the refusal for claude and the adapter-spawn failure for opencode.
/// It is stable across machines because the missing `--cwd` fails the adapter
/// spawn itself — neither bridge program is consulted, so neither needs to be
/// installed and neither can differ.
#[test]
fn the_two_acp_lanes_answer_a_bare_agent_start_identically() {
    let claude = Jail::new("parity-claude");
    let (claude_code, _, claude_err) = claude.start("acp/claude-code", &[]);
    claude.assert_nothing_created("a failed claude-code/acp start");

    let opencode = Jail::new("parity-opencode");
    let (opencode_code, _, opencode_err) = opencode.start("opencode", &[]);
    opencode.assert_nothing_created("a failed opencode/acp start");

    assert_eq!(
        claude_err, opencode_err,
        "claude-code/acp and opencode/acp are the same lane topology; a bare agent \
         start must not be routed by harness"
    );
    assert_eq!(
        claude_code, opencode_code,
        "…and must not differ in exit code either"
    );
}
