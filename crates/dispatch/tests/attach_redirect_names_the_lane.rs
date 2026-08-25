//! `qd attach`'s unattachable-daemon redirect must name the LANE it is refusing,
//! and point at a drive command that actually works on it. Both halves are
//! regression pins, driven through the real binary.
//!
//! # The bug this exists to keep dead
//!
//! `common::daemon_redirect` opened with the literal word **"codex"**. That was
//! true when it was written — `codex/daemon` was the only lane whose `attach`
//! answered `NotSupported` — and it stopped being true twice: once when `pi` grew
//! a daemon lane, and again when ACP became a MODE rather than a harness, which
//! made `claude-code/acp` the third. Attaching to a claude ACP session therefore
//! reported, in full, that "codex sessions are daemon-hosted". Nothing failed; the
//! sentence simply named a harness the user was not running.
//!
//! The second half is worse than wording. The redirect told the user to drive the
//! session with `qd send:relay <name> <text>`, and on a `claude-code/acp` row that
//! command FAILS: `send:relay` runs its `Management::Bare` refusal ahead of its own
//! acp arm, and an ACP row's provider is `claude-code` since the remodel, so the
//! claude-only classifier calls it bare and answers with advice about wrapping a
//! relay — for a session that has no relay and never wanted one. `qd send` is the
//! primary surface and routes on the lane (`LaneOps::deliver` picks the carrier),
//! which is exactly the question the reader of this line cannot answer themselves.
//!
//! # Which lanes are actually pinned here, and why it is three and not four
//!
//! The redirect's population is `is_daemon() && !has_viewer()` — pinned upstream by
//! `only_unattachable_daemon_lanes_refuse_attach` in `quorum_qw::lanes`. That is
//! `codex/daemon`, `pi/daemon` and `claude-code/acp`. It is NOT every daemon lane:
//! `codex/app-server` and `opencode/acp` are daemons whose residence a second
//! client can JOIN, so their `attach` succeeds and they never reach this message.
//! `codex/daemon` reaches it only WITHOUT an `endpoint` — with one, `qd attach`
//! opens a viewer (see `attach_verb.rs`). Each row below is forged accordingly.
//!
//! # Why it is asked of the binary
//!
//! The regression is a message whose content depends on lane resolution, which
//! happens after the fuzzy resolver, the tombstone rejection and the codex-viewer
//! guard. There is no smaller pure function that carries it — the wrong-harness
//! string was a *constant*, so any test of the helper alone would have been just as
//! wrong as the helper. Mirrors `attach_verb.rs`'s harness: forge a registry row
//! under a jailed HOME, run the real `qd`, assert exit + stderr.
//!
//! # Hermeticity
//!
//! The redirect is emitted BEFORE anything is contacted: `LaneOps::attach` refuses
//! on the lane's topology alone. So the forged endpoints are never dialled, no
//! harness binary is consulted, and nothing on the machine needs to be installed.
//! `ZMX_DIR` and `CODEX_HOME` point into the jail so no real pane or codex tree is
//! ever read, and `assert_nothing_created` checks that no row was claimed rather
//! than assuming it.
//!
//! # Mutation evidence
//!
//! Restore the literal — `"codex sessions are daemon-hosted …"` in
//! `verbs/common.rs` — and `the_claude_acp_redirect_does_not_say_codex` plus two
//! rows of `every_unattachable_lane_names_itself` go RED while the codex row stays
//! green, which is the split that says the message is reading the lane rather than
//! happening to contain the right word. Put `qd send:relay` back and
//! `the_redirect_points_at_the_working_drive_command` reds on its own.
//!
//! # The jail is rooted SHORT on purpose
//!
//! `/tmp/qd-attachlane`, not a `tempfile::tempdir()` — `qd` resolves a qrmux socket
//! dir under the jailed HOME before it parses a verb's arguments, and a Unix socket
//! path must fit 104 bytes. Same reasoning, same remedy as
//! `acp_lane_is_not_the_claude_pane.rs`.

