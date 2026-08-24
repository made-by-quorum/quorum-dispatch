//! `qd start --provider <provider>/<lane>` — the LANE-NAMING argument, driven
//! through the real binary.
//!
//! `--provider` used to name a program and leave the topology to a flag. It now
//! also accepts a LANE id — `--provider codex/daemon` says both at once — and
//! every lane id `qd ls --json` prints is accepted back, so a lane copied out of
//! a listing can be pasted after the flag. Naming a lane PINS it: a topology
//! flag that asks for a different lane is refused rather than silently winning,
//! and one that agrees is a no-op.
//!
//! # Why this is its own binary
//!
//! It was written into no existing file, and each rejection is a fact about
//! this surface rather than a filing preference.
//!
//! - `start_surface_a.rs` is the lifecycle-collapse workstream-A spec rows
//!   (`--json` identity, the bind arms, name validation), and every test in it
//!   calls `require_bins()` — it cannot run without a built `qrmux` and
//!   `fakerepl` because its subject is what a start CREATES. Nothing here
//!   creates anything, and paying a mux dependency for a parse-boundary test
//!   would make a fast, hermetic file slow and machine-dependent.
//! - `p0_qafix.rs` and `p0_id_matrix.rs` are the P0 QA rulings R1/R2/R3, each
//!   test carrying a mutation-evidence note tied to that spec. The lane
//!   argument is not one of those rulings.
//! - `pi_extension_lane.rs`, `pi_interactive_lane.rs`,
//!   `codex_interactive_lane.rs` are each ONE harness's lane, end to end. The
//!   lane argument is a property of the CLI boundary that spans all four
//!   harnesses at once; putting it in any one of them would file a cross-cutting
//!   rule under a single harness and hide it from the other three.
//!
//! # What is deliberately NOT here
//!
//! The parse rule itself is unit-tested where it lives — `quorum_qw::lane`
//! (`a_lane_id_may_be_named_as_the_provider_argument`,
//! `a_named_lane_refuses_a_flag_that_contradicts_it`,
//! `every_printed_lane_id_is_accepted_as_a_provider_argument`) and
//! `bin/qd/cli.rs` (`the_help_promises_the_lane_form_and_every_printed_lane_is_
//! accepted`). Re-asserting `parse_provider_arg` here would buy nothing. What
//! only the binary can answer is whether the parse is WIRED: whether the value
//! reaches the router before anything is claimed, whether the refusals a user
//! actually reads say what they are supposed to say, and whether the ordering
//! of `run_start`'s guard chain puts each mistake in front of its own sentence
//! rather than a neighbour's.
//!
//! # How "accepted" is asserted without spawning anything
//!
//! There are no harness binaries and no credentials here, so a test that let a
//! lane id through to a real create would be testing the machine rather than
//! the argument. Every probe therefore pairs the provider with
//! `--fork nosuchsession`, and that pairing is chosen for its POSITION in
//! `run_start`: the `--provider` accept-set check fires at the top (before the
//! R20 prompt, before `provider_for`, before any lane is built), and fork-target
//! resolution fires below it against an empty registry. So the exact same
//! command line separates the two outcomes with no third possibility:
//!
//! - a value the engine cannot place → its own refusal, and fork resolution is
//!   never reached;
//! - a value it CAN place → `No session matching "nosuchsession"`, which is
//!   proof the argument got past the boundary, and proof nothing was created —
//!   target resolution writes nothing and spawns nothing.
//!
//! The negative half matters as much as the positive one: asserting only "exit
//! 1" would pass for a build that rejected every lane id, so each accepted probe
//! also asserts the stderr is NOT one of the two placement refusals.
//!
//! # The jail is rooted SHORT on purpose
//!
//! `/tmp/qd-lanearg`, not a `tempfile::tempdir()`. `qd` resolves a qrmux socket
//! dir under the jailed HOME before it parses a verb's arguments at all, and a
//! Unix socket path must fit 104 bytes — on a machine whose `$TMPDIR` is the
//! long per-session path Cargo hands out, a tempdir-rooted jail fails with
//! "socket dir … is too long" and never reaches the code under test. That
//! failure is environmental and it is not this file's subject, so the jail is
//! sited where it cannot happen.

