//! Lifecycle-collapse A-2 (spec §3, the F3 four-arm ruling): the FOURTH boot
//! micro-phase. Bind-at-boot-confirm stops being a best-effort warning and
//! becomes a hard readiness gate — `qd start` exit 0 GUARANTEES the pre-minted
//! stable id is bound to the live registry row's provider sessionId.
//!
//! The four ways the old bind block ended short of bound, each RULED (spec
//! A-2 / stress-test F3):
//!
//! - `NoneBindable` (row has no sessionId yet, or no live named row): RETRY
//!   until the budget — pinned by reference to the existing boot-phase
//!   timeout, one knob ([`crate::boot::BootTimeouts::pid_phase_ms`]) — then
//!   fail (class `unbound`).
//! - `Ambiguous{count}`: fail IMMEDIATELY, never retry — retrying a start
//!   against a duplicated name is how a third same-name session gets minted
//!   (class `ambiguous`).
//! - idstore bind `Err`: retry-then-fail (class `unbound`, the error rides
//!   the detail).
//! - `SessionHasDifferentId{existing}`: fail immediately, naming BOTH ids
//!   (class `diverged`).
//!
//! The caller (`qd start`) maps a failure to exit 1 with the human text on
//! stderr and, under `--json`, the machine error object on stdout. The
//! session itself is left running — killing it would not help, and the I6
//! posture applies: say exactly what was left.

use std::path::Path;

use crate::boot::Sleeper;
use crate::effects::Clock;
use crate::idstore;
use crate::registry::{self, LiveNamePick};

/// Success: the id is bound (or was already bound to the same pair — the fork
/// path's idempotent re-bind).
#[derive(Debug, Clone, PartialEq)]
pub struct BindPhaseOk {
    /// The provider session UUID the stable id is bound to.
    pub session_id: String,
    /// The live row's pid, when known (for `--json` output).
    pub pid: Option<i64>,
    /// The live row's status string at bind time, when present.
    pub status: Option<String>,
}

/// Failure: one of the three ruled machine classes (spec A-2's `--json`
/// error object: `unbound` | `ambiguous` | `diverged`).
#[derive(Debug, Clone, PartialEq)]
pub enum BindPhaseFailure {
    /// Budget exhausted and the id is still unbound: no live named row, a row
    /// with no sessionId yet, or a persistently erroring idstore bind.
    Unbound {
        /// The live named row's pid if one was ever seen.
        pid: Option<i64>,
        /// The last idstore bind error, when that arm is what kept failing.
        last_bind_err: Option<String>,
    },
    /// More than one RUNNING session claims the name. Failed immediately —
    /// never retried (a retry mints a third same-name session).
    Ambiguous { count: usize },
    /// The registry session already maps to a DIFFERENT stable id. Failed
    /// immediately, naming both ids.
    Diverged {
        /// The provider session UUID the row carries.
        registry_session_id: String,
        /// The stable id that session is already mapped to.
        existing_id: String,
        pid: Option<i64>,
    },
}

impl BindPhaseFailure {
    /// The machine class for the `--json` error object (spec A-2).
    pub fn class(&self) -> &'static str {
        match self {
            BindPhaseFailure::Unbound { .. } => "unbound",
            BindPhaseFailure::Ambiguous { .. } => "ambiguous",
            BindPhaseFailure::Diverged { .. } => "diverged",
        }
    }
}