use std::path::PathBuf;
use std::process::Command;

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// The phrase that states the REASON. It is shared by all three lanes and says
/// nothing about identity, which is precisely why the lane id has to be asserted
/// separately below — the pre-fix message contained this too.
const REASON: &str = "sessions are daemon-hosted (no terminal to attach)";

/// A per-test jail: a HOME with an empty session registry, an empty mux dir (so no
/// forged row can resolve a real pane) and an empty codex tree.
struct Jail {
    root: PathBuf,
    home: PathBuf,
    zmx: PathBuf,
    codex_home: PathBuf,
    sessions: PathBuf,
}

impl Jail {
    fn new(tag: &str) -> Jail {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = PathBuf::from("/tmp/qd-attachlane").join(format!("{tag}-{nanos}"));
        let home = root.join("home");
        let sessions = home.join(".claude").join("sessions");
        let zmx = root.join("zmx");
        let codex_home = root.join("codex");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        std::fs::create_dir_all(&zmx).expect("zmx dir");
        std::fs::create_dir_all(&codex_home).expect("codex home");
        Jail {
            root,
            home,
            zmx,
            codex_home,
            sessions,
        }
    }

    /// Forge one registry row, then run `qd attach <name>` against this jail.
    /// Returns (exit code, stdout, stderr).
    fn attach(&self, pid: i64, row_json: &str, name: &str) -> (i32, String, String) {
        std::fs::write(self.sessions.join(format!("{pid}.json")), row_json).expect("forge row");
        let out = Command::new(qd_bin())
            .args(["attach", name])
            .env("HOME", &self.home)
            .env("ZMX_DIR", &self.zmx)
            .env("CODEX_HOME", &self.codex_home)
            .output()
            .expect("spawn qd");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// The refusal must not have claimed anything: exactly the ONE forged row is
    /// still all there is. Asserted rather than assumed — a redirect that had
    /// slipped BELOW a create would still print the right sentence.
    fn assert_nothing_created(&self, what: &str) {
        let rows: Vec<_> = std::fs::read_dir(&self.sessions)
            .expect("read sessions dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "{what} must leave only the forged row; the registry holds {rows:?}"
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

/// A `claude-code/acp` row: the provider is the PROGRAM and the topology is the
/// `hosting` stamp. This is the spelling every ACP row written since the remodel
/// carries, and the one the old message got wrong.
fn claude_acp_row(pid: i64, name: &str) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"019ea0b3-04d3-7400-8d95-f55d41e961e4","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"{name}","version":"0.1.0","provider":"claude-code","hosting":"acp","endpoint":"ws://127.0.0.1:18999"}}"#
    )
}

/// A `pi/daemon` row — `daemon` is pi's structural default, and stamped anyway so
/// the row states its lane rather than relying on one.
fn pi_daemon_row(pid: i64, name: &str) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"019ea0b3-04d3-7400-8d95-f55d41e961f0","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"{name}","version":"0.1.0","provider":"pi","hosting":"daemon"}}"#
    )
}

/// A `codex/daemon` row with NO `endpoint` — the codex case that still reaches the
/// redirect, because there is nothing for `qd attach` to point a viewer at.
fn codex_daemon_row(pid: i64, name: &str) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"019ea0b3-04d3-7400-8d95-f55d41e961f1","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"{name}","version":"0.134.0","provider":"codex","hosting":"daemon"}}"#
    )
}

/// The three rows this file is about, each with the lane id it must name.
fn unattachable_rows() -> Vec<(&'static str, i64, String, &'static str)> {
    vec![
        (
            "claude-code/acp",
            7101,
            claude_acp_row(7101, "ccacp"),
            "ccacp",
        ),
        ("pi/daemon", 7102, pi_daemon_row(7102, "pid"), "pid"),
        ("codex/daemon", 7103, codex_daemon_row(7103, "cxd"), "cxd"),
    ]
}

