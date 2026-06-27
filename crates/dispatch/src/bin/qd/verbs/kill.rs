//! REAL `qd stop` backend (P0 W1: today's `kill` renamed, qb spec-cli §11;
//! originally spec §5.1; TS `commands/lifecycle.ts:532-789`). The retired
//! `kill` verb no longer reaches this module (it errors in verbs/stubs.rs), so
//! every user-facing string here names `stop` — the verb the caller invoked.
//!
//! The decision core (the three-fallback zmx-target resolution) lives in the pure
//! `dispatch::kill` module; this verb drives the live effects around the plan:
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
//! Exits 0/1 only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::ArgMatches;

use dispatch::effects::{
    find_zmx_wrapper_for_pid, is_pid_alive, kill_pid, Clock, Env, RealClock, RealEnv,
    RealProcessTable,
};
use dispatch::exec::RealExec;
use dispatch::join::JoinOpts;
use dispatch::kill::{have_kill_target, resolve_zmx_target};
use dispatch::launch::remove_session_env_file;
use dispatch::mux::{Mux, MuxSession};
use dispatch::paths::SbPaths;
use dispatch::reconcile::{plan as reconcile_plan, Action};
use dispatch::registry::{ensure_tombstone, read_entries, read_entry, tombstone, RegistryEntry};
use dispatch::zmx_dir::{legacy_zmx_dirs, resolve_zmx_dir, XdgFamily};

use super::common;
use super::common::resolve_or_die;

