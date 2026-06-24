//! Republish→status Sink bridge (WP-B2b-1, memo R1: daemon-as-status-writer).
//!
//! A concrete [`sbmux::headless::Sink`] the daemon attaches to a headless session
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

use crate::registry::{self, RegistryEntry, StatusWriteOutcome};
use sbmux::headless::{Republish, Sink};
use sbmux::stream_json::TurnOutcome;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

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
        }
    }

    /// A production minting sink (real wall-clock epoch-ms). See [`Self::with_mint`].
    pub fn with_mint_system_clock(sessions_dir: PathBuf, pid: i64, identity: MintIdentity) -> Self {
        Self::with_mint(sessions_dir, pid, identity, Self::system_clock())
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
            Republish::Ready { session_id } => self.on_ready(&session_id),
            // Coalescible mid-turn output → no status move (avoid write amplification).
            Republish::Event(_) => {}
            // Turn complete.
            Republish::TurnEnd(_) => self.write_status("idle"),
            // Circuit-breaker killed the child mid-turn → degraded, no clean result.
            Republish::Breaker { .. } => self.write_status("offline"),
            // EOF: clean completion → idle (idempotent re-assert); abort / no-turn
            // → offline (fail-closed display — never leave a stale "busy").
            Republish::Eof(outcome) => match outcome {
                TurnOutcome::Completed => self.write_status("idle"),
                TurnOutcome::Aborted | TurnOutcome::NoTurn => self.write_status("offline"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{read_entry, write_entry, RegistryEntry};
    use sbmux::stream_json::{ResultEvent, StreamEvent, Usage};
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
