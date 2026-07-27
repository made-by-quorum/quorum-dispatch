//! `qd attach` — the human "get me into this session" verb — plus the retired
//! `qd connect` migration stub.
//!
//! Drives the REAL `qd` binary (`CARGO_BIN_EXE_qd`) against a JAILED, empty HOME
//! (L9a / ADD-4 — never the real home; HOME + ZMX_DIR point into a per-test
//! tempdir + an EMPTY zmx dir, so a forged claude row is necessarily COLD: it has
//! no live mux pane to attach to). Mirrors the provider_field.rs harness — forge a
//! registry row, run the bin, assert exit + stderr; no new harness invented.
//!
//! Each test carries a MUTATION-EVIDENCE comment naming the mutation it kills.

use std::path::Path;
use std::process::Command;

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// Forge a single registry row `<pid>.json` under a freshly-jailed HOME and run
/// `qd <args...>`. Returns (exit_code, stdout, stderr). HOME → `<dir>/home`,
/// ZMX_DIR → an EMPTY `<dir>/zmx` (so claude rows are cold). CODEX_HOME points at
/// an empty codex tree so a codex row resolves without a real daemon.
fn run_qd_with_row(dir: &Path, pid: i64, row_json: &str, args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    let codex_home = dir.join("codex");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(sessions.join(format!("{pid}.json")), row_json).unwrap();

    let out = Command::new(qd_bin())
        .args(args)
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run `qd <args...>` against a freshly-jailed, EMPTY HOME (no rows). Used for the
/// unknown-name resolve_or_die path + the no-arg clap error.
fn run_qd_empty(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    let out = Command::new(qd_bin())
        .args(args)
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// A codex registry row (provider+endpoint) — Hosting::Daemon. The redirect fires
// before any attach attempt, so the (unreachable) endpoint is never contacted.
fn codex_row(pid: i64, name: &str) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"019ea0b3-04d3-7400-8d95-f55d41e961e4","cwd":"/work/codexA","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"{name}","version":"0.134.0","provider":"codex","endpoint":"ws://127.0.0.1:18951"}}"#
    )
}

// A claude registry row (provider absent → claude-code default). In the empty-zmx
// jail it has no live pane → COLD.
fn claude_row(pid: i64, name: &str) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"claude-sid-{pid}","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"{name}","version":"0.1.0"}}"#
    )
}

// An AUTO-named claude row: NO registry `name`, so (with no transcript title in the
// jail) the joined row is user_named=false — exactly the case the default list-cap
// filter (named-only) drops. Resolvable only by sessionId prefix.
fn claude_row_autonamed(pid: i64) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"claude-sid-{pid}","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","version":"0.1.0"}}"#
    )
}

// === Daemon redirect (codex) ===

/// `qd attach <codex-row>` → the LOUD daemon redirect naming BOTH `qd send:relay`
/// AND `qd resume`, exit 1, and does NOT attach (mutation evidence: there is no
/// terminal-takeover, and stdout stays empty — the verb never reaches mux.attach).
///
/// MUTATION EVIDENCE: removing the `Hosting::Daemon` branch in `attach_resolved`
/// would fall through to `refuse_unknown_provider` ("unknown provider \"codex\"")
/// or attempt an attach — both red this (the redirect names send:relay+resume).
#[test]
fn attach_codex_is_daemon_redirect_not_terminal_attach() {
    let t = tempfile::tempdir().unwrap();
    let (code, out, err) =
        run_qd_with_row(t.path(), 5050, &codex_row(5050, "cx"), &["attach", "cx"]);
    assert_eq!(code, 1, "attach on a daemon-hosted row exits 1");
    assert!(
        err.contains("daemon-hosted"),
        "names the daemon hosting reason, got: {err}"
    );
    assert!(
        err.contains("qd send:relay cx"),
        "redirect points at qd send:relay, got: {err}"
    );
    assert!(
        err.contains("qd resume cx"),
        "redirect points at qd resume, got: {err}"
    );
    assert!(
        !err.contains("unknown provider"),
        "codex is supported (daemon), NOT 'unknown provider', got: {err}"
    );
    // Mutation evidence the verb did NOT attach: no interactive takeover output.
    assert!(
        out.is_empty(),
        "no attach output on the daemon-redirect path, got: {out}"
    );
}

// === connect verb ALIAS: backward-compat alias for attach, routes by provider ===

