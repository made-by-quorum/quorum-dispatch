//! WP-D part (b) — the INTERNAL `busy`-staleness recency signal (status-tool
//! guard). **Computed + exposed as a capability, but renders NOTHING.**
//!
//! ## Why this exists (the fleet-observability gap, §9a memo Step-1)
//! A registry `busy` status is **sticky**: when a turn hangs mid-flight the
//! session's own answerer never writes `idle` back, so the row shows `busy`
//! forever with `updatedAt` frozen — and a hung-busy session is byte-identical to
//! a working-busy one. Part (a)'s liveness gate catches a session whose pid is
//! **dead**; this catches the other half — a pid that is **alive** but whose
//! `busy` has gone **stale** (no registry progress for longer than τ).
//!
//! ## What it is — and what it is NOT
//! This is a **heuristic**, not a guarantee. Distinguishing "working a long turn"
//! from "truly hung" is the same ready-vs-stuck gap that the OS classifier cannot
//! close either (§9a memo Step-2/Step-3); it needs a producer progress signal,
//! which is the parallel (B) track. So a long legitimate turn (whose `updatedAt`
//! is frozen for its duration) can read stale here — acceptable because this
//! signal renders nothing and only feeds a future, separately-approved indicator.
//!
//! ## INTERNAL-ONLY boundary (this WP)
//! The signal is a pure capability ready to be threaded into the status fold (in
//! the engine's `qd ls` consumers, or an outside consumer's status-liveness fold). It produces
//! **no `busy-stale` token, no warn glyph, no new status string** — surfacing it
//! is DISPLAY SURFACE, held by the lead for Pete. This module therefore emits
//! nothing on its own; it only computes.

use crate::model::SessionStatus;

/// The staleness threshold τ (ms): how long a `busy` row may sit with a frozen
/// `updatedAt` before it is internally considered STALE. A tuned INTERNAL
/// constant (no command/display surface) — generous on purpose, since a
/// legitimate long turn keeps `busy` with a frozen `updatedAt` for its whole
/// duration and must not be flagged casually. The eventual visible step (held by
/// the lead) is where this is finalized; until then it is consumed by nothing
/// user-visible. Chosen at 5 minutes: well beyond a typical turn's wall time, far
/// under an overnight stall.
pub const BUSY_STALE_TAU_MS: i64 = 300_000;

/// Is this row a STALE `busy` — `status == Busy` AND no registry update for
/// strictly MORE than `tau_ms`? Pure over the data already on the row (the status,
/// the last-update epoch ms, and the injected `now`); does NOT read the clock or
/// any disk itself, so it is deterministic and side-effect-free.
///
/// Rules. A non-`Busy` status is NEVER stale (idle/shell/cold/killed are out of
/// scope). `updated_at_ms == None` (no recorded progress instant) ⇒ NOT stale
/// (insufficient evidence, fail-open, never a false positive). Boundary: `now -
/// updated_at == tau` is NOT yet stale (strict `>`), so exactly τ elapsed is the
/// last non-stale moment.
pub fn is_busy_stale(
    status: SessionStatus,
    updated_at_ms: Option<i64>,
    now_ms: i64,
    tau_ms: i64,
) -> bool {
    if status != SessionStatus::Busy {
        return false;
    }
    match updated_at_ms {
        Some(ts) => now_ms - ts > tau_ms,
        None => false,
    }
}

/// [`is_busy_stale`] over the tuned [`BUSY_STALE_TAU_MS`] — the capability a
/// consumer calls without restating τ.
pub fn is_busy_stale_default(
    status: SessionStatus,
    updated_at_ms: Option<i64>,
    now_ms: i64,
) -> bool {
    is_busy_stale(status, updated_at_ms, now_ms, BUSY_STALE_TAU_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// τ BOUNDARY (§6 DoD): below τ not stale, exactly τ not stale (strict `>`),
    /// one ms past τ stale. The recency flag computes correctly at the boundary.
    #[test]
    fn busy_stale_at_tau_boundary() {
        let tau = 1000;
        let now = 10_000;
        // updated 999ms ago → not stale.
        assert!(!is_busy_stale(
            SessionStatus::Busy,
            Some(now - (tau - 1)),
            now,
            tau
        ));
        // updated EXACTLY τ ago → not yet stale (strict >).
        assert!(!is_busy_stale(
            SessionStatus::Busy,
            Some(now - tau),
            now,
            tau
        ));
        // updated τ+1 ago → stale.
        assert!(is_busy_stale(
            SessionStatus::Busy,
            Some(now - (tau + 1)),
            now,
            tau
        ));
    }

    /// FALSE-POSITIVE guard: only `busy` is ever stale — a long-frozen IDLE/SHELL/
    /// COLD/KILLED row is never flagged (the signal is about a stuck-busy turn).
    #[test]
    fn only_busy_can_be_stale() {
        let tau = 1000;
        let now = 1_000_000; // arbitrarily far past any plausible updatedAt
        for st in [
            SessionStatus::Idle,
            SessionStatus::Shell,
            SessionStatus::Cold,
            SessionStatus::Killed,
        ] {
            assert!(
                !is_busy_stale(st, Some(0), now, tau),
                "{st:?} must never be busy-stale"
            );
        }
        // ... but a busy row equally far past IS stale.
        assert!(is_busy_stale(SessionStatus::Busy, Some(0), now, tau));
    }

    /// FALSE-POSITIVE guard 2: a busy row with NO recorded update instant is NOT
    /// stale (insufficient evidence — fail-open, never a spurious stale flag).
    #[test]
    fn busy_without_updated_at_is_not_stale() {
        assert!(!is_busy_stale(SessionStatus::Busy, None, 1_000_000, 1000));
    }

    /// FRESH busy (active progress) is not stale — the working-session case the
    /// signal must leave alone.
    #[test]
    fn fresh_busy_is_not_stale() {
        let now = 50_000;
        assert!(!is_busy_stale(
            SessionStatus::Busy,
            Some(now - 10),
            now,
            1000
        ));
    }

    /// The default wrapper consults the tuned τ: just under 5 minutes is fresh,
    /// just over is stale.
    #[test]
    fn default_tau_wrapper() {
        let now = 1_000_000_000;
        assert!(!is_busy_stale_default(
            SessionStatus::Busy,
            Some(now - (BUSY_STALE_TAU_MS - 1)),
            now
        ));
        assert!(is_busy_stale_default(
            SessionStatus::Busy,
            Some(now - (BUSY_STALE_TAU_MS + 1)),
            now
        ));
    }
}
