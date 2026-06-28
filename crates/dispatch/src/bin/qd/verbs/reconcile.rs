//! REAL `qd reconcile` backend (spec §5.4; TS `session.ts:1098-1177` +
//! `commands/lifecycle.ts:937-971`).
//!
//! The I1/I3/I5 repair plan is the pure `dispatch::reconcile::plan`; this verb gathers
//! the live registry + the RAW cross-dir zmx list (full Bug-D tier sweep) + a
//! liveness predicate, drives the plan (tombstone, zmx kill, kill_pid) under the
//! `--dry-run` gate, AND runs `stray::classify` READ-ONLY in the same pass —
//! reported as observation lines, NO adopt/takeover write path (PARKED, carry 4).
//! Exits 0/1.

use std::path::PathBuf;

use clap::ArgMatches;

use dispatch::effects::{
    is_pid_alive, kill_pid, Clock, Env, ProcessTable, RealClock, RealEnv, RealProcessTable,
};
use dispatch::exec::RealExec;
use dispatch::mux::{Mux, MuxSession};
use dispatch::paths::QdPaths;
use dispatch::reconcile::{plan, Action};
use dispatch::registry::{read_entries, tombstone, RegistryEntry};
use dispatch::stray::classify;
use dispatch::zmx_dir::{legacy_zmx_dirs, resolve_zmx_dir, XdgFamily};

use super::common;

/// `qd reconcile [--dry-run]`.
pub fn run(m: &ArgMatches) -> i32 {
    let dry_run = m.get_flag("dry-run");

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd reconcile: HOME is not set — cannot resolve the session state dir.");
            return 1;
        }
    };
    let paths = QdPaths::from_home(&home);
    // Backend-selected mux (C1 D3). ONE QD_MUX parse drives the mux AND the dir
    // sweep below. A bogus QD_MUX exits loudly here.
    let backend = match common::select_backend(&env) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let mux_box = match common::build_mux(backend, &home, &env) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let mux: &dyn Mux = mux_box.as_ref();
    let exec = RealExec;
    let pt = RealProcessTable::new(exec);
    let clock = RealClock;

    // Live registry entries (no tombstones) — getPidEntries.
    let registry: Vec<RegistryEntry> = read_entries(&paths.sessions_dir, false)
        .into_iter()
        .map(|s| s.entry)
        .collect();

    // RAW mux list across the dir sweep, INCLUDING ended/unreachable tasks.
    // Backend-keyed (C1 D2): zmx = FULL Bug-D tier sweep (canonical + legacy,
    // `/tmp` + the env-derived XDG family); embedded = the single qrmux dir
    // (legacy EMPTY — D-LISTRAW: embedded list_raw never surfaces ended sessions).
    // A14-2(c): the surviving zmx READ scan honors QD_TEST_SCAN_ROOTS (test lanes
    // only; production = literal /tmp). A14-2(d): the dry-run gate + negative-control
    // belts stand for the destructive surface below.
    let canonical = match backend {
        dispatch::mux_selector::Backend::Zmx => resolve_zmx_dir(&env),
        dispatch::mux_selector::Backend::Embedded => {
            match dispatch::qrmux_dir::resolve_qrmux_dir(&home, &env) {
                Ok(d) => d,
                Err(msg) => {
                    eprintln!("qd reconcile: {msg}");
                    return 1;
                }
            }
        }
    };
    let legacy = match backend {
        dispatch::mux_selector::Backend::Zmx => {
            let scan_roots =
                dispatch::zmx_dir::legacy_scan_roots(&env, std::path::Path::new("/tmp"));
            let xdg = XdgFamily::from_env(&env, env.uid());
            legacy_zmx_dirs(env.uid(), &canonical, &scan_roots, Some(&xdg))
        }
        dispatch::mux_selector::Backend::Embedded => Vec::new(),
    };
    let mut dirs = vec![canonical.clone()];
    dirs.extend(legacy);
    let mut zmx_raw: Vec<MuxSession> = Vec::new();
    for dir in &dirs {
        zmx_raw.extend(mux.list_raw(dir).unwrap_or_default());
    }

    // The I1/I3/I5 plan (pure). Liveness via the WS-R R3a-Step-3 RECONCILED
    // predicate (R1 §2): the registry is a CACHE reconciled against KERNEL TRUTH,
    // so a crashed session's `busy` row is never treated as live. The predicate
    // composes the O(1) flock fast-path (R3a-Step-1) with the reuse-robust
    // `(pid, start_ms)` `/proc` authority (R3a-Step-2). A row carrying a recorded
    // `started_at` is checked reuse-robustly (a recycled pid => not-alive => the
    // dead row is tombstoned); a row WITHOUT a recorded start falls back to the
    // legacy `kill -0` check (fail-open, no regression for un-stamped rows).
    let os = dispatch::liveness::OsLiveness::new();
    let state_dir = paths.state_dir.clone();
    // Per-pid lookup of (recorded start_ms, session_id) for the reuse-robust check.
    let identity_of = |pid: i64| -> (Option<i64>, Option<String>) {
        registry
            .iter()
            .find(|e| e.pid == Some(pid))
            .map(|e| (e.started_at, e.session_id.clone()))
            .unwrap_or((None, None))
    };
    let is_alive = |pid: i64| {
        match identity_of(pid) {
            // Reuse-robust kernel-truth reconcile (flock fast-path + /proc start_ms).
            (Some(start), session_id) => dispatch::liveness::is_session_live_reconciled(
                Some(state_dir.as_path()),
                session_id.as_deref(),
                pid,
                start,
                &os,
            ),
            // No recorded start to reuse-guard => legacy kill -0 (fail-open).
            (None, _) => is_pid_alive(pid as i32),
        }
    };
    let p = plan(&registry, &zmx_raw, &is_alive);

    // --- Carry 4: stray discovery, READ-ONLY observation (no write path). ---
    let strays = gather_strays(&paths, &pt, &registry, &clock);

    // --- Drive the plan + report (lifecycle.ts:941-960). ---
    let mut errors: Vec<String> = Vec::new();
    if p.actions.is_empty() {
        println!("Nothing to reconcile — all sources of truth agree.");
    } else {
        let verb = if dry_run { "Would repair" } else { "Repaired" };
        println!("{verb} {} drift item(s):", p.actions.len());
        for action in &p.actions {
            match action {
                Action::TombstoneDeadRegistry { pid, detail } => {
                    println!("  tombstone: {detail}");
                    if !dry_run {
                        let ok = tombstone(&paths.sessions_dir, *pid);
                        if !ok {
                            errors.push(format!("failed to tombstone {pid}.json"));
                        }
                    }
                }
                Action::ReapOrphanWrapper {
                    pid,
                    name,
                    socket_dir,
                    detail,
                } => {
                    println!("  reap-wrapper: {detail}");
                    if !dry_run {
                        let dir = socket_dir
                            .clone()
                            .map(PathBuf::from)
                            .unwrap_or_else(|| canonical.clone());
                        let code = mux.kill(&dir, name).unwrap_or(1);
                        // Best-effort: also kill the tracked pid if still around.
                        if *pid > 0 {
                            kill_pid(*pid, dispatch::effects::KILL_GRACE_MS);
                        }
                        if code != 0 && *pid > 0 && is_pid_alive(*pid) {
                            errors
                                .push(format!("failed to reap orphan wrapper {name} (pid {pid})"));
                        }
                    }
                }
            }
        }
    }

    // Stray observation lines (additive vs TS; named divergence §9 row 6). These
    // are OBSERVATIONS only — registry coverage converges by observation, not
    // seizure (TAKEOVER is PARKED, carry 4).
    if !strays.is_empty() {
        for s in &strays {
            println!(
                "  stray: {} {}",
                s.pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                s.jsonl_path.display()
            );
        }
    }

    if !errors.is_empty() {
        eprintln!("\n{} error(s):", errors.len());
        for e in &errors {
            eprintln!("  {e}");
        }
        return 1;
    }
    0
}

