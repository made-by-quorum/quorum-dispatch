//! R3b-Step-1 — three-layer health classification + the first-class **WEDGED**
//! state (alive-but-not-progressing), the GENUINE two-signal guard.
//!
//! ## The three layers (R1 §4)
//! - **Layer 1 — liveness**: dead vs alive, from the R3a reuse-robust `/proc`
//!   reconcile ([`crate::liveness::LifecycleState`]). OS truth wins for
//!   death/identity: nothing here resurrects a `Gone`/`Exited*` process.
//! - **Layer 2 — readiness = signal-A**: the wall-clock turn-start timeout
//!   ([`crate::liveness::classify_obs`] → [`crate::liveness::LifecycleState::Stuck`],
//!   `turn_in_flight && since_turn_start_ms > τ`). Fires on ANY long turn.
//! - **Layer 3 — progress = signal-B**: output-derived staleness
//!   ([`crate::progress::progress_stale`] over the live [`crate::progress`]
//!   producer). Fires only when output has genuinely stopped.
//!
//! ## WEDGED = signal-A AND signal-B, over DISJOINT producers
//! `Wedged IFF live.is_alive() && status == Busy && signal_a_stuck && signal_b_stale`.
//! The two stale-signals arrive as **two separate parameters from two call sites**
//! (signal-A from the turn-start anchor, signal-B from the output cadence) and are
//! **NEVER derived one from the other** — that is the enforced non-vacuity
//! constraint. A long STREAMING turn trips signal-A but NOT signal-B → `Busy`, never
//! `Wedged`; a long SILENT turn trips both → `Wedged`. (`is_busy_stale`/`updated_at`
//! is deliberately NOT a guard signal: it co-fires with signal-A and would re-create
//! the vacuity the gate caught.)
//!
//! ## What Wedged is FOR (Pete's binding ruling)
//! `Wedged` arms AUTO-RECOVERY only — never a per-session human flag. Unlike
//! [`crate::liveness::LifecycleState::Stuck`] (which is `is_alive()` and
//! `is_diagnostic_stuck()` — the type contract FORBIDS building control automation
//! on it), `Wedged` is the first-class verdict that [`Health::authorizes_recovery`].
//! The DESTRUCTIVE rung (kill+respawn) stays additionally gated on signal-B being
//! LIVE for the session's harness (the safe-by-default cap, enforced in
//! `recovery.rs`, R3c); at THIS layer a `Wedged` verdict is the classification, the
//! cap decides how far the ladder may climb.

use crate::liveness::{DaemonLiveness, LifecycleState};
use crate::model::SessionStatus;

/// The grace window (ms) during which a headless session whose daemon is
/// [`DaemonLiveness::Down`] is NOT classified `Wedged` (Open-Q #7). When the daemon
/// dies, BOTH of a headless session's signal producers stop (no reader ⇒ no
/// `Republish::Event`, and `updated_at` freezes), which would false-WEDGE every
/// headless row during a daemon restart. The window itself (keyed on the persisted
/// daemon-up timestamp) is enforced by the caller in the recovery row, §5; this
/// constant is the default span the caller applies.
pub const DAEMON_DOWN_GRACE_MS: i64 = 300_000;

/// The three-layer health verdict. Distinct from [`SessionStatus`] (the registry
/// status string) and from [`LifecycleState`] (the OS/readiness facet): this is the
/// fold that the recovery ladder reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// OS-confirmed dead (`Exited*`/`Gone`). The death/kill gate's domain, not the
    /// recovery ladder's.
    Dead,
    /// Alive and not in a turn (or a non-`busy` registry status) — nothing to do.
    Idle,
    /// Alive and working: in a turn that is EITHER under τ OR still producing output
    /// (signal-B fresh). The healthy long-streaming turn lands here, NOT in `Wedged`.
    Busy,
    /// Alive, in a `busy` turn, past τ (signal-A), AND silent past N (signal-B) —
    /// the first-class alive-but-not-progressing state. The ONLY verdict that
    /// [`Self::authorizes_recovery`].
    Wedged,
}

