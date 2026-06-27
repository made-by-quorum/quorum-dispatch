//! Republish→status Sink bridge (WP-B2b-1, memo R1: daemon-as-status-writer).
//!
//! A concrete [`qrmux::headless::Sink`] the daemon attaches to a headless session
//! so the registry status row is written in REAL TIME — control no longer rests
//! on a stale disk read. Each [`Republish`] the headless pump delivers is mapped
//! to a CAS-guarded [`registry::set_status`] per the lifecycle below; because the
//! write is CAS-guarded, even this sink cannot stomp a FOREIGN incarnation's row
//! (a `Rejected`/`NoRow` outcome is logged and swallowed — a status write never
//! panics the reader/pump).
//!
//! # Status lifecycle (one mapping per Republish)
//! - [`Republish::Ready`] → `"busy"` (the turn is running; producer is up).
//! - [`Republish::Event`] → no write (coalescible mid-turn output; still busy —
//!   avoid write amplification).
//! - [`Republish::TurnEnd`] → `"idle"` (turn complete).
//! - [`Republish::Breaker`] → `"offline"` (circuit-breaker killed the child
//!   mid-turn — never leave it busy forever; R2/§H.1 hook).
//! - [`Republish::Eof`] with [`TurnOutcome::Completed`] → `"idle"` (the row is
//!   already idle from the `TurnEnd`; an idempotent re-assert is fine).
//! - [`Republish::Eof`] with [`TurnOutcome::Aborted`] or [`TurnOutcome::NoTurn`]
//!   → `"offline"` (EOF without a clean result — fail-closed display: do NOT
//!   leave a stale "busy"; the keystone R2 behavior the gate demands).
//!
//! The clock (`now_ms`) is injected so the sink is deterministically testable.
//!
//! This module is NOT wired into the live daemon launch yet — that, plus the
//! socket fan-out, is WP-B2b-2.

use crate::progress::{ProgressRecorder, ProgressSource, TurnStartRecorder};
use crate::registry::{self, RegistryEntry, StatusWriteOutcome};
use qrmux::headless::{Republish, Sink};
use qrmux::stream_json::TurnOutcome;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Injected clock: returns "now" in epoch-ms. Boxed so the sink can carry a real
/// `SystemTime`-backed clock in production and a fixed clock in tests.
pub type NowFn = Box<dyn Fn() -> i64 + Send + Sync>;

/// WP-B5-i (identity option B, daemon-mint fallback): the identity the daemon
/// stamps onto the child-pid-keyed registry row it MINTS for a headless session.
/// `session_id` is NOT here — it is only known at the `system/init` event
/// (delivered as [`Republish::Ready`]) and supplied at mint time.
///
/// WHY a daemon mint at all: a headless `claude -p --output-format stream-json`
/// child writes NO `<pid>.json` registration row even with
/// `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1` (verified first-hand, B5-i; the flag
/// is proven-interactive, inert headless). So the daemon stamps the row itself —
/// CHILD-PID-keyed (never daemon-pid-keyed, the rejected option A) — borrowing only
/// the row-CREATION mechanism from option A (cf. the codex `resume_daemon.rs`
/// daemon-row write), per the `WP-B-CS-1-IDENTITY-FORK-RULING.md` caveat-2 fallback.
#[derive(Debug, Clone)]
pub struct MintIdentity {
    /// The sb session name → addressability by name (`sb ls`/`relay`/`connect`).
    pub name: Option<String>,
    /// The per-session working directory (informational on the row).
    pub cwd: Option<String>,
    /// The hosting/mode discriminant the `sb connect` resolver branches on
    /// (`Some("headless")` → `TargetMode::HeadlessAgent` → observe). Reuses the
    /// existing `entrypoint` row field (no new schema; existing rows leave it None).
    pub entrypoint: Option<String>,
    /// The provider id. For a claude headless row this is **`None`** (absent):
    /// claude rows carry NO `provider` field (the existing interactive invariant,
    /// common.rs), and the join defaults absent → `"claude-code"` (model.rs). An
    /// explicit `Some("claude")` is REJECTED by `provider_for`/
    /// `refuse_unknown_provider` (no such provider) and would make `sb connect`
    /// refuse the row "unknown provider" — so the mint leaves it `None`, uniform
    /// with the interactive `<pid>.json` model (the WP-B5-i D ruling). The field
    /// stays on `MintIdentity` for a future non-claude minter.
    pub provider: Option<String>,
}