/// `qd stop <session>` — provider split, then the claude/zmx dual-reap core.
pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    // W3 (ADD-15, Pete 2026-06-05): the interactive confirmation prompt is GONE —
    // kill executes directly. `--force` stays PARSE-ACCEPTED as a deprecated no-op
    // (15+ in-repo scripted callers + the SBQA battery pass it; clap's unknown-flag
    // exit-2 would mint a new failure on the most destructive verb). Safety belts
    // are non-interactive: S2 name validation + resolve_or_die's LOUD
    // ambiguous-prefix refusal (common.rs) + the unambiguous W4 success line.
    let _ = m.get_flag("force");
    let server = m.get_flag("server");

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd stop: HOME is not set — cannot resolve the session state dir.");
            return 1;
        }
    };
    let paths = SbPaths::from_home(&home);

    let sessions = match common::all_sessions(JoinOpts::default()) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let session = match resolve_or_die(query, &sessions) {
        Ok(s) => s,
        Err(code) => return code,
    };

    // OpenCode kill (lifecycle.ts:542-577) — parked: the A1 join design only ever
    // constructs provider:"claude-code" rows (see model.rs:92 + join.rs construction
    // sites), so this branch is structurally unreachable until an OC-join lands.
    // Kept structurally honest (ADD-9a), mirroring run_attach's parked OC branch.
    // NOTE: spec §2 carries no OC exclusion — the real ground is the A1 join design.
    if session.provider == "opencode" {
        eprintln!("qd stop: OpenCode kill is not supported in the Rust engine (parked).");
        let _ = server; // --server is the OC-only flag; parked with the branch.
        return 1;
    }
    // codex P2 W7 (codex-p2-spec §7.6 kill paragraph): a codex row is DAEMON-hosted —
    // the daemon pid IS the session. Reap it GROUP-addressed (the W4
    // RealDaemonSpawner::kill pgid ladder — the launcher ignores SIGTERM + a
    // launcher-only SIGKILL orphans the native child, so GROUP is mandatory) → then
    // ensure_tombstone with the captured row (carries provider + endpoint). NO
    // zmx/mux reap (a codex row has no pane). THIN dispatch branch → the NEW
    // resume_daemon::kill_codex; RETURNS before any claude zmx/pid dual-reap. Placed
    // BEFORE refuse_unknown_provider (which would otherwise refuse "codex").
    if session.provider == "codex" {
        return run_codex_kill(&paths, session);
    }
    // scoped-ACP-CC (F3 / rider-4 kill-no-leak): an acp/* row is daemon-hosted — the
    // resident adapter pid IS the session. GROUP-reap it (the adapter + its bridge child
    // together) via the SAME proven group ladder as codex. Placed BEFORE
    // refuse_unknown_provider (which would otherwise refuse acp and leave both procs alive).
    if session.provider.starts_with("acp/") {
        return run_acp_kill(&paths, session);
    }
    // codex P1, R1 (codex-p1-spec section 2.3): refuse an unknown provider LOUDLY.
    if let Some(code) = common::refuse_unknown_provider("stop", session) {
        return code;
    }

    // --- claude-code kill: dual-reap, PID-targeted, loud-on-partial (Bug A / I4) ---
    let exec = RealExec;
    // Backend-selected mux (C1 D3). ONE SB_MUX parse drives the mux AND the dir
    // set below. A bogus SB_MUX exits loudly here.
    let backend = match common::select_backend(&env) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let mux = match common::build_mux(backend, &home, &env) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let mux: &dyn Mux = mux.as_ref();
    let pt = RealProcessTable::new(exec);
    let clock = RealClock;

    let pid = session.pid.unwrap_or(0);

    // The socket dirs to scan/kill in. Backend-keyed (C1 D2): zmx keeps the
    // canonical + cross-dir legacy scan (Bug D); embedded uses the single qrmux
    // dir (legacy EMPTY). A14-2(c): the surviving zmx READ scan honors
    // SB_TEST_SCAN_ROOTS (test lanes only; production = literal /tmp). Per the
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
    let mut dirs = vec![canonical.clone()];
    dirs.extend(legacy);

    let canonical_str = canonical.to_string_lossy().into_owned();

    // Ancestry wrapper for the claude PID (red-team #1) — the wrapper is a real
    // ANCESTOR even when its socket lives in an unscanned dir. Walk the full
    // ps `{ppid,cmd}` rows (the wrapper is a bash/zmx process, not a claude one,
    // so claude_procs alone is insufficient). The SAME rows feed the r7
    // ownership chain (clause (b) of the resolver's confirmed-target gate).
    let proc_rows = if pid > 0 { ps_rows(&pt) } else { None };
    let wrapper = proc_rows
        .as_ref()
        .and_then(|rows| find_zmx_wrapper_for_pid(pid as i32, rows))
        .map(|w| (w.wrapper_pid, w.zmx_name));
    let owner_chain: Vec<i32> = proc_rows
        .as_ref()
        .map(|rows| dispatch::effects::ancestor_chain(pid as i32, rows))
        .unwrap_or_default();

    // Gather the FILTERED + RAW cross-dir lists for the by-name fallbacks.
    let live_list = scan_all_dirs(mux, &dirs, false);
    let raw_list = scan_all_dirs(mux, &dirs, true);

    // r8 (identity-first): establish what the recorded pid IS before deriving
    // any ownership witness from it. A Registry-sourced pid (LiveRegistry /
    // Tombstoned rows) is a historical claim with a reuse window — alive but
    // not the session's program ⇒ FOREIGN ⇒ no clause-(a) pid, no wrapper, no
    // ancestor chain (a reused pid's ancestry otherwise reaches a live
    // victim's pane). A pane-derived pid (ColdJsonl adoption / ZmxOnly) is
    // same-invocation pane truth and exempt.
    let provenance = match session.which_branch {
        dispatch::model::SessionBranch::LiveRegistry
        | dispatch::model::SessionBranch::Tombstoned => dispatch::kill::PidProvenance::Registry,
        dispatch::model::SessionBranch::ColdJsonl | dispatch::model::SessionBranch::ZmxOnly => {
            dispatch::kill::PidProvenance::PaneDerived
        }
    };
    // The session program may be overridden (CLAUDE_BIN — test SUTs, alt
    // installs); a legit recorded pid then shows THAT cmdline, not "claude".
    let session_bin = dispatch::launch::claude_bin(&env);
    let pid_alive_now = pid > 0 && is_pid_alive(pid as i32);
    let pid_cmdline = if pid_alive_now {
        dispatch::create_daemon::real_cmdline_probe(pid)
    } else {
        None
    };
    // The start-time arm: (pid, start-time) identity survives `exec` and is
    // program-agnostic — the sound half of the foreign check (the cmdline
    // arm is the belt; see kill::pid_is_foreign).
    let pid_started = if pid_alive_now {
        dispatch::effects::proc_start_ms(pid as i32)
    } else {
        None
    };
    let (ownership_pid, wrapper, owner_chain, pid_foreign) = dispatch::kill::gate_pid_witnesses(
        provenance,
        pid,
        pid_alive_now,
        pid_cmdline.as_deref(),
        &session_bin,
        pid_started,
        session.started_at_ms,
        wrapper,
        owner_chain,
    );

    // punch item 8 (b3-kill-spec): the descendant-tree snapshot for the
    // grandchild sweep — taken BEFORE any kill while the tree is intact (the
    // pane teardown reparents survivors to init and erases the ppid evidence).
    //
    // SWEEP-ROOT GATE (b3 adversarial concern 1): the lone-pid kill gate
    // (!pid_foreign) is NOT strong enough to root a SUBTREE sweep. pid_foreign
    // is false when the cmdline read failed (the adjudicated "probably died in
    // the probe window" cell) — but a reused registry pid now held by a
    // STRANGER, whose cmdline probe happened to fail, would otherwise root a
    // sweep over the STRANGER's children, and every per-victim recheck would
    // PASS (those victims genuinely are the stranger's descendants). So the
    // SWEEP requires a POSITIVE root witness (cmdline-matched-ours OR
    // confirmed-started-ours; a confirmed start MISMATCH is a hard veto) via
    // sweep_root_allowed. When the only evidence is "alive but
    // unidentifiable", the lone root-kill below still runs (r7/r8 unchanged) —
    // only the subtree sweep is withheld (grandchildren leak = accepted leak
    // direction). PaneDerived provenance is same-invocation pane truth, never
    // a reuse window — it roots without the registry-row witness.
    let sweep_root_ok = pid > 0
        && pid_alive_now
        && !pid_foreign
        && (provenance == dispatch::kill::PidProvenance::PaneDerived
            || dispatch::kill::sweep_root_allowed(
                pid_cmdline.as_deref(),
                &session_bin,
                pid_started,
                session.started_at_ms,
            ));
    // Each victim is stamped with its current (pid, start-time) at STAMP TIME
    // — the identity anchor runs forward from here, NOT back to the snapshot
    // (the ~1-3s scan gap means a victim could in principle have been replaced
    // between snapshot and stamp; the forward anchor + the kill_pid_tree
    // recheck close the window from stamp time on). descendant_kill_list
    // returns EMPTY for self-stop (qd inside the session's own tree — never
    // sweep toward the caller; the L10 PID-only lesson).
    let descendant_victims: Vec<(i32, i64)> = if sweep_root_ok {
        proc_rows
            .as_ref()
            .map(|rows| {
                dispatch::kill::descendant_kill_list(pid as i32, std::process::id() as i32, rows)
                    .into_iter()
                    .filter_map(|p| dispatch::effects::proc_start_ms(p).map(|s| (p, s)))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let pid_alive_probe = |p: i32| is_pid_alive(p);
    let mut target = resolve_zmx_target(
        session.zmx_name.as_deref(),
        session.name.as_deref(),
        ownership_pid,
        &canonical_str,
        &live_list,
        &raw_list,
        wrapper,
        &owner_chain,
        &pid_alive_probe,
    );

    // r7 OPEN-Q1 (fallback-3 belt): an ended task's recorded wrapper pid is
    // mint-time data with a real reuse window. Only signal it when its CURRENT
    // cmdline parses as a zmx wrapper for this name; otherwise it is either a
    // reused stranger (never signal) or already gone (nothing to signal) — in
    // both cases the wrapper slot is cleared and the rest of the reap proceeds.
    // The ancestry wrapper (zmx_dir_unconfirmed) needs no guard: its pid came
    // from a fresh `ps` walk moments ago, cmdline-matched by construction.
    if target.wrapper_pid > 0 && !target.zmx_dir_unconfirmed {
        let cmdline = dispatch::create_daemon::real_cmdline_probe(i64::from(target.wrapper_pid));
        let name = target.zmx_name.as_deref().unwrap_or("");
        if !dispatch::kill::wrapper_kill_allowed(cmdline.as_deref(), name) {
            target.wrapper_pid = 0;
        }
    }

    let have_zmx = target.zmx_name.is_some();
    let have_pid = pid > 0;

    // The two verify belts below must judge "still there" identically: a pane
    // counts only at the dir the kill was pinned to (an ownership-refused
    // same-name pane in another dir is someone else's). socket_dir-less rows
    // stay conservative-loud; an unconfirmed dir falls back to name-only.
    let at_target_dir = |z: &MuxSession| {
        target.zmx_dir_unconfirmed
            || z.socket_dir.is_none()
            || z.socket_dir.as_deref() == target.zmx_dir.as_deref()
    };

    if !have_kill_target(&target, pid) {
        eprintln!("Session is not in zmx and has no PID. Nothing to kill.");
        return 1;
    }

    // W3 NOTE (war story carried from the deleted confirmation block, lifecycle.ts:
    // 651-681 / old kill.rs:127-154): the TS prompt once made a non-TTY caller
    // BLOCK FOREVER on the stdin read (field-observed 2h wedge); the Rust port
    // first hardened that to a fail-closed refusal, then ADD-15 W3 removed the
    // prompt entirely ("misguided for this tool"). No code path reads stdin in
    // this verb anymore — the hang class is dead structurally, not defended.

    let mut failures: Vec<String> = Vec::new();

    // Capture the registry entry BEFORE killing (lifecycle.ts:684-687) — claude
    // removes its own <pid>.json on graceful SIGTERM.
    let captured: Option<RegistryEntry> = if pid > 0 {
        read_entry(&paths.sessions_dir, pid)
    } else {
        None
    };

    // 1. Reap the zmx session (lifecycle.ts:689-739).
    if have_zmx {
        let zmx_name = target.zmx_name.clone().unwrap();
        let mut code = 0;
        if !target.zmx_dir_unconfirmed {
            if let Some(dir) = &target.zmx_dir {
                code = mux.kill(Path::new(dir), &zmx_name).unwrap_or(1);
            }
        }
        if target.wrapper_pid > 0 {
            kill_pid(target.wrapper_pid, dispatch::effects::KILL_GRACE_MS);
        }
        // Verify gone: 12×250ms (~3s), fail-safe loud rc=1. A loaded host clears
        // the dying socket slower; the worst case is a loud "possible leak", never
        // a silent desync. When the dir was unconfirmed we additionally require the
        // wrapper PID to be dead (an unscanned-dir leak would otherwise be invisible).
        let mut still_there = true;
        for _ in 0..12 {
            // r7: verify at the dir the kill was pinned to (see at_target_dir).
            let in_scanned = scan_all_dirs(mux, &dirs, false)
                .iter()
                .any(|z| z.name == zmx_name && at_target_dir(z));
            let wrapper_alive = target.wrapper_pid > 0 && is_pid_alive(target.wrapper_pid);
            still_there = in_scanned || wrapper_alive;
            if !still_there {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if still_there {
            let wrapper_still_alive = target.wrapper_pid > 0 && is_pid_alive(target.wrapper_pid);
            failures.push(if wrapper_still_alive {
                format!(
                    "zmx wrapper \"{zmx_name}\" (pid {} still alive — leak)",
                    target.wrapper_pid
                )
            } else if target.zmx_dir_unconfirmed {
                format!(
                    "zmx wrapper \"{zmx_name}\" (pid {}, socket dir unknown — possible leak)",
                    target.wrapper_pid
                )
            } else {
                format!("zmx session \"{zmx_name}\" (zmx kill exit {code})")
            });
        }
    }

    // 2. Reap the claude PID directly — never rely on zmx kill alone (Bug A).
    //    r7 OPEN-Q1: unless the pid is FOREIGN (reused by a process the
    //    cmdline cannot identify as the session program) — then the stranger
    //    is never signaled; say so (the silent path would read as a kill).
    //    Liveness is RE-CHECKED here: a mux-corroborated pane kill in step 1
    //    may already have reaped the process (the clause-(a) path where the
    //    cmdline could not identify it) — then there is nothing to skip and
    //    nothing to announce.
    if have_pid {
        if pid_foreign && is_pid_alive(pid as i32) {
            eprintln!(
                "note: pid {pid} now belongs to a different process (not the session's \
                 program) — not signaled; the dead session's record is tombstoned."
            );
        } else if !pid_foreign {
            let dead = kill_pid(pid as i32, dispatch::effects::KILL_GRACE_MS);
            if !dead {
                failures.push(format!("claude process PID {pid}"));
            }
        }
    }

    // 2b. punch item 8: the descendant sweep — the grandchild-survival gap
    //     (P2 panel-unanimous). claude's graceful exit does not reliably reap
    //     its children's children; sweep the pre-kill snapshot bottom-up.
    //     Every signal inside kill_pid_tree is (pid, start-time)-re-verified
    //     (a reused pid is never signaled); a survivor that still IS the
    //     snapshot process is a loud leak, never a silent one. Empty for
    //     foreign/dead/self-stop/no-positive-witness roots (see the snapshot
    //     site above).
    //
    //     KNOWN SHAPE (b3 adversarial concern 4, defensible — documented not
    //     fixed): a session B being COMMISSIONED from inside session A (an
    //     embedded `qd start B` running as a descendant of A's claude) at the
    //     instant `qd stop A` snapshots is a true descendant of A, so its
    //     nascent process is swept. This is `qd stop A` stopping A's whole
    //     subtree, which includes the in-flight commission it spawned — the
    //     intended semantics of stopping the commissioner; the window closes
    //     when `qd start B` exits and B reparents off A.
    if !descendant_victims.is_empty() {
        for leftover in
            dispatch::effects::kill_pid_tree(&descendant_victims, dispatch::effects::KILL_GRACE_MS)
        {
            failures.push(format!("descendant process PID {leftover} (still alive)"));
        }
    }

    // 3. Tombstone IFF the process is actually dead (I4) — or FOREIGN (r7: a
    //    reused pid means the SESSION is dead; the live stranger is not it).
    //    ensure_tombstone renames a live <pid>.json, or synthesizes one from
    //    the captured/fallback entry.
    if pid > 0 {
        if pid_foreign || !is_pid_alive(pid as i32) {
            let fallback = captured.clone().unwrap_or_else(|| RegistryEntry {
                pid: Some(pid),
                session_id: Some(session.session_id.clone()),
                cwd: session.cwd.clone(),
                started_at: session.started_at_ms.or(Some(clock.now_ms())),
                updated_at: Some(clock.now_ms()),
                status: Some("idle".to_string()),
                name: session.name.clone(),
                ..Default::default()
            });
            ensure_tombstone(&paths.sessions_dir, pid, Some(&fallback));
        } else {
            failures.push(format!(
                "registry tombstone for PID {pid} (process still alive)"
            ));
        }
    }

    if !failures.is_empty() {
        let label = session
            .name
            .clone()
            .or_else(|| target.zmx_name.clone())
            .unwrap_or_else(|| pid.to_string());
        eprintln!(
            "Failed to fully reap session \"{label}\". Could not reap: {}.",
            failures.join("; ")
        );
        return 1;
    }

    // F1 cleanup runs BEFORE kill-verify (verify may exit 1 and must not strand
    // the per-session env file). Best-effort.
    let kill_session_name = session
        .name
        .clone()
        .or_else(|| target.zmx_name.clone())
        .unwrap_or_default();
    if !kill_session_name.is_empty() {
        remove_session_env_file(&home, &kill_session_name);
    }

    // Kill-verify (F2/C2, lifecycle.ts:760-784): ONE presence scan for the pane
    // we actually targeted. A survivor means the "success" would be a silent
    // lie — exit 1 with an advisory. The hint uses `zmx ls`, NEVER `zmx kill`
    // (don't direct the operator to kill an innocent same-named session).
    // r7/M3: keyed to target.zmx_name (pane truth), NOT session.name — when no
    // pane was targeted there is nothing to verify, and a registry-name scan
    // would false-flag an ownership-refused innocent as a "survivor". Dir-pinned
    // for the same reason when the dir was confirmed.
    let verify_name = target.zmx_name.clone().unwrap_or_default();
    if !verify_name.is_empty() {
        if let Some(survivor) = scan_all_dirs(mux, &dirs, false)
            .into_iter()
            .find(|z| z.name == verify_name && at_target_dir(z))
        {
            let dir = survivor.socket_dir.unwrap_or(canonical_str.clone());
            eprintln!(
                "WARNING: zmx session \"{verify_name}\" still exists after kill (found in {dir}). \
                 Registry cleaned but zmx task was not reached. Verify with: ZMX_DIR={dir} zmx ls"
            );
            return 1;
        }
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
    let zmx_label = target.zmx_name.clone().unwrap_or_else(|| "-".to_string());
    let pid_label = if pid > 0 {
        pid.to_string()
    } else {
        "-".to_string()
    };
    let unconfirmed = if target.zmx_name.is_some() && target.zmx_dir_unconfirmed {
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

/// codex P2 W7 (codex-p2-spec §7.6 kill paragraph) — the codex KILL path at the
/// verb layer. A codex row is daemon-hosted; the daemon pid IS the session. ALL kill
/// logic lives in [`dispatch::resume_daemon::kill_codex`] (the GROUP-pgid ladder reap +
/// tombstone); this is the thin glue: capture the row BEFORE the kill (the tombstone
/// audit trail carries provider + endpoint), call the lib, print the success line.
/// NO zmx/mux reap (no pane). The HONEST EDGE (an already-dead daemon pid → no signal,
/// still tombstone) is handled inside `kill_codex`.
fn run_codex_kill(paths: &SbPaths, session: &dispatch::model::Session) -> i32 {
    use dispatch::create_daemon::{real_cmdline_probe, RealDaemonSpawner};
    use dispatch::resume_daemon::kill_codex;

    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    let pid = session.pid.unwrap_or(0);
    if pid <= 0 {
        // A codex row with no daemon pid has nothing to reap (and nothing to key a
        // tombstone by). Mirrors the claude "nothing to kill" wording shape.
        eprintln!("qd stop: \"{name}\": codex session has no daemon pid. Nothing to kill.");
        return 1;
    }

    // Capture the row BEFORE the kill (claude removes its own <pid>.json on graceful
    // SIGTERM; the codex daemon knows nothing of qd, but capturing keeps the tombstone
    // audit trail — provider + endpoint — even if the kill races a row rewrite).
    let captured = read_entry(&paths.sessions_dir, pid);
    let spawner = RealDaemonSpawner;
    // W9 FIX M-1: the cmdline-identity guard — kill_codex group-signals ONLY when
    // the live pid's command line is OUR codex daemon (never a reused-pid foreign
    // group). The probe reads one pid's cmdline via the existing `ps` seam.
    let probe = real_cmdline_probe;
    let outcome = kill_codex(
        &paths.sessions_dir,
        pid,
        captured.as_ref(),
        &spawner,
        &probe,
    );

    // ONE unambiguous success line (the W4 kill format shape): codex rows have no zmx
    // name, so the zmx slot is `-`. The daemon pid is the reaped identity.
    let reg_name = session.name.clone().unwrap_or_else(|| "-".to_string());
    if outcome.was_alive {
        println!("killed {reg_name} (zmx -, pid {pid})");
    } else {
        // The already-dead edge: the daemon was already gone — we tombstoned the
        // dead row (the dead-row seal). Honest about what happened.
        println!("killed {reg_name} (zmx -, pid {pid}) [daemon already dead — tombstoned]");
    }
    0
}

/// scoped-ACP-CC stop (F3 / rider-4 kill-no-leak): the ACP analog of [`run_codex_kill`].
/// GROUP-reaps the resident adapter pid (= pgid) via [`kill_acp`] — reaping the adapter
/// AND its bridge child together — gated on the ACP cmdline identity, then tombstones. No
/// zmx/mux reap (an acp row has no pane). After this, `pgrep` shows ZERO adapter/bridge
/// procs for the killed session (the no-leaked-procs proof).
fn run_acp_kill(paths: &SbPaths, session: &dispatch::model::Session) -> i32 {
    use dispatch::create_daemon::{real_cmdline_probe, RealDaemonSpawner};
    use dispatch::resume_daemon::kill_acp;

    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    let pid = session.pid.unwrap_or(0);
    if pid <= 0 {
        eprintln!("qd stop: \"{name}\": acp session has no daemon pid. Nothing to kill.");
        return 1;
    }
    // Capture the row BEFORE the kill (carries provider + endpoint for the tombstone).
    let captured = read_entry(&paths.sessions_dir, pid);
    let spawner = RealDaemonSpawner;
    let probe = real_cmdline_probe;
    let outcome = kill_acp(&paths.sessions_dir, pid, captured.as_ref(), &spawner, &probe);

    let reg_name = session.name.clone().unwrap_or_else(|| "-".to_string());
    if outcome.was_alive {
        println!("killed {reg_name} (zmx -, pid {pid})");
    } else {
        println!("killed {reg_name} (zmx -, pid {pid}) [adapter already dead — tombstoned]");
    }
    0
}

/// Scan every socket dir (canonical + legacy) with the FILTERED or RAW list and
/// flatten into one cross-dir vec (the kill-side equivalent of the join's
/// merge_canonical_wins — but kill wants ALL rows by name, not a dedupe).
fn scan_all_dirs(mux: &dyn Mux, dirs: &[PathBuf], raw: bool) -> Vec<MuxSession> {
    let mut out = Vec::new();
    for dir in dirs {
        let list = if raw {
            mux.list_raw(dir).unwrap_or_default()
        } else {
            mux.list(dir).unwrap_or_default()
        };
        out.extend(list);
    }
    out
}

/// The full `ps -eo pid=,ppid=,command=` rows for the ancestry walk. The walk
/// needs ALL processes (the zmx wrapper is a bash/zmx process, not a claude one),
/// so we route the same single parse `RealProcessTable` uses through the public
/// `Exec` seam + `parse_ps_rows`.
fn ps_rows(
    _pt: &RealProcessTable<RealExec>,
) -> Option<std::collections::HashMap<i32, dispatch::effects::ProcRow>> {
    use dispatch::effects::parse_ps_rows;
    use dispatch::exec::Exec;
    let exec = RealExec;
    let r = exec
        .run(
            "ps",
            &["-eo".to_string(), "pid=,ppid=,command=".to_string()],
            &[],
            None,
            None,
        )
        .ok()?;
    Some(parse_ps_rows(&r.stdout))
}
