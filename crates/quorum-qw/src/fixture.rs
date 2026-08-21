//! An in-memory [`LaneOps`] implementation for the conformance suite.
//!
//! Deliberately NOT reachable from the CLI — the same posture as
//! `provider::fixture::FixtureDaemonProvider`, which is left out of
//! `provider_for` so `qd new --provider fixture-daemon` cannot boot. A fixture
//! that can be started by a user is a production lane with no owner.
//!
//! It exists to prove two things the real lanes cannot prove cheaply: that the
//! trait is implementable without a live agent, and that the conformance suite
//! discriminates — a lane wired to answer correctly must go green, so a red
//! elsewhere means a real gap rather than a broken harness.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::lane::Lane;

use crate::contract::*;

/// What a [`FixtureLane`] should do for one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixtureBehavior {
    Works,
    Fails(LaneError),
}

/// A fully in-memory lane.
pub struct FixtureLane {
    lane: Lane,
    sessions: RefCell<HashMap<String, SessionSummary>>,
    delivered: RefCell<HashMap<String, Terminal>>,
    next_id: RefCell<u64>,
    /// Per-op overrides, keyed by the op names used in the conformance grid.
    behavior: HashMap<String, FixtureBehavior>,
    health_source: HealthSource,
    /// What `deliver` promises about terminals.
    terminal_expectation: TerminalExpectation,
}

impl FixtureLane {
    pub fn new(lane: Lane) -> Self {
        FixtureLane {
            lane,
            sessions: RefCell::new(HashMap::new()),
            delivered: RefCell::new(HashMap::new()),
            next_id: RefCell::new(1),
            behavior: HashMap::new(),
            health_source: if lane.is_pane() {
                HealthSource::RegistryStatus
            } else {
                HealthSource::LiveRpc
            },
            terminal_expectation: TerminalExpectation::Pending,
        }
    }

    /// Make one op fail, so the suite can be shown to discriminate.
    pub fn with_failure(mut self, op: &str, err: LaneError) -> Self {
        self.behavior
            .insert(op.to_string(), FixtureBehavior::Fails(err));
        self
    }

    pub fn with_terminal_expectation(mut self, t: TerminalExpectation) -> Self {
        self.terminal_expectation = t;
        self
    }

    fn gate(&self, op: &str) -> Result<(), LaneError> {
        match self.behavior.get(op) {
            Some(FixtureBehavior::Fails(e)) => Err(e.clone()),
            _ => Ok(()),
        }
    }

    /// Test hook: force a session cold so the wake-policy assertions have
    /// something to act on.
    pub fn sessions_set_cold(&self, id: &SessionId) {
        if let Some(s) = self.sessions.borrow_mut().get_mut(&id.0) {
            s.status = SessionStatus::Cold;
        }
    }

    fn mint(&self, prefix: &str) -> String {
        let mut n = self.next_id.borrow_mut();
        let id = format!("{prefix}-{n}");
        *n += 1;
        id
    }
}

impl FixtureLane {
    /// Inherent, not a trait method — see [`LaneOps`] for why the interface has
    /// no lane accessor.
    pub fn lane(&self) -> Lane {
        self.lane
    }
}

impl LaneOps for FixtureLane {
    fn start(&self, req: &StartRequest) -> Result<SessionHandle, LaneError> {
        self.gate("start")?;
        let id = req
            .resume
            .clone()
            .unwrap_or_else(|| SessionId(self.mint("sess")));
        self.sessions.borrow_mut().insert(
            id.0.clone(),
            SessionSummary {
                id: id.clone(),
                provider: self.lane.harness.provider_id().to_string(),
                name: Some(req.name.clone()),
                cwd: Some(req.cwd.to_string_lossy().into_owned()),
                status: SessionStatus::Idle,
                turns: 0,
                tokens: 0,
                last_active_ms: Some(0),
                git_branch: None,
            },
        );
        Ok(SessionHandle {
            // The fixture is identified from birth — an in-memory lane has no
            // bind phase to wait for. The two real lanes that cannot answer here
            // are `claude-code/mux-pane` and `codex/mux-pane`; see
            // [`SessionHandle::id`].
            id: Some(id),
            qd_id: Some(self.mint("qd")),
            pid: self.lane.is_pane().then_some(4242),
            started_at_ms: Some(0),
            // An in-memory lane creates no pane and no mux dir, and produces no
            // notices. Fabricating either would be the fixture asserting a fact
            // no real observation stands behind — the same rule its `wake` keeps
            // for `resident`/`pane` below.
            socket_dir: None,
            notes: Vec::new(),
        })
    }

