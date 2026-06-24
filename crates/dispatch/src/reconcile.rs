//! `sb reconcile` decision core (spec §5.4; TS `session.ts:1098-1177` +
//! `commands/lifecycle.ts:937-971`).
//!
//! Idempotently detect drift across the three sources of truth and PLAN repairs.
//! This module is a PURE decider: it takes the live registry entries, the RAW
//! cross-dir zmx list, and a liveness predicate, and returns the action plan.
//! The bin verb drives the plan (tombstone, zmx kill, kill_pid) and the
//! `--dry-run` gate.
//!
//! # The invariants (comments-carry)
//!
//! - **I1** (`session.ts:1116-1133`): a live registry entry (`<pid>.json`) whose
//!   claude PID is DEAD → tombstone it.
//! - **I3** (`session.ts:1135-1167`): a zmx wrapper whose claude PID is dead AND
//!   that has no live registry entry → reap it (orphan). Skip clients>0 (attached
//!   — never touch). Skip a wrapper that is alive AND tied to a live registry
//!   entry.
//! - **I5** (the safety floor): NEVER touch a session whose claude PID is alive.
//!   Acts only on specific PIDs (never pkill/patterns) — same L10 discipline.
//!
//! # Carry 4: stray observation, read-only
//!
//! Stray discovery (`stray::classify`) runs READ-ONLY in the same pass and is
//! reported as observation lines. There is NO adopt/takeover write path (PARKED).
//! Registry coverage converges by OBSERVATION, not seizure. The stray pass is
//! orchestrated by the bin verb (it owns the transcript/proc gather); this module
//! owns only the I1/I3/I5 repair plan.

use crate::mux::MuxSession;
use crate::registry::RegistryEntry;
use std::collections::HashSet;

/// A planned reconcile action (`ReconcileAction`, the TS `result.actions` shape).
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// I1: tombstone a dead-PID live registry entry.
    TombstoneDeadRegistry { pid: i64, detail: String },
    /// I3: reap an orphan / dead zmx wrapper.
    ReapOrphanWrapper {
        /// The wrapper's tracked pid (0 when the task is purely ended/unreachable
        /// with no live wrapper pid). The bin verb kills this pid if > 0.
        pid: i32,
        name: String,
        socket_dir: Option<String>,
        detail: String,
    },
}

/// The reconcile plan: ordered actions (I1 first, then I3), plus the set of live
/// registry PIDs (so the bin verb / tests can cross-check I5 was honored).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub live_registry_pids: HashSet<i64>,
}