/// A [`Sink`] that maps each [`Republish`] to a CAS-guarded registry status write
/// for one daemon-owned incarnation. See the module docs for the lifecycle.
pub struct RegistryStatusSink {
    /// The sessions dir holding `<pid>.json`.
    sessions_dir: PathBuf,
    /// The pid naming the row this sink writes.
    pid: i64,
    /// The incarnation stamp this sink's daemon established at boot — the CAS
    /// guard. A write whose on-disk `started_at` disagrees is Rejected (no stomp).
    expected_started_at: Option<i64>,
    /// Injected clock (epoch-ms) — determinism for tests.
    now: NowFn,
    /// WP-B5-i: when `Some`, the sink MINTS the child-pid-keyed row on the FIRST
    /// [`Republish::Ready`] (the daemon-mint fallback — claude writes no row
    /// headless). `None` = today's flip-only behaviour (boot owns row creation; a
    /// `set_status` against a missing row is a benign `NoRow` no-op).
    mint: Option<MintIdentity>,
    /// The `started_at` the mint writes AND the CAS guard expects afterwards (kept
    /// coherent: minted row's `startedAt` == `expected_started_at`). Unused when
    /// `mint` is `None`.
    mint_started_at: i64,
    /// One-shot guard: the row is minted on the first `Ready` only; subsequent
    /// `Ready`s (a multi-turn resume) just re-assert `busy` via `set_status`.
    minted: AtomicBool,
    /// R3b-Step-0: the live **signal-B** tap. When `Some`, each OUTPUT `Republish`
    /// (`Ready`/`Event`/`TurnEnd`) advances `last_output_ms` for this incarnation's
    /// session in the shared progress producer. `Republish::Event` is the
    /// genuinely-DISJOINT tick: it advances signal-B WITHOUT moving the registry
    /// status or `updated_at` (note the `Event(_)` status arm writes nothing) — that
    /// is exactly what lets a long STREAMING turn stay `Busy` (signal-B fresh) while
    /// signal-A's turn-start timeout has fired. `None` for non-daemon / unit-test
    /// sinks. See [`crate::progress`].
    progress: Option<Arc<ProgressRecorder>>,
    /// R3c item-2: the live **signal-A** tap (the standing turn-start producer). When
    /// `Some`, a turn-START Republish (`Ready`) records the turn-start anchor and a
    /// turn-END Republish (`TurnEnd`/`Eof`/`Breaker`) clears it, so a consumer reads a
    /// REAL `since_turn_start_ms` for `classify_obs` (NOT a synthetic input). Fed at
    /// the SAME points the status flips, so `turn_in_flight` aligns with `busy` by
    /// construction. `None` for non-daemon / unit-test sinks. See [`crate::progress`].
    turn_clock: Option<Arc<TurnStartRecorder>>,
    /// The session_id (learned at the first `Republish::Ready` / `system-init`) — the
    /// key the progress producer and WS-A query by. Set once per incarnation.
    session_id: OnceLock<String>,
}

impl RegistryStatusSink {
    /// Build a sink for one incarnation. `now` is the injected epoch-ms clock.
    pub fn new(
        sessions_dir: PathBuf,
        pid: i64,
        expected_started_at: Option<i64>,
        now: NowFn,
    ) -> Self {
        Self {
            sessions_dir,
            pid,
            expected_started_at,
            now,
            mint: None,
            mint_started_at: 0,
            minted: AtomicBool::new(false),
            progress: None,
            turn_clock: None,
            session_id: OnceLock::new(),
        }
    }

    /// A production sink whose clock reads the real wall clock (epoch-ms).
    pub fn with_system_clock(
        sessions_dir: PathBuf,
        pid: i64,
        expected_started_at: Option<i64>,
    ) -> Self {
        Self::new(sessions_dir, pid, expected_started_at, Self::system_clock())
    }

