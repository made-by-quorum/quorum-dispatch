//! REAL `qd stop` backend (P0 W1: today's `kill` renamed, qb spec-cli §11;
//! originally spec §5.1; TS `commands/lifecycle.ts:532-789`). The retired
//! `kill` verb no longer reaches this module (it errors in verbs/stubs.rs), so
//! every user-facing string here names `stop` — the verb the caller invoked.
//!
//! BOTH halves of the pane reap now live in `dispatch::kill` — the pure decision
//! core (the three-fallback zmx-target resolution) it always held, and, since
//! stage-2 phase 3, the effect sequence that drives it (`reap_pane_session`):
//!   - capture the registry entry BEFORE kill (claude self-removes its `<pid>.json`
//!     on graceful SIGTERM, so we keep a copy to synthesize a tombstone),
//!   - reap the zmx session (verify-gone 12×250ms, fail-safe loud rc=1),
//!   - reap the claude PID (SIGTERM→SIGKILL via `effects::kill_pid`),
//!   - sweep the claude pid's DESCENDANT TREE from a pre-kill snapshot, each
//!     victim (pid,start-time)-verified (punch item 8 — the grandchild gap),
//!   - `ensure_tombstone` with the captured fallback (registry primitive),
//!   - F1 env-file cleanup (best-effort),
//!   - F2/C2 post-verify scan (a survivor → exit 1 advisory; the hint uses
//!     `zmx ls`, NEVER `zmx kill` an unconfirmed target).
//!
//! It was extracted because `LaneOps::kill` had nothing to delegate to while it
//! was inlined here. What this verb kept is the part that is NOT a lane's: the
//! CLI-resolved mux/dir geometry, every rendering site (the reap answers in
//! fields, never in prints), and the dead-pid registry sweep — housekeeping over
//! ALL registry rows, not this session's lane.
//!
//! # What the lane replaced, and what it deliberately did not
//!
//! The FOUR DAEMON lanes go through `LaneOps::kill` (`stop_daemon`, at the
//! bottom). That retired a guarded four-arm provider if-chain whose ORDER was
//! load-bearing — codex+Daemon, `starts_with("acp/")`, pi+Daemon, then the
//! unknown-provider refusal wrapped in its own hosting guard — plus the three
//! near-identical verb bodies it dispatched into. The lane keys on the LANE, so
//! the arm that could be forgotten does not exist.
//!
//! The three PANE lanes deliberately do NOT, and the reason is stated in full at
//! the fall-through below: `LaneOps::kill` re-resolves the row by id and joins the
//! mux BY NAME, while the row this verb holds carries the gather's PID/ancestry
//! join — and `reap_pane_session` consumes exactly those pane coordinates. It also
//! means `dispatch::kill::PidProvenance`, which the pane reap reads off
//! `which_branch`, is byte-identically the join's on the one destructive path.
//!
//! Exits 0/1 only.

use std::path::{Path, PathBuf};

use clap::ArgMatches;

use dispatch::effects::{is_pid_alive, Env, RealClock, RealEnv};
use dispatch::exec::RealExec;
use dispatch::mux::Mux;
use dispatch::paths::QdPaths;
use dispatch::reconcile::{plan as reconcile_plan, Action};
use dispatch::registry::{read_entries, tombstone, RegistryEntry};
use dispatch::zmx_dir::{legacy_zmx_dirs, resolve_zmx_dir, XdgFamily};
use quorum_qw::contract::{LaneError, LaneOps, SessionId};
use quorum_qw::lane::{Harness, Lane};

use super::common;

