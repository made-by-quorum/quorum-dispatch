//! R3b-Step-0 — the shared, output-derived PROGRESS producer (**signal-B**).
//!
//! ## Why this exists (the keystone the gate caught)
//! The WEDGED guard needs TWO genuinely-disjoint signals (R2 §0, RF-1). signal-A
//! ([`crate::liveness::classify_obs`] → [`crate::liveness::LifecycleState::Stuck`])
//! is a **wall-clock turn-start timeout**: `turn_in_flight && since_turn_start_ms >
//! τ`. It NEVER consults incremental output, so it fires on ANY long turn —
//! healthy-streaming or hung alike. [`crate::status_recency::is_busy_stale`] is NOT
//! a second signal either: it co-fires with signal-A by construction (both read the
//! turn lifecycle off the same daemon stream). Using it as the second guard was the
//! VACUITY the gate caught, and it is RETRACTED as a guard (it stays a display-only
//! recency hint).
//!
//! **signal-B is the genuinely-disjoint surface this module provides:** "no OUTPUT
//! for N ms". Its producer is the incremental stream itself — NOT the turn
//! lifecycle. A long turn that IS streaming output keeps signal-B fresh → `Busy`; a
//! turn that is long AND silent trips BOTH signals → `Wedged`. That independence is
//! the whole non-vacuity of the guard, and it is PROVEN on the real classifier
//! (faultinj `class2_wedge_vs_longturn`), not assumed.
//!
//! ## A REAL LIVE producer, not a `#[cfg(test)]` fixture
//! The rev0 defect was a signal that existed only in tests. The live tap is the
//! dispatch-side status sink ([`crate::daemon_status`]): every [`Republish`] a
//! session emits flows through it, and `Republish::Event` (mid-turn output) is the
//! one that advances output WITHOUT moving the registry `updated_at`/status (the
//! documented disjointness guarantee, `daemon_status.rs`). The recorder is fed
//! there on the live path; the faultinj harness feeds the SAME recorder from a real
//! victim's stdout bytes (the `PtyBytes` floor). Same code, real output, both paths.
//!
//! ## Shared surface (one producer, not two)
//! WS-A's "started/streaming" event contract consumes the SAME producer
//! ([`ProgressProducer::last_output_ms`] / the [`ProgressTick`] stream); it does NOT
//! build a parallel one. This module is independent of [`crate::health`] (it is not
//! buried in the wedge classifier) so both WS-R (the wedge guard) and WS-A depend on
//! `progress`, not on each other.

use std::collections::HashMap;
use std::sync::Mutex;

/// Default N (ms): how long a session may emit NO output before signal-B is STALE.
/// Generous on purpose (a legitimately quiet stretch mid-turn must not flap the
/// guard); the PRIMARY false-positive defense is that a STREAMING turn keeps this
/// fresh regardless of N — N only governs the genuinely-silent case. Matches the
/// turn-start τ ([`crate::liveness::STUCK_THRESHOLD_MS`]) by default; per-session
/// tunable by the caller (it is a plain parameter to [`progress_stale`]).
pub const PROGRESS_STALE_N_MS: i64 = 300_000;

/// Which live surface a progress observation came from (per-harness provenance).
/// Diagnostic/forensic only — the staleness predicate does not branch on it; it
/// rides [`ProgressTick`] so a consumer (WS-A) can attribute a tick to its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressSource {
    /// Claude Code headless: `Republish::Event` per-line stream-event cadence
    /// (`qrmux` reader → dispatch `daemon_status` sink). The primary live producer.
    AcpUpdate,
    /// JSONL transcript byte/line growth past an entry-time offset (`wait.rs`).
    TranscriptGrowth,
    /// codex: rollout-JSONL tail growth (`provider/codex/rollout.rs`).
    RolloutTail,
    /// Interactive / floor: raw PTY/stdout output-byte deltas (coarse, parse-free).
    PtyBytes,
}

/// A single observed output instant for a session: "session `session_id` produced
/// output at `last_output_ms`, seen via `source`". The unit a live tap emits and
/// WS-A's streaming-event contract consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressTick {
    pub session_id: String,
    pub last_output_ms: i64,
    pub source: ProgressSource,
}