mod common;

use std::path::PathBuf;
use std::process::Command;

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// A per-test jail: a HOME with an empty session registry, and a working dir to
/// point `--cwd` at.
///
/// Short-rooted (see the module header) and tagged + nanosecond-stamped so
/// concurrently-running tests in this binary never share a registry — an empty
/// registry is load-bearing for every fork probe below, and a neighbour's row
/// landing in it would turn "No session matching" into a resolution that
/// SUCCEEDS and goes on to spawn.
struct Jail {
    root: PathBuf,
    home: PathBuf,
    work: PathBuf,
    sessions: PathBuf,
}

impl Jail {
    fn new(tag: &str) -> Jail {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = PathBuf::from("/tmp/qd-lanearg").join(format!("{tag}-{nanos}"));
        let home = root.join("home");
        let work = root.join("work");
        let sessions = home.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        std::fs::create_dir_all(&work).expect("work dir");
        common::assert_not_real_home(&home);
        Jail {
            root,
            home,
            work,
            sessions,
        }
    }

    /// Run the REAL `qd` against this jail. Returns (exit code, stdout, stderr).
    ///
    /// `CLAUDE_BIN` points at a path that does not exist, which is the belt to
    /// the module header's braces: if one of these probes ever stops failing
    /// early and reaches a launch, it dies at the exec rather than booting a
    /// real agent on the developer's machine — and dies with a stderr none of
    /// the assertions below accept.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(qd_bin())
            .args(args)
            .env("HOME", &self.home)
            .env("CLAUDE_BIN", "/nonexistent/claude-start-provider-lane")
            .output()
            .expect("spawn qd");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// `qd start probe --provider <provider> [extra…] --cwd <jail work>`.
    fn start_with_provider(&self, provider: &str, extra: &[&str]) -> (i32, String, String) {
        let cwd = self.work.to_string_lossy().into_owned();
        let mut args: Vec<&str> = vec!["start", "probe", "--provider", provider];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--cwd", &cwd]);
        self.run(&args)
    }

    /// Nothing may be created by any probe in this file. Asserted rather than
    /// assumed: every refusal here claims to fire BEFORE the claim, and a guard
    /// that moved below the claim would still print the right sentence.
    fn assert_nothing_created(&self, what: &str) {
        let rows: Vec<_> = std::fs::read_dir(&self.sessions)
            .expect("read sessions dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert!(
            rows.is_empty(),
            "{what} must refuse before anything is claimed; the registry holds {rows:?}"
        );
    }
}