/// Gather strays READ-ONLY: scan transcripts + claude procs, build the registry
/// union, classify. The PARKED-takeover path carries pid/paths but never acts.
fn gather_strays(
    paths: &QdPaths,
    pt: &RealProcessTable<RealExec>,
    registry: &[RegistryEntry],
    clock: &RealClock,
) -> Vec<dispatch::stray::Stray> {
    use std::collections::HashSet;
    let transcripts = dispatch::jsonl::scan_all(&paths.projects_dir);
    // The UNION of live + tombstoned registry session ids (a managed transcript
    // is never a stray). Live ids from `registry`; tombstoned from the dir.
    let mut reg_ids: HashSet<String> = registry
        .iter()
        .filter_map(|e| e.session_id.clone())
        .collect();
    for t in dispatch::registry::get_tombstoned_entries(&paths.sessions_dir) {
        if let Some(sid) = t.data.session_id {
            reg_ids.insert(sid);
        }
    }
    // Registry pids the engine already accounts for (a proc whose pid is here is
    // managed → not stray evidence).
    let reg_pids_alive: HashSet<i32> = registry
        .iter()
        .filter_map(|e| e.pid)
        .filter(|&p| is_pid_alive(p as i32))
        .map(|p| p as i32)
        .collect();
    let claude_procs = pt.claude_procs().unwrap_or_default();
    classify(
        &transcripts,
        &reg_ids,
        &reg_pids_alive,
        &claude_procs,
        clock.now_ms(),
    )
}