/// The read surface the health classifier (and WS-A) query: the epoch-ms instant a
/// session last produced ANY output, or `None` if no output has ever been observed
/// for it. `None` is insufficient evidence, NOT a wedge — see [`progress_stale`]'s
/// fail-open rule.
pub trait ProgressProducer {
    fn last_output_ms(&self, session_id: &str) -> Option<i64>;
}

/// signal-B staleness predicate: has this session emitted NO output for strictly
/// MORE than `n_ms`? Pure over the data passed in (the last-output instant, the
/// injected `now`, and N); reads no clock and no disk, so it is deterministic and
/// side-effect-free — exactly the shape of [`crate::status_recency::is_busy_stale`].
///
/// Rules, chosen to never produce a false WEDGE:
/// - `last_output_ms == None` (no output ever observed) ⇒ **NOT stale** (fail-open;
///   insufficient evidence must never authorize recovery on a guess). A session for
///   which no producer is wired reads not-stale here, and the safe-by-default cap
///   (`health` / `recovery`) keeps such a session advisory-only anyway.
/// - Boundary is strict `>`: `now - last == n_ms` is the LAST non-stale moment
///   (exactly N elapsed is not yet stale), matching `is_busy_stale`'s τ boundary.
pub fn progress_stale(last_output_ms: Option<i64>, now_ms: i64, n_ms: i64) -> bool {
    match last_output_ms {
        Some(ts) => now_ms - ts > n_ms,
        None => false,
    }
}

/// [`progress_stale`] over the tuned [`PROGRESS_STALE_N_MS`] — the capability a
/// consumer calls without restating N.
pub fn progress_stale_default(last_output_ms: Option<i64>, now_ms: i64) -> bool {
    progress_stale(last_output_ms, now_ms, PROGRESS_STALE_N_MS)
}

/// The live recorder/store: a per-session last-output timestamp, advanced on each
/// tapped emission on the live path (the `daemon_status` sink's `Republish::Event`,
/// the faultinj victim's stdout bytes, codex rollout tail, …). Thread-safe and
/// shareable via `Arc`: the daemon reader thread WRITES while the classifier READS.
///
/// **Monotonic**: an out-of-order or replayed tick never moves `last_output_ms`
/// backward, so a late-arriving stale observation can never make a fresh session
/// look stale. **Poison-tolerant**: a panic in one holder must not freeze progress
/// tracking for every other session (this feeds a reliability guard), so the lock
/// is recovered from poison rather than propagated.
#[derive(Debug, Default)]
pub struct ProgressRecorder {
    last: Mutex<HashMap<String, i64>>,
}

impl ProgressRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `session_id` produced output at `ts_ms` (via `source`). Monotonic
    /// max — a backward/duplicate timestamp is ignored. `source` is provenance only;
    /// the stored surface is the timestamp the classifier queries.
    pub fn record(&self, session_id: &str, ts_ms: i64, _source: ProgressSource) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let slot = g.entry(session_id.to_string()).or_insert(ts_ms);
        if ts_ms > *slot {
            *slot = ts_ms;
        }
    }

    /// Drop a session's progress record (e.g. on tombstone/GC) so the map does not
    /// grow without bound. Absent → no-op.
    pub fn forget(&self, session_id: &str) {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }
}

impl ProgressProducer for ProgressRecorder {
    fn last_output_ms(&self, session_id: &str) -> Option<i64> {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .copied()
    }
}