/// `qd stop <session>` — the lane, then either the daemon reap or the pane
/// dual-reap. See the module docs for which goes where, and why.
pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    // W3 (ADD-15, Pete 2026-06-05): the interactive confirmation prompt is GONE —
    // kill executes directly. `--force` stays PARSE-ACCEPTED as a deprecated no-op
    // (15+ in-repo scripted callers + the QDQA battery pass it; clap's unknown-flag
    // exit-2 would mint a new failure on the most destructive verb). Safety belts
    // are non-interactive: S2 name validation + resolve_or_die's LOUD
    // ambiguous-prefix refusal (common.rs) + the unambiguous W4 success line.
    let _ = m.get_flag("force");
    // `--server` was the OpenCode-only kill flag; A-OC.1 un-parks opencode (an `acp/opencode`
    // row now GROUP-reaps via the acp/ arm below), so it is a deprecated parse-accepted no-op.
    let _ = m.get_flag("server");

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd stop: HOME is not set — cannot resolve the session state dir.");
            return 1;
        }
    };
    let paths = QdPaths::from_home(&home);

    // Resolve through the sealed uncapped entry. D-2 accept-set: stop may target a
    // tombstone (idempotent re-stop / cleanup, C-1), so no post-resolve rejection.
    // Uncapped also fixes stop's prior inability to even FIND a tombstone or an
    // out-of-cap session — it used JoinOpts::default() (20 named, no tombstones).
    let session = match common::resolve_session_uncapped(query) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let session = &session;

    // --- THE LANE, from the ROW's provider + hosting -------------------------
    //
    // ONE call, in place of the four guarded if-chains that stood here: codex+
    // Daemon, `starts_with("acp/")`, pi+Daemon, and the `row_hosting(..).is_none()`
    // wrapper around the unknown-provider refusal. Their ORDER was load-bearing and
    // enforced by comment — and getting it wrong did not merely fail, it LEAKED: the
    // daemon group-kill applied to a PANE row silently tombstones the row while the
    // pane keeps running, because the cmdline guard correctly refuses to signal a TUI
    // that has no `app-server` / `pi-daemon` on its command line. Here the lane is
    // the key, so there is no ordering to get wrong and no arm to forget.
    //
    // `None` ⇒ a genuinely unknown provider ⇒ the pre-existing loud refusal.
    // `lane_for` answers `None` for exactly the ids `row_hosting` answered `None`
    // for, so the guard the refusal used to carry is the `else` branch now.
    //
    // NAMED USER-VISIBLE CHANGE — bare `opencode`. `refuse_unknown_provider` waves
    // "opencode" through and `row_hosting("opencode", None)` answers `Daemon`, so
    // such a row used to fall PAST all four arms into the CLAUDE PANE dual-reap —
    // the mux-kill-plus-pid-kill machinery, aimed at a daemon-hosted row that has
    // no pane at all. `lane_for` accepts `opencode` as the CLI alias it is and
    // resolves it to the acp/opencode DAEMON lane. In practice these rows come from
    // the opencode store's cold scan (`join.rs`'s opencode branch: no registry
    // record, no pid), so both before and after they exit 1 having killed nothing;
    // what changes is that the sentence now describes a daemon rather than a pane.
    let Some(lane) = quorum_qw::lane_for(&session.provider, session.hosting.as_deref()) else {
        return common::refuse_unknown_provider("stop", session).unwrap_or(1);
    };

    // The four DAEMON lanes: the resident pid IS the session, and the reap is a
    // group-addressed signal ladder gated on a cmdline identity check. All three
    // near-identical verb bodies that used to live at the bottom of this file
    // (`run_codex_kill` / `run_acp_kill` / `run_pi_kill`) are gone; what is left is
    // their rendering, which is all they ever held that a library must not.
    if lane.is_daemon() {
        return stop_daemon(&env, &paths, session, lane);
    }

    // --- the three PANE lanes: dual-reap, PID-targeted, loud-on-partial (Bug A / I4) ---
    //
    // THE PANE REAP DELIBERATELY DOES NOT GO THROUGH THE LANE, and the reason is
    // the row, not the routing. [`quorum_qw::contract::LaneOps::kill`] is addressed
    // by [`SessionId`] and re-resolves it with `row_for_id`, whose pane join is
    // BY NAME (`pane.name == row.name`); the row this verb already holds got its
    // pane coordinates from the gather's join, which matches by PID and then by
    // ancestry. Those disagree whenever the pane's name is not the row's — and it
    // need not be: `derive_zmx_name` SANITISES the session name, and an unnamed row
    // gets `claude-<id8>`. `reap_pane_session` consumes exactly those coordinates
    // (`resolve_zmx_target`'s `lookup_key` is `zmx_name.or(session_name)`), so
    // handing it the weaker row would silently demote fallback 1 to the ancestry
    // fallback and stamp the success line `[zmx dir unconfirmed]`. On the one
    // DESTRUCTIVE verb that is not a trade worth making for a shorter function.
    //
    // It also keeps [`dispatch::kill::PidProvenance`] EXACTLY as it was, and that is
    // the load-bearing half: `reap_pane_session` reads `session.which_branch` to
    // choose it, `ColdJsonl`/`ZmxOnly` ⇒ `PaneDerived` ⇒ the r8 foreign gate is
    // skipped and the subtree sweep is permitted, anything else ⇒ `Registry` ⇒
    // withheld. Reaping from the row qd holds means the branch is the JOIN's, byte
    // for byte, on every row this arm sees.
    //
    // The reap itself is [`dispatch::kill::reap_pane_session`], EXTRACTED from
    // this body in stage-2 phase 3 (the `kill` step). It used to be ~450 lines
    // inlined right here — interleaved with printing and four early returns — so
    // there was nothing `LaneOps::kill` could delegate to and the three pane lanes
    // were blocked on that alone. Nothing about the sequence changed: same
    // ordering, same 12×250ms verify loop, same failure wording, same tombstone
    // step. What stays in this verb is what a library must not own — the CLI-
    // resolved mux/dir geometry, the four rendering sites below, and the dead-pid
    // registry sweep, which is housekeeping over ALL rows rather than this
    // session's lane.
    let exec = RealExec;
    // Backend-selected mux (C1 D3). ONE QD_MUX parse drives the mux AND the dir
    // set below. A bogus QD_MUX exits loudly here.
    let backend = match common::select_backend(&env) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let mux = match common::build_mux(backend, &home, &env) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let mux: &dyn Mux = mux.as_ref();
    let clock = RealClock;

    // The socket dirs to scan/kill in. Backend-keyed (C1 D2): zmx keeps the
    // canonical + cross-dir legacy scan (Bug D); embedded uses the single qrmux
    // dir (legacy EMPTY). A14-2(c): the surviving zmx READ scan honors
    // QD_TEST_SCAN_ROOTS (test lanes only; production = literal /tmp). Per the
    // A14-2 discriminator the kill TARGET is registry-known/user-named + socket-
    // addressed — NOT sourced from /tmp enumeration alone (visibility-only).
    let canonical = match backend {
        dispatch::mux_selector::Backend::Zmx => resolve_zmx_dir(&env),
        dispatch::mux_selector::Backend::Embedded => {
            match dispatch::qrmux_dir::resolve_qrmux_dir(&home, &env) {
                Ok(d) => d,
                Err(msg) => {
                    eprintln!("qd stop: {msg}");
                    return 1;
                }
            }
        }
    };
    let legacy = match backend {
        dispatch::mux_selector::Backend::Zmx => {
            let scan_roots = dispatch::zmx_dir::legacy_scan_roots(&env, Path::new("/tmp"));
            let xdg = XdgFamily::from_env(&env, env.uid());
            legacy_zmx_dirs(env.uid(), &canonical, &scan_roots, Some(&xdg))
        }
        dispatch::mux_selector::Backend::Embedded => Vec::new(),
    };
    // CANONICAL FIRST — the reap reads `dirs[0]` as the canonical dir.
    let mut dirs = vec![canonical];
    dirs.extend(legacy);

    // W3 NOTE (war story carried from the deleted confirmation block, lifecycle.ts:
    // 651-681 / old kill.rs:127-154): the TS prompt once made a non-TTY caller
    // BLOCK FOREVER on the stdin read (field-observed 2h wedge); the Rust port
    // first hardened that to a fail-closed refusal, then ADD-15 W3 removed the
    // prompt entirely ("misguided for this tool"). No code path reads stdin in
    // this verb anymore — the hang class is dead structurally, not defended.

    // `pi/extension`: capture the control socket BEFORE the reap.
    //
    // The endpoint lives on the registry ROW, not on the joined `Session`, and
    // the reap tombstones that row — so reading it afterwards is a race against
    // our own cleanup. Resolved here, unlinked below.
    let control_socket = lane.has_control_channel().then(|| {
        let endpoint = session
            .pid
            .filter(|&p| p != 0)
            .and_then(|p| dispatch::registry::read_entry(&paths.sessions_dir, p))
            .and_then(|e| e.endpoint);
        quorum_qw::provider::pi::extension::socket_for(
            &env,
            endpoint.as_deref(),
            &session.session_id,
        )
    });

    let reap = dispatch::kill::reap_pane_session(
        &dispatch::kill::PaneReapDeps {
            paths: &paths,
            env: &env,
            exec: &exec,
            clock: &clock,
            mux,
            dirs: &dirs,
        },
        session,
    );

    // Unlink the control socket the reaped session was serving.
    //
    // WHY HERE AND NOT ONLY IN THE LANE. `LaneOps::kill` does this too, for
    // callers that go through it — but this verb deliberately does NOT go
    // through the lane for a pane row (see the long note above: the lane
    // re-resolves pane coordinates by NAME, and this verb already holds the
    // stronger ones from the gather's join). So the cleanup has to exist on both
    // paths, and the duplication is the same shape, for the same reason, as the
    // codex viewer reap.
    //
    // Left behind, the socket file outlives the process that bound it and
    // `connect(2)` on it fails with ECONNREFUSED — which reads as "the channel
    // broke" when the truth is "the session is gone". The path comes from
    // `quorum_qw`, never re-derived here, so both halves address one socket.
    if let Some(sock) = &control_socket {
        quorum_qw::provider::pi::extension::install::remove_socket(sock);
    }

    // r7 OPEN-Q1: the foreign-pid note is non-fatal and must be said out loud —
    // the silent path would read as a kill. Printed FIRST, exactly where step 2
    // printed it relative to everything below.
    for note in &reap.notes {
        eprintln!("{note}");
    }

    if reap.nothing_to_kill {
        eprintln!("Session is not in zmx and has no PID. Nothing to kill.");
        return 1;
    }

    if !reap.failures.is_empty() {
        let label = session
            .name
            .clone()
            .or_else(|| reap.zmx_name.clone())
            .unwrap_or_else(|| reap.pid.to_string());
        eprintln!(
            "Failed to fully reap session \"{label}\". Could not reap: {}.",
            reap.failures.join("; ")
        );
        return 1;
    }

    // Kill-verify (F2/C2, lifecycle.ts:751-784): the reap's post-verify scan found
    // the pane we targeted STILL PRESENT. A survivor means the "success" would be
    // a silent lie — exit 1 with an advisory. The hint uses `zmx ls`, NEVER
    // `zmx kill` (don't direct the operator to kill an innocent same-named
    // session).
    if let Some(dir) = &reap.survivor_dir {
        let verify_name = reap.zmx_name.clone().unwrap_or_default();
        eprintln!(
            "WARNING: zmx session \"{verify_name}\" still exists after kill (found in {dir}). \
             Registry cleaned but zmx task was not reached. Verify with: ZMX_DIR={dir} zmx ls"
        );
        return 1;
    }

    // Dead-PID registry sweep (root-cause complement to the resolver fix): the
    // killed session's OWN <pid>.json was just tombstoned, but a session that died
    // WITHOUT graceful shutdown leaves its `<pid>.json` behind forever (only the
    // MANUAL `qd reconcile` sweeps it). Two records for one logical session — one
    // live pid + one dead-pid stale "idle" — is what caused false "Ambiguous —
    // matches 2 sessions". So after a successful kill, opportunistically tombstone
    // OTHER dead-pid registry records too.
    //
    // We REUSE the reconcile decider (the I1 invariant) rather than reimplement pid
    // liveness/tombstoning. Passing an EMPTY zmx_raw means the I3 (zmx-wrapper reap)
    // loop iterates nothing — so this can NEVER reap an attached/other zmx session
    // (I3 is the riskier path; kill stays out of it). I5 is structural inside the
    // decider: a registry pid that `is_pid_alive` returns true for is added to
    // `live_registry_pids` and NEVER appears in an action — a genuinely-alive
    // session's record is untouched. Cheap: just stat + a kill(pid,0) per row.
    let registry: Vec<RegistryEntry> = read_entries(&paths.sessions_dir, false)
        .into_iter()
        .map(|s| s.entry)
        .collect();
    let is_alive = |p: i64| is_pid_alive(p as i32);
    let sweep = reconcile_plan(&registry, &[], &is_alive);
    let mut cleaned = 0usize;
    for action in &sweep.actions {
        // Apply ONLY I1 (TombstoneDeadRegistry); ignore any I3 (none, given the
        // empty zmx_raw — but the match keeps the intent explicit and safe).
        if let Action::TombstoneDeadRegistry { pid: dead_pid, .. } = action {
            if tombstone(&paths.sessions_dir, *dead_pid) {
                cleaned += 1;
            }
        }
    }

    // W4 (ADD-15, Pete-verbatim format): ONE unambiguous success line naming all
    // three identifier namespaces — registry name, zmx name, pid — `-` for absent
    // fields. Replaces TS's namespace-ambiguous `Killed session "<label>".`
    // (lifecycle.ts label = name || zmx || pid — the reader couldn't tell WHICH).
    // ` [zmx dir unconfirmed]` keeps the zmxDirUnconfirmed honesty marker
    // (lifecycle.ts:633-637 class): success is real (wrapper verified dead) but
    // the socket-dir claim stays honest. Failure paths above are byte-UNCHANGED.
    let reg_name = session.name.clone().unwrap_or_else(|| "-".to_string());
    let zmx_label = reap.zmx_name.clone().unwrap_or_else(|| "-".to_string());
    let pid_label = if reap.pid > 0 {
        reap.pid.to_string()
    } else {
        "-".to_string()
    };
    let unconfirmed = if reap.zmx_name.is_some() && reap.zmx_dir_unconfirmed {
        " [zmx dir unconfirmed]"
    } else {
        ""
    };
    println!("killed {reg_name} (zmx {zmx_label}, pid {pid_label}){unconfirmed}");
    // Quiet by default: only a one-line summary when the sweep actually removed
    // stale dead-pid records (matches the verb's terse single-line output style).
    if cleaned > 0 {
        let plural = if cleaned == 1 { "" } else { "s" };
        println!("cleaned {cleaned} dead record{plural}");
    }
    0
}