/// THE regression, stated at its narrowest: a claude ACP session is not a codex
/// session and must not be described as one.
///
/// The negative is asserted against the whole of stderr rather than the first
/// token, because "codex" appearing ANYWHERE in a claude row's refusal is the bug —
/// including in a re-worded remedy line.
#[test]
fn the_claude_acp_redirect_does_not_say_codex() {
    let jail = Jail::new("acp-not-codex");
    let (code, out, err) = jail.attach(7101, &claude_acp_row(7101, "ccacp"), "ccacp");

    assert!(
        err.contains(REASON),
        "a claude-code/acp row is daemon-hosted and must get the redirect, got: {err}"
    );
    assert!(
        !err.contains("codex"),
        "the redirect for a claude ACP session must never name codex, got: {err}"
    );
    assert!(
        err.contains("claude-code/acp"),
        "…and must name its own lane, got: {err}"
    );
    assert_eq!(code, 1, "the redirect exits 1; stderr was: {err}");
    assert!(
        out.is_empty(),
        "the redirect writes nothing to stdout: {out}"
    );
    jail.assert_nothing_created("a refused attach");
}

/// Every lane that can reach the redirect identifies ITSELF, by the same stable
/// `<program>/<topology>` id `qd ls --json` puts in the `lane` key — so the string
/// a user reads is one they can paste back into `--provider`.
///
/// Asserted as the LEADING token, which is the strong form: a message that merely
/// mentioned the lane somewhere could still open by calling it something else.
/// The codex row is in the table deliberately — it is the one the old literal got
/// right, and keeping it here proves the fix reads the lane rather than having
/// swapped one hardcoded harness for another.
#[test]
fn every_unattachable_lane_names_itself() {
    for (lane_id, pid, row, name) in unattachable_rows() {
        let jail = Jail::new(&lane_id.replace('/', "-"));
        let (code, _out, err) = jail.attach(pid, &row, name);
        assert!(
            err.starts_with(&format!("{lane_id} {REASON}")),
            "a {lane_id} row must open its refusal with its own lane id, got: {err}"
        );
        assert_eq!(
            code, 1,
            "{lane_id}: the redirect exits 1; stderr was: {err}"
        );
        jail.assert_nothing_created(lane_id);
    }
}

/// The second half of the bug: the remedy must be a command that WORKS.
///
/// `qd send` is the primary, lane-routed send surface. `qd send:relay` is the
/// hidden compatibility/debug verb, and on a `claude-code/acp` row it does not just
/// read oddly — its `Management::Bare` gate refuses the row outright, because an
/// ACP row's provider is `claude-code` and the claude-only classifier finds no
/// relay. Pointing a user at that is a worse failure than the wrong harness name,
/// because they will run it and be told to go wrap a relay that has nothing to do
/// with their session.
///
/// `qd resume` is checked in the same breath: it is correct for all three lanes
/// (each has its own arm in `qd resume`'s exhaustive lane match) and must survive
/// the rewrite of the line it shares.
#[test]
fn the_redirect_points_at_the_working_drive_command() {
    for (lane_id, pid, row, name) in unattachable_rows() {
        let jail = Jail::new(&format!("remedy-{}", lane_id.replace('/', "-")));
        let (_code, _out, err) = jail.attach(pid, &row, name);
        assert!(
            err.contains(&format!("qd send {name} <text>")),
            "{lane_id}: the drive pointer is the lane-routed `qd send`, got: {err}"
        );
        assert!(
            !err.contains("send:relay"),
            "{lane_id}: `qd send:relay` is the hidden compat verb and is REFUSED \
             outright on claude-code/acp — it must not be advertised here, got: {err}"
        );
        assert!(
            err.contains(&format!("qd resume {name}")),
            "{lane_id}: the revive pointer stays `qd resume`, got: {err}"
        );
        jail.assert_nothing_created(lane_id);
    }
}