/// Run the bind micro-phase: poll the registry for the live named row's
/// sessionId and bind the pre-minted stable id to it, honoring the four-arm
/// ruling above. Check-before-sleep: a zero budget still makes one full pass.
///
/// Seam-injected (clock/sleeper/liveness) so every arm is unit-testable
/// without booting anything.
#[allow(clippy::too_many_arguments)]
pub fn run_bind_phase(
    sessions_dir: &Path,
    ids_path: &Path,
    name: &str,
    qd_session_id: &str,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
    is_alive: &dyn Fn(i64) -> bool,
    budget_ms: i64,
    poll_ms: u64,
) -> Result<BindPhaseOk, BindPhaseFailure> {
    let deadline = clock.now_ms() + budget_ms;
    let mut seen_pid: Option<i64> = None;
    let mut last_bind_err: Option<String> = None;
    loop {
        let rows = registry::read_entries(sessions_dir, false);
        // The decision comes from the single-homed picker; pid/status are
        // captured alongside from the same scan for the JSON surface.
        let live_named: Vec<&registry::ScannedEntry> = rows
            .iter()
            .filter(|s| {
                !s.tombstoned
                    && s.entry.name.as_deref() == Some(name)
                    && s.entry.pid.is_some_and(|p| is_alive(p))
            })
            .collect();
        if let Some(row) = live_named.first() {
            seen_pid = row.entry.pid;
        }
        match registry::pick_live_named_row(&rows, name, is_alive) {
            LiveNamePick::One { session_id: sid } => {
                match idstore::bind(ids_path, qd_session_id, &sid, clock) {
                    Ok(idstore::BindOutcome::Bound)
                    | Ok(idstore::BindOutcome::AlreadyBoundSameId) => {
                        let status = live_named.first().and_then(|r| r.entry.status.clone());
                        return Ok(BindPhaseOk {
                            session_id: sid,
                            pid: seen_pid,
                            status,
                        });
                    }
                    Ok(idstore::BindOutcome::SessionHasDifferentId { existing }) => {
                        // Diverged: immediate, naming both — polling cannot
                        // heal a row that is already someone else's.
                        return Err(BindPhaseFailure::Diverged {
                            registry_session_id: sid,
                            existing_id: existing,
                            pid: seen_pid,
                        });
                    }
                    Err(e) => {
                        // bind Err → retry-then-fail (the F3 ruling).
                        last_bind_err = Some(e);
                    }
                }
            }
            LiveNamePick::NoneBindable => {
                // No sessionId yet (or no live row yet) → retry until budget.
            }
            LiveNamePick::Ambiguous { count } => {
                // Fail immediately, NEVER retry.
                return Err(BindPhaseFailure::Ambiguous { count });
            }
        }
        if clock.now_ms() + poll_ms as i64 > deadline {
            return Err(BindPhaseFailure::Unbound {
                pid: seen_pid,
                last_bind_err,
            });
        }
        sleeper.sleep_ms(poll_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;

    /// Fake clock: starts at 1_000_000, advanced by the fake sleeper.
    struct TestClock(std::rc::Rc<Cell<i64>>);
    impl Clock for TestClock {
        fn now_ms(&self) -> i64 {
            self.0.get()
        }
    }

    /// Fake sleeper: advances the shared clock; optionally runs a hook on the
    /// nth sleep (e.g. "the registry row gains its sessionId on poll 3").
    struct TestSleeper<'a> {
        clock: std::rc::Rc<Cell<i64>>,
        calls: Cell<u64>,
        hook_at: Option<u64>,
        hook: Option<Box<dyn Fn() + 'a>>,
    }
    impl Sleeper for TestSleeper<'_> {
        fn sleep_ms(&self, ms: u64) {
            self.clock.set(self.clock.get() + ms as i64);
            let n = self.calls.get() + 1;
            self.calls.set(n);
            if let (Some(at), Some(hook)) = (self.hook_at, self.hook.as_ref()) {
                if n == at {
                    hook();
                }
            }
        }
    }

    fn write_row(dir: &Path, pid: i64, name: &str, session_id: Option<&str>) {
        let mut row = serde_json::json!({
            "pid": pid,
            "status": "idle",
            "name": name,
        });
        if let Some(sid) = session_id {
            row["sessionId"] = serde_json::Value::String(sid.to_string());
        }
        std::fs::write(
            dir.join(format!("{pid}.json")),
            serde_json::to_vec(&row).unwrap(),
        )
        .unwrap();
    }

    /// Pre-mint a KNOWN unbound id (production mints via `mint_unbound` before
    /// the bind phase runs; tests need a deterministic id).
    fn mint_known(ids: &Path, id: &str, clock: &dyn Clock) {
        let mut gen = || id.to_string();
        idstore::mint_unbound_with(ids, Some("t"), clock, &mut gen).unwrap();
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        sessions: PathBuf,
        ids: PathBuf,
        clock_cell: std::rc::Rc<Cell<i64>>,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let ids = tmp.path().join("ids.json");
        Fixture {
            _tmp: tmp,
            sessions,
            ids,
            clock_cell: std::rc::Rc::new(Cell::new(1_000_000)),
        }
    }

    const BUDGET: i64 = 40_000;
    const POLL: u64 = 125;

    #[test]
    fn binds_immediately_when_session_id_present() {
        let fx = fixture();
        write_row(&fx.sessions, 42, "alpha", Some("uuid-alpha"));
        let clock = TestClock(fx.clock_cell.clone());
        mint_known(&fx.ids, "qdidaaa2", &clock);
        let sleeper = TestSleeper {
            clock: fx.clock_cell.clone(),
            calls: Cell::new(0),
            hook_at: None,
            hook: None,
        };
        let out = run_bind_phase(
            &fx.sessions,
            &fx.ids,
            "alpha",
            "qdidaaa2",
            &clock,
            &sleeper,
            &|_| true,
            BUDGET,
            POLL,
        )
        .expect("bound");
        assert_eq!(out.session_id, "uuid-alpha");
        assert_eq!(out.pid, Some(42));
        assert_eq!(sleeper.calls.get(), 0, "no polls needed");
        // The independent read the acceptance names: the idstore map now
        // carries session-uuid → stable-id.
        let map = idstore::fold(&fx.ids);
        assert_eq!(
            map.by_session.get("uuid-alpha").map(String::as_str),
            Some("qdidaaa2")
        );
    }

    #[test]
    fn none_bindable_retries_until_session_id_lands() {
        let fx = fixture();
        write_row(&fx.sessions, 42, "alpha", None); // no sessionId yet
        let sessions = fx.sessions.clone();
        let clock = TestClock(fx.clock_cell.clone());
        mint_known(&fx.ids, "qdidaaa3", &clock);
        let sleeper = TestSleeper {
            clock: fx.clock_cell.clone(),
            calls: Cell::new(0),
            hook_at: Some(3),
            hook: Some(Box::new(move || {
                write_row(&sessions, 42, "alpha", Some("uuid-late"));
            })),
        };
        let out = run_bind_phase(
            &fx.sessions,
            &fx.ids,
            "alpha",
            "qdidaaa3",
            &clock,
            &sleeper,
            &|_| true,
            BUDGET,
            POLL,
        )
        .expect("bound after retries");
        assert_eq!(out.session_id, "uuid-late");
        assert!(sleeper.calls.get() >= 3, "polled until the sid landed");
    }

    #[test]
    fn none_bindable_fails_unbound_at_budget() {
        let fx = fixture();
        write_row(&fx.sessions, 42, "alpha", None); // never gains a sessionId
        let clock = TestClock(fx.clock_cell.clone());
        let sleeper = TestSleeper {
            clock: fx.clock_cell.clone(),
            calls: Cell::new(0),
            hook_at: None,
            hook: None,
        };
        let err = run_bind_phase(
            &fx.sessions,
            &fx.ids,
            "alpha",
            "qdidaaa4",
            &clock,
            &sleeper,
            &|_| true,
            BUDGET,
            POLL,
        )
        .expect_err("unbound at budget");
        assert_eq!(err.class(), "unbound");
        match err {
            BindPhaseFailure::Unbound { pid, .. } => assert_eq!(pid, Some(42)),
            other => panic!("wrong arm: {other:?}"),
        }
        // The fake clock advanced past the whole budget — the retry loop ran
        // to its deadline, not forever and not zero times.
        assert!(fx.clock_cell.get() >= 1_000_000 + BUDGET - POLL as i64);
        assert!(sleeper.calls.get() > 0);
    }

    #[test]
    fn ambiguous_fails_immediately_never_retries() {
        let fx = fixture();
        write_row(&fx.sessions, 41, "alpha", Some("uuid-a"));
        write_row(&fx.sessions, 42, "alpha", Some("uuid-b"));
        let clock = TestClock(fx.clock_cell.clone());
        let sleeper = TestSleeper {
            clock: fx.clock_cell.clone(),
            calls: Cell::new(0),
            hook_at: None,
            hook: None,
        };
        let err = run_bind_phase(
            &fx.sessions,
            &fx.ids,
            "alpha",
            "qdidaaa5",
            &clock,
            &sleeper,
            &|_| true,
            BUDGET,
            POLL,
        )
        .expect_err("ambiguous");
        assert_eq!(err, BindPhaseFailure::Ambiguous { count: 2 });
        // THE ruled arm: zero sleeps — ambiguous is never retried.
        assert_eq!(sleeper.calls.get(), 0);
    }

    #[test]
    fn diverged_fails_immediately_naming_both_ids() {
        let fx = fixture();
        write_row(&fx.sessions, 42, "alpha", Some("uuid-alpha"));
        let clock = TestClock(fx.clock_cell.clone());
        // Pre-bind the session uuid to a DIFFERENT stable id.
        mint_known(&fx.ids, "qdther22", &clock);
        mint_known(&fx.ids, "qdidaaa6", &clock);
        idstore::bind(&fx.ids, "qdther22", "uuid-alpha", &clock).unwrap();
        let sleeper = TestSleeper {
            clock: fx.clock_cell.clone(),
            calls: Cell::new(0),
            hook_at: None,
            hook: None,
        };
        let err = run_bind_phase(
            &fx.sessions,
            &fx.ids,
            "alpha",
            "qdidaaa6",
            &clock,
            &sleeper,
            &|_| true,
            BUDGET,
            POLL,
        )
        .expect_err("diverged");
        match &err {
            BindPhaseFailure::Diverged {
                registry_session_id,
                existing_id,
                ..
            } => {
                assert_eq!(registry_session_id, "uuid-alpha");
                assert_eq!(existing_id, "qdther22");
            }
            other => panic!("wrong arm: {other:?}"),
        }
        assert_eq!(err.class(), "diverged");
        assert_eq!(sleeper.calls.get(), 0, "diverged is never retried");
    }

    #[test]
    fn bind_err_retries_then_fails_unbound_with_detail() {
        let fx = fixture();
        write_row(&fx.sessions, 42, "alpha", Some("uuid-alpha"));
        // Make the idstore path un-writable: a DIRECTORY at the file path.
        std::fs::create_dir_all(&fx.ids).unwrap();
        let clock = TestClock(fx.clock_cell.clone());
        let sleeper = TestSleeper {
            clock: fx.clock_cell.clone(),
            calls: Cell::new(0),
            hook_at: None,
            hook: None,
        };
        let err = run_bind_phase(
            &fx.sessions,
            &fx.ids,
            "alpha",
            "qdidaaa7",
            &clock,
            &sleeper,
            &|_| true,
            BUDGET,
            POLL,
        )
        .expect_err("unbound after bind errors");
        match err {
            BindPhaseFailure::Unbound { last_bind_err, .. } => {
                assert!(last_bind_err.is_some(), "the bind error rides the detail");
            }
            other => panic!("wrong arm: {other:?}"),
        }
        assert!(
            sleeper.calls.get() > 0,
            "the Err arm retried before failing"
        );
    }

    #[test]
    fn dead_row_is_invisible_no_live_row_fails_unbound() {
        let fx = fixture();
        write_row(&fx.sessions, 42, "alpha", Some("uuid-dead"));
        let clock = TestClock(fx.clock_cell.clone());
        let sleeper = TestSleeper {
            clock: fx.clock_cell.clone(),
            calls: Cell::new(0),
            hook_at: None,
            hook: None,
        };
        let err = run_bind_phase(
            &fx.sessions,
            &fx.ids,
            "alpha",
            "qdidaaa8",
            &clock,
            &sleeper,
            &|_| false, // nothing is alive
            BUDGET,
            POLL,
        )
        .expect_err("no live row");
        assert_eq!(err.class(), "unbound");
        match err {
            BindPhaseFailure::Unbound { pid, .. } => {
                assert_eq!(pid, None, "a dead row never supplies the pid")
            }
            other => panic!("wrong arm: {other:?}"),
        }
    }
}