impl Health {
    /// Does this verdict AUTHORIZE the recovery ladder? Only `Wedged`. This is the
    /// recovery-authorizing counterpart to [`LifecycleState::is_diagnostic_stuck`]:
    /// where `Stuck` exists precisely to FORBID control automation, `Wedged` exists
    /// to arm it (subject to the safe-by-default destructive cap in `recovery.rs`).
    /// `Dead` is recovered through the death/kill gate, not the wedge ladder, so it
    /// is not "wedge-recovery-authorizing" here.
    pub fn authorizes_recovery(self) -> bool {
        matches!(self, Health::Wedged)
    }

    /// Lowercase wire/display token.
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Dead => "dead",
            Health::Idle => "idle",
            Health::Busy => "busy",
            Health::Wedged => "wedged",
        }
    }
}

/// Fold the three layers into a single [`Health`]. PURE: the staleness of each
/// signal is computed by the CALLERS (signal-A via [`crate::liveness::classify_obs`]
/// → `Stuck`; signal-B via [`crate::progress::progress_stale`]) and passed in as two
/// independent booleans — this function never derives one from the other, which is
/// what keeps the WEDGED guard non-vacuous.
///
/// Precedence:
/// 1. **Layer 1 wins for death**: `is_dead()` ⇒ `Dead` (never `Wedged`); a
///    not-ours/not-alive process is non-actionable ⇒ `Idle`.
/// 2. **Daemon-down grace**: a headless session whose daemon is `Down` loses both
///    producers, so suppress the wedge verdict (⇒ `Busy`/`Idle` by status) for the
///    grace window (see [`DAEMON_DOWN_GRACE_MS`]).
/// 3. Not a `busy` turn ⇒ `Idle`.
/// 4. `busy` + alive: `Wedged` iff BOTH signals stale, else `Busy`.
#[allow(clippy::too_many_arguments)]
pub fn classify_health(
    live: LifecycleState,
    status: SessionStatus,
    signal_a_stuck: bool,
    signal_b_stale: bool,
    daemon_state: DaemonLiveness,
    is_headless: bool,
    now_ms: i64,
) -> Health {
    // Reserved: the grace-window arithmetic (now vs the persisted daemon-up stamp)
    // lives in the caller (the recovery row, §5); the pure fold only needs the
    // already-resolved `daemon_state`. Kept in the signature for that contract.
    let _ = now_ms;

    // Layer 1: OS truth wins for death/identity.
    if live.is_dead() {
        return Health::Dead;
    }
    if !live.is_alive() {
        // `NotOurs` — a reused pid, not our session: never alive, never dead, never
        // actionable. Non-`busy`/non-wedge.
        return Health::Idle;
    }

    // Daemon-down grace (Open-Q #7): both signal producers are gone for a headless
    // row whose daemon is Down — do NOT WEDGE it during a daemon restart window.
    if is_headless && daemon_state == DaemonLiveness::Down {
        return if status == SessionStatus::Busy {
            Health::Busy
        } else {
            Health::Idle
        };
    }

    // Not in a turn ⇒ nothing to wedge.
    if status != SessionStatus::Busy {
        return Health::Idle;
    }

    // Alive + busy: the genuine two-signal guard. BOTH disjoint signals must be
    // stale; either one fresh ⇒ still `Busy` (the streaming long turn is fresh on
    // signal-B even though signal-A fired).
    if signal_a_stuck && signal_b_stale {
        Health::Wedged
    } else {
        Health::Busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn classify(
        live: LifecycleState,
        status: SessionStatus,
        a: bool,
        b: bool,
    ) -> Health {
        classify_health(live, status, a, b, DaemonLiveness::Up, true, NOW)
    }

    /// The keystone: BOTH signals stale on an alive busy turn ⇒ `Wedged`, and only
    /// then. This is the verdict the recovery ladder arms on.
    #[test]
    fn wedged_iff_both_signals_stale() {
        assert_eq!(
            classify(LifecycleState::AliveWorking, SessionStatus::Busy, true, true),
            Health::Wedged
        );
    }

    /// The RF-1 false-positive control, at the classifier level: a healthy long
    /// STREAMING turn fires signal-A (past τ) but NOT signal-B (output fresh) ⇒
    /// `Busy`, NEVER `Wedged`. Reverting to a single-signal guard (treating
    /// signal-A alone as wedge) would mis-classify this — the negative-control seam.
    #[test]
    fn streaming_long_turn_is_busy_not_wedged() {
        assert_eq!(
            classify(LifecycleState::Stuck, SessionStatus::Busy, true, false),
            Health::Busy,
            "signal-A fired but signal-B is fresh (streaming) → Busy"
        );
    }

    /// signal-B stale alone (silent) but signal-A not yet past τ ⇒ still `Busy`
    /// (a brief quiet stretch early in a turn is not a wedge). Both must fire.
    #[test]
    fn silent_but_within_tau_is_busy() {
        assert_eq!(
            classify(LifecycleState::AliveWorking, SessionStatus::Busy, false, true),
            Health::Busy
        );
    }

    /// Neither signal ⇒ a plain working turn is `Busy`.
    #[test]
    fn fresh_busy_is_busy() {
        assert_eq!(
            classify(LifecycleState::AliveWorking, SessionStatus::Busy, false, false),
            Health::Busy
        );
    }

    /// Layer 1 wins for death: a dead process is `Dead` even if the (stale) registry
    /// row still says busy and both signals read stale — never `Wedged`.
    #[test]
    fn dead_wins_over_wedge() {
        for d in [
            LifecycleState::Gone,
            LifecycleState::ExitedClean,
            LifecycleState::ExitedSignal,
        ] {
            assert_eq!(
                classify(d, SessionStatus::Busy, true, true),
                Health::Dead,
                "{d:?} must be Dead, not Wedged"
            );
        }
    }

    /// A `NotOurs` (reused) pid is non-actionable: `Idle`, never `Wedged`.
    #[test]
    fn not_ours_is_idle() {
        assert_eq!(
            classify(LifecycleState::NotOurs, SessionStatus::Busy, true, true),
            Health::Idle
        );
    }

    /// Non-`busy` status ⇒ `Idle` regardless of the signal bits (nothing in flight
    /// to wedge).
    #[test]
    fn non_busy_is_idle() {
        for st in [
            SessionStatus::Idle,
            SessionStatus::Shell,
            SessionStatus::Cold,
            SessionStatus::Killed,
        ] {
            assert_eq!(
                classify(LifecycleState::AliveReady, st, true, true),
                Health::Idle,
                "{st:?} → Idle"
            );
        }
    }

    /// Daemon-down grace: a headless busy row whose daemon is Down is NOT wedged
    /// (both producers are gone during the restart) — it reads `Busy` for the window.
    #[test]
    fn daemon_down_headless_is_not_wedged() {
        assert_eq!(
            classify_health(
                LifecycleState::Stuck,
                SessionStatus::Busy,
                true,
                true,
                DaemonLiveness::Down,
                true,
                NOW,
            ),
            Health::Busy,
            "headless + daemon Down → suppress wedge (Open-Q #7)"
        );
    }

    /// The grace is HEADLESS-only: an interactive (non-headless) session is NOT
    /// shielded by a daemon-down probe (its producer is the PTY, not the daemon
    /// reader), so the genuine two-signal wedge still classifies.
    #[test]
    fn daemon_down_interactive_still_wedges() {
        assert_eq!(
            classify_health(
                LifecycleState::Stuck,
                SessionStatus::Busy,
                true,
                true,
                DaemonLiveness::Down,
                false,
                NOW,
            ),
            Health::Wedged
        );
    }

    /// Only `Wedged` authorizes recovery — the recovery-authorizing counterpart to
    /// `LifecycleState::is_diagnostic_stuck` (which forbids it).
    #[test]
    fn only_wedged_authorizes_recovery() {
        assert!(Health::Wedged.authorizes_recovery());
        assert!(!Health::Busy.authorizes_recovery());
        assert!(!Health::Idle.authorizes_recovery());
        assert!(!Health::Dead.authorizes_recovery());
    }
}