    /// WP-B5-i (identity option B): a MINTING sink for a headless session, keyed on
    /// the claude CHILD `pid`. On the first [`Republish::Ready`] it stamps the
    /// child-pid-keyed registry row from `identity` + the event's `session_id`
    /// (status `busy`), so the session becomes addressable (`sb ls`/`relay`/
    /// `connect` by id AND name); every later flip is a CAS-guarded `set_status`
    /// against the `started_at` it minted. The `started_at` is stamped ONCE here so
    /// the minted row and the CAS guard agree.
    pub fn with_mint(sessions_dir: PathBuf, pid: i64, identity: MintIdentity, now: NowFn) -> Self {
        let started_at = now();
        Self {
            sessions_dir,
            pid,
            expected_started_at: Some(started_at),
            now,
            mint: Some(identity),
            mint_started_at: started_at,
            minted: AtomicBool::new(false),
            progress: None,
            turn_clock: None,
            session_id: OnceLock::new(),
        }
    }

    /// A production minting sink (real wall-clock epoch-ms). See [`Self::with_mint`].
    pub fn with_mint_system_clock(sessions_dir: PathBuf, pid: i64, identity: MintIdentity) -> Self {
        Self::with_mint(sessions_dir, pid, identity, Self::system_clock())
    }

    /// R3b-Step-0: attach the shared progress producer so this sink's OUTPUT
    /// Republishes advance **signal-B** on the live path. Builder form so the
    /// existing constructors (and every current caller/test) compile unchanged.
    pub fn with_progress(mut self, recorder: Arc<ProgressRecorder>) -> Self {
        self.progress = Some(recorder);
        self
    }

    /// R3c item-2: attach the standing **signal-A** turn-start producer so this
    /// sink records the turn-start anchor on turn boundaries (a consumer then reads
    /// a REAL `since_turn_start_ms` for `classify_obs`). Builder form so existing
    /// callers/tests compile unchanged.
    pub fn with_turn_clock(mut self, recorder: Arc<TurnStartRecorder>) -> Self {
        self.turn_clock = Some(recorder);
        self
    }

    /// Record a turn-START anchor (signal-A) IF a turn clock is attached and the
    /// session_id is known. First-start-wins within a turn (the recorder dedups),
    /// so a mid-turn re-`Ready` does not reset the anchor. Timestamped by the
    /// injected clock (deterministic in tests); aligned with the `busy` status flip.
    fn note_turn_started(&self) {
        if let Some(clock) = &self.turn_clock {
            if let Some(sid) = self.session_id.get() {
                clock.turn_started(sid, (self.now)());
            }
        }
    }

    /// Clear the turn-START anchor (signal-A) on a turn boundary (idle/offline) IF a
    /// turn clock is attached and the session_id is known. Aligned with the
    /// status flip away from `busy`.
    fn note_turn_ended(&self) {
        if let Some(clock) = &self.turn_clock {
            if let Some(sid) = self.session_id.get() {
                clock.turn_ended(sid);
            }
        }
    }

    /// Record one signal-B output tick for this incarnation's session, IF a producer
    /// is attached and the session_id is known (it is, after the first `Ready`).
    /// Keyed on session_id (the producer's query key); timestamped by the injected
    /// clock so tests are deterministic. Never writes registry status — this is the
    /// surface disjoint from signal-A.
    fn note_output(&self) {
        if let Some(recorder) = &self.progress {
            if let Some(sid) = self.session_id.get() {
                recorder.record(sid, (self.now)(), ProgressSource::AcpUpdate);
            }
        }
    }

