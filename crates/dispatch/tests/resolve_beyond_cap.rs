//! §7 — resolver cap discoverability fix (C1 component spec).
//!
//! Drives the REAL `qd` binary against a JAILED HOME (L9a / ADD-4 — HOME + ZMX_DIR
//! into a per-test tempdir, never the real home). Mirrors the dupid_collision.rs /
//! connect_verb.rs harness shape: forge `<pid>.json` registry rows, run the bin,
//! assert exit + stderr. No new harness invented.
//!
//! THE INVARIANT under test: the eight ACTION verbs (connect / resume / fork /
//! stop / send:pty / send:http / send:relay / wait) resolve a `<session>` handle
//! against the FULL session universe — resolution is NEVER subject to the `ls`
//! display cap. Read verbs (info/ping/mark) and the `ls`/`live` DISPLAY cap are
//! unaffected.
//!
//! RESOLUTION ORACLE (non-destructive, per the plan: tests assert RESOLUTION, not
//! destructive execution): a verb that printed `No session matching "<q>"` FAILED
//! to resolve; any other outcome means it resolved past the cap and proceeded to
//! its verb-specific (here harmless: cold/no-pane/no-relay) follow-on.
//!
//! Each test carries a MUTATION-EVIDENCE / revert→RED comment naming what it kills.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Instant;

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// A pid that is reliably DEAD (never a running process): `is_pid_alive` → false.
const DEAD_PID: i64 = 2_147_483_646;

/// Spawn a real, short-lived child so we have a genuinely-ALIVE pid distinct from
/// the runner. `is_pid_alive` (kill(pid,0)) sees it live while the bin runs.
/// Caller kills + reaps it after the assertion.
fn live_child() -> Child {
    Command::new("sleep").arg("30").spawn().expect("spawn sleep")
}

/// Forge live `<pid>.json` rows + tombstoned `<pid>.json.tombstoned` rows under a
/// freshly-jailed HOME and run `qd <args...>`. Returns (exit, stdout, stderr).
/// CLAUDE_BIN points at a nonexistent path so a cold-revive follow-on fails loudly
/// downstream (with NON-"No session matching" stderr) instead of booting claude.
fn run_qd(
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
        .env("CLAUDE_BIN", "/nonexistent/claude-resolve-beyond-cap")
        .env_remove("QD_HOME") // pin the idstore/state dir to the jail's HOME
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A named claude row (status idle). `updated_at` drives the lastActive-desc sort
/// the display cap truncates against.
fn row(pid: i64, session_id: &str, name: &str, updated_at: i64) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":1717000000000,"updatedAt":{updated_at},"status":"idle","name":"{name}","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
    )
}

/// An AUTO-named claude row: NO registry `name`, so the joined row is
/// user_named=false — the row the default cap's named-only filter drops, and which
/// needs include_all:true (D-1), not just limit:None, to resolve by id.
fn row_autonamed(pid: i64, session_id: &str, updated_at: i64) -> String {
    format!(
        r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/w","startedAt":1717000000000,"updatedAt":{updated_at},"status":"idle","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}"#
    )
}

/// True when the verb FAILED to resolve (printed the phantom miss).
fn missed(err: &str) -> bool {
    err.contains("No session matching")
}

/// Seed the idstore so forged rows carry a real `qdId` (else `qd_id=None` and the
/// resolver's tier-a exact-id / tier-e id-prefix can never fire — the canonical
/// agent-facing handle would be untestable). Writes `mint` events to the SAME
/// `<home>/.quorum/dispatch/state/ids.jsonl` the binary's join folds + `fill_qd_ids`
/// reads, binding each `(sessionId → qdId)`. qdIds must be 8 chars of the idstore
/// ALPHABET (base36 minus 0/1/o/l). Call BEFORE `run_qd` on the same dir.
fn seed_ids(dir: &Path, binds: &[(&str, &str)]) {
    let state = dir
        .join("home")
        .join(".quorum")
        .join("dispatch")
        .join("state");
    std::fs::create_dir_all(&state).unwrap();
    let mut s = String::new();
    for (sid, qid) in binds {
        s.push_str(&format!(
            r#"{{"event":"mint","id":"{qid}","session_id":"{sid}"}}"#
        ));
        s.push('\n');
    }
    std::fs::write(state.join("ids.jsonl"), s).unwrap();
}