/// The four DAEMON lanes' `qd stop`, through [`quorum_qw::contract::LaneOps::kill`].
///
/// It replaces `run_codex_kill`, `run_acp_kill` and `run_pi_kill` — three bodies
/// that differed in a noun and a bracketed clause and were otherwise the same
/// twelve lines: capture the row before the kill, group-reap it under a cmdline
/// identity guard, tombstone, print. Every one of those steps is the lane's now
/// (each arm calls the SAME delegate its verb body called), and every one of the
/// DIFFERENCES is here, where a difference in wording belongs.
///
/// What the lane could not give back, and therefore reports: `was_alive` is the
/// delegate's OWN answer, read at the instant it decided whether to signal — for
/// codex and acp that is `pid > 0 && is_pid_alive(pid) && cmdline_is_our_daemon(…)`,
/// computed inside the kill. Re-probing it here would reimplement the identity
/// guard AND race the kill it describes.
fn stop_daemon(
    env: &RealEnv,
    paths: &QdPaths,
    session: &dispatch::model::Session,
    lane: Lane,
) -> i32 {
    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    let no_pid = daemon_no_pid_clause(lane);

    let ops = dispatch::lane::open(lane, env, paths.clone());
    let report = match ops.kill(&SessionId(session.session_id.clone())) {
        Ok(r) => r,
        // The lane could not ADDRESS the row. `row_for_id` is registry-keyed —
        // tombstone-aware, so an idempotent second `qd stop` still resolves — and
        // EVERY daemon row qd can carry a pid for was written into that registry by
        // the create or the revive that spawned the resident. So what lands here is
        // a daemon-shaped row read out of a provider's own COLD STORE (`join.rs`'s
        // codex / pi / opencode cold branches, all of which carry `pid: None`):
        // there is no resident to signal and no row to tombstone, which is exactly
        // what the no-pid clause says.
        Err(LaneError::NotFound { .. }) => {
            eprintln!("qd stop: \"{name}\": {no_pid} Nothing to kill.");
            return 1;
        }
        Err(e) => {
            eprintln!("qd stop: {e}");
            return 1;
        }
    };

    if report.observed.nothing_to_kill {
        eprintln!("qd stop: \"{name}\": {no_pid} Nothing to kill.");
        return 1;
    }

    // `was_alive` is the delegate's OWN answer at the instant it decided whether to
    // signal; `None` cannot arise on a daemon arm (a pane lane never reaches this
    // function) and reads as "not observed alive", which is the conservative half.
    println!(
        "{}",
        daemon_success_line(
            lane,
            session,
            report.observed.pid,
            !report.observed.was_alive.unwrap_or(false),
        )
    );
    0
}

