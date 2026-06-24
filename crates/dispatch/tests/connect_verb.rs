//! W1 ADD-26 — `sb connect` (the human "get me into this session" verb).
//! P0 start-surface rework (STATE 22): the `attach` verb is RETIRED (erroring
//! stub, the new/kill pattern) — the old demoted-attach pins below were
//! retargeted to the stub contract; connect is the one attach-mechanic caller.
//!
//! Drives the REAL `sb` binary (`CARGO_BIN_EXE_dispatch`) against a JAILED, empty HOME
//! (L9a / ADD-4 — never the real home; HOME + ZMX_DIR point into a per-test
//! tempdir + an EMPTY zmx dir, so a forged claude row is necessarily COLD: it has
//! no live mux pane to attach to). Mirrors the provider_field.rs harness — forge a
//! registry row, run the bin, assert exit + stderr; no new harness invented.
//!
//! Each test carries a MUTATION-EVIDENCE comment naming the mutation it kills.

use std::path::Path;
use std::process::Command;

fn sb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dispatch")
}

/// Forge a single registry row `<pid>.json` under a freshly-jailed HOME and run
/// `sb <args...>`. Returns (exit_code, stdout, stderr). HOME → `<dir>/home`,
/// ZMX_DIR → an EMPTY `<dir>/zmx` (so claude rows are cold). CODEX_HOME points at
/// an empty codex tree so a codex row resolves without a real daemon.
fn run_sb_with_row(dir: &Path, pid: i64, row_json: &str, args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    let codex_home = dir.join("codex");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(sessions.join(format!("{pid}.json")), row_json).unwrap();

    let out = Command::new(sb_bin())
        .args(args)
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("spawn sb");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run `sb <args...>` against a freshly-jailed, EMPTY HOME (no rows). Used for the
/// unknown-name resolve_or_die path + the no-arg clap error.
fn run_sb_empty(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let home = dir.join("home");
    let zmx = dir.join("zmx");
    let sessions = home.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&zmx).unwrap();
    let out = Command::new(sb_bin())
        .args(args)
        .env("HOME", &home)
        .env("ZMX_DIR", &zmx)
        .output()
        .expect("spawn sb");
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

/// `sb connect <codex-row>` → the LOUD daemon redirect naming BOTH `sb send:relay`
/// AND `sb resume`, exit 1, and does NOT attach (mutation evidence: there is no
/// terminal-takeover, and stdout stays empty — the verb never reaches mux.attach).
///
/// MUTATION EVIDENCE: removing the `Hosting::Daemon` branch in `attach_resolved`
/// would fall through to `refuse_unknown_provider` ("unknown provider \"codex\"")
/// or attempt an attach — both red this (the redirect names send:relay+resume).
#[test]
fn connect_codex_is_daemon_redirect_not_attach() {
    let t = tempfile::tempdir().unwrap();
    let (code, out, err) =
        run_sb_with_row(t.path(), 5050, &codex_row(5050, "cx"), &["connect", "cx"]);
    assert_eq!(code, 1, "connect on a daemon-hosted row exits 1");
    assert!(
        err.contains("daemon-hosted"),
        "names the daemon hosting reason, got: {err}"
    );
    assert!(
        err.contains("sb send:relay cx"),
        "redirect points at sb send:relay, got: {err}"
    );
    assert!(
        err.contains("sb resume cx"),
        "redirect points at sb resume, got: {err}"
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

// === attach verb RETIRED (STATE 22): erroring stub, fires before resolution ===
// (Retired here with their history: `attach_codex_names_relay_and_resume_not_
// unknown_provider` pinned the latent codex-redirect fix THROUGH the attach verb
// — that fact stays pinned through connect above; `cold_claude_attach_emits_
// shared_cold_error` pinned the demoted-attach cold pointer — the shared
// `cold_session_error` helper died with the verb, connect's cold path
// auto-revives instead.)

/// `sb attach <anything>` is the retired erroring stub: exit 1, the pinned
/// stderr line, NO resolution/state touched — even with a perfectly attachable
/// row forged in the registry.
///
/// MUTATION EVIDENCE: re-routing dispatch back to the old run_attach reds this
/// (a codex row would print the daemon redirect, not the retired line).
#[test]
fn attach_verb_is_retired_erroring_stub() {
    let t = tempfile::tempdir().unwrap();
    let (code, out, err) =
        run_sb_with_row(t.path(), 5051, &codex_row(5051, "cx"), &["attach", "cx"]);
    assert_eq!(code, 1, "retired attach exits 1");
    assert!(
        err.contains(
            "sb attach: `attach` is retired; humans use `sb connect`, agents use `sb send:relay`"
        ),
        "the exact retired-stub line, got: {err}"
    );
    assert!(out.is_empty(), "stub writes nothing to stdout, got: {out}");
    // The stub fires at dispatch: no resolution output (no redirect, no cold
    // wording, no 'No session matching').
    assert!(
        !err.contains("daemon-hosted") && !err.contains("No session matching"),
        "stub fires before any resolution, got: {err}"
    );
}

/// W1 phase 2 — connect's cold→auto-revive DIVERGENCE from attach. A cold claude
/// `sb connect` no longer short-circuits to the pure cold-error: it ATTEMPTS the
/// detached revive (resume::revive_claude) FIRST. In this jail the revive cannot
/// confirm boot (no real claude under the forged row), so it drives the real
/// run_detached + the ADR-0005 ready-wait to a genuine timeout — hence this test is
/// LIVE/SLOW (the boot waiter's default deadline is ~40-60s, with no env knob) and
/// is `#[ignore]`d in the fast lane. The load-bearing observation: connect's cold
/// path REACHES the revive machinery (its stderr carries the revive-failure line —
/// "could not launch" / "did not confirm ready" / "Failed to resume"), which attach
/// NEVER does. Then connect appends the shared cold-error recovery pointer.
///
/// Run with: `cargo test -p dispatch --test connect_verb -- --ignored connect_cold`.
///
/// MUTATION EVIDENCE: reverting connect to short-circuit Cold → a bare cold-error
/// (the pre-phase-2 behavior) reds the revive-attempt assert (no revive line ever
/// appears).
#[test]
#[ignore = "live/slow: drives the real boot waiter to a ~40-60s timeout"]
fn cold_claude_connect_attempts_revive_then_fails_loudly() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) =
        run_sb_with_row(t.path(), 6061, &claude_row(6061, "wk"), &["connect", "wk"]);
    assert_eq!(
        code, 1,
        "connect on a cold claude row whose revive fails exits 1"
    );
    // Evidence the revive machinery RAN (attach never produces any of these).
    assert!(
        err.contains("did not confirm ready")
            || err.contains("could not launch")
            || err.contains("Failed to resume"),
        "connect: cold path attempts revive (revive-failure line expected), got: {err}"
    );
    // revive-FAILS returns the revive's own loud error; it must NOT append the
    // cold-error pointer (that says "revive with `sb connect`" — re-run the command
    // that just failed, circular on the human verb). The revive line above stands alone.
    assert!(
        !err.contains("revive and attach with: sb connect"),
        "connect: revive-fails does NOT append the circular cold-error pointer, got: {err}"
    );
}

/// Bug #1 regression — connect must RESOLVE an AUTO-named (user_named=false) cold
/// session, not die "No session matching". connect used to pass `JoinOpts::default()`
/// (include_all=false), whose list cap keeps only user_named rows (join.rs
/// apply_list_cap), so an auto-named session was invisible to connect even though
/// resume (include_all=true) could see it — defeating connect's whole "attach OR
/// resume, you don't think about which" contract. With include_all=true (tombstones
/// stay excluded — connect's pre-existing posture), connect resolves the row and
/// REACHES the cold→revive machinery.
///
/// FAST + deterministic: the forged row's recorded cwd `/w` does not exist, so
/// revive_claude's F3 cwd reality-check short-circuits with a clean error BEFORE the
/// slow boot waiter. That error is itself proof the revive machinery ran (it lives
/// inside the revive path, reached only AFTER resolution).
///
/// MUTATION EVIDENCE: reverting connect's opts to `JoinOpts::default()` reds this —
/// the auto-named row is cap-filtered, so connect prints `No session matching` and
/// never reaches revive.
#[test]
fn connect_resolves_auto_named_cold_session() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_sb_with_row(
        t.path(),
        7071,
        &claude_row_autonamed(7071),
        &["connect", "claude-sid-7071"],
    );
    assert_eq!(
        code, 1,
        "revive of the forged auto-named row fails → exit 1"
    );
    assert!(
        !err.contains("No session matching"),
        "connect must RESOLVE the auto-named row, not drop it as unnamed, got: {err}"
    );
    // Evidence the revive machinery RAN (the cwd reality-check is inside revive_claude,
    // reached only after resolution succeeds).
    assert!(
        err.contains("recorded directory no longer exists")
            || err.contains("did not confirm ready")
            || err.contains("could not launch")
            || err.contains("Failed to resume"),
        "connect: reaches the revive machinery for an auto-named session, got: {err}"
    );
}