    /// `render` and `cwd_override` are accepted and IGNORED — an in-memory lane
    /// builds no pane and launches no process, so there is nothing for either to
    /// decide. Present in the signature because the contract carries them;
    /// silently unused, never defaulted away.
    fn wake(
        &self,
        id: &SessionId,
        _render: crate::launch::RenderMode,
        _cwd_override: Option<String>,
    ) -> Result<WakeOutcome, LaneError> {
        self.gate("wake")?;
        let mut sessions = self.sessions.borrow_mut();
        let s = sessions
            .get_mut(&id.0)
            .ok_or_else(|| LaneError::NotFound { id: id.clone() })?;
        s.status = SessionStatus::Idle;
        Ok(WakeOutcome {
            // The fixture always relaunches: it has no already-running arm,
            // exactly as the three pane lanes have none.
            state: WakeState::Revived,
            handle: SessionHandle {
                id: Some(id.clone()),
                // No mint happens on a revive — see [`SessionHandle::qd_id`].
                qd_id: None,
                pid: self.lane.is_pane().then_some(4243),
                started_at_ms: Some(0),
                socket_dir: None,
                notes: Vec::new(),
            },
            // An in-memory lane produces neither. Reporting a fabricated
            // endpoint or pane name would be the fixture asserting a fact no
            // real observation stands behind.
            resident: None,
            pane: None,
        })
    }

    fn kill(&self, id: &SessionId) -> Result<KillReport, LaneError> {
        self.gate("kill")?;
        let existed = self.sessions.borrow_mut().remove(&id.0).is_some();
        if !existed {
            return Err(LaneError::NotFound { id: id.clone() });
        }
        Ok(KillReport {
            outcome: KillOutcome {
                reaped: Confirmation::Yes,
                tombstoned: Confirmation::Yes,
            },
            // Nothing was observed because nothing real was reaped: no pane
            // name, no clauses, no signal instant. The default is the honest
            // answer for an in-memory lane, not a stand-in for one.
            observed: ReapObservations::default(),
        })
    }