/// `qd connect <session>` is a hidden backward-compat alias for `qd attach`.
/// It resolves the session and routes by provider — a codex row produces the
/// daemon redirect (exit 1), not the old retirement stub line.
///
/// MUTATION EVIDENCE: reverting to the retirement stub reds the daemon-hosted assert.
#[test]
fn connect_verb_is_attach_alias() {
    let t = tempfile::tempdir().unwrap();
    let (code, out, err) =
        run_qd_with_row(t.path(), 5051, &codex_row(5051, "cx"), &["connect", "cx"]);
    assert_eq!(code, 1, "codex via connect alias exits 1 (daemon redirect)");
    assert!(
        err.contains("daemon-hosted"),
        "codex row via connect alias gets the daemon redirect, got: {err}"
    );
    assert!(out.is_empty(), "daemon redirect writes nothing to stdout, got: {out}");
    assert!(
        !err.contains("renamed to qd attach"),
        "retirement stub line must not appear (connect is now an alias), got: {err}"
    );
}

/// A cold claude `qd attach` attempts the
/// detached revive (resume::revive_claude) FIRST. In this jail the revive cannot
/// confirm boot (no real claude under the forged row), so it drives the real
/// run_detached + the ADR-0005 ready-wait to a genuine timeout — hence this test is
/// LIVE/SLOW (the boot waiter's default deadline is ~40-60s, with no env knob) and
/// is `#[ignore]`d in the fast lane. The load-bearing observation: attach's cold
/// path REACHES the revive machinery (its stderr carries the revive-failure line —
/// "could not launch" / "did not confirm ready" / "Failed to resume").
///
/// Run with: `cargo test -p quorum-dispatch --test attach_verb -- --ignored cold_claude_attach`.
///
/// MUTATION EVIDENCE: reverting attach to short-circuit Cold → a bare cold-error
/// (the pre-phase-2 behavior) reds the revive-attempt assert (no revive line ever
/// appears).
#[test]
#[ignore = "live/slow: drives the real boot waiter to a ~40-60s timeout"]
fn cold_claude_attach_attempts_revive_then_fails_loudly() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) =
        run_qd_with_row(t.path(), 6061, &claude_row(6061, "wk"), &["attach", "wk"]);
    assert_eq!(
        code, 1,
        "attach on a cold claude row whose revive fails exits 1"
    );
    // Evidence the revive machinery RAN.
    assert!(
        err.contains("did not confirm ready")
            || err.contains("could not launch")
            || err.contains("Failed to resume"),
        "attach: cold path attempts revive (revive-failure line expected), got: {err}"
    );
    // revive-FAILS returns the revive's own loud error; it must NOT append the
    // circular recovery pointer. The revive line above stands alone.
    assert!(
        !err.contains("revive and attach with: qd attach"),
        "attach: revive-fails does NOT append the circular cold-error pointer, got: {err}"
    );
}

/// Bug #1 regression — attach must RESOLVE an AUTO-named (user_named=false) cold
/// session, not die "No session matching". The old implementation used to pass `JoinOpts::default()`
/// (include_all=false), whose list cap keeps only user_named rows (join.rs
/// apply_list_cap), so an auto-named session was invisible to attach even though
/// resume (include_all=true) could see it — defeating attach's whole "attach OR
/// resume, you don't think about which" contract. With include_all=true (tombstones
/// stay excluded — the verb's pre-existing posture), attach resolves the row and
/// REACHES the cold→revive machinery.
///
/// FAST + deterministic: the forged row's recorded cwd `/w` does not exist, so
/// revive_claude's F3 cwd reality-check short-circuits with a clean error BEFORE the
/// slow boot waiter. That error is itself proof the revive machinery ran (it lives
/// inside the revive path, reached only AFTER resolution).
///
/// MUTATION EVIDENCE: reverting attach's opts to `JoinOpts::default()` reds this —
/// the auto-named row is cap-filtered, so attach prints `No session matching` and
/// never reaches revive.
#[test]
fn attach_resolves_auto_named_cold_session() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_with_row(
        t.path(),
        7071,
        &claude_row_autonamed(7071),
        &["attach", "claude-sid-7071"],
    );
    assert_eq!(
        code, 1,
        "revive of the forged auto-named row fails → exit 1"
    );
    assert!(
        !err.contains("No session matching"),
        "attach must RESOLVE the auto-named row, not drop it as unnamed, got: {err}"
    );
    // Evidence the revive machinery RAN (the cwd reality-check is inside revive_claude,
    // reached only after resolution succeeds).
    assert!(
        err.contains("recorded directory no longer exists")
            || err.contains("did not confirm ready")
            || err.contains("could not launch")
            || err.contains("Failed to resume"),
        "attach: reaches the revive machinery for an auto-named session, got: {err}"
    );
}