/// Seed `n` filler named rows that sort ABOVE `target` (larger updatedAt), so
/// `target` lands at position `n+1` — beyond the 50-cap (connect/resume/fork) AND
/// the 20-cap (stop/send/wait). Returns the full row vec including `target`.
fn rows_burying(target: (i64, String), n: usize) -> Vec<(i64, String)> {
    let mut v = Vec::with_capacity(n + 1);
    for i in 0..n {
        let pid = 70_000 + i as i64;
        // Large, strictly-descending updatedAt → all sort above target's small one.
        let updated = 1_800_000_000_000 - i as i64;
        v.push((pid, row(pid, &format!("buuid-{i:04}-4aaa-8aaa-{i:012x}"), &format!("filler{i}"), updated)));
    }
    v.push(target);
    v
}

// The over-cap target: tiny updatedAt → sorts dead last. UUID-shaped sessionId so
// the provider-UUID tier resolves it; distinctive name + name-prefix for the
// name/prefix tiers.
const TGT_PID: i64 = 60_001;
const TGT_UUID: &str = "deadbeef-0001-4aaa-8aaa-000000000001";
const TGT_NAME: &str = "needle-cold-target";
const TGT_UPDATED: i64 = 1_000_000_000_000;
// A valid 8-char idstore id for TGT (ALPHABET = base36 minus 0/1/o/l).
const TGT_QDID: &str = "tgtqd234";

fn buried_target_jail() -> Vec<(i64, String)> {
    rows_burying((TGT_PID, row(TGT_PID, TGT_UUID, TGT_NAME, TGT_UPDATED)), 60)
}

// === Red-team #1 (super13): every action verb resolves beyond the cap by name; ===
// === the SEAL is what makes this pass — revert it and every assertion goes RED. ===
//
// MUTATION EVIDENCE (revert→RED control): reverting the seal so any verb resolves
// against a capped JoinOpts { limit: Some(50/20) } again drops `needle-cold-target`
// (position 61 of 61) → that verb prints `No session matching` → `missed()` true →
// this test RED. Live in-cap resolution is unchanged (covered below), so the seal
// is the sole differentiator.
#[test]
fn every_action_verb_resolves_target_beyond_the_cap_by_name() {
    let rows = buried_target_jail();
    // (verb args, a substring its POST-resolve follow-on prints — proves it got
    // past resolution; absence of "No session matching" is the core oracle.)
    let verbs: &[&[&str]] = &[
        &["connect", TGT_NAME],
        &["resume", TGT_NAME],
        &["start", "forked-child", "--fork", TGT_NAME],
        &["stop", TGT_NAME],
        &["send:pty", TGT_NAME, "hello"],
        &["send:http", TGT_NAME, "hello"],
        &["send:relay", TGT_NAME, "hello"],
        &["wait", TGT_NAME, "--timeout", "0"],
    ];
    for args in verbs {
        let t = tempfile::tempdir().unwrap();
        let (_code, _out, err) = run_qd(t.path(), &rows, &[], args);
        assert!(
            !missed(&err),
            "verb {:?} must resolve `{TGT_NAME}` past the cap (seal); stderr: {err}",
            args
        );
    }
}