// === resolve_or_die: unknown name ===

/// An unknown session name → resolve_or_die's clear `No session matching "<q>"`
/// error, exit 1.
#[test]
fn connect_unknown_name_clear_error() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_sb_empty(t.path(), &["connect", "nope"]);
    assert_eq!(code, 1, "unknown name exits 1");
    assert!(
        err.contains(r#"No session matching "nope""#),
        "resolve_or_die clear error, got: {err}"
    );
}

// === fail-connect-noarg: clap required-arg error ===

/// `sb connect` with no `<session>` → clap required-arg error (commander phrasing,
/// exit 1 per cli.rs's centralized mapping).
#[test]
fn connect_noarg_required_arg_error() {
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_sb_empty(t.path(), &["connect"]);
    assert_eq!(code, 1, "missing required <session> exits 1");
    assert!(
        err.contains("missing required argument 'session'"),
        "clap/commander required-arg error, got: {err}"
    );
}

// === W2: resume help documents resume as AGENT-FACING (revive-to-drivable, no tail) ===

/// `sb resume --help` now documents resume as the AGENT verb: it revives a cold
/// session to a DRIVABLE state with NO interactive attach tail (non-TTY safe), and
/// points humans wanting an interactive landing at `sb connect`.
///
/// MUTATION EVIDENCE: reverting help::RESUME to the old "Resume a dead session
/// (wraps in zmx by default)" one-liner reds the agent-facing / drivable / connect
/// asserts.
#[test]
fn resume_help_documents_agent_facing_revive_to_drivable() {
    let t = tempfile::tempdir().unwrap();
    let (code, help, _e) = run_sb_empty(t.path(), &["resume", "--help"]);
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
        help.contains("sb connect"),
        "points humans at sb connect for the interactive landing, got: {help}"
    );
    // No longer claims to merely "Resume a dead session" with no agent framing.
    assert!(
        !help.contains("Resume a dead session (wraps in zmx by default)"),
        "the bare pre-W2 one-liner is gone, got: {help}"
    );
}