/// Reap the jail. Best-effort and on `Drop` rather than at the end of a test
/// body, so a PANICKING test cleans up too — nothing here is evidence worth
/// keeping, because every assertion prints the stderr it judged.
///
/// Only the run's own nanosecond-stamped subdir is removed. `/tmp/qd-lanearg`
/// itself stays: the six tests in this binary run concurrently under one root,
/// and removing the shared parent would race a sibling that is mid-jail.
impl Drop for Jail {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A topology token that is not a lane for ANY harness, used to make the engine
/// recite a harness's real lanes.
///
/// It has to be nonsense for all four programs at once, because the harvest
/// below asks every one of them with the same token — a token that happened to
/// be a real lane somewhere would return a session attempt instead of a list.
const NOT_A_TOPOLOGY: &str = "sideways";

/// The four programs this engine runs. Spelled out because a test may not import
/// `Harness::ALL` — this binary drives `qd` from the outside, and reaching into
/// the crate for the list would let a bug in the list hide itself.
///
/// This IS the literal the production code refuses to keep (the lane list in
/// every message below is derived from `Lane::ALL`), and here that inversion is
/// the point: an outside observer naming the programs, asking the engine what
/// lanes each has, and feeding the answers back.
const PROGRAMS: [&str; 4] = ["claude-code", "codex", "pi", "opencode"];

/// Pull the lane list out of a `"<arg>" is not a lane` refusal.
///
/// The message ends `<program>'s lanes are: a, b, c.` — one sentence, comma
/// separated, terminated by the full stop that ends the line.
fn advertised_lanes(stderr: &str) -> Vec<String> {
    let tail = stderr
        .split("lanes are: ")
        .nth(1)
        .unwrap_or_else(|| panic!("the refusal must advertise a lane list; got {stderr:?}"));
    tail.trim()
        .trim_end_matches('.')
        .split(", ")
        .map(|s| s.trim().to_string())
        .collect()
}

// ===========================================================================
// The round trip: what the engine ADVERTISES is what it ACCEPTS.
// ===========================================================================

/// Every lane the engine names in its own refusal is accepted back as a
/// `--provider` value.
///
/// This is the promise `--provider`'s help makes in prose ("Every lane id `qd ls
/// --json` prints is accepted here, so a lane copied out of a listing can be
/// pasted back"), asserted as a loop rather than trusted as a sentence.
///
/// # Why the list is harvested instead of written down
///
/// Nine lanes exist today and hard-coding them would test this file's memory,
/// not the engine's consistency. The lane list in the "is not a lane" refusal is
/// derived from `Lane::ALL`, and so is the set `parse_provider_arg` accepts; the
/// bug worth catching is the two DISAGREEING — a lane advertised to a user who
/// then cannot type it, or one the parser takes that no message ever mentions.
/// Harvesting the advertised list and feeding it back is the only shape that
/// catches the first, and it needs no edit when a tenth lane lands.
///
/// The total is asserted as a FLOOR, for the same reason: nine is what there
/// is, a tenth should be picked up automatically, and a regression that LOSES
/// one still trips the assertion. An equality would have to be edited by the
/// person adding a lane, which is exactly the maintenance the derivation exists
/// to abolish.
#[test]
fn every_lane_the_engine_advertises_is_accepted_back_as_a_provider_argument() {
    let j = Jail::new("roundtrip");
    let mut all_lanes: Vec<String> = Vec::new();

    for program in PROGRAMS {
        // Ask the engine what lanes this program has, by naming one it cannot
        // have. The refusal is the only surface that recites the list.
        let (code, _out, err) = j.start_with_provider(&format!("{program}/{NOT_A_TOPOLOGY}"), &[]);
        assert_eq!(
            code, 1,
            "a nonsense topology for {program} must refuse: {err}"
        );
        let lanes = advertised_lanes(&err);
        assert!(
            !lanes.is_empty(),
            "{program} must advertise at least one lane; got {err:?}"
        );
        for lane in &lanes {
            assert!(
                lane.starts_with(&format!("{program}/")),
                "{program}'s advertised lane {lane:?} must be spelled under {program}: {err:?}"
            );
        }
        all_lanes.extend(lanes);
    }

    assert!(
        all_lanes.len() >= 9,
        "nine lanes exist; losing one is a regression, gaining one should need no \
         edit here. Advertised: {all_lanes:?}"
    );

    for lane in &all_lanes {
        // The pairing that separates "placed" from "unplaceable" without a
        // spawn — see the module header on why `--fork` is the probe.
        let (code, _out, err) = j.start_with_provider(lane, &["--fork", "nosuchsession"]);
        assert_eq!(
            code, 1,
            "the probe fails on its fork target, not on the lane: {lane} said {err:?}"
        );
        assert!(
            err.contains("No session matching \"nosuchsession\""),
            "{lane} must be PLACED and reach fork resolution; got {err:?}"
        );
        // The two placement refusals, named explicitly. Without these the test
        // would pass for a build that rejected every lane id, because a
        // rejection also exits 1.
        assert!(
            !err.contains("is not a lane"),
            "{lane} is a lane this engine advertises — refusing it is the round trip \
             breaking in the direction the help promises: {err:?}"
        );
        assert!(
            !err.contains("unknown provider"),
            "{lane} names a program this engine supports; calling it unknown sends the \
             user to fix the wrong half: {err:?}"
        );
    }

    j.assert_nothing_created("a fork-target failure");
}

// ===========================================================================
// The two placement refusals, verbatim.
// ===========================================================================

/// A MISTYPED lane blames the topology, and lists the ones that exist.
///
/// The old refusal called `claude-code/daemon` an unknown provider, which is
/// false twice over: `claude-code` is a program this engine runs, and the
/// sentence sends the reader to doubt the half of their argument that was
/// right. The repair is not just a nicer tone — it is the difference between a
/// user re-reading the provider list (where their answer is not) and reading a
/// lane list (where it is).
///
/// Pinned VERBATIM, whole line, because every part of it is load-bearing: the
/// quoted argument echoes what they typed, the program name says which half is
/// fine, and the trailing list is the answer. `assert_eq` rather than
/// `contains` so a lane silently dropped from — or added to — a program's list
/// shows up here as a diff rather than passing a substring check.
///
/// Three arguments, three shapes of the same mistake: a topology that is real
/// but belongs to another harness (`claude-code/daemon`, `codex/acp`), and one
/// that is not a topology at all (`codex/sideways`). All three are the same
/// `Lane::from_id` miss, and it matters that they read alike — the engine cannot
/// tell "you meant another harness's lane" from "you made that word up", and
/// pretending otherwise would mean guessing.
#[test]
fn a_mistyped_lane_blames_the_topology_and_recites_the_real_ones() {
    let j = Jail::new("mistyped");

    for (arg, expected) in [
        (
            "claude-code/daemon",
            "qd start: \"claude-code/daemon\" is not a lane — claude-code has no such \
             topology. claude-code's lanes are: claude-code/mux-pane, claude-code/acp.",
        ),
        (
            "codex/acp",
            "qd start: \"codex/acp\" is not a lane — codex has no such topology. codex's \
             lanes are: codex/mux-pane, codex/daemon, codex/app-server.",
        ),
        (
            "codex/sideways",
            "qd start: \"codex/sideways\" is not a lane — codex has no such topology. \
             codex's lanes are: codex/mux-pane, codex/daemon, codex/app-server.",
        ),
    ] {
        let (code, _out, err) = j.start_with_provider(arg, &[]);
        assert_eq!(code, 1, "{arg} must exit 1; stderr: {err}");
        assert_eq!(
            err.trim(),
            expected,
            "the refusal for {arg} is pinned verbatim"
        );
    }

    j.assert_nothing_created("a mistyped lane");
}

/// An UNKNOWN PROGRAM is still an unknown provider — and now teaches the lane
/// form on its way past.
///
/// The two refusals are a partition and the test asserts both halves of it: a
/// value whose first segment names no program can only be an unknown provider,
/// however many slashes it has (`weird/daemon` is not a mistyped lane, because
/// there is no `weird` whose lanes could be listed), and a value whose first
/// segment DOES name a program is never reported this way (the test above).
///
/// The parenthetical is part of the pin. A user who typed `weird/daemon` was
/// reaching for a lane, and a refusal that lists only bare program names would
/// answer their typo while hiding the form they were actually after.
#[test]
fn an_unknown_program_is_still_an_unknown_provider_and_teaches_the_lane_form() {
    let j = Jail::new("unknown");
    let expected_tail = "— this engine supports: claude-code, codex, pi, opencode. \
                         (A lane can be named directly too, e.g. --provider codex/daemon.)";

    for arg in ["weird", "weird/daemon"] {
        let (code, _out, err) = j.start_with_provider(arg, &[]);
        assert_eq!(code, 1, "{arg} must exit 1; stderr: {err}");
        assert_eq!(
            err.trim(),
            format!("qd start: unknown provider \"{arg}\" {expected_tail}"),
            "the unknown-provider refusal for {arg} is pinned verbatim"
        );
        assert!(
            !err.contains("is not a lane"),
            "{arg} names no program, so there is no lane list to offer — the \
             topology sentence would be inventing a harness: {err:?}"
        );
    }

    j.assert_nothing_created("an unknown provider");
}

// ===========================================================================
// Naming a lane PINS it.
// ===========================================================================

/// A named lane plus a topology flag that names a DIFFERENT one is refused, not
/// resolved.
///
/// The alternative is the failure `Lane::for_create` exists to make
/// unrepresentable: a caller asks for two lanes in one command, one of them
/// silently wins, and they get a session in a topology they did not ask for with
/// exit 0. Which half won would then be an implementation detail nobody could
/// read off the command line.
///
/// The remedy sentence differs by SPELLING and both are pinned, because they are
/// giving different advice to different people. `codex/daemon` is a current way
/// to name a lane, so the advice is symmetric — drop whichever half is wrong.
/// `acp/claude-code` is the legacy spelling, so the advice also names what it has
/// become (`claude-code/acp`): a caller reading it may not know the spelling
/// moved, and telling them only "drop the flag" would leave them typing a
/// deprecated id forever.
///
/// Note which flag each legacy case carries. `acp/claude-code --interactive` is
/// NOT here, and its absence is correct rather than an oversight: that pair is
/// caught one guard earlier by the `--interactive`-needs-a-pane check, which
/// asks `Harness::supports(Mode::Pane)` of the harness rather than of the
/// spelling. `pi_interactive_lane.rs` pins that arm for `acp/opencode`. The
/// legacy rows here therefore use `--daemon`, which reaches the pin conflict.
#[test]
fn a_named_lane_pins_it_and_a_contradicting_flag_is_refused() {
    let j = Jail::new("pinned");

    // The CURRENT spelling: symmetric advice, because both halves are sayable.
    for (lane, flag) in [
        ("codex/daemon", "--interactive"),
        ("codex/daemon", "--app-server"),
        ("codex/mux-pane", "--daemon"),
        ("claude-code/acp", "--interactive"),
        ("pi/extension", "--daemon"),
    ] {
        let (code, _out, err) = j.start_with_provider(lane, &[flag]);
        assert_eq!(code, 1, "{lane} + {flag} must exit 1; stderr: {err}");
        assert_eq!(
            err.trim(),
            format!(
                "qd start: --provider {lane} already names the {lane} lane, and the \
                 topology flag you passed names a different one. Drop the flag, or name \
                 the lane you want — not both."
            ),
            "the pin conflict for {lane} + {flag} is pinned verbatim"
        );
    }

    // The LEGACY spelling: the same conflict, plus the current name for it.
    for (legacy, current) in [
        ("acp/claude-code", "claude-code/acp"),
        ("acp/opencode", "opencode/acp"),
    ] {
        let (code, _out, err) = j.start_with_provider(legacy, &["--daemon"]);
        assert_eq!(code, 1, "{legacy} + --daemon must exit 1; stderr: {err}");
        assert_eq!(
            err.trim(),
            format!(
                "qd start: --provider {legacy} already names the {current} lane, and the \
                 topology flag you passed names a different one. That spelling is the \
                 older way to say \"{current}\"; drop the flag, or name the lane you want \
                 directly."
            ),
            "the legacy pin conflict for {legacy} is pinned verbatim"
        );
    }

    j.assert_nothing_created("a pin conflict");
}

/// A topology flag that AGREES with the named lane is a no-op.
///
/// The refusal above is a conflict check, not a ban on saying the same thing
/// twice, and the distinction is worth an end-to-end row: `--provider pi/daemon
/// --daemon` and `--provider pi --daemon` are the same request, and a scripted
/// caller that acquired the lane id from a listing and kept its flag must not be
/// punished for the redundancy. A check written as "a lane id plus any topology
/// flag is an error" would pass every assertion in the test above and fail every
/// one here.
///
/// One agreeing pair per lane, which is every lane there is — the mapping from
/// lane to its flag is total, and asserting it lane-by-lane is what catches a
/// single arm of the topology chain being wired to the wrong `CreateTopology`.
/// The pairs are written out rather than harvested, because the flag a lane
/// corresponds to is precisely the fact under test and deriving it from the
/// engine would assert nothing.
///
/// The `--fork` probe means "no-op" is observed as REACHING fork resolution —
/// the same proof-of-placement the round-trip test uses, and the same guarantee
/// that no session is created.
#[test]
fn a_topology_flag_that_agrees_with_the_named_lane_is_a_no_op() {
    let j = Jail::new("agreeing");

    for (lane, flag) in [
        ("claude-code/mux-pane", "--interactive"),
        ("claude-code/acp", "--acp"),
        ("codex/mux-pane", "--interactive"),
        ("codex/daemon", "--daemon"),
        ("codex/app-server", "--app-server"),
        ("pi/mux-pane", "--interactive"),
        ("pi/daemon", "--daemon"),
        ("pi/extension", "--extension"),
        ("opencode/acp", "--acp"),
    ] {
        let (code, _out, err) = j.start_with_provider(lane, &[flag, "--fork", "nosuchsession"]);
        assert_eq!(
            code, 1,
            "{lane} + {flag} must fail on its fork target, not on the pair: {err}"
        );
        assert!(
            err.contains("No session matching \"nosuchsession\""),
            "{lane} + {flag} say the same thing twice and must be honoured: {err:?}"
        );
        assert!(
            !err.contains("already names the"),
            "{flag} is the flag form of {lane} — refusing the pair would punish a \
             caller for redundancy, not for a contradiction: {err:?}"
        );
    }

    j.assert_nothing_created("an agreeing flag");
}

/// A `--provider` that names no lane never reaches the R20 harness prompt, and
/// never reaches a claim.
///
/// The ORDERING is the assertion. `run_start` validates the typed `--provider`
/// above `resolve_provider_by_asking`, deliberately: a caller who typed a value
/// and got it wrong should fail on their own terms rather than be handed a menu
/// that silently discards what they typed — and a menu is a worse outcome than
/// an error precisely because it looks like progress.
///
/// # `--json` gets the bare line, and that is pinned rather than endorsed
///
/// A-1 gave `qd start --json` an error-object contract, and A-5's name
/// rejection honours it (`a5_rejection_under_json_emits_error_object` in
/// `start_surface_a.rs`: stdout carries `{error: {class: "start-failed", …}}`).
/// The provider accept-set check does NOT — it prints its sentence to stderr,
/// leaves stdout empty, and exits 1, because it fires above the block that
/// builds that object.
///
/// That is a real difference between two pre-create rejections and this test
/// does not pretend otherwise; it pins the shape so that closing the gap is a
/// deliberate act with a failing test to update, instead of a silent change in
/// what a machine caller parses. What IS unambiguously right here, and is the
/// reason the row exists, is that stdout stays clean: a `--json` caller must
/// never find a human sentence — or a prompt — where an object was promised.
#[test]
fn an_unplaceable_provider_is_refused_before_the_prompt_and_before_any_claim() {
    let j = Jail::new("preprompt");

    for arg in ["claude-code/daemon", "weird/daemon"] {
        let (code, out, err) = j.start_with_provider(arg, &["--json"]);
        assert_eq!(code, 1, "{arg} must exit 1 under --json; stderr: {err}");
        assert!(
            err.starts_with("qd start: "),
            "the refusal stays on stderr and keeps its attribution: {err:?}"
        );
        assert!(
            out.is_empty(),
            "a --json caller must not find a human sentence on stdout: {out:?}"
        );
        assert!(
            !err.contains("which harness should"),
            "a typed-and-wrong --provider is not a reason to offer the R20 menu: {err:?}"
        );
    }

    j.assert_nothing_created("an unplaceable provider under --json");
}
