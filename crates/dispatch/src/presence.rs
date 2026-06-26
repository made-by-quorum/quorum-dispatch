//! R3c-Step-2 §4 — the **presence** façade: the single shared "is this session
//! alive / ready / wedged, and who is it" surface that WS-A and WS-B consume so
//! neither reimplements kernel-truth liveness or the WEDGED guard.
//!
//! This module **delegates to the seeds, never forks them**: liveness
//! ([`crate::liveness`]), health ([`crate::health`]), the flock probe
//! ([`crate::livelock`]), and the ladder ([`crate::recovery`]). It re-exports their
//! types so A/B have ONE import surface, and adds the [`Presence`] snapshot + the
//! consumption-contract functions.
//!
//! ## The non-vacuity invariant (the contract A/B must honor)
//! `Health::Wedged` is authoritative **ONLY** from [`health_of`] — the genuine
//! two-signal guard lives inside it. A/B treating any SINGLE signal as WEDGED is a
//! contract violation (guarded by the anti-masquerade test, §3). [`presence_of`] is
//! the cheap disk/flock SNAPSHOT and never fabricates `Wedged` (it has no live
//! signals) — it returns `Idle`/`Busy`/`Dead` only.
//!
//! ## Read-only for A/B (P8 — the SC-1↔WS-R boundary)
//! Presence never mutates the registry or fires the ladder. Recovery is WS-R's to
//! drive; A/B observe via [`Presence::recovery`]. A Rung-4 respawn mints a FRESH
//! session_id + bumps incarnation, orphaning WS-A's per-session outbound queue keyed
//! on the OLD incarnation — so WS-R emits a [`SessionReplaced`] event on the
//! `Respawn → Idle@newIncarnation` transition ([`detect_session_replaced`]); WS-A
//! fences its queue on the new incarnation (its own replay-vs-drop call).

use crate::health::classify_health;
use crate::liveness::{
    readiness_facet, DaemonLiveness, LifecycleState, LivenessSource, OsLiveness, ProcKey,
};
use crate::model::SessionStatus;
use std::path::Path;

// --- Re-exports: the ONE import surface A/B/R3d consume (delegate, never fork) ---
pub use crate::health::Health;
pub use crate::liveness::Readiness;
pub use crate::recovery::{Identity, LadderState, Rung};

/// A session's presence snapshot — the shared "alive/ready/wedged, and who is it"
/// concern (R3c §4). The `health` field is the SNAPSHOT health (never `Wedged` —
/// the authoritative wedge verdict comes from [`health_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub session_id: String,
    /// `(pid, start_ms)` — the reuse-robust OS identity (`None` if no live row).
    pub identity: Option<ProcKey>,
    /// The monotonic claim fence (WS-A fences its outbound queue on this; `None`
    /// if unknown).
    pub incarnation: Option<u64>,
    /// Layer-1 liveness (OS truth).
    pub liveness: LifecycleState,
    /// The `sb ls` readiness facet (`None` for not-alive).
    pub readiness: Option<Readiness>,
    /// SNAPSHOT health — `Dead`/`Idle`/`Busy` only. `Wedged` is authoritative ONLY
    /// via [`health_of`] (the two-signal guard); `presence_of` has no live signals.
    pub health: Health,
    pub daemon: DaemonLiveness,
    pub status: SessionStatus,
    pub updated_at_ms: Option<i64>,
    /// The active ladder state, if a recovery row exists (read-only for A/B).
    pub recovery: Option<LadderState>,
}

/// Cheap liveness-only fast path: the flock probe, NO `/proc` walk (R3c §4). `true`
/// iff a live holder still owns the session's liveness lock.
pub fn is_live(state_dir: &Path, session_id: &str) -> bool {
    !crate::livelock::probe_dead(state_dir, session_id)
}

/// The AUTHORITATIVE health classification — the ONLY place `Health::Wedged` is
/// produced (R3c §4 / the RF-1 two-signal guard). Wraps [`classify_health`]: signal-A
/// and signal-B arrive as two INDEPENDENT booleans (never derived one from the
/// other). A/B MUST route every wedge question through here.
#[allow(clippy::too_many_arguments)]
pub fn health_of(
    live: LifecycleState,
    status: SessionStatus,
    signal_a_stuck: bool,
    signal_b_stale: bool,
    daemon: DaemonLiveness,
    is_headless: bool,
    now_ms: i64,
) -> Health {
    classify_health(
        live,
        status,
        signal_a_stuck,
        signal_b_stale,
        daemon,
        is_headless,
        now_ms,
    )
}