/// qdId — the CANONICAL agent-facing handle — resolves beyond the cap across ALL
/// eight action verbs. Without the idstore seed `qd_id` is None and tiers a/e never
/// fire; `seed_ids` mints a real qdId for the buried target so the resolver's
/// exact-id tier resolves it through the uncapped binary.
///
/// MUTATION EVIDENCE (revert→RED): re-capping the sealed entry drops the buried
/// target → every verb prints `No session matching "tgtqd234"` → RED.
#[test]
fn every_action_verb_resolves_target_beyond_the_cap_by_qdid() {
    let rows = buried_target_jail();
    let verbs: &[&[&str]] = &[
        &["connect", TGT_QDID],
        &["resume", TGT_QDID],
        &["start", "forked-child", "--fork", TGT_QDID],
        &["stop", TGT_QDID],
        &["send:pty", TGT_QDID, "hello"],
        &["send:http", TGT_QDID, "hello"],
        &["send:relay", TGT_QDID, "hello"],
        &["wait", TGT_QDID, "--timeout", "0"],
    ];
    for args in verbs {
        let t = tempfile::tempdir().unwrap();
        seed_ids(t.path(), &[(TGT_UUID, TGT_QDID)]);
        let (_code, _out, err) = run_qd(t.path(), &rows, &[], args);
        assert!(
            !missed(&err),
            "verb {:?} must resolve by qdId `{TGT_QDID}` past the cap; stderr: {err}",
            args
        );
    }
    // POSITIVE proof the qdId tier actually resolved THE row (guards against a
    // false-green !missed): a tombstoned target beyond the cap, addressed BY qdId,
    // must surface in connect's D-2 message carrying that very qdId.
    let tomb_qid = "tmbqd234";
    let tomb_sid = "70m6b0c0-4aaa-8aaa-aaaa-00000000000c";
    let tomb = (60_009_i64, row(60_009, tomb_sid, "qid-tomb", TGT_UPDATED));
    let fillers = rows_burying(
        (TGT_PID, row(TGT_PID, TGT_UUID, "filler-needle", TGT_UPDATED + 9)),
        60,
    );
    let t = tempfile::tempdir().unwrap();
    seed_ids(t.path(), &[(tomb_sid, tomb_qid)]);
    let (code, _out, err) = run_qd(t.path(), &fillers, &[tomb], &["connect", tomb_qid]);
    assert!(
        err.contains(tomb_qid) && err.contains("is stopped"),
        "qdId tier resolved the tombstone (D-2 echoes the qdId `{tomb_qid}`); stderr: {err}"
    );
    assert_eq!(code, 1, "tombstone reject exits 1; stderr: {err}");
}

/// Beyond-cap resolution by FULL provider UUID and by NAME-PREFIX, via send:http
/// (a verb whose post-resolve outcome is the deterministic, non-destructive
/// "not an OpenCode session" error — a clean resolution oracle).
#[test]
fn beyond_cap_resolves_by_uuid_and_by_prefix() {
    let rows = buried_target_jail();
    for handle in [TGT_UUID, "needle-cold"] {
        let t = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_qd(t.path(), &rows, &[], &["send:http", handle, "x"]);
        assert!(!missed(&err), "handle `{handle}` must resolve past the cap; stderr: {err}");
        assert!(
            err.contains("not an OpenCode session"),
            "resolved → send:http's post-resolve error; handle `{handle}`, code {code}, stderr: {err}"
        );
    }
}

/// D-1 (include_all:true): an AUTO-named cold session far beyond the cap resolves
/// by its UUID. limit:None alone would NOT suffice — the include_all=false
/// named-only filter drops auto-named rows; only include_all:true resolves it.
///
/// MUTATION EVIDENCE: flipping the sealed entry to include_all:false reds this
/// (the auto-named row is filtered out → `No session matching`).
#[test]
fn beyond_cap_resolves_auto_named_by_uuid() {
    let auto_uuid = "a570beef-0002-4aaa-8aaa-000000000002";
    let target = (60_002_i64, row_autonamed(60_002, auto_uuid, 1_000_000_000_001));
    let rows = rows_burying(target, 60);
    let t = tempfile::tempdir().unwrap();
    let (_code, _out, err) = run_qd(t.path(), &rows, &[], &["send:http", auto_uuid, "x"]);
    assert!(!missed(&err), "auto-named beyond-cap row must resolve by UUID (D-1); stderr: {err}");
    assert!(err.contains("not an OpenCode session"), "resolved; stderr: {err}");
}