    /// Never degraded: an in-memory store cannot be refused, so `degraded` is
    /// EMPTY rather than unknown. A fixture that faked a degradation would be
    /// asserting a failure mode it does not have.
    fn list(&self) -> Result<Listing, LaneError> {
        self.gate("list")?;
        let mut out: Vec<SessionSummary> = self.sessions.borrow().values().cloned().collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Listing {
            sessions: out,
            degraded: Vec::new(),
        })
    }

    fn health(&self, id: &SessionId) -> Result<Health, LaneError> {
        self.gate("health")?;
        let sessions = self.sessions.borrow();
        let s = sessions
            .get(&id.0)
            .ok_or_else(|| LaneError::NotFound { id: id.clone() })?;
        Ok(Health {
            status: s.status,
            source: self.health_source,
            observed_at_ms: Some(0),
        })
    }

    /// An in-memory session is wired to the fixture itself, so a KNOWN id always
    /// has somewhere to receive. Liveness is not consulted, matching the real
    /// lanes: a cold fixture session still has a receive path, and `deliver`'s
    /// `wake_if_cold` is what resolves its coldness.
    ///
    /// `with_failure("receive_path", …)` is how a test asks for the refusing
    /// shapes — the fixture must not INVENT a `None`/`Undetermined` it has no
    /// topology to justify.
    fn receive_path(&self, id: &SessionId) -> Result<ReceivePath, LaneError> {
        self.gate("receive_path")?;
        if !self.sessions.borrow().contains_key(&id.0) {
            return Err(LaneError::NotFound { id: id.clone() });
        }
        Ok(ReceivePath::Available)
    }

    fn deliver(
        &self,
        id: &SessionId,
        _msg: &Message,
        policy: &DeliverPolicy,
    ) -> Result<Receipt, LaneError> {
        self.gate("deliver")?;
        let mut woke = Confirmation::No;
        {
            let mut sessions = self.sessions.borrow_mut();
            let s = sessions
                .get_mut(&id.0)
                .ok_or_else(|| LaneError::NotFound { id: id.clone() })?;
            // `wake_if_cold` FIRST, exactly as `lanes::LaneImpl::deliver` reads
            // it: with no wake asked for there is no liveness question to answer,
            // so a cold row is ATTEMPTED rather than refused. The fixture would be
            // within its rights to refuse — its status is the truth, not a
            // projection over a pid — but the uniform suite is only worth running
            // while the fixture answers the same shape the real lanes do, and the
            // real lanes attempt. See [`crate::contract::LaneOps::deliver`].
            //
            // The half that survives unchanged is the one this always existed for:
            // no wake asked for means NO REVIVE. The status below is flipped only
            // inside the `wake_if_cold` arm, so a cold session that is delivered
            // into without a wake stays cold.
            if policy.wake_if_cold && s.status == SessionStatus::Cold {
                s.status = SessionStatus::Idle;
                // The fixture flips the status itself, so the revive is OBSERVED
                // here — `Yes`, never `Unknown`. `Unknown` belongs to a lane that
                // woke something it cannot re-read.
                woke = Confirmation::Yes;
            }
        }
        let mid = MessageId(self.mint("msg"));
        if self.terminal_expectation == TerminalExpectation::Pending {
            self.delivered
                .borrow_mut()
                .insert(mid.0.clone(), Terminal::Seen);
        }
        Ok(Receipt {
            message_id: mid,
            accepted: true,
            terminal: self.terminal_expectation.clone(),
            woke,
        })
    }

    fn await_terminal(
        &self,
        _id: &SessionId,
        message_id: &MessageId,
        _budget_ms: u64,
    ) -> Result<Terminal, LaneError> {
        self.gate("await_terminal")?;
        Ok(self
            .delivered
            .borrow()
            .get(&message_id.0)
            .cloned()
            .unwrap_or(Terminal::TimedOut))
    }

    fn recover(&self, _at: &LedgerAddress, message_id: &MessageId) -> Result<Terminal, LaneError> {
        self.gate("recover")?;
        Ok(self
            .delivered
            .borrow()
            .get(&message_id.0)
            .cloned()
            .unwrap_or(Terminal::Undetermined {
                reason: "fixture has no transcript to search".to_string(),
            }))
    }

    /// The fixture's ledger is its `delivered` map, so "is there a terminal" is
    /// exactly "is there an entry" — the same source `recover` answers from, read
    /// without writing anything.
    fn resolved(
        &self,
        _at: &LedgerAddress,
        message_id: &MessageId,
    ) -> Result<Option<Terminal>, LaneError> {
        self.gate("resolved")?;
        Ok(self.delivered.borrow().get(&message_id.0).cloned())
    }

    /// The fixture has no turn machinery, so its answer is its session's STATUS:
    /// a live session is idle and always was (`IdleAtEntry`), and a cold one
    /// cannot be observed at all.
    ///
    /// `Cold → Undetermined` rather than `Cold →` some idle-ish answer is the
    /// point of the variant: a session nothing is driving is not "idle", it is
    /// unobservable, and a suite that let the fixture answer `IdleAtEntry` for it
    /// would be asserting that a lane may report idleness off a source it does not
    /// have. It also keeps `TurnState::Undetermined` a REACHED variant in the
    /// conformance grid rather than a documented one.
    fn await_idle(&self, id: &SessionId, _budget_ms: u64) -> Result<TurnState, LaneError> {
        self.gate("await_idle")?;
        let sessions = self.sessions.borrow();
        let Some(s) = sessions.get(&id.0) else {
            return Err(LaneError::NotFound { id: id.clone() });
        };
        Ok(match s.status {
            SessionStatus::Cold => TurnState::Undetermined {
                reason: "the fixture lane has no live turn source for a cold session".to_string(),
            },
            _ => TurnState::IdleAtEntry,
        })
    }

    fn attach(&self, id: &SessionId) -> Result<i32, LaneError> {
        self.gate("attach")?;
        if !self.sessions.borrow().contains_key(&id.0) {
            return Err(LaneError::NotFound { id: id.clone() });
        }
        // A fixture never takes over a real terminal; 0 stands for "attached and
        // the attached program exited cleanly".
        Ok(0)
    }
}