/// PURE: build the reconcile plan (port of `reconcile`, session.ts:1108-1176).
///
/// - `registry`: the LIVE registry entries (no tombstones) — `getPidEntries`.
/// - `zmx_raw`: the RAW cross-dir zmx list (INCLUDES ended/unreachable) —
///   `getZmxSessionsRaw`.
/// - `is_alive`: liveness predicate (`isProcessAlive`), injected so tests forge a
///   process table with NO live sessions needed.
///
/// I5 is structural: a registry PID that `is_alive` returns true for is added to
/// `live_registry_pids` and NEVER appears in an action. An I3 candidate whose
/// wrapper is alive, or whose (non-dead) pid is a live registry pid, is skipped.
pub fn plan(
    registry: &[RegistryEntry],
    zmx_raw: &[MuxSession],
    is_alive: &dyn Fn(i64) -> bool,
) -> Plan {
    let mut actions = Vec::new();
    let mut live_registry_pids: HashSet<i64> = HashSet::new();

    // --- I1: dead-PID registry entries → tombstone (session.ts:1116-1133) ---
    for entry in registry {
        let Some(pid) = entry.pid else {
            continue; // a registry blob with no pid can't be keyed/tombstoned.
        };
        if is_alive(pid) {
            live_registry_pids.insert(pid); // I5: alive → never touched.
            continue;
        }
        let label = entry
            .name
            .clone()
            .or_else(|| entry.session_id.clone())
            .unwrap_or_default();
        actions.push(Action::TombstoneDeadRegistry {
            pid,
            detail: format!("{label} (pid {pid} dead)"),
        });
    }

    // --- I3: orphan / dead zmx wrappers → reap (session.ts:1135-1167) ---
    // RAW list includes ended/unreachable. A task is an orphan to reap when it is
    // ended/unreachable, OR its tracked wrapper pid is dead and it has no live
    // registry entry. NEVER touch an attached (clients>0) session or one whose
    // wrapper pid is alive and tied to a live registry entry (I5).
    for z in zmx_raw {
        if z.clients > 0 {
            continue; // attached/interactive — never touch (I5).
        }
        let dead =
            z.ended.is_some() || z.zmx_status.as_deref() == Some("unreachable") || z.err.is_some();
        let wrapper_alive = !dead && is_alive(z.pid as i64);
        if wrapper_alive {
            continue; // healthy live wrapper — leave it (I5).
        }
        if !dead && live_registry_pids.contains(&(z.pid as i64)) {
            continue; // belongs to a live session (I5).
        }
        let detail = if dead {
            format!(
                "zmx \"{}\" (ended/unreachable, in {})",
                z.name,
                z.socket_dir.as_deref().unwrap_or("?")
            )
        } else {
            format!(
                "zmx \"{}\" (pid {} dead, in {})",
                z.name,
                z.pid,
                z.socket_dir.as_deref().unwrap_or("?")
            )
        };
        actions.push(Action::ReapOrphanWrapper {
            pid: z.pid,
            name: z.name.clone(),
            socket_dir: z.socket_dir.clone(),
            detail,
        });
    }

    Plan {
        actions,
        live_registry_pids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: i64, name: &str, sid: &str) -> RegistryEntry {
        RegistryEntry {
            pid: Some(pid),
            session_id: Some(sid.to_string()),
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    fn z(name: &str, pid: i32, clients: u32, dir: &str) -> MuxSession {
        MuxSession {
            name: name.to_string(),
            pid,
            clients,
            created: 0,
            start_dir: "/h".to_string(),
            cmd: "claude".to_string(),
            current: false,
            socket_dir: Some(dir.to_string()),
            ended: None,
            exit_code: None,
            zmx_status: None,
            err: None,
        }
    }

    fn alive_set(set: &[i64]) -> impl Fn(i64) -> bool {
        let owned: Vec<i64> = set.to_vec();
        move |p: i64| owned.contains(&p)
    }

    const DIR: &str = "/tmp/zmx-501";

    #[test]
    fn i1_dead_pid_registry_entry_tombstoned() {
        let reg = vec![entry(100, "sess", "sid-1")];
        let p = plan(&reg, &[], &alive_set(&[])); // 100 dead
        assert_eq!(p.actions.len(), 1);
        match &p.actions[0] {
            Action::TombstoneDeadRegistry { pid, detail } => {
                assert_eq!(*pid, 100);
                assert!(detail.contains("sess (pid 100 dead)"));
            }
            other => panic!("expected tombstone, got {other:?}"),
        }
    }

    #[test]
    fn i1_alive_pid_registry_entry_untouched_and_tracked() {
        let reg = vec![entry(100, "sess", "sid-1")];
        let p = plan(&reg, &[], &alive_set(&[100]));
        assert!(
            p.actions.is_empty(),
            "alive PID must never be an action (I5)"
        );
        assert!(p.live_registry_pids.contains(&100));
    }

    #[test]
    fn i3_orphan_dead_wrapper_no_registry_is_reaped() {
        // zmx wrapper pid dead, no registry entry → orphan → reap.
        let z1 = z("orphan", 200, 0, DIR);
        let p = plan(&[], &[z1], &alive_set(&[]));
        assert_eq!(p.actions.len(), 1);
        match &p.actions[0] {
            Action::ReapOrphanWrapper {
                pid,
                name,
                socket_dir,
                detail,
            } => {
                assert_eq!(*pid, 200);
                assert_eq!(name, "orphan");
                assert_eq!(socket_dir.as_deref(), Some(DIR));
                assert!(detail.contains("pid 200 dead"));
            }
            other => panic!("expected reap, got {other:?}"),
        }
    }

    #[test]
    fn i3_ended_task_reaped_even_with_zero_pid() {
        let mut z1 = z("ended", 0, 0, DIR);
        z1.ended = Some(1_700_000_000);
        let p = plan(&[], &[z1], &alive_set(&[]));
        assert_eq!(p.actions.len(), 1);
        match &p.actions[0] {
            Action::ReapOrphanWrapper { detail, .. } => {
                assert!(detail.contains("ended/unreachable"));
            }
            other => panic!("expected reap, got {other:?}"),
        }
    }

    #[test]
    fn i3_attached_session_clients_gt_zero_never_touched() {
        // clients>0 → attached/interactive → never touch, even if pid "dead".
        let z1 = z("attached", 300, 2, DIR);
        let p = plan(&[], &[z1], &alive_set(&[]));
        assert!(
            p.actions.is_empty(),
            "attached session must be skipped (I5)"
        );
    }

    #[test]
    fn i3_live_wrapper_with_live_registry_entry_skipped() {
        // wrapper pid alive AND tied to a live registry entry → leave it (I5).
        let reg = vec![entry(400, "live", "sid-live")];
        let z1 = z("live", 400, 0, DIR);
        let p = plan(&reg, &[z1], &alive_set(&[400]));
        assert!(p.actions.is_empty());
        assert!(p.live_registry_pids.contains(&400));
    }

    #[test]
    fn i3_live_wrapper_alone_is_left_even_without_registry() {
        // A live wrapper pid with NO registry entry is still not an orphan — a
        // live process is never reaped (I5: wrapper_alive guard).
        let z1 = z("livestray", 500, 0, DIR);
        let p = plan(&[], &[z1], &alive_set(&[500]));
        assert!(
            p.actions.is_empty(),
            "live wrapper pid must not be reaped (I5)"
        );
    }

    /// The pid an action targets (for the I5 negative control). Tombstone targets
    /// the registry pid; reap targets the wrapper pid.
    fn action_pid(a: &Action) -> i64 {
        match a {
            Action::TombstoneDeadRegistry { pid, .. } => *pid,
            Action::ReapOrphanWrapper { pid, .. } => *pid as i64,
        }
    }

    #[test]
    fn i5_negative_control_alive_pid_must_not_produce_an_action() {
        // MUST-FAIL injection harness: if the decider EVER produced an action that
        // targets an ALIVE pid, this reds. Combined registry + zmx, all alive.
        let reg = vec![entry(100, "a", "s-a"), entry(200, "b", "s-b")];
        let z_a = z("a", 100, 0, DIR);
        let z_b = z("b", 200, 0, DIR);
        let alive = [100i64, 200];
        let p = plan(&reg, &[z_a, z_b], &alive_set(&alive));
        // No action may target a live pid (I5). A decider regression that touched
        // a live PID would put 100/200 into an action here and red this assert.
        for a in &p.actions {
            let pid = action_pid(a);
            assert!(
                !alive.contains(&pid),
                "I5 VIOLATION: action targets alive pid {pid}: {a:?}"
            );
        }
        // And in fact there is nothing to do — all four sources agree (all alive).
        assert!(p.actions.is_empty());
    }

    #[test]
    fn combined_drift_orders_i1_before_i3() {
        // A dead registry entry (I1) and a separate orphan wrapper (I3).
        let reg = vec![entry(100, "deadreg", "sid-1")];
        let z_orphan = z("orphan", 200, 0, DIR);
        let p = plan(&reg, &[z_orphan], &alive_set(&[]));
        assert_eq!(p.actions.len(), 2);
        assert!(matches!(p.actions[0], Action::TombstoneDeadRegistry { .. }));
        assert!(matches!(p.actions[1], Action::ReapOrphanWrapper { .. }));
    }

    #[test]
    fn registry_entry_without_pid_is_ignored() {
        let mut e = entry(0, "x", "s");
        e.pid = None;
        let p = plan(&[e], &[], &alive_set(&[]));
        assert!(p.actions.is_empty());
    }
}