/// Red-team #1 (control half): a LIVE, IN-CAP session resolves exactly as before —
/// the seal does not change normal-path resolution. Pairs with the revert→RED test
/// above: that one goes RED on revert, THIS one stays GREEN, isolating the seal as
/// the sole cause.
#[test]
fn live_in_cap_session_resolution_unchanged() {
    let mut c = live_child();
    let pid = c.id() as i64;
    let rows = [(pid, row(pid, "11ve0001-4aaa-8aaa-aaaa-000000000003", "in-cap-live", 1_799_000_000_000))];
    let t = tempfile::tempdir().unwrap();
    let (_code, _out, err) = run_qd(t.path(), &rows, &[], &["send:http", "in-cap-live", "x"]);
    let _ = c.kill();
    let _ = c.wait();
    assert!(!missed(&err), "an in-cap live session still resolves; stderr: {err}");
    assert!(err.contains("not an OpenCode session"), "resolved; stderr: {err}");
}

// === D-2: post-resolve tombstone rejection ===

/// A tombstoned target beyond the cap RESOLVES (include_tombstoned:true), then a
/// reject-set verb (connect) REJECTS it post-resolve with the clear "it is stopped
/// — resume it first" message — never the misleading `No session matching`.
///
/// MUTATION EVIDENCE: dropping the reject_if_tombstoned call → connect would try to
/// attach a tombstone; restoring include_tombstoned:false → `No session matching`.
/// Either reds this (no "is stopped" / "resume it first").
#[test]
fn tombstoned_target_rejected_with_clear_message() {
    let tomb = (60_003_i64, row(60_003, "70m6b001-4aaa-8aaa-aaaa-000000000004", "stopped-needle", TGT_UPDATED));
    let fillers = rows_burying((TGT_PID, row(TGT_PID, TGT_UUID, TGT_NAME, TGT_UPDATED + 5)), 60);
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(t.path(), &fillers, &[tomb], &["connect", "stopped-needle"]);
    assert!(!missed(&err), "a tombstone must RESOLVE (found, not phantom-missed); stderr: {err}");
    assert!(
        err.contains("is stopped") && err.contains("resume it first"),
        "D-2 clear rejection message; stderr: {err}"
    );
    assert_eq!(code, 1, "reject exits 1; stderr: {err}");
}

/// Accept-set: `stop` on an ALREADY-tombstoned row resolves it (no post-resolve
/// rejection) and is a graceful no-op on the gone pid — exit 0, no panic.
///
/// MUTATION EVIDENCE: routing stop through a path that excludes tombstones reds the
/// resolution (`No session matching`); adding stop to the reject-set reds the exit.
#[test]
fn stop_on_already_tombstoned_is_graceful() {
    let tomb = (60_004_i64, row(60_004, "70m6b002-4aaa-8aaa-aaaa-000000000005", "stop-tomb", TGT_UPDATED));
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(t.path(), &[], &[tomb], &["stop", "stop-tomb"]);
    assert!(!missed(&err), "stop must RESOLVE a tombstone (accept-set); stderr: {err}");
    assert_ne!(code, 101, "no panic on a gone pid; stderr: {err}");
    assert_eq!(code, 0, "graceful no-op on an already-stopped row; stderr: {err}");
}

// === Red-team #2 (super13): ambiguity errors cleanly, never silently picks ===

/// TWO genuinely-ALIVE same-name rows beyond the cap → loud `Ambiguous`, exit 1,
/// NEVER a silent pick. The enlarged (uncapped) candidate set must not start
/// silently choosing among real collisions.
///
/// MUTATION EVIDENCE: a resolver that picks first-match instead of erroring reds
/// this (no "Ambiguous"; the verb would proceed).
#[test]
fn ambiguous_two_live_same_name_errors_never_picks() {
    let mut c1 = live_child();
    let mut c2 = live_child();
    let p1 = c1.id() as i64;
    let p2 = c2.id() as i64;
    // Distinct ids (no dedup), SAME name, both ALIVE; buried beyond the cap.
    let dup = vec![
        (p1, row(p1, "d0110001-4aaa-8aaa-aaaa-000000000006", "twin", 1_000_000_000_002)),
        (p2, row(p2, "d0110002-4aaa-8aaa-aaaa-000000000007", "twin", 1_000_000_000_003)),
    ];
    let mut rows = rows_burying((TGT_PID, row(TGT_PID, TGT_UUID, "unused-needle", TGT_UPDATED)), 60);
    rows.extend(dup);
    let t = tempfile::tempdir().unwrap();
    let (code, _out, err) = run_qd(t.path(), &rows, &[], &["connect", "twin"]);
    let _ = c1.kill();
    let _ = c1.wait();
    let _ = c2.kill();
    let _ = c2.wait();
    assert!(err.contains("Ambiguous"), "two live same-name → loud Ambiguous; stderr: {err}");
    assert_eq!(code, 1, "ambiguity exits 1; stderr: {err}");
}