/// signal-A STANDING producer: the per-session wall-clock turn-start anchor — the
/// genuinely-disjoint counterpart of [`ProgressRecorder`] (signal-B). The daemon
/// records when a turn STARTS (status → `busy`) and clears it when the turn ENDS
/// (status → `idle`/`offline`); the classifier reads
/// [`TurnStartProducer::since_turn_start_ms`] to drive
/// [`crate::liveness::StreamLiveness::classify_obs`] (signal-A → `Stuck` past τ).
///
/// This is the LIVE producer the rev0 design lacked: `classify_obs` had no
/// non-test production consumer of a real `since_turn_start_ms` — the live-chain
/// gate fed it a synthetic past-τ value. Fed on the SAME live path as signal-B
/// (the `daemon_status` sink's turn lifecycle), so `turn_in_flight` here aligns
/// with the registry `busy` status by construction.
///
/// **First-start-wins within a turn** (the anchor measures the WHOLE turn, not the
/// last event): a mid-turn re-`Ready` does NOT reset the anchor. **Poison-tolerant**
/// (a reliability guard must not freeze on one holder's panic). Thread-safe/shareable
/// via `Arc` (the daemon reader thread WRITES while the classifier READS).
#[derive(Debug, Default)]
pub struct TurnStartRecorder {
    /// session_id -> the epoch-ms instant the in-flight turn STARTED. Absent ⇒ no
    /// turn in flight (→ never `Stuck`).
    inflight: Mutex<HashMap<String, i64>>,
}

impl TurnStartRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a turn STARTED for `session_id` at `ts_ms`. First-start-wins
    /// within a turn: a re-`Ready` while a turn is already in flight does NOT reset
    /// the anchor (since_turn_start measures the whole turn).
    pub fn turn_started(&self, session_id: &str, ts_ms: i64) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .or_insert(ts_ms);
    }

    /// Record that the in-flight turn ENDED for `session_id` (no turn in flight).
    /// Absent → no-op.
    pub fn turn_ended(&self, session_id: &str) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// The turn-start anchor for `session_id`: `Some(turn_start_ms)` while a turn is
    /// in flight, else `None`. (The `turn_in_flight` bit `classify_obs` reads is
    /// exactly `is_some()`.)
    pub fn turn_start_ms(&self, session_id: &str) -> Option<i64> {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .copied()
    }
}

/// The signal-A read surface the classifier (and the coordinator) query: ms since
/// the in-flight turn started, or `None` if no turn is in flight (→ NOT `Stuck`).
/// Pure over the data; the analog of [`ProgressProducer`] for signal-A.
pub trait TurnStartProducer {
    fn since_turn_start_ms(&self, session_id: &str, now_ms: i64) -> Option<i64>;
}