// === resolve_or_die: unknown name ===

/// An unknown session name → resolve_or_die's clear `No session matching "<q>"`
/// error, exit 1.
#[test]
fn attach_unknown_name_clear_error() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_empty(t.path(), &["attach", "nope"]);
    assert_eq!(code, 1, "unknown name exits 1");
    assert!(
        err.contains(r#"No session matching "nope""#),
        "resolve_or_die clear error, got: {err}"
    );
}

// === fail-attach-noarg: clap required-arg error ===

/// `qd attach` with no `<session>` → clap required-arg error (commander phrasing,
/// exit 1 per cli.rs's centralized mapping).
#[test]
fn attach_noarg_required_arg_error() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd_empty(t.path(), &["attach"]);
    assert_eq!(code, 1, "missing required <session> exits 1");
    assert!(
        err.contains("missing required argument 'session'"),
        "clap/commander required-arg error, got: {err}"
    );
}

// === W2: resume help documents resume as AGENT-FACING (revive-to-drivable, no tail) ===

/// `qd resume --help` now documents resume as the AGENT verb: it revives a cold
/// session to a DRIVABLE state with NO interactive attach tail (non-TTY safe), and
/// points humans wanting an interactive landing at `qd attach`.
///
/// MUTATION EVIDENCE: reverting help::RESUME to the old "Resume a dead session
/// (wraps in zmx by default)" one-liner reds the agent-facing / drivable / attach
/// asserts.
#[test]
fn resume_help_documents_agent_facing_revive_to_drivable() {
    let t = tempfile::tempdir().unwrap();
    let (code, help, _e) = run_qd_empty(t.path(), &["resume", "--help"]);
    assert_eq!(code, 0, "resume --help exits 0");
    let lc = help.to_lowercase();
    assert!(
        lc.contains("agent"),
        "names resume as agent-facing, got: {help}"
    );
    assert!(
        lc.contains("drivable"),
        "describes revive-to-drivable, got: {help}"
    );
    assert!(
        lc.contains("send:relay"),
        "points at the working send:relay channel, got: {help}"
    );
    assert!(
        help.contains("qd attach"),
        "points humans at qd attach for the interactive landing, got: {help}"
    );
    // No longer claims to merely "Resume a dead session" with no agent framing.
    assert!(
        !help.contains("Resume a dead session (wraps in zmx by default)"),
        "the bare pre-W2 one-liner is gone, got: {help}"
    );
}

// === connect retirement: absent from TOP help, --help marks it renamed ===

/// `connect` is absent from the top-level command table (`qd --help`), which now
/// carries the start/resume/attach model line; `qd connect --help` still renders
/// a migration pointer.
///
/// MUTATION EVIDENCE: re-listing connect in help::TOP reds the absence assert;
/// dropping the CONNECT-const renamed marker reds the pointer assert; dropping
/// the model line from help::TOP reds the model-line assert.
#[test]
fn connect_retired_from_top_help_and_help_marks_renamed() {
    let t = tempfile::tempdir().unwrap();

    let (code, top, _e) = run_qd_empty(t.path(), &["--help"]);
    assert_eq!(code, 0, "top help exits 0");
    assert!(
        top.contains("attach <session>"),
        "attach listed in the top help, got: {top}"
    );
    // The STATE-21 model line (spec-w7-start-surface D1).
    assert!(
        top.contains("start = new participant (fresh or forked)")
            && top.contains("resume = same participant wakes")
            && top.contains("attach = enter live or cold session"),
        "the start/resume/attach model line is in the overview, got: {top}"
    );
    // connect is gone: no `connect <session>` table row in the top help.
    assert!(
        !top.contains("connect <session>"),
        "connect is off the top-level command table, got: {top}"
    );

    let (hc, chelp, _e2) = run_qd_empty(t.path(), &["connect", "--help"]);
    assert_eq!(hc, 0, "connect --help exits 0 (still dispatchable)");
    assert!(
        chelp.contains("(renamed — use qd attach)"),
        "connect --help marks the verb renamed, got: {chelp}"
    );
}