/// A LIVE row + a STALE dead-pid row sharing a name (both beyond the cap) resolve
/// to the LIVE one via the pid-aware live-over-stale refine — NOT a false
/// `Ambiguous`, NOT a phantom miss.
#[test]
fn live_over_stale_same_name_resolves_to_live() {
    let mut c = live_child();
    let live = c.id() as i64;
    let rows = vec![
        (live, row(live, "11ve0008-4aaa-8aaa-aaaa-000000000008", "halflife", 1_000_000_000_004)),
        (DEAD_PID, row(DEAD_PID, "57a1e009-4aaa-8aaa-aaaa-000000000009", "halflife", 1_000_000_000_005)),
    ];
    let t = tempfile::tempdir().unwrap();
    let (_code, _out, err) = run_qd(t.path(), &rows, &[], &["connect", "halflife"]);
    let _ = c.kill();
    let _ = c.wait();
    assert!(!err.contains("Ambiguous"), "the dead-pid stale row must NOT collide; stderr: {err}");
    assert!(!missed(&err), "the live row resolves; stderr: {err}");
}

// === Red-team #3 (super13): the intentional send:relay liveness-dedup change ===

/// send:relay previously used a FILE-LOCAL, status-only resolver (the pure
/// resolve.rs matcher). Routed through the sealed entry it now gets
/// common::resolve_or_die's PID-AWARE live-over-stale dedup. So a LIVE row + a
/// STALE dead-pid row sharing a name (no relay sidecar → the fast path misses,
/// the full-scan fallback runs) resolves to the LIVE one instead of dying
/// `Ambiguous`.
///
/// MUTATION EVIDENCE (the intentional change, noted in the commit body): restoring
/// send:relay's status-only file-local resolver counts the dead-pid row (status
/// still "idle") as live → two live matches → `Ambiguous` → this test RED.
#[test]
fn send_relay_pid_aware_dedup_resolves_live_over_stale() {
    let mut c = live_child();
    let live = c.id() as i64;
    let rows = vec![
        (live, row(live, "11ve000a-4aaa-8aaa-aaaa-00000000000a", "relaytwin", 1_000_000_000_006)),
        (DEAD_PID, row(DEAD_PID, "57a1e00b-4aaa-8aaa-aaaa-00000000000b", "relaytwin", 1_000_000_000_007)),
    ];
    let t = tempfile::tempdir().unwrap();
    let (_code, _out, err) = run_qd(t.path(), &rows, &[], &["send:relay", "relaytwin", "hi"]);
    let _ = c.kill();
    let _ = c.wait();
    assert!(
        !err.contains("Ambiguous"),
        "send:relay must gain pid-aware dedup (resolve to live, not Ambiguous); stderr: {err}"
    );
    assert!(!missed(&err), "the live relaytwin resolves; stderr: {err}");
}

// === Display cap intact (NOT regressed by the resolver change) ===