    /// The real wall-clock epoch-ms closure shared by the production constructors.
    fn system_clock() -> NowFn {
        Box::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        })
    }

    /// WP-B5-i: mint (or, on a non-first `Ready`, re-assert) the child-pid-keyed
    /// row. The FIRST `Ready` of a minting sink writes the full row — pid, the
    /// `system/init` `session_id`, name/cwd/marker/provider, `startedAt` ==
    /// `mint_started_at`, status `busy`. This is the daemon-mint fallback: claude
    /// writes no `<pid>.json` headless, so without this the row never exists and
    /// every `set_status` is a `NoRow` no-op (the boot-row-wait test's red-before).
    /// A non-minting sink (or a later `Ready`) just flips `busy`.
    fn on_ready(&self, session_id: &str) {
        // R3b-Step-0: capture the session_id once (the progress producer's key).
        // `system-init` (Ready) is the first event of an incarnation, so signal-B
        // ticks on later `Event`s have a key to record under.
        let _ = self.session_id.set(session_id.to_string());
        if let Some(identity) = &self.mint {
            if !self.minted.swap(true, Ordering::SeqCst) {
                let now_ms = (self.now)();
                let entry = RegistryEntry {
                    pid: Some(self.pid),
                    session_id: Some(session_id.to_string()),
                    cwd: identity.cwd.clone(),
                    started_at: Some(self.mint_started_at),
                    updated_at: Some(now_ms),
                    status: Some("busy".to_string()),
                    name: identity.name.clone(),
                    entrypoint: identity.entrypoint.clone(),
                    provider: identity.provider.clone(),
                    ..Default::default()
                };
                if let Err(e) = registry::write_entry(&self.sessions_dir, &entry) {
                    debug_warn(&format!(
                        "headless row MINT failed (swallowed): pid={} session_id={session_id} \
                         error={e}",
                        self.pid
                    ));
                }
                return;
            }
        }
        self.write_status("busy");
    }

    /// Apply one status flip via the CAS-guarded writer. A `Rejected`/`NoRow`
    /// outcome (or an I/O error) is logged and SWALLOWED — a status write never
    /// panics the reader/pump.
    ///
    /// Diagnostics use the crate's `SB_DEBUG`-gated `eprintln!` convention
    /// (cf. `registry::debug_warn`) rather than a `tracing` dependency the crate
    /// does not carry — the swallow behavior is what the spec's contract demands.
    fn write_status(&self, status: &str) {
        let now_ms = (self.now)();
        match registry::set_status(
            &self.sessions_dir,
            self.pid,
            self.expected_started_at,
            status,
            now_ms,
        ) {
            Ok(StatusWriteOutcome::Written) => {}
            Ok(StatusWriteOutcome::Rejected { on_disk_started_at }) => {
                debug_warn(&format!(
                    "daemon status write REJECTED (foreign incarnation owns row, no stomp): \
                     pid={} status={status} expected_started_at={:?} on_disk_started_at={:?}",
                    self.pid, self.expected_started_at, on_disk_started_at
                ));
            }
            Ok(StatusWriteOutcome::NoRow) => {
                debug_warn(&format!(
                    "daemon status write SKIPPED (no <pid>.json row; boot owns creation): \
                     pid={} status={status}",
                    self.pid
                ));
            }
            Err(e) => {
                debug_warn(&format!(
                    "daemon status write FAILED (swallowed): pid={} status={status} error={e}",
                    self.pid
                ));
            }
        }
    }
}

/// One diagnostic line to stderr, gated behind `SB_DEBUG=1` (silent by default —
/// mirrors `registry::debug_warn`; a status write never crashes the pump).
fn debug_warn(msg: &str) {
    if std::env::var_os("SB_DEBUG").is_some_and(|v| v == "1") {
        eprintln!("sb[daemon_status]: {msg}");
    }
}