impl TurnStartProducer for TurnStartRecorder {
    fn since_turn_start_ms(&self, session_id: &str, now_ms: i64) -> Option<i64> {
        // Saturating at 0: a clock skew that puts `now` before the anchor is "just
        // started", never a negative elapsed (which would underflow the τ compare).
        self.turn_start_ms(session_id)
            .map(|start| (now_ms - start).max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N BOUNDARY (mirrors the `is_busy_stale` τ test): below N not stale, exactly N
    /// not stale (strict `>`), one ms past N stale.
    #[test]
    fn progress_stale_at_n_boundary() {
        let n = 1000;
        let now = 10_000;
        assert!(
            !progress_stale(Some(now - (n - 1)), now, n),
            "below N: fresh"
        );
        assert!(
            !progress_stale(Some(now - n), now, n),
            "exactly N: not yet stale (strict >)"
        );
        assert!(progress_stale(Some(now - (n + 1)), now, n), "past N: stale");
    }

    /// FAIL-OPEN: no output ever observed ⇒ NOT stale (never a false WEDGE on
    /// insufficient evidence). This is the safety-critical rule.
    #[test]
    fn no_output_is_never_stale() {
        assert!(!progress_stale(None, 1_000_000, 1000));
        assert!(!progress_stale_default(None, 1_000_000_000));
    }

    /// The non-vacuity crux, at the predicate level: given the SAME elapsed wall-time
    /// since turn-start (signal-A would fire for both), a STREAMING session (recent
    /// output) is signal-B-FRESH while a SILENT one is signal-B-STALE. signal-B
    /// genuinely separates work from hang — which signal-A cannot.
    #[test]
    fn streaming_is_fresh_while_silent_is_stale_at_same_wall_time() {
        let now = 500_000;
        let n = 1000;
        // Both turns started long ago (signal-A: stuck for both). Streaming emitted
        // 200ms ago; silent last emitted 5s ago.
        let streaming_last = Some(now - 200);
        let silent_last = Some(now - 5000);
        assert!(
            !progress_stale(streaming_last, now, n),
            "streaming → fresh → Busy"
        );
        assert!(
            progress_stale(silent_last, now, n),
            "silent → stale → Wedged"
        );
    }

    /// Recorder advances monotonically and never moves backward on a stale tick.
    #[test]
    fn recorder_is_monotonic() {
        let r = ProgressRecorder::new();
        assert_eq!(r.last_output_ms("s"), None);
        r.record("s", 100, ProgressSource::AcpUpdate);
        assert_eq!(r.last_output_ms("s"), Some(100));
        r.record("s", 250, ProgressSource::PtyBytes);
        assert_eq!(r.last_output_ms("s"), Some(250));
        // A backward/duplicate tick is ignored.
        r.record("s", 180, ProgressSource::AcpUpdate);
        assert_eq!(r.last_output_ms("s"), Some(250));
        r.record("s", 250, ProgressSource::AcpUpdate);
        assert_eq!(r.last_output_ms("s"), Some(250));
    }

    /// Sessions are isolated — one session's output never advances another's signal.
    #[test]
    fn recorder_isolates_sessions() {
        let r = ProgressRecorder::new();
        r.record("a", 100, ProgressSource::AcpUpdate);
        r.record("b", 999, ProgressSource::RolloutTail);
        assert_eq!(r.last_output_ms("a"), Some(100));
        assert_eq!(r.last_output_ms("b"), Some(999));
        assert_eq!(r.last_output_ms("c"), None);
    }

    /// `forget` drops the record (GC) and reverts the session to the fail-open None.
    #[test]
    fn recorder_forget_drops_record() {
        let r = ProgressRecorder::new();
        r.record("s", 100, ProgressSource::AcpUpdate);
        assert_eq!(r.last_output_ms("s"), Some(100));
        r.forget("s");
        assert_eq!(r.last_output_ms("s"), None);
    }

    /// The recorder is `Send + Sync` (shareable across the daemon reader thread and
    /// the classifier) — a compile-time assertion.
    #[test]
    fn recorder_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProgressRecorder>();
    }

    /// signal-A producer: a turn in flight yields `since_turn_start_ms`; no turn ⇒
    /// `None` (→ never Stuck). First-start-wins (a mid-turn re-start does not reset
    /// the anchor); turn_ended clears it.
    #[test]
    fn turn_start_recorder_tracks_in_flight_anchor() {
        let r = TurnStartRecorder::new();
        // No turn → None (the fail-safe: classify_obs's turn_in_flight is false).
        assert_eq!(r.since_turn_start_ms("s", 10_000), None);
        // Turn starts at t=1000.
        r.turn_started("s", 1_000);
        assert_eq!(r.turn_start_ms("s"), Some(1_000));
        assert_eq!(r.since_turn_start_ms("s", 6_000), Some(5_000));
        // First-start-wins: a re-Ready at t=4000 does NOT reset the anchor.
        r.turn_started("s", 4_000);
        assert_eq!(
            r.since_turn_start_ms("s", 6_000),
            Some(5_000),
            "anchor unchanged"
        );
        // Turn ends → no anchor → None.
        r.turn_ended("s");
        assert_eq!(r.since_turn_start_ms("s", 6_000), None);
    }

    /// A clock skew that puts `now` before the anchor saturates at 0 (never a
    /// negative elapsed that would underflow the τ comparison).
    #[test]
    fn turn_start_since_saturates_at_zero_on_skew() {
        let r = TurnStartRecorder::new();
        r.turn_started("s", 10_000);
        assert_eq!(r.since_turn_start_ms("s", 9_000), Some(0));
    }

    /// Sessions are isolated — one session's turn never advances another's anchor.
    #[test]
    fn turn_start_recorder_isolates_sessions() {
        let r = TurnStartRecorder::new();
        r.turn_started("a", 100);
        assert_eq!(r.turn_start_ms("a"), Some(100));
        assert_eq!(r.turn_start_ms("b"), None);
    }

    /// `Send + Sync` (the daemon reader thread WRITES while the classifier READS).
    #[test]
    fn turn_start_recorder_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TurnStartRecorder>();
    }
}