/// The no-pid clause, per lane. codex and acp call it a "daemon pid"; pi calls it
/// a "resident pid", because that is what pi's row records and what pi's own arm
/// has always said. Preserved as three clauses, not normalised to one: silently
/// rewriting a user-facing line during a code move is the change that goes
/// unnoticed.
fn daemon_no_pid_clause(lane: Lane) -> &'static str {
    match lane.harness {
        Harness::Codex => "codex session has no daemon pid.",
        Harness::Pi => "pi session has no resident pid.",
        // Both ACP lanes. claude-code has no daemon lane at all, so it cannot
        // reach here; giving it the acp clause rather than an `unreachable!` keeps
        // an impossible input from being a panic.
        _ => "acp session has no daemon pid.",
    }
}

/// ONE unambiguous success line (the W4 kill format shape). A daemon-hosted session
/// has no mux pane at all, so the pid IS the reaped identity.
///
/// `already_dead` is the honest edge: the resident was gone before we got there, so
/// we tombstoned the dead row (the dead-row seal) and signalled nothing.
///
/// The pi line differs from the codex/acp pair in BOTH halves — it names the row by
/// `name || sessionId` where they use `name || "-"`, and calls the pid a bare `pid`
/// where they call it a `daemon pid`. That asymmetry predates the lane and is kept
/// deliberately; see [`daemon_no_pid_clause`].
fn daemon_success_line(
    lane: Lane,
    session: &dispatch::model::Session,
    pid: i64,
    already_dead: bool,
) -> String {
    match lane.harness {
        Harness::Pi => {
            let name = session
                .name
                .clone()
                .unwrap_or_else(|| session.session_id.clone());
            if already_dead {
                format!("killed {name} (pid {pid}) [resident already dead — tombstoned]")
            } else {
                format!("killed {name} (pid {pid})")
            }
        }
        harness => {
            let reg_name = session.name.clone().unwrap_or_else(|| "-".to_string());
            let suffix = if !already_dead {
                ""
            } else if harness == Harness::Codex {
                " [daemon already dead — tombstoned]"
            } else {
                " [adapter already dead — tombstoned]"
            };
            format!("killed {reg_name} (daemon pid {pid}){suffix}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{daemon_no_pid_clause, daemon_success_line};
    use quorum_qw::lane::{Harness, Lane, Mode};

    /// The two fields these lines read, and nothing else — the rest is spelled out
    /// because `Session` has no `Default` (a session with no branch and no provider
    /// is not a session).
    fn row(name: Option<&str>) -> dispatch::model::Session {
        dispatch::model::Session {
            name: name.map(str::to_string),
            user_named: None,
            session_id: "sid-1".to_string(),
            code: None,
            qd_id: None,
            pid: Some(4242),
            status: dispatch::model::SessionStatus::Cold,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "codex".to_string(),
            entrypoint: None,
            lineage: None,
            hosting: Some("daemon".to_string()),
            which_branch: dispatch::model::SessionBranch::LiveRegistry,
        }
    }

    /// The daemon lane of `harness` — genuinely "the daemon lane of THIS harness",
    /// not "this row's lane": the fixture row above is a codex row throughout, and
    /// these tests vary the harness independently of it to walk the four daemon
    /// lanes' prose.
    ///
    /// It refuses rather than asserting a standing claim about the harness table.
    /// The `expect("every non-claude harness has a daemon lane")` this replaced was
    /// true when written and is still true today, but it was a sentence about lanes
    /// this test does not name, parked in a test that names four — so the day a
    /// harness stops having a daemon lane, the panic would have explained the wrong
    /// thing. `None` here means THIS call named a harness with no daemon lane, i.e.
    /// the test is wrong, and the message says which harness.
    fn daemon(harness: Harness) -> Lane {
        Lane::new(harness, Mode::Daemon)
            .unwrap_or_else(|| panic!("{} has no daemon lane", harness.provider_id()))
    }

    /// The same, for the ACP bridge lane. A separate helper rather than a second
    /// argument to [`daemon`], because these are two different questions: the
    /// lines below are pinned per LANE, and `claude-code` has both a lane with no
    /// daemon at all and a lane that is one.
    fn acp(harness: Harness) -> Lane {
        Lane::new(harness, Mode::Acp)
            .unwrap_or_else(|| panic!("{} has no acp lane", harness.provider_id()))
    }

    /// The four daemon lanes' `qd stop` prose, pinned BYTE FOR BYTE against what the
    /// three retired verb bodies (`run_codex_kill` / `run_acp_kill` / `run_pi_kill`)
    /// printed. The bodies collapsed into one `LaneOps::kill` call; the lines did
    /// not, and the asymmetries between them are the reason — pi names the row and
    /// the pid differently from the codex/acp pair, and each lane's already-dead
    /// clause names its OWN process ("daemon" / "adapter" / "resident").
    ///
    /// MUTATION EVIDENCE: normalise any one of the three shapes onto another — the
    /// obvious "cleanup" a collapse invites — and the matching assertion reds with
    /// the exact line the retired body used to print.
    #[test]
    fn the_daemon_stop_lines_survive_the_collapse_byte_for_byte() {
        let named = row(Some("wk"));
        // codex: name || "-", "daemon pid", "daemon already dead".
        assert_eq!(
            daemon_success_line(daemon(Harness::Codex), &named, 4242, false),
            "killed wk (daemon pid 4242)"
        );
        assert_eq!(
            daemon_success_line(daemon(Harness::Codex), &named, 4242, true),
            "killed wk (daemon pid 4242) [daemon already dead — tombstoned]"
        );
        // The ACP lanes of both harnesses: the SAME shape as codex, with the
        // adapter's own noun.
        for harness in [Harness::ClaudeCode, Harness::Opencode] {
            assert_eq!(
                daemon_success_line(acp(harness), &named, 4242, false),
                "killed wk (daemon pid 4242)"
            );
            assert_eq!(
                daemon_success_line(acp(harness), &named, 4242, true),
                "killed wk (daemon pid 4242) [adapter already dead — tombstoned]"
            );
        }
        // pi: a DIFFERENT shape — bare `pid`, and the resident's noun.
        assert_eq!(
            daemon_success_line(daemon(Harness::Pi), &named, 4242, false),
            "killed wk (pid 4242)"
        );
        assert_eq!(
            daemon_success_line(daemon(Harness::Pi), &named, 4242, true),
            "killed wk (pid 4242) [resident already dead — tombstoned]"
        );

        // The nameless row: codex/acp render `-`, pi renders the session id. Two
        // different fallbacks, and neither is a placeholder for the other.
        let anon = row(None);
        assert_eq!(
            daemon_success_line(daemon(Harness::Codex), &anon, 4242, false),
            "killed - (daemon pid 4242)"
        );
        assert_eq!(
            daemon_success_line(daemon(Harness::Pi), &anon, 4242, false),
            "killed sid-1 (pid 4242)"
        );
    }

    /// The no-pid clause, per lane — the "Nothing to kill." refusal each retired
    /// body printed. pi's names a RESIDENT pid because that is what pi's row records.
    #[test]
    fn the_no_pid_clause_names_each_lanes_own_process() {
        assert_eq!(
            daemon_no_pid_clause(daemon(Harness::Codex)),
            "codex session has no daemon pid."
        );
        assert_eq!(
            daemon_no_pid_clause(daemon(Harness::Pi)),
            "pi session has no resident pid."
        );
        for harness in [Harness::ClaudeCode, Harness::Opencode] {
            assert_eq!(
                daemon_no_pid_clause(acp(harness)),
                "acp session has no daemon pid."
            );
        }
    }
}