// === attach retirement: absent from TOP help, --help marks it retired ===

/// `attach` is absent from the top-level command table (`sb --help`) — which now
/// carries the start/resume/connect MODEL LINE (STATE 21) — and `sb attach
/// --help` still renders, marked retired and pointing at connect.
///
/// MUTATION EVIDENCE: re-listing attach in help::TOP reds the absence assert;
/// dropping the ATTACH-const retired marker reds the pointer assert; dropping
/// the model line from help::TOP reds the model-line assert.
#[test]
fn attach_retired_from_top_help_and_help_marks_retired() {
    let t = tempfile::tempdir().unwrap();

    let (code, top, _e) = run_sb_empty(t.path(), &["--help"]);
    assert_eq!(code, 0, "top help exits 0");
    assert!(
        top.contains("connect <session>"),
        "connect listed in the top help, got: {top}"
    );
    // The STATE-21 model line (spec-w7-start-surface D1).
    assert!(
        top.contains("start = new participant (fresh or forked)")
            && top.contains("resume = same participant wakes")
            && top.contains("connect = attach-to-live"),
        "the start/resume/connect model line is in the overview, got: {top}"
    );
    // attach is gone: no `attach <session>` table row in the top help.
    assert!(
        !top.contains("attach <session>"),
        "attach is off the top-level command table, got: {top}"
    );

    let (hc, ahelp, _e2) = run_sb_empty(t.path(), &["attach", "--help"]);
    assert_eq!(hc, 0, "attach --help exits 0 (still dispatchable)");
    assert!(
        ahelp.contains("(retired — use connect)"),
        "attach --help marks the verb retired, got: {ahelp}"
    );
}