/// The gated display status (cache-as-display; re-probes liveness before returning).
/// Wraps [`crate::liveness::gated_ls_status`].
pub fn display_status(
    status: SessionStatus,
    pid: Option<i64>,
    recorded_start_ms: Option<i64>,
    src: &dyn LivenessSource,
) -> SessionStatus {
    crate::liveness::gated_ls_status(status, pid, recorded_start_ms, src)
}

/// Snapshot one session's presence from kernel truth + cache, FAIL-CLOSED (R3c §4).
///
/// Disk-readable facts only: the recovery row (`LadderState` + the identity it acts
/// on) and the flock liveness probe. `liveness` is re-classified from the OS for the
/// row's `ProcKey`; `health` is the SNAPSHOT fold (signals defaulted to fresh → never
/// `Wedged`; the authoritative wedge is [`health_of`] with the live signals the
/// coordinator supplies). `daemon` defaults to `Up` (the coordinator overlays a real
/// daemon probe where it has one).
pub fn presence_of(state_dir: &Path, session_id: &str, now_ms: i64) -> Presence {
    let row = crate::recovery::read_recovery_row(state_dir, session_id);
    let recovery = row.as_ref().map(|r| r.ladder_state);
    let identity = row.as_ref().and_then(|r| {
        r.old_identity.as_ref().map(|i| ProcKey::new(i.pid, i.start_ms))
    });
    let incarnation = row.as_ref().and_then(|r| r.old_identity.as_ref().map(|i| i.incarnation));

    let live = is_live(state_dir, session_id);
    // Classify liveness from the OS for the known identity; without one, fall back to
    // the flock fact (live ⇒ AliveSilentValid, else Gone) — fail-closed alive only
    // when the lock says a holder is present.
    let src = OsLiveness::default();
    let liveness = match identity {
        Some(key) => src.classify(key),
        None => {
            if live {
                LifecycleState::AliveSilentValid
            } else {
                LifecycleState::Gone
            }
        }
    };

    // Snapshot status: a recovery-tracked session in a Running ladder is Busy;
    // otherwise derive from liveness (live ⇒ Idle, else Cold). The display path
    // (`display_status`) is where the gated registry status is resolved.
    let status = match recovery {
        Some(LadderState::Running(_)) => SessionStatus::Busy,
        _ if live => SessionStatus::Idle,
        _ => SessionStatus::Cold,
    };

    // SNAPSHOT health: signals defaulted fresh → classify_health yields Dead/Idle/
    // Busy only (NEVER Wedged — that requires the live two-signal guard via health_of).
    let daemon = DaemonLiveness::Up;
    let health = classify_health(liveness, status, false, false, daemon, true, now_ms);

    Presence {
        session_id: session_id.to_string(),
        identity,
        incarnation,
        liveness,
        readiness: readiness_facet(liveness),
        health,
        daemon,
        status,
        updated_at_ms: row.as_ref().and_then(|r| r.last_attempt_ms),
        recovery,
    }
}

// ===========================================================================
// The session-replaced event (P8 — the SC-1↔WS-R boundary contract)
// ===========================================================================

/// Emitted by WS-R on the `Respawn → Idle@newIncarnation` ladder transition: a
/// Rung-4 respawn replaced a wedged session with a fresh `session_id` + bumped
/// incarnation. WS-A subscribes to this and FENCES its per-session outbound queue on
/// `new_incarnation` (replay-vs-drop is WS-A's own call). Carries the OLD identity so
/// a consumer can reconcile the queue keyed on the old incarnation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReplaced {
    pub old_identity: Identity,
    pub new_session_id: String,
    pub new_incarnation: u64,
}

