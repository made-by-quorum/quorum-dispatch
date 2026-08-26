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
//! driver route (`crate::driver::start_route`) refuses a `--headless` start that
//! carries no prompt with **"agent/headless start requires -p \<prompt\>"**,
//! because the headless surface's whole payload was a `claude -p` turn and asking
//! for it with no prompt names a degenerate no-op turn. That sentence describes
//! nothing an ACP resident does — starting one bare is the ordinary thing an
//! agent asks for — and `opencode/acp`, which has the IDENTICAL topology, was
//! never refused. So the same request differed by harness where it should have
//! differed by lane.
//!
//! # The control moved onto `--headless` (2026-08-26, ADR-0011 addendum)
//!
//! That refusal used to also catch a BARE agent start of the pane lane, and the
//! control below was spelled as one. It is not any more: a detected agent caller
//! (env marker or pipe) now takes the pane lane's ordinary create, the same one
//! every other pane lane already gave it, and only an explicit `--headless` still
//! reads the refusal. The control therefore passes `--headless` — the flag is now
//! what summons the sentence, and summoning it is the control's entire job. The
//! ACP probes stay BARE, because bare is the shape the regression was about.
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
//! `Driver::Agent` — the caller this whole regression is about. That is no longer
//! something the control can double-check for the ACP probes, because the driver
//! no longer changes what a bare agent start of the PANE lane does; the control
//! forces the driver with `--headless` instead and proves the route's existence.
//! The ACP probes are still driven as agents, and the parity test is what keeps
//! them from passing vacuously: two lanes cannot answer byte-identically by
//! accident.
//!
//! # Mutation evidence
//!
//! Restore the old predicate — `let claude_pane = lane.harness ==
//! Harness::ClaudeCode` in `lifecycle.rs::run_start` — and the PARITY test below
//! REDs while the pane control stays green, which is the split that says the file
//! is measuring the lane and not the route's existence: under the mutation the
//! claude ACP start is walked up the pane launch's path and answers with that
//! path's failure, while `opencode/acp` still answers the adapter spawn's.
//!
//! The no-prompt test's mutation sensitivity WEAKENED on 2026-08-26 (ADR-0011
//! addendum) and this file states it rather than hiding it: since a detected
//! agent's bare pane start creates instead of refusing, a mutated binary would
//! walk the claude ACP start onto the pane path WITHOUT printing the refusal, so
//! that test alone would stay green. It is kept because the refusal is what the
//! bug actually printed and re-printing it must stay a failure; the parity test
//! carries the mutation weight now.
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

/// The pane lane's no-prompt refusal — the exact sentence the ACP lanes must
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

/// THE CONTROL, and it is written first for a reason: it proves the refusal this
/// file is about is live in this build, on this lane. Without it every assertion
/// below would pass on a binary that had simply deleted the route.
///
/// `--headless` is what makes it a control now (see the module header). The flag
/// forces `Driver::Agent` on its own, so this no longer doubles as proof that a
/// NULL stdin detects one — but the ACP probes below need only reach the route,
/// and this proves the route is still there and still answering with the sentence
/// they are asserted never to read.
#[test]
fn an_explicit_headless_start_of_the_claude_pane_is_still_refused_for_a_missing_prompt() {
    let jail = Jail::new("pane-control");
    let (code, _out, err) = jail.start("claude-code", &["--headless"]);
    assert!(
        err.contains(PANE_NO_PROMPT_REFUSAL),
        "the claude PANE lane's --headless no-prompt refusal must be unchanged; got: {err}"
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
