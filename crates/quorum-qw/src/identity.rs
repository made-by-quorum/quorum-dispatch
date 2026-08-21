//! WS-R R3a-Step-2 — the full session identity triple `(pid, start_ms,
//! incarnation)` and incarnation fencing (R1 §3, §8; resolves Open-Q #10).
//!
//! ## Why three components
//! - `pid` — the kernel PID. Alone it is NOT a stable identity: Linux recycles
//!   PIDs (`pid_max` wraps), so a dead session's PID can be reused by a stranger.
//! - `start_ms` — `/proc/<pid>/stat` field 22 -> epoch ms ([`crate::liveness::ProcKey`]).
//!   `(pid, start_ms)` defeats PID reuse: a PID now held by a process that
//!   started materially later is a DIFFERENT process
//!   ([`crate::liveness::LifecycleState::NotOurs`]), not our session resurrected.
//!   This is the SIGNAL-SAFETY / registry-reconciliation half.
//! - `incarnation` — a monotonic counter per session-NAME (R1 §3 inv 2),
//!   persisted in the name-claim file ([`crate::registry::claim_payload_with_incarnation`]).
//!   It is the NAME-LEVEL fence: a writer holding incarnation N must not stomp a
//!   row owned by N+1. `start_ms` is the ROW-level fence (the `started_at` CAS in
//!   [`crate::registry::set_status`]); incarnation fences across a full
//!   kill+respawn under the SAME name (a fresh process, fresh `started_at`, and a
//!   bumped incarnation).
//!
//! ## The two separate concerns (do NOT conflate — R1 §3, RF-4)
//! `start_ms` is a registry-reconciliation key, NOT a signal-safety precondition.
//! pidfd signal-safety (Rung 1, R3c) is by the fd binding ALONE; this struct's
//! `(pid, start_ms)` answers "is the registry row I am about to read/write the
//! same incarnation?", a cache-correctness question. Keeping them apart avoids a
//! re-probe TOCTOU between pidfd open and send.
//!
//! LIVES IN qw, NOT in the `quorum-core` leaf (qd/qw split). The intent was core —
//! this file knows nothing about any provider, and incarnation fencing is
//! infrastructure both sides of the split need. It cannot go there: `proc_key()`
//! calls [`crate::liveness::ProcKey::new`], a REAL dependency (not just the doc
//! links above), and `liveness` lives in qw. `quorum-core` is a strict leaf that
//! depends on nothing else in the workspace, so following identity into core would
//! have meant dragging `ProcKey` out of the classifier it was defined for — a
//! bigger, differently-shaped change than a relocation. qw is where the whole
//! `(pid, start_ms)` story already lives, so it goes beside it. `dispatch`
//! re-exports the module under its old flat name, so `dispatch::identity::…` /
//! `crate::identity::…` (the `adopt` verb, `relay_server::mcp`) keeps resolving.

use crate::liveness::ProcKey;
use serde::{Deserialize, Serialize};

/// The reuse-robust, fence-carrying identity of one managed session (R1 §8).
/// `session_id` is the STABLE id (from `idstore`, survives respawn under the
/// same name); `(pid, start_ms)` is the live-process identity (changes each
/// incarnation); `incarnation` is the monotonic name-level fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdentity {
    /// Stable session id (from `idstore` / `ids.jsonl`) — survives respawn.
    pub session_id: String,
    /// Kernel PID of this incarnation.
    pub pid: i32,
    /// `/proc/<pid>/stat` field 22 -> epoch ms, recorded at registration.
    pub start_ms: i64,
    /// Monotonic per-session-NAME fence; persisted in the name-claim file.
    /// Never decremented (R1 §3 inv 2). Absent on a legacy claim => 0.
    pub incarnation: u64,
}

impl SessionIdentity {
    pub fn new(session_id: impl Into<String>, pid: i32, start_ms: i64, incarnation: u64) -> Self {
        Self {
            session_id: session_id.into(),
            pid,
            start_ms,
            incarnation,
        }
    }

    /// The `(pid, start_ms)` projection used by the OS liveness classifier and
    /// the kill/identity gate — the signal-safety / row-reconciliation half.
    pub fn proc_key(&self) -> ProcKey {
        ProcKey::new(self.pid, self.start_ms)
    }

    /// Incarnation FENCE check (R1 §3): may a writer holding `self.incarnation`
    /// legitimately act on a row/claim currently owned by `on_disk_incarnation`?
    ///
    /// Only when `self.incarnation >= on_disk_incarnation`. A writer at N that
    /// finds the name claimed at N+1 has been FENCED OUT (a newer incarnation
    /// reclaimed the name) and MUST self-terminate — it must NOT stomp N+1. This
    /// is the monotonic-fence rule; equality is allowed (same incarnation
    /// re-asserting), a strictly-newer on-disk incarnation is the veto.
    pub fn fences_out(&self, on_disk_incarnation: u64) -> bool {
        self.incarnation < on_disk_incarnation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_key_projection() {
        let id = SessionIdentity::new("sid-1", 4242, 1_700_000_000_000, 3);
        assert_eq!(id.proc_key(), ProcKey::new(4242, 1_700_000_000_000));
    }

    #[test]
    fn fences_out_only_on_strictly_newer_on_disk() {
        let id = SessionIdentity::new("sid-1", 1, 0, 5);
        // A writer at 5 is fenced out by an on-disk 6 (newer reclaim).
        assert!(id.fences_out(6), "writer at N is fenced by on-disk N+1");
        // Same incarnation is NOT fenced (re-asserting our own row).
        assert!(!id.fences_out(5), "same incarnation is not fenced");
        // An OLDER on-disk incarnation does not fence a newer writer.
        assert!(
            !id.fences_out(4),
            "a newer writer is never fenced by an older row"
        );
        // Legacy on-disk (0) never fences.
        assert!(!id.fences_out(0), "incarnation 0 (legacy) never fences");
    }
}