/// `ls` default stays CAPPED while `ls --all` is uncapped — the DISPLAY cap is
/// untouched by uncapping RESOLUTION.
#[test]
fn display_cap_intact_default_capped_all_uncapped() {
    // 30 named rows, all distinct updatedAt.
    let mut rows = Vec::new();
    for i in 0..30 {
        let pid = 80_000 + i as i64;
        rows.push((pid, row(pid, &format!("d15p{i:04}-4aaa-8aaa-aaaa-{i:012x}"), &format!("disp{i}"), 1_700_000_000_000 + i as i64)));
    }
    let t = tempfile::tempdir().unwrap();
    let (_c1, all_out, _e1) = run_qd(t.path(), &rows, &[], &["ls", "--all", "--json"]);
    let (_c2, def_out, _e2) = run_qd(t.path(), &rows, &[], &["ls", "--json"]);
    // Count ONLY my seeded rows (name prefix "disp") — this is a SHARED box, so the
    // real fleet's live/stray sessions leak into the process-table scan; the test
    // asserts the cap PROPERTY over my rows, not an absolute count of the host.
    let count_disp = |out: &str| -> usize {
        serde_json::from_str::<Vec<serde_json::Value>>(out)
            .map(|v| {
                v.iter()
                    .filter(|r| {
                        r.get("name")
                            .and_then(|n| n.as_str())
                            .is_some_and(|n| n.starts_with("disp"))
                    })
                    .count()
            })
            .unwrap_or(0)
    };
    let all_disp = count_disp(&all_out);
    let def_disp = count_disp(&def_out);
    // --all is UNCAPPED: every one of my 30 seeded rows surfaces.
    assert_eq!(all_disp, 30, "ls --all is uncapped (all 30 seeded rows); all_out: {all_out}");
    // default view stays CAPPED: fewer of my rows than --all, never above the 20 cap.
    assert!(def_disp < all_disp, "default truncates vs --all (def {def_disp} < all {all_disp})");
    assert!(def_disp <= 20, "default cap is <=20, got {def_disp}");
}

// === Structural guard (cheap insurance; the real seal is the type system) ===

/// No ACTION-verb source file calls the now-private slice-taking `resolve_or_die`;
/// each routes resolution through the sealed `resolve_session_uncapped*`. This is
/// belt-and-suspenders over Option B's compile-time seal (a capped JoinOpts is
/// already unexpressible in the sealed entry).
#[test]
fn structural_guard_action_verbs_route_through_sealed_entry() {
    let verbs_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/bin/qd/verbs");
    for file in ["connect.rs", "resume.rs", "lifecycle.rs", "kill.rs", "send.rs", "send_relay.rs", "wait.rs"] {
        let src = std::fs::read_to_string(verbs_dir.join(file)).unwrap();
        assert!(
            src.contains("resolve_session_uncapped"),
            "{file} must route resolution through the sealed uncapped entry"
        );
        assert!(
            !src.contains("resolve_or_die("),
            "{file} must NOT call the private slice-taking resolve_or_die (capped-slice footgun)"
        );
    }
}

// === Scale / perf benchmark (C3) — IGNORED; run against the RELEASE bin ===

/// 1,000 synthetic sessions → resolve p50 < 50ms. Records the scaling number the
/// plan must not regress (per §6, the exact-id fast-path/index is the DEFERRED
/// optimization whose trigger is THIS guard firing). Ignored by default: run with
/// `cargo test --release -- --ignored resolve_p50` so CARGO_BIN_EXE_qd is the
/// release binary (end-to-end, including the per-invocation startup a scripted loop
/// actually pays). p50 reported on stderr for the build-ready record.
#[test]
#[ignore = "perf benchmark — run with --release --ignored; gates the §6 fast-path"]
fn resolve_p50_under_50ms_at_1000_sessions() {
    let mut rows = Vec::with_capacity(1000);
    for i in 0..1000 {
        let pid = 100_000 + i as i64;
        rows.push((pid, row(pid, &format!("benc{i:04}-4aaa-8aaa-aaaa-{i:012x}"), &format!("bench{i}"), 1_700_000_000_000 + i as i64)));
    }
    // Resolve the OLDEST (bench0, smallest updatedAt) — the worst case (full gather
    // + sort + match, no fast-path). info resolves uncapped and exits 0.
    let t = tempfile::tempdir().unwrap();
    let mut samples = Vec::new();
    for _ in 0..11 {
        let start = Instant::now();
        let (code, _out, err) = run_qd(t.path(), &rows, &[], &["info", "bench0"]);
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(code, 0, "info must resolve bench0 among 1000; stderr: {err}");
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = samples[samples.len() / 2];
    eprintln!("resolve_p50_at_1000_sessions = {p50:.1} ms (end-to-end, release bin)");
    assert!(p50 < 50.0, "resolve p50 must be < 50ms at 1000 sessions, got {p50:.1}ms");
}