impl Sink for RegistryStatusSink {
    fn deliver(&self, msg: Republish) {
        match msg {
            // Producer up / first output → the turn is running. For a MINTING
            // sink (headless), the first Ready also stamps the child-pid-keyed row
            // (the daemon-mint fallback); see [`Self::on_ready`].
            Republish::Ready { session_id } => {
                self.on_ready(&session_id);
                // Turn running (status → busy) → record the signal-A turn-start
                // anchor (after on_ready set the session_id). First-start-wins.
                self.note_turn_started();
                // First output of the turn → advance signal-B.
                self.note_output();
            }
            // Coalescible mid-turn output → NO status move (avoid write
            // amplification) — but it IS output, so advance signal-B. This is the
            // genuinely-disjoint tick: `last_output_ms` moves while `updated_at` /
            // status stay frozen, which is what tells a STREAMING long turn (Busy)
            // from a SILENT one (Wedged) even when signal-A has fired for both.
            Republish::Event(_) => self.note_output(),
            // Turn complete → final output instant, then flip idle. The turn is over
            // → clear the signal-A anchor (turn_in_flight aligns with `busy`).
            Republish::TurnEnd(_) => {
                self.note_output();
                self.note_turn_ended();
                self.write_status("idle");
            }
            // Circuit-breaker killed the child mid-turn → degraded, no clean result.
            // The turn is over (no longer in flight) → clear the signal-A anchor.
            Republish::Breaker { .. } => {
                self.note_turn_ended();
                self.write_status("offline");
            }
            // EOF: clean completion → idle (idempotent re-assert); abort / no-turn
            // → offline (fail-closed display — never leave a stale "busy"). Either
            // way the turn is over → clear the signal-A anchor.
            Republish::Eof(outcome) => {
                self.note_turn_ended();
                match outcome {
                    TurnOutcome::Completed => self.write_status("idle"),
                    TurnOutcome::Aborted | TurnOutcome::NoTurn => self.write_status("offline"),
                }
            }
            // R3c-Step-1 daemon-driven wake: the control socket nudged a parked
            // headless session. It is not the child's output, but it advances
            // signal-B — the observable "the daemon drove the wake" evidence (the
            // session's progress clock moves) — WITHOUT moving status (a wake is not
            // a turn boundary). No write amplification.
            Republish::Wake => self.note_output(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{read_entry, write_entry, RegistryEntry};
    use qrmux::stream_json::{ResultEvent, StreamEvent, Usage};
    use std::path::Path;
    use tempfile::tempdir;

    const T: i64 = 1000;
    const PID: i64 = 4242;

    /// A fixed-clock sink over the given dir, matching incarnation T.
    fn sink(dir: &Path) -> RegistryStatusSink {
        RegistryStatusSink::new(dir.to_path_buf(), PID, Some(T), Box::new(|| 9_999))
    }

    /// Boot a fresh row (write_entry, started_at=T, status="idle").
    fn boot_row(dir: &Path) {
        let entry = RegistryEntry {
            pid: Some(PID),
            session_id: Some("sid-boot".into()),
            started_at: Some(T),
            updated_at: Some(T),
            status: Some("idle".into()),
            ..Default::default()
        };
        write_entry(dir, &entry).unwrap();
    }

    fn status(dir: &Path) -> Option<String> {
        read_entry(dir, PID).unwrap().status
    }

    fn result_event() -> ResultEvent {
        ResultEvent {
            session_id: "sid-boot".into(),
            is_error: false,
            stop_reason: Some("end_turn".into()),
            usage: Usage::default(),
            total_cost_usd: None,
        }
    }

    // --- Test #7: Sink lifecycle ---

    #[test]
    fn sink_ready_flips_to_busy() {
        let dir = tempdir().unwrap();
        boot_row(dir.path());
        sink(dir.path()).deliver(Republish::Ready {
            session_id: "sid-boot".into(),
        });
        assert_eq!(status(dir.path()).as_deref(), Some("busy"));
    }

    #[test]
    fn sink_event_does_not_flip_status() {
        let dir = tempdir().unwrap();
        boot_row(dir.path());
        let s = sink(dir.path());
        s.deliver(Republish::Ready {
            session_id: "sid-boot".into(),
        });
        assert_eq!(status(dir.path()).as_deref(), Some("busy"));
        // A bare Event must NOT move the status (no spurious flip, no write amp).
        s.deliver(Republish::Event(StreamEvent::Assistant {
            session_id: "sid-boot".into(),
            content: "hi".into(),
        }));
        assert_eq!(
            status(dir.path()).as_deref(),
            Some("busy"),
            "Event must not flip status"
        );
    }

    /// R3b-Step-0 DISJOINTNESS PROOF (the keystone, at the production tap): a
    /// mid-turn `Event` advances **signal-B** (`last_output_ms` in the progress
    /// producer) but moves NEITHER the registry status NOR `updated_at` (signal-A's
    /// surface). This is exactly why a STREAMING long turn stays `Busy` while a
    /// SILENT one wedges: output advances one surface, the turn-lifecycle the other,
    /// and they are genuinely disjoint on the LIVE path — not a test fixture.
    #[test]
    fn sink_event_advances_signal_b_but_not_status_or_updated_at() {
        use crate::progress::{ProgressProducer, ProgressRecorder};
        use std::sync::atomic::AtomicI64;
        let dir = tempdir().unwrap();
        boot_row(dir.path());
        let clock = Arc::new(AtomicI64::new(1000));
        let c2 = clock.clone();
        let recorder = Arc::new(ProgressRecorder::new());
        let s = RegistryStatusSink::new(
            dir.path().to_path_buf(),
            PID,
            Some(T),
            Box::new(move || c2.load(Ordering::SeqCst)),
        )
        .with_progress(recorder.clone());

        // No output observed yet → signal-B has no reading (fail-open at the predicate).
        assert_eq!(recorder.last_output_ms("sid-boot"), None);

        // Ready (turn start) at t=2000: status→busy, signal-B records first output.
        clock.store(2000, Ordering::SeqCst);
        s.deliver(Republish::Ready {
            session_id: "sid-boot".into(),
        });
        assert_eq!(status(dir.path()).as_deref(), Some("busy"));
        assert_eq!(recorder.last_output_ms("sid-boot"), Some(2000));
        let updated_after_ready = read_entry(dir.path(), PID).unwrap().updated_at;

        // Mid-turn Event at a LATER t=5000: signal-B advances; status + updated_at DON'T.
        clock.store(5000, Ordering::SeqCst);
        s.deliver(Republish::Event(StreamEvent::Assistant {
            session_id: "sid-boot".into(),
            content: "tok".into(),
        }));
        assert_eq!(
            recorder.last_output_ms("sid-boot"),
            Some(5000),
            "Event advances signal-B (last_output_ms)"
        );
        assert_eq!(
            status(dir.path()).as_deref(),
            Some("busy"),
            "Event does NOT move status"
        );
        assert_eq!(
            read_entry(dir.path(), PID).unwrap().updated_at,
            updated_after_ready,
            "Event does NOT move updated_at — signal-B is disjoint from signal-A's surface"
        );
    }

    #[test]
    fn sink_turn_end_flips_to_idle() {
        let dir = tempdir().unwrap();
        boot_row(dir.path());
        let s = sink(dir.path());
        s.deliver(Republish::Ready {
            session_id: "sid-boot".into(),
        });
        s.deliver(Republish::TurnEnd(result_event()));
        assert_eq!(status(dir.path()).as_deref(), Some("idle"));
    }

    #[test]
    fn sink_breaker_flips_to_offline() {
        let dir = tempdir().unwrap();
        boot_row(dir.path());
        let s = sink(dir.path());
        s.deliver(Republish::Ready {
            session_id: "sid-boot".into(),
        });
        s.deliver(Republish::Breaker {
            cap_bytes: 1024,
            observed_bytes: 2048,
        });
        assert_eq!(status(dir.path()).as_deref(), Some("offline"));
    }

    #[test]
    fn sink_eof_aborted_flips_to_offline_not_stale_busy() {
        let dir = tempdir().unwrap();
        boot_row(dir.path());
        let s = sink(dir.path());
        s.deliver(Republish::Ready {
            session_id: "sid-boot".into(),
        });
        // EOF without a clean result → fail-closed display (offline), NOT busy.
        s.deliver(Republish::Eof(TurnOutcome::Aborted));
        assert_eq!(
            status(dir.path()).as_deref(),
            Some("offline"),
            "aborted EOF must fail closed, never stale-busy"
        );
    }

    #[test]
    fn sink_eof_completed_stays_idle() {
        let dir = tempdir().unwrap();
        boot_row(dir.path());
        let s = sink(dir.path());
        s.deliver(Republish::Ready {
            session_id: "sid-boot".into(),
        });
        s.deliver(Republish::TurnEnd(result_event()));
        assert_eq!(status(dir.path()).as_deref(), Some("idle"));
        // Idempotent re-assert idle on a clean EOF — not flipped.
        s.deliver(Republish::Eof(TurnOutcome::Completed));
        assert_eq!(status(dir.path()).as_deref(), Some("idle"));
    }

    // --- Test #8: false-pos & false-neg ---

    /// Negative: a bare Event writes NO status (the on-disk row is byte-unchanged).
    #[test]
    fn sink_bare_event_writes_nothing() {
        let dir = tempdir().unwrap();
        boot_row(dir.path());
        let before = std::fs::read(dir.path().join(format!("{PID}.json"))).unwrap();
        sink(dir.path()).deliver(Republish::Event(StreamEvent::RateLimit {
            session_id: "sid-boot".into(),
        }));
        let after = std::fs::read(dir.path().join(format!("{PID}.json"))).unwrap();
        assert_eq!(before, after, "a bare Event must not write the row at all");
    }

    /// Positive: a Breaker on a busy row DOES flip to offline.
    #[test]
    fn sink_breaker_on_busy_flips_offline() {
        let dir = tempdir().unwrap();
        boot_row(dir.path());
        let s = sink(dir.path());
        s.deliver(Republish::Ready {
            session_id: "sid-boot".into(),
        });
        assert_eq!(status(dir.path()).as_deref(), Some("busy"));
        s.deliver(Republish::Breaker {
            cap_bytes: 1,
            observed_bytes: 2,
        });
        assert_eq!(status(dir.path()).as_deref(), Some("offline"));
    }

    // --- WP-B5-i: daemon-mint fallback (caveat-1 re-shaped teeth) ---

    const CHILD_PID: i64 = 9090;
    const MINT_TS: i64 = 7777;

    /// A minting sink over a fixed clock (so the minted `startedAt` == `MINT_TS`).
    fn mint_sink(dir: &Path) -> RegistryStatusSink {
        RegistryStatusSink::with_mint(
            dir.to_path_buf(),
            CHILD_PID,
            MintIdentity {
                name: Some("hl-1".into()),
                cwd: Some("/work".into()),
                entrypoint: Some("headless".into()),
                // WP-B5-i D ruling: claude rows carry NO provider field.
                provider: None,
            },
            Box::new(|| MINT_TS),
        )
    }

    /// GREEN-AFTER: on an EMPTY dir (no boot row — claude writes none headless), the
    /// first `Ready` MINTS the child-pid-keyed row: status busy + the system/init
    /// session_id + name + the headless marker + provider, keyed on CHILD_PID.
    #[test]
    fn mint_sink_ready_creates_child_pid_keyed_row() {
        let dir = tempdir().unwrap();
        // No boot_row: the dir is empty.
        assert!(
            read_entry(dir.path(), CHILD_PID).is_none(),
            "precondition: no row"
        );
        mint_sink(dir.path()).deliver(Republish::Ready {
            session_id: "claude-uuid".into(),
        });
        let row = read_entry(dir.path(), CHILD_PID).expect("row must be minted");
        assert_eq!(row.pid, Some(CHILD_PID), "keyed on the CHILD pid");
        assert_eq!(row.status.as_deref(), Some("busy"));
        assert_eq!(
            row.session_id.as_deref(),
            Some("claude-uuid"),
            "from system/init"
        );
        assert_eq!(row.name.as_deref(), Some("hl-1"));
        assert_eq!(
            row.entrypoint.as_deref(),
            Some("headless"),
            "connect discriminant"
        );
        // WP-B5-i D ruling: a claude row carries NO provider field (absent → the
        // join defaults to "claude-code"; an explicit value is REJECTED by the
        // resolver). The `..._provider_resolves_through_join_default` guard below
        // pins WHY absent is load-bearing.
        assert_eq!(row.provider, None, "claude rows carry no provider field");
        assert_eq!(row.started_at, Some(MINT_TS));
    }

    /// WP-B5-i (D) — the CONNECT-RESOLUTION guard the row/Sink-layer tests missed
    /// (the live-CLI DoD earned its keep by surfacing this): the minted row's
    /// `provider` must be a value `sb connect`'s resolver ACCEPTS. The join reads
    /// the field verbatim and defaults absent → `"claude-code"`
    /// (`join.rs`/`model.rs`); `refuse_unknown_provider`/`provider_for` then accept
    /// only real provider ids. So the minted (absent) provider, after the join
    /// default, MUST resolve via `provider_for` — else `sb connect` refuses the row
    /// "unknown provider" before the observe resolver ever runs.
    ///
    /// FIX-SHAPED MUTATION (the exact banked defect this caught): mint
    /// `provider: Some("claude")` → `provider_for("claude")` is `None` → this reds.
    #[test]
    fn mint_provider_resolves_through_join_default() {
        let dir = tempdir().unwrap();
        mint_sink(dir.path()).deliver(Republish::Ready {
            session_id: "claude-uuid".into(),
        });
        let row = read_entry(dir.path(), CHILD_PID).expect("row must be minted");
        // The join's absent → claude-code default (model.rs / join.rs:444).
        let resolved = row.provider.as_deref().unwrap_or("claude-code");
        assert!(
            crate::provider::provider_for(resolved).is_some(),
            "the minted row's provider {resolved:?} must resolve via provider_for \
             (else `sb connect` refuses the headless row 'unknown provider')"
        );
    }

    /// RED-BEFORE: WITHOUT the mint (a plain non-minting sink, today's behaviour) the
    /// same empty-dir `Ready` hits `set_status` → `NoRow` → silent no-op: the row is
    /// NEVER created, so the session is never addressable and status never flips.
    /// This is the fix-shaped mutation for the mint — deleting the mint reverts to
    /// exactly this dead path.
    #[test]
    fn nonminting_sink_ready_on_empty_dir_is_norow_noop() {
        let dir = tempdir().unwrap();
        // The non-minting constructor = the pre-B5 flip-only sink.
        RegistryStatusSink::new(
            dir.path().to_path_buf(),
            CHILD_PID,
            Some(MINT_TS),
            Box::new(|| 1),
        )
        .deliver(Republish::Ready {
            session_id: "claude-uuid".into(),
        });
        assert!(
            read_entry(dir.path(), CHILD_PID).is_none(),
            "no mint → no row → never addressable (NoRow no-op)"
        );
    }

    /// After minting, a `TurnEnd` flips the SAME child row to idle (the CAS guard
    /// matches the minted `startedAt`, so the flip is Written, not Rejected).
    #[test]
    fn mint_then_turn_end_flips_minted_row_idle() {
        let dir = tempdir().unwrap();
        let s = mint_sink(dir.path());
        s.deliver(Republish::Ready {
            session_id: "u".into(),
        });
        assert_eq!(
            read_entry(dir.path(), CHILD_PID).unwrap().status.as_deref(),
            Some("busy")
        );
        s.deliver(Republish::TurnEnd(result_event()));
        assert_eq!(
            read_entry(dir.path(), CHILD_PID).unwrap().status.as_deref(),
            Some("idle"),
            "TurnEnd CAS-flips the minted row (startedAt agrees)"
        );
    }

    /// The mint is ONCE: a second `Ready` (multi-turn resume) re-asserts busy via
    /// `set_status` and does NOT clobber the row's identity (session_id preserved).
    #[test]
    fn mint_is_once_second_ready_preserves_identity() {
        let dir = tempdir().unwrap();
        let s = mint_sink(dir.path());
        s.deliver(Republish::Ready {
            session_id: "first".into(),
        });
        s.deliver(Republish::TurnEnd(result_event())); // idle
        s.deliver(Republish::Ready {
            session_id: "ignored-second".into(),
        });
        let row = read_entry(dir.path(), CHILD_PID).unwrap();
        assert_eq!(row.status.as_deref(), Some("busy"), "re-asserted busy");
        assert_eq!(
            row.session_id.as_deref(),
            Some("first"),
            "the mint is one-shot — a later Ready must not re-stamp identity"
        );
    }

    /// A CAS-foreign TurnEnd is Rejected by the writer → does NOT falsely idle a
    /// foreign row. The sink swallows the rejection; the row keeps its status.
    #[test]
    fn sink_foreign_turn_end_does_not_idle_foreign_row() {
        let dir = tempdir().unwrap();
        // The on-disk row belongs to a DIFFERENT incarnation (started_at != T).
        let entry = RegistryEntry {
            pid: Some(PID),
            started_at: Some(5555),
            status: Some("busy".into()),
            ..Default::default()
        };
        write_entry(dir.path(), &entry).unwrap();
        // Our sink believes it owns incarnation T — its idle write must be rejected.
        sink(dir.path()).deliver(Republish::TurnEnd(result_event()));
        assert_eq!(
            status(dir.path()).as_deref(),
            Some("busy"),
            "foreign incarnation's status must not be flipped"
        );
    }
}