/// Detect the `Respawn → Idle@newIncarnation` transition that mints a successor.
///
/// Returns `Some(SessionReplaced)` IFF the ladder was `Running(Respawn)` and has now
/// settled to `Idle` AND a successor was minted (the row carries `old_identity` +
/// `new_session_id` + the new incarnation). Any other transition ⇒ `None` (no
/// spurious replacement events). This is the typed boundary contract; WS-R writes the
/// event to the log, A subscribes — presence DEFINES + DETECTS, the coordinator EMITS
/// (presence is read-only for A/B).
pub fn detect_session_replaced(
    prev: LadderState,
    next: LadderState,
    old_identity: Option<&Identity>,
    new_session_id: Option<&str>,
    new_incarnation: Option<u64>,
) -> Option<SessionReplaced> {
    if prev != LadderState::Running(Rung::Respawn) || next != LadderState::Idle {
        return None;
    }
    match (old_identity, new_session_id, new_incarnation) {
        (Some(old), Some(nsid), Some(inc)) => Some(SessionReplaced {
            old_identity: old.clone(),
            new_session_id: nsid.to_string(),
            new_incarnation: inc,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::{write_recovery_row, RecoveryRow};
    use tempfile::tempdir;

    #[test]
    fn is_live_tracks_the_flock_holder() {
        use crate::livelock::LivenessLock;
        let dir = tempdir().unwrap();
        // ENOENT (no lock file) is NOT positive death → fail-closed alive.
        assert!(
            is_live(dir.path(), "sess-x"),
            "ENOENT is not positive death (fail-closed alive)"
        );
        // A HELD lock → a fresh probe fd cannot acquire → live.
        {
            let _lock = LivenessLock::acquire(dir.path(), "sess-x")
                .expect("acquire io")
                .expect("lock free → acquired");
            assert!(is_live(dir.path(), "sess-x"), "held lock → live");
        } // released here
        // Released lock (file persists, lock free) → probe_dead → NOT live.
        assert!(
            !is_live(dir.path(), "sess-x"),
            "released lock → probe_dead positive → not live"
        );
    }

    #[test]
    fn health_of_is_the_only_wedged_source() {
        // BOTH signals stale on an alive busy turn ⇒ Wedged (the authoritative path).
        assert_eq!(
            health_of(
                LifecycleState::AliveWorking,
                SessionStatus::Busy,
                true,
                true,
                DaemonLiveness::Up,
                true,
                0
            ),
            Health::Wedged
        );
        // A streaming long turn (signal-B fresh) is Busy, never Wedged.
        assert_eq!(
            health_of(
                LifecycleState::Stuck,
                SessionStatus::Busy,
                true,
                false,
                DaemonLiveness::Up,
                true,
                0
            ),
            Health::Busy
        );
    }

    #[test]
    fn presence_snapshot_never_wedges() {
        let dir = tempdir().unwrap();
        let mut row = RecoveryRow::cold("sess-p");
        row.ladder_state = LadderState::Running(Rung::ControlWake);
        row.old_identity = Some(Identity {
            pid: 999_999, // a pid that is not alive → Gone
            start_ms: 1,
            incarnation: 4,
        });
        write_recovery_row(dir.path(), &row).unwrap();

        let p = presence_of(dir.path(), "sess-p", 1_000);
        // The snapshot surfaces the ladder + identity, and NEVER fabricates Wedged.
        assert_eq!(p.recovery, Some(LadderState::Running(Rung::ControlWake)));
        assert_eq!(p.incarnation, Some(4));
        assert_ne!(p.health, Health::Wedged, "snapshot must never be Wedged");
    }

    #[test]
    fn session_replaced_fires_only_on_respawn_to_idle_with_successor() {
        let old = Identity {
            pid: 10,
            start_ms: 20,
            incarnation: 3,
        };
        // The replacement transition with a minted successor → event.
        let ev = detect_session_replaced(
            LadderState::Running(Rung::Respawn),
            LadderState::Idle,
            Some(&old),
            Some("succ"),
            Some(4),
        );
        assert_eq!(
            ev,
            Some(SessionReplaced {
                old_identity: old.clone(),
                new_session_id: "succ".into(),
                new_incarnation: 4,
            })
        );

        // A non-respawn transition → no event.
        assert_eq!(
            detect_session_replaced(
                LadderState::Running(Rung::PtyInject),
                LadderState::Idle,
                Some(&old),
                Some("succ"),
                Some(4)
            ),
            None
        );
        // Respawn→Idle but no successor minted → no event.
        assert_eq!(
            detect_session_replaced(
                LadderState::Running(Rung::Respawn),
                LadderState::Idle,
                Some(&old),
                None,
                None
            ),
            None
        );
    }
}
